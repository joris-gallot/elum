//! Embedded SVG assets and the lightweight `Icon` component for rendering
//! them.
//!
//! Asset bytes are baked into the binary with `include_bytes!`. To add an
//! icon:
//! 1. Drop the SVG into `crates/elum/assets/icons/<name>.svg`.
//! 2. Add a variant to `IconName` and its `path()` arm.
//! 3. Add a match arm in `ElumAssets::load`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{
  prelude::*, svg, App, AssetSource, Hsla, IntoElement, Pixels, RenderOnce, SharedString, Window,
};

/// Asset source backed by `include_bytes!`. Registered once at app start
/// via `gpui::Application::with_assets(...)`.
pub struct ElumAssets;

impl AssetSource for ElumAssets {
  fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
    let bytes: &'static [u8] = match path {
      "icons/x.svg" => include_bytes!("../assets/icons/x.svg"),
      _ => return Ok(None),
    };
    Ok(Some(Cow::Borrowed(bytes)))
  }

  fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
    Ok(vec![])
  }
}

/// Type-safe icon set. Strings stay private to this module so callers
/// can't typo a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconName {
  X,
}

impl IconName {
  fn path(self) -> &'static str {
    match self {
      Self::X => "icons/x.svg",
    }
  }
}

/// Render an icon as a colored, sized SVG. Color and size default to the
/// inherited text style if not overridden.
#[derive(IntoElement)]
pub struct Icon {
  name: IconName,
  color: Option<Hsla>,
  size: Option<Pixels>,
}

#[allow(dead_code)] // builders kept for future callers (header bars,
                    // host-row indicators, etc.)
impl Icon {
  pub fn new(name: IconName) -> Self {
    Self {
      name,
      color: None,
      size: None,
    }
  }

  pub fn color(mut self, color: impl Into<Hsla>) -> Self {
    self.color = Some(color.into());
    self
  }

  pub fn size(mut self, size: Pixels) -> Self {
    self.size = Some(size);
    self
  }
}

impl RenderOnce for Icon {
  fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
    let text_color = self.color.unwrap_or_else(|| window.text_style().color);
    let pixel_size = self
      .size
      .unwrap_or_else(|| window.text_style().font_size.to_pixels(window.rem_size()));
    svg()
      .flex_none()
      .size(pixel_size)
      .text_color(text_color)
      .path(self.name.path())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_icons_load() {
    let assets = ElumAssets;
    let bytes = assets.load("icons/x.svg").expect("load");
    assert!(bytes.is_some());
    let bytes = bytes.unwrap();
    assert!(
      bytes.starts_with(b"<svg"),
      "expected svg, got {:?}",
      &bytes[..bytes.len().min(20)]
    );
  }

  #[test]
  fn unknown_icon_returns_none() {
    let assets = ElumAssets;
    let bytes = assets.load("icons/does-not-exist.svg").expect("load");
    assert!(bytes.is_none());
  }

  #[test]
  fn icon_name_path_round_trip() {
    assert_eq!(IconName::X.path(), "icons/x.svg");
  }
}
