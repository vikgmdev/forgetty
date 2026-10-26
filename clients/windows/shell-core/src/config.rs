//! `%APPDATA%\forgetty\config.toml`. Every key is optional. Pure: no UI, no file watching.
//!
//! The keys GTK also reads (`font_family`, `font_size`, `theme = "Name"`, `shell`,
//! `bell_mode`, `notification_mode`, `profiles`, `default_profile`, `paste_warn_size`,
//! `paste_warn_newline`, `scrollback_lines`, ...) are parsed by `forgetty_config::Config`, so
//! both clients read one file the same way and share the 486 bundled themes and
//! `<config dir>/themes/*.toml`. The Windows keys below stay as they were.
//!
//! ```toml
//! shell = "pwsh.exe -NoLogo"     # a program and its arguments ("..." groups a path with spaces)
//! theme = "Dracula"              # a bundled or custom theme by name (or the [theme] table below)
//! font_family = "Cascadia Mono"  # same as [font] family
//! font_size = 11.0               # same as [font] size
//! default_profile = "PowerShell"
//! [[profiles]]
//! name = "PowerShell"
//! command = "pwsh.exe -NoLogo"
//! directory = "~\\src"
//!
//! scrollback_bytes = 10485760   # libghostty bytes per pane (default 10 MiB)
//! backdrop = "mica"             # mica | mica_alt | blur | none
//! sidebar = false               # start with the sidebar open
//! pane_padding_x = 6.0          # px between a pane border and its text (default 6)
//! pane_padding_y = 4.0          # default 4
//! bell_mode = "visual"          # visual | audio | both | none
//! notification_mode = "all"     # all | ringonly | none
//! paste_warn_size = 5120        # ask before pasting more bytes than this (0 = never)
//! paste_warn_newline = true     # ask before a multi-line paste into a program without bracketed paste
//! smart_copy = true             # copy strips box-drawing characters (false keeps them)
//! right_click = "menu"           # menu (default, as GTK) | copy_paste (Windows Terminal)
//! [font]
//! family = "Cascadia Mono"
//! size = 11.0            # points
//! [theme]                  # colors by hand; `theme = "Name"` above is the other way
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
//! Action names: see `keys::ACTIONS`.
//!
//! Keys that GTK reads and a client does not use (`cursor_style`, `on_launch`,
//! `session_trash_days`, `byte_log_*`, `relay*`, `bind_addrs`) are accepted and ignored. A key
//! nobody knows is a warning, with a suggestion when it is close to a known one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use forgetty_config::NotificationMode;

use crate::bell::BellMode;
use crate::resolve::{resolve_dir, resolve_program, Host};

/// libghostty-vt's default scrollback (`forgetty_vt::DEFAULT_MAX_SCROLLBACK_BYTES`; a test keeps
/// the two equal, because shell-core does not link the VT crate outside its tests).
pub const DEFAULT_SCROLLBACK_BYTES: usize = 10 * 1024 * 1024;

/// Zoom limits and step, in points, as GTK (`app.rs` `FONT_SIZE_MIN/MAX`).
pub const ZOOM_MIN_PT: f32 = 6.0;
pub const ZOOM_MAX_PT: f32 = 72.0;
pub const ZOOM_STEP_PT: f32 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    In,
    Out,
    Reset,
}

/// The size after a zoom step. `default_pt` is the configured size (what Reset returns to).
pub fn zoomed(current_pt: f32, how: Zoom, default_pt: f32) -> f32 {
    match how {
        Zoom::In => (current_pt + ZOOM_STEP_PT).min(ZOOM_MAX_PT),
        Zoom::Out => (current_pt - ZOOM_STEP_PT).max(ZOOM_MIN_PT),
        Zoom::Reset => default_pt,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    Mica,
    MicaAlt,
    Blur,
    None,
}

/// A named shell for a new tab (`[[profiles]]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// The program and its arguments, as the user wrote them.
    pub command: String,
    /// `command` as an argv. Never empty.
    pub argv: Vec<String>,
    /// Start directory as written. `~` is the home directory; see [`expand_dir`].
    pub directory: Option<String>,
    /// `directory` expanded and checked when the file was read (so a click costs no disk
    /// access). `None` with `dir_problem` set: it is missing or a network path.
    pub start_dir: Option<String>,
    /// Why `directory` is not used, when it is not.
    pub dir_problem: Option<String>,
    /// A GTK icon name. Kept so a file written for GTK loads; this client shows none.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The shell program and arguments, as written. `None` means FORGETTY_SHELL, then COMSPEC.
    pub shell: Option<String>,
    /// `shell` as an argv with the program's full path, found when the file was read.
    pub shell_argv: Option<Vec<String>>,
    /// Scrollback budget per pane in libghostty-vt bytes.
    pub scrollback_bytes: usize,
    pub backdrop: Backdrop,
    pub sidebar_open: bool,
    /// Space between a pane's border and its cell grid, in px.
    pub pane_padding_x: f32,
    pub pane_padding_y: f32,
    pub bell_mode: BellMode,
    pub notification_mode: NotificationMode,
    /// A paste over this many bytes asks first. 0 turns the question off.
    pub paste_warn_size: usize,
    /// A multi-line paste into a program without bracketed paste asks first.
    pub paste_warn_newline: bool,
    /// Copy strips box-drawing characters (the text of a bordered TUI, without its border).
    pub smart_copy: bool,
    /// A plain right-click copies a selection or pastes (Windows Terminal). Off by default: a
    /// plain right-click opens the context menu, as in GTK. Shift+right-click opens the menu either way.
    pub right_click_copy_paste: bool,
    /// Action name to chords, from `[keybindings]`. Overrides the defaults.
    pub keybindings: HashMap<String, Vec<String>>,
    /// Named shells for new tabs, in file order.
    pub profiles: Vec<Profile>,
    /// Name of the profile a plain new tab uses. `None`, or a name no profile has: the first
    /// profile, as GTK; with no profiles, the shell.
    pub default_profile: Option<String>,
    /// The name in `theme = "Name"`, when the theme came from a name.
    pub theme_name: Option<String>,
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
            shell_argv: None,
            scrollback_bytes: DEFAULT_SCROLLBACK_BYTES,
            backdrop: Backdrop::Mica,
            sidebar_open: false,
            pane_padding_x: 6.0,
            pane_padding_y: 4.0,
            bell_mode: Default::default(),
            notification_mode: NotificationMode::All,
            paste_warn_size: crate::paste::DEFAULT_WARN_SIZE,
            paste_warn_newline: true,
            smart_copy: true,
            right_click_copy_paste: false,
            keybindings: HashMap::new(),
            profiles: Vec::new(),
            default_profile: None,
            theme_name: None,
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

/// A parsed file: the config, plus what the user should be told about it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Unknown keys, values out of range, a theme that does not exist. Each is one short line.
    pub warnings: Vec<String>,
}

