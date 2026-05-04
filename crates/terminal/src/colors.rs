//! Terminal color palette: chrome + 16 ANSI entries, installed as a
//! `gpui::Global` and read by the painter each frame.

use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor, Rgb};
use gpui::{Global, Rgba};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalTheme {
  pub foreground: Rgba,
  pub background: Rgba,
  pub cursor: Rgba,
  pub selection: Rgba,
  /// Background fill for the active Cmd+F match; distinct from `selection`.
  pub search_match: Rgba,
  /// 16 ANSI entries: black, red, green, yellow, blue, magenta, cyan, white, then bright variants.
  pub ansi: [Rgba; 16],
}

impl Global for TerminalTheme {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalThemeId {
  #[default]
  OneDark,
  Dracula,
  SolarizedDark,
  SolarizedLight,
  Nord,
  GitHubLight,
  TomorrowNightBright,
}

impl TerminalThemeId {
  pub fn all() -> &'static [TerminalThemeId] {
    &[
      TerminalThemeId::OneDark,
      TerminalThemeId::Dracula,
      TerminalThemeId::SolarizedDark,
      TerminalThemeId::SolarizedLight,
      TerminalThemeId::Nord,
      TerminalThemeId::GitHubLight,
      TerminalThemeId::TomorrowNightBright,
    ]
  }

  pub fn slug(self) -> &'static str {
    match self {
      TerminalThemeId::OneDark => "one_dark",
      TerminalThemeId::Dracula => "dracula",
      TerminalThemeId::SolarizedDark => "solarized_dark",
      TerminalThemeId::SolarizedLight => "solarized_light",
      TerminalThemeId::Nord => "nord",
      TerminalThemeId::GitHubLight => "github_light",
      TerminalThemeId::TomorrowNightBright => "tomorrow_night_bright",
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      TerminalThemeId::OneDark => "One Dark",
      TerminalThemeId::Dracula => "Dracula",
      TerminalThemeId::SolarizedDark => "Solarized Dark",
      TerminalThemeId::SolarizedLight => "Solarized Light",
      TerminalThemeId::Nord => "Nord",
      TerminalThemeId::GitHubLight => "GitHub Light",
      TerminalThemeId::TomorrowNightBright => "Tomorrow Night Bright",
    }
  }

  /// Falls back to the default on unknown slugs so deserialization stays tolerant.
  pub fn from_slug(slug: &str) -> Self {
    Self::all()
      .iter()
      .copied()
      .find(|t| t.slug() == slug)
      .unwrap_or_default()
  }

  pub fn theme(self) -> TerminalTheme {
    match self {
      TerminalThemeId::OneDark => one_dark(),
      TerminalThemeId::Dracula => dracula(),
      TerminalThemeId::SolarizedDark => solarized_dark(),
      TerminalThemeId::SolarizedLight => solarized_light(),
      TerminalThemeId::Nord => nord(),
      TerminalThemeId::GitHubLight => github_light(),
      TerminalThemeId::TomorrowNightBright => tomorrow_night_bright(),
    }
  }
}

impl Default for TerminalTheme {
  fn default() -> Self {
    one_dark()
  }
}

impl TerminalTheme {
  pub fn to_rgba(&self, color: AlacColor) -> Rgba {
    match color {
      AlacColor::Named(name) => self.named(name),
      AlacColor::Spec(rgb) => rgb_to_rgba(rgb),
      AlacColor::Indexed(idx) => self.indexed(idx),
    }
  }

  fn named(&self, name: NamedColor) -> Rgba {
    match name {
      NamedColor::Black => self.ansi[0],
      NamedColor::Red => self.ansi[1],
      NamedColor::Green => self.ansi[2],
      NamedColor::Yellow => self.ansi[3],
      NamedColor::Blue => self.ansi[4],
      NamedColor::Magenta => self.ansi[5],
      NamedColor::Cyan => self.ansi[6],
      NamedColor::White => self.ansi[7],
      NamedColor::BrightBlack => self.ansi[8],
      NamedColor::BrightRed => self.ansi[9],
      NamedColor::BrightGreen => self.ansi[10],
      NamedColor::BrightYellow => self.ansi[11],
      NamedColor::BrightBlue => self.ansi[12],
      NamedColor::BrightMagenta => self.ansi[13],
      NamedColor::BrightCyan => self.ansi[14],
      NamedColor::BrightWhite => self.ansi[15],

      NamedColor::Foreground => self.foreground,
      NamedColor::Background => self.background,
      NamedColor::Cursor => self.cursor,
      // Dim slots fall back to fg until bold/dim styling lands.
      _ => self.foreground,
    }
  }

