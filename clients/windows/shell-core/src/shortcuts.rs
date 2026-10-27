//! The Keybindings page and the shortcuts window (F1): what they list and how a chord that the
//! user pressed becomes an edit of `[keybindings]` in `config.toml`. Pure.

use std::collections::HashMap;

use crate::keys::{Action, Chord, Keymap, KeymapPreset, ACTIONS};
use crate::settings_file::{Edit, V};

/// The heading an action is listed under.
pub fn category(a: Action) -> &'static str {
    use Action::*;
    match a {
        NewTab | CloseTab | NextTab | PrevTab | GotoTab(_) | MoveTabLeft | MoveTabRight | RenameTab | DuplicateTab | CloseTabsRight | CloseOtherTabs => "Tabs",
        RenamePane | SplitRight | SplitDown | SplitLeft | SplitUp | ClosePane | FocusLeft | FocusRight | FocusUp | FocusDown | ZoomPane | ResizeLeft | ResizeRight | ResizeUp | ResizeDown | PaneMenu => "Panes",
        PasteCtrlV | CopyOrInterrupt | CopyRaw | SelectAll | ClipboardWriteOff | ClipboardWriteAsk | ClipboardWriteAllow => "Clipboard",
        Search => "Search",
        ZoomIn | ZoomOut | ZoomReset => "Zoom",
        ClearTerminal | ResetTerminal => "Terminal",
        RenameWorkspace | NewWorkspace | NextWorkspace | PrevWorkspace | GotoWorkspace(_) | DuplicateWorkspace | DeleteWorkspace | MoveWorkspaceUp | MoveWorkspaceDown | ToggleSidebar => "Workspaces",
        PairMachine | NextMachine | PrevMachine => "Machines",
        Profile(_) => "Profiles",
        CommandPalette | OpenConfig | ReloadConfig | ChooseTheme | Quit | CheckUpdates | OpenSettings | ShowShortcuts => "Application",
    }
}

/// Headings in the order the page shows them.
pub const CATEGORIES: &[&str] = &["Tabs", "Panes", "Workspaces", "Machines", "Clipboard", "Search", "Zoom", "Terminal", "Profiles", "Application"];

/// A chord as `config.toml` spells it ("ctrl+shift+t", "alt+shift+plus").
pub fn config_form(c: &Chord) -> String {
    let mut s = String::new();
    for (on, name) in [(c.ctrl, "ctrl"), (c.alt, "alt"), (c.shift, "shift"), (c.win, "win")] {
        if on {
            s.push_str(name);
            s.push('+');
        }
    }
    s.push_str(match c.key.as_str() {
        "=" if c.shift => "plus",
        "-" => "minus",
        " " => "space",
        k => k,
    });
    s
}

/// Is this chord one the Keybindings page accepts? A key that types text needs Ctrl, Alt or Win
/// with it (else the binding would eat that character); the F keys stand alone. Ctrl+Alt with a
/// letter, digit or symbol is how Windows reports AltGr, so it would take that character away
/// from every keyboard that types it with AltGr: refused.
pub fn capture_check(c: &Chord) -> Result<(), &'static str> {
    const MODS: &[&str] = &["shift", "control", "ctrl", "alt", "win", "super", "meta", "platform", "capslock", "function", "fn"];
    if c.key.is_empty() || MODS.contains(&c.key.as_str()) {
        return Err("Press a key together with the modifiers");
    }
    let is_f = c.key.len() >= 2 && c.key.starts_with('f') && c.key[1..].chars().all(|ch| ch.is_ascii_digit());
    if c.ctrl && c.alt && c.key.chars().count() == 1 {
        return Err("Ctrl+Alt with a letter or symbol is AltGr on many keyboards. Use Ctrl+Shift, or Ctrl+Alt with an arrow or F key");
    }
    if is_f || c.ctrl || c.alt || c.win {
        Ok(())
    } else {
        Err("Hold Ctrl, Alt or Win with the key (F1 to F24 alone are fine)")
    }
}

/// One action in the page.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyRow {
    pub action: Action,
    pub name: &'static str,
    pub title: String,
    /// The chords now in force, as shown ("Ctrl+Shift+T").
    pub chords: Vec<String>,
    /// The file has an entry for it.
    pub customized: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Header(&'static str),
    Row(KeyRow),
}

