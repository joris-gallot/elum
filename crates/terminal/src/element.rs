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

use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
  fill, point, px, relative, size, App, Bounds, CursorStyle, DispatchPhase, Element, ElementId,
  FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InputHandler,
  InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
  Point, Rgba, ScrollWheelEvent, SharedString, Size, Style, TextAlign, TextRun, TextStyle,
  UTF16Selection, UnderlineStyle, WeakEntity, Window,
};

use crate::colors::{
  cursor_color, default_background, default_foreground, selection_color, to_rgba,
};
use crate::view::TerminalView;
use crate::{CellSnapshot, GridSnapshot, Terminal};

/// Vertical multiplier on font size to derive line height.
const LINE_HEIGHT_RATIO: f32 = 1.3;

pub struct TerminalElement {
  terminal: Arc<Terminal>,
  view: WeakEntity<TerminalView>,
  focused: bool,
  blink_phase: bool,
}

impl TerminalElement {
  pub fn new(
    terminal: Arc<Terminal>,
    view: WeakEntity<TerminalView>,
    focused: bool,
    blink_phase: bool,
  ) -> Self {
    Self {
      terminal,
      view,
      focused,
      blink_phase,
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
  hitbox: Hitbox,
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

    // Sync the grid size + PTY size to the *real* bounds we got from
    // layout, using the *real* cell metrics. This must happen before we
    // snapshot the grid so the snapshot reflects the post-resize state.
    let _ = self.view.update(cx, |view, _| {
      view.sync_metrics(cell_width, line_height, bounds);
    });

    let snapshot = self.terminal.snapshot_grid();
    let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

    Prepaint {
      snapshot,
      cell_width,
      line_height,
      font_size,
      text_style,
      origin: bounds.origin,
      hitbox,
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
    let snapshot = &prepaint.snapshot;
    let cursor_kind = resolve_cursor_kind(snapshot, self.focused, self.blink_phase);
    let ctx = PaintCtx {
      origin: prepaint.origin,
      cell_w: prepaint.cell_width,
      line_h: prepaint.line_height,
      font_size: prepaint.font_size,
      cursor: snapshot.cursor,
      cursor_filled: cursor_kind == CursorKind::Filled,
      selection: snapshot.selection,
      display_offset: snapshot.display_offset,
      fg_default: default_foreground(),
      bg_default: default_background(),
      cursor_bg: cursor_color(),
      sel_bg: selection_color(),
      text_style: &prepaint.text_style,
    };

    // Window background - cells without explicit bg paint over this.
    window.paint_quad(fill(bounds, ctx.bg_default));

    // Pass 1: background quads. We coalesce horizontally adjacent cells
    // with the same effective bg into a single quad; reduces draw calls
    // and avoids hairlines between identical neighbors. Filled cursor
    // is rendered as bg here so adjacent same-bg cells coalesce.
    for (row_idx, row) in snapshot.rows.iter().enumerate() {
      paint_row_backgrounds(&ctx, row_idx, row, window);
    }

    // Pass 2: shape and paint the row text. One `shape_line` per row,
    // with adjacent same-style cells coalesced into shared TextRuns.
    for (row_idx, row) in snapshot.rows.iter().enumerate() {
      paint_row_text(&ctx, row_idx, row, window, cx);
    }

    // Pass 3: non-filled cursor decorations (hollow border, beam, underline).
    paint_cursor_overlay(cursor_kind, &ctx, window);

    // Pass 4: marked text overlay if the IME is composing.
    if let Some(view) = self.view.upgrade() {
      let marked = view.read(cx).marked_text().map(str::to_owned);
      if let Some(text) = marked {
        paint_marked_text_overlay(&ctx, &text, window, cx);
      }
    }

    self.register_mouse_listeners(prepaint.hitbox.clone(), window);
    self.register_input_handler(&ctx, window, cx);
  }
}

impl TerminalElement {
  /// Register window-level mouse listeners for the next frame, dispatching
  /// element-local positions back into the [`TerminalView`]. Done here
  /// rather than on a wrapper div so we can subtract the hitbox origin
  /// and so the dispatcher respects z-order (overlays above the terminal
  /// will not see clicks reach us).
  fn register_mouse_listeners(&self, hitbox: Hitbox, window: &mut Window) {
    let down_view = self.view.clone();
    let down_hitbox = hitbox.clone();
    window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
      if phase != DispatchPhase::Bubble {
        return;
      }
      if !down_hitbox.is_hovered(window) {
        return;
      }
      let local = e.position - down_hitbox.bounds.origin;
      let click_count = e.click_count;
      let button = e.button;
      let modifiers = e.modifiers;
      let _ = down_view.update(cx, |view, cx| {
        view.on_pointer_down(local, button, modifiers, click_count, window, cx);
      });
    });

    let move_view = self.view.clone();
    let move_hitbox = hitbox.clone();
    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
      if phase != DispatchPhase::Bubble {
        return;
      }
      // Drag continues even if the cursor leaves the hitbox. The view
      // itself decides whether to act based on `selection.dragging`.
      if !move_hitbox.is_hovered(window) && e.pressed_button.is_none() {
        return;
      }
      let local = e.position - move_hitbox.bounds.origin;
      let pressed_button = e.pressed_button;
      let modifiers = e.modifiers;
      let _ = move_view.update(cx, |view, cx| {
        view.on_pointer_move(local, pressed_button, modifiers, cx);
      });
    });

    let up_view = self.view.clone();
    let up_hitbox = hitbox.clone();
    window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, cx| {
      if phase != DispatchPhase::Bubble {
        return;
      }
      let local = e.position - up_hitbox.bounds.origin;
      let button = e.button;
      let modifiers = e.modifiers;
      let _ = up_view.update(cx, |view, cx| {
        view.on_pointer_up(local, button, modifiers, cx);
      });
    });

    let scroll_view = self.view.clone();
    let scroll_hitbox = hitbox.clone();
    window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
      if phase != DispatchPhase::Bubble || !scroll_hitbox.should_handle_scroll(window) {
        return;
      }
      let local = e.position - scroll_hitbox.bounds.origin;
      let _ = scroll_view.update(cx, |view, cx| {
        view.on_scroll_wheel_at(local, e, cx);
      });
    });

    // Show the I-beam cursor while hovering the terminal so the user
    // visually knows text is selectable. Must be inside paint with the
    // hitbox in scope.
    window.set_cursor_style(CursorStyle::IBeam, &hitbox);
  }

  /// Register the platform IME pipeline so non-ASCII / composition input reaches the PTY
  fn register_input_handler(&self, ctx: &PaintCtx<'_>, window: &mut Window, cx: &mut App) {
    let Some(view) = self.view.upgrade() else {
      return;
    };
    let focus = view.read(cx).focus().clone();
    let cursor_bounds = Some(Bounds::new(
      Point::new(
        ctx.origin.x + ctx.cell_w * ctx.cursor.1 as f32,
        ctx.origin.y + ctx.line_h * ctx.cursor.0 as f32,
      ),
      Size::new(ctx.cell_w, ctx.line_h),
    ));
    let handler = TerminalInputHandler {
      view: self.view.clone(),
      cursor_bounds,
    };
    window.handle_input(&focus, handler, cx);
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorKind {
  None,
  Filled,
  Hollow,
  Underline,
  Beam,
}

struct PaintCtx<'a> {
  origin: Point<Pixels>,
  cell_w: Pixels,
  line_h: Pixels,
  font_size: Pixels,
  cursor: (usize, usize),
  cursor_filled: bool,
  selection: Option<alacritty_terminal::selection::SelectionRange>,
  display_offset: usize,
  fg_default: Rgba,
  bg_default: Rgba,
  cursor_bg: Rgba,
  sel_bg: Rgba,
  text_style: &'a TextStyle,
}

