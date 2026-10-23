//! Configuration loading and management for Forgetty.
//!
//! Handles reading, parsing, and validating the TOML configuration file,
//! provides default values, and manages the theme system.

pub mod bundled_themes;
pub mod defaults;
pub mod loader;
pub mod remote;
pub mod schema;
pub mod theme;

pub use loader::{
    config_file_path, load_config, load_config_as_text, parse_and_save_config,
    parse_and_save_config_at, parse_and_save_config_checked_at, save_config, set_config_path,
    SavedConfig,
};
pub use remote::{validate_remote_settings, ConfigError, RemoteOverrides, RemoteSettings};
pub use schema::{BellMode, Config, CursorStyle, NotificationMode, OnLaunch, ProfileConfig};
pub use theme::{
    load_theme_by_name, load_theme_catalog, parse_theme_file, PreviewColors, Theme,
    ThemeCatalogEntry, ThemeSource,
};
