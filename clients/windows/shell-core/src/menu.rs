//! Context menus of the pane, the tab and the workspace: what each lists, when an item is
//! greyed, where the menu sits, how the keyboard moves in it. No UI code: the shell draws the
//! rows and runs the command of the one that was chosen.

use crate::keys::Action;

pub const ROW_H: f32 = 28.0;
pub const SEP_H: f32 = 9.0;
pub const SWATCH_H: f32 = 36.0;
pub const PAD: f32 = 4.0;
pub const MIN_W: f32 = 190.0;
pub const MAX_W: f32 = 360.0;
/// Room kept between a menu and the window edge.
pub const EDGE: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Pane,
    Tab,
    Workspace,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuCmd {
    // Pane.
    Copy,
    Paste,
    SelectAll,
    Find,
    Clear,
    Reset,
    SplitRight,
    SplitDown,
    SplitLeft,
    SplitUp,
    /// The link under the pointer when the menu opened.
    OpenLink(String),
    // Tab.
    TabRename,
    TabDuplicate,
    TabMoveLeft,
    TabMoveRight,
    TabCloseRight,
    TabCloseOthers,
    TabClose,
    // Workspace.
    WsRename,
    WsDuplicate,
    WsMoveUp,
    WsMoveDown,
    WsDelete,
    /// A color of the tab or the workspace the menu belongs to. `None` clears it.
    Color(Option<u32>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Cmd { cmd: MenuCmd, label: String, hint: String, enabled: bool },
    Sep,
    /// The color row: the presets, then "none". `current` is marked.
    Swatches { current: Option<u32> },
}

impl Item {
    fn cmd(cmd: MenuCmd, label: &str, enabled: bool) -> Item {
        Item::Cmd { cmd, label: label.to_string(), hint: String::new(), enabled }
    }

    /// The row can be chosen with the keyboard.
    pub fn selectable(&self) -> bool {
        match self {
            Item::Cmd { enabled, .. } => *enabled,
            Item::Sep => false,
            Item::Swatches { .. } => true,
        }
    }

    pub fn height(&self) -> f32 {
        match self {
            Item::Cmd { .. } => ROW_H,
            Item::Sep => SEP_H,
            Item::Swatches { .. } => SWATCH_H,
        }
    }
}

/// The shortcut a command shows, when it has an action.
pub fn action_of(cmd: &MenuCmd) -> Option<Action> {
    Some(match cmd {
        MenuCmd::Paste => return None,
        MenuCmd::SelectAll => Action::SelectAll,
        MenuCmd::Find => Action::Search,
        MenuCmd::Clear => Action::ClearTerminal,
        MenuCmd::Reset => Action::ResetTerminal,
        MenuCmd::SplitRight => Action::SplitRight,
        MenuCmd::SplitDown => Action::SplitDown,
        MenuCmd::SplitLeft => Action::SplitLeft,
        MenuCmd::SplitUp => Action::SplitUp,
        MenuCmd::TabRename => Action::RenameTab,
        MenuCmd::TabDuplicate => Action::DuplicateTab,
        MenuCmd::TabMoveLeft => Action::MoveTabLeft,
        MenuCmd::TabMoveRight => Action::MoveTabRight,
        MenuCmd::TabCloseRight => Action::CloseTabsRight,
        MenuCmd::TabCloseOthers => Action::CloseOtherTabs,
        MenuCmd::TabClose => Action::CloseTab,
        MenuCmd::WsRename => Action::RenameWorkspace,
        MenuCmd::WsDuplicate => Action::DuplicateWorkspace,
        MenuCmd::WsMoveUp => Action::MoveWorkspaceUp,
        MenuCmd::WsMoveDown => Action::MoveWorkspaceDown,
        MenuCmd::WsDelete => Action::DeleteWorkspace,
        MenuCmd::Copy | MenuCmd::OpenLink(_) | MenuCmd::Color(_) => return None,
    })
}

/// "Open <url>", the url cut to a length that fits a menu.
pub fn link_label(url: &str) -> String {
    let clean = crate::notify::sanitize(url, 64);
    format!("Open {clean}")
}

pub struct PaneMenuCtx {
    pub has_selection: bool,
    /// The http(s) link under the pointer.
    pub url: Option<String>,
    /// The machine's server can place a split first (left or up). Without it the two rows
    /// are left out: an older server would split on the other side without saying so.
    pub split_before: bool,
}

/// The four split rows. Left and Up only when the machine supports them.
fn split_rows(split_before: bool) -> Vec<Item> {
    let mut v = vec![Item::cmd(MenuCmd::SplitRight, "Split Right", true), Item::cmd(MenuCmd::SplitDown, "Split Down", true)];
    if split_before {
        v.push(Item::cmd(MenuCmd::SplitLeft, "Split Left", true));
        v.push(Item::cmd(MenuCmd::SplitUp, "Split Up", true));
    }
    v
}

/// Copy, Paste, Select All, Find, Clear, Reset, Split, then Open <url> (GTK's menu has the
/// first four and the link, the rest is new). Copy is greyed with nothing selected.
pub fn pane_menu(c: &PaneMenuCtx) -> Vec<Item> {
    let mut v = vec![
        Item::cmd(MenuCmd::Copy, "Copy", c.has_selection),
        Item::cmd(MenuCmd::Paste, "Paste", true),
        Item::Sep,
        Item::cmd(MenuCmd::SelectAll, "Select All", true),
        Item::cmd(MenuCmd::Find, "Find", true),
        Item::Sep,
        Item::cmd(MenuCmd::Clear, "Clear", true),
        Item::cmd(MenuCmd::Reset, "Reset", true),
        Item::Sep,
    ];
    v.extend(split_rows(c.split_before));
    if let Some(u) = &c.url {
        v.push(Item::Sep);
        v.push(Item::cmd(MenuCmd::OpenLink(u.clone()), &link_label(u), true));
    }
    v
}

pub struct TabMenuCtx {
    pub index: usize,
    pub count: usize,
    pub color: Option<u32>,
    /// See `PaneMenuCtx::split_before`.
    pub split_before: bool,
}

/// GTK's tab menu: color, rename, duplicate, split, move, find, then the three closes.
/// ("Move to New Window" and the Export stub wait for the multi-window work.)
pub fn tab_menu(c: &TabMenuCtx) -> Vec<Item> {
    let mut v = vec![
        Item::Swatches { current: c.color },
        Item::Sep,
        Item::cmd(MenuCmd::TabRename, "Rename Tab", true),
        Item::cmd(MenuCmd::TabDuplicate, "Duplicate Tab", true),
    ];
    v.extend(split_rows(c.split_before));
    v.extend([
        Item::Sep,
        Item::cmd(MenuCmd::TabMoveLeft, "Move Left", c.index > 0),
        Item::cmd(MenuCmd::TabMoveRight, "Move Right", c.index + 1 < c.count),
        Item::cmd(MenuCmd::Find, "Find", true),
        Item::Sep,
        Item::cmd(MenuCmd::TabCloseRight, "Close Tabs to the Right", c.index + 1 < c.count),
        Item::cmd(MenuCmd::TabCloseOthers, "Close Other Tabs", c.count > 1),
        Item::cmd(MenuCmd::TabClose, "Close Tab", true),
    ]);
    v
}

pub struct WorkspaceMenuCtx {
    pub index: usize,
    pub count: usize,
    pub color: Option<u32>,
}

/// Color, rename, duplicate, move up and down, delete. Delete is greyed for the last workspace.
pub fn workspace_menu(c: &WorkspaceMenuCtx) -> Vec<Item> {
    vec![
        Item::Swatches { current: c.color },
        Item::Sep,
        Item::cmd(MenuCmd::WsRename, "Rename Workspace", true),
        Item::cmd(MenuCmd::WsDuplicate, "Duplicate Workspace", true),
        Item::Sep,
        Item::cmd(MenuCmd::WsMoveUp, "Move Up", c.index > 0),
        Item::cmd(MenuCmd::WsMoveDown, "Move Down", c.index + 1 < c.count),
        Item::Sep,
        Item::cmd(MenuCmd::WsDelete, "Delete Workspace", crate::ops::can_delete_workspace(c.count)),
    ]
}

/// An open menu.
#[derive(Clone, Debug)]
pub struct Menu {
    pub kind: MenuKind,
    pub items: Vec<Item>,
    /// The row the keyboard is on.
    pub selected: Option<usize>,
    /// In a color row: the swatch the keyboard is on (0..PRESETS.len(), the last is "none").
    pub swatch: usize,
    /// Top-left corner and size, in window px.
    pub pos: (f32, f32),
    pub size: (f32, f32),
}

/// Number of swatches in the color row: the presets and "none".
pub const SWATCHES: usize = crate::colors::PRESETS.len() + 1;

impl Menu {
    /// A menu that opens at `anchor` in a window of `viewport` px. It flips to the other side
    /// of the pointer when it would run out of the window, and stays inside as far as it fits.
    pub fn open(kind: MenuKind, items: Vec<Item>, anchor: (f32, f32), viewport: (f32, f32)) -> Self {
        let size = Self::measure(&items);
        let pos = place(size, anchor, viewport);
        Menu { kind, items, selected: None, swatch: 0, pos, size }
    }

    /// Width from the longest label and hint (a Segoe UI 12.5 px character is about 7 px),
    /// height from the rows.
    pub fn measure(items: &[Item]) -> (f32, f32) {
        let mut w: f32 = MIN_W;
        for it in items {
            if let Item::Cmd { label, hint, .. } = it {
                let chars = label.chars().count() as f32 * 7.0 + hint.chars().count() as f32 * 6.5;
                let gap = if hint.is_empty() { 0.0 } else { 28.0 };
                w = w.max(chars + gap + 2.0 * 12.0 + 2.0 * PAD);
            }
        }
        let h: f32 = items.iter().map(Item::height).sum::<f32>() + 2.0 * PAD;
        (w.min(MAX_W), h)
    }

    /// Top of row `i`, relative to the menu.
    pub fn row_top(&self, i: usize) -> f32 {
        PAD + self.items[..i.min(self.items.len())].iter().map(Item::height).sum::<f32>()
    }

    /// The row at `y` (relative to the menu), if any.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        let mut top = PAD;
        for (i, it) in self.items.iter().enumerate() {
            if y >= top && y < top + it.height() {
                return Some(i);
            }
            top += it.height();
        }
        None
    }

    /// Up (`-1`) or Down (`1`): the next row that can be chosen, wrapping.
    pub fn move_selection(&mut self, delta: i32) {
        let n = self.items.len();
        if n == 0 || !self.items.iter().any(Item::selectable) {
            return;
        }
        let mut i = match (self.selected, delta >= 0) {
            (Some(i), _) => i,
            (None, true) => n - 1,
            (None, false) => 0,
        };
        for _ in 0..n {
            i = if delta >= 0 { (i + 1) % n } else { (i + n - 1) % n };
            if self.items[i].selectable() {
                self.selected = Some(i);
                return;
            }
        }
    }

    /// Left (`-1`) or Right (`1`) in a color row.
    pub fn move_swatch(&mut self, delta: i32) {
        if matches!(self.selected.and_then(|i| self.items.get(i)), Some(Item::Swatches { .. })) {
            self.swatch = (self.swatch as i32 + delta).rem_euclid(SWATCHES as i32) as usize;
        }
    }

    /// The color of swatch `i`: a preset, or `None` for the last ("none").
    pub fn swatch_color(i: usize) -> Option<u32> {
        crate::colors::PRESETS.get(i).map(|p| p.1)
    }

    /// The command of the row the keyboard is on (Enter).
    pub fn activate(&self) -> Option<MenuCmd> {
        match self.items.get(self.selected?)? {
            Item::Cmd { cmd, enabled: true, .. } => Some(cmd.clone()),
            Item::Swatches { .. } => Some(MenuCmd::Color(Self::swatch_color(self.swatch))),
            _ => None,
        }
    }

    /// The command of the row a click at `y` landed on (not a swatch: the shell reads those).
    pub fn command_at(&self, y: f32) -> Option<MenuCmd> {
        match self.items.get(self.row_at(y)?)? {
            Item::Cmd { cmd, enabled: true, .. } => Some(cmd.clone()),
            _ => None,
        }
    }
}

