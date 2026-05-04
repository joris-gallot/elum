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
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::Direction;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use gpui::prelude::FluentBuilder as _;
use gpui::{
  actions, div, px, AppContext as _, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle,
  Focusable, InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, ParentElement,
  Pixels, Point, Render, ScrollWheelEvent, SharedString, Styled, Subscription, Task, TouchPhase,
  Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _};
use ui::UiIconName;

actions!(
  terminal,
  [
    Copy,
    Paste,
    SelectAll,
    Search,
    SearchNext,
    SearchPrev,
    SearchDismiss,
    Tab,
    ShiftTab,
  ]
);

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

use crate::colors::TerminalTheme;
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
  search: Option<SearchState>,
  _focus_in: Option<Subscription>,
  _focus_out: Option<Subscription>,
  _relay: Task<()>,
  _blink: Task<()>,
}

/// Cap on `collect_matches` to keep "X of N" cheap on huge scrollbacks.
const MAX_MATCH_COUNT: usize = 1000;

struct SearchState {
  input: gpui::Entity<InputState>,
  regex: Option<RegexSearch>,
  current: Option<Match>,
  no_match: bool,
  /// Index of `current` in the full match list, 0-based.
  current_index: Option<usize>,
  /// Total matches, capped at `MAX_MATCH_COUNT`.
  total: usize,
  total_capped: bool,
  _input_subscription: Subscription,
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
      search: None,
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

  pub(crate) fn focus(&self) -> &FocusHandle {
    &self.focus
  }

  pub(crate) fn marked_text(&self) -> Option<&str> {
    self.marked_text.as_deref()
  }

  pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
    if text.is_empty() {
      self.clear_marked_text(cx);
      return;
    }
    self.marked_text = Some(text);
    cx.notify();
  }

  pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
    if self.marked_text.take().is_some() {
      cx.notify();
    }
  }

  pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
    if text.is_empty() {
      return;
    }
    self.terminal.scroll_to_bottom();
    self.terminal.clear_selection();
    let _ = self.to_remote.send(text.as_bytes().to_vec());
    cx.notify();
  }

  fn handle_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, _cx: &mut Context<Self>) {
    // Search-input keystrokes bubble up to this div; only relay when terminal owns focus.
    if !self.focus.is_focused(window) {
      return;
    }
    let mode = self.terminal.with_term(|term| *term.mode());
    if let Some(bytes) = keystroke_to_bytes(&ev.keystroke, mode) {
      // Typing engages the live shell: snap to bottom and clear stale selection.
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

  fn on_tab(&mut self, _: &Tab, _window: &mut Window, cx: &mut Context<Self>) {
    self.send_pty(b"\x09".to_vec());
    cx.stop_propagation();
  }

  fn on_shift_tab(&mut self, _: &ShiftTab, _window: &mut Window, cx: &mut Context<Self>) {
    self.send_pty(b"\x1b[Z".to_vec());
    cx.stop_propagation();
  }

  fn send_pty(&self, bytes: Vec<u8>) {
    self.terminal.scroll_to_bottom();
    self.terminal.clear_selection();
    let _ = self.to_remote.send(bytes);
  }

  fn on_search(&mut self, _: &Search, window: &mut Window, cx: &mut Context<Self>) {
    // Seed from a single-line selection only; multi-line text rarely matches as a query.
    let seed = self
      .terminal
      .selection_text()
      .filter(|s| !s.is_empty() && !s.contains('\n'));

    if let Some(state) = &self.search {
      let input = state.input.clone();
      let handle = input.read(cx).focus_handle(cx);
      window.focus(&handle, cx);
      // Identical seed = the highlight is the search match, not a fresh user selection.
      if let Some(seed) = seed {
        let current = input.read(cx).value().to_string();
        if seed != current {
          input.update(cx, |state, cx| state.set_value(seed, window, cx));
          self.on_search_text_changed(cx);
        }
      }
      return;
    }
    let input = cx.new(|cx| {
      let s = InputState::new(window, cx).placeholder("Find");
      if let Some(seed) = seed.clone() {
        s.default_value(seed)
      } else {
        s
      }
    });
    let sub = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| match ev {
      InputEvent::Change => this.on_search_text_changed(cx),
      InputEvent::PressEnter { .. } => this.search_step(Direction::Right, cx),
      InputEvent::Focus | InputEvent::Blur => cx.notify(),
    });
    // The seed text didn't go through Change, so trigger a search manually.
    let needs_initial_search = seed.is_some();
    let handle = input.read(cx).focus_handle(cx);
    self.search = Some(SearchState {
      input,
      regex: None,
      current: None,
      no_match: false,
      current_index: None,
      total: 0,
      total_capped: false,
      _input_subscription: sub,
    });
    window.focus(&handle, cx);
    if needs_initial_search {
      self.on_search_text_changed(cx);
    }
    cx.notify();
  }

  fn on_search_next(&mut self, _: &SearchNext, _window: &mut Window, cx: &mut Context<Self>) {
    self.search_step(Direction::Right, cx);
  }

  fn on_search_prev(&mut self, _: &SearchPrev, _window: &mut Window, cx: &mut Context<Self>) {
    self.search_step(Direction::Left, cx);
  }

  fn on_search_dismiss(&mut self, _: &SearchDismiss, window: &mut Window, cx: &mut Context<Self>) {
    if self.search.take().is_some() {
      window.focus(&self.focus, cx);
      cx.notify();
    }
  }

  fn on_search_text_changed(&mut self, cx: &mut Context<Self>) {
    let Some(state) = self.search.as_mut() else {
      return;
    };
    let query = state.input.read(cx).value().to_string();
    if query.is_empty() {
      state.regex = None;
      state.current = None;
      state.no_match = false;
      state.current_index = None;
      state.total = 0;
      state.total_capped = false;
      cx.notify();
      return;
    }
    state.regex = RegexSearch::new(&query).ok();
    state.current = None;
    state.current_index = None;
    if state.regex.is_none() {
      state.no_match = true;
      state.total = 0;
      state.total_capped = false;
      cx.notify();
      return;
    }
    // Start at the top so "1 of N" is the first match in reading order.
    self.search_step(Direction::Right, cx);
  }

  fn search_step(&mut self, direction: Direction, cx: &mut Context<Self>) {
    let Some(state) = self.search.as_mut() else {
      return;
    };
    let Some(mut regex) = state.regex.take() else {
      return;
    };
    let origin = self.compute_search_origin(direction);
    let mut result = self.terminal.regex_search(&mut regex, origin, direction);
    // Wrap to the opposite edge so Enter / Cmd+G keeps cycling instead of dead-ending.
    if result.is_none() {
      let wrap = self.wrap_origin(direction);
      result = self.terminal.regex_search(&mut regex, wrap, direction);
    }

    let all = self
      .terminal
      .collect_matches(&mut regex, MAX_MATCH_COUNT + 1);
    let total_capped = all.len() > MAX_MATCH_COUNT;
    let total = all.len().min(MAX_MATCH_COUNT);
    let current_index = result
      .as_ref()
      .and_then(|m| all.iter().take(total).position(|x| x.start() == m.start()));

    let state = self.search.as_mut().unwrap();
    state.regex = Some(regex);
    state.total = total;
    state.total_capped = total_capped;
    state.current_index = current_index;
    match result {
      Some(m) => {
        self.terminal.scroll_to_line(m.start().line);
        state.current = Some(m);
        state.no_match = false;
      }
      None => {
        state.no_match = true;
      }
    }
    cx.notify();
  }

  fn wrap_origin(&self, direction: Direction) -> alacritty_terminal::index::Point {
    use alacritty_terminal::index::{Column, Point as AlacPoint};
    let cols = self.last_size.map_or(80, |(_, c)| c) as usize;
    match direction {
      Direction::Right => AlacPoint::new(self.topmost_line(), Column(0)),
      Direction::Left => AlacPoint::new(self.bottommost_line(), Column(cols.saturating_sub(1))),
    }
  }

  fn compute_search_origin(&self, direction: Direction) -> alacritty_terminal::index::Point {
    use alacritty_terminal::index::{Column, Point as AlacPoint};
    let cols = self.last_size.map_or(80, |(_, c)| c) as usize;
    let last_col = Column(cols.saturating_sub(1));
    let state = self.search.as_ref();
    let current = state.and_then(|s| s.current.as_ref());

    match (current, direction) {
      // Stepping past the buffer edge would land on a line alacritty's grid panics on;
      // fall back to the wrap origin so the next search becomes the wrap-around itself.
      (Some(m), Direction::Right) => {
        let p = step_after(*m.end(), cols);
        if p.line.0 > self.bottommost_line().0 {
          self.wrap_origin(Direction::Right)
        } else {
          p
        }
      }
      (Some(m), Direction::Left) => match step_before(*m.start(), cols) {
        Some(p) if p.line.0 >= self.topmost_line().0 => p,
        _ => self.wrap_origin(Direction::Left),
      },
      (None, Direction::Left) => AlacPoint::new(self.bottommost_line(), last_col),
      (None, Direction::Right) => AlacPoint::new(self.topmost_line(), Column(0)),
    }
  }

  fn topmost_line(&self) -> alacritty_terminal::index::Line {
    self.terminal.with_term(|t| t.topmost_line())
  }

  fn bottommost_line(&self) -> alacritty_terminal::index::Line {
    self.terminal.with_term(|t| t.bottommost_line())
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
      // Wrap so the remote can skip auto-indent / interpretation.
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

  /// Pointer pressed at element-local `pos`. Selection mode follows click_count
  /// (1 = simple, 2 = semantic/word, 3+ = lines), shift extends an existing one.
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
    // Bare click without drag: clear so we don't paint a phantom 0-width selection.
    let still_empty = self
      .terminal
      .with_term(|t| t.selection.as_ref().is_some_and(|s| s.is_empty()));
    if still_empty {
      self.terminal.clear_selection();
      cx.notify();
    }
  }

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
    // Fall back to the FONT_SIZE heuristic only on the first frame.
    let line_height = self
      .last_cell_metrics
      .map_or_else(|| px(FONT_SIZE * LINE_HEIGHT_RATIO), |(_, h)| h);

    // Reset residual on a fresh gesture so the previous gesture's leftover doesn't jump us.
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
        // Remote owns the viewport; sub-line offset would visually misalign.
        self.scroll_px_acc = 0.0;
      } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        // Alt-screen apps (vim, less, htop) redraw in place; translate wheel to arrow keys.
        let bytes = alt_scroll(lines, mode.contains(TermMode::APP_CURSOR));
        let _ = self.to_remote.send(bytes);
        self.scroll_px_acc = 0.0;
      } else {
        self.terminal.scroll_lines(lines);
      }
    }

    // Drop residual at the scrollback edge so we don't peek empty space.
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

  /// Sub-line paint offset in pixels, in `(-line_h, line_h)`.
  pub(crate) fn scroll_offset_y(&self) -> f32 {
    self.scroll_px_acc
  }

  fn copy_selection_text(&self) -> Option<String> {
    self.terminal.selection_text()
  }

  /// Apply real bounds + cell metrics from prepaint; clamps to `MIN_COLS`/`MIN_ROWS`.
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

    let theme = cx
      .try_global::<TerminalTheme>()
      .copied()
      .unwrap_or_default();
    let effects = alacritty_event_effects(
      event,
      self.current_window_size(),
      clipboard_text.as_deref(),
      // Runtime OSC overrides win over the theme palette.
      |index| {
        self
          .terminal
          .osc_color_override(index)
          .unwrap_or_else(|| theme.color_index_rgb(index))
      },
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

fn match_to_range(m: &Match) -> alacritty_terminal::selection::SelectionRange {
  alacritty_terminal::selection::SelectionRange {
    start: *m.start(),
    end: *m.end(),
    is_block: false,
  }
}

/// Move one cell forward, wrapping to the next line at end-of-row.
fn step_after(
  p: alacritty_terminal::index::Point,
  cols: usize,
) -> alacritty_terminal::index::Point {
  use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
  if p.column.0 + 1 < cols {
    AlacPoint::new(p.line, Column(p.column.0 + 1))
  } else {
    AlacPoint::new(Line(p.line.0 + 1), Column(0))
  }
}

/// Move one cell backward, wrapping to the previous line at start-of-row.
/// Returns `None` if `p` is `(Line(MIN), Column(0))` (no preceding cell).
fn step_before(
  p: alacritty_terminal::index::Point,
  cols: usize,
) -> Option<alacritty_terminal::index::Point> {
  use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
  if p.column.0 > 0 {
    Some(AlacPoint::new(p.line, Column(p.column.0 - 1)))
  } else if p.line.0 > i32::MIN {
    Some(AlacPoint::new(
      Line(p.line.0 - 1),
      Column(cols.saturating_sub(1)),
    ))
  } else {
    None
  }
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

impl Render for TerminalView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let focused = self.focus.is_focused(window);
    let blink_phase = self.cursor_blink_phase;
    let theme = cx
      .try_global::<TerminalTheme>()
      .copied()
      .unwrap_or_default();

    let mut root = div()
      .id("terminal-view")
      .key_context(KEY_CONTEXT)
      .track_focus(&self.focus)
      .on_action(cx.listener(Self::on_copy))
      .on_action(cx.listener(Self::on_paste))
      .on_action(cx.listener(Self::on_select_all))
      .on_action(cx.listener(Self::on_search))
      .on_action(cx.listener(Self::on_search_next))
      .on_action(cx.listener(Self::on_search_prev))
      .on_action(cx.listener(Self::on_search_dismiss))
      .on_action(cx.listener(Self::on_tab))
      .on_action(cx.listener(Self::on_shift_tab))
      .on_key_down(cx.listener(Self::handle_key_down))
      .size_full()
      .relative()
      .bg(theme.background)
      .text_color(theme.foreground)
      .font_family(FONT_FAMILY)
      .text_size(px(FONT_SIZE))
      .p(px(PADDING))
      .child(TerminalElement::new(
        self.terminal.clone(),
        cx.entity().downgrade(),
        focused,
        blink_phase,
        self.viewport_search_matches(),
        self
          .search
          .as_ref()
          .and_then(|s| s.current.as_ref())
          .map(match_to_range),
      ));

    if let Some(state) = &self.search {
      root = root.child(self.render_search_bar(state, window, cx));
    }

    root
  }
}

