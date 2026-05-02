use gpui::{
  div, prelude::*, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
};
use gpui_component::Root;
use workspace::Workspace;

pub struct AppRoot {
  workspace: Entity<Workspace>,
  focus_handle: FocusHandle,
}

impl AppRoot {
  pub fn new(workspace: Entity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
    Self {
      workspace,
      focus_handle: cx.focus_handle(),
    }
  }
}

impl Focusable for AppRoot {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for AppRoot {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let sheet_layer = Root::render_sheet_layer(window, cx);
    let dialog_layer = Root::render_dialog_layer(window, cx);
    let notification_layer = Root::render_notification_layer(window, cx);

    div()
      .size_full()
      .child(self.workspace.clone())
      .children(sheet_layer)
      .children(dialog_layer)
      .children(notification_layer)
  }
}
