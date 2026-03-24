//! Configuration file loading and parsing.
//!
//! Handles locating the configuration file on disk, reading it,
//! and deserializing it into a `Config` struct.

use std::path::Path;

use crate::schema::Config;
use forgetty_core::Result;

/// Loads the configuration from the given path, or from the default location.
///
/// If `path` is `Some`, reads from that file. Otherwise, looks for a config file
/// at the platform-specific default location (e.g., `~/.config/forgetty/config.toml`).
///
/// If no configuration file exists, returns the default configuration.
pub fn load_config(path: Option<&Path>) -> Result<Config> {
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let default_path = forgetty_core::platform::config_dir().join("config.toml");
            if !default_path.exists() {
                tracing::info!("No config file found, using defaults");
                return Ok(Config::default());
            }
            default_path
        }
    };

    if !config_path.exists() {
        return Err(forgetty_core::ForgettyError::Config(format!(
            "Config file not found: {}",
            config_path.display()
        )));
    }

    let contents = std::fs::read_to_string(&config_path)?;
    let config: Config = toml::from_str(&contents).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!(
            "Failed to parse {}: {}",
            config_path.display(),
            e
        ))
    })?;

    tracing::info!("Loaded config from {}", config_path.display());
    Ok(config)
}
