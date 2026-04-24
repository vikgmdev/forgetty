//! Theme definitions for terminal colors.
//!
//! Defines the color scheme used by the terminal, including ANSI colors,
//! foreground/background defaults, cursor color, and selection highlight.
//! Also provides the theme catalog API for loading bundled and custom themes.

use std::path::PathBuf;

use forgetty_core::Rgba;
use serde::{Deserialize, Serialize};

use crate::bundled_themes::BUNDLED_THEMES;

/// A terminal color theme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// The 16 standard ANSI colors (0-7 normal, 8-15 bright).
    #[serde(default = "default_ansi_colors")]
    pub ansi_colors: [Rgba; 16],

    /// The default foreground color.
    #[serde(default = "default_foreground")]
    pub foreground: Rgba,

    /// The default background color.
    #[serde(default = "default_background")]
    pub background: Rgba,

    /// The cursor color.
    #[serde(default = "default_cursor_color")]
    pub cursor: Rgba,

    /// The selection highlight color.
    #[serde(default = "default_selection_color")]
    pub selection: Rgba,

    /// The search match highlight color (non-focused matches).
    #[serde(default = "default_search_match_color")]
    pub search_match: Rgba,

    /// The search match highlight color for the currently focused match.
    #[serde(default = "default_search_current_color")]
    pub search_current: Rgba,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ansi_colors: default_ansi_colors(),
            foreground: default_foreground(),
            background: default_background(),
            cursor: default_cursor_color(),
            selection: default_selection_color(),
            search_match: default_search_match_color(),
            search_current: default_search_current_color(),
        }
    }
}

fn default_foreground() -> Rgba {
    Rgba::rgb(205, 214, 244) // Catppuccin Mocha #cdd6f4
}

fn default_background() -> Rgba {
    Rgba::rgb(40, 40, 40) // Neutral dark #282828
}

fn default_cursor_color() -> Rgba {
    Rgba::rgb(245, 224, 220) // Catppuccin Mocha #f5e0dc
}

fn default_selection_color() -> Rgba {
    Rgba::new(88, 91, 112, 128) // Catppuccin Mocha #585b70 with alpha
}

fn default_search_match_color() -> Rgba {
    Rgba::new(249, 226, 175, 80) // Warm amber, semi-transparent (Catppuccin Yellow)
}

fn default_search_current_color() -> Rgba {
    Rgba::new(250, 179, 135, 160) // Brighter orange, more opaque (Catppuccin Peach)
}

// ---------------------------------------------------------------------------
// Theme file parsing — matches the `[colors]` / `[colors.ansi]` / `[colors.bright]` format
// ---------------------------------------------------------------------------

/// Named ANSI colors (indices 0-7).
#[derive(Debug, Clone, Deserialize)]
struct AnsiColors {
    black: Rgba,
    red: Rgba,
    green: Rgba,
    yellow: Rgba,
    blue: Rgba,
    magenta: Rgba,
    cyan: Rgba,
    white: Rgba,
}

/// Named bright colors (indices 8-15).
#[derive(Debug, Clone, Deserialize)]
struct BrightColors {
    black: Rgba,
    red: Rgba,
    green: Rgba,
    yellow: Rgba,
    blue: Rgba,
    magenta: Rgba,
    cyan: Rgba,
    white: Rgba,
}

/// The `[colors]` table in a theme TOML file.
#[derive(Debug, Clone, Deserialize)]
struct ThemeColors {
    foreground: Rgba,
    background: Rgba,
    cursor: Option<Rgba>,
    selection: Option<Rgba>,
    ansi: Option<AnsiColors>,
    bright: Option<BrightColors>,
}

/// Top-level structure of a `.toml` theme file.
#[derive(Debug, Clone, Deserialize)]
struct ThemeFile {
    colors: ThemeColors,
}

