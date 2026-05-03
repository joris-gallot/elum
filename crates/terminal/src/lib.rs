//! Source-agnostic terminal data model wrapping `alacritty_terminal::Term`.

pub mod colors;
pub mod element;
pub mod keys;
pub mod mouse;
pub mod view;

use std::sync::Arc;

use alacritty_terminal::{
  event::{Event as AlacEvent, EventListener},
  grid::{Dimensions, Scroll},
  index::{Column, Line, Point as AlacPoint, Side},
  selection::{Selection as AlacSelection, SelectionType},
  term::cell::Flags,
  term::color,
  term::Config,
  term::TermMode,
  vte::ansi::{Processor, StdSyncHandler},
  Term,
};
use flume::{unbounded, Receiver, Sender};
use parking_lot::FairMutex;

/// Grid dimensions in cells.
#[derive(Debug, Clone, Copy)]
pub struct GridSize {
  pub rows: u16,
  pub cols: u16,
  pub scrollback: u16,
}

pub const DEFAULT_SCROLLBACK_LINES: u16 = 10_000;

impl GridSize {
  pub fn new(rows: u16, cols: u16) -> Self {
    Self {
      rows,
      cols,
      scrollback: DEFAULT_SCROLLBACK_LINES,
    }
  }
}

impl Dimensions for GridSize {
  fn columns(&self) -> usize {
    self.cols as usize
  }

  fn screen_lines(&self) -> usize {
    self.rows as usize
  }

  fn total_lines(&self) -> usize {
    self.rows as usize + self.scrollback as usize
  }
}

/// Forwards alacritty events through a `flume` channel.
pub struct ChannelListener(Sender<AlacEvent>);

impl EventListener for ChannelListener {
  fn send_event(&self, event: AlacEvent) {
    let _ = self.0.send(event);
  }
}

pub struct Terminal {
  term: Arc<FairMutex<Term<ChannelListener>>>,
  processor: FairMutex<Processor<StdSyncHandler>>,
  events: Receiver<AlacEvent>,
}

impl Terminal {
  pub fn new(size: GridSize) -> Self {
    let (events_tx, events_rx) = unbounded();
    let config = Config {
      scrolling_history: size.scrollback as usize,
      ..Config::default()
    };
    let term = Term::new(config, &size, ChannelListener(events_tx));
    Self {
      term: Arc::new(FairMutex::new(term)),
      processor: FairMutex::new(Processor::new()),
      events: events_rx,
    }
  }

  /// Feed raw PTY bytes verbatim - no newline rewriting, no escape filtering.
  pub fn write_remote(&self, bytes: &[u8]) {
    self.advance(bytes);
  }

  fn advance(&self, bytes: &[u8]) {
    let mut term = self.term.lock();
    let mut processor = self.processor.lock();
    processor.advance(&mut *term, bytes);
  }

  pub fn with_term<R>(&self, f: impl FnOnce(&Term<ChannelListener>) -> R) -> R {
    let term = self.term.lock();
    f(&term)
  }

  pub fn resize(&self, size: GridSize) {
    let mut term = self.term.lock();
    term.resize(size);
  }

