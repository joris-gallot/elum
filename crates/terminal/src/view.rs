//! GPUI view: focus, keyboard input, byte relay, window-resize sync, and
//! delegating actual paint to [`crate::element::TerminalElement`].
//!
//! Cell metrics now come from real font measurement inside the element, so
//! the view only needs an approximate divisor when computing how many
//! cells fit the current viewport. We use a reasonable estimate based on
//! `FONT_SIZE` and refine on every render - the exact glyph advance cell
//! width inside the element will round to the same integer cells.

use std::sync::Arc;

use alacritty_terminal::event::{Event as AlacEvent, WindowSize};
use gpui::{
  actions, div, px, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
  InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, ParentElement, Pixels,
  Point, Render, ScrollWheelEvent, Styled, Subscription, Task, Window,
};

actions!(terminal, [Copy, Paste, SelectAll,]);

pub const KEY_CONTEXT: &str = "TerminalView";

#[derive(Debug)]
pub enum TerminalEvent {
  /// The remote shell exited (channel EOF/close received)
  ShellClosed,
  /// The remote rang the bell (received `\x07`)
  Bell,
  /// The remote changed or reset the terminal title.
  TitleChanged(Option<String>),
}

use crate::colors::{default_background, default_foreground};
use crate::element::TerminalElement;
use crate::keys::keystroke_to_bytes;
use crate::mouse::{
  alt_scroll, mouse_button_report, mouse_move_report, mouse_reporting_enabled, scroll_report,
  MouseCell, ScrollDirection,
};
use crate::{GridSize, SelectionKind, Terminal};

const FONT_SIZE: f32 = 13.0;
const FONT_FAMILY: &str = "Menlo";
const LINE_HEIGHT_RATIO: f32 = 1.3;
const PADDING: f32 = 8.0;
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
  Cell,
  Word,
  Line,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
  pub anchor: (usize, usize),
  pub focus: (usize, usize),
  pub origin: (usize, usize),
  pub mode: SelectionMode,
  pub dragging: bool,
}

impl Selection {
  /// Return `(start, end)` ordered so `start <= end` in line-then-column
  /// reading order. The end column is exclusive.
  pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
    if self.anchor <= self.focus {
      (self.anchor, self.focus)
    } else {
      (self.focus, self.anchor)
    }
  }

  /// Inclusive of the start cell, exclusive of the end cell - matches
  /// how text editors typically render a drag selection.
  pub fn contains(&self, row: usize, col: usize) -> bool {
    let ((sr, sc), (er, ec)) = self.normalized();
    if row < sr || row > er {
      return false;
    }
    if sr == er {
      return col >= sc && col < ec;
    }
    if row == sr {
      return col >= sc;
    }
    if row == er {
      return col < ec;
    }
    true
  }
}

pub struct TerminalView {
  terminal: Arc<Terminal>,
  to_remote: flume::Sender<Vec<u8>>,
  resize_remote: flume::Sender<(u16, u16)>,
  last_size: Option<(u16, u16)>,
  /// Real cell metrics measured by the element on the most recent paint.
  /// `None` only before the first frame; `pixel_to_cell` falls back to a
  /// heuristic in that brief window.
  last_cell_metrics: Option<(Pixels, Pixels)>,
  /// Sub-line accumulator for `ScrollWheelEvent` pixel deltas.
  scroll_px_acc: f32,
  selection: Option<Selection>,
  cursor_blink_phase: bool,
  focus: FocusHandle,
  _focus_in: Option<Subscription>,
  _focus_out: Option<Subscription>,
  _relay: Task<()>,
  _blink: Task<()>,
}

