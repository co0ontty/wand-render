//! 常驻进程的内存指标（`stats.rssBytes` / `stats.liveBytes`）。
//!
//! liveBytes 由 registry 汇总各会话保留状态的估值；这里负责 RSS 和物理内存：
//! Linux 读 `/proc/self/statm`，其他 Unix 退化成 `getrusage` 的峰值常驻集
//! （daemon 常驻、内存平稳，峰值与当前值差距很小，够用且不需要 mach 绑定）。
//! 非 Unix 没有等价的可移植接口，返回 0。

#[cfg(unix)]
#[cfg(target_os = "macos")]
const RUSAGE_UNIT_BYTES: u64 = 1;
#[cfg(unix)]
#[cfg(not(target_os = "macos"))]
const RUSAGE_UNIT_BYTES: u64 = 1024;

#[cfg(unix)]
pub fn rss_bytes() -> u64 {
  #[cfg(target_os = "linux")]
  if let Some(bytes) = linux_resident_bytes() {
    return bytes;
  }
  max_rss_bytes()
}

/// 用于新会话准入。无法可靠查询的平台返回 `None`，调用方使用保守后备预算。
#[cfg(target_os = "linux")]
pub fn physical_memory_bytes() -> Option<u64> {
  let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
  let line = contents.lines().find(|line| line.starts_with("MemTotal:"))?;
  let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
  let host = kib.checked_mul(1024)?;
  // 容器里 MemTotal 常显示宿主机内存；cgroup v2 上限更接近进程实际可用量。
  let cgroup_limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
    .ok()
    .and_then(|value| value.trim().parse::<u64>().ok())
    .filter(|bytes| *bytes > 0);
  Some(cgroup_limit.map_or(host, |limit| host.min(limit)))
}

#[cfg(target_os = "macos")]
pub fn physical_memory_bytes() -> Option<u64> {
  let mut bytes = 0u64;
  let mut length = std::mem::size_of::<u64>();
  let result = unsafe {
    libc::sysctlbyname(
      c"hw.memsize".as_ptr(),
      (&mut bytes as *mut u64).cast(),
      &mut length,
      std::ptr::null_mut(),
      0,
    )
  };
  (result == 0 && length == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn physical_memory_bytes() -> Option<u64> {
  None
}

/// 只用于新会话准入的保守预算：物理内存的四分之一，最多 2 GiB。
/// 无法探测内存的平台退回 512 MiB；此值不限制既有 PTY 的继续运行。
pub fn admission_budget_bytes() -> u64 {
  budget_for_physical_memory(physical_memory_bytes())
}

fn budget_for_physical_memory(bytes: Option<u64>) -> u64 {
  const MAX_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
  const UNKNOWN_MEMORY_BUDGET: u64 = 512 * 1024 * 1024;
  bytes
    .filter(|bytes| *bytes > 0)
    .map(|bytes| (bytes / 4).min(MAX_BUDGET))
    .unwrap_or(UNKNOWN_MEMORY_BUDGET)
}

/// Windows 第一阶段不支持：没有 `getrusage` 等价物，0 表示「未知」。
#[cfg(not(unix))]
pub fn rss_bytes() -> u64 {
  0
}

#[cfg(unix)]
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

#[cfg(unix)]
fn max_rss_bytes() -> u64 {
  let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
  if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
    return 0;
  }
  (usage.ru_maxrss.max(0) as u64) * RUSAGE_UNIT_BYTES
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;

  #[test]
  fn rss_is_plausible() {
    let bytes = rss_bytes();
    // 真实进程必然占用几十 KB 以上、几十 GB 以下。
    assert!(bytes > 64 * 1024, "implausible rss: {bytes}");
    assert!(bytes < 64 * 1024 * 1024 * 1024, "implausible rss: {bytes}");
  }

  #[test]
  fn admission_budget_tracks_host_size_and_caps_at_two_gib() {
    assert_eq!(budget_for_physical_memory(Some(1 << 30)), 256 << 20);
    assert_eq!(budget_for_physical_memory(Some(16 << 30)), 2 << 30);
    assert_eq!(budget_for_physical_memory(None), 512 << 20);
  }
}