impl ThemeFile {
    fn into_theme(self) -> Theme {
        let c = self.colors;

        let default = default_ansi_colors();

        let ansi_normal: [Rgba; 8] = match c.ansi {
            Some(a) => [a.black, a.red, a.green, a.yellow, a.blue, a.magenta, a.cyan, a.white],
            None => [
                default[0], default[1], default[2], default[3], default[4], default[5], default[6],
                default[7],
            ],
        };

        let ansi_bright: [Rgba; 8] = match c.bright {
            Some(b) => [b.black, b.red, b.green, b.yellow, b.blue, b.magenta, b.cyan, b.white],
            None => [
                default[8],
                default[9],
                default[10],
                default[11],
                default[12],
                default[13],
                default[14],
                default[15],
            ],
        };

        let mut ansi_colors = [Rgba::rgb(0, 0, 0); 16];
        ansi_colors[..8].copy_from_slice(&ansi_normal);
        ansi_colors[8..].copy_from_slice(&ansi_bright);

        Theme {
            ansi_colors,
            foreground: c.foreground,
            background: c.background,
            cursor: c.cursor.unwrap_or_else(default_cursor_color),
            selection: c.selection.unwrap_or_else(default_selection_color),
            search_match: default_search_match_color(),
            search_current: default_search_current_color(),
        }
    }
}

/// Parse a theme from a `.toml` file's contents (the `[colors]` file format).
///
/// This is the public entry point used by the preferences UI and anywhere else
/// that needs to load bundled or user-provided theme files.
pub fn parse_theme_file(contents: &str) -> Result<Theme, toml::de::Error> {
    let tf: ThemeFile = toml::from_str(contents)?;
    Ok(tf.into_theme())
}

// ---------------------------------------------------------------------------
// Theme catalog: bundled + custom theme discovery
// ---------------------------------------------------------------------------

/// Aliases for commonly requested theme names that differ from upstream names.
/// Maps (alias_display_name → upstream_display_name).
const THEME_ALIASES: &[(&str, &str)] = &[
    ("Solarized Dark", "iTerm2 Solarized Dark"),
    ("Solarized Light", "iTerm2 Solarized Light"),
    ("Tokyo Night", "TokyoNight"),
    ("One Dark", "One Dark Two"),
    ("One Light", "Atom One Light"),
    ("Monokai", "Monokai Classic"),
    ("Ayu Dark", "Ayu"),
    ("Everforest", "Everforest Dark Hard"),
    ("Kanagawa", "Kanagawa Wave"),
];

/// Where a theme originated.
#[derive(Debug, Clone)]
pub enum ThemeSource {
    /// Built into the binary via `include_str!`.
    Bundled,
    /// User-provided file in `~/.config/forgetty/themes/`.
    Custom(PathBuf),
}

/// Lightweight preview colors for a theme swatch (bg, fg, + 6 ANSI).
#[derive(Debug, Clone)]
pub struct PreviewColors {
    pub background: Rgba,
    pub foreground: Rgba,
    /// First 6 ANSI colors: black, red, green, yellow, blue, magenta.
    pub ansi_sample: [Rgba; 6],
}

/// A single entry in the theme catalog (name + swatch, no full parse).
#[derive(Debug, Clone)]
pub struct ThemeCatalogEntry {
    pub name: String,
    pub source: ThemeSource,
    pub preview_colors: PreviewColors,
}

/// Extract `PreviewColors` from a TOML theme string.
///
/// This does a full parse (same cost as `parse_theme_file`) but only returns
/// the lightweight preview data. Themes are small (~400 bytes) so full parse
/// is fast enough for 500+ themes.
fn preview_from_toml(toml_str: &str) -> Option<PreviewColors> {
    let theme = parse_theme_file(toml_str).ok()?;
    Some(PreviewColors {
        background: theme.background,
        foreground: theme.foreground,
        ansi_sample: [
            theme.ansi_colors[0],
            theme.ansi_colors[1],
            theme.ansi_colors[2],
            theme.ansi_colors[3],
            theme.ansi_colors[4],
            theme.ansi_colors[5],
        ],
    })
}

