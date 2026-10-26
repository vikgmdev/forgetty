//! Context menus (pane, tab, workspace), the delete confirmation, tab and workspace operations,
//! keyboard pane resize, and the pane attention ring. The rules live in `forgetty_shell_core`
//! (`menu`, `ops`, `colors`, `layout::resize_by_key`); this file wires them to the backend and
//! draws them.

use std::time::Duration;

use forgetty_shell_core::layout::Side;
use forgetty_shell_core::menu::{self, Item, Menu, MenuCmd, MenuKind, NewTabMenuCtx, PaneMenuCtx, TabMenuCtx, WorkspaceMenuCtx};
use forgetty_shell_core::{colors, ops, NewTab, PaneId, PaneStatus, SplitDir, SplitRatio, Uuid};
use gpui::{
    div, ease_out_quint, prelude::*, px, Animation, AnimationExt, Context, Div, Hsla, Keystroke, MouseButton,
    SharedString, Window,
};

use crate::shell::*;
use crate::view::hsla;

fn alpha(hex: u32, a: f32) -> Hsla {
    let mut c = hsla(hex);
    c.a = a;
    c
}

/// Amber of the attention ring and the tab badge.
pub const ATTENTION: u32 = 0xffc700;
/// Pixels one keyboard resize moves a divider (GTK: 20).
pub const RESIZE_STEP: f32 = 20.0;

/// What an open menu acts on.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuTarget {
    Pane(PaneId),
    Tab(Uuid),
    Workspace(Uuid),
    /// The new-tab drop-down. The splits act on the active pane.
    NewTab,
}

pub struct OpenMenu {
    pub menu: Menu,
    pub target: MenuTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmAct {
    /// Delete the workspace with this id.
    DeleteWorkspace(Uuid),
    /// End the app while local shells are open.
    Quit,
}

/// A question that waits for y (do it) or Enter/Esc/click on Cancel (do not).
pub struct Confirm {
    pub text: String,
    pub act: ConfirmAct,
}

impl AppShell {
    // ---- opening ------------------------------------------------------------------

    fn fill_hints(&self, items: &mut [Item]) {
        for it in items {
            if let Item::Cmd { cmd, hint, .. } = it {
                let h = match cmd {
                    // GTK shows Ctrl+C and Ctrl+V; both sets also take the Shift forms.
                    MenuCmd::Copy if self.keymap.preset() == forgetty_shell_core::keys::KeymapPreset::Gtk => "Ctrl+C".to_string(),
                    MenuCmd::Paste if self.keymap.preset() == forgetty_shell_core::keys::KeymapPreset::Gtk => "Ctrl+V".to_string(),
                    MenuCmd::Copy => "Ctrl+Shift+C".to_string(),
                    MenuCmd::Paste => "Ctrl+Shift+V".to_string(),
                    c => menu::action_of(c).map(|a| self.keymap.shortcut(a)).unwrap_or_default(),
                };
                *hint = h;
            }
        }
    }

    fn open_menu(&mut self, kind: MenuKind, mut items: Vec<Item>, target: MenuTarget, anchor: (f32, f32), keys: bool, cx: &mut Context<Self>) {
        self.fill_hints(&mut items);
        let mut m = Menu::open(kind, items, anchor, self.viewport);
        if keys {
            m.move_selection(1);
        }
        self.palette = None;
        self.rename = None;
        self.confirm = None;
        self.menu = Some(OpenMenu { menu: m, target });
        cx.notify();
    }

    /// The pane's menu. `url` is the http(s) link under the pointer. `keys` opens it for the
    /// keyboard (the first row is selected).
    pub fn open_pane_menu(&mut self, pane: PaneId, pos: (f32, f32), url: Option<String>, has_selection: bool, keys: bool, cx: &mut Context<Self>) {
        let items = menu::pane_menu(&PaneMenuCtx { has_selection, url, split_before: self.backend.supports_split_before() });
        self.open_menu(MenuKind::Pane, items, MenuTarget::Pane(pane), pos, keys, cx);
    }

