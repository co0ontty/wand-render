//! `wand-render`：常驻 Render 守护进程。
//!
//! 生命周期（`docs/render-protocol.md` §6 / §9.3）：
//! - socket / token / pid / meta 全部按 config 路径派生，与 legacy `terminald`
//!   命名空间**刻意不同**，升级期两套并存但不互相领养；
//! - 忽略 SIGHUP（脱离父进程后终端关闭不影响 Render）；
//! - SIGTERM/SIGINT 等价 `shutdown { mode: "drain" }`：停止接受新会话，但**保留**
//!   运行中的 PTY、进程继续存活，最后一个会话退出后自行结束；第二次信号 == `now`。

mod args;
#[cfg(unix)]
mod server;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::Path;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(unix)]
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use anyhow::{anyhow, Result};
#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use wand_render::security::{self, ExistingSocket};
#[cfg(unix)]
use wand_render::{render_paths, RenderPaths, RenderRegistry};
use wand_render_protocol::RENDER_PROTOCOL_VERSION;
#[cfg(unix)]
use wand_render_protocol::ShutdownMode;

#[cfg(unix)]
use crate::server::{generate_token, ClientHub, HubSink, RenderServer};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 退出前留给响应/事件冲刷的时间。
#[cfg(unix)]
const EXIT_FLUSH_DELAY: Duration = Duration::from_millis(150);

/// drain 期间「最后一个会话是否已退出」的检查间隔。
#[cfg(unix)]
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 信号处理器只能做 async-signal-safe 的事，所以只往这个 fd 写一个字节。
#[cfg(unix)]
static SHUTDOWN_SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

fn main() {
  if let Err(error) = run() {
    eprintln!("wand-render: {error:#}");
    std::process::exit(1);
  }
}

fn run() -> Result<()> {
  match args::parse(std::env::args().skip(1)).map_err(|message| anyhow!(message))? {
    args::Command::Version => {
      // 带上协议版本：打包脚本据此断言「二进制自报的协议版本」与源码常量一致。
      // 只报 crate 版本时，协议常量解析错误（例如把 `u32` 里的 32 当成版本号）无法被发现。
      println!("wand-render {VERSION} (protocol {RENDER_PROTOCOL_VERSION})");
      Ok(())
    }
    args::Command::Help => {
      print!("{}", args::HELP);
      Ok(())
    }
    args::Command::Run { config_path } => serve(&config_path),
  }
}

/// 非 Unix（Windows）：第一阶段不支持，入口就给出明确说明，
/// 而不是编译失败或运行到一半神秘崩溃（协议 §9.5.1）。
#[cfg(not(unix))]
fn serve(config_path: &Path) -> Result<()> {
  let _ = config_path;
  Err(anyhow!(
    "{}",
    wand_render::paths::WINDOWS_UNSUPPORTED_MESSAGE
  ))
}

