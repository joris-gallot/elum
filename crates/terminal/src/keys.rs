//! Keystroke → byte encoding for the terminal input path.
//!
//! The function is pure: same `(keystroke, mode)` always yields the same
//! bytes. That makes it trivially testable without a window or app.

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mods {
  None,
  Alt,
  Ctrl,
  Shift,
  ShiftAlt,
  CtrlShift,
  CtrlAlt,
  ShiftCtrlAlt,
  /// Anything involving the platform-meta key (Cmd on macOS, Win on
  /// Linux/Windows). Those are reserved for app-level shortcuts; we
  /// don't forward them to the PTY.
  Other,
}

impl Mods {
  fn of(ks: &Keystroke) -> Self {
    match (
      ks.modifiers.alt,
      ks.modifiers.control,
      ks.modifiers.shift,
      ks.modifiers.platform,
    ) {
      (false, false, false, false) => Mods::None,
      (true, false, false, false) => Mods::Alt,
      (false, true, false, false) => Mods::Ctrl,
      (false, false, true, false) => Mods::Shift,
      (true, false, true, false) => Mods::ShiftAlt,
      (false, true, true, false) => Mods::CtrlShift,
      (true, true, false, false) => Mods::CtrlAlt,
      (true, true, true, false) => Mods::ShiftCtrlAlt,
      _ => Mods::Other,
    }
  }

  /// xterm modifier code in the range 1..=8 used inside CSI sequences
  /// like `\x1b[1;<code>A`. `1` means no modifier (and is omitted in the
  /// emitted sequence).
  fn xterm_code(self) -> Option<u8> {
    Some(match self {
      Mods::None => 1,
      Mods::Shift => 2,
      Mods::Alt => 3,
      Mods::ShiftAlt => 4,
      Mods::Ctrl => 5,
      Mods::CtrlShift => 6,
      Mods::CtrlAlt => 7,
      Mods::ShiftCtrlAlt => 8,
      Mods::Other => return None,
    })
  }
}

/// Encode a `Keystroke` as the byte sequence to send over the SSH channel.
/// Returns `None` if the keystroke has no terminal-meaningful encoding
/// (e.g. unmodified Ctrl held alone, an unknown key name).
pub fn keystroke_to_bytes(ks: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
  let mods = Mods::of(ks);
  let key = ks.key.as_str();

  if let Some(bytes) = special_key(key, &mods, mode) {
    return Some(bytes);
  }

  // Ctrl-letter: a..z map to 0x01..0x1a. Standard since the 1970s.
  if mods == Mods::Ctrl
    && key.len() == 1
    && key.chars().next().is_some_and(|c| c.is_ascii_lowercase())
  {
    let byte = key.bytes().next().unwrap() - b'a' + 1;
    return Some(vec![byte]);
  }

  // Plain printable input. Trust `key_char` first - GPUI sets it to the
  // literal character that would be typed (handles space, altgr, IME,
  // shifted letters). Only consult the bare `key` string if no char was
  // provided AND the key isn't a known named special - otherwise we'd
  // emit literal "tab" / "f1" / etc. as bytes.
  if matches!(mods, Mods::None | Mods::Shift) {
    if let Some(text) = ks.key_char.as_deref() {
      if !text.is_empty() {
        return Some(text.as_bytes().to_vec());
      }
    }
    if !is_named_key(key) {
      return Some(key.as_bytes().to_vec());
    }
  }

  None
}

