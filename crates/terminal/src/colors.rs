//! Mapping from `alacritty_terminal::vte::ansi::Color` to GPUI `Rgba`.
//!
//! Three sources of color in alacritty:
//! - `Named(NamedColor)`: 16 ANSI colors + bright variants + Foreground/
//!   Background/Cursor/etc. semantic names.
//! - `Spec(Rgb)`: literal 24-bit RGB the terminal sequences asked for.
//! - `Indexed(u8)`: 256-color palette index. The first 16 alias the named
//!   colors; 16..232 form a 6×6×6 RGB cube; 232..256 are 24 grayscale steps.

use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor, Rgb};
use gpui::Rgba;

/// Convert an alacritty `Color` to a GPUI `Rgba`. The two reference colors,
/// `default_fg` and `default_bg`, apply when the cell asks for the
/// terminal's notional foreground/background.
pub fn to_rgba(color: AlacColor, default_fg: Rgba, default_bg: Rgba) -> Rgba {
  match color {
    AlacColor::Named(name) => named_color(name, default_fg, default_bg),
    AlacColor::Spec(rgb) => rgb_to_rgba(rgb),
    AlacColor::Indexed(idx) => indexed_color(idx, default_fg, default_bg),
  }
}

pub fn default_foreground() -> Rgba {
  rgb(0xe6e6e6)
}

pub fn default_foreground_rgb() -> Rgb {
  alac_rgb(0xe6e6e6)
}

pub fn default_background() -> Rgba {
  rgb(0x101418)
}

pub fn default_background_rgb() -> Rgb {
  alac_rgb(0x101418)
}

pub fn cursor_color() -> Rgba {
  rgb(0xe6e6e6)
}

pub fn cursor_color_rgb() -> Rgb {
  alac_rgb(0xe6e6e6)
}

pub fn selection_color() -> Rgba {
  rgb(0x3a4d6a)
}

/// Fallback RGB palette for terminal color requests. Alacritty stores colors
/// that escape sequences override; when an app queries a color that has not
/// been overridden we answer from the same static palette used for rendering.
pub fn color_index_rgb(index: usize) -> Rgb {
  match index {
    0 => alac_rgb(0x1f2329),
    1 => alac_rgb(0xe06c75),
    2 => alac_rgb(0x98c379),
    3 => alac_rgb(0xe5c07b),
    4 => alac_rgb(0x61afef),
    5 => alac_rgb(0xc678dd),
    6 => alac_rgb(0x56b6c2),
    7 => alac_rgb(0xdcdfe4),
    8 => alac_rgb(0x4b5263),
    9 => alac_rgb(0xff7b86),
    10 => alac_rgb(0xb6e58c),
    11 => alac_rgb(0xffd58a),
    12 => alac_rgb(0x7fc4ff),
    13 => alac_rgb(0xd8a3ec),
    14 => alac_rgb(0x6cd1de),
    15 => alac_rgb(0xffffff),
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
      default_foreground_rgb()
    }
    x if x == NamedColor::Background as usize => default_background_rgb(),
    x if x == NamedColor::Cursor as usize => cursor_color_rgb(),
    x if x == NamedColor::DimForeground as usize => default_foreground_rgb(),
    x if x == NamedColor::DimBlack as usize => alac_rgb(0x15181d),
    x if x == NamedColor::DimRed as usize => alac_rgb(0x9d4c52),
    x if x == NamedColor::DimGreen as usize => alac_rgb(0x6a8955),
    x if x == NamedColor::DimYellow as usize => alac_rgb(0xa08956),
    x if x == NamedColor::DimBlue as usize => alac_rgb(0x447aa7),
    x if x == NamedColor::DimMagenta as usize => alac_rgb(0x8b549b),
    x if x == NamedColor::DimCyan as usize => alac_rgb(0x3c7f88),
    x if x == NamedColor::DimWhite as usize => alac_rgb(0x9a9ca0),
    _ => default_foreground_rgb(),
  }
}

