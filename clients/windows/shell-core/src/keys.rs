//! Actions, key chords and the configurable keymap. UI-independent: the GPUI
//! layer converts a `Keystroke` into a `Chord` and asks the `Keymap`.

use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// 1-based. 9 means the last tab, as in browsers.
    GotoTab(u8),
    MoveTabLeft,
    MoveTabRight,
    RenameTab,
    CheckUpdates,
    /// Per-machine policy for programs that set the clipboard (OSC 52, write only).
    ClipboardWriteOff,
    ClipboardWriteAsk,
    ClipboardWriteAllow,
    RenamePane,
    RenameWorkspace,
    SplitRight,
    SplitDown,
    ClosePane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ZoomPane,
    ToggleSidebar,
    CommandPalette,
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    PairMachine,
    NextMachine,
    PrevMachine,
    SelectAll,
    /// Find in the scrollback of the active pane (toggles the find bar).
    Search,
    /// Copy the selection without the smart-copy cleanup (box-drawing characters stay).
    CopyRaw,
    /// Ctrl+L to the program, as GTK's "Clear".
    ClearTerminal,
    /// Full reset of the terminal state (RIS), then Ctrl+L, as GTK's "Reset".
    ResetTerminal,
    /// Move the divider next to the active pane, 20 px, as GTK's Alt+Shift+Arrow.
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    DuplicateTab,
    CloseTabsRight,
    CloseOtherTabs,
    DuplicateWorkspace,
    DeleteWorkspace,
    MoveWorkspaceUp,
    MoveWorkspaceDown,
    /// The context menu of the active pane, at its top-left corner.
    PaneMenu,
}

pub struct ActionInfo {
    pub action: Action,
    /// Key used in `config.toml` `[keybindings]`.
    pub name: &'static str,
    /// Title shown in the command palette.
    pub title: &'static str,
    pub default_keys: &'static [&'static str],
}

macro_rules! info {
    ($a:expr, $n:expr, $t:expr, [$($k:expr),*]) => {
        ActionInfo { action: $a, name: $n, title: $t, default_keys: &[$($k),*] }
    };
}

