use gpui::{App, SharedString};
use gpui_component::setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings};
use terminal::colors::{TerminalTheme, TerminalThemeId};

use crate::app_settings::{AppSettings, ThemeMode};

/// Push settings into the live `gpui_component::Theme` and `TerminalTheme` globals.
/// The terminal palette tracks the active app mode: dark mode uses
/// `terminal_dark_theme`, light mode uses `terminal_light_theme`.
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
  let is_dark = gpui_component::Theme::global(cx).mode.is_dark();
  let chosen = if is_dark {
    settings.terminal_dark_theme
  } else {
    settings.terminal_light_theme
  };
  cx.set_global::<TerminalTheme>(chosen.theme());
}

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

/// Build a terminal-theme dropdown that reads/writes one slot of `AppSettings`.
fn terminal_theme_dropdown(
  read: fn(&AppSettings) -> TerminalThemeId,
  write: fn(&mut AppSettings, TerminalThemeId),
) -> SettingField<SharedString> {
  let options: Vec<(SharedString, SharedString)> = TerminalThemeId::all()
    .iter()
    .map(|id| (id.slug().into(), id.label().into()))
    .collect();
  SettingField::dropdown(
    options,
    move |cx: &App| -> SharedString { read(cx.global::<AppSettings>()).slug().into() },
    move |val: SharedString, cx: &mut App| {
      let new_id = TerminalThemeId::from_slug(val.as_ref());
      if read(cx.global::<AppSettings>()) == new_id {
        return;
      }
      update_settings_and_apply(cx, |settings| write(settings, new_id));
    },
  )
}

pub(crate) fn settings(_cx: &mut App) -> Settings {
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

  let auto_field = SettingField::checkbox(
    |cx: &App| cx.global::<AppSettings>().auto_switch_theme,
    |on: bool, cx: &mut App| {
      if cx.global::<AppSettings>().auto_switch_theme == on {
        return;
      }
      update_settings_and_apply(cx, |settings| settings.auto_switch_theme = on);
    },
  );

  let dark_terminal_field = terminal_theme_dropdown(
    |s| s.terminal_dark_theme,
    |s, id| s.terminal_dark_theme = id,
  );
  let light_terminal_field = terminal_theme_dropdown(
    |s| s.terminal_light_theme,
    |s, id| s.terminal_light_theme = id,
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
    )
    .item(
      SettingItem::new("Terminal theme (dark)", dark_terminal_field)
        .description("Palette used by the terminal grid while the app is in dark mode."),
    )
    .item(
      SettingItem::new("Terminal theme (light)", light_terminal_field)
        .description("Palette used by the terminal grid while the app is in light mode."),
    );

  let general = SettingPage::new("General")
    .default_open(true)
    .group(appearance);

  Settings::new("app-settings").page(general)
}
