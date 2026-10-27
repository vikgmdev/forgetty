//! The Settings window without any drawing: which page is open, what is selected, what a key
//! does, and what the window shows. The GPUI side (`src/settings.rs`) draws a [`View`] and hands
//! keys and clicks to [`Settings`]; a change comes back as an [`Effect`] that the shell turns
//! into a write of `config.toml` (see [`crate::settings_file`]). Linux tests cover this file.

use std::collections::HashMap;

use crate::config::{Config, ThemeColors};
use crate::keys::{Action, Chord};
use crate::settings::{self, Def, Kind, Page};
use crate::settings_file::{Edit, V};
use crate::shortcuts::{self, Line, Mode};
use crate::textarea::{error_line, KeyIn, Out, TextArea};

/// What the window needs to know about the app.
pub struct Ctx<'a> {
    /// The config as the file says it (not the colors of a theme picked for one session).
    pub cfg: &'a Config,
    pub themes: &'a [String],
    pub fonts: &'a [String],
    pub colors: &'a dyn Fn(&str) -> Option<ThemeColors>,
    pub config_path: &'a str,
}

/// What the shell does after a key.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    None,
    Close,
    /// Change these keys in the file.
    Edits(Vec<Edit>),
    Theme(String),
    Font { family: Option<String>, size: Option<f32> },
    /// The whole text of the TOML editor.
    SaveText(String),
    /// Read the file into the TOML editor.
    LoadEditor,
    OpenConfigFile,
    PasteIntoEditor,
    CopyText(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub ok: bool,
    pub text: String,
}

/// What a finished write was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrote {
    Setting,
    Editor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Theme,
    Font,
    Size,
}

struct Entry {
    id: &'static str,
    ta: TextArea,
    error: Option<String>,
}

struct Capture {
    action: Action,
    mode: Mode,
    msg: Option<String>,
    conflict: Option<(Chord, shortcuts::Conflict)>,
}

struct Look {
    zone: Zone,
    theme_filter: String,
    theme_hi: usize,
    font_filter: String,
    font_hi: usize,
    mono_only: bool,
    size: f32,
}

pub struct Settings {
    pub page: Page,
    sel: usize,
    top: usize,
    visible: usize,
    entry: Option<Entry>,
    capture: Option<Capture>,
    key_filter: String,
    look: Look,
    editor: TextArea,
    editor_loaded: bool,
    editor_msg: Option<String>,
    discard_armed: bool,
    pending: HashMap<&'static str, V>,
    status: Option<Status>,
    pub busy: bool,
}