pub const ACTIONS: &[ActionInfo] = &[
    info!(Action::NewTab, "new_tab", "New tab", ["ctrl+shift+t"]),
    info!(Action::CloseTab, "close_tab", "Close tab", ["ctrl+shift+q"]),
    info!(Action::NextTab, "next_tab", "Next tab", ["ctrl+tab"]),
    info!(Action::PrevTab, "prev_tab", "Previous tab", ["ctrl+shift+tab"]),
    info!(Action::GotoTab(1), "goto_tab_1", "Go to tab 1", ["ctrl+1"]),
    info!(Action::GotoTab(2), "goto_tab_2", "Go to tab 2", ["ctrl+2"]),
    info!(Action::GotoTab(3), "goto_tab_3", "Go to tab 3", ["ctrl+3"]),
    info!(Action::GotoTab(4), "goto_tab_4", "Go to tab 4", ["ctrl+4"]),
    info!(Action::GotoTab(5), "goto_tab_5", "Go to tab 5", ["ctrl+5"]),
    info!(Action::GotoTab(6), "goto_tab_6", "Go to tab 6", ["ctrl+6"]),
    info!(Action::GotoTab(7), "goto_tab_7", "Go to tab 7", ["ctrl+7"]),
    info!(Action::GotoTab(8), "goto_tab_8", "Go to tab 8", ["ctrl+8"]),
    info!(Action::GotoTab(9), "goto_tab_9", "Go to last tab", ["ctrl+9"]),
    info!(Action::MoveTabLeft, "move_tab_left", "Move tab left", ["ctrl+shift+pageup"]),
    info!(Action::MoveTabRight, "move_tab_right", "Move tab right", ["ctrl+shift+pagedown"]),
    info!(Action::RenameTab, "rename_tab", "Rename tab", ["ctrl+shift+r"]),
    info!(Action::CheckUpdates, "check_updates", "Check for updates", []),
    info!(Action::ClipboardWriteOff, "clipboard_write_off", "Clipboard writes from programs on this machine: off", []),
    info!(Action::ClipboardWriteAsk, "clipboard_write_ask", "Clipboard writes from programs on this machine: ask", []),
    info!(Action::ClipboardWriteAllow, "clipboard_write_allow", "Clipboard writes from programs on this machine: allow", []),
    info!(Action::RenamePane, "rename_pane", "Rename pane", []),
    info!(Action::RenameWorkspace, "rename_workspace", "Rename workspace", []),
    info!(Action::SplitRight, "split_right", "Split pane right", ["alt+shift+plus"]),
    info!(Action::SplitDown, "split_down", "Split pane down", ["alt+shift+minus"]),
    info!(Action::ClosePane, "close_pane", "Close pane", ["ctrl+shift+w"]),
    info!(Action::FocusLeft, "focus_left", "Focus pane left", ["ctrl+alt+left"]),
    info!(Action::FocusRight, "focus_right", "Focus pane right", ["ctrl+alt+right"]),
    info!(Action::FocusUp, "focus_up", "Focus pane up", ["ctrl+alt+up"]),
    info!(Action::FocusDown, "focus_down", "Focus pane down", ["ctrl+alt+down"]),
    info!(Action::ZoomPane, "zoom_pane", "Toggle pane zoom", ["ctrl+shift+z"]),
    info!(Action::ToggleSidebar, "toggle_sidebar", "Toggle sidebar", ["ctrl+shift+b"]),
    info!(Action::CommandPalette, "command_palette", "Command palette", ["ctrl+shift+p"]),
    info!(Action::NewWorkspace, "new_workspace", "New workspace", ["ctrl+shift+n"]),
    info!(Action::NextWorkspace, "next_workspace", "Next workspace", []),
    info!(Action::PrevWorkspace, "prev_workspace", "Previous workspace", []),
    info!(Action::PairMachine, "pair_machine", "Pair machine...", []),
    info!(Action::NextMachine, "next_machine", "Next machine", []),
    info!(Action::PrevMachine, "prev_machine", "Previous machine", []),
    info!(Action::Search, "search", "Find in terminal", ["ctrl+shift+f"]),
    info!(Action::SelectAll, "select_all", "Select all", ["ctrl+shift+a"]),
    info!(Action::CopyRaw, "copy_raw", "Copy selection as is", []),
    info!(Action::ClearTerminal, "clear_terminal", "Clear terminal", []),
    info!(Action::ResetTerminal, "reset_terminal", "Reset terminal", []),
    info!(Action::ResizeLeft, "resize_pane_left", "Resize pane left", ["alt+shift+left"]),
    info!(Action::ResizeRight, "resize_pane_right", "Resize pane right", ["alt+shift+right"]),
    info!(Action::ResizeUp, "resize_pane_up", "Resize pane up", ["alt+shift+up"]),
    info!(Action::ResizeDown, "resize_pane_down", "Resize pane down", ["alt+shift+down"]),
    info!(Action::DuplicateTab, "duplicate_tab", "Duplicate tab", []),
    info!(Action::CloseTabsRight, "close_tabs_right", "Close tabs to the right", []),
    info!(Action::CloseOtherTabs, "close_other_tabs", "Close other tabs", []),
    info!(Action::DuplicateWorkspace, "duplicate_workspace", "Duplicate workspace", []),
    info!(Action::DeleteWorkspace, "delete_workspace", "Delete workspace", []),
    info!(Action::MoveWorkspaceUp, "move_workspace_up", "Move workspace up", []),
    info!(Action::MoveWorkspaceDown, "move_workspace_down", "Move workspace down", []),
    info!(Action::PaneMenu, "pane_menu", "Pane menu", []),
];

impl Action {
    pub fn info(self) -> &'static ActionInfo {
        ACTIONS.iter().find(|i| i.action == self).expect("every action is listed")
    }
    pub fn from_name(name: &str) -> Option<Action> {
        ACTIONS.iter().find(|i| i.name == name).map(|i| i.action)
    }
}

/// A key combination. `key` is lower case and named like GPUI's `Keystroke::key`
/// ("a", "tab", "pageup", "-", "="). `plus` and `minus` are accepted as aliases
/// of "=" and "-" (the unshifted keys on a US layout).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: String,
}

impl Chord {
    pub fn new(ctrl: bool, alt: bool, shift: bool, win: bool, key: &str) -> Self {
        Chord { ctrl, alt, shift, win, key: canonical_key(key) }
    }

