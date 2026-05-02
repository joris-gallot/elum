//! Type-safe icon set for the Elum UI.
//!
//! Implements `gpui_component::IconNamed`, so values slot directly into
//! `gpui_component::Icon::new(...)` and `Button::icon(...)`. Paths point
//! into `crates/elum-ui/assets/`, served by [`crate::assets::AppAssets`].
//!
//! Adding an icon:
//! 1. Drop the SVG into `crates/elum-ui/assets/icons/<name>.svg`.
//! 2. Add a variant here and its `path()` arm.

use gpui::SharedString;
use gpui_component::IconNamed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiIconName {
  X,
  Plus,
}

impl IconNamed for UiIconName {
  fn path(self) -> SharedString {
    match self {
      Self::X => "icons/x.svg".into(),
      Self::Plus => "icons/plus.svg".into(),
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
