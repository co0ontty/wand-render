//! 单个 PTY 会话：所有权、读取线程、有界 journal、VT 屏幕模型与快照。
//!
//! 锁的约定（**任何两个锁都不得嵌套持有**，避免读取线程与维护线程互相卡死）：
//!
//! - `state`：journal / 屏幕模型 / 快照 / 状态。读取线程与写路径都会短暂持有；
//!   快照重算也在其中完成，保证「基线 + pending」自洽。
//! - `writer`：PTY 输入侧。**必须在 state 之外**：PTY 缓冲区满时 `write` 会阻塞，
//!   若持着 state 写就会把读取线程一起堵死（它也要拿 state）。
//! - `master`：只在 resize 时用到，同样与 state 分开。
//!
//! 读取线程用 `Instant`/`Condvar` 与退出线程协调：先记录退出状态，等读取线程
//! 把剩余输出吐完（有上限），最后才广播 `exit`；否则客户端可能先收到 exit。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use vt100::Parser;
use wand_render_protocol::{
  Chunk, CreateOrAttachParams, Event, PendingOp, SessionState, SessionStatus, TerminalSnapshot,
  PTY_OUTPUT_MAX_CHARS, SCROLLBACK_LINES, SNAPSHOT_PENDING_MAX_CHARS, SNAPSHOT_PENDING_MAX_OPS,
  SNAPSHOT_QUIET_MS,
};

use crate::bounds::{ChunkWindow, TextWindow};
use crate::error::RenderError;
use crate::marker::MarkerStripper;
#[cfg(unix)]
use crate::signal;
use crate::sink::EventSink;
use crate::snapshot::{self, DecModeTracker};
use crate::utf8::IncrementalUtf8Decoder;

/// 读取线程读取缓冲大小：足够大以减少系统调用，又不至于让单帧内存过胖。
const READ_BUFFER_BYTES: usize = 32 * 1024;
/// 退出后等读取线程排空残余输出的上限。
const READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// 取锁时把「中毒」当成正常状态：会话数据本身没有跨会话不变量，
/// 一个 panic 过的线程不该让其它会话的 attach 全部失败。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Inner {
  status: SessionStatus,
  exit_code: Option<i32>,
  cols: u16,
  rows: u16,
  seq: u64,
  output: TextWindow,
  chunks: ChunkWindow,
  parser: Parser,
  /// 上一次 checkpoint 拍下的基线屏幕。**冻结**：它只反映 checkpoint 那一刻的屏幕，
  /// 之后的操作走 `pending`（协议要求「先写 data，再按序重放 pending」）。
  baseline: TerminalSnapshot,
  pending: Vec<PendingOp>,
  pending_chars: usize,
  snapshot_dirty: bool,
  last_write_at: Option<Instant>,
  marker: Option<MarkerStripper>,
  modes: DecModeTracker,
}

impl Inner {
  fn pending_over_budget(&self) -> bool {
    self.pending_chars > SNAPSHOT_PENDING_MAX_CHARS || self.pending.len() > SNAPSHOT_PENDING_MAX_OPS
  }

  /// checkpoint：把**当前**屏幕固化成新基线，并丢弃已经落进基线的 pending。
  ///
  /// 关键点：基线取当前屏幕，所以之前累积的 pending 不能再交出去（它们已经在屏幕里了，
  /// 重放会写两遍）；同时基线之后新到的操作会继续进 pending，保证
  /// 「baseline + pending == 当前屏幕」在任何时刻都成立 —— 这是重连不丢输出的前提。
  fn checkpoint(&mut self) {
    self.baseline = snapshot::build_baseline(self.parser.screen(), self.modes.autowrap());
    self.pending.clear();
    self.pending_chars = 0;
    self.snapshot_dirty = false;
  }

  /// 供读取的快照：冻结基线 + 基线之后的操作。
  fn snapshot_for_read(&self) -> TerminalSnapshot {
    let mut snapshot = self.baseline.clone();
    snapshot.pending = self.pending.clone();
    snapshot
  }

