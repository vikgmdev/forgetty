//! WP1 (terminal pane parity) in the `--smoke` gate. A view that is not in the window gets
//! fed bytes and driven through the same methods the mouse and keyboard handlers call, so
//! each check reads real state: the links the pane found, the text a copy would put on the
//! clipboard, the offset after a scrollbar drag. The find bar also runs on the real ConPTY
//! panes (see `Phase::Wp1Pane`).
//!
//! Sub steps 100 to 109 of the W5 view phase; they hand back to step 50 (OSC 52).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forgetty_shell_core::selection::Sel;
use forgetty_shell_core::{PaneEvent, PaneId, PaneSource};
use gpui::{point, px, size, AppContext as _, Bounds, Context, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Window};
use serde_json::json;

use super::{check, fail, record, row_text, ShellSmoke};
use crate::shell::AppShell;
use crate::view::TerminalView;

/// A pane source that feeds what the smoke sends and keeps what the view writes.
struct Feed {
    rx: async_channel::Receiver<PaneEvent>,
    input: Arc<Mutex<Vec<u8>>>,
}

impl PaneSource for Feed {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.rx.clone()
    }
    fn write_input(&self, bytes: &[u8]) {
        self.input.lock().unwrap().extend_from_slice(bytes);
    }
    fn write_auto(&self, _bytes: &[u8]) {}
    fn resize(&self, _rows: u16, _cols: u16) {}
    fn shutdown(&self) {}
}

#[derive(Default)]
pub struct Wp1 {
    view: Option<Entity<TerminalView>>,
    tx: Option<async_channel::Sender<PaneEvent>>,
    fed: u64,
    toasts: usize,
}

const ROWS: usize = 10;
const COLS: usize = 40;
const SIMPLE_URL: &str = "https://example.com/a_(b)";
const WRAPPED_URL: &str = "https://example.org/very/long/path/that/wraps/over/rows?q=1";

/// 30 numbered lines (scrollback), a first marker, then 5 more numbered lines, then exactly the
/// rows of the viewport: a link, a wrapped link, a bordered block, the second marker.
fn content() -> Vec<u8> {
    let mut s = String::new();
    for i in 1..=30 {
        s.push_str(&format!("L{i:02}\r\n"));
    }
    s.push_str("alpha FIND-me one\r\n");
    for i in 31..=35 {
        s.push_str(&format!("L{i}\r\n"));
    }
    s.push_str(&format!("see {SIMPLE_URL}). end\r\n"));
    s.push_str(&format!("wrap: {WRAPPED_URL}"));
    s.push_str("\r\n");
    s.push_str("\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\r\n");
    s.push_str("\u{2502} code(x) \u{2502}\r\n");
    s.push_str("\u{2502} more     \u{2502}\r\n");
    s.push_str("\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}\r\n");
    s.push_str("FIND-ME two");
    s.into_bytes()
}

fn rows_of(v: &mut TerminalView) -> Vec<String> {
    let screen = v.term.screen();
    (0..screen.rows()).map(|r| row_text(screen, r)).collect()
}

impl ShellSmoke {
    fn wp1_send(&self, ev: PaneEvent) {
        if let Some(tx) = &self.wp1.tx {
            let _ = tx.try_send(ev);
        }
    }

