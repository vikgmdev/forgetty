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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ansi_colors: default_ansi_colors(),
            foreground: default_foreground(),
            background: default_background(),
            cursor: default_cursor_color(),
            selection: default_selection_color(),
        }
    }
}

fn default_foreground() -> Rgba {
    Rgba::rgb(204, 204, 204)
}

fn default_background() -> Rgba {
    Rgba::rgb(24, 24, 24)
}

fn default_cursor_color() -> Rgba {
    Rgba::rgb(255, 255, 255)
}

fn default_selection_color() -> Rgba {
    Rgba::new(100, 100, 180, 128)
}

/// Returns the default ANSI 16-color palette (roughly xterm-256color defaults).
fn default_ansi_colors() -> [Rgba; 16] {
    [
        // Normal colors (0-7)
        Rgba::rgb(0, 0, 0),       // Black
        Rgba::rgb(205, 49, 49),   // Red
        Rgba::rgb(13, 188, 121),  // Green
        Rgba::rgb(229, 229, 16),  // Yellow
        Rgba::rgb(36, 114, 200),  // Blue
        Rgba::rgb(188, 63, 188),  // Magenta
        Rgba::rgb(17, 168, 205),  // Cyan
        Rgba::rgb(204, 204, 204), // White
        // Bright colors (8-15)
        Rgba::rgb(102, 102, 102), // Bright Black
        Rgba::rgb(241, 76, 76),   // Bright Red
        Rgba::rgb(35, 209, 139),  // Bright Green
        Rgba::rgb(245, 245, 67),  // Bright Yellow
        Rgba::rgb(59, 142, 234),  // Bright Blue
        Rgba::rgb(214, 112, 214), // Bright Magenta
        Rgba::rgb(41, 184, 219),  // Bright Cyan
        Rgba::rgb(242, 242, 242), // Bright White
    ]
}
