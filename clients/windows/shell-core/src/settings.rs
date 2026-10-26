//! What the Settings window shows and how a change becomes an edit of `config.toml`. Pure: the
//! window (`src/settings.rs`) draws these and calls them.
//!
//! Every row reads its value from the config that was loaded from the file, so a value the user
//! typed into the TOML editor shows up here, and a change goes back as one edit
//! ([`crate::settings_file`]) that leaves the rest of the file alone.

use crate::bell::BellMode;
use crate::config::{Backdrop, Config};
use crate::settings_file::{Edit, V};
use forgetty_config::NotificationMode;

/// The pages in the left list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    General,
    Appearance,
    Terminal,
    Keybindings,
    /// The TOML editor.
    Advanced,
    About,
}

impl Page {
    pub const ALL: [Page; 6] = [Page::General, Page::Appearance, Page::Terminal, Page::Keybindings, Page::Advanced, Page::About];

    pub fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Appearance => "Appearance",
            Page::Terminal => "Terminal",
            Page::Keybindings => "Keybindings",
            Page::Advanced => "TOML editor",
            Page::About => "About",
        }
    }
}

/// How a row is edited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Toggle,
    /// (value in the file, label)
    Choice(&'static [(&'static str, &'static str)]),
    /// An integer. The shown number is the stored one divided by `scale`.
    Int { min: i64, max: i64, step: i64, scale: i64 },
    Float { min: f64, max: f64, step: f64 },
    Text { placeholder: &'static str },
}

pub struct Def {
    /// Name of the key; also the id the window uses for the row.
    pub id: &'static str,
    /// Where it is in the file: `key` or `table.key`.
    pub path: &'static str,
    pub page: Page,
    /// Heading inside the page.
    pub section: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub get: fn(&Config) -> V,
    pub default: fn() -> V,
    /// What a change needs: nothing (live), or a new pane / the next start.
    pub applies: &'static str,
}

const MIB: i64 = 1024 * 1024;

fn s(x: &str) -> V {
    V::Str(x.to_string())
}

fn bell_name(b: BellMode) -> &'static str {
    match b {
        BellMode::Visual => "visual",
        BellMode::Audio => "audio",
        BellMode::Both => "both",
        BellMode::None => "none",
    }
}

fn notif_name(n: NotificationMode) -> &'static str {
    match n {
        NotificationMode::All => "all",
        NotificationMode::RingOnly => "ringonly",
        NotificationMode::None => "none",
    }
}

fn backdrop_name(b: Backdrop) -> &'static str {
    match b {
        Backdrop::Mica => "mica",
        Backdrop::MicaAlt => "mica_alt",
        Backdrop::Blur => "blur",
        Backdrop::None => "none",
    }
}