impl TerminalView {
  pub fn new<K: Send + 'static>(
    terminal: Arc<Terminal>,
    from_remote: flume::Receiver<Vec<u8>>,
    to_remote: flume::Sender<Vec<u8>>,
    resize_remote: flume::Sender<(u16, u16)>,
    keepalive: K,
    cx: &mut Context<Self>,
  ) -> Self {
    let term = terminal.clone();
    let focus = cx.focus_handle();

    let relay = cx.spawn(async move |this, cx| {
      let _keepalive = keepalive;
      while let Ok(first) = from_remote.recv_async().await {
        let mut bytes = first;
        while let Ok(more) = from_remote.try_recv() {
          bytes.extend_from_slice(&more);
        }
        let term_for_update = term.clone();
        let _ = this.update(cx, move |this, cx| {
          term_for_update.write_remote(&bytes);
          // Surface alacritty-side events and forward protocol responses
          // back to the remote shell in the same order alacritty emitted them.
          for event in term_for_update.drain_events() {
            this.dispatch_alac_event(event, cx);
          }
          cx.notify();
        });
      }
      // Sender side dropped, the SSH relay task closed the channel
      let _ = this.update(cx, |_, cx| {
        cx.emit(TerminalEvent::ShellClosed);
      });
    });

    let blink = cx.spawn(async move |this, cx| {
      use std::time::Duration;
      loop {
        cx.background_executor()
          .timer(Duration::from_millis(530))
          .await;
        let r = this.update(cx, |this, cx| {
          this.cursor_blink_phase = !this.cursor_blink_phase;
          cx.notify();
        });
        if r.is_err() {
          break;
        }
      }
    });

    Self {
      terminal,
      to_remote,
      resize_remote,
      last_size: None,
      last_cell_metrics: None,
      scroll_px_acc: 0.0,
      selection: None,
      cursor_blink_phase: true,
      focus,
      _focus_in: None,
      _focus_out: None,
      _relay: relay,
      _blink: blink,
    }
  }

  pub fn install_focus_handlers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self._focus_in.is_some() || self._focus_out.is_some() {
      return;
    }

    self._focus_in = Some(cx.on_focus_in(&self.focus, window, |this, _window, cx| {
      this.on_focus_changed(true, cx);
    }));
    self._focus_out = Some(
      cx.on_focus_out(&self.focus, window, |this, _event, _window, cx| {
        this.on_focus_changed(false, cx);
      }),
    );
  }

  pub fn selection(&self) -> Option<Selection> {
    self.selection
  }

  fn handle_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, _cx: &mut Context<Self>) {
    // Anything we got here is a keystroke that didn't match a bound
    // action - forward it to the remote shell. App shortcuts (Cmd-C,
    // Cmd-V, Cmd-A) are dispatched via `on_action` below and never
    // reach this path.
    let mode = self.terminal.with_term(|term| *term.mode());
    if let Some(bytes) = keystroke_to_bytes(&ev.keystroke, mode) {
      // Typing means engaging with the live shell - snap back to
      // bottom so input is visible, and clear any stale selection.
      self.terminal.scroll_to_bottom();
      self.selection = None;
      let _ = self.to_remote.send(bytes);
    }
  }

  fn on_focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
    let mode = self.terminal.with_term(|term| *term.mode());
    if let Some(bytes) = focus_report(focused, mode) {
      let _ = self.to_remote.send(bytes);
    }
    self.cursor_blink_phase = true;
    cx.notify();
  }

  fn on_copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
    if let Some(text) = self.copy_selection_text() {
      cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
  }

  fn on_paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
    self.paste_from_clipboard(cx);
  }

  fn on_select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
    self.select_all(cx);
  }

  fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
    use alacritty_terminal::term::TermMode;

    let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
      return;
    };
    if text.is_empty() {
      return;
    }

    let mode = self.terminal.with_term(|term| *term.mode());
    let bytes = if mode.contains(TermMode::BRACKETED_PASTE) {
      // The remote app (vim, less, shells with sane configs) is
      // signaling it wants pasted content delimited so it can skip
      // auto-indent / interpretation. Wrap the payload accordingly.
      let mut out = Vec::with_capacity(text.len() + 12);
      out.extend_from_slice(b"\x1b[200~");
      out.extend_from_slice(text.as_bytes());
      out.extend_from_slice(b"\x1b[201~");
      out
    } else {
      text.into_bytes()
    };

    self.terminal.scroll_to_bottom();
    self.selection = None;
    let _ = self.to_remote.send(bytes);
    cx.notify();
  }

  fn select_all(&mut self, cx: &mut Context<Self>) {
    let snapshot = self.terminal.snapshot_grid();
    if snapshot.rows.is_empty() {
      return;
    }
    let last_row = snapshot.rows.len() - 1;
    let cols = snapshot.rows[last_row].len();
    self.selection = Some(Selection {
      anchor: (0, 0),
      focus: (last_row, cols),
      origin: (0, 0),
      mode: SelectionMode::Cell,
      dragging: false,
    });
    cx.notify();
  }

  /// Pointer pressed inside the terminal area. `pos` is element-local
  ///
  /// Selection variants:
  /// - `click_count == 1` and no shift: start a fresh drag selection.
  /// - `click_count == 1` and shift: extend the existing selection's focus
  ///   to the clicked cell (or start a new one if none exists).
  /// - `click_count == 2`: select the word under the cursor.
  /// - `click_count >= 3`: select the entire line under the cursor.
  pub(crate) fn on_pointer_down(
    &mut self,
    pos: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
    click_count: usize,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    window.focus(&self.focus, cx);
    let cell = self.pixel_to_cell(pos);
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_button_report(mouse_cell(cell), button, modifiers, true, mode) {
      self.selection = None;
      self.terminal.scroll_to_bottom();
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      self.selection = None;
      cx.notify();
      return;
    }

    if button != MouseButton::Left {
      return;
    }

    self.selection = match click_count {
      0 | 1 if modifiers.shift => {
        let (anchor, origin) = self
          .selection
          .map_or((cell, cell), |s| (s.anchor, s.origin));
        Some(Selection {
          anchor,
          focus: cell,
          origin,
          mode: SelectionMode::Cell,
          dragging: false,
        })
      }
      0 | 1 => Some(Selection {
        anchor: cell,
        focus: cell,
        origin: cell,
        mode: SelectionMode::Cell,
        dragging: true,
      }),
      2 => self.word_selection_at(cell),
      _ => self.line_selection_at(cell),
    };
    cx.notify();
  }

  /// Word-mode selection seeded at `(row, col)`. Drag enabled so the user
  /// can extend by whole-word increments (handled in [`Self::on_pointer_move`]).
  fn word_selection_at(&self, cell: (usize, usize)) -> Option<Selection> {
    let snapshot = self.terminal.snapshot_grid();
    let (anchor, focus) = word_bounds(&snapshot, cell)?;
    Some(Selection {
      anchor,
      focus,
      origin: cell,
      mode: SelectionMode::Word,
      dragging: true,
    })
  }

  /// Line-mode selection seeded at `(row, _)`. Drag enabled so the user
  /// can extend across whole lines.
  fn line_selection_at(&self, cell: (usize, usize)) -> Option<Selection> {
    let snapshot = self.terminal.snapshot_grid();
    let (anchor, focus) = line_bounds(&snapshot, cell)?;
    Some(Selection {
      anchor,
      focus,
      origin: cell,
      mode: SelectionMode::Line,
      dragging: true,
    })
  }

  pub(crate) fn on_pointer_move(
    &mut self,
    pos: Point<Pixels>,
    pressed_button: Option<MouseButton>,
    modifiers: Modifiers,
    cx: &mut Context<Self>,
  ) {
    let pointer = self.pixel_to_cell(pos);
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_move_report(mouse_cell(pointer), pressed_button, modifiers, mode) {
      self.selection = None;
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      self.selection = None;
      return;
    }

    let Some(sel) = self.selection else {
      return;
    };
    if !sel.dragging {
      return;
    }

    // For Word/Line modes we need a fresh snapshot to recompute snapped
    // bounds. For Cell mode, the pointer cell is the focus.
    let new = match sel.mode {
      SelectionMode::Cell => Selection {
        anchor: sel.anchor,
        focus: pointer,
        origin: sel.origin,
        mode: SelectionMode::Cell,
        dragging: true,
      },
      SelectionMode::Word => {
        let snapshot = self.terminal.snapshot_grid();
        let (origin_start, origin_end) =
          word_bounds(&snapshot, sel.origin).unwrap_or((sel.origin, sel.origin));
        let (pointer_start, pointer_end) =
          word_bounds(&snapshot, pointer).unwrap_or((pointer, pointer));
        let (anchor, focus) = if pointer >= sel.origin {
          (origin_start, pointer_end)
        } else {
          (pointer_start, origin_end)
        };
        Selection {
          anchor,
          focus,
          origin: sel.origin,
          mode: SelectionMode::Word,
          dragging: true,
        }
      }
      SelectionMode::Line => {
        let snapshot = self.terminal.snapshot_grid();
        let (origin_start, origin_end) =
          line_bounds(&snapshot, sel.origin).unwrap_or((sel.origin, sel.origin));
        let (pointer_start, pointer_end) =
          line_bounds(&snapshot, pointer).unwrap_or((pointer, pointer));
        let (anchor, focus) = if pointer.0 >= sel.origin.0 {
          (origin_start, pointer_end)
        } else {
          (pointer_start, origin_end)
        };
        Selection {
          anchor,
          focus,
          origin: sel.origin,
          mode: SelectionMode::Line,
          dragging: true,
        }
      }
    };

    if (new.anchor, new.focus) != (sel.anchor, sel.focus) {
      self.selection = Some(new);
      cx.notify();
    }
  }

  pub(crate) fn on_pointer_up(
    &mut self,
    pos: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
    cx: &mut Context<Self>,
  ) {
    let cell = self.pixel_to_cell(pos);
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_button_report(mouse_cell(cell), button, modifiers, false, mode) {
      self.selection = None;
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      self.selection = None;
      cx.notify();
      return;
    }

    if button != MouseButton::Left {
      return;
    }

    if let Some(sel) = self.selection.as_mut() {
      sel.dragging = false;
      // If the user merely clicked (no drag), the anchor and focus
      // are equal - there's nothing selected. Clear the state so we
      // don't paint a phantom highlight.
      if sel.anchor == sel.focus {
        self.selection = None;
      }
    }
  }

  /// Convert an element-local point (origin at the hitbox top-left, i.e.
  /// the cell grid origin) to a `(row, col)` display cell, using the cell
  /// metrics measured during the most recent paint. Falls back to a
  /// crude heuristic only if no paint has happened yet (clicks landing
  /// before the first frame are vanishingly rare in practice).
  fn pixel_to_cell(&self, pos: Point<Pixels>) -> (usize, usize) {
    let (cell_w, line_h) = self.last_cell_metrics.map_or(
      (FONT_SIZE * 0.6, FONT_SIZE * LINE_HEIGHT_RATIO),
      |(w, h)| (f32::from(w), f32::from(h)),
    );
    let x = f32::from(pos.x).max(0.0);
    let y = f32::from(pos.y).max(0.0);
    let col = (x / cell_w) as usize;
    let row = (y / line_h) as usize;
    // Clamp to the current grid so the user can't anchor outside it.
    let (rows, cols) = self.last_size.unwrap_or((24, 80));
    (
      row.min(rows.saturating_sub(1) as usize),
      col.min(cols.saturating_sub(1) as usize),
    )
  }

  pub(crate) fn on_scroll_wheel_at(
    &mut self,
    pos: Point<Pixels>,
    ev: &ScrollWheelEvent,
    cx: &mut Context<Self>,
  ) {
    use alacritty_terminal::term::TermMode;

    // Use the real measured line height when available; fall back to the
    // FONT_SIZE-based heuristic only on the first frame.
    let line_height = self
      .last_cell_metrics
      .map_or_else(|| px(FONT_SIZE * LINE_HEIGHT_RATIO), |(_, h)| h);
    let pixel_y = ev.delta.pixel_delta(line_height).y;
    self.scroll_px_acc += f32::from(pixel_y);

    let line_h = f32::from(line_height);
    let lines = (self.scroll_px_acc / line_h) as i32;
    if lines == 0 {
      return;
    }
    self.scroll_px_acc -= lines as f32 * line_h;

    let mode = self.terminal.with_term(|term| *term.mode());
    let cell = mouse_cell(self.pixel_to_cell(pos));
    let direction = if lines.is_positive() {
      ScrollDirection::Up
    } else {
      ScrollDirection::Down
    };

    if let Some(report) = scroll_report(cell, direction, ev.modifiers, mode) {
      let count = lines.unsigned_abs() as usize;
      let mut bytes = Vec::with_capacity(report.len() * count);
      for _ in 0..count {
        bytes.extend_from_slice(&report);
      }
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    // Alt-screen apps (vim, htop, less, man, top, …) keep their own
    // viewport; the local scrollback is empty there. When the remote
    // also opted into ALTERNATE_SCROLL, translate wheel deltas into
    // up/down arrow keystrokes so the app scrolls naturally.
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
      let bytes = alt_scroll(lines, mode.contains(TermMode::APP_CURSOR));
      let _ = self.to_remote.send(bytes);
    } else {
      self.terminal.scroll_lines(lines);
    }
  }

  /// Walk the current grid snapshot and gather the selected text, with
  /// trailing whitespace stripped per row and lines joined by `\n`.
  fn copy_selection_text(&self) -> Option<String> {
    let sel = self.selection?;
    let (start, end) = sel.normalized();
    let kind = match sel.mode {
      SelectionMode::Cell => SelectionKind::Cell,
      SelectionMode::Word => SelectionKind::Word,
      SelectionMode::Line => SelectionKind::Line,
    };
    if let Some(text) = self.terminal.selected_text(start, end, kind) {
      return Some(text);
    }

    let snapshot = self.terminal.snapshot_grid();
    let ((sr, sc), (er, ec)) = (start, end);
    let last_row = er.min(snapshot.rows.len().saturating_sub(1));
    let mut out = String::new();
    for row_idx in sr..=last_row {
      let row = &snapshot.rows[row_idx];
      let start = if row_idx == sr { sc } else { 0 };
      let end = if row_idx == er { ec } else { row.len() }.min(row.len());
      let mut line: String = row
        .get(start..end)
        .map(|slice| slice.iter().map(|c| c.c).collect())
        .unwrap_or_default();
      // Terminals pad with trailing spaces; strip them per line so
      // copied text doesn't carry junk into pastes.
      let trimmed_len = line.trim_end_matches(' ').len();
      line.truncate(trimmed_len);
      out.push_str(&line);
      if row_idx < last_row {
        out.push('\n');
      }
    }
    if out.is_empty() {
      None
    } else {
      Some(out)
    }
  }

  /// Called by [`crate::element::TerminalElement`] every prepaint with the
  /// real bounds of the terminal area and the real per-cell metrics from
  /// the cascaded font. Owns the resize policy: clamps to `MIN_COLS`/
  /// `MIN_ROWS`, resizes alacritty's grid, and forwards the new size to
  /// the SSH PTY when it actually changed.
  pub(crate) fn sync_metrics(
    &mut self,
    cell_width: Pixels,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
  ) {
    self.last_cell_metrics = Some((cell_width, line_height));

    let cell_w = f32::from(cell_width);
    let line_h = f32::from(line_height);
    if cell_w <= 0.0 || line_h <= 0.0 {
      return;
    }
    let cols = ((f32::from(bounds.size.width) / cell_w).floor() as u16).max(MIN_COLS);
    let rows = ((f32::from(bounds.size.height) / line_h).floor() as u16).max(MIN_ROWS);
    let next = (rows, cols);

    if Some(next) != self.last_size {
      self.terminal.resize(GridSize::new(rows, cols));
      let _ = self.resize_remote.send((cols, rows));
      self.last_size = Some(next);
    }
  }

  fn current_window_size(&self) -> WindowSize {
    let (rows, cols) = self.last_size.unwrap_or((24, 80));
    let (cell_width, cell_height) = self
      .last_cell_metrics
      .map_or((8, 17), |(w, h)| (pixels_to_u16(w), pixels_to_u16(h)));

    WindowSize {
      num_lines: rows,
      num_cols: cols,
      cell_width,
      cell_height,
    }
  }

  fn dispatch_alac_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
    let clipboard_text = if matches!(event, AlacEvent::ClipboardLoad(_, _)) {
      cx.read_from_clipboard().and_then(|item| item.text())
    } else {
      None
    };

    let effects = alacritty_event_effects(
      event,
      self.current_window_size(),
      clipboard_text.as_deref(),
      |index| self.terminal.color_rgb(index),
    );

    for bytes in effects.remote_writes {
      let _ = self.to_remote.send(bytes);
    }

    if let Some(text) = effects.clipboard_store {
      cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    if let Some(title) = effects.title {
      cx.emit(TerminalEvent::TitleChanged(title));
    }

    if effects.cursor_blinking_changed {
      self.cursor_blink_phase = true;
    }

    if effects.wakeup {
      cx.notify();
    }

    if effects.bell {
      cx.emit(TerminalEvent::Bell);
    }

    if effects.shell_closed {
      cx.emit(TerminalEvent::ShellClosed);
    }
  }
}