// ---- the view -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    Toggle(bool),
    /// A choice with the options on both sides ("< Both >").
    Choice,
    Number,
    Text,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EntryView {
    pub before: String,
    pub after: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowView {
    /// Position among the rows of the page (what a click names).
    pub index: usize,
    pub header: Option<&'static str>,
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub control: Control,
    pub value: String,
    pub placeholder: &'static str,
    pub selected: bool,
    pub changed: bool,
    pub applies: &'static str,
    pub entry: Option<EntryView>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum KeyLineView {
    Header(&'static str),
    Row { index: usize, title: String, chords: Vec<String>, customized: bool, selected: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeysView {
    pub filter: String,
    pub lines: Vec<KeyLineView>,
    pub total_rows: usize,
    /// Set while a chord is being captured.
    pub capture: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Swatch {
    pub colors: ThemeColors,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListView {
    pub filter: String,
    pub items: Vec<(String, bool, bool)>,
    pub total: usize,
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LookView {
    pub themes: ListView,
    pub fonts: ListView,
    pub mono_only: bool,
    pub size: f32,
    pub size_focused: bool,
    /// What the preview shows: theme colors, family and size.
    pub preview_colors: Option<ThemeColors>,
    pub preview_theme: String,
    pub preview_family: String,
    pub current_theme: String,
    pub current_family: String,
    pub current_size: f32,
    pub theme_note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TomlView {
    /// (line number from 1, text) of the lines on screen.
    pub lines: Vec<(usize, String)>,
    pub caret: Option<(usize, usize)>,
    pub error_line: Option<usize>,
    pub message: Option<String>,
    pub dirty: bool,
    pub loaded: bool,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Rows(Vec<RowView>),
    Keys(KeysView),
    Look(LookView),
    Toml(TomlView),
    About(Vec<(String, String)>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub page: Page,
    pub body: Body,
    pub status: Option<Status>,
    /// One line of keys for the page.
    pub hint: String,
    pub busy: bool,
}

// ---- state --------------------------------------------------------------------------------

fn is_printable(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| !c.is_control())
}

fn lower_has(hay: &str, needle: &str) -> bool {
    needle.is_empty() || hay.to_lowercase().contains(&needle.to_lowercase())
}

impl Settings {
    pub fn new(ctx: &Ctx) -> Self {
        let mut s = Settings {
            page: Page::General,
            sel: 0,
            top: 0,
            visible: 8,
            entry: None,
            capture: None,
            key_filter: String::new(),
            look: Look { zone: Zone::Theme, theme_filter: String::new(), theme_hi: 0, font_filter: String::new(), font_hi: 0, mono_only: true, size: ctx.cfg.font_size_pt },
            editor: TextArea::new(""),
            editor_loaded: false,
            editor_msg: None,
            discard_armed: false,
            pending: HashMap::new(),
            status: None,
            busy: false,
        };
        s.look_sync(ctx);
        s
    }

    /// How many rows the page can show (the window sets it from its height).
    pub fn set_visible(&mut self, n: usize) {
        self.visible = n.max(3);
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    fn items(&self) -> Vec<&'static Def> {
        settings::defs_of(self.page).collect()
    }

    fn value_of(&self, d: &Def, ctx: &Ctx) -> V {
        self.pending.get(d.id).cloned().unwrap_or_else(|| d.read(ctx.cfg))
    }

    pub fn select_page(&mut self, p: Page, ctx: &Ctx) -> Effect {
        self.page = p;
        self.sel = 0;
        self.top = 0;
        self.entry = None;
        self.capture = None;
        self.discard_armed = false;
        self.status = None;
        match p {
            Page::Advanced if !self.editor_loaded => Effect::LoadEditor,
            Page::Appearance => {
                self.look_sync(ctx);
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn next_page(&mut self, dir: i32, ctx: &Ctx) -> Effect {
        let all = Page::ALL;
        let i = all.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        let n = all[(i + dir).rem_euclid(all.len() as i32) as usize];
        self.select_page(n, ctx)
    }

    /// The file changed (our own write or somebody else's): the values come from the file again.
    pub fn config_changed(&mut self, ctx: &Ctx) {
        self.pending.clear();
        self.look.size = ctx.cfg.font_size_pt;
        self.look_sync(ctx);
    }

    // ---- the Appearance page ----

    fn theme_list<'a>(&self, ctx: &'a Ctx) -> Vec<&'a String> {
        ctx.themes.iter().filter(|n| lower_has(n, &self.look.theme_filter)).collect()
    }

    fn font_list<'a>(&self, ctx: &'a Ctx) -> Vec<&'a String> {
        let cur = &ctx.cfg.font_family;
        ctx.fonts
            .iter()
            .filter(|n| lower_has(n, &self.look.font_filter) && (!self.look.mono_only || settings::looks_monospace(n) || *n == cur))
            .collect()
    }

    /// Put the highlight on the theme and font in force.
    fn look_sync(&mut self, ctx: &Ctx) {
        let cur_t = ctx.cfg.theme_name.clone().unwrap_or_default();
        self.look.theme_hi = self.theme_list(ctx).iter().position(|n| **n == cur_t).unwrap_or(0);
        let cur_f = &ctx.cfg.font_family;
        self.look.font_hi = self.font_list(ctx).iter().position(|n| *n == cur_f).unwrap_or(0);
    }

    // ---- keys ----

    pub fn key(&mut self, k: &KeyIn, ctx: &Ctx) -> Effect {
        if self.busy {
            // A write is on its way: only closing and moving between pages are allowed.
            if k.key == "escape" {
                return Effect::Close;
            }
            return Effect::None;
        }
        // Keys that work on every page, except while an entry or a capture is open.
        if self.capture.is_some() {
            return self.capture_key(k, ctx);
        }
        if self.entry.is_some() {
            return self.entry_key(k, ctx);
        }
        if k.ctrl && !k.alt {
            match k.key {
                "tab" if k.shift => return self.next_page(-1, ctx),
                "tab" | "pagedown" => return self.next_page(1, ctx),
                "pageup" => return self.next_page(-1, ctx),
                _ => {}
            }
        }
        self.status = None;
        match self.page {
            Page::General | Page::Terminal => self.rows_key(k, ctx),
            Page::Keybindings => self.keys_key(k, ctx),
            Page::Appearance => self.look_key(k, ctx),
            Page::Advanced => self.toml_key(k, ctx),
            Page::About => match k.key {
                "escape" => Effect::Close,
                "enter" => Effect::OpenConfigFile,
                _ => Effect::None,
            },
        }
    }

    // -- rows --

    fn move_sel(&mut self, n: usize, delta: isize) {
        if n == 0 {
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    fn rows_key(&mut self, k: &KeyIn, ctx: &Ctx) -> Effect {
        let items = self.items();
        let n = items.len();
        match k.key {
            "escape" => return Effect::Close,
            "up" => self.move_sel(n, -1),
            "down" => self.move_sel(n, 1),
            "pageup" => self.move_sel(n, -(self.visible as isize)),
            "pagedown" => self.move_sel(n, self.visible as isize),
            "home" => self.sel = 0,
            "end" => self.sel = n.saturating_sub(1),
            _ => {
                let Some(d) = items.get(self.sel).copied() else { return Effect::None };
                return self.row_action(d, k, ctx);
            }
        }
        Effect::None
    }

    fn change(&mut self, d: &'static Def, v: V) -> Effect {
        self.pending.insert(d.id, v.clone());
        Effect::Edits(d.edits(&v))
    }

    fn row_action(&mut self, d: &'static Def, k: &KeyIn, ctx: &Ctx) -> Effect {
        let cur = self.value_of(d, ctx);
        match (k.key, &d.kind) {
            ("delete", _) | ("backspace", Kind::Choice(_) | Kind::Toggle) => {
                // Back to the default: the key leaves the file.
                self.pending.insert(d.id, (d.default)());
                Effect::Edits(vec![Edit::remove(d.path)])
            }
            ("left", _) => d.step(&cur, -1).map_or(Effect::None, |v| self.change(d, v)),
            ("right", _) => d.step(&cur, 1).map_or(Effect::None, |v| self.change(d, v)),
            ("space" | "enter", Kind::Toggle | Kind::Choice(_)) => d.step(&cur, 1).map_or(Effect::None, |v| self.change(d, v)),
            ("enter" | "space", Kind::Int { .. } | Kind::Float { .. } | Kind::Text { .. }) => {
                self.start_entry(d, &cur);
                Effect::None
            }
            _ => {
                // Typing on a number or text row starts the entry with that character.
                if let (Some(t), Kind::Int { .. } | Kind::Float { .. } | Kind::Text { .. }) = (k.text, &d.kind) {
                    if !k.ctrl && !k.alt && is_printable(t) {
                        self.start_entry(d, &V::Str(String::new()));
                        if let Some(e) = self.entry.as_mut() {
                            e.ta.insert(t);
                        }
                    }
                }
                Effect::None
            }
        }
    }

    fn start_entry(&mut self, d: &'static Def, cur: &V) {
        let text = match (&d.kind, cur) {
            (Kind::Text { .. }, V::Str(s)) => s.clone(),
            (_, v) => d.show(v),
        };
        let mut ta = TextArea::new(&text);
        ta.doc_end();
        self.entry = Some(Entry { id: d.id, ta, error: None });
    }

    fn entry_key(&mut self, k: &KeyIn, _ctx: &Ctx) -> Effect {
        let Some(mut e) = self.entry.take() else { return Effect::None };
        let Some(d) = settings::def(e.id) else { return Effect::None };
        match k.key {
            "escape" => return Effect::None,
            "enter" => {
                let text = e.ta.text();
                match d.parse_text(&text) {
                    Ok(v) => return self.change(d, v),
                    Err(msg) => e.error = Some(msg),
                }
            }
            "up" | "down" | "pageup" | "pagedown" | "tab" => {}
            _ => {
                if e.ta.key(k, 1) == Out::Edited {
                    e.error = None;
                }
                // One line: a pasted line break becomes a space.
                if e.ta.lines().len() > 1 {
                    let one = e.ta.text().replace('\n', " ");
                    e.ta.set_text(&one);
                    e.ta.doc_end();
                }
            }
        }
        self.entry = Some(e);
        Effect::None
    }

    /// Text pasted into whatever takes text now (an entry, a filter or the TOML editor).
    pub fn paste(&mut self, text: &str) {
        if self.page == Page::Advanced && self.entry.is_none() && self.capture.is_none() {
            if self.editor.insert(text) {
                self.editor_msg = None;
            } else {
                self.editor_msg = Some("Too much text for the editor".into());
            }
            self.discard_armed = false;
            return;
        }
        let one = text.replace(['\r', '\n'], " ");
        if let Some(e) = self.entry.as_mut() {
            e.ta.insert(&one);
            e.error = None;
        } else if self.page == Page::Keybindings && self.capture.is_none() {
            self.key_filter.push_str(one.trim());
            self.sel = 0;
            self.top = 0;
        } else if self.page == Page::Appearance {
            match self.look.zone {
                Zone::Theme => self.look.theme_filter.push_str(one.trim()),
                Zone::Font => self.look.font_filter.push_str(one.trim()),
                Zone::Size => {}
            }
            self.look.theme_hi = 0;
            self.look.font_hi = 0;
        }
    }

    // -- keybindings --

    fn key_lines(&self, ctx: &Ctx) -> Vec<Line> {
        shortcuts::lines(ctx.cfg.keymap, &ctx.cfg.keybindings, &self.key_filter)
    }

    fn key_rows(&self, ctx: &Ctx) -> Vec<shortcuts::KeyRow> {
        self.key_lines(ctx)
            .into_iter()
            .filter_map(|l| match l {
                Line::Row(r) => Some(r),
                Line::Header(_) => None,
            })
            .collect()
    }

    fn keys_key(&mut self, k: &KeyIn, ctx: &Ctx) -> Effect {
        let rows = self.key_rows(ctx);
        let n = rows.len();
        let cur = rows.get(self.sel).map(|r| r.action);
        let plain = !k.ctrl && !k.alt && !k.win;
        match k.key {
            "escape" => {
                if !self.key_filter.is_empty() {
                    self.key_filter.clear();
                    self.sel = 0;
                    self.top = 0;
                    return Effect::None;
                }
                return Effect::Close;
            }
            "up" => self.move_sel(n, -1),
            "down" => self.move_sel(n, 1),
            "pageup" if plain => self.move_sel(n, -(self.visible as isize)),
            "pagedown" if plain => self.move_sel(n, self.visible as isize),
            "home" if plain => self.sel = 0,
            "end" if plain => self.sel = n.saturating_sub(1),
            "enter" => {
                if let Some(a) = cur {
                    self.capture = Some(Capture { action: a, mode: if k.ctrl { Mode::Add } else { Mode::Replace }, msg: None, conflict: None });
                }
            }
            "delete" if plain => {
                if let Some(a) = cur {
                    return Effect::Edits(shortcuts::plan_clear(ctx.cfg.keymap, a));
                }
            }
            "r" if k.ctrl && k.shift => return Effect::Edits(shortcuts::plan_reset_all(&ctx.cfg.keybindings)),
            "r" if k.ctrl => {
                if let Some(a) = cur {
                    return Effect::Edits(shortcuts::plan_reset(a));
                }
            }
            "backspace" if plain => {
                if self.key_filter.pop().is_some() {
                    self.sel = 0;
                    self.top = 0;
                }
            }
            "space" if plain => {
                self.key_filter.push(' ');
                self.sel = 0;
                self.top = 0;
            }
            _ => {
                if let Some(t) = k.text {
                    if plain && is_printable(t) {
                        self.key_filter.push_str(t);
                        self.sel = 0;
                        self.top = 0;
                    }
                }
            }
        }
        Effect::None
    }

    fn capture_key(&mut self, k: &KeyIn, ctx: &Ctx) -> Effect {
        let Some(mut c) = self.capture.take() else { return Effect::None };
        let bare = !k.ctrl && !k.alt && !k.shift && !k.win;
        // With a conflict waiting, Enter moves the chord and Esc gives up.
        if let Some((chord, _)) = c.conflict.clone() {
            if bare && k.key == "enter" {
                return match shortcuts::plan_assign(ctx.cfg.keymap, &ctx.cfg.keybindings, c.action, &chord, c.mode, true) {
                    Ok(edits) => Effect::Edits(edits),
                    Err(_) => Effect::None,
                };
            }
            if bare && k.key == "escape" {
                return Effect::None;
            }
            c.conflict = None;
        }
        if bare && k.key == "escape" {
            return Effect::None;
        }
        let chord = Chord::new(k.ctrl, k.alt, k.shift, k.win, k.key);
        if let Err(msg) = shortcuts::capture_check(&chord) {
            c.msg = Some(msg.to_string());
            self.capture = Some(c);
            return Effect::None;
        }
        match shortcuts::plan_assign(ctx.cfg.keymap, &ctx.cfg.keybindings, c.action, &chord, c.mode, false) {
            Ok(edits) => {
                if edits.is_empty() {
                    return Effect::None;
                }
                Effect::Edits(edits)
            }
            Err(conf) => {
                c.msg = None;
                c.conflict = Some((chord, conf));
                self.capture = Some(c);
                Effect::None
            }
        }
    }

    // -- appearance --

    fn look_key(&mut self, k: &KeyIn, ctx: &Ctx) -> Effect {
        let plain = !k.ctrl && !k.alt && !k.win;
        match k.key {
            "escape" => {
                let f = match self.look.zone {
                    Zone::Theme => &mut self.look.theme_filter,
                    Zone::Font => &mut self.look.font_filter,
                    Zone::Size => return Effect::Close,
                };
                if f.is_empty() {
                    return Effect::Close;
                }
                f.clear();
                self.look.theme_hi = 0;
                self.look.font_hi = 0;
                return Effect::None;
            }
            "tab" => {
                self.look.zone = match (self.look.zone, k.shift) {
                    (Zone::Theme, false) => Zone::Font,
                    (Zone::Font, false) => Zone::Size,
                    (Zone::Size, false) => Zone::Theme,
                    (Zone::Theme, true) => Zone::Size,
                    (Zone::Font, true) => Zone::Theme,
                    (Zone::Size, true) => Zone::Font,
                };
                return Effect::None;
            }
            "m" if k.ctrl => {
                self.look.mono_only = !self.look.mono_only;
                self.look.font_hi = 0;
                return Effect::None;
            }
            _ => {}
        }
        let page = self.visible.max(3) as isize;
        match self.look.zone {
            Zone::Theme => {
                let n = self.theme_list(ctx).len();
                match k.key {
                    "up" => self.look.theme_hi = step(self.look.theme_hi, n, -1),
                    "down" => self.look.theme_hi = step(self.look.theme_hi, n, 1),
                    "pageup" => self.look.theme_hi = step(self.look.theme_hi, n, -page),
                    "pagedown" => self.look.theme_hi = step(self.look.theme_hi, n, page),
                    "home" if plain => self.look.theme_hi = 0,
                    "end" if plain => self.look.theme_hi = n.saturating_sub(1),
                    "enter" => {
                        if let Some(name) = self.theme_list(ctx).get(self.look.theme_hi) {
                            return Effect::Theme((*name).clone());
                        }
                    }
                    "backspace" if plain => {
                        self.look.theme_filter.pop();
                        self.look.theme_hi = 0;
                    }
                    _ => self.type_filter(k, true),
                }
            }
            Zone::Font => {
                let n = self.font_list(ctx).len();
                match k.key {
                    "up" => self.look.font_hi = step(self.look.font_hi, n, -1),
                    "down" => self.look.font_hi = step(self.look.font_hi, n, 1),
                    "pageup" => self.look.font_hi = step(self.look.font_hi, n, -page),
                    "pagedown" => self.look.font_hi = step(self.look.font_hi, n, page),
                    "home" if plain => self.look.font_hi = 0,
                    "end" if plain => self.look.font_hi = n.saturating_sub(1),
                    "enter" => {
                        if let Some(name) = self.font_list(ctx).get(self.look.font_hi) {
                            return Effect::Font { family: Some((*name).clone()), size: None };
                        }
                    }
                    "backspace" if plain => {
                        self.look.font_filter.pop();
                        self.look.font_hi = 0;
                    }
                    _ => self.type_filter(k, false),
                }
            }
            Zone::Size => match k.key {
                "left" | "down" => self.look.size = (self.look.size - 0.5).max(settings::FONT_MIN),
                "right" | "up" => self.look.size = (self.look.size + 0.5).min(settings::FONT_MAX),
                "enter" => return Effect::Font { family: None, size: Some(self.look.size) },
                _ => {}
            },
        }
        Effect::None
    }

    fn type_filter(&mut self, k: &KeyIn, theme: bool) {
        let t = if k.key == "space" { Some(" ") } else { k.text };
        let Some(t) = t else { return };
        if k.ctrl || k.alt || k.win || !is_printable(t) {
            return;
        }
        if theme {
            self.look.theme_filter.push_str(t);
            self.look.theme_hi = 0;
        } else {
            self.look.font_filter.push_str(t);
            self.look.font_hi = 0;
        }
    }

    // -- TOML editor --

    fn toml_key(&mut self, k: &KeyIn, _ctx: &Ctx) -> Effect {
        if !self.editor_loaded {
            return if k.key == "escape" { Effect::Close } else { Effect::None };
        }
        if k.key == "escape" && !k.ctrl && !k.alt {
            if self.editor.is_dirty() && !self.discard_armed {
                self.discard_armed = true;
                self.editor_msg = Some("There are changes that are not saved. Press Esc again to close without saving, or Ctrl+S to save.".into());
                return Effect::None;
            }
            return Effect::Close;
        }
        self.discard_armed = false;
        if k.ctrl && !k.alt {
            match k.key {
                "s" => return Effect::SaveText(self.editor.text()),
                "r" => return Effect::LoadEditor,
                "v" => return Effect::PasteIntoEditor,
                "c" if k.shift => return Effect::CopyText(self.editor.text()),
                _ => {}
            }
        }
        if k.key == "v" && k.ctrl {
            return Effect::PasteIntoEditor;
        }
        let page = self.visible.max(3);
        if self.editor.key(k, page) == Out::Edited {
            self.editor_msg = None;
        }
        self.editor.ensure_visible(self.visible);
        Effect::None
    }

    /// The editor's text arrived from the file (or the read failed).
    pub fn editor_loaded(&mut self, r: Result<String, String>) {
        match r {
            Ok(text) => {
                self.editor = TextArea::new(&text);
                self.editor_loaded = true;
                self.editor_msg = None;
                self.discard_armed = false;
            }
            Err(e) => {
                self.editor_loaded = false;
                self.editor_msg = Some(e);
            }
        }
    }

    /// A write finished.
    pub fn saved(&mut self, what: Wrote, r: Result<Vec<String>, String>) {
        self.busy = false;
        match (what, r) {
            (Wrote::Setting, Ok(warn)) => {
                self.status = Some(Status { ok: true, text: warn.first().map_or_else(|| "Saved".to_string(), |w| format!("Saved. {w}")) });
            }
            (Wrote::Setting, Err(e)) => {
                self.pending.clear();
                self.status = Some(Status { ok: false, text: e });
            }
            (Wrote::Editor, Ok(warn)) => {
                self.editor.mark_saved();
                self.editor_msg = None;
                self.status = Some(Status { ok: true, text: warn.first().map_or_else(|| "Saved".to_string(), |w| format!("Saved. {w}")) });
            }
            (Wrote::Editor, Err(e)) => {
                if let Some(n) = error_line(&e) {
                    self.editor.go_to_line(n);
                    self.editor.ensure_visible(self.visible);
                }
                self.editor_msg = Some(e);
            }
        }
    }

    /// A click on a row of a rows page, or on a control inside it.
    pub fn click_row(&mut self, index: usize, ctx: &Ctx) {
        if self.page == Page::General || self.page == Page::Terminal {
            if index < self.items().len() {
                self.sel = index;
                self.entry = None;
            }
        } else if self.page == Page::Keybindings && index < self.key_rows(ctx).len() {
            self.sel = index;
        }
    }

    /// A click on a theme or font in its list: highlight it (the preview follows).
    pub fn pick_look(&mut self, zone: Zone, name: &str, ctx: &Ctx) {
        self.look.zone = zone;
        match zone {
            Zone::Theme => {
                if let Some(i) = self.theme_list(ctx).iter().position(|n| n.as_str() == name) {
                    self.look.theme_hi = i;
                }
            }
            Zone::Font => {
                if let Some(i) = self.font_list(ctx).iter().position(|n| n.as_str() == name) {
                    self.look.font_hi = i;
                }
            }
            Zone::Size => {}
        }
    }

    pub fn click_look(&mut self, zone: Zone, index: Option<usize>) {
        self.look.zone = zone;
        if let Some(i) = index {
            match zone {
                Zone::Theme => self.look.theme_hi = i,
                Zone::Font => self.look.font_hi = i,
                Zone::Size => {}
            }
        }
    }

    /// Clicks on the buttons of the pages. These are the same as the keys.
    pub fn press(&mut self, name: &str, ctx: &Ctx) -> Effect {
        let k = |key: &'static str| KeyIn { key, ..Default::default() };
        match name {
            "row-left" => self.key(&k("left"), ctx),
            "row-right" => self.key(&k("right"), ctx),
            "row-toggle" => self.key(&k("space"), ctx),
            "row-edit" => self.key(&k("enter"), ctx),
            "row-reset" => self.key(&k("delete"), ctx),
            "key-change" => self.key(&k("enter"), ctx),
            "key-add" => self.key(&KeyIn { key: "enter", ctrl: true, ..Default::default() }, ctx),
            "key-clear" => self.key(&k("delete"), ctx),
            "key-reset" => self.key(&KeyIn { key: "r", ctrl: true, ..Default::default() }, ctx),
            "key-reset-all" => self.key(&KeyIn { key: "r", ctrl: true, shift: true, ..Default::default() }, ctx),
            "key-cancel" | "entry-cancel" => self.key(&k("escape"), ctx),
            "key-move" | "entry-ok" => self.key(&k("enter"), ctx),
            "look-apply" => self.key(&k("enter"), ctx),
            "look-mono" => self.key(&KeyIn { key: "m", ctrl: true, ..Default::default() }, ctx),
            "size-minus" => {
                self.look.zone = Zone::Size;
                self.key(&k("left"), ctx)
            }
            "size-plus" => {
                self.look.zone = Zone::Size;
                self.key(&k("right"), ctx)
            }
            "toml-save" => self.key(&KeyIn { key: "s", ctrl: true, ..Default::default() }, ctx),
            "toml-revert" => self.key(&KeyIn { key: "r", ctrl: true, ..Default::default() }, ctx),
            "toml-paste" => Effect::PasteIntoEditor,
            "toml-copy" => Effect::CopyText(self.editor.text()),
            "about-open" => Effect::OpenConfigFile,
            "close" => Effect::Close,
            _ => Effect::None,
        }
    }

    /// A click inside the TOML editor text.
    pub fn click_editor(&mut self, row: usize, col: usize) {
        self.editor.click(row, col);
    }

    pub fn scroll(&mut self, delta: isize, ctx: &Ctx) {
        match self.page {
            Page::Advanced => self.editor.scroll(delta, self.visible),
            Page::General | Page::Terminal => {
                let n = self.items().len();
                self.top = (self.top as isize + delta).clamp(0, n.saturating_sub(self.visible) as isize) as usize;
            }
            Page::Keybindings => {
                let n = self.key_lines(ctx).len();
                self.top = (self.top as isize + delta).clamp(0, n.saturating_sub(self.visible) as isize) as usize;
            }
            _ => {}
        }
    }

    // ---- the view ----

    fn follow(&mut self, line_of_sel: usize) {
        let v = self.visible.max(1);
        if line_of_sel < self.top {
            self.top = line_of_sel;
        } else if line_of_sel >= self.top + v {
            self.top = line_of_sel + 1 - v;
        }
    }

    pub fn view(&mut self, ctx: &Ctx) -> View {
        let hint = self.hint();
        let body = match self.page {
            Page::General | Page::Terminal => self.rows_view(ctx),
            Page::Keybindings => self.keys_view(ctx),
            Page::Appearance => Body::Look(self.look_view(ctx)),
            Page::Advanced => Body::Toml(self.toml_view()),
            Page::About => Body::About(vec![
                ("Version".into(), env!("CARGO_PKG_VERSION").to_string()),
                ("Configuration file".into(), ctx.config_path.to_string()),
                ("Shortcut set".into(), ctx.cfg.keymap.label().to_string()),
                ("Settings are saved to the file above. The file is read again whenever it changes, so edits made elsewhere show here.".into(), String::new()),
            ]),
        };
        View { page: self.page, body, status: self.status.clone(), hint, busy: self.busy }
    }

    fn hint(&self) -> String {
        if let Some(c) = &self.capture {
            return if c.conflict.is_some() { "Enter: move it here. Esc: cancel.".into() } else { "Press the new shortcut. Esc: cancel.".into() };
        }
        if self.entry.is_some() {
            return "Enter: save. Esc: cancel.".into();
        }
        match self.page {
            Page::General | Page::Terminal => "Up/Down: row. Left/Right or Space: change. Enter: type a value. Delete: default. Ctrl+Tab: next page. Esc: close.".into(),
            Page::Keybindings => "Type to search. Enter: change. Ctrl+Enter: add. Delete: unbind. Ctrl+R: default. Ctrl+Shift+R: all defaults. Esc: close.".into(),
            Page::Appearance => "Tab: list. Up/Down: preview. Enter: apply. Ctrl+M: all fonts. Left/Right: size. Esc: close.".into(),
            Page::Advanced => "Ctrl+S: save. Ctrl+R: read the file again. Ctrl+V: paste. Esc: close.".into(),
            Page::About => "Enter: open the file in your editor. Ctrl+Tab: next page. Esc: close.".into(),
        }
    }

    fn rows_view(&mut self, ctx: &Ctx) -> Body {
        let items = self.items();
        let n = items.len();
        self.sel = self.sel.min(n.saturating_sub(1));
        self.follow(self.sel);
        let mut rows = Vec::new();
        let mut last_section = "";
        for (i, d) in items.iter().enumerate() {
            let header = (d.section != last_section).then_some(d.section);
            last_section = d.section;
            let v = self.value_of(d, ctx);
            let control = match (&d.kind, &v) {
                (Kind::Toggle, V::Bool(b)) => Control::Toggle(*b),
                (Kind::Choice(_), _) => Control::Choice,
                (Kind::Int { .. } | Kind::Float { .. }, _) => Control::Number,
                _ => Control::Text,
            };
            let entry = self.entry.as_ref().filter(|e| e.id == d.id).map(|e| {
                let (r, c) = e.ta.cursor();
                let line = e.ta.lines().get(r).cloned().unwrap_or_default();
                let at = line.char_indices().nth(c).map(|x| x.0).unwrap_or(line.len());
                EntryView { before: line[..at].to_string(), after: line[at..].to_string(), error: e.error.clone() }
            });
            rows.push(RowView {
                index: i,
                header,
                id: d.id,
                label: d.label,
                help: d.help,
                control,
                value: d.show(&v),
                placeholder: if let Kind::Text { placeholder } = d.kind { placeholder } else { "" },
                selected: i == self.sel,
                changed: !d.is_default(&v),
                applies: d.applies,
                entry,
            });
        }
        // Only the rows from `top` are returned; the window draws them in order.
        let top = self.top.min(rows.len());
        // A header belongs to the first row of its section. A section cut by the scroll position
        // still shows its heading above the first row on screen.
        let mut out: Vec<RowView> = rows.split_off(top);
        if let Some(first) = out.first_mut() {
            if first.header.is_none() {
                first.header = items.get(top).map(|d| d.section);
            }
        }
        Body::Rows(out)
    }

    fn keys_view(&mut self, ctx: &Ctx) -> Body {
        let lines = self.key_lines(ctx);
        let rows: usize = lines.iter().filter(|l| matches!(l, Line::Row(_))).count();
        self.sel = self.sel.min(rows.saturating_sub(1));
        // The line the selected row is on.
        let mut idx = 0;
        let mut sel_line = 0;
        for (li, l) in lines.iter().enumerate() {
            if matches!(l, Line::Row(_)) {
                if idx == self.sel {
                    sel_line = li;
                }
                idx += 1;
            }
        }
        self.follow(sel_line);
        let top = self.top.min(lines.len().saturating_sub(1));
        let mut out = Vec::new();
        let mut ri = 0;
        for (li, l) in lines.into_iter().enumerate() {
            match l {
                Line::Header(h) => {
                    if li >= top {
                        out.push(KeyLineView::Header(h));
                    }
                }
                Line::Row(r) => {
                    if li >= top {
                        out.push(KeyLineView::Row { index: ri, title: r.title, chords: r.chords, customized: r.customized, selected: ri == self.sel });
                    }
                    ri += 1;
                }
            }
        }
        let capture = self.capture.as_ref().map(|c| {
            let title = c.action.info().title.to_string();
            let prompt = match (&c.conflict, &c.msg) {
                (Some((chord, conf)), _) => format!("{chord} is used by \"{}\". Press Enter to move it here, or Esc to cancel.", conf.title),
                (None, Some(m)) => m.clone(),
                (None, None) => match c.mode {
                    Mode::Replace => "Press the new shortcut".to_string(),
                    Mode::Add => "Press the shortcut to add".to_string(),
                },
            };
            (title, prompt)
        });
        Body::Keys(KeysView { filter: self.key_filter.clone(), lines: out, total_rows: rows, capture })
    }

    fn look_view(&mut self, ctx: &Ctx) -> LookView {
        let themes = self.theme_list(ctx);
        let fonts = self.font_list(ctx);
        let cur_theme = ctx.cfg.theme_name.clone().unwrap_or_default();
        let window = |hi: usize, n: usize| -> (usize, usize) {
            let v = self.visible.max(3);
            let first = if hi >= v { hi + 1 - v } else { 0 };
            (first, (first + v).min(n))
        };
        let (t0, t1) = window(self.look.theme_hi, themes.len());
        let (f0, f1) = window(self.look.font_hi, fonts.len());
        let theme_items: Vec<(String, bool, bool)> =
            themes[t0..t1].iter().enumerate().map(|(i, n)| ((*n).clone(), t0 + i == self.look.theme_hi, **n == cur_theme)).collect();
        let font_items: Vec<(String, bool, bool)> =
            fonts[f0..f1].iter().enumerate().map(|(i, n)| ((*n).clone(), f0 + i == self.look.font_hi, **n == ctx.cfg.font_family)).collect();
        let preview_theme = themes.get(self.look.theme_hi).map(|n| (*n).clone()).unwrap_or_else(|| cur_theme.clone());
        let preview_family = fonts.get(self.look.font_hi).map(|n| (*n).clone()).unwrap_or_else(|| ctx.cfg.font_family.clone());
        LookView {
            themes: ListView { filter: self.look.theme_filter.clone(), items: theme_items, total: themes.len(), focused: self.look.zone == Zone::Theme },
            fonts: ListView { filter: self.look.font_filter.clone(), items: font_items, total: fonts.len(), focused: self.look.zone == Zone::Font },
            mono_only: self.look.mono_only,
            size: self.look.size,
            size_focused: self.look.zone == Zone::Size,
            preview_colors: (ctx.colors)(&preview_theme),
            preview_theme,
            preview_family,
            current_theme: cur_theme,
            current_family: ctx.cfg.font_family.clone(),
            current_size: ctx.cfg.font_size_pt,
            theme_note: None,
        }
    }

    fn toml_view(&mut self) -> TomlView {
        self.editor.ensure_visible(self.visible);
        let top = self.editor.top;
        let lines: Vec<(usize, String)> =
            self.editor.lines().iter().enumerate().skip(top).take(self.visible).map(|(i, l)| (i + 1, l.clone())).collect();
        let err = self.editor_msg.as_deref().and_then(error_line);
        TomlView {
            lines,
            caret: Some(self.editor.cursor()),
            error_line: err,
            message: self.editor_msg.clone(),
            dirty: self.editor.is_dirty(),
            loaded: self.editor_loaded,
            path: String::new(),
        }
    }
}

fn step(i: usize, n: usize, d: isize) -> usize {
    if n == 0 {
        return 0;
    }
    (i as isize + d).clamp(0, n as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::apply;

    fn colors(_: &str) -> Option<ThemeColors> {
        None
    }

    struct H {
        cfg: Config,
        themes: Vec<String>,
        fonts: Vec<String>,
    }

    impl H {
        fn new(text: &str) -> H {
            H {
                cfg: Config::parse(text).unwrap().config,
                themes: vec!["Dracula".into(), "Nord".into(), "Solarized".into()],
                fonts: vec!["Cascadia Mono".into(), "Consolas".into(), "Segoe UI".into(), "Fira Code".into()],
            }
        }
        fn ctx(&self) -> Ctx<'_> {
            Ctx { cfg: &self.cfg, themes: &self.themes, fonts: &self.fonts, colors: &colors, config_path: "C:\\x\\config.toml" }
        }
    }

    fn k(key: &str) -> KeyIn<'_> {
        KeyIn { key, ..Default::default() }
    }
    fn ck<'a>(key: &'a str, ctrl: bool, alt: bool, shift: bool) -> KeyIn<'a> {
        KeyIn { key, ctrl, alt, shift, ..Default::default() }
    }
    fn typed(t: &str) -> KeyIn<'_> {
        KeyIn { key: "x", text: Some(t), ..Default::default() }
    }

    fn rows(s: &mut Settings, h: &H) -> Vec<RowView> {
        match s.view(&h.ctx()).body {
            Body::Rows(r) => r,
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn a_choice_row_steps_and_the_new_value_shows_before_the_file_reloads() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        // General: shell, keymap, backdrop, sidebar. Down once is keymap.
        assert_eq!(s.key(&k("down"), &c), Effect::None);
        let e = s.key(&k("right"), &c);
        assert_eq!(e, Effect::Edits(vec![Edit::set("keymap", V::Str("windows".into()))]));
        let r = rows(&mut s, &h);
        let km = r.iter().find(|x| x.id == "keymap").unwrap();
        assert_eq!(km.value, "Windows shortcuts");
        assert!(km.changed);
        // A save that failed puts the old value back.
        s.saved(Wrote::Setting, Err("Not saved".into()));
        let r = rows(&mut s, &h);
        assert_eq!(r.iter().find(|x| x.id == "keymap").unwrap().value, "GTK shortcuts");
        assert_eq!(s.status().map(|x| x.ok), Some(false));
    }

    #[test]
    fn a_toggle_flips_and_delete_returns_the_default_by_removing_the_key() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Terminal, &c);
        // Rows: scrollback, bell, notifications, smart copy ... find smart copy.
        let idx = settings::defs_of(Page::Terminal).position(|d| d.id == "smart_copy").unwrap();
        for _ in 0..idx {
            s.key(&k("down"), &c);
        }
        assert_eq!(s.key(&k("space"), &c), Effect::Edits(vec![Edit::set("smart_copy", V::Bool(false))]));
        assert_eq!(s.key(&k("delete"), &c), Effect::Edits(vec![Edit::remove("smart_copy")]));
    }

    #[test]
    fn a_number_is_typed_checked_and_saved_in_the_unit_of_the_file() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Terminal, &c);
        assert_eq!(s.key(&k("enter"), &c), Effect::None); // scrollback row: entry opens
        for _ in 0..2 {
            s.key(&k("backspace"), &c);
        }
        s.key(&typed("3"), &c);
        s.key(&typed("2"), &c);
        let r = rows(&mut s, &h);
        assert_eq!(r[0].entry.as_ref().map(|e| e.before.as_str()), Some("32"));
        assert_eq!(s.key(&k("enter"), &c), Effect::Edits(vec![Edit::set("scrollback_bytes", V::Int(32 * 1024 * 1024))]));
        // A bad value stays open with the reason.
        s.select_page(Page::Terminal, &c);
        s.key(&k("enter"), &c);
        for _ in 0..4 {
            s.key(&k("backspace"), &c);
        }
        s.key(&typed("9"), &c);
        s.key(&typed("9"), &c);
        s.key(&typed("9"), &c);
        s.key(&typed("9"), &c);
        assert_eq!(s.key(&k("enter"), &c), Effect::None);
        let r = rows(&mut s, &h);
        assert!(r[0].entry.as_ref().and_then(|e| e.error.as_deref()).unwrap().contains("Between"));
        // Esc closes the entry, not the window.
        assert_eq!(s.key(&k("escape"), &c), Effect::None);
        assert!(rows(&mut s, &h)[0].entry.is_none());
        assert_eq!(s.key(&k("escape"), &c), Effect::Close);
    }

    #[test]
    fn a_blank_shell_entry_removes_the_key() {
        let h = H::new("shell = \"cmd.exe\"\n");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.key(&k("enter"), &c);
        for _ in 0..30 {
            s.key(&k("backspace"), &c);
        }
        assert_eq!(s.key(&k("enter"), &c), Effect::Edits(vec![Edit::remove("shell")]));
    }

    #[test]
    fn pages_switch_with_ctrl_tab_and_the_editor_loads_on_first_open() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        assert_eq!(s.page, Page::General);
        s.key(&ck("tab", true, false, false), &c);
        assert_eq!(s.page, Page::Appearance);
        s.key(&ck("tab", true, false, true), &c);
        assert_eq!(s.page, Page::General);
        assert_eq!(s.select_page(Page::Advanced, &c), Effect::LoadEditor);
        s.editor_loaded(Ok("a = 1\n".into()));
        assert_eq!(s.select_page(Page::About, &c), Effect::None);
        assert_eq!(s.select_page(Page::Advanced, &c), Effect::None);
    }

    #[test]
    fn capturing_a_chord_assigns_it_and_a_taken_chord_asks_first() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Keybindings, &c);
        // Search for the row.
        for ch in "toggle sidebar".chars() {
            let t = ch.to_string();
            if ch == ' ' {
                s.key(&k("space"), &c);
            } else {
                s.key(&typed(&t), &c);
            }
        }
        let v = s.view(&c);
        let Body::Keys(kv) = &v.body else { panic!() };
        assert_eq!(kv.total_rows, 1);
        assert_eq!(s.key(&k("enter"), &c), Effect::None);
        assert!(matches!(&s.view(&c).body, Body::Keys(x) if x.capture.is_some()));
        // A plain letter is refused with a reason.
        assert_eq!(s.key(&typed("a"), &c), Effect::None);
        let Body::Keys(kv) = s.view(&c).body else { panic!() };
        assert!(kv.capture.unwrap().1.contains("Ctrl"));
        // A free chord.
        let e = s.key(&ck("h", true, true, false), &c);
        assert_eq!(e, Effect::Edits(vec![Edit::set("keybindings.toggle_sidebar", V::Str("ctrl+alt+h".into()))]));
        // A taken chord waits for Enter.
        s.key(&k("enter"), &c);
        assert_eq!(s.key(&ck("t", true, false, true), &c), Effect::None);
        let Body::Keys(kv) = s.view(&c).body else { panic!() };
        let prompt = kv.capture.unwrap().1;
        assert!(prompt.contains("New tab") && prompt.contains("Enter"), "{prompt}");
        match s.key(&k("enter"), &c) {
            Effect::Edits(e) => {
                assert_eq!(e.len(), 2);
                let text = apply("", &e).unwrap();
                assert!(text.contains("new_tab") && text.contains("ctrl+shift+t"));
            }
            e => panic!("{e:?}"),
        }
        // Esc cancels a capture.
        s.key(&k("enter"), &c);
        assert_eq!(s.key(&k("escape"), &c), Effect::None);
        assert!(matches!(&s.view(&c).body, Body::Keys(x) if x.capture.is_none()));
    }

    #[test]
    fn unbind_reset_and_reset_all_on_the_keybindings_page() {
        let h = H::new("[keybindings]\nnew_tab = \"ctrl+k\"\n");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Keybindings, &c);
        for ch in "new tab".chars() {
            if ch == ' ' {
                s.key(&k("space"), &c);
            } else {
                s.key(&typed(&ch.to_string()), &c);
            }
        }
        let Body::Keys(kv) = s.view(&c).body else { panic!() };
        let row = kv.lines.iter().find_map(|l| if let KeyLineView::Row { title, customized, chords, .. } = l { (title == "New tab").then(|| (*customized, chords.clone())) } else { None }).unwrap();
        assert_eq!(row, (true, vec!["Ctrl+K".to_string()]));
        assert_eq!(s.key(&k("delete"), &c), Effect::Edits(vec![Edit::set("keybindings.new_tab", V::Str("none".into()))]));
        assert_eq!(s.key(&ck("r", true, false, false), &c), Effect::Edits(vec![Edit::remove("keybindings.new_tab")]));
        assert_eq!(s.key(&ck("r", true, false, true), &c), Effect::Edits(vec![Edit::remove("keybindings.new_tab")]));
        // Esc clears the search first, then closes.
        assert_eq!(s.key(&k("escape"), &c), Effect::None);
        assert_eq!(s.key(&k("escape"), &c), Effect::Close);
    }

    #[test]
    fn appearance_previews_a_highlighted_theme_and_applies_on_enter() {
        let h = H::new("theme = \"Nord\"\nfont_family = \"Consolas\"\nfont_size = 12\n");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Appearance, &c);
        let Body::Look(v) = s.view(&c).body else { panic!() };
        assert_eq!(v.preview_theme, "Nord");
        assert_eq!(v.current_theme, "Nord");
        assert_eq!(v.preview_family, "Consolas");
        assert_eq!(v.size, 12.0);
        s.key(&k("down"), &c);
        let Body::Look(v) = s.view(&c).body else { panic!() };
        assert_eq!(v.preview_theme, "Solarized");
        assert_eq!(v.current_theme, "Nord");
        assert_eq!(s.key(&k("enter"), &c), Effect::Theme("Solarized".into()));
        // Typing filters the list.
        for ch in ["d", "r"] {
            s.key(&typed(ch), &c);
        }
        let Body::Look(v) = s.view(&c).body else { panic!() };
        assert_eq!(v.themes.items.len(), 1);
        assert_eq!(v.preview_theme, "Dracula");
        // Fonts: the short list hides the proportional ones until Ctrl+M.
        s.key(&k("escape"), &c);
        s.key(&k("tab"), &c);
        let Body::Look(v) = s.view(&c).body else { panic!() };
        assert!(v.fonts.items.iter().all(|f| f.0 != "Segoe UI"));
        s.key(&ck("m", true, false, false), &c);
        let Body::Look(v) = s.view(&c).body else { panic!() };
        assert!(v.fonts.items.iter().any(|f| f.0 == "Segoe UI"));
        // Size zone.
        s.key(&k("tab"), &c);
        s.key(&k("right"), &c);
        assert_eq!(s.key(&k("enter"), &c), Effect::Font { family: None, size: Some(12.5) });
    }

    #[test]
    fn the_toml_editor_saves_whole_text_warns_before_discarding_and_marks_the_error_line() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        assert_eq!(s.select_page(Page::Advanced, &c), Effect::LoadEditor);
        s.editor_loaded(Ok("a = 1\nb = 2\n".into()));
        s.key(&typed("x"), &c);
        let e = s.key(&ck("s", true, false, false), &c);
        assert_eq!(e, Effect::SaveText("xa = 1\nb = 2\n".into()));
        s.busy = true;
        s.saved(Wrote::Editor, Err("TOML parse error at line 2, column 3".into()));
        let Body::Toml(t) = s.view(&c).body else { panic!() };
        assert_eq!(t.error_line, Some(2));
        assert!(t.dirty);
        assert_eq!(t.caret, Some((1, 0)));
        // Esc with unsaved text asks first.
        assert_eq!(s.key(&k("escape"), &c), Effect::None);
        assert_eq!(s.key(&k("escape"), &c), Effect::Close);
        // Saved: clean.
        s.saved(Wrote::Editor, Ok(vec![]));
        let Body::Toml(t) = s.view(&c).body else { panic!() };
        assert!(!t.dirty);
        // Paste goes through the shell.
        assert_eq!(s.key(&ck("v", true, false, false), &c), Effect::PasteIntoEditor);
        s.paste("p = 3\nq = 4");
        let Body::Toml(t) = s.view(&c).body else { panic!() };
        assert!(t.dirty);
    }

    #[test]
    fn keys_do_nothing_while_a_write_is_in_flight_except_close() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.busy = true;
        assert_eq!(s.key(&k("right"), &c), Effect::None);
        assert_eq!(s.key(&k("escape"), &c), Effect::Close);
    }

    #[test]
    fn scrolling_keeps_the_selected_row_on_screen() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::Terminal, &c);
        s.set_visible(3);
        for _ in 0..6 {
            s.key(&k("down"), &c);
        }
        let r = rows(&mut s, &h);
        assert!(r.iter().take(3).any(|x| x.selected), "{:?}", r.iter().map(|x| (x.id, x.selected)).collect::<Vec<_>>());
        assert!(r[0].header.is_some());
    }

    #[test]
    fn about_shows_the_file_and_enter_opens_it() {
        let h = H::new("");
        let c = h.ctx();
        let mut s = Settings::new(&c);
        s.select_page(Page::About, &c);
        let v = s.view(&c);
        let Body::About(rows) = &v.body else { panic!() };
        assert!(rows.iter().any(|(_, v)| v.contains("config.toml")));
        assert_eq!(s.key(&k("enter"), &c), Effect::OpenConfigFile);
    }
}