    pub(super) fn wp1(&mut self, window: &mut Window, cx: &mut Context<AppShell>, cfg: &crate::config::Config) {
        if self.sub_t0.elapsed() > Duration::from_secs(20) {
            fail(&format!("wp1 checks stuck at step {}", self.sub));
        }
        let view = self.wp1.view.clone();
        match self.sub {
            100 => {
                let (tx, rx) = async_channel::unbounded();
                let input = Arc::new(Mutex::new(Vec::new()));
                let mut cfg = cfg.clone();
                cfg.paste_warn_size = 100;
                let v = cx.new(|cx| TerminalView::new(cfg, PaneId::new(), Box::new(Feed { rx, input }), ROWS, COLS, true, cx));
                self.wp1.view = Some(v);
                self.wp1.tx = Some(tx);
                let bytes = content();
                self.wp1.fed = bytes.len() as u64;
                self.wp1_send(PaneEvent::Output(bytes));
                self.sub = 101;
                self.sub_t0 = Instant::now();
            }
            101 => {
                let Some(v) = view else { return };
                if v.read(cx).fed_bytes < self.wp1.fed {
                    return;
                }
                // The pane is laid out as the window would: exact cells, no padding.
                let (cw, lh) = {
                    let m = &v.read(cx).metrics;
                    (f32::from(m.cell_w), f32::from(m.line_h))
                };
                v.update(cx, |v, _| v.apply_bounds(Bounds::new(point(px(0.0), px(0.0)), size(px(cw * COLS as f32), px(lh * ROWS as f32)))));
                self.wp1_geometry_checks(&v, window, cx, (cw, lh));
                // The find bar: a scan in slices on the timer, the way the app runs it.
                v.update(cx, |v, vcx| v.search_set("find-me", vcx));
                self.sub = 102;
                self.sub_t0 = Instant::now();
            }
            102 => {
                let Some(v) = view else { return };
                let Some((done, found, current, _)) = v.read(cx).search_status() else { return };
                if !done {
                    return;
                }
                let top = v.read(cx).term.scrollbar_state().1;
                let spans = v.read(cx).search_spans();
                record("wp1_search", json!({"found": found, "current": current, "spans": spans.len(), "top": top}));
                // Case-insensitive: "FIND-me" and "FIND-ME" both match "find-me".
                check("search_finds_every_match_ignoring_case", found == 2);
                // The first match at or below the viewport is current, and it is in view.
                check("search_current_is_visible_match", current == 1 && spans.len() == 1 && spans[0].3);
                // Enter wraps around to the match in the scrollback and scrolls to it.
                v.update(cx, |v, vcx| v.search_goto(true, vcx));
                let (top2, spans2, cur2) = {
                    let r = v.read(cx);
                    (r.term.scrollbar_state().1, r.search_spans(), r.search_status().map(|s| s.2))
                };
                check("search_next_scrolls_to_match", cur2 == Some(0) && top2 < top && spans2.len() == 1 && spans2[0].3);
                v.update(cx, |v, vcx| v.search_goto(false, vcx));
                let cur3 = v.read(cx).search_status().map(|s| s.2);
                check("search_previous_goes_back", cur3 == Some(1));
                // Esc through the key path closes it and leaves the view where it is.
                v.update(cx, |v, vcx| v.search_key(&super::key("escape"), false, vcx));
                check("search_escape_closes", v.read(cx).search.is_none());
                v.update(cx, |v, _| v.term.scroll_viewport_bottom());
                self.wp1_after_search(&v, cx)
            }
            // Two bells right behind each other: one rings, the flash shows.
            106 => {
                let Some(v) = view else { return };
                if self.sub_t0.elapsed() < Duration::from_millis(60) {
                    return;
                }
                let (bells, flash) = (v.read(cx).bells, v.read(cx).bell_flash());
                check("bell_rate_limited_to_one_per_200ms", bells == 1);
                check("bell_visual_flash_on", flash);
                self.sub = 107;
                self.sub_t0 = Instant::now();
            }
            // After the gap the next one rings.
            107 => {
                if self.sub_t0.elapsed() < Duration::from_millis(300) {
                    return;
                }
                self.wp1_send(PaneEvent::Output(b"\x07".to_vec()));
                self.sub = 108;
                self.sub_t0 = Instant::now();
            }
            // Ctrl+C then a bell right after: the shell's beep after an interrupt is dropped.
            108 => {
                let Some(v) = view else { return };
                if self.sub_t0.elapsed() < Duration::from_millis(100) {
                    return;
                }
                check("bell_rings_after_the_gap", v.read(cx).bells == 2);
                v.update(cx, |v, _| {
                    v.capture_input = Some(Vec::new());
                    v.send_keystroke(&super::key("ctrl+c"), forgetty_vt::ffi::GHOSTTY_KEY_ACTION_PRESS);
                });
                self.wp1_send(PaneEvent::Output(b"\x07".to_vec()));
                self.sub = 104;
                self.sub_t0 = Instant::now();
            }
            104 => {
                let Some(v) = view else { return };
                if self.sub_t0.elapsed() < Duration::from_millis(250) {
                    return;
                }
                check("bell_after_ctrl_c_is_dropped", v.read(cx).bells == 2);
                // The next bell is real again.
                self.wp1_send(PaneEvent::Output(b"\x07".to_vec()));
                self.sub = 105;
                self.sub_t0 = Instant::now();
            }
            105 => {
                let Some(v) = view else { return };
                if self.sub_t0.elapsed() < Duration::from_millis(150) {
                    return;
                }
                check("bell_rings_again_after_ctrl_c", v.read(cx).bells == 3);
                self.wp1.view = None;
                self.wp1.tx = None;
                self.sub = 50;
                self.sub_t0 = Instant::now();
            }
            _ => {}
        }
    }