fn named_color(name: NamedColor, fg: Rgba, bg: Rgba) -> Rgba {
  match name {
    NamedColor::Black => rgb(0x1f2329),
    NamedColor::Red => rgb(0xe06c75),
    NamedColor::Green => rgb(0x98c379),
    NamedColor::Yellow => rgb(0xe5c07b),
    NamedColor::Blue => rgb(0x61afef),
    NamedColor::Magenta => rgb(0xc678dd),
    NamedColor::Cyan => rgb(0x56b6c2),
    NamedColor::White => rgb(0xdcdfe4),

    NamedColor::BrightBlack => rgb(0x4b5263),
    NamedColor::BrightRed => rgb(0xff7b86),
    NamedColor::BrightGreen => rgb(0xb6e58c),
    NamedColor::BrightYellow => rgb(0xffd58a),
    NamedColor::BrightBlue => rgb(0x7fc4ff),
    NamedColor::BrightMagenta => rgb(0xd8a3ec),
    NamedColor::BrightCyan => rgb(0x6cd1de),
    NamedColor::BrightWhite => rgb(0xffffff),

    // Semantic names - fall back to the caller's defaults.
    NamedColor::Foreground => fg,
    NamedColor::Background => bg,
    NamedColor::Cursor => cursor_color(),

    // Any other named slot (DimBlack, etc.) maps to its plain variant
    // for now - refine when bold/dim styling lands.
    _ => fg,
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

/// 256-color palette: `0..16` → ANSI named, `16..232` → 6×6×6 RGB cube,
/// `232..256` → 24 grayscale steps. Spec from xterm.
fn indexed_color(idx: u8, fg: Rgba, bg: Rgba) -> Rgba {
  if idx < 16 {
    let named = match idx {
      0 => NamedColor::Black,
      1 => NamedColor::Red,
      2 => NamedColor::Green,
      3 => NamedColor::Yellow,
      4 => NamedColor::Blue,
      5 => NamedColor::Magenta,
      6 => NamedColor::Cyan,
      7 => NamedColor::White,
      8 => NamedColor::BrightBlack,
      9 => NamedColor::BrightRed,
      10 => NamedColor::BrightGreen,
      11 => NamedColor::BrightYellow,
      12 => NamedColor::BrightBlue,
      13 => NamedColor::BrightMagenta,
      14 => NamedColor::BrightCyan,
      _ => NamedColor::BrightWhite,
    };
    named_color(named, fg, bg)
  } else if idx < 232 {
    // 6x6x6 cube. xterm uses 0, 95, 135, 175, 215, 255 as the levels.
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
    // 24 gray steps from 8 to 238 in increments of 10.
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

fn alac_rgb(hex: u32) -> Rgb {
  Rgb {
    r: ((hex >> 16) & 0xff) as u8,
    g: ((hex >> 8) & 0xff) as u8,
    b: (hex & 0xff) as u8,
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
    let c = to_rgba(
      AlacColor::Named(NamedColor::Red),
      default_foreground(),
      default_background(),
    );
    assert!(rgba_eq(c, rgb(0xe06c75)));
  }

  #[test]
  fn named_foreground_returns_default_fg() {
    let c = to_rgba(
      AlacColor::Named(NamedColor::Foreground),
      default_foreground(),
      default_background(),
    );
    assert!(rgba_eq(c, default_foreground()));
  }

  #[test]
  fn named_background_returns_default_bg() {
    let c = to_rgba(
      AlacColor::Named(NamedColor::Background),
      default_foreground(),
      default_background(),
    );
    assert!(rgba_eq(c, default_background()));
  }

  #[test]
  fn spec_rgb_passes_through() {
    let c = to_rgba(
      AlacColor::Spec(Rgb {
        r: 0xab,
        g: 0xcd,
        b: 0xef,
      }),
      default_foreground(),
      default_background(),
    );
    assert!(approx(c.r, 0xab as f32 / 255.0));
    assert!(approx(c.g, 0xcd as f32 / 255.0));
    assert!(approx(c.b, 0xef as f32 / 255.0));
  }

  #[test]
  fn indexed_first_16_alias_named_colors() {
    for i in 0u8..16 {
      let from_indexed = to_rgba(
        AlacColor::Indexed(i),
        default_foreground(),
        default_background(),
      );
      let named = match i {
        0 => NamedColor::Black,
        1 => NamedColor::Red,
        2 => NamedColor::Green,
        3 => NamedColor::Yellow,
        4 => NamedColor::Blue,
        5 => NamedColor::Magenta,
        6 => NamedColor::Cyan,
        7 => NamedColor::White,
        8 => NamedColor::BrightBlack,
        9 => NamedColor::BrightRed,
        10 => NamedColor::BrightGreen,
        11 => NamedColor::BrightYellow,
        12 => NamedColor::BrightBlue,
        13 => NamedColor::BrightMagenta,
        14 => NamedColor::BrightCyan,
        _ => NamedColor::BrightWhite,
      };
      let from_named = to_rgba(
        AlacColor::Named(named),
        default_foreground(),
        default_background(),
      );
      assert!(
        rgba_eq(from_indexed, from_named),
        "index {i} should match named color"
      );
    }
  }

  #[test]
  fn indexed_232_is_dark_gray() {
    // 232 = level 8/255 = 0.0314.
    let c = to_rgba(
      AlacColor::Indexed(232),
      default_foreground(),
      default_background(),
    );
    assert!(approx(c.r, 8.0 / 255.0));
    assert!(approx(c.g, c.r));
    assert!(approx(c.b, c.r));
  }

  #[test]
  fn indexed_255_is_near_white_gray() {
    // 255 = level 8 + 23*10 = 238/255.
    let c = to_rgba(
      AlacColor::Indexed(255),
      default_foreground(),
      default_background(),
    );
    assert!(approx(c.r, 238.0 / 255.0));
  }

  #[test]
  fn indexed_cube_corner_is_black() {
    // 16 = (0,0,0) in the 6x6x6 cube.
    let c = to_rgba(
      AlacColor::Indexed(16),
      default_foreground(),
      default_background(),
    );
    assert!(approx(c.r, 0.0));
    assert!(approx(c.g, 0.0));
    assert!(approx(c.b, 0.0));
  }

  #[test]
  fn indexed_cube_opposite_is_white() {
    // 231 = (5,5,5) in the cube → 255,255,255.
    let c = to_rgba(
      AlacColor::Indexed(231),
      default_foreground(),
      default_background(),
    );
    assert!(approx(c.r, 1.0));
    assert!(approx(c.g, 1.0));
    assert!(approx(c.b, 1.0));
  }

  #[test]
  fn color_index_rgb_matches_primary_palette() {
    assert_eq!(color_index_rgb(1), alac_rgb(0xe06c75));
    assert_eq!(color_index_rgb(4), alac_rgb(0x61afef));
  }

  #[test]
  fn color_index_rgb_supports_cube_and_gray_fallbacks() {
    assert_eq!(color_index_rgb(16), Rgb { r: 0, g: 0, b: 0 });
    assert_eq!(
      color_index_rgb(231),
      Rgb {
        r: 255,
        g: 255,
        b: 255,
      }
    );
    assert_eq!(
      color_index_rgb(255),
      Rgb {
        r: 238,
        g: 238,
        b: 238,
      }
    );
  }

  #[test]
  fn semantic_color_indices_return_defaults() {
    assert_eq!(
      color_index_rgb(NamedColor::Foreground as usize),
      default_foreground_rgb()
    );
    assert_eq!(
      color_index_rgb(NamedColor::Background as usize),
      default_background_rgb()
    );
    assert_eq!(
      color_index_rgb(NamedColor::Cursor as usize),
      cursor_color_rgb()
    );
  }
}
