//! 有状态增量 UTF-8 解码器。
//!
//! 协议要求（`docs/render-protocol.md` §4）：只产出完整标量序列，不完整尾部留到
//! 下一个 chunk；会话结束时丢弃尾部，**绝不产出 U+FFFD**。因此这里对真正的
//! 非法字节序列选择丢弃而不是替换 —— 替换会往客户端屏幕上塞进一个不存在的字符，
//! 而 legacy 的 `StringDecoder` 语义在跨 chunk 边界时才需要「等待」这一层。

#[derive(Debug, Default)]
pub struct IncrementalUtf8Decoder {
  /// 最多 3 字节：只有可能是一个标量序列前缀的尾巴才会被留下。
  pending: Vec<u8>,
}

impl IncrementalUtf8Decoder {
  pub fn new() -> Self {
    Self { pending: Vec::new() }
  }

  /// 吃掉一段原始字节，返回其中已完成的文本（可能为空）。
  pub fn push(&mut self, bytes: &[u8]) -> String {
    if bytes.is_empty() && self.pending.is_empty() {
      return String::new();
    }
    let mut data = std::mem::take(&mut self.pending);
    data.extend_from_slice(bytes);

    let mut out = String::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
      match std::str::from_utf8(&data[index..]) {
        Ok(valid) => {
          out.push_str(valid);
          index = data.len();
        }
        Err(error) => {
          let valid_up_to = error.valid_up_to();
          if valid_up_to > 0 {
            out.push_str(std::str::from_utf8(&data[index..index + valid_up_to]).unwrap_or_default());
            index += valid_up_to;
          }
          match error.error_len() {
            // 尾部不完整：留到下一个 chunk 再拼。
            None => break,
            // 真正的非法序列：丢掉这些字节，不产出替换字符。
            Some(len) => index += len,
          }
        }
      }
    }
    self.pending = data[index..].to_vec();
    out
  }

  /// 会话结束：丢弃不完整尾部（不产出 U+FFFD）。
  pub fn discard_tail(&mut self) {
    self.pending.clear();
  }

  pub fn pending_bytes(&self) -> usize {
    self.pending.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn splits_multibyte_scalar_across_chunks() {
    let text = "a中b";
    let bytes = text.as_bytes();
    // 把「中」的三字节从中间切开：前 2 字节一波，剩下的一波。
    let mut decoder = IncrementalUtf8Decoder::new();
    let mut out = decoder.push(&bytes[..3]);
    assert_eq!(out, "a");
    out.push_str(&decoder.push(&bytes[3..]));
    assert_eq!(out, text);
    assert_eq!(decoder.pending_bytes(), 0);
  }

  #[test]
  fn splits_four_byte_scalar_into_three_chunks() {
    let text = "😀ok";
    let bytes = text.as_bytes();
    let mut decoder = IncrementalUtf8Decoder::new();
    let mut out = decoder.push(&bytes[..1]);
    out.push_str(&decoder.push(&bytes[1..2]));
    out.push_str(&decoder.push(&bytes[2..]));
    assert_eq!(out, text);
  }

  #[test]
  fn never_produces_replacement_character() {
    let mut decoder = IncrementalUtf8Decoder::new();
    let out = decoder.push(&[0x41, 0xff, 0xfe, 0x42]);
    assert_eq!(out, "AB");
    assert!(!out.contains('\u{fffd}'));
  }

  #[test]
  fn discarded_tail_never_emits_replacement_character() {
    let mut decoder = IncrementalUtf8Decoder::new();
    let first = decoder.push(&"中".as_bytes()[..2]);
    assert_eq!(first, "");
    assert_eq!(decoder.pending_bytes(), 2);
    decoder.discard_tail();
    assert_eq!(decoder.pending_bytes(), 0);
    assert_eq!(decoder.push(b"x"), "x");
  }

  #[test]
  fn long_multibyte_stream_round_trips() {
    let text = "中文🙂abc".repeat(1000);
    let bytes = text.as_bytes();
    let mut decoder = IncrementalUtf8Decoder::new();
    let mut out = String::new();
    // 任意 3 字节切分：每次都必须拼回原文，且不含替换字符。
    for chunk in bytes.chunks(3) {
      out.push_str(&decoder.push(chunk));
    }
    assert_eq!(out, text);
    assert!(!out.contains('\u{fffd}'));
  }
}
