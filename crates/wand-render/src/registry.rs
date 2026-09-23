//! 会话注册表：sessionId → 会话，外加常驻的快照维护线程。
//!
//! 注册表**不做自动淘汰**：已退出的会话要留着，Server 重启后才能通过 attach
//! 拿到终态（谁该忘掉一个会话由 Server 决定，走 `forget`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wand_render_protocol::{
  CreateOrAttachParams, CreateOrAttachResult, Event, HelloResult, SessionState, ShutdownMode,
  StatsResult, RENDER_PROTOCOL_VERSION,
};

use crate::error::RenderError;
use crate::resources::rss_bytes;
use crate::session::Session;
use crate::sink::EventSink;
use crate::snapshot;
use crate::time::iso8601_now;

/// 快照维护线程的轮询间隔（远小于 100ms 静默窗口，保证及时固化基线）。
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(20);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct RenderRegistry {
  sessions: Mutex<HashMap<String, Arc<Session>>>,
  sink: Arc<dyn EventSink>,
  version: String,
  started_at: Instant,
  started_at_iso: String,
  shutting_down: AtomicBool,
  maintenance_stop: Arc<AtomicBool>,
  maintenance: Mutex<Option<JoinHandle<()>>>,
}

impl RenderRegistry {
  pub fn new(version: impl Into<String>, sink: Arc<dyn EventSink>) -> Arc<Self> {
    let registry = Arc::new(Self {
      sessions: Mutex::new(HashMap::new()),
      sink,
      version: version.into(),
      started_at: Instant::now(),
      started_at_iso: iso8601_now(),
      shutting_down: AtomicBool::new(false),
      maintenance_stop: Arc::new(AtomicBool::new(false)),
      maintenance: Mutex::new(None),
    });
    registry.start_maintenance();
    registry
  }

  /// 启动快照维护线程。用 `Weak` 引用注册表：进程退出时线程不会拖住它。
  fn start_maintenance(self: &Arc<Self>) {
    let stop = Arc::clone(&self.maintenance_stop);
    let weak = Arc::downgrade(self);
    let handle = std::thread::Builder::new()
      .name("wand-render-snapshot".into())
      .spawn(move || {
        while !stop.load(Ordering::SeqCst) {
          std::thread::sleep(MAINTENANCE_INTERVAL);
          let Some(registry) = weak.upgrade() else { break };
          let sessions: Vec<Arc<Session>> = lock(&registry.sessions).values().cloned().collect();
          let now = Instant::now();
          for session in sessions {
            session.refresh_snapshot(now);
          }
        }
      })
      .ok();
    *lock(&self.maintenance) = handle;
  }

  pub fn stop_maintenance(&self) {
    self.maintenance_stop.store(true, Ordering::SeqCst);
    if let Some(handle) = lock(&self.maintenance).take() {
      let _ = handle.join();
    }
  }

  /// 新建或返回既有会话。
  ///
  /// - 不存在 → 新建 PTY，`is_new = true`
  /// - 存在且在跑 → **不重启**，返回现值，`is_new = false`
  /// - 存在但已退出 → 返回退出状态，`is_new = false`
  pub fn create_or_attach(
    &self,
    params: &CreateOrAttachParams,
  ) -> Result<CreateOrAttachResult, RenderError> {
    let existing = lock(&self.sessions).get(&params.session_id).cloned();
    if let Some(existing) = existing {
      return Ok(CreateOrAttachResult {
        state: existing.state(params.after_seq),
        is_new: false,
      });
    }
    if self.shutting_down.load(Ordering::SeqCst) {
      return Err(RenderError::Conflict(
        "render is shutting down; refusing to create new sessions".into(),
      ));
    }
    // 先建 PTY 再登记：spawn 失败时不会留下半截记录。
    let session = Session::spawn(params, Arc::clone(&self.sink))?;
    let mut sessions = lock(&self.sessions);
    if let Some(existing) = sessions.get(&params.session_id).cloned() {
      // 并发 createOrAttach 竞态：丢掉刚建起来的那个，保留已有的。
      drop(sessions);
      session.retire();
      return Ok(CreateOrAttachResult {
        state: existing.state(params.after_seq),
        is_new: false,
      });
    }
    sessions.insert(params.session_id.clone(), Arc::clone(&session));
    drop(sessions);
    Ok(CreateOrAttachResult {
      state: session.state(params.after_seq),
      is_new: true,
    })
  }

