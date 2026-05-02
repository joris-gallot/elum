//! GPUI view: focus, keyboard input, byte relay, window-resize sync, and
//! delegating actual paint to [`crate::element::TerminalElement`].
//!
//! Cell metrics now come from real font measurement inside the element, so
//! the view only needs an approximate divisor when computing how many
//! cells fit the current viewport. We use a reasonable estimate based on
//! `FONT_SIZE` and refine on every render - the exact glyph advance cell
//! width inside the element will round to the same integer cells.

use std::sync::Arc;

use gpui::{
  actions, div, px, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
  InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Point, Render,
  ScrollWheelEvent, Styled, Task, Window,
};

actions!(terminal, [Copy, Paste, SelectAll,]);

pub const KEY_CONTEXT: &str = "TerminalView";

#[derive(Debug)]
pub enum TerminalEvent {
  /// The remote shell exited (channel EOF/close received). The view's
  /// underlying SSH session is dead; the tab should be torn down.
  ShellClosed,
}

use crate::colors::{default_background, default_foreground};
use crate::element::TerminalElement;
use crate::keys::keystroke_to_bytes;
use crate::{GridSize, Terminal};

const FONT_SIZE: f32 = 13.0;
const FONT_FAMILY: &str = "Menlo";
const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Heuristic glyph advance for `FONT_FAMILY` at `FONT_SIZE`. Used only to
/// compute viewport → grid cell counts; the element itself uses real
/// font metrics for paint, so a small mismatch just rounds rows/cols.
const APPROX_CELL_WIDTH: f32 = 7.8;
const PADDING: f32 = 8.0;
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 5;

#[derive(Clone, Copy, Debug)]
pub struct Selection {
  pub anchor: (usize, usize),
  pub focus: (usize, usize),
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
  /// Sub-line accumulator for `ScrollWheelEvent` pixel deltas.
  scroll_px_acc: f32,
  selection: Option<Selection>,
  focus: FocusHandle,
  _relay: Task<()>,
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
        let term = term.clone();
        let _ = this.update(cx, move |_, cx| {
          term.write(&bytes);
          cx.notify();
        });
      }
      // Sender side dropped, the SSH relay task closed the channel
      let _ = this.update(cx, |_, cx| {
        cx.emit(TerminalEvent::ShellClosed);
      });
    });

    Self {
      terminal,
      to_remote,
      resize_remote,
      last_size: None,
      scroll_px_acc: 0.0,
      selection: None,
      focus,
      _relay: relay,
    }
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
      dragging: false,
    });
    cx.notify();
  }

  /// Pointer pressed inside the terminal area. `pos` is element-local
  /// (already shifted by the hitbox origin in [`crate::element`]).
  pub(crate) fn on_pointer_down(
    &mut self,
    pos: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    // Make sure keystrokes go to us right after a click, even if focus
    // was previously elsewhere (e.g. the sidebar).
    window.focus(&self.focus, cx);
    let cell = self.pixel_to_cell(pos);
    self.selection = Some(Selection {
      anchor: cell,
      focus: cell,
      dragging: true,
    });
    cx.notify();
  }

  pub(crate) fn on_pointer_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
    if !self.selection.is_some_and(|s| s.dragging) {
      return;
    }
    // Compute the cell first to release the immutable borrow on `self`
    // before mutating the selection.
    let cell = self.pixel_to_cell(pos);
    if let Some(sel) = self.selection.as_mut() {
      if cell != sel.focus {
        sel.focus = cell;
        cx.notify();
      }
    }
  }

  pub(crate) fn on_pointer_up(&mut self, _cx: &mut Context<Self>) {
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
  /// the cell grid origin) to a `(row, col)` display cell. The element
  /// uses real font metrics for paint, so a small mismatch with these
  /// approximate constants just rounds rows/cols.
  fn pixel_to_cell(&self, pos: Point<Pixels>) -> (usize, usize) {
    let x = f32::from(pos.x).max(0.0);
    let y = f32::from(pos.y).max(0.0);
    let line_h = FONT_SIZE * LINE_HEIGHT_RATIO;
    let col = (x / APPROX_CELL_WIDTH) as usize;
    let row = (y / line_h) as usize;
    // Clamp to the current grid so the user can't anchor outside it.
    let (rows, cols) = self.last_size.unwrap_or((24, 80));
    (
      row.min(rows.saturating_sub(1) as usize),
      col.min(cols.saturating_sub(1) as usize),
    )
  }

  /// Walk the current grid snapshot and gather the selected text, with
  /// trailing whitespace stripped per row and lines joined by `\n`.
  fn copy_selection_text(&self) -> Option<String> {
    let sel = self.selection?;
    let snapshot = self.terminal.snapshot_grid();
    let ((sr, sc), (er, ec)) = sel.normalized();
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

  fn handle_scroll_wheel(
    &mut self,
    ev: &ScrollWheelEvent,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) {
    // Approximate line height - actual paint metrics are computed
    // inside the element, but for scroll quantization a constant is
    // close enough. We accumulate fractional pixels so a slow trackpad
    // wheel still triggers eventual line steps.
    let line_height = px(FONT_SIZE * LINE_HEIGHT_RATIO);
    let pixel_y = ev.delta.pixel_delta(line_height).y;
    self.scroll_px_acc += f32::from(pixel_y);

    let line_h = f32::from(line_height);
    let lines = (self.scroll_px_acc / line_h) as i32;
    if lines != 0 {
      self.terminal.scroll_lines(lines);
      self.scroll_px_acc -= lines as f32 * line_h;
    }
  }

  /// Compute a grid size that fits the current viewport and propagate
  /// changes to both the alacritty model and the SSH PTY.
  fn sync_size_to_viewport(&mut self, window: &Window) {
    let viewport = window.viewport_size();
    let avail_w = (f32::from(viewport.width) - 2.0 * PADDING).max(0.0);
    let avail_h = (f32::from(viewport.height) - 2.0 * PADDING).max(0.0);
    let line_h = FONT_SIZE * LINE_HEIGHT_RATIO;
    let cols = ((avail_w / APPROX_CELL_WIDTH).floor() as u16).max(MIN_COLS);
    let rows = ((avail_h / line_h).floor() as u16).max(MIN_ROWS);
    let next = (rows, cols);

    if Some(next) != self.last_size {
      self.terminal.resize(GridSize::new(rows, cols));
      let _ = self.resize_remote.send((cols, rows));
      self.last_size = Some(next);
    }
  }
}

impl Focusable for TerminalView {
  fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
    self.focus.clone()
  }
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Render for TerminalView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    self.sync_size_to_viewport(window);

    div()
      .id("terminal-view")
      .key_context(KEY_CONTEXT)
      .track_focus(&self.focus)
      .on_action(cx.listener(Self::on_copy))
      .on_action(cx.listener(Self::on_paste))
      .on_action(cx.listener(Self::on_select_all))
      .on_key_down(cx.listener(Self::handle_key_down))
      .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
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
      ))
  }
}

#[cfg(test)]
mod selection_tests {
  use super::*;

  fn sel(anchor: (usize, usize), focus: (usize, usize)) -> Selection {
    Selection {
      anchor,
      focus,
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
