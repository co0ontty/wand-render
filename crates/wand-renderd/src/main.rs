//! `wand-render`：常驻 Render 守护进程。
//!
//! 生命周期（`docs/render-protocol.md` §6）：
//! - socket / token / pid / meta 全部按 config 路径派生，与 legacy `terminald`
//!   命名空间**刻意不同**，升级期两套并存但不互相领养；
//! - 忽略 SIGHUP（脱离父进程后终端关闭不影响 Render）；
//! - SIGTERM 优雅退出（等价 `shutdown { mode: "drain" }`：不杀运行中的 PTY）。

mod args;
mod server;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use wand_render::{render_paths, RenderPaths, RenderRegistry};
use wand_render_protocol::{ShutdownMode, RENDER_PROTOCOL_VERSION};

use crate::server::{generate_token, ClientHub, HubSink, RenderServer};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 退出前留给响应/事件冲刷的时间。
const EXIT_FLUSH_DELAY: Duration = Duration::from_millis(150);

/// 信号处理器只能做 async-signal-safe 的事，所以只往这个 fd 写一个字节。
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
      println!("wand-render {VERSION}");
      return Ok(());
    }
    args::Command::Help => {
      print!("{}", args::HELP);
      return Ok(());
    }
    args::Command::Run { config_path } => serve(&config_path),
  }
}

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

  // 3. 单实例：socket 还能连上就绝不抢。
  if socket_is_live(&paths.socket_path) {
    return Err(anyhow!(
      "another Render is already listening on {}",
      paths.socket_path.display()
    ));
  }
  // 4. 陈旧 socket 残留会让 bind 失败，先清掉。
  if paths.socket_path.exists() {
    std::fs::remove_file(&paths.socket_path)
      .with_context(|| format!("failed to remove stale {}", paths.socket_path.display()))?;
  }

  let listener = UnixListener::bind(&paths.socket_path)
    .with_context(|| format!("failed to bind {}", paths.socket_path.display()))?;
  std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600))
    .context("failed to chmod the render socket")?;

  // 5. bind 成功之后才发布凭据/元数据：并发的第二个进程不会覆盖活着的 Render。
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
  write_private(
    &paths.meta_path,
    format!("{meta}\n").as_bytes(),
    0o644,
  )?;

  let hub = ClientHub::new();
  let shutdown_channel = install_shutdown_signals()?;
  let registry = RenderRegistry::new(VERSION, HubSink::new(Arc::clone(&hub)));
  let server = RenderServer::new(
    Arc::clone(&hub),
    Arc::clone(&registry),
    token,
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

  // `drain` 的协议定义是「停止接受新会话，已退出会话释放，**运行中会话保留**」
  // （docs/render-protocol.md §6），所以它必须让进程继续活着 —— 一旦主线程返回，
  // PTY master fd 关闭，所有子进程收 SIGHUP 死掉，「保留运行中会话」就成了空话。
  //
  // 退出规则：**已经处于 drain 状态时再收到一次 drain 请求（SIGTERM/SIGINT 或
  // `shutdown {mode:"drain"}` 都算）就升级为 now**，杀掉 PTY 并退出。
  // 于是“连按两次 Ctrl-C / kill 两次”就能真正停掉一个已 drain 的 Render。
  let mode = loop {
    match shutdown_channel.1.recv() {
      Ok(ShutdownMode::Drain) => {
        if registry.is_shutting_down() {
          eprintln!("wand-render already drained; treating this request as `now` and stopping");
          registry.begin_shutdown(ShutdownMode::Now);
          break ShutdownMode::Now;
        }
        registry.begin_shutdown(ShutdownMode::Drain);
        eprintln!(
          "wand-render drained: no new sessions will be accepted, running sessions keep their PTYs;"
        );
        eprintln!("send SIGTERM again (or `shutdown {{mode:\"now\"}}`) to stop it for real");
        continue;
      }
      Ok(ShutdownMode::Now) | Err(_) => {
        registry.begin_shutdown(ShutdownMode::Now);
        break ShutdownMode::Now;
      }
    }
  };
  // 给 shutdown 响应与最后一个事件一点出场时间，然后收摊。
  std::thread::sleep(EXIT_FLUSH_DELAY);
  registry.stop_maintenance();
  cleanup(&paths);
  eprintln!("wand-render stopped ({mode:?})");
  Ok(())
}

/// 安装 SIGTERM/SIGINT 优雅退出（SIGHUP 已在 [`ignore_sighup`] 里忽略），
/// 返回 (发送端, 接收端)。
///
/// SIGTERM 语义：每次信号都请求 drain（不杀运行中的 PTY，进程继续服务 attach）；
/// 已经 drain 之后再收到一次请求才升级为 now。见 [`serve`] 里的主循环。
fn install_shutdown_signals() -> Result<(SyncSender<ShutdownMode>, std::sync::mpsc::Receiver<ShutdownMode>)> {
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
  // 信号只写一个字节，每次都当作 drain 交给主线程；“已 drain ⇒ 升级为 now”的
  // 逃逸逻辑在主循环里（见 [`serve`]），信号线程不需要自己计数。
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

fn ignore_sighup() {
  unsafe {
    let mut ignore: libc::sigaction = std::mem::zeroed();
    ignore.sa_sigaction = libc::SIG_IGN;
    libc::sigaction(libc::SIGHUP, &ignore, std::ptr::null_mut());
  }
}

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

/// socket 上还有活的 Render 吗？（陈旧 socket 文件会 connect 失败。）
fn socket_is_live(socket_path: &Path) -> bool {
  if !socket_path.exists() {
    return false;
  }
  match std::os::unix::net::UnixStream::connect(socket_path) {
    Ok(stream) => {
      // 连上就说明有进程在 listen：立即释放，绝不干扰它。
      let _ = stream.shutdown(std::net::Shutdown::Both);
      true
    }
    Err(_) => false,
  }
}

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

fn cleanup(paths: &RenderPaths) {
  for path in [
    &paths.socket_path,
    &paths.token_path,
    &paths.pid_path,
    &paths.meta_path,
  ] {
    let _ = std::fs::remove_file(path);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn socket_liveness_probe_handles_missing_paths() {
    assert!(!socket_is_live(Path::new("/tmp/wand-render-does-not-exist.sock")));
  }

  #[test]
  fn write_private_sets_the_requested_mode() {
    let dir = std::env::temp_dir().join(format!("wand-render-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("token");
    write_private(&path, b"secret\n", 0o600).expect("write");
    let mode = std::fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let contents = std::fs::read_to_string(&path).expect("read");
    assert_eq!(contents, "secret\n");
    let _ = std::fs::remove_dir_all(&dir);
  }
}