  /// Positive `delta` scrolls up into history; negative pushes back toward live.
  pub fn scroll_lines(&self, delta: i32) {
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Delta(delta));
  }

  pub fn scroll_to_bottom(&self) {
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Bottom);
  }

  /// Drain queued alacritty events. Consumers forward bytes back to the SSH channel.
  pub fn drain_events(&self) -> Vec<AlacEvent> {
    self.events.try_iter().collect()
  }

  /// Lines scrolled into history; `0` = live output.
  pub fn display_offset(&self) -> usize {
    self.term.lock().grid().display_offset()
  }

  /// Scrollback lines retained above the live screen.
  pub fn history_size(&self) -> usize {
    self.term.lock().grid().history_size()
  }

  /// Start a fresh selection of the given kind anchored at `point` / `side`.
  /// Replaces any prior selection.
  pub fn start_selection(&self, ty: SelectionType, point: AlacPoint, side: Side) {
    let mut term = self.term.lock();
    term.selection = Some(AlacSelection::new(ty, point, side));
  }

  /// Drag the focus end of the live selection. No-op if no selection has
  /// been started.
  pub fn update_selection(&self, point: AlacPoint, side: Side) {
    let mut term = self.term.lock();
    if let Some(sel) = term.selection.as_mut() {
      sel.update(point, side);
    }
  }

  /// Drop the current selection. Idempotent.
  pub fn clear_selection(&self) {
    let mut term = self.term.lock();
    term.selection = None;
  }

  pub fn has_selection(&self) -> bool {
    self.term.lock().selection.is_some()
  }

  pub fn selection_text(&self) -> Option<String> {
    self.term.lock().selection_to_string()
  }

  /// Select scrollback + live screen, matching iTerm2 / Terminal.app's "Select All".
  pub fn select_all(&self) {
    let mut term = self.term.lock();
    let topmost = term.topmost_line();
    let bottommost = term.bottommost_line();
    let last_col = term.last_column();
    let mut sel = AlacSelection::new(
      SelectionType::Simple,
      AlacPoint::new(topmost, Column(0)),
      Side::Left,
    );
    sel.update(AlacPoint::new(bottommost, last_col), Side::Right);
    term.selection = Some(sel);
  }

  /// Runtime OSC override for a palette slot, `None` if the slot is at its theme default.
  pub fn osc_color_override(&self, index: usize) -> Option<alacritty_terminal::vte::ansi::Rgb> {
    if index >= color::COUNT {
      return None;
    }
    self.with_term(|term| term.colors()[index])
  }

  /// Snapshot the visible viewport (honors `display_offset` for scrollback).
  pub fn snapshot_grid(&self) -> GridSnapshot {
    self.with_term(|term| {
      let cols = term.columns();
      let rows = term.screen_lines();
      // Visible row `r` maps to alacritty grid line `r - display_offset`.
      let display_offset = term.grid().display_offset() as i32;
      let history_size = term.grid().history_size() as i32;

      let cursor_pt = term.grid().cursor.point;
      let cursor_display_line = cursor_pt.line.0 + display_offset;
      let cursor_in_view = cursor_display_line >= 0 && cursor_display_line < rows as i32;
      let cursor = (cursor_display_line.max(0) as usize, cursor_pt.column.0);
      let cursor_visible = cursor_in_view && term.mode().contains(TermMode::SHOW_CURSOR);
      let style = term.cursor_style();

      let snapshot_row = |grid_line: i32| -> Vec<CellSnapshot> {
        (0..cols)
          .map(|col| {
            let cell = &term.grid()[AlacPoint::new(Line(grid_line), Column(col))];
            CellSnapshot {
              c: cell.c,
              fg: cell.fg,
              bg: cell.bg,
              bold: cell.flags.contains(Flags::BOLD),
              italic: cell.flags.contains(Flags::ITALIC),
              underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
              strikeout: cell.flags.contains(Flags::STRIKEOUT),
              dim: cell.flags.contains(Flags::DIM),
              inverse: cell.flags.contains(Flags::INVERSE),
              wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
            }
          })
          .collect()
      };

      let snapshot_rows: Vec<Vec<CellSnapshot>> = (0..rows)
        .map(|row| snapshot_row(row as i32 - display_offset))
        .collect();

      // Overscan: one extra row above/below the viewport for sub-line scroll fill.
      let overscan_top = if display_offset < history_size {
        Some(snapshot_row(-1 - display_offset))
      } else {
        None
      };
      let overscan_bottom = if display_offset > 0 {
        Some(snapshot_row(rows as i32 - display_offset))
      } else {
        None
      };

      let selection_range = term.selection.as_ref().and_then(|s| s.to_range(term));

      GridSnapshot {
        rows: snapshot_rows,
        overscan_top,
        overscan_bottom,
        cursor,
        cursor_visible,
        cursor_shape: style.shape,
        cursor_blinking: style.blinking,
        selection: selection_range,
        display_offset: display_offset as usize,
      }
    })
  }
}

#[derive(Debug, Clone)]
pub struct CellSnapshot {
  pub c: char,
  pub fg: alacritty_terminal::vte::ansi::Color,
  pub bg: alacritty_terminal::vte::ansi::Color,
  pub bold: bool,
  pub italic: bool,
  pub underline: bool,
  pub strikeout: bool,
  pub dim: bool,
  pub inverse: bool,
  /// Right half of a wide-char pair; skip rendering, the left half spans both cells.
  pub wide_spacer: bool,
}

#[derive(Debug, Clone)]
pub struct GridSnapshot {
  pub rows: Vec<Vec<CellSnapshot>>,
  /// Row above the topmost visible row; `None` at the top of scrollback.
  pub overscan_top: Option<Vec<CellSnapshot>>,
  /// Row below the bottommost visible row; `None` at the live tail.
  pub overscan_bottom: Option<Vec<CellSnapshot>>,
  pub cursor: (usize, usize),
  pub cursor_visible: bool,
  pub cursor_shape: alacritty_terminal::vte::ansi::CursorShape,
  pub cursor_blinking: bool,
  /// Selection in absolute grid coordinates; painter subtracts `display_offset`.
  pub selection: Option<alacritty_terminal::selection::SelectionRange>,
  pub display_offset: usize,
}

#[cfg(test)]
mod tests {
  use super::*;
  use alacritty_terminal::event::Event;
  use alacritty_terminal::index::{Column, Line, Point, Side};
  use alacritty_terminal::selection::SelectionType;

  fn cell_at(t: &Terminal, line: usize, col: usize) -> char {
    t.with_term(|term| {
      let point = alacritty_terminal::index::Point::new(
        alacritty_terminal::index::Line(line as i32),
        alacritty_terminal::index::Column(col),
      );
      term.grid()[point].c
    })
  }

