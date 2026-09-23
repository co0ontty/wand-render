//! Render 的按 config 隔离寻址，必须与 `src/render-protocol.ts` 的 `renderPaths`
//! 逐字节一致：suffix = `sha256(<realpath 归一化后的 config 路径>)` 的 hex 前 12 位。
//!
//! 归一化规则（协议 §9.2）：先 `canonicalize`（realpath，解析符号链接），
//! 路径还不存在（canonicalize 失败）时回退词法绝对化结果。两侧必须用同一条规则，
//! 否则同一个 config 经符号链接或不同写法访问会派生出两套 socket/token/pid，
//! 产生两个 Render 并分裂 PTY 所有权。

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// Windows 第一阶段不支持 Render：传输要换成命名管道，进程组与 POSIX 信号要按
/// ConPTY 语义重写（协议 §9.5.1）。入口处直接给出这句说明，而不是编译失败或
/// 运行到一半出现神秘崩溃。
pub const WINDOWS_UNSUPPORTED_MESSAGE: &str =
  "wand-render 的第一阶段只支持 macOS / Linux：Windows 需要命名管道与 ConPTY 实现（见 docs/render-protocol.md §9.5.1）";

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

/// 当前平台能否运行 Render。`Err` 里是可以直接打印给用户的说明。
pub fn check_platform_supported() -> Result<(), String> {
  if cfg!(unix) {
    Ok(())
  } else {
    Err(WINDOWS_UNSUPPORTED_MESSAGE.to_string())
  }
}

/// 与 TS `renderPaths` 对齐的路径派生。
pub fn render_paths(config_path: &Path) -> RenderPaths {
  let resolved = normalized_absolute(config_path);
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
  RenderPaths {
    socket_path: socket_path(&suffix),
    token_path: dir.join(format!(".render-{suffix}.token")),
    pid_path: dir.join(format!(".render-{suffix}.pid")),
    meta_path: dir.join(format!(".render-{suffix}.json")),
  }
}

