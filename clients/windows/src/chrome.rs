//! Rendering of the shell: titlebar with tabs, sidebar, split panes with
//! dividers, command palette.

use std::time::Duration;

use forgetty_shell_core::layout::SplitDir;
use forgetty_shell_core::{LabelTarget, PaneStatus, Tab};
use gpui::{
    canvas, div, ease_out_quint, prelude::*, px, Animation, AnimationExt, AnyElement, Context, Div,
    FontWeight, Hsla, IntoElement, MouseButton, MouseDownEvent, Render, SharedString, Window, WindowControlArea,
};

use crate::shell::*;
use crate::view::hsla;

fn alpha(hex: u32, a: f32) -> Hsla {
    let mut c = hsla(hex);
    c.a = a;
    c
}

const OK: u32 = 0x6ccb5f;
const WARN: u32 = 0xe5c07b;
const BAD: u32 = 0xff6b68;

impl AppShell {
    fn chrome_bg(&self) -> Hsla {
        alpha(self.theme.chrome, if self.mica { 0.72 } else { 1.0 })
    }

    fn spinner(&self) -> AnyElement {
        let accent = self.theme.accent;
        let mut d = div().relative().w(px(12.0)).h(px(12.0)).flex_none();
        for k in 0..8usize {
            let ang = k as f32 / 8.0 * std::f32::consts::TAU;
            let (cx, cy) = (6.0 + 4.6 * ang.sin(), 6.0 - 4.6 * ang.cos());
            let phase = ((k + 8 - self.spin % 8) % 8) as f32 / 8.0;
            d = d.child(
                div()
                    .absolute()
                    .left(px(cx - 1.2))
                    .top(px(cy - 1.2))
                    .w(px(2.4))
                    .h(px(2.4))
                    .rounded_full()
                    .bg(alpha(accent, 0.15 + 0.85 * phase)),
            );
        }
        d.into_any_element()
    }

