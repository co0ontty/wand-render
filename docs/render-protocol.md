# Render 协议（冻结 v1）

状态：**冻结**。两侧实现（Rust `wand-renderd`、Node `src/render-daemon-client.ts`）必须逐字段对齐本文。
改动本协议必须同时提升 `RENDER_PROTOCOL_VERSION` 并更新本文与两侧常量。

参考实现：
- Rust：`wand-rs/crates/wand-render-protocol/src/lib.rs`（类型与帧编解码的单一真源）
- Node：`src/render-protocol.ts`（同一份契约的 TS 镜像）

---

## 1. 职责边界：Server 与 Render 分开

| | **Server**（可重启） | **Render**（常驻，`wand-renderd`） |
| --- | --- | --- |
| 生命周期 | 随 `npm update` / 自更新重启 | 独立 detached 进程，**Server 重启不停止** |
| 拥有 | HTTP/WS、鉴权、SQLite、任务/工作区/Mission、provider 流解析、权限弹窗投影、Web 资产 | PTY 进程、输出 journal、VT 屏幕模型、退出状态、启动标记 |
| 不拥有 | PTY 生命周期 | DB、HTTP、鉴权、聊天投影、权限判定 |

因此 Server 重启后必须能**完整重建**内存快照：Render 的 `attach`/`list` 返回的 `SessionState` 是唯一来源。

**Render 明确不做**：不解析 Claude 的聊天/权限（那是 Server 的投影，未来迁到 `wand-runtime`）、不写数据库、不暴露网络端口。Render 只做「终端与进程」。

## 2. 传输、帧、寻址

- 传输：Unix domain socket（macOS / Linux）。Windows 用命名管道，第一阶段不实现。
- 帧格式：`u32` 大端长度 + UTF-8 JSON 正文。单帧上限 **64 MiB**（`MAX_FRAME_BYTES`）。
- 每个连接独立解码；一条不可解析的帧 → 关闭该连接，不影响其他连接与 PTY。
- 鉴权：连接后第一个请求必须带 token（从 token 文件读取，mode 0600）；不符立即关闭。
- 路径（按 config 路径派生，`:suffix` = `sha256(realpath(configPath))[:12]`）：

| 文件 | 路径 | 权限 |
| --- | --- | --- |
| socket | `/tmp/wand-render-<uid>-<suffix>.sock` | 0600 |
| token | `<configDir>/.render-<suffix>.token` | 0600 |
| pid | `<configDir>/.render-<suffix>.pid` | 0644 |
| meta | `<configDir>/.render-<suffix>.json` | 0644 |

**命名隔离**：与 legacy `terminald`（`.terminald-<suffix>.token` / `wand-terminald-<uid>-<suffix>.sock`）使用不同文件名与 socket 名，**两侧永不互相领养**。升级期两套并存是预期行为（见 §7）。

## 3. 请求 / 响应 / 事件

请求（Client → Render）：

```json
{ "id": 1, "token": "<hex>", "protocolVersion": 1, "method": "createOrAttach", "params": { } }
```

响应（Render → Client）：

```json
{ "id": 1, "ok": true,  "result": { } }
{ "id": 1, "ok": false, "error": { "code": "notFound", "message": "..." } }
```

事件（Render → Client，无 `id`）：

```json
{ "event": "data", "sessionId": "s1", "incarnationId": "u-1", "data": "…", "seq": 12 }
{ "event": "exit", "sessionId": "s1", "incarnationId": "u-1", "exitCode": 0, "signal": null }
{ "event": "reconcile", "sessionIds": ["s1"] }
```

错误码：`unauthorized` / `badRequest` / `notFound` / `conflict` / `unsupportedMethod` / `protocolMismatch` / `internal`。

### 方法表

| method | params | result |
| --- | --- | --- |
| `hello` | — | `{ version, protocolVersion, pid, startedAt, sessions }` |
| `ping` | — | `{ pong: true }` |
| `list` | — | `{ sessions: SessionState[] }`（快照有界，见 §9.1） |
| `attach` | `{ sessionId, afterSeq? }` | `{ state: SessionState }`（完整快照）；未知 session → `notFound` |
| `createOrAttach` | `{ sessionId, file, args[], cwd, env{}, name, cols, rows, launchMarkerToken?, afterSeq? }` | `{ state, isNew }` |
| `write` | `{ sessionId, data }` | `{}` |
| `resize` | `{ sessionId, cols, rows }` | `{}` |
| `kill` | `{ sessionId, signal? }` | `{}` |
| `forget` | `{ sessionId }` | `{}` |
| `stats` | — | `{ uptimeMs, sessions, liveBytes, rssBytes }` |
| `shutdown` | `{ mode: "drain" \| "now" }` | `{}`（语义见 §9.3） |