  fn push_pending(&mut self, data: &str) {
    // 相邻 data 合成一条：客户端逐条重放 pending，条目越少越快。
    match self.pending.last_mut() {
      Some(PendingOp::Data { data: last }) => last.push_str(data),
      _ => self.pending.push(PendingOp::Data {
        data: data.to_string(),
      }),
    }
    self.pending_chars += data.chars().count();
  }
}

struct ChildExit {
  exit_code: Option<i32>,
  signal: Option<i32>,
}

pub struct Session {
  session_id: String,
  incarnation_id: String,
  pid: u32,
  launch_marker_token: Option<String>,
  retired: AtomicBool,
  state: Mutex<Inner>,
  writer: Mutex<Option<Box<dyn Write + Send>>>,
  master: Mutex<Option<Box<dyn MasterPty + Send>>>,
  reader_done: (Mutex<bool>, Condvar),
  sink: Arc<dyn EventSink>,
}

impl Session {
  /// 新建 PTY 会话。返回时读取线程与退出监视线程都已经在跑。
  pub fn spawn(
    params: &CreateOrAttachParams,
    sink: Arc<dyn EventSink>,
  ) -> Result<Arc<Self>, RenderError> {
    if params.session_id.is_empty() {
      return Err(RenderError::BadRequest("sessionId is required".into()));
    }
    if params.file.is_empty() {
      return Err(RenderError::BadRequest("file is required".into()));
    }
    if params.cols == 0 || params.rows == 0 {
      return Err(RenderError::BadRequest(format!(
        "invalid terminal size {}x{}",
        params.cols, params.rows
      )));
    }
    // cwd 不存在时必须报错，而不是像 portable-pty 那样静默回落到 HOME。
    if !params.cwd.is_empty() && !Path::new(&params.cwd).is_dir() {
      return Err(RenderError::BadRequest(format!(
        "cwd {} is not a directory",
        params.cwd
      )));
    }

    let pty_system = native_pty_system();
    let pair = pty_system
      .openpty(PtySize {
        rows: params.rows,
        cols: params.cols,
        pixel_width: 0,
        pixel_height: 0,
      })
      .map_err(|error| RenderError::Spawn(format!("openpty failed: {error}")))?;

    let mut command = CommandBuilder::new(&params.file);
    command.args(&params.args);
    if !params.cwd.is_empty() {
      command.cwd(&params.cwd);
    }
    // legacy node-pty 用请求里的 env 覆盖整份环境变量，Render 自身的启动环境
    // 不会泄漏进用户会话；这里保持一致（`CommandBuilder::new` 默认继承本进程 env）。
    command.env_clear();
    for (key, value) in &params.env {
      command.env(key, value);
    }
    if !params.cwd.is_empty() {
      command.env("PWD", &params.cwd);
    }
    let term = if !params.name.is_empty() {
      params.name.clone()
    } else {
      params
        .env
        .get("TERM")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| "xterm-color".to_string())
    };
    command.env("TERM", term);

    let child = pair
      .slave
      .spawn_command(command)
      .map_err(|error| RenderError::Spawn(format!("spawn {} failed: {error}", params.file)))?;
    // 父进程必须放开 slave，否则子进程退出后 master 侧读不到 EOF/EIO。
    drop(pair.slave);

    let reader = match pair.master.try_clone_reader() {
      Ok(reader) => reader,
      Err(error) => {
        force_kill(child.process_id().unwrap_or(0));
        return Err(RenderError::Spawn(format!("pty reader failed: {error}")));
      }
    };
    let writer = match pair.master.take_writer() {
      Ok(writer) => writer,
      Err(error) => {
        force_kill(child.process_id().unwrap_or(0));
        return Err(RenderError::Spawn(format!("pty writer failed: {error}")));
      }
    };

    let pid = child.process_id().unwrap_or(0);
    let marker = params
      .launch_marker_token
      .as_deref()
      .filter(|token| !token.is_empty())
      .map(MarkerStripper::new);

