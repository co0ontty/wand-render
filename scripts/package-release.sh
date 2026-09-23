#!/usr/bin/env bash
# 为单个 triple 构建并打包 wand-render，产出可被 main 仓库与 wand-render-bin 直接消费的产物。
#
# 用法：
#   scripts/package-release.sh                 # 当前平台
#   TRIPLE=linux-x64 scripts/package-release.sh
#   OUT_DIR=/tmp/out scripts/package-release.sh
set -euo pipefail

cd "$(dirname "$0")/.."
. scripts/triples.sh

TRIPLE="${TRIPLE:-$(host_triple)}"
TARGET="$(triple_to_target "$TRIPLE" || true)"
if [ -z "$TARGET" ]; then
  echo "unsupported triple: $TRIPLE (supported: $(supported_triples))" >&2
  exit 1
fi

# 版本只有一个真源：workspace 的 Cargo.toml。tag 与它不一致时 CI 会在这里失败。
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
# 只认常量定义行：文件头注释里也出现了这个标识符，按标识符 grep 会先命中注释而拿到空值。
# 必须锚到 `: u32 = `：宽松写法会先吃掉类型名 `u32` 里的 32，把协议版本解析成 32。
PROTOCOL_VERSION="$(sed -n 's/^pub const RENDER_PROTOCOL_VERSION: *u32 *= *\([0-9][0-9]*\).*/\1/p' crates/wand-render-protocol/src/lib.rs | head -1)"
MIN_SERVER_VERSION="$(sed -n 's/.*"minServerVersion": *"\([^"]*\)".*/\1/p' release.json)"
if [ -z "$VERSION" ] || [ -z "$PROTOCOL_VERSION" ] || [ -z "$MIN_SERVER_VERSION" ]; then
  echo "cannot resolve version (crate=$VERSION protocol=$PROTOCOL_VERSION minServer=$MIN_SERVER_VERSION)" >&2
  exit 1
fi

echo "[package] triple=$TRIPLE target=$TARGET version=$VERSION protocol=$PROTOCOL_VERSION"

# 静态 musl 的 rustflags 在 .cargo/config.toml；这里只负责装 target。
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "[package] installing rust target $TARGET"
  rustup target add "$TARGET"
fi

cargo build --release --locked --target "$TARGET" -p wand-renderd

SRC="target/$TARGET/release/wand-render"
[ -x "$SRC" ] || { echo "build did not produce $SRC" >&2; exit 1; }

OUT_DIR="${OUT_DIR:-dist}/v$VERSION/$TRIPLE"
mkdir -p "$OUT_DIR" "dist/native/$TRIPLE"
cp "$SRC" "$OUT_DIR/wand-render"
printf '%s\n' "$VERSION" > "$OUT_DIR/wand-render.version"
printf '%s\n' "$PROTOCOL_VERSION" > "$OUT_DIR/wand-render.protocol"

# 分发路径上必须可执行：npm/git 传输过程会丢可执行位（legacy 的 node-pty spawn-helper 就栽在这）。
chmod 0755 "$OUT_DIR/wand-render"

# 自检：产物必须能被执行并报出版本。stub 或损坏的二进制在这里被拦下，
# 绝不进入分发目录（曾经有一个只打印 "(stub)" 的 302KB 假二进制进过 native/）。
RUN_VERSION="$("$OUT_DIR/wand-render" --version 2>&1 || true)"
case "$RUN_VERSION" in
  *"$VERSION"*) ;;
  *) echo "packaged binary reported unexpected version: $RUN_VERSION" >&2; exit 1 ;;
esac
case "$RUN_VERSION" in
  *stub*|*TODO*) echo "packaged binary looks like a stub: $RUN_VERSION" >&2; exit 1 ;;
esac

# 交叉校验三件事：二进制自报的协议版本等于源码常量；源码解析出的版本号合理；二进制不是 stub。
# 解析错、忘了重新编译、二进制来自别的提交，都在这里暴露，而不是等 Server 启动时报 protocolMismatch。
BINARY_PROTOCOL="$(printf '%s' "$RUN_VERSION" | sed -n 's/.*(protocol \([0-9][0-9]*\)).*/\1/p')"
if [ "$BINARY_PROTOCOL" != "$PROTOCOL_VERSION" ]; then
  echo "binary reports protocol=$BINARY_PROTOCOL but source declares $PROTOCOL_VERSION ($RUN_VERSION)" >&2
  exit 1
fi
case "$PROTOCOL_VERSION" in
  1|2|3|4|5|6|7|8|9|1[0-9]) ;;
  *) echo "implausible protocol version parsed from source: $PROTOCOL_VERSION" >&2; exit 1 ;;
esac

SHA="$(shasum -a 256 "$OUT_DIR/wand-render" | awk '{print $1}')"
SIZE="$(wc -c < "$OUT_DIR/wand-render" | tr -d ' ')"
printf '%s  wand-render\n' "$SHA" > "$OUT_DIR/wand-render.sha256"

# main 仓库直接取用的布局，与 wand-render-bin 的布局保持同构。
cp "$OUT_DIR/wand-render" "dist/native/$TRIPLE/wand-render"
cp "$OUT_DIR/wand-render.version" "dist/native/$TRIPLE/wand-render.version"
cp "$OUT_DIR/wand-render.sha256" "dist/native/$TRIPLE/wand-render.sha256"
chmod 0755 "dist/native/$TRIPLE/wand-render"

mkdir -p dist/fragments
cat > "dist/fragments/$TRIPLE.json" <<JSON
{
  "triple": "$TRIPLE",
  "rustTarget": "$TARGET",
  "version": "$VERSION",
  "protocolVersion": $PROTOCOL_VERSION,
  "minServerVersion": "$MIN_SERVER_VERSION",
  "sha256": "$SHA",
  "size": $SIZE
}
JSON

echo "[package] wrote $OUT_DIR/wand-render ($SIZE bytes, sha256 $SHA)"
