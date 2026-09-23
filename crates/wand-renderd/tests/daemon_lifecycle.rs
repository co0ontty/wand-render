//! `wand-render` 守护进程的生命周期端到端验证（协议 §9.3 / §9.4）。
//!
//! 这里跑**真实二进制**（`CARGO_BIN_EXE_wand-render`）+ 真实 PTY，因为「drain 保留
//! 进程与 PTY、第二个信号才退出」这条语义恰恰是进程级行为：单元测试里主线程还在，
//! 一旦主线程真的返回，PTY master fd 就被关掉，子进程收 SIGHUP 死掉 —— 只有跑起来
//! 才能证明没发生这件事。

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wand_render::render_paths;
use wand_render_protocol::{
  decode_frame, encode_frame, ErrorCode, Event, Request, Response, RENDER_PROTOCOL_VERSION,
};

/// 单次 socket 读的等待上限：轮询用，真正的超时由各测试自己控制。
const READ_POLL: Duration = Duration::from_millis(200);

struct Daemon {
  child: Child,
  socket: UnixStream,
  token: String,
  paths: wand_render::RenderPaths,
  dir: PathBuf,
  buffer: Vec<u8>,
  responses: Vec<Response>,
  events: Vec<Event>,
  next_id: u32,
}

impl Daemon {
  fn start(dir: &Path) -> Self {
    std::fs::create_dir_all(dir).expect("config dir");
    let config = dir.join("config.json");
    // 让 config 真的存在：两侧的路径派生都会走 realpath，测试与守护进程一致。
    std::fs::write(&config, "{}").expect("write config");
    let paths = render_paths(&config);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wand-render"))
      .arg("-c")
      .arg(&config)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      // 继承 stderr：失败时测试输出里能直接看到守护进程自己的诊断。
      .stderr(Stdio::inherit())
      .spawn()
      .expect("spawn wand-render");
    let (socket, token) = connect_when_ready(&mut child, &paths);
    Self {
      child,
      socket,
      token,
      paths,
      dir: dir.to_path_buf(),
      buffer: Vec::new(),
      responses: Vec::new(),
      events: Vec::new(),
      next_id: 1,
    }
  }

  fn pid(&self) -> u32 {
    self.child.id()
  }

  fn is_running(&mut self) -> bool {
    self.child.try_wait().expect("try_wait").is_none()
  }

  fn request(&mut self, method: &str, params: Value) -> Response {
    let id = self.next_id;
    self.next_id += 1;
    let request = Request {
      id,
      token: self.token.clone(),
      protocol_version: RENDER_PROTOCOL_VERSION,
      method: method.to_string(),
      params: if params.is_null() { None } else { Some(params) },
    };
    self
      .socket
      .write_all(&encode_frame(&request).expect("encode request"))
      .expect("write request");
    self.consume_until(|responses, _| responses.iter().any(|response| response.id == id));
    self
      .responses
      .iter()
      .find(|response| response.id == id)
      .cloned()
      .expect("response for the request we just sent")
  }

