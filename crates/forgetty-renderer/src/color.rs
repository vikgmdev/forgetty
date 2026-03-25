//! Color conversion and management utilities.
//!
//! Handles conversion between the various color representations used
//! throughout the terminal (ANSI indices, RGB, theme colors) and the
//! GPU-friendly formats needed by the renderer.

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
    /// The 16 standard ANSI colors.
    pub ansi: [[u8; 4]; 16],
}

impl Default for ColorScheme {
    /// Catppuccin Mocha-inspired defaults.
    fn default() -> Self {
        Self {
            foreground: [205, 214, 244, 255], // #CDD6F4
            background: [30, 30, 46, 255],    // #1E1E2E
            cursor: [245, 224, 220, 255],     // #F5E0DC
            selection: [88, 91, 112, 128],    // #585B70 with alpha
            ansi: [
                [69, 71, 90, 255],    // 0  black   #45475A
                [243, 139, 168, 255], // 1  red     #F38BA8
                [166, 227, 161, 255], // 2  green   #A6E3A1
                [249, 226, 175, 255], // 3  yellow  #F9E2AF
                [137, 180, 250, 255], // 4  blue    #89B4FA
                [245, 194, 231, 255], // 5  magenta #F5C2E7
                [148, 226, 213, 255], // 6  cyan    #94E2D5
                [186, 194, 222, 255], // 7  white   #BAC2DE
                [88, 91, 112, 255],   // 8  bright black   #585B70
                [243, 139, 168, 255], // 9  bright red     #F38BA8
                [166, 227, 161, 255], // 10 bright green   #A6E3A1
                [249, 226, 175, 255], // 11 bright yellow  #F9E2AF
                [137, 180, 250, 255], // 12 bright blue    #89B4FA
                [245, 194, 231, 255], // 13 bright magenta #F5C2E7
                [148, 226, 213, 255], // 14 bright cyan    #94E2D5
                [205, 214, 244, 255], // 15 bright white   #CDD6F4
            ],
        }
    }
}

impl ColorScheme {
    /// Resolve a terminal Color to an RGBA foreground color.
    pub fn resolve_fg(&self, color: Color) -> [u8; 4] {
        match color {
            Color::Default => self.foreground,
            Color::Indexed(idx) => self.resolve_indexed(idx),
            Color::Rgb(r, g, b) => [r, g, b, 255],
        }
    }

    /// Resolve a terminal Color to an RGBA background color.
    pub fn resolve_bg(&self, color: Color) -> [u8; 4] {
        match color {
            Color::Default => self.background,
            Color::Indexed(idx) => self.resolve_indexed(idx),
            Color::Rgb(r, g, b) => [r, g, b, 255],
        }
    }

    /// Resolve a 256-color palette index to RGBA.
    fn resolve_indexed(&self, idx: u8) -> [u8; 4] {
        match idx {
            // Standard 16 ANSI colors
            0..=15 => self.ansi[idx as usize],
            // 6x6x6 color cube (indices 16-231)
            16..=231 => {
                let idx = idx - 16;
                let r = idx / 36;
                let g = (idx % 36) / 6;
                let b = idx % 6;
                // Map each component: 0 -> 0, 1 -> 95, 2 -> 135, 3 -> 175, 4 -> 215, 5 -> 255
                let to_val = |c: u8| -> u8 {
                    if c == 0 {
                        0
                    } else {
                        55 + c * 40
                    }
                };
                [to_val(r), to_val(g), to_val(b), 255]
            }
            // Grayscale ramp (indices 232-255)
            232..=255 => {
                let level = 8 + (idx - 232) * 10;
                [level, level, level, 255]
            }
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
        assert_eq!(scheme.resolve_bg(Color::Default), [30, 30, 46, 255]);
    }

    #[test]
    fn test_resolve_fg_rgb() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.resolve_fg(Color::Rgb(100, 200, 50)), [100, 200, 50, 255]);
    }

    #[test]
    fn test_resolve_fg_indexed_ansi() {
        let scheme = ColorScheme::default();
        // Index 1 = red
        assert_eq!(scheme.resolve_fg(Color::Indexed(1)), [243, 139, 168, 255]);
        // Index 0 = black
        assert_eq!(scheme.resolve_fg(Color::Indexed(0)), [69, 71, 90, 255]);
    }

    #[test]
    fn test_resolve_indexed_color_cube() {
        let scheme = ColorScheme::default();
        // Index 16 = rgb(0,0,0) in cube
        assert_eq!(scheme.resolve_fg(Color::Indexed(16)), [0, 0, 0, 255]);
        // Index 21 = rgb(0,0,255)
        assert_eq!(scheme.resolve_fg(Color::Indexed(21)), [0, 0, 255, 255]);
        // Index 196 = rgb(255,0,0)
        assert_eq!(scheme.resolve_fg(Color::Indexed(196)), [255, 0, 0, 255]);
    }

    #[test]
    fn test_resolve_indexed_grayscale() {
        let scheme = ColorScheme::default();
        // Index 232 = darkest gray
        assert_eq!(scheme.resolve_fg(Color::Indexed(232)), [8, 8, 8, 255]);
        // Index 255 = lightest gray
        assert_eq!(scheme.resolve_fg(Color::Indexed(255)), [238, 238, 238, 255]);
    }

    #[test]
    fn test_resolve_bg_indexed() {
        let scheme = ColorScheme::default();
        // bg resolution uses the same indexed palette
        assert_eq!(scheme.resolve_bg(Color::Indexed(4)), [137, 180, 250, 255]);
    }
}