  pub fn attach(&self, session_id: &str, after_seq: u64) -> Option<SessionState> {
    let session = lock(&self.sessions).get(session_id).cloned();
    session.map(|session| session.state(after_seq))
  }

  /// 批量读取会话状态（`list` 专用）。
  ///
  /// 每个会话的 `terminalSnapshot` 按协议 §9.1.1 裁剪到 64KiB：`list` 把一个响应里
  /// 内联所有会话，完整回滚快照（5000 行 × 1000 列可达 5MB）会让响应撞上单帧上限。
  /// `output`/`chunks` 仍按 §4 的上限，需要精确重建屏幕的调用方走
  /// `attach`/`create_or_attach`，它们返回完整状态。
  pub fn list_sessions(&self) -> Vec<SessionState> {
    lock(&self.sessions)
      .values()
      .map(|session| {
        let mut state = session.state(0);
        state.terminal_snapshot = state.terminal_snapshot.and_then(|snapshot| {
          snapshot::bound_for_list(snapshot, snapshot::LIST_SNAPSHOT_MAX_BYTES)
        });
        state
      })
      .collect()
  }

  /// 仍在运行的会话数。
  ///
  /// `drain` 的退出条件（协议 §9.3）是「最后一个会话退出」，守护进程的关闭循环
  /// 用它做判定，所以这里不能受已退出但尚未 forget 的会话影响。
  pub fn running_session_count(&self) -> usize {
    // 先克隆句柄再放开注册表锁：会话锁只在注册表锁之外拿（锁嵌套方向保持
    // 「注册表 → 会话」，见 session.rs 的锁约定）。
    let sessions: Vec<Arc<Session>> = lock(&self.sessions).values().cloned().collect();
    sessions
      .iter()
      .filter(|session| session.is_running())
      .count()
  }

  pub fn write(&self, session_id: &str, data: &str) -> Result<(), RenderError> {
    self.session(session_id)?.write(data)
  }

  pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), RenderError> {
    self.session(session_id)?.resize(cols, rows)
  }

  pub fn kill(&self, session_id: &str, signal: Option<&str>) -> Result<(), RenderError> {
    let session = self.session(session_id)?;
    // 与 legacy 一致：已退出的会话不再投递信号。
    if !session.is_running() {
      return Ok(());
    }
    session.kill(signal)
  }

  /// 摘除会话（Server 明确要求忘掉时才调用）。
  pub fn forget(&self, session_id: &str) -> bool {
    let removed = lock(&self.sessions).remove(session_id);
    match removed {
      Some(session) => {
        session.retire();
        self.publish_reconcile();
        true
      }
      None => false,
    }
  }

  pub fn session_count(&self) -> usize {
    lock(&self.sessions).len()
  }

  pub fn hello(&self) -> HelloResult {
    HelloResult {
      version: self.version.clone(),
      protocol_version: RENDER_PROTOCOL_VERSION,
      pid: std::process::id(),
      started_at: self.started_at_iso.clone(),
      sessions: self.session_count(),
    }
  }

  pub fn stats(&self) -> StatsResult {
    let sessions: Vec<Arc<Session>> = lock(&self.sessions).values().cloned().collect();
    StatsResult {
      uptime_ms: self.started_at.elapsed().as_millis() as u64,
      sessions: sessions.len(),
      live_bytes: sessions.iter().map(|session| session.live_bytes()).sum(),
      rss_bytes: rss_bytes(),
    }
  }

  pub fn is_shutting_down(&self) -> bool {
    self.shutting_down.load(Ordering::SeqCst)
  }

  /// `drain`：停止接受新会话、释放已退出会话，**运行中的 PTY 一律不杀**。
  /// `now`：杀掉所有 PTY（只在用户明确要求时使用）。
  pub fn begin_shutdown(&self, mode: ShutdownMode) {
    self.shutting_down.store(true, Ordering::SeqCst);
    match mode {
      ShutdownMode::Drain => {
        // 先克隆句柄再放开注册表锁：`is_running()` 要拿会话锁，不能在持有注册表锁时嵌套。
        let all: Vec<(String, Arc<Session>)> = lock(&self.sessions)
          .iter()
          .map(|(session_id, session)| (session_id.clone(), Arc::clone(session)))
          .collect();
        let stale: Vec<String> = all
          .into_iter()
          .filter(|(_, session)| !session.is_running())
          .map(|(session_id, _)| session_id)
          .collect();
        if !stale.is_empty() {
          let mut sessions = lock(&self.sessions);
          for session_id in stale {
            sessions.remove(&session_id);
          }
        }
      }
      ShutdownMode::Now => {
        let all: Vec<Arc<Session>> = {
          let mut sessions = lock(&self.sessions);
          let all: Vec<Arc<Session>> = sessions.values().cloned().collect();
          sessions.clear();
          all
        };
        for session in all {
          let _ = session.kill(Some("SIGKILL"));
        }
      }
    }
    self.publish_reconcile();
  }

  /// 告知客户端「当前真正存在的会话集合」（例如刚 forget 掉一个会话）。
  pub fn publish_reconcile(&self) {
    let session_ids: Vec<String> = lock(&self.sessions).keys().cloned().collect();
    self.sink.publish(Event::Reconcile { session_ids });
  }

  fn session(&self, session_id: &str) -> Result<Arc<Session>, RenderError> {
    lock(&self.sessions)
      .get(session_id)
      .cloned()
      .ok_or_else(|| RenderError::NotFound(session_id.to_string()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::sink::NullSink;

  #[test]
  fn hello_reports_protocol_version_and_started_at() {
    let registry = RenderRegistry::new("0.1.0-test", Arc::new(NullSink));
    let hello = registry.hello();
    assert_eq!(hello.protocol_version, RENDER_PROTOCOL_VERSION);
    assert_eq!(hello.version, "0.1.0-test");
    assert_eq!(hello.pid, std::process::id());
    assert!(hello.started_at.ends_with('Z'));
    assert_eq!(hello.sessions, 0);
    registry.stop_maintenance();
  }

  #[test]
  fn attach_and_forget_missing_sessions() {
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    assert!(registry.attach("nope", 0).is_none());
    assert!(matches!(
      registry.write("nope", "x"),
      Err(RenderError::NotFound(_))
    ));
    assert!(!registry.forget("nope"));
    assert_eq!(registry.stats().sessions, 0);
    registry.stop_maintenance();
  }

  #[test]
  fn empty_registry_lists_nothing_and_reports_no_running_sessions() {
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    assert!(registry.list_sessions().is_empty());
    assert_eq!(registry.running_session_count(), 0);
    assert!(!registry.is_shutting_down());
    registry.begin_shutdown(ShutdownMode::Drain);
    assert!(registry.is_shutting_down());
    assert_eq!(registry.running_session_count(), 0);
    registry.stop_maintenance();
  }

  /// drain 只置位「不再接受新会话」，把退出决策留给守护进程的关闭循环。
  #[test]
  fn drain_flags_shutdown_without_touching_anything_else() {
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    registry.begin_shutdown(ShutdownMode::Drain);
    assert!(registry.is_shutting_down());
    assert!(matches!(
      registry.create_or_attach(&params("late")),
      Err(RenderError::Conflict(_))
    ));
    registry.stop_maintenance();
  }

  fn params(session_id: &str) -> wand_render_protocol::CreateOrAttachParams {
    wand_render_protocol::CreateOrAttachParams {
      session_id: session_id.to_string(),
      file: "/bin/sh".to_string(),
      args: Vec::new(),
      cwd: std::env::temp_dir().to_string_lossy().to_string(),
      env: std::collections::BTreeMap::new(),
      name: "xterm-256color".to_string(),
      cols: 80,
      rows: 24,
      launch_marker_token: None,
      after_seq: 0,
    }
  }
}