impl Focusable for TerminalView {
  fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
    self.focus.clone()
  }
}

impl EventEmitter<TerminalEvent> for TerminalView {}

#[derive(Debug, Default, PartialEq, Eq)]
struct AlacrittyEventEffects {
  remote_writes: Vec<Vec<u8>>,
  clipboard_store: Option<String>,
  title: Option<Option<String>>,
  bell: bool,
  shell_closed: bool,
  wakeup: bool,
  cursor_blinking_changed: bool,
}

fn alacritty_event_effects(
  event: AlacEvent,
  window_size: WindowSize,
  clipboard_text: Option<&str>,
  mut color_at: impl FnMut(usize) -> alacritty_terminal::vte::ansi::Rgb,
) -> AlacrittyEventEffects {
  let mut effects = AlacrittyEventEffects::default();

  match event {
    AlacEvent::PtyWrite(out) => {
      effects.remote_writes.push(out.into_bytes());
    }
    AlacEvent::TextAreaSizeRequest(format) => {
      effects.remote_writes.push(format(window_size).into_bytes());
    }
    AlacEvent::ColorRequest(index, format) => {
      effects
        .remote_writes
        .push(format(color_at(index)).into_bytes());
    }
    AlacEvent::ClipboardStore(_, data) => {
      effects.clipboard_store = Some(data);
    }
    AlacEvent::ClipboardLoad(_, format) => {
      effects
        .remote_writes
        .push(format(clipboard_text.unwrap_or("")).into_bytes());
    }
    AlacEvent::Title(title) => {
      effects.title = Some(Some(title));
    }
    AlacEvent::ResetTitle => {
      effects.title = Some(None);
    }
    AlacEvent::CursorBlinkingChange => {
      effects.cursor_blinking_changed = true;
    }
    AlacEvent::Wakeup => {
      effects.wakeup = true;
    }
    AlacEvent::Bell => {
      effects.bell = true;
    }
    AlacEvent::Exit | AlacEvent::ChildExit(_) => {
      effects.shell_closed = true;
    }
    AlacEvent::MouseCursorDirty => {}
  }

  effects
}

