use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
  actions, div, px, relative, Action, AnyElement, App, AppContext, Context, Entity, FocusHandle,
  Focusable, InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled,
  Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::resizable::{h_resizable, resizable_panel};
use gpui_component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_component::tab::{Tab as ComponentTab, TabBar};
use gpui_component::{ActiveTheme, Sizable, StyledExt, TitleBar};
use serde::Deserialize;
use ssh::{AuthMethod, ConnectConfig, Session, ShellHandle};
use terminal::view::{TerminalEvent, TerminalView};
use terminal::{GridSize, Terminal};
use tokio::runtime::Runtime;

use crate::host_book::{Host, HostAuth, HostBook};
use crate::keychain;
use ui::add_host_dialog::{self, NewAuth, NewHostInput};
use ui::secret_prompt::{self, SecretPrompt};
use ui::UiIconName;

actions!(
  elum,
  [
    /// Close the currently-active tab.
    CloseTab,
    /// Switch to the next tab (wraps around).
    NextTab,
    /// Switch to the previous tab (wraps around).
    PrevTab,
    /// Quit the application.
    Quit,
  ]
);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = elum, no_json)]
pub struct EditHost(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = elum, no_json)]
pub struct DeleteHost(pub SharedString);

/// Key context for app-level bindings. Bindings registered under this
/// context fire from anywhere within the app (including focused terminals).
/// Keystroke->action wiring lives in [`crate::keymap`].
pub const KEY_CONTEXT: &str = "Workspace";

const SIDEBAR_DEFAULT_WIDTH: f32 = 200.0;
const SIDEBAR_MIN_WIDTH: f32 = 160.0;
const SIDEBAR_MAX_WIDTH: f32 = 400.0;
const TAB_BAR_HEIGHT_PX: f32 = 30.0;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

pub struct Workspace {
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
  /// Subscription to the [`TerminalView`]'s `ShellClosed` event. Held in
  /// the tab so it stays alive for as long as the tab does; dropped on
  /// tab close.
  _shell_closed: Option<Subscription>,
}

enum TabState {
  Connecting,
  Connected { view: Entity<TerminalView> },
  Failed { error: String },
}

impl Workspace {
  pub fn new(
    host_book: HostBook,
    runtime: Arc<Runtime>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let focus = cx.focus_handle();
    // Focus the app on init so keybindings dispatch immediately, before
    // the user has clicked anything. When a tab becomes active we hand
    // focus down to its TerminalView.
    window.focus(&focus, cx);
    Self {
      host_book,
      tabs: Vec::new(),
      active_tab: None,
      runtime,
      next_tab_id: 1,
      focus,
    }
  }

