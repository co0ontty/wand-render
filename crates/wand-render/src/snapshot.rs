//! VT 屏幕模型 → 版本化 ANSI 快照。
//!
//! 判据是**屏幕等价**，不是字节相同：把 `data` 写进一个同尺寸终端后，
//! 可见行、光标位置、SGR 属性、备用屏幕状态必须与 Render 内部的屏幕一致
//! （见 `docs/render-protocol.md` §5）。
//!
//! 组合方式：
//! 1. 历史行（≤ [`SCROLLBACK_LINES`]）先写，最终落进客户端回滚缓冲 —— 只影响历史，不影响可见屏；
//! 2. 备用屏幕开关（两个方向都发，避免客户端停在错的屏幕上）；
//! 3. `contents_formatted()`：自带 `ClearAttrs` + `\x1b[H\x1b[J` + 每格 SGR 差异 + 光标定位；
//! 4. `attributes_formatted()` + `input_mode_formatted()`：恢复活动属性、应用键盘/光标、
//!    bracketed paste、鼠标模式；
//! 5. autowrap：vt100 不跟踪 DECAWM，由 [`DecModeTracker`] 从原始字节流里补。
//!    光标可见性已包含在 (3) 的 `HideCursor` 里。

use vt100::Screen;
use wand_render_protocol::{PendingOp, TerminalSnapshot, SCROLLBACK_LINES};

/// 快照版本号，客户端按这个字段决定如何应用 `data`（协议 v1 固定为 1）。
pub const VERSION: u32 = 1;

/// 拍基线：当前屏幕 + 空 pending（基线之后的操作由调用方按序记录）。
pub fn build_baseline(screen: &Screen, autowrap: bool) -> TerminalSnapshot {
  build(screen, Vec::new(), autowrap)
}

/// 生成快照。`pending` 是基线之后的 data/resize 序列，按序重放即可回到当前屏幕。
pub fn build(screen: &Screen, pending: Vec<PendingOp>, autowrap: bool) -> TerminalSnapshot {
  let (rows, cols) = screen.size();
  let mut data: Vec<u8> = Vec::new();
  if !screen.alternate_screen() {
    write_scrollback(&mut data, screen, cols);
  }
  data.extend_from_slice(if screen.alternate_screen() {
    b"\x1b[?1049h"
  } else {
    b"\x1b[?1049l"
  });
  data.extend_from_slice(&screen.contents_formatted());
  // 光标定位可能重画单元格，所以活动属性必须在定位之后再设一次。
  data.extend_from_slice(&screen.attributes_formatted());
  data.extend_from_slice(&screen.input_mode_formatted());
  data.extend_from_slice(if autowrap { b"\x1b[?7h" } else { b"\x1b[?7l" });

  TerminalSnapshot {
    version: VERSION,
    // 屏幕内容只会由合法 UTF-8 构造出来（PTY 解码器已经丢掉非法字节）。
    data: String::from_utf8_lossy(&data).into_owned(),
    cols,
    rows,
    pending,
  }
}

/// 写出可见屏之上最多 `SCROLLBACK_LINES` 行历史。
///
/// 在 `Screen` 的克隆上挪动 scrollback 偏移，避免动到正在被写入的屏幕；
/// 每行照 legacy 的换行规则输出（软换行的续行不再补 `\r\n`）。
fn write_scrollback(data: &mut Vec<u8>, screen: &Screen, cols: u16) {
  let mut view = screen.clone();
  // `set_scrollback` 会 clamp 到真实历史长度，于是能反查总行数。
  view.set_scrollback(usize::MAX);
  let total = view.scrollback();
  if total == 0 {
    return;
  }
  let start = total.saturating_sub(SCROLLBACK_LINES);
  for offset in (start + 1..=total).rev() {
    view.set_scrollback(offset);
    let wrapped = view.row_wrapped(0);
    if let Some(row) = view.rows_formatted(0, cols).next() {
      data.extend_from_slice(&row);
    }
    if !wrapped {
      data.extend_from_slice(b"\r\n");
    }
  }
}

/// DEC 私有模式跟踪。目前只关心 DECAWM(`?7`)，因为 vt100 不保存它，
/// 而 autowrap 会直接改变客户端重放 `pending` 时的结果。
#[derive(Debug)]
pub struct DecModeTracker {
  autowrap: bool,
  tail: Vec<u8>,
}

/// 单条私有模式序列最多允许的长度；超出即认为不是我们要找的东西。
const MAX_SEQUENCE_BYTES: usize = 32;

impl Default for DecModeTracker {
  fn default() -> Self {
    Self::new()
  }
}

impl DecModeTracker {
  pub fn new() -> Self {
    // 终端的默认值是「自动换行开启」。
    Self {
      autowrap: true,
      tail: Vec::new(),
    }
  }

  pub fn autowrap(&self) -> bool {
    self.autowrap
  }

