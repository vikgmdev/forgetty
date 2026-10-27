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
    /// Close every tab of this PC, stop the local server and quit.
    EndSession,
    RetryLocalServer,
    /// Per-machine policy for programs that set the clipboard (OSC 52, write only).
    ClipboardWriteOff,
    ClipboardWriteAsk,
    ClipboardWriteAllow,
    RenamePane,
    RenameWorkspace,
    SplitRight,
    SplitDown,
    SplitLeft,
    SplitUp,
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
    /// Font size of the active pane, one point at a time (6 to 72), as GTK. Reset returns to
    /// the configured size.
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// A new tab with profile N (1-based), as GTK's Ctrl+Shift+1..9.
    Profile(u8),
    /// Open `config.toml` in the default editor (created when missing).
    OpenConfig,
    /// Read `config.toml` again now.
    ReloadConfig,
    /// A list of every theme: choosing one applies it to this session.
    ChooseTheme,
    /// Switch to workspace N (1-based), as GTK's Alt+1..9.
    GotoWorkspace(u8),
    /// End the app (GTK's Ctrl+Shift+Q). Asks first while a local shell is open.
    Quit,
    /// The Settings window (GTK's Ctrl+.).
    OpenSettings,
    /// The shortcuts window (GTK's F1).
    ShowShortcuts,
    /// Paste (GTK's Ctrl+V). Ctrl+Shift+V pastes in both sets and is not an action.
    PasteCtrlV,
    /// Copy while a selection is on screen, else the key goes to the program as the interrupt
    /// (GTK's Ctrl+C). Ctrl+Shift+C copies in both sets and is not an action.
    CopyOrInterrupt,
}

/// Which set of default chords is in force. `gtk` (the default) takes the chords of the GTK app,
/// so the same habits work on both. `windows` is the set this client had before, where several
/// chords differ (close tab Ctrl+Shift+Q, new workspace Ctrl+Shift+N, focus Ctrl+Alt+Arrow, tab
/// jump Ctrl+1..9, zoom pane, rename tab, select all). A `[keybindings]` entry overrides either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KeymapPreset {
    #[default]
    Gtk,
    Windows,
}

impl KeymapPreset {
    pub const ALL: [KeymapPreset; 2] = [KeymapPreset::Gtk, KeymapPreset::Windows];

    /// The value in `config.toml`.
    pub fn name(self) -> &'static str {
        match self {
            KeymapPreset::Gtk => "gtk",
            KeymapPreset::Windows => "windows",
        }
    }

    /// For the palette and the notice.
    pub fn label(self) -> &'static str {
        match self {
            KeymapPreset::Gtk => "GTK shortcuts",
            KeymapPreset::Windows => "Windows shortcuts",
        }
    }

    pub fn parse(s: &str) -> Option<KeymapPreset> {
        KeymapPreset::ALL.into_iter().find(|p| p.name() == s.trim().to_ascii_lowercase())
    }
}

pub struct ActionInfo {
    pub action: Action,
    /// Key used in `config.toml` `[keybindings]`.
    pub name: &'static str,
    /// Title shown in the command palette.
    pub title: &'static str,
    /// Default chords in the `gtk` set and in the `windows` set.
    pub gtk_keys: &'static [&'static str],
    pub windows_keys: &'static [&'static str],
}

impl ActionInfo {
    pub fn default_keys(&self, preset: KeymapPreset) -> &'static [&'static str] {
        match preset {
            KeymapPreset::Gtk => self.gtk_keys,
            KeymapPreset::Windows => self.windows_keys,
        }
    }
}

/// An action with the same chords in both sets.
macro_rules! info {
    ($a:expr, $n:expr, $t:expr, [$($k:expr),*]) => {
        ActionInfo { action: $a, name: $n, title: $t, gtk_keys: &[$($k),*], windows_keys: &[$($k),*] }
    };
}