pub const DEFS: &[Def] = &[
    Def {
        id: "shell",
        path: "shell",
        page: Page::General,
        section: "Shell",
        label: "Shell",
        help: "Program and arguments for a new tab, for example pwsh.exe -NoLogo. Blank: the system shell.",
        kind: Kind::Text { placeholder: "(the system shell)" },
        get: |c| s(c.shell.as_deref().unwrap_or("")),
        default: || V::Str(String::new()),
        applies: "",
    },
    Def {
        id: "keymap",
        path: "keymap",
        page: Page::General,
        section: "Shortcuts",
        label: "Shortcut set",
        help: "GTK: the chords of the GTK app. Windows: this client's own set. Entries in the Keybindings page win in both.",
        kind: Kind::Choice(&[("gtk", "GTK shortcuts"), ("windows", "Windows shortcuts")]),
        get: |c| s(c.keymap.name()),
        default: || V::Str("gtk".into()),
        applies: "",
    },
    Def {
        id: "backdrop",
        path: "backdrop",
        page: Page::General,
        section: "Window",
        label: "Window backdrop",
        help: "Mica needs Windows 11 22H2 or later. Older builds use a plain background.",
        kind: Kind::Choice(&[("mica", "Mica"), ("mica_alt", "Mica Alt"), ("blur", "Blur"), ("none", "None")]),
        get: |c| s(backdrop_name(c.backdrop)),
        default: || V::Str("mica".into()),
        applies: "",
    },
    Def {
        id: "sidebar",
        path: "sidebar",
        page: Page::General,
        section: "Window",
        label: "Open the sidebar at start",
        help: "Machines, workspaces and tabs on the left.",
        kind: Kind::Toggle,
        get: |c| V::Bool(c.sidebar_open),
        default: || V::Bool(false),
        applies: "next start",
    },
    Def {
        id: "scrollback_bytes",
        path: "scrollback_bytes",
        page: Page::Terminal,
        section: "Scrollback",
        label: "Scrollback per pane (MiB)",
        help: "Memory the terminal keeps for the lines that scrolled away.",
        kind: Kind::Int { min: 1, max: 2048, step: 1, scale: MIB },
        get: |c| V::Int((c.scrollback_bytes as i64) / MIB),
        default: || V::Int(10),
        applies: "new panes",
    },
    Def {
        id: "bell_mode",
        path: "bell_mode",
        page: Page::Terminal,
        section: "Alerts",
        label: "Bell",
        help: "What happens when a program rings the bell.",
        kind: Kind::Choice(&[("visual", "Visual"), ("audio", "Audio"), ("both", "Both"), ("none", "None")]),
        get: |c| s(bell_name(c.bell_mode)),
        default: || V::Str("visual".into()),
        applies: "",
    },
    Def {
        id: "notification_mode",
        path: "notification_mode",
        page: Page::Terminal,
        section: "Alerts",
        label: "Notifications",
        help: "When a program may raise a Windows notification.",
        kind: Kind::Choice(&[("all", "All"), ("ringonly", "Ring and badge only"), ("none", "None")]),
        get: |c| s(notif_name(c.notification_mode)),
        default: || V::Str("all".into()),
        applies: "",
    },
    Def {
        id: "smart_copy",
        path: "smart_copy",
        page: Page::Terminal,
        section: "Copy and mouse",
        label: "Smart copy",
        help: "Copy leaves out the box-drawing characters of a bordered program.",
        kind: Kind::Toggle,
        get: |c| V::Bool(c.smart_copy),
        default: || V::Bool(true),
        applies: "",
    },
    Def {
        id: "right_click",
        path: "right_click",
        page: Page::Terminal,
        section: "Copy and mouse",
        label: "Right click",
        help: "Menu: a context menu, as GTK. Copy and paste: copies a selection or pastes, as Windows Terminal.",
        kind: Kind::Choice(&[("menu", "Open the menu"), ("copy_paste", "Copy or paste")]),
        get: |c| s(if c.right_click_copy_paste { "copy_paste" } else { "menu" }),
        default: || V::Str("menu".into()),
        applies: "",
    },
    Def {
        id: "pane_padding_x",
        path: "pane_padding_x",
        page: Page::Terminal,
        section: "Layout",
        label: "Pane padding, sides (px)",
        help: "Space between the border of a pane and its text.",
        kind: Kind::Float { min: 0.0, max: 64.0, step: 1.0 },
        get: |c| V::Float(f64::from(c.pane_padding_x)),
        default: || V::Float(6.0),
        applies: "",
    },
    Def {
        id: "pane_padding_y",
        path: "pane_padding_y",
        page: Page::Terminal,
        section: "Layout",
        label: "Pane padding, top and bottom (px)",
        help: "Space above and below the text of a pane.",
        kind: Kind::Float { min: 0.0, max: 64.0, step: 1.0 },
        get: |c| V::Float(f64::from(c.pane_padding_y)),
        default: || V::Float(4.0),
        applies: "",
    },
    Def {
        id: "paste_warn_size",
        path: "paste_warn_size",
        page: Page::Terminal,
        section: "Paste",
        label: "Ask before a large paste (bytes)",
        help: "A paste over this many bytes asks first. 0 never asks.",
        kind: Kind::Int { min: 0, max: 65536, step: 512, scale: 1 },
        get: |c| V::Int(c.paste_warn_size as i64),
        default: || V::Int(5120),
        applies: "",
    },
    Def {
        id: "paste_warn_newline",
        path: "paste_warn_newline",
        page: Page::Terminal,
        section: "Paste",
        label: "Ask before a multi-line paste",
        help: "For a program that does not use bracketed paste, a line break would run the command.",
        kind: Kind::Toggle,
        get: |c| V::Bool(c.paste_warn_newline),
        default: || V::Bool(true),
        applies: "",
    },
];