  pub fn scan(&mut self, bytes: &[u8]) {
    let mut data = std::mem::take(&mut self.tail);
    data.extend_from_slice(bytes);
    let mut index = 0;
    while index < data.len() {
      if data[index] != 0x1b {
        index += 1;
        continue;
      }
      match parse_dec_private_mode(&data[index..]) {
        DecScan::Mode {
          matches_mode_7,
          final_byte,
          consumed,
        } => {
          if matches_mode_7 {
            self.autowrap = final_byte == b'h';
          }
          index += consumed;
        }
        DecScan::Incomplete => {
          // 序列被 chunk 切开：留着尾巴等下一次扫描。
          self.tail.extend_from_slice(&data[index..]);
          return;
        }
        DecScan::NotAMode => index += 1,
      }
    }
  }
}

enum DecScan {
  Mode {
    matches_mode_7: bool,
    final_byte: u8,
    consumed: usize,
  },
  Incomplete,
  NotAMode,
}

/// 解析 `ESC [ ? p1 ; p2 ... h|l`。
fn parse_dec_private_mode(bytes: &[u8]) -> DecScan {
  if bytes.len() > MAX_SEQUENCE_BYTES {
    return DecScan::NotAMode;
  }
  if bytes.len() < 2 {
    return DecScan::Incomplete;
  }
  if bytes[1] != b'[' {
    return DecScan::NotAMode;
  }
  if bytes.len() < 3 {
    return DecScan::Incomplete;
  }
  if bytes[2] != b'?' {
    return DecScan::NotAMode;
  }
  let mut index = 3;
  let mut mode: u32 = 0;
  let mut seen_digit = false;
  let mut matches = false;
  while index < bytes.len() {
    match bytes[index] {
      b'0'..=b'9' => {
        seen_digit = true;
        mode = mode.saturating_mul(10).saturating_add(u32::from(bytes[index] - b'0'));
        index += 1;
      }
      b';' => {
        if !seen_digit {
          return DecScan::NotAMode;
        }
        matches |= mode == 7;
        mode = 0;
        seen_digit = false;
        index += 1;
      }
      b'h' | b'l' => {
        if !seen_digit {
          return DecScan::NotAMode;
        }
        matches |= mode == 7;
        return DecScan::Mode {
          matches_mode_7: matches,
          final_byte: bytes[index],
          consumed: index + 1,
        };
      }
      _ => return DecScan::NotAMode,
    }
  }
  DecScan::Incomplete
}

#[cfg(test)]
mod tests {
  use super::*;
  use vt100::Parser;

  const ROWS: u16 = 10;
  const COLS: u16 = 40;

  fn feed(parser: &mut Parser, bytes: &[u8]) {
    parser.process(bytes);
  }

  /// 快照等价性：跑一段带 SGR 颜色与光标移动的脚本，取快照，
  /// 把快照喂进一个全新的同尺寸 parser，两边 `contents()` 必须一致。
  #[test]
  fn snapshot_restores_screen_equivalence() {
    let mut parser = Parser::new(ROWS, COLS, 0);
    feed(
      &mut parser,
      b"\x1b[31mred\x1b[0m plain\r\n\x1b[1;32mbold green\x1b[0m\r\n",
    );
    feed(&mut parser, b"line3\r\nline4\r\n");
    // 移动光标后覆盖写，制造「可见屏有洞」的形态。
    feed(&mut parser, b"\x1b[2;3HXX\x1b[5;1Hlast\x1b[3;7H\x1b[4mU\x1b[0m");

    let snapshot = build(parser.screen(), Vec::new(), true);
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.cols, COLS);
    assert_eq!(snapshot.rows, ROWS);

    let mut replay = Parser::new(ROWS, COLS, 0);
    feed(&mut replay, snapshot.data.as_bytes());

