//! Terminal data model: wraps `alacritty_terminal::Term` and exposes
//! source-agnostic write APIs. The model knows nothing about SSH,
//! PTYs, or rendering, those concerns belong to other crates.

pub mod colors;
pub mod element;
pub mod keys;
pub mod view;

use std::sync::Arc;

use alacritty_terminal::{
  event::{Event as AlacEvent, EventListener},
  grid::Dimensions,
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

impl Terminal {
  /// Create a fresh terminal with the given grid size and default config.
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

  /// Feed display text into the terminal. Bare `\n` is normalized to `\r\n`
  /// so debug/display-only output starts the next line at column 0.
  ///
  /// Do not use this for bytes coming from a real PTY/SSH channel; use
  /// [`Self::write_remote`] so terminal control semantics remain exact.
  pub fn write(&self, bytes: &[u8]) {
    let mut converted = Vec::with_capacity(bytes.len());
    let mut prev = 0u8;
    for &b in bytes {
      if b == b'\n' && prev != b'\r' {
        converted.push(b'\r');
      }
      converted.push(b);
      prev = b;
    }

    self.advance(&converted);
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
  fn writes_plain_text_into_grid() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write(b"hello");
    assert_eq!(cell_at(&t, 0, 0), 'h');
    assert_eq!(cell_at(&t, 0, 1), 'e');
    assert_eq!(cell_at(&t, 0, 2), 'l');
    assert_eq!(cell_at(&t, 0, 3), 'l');
    assert_eq!(cell_at(&t, 0, 4), 'o');
  }

  #[test]
  fn line_feed_advances_cursor_to_next_row_column_zero() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write(b"ab\nc");
    assert_eq!(cell_at(&t, 0, 0), 'a');
    assert_eq!(cell_at(&t, 0, 1), 'b');
    assert_eq!(cell_at(&t, 1, 0), 'c');
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
  fn carriage_return_returns_to_column_zero_without_advancing_row() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write(b"ab\rc");
    assert_eq!(cell_at(&t, 0, 0), 'c');
    assert_eq!(cell_at(&t, 0, 1), 'b');
  }

  #[test]
  fn ansi_color_escape_sets_foreground_color() {
    let t = Terminal::new(GridSize::new(24, 80));
    // SGR 31 = red foreground. SGR 0 = reset.
    t.write(b"\x1b[31mred\x1b[0mx");

    let (r_fg, x_fg) = t.with_term(|term| {
      let red_point = alacritty_terminal::index::Point::new(
        alacritty_terminal::index::Line(0),
        alacritty_terminal::index::Column(0),
      );
      let x_point = alacritty_terminal::index::Point::new(
        alacritty_terminal::index::Line(0),
        alacritty_terminal::index::Column(3),
      );
      (term.grid()[red_point].fg, term.grid()[x_point].fg)
    });

    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    assert!(matches!(r_fg, Color::Named(NamedColor::Red)));
    assert!(matches!(x_fg, Color::Named(NamedColor::Foreground)));
  }

  #[test]
  fn cursor_position_reflects_writes() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.write(b"abcde");
    let cursor = t.with_term(|term| term.grid().cursor.point);
    assert_eq!(cursor.line.0, 0);
    assert_eq!(cursor.column.0, 5);
  }

  #[test]
  fn resize_changes_grid_dimensions() {
    let t = Terminal::new(GridSize::new(24, 80));
    t.resize(GridSize::new(10, 40));
    let (rows, cols) = t.with_term(|term| (term.screen_lines(), term.columns()));
    assert_eq!(rows, 10);
    assert_eq!(cols, 40);
  }

  fn row_text(snap: &GridSnapshot, row: usize) -> String {
    snap.rows[row]
      .iter()
      .map(|c| c.c)
      .collect::<String>()
      .trim_end()
      .to_string()
  }

  #[test]
  fn scroll_lines_changes_visible_viewport() {
    let t = Terminal::new(GridSize::new(3, 20));
    for i in 0..20 {
      t.write(format!("line{i:02}\n").as_bytes());
    }
    let live_top = row_text(&t.snapshot_grid(), 0);

    t.scroll_lines(5);
    let scrolled_top = row_text(&t.snapshot_grid(), 0);

    assert_ne!(
      live_top, scrolled_top,
      "scrolling into history must change the top visible row"
    );
  }

  #[test]
  fn scroll_to_bottom_returns_to_live_view() {
    let t = Terminal::new(GridSize::new(3, 20));
    for i in 0..20 {
      t.write(format!("line{i:02}\n").as_bytes());
    }
    let live_top = row_text(&t.snapshot_grid(), 0);

    t.scroll_lines(10);
    assert_ne!(row_text(&t.snapshot_grid(), 0), live_top);

    t.scroll_to_bottom();
    assert_eq!(row_text(&t.snapshot_grid(), 0), live_top);
  }

  #[test]
  fn scrollback_retains_lines_pushed_off_viewport() {
    let mut size = GridSize::new(5, 80);
    size.scrollback = 50;
    let t = Terminal::new(size);

    // Write 10 lines; the first 5 should scroll into the history buffer.
    for i in 0..10 {
      t.write(format!("line{i}\n").as_bytes());
    }

    let history = t.with_term(|term| term.history_size());
    assert!(
      history >= 5,
      "expected at least 5 lines in scrollback, got {history}"
    );
  }

  #[test]
  fn snapshot_grid_captures_text_and_cursor_position() {
    let t = Terminal::new(GridSize::new(5, 20));
    t.write(b"hi\r\nthere");

    let snap = t.snapshot_grid();
    assert_eq!(snap.rows.len(), 5);
    assert_eq!(snap.rows[0].len(), 20);
    assert_eq!(snap.rows[0][0].c, 'h');
    assert_eq!(snap.rows[0][1].c, 'i');
    assert_eq!(snap.rows[1][0].c, 't');
    assert_eq!(snap.rows[1][4].c, 'e');
    // Cursor sits one past the last written char on row 1.
    assert_eq!(snap.cursor, (1, 5));
    assert!(snap.cursor_visible);
  }

  #[test]
  fn snapshot_grid_marks_inverse_flag_after_sgr_7() {
    let t = Terminal::new(GridSize::new(3, 10));
    t.write(b"\x1b[7mX\x1b[0mY");
    let snap = t.snapshot_grid();
    assert!(snap.rows[0][0].inverse);
    assert!(!snap.rows[0][1].inverse);
  }

  #[test]
  fn wide_char_occupies_two_cells_with_spacer() {
    use alacritty_terminal::term::cell::Flags;

    let t = Terminal::new(GridSize::new(24, 80));
    // 你 (U+4F60) and 好 (U+597D): each is rendered double-width.
    t.write("你好".as_bytes());

    let (c0, c1, c2, c3, f0, f1) = t.with_term(|term| {
      let p = |col: usize| {
        alacritty_terminal::index::Point::new(
          alacritty_terminal::index::Line(0),
          alacritty_terminal::index::Column(col),
        )
      };
      (
        term.grid()[p(0)].c,
        term.grid()[p(1)].c,
        term.grid()[p(2)].c,
        term.grid()[p(3)].c,
        term.grid()[p(0)].flags,
        term.grid()[p(1)].flags,
      )
    });

    assert_eq!(c0, '你');
    assert!(f0.contains(Flags::WIDE_CHAR));
    // Cell 1 is the right half of the wide char - alacritty stores it as a
    // spacer flagged WIDE_CHAR_SPACER. The visible char is implementation-
    // defined; we only assert the flag.
    assert!(f1.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(c2, '好');
    // The two-cell width sequence repeats:
    assert_eq!(c1, ' '); // spacer renders as space
    let _ = c3; // unrelated cell, unused
  }
}
