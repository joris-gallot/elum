use gpui::SharedString;
use gpui_component::IconNamed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiIconName {
  X,
  Plus,
  Server,
  Settings,
  ShieldCheck,
  TriangleAlert,
  Import,
}

impl IconNamed for UiIconName {
  fn path(self) -> SharedString {
    match self {
      Self::X => "icons/x.svg".into(),
      Self::Plus => "icons/plus.svg".into(),
      Self::Server => "icons/server.svg".into(),
      Self::Settings => "icons/settings.svg".into(),
      Self::ShieldCheck => "icons/shield-check.svg".into(),
      Self::TriangleAlert => "icons/triangle-alert.svg".into(),
      Self::Import => "icons/import.svg".into(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn icon_names_have_stable_paths() {
    assert_eq!(UiIconName::X.path(), SharedString::from("icons/x.svg"));
  }
}
