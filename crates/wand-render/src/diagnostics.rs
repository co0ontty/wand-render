//! Bounded, private fault records independent of detached stdio.
//! Never record PTY bytes, commands, environment, tokens or panic payloads.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, TryLockError};

const MAX_BYTES: u64 = 256 * 1024;
static PATH: OnceLock<PathBuf> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Called once by the daemon after it has acquired its endpoint. The panic
/// hook records only source location/thread identity, not arbitrary payloads.
pub fn install(path: PathBuf) {
  if PATH.set(path).is_err() {
    return;
  }
  std::panic::set_hook(Box::new(|info| {
    let location = info.location().map(|location| {
      let file = Path::new(location.file()).file_name().unwrap_or_default().to_string_lossy();
      format!("{file}:{}:{}", location.line(), location.column())
    }).unwrap_or_else(|| "unknown".into());
    let thread = std::thread::current();
    record(&format!("panic:{location}"), thread.name());
  }));
}

pub fn record(code: &str, session: Option<&str>) {
  let Some(path) = PATH.get() else { return };
  let _guard = match WRITE_LOCK.try_lock() {
    Ok(guard) => guard,
    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    // In particular, a panic while writing a diagnostic must not deadlock
    // recursively in the panic hook, or block a different PTY's reader.
    Err(TryLockError::WouldBlock) => return,
  };
  // Observability must never become a new reason to lose PTY output.
  let _ = append(path, code, session);
}

fn append(path: &Path, code: &str, session: Option<&str>) -> std::io::Result<()> {
  if let Ok(metadata) = fs::symlink_metadata(path) {
    if !metadata.file_type().is_file() {
      return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "diagnostic path is not a regular file"));
    }
    if metadata.len() >= MAX_BYTES {
      fs::rename(path, path.with_extension("jsonl.1"))?;
    }
  }
  let mut options = OpenOptions::new();
  options.create(true).append(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
  }
  let mut file = options.open(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
  }
  let entry = serde_json::json!({
    "at": crate::time::iso8601_now(),
    "pid": std::process::id(),
    "code": code.chars().take(256).collect::<String>(),
    "session": session.map(|value| value.chars().take(128).collect::<String>()),
  });
  writeln!(file, "{entry}")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn test_path() -> PathBuf {
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("wand-render-diagnostics-{}-{unique}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    dir.join("faults.jsonl")
  }

  #[test]
  fn records_only_bounded_metadata_and_rotates_one_previous_file() {
    let path = test_path();
    append(&path, "screen-process-panic", Some("session-1")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["code"], "screen-process-panic");
    assert_eq!(value["session"], "session-1");
    assert_eq!(value.as_object().unwrap().len(), 4);
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    OpenOptions::new().write(true).open(&path).unwrap().set_len(MAX_BYTES).unwrap();
    append(&path, "pty-read-error", None).unwrap();
    assert_eq!(fs::metadata(path.with_extension("jsonl.1")).unwrap().len(), MAX_BYTES);
    assert!(fs::metadata(&path).unwrap().len() < 1024);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }

  #[test]
  #[cfg(unix)]
  fn refuses_symlink_targets() {
    let path = test_path();
    let target = path.with_extension("target");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(append(&path, "fault", None).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
  }
}
