//! Configuration file loading, parsing, and saving.
//!
//! Handles locating the configuration file on disk, reading it,
//! deserializing it into a `Config` struct, and writing updated
//! configuration back to disk.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::remote;
use crate::schema::Config;
use forgetty_core::Result;

/// The `--config-file` of this process, if any (see [`set_config_path`]).
static CONFIG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record the config file this process uses (`--config-file`), so that every
/// load and save that names no path (the settings view, a reload) uses the same
/// file and never another one. `None`: the default location.
pub fn set_config_path(path: Option<PathBuf>) {
    *CONFIG_PATH.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

/// The config file of this process: the one given to [`set_config_path`], or
/// `~/.config/forgetty/config.toml`.
pub fn config_file_path() -> PathBuf {
    CONFIG_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| forgetty_core::platform::config_dir().join("config.toml"))
}

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
            let default_path = config_file_path();
            if !default_path.exists() {
                tracing::info!("No config file found, using defaults");
                return Ok(Config { source: Some(default_path), ..Config::default() });
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
    let mut config: Config = toml::from_str(&contents).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!(
            "Failed to parse {}: {}",
            config_path.display(),
            e
        ))
    })?;

    check_unknown_keys(&config, &contents, &config_path)?;
    config.source = Some(config_path.clone());

    tracing::info!("Loaded config from {}", config_path.display());
    Ok(config)
}

/// The settings that decide where a server listens and which relays it uses,
/// or none, with the kind of value each one takes.
const GUARDED_KEYS: [(&str, Kind); 3] =
    [("relay", Kind::Relay), ("relay_urls", Kind::RelayUrls), ("bind_addrs", Kind::BindAddrs)];

#[derive(Clone, Copy)]
enum Kind {
    /// `"none"` or a relay URL.
    Relay,
    /// Relay URLs.
    RelayUrls,
    /// IP address literals.
    BindAddrs,
}

impl Kind {
    /// Would `v` be a valid value of the setting? A string is accepted for the
    /// list settings too: `bind_addr = "10.0.0.1"` is a plausible slip. The
    /// value parsers are the server's (`crate::remote`): trimmed, any case.
    fn fits(self, v: &toml::Value) -> bool {
        let one = |s: &str| match self {
            Kind::Relay => remote::is_relay_none(s) || remote::parse_relay_url(s).is_ok(),
            Kind::RelayUrls => remote::parse_relay_url(s).is_ok(),
            Kind::BindAddrs => remote::parse_bind_addr(s).is_ok(),
        };
        match (self, v) {
            (_, toml::Value::String(s)) => one(s),
            (Kind::RelayUrls | Kind::BindAddrs, toml::Value::Array(a)) => {
                !a.is_empty() && a.iter().all(|x| matches!(x, toml::Value::String(s) if one(s)))
            }
            _ => false,
        }
    }
}

/// Warn about every top-level key this version does not know. A key within
/// edit distance 2 of `relay`, `relay_urls` or `bind_addrs` (`rellay`,
/// `bind_addr`) whose value would be valid for that setting (`"none"` or a URL
/// for `relay`, URLs for `relay_urls`, IP literals for `bind_addrs`) is an
/// error: the setting would be ignored, and `relay = "none"` is what keeps a WireGuard-only
/// server off the relays. Every other unknown key only warns, so a newer build
/// that adds `retry = 3` does not stop an older server. The message names the
/// file and line.
fn check_unknown_keys(config: &Config, text: &str, path: &Path) -> Result<()> {
    for (key, value) in &config.extra {
        let line = text
            .lines()
            .position(|l| {
                let l = l.trim_start().trim_start_matches('[');
                l.strip_prefix(key.as_str())
                    .is_some_and(|rest| rest.trim_start().starts_with(['=', ']', '.']))
            })
            .map(|i| i + 1);
        let at = match line {
            Some(n) => format!("{}:{n}", path.display()),
            None => path.display().to_string(),
        };
        let near = GUARDED_KEYS
            .iter()
            .find(|(g, kind)| edit_distance(&key.to_ascii_lowercase(), g) <= 2 && kind.fits(value));
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

/// Returns the current contents of the config file ([`config_file_path`]) as a
/// raw string.
///
/// Returns an empty string if the file does not exist yet.
pub fn load_config_as_text() -> String {
    std::fs::read_to_string(config_file_path()).unwrap_or_default()
}

/// Parse a TOML string as `Config`, save it to the default config file, and
/// return the parsed value. See [`parse_and_save_config_at`].
pub fn parse_and_save_config(toml_text: &str) -> Result<Config> {
    parse_and_save_config_at(toml_text, None)
}

/// Parse a TOML string as `Config` and write it to `path` (`None`: the default
/// config file), then return the parsed value.
///
/// The new text is validated as `load_config` would: it must parse, and it
/// must pass the unknown-key check (a misspelt `relay` is an error) and the
/// server's check of the remote settings (`validate_remote_settings`). A text
/// that passes is written even when the file on disk does not load: this is
/// how a broken config is repaired in the text editor.
pub fn parse_and_save_config_at(toml_text: &str, path: Option<&Path>) -> Result<Config> {
    let config_path = target_path(path);
    let mut config: Config = toml::from_str(toml_text).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!("Failed to parse config: {}", e))
    })?;
    check_unknown_keys(&config, toml_text, &config_path)?;
    // The server refuses a config whose remote settings are invalid (exit 78):
    // do not write one. No command-line overrides here: the file must stand
    // on its own.
    remote::validate_remote_settings(&config, &remote::RemoteOverrides::default())
        .map_err(|e| forgetty_core::ForgettyError::Config(e.to_string()))?;
    config.source = Some(config_path.clone());
    write_config(&config, &config_path)?;
    Ok(config)
}

