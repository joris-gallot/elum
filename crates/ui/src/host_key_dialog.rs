use std::cell::Cell;
use std::rc::Rc;

use gpui::{div, px, App, ParentElement, SharedString, Styled, Window};
use gpui_component::{
  button::{Button, ButtonVariants as _},
  dialog::{CancelDialog, DialogFooter},
  v_flex, ActiveTheme as _, Icon, WindowExt as _,
};

use crate::UiIconName;

#[derive(Debug, Clone)]
pub enum HostKeyDialogKind {
  /// Host has no entry in `known_hosts` yet.
  New,
  /// Host has an entry but the key currently presented differs.
  Changed { previous_line: usize },
}

#[derive(Debug, Clone)]
pub struct HostKeyDialogInfo {
  pub host: String,
  pub port: u16,
  pub kind: HostKeyDialogKind,
  pub key_algorithm: String,
  /// `SHA256:base64` formatted fingerprint to display.
  pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDialogVerdict {
  AcceptOnce,
  AcceptAndRemember,
  Reject,
}

pub fn open<F>(window: &mut Window, cx: &mut App, info: HostKeyDialogInfo, on_done: F)
where
  F: Fn(HostKeyDialogVerdict, &mut Window, &mut App) + 'static,
{
  let on_done = Rc::new(on_done);
  // Buttons set this before dispatching `CancelDialog`; close-time None means Reject.
  let verdict = Rc::new(Cell::new(None::<HostKeyDialogVerdict>));

  window.open_dialog(cx, move |dialog, _, cx| {
    let verdict_for_once = verdict.clone();
    let verdict_for_remember = verdict.clone();
    let verdict_for_close = verdict.clone();
    let on_done_for_close = on_done.clone();
    let info = info.clone();

    let host_label = if info.port == 22 {
      info.host.clone()
    } else {
      format!("{}:{}", info.host, info.port)
    };

    let (icon, icon_color, title, lede, accept_label, show_once) = match &info.kind {
      HostKeyDialogKind::New => (
        UiIconName::ShieldCheck,
        cx.theme().primary,
        SharedString::from("Verify host identity"),
        SharedString::from(format!(
          "The authenticity of host {host_label:?} can't be established. Are you sure you want to continue?"
        )),
        SharedString::from("Connect & remember"),
        true,
      ),
      HostKeyDialogKind::Changed { previous_line } => (
        UiIconName::TriangleAlert,
        cx.theme().danger,
        SharedString::from("Host key has changed"),
        SharedString::from(format!(
          "WARNING: the key for {host_label:?} differs from the one recorded earlier (line {previous_line}). \
           Someone could be eavesdropping on you (man-in-the-middle attack). Only continue if you intended to change keys."
        )),
        SharedString::from("I trust this, replace key"),
        false,
      ),
    };

    let foreground = cx.theme().foreground;
    let muted = cx.theme().muted_foreground;
    let body = v_flex()
      .gap_3()
      .child(
        div()
          .flex()
          .items_center()
          .gap_2()
          .text_color(icon_color)
          .child(Icon::new(icon))
          .child(div().text_color(foreground).child(title)),
      )
      .child(div().text_color(muted).child(lede))
      .child(
        div()
          .text_color(muted)
          .font_family("Menlo")
          .text_size(px(12.))
          .child(SharedString::from(format!(
            "{} key fingerprint:\n  {}",
            info.key_algorithm.to_uppercase(),
            info.fingerprint
          ))),
      );

    let mut footer = DialogFooter::new().gap_2().child(
      Button::new("cancel")
        .outline()
        .label(match info.kind {
          HostKeyDialogKind::New => "Cancel",
          HostKeyDialogKind::Changed { .. } => "Disconnect",
        })
        .on_click(|_, window, cx| {
          window.dispatch_action(Box::new(CancelDialog), cx);
        }),
    );
    if show_once {
      footer = footer.child(
        Button::new("accept-once")
          .outline()
          .label("Connect once")
          .on_click(move |_, window, cx| {
            verdict_for_once.set(Some(HostKeyDialogVerdict::AcceptOnce));
            window.dispatch_action(Box::new(CancelDialog), cx);
          }),
      );
    }
    footer = footer.child(
      Button::new("accept-remember")
        .primary()
        .label(accept_label)
        .on_click(move |_, window, cx| {
          verdict_for_remember.set(Some(HostKeyDialogVerdict::AcceptAndRemember));
          window.dispatch_action(Box::new(CancelDialog), cx);
        }),
    );

    dialog
      .w(px(480.))
      .child(body)
      .footer(footer)
      .on_close(move |_, window, cx| {
        let chosen = verdict_for_close
          .get()
          .unwrap_or(HostKeyDialogVerdict::Reject);
        on_done_for_close(chosen, window, cx);
      })
  });
}