    pub fn status_icon(&self, st: PaneStatus) -> Option<AnyElement> {
        Some(match st {
            PaneStatus::None | PaneStatus::Idle => return None,
            PaneStatus::Working => self.spinner(),
            PaneStatus::Waiting => div().w(px(12.0)).h(px(12.0)).flex_none().flex().items_center().justify_center()
                .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(hsla(WARN)))
                .into_any_element(),
            PaneStatus::Done => div().w(px(12.0)).flex_none().text_size(px(12.0)).text_color(hsla(OK)).child("\u{2713}").into_any_element(),
            PaneStatus::Failed => div().w(px(12.0)).flex_none().text_size(px(12.0)).text_color(hsla(BAD)).child("\u{2715}").into_any_element(),
        })
    }

    fn render_tab(&self, i: usize, tab: &Tab, active: bool, tw: f32, cx: &mut Context<Self>) -> AnyElement {
        let id = tab.id;
        let th = self.theme;
        let renaming = self.rename.as_ref().filter(|r| r.tab == id && r.target != LabelTarget::Workspace);
        let title = self.tab_title(tab);
        let cwd = self.tab_cwd(tab);
        let status = self.tab_status(tab);
        let tint = self.tab_colors.get(&id).copied();
        let badge = self.tab_has_attention(tab);
        let hover = alpha(th.hover, if self.mica { 0.6 } else { 1.0 });

        let mut label = div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis();
        if let Some(r) = renaming {
            // Inline editor: text with a caret at the cursor position.
            let t = &r.edit.text;
            let split = t.char_indices().nth(r.edit.cursor).map(|x| x.0).unwrap_or(t.len());
            label = div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .whitespace_nowrap()
                .child(SharedString::from(t[..split].to_string()))
                .child(div().w(px(1.0)).h(px(14.0)).bg(hsla(th.accent)))
                .child(SharedString::from(t[split..].to_string()));
        } else {
            label = label.child(SharedString::from(title));
        }

        div()
            .id(("tab", i))
            .w(px(tw))
            .h(px(TITLEBAR_H - 6.0))
            .mt(px(6.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .px(px(10.0))
            .rounded_t(px(8.0))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(hsla(if active { th.fg } else { th.dim }))
            .bg(if active { alpha(th.tab_active, 1.0) } else { alpha(th.chrome, 0.0) })
            .when(!active, |d| d.hover(move |s| s.bg(hover)))
            .when(active, |d| d.border_t_2().border_color(hsla(tint.unwrap_or(th.accent))))
            .on_mouse_down(MouseButton::Left, cx.listener(move |s, ev, _w, cx| s.on_tab_mouse_down(id, i, ev, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |s, ev, _w, cx| s.on_tab_mouse_down(id, i, ev, cx)))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |s, _ev, _w, cx| s.close_tab(id, cx)),
            )
            .children(tint.map(|c| div().w(px(8.0)).h(px(8.0)).flex_none().rounded_full().bg(hsla(c))))
            .children(self.status_icon(status))
            .child(label)
            .children(badge.then(|| {
                div().w(px(8.0)).h(px(8.0)).flex_none().rounded_full().bg(hsla(crate::menus::ATTENTION))
            }))
            .when(!cwd.is_empty() && tw >= 150.0 && renaming.is_none(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .max_w(px(70.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.0))
                        .text_color(hsla(th.dim))
                        .child(SharedString::from(cwd)),
                )
            })
            .child(
                div()
                    .id(("tab-close", i))
                    .flex_none()
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .text_size(px(14.0))
                    .text_color(hsla(th.dim))
                    .when(!active, |d| d.opacity(0.0))
                    .hover(move |s| s.bg(hover).text_color(hsla(th.fg)).opacity(1.0))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |s, _ev, _w, cx| {
                            cx.stop_propagation();
                            s.close_tab(id, cx)
                        }),
                    )
                    .child("\u{00d7}"),
            )
            .into_any_element()
    }

    fn caption_button(&self, id: &'static str, area: WindowControlArea, glyph: AnyElement, danger: bool) -> AnyElement {
        let th = self.theme;
        let hover = if danger { hsla(0xe81123) } else { alpha(th.hover, 1.0) };
        div()
            .id(id)
            .occlude()
            .w(px(46.0))
            .h(px(TITLEBAR_H))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .text_color(hsla(th.fg))
            .hover(move |s| s.bg(hover))
            .window_control_area(area)
            .child(glyph)
            .into_any_element()
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let (_, tw) = self.tab_strip();
        let active_idx = self.layout.active_workspace().map(|w| w.active_tab).unwrap_or(0);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_end()
            .flex_none()
            .w_full()
            .h(px(TITLEBAR_H))
            .bg(self.chrome_bg())
            .pl(px(TAB_LEFT));
        if let Some(ws) = self.layout.active_workspace() {
            for (i, t) in ws.tabs.iter().enumerate() {
                bar = bar.child(self.render_tab(i, t, i == active_idx, tw, cx));
            }
        }
        let hover = alpha(th.hover, 1.0);
        bar = bar.child(
            div()
                .id("new-tab")
                .flex_none()
                .w(px(PLUS_W))
                .h(px(TITLEBAR_H - 6.0))
                .mt(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_t(px(8.0))
                .cursor_pointer()
                .text_size(px(17.0))
                .text_color(hsla(th.dim))
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.new_tab(cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(|s, _e, _w, cx| s.open_new_tab_menu(false, cx)))
                .child("+"),
        );
        // The drop-down: profiles, a plain tab, the splits.
        bar = bar.child(
            div()
                .id("new-tab-menu")
                .flex_none()
                .w(px(CHEVRON_W))
                .h(px(TITLEBAR_H - 6.0))
                .mt(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_t(px(8.0))
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(hsla(th.dim))
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.open_new_tab_menu(false, cx)))
                .child("\u{25be}"),
        );
        // Everything else is draggable titlebar.
        bar = bar.child(div().flex_1().h_full().min_w(px(24.0)).window_control_area(WindowControlArea::Drag));

        let line = |w: f32, h: f32| div().w(px(w)).h(px(h)).bg(hsla(th.fg));
        let boxed = |s: f32| div().w(px(s)).h(px(s)).border_1().border_color(hsla(th.fg));
        let max_glyph = if window.is_maximized() {
            div()
                .relative()
                .w(px(10.0))
                .h(px(10.0))
                .child(div().absolute().left(px(2.0)).top(px(0.0)).w(px(8.0)).h(px(8.0)).border_1().border_color(hsla(th.fg)))
                .child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(2.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .border_1()
                        .border_color(hsla(th.fg))
                        .bg(self.chrome_bg()),
                )
                .into_any_element()
        } else {
            boxed(10.0).into_any_element()
        };
        bar.child(self.caption_button("cap-min", WindowControlArea::Min, line(10.0, 1.0).into_any_element(), false))
            .child(self.caption_button("cap-max", WindowControlArea::Max, max_glyph, false))
            .child(self.caption_button(
                "cap-close",
                WindowControlArea::Close,
                div().text_size(px(18.0)).child("\u{00d7}").into_any_element(),
                true,
            ))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let hover = alpha(th.hover, 1.0);
        let header = |t: &'static str| {
            div()
                .px(px(14.0))
                .pt(px(14.0))
                .pb(px(5.0))
                .text_size(px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(hsla(th.dim))
                .child(t)
        };
        let mut sb = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_W))
            .h_full()
            .overflow_hidden()
            .bg(alpha(th.chrome, if self.mica { 0.60 } else { 1.0 }))
            .border_r_1()
            .border_color(alpha(th.border, 0.7))
            .text_size(px(12.5))
            .text_color(hsla(th.fg));

        // Machines: the local one and every paired remote, with the state of each link.
        sb = sb.child(header("MACHINES"));
        let now = std::time::Instant::now();
        for (i, e) in self.machines.iter().enumerate() {
            let active = e.id == self.machine.id;
            let id = e.id;
            let (dot, label) = match (e.kind, &e.conn) {
                (forgetty_shell_core::MachineKind::Local, _) => (OK, "local"),
                (_, forgetty_shell_core::ConnState::Connected) => (OK, "connected"),
                (_, c @ forgetty_shell_core::ConnState::Reconnecting { .. }) => {
                    (WARN, if c.show_overlay(now) { "reconnecting" } else { "connecting" })
                }
                (_, forgetty_shell_core::ConnState::UnreachableDirectOnly { .. }) => (WARN, "unreachable"),
                (_, forgetty_shell_core::ConnState::Failed(_)) => (BAD, "offline"),
                (_, forgetty_shell_core::ConnState::Closed) => (BAD, "closed"),
            };
            sb = sb.child(
                div()
                    .id(("machine", i))
                    .mx(px(8.0))
                    .px(px(8.0))
                    .h(px(28.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .when(active, |d| d.bg(alpha(th.hover, 0.8)))
                    .hover(move |s| s.bg(hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.switch_machine(id, cx)))
                    .child(div().w(px(7.0)).h(px(7.0)).flex_none().rounded_full().bg(hsla(dot)))
                    .child(div().flex_1().truncate().child(SharedString::from(forgetty_shell_core::notify::sanitize(&e.name, 48))))
                    .child(div().flex_none().text_size(px(10.5)).text_color(hsla(th.dim)).child(label)),
            );
        }
        sb = sb.child(
            div()
                .id("machine-pair")
                .mx(px(8.0))
                .px(px(8.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_color(hsla(th.accent))
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.open_pair(cx)))
                .child("+ Pair machine..."),
        );

        // Workspaces.
        sb = sb.child(header("WORKSPACES"));
        for (i, ws) in self.layout.workspaces.iter().enumerate() {
            let active = i == self.layout.active_workspace;
            let ws_color = forgetty_shell_core::colors::from_wire(ws.color.as_deref());
            let ws_badge = ws.tabs.iter().any(|t| self.tab_has_attention(t));
            sb = sb.child(
                div()
                    .id(("ws", i))
                    .mx(px(8.0))
                    .px(px(8.0))
                    .h(px(28.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .when(active, |d| d.bg(alpha(th.hover, 0.8)))
                    .hover(move |s| s.bg(hover))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |s, ev: &MouseDownEvent, _w, cx| {
                        if ev.click_count >= 2 {
                            s.begin_rename_workspace(i, cx);
                        } else {
                            s.rename = None;
                            s.set_workspace(i, cx);
                        }
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |s, ev: &MouseDownEvent, _w, cx| {
                        s.open_workspace_menu(i, (f32::from(ev.position.x), f32::from(ev.position.y)), false, cx);
                    }))
                    .child(div().w(px(3.0)).h(px(14.0)).rounded_full().bg(match ws_color {
                        Some(c) => hsla(c),
                        None if active => hsla(th.accent),
                        None => alpha(th.border, 1.0),
                    }))
                    .child(match self.rename.as_ref().filter(|r| r.target == LabelTarget::Workspace && r.id == ws.id) {
                        Some(r) => {
                            let t = &r.edit.text;
                            let split = t.char_indices().nth(r.edit.cursor).map(|x| x.0).unwrap_or(t.len());
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .flex()
                                .flex_row()
                                .whitespace_nowrap()
                                .child(SharedString::from(t[..split].to_string()))
                                .child(div().w(px(1.0)).h(px(14.0)).bg(hsla(th.accent)))
                                .child(SharedString::from(t[split..].to_string()))
                        }
                        None => div().flex_1().truncate().child(SharedString::from(forgetty_shell_core::notify::sanitize(&ws.name, 80))),
                    })
                    .children(ws_badge.then(|| {
                        div().w(px(7.0)).h(px(7.0)).flex_none().rounded_full().bg(hsla(crate::menus::ATTENTION))
                    }))
                    .child(div().text_size(px(10.5)).text_color(hsla(th.dim)).child(SharedString::from(ws.tabs.len().to_string()))),
            );
        }
        sb = sb.child(
            div()
                .id("ws-new")
                .mx(px(8.0))
                .px(px(8.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_color(hsla(th.dim))
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.new_workspace(cx)))
                .child("+ New workspace"),
        );

        // Tabs of the active workspace.
        sb = sb.child(header("TABS"));
        if let Some(ws) = self.layout.active_workspace() {
            for (i, t) in ws.tabs.iter().enumerate() {
                let active = i == ws.active_tab;
                let id = t.id;
                let st = self.tab_status(t);
                let cwd = self.tab_cwd(t);
                let tint = self.tab_colors.get(&id).copied();
                let badge = self.tab_has_attention(t);
                sb = sb.child(
                    div()
                        .id(("sb-tab", i))
                        .mx(px(8.0))
                        .px(px(8.0))
                        .h(px(28.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(5.0))
                        .cursor_pointer()
                        .text_color(hsla(if active { th.fg } else { th.dim }))
                        .when(active, |d| d.bg(alpha(th.hover, 0.8)))
                        .hover(move |s| s.bg(hover))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.focus_tab(id, cx)))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |s, ev: &MouseDownEvent, _w, cx| {
                            s.open_tab_menu(id, (f32::from(ev.position.x), f32::from(ev.position.y)), false, cx);
                        }))
                        .children(tint.map(|c| div().w(px(8.0)).h(px(8.0)).flex_none().rounded_full().bg(hsla(c))))
                        .child(
                            div()
                                .w(px(12.0))
                                .flex_none()
                                .children(self.status_icon(st)),
                        )
                        .child(div().flex_1().truncate().child(SharedString::from(self.tab_title(t))))
                        .children(badge.then(|| {
                            div().w(px(7.0)).h(px(7.0)).flex_none().rounded_full().bg(hsla(crate::menus::ATTENTION))
                        }))
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(70.0))
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(hsla(th.dim))
                                .child(SharedString::from(cwd)),
                        ),
                );
            }
        }

        // About: the update check runs only when this is clicked.
        let line = self.update.line(env!("CARGO_PKG_VERSION"));
        sb = sb.child(header("ABOUT"));
        sb = sb.child(
            div()
                .id("check-updates")
                .mx(px(8.0))
                .px(px(8.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_color(hsla(th.dim))
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.check_for_updates(cx)))
                .child("Check for updates"),
        );
        if !line.is_empty() {
            let found = matches!(self.update, crate::update::UpdateState::Available { .. });
            sb = sb.child(
                div()
                    .id("update-status")
                    .mx(px(8.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(11.0))
                    .text_color(hsla(if found { th.accent } else { th.dim }))
                    .when(found, |d| d.cursor_pointer().on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.update_line_clicked(cx))))
                    .child(SharedString::from(line)),
            );
        }
        sb
    }

    fn render_panes(&self, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let mut root = div().relative().flex_1().h_full().min_w_0().overflow_hidden();
        let Some((tab_id, geo)) = self.active_geometry() else { return root };
        let active = self.active_pane();
        let multi = self.active_tab().is_some_and(|t| t.pane_tree.pane_count() > 1);
        let zoomed = self.zoomed.contains_key(&tab_id);
        let overlay = self.overlay_text(std::time::Instant::now());
        let retry = overlay.is_some() && self.overlay_retry();
        for (n, (pane, r)) in geo.panes.iter().enumerate() {
            let pane = *pane;
            let focused = Some(pane) == active;
            let mut d = div()
                .absolute()
                .left(px(r.x))
                .top(px(r.y))
                .w(px(r.w))
                .h(px(r.h))
                .overflow_hidden()
                .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, w, cx| s.focus_pane(pane, w, cx)));
            d = match self.views.get(&pane) {
                Some(v) => d.child(v.clone()),
                None => d,
            };
            if multi && focused {
                // Thin accent frame as four 1 px bars (a bordered full-pane quad costs
                // a full-pane fill on software GPUs).
                let acc = hsla(th.accent);
                let bar = || div().absolute().bg(acc);
                d = d
                    .child(bar().top_0().left_0().w_full().h(px(1.0)))
                    .child(bar().bottom_0().left_0().w_full().h(px(1.0)))
                    .child(bar().top_0().left_0().h_full().w(px(1.0)))
                    .child(bar().top_0().right_0().h_full().w(px(1.0)));
            }
            if self.attention.contains(&pane) {
                // The pane rang its bell or sent a notification while nobody looked: a 3 px
                // amber ring until the user looks at it.
                let ring = alpha(crate::menus::ATTENTION, 0.9);
                let bar = || div().absolute().bg(ring);
                d = d
                    .child(bar().top_0().left_0().w_full().h(px(3.0)))
                    .child(bar().bottom_0().left_0().w_full().h(px(3.0)))
                    .child(bar().top_0().left_0().h_full().w(px(3.0)))
                    .child(bar().top_0().right_0().h_full().w(px(3.0)));
            }
            if let Some(text) = &overlay {
                // Only this machine's panes, only after the link has been down for 1 s.
                d = d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_col()
                        .gap(px(10.0))
                        .bg(alpha(th.bg, 0.72))
                        .text_color(hsla(th.fg))
                        .text_size(px(13.0))
                        .child(SharedString::from(text.clone()))
                        .when(retry, |o| {
                            o.child(
                                div()
                                    .id(("retry", n))
                                    .px(px(14.0))
                                    .h(px(30.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(hsla(th.accent))
                                    .bg(alpha(th.accent, 0.25))
                                    .hover(move |s| s.bg(alpha(th.hover, 1.0)))
                                    .child("Retry")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| {
                                        cx.stop_propagation();
                                        s.retry_connection(cx);
                                    })),
                            )
                        }),
                );
            }
            if zoomed {
                d = d.child(
                    div()
                        .absolute()
                        .top(px(6.0))
                        .right(px(14.0))
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(4.0))
                        .bg(alpha(th.accent, 0.9))
                        .text_color(hsla(th.bg))
                        .text_size(px(10.0))
                        .child("ZOOM"),
                );
            }
            root = root.child(d);
        }
        for (i, dv) in geo.dividers.iter().enumerate() {
            let dvc = dv.clone();
            let (hx, hy, hw, hh) = match dv.dir {
                SplitDir::Horizontal => (dv.bar.x - 4.0, dv.bar.y, dv.bar.w + 8.0, dv.bar.h),
                SplitDir::Vertical => (dv.bar.x, dv.bar.y - 4.0, dv.bar.w, dv.bar.h + 8.0),
            };
            let dragging = matches!(&self.drag, Some(Drag::Divider { divider, .. }) if divider.path == dv.path);
            let line_col = if dragging { hsla(th.accent) } else { alpha(th.border, 1.0) };
            let hot = hsla(th.accent);
            let mut d = div()
                .id(("divider", i))
                .absolute()
                .left(px(hx))
                .top(px(hy))
                .w(px(hw))
                .h(px(hh))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |s, _e, _w, cx| {
                        cx.stop_propagation();
                        s.begin_divider_drag(tab_id, dvc.clone(), cx)
                    }),
                );
            let line = match dv.dir {
                SplitDir::Horizontal => div().absolute().left(px(4.0)).top(px(0.0)).w(px(dv.bar.w)).h_full(),
                SplitDir::Vertical => div().absolute().left(px(0.0)).top(px(4.0)).w_full().h(px(dv.bar.h)),
            };
            d = match dv.dir {
                SplitDir::Horizontal => d.cursor_col_resize(),
                SplitDir::Vertical => d.cursor_row_resize(),
            };
            root = root.child(d.hover(move |s| s.bg(alpha(0x000000, 0.0))).child(line.bg(line_col)));
            let _ = hot;
        }
        root
    }

    fn render_palette(&self) -> Option<Div> {
        let p = self.palette.as_ref()?;
        let th = self.theme;
        let width = 560.0f32.min(self.viewport.0 - 40.0).max(240.0);
        let t = &p.edit.text;
        let split = t.char_indices().nth(p.edit.cursor).map(|x| x.0).unwrap_or(t.len());

        let input = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(40.0))
            .px(px(14.0))
            .border_b_1()
            .border_color(alpha(th.border, 1.0))
            .text_size(px(14.0))
            .child(if t.is_empty() {
                div().flex().flex_row().items_center().child(div().w(px(1.0)).h(px(18.0)).bg(hsla(th.accent))).child(
                    div().ml(px(2.0)).text_color(hsla(th.dim)).child(if p.themes.is_some() { "Type a theme name" } else { "Type a command" }),
                )
            } else {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(SharedString::from(t[..split].to_string()))
                    .child(div().w(px(1.0)).h(px(18.0)).bg(hsla(th.accent)))
                    .child(SharedString::from(t[split..].to_string()))
            });

        let mut list = div().flex().flex_col().py(px(4.0)).max_h(px(380.0)).overflow_hidden();
        if p.items.is_empty() {
            list = list.child(div().px(px(14.0)).h(px(30.0)).flex().items_center().text_color(hsla(th.dim)).child(if p.themes.is_some() { "No matching theme" } else { "No matching command" }));
        }
        // Keep the selection in view: show a window of 11 rows.
        let first = p.selected.saturating_sub(10);
        for (n, it) in p.items.iter().enumerate().skip(first).take(11) {
            let sel = n == p.selected;
            let mut title = div().flex().flex_row().flex_1().min_w_0();
            let chars: Vec<char> = it.title.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let hit = it.hits.contains(&i);
                let mut j = i;
                while j < chars.len() && it.hits.contains(&j) == hit {
                    j += 1;
                }
                let seg: String = chars[i..j].iter().collect();
                let seg = if seg.starts_with(' ') || seg.ends_with(' ') {
                    seg.replace(' ', "\u{00a0}")
                } else {
                    seg
                };
                title = title.child(
                    div()
                        .whitespace_nowrap()
                        .text_color(if hit { hsla(th.accent) } else { hsla(th.fg) })
                        .when(hit, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(SharedString::from(seg)),
                );
                i = j;
            }
            list = list.child(
                div()
                    .mx(px(6.0))
                    .px(px(10.0))
                    .h(px(30.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .rounded(px(5.0))
                    .text_size(px(13.0))
                    .when(sel, |d| d.bg(alpha(th.accent, 0.20)))
                    .child(title)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(hsla(th.dim))
                            .child(SharedString::from(it.shortcut.clone())),
                    ),
            );
        }

        let panel = div()
            .w(px(width))
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.98))
            .shadow_lg()
            .text_color(hsla(th.fg))
            .overflow_hidden()
            .child(input)
            .child(list)
            .with_animation(
                "palette-in",
                Animation::new(Duration::from_millis(80)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t).mt(px(-8.0 * (1.0 - t))),
            );

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TITLEBAR_H + 36.0))
                .occlude()
                .child(panel),
        )
    }
}

