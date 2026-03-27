//! Theme definitions for terminal colors.
//!
//! Defines the color scheme used by the terminal, including ANSI colors,
//! foreground/background defaults, cursor color, and selection highlight.

use forgetty_core::Rgba;
use serde::{Deserialize, Serialize};

/// A terminal color theme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// The 16 standard ANSI colors (0-7 normal, 8-15 bright).
    #[serde(default = "default_ansi_colors")]
    pub ansi_colors: [Rgba; 16],

    /// The default foreground color.
    #[serde(default = "default_foreground")]
    pub foreground: Rgba,

    /// The default background color.
    #[serde(default = "default_background")]
    pub background: Rgba,

    /// The cursor color.
    #[serde(default = "default_cursor_color")]
    pub cursor: Rgba,

    /// The selection highlight color.
    #[serde(default = "default_selection_color")]
    pub selection: Rgba,

    /// The search match highlight color (non-focused matches).
    #[serde(default = "default_search_match_color")]
    pub search_match: Rgba,

    /// The search match highlight color for the currently focused match.
    #[serde(default = "default_search_current_color")]
    pub search_current: Rgba,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ansi_colors: default_ansi_colors(),
            foreground: default_foreground(),
            background: default_background(),
            cursor: default_cursor_color(),
            selection: default_selection_color(),
            search_match: default_search_match_color(),
            search_current: default_search_current_color(),
        }
    }
}

fn default_foreground() -> Rgba {
    Rgba::rgb(205, 214, 244) // Catppuccin Mocha #cdd6f4
}

fn default_background() -> Rgba {
    Rgba::rgb(40, 40, 40) // Neutral dark #282828
}

fn default_cursor_color() -> Rgba {
    Rgba::rgb(245, 224, 220) // Catppuccin Mocha #f5e0dc
}

fn default_selection_color() -> Rgba {
    Rgba::new(88, 91, 112, 128) // Catppuccin Mocha #585b70 with alpha
}

fn default_search_match_color() -> Rgba {
    Rgba::new(249, 226, 175, 80) // Warm amber, semi-transparent (Catppuccin Yellow)
}

fn default_search_current_color() -> Rgba {
    Rgba::new(250, 179, 135, 160) // Brighter orange, more opaque (Catppuccin Peach)
}

/// Returns the default ANSI 16-color palette (Catppuccin Mocha).
fn default_ansi_colors() -> [Rgba; 16] {
    [
        // Normal colors (0-7)
        Rgba::rgb(69, 71, 90),    // Black   #45475a
        Rgba::rgb(243, 139, 168), // Red     #f38ba8
        Rgba::rgb(166, 227, 161), // Green   #a6e3a1
        Rgba::rgb(249, 226, 175), // Yellow  #f9e2af
        Rgba::rgb(137, 180, 250), // Blue    #89b4fa
        Rgba::rgb(245, 194, 231), // Magenta #f5c2e7
        Rgba::rgb(148, 226, 213), // Cyan    #94e2d5
        Rgba::rgb(186, 194, 222), // White   #bac2de
        // Bright colors (8-15)
        Rgba::rgb(88, 91, 112),   // Bright Black   #585b70
        Rgba::rgb(243, 139, 168), // Bright Red     #f38ba8
        Rgba::rgb(166, 227, 161), // Bright Green   #a6e3a1
        Rgba::rgb(249, 226, 175), // Bright Yellow  #f9e2af
        Rgba::rgb(137, 180, 250), // Bright Blue    #89b4fa
        Rgba::rgb(245, 194, 231), // Bright Magenta #f5c2e7
        Rgba::rgb(148, 226, 213), // Bright Cyan    #94e2d5
        Rgba::rgb(205, 214, 244), // Bright White   #cdd6f4
    ]
}
