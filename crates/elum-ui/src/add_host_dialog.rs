use std::path::PathBuf;
use std::rc::Rc;

use gpui::{px, App, AppContext, Entity, ParentElement, Styled, Window};
use gpui_component::{
  button::{Button, ButtonVariants as _},
  dialog::{DialogAction, DialogClose, DialogFooter},
  form::{field, v_form},
  input::{Input, InputState},
  WindowExt as _,
};

#[derive(Debug, Clone)]
pub struct NewHostInput {
  pub name: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub key_path: PathBuf,
  pub key_passphrase: Option<String>,
}

pub fn open<F>(window: &mut Window, cx: &mut App, initial: Option<NewHostInput>, on_submit: F)
where
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
  let key_default = initial
    .as_ref()
    .map(|i| i.key_path.to_string_lossy().to_string());
  let passphrase_default = initial.as_ref().and_then(|i| i.key_passphrase.clone());

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
    let s = InputState::new(window, cx)
      .masked(true)
      .placeholder("Leave empty if key is unencrypted");
    if let Some(v) = passphrase_default {
      s.default_value(v)
    } else {
      s
    }
  });

  window.open_dialog(cx, move |dialog, _, _| {
    let name = name.clone();
    let host = host.clone();
    let port = port.clone();
    let user = user.clone();
    let key = key.clone();
    let passphrase = passphrase.clone();
    let on_submit = on_submit.clone();

    let body = v_form()
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

    dialog
      .w(px(440.))
      .title(title)
      .child(body)
      .footer(
        DialogFooter::new()
          .gap_2()
          .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
          .child(DialogAction::new().child(Button::new("save").primary().label(confirm_label))),
      )
      .on_ok(move |_, _window, cx| {
        let name_v = read_value(&name, cx);
        let host_v = read_value(&host, cx);
        let port_v: u16 = match read_value(&port, cx).parse() {
          Ok(p) if p > 0 => p,
          _ => return false,
        };
        let user_v = read_value(&user, cx);
        let key_v = read_value(&key, cx);
        let passphrase_v = read_value(&passphrase, cx);

        if name_v.is_empty() || host_v.is_empty() || user_v.is_empty() || key_v.is_empty() {
          return false;
        }

        let input = NewHostInput {
          name: name_v,
          host: host_v,
          port: port_v,
          user: user_v,
          key_path: PathBuf::from(key_v),
          key_passphrase: if passphrase_v.is_empty() {
            None
          } else {
            Some(passphrase_v)
          },
        };
        on_submit(input, cx);
        true
      })
  });
}

fn read_value(state: &Entity<InputState>, cx: &App) -> String {
  state.read(cx).value().to_string()
}
