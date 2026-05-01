//! Modal "Add Host" form.
//!
//! Uses `gpui_component::dialog::Dialog` directly (the general-purpose
//! modal primitive) rather than `AlertDialog` / [`crate::ConfirmDialog`].
//! `Dialog` is the right primitive for a form modal: it has explicit
//! Header / Body / Footer slots, right-aligned action buttons, and full
//! layout control.
//!
//! `AlertDialog` (and the `ConfirmDialog` builder we keep around for
//! destructive actions) is for confirm-style modals where buttons are
//! center-aligned and content is description text. Forms don't fit that
//! shape.
//!
//! The dialog is domain-agnostic: it collects five string fields and
//! hands them back via [`NewHostInput`]. The caller assigns IDs,
//! persists, refreshes the sidebar, etc.
//!
//! Validation: name + host + user + key required, port must parse as a
//! non-zero `u16`. Returning `false` from `on_ok` keeps the dialog open
//! so the user can fix invalid input.

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

/// The validated form values returned on submit. Kept intentionally
/// narrow: just the data the form collects, not a domain `Host` (the
/// caller assigns IDs and any extra metadata).
#[derive(Debug, Clone)]
pub struct NewHostInput {
  pub name: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub key_path: PathBuf,
}

/// Open the "Add Host" modal. `on_submit` runs on the GPUI main thread
/// when the user confirms with valid input; it is given the form values
/// plus a `&mut App` so it can update entities, write files, etc.
pub fn open<F>(window: &mut Window, cx: &mut App, on_submit: F)
where
  F: Fn(NewHostInput, &mut App) + 'static,
{
  // The submit handler is `Fn` (the dialog may rebuild between frames).
  // Wrap in `Rc` so it stays cheaply cloneable as it's captured into
  // nested closures.
  let on_submit = Rc::new(on_submit);

  let name = cx.new(|cx| InputState::new(window, cx).placeholder("My VPS"));
  let host = cx.new(|cx| InputState::new(window, cx).placeholder("example.com or 1.2.3.4"));
  let port = cx.new(|cx| InputState::new(window, cx).default_value("22"));
  let user = cx.new(|cx| InputState::new(window, cx).placeholder("root"));
  let key = cx.new(|cx| InputState::new(window, cx).placeholder("/Users/you/.ssh/id_ed25519"));

  window.open_dialog(cx, move |dialog, _, _| {
    let name = name.clone();
    let host = host.clone();
    let port = port.clone();
    let user = user.clone();
    let key = key.clone();
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
      );

    dialog
      .w(px(440.))
      .title("Add Host")
      .child(body)
      .footer(
        DialogFooter::new()
          .gap_2()
          .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
          .child(DialogAction::new().child(Button::new("save").primary().label("Save"))),
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

        if name_v.is_empty() || host_v.is_empty() || user_v.is_empty() || key_v.is_empty() {
          return false;
        }

        let input = NewHostInput {
          name: name_v,
          host: host_v,
          port: port_v,
          user: user_v,
          key_path: PathBuf::from(key_v),
        };
        on_submit(input, cx);
        true
      })
  });
}

fn read_value(state: &Entity<InputState>, cx: &App) -> String {
  state.read(cx).value().to_string()
}
