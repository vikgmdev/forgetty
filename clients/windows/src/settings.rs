//! The Settings window and the shortcuts window (F1). All state and all key handling live in
//! `forgetty_shell_core::settings_ui` (tested on Linux); this file draws a [`View`] and turns an
//! [`Effect`] into work: a write of `config.toml` on the background executor, then the same live
//! reload as any other edit of the file.

use forgetty_shell_core::settings::{self, Page};
use forgetty_shell_core::settings_file;
use forgetty_shell_core::settings_ui::{Body, Control, Ctx, Effect, KeyLineView, Settings, View, Wrote, Zone};
use forgetty_shell_core::shortcuts;
use forgetty_shell_core::textarea::KeyIn;
use gpui::{div, prelude::*, px, ClipboardItem, Context, Div, FontWeight, Keystroke, MouseButton, ScrollDelta, ScrollWheelEvent, SharedString, Stateful, Window};

use crate::shell::*;
use crate::view::hsla;

fn alpha(hex: u32, a: f32) -> gpui::Hsla {
    let mut c = hsla(hex);
    c.a = a;
    c
}

const BAD: u32 = 0xff6b68;
const GOOD: u32 = 0x6ccb5f;
const WARN: u32 = 0xe5c07b;

pub struct SettingsWin {
    pub st: Settings,
    pub fonts: Vec<String>,
}

pub struct ShortcutsWin {
    pub top: usize,
}

enum Job {
    Edits(Vec<settings_file::Edit>),
    Theme(String),
    Font(Option<String>, Option<f32>),
    Text(String, String),
}

fn config_path_text() -> String {
    crate::config::config_path().map(|p| p.display().to_string()).unwrap_or_else(|| "(no configuration folder)".into())
}

impl AppShell {
    // ---- opening and closing ----------------------------------------------------------

    pub fn open_settings(&mut self, page: Option<Page>, cx: &mut Context<Self>) {
        self.palette = None;
        self.menu = None;
        self.rename = None;
        self.shortcuts = None;
        if self.settings.is_some() {
            if let Some(p) = page {
                self.settings_page(p, cx);
            }
            return;
        }
        // Listing every installed font takes a moment: the window opens at once and the list
        // fills in when the background thread is done.
        let loading = self.font_names.is_none();
        if loading {
            let ts = cx.text_system().clone();
            cx.spawn(async move |this, cx| {
                let names = cx
                    .background_executor()
                    .spawn(async move {
                        let mut names = ts.all_font_names();
                        names.retain(|n| !n.starts_with('@') && !n.is_empty());
                        names.sort_by_key(|n| n.to_lowercase());
                        names.dedup();
                        names
                    })
                    .await;
                let _ = this.update(cx, |s, cx| s.fonts_loaded(names, cx));
            })
            .detach();
        }
        let fonts = self.font_names.clone().unwrap_or_default();
        let idx = self.themes.clone();
        let st = {
            let names: &[String] = idx.as_ref().map(|t| t.names.as_slice()).unwrap_or(&[]);
            let colors = |n: &str| idx.as_ref().and_then(|t| t.colors(n));
            let path = config_path_text();
            let ctx = Ctx { cfg: &self.file_cfg, themes: names, fonts: &fonts, colors: &colors, config_path: &path };
            let mut st = Settings::new(&ctx);
            st.set_fonts_loading(loading);
            st
        };
        self.settings = Some(Box::new(SettingsWin { st, fonts }));
        if let Some(p) = page {
            self.settings_page(p, cx);
        }
        cx.notify();
    }

    fn fonts_loaded(&mut self, names: Vec<String>, cx: &mut Context<Self>) {
        self.font_names = Some(names.clone());
        if let Some(w) = self.settings.as_mut() {
            w.fonts = names;
        }
        self.with_settings(|st, ctx| st.fonts_changed(ctx));
        cx.notify();
    }

