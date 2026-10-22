//! `%APPDATA%\forgetty\config.toml`. Every key is optional.
//!
//! ```toml
//! shell = "pwsh.exe"
//! scrollback_bytes = 10485760   # libghostty bytes per pane (default 10 MiB)
//! backdrop = "mica"             # mica | mica_alt | blur | none
//! sidebar = false               # start with the sidebar open
//! pane_padding_x = 6.0          # px between a pane border and its text (default 6)
//! pane_padding_y = 4.0          # default 4
//! [font]
//! family = "Cascadia Mono"
//! size = 11.0            # points
//! [theme]
//! foreground = "#cccccc"
//! background = "#0c0c0c"
//! cursor     = "#ffffff"
//! selection  = "#264f78"
//! accent     = "#60cdff"
//! ansi = ["#0c0c0c", "#c50f1f", ... 16 entries]
//! [keybindings]            # action = "chord" | ["chord", ...] | "none"
//! new_tab = "ctrl+shift+t"
//! split_right = ["alt+shift+plus", "ctrl+shift+d"]
//! close_pane = "none"
//! ```
//! Action names: see `forgetty_shell_core::keys::ACTIONS`.

use std::collections::HashMap;
use std::path::PathBuf;

use forgetty_vt::DEFAULT_MAX_SCROLLBACK_BYTES;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    shell: Option<String>,
    /// Legacy W1 key, in lines. `scrollback_bytes` wins when both are set.
    scrollback: Option<usize>,
    scrollback_bytes: Option<usize>,
    backdrop: Option<String>,
    sidebar: Option<bool>,
    pane_padding_x: Option<f32>,
    pane_padding_y: Option<f32>,
    keybindings: HashMap<String, KeyValue>,
    font: FontSection,
    theme: ThemeSection,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeyValue {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FontSection {
    family: Option<String>,
    size: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThemeSection {
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    selection: Option<String>,
    accent: Option<String>,
    ansi: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    Mica,
    MicaAlt,
    Blur,
    None,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub shell: Option<String>,
    /// Scrollback budget per pane in libghostty-vt bytes.
    pub scrollback_bytes: usize,
    pub backdrop: Backdrop,
    pub sidebar_open: bool,
    /// Space between a pane's border and its cell grid, in px.
    pub pane_padding_x: f32,
    pub pane_padding_y: f32,
    /// Action name to chords, from `[keybindings]`. Overrides the defaults.
    pub keybindings: HashMap<String, Vec<String>>,
    pub accent: u32,
    pub font_family: String,
    /// Points. GPUI sizes are px at 96 dpi, so px = pt * 96 / 72.
    pub font_size_pt: f32,
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: [u32; 16],
}

/// Windows Terminal "Campbell".
pub const CAMPBELL: [u32; 16] = [
    0x0c0c0c, 0xc50f1f, 0x13a10e, 0xc19c00, 0x0037da, 0x881798, 0x3a96dd, 0xcccccc, 0x767676,
    0xe74856, 0x16c60c, 0xf9f1a5, 0x3b78ff, 0xb4009e, 0x61d6d6, 0xf2f2f2,
];

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: None,
            scrollback_bytes: DEFAULT_MAX_SCROLLBACK_BYTES,
            backdrop: Backdrop::Mica,
            sidebar_open: false,
            pane_padding_x: 6.0,
            pane_padding_y: 4.0,
            keybindings: HashMap::new(),
            accent: 0x60cdff,
            font_family: "Cascadia Mono".into(),
            font_size_pt: 11.0,
            foreground: 0xcccccc,
            background: 0x0c0c0c,
            cursor: 0xffffff,
            selection: 0x264f78,
            ansi: CAMPBELL,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FORGETTY_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(base.join("forgetty").join("config.toml"))
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('#'), 16).ok().filter(|v| *v <= 0xffffff)
}

impl Config {
    /// Load the user config. Missing file or parse errors fall back to defaults
    /// (the error text is returned so the caller can log it).
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = config_path() else { return (Self::default(), None) };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::from_toml(&text) {
                Ok(c) => (c, None),
                Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => (Self::default(), None),
        }
    }

    pub fn from_toml(text: &str) -> Result<Self, String> {
        let f: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut c = Self::default();
        c.shell = f.shell;
        if let Some(n) = f.scrollback.filter(|n| *n <= 1_000_000) {
            // W1 compatibility: ~2 KB of libghostty page memory per line.
            c.scrollback_bytes = n * 2048;
        }
        if let Some(n) = f.scrollback_bytes.filter(|n| (64 * 1024..=4usize << 30).contains(n)) {
            c.scrollback_bytes = n;
        }
        if let Some(b) = f.backdrop.as_deref() {
            c.backdrop = match b.to_ascii_lowercase().as_str() {
                "mica" => Backdrop::Mica,
                "mica_alt" | "micaalt" => Backdrop::MicaAlt,
                "blur" | "acrylic" => Backdrop::Blur,
                "none" | "solid" | "opaque" => Backdrop::None,
                other => return Err(format!("backdrop: unknown value '{other}'")),
            };
        }
        if let Some(v) = f.sidebar {
            c.sidebar_open = v;
        }
        if let Some(v) = f.pane_padding_x.filter(|v| (0.0..=64.0).contains(v)) {
            c.pane_padding_x = v;
        }
        if let Some(v) = f.pane_padding_y.filter(|v| (0.0..=64.0).contains(v)) {
            c.pane_padding_y = v;
        }
        for (name, v) in f.keybindings {
            let keys = match v {
                KeyValue::One(s) => vec![s],
                KeyValue::Many(v) => v,
            };
            c.keybindings.insert(name, keys);
        }
        if let Some(v) = f.font.family {
            c.font_family = v;
        }
        if let Some(v) = f.font.size.filter(|v| *v >= 4.0 && *v <= 96.0) {
            c.font_size_pt = v;
        }
        let set = |dst: &mut u32, src: Option<String>| {
            if let Some(v) = src.as_deref().and_then(hex) {
                *dst = v;
            }
        };
        set(&mut c.foreground, f.theme.foreground);
        set(&mut c.background, f.theme.background);
        set(&mut c.cursor, f.theme.cursor);
        set(&mut c.selection, f.theme.selection);
        set(&mut c.accent, f.theme.accent);
        if let Some(list) = f.theme.ansi {
            for (i, s) in list.iter().take(16).enumerate() {
                if let Some(v) = hex(s) {
                    c.ansi[i] = v;
                }
            }
        }
        Ok(c)
    }

    pub fn font_size_px(&self) -> f32 {
        self.font_size_pt * 96.0 / 72.0
    }
}
