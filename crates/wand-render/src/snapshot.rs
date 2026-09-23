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

/// `list` 返回的单个快照上限（协议 §9.1.1）：序列化后必须 ≤ 64KiB。
///
/// 这里没有放进 `wand-render-protocol`（协议单一真源）是因为本轮改动范围只覆盖
/// Rust 侧实现；两侧一旦需要共享这个常量，应当提升到协议 crate 并同步 TS 镜像。
pub const LIST_SNAPSHOT_MAX_BYTES: usize = 64 * 1024;

/// `list` 用的有界快照（协议 §9.1.1）。
///
/// `list` 把一个响应里内联所有会话的输出、chunk 窗口与 5000 行回滚快照：单个
/// 1000 列满回滚会话就能到 5MB，十几个会话会撞上单帧 `MAX_FRAME_BYTES`，而超限的
/// 响应以前被静默丢弃 → 客户端只看到 10s 超时。裁剪顺序是「先丢最少信息，再保证
/// 写进终端不会把终端卡住」：
///
/// 1. 完整快照已经够小 → 原样返回；
/// 2. 丢掉 `pending`（它是基线之后的增量，丢了只是预览略旧）；
/// 3. 截断 `data` 到前缀，并去掉末尾未收尾的转义序列；
/// 4. 连元信息都放不下（`max_bytes` 极小）→ `None`（置空，客户端退回用 `output` 重放）。
///
/// 裁剪后的快照**刻意不是屏幕等价的**：它只服务「列表展示 + 有/无屏幕判断」，
/// 需要精确重建屏幕的调用方必须走 `attach`（协议 §9.1.2）。
pub fn bound_for_list(snapshot: TerminalSnapshot, max_bytes: usize) -> Option<TerminalSnapshot> {
  if serialized_len(&snapshot).is_some_and(|length| length <= max_bytes) {
    return Some(snapshot);
  }
  let mut candidate = snapshot;
  candidate.pending.clear();
  if serialized_len(&candidate).is_some_and(|length| length <= max_bytes) {
    return Some(candidate);
  }

  // JSON 转义（`\r`、引号、控制字符）让「字符数」无法直接换算成字节数，
  // 所以直接用真实序列化结果对字符数二分。已知完整 data 放不下，
  // 所以在 [0, total) 里找最大的可放下前缀；候选里已经包含截断尾部的属性重置，
  // 保证补完重置之后仍然不超预算。
  let total = candidate.data.chars().count();
  let mut low = 0usize;
  let mut high = total;
  while high - low > 1 {
    let mid = low + (high - low) / 2;
    if prefix_with_reset_fits(&candidate, mid, max_bytes) {
      low = mid;
    } else {
      high = mid;
    }
  }
  candidate.data = candidate.data.chars().take(low).collect();
  let trimmed = trim_incomplete_escape(&candidate.data);
  if trimmed.len() != candidate.data.len() {
    candidate.data = trimmed.to_string();
  }
  candidate.data.push_str(SNAPSHOT_TRUNCATION_RESET);
  serialized_len(&candidate)
    .is_some_and(|length| length <= max_bytes)
    .then_some(candidate)
}

/// 截断点可能停在任意 SGR 状态下：补一个重置，避免预览把后续输出染色。
const SNAPSHOT_TRUNCATION_RESET: &str = "\u{1b}[0m";

fn serialized_len(snapshot: &TerminalSnapshot) -> Option<usize> {
  serde_json::to_vec(snapshot).ok().map(|body| body.len())
}

fn prefix_with_reset_fits(snapshot: &TerminalSnapshot, chars: usize, max_bytes: usize) -> bool {
  let mut data: String = snapshot.data.chars().take(chars).collect();
  data.push_str(SNAPSHOT_TRUNCATION_RESET);
  let candidate = TerminalSnapshot {
    data,
    ..snapshot.clone()
  };
  serialized_len(&candidate).is_some_and(|length| length <= max_bytes)
}

/// 截断必须保证不留下**未收尾**的转义序列。
///
/// 例子：`data` 被截到 `...\x1b[3` 时，客户端把这段写进终端后，终端解析器会一直
/// 等这条 CSI 的收尾字节，随后真正的输出会被吞进这条残缺序列里 —— 比「屏幕不完整」
/// 严重得多。所以截断后要从末尾往前找，把最后一个未完成的 ESC 序列整段砍掉。
fn trim_incomplete_escape(data: &str) -> &str {
  let bytes = data.as_bytes();
  let mut cut = data.len();
  let mut search_end = data.len();
  loop {
    let index = match bytes[..search_end].iter().rposition(|byte| *byte == 0x1b) {
      Some(index) => index,
      None => return &data[..cut],
    };
    if escape_end(&bytes[index..]).is_some() {
      return &data[..cut];
    }
    cut = index;
    search_end = index;
  }
}

