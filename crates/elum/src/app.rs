//! Top-level app shell: sidebar of hosts, tab bar, active terminal view.
//!
//! The app entity owns:
//! - The persistent `HostBook` (loaded from disk at boot).
//! - A `Vec<Tab>` of currently-open connections, each in one of three states
//!   (`Connecting`, `Connected`, `Failed`).
//! - A handle to the tokio runtime so it can spawn SSH connection tasks
//!   without blocking the GPUI main thread.
//!
//! Each new tab spawns a tokio task to `Session::connect` + `open_shell`.
//! When the task resolves, it updates the tab's state via `WeakEntity`
//! back on the GPUI main thread, materializing a `TerminalView` entity.
//!
//! Keyboard:
//! - Bindings under `key_context("ElumApp")` cover app-wide shortcuts
//!   (close tab, switch tabs).
//! - Bindings under `key_context("TerminalView")` (registered separately)
//!   cover in-terminal copy/paste/select-all.
//! - Action dispatch bubbles outward, so a focused terminal still receives
//!   app-level shortcuts that don't conflict.

use std::sync::Arc;

use elum_ssh::{ConnectConfig, Session, ShellHandle};
use elum_terminal::view::TerminalView;
use elum_terminal::{GridSize, Terminal};
use gpui::{
  actions, div, px, rgb, AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable,
  InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, SharedString,
  StatefulInteractiveElement, Styled, Window,
};
use tokio::runtime::Runtime;

use crate::assets::{Icon, IconName};
use crate::host_book::{Host, HostBook};

actions!(
  elum,
  [
    /// Close the currently-active tab.
    CloseTab,
    /// Switch to the next tab (wraps around).
    NextTab,
    /// Switch to the previous tab (wraps around).
    PrevTab,
  ]
);

/// Key context for app-level bindings. Bindings registered under this
/// context fire from anywhere within the app (including focused terminals).
pub const KEY_CONTEXT: &str = "ElumApp";

pub fn register_default_keybindings(cx: &mut App) {
  cx.bind_keys([
    KeyBinding::new("cmd-w", CloseTab, Some(KEY_CONTEXT)),
    KeyBinding::new("cmd-shift-]", NextTab, Some(KEY_CONTEXT)),
    KeyBinding::new("cmd-shift-[", PrevTab, Some(KEY_CONTEXT)),
  ]);
}

const SIDEBAR_WIDTH_PX: f32 = 200.0;
const TAB_BAR_HEIGHT_PX: f32 = 30.0;
const SIDEBAR_BG: u32 = 0x1a1d23;
const HOVER_BG: u32 = 0x252830;
const ACCENT_BG: u32 = 0x101418;
const TEXT_COLOR: u32 = 0xe6e6e6;
const MUTED_TEXT_COLOR: u32 = 0x8b9099;
const ERROR_TEXT_COLOR: u32 = 0xe06c75;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

pub struct ElumApp {
  host_book: HostBook,
  tabs: Vec<Tab>,
  active_tab: Option<usize>,
  runtime: Arc<Runtime>,
  next_tab_id: u64,
  focus: FocusHandle,
}

struct Tab {
  /// Stable across reorderings; used by spawned tasks to find their tab
  /// after the user may have closed/moved siblings.
  id: u64,
  host: Host,
  state: TabState,
}

enum TabState {
  Connecting,
  Connected { view: Entity<TerminalView> },
  Failed { error: String },
}

impl ElumApp {
  pub fn new(host_book: HostBook, runtime: Arc<Runtime>, cx: &mut Context<Self>) -> Self {
    Self {
      host_book,
      tabs: Vec::new(),
      active_tab: None,
      runtime,
      next_tab_id: 1,
      focus: cx.focus_handle(),
    }
  }