  /// Focus the active tab's `TerminalView`, if any. Falls back to the app
  /// focus handle when there is no active tab or the tab isn't connected
  /// yet, so keystrokes always have somewhere to land.
  fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
    let view_focus = self
      .active_tab
      .and_then(|i| self.tabs.get(i))
      .and_then(|tab| match &tab.state {
        TabState::Connected { view } => Some(view.read(cx).focus_handle(cx)),
        _ => None,
      });
    match view_focus {
      Some(handle) => window.focus(&handle, cx),
      None => window.focus(&self.focus, cx),
    }
  }

  fn connect_to_host(&mut self, host_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
    let Some(host) = self.host_book.hosts().get(host_idx).cloned() else {
      return;
    };
    let tab_id = self.next_tab_id;
    self.next_tab_id += 1;
    self.tabs.push(Tab {
      id: tab_id,
      host: host.clone(),
      state: TabState::Connecting,
      _shell_closed: None,
    });
    self.active_tab = Some(self.tabs.len() - 1);
    cx.notify();

    match &host.auth {
      HostAuth::PublicKey {
        key_path,
        passphrase_in_keychain,
      } => {
        let passphrase = if *passphrase_in_keychain {
          keychain::fetch(&host.id, keychain::PASSPHRASE)
        } else {
          None
        };
        let auth = AuthMethod::PublicKey {
          key_path: key_path.clone(),
          passphrase,
        };
        self.spawn_connect(tab_id, host, auth, window, cx);
      }
      HostAuth::Password { in_keychain } => {
        if *in_keychain {
          if let Some(pw) = keychain::fetch(&host.id, keychain::PASSWORD) {
            let auth = AuthMethod::Password { password: pw };
            self.spawn_connect(tab_id, host, auth, window, cx);
            return;
          }
        }
        self.prompt_password_then_connect(tab_id, host, window, cx);
      }
    }
  }

  fn prompt_password_then_connect(
    &mut self,
    tab_id: u64,
    host: Host,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let view = cx.entity().downgrade();
    let host_name = host.name.clone();
    secret_prompt::open(
      window,
      cx,
      SecretPrompt {
        title: format!("Password for {host_name}").into(),
        label: "Password".into(),
        placeholder: "SSH password".into(),
        save_label: "Save in Keychain".into(),
        confirm_label: "Connect".into(),
      },
      move |result, window, cx| {
        let host = host.clone();
        let _ = view.update(cx, |this, cx| match result {
          Some(submit) => {
            if submit.save_in_keychain {
              match keychain::store(&host.id, keychain::PASSWORD, &submit.secret) {
                Ok(()) => this.mark_password_saved(&host.id),
                Err(e) => eprintln!("warning: keychain store for {} failed: {e:#}", host.id),
              }
            }
            let auth = AuthMethod::Password {
              password: submit.secret,
            };
            this.spawn_connect(tab_id, host, auth, window, cx);
          }
          None => {
            this.close_tab_by_id(tab_id, window, cx);
          }
        });
      },
    );
  }

  /// Drop the tab identified by `tab_id`, regardless of its current
  /// position in `self.tabs`. No-op if the tab no longer exists (e.g.,
  /// the user closed it manually before this fired).
  fn close_tab_by_id(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) {
      self.close_tab_at(idx, window, cx);
    }
  }

  fn spawn_connect(
    &mut self,
    tab_id: u64,
    host: Host,
    auth: AuthMethod,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let runtime = self.runtime.clone();
    cx.spawn_in(window, async move |this, cx| {
      let host_for_task = host.clone();
      let auth_for_task = auth.clone();
      let join = runtime.spawn(async move {
        let cfg = ConnectConfig::new(
          &host_for_task.host,
          host_for_task.port,
          &host_for_task.user,
          auth_for_task,
        );
        let session = Session::connect(&cfg).await?;
        let shell = session.open_shell(INITIAL_COLS, INITIAL_ROWS).await?;
        Ok::<ShellHandle, anyhow::Error>(shell)
      });

      match join.await {
        Ok(Ok(shell)) => {
          let _ = this.update_in(cx, move |this, window, cx| {
            this.finalize_tab(tab_id, Ok(shell), window, cx);
            this.focus_active(window, cx);
          });
        }
        Ok(Err(err)) => {
          let is_pubkey = matches!(auth, AuthMethod::PublicKey { .. });
          if is_pubkey && is_encrypted_key_error(&err) {
            let _ = this.update_in(cx, move |_this, window, cx| {
              let view = cx.entity().downgrade();
              let host_name = host.name.clone();
              let key_path = match &host.auth {
                HostAuth::PublicKey { key_path, .. } => key_path.clone(),
                HostAuth::Password { .. } => return,
              };
              secret_prompt::open(
                window,
                cx,
                SecretPrompt {
                  title: format!("Unlock key for {host_name}").into(),
                  label: "Passphrase".into(),
                  placeholder: "SSH key passphrase".into(),
                  save_label: "Save in Keychain".into(),
                  confirm_label: "Unlock".into(),
                },
                move |result, window, cx| {
                  let host = host.clone();
                  let key_path = key_path.clone();
                  let _ = view.update(cx, |this, cx| match result {
                    Some(submit) => {
                      if submit.save_in_keychain {
                        match keychain::store(&host.id, keychain::PASSPHRASE, &submit.secret) {
                          Ok(()) => this.mark_passphrase_saved(&host.id),
                          Err(e) => {
                            eprintln!("warning: keychain store for {} failed: {e:#}", host.id);
                          }
                        }
                      }
                      let auth = AuthMethod::PublicKey {
                        key_path,
                        passphrase: Some(submit.secret),
                      };
                      this.spawn_connect(tab_id, host, auth, window, cx);
                    }
                    None => {
                      this.close_tab_by_id(tab_id, window, cx);
                    }
                  });
                },
              );
            });
          } else {
            let msg = format!("{err:#}");
            let _ = this.update_in(cx, move |this, window, cx| {
              this.finalize_tab(tab_id, Err(msg), window, cx);
            });
          }
        }
        Err(e) => {
          let msg = format!("task join error: {e}");
          let _ = this.update_in(cx, move |this, window, cx| {
            this.finalize_tab(tab_id, Err(msg), window, cx);
          });
        }
      }
    })
    .detach();
  }

  fn mark_passphrase_saved(&mut self, host_id: &str) {
    self.update_host_in_place(host_id, |host| {
      if let HostAuth::PublicKey {
        passphrase_in_keychain,
        ..
      } = &mut host.auth
      {
        if !*passphrase_in_keychain {
          *passphrase_in_keychain = true;
          return true;
        }
      }
      false
    });
  }

  fn mark_password_saved(&mut self, host_id: &str) {
    self.update_host_in_place(host_id, |host| {
      if let HostAuth::Password { in_keychain } = &mut host.auth {
        if !*in_keychain {
          *in_keychain = true;
          return true;
        }
      }
      false
    });
  }

  fn update_host_in_place(&mut self, host_id: &str, mutate: impl FnOnce(&mut Host) -> bool) {
    let Some(idx) = self.host_book.hosts().iter().position(|h| h.id == host_id) else {
      return;
    };
    let mut host = self.host_book.hosts()[idx].clone();
    if mutate(&mut host) {
      self.host_book.replace(idx, host);
      self.persist_host_book();
    }
  }

  fn finalize_tab(
    &mut self,
    tab_id: u64,
    result: std::result::Result<ShellHandle, String>,
    window: &mut Window,
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

        let subscription = cx.subscribe_in(
          &view,
          window,
          move |this, _, ev: &TerminalEvent, window, cx| match ev {
            TerminalEvent::ShellClosed => {
              if let Some(idx) = this.tabs.iter().position(|t| t.id == tab_id) {
                this.close_tab_at(idx, window, cx);
              }
            }
          },
        );
        self.tabs[idx].state = TabState::Connected { view };
        self.tabs[idx]._shell_closed = Some(subscription);
      }
      Err(error) => {
        self.tabs[idx].state = TabState::Failed { error };
      }
    }
    cx.notify();
  }

  fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
    if idx < self.tabs.len() && self.active_tab != Some(idx) {
      self.active_tab = Some(idx);
      self.focus_active(window, cx);
      cx.notify();
    }
  }

  fn close_tab_at(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
    if idx >= self.tabs.len() {
      return;
    }
    self.tabs.remove(idx);
    self.active_tab = if self.tabs.is_empty() {
      None
    } else {
      Some(self.active_tab.map_or(0, |a| a.min(self.tabs.len() - 1)))
    };
    self.focus_active(window, cx);
    cx.notify();
  }

  fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(idx) = self.active_tab {
      self.close_tab_at(idx, window, cx);
    }
  }

  fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
    if self.tabs.is_empty() {
      return;
    }
    let next = self.active_tab.map_or(0, |a| (a + 1) % self.tabs.len());
    self.activate_tab(next, window, cx);
  }

  fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
    if self.tabs.is_empty() {
      return;
    }
    let len = self.tabs.len();
    let prev = self.active_tab.map_or(len - 1, |a| (a + len - 1) % len);
    self.activate_tab(prev, window, cx);
  }

  fn on_quit(&mut self, _: &Quit, _w: &mut Window, cx: &mut Context<Self>) {
    cx.quit();
  }

  pub(crate) fn add_host_from_dialog(&mut self, input: NewHostInput, cx: &mut Context<Self>) {
    let id = generate_host_id();
    let auth = build_host_auth(&id, input.auth, &HostAuth::Password { in_keychain: false });
    let host = Host {
      id,
      name: input.name,
      host: input.host,
      port: input.port,
      user: input.user,
      auth,
    };
    self.host_book.add(host);
    self.persist_host_book();
    cx.notify();
  }

  pub(crate) fn replace_host_from_dialog(
    &mut self,
    host_id: &str,
    input: NewHostInput,
    cx: &mut Context<Self>,
  ) {
    let Some(idx) = self.host_book.hosts().iter().position(|h| h.id == host_id) else {
      return;
    };
    let existing = self.host_book.hosts()[idx].clone();
    purge_obsolete_keychain_entries(&existing.auth, &input.auth, &existing.id);
    let auth = build_host_auth(&existing.id, input.auth, &existing.auth);
    let host = Host {
      id: existing.id,
      name: input.name,
      host: input.host,
      port: input.port,
      user: input.user,
      auth,
    };
    self.host_book.replace(idx, host);
    self.persist_host_book();
    cx.notify();
  }

  pub(crate) fn remove_host_by_id(&mut self, host_id: &str, cx: &mut Context<Self>) {
    let Some(idx) = self.host_book.hosts().iter().position(|h| h.id == host_id) else {
      return;
    };
    keychain::delete(host_id, keychain::PASSPHRASE);
    keychain::delete(host_id, keychain::PASSWORD);
    self.host_book.remove(idx);
    self.persist_host_book();
    cx.notify();
  }

  fn persist_host_book(&self) {
    if let Err(e) = self.host_book.save() {
      eprintln!("warning: failed to persist host book: {e:#}");
    }
  }

  fn on_edit_host(&mut self, action: &EditHost, window: &mut Window, cx: &mut Context<Self>) {
    let host_id = action.0.to_string();
    let Some(host) = self.host_book.hosts().iter().find(|h| h.id == host_id) else {
      return;
    };
    let auth = match &host.auth {
      HostAuth::PublicKey { key_path, .. } => NewAuth::PublicKey {
        key_path: key_path.clone(),
        passphrase: None,
      },
      HostAuth::Password { .. } => NewAuth::Password {
        password: String::new(),
      },
    };
    let initial = NewHostInput {
      name: host.name.clone(),
      host: host.host.clone(),
      port: host.port,
      user: host.user.clone(),
      auth,
    };
    let view = cx.entity().downgrade();
    add_host_dialog::open(window, cx, Some(initial), move |input, cx| {
      let host_id = host_id.clone();
      let _ = view.update(cx, |this, cx| {
        this.replace_host_from_dialog(&host_id, input, cx);
      });
    });
  }

  fn on_delete_host(&mut self, action: &DeleteHost, window: &mut Window, cx: &mut Context<Self>) {
    use gpui_component::{button::ButtonVariant, dialog::DialogButtonProps, WindowExt as _};

    let host_id = action.0.to_string();
    let Some(host) = self.host_book.hosts().iter().find(|h| h.id == host_id) else {
      return;
    };
    let host_name = host.name.clone();
    let view = cx.entity().downgrade();

    window.open_alert_dialog(cx, move |alert, _, _| {
      let view = view.clone();
      let host_id = host_id.clone();

      alert
        .title("Remove host?")
        .description(SharedString::from(format!(
          "Permanently remove \"{host_name}\" from your hosts. Open tabs stay connected."
        )))
        .close_button(true)
        .overlay_closable(true)
        .button_props(
          DialogButtonProps::default()
            .show_cancel(true)
            .ok_text("Remove")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
              let host_id = host_id.clone();
              let _ = view.update(cx, |this, cx| {
                this.remove_host_by_id(&host_id, cx);
              });
              true
            }),
        )
    });
  }

  fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
    let view = cx.entity().downgrade();
    let theme = cx.theme();

    let add_button = Button::new("add-host")
      .icon(UiIconName::Plus)
      .ghost()
      .small()
      .tooltip("Add host")
      .on_click({
        let view = view.clone();
        move |_, window, cx| {
          let view = view.clone();
          add_host_dialog::open(window, cx, None, move |input, cx| {
            let _ = view.update(cx, |app, cx| {
              app.add_host_from_dialog(input, cx);
            });
          });
        }
      });

    let header = div()
      .h_flex()
      .gap_2()
      .p_2()
      .w_full()
      .justify_between()
      .child(
        div()
          .flex_1()
          .text_color(theme.muted_foreground)
          .child("Hosts"),
      )
      .child(add_button);

    let menu = if self.host_book.is_empty() {
      SidebarMenu::new().child(SidebarMenuItem::new("No hosts yet.").disable(true))
    } else {
      SidebarMenu::new().children(self.host_book.hosts().iter().enumerate().map(|(i, host)| {
        let view_for_click = view.clone();
        let host_id = SharedString::from(host.id.clone());
        SidebarMenuItem::new(SharedString::from(host.name.clone()))
          .icon(UiIconName::Server)
          .on_click(move |_, window, cx| {
            let _ = view_for_click.update(cx, |this, cx| {
              this.connect_to_host(i, window, cx);
            });
          })
          .context_menu(move |menu, _, _| {
            menu
              .menu("Edit", Box::new(EditHost(host_id.clone())))
              .separator()
              .menu("Delete", Box::new(DeleteHost(host_id.clone())))
          })
      }))
    };

    Sidebar::new("hosts")
      .w(relative(1.))
      .border_0()
      .header(header)
      .child(menu)
      .bg(theme.sidebar)
      .into_any_element()
  }

  fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
    if self.tabs.is_empty() {
      // Empty bar still gets rendered (for height stability) but with no tabs inside.
      return div().h(px(TAB_BAR_HEIGHT_PX)).into_any_element();
    }

    let mut bar = TabBar::new("workspace-tabs")
      .selected_index(self.active_tab.unwrap_or(0))
      .on_click(cx.listener(|this, ix: &usize, window, cx| {
        this.activate_tab(*ix, window, cx);
      }));

    for (i, tab) in self.tabs.iter().enumerate() {
      let close_id = ("tab-close", tab.id as usize);
      let close_button = Button::new(close_id)
        .icon(UiIconName::X)
        .ghost()
        .xsmall()
        .mr_1()
        .on_click(cx.listener(move |this, _, window, cx| {
          this.close_tab_at(i, window, cx);
        }));

      bar = bar.child(
        ComponentTab::new()
          .label(SharedString::from(tab.host.name.clone()))
          .suffix(close_button),
      );
    }

    bar.into_any_element()
  }

  fn render_active_body(&self, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme();
    let muted_fg = theme.muted_foreground;
    let danger = theme.danger;

    let Some(idx) = self.active_tab else {
      return div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_color(muted_fg)
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
        .text_color(muted_fg)
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
        .text_color(danger)
        .child(SharedString::from(format!(
          "Failed to connect to {}",
          tab.host.name
        )))
        .child(
          div()
            .text_color(muted_fg)
            .child(SharedString::from(error.clone())),
        )
        .into_any_element(),
      TabState::Connected { view } => view.clone().into_any_element(),
    }
  }
}

