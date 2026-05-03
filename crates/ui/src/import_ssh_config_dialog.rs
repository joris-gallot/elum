use std::cell::RefCell;
use std::rc::Rc;

use gpui::{div, px, App, ParentElement, SharedString, Styled, Window};
use gpui_component::{
  button::{Button, ButtonVariants as _},
  checkbox::Checkbox,
  dialog::{CancelDialog, ConfirmDialog, DialogFooter},
  h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _,
};

#[derive(Debug, Clone)]
pub struct ImportRow {
  pub alias: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub already_exists: bool,
  pub warnings: Vec<String>,
}

pub fn open<F>(window: &mut Window, cx: &mut App, rows: Vec<ImportRow>, on_submit: F)
where
  F: Fn(Vec<usize>, &mut App) + 'static,
{
  let on_submit = Rc::new(on_submit);
  // New entries default to checked, existing ones default to unchecked.
  let selected: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(
    rows.iter().map(|r| !r.already_exists).collect(),
  ));
  let rows = Rc::new(rows);

  window.open_dialog(cx, move |dialog, _, cx| {
    let rows = rows.clone();
    let selected = selected.clone();
    let on_submit = on_submit.clone();

    let body: gpui::Div = if rows.is_empty() {
      v_flex().gap_2().child(
        div()
          .text_color(cx.theme().muted_foreground)
          .child("No importable hosts found in ~/.ssh/config."),
      )
    } else {
      let muted = cx.theme().muted_foreground;
      let danger = cx.theme().danger;
      let mut list = v_flex().gap_1();
      for (i, row) in rows.iter().enumerate() {
        let selected_for_click = selected.clone();
        let endpoint = if row.port == 22 {
          format!("{}@{}", row.user, row.host)
        } else {
          format!("{}@{}:{}", row.user, row.host, row.port)
        };
        let mut line = h_flex()
          .gap_2()
          .items_center()
          .child(
            Checkbox::new(("import-row", i))
              .checked(selected.borrow()[i])
              .on_click(move |checked, window, _| {
                if let Some(slot) = selected_for_click.borrow_mut().get_mut(i) {
                  *slot = *checked;
                }
                window.refresh();
              }),
          )
          .child(
            div()
              .min_w(px(120.))
              .child(SharedString::from(row.alias.clone())),
          )
          .child(
            div()
              .flex_1()
              .text_color(muted)
              .text_size(px(12.))
              .child(SharedString::from(endpoint)),
          );
        if row.already_exists {
          line = line.child(
            div()
              .text_color(muted)
              .text_size(px(11.))
              .child(SharedString::from("exists")),
          );
        }
        if !row.warnings.is_empty() {
          line = line.child(
            div()
              .text_color(danger)
              .text_size(px(11.))
              .child(SharedString::from(row.warnings.join(", "))),
          );
        }
        list = list.child(line);
      }
      v_flex()
        .gap_2()
        .child(
          div()
            .text_color(cx.theme().muted_foreground)
            .child("Select hosts to import."),
        )
        .child(list)
    };

    let any_selected = selected.borrow().iter().any(|v| *v);
    let footer = DialogFooter::new()
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
        Button::new("import")
          .primary()
          .flex_1()
          .label("Import")
          .disabled(!any_selected)
          .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(ConfirmDialog), cx);
          }),
      );

    dialog
      .w(px(520.))
      .title("Import from ~/.ssh/config")
      .child(body)
      .footer(footer)
      .on_ok(move |_, _window, cx| {
        let picks: Vec<usize> = selected
          .borrow()
          .iter()
          .enumerate()
          .filter_map(|(i, v)| v.then_some(i))
          .collect();
        if picks.is_empty() {
          return false;
        }
        on_submit(picks, cx);
        true
      })
  });
}