pub fn def(id: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.id == id)
}

pub fn defs_of(page: Page) -> impl Iterator<Item = &'static Def> {
    DEFS.iter().filter(move |d| d.page == page)
}

fn trim_float(f: f64) -> String {
    if (f - f.round()).abs() < 1e-9 {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.1}")
    }
}

impl Def {
    pub fn read(&self, cfg: &Config) -> V {
        (self.get)(cfg)
    }

    pub fn is_default(&self, v: &V) -> bool {
        *v == (self.default)()
    }

    /// The text of a value in the row ("Both", "10", "Off"...).
    pub fn show(&self, v: &V) -> String {
        match (&self.kind, v) {
            (Kind::Choice(opts), V::Str(x)) => opts.iter().find(|o| o.0 == x).map(|o| o.1.to_string()).unwrap_or_else(|| x.clone()),
            (Kind::Toggle, V::Bool(b)) => if *b { "On" } else { "Off" }.to_string(),
            (_, V::Int(i)) => i.to_string(),
            (_, V::Float(f)) => trim_float(*f),
            (_, V::Str(x)) => x.clone(),
            (_, V::Bool(b)) => b.to_string(),
            (_, V::List(l)) => l.join(", "),
        }
    }

    /// The value the user typed, checked against the row's range. The error says what is wrong
    /// in a few words, for the row.
    pub fn parse_text(&self, text: &str) -> Result<V, String> {
        let t = text.trim();
        match self.kind {
            Kind::Text { .. } => Ok(V::Str(t.to_string())),
            Kind::Int { min, max, .. } => {
                let n: i64 = t.parse().map_err(|_| "Type a whole number".to_string())?;
                if n < min || n > max {
                    return Err(format!("Between {min} and {max}"));
                }
                Ok(V::Int(n))
            }
            Kind::Float { min, max, .. } => {
                let n: f64 = t.replace(',', ".").parse().map_err(|_| "Type a number".to_string())?;
                if !n.is_finite() || n < min || n > max {
                    return Err(format!("Between {} and {}", trim_float(min), trim_float(max)));
                }
                Ok(V::Float(n))
            }
            _ => Err("Not a typed value".into()),
        }
    }

    /// The next or previous option, or the value one step up or down (clamped).
    pub fn step(&self, v: &V, dir: i32) -> Option<V> {
        match (&self.kind, v) {
            (Kind::Choice(opts), V::Str(x)) => {
                let i = opts.iter().position(|o| o.0 == x).unwrap_or(0) as i32;
                let n = opts.len() as i32;
                Some(V::Str(opts[(i + dir).rem_euclid(n) as usize].0.to_string()))
            }
            (Kind::Toggle, V::Bool(b)) => Some(V::Bool(!b)),
            (Kind::Int { min, max, step, .. }, V::Int(i)) => Some(V::Int((i + i64::from(dir) * step).clamp(*min, *max))),
            (Kind::Float { min, max, step }, V::Float(f)) => Some(V::Float((f + f64::from(dir) * step).clamp(*min, *max))),
            _ => None,
        }
    }

    /// The edit that stores `v`. A blank text removes the key; a value is always written as the
    /// type the file expects.
    pub fn edits(&self, v: &V) -> Vec<Edit> {
        match (&self.kind, v) {
            (Kind::Text { .. }, V::Str(x)) if x.trim().is_empty() => vec![Edit::remove(self.path)],
            (Kind::Int { scale, .. }, V::Int(i)) => vec![Edit::set(self.path, V::Int(i * scale))],
            (_, v) => vec![Edit::set(self.path, v.clone())],
        }
    }
}

// ---- theme and font ------------------------------------------------------------------------

/// Why a named theme cannot be written: the file sets the colors by hand.
pub const THEME_TABLE: &str = "The file sets the colors by hand in a [theme] table. Remove that table in the TOML editor to use a named theme.";