`createOrAttach` 语义：

- session 不存在 → 用 `file/args/cwd/env/name/cols/rows` **新建** PTY，`isNew = true`。
- session 已存在且在运行 → **不重启**，返回现值，`isNew = false`。
- 已存在但已退出 → 返回退出后的状态，`isNew = false`（Server 决定是否新建会话记录）。

## 4. SessionState（与 Node 的 `TerminalSessionState` 逐字段对齐）

```ts
interface SessionState {
  sessionId: string;
  incarnationId: string;      // 每次新建 PTY 生成一次；attach 不改变
  pid: number;
  status: "running" | "exited";
  exitCode: number | null;
  cols: number;
  rows: number;
  seq: number;                // 最后分配的 chunk 序号
  output: string;             // 有界累积输出（供 Server 重建文本视图）
  chunks: { data: string; seq: number }[];  // 有界重放窗口，用于补洞
  terminalSnapshot: TerminalSnapshot | null;
  launchMarkerToken: string | null;
}
```

**必须与 `src/terminal-host.ts` 的 `TerminalSessionState` 同名同义**，字段名 camelCase，缺省用 `null` 而不是省略，
这样 Node 侧 `RenderDaemonClient` 可以直接把结果塞进既有 inventory，`ProcessManager` 不需要改。

### 有界性

| 项 | 上限 | 来源 |
| --- | --- | --- |
| `chunks` 窗口（按字符累计，保留最新） | 200000 字符 | `PTY_OUTPUT_MAX_SIZE` |
| 单个 chunk 超限时 | 只保留末尾 200000 字符 | 同上 |
| `output` | 200000 字符（保尾） | 同上 |
| VT 回滚行数 | 5000 行 | 对齐 `serializer.serialize({ scrollback: 5000 })` |

### Render 资源准入

`stats.liveBytes` 是各会话**保留状态的估值**：包括 `output` / `chunks` 的已分配容量、
VT 网格单元（`vt100::Cell` 为 32 字节，历史行按最近一次基线所见数量估算）、
基线快照与未固化的 pending。它不包含进程代码、线程栈和临时序列化副本，
也可能因历史最大宽高而高估；进程级占用另看 `rssBytes`（Linux 为当前常驻量，
macOS 为 `getrusage` 报告的峰值）。两者不保证相等。

只在**新建**会话时检查准入：最多 200 个运行中的 PTY（已退出记录不计数）；估算已保留字节加新屏幕和 1 MiB
启动预留，不得超过 `min(物理内存 / 4, 2 GiB)`。无法探测物理内存时用 512 MiB。
达到阈值返回 `conflict`；已有会话的 `attach`、输出、写入、resize 不受准入阈值影响，
也不会为释放内存而杀掉 PTY。这是准入保护，不是进程 RSS 的硬保证。

新建和 resize 的尺寸都限制为最多 1024 列、512 行、500000 个可见网格单元
（覆盖 Server 既有的 1000×500 范围）；
超出返回 `badRequest`，避免单次异常请求触发巨量网格分配。

### UTF-8 解码规则（必须与 legacy 行为一致）

- 每个 session 维护**有状态增量解码器**：只发出完整 UTF-8 标量序列，把不完整尾部留在缓冲区等下一个 chunk。
- session 结束时**丢弃**尾部不完整序列，不产出 U+FFFD。
- 超长 chunk 在解码后再做有界裁剪，不切断标量序列。

### seq 语义

- 每个 session 内单调自增，从 1 开始；`incarnationId` 改变时从 1 重新开始。
- `attach(afterSeq)` 返回 `chunks` 中 `seq > afterSeq` 的部分作为补洞数据；Server 侧据此重建。
- 事件 `data` 与 `chunks` 用同一套 seq。

## 5. TerminalSnapshot（语义要求）

```ts
interface TerminalSnapshot {
  version: 1;
  data: string;      // 版本化 ANSI 快照：写入客户端终端后屏幕等价
  cols: number;
  rows: number;
  pending: ({ type: "data"; data: string } | { type: "resize"; cols: number; rows: number })[];
}
```

