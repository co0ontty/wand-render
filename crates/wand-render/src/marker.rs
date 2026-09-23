//! Wand 私有 CLI 退出标记的剥离。
//!
//! 启动 provider CLI 的 shell 会在命令前后打一个私有标记
//! （`\x1eWAND_CLI_EXIT:<token>:<code>\x1f`，见 `src/pty-shell-launch.ts`）。
//! legacy `terminald` 在把数据喂给屏幕模型与文本 journal **之前**剥掉它，
//! 所以 Render 必须做同样的事，否则标记会漏进 Server 重建的聊天文本里。
//!
//! 与 legacy 保持一致的细节：`chunks` 与 `data` 事件仍然是**原始** PTY 数据
//! （客户端与 Server 侧还有各自的标记处理），只有 `output` / 屏幕模型用剥离后的数据。

const MARKER_START: &str = "\u{1e}WAND_CLI_EXIT:";
const MARKER_END: &str = "\u{1f}";

/// 会话级有状态剥离器：标记可能被 PTY 切成多个 chunk。
#[derive(Debug)]
pub struct MarkerStripper {
  prefix: String,
  pending: String,
  completed: bool,
}

impl MarkerStripper {
  pub fn new(token: &str) -> Self {
    Self {
      prefix: format!("{MARKER_START}{token}:"),
      pending: String::new(),
      completed: false,
    }
  }

  pub fn is_completed(&self) -> bool {
    self.completed
  }

  /// 返回该 chunk 里可见（已剥离私有标记）的部分。
  pub fn consume(&mut self, chunk: &str) -> String {
    if self.completed {
      return chunk.to_string();
    }
    let mut combined = std::mem::take(&mut self.pending);
    combined.push_str(chunk);

    if let Some(start) = combined.find(self.prefix.as_str()) {
      let after_prefix = start + self.prefix.len();
      match combined[after_prefix..].find(MARKER_END) {
        None => {
          // 标记还没收完：整段挂起，等后续 chunk。
          self.pending = combined[start..].to_string();
          return combined[..start].to_string();
        }
        Some(offset) => {
          let status = &combined[after_prefix..after_prefix + offset];
          // 与 legacy `/^\d{1,3}$/` 等价：退出码最多三位。
          let is_exit_code = !status.is_empty()
            && status.len() <= 3
            && status.chars().all(|ch| ch.is_ascii_digit());
          if is_exit_code {
            self.completed = true;
            let mut visible = String::with_capacity(combined.len());
            visible.push_str(&combined[..start]);
            visible.push_str(&combined[after_prefix + offset + MARKER_END.len()..]);
            return visible;
          }
        }
      }
    }

    let partial = longest_prefix_suffix(&combined, &self.prefix);
    if partial > 0 {
      let boundary = combined.len() - partial;
      self.pending = combined[boundary..].to_string();
      return combined[..boundary].to_string();
    }
    combined
  }
}

/// `value` 的末尾有多少字符正好是 `prefix` 的前缀（保证切分处不会漏掉标记）。
fn longest_prefix_suffix(value: &str, prefix: &str) -> usize {
  let value_chars: Vec<char> = value.chars().collect();
  let prefix_chars: Vec<char> = prefix.chars().collect();
  let max = value_chars.len().min(prefix_chars.len().saturating_sub(1));
  for length in (1..=max).rev() {
    let tail = &value_chars[value_chars.len() - length..];
    if tail == &prefix_chars[..length] {
      return length;
    }
  }
  0
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strips_marker_in_one_chunk() {
    let mut stripper = MarkerStripper::new("tok");
    let visible = stripper.consume("before\u{1e}WAND_CLI_EXIT:tok:0\u{1f}after");
    assert_eq!(visible, "beforeafter");
    assert!(stripper.is_completed());
    assert_eq!(stripper.consume("later"), "later");
  }

  #[test]
  fn strips_marker_split_across_chunks() {
    let mut stripper = MarkerStripper::new("tok");
    let mut visible = String::new();
    visible.push_str(&stripper.consume("hello\u{1e}WAND_CLI_EX"));
    visible.push_str(&stripper.consume("IT:tok:13"));
    visible.push_str(&stripper.consume("0\u{1f}bye"));
    assert_eq!(visible, "hellobye");
    assert!(stripper.is_completed());
  }

  #[test]
  fn holds_partial_prefix_at_the_end_without_data_loss() {
    let mut stripper = MarkerStripper::new("tok");
    let first = stripper.consume("abc\u{1e}WAND");
    assert_eq!(first, "abc");
    let second = stripper.consume("_CLI_EXIT:tok:9\u{1f}");
    assert_eq!(second, "");
    assert!(stripper.is_completed());
  }

  #[test]
  fn keeps_foreign_markers_visible() {
    let mut stripper = MarkerStripper::new("tok");
    let visible = stripper.consume("x\u{1e}WAND_CLI_EXIT:other:0\u{1f}y");
    assert_eq!(visible, "x\u{1e}WAND_CLI_EXIT:other:0\u{1f}y");
    assert!(!stripper.is_completed());
  }

  #[test]
  fn ignores_non_numeric_status() {
    let mut stripper = MarkerStripper::new("tok");
    let visible = stripper.consume("\u{1e}WAND_CLI_EXIT:tok:abc\u{1f}");
    assert_eq!(visible, "\u{1e}WAND_CLI_EXIT:tok:abc\u{1f}");
    assert!(!stripper.is_completed());
  }
}