/// An action whose chords differ: `gtk` first, then `windows`.
macro_rules! differ {
    ($a:expr, $n:expr, $t:expr, gtk [$($g:expr),*], windows [$($w:expr),*]) => {
        ActionInfo { action: $a, name: $n, title: $t, gtk_keys: &[$($g),*], windows_keys: &[$($w),*] }
    };
}

pub const ACTIONS: &[ActionInfo] = &[
    info!(Action::NewTab, "new_tab", "New tab", ["ctrl+shift+t"]),
    differ!(Action::CloseTab, "close_tab", "Close tab", gtk [], windows ["ctrl+shift+q"]),
    differ!(Action::NextTab, "next_tab", "Next tab", gtk ["ctrl+tab", "ctrl+pagedown"], windows ["ctrl+tab"]),
    differ!(Action::PrevTab, "prev_tab", "Previous tab", gtk ["ctrl+shift+tab", "ctrl+pageup"], windows ["ctrl+shift+tab"]),
    differ!(Action::GotoTab(1), "goto_tab_1", "Go to tab 1", gtk [], windows ["ctrl+1"]),
    differ!(Action::GotoTab(2), "goto_tab_2", "Go to tab 2", gtk [], windows ["ctrl+2"]),
    differ!(Action::GotoTab(3), "goto_tab_3", "Go to tab 3", gtk [], windows ["ctrl+3"]),
    differ!(Action::GotoTab(4), "goto_tab_4", "Go to tab 4", gtk [], windows ["ctrl+4"]),
    differ!(Action::GotoTab(5), "goto_tab_5", "Go to tab 5", gtk [], windows ["ctrl+5"]),
    differ!(Action::GotoTab(6), "goto_tab_6", "Go to tab 6", gtk [], windows ["ctrl+6"]),
    differ!(Action::GotoTab(7), "goto_tab_7", "Go to tab 7", gtk [], windows ["ctrl+7"]),
    differ!(Action::GotoTab(8), "goto_tab_8", "Go to tab 8", gtk [], windows ["ctrl+8"]),
    differ!(Action::GotoTab(9), "goto_tab_9", "Go to last tab", gtk [], windows ["ctrl+9"]),
    info!(Action::MoveTabLeft, "move_tab_left", "Move tab left", ["ctrl+shift+pageup"]),
    info!(Action::MoveTabRight, "move_tab_right", "Move tab right", ["ctrl+shift+pagedown"]),
    differ!(Action::RenameTab, "rename_tab", "Rename tab", gtk [], windows ["ctrl+shift+r"]),
    info!(Action::CheckUpdates, "check_updates", "Check for updates", []),
    info!(Action::EndSession, "end_session", "Quit and end all tabs on this PC", []),
    info!(Action::RetryLocalServer, "retry_local_server", "Retry the local server", []),
    info!(Action::ClipboardWriteOff, "clipboard_write_off", "Clipboard writes from programs on this machine: off", []),
    info!(Action::ClipboardWriteAsk, "clipboard_write_ask", "Clipboard writes from programs on this machine: ask", []),
    info!(Action::ClipboardWriteAllow, "clipboard_write_allow", "Clipboard writes from programs on this machine: allow", []),
    info!(Action::RenamePane, "rename_pane", "Rename pane", []),
    info!(Action::RenameWorkspace, "rename_workspace", "Rename workspace", []),
    info!(Action::SplitRight, "split_right", "Split pane right", ["alt+shift+plus"]),
    info!(Action::SplitDown, "split_down", "Split pane down", ["alt+shift+minus"]),
    // GTK has no chord for these two (menu only), so neither has one here.
    info!(Action::SplitLeft, "split_left", "Split pane left", []),
    info!(Action::SplitUp, "split_up", "Split pane up", []),
    info!(Action::ClosePane, "close_pane", "Close pane", ["ctrl+shift+w"]),
    differ!(Action::FocusLeft, "focus_left", "Focus pane left", gtk ["alt+left"], windows ["ctrl+alt+left"]),
    differ!(Action::FocusRight, "focus_right", "Focus pane right", gtk ["alt+right"], windows ["ctrl+alt+right"]),
    differ!(Action::FocusUp, "focus_up", "Focus pane up", gtk ["alt+up"], windows ["ctrl+alt+up"]),
    differ!(Action::FocusDown, "focus_down", "Focus pane down", gtk ["alt+down"], windows ["ctrl+alt+down"]),
    differ!(Action::ZoomPane, "zoom_pane", "Toggle pane zoom", gtk [], windows ["ctrl+shift+z"]),
    differ!(Action::ToggleSidebar, "toggle_sidebar", "Toggle sidebar", gtk ["ctrl+alt+b"], windows ["ctrl+shift+b"]),
    info!(Action::CommandPalette, "command_palette", "Command palette", ["ctrl+shift+p"]),
    differ!(Action::NewWorkspace, "new_workspace", "New workspace", gtk ["ctrl+alt+n"], windows ["ctrl+shift+n"]),
    differ!(Action::NextWorkspace, "next_workspace", "Next workspace", gtk ["ctrl+alt+pagedown"], windows ["ctrl+alt+pagedown"]),
    differ!(Action::PrevWorkspace, "prev_workspace", "Previous workspace", gtk ["ctrl+alt+pageup"], windows ["ctrl+alt+pageup"]),
    info!(Action::PairMachine, "pair_machine", "Pair machine...", []),
    info!(Action::NextMachine, "next_machine", "Next machine", []),
    info!(Action::PrevMachine, "prev_machine", "Previous machine", []),
    info!(Action::Search, "search", "Find in terminal", ["ctrl+shift+f"]),
    differ!(Action::SelectAll, "select_all", "Select all", gtk [], windows ["ctrl+shift+a"]),
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
    // GTK: <Control>equal / <Control>plus, <Control>minus, <Control>0.
    info!(Action::ZoomIn, "zoom_in", "Zoom in", ["ctrl+=", "ctrl+shift+="]),
    info!(Action::ZoomOut, "zoom_out", "Zoom out", ["ctrl+-"]),
    info!(Action::ZoomReset, "zoom_reset", "Reset zoom", ["ctrl+0"]),
    info!(Action::Profile(1), "profile_1", "Open profile 1", ["ctrl+shift+1"]),
    info!(Action::Profile(2), "profile_2", "Open profile 2", ["ctrl+shift+2"]),
    info!(Action::Profile(3), "profile_3", "Open profile 3", ["ctrl+shift+3"]),
    info!(Action::Profile(4), "profile_4", "Open profile 4", ["ctrl+shift+4"]),
    info!(Action::Profile(5), "profile_5", "Open profile 5", ["ctrl+shift+5"]),
    info!(Action::Profile(6), "profile_6", "Open profile 6", ["ctrl+shift+6"]),
    info!(Action::Profile(7), "profile_7", "Open profile 7", ["ctrl+shift+7"]),
    info!(Action::Profile(8), "profile_8", "Open profile 8", ["ctrl+shift+8"]),
    info!(Action::Profile(9), "profile_9", "Open profile 9", ["ctrl+shift+9"]),
    // GTK has these in its menu only, so no chord here either.
    info!(Action::OpenConfig, "open_config", "Open configuration file", []),
    info!(Action::ReloadConfig, "reload_config", "Reload configuration", []),
    // GTK: Ctrl+, opens the Appearance sidebar (theme and font).
    differ!(Action::ChooseTheme, "choose_theme", "Choose theme...", gtk ["ctrl+,"], windows []),
    differ!(Action::Quit, "quit", "Quit", gtk ["ctrl+shift+q"], windows []),
    // GTK: win.open-settings is Ctrl+. and win.show-shortcuts is F1 (it also has Ctrl+Shift+P
    // there, which stays the command palette here). F1 is not a key a shell needs.
    differ!(Action::OpenSettings, "open_settings", "Settings...", gtk ["ctrl+."], windows []),
    info!(Action::ShowShortcuts, "show_shortcuts", "Keyboard shortcuts", ["f1"]),
    differ!(Action::PasteCtrlV, "paste_ctrl_v", "Paste (Ctrl+V)", gtk ["ctrl+v"], windows []),
    differ!(Action::CopyOrInterrupt, "copy_or_interrupt", "Copy, else interrupt (Ctrl+C)", gtk ["ctrl+c"], windows []),
    differ!(Action::GotoWorkspace(1), "goto_workspace_1", "Go to workspace 1", gtk ["alt+1"], windows []),
    differ!(Action::GotoWorkspace(2), "goto_workspace_2", "Go to workspace 2", gtk ["alt+2"], windows []),
    differ!(Action::GotoWorkspace(3), "goto_workspace_3", "Go to workspace 3", gtk ["alt+3"], windows []),
    differ!(Action::GotoWorkspace(4), "goto_workspace_4", "Go to workspace 4", gtk ["alt+4"], windows []),
    differ!(Action::GotoWorkspace(5), "goto_workspace_5", "Go to workspace 5", gtk ["alt+5"], windows []),
    differ!(Action::GotoWorkspace(6), "goto_workspace_6", "Go to workspace 6", gtk ["alt+6"], windows []),
    differ!(Action::GotoWorkspace(7), "goto_workspace_7", "Go to workspace 7", gtk ["alt+7"], windows []),
    differ!(Action::GotoWorkspace(8), "goto_workspace_8", "Go to workspace 8", gtk ["alt+8"], windows []),
    differ!(Action::GotoWorkspace(9), "goto_workspace_9", "Go to workspace 9", gtk ["alt+9"], windows []),
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
    preset: KeymapPreset,
    by_chord: HashMap<Chord, Action>,
    by_action: HashMap<Action, Vec<Chord>>,
    /// Config entries that did not parse, for logging.
    pub warnings: Vec<String>,
}