impl Focusable for Workspace {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus.clone()
  }
}

impl Render for Workspace {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let bg = cx.theme().accent;
    let fg = cx.theme().foreground;
    let titlebar_bg = cx.theme().sidebar;

    let main = div()
      .flex()
      .flex_col()
      .flex_1()
      .h_full()
      .child(self.render_tab_bar(cx))
      .child(self.render_active_body(cx))
      .into_any_element();

    div()
      .key_context(KEY_CONTEXT)
      .track_focus(&self.focus)
      .on_action(cx.listener(Self::on_close_tab))
      .on_action(cx.listener(Self::on_next_tab))
      .on_action(cx.listener(Self::on_prev_tab))
      .on_action(cx.listener(Self::on_quit))
      .on_action(cx.listener(Self::on_edit_host))
      .on_action(cx.listener(Self::on_delete_host))
      .flex()
      .flex_col()
      .size_full()
      .bg(bg)
      .text_color(fg)
      .child(TitleBar::new().bg(titlebar_bg))
      .child(
        h_resizable("workspace-split")
          .child(
            resizable_panel()
              .size(px(SIDEBAR_DEFAULT_WIDTH))
              .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
              .child(self.render_sidebar(cx)),
          )
          .child(main),
      )
  }
}

/// True when the connect failure was caused by a missing/wrong passphrase
/// for an encrypted key. Matched on the error chain since russh wraps the
/// `russh::keys::Error::KeyIsEncrypted` variant inside an anyhow context.
fn is_encrypted_key_error(err: &anyhow::Error) -> bool {
  err
    .chain()
    .any(|cause| cause.to_string().contains("The key is encrypted"))
}