/// Everything the page lists, under headings. `filter` keeps the rows whose title, chord or
/// heading contains it (case does not matter).
pub fn lines(preset: KeymapPreset, overrides: &HashMap<String, Vec<String>>, filter: &str) -> Vec<Line> {
    let km = Keymap::new(preset, overrides);
    let f = filter.trim().to_lowercase();
    let mut out = Vec::new();
    for cat in CATEGORIES {
        let mut rows = Vec::new();
        for info in ACTIONS.iter().filter(|i| category(i.action) == *cat) {
            let chords: Vec<String> = km.chords(info.action).iter().map(|c| c.to_string()).collect();
            let row = KeyRow { action: info.action, name: info.name, title: info.title.to_string(), chords, customized: overrides.contains_key(info.name) };
            let hit = f.is_empty() || row.title.to_lowercase().contains(&f) || row.chords.iter().any(|c| c.to_lowercase().contains(&f)) || cat.to_lowercase().contains(&f);
            if hit {
                rows.push(row);
            }
        }
        if !rows.is_empty() {
            out.push(Line::Header(cat));
            out.extend(rows.into_iter().map(Line::Row));
        }
    }
    out
}

/// Chords in force for one action, in the form the file stores.
fn forms(km: &Keymap, a: Action) -> Vec<String> {
    km.chords(a).iter().map(config_form).collect()
}

/// The value to store for `a` when it should have exactly `chords`: no entry when that is the
/// default of the set, `"none"` for no chord, a string for one, an array for several.
fn edits_for(preset: KeymapPreset, a: Action, chords: Vec<String>) -> Vec<Edit> {
    let info = a.info();
    let path = format!("keybindings.{}", info.name);
    let canon = |v: &[String]| -> Vec<String> { v.iter().filter_map(|s| Chord::parse(s)).map(|c| config_form(&c)).collect() };
    if canon(&chords) == canon(&info.default_keys(preset).iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
        return vec![Edit::Remove(path)];
    }
    let v = match chords.len() {
        0 => V::Str("none".into()),
        1 => V::Str(chords[0].clone()),
        _ => V::List(chords),
    };
    vec![Edit::Set(path, v)]
}

/// Another action owns the chord.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub action: Action,
    pub title: String,
}

/// How the chord is stored: replace the action's chords with it, or add it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Replace,
    Add,
}

/// Who has `chord` now, if not `a`.
pub fn owner(preset: KeymapPreset, overrides: &HashMap<String, Vec<String>>, a: Action, chord: &Chord) -> Option<Conflict> {
    let km = Keymap::new(preset, overrides);
    km.lookup(chord).filter(|o| *o != a).map(|o| Conflict { action: o, title: o.info().title.to_string() })
}

/// The edits that give `a` the chord. When another action has it, the call is refused with the
/// owner (`take = false`), or the other action loses it (`take = true`, "move it here").
pub fn plan_assign(
    preset: KeymapPreset,
    overrides: &HashMap<String, Vec<String>>,
    a: Action,
    chord: &Chord,
    mode: Mode,
    take: bool,
) -> Result<Vec<Edit>, Conflict> {
    let km = Keymap::new(preset, overrides);
    let mut edits = Vec::new();
    if let Some(o) = km.lookup(chord).filter(|o| *o != a) {
        if !take {
            return Err(Conflict { action: o, title: o.info().title.to_string() });
        }
        let keep: Vec<String> = forms(&km, o).into_iter().filter(|s| Chord::parse(s).as_ref() != Some(chord)).collect();
        edits.extend(edits_for(preset, o, keep));
    }
    let mut mine = match mode {
        Mode::Replace => Vec::new(),
        Mode::Add => forms(&km, a),
    };
    let f = config_form(chord);
    if !mine.contains(&f) {
        mine.push(f);
    }
    edits.extend(edits_for(preset, a, mine));
    Ok(edits)
}

/// Would the keymap that `edits` make still reach Settings from the keyboard? It does while
/// `open_settings` or `command_palette` (whose list has the Settings entry) has a chord. The last
/// unbind of both is refused, so a user cannot lock themselves out of this window.
pub fn keeps_settings_reachable(preset: KeymapPreset, overrides: &HashMap<String, Vec<String>>, edits: &[Edit]) -> Result<(), &'static str> {
    let mut o = overrides.clone();
    for e in edits {
        match e {
            Edit::Set(path, v) => {
                if let Some(name) = path.strip_prefix("keybindings.") {
                    let keys = match v {
                        V::Str(s) => vec![s.clone()],
                        V::List(l) => l.clone(),
                        _ => continue,
                    };
                    o.insert(name.to_string(), keys);
                }
            }
            Edit::Remove(path) => {
                if let Some(name) = path.strip_prefix("keybindings.") {
                    o.remove(name);
                }
            }
        }
    }
    let km = Keymap::new(preset, &o);
    if km.chords(Action::OpenSettings).is_empty() && km.chords(Action::CommandPalette).is_empty() {
        Err("Not changed: Settings would have no shortcut. Keep a shortcut for Settings or for the command palette")
    } else {
        Ok(())
    }
}

