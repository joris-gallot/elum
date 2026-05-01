//! Custom GPUI `Element` for the terminal grid.
//!
//! Layout claims the full
//! parent area, prepaint snapshots the grid and measures the cell from real
//! font metrics, paint emits one batch of background quads followed by one
//! shaped text line per row plus the cursor block.
//!
//! Design notes:
//! - Cell width is the advance of `'M'` in the cascaded font. We force every
//!   glyph to that advance via `shape_line(.., Some(cell_width))`. CJK and
//!   emoji are squished to one cell each - fine for a terminal client; the
//!   downside is purely aesthetic.
//! - Adjacent cells with identical style attributes (fg, bold, italic,
//!   underline) are coalesced into a single `TextRun` to keep the run
//!   array small. Backgrounds are painted as separate quads underneath.
//! - The cursor is painted as a single quad in cursor color; the glyph
//!   under it is rendered with the original background color so it stays
//!   legible against the bright block.

use std::sync::Arc;

use gpui::{
  fill, point, px, relative, size, App, Bounds, Element, ElementId, FontStyle, FontWeight,
  GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Pixels, Point, Rgba,
  SharedString, Size, Style, TextAlign, TextRun, TextStyle, UnderlineStyle, Window,
};

use crate::colors::{
  cursor_color, default_background, default_foreground, selection_color, to_rgba,
};
use crate::view::Selection;
use crate::{CellSnapshot, GridSnapshot, Terminal};

/// Vertical multiplier on font size to derive line height.
const LINE_HEIGHT_RATIO: f32 = 1.3;

pub struct TerminalElement {
  terminal: Arc<Terminal>,
  selection: Option<Selection>,
}

impl TerminalElement {
  pub fn new(terminal: Arc<Terminal>, selection: Option<Selection>) -> Self {
    Self {
      terminal,
      selection,
    }
  }
}

pub struct Prepaint {
  snapshot: GridSnapshot,
  cell_width: Pixels,
  line_height: Pixels,
  font_size: Pixels,
  text_style: TextStyle,
  origin: Point<Pixels>,
}

impl IntoElement for TerminalElement {
  type Element = Self;
  fn into_element(self) -> Self::Element {
    self
  }
}

impl Element for TerminalElement {
  type RequestLayoutState = ();
  type PrepaintState = Prepaint;

  fn id(&self) -> Option<ElementId> {
    None
  }

  fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
    None
  }

  fn request_layout(
    &mut self,
    _id: Option<&GlobalElementId>,
    _inspector: Option<&InspectorElementId>,
    window: &mut Window,
    cx: &mut App,
  ) -> (LayoutId, Self::RequestLayoutState) {
    let style = Style {
      size: Size {
        width: relative(1.).into(),
        height: relative(1.).into(),
      },
      ..Default::default()
    };
    let layout_id = window.request_layout(style, [], cx);
    (layout_id, ())
  }

  fn prepaint(
    &mut self,
    _id: Option<&GlobalElementId>,
    _inspector: Option<&InspectorElementId>,
    bounds: Bounds<Pixels>,
    _: &mut Self::RequestLayoutState,
    window: &mut Window,
    cx: &mut App,
  ) -> Self::PrepaintState {
    let text_style = window.text_style();
    let rem_size = window.rem_size();
    let font_size = text_style.font_size.to_pixels(rem_size);
    let line_height = px(f32::from(font_size) * LINE_HEIGHT_RATIO);

    let font_id = cx.text_system().resolve_font(&text_style.font());
    // Use 'M' for cell width, broadest representative ASCII glyph in most monospace fonts.
    let cell_width = cx
      .text_system()
      .advance(font_id, font_size, 'M')
      .map_or_else(|_| px(f32::from(font_size) * 0.6), |adv| adv.width);

    let snapshot = self.terminal.snapshot_grid();

    Prepaint {
      snapshot,
      cell_width,
      line_height,
      font_size,
      text_style,
      origin: bounds.origin,
    }
  }

  fn paint(
    &mut self,
    _id: Option<&GlobalElementId>,
    _inspector: Option<&InspectorElementId>,
    bounds: Bounds<Pixels>,
    _: &mut Self::RequestLayoutState,
    prepaint: &mut Self::PrepaintState,
    window: &mut Window,
    cx: &mut App,
  ) {
    let bg_default = default_background();
    let fg_default = default_foreground();
    let cursor_bg = cursor_color();
    let sel_bg = selection_color();

    // Window background - cells without explicit bg paint over this.
    window.paint_quad(fill(bounds, bg_default));

    let cell_w = prepaint.cell_width;
    let line_h = prepaint.line_height;
    let origin = prepaint.origin;
    let snapshot = &prepaint.snapshot;
    let selection = self.selection;

    // Pass 1: background quads. We coalesce horizontally adjacent cells
    // with the same effective bg into a single quad; reduces draw calls
    // and avoids hairlines between identical neighbors.
    for (row_idx, row) in snapshot.rows.iter().enumerate() {
      paint_row_backgrounds(
        row,
        row_idx,
        snapshot.cursor,
        snapshot.cursor_visible,
        selection,
        origin,
        cell_w,
        line_h,
        fg_default,
        bg_default,
        cursor_bg,
        sel_bg,
        window,
      );
    }

    // Pass 2: shape and paint the row text. One `shape_line` per row,
    // with adjacent same-style cells coalesced into shared TextRuns.
    for (row_idx, row) in snapshot.rows.iter().enumerate() {
      paint_row_text(
        row,
        row_idx,
        snapshot.cursor,
        snapshot.cursor_visible,
        origin,
        cell_w,
        line_h,
        prepaint.font_size,
        &prepaint.text_style,
        fg_default,
        bg_default,
        window,
        cx,
      );
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn paint_row_backgrounds(
  row: &[CellSnapshot],
  row_idx: usize,
  cursor: (usize, usize),
  cursor_visible: bool,
  selection: Option<Selection>,
  origin: Point<Pixels>,
  cell_w: Pixels,
  line_h: Pixels,
  fg_default: Rgba,
  bg_default: Rgba,
  cursor_bg: Rgba,
  sel_bg: Rgba,
  window: &mut Window,
) {
  let y = origin.y + line_h * row_idx as f32;

  let mut run_start: Option<usize> = None;
  let mut run_color: Rgba = bg_default;

  for (col, cell) in row.iter().enumerate() {
    let bg = effective_bg(cell, fg_default, bg_default);
    let is_cursor = cursor_visible && cursor == (row_idx, col);
    let is_selected = selection.is_some_and(|s| s.contains(row_idx, col));
    // Selection wins over cursor: the user is highlighting this cell,
    // not editing at it. Cursor wins over the cell's own bg.
    let final_bg = if is_selected {
      sel_bg
    } else if is_cursor {
      cursor_bg
    } else {
      bg
    };

    match run_start {
      Some(_) if final_bg == run_color => {
        // continue current run
      }
      Some(start) => {
        if run_color != bg_default {
          flush_bg_run(run_color, start, col, y, origin, cell_w, line_h, window);
        }
        run_start = Some(col);
        run_color = final_bg;
      }
      None => {
        run_start = Some(col);
        run_color = final_bg;
      }
    }
  }

  if let Some(start) = run_start {
    if run_color != bg_default {
      flush_bg_run(
        run_color,
        start,
        row.len(),
        y,
        origin,
        cell_w,
        line_h,
        window,
      );
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn flush_bg_run(
  color: Rgba,
  start_col: usize,
  end_col: usize,
  y: Pixels,
  origin: Point<Pixels>,
  cell_w: Pixels,
  line_h: Pixels,
  window: &mut Window,
) {
  let x = origin.x + cell_w * start_col as f32;
  let span = cell_w * (end_col - start_col) as f32;
  let bounds = Bounds::new(point(x, y), size(span, line_h));
  window.paint_quad(fill(bounds, color));
}

#[allow(clippy::too_many_arguments)]
fn paint_row_text(
  row: &[CellSnapshot],
  row_idx: usize,
  cursor: (usize, usize),
  cursor_visible: bool,
  origin: Point<Pixels>,
  cell_w: Pixels,
  line_h: Pixels,
  font_size: Pixels,
  text_style: &TextStyle,
  fg_default: Rgba,
  bg_default: Rgba,
  window: &mut Window,
  cx: &mut App,
) {
  // Build the text and runs by coalescing adjacent cells with identical
  // style attributes (fg color + bold + italic + underline).
  let mut text = String::with_capacity(row.len());
  let mut runs: Vec<TextRun> = Vec::new();

  let mut current_style: Option<RowRunStyle> = None;
  let mut current_len: usize = 0;

  for (col, cell) in row.iter().enumerate() {
    let glyph: char = if cell.wide_spacer { ' ' } else { cell.c };
    let glyph_str: SharedString = glyph.to_string().into();
    let glyph_byte_len = glyph_str.len();

    let is_cursor = cursor_visible && cursor == (row_idx, col);
    let style = row_run_style(cell, is_cursor, text_style, fg_default, bg_default);

    match &current_style {
      Some(s) if s == &style => {
        current_len += glyph_byte_len;
      }
      Some(s) => {
        runs.push(s.to_text_run(current_len, text_style));
        current_style = Some(style);
        current_len = glyph_byte_len;
      }
      None => {
        current_style = Some(style);
        current_len = glyph_byte_len;
      }
    }
    text.push_str(&glyph_str);
  }

  if let Some(s) = current_style.take() {
    runs.push(s.to_text_run(current_len, text_style));
  }

  if text.is_empty() {
    return;
  }

  let row_origin = point(origin.x, origin.y + line_h * row_idx as f32);
  let shaped_line =
    window
      .text_system()
      .shape_line(SharedString::from(text), font_size, &runs, Some(cell_w));

  let _ = shaped_line.paint(row_origin, line_h, TextAlign::Left, None, window, cx);
}

/// Effective background for a cell, accounting for `inverse`. Cursor
/// override is applied later.
fn effective_bg(cell: &CellSnapshot, fg_default: Rgba, bg_default: Rgba) -> Rgba {
  let cell_fg = to_rgba(cell.fg, fg_default, bg_default);
  let cell_bg = to_rgba(cell.bg, fg_default, bg_default);
  if cell.inverse {
    cell_fg
  } else {
    cell_bg
  }
}

#[derive(Clone, PartialEq)]
struct RowRunStyle {
  color: Hsla,
  bold: bool,
  italic: bool,
  underline: Option<Hsla>,
}

impl RowRunStyle {
  fn to_text_run(&self, len: usize, base: &TextStyle) -> TextRun {
    let mut font = base.font();
    if self.bold {
      font.weight = FontWeight::BOLD;
    }
    if self.italic {
      font.style = FontStyle::Italic;
    }
    TextRun {
      len,
      font,
      color: self.color,
      background_color: None,
      underline: self.underline.map(|color| UnderlineStyle {
        color: Some(color),
        thickness: px(1.),
        wavy: false,
      }),
      strikethrough: None,
    }
  }
}

fn row_run_style(
  cell: &CellSnapshot,
  is_cursor: bool,
  _base: &TextStyle,
  fg_default: Rgba,
  bg_default: Rgba,
) -> RowRunStyle {
  let cell_fg = to_rgba(cell.fg, fg_default, bg_default);
  let cell_bg = to_rgba(cell.bg, fg_default, bg_default);
  let (mut fg, _bg) = if cell.inverse {
    (cell_bg, cell_fg)
  } else {
    (cell_fg, cell_bg)
  };
  if is_cursor {
    // Glyph beneath the cursor uses the cell's effective bg so it stays
    // legible against the cursor block.
    fg = if cell.inverse { cell_fg } else { cell_bg };
  }

  RowRunStyle {
    color: fg.into(),
    bold: cell.bold,
    italic: cell.italic,
    underline: if cell.underline {
      Some(fg.into())
    } else {
      None
    },
  }
}
