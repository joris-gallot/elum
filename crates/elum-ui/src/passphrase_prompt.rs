use std::cell::Cell;
use std::rc::Rc;

use gpui::{px, App, AppContext, Entity, ParentElement, SharedString, Styled, Window};
use gpui_component::{
  button::{Button, ButtonVariants as _},
  checkbox::Checkbox,
  dialog::{DialogAction, DialogClose, DialogFooter},
  form::{field, v_form},
  input::{Input, InputState},
  WindowExt as _,
};

pub struct PassphraseSubmit {
  pub passphrase: String,
  pub save_in_keychain: bool,
}

pub fn open<F>(window: &mut Window, cx: &mut App, host_name: String, on_submit: F)
where
  F: Fn(PassphraseSubmit, &mut Window, &mut App) + 'static,
{
  let on_submit = Rc::new(on_submit);
  let save = Rc::new(Cell::new(true));

  let passphrase = cx.new(|cx| {
    InputState::new(window, cx)
      .masked(true)
      .placeholder("SSH key passphrase")
  });

  let title = SharedString::from(format!("Unlock key for {host_name}"));

  window.open_dialog(cx, move |dialog, _, _| {
    let passphrase = passphrase.clone();
    let save = save.clone();
    let on_submit = on_submit.clone();

    let body = v_form()
      .child(
        field()
          .label("Passphrase")
          .required(true)
          .child(Input::new(&passphrase)),
      )
      .child(
        field().child(
          Checkbox::new("save-in-keychain")
            .label("Save in Keychain")
            .checked(save.get())
            .on_click({
              let save = save.clone();
              move |checked, _, _| save.set(*checked)
            }),
        ),
      );

    dialog
      .w(px(400.))
      .title(title.clone())
      .child(body)
      .footer(
        DialogFooter::new()
          .gap_2()
          .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
          .child(DialogAction::new().child(Button::new("unlock").primary().label("Unlock"))),
      )
      .on_ok(move |_, window, cx| {
        let value = read_value(&passphrase, cx);
        if value.is_empty() {
          return false;
        }
        on_submit(
          PassphraseSubmit {
            passphrase: value,
            save_in_keychain: save.get(),
          },
          window,
          cx,
        );
        true
      })
  });
}

fn read_value(state: &Entity<InputState>, cx: &App) -> String {
  state.read(cx).value().to_string()
}