    pub fn open_tab_menu(&mut self, tab: Uuid, pos: (f32, f32), keys: bool, cx: &mut Context<Self>) {
        let Some((w, t)) = self.layout.find_tab(tab) else { return };
        let count = self.layout.workspaces[w].tabs.len();
        let items = menu::tab_menu(&TabMenuCtx { index: t, count, color: self.tab_colors.get(&tab).copied(), split_before: self.backend.supports_split_before() });
        self.open_menu(MenuKind::Tab, items, MenuTarget::Tab(tab), pos, keys, cx);
    }

    pub fn open_workspace_menu(&mut self, idx: usize, pos: (f32, f32), keys: bool, cx: &mut Context<Self>) {
        let Some(ws) = self.layout.workspaces.get(idx) else { return };
        let items = menu::workspace_menu(&WorkspaceMenuCtx {
            index: idx,
            count: self.layout.workspaces.len(),
            color: colors::from_wire(ws.color.as_deref()),
        });
        self.open_menu(MenuKind::Workspace, items, MenuTarget::Workspace(ws.id), pos, keys, cx);
    }

    /// The new-tab drop-down, under the arrow. `keys` selects the first row.
    pub fn open_new_tab_menu(&mut self, keys: bool, cx: &mut Context<Self>) {
        let (left, tw) = self.tab_strip();
        let n = self.layout.active_workspace().map(|w| w.tabs.len()).unwrap_or(0) as f32;
        let at = (left + n * tw + PLUS_W, TITLEBAR_H);
        // Profiles start programs on this PC: a remote machine's menu lists none.
        let profiles = if self.machine.kind == forgetty_shell_core::MachineKind::Local {
            self.cfg.profiles.iter().map(|p| p.name.clone()).collect()
        } else {
            Vec::new()
        };
        let items = menu::new_tab_menu(&NewTabMenuCtx {
            profiles,
            split_before: self.backend.supports_split_before(),
            has_pane: self.active_pane().is_some(),
        });
        self.open_menu(MenuKind::NewTab, items, MenuTarget::NewTab, at, keys, cx);
    }

