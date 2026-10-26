//! WP2 (menus and workspace/tab operations) in the `--smoke` gate, on the local ConPTY backend:
//! workspace create/color/move/duplicate/delete (with the confirmation), tab color/duplicate/close
//! right/close others, the three context menus (open, keyboard, commands), keyboard pane resize,
//! the attention ring and tab badge from a real bell, and the window title.
//!
//! One state machine, stepped per painted frame. Each step runs a command through the same
//! shell method the menu or the chord calls, and the next step waits for the backend's answer.

use std::time::{Duration, Instant};

use forgetty_shell_core::menu::{Item, MenuCmd, MenuKind};
use forgetty_shell_core::{PaneEvent, PaneId, SplitDir, Uuid};
use gpui::{point, px, Context, Modifiers, MouseButton, MouseDownEvent, Window};
use serde_json::json;

use super::{check, fail, key, record, ShellSmoke};
use crate::shell::AppShell;

/// The screenshot with a tab menu open, a colored tab and a tab badge.
pub const WP2_SHOT: &str = "wp2-menu.png";

#[derive(Default)]
pub struct Wp2 {
    pub(in crate::smoke) w0: Uuid,
    pub(in crate::smoke) w1: Uuid,
    pub(in crate::smoke) ids: Vec<Uuid>,
    pub(in crate::smoke) target: usize,
    ratio: f32,
    ratio_after: f32,
    other_tab: Uuid,
    other_pane: Option<PaneId>,
    bells: u32,
    shot_frames: u32,
    /// The pane a split left/up was aimed at, and the leaf count before it.
    pub(in crate::smoke) split_target: Option<PaneId>,
    pub(in crate::smoke) leaves: usize,
}

fn ws_count(s: &AppShell) -> usize {
    s.layout.workspaces.len()
}

fn tabs_of(s: &AppShell, w: usize) -> Vec<Uuid> {
    s.layout.workspaces.get(w).map(|w| w.tabs.iter().map(|t| t.id).collect()).unwrap_or_default()
}

fn menu_has(s: &AppShell, label: &str, enabled: bool) -> bool {
    s.menu.as_ref().is_some_and(|o| {
        o.menu.items.iter().any(|i| matches!(i, Item::Cmd { label: l, enabled: e, .. } if l == label && *e == enabled))
    })
}

impl ShellSmoke {
    fn wp2_next(&mut self) {
        self.sub += 1;
        self.sub_t0 = Instant::now();
    }