// ---- the Windows-only part of the file -------------------------------------------------------

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
    smart_copy: Option<bool>,
    right_click: Option<String>,
    keybindings: HashMap<String, KeyValue>,
    font: FontSection,
    /// A name (`theme = "Dracula"`) or the colors table.
    theme: Option<toml::Value>,
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

/// Top-level keys only this client reads.
const WINDOWS_KEYS: &[&str] = &[
    "scrollback",
    "scrollback_bytes",
    "backdrop",
    "sidebar",
    "pane_padding_x",
    "pane_padding_y",
    "smart_copy",
    "right_click",
    "keybindings",
    "font",
    "theme",
];

/// Keys `forgetty_config::Config` reads (all of them, so a typo of any is suggested).
const SHARED_KEYS: &[&str] = &[
    "font_family",
    "font_size",
    "shell",
    "scrollback_lines",
    "cursor_style",
    "bell_mode",
    "notification_mode",
    "on_launch",
    "profiles",
    "default_profile",
    "paste_warn_size",
    "paste_warn_newline",
    "session_trash_days",
    "byte_log_ring_kb",
    "byte_log_max_mb",
    "relay_urls",
    "relay",
    "bind_addrs",
];

const FONT_KEYS: &[&str] = &["family", "size"];
const THEME_KEYS: &[&str] = &["foreground", "background", "cursor", "selection", "accent", "ansi"];

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('#'), 16).ok().filter(|v| *v <= 0xffffff)
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

/// The known name closest to `key`, when it is close enough to be a typo.
pub fn suggest<'a>(key: &str, known: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let key = key.to_ascii_lowercase();
    known
        .into_iter()
        .map(|k| (edit_distance(&key, &k.to_ascii_lowercase()), k))
        .filter(|(d, k)| *d <= 2.min(k.len() / 3 + 1))
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Keys in a `[section]` table that the section does not know.
fn unknown_in(table: &toml::Value, known: &[&str], section: &str, out: &mut Vec<String>) {
    let Some(t) = table.as_table() else { return };
    for k in t.keys().filter(|k| !known.contains(&k.as_str())) {
        out.push(unknown_key_text(&format!("{section}.{k}"), k, known.iter().copied()));
    }
}

fn unknown_key_text<'a>(shown: &str, key: &str, known: impl IntoIterator<Item = &'a str>) -> String {
    match suggest(key, known) {
        Some(near) => format!("unknown key `{shown}`, did you mean `{near}`?"),
        None => format!("unknown key `{shown}` (ignored)"),
    }
}

/// `text` as a command line with the program's full path: the whole text when it is a full
/// path to a file (a path with spaces, as the old `shell = "C:\\Program Files\\...\\pwsh.exe"`
/// was), else split into words with [`crate::argv::split`]. The program is found with
/// [`crate::resolve::resolve_program`]: PATH only, no network path, no relative path.
pub fn command_argv(text: &str, host: &Host) -> Result<Vec<String>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("the command is empty".into());
    }
    if crate::resolve::is_absolute(t) && crate::osc::is_safe_local_path(t) && (host.is_file)(t) {
        return Ok(vec![t.to_string()]);
    }
    let mut v = crate::argv::split(t)?;
    let Some(first) = v.first() else { return Err("the command is empty".into()) };
    let hint = v.len() > 1 && crate::resolve::is_absolute(first);
    match resolve_program(first, host) {
        Ok(full) => v[0] = full,
        Err(e) if hint => return Err(format!("{e} (put a path with spaces in quotes)")),
        Err(e) => return Err(e),
    }
    Ok(v)
}

/// A profile's start directory: `~` or `~\x` or `~/x` becomes `home`. Any other text stays as
/// it is. `None` when the text is empty.
pub fn expand_dir(dir: &str, home: Option<&str>) -> Option<String> {
    let d = dir.trim();
    if d.is_empty() {
        return None;
    }
    if d == "~" {
        return home.map(str::to_string);
    }
    if let Some(rest) = d.strip_prefix("~\\").or_else(|| d.strip_prefix("~/")) {
        let home = home?;
        return Some(format!("{}\\{}", home.trim_end_matches(['\\', '/']), rest));
    }
    Some(d.to_string())
}