    /// Everything that needs no waiting: links, code block, selection, scrollbar, paste, clear.
    fn wp1_geometry_checks(&mut self, v: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<AppShell>, (cw, lh): (f32, f32)) {
        let cell = |row: usize, col: usize| point(px((col as f32 + 0.5) * cw), px((row as f32 + 0.5) * lh));
        let rows = v.update(cx, |v, _| rows_of(v));
        let find = |needle: &str| rows.iter().position(|r| r.contains(needle));
        let (Some(r_simple), Some(r_wrap), Some(r_box), Some(r_two)) =
            (find("see https"), find("wrap: https"), find("code(x)"), find("FIND-ME two"))
        else {
            record("wp1_screen", json!(rows));
            check("wp1_layout_as_expected", false);
            return;
        };
        check("wp1_layout_as_expected", r_wrap + 1 < ROWS);
        let none = Modifiers::default();
        let ctrl = Modifiers { control: true, ..Default::default() };
        let shift = Modifiers { shift: true, ..Default::default() };

        // ---- links ----
        let c_simple = rows[r_simple].find("example").unwrap_or(5);
        let simple = v.update(cx, |v, _| v.link_at(r_simple, c_simple));
        check("link_found_and_trailing_punctuation_cut", simple.as_ref().is_some_and(|l| l.url == SIMPLE_URL));
        let wrapped = v.update(cx, |v, _| v.link_at(r_wrap + 1, 3));
        record("wp1_wrapped_link", json!(wrapped.as_ref().map(|l| (l.url.clone(), l.segments.clone()))));
        check("link_wrapped_over_rows_found_whole", wrapped.as_ref().is_some_and(|l| l.url == WRAPPED_URL && l.segments.len() == 2));
        check("link_none_on_plain_text", v.update(cx, |v, _| v.link_at(r_two, 2)).is_none());
        // Ctrl+click opens it, a plain click does not.
        let down = |position, modifiers| MouseDownEvent { button: MouseButton::Left, position, modifiers, click_count: 1, first_mouse: false };
        let up = |position| MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1 };
        v.update(cx, |v, vcx| v.on_mouse_down(&down(cell(r_simple, c_simple), none), window, vcx));
        v.update(cx, |v, vcx| v.on_pointer_up(&up(cell(r_simple, c_simple)), vcx));
        check("link_plain_click_does_not_open", v.read(cx).opened_urls.is_empty());
        v.update(cx, |v, vcx| v.on_mouse_down(&down(cell(r_simple, c_simple), ctrl), window, vcx));
        check("link_ctrl_click_opens", v.read(cx).opened_urls == vec![SIMPLE_URL.to_string()]);
        // Only web links may reach the OS.
        let bad = [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "ms-msdt:/id PCWDiagnostic",
            "http://",
            "https://exa mple.com",
            "https://a.com/\u{202e}txt",
            "https://a.com/\u{1b}[0m",
        ];
        v.update(cx, |v, vcx| bad.iter().for_each(|u| v.open_link(u, vcx)));
        check("link_other_schemes_refused", v.read(cx).opened_urls.len() == 1);
        // Hover underlines (the pointer must be over the pane) and clears when it leaves.
        let mv = |position| MouseMoveEvent { position, pressed_button: None, modifiers: Modifiers::default() };
        v.update(cx, |v, vcx| v.on_pointer_move(&mv(cell(r_simple, c_simple)), vcx));
        let spans = v.update(cx, |v, _| v.link_spans());
        check("link_hover_underlines", v.read(cx).hover_link.is_some() && spans.len() == 1);
        v.update(cx, |v, vcx| v.on_pointer_move(&mv(point(px(-5.0), px(5.0))), vcx));
        check("link_hover_clears_outside_pane", v.read(cx).hover_link.is_none());