/// No chord for `a` (an entry `"none"`).
pub fn plan_clear(preset: KeymapPreset, a: Action) -> Vec<Edit> {
    edits_for(preset, a, Vec::new())
}

/// Back to the default of the set: the entry goes away.
pub fn plan_reset(a: Action) -> Vec<Edit> {
    vec![Edit::Remove(format!("keybindings.{}", a.info().name))]
}

/// Every entry of the actions the client knows goes away. Entries it does not know stay.
pub fn plan_reset_all(overrides: &HashMap<String, Vec<String>>) -> Vec<Edit> {
    let mut names: Vec<&str> = overrides.keys().filter(|n| Action::from_name(n).is_some()).map(String::as_str).collect();
    names.sort_unstable();
    names.into_iter().map(|n| Edit::Remove(format!("keybindings.{n}"))).collect()
}

// ---- the shortcuts window --------------------------------------------------------------

pub struct Group {
    pub title: &'static str,
    /// (what it does, the keys), in the order shown.
    pub items: Vec<(String, String)>,
}

/// The F1 window: every action that has a chord in the set now in force (with the user's own
/// entries), by heading, plus the keys that are not actions.
pub fn groups(preset: KeymapPreset, overrides: &HashMap<String, Vec<String>>) -> Vec<Group> {
    let km = Keymap::new(preset, overrides);
    let mut out: Vec<Group> = Vec::new();
    for cat in CATEGORIES {
        let mut items = Vec::new();
        for info in ACTIONS.iter().filter(|i| category(i.action) == *cat) {
            let chords: Vec<String> = km.chords(info.action).iter().map(|c| c.to_string()).collect();
            if !chords.is_empty() {
                items.push((info.title.to_string(), chords.join("  or  ")));
            }
        }
        if *cat == "Search" {
            items.push(("Next match (in the find bar)".into(), "Enter".into()));
            items.push(("Previous match".into(), "Shift+Enter".into()));
            items.push(("Close the find bar".into(), "Esc".into()));
        }
        if *cat == "Application" {
            items.push(("Open a link".into(), "Ctrl+Click on the link".into()));
        }
        if !items.is_empty() {
            out.push(Group { title: cat, items });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::apply;

    fn none() -> HashMap<String, Vec<String>> {
        HashMap::new()
    }
    fn ch(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn every_action_has_a_heading_and_every_heading_is_listed() {
        for i in ACTIONS {
            assert!(CATEGORIES.contains(&category(i.action)), "{}", i.name);
        }
        for p in KeymapPreset::ALL {
            let l = lines(p, &none(), "");
            let rows = l.iter().filter(|x| matches!(x, Line::Row(_))).count();
            assert_eq!(rows, ACTIONS.len());
        }
    }

    #[test]
    fn a_chord_written_to_the_file_reads_back_as_the_same_chord() {
        for p in KeymapPreset::ALL {
            for i in ACTIONS {
                for k in i.default_keys(p) {
                    let c = ch(k);
                    assert_eq!(Chord::parse(&config_form(&c)), Some(c.clone()), "{k}");
                }
            }
        }
        for k in ["alt+shift+plus", "ctrl+=", "ctrl+minus", "ctrl+,", "ctrl+space", "f1", "ctrl+alt+pageup"] {
            let c = ch(k);
            assert_eq!(Chord::parse(&config_form(&c)), Some(c), "{k}");
        }
    }

    #[test]
    fn a_pressed_chord_needs_a_modifier_unless_it_is_an_f_key() {
        assert!(capture_check(&ch("ctrl+k")).is_ok());
        assert!(capture_check(&ch("alt+left")).is_ok());
        // AltGr arrives as Ctrl+Alt: a character key with both is refused, other keys are fine.
        for k in ["ctrl+alt+h", "ctrl+alt+2", "ctrl+alt+;", "ctrl+alt+shift+e"] {
            assert!(capture_check(&ch(k)).unwrap_err().contains("AltGr"), "{k}");
        }
        for k in ["ctrl+alt+left", "ctrl+alt+f5", "ctrl+alt+tab", "ctrl+shift+h", "alt+h"] {
            assert!(capture_check(&ch(k)).is_ok(), "{k}");
        }
        assert!(capture_check(&Chord::new(false, false, false, false, "f5")).is_ok());
        assert!(capture_check(&Chord::new(false, false, false, false, "a")).is_err());
        assert!(capture_check(&Chord::new(false, false, true, false, "a")).is_err());
        assert!(capture_check(&Chord::new(true, false, false, false, "shift")).is_err());
        assert!(capture_check(&Chord::new(false, false, false, false, "enter")).is_err());
    }

    #[test]
    fn assigning_a_free_chord_writes_one_entry_and_it_loads() {
        let e = plan_assign(KeymapPreset::Gtk, &none(), Action::ToggleSidebar, &ch("ctrl+alt+h"), Mode::Replace, false).unwrap();
        assert_eq!(e, vec![Edit::Set("keybindings.toggle_sidebar".into(), V::Str("ctrl+alt+h".into()))]);
        let text = apply("# mine\n", &e).unwrap();
        let cfg = crate::config::Config::parse(&text).unwrap();
        assert!(cfg.warnings.is_empty());
        let km = Keymap::new(KeymapPreset::Gtk, &cfg.config.keybindings);
        assert_eq!(km.lookup(&ch("ctrl+alt+h")), Some(Action::ToggleSidebar));
        assert_eq!(km.lookup(&ch("ctrl+alt+b")), None);
    }

    #[test]
    fn adding_keeps_the_other_chords_and_the_default_set_removes_the_entry() {
        let e = plan_assign(KeymapPreset::Gtk, &none(), Action::NextTab, &ch("ctrl+alt+j"), Mode::Add, false).unwrap();
        assert_eq!(e, vec![Edit::Set("keybindings.next_tab".into(), V::List(vec!["ctrl+tab".into(), "ctrl+pagedown".into(), "ctrl+alt+j".into()]))]);
        // Putting the default back is no entry at all.
        let e = plan_assign(KeymapPreset::Gtk, &none(), Action::ToggleSidebar, &ch("ctrl+alt+b"), Mode::Replace, false).unwrap();
        assert_eq!(e, vec![Edit::Remove("keybindings.toggle_sidebar".into())]);
    }

    #[test]
    fn a_chord_somebody_has_is_refused_or_moved() {
        let r = plan_assign(KeymapPreset::Gtk, &none(), Action::ToggleSidebar, &ch("ctrl+shift+t"), Mode::Replace, false);
        assert_eq!(r, Err(Conflict { action: Action::NewTab, title: "New tab".into() }));
        let e = plan_assign(KeymapPreset::Gtk, &none(), Action::ToggleSidebar, &ch("ctrl+shift+t"), Mode::Replace, true).unwrap();
        assert_eq!(e[0], Edit::Set("keybindings.new_tab".into(), V::Str("none".into())));
        assert_eq!(e[1], Edit::Set("keybindings.toggle_sidebar".into(), V::Str("ctrl+shift+t".into())));
        let text = apply("", &e).unwrap();
        let cfg = crate::config::Config::parse(&text).unwrap().config;
        let km = Keymap::new(KeymapPreset::Gtk, &cfg.keybindings);
        assert_eq!(km.lookup(&ch("ctrl+shift+t")), Some(Action::ToggleSidebar));
        assert!(km.chords(Action::NewTab).is_empty());
        // Moving one of two chords leaves the other.
        let e = plan_assign(KeymapPreset::Gtk, &none(), Action::ToggleSidebar, &ch("ctrl+tab"), Mode::Replace, true).unwrap();
        assert_eq!(e[0], Edit::Set("keybindings.next_tab".into(), V::Str("ctrl+pagedown".into())));
        // A chord the action already has is no conflict.
        assert!(plan_assign(KeymapPreset::Gtk, &none(), Action::NewTab, &ch("ctrl+shift+t"), Mode::Add, false).is_ok());
    }

    #[test]
    fn the_last_way_to_settings_cannot_be_unbound() {
        let none = none();
        let clear = |a| plan_clear(KeymapPreset::Windows, a);
        // The windows set has no Settings chord of its own: the palette is the way.
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &none, &clear(Action::NewTab)).is_ok());
        let e = keeps_settings_reachable(KeymapPreset::Windows, &none, &clear(Action::CommandPalette)).unwrap_err();
        assert!(e.contains("Settings"), "{e}");
        // With a Settings chord the palette may go, and then the Settings chord may not.
        let mut o = none.clone();
        o.insert("open_settings".into(), vec!["ctrl+shift+o".into()]);
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &o, &clear(Action::CommandPalette)).is_ok());
        o.insert("command_palette".into(), vec!["none".into()]);
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &o, &clear(Action::OpenSettings)).is_err());
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &o, &plan_reset(Action::OpenSettings)).is_err());
        // Putting one back is fine, and so is moving the Settings chord to another key.
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &o, &plan_reset(Action::CommandPalette)).is_ok());
        let mv = plan_assign(KeymapPreset::Windows, &o, Action::OpenSettings, &ch("ctrl+shift+j"), Mode::Replace, false).unwrap();
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &o, &mv).is_ok());
        // Taking the palette's chord for another action leaves the palette without one.
        let take = plan_assign(KeymapPreset::Windows, &none, Action::NewTab, &ch("ctrl+shift+p"), Mode::Replace, true).unwrap();
        assert!(keeps_settings_reachable(KeymapPreset::Windows, &none, &take).is_err());
    }

    #[test]
    fn reset_and_clear_and_reset_all() {
        assert_eq!(plan_reset(Action::NewTab), vec![Edit::Remove("keybindings.new_tab".into())]);
        assert_eq!(plan_clear(KeymapPreset::Gtk, Action::NewTab), vec![Edit::Set("keybindings.new_tab".into(), V::Str("none".into()))]);
        // An action with no default chord: clear is no entry.
        assert_eq!(plan_clear(KeymapPreset::Gtk, Action::RenameTab), vec![Edit::Remove("keybindings.rename_tab".into())]);
        let mut o = none();
        o.insert("new_tab".into(), vec!["ctrl+k".into()]);
        o.insert("mystery".into(), vec!["x".into()]);
        let e = plan_reset_all(&o);
        assert_eq!(e, vec![Edit::Remove("keybindings.new_tab".into())]);
        let text = apply("# keep\n[keybindings]\nnew_tab = \"ctrl+k\"\nmystery = \"x\"\n", &e).unwrap();
        assert!(text.contains("mystery") && text.contains("# keep") && !text.contains("new_tab"));
    }

    #[test]
    fn the_page_marks_what_the_file_changed_and_filters() {
        let mut o = none();
        o.insert("new_tab".into(), vec!["ctrl+k".into()]);
        let l = lines(KeymapPreset::Gtk, &o, "new tab");
        let rows: Vec<&KeyRow> = l.iter().filter_map(|x| if let Line::Row(r) = x { Some(r) } else { None }).collect();
        let nt = rows.iter().find(|r| r.action == Action::NewTab).unwrap();
        assert!(nt.customized);
        assert_eq!(nt.chords, vec!["Ctrl+K".to_string()]);
        assert!(rows.iter().all(|r| r.title.to_lowercase().contains("new tab")));
        let by_chord = lines(KeymapPreset::Gtk, &none(), "ctrl+alt+b");
        assert!(by_chord.iter().any(|x| matches!(x, Line::Row(r) if r.action == Action::ToggleSidebar)));
    }

    #[test]
    fn the_shortcuts_window_follows_the_set_and_the_file() {
        let g = groups(KeymapPreset::Gtk, &none());
        let all: Vec<&(String, String)> = g.iter().flat_map(|x| x.items.iter()).collect();
        assert!(all.iter().any(|(t, k)| t == "New tab" && k == "Ctrl+Shift+T"));
        assert!(all.iter().any(|(t, _)| t == "Open a link"));
        // No chord, no row.
        assert!(!all.iter().any(|(t, _)| t == "Rename tab"));
        let w = groups(KeymapPreset::Windows, &none());
        assert!(w.iter().flat_map(|x| x.items.iter()).any(|(t, _)| t == "Rename tab"));
        let mut o = none();
        o.insert("new_tab".into(), vec!["none".into()]);
        let g = groups(KeymapPreset::Gtk, &o);
        assert!(!g.iter().flat_map(|x| x.items.iter()).any(|(t, _)| t == "New tab"));
    }
}