/// Special-key table for the keys that matter to a daily SSH workflow.
fn special_key(key: &str, mods: &Mods, mode: TermMode) -> Option<Vec<u8>> {
  // Plain control keys with no modifier-friendly CSI/SS3 form. Handled
  // first since they take precedence over any structured dispatch below.
  if let Some(s) = simple_key(key, mods) {
    return Some(s);
  }

  let app_cursor = mode.contains(TermMode::APP_CURSOR);
  let mod_code = mods.xterm_code()?;

  // Cursor keys: arrows + home/end. Use SS3 when the remote requested
  // application cursor mode AND no modifiers are held; otherwise fall
  // back to CSI with an explicit modifier code (1 means "none" and is
  // emitted bare).
  if let Some(letter) = cursor_letter(key) {
    if mod_code == 1 {
      return Some(if app_cursor {
        format!("\x1bO{}", letter as char).into_bytes()
      } else {
        format!("\x1b[{}", letter as char).into_bytes()
      });
    }
    return Some(format!("\x1b[1;{}{}", mod_code, letter as char).into_bytes());
  }

  // F1..=F4 use SS3 letters (P/Q/R/S) without modifiers, CSI 1;mod when
  // modifiers are held.
  if let Some(letter) = f1_to_f4_letter(key) {
    if mod_code == 1 {
      return Some(format!("\x1bO{}", letter as char).into_bytes());
    }
    return Some(format!("\x1b[1;{}{}", mod_code, letter as char).into_bytes());
  }

  // Tilde-style keys: insert/delete/pageup/pagedown/F5..=F12.
  if let Some(num) = tilde_num(key) {
    if mod_code == 1 {
      return Some(format!("\x1b[{num}~").into_bytes());
    }
    return Some(format!("\x1b[{num};{mod_code}~").into_bytes());
  }

  None
}

/// Plain control keys whose encoding does not follow the CSI/SS3 family
/// (so the modifier-code machinery doesn't apply). Each variant lists
/// only the modifier combos that have a meaningful encoding.
fn simple_key(key: &str, mods: &Mods) -> Option<Vec<u8>> {
  let s: &[u8] = match (key, mods) {
    ("tab", Mods::None) => b"\x09",
    ("tab", Mods::Shift) => b"\x1b[Z",
    ("escape", Mods::None) => b"\x1b",
    ("enter", Mods::None) => b"\x0d",
    ("enter", Mods::Shift) => b"\x0a",
    ("enter", Mods::Alt) => b"\x1b\x0d",
    ("backspace", Mods::None) => b"\x7f",
    ("backspace", Mods::Ctrl) => b"\x08",
    ("backspace", Mods::Alt) => b"\x1b\x7f",
    ("backspace", Mods::Shift) => b"\x7f",
    ("space", Mods::Ctrl) => b"\x00",
    _ => return None,
  };
  Some(s.to_vec())
}

/// CSI/SS3 final byte for cursor keys.
fn cursor_letter(key: &str) -> Option<u8> {
  Some(match key {
    "up" => b'A',
    "down" => b'B',
    "right" => b'C',
    "left" => b'D',
    "home" => b'H',
    "end" => b'F',
    _ => return None,
  })
}

/// SS3 final byte for F1..=F4. F5+ use the tilde-number form.
fn f1_to_f4_letter(key: &str) -> Option<u8> {
  Some(match key {
    "f1" => b'P',
    "f2" => b'Q',
    "f3" => b'R',
    "f4" => b'S',
    _ => return None,
  })
}

/// Number used in `\x1b[<num>~` (or `\x1b[<num>;<mod>~`) for keys whose
/// encoding follows the tilde family.
fn tilde_num(key: &str) -> Option<u32> {
  Some(match key {
    "insert" => 2,
    "delete" => 3,
    "pageup" => 5,
    "pagedown" => 6,
    "f5" => 15,
    "f6" => 17,
    "f7" => 18,
    "f8" => 19,
    "f9" => 20,
    "f10" => 21,
    "f11" => 23,
    "f12" => 24,
    _ => return None,
  })
}

