//! ISO-8601 时间戳（`hello.startedAt` / 元数据文件用）。
//!
//! 只为这一个字段引入 chrono / time 不值得：Render 是常驻小进程，
//! 这里用 civil-from-days 算法把 epoch 秒转成 UTC 字符串。

use std::time::{SystemTime, UNIX_EPOCH};

/// `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
pub fn iso8601_from_unix_seconds(seconds: i64) -> String {
  let days = seconds.div_euclid(86_400);
  let second_of_day = seconds.rem_euclid(86_400);
  let (year, month, day) = civil_from_days(days);
  let hour = second_of_day / 3600;
  let minute = (second_of_day % 3600) / 60;
  let second = second_of_day % 60;
  format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub fn iso8601_now() -> String {
  let seconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
    Ok(duration) => duration.as_secs() as i64,
    // 系统时钟早于 1970 是异常配置，但不能让守护进程因此崩掉。
    Err(_) => 0,
  };
  iso8601_from_unix_seconds(seconds)
}

/// Howard Hinnant 的 civil_from_days：epoch 天数 → (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
  let shifted = days + 719_468;
  let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
  let day_of_era = shifted - era * 146_097; // [0, 146096]
  let year_of_era =
    (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
  let year = year_of_era + era * 400;
  let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
  let month_prime = (5 * day_of_year + 2) / 153; // [0, 11]
  let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32; // [1, 31]
  let month = if month_prime < 10 {
    month_prime + 3
  } else {
    month_prime - 9
  } as u32;
  let adjusted_year = if month <= 2 { year + 1 } else { year };
  (adjusted_year, month, day)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn formats_known_epochs() {
    assert_eq!(iso8601_from_unix_seconds(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso8601_from_unix_seconds(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(iso8601_from_unix_seconds(951_782_400), "2000-02-29T00:00:00Z");
  }

  #[test]
  fn now_is_well_formed() {
    let now = iso8601_now();
    assert_eq!(now.len(), 20, "unexpected ISO-8601 length: {now}");
    assert!(now.ends_with('Z'));
    assert!(now.starts_with("20"));
  }
}
