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
| `list` | — | `{ sessions: SessionState[] }` |
| `attach` | `{ sessionId, afterSeq? }` | `{ state: SessionState }`；未知 session → `notFound` |
| `createOrAttach` | `{ sessionId, file, args[], cwd, env{}, name, cols, rows, launchMarkerToken?, afterSeq? }` | `{ state, isNew }` |
| `write` | `{ sessionId, data }` | `{}` |
| `resize` | `{ sessionId, cols, rows }` | `{}` |
| `kill` | `{ sessionId, signal? }` | `{}` |
| `forget` | `{ sessionId }` | `{}` |
| `stats` | — | `{ uptimeMs, sessions, liveBytes, rssBytes }` |
| `shutdown` | `{ mode: "drain" \| "now" }` | `{}`（为 Render 自身升级预留） |

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