        // ---- code block: hover, then the copy button ----
        let (c0, c1) = (rows[r_box].find('\u{2502}').unwrap_or(0), 5);
        v.update(cx, |v, vcx| v.on_pointer_move(&mv(cell(r_box, c0 + c1)), vcx));
        let block = v.read(cx).hover_block.clone();
        record("wp1_block", json!(block.as_ref().map(|b| (b.top_row, b.bottom_row, b.left_col, b.right_col))));
        check(
            "code_block_found_under_pointer",
            block.as_ref().is_some_and(|b| b.top_row + 1 == r_box && b.bottom_row == r_box + 2 && b.left_col == 0 && b.right_col == 9),
        );
        if let Some(b) = block {
            let btn = point(px((b.right_col as f32 + 1.0) * cw - 12.0), px(b.top_row as f32 * lh + 12.0));
            v.update(cx, |v, vcx| v.on_mouse_down(&down(btn, none), window, vcx));
            let clip = cx.read_from_clipboard().and_then(|i| i.text()).unwrap_or_default();
            record("wp1_block_clip", json!(clip));
            check("code_block_button_copies_inner_text", clip.contains("code(x)") && clip.contains("more") && !clip.contains('\u{2502}'));
        } else {
            check("code_block_button_copies_inner_text", false);
        }
        v.update(cx, |v, vcx| v.on_pointer_move(&mv(point(px(-5.0), px(5.0))), vcx));

        // ---- selection: select all, smart copy, shift-click ----
        v.update(cx, |v, vcx| v.select_all(vcx));
        let (range, smart, raw) = v.update(cx, |v, _| {
            let range = v.sel.as_ref().map(|s| s.range());
            (range, v.selection_text_as(true).unwrap_or_default(), v.selection_text_as(false).unwrap_or_default())
        });
        let top = v.read(cx).term.scrollbar_state().1 as usize;
        check("select_all_covers_the_screen", range == Some(((top, 0), (top + ROWS - 1, COLS - 1))));
        check("smart_copy_strips_box_drawing", smart.contains("code(x)") && !smart.contains('\u{250c}') && !smart.contains('\u{2502}'));
        check("raw_copy_keeps_box_drawing", raw.contains('\u{250c}') && raw.contains('\u{2502}'));
        // The two halves of the wrapped URL are one line of text, not two.
        check("copy_joins_soft_wrapped_rows", smart.contains(WRAPPED_URL));
        v.update(cx, |v, _| v.sel = Some(Sel::new(forgetty_shell_core::selection::SelMode::Normal, (top + 2, 1), (top + 2, 3))));
        v.update(cx, |v, vcx| v.on_mouse_down(&down(cell(5, 7), shift), window, vcx));
        let (r, selecting) = v.read(cx).sel.as_ref().map(|s| (s.range(), v.read(cx).selecting)).unwrap_or_default();
        check("shift_click_extends_selection", r == ((top + 2, 1), (top + 5, 7)) && !selecting);
        v.update(cx, |v, _| v.sel = None);

