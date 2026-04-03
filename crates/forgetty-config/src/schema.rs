//! Configuration schema definitions.
//!
//! Defines the top-level `Config` struct and all nested configuration types
//! that map to the TOML configuration file.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::theme::Theme;

/// The bell mode -- how the terminal responds to BEL (0x07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum BellMode {
    /// A brief visual flash overlay on the terminal pane.
    #[default]
    Visual,
    /// An audible system beep.
    Audio,
    /// Both visual flash and audio beep.
    Both,
    /// Bell is silently ignored.
    None,
}

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
    /// An unfilled (hollow) block cursor outline.
    #[serde(alias = "block_hollow")]
    BlockHollow,
}

/// The notification mode -- controls which notification outputs fire on OSC/BEL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NotificationMode {
    /// Ring + tab badge + desktop notification for OSC; ring + badge for BEL.
    #[default]
    All,
    /// Ring and tab badge only; desktop notifications are suppressed.
    RingOnly,
    /// All notifications (ring, badge, desktop) are suppressed.
    None,
}

/// The behavior when Forgetty is launched with no explicit flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnLaunch {
    /// Restore all saved sessions (default, like Chrome).
    #[default]
    Restore,
    /// Always open a fresh window, ignoring saved sessions.
    New,
}

/// The top-level Forgetty configuration.
///
/// Supports two theme formats for backward compatibility:
/// - **New format:** `theme = "Theme Name"` (a string referencing a named theme)
/// - **Old format:** `[theme]` inline table with full color values
///
/// On load, `theme_name` is resolved to a `Theme` struct. On save, only
/// `theme_name` is written (not inline colors), keeping `config.toml` clean.
#[derive(Debug, Clone)]
pub struct Config {
    /// The font family name (e.g., "JetBrains Mono").
    pub font_family: String,

    /// The font size in points.
    pub font_size: f32,

    /// The resolved color theme (not serialized directly).
    pub theme: Theme,

    /// The theme name for config persistence (e.g., "Dracula", "Default Dark").
    /// When `Some`, this is what gets written to `config.toml`.
    /// When `None`, the inline `[theme]` format was used (legacy).
    pub theme_name: Option<String>,

    /// The shell command to launch (e.g., "/bin/zsh").
    /// If `None`, the user's default shell is used.
    pub shell: Option<String>,

    /// Maximum number of scrollback lines to retain.
    pub scrollback_lines: usize,

    /// The cursor style.
    pub cursor_style: CursorStyle,

    /// The bell mode (visual, audio, both, or none).
    pub bell_mode: BellMode,

    /// The notification mode (all, ringonly, none).
    pub notification_mode: NotificationMode,

    /// Custom keybindings mapping action names to key combinations.
    pub keybindings: HashMap<String, String>,

    /// Behavior on bare launch (no flags). `Restore` (default) restores all
    /// saved sessions; `New` always opens a fresh window.
    pub on_launch: OnLaunch,
}

impl Default for Config {
    fn default() -> Self {
        crate::defaults::default_config()
    }
}

// ---------------------------------------------------------------------------
// Custom Serialize: writes `theme = "name"` when theme_name is set,
// otherwise falls back to inline `[theme]` table.
// ---------------------------------------------------------------------------

impl Serialize for Config {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // Count fields: font_family, font_size, theme/theme_name, shell?,
        // scrollback_lines, cursor_style, bell_mode, notification_mode, on_launch, keybindings?
        let mut len = 6; // font_family, font_size, theme, scrollback_lines, cursor_style, on_launch
        len += 2; // bell_mode, notification_mode
        if self.shell.is_some() {
            len += 1;
        }
        if !self.keybindings.is_empty() {
            len += 1;
        }

        let mut map = serializer.serialize_map(Some(len))?;

        map.serialize_entry("font_family", &self.font_family)?;
        map.serialize_entry("font_size", &self.font_size)?;

        // Write theme_name as `theme = "Name"` or fall back to inline theme.
        if let Some(ref name) = self.theme_name {
            map.serialize_entry("theme", name)?;
        } else {
            map.serialize_entry("theme", &self.theme)?;
        }

        if let Some(ref shell) = self.shell {
            map.serialize_entry("shell", shell)?;
        }

        map.serialize_entry("scrollback_lines", &self.scrollback_lines)?;
        map.serialize_entry("cursor_style", &self.cursor_style)?;
        map.serialize_entry("bell_mode", &self.bell_mode)?;
        map.serialize_entry("notification_mode", &self.notification_mode)?;
        map.serialize_entry("on_launch", &self.on_launch)?;

        if !self.keybindings.is_empty() {
            map.serialize_entry("keybindings", &self.keybindings)?;
        }

        map.end()
    }
}

// ---------------------------------------------------------------------------
// Custom Deserialize: accepts both `theme = "Name"` and `[theme]` inline table.
// ---------------------------------------------------------------------------

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Raw intermediate representation that handles both theme formats.
        #[derive(Deserialize)]
        struct RawConfig {
            #[serde(default = "default_font_family")]
            font_family: String,

            #[serde(default = "default_font_size")]
            font_size: f32,

            /// Accepts either a string ("Dracula") or an inline theme table.
            #[serde(default)]
            theme: Option<toml::Value>,

            #[serde(default)]
            shell: Option<String>,

            #[serde(default = "default_scrollback_lines")]
            scrollback_lines: usize,

            #[serde(default)]
            cursor_style: CursorStyle,

            #[serde(default)]
            bell_mode: BellMode,

            #[serde(default)]
            notification_mode: NotificationMode,

            #[serde(default)]
            keybindings: HashMap<String, String>,

            /// Deserialized as raw string so unknown values can fall back to
            /// the default with a warning rather than failing to load config.
            #[serde(default)]
            on_launch: Option<String>,
        }

        let raw = RawConfig::deserialize(deserializer)?;

        let (theme, theme_name) = match raw.theme {
            Some(toml::Value::String(name)) => {
                // New format: theme = "Name"
                let resolved = crate::theme::load_theme_by_name(&name).unwrap_or_else(|| {
                    tracing::warn!("Theme '{name}' not found, falling back to default");
                    Theme::default()
                });
                (resolved, Some(name))
            }
            Some(table @ toml::Value::Table(_)) => {
                // Old format: [theme] inline table
                let theme: Theme = table.try_into().unwrap_or_else(|e| {
                    tracing::warn!("Failed to parse inline theme: {e}, using default");
                    Theme::default()
                });
                (theme, None)
            }
            _ => (Theme::default(), None),
        };

        let on_launch = match raw.on_launch.as_deref() {
            None | Some("restore") => OnLaunch::Restore,
            Some("new") => OnLaunch::New,
            Some(unknown) => {
                tracing::warn!(
                    "Unknown on_launch value {:?}, falling back to \"restore\"",
                    unknown
                );
                OnLaunch::Restore
            }
        };

        Ok(Config {
            font_family: raw.font_family,
            font_size: raw.font_size,
            theme,
            theme_name,
            shell: raw.shell,
            scrollback_lines: raw.scrollback_lines,
            cursor_style: raw.cursor_style,
            bell_mode: raw.bell_mode,
            notification_mode: raw.notification_mode,
            keybindings: raw.keybindings,
            on_launch,
        })
    }
}

fn default_font_family() -> String {
    "monospace".to_string()
}

fn default_font_size() -> f32 {
    12.0
}

fn default_scrollback_lines() -> usize {
    10_000
}