fn rgba(c: forgetty_core::Rgba) -> u32 {
    (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
}

impl Config {
    /// The profile a plain new tab uses: `default_profile` by name, else the first.
    pub fn default_profile(&self) -> Option<&Profile> {
        self.default_profile
            .as_deref()
            .and_then(|n| self.profiles.iter().find(|p| p.name == n))
            .or_else(|| self.profiles.first())
    }

    /// Profile `n`, 1-based, for Ctrl+Shift+1..9.
    pub fn profile_n(&self, n: usize) -> Option<&Profile> {
        n.checked_sub(1).and_then(|i| self.profiles.get(i)).filter(|_| n <= 9)
    }

    pub fn font_size_px(&self) -> f32 {
        self.font_size_pt * 96.0 / 72.0
    }

    /// Parse the file. A file that does not parse, or has a wrong value (`bell_mode = "loud"`),
    /// is an `Err` and changes nothing. Out-of-range numbers, unknown keys and an unknown theme
    /// name are warnings: the key keeps its default.
    pub fn parse(text: &str) -> Result<Loaded, String> {
        Self::parse_with(text, &Host::real(None))
    }

    /// `parse` against `host`: where programs and folders are looked up, and where the custom
    /// themes are. All the disk access of a read happens here, on the caller's (worker) thread.
    pub fn parse_with(text: &str, host: &Host) -> Result<Loaded, String> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let f: FileConfig =
            toml::Value::Table(table.clone()).try_into().map_err(|e: toml::de::Error| e.to_string())?;
        // The keys both clients read go through forgetty-config. Take out what only this client
        // reads (its types differ: `[theme]`, `keybindings` lists, `shell` in the old form).
        let mut shared_table = table.clone();
        for k in WINDOWS_KEYS {
            shared_table.remove(*k);
        }
        // The older Windows parser took `bell_mode = "Both"`; forgetty-config wants lower case.
        for k in ["bell_mode", "notification_mode", "cursor_style", "on_launch"] {
            if let Some(toml::Value::String(v)) = shared_table.get_mut(k) {
                *v = v.trim().to_ascii_lowercase();
            }
        }
        let shared: forgetty_config::Config =
            toml::Value::Table(shared_table).try_into().map_err(|e: toml::de::Error| e.to_string())?;

        let mut warnings: Vec<String> = Vec::new();
        let mut c = Self::default();

        // Unknown keys, anywhere a typo can hide.
        let known = || WINDOWS_KEYS.iter().chain(SHARED_KEYS.iter()).copied();
        for k in shared.extra.keys() {
            warnings.push(unknown_key_text(k, k, known()));
        }
        if let Some(v) = table.get("font") {
            unknown_in(v, FONT_KEYS, "font", &mut warnings);
        }
        if let Some(v) = f.theme.as_ref().filter(|v| v.is_table()) {
            unknown_in(v, THEME_KEYS, "theme", &mut warnings);
        }

        // shell
        if let Some(s) = f.shell.as_deref().or(shared.shell.as_deref()) {
            match command_argv(s, host) {
                Ok(argv) => {
                    c.shell = Some(s.to_string());
                    c.shell_argv = Some(argv);
                }
                Err(e) => warnings.push(format!("shell: {e} (using the default shell)")),
            }
        }

        // scrollback: scrollback_bytes, then scrollback_lines (GTK), then the W1 `scrollback`.
        let lines = |n: usize| n.saturating_mul(2048);
        if let Some(n) = f.scrollback.filter(|n| *n <= 1_000_000) {
            // W1 compatibility: ~2 KB of libghostty page memory per line.
            c.scrollback_bytes = lines(n);
        }
        if table.contains_key("scrollback_lines") {
            if shared.scrollback_lines <= 1_000_000 {
                c.scrollback_bytes = lines(shared.scrollback_lines);
            } else {
                warnings.push("scrollback_lines: over 1000000, ignored".into());
            }
        }
        if let Some(n) = f.scrollback_bytes {
            if (64 * 1024..=4usize << 30).contains(&n) {
                c.scrollback_bytes = n;
            } else {
                warnings.push("scrollback_bytes: outside 65536..4294967296, ignored".into());
            }
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
        for (name, val, dst) in [
            ("pane_padding_x", f.pane_padding_x, &mut c.pane_padding_x),
            ("pane_padding_y", f.pane_padding_y, &mut c.pane_padding_y),
        ] {
            if let Some(v) = val {
                if (0.0..=64.0).contains(&v) {
                    *dst = v;
                } else {
                    warnings.push(format!("{name}: outside 0..64, ignored"));
                }
            }
        }
        if table.contains_key("bell_mode") {
            c.bell_mode = match shared.bell_mode {
                forgetty_config::BellMode::Visual => BellMode::Visual,
                forgetty_config::BellMode::Audio => BellMode::Audio,
                forgetty_config::BellMode::Both => BellMode::Both,
                forgetty_config::BellMode::None => BellMode::None,
            };
        }
        if table.contains_key("notification_mode") {
            c.notification_mode = shared.notification_mode;
        }
        if table.contains_key("paste_warn_size") {
            c.paste_warn_size = shared.paste_warn_size;
        }
        if table.contains_key("paste_warn_newline") {
            c.paste_warn_newline = shared.paste_warn_newline;
        }
        if let Some(v) = f.smart_copy {
            c.smart_copy = v;
        }
        if let Some(m) = f.right_click.as_deref() {
            c.right_click_copy_paste = match m.trim().to_ascii_lowercase().as_str() {
                "copy_paste" => true,
                "menu" => false,
                _ => return Err(format!("right_click: unknown value '{m}' (copy_paste, menu)")),
            };
        }
        for (name, v) in f.keybindings {
            let keys = match v {
                KeyValue::One(s) => vec![s],
                KeyValue::Many(v) => v,
            };
            c.keybindings.insert(name, keys);
        }

        // Profiles. forgetty-config already dropped the ones with no name or no command.
        for p in &shared.profiles {
            match command_argv(&p.command, host) {
                Ok(argv) => {
                    let directory = p.directory.clone().filter(|d| !d.trim().is_empty());
                    let (start_dir, dir_problem) = match directory.as_deref().map(|d| resolve_dir(d, host)) {
                        Some(Ok(d)) => (d, None),
                        Some(Err(e)) => {
                            warnings.push(format!("profile `{}`: directory {e}, it starts in the home directory", p.name));
                            (None, Some(e))
                        }
                        None => (None, None),
                    };
                    c.profiles.push(Profile {
                        name: p.name.clone(),
                        command: p.command.clone(),
                        argv,
                        directory,
                        start_dir,
                        dir_problem,
                        icon: p.icon.clone(),
                    })
                }
                Err(e) => warnings.push(format!("profile `{}`: {e}, skipped", p.name)),
            }
        }
        if table.get("profiles").and_then(|v| v.as_array()).is_some_and(|a| a.len() > shared.profiles.len()) {
            warnings.push("a profile with no name or no command was skipped".into());
        }
        for (i, p) in c.profiles.iter().enumerate() {
            if c.profiles[..i].iter().any(|q| q.name == p.name) {
                warnings.push(format!("profile `{}` is listed twice; a name picks the first", p.name));
            }
        }
        if let Some(d) = shared.default_profile.as_deref() {
            if !c.profiles.iter().any(|p| p.name == d) {
                warnings.push(match suggest(d, c.profiles.iter().map(|p| p.name.as_str())) {
                    Some(near) => format!("default_profile `{d}` is not a profile, did you mean `{near}`?"),
                    None => format!("default_profile `{d}` is not a profile; the first profile is used"),
                });
            }
            c.default_profile = Some(d.to_string());
        }
        if c.profiles.len() > 9 {
            warnings.push("more than 9 profiles: Ctrl+Shift+1..9 open the first 9, the menu lists all".into());
        }

        // font
        if let Some(v) = f.font.family.or_else(|| table.contains_key("font_family").then(|| shared.font_family.clone())) {
            if !v.trim().is_empty() {
                c.font_family = v;
            }
        }
        let size = f.font.size.or_else(|| table.contains_key("font_size").then_some(shared.font_size));
        if let Some(v) = size {
            if (4.0..=96.0).contains(&v) {
                c.font_size_pt = v;
            } else {
                warnings.push("font size: outside 4..96, ignored".into());
            }
        }

        // theme: a name, then the colors table on top of the defaults
        let themes_dir = host.themes_dir.clone().unwrap_or_else(default_themes_dir);
        let mut named: Option<(String, forgetty_config::Theme)> = None;
        let mut section = ThemeSection::default();
        match f.theme {
            Some(toml::Value::String(name)) => match forgetty_config::load_theme_by_name_in(&name, &themes_dir) {
                Some(t) => named = Some((name, t)),
                None => warnings.push(theme_missing(&name, &themes_dir)),
            },
            Some(v @ toml::Value::Table(_)) => {
                section = v.try_into().map_err(|e: toml::de::Error| format!("theme: {e}"))?;
            }
            Some(_) => return Err("theme: must be a name or a table of colors".into()),
            None => {}
        }
        if let Some((name, t)) = named {
            c.foreground = rgba(t.foreground);
            c.background = rgba(t.background);
            c.cursor = rgba(t.cursor);
            c.selection = rgba(t.selection);
            for (i, col) in t.ansi_colors.iter().enumerate() {
                c.ansi[i] = rgba(*col);
            }
            c.theme_name = Some(name);
        }
        let mut set = |label: &str, dst: &mut u32, src: Option<String>| {
            if let Some(s) = src {
                match hex(&s) {
                    Some(v) => *dst = v,
                    None => warnings.push(format!("theme {label}: `{s}` is not #rrggbb, ignored")),
                }
            }
        };
        set("foreground", &mut c.foreground, section.foreground);
        set("background", &mut c.background, section.background);
        set("cursor", &mut c.cursor, section.cursor);
        set("selection", &mut c.selection, section.selection);
        set("accent", &mut c.accent, section.accent);
        if let Some(list) = section.ansi {
            if list.len() != 16 {
                warnings.push(format!("theme ansi: {} entries, 16 expected", list.len()));
            }
            for (i, s) in list.iter().take(16).enumerate() {
                match hex(s) {
                    Some(v) => c.ansi[i] = v,
                    None => warnings.push(format!("theme ansi[{i}]: `{s}` is not #rrggbb, ignored")),
                }
            }
        }
        Ok(Loaded { config: c, warnings })
    }

    /// `parse`, without the warnings.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        Self::parse(text).map(|l| l.config)
    }
}