  #[test]
  fn remote_write_preserves_pty_line_feed_semantics() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write_remote(b"ab\nc");
    assert_eq!(cell_at(&t, 0, 0), 'a');
    assert_eq!(cell_at(&t, 0, 1), 'b');
    // A raw LF from a PTY advances the row but does not imply carriage
    // return. The SSH path must preserve that protocol detail.
    assert_eq!(cell_at(&t, 1, 2), 'c');
    assert_eq!(cell_at(&t, 1, 0), ' ');
  }

  #[test]
  fn device_attributes_sequence_emits_pty_write_event() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write_remote(b"\x1b[c");

    let events = t.drain_events();
    assert!(
      events
        .iter()
        .any(|event| matches!(event, Event::PtyWrite(bytes) if bytes == "\x1b[?6c")),
      "CSI c should make alacritty request a DA response write, got {events:?}"
    );
  }

  #[test]
  fn resize_changes_grid_dimensions() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.resize(GridSize::new(10, 40));
    let (rows, cols) = t.with_term(|term| (term.screen_lines(), term.columns()));
    assert_eq!(rows, 10);
    assert_eq!(cols, 40);
  }

  // ---- Selection-state tests --------------------------------------------
  // These tests drive the terminal's alacritty-backed selection through
  // `start_selection` / `update_selection` / `clear_selection` and verify
  // the round-trip through `selection_text` and `snapshot_grid`.

  #[test]
  fn start_selection_then_update_to_end_returns_full_text() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote(b"hello");

    t.start_selection(
      SelectionType::Simple,
      Point::new(Line(0), Column(0)),
      Side::Left,
    );
    t.update_selection(Point::new(Line(0), Column(4)), Side::Right);

    assert_eq!(t.selection_text(), Some("hello".to_string()));
  }

  #[test]
  fn clear_selection_drops_text() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote(b"hello");
    t.start_selection(
      SelectionType::Simple,
      Point::new(Line(0), Column(0)),
      Side::Left,
    );
    t.update_selection(Point::new(Line(0), Column(4)), Side::Right);
    assert!(t.has_selection());

    t.clear_selection();
    assert!(!t.has_selection());
    assert_eq!(t.selection_text(), None);
  }

  #[test]
  fn semantic_selection_expands_to_word_boundaries() {
    let t = Terminal::new(GridSize::new(3, 30));
    t.write_remote(b"hello world foo");

    // Anchor inside `world`. Semantic mode snaps to word bounds even
    // without dragging the focus.
    t.start_selection(
      SelectionType::Semantic,
      Point::new(Line(0), Column(7)),
      Side::Left,
    );
    t.update_selection(Point::new(Line(0), Column(8)), Side::Right);

    assert_eq!(t.selection_text(), Some("world".to_string()));
  }

  #[test]
  fn lines_selection_emits_full_line_with_trailing_newline() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote(b"alpha\r\nbeta");

    t.start_selection(
      SelectionType::Lines,
      Point::new(Line(0), Column(2)),
      Side::Left,
    );

    assert_eq!(t.selection_text(), Some("alpha\n".to_string()));
  }

  #[test]
  fn select_all_covers_visible_grid() {
    let t = Terminal::new(GridSize::new(3, 5));
    t.write_remote(b"abc");
    t.select_all();

    let text = t.selection_text().expect("non-empty selection");
    assert!(
      text.starts_with("abc"),
      "expected select_all to cover written text, got {text:?}"
    );
  }

  #[test]
  fn snapshot_exposes_selection_range_in_absolute_grid_coords() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote(b"hello");
    t.start_selection(
      SelectionType::Simple,
      Point::new(Line(0), Column(0)),
      Side::Left,
    );
    t.update_selection(Point::new(Line(0), Column(2)), Side::Right);

    let snap = t.snapshot_grid();
    let range = snap.selection.expect("selection range present in snapshot");
    assert_eq!(range.start, Point::new(Line(0), Column(0)));
    assert_eq!(range.end, Point::new(Line(0), Column(2)));
    assert_eq!(snap.display_offset, 0);
  }

  #[test]
  fn selection_in_scrollback_survives_scroll() {
    // Selection is stored in absolute grid coords (Line can be negative
    // for scrollback), so scrolling must not move the highlight relative
    // to its content.
    let mut size = GridSize::new(3, 10);
    size.scrollback = 50;
    let t = Terminal::new(size);
    for i in 0..10 {
      t.write_remote(format!("line{i}\r\n").as_bytes());
    }

    // Anchor a selection on a scrollback line (Line(-5)) and scroll
    // around. The text it serializes should be stable.
    t.start_selection(
      SelectionType::Lines,
      Point::new(Line(-5), Column(0)),
      Side::Left,
    );
    let text_before = t.selection_text();
    t.scroll_lines(3);
    let text_after = t.selection_text();
    assert_eq!(text_before, text_after);
  }
}