要求：

1. **等价性**是判据，**不是字节相同**：把 `data` 写进一个同尺寸的终端模拟器后，屏幕内容（可见行、光标位置、SGR 属性、备用屏幕状态）必须与 Render 内部屏幕一致。
2. 序号化：`data` 建立基线屏幕，`pending` 按序重放即得到当前屏幕。客户端顺序固定为「写 `data` → 逐项重放 `pending` → fit 当前尺寸」。
3. 客户端**不得**用会 soft-reset 的 resize（保留粘贴模式/备用屏幕）。
4. 生成时机：写入静默 100ms 后，或 pending 超过 256KiB / 1024 项（对齐 legacy 常量）。高频输出期间不重算。
5. 备用屏幕激活时快照必须能在客户端重建备用屏幕。

## 6. 生命周期

- **启动**：Server 优先 adopt 已存在的 Render（socket + token + `hello` 校验 `protocolVersion`）。
  不存在则 detached spawn（`setsid`，stdio 忽略，不随 Server 退出），等待 socket 就绪（5s）。
- **端口/实例隔离**：socket / token / pid / meta 全部按 config 路径派生；`-c /tmp/x/config.json` 得到独立 Render。
- **disconnect（Server 侧）**：**只解绑**——关 socket、清 handle、保留 Render 与所有 PTY。
- **single instance**：pid 活着但 socket 不可用 → 等待就绪而不是另起一个；协议版本不匹配 → 明确报错，**不做降级运行**。
- **升级 Render 自身**：`shutdown { mode: "drain" }` 停止接受新会话，已退出会话释放，运行中会话保留；
  `mode: "now"` 杀掉所有 PTY 后退出（只在用户明确要求时使用）。

## 7. 与 legacy `terminald` 的共存（无损升级）

升级期 Render 与 legacy daemon 并存，**互相不领养**：

- 新会话一律进 Render。
- 旧会话（legacy daemon 仍持有）由 Server 的 `CompositeTerminalHost` 按所有权路由：先查 legacy inventory，命中则用 legacy 后端，否则用 Render。
- 只有当 legacy daemon 不再持有任何 running 会话时，才停止使用它。**升级过程中不杀任何 PTY**。
- 回滚：引擎开关切回 legacy；不需要回滚数据（DB schema 未变）。

## 8. 不做的事（防止范围蔓延）

- 不实现 Windows 命名管道（第一阶段）。
- 不做跨机器 Render 连接（只允许本机 socket）。
- 不在 Render 内做聊天/权限/DB/HTTP。
- 不改变对客户端的 WS 契约（`init`/`output`/`ping`/`resync_required`/`pty_error` 不变）。

---

## 9. 发布前修订（v1）

v1 **尚未随任何 npm 版本发布**。在第一次发布之前，本协议允许就地修订；一旦发布过，
任何语义变更都必须提升 `RENDER_PROTOCOL_VERSION` 并同步两侧常量与本文。

### 9.1 `list` 的快照必须有界，`attach` 才是权威再同步原语

触发过的问题：`list` 内联每个会话的完整输出、chunk 窗口与 5000 行回滚快照，
单个 1000 列会话的状态可达 5MB；十几个会话就让响应撞上 64MiB 单帧上限，
而超限的帧被静默丢弃 → 客户端只看到 10s 超时。

规则：

**§9.1.1** `list` 返回的 `terminalSnapshot` **必须做有界裁剪**（序列化后不超过 64KiB），
   用于列表显示与「有/无屏幕」判断；`output` / `chunks` 仍按 §4 的上限。
   裁剪顺序固定，两侧行为一致：完整快照够小就原样返回 → 否则先丢 `pending`
   （它是基线之后的增量，丢了只是预览略旧）→ 仍超限就截断 `data` 成前缀
   （前缀必须收在完整的转义序列处，绝不能留下未收尾的 `ESC`，否则客户端终端
   会一直等这条序列的收尾而吞掉后续真实输出）→ 连元信息都放不下则置空 `null`。
   因此 `list` 里的快照**不是**屏幕等价的预览。
**§9.1.2** **`attach` 是唯一权威的再同步原语**：需要精确重建屏幕的调用方（Server 重启后的
   恢复路径）必须走 `attach`，不能依赖 `list` 里的裁剪快照。断线后如果 `chunks`
   无法连续覆盖断线前的 `seq` 到最新 `seq`，Server 必须用完整 `attach` 状态重建屏幕，
   并通知已订阅客户端重新同步。