/// Top-left corner of a menu of `size` opened at `anchor`.
pub fn place(size: (f32, f32), anchor: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let fit = |a: f32, s: f32, v: f32| {
        let mut p = a;
        if p + s + EDGE > v {
            p = a - s; // the other side of the pointer
        }
        p.min(v - s - EDGE).max(EDGE)
    };
    (fit(anchor.0, size.0, viewport.0), fit(anchor.1, size.1, viewport.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                Item::Cmd { label, .. } => label.clone(),
                Item::Sep => "-".into(),
                Item::Swatches { .. } => "colors".into(),
            })
            .collect()
    }

    fn enabled(items: &[Item], name: &str) -> bool {
        items
            .iter()
            .find_map(|i| match i {
                Item::Cmd { label, enabled, .. } if label == name => Some(*enabled),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn the_pane_menu_has_gtks_items_and_the_link_last() {
        let m = pane_menu(&PaneMenuCtx { has_selection: false, url: None, split_before: false });
        assert_eq!(
            labels(&m),
            ["Copy", "Paste", "-", "Select All", "Find", "-", "Clear", "Reset", "-", "Split Right", "Split Down"]
        );
        assert!(!enabled(&m, "Copy"), "nothing selected");
        assert!(enabled(&pane_menu(&PaneMenuCtx { has_selection: true, url: None, split_before: false }), "Copy"));
        let m = pane_menu(&PaneMenuCtx { has_selection: true, url: Some("https://example.com/a".into()), split_before: false });
        assert_eq!(labels(&m).last().unwrap(), "Open https://example.com/a");
        assert!(matches!(&m[m.len() - 1], Item::Cmd { cmd: MenuCmd::OpenLink(u), .. } if u == "https://example.com/a"));
        assert_eq!(m[m.len() - 2], Item::Sep);
        // A long link is cut and cannot carry control or bidi characters into the label.
        let long = format!("https://example.com/{}\u{202e}x", "a".repeat(200));
        let l = link_label(&long);
        assert!(l.chars().count() <= 70 && !l.contains('\u{202e}'), "{l}");
    }

    #[test]
    fn split_left_and_up_are_listed_only_for_a_server_that_can_place_them() {
        let names = |before| labels(&pane_menu(&PaneMenuCtx { has_selection: false, url: None, split_before: before }));
        assert!(!names(false).iter().any(|l| l == "Split Left" || l == "Split Up"));
        let n = names(true);
        assert_eq!(&n[n.len() - 4..], ["Split Right", "Split Down", "Split Left", "Split Up"]);
        let tab = |before| labels(&tab_menu(&TabMenuCtx { index: 0, count: 2, color: None, split_before: before }));
        assert!(!tab(false).iter().any(|l| l == "Split Left"));
        assert!(tab(true).iter().any(|l| l == "Split Left") && tab(true).iter().any(|l| l == "Split Up"));
    }

    #[test]
    fn the_tab_menu_greys_what_cannot_happen() {
        let c = |index, count| tab_menu(&TabMenuCtx { index, count, color: None, split_before: false });
        let only = c(0, 1);
        assert!(!enabled(&only, "Move Left") && !enabled(&only, "Move Right"));
        assert!(!enabled(&only, "Close Tabs to the Right") && !enabled(&only, "Close Other Tabs"));
        assert!(enabled(&only, "Close Tab"));
        let mid = c(1, 3);
        assert!(enabled(&mid, "Move Left") && enabled(&mid, "Move Right"));
        assert!(enabled(&mid, "Close Tabs to the Right") && enabled(&mid, "Close Other Tabs"));
        let last = c(2, 3);
        assert!(!enabled(&last, "Move Right") && !enabled(&last, "Close Tabs to the Right"));
        assert!(matches!(mid[0], Item::Swatches { current: None }));
    }

    #[test]
    fn the_workspace_menu_will_not_delete_the_last_one() {
        let m = |index, count| workspace_menu(&WorkspaceMenuCtx { index, count, color: Some(0x3987e4) });
        assert!(!enabled(&m(0, 1), "Delete Workspace"));
        assert!(enabled(&m(0, 2), "Delete Workspace"));
        assert!(!enabled(&m(0, 3), "Move Up") && enabled(&m(0, 3), "Move Down"));
        assert!(enabled(&m(2, 3), "Move Up") && !enabled(&m(2, 3), "Move Down"));
        assert!(matches!(m(0, 1)[0], Item::Swatches { current: Some(0x3987e4) }));
    }

    #[test]
    fn a_menu_stays_inside_the_window() {
        let size = (200.0, 300.0);
        let vp = (1000.0, 700.0);
        assert_eq!(place(size, (100.0, 100.0), vp), (100.0, 100.0));
        // Near the right edge: opens to the left of the pointer.
        assert_eq!(place(size, (900.0, 100.0), vp), (700.0, 100.0));
        // Near the bottom: opens above the pointer.
        assert_eq!(place(size, (100.0, 600.0), vp), (100.0, 300.0));
        // A window too small for it: pinned to the top-left margin, never negative.
        let (x, y) = place(size, (50.0, 50.0), (150.0, 250.0));
        assert!(x >= EDGE && y >= EDGE);
    }

    #[test]
    fn the_keyboard_skips_rows_that_cannot_be_chosen_and_wraps() {
        let items = pane_menu(&PaneMenuCtx { has_selection: false, url: None, split_before: false });
        let mut m = Menu::open(MenuKind::Pane, items, (10.0, 10.0), (1000.0, 700.0));
        m.move_selection(1);
        assert_eq!(m.selected, Some(1), "Copy is greyed, Paste is first");
        m.move_selection(-1);
        assert_eq!(m.selected, Some(10), "up from the first wraps to the last row");
        m.move_selection(1);
        assert_eq!(m.selected, Some(1));
        assert_eq!(m.activate(), Some(MenuCmd::Paste));
        let mut m = Menu::open(MenuKind::Pane, pane_menu(&PaneMenuCtx { has_selection: true, url: None, split_before: false }), (0.0, 0.0), (900.0, 900.0));
        m.move_selection(-1);
        assert_eq!(m.selected, Some(10));
        assert_eq!(m.activate(), Some(MenuCmd::SplitDown));
    }

    #[test]
    fn the_color_row_takes_left_right_and_enter() {
        let items = tab_menu(&TabMenuCtx { index: 0, count: 2, color: None, split_before: false });
        let mut m = Menu::open(MenuKind::Tab, items, (5.0, 5.0), (900.0, 900.0));
        m.move_selection(1);
        assert_eq!(m.selected, Some(0));
        assert_eq!(m.activate(), Some(MenuCmd::Color(Some(crate::colors::PRESETS[0].1))));
        m.move_swatch(1);
        m.move_swatch(1);
        assert_eq!(m.activate(), Some(MenuCmd::Color(Some(crate::colors::PRESETS[2].1))));
        m.move_swatch(-3);
        assert_eq!(m.swatch, SWATCHES - 1);
        assert_eq!(m.activate(), Some(MenuCmd::Color(None)), "the last swatch clears the color");
        m.move_swatch(1);
        assert_eq!(m.swatch, 0);
        // Left and right do nothing on other rows.
        m.move_selection(1);
        let s = m.swatch;
        m.move_swatch(1);
        assert_eq!(m.swatch, s);
    }

    #[test]
    fn rows_and_clicks_line_up() {
        let items = workspace_menu(&WorkspaceMenuCtx { index: 0, count: 2, color: None });
        let m = Menu::open(MenuKind::Workspace, items, (0.0, 0.0), (900.0, 900.0));
        // Row 0 is the color row, row 1 a separator, row 2 "Rename Workspace".
        assert_eq!(m.row_at(PAD + 1.0), Some(0));
        assert_eq!(m.row_at(m.row_top(1) + 1.0), Some(1));
        assert_eq!(m.command_at(m.row_top(2) + 1.0), Some(MenuCmd::WsRename));
        assert_eq!(m.command_at(m.row_top(1) + 1.0), None, "a separator is not a command");
        // "Move Up" at index 0 is greyed: a click does nothing.
        assert_eq!(m.command_at(m.row_top(5) + 1.0), None);
        assert_eq!(m.row_at(-1.0), None);
        assert_eq!(m.row_at(m.size.1), None);
        let total: f32 = m.items.iter().map(Item::height).sum::<f32>() + 2.0 * PAD;
        assert_eq!(m.size.1, total);
        assert!(m.size.0 >= MIN_W && m.size.0 <= MAX_W);
    }

    #[test]
    fn every_command_with_an_action_is_a_known_action() {
        let all = [
            MenuCmd::SelectAll,
            MenuCmd::Find,
            MenuCmd::Clear,
            MenuCmd::Reset,
            MenuCmd::SplitRight,
            MenuCmd::SplitDown,
            MenuCmd::SplitLeft,
            MenuCmd::SplitUp,
            MenuCmd::TabRename,
            MenuCmd::TabDuplicate,
            MenuCmd::TabMoveLeft,
            MenuCmd::TabMoveRight,
            MenuCmd::TabCloseRight,
            MenuCmd::TabCloseOthers,
            MenuCmd::TabClose,
            MenuCmd::WsRename,
            MenuCmd::WsDuplicate,
            MenuCmd::WsMoveUp,
            MenuCmd::WsMoveDown,
            MenuCmd::WsDelete,
        ];
        for c in &all {
            assert!(action_of(c).is_some(), "{c:?}");
        }
        assert!(action_of(&MenuCmd::Copy).is_none() && action_of(&MenuCmd::Paste).is_none());
    }
}
