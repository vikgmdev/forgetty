//! Color conversion and management utilities.
//!
//! Handles conversion between the various color representations used
//! throughout the terminal (RGB, theme colors) and the GPU-friendly
//! formats needed by the renderer.
//!
//! All palette lookups are now resolved by libghostty-vt before reaching
//! the renderer. The `Color` enum only has `Default` and `Rgb` variants.

use forgetty_vt::Color;

/// A color scheme mapping terminal abstract colors to concrete RGBA values.
pub struct ColorScheme {
    /// Default foreground color.
    pub foreground: [u8; 4],
    /// Default background color.
    pub background: [u8; 4],
    /// Cursor color.
    pub cursor: [u8; 4],
    /// Selection highlight color.
    pub selection: [u8; 4],
    /// The 16 standard ANSI colors (kept for theme UI, not for palette resolution).
    pub ansi: [[u8; 4]; 16],
}

impl Default for ColorScheme {
    /// Default color scheme aligned with `Theme::default()` (Catppuccin Mocha).
    fn default() -> Self {
        Self {
            foreground: [205, 214, 244, 255], // #cdd6f4
            background: [40, 40, 40, 255],    // #282828
            cursor: [245, 224, 220, 255],     // #f5e0dc
            selection: [88, 91, 112, 128],    // #585b70 with alpha
            ansi: [
                [69, 71, 90, 255],    // 0  black   #45475a
                [243, 139, 168, 255], // 1  red     #f38ba8
                [166, 227, 161, 255], // 2  green   #a6e3a1
                [249, 226, 175, 255], // 3  yellow  #f9e2af
                [137, 180, 250, 255], // 4  blue    #89b4fa
                [245, 194, 231, 255], // 5  magenta #f5c2e7
                [148, 226, 213, 255], // 6  cyan    #94e2d5
                [186, 194, 222, 255], // 7  white   #bac2de
                [88, 91, 112, 255],   // 8  bright black   #585b70
                [243, 139, 168, 255], // 9  bright red     #f38ba8
                [166, 227, 161, 255], // 10 bright green   #a6e3a1
                [249, 226, 175, 255], // 11 bright yellow  #f9e2af
                [137, 180, 250, 255], // 12 bright blue    #89b4fa
                [245, 194, 231, 255], // 13 bright magenta #f5c2e7
                [148, 226, 213, 255], // 14 bright cyan    #94e2d5
                [205, 214, 244, 255], // 15 bright white   #cdd6f4
            ],
        }
    }
}

impl ColorScheme {
    /// Build a `ColorScheme` from a config `Theme`.
    pub fn from_theme(theme: &forgetty_config::theme::Theme) -> Self {
        let to_rgba = |c: forgetty_core::Rgba| -> [u8; 4] { [c.r, c.g, c.b, c.a] };
        let mut ansi = [[0u8; 4]; 16];
        for (i, color) in theme.ansi_colors.iter().enumerate() {
            ansi[i] = to_rgba(*color);
        }
        Self {
            foreground: to_rgba(theme.foreground),
            background: to_rgba(theme.background),
            cursor: to_rgba(theme.cursor),
            selection: to_rgba(theme.selection),
            ansi,
        }
    }

    /// Resolve a terminal Color to an RGBA foreground color.
    ///
    /// Colors are pre-resolved by libghostty-vt, so this only handles
    /// `Default` (use theme foreground) and `Rgb` (use directly).
    pub fn resolve_fg(&self, color: Color) -> [u8; 4] {
        match color {
            Color::Default => self.foreground,
            Color::Rgb(r, g, b) => [r, g, b, 255],
        }
    }

    /// Resolve a terminal Color to an RGBA background color.
    ///
    /// Colors are pre-resolved by libghostty-vt, so this only handles
    /// `Default` (use theme background) and `Rgb` (use directly).
    pub fn resolve_bg(&self, color: Color) -> [u8; 4] {
        match color {
            Color::Default => self.background,
            Color::Rgb(r, g, b) => [r, g, b, 255],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_fg_default() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.resolve_fg(Color::Default), [205, 214, 244, 255]);
    }

    #[test]
    fn test_resolve_bg_default() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.resolve_bg(Color::Default), [40, 40, 40, 255]);
    }

    #[test]
    fn test_resolve_fg_rgb() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.resolve_fg(Color::Rgb(100, 200, 50)), [100, 200, 50, 255]);
    }

    #[test]
    fn test_resolve_bg_rgb() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.resolve_bg(Color::Rgb(100, 200, 50)), [100, 200, 50, 255]);
    }
}