**§9.1.3** 任何请求的响应在编码后仍超过 `MAX_FRAME_BYTES` 时，**必须回一个 `internal` 错误帧并说明原因**，
   绝不静默丢弃该响应（静默丢弃会把内部错误伪装成网络超时）。
4. `createOrAttach` 与 `attach` 返回的 `state` 是完整的（不受 §9.1.1 的裁剪约束）。

### 9.2 路径必须 realpath 归一化

`<suffix>` 必须基于 **canonicalize（realpath 后）** 的 config 路径计算，而不是词法 `path.resolve`。
否则同一个 config 经符号链接或不同写法访问会派生两套 socket/token/pid，产生两个 Render，
PTY 所有权分裂。两侧实现必须使用同一算法，顺序也要一致：

1. 先词法绝对化并归一 `.`/`..`（Node：`path.resolve`；Rust：`lexical_absolute`）；
2. 再对第 1 步的结果做 `canonicalize`（Node：`fs.realpathSync`；Rust：`std::fs::canonicalize`），
   成功就用它的结果；
3. `canonicalize` 失败（路径还不存在，例如首次启动）就停在词法结果上。

顺序不能颠倒：直接对原始路径 `canonicalize` 会在「`..` 前面是符号链接」时给出内核语义
（`..` = 符号链接目标的父目录）从而与 Node 分叉。

第 3 条同时意味着「文件此刻是否存在」会决定走哪条分支，而两者在符号链接路径上结论不同
（macOS 的 `/tmp` 是 `/private/tmp` 的符号链接）：同一个 config 在文件创建前与创建后会
得到两个 suffix。所以调用方必须在**同一个时刻**用同一份归一化路径去派生与连接，不要
一半走 realpath、一半走词法。

### 9.3 `shutdown` 语义

- `drain`：**停止接受新会话（新 `createOrAttach` 返回 `conflict`），但进程继续存活**，
  已运行会话不受影响；当最后一个会话退出时自行结束（一个运行中会话都没有时立即结束）。
- `now`：杀掉所有 PTY 后退出。
- 收到 SIGTERM/SIGINT 等价于 `drain`；第二次信号等价于 `now`。

旧实现里任何 shutdown 都会立刻退出进程，而进程退出会关闭 PTY master fd 并让子进程收到 SIGHUP
——也就是说 `drain` 在旧实现里等于杀 PTY，与本文承诺相反。这条必须按上述语义修正。

### 9.4 `kill` 的默认信号

默认信号是 **SIGHUP**，对齐 node-pty 的 `kill()` 与 legacy daemon 行为。
调用方不传 `signal` 时期望的行为是「关闭终端」，不是「礼貌请进程退出」，两者对 TUI 与
前台作业的可见结果不同（SIGTERM 会被许多 TUI 当作可忽略/清理信号）。

### 9.5 跨平台与连接方身份校验

**§9.5.1** **传输**：Unix 用 domain socket；Windows 用命名管道（`\\.\pipe\wand-render-<suffix>`）。
   Windows 第一阶段不实现，但代码必须 `cfg` 拆分并对用户给出明确错误，不是编译失败。
**§9.5.2** **socket 归属校验**：socket 放在 `/tmp`（macOS 路径长度上限约 104 字节）。因为 `/tmp` 全局可写，
   任何本机进程都能抢先 `bind` 同名路径，所以：
   - 服务端 accept 之后必须通过 `getpeereid()`（macOS）/ `SO_PEERCRED`（Linux）校验对端 uid == 自身 uid；
   - 客户端连接后、发送 token **之前**，必须校验目标 socket 是 socket 类型、属主是自己、权限 0600；
   - 两侧都不满足时拒绝通信，而不是把 token 交给对方。
   - 服务端 **bind 之前**也要校验：路径已存在但不是「本用户的 0600 socket」时拒绝启动，
     绝不 `unlink` 不属于自己的路径；只有「本用户的 0600 socket 且连不上」才算陈旧残留，可以删掉重建。
**§9.5.3** **信号**：`SIGWINCH` 语义、进程组与会话创建只在 Unix 上有意义；Windows 分支要用
   ConPTY 语义重写，不做「能编译就上线」的移植。
**§9.5.4** **分发路径必须可执行**：产物经 npm/git 传输会丢可执行位。Render 启动前必须校验并按需 `chmod 0755`。