/// 传输端点：Unix 用 domain socket，Windows 用命名管道（协议 §9.5.1）。
///
/// Unix socket 放 `/tmp` 是为了短路径（macOS 的 sun_path 上限约 104 字节）；
/// `uid` 进名字，避免同一机器上不同用户互相看见对方的 socket。
pub fn socket_path(suffix: &str) -> PathBuf {
  #[cfg(unix)]
  {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/wand-render-{uid}-{suffix}.sock"))
  }
  #[cfg(not(unix))]
  {
    // 命名管道没有「文件权限」，隔离靠管道名与 ACL；第一阶段不实现。
    PathBuf::from(format!(r"\\.\pipe\wand-render-{suffix}"))
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

/// config 路径归一化：先词法绝对化，再 `canonicalize`（realpath，解析符号链接），
/// `canonicalize` 失败（路径还不存在）就停在词法结果上。
///
/// 顺序很关键：Node 侧是 `realpathSync(path.resolve(configPath))`，而直接对原始路径
/// `canonicalize` 会在「`..` 前面是符号链接」这种写法上给出内核语义的不同结果
/// （内核把 `..` 解释成符号链接目标的父目录），两侧就分叉了 —— 分叉意味着
/// 两个 socket、两个 Render、PTY 所有权分裂（协议 §9.2）。
///
/// 另外注意「文件是否存在」会决定 `canonicalize` 是否成功：`/tmp` 在 macOS 上是
/// `/private/tmp` 的符号链接，所以 config 存在时得到 `/private/tmp/...`、不存在时
/// 得到 `/tmp/...`。
pub fn normalized_absolute(path: &Path) -> PathBuf {
  let lexical = lexical_absolute(path);
  std::fs::canonicalize(&lexical).unwrap_or(lexical)
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
  use std::sync::atomic::{AtomicU64, Ordering};

  fn unique_temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
      "wand-render-paths-{tag}-{}-{}",
      std::process::id(),
      COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
  }

  #[test]
  fn suffix_matches_python_sha256_for_known_paths() {
    // 独立算出来的值：python3 -c 'hashlib.sha256(b"/tmp/...").hexdigest()[:12]'，
    // 并与 `renderPaths` 的实际输出对照过。
    assert_eq!(config_suffix(Path::new("/tmp/wand-render-config.json")), "0bc2538acaf7");
    assert_eq!(config_suffix(Path::new("/tmp/wand-dev/config.json")), "262a247724e8");
  }

  #[test]
  fn normalizes_like_path_resolve() {
    assert_eq!(
      lexical_absolute(Path::new("/tmp/a/../b/./config.json")),
      PathBuf::from("/tmp/b/config.json")
    );
    assert_eq!(lexical_absolute(Path::new("/../../../x")), PathBuf::from("/x"));
  }

  /// 归一化必须走 realpath：同一个 config 的符号链接写法与真实写法派生出**同一套**路径。
  #[test]
  #[cfg(unix)]
  fn symlinked_directory_derives_the_same_paths() {
    let real = unique_temp_dir("real");
    let link_parent = unique_temp_dir("link");
    let link = link_parent.join("alias");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let config_name = Path::new("config.json");
    std::fs::write(real.join(config_name), "{}").expect("write config");

    let via_real = render_paths(&real.join(config_name));
    let via_link = render_paths(&link.join(config_name));
    assert_eq!(via_real, via_link, "a symlinked config must derive identical paths");
    // 归一化后的目录也必须是 realpath 后的（链接目录本身不能被保留）。
    assert_eq!(via_link.config_dir(), std::fs::canonicalize(&real).expect("canonical"));

    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_dir_all(&link_parent);
  }

  /// config 还不存在（第一次启动）时回退词法结果，而不是报错或产出空 suffix。
  #[test]
  fn missing_config_path_falls_back_to_the_lexical_result() {
    let dir = unique_temp_dir("missing");
    let config = dir.join("config.json");
    assert!(!config.exists());
    assert_eq!(normalized_absolute(&config), lexical_absolute(&config));
    let paths = render_paths(&config);
    assert_eq!(
      paths.token_path,
      lexical_absolute(&dir).join(format!(".render-{}.token", config_suffix(&lexical_absolute(&config))))
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 已存在的 config 必须解析成 realpath（符号链接被展开）。
  #[test]
  fn existing_config_path_is_canonicalized() {
    let dir = unique_temp_dir("existing");
    let real = dir.join("real");
    std::fs::create_dir_all(&real).expect("real dir");
    let config = real.join("config.json");
    std::fs::write(&config, "{}").expect("write config");
    assert_eq!(
      normalized_absolute(&config),
      std::fs::canonicalize(&config).expect("canonical")
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// 顺序必须是「先词法归一、后 realpath」：Node 侧是
  /// `realpathSync(path.resolve(p))`，内核语义（先 realpath 再解释 `..`）会在
  /// 「`..` 前面是符号链接」时给出另一个路径，两侧就分叉了。
  #[test]
  #[cfg(unix)]
  fn lexical_normalization_happens_before_realpath() {
    let root = unique_temp_dir("order");
    let deep = root.join("a").join("b");
    std::fs::create_dir_all(&deep).expect("dirs");
    std::os::unix::fs::symlink(&deep, root.join("link")).expect("symlink");
    std::fs::write(root.join("config.json"), "{}").expect("root config");
    std::fs::write(root.join("a").join("config.json"), "{}").expect("deep config");

    // 词法归一：`root/link/../config.json` → `root/config.json`。
    // 内核语义：`link` → `root/a/b`，`..` → `root/a`，于是落在 `root/a/config.json`。
    let via_link = root.join("link/../config.json");
    assert_eq!(
      normalized_absolute(&via_link),
      std::fs::canonicalize(root.join("config.json")).expect("canonical root config")
    );
    assert_ne!(
      normalized_absolute(&via_link),
      std::fs::canonicalize(root.join("a").join("config.json")).expect("canonical deep config")
    );
    let _ = std::fs::remove_dir_all(&root);
  }

  #[test]
  #[cfg(unix)]
  fn socket_path_uses_uid_and_suffix() {
    let uid = unsafe { libc::getuid() };
    assert_eq!(
      socket_path("0bc2538acaf7"),
      PathBuf::from(format!("/tmp/wand-render-{uid}-0bc2538acaf7.sock"))
    );
    // 用一个一定不存在的路径：归一化只能回退词法结果，断言因此与运行环境无关。
    let config = Path::new("/tmp/wand-render-paths-does-not-exist/config.json");
    assert!(!config.exists());
    let paths = render_paths(config);
    let suffix = config_suffix(&lexical_absolute(config));
    let dir = PathBuf::from("/tmp/wand-render-paths-does-not-exist");
    assert_eq!(paths.token_path, dir.join(format!(".render-{suffix}.token")));
    assert_eq!(paths.pid_path, dir.join(format!(".render-{suffix}.pid")));
    assert_eq!(paths.meta_path, dir.join(format!(".render-{suffix}.json")));
  }

  /// Windows 只派生命名管道形态（协议 §9.5.1），不提供 Unix socket。
  #[test]
  #[cfg(not(unix))]
  fn socket_path_uses_the_named_pipe_form_on_windows() {
    assert_eq!(
      socket_path("0bc2538acaf7"),
      PathBuf::from(r"\\.\pipe\wand-render-0bc2538acaf7")
    );
    assert!(check_platform_supported().is_err());
  }

  #[test]
  fn meta_files_live_next_to_the_config() {
    // 存在的路径会被 realpath 展开（macOS 的 /tmp → /private/tmp），所以这里用
    // 不存在的路径断言「meta 落在 config 同目录」这条关系。
    let config = Path::new("/tmp/wand-render-paths-does-not-exist/config.json");
    let paths = render_paths(config);
    assert_eq!(
      paths.config_dir(),
      Path::new("/tmp/wand-render-paths-does-not-exist")
    );
  }
}