/// Cap on per-frame viewport match scans; far above any plausible visible count.
const VIEWPORT_MATCH_LIMIT: usize = 200;

impl TerminalView {
  fn viewport_search_matches(&mut self) -> Vec<alacritty_terminal::selection::SelectionRange> {
    let Some(state) = self.search.as_mut() else {
      return Vec::new();
    };
    let Some(regex) = state.regex.as_mut() else {
      return Vec::new();
    };
    self
      .terminal
      .matches_in_viewport(regex, VIEWPORT_MATCH_LIMIT)
      .iter()
      .map(match_to_range)
      .collect()
  }

  fn render_search_bar(
    &self,
    state: &SearchState,
    window: &Window,
    cx: &mut Context<Self>,
  ) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let input_handle = state.input.read(cx).focus_handle(cx);
    let input_focused = input_handle.is_focused(window);
    let mut bar = gpui_component::h_flex()
      .id("terminal-search-bar")
      .occlude()
      .absolute()
      .top_2()
      .right_2()
      .gap_2()
      .items_center()
      .px_2()
      .py_1()
      .rounded_md()
      .bg(theme.popover)
      .text_color(theme.popover_foreground)
      .text_size(px(13.))
      .font_family(".SystemUIFont")
      .border_1()
      .border_color(theme.border)
      .on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |_, _, window, cx| {
          window.focus(&input_handle, cx);
        }),
      )
      .child(
        div()
          .flex_1()
          .rounded_sm()
          .border_1()
          .when_else(
            input_focused,
            |this| this.border_color(theme.ring),
            |this| this.border_color(theme.border),
          )
          .child(Input::new(&state.input).appearance(false).w(px(200.0))),
      );

    let (label_text, label_color) = if let Some(idx) = state.current_index {
      let suffix = if state.total_capped { "+" } else { "" };
      (
        format!("{} of {}{suffix}", idx + 1, state.total),
        theme.muted_foreground,
      )
    } else if state.no_match {
      ("no match".into(), theme.danger)
    } else {
      ("no match".into(), theme.muted_foreground)
    };
    bar = bar.child(
      div()
        .w(px(60.))
        .text_color(label_color)
        .text_size(px(11.))
        .child(SharedString::from(label_text)),
    );

    let nav_disabled = state.no_match || state.regex.is_none();
    bar = bar
      .child(
        Button::new("search-prev")
          .icon(UiIconName::ArrowUp)
          .ghost()
          .xsmall()
          .disabled(nav_disabled)
          .on_click(cx.listener(|this, _, window, cx| {
            this.on_search_prev(&SearchPrev, window, cx);
          })),
      )
      .child(
        Button::new("search-next")
          .icon(UiIconName::ArrowDown)
          .ghost()
          .xsmall()
          .disabled(nav_disabled)
          .on_click(cx.listener(|this, _, window, cx| {
            this.on_search_next(&SearchNext, window, cx);
          })),
      )
      .child(
        Button::new("search-close")
          .icon(IconName::Close)
          .ghost()
          .xsmall()
          .on_click(cx.listener(|this, _, window, cx| {
            this.on_search_dismiss(&SearchDismiss, window, cx);
          })),
      );

    bar
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
  use gpui::{Bounds, Entity, Size, TestAppContext};

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

  // ---- Search state machine ----------------------------------------------

  use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
  use alacritty_terminal::selection::SelectionType;
  use gpui::VisualTestContext;

  /// Builds a TerminalView attached to a real test window so action handlers
  /// that need `&mut Window` can be invoked.
  fn make_search_rig(
    cx: &mut TestAppContext,
  ) -> (Entity<TerminalView>, Arc<Terminal>, &mut VisualTestContext) {
    cx.update(|cx| gpui_component::init(cx));
    let terminal = Arc::new(Terminal::new(GridSize::new(24, 80)));
    let (_from_remote_tx, from_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (to_remote_tx, _to_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (resize_tx, _resize_rx) = flume::unbounded::<(u16, u16)>();
    let term_clone = terminal.clone();
    // gpui-component's `Root` must be the window root for InputState to function;
    // we capture the inner TerminalView via a shared cell so tests can drive it.
    let view_holder: std::rc::Rc<std::cell::RefCell<Option<Entity<TerminalView>>>> =
      std::rc::Rc::new(std::cell::RefCell::new(None));
    let view_holder_clone = view_holder.clone();
    let (_root, vcx) = cx.add_window_view(move |window, cx| {
      let inner =
        cx.new(|cx| TerminalView::new(term_clone, from_remote_rx, to_remote_tx, resize_tx, (), cx));
      *view_holder_clone.borrow_mut() = Some(inner.clone());
      gpui_component::Root::new(inner, window, cx)
    });
    let view = view_holder.borrow().as_ref().unwrap().clone();
    (view, terminal, vcx)
  }

  fn set_query(view: &Entity<TerminalView>, cx: &mut VisualTestContext, q: &str) {
    view.update_in(cx, |v: &mut TerminalView, window, cx| {
      let input = v.search.as_ref().expect("search open").input.clone();
      input.update(cx, |s, cx| s.set_value(q, window, cx));
      v.on_search_text_changed(cx);
    });
  }

  #[gpui::test]
  async fn search_lands_on_first_match(cx: &mut TestAppContext) {
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo baz");

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "foo");

    view.read_with(cx, |v, _| {
      let s = v.search.as_ref().expect("open");
      assert_eq!(s.total, 2);
      assert_eq!(s.current_index, Some(0));
      assert!(!s.no_match);
    });
  }

  #[gpui::test]
  async fn search_next_wraps_to_first_after_last(cx: &mut TestAppContext) {
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo");

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "foo");

    view.update_in(cx, |v, window, cx| {
      v.on_search_next(&SearchNext, window, cx);
    });
    view.read_with(cx, |v, _| {
      assert_eq!(v.search.as_ref().unwrap().current_index, Some(1));
    });

    // Past the last match: should wrap to index 0, no_match stays false.
    view.update_in(cx, |v, window, cx| {
      v.on_search_next(&SearchNext, window, cx);
    });
    view.read_with(cx, |v, _| {
      let s = v.search.as_ref().unwrap();
      assert_eq!(s.current_index, Some(0));
      assert!(!s.no_match);
    });
  }

  #[gpui::test]
  async fn search_prev_wraps_to_last_before_first(cx: &mut TestAppContext) {
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo");

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "foo");

    // Initial = index 0. Prev should wrap to the last (index 1).
    view.update_in(cx, |v, window, cx| {
      v.on_search_prev(&SearchPrev, window, cx);
    });
    view.read_with(cx, |v, _| {
      assert_eq!(v.search.as_ref().unwrap().current_index, Some(1));
    });
  }

  #[gpui::test]
  async fn empty_query_resets_search_state(cx: &mut TestAppContext) {
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo");

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "foo");
    set_query(&view, cx, "");

    view.read_with(cx, |v, _| {
      let s = v.search.as_ref().unwrap();
      assert!(s.regex.is_none());
      assert_eq!(s.total, 0);
      assert_eq!(s.current_index, None);
      assert!(!s.no_match, "empty query is not a 'no match' state");
    });
  }

  #[gpui::test]
  async fn no_match_query_flags_no_match(cx: &mut TestAppContext) {
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo");

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "zzz");

    view.read_with(cx, |v, _| {
      let s = v.search.as_ref().unwrap();
      assert!(s.no_match);
      assert_eq!(s.total, 0);
      assert_eq!(s.current_index, None);
    });
  }

  #[gpui::test]
  async fn mouse_selection_survives_search_dismiss(cx: &mut TestAppContext) {
    // Regression: search used to overwrite term.selection. The mouse selection
    // must remain intact after opening and closing the search bar.
    let (view, terminal, cx) = make_search_rig(cx);
    terminal.write_remote(b"foo bar foo");
    terminal.start_selection(
      SelectionType::Simple,
      AlacPoint::new(Line(0), Column(0)),
      Side::Left,
    );
    terminal.update_selection(AlacPoint::new(Line(0), Column(2)), Side::Right);
    assert_eq!(terminal.selection_text(), Some("foo".into()));

    view.update_in(cx, |v, window, cx| v.on_search(&Search, window, cx));
    set_query(&view, cx, "bar");
    view.update_in(cx, |v, window, cx| {
      v.on_search_dismiss(&SearchDismiss, window, cx);
    });

    assert_eq!(
      terminal.selection_text(),
      Some("foo".into()),
      "mouse selection must survive an unrelated search session"
    );
  }
}