    let mut inner = Inner {
      status: SessionStatus::Running,
      exit_code: None,
      cols: params.cols,
      rows: params.rows,
      seq: 0,
      output: TextWindow::new(PTY_OUTPUT_MAX_CHARS),
      chunks: ChunkWindow::new(PTY_OUTPUT_MAX_CHARS),
      parser: Parser::new(params.rows, params.cols, SCROLLBACK_LINES),
      baseline: TerminalSnapshot {
        version: snapshot::VERSION,
        data: String::new(),
        cols: params.cols,
        rows: params.rows,
        pending: Vec::new(),
      },
      pending: Vec::new(),
      pending_chars: 0,
      snapshot_dirty: true,
      last_write_at: None,
      marker,
      modes: DecModeTracker::new(),
    };
    inner.checkpoint();

    let session = Arc::new(Session {
      session_id: params.session_id.clone(),
      incarnation_id: new_incarnation_id(),
      pid,
      launch_marker_token: params.launch_marker_token.clone(),
      retired: AtomicBool::new(false),
      state: Mutex::new(inner),
      writer: Mutex::new(Some(writer)),
      master: Mutex::new(Some(pair.master)),
      reader_done: (Mutex::new(false), Condvar::new()),
      sink,
    });

    let reader_session = Arc::clone(&session);
    let read_thread = std::thread::Builder::new()
      .name(format!("wand-render-read-{}", session.session_id))
      .spawn(move || reader_session.read_loop(reader));
    if let Err(error) = read_thread {
      force_kill(pid);
      return Err(RenderError::Spawn(format!("reader thread failed: {error}")));
    }

    let exit_session = Arc::clone(&session);
    let exit_thread = std::thread::Builder::new()
      .name(format!("wand-render-exit-{}", session.session_id))
      .spawn(move || exit_session.exit_loop(child));
    if let Err(error) = exit_thread {
      force_kill(pid);
      return Err(RenderError::Spawn(format!("exit thread failed: {error}")));
    }

