//! Structured process protocol v2. This is a separate daemon/namespace from PTY v1.
//! Wire contract: `render/docs/structured-protocol-v2.md`.
//! TypeScript mirror: `src/render-structured-protocol.ts`.
//!
//! Changes to this wire format require a version bump in BOTH implementations.
//! Do not change the PTY v1 version when evolving this independent protocol.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const STRUCTURED_PROTOCOL_VERSION: u32 = 2;
/// Each stream owns a bounded, append-only replay window; never return secrets from spawn.
pub const RUN_LOG_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Bound a single attach page, independently of the framing maximum.
pub const REPLAY_PAGE_MAX_BYTES: usize = 512 * 1024;

/// Outer request/response framing and error codes reuse the PTY protocol's
/// length-prefixed JSON envelope, but `protocolVersion` MUST be 2 on this socket.
pub use crate::{ErrorBody, ErrorCode, Request, Response, ShutdownMode};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    pub run_id: String,
    pub incarnation_id: String,
    pub pid: u32,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_seq: u64,
    pub stderr_seq: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnParams {
    pub run_id: String,
    pub file: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResult {
    pub state: RunState,
    pub is_new: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListResult {
    /// Metadata only: NEVER embed replay bytes, argv, cwd, stdin or env.
    pub runs: Vec<RunState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AttachParams {
    pub run_id: String,
    #[serde(default)]
    pub after_stdout_seq: u64,
    #[serde(default)]
    pub after_stderr_seq: u64,
    /// Total UTF-8 payload bytes returned across both streams (bounded by
    /// `REPLAY_PAGE_MAX_BYTES` by the daemon, regardless of client input).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamChunk {
    pub seq: u64,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStream {
    pub chunks: Vec<StreamChunk>,
    /// Sequence of the last returned chunk, or the requested cursor if empty.
    pub next_seq: u64,
    /// True only if this page reaches the sequence in the authoritative state.
    pub complete: bool,
    /// Client cursor predates the retained window; do not silently drop a gap.
    pub reset_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AttachResult {
    pub state: RunState,
    pub stdout: ReplayStream,
    pub stderr: ReplayStream,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunIdParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InterruptParams {
    pub run_id: String,
    #[serde(default)]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub started_at: String,
    pub runs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StatsResult {
    pub runs: usize,
    pub running_runs: usize,
    pub retained_log_bytes: usize,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Stream {
        run_id: String,
        incarnation_id: String,
        stream: StreamName,
        seq: u64,
        data: String,
    },
    #[serde(rename_all = "camelCase")]
    Exit {
        run_id: String,
        incarnation_id: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    #[serde(rename_all = "camelCase")]
    Reconcile {
        run_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StreamName {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_frame_and_event_roundtrip() {
        let request = Request {
            id: 1,
            token: "test-token".into(),
            protocol_version: STRUCTURED_PROTOCOL_VERSION,
            method: "attach".into(),
            params: Some(serde_json::to_value(AttachParams {
                run_id: "structured:test".into(),
                after_stdout_seq: 3,
                after_stderr_seq: 0,
                max_bytes: Some(1024),
            }).unwrap()),
        };
        let frame = crate::encode_frame(&request).unwrap();
        let (_, decoded): (usize, Request) = crate::decode_frame(&frame).unwrap().unwrap();
        assert_eq!(decoded, request);
        let event = Event::Stream {
            run_id: "structured:test".into(),
            incarnation_id: "incarnation".into(),
            stream: StreamName::Stderr,
            seq: 4,
            data: "中文💡".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"], "stream");
        assert_eq!(value["incarnationId"], "incarnation");
        assert_eq!(value["stream"], "stderr");
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
    }

    #[test]
    fn inventory_and_attach_do_not_contain_spawn_secrets() {
        let state = RunState {
            run_id: "structured:test".into(),
            incarnation_id: "incarnation".into(),
            pid: 42,
            status: RunStatus::Exited,
            exit_code: Some(0),
            signal: None,
            stdout_seq: 1,
            stderr_seq: 0,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let value = serde_json::to_value(ListResult { runs: vec![state.clone()] }).unwrap();
        assert_eq!(value["runs"][0]["stdoutSeq"], 1);
        assert!(value["runs"][0].get("stdoutLog").is_none());
        assert!(value["runs"][0].get("env").is_none());
        assert!(value["runs"][0].get("file").is_none());
        let attached = serde_json::to_value(AttachResult {
            state,
            stdout: ReplayStream {
                chunks: vec![StreamChunk { seq: 1, data: "ok".into() }],
                next_seq: 1,
                complete: true,
                reset_required: false,
            },
            stderr: ReplayStream {
                chunks: vec![], next_seq: 0, complete: true, reset_required: false,
            },
        }).unwrap();
        assert_eq!(attached["stdout"]["chunks"][0]["data"], "ok");
        assert_eq!(attached["stderr"]["resetRequired"], false);
    }
}