  fn indexed(&self, idx: u8) -> Rgba {
    if idx < 16 {
      self.ansi[idx as usize]
    } else if idx < 232 {
      let cube_idx = (idx - 16) as u32;
      let r = cube_idx / 36;
      let g = (cube_idx / 6) % 6;
      let b = cube_idx % 6;
      Rgba {
        r: cube_level(r) as f32 / 255.0,
        g: cube_level(g) as f32 / 255.0,
        b: cube_level(b) as f32 / 255.0,
        a: 1.0,
      }
    } else {
      let level = 8 + (idx - 232) as u32 * 10;
      let v = level as f32 / 255.0;
      Rgba {
        r: v,
        g: v,
        b: v,
        a: 1.0,
      }
    }
  }

  /// RGB lookup for OSC color queries.
  pub fn color_index_rgb(&self, index: usize) -> Rgb {
    match index {
      0..=15 => rgba_to_rgb(self.ansi[index]),
      16..=231 => {
        let cube_idx = (index - 16) as u32;
        Rgb {
          r: cube_level(cube_idx / 36) as u8,
          g: cube_level((cube_idx / 6) % 6) as u8,
          b: cube_level(cube_idx % 6) as u8,
        }
      }
      232..=255 => {
        let level = 8 + (index - 232) as u8 * 10;
        Rgb {
          r: level,
          g: level,
          b: level,
        }
      }
      x if x == NamedColor::Foreground as usize || x == NamedColor::BrightForeground as usize => {
        rgba_to_rgb(self.foreground)
      }
      x if x == NamedColor::Background as usize => rgba_to_rgb(self.background),
      x if x == NamedColor::Cursor as usize => rgba_to_rgb(self.cursor),
      x if x == NamedColor::DimForeground as usize => rgba_to_rgb(self.foreground),
      _ => rgba_to_rgb(self.foreground),
    }
  }
}

fn rgb_to_rgba(c: Rgb) -> Rgba {
  Rgba {
    r: c.r as f32 / 255.0,
    g: c.g as f32 / 255.0,
    b: c.b as f32 / 255.0,
    a: 1.0,
  }
}

fn rgba_to_rgb(c: Rgba) -> Rgb {
  Rgb {
    r: (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
    g: (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
    b: (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
  }
}

fn cube_level(i: u32) -> u32 {
  match i {
    0 => 0,
    n => 55 + n * 40,
  }
}

fn rgb(hex: u32) -> Rgba {
  Rgba {
    r: ((hex >> 16) & 0xff) as f32 / 255.0,
    g: ((hex >> 8) & 0xff) as f32 / 255.0,
    b: (hex & 0xff) as f32 / 255.0,
    a: 1.0,
  }
}

// ---------- Built-in presets ----------

fn one_dark() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0xe6e6e6),
    background: rgb(0x101418),
    cursor: rgb(0xe6e6e6),
    selection: rgb(0x3a4d6a),
    search_match: rgb(0xe5c07b),
    ansi: [
      rgb(0x1f2329), // black
      rgb(0xe06c75), // red
      rgb(0x98c379), // green
      rgb(0xe5c07b), // yellow
      rgb(0x61afef), // blue
      rgb(0xc678dd), // magenta
      rgb(0x56b6c2), // cyan
      rgb(0xdcdfe4), // white
      rgb(0x4b5263), // bright black
      rgb(0xff7b86), // bright red
      rgb(0xb6e58c), // bright green
      rgb(0xffd58a), // bright yellow
      rgb(0x7fc4ff), // bright blue
      rgb(0xd8a3ec), // bright magenta
      rgb(0x6cd1de), // bright cyan
      rgb(0xffffff), // bright white
    ],
  }
}