fn resolve_cursor_kind(snapshot: &GridSnapshot, focused: bool, blink_phase: bool) -> CursorKind {
  if !snapshot.cursor_visible {
    return CursorKind::None;
  }
  // DECSCUSR-requested blink: hide on the off-phase. Only animates while
  // focused; an unfocused window keeps its indicator steady.
  if snapshot.cursor_blinking && focused && !blink_phase {
    return CursorKind::None;
  }

  match (snapshot.cursor_shape, focused) {
    (CursorShape::Hidden, _) => CursorKind::None,
    // Unfocused: always hollow regardless of the requested shape
    (_, false) => CursorKind::Hollow,
    (CursorShape::HollowBlock, true) => CursorKind::Hollow,
    (CursorShape::Block, true) => CursorKind::Filled,
    (CursorShape::Underline, true) => CursorKind::Underline,
    (CursorShape::Beam, true) => CursorKind::Beam,
  }
}

/// Paint the cursor overlay for non-`Filled` kinds
fn paint_cursor_overlay(kind: CursorKind, ctx: &PaintCtx<'_>, window: &mut Window) {
  if matches!(kind, CursorKind::None | CursorKind::Filled) {
    return;
  }
  let (row, col) = ctx.cursor;
  let color = ctx.cursor_bg;
  let cell_w = ctx.cell_w;
  let line_h = ctx.line_h;
  let cell_origin = point(
    ctx.origin.x + cell_w * col as f32,
    ctx.origin.y + line_h * row as f32,
  );
  match kind {
    CursorKind::Hollow => {
      let t = px(1.0);
      window.paint_quad(fill(Bounds::new(cell_origin, size(cell_w, t)), color));
      window.paint_quad(fill(
        Bounds::new(
          point(cell_origin.x, cell_origin.y + line_h - t),
          size(cell_w, t),
        ),
        color,
      ));
      window.paint_quad(fill(Bounds::new(cell_origin, size(t, line_h)), color));
      window.paint_quad(fill(
        Bounds::new(
          point(cell_origin.x + cell_w - t, cell_origin.y),
          size(t, line_h),
        ),
        color,
      ));
    }
    CursorKind::Underline => {
      let t = px(2.0);
      window.paint_quad(fill(
        Bounds::new(
          point(cell_origin.x, cell_origin.y + line_h - t),
          size(cell_w, t),
        ),
        color,
      ));
    }
    CursorKind::Beam => {
      let t = px(2.0);
      window.paint_quad(fill(Bounds::new(cell_origin, size(t, line_h)), color));
    }
    CursorKind::None | CursorKind::Filled => unreachable!(),
  }
}