impl AppShell {
    /// A program asks to set the clipboard and the machine's policy is `ask`. The banner does not
    /// take the keyboard and does not cover the terminal, only its own corner. It shows the size,
    /// the line count and the first and last lines with the line breaks visible, and warns when
    /// pasting would end with Enter. The buttons carry the prompt's nonce and are dim (and do
    /// nothing) for the first 500 ms.
    fn render_clip_prompt(&self, cx: &mut Context<Self>) -> Option<Div> {
        let p = self.clip_prompt.as_ref()?;
        let th = self.theme;
        let name = self.machine_name(p.machine);
        let nonce = p.nonce;
        let live = p.shown_at.elapsed() >= crate::clipboard::CLICK_GUARD;
        let pv = &p.preview;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px(px(12.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .text_size(px(12.5))
                .border_1()
                .border_color(alpha(th.border, 1.0))
                .when(!live, |b| b.opacity(0.45))
                .when(live, |b| {
                    b.cursor_pointer()
                        .when(primary, |b| b.bg(alpha(th.accent, 0.25)).border_color(hsla(th.accent)))
                        .hover(move |s| s.bg(alpha(th.hover, 1.0)))
                })
                .child(label)
        };
        let mono = |t: String| {
            div().text_size(px(12.0)).font_family("Cascadia Mono").text_color(hsla(th.fg)).truncate().child(SharedString::from(t))
        };
        let mut panel = div()
            .absolute()
            .bottom(px(16.0))
            .right(px(16.0))
            .w(px(420.0).min(px(self.viewport.0 - 32.0)))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.98))
            .shadow_lg()
            .text_color(hsla(th.fg))
            .child(div().text_size(px(13.0)).child(SharedString::from(format!(
                "A program on {name} wants to copy {} line{} ({} bytes) to your clipboard.",
                pv.lines,
                if pv.lines == 1 { "" } else { "s" },
                pv.bytes
            ))));
        for l in &pv.head {
            panel = panel.child(mono(l.clone()));
        }
        if pv.hidden > 0 {
            panel = panel.child(div().text_size(px(12.0)).text_color(hsla(th.dim)).child(SharedString::from(format!("... {} more lines ...", pv.hidden))));
        }
        for l in &pv.tail {
            panel = panel.child(mono(l.clone()));
        }
        if pv.ends_with_enter {
            panel = panel.child(
                div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(hsla(th.accent)).child("Ends with Enter: pasting it runs a command"),
            );
        }
        Some(
            panel
                .child(div().text_size(px(12.0)).text_color(hsla(th.dim)).child("Your answer is kept for this machine."))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .justify_end()
                        .child(button("clip-deny", "Deny", false).on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.clip_answer(false, nonce, cx))))
                        .child(button("clip-allow", "Allow", true).on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.clip_answer(true, nonce, cx)))),
                ),
        )
    }

    /// The pairing dialog: paste, confirm (machine, node id, relay), pair.
    fn render_pair(&self, cx: &mut Context<Self>) -> Option<Div> {
        let d = self.pair.as_ref()?;
        let th = self.theme;
        let line = |t: String, strong: bool| {
            div()
                .text_size(px(13.0))
                .text_color(hsla(if strong { th.fg } else { th.dim }))
                .child(SharedString::from(t))
        };
        let row = |k: &str, v: String| {
            div()
                .flex()
                .flex_row()
                .gap(px(10.0))
                .child(div().w(px(76.0)).flex_none().text_size(px(12.5)).text_color(hsla(th.dim)).child(SharedString::from(k.to_string())))
                .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).font_family("Cascadia Mono").child(SharedString::from(v)))
        };
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .h(px(30.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .text_size(px(13.0))
                .border_1()
                .border_color(alpha(th.border, 1.0))
                .when(primary, |b| b.bg(alpha(th.accent, 0.25)).border_color(hsla(th.accent)))
                .hover(move |s| s.bg(alpha(th.hover, 1.0)))
                .child(label)
        };
        let mut body = div().flex().flex_col().gap(px(10.0)).p(px(18.0));
        body = body.child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child("Pair machine"));
        match &d.stage {
            crate::machines::PairStage::Input => {
                body = body
                    .child(line("Run `forgetty pair` on the other machine, copy the text it prints and paste it here.".into(), false))
                    .child(
                        div()
                            .h(px(34.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(alpha(th.border, 1.0))
                            .text_color(hsla(th.dim))
                            .text_size(px(12.5))
                            .child("Copy the text and it is offered here, or press Ctrl+V"),
                    )
                    .children(d.prefill.as_ref().map(|(_, o)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .p(px(10.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(hsla(th.accent))
                            .child(line("Found on the clipboard:".into(), true))
                            .child(row("Machine", o.machine.clone()))
                            .child(row("Node ID", format!("{}...", o.node_short)))
                            .child(
                                div().flex().flex_row().justify_end().child(
                                    button("pair-use", "Pair", true).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.pair_use_prefill(cx))),
                                ),
                            )
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .justify_end()
                            .child(button("pair-cancel", "Cancel", false).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.pair_cancel(cx)))),
                    );
            }
            crate::machines::PairStage::Confirm(o) => {
                body = body
                    .child(line("Pair with this machine?".into(), true))
                    .child(row("Machine", o.machine.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(10.0))
                            .child(div().w(px(76.0)).flex_none().text_size(px(12.5)).text_color(hsla(th.dim)).child("Node ID"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(13.0))
                                    .font_family("Cascadia Mono")
                                    .child(SharedString::from(o.node_grouped.clone())),
                            ),
                    )
                    .child(line("Compare the Node ID with `forgetty pair` on the server. If it differs, cancel.".into(), true))
                    .child(row("Relay", if o.relay.is_empty() { if o.relays_disabled { "none (WireGuard only)".to_string() } else { "none".to_string() } } else { o.relay.clone() }))
                    .when(!o.direct.is_empty(), |b| {
                        b.child(row("Address", o.direct.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")))
                    })
                    .child(line("Pairing gives this PC full access to the sessions on that machine.".into(), false))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .justify_end()
                            .child(button("pair-cancel", "Cancel", false).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.pair_cancel(cx))))
                            .child(button("pair-confirm", "Confirm", true).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.pair_confirm(cx)))),
                    );
            }
            crate::machines::PairStage::Working(o) => {
                body = body.child(line(format!("Pairing with {}...", o.machine), true));
            }
            crate::machines::PairStage::Failed(msg) => {
                body = body.child(line(msg.clone(), true)).child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .justify_end()
                        .child(button("pair-cancel", "Close", false).on_mouse_down(MouseButton::Left, cx.listener(|s, _e, _w, cx| s.pair_cancel(cx)))),
                );
            }
        }
        let panel = div()
            .w(px(520.0).min(px(self.viewport.0 - 40.0)))
            .rounded(px(10.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.98))
            .shadow_lg()
            .text_color(hsla(th.fg))
            .child(body);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TITLEBAR_H + 60.0))
                .occlude()
                .child(panel),
        )
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.smoke_mode {
            let now = std::time::Instant::now();
            if let Some(end) = self.last_paint_end.take() {
                if self.idle_ms.len() < 20_000 {
                    self.idle_ms.push(now.duration_since(end).as_secs_f32() * 1000.0);
                }
            }
            self.render_t0 = Some(now);
        }
        let vp = window.viewport_size();
        self.viewport = (f32::from(vp.width), f32::from(vp.height));
        if self.take_sync_dirty() {
            self.sync(window, cx);
        }
        let th = self.theme;
        let entity = cx.entity();
        let mut body = div().flex().flex_row().flex_1().min_h_0().w_full();
        if self.sidebar_open {
            body = body.child(self.render_sidebar(cx));
        }
        body = body.child(self.render_panes(cx));

        let out = div()
            .size_full()
            .flex()
            .flex_col()
            .font_family("Segoe UI")
            .text_color(hsla(th.fg))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|s, ev: &gpui::KeyDownEvent, w, cx| {
                if s.handle_key(&ev.keystroke, w, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(self.render_titlebar(window, cx))
            .child(body)
            .children(self.render_palette())
            .children(self.render_pair(cx))
            .children(self.render_clip_prompt(cx))
            .children(self.render_menu(cx))
            .children(self.render_confirm(cx))
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        entity.update(cx, |s, cx| s.after_paint(window, cx));
                    },
                )
                .absolute()
                .w(px(1.0))
                .h(px(1.0)),
            );
        out
    }
}
