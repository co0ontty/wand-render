//! unix socket 服务：帧编解码、鉴权、方法分发与事件广播。
//!
//! 关键约束（`docs/render-protocol.md` §3）：
//! - 第一个请求必须带正确 token，否则**立即关闭**这条连接；
//! - `protocolVersion` 必须是 1，否则回 `protocolMismatch`；
//! - 长度越界或 JSON 解析失败只关这一条连接，不影响别的连接与 PTY。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use wand_render::{EventSink, RenderError, RenderErrorKind, RenderRegistry};
use wand_render_protocol::{
  decode_frame, encode_frame, AttachParams, AttachResult, CreateOrAttachParams,
  ErrorBody, ErrorCode, Event, ForgetParams, KillParams,
  ListResult, Request, ResizeParams, Response, ShutdownMode, ShutdownParams, WriteParams,
  MAX_FRAME_BYTES, RENDER_PROTOCOL_VERSION,
};

/// 每条连接的发送队列深度。满了说明客户端读得太慢：断开它，
/// 让它重连后用 `attach(afterSeq)` 补洞，而不是阻塞 PTY 读取线程。
const EVENT_QUEUE_DEPTH: usize = 1024;
/// 单条连接的读缓冲上限（一帧最多 `MAX_FRAME_BYTES`）。
const MAX_REQUEST_BUFFER_BYTES: usize = MAX_FRAME_BYTES as usize + 4;

type DispatchResult = Result<JsonValue, (ErrorCode, String)>;

/// 常量时间比较，避免 token 校验泄漏长度/前缀信息。
pub fn tokens_match(expected: &str, provided: &str) -> bool {
  let expected = expected.as_bytes();
  let provided = provided.as_bytes();
  let mut diff = (expected.len() ^ provided.len()) as u8;
  let length = expected.len().max(provided.len()).max(1);
  for index in 0..length {
    let left = expected.get(index).copied().unwrap_or(0);
    let right = provided.get(index).copied().unwrap_or(0);
    diff |= left ^ right;
  }
  diff == 0
}

/// 32 字节随机 token 的 hex 形式（与 legacy `randomBytes(32).toString("hex")` 同形）。
pub fn generate_token() -> std::io::Result<String> {
  let mut bytes = [0u8; 32];
  let mut file = std::fs::File::open("/dev/urandom")?;
  file
    .read_exact(&mut bytes)
    .map_err(|error| std::io::Error::other(format!("/dev/urandom: {error}")))?;
  let mut token = String::with_capacity(64);
  for byte in bytes {
    token.push_str(&format!("{byte:02x}"));
  }
  Ok(token)
}

/// 已鉴权连接的集合。广播只发给已鉴权的连接。
pub struct ClientHub {
  next_id: AtomicU64,
  clients: Mutex<Vec<Arc<ClientConn>>>,
}

impl ClientHub {
  pub fn new() -> Arc<Self> {
    Arc::new(Self {
      next_id: AtomicU64::new(1),
      clients: Mutex::new(Vec::new()),
    })
  }

  pub fn connect(&self, stream: UnixStream) -> Arc<ClientConn> {
    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
    let (sender, receiver) = sync_channel(EVENT_QUEUE_DEPTH);
    let conn = Arc::new(ClientConn {
      id,
      stream,
      sender,
      authed: AtomicBool::new(false),
      closed: AtomicBool::new(false),
    });
    self
      .clients
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(Arc::clone(&conn));
    spawn_writer(Arc::clone(&conn), receiver);
    conn
  }

  pub fn disconnect(&self, conn: &Arc<ClientConn>) {
    conn.close();
    self
      .clients
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .retain(|candidate| candidate.id != conn.id);
  }

  pub fn broadcast(&self, frame: Vec<u8>) {
    let clients: Vec<Arc<ClientConn>> = self
      .clients
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    let mut slow: Vec<Arc<ClientConn>> = Vec::new();
    for conn in clients {
      if !conn.is_authed() {
        continue;
      }
      if !conn.try_send(frame.clone()) {
        slow.push(conn);
      }
    }
    for conn in slow {
      self.disconnect(&conn);
    }
  }
}

