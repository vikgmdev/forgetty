//! Configuration schema definitions.
//!
//! Defines the top-level `Config` struct and all nested configuration types
//! that map to the TOML configuration file.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::theme::Theme;

/// The cursor rendering style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum CursorStyle {
    /// A filled block cursor.
    #[default]
    Block,
    /// A thin vertical bar cursor.
    Bar,
    /// A horizontal underline cursor.
    Underline,
}

/// The top-level Forgetty configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The font family name (e.g., "JetBrains Mono").
    #[serde(default = "default_font_family")]
    pub font_family: String,

    /// The font size in points.
    #[serde(default = "default_font_size")]
    pub font_size: f32,

    /// The color theme.
    #[serde(default)]
    pub theme: Theme,

    /// The shell command to launch (e.g., "/bin/zsh").
    /// If `None`, the user's default shell is used.
    #[serde(default)]
    pub shell: Option<String>,

    /// Maximum number of scrollback lines to retain.
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: usize,

    /// The cursor style.
    #[serde(default)]
    pub cursor_style: CursorStyle,

    /// Custom keybindings mapping action names to key combinations.
    #[serde(default)]
    pub keybindings: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        crate::defaults::default_config()
    }
}

fn default_font_family() -> String {
    "monospace".to_string()
}

fn default_font_size() -> f32 {
    16.0
}

fn default_scrollback_lines() -> usize {
    10_000
}
