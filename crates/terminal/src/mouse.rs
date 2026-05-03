//! Terminal mouse-report encoding.

use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::term::TermMode;
use gpui::{Modifiers, MouseButton, Pixels, Point};

/// Element-local pixel -> absolute alacritty grid point + cell side. Out-of-grid
/// points clamp to the nearest edge with `side` indicating the overshoot direction.
pub fn pixel_to_point_and_side(
  pos: Point<Pixels>,
  cell_width: Pixels,
  line_height: Pixels,
  cols: usize,
  rows: usize,
  display_offset: usize,
) -> (AlacPoint, Side) {
  let cell_w = f32::from(cell_width).max(1.0);
  let line_h = f32::from(line_height).max(1.0);
  let x = f32::from(pos.x).max(0.0);
  let y = f32::from(pos.y).max(0.0);

  let mut col = (x / cell_w) as usize;
  let cell_x = x % cell_w;
  let mut side = if cell_x > cell_w / 2.0 {
    Side::Right
  } else {
    Side::Left
  };
  let last_col = cols.saturating_sub(1);
  if col > last_col {
    col = last_col;
    side = Side::Right;
  }

  let mut viewport_row = (y / line_h) as i32;
  if rows > 0 && viewport_row >= rows as i32 {
    viewport_row = rows as i32 - 1;
    side = Side::Right;
  }
  if viewport_row < 0 {
    viewport_row = 0;
    side = Side::Left;
  }

  let line = Line(viewport_row - display_offset as i32);
  (AlacPoint::new(line, Column(col)), side)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseCell {
  pub row: usize,
  pub col: usize,
}

impl MouseCell {
  pub fn new(row: usize, col: usize) -> Self {
    Self { row, col }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
  Up,
  Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseFormat {
  Sgr,
  Normal { utf8: bool },
}

impl MouseFormat {
  fn from_mode(mode: TermMode) -> Self {
    if mode.contains(TermMode::SGR_MOUSE) {
      Self::Sgr
    } else {
      Self::Normal {
        utf8: mode.contains(TermMode::UTF8_MOUSE),
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtermMouseButton {
  Left = 0,
  Middle = 1,
  Right = 2,
  LeftMove = 32,
  MiddleMove = 33,
  RightMove = 34,
  NoneMove = 35,
  ScrollUp = 64,
  ScrollDown = 65,
}

impl XtermMouseButton {
  fn from_button(button: MouseButton) -> Option<Self> {
    Some(match button {
      MouseButton::Left => Self::Left,
      MouseButton::Middle => Self::Middle,
      MouseButton::Right => Self::Right,
      MouseButton::Navigate(_) => return None,
    })
  }

  fn from_move_button(button: Option<MouseButton>) -> Option<Self> {
    Some(match button {
      Some(MouseButton::Left) => Self::LeftMove,
      Some(MouseButton::Middle) => Self::MiddleMove,
      Some(MouseButton::Right) => Self::RightMove,
      Some(MouseButton::Navigate(_)) => return None,
      None => Self::NoneMove,
    })
  }

  fn from_scroll(direction: ScrollDirection) -> Self {
    match direction {
      ScrollDirection::Up => Self::ScrollUp,
      ScrollDirection::Down => Self::ScrollDown,
    }
  }
}

pub fn mouse_reporting_enabled(mode: TermMode) -> bool {
  mode.intersects(TermMode::MOUSE_MODE)
}

pub fn mouse_button_report(
  cell: MouseCell,
  button: MouseButton,
  modifiers: Modifiers,
  pressed: bool,
  mode: TermMode,
) -> Option<Vec<u8>> {
  if !mouse_reporting_enabled(mode) {
    return None;
  }

  let button = XtermMouseButton::from_button(button)?;
  mouse_report(
    cell,
    button,
    pressed,
    modifiers,
    MouseFormat::from_mode(mode),
  )
}

pub fn mouse_move_report(
  cell: MouseCell,
  pressed_button: Option<MouseButton>,
  modifiers: Modifiers,
  mode: TermMode,
) -> Option<Vec<u8>> {
  if !mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) {
    return None;
  }

  let button = XtermMouseButton::from_move_button(pressed_button)?;
  if mode.contains(TermMode::MOUSE_DRAG) && matches!(button, XtermMouseButton::NoneMove) {
    return None;
  }

  mouse_report(cell, button, true, modifiers, MouseFormat::from_mode(mode))
}

pub fn scroll_report(
  cell: MouseCell,
  direction: ScrollDirection,
  modifiers: Modifiers,
  mode: TermMode,
) -> Option<Vec<u8>> {
  if !mouse_reporting_enabled(mode) {
    return None;
  }

  mouse_report(
    cell,
    XtermMouseButton::from_scroll(direction),
    true,
    modifiers,
    MouseFormat::from_mode(mode),
  )
}

pub fn alt_scroll(scroll_lines: i32, app_cursor: bool) -> Vec<u8> {
  let cmd = if scroll_lines > 0 { b'A' } else { b'B' };
  let prefix: &[u8] = if app_cursor { b"\x1bO" } else { b"\x1b[" };
  let count = scroll_lines.unsigned_abs() as usize;
  let mut out = Vec::with_capacity(prefix.len() * count + count);

  for _ in 0..count {
    out.extend_from_slice(prefix);
    out.push(cmd);
  }

  out
}

fn mouse_report(
  cell: MouseCell,
  button: XtermMouseButton,
  pressed: bool,
  modifiers: Modifiers,
  format: MouseFormat,
) -> Option<Vec<u8>> {
  let code = button as u8 + modifier_code(modifiers);

  match format {
    MouseFormat::Sgr => Some(sgr_mouse_report(cell, code, pressed).into_bytes()),
    MouseFormat::Normal { utf8 } => {
      if pressed {
        normal_mouse_report(cell, code, utf8)
      } else {
        normal_mouse_report(cell, 3 + modifier_code(modifiers), utf8)
      }
    }
  }
}

fn modifier_code(modifiers: Modifiers) -> u8 {
  let mut code = 0;
  if modifiers.shift {
    code += 4;
  }
  if modifiers.alt {
    code += 8;
  }
  if modifiers.control {
    code += 16;
  }
  code
}

fn normal_mouse_report(cell: MouseCell, button: u8, utf8: bool) -> Option<Vec<u8>> {
  let max_point = if utf8 { 2015 } else { 223 };
  if cell.row >= max_point || cell.col >= max_point {
    return None;
  }

  let mut out = vec![b'\x1b', b'[', b'M', 32 + button];
  push_normal_coord(&mut out, cell.col, utf8);
  push_normal_coord(&mut out, cell.row, utf8);
  Some(out)
}

fn push_normal_coord(out: &mut Vec<u8>, pos: usize, utf8: bool) {
  let encoded = 32 + 1 + pos;
  if utf8 && encoded >= 128 {
    out.push((0xc0 + encoded / 64) as u8);
    out.push((0x80 + (encoded & 63)) as u8);
  } else {
    out.push(encoded as u8);
  }
}

fn sgr_mouse_report(cell: MouseCell, button: u8, pressed: bool) -> String {
  let suffix = if pressed { 'M' } else { 'm' };
  format!(
    "\x1b[<{};{};{}{}",
    button,
    cell.col + 1,
    cell.row + 1,
    suffix
  )
}

#[cfg(test)]
mod pixel_to_point_tests {
  use super::*;
  use gpui::px;

  fn pt(x: f32, y: f32) -> Point<Pixels> {
    Point::new(px(x), px(y))
  }

  // Standard grid metrics used across the tests so the math is easy to
  // verify by inspection (10×16 cells, 80×24 grid).
  const CW: f32 = 10.0;
  const LH: f32 = 16.0;
  const COLS: usize = 80;
  const ROWS: usize = 24;

  #[test]
  fn click_inside_left_half_picks_left_side() {
    let (p, s) = pixel_to_point_and_side(pt(3.0, 8.0), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p, AlacPoint::new(Line(0), Column(0)));
    assert_eq!(s, Side::Left);
  }

  #[test]
  fn click_inside_right_half_picks_right_side() {
    let (p, s) = pixel_to_point_and_side(pt(7.0, 8.0), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p, AlacPoint::new(Line(0), Column(0)));
    assert_eq!(s, Side::Right);
  }

  #[test]
  fn click_at_cell_boundary_advances_to_next_column() {
    // x = 10.0 = exactly cell_w → next column, left half.
    let (p, s) = pixel_to_point_and_side(pt(10.0, 8.0), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p.column, Column(1));
    assert_eq!(s, Side::Left);
  }

  #[test]
  fn click_past_right_edge_clamps_to_last_column_with_right_side() {
    let x = (COLS as f32 + 5.0) * CW;
    let (p, s) = pixel_to_point_and_side(pt(x, 8.0), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p.column, Column(COLS - 1));
    assert_eq!(s, Side::Right);
  }

  #[test]
  fn click_past_bottom_clamps_to_last_row_with_right_side() {
    let y = (ROWS as f32 + 3.0) * LH;
    let (p, s) = pixel_to_point_and_side(pt(0.0, y), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p.line, Line(ROWS as i32 - 1));
    assert_eq!(s, Side::Right);
  }

  #[test]
  fn negative_pixels_clamp_to_origin() {
    let (p, s) = pixel_to_point_and_side(pt(-5.0, -10.0), px(CW), px(LH), COLS, ROWS, 0);
    assert_eq!(p, AlacPoint::new(Line(0), Column(0)));
    assert_eq!(s, Side::Left);
  }

  #[test]
  fn display_offset_shifts_line_into_scrollback() {
    // viewport row 0 with display_offset=5 → grid line -5 (5 lines into history).
    let (p, _) = pixel_to_point_and_side(pt(0.0, 0.0), px(CW), px(LH), COLS, ROWS, 5);
    assert_eq!(p.line, Line(-5));
  }

  #[test]
  fn display_offset_lets_visible_bottom_stay_in_grid() {
    // Last visible row with offset=5 → grid line (ROWS-1) - 5 = 18.
    let (p, _) = pixel_to_point_and_side(
      pt(0.0, (ROWS as f32 - 1.0) * LH + 1.0),
      px(CW),
      px(LH),
      COLS,
      ROWS,
      5,
    );
    assert_eq!(p.line, Line(ROWS as i32 - 1 - 5));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cell() -> MouseCell {
    MouseCell::new(4, 9)
  }

  fn shift_ctrl() -> Modifiers {
    Modifiers {
      shift: true,
      control: true,
      ..Modifiers::default()
    }
  }

  #[test]
  fn no_mouse_mode_produces_no_report() {
    assert_eq!(
      mouse_button_report(
        cell(),
        MouseButton::Left,
        Modifiers::default(),
        true,
        TermMode::empty()
      ),
      None
    );
  }

  #[test]
  fn normal_left_press_uses_x10_encoding() {
    assert_eq!(
      mouse_button_report(
        MouseCell::new(0, 0),
        MouseButton::Left,
        Modifiers::default(),
        true,
        TermMode::MOUSE_REPORT_CLICK
      ),
      Some(b"\x1b[M !!".to_vec())
    );
  }

  #[test]
  fn normal_release_uses_button_three() {
    assert_eq!(
      mouse_button_report(
        MouseCell::new(0, 0),
        MouseButton::Left,
        Modifiers::default(),
        false,
        TermMode::MOUSE_REPORT_CLICK
      ),
      Some(b"\x1b[M#!!".to_vec())
    );
  }

  #[test]
  fn sgr_right_press_uses_one_based_coordinates() {
    assert_eq!(
      mouse_button_report(
        cell(),
        MouseButton::Right,
        Modifiers::default(),
        true,
        TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
      ),
      Some(b"\x1b[<2;10;5M".to_vec())
    );
  }

  #[test]
  fn sgr_release_uses_lowercase_m() {
    assert_eq!(
      mouse_button_report(
        cell(),
        MouseButton::Left,
        Modifiers::default(),
        false,
        TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
      ),
      Some(b"\x1b[<0;10;5m".to_vec())
    );
  }

  #[test]
  fn modifiers_are_added_to_button_code() {
    assert_eq!(
      mouse_button_report(
        cell(),
        MouseButton::Left,
        shift_ctrl(),
        true,
        TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
      ),
      Some(b"\x1b[<20;10;5M".to_vec())
    );
  }

  #[test]
  fn drag_mode_reports_drag_but_not_hover() {
    let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
    assert_eq!(
      mouse_move_report(cell(), Some(MouseButton::Left), Modifiers::default(), mode),
      Some(b"\x1b[<32;10;5M".to_vec())
    );
    assert_eq!(
      mouse_move_report(cell(), None, Modifiers::default(), mode),
      None
    );
  }

  #[test]
  fn motion_mode_reports_hover() {
    assert_eq!(
      mouse_move_report(
        cell(),
        None,
        Modifiers::default(),
        TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE
      ),
      Some(b"\x1b[<35;10;5M".to_vec())
    );
  }

  #[test]
  fn scroll_reports_wheel_buttons() {
    let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
    assert_eq!(
      scroll_report(cell(), ScrollDirection::Up, Modifiers::default(), mode),
      Some(b"\x1b[<64;10;5M".to_vec())
    );
    assert_eq!(
      scroll_report(cell(), ScrollDirection::Down, Modifiers::default(), mode),
      Some(b"\x1b[<65;10;5M".to_vec())
    );
  }

  #[test]
  fn normal_mode_clips_coordinates_above_223() {
    assert_eq!(
      mouse_button_report(
        MouseCell::new(223, 0),
        MouseButton::Left,
        Modifiers::default(),
        true,
        TermMode::MOUSE_REPORT_CLICK
      ),
      None
    );
  }

  #[test]
  fn utf8_mode_allows_larger_coordinates() {
    assert!(mouse_button_report(
      MouseCell::new(300, 300),
      MouseButton::Left,
      Modifiers::default(),
      true,
      TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE
    )
    .is_some());
  }

  #[test]
  fn alt_scroll_uses_cursor_sequences() {
    assert_eq!(alt_scroll(2, false), b"\x1b[A\x1b[A".to_vec());
    assert_eq!(alt_scroll(-1, true), b"\x1bOB".to_vec());
  }
}
