use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{px, App, AppContext, Entity, IntoElement, ParentElement, Styled, Window};
use gpui_component::{
  button::{Button, ButtonVariants as _},
  dialog::{CancelDialog, ConfirmDialog, DialogFooter},
  form::{field, v_form},
  input::{Input, InputState},
  select::{Select, SelectState},
  tab::{Tab, TabBar},
  IndexPath, WindowExt as _,
};

#[derive(Debug, Clone)]
pub enum NewAuth {
  PublicKey {
    key_path: PathBuf,
    passphrase: Option<String>,
  },
  Password {
    password: String,
  },
}

#[derive(Debug, Clone)]
pub struct NewHostInput {
  pub name: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub auth: NewAuth,
  /// Name of another host to tunnel through (ProxyJump). `None` = direct.
  pub proxy_jump: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JumpHostOption {
  pub name: String,
}

const MODE_KEY: usize = 0;
const MODE_PASSWORD: usize = 1;

pub fn open<F>(
  window: &mut Window,
  cx: &mut App,
  initial: Option<NewHostInput>,
  jump_hosts: Vec<JumpHostOption>,
  on_submit: F,
) where
  F: Fn(NewHostInput, &mut App) + 'static,
{
  let on_submit = Rc::new(on_submit);
  let is_editing = initial.is_some();
  let title = if is_editing { "Edit Host" } else { "Add Host" };
  let confirm_label = if is_editing { "Save" } else { "Add" };

  let name_default = initial.as_ref().map(|i| i.name.clone());
  let host_default = initial.as_ref().map(|i| i.host.clone());
  let port_default = initial
    .as_ref()
    .map_or_else(|| "22".to_string(), |i| i.port.to_string());
  let user_default = initial.as_ref().map(|i| i.user.clone());

  let initial_mode = match initial.as_ref().map(|i| &i.auth) {
    Some(NewAuth::Password { .. }) => MODE_PASSWORD,
    _ => MODE_KEY,
  };
  let key_default = initial.as_ref().and_then(|i| match &i.auth {
    NewAuth::PublicKey { key_path, .. } => Some(key_path.to_string_lossy().to_string()),
    NewAuth::Password { .. } => None,
  });

  // Index 0 = sentinel; selection read by row index, not label, to allow a host literally named "none".
  let jump_host_names: Vec<String> = jump_hosts.iter().map(|h| h.name.clone()).collect();
  let jump_options: Vec<String> = std::iter::once("none".to_string())
    .chain(jump_host_names.iter().cloned())
    .collect();
  let initial_jump_index = initial
    .as_ref()
    .and_then(|i| i.proxy_jump.as_deref())
    .and_then(|name| {
      jump_hosts
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case(name))
        .map(|p| p + 1)
    })
    .unwrap_or(0);
  let jump_state = cx.new(|cx| {
    SelectState::new(
      jump_options,
      Some(IndexPath::new(initial_jump_index)),
      window,
      cx,
    )
  });

  let mode = Rc::new(Cell::new(initial_mode));

  let name = cx.new(|cx| {
    let s = InputState::new(window, cx).placeholder("My VPS");
    if let Some(v) = name_default {
      s.default_value(v)
    } else {
      s
    }
  });
  let host = cx.new(|cx| {
    let s = InputState::new(window, cx).placeholder("example.com or 1.2.3.4");
    if let Some(v) = host_default {
      s.default_value(v)
    } else {
      s
    }
  });
  let port = cx.new(|cx| InputState::new(window, cx).default_value(port_default));
  let user = cx.new(|cx| {
    let s = InputState::new(window, cx).placeholder("root");
    if let Some(v) = user_default {
      s.default_value(v)
    } else {
      s
    }
  });
  let key = cx.new(|cx| {
    let s = InputState::new(window, cx).placeholder("/Users/you/.ssh/id_ed25519");
    if let Some(v) = key_default {
      s.default_value(v)
    } else {
      s
    }
  });

