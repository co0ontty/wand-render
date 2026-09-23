//! 信号名 → 编号。协议里 `kill` 的 `signal` 是名字（`"SIGTERM"` 等），
//! 与 legacy `src/signal-utils.ts` 的表保持同名同义。
//!
//! 这里刻意用 `libc` 常量而不是硬编码数字：legacy 的 TS 表是 Linux 编号，
//! 在 macOS 上 SIGBUS/SIGSTOP 之类会错位，用 libc 才能真正杀对信号。

/// `kill` 未指定信号时的默认值（node-pty 与 legacy 都是 SIGTERM）。
pub const DEFAULT: i32 = libc::SIGTERM;

/// 名字 → 编号；大小写不敏感，允许省略 `SIG` 前缀。未知名字返回 `None`。
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn accepts_the_documented_names() {
    assert_eq!(number("SIGTERM"), Some(libc::SIGTERM));
    assert_eq!(number("SIGKILL"), Some(libc::SIGKILL));
    assert_eq!(number("SIGINT"), Some(libc::SIGINT));
    assert_eq!(number("SIGHUP"), Some(libc::SIGHUP));
    assert_eq!(number("SIGQUIT"), Some(libc::SIGQUIT));
  }

  #[test]
  fn is_case_insensitive_and_prefix_optional() {
    assert_eq!(number("sigterm"), Some(libc::SIGTERM));
    assert_eq!(number(" term "), Some(libc::SIGTERM));
  }

  #[test]
  fn rejects_unknown_names() {
    assert_eq!(number("SIGBREAKFAST"), None);
    assert_eq!(number(""), None);
  }
}