/// Paint the IME pre-edit text on top of the terminal grid at the cursor position
fn paint_marked_text_overlay(ctx: &PaintCtx<'_>, marked: &str, window: &mut Window, cx: &mut App) {
  if marked.is_empty() {
    return;
  }
  let (row, col) = ctx.cursor;
  let origin = point(
    ctx.origin.x + ctx.cell_w * col as f32,
    ctx.origin.y + ctx.line_h * row as f32,
  );

  let mut style = ctx.text_style.clone();
  style.underline = Some(UnderlineStyle {
    color: Some(style.color),
    thickness: px(1.0),
    wavy: false,
  });
  let runs = vec![TextRun {
    len: marked.len(),
    font: style.font(),
    color: style.color,
    background_color: None,
    underline: style.underline,
    strikethrough: None,
  }];
  let shaped = window.text_system().shape_line(
    SharedString::from(marked.to_string()),
    ctx.font_size,
    &runs,
    None,
  );

  // Clear the area behind the marked text so the cell glyphs at the
  // cursor (and any wraparound) don't bleed through the composing line.
  let cover = Bounds::new(origin, Size::new(shaped.width, ctx.line_h));
  window.paint_quad(fill(cover, ctx.bg_default));

  let _ = shaped.paint(origin, ctx.line_h, TextAlign::Left, None, window, cx);
}

