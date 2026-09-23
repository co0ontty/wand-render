# Structured Render wire protocol v2 (S1 implementation in progress)

This document specifies the **new** structured CLI process daemon contract. It does **not**
replace the frozen PTY v1 contract (`render-protocol.md`) or the legacy terminald.
The Rust types are in `crates/wand-render-protocol/src/structured_v2.rs`; the Node mirror is
`src/render-structured-protocol.ts` in the parent repository. The new
`wand-structured-renderd` daemon has a registry and wire-level integration test; **the Node
adapter, distribution and production routing are not yet enabled**. Changing these fields
requires bumping the v2 version
on both sides and updating this document. PTY v1 retains protocol version 1.

## Namespace and authentication

Canonicalize the absolute config path (`realpath` if present, otherwise lexical absolute path),
then use the first 12 hexadecimal characters of SHA-256 as `<suffix>`. v2 uses
`/tmp/wand-structured-render-<uid>-<suffix>.sock` and
`<configDir>/.structured-render-<suffix>.{token,pid,json}`. It never accesses v1's
`wand-render-...` or terminald's `wand-terminald-...` sockets. Socket and token must be 0600;
verify the connecting uid before reading any token. Refuse protocol mismatches and foreign sockets.

The transport is the PTY v1 `u32` big-endian length plus UTF-8 JSON envelope (`Request`/
`Response`); **protocolVersion = 2** on every request. The first request must authenticate.
Events have no `id`, and use a tagged `event` field. Raw env, argv, prompt, and connection
credentials must never appear in inventory, responses, diagnostic logs or error messages.

## Methods

- `hello` returns version, protocolVersion, pid, startedAt and run count; `ping` checks liveness.
- `spawn` accepts `{runId,file,args,cwd,env,stdinData?}`. It atomically reserves `runId` and
  returns `{state,isNew}`. A second spawn for a present run (even exited) is an attach, never a
  second child. Write `stdinData` **once** then close stdin; omit stdin to ignore it. If the
  child fails to launch, create an attachable exited record with `pid=0`, negative OS error
  code, and empty logs; never silently lose or retry the same runId.
- `list` returns `{runs:RunState[]}`: metadata only; no logs or spawn arguments.
- `attach` accepts `{runId,afterStdoutSeq?,afterStderrSeq?,maxBytes?}` and returns
  `{state,stdout:ReplayStream,stderr:ReplayStream}`. Stream chunks are `{seq,data}`. Each
  stream has `nextSeq`, `complete`, `resetRequired`. `resetRequired` is mandatory if the
  requested cursor predates the retained window. `complete` means the page reaches the
  authoritative state sequence. Client pages forward until complete while buffering live
  events, then deduplicates by `(incarnationId,stream,seq)`. If a gap is detected, reset the
  reducer from a full retained replay or report truncated recovery; **never** append a fragment
  as if it were continuous. Max page payload is 512 KiB even when clients ask for more.
- `interrupt` sends a POSIX signal to a known run; `forget` deletes the owned record and may
  terminate that run only. `stats` reports run counts, retained log and RSS. `shutdown` accepts
  `{mode:"drain"|"now"}`: drain refuses new runs, waits for existing children; now kills
  only v2-owned children. Server disconnect closes a socket, **never** its child processes.

`RunState` contains `runId`, `incarnationId`, `pid`, `status` (`running|exited`), `exitCode`,
`signal`, `stdoutSeq`, `stderrSeq`, `stdoutTruncated`, `stderrTruncated`. Each decoded UTF-8
stream advances its own sequence once per non-empty chunk. Keep at most 8 MiB of decoded
bytes **per stream per run**, with bounded global admission and subscriber queues. Incomplete
UTF-8 on process exit is discarded, not replaced with U+FFFD. Unix socket broadcast events:
`stream` (runId, incarnationId, stream, seq, data), `exit` (runId, incarnationId, exitCode,
signal), `reconcile` (runIds).

## Acceptance before activation

The shared `StructuredExecHost` contract must pass with both terminald and v2; also verify
competing spawn/owner conflict, UTF-8 fragmentation, 8 MiB replay pagination/truncation,
subscriber overflow/reconnect, Server restart retaining PID, drain and explicit shutdown.
Only new runs may opt into v2. Existing run ownership comes from the corresponding daemon
inventory, not from a config toggle. Claude/Grok remain static/mock-only per the current user
instruction and must not be presented as live-validated providers.
