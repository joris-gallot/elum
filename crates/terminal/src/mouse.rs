//! Terminal mouse-report encoding.
//!
//! The API is deliberately pure and GPUI-light: callers provide a cell point,
//! a button/scroll event, modifiers, and the current alacritty `TermMode`.
//! The returned bytes can be written straight to the SSH/PTTY channel.

use alacritty_terminal::term::TermMode;
use gpui::{Modifiers, MouseButton};

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