  /// 读到条件成立或超时（超时不 panic，由调用方断言），返回是否成立。
  fn consume_until<F: FnMut(&Vec<Response>, &Vec<Event>) -> bool>(
    &mut self,
    mut condition: F,
  ) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
      if condition(&self.responses, &self.events) {
        return true;
      }
      if Instant::now() >= deadline {
        return false;
      }
      let mut chunk = [0u8; 8192];
      match self.socket.read(&mut chunk) {
        Ok(0) => return condition(&self.responses, &self.events),
        Ok(read) => {
          self.buffer.extend_from_slice(&chunk[..read]);
          self.drain_frames();
        }
        // 读超时（轮询）：继续等。
        Err(_) => {}
      }
    }
  }

  fn drain_frames(&mut self) {
    loop {
      match decode_frame::<Value>(&self.buffer) {
        Ok(Some((consumed, value))) => {
          self.buffer.drain(..consumed);
          if value.get("id").is_some() {
            self
              .responses
              .push(serde_json::from_value(value).expect("response"));
          } else {
            self.events.push(serde_json::from_value(value).expect("event"));
          }
        }
        Ok(None) => return,
        Err(error) => panic!("frame decode failed: {error}"),
      }
    }
  }

  fn wait_for_exit(&mut self, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
      if self.child.try_wait().expect("try_wait").is_some() {
        return true;
      }
      std::thread::sleep(Duration::from_millis(20));
    }
    false
  }

  fn sent_signal(&self, session_id: &str) -> Option<i32> {
    self.events.iter().find_map(|event| match event {
      Event::Exit {
        session_id: id,
        signal,
        ..
      } if id == session_id => *signal,
      _ => None,
    })
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    // 测试失败时也要收干净：先杀守护进程，它退出会带走自己的 PTY。
    let _ = self.child.kill();
    let _ = self.child.wait();
    for path in [
      &self.paths.socket_path,
      &self.paths.token_path,
      &self.paths.pid_path,
      &self.paths.meta_path,
    ] {
      let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

/// 等 socket 与 token 同时就绪（守护进程先 bind 再写 token）。
fn connect_when_ready(child: &mut Child, paths: &wand_render::RenderPaths) -> (UnixStream, String) {
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    if let Ok(token) = std::fs::read_to_string(&paths.token_path) {
      let token = token.trim().to_string();
      if !token.is_empty() {
        if let Ok(socket) = UnixStream::connect(&paths.socket_path) {
          socket.set_read_timeout(Some(READ_POLL)).expect("read timeout");
          return (socket, token);
        }
      }
    }
    if let Some(status) = child.try_wait().expect("try_wait") {
      panic!("wand-render exited before it became ready: {status}");
    }
    assert!(
      Instant::now() < deadline,
      "wand-render never became ready on {}",
      paths.socket_path.display()
    );
    std::thread::sleep(Duration::from_millis(20));
  }
}

/// `sleep 30`：足够长，能证明 drain 期间它一直活着。
fn create_params(session_id: &str) -> Value {
  let mut env: BTreeMap<String, String> = BTreeMap::new();
  env.insert("PATH".to_string(), "/usr/bin:/bin:/usr/sbin:/sbin".to_string());
  env.insert(
    "HOME".to_string(),
    std::env::temp_dir().to_string_lossy().to_string(),
  );
  env.insert("TERM".to_string(), "xterm-256color".to_string());
  json!({
    "sessionId": session_id,
    "file": "/bin/sh",
    "args": ["-c", "sleep 30"],
    "cwd": std::env::temp_dir().to_string_lossy(),
    "env": env,
    "name": "xterm-256color",
    "cols": 80,
    "rows": 24,
  })
}

fn create_session(daemon: &mut Daemon, session_id: &str) -> u32 {
  let response = daemon.request("createOrAttach", create_params(session_id));
  assert!(response.ok, "createOrAttach failed: {response:?}");
  let state = &response.result.expect("result")["state"];
  assert_eq!(state["status"], "running");
  let pid = state["pid"].as_u64().expect("pid") as u32;
  assert!(pid > 0, "a PTY session must have a pid");
  pid
}

/// 进程是否仍然存在且不是僵尸（僵尸已经不再运行，但 `kill(pid, 0)` 仍会成功）。
fn process_is_alive(pid: u32) -> bool {
  if pid == 0 {
    return false;
  }
  if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
    return false;
  }
  match Command::new("ps")
    .args(["-o", "state=", "-p", &pid.to_string()])
    .output()
  {
    Ok(output) => {
      let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
      !state.is_empty() && !state.starts_with('Z')
    }
    // 查不到状态就不假装它死了。
    Err(_) => true,
  }
}

fn temp_dir(tag: &str) -> PathBuf {
  static COUNTER: AtomicU64 = AtomicU64::new(1);
  std::env::temp_dir().join(format!(
    "wand-renderd-lifecycle-{tag}-{}-{}",
    std::process::id(),
    COUNTER.fetch_add(1, Ordering::Relaxed)
  ))
}