fn pixels_to_u16(value: Pixels) -> u16 {
  f32::from(value).round().clamp(0.0, u16::MAX as f32) as u16
}

fn mouse_cell((row, col): (usize, usize)) -> MouseCell {
  MouseCell::new(row, col)
}

fn focus_report(focused: bool, mode: alacritty_terminal::term::TermMode) -> Option<Vec<u8>> {
  if !mode.contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT) {
    return None;
  }
  Some(if focused {
    b"\x1b[I".to_vec()
  } else {
    b"\x1b[O".to_vec()
  })
}

/// Word-class predicate for double-click selection. Includes characters
/// commonly found in identifiers, paths, and URLs so a double-click on
/// `~/.config/foo.toml` selects the whole token rather than stopping at
/// each separator.
fn is_word_char(c: char) -> bool {
  c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | ':')
}

/// Word bounds (inclusive start, exclusive end) of the run containing
/// `(row, col)`. Whitespace cells return a single-cell range so the user
/// still sees feedback. `None` if the cell is out of grid range.
fn word_bounds(
  snapshot: &crate::GridSnapshot,
  (row, col): (usize, usize),
) -> Option<((usize, usize), (usize, usize))> {
  let row_cells = snapshot.rows.get(row)?;
  let center = row_cells.get(col)?;
  if !is_word_char(center.c) {
    return Some(((row, col), (row, col + 1)));
  }
  let mut start = col;
  while start > 0
    && row_cells
      .get(start - 1)
      .is_some_and(|cell| is_word_char(cell.c))
  {
    start -= 1;
  }
  let mut end = col;
  while end < row_cells.len() && is_word_char(row_cells[end].c) {
    end += 1;
  }
  Some(((row, start), (row, end)))
}

