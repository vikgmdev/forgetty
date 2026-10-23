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

    check_unknown_keys(&config, &contents, &config_path)?;

    tracing::info!("Loaded config from {}", config_path.display());
    Ok(config)
}

/// The settings that decide where a server listens and which relays it uses, or
/// none. A misspelling of one of them must stop the load, not be ignored.
const GUARDED_KEYS: [&str; 3] = ["relay", "relay_urls", "bind_addrs"];

/// Warn about every top-level key this version does not know. A key within edit
/// distance 2 of `relay`, `relay_urls` or `bind_addrs` (`rellay`, `bind_addr`) is an error:
/// the setting would be ignored, and `relay = "none"` is what keeps a
/// WireGuard-only server off the relays. The message names the file and line.
fn check_unknown_keys(config: &Config, text: &str, path: &Path) -> Result<()> {
    for key in config.extra.keys() {
        let line = text
            .lines()
            .position(|l| {
                let l = l.trim_start().trim_start_matches('[');
                l.strip_prefix(key.as_str()).is_some_and(|rest| {
                    rest.trim_start().starts_with(['=', ']', '.'])
                })
            })
            .map(|i| i + 1);
        let at = match line {
            Some(n) => format!("{}:{n}", path.display()),
            None => path.display().to_string(),
        };
        if let Some(near) = GUARDED_KEYS.iter().find(|g| edit_distance(key, g) <= 2) {
            return Err(forgetty_core::ForgettyError::Config(format!(
                "{at}: unknown key `{key}`, did you mean `{near}`? Not starting: the setting \
                 would be ignored"
            )));
        }
        tracing::warn!("{at}: unknown config key `{key}` (ignored, kept on save)");
    }
    Ok(())
}

/// Levenshtein distance of two short strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(text: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, text).unwrap();
        load_config(Some(&p))
    }

    #[test]
    fn edit_distance_counts_edits() {
        assert_eq!(edit_distance("relay", "relay"), 0);
        assert_eq!(edit_distance("rellay", "relay"), 1);
        assert_eq!(edit_distance("bind_addr", "bind_addrs"), 1);
        assert_eq!(edit_distance("font_size", "relay"), 9);
    }

    #[test]
    fn a_typo_of_relay_or_bind_addrs_is_refused_with_file_and_line() {
        for (text, key) in [
            ("font_size = 12.0\nrellay = \"none\"\n", "rellay"),
            ("bind_addr = [\"10.0.0.1\"]\n", "bind_addr"),
            ("relays = \"none\"\n", "relays"),
            ("relay_url = \"x\"\n", "relay_url"),
        ] {
            let err = load_text(text).unwrap_err().to_string();
            assert!(err.contains("config.toml:"), "{err}");
            assert!(err.contains(key) && err.contains("did you mean"), "{err}");
        }
        let err = load_text("font_size = 12.0\nrellay = \"none\"\n").unwrap_err().to_string();
        assert!(err.contains("config.toml:2"), "{err}");
    }

    #[test]
    fn other_unknown_keys_load_and_survive_a_serialize_round_trip() {
        let c = load_text("font_size = 13.0\nfuture_flag = true\n[future_table]\nx = 1\n")
            .unwrap();
        assert_eq!(c.extra.len(), 2);
        let text = toml::to_string_pretty(&c).unwrap();
        let again: Config = toml::from_str(&text).unwrap();
        assert_eq!(again.extra, c.extra, "{text}");
        assert_eq!(again.font_size, 13.0);
    }

    #[test]
    fn known_keys_are_never_unknown() {
        let c = load_text("relay = \"none\"\nbind_addrs = [\"10.0.0.1\"]\nrelay_urls = []\n")
            .unwrap();
        assert!(c.extra.is_empty());
    }
}