/// 从 `bytes[0] == ESC` 开始，返回这条转义序列的收尾位置（不含收尾字符之后）。
/// `None` 表示字符串在序列收尾前就结束了。
fn escape_end(bytes: &[u8]) -> Option<usize> {
  // 裸 ESC 结尾（没有第二个字节）本身就不完整。
  let second = *bytes.get(1)?;
  match second {
    // CSI：参数字节 0x30..=0x3f 与中间字节 0x20..=0x2f，收尾字节 0x40..=0x7e。
    b'[' => {
      let mut index = 2;
      while let Some(byte) = bytes.get(index).copied() {
        match byte {
          0x20..=0x3f => index += 1,
          0x40..=0x7e => return Some(index + 1),
          _ => return None,
        }
      }
      None
    }
    // OSC(`]`) / DCS(`P`) / SOS(`X`) / PM(`^`) / APC(`_`)：以 BEL 或 ST(`ESC \`) 收尾。
    b']' | b'P' | b'X' | b'^' | b'_' => {
      let mut index = 2;
      while let Some(byte) = bytes.get(index).copied() {
        if byte == 0x07 {
          return Some(index + 1);
        }
        if byte == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
          return Some(index + 2);
        }
        index += 1;
      }
      None
    }
    // 两字节序列（`ESC M`、`ESC 7` 等）本身就是完整的。
    _ => Some(2),
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

  // ── §9.1.1 list 快照裁剪 ──

  fn manual_snapshot(data: &str, pending: &str) -> TerminalSnapshot {
    TerminalSnapshot {
      version: VERSION,
      data: data.to_string(),
      cols: 80,
      rows: 24,
      pending: if pending.is_empty() {
        Vec::new()
      } else {
        vec![PendingOp::Data {
          data: pending.to_string(),
        }]
      },
    }
  }

  fn assert_no_dangling_escape(data: &str) {
    let bytes = data.as_bytes();
    if let Some(index) = bytes.iter().rposition(|byte| *byte == 0x1b) {
      assert!(
        escape_end(&bytes[index..]).is_some(),
        "snapshot data ends inside an escape sequence: {:?}",
        &data[index..]
      );
    }
  }

  #[test]
  fn list_budget_keeps_small_snapshots_intact() {
    let snapshot = manual_snapshot("\x1b[31mred\x1b[0m", "tail");
    let bounded = bound_for_list(snapshot.clone(), LIST_SNAPSHOT_MAX_BYTES).expect("fits");
    assert_eq!(bounded, snapshot);
  }

  /// pending 是基线之后的增量，超出预算时先丢它（attach 会给全量）。
  #[test]
  fn list_budget_drops_pending_before_truncating_data() {
    let snapshot = manual_snapshot("visible", &"p".repeat(300_000));
    assert!(serialized_len(&snapshot).expect("json") > LIST_SNAPSHOT_MAX_BYTES);
    let bounded = bound_for_list(snapshot, LIST_SNAPSHOT_MAX_BYTES).expect("bounded");
    assert!(bounded.pending.is_empty(), "pending must be dropped first");
    assert_eq!(bounded.data, "visible", "data must survive dropping pending");
    assert!(serialized_len(&bounded).expect("json") <= LIST_SNAPSHOT_MAX_BYTES);
  }

  /// data 自己超预算时截断成前缀，且绝不留未收尾的转义序列。
  #[test]
  fn list_budget_truncates_data_without_breaking_the_terminal() {
    // 前缀全是转义序列，于是截断点大概率落在某条序列中间（正是 trim 要处理的形态）。
    let snapshot = manual_snapshot(
      &format!("{}{}", "\x1b[31m".repeat(200), "x".repeat(20_000)),
      "",
    );
    let bounded = bound_for_list(snapshot.clone(), 1024).expect("bounded");
    assert!(bounded.data.len() < snapshot.data.len(), "data must be truncated");
    assert!(serialized_len(&bounded).expect("json") <= 1024);
    assert_no_dangling_escape(&bounded.data);
    // 截断后补一个属性重置，避免预览把后续输出染色。
    assert!(bounded.data.ends_with("\x1b[0m"));

    // 语义证据：把截断后的前缀写进终端再写 TAIL，TAIL 必须真的被打印出来
    // （若前缀停在 `\x1b[3` 中间，TAIL 会被吞进那条残缺序列而消失）。
    let mut replay = Parser::new(24, 80, 0);
    replay.process(bounded.data.as_bytes());
    replay.process(b"TAIL");
    assert!(
      replay.screen().contents().contains("TAIL"),
      "truncated snapshot swallowed subsequent output"
    );
  }

  /// 连元信息都放不下时置空（而不是回一个会撑爆帧的快照）。
  #[test]
  fn list_budget_nulls_out_when_even_metadata_does_not_fit() {
    let snapshot = manual_snapshot("data", "pending");
    assert!(bound_for_list(snapshot, 8).is_none());
  }

  #[test]
  fn incomplete_escapes_are_trimmed() {
    assert_eq!(trim_incomplete_escape("abc"), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1b"), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1b["), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1b[3"), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1b[31m"), "abc\x1b[31m");
    // 前一条完整、后一条残缺：只砍残缺的那条。
    assert_eq!(trim_incomplete_escape("abc\x1b[31m\x1b[3"), "abc\x1b[31m");
    // OSC / DCS 必须有 BEL 或 ST 收尾。
    assert_eq!(trim_incomplete_escape("abc\x1b]0;title"), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1b]0;title\x07"), "abc\x1b]0;title\x07");
    assert_eq!(trim_incomplete_escape("abc\x1bP1;2"), "abc");
    assert_eq!(trim_incomplete_escape("abc\x1bP1;2\x1b\\"), "abc\x1bP1;2\x1b\\");
    // 两字节序列（ESC M / ESC 7）是完整的。
    assert_eq!(trim_incomplete_escape("abc\x1bM"), "abc\x1bM");
    assert_eq!(trim_incomplete_escape("\x1b"), "");
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
