//! Render 协议 v1 的**单一真源**。
//!
//! 契约文档：`docs/render-protocol.md`。Node 侧镜像：`src/render-protocol.ts`。
//! 改动此文件必须同步提升 [`RENDER_PROTOCOL_VERSION`] 并更新文档与 TS 镜像。

use serde::{Deserialize, Serialize};

/// 与 `src/render-protocol.ts` 的 `RENDER_PROTOCOL_VERSION` 必须一致。
pub const RENDER_PROTOCOL_VERSION: u32 = 1;

/// 单帧上限：`u32` 长度前缀 + UTF-8 JSON。
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// 与 `src/pty-text-utils.ts` 的 `PTY_OUTPUT_MAX_SIZE` 对齐。
pub const PTY_OUTPUT_MAX_CHARS: usize = 200_000;

/// 与 legacy `serializer.serialize({ scrollback: 5000 })` 对齐。
pub const SCROLLBACK_LINES: usize = 5_000;

/// 快照重算的静默窗口（毫秒）。
pub const SNAPSHOT_QUIET_MS: u64 = 100;
/// pending 字符上限，超过即重算快照。
pub const SNAPSHOT_PENDING_MAX_CHARS: usize = 256 * 1024;
/// pending 操作条数上限，超过即重算快照。
pub const SNAPSHOT_PENDING_MAX_OPS: usize = 1024;

// ── 信封 ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub id: u32,
    pub token: String,
    pub protocol_version: u32,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub id: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    Unauthorized,
    BadRequest,
    NotFound,
    Conflict,
    UnsupportedMethod,
    ProtocolMismatch,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Data {
        session_id: String,
        incarnation_id: String,
        data: String,
        seq: u64,
    },
    #[serde(rename_all = "camelCase")]
    Exit {
        session_id: String,
        incarnation_id: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    #[serde(rename_all = "camelCase")]
    Reconcile { session_ids: Vec<String> },
}

// ── 方法参数 / 结果 ──

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub started_at: String,
    pub sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListResult {
    pub sessions: Vec<SessionState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AttachParams {
    pub session_id: String,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AttachResult {
    pub state: SessionState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrAttachParams {
    pub session_id: String,
    pub file: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_term_name")]
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_marker_token: Option<String>,
    #[serde(default)]
    pub after_seq: u64,
}

fn default_term_name() -> String {
    "xterm-256color".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrAttachResult {
    pub state: SessionState,
    pub is_new: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriteParams {
    pub session_id: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResizeParams {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KillParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ForgetParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatsResult {
    pub uptime_ms: u64,
    pub sessions: usize,
    pub live_bytes: u64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownParams {
    pub mode: ShutdownMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ShutdownMode {
    Drain,
    Now,
}

// ── SessionState ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub session_id: String,
    pub incarnation_id: String,
    pub pid: u32,
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    pub seq: u64,
    pub output: String,
    pub chunks: Vec<Chunk>,
    pub terminal_snapshot: Option<TerminalSnapshot>,
    pub launch_marker_token: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub data: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSnapshot {
    pub version: u32,
    pub data: String,
    pub cols: u16,
    pub rows: u16,
    pub pending: Vec<PendingOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PendingOp {
    #[serde(rename_all = "camelCase")]
    Data { data: String },
    #[serde(rename_all = "camelCase")]
    Resize { cols: u16, rows: u16 },
}

// ── 帧编解码（纯函数，供任意 IO 复用）──

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame length {0} exceeds MAX_FRAME_BYTES")]
    TooLarge(u32),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// 编码一帧：`u32` 大端长度 + UTF-8 JSON。
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(value)?;
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(FrameError::TooLarge(body.len() as u32));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// 从缓冲区尝试解出一帧。
///
/// 返回 `Ok(Some((frame_bytes_consumed, value)))`、`Ok(None)`（数据不足，需继续读）或错误。
/// 长度越界视为致命错误（调用方应关闭连接）。
pub fn decode_frame<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> Result<Option<(usize, T)>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let value: T = serde_json::from_slice(&buf[4..total])?;
    Ok(Some((total, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_request() {
        let request = Request {
            id: 7,
            token: "abc".into(),
            protocol_version: RENDER_PROTOCOL_VERSION,
            method: "list".into(),
            params: None,
        };
        let encoded = encode_frame(&request).expect("encode");
        assert_eq!(&encoded[..4], &(encoded.len() as u32 - 4).to_be_bytes());
        let (consumed, decoded): (usize, Request) =
            decode_frame(&encoded).expect("decode").expect("complete");
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, request);
    }

    #[test]
    fn frame_partial_returns_none() {
        let response = Response {
            id: 1,
            ok: true,
            result: Some(serde_json::json!({ "pong": true })),
            error: None,
        };
        let encoded = encode_frame(&response).expect("encode");
        assert!(decode_frame::<Response>(&encoded[..3]).expect("short").is_none());
        assert!(decode_frame::<Response>(&encoded[..encoded.len() - 1])
            .expect("partial")
            .is_none());
    }

    #[test]
    fn frame_rejects_oversized_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        assert!(matches!(
            decode_frame::<Request>(&buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn session_state_uses_camel_case_and_nulls() {
        let state = SessionState {
            session_id: "s1".into(),
            incarnation_id: "u1".into(),
            pid: 42,
            status: SessionStatus::Exited,
            exit_code: Some(0),
            cols: 80,
            rows: 24,
            seq: 3,
            output: "hi".into(),
            chunks: vec![Chunk { data: "hi".into(), seq: 3 }],
            terminal_snapshot: None,
            launch_marker_token: None,
        };
        let value = serde_json::to_value(&state).expect("json");
        assert_eq!(value["sessionId"], "s1");
        assert_eq!(value["incarnationId"], "u1");
        assert_eq!(value["status"], "exited");
        assert_eq!(value["terminalSnapshot"], serde_json::Value::Null);
        assert_eq!(value["launchMarkerToken"], serde_json::Value::Null);
        assert_eq!(value["chunks"][0]["seq"], 3);
    }

    #[test]
    fn pending_op_is_tagged() {
        let op = PendingOp::Resize { cols: 120, rows: 40 };
        let value = serde_json::to_value(&op).expect("json");
        assert_eq!(value["type"], "resize");
        assert_eq!(value["cols"], 120);
    }

    #[test]
    fn event_data_shape() {
        let event = Event::Data {
            session_id: "s1".into(),
            incarnation_id: "u1".into(),
            data: "x".into(),
            seq: 9,
        };
        let value = serde_json::to_value(&event).expect("json");
        assert_eq!(value["event"], "data");
        assert_eq!(value["sessionId"], "s1");
        assert_eq!(value["seq"], 9);
        assert!(value.get("id").is_none());
    }
}