fn dracula() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0xf8f8f2),
    background: rgb(0x282a36),
    cursor: rgb(0xf8f8f0),
    selection: rgb(0x44475a),
    search_match: rgb(0xffb86c),
    ansi: [
      rgb(0x21222c),
      rgb(0xff5555),
      rgb(0x50fa7b),
      rgb(0xf1fa8c),
      rgb(0xbd93f9),
      rgb(0xff79c6),
      rgb(0x8be9fd),
      rgb(0xf8f8f2),
      rgb(0x6272a4),
      rgb(0xff6e6e),
      rgb(0x69ff94),
      rgb(0xffffa5),
      rgb(0xd6acff),
      rgb(0xff92df),
      rgb(0xa4ffff),
      rgb(0xffffff),
    ],
  }
}

fn solarized_dark() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0x839496),
    background: rgb(0x002b36),
    cursor: rgb(0x93a1a1),
    selection: rgb(0x073642),
    search_match: rgb(0xb58900),
    ansi: [
      rgb(0x073642),
      rgb(0xdc322f),
      rgb(0x859900),
      rgb(0xb58900),
      rgb(0x268bd2),
      rgb(0xd33682),
      rgb(0x2aa198),
      rgb(0xeee8d5),
      rgb(0x002b36),
      rgb(0xcb4b16),
      rgb(0x586e75),
      rgb(0x657b83),
      rgb(0x839496),
      rgb(0x6c71c4),
      rgb(0x93a1a1),
      rgb(0xfdf6e3),
    ],
  }
}

fn solarized_light() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0x657b83),
    background: rgb(0xfdf6e3),
    cursor: rgb(0x586e75),
    selection: rgb(0xeee8d5),
    search_match: rgb(0xffd479),
    ansi: [
      rgb(0xeee8d5),
      rgb(0xdc322f),
      rgb(0x859900),
      rgb(0xb58900),
      rgb(0x268bd2),
      rgb(0xd33682),
      rgb(0x2aa198),
      rgb(0x073642),
      rgb(0xfdf6e3),
      rgb(0xcb4b16),
      rgb(0x93a1a1),
      rgb(0x839496),
      rgb(0x657b83),
      rgb(0x6c71c4),
      rgb(0x586e75),
      rgb(0x002b36),
    ],
  }
}

fn nord() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0xd8dee9),
    background: rgb(0x2e3440),
    cursor: rgb(0xd8dee9),
    selection: rgb(0x434c5e),
    search_match: rgb(0xebcb8b),
    ansi: [
      rgb(0x3b4252),
      rgb(0xbf616a),
      rgb(0xa3be8c),
      rgb(0xebcb8b),
      rgb(0x81a1c1),
      rgb(0xb48ead),
      rgb(0x88c0d0),
      rgb(0xe5e9f0),
      rgb(0x4c566a),
      rgb(0xbf616a),
      rgb(0xa3be8c),
      rgb(0xebcb8b),
      rgb(0x81a1c1),
      rgb(0xb48ead),
      rgb(0x8fbcbb),
      rgb(0xeceff4),
    ],
  }
}

fn github_light() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0x24292e),
    background: rgb(0xffffff),
    cursor: rgb(0x24292e),
    selection: rgb(0xc8e1ff),
    search_match: rgb(0xfff5b1),
    ansi: [
      rgb(0x24292e),
      rgb(0xd73a49),
      rgb(0x22863a),
      rgb(0xb08800),
      rgb(0x005cc5),
      rgb(0x6f42c1),
      rgb(0x1b7c83),
      rgb(0x6a737d),
      rgb(0x959da5),
      rgb(0xcb2431),
      rgb(0x28a745),
      rgb(0xdbab09),
      rgb(0x2188ff),
      rgb(0x8a63d2),
      rgb(0x3192aa),
      rgb(0xd1d5da),
    ],
  }
}

