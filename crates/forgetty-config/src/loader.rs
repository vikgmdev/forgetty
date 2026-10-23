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

/// The settings that decide where a server listens and which relays it uses,
/// or none, with the value shape each one has.
const GUARDED_KEYS: [(&str, Shape); 3] = [
    ("relay", Shape::Str),
    ("relay_urls", Shape::StrList),
    ("bind_addrs", Shape::StrList),
];

#[derive(Clone, Copy)]
enum Shape {
    /// A string.
    Str,
    /// An array of strings (a single string is accepted too: `bind_addr =
    /// "10.0.0.1"` is a plausible slip for the plural key).
    StrList,
}

impl Shape {
    fn fits(self, v: &toml::Value) -> bool {
        match (self, v) {
            (Shape::Str, toml::Value::String(_)) => true,
            (Shape::StrList, toml::Value::String(_)) => true,
            (Shape::StrList, toml::Value::Array(a)) => {
                a.iter().all(|x| matches!(x, toml::Value::String(_)))
            }
            _ => false,
        }
    }
}

/// Warn about every top-level key this version does not know. A key within
/// edit distance 2 of `relay`, `relay_urls` or `bind_addrs` (`rellay`,
/// `bind_addr`) whose value has the shape of that setting (a string for
/// `relay`, an array of strings for the other two) is an error: the setting
/// would be ignored, and `relay = "none"` is what keeps a WireGuard-only
/// server off the relays. Every other unknown key only warns, so a newer build
/// that adds `retry = 3` does not stop an older server. The message names the
/// file and line.
fn check_unknown_keys(config: &Config, text: &str, path: &Path) -> Result<()> {
    for (key, value) in &config.extra {
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
        let near = GUARDED_KEYS
            .iter()
            .find(|(g, shape)| edit_distance(key, g) <= 2 && shape.fits(value));
        if let Some((near, _)) = near {
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

/// Why the file at `path` does not load, or `None` when it is missing or loads.
fn unloadable_on_disk(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(e.to_string()),
    };
    match toml::from_str::<Config>(&text) {
        Err(e) => Some(e.to_string().lines().next().unwrap_or_default().to_string()),
        Ok(c) => check_unknown_keys(&c, &text, path).err().map(|e| e.to_string()),
    }
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

    // A config.toml that does not load made the app run on defaults. Saving
    // those over it would delete the user's settings (a relay = "none" among
    // them). Refuse, and say why; the text editor (`parse_and_save_config`)
    // can still write a fixed file.
    if let Some(problem) = unloadable_on_disk(&config_path) {
        return Err(forgetty_core::ForgettyError::Config(format!(
            "not saving: {} does not load ({problem}). Fix it in the text editor or remove it",
            config_path.display()
        )));
    }

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
            ("relay_url = [\"https://r.example.org\"]\n", "relay_url"),
        ] {
            let err = load_text(text).unwrap_err().to_string();
            assert!(err.contains("config.toml:"), "{err}");
            assert!(err.contains(key) && err.contains("did you mean"), "{err}");
        }
        let err = load_text("font_size = 12.0\nrellay = \"none\"\n").unwrap_err().to_string();
        assert!(err.contains("config.toml:2"), "{err}");
    }

    #[test]
    fn a_future_key_near_relay_only_warns() {
        // Different value shapes: not a misspelling of the guarded setting.
        for text in [
            "retry = 3\n",
            "delay = 1.5\n",
            "replay = true\n",
            "ready = [1, 2]\n",
            "[reply]\nx = 1\n",
            "bind_addr = 5\n",
            "relays = [1]\n",
        ] {
            let c = load_text(text).unwrap_or_else(|e| panic!("{text:?} was refused: {e}"));
            assert!(!c.extra.is_empty(), "{text:?}");
        }
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
    fn a_config_that_does_not_load_is_not_overwritten_by_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        // Missing or fine: nothing in the way.
        assert!(unloadable_on_disk(&p).is_none());
        std::fs::write(&p, "relay = \"none\"\n").unwrap();
        assert!(unloadable_on_disk(&p).is_none());
        // A parse error, and a guarded typo, both block a save.
        std::fs::write(&p, "relay = \"none\n").unwrap();
        assert!(unloadable_on_disk(&p).is_some());
        std::fs::write(&p, "rellay = \"none\"\n").unwrap();
        assert!(unloadable_on_disk(&p).unwrap().contains("did you mean"));
    }

    #[test]
    fn known_keys_are_never_unknown() {
        let c = load_text("relay = \"none\"\nbind_addrs = [\"10.0.0.1\"]\nrelay_urls = []\n")
            .unwrap();
        assert!(c.extra.is_empty());
    }
}