    pub(super) fn wp2(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        if self.sub_t0.elapsed() > Duration::from_secs(10) {
            record("wp2_stuck_at", json!(self.sub));
            record("wp2_layout", json!(format!(
                "ws={} active={} tabs={:?} menu={} confirm={}",
                ws_count(shell),
                shell.layout.active_workspace,
                shell.layout.workspaces.iter().map(|w| (w.name.clone(), w.tabs.len(), w.color.clone())).collect::<Vec<_>>(),
                shell.menu.is_some(),
                shell.confirm.is_some()
            )));
            fail(&format!("wp2 checks stuck at step {}", self.sub));
        }
        // `until!(cond)`: wait here until the backend's answer has reached the shell.
        macro_rules! until {
            ($c:expr) => {
                if !$c {
                    return;
                }
            };
        }
        match self.sub {
            // ---- workspaces -------------------------------------------------------------
            0 => {
                self.wp2.w0 = shell.layout.workspaces[0].id;
                shell.new_workspace(cx);
                self.wp2_next();
            }
            1 => {
                until!(ws_count(shell) == 2);
                self.wp2.w1 = shell.layout.workspaces[1].id;
                shell.set_workspace(1, cx);
                self.wp2_next();
            }
            2 => {
                until!(shell.layout.active_workspace == 1);
                check("ws_create_and_switch", true);
                shell.set_workspace_color(0, Some(0x3987e4), cx);
                self.wp2_next();
            }
            3 => {
                until!(shell.layout.workspaces[0].color.as_deref() == Some("#3987e4"));
                check("ws_color_set", true);
                shell.set_workspace_color(0, None, cx);
                self.wp2_next();
            }
            4 => {
                until!(shell.layout.workspaces[0].color.is_none());
                check("ws_color_cleared", true);
                // The active workspace (index 1) moves up and stays the active one.
                shell.move_workspace(1, true, cx);
                self.wp2_next();
            }
            5 => {
                let l = &shell.layout;
                until!(l.workspaces[0].id == self.wp2.w1 && l.workspaces[1].id == self.wp2.w0 && l.active_workspace == 0);
                check("ws_move_up_active_follows", true);
                shell.duplicate_workspace(0, cx);
                self.wp2_next();
            }
            6 => {
                let l = &shell.layout;
                until!(ws_count(shell) == 3 && l.active_workspace == 1);
                let copy = &l.workspaces[1];
                check("ws_duplicate_next_to_source", copy.name.ends_with("(copy)") && copy.color.is_none());
                check("ws_duplicate_tab_per_tab", copy.tabs.len() == l.workspaces[0].tabs.len());
                let title = shell.window_title();
                record("wp2_title_three_ws", json!(title));
                check("window_title_names_workspace", title.starts_with(&format!("{} - ", copy.name)) && title.ends_with("- Forgetty"));
                shell.new_tab(cx);
                self.wp2_next();
            }
            7 => {
                until!(tabs_of(shell, 1).len() == 2);
                // Two tabs: the delete asks. Enter says no.
                shell.delete_workspace(1, cx);
                check("ws_delete_asks_with_two_tabs", shell.confirm.as_ref().is_some_and(|c| c.text.contains("2 tabs")));
                let handled = shell.handle_key(&key("enter"), window, cx);
                check("ws_delete_enter_cancels", handled && shell.confirm.is_none() && ws_count(shell) == 3);
                shell.delete_workspace(1, cx);
                shell.confirm_yes(cx);
                self.wp2_next();
            }
            8 => {
                until!(ws_count(shell) == 2);
                check("ws_delete_confirmed", shell.layout.workspaces[0].id == self.wp2.w1 && shell.layout.workspaces[1].id == self.wp2.w0);
                // One tab: no question.
                shell.delete_workspace(0, cx);
                check("ws_delete_single_tab_no_question", shell.confirm.is_none());
                self.wp2_next();
            }
            9 => {
                until!(ws_count(shell) == 1 && shell.layout.workspaces[0].id == self.wp2.w0);
                check("ws_delete_single_tab", true);
                shell.delete_workspace(0, cx);
                check("ws_last_cannot_be_deleted", shell.confirm.is_none());
                let title = shell.window_title();
                record("wp2_title_one_ws", json!(title));
                let name = shell.layout.workspaces[0].name.clone();
                check("window_title_one_workspace_has_no_workspace_name", !title.starts_with(&format!("{name} - ")) && title.ends_with("Forgetty"));
                self.wp2_next();
            }
            10 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(400));
                check("ws_last_stays", ws_count(shell) == 1);
                // ---- tabs ---------------------------------------------------------------
                let n = tabs_of(shell, 0).len();
                self.wp2.target = n.max(4);
                for _ in n..self.wp2.target {
                    shell.new_tab(cx);
                }
                self.wp2_next();
            }
            11 => {
                until!(tabs_of(shell, 0).len() == self.wp2.target);
                let ids = tabs_of(shell, 0);
                shell.set_tab_color(ids[1], Some(0xe0493a), cx);
                let set = shell.tab_colors.get(&ids[1]) == Some(&0xe0493a);
                shell.set_tab_color(ids[1], None, cx);
                check("tab_color_set_and_cleared", set && !shell.tab_colors.contains_key(&ids[1]));
                self.wp2.ids = ids.clone();
                shell.duplicate_tab(ids[1], cx);
                self.wp2_next();
            }
            12 => {
                let ids = tabs_of(shell, 0);
                until!(ids.len() == self.wp2.target + 1 && !self.wp2.ids.contains(&ids[2]));
                check("tab_duplicate_next_to_source", ids[1] == self.wp2.ids[1] && ids[3] == self.wp2.ids[2]);
                shell.close_tabs_right(ids[1], cx);
                self.wp2_next();
            }
            13 => {
                let ids = tabs_of(shell, 0);
                until!(ids.len() == 2);
                check("tab_close_to_the_right", ids[0] == self.wp2.ids[0] && ids[1] == self.wp2.ids[1]);
                shell.new_tab(cx);
                shell.new_tab(cx);
                self.wp2_next();
            }
            14 => {
                until!(tabs_of(shell, 0).len() == 4);
                let keep = tabs_of(shell, 0)[1];
                self.wp2.ids = vec![keep];
                shell.close_other_tabs(keep, cx);
                self.wp2_next();
            }
            15 => {
                let ids = tabs_of(shell, 0);
                until!(ids.len() == 1);
                check("tab_close_others", ids[0] == self.wp2.ids[0]);
                shell.new_tab(cx);
                self.wp2_next();
            }
            // ---- menus ------------------------------------------------------------------
            16 => {
                until!(tabs_of(shell, 0).len() == 2);
                let ids = tabs_of(shell, 0);
                let active = shell.active_tab().map(|t| t.id).unwrap_or(ids[0]);
                shell.open_tab_menu(active, (300.0, 40.0), false, cx);
                check(
                    "menu_tab_opens",
                    shell.menu.as_ref().is_some_and(|o| o.menu.kind == MenuKind::Tab && o.menu.selected.is_none())
                        && menu_has(shell, "Duplicate Tab", true)
                        && menu_has(shell, "Close Other Tabs", true),
                );
                shell.handle_key(&key("down"), window, cx);
                check("menu_down_selects_a_row", shell.menu.as_ref().is_some_and(|o| o.menu.selected.is_some()));
                let handled = shell.handle_key(&key("escape"), window, cx);
                check("menu_escape_closes", handled && shell.menu.is_none());
                // A color through the menu path.
                shell.open_tab_menu(active, (300.0, 40.0), false, cx);
                shell.run_menu_cmd(MenuCmd::Color(Some(0x3987e4)), window, cx);
                check("menu_color_sets_tab_color", shell.menu.is_none() && shell.tab_colors.get(&active) == Some(&0x3987e4));
                // Shift+right-click on the pane asks for the pane menu.
                let pane = shell.active_pane().unwrap();
                let Some(view) = shell.views.get(&pane).cloned() else { fail("wp2: no view") };
                let bounds = view.read(cx).bounds;
                let at = point(bounds.origin.x + px(60.0), bounds.origin.y + px(40.0));
                let down = |modifiers| MouseDownEvent { button: MouseButton::Right, position: at, modifiers, click_count: 1, first_mouse: false };
                view.update(cx, |v, vcx| v.on_mouse_down(&down(Modifiers { shift: true, ..Default::default() }), window, vcx));
                self.wp2_next();
            }
            17 => {
                until!(shell.menu.is_some());
                let pane = shell.active_pane().unwrap();
                check(
                    "menu_pane_opens_on_shift_right_click",
                    shell.menu.as_ref().is_some_and(|o| o.menu.kind == MenuKind::Pane && o.target == crate::menus::MenuTarget::Pane(pane))
                        && menu_has(shell, "Copy", false)
                        && menu_has(shell, "Paste", true)
                        && menu_has(shell, "Split Right", true),
                );
                shell.close_menu(cx);
                // A plain right-click opens the menu (GTK) unless `right_click = "copy_paste"`.
                check("right_click_default_is_the_menu", !shell.cfg.right_click_copy_paste);
                let Some(view) = shell.views.get(&pane).cloned() else { fail("wp2: no view") };
                // Open the pane menu with a link and use the keyboard.
                shell.open_pane_menu(pane, (200.0, 200.0), Some("https://example.com/menu".into()), false, true, cx);
                check(
                    "menu_pane_link_item",
                    menu_has(shell, "Open https://example.com/menu", true) && shell.menu.as_ref().is_some_and(|o| o.menu.selected == Some(1)),
                );
                shell.run_menu_cmd(MenuCmd::OpenLink("https://example.com/menu".into()), window, cx);
                let opened = view.read(cx).opened_urls.iter().any(|u| u == "https://example.com/menu");
                check("menu_open_link_runs", opened && shell.menu.is_none());
                // Split Right from the menu.
                shell.open_pane_menu(pane, (200.0, 200.0), None, false, false, cx);
                shell.run_menu_cmd(MenuCmd::SplitRight, window, cx);
                self.wp2_next();
            }
            18 => {
                until!(Self::tab_pane_count(shell) == 2);
                check("menu_split_right", true);
                let Some((_, geo)) = shell.active_geometry() else { fail("wp2: no geometry") };
                self.wp2.ratio = geo.dividers.first().map(|d| d.ratio).unwrap_or(0.5);
                // Alt+Shift+Left: the new (second) pane grows, the divider moves left.
                self.press(shell, window, cx, "alt+shift+left");
                let after = shell.active_geometry().and_then(|(_, g)| g.dividers.first().map(|d| d.ratio)).unwrap_or(0.5);
                record("wp2_resize", json!({"before": self.wp2.ratio, "after": after}));
                check("resize_key_moves_divider_toward_the_arrow", after < self.wp2.ratio - 0.001);
                self.wp2.ratio_after = after;
                self.wp2_next();
            }
            19 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(600));
                let now = shell.active_geometry().and_then(|(_, g)| g.dividers.first().map(|d| d.ratio)).unwrap_or(0.5);
                check("resize_key_kept_by_the_backend", (now - self.wp2.ratio_after).abs() < 0.002);
                self.press(shell, window, cx, "alt+shift+right");
                let back = shell.active_geometry().and_then(|(_, g)| g.dividers.first().map(|d| d.ratio)).unwrap_or(0.5);
                check("resize_key_back_to_start", (back - self.wp2.ratio).abs() < 0.01);
                self.wp2_next();
            }
            // ---- attention --------------------------------------------------------------
            20 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(300));
                let ids = tabs_of(shell, 0);
                let active = shell.active_tab().map(|t| t.id).unwrap();
                let Some(other) = ids.iter().copied().find(|t| *t != active) else { fail("wp2: no second tab") };
                let pane = shell.layout.find_tab(other).and_then(|(w, t)| shell.layout.workspaces[w].tabs.get(t).map(|t| shell.active_pane_of(t)));
                let Some(pane) = pane else { fail("wp2: no pane in the second tab") };
                self.wp2.other_tab = other;
                self.wp2.other_pane = Some(pane);
                check("attention_starts_empty", shell.attention.is_empty());
                // A bell in the pane nobody looks at.
                match self.local.pty(pane).map(|p| p.injector()) {
                    Some(tx) => {
                        let _ = tx.try_send(PaneEvent::Output(vec![0x07]));
                    }
                    None => fail("wp2: no pty for the second tab"),
                }
                // And one in the pane that has the focus.
                let ap = shell.active_pane().unwrap();
                self.wp2.bells = shell.views.get(&ap).map(|v| v.read(cx).bells).unwrap_or(0);
                if let Some(tx) = self.local.pty(ap).map(|p| p.injector()) {
                    let _ = tx.try_send(PaneEvent::Output(vec![0x07]));
                }
                self.wp2_next();
            }
            21 => {
                let pane = self.wp2.other_pane.unwrap();
                let ap = shell.active_pane().unwrap();
                let rang = shell.views.get(&ap).is_some_and(|v| v.read(cx).bells > self.wp2.bells);
                until!(shell.attention.contains(&pane) && rang);
                check("bell_in_background_pane_raises_attention", true);
                let other = shell.layout.find_tab(self.wp2.other_tab).map(|(w, t)| &shell.layout.workspaces[w].tabs[t]);
                check("tab_badge_for_background_bell", other.is_some_and(|t| shell.tab_has_attention(t)));
                check("bell_in_focused_pane_stays_quiet", !shell.attention.contains(&ap));
                // The screenshot: menu open on the colored tab, badge on the other one.
                let active = shell.active_tab().map(|t| t.id).unwrap();
                shell.open_tab_menu(active, (300.0, 44.0), false, cx);
                if !shell.sidebar_open {
                    shell.toggle_sidebar(cx);
                }
                self.wp2.shot_frames = 0;
                self.wp2_next();
            }
            22 => {
                self.wp2.shot_frames += 1;
                until!(self.sub_t0.elapsed() > Duration::from_millis(600) && self.wp2.shot_frames > 6);
                let r = crate::screenshot::capture(window, WP2_SHOT);
                match &r {
                    Ok(s) => record("wp2_screenshot", json!({"file": WP2_SHOT, "w": s.w, "h": s.h, "distinct_colors": s.distinct, "non_background_pct": s.busy_pct})),
                    Err(e) => record("wp2_screenshot", json!({"error": e})),
                }
                check("wp2_screenshot", r.as_ref().is_ok_and(|s| s.distinct >= 8));
                shell.close_menu(cx);
                // Looking at the pane answers its bell.
                shell.focus_tab(self.wp2.other_tab, cx);
                self.wp2_next();
            }
            23 => {
                until!(shell.active_tab().is_some_and(|t| t.id == self.wp2.other_tab) && shell.attention.is_empty());
                check("attention_clears_when_the_pane_is_looked_at", true);
                // A plain right-click on the pane.
                let pane = shell.active_pane().unwrap();
                let Some(view) = shell.views.get(&pane).cloned() else { fail("wp2: no view") };
                let bounds = view.read(cx).bounds;
                let at = point(bounds.origin.x + px(60.0), bounds.origin.y + px(40.0));
                let down = MouseDownEvent { button: MouseButton::Right, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false };
                view.update(cx, |v, vcx| v.on_mouse_down(&down, window, vcx));
                self.wp2_next();
            }
            // ---- split left and up ------------------------------------------------------
            24 => {
                until!(shell.menu.is_some());
                check("plain_right_click_opens_the_menu", shell.menu.as_ref().is_some_and(|o| o.menu.kind == MenuKind::Pane));
                check(
                    "menu_lists_split_left_and_up_when_the_backend_can",
                    shell.backend.supports_split_before() && menu_has(shell, "Split Left", true) && menu_has(shell, "Split Up", true),
                );
                self.wp2.split_target = shell.active_pane();
                self.wp2.leaves = shell.active_tab().map(|t| t.pane_tree.leaves().len()).unwrap_or(0);
                shell.run_menu_cmd(MenuCmd::SplitLeft, window, cx);
                self.wp2_next();
            }
            25 => {
                let leaves = shell.active_tab().map(|t| t.pane_tree.leaves()).unwrap_or_default();
                let old = leaves.iter().position(|p| Some(*p) == self.wp2.split_target);
                // The new pane sits right before the one that was split, and has the focus.
                until!(leaves.len() == self.wp2.leaves + 1 && old.is_some_and(|i| i > 0 && Some(leaves[i - 1]) == shell.active_pane()));
                check("split_left_puts_the_new_pane_first", true);
                self.wp2.leaves = leaves.len();
                self.wp2.split_target = shell.active_pane();
                shell.split_before(SplitDir::Vertical, cx);
                self.wp2_next();
            }
            26 => {
                let leaves = shell.active_tab().map(|t| t.pane_tree.leaves()).unwrap_or_default();
                let old = leaves.iter().position(|p| Some(*p) == self.wp2.split_target);
                until!(leaves.len() == self.wp2.leaves + 1 && old.is_some_and(|i| i > 0 && Some(leaves[i - 1]) == shell.active_pane()));
                check("split_up_puts_the_new_pane_first", true);
                self.wp2_next();
            }
            // ---- a failed backend call is shown, not just printed ----------------------
            27 => {
                // A workspace that does not exist: the backend answers with an error.
                let fut = shell.backend.delete_workspace(Uuid::new_v4());
                shell.call(cx, fut, |_, _, _| {});
                self.wp2_next();
            }
            28 => {
                let shown = shell.active_pane().and_then(|p| shell.views.get(&p).cloned()).and_then(|v| v.read(cx).notice_text());
                until!(shown.as_ref().is_some_and(|t| t.starts_with("Failed")));
                check("backend_error_is_shown_as_a_notice", true);
                self.goto(super::Phase::RSetup);
            }
            _ => {}
        }
    }
}