    assert_eq!(replay.screen().contents(), parser.screen().contents());
    assert_eq!(
      replay.screen().cursor_position(),
      parser.screen().cursor_position()
    );
    // SGR 属性也要等价：'red' 落在 (0,0)，'bold green' 落在 (1,0)。
    for (row, col) in [(0u16, 0u16), (1, 0), (2, 0)] {
      let live = parser.screen().cell(row, col);
      let restored = replay.screen().cell(row, col);
      assert_eq!(live.map(|c| c.contents()), restored.map(|c| c.contents()));
      assert_eq!(live.map(|c| c.fgcolor()), restored.map(|c| c.fgcolor()));
      assert_eq!(live.map(|c| c.bold()), restored.map(|c| c.bold()));
    }
  }

  #[test]
  fn snapshot_matches_attributes_and_cursor_position() {
    let mut parser = Parser::new(ROWS, COLS, 0);
    feed(&mut parser, b"\x1b[33;44mstyled\x1b[0m\x1b[4;10Hmid");
    let snapshot = build(parser.screen(), Vec::new(), true);
    let mut replay = Parser::new(ROWS, COLS, 0);
    feed(&mut replay, snapshot.data.as_bytes());
    assert_eq!(
      replay.screen().cursor_position(),
      parser.screen().cursor_position()
    );
    assert_eq!(
      replay.screen().cell(0, 0).map(|cell| cell.fgcolor()),
      parser.screen().cell(0, 0).map(|cell| cell.fgcolor())
    );
    assert_eq!(
      replay.screen().cell(0, 0).map(|cell| cell.bgcolor()),
      parser.screen().cell(0, 0).map(|cell| cell.bgcolor())
    );
  }

  #[test]
  fn snapshot_rebuilds_the_alternate_screen() {
    let mut parser = Parser::new(ROWS, COLS, 0);
    feed(&mut parser, b"normal screen\r\n");
    feed(&mut parser, b"\x1b[?1049h\x1b[2J\x1b[Halt content");
    assert!(parser.screen().alternate_screen());

    let snapshot = build(parser.screen(), Vec::new(), true);
    assert!(snapshot.data.contains("\x1b[?1049h"));
    let mut replay = Parser::new(ROWS, COLS, 0);
    feed(&mut replay, snapshot.data.as_bytes());
    assert!(replay.screen().alternate_screen());
    assert_eq!(replay.screen().contents(), parser.screen().contents());
  }

  #[test]
  fn snapshot_replays_pending_after_the_baseline() {
    let mut parser = Parser::new(ROWS, COLS, 0);
    feed(&mut parser, b"base\r\n");
    let pending = vec![
      PendingOp::Data {
        data: "next line\r\n".to_string(),
      },
      PendingOp::Resize {
        cols: 30,
        rows: 8,
      },
    ];
    let snapshot = build(parser.screen(), pending, true);
    assert_eq!(snapshot.pending.len(), 2);

    let mut replay = Parser::new(ROWS, COLS, 0);
    feed(&mut replay, snapshot.data.as_bytes());
    for op in &snapshot.pending {
      match op {
        PendingOp::Data { data } => feed(&mut replay, data.as_bytes()),
        PendingOp::Resize { cols, rows } => replay.screen_mut().set_size(*rows, *cols),
      }
    }
    // 基线 + pending 重放后屏幕应是「base + next line」，最后按新尺寸裁剪。
    assert!(replay.screen().contents().starts_with("base"));
    assert!(replay.screen().contents().contains("next line"));
  }

  #[test]
  fn scrollback_is_bounded_and_pushed_above_the_screen() {
    let mut parser = Parser::new(4, 20, SCROLLBACK_LINES);
    for index in 0..100 {
      feed(&mut parser, format!("history-{index}\r\n").as_bytes());
    }
    let snapshot = build(parser.screen(), Vec::new(), true);
    assert!(snapshot.data.contains("history-99"));
    // 可见屏仍然是最后 4 行。
    let mut replay = Parser::new(4, 20, 0);
    feed(&mut replay, snapshot.data.as_bytes());
    assert_eq!(replay.screen().contents(), parser.screen().contents());
  }

  #[test]
  fn scrollback_cap_drops_the_oldest_lines() {
    let mut parser = Parser::new(4, 20, 20_000);
    for index in 0..6_000 {
      feed(&mut parser, format!("line-{index}\r\n").as_bytes());
    }
    let snapshot = build(parser.screen(), Vec::new(), true);
    let history_lines = snapshot.data.matches("\r\n").count();
    assert!(
      history_lines <= SCROLLBACK_LINES + usize::from(ROWS),
      "history lines {history_lines} exceeded the {SCROLLBACK_LINES} cap"
    );
    let mut replay = Parser::new(4, 20, 0);
    feed(&mut replay, snapshot.data.as_bytes());
    assert_eq!(replay.screen().contents(), parser.screen().contents());
  }

  #[test]
  fn autowrap_mode_is_tracked_across_chunks() {
    let mut tracker = DecModeTracker::new();
    assert!(tracker.autowrap());
    tracker.scan(b"abc\x1b[?7");
    assert!(tracker.autowrap());
    tracker.scan(b"lrest");
    assert!(!tracker.autowrap());
    tracker.scan(b"\x1b[?25l\x1b[?7h");
    assert!(tracker.autowrap());
  }

  #[test]
  fn snapshot_emits_the_autowrap_mode() {
    let mut parser = Parser::new(ROWS, COLS, 0);
    feed(&mut parser, b"\x1b[?7lwrapped-off");
    let mut tracker = DecModeTracker::new();
    tracker.scan(b"\x1b[?7l");
    let snapshot = build(parser.screen(), Vec::new(), tracker.autowrap());
    assert!(snapshot.data.contains("\x1b[?7l"));
    let on = build(parser.screen(), Vec::new(), true);
    assert!(on.data.contains("\x1b[?7h"));
  }
}