    Ok(session)
  }

  pub fn is_running(&self) -> bool {
    lock(&self.state).status == SessionStatus::Running
  }

  /// 快照 `SessionState`；`chunks` 只包含 `seq > after_seq` 的部分（补洞数据）。
  ///
  /// 这里返回的是**完整**状态（协议 §9.1.4）：`list` 的快照裁剪只发生在
  /// `RenderRegistry::list_sessions`，`attach` / `createOrAttach` 都必须走这里。
  pub fn state(&self, after_seq: u64) -> SessionState {
    let inner = lock(&self.state);
    SessionState {
      session_id: self.session_id.clone(),
      incarnation_id: self.incarnation_id.clone(),
      pid: self.pid,
      status: inner.status,
      exit_code: inner.exit_code,
      cols: inner.cols,
      rows: inner.rows,
      seq: inner.seq,
      output: inner.output.to_string_value(),
      chunks: inner
        .chunks
        .iter()
        .filter(|(seq, _)| *seq > after_seq)
        .map(|(seq, data)| Chunk {
          data: data.to_string(),
          seq,
        })
        .collect(),
      terminal_snapshot: Some(inner.snapshot_for_read()),
      launch_marker_token: self.launch_marker_token.clone(),
    }
  }

  pub fn live_bytes(&self) -> u64 {
    let inner = lock(&self.state);
    (inner.output.bytes() + inner.chunks.bytes()) as u64
  }

  pub fn write(&self, data: &str) -> Result<(), RenderError> {
    if !self.is_running() {
      return Err(RenderError::Conflict(format!(
        "session {} is not running",
        self.session_id
      )));
    }
    let mut writer = lock(&self.writer);
    match writer.as_mut() {
      Some(writer) => {
        writer.write_all(data.as_bytes())?;
        writer.flush()?;
        Ok(())
      }
      None => Err(RenderError::Conflict(format!(
        "session {} has no pty writer",
        self.session_id
      ))),
    }
  }

  pub fn resize(&self, cols: u16, rows: u16) -> Result<(), RenderError> {
    if cols == 0 || rows == 0 {
      return Err(RenderError::BadRequest(format!(
        "invalid terminal size {cols}x{rows}"
      )));
    }
    {
      let mut inner = lock(&self.state);
      if inner.status != SessionStatus::Running {
        return Err(RenderError::Conflict(format!(
          "session {} is not running",
          self.session_id
        )));
      }
      inner.cols = cols;
      inner.rows = rows;
      inner.parser.screen_mut().set_size(rows, cols);
      inner.pending.push(PendingOp::Resize { cols, rows });
      // 尺寸变化会让旧基线失效：下一次静默窗口一定会重算。
      inner.snapshot_dirty = true;
    }
    let master = lock(&self.master);
    if let Some(master) = master.as_ref() {
      master
        .resize(PtySize {
          rows,
          cols,
          pixel_width: 0,
          pixel_height: 0,
        })
        .map_err(|error| RenderError::Internal(format!("pty resize failed: {error}")))?;
    }
    Ok(())
  }

  /// 信号名 → 信号，投递给**子进程所在的进程组**。
  ///
  /// 不传 signal 时期望的语义是「关闭终端」，所以默认值是 SIGHUP（协议 §9.4），
  /// 与 node-pty 的 `kill()` 和 legacy daemon 一致。
  #[cfg(unix)]
  pub fn kill(&self, signal_name: Option<&str>) -> Result<(), RenderError> {
    let signo = match signal_name {
      Some(name) if !name.trim().is_empty() => signal::number(name)
        .ok_or_else(|| RenderError::BadRequest(format!("unsupported signal {name}")))?,
      _ => signal::DEFAULT,
    };
    kill_pid(self.pid, signo);
    Ok(())
  }

  /// Windows / ConPTY 第一阶段不支持按信号投递，明确报错而不是静默无操作。
  #[cfg(not(unix))]
  pub fn kill(&self, _signal_name: Option<&str>) -> Result<(), RenderError> {
    Err(RenderError::Internal(
      "kill requires POSIX signals, which this platform does not implement yet (see docs/render-protocol.md §9.5.3)"
        .to_string(),
    ))
  }

  /// 从注册表摘除：停止记录/广播，并回收 PTY 句柄与子进程（与 legacy
  /// `forget` 一样先送 SIGTERM）。
  pub fn retire(&self) {
    self.retired.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    kill_pid(self.pid, libc::SIGTERM);
    // 非 Unix 没有信号，也没有保留 Child 句柄：Windows 要终止会话得改成
    // 持有 ConPTY 句柄／Job Object，属于「第一阶段不支持」的范围（协议 §9.5.3）。
    // 关掉 master，客户端看到的会话状态不再变化；读取线程若仍阻塞在
    // 孙进程持有的 slave 上，会随进程退出一起结束。
    *lock(&self.master) = None;
    *lock(&self.writer) = None;
  }

  /// 维护线程调用：静默超过 100ms 才 checkpoint，高频输出期间不 checkpoint。
  pub fn refresh_snapshot(&self, now: Instant) {
    let mut inner = match self.state.try_lock() {
      Ok(guard) => guard,
      Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
      // 读取线程正在写入：这一轮跳过，下一轮（20ms 后）再说。
      Err(TryLockError::WouldBlock) => return,
    };
    if !inner.snapshot_dirty {
      return;
    }
    let quiet = inner
      .last_write_at
      .map(|last| now.saturating_duration_since(last) >= Duration::from_millis(SNAPSHOT_QUIET_MS))
      .unwrap_or(true);
    if quiet {
      inner.checkpoint();
    }
  }

  fn read_loop(self: Arc<Self>, mut reader: Box<dyn Read + Send>) {
    let mut decoder = IncrementalUtf8Decoder::new();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
      match reader.read(&mut buffer) {
        Ok(0) => break,
        Ok(read) => {
          let text = decoder.push(&buffer[..read]);
          if !text.is_empty() {
            self.append(&text);
          }
        }
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
        // EIO / EOF：子进程已退出，读取线程自然收尾。
        Err(_) => break,
      }
    }
    // 协议要求：会话结束时丢弃不完整尾部，绝不产出 U+FFFD。
    decoder.discard_tail();
    let (done, condvar) = &self.reader_done;
    *lock(done) = true;
    condvar.notify_all();
  }

  fn exit_loop(self: Arc<Self>, child: Box<dyn Child + Send + Sync>) {
    let exit = wait_for_child(child);
    {
      let mut inner = lock(&self.state);
      inner.status = SessionStatus::Exited;
      inner.exit_code = exit.exit_code;
    }
    // 先让读取线程把残余输出吐完，再广播 exit，保住「data 先于 exit」的顺序。
    self.wait_reader(READER_DRAIN_TIMEOUT);
    {
      let mut inner = lock(&self.state);
      // 退出后屏幕不会再变：把 pending 落进基线，让 attach 直接拿到终态屏幕。
      if inner.snapshot_dirty {
        inner.checkpoint();
      }
    }
    // 已 forget 的会话不再广播：Server 侧已经把它当不存在了。
    if self.retired.load(Ordering::SeqCst) {
      return;
    }
    self.sink.publish(Event::Exit {
      session_id: self.session_id.clone(),
      incarnation_id: self.incarnation_id.clone(),
      exit_code: exit.exit_code,
      signal: exit.signal,
    });
  }

  fn wait_reader(&self, timeout: Duration) {
    let (done, condvar) = &self.reader_done;
    let mut finished = lock(done);
    if *finished {
      return;
    }
    let deadline = Instant::now() + timeout;
    while !*finished {
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return;
      }
      let (guard, _) = condvar
        .wait_timeout(finished, remaining)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      finished = guard;
    }
  }

  /// PTY 字节 → seq → journal / 屏幕模型 / 事件。
  fn append(&self, data: &str) {
    if self.retired.load(Ordering::SeqCst) {
      return;
    }
    let seq = {
      let mut inner = lock(&self.state);
      inner.seq += 1;
      let seq = inner.seq;
      // chunks 与 data 事件保持原始 PTY 数据（与 legacy 一致，标记剥离只作用于
      // output / 屏幕模型）。
      inner.chunks.push(seq, data);
      let visible = match inner.marker.as_mut() {
        Some(stripper) => stripper.consume(data),
        None => data.to_string(),
      };
      if !visible.is_empty() {
        inner.output.push(&visible);
        inner.modes.scan(visible.as_bytes());
        inner.parser.process(visible.as_bytes());
        inner.push_pending(&visible);
        inner.last_write_at = Some(Instant::now());
        inner.snapshot_dirty = true;
        if inner.pending_over_budget() {
          // pending 太大：不等静默窗口，立刻重建基线（legacy 的同名阈值）。
          inner.checkpoint();
        }
      }
      seq
    };
    self.sink.publish(Event::Data {
      session_id: self.session_id.clone(),
      incarnation_id: self.incarnation_id.clone(),
      data: data.to_string(),
      seq,
    });
  }
}

