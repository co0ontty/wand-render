//! 信号名 → 编号。协议里 `kill` 的 `signal` 是名字（`"SIGTERM"` 等），
//! 与 legacy `src/signal-utils.ts` 的表保持同名同义。
//!
//! 这里刻意用 `libc` 常量而不是硬编码数字：legacy 的 TS 表是 Linux 编号，
//! 在 macOS 上 SIGBUS/SIGSTOP 之类会错位，用 libc 才能真正杀对信号。
//!
//! 只有 Unix 有这套 POSIX 信号：Windows 上 `kill` 走 ConPTY 的关闭语义
//! （第一阶段不实现，见协议 §9.5.3），所以名字表在非 Unix 上恒为「未知」。

/// `kill` 未指定信号时的默认值：**SIGHUP**（协议 §9.4）。
///
/// 对齐 node-pty 的 `kill()` 与 legacy daemon：调用方不传 signal 时想要的是
/// 「关闭终端」，不是「礼貌请进程退出」——SIGTERM 会被不少 TUI 当作可清理、
/// 可忽略的信号，语义完全不同。
#[cfg(unix)]
pub const DEFAULT: i32 = libc::SIGHUP;
/// 非 Unix 没有 SIGHUP。这里只留一个占位常量保证类型可用；真正的投递在
/// `session::kill` 上直接返回「平台不支持」，不会走到这里。
#[cfg(not(unix))]
pub const DEFAULT: i32 = libc::SIGTERM;

/// 名字 → 编号；大小写不敏感，允许省略 `SIG` 前缀。未知名字返回 `None`。
#[cfg(unix)]
pub fn number(name: &str) -> Option<i32> {
  let upper = name.trim().to_ascii_uppercase();
  let body = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
  let signo = match body {
    "HUP" => libc::SIGHUP,
    "INT" => libc::SIGINT,
    "QUIT" => libc::SIGQUIT,
    "KILL" => libc::SIGKILL,
    "TERM" => libc::SIGTERM,
    "STOP" => libc::SIGSTOP,
    "CONT" => libc::SIGCONT,
    "USR1" => libc::SIGUSR1,
    "USR2" => libc::SIGUSR2,
    "PIPE" => libc::SIGPIPE,
    "ALRM" => libc::SIGALRM,
    "ABRT" => libc::SIGABRT,
    "CHLD" => libc::SIGCHLD,
    _ => return None,
  };
  Some(signo)
}

/// Windows 没有 POSIX 信号：任何名字都算未知，调用方会拿到 `badRequest`。
#[cfg(not(unix))]
pub fn number(_name: &str) -> Option<i32> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(unix)]
  #[test]
  fn accepts_the_documented_names() {
    assert_eq!(number("SIGTERM"), Some(libc::SIGTERM));
    assert_eq!(number("SIGKILL"), Some(libc::SIGKILL));
    assert_eq!(number("SIGINT"), Some(libc::SIGINT));
    assert_eq!(number("SIGHUP"), Some(libc::SIGHUP));
    assert_eq!(number("SIGQUIT"), Some(libc::SIGQUIT));
  }

  #[cfg(unix)]
  #[test]
  fn is_case_insensitive_and_prefix_optional() {
    assert_eq!(number("sigterm"), Some(libc::SIGTERM));
    assert_eq!(number(" term "), Some(libc::SIGTERM));
  }

  #[cfg(unix)]
  #[test]
  fn rejects_unknown_names() {
    assert_eq!(number("SIGBREAKFAST"), None);
    assert_eq!(number(""), None);
  }

  /// §9.4：不传 signal 时是「关闭终端」，即 SIGHUP，不是 SIGTERM。
  #[cfg(unix)]
  #[test]
  fn default_signal_is_sighup() {
    assert_eq!(DEFAULT, libc::SIGHUP);
    assert_eq!(number("HUP"), Some(DEFAULT));
    assert_ne!(DEFAULT, libc::SIGTERM);
  }

  #[cfg(not(unix))]
  #[test]
  fn windows_has_no_posix_signal_names() {
    assert_eq!(number("SIGHUP"), None);
    assert_eq!(number("SIGTERM"), None);
  }
}