        // ---- auto-scroll while dragging past the top edge ----
        v.update(cx, |v, _| v.scroll_rows(-8));
        let off0 = v.read(cx).term.scrollbar_state().1;
        v.update(cx, |v, vcx| v.on_mouse_down(&down(cell(4, 2), none), window, vcx));
        let head0 = v.read(cx).sel.as_ref().map(|s| s.range().0.0).unwrap_or(0);
        let drag = MouseMoveEvent { position: point(px(10.0), px(-3.0 * lh)), pressed_button: Some(MouseButton::Left), modifiers: none };
        v.update(cx, |v, vcx| v.on_pointer_move(&drag, vcx));
        v.update(cx, |v, vcx| {
            v.autoscroll_tick(vcx);
            v.autoscroll_tick(vcx);
        });
        let (off1, head1) = (v.read(cx).term.scrollbar_state().1, v.read(cx).sel.as_ref().map(|s| s.range().0.0).unwrap_or(0));
        record("wp1_autoscroll", json!({"off": [off0, off1], "head": [head0, head1]}));
        check("drag_past_top_scrolls_and_extends", off1 < off0 && head1 < head0);
        v.update(cx, |v, vcx| v.on_pointer_up(&up(cell(4, 2)), vcx));
        check("drag_release_ends_selection", !v.read(cx).selecting);
        v.update(cx, |v, _| {
            v.sel = None;
            v.term.scroll_viewport_bottom();
        });

        // ---- scrollbar ----
        let (total, off, len) = v.read(cx).term.scrollbar_state();
        let thumb = v.read(cx).sb_thumb();
        check("scrollbar_shown_with_scrollback", total > len && thumb.is_some());
        let track = lh * ROWS as f32 + 2.0 * v.read(cx).cfg.pane_padding_y;
        check("scrollbar_thumb_at_bottom", thumb.is_some_and(|t| (t.top + t.height - track).abs() < 1.0) && off + len == total);
        v.update(cx, |v, vcx| v.scrollbar_drag_to(-1000.0, 0.0, vcx));
        check("scrollbar_drag_to_top", v.read(cx).term.scrollbar_state().1 == 0);
        v.update(cx, |v, vcx| v.scrollbar_drag_to(10_000.0, 0.0, vcx));
        let (t2, o2, l2) = v.read(cx).term.scrollbar_state();
        check("scrollbar_drag_to_bottom", o2 + l2 == t2);
        let mid = v.read(cx).bounds.origin.y;
        v.update(cx, |v, vcx| v.scrollbar_drag_to(f32::from(mid) + track / 2.0, 0.0, vcx));
        let o3 = v.read(cx).term.scrollbar_state().1;
        check("scrollbar_drag_to_middle", o3 > 0 && o3 + l2 < t2);
        v.update(cx, |v, _| v.term.scroll_viewport_bottom());

        // ---- paste warning ----
        let big = "x".repeat(200);
        let asked = v.update(cx, |v, _| v.send_paste(&big));
        let large = v.read(cx).pending_paste.as_ref().is_some_and(|p| p.large && p.size == 200);
        v.update(cx, |v, _| v.resolve_paste(false));
        let small = v.update(cx, |v, _| v.send_paste("short"));
        check("paste_over_limit_asks_first", asked && large && !small);

        // ---- clear and reset ----
        v.update(cx, |v, vcx| {
            v.capture_input = Some(Vec::new());
            v.clear_terminal(vcx);
        });
        let sent = v.update(cx, |v, _| v.capture_input.take().unwrap_or_default());
        check("clear_sends_ctrl_l", sent == [0x0c]);
        v.update(cx, |v, vcx| {
            v.capture_input = Some(Vec::new());
            v.term.feed(b"\x1b[31mred\x1b[?1000h");
            v.reset_terminal(vcx);
        });
        let (sent, tracking, total, first) = v.update(cx, |v, _| {
            let sent = v.capture_input.take().unwrap_or_default();
            let tracking = v.term.is_mouse_tracking();
            let total = v.term.scrollbar_state().0;
            let first = rows_of(v).iter().all(|r| r.trim().is_empty());
            (sent, tracking, total as usize, first)
        });
        record("wp1_reset_total_rows", json!(total));
        check("reset_clears_screen_and_modes", sent == [0x0c] && !tracking && first);