/// The themes folder next to the default config file (`%APPDATA%\\forgetty\\themes`).
pub fn default_themes_dir() -> PathBuf {
    forgetty_core::platform::config_dir().join("themes")
}

fn theme_missing(name: &str, dir: &Path) -> String {
    let catalog = forgetty_config::load_theme_catalog_in(dir);
    let near = suggest(name, catalog.iter().map(|e| e.name.as_str()));
    match near {
        Some(n) => format!("theme `{name}` not found, did you mean `{n}`? (the colors stay default)"),
        None => format!("theme `{name}` not found (the colors stay default)"),
    }
}

fn colors_of(t: &forgetty_config::Theme) -> ThemeColors {
    let mut ansi = [0u32; 16];
    for (i, c) in t.ansi_colors.iter().enumerate() {
        ansi[i] = rgba(*c);
    }
    ThemeColors {
        foreground: rgba(t.foreground),
        background: rgba(t.background),
        cursor: rgba(t.cursor),
        selection: rgba(t.selection),
        ansi,
    }
}

/// Names of the bundled and custom themes, sorted, for a picker. Reads the disk: call it from a
/// worker, or use a [`ThemeIndex`].
pub fn theme_names() -> Vec<String> {
    ThemeIndex::build(&default_themes_dir()).names
}