/// Build the persisted `HostAuth` from a dialog submission, writing any
/// freshly-supplied secrets to the keychain. Falls back to the previous
/// auth's `*_in_keychain` flag when the user kept the same auth mode and
/// did not retype the secret.
fn build_host_auth(host_id: &str, input: NewAuth, previous: &HostAuth) -> HostAuth {
  match input {
    NewAuth::PublicKey {
      key_path,
      passphrase,
    } => {
      let mut passphrase_in_keychain = matches!(
        previous,
        HostAuth::PublicKey {
          passphrase_in_keychain: true,
          ..
        }
      );
      if let Some(p) = passphrase.as_deref() {
        match keychain::store(host_id, keychain::PASSPHRASE, p) {
          Ok(()) => passphrase_in_keychain = true,
          Err(e) => eprintln!("warning: keychain store for {host_id} failed: {e:#}"),
        }
      }
      HostAuth::PublicKey {
        key_path,
        passphrase_in_keychain,
      }
    }
    NewAuth::Password { password } => {
      let mut in_keychain = matches!(previous, HostAuth::Password { in_keychain: true });
      match keychain::store(host_id, keychain::PASSWORD, &password) {
        Ok(()) => in_keychain = true,
        Err(e) => eprintln!("warning: keychain store for {host_id} failed: {e:#}"),
      }
      HostAuth::Password { in_keychain }
    }
  }
}

/// When the user switches auth modes on edit, drop the now-unused secret
/// from the keychain. Same-mode edits leave the existing entry alone
fn purge_obsolete_keychain_entries(prev: &HostAuth, new: &NewAuth, host_id: &str) {
  match (prev, new) {
    (HostAuth::PublicKey { .. }, NewAuth::Password { .. }) => {
      keychain::delete(host_id, keychain::PASSPHRASE);
    }
    (HostAuth::Password { .. }, NewAuth::PublicKey { .. }) => {
      keychain::delete(host_id, keychain::PASSWORD);
    }
    _ => {}
  }
}

/// Generate a stable-enough host ID from the wall clock
/// Collisions are effectively impossible for human-paced "Add Host" use
fn generate_host_id() -> String {
  let ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis())
    .unwrap_or(0);
  format!("host-{ms}")
}
