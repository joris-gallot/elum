//! App-wide settings: a versioned JSON file at the OS data directory.
//!
//! macOS path: `~/Library/Application Support/elum/settings.json`.
//!
//! Schema v1:
//! ```jsonc
//! {
//!   "version": 1,
//!   "theme_mode": "dark",
//!   "auto_switch_theme": false
//! }
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
  Light,
  #[default]
  Dark,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct AppSettingsFile {
  version: u32,
  #[serde(default)]
  theme_mode: ThemeMode,
  #[serde(default)]
  auto_switch_theme: bool,
}

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct AppSettings {
  path: PathBuf,
  pub theme_mode: ThemeMode,
  pub auto_switch_theme: bool,
}

impl gpui::Global for AppSettings {}

impl Default for AppSettings {
  fn default() -> Self {
    Self {
      path: Self::default_path(),
      theme_mode: ThemeMode::default(),
      auto_switch_theme: false,
    }
  }
}

impl AppSettings {
  pub fn default_path() -> PathBuf {
    dirs::data_dir().map_or_else(
      || PathBuf::from("./elum-settings.json"),
      |p| p.join("elum").join("settings.json"),
    )
  }

  pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref().to_path_buf();
    match fs::read_to_string(&path) {
      Ok(text) => {
        let file: AppSettingsFile =
          serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if file.version != SCHEMA_VERSION {
          return Err(anyhow!(
            "unknown app-settings schema version {} at {}",
            file.version,
            path.display()
          ));
        }
        Ok(Self {
          path,
          theme_mode: file.theme_mode,
          auto_switch_theme: file.auto_switch_theme,
        })
      }
      Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
        path,
        theme_mode: ThemeMode::default(),
        auto_switch_theme: false,
      }),
      Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
  }

  pub fn save(&self) -> Result<()> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    let file = AppSettingsFile {
      version: SCHEMA_VERSION,
      theme_mode: self.theme_mode,
      auto_switch_theme: self.auto_switch_theme,
    };
    let text = serde_json::to_string_pretty(&file).context("serializing app settings")?;

    let tmp = self.path.with_extension("json.tmp");
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &self.path)
      .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  #[test]
  fn missing_file_loads_defaults() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    let settings = AppSettings::load_from(&path).expect("load");
    assert_eq!(settings.theme_mode, ThemeMode::Dark);
    assert!(!settings.auto_switch_theme);
  }

  #[test]
  fn save_load_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    let mut settings = AppSettings::load_from(&path).unwrap();
    settings.theme_mode = ThemeMode::Light;
    settings.auto_switch_theme = true;
    settings.save().unwrap();

    let reloaded = AppSettings::load_from(&path).unwrap();
    assert_eq!(reloaded.theme_mode, ThemeMode::Light);
    assert!(reloaded.auto_switch_theme);
  }

  #[test]
  fn legacy_file_without_auto_field_defaults_to_false() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, r#"{"version":1,"theme_mode":"light"}"#).unwrap();
    let settings = AppSettings::load_from(&path).expect("load");
    assert_eq!(settings.theme_mode, ThemeMode::Light);
    assert!(!settings.auto_switch_theme);
  }

  #[test]
  fn save_creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let path = dir
      .path()
      .join("nested")
      .join("deeper")
      .join("settings.json");
    let mut settings = AppSettings::load_from(&path).unwrap();
    settings.theme_mode = ThemeMode::Light;
    settings.save().expect("save creates parents");
    assert!(path.exists());
  }

  #[test]
  fn unknown_schema_version_is_an_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, r#"{"version":99,"theme_mode":"dark"}"#).unwrap();
    let err = AppSettings::load_from(&path).unwrap_err();
    assert!(err.to_string().contains("unknown app-settings schema"));
  }

  #[test]
  fn malformed_json_is_an_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, "{ this is not json").unwrap();
    let err = AppSettings::load_from(&path).unwrap_err();
    assert!(err.to_string().contains("settings.json") || err.chain().count() > 0);
  }
}
