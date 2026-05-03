use gpui::App;
use gpui_component::setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings};

use crate::app_settings::{AppSettings, ThemeMode};

pub fn apply_theme(settings: &AppSettings, cx: &mut App) {
  if settings.auto_switch_theme {
    gpui_component::Theme::sync_system_appearance(None, cx);
  } else {
    let mode = match settings.theme_mode {
      ThemeMode::Dark => gpui_component::ThemeMode::Dark,
      ThemeMode::Light => gpui_component::ThemeMode::Light,
    };
    gpui_component::Theme::change(mode, None, cx);
  }
}

/// Update one field on the global `AppSettings`, re-apply the theme, refresh
/// every window, and persist the new settings to disk
fn update_settings_and_apply(cx: &mut App, mutate: impl FnOnce(&mut AppSettings)) {
  let settings_to_save = {
    let settings = cx.global_mut::<AppSettings>();
    mutate(settings);
    settings.clone()
  };
  apply_theme(&settings_to_save, cx);
  cx.refresh_windows();
  if let Err(e) = settings_to_save.save() {
    eprintln!("warning: failed to save app settings: {e:#}");
  }
}

pub(crate) fn settings(_cx: &mut App) -> Settings {
  // Switch: ON = Dark mode, OFF = Light mode.
  let dark_mode_field = SettingField::switch(
    |cx: &App| matches!(cx.global::<AppSettings>().theme_mode, ThemeMode::Dark),
    |on: bool, cx: &mut App| {
      let new_mode = if on {
        ThemeMode::Dark
      } else {
        ThemeMode::Light
      };
      let already = cx.global::<AppSettings>().theme_mode == new_mode;
      if already {
        return;
      }
      update_settings_and_apply(cx, |settings| settings.theme_mode = new_mode);
    },
  );

  // Checkbox: when enabled, theme follows the OS appearance.
  let auto_field = SettingField::checkbox(
    |cx: &App| cx.global::<AppSettings>().auto_switch_theme,
    |on: bool, cx: &mut App| {
      if cx.global::<AppSettings>().auto_switch_theme == on {
        return;
      }
      update_settings_and_apply(cx, |settings| settings.auto_switch_theme = on);
    },
  );

  let appearance = SettingGroup::new()
    .title("Appearance")
    .description("Visual presentation of the app.")
    .item(
      SettingItem::new("Dark mode", dark_mode_field)
        .description("Toggle between light and dark themes."),
    )
    .item(
      SettingItem::new("Auto switch theme", auto_field)
        .description("Follow the system appearance instead of the manual choice above."),
    );

  let general = SettingPage::new("General")
    .default_open(true)
    .group(appearance);

  Settings::new("app-settings").page(general)
}