    /// The "Pane menu" action: the active pane's menu at its top-left corner.
    pub fn open_active_pane_menu(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.active_pane() else { return };
        let (ox, oy) = self.content_origin();
        let at = self
            .active_geometry()
            .and_then(|(_, g)| g.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| (r.x + ox + 24.0, r.y + oy + 24.0)))
            .unwrap_or((ox + 24.0, oy + 24.0));
        let (has_sel, url) = match self.views.get(&pane) {
            Some(v) => (v.read(cx).has_selection(), None),
            None => (false, None),
        };
        self.open_pane_menu(pane, at, url, has_sel, true, cx);
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            // The keyboard goes back to the terminal.
            self.request_sync(cx);
        }
    }

    // ---- keyboard -----------------------------------------------------------------

    /// A key while the menu is open. Always consumed.
    pub fn menu_key(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.menu.as_mut() else { return };
        match ks.key.as_str() {
            "escape" => return self.close_menu(cx),
            "up" => open.menu.move_selection(-1),
            "down" => open.menu.move_selection(1),
            "left" => open.menu.move_swatch(-1),
            "right" => open.menu.move_swatch(1),
            "enter" | "space" => {
                if let Some(cmd) = open.menu.activate() {
                    return self.run_menu_cmd(cmd, window, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Run the chosen command on the menu's target and close the menu.
    pub fn run_menu_cmd(&mut self, cmd: MenuCmd, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.menu.take() else { return };
        self.request_sync(cx);
        match open.target {
            MenuTarget::Pane(pane) => self.run_pane_cmd(pane, cmd, window, cx),
            MenuTarget::Tab(tab) => self.run_tab_cmd(tab, cmd, cx),
            MenuTarget::Workspace(id) => self.run_workspace_cmd(id, cmd, cx),
            MenuTarget::NewTab => self.run_new_tab_cmd(cmd, cx),
        }
        cx.notify();
    }

    fn run_pane_cmd(&mut self, pane: PaneId, cmd: MenuCmd, window: &mut Window, cx: &mut Context<Self>) {
        // The commands act on the pane that was clicked.
        self.focus_pane(pane, window, cx);
        let Some(view) = self.views.get(&pane).cloned() else { return };
        match cmd {
            MenuCmd::Copy => view.update(cx, |v, cx| v.copy_selection(cx)),
            MenuCmd::Paste => view.update(cx, |v, cx| v.paste(cx)),
            MenuCmd::SelectAll => view.update(cx, |v, cx| v.select_all(cx)),
            MenuCmd::Find => view.update(cx, |v, cx| {
                if v.search.is_none() {
                    v.toggle_search(cx)
                }
            }),
            MenuCmd::Clear => view.update(cx, |v, cx| v.clear_terminal(cx)),
            MenuCmd::Reset => view.update(cx, |v, cx| v.reset_terminal(cx)),
            MenuCmd::OpenLink(url) => view.update(cx, |v, cx| v.open_link(&url, cx)),
            MenuCmd::SplitRight => self.split(SplitDir::Horizontal, cx),
            MenuCmd::SplitDown => self.split(SplitDir::Vertical, cx),
            MenuCmd::SplitLeft => self.split_before(SplitDir::Horizontal, cx),
            MenuCmd::SplitUp => self.split_before(SplitDir::Vertical, cx),
            _ => {}
        }
    }

    fn run_tab_cmd(&mut self, tab: Uuid, cmd: MenuCmd, cx: &mut Context<Self>) {
        match cmd {
            MenuCmd::TabRename => self.begin_rename(tab, cx),
            MenuCmd::TabDuplicate => self.duplicate_tab(tab, cx),
            MenuCmd::SplitRight | MenuCmd::SplitDown | MenuCmd::SplitLeft | MenuCmd::SplitUp => {
                self.focus_tab(tab, cx);
                let (dir, before) = match cmd {
                    MenuCmd::SplitRight => (SplitDir::Horizontal, false),
                    MenuCmd::SplitDown => (SplitDir::Vertical, false),
                    MenuCmd::SplitLeft => (SplitDir::Horizontal, true),
                    _ => (SplitDir::Vertical, true),
                };
                self.split_in(tab, dir, before, cx);
            }
            MenuCmd::TabMoveLeft => self.move_tab_id(tab, -1, cx),
            MenuCmd::TabMoveRight => self.move_tab_id(tab, 1, cx),
            MenuCmd::Find => {
                self.focus_tab(tab, cx);
                let view = self
                    .layout
                    .find_tab(tab)
                    .map(|(w, t)| self.active_pane_of(&self.layout.workspaces[w].tabs[t]))
                    .and_then(|p| self.views.get(&p).cloned());
                if let Some(v) = view {
                    v.update(cx, |v, cx| {
                        if v.search.is_none() {
                            v.toggle_search(cx)
                        }
                    });
                }
            }
            MenuCmd::TabCloseRight => self.close_tabs_right(tab, cx),
            MenuCmd::TabCloseOthers => self.close_other_tabs(tab, cx),
            MenuCmd::TabClose => self.close_tab(tab, cx),
            MenuCmd::Color(c) => self.set_tab_color(tab, c, cx),
            _ => {}
        }
    }

    fn run_new_tab_cmd(&mut self, cmd: MenuCmd, cx: &mut Context<Self>) {
        match cmd {
            MenuCmd::NewTab => self.new_tab(cx),
            MenuCmd::Profile(n) => self.open_profile(n, cx),
            MenuCmd::PairMachine => self.open_pair(cx),
            MenuCmd::SplitRight => self.split(SplitDir::Horizontal, cx),
            MenuCmd::SplitDown => self.split(SplitDir::Vertical, cx),
            MenuCmd::SplitLeft => self.split_before(SplitDir::Horizontal, cx),
            MenuCmd::SplitUp => self.split_before(SplitDir::Vertical, cx),
            _ => {}
        }
    }

    fn run_workspace_cmd(&mut self, id: Uuid, cmd: MenuCmd, cx: &mut Context<Self>) {
        // The menu names the workspace by id. Take its position now: the layout may have
        // changed while the menu was open.
        let Some(idx) = self.layout.workspaces.iter().position(|w| w.id == id) else { return };
        match cmd {
            MenuCmd::WsRename => self.begin_rename_workspace(idx, cx),
            MenuCmd::WsDuplicate => self.duplicate_workspace(idx, cx),
            MenuCmd::WsMoveUp => self.move_workspace(idx, true, cx),
            MenuCmd::WsMoveDown => self.move_workspace(idx, false, cx),
            MenuCmd::WsDelete => self.delete_workspace(idx, cx),
            MenuCmd::Color(c) => self.set_workspace_color(idx, c, cx),
            _ => {}
        }
    }

    // ---- tab operations -------------------------------------------------------------

    pub fn set_tab_color(&mut self, tab: Uuid, color: Option<u32>, cx: &mut Context<Self>) {
        match color {
            Some(c) => {
                self.tab_colors.insert(tab, c);
            }
            None => {
                self.tab_colors.remove(&tab);
            }
        }
        cx.notify();
    }

    /// A new tab in the directory of `tab`'s active pane, next to `tab`.
    pub fn duplicate_tab(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let Some((w, t)) = self.layout.find_tab(tab) else { return };
        let ws = &self.layout.workspaces[w];
        let (count, src) = (ws.tabs.len(), self.active_pane_of(&ws.tabs[t]));
        let r = self.content_rect();
        let (rows, cols) = self.grid_for(r.w, r.h);
        let req = NewTab { workspace_idx: w, rows, cols, inherit_cwd_from: Some(src), ..Default::default() };
        let fut = self.backend.new_tab(req);
        self.call(cx, fut, move |s, reply, cx| {
            s.local_active.insert(reply.tab_id, reply.pane_id);
            // The server appends. The copy goes right after its source.
            if t + 1 < count {
                let mv = s.backend.move_tab(reply.tab_id, t + 1);
                s.call(cx, mv, |_, _, _| {});
            }
            cx.notify();
        });
    }

    fn tab_ids_of(&self, tab: Uuid) -> Option<(Vec<Uuid>, usize)> {
        let (w, t) = self.layout.find_tab(tab)?;
        Some((self.layout.workspaces[w].tabs.iter().map(|x| x.id).collect(), t))
    }

    pub fn close_tabs_right(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let Some((ids, i)) = self.tab_ids_of(tab) else { return };
        for id in ops::tabs_right_of(&ids, i) {
            self.close_tab(id, cx);
        }
    }

    pub fn close_other_tabs(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let Some((ids, i)) = self.tab_ids_of(tab) else { return };
        for id in ops::tabs_other_than(&ids, i) {
            self.close_tab(id, cx);
        }
    }

    pub fn move_tab_id(&mut self, tab: Uuid, delta: isize, cx: &mut Context<Self>) {
        let Some((w, t)) = self.layout.find_tab(tab) else { return };
        let to = t as isize + delta;
        if to < 0 || to >= self.layout.workspaces[w].tabs.len() as isize {
            return;
        }
        let fut = self.backend.move_tab(tab, to as usize);
        self.call(cx, fut, |_, _, _| {});
    }

    // ---- workspace operations ---------------------------------------------------------

    pub fn set_workspace_color(&mut self, idx: usize, color: Option<u32>, cx: &mut Context<Self>) {
        if idx >= self.layout.workspaces.len() {
            return;
        }
        let id = self.layout.workspaces[idx].id;
        let fut = self.backend.set_workspace_color(id, colors::wire_color(color));
        self.call(cx, fut, |_, _, _| {});
    }

    /// A copy of the workspace with fresh shells, right after it. The copy opens.
    pub fn duplicate_workspace(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.layout.workspaces.len() {
            return;
        }
        let r = self.content_rect();
        let (rows, cols) = self.grid_for(r.w, r.h);
        let id = self.layout.workspaces[idx].id;
        let fut = self.backend.duplicate_workspace(id, None, rows, cols);
        self.call(cx, fut, |s, reply, cx| {
            for (tab, pane) in &reply.tabs {
                s.local_active.insert(*tab, *pane);
            }
            s.set_workspace(reply.workspace_idx, cx);
        });
    }

    pub fn move_workspace(&mut self, idx: usize, up: bool, cx: &mut Context<Self>) {
        let Some((from, to)) = ops::workspace_swap(idx, self.layout.workspaces.len(), up) else { return };
        let (Some(a), Some(b)) = (self.layout.workspaces.get(from), self.layout.workspaces.get(to)) else { return };
        let fut = self.backend.move_workspace(a.id, b.id);
        self.call(cx, fut, |_, _, _| {});
    }

    /// What deleting the workspace at `idx` would end.
    fn delete_info(&self, idx: usize) -> Option<ops::DeleteInfo> {
        let ws = self.layout.workspaces.get(idx)?;
        let panes: Vec<PaneId> = ws.tabs.iter().flat_map(|t| t.pane_tree.leaves()).collect();
        let busy = panes
            .iter()
            .filter(|p| self.meta.get(p).is_some_and(|m| matches!(m.status, PaneStatus::Working | PaneStatus::Waiting)))
            .count();
        Some(ops::DeleteInfo {
            tabs: ws.tabs.len(),
            panes: panes.len(),
            busy,
            remote: self.machine.kind == forgetty_shell_core::MachineKind::Remote,
        })
    }

    /// Delete a workspace. It asks first when it holds more than one tab or pane, when a
    /// program runs in it, and always on a remote machine. The last workspace stays.
    pub fn delete_workspace(&mut self, idx: usize, cx: &mut Context<Self>) {
        if !ops::can_delete_workspace(self.layout.workspaces.len()) {
            return;
        }
        let Some(info) = self.delete_info(idx) else { return };
        let ws = &self.layout.workspaces[idx];
        if ops::delete_workspace_asks(&info) {
            self.menu = None;
            self.palette = None;
            self.rename = None;
            self.confirm = Some(Confirm {
                text: ops::delete_workspace_text(&ws.name, &info),
                act: ConfirmAct::DeleteWorkspace(ws.id),
            });
            cx.notify();
        } else {
            let id = ws.id;
            self.delete_workspace_now(id, cx);
        }
    }

    fn delete_workspace_now(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !ops::can_delete_workspace(self.layout.workspaces.len()) || !self.layout.workspaces.iter().any(|w| w.id == id) {
            return;
        }
        // By id: a position could name another workspace by the time the server reads it.
        let fut = self.backend.delete_workspace(id);
        self.call(cx, fut, |_, _, _| {});
    }

    /// The user said yes (y, or the button).
    pub fn confirm_yes(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.confirm.take() else { return };
        match c.act {
            ConfirmAct::DeleteWorkspace(id) => self.delete_workspace_now(id, cx),
            ConfirmAct::Quit => {
                self.quit_ok = true;
                cx.quit();
                return;
            }
        }
        self.request_sync(cx);
    }

    pub fn confirm_no(&mut self, cx: &mut Context<Self>) {
        if self.confirm.take().is_some() {
            self.request_sync(cx);
        }
    }

    pub fn confirm_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        match ks.key.as_str() {
            "y" if !ks.modifiers.control && !ks.modifiers.alt => self.confirm_yes(cx),
            // Enter is "no" too: a stray Enter must not delete a workspace or end the shells.
            "enter" | "escape" | "n" => self.confirm_no(cx),
            _ => {}
        }
    }

    // ---- split inside a given tab -------------------------------------------------------

    /// Split the active pane of `tab` (the tab menu's Split items). The tab need not be on
    /// screen yet, so the size comes from the content area.
    pub fn split_in(&mut self, tab: Uuid, dir: SplitDir, before: bool, cx: &mut Context<Self>) {
        let Some((w, t)) = self.layout.find_tab(tab) else { return };
        let tb = &self.layout.workspaces[w].tabs[t];
        let pane = self.active_pane_of(tb);
        let rect = tb
            .pane_tree
            .geometry(self.content_rect(), BAR)
            .panes
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| self.content_rect());
        self.split_pane_in(tab, pane, rect, dir, before, cx);
    }

    // ---- keyboard resize --------------------------------------------------------------

    /// Alt+Shift+Arrow: move the divider next to the active pane by 20 px (GTK).
    pub fn resize_pane_key(&mut self, side: Side, cx: &mut Context<Self>) {
        let (Some((tab_id, geo)), Some(pane)) = (self.active_geometry(), self.active_pane()) else { return };
        // A zoomed pane has no divider on screen.
        if self.zoomed.contains_key(&tab_id) {
            return;
        }
        let Some((w, t)) = self.layout.find_tab(tab_id) else { return };
        let min = match side {
            Side::Left | Side::Right => MIN_PANE_W,
            Side::Up | Side::Down => MIN_PANE_H,
        };
        let tree = &self.layout.workspaces[w].tabs[t].pane_tree;
        let Some(ch) = forgetty_shell_core::layout::resize_by_key(tree, &geo, pane, side, RESIZE_STEP, BAR, min) else {
            return;
        };
        self.layout.workspaces[w].tabs[t].pane_tree.set_ratio_at(&ch.path, ch.ratio);
        let fut = self.backend.update_split_ratios(vec![SplitRatio { pane_id: ch.first_leaf, ratio: ch.ratio }]);
        self.call(cx, fut, |_, _, _| {});
        self.request_sync(cx);
    }

    // ---- attention (bell and notification) ----------------------------------------------

    /// A bell or a notification from `pane` of `machine`. The pane's ring and its tab's badge
    /// show until the user looks at the pane.
    pub fn raise_attention(&mut self, machine: Uuid, pane: PaneId, cx: &mut Context<Self>) {
        let shown = self.machine.id;
        let changed = self
            .with_machine(machine, |s| {
                let focused = machine == shown && s.active_pane() == Some(pane);
                ops::raises_attention(s.window_active, focused) && s.layout.all_panes().contains(&pane) && s.attention.insert(pane)
            })
            .unwrap_or(false);
        if changed {
            cx.notify();
        }
    }

    pub fn tab_has_attention(&self, tab: &forgetty_shell_core::Tab) -> bool {
        !self.attention.is_empty() && tab.pane_tree.leaves().iter().any(|p| self.attention.contains(p))
    }

    // ---- drawing ------------------------------------------------------------------------

    pub(crate) fn render_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let open = self.menu.as_ref()?;
        let m = &open.menu;
        let th = self.theme;
        let hover = alpha(th.hover, 1.0);
        let mut panel = div()
            .absolute()
            .left(px(m.pos.0))
            .top(px(m.pos.1))
            .w(px(m.size.0))
            .flex()
            .flex_col()
            .p(px(menu::PAD))
            .rounded(px(8.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.98))
            .shadow_lg()
            .text_size(px(12.5))
            .text_color(hsla(th.fg))
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _e, _w, cx| cx.stop_propagation()))
            .on_mouse_down(MouseButton::Right, cx.listener(|_, _e, _w, cx| cx.stop_propagation()));
        for (i, it) in m.items.iter().enumerate() {
            let sel = m.selected == Some(i);
            match it {
                Item::Sep => {
                    panel = panel.child(
                        div().h(px(menu::SEP_H)).flex().items_center().child(div().w_full().h(px(1.0)).bg(alpha(th.border, 1.0))),
                    );
                }
                Item::Cmd { cmd, label, hint, enabled } => {
                    let (cmd, enabled) = (cmd.clone(), *enabled);
                    let mut row = div()
                        .id(("menu-item", i))
                        .h(px(menu::ROW_H))
                        .px(px(8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(16.0))
                        .rounded(px(4.0))
                        .when(sel && enabled, |d| d.bg(alpha(th.accent, 0.20)))
                        .child(div().flex_1().min_w_0().truncate().child(SharedString::from(label.clone())))
                        .child(div().flex_none().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(hint.clone())));
                    if enabled {
                        row = row.cursor_pointer().hover(move |s| s.bg(hover)).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |s, _e, w, cx| {
                                cx.stop_propagation();
                                s.run_menu_cmd(cmd.clone(), w, cx)
                            }),
                        );
                    } else {
                        row = row.opacity(0.45);
                    }
                    panel = panel.child(row);
                }
                Item::Swatches { current } => {
                    let mut line = div().h(px(menu::SWATCH_H)).flex().flex_row().items_center().justify_center().gap(px(6.0));
                    for k in 0..menu::SWATCHES {
                        let color = Menu::swatch_color(k);
                        let chosen = *current == color;
                        let cursor = sel && m.swatch == k;
                        let mut dot = div()
                            .id(("swatch", i * 16 + k))
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .border_2()
                            .border_color(if cursor {
                                hsla(th.accent)
                            } else if chosen {
                                hsla(th.fg)
                            } else {
                                alpha(th.border, 0.0)
                            });
                        dot = match color {
                            Some(c) => dot.bg(hsla(c)),
                            // "No color": an empty ring with a cross.
                            None => dot.border_color(if cursor { hsla(th.accent) } else { alpha(th.dim, 1.0) }).text_size(px(11.0)).text_color(hsla(th.dim)).child("\u{00d7}"),
                        };
                        line = line.child(dot.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |s, _e, w, cx| {
                                cx.stop_propagation();
                                s.run_menu_cmd(MenuCmd::Color(color), w, cx)
                            }),
                        ));
                    }
                    panel = panel.child(line);
                }
            }
        }
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.close_menu(cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(|s, _e, _w, cx| s.close_menu(cx)))
                .child(panel)
                .with_animation(
                    "menu-in",
                    Animation::new(Duration::from_millis(60)).with_easing(ease_out_quint()),
                    |el, t| el.opacity(0.4 + 0.6 * t),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_confirm(&self, cx: &mut Context<Self>) -> Option<Div> {
        let c = self.confirm.as_ref()?;
        let quit = matches!(c.act, ConfirmAct::Quit);
        let th = self.theme;
        let button = |id: &'static str, label: &'static str, danger: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .h(px(30.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .border_1()
                .border_color(if danger { hsla(0xff6b68) } else { hsla(th.border) })
                .bg(if danger { alpha(0xff6b68, 0.22) } else { alpha(th.hover, 1.0) })
                .child(label)
        };
        let panel = div()
            .w(px(420.0f32.min(self.viewport.0 - 40.0).max(240.0)))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .p(px(18.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.99))
            .shadow_lg()
            .text_size(px(13.0))
            .text_color(hsla(th.fg))
            .child(SharedString::from(c.text.clone()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("confirm-no", "Cancel", false).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.confirm_no(cx))))
                    .child(button("confirm-yes", if quit { "Quit" } else { "Delete" }, true).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.confirm_yes(cx)))),
            )
            .child(div().text_size(px(11.0)).text_color(hsla(th.dim)).child(if quit { "y quits. Enter or Esc cancels." } else { "y deletes. Enter or Esc cancels." }));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TITLEBAR_H + 80.0))
                .bg(alpha(0x000000, 0.25))
                .occlude()
                .child(panel),
        )
    }
}