    pub fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.take().is_some() {
            // The keyboard goes back to the terminal.
            self.on_active_view(window, cx, |_, _| {});
            cx.notify();
        }
    }

    /// Run `f` on the state with a [`Ctx`] built from the app. The state is out of `self` while
    /// `f` runs, so `f` cannot call back into the shell.
    fn with_settings<R>(&mut self, f: impl FnOnce(&mut Settings, &Ctx) -> R) -> Option<R> {
        let mut win = self.settings.take()?;
        let idx = self.themes.clone();
        let names: &[String] = idx.as_ref().map(|t| t.names.as_slice()).unwrap_or(&[]);
        let colors = |n: &str| idx.as_ref().and_then(|t| t.colors(n));
        let path = config_path_text();
        let r = {
            let SettingsWin { st, fonts } = &mut *win;
            let ctx = Ctx { cfg: &self.file_cfg, themes: names, fonts: fonts.as_slice(), colors: &colors, config_path: &path };
            f(st, &ctx)
        };
        self.settings = Some(win);
        Some(r)
    }

    /// The file was read again: the window shows what it says.
    pub fn settings_config_changed(&mut self, cx: &mut Context<Self>) {
        if self.with_settings(|st, ctx| st.config_changed(ctx)) == Some(Effect::ProbeEditor) {
            self.settings_probe_editor(cx);
        }
    }

    /// Read the file on the background executor and let the editor follow it.
    fn settings_probe_editor(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else { return };
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { settings_file::read_for_editor(&path) }).await;
            let _ = this.update(cx, |s, cx| {
                if let Some(w) = s.settings.as_mut() {
                    w.st.editor_probe(r);
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- input ------------------------------------------------------------------------

    pub fn settings_key(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ks.modifiers;
        if ks.key == "f1" && !m.control && !m.alt && !m.shift {
            self.open_shortcuts(cx);
            return;
        }
        let k = KeyIn { key: ks.key.as_str(), ctrl: m.control, shift: m.shift, alt: m.alt, win: m.platform, text: ks.key_char.as_deref() };
        let Some(e) = self.with_settings(|st, ctx| st.key(&k, ctx)) else { return };
        self.settings_effect(e, window, cx);
        cx.notify();
    }

    pub fn settings_press(&mut self, name: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.with_settings(|st, ctx| st.press(name, ctx)) else { return };
        self.settings_effect(e, window, cx);
        cx.notify();
    }

    pub fn settings_page(&mut self, p: Page, cx: &mut Context<Self>) {
        let Some(e) = self.with_settings(|st, ctx| st.select_page(p, ctx)) else { return };
        if e == Effect::LoadEditor {
            self.settings_load_editor(cx);
        }
        cx.notify();
    }

    pub fn settings_select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.with_settings(|st, ctx| st.click_row(index, ctx));
        cx.notify();
    }

    pub fn settings_look(&mut self, zone: Zone, index: Option<usize>, cx: &mut Context<Self>) {
        self.with_settings(|st, _| st.click_look(zone, index));
        cx.notify();
    }

    pub fn settings_wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let dy = match ev.delta {
            ScrollDelta::Pixels(p) => f32::from(p.y) / 24.0,
            ScrollDelta::Lines(l) => l.y * 3.0,
        };
        let rows = (-dy).round() as isize;
        if rows == 0 {
            return;
        }
        self.with_settings(|st, ctx| st.scroll(rows, ctx));
        cx.notify();
    }

    fn settings_effect(&mut self, e: Effect, window: &mut Window, cx: &mut Context<Self>) {
        match e {
            Effect::None => {}
            Effect::Close => self.close_settings(window, cx),
            Effect::Edits(edits) => self.settings_write(Job::Edits(edits), cx),
            Effect::Theme(name) => {
                // A theme picked for one session in the palette gives way to the one in the file.
                self.theme_override = None;
                self.settings_write(Job::Theme(name), cx);
            }
            Effect::Font { family, size } => self.settings_write(Job::Font(family, size), cx),
            Effect::SaveText { text, expect } => self.settings_write(Job::Text(text, expect), cx),
            Effect::ProbeEditor => self.settings_probe_editor(cx),
            Effect::LoadEditor => self.settings_load_editor(cx),
            Effect::OpenConfigFile => self.open_config_file(cx),
            Effect::PasteIntoEditor => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(w) = self.settings.as_mut() {
                        w.st.paste(&t);
                    }
                }
            }
            Effect::CopyText(t) => cx.write_to_clipboard(ClipboardItem::new_string(t)),
        }
    }

    /// Write the file on the background executor. The window waits (`busy`) until it is done.
    fn settings_write(&mut self, job: Job, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else {
            self.notice_active("No configuration folder (APPDATA is not set)".into(), cx);
            return;
        };
        let what = if matches!(job, Job::Text(..)) { Wrote::Editor } else { Wrote::Setting };
        if let Some(w) = self.settings.as_mut() {
            w.st.busy = true;
        }
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match job {
                        Job::Edits(e) => settings_file::save_edits(&path, &e),
                        Job::Theme(n) => settings_file::save_with(&path, |t| settings::theme_edits(t, &n)),
                        Job::Font(f, s) => settings_file::save_with(&path, |t| Ok(settings::font_edits(t, f.as_deref(), s))),
                        Job::Text(t, expect) => settings_file::save_text(&path, &t, &expect),
                    }
                })
                .await;
            let _ = this.update(cx, |s, cx| s.settings_saved(what, result, cx));
        })
        .detach();
    }

    fn settings_saved(&mut self, what: Wrote, result: Result<settings_file::Saved, String>, cx: &mut Context<Self>) {
        let ok = result.is_ok();
        let r = result.map(|s| s.warnings);
        if self.settings.is_some() {
            if let Some(w) = self.settings.as_mut() {
                w.st.saved(what, r);
            }
        } else if let Err(e) = r {
            self.notice_active(e, cx);
        }
        if ok {
            // The watcher would see the write too. Asking says "Configuration reloaded" at once.
            self.reload_config_now(cx);
        }
        cx.notify();
    }

    fn settings_load_editor(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else {
            if let Some(w) = self.settings.as_mut() {
                w.st.editor_loaded(Err("No configuration folder (APPDATA is not set)".into()));
            }
            return;
        };
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { settings_file::read_for_editor(&path) }).await;
            let _ = this.update(cx, |s, cx| {
                if let Some(w) = s.settings.as_mut() {
                    w.st.editor_loaded(r);
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- the shortcuts window -----------------------------------------------------------

    pub fn open_shortcuts(&mut self, cx: &mut Context<Self>) {
        self.palette = None;
        self.menu = None;
        self.rename = None;
        self.shortcuts = Some(ShortcutsWin { top: 0 });
        cx.notify();
    }

    fn shortcut_lines(&self) -> Vec<(bool, String, String)> {
        let mut out = Vec::new();
        for g in shortcuts::groups(self.cfg.keymap, &self.cfg.keybindings) {
            out.push((true, g.title.to_string(), String::new()));
            for (t, k) in g.items {
                out.push((false, t, k));
            }
        }
        out
    }

    fn shortcuts_visible(&self) -> usize {
        (((self.viewport.1 - TITLEBAR_H - 150.0) / 26.0).floor() as usize).clamp(4, 40)
    }

    pub fn shortcuts_key(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.shortcut_lines().len();
        let vis = self.shortcuts_visible();
        let Some(w) = self.shortcuts.as_mut() else { return };
        let max = n.saturating_sub(vis);
        match ks.key.as_str() {
            "escape" | "enter" | "f1" => {
                self.shortcuts = None;
                self.on_active_view(window, cx, |_, _| {});
            }
            "up" => w.top = w.top.saturating_sub(1),
            "down" => w.top = (w.top + 1).min(max),
            "pageup" => w.top = w.top.saturating_sub(vis),
            "pagedown" => w.top = (w.top + vis).min(max),
            "home" => w.top = 0,
            "end" => w.top = max,
            _ => {}
        }
        cx.notify();
    }

    pub fn shortcuts_wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let n = self.shortcut_lines().len();
        let max = n.saturating_sub(self.shortcuts_visible());
        let dy = match ev.delta {
            ScrollDelta::Pixels(p) => f32::from(p.y) / 24.0,
            ScrollDelta::Lines(l) => l.y * 3.0,
        };
        if let Some(w) = self.shortcuts.as_mut() {
            w.top = ((w.top as isize) + (-dy).round() as isize).clamp(0, max as isize) as usize;
        }
        cx.notify();
    }

    pub fn close_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts = None;
        self.on_active_view(window, cx, |_, _| {});
        cx.notify();
    }

    // ---- drawing ------------------------------------------------------------------------

    /// Called by `render` before the tree is built: sizes the pages to the window and returns what
    /// to draw.
    pub fn settings_prepare(&mut self) -> Option<View> {
        let h = self.viewport.1 - TITLEBAR_H;
        self.with_settings(|st, ctx| {
            let n = match st.page {
                Page::General | Page::Terminal => ((h - 150.0) / 64.0).floor(),
                Page::Keybindings => ((h - 190.0) / 30.0).floor(),
                Page::Appearance => ((h - 190.0) / 26.0).floor().min(14.0),
                Page::Advanced => ((h - 190.0) / 19.0).floor(),
                Page::About => 6.0,
            };
            st.set_visible(n.max(3.0) as usize);
            st.view(ctx)
        })
    }

    pub fn render_settings(&self, v: View, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let w = self.viewport.0;
        let h = self.viewport.1 - TITLEBAR_H;
        let nav_w = 190.0f32.min(w * 0.3);

        let mut nav = div()
            .w(px(nav_w))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(10.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.3), 1.0))
            .border_r_1()
            .border_color(alpha(th.border, 1.0))
            .child(div().px(px(10.0)).pb(px(8.0)).text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child("Settings"));
        for (i, p) in Page::ALL.into_iter().enumerate() {
            let on = p == v.page;
            nav = nav.child(
                div()
                    .id(("settings-nav", i))
                    .px(px(10.0))
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .when(on, |d| d.bg(alpha(th.accent, 0.20)).font_weight(FontWeight::SEMIBOLD))
                    .hover(move |s| s.bg(alpha(th.hover, 1.0)))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_page(p, cx)))
                    .child(p.title()),
            );
        }
        nav = nav.child(div().flex_1()).child(self.sbtn(("settings-close", 0), "Close (Esc)", false, cx, |s, w, cx| s.settings_press("close", w, cx)));

        let status = v.status.as_ref().map(|s| {
            div().text_size(px(12.0)).text_color(hsla(if s.ok { GOOD } else { BAD })).child(SharedString::from(forgetty_shell_core::notify::notice_text(&s.text)))
        });
        let body: Div = match v.body {
            Body::Rows(rows) => self.settings_rows(rows, cx),
            Body::Keys(k) => self.settings_keys(k, cx),
            Body::Look(l) => self.settings_look_page(l, cx),
            Body::Toml(t) => self.settings_toml(t, cx),
            Body::About(a) => self.settings_about(a, cx),
        };
        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(18.0))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(div().text_size(px(17.0)).font_weight(FontWeight::SEMIBOLD).child(v.page.title()))
                    .children(if v.busy { Some(div().text_size(px(12.0)).text_color(hsla(th.dim)).child("Saving...")) } else { None })
                    .children(status),
            )
            .child(body)
            .child(div().flex_none().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(v.hint)));

        div()
            .absolute()
            .top(px(TITLEBAR_H))
            .left_0()
            .w_full()
            .h(px(h))
            .occlude()
            .flex()
            .flex_row()
            .bg(alpha(th.bg, 1.0))
            .text_color(hsla(th.fg))
            .text_size(px(13.0))
            .on_scroll_wheel(cx.listener(|s, ev: &ScrollWheelEvent, _w, cx| s.settings_wheel(ev, cx)))
            .child(nav)
            .child(main)
    }

    /// A button. `f` runs on a left click.
    fn sbtn(
        &self,
        id: (&'static str, usize),
        label: &str,
        primary: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut AppShell, &mut Window, &mut Context<AppShell>) + 'static,
    ) -> Stateful<Div> {
        let th = self.theme;
        div()
            .id(id)
            .px(px(12.0))
            .h(px(28.0))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .text_size(px(12.5))
            .border_1()
            .border_color(if primary { hsla(th.accent) } else { alpha(th.border, 1.0) })
            .when(primary, |d| d.bg(alpha(th.accent, 0.25)))
            .hover(move |s| s.bg(alpha(th.hover, 1.0)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, w, cx| f(s, w, cx)))
            .child(SharedString::from(label.to_string()))
    }

    fn settings_rows(&self, rows: Vec<forgetty_shell_core::settings_ui::RowView>, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let mut col = div().flex_1().min_h_0().flex().flex_col().gap(px(6.0)).overflow_hidden();
        for r in rows {
            if let Some(h) = r.header {
                col = col.child(div().pt(px(6.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(hsla(th.dim)).child(SharedString::from(h.to_uppercase())));
            }
            let idx = r.index;
            let mut right = div().flex().flex_row().items_center().gap(px(6.0)).flex_none();
            if let Some(e) = &r.entry {
                right = right
                    .child(
                        div()
                            .min_w(px(180.0))
                            .h(px(28.0))
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(hsla(th.accent))
                            .font_family("Cascadia Mono")
                            .child(SharedString::from(e.before.replace(' ', "\u{00a0}")))
                            .child(div().w(px(1.0)).h(px(16.0)).bg(hsla(th.accent)))
                            .child(SharedString::from(e.after.replace(' ', "\u{00a0}"))),
                    )
                    .child(self.sbtn(("entry-ok", idx), "Save", true, cx, |s, w, cx| s.settings_press("entry-ok", w, cx)))
                    .child(self.sbtn(("entry-cancel", idx), "Cancel", false, cx, |s, w, cx| s.settings_press("entry-cancel", w, cx)));
            } else {
                match r.control {
                    Control::Toggle(on) => {
                        right = right.child(self.sbtn(("row-toggle", idx), if on { "On" } else { "Off" }, on, cx, move |s, w, cx| {
                            s.settings_select(idx, cx);
                            s.settings_press("row-toggle", w, cx)
                        }));
                    }
                    Control::Choice => {
                        right = right
                            .child(self.sbtn(("row-left", idx), "<", false, cx, move |s, w, cx| {
                                s.settings_select(idx, cx);
                                s.settings_press("row-left", w, cx)
                            }))
                            .child(div().min_w(px(150.0)).flex().justify_center().child(SharedString::from(r.value.clone())))
                            .child(self.sbtn(("row-right", idx), ">", false, cx, move |s, w, cx| {
                                s.settings_select(idx, cx);
                                s.settings_press("row-right", w, cx)
                            }));
                    }
                    Control::Number => {
                        right = right
                            .child(self.sbtn(("row-left", idx), "-", false, cx, move |s, w, cx| {
                                s.settings_select(idx, cx);
                                s.settings_press("row-left", w, cx)
                            }))
                            .child(self.sbtn(("row-edit", idx), &r.value, false, cx, move |s, w, cx| {
                                s.settings_select(idx, cx);
                                s.settings_press("row-edit", w, cx)
                            }))
                            .child(self.sbtn(("row-right", idx), "+", false, cx, move |s, w, cx| {
                                s.settings_select(idx, cx);
                                s.settings_press("row-right", w, cx)
                            }));
                    }
                    Control::Text => {
                        let shown = if r.value.is_empty() { r.placeholder.to_string() } else { r.value.clone() };
                        right = right.child(self.sbtn(("row-edit", idx), &shown, false, cx, move |s, w, cx| {
                            s.settings_select(idx, cx);
                            s.settings_press("row-edit", w, cx)
                        }));
                    }
                }
                if r.changed {
                    right = right.child(self.sbtn(("row-reset", idx), "Default", false, cx, move |s, w, cx| {
                        s.settings_select(idx, cx);
                        s.settings_press("row-reset", w, cx)
                    }));
                }
            }
            let mut left = div().flex_1().min_w_0().flex().flex_col().gap(px(2.0)).child(div().child(r.label)).child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(r.help));
            if !r.applies.is_empty() {
                left = left.child(div().text_size(px(11.0)).text_color(hsla(th.dim)).child(SharedString::from(format!("Applies to {}", r.applies))));
            }
            if let Some(err) = r.entry.as_ref().and_then(|e| e.error.clone()) {
                left = left.child(div().text_size(px(11.5)).text_color(hsla(BAD)).child(SharedString::from(err)));
            }
            col = col.child(
                div()
                    .id(("settings-row", idx))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(12.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if r.selected { hsla(th.accent) } else { alpha(th.border, 0.6) })
                    .when(r.selected, |d| d.bg(alpha(th.accent, 0.08)))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_select(idx, cx)))
                    .child(left)
                    .child(right),
            );
        }
        col
    }

    fn settings_keys(&self, k: forgetty_shell_core::settings_ui::KeysView, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let filter = div()
            .h(px(32.0))
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .child(if k.filter.is_empty() {
                div().text_color(hsla(th.dim)).child("Type to search the shortcuts")
            } else {
                div().flex().flex_row().items_center().child(SharedString::from(k.filter.replace(' ', "\u{00a0}"))).child(div().w(px(1.0)).h(px(16.0)).bg(hsla(th.accent)))
            })
            .child(div().flex_1())
            .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(format!("{} shortcuts", k.total_rows))));
        let mut col = div().flex_1().min_h_0().flex().flex_col().gap(px(8.0)).overflow_hidden();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(div().flex_1().child(filter))
                .child(self.sbtn(("key-reset-all", 0), "Reset all", false, cx, |s, w, cx| s.settings_press("key-reset-all", w, cx))),
        );
        if let Some((title, prompt)) = &k.capture {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(hsla(th.accent))
                    .bg(alpha(th.accent, 0.10))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(title.clone())))
                    .child(div().child(SharedString::from(prompt.clone())))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .pt(px(4.0))
                            .child(self.sbtn(("key-move", 0), "Enter: move it here", false, cx, |s, w, cx| s.settings_press("key-move", w, cx)))
                            .child(self.sbtn(("key-cancel", 0), "Cancel", false, cx, |s, w, cx| s.settings_press("key-cancel", w, cx))),
                    ),
            );
        }
        for line in k.lines {
            match line {
                KeyLineView::Header(h) => {
                    col = col.child(div().pt(px(4.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(hsla(th.dim)).child(SharedString::from(h.to_uppercase())));
                }
                KeyLineView::Row { index, title, chords, customized, selected } => {
                    let mut chips = div().flex().flex_row().gap(px(6.0)).flex_none();
                    if chords.is_empty() {
                        chips = chips.child(div().text_color(hsla(th.dim)).child("(none)"));
                    }
                    for c in chords {
                        chips = chips.child(
                            div().px(px(7.0)).py(px(1.0)).rounded(px(4.0)).border_1().border_color(alpha(th.border, 1.0)).text_size(px(12.0)).font_family("Cascadia Mono").child(SharedString::from(c)),
                        );
                    }
                    let mut row = div()
                        .id(("key-row", index))
                        .h(px(28.0))
                        .px(px(10.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .when(selected, |d| d.bg(alpha(th.accent, 0.16)))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_select(index, cx)))
                        .child(div().flex_1().min_w_0().truncate().child(SharedString::from(title)))
                        .children(if customized { Some(div().text_size(px(11.0)).text_color(hsla(th.accent)).child("changed")) } else { None })
                        .child(chips);
                    if selected {
                        row = row
                            .child(self.sbtn(("key-change", index), "Change", true, cx, |s, w, cx| s.settings_press("key-change", w, cx)))
                            .child(self.sbtn(("key-add", index), "Add", false, cx, |s, w, cx| s.settings_press("key-add", w, cx)))
                            .child(self.sbtn(("key-clear", index), "Unbind", false, cx, |s, w, cx| s.settings_press("key-clear", w, cx)))
                            .child(self.sbtn(("key-reset", index), "Default", false, cx, |s, w, cx| s.settings_press("key-reset", w, cx)));
                    }
                    col = col.child(row);
                }
            }
        }
        col
    }

    fn settings_look_page(&self, l: forgetty_shell_core::settings_ui::LookView, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let list = |title: &str, lv: forgetty_shell_core::settings_ui::ListView, zone: Zone, tag: &'static str, cx: &mut Context<Self>| -> Div {
            let mut col = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .p(px(8.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(if lv.focused { hsla(th.accent) } else { alpha(th.border, 1.0) })
                .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_look(zone, None, cx)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(title.to_string())))
                        .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(format!("{}", lv.total)))),
                )
                .child(
                    div()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(4.0))
                        .bg(alpha(th.hover, 0.6))
                        .text_size(px(12.0))
                        .child(if lv.filter.is_empty() { div().text_color(hsla(th.dim)).child("Type to filter") } else { div().child(SharedString::from(lv.filter.clone())) }),
                );
            for (i, (name, hi, current)) in lv.items.into_iter().enumerate() {
                let label = if current { format!("{name}  (current)") } else { name.clone() };
                col = col.child(
                    div()
                        .id((tag, i))
                        .h(px(24.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .text_size(px(12.5))
                        .truncate()
                        .when(hi, |d| d.bg(alpha(th.accent, 0.22)))
                        .hover(move |s| s.bg(alpha(th.hover, 1.0)))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_look_pick(zone, name.clone(), cx)))
                        .child(SharedString::from(label)),
                );
            }
            col
        };
        let themes = list("Theme", l.themes.clone(), Zone::Theme, "look-theme", cx);
        let fonts = list("Font", l.fonts.clone(), Zone::Font, "look-font", cx);

        // The preview: the colors of the highlighted theme in the highlighted font.
        let (bg, fg, ansi) = match &l.preview_colors {
            Some(c) => (c.background, c.foreground, c.ansi),
            None => (self.cfg.background, self.cfg.foreground, self.cfg.ansi),
        };
        let px_size = (l.size * 96.0 / 72.0).min(30.0);
        let fam = SharedString::from(l.preview_family.clone());
        let line = |parts: Vec<(&'static str, u32)>| {
            let mut d = div().flex().flex_row().whitespace_nowrap();
            for (t, c) in parts {
                d = d.child(div().text_color(hsla(c)).child(t));
            }
            d
        };
        let preview = div()
            .flex_none()
            .w(px(300.0f32.min(self.viewport.0 * 0.3)))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Preview"))
            .child(
                div()
                    .p(px(12.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(alpha(th.border, 1.0))
                    .bg(hsla(bg))
                    .font_family(fam)
                    .text_size(px(px_size))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(line(vec![("PS C:\\> ", ansi[2]), ("git status", fg)]))
                    .child(line(vec![("On branch ", fg), ("main", ansi[6])]))
                    .child(line(vec![("modified:  ", ansi[1]), ("src/shell.rs", fg)]))
                    .child(line(vec![("new file:  ", ansi[2]), ("README.md", fg)]))
                    .child(line(vec![("warning: ", ansi[3]), ("0O 1lI {} []", fg)]))
                    .child(line(vec![("info ", ansi[4]), ("42 ", ansi[5]), ("ms", fg)])),
            )
            .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(format!("{} / {} / {} pt{}", l.preview_theme, l.preview_family, trim_pt(l.size), if l.fonts_loading { "   (reading the installed fonts...)" } else { "" }))))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .p(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if l.size_focused { hsla(th.accent) } else { alpha(th.border, 1.0) })
                    .child(div().child("Size"))
                    .child(self.sbtn(("size-minus", 0), "-", false, cx, |s, w, cx| s.settings_press("size-minus", w, cx)))
                    .child(div().min_w(px(50.0)).flex().justify_center().child(SharedString::from(format!("{} pt", trim_pt(l.size)))))
                    .child(self.sbtn(("size-plus", 0), "+", false, cx, |s, w, cx| s.settings_press("size-plus", w, cx)))
                    .child(div().flex_1())
                    .child(self.sbtn(("look-apply-size", 0), "Apply", false, cx, |s, w, cx| {
                        s.settings_look(Zone::Size, None, cx);
                        s.settings_press("look-apply", w, cx)
                    })),
            )
            .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(SharedString::from(format!("In use: {} / {} / {} pt", if l.current_theme.is_empty() { "(default)" } else { l.current_theme.as_str() }, l.current_family, trim_pt(l.current_size)))))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .child(self.sbtn(("look-apply-theme", 0), "Apply theme", true, cx, |s, w, cx| {
                        s.settings_look(Zone::Theme, None, cx);
                        s.settings_press("look-apply", w, cx)
                    }))
                    .child(self.sbtn(("look-apply-font", 0), "Apply font", true, cx, |s, w, cx| {
                        s.settings_look(Zone::Font, None, cx);
                        s.settings_press("look-apply", w, cx)
                    })),
            )
            .child(self.sbtn(("look-mono", 0), if l.mono_only { "Showing monospace fonts. Show all" } else { "Showing all fonts. Monospace only" }, false, cx, |s, w, cx| s.settings_press("look-mono", w, cx)));
        div().flex_1().min_h_0().flex().flex_row().gap(px(10.0)).overflow_hidden().child(themes).child(fonts).child(preview)
    }

    /// A click on a theme or font in the list: highlight it (the preview follows).
    pub fn settings_look_pick(&mut self, zone: Zone, name: String, cx: &mut Context<Self>) {
        self.with_settings(|st, ctx| st.pick_look(zone, &name, ctx));
        cx.notify();
    }

    fn settings_toml(&self, t: forgetty_shell_core::settings_ui::TomlView, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let bar = div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .items_center()
            .child(self.sbtn(("toml-save", 0), "Save", true, cx, |s, w, cx| s.settings_press("toml-save", w, cx)))
            .child(self.sbtn(("toml-revert", 0), "Read the file again", false, cx, |s, w, cx| s.settings_press("toml-revert", w, cx)))
            .child(self.sbtn(("toml-paste", 0), "Paste", false, cx, |s, w, cx| s.settings_press("toml-paste", w, cx)))
            .child(self.sbtn(("toml-copy", 0), "Copy all", false, cx, |s, w, cx| s.settings_press("toml-copy", w, cx)))
            .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(if t.dirty { "Not saved" } else { "" }));
        let mut editor = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .p(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.bg, th.chrome, 0.35), 1.0))
            .font_family("Cascadia Mono")
            .text_size(px(12.5))
            .overflow_hidden();
        if !t.loaded {
            editor = editor.child(div().text_color(hsla(th.dim)).child(SharedString::from(t.message.clone().unwrap_or_else(|| "Reading the file...".into()))));
        }
        for (n, text) in t.lines {
            let row = n - 1;
            let bad = t.error_line == Some(n);
            let caret = t.caret.filter(|(r, _)| *r == row).map(|(_, c)| c);
            let body: Div = match caret {
                Some(c) => {
                    let at = text.char_indices().nth(c).map(|x| x.0).unwrap_or(text.len());
                    div()
                        .flex()
                        .flex_row()
                        .child(SharedString::from(text[..at].replace(' ', "\u{00a0}")))
                        .child(div().w(px(1.5)).h(px(16.0)).bg(hsla(th.accent)))
                        .child(SharedString::from(text[at..].replace(' ', "\u{00a0}")))
                }
                None => div().child(SharedString::from(text.replace(' ', "\u{00a0}"))),
            };
            editor = editor.child(
                div()
                    .id(("toml-line", row))
                    .h(px(19.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .gap(px(10.0))
                    .whitespace_nowrap()
                    .when(bad, |d| d.bg(alpha(BAD, 0.20)))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |s, _e, _w, cx| s.settings_click_editor(row, cx)))
                    .child(div().w(px(34.0)).flex_none().text_color(hsla(th.dim)).child(SharedString::from(format!("{n}"))))
                    .child(body),
            );
        }
        let mut col = div().flex_1().min_h_0().flex().flex_col().gap(px(8.0)).overflow_hidden().child(bar);
        if t.stale {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .items_center()
                    .p(px(6.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(alpha(WARN, 0.8))
                    .child(div().flex_1().text_size(px(12.0)).child("The file changed on disk. Reload shows the new text and drops your edits. Keep holds your edits."))
                    .child(self.sbtn(("toml-reload", 0), "Reload", false, cx, |s, w, cx| s.settings_press("toml-reload", w, cx)))
                    .child(self.sbtn(("toml-keep", 0), "Keep", true, cx, |s, w, cx| s.settings_press("toml-keep", w, cx))),
            );
        }
        col = col.child(editor);
        if let Some(m) = &t.message {
            if t.loaded {
                col = col.child(div().text_size(px(12.0)).text_color(hsla(BAD)).child(SharedString::from(forgetty_shell_core::notify::notice_text(m))));
            }
        }
        col
    }

    pub fn settings_click_editor(&mut self, row: usize, cx: &mut Context<Self>) {
        self.with_settings(|st, _| st.click_editor(row, usize::MAX));
        cx.notify();
    }

    fn settings_about(&self, rows: Vec<(String, String)>, cx: &mut Context<Self>) -> Div {
        let th = self.theme;
        let mut col = div().flex_1().min_h_0().flex().flex_col().gap(px(10.0));
        for (k, v) in rows {
            if v.is_empty() {
                col = col.child(div().text_color(hsla(th.dim)).child(SharedString::from(k)));
            } else {
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(12.0))
                        .child(div().w(px(170.0)).flex_none().text_color(hsla(th.dim)).child(SharedString::from(k)))
                        .child(div().flex_1().min_w_0().font_family("Cascadia Mono").text_size(px(12.5)).child(SharedString::from(v))),
                );
            }
        }
        col.child(div().flex().flex_row().child(self.sbtn(("about-open", 0), "Open the file in your editor", true, cx, |s, w, cx| s.settings_press("about-open", w, cx))))
    }

    pub fn render_shortcuts(&self, cx: &mut Context<Self>) -> Option<Div> {
        let w = self.shortcuts.as_ref()?;
        let th = self.theme;
        let lines = self.shortcut_lines();
        let vis = self.shortcuts_visible();
        let mut list = div().flex().flex_col().overflow_hidden();
        for (head, t, k) in lines.into_iter().skip(w.top).take(vis) {
            list = list.child(if head {
                div().h(px(26.0)).pt(px(8.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(hsla(th.dim)).child(SharedString::from(t.to_uppercase()))
            } else {
                div()
                    .h(px(26.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(t)))
                    .child(div().flex_none().text_size(px(12.0)).font_family("Cascadia Mono").text_color(hsla(th.dim)).child(SharedString::from(k)))
            });
        }
        let panel = div()
            .w(px(560.0f32.min(self.viewport.0 - 40.0).max(260.0)))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(18.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(alpha(th.border, 1.0))
            .bg(alpha(blend(th.chrome, th.bg, 0.2), 0.99))
            .shadow_lg()
            .text_size(px(13.0))
            .text_color(hsla(th.fg))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().flex_1().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child("Keyboard shortcuts"))
                    .child(div().text_size(px(11.5)).text_color(hsla(th.dim)).child(self.cfg.keymap.label())),
            )
            .child(list)
            .child(
                div().flex().flex_row().items_center().gap(px(8.0)).child(div().flex_1().text_size(px(11.5)).text_color(hsla(th.dim)).child("Up/Down or the wheel scroll. Esc or F1 closes."))
                    .child(self.sbtn(("shortcuts-settings", 0), "Change shortcuts...", false, cx, |s, _w, cx| {
                        s.shortcuts = None;
                        s.open_settings(Some(Page::Keybindings), cx);
                    }))
                    .child(self.sbtn(("shortcuts-close", 0), "Close", true, cx, |s, w, cx| s.close_shortcuts(w, cx))),
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
                .pt(px(TITLEBAR_H + 40.0))
                .occlude()
                .on_scroll_wheel(cx.listener(|s, ev: &ScrollWheelEvent, _w, cx| s.shortcuts_wheel(ev, cx)))
                .child(panel),
        )
    }
}

fn trim_pt(f: f32) -> String {
    if (f - f.round()).abs() < 0.05 {
        format!("{}", f.round() as i32)
    } else {
        format!("{f:.1}")
    }
}
