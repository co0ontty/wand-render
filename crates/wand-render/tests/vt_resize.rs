//! Regression for the vt100 resize invariant used by Claude/tmux redraws.
//! A two-cell glyph clipped at the right edge must not leave a wide leader
//! pointing beyond the row, in either the active or the hidden screen.

use vt100::Parser;

#[test]
fn clipped_wide_cell_can_be_overwritten_or_erased_after_resize() {
  for alternate in [false, true] {
    for redraw in ["x", "\x1b[K", "\x1b[1X", "\x1b[1P"] {
      let mut parser = Parser::new(3, 10, 10);
      if alternate {
        parser.process(b"\x1b[?1049h");
      }
      parser.process("kept\x1b[1;9H中".as_bytes());
      parser.screen_mut().set_size(3, 9);
      let edge = parser.screen().cell(0, 8).unwrap();
      assert!(!edge.is_wide(), "a clipped wide leader must be cleared");
      assert!(!edge.is_wide_continuation());
      parser.process(format!("\x1b[1;9H{redraw}").as_bytes());
      assert!(parser.screen().contents().starts_with("kept"));
      let _ = parser.screen().contents_formatted();
    }
  }
}

#[test]
fn resize_repairs_hidden_normal_screen_as_well_as_alternate_screen() {
  let mut parser = Parser::new(3, 10, 10);
  parser.process("\x1b[1;9H中\x1b[?1049h\x1b[1;9H文".as_bytes());
  parser.screen_mut().set_size(3, 9);
  parser.process(b"\x1b[1;9Hx\x1b[?1049l\x1b[1;9Hy");
  assert_eq!(parser.screen().cell(0, 8).unwrap().contents(), "y");
}