    /// Parse "ctrl+shift+p", "alt+shift+plus", "ctrl-tab".
    pub fn parse(s: &str) -> Option<Chord> {
        let s = s.trim().to_lowercase();
        if s.is_empty() || s == "none" {
            return None;
        }
        // A trailing "+" or "-" key is the key itself: "alt+shift++".
        let mut c = Chord { ctrl: false, alt: false, shift: false, win: false, key: String::new() };
        let mut rest = s.as_str();
        loop {
            let split = rest.find(['+', '-']);
            match split {
                Some(i) if i > 0 && i + 1 < rest.len() + 0 => {
                    let m = &rest[..i];
                    match m {
                        "ctrl" | "control" => c.ctrl = true,
                        "alt" | "option" => c.alt = true,
                        "shift" => c.shift = true,
                        "win" | "super" | "cmd" | "meta" => c.win = true,
                        _ => return None,
                    }
                    rest = &rest[i + 1..];
                }
                _ => break,
            }
        }
        if rest.is_empty() {
            return None;
        }
        c.key = canonical_key(rest);
        Some(c)
    }
}

fn canonical_key(k: &str) -> String {
    match k {
        "plus" | "+" | "equal" | "equals" => "=".into(),
        "minus" | "dash" | "hyphen" => "-".into(),
        "esc" => "escape".into(),
        "return" => "enter".into(),
        "pgup" | "page_up" => "pageup".into(),
        "pgdn" | "page_down" | "pagedn" => "pagedown".into(),
        "space" => " ".into(),
        other => other.to_lowercase(),
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        if self.win {
            write!(f, "Win+")?;
        }
        let k = match self.key.as_str() {
            "=" if self.shift => "Plus".to_string(),
            "=" => "=".to_string(),
            "-" => "Minus".to_string(),
            " " => "Space".to_string(),
            "pageup" => "PageUp".to_string(),
            "pagedown" => "PageDown".to_string(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut cs = k.chars();
                cs.next().map(|c| c.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default()
            }
        };
        write!(f, "{k}")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    by_chord: HashMap<Chord, Action>,
    by_action: HashMap<Action, Vec<Chord>>,
    /// Config entries that did not parse, for logging.
    pub warnings: Vec<String>,
}

impl Keymap {
    /// Defaults, then `overrides` (config key `name` to a list of chords; an
    /// empty list or "none" unbinds the action).
    pub fn new(overrides: &HashMap<String, Vec<String>>) -> Self {
        let mut km = Keymap::default();
        let mut per_action: Vec<(Action, Vec<String>)> = ACTIONS
            .iter()
            .map(|i| (i.action, i.default_keys.iter().map(|s| s.to_string()).collect()))
            .collect();
        for (name, keys) in overrides {
            match Action::from_name(name) {
                Some(a) => {
                    if let Some(e) = per_action.iter_mut().find(|(x, _)| *x == a) {
                        e.1 = keys.clone();
                    }
                }
                None => km.warnings.push(format!("unknown action '{name}' in keybindings")),
            }
        }
        // Explicit user bindings win a chord conflict with defaults.
        let user: Vec<Action> = overrides.keys().filter_map(|n| Action::from_name(n)).collect();
        for pass_user in [false, true] {
            for (action, keys) in &per_action {
                if user.contains(action) != pass_user {
                    continue;
                }
                for k in keys {
                    match Chord::parse(k) {
                        Some(c) => {
                            if let Some(old) = km.by_chord.insert(c.clone(), *action) {
                                if old != *action {
                                    if let Some(v) = km.by_action.get_mut(&old) {
                                        v.retain(|x| x != &c);
                                    }
                                }
                            }
                            let v = km.by_action.entry(*action).or_default();
                            if !v.contains(&c) {
                                v.push(c);
                            }
                        }
                        None if k.trim().eq_ignore_ascii_case("none") || k.trim().is_empty() => {}
                        None => km.warnings.push(format!("bad key '{k}' for {}", action.info().name)),
                    }
                }
            }
        }
        km
    }

    pub fn default_map() -> Self {
        Self::new(&HashMap::new())
    }

    pub fn lookup(&self, c: &Chord) -> Option<Action> {
        self.by_chord.get(c).copied()
    }

    pub fn chords(&self, a: Action) -> &[Chord] {
        self.by_action.get(&a).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// First chord of an action as display text, or empty.
    pub fn shortcut(&self, a: Action) -> String {
        self.chords(a).first().map(|c| c.to_string()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        let c = Chord::parse("Alt+Shift+Plus").unwrap();
        assert_eq!(c, Chord::new(false, true, true, false, "="));
        assert_eq!(Chord::parse("alt-shift--").unwrap(), Chord::new(false, true, true, false, "-"));
        assert_eq!(Chord::parse("alt+shift++").unwrap(), c);
        assert_eq!(Chord::parse("ctrl-tab").unwrap().key, "tab");
        assert_eq!(Chord::parse("ctrl+1").unwrap().key, "1");
        assert!(Chord::parse("none").is_none());
        assert!(Chord::parse("bogus+x").is_none());
        assert_eq!(Chord::parse("ctrl+shift+p").unwrap().to_string(), "Ctrl+Shift+P");
        assert_eq!(c.to_string(), "Alt+Shift+Plus");
    }

    #[test]
    fn defaults_and_overrides() {
        let km = Keymap::default_map();
        assert_eq!(km.lookup(&Chord::new(true, false, true, false, "p")), Some(Action::CommandPalette));
        assert_eq!(km.lookup(&Chord::new(false, true, true, false, "=")), Some(Action::SplitRight));
        assert_eq!(km.lookup(&Chord::new(false, true, true, false, "-")), Some(Action::SplitDown));
        assert_eq!(km.lookup(&Chord::new(true, false, false, false, "3")), Some(Action::GotoTab(3)));
        assert_eq!(km.shortcut(Action::ToggleSidebar), "Ctrl+Shift+B");
        let mut o = HashMap::new();
        o.insert("toggle_sidebar".to_string(), vec!["ctrl+shift+k".to_string()]);
        o.insert("new_tab".to_string(), vec!["none".to_string()]);
        o.insert("split_right".to_string(), vec!["ctrl+shift+k".to_string()]);
        o.insert("nonsense".to_string(), vec!["x".to_string()]);
        let km = Keymap::new(&o);
        assert!(km.lookup(&Chord::new(true, false, true, false, "b")).is_none());
        assert!(km.chords(Action::NewTab).is_empty());
        // Conflict: both user-bound, last inserted wins, but only one owner remains.
        let owner = km.lookup(&Chord::new(true, false, true, false, "k")).unwrap();
        assert!(owner == Action::SplitRight || owner == Action::ToggleSidebar);
        assert_eq!(km.warnings.len(), 1);
        let names: std::collections::HashSet<_> = ACTIONS.iter().map(|a| a.name).collect();
        assert_eq!(names.len(), ACTIONS.len());
    }

    #[test]
    fn terminal_actions_have_their_default_chords_and_no_warnings() {
        let km = Keymap::default_map();
        assert_eq!(km.lookup(&Chord::new(true, false, true, false, "f")), Some(Action::Search));
        assert_eq!(km.lookup(&Chord::new(true, false, true, false, "a")), Some(Action::SelectAll));
        assert!(km.chords(Action::ResetTerminal).is_empty(), "menu-only in GTK: palette only here");
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        assert_eq!(Action::from_name("copy_raw"), Some(Action::CopyRaw));
    }

    /// WP2: only the keyboard pane resize has a chord (GTK's Alt+Shift+Arrow, free on Windows).
    /// Every chord that existed before still does what it did.
    #[test]
    fn wp2_actions_take_only_gtks_chord_and_move_no_existing_one() {
        let km = Keymap::default_map();
        let c = |ctrl, alt, shift, key: &str| km.lookup(&Chord::new(ctrl, alt, shift, false, key));
        assert_eq!(c(false, true, true, "left"), Some(Action::ResizeLeft));
        assert_eq!(c(false, true, true, "right"), Some(Action::ResizeRight));
        assert_eq!(c(false, true, true, "up"), Some(Action::ResizeUp));
        assert_eq!(c(false, true, true, "down"), Some(Action::ResizeDown));
        // Focus keeps Ctrl+Alt+Arrow, splits keep Alt+Shift+plus and minus.
        assert_eq!(c(true, true, false, "left"), Some(Action::FocusLeft));
        assert_eq!(c(false, true, true, "plus"), Some(Action::SplitRight));
        assert_eq!(c(false, true, true, "minus"), Some(Action::SplitDown));
        assert_eq!(c(true, false, true, "q"), Some(Action::CloseTab));
        assert_eq!(c(true, false, true, "pageup"), Some(Action::MoveTabLeft));
        for a in [
            Action::DuplicateTab,
            Action::CloseTabsRight,
            Action::CloseOtherTabs,
            Action::DuplicateWorkspace,
            Action::DeleteWorkspace,
            Action::MoveWorkspaceUp,
            Action::MoveWorkspaceDown,
            Action::PaneMenu,
        ] {
            assert!(km.chords(a).is_empty(), "{a:?} has no chord: GTK has none");
        }
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
    }
}