/// Line bounds (col 0 to row length) for the row of `(row, _)`. `None` if
/// the row is out of range.
fn line_bounds(
  snapshot: &crate::GridSnapshot,
  (row, _): (usize, usize),
) -> Option<((usize, usize), (usize, usize))> {
  let row_cells = snapshot.rows.get(row)?;
  Some(((row, 0), (row, row_cells.len())))
}

impl Render for TerminalView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let focused = self.focus.is_focused(window);
    let blink_phase = self.cursor_blink_phase;

    div()
      .id("terminal-view")
      .key_context(KEY_CONTEXT)
      .track_focus(&self.focus)
      .on_action(cx.listener(Self::on_copy))
      .on_action(cx.listener(Self::on_paste))
      .on_action(cx.listener(Self::on_select_all))
      .on_key_down(cx.listener(Self::handle_key_down))
      .size_full()
      .bg(default_background())
      .text_color(default_foreground())
      .font_family(FONT_FAMILY)
      .text_size(px(FONT_SIZE))
      .p(px(PADDING))
      .child(TerminalElement::new(
        self.terminal.clone(),
        self.selection,
        cx.entity().downgrade(),
        focused,
        blink_phase,
      ))
  }
}

#[cfg(test)]
mod alacritty_event_tests {
  use super::*;
  use std::sync::Arc;