pub struct ClientConn {
  id: u64,
  stream: UnixStream,
  sender: SyncSender<Vec<u8>>,
  authed: AtomicBool,
  closed: AtomicBool,
}

impl ClientConn {
  pub fn is_authed(&self) -> bool {
    self.authed.load(Ordering::SeqCst)
  }

  pub fn is_closed(&self) -> bool {
    self.closed.load(Ordering::SeqCst)
  }

  /// 请求的响应必须送达：队列满时阻塞（这是客户端自己的队列）。
  pub fn send_blocking(&self, frame: Vec<u8>) -> bool {
    if self.is_closed() {
      return false;
    }
    self.sender.send(frame).is_ok()
  }

  /// 事件的发送绝不阻塞 PTY 读取线程：慢客户端会被断开，靠 reattach 补洞。
  pub fn try_send(&self, frame: Vec<u8>) -> bool {
    if self.is_closed() {
      return false;
    }
    match self.sender.try_send(frame) {
      Ok(()) => true,
      Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
  }

  pub fn close(&self) {
    if !self.closed.swap(true, Ordering::SeqCst) {
      let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
  }
}

fn spawn_writer(conn: Arc<ClientConn>, receiver: Receiver<Vec<u8>>) {
  let _ = std::thread::Builder::new()
    .name(format!("wand-render-write-{}", conn.id))
    .spawn(move || {
      while let Ok(frame) = receiver.recv() {
        if (&conn.stream).write_all(&frame).is_err() {
          break;
        }
      }
      conn.close();
    });
}

/// 事件出口：把协议事件编码一次，广播给所有已鉴权连接。
pub struct HubSink {
  hub: Arc<ClientHub>,
}

impl HubSink {
  pub fn new(hub: Arc<ClientHub>) -> Arc<Self> {
    Arc::new(Self { hub })
  }
}

impl EventSink for HubSink {
  fn publish(&self, event: Event) {
    match encode_frame(&event) {
      Ok(frame) => self.hub.broadcast(frame),
      // 事件编码失败只可能是内部类型问题，不能影响 PTY。
      Err(_) => {}
    }
  }
}

pub struct RenderServer {
  hub: Arc<ClientHub>,
  registry: Arc<RenderRegistry>,
  token: String,
  shutdown: SyncSender<ShutdownMode>,
}

impl RenderServer {
  pub fn new(
    hub: Arc<ClientHub>,
    registry: Arc<RenderRegistry>,
    token: String,
    shutdown: SyncSender<ShutdownMode>,
  ) -> Arc<Self> {
    Arc::new(Self {
      hub,
      registry,
      token,
      shutdown,
    })
  }

  /// 接受连接直到进程退出。单条连接的失败绝不影响别的连接与 PTY。
  pub fn serve(self: &Arc<Self>, listener: std::os::unix::net::UnixListener) {
    for stream in listener.incoming() {
      match stream {
        Ok(stream) => self.accept_connection(stream),
        // accept 失败（EMFILE / ECONNABORTED）不能让 daemon 退出。
        Err(_) => continue,
      }
    }
  }

  fn accept_connection(self: &Arc<Self>, stream: UnixStream) {
    let conn = self.hub.connect(stream);
    let server = Arc::clone(self);
    let read_conn = Arc::clone(&conn);
    let spawned = std::thread::Builder::new()
      .name(format!("wand-render-read-{}", conn.id))
      .spawn(move || server.read_loop(read_conn));
    if spawned.is_err() {
      self.hub.disconnect(&conn);
    }
  }

  fn read_loop(self: Arc<Self>, conn: Arc<ClientConn>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
      let read = match (&conn.stream).read(&mut chunk) {
        Ok(0) => break,
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
        Err(_) => break,
      };
      buffer.extend_from_slice(&chunk[..read]);
      loop {
        match decode_frame::<JsonValue>(&buffer) {
          Ok(Some((consumed, value))) => {
            buffer.drain(..consumed);
            self.handle_frame(&conn, value);
            if conn.is_closed() {
              self.hub.disconnect(&conn);
              return;
            }
          }
          Ok(None) => break,
          // 长度越界 / JSON 不合法：只关这条连接。
          Err(_) => {
            self.hub.disconnect(&conn);
            return;
          }
        }
      }
      if buffer.len() > MAX_REQUEST_BUFFER_BYTES {
        self.hub.disconnect(&conn);
        return;
      }
    }
    self.hub.disconnect(&conn);
  }

  fn handle_frame(self: &Arc<Self>, conn: &Arc<ClientConn>, value: JsonValue) {
    let request: Request = match serde_json::from_value(value.clone()) {
      Ok(request) => request,
      Err(error) => {
        let id = value
          .get("id")
          .and_then(JsonValue::as_u64)
          .unwrap_or(0) as u32;
        self.respond(conn, id, Err((ErrorCode::BadRequest, error.to_string())));
        return;
      }
    };

    // 鉴权：token 不对就立刻断开（连接建立后的第一个请求尤其如此）。
    if !tokens_match(&self.token, &request.token) {
      conn.close();
      return;
    }
    if request.protocol_version != RENDER_PROTOCOL_VERSION {
      self.respond(
        conn,
        request.id,
        Err((
          ErrorCode::ProtocolMismatch,
          format!(
            "protocol version {} is not supported (expected {})",
            request.protocol_version, RENDER_PROTOCOL_VERSION
          ),
        )),
      );
      return;
    }
    conn.authed.store(true, Ordering::SeqCst);
    self.dispatch(conn, request);
  }

  fn dispatch(self: &Arc<Self>, conn: &Arc<ClientConn>, request: Request) {
    let params = request.params.clone().unwrap_or(JsonValue::Null);
    let result = match request.method.as_str() {
      "hello" => to_json(self.registry.hello()),
      "ping" => Ok(serde_json::json!({ "pong": true })),
      "list" => self.list_with_budget(),
      "attach" => match parse_params::<AttachParams>(&params) {
        Ok(parsed) => match self.registry.attach(&parsed.session_id, parsed.after_seq) {
          Some(state) => to_json(AttachResult { state }),
          None => Err((
            ErrorCode::NotFound,
            format!("session {} not found", parsed.session_id),
          )),
        },
        Err(error) => Err(error),
      },
      "createOrAttach" => match parse_params::<CreateOrAttachParams>(&params) {
        Ok(parsed) => match self.registry.create_or_attach(&parsed) {
          Ok(result) => to_json(result),
          Err(error) => Err(map_render_error(error)),
        },
        Err(error) => Err(error),
      },
      "write" => match parse_params::<WriteParams>(&params) {
        Ok(parsed) => empty_result(self.registry.write(&parsed.session_id, &parsed.data)),
        Err(error) => Err(error),
      },
      "resize" => match parse_params::<ResizeParams>(&params) {
        Ok(parsed) => empty_result(
          self
            .registry
            .resize(&parsed.session_id, parsed.cols, parsed.rows),
        ),
        Err(error) => Err(error),
      },
      "kill" => match parse_params::<KillParams>(&params) {
        Ok(parsed) => empty_result(self.registry.kill(&parsed.session_id, parsed.signal.as_deref())),
        Err(error) => Err(error),
      },
      "forget" => match parse_params::<ForgetParams>(&params) {
        Ok(parsed) => {
          // 未知 session 与 legacy 一样算成功（幂等清理）。
          self.registry.forget(&parsed.session_id);
          Ok(empty_object())
        }
        Err(error) => Err(error),
      },
      "stats" => to_json(self.registry.stats()),
      "shutdown" => match parse_params::<ShutdownParams>(&params) {
        Ok(parsed) => {
          // 只把意图交给主线程：它是唯一的关闭决策点。提前在这里调
          // `begin_shutdown` 会把 shutting_down 置位，主线程随后接到 Drain 时会
          // 误判成「已经 drain 过」而直接升级为 now。
          let _ = self.shutdown.send(parsed.mode);
          Ok(empty_object())
        }
        Err(error) => Err(error),
      },
      other => Err((
        ErrorCode::UnsupportedMethod,
        format!("unsupported method {other}"),
      )),
    };
    self.respond(conn, request.id, result);
  }

  /// `list` 的响应体上限控制。
  ///
  /// `list` 会内联每个会话的 `output` (≤20 万字符) + `chunks` (≤20 万字符) + 完整
  /// 回滚快照（≤5000 行 × 列宽）：cols=1000 的满回滚会话单个状态就约 5MB，十几个
  /// 就会撞上 64MiB 的单帧上限。撞上时**不能让客户端拿到超时/死连接** —— 那等于整
  /// 个 daemon 无法被 adopt（`connect()` 必须先 `list`），`render.engine=rust` 会直
  /// 接起不来。所以这里逐级降级：完整 → 丢快照 → 只留元信息，完整状态仍由 `attach`
  /// 提供（协议 §1 里两个入口都是 SessionState 的来源）。
  fn list_with_budget(&self) -> DispatchResult {
    const ATTEMPTS: [(bool, bool); 3] = [(true, true), (false, true), (false, false)];
    let mut last_error = String::from("unknown");
    for (include_snapshot, include_journal) in ATTEMPTS {
      let sessions = self
        .registry
        .list_with_options(include_snapshot, include_journal);
      let value = match to_json(ListResult { sessions }) {
        Ok(value) => value,
        Err(error) => return Err(error),
      };
      match serde_json::to_vec(&value) {
        Ok(body) if body.len() as u64 <= MAX_FRAME_BYTES as u64 => {
          if !include_snapshot || !include_journal {
            eprintln!(
              "wand-render: list response exceeded MAX_FRAME_BYTES; degraded to snapshots={include_snapshot} journal={include_journal} (per-session full state stays available via attach)"
            );
          }
          return Ok(value);
        }
        Ok(body) => last_error = format!("{} bytes", body.len()),
        Err(error) => last_error = error.to_string(),
      }
    }
    Err((
      ErrorCode::Internal,
      format!(
        "list response exceeds MAX_FRAME_BYTES even without snapshots/journal ({last_error}); fetch sessions individually with attach"
      ),
    ))
  }

  fn respond(&self, conn: &Arc<ClientConn>, id: u32, result: DispatchResult) {
    let response = match result {
      Ok(value) => Response {
        id,
        ok: true,
        result: Some(value),
        error: None,
      },
      Err((code, message)) => Response {
        id,
        ok: false,
        result: None,
        error: Some(ErrorBody { code, message }),
      },
    };
    match encode_frame(&response) {
      Ok(frame) => {
        conn.send_blocking(frame);
      }
      // 编码失败绝不能静默吞掉：客户端会一直等到自己的 10s 超时，而且分不清
      // 「daemon 死了」和「响应太大」。先回一个 internal 错误帧（客户端至少拿到
      // 确定的失败原因），连错误帧都编不出来（例如失败的正是错误帧本身）就关掉
      // 连接，让对端立刻看到 close 而不是干等。
      Err(error) => {
        let fallback = Response {
          id,
          ok: false,
          result: None,
          error: Some(ErrorBody {
            code: ErrorCode::Internal,
            message: format!("response frame could not be encoded: {error}"),
          }),
        };
        if let Ok(frame) = encode_frame(&fallback) {
          conn.send_blocking(frame);
        } else {
          conn.close();
        }
      }
    }
  }
}

fn to_json<T: serde::Serialize>(value: T) -> DispatchResult {
  serde_json::to_value(value).map_err(|error| (ErrorCode::Internal, error.to_string()))
}

fn empty_object() -> JsonValue {
  JsonValue::Object(serde_json::Map::new())
}

fn empty_result(result: Result<(), RenderError>) -> DispatchResult {
  match result {
    Ok(()) => Ok(empty_object()),
    Err(error) => Err(map_render_error(error)),
  }
}

fn parse_params<T: DeserializeOwned>(params: &JsonValue) -> Result<T, (ErrorCode, String)> {
  let normalized = if params.is_null() {
    JsonValue::Object(serde_json::Map::new())
  } else {
    params.clone()
  };
  serde_json::from_value(normalized).map_err(|error| (ErrorCode::BadRequest, error.to_string()))
}

fn map_render_error(error: RenderError) -> (ErrorCode, String) {
  let code = match error.kind() {
    RenderErrorKind::NotFound => ErrorCode::NotFound,
    RenderErrorKind::BadRequest => ErrorCode::BadRequest,
    RenderErrorKind::Conflict => ErrorCode::Conflict,
    RenderErrorKind::Internal => ErrorCode::Internal,
  };
  (code, error.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::net::UnixStream;
  use std::time::Duration;
  use wand_render::sink::NullSink;

  #[test]
  fn token_comparison_is_exact() {
    assert!(tokens_match("abc123", "abc123"));
    assert!(!tokens_match("abc123", "abc124"));
    assert!(!tokens_match("abc123", "abc12"));
    assert!(!tokens_match("abc123", ""));
    assert!(!tokens_match("", "x"));
    assert!(tokens_match("", ""));
  }

  #[test]
  fn generated_tokens_are_32_bytes_of_hex() {
    let token = generate_token().expect("token");
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert_ne!(token, generate_token().expect("token"));
  }

  #[test]
  fn params_parsing_reports_bad_requests() {
    let error = parse_params::<WriteParams>(&serde_json::json!({ "sessionId": "s" }))
      .expect_err("missing data must fail");
    assert_eq!(error.0, ErrorCode::BadRequest);
    // 空 params 会被补成空对象，仍然报 badRequest 而不是内部错误。
    let error = parse_params::<ForgetParams>(&JsonValue::Null).expect_err("missing sessionId");
    assert_eq!(error.0, ErrorCode::BadRequest);
  }

  #[test]
  fn render_errors_map_to_protocol_codes() {
    assert_eq!(
      map_render_error(RenderError::NotFound("s".into())).0,
      ErrorCode::NotFound
    );
    assert_eq!(
      map_render_error(RenderError::Conflict("s".into())).0,
      ErrorCode::Conflict
    );
    assert_eq!(
      map_render_error(RenderError::BadRequest("s".into())).0,
      ErrorCode::BadRequest
    );
    assert_eq!(
      map_render_error(RenderError::Internal("s".into())).0,
      ErrorCode::Internal
    );
  }

  /// 超限响应必须回一个明确的 `internal` 错误帧，而不是被静默吞掉 ——
  /// 否则客户端只会看到自己的 10s 超时，甚至以为 daemon 挂了。
  #[test]
  fn oversized_response_falls_back_to_an_internal_error_frame() {
    let hub = ClientHub::new();
    let (server_side, mut client_side) = UnixStream::pair().expect("socketpair");
    let conn = hub.connect(server_side);
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    let (shutdown, _shutdown_rx) = sync_channel(1);
    let server = RenderServer::new(Arc::clone(&hub), Arc::clone(&registry), "token".into(), shutdown);

    let oversized = JsonValue::String("x".repeat(MAX_FRAME_BYTES as usize + 16));
    server.respond(&conn, 42, Ok(oversized));

    client_side
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while buffer.len() < 4 {
      match client_side.read(&mut chunk) {
        Ok(0) => break,
        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        Err(_) => break,
      }
    }
    let (_, response) = decode_frame::<Response>(&buffer)
      .expect("decode")
      .expect("one complete frame");
    assert_eq!(response.id, 42);
    assert!(!response.ok);
    assert_eq!(
      response.error.expect("error body").code,
      ErrorCode::Internal
    );
    registry.stop_maintenance();
  }
}
