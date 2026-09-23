//! 连接方身份与 socket 归属校验（协议 §9.5.2）。
//!
//! socket 派生在 `/tmp`（macOS 的 sun_path 上限约 104 字节），而 `/tmp` 是全局可写
//! 目录：任何本机进程都能抢先 `bind` 同名路径，把 token 骗到手（token 由 Server 在
//! 首个 `hello` 时通过这条连接发出）。所以两侧都必须校验身份：
//!
//! - 服务端：accept 之后、**读取 token 之前**，确认对端 uid 与自身一致；
//! - 客户端：连接之后、发送 token 之前，确认目标是本用户拥有的 0600 socket
//!   （Node 侧实现，见 `src/render-daemon-client.ts` 的 `assertSocketOwnership`）；
//! - 服务端 bind 之前：路径已存在但不是「本用户的 0600 socket」时**拒绝启动**，
//!   绝不 unlink 别人的文件。
//!
//! 只用 `libc` 直接调系统接口，不引入新依赖。

use std::path::Path;

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

/// 本进程的 uid。
pub fn current_uid() -> u32 {
  unsafe { libc::getuid() }
}

/// 对端 uid。
///
/// Linux 用 `SO_PEERCRED`，其他 Unix（macOS / *BSD）用 `getpeereid()`：
/// 两者都由内核填写，调用方无法伪造。
pub fn peer_uid(stream: &UnixStream) -> Result<u32, String> {
  peer_uid_of(stream.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn peer_uid_of(fd: std::os::unix::io::RawFd) -> Result<u32, String> {
  let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
  let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
  let result = unsafe {
    libc::getsockopt(
      fd,
      libc::SOL_SOCKET,
      libc::SO_PEERCRED,
      (&mut credentials as *mut libc::ucred).cast(),
      &mut length,
    )
  };
  if result != 0 {
    return Err(format!(
      "SO_PEERCRED failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(credentials.uid)
}

#[cfg(any(
  target_os = "macos",
  target_os = "ios",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly",
))]
fn peer_uid_of(fd: std::os::unix::io::RawFd) -> Result<u32, String> {
  let mut uid: libc::uid_t = 0;
  let mut gid: libc::gid_t = 0;
  let result = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
  if result != 0 {
    return Err(format!(
      "getpeereid failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(uid as u32)
}

/// 其他 Unix（例如 Solaris/illumos）没有上述任一个接口：**失败关闭**地拒绝一切连接，
/// 而不是「拿不到身份就放行」。要支持它们需要改用 `getpeerucred` 之类的接口。
#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "ios",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly",
)))]
fn peer_uid_of(_fd: std::os::unix::io::RawFd) -> Result<u32, String> {
  Err("this platform has no getpeereid/SO_PEERCRED equivalent; refusing to read any token".to_string())
}

/// 纯比较逻辑（与系统调用分开，便于单测覆盖「不是同 uid 就拒绝」）。
pub fn check_peer_uid(peer: u32, expected: u32) -> Result<(), String> {
  if peer == expected {
    Ok(())
  } else {
    Err(format!(
      "peer uid {peer} is not this process's uid {expected}"
    ))
  }
}

/// accept 之后的第一道闸：对端必须是同一个用户。
///
/// 取不到凭据时**失败关闭**（fail closed）：连不上身份就不该继续读 token。
pub fn ensure_peer_uid(stream: &UnixStream, expected: u32) -> Result<(), String> {
  let peer = peer_uid(stream)?;
  check_peer_uid(peer, expected)
}

/// 已存在的 socket 路径分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingSocket {
  /// 路径不存在，可以直接 bind。
  Absent,
  /// 是本用户的 0600 socket：可能是活着的实例，也可能是崩溃留下的陈旧残留，
  /// 由调用方再判断 liveness 后决定「拒绝启动」还是「删掉重建」。
  Owned,
  /// 存在但**不是**本用户的 0600 socket：必须拒绝启动，绝不能 unlink。
  Foreign(String),
}

