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
  grid::Dimensions,
  index::{Column, Line, Point, Side},
  selection::{Selection as AlacSelection, SelectionType},
  term::color,
  term::Config,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
  Cell,
  Word,
  Line,
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
    use alacritty_terminal::grid::Scroll;
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Delta(delta));
  }

  /// Snap the viewport back to the bottom (the live cursor position).
  /// Called when the user types something, since output should land in
  /// view rather than be hidden under scrollback.
  pub fn scroll_to_bottom(&self) {
    use alacritty_terminal::grid::Scroll;
    let mut term = self.term.lock();
    term.scroll_display(Scroll::Bottom);
  }

  /// Drain queued alacritty events (e.g. PTY writes the terminal wants to
  /// emit in response to escape sequences). Returns ownership; consumers
  /// forward bytes to the SSH channel.
  pub fn drain_events(&self) -> Vec<AlacEvent> {
    self.events.try_iter().collect()
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

  pub fn cursor_blinking(&self) -> bool {
    self.with_term(|term| term.cursor_style().blinking)
  }

  /// Convert a display-coordinate selection to text using alacritty's own
  /// selection serializer. This preserves terminal details like wrapped
  /// lines and wide-character spacer cells better than reconstructing text
  /// from our rendered cell snapshot.
  pub fn selected_text(
    &self,
    start: (usize, usize),
    end_exclusive: (usize, usize),
    kind: SelectionKind,
  ) -> Option<String> {
    let mut term = self.term.lock();
    let selection = selection_from_display_range(&term, start, end_exclusive, kind)?;
    let previous = term.selection.replace(selection);
    let text = term.selection_to_string();
    term.selection = previous;
    text
  }

  /// Snapshot the visible grid as one `String` per row. Trailing whitespace
  /// is stripped per line. Used by debug/log paths; the styled view uses
  /// [`Terminal::snapshot_grid`] instead.
  pub fn snapshot_lines(&self) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line, Point};

    self.with_term(|term| {
      let cols = term.columns();
      let rows = term.screen_lines();
      (0..rows)
        .map(|row| {
          (0..cols)
            .map(|col| term.grid()[Point::new(Line(row as i32), Column(col))].c)
            .collect::<String>()
            .trim_end()
            .to_string()
        })
        .collect()
    })
  }

  /// Snapshot every visible cell with its style flags, plus the cursor
  /// position, visibility, shape, and DECSCUSR blink request. Honors the
  /// current scroll offset: if the user has scrolled into history, the
  /// snapshot reflects the historical lines, not the live bottom of the
  /// grid.
  pub fn snapshot_grid(&self) -> GridSnapshot {
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::TermMode;

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
              let cell = &term.grid()[Point::new(Line(grid_line), Column(col))];
              CellSnapshot {
                c: cell.c,
                fg: cell.fg,
                bg: cell.bg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.contains(Flags::UNDERLINE),
                inverse: cell.flags.contains(Flags::INVERSE),
                wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
              }
            })
            .collect()
        })
        .collect();

      GridSnapshot {
        rows: snapshot_rows,
        cursor,
        cursor_visible,
        cursor_shape: style.shape,
        cursor_blinking: style.blinking,
      }
    })
  }
}

fn selection_from_display_range(
  term: &Term<ChannelListener>,
  start: (usize, usize),
  end_exclusive: (usize, usize),
  kind: SelectionKind,
) -> Option<AlacSelection> {
  if start >= end_exclusive {
    return None;
  }

  let display_offset = term.grid().display_offset() as i32;
  let last_col = term.columns().saturating_sub(1);
  let line_for_row = |row: usize| Line(row as i32 - display_offset);
  let col = |col: usize| Column(col.min(last_col));

  let selection_type = match kind {
    // Word-mode is already snapped by the view. Use a simple alacritty range
    // here so copied text exactly matches the visible highlighted cells.
    SelectionKind::Cell | SelectionKind::Word => SelectionType::Simple,
    SelectionKind::Line => SelectionType::Lines,
  };

  let start_point = Point::new(line_for_row(start.0), col(start.1));
  let (end_row, end_col, end_side) = match selection_type {
    SelectionType::Lines => (end_exclusive.0, 0, Side::Right),
    _ if end_exclusive.1 == 0 => {
      if end_exclusive.0 == 0 {
        return None;
      }
      (end_exclusive.0 - 1, last_col, Side::Right)
    }
    _ => (end_exclusive.0, end_exclusive.1 - 1, Side::Right),
  };

  let end_point = Point::new(line_for_row(end_row), col(end_col));
  let mut selection = AlacSelection::new(selection_type, start_point, Side::Left);
  selection.update(end_point, end_side);
  Some(selection)
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
}

#[cfg(test)]
mod tests {
  use super::*;

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
    use alacritty_terminal::event::Event;

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

  #[test]
  fn selected_text_uses_terminal_wrapped_line_serialization() {
    let mut size = GridSize::new(3, 3);
    size.scrollback = 10;
    let t = Terminal::new(size);
    t.write_remote(b"abcdef");

    assert_eq!(
      t.selected_text((0, 0), (1, 3), SelectionKind::Cell),
      Some("abcdef".to_string())
    );
  }

  #[test]
  fn selected_text_skips_wide_char_spacer_cells() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote("你x".as_bytes());

    assert_eq!(
      t.selected_text((0, 0), (0, 3), SelectionKind::Cell),
      Some("你x".to_string())
    );
  }

  #[test]
  fn selected_text_line_mode_preserves_terminal_line_semantics() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write_remote(b"alpha\r\nbeta");

    assert_eq!(
      t.selected_text((0, 0), (0, 10), SelectionKind::Line),
      Some("alpha\n".to_string())
    );
  }
}