        // ---- OSC 12 cursor color ----
        v.update(cx, |v, _| {
            v.term.feed(b"\x1b]12;#ff8800\x07");
            let _ = v.term.screen();
        });
        let c = v.read(cx).term.cursor_color();
        v.update(cx, |v, _| {
            v.term.feed(b"\x1b]112\x07");
            let _ = v.term.screen();
        });
        let c2 = v.read(cx).term.cursor_color();
        record("wp1_cursor_color", json!([c, c2]));
        check("osc12_sets_and_osc112_resets_cursor_color", c == Some((255, 136, 0)) && c2.is_none());
    }

    /// The bells start here: two with a gap, and one that lands inside the rate limit.
    fn wp1_after_search(&mut self, v: &Entity<TerminalView>, cx: &mut Context<AppShell>) {
        let before = v.read(cx).bells;
        self.wp1_send(PaneEvent::Output(b"\x07".to_vec()));
        self.wp1_send(PaneEvent::Output(b"\x07".to_vec()));
        self.sub = 106;
        self.sub_t0 = Instant::now();
        record("wp1_bells_before", json!(before));
    }
}

impl ShellSmoke {
    /// The find bar and the program notifications on the real ConPTY panes of the 4-pane tab.
    pub(super) fn wp1_pane(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        let (p0, p1) = (self.panes4[0], self.panes4[1]);
        let (Some(v0), Some(v1)) = (shell.views.get(&p0).cloned(), shell.views.get(&p1).cloned()) else { fail("wp1 pane views vanished") };
        if self.sub_t0.elapsed() > Duration::from_secs(12) {
            record("wp1_pane_stuck_at", json!(self.sub));
            fail(&format!("wp1 pane checks stuck at step {}", self.sub));
        }
        match self.sub {
            0 => {
                shell.focus_pane(p0, window, cx);
                // The real chord, then the token typed into the find bar.
                self.press(shell, window, cx, "ctrl+shift+f");
                check("search_chord_opens_find_bar", v0.read(cx).search.is_some());
                let token = self.tokens[0].clone();
                v0.update(cx, |v, vcx| {
                    for ch in token.chars() {
                        v.search_key(&super::typed(ch), false, vcx);
                    }
                });
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            1 => {
                let Some((done, found, _, _)) = v0.read(cx).search_status() else { return };
                if !done {
                    return;
                }
                // The command line that echoed the token, and the line it printed.
                let spans = v0.read(cx).search_spans();
                record("wp1_pane_search", json!({"found": found, "visible": spans.len()}));
                check("search_finds_token_in_real_pane", found >= 2 && spans.len() >= 2);
                v0.update(cx, |v, vcx| v.search_key(&super::key("escape"), false, vcx));
                check("search_closes_with_escape", v0.read(cx).search.is_none());
                // A program asks for a desktop notification (OSC 9) in a pane the user is not in.
                self.wp1.toasts = shell.toast_log.len();
                v1.update(cx, |v, _| v.send_bytes(b"prompt $E]9;smoke-ping$E\\$P$G\r"));
                self.sub = 2;
                self.sub_t0 = Instant::now();
            }
            2 => {
                if shell.toast_log.len() <= self.wp1.toasts {
                    if self.sub_t0.elapsed() > Duration::from_secs(8) {
                        // ConPTY may not pass this sequence on; say so and go on.
                        check("local_osc9_raises_toast", false);
                        self.wp1_restore_prompt(&v1, cx);
                    }
                    return;
                }
                let rec = shell.toast_log[self.wp1.toasts].clone();
                record("wp1_toast", json!({"title": rec.text.title, "lines": rec.text.lines}));
                check("local_osc9_raises_toast", rec.pane == p1 && rec.text.lines.first().map(String::as_str) == Some("smoke-ping"));
                self.wp1_restore_prompt(&v1, cx);
            }
            3 => {
                if super::snap(shell, cx, p1).is_some_and(|s| s.at_prompt()) && self.sub_t0.elapsed() > Duration::from_millis(500) {
                    self.goto(super::Phase::Keys4);
                }
            }
            _ => {}
        }
    }

    fn wp1_restore_prompt(&mut self, v1: &Entity<TerminalView>, cx: &mut Context<AppShell>) {
        v1.update(cx, |v, _| v.send_bytes(b"prompt $P$G\r"));
        self.sub = 3;
        self.sub_t0 = Instant::now();
    }
}