  fn connect_to_host(&mut self, host_idx: usize, cx: &mut Context<Self>) {
    let Some(host) = self.host_book.hosts().get(host_idx).cloned() else {
      return;
    };
    let id = self.next_tab_id;
    self.next_tab_id += 1;
    self.tabs.push(Tab {
      id,
      host: host.clone(),
      state: TabState::Connecting,
    });
    self.active_tab = Some(self.tabs.len() - 1);
    cx.notify();

    let runtime = self.runtime.clone();
    cx.spawn(async move |this, cx| {
      // Connect on the tokio runtime, russh requires it. We await
      // the JoinHandle from inside a GPUI task, which is fine: flume
      // and tokio JoinHandles are runtime-agnostic at the await-site.
      let join = runtime.spawn(async move {
        let cfg = ConnectConfig::new(&host.host, host.port, &host.user, &host.key_path);
        let session = Session::connect(&cfg).await?;
        let shell = session.open_shell(INITIAL_COLS, INITIAL_ROWS).await?;
        Ok::<ShellHandle, anyhow::Error>(shell)
      });

      let shell_result = match join.await {
        Ok(Ok(shell)) => Ok(shell),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("task join error: {e}")),
      };

      let _ = this.update(cx, move |this, cx| {
        this.finalize_tab(id, shell_result, cx);
      });
    })
    .detach();
  }

  fn finalize_tab(
    &mut self,
    tab_id: u64,
    result: std::result::Result<ShellHandle, String>,
    cx: &mut Context<Self>,
  ) {
    let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
      // Tab was closed while connecting, drop the shell silently.
      return;
    };
    match result {
      Ok(shell) => {
        let terminal = Arc::new(Terminal::new(GridSize::new(INITIAL_ROWS, INITIAL_COLS)));
        let from_remote = shell.from_remote.clone();
        let to_remote = shell.to_remote.clone();
        let resize_remote = shell.resize.clone();
        let view = cx.new(move |cx| {
          TerminalView::new(terminal, from_remote, to_remote, resize_remote, shell, cx)
        });
        self.tabs[idx].state = TabState::Connected { view };
      }
      Err(error) => {
        self.tabs[idx].state = TabState::Failed { error };
      }
    }
    cx.notify();
  }

  fn activate_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
    if idx < self.tabs.len() && self.active_tab != Some(idx) {
      self.active_tab = Some(idx);
      cx.notify();
    }
  }

  fn close_tab_at(&mut self, idx: usize, cx: &mut Context<Self>) {
    if idx >= self.tabs.len() {
      return;
    }
    self.tabs.remove(idx);
    self.active_tab = if self.tabs.is_empty() {
      None
    } else {
      Some(self.active_tab.map_or(0, |a| a.min(self.tabs.len() - 1)))
    };
    cx.notify();
  }

  fn on_close_tab(&mut self, _: &CloseTab, _w: &mut Window, cx: &mut Context<Self>) {
    if let Some(idx) = self.active_tab {
      self.close_tab_at(idx, cx);
    }
  }

  fn on_next_tab(&mut self, _: &NextTab, _w: &mut Window, cx: &mut Context<Self>) {
    if self.tabs.is_empty() {
      return;
    }
    let next = self.active_tab.map_or(0, |a| (a + 1) % self.tabs.len());
    self.activate_tab(next, cx);
  }

  fn on_prev_tab(&mut self, _: &PrevTab, _w: &mut Window, cx: &mut Context<Self>) {
    if self.tabs.is_empty() {
      return;
    }
    let len = self.tabs.len();
    let prev = self.active_tab.map_or(len - 1, |a| (a + len - 1) % len);
    self.activate_tab(prev, cx);
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
    let header = div()
      .px_3()
      .py_2()
      .text_color(rgb(MUTED_TEXT_COLOR))
      .child(SharedString::from("Hosts"));

    let mut sidebar = div()
      .flex()
      .flex_col()
      .w(px(SIDEBAR_WIDTH_PX))
      .h_full()
      .bg(rgb(SIDEBAR_BG))
      .child(header);

    if self.host_book.is_empty() {
      sidebar = sidebar.child(
        div()
          .px_3()
          .py_2()
          .text_color(rgb(MUTED_TEXT_COLOR))
          .child("No hosts yet."),
      );
    } else {
      for (i, host) in self.host_book.hosts().iter().enumerate() {
        let row = div()
          .id(("host-row", i))
          .px_3()
          .py_1p5()
          .cursor_pointer()
          .hover(|s| s.bg(rgb(HOVER_BG)))
          .on_click(cx.listener(move |this, _, _, cx| this.connect_to_host(i, cx)))
          .child(SharedString::from(host.name.clone()));
        sidebar = sidebar.child(row);
      }
    }

    sidebar.into_any_element()
  }

  fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
    let mut bar = div()
      .flex()
      .flex_row()
      .h(px(TAB_BAR_HEIGHT_PX))
      .bg(rgb(SIDEBAR_BG));

    for (i, tab) in self.tabs.iter().enumerate() {
      let is_active = self.active_tab == Some(i);
      let title = SharedString::from(tab.host.name.clone());
      let entry = div()
        .id(("tab", tab.id as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .h_full()
        .cursor_pointer()
        .bg(if is_active {
          rgb(ACCENT_BG)
        } else {
          rgb(SIDEBAR_BG)
        })
        .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(i, cx)))
        .child(title)
        .child(
          div()
            .id(("tab-close", tab.id as usize))
            .px_1()
            .text_color(rgb(MUTED_TEXT_COLOR))
            .hover(|s| s.text_color(rgb(TEXT_COLOR)))
            .on_click(cx.listener(move |this, _, _, cx| {
              this.close_tab_at(i, cx);
            }))
            .child(Icon::new(IconName::X).size(px(12.))),
        );
      bar = bar.child(entry);
    }
    bar.into_any_element()
  }

  fn render_active_body(&self, _cx: &mut Context<Self>) -> AnyElement {
    let Some(idx) = self.active_tab else {
      return div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_color(rgb(MUTED_TEXT_COLOR))
        .child("Click a host in the sidebar to connect.")
        .into_any_element();
    };
    let Some(tab) = self.tabs.get(idx) else {
      return div().flex_1().into_any_element();
    };
    match &tab.state {
      TabState::Connecting => div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_color(rgb(MUTED_TEXT_COLOR))
        .child(SharedString::from(format!(
          "Connecting to {}…",
          tab.host.name
        )))
        .into_any_element(),
      TabState::Failed { error } => div()
        .flex()
        .flex_1()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .text_color(rgb(ERROR_TEXT_COLOR))
        .child(SharedString::from(format!(
          "Failed to connect to {}",
          tab.host.name
        )))
        .child(
          div()
            .text_color(rgb(MUTED_TEXT_COLOR))
            .child(SharedString::from(error.clone())),
        )
        .into_any_element(),
      TabState::Connected { view } => view.clone().into_any_element(),
    }
  }
}

impl Focusable for ElumApp {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus.clone()
  }
}

impl Render for ElumApp {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    div()
      .key_context(KEY_CONTEXT)
      .track_focus(&self.focus)
      .on_action(cx.listener(Self::on_close_tab))
      .on_action(cx.listener(Self::on_next_tab))
      .on_action(cx.listener(Self::on_prev_tab))
      .flex()
      .flex_row()
      .size_full()
      .bg(rgb(ACCENT_BG))
      .text_color(rgb(TEXT_COLOR))
      .child(self.render_sidebar(cx))
      .child(
        div()
          .flex()
          .flex_col()
          .flex_1()
          .child(self.render_tab_bar(cx))
          .child(self.render_active_body(cx)),
      )
  }
}