fn paint_row_backgrounds(
  ctx: &PaintCtx<'_>,
  row_idx: usize,
  row: &[CellSnapshot],
  window: &mut Window,
) {
  let y = ctx.origin.y + ctx.line_h * row_idx as f32;
  let grid_line = Line(row_idx as i32 - ctx.display_offset as i32);

  let mut run_start: Option<usize> = None;
  let mut run_color: Rgba = ctx.bg_default;

  for (col, cell) in row.iter().enumerate() {
    let bg = effective_bg(cell, ctx.fg_default, ctx.bg_default);
    let is_cursor = ctx.cursor_filled && ctx.cursor == (row_idx, col);
    let is_selected = ctx.selection.is_some_and(|range| {
      let here = range.contains(AlacPoint::new(grid_line, Column(col)));
      // Wide-char trailing spacers should highlight together with the
      // left half: alacritty's range only covers the left cell, so we
      // also accept the cell at col-1 when this is a spacer.
      let spacer_partner =
        cell.wide_spacer && col > 0 && range.contains(AlacPoint::new(grid_line, Column(col - 1)));
      here || spacer_partner
    });
    // Selection wins over cursor: the user is highlighting this cell,
    // not editing at it. Cursor wins over the cell's own bg.
    let final_bg = if is_selected {
      ctx.sel_bg
    } else if is_cursor {
      ctx.cursor_bg
    } else {
      bg
    };

    match run_start {
      Some(_) if final_bg == run_color => {
        // continue current run
      }
      Some(start) => {
        if run_color != ctx.bg_default {
          flush_bg_run(ctx, run_color, start, col, y, window);
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
    if run_color != ctx.bg_default {
      flush_bg_run(ctx, run_color, start, row.len(), y, window);
    }
  }
}

fn flush_bg_run(
  ctx: &PaintCtx<'_>,
  color: Rgba,
  start_col: usize,
  end_col: usize,
  y: Pixels,
  window: &mut Window,
) {
  let x = ctx.origin.x + ctx.cell_w * start_col as f32;
  let span = ctx.cell_w * (end_col - start_col) as f32;
  let bounds = Bounds::new(point(x, y), size(span, ctx.line_h));
  window.paint_quad(fill(bounds, color));
}

fn paint_row_text(
  ctx: &PaintCtx<'_>,
  row_idx: usize,
  row: &[CellSnapshot],
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

    let is_cursor = ctx.cursor_filled && ctx.cursor == (row_idx, col);
    let style = row_run_style(
      cell,
      is_cursor,
      ctx.text_style,
      ctx.fg_default,
      ctx.bg_default,
    );

    match &current_style {
      Some(s) if s == &style => {
        current_len += glyph_byte_len;
      }
      Some(s) => {
        runs.push(s.to_text_run(current_len, ctx.text_style));
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
    runs.push(s.to_text_run(current_len, ctx.text_style));
  }

  if text.is_empty() {
    return;
  }

  let row_origin = point(ctx.origin.x, ctx.origin.y + ctx.line_h * row_idx as f32);
  let shaped_line = window.text_system().shape_line(
    SharedString::from(text),
    ctx.font_size,
    &runs,
    Some(ctx.cell_w),
  );

  let _ = shaped_line.paint(row_origin, ctx.line_h, TextAlign::Left, None, window, cx);
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

struct TerminalInputHandler {
  view: WeakEntity<TerminalView>,
  /// Cursor cell rect in element-local coordinates plus the element's
  /// origin offset, used to anchor the IME candidate window beside the caret
  cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
  fn selected_text_range(
    &mut self,
    _ignore_disabled_input: bool,
    _window: &mut Window,
    _cx: &mut App,
  ) -> Option<UTF16Selection> {
    // The terminal has no editable selection in the input-handler sense
    Some(UTF16Selection {
      range: 0..0,
      reversed: false,
    })
  }

  fn marked_text_range(
    &mut self,
    _window: &mut Window,
    cx: &mut App,
  ) -> Option<std::ops::Range<usize>> {
    let len = self
      .view
      .upgrade()?
      .read(cx)
      .marked_text()
      .map(|s| s.encode_utf16().count())?;
    Some(0..len)
  }

  fn text_for_range(
    &mut self,
    _range_utf16: std::ops::Range<usize>,
    _adjusted_range: &mut Option<std::ops::Range<usize>>,
    _window: &mut Window,
    _cx: &mut App,
  ) -> Option<String> {
    // The terminal grid isn't an editable document
    // the IME has no use for arbitrary substrings of it
    None
  }

  fn replace_text_in_range(
    &mut self,
    _replacement_range: Option<std::ops::Range<usize>>,
    text: &str,
    _window: &mut Window,
    cx: &mut App,
  ) {
    let _ = self.view.update(cx, |view, cx| {
      view.clear_marked_text(cx);
      view.commit_text(text, cx);
    });
  }

  fn replace_and_mark_text_in_range(
    &mut self,
    _range_utf16: Option<std::ops::Range<usize>>,
    new_text: &str,
    _new_selected_range: Option<std::ops::Range<usize>>,
    _window: &mut Window,
    cx: &mut App,
  ) {
    let _ = self.view.update(cx, |view, cx| {
      view.set_marked_text(new_text.to_string(), cx);
    });
  }

  fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
    let _ = self.view.update(cx, |view, cx| {
      view.clear_marked_text(cx);
    });
  }

  fn bounds_for_range(
    &mut self,
    _range_utf16: std::ops::Range<usize>,
    _window: &mut Window,
    _cx: &mut App,
  ) -> Option<Bounds<Pixels>> {
    self.cursor_bounds
  }

  fn character_index_for_point(
    &mut self,
    _point: Point<Pixels>,
    _window: &mut Window,
    _cx: &mut App,
  ) -> Option<usize> {
    // No editable document layout to map points into.
    None
  }

  fn apple_press_and_hold_enabled(&mut self) -> bool {
    false
  }
}