/// Returns true for keys that are named (and shouldn't be passed through as
/// printable text). Prevents `keystroke_to_bytes` from emitting the literal
/// strings "enter", "tab", etc. when no special-key match was found.
fn is_named_key(key: &str) -> bool {
  matches!(
    key,
    "tab"
      | "escape"
      | "enter"
      | "backspace"
      | "space"
      | "home"
      | "end"
      | "up"
      | "down"
      | "left"
      | "right"
      | "insert"
      | "delete"
      | "pageup"
      | "pagedown"
      | "f1"
      | "f2"
      | "f3"
      | "f4"
      | "f5"
      | "f6"
      | "f7"
      | "f8"
      | "f9"
      | "f10"
      | "f11"
      | "f12"
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use gpui::Modifiers;

  fn ks(key: &str) -> Keystroke {
    Keystroke {
      key: key.into(),
      modifiers: Modifiers::default(),
      key_char: None,
    }
  }

  fn ks_with(key: &str, mods: Modifiers) -> Keystroke {
    Keystroke {
      key: key.into(),
      modifiers: mods,
      key_char: None,
    }
  }

  fn ctrl() -> Modifiers {
    Modifiers {
      control: true,
      ..Modifiers::default()
    }
  }

  fn shift() -> Modifiers {
    Modifiers {
      shift: true,
      ..Modifiers::default()
    }
  }

  fn alt() -> Modifiers {
    Modifiers {
      alt: true,
      ..Modifiers::default()
    }
  }

  #[test]
  fn enter_emits_carriage_return() {
    assert_eq!(
      keystroke_to_bytes(&ks("enter"), TermMode::empty()),
      Some(b"\x0d".to_vec())
    );
  }

  #[test]
  fn backspace_emits_del() {
    assert_eq!(
      keystroke_to_bytes(&ks("backspace"), TermMode::empty()),
      Some(b"\x7f".to_vec())
    );
  }

  #[test]
  fn ctrl_c_emits_etx() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("c", ctrl()), TermMode::empty()),
      Some(b"\x03".to_vec())
    );
  }

  #[test]
  fn ctrl_d_emits_eot() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("d", ctrl()), TermMode::empty()),
      Some(b"\x04".to_vec())
    );
  }

  #[test]
  fn ctrl_letter_range_a_to_z() {
    for (i, c) in ('a'..='z').enumerate() {
      let key = c.to_string();
      let expected = vec![(i + 1) as u8];
      assert_eq!(
        keystroke_to_bytes(&ks_with(&key, ctrl()), TermMode::empty()),
        Some(expected),
        "ctrl-{key} should emit byte {expected_byte:#04x}",
        expected_byte = i + 1
      );
    }
  }

  #[test]
  fn arrow_up_normal_mode_emits_csi_a() {
    assert_eq!(
      keystroke_to_bytes(&ks("up"), TermMode::empty()),
      Some(b"\x1b[A".to_vec())
    );
  }

  #[test]
  fn arrow_up_app_cursor_mode_emits_ss3_a() {
    assert_eq!(
      keystroke_to_bytes(&ks("up"), TermMode::APP_CURSOR),
      Some(b"\x1bOA".to_vec())
    );
  }

  #[test]
  fn all_arrows_switch_with_app_cursor_mode() {
    let normal = TermMode::empty();
    let app = TermMode::APP_CURSOR;
    assert_eq!(
      keystroke_to_bytes(&ks("down"), normal),
      Some(b"\x1b[B".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("down"), app),
      Some(b"\x1bOB".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("right"), normal),
      Some(b"\x1b[C".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("right"), app),
      Some(b"\x1bOC".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("left"), normal),
      Some(b"\x1b[D".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("left"), app),
      Some(b"\x1bOD".to_vec())
    );
  }

  #[test]
  fn shift_tab_emits_back_tab() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("tab", shift()), TermMode::empty()),
      Some(b"\x1b[Z".to_vec())
    );
  }

  #[test]
  fn alt_enter_emits_meta_enter() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("enter", alt()), TermMode::empty()),
      Some(b"\x1b\x0d".to_vec())
    );
  }

  #[test]
  fn printable_letter_passes_through() {
    assert_eq!(
      keystroke_to_bytes(&ks("a"), TermMode::empty()),
      Some(b"a".to_vec())
    );
  }

  #[test]
  fn shift_letter_uses_key_char_when_present() {
    let mut k = ks_with("a", shift());
    k.key_char = Some("A".into());
    assert_eq!(
      keystroke_to_bytes(&k, TermMode::empty()),
      Some(b"A".to_vec())
    );
  }

  #[test]
  fn cmd_modifier_does_not_emit_anything() {
    // Cmd-arrow is reserved for app-level shortcuts (window/tab nav) and
    // must never reach the PTY.
    let mods = Modifiers {
      platform: true,
      ..Modifiers::default()
    };
    assert_eq!(
      keystroke_to_bytes(&ks_with("right", mods), TermMode::empty()),
      None
    );
  }

  #[test]
  fn f1_to_f4_emit_ss3_letters() {
    let cases = [("f1", "P"), ("f2", "Q"), ("f3", "R"), ("f4", "S")];
    for (key, letter) in cases {
      assert_eq!(
        keystroke_to_bytes(&ks(key), TermMode::empty()),
        Some(format!("\x1b{}{}", "O", letter).into_bytes()),
        "{key} should emit SS3 {letter}",
      );
    }
  }

  #[test]
  fn f5_to_f12_emit_csi_tilde_numbers() {
    let cases = [
      ("f5", 15),
      ("f6", 17),
      ("f7", 18),
      ("f8", 19),
      ("f9", 20),
      ("f10", 21),
      ("f11", 23),
      ("f12", 24),
    ];
    for (key, num) in cases {
      assert_eq!(
        keystroke_to_bytes(&ks(key), TermMode::empty()),
        Some(format!("\x1b[{num}~").into_bytes()),
        "{key} should emit \\x1b[{num}~",
      );
    }
  }

  #[test]
  fn shift_arrow_emits_csi_with_modifier_code_2() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("right", shift()), TermMode::empty()),
      Some(b"\x1b[1;2C".to_vec())
    );
  }

  #[test]
  fn ctrl_arrow_emits_csi_with_modifier_code_5() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("left", ctrl()), TermMode::empty()),
      Some(b"\x1b[1;5D".to_vec())
    );
  }

  #[test]
  fn ctrl_shift_arrow_emits_csi_with_modifier_code_6() {
    let mods = Modifiers {
      control: true,
      shift: true,
      ..Modifiers::default()
    };
    assert_eq!(
      keystroke_to_bytes(&ks_with("up", mods), TermMode::empty()),
      Some(b"\x1b[1;6A".to_vec())
    );
  }

  #[test]
  fn modified_arrow_ignores_app_cursor_mode() {
    // App-cursor SS3 form has no place to put a modifier code, so we
    // always fall back to the CSI form when modifiers are pressed.
    assert_eq!(
      keystroke_to_bytes(&ks_with("up", shift()), TermMode::APP_CURSOR),
      Some(b"\x1b[1;2A".to_vec())
    );
  }

  #[test]
  fn shift_pageup_emits_csi_with_modifier_code() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("pageup", shift()), TermMode::empty()),
      Some(b"\x1b[5;2~".to_vec())
    );
  }

  #[test]
  fn shift_f5_emits_csi_with_modifier_code() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("f5", shift()), TermMode::empty()),
      Some(b"\x1b[15;2~".to_vec())
    );
  }

  #[test]
  fn shift_f1_emits_csi_one_two_p() {
    assert_eq!(
      keystroke_to_bytes(&ks_with("f1", shift()), TermMode::empty()),
      Some(b"\x1b[1;2P".to_vec())
    );
  }

  #[test]
  fn delete_emits_csi_3_tilde() {
    assert_eq!(
      keystroke_to_bytes(&ks("delete"), TermMode::empty()),
      Some(b"\x1b[3~".to_vec())
    );
  }

  #[test]
  fn space_with_key_char_emits_space_byte() {
    // GPUI on macOS emits `key = "space"` with `key_char = Some(" ")`
    // when the spacebar is pressed unmodified. We must trust key_char
    // and not let `is_named_key("space")` swallow the byte.
    let mut k = ks("space");
    k.key_char = Some(" ".into());
    assert_eq!(
      keystroke_to_bytes(&k, TermMode::empty()),
      Some(b" ".to_vec())
    );
  }

  #[test]
  fn space_without_key_char_returns_none() {
    // Defensive: if no platform-provided char (test harness without
    // setting key_char), the named-key guard correctly suppresses
    // emitting the literal string "space".
    assert_eq!(keystroke_to_bytes(&ks("space"), TermMode::empty()), None);
  }

  #[test]
  fn pageup_pagedown_emit_csi_5_6_tilde() {
    assert_eq!(
      keystroke_to_bytes(&ks("pageup"), TermMode::empty()),
      Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
      keystroke_to_bytes(&ks("pagedown"), TermMode::empty()),
      Some(b"\x1b[6~".to_vec())
    );
  }
}