  use alacritty_terminal::term::ClipboardType;
  use alacritty_terminal::vte::ansi::Rgb;

  fn window_size() -> WindowSize {
    WindowSize {
      num_lines: 24,
      num_cols: 80,
      cell_width: 8,
      cell_height: 17,
    }
  }

  fn effects(event: AlacEvent, clipboard: Option<&str>) -> AlacrittyEventEffects {
    alacritty_event_effects(event, window_size(), clipboard, |index| {
      assert_eq!(index, 1);
      Rgb { r: 1, g: 2, b: 3 }
    })
  }

  #[test]
  fn pty_write_is_forwarded_to_remote() {
    let effects = effects(AlacEvent::PtyWrite("abc".into()), None);
    assert_eq!(effects.remote_writes, vec![b"abc".to_vec()]);
  }

  #[test]
  fn text_area_size_request_uses_current_grid_and_cell_size() {
    let event = AlacEvent::TextAreaSizeRequest(Arc::new(|size| {
      format!(
        "{}x{}@{}x{}",
        size.num_cols, size.num_lines, size.cell_width, size.cell_height
      )
    }));

    let effects = effects(event, None);
    assert_eq!(effects.remote_writes, vec![b"80x24@8x17".to_vec()]);
  }

  #[test]
  fn color_request_uses_terminal_color_callback() {
    let event = AlacEvent::ColorRequest(
      1,
      Arc::new(|rgb| format!("rgb:{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)),
    );

    let effects = effects(event, None);
    assert_eq!(effects.remote_writes, vec![b"rgb:010203".to_vec()]);
  }

  #[test]
  fn clipboard_load_formats_clipboard_text_for_remote() {
    let event = AlacEvent::ClipboardLoad(
      ClipboardType::Clipboard,
      Arc::new(|text| format!("clip:{text}")),
    );

    let effects = effects(event, Some("payload"));
    assert_eq!(effects.remote_writes, vec![b"clip:payload".to_vec()]);
  }

  #[test]
  fn clipboard_load_uses_empty_string_when_clipboard_is_missing() {
    let event = AlacEvent::ClipboardLoad(
      ClipboardType::Clipboard,
      Arc::new(|text| format!("clip:{text}")),
    );

    let effects = effects(event, None);
    assert_eq!(effects.remote_writes, vec![b"clip:".to_vec()]);
  }

  #[test]
  fn clipboard_store_records_text_for_ui_clipboard() {
    let effects = effects(
      AlacEvent::ClipboardStore(ClipboardType::Clipboard, "copied".into()),
      None,
    );
    assert_eq!(effects.clipboard_store, Some("copied".into()));
  }

  #[test]
  fn title_events_are_preserved() {
    let titled = effects(AlacEvent::Title("vim".into()), None);
    assert_eq!(titled.title, Some(Some("vim".into())));

    let reset = effects(AlacEvent::ResetTitle, None);
    assert_eq!(reset.title, Some(None));
  }

  #[test]
  fn bell_and_exit_surface_ui_events() {
    assert!(effects(AlacEvent::Bell, None).bell);
    assert!(effects(AlacEvent::Exit, None).shell_closed);
  }

  #[test]
  fn wakeup_marks_view_dirty_without_remote_write() {
    let effects = effects(AlacEvent::Wakeup, None);
    assert!(effects.wakeup);
    assert!(effects.remote_writes.is_empty());
  }
}

#[cfg(test)]
mod focus_report_tests {
  use super::*;
  use alacritty_terminal::term::TermMode;

  #[test]
  fn focus_report_is_none_when_mode_is_disabled() {
    assert_eq!(focus_report(true, TermMode::empty()), None);
    assert_eq!(focus_report(false, TermMode::empty()), None);
  }

  #[test]
  fn focus_report_emits_focus_in_when_enabled() {
    assert_eq!(
      focus_report(true, TermMode::FOCUS_IN_OUT),
      Some(b"\x1b[I".to_vec())
    );
  }

  #[test]
  fn focus_report_emits_focus_out_when_enabled() {
    assert_eq!(
      focus_report(false, TermMode::FOCUS_IN_OUT),
      Some(b"\x1b[O".to_vec())
    );
  }
}

#[cfg(test)]
mod selection_tests {
  use super::*;

  fn sel(anchor: (usize, usize), focus: (usize, usize)) -> Selection {
    Selection {
      anchor,
      focus,
      origin: anchor,
      mode: SelectionMode::Cell,
      dragging: false,
    }
  }

  #[test]
  fn normalized_orders_anchor_focus_by_reading_order() {
    let s = sel((5, 10), (3, 2));
    assert_eq!(s.normalized(), ((3, 2), (5, 10)));
  }

  #[test]
  fn contains_inclusive_start_exclusive_end_single_row() {
    let s = sel((0, 2), (0, 6));
    assert!(!s.contains(0, 1));
    assert!(s.contains(0, 2));
    assert!(s.contains(0, 5));
    assert!(!s.contains(0, 6));
  }

  #[test]
  fn contains_first_row_starts_at_anchor_col() {
    let s = sel((1, 5), (3, 2));
    assert!(!s.contains(1, 4));
    assert!(s.contains(1, 5));
    assert!(s.contains(1, 100));
  }

  #[test]
  fn contains_last_row_stops_at_focus_col() {
    let s = sel((1, 5), (3, 2));
    assert!(s.contains(3, 0));
    assert!(s.contains(3, 1));
    assert!(!s.contains(3, 2));
  }

  #[test]
  fn contains_middle_rows_full_width() {
    let s = sel((1, 5), (3, 2));
    assert!(s.contains(2, 0));
    assert!(s.contains(2, 79));
  }

  #[test]
  fn contains_outside_row_range_is_false() {
    let s = sel((2, 0), (4, 0));
    assert!(!s.contains(1, 50));
    assert!(!s.contains(5, 0));
  }
}
