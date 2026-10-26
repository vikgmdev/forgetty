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
use forgetty_shell_core::selection::{self, AUTOSCROLL_TICK};
use forgetty_shell_core::{copytext, paste as paste_rules, PaneEvent, PaneId, PaneSource};
use forgetty_vt::ffi;
use forgetty_vt::screen::{CellAttributes, Color};
use forgetty_vt::{RowText, Screen, Terminal};
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

const SHAPE_CACHE_MAX: usize = 4096;
const FEED_CHUNK: usize = 256 * 1024;
/// Replies to queries in replayed output are dropped, but never for longer than this: under
/// constant output the replay never "drains", and a program waiting for an answer must get it.
const REPLAY_MAX: Duration = Duration::from_secs(2);
/// How long the "input dropped" notice shows.
const DROP_NOTICE: Duration = Duration::from_secs(4);
const NOTICE_FOR: Duration = Duration::from_secs(8);

/// Scale brightness (same as a black overlay of alpha 1-k).
pub fn shade(c: Hsla, k: f32) -> Hsla {
    Hsla { l: c.l * k, ..c }
}

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

pub use forgetty_shell_core::selection::{Sel, SelMode};

fn make_term(cfg: &Config, rows: usize, cols: usize) -> Terminal {
    let palette: [CoreRgba; 16] = std::array::from_fn(|i| {
        let h = cfg.ansi[i];
        CoreRgba::rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
    });
    // libghostty-vt budgets scrollback in bytes (default 10 MiB per pane).
    Terminal::with_scrollback(rows, cols, palette, cfg.scrollback_bytes)
}

/// What a pane tells the shell about.
#[derive(Clone, Debug)]
pub enum ViewEvent {
    /// A program asked for the clipboard (OSC 52). Never sent for replayed output.
    Clipboard(forgetty_shell_core::osc52::Request),
}

/// Counts of OSC 52 requests seen live in this pane (the smoke reads them).
#[derive(Clone, Copy, Default, Debug)]
pub struct ClipboardSeen {
    pub writes: u32,
    pub too_large: u32,
    pub reads_denied: u32,
    /// Requests found inside replayed output: dropped.
    pub replayed: u32,
}

pub struct TerminalView {
    pub osc52: forgetty_shell_core::osc52::Scanner,
    pub clip_seen: ClipboardSeen,
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
    /// The pointer as last seen during a selection drag (window coordinates).
    last_pointer: Point<Pixels>,
    /// The auto-scroll timer of a selection drag is running.
    autoscrolling: bool,
    pub mouse_reporting: bool,
    pub marked: Option<String>,
    pub cursor_on: bool,
    pub bounds: Bounds<Pixels>,
    pub window_active: bool,
    /// Set by the shell: the pane is on screen (tab active and not hidden by zoom).
    pub visible: bool,
    /// Set by the shell: this is the active pane of the active tab.
    pub focused: bool,
    #[allow(dead_code)]
    pub pane_id: PaneId,
    pub closed: Option<String>,
    pub frames: u64,
    pub resizes: u64,
    pub last_paint: Option<Instant>,
    /// Smoke only: dry-run key encoding writes here instead of the source.
    pub capture_input: Option<Vec<u8>>,
    /// What the IME handler was asked, in order (the smoke reads it; at most 200 entries).
    pub ime_log: Vec<String>,
    /// How many times the IME asked where to put its candidate window, and the last answer
    /// `(cursor row, cursor col, x, y, w, h, element x, element y)`.
    pub ime_bounds_calls: u32,
    pub ime_bounds_last: Option<(usize, usize, f32, f32, f32, f32, f32, f32)>,
    pub pending_paste: Option<PendingPaste>,
    /// 0.0 normal, otherwise the share of brightness removed (unfocused pane in a split).
    pub dim: f32,
    /// Set after a VT reset: the next resize is sent even if the size is unchanged.
    pub force_size: bool,
    /// True from a reset until the replay has drained (or `REPLAY_MAX` passed).
    pub replaying: bool,
    replay_since: Option<Instant>,
    /// The source sends `ReplayEnd`, so the replay ends there and not by timer or drain.
    replay_exact: bool,
    /// A line shown for a few seconds (server restarted and input was lost).
    notice: Option<(String, Instant)>,
    /// "Size set by <name>" while another client owns the PTY size (AD-026).
    size_owner: Option<String>,
    /// The source's PTY belongs to whichever client typed last (AD-020): the VT keeps the
    /// size the server reports and is centered in the pane.
    letterbox: bool,
    /// What fits the window, in rows and columns. Equals `(rows, cols)` unless letterboxed.
    pub win_grid: (usize, usize),
    /// A dead key was pressed: the next key goes to the OS so it can compose (WM_CHAR).
    dead_pending: bool,
    /// Touchpad wheel pixels not yet turned into mouse-mode notches.
    wheel_accum: f32,
    /// Input bytes the source dropped, as last seen, and until when the notice shows.
    dropped_seen: u64,
    drop_notice_until: Option<Instant>,
    pub resets: u64,
    pub fed_bytes: u64,
    pub feed_time: Duration,
    pub max_feed: Duration,
}

