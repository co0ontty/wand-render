//! 常驻进程的内存指标（`stats.rssBytes` / `stats.liveBytes`）。
//!
//! liveBytes 由 registry 汇总各会话 journal 的字节数；这里只负责 RSS：
//! Linux 读 `/proc/self/statm`，其他平台退化成 `getrusage` 的峰值常驻集
//! （daemon 常驻、内存平稳，峰值与当前值差距很小，够用且不需要 mach 绑定）。

#[cfg(target_os = "macos")]
const RUSAGE_UNIT_BYTES: u64 = 1;
#[cfg(not(target_os = "macos"))]
const RUSAGE_UNIT_BYTES: u64 = 1024;

pub fn rss_bytes() -> u64 {
  #[cfg(target_os = "linux")]
  if let Some(bytes) = linux_resident_bytes() {
    return bytes;
  }
  max_rss_bytes()
}

#[cfg(target_os = "linux")]
fn linux_resident_bytes() -> Option<u64> {
  let content = std::fs::read_to_string("/proc/self/statm").ok()?;
  // 第二列是常驻页数。
  let resident_pages: u64 = content.split_whitespace().nth(1)?.parse().ok()?;
  let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if page_size <= 0 {
    return None;
  }
  Some(resident_pages * page_size as u64)
}

fn max_rss_bytes() -> u64 {
  let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
  if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
    return 0;
  }
  (usage.ru_maxrss.max(0) as u64) * RUSAGE_UNIT_BYTES
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rss_is_plausible() {
    let bytes = rss_bytes();
    // 真实进程必然占用几十 KB 以上、几十 GB 以下。
    assert!(bytes > 64 * 1024, "implausible rss: {bytes}");
    assert!(bytes < 64 * 1024 * 1024 * 1024, "implausible rss: {bytes}");
  }
}
