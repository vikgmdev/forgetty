//! Configuration file loading, parsing, and saving.
//!
//! Handles locating the configuration file on disk, reading it,
//! deserializing it into a `Config` struct, and writing updated
//! configuration back to disk.

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

/// Returns the current `config.toml` contents as a raw string.
///
/// Returns an empty string if the file does not exist yet.
pub fn load_config_as_text() -> String {
    let config_path = forgetty_core::platform::config_dir().join("config.toml");
    std::fs::read_to_string(config_path).unwrap_or_default()
}

/// Parse a TOML string as `Config`, save it to disk, and return the parsed value.
///
/// Returns an error if the text is not valid TOML or does not conform to the
/// `Config` schema. On success the config is atomically written to the default
/// config path.
pub fn parse_and_save_config(toml_text: &str) -> Result<Config> {
    let config: Config = toml::from_str(toml_text).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!("Failed to parse config: {}", e))
    })?;
    save_config(&config)?;
    Ok(config)
}

/// Saves the configuration to `~/.config/forgetty/config.toml`.
///
/// Creates the config directory if it does not exist. Uses an atomic
/// write pattern (write to temp file, then rename) to prevent partial
/// writes from confusing the `ConfigWatcher`.
pub fn save_config(config: &Config) -> Result<()> {
    let config_dir = forgetty_core::platform::config_dir();
    let config_path = config_dir.join("config.toml");

    // Ensure the config directory exists.
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir)?;
    }

    let toml_string = toml::to_string_pretty(config).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!("Failed to serialize config: {}", e))
    })?;

    // Atomic write: write to a temp file in the same directory, then rename.
    let tmp_path = config_dir.join("config.toml.tmp");
    std::fs::write(&tmp_path, &toml_string)?;
    std::fs::rename(&tmp_path, &config_path)?;

    tracing::info!("Saved config to {}", config_path.display());
    Ok(())
}
