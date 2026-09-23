//! Keep an IPC listener reachable when a temporary-directory cleaner unlinks it.
//! Accepted streams and daemon-owned processes are independent of the listener.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct RecoveringListener {
  listener: UnixListener,
  path: PathBuf,
  identity: (u64, u64),
  checked: Instant,
  touched: Instant,
  warned: bool,
}

impl RecoveringListener {
  pub fn new(listener: UnixListener, path: PathBuf) -> io::Result<Self> {
    listener.set_nonblocking(true)?;
    let meta = fs::symlink_metadata(&path)?;
    Ok(Self { listener, path, identity: (meta.dev(), meta.ino()),
      checked: Instant::now(), touched: Instant::now(), warned: false })
  }

  pub fn accept(&mut self) -> io::Result<(UnixStream, SocketAddr)> {
    if self.checked.elapsed() >= Duration::from_secs(1) {
      self.checked = Instant::now();
      if let Err(error) = self.maintain() {
        if !self.warned {
          eprintln!("wand-render: socket recovery failed: {error}");
          self.warned = true;
        }
      }
    }
    let (stream, address) = self.listener.accept()?;
    // macOS inherits O_NONBLOCK from the listening fd. Readers use blocking
    // framing loops, so restore their mode before handing the stream over.
    stream.set_nonblocking(false)?;
    Ok((stream, address))
  }

  fn maintain(&mut self) -> io::Result<()> {
    match fs::symlink_metadata(&self.path) {
      Ok(meta) => {
        // Never unlink or touch an endpoint that replaced our own inode.
        if (meta.dev(), meta.ino()) == self.identity
          && self.touched.elapsed() >= Duration::from_secs(60) {
          use std::ffi::CString;
          use std::os::unix::ffi::OsStrExt;
          let path = CString::new(self.path.as_os_str().as_bytes())?;
          let result = unsafe {
            libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), libc::AT_SYMLINK_NOFOLLOW)
          };
          if result != 0 { return Err(io::Error::last_os_error()); }
          self.touched = Instant::now();
        }
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        // Bind first: an EADDRINUSE race must not displace another listener.
        let replacement = UnixListener::bind(&self.path)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        replacement.set_nonblocking(true)?;
        let meta = fs::symlink_metadata(&self.path)?;
        self.identity = (meta.dev(), meta.ino());
        self.listener = replacement;
        self.touched = Instant::now();
        self.warned = false;
        eprintln!("wand-render: restored missing socket; existing sessions preserved");
      }
      Err(error) => return Err(error),
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Read, Write};

  #[test]
  fn rebind_preserves_accepted_streams_and_refuses_replacement_paths() {
    let dir = std::env::temp_dir().join(format!("wand-socket-guard-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ipc.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let mut guard = RecoveringListener::new(listener, path.clone()).unwrap();
    let mut old_client = UnixStream::connect(&path).unwrap();
    let (mut old_server, _) = guard.accept().unwrap();
    old_server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    for _ in 0..2 {
      fs::remove_file(&path).unwrap();
      guard.maintain().unwrap();
      assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
      let mut new_client = UnixStream::connect(&path).unwrap();
      let (mut new_server, _) = guard.accept().unwrap();
      new_server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
      new_client.write_all(b"new").unwrap();
      let mut data = [0; 3];
      new_server.read_exact(&mut data).unwrap();
      assert_eq!(&data, b"new");
      old_client.write_all(b"old").unwrap();
      old_server.read_exact(&mut data).unwrap();
      assert_eq!(&data, b"old");
    }
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"replacement").unwrap();
    guard.maintain().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    fs::remove_dir_all(dir).unwrap();
  }
}
