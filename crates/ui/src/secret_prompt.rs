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

pub struct SecretPrompt {
  pub title: SharedString,
  pub label: SharedString,
  pub placeholder: SharedString,
  pub save_label: SharedString,
  pub confirm_label: SharedString,
}

pub struct SecretSubmit {
  pub secret: String,
  pub save_in_keychain: bool,
}

pub fn open<F>(window: &mut Window, cx: &mut App, prompt: SecretPrompt, on_done: F)
where
  // `None` means the user dismissed the dialog without submitting (cancel
  // button, escape, click outside). Caller should handle teardown.
  F: Fn(Option<SecretSubmit>, &mut Window, &mut App) + 'static,
{
  let on_done = Rc::new(on_done);
  let save = Rc::new(Cell::new(true));
  // Flipped to `true` once `on_ok` has fired the submit callback, so the
  // close-time callback below can tell submit-then-close from cancel.
  let submitted = Rc::new(Cell::new(false));

  let placeholder = prompt.placeholder.clone();
  let secret = cx.new(|cx| {
    InputState::new(window, cx)
      .masked(true)
      .placeholder(placeholder.clone())
  });

  window.open_dialog(cx, move |dialog, _, _| {
    let secret = secret.clone();
    let save = save.clone();
    let submitted = submitted.clone();
    let on_done_for_ok = on_done.clone();
    let on_done_for_close = on_done.clone();
    let label = prompt.label.clone();
    let save_label = prompt.save_label.clone();
    let confirm_label = prompt.confirm_label.clone();

    let body = v_form()
      .child(
        field()
          .label(label)
          .required(true)
          .child(Input::new(&secret)),
      )
      .child(
        field().child(
          Checkbox::new("save-in-keychain")
            .label(save_label)
            .checked(save.get())
            .on_click({
              let save = save.clone();
              move |checked, _, _| save.set(*checked)
            }),
        ),
      );

    let submitted_for_ok = submitted.clone();
    dialog
      .w(px(400.))
      .title(prompt.title.clone())
      .child(body)
      .footer(
        DialogFooter::new()
          .gap_2()
          .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
          .child(DialogAction::new().child(Button::new("ok").primary().label(confirm_label))),
      )
      .on_ok(move |_, window, cx| {
        let value = read_value(&secret, cx);
        if value.is_empty() {
          return false;
        }
        submitted_for_ok.set(true);
        on_done_for_ok(
          Some(SecretSubmit {
            secret: value,
            save_in_keychain: save.get(),
          }),
          window,
          cx,
        );
        true
      })
      .on_close(move |_, window, cx| {
        if !submitted.get() {
          on_done_for_close(None, window, cx);
        }
      })
  });
}

fn read_value(state: &Entity<InputState>, cx: &App) -> String {
  state.read(cx).value().to_string()
}