/// A paste waiting for the user's confirmation (Enter pastes, Esc cancels).
pub struct PendingPaste {
    pub bytes: Vec<u8>,
    pub preview: Vec<String>,
    pub total_lines: usize,
    pub stripped: usize,
    /// Bytes of the text as copied. Shown when it is over `paste_warn_size`.
    pub size: usize,
    pub large: bool,
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

impl gpui::EventEmitter<ViewEvent> for TerminalView {}

impl TerminalView {
    pub fn new(
        cfg: Config,
        pane_id: PaneId,
        source: Box<dyn PaneSource>,
        rows: usize,
        cols: usize,
        smoke_mode: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let metrics = Metrics::measure(cx, &cfg);
        let (rows, cols) = (rows.clamp(1, 500), cols.clamp(2, 1000));
        let term = make_term(&cfg, rows, cols);
        let letterbox = source.letterbox();

        // Pump source output into the VT on the UI thread. Batches are capped
        // and the task yields between them so frames keep flowing while a
        // multi-megabyte burst drains.
        let rx = source.events();
        cx.spawn(async move |this, cx| {
            // A `Reset` is applied alone; whatever follows is a replay.
            let mut pending: Option<PaneEvent> = None;
            loop {
                let first = match pending.take() {
                    Some(e) => e,
                    None => match rx.recv().await {
                        Ok(e) => e,
                        Err(_) => break,
                    },
                };
                let mut batch = Vec::new();
                let mut closed = None;
                let mut reset = None;
                let mut replay_end = false;
                match first {
                    PaneEvent::Reset { rows, cols, exact_replay } => reset = Some((rows, cols, exact_replay)),
                    PaneEvent::Output(b) => batch = b,
                    PaneEvent::ReplayEnd => replay_end = true,
                    PaneEvent::Closed(r) => closed = Some(r),
                }
                while reset.is_none() && closed.is_none() && !replay_end && batch.len() < FEED_CHUNK {
                    match rx.try_recv() {
                        Ok(PaneEvent::Output(b)) => batch.extend_from_slice(&b),
                        // The batch so far is replay; what follows is live.
                        Ok(PaneEvent::ReplayEnd) => replay_end = true,
                        Ok(PaneEvent::Closed(r)) => closed = Some(r),
                        Ok(r @ PaneEvent::Reset { .. }) => {
                            pending = Some(r);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let caught_up = pending.is_none() && rx.is_empty();
                if this.update(cx, |v, cx| v.feed(reset, &batch, closed, caught_up, replay_end, cx)).is_err() {
                    break;
                }
                YieldNow(false).await;
            }
        })
        .detach();

        if !smoke_mode {
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
            rows,
            cols,
            row_hash: vec![0; rows],
            rows_shaped: vec![None; rows],
            shape_cache: HashMap::new(),
            shape_misses: 0,
            rows_reshaped_last: 0,
            last_gen: 0,
            focus: cx.focus_handle(),
            scroll_accum: 0.0,
            sel: None,
            selecting: false,
            last_pointer: Point::default(),
            autoscrolling: false,
            mouse_reporting: false,
            marked: None,
            cursor_on: true,
            bounds: Bounds::default(),
            window_active: true,
            visible: true,
            focused: false,
            pane_id,
            closed: None,
            frames: 0,
            resizes: 0,
            last_paint: None,
            capture_input: None,
            ime_log: Vec::new(),
            ime_bounds_calls: 0,
            ime_bounds_last: None,
            pending_paste: None,
            dim: 0.0,
            force_size: false,
            replaying: false,
            osc52: Default::default(),
            clip_seen: Default::default(),
            replay_since: None,
            replay_exact: false,
            notice: None,
            size_owner: None,
            letterbox,
            win_grid: (rows, cols),
            dead_pending: false,
            wheel_accum: 0.0,
            dropped_seen: 0,
            drop_notice_until: None,
            resets: 0,
            fed_bytes: 0,
            feed_time: Duration::ZERO,
            max_feed: Duration::ZERO,
        }
    }

    // ---- output ---------------------------------------------------------

    /// Start over with an empty VT at the PTY's size (a remote pane attached,
    /// or the server's log no longer holds our offset). The next frame sees the
    /// size mismatch with the window and resizes the VT and the PTY.
    fn reset_term(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = ((rows as usize).clamp(1, 500), (cols as usize).clamp(2, 1000));
        self.term = make_term(&self.cfg, rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.row_hash = vec![0; rows];
        self.rows_shaped = vec![None; rows];
        self.last_gen = 0;
        self.sel = None;
        self.scroll_accum = 0.0;
        self.force_size = true;
        self.resets += 1;
    }

    fn feed(
        &mut self,
        reset: Option<(u16, u16, bool)>,
        bytes: &[u8],
        closed: Option<String>,
        caught_up: bool,
        replay_end: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some((rows, cols, exact)) = reset {
            self.osc52 = Default::default();
            self.reset_term(rows, cols);
            self.replaying = true;
            self.replay_exact = exact;
            self.replay_since = Some(Instant::now());
        }
        if !bytes.is_empty() {
            let t0 = Instant::now();
            self.term.feed(bytes);
            let dt = t0.elapsed();
            // libghostty-vt does not surface OSC 52, so this view reads the bytes itself.
            // Replayed output is history: a program asked for that clipboard write long ago.
            for req in self.osc52.feed(bytes) {
                if self.replaying {
                    self.clip_seen.replayed += 1;
                    continue;
                }
                match &req {
                    forgetty_shell_core::osc52::Request::Write { .. } => self.clip_seen.writes += 1,
                    forgetty_shell_core::osc52::Request::TooLarge => self.clip_seen.too_large += 1,
                    forgetty_shell_core::osc52::Request::ReadDenied => self.clip_seen.reads_denied += 1,
                }
                cx.emit(ViewEvent::Clipboard(req));
            }
            self.fed_bytes += bytes.len() as u64;
            self.feed_time += dt;
            self.max_feed = self.max_feed.max(dt);
            // Terminal replies (DA, CPR, mode queries). ConPTY waits on these.
            // A replay after a reset re-runs old queries: their answers would reach
            // the program a second time, so they are dropped.
            for resp in self.term.drain_write_pty() {
                if self.replaying && !self.replay_exact && self.replay_since.is_some_and(|t| t.elapsed() > REPLAY_MAX) {
                    self.replaying = false;
                }
                if !self.replaying {
                    // AD-025: not typing. The server does not count it as activity.
                    self.source.write_auto(&resp);
                }
            }
            // Titles and cwd come from the backend (it scans the same bytes).
            drop(self.term.drain_events());
        }
        // With `replay_end` from the server the replay ends exactly there. Without it, the
        // replay is over when the channel drains or after `REPLAY_MAX`.
        if replay_end || (!self.replay_exact && (caught_up || self.replay_since.is_some_and(|t| t.elapsed() > REPLAY_MAX))) {
            self.replaying = false;
        }
        if let Some(reason) = closed {
            self.closed = Some(reason);
        }
        // Panes on hidden tabs keep their VT current but do not ask for frames.
        if self.visible {
            cx.notify();
        }
    }

    /// The pane gained or lost the user's focus (active pane, window active).
    /// Sends DECSET 1004 focus events when the program asked for them.
    pub fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.cursor_on = true;
        if self.term.is_focus_reporting() {
            if let Some(b) = Input::encode_focus(focused) {
                self.send_auto(&b);
            }
        }
        cx.notify();
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            if visible {
                self.last_gen = 0;
                cx.notify();
            }
        }
    }

    /// Dim the pane by recoloring (no overlay quad: full-pane alpha quads are costly
    /// on software GPUs). Rows are rebuilt with scaled colors.
    pub fn set_dim(&mut self, dim: f32, cx: &mut Context<Self>) {
        if (self.dim - dim).abs() > 1e-3 {
            self.dim = dim;
            self.last_gen = 0;
            self.row_hash.iter_mut().for_each(|h| *h = 0);
            cx.notify();
        }
    }

    fn blink_tick(&mut self, cx: &mut Context<Self>) {
        if !self.visible || !self.focused {
            self.cursor_on = true;
            return;
        }
        if self.term.cursor_visible() && self.term.cursor_blinking() && self.window_active {
            self.cursor_on = !self.cursor_on;
            cx.notify();
        } else if !self.cursor_on {
            self.cursor_on = true;
            cx.notify();
        }
    }

    /// This window became the active client again: send the grid size once more
    /// so the server's size policy (latest active client wins) picks ours.
    pub fn reassert_size(&self) {
        if self.visible && self.rows > 0 && self.cols > 0 {
            self.source.reassert_size(self.rows as u16, self.cols as u16);
        }
    }

    // ---- frame preparation (called from the element's prepaint) -----------

    /// Follow the window: at most one resize per frame, since this runs once
    /// per prepaint.
    pub fn apply_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;

        let cols = ((f32::from(bounds.size.width) / f32::from(self.metrics.cell_w) + 0.1).floor() as usize).clamp(2, 1000);
        let rows = ((f32::from(bounds.size.height) / f32::from(self.metrics.line_h) + 0.1).floor() as usize).clamp(1, 500);
        if self.letterbox {
            // The VT keeps the PTY's size (the latest active client owns it, AD-020). Only
            // tell the server what fits here: it applies this when we are the active client.
            self.win_grid = (rows, cols);
            let again = std::mem::take(&mut self.force_size);
            if again {
                self.source.reassert_size(rows as u16, cols as u16);
            } else {
                self.source.resize(rows as u16, cols as u16);
            }
            return;
        }
        if rows == self.rows && cols == self.cols {
            if std::mem::take(&mut self.force_size) {
                self.source.reassert_size(rows as u16, cols as u16);
            }
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.resizes += 1;
        self.term.resize(rows, cols);
        if std::mem::take(&mut self.force_size) {
            self.source.reassert_size(rows as u16, cols as u16);
        } else {
            self.source.resize(rows as u16, cols as u16);
        }
        self.row_hash = vec![0; rows];
        self.rows_shaped = vec![None; rows];
        self.last_gen = 0;
        self.sel = None;
    }

    /// The server reported a new PTY size for this pane (`PaneResized`). A letterboxed view
    /// follows it, whoever asked for it.
    pub fn set_pty_size(&mut self, rows: u16, cols: u16, cx: &mut Context<Self>) {
        if !self.letterbox {
            return;
        }
        let (rows, cols) = ((rows as usize).clamp(1, 500), (cols as usize).clamp(2, 1000));
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.resizes += 1;
        self.term.resize(rows, cols);
        self.row_hash = vec![0; rows];
        self.rows_shaped = vec![None; rows];
        self.last_gen = 0;
        self.sel = None;
        cx.notify();
    }

    /// Offset of the (smaller) PTY grid inside the pane: it sits in the middle. Zero when the
    /// grid fills the pane, or is larger than it, or the pane is not letterboxed.
    pub fn letterbox_pad(&self) -> (f32, f32) {
        if !self.letterbox {
            return (0.0, 0.0);
        }
        let (w, h) = (f32::from(self.bounds.size.width), f32::from(self.bounds.size.height));
        let gw = f32::from(self.metrics.cell_w) * self.cols as f32;
        let gh = f32::from(self.metrics.line_h) * self.rows as f32;
        (((w - gw) / 2.0).max(0.0).floor(), ((h - gh) / 2.0).max(0.0).floor())
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
            let h = hash_row(screen, r) ^ if self.dim > 0.0 { 0x9E37_79B9_7F4A_7C15 } else { 0 };
            if self.rows_shaped[r].is_some() && self.row_hash[r] == h {
                continue;
            }
            self.row_hash[r] = h;
            let shaped = match self.shape_cache.get(&h) {
                Some(s) => s.clone(),
                None => {
                    let m = &self.metrics;
                    let (text, runs, bgs) = build_row(screen, r, &m.font, &self.cfg, 1.0 - self.dim);
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

    // ---- clear and reset -----------------------------------------------------------

    /// Ctrl+L to the program: it clears the screen and draws its prompt (GTK's "Clear").
    pub fn clear_terminal(&mut self, cx: &mut Context<Self>) {
        self.snap_to_bottom();
        self.send_bytes(&[0x0c]);
        cx.notify();
    }

    /// Reset the terminal state, then Ctrl+L so the shell draws its prompt again (GTK's
    /// "Reset"). This resets this client's VT: colors, modes, scrollback. The program is not
    /// told, as in GTK.
    pub fn reset_terminal(&mut self, cx: &mut Context<Self>) {
        self.term.reset();
        self.row_hash = vec![0; self.rows];
        self.rows_shaped = vec![None; self.rows];
        self.last_gen = 0;
        self.sel = None;
        self.scroll_accum = 0.0;
        self.send_bytes(&[0x0c]);
        cx.notify();
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

    /// Bytes the user typed, pasted or clicked.
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        match self.capture_input.as_mut() {
            Some(buf) => buf.extend_from_slice(bytes),
            None => self.source.write_input(bytes),
        }
    }

    /// Bytes the program did not get from the user: focus reports (AD-025).
    pub fn send_auto(&mut self, bytes: &[u8]) {
        match self.capture_input.as_mut() {
            Some(buf) => buf.extend_from_slice(bytes),
            None => self.source.write_auto(bytes),
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
        if self.pending_paste.is_some() {
            match ks.key.as_str() {
                "enter" => self.resolve_paste(true),
                "escape" => self.resolve_paste(false),
                _ => {}
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
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
        // Dead keys (US-International ', ^ in French, ´ in Spanish): the OS composes. The dead
        // key press itself and the key after it go to TranslateMessage (we do not stop the
        // event), and the finished text arrives through WM_CHAR and `replace_text_in_range`.
        // A key that "prefers character input" with no ctrl or alt held is a dead key.
        if std::mem::take(&mut self.dead_pending) {
            return;
        }
        if ev.prefer_character_input && !m.control && !m.alt && !m.platform && ks.key_char.is_some() {
            self.dead_pending = true;
            return;
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
        cx.notify();
    }

    /// Sanitize and send, or park the paste for confirmation when it could run
    /// commands the user did not see (newline without bracketed paste, hidden
    /// control bytes). Returns true if a confirm prompt is now showing.
    pub fn send_paste(&mut self, text: &str) -> bool {
        let bracketed = encoder::private_mode(self.term.raw_handle(), 2004);
        let p = forgetty_vt::paste::prepare(text, bracketed);
        // GTK asks only for a big paste. This also asks for what could run commands unseen.
        let large = paste_rules::is_large(text, self.cfg.paste_warn_size);
        if p.needs_confirm || large {
            self.pending_paste = Some(PendingPaste {
                bytes: p.bytes,
                preview: text.lines().take(8).map(visible).collect(),
                total_lines: text.lines().count().max(1),
                stripped: p.stripped,
                size: text.len(),
                large,
            });
            return true;
        }
        self.snap_to_bottom();
        self.send_bytes(&p.bytes);
        false
    }

    pub fn resolve_paste(&mut self, accept: bool) {
        if let Some(p) = self.pending_paste.take() {
            if accept {
                self.snap_to_bottom();
                self.send_bytes(&p.bytes);
            }
        }
    }

    /// A line at the bottom while input is being dropped (no link and the buffer is full).
    fn drop_overlay(&mut self) -> Option<gpui::Div> {
        let dropped = self.source.dropped_input_bytes();
        if dropped > self.dropped_seen {
            self.dropped_seen = dropped;
            self.drop_notice_until = Some(Instant::now() + DROP_NOTICE);
        }
        let now = Instant::now();
        let mut lines: Vec<String> = Vec::new();
        if let Some((text, until)) = &self.notice {
            if now < *until {
                lines.push(text.clone());
            }
        }
        if self.drop_notice_until.is_some_and(|t| now < t) {
            lines.push(format!("Input dropped ({dropped} bytes in all): the link is down and the buffer is full"));
        }
        if lines.is_empty() {
            self.notice = None;
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .p_1()
                .bg(hsla(0x5a1d1d))
                .text_color(hsla(0xffffff))
                .font_family(self.metrics.font.family.clone())
                .text_size(self.metrics.font_size)
                .children(lines),
        )
    }

    /// Show `text` at the bottom of the pane for a few seconds. It does not take the focus or
    /// block input.
    pub fn show_notice(&mut self, text: String, cx: &mut Context<Self>) {
        self.notice = Some((text, Instant::now() + NOTICE_FOR));
        cx.notify();
        // Repaint when it expires, even if nothing else happens.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR + Duration::from_millis(100)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// The notice on screen now (smoke).
    pub fn notice_text(&self) -> Option<String> {
        self.notice.as_ref().filter(|(_, t)| Instant::now() < *t).map(|(s, _)| s.clone())
    }

    /// The client that owns the PTY size changed (AD-026). `None` clears the label.
    pub fn set_size_owner(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        if self.size_owner != label {
            self.size_owner = label;
            cx.notify();
        }
    }

    /// "Size set by <client>" in the letterbox area. Only shown where there is a margin: when
    /// the PTY grid fills the pane (this client owns the size) there is nothing to explain.
    fn owner_label(&self) -> Option<gpui::Div> {
        let label = self.size_owner.as_ref()?;
        let (px_, py_) = self.letterbox_pad();
        if px_ < 1.0 && py_ < 1.0 {
            return None;
        }
        // The bigger margin holds it: below the grid when there is room above and below,
        // otherwise at the top of the pane.
        let mut d = div()
            .absolute()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .text_color(hsla(0x8a8f98))
            .font_family(self.metrics.font.family.clone())
            .text_size(self.metrics.font_size * 0.85)
            .child(label.clone());
        d = if py_ >= self.metrics.line_h.into() { d.bottom(px((py_ - f32::from(self.metrics.line_h)).max(0.0) / 2.0)) } else { d.top_0() };
        Some(d)
    }

    /// The owner label on screen (smoke).
    pub fn size_owner_label(&self) -> Option<String> {
        self.size_owner.clone()
    }

    /// Bytes of input this pane dropped so far (smoke and status).
    pub fn dropped_input(&self) -> u64 {
        self.source.dropped_input_bytes()
    }

    fn paste_overlay(&self) -> Option<gpui::Div> {
        let p = self.pending_paste.as_ref()?;
        let mut d = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .p_2()
            .bg(hsla(0x202020))
            .border_b_1()
            .border_color(hsla(0xe5c07b))
            .text_color(hsla(0xffffff))
            .font_family(self.metrics.font.family.clone())
            .text_size(self.metrics.font_size)
            .flex()
            .flex_col();
        let mut note = if p.large { format!(", {}", paste_rules::size_label(p.size)) } else { String::new() };
        if p.stripped > 0 {
            note.push_str(&format!(", {} control char(s) removed", p.stripped));
        }
        d = d.child(format!(
            "Paste {} line(s){note}?  Enter = paste, Esc = cancel",
            p.total_lines
        ));
        for l in &p.preview {
            d = d.child(SharedString::from(l.clone()));
        }
        if p.total_lines > p.preview.len() {
            d = d.child(format!("... {} more line(s)", p.total_lines - p.preview.len()));
        }
        Some(d)
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        self.copy_selection_as(self.cfg.smart_copy, cx);
    }

    /// Copy the selection. `smart` strips box-drawing characters as well (trailing spaces and
    /// line endings are always cleaned).
    pub fn copy_selection_as(&mut self, smart: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text_as(smart) {
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    /// Select what is on screen now (GTK's "Select All" does the same: not the scrollback).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let top = self.term.scrollbar_state().1 as usize;
        self.sel = Some(Sel::all(top, top + self.rows.saturating_sub(1), self.cols.saturating_sub(1)));
        self.selecting = false;
        cx.notify();
    }

    // ---- mouse ----------------------------------------------------------------

    fn local(&self, p: Point<Pixels>) -> (f32, f32) {
        let (px_, py_) = self.letterbox_pad();
        (
            (f32::from(p.x) - f32::from(self.bounds.origin.x) - px_).max(0.0),
            (f32::from(p.y) - f32::from(self.bounds.origin.y) - py_).max(0.0),
        )
    }

    /// Pointer position inside the text area; negative above and left of it.
    fn local_unclamped(&self, p: Point<Pixels>) -> (f32, f32) {
        let (px_, py_) = self.letterbox_pad();
        (
            f32::from(p.x) - f32::from(self.bounds.origin.x) - px_,
            f32::from(p.y) - f32::from(self.bounds.origin.y) - py_,
        )
    }

    fn cell_at(&self, p: Point<Pixels>) -> (usize, usize) {
        let (x, y) = self.local(p);
        let col = ((x / f32::from(self.metrics.cell_w)) as usize).min(self.cols.saturating_sub(1));
        let row = ((y / f32::from(self.metrics.line_h)) as usize).min(self.rows.saturating_sub(1));
        (row, col)
    }

    /// Pointer position and grid geometry for the mouse encoder.
    fn mouse_args(&self, p: Point<Pixels>) -> ((f32, f32), (u32, u32), (u32, u32)) {
        let pos = self.local(p);
        let pixels = self.term.dec_mode(1016);
        mouse_geom(pos, f32::from(self.metrics.cell_w), f32::from(self.metrics.line_h), self.cols, self.rows, pixels)
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
            let (pos, g, c) = self.mouse_args(ev.position);
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
                // Shift+click moves the end of an existing selection (GTK: no drag after it).
                if ev.modifiers.shift && ev.click_count <= 1 {
                    if let Some(sel) = self.sel.as_mut() {
                        sel.extend_to((ar, col), (ar, col));
                        self.selecting = false;
                        cx.notify();
                        return;
                    }
                }
                let (mode, u0, u1) = match ev.click_count {
                    0 | 1 => (SelMode::Normal, (ar, col), (ar, col)),
                    2 => {
                        let (c0, c1) = self.word_bounds(vr, col);
                        (SelMode::Word, (ar, c0), (ar, c1))
                    }
                    _ => (SelMode::Line, (ar, 0), (ar, self.cols.saturating_sub(1))),
                };
                self.sel = Some(Sel::new(mode, u0, u1));
                self.selecting = true;
                self.last_pointer = ev.position;
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

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        if self.wants_mouse(ev.modifiers.shift) {
            let (pos, g, c) = self.mouse_args(ev.position);
            let handle = self.term.raw_handle();
            if let Some(bytes) = self.input.encode_mouse_motion(pos, &ev.modifiers, handle, g, c) {
                self.send_bytes(&bytes);
            }
            return;
        }
    }

    /// Every mouse move of the window, also while the pointer is outside the pane (it is
    /// captured while a button is held). The element registers this each frame.
    pub fn on_pointer_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.finish_selection(cx);
            return;
        }
        self.last_pointer = ev.position;
        self.extend_to_pointer(cx);
        self.autoscroll_ensure(cx);
    }

    /// The left button went up anywhere in the window.
    pub fn on_pointer_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Left && self.selecting {
            self.finish_selection(cx);
        }
    }

    /// Move the end of the selection under the pointer (clamped to the grid).
    fn extend_to_pointer(&mut self, cx: &mut Context<Self>) {
        let (vr, col) = self.cell_at(self.last_pointer);
        let ar = self.abs_row(vr);
        let (h0, h1) = match self.sel.as_ref().map(|s| s.mode) {
            Some(SelMode::Word) => {
                let (c0, c1) = self.word_bounds(vr, col);
                ((ar, c0), (ar, c1))
            }
            Some(SelMode::Line) => ((ar, 0), (ar, self.cols.saturating_sub(1))),
            _ => ((ar, col), (ar, col)),
        };
        if self.sel.as_mut().is_some_and(|s| s.extend_to(h0, h1)) {
            cx.notify();
        }
    }

    /// Rows to scroll per tick for the pointer where it is now (see `selection::autoscroll_rows`).
    fn autoscroll_amount(&self) -> isize {
        let (_, y) = self.local_unclamped(self.last_pointer);
        selection::autoscroll_rows(y, self.rows as f32 * f32::from(self.metrics.line_h), f32::from(self.metrics.line_h))
    }

    /// While a drag holds the pointer above or below the text, scroll on a timer. GTK scrolls
    /// only when the mouse moves; this keeps going while the pointer rests there. The timer
    /// runs only during such a drag.
    fn autoscroll_ensure(&mut self, cx: &mut Context<Self>) {
        if self.autoscrolling || self.autoscroll_amount() == 0 {
            return;
        }
        self.autoscrolling = true;
        cx.spawn(async move |this, cx| loop {
            let keep = this.update(cx, |v, cx| v.autoscroll_tick(cx)).unwrap_or(false);
            if !keep {
                break;
            }
            cx.background_executor().timer(AUTOSCROLL_TICK).await;
        })
        .detach();
    }

    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let rows = if self.selecting { self.autoscroll_amount() } else { 0 };
        if rows == 0 {
            self.autoscrolling = false;
            return false;
        }
        self.scroll_rows(rows);
        self.extend_to_pointer(cx);
        cx.notify();
        true
    }

    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.sel.as_ref().is_some_and(|s| !s.is_empty()) {
            self.copy_selection(cx); // copy on select
        } else {
            self.sel = None;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_reporting {
            self.mouse_reporting = false;
            let (pos, g, c) = self.mouse_args(ev.position);
            let handle = self.term.raw_handle();
            let b = encoder::mouse_button_to_ghostty(ev.button);
            if let Some(bytes) = self.input.encode_mouse_button(b, false, pos, &ev.modifiers, handle, g, c) {
                self.send_bytes(&bytes);
            }
            return;
        }
        if ev.button == MouseButton::Left && self.selecting {
            self.finish_selection(cx);
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
            let (pos, g, c) = self.mouse_args(ev.position);
            let handle = self.term.raw_handle();
            // Touchpads send many small deltas: add them up and send one notch per 3 rows.
            self.wheel_accum += dy;
            let step = 3.0 * lh;
            let notches = (self.wheel_accum.abs() / step).trunc() as usize;
            if notches == 0 {
                return;
            }
            let up = self.wheel_accum > 0.0;
            self.wheel_accum -= self.wheel_accum.signum() * notches as f32 * step;
            for _ in 0..notches {
                if let Some(bytes) = self.input.encode_mouse_wheel(up, pos, &ev.modifiers, handle, g, c) {
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

    /// The selected text for the clipboard (smart copy unless `smart_copy = false`).
    pub fn selection_text(&mut self) -> Option<String> {
        self.selection_text_as(self.cfg.smart_copy)
    }

    /// The selected text, including rows that scrolled out of the viewport. Rows are read by
    /// absolute number, so the viewport stays where it is.
    pub fn selection_text_as(&mut self, smart: bool) -> Option<String> {
        let ((r0, c0), (r1, c1)) = self.sel.as_ref()?.range();
        let total = self.term.scrollbar_state().0 as usize;
        let r1 = r1.min(total.saturating_sub(1));
        if r0 > r1 {
            return None;
        }
        let mut rt = RowText::default();
        let mut rows: Vec<(String, bool)> = Vec::with_capacity(r1 - r0 + 1);
        for ar in r0..=r1 {
            if !self.term.row_text(ar, &mut rt) {
                continue; // the row is gone (scrollback was trimmed)
            }
            let first = if ar == r0 { c0 } else { 0 };
            let last = if ar == r1 { c1 } else { usize::MAX };
            rows.push((copytext::slice_cols(&rt.text, &rt.cols, first, last), rt.wrapped));
        }
        let joined = copytext::join_rows(&rows);
        Some(if smart { copytext::smart_copy(&joined) } else { copytext::raw_copy(&joined) })
    }

    // ---- after paint ---------------------------------------------------------------

    /// Called right after the element painted a frame (scene built, present
    /// follows in the same draw call).
    pub fn after_paint(&mut self, window: &mut Window) {
        let _ = window;
        self.frames += 1;
        self.last_paint = Some(Instant::now());
    }

    fn ime_note(&mut self, what: String) {
        if self.ime_log.len() < 200 {
            self.ime_log.push(what);
        }
    }

    /// Preedit width in cells (wide glyphs count 2).
    pub fn marked_cells(&self) -> usize {
        self.marked
            .as_ref()
            .map(|m| m.chars().map(|c| if (c as u32) >= 0x1100 { 2 } else { 1 }).sum())
            .unwrap_or(0)
    }
}

/// Pixel geometry for the libghostty-vt mouse encoder. Its cell size is a whole number of
/// pixels, but a cell is 8.6 px wide, so it would count columns wrong (at column 100 it is off
/// by 5). The column and row are worked out here from the float cell size, and the encoder is
/// given a grid of 64x64 "virtual pixel" cells with the pointer at the same place inside the
/// same cell. In pixel mode (DECSET 1016) the encoder reports real pixels, so those go through.
pub fn mouse_geom(
    pos: (f32, f32),
    cell_w: f32,
    line_h: f32,
    cols: usize,
    rows: usize,
    pixels: bool,
) -> ((f32, f32), (u32, u32), (u32, u32)) {
    const V: f32 = 64.0;
    let (cols, rows) = (cols.max(1), rows.max(1));
    if pixels {
        return (
            pos,
            ((cell_w * cols as f32).round() as u32, (line_h * rows as f32).round() as u32),
            (cell_w.round() as u32, line_h.round() as u32),
        );
    }
    let axis = |p: f32, cell: f32, n: usize| -> f32 {
        let cells = (p / cell).max(0.0);
        let idx = cells.floor() as usize;
        if idx >= n {
            return (n as f32 - 0.5) * V;
        }
        (idx as f32 + (cells - idx as f32).clamp(0.0, 0.99)) * V
    };
    (
        (axis(pos.0, cell_w, cols), axis(pos.1, line_h, rows)),
        (cols as u32 * V as u32, rows as u32 * V as u32),
        (V as u32, V as u32),
    )
}

/// Smoke: the mouse report for a click in column `c` is `c + 1` at 8.6 px cells, wide grid.
pub fn mouse_selftest() -> Result<(), String> {
    let palette: [CoreRgba; 16] = std::array::from_fn(|_| CoreRgba::rgb(0, 0, 0));
    let mut term = Terminal::with_scrollback(40, 200, palette, 1 << 20);
    term.feed(b"\x1b[?1000h\x1b[?1006h");
    let mut input = Input::new();
    let (cw, lh) = (8.6_f32, 19.3_f32);
    for (col, row) in [(0usize, 0usize), (1, 1), (57, 3), (100, 5), (150, 20), (199, 39)] {
        for frac in [0.05_f32, 0.5, 0.95] {
            let pos = ((col as f32 + frac) * cw, (row as f32 + frac) * lh);
            let (p, g, c) = mouse_geom(pos, cw, lh, 200, 40, false);
            let bytes = input
                .encode_mouse_button(ffi::GHOSTTY_MOUSE_BUTTON_LEFT, true, p, &gpui::Modifiers::default(), term.raw_handle(), g, c)
                .ok_or("no mouse report")?;
            let want = format!("\x1b[<0;{};{}M", col + 1, row + 1);
            if bytes != want.as_bytes() {
                return Err(format!("col {col} row {row} frac {frac}: got {:?}, want {:?}", String::from_utf8_lossy(&bytes), want));
            }
            // release, so the next press is a fresh press
            let _ = input.encode_mouse_button(ffi::GHOSTTY_MOUSE_BUTTON_LEFT, false, p, &gpui::Modifiers::default(), term.raw_handle(), g, c);
        }
    }
    Ok(())
}

/// Make control characters visible in the confirm preview.
fn visible(line: &str) -> String {
    let mut out = String::new();
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push((c as u8 + 64) as char);
            }
            '\u{7f}' => out.push_str("^?"),
            c if (0x80..=0x9f).contains(&(c as u32)) => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
        if out.chars().count() > 160 {
            out.push('…');
            break;
        }
    }
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
        self.ime_note("unmark".into());
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
        self.ime_note(format!("commit:{text}"));
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
        self.ime_note(format!("mark:{new_text}"));
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
        // Anchor the candidate window under the cursor cell. The cursor is read from the render
        // state, which `screen()` brings up to date (output may have arrived since the last frame).
        let _ = self.term.screen();
        let (r, c) = self.term.cursor();
        let m = &self.metrics;
        let b = Bounds::new(
            point(element_bounds.origin.x + m.cell_w * c as f32, element_bounds.origin.y + m.line_h * r as f32),
            size(m.cell_w * self.marked_cells().max(1) as f32, m.line_h),
        );
        self.ime_bounds_calls += 1;
        self.ime_bounds_last = Some((
            r,
            c,
            f32::from(b.origin.x),
            f32::from(b.origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height),
            f32::from(element_bounds.origin.x),
            f32::from(element_bounds.origin.y),
        ));
        Some(b)
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
    k: f32,
) -> (SharedString, Vec<TextRun>, Vec<(usize, usize, Hsla)>) {
    let default_bg = shade(hsla(cfg.background), k);
    let mut text = String::with_capacity(screen.cols());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut bgs: Vec<(usize, usize, Hsla)> = Vec::new();
    let mut last_key: Option<RunKey> = None;

    for (c, cell) in screen.row(r).iter().enumerate() {
        let (fg, bg) = resolve(&cell.attrs, cfg);
        let (fg, bg) = (shade(fg, k), shade(bg, k));
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
            .bg(shade(hsla(self.cfg.background), 1.0 - self.dim))
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
            // Released outside the pane (the pointer is captured while a button is held).
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                div()
                    .size_full()
                    .px(px(self.cfg.pane_padding_x))
                    .py(px(self.cfg.pane_padding_y))
                    .child(TerminalElement::new(cx.entity())),
            )
            .children(self.paste_overlay())
            .children(self.owner_label())
            .children(self.drop_overlay())
    }
}
