//! GPUI view: focus, keyboard input, byte relay, window-resize sync, and
//! delegating actual paint to [`crate::element::TerminalElement`].
//!
//! Cell metrics now come from real font measurement inside the element, so
//! the view only needs an approximate divisor when computing how many
//! cells fit the current viewport. We use a reasonable estimate based on
//! `FONT_SIZE` and refine on every render - the exact glyph advance cell
//! width inside the element will round to the same integer cells.

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event as AlacEvent, WindowSize};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use gpui::{
  actions, div, px, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
  InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, ParentElement, Pixels,
  Point, Render, ScrollWheelEvent, Styled, Subscription, Task, TouchPhase, Window,
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
use crate::{GridSize, Terminal};

const FONT_SIZE: f32 = 13.0;
const FONT_FAMILY: &str = "Menlo";
const LINE_HEIGHT_RATIO: f32 = 1.3;
const PADDING: f32 = 8.0;
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 5;

pub struct TerminalView {
  terminal: Arc<Terminal>,
  to_remote: flume::Sender<Vec<u8>>,
  resize_remote: flume::Sender<(u16, u16)>,
  last_size: Option<(u16, u16)>,
  last_cell_metrics: Option<(Pixels, Pixels)>,
  /// Sub-line accumulator for `ScrollWheelEvent` pixel deltas.
  scroll_px_acc: f32,
  /// Pointer is currently down with the left button and we're tracking
  /// drag updates. The selection itself lives in `term.selection`.
  dragging: bool,
  cursor_blink_phase: bool,
  /// Pre-edit text from the platform IME while a composition is in
  /// progress, `None` when no composition is active. Painted by the
  /// element with an underline overlay at the cursor position.
  marked_text: Option<String>,
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

    let blink = cx.spawn(async move |this, cx| loop {
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
    });

    Self {
      terminal,
      to_remote,
      resize_remote,
      last_size: None,
      last_cell_metrics: None,
      scroll_px_acc: 0.0,
      dragging: false,
      cursor_blink_phase: true,
      marked_text: None,
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

  /// Borrow the focus handle. The element needs it to register the
  /// platform [`gpui::InputHandler`] in its paint phase.
  pub(crate) fn focus(&self) -> &FocusHandle {
    &self.focus
  }

  /// Currently composing text from the IME (pre-edit), if any. The
  /// element paints this with an underline at the cursor.
  pub(crate) fn marked_text(&self) -> Option<&str> {
    self.marked_text.as_deref()
  }

  /// IME callback: replace the pre-edit text. An empty string clears
  /// the marked state. Notifies the view so the element repaints.
  pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
    if text.is_empty() {
      self.clear_marked_text(cx);
      return;
    }
    self.marked_text = Some(text);
    cx.notify();
  }

  /// IME callback: drop any pre-edit state without committing.
  pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
    if self.marked_text.take().is_some() {
      cx.notify();
    }
  }

  /// IME callback: the user accepted a candidate, push the bytes
  /// into the PTY and bring the viewport back to the live tail. Marked
  /// state is dropped by the platform separately via `clear_marked_text`.
  pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
    if text.is_empty() {
      return;
    }
    self.terminal.scroll_to_bottom();
    self.terminal.clear_selection();
    let _ = self.to_remote.send(text.as_bytes().to_vec());
    cx.notify();
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
      self.terminal.clear_selection();
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
    self.terminal.clear_selection();
    let _ = self.to_remote.send(bytes);
    cx.notify();
  }

  fn select_all(&mut self, cx: &mut Context<Self>) {
    self.terminal.select_all();
    cx.notify();
  }

  /// Pointer pressed inside the terminal area. `pos` is element-local.
  ///
  /// Selection variants are driven by alacritty's own `SelectionType`:
  /// - `click_count == 1`, no shift: `Simple` selection anchored at the
  ///   click. Drag extends focus.
  /// - `click_count == 1`, shift held: extend the existing selection's
  ///   focus to the click point. Otherwise behaves like a fresh click.
  /// - `click_count == 2`: `Semantic` selection (word boundaries from
  ///   alacritty's `Term::semantic_search_*`). Drag extends by whole
  ///   words.
  /// - `click_count >= 3`: `Lines` selection. Drag extends by whole lines.
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
    let (point, side) = self.pixel_to_point_and_side(pos);
    let cell = mouse_cell((
      (point.line.0 + self.terminal.display_offset() as i32).max(0) as usize,
      point.column.0,
    ));
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_button_report(cell, button, modifiers, true, mode) {
      self.terminal.clear_selection();
      self.terminal.scroll_to_bottom();
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      self.terminal.clear_selection();
      cx.notify();
      return;
    }

    if button != MouseButton::Left {
      return;
    }

    match click_count {
      0 | 1 if modifiers.shift && self.terminal.has_selection() => {
        // Extend the existing selection's focus to the new point.
        // Anchor/type are preserved.
        self.terminal.update_selection(point, side);
        self.dragging = false;
      }
      0 | 1 => {
        self
          .terminal
          .start_selection(SelectionType::Simple, point, side);
        self.dragging = true;
      }
      2 => {
        self
          .terminal
          .start_selection(SelectionType::Semantic, point, side);
        self.dragging = true;
      }
      _ => {
        self
          .terminal
          .start_selection(SelectionType::Lines, point, side);
        self.dragging = true;
      }
    }
    cx.notify();
  }

  pub(crate) fn on_pointer_move(
    &mut self,
    pos: Point<Pixels>,
    pressed_button: Option<MouseButton>,
    modifiers: Modifiers,
    cx: &mut Context<Self>,
  ) {
    let (point, side) = self.pixel_to_point_and_side(pos);
    let cell = mouse_cell((
      (point.line.0 + self.terminal.display_offset() as i32).max(0) as usize,
      point.column.0,
    ));
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_move_report(cell, pressed_button, modifiers, mode) {
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      return;
    }

    if !self.dragging {
      return;
    }
    self.terminal.update_selection(point, side);
    cx.notify();
  }

  pub(crate) fn on_pointer_up(
    &mut self,
    pos: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
    cx: &mut Context<Self>,
  ) {
    let (point, _side) = self.pixel_to_point_and_side(pos);
    let cell = mouse_cell((
      (point.line.0 + self.terminal.display_offset() as i32).max(0) as usize,
      point.column.0,
    ));
    let mode = self.terminal.with_term(|term| *term.mode());

    if let Some(bytes) = mouse_button_report(cell, button, modifiers, false, mode) {
      self.terminal.clear_selection();
      let _ = self.to_remote.send(bytes);
      cx.notify();
      return;
    }

    if mouse_reporting_enabled(mode) {
      self.terminal.clear_selection();
      cx.notify();
      return;
    }

    if button != MouseButton::Left {
      return;
    }

    self.dragging = false;
    // A bare click (no drag) collapses the selection to a zero-width
    // range; alacritty's `is_empty()` reports it. Clear so we don't
    // paint a phantom highlight on the click cell.
    let still_empty = self
      .terminal
      .with_term(|t| t.selection.as_ref().is_some_and(|s| s.is_empty()));
    if still_empty {
      self.terminal.clear_selection();
      cx.notify();
    }
  }

  /// Convert an element-local point to an absolute alacritty grid point
  /// plus the cell side (left/right) for half-cell precision. Uses the
  /// cell metrics measured during the most recent paint and the live
  /// scrollback offset so selections in scrollback stay anchored to
  /// their content.
  fn pixel_to_point_and_side(
    &self,
    pos: Point<Pixels>,
  ) -> (
    alacritty_terminal::index::Point,
    alacritty_terminal::index::Side,
  ) {
    let (cell_w, line_h) = self
      .last_cell_metrics
      .unwrap_or((px(FONT_SIZE * 0.6), px(FONT_SIZE * LINE_HEIGHT_RATIO)));
    let (rows, cols) = self.last_size.unwrap_or((24, 80));
    crate::mouse::pixel_to_point_and_side(
      pos,
      cell_w,
      line_h,
      cols as usize,
      rows as usize,
      self.terminal.display_offset(),
    )
  }

  pub(crate) fn on_scroll_wheel_at(
    &mut self,
    pos: Point<Pixels>,
    ev: &ScrollWheelEvent,
    cx: &mut Context<Self>,
  ) {
    // Use the real measured line height when available; fall back to the
    // FONT_SIZE-based heuristic only on the first frame.
    let line_height = self
      .last_cell_metrics
      .map_or_else(|| px(FONT_SIZE * LINE_HEIGHT_RATIO), |(_, h)| h);

    // Reset the residual at the start of a fresh trackpad gesture. Without
    // this, leftover sub-line offset (up to ~1 line) from the previous
    // gesture would jump the viewport on the first event of the next.
    if matches!(ev.touch_phase, TouchPhase::Started) {
      self.scroll_px_acc = 0.0;
    }

    let pixel_y = ev.delta.pixel_delta(line_height).y;
    self.scroll_px_acc += f32::from(pixel_y);

    let line_h = f32::from(line_height);
    let lines = (self.scroll_px_acc / line_h) as i32;
    self.scroll_px_acc -= lines as f32 * line_h;

    let mode = self.terminal.with_term(|term| *term.mode());

    if lines != 0 {
      let (point, _) = self.pixel_to_point_and_side(pos);
      let cell = mouse_cell((
        (point.line.0 + self.terminal.display_offset() as i32).max(0) as usize,
        point.column.0,
      ));
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
        // Mouse-mode reporting: the remote owns the viewport. Don't
        // carry a sub-line offset that would visually shift the grid.
        self.scroll_px_acc = 0.0;
      } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        // Alt-screen apps (vim, htop, less, man, top, …) keep their own
        // viewport; the local scrollback is empty there. When the remote
        // also opted into ALTERNATE_SCROLL, translate wheel deltas into
        // up/down arrow keystrokes so the app scrolls naturally.
        let bytes = alt_scroll(lines, mode.contains(TermMode::APP_CURSOR));
        let _ = self.to_remote.send(bytes);
        // Same rationale as mouse-mode: remote app redraws in place.
        self.scroll_px_acc = 0.0;
      } else {
        self.terminal.scroll_lines(lines);
      }
    }

    // Edge clamp for normal scrollback: at the top of history a positive
    // residual would peek empty space above; at the live bottom a negative
    // residual would peek empty space below. Drop the residual at the edge
    // so the viewport rests cleanly on the grid.
    if !self.is_remote_owned_scroll(mode) {
      let display_offset = self.terminal.display_offset();
      let history_size = self.terminal.history_size();
      if (self.scroll_px_acc > 0.0 && display_offset >= history_size)
        || (self.scroll_px_acc < 0.0 && display_offset == 0)
      {
        self.scroll_px_acc = 0.0;
      }
    }

    cx.notify();
  }

  fn is_remote_owned_scroll(&self, mode: TermMode) -> bool {
    crate::mouse::mouse_reporting_enabled(mode)
      || mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
  }

  /// Sub-line vertical paint offset, in pixels. Positive shifts the grid
  /// downward (history peeking from top); negative shifts upward. Always
  /// in `(-line_h, line_h)` and zero outside normal scrollback mode.
  pub(crate) fn scroll_offset_y(&self) -> f32 {
    self.scroll_px_acc
  }

  /// Materialize the live selection as plain text. Delegates to
  /// alacritty's `selection_to_string`, which already handles wide
  /// chars, wrapped lines, and semantic/line-mode rules correctly.
  fn copy_selection_text(&self) -> Option<String> {
    self.terminal.selection_text()
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
mod view_tests {
  use super::*;
  use gpui::{AppContext as _, Bounds, Entity, Size, TestAppContext};

  #[derive(Debug, PartialEq, Eq)]
  enum Captured {
    ShellClosed,
    Bell,
    TitleChanged(Option<String>),
  }

  /// Test rig: a `TerminalView` plus the channel ends not owned by it,
  /// so tests can both push input bytes and assert on outgoing PTY writes
  /// / resize signals.
  struct Rig {
    terminal: Arc<Terminal>,
    view: Entity<TerminalView>,
    from_remote_tx: flume::Sender<Vec<u8>>,
    to_remote_rx: flume::Receiver<Vec<u8>>,
    resize_rx: flume::Receiver<(u16, u16)>,
  }

  fn make_view(cx: &mut TestAppContext) -> Rig {
    let terminal = Arc::new(Terminal::new(GridSize::new(24, 80)));
    let (from_remote_tx, from_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (to_remote_tx, to_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (resize_tx, resize_rx) = flume::unbounded::<(u16, u16)>();

    let view = cx.new(|cx| {
      TerminalView::new(
        terminal.clone(),
        from_remote_rx,
        to_remote_tx,
        resize_tx,
        (),
        cx,
      )
    });

    Rig {
      terminal,
      view,
      from_remote_tx,
      to_remote_rx,
      resize_rx,
    }
  }

  /// Tiny entity that subscribes to the view's event stream and records
  /// what it sees, so individual tests can match against a captured list.
  struct Recorder {
    events: Vec<Captured>,
  }

  fn record(rig: &Rig, cx: &mut TestAppContext) -> Entity<Recorder> {
    let view = rig.view.clone();
    cx.new(|cx| {
      cx.subscribe(&view, |this: &mut Recorder, _, ev: &TerminalEvent, _| {
        let captured = match ev {
          TerminalEvent::ShellClosed => Captured::ShellClosed,
          TerminalEvent::Bell => Captured::Bell,
          TerminalEvent::TitleChanged(t) => Captured::TitleChanged(t.clone()),
        };
        this.events.push(captured);
      })
      .detach();
      Recorder { events: Vec::new() }
    })
  }

  fn first_row_text(terminal: &Terminal) -> String {
    let snap = terminal.snapshot_grid();
    snap.rows[0]
      .iter()
      .map(|c| c.c)
      .collect::<String>()
      .trim_end()
      .to_string()
  }

  #[gpui::test]
  async fn relay_pushes_bytes_into_grid(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    rig.from_remote_tx.send(b"hello".to_vec()).unwrap();
    cx.run_until_parked();
    assert_eq!(first_row_text(&rig.terminal), "hello");
  }

  #[gpui::test]
  async fn bell_byte_emits_bell_event(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    let recorder = record(&rig, cx);

    rig.from_remote_tx.send(vec![0x07]).unwrap();
    cx.run_until_parked();

    recorder.read_with(cx, |r, _| {
      assert!(
        r.events.contains(&Captured::Bell),
        "expected Bell, got {:?}",
        r.events
      );
    });
  }

  #[gpui::test]
  async fn relay_sender_drop_emits_shell_closed(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    let recorder = record(&rig, cx);

    // Dropping the only remote-side sender ends the relay loop and
    // surfaces ShellClosed to subscribers (the workspace uses this to
    // tear the tab down).
    drop(rig.from_remote_tx);
    cx.run_until_parked();

    recorder.read_with(cx, |r, _| {
      assert!(
        r.events.contains(&Captured::ShellClosed),
        "expected ShellClosed, got {:?}",
        r.events
      );
    });
  }

  #[gpui::test]
  async fn osc_title_emits_title_changed(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    let recorder = record(&rig, cx);

    // OSC 0 sets both the icon name and window title. `\x07` (BEL)
    // terminates the sequence on most terminals; alacritty parses it.
    rig
      .from_remote_tx
      .send(b"\x1b]0;mytitle\x07".to_vec())
      .unwrap();
    cx.run_until_parked();

    recorder.read_with(cx, |r, _| {
      let matched = r.events.iter().any(|e| match e {
        Captured::TitleChanged(Some(t)) => t == "mytitle",
        _ => false,
      });
      assert!(
        matched,
        "expected TitleChanged(\"mytitle\"), got {:?}",
        r.events
      );
    });
  }

  #[gpui::test]
  async fn sync_metrics_resizes_grid_and_pty(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    // 400px / 8px = 50 columns. 160px / 16px = 10 rows.
    let bounds = Bounds::new(Point::new(px(0.), px(0.)), Size::new(px(400.), px(160.)));

    rig.view.update(cx, |v: &mut TerminalView, _| {
      v.sync_metrics(px(8.), px(16.), bounds);
    });

    let (rows, cols) = rig.terminal.with_term(|t| {
      use alacritty_terminal::grid::Dimensions;
      (t.screen_lines(), t.columns())
    });
    assert_eq!(rows, 10);
    assert_eq!(cols, 50);

    let resize = rig.resize_rx.try_recv().expect("resize forwarded to PTY");
    assert_eq!(resize, (50, 10));
  }

  #[gpui::test]
  async fn ime_set_marked_text_records_pre_edit(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    rig.view.update(cx, |v: &mut TerminalView, cx| {
      v.set_marked_text("ni".into(), cx);
    });
    rig.view.read_with(cx, |v, _| {
      assert_eq!(v.marked_text(), Some("ni"));
    });
  }

  #[gpui::test]
  async fn ime_set_marked_text_with_empty_clears(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    rig.view.update(cx, |v: &mut TerminalView, cx| {
      v.set_marked_text("ni".into(), cx);
      // Platform sometimes signals "stop composing" with an empty
      // string; that should drop the marked state, not paint "" on
      // top of the grid.
      v.set_marked_text(String::new(), cx);
    });
    rig.view.read_with(cx, |v, _| {
      assert_eq!(v.marked_text(), None);
    });
  }

  #[gpui::test]
  async fn ime_commit_text_sends_bytes_to_pty(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    rig.view.update(cx, |v: &mut TerminalView, cx| {
      // Simulate the platform calling `replace_text_in_range` after the
      // user picks a candidate from the IME popup.
      v.set_marked_text("ni hao".into(), cx);
      v.clear_marked_text(cx);
      v.commit_text("你好", cx);
    });

    rig.view.read_with(cx, |v, _| {
      assert_eq!(v.marked_text(), None);
    });
    let bytes = rig
      .to_remote_rx
      .try_recv()
      .expect("commit forwarded to PTY");
    assert_eq!(bytes, "你好".as_bytes());
  }

  #[gpui::test]
  async fn ime_commit_empty_text_is_a_noop(cx: &mut TestAppContext) {
    let rig = make_view(cx);
    rig.view.update(cx, |v: &mut TerminalView, cx| {
      v.commit_text("", cx);
    });
    assert!(
      rig.to_remote_rx.try_recv().is_err(),
      "empty commit must not write to the PTY"
    );
  }
}