/// 投递信号：只有确认子进程是它自己进程组的组长时才用 `-pid`（forkpty 路径下
/// 子进程已经 `setsid`），这样既能覆盖它拉起的子进程，又不会误伤 Render 自己。
#[cfg(unix)]
fn kill_pid(pid: u32, signo: i32) {
  if pid == 0 {
    return;
  }
  let pid = pid as libc::pid_t;
  let process_group = unsafe { libc::getpgid(pid) };
  let target = if process_group == pid { -pid } else { pid };
  unsafe {
    libc::kill(target, signo);
  }
}

/// spawn 失败时的兜底回收用 SIGKILL：此刻还不知道子进程初始化到哪一步，
/// SIGHUP/SIGTERM 都可能被忽略而把进程留在机器上。
#[cfg(unix)]
fn force_kill(pid: u32) {
  kill_pid(pid, libc::SIGKILL);
}

/// 非 Unix 没有信号，也没有可用的 pid 句柄（Windows 用 ConPTY 句柄与 Job Object）：
/// 这一阶段的兜底回收交给 portable-pty 的句柄在 drop 时处理（协议 §9.5.3）。
#[cfg(not(unix))]
fn force_kill(_pid: u32) {}

/// 直接 `waitpid` 拿原始状态：portable-pty 的 `ExitStatus::signal()` 返回的是
/// `strsignal` 描述（"Killed: 9"），换不回信号编号，而协议要的是编号。
#[cfg(unix)]
fn wait_for_child(mut child: Box<dyn Child + Send + Sync>) -> ChildExit {
  let pid = child.process_id().unwrap_or(0);
  if pid == 0 {
    let _ = child.try_wait();
    return ChildExit {
      exit_code: None,
      signal: None,
    };
  }
  loop {
    let mut status: libc::c_int = 0;
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
    if result == pid as libc::pid_t {
      return decode_wait_status(status);
    }
    if result < 0 {
      let error = std::io::Error::last_os_error();
      if error.kind() == std::io::ErrorKind::Interrupted {
        continue;
      }
      // ECHILD：状态已经被别处收割，退回 generic 状态而不是谎报退出码。
      return match child.try_wait().ok().flatten() {
        Some(status) if status.signal().is_none() => ChildExit {
          exit_code: Some(status.exit_code() as i32),
          signal: None,
        },
        _ => ChildExit {
          exit_code: None,
          signal: None,
        },
      };
    }
  }
}