/// A bundled or custom theme by name, as the colors this client uses. Reads the disk.
pub fn theme_colors(name: &str) -> Option<ThemeColors> {
    forgetty_config::load_theme_by_name_in(name, &default_themes_dir()).map(|t| colors_of(&t))
}

/// Every theme, in memory: the names for the picker and the colors for a pick. Built on a
/// worker thread (one pass over the bundled themes and the custom folder) and rebuilt when the
/// folder changes, so the UI thread never reads a theme file.
#[derive(Debug, Clone, Default)]
pub struct ThemeIndex {
    /// Sorted by lower-case name, no duplicates.
    pub names: Vec<String>,
    colors: HashMap<String, ThemeColors>,
}

impl ThemeIndex {
    pub fn build(themes_dir: &Path) -> Self {
        let mut colors = HashMap::new();
        let mut names = Vec::new();
        for (name, t) in forgetty_config::load_all_themes_in(themes_dir) {
            if colors.insert(name.clone(), colors_of(&t)).is_none() {
                names.push(name);
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        ThemeIndex { names, colors }
    }

    pub fn colors(&self, name: &str) -> Option<ThemeColors> {
        self.colors.get(name).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeColors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: [u32; 16],
}

impl Config {
    pub fn with_colors(mut self, t: ThemeColors) -> Self {
        self.foreground = t.foreground;
        self.background = t.background;
        self.cursor = t.cursor;
        self.selection = t.selection;
        self.ansi = t.ansi;
        self
    }
}

// ---- what a reload changes --------------------------------------------------------------------

/// Which parts of the config differ, so a reload touches only what changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Font family or size: every pane re-measures (and drops its zoom).
    pub font: bool,
    /// Foreground, background, cursor, selection, accent or the 16 colors.
    pub colors: bool,
    pub bell: bool,
    pub notifications: bool,
    pub keybindings: bool,
    /// Profiles or the default profile.
    pub profiles: bool,
    /// paste_warn_size, paste_warn_newline.
    pub paste: bool,
    /// The shell for a plain new tab.
    pub shell: bool,
    pub backdrop: bool,
    /// Pane padding.
    pub padding: bool,
    /// smart_copy, right_click.
    pub mouse_and_copy: bool,
    /// Keys that only apply to a new pane or at the next start.
    pub later: Vec<&'static str>,
}

impl Changes {
    pub fn any(&self) -> bool {
        self.font
            || self.colors
            || self.bell
            || self.notifications
            || self.keybindings
            || self.profiles
            || self.paste
            || self.shell
            || self.backdrop
            || self.padding
            || self.mouse_and_copy
            || !self.later.is_empty()
    }

    /// "font, colors, profiles", for the notice after a reload.
    pub fn summary(&self) -> String {
        let mut v: Vec<&str> = Vec::new();
        for (on, name) in [
            (self.font, "font"),
            (self.colors, "colors"),
            (self.bell, "bell"),
            (self.notifications, "notifications"),
            (self.keybindings, "keys"),
            (self.profiles, "profiles"),
            (self.paste, "paste"),
            (self.shell, "shell"),
            (self.backdrop, "backdrop"),
            (self.padding, "padding"),
            (self.mouse_and_copy, "copy and mouse"),
        ] {
            if on {
                v.push(name);
            }
        }
        v.join(", ")
    }
}

/// What differs between two configs.
pub fn diff(old: &Config, new: &Config) -> Changes {
    let mut ch = Changes {
        font: old.font_family != new.font_family || old.font_size_pt != new.font_size_pt,
        colors: old.foreground != new.foreground
            || old.background != new.background
            || old.cursor != new.cursor
            || old.selection != new.selection
            || old.accent != new.accent
            || old.ansi != new.ansi,
        bell: old.bell_mode != new.bell_mode,
        notifications: old.notification_mode != new.notification_mode,
        keybindings: old.keybindings != new.keybindings,
        profiles: old.profiles != new.profiles || old.default_profile != new.default_profile,
        paste: old.paste_warn_size != new.paste_warn_size || old.paste_warn_newline != new.paste_warn_newline,
        shell: old.shell != new.shell || old.shell_argv != new.shell_argv,
        backdrop: old.backdrop != new.backdrop,
        padding: old.pane_padding_x != new.pane_padding_x || old.pane_padding_y != new.pane_padding_y,
        mouse_and_copy: old.smart_copy != new.smart_copy || old.right_click_copy_paste != new.right_click_copy_paste,
        later: Vec::new(),
    };
    if old.scrollback_bytes != new.scrollback_bytes {
        ch.later.push("scrollback (new panes)");
    }
    if old.sidebar_open != new.sidebar_open {
        ch.later.push("sidebar (next start)");
    }
    ch
}

/// The first line of an error, which can be several lines long (TOML errors are).
fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim()
}

/// The notice text after a successful reload. `asked` is a reload the user asked for.
pub fn reload_notice(ch: &Changes, warnings: &[String], asked: bool) -> Option<String> {
    let mut text = if ch.any() {
        let mut t = format!("Configuration reloaded: {}", ch.summary());
        if !ch.later.is_empty() {
            let later = ch.later.join(", ");
            t = if ch.summary().is_empty() { format!("Configuration reloaded. Later: {later}") } else { format!("{t}. Later: {later}") };
        }
        t
    } else if asked {
        "Configuration reloaded, nothing changed".to_string()
    } else if warnings.is_empty() {
        return None;
    } else {
        "Configuration reloaded".to_string()
    };
    if let Some(first) = warnings.first() {
        let more = if warnings.len() > 1 { format!(" (+{} more)", warnings.len() - 1) } else { String::new() };
        text.push_str(&format!(". Warning: {first}{more}"));
    }
    Some(text)
}

/// Why "Open configuration file" must not touch `path`, or `None` when it may. `FORGETTY_CONFIG`
/// can name any path, and the action opens it with the shell's `open` verb and may write a
/// template to it, so only a `.toml` file on a local path qualifies.
pub fn open_refusal(path: &Path) -> Option<String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = crate::notify::sanitize(&name, 40);
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml")) {
        return Some(format!("Not opened: the configuration file {name} is not a .toml file"));
    }
    if !crate::osc::is_safe_local_path(&path.to_string_lossy()) {
        return Some(format!("Not opened: {name} is on a network path"));
    }
    None
}

