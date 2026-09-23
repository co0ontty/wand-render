//! Render 的按 config 隔离寻址，必须与 `src/render-protocol.ts` 的 `renderPaths`
//! 逐字节一致：suffix = `sha256(path.resolve(configPath))` 的 hex 前 12 位。
//!
//! 注意 TS 用的是 `path.resolve`（纯词法绝对化，**不做** realpath），不是文档里
//! 写的 realpath；这里按实现走，并用 python3 独立算出的 sha 值做交叉验证。

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderPaths {
  pub socket_path: PathBuf,
  pub token_path: PathBuf,
  pub pid_path: PathBuf,
  pub meta_path: PathBuf,
}

impl RenderPaths {
  pub fn config_dir(&self) -> &Path {
    self.token_path.parent().unwrap_or(Path::new("/"))
  }
}

/// 与 TS `renderPaths` 对齐的路径派生。
pub fn render_paths(config_path: &Path) -> RenderPaths {
  let resolved = lexical_absolute(config_path);
  let suffix = config_suffix(&resolved);
  let dir = resolved
    .parent()
    .map(|parent| {
      if parent.as_os_str().is_empty() {
        PathBuf::from("/")
      } else {
        parent.to_path_buf()
      }
    })
    .unwrap_or_else(|| PathBuf::from("/"));
  let uid = unsafe { libc::getuid() };
  RenderPaths {
    // macOS 的 Unix socket 路径上限约 100 字节，所以 socket 放 /tmp 保持短。
    socket_path: PathBuf::from(format!("/tmp/wand-render-{uid}-{suffix}.sock")),
    token_path: dir.join(format!(".render-{suffix}.token")),
    pid_path: dir.join(format!(".render-{suffix}.pid")),
    meta_path: dir.join(format!(".render-{suffix}.json")),
  }
}

/// `sha256(realpath)` 的 hex 前 12 位。
pub fn config_suffix(resolved_config_path: &Path) -> String {
  let mut hasher = Sha256::new();
  hasher.update(resolved_config_path.as_os_str().as_encoded_bytes());
  let digest = hasher.finalize();
  let mut hex = String::with_capacity(12);
  for byte in digest.iter().take(6) {
    hex.push_str(&format!("{byte:02x}"));
  }
  hex
}

/// `path.resolve` 的等价物：相对路径拼上 cwd，并做词法归一（去掉 `.`、
/// 折叠 `..`），但**不**解析符号链接。
pub fn lexical_absolute(path: &Path) -> PathBuf {
  let joined = if path.is_absolute() {
    path.to_path_buf()
  } else {
    match std::env::current_dir() {
      Ok(cwd) => cwd.join(path),
      Err(_) => path.to_path_buf(),
    }
  };
  let mut normalized = PathBuf::new();
  for component in joined.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        // 根目录之上的 `..` 直接丢弃，与 path.resolve 一致。
        if !normalized.pop() {
          normalized.push("/");
        }
      }
      other => normalized.push(other.as_os_str()),
    }
  }
  if normalized.as_os_str().is_empty() {
    normalized.push("/");
  }
  normalized
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn suffix_matches_python_sha256_for_known_paths() {
    // 独立算出来的值：python3 -c 'hashlib.sha256(b"/tmp/...").hexdigest()[:12]'，
    // 并与 `renderPaths` 的实际输出对照过。
    assert_eq!(config_suffix(Path::new("/tmp/wand-render-config.json")), "0bc2538acaf7");
    assert_eq!(config_suffix(Path::new("/tmp/wand-dev/config.json")), "262a247724e8");
  }

  #[test]
  fn socket_path_uses_uid_and_suffix() {
    let paths = render_paths(Path::new("/tmp/wand-render-config.json"));
    let uid = unsafe { libc::getuid() };
    assert_eq!(
      paths.socket_path,
      PathBuf::from(format!("/tmp/wand-render-{uid}-0bc2538acaf7.sock"))
    );
    assert_eq!(
      paths.token_path,
      PathBuf::from("/tmp/.render-0bc2538acaf7.token")
    );
    assert_eq!(paths.pid_path, PathBuf::from("/tmp/.render-0bc2538acaf7.pid"));
    assert_eq!(paths.meta_path, PathBuf::from("/tmp/.render-0bc2538acaf7.json"));
  }

  #[test]
  fn meta_files_live_next_to_the_config() {
    let paths = render_paths(Path::new("/tmp/wand-dev/config.json"));
    assert_eq!(paths.config_dir(), Path::new("/tmp/wand-dev"));
    assert_eq!(
      paths.token_path,
      PathBuf::from("/tmp/wand-dev/.render-262a247724e8.token")
    );
  }

  #[test]
  fn normalizes_like_path_resolve() {
    assert_eq!(
      lexical_absolute(Path::new("/tmp/a/../b/./config.json")),
      PathBuf::from("/tmp/b/config.json")
    );
    assert_eq!(lexical_absolute(Path::new("/../../../x")), PathBuf::from("/x"));
  }
}
