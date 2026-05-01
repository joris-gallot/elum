//! Asset source for the Elum app.
//!
//! Scans `crates/elum-ui/assets/` at compile time via `rust-embed`, then
//! falls back to `gpui-component-assets` for the icons rendered by
//! `gpui-component` internals (Dialog close, Select chevron, etc.).
//!
//! Adding a new asset is just dropping the file under `assets/`. The
//! type-safe icon enum lives in [`crate::icons::UiIconName`].

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
struct UiAssets;

pub struct AppAssets;

impl AssetSource for AppAssets {
  fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
    if let Some(asset) = UiAssets::get(path) {
      return Ok(Some(asset.data));
    }
    // Treat any error from the fallback as "not found" so the
    // `AssetSource` contract stays consistent (Ok(None) on miss).
    Ok(gpui_component_assets::Assets.load(path).ok().flatten())
  }

  fn list(&self, path: &str) -> Result<Vec<SharedString>> {
    let mut items = gpui_component_assets::Assets.list(path).unwrap_or_default();

    for asset in UiAssets::iter() {
      if asset.starts_with(path) {
        items.push(asset.as_ref().to_string().into());
      }
    }

    items.sort();
    items.dedup();
    Ok(items)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_icon_loads_from_embed() {
    let bytes = AppAssets.load("icons/x.svg").expect("load");
    assert!(bytes.is_some(), "icons/x.svg should be embedded");
    let bytes = bytes.unwrap();
    assert!(
      bytes.starts_with(b"<svg"),
      "expected svg, got {:?}",
      &bytes[..bytes.len().min(20)]
    );
  }

  #[test]
  fn missing_asset_returns_none() {
    let bytes = AppAssets
      .load("totally/missing-asset-not-in-any-bundle.svg")
      .expect("load");
    assert!(bytes.is_none());
  }
}