/// 校验 socket 路径的归属（协议 §9.5.2）。
///
/// 用 `lstat`（`symlink_metadata`）而不是 `stat`：路径上的符号链接要当成
/// 「不是我们的 socket」，而不是 follow 到底下真正被链接的对象。
pub fn inspect_socket_path(path: &Path) -> ExistingSocket {
  let metadata = match std::fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ExistingSocket::Absent,
    Err(error) => return ExistingSocket::Foreign(format!("it cannot be inspected: {error}")),
  };
  let file_type = metadata.file_type();
  if file_type.is_symlink() {
    return ExistingSocket::Foreign("it is a symbolic link, not a socket".to_string());
  }
  if !file_type.is_socket() {
    return ExistingSocket::Foreign("it is not a unix socket".to_string());
  }
  let uid = metadata.uid();
  let own = current_uid();
  if uid != own {
    return ExistingSocket::Foreign(format!("it is owned by uid {uid}, not by this user ({own})"));
  }
  let mode = metadata.permissions().mode() & 0o777;
  if mode != 0o600 {
    return ExistingSocket::Foreign(format!("its mode is {mode:o}, expected 600"));
  }
  ExistingSocket::Owned
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::net::UnixListener;
  use std::path::PathBuf;
  use std::sync::atomic::{AtomicU64, Ordering};

  fn unique_temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
      "wand-render-security-{tag}-{}-{}",
      std::process::id(),
      COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
  }

  #[test]
  fn absent_path_is_bindable() {
    let dir = unique_temp_dir("absent");
    assert_eq!(
      inspect_socket_path(&dir.join("nope.sock")),
      ExistingSocket::Absent
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 普通文件占位（抢注的典型形态）必须被拒绝，且不能被误删。
  #[test]
  fn regular_file_is_foreign() {
    let dir = unique_temp_dir("regular");
    let path = dir.join("wand-render.sock");
    std::fs::write(&path, b"not a socket").expect("write");
    match inspect_socket_path(&path) {
      ExistingSocket::Foreign(reason) => assert!(
        reason.contains("not a unix socket"),
        "unexpected reason: {reason}"
      ),
      other => panic!("expected Foreign, got {other:?}"),
    }
    assert!(path.exists(), "inspection must never remove anything");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn symlink_to_a_socket_is_foreign() {
    let dir = unique_temp_dir("symlink");
    let target = dir.join("real.sock");
    let _listener = UnixListener::bind(&target).expect("bind");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let link = dir.join("link.sock");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    match inspect_socket_path(&link) {
      ExistingSocket::Foreign(reason) => assert!(
        reason.contains("symbolic link"),
        "unexpected reason: {reason}"
      ),
      other => panic!("expected Foreign, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 权限过宽的 socket（别人能写 = 别人能抢先接管）必须被拒绝。
  #[test]
  fn socket_with_wrong_mode_is_foreign() {
    let dir = unique_temp_dir("mode");
    let path = dir.join("wand-render.sock");
    let _listener = UnixListener::bind(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    match inspect_socket_path(&path) {
      ExistingSocket::Foreign(reason) => assert!(
        reason.contains("600"),
        "unexpected reason: {reason}"
      ),
      other => panic!("expected Foreign, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn own_private_socket_is_owned() {
    let dir = unique_temp_dir("owned");
    let path = dir.join("wand-render.sock");
    let _listener = UnixListener::bind(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(inspect_socket_path(&path), ExistingSocket::Owned);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 同 uid 的 socketpair 必须通过；期望值不同时必须被拒绝（这就是「别的用户」的判据）。
  #[test]
  fn peer_uid_matches_this_process_and_rejects_others() {
    let (left, right) = UnixStream::pair().expect("socketpair");
    assert_eq!(peer_uid(&left).expect("peer uid"), current_uid());
    assert!(ensure_peer_uid(&left, current_uid()).is_ok());
    let error = ensure_peer_uid(&right, current_uid() + 1).expect_err("must reject other uids");
    assert!(error.contains("peer uid"), "unexpected message: {error}");
    assert!(check_peer_uid(1000, 1000).is_ok());
    assert!(check_peer_uid(1000, 1001).is_err());
  }
}