fn tomorrow_night_bright() -> TerminalTheme {
  TerminalTheme {
    foreground: rgb(0xeaeaea),
    background: rgb(0x000000),
    cursor: rgb(0xeaeaea),
    selection: rgb(0x424242),
    search_match: rgb(0xe7c547),
    ansi: [
      rgb(0x000000),
      rgb(0xd54e53),
      rgb(0xb9ca4a),
      rgb(0xe7c547),
      rgb(0x7aa6da),
      rgb(0xc397d8),
      rgb(0x70c0b1),
      rgb(0xeaeaea),
      rgb(0x666666),
      rgb(0xff3334),
      rgb(0x9ec400),
      rgb(0xe7c547),
      rgb(0x7aa6da),
      rgb(0xb77ee0),
      rgb(0x54ced6),
      rgb(0xffffff),
    ],
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
  }

  fn rgba_eq(a: Rgba, b: Rgba) -> bool {
    approx(a.r, b.r) && approx(a.g, b.g) && approx(a.b, b.b) && approx(a.a, b.a)
  }

  #[test]
  fn named_red_maps_to_palette_red() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Named(NamedColor::Red));
    assert!(rgba_eq(c, theme.ansi[1]));
  }

  #[test]
  fn named_foreground_returns_chrome_fg() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Named(NamedColor::Foreground));
    assert!(rgba_eq(c, theme.foreground));
  }

  #[test]
  fn named_background_returns_chrome_bg() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Named(NamedColor::Background));
    assert!(rgba_eq(c, theme.background));
  }

  #[test]
  fn spec_rgb_passes_through() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Spec(Rgb {
      r: 0xab,
      g: 0xcd,
      b: 0xef,
    }));
    assert!(approx(c.r, 0xab as f32 / 255.0));
    assert!(approx(c.g, 0xcd as f32 / 255.0));
    assert!(approx(c.b, 0xef as f32 / 255.0));
  }

  #[test]
  fn indexed_first_16_alias_named_colors() {
    let theme = TerminalTheme::default();
    for i in 0u8..16 {
      let from_indexed = theme.to_rgba(AlacColor::Indexed(i));
      assert!(
        rgba_eq(from_indexed, theme.ansi[i as usize]),
        "index {i} should match ansi[{i}]"
      );
    }
  }

  #[test]
  fn indexed_232_is_dark_gray() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Indexed(232));
    assert!(approx(c.r, 8.0 / 255.0));
    assert!(approx(c.g, c.r));
    assert!(approx(c.b, c.r));
  }

  #[test]
  fn indexed_255_is_near_white_gray() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Indexed(255));
    assert!(approx(c.r, 238.0 / 255.0));
  }

  #[test]
  fn indexed_cube_corner_is_black() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Indexed(16));
    assert!(approx(c.r, 0.0) && approx(c.g, 0.0) && approx(c.b, 0.0));
  }

  #[test]
  fn indexed_cube_opposite_is_white() {
    let theme = TerminalTheme::default();
    let c = theme.to_rgba(AlacColor::Indexed(231));
    assert!(approx(c.r, 1.0) && approx(c.g, 1.0) && approx(c.b, 1.0));
  }

  #[test]
  fn theme_id_slug_roundtrip() {
    for id in TerminalThemeId::all() {
      assert_eq!(TerminalThemeId::from_slug(id.slug()), *id);
    }
  }

  #[test]
  fn theme_id_unknown_slug_falls_back_to_default() {
    assert_eq!(
      TerminalThemeId::from_slug("definitely_not_a_theme"),
      TerminalThemeId::default()
    );
  }

  #[test]
  fn theme_factory_matches_id() {
    let id = TerminalThemeId::Dracula;
    assert_eq!(id.theme(), dracula());
  }

  #[test]
  fn color_index_rgb_matches_palette() {
    let theme = TerminalTheme::default();
    assert_eq!(theme.color_index_rgb(1), rgba_to_rgb(theme.ansi[1]));
    assert_eq!(theme.color_index_rgb(4), rgba_to_rgb(theme.ansi[4]));
  }

  #[test]
  fn color_index_rgb_supports_cube_and_gray_fallbacks() {
    let theme = TerminalTheme::default();
    assert_eq!(theme.color_index_rgb(16), Rgb { r: 0, g: 0, b: 0 });
    assert_eq!(
      theme.color_index_rgb(231),
      Rgb {
        r: 255,
        g: 255,
        b: 255
      }
    );
    assert_eq!(
      theme.color_index_rgb(255),
      Rgb {
        r: 238,
        g: 238,
        b: 238
      }
    );
  }

  #[test]
  fn color_index_rgb_semantic_returns_chrome() {
    let theme = TerminalTheme::default();
    assert_eq!(
      theme.color_index_rgb(NamedColor::Foreground as usize),
      rgba_to_rgb(theme.foreground)
    );
    assert_eq!(
      theme.color_index_rgb(NamedColor::Background as usize),
      rgba_to_rgb(theme.background)
    );
    assert_eq!(
      theme.color_index_rgb(NamedColor::Cursor as usize),
      rgba_to_rgb(theme.cursor)
    );
  }
}
