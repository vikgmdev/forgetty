//! TerminalView: owns the libghostty-vt `Terminal` and talks to a `PaneSource`.
//! It never knows whether bytes come from a local ConPTY or a remote pane.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use forgetty_core::Rgba as CoreRgba;
use forgetty_vt::ffi;
use forgetty_vt::screen::{CellAttributes, Color};
use forgetty_vt::{Screen, Terminal};
use gpui::{
    div, font, point, prelude::*, px, rgb, size, App, Bounds, ClipboardItem, Context, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Hsla, IntoElement, KeyDownEvent, KeyUpEvent,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, StrikethroughStyle, TextRun,
    UTF16Selection, UnderlineStyle, Window,
};

use crate::config::Config;
use crate::element::TerminalElement;
use crate::encoder::{self, Input};
use crate::smoke::Smoke;
use crate::source::{PaneEvent, PaneSource};

pub const INITIAL_ROWS: usize = 30;
pub const INITIAL_COLS: usize = 120;
const SHAPE_CACHE_MAX: usize = 4096;
const FEED_CHUNK: usize = 256 * 1024;

pub fn hsla(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    hsla(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

pub struct Metrics {
    pub font: Font,
    pub font_size: Pixels,
    pub cell_w: Pixels,
    pub line_h: Pixels,
}

impl Metrics {
    /// Picks the configured family if installed, else a monospace fallback.
    pub fn measure(cx: &App, cfg: &Config) -> Self {
        let ts = cx.text_system();
        let names = ts.all_font_names();
        let family = std::iter::once(cfg.font_family.as_str())
            .chain(["Cascadia Mono", "Cascadia Code", "Consolas", "Courier New"])
            .find(|f| names.iter().any(|n| n == f))
            .unwrap_or(cfg.font_family.as_str())
            .to_string();
        let font = font(family);
        let font_size = px(cfg.font_size_px());
        let id = ts.resolve_font(&font);
        let cell_w = ts.advance(id, font_size, 'm').map(|s| s.width).unwrap_or(px(8.0));
        let line_h = px((f32::from(font_size) * 1.25).ceil());
        Metrics { font, font_size, cell_w, line_h }
    }
}

/// Shaped, ready-to-paint form of one terminal row. Cached by content hash.
pub struct RowShaped {
    pub line: gpui::ShapedLine,
    /// (first col, one past last col, color)
    pub bgs: Vec<(usize, usize, Hsla)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelMode {
    Normal,
    Word,
    Line,
}

/// Selection in absolute rows (scrollback offset + viewport row).
#[derive(Clone)]
pub struct Sel {
    pub mode: SelMode,
    /// Initial unit (a single cell, a word, or a line) as inclusive (row, col) ends.
    pub a0: (usize, usize),
    pub a1: (usize, usize),
    /// Current drag unit.
    pub h0: (usize, usize),
    pub h1: (usize, usize),
}

impl Sel {
    pub fn range(&self) -> ((usize, usize), (usize, usize)) {
        (self.a0.min(self.h0), self.a1.max(self.h1))
    }
    pub fn is_empty(&self) -> bool {
        self.mode == SelMode::Normal && self.a0 == self.h0 && self.a1 == self.h1
    }
}

pub struct TerminalView {
    pub cfg: Config,
    pub term: Terminal,
    pub source: Box<dyn PaneSource>,
    pub input: Input,
    pub metrics: Metrics,
    pub rows: usize,
    pub cols: usize,
    pub row_hash: Vec<u64>,
    pub rows_shaped: Vec<Option<Arc<RowShaped>>>,
    pub shape_cache: HashMap<u64, Arc<RowShaped>>,
    pub shape_misses: u64,
    pub rows_reshaped_last: usize,
    pub last_gen: u64,
    pub focus: FocusHandle,
    pub scroll_accum: f32,
    pub sel: Option<Sel>,
    pub selecting: bool,
    pub mouse_reporting: bool,
    pub marked: Option<String>,
    pub cursor_on: bool,
    pub bounds: Bounds<Pixels>,
    pub window_active: bool,
    pub title: String,
    pub applied_title: String,
    pub closed: Option<String>,
    pub frames: u64,
    pub resizes: u64,
    pub last_paint: Option<Instant>,
    pub smoke: Option<Box<Smoke>>,
    /// Smoke only: dry-run key encoding writes here instead of the source.
    pub capture_input: Option<Vec<u8>>,
}

struct YieldNow(bool);
impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl TerminalView {
    pub fn new(
        cfg: Config,
        source: Box<dyn PaneSource>,
        smoke: Option<Box<Smoke>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let metrics = Metrics::measure(cx, &cfg);
        let palette: [CoreRgba; 16] = std::array::from_fn(|i| {
            let h = cfg.ansi[i];
            CoreRgba::rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
        });
        // libghostty-vt budgets scrollback in bytes: ~2 KB per line of up to 128 columns.
        let term = Terminal::with_scrollback(INITIAL_ROWS, INITIAL_COLS, palette, cfg.scrollback_lines * 2048);

        // Pump source output into the VT on the UI thread. Batches are capped
        // and the task yields between them so frames keep flowing while a
        // multi-megabyte burst drains.
        let rx = source.events();
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(first) = rx.recv().await else { break };
                let mut batch = Vec::new();
                let mut closed = None;
                match first {
                    PaneEvent::Output(b) => batch = b,
                    PaneEvent::Closed(r) => closed = Some(r),
                }
                while closed.is_none() && batch.len() < FEED_CHUNK {
                    match rx.try_recv() {
                        Ok(PaneEvent::Output(b)) => batch.extend_from_slice(&b),
                        Ok(PaneEvent::Closed(r)) => closed = Some(r),
                        Err(_) => break,
                    }
                }
                if this.update(cx, |v, cx| v.feed(&batch, closed, cx)).is_err() {
                    break;
                }
                YieldNow(false).await;
            }
        })
        .detach();

        let is_smoke = smoke.is_some();
        if !is_smoke {
            // Cursor blink: repaint only while the cursor actually blinks.
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(530)).await;
                if this.update(cx, |v, cx| v.blink_tick(cx)).is_err() {
                    break;
                }
            })
            .detach();
        }

        Self {
            cfg,
            term,
            source,
            input: Input::new(),
            metrics,
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            row_hash: vec![0; INITIAL_ROWS],
            rows_shaped: vec![None; INITIAL_ROWS],
            shape_cache: HashMap::new(),
            shape_misses: 0,
            rows_reshaped_last: 0,
            last_gen: 0,
            focus: cx.focus_handle(),
            scroll_accum: 0.0,
            sel: None,
            selecting: false,
            mouse_reporting: false,
            marked: None,
            cursor_on: true,
            bounds: Bounds::default(),
            window_active: true,
            title: String::new(),
            applied_title: String::new(),
            closed: None,
            frames: 0,
            resizes: 0,
            last_paint: None,
            smoke,
            capture_input: None,
        }
    }

    // ---- output ---------------------------------------------------------

    fn feed(&mut self, bytes: &[u8], closed: Option<String>, cx: &mut Context<Self>) {
        if !bytes.is_empty() {
            self.term.feed(bytes);
            // Terminal replies (DA, CPR, mode queries). ConPTY waits on these.
            for resp in self.term.drain_write_pty() {
                self.source.write_input(&resp);
            }
            for ev in self.term.drain_events() {
                if let forgetty_vt::TerminalEvent::TitleChanged(t) = ev {
                    self.title = t;
                }
            }
        }
        if let Some(reason) = closed {
            self.closed = Some(reason.clone());
            if self.smoke.is_some() {
                crate::smoke::fail(&format!("pane closed: {reason}"));
            }
            cx.quit();
        }
        cx.notify();
    }

    pub fn on_activation(&mut self, active: bool, cx: &mut Context<Self>) {
        self.window_active = active;
        self.cursor_on = true;
        if self.term.is_focus_reporting() {
            if let Some(b) = Input::encode_focus(active) {
                self.send_bytes(&b);
            }
        }
        cx.notify();
    }

    fn blink_tick(&mut self, cx: &mut Context<Self>) {
        if self.term.cursor_visible() && self.term.cursor_blinking() && self.window_active {
            self.cursor_on = !self.cursor_on;
            cx.notify();
        } else if !self.cursor_on {
            self.cursor_on = true;
            cx.notify();
        }
    }

    // ---- frame preparation (called from the element's prepaint) -----------

    /// Follow the window: at most one resize per frame, since this runs once
    /// per prepaint.
    pub fn apply_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;
        let cols = ((f32::from(bounds.size.width) / f32::from(self.metrics.cell_w) + 0.1).floor() as usize).clamp(2, 1000);
        let rows = ((f32::from(bounds.size.height) / f32::from(self.metrics.line_h) + 0.1).floor() as usize).clamp(1, 500);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.resizes += 1;
        self.term.resize(rows, cols);
        self.source.resize(rows as u16, cols as u16);
        self.row_hash = vec![0; rows];
        self.rows_shaped = vec![None; rows];
        self.last_gen = 0;
        self.sel = None;
    }

    /// Re-shape only rows whose content changed. Rows the VT did not touch are
    /// skipped without hashing; changed rows are looked up by content hash so
    /// scrolling back over seen content reuses shaped lines.
    pub fn prepare_rows(&mut self, window: &mut Window) {
        let screen = self.term.screen();
        let gen_now = screen.generation();
        let mut reshaped = 0;
        for r in 0..screen.rows().min(self.rows_shaped.len()) {
            if self.rows_shaped[r].is_some() && !screen.row_dirty_since(r, self.last_gen) {
                continue;
            }
            let h = hash_row(screen, r);
            if self.rows_shaped[r].is_some() && self.row_hash[r] == h {
                continue;
            }
            self.row_hash[r] = h;
            let shaped = match self.shape_cache.get(&h) {
                Some(s) => s.clone(),
                None => {
                    let m = &self.metrics;
                    let (text, runs, bgs) = build_row(screen, r, &m.font, &self.cfg);
                    let line = window.text_system().shape_line(text, m.font_size, &runs, Some(m.cell_w));
                    let s = Arc::new(RowShaped { line, bgs });
                    if self.shape_cache.len() >= SHAPE_CACHE_MAX {
                        self.shape_cache.clear();
                    }
                    self.shape_cache.insert(h, s.clone());
                    self.shape_misses += 1;
                    reshaped += 1;
                    s
                }
            };
            self.rows_shaped[r] = Some(shaped);
        }
        self.rows_reshaped_last = reshaped;
        self.last_gen = gen_now;
    }

    // ---- scrolling -----------------------------------------------------------

    /// Scroll the viewport by whole rows (negative = into history). No re-parse:
    /// libghostty-vt just moves its viewport pin.
    pub fn scroll_rows(&mut self, delta: isize) {
        if delta != 0 {
            self.term.scroll_viewport_delta(delta);
        }
    }

    fn snap_to_bottom(&mut self) {
        let (total, offset, len) = self.term.scrollbar_state();
        if offset + len < total {
            self.term.scroll_viewport_bottom();
        }
    }

    // ---- keyboard ---------------------------------------------------------

    pub fn send_bytes(&mut self, bytes: &[u8]) {
        match self.capture_input.as_mut() {
            Some(buf) => buf.extend_from_slice(bytes),
            None => self.source.write_input(bytes),
        }
    }

    /// The one keystroke path: GPUI keystroke -> libghostty-vt encoder ->
    /// source. The smoke test calls this too.
    pub fn send_keystroke(&mut self, ks: &gpui::Keystroke, action: i32) -> bool {
        let handle = self.term.raw_handle();
        match self.input.encode_key(ks, action, handle) {
            Some(bytes) => {
                if action != ffi::GHOSTTY_KEY_ACTION_RELEASE {
                    self.snap_to_bottom();
                    self.cursor_on = true;
                }
                self.send_bytes(&bytes);
                true
            }
            None => false,
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        if m.control && m.shift && !m.alt {
            match ks.key.as_str() {
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if m.shift && !m.control && !m.alt {
            let page = self.rows.saturating_sub(1).max(1) as isize;
            match ks.key.as_str() {
                "pageup" => {
                    self.scroll_rows(-page);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.scroll_rows(page);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        // AltGr layouts report ctrl+alt with the composed character: send the text.
        if ev.prefer_character_input {
            if let Some(t) = ks.key_char.as_deref().filter(|t| !t.is_empty()) {
                self.snap_to_bottom();
                self.send_bytes(t.as_bytes());
                cx.stop_propagation();
                return;
            }
        }
        let action = if ev.is_held { ffi::GHOSTTY_KEY_ACTION_REPEAT } else { ffi::GHOSTTY_KEY_ACTION_PRESS };
        if self.send_keystroke(ks, action) {
            if self.sel.take().is_some() {
                cx.notify();
            }
            // Handled: stops the WM_CHAR path from inserting the text twice.
            cx.stop_propagation();
        }
        let _ = window;
    }

    fn on_key_up(&mut self, ev: &KeyUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        // Only produces bytes when the app enabled Kitty release reporting.
        if self.send_keystroke(&ev.keystroke, ffi::GHOSTTY_KEY_ACTION_RELEASE) {
            cx.stop_propagation();
        }
    }

    fn on_mods(&mut self, _ev: &ModifiersChangedEvent, _w: &mut Window, _cx: &mut Context<Self>) {}

    // ---- clipboard ----------------------------------------------------------

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        self.send_paste(&text);
    }

    pub fn send_paste(&mut self, text: &str) {
        let bracketed = encoder::private_mode(self.term.raw_handle(), 2004);
        self.snap_to_bottom();
        let bytes = paste_bytes(text, bracketed);
        self.send_bytes(&bytes);
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    // ---- mouse ----------------------------------------------------------------

    fn local(&self, p: Point<Pixels>) -> (f32, f32) {
        (
            (f32::from(p.x) - f32::from(self.bounds.origin.x)).max(0.0),
            (f32::from(p.y) - f32::from(self.bounds.origin.y)).max(0.0),
        )
    }

    fn cell_at(&self, p: Point<Pixels>) -> (usize, usize) {
        let (x, y) = self.local(p);
        let col = ((x / f32::from(self.metrics.cell_w)) as usize).min(self.cols.saturating_sub(1));
        let row = ((y / f32::from(self.metrics.line_h)) as usize).min(self.rows.saturating_sub(1));
        (row, col)
    }

    fn grid_px(&self) -> ((u32, u32), (u32, u32)) {
        (
            (
                (f32::from(self.metrics.cell_w) * self.cols as f32) as u32,
                (f32::from(self.metrics.line_h) * self.rows as f32) as u32,
            ),
            (f32::from(self.metrics.cell_w) as u32, f32::from(self.metrics.line_h) as u32),
        )
    }

    fn wants_mouse(&self, shift: bool) -> bool {
        self.term.is_mouse_tracking() && !shift
    }

    fn abs_row(&self, vrow: usize) -> usize {
        self.term.scrollbar_state().1 as usize + vrow
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if self.wants_mouse(ev.modifiers.shift) {
            let (g, c) = self.grid_px();
            let pos = self.local(ev.position);
            let handle = self.term.raw_handle();
            let b = encoder::mouse_button_to_ghostty(ev.button);
            if let Some(bytes) = self.input.encode_mouse_button(b, true, pos, &ev.modifiers, handle, g, c) {
                self.send_bytes(&bytes);
            }
            self.mouse_reporting = true;
            return;
        }
        match ev.button {
            MouseButton::Left => {
                let (vr, col) = self.cell_at(ev.position);
                let ar = self.abs_row(vr);
                let (mode, u0, u1) = match ev.click_count {
                    0 | 1 => (SelMode::Normal, (ar, col), (ar, col)),
                    2 => {
                        let (c0, c1) = self.word_bounds(vr, col);
                        (SelMode::Word, (ar, c0), (ar, c1))
                    }
                    _ => (SelMode::Line, (ar, 0), (ar, self.cols.saturating_sub(1))),
                };
                self.sel = Some(Sel { mode, a0: u0, a1: u1, h0: u0, h1: u1 });
                self.selecting = true;
                cx.notify();
            }
            MouseButton::Right => {
                // Windows Terminal convention: copy if selected, else paste.
                if self.sel.as_ref().is_some_and(|s| !s.is_empty()) {
                    self.copy_selection(cx);
                    self.sel = None;
                    cx.notify();
                } else {
                    self.paste(cx);
                }
            }
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.wants_mouse(ev.modifiers.shift) {
            let (g, c) = self.grid_px();
            let pos = self.local(ev.position);
            let handle = self.term.raw_handle();
            if let Some(bytes) = self.input.encode_mouse_motion(pos, &ev.modifiers, handle, g, c) {
                self.send_bytes(&bytes);
            }
            return;
        }
        if self.selecting && ev.pressed_button == Some(MouseButton::Left) {
            let (vr, col) = self.cell_at(ev.position);
            let ar = self.abs_row(vr);
            let (h0, h1) = match self.sel.as_ref().map(|s| s.mode) {
                Some(SelMode::Word) => {
                    let (c0, c1) = self.word_bounds(vr, col);
                    ((ar, c0), (ar, c1))
                }
                Some(SelMode::Line) => ((ar, 0), (ar, self.cols.saturating_sub(1))),
                _ => ((ar, col), (ar, col)),
            };
            if let Some(s) = self.sel.as_mut() {
                if s.h0 != h0 || s.h1 != h1 {
                    s.h0 = h0;
                    s.h1 = h1;
                    cx.notify();
                }
            }
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_reporting {
            self.mouse_reporting = false;
            let (g, c) = self.grid_px();
            let pos = self.local(ev.position);
            let handle = self.term.raw_handle();
            let b = encoder::mouse_button_to_ghostty(ev.button);
            if let Some(bytes) = self.input.encode_mouse_button(b, false, pos, &ev.modifiers, handle, g, c) {
                self.send_bytes(&bytes);
            }
            return;
        }
        if ev.button == MouseButton::Left && self.selecting {
            self.selecting = false;
            if self.sel.as_ref().is_some_and(|s| !s.is_empty()) {
                self.copy_selection(cx); // copy on select
            } else {
                self.sel = None;
                cx.notify();
            }
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let lh = f32::from(self.metrics.line_h);
        let dy = match ev.delta {
            ScrollDelta::Pixels(p) => f32::from(p.y),
            ScrollDelta::Lines(l) => l.y * 3.0 * lh,
        };
        if dy == 0.0 {
            return;
        }
        if self.wants_mouse(ev.modifiers.shift) {
            let (g, c) = self.grid_px();
            let pos = self.local(ev.position);
            let handle = self.term.raw_handle();
            let notches = ((dy.abs() / (3.0 * lh)).ceil() as usize).max(1);
            for _ in 0..notches {
                if let Some(bytes) = self.input.encode_mouse_wheel(dy > 0.0, pos, &ev.modifiers, handle, g, c) {
                    self.send_bytes(&bytes);
                }
            }
            return;
        }
        // Accumulate pixels and move whole rows: smooth input, row-snapped view.
        self.scroll_accum += dy;
        let rows = (self.scroll_accum / lh).trunc();
        if rows != 0.0 {
            self.scroll_accum -= rows * lh;
            self.scroll_rows(-(rows as isize));
            cx.notify();
        }
    }

    // ---- selection text --------------------------------------------------------

    pub fn word_bounds(&mut self, vrow: usize, col: usize) -> (usize, usize) {
        let screen = self.term.screen();
        if vrow >= screen.rows() {
            return (col, col);
        }
        let cells = screen.row(vrow);
        let is_word = |i: usize| {
            let c = &cells[i];
            if c.attrs.spacer {
                return true;
            }
            !c.grapheme.chars().all(|ch| ch.is_whitespace() || ",;\"'()[]{}<>|".contains(ch))
        };
        if col >= cells.len() || !is_word(col) {
            return (col, col);
        }
        let (mut a, mut b) = (col, col);
        while a > 0 && is_word(a - 1) {
            a -= 1;
        }
        while b + 1 < cells.len() && is_word(b + 1) {
            b += 1;
        }
        (a, b)
    }

    /// Visible part of the selection per viewport row: (row, first col, last col).
    pub fn selection_spans(&self) -> Vec<(usize, usize, usize)> {
        let Some(sel) = &self.sel else { return Vec::new() };
        let ((r0, c0), (r1, c1)) = sel.range();
        let offset = self.term.scrollbar_state().1 as usize;
        let mut out = Vec::new();
        for v in 0..self.rows {
            let ar = offset + v;
            if ar < r0 || ar > r1 {
                continue;
            }
            let a = if ar == r0 { c0 } else { 0 };
            let b = if ar == r1 { c1 } else { self.cols.saturating_sub(1) };
            out.push((v, a, b));
        }
        out
    }

    pub fn selection_text(&self) -> Option<String> {
        let spans = self.selection_spans();
        if spans.is_empty() {
            return None;
        }
        let screen = self.term.screen();
        let mut out = String::new();
        let n = spans.len();
        for (i, (v, a, b)) in spans.into_iter().enumerate() {
            let cells = screen.row(v);
            let mut line = String::new();
            for c in cells.iter().take(b + 1).skip(a) {
                if !c.attrs.spacer {
                    line.push_str(&c.grapheme);
                }
            }
            let wrapped = screen.is_row_wrapped(v);
            out.push_str(if wrapped { &line } else { line.trim_end() });
            if i + 1 < n && !wrapped {
                out.push('\n');
            }
        }
        Some(out)
    }

    // ---- after paint ---------------------------------------------------------------

    /// Called right after the element painted a frame (scene built, present
    /// follows in the same draw call).
    pub fn after_paint(&mut self, window: &mut Window) {
        self.frames += 1;
        if self.title != self.applied_title {
            self.applied_title = self.title.clone();
            window.set_window_title(&self.title);
        }
        let now = Instant::now();
        self.smoke_step(window, now);
        self.last_paint = Some(now);
    }

    /// Preedit width in cells (wide glyphs count 2).
    pub fn marked_cells(&self) -> usize {
        self.marked
            .as_ref()
            .map(|m| m.chars().map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 }).sum())
            .unwrap_or(0)
    }
}

/// Clipboard text to PTY bytes: newlines become CR, and with bracketed paste
/// (DECSET 2004) the text is wrapped and cannot close the bracket early.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let t = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return t.into_bytes();
    }
    let t = t.replace('\x1b', "");
    let mut out = Vec::with_capacity(t.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(t.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ---- IME (TSF) through GPUI's input handler -----------------------------------------

impl gpui::EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.marked.clone()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let n = self.marked.as_deref().map(utf16_len).unwrap_or(0);
        Some(UTF16Selection { range: n..n, reversed: false })
    }

    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_deref().map(|m| 0..utf16_len(m))
    }

    fn unmark_text(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Commit: composed text (or plain typed text the key path did not handle).
        self.marked = None;
        if !text.is_empty() {
            self.snap_to_bottom();
            self.send_bytes(text.as_bytes());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _sel: Option<Range<usize>>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Anchor the candidate window under the cursor cell.
        let (r, c) = self.term.cursor();
        let m = &self.metrics;
        Some(Bounds::new(
            point(element_bounds.origin.x + m.cell_w * c as f32, element_bounds.origin.y + m.line_h * r as f32),
            size(m.cell_w * self.marked_cells().max(1) as f32, m.line_h),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _p: Point<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// ---- row building ------------------------------------------------------------------

fn hash_row(screen: &Screen, r: usize) -> u64 {
    let mut h = DefaultHasher::new();
    for c in screen.row(r) {
        c.grapheme.hash(&mut h);
        match c.attrs.fg {
            Color::Default => 0u8.hash(&mut h),
            Color::Rgb(a, b, d) => (1u8, a, b, d).hash(&mut h),
        }
        match c.attrs.bg {
            Color::Default => 0u8.hash(&mut h),
            Color::Rgb(a, b, d) => (1u8, a, b, d).hash(&mut h),
        }
        let flags = (c.attrs.bold as u8)
            | (c.attrs.italic as u8) << 1
            | (c.attrs.underline as u8) << 2
            | (c.attrs.strikethrough as u8) << 3
            | (c.attrs.inverse as u8) << 4
            | (c.attrs.dim as u8) << 5
            | (c.attrs.wide as u8) << 6
            | (c.attrs.spacer as u8) << 7;
        flags.hash(&mut h);
    }
    h.finish()
}

#[derive(PartialEq, Clone, Copy)]
struct RunKey {
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn resolve(attrs: &CellAttributes, cfg: &Config) -> (Hsla, Hsla) {
    let fg = match attrs.fg {
        Color::Default => hsla(cfg.foreground),
        Color::Rgb(r, g, b) => rgb8(r, g, b),
    };
    let bg = match attrs.bg {
        Color::Default => hsla(cfg.background),
        Color::Rgb(r, g, b) => rgb8(r, g, b),
    };
    let (mut fg, bg) = if attrs.inverse { (bg, fg) } else { (fg, bg) };
    if attrs.dim {
        fg.a *= 0.6;
    }
    (fg, bg)
}

/// One row -> (text, text runs, background runs). Adjacent cells with the same
/// style merge. A wide glyph's spacer cell contributes one space so the grid
/// stays aligned (the glyph itself overflows into the blank spacer cell).
fn build_row(
    screen: &Screen,
    r: usize,
    base: &Font,
    cfg: &Config,
) -> (SharedString, Vec<TextRun>, Vec<(usize, usize, Hsla)>) {
    let default_bg = hsla(cfg.background);
    let mut text = String::with_capacity(screen.cols());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut bgs: Vec<(usize, usize, Hsla)> = Vec::new();
    let mut last_key: Option<RunKey> = None;

    for (c, cell) in screen.row(r).iter().enumerate() {
        let (fg, bg) = resolve(&cell.attrs, cfg);
        let g = if cell.grapheme.is_empty() || cell.attrs.spacer { " " } else { cell.grapheme.as_str() };
        text.push_str(g);

        let key = RunKey {
            fg,
            bold: cell.attrs.bold,
            italic: cell.attrs.italic,
            underline: cell.attrs.underline,
            strike: cell.attrs.strikethrough,
        };
        if last_key == Some(key) {
            runs.last_mut().unwrap().len += g.len();
        } else {
            let mut f = base.clone();
            if key.bold {
                f.weight = FontWeight::BOLD;
            }
            if key.italic {
                f.style = FontStyle::Italic;
            }
            runs.push(TextRun {
                len: g.len(),
                font: f,
                color: fg,
                background_color: None,
                underline: key.underline.then(|| UnderlineStyle { color: Some(fg), thickness: px(1.0), wavy: false }),
                strikethrough: key.strike.then(|| StrikethroughStyle { color: Some(fg), thickness: px(1.0) }),
            });
            last_key = Some(key);
        }

        if bg != default_bg {
            match bgs.last_mut() {
                Some(last) if last.1 == c && last.2 == bg => last.1 = c + 1,
                _ => bgs.push((c, c + 1, bg)),
            }
        }
    }
    (text.into(), runs, bgs)
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window_active = window.is_window_active();
        div()
            .size_full()
            .bg(hsla(self.cfg.background))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_modifiers_changed(cx.listener(Self::on_mods))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TerminalElement::new(cx.entity()))
    }
}