/// POSIX wait 状态解码（`0x7f` 低位的规范编码，Linux 与 macOS 一致）。
#[cfg(unix)]
fn decode_wait_status(status: libc::c_int) -> ChildExit {
  let raw = status & 0xffff;
  let term_signal = raw & 0x7f;
  if term_signal == 0 {
    return ChildExit {
      exit_code: Some((raw >> 8) & 0xff),
      signal: None,
    };
  }
  if term_signal != 0x7f {
    return ChildExit {
      exit_code: None,
      signal: Some(term_signal),
    };
  }
  // 0x7f：stopped / continued，正常不会在 waitpid(0) 且未设 WUNTRACED 时出现。
  ChildExit {
    exit_code: None,
    signal: None,
  }
}

/// 非 Unix（Windows / ConPTY）：等 portable-pty 的 `Child::wait` 返回。
/// 没有 signal 语义，只能报退出码。
#[cfg(not(unix))]
fn wait_for_child(mut child: Box<dyn Child + Send + Sync>) -> ChildExit {
  match child.wait() {
    Ok(status) => ChildExit {
      exit_code: Some(status.exit_code() as i32),
      signal: None,
    },
    // 拿不到状态就照实报告「未知」，不编造退出码。
    Err(_) => ChildExit {
      exit_code: None,
      signal: None,
    },
  }
}

/// 每次新建 PTY 生成一次的实例标识（attach 不改变）。
fn new_incarnation_id() -> String {
  use std::sync::atomic::AtomicU64;
  static COUNTER: AtomicU64 = AtomicU64::new(1);
  // 与 legacy 的 `u-<pid>-<n>` 同形：跨进程可读、进程内单调。
  let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or(0);
  format!("u-{}-{nanos}-{counter}", std::process::id())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[cfg(unix)]
  fn wait_status_decoding() {
    assert_eq!(decode_wait_status(0).exit_code, Some(0));
    assert_eq!(decode_wait_status(0).signal, None);
    assert_eq!(decode_wait_status(3 << 8).exit_code, Some(3));
    assert_eq!(decode_wait_status(libc::SIGKILL).signal, Some(libc::SIGKILL));
    assert_eq!(decode_wait_status(libc::SIGKILL).exit_code, None);
    assert_eq!(decode_wait_status(0x7f).exit_code, None);
    assert_eq!(decode_wait_status(0x7f).signal, None);
  }

  #[test]
  fn incarnation_ids_are_unique() {
    let first = new_incarnation_id();
    let second = new_incarnation_id();
    assert_ne!(first, second);
    assert!(first.starts_with("u-"));
  }

  #[test]
  #[cfg(unix)]
  fn signal_delivery_rejects_unknown_names() {
    // 只验证错误分支：真正的投递在 PTY 集成测试里覆盖。
    assert!(signal::number("SIGBREAKFAST").is_none());
  }
}