#[cfg(unix)]
fn serve(config_path: &Path) -> Result<()> {
  let paths = render_paths(config_path);

  // 1. 忽略 SIGHUP：Server 重启、终端关闭都不能带走 Render 与其 PTY。
  ignore_sighup();
  // 2. socket / token 只给本用户（socket 的 0600 在 bind 之后显式再设一次）。
  unsafe {
    libc::umask(0o077);
  }

  std::fs::create_dir_all(paths.config_dir())
    .with_context(|| format!("failed to create {}", paths.config_dir().display()))?;

  // 3. 单实例：socket 还能连上就绝不抢；路径不属于本用户时拒绝启动（协议 §9.5.2）。
  prepare_socket_path(&paths)?;

  let listener = UnixListener::bind(&paths.socket_path)
    .with_context(|| format!("failed to bind {}", paths.socket_path.display()))?;
  std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600))
    .context("failed to chmod the render socket")?;

  // 4. bind 成功之后才发布凭据/元数据：并发的第二个进程不会覆盖活着的 Render。
  let token = generate_token().context("failed to generate the render token")?;
  write_private(&paths.token_path, token.as_bytes(), 0o600)?;
  write_private(
    &paths.pid_path,
    format!("{}\n", std::process::id()).as_bytes(),
    0o644,
  )?;
  let meta = serde_json::json!({
    "version": VERSION,
    "protocolVersion": RENDER_PROTOCOL_VERSION,
    "pid": std::process::id(),
    "startedAt": wand_render::time::iso8601_now(),
    "sessions": 0,
  });
  write_private(&paths.meta_path, format!("{meta}\n").as_bytes(), 0o644)?;

  let hub = ClientHub::new();
  let shutdown_channel = install_shutdown_signals()?;
  let registry = RenderRegistry::new(VERSION, HubSink::new(Arc::clone(&hub)));
  let server = RenderServer::new(
    Arc::clone(&hub),
    Arc::clone(&registry),
    token.clone(),
    shutdown_channel.0.clone(),
  );

  let accept_server = Arc::clone(&server);
  std::thread::Builder::new()
    .name("wand-render-accept".into())
    .spawn(move || accept_server.serve(listener))
    .context("failed to start the accept loop")?;

  eprintln!(
    "wand-render {VERSION} listening on {} (pid {})",
    paths.socket_path.display(),
    std::process::id()
  );

  let mode = shutdown_loop(&registry, &shutdown_channel.1);
  // 给 shutdown 响应与最后一个事件一点出场时间，然后收摊。
  std::thread::sleep(EXIT_FLUSH_DELAY);
  registry.stop_maintenance();
  cleanup(&paths, &token, std::process::id());
  eprintln!("wand-render stopped ({mode:?})");
  Ok(())
}

/// 关闭决策循环（协议 §9.3）。
///
/// 返回值是最终模式：`Drain` = 最后一个会话已退出、自然收摊；`Now` = 杀掉所有 PTY
/// 立刻退出。要点：
///
/// - `drain` **不能结束进程**：主线程一旦返回，PTY master fd 就被关掉，子进程收
///   SIGHUP 全死，「保留运行中会话」变成空话。所以这里只置位 shutting_down，
///   把「停止接受新会话」交给 registry，然后等运行中的会话自己退完。
/// - 已经是 drain 状态时再来一次请求（信号或 `shutdown{drain}`）才升级为 `now`，
///   这是唯一的强杀出口。
#[cfg(unix)]
fn shutdown_loop(registry: &RenderRegistry, requests: &Receiver<ShutdownMode>) -> ShutdownMode {
  loop {
    match requests.recv_timeout(DRAIN_POLL_INTERVAL) {
      Ok(ShutdownMode::Drain) => {
        if registry.is_shutting_down() {
          eprintln!("wand-render already drained; treating this request as `now` and stopping");
          registry.begin_shutdown(ShutdownMode::Now);
          return ShutdownMode::Now;
        }
        registry.begin_shutdown(ShutdownMode::Drain);
        eprintln!(
          "wand-render drained: no new sessions will be accepted, running sessions keep their PTYs"
        );
        eprintln!("send SIGTERM again (or `shutdown {{\"mode\":\"now\"}}`) to stop it for real");
        // 一个运行中的会话都没有：没有什么可以保留，直接收摊。
        if registry.running_session_count() == 0 {
          return ShutdownMode::Drain;
        }
      }
      Ok(ShutdownMode::Now) => {
        registry.begin_shutdown(ShutdownMode::Now);
        return ShutdownMode::Now;
      }
      Err(RecvTimeoutError::Timeout) => {
        if registry.is_shutting_down() && registry.running_session_count() == 0 {
          eprintln!("wand-render drained: the last session exited; stopping");
          return ShutdownMode::Drain;
        }
      }
      // 信号线程消失（几乎不可能）：不知道还能不能收到请求，按 now 收摊。
      Err(RecvTimeoutError::Disconnected) => {
        registry.begin_shutdown(ShutdownMode::Now);
        return ShutdownMode::Now;
      }
    }
  }
}