/// §9.3：drain 之后进程与 PTY 都活着、attach 仍返回 running、新会话被拒；
/// 最后一个会话退出后进程自行结束。
#[test]
fn drain_keeps_the_daemon_and_the_pty_alive_until_the_last_session_exits() {
  let mut daemon = Daemon::start(&temp_dir("drain"));
  assert!(daemon.request("hello", Value::Null).ok);

  let pty_pid = create_session(&mut daemon, "s1");
  assert!(process_is_alive(pty_pid));

  let drained = daemon.request("shutdown", json!({ "mode": "drain" }));
  assert!(drained.ok, "shutdown(drain) must be answered: {drained:?}");

  std::thread::sleep(Duration::from_millis(400));
  assert!(daemon.is_running(), "drain must not stop the daemon");
  assert!(
    process_is_alive(pty_pid),
    "drain must not kill the running PTY (its output would be lost with it)"
  );

  let attached = daemon.request("attach", json!({ "sessionId": "s1" }));
  assert!(attached.ok, "attach must keep working while draining");
  assert_eq!(
    attached.result.expect("result")["state"]["status"],
    "running",
    "a draining daemon must still report its running sessions"
  );

  let rejected = daemon.request("createOrAttach", create_params("s2"));
  assert!(!rejected.ok, "new sessions must be refused while draining");
  assert_eq!(
    rejected.error.expect("error body").code,
    ErrorCode::Conflict
  );

  // 不传 signal 的 kill 是 SIGHUP（§9.4）；最后一个会话退出后守护进程自行结束。
  let killed = daemon.request("kill", json!({ "sessionId": "s1" }));
  assert!(killed.ok, "kill failed: {killed:?}");
  assert!(
    daemon.consume_until(|_, events| events.iter().any(|event| matches!(
      event,
      Event::Exit { session_id, .. } if session_id == "s1"
    ))),
    "the exit event never arrived"
  );
  assert_eq!(
    daemon.sent_signal("s1"),
    Some(libc::SIGHUP),
    "kill without a signal must deliver SIGHUP"
  );
  assert!(
    daemon.wait_for_exit(Duration::from_secs(10)),
    "the daemon must stop once the last session exited"
  );
  assert!(
    !process_is_alive(pty_pid),
    "the PTY must be gone together with the daemon"
  );
}

/// §9.3：`shutdown { mode: "now" }` 杀掉所有 PTY 后退出。
#[test]
fn shutdown_now_kills_the_ptys_and_exits() {
  let mut daemon = Daemon::start(&temp_dir("now"));
  let pty_pid = create_session(&mut daemon, "s1");

  let stopped = daemon.request("shutdown", json!({ "mode": "now" }));
  assert!(stopped.ok, "shutdown(now) must be answered: {stopped:?}");
  assert!(
    daemon.wait_for_exit(Duration::from_secs(10)),
    "shutdown(now) must stop the daemon"
  );
  assert!(!process_is_alive(pty_pid), "shutdown(now) must kill the PTY");
}

/// §9.3：SIGTERM 等价 drain（进程与 PTY 都留下），第二次信号才真退出。
#[test]
fn sigterm_drains_and_the_second_signal_stops_the_daemon() {
  let mut daemon = Daemon::start(&temp_dir("signals"));
  let pty_pid = create_session(&mut daemon, "s1");

  unsafe {
    libc::kill(daemon.pid() as libc::pid_t, libc::SIGTERM);
  }
  std::thread::sleep(Duration::from_millis(400));
  assert!(daemon.is_running(), "the first SIGTERM must equal drain");
  assert!(process_is_alive(pty_pid), "drain must keep the PTY alive");

  // drain 的证据：新会话被拒。
  let rejected = daemon.request("createOrAttach", create_params("s2"));
  assert!(!rejected.ok);
  assert_eq!(
    rejected.error.expect("error body").code,
    ErrorCode::Conflict
  );

  // 第二次信号 == now。
  unsafe {
    libc::kill(daemon.pid() as libc::pid_t, libc::SIGTERM);
  }
  assert!(
    daemon.wait_for_exit(Duration::from_secs(10)),
    "the second signal must stop the daemon"
  );
  assert!(!process_is_alive(pty_pid));
}