/// Load the full theme catalog: bundled themes + user custom themes.
///
/// - Bundled themes come from `BUNDLED_THEMES` (compile-time embedded).
/// - Custom themes come from `~/.config/forgetty/themes/*.toml`.
/// - Custom themes override bundled ones with the same display name.
/// - "Default Dark" is always pinned to the top.
/// - All other entries are sorted alphabetically by name.
pub fn load_theme_catalog() -> Vec<ThemeCatalogEntry> {
    let mut entries: Vec<ThemeCatalogEntry> = Vec::with_capacity(BUNDLED_THEMES.len() + 16);

    // 1. Add "Default Dark" (hardcoded fallback theme, not in BUNDLED_THEMES).
    let default_theme = Theme::default();
    entries.push(ThemeCatalogEntry {
        name: "Default Dark".to_string(),
        source: ThemeSource::Bundled,
        preview_colors: PreviewColors {
            background: default_theme.background,
            foreground: default_theme.foreground,
            ansi_sample: [
                default_theme.ansi_colors[0],
                default_theme.ansi_colors[1],
                default_theme.ansi_colors[2],
                default_theme.ansi_colors[3],
                default_theme.ansi_colors[4],
                default_theme.ansi_colors[5],
            ],
        },
    });

    // 2. Load bundled themes.
    let mut bundled: Vec<ThemeCatalogEntry> = Vec::with_capacity(BUNDLED_THEMES.len());
    for &(name, toml_str) in BUNDLED_THEMES {
        match preview_from_toml(toml_str) {
            Some(preview) => {
                bundled.push(ThemeCatalogEntry {
                    name: name.to_string(),
                    source: ThemeSource::Bundled,
                    preview_colors: preview,
                });
            }
            None => {
                tracing::warn!("Skipping malformed bundled theme: {name}");
            }
        }
    }
    bundled.sort_by_key(|t| t.name.to_lowercase());

    // 3. Load custom themes from ~/.config/forgetty/themes/
    let mut custom: Vec<ThemeCatalogEntry> = Vec::new();
    let themes_dir = forgetty_core::platform::config_dir().join("themes");
    if themes_dir.is_dir() {
        if let Ok(read_dir) = std::fs::read_dir(&themes_dir) {
            for entry in read_dir.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml") {
                    match std::fs::read_to_string(&path) {
                        Ok(contents) => {
                            // Extract display name from `name = "..."` field, or use file stem.
                            let display_name = extract_name_field(&contents).unwrap_or_else(|| {
                                path.file_stem().unwrap_or_default().to_string_lossy().to_string()
                            });

                            match preview_from_toml(&contents) {
                                Some(preview) => {
                                    custom.push(ThemeCatalogEntry {
                                        name: display_name,
                                        source: ThemeSource::Custom(path),
                                        preview_colors: preview,
                                    });
                                }
                                None => {
                                    tracing::warn!(
                                        "Skipping malformed custom theme: {}",
                                        path.display()
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Skipping unreadable custom theme {}: {e}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
    }
    custom.sort_by_key(|t| t.name.to_lowercase());

    // 4. Merge: custom themes override bundled ones with the same name.
    //    Build a set of custom names for dedup.
    let custom_names: std::collections::HashSet<String> =
        custom.iter().map(|e| e.name.to_lowercase()).collect();

    // Add bundled themes that are NOT overridden by custom.
    for entry in bundled {
        if !custom_names.contains(&entry.name.to_lowercase()) {
            entries.push(entry);
        }
    }

    // Add all custom themes.
    for entry in custom {
        entries.push(entry);
    }

    // 5. Add alias entries: if the alias name is not already taken by a custom
    //    theme, create a catalog entry that points to the upstream theme's data.
    for &(alias_name, upstream_name) in THEME_ALIASES {
        // Skip if a custom theme already uses this alias name.
        if custom_names.contains(&alias_name.to_lowercase()) {
            continue;
        }
        // Skip if the alias name is already present (e.g., upstream renamed it).
        if entries.iter().any(|e| e.name.eq_ignore_ascii_case(alias_name)) {
            continue;
        }
        // Find the upstream entry and clone it with the alias name.
        if let Some(upstream) = entries.iter().find(|e| e.name == upstream_name) {
            entries.push(ThemeCatalogEntry {
                name: alias_name.to_string(),
                source: upstream.source.clone(),
                preview_colors: upstream.preview_colors.clone(),
            });
        }
    }

    // 6. Sort everything alphabetically.
    entries.sort_by_key(|e| e.name.to_lowercase());

    entries
}

/// Load a full `Theme` struct by display name.
///
/// Search order: custom themes first, then bundled, then "Default Dark".
/// Returns `None` if the name is not found.
pub fn load_theme_by_name(name: &str) -> Option<Theme> {
    if name == "Default Dark" {
        return Some(Theme::default());
    }

    // Resolve alias to upstream name (if applicable).
    let resolved = THEME_ALIASES
        .iter()
        .find(|&&(alias, _)| alias == name)
        .map(|&(_, upstream)| upstream)
        .unwrap_or(name);

    // Check custom themes first (override bundled).
    let themes_dir = forgetty_core::platform::config_dir().join("themes");
    if themes_dir.is_dir() {
        if let Ok(read_dir) = std::fs::read_dir(&themes_dir) {
            for entry in read_dir.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml") {
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        let display_name = extract_name_field(&contents).unwrap_or_else(|| {
                            path.file_stem().unwrap_or_default().to_string_lossy().to_string()
                        });
                        if display_name == name || display_name == resolved {
                            return parse_theme_file(&contents).ok();
                        }
                    }
                }
            }
        }
    }

    // Check bundled themes (try both original name and resolved alias).
    for &(bundled_name, toml_str) in BUNDLED_THEMES {
        if bundled_name == name || bundled_name == resolved {
            return parse_theme_file(toml_str).ok();
        }
    }

    None
}

/// Extract the `name = "..."` field from a theme TOML string.
fn extract_name_field(toml_str: &str) -> Option<String> {
    for line in toml_str.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("name") {
            if let Some(eq_pos) = trimmed.find('=') {
                let value = trimmed[eq_pos + 1..].trim();
                // Strip surrounding quotes.
                if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                    return Some(value[1..value.len() - 1].to_string());
                }
            }
        }
    }
    None
}

/// Returns the default ANSI 16-color palette (Catppuccin Mocha).
fn default_ansi_colors() -> [Rgba; 16] {
    [
        // Normal colors (0-7)
        Rgba::rgb(69, 71, 90),    // Black   #45475a
        Rgba::rgb(243, 139, 168), // Red     #f38ba8
        Rgba::rgb(166, 227, 161), // Green   #a6e3a1
        Rgba::rgb(249, 226, 175), // Yellow  #f9e2af
        Rgba::rgb(137, 180, 250), // Blue    #89b4fa
        Rgba::rgb(245, 194, 231), // Magenta #f5c2e7
        Rgba::rgb(148, 226, 213), // Cyan    #94e2d5
        Rgba::rgb(186, 194, 222), // White   #bac2de
        // Bright colors (8-15)
        Rgba::rgb(88, 91, 112),   // Bright Black   #585b70
        Rgba::rgb(243, 139, 168), // Bright Red     #f38ba8
        Rgba::rgb(166, 227, 161), // Bright Green   #a6e3a1
        Rgba::rgb(249, 226, 175), // Bright Yellow  #f9e2af
        Rgba::rgb(137, 180, 250), // Bright Blue    #89b4fa
        Rgba::rgb(245, 194, 231), // Bright Magenta #f5c2e7
        Rgba::rgb(148, 226, 213), // Bright Cyan    #94e2d5
        Rgba::rgb(205, 214, 244), // Bright White   #cdd6f4
    ]
}