/// bind 之前的 socket 归属校验（协议 §9.5.2）。
///
/// socket 落在全局可写的 `/tmp`，任何本机进程都能抢先 `bind` 同名路径，所以：
///
/// - 路径不存在 → 直接 bind；
/// - 是本用户的 0600 socket 且有人在 listen → 已经有 Render，拒绝启动；
/// - 是本用户的 0600 socket 但连不上 → 崩溃留下的陈旧残留，删掉重建；
/// - 其他任何情况（普通文件、符号链接、别人的 socket、权限过宽）→ **拒绝启动**，
///   绝不 unlink 不属于自己的路径。
#[cfg(unix)]
fn prepare_socket_path(paths: &RenderPaths) -> Result<()> {
  let socket_path = paths.socket_path.as_path();
  match security::inspect_socket_path(socket_path) {
    ExistingSocket::Absent => {
      if let Some(pid) = live_owner_pid(&paths.pid_path) {
        return Err(anyhow!("Render owner is alive (pid {pid}); waiting for socket recovery"));
      }
      Ok(())
    },
    ExistingSocket::Owned => {
      // pid 文件是 daemon 自己的权威存活记录：它先写 pid 再 bind，所以
      // 「pid 活着」比「connect 能不能连上」更可靠（后者会与 close 竞争）。
      if let Some(pid) = live_owner_pid(&paths.pid_path) {
        return Err(anyhow!(
          "another Render is already running (pid {pid}) on {}",
          socket_path.display()
        ));
      }
      if socket_is_live(socket_path) {
        return Err(anyhow!(
          "another Render is already listening on {}",
          socket_path.display()
        ));
      }
      std::fs::remove_file(socket_path).with_context(|| {
        format!(
          "failed to remove the stale socket {}",
          socket_path.display()
        )
      })?;
      Ok(())
    }
    ExistingSocket::Foreign(reason) => Err(anyhow!(
      "refusing to start: {} already exists but {reason}; wand-render never unlinks a path it does not own \
       (verify it is not another user's socket, then move it aside)",
      socket_path.display()
    )),
  }
}

/// 安装 SIGTERM/SIGINT 优雅退出（SIGHUP 已在 [`ignore_sighup`] 里忽略），
/// 返回 (发送端, 接收端)。
///
/// SIGTERM 语义：每次信号都请求 drain（不杀运行中的 PTY，进程继续服务 attach）；
/// 已经 drain 之后再收到一次请求才升级为 now。见 [`shutdown_loop`]。
#[cfg(unix)]
fn install_shutdown_signals() -> Result<(
  SyncSender<ShutdownMode>,
  std::sync::mpsc::Receiver<ShutdownMode>,
)> {
  let (read_fd, write_fd) = create_pipe()?;
  SHUTDOWN_SIGNAL_FD.store(write_fd, Ordering::SeqCst);

  unsafe {
    let mut handler: libc::sigaction = std::mem::zeroed();
    handler.sa_sigaction = on_shutdown_signal as extern "C" fn(libc::c_int) as usize;
    handler.sa_flags = 0;
    libc::sigaction(libc::SIGTERM, &handler, std::ptr::null_mut());
    libc::sigaction(libc::SIGINT, &handler, std::ptr::null_mut());
  }

  let (sender, receiver) = sync_channel(4);
  // 信号只写一个字节，每次都当作 drain 交给主循环；“已 drain ⇒ 升级为 now”的
  // 逃逸逻辑在 [`shutdown_loop`] 里，信号线程不需要自己计数。
  let signal_sender = sender.clone();
  std::thread::Builder::new()
    .name("wand-render-signals".into())
    .spawn(move || {
      let mut byte = [0u8; 1];
      loop {
        let read = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast(), 1) };
        if read == 1 {
          if signal_sender.send(ShutdownMode::Drain).is_err() {
            return;
          }
          continue;
        }
        if read < 0 {
          let error = std::io::Error::last_os_error();
          if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
          }
        }
        return;
      }
    })
    .context("failed to start the signal watcher")?;

  Ok((sender, receiver))
}