/// `path`, or the default config file.
fn target_path(path: Option<&Path>) -> std::path::PathBuf {
    match path {
        Some(p) => p.to_path_buf(),
        None => config_file_path(),
    }
}

/// Saves the configuration to the file it was loaded from (`config.source`,
/// set by `load_config`, for example from `--config-file`), or to
/// `~/.config/forgetty/config.toml`.
///
/// Creates the config directory if it does not exist. Uses an atomic
/// write pattern (write to temp file, then rename) to prevent partial
/// writes from confusing the `ConfigWatcher`.
pub fn save_config(config: &Config) -> Result<()> {
    let config_path = target_path(config.source.as_deref());

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
    write_config(config, &config_path)
}

/// Write `config` to `config_path` atomically.
fn write_config(config: &Config, config_path: &Path) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    // Ensure the config directory exists.
    if !config_dir.exists() {
        std::fs::create_dir_all(config_dir)?;
    }

    let toml_string = toml::to_string_pretty(config).map_err(|e| {
        forgetty_core::ForgettyError::Config(format!("Failed to serialize config: {}", e))
    })?;

    // Atomic write: write to a temp file in the same directory, then rename.
    let tmp_path = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, &toml_string)?;
    std::fs::rename(&tmp_path, config_path)?;

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
            ("relay_url = \"https://r.example.org\"\n", "relay_url"),
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
            "delay = \"500ms\"\n",
            "retry = \"always\"\n",
            "reply = \"hi\"\n",
            "bind_addr = [\"not an ip\"]\n",
            "relays = \"foo\"\n",
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
        let c = load_text("font_size = 13.0\nfuture_flag = true\n[future_table]\nx = 1\n").unwrap();
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
    fn the_text_editor_repairs_a_config_that_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "relay = \"none\n").unwrap(); // does not parse
        assert!(load_config(Some(&p)).is_err());
        // The settings save is refused ...
        let c = Config { source: Some(p.clone()), ..Config::default() };
        assert!(save_config(&c).is_err());
        // ... a text that is still bad is refused too, and writes nothing ...
        assert!(parse_and_save_config_at("rellay = \"none\"\n", Some(&p)).is_err());
        assert!(parse_and_save_config_at("relay = \"nne\"\n", Some(&p)).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "relay = \"none\n");
        // ... and a valid text repairs the file.
        let fixed =
            parse_and_save_config_at("relay = \"none\"\nbind_addrs = [\"10.0.0.1\"]\n", Some(&p))
                .unwrap();
        assert_eq!(fixed.relay.as_deref(), Some("none"));
        assert_eq!(load_config(Some(&p)).unwrap().relay.as_deref(), Some("none"));
        // Now ordinary saves work again, to the same file.
        let loaded = load_config(Some(&p)).unwrap();
        assert_eq!(loaded.source.as_deref(), Some(p.as_path()));
        save_config(&loaded).unwrap();
        assert_eq!(load_config(Some(&p)).unwrap().relay.as_deref(), Some("none"));
    }

    #[test]
    fn a_save_goes_back_to_the_file_the_config_was_loaded_from() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("custom.toml");
        std::fs::write(&p, "font_size = 14.0\nrelay = \"none\"\n").unwrap();
        let mut c = load_config(Some(&p)).unwrap();
        c.font_size = 15.0;
        save_config(&c).unwrap();
        let again = load_config(Some(&p)).unwrap();
        assert_eq!(again.font_size, 15.0);
        assert_eq!(again.relay.as_deref(), Some("none"));
        // A broken --config-file blocks the save to that file.
        std::fs::write(&p, "font_size = \n").unwrap();
        assert!(save_config(&c).is_err());
    }

    /// The only test that sets the process-wide config path.
    #[test]
    fn a_config_path_set_for_the_process_is_the_one_every_call_uses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("elsewhere.toml");
        set_config_path(Some(p.clone()));
        assert_eq!(config_file_path(), p);
        // No file yet: defaults, whose save goes to that file.
        let c = load_config(None).unwrap();
        assert_eq!(c.source.as_deref(), Some(p.as_path()));
        assert_eq!(load_config_as_text(), "");
        parse_and_save_config("font_size = 17.0\n").unwrap();
        assert!(load_config_as_text().contains("font_size = 17"));
        assert_eq!(load_config(None).unwrap().font_size, 17.0);
        // A load that fails leaves a default config whose save still names the
        // same file (what the apps do), and the guard refuses it.
        std::fs::write(&p, "font_size = \n").unwrap();
        assert!(load_config(None).is_err());
        let fallback = Config { source: Some(config_file_path()), ..Config::default() };
        assert!(save_config(&fallback).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "font_size = \n");
        set_config_path(None);
    }

    #[test]
    fn known_keys_are_never_unknown() {
        let c =
            load_text("relay = \"none\"\nbind_addrs = [\"10.0.0.1\"]\nrelay_urls = []\n").unwrap();
        assert!(c.extra.is_empty());
    }
}
