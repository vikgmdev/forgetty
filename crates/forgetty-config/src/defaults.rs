//! Default configuration values.
//!
//! Provides a sensible default configuration for users who have not yet
//! created a configuration file.

use crate::schema::{BellMode, Config, CursorStyle, NotificationMode, OnLaunch, ProfileConfig};
use std::collections::HashMap;

/// Returns the default Forgetty configuration.
///
/// Uses sensible defaults suitable for most systems:
/// - monospace font at 12pt
/// - dark theme
/// - 10,000 scrollback lines
/// - block cursor
pub fn default_config() -> Config {
    let theme = crate::theme::load_theme_by_name("0x96f").unwrap_or_default();
    Config {
        font_family: "monospace".to_string(),
        font_size: 12.0,
        theme,
        theme_name: Some("0x96f".to_string()),
        shell: None,
        scrollback_lines: 10_000,
        cursor_style: CursorStyle::Block,
        bell_mode: BellMode::Visual,
        notification_mode: NotificationMode::All,
        keybindings: HashMap::new(),
        on_launch: OnLaunch::Restore,
        profiles: Vec::<ProfileConfig>::new(),
        default_profile: None,
    }
}