#[cfg(unix)]
extern "C" fn on_shutdown_signal(_signal: libc::c_int) {
  let fd = SHUTDOWN_SIGNAL_FD.load(Ordering::SeqCst);
  if fd >= 0 {
    let byte = [1u8];
    // write 是 async-signal-safe 的；失败说明管道满了，退出语义已经确定。
    unsafe {
      libc::write(fd, byte.as_ptr().cast(), 1);
    }
  }
}

#[cfg(unix)]
fn ignore_sighup() {
  unsafe {
    let mut ignore: libc::sigaction = std::mem::zeroed();
    ignore.sa_sigaction = libc::SIG_IGN;
    libc::sigaction(libc::SIGHUP, &ignore, std::ptr::null_mut());
  }
}

#[cfg(unix)]
fn create_pipe() -> Result<(libc::c_int, libc::c_int)> {
  let mut fds = [0 as libc::c_int; 2];
  let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if result != 0 {
    return Err(anyhow!(
      "failed to create a signal pipe: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok((fds[0], fds[1]))
}

/// 单次 connect 探测：能连上说明有进程在 listen，立即释放，绝不干扰它。
#[cfg(unix)]
fn probe_socket_once(socket_path: &Path) -> bool {
  match std::os::unix::net::UnixStream::connect(socket_path) {
    Ok(stream) => {
      let _ = stream.shutdown(std::net::Shutdown::Both);
      true
    }
    Err(_) => false,
  }
}

/// socket 上还有活的 Render 吗？（陈旧 socket 文件会 connect 失败。）
///
/// 连接探活，**必须两次都成功**才判定为「有活的 Render」。
/// 单次探测在这里不够：刚刚退出的 daemon 与我们的 connect 存在竞态 —— CI 上就真的遇到过
/// 「监听器已 drop、文件还在，connect 却成功一次」，于是崩溃残留被误判成活着，
/// 结果是新 daemon 拒绝启动、Server 回退 legacy。等一小会儿再探一次可以区分
/// 「真的有进程在 listen」与「正在消失的残留」。
#[cfg(unix)]
fn socket_is_live(socket_path: &Path) -> bool {
  if !socket_path.exists() {
    return false;
  }
  if !probe_socket_once(socket_path) {
    return false;
  }
  std::thread::sleep(std::time::Duration::from_millis(50));
  socket_path.exists() && probe_socket_once(socket_path)
}

/// 读取 pid 文件并确认进程还活着。pid 复用由「socket 路径属于本用户且是 0600」共同约束。
#[cfg(unix)]
fn live_owner_pid(pid_path: &Path) -> Option<u32> {
  let raw = std::fs::read_to_string(pid_path).ok()?;
  let pid: i32 = raw.trim().parse().ok()?;
  if pid <= 0 {
    return None;
  }
  // 只关心「存在且可发信号」，不发信号本身。
  if unsafe { libc::kill(pid, 0) } == 0 {
    return Some(pid as u32);
  }
  match std::io::Error::last_os_error().raw_os_error() {
    // EPERM：进程存在但不是我们的（不会出现在本用户路径上，保守视为活着）。
    Some(libc::EPERM) => Some(pid as u32),
    _ => None,
  }
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
  let mut file = std::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .open(path)
    .with_context(|| format!("failed to open {}", path.display()))?;
  // 先设权限再写内容：读者看到文件时权限已经是最终值。
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    .with_context(|| format!("failed to chmod {}", path.display()))?;
  file
    .write_all(contents)
    .with_context(|| format!("failed to write {}", path.display()))?;
  file.flush().ok();
  Ok(())
}

/// 收摊时只删**自己拥有**的文件。
///
/// 升级期/僵尸期同一个 config 可能出现两个 daemon：老进程若无条件 unlink，会把新进程的
/// token/pid/meta 一并删掉，新 daemon 随即变成「活着但凭据没了」，所有终端断联。
/// socket 仍然照删 —— 真的被占时 bind 会 EADDRINUSE，而新 daemon 的 prepare_socket_path
/// 会自己处理陈留的 socket 文件。
#[cfg(unix)]
fn cleanup(paths: &RenderPaths, token: &str, pid: u32) {
  let _ = std::fs::remove_file(&paths.socket_path);
  remove_if_owned(&paths.token_path, |raw| raw.trim() == token);
  remove_if_owned(&paths.pid_path, |raw| raw.trim() == pid.to_string());
  remove_if_owned(&paths.meta_path, |raw| {
    serde_json::from_str::<serde_json::Value>(raw)
      .ok()
      .and_then(|value| value.get("pid").and_then(serde_json::Value::as_u64))
      == Some(u64::from(pid))
  });
}

/// 内容还是本进程写的值才删（读不到或被别人改写都算「不是自己的」）。
#[cfg(unix)]
fn remove_if_owned(path: &Path, owned: impl Fn(&str) -> bool) {
  let Ok(raw) = std::fs::read_to_string(path) else {
    return;
  };
  if owned(&raw) {
    let _ = std::fs::remove_file(path);
  }
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use std::collections::BTreeMap;
  use std::sync::Arc;
  use std::time::{Duration, Instant};

  use wand_render::sink::NullSink;
  use wand_render::RenderError;
  use wand_render_protocol::CreateOrAttachParams;

  #[test]
  fn socket_liveness_probe_handles_missing_paths() {
    assert!(!socket_is_live(Path::new("/tmp/wand-render-does-not-exist.sock")));
  }

  #[test]
  fn write_private_sets_the_requested_mode() {
    let dir = test_dir("write-private");
    let path = dir.join("token");
    write_private(&path, b"secret\n", 0o600).expect("write");
    let mode = std::fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let contents = std::fs::read_to_string(&path).expect("read");
    assert_eq!(contents, "secret\n");
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// §9.5.2：路径已被别人占用（普通文件 / 符号链接 / 权限过宽）时拒绝启动，
  /// 而且**不能**动那个文件。
  #[test]
  fn prepare_socket_path_refuses_foreign_paths_without_removing_them() {
    let dir = test_dir("foreign");
    let paths = test_paths(&dir);
    let path = paths.socket_path.clone();
    std::fs::write(&path, b"someone else's file").expect("write");
    let error = prepare_socket_path(&paths).expect_err("a regular file must be refused");
    assert!(
      error.to_string().contains("not a unix socket"),
      "unhelpful error: {error}"
    );
    assert!(path.exists(), "the daemon must not unlink what it does not own");

    let target = dir.join("target.sock");
    let listener = std::os::unix::net::UnixListener::bind(&target).expect("bind");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let link = dir.join("link.sock");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let link_paths = RenderPaths {
      socket_path: link,
      token_path: paths.token_path.clone(),
      pid_path: paths.pid_path.clone(),
      meta_path: paths.meta_path.clone(),
    };
    assert!(prepare_socket_path(&link_paths).is_err());
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
  }

  fn test_paths(dir: &Path) -> RenderPaths {
    RenderPaths {
      socket_path: dir.join("wand-render.sock"),
      token_path: dir.join(".render.token"),
      pid_path: dir.join(".render.pid"),
      meta_path: dir.join(".render.json"),
    }
  }

  /// 收摊只删自己的东西：别人（接替我们的新 daemon）写进去的 token/pid/meta 必须留着。
  #[test]
  fn cleanup_only_removes_files_this_process_owns() {
    let dir = test_dir("cleanup-ownership");
    let paths = test_paths(&dir);
    std::fs::write(&paths.token_path, "someone-elses-token\n").expect("write token");
    std::fs::write(&paths.pid_path, "999999\n").expect("write pid");
    std::fs::write(&paths.meta_path, "{\"pid\":999999}\n").expect("write meta");
    cleanup(&paths, "our-token", 4242);
    assert!(paths.token_path.exists(), "a successor's token must survive our shutdown");
    assert!(paths.pid_path.exists(), "a successor's pid must survive our shutdown");
    assert!(paths.meta_path.exists(), "a successor's meta must survive our shutdown");

    std::fs::write(&paths.token_path, "our-token\n").expect("write token");
    std::fs::write(&paths.pid_path, "4242\n").expect("write pid");
    std::fs::write(&paths.meta_path, "{\"pid\":4242}\n").expect("write meta");
    cleanup(&paths, "our-token", 4242);
    assert!(!paths.token_path.exists(), "our own token must be removed");
    assert!(!paths.pid_path.exists(), "our own pid must be removed");
    assert!(!paths.meta_path.exists(), "our own meta must be removed");
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 活得着的 Render 必须拒绝启动而不是抢 socket。
  #[test]
  fn prepare_socket_path_refuses_when_a_render_is_listening() {
    let dir = test_dir("live");
    let paths = test_paths(&dir);
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket_path).expect("bind");
    std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let error = prepare_socket_path(&paths).expect_err("a live listener must be respected");
    assert!(
      error.to_string().contains("already listening"),
      "unhelpful error: {error}"
    );
    assert!(paths.socket_path.exists());
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 本用户 0600 的陈旧 socket（进程已死）可以回收重建。
  #[test]
  fn prepare_socket_path_reclaims_a_stale_own_socket() {
    let dir = test_dir("stale");
    let paths = test_paths(&dir);
    {
      let listener = std::os::unix::net::UnixListener::bind(&paths.socket_path).expect("bind");
      std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
      drop(listener);
    }
    assert!(
      paths.socket_path.exists(),
      "a dropped listener leaves the socket file behind"
    );
    prepare_socket_path(&paths).expect("a stale own socket must be reclaimed");
    assert!(
      !paths.socket_path.exists(),
      "the stale socket must be removed before bind"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// pid 文件里是一个死 pid：即使 socket 还在文件系统上，也应视为陈旧并回收。
  #[test]
  fn prepare_socket_path_reclaims_when_the_recorded_pid_is_dead() {
    let dir = test_dir("dead-pid");
    let paths = test_paths(&dir);
    {
      let listener = std::os::unix::net::UnixListener::bind(&paths.socket_path).expect("bind");
      std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
      drop(listener);
    }
    // 一个几乎不可能存在的 pid：确认「死 pid」不会阻止回收。
    std::fs::write(&paths.pid_path, "999999\n").expect("write pid");
    assert_eq!(live_owner_pid(&paths.pid_path), None);
    prepare_socket_path(&paths).expect("a dead owner must not block a restart");
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// pid 文件里是活着的 pid（这里用测试进程自己）：必须拒绝启动，
  /// 且错误信息要说清是「有进程在跑」，而不是含糊的「连不上」。
  #[test]
  fn prepare_socket_path_refuses_when_the_recorded_pid_is_alive() {
    let dir = test_dir("live-pid");
    let paths = test_paths(&dir);
    {
      let listener = std::os::unix::net::UnixListener::bind(&paths.socket_path).expect("bind");
      std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
      drop(listener);
    }
    std::fs::write(&paths.pid_path, format!("{}\n", std::process::id())).expect("write pid");
    let error = prepare_socket_path(&paths).expect_err("a live owner must be respected");
    assert!(
      error.to_string().contains("already running"),
      "unhelpful error: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 没有运行中会话时，drain 立刻收摊（没有东西需要保留）。
  #[test]
  fn shutdown_loop_stops_immediately_when_nothing_is_running() {
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    let (sender, receiver) = sync_channel(1);
    sender.send(ShutdownMode::Drain).expect("request");
    assert_eq!(shutdown_loop(&registry, &receiver), ShutdownMode::Drain);
    assert!(registry.is_shutting_down());
    registry.stop_maintenance();
  }

  /// §9.3：drain 后进程必须继续活着、拒绝新会话；最后一个会话退出才收摊。
  #[test]
  fn shutdown_loop_keeps_running_until_the_last_session_exits() {
    let registry = running_registry();
    let (sender, receiver) = sync_channel(2);
    sender.send(ShutdownMode::Drain).expect("request");
    let loop_registry = Arc::clone(&registry);
    let handle = std::thread::spawn(move || shutdown_loop(&loop_registry, &receiver));

    std::thread::sleep(Duration::from_millis(300));
    assert!(
      !handle.is_finished(),
      "drain must not end the daemon while a PTY is still running"
    );
    assert!(registry.is_shutting_down());
    assert_eq!(registry.running_session_count(), 1);
    // 新会话被拒（协议 §9.3）。
    assert!(matches!(
      registry.create_or_attach(&session_params("late")),
      Err(RenderError::Conflict(_))
    ));

    registry.kill("life", Some("SIGKILL")).expect("kill");
    assert!(
      wait_until(Duration::from_secs(5), || handle.is_finished()),
      "the daemon must stop once the last session exited"
    );
    assert_eq!(handle.join().expect("join"), ShutdownMode::Drain);
    registry.stop_maintenance();
  }

  /// §9.3：第二次 drain 请求（信号也是走这条路）升级为 now，PTY 被杀掉。
  #[test]
  fn second_drain_request_escalates_to_now() {
    let registry = running_registry();
    let (sender, receiver) = sync_channel(2);
    sender.send(ShutdownMode::Drain).expect("first request");
    sender.send(ShutdownMode::Drain).expect("second request");
    assert_eq!(shutdown_loop(&registry, &receiver), ShutdownMode::Now);
    assert_eq!(registry.running_session_count(), 0);
    assert!(registry.session_count() == 0);
    registry.stop_maintenance();
  }

  /// `shutdown { mode: "now" }` 直接收摊，不等会话退出。
  #[test]
  fn now_request_stops_immediately() {
    let registry = running_registry();
    let (sender, receiver) = sync_channel(1);
    sender.send(ShutdownMode::Now).expect("request");
    assert_eq!(shutdown_loop(&registry, &receiver), ShutdownMode::Now);
    assert_eq!(registry.running_session_count(), 0);
    registry.stop_maintenance();
  }

  fn test_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
      "wand-renderd-{tag}-{}-{}",
      std::process::id(),
      COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
  }

  fn session_params(session_id: &str) -> CreateOrAttachParams {
    CreateOrAttachParams {
      session_id: session_id.to_string(),
      file: "/bin/sh".to_string(),
      args: vec!["-c".to_string(), "sleep 30".to_string()],
      cwd: std::env::temp_dir().to_string_lossy().to_string(),
      env: BTreeMap::new(),
      name: "xterm-256color".to_string(),
      cols: 80,
      rows: 24,
      launch_marker_token: None,
      after_seq: 0,
    }
  }

  /// 一个持有运行中 PTY（`sleep 30`）的 registry。
  fn running_registry() -> Arc<RenderRegistry> {
    let registry = RenderRegistry::new("test", Arc::new(NullSink));
    registry
      .create_or_attach(&session_params("life"))
      .expect("create");
    assert_eq!(registry.running_session_count(), 1);
    registry
  }

  fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut condition: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
      if condition() {
        return true;
      }
      if Instant::now() >= deadline {
        return false;
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }
}