  let passphrase = cx.new(|cx| {
    InputState::new(window, cx)
      .masked(true)
      .placeholder("Leave empty if key is unencrypted")
  });

  let password = cx.new(|cx| {
    InputState::new(window, cx)
      .masked(true)
      .placeholder("SSH password")
  });

  window.open_dialog(cx, move |dialog, _, _| {
    let name = name.clone();
    let host = host.clone();
    let port = port.clone();
    let user = user.clone();
    let key = key.clone();
    let passphrase = passphrase.clone();
    let password = password.clone();
    let mode = mode.clone();
    let jump_state = jump_state.clone();
    let jump_host_names = jump_host_names.clone();
    let on_submit = on_submit.clone();

    let auth_tabs = TabBar::new("auth-mode")
      .segmented()
      .selected_index(mode.get())
      .child(Tab::new().label("Key"))
      .child(Tab::new().label("Password"))
      .on_click({
        let mode = mode.clone();
        move |ix, window, _| {
          mode.set(*ix);
          window.refresh();
        }
      });

    let mut form = v_form()
      .child(
        field()
          .label("Name")
          .required(true)
          .child(Input::new(&name)),
      )
      .child(
        field()
          .label("Host")
          .required(true)
          .child(Input::new(&host)),
      )
      .child(
        field()
          .label("Port")
          .required(true)
          .child(Input::new(&port)),
      )
      .child(
        field()
          .label("User")
          .required(true)
          .child(Input::new(&user)),
      )
      .child(field().label("Authentication").child(auth_tabs));

    if mode.get() == MODE_PASSWORD {
      form = form.child(
        field()
          .label("Password")
          .required(true)
          .child(Input::new(&password)),
      );
    } else {
      form = form
        .child(
          field()
            .label("Key path")
            .required(true)
            .child(Input::new(&key)),
        )
        .child(
          field()
            .label("Key passphrase")
            .child(Input::new(&passphrase)),
        );
    }

    form = form.child(
      field()
        .label("Via (jump host)")
        .child(Select::new(&jump_state).into_element()),
    );

    dialog
      .w(px(440.))
      .overlay_closable(false)
      .title(title)
      .child(form)
      .footer(
        DialogFooter::new()
          .gap_2()
          .child(
            Button::new("cancel")
              .outline()
              .flex_1()
              .label("Cancel")
              .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(CancelDialog), cx);
              }),
          )
          .child(
            Button::new("save")
              .primary()
              .flex_1()
              .label(confirm_label)
              .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(ConfirmDialog), cx);
              }),
          ),
      )
      .on_ok(move |_, _window, cx| {
        let name_v = read_value(&name, cx);
        let host_v = read_value(&host, cx);
        let port_v: u16 = match read_value(&port, cx).parse() {
          Ok(p) if p > 0 => p,
          _ => return false,
        };
        let user_v = read_value(&user, cx);
        if name_v.is_empty() || host_v.is_empty() || user_v.is_empty() {
          return false;
        }

        let auth = if mode.get() == MODE_PASSWORD {
          let pw = read_value(&password, cx);
          if pw.is_empty() {
            return false;
          }
          NewAuth::Password { password: pw }
        } else {
          let key_v = read_value(&key, cx);
          if key_v.is_empty() {
            return false;
          }
          let pp = read_value(&passphrase, cx);
          NewAuth::PublicKey {
            key_path: PathBuf::from(key_v),
            passphrase: if pp.is_empty() { None } else { Some(pp) },
          }
        };

        let proxy_jump = jump_state
          .read(cx)
          .selected_index(cx)
          .map(|ix| ix.row)
          .filter(|row| *row > 0)
          .and_then(|row| jump_host_names.get(row - 1).cloned());

        let input = NewHostInput {
          name: name_v,
          host: host_v,
          port: port_v,
          user: user_v,
          auth,
          proxy_jump,
        };
        on_submit(input, cx);
        true
      })
  });
}

fn read_value(state: &Entity<InputState>, cx: &App) -> String {
  state.read(cx).value().to_string()
}
