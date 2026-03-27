//! Default configuration values.
//!
//! Provides a sensible default configuration for users who have not yet
//! created a configuration file.

use crate::schema::{Config, CursorStyle};
use crate::theme::Theme;
use std::collections::HashMap;

/// Returns the default Forgetty configuration.
///
/// Uses sensible defaults suitable for most systems:
/// - monospace font at 12pt
/// - dark theme
/// - 10,000 scrollback lines
/// - block cursor
pub fn default_config() -> Config {
    Config {
        font_family: "monospace".to_string(),
        font_size: 12.0,
        theme: Theme::default(),
        shell: None,
        scrollback_lines: 10_000,
        cursor_style: CursorStyle::Block,
        keybindings: HashMap::new(),
    }
}
