//! unix socket 服务：帧编解码、鉴权、方法分发与事件广播。
//!
//! 关键约束（`docs/render-protocol.md` §3）：
//! - 第一个请求必须带正确 token，否则**立即关闭**这条连接；
//! - `protocolVersion` 必须是 1，否则回 `protocolMismatch`；
//! - 长度越界或 JSON 解析失败只关这一条连接，不影响别的连接与 PTY。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use wand_render::{security, EventSink, RenderError, RenderErrorKind, RenderRegistry};
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
  // A u8 truncates lengths modulo 256: appending 256 NUL bytes would otherwise
  // pass constant-time comparison when the common prefix is identical.
  let mut diff = expected.len() ^ provided.len();
  let length = expected.len().max(provided.len()).max(1);
  for index in 0..length {
    let left = expected.get(index).copied().unwrap_or(0);
    let right = provided.get(index).copied().unwrap_or(0);
    diff |= (left ^ right) as usize;
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
    spawn_writer(&conn, receiver);
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

/// 启动写线程。
///
/// 只持 `Weak<ClientConn>`：writer 线程若强引用 `ClientConn`，而 `ClientConn` 又持有
/// 这条 channel 的 sender，就构成「recv 等 sender、sender 等自己」的引用环 —— 连接断开
/// 后 `ClientConn` 永不释放，`UnixStream` 的 fd 与这个线程各泄漏一份。慢客户端反复
/// 重连时 fd 会一路涨到 EMFILE，表现就是「终端连不上」。
fn spawn_writer(conn: &Arc<ClientConn>, receiver: Receiver<Vec<u8>>) {
  let weak = Arc::downgrade(conn);
  let _ = std::thread::Builder::new()
    .name(format!("wand-render-write-{}", conn.id))
    .spawn(move || {
      while let Ok(frame) = receiver.recv() {
        // 最后一个强引用没了：连接已被释放，sender 也随之 drop。
        let Some(conn) = weak.upgrade() else { break };
        if (&conn.stream).write_all(&frame).is_err() {
          conn.close();
          break;
        }
      }
      if let Some(conn) = weak.upgrade() {
        conn.close();
      }
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
    // 事件编码失败只可能是内部类型问题，不能影响 PTY。
    if let Ok(frame) = encode_frame(&event) {
      self.hub.broadcast(frame);
    }
  }
}

pub struct RenderServer {
  hub: Arc<ClientHub>,
  registry: Arc<RenderRegistry>,
  token: String,
  shutdown: SyncSender<ShutdownMode>,
  /// 允许连接的对端 uid（协议 §9.5.2）。正常就是本进程 uid；单测把它改成别的
  /// 值来伪造「别的用户连过来」的场景。
  expected_peer_uid: AtomicU32,
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
      expected_peer_uid: AtomicU32::new(security::current_uid()),
    })
  }

  /// 接受连接直到进程退出。单条连接的失败绝不影响别的连接与 PTY。
  pub fn serve(self: &Arc<Self>, listener: std::os::unix::net::UnixListener) {
    let path = match listener.local_addr().ok().and_then(|addr| addr.as_pathname().map(|p| p.to_path_buf())) {
      Some(path) => path,
      None => { eprintln!("wand-render: listener has no socket path"); return; }
    };
    let mut listener = match wand_render::socket_keepalive::RecoveringListener::new(listener, path) {
      Ok(listener) => listener,
      Err(error) => { eprintln!("wand-render: cannot monitor socket: {error}"); return; }
    };
    loop {
      match listener.accept() {
        Ok((stream, _)) => self.accept_connection(stream),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Back off resource exhaustion instead of spinning at 100% CPU.
        Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
      }
    }
  }

  fn accept_connection(self: &Arc<Self>, stream: UnixStream) {
    // 协议 §9.5.2 的第一道闸：socket 派生在全局可写的 /tmp，抢注者 connect 之后
    // 第一个 hello 就能拿到 token。所以身份校验必须在**读取任何字节之前**完成，
    // 而且取不到凭据时要失败关闭。
    if let Err(reason) =
      security::ensure_peer_uid(&stream, self.expected_peer_uid.load(Ordering::SeqCst))
    {
      eprintln!("wand-render: rejected a connection before reading its token: {reason}");
      let _ = stream.shutdown(std::net::Shutdown::Both);
      return;
    }
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
        let method = value
          .get("method")
          .and_then(JsonValue::as_str)
          .unwrap_or("unknown");
        self.respond(conn, id, method, Err((ErrorCode::BadRequest, error.to_string())));
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
        &request.method,
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
      "list" => self.list_response(),
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
    self.respond(conn, request.id, &request.method, result);
  }

  /// `list` 的响应体。
  ///
  /// 每个会话的快照已经由 `RenderRegistry::list_sessions` 按 §9.1.1 裁剪到 64KiB，
  /// 这里只负责帧上限检查：`output`/`chunks` 不能再砍（§9.1.1 要求它们保持 §4 上限），
  /// 所以真的放不下时回一个 `internal` 错误帧说明原因，让客户端调用方明确知道
  /// 「太大」而不是干等 10s 超时（§9.1.3）。
  fn list_response(&self) -> DispatchResult {
    self.list_response_with_frame_budget(MAX_FRAME_BYTES as usize)
  }

  /// 帧上限可注入，单测用一个小预算就能走到超限分支（真造 140 个满会话不现实）。
  fn list_response_with_frame_budget(&self, max_bytes: usize) -> DispatchResult {
    let value = to_json(ListResult {
      sessions: self.registry.list_sessions(),
    })?;
    match serde_json::to_vec(&value) {
      Ok(body) if body.len() <= max_bytes => Ok(value),
      Ok(body) => Err((
        ErrorCode::Internal,
        format!(
          "list response is {} bytes, over the {max_bytes}-byte frame limit (MAX_FRAME_BYTES); fetch large session states individually with attach",
          body.len()
        ),
      )),
      Err(error) => Err((ErrorCode::Internal, error.to_string())),
    }
  }

  fn respond(&self, conn: &Arc<ClientConn>, id: u32, method: &str, result: DispatchResult) {
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
            // 带上方法名与原始原因：客户端必须能区分「响应太大」与其他内部错误，
            // 否则只能看到自己的 10s 超时（协议 §9.1.3）。
            message: format!("{method} response could not be encoded as a frame: {error}"),
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
    assert!(!tokens_match("abc123", &format!("abc123{}", "\0".repeat(256))));
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
    let (server, registry, conn, mut client_side) = test_server();

    let oversized = JsonValue::String("x".repeat(MAX_FRAME_BYTES as usize + 16));
    server.respond(&conn, 42, "list", Ok(oversized));

    let response = read_response(&mut client_side);
    assert_eq!(response.id, 42);
    assert!(!response.ok);
    let message = response.error.expect("error body");
    assert_eq!(message.code, ErrorCode::Internal);
    // 说明原因：客户端要能看出是「帧太大」，而不是随便一个内部错误。
    assert!(
      message.message.contains("list") && message.message.contains("MAX_FRAME_BYTES"),
      "unhelpful message: {}",
      message.message
    );
    registry.stop_maintenance();
  }

  /// §9.1.3：`list` 响应超过单帧上限时必须报 `internal`，而不是砍掉 journal 静默降级。
  #[test]
  fn oversized_list_response_reports_an_internal_error() {
    let (server, registry, conn, mut client_side) = test_server();
    let error = server
      .list_response_with_frame_budget(8)
      .expect_err("a tiny budget must trip the frame limit");
    assert_eq!(error.0, ErrorCode::Internal);
    assert!(
      error.1.contains("frame limit"),
      "unhelpful message: {}",
      error.1
    );

    // 整条路径：错误也会真的变成一个 internal 错误帧发到对端。
    server.respond(&conn, 7, "list", Err(error));
    let response = read_response(&mut client_side);
    assert_eq!(response.id, 7);
    assert!(!response.ok);
    assert_eq!(response.error.expect("error body").code, ErrorCode::Internal);
    registry.stop_maintenance();
  }

  #[test]
  fn empty_list_response_is_ok() {
    let (server, registry, _conn, _client) = test_server();
    let value = server.list_response().expect("empty list");
    assert_eq!(value["sessions"], JsonValue::Array(Vec::new()));
    registry.stop_maintenance();
  }

  /// 回归：断开连接必须真正释放它的 fd 与写线程。
  ///
  /// 曾经的 writer 线程强引用 `ClientConn`，而 `ClientConn` 持着同一个 channel 的
  /// sender —— 引用环让每条断开过的连接的 socket fd 与线程永久泄漏（线上 0.1.0 daemon
  /// 25 小时累积 594 个 fd / 610 个线程，最终会打到 EMFILE 让新连接连不上）。
  #[test]
  fn disconnecting_releases_the_socket_fd_and_writer_thread() {
    let hub = ClientHub::new();
    let baseline = open_fd_count();
    for _ in 0..32 {
      let (server_side, client_side) = UnixStream::pair().expect("socketpair");
      let conn = hub.connect(server_side);
      hub.disconnect(&conn);
      drop(conn);
      drop(client_side);
    }
    let after = wait_for_fd_count_at_most(baseline + 2);
    assert!(
      after <= baseline + 2,
      "disconnected clients leaked fds: baseline={baseline} after={after}"
    );
  }

  /// 让 fd 数回落到上限以内（写线程退栈是异步的）；超时返回当时的值。
  fn wait_for_fd_count_at_most(limit: usize) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
      let count = open_fd_count();
      if count <= limit || std::time::Instant::now() >= deadline {
        return count;
      }
      std::thread::sleep(Duration::from_millis(20));
    }
  }

  /// 进程当前打开的 fd 数（0..4096 里能 `fcntl(F_GETFD)` 命中的个数）。
  fn open_fd_count() -> usize {
    (0..4096)
      .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
      .count()
  }

  /// §9.5.2：uid 不符的连接必须在读到 token 之前就被关掉。
  #[test]
  fn foreign_peer_is_rejected_before_the_token_is_read() {
    let (server, registry, _conn, _client_side) = test_server();
    // 对端就是本进程，所以把期望值改成别的 uid 来伪造「另一个用户连过来」。
    server
      .expected_peer_uid
      .store(security::current_uid().wrapping_add(1), Ordering::SeqCst);

    let (peer, mut other) = UnixStream::pair().expect("socketpair");
    // 必须在服务端关闭对端之前设好读超时：连接已被 shutdown 之后 macOS 会对
    // `set_read_timeout` 报 EINVAL。
    other
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("read timeout");
    server.accept_connection(peer);

    // 送一个**完全合法**的请求：拒绝必须发生在鉴权之前，且不能有任何回复。
    let request = Request {
      id: 1,
      token: "token".into(),
      protocol_version: RENDER_PROTOCOL_VERSION,
      method: "hello".into(),
      params: None,
    };
    let _ = other.write_all(&encode_frame(&request).expect("encode"));
    let mut buffer = [0u8; 64];
    let read = other.read(&mut buffer).unwrap_or(0);
    assert_eq!(read, 0, "a foreign peer must be disconnected without an answer");
    registry.stop_maintenance();
  }

  /// 同 uid 的连接必须正常通过身份校验并拿到 hello 响应。
  #[test]
  fn same_uid_peer_is_served_after_the_identity_check() {
    let (server, registry, _conn, _client) = test_server();
    let (peer, mut other) = UnixStream::pair().expect("socketpair");
    other
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("read timeout");
    server.accept_connection(peer);

    let request = Request {
      id: 5,
      token: "token".into(),
      protocol_version: RENDER_PROTOCOL_VERSION,
      method: "hello".into(),
      params: None,
    };
    other
      .write_all(&encode_frame(&request).expect("encode"))
      .expect("write");
    let response = read_response(&mut other);
    assert_eq!(response.id, 5);
    assert!(response.ok, "same-uid peer must be served: {response:?}");
    registry.stop_maintenance();
  }

  /// 测试用服务器：返回 (Arc<RenderServer>, registry, 已连接的服务端 conn, 客户端流)。
  fn test_server() -> (
    Arc<RenderServer>,
    Arc<RenderRegistry>,
    Arc<ClientConn>,
    UnixStream,
  ) {
    let hub = ClientHub::new();
    let (server_side, client_side) = UnixStream::pair().expect("socketpair");
    let conn = hub.connect(server_side);
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    let (shutdown, _shutdown_rx) = sync_channel(1);
    let server = RenderServer::new(Arc::clone(&hub), Arc::clone(&registry), "token".into(), shutdown);
    (server, registry, conn, client_side)
  }

  fn read_response(stream: &mut UnixStream) -> Response {
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while decode_frame::<Response>(&buffer).expect("decode").is_none() {
      match stream.read(&mut chunk) {
        Ok(0) => panic!("connection closed before a response arrived"),
        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        Err(error) => panic!("read failed: {error}"),
      }
    }
    decode_frame::<Response>(&buffer)
      .expect("decode")
      .expect("one complete frame")
      .1
  }
}
