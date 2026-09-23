# wand-render

Wand 的常驻 **Render** 进程（Rust）。它持有 PTY、输出 journal 与 VT 屏幕模型，独立于 Wand
的 Server（Node）生命周期：**Server 因 npm 升级重启时，Render 与用户 shell 不停止。**

- 契约： [`docs/render-protocol.md`](docs/render-protocol.md)（协议 v1，冻结）
- 消费方： `co0ontty/wand` 的 `src/render-*`（以子模块 `render/` 引入）
- 产物： 构建出的二进制发布到 `co0ontty/wand-render-bin`，由主仓库以子模块 `render-bin/` 引入

## 为什么单独一个仓库

1. **发布节奏与 Server 解耦**：Render 修 bug 不必等 Server 发版，Server 也不必重新编译 Rust。
2. **产物可校验、可离线**：每个平台二进制带 sha256 与版本，由 `wand-render-bin` 固定；
   npm 包内直接内嵌，安装时不需要网络，也不需要用户装 Rust 工具链。
3. **平台矩阵需要各自的 CI runner**：Linux 产物静态链接 musl，必须在 Linux runner 上原生构建。

## 支持矩阵

| triple | Rust target | 构建机 | 状态 |
| --- | --- | --- | --- |
| `darwin-arm64` | `aarch64-apple-darwin` | `macos-14` | 支持 |
| `darwin-x64` | `x86_64-apple-darwin` | `macos-13` | 支持 |
| `linux-x64` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` | 支持（静态 musl） |
| `linux-arm64` | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | 支持（静态 musl） |
| `win32-x64` | — | — | **第一阶段不支持** |

Windows 不支持的硬原因不是「懒得写」：协议要求 Render 自身可被独立升级与寻址，Windows 需要命名管道
（不是 Unix socket），PTY 要换 ConPTY，信号语义（SIGHUP/SIGWINCH 不存在）也要重做。
代码已按 `cfg` 拆分并保留位置，Windows 上会给出明确错误而不是编译失败或神秘崩溃。

## 本地构建

```bash
cargo test --all-targets            # 单测 + 真 PTY 集成测试
cargo build --release               # target/release/wand-render
./target/release/wand-render --version
```

macOS 首次用 SwiftTerm 无关；本仓库只需要 Rust 工具链（见 `rust-toolchain.toml`）。

## 打包一个平台产物

```bash
scripts/package-release.sh                      # 当前平台
TRIPLE=linux-x64 scripts/package-release.sh     # 显式指定（需先装对应 target）
```

产出在 `dist/v<version>/<triple>/`：`wand-render`、`wand-render.version`、`wand-render.sha256`、
以及 `dist/fragments/<triple>.json`。产物同时写入 `dist/native/<triple>/`，方便主仓库直接取用。

## 发版流程

1. 改 `Cargo.toml` 的 workspace `version`，提交。
2. 打 tag 并推送：`git tag v0.1.1 && git push origin v0.1.1`（tag 必须与 crate 版本一致，脚本会断言）。
3. CI（`.github/workflows/release.yml`）在四个平台原生构建，产出 GitHub Release 资产，
   再把二进制与 `manifest.json` 推送到 `co0ontty/wand-render-bin`（需要 secret `RENDER_BIN_TOKEN`）。
4. 回主仓库 `co0ontty/wand`：更新 `render-bin/` 子模块指针指向新的产物提交；如需新源码，
   同步更新 `render/` 子模块指针。**两步互相独立**，这也是「Render 与 Server 分开更新」的落点。
5. 主仓库 `npm run build` 会把 `render-bin/v<version>/<triple>/*` 校验 sha256 后 stage 到
   `dist/native/<triple>/`，随 npm 包一起发布。

## 与 Server 的版本握手

`manifest.json` 记录每个版本的 `protocolVersion` 与 `minServerVersion`。Server 启动时：

- `protocolVersion` 不匹配 → **拒绝启动**（不降级运行，避免用错语义静默破坏会话）。
- 二进制版本低于包内随附版本 → 先就位新二进制再启动（`scripts/install-render-binary.js`）。
- 找不到本平台二进制 → `render.engine=auto` 打醒目警告并回退 legacy；`render.engine=rust` 直接报错。

## 不做的事

- 不做 Windows（第一阶段）。
- 不做跨机器 Render（只允许本机 socket / 命名管道）。
- 不在 Render 里做聊天解析、权限判定、数据库、HTTP。
