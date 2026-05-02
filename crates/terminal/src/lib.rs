//! Terminal data model: wraps `alacritty_terminal::Term` and exposes
//! source-agnostic write APIs. The model knows nothing about SSH,
//! PTYs, or rendering, those concerns belong to other crates.

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

/// Logical terminal grid dimensions, expressed in cells (not pixels).
///
/// Implements `alacritty_terminal::grid::Dimensions` so it can be passed
/// directly to `Term::new` / `Term::resize`.
#[derive(Debug, Clone, Copy)]
pub struct GridSize {
  pub rows: u16,
  pub cols: u16,
  pub scrollback: u16,
}

/// Default scrollback line count. Tuned for "comfortable for casual SSH"
/// without keeping huge log dumps in memory forever. Override per-call via
/// the public field if needed.
pub const DEFAULT_SCROLLBACK_LINES: u16 = 10_000;

impl GridSize {
  /// Build a `GridSize` with the default scrollback. Use field assignment
  /// (`{ scrollback: 0, ..GridSize::new(rows, cols) }`) to override.
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

/// `EventListener` that forwards alacritty events through a `flume` channel.
/// Consumers drain the receiver at their own cadence (or ignore it entirely
/// in tests).
pub struct ChannelListener(Sender<AlacEvent>);

impl EventListener for ChannelListener {
  fn send_event(&self, event: AlacEvent) {
    let _ = self.0.send(event);
  }
}

/// Source-agnostic terminal model. Feed display text via [`Terminal::write`]
/// or raw PTY bytes via [`Terminal::write_remote`]; inspect the grid via
/// [`Terminal::with_term`].
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

  /// Feed raw bytes from a real PTY/SSH shell into the terminal. This path
  /// intentionally does not rewrite newlines or any other control bytes.
  pub fn write_remote(&self, bytes: &[u8]) {
    self.advance(bytes);
  }

  fn advance(&self, bytes: &[u8]) {
    let mut term = self.term.lock();
    let mut processor = self.processor.lock();
    processor.advance(&mut *term, bytes);
  }

  /// Borrow the underlying `Term` for inspection (rendering, tests).
  pub fn with_term<R>(&self, f: impl FnOnce(&Term<ChannelListener>) -> R) -> R {
    let term = self.term.lock();
    f(&term)
  }

  /// Resize the grid. Pending: signal the SSH side so the remote can
  /// re-flow output.
  pub fn resize(&self, size: GridSize) {
    let mut term = self.term.lock();
    term.resize(size);
  }

  /// Shift the visible viewport by `delta` lines into / out of scrollback.
  /// Positive shifts the view *up* into history; negative pushes it back
  /// toward live output. No-op if no scrollback is available in the
  /// requested direction.
  pub fn scroll_lines(&self, delta: i32) {
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Delta(delta));
  }

  /// Snap the viewport back to the bottom (the live cursor position).
  /// Called when the user types something, since output should land in
  /// view rather than be hidden under scrollback.
  pub fn scroll_to_bottom(&self) {
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Bottom);
  }

  /// Drain queued alacritty events (e.g. PTY writes the terminal wants to
  /// emit in response to escape sequences). Returns ownership; consumers
  /// forward bytes to the SSH channel.
  pub fn drain_events(&self) -> Vec<AlacEvent> {
    self.events.try_iter().collect()
  }

  /// How many lines the user has scrolled into history. `0` means the
  /// viewport is showing live output.
  pub fn display_offset(&self) -> usize {
    self.term.lock().grid().display_offset()
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

  /// True if the user currently has any selection active.
  pub fn has_selection(&self) -> bool {
    self.term.lock().selection.is_some()
  }

  /// Materialize the selected cells as a `String`, honoring wide chars,
  /// trailing whitespace stripping, and CRLF semantics. Returns `None`
  /// if the selection is empty or out of grid bounds.
  pub fn selection_text(&self) -> Option<String> {
    self.term.lock().selection_to_string()
  }

  /// Replace the selection with one covering everything from the topmost
  /// scrollback line to the bottommost live row. Matches the behavior of
  /// iTerm2 / Terminal.app when the user invokes "Select All" (scrollback
  /// included, not just the visible viewport).
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

  /// RGB value for a terminal color request. Runtime OSC color overrides
  /// stored by alacritty win over our static fallback palette.
  pub fn color_rgb(&self, index: usize) -> alacritty_terminal::vte::ansi::Rgb {
    self.with_term(|term| {
      if index < color::COUNT {
        term.colors()[index].unwrap_or_else(|| crate::colors::color_index_rgb(index))
      } else {
        crate::colors::default_foreground_rgb()
      }
    })
  }

  /// Snapshot every visible cell with its style flags, plus the cursor
  /// position, visibility, shape, and DECSCUSR blink request. Honors the
  /// current scroll offset: if the user has scrolled into history, the
  /// snapshot reflects the historical lines, not the live bottom of the
  /// grid.
  pub fn snapshot_grid(&self) -> GridSnapshot {
    self.with_term(|term| {
      let cols = term.columns();
      let rows = term.screen_lines();
      // alacritty grids use negative line numbers for scrollback. When
      // `display_offset > 0` we're scrolled up; the visible area maps
      // to grid lines `(row - display_offset)` for `row in 0..rows`.
      let display_offset = term.grid().display_offset() as i32;

      let cursor_pt = term.grid().cursor.point;
      let cursor_display_line = cursor_pt.line.0 + display_offset;
      let cursor_in_view = cursor_display_line >= 0 && cursor_display_line < rows as i32;
      let cursor = (cursor_display_line.max(0) as usize, cursor_pt.column.0);
      let cursor_visible = cursor_in_view && term.mode().contains(TermMode::SHOW_CURSOR);
      let style = term.cursor_style();

      let snapshot_rows: Vec<Vec<CellSnapshot>> = (0..rows)
        .map(|row| {
          let grid_line = row as i32 - display_offset;
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
        })
        .collect();

      // Selection range - clipped/normalized by alacritty against the
      // grid, so the painter doesn't need to worry about the order of
      // anchor/focus or scrollback edges.
      let selection_range = term.selection.as_ref().and_then(|s| s.to_range(term));

      GridSnapshot {
        rows: snapshot_rows,
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

/// One terminal cell flattened for rendering. Color is left as alacritty's
/// `Color` so the view can apply theme-aware mapping at paint time.
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
  /// Right half of a wide-char pair. Skip rendering its glyph; the left
  /// neighbor's wide glyph spans both cells visually.
  pub wide_spacer: bool,
}

#[derive(Debug, Clone)]
pub struct GridSnapshot {
  pub rows: Vec<Vec<CellSnapshot>>,
  pub cursor: (usize, usize),
  pub cursor_visible: bool,
  pub cursor_shape: alacritty_terminal::vte::ansi::CursorShape,
  pub cursor_blinking: bool,
  /// Live selection clipped to grid bounds, in absolute alacritty grid
  /// coordinates. The painter must subtract `display_offset` to translate
  /// to viewport rows.
  pub selection: Option<alacritty_terminal::selection::SelectionRange>,
  /// Number of lines the user has scrolled up into history. Display row
  /// `r` corresponds to grid `Line(r as i32 - display_offset as i32)`.
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