/// Does the text have a `[theme]` table (colors by hand)?
fn has_table(text: &str, name: &str) -> bool {
    toml::from_str::<toml::Table>(text).ok().and_then(|t| t.get(name).map(toml::Value::is_table)).unwrap_or(false)
}

/// `theme = "name"`. Not when the file has a `[theme]` table: a key and a table cannot share a
/// name, and the colors the user typed are not ours to delete.
pub fn theme_edits(file_text: &str, name: &str) -> Result<Vec<Edit>, String> {
    if has_table(file_text, "theme") {
        return Err(THEME_TABLE.to_string());
    }
    Ok(vec![Edit::set("theme", V::Str(name.to_string()))])
}

/// Font family and/or size. A value the file keeps in a `[font]` table is changed there (the table
/// wins over the top-level key), else the top-level key is used.
pub fn font_edits(file_text: &str, family: Option<&str>, size: Option<f32>) -> Vec<Edit> {
    let t: toml::Table = toml::from_str(file_text).unwrap_or_default();
    let font = t.get("font").and_then(toml::Value::as_table);
    let in_table = |k: &str| font.is_some_and(|f| f.contains_key(k));
    let mut out = Vec::new();
    if let Some(f) = family {
        out.push(Edit::set(if in_table("family") { "font.family" } else { "font_family" }, V::Str(f.to_string())));
    }
    if let Some(sz) = size {
        // One decimal is plenty and keeps 11.5 from becoming 11.500000190734863.
        let v = (f64::from(sz) * 10.0).round() / 10.0;
        out.push(Edit::set(if in_table("size") { "font.size" } else { "font_size" }, V::Float(v)));
    }
    out
}

/// The smallest and largest font size the window offers (the config accepts 4 to 96).
pub const FONT_MIN: f32 = 6.0;
pub const FONT_MAX: f32 = 72.0;

