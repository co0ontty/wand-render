//! 有界文本窗口。协议里的 `output` 与 `chunks` 都按**字符**（不是字节）计数，
//! 因为客户端与服务端两侧的 legacy 实现（`PTY_OUTPUT_MAX_SIZE`）都是按字符截断的。
//!
//! 用分段（`VecDeque<String>`）而不是一个大 `String` 保存：每次追加都要从头部
//! 丢弃旧内容时，`String::drain` 会搬移剩余全部字节（对 20 万字符窗口就是 O(n)），
//! 小 chunk 高频输出时会退化成 O(n²)。分段只在真正需要时切掉最老的一段。

use std::collections::VecDeque;

/// 按字符取尾部；不会切断多字节标量。
pub fn tail_chars(value: &str, max_chars: usize) -> &str {
  if max_chars == 0 {
    return "";
  }
  let count = value.chars().count();
  if count <= max_chars {
    return value;
  }
  match value.char_indices().nth(count - max_chars) {
    Some((index, _)) => &value[index..],
    None => value,
  }
}

/// 有界累积文本（`output` 语义：保留最新的 `max_chars` 个字符）。
#[derive(Debug)]
pub struct TextWindow {
  max_chars: usize,
  chars: usize,
  segments: VecDeque<String>,
}

impl TextWindow {
  pub fn new(max_chars: usize) -> Self {
    Self {
      max_chars,
      chars: 0,
      segments: VecDeque::new(),
    }
  }

  pub fn push(&mut self, chunk: &str) {
    if chunk.is_empty() || self.max_chars == 0 {
      return;
    }
    let chunk_chars = chunk.chars().count();
    if chunk_chars >= self.max_chars {
      // 单个 chunk 就超限时只留它的末尾，与 legacy `safeSliceTail` 一致。
      self.segments.clear();
      self.segments.push_back(tail_chars(chunk, self.max_chars).to_string());
      self.chars = self.max_chars;
      return;
    }
    self.segments.push_back(chunk.to_string());
    self.chars += chunk_chars;
    self.trim();
  }

  fn trim(&mut self) {
    let mut overflow = self.chars.saturating_sub(self.max_chars);
    while overflow > 0 {
      let front_chars = match self.segments.front() {
        Some(front) => front.chars().count(),
        None => {
          self.chars = 0;
          return;
        }
      };
      if front_chars <= overflow {
        self.segments.pop_front();
        self.chars -= front_chars;
        overflow -= front_chars;
        continue;
      }
      let keep = front_chars - overflow;
      let front = self.segments.pop_front().unwrap_or_default();
      let trimmed = tail_chars(&front, keep).to_string();
      self.segments.push_front(trimmed);
      self.chars -= overflow;
      overflow = 0;
    }
  }

  pub fn chars(&self) -> usize {
    self.chars
  }

  pub fn bytes(&self) -> usize {
    self.segments.iter().map(String::len).sum()
  }

  pub fn to_string_value(&self) -> String {
    let mut joined = String::with_capacity(self.bytes());
    for segment in &self.segments {
      joined.push_str(segment);
    }
    joined
  }
}

/// 有界 chunk 重放窗口（`chunks` 语义：按字符累计 ≤ `max_chars`，保留最新的）。
///
/// 与 legacy `appendTerminalChunkWindow` 等价：单个 chunk 超限时先截成
/// 「末尾 max_chars 个字符」，再从最老的一端整块丢弃。
#[derive(Debug)]
pub struct ChunkWindow {
  max_chars: usize,
  chars: usize,
  entries: VecDeque<(u64, String)>,
}

impl ChunkWindow {
  pub fn new(max_chars: usize) -> Self {
    Self {
      max_chars,
      chars: 0,
      entries: VecDeque::new(),
    }
  }

  pub fn push(&mut self, seq: u64, chunk: &str) {
    let stored = if self.max_chars == 0 {
      String::new()
    } else if chunk.chars().count() > self.max_chars {
      tail_chars(chunk, self.max_chars).to_string()
    } else {
      chunk.to_string()
    };
    let stored_chars = stored.chars().count();
    self.chars += stored_chars;
    self.entries.push_back((seq, stored));
    while self.chars > self.max_chars && self.entries.len() > 1 {
      if let Some((_, front)) = self.entries.pop_front() {
        self.chars -= front.chars().count();
      }
    }
  }

  pub fn chars(&self) -> usize {
    self.chars
  }

  pub fn bytes(&self) -> usize {
    self.entries.iter().map(|(_, data)| data.len()).sum()
  }

  pub fn iter(&self) -> impl Iterator<Item = (u64, &str)> {
    self.entries.iter().map(|(seq, data)| (*seq, data.as_str()))
  }

  pub fn last_seq(&self) -> Option<u64> {
    self.entries.back().map(|(seq, _)| *seq)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use wand_render_protocol::PTY_OUTPUT_MAX_CHARS;

  #[test]
  fn text_window_keeps_the_tail() {
    let mut window = TextWindow::new(10);
    window.push("abcde");
    window.push("fghij");
    assert_eq!(window.to_string_value(), "abcdefghij");
    window.push("klm");
    assert_eq!(window.to_string_value(), "defghijklm");
    assert_eq!(window.chars(), 10);
    assert_eq!(window.bytes(), 10);
  }

  #[test]
  fn text_window_truncates_oversized_chunk_to_its_tail() {
    let mut window = TextWindow::new(4);
    window.push("abcdefgh");
    assert_eq!(window.to_string_value(), "efgh");
  }

  #[test]
  fn text_window_never_splits_scalars() {
    let mut window = TextWindow::new(2);
    window.push("中文字");
    assert_eq!(window.to_string_value(), "文字");
  }

  #[test]
  fn chunk_window_bounds_by_chars_and_keeps_newest() {
    let mut window = ChunkWindow::new(10);
    window.push(1, "aaa");
    window.push(2, "bbb");
    window.push(3, "ccc");
    window.push(4, "ddd");
    let seqs: Vec<u64> = window.iter().map(|(seq, _)| seq).collect();
    assert_eq!(window.chars(), 9);
    assert_eq!(seqs, vec![2, 3, 4]);
    assert_eq!(window.last_seq(), Some(4));
  }

  #[test]
  fn chunk_window_truncates_single_oversized_chunk() {
    let mut window = ChunkWindow::new(5);
    window.push(1, "0123456789");
    assert_eq!(window.chars(), 5);
    assert_eq!(window.iter().collect::<Vec<_>>(), vec![(1, "56789")]);
  }

  /// 30 万字符灌进协议上限的窗口后必须只剩最新的 20 万，且是尾部内容。
  #[test]
  fn chunk_window_bounds_three_hundred_thousand_chars() {
    let mut window = ChunkWindow::new(PTY_OUTPUT_MAX_CHARS);
    let mut expected_tail = String::new();
    for index in 0..300_000usize {
      let chunk = format!("{index:07}");
      expected_tail.push_str(&chunk);
      window.push(index as u64 + 1, &chunk);
    }
    assert!(window.chars() <= PTY_OUTPUT_MAX_CHARS, "chars={}", window.chars());
    let joined: String = window.iter().map(|(_, data)| data).collect();
    assert!(expected_tail.ends_with(&joined));
    assert_eq!(window.last_seq(), Some(300_000));
  }

  #[test]
  fn tail_chars_handles_empty_and_zero() {
    assert_eq!(tail_chars("abc", 0), "");
    assert_eq!(tail_chars("", 5), "");
    assert_eq!(tail_chars("abc", 9), "abc");
  }
}