/// The notice for a config file that is gone: nothing changes.
pub fn missing_notice() -> String {
    "config.toml is gone, the last good settings stay".to_string()
}

/// The notice for a file that was not applied.
pub fn failed_notice(err: &str) -> String {
    format!("Configuration not applied, the last good settings stay. {}", first_line(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with a few programs on PATH and a few folders.
    fn host() -> Host {
        Host::fake(
            &[
                "C:\\tools\\pwsh.exe",
                "C:\\tools\\wsl.exe",
                "C:\\tools\\a.exe",
                "C:\\Program Files\\Git\\bin\\bash.exe",
                "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
            ],
            &["C:\\Users\\v\\src", "D:\\work"],
        )
    }

    fn p(t: &str) -> Loaded {
        Config::parse_with(t, &host()).unwrap()
    }

    #[test]
    fn defaults_match_the_vt_default_scrollback() {
        assert_eq!(DEFAULT_SCROLLBACK_BYTES, forgetty_vt::DEFAULT_MAX_SCROLLBACK_BYTES);
        let l = p("");
        assert!(l.warnings.is_empty());
        assert_eq!(l.config, Config::default());
    }

    #[test]
    fn the_windows_file_of_before_still_loads_without_a_warning() {
        let l = p(r##"
shell = "pwsh.exe"
scrollback_bytes = 1048576
backdrop = "none"
sidebar = true
pane_padding_x = 2.0
bell_mode = "audio"
paste_warn_size = 100
smart_copy = false
right_click = "copy_paste"
[font]
family = "Consolas"
size = 13.0
[theme]
foreground = "#112233"
ansi = ["#000001"]
[keybindings]
new_tab = "ctrl+t"
"##);
        // The one-entry ansi list is the only thing to say.
        assert_eq!(l.warnings.len(), 1, "{:?}", l.warnings);
        let c = l.config;
        assert_eq!(c.shell.as_deref(), Some("pwsh.exe"));
        assert_eq!(c.font_family, "Consolas");
        assert_eq!(c.foreground, 0x112233);
        assert_eq!(c.ansi[0], 0x000001);
        assert_eq!(c.bell_mode, BellMode::Audio);
        assert_eq!(c.paste_warn_size, 100);
        assert!(c.right_click_copy_paste && !c.smart_copy && c.sidebar_open);
        assert_eq!(c.keybindings["new_tab"], vec!["ctrl+t".to_string()]);
    }

    #[test]
    fn gtk_keys_are_read_by_the_shared_crate() {
        let l = p(r#"
font_family = "JetBrains Mono"
font_size = 14
notification_mode = "ringonly"
paste_warn_newline = false
scrollback_lines = 5000
cursor_style = "bar"
on_launch = "new"
relay = "none"
session_trash_days = 3
"#);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let c = l.config;
        assert_eq!(c.font_family, "JetBrains Mono");
        assert_eq!(c.font_size_pt, 14.0);
        assert_eq!(c.notification_mode, NotificationMode::RingOnly);
        assert!(!c.paste_warn_newline);
        assert_eq!(c.scrollback_bytes, 5000 * 2048);
        // [font] wins over font_family when both are there.
        let c = p("font_family = \"A\"\n[font]\nfamily = \"B\"\n").config;
        assert_eq!(c.font_family, "B");
    }

    #[test]
    fn enum_values_are_not_case_sensitive_as_before() {
        let c = p("bell_mode = \"Both\"\nnotification_mode = \"RingOnly\"\n").config;
        assert_eq!(c.bell_mode, BellMode::Both);
        assert_eq!(c.notification_mode, NotificationMode::RingOnly);
    }

    #[test]
    fn a_wrong_value_is_an_error_and_a_bad_number_is_a_warning() {
        assert!(Config::parse("notification_mode = \"loud\"").is_err());
        assert!(Config::parse("bell_mode = \"loud\"").is_err());
        assert!(Config::parse("font = 3").is_err());
        assert!(Config::parse("backdrop = \"glass\"").is_err());
        assert!(Config::parse("this is not toml").is_err());
        assert!(Config::parse("theme = 3").is_err());
        let l = p("font_size = 500\npane_padding_x = 100\nscrollback_bytes = 5");
        assert_eq!(l.warnings.len(), 3, "{:?}", l.warnings);
        assert_eq!(l.config.font_size_pt, Config::default().font_size_pt);
    }

    #[test]
    fn unknown_keys_warn_and_suggest() {
        let l = p("bell_mdoe = \"none\"\nfont_sise = 12\nsomething_new = 1\n[font]\nfamly = \"x\"\n[theme]\nforegroud = \"#fff\"\n");
        let w = l.warnings.join("\n");
        assert!(w.contains("`bell_mdoe`, did you mean `bell_mode`?"), "{w}");
        assert!(w.contains("`font_sise`, did you mean `font_size`?"), "{w}");
        assert!(w.contains("`something_new` (ignored)"), "{w}");
        assert!(w.contains("`font.famly`, did you mean `family`?"), "{w}");
        assert!(w.contains("`theme.foregroud`, did you mean `foreground`?"), "{w}");
        // Known keys, Windows and GTK, never warn.
        assert!(p("sidebar = true\nrelay_urls = []\nbind_addrs = []\n").warnings.is_empty());
    }

    #[test]
    fn a_theme_by_name_sets_the_colors_and_a_missing_one_says_so() {
        let l = p("theme = \"Dracula\"");
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.config.theme_name.as_deref(), Some("Dracula"));
        assert_ne!(l.config.background, Config::default().background);
        assert_ne!(l.config.ansi, CAMPBELL);
        let l = p("theme = \"Drakula\"");
        assert_eq!(l.config.ansi, CAMPBELL);
        assert!(l.warnings[0].contains("did you mean `Dracula`"), "{:?}", l.warnings);
        let l = p("theme = \"no such theme at all\"");
        assert!(l.warnings[0].contains("not found"), "{:?}", l.warnings);
        assert!(theme_names().len() >= 400);
        assert!(theme_names().iter().any(|n| n == "Nord"));
        assert_eq!(theme_colors("Nord").unwrap().background, Config::parse("theme = \"Nord\"").unwrap().config.background);
    }

    #[test]
    fn profiles_have_arguments_and_a_default() {
        let l = Config::parse_with(
            r#"
default_profile = "Git"
[[profiles]]
name = "PowerShell"
command = "pwsh.exe -NoLogo -NoProfile"
directory = "~\\src"
[[profiles]]
name = "Git"
command = "C:\\Program Files\\Git\\bin\\bash.exe"
[[profiles]]
name = "WSL"
command = "wsl.exe -d 'Ubuntu 24.04' --cd ~"
[[profiles]]
name = "Broken"
command = "pwsh.exe \"unclosed"
[[profiles]]
name = ""
command = "x"
"#,
            &host(),
        )
        .unwrap();
        let c = l.config;
        assert_eq!(c.profiles.len(), 3, "{:?}", l.warnings);
        assert_eq!(c.profiles[0].argv, ["C:\\tools\\pwsh.exe", "-NoLogo", "-NoProfile"]);
        assert_eq!(c.profiles[0].directory.as_deref(), Some("~\\src"));
        assert_eq!(c.profiles[0].start_dir.as_deref(), Some("C:\\Users\\v\\src"));
        assert!(c.profiles[0].dir_problem.is_none());
        // A command that names a file is one program, even with spaces.
        assert_eq!(c.profiles[1].argv, ["C:\\Program Files\\Git\\bin\\bash.exe"]);
        assert_eq!(c.profiles[2].argv, ["C:\\tools\\wsl.exe", "-d", "Ubuntu 24.04", "--cd", "~"]);
        assert_eq!(c.default_profile().unwrap().name, "Git");
        assert_eq!(c.profile_n(1).unwrap().name, "PowerShell");
        assert!(c.profile_n(0).is_none() && c.profile_n(4).is_none());
        let w = l.warnings.join("\n");
        assert!(w.contains("profile `Broken`"), "{w}");
        // An unknown default falls back to the first profile and says so.
        let l = p("default_profile = \"Nope\"\n[[profiles]]\nname = \"A\"\ncommand = \"a.exe\"\n");
        assert_eq!(l.config.default_profile().unwrap().name, "A");
        assert!(l.warnings[0].contains("`Nope` is not a profile"), "{:?}", l.warnings);
    }

    #[test]
    fn shell_with_arguments_and_the_old_path_form() {
        let c = p("shell = \"pwsh.exe -NoLogo\"").config;
        assert_eq!(c.shell.as_deref(), Some("pwsh.exe -NoLogo"));
        assert_eq!(c.shell_argv.unwrap(), ["C:\\tools\\pwsh.exe", "-NoLogo"]);
        let c = p("shell = 'C:\\Program Files\\PowerShell\\7\\pwsh.exe'").config;
        assert_eq!(c.shell_argv.unwrap(), ["C:\\Program Files\\PowerShell\\7\\pwsh.exe"]);
        let l = p("shell = \"\"");
        assert!(l.config.shell.is_none() && l.warnings[0].starts_with("shell:"));
    }

    #[test]
    fn a_program_that_is_not_found_or_is_a_network_path_is_refused_with_a_warning() {
        let l = p(r#"
shell = "ghost.exe -x"
[[profiles]]
name = "Ghost"
command = "ghost.exe"
[[profiles]]
name = "Share"
command = '\\\\srv\\share\\tool.exe'
[[profiles]]
name = "Rel"
command = ".\\tool.exe"
[[profiles]]
name = "Spaces"
command = "C:\\Program Files\\Git\\bin\\bash.exe -l"
[[profiles]]
name = "Ok"
command = "pwsh.exe"
directory = '\\\\srv\\share'
[[profiles]]
name = "Gone"
command = "pwsh.exe"
directory = "D:\\nope"
"#);
        assert!(l.config.shell.is_none() && l.config.shell_argv.is_none());
        let names: Vec<&str> = l.config.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Ok", "Gone"], "{:?}", l.warnings);
        let w = l.warnings.join("\n");
        assert!(w.contains("shell: `ghost.exe` is not on PATH"), "{w}");
        assert!(w.contains("profile `Ghost`") && w.contains("is not on PATH"), "{w}");
        assert!(w.contains("profile `Share`") && w.contains("network"), "{w}");
        assert!(w.contains("profile `Rel`") && w.contains("not a full path"), "{w}");
        assert!(w.contains("profile `Spaces`") && w.contains("quotes"), "{w}");
        // A bad folder keeps the profile and says why, once at read time and again on use.
        let ok = &l.config.profiles[0];
        assert!(ok.start_dir.is_none() && ok.dir_problem.as_deref().unwrap().contains("network"));
        let gone = &l.config.profiles[1];
        assert!(gone.start_dir.is_none() && gone.dir_problem.as_deref().unwrap().contains("does not exist"));
        assert!(w.contains("profile `Ok`: directory") && w.contains("profile `Gone`: directory"), "{w}");
    }

    #[test]
    fn the_custom_themes_come_from_the_folder_the_host_names() {
        let dir = std::env::temp_dir().join(format!("forgetty-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mine.toml"), "name = \"My Theme\"\n[colors]\nforeground = \"#102030\"\nbackground = \"#405060\"\n").unwrap();
        let mut h = host();
        h.themes_dir = Some(dir.clone());
        let l = Config::parse_with("theme = \"My Theme\"", &h).unwrap();
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!((l.config.foreground, l.config.background), (0x102030, 0x405060));
        let idx = ThemeIndex::build(&dir);
        assert!(idx.names.iter().any(|n| n == "My Theme") && idx.names.len() >= 400);
        assert_eq!(idx.colors("My Theme").unwrap().background, 0x405060);
        assert_eq!(idx.colors("Nord").unwrap().background, theme_colors("Nord").unwrap().background);
        assert!(idx.colors("No Such").is_none());
        // Without the folder the theme is unknown.
        h.themes_dir = Some(dir.join("absent"));
        assert!(Config::parse_with("theme = \"My Theme\"", &h).unwrap().warnings[0].contains("not found"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_configuration_file_only_takes_a_local_toml_path() {
        assert!(open_refusal(Path::new("C:\\Users\\v\\AppData\\forgetty\\config.toml")).is_none());
        assert!(open_refusal(Path::new("/tmp/x/CONFIG.TOML")).is_none());
        for bad in ["C:\\Windows\\System32\\calc.exe", "/tmp/x/config", "/tmp/x/config.toml.exe", "/tmp/x/.toml", "C:\\x\\evil.bat"] {
            let r = open_refusal(Path::new(bad));
            assert!(r.as_deref().is_some_and(|t| t.contains("not a .toml")), "{bad}: {r:?}");
        }
        assert!(open_refusal(Path::new("\\\\srv\\share\\config.toml")).unwrap().contains("network"));
    }

    #[test]
    fn directories_expand_the_home_marker() {
        assert_eq!(expand_dir("~", Some("C:\\Users\\v")).as_deref(), Some("C:\\Users\\v"));
        assert_eq!(expand_dir("~\\src", Some("C:\\Users\\v\\")).as_deref(), Some("C:\\Users\\v\\src"));
        assert_eq!(expand_dir("~/src", Some("C:\\Users\\v")).as_deref(), Some("C:\\Users\\v\\src"));
        assert_eq!(expand_dir("D:\\work", None).as_deref(), Some("D:\\work"));
        assert_eq!(expand_dir("~\\x", None), None);
        assert_eq!(expand_dir("  ", Some("h")), None);
        // A name that only starts with ~ is a name.
        assert_eq!(expand_dir("~x", Some("h")).as_deref(), Some("~x"));
    }

    #[test]
    fn zoom_steps_and_clamps_like_gtk() {
        assert_eq!(zoomed(11.0, Zoom::In, 11.0), 12.0);
        assert_eq!(zoomed(11.0, Zoom::Out, 11.0), 10.0);
        assert_eq!(zoomed(72.0, Zoom::In, 11.0), 72.0);
        assert_eq!(zoomed(6.0, Zoom::Out, 11.0), 6.0);
        assert_eq!(zoomed(30.0, Zoom::Reset, 11.0), 11.0);
        assert_eq!(zoomed(71.5, Zoom::In, 11.0), 72.0);
    }

    #[test]
    fn a_diff_names_only_what_changed() {
        let a = Config::default();
        assert!(!diff(&a, &a.clone()).any());
        let mut b = a.clone();
        b.font_size_pt = 14.0;
        b.bell_mode = BellMode::None;
        b.paste_warn_newline = false;
        b.sidebar_open = true;
        b.scrollback_bytes += 1;
        let d = diff(&a, &b);
        assert!(d.font && d.bell && d.paste && !d.colors && !d.profiles && !d.keybindings);
        assert_eq!(d.later.len(), 2);
        assert_eq!(d.summary(), "font, bell, paste");
        let mut c = a.clone();
        c.ansi[3] = 1;
        c.profiles.push(Profile { name: "x".into(), command: "x".into(), argv: vec!["x".into()], directory: None, start_dir: None, dir_problem: None, icon: None });
        let d = diff(&a, &c);
        assert!(d.colors && d.profiles && !d.font);
        let mut e = a.clone();
        e.keybindings.insert("new_tab".into(), vec![]);
        e.notification_mode = NotificationMode::None;
        let d = diff(&a, &e);
        assert!(d.keybindings && d.notifications);
    }

    #[test]
    fn a_syntax_error_notice_names_the_problem_in_its_first_line() {
        let e = Config::parse("font_size = [\n").unwrap_err();
        assert!(failed_notice(&e).contains("TOML parse error"), "{e}");
        let e = Config::parse("bell_mode = \"loud\"").unwrap_err();
        assert!(failed_notice(&e).contains("loud"), "{e}");
    }

    #[test]
    fn notices() {
        let none = Changes::default();
        assert_eq!(reload_notice(&none, &[], false), None);
        assert_eq!(reload_notice(&none, &[], true).unwrap(), "Configuration reloaded, nothing changed");
        let ch = Changes { font: true, colors: true, ..Default::default() };
        assert_eq!(reload_notice(&ch, &[], false).unwrap(), "Configuration reloaded: font, colors");
        let w = vec!["unknown key `a`".to_string(), "b".to_string()];
        let t = reload_notice(&none, &w, false).unwrap();
        assert!(t.contains("Warning: unknown key `a` (+1 more)"), "{t}");
        let later = Changes { later: vec!["sidebar (next start)"], ..Default::default() };
        assert!(reload_notice(&later, &[], false).unwrap().contains("Later: sidebar"));
        assert!(failed_notice("TOML parse error at line 3\n  |\n3 | x").ends_with("TOML parse error at line 3"));
    }
}