/// Does a font family name look like a monospace one? A guess from the name, for the short list
/// (the window can show every family too).
pub fn looks_monospace(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["mono", "code", "consol", "courier", "terminal", "fixed", "hack", "iosevka", "menlo", "fira", "jetbrains", "cascadia", "lucida console", "ubuntu mono", "source code", "inconsolata", "dejavu sans mono", "nerd font", "monaco", "droid sans mono", "pt mono", "anonymous", "input", "victor", "commit", "sf mono"]
        .iter()
        .any(|k| n.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::apply;

    fn cfg(text: &str) -> Config {
        Config::parse(text).unwrap().config
    }

    #[test]
    fn every_row_reads_what_the_file_says_and_writes_it_back_unchanged() {
        let c = cfg("bell_mode = \"both\"\nnotification_mode = \"ringonly\"\nsmart_copy = false\nright_click = \"copy_paste\"\nbackdrop = \"mica_alt\"\nkeymap = \"windows\"\nsidebar = true\nscrollback_bytes = 20971520\npane_padding_x = 2.5\npaste_warn_size = 1024\npaste_warn_newline = false\n");
        let expect: &[(&str, V)] = &[
            ("bell_mode", V::Str("both".into())),
            ("notification_mode", V::Str("ringonly".into())),
            ("smart_copy", V::Bool(false)),
            ("right_click", V::Str("copy_paste".into())),
            ("backdrop", V::Str("mica_alt".into())),
            ("keymap", V::Str("windows".into())),
            ("sidebar", V::Bool(true)),
            ("scrollback_bytes", V::Int(20)),
            ("pane_padding_x", V::Float(2.5)),
            ("paste_warn_size", V::Int(1024)),
            ("paste_warn_newline", V::Bool(false)),
        ];
        for (id, v) in expect {
            let d = def(id).unwrap();
            assert_eq!(&d.read(&c), v, "{id}");
            // Writing the shown value gives a file that reads the same.
            let text = apply("", &d.edits(v)).unwrap();
            let again = cfg(&text);
            assert_eq!(&d.read(&again), v, "{id} after {text}");
        }
    }

    #[test]
    fn the_defaults_of_the_rows_are_the_defaults_of_the_config() {
        let c = Config::default();
        for d in DEFS {
            assert_eq!(d.read(&c), (d.default)(), "{}", d.id);
            assert!(d.is_default(&(d.default)()));
        }
        let mut ids: Vec<_> = DEFS.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), DEFS.len());
        // Every key is one the client knows: a row can never write a typo.
        for d in DEFS {
            let text = apply("", &d.edits(&(d.default)())).unwrap();
            let l = Config::parse(&text).unwrap();
            assert!(l.warnings.is_empty(), "{}: {:?}", d.id, l.warnings);
        }
    }

    #[test]
    fn typed_values_are_checked_and_steps_clamp() {
        let n = def("paste_warn_size").unwrap();
        assert_eq!(n.parse_text(" 2048 "), Ok(V::Int(2048)));
        assert!(n.parse_text("many").is_err());
        assert!(n.parse_text("70000").unwrap_err().contains("65536"));
        assert_eq!(n.step(&V::Int(0), -1), Some(V::Int(0)));
        assert_eq!(n.step(&V::Int(65536), 1), Some(V::Int(65536)));
        assert_eq!(n.step(&V::Int(5120), 1), Some(V::Int(5632)));
        let f = def("pane_padding_x").unwrap();
        assert_eq!(f.parse_text("3,5"), Ok(V::Float(3.5)));
        assert!(f.parse_text("-1").is_err());
        let c = def("bell_mode").unwrap();
        assert_eq!(c.step(&V::Str("none".into()), 1), Some(V::Str("visual".into())));
        assert_eq!(c.step(&V::Str("visual".into()), -1), Some(V::Str("none".into())));
        assert_eq!(c.show(&V::Str("both".into())), "Both");
        assert_eq!(def("smart_copy").unwrap().show(&V::Bool(true)), "On");
    }

    #[test]
    fn a_blank_shell_removes_the_key_and_scrollback_is_stored_in_bytes() {
        let sh = def("shell").unwrap();
        assert_eq!(sh.edits(&V::Str("  ".into())), vec![Edit::remove("shell")]);
        let out = apply("shell = \"x.exe\"\n", &sh.edits(&V::Str(String::new()))).unwrap();
        assert!(!out.contains("shell"));
        let sb = def("scrollback_bytes").unwrap();
        assert_eq!(sb.edits(&V::Int(32)), vec![Edit::set("scrollback_bytes", V::Int(32 * 1024 * 1024))]);
    }

    #[test]
    fn a_named_theme_is_not_written_over_colors_typed_by_hand() {
        assert_eq!(theme_edits("font_size = 12\n", "Dracula").unwrap(), vec![Edit::set("theme", V::Str("Dracula".into()))]);
        assert_eq!(theme_edits("theme = \"Old\"\n", "Dracula").unwrap().len(), 1);
        assert!(theme_edits("[theme]\nbackground = \"#000000\"\n", "Dracula").unwrap_err().contains("[theme]"));
    }

    #[test]
    fn a_font_is_changed_where_the_file_keeps_it() {
        assert_eq!(font_edits("", Some("Consolas"), Some(12.5)), vec![Edit::set("font_family", V::Str("Consolas".into())), Edit::set("font_size", V::Float(12.5))]);
        let e = font_edits("[font]\nfamily = \"A\"\nsize = 11.0\n", Some("B"), Some(13.0));
        assert_eq!(e, vec![Edit::set("font.family", V::Str("B".into())), Edit::set("font.size", V::Float(13.0))]);
        // A table with only the family: the size goes to the top-level key (the table has none).
        let e = font_edits("[font]\nfamily = \"A\"\n", None, Some(9.0));
        assert_eq!(e, vec![Edit::set("font_size", V::Float(9.0))]);
        let text = apply("[font]\nfamily = \"A\"\n", &font_edits("[font]\nfamily = \"A\"\n", Some("B"), Some(11.1))).unwrap();
        let c = cfg(&text);
        assert_eq!((c.font_family.as_str(), c.font_size_pt), ("B", 11.1));
    }

    #[test]
    fn the_short_font_list_keeps_the_usual_suspects() {
        for n in ["Cascadia Mono", "Consolas", "JetBrains Mono", "Fira Code", "Courier New", "Lucida Console"] {
            assert!(looks_monospace(n), "{n}");
        }
        for n in ["Segoe UI", "Arial", "Times New Roman"] {
            assert!(!looks_monospace(n), "{n}");
        }
    }
}