impl Keymap {
    /// The defaults of `preset`, then `overrides` (config key `name` to a list of chords; an
    /// empty list or "none" unbinds the action).
    pub fn new(preset: KeymapPreset, overrides: &HashMap<String, Vec<String>>) -> Self {
        let mut km = Keymap { preset, ..Keymap::default() };
        let mut per_action: Vec<(Action, Vec<String>)> = ACTIONS
            .iter()
            .map(|i| (i.action, i.default_keys(preset).iter().map(|s| s.to_string()).collect()))
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

    /// The defaults of `preset`, no overrides.
    pub fn defaults(preset: KeymapPreset) -> Self {
        Self::new(preset, &HashMap::new())
    }

    /// The defaults of the default preset (`gtk`).
    pub fn default_map() -> Self {
        Self::defaults(KeymapPreset::default())
    }

    pub fn preset(&self) -> KeymapPreset {
        self.preset
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
        let km = Keymap::defaults(KeymapPreset::Windows);
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
        let km = Keymap::new(KeymapPreset::Windows, &o);
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
        let km = Keymap::defaults(KeymapPreset::Windows);
        assert_eq!(km.lookup(&Chord::new(true, false, true, false, "f")), Some(Action::Search));
        assert_eq!(km.lookup(&Chord::new(true, false, true, false, "a")), Some(Action::SelectAll));
        assert!(km.chords(Action::ResetTerminal).is_empty(), "menu-only in GTK: palette only here");
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        assert_eq!(Action::from_name("copy_raw"), Some(Action::CopyRaw));
    }

    /// WP3: GTK's zoom and profile chords. Config and theme actions are menu-only, as in GTK.
    #[test]
    fn wp3_actions_take_gtks_chords_and_move_no_existing_one() {
        let km = Keymap::defaults(KeymapPreset::Windows);
        let c = |ctrl, alt, shift, key: &str| km.lookup(&Chord::new(ctrl, alt, shift, false, key));
        assert_eq!(c(true, false, false, "="), Some(Action::ZoomIn));
        assert_eq!(c(true, false, false, "plus"), Some(Action::ZoomIn));
        assert_eq!(c(true, false, true, "="), Some(Action::ZoomIn));
        assert_eq!(c(true, false, false, "-"), Some(Action::ZoomOut));
        assert_eq!(c(true, false, false, "0"), Some(Action::ZoomReset));
        for n in 1..=9u8 {
            assert_eq!(c(true, false, true, &n.to_string()), Some(Action::Profile(n)));
            // Ctrl+N alone is still "go to tab N".
            assert_eq!(c(true, false, false, &n.to_string()), Some(Action::GotoTab(n)));
        }
        assert_eq!(c(false, true, true, "="), Some(Action::SplitRight));
        for a in [Action::OpenConfig, Action::ReloadConfig, Action::ChooseTheme] {
            assert!(km.chords(a).is_empty());
        }
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        assert_eq!(Action::from_name("profile_3"), Some(Action::Profile(3)));
        assert_eq!(km.shortcut(Action::ZoomIn), "Ctrl+=");
    }

    /// WP2: only the keyboard pane resize has a chord (GTK's Alt+Shift+Arrow, free on Windows).
    /// Every chord that existed before still does what it did.
    #[test]
    fn wp2_actions_take_only_gtks_chord_and_move_no_existing_one() {
        let km = Keymap::defaults(KeymapPreset::Windows);
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

    /// The `gtk` set is GTK's own table (`app.rs` accelerators and the shortcuts window).
    #[test]
    fn the_gtk_set_has_gtks_chords() {
        let km = Keymap::defaults(KeymapPreset::Gtk);
        assert_eq!(km.preset(), KeymapPreset::Gtk);
        assert_eq!(KeymapPreset::default(), KeymapPreset::Gtk);
        let c = |ctrl, alt, shift, key: &str| km.lookup(&Chord::new(ctrl, alt, shift, false, key));
        // Tabs and panes.
        assert_eq!(c(true, false, true, "t"), Some(Action::NewTab));
        assert_eq!(c(true, false, true, "w"), Some(Action::ClosePane));
        assert_eq!(c(false, true, true, "plus"), Some(Action::SplitRight));
        assert_eq!(c(false, true, true, "minus"), Some(Action::SplitDown));
        assert_eq!(c(false, true, false, "left"), Some(Action::FocusLeft));
        assert_eq!(c(false, true, false, "down"), Some(Action::FocusDown));
        assert_eq!(c(false, true, true, "right"), Some(Action::ResizeRight));
        assert_eq!(c(true, false, true, "f"), Some(Action::Search));
        // Workspaces.
        assert_eq!(c(true, true, false, "n"), Some(Action::NewWorkspace));
        assert_eq!(c(true, true, false, "b"), Some(Action::ToggleSidebar));
        assert_eq!(c(true, true, false, "pagedown"), Some(Action::NextWorkspace));
        assert_eq!(c(true, true, false, "pageup"), Some(Action::PrevWorkspace));
        for n in 1..=9u8 {
            assert_eq!(c(false, true, false, &n.to_string()), Some(Action::GotoWorkspace(n)));
            // Ctrl+N is not a tab jump in GTK: the program gets it.
            assert_eq!(c(true, false, false, &n.to_string()), None);
            assert_eq!(c(true, false, true, &n.to_string()), Some(Action::Profile(n)));
        }
        // Help, settings, quit.
        assert_eq!(c(true, false, true, "q"), Some(Action::Quit));
        assert_eq!(c(true, false, false, ","), Some(Action::ChooseTheme));
        // Libadwaita's tab view keys.
        assert_eq!(c(true, false, false, "pagedown"), Some(Action::NextTab));
        assert_eq!(c(true, false, false, "pageup"), Some(Action::PrevTab));
        assert_eq!(c(true, false, false, "tab"), Some(Action::NextTab));
        assert_eq!(c(true, false, true, "pageup"), Some(Action::MoveTabLeft));
        // Windows-only chords are free for the program.
        for (ctrl, alt, shift, key) in [(true, true, false, "left"), (true, false, true, "z"), (true, false, true, "r"), (true, false, true, "a")] {
            assert_eq!(c(ctrl, alt, shift, key), None, "{ctrl} {alt} {shift} {key}");
        }
        // The palette stays on Ctrl+Shift+P (GTK opens its shortcuts list there).
        assert_eq!(c(true, false, true, "p"), Some(Action::CommandPalette));
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        assert_eq!(km.shortcut(Action::ToggleSidebar), "Ctrl+Alt+B");
        assert_eq!(km.shortcut(Action::ChooseTheme), "Ctrl+,");
    }

    #[test]
    fn the_windows_set_is_what_it_was() {
        let km = Keymap::defaults(KeymapPreset::Windows);
        let c = |ctrl, alt, shift, key: &str| km.lookup(&Chord::new(ctrl, alt, shift, false, key));
        assert_eq!(c(true, false, true, "q"), Some(Action::CloseTab));
        assert_eq!(c(true, false, true, "n"), Some(Action::NewWorkspace));
        assert_eq!(c(true, true, false, "left"), Some(Action::FocusLeft));
        assert_eq!(c(true, false, false, "5"), Some(Action::GotoTab(5)));
        assert_eq!(c(true, false, true, "z"), Some(Action::ZoomPane));
        assert_eq!(c(true, false, true, "b"), Some(Action::ToggleSidebar));
        // Next and previous workspace: the same chords as GTK (the pager keys are free in this set).
        assert_eq!(c(true, true, false, "pagedown"), Some(Action::NextWorkspace));
        assert_eq!(c(true, true, false, "pageup"), Some(Action::PrevWorkspace));
        assert_eq!(c(false, true, false, "left"), None);
        assert_eq!(c(false, true, false, "1"), None);
        assert!(km.chords(Action::Quit).is_empty());
        assert!(km.warnings.is_empty());
    }

    #[test]
    fn ctrl_v_and_ctrl_c_are_actions_that_can_be_unbound() {
        let gtk = Keymap::defaults(KeymapPreset::Gtk);
        assert_eq!(gtk.lookup(&Chord::new(true, false, false, false, "v")), Some(Action::PasteCtrlV));
        assert_eq!(gtk.lookup(&Chord::new(true, false, false, false, "c")), Some(Action::CopyOrInterrupt));
        let win = Keymap::defaults(KeymapPreset::Windows);
        assert_eq!(win.lookup(&Chord::new(true, false, false, false, "v")), None);
        assert_eq!(win.lookup(&Chord::new(true, false, false, false, "c")), None);
        let mut o = HashMap::new();
        o.insert("paste_ctrl_v".to_string(), vec!["none".to_string()]);
        o.insert("copy_or_interrupt".to_string(), vec!["none".to_string()]);
        let km = Keymap::new(KeymapPreset::Gtk, &o);
        assert_eq!(km.lookup(&Chord::new(true, false, false, false, "v")), None);
        assert_eq!(km.lookup(&Chord::new(true, false, false, false, "c")), None);
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
    }

    #[test]
    fn no_chord_has_two_owners_in_either_set() {
        for preset in KeymapPreset::ALL {
            let mut seen: HashMap<Chord, &str> = HashMap::new();
            for info in ACTIONS {
                for k in info.default_keys(preset) {
                    let c = Chord::parse(k).unwrap_or_else(|| panic!("{} has a bad chord {k}", info.name));
                    if let Some(other) = seen.insert(c, info.name) {
                        panic!("{preset:?}: {k} is on both {other} and {}", info.name);
                    }
                }
            }
        }
    }

    #[test]
    fn presets_parse_and_user_keys_win_in_both() {
        assert_eq!(KeymapPreset::parse(" GTK "), Some(KeymapPreset::Gtk));
        assert_eq!(KeymapPreset::parse("windows"), Some(KeymapPreset::Windows));
        assert_eq!(KeymapPreset::parse("emacs"), None);
        let mut o = HashMap::new();
        o.insert("focus_left".to_string(), vec!["ctrl+h".to_string()]);
        for p in KeymapPreset::ALL {
            let km = Keymap::new(p, &o);
            assert_eq!(km.lookup(&Chord::new(true, false, false, false, "h")), Some(Action::FocusLeft));
            assert_eq!(km.chords(Action::FocusLeft).len(), 1);
        }
    }
}
