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

| triple | Rust target | 构建机 | 产物形态 | 状态 |
| --- | --- | --- | --- | --- |
| `darwin-arm64` | `aarch64-apple-darwin` | `macos-14` | Mach-O arm64 | 支持 |
| `darwin-x64` | `x86_64-apple-darwin` | `macos-14`（交叉编译） | Mach-O x86_64 | 支持 |
| `linux-x64` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` | ELF static-pie | 支持 |
| `linux-arm64` | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | ELF static | 支持 |
| `win32-x64` | — | — | — | **未实现**（见下） |

两个 darwin 目标都在 arm64 runner 上构建：`x86_64-apple-darwin` 支持交叉编译，不依赖正在退役的
Intel runner；产物在 CI 里通过 Rosetta 执行一次 `--version` 冒烟，确保交叉编译结果真能跑。
Linux 用静态 musl（`crt-static`），避免在旧发行版上因 glibc 符号缺失起不来；
注意 musl 目标需要 `musl-gcc`（CI 通过 `MUSL_CC` 传入 `CARGO_TARGET_*_LINKER`）。

Windows 未实现的硬原因不是「懒得写」：协议要求 Render 自身可被独立升级与寻址，Windows 需要命名管道
（不是 Unix socket），PTY 要换 ConPTY，信号语义（SIGHUP/SIGWINCH 不存在）也要重做，
对端的身份校验还得换成 `GetNamedPipeClientProcessId` + token 检查。代码已按 `cfg` 拆分并保留位置，
`cargo check --target x86_64-pc-windows-msvc` 通过，运行时会给出明确错误而不是神秘崩溃；
在把 Windows 做成「支持」之前，需要一台真 Windows 机器做验收 —— 本仓库不会发布无法验证的平台产物。

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
TRIPLE=linux-x64 MUSL_CC=musl-gcc scripts/package-release.sh   # 显式指定（需先装 target 与 musl 链接器）
```

### 在 macOS 上验证另外三个平台

不必等 CI：两个 darwin 目标可本地交叉编译，两个 Linux 目标可在与 CI 同构的容器里构建
（用 glibc 宿主 + musl target；**不要**用 alpine —— 那会让宿主本身也是 musl 目标，
`crt-static` 作用到 proc-macro 上必然编译失败）。

```bash
# darwin-x64：交叉编译 + Rosetta 真跑
TRIPLE=darwin-x64 scripts/package-release.sh
arch -x86_64 dist/v0.1.1/darwin-x64/wand-render --version

# linux-x64 / linux-arm64：与 CI 同构的 glibc 容器 + musl 链接器，含真 PTY 集成测试
docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/tmp/target rust:1.98-slim sh -c '
  apt-get update -qq && apt-get install -y -qq musl-tools file
  uname -m   # x86_64 或 aarch64，决定 TRIPLE
  MUSL_CC=musl-gcc TRIPLE=linux-arm64 OUT_DIR=/tmp/out bash scripts/package-release.sh
  /tmp/out/v0.1.1/linux-arm64/wand-render --version
  cargo test --release --locked --target aarch64-unknown-linux-musl
'
```

产出在 `dist/v<version>/<triple>/`：`wand-render`、`wand-render.version`、`wand-render.sha256`、
以及 `dist/fragments/<triple>.json`。产物同时写入 `dist/native/<triple>/`，方便主仓库直接取用。

## 发版流程

1. 改 `Cargo.toml` 的 workspace `version`，`cargo build` 更新 `Cargo.lock`，一起提交。
2. 打 tag 并推送：`git tag v0.1.1 && git push origin v0.1.1`（tag 必须与 crate 版本一致，脚本会断言）。
3. CI（`.github/workflows/release.yml`）在四平台构建 + 跑测试 + 冒烟，**断言四个平台产物齐全**
   （少一个平台就发版 = 那个平台的用户静默回落到 legacy），然后挂到 GitHub Release：
   `wand-render-<version>-<triple>.tar.gz` + `.sha256` + `manifest.json` + `checksums.txt`。
4. 在 `co0ontty/wand-render-bin` 触发 `Sync artifacts`（Actions → Run workflow → 填版本号）：
   它拉取上面的 Release 资产、逐个校验 sha256、写进 `v<version>/<triple>/` 并合并 `manifest.json`。
   **这一步零密钥**（拉公开 Release 不需要凭据，提交用本仓库自己的 GITHUB_TOKEN）。
5. 回主仓库 `co0ontty/wand`：更新 `render-bin/` 子模块指针；如需新源码，同步更新 `render/`
   子模块指针。**两步互相独立**，这也是「Render 与 Server 分开更新」的落点。
6. 主仓库 `npm run build` 把 `render-bin/v<version>/<triple>/*` 校验 sha256 后 stage 到
   `dist/native/<triple>/`（`--all`，覆盖全部平台），随 npm 包一起发布。

## 与 Server 的版本握手

`manifest.json` 记录每个版本的 `protocolVersion` 与 `minServerVersion`。Server 启动时：

- `protocolVersion` 不匹配 → **拒绝启动**（不降级运行，避免用错语义静默破坏会话）。
- 二进制版本低于包内随附版本 → 先就位新二进制再启动（`scripts/install-render-binary.js`）。
- 找不到本平台二进制 → `render.engine=auto` 打醒目警告并回退 legacy；`render.engine=rust` 直接报错。

## 不做的事

- 不做 Windows（第一阶段）。
- 不做跨机器 Render（只允许本机 socket / 命名管道）。
- 不在 Render 里做聊天解析、权限判定、数据库、HTTP。
