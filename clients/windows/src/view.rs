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
use forgetty_shell_core::bell::{self, BellGate};
use forgetty_shell_core::codeblock::{self, CodeBlock};
use forgetty_shell_core::links::{self, Link, LogicalLine};
use forgetty_shell_core::scrollbar;
use forgetty_shell_core::search::{self, SearchJob};
use forgetty_shell_core::selection::{self, AUTOSCROLL_TICK};
use forgetty_shell_core::copytext::CopyJob;
use forgetty_shell_core::fingerprint::{self, Fingerprint};
use forgetty_shell_core::{paste as paste_rules, PaneEvent, PaneId, PaneSource};
use forgetty_vt::ffi;
use forgetty_vt::screen::{CellAttributes, Color};
use forgetty_vt::{RowText, Screen, Terminal, TerminalEvent};
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
use crate::lineedit::{EditResult, LineEdit};

const SHAPE_CACHE_MAX: usize = 4096;
const FEED_CHUNK: usize = 256 * 1024;
/// Replies to queries in replayed output are dropped, but never for longer than this: under
/// constant output the replay never "drains", and a program waiting for an answer must get it.
const REPLAY_MAX: Duration = Duration::from_secs(2);
/// How long the "input dropped" notice shows.
const DROP_NOTICE: Duration = Duration::from_secs(4);
const NOTICE_FOR: Duration = Duration::from_secs(8);
/// The copy button of a code block, in px (GTK: 24).
pub const CODE_BTN: f32 = 24.0;
/// How long the copy button shows a check mark after a copy.
const COPIED_FOR: Duration = Duration::from_millis(1500);
/// Code blocks are looked for at most this often while the pointer moves over changing output.
const CODE_SCAN_GAP: Duration = Duration::from_millis(50);
/// Width of the scrollbar and the shortest thumb, in px.
const SB_W: f32 = 10.0;
const SB_MIN_THUMB: f32 = 24.0;
/// One slice of a scrollback search on the UI thread. Input and paint run between slices.
const SEARCH_SLICE: Duration = Duration::from_millis(4);
/// New output makes the matches stale. The scan starts again after this, not on every chunk.
const SEARCH_RESTART_GAP: Duration = Duration::from_millis(500);

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
    /// The pane's bell rang and passed the rate gate, and the bell mode is not `none`.
    Bell { pane: PaneId },
    /// The user asked for the pane's context menu at a window position.
    ContextMenu { pane: PaneId, pos: (f32, f32), url: Option<String>, has_selection: bool },
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
    /// The scrollbar thumb is held: where in the thumb it was grabbed, in px from its top.
    sb_drag: Option<f32>,
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
    pub search: Option<SearchState>,
    /// The http(s) link under the pointer, and the cell and screen generation it was found for.
    pub hover_link: Option<Link>,
    hover_cell: Option<(usize, usize)>,
    hover_gen: u64,
    /// Smoke: opening a link only records it.
    smoke_mode: bool,
    pub opened_urls: Vec<String>,
    /// Code blocks of the viewport, found when the pointer moves over changed output.
    code_blocks: Vec<CodeBlock>,
    code_gen: Option<u64>,
    code_scanned: Option<Instant>,
    pub hover_block: Option<CodeBlock>,
    copied_until: Option<Instant>,
    bell: BellGate,
    bell_until: Option<Instant>,
    /// Bells that passed the rate limit (the smoke reads it).
    pub bells: u32,
    /// The grid size or the VT changed since the last frame: row numbers moved.
    layout_changed: bool,
    /// 0.0 normal, otherwise the share of brightness removed (unfocused pane in a split).
    pub dim: f32,
    /// Set after a VT reset: the next resize is sent even if the size is unchanged.
    pub force_size: bool,
    /// libghostty refused this size: do not try it again until the target changes.
    pub(crate) resize_refused: Option<(usize, usize)>,
    /// A copy over many rows, read in slices: the job and whether to smart-copy.
    copy_task: Option<CopyTask>,
    copy_driver: bool,
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

/// The find bar of a pane and the scan behind it.
pub struct SearchState {
    pub edit: LineEdit,
    /// The scan for the current query. Holds the matches (all of them once it is done).
    pub job: Option<SearchJob>,
    /// Index into `job.matches` of the match Enter moved to.
    pub current: usize,
    /// When the scan ends, move to the first match at or below the viewport and scroll to it.
    /// Set by a new query, not by a restart for new output (that must not move the view).
    jump: bool,
    /// The match (row, col) that was current before a restart for new output.
    keep: Option<(u32, u16)>,
    /// Output arrived since the scan started.
    stale: bool,
    started: Instant,
    /// The timer task that runs the slices is alive.
    driver: bool,
    /// The scan just finished for a new query: scroll to the current match.
    scroll_now: bool,
    scratch: RowText,
    /// Scans started from the top (a new query, or a buffer that shrank) and scans that went on
    /// over new output. The smoke reads them.
    pub restarts: u32,
    pub extends: u32,
    /// Hashes of rows of the settled part of the buffer, taken when the scan was made.
    /// When the buffer is full, old rows are dropped and every row index moves up: the rows
    /// then differ, and the scan starts over instead of going on with wrong row numbers.
    fp: Fingerprint,
}

/// A big copy read in slices. `fp` is the fingerprint of the part of the selection that is
/// already in the scrollback: if those rows are not the same rows at the next slice, the
/// buffer dropped old lines in between and the row numbers are wrong.
struct CopyTask {
    job: CopyJob,
    smart: bool,
    fp: Fingerprint,
    /// What was fed to the terminal (`fed_bytes`, `resets`) when `fp` last matched. The rows
    /// can only move when the terminal is fed, so while this is the same nothing is checked.
    checked: (u64, u64),
}

/// Fingerprint of rows `lo..hi` of the buffer (see `fingerprint.rs`).
fn rows_fp(term: &Terminal, scratch: &mut RowText, lo: usize, hi: usize) -> Fingerprint {
    Fingerprint::take(lo, hi, |row| {
        if term.row_text(row, scratch) {
            fingerprint::hash_text(&scratch.text, scratch.wrapped)
        } else {
            0
        }
    })
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
            sb_drag: None,
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
            search: None,
            hover_link: None,
            hover_cell: None,
            hover_gen: 0,
            smoke_mode,
            opened_urls: Vec::new(),
            code_blocks: Vec::new(),
            code_gen: None,
            code_scanned: None,
            hover_block: None,
            copied_until: None,
            bell: BellGate::default(),
            bell_until: None,
            bells: 0,
            layout_changed: false,
            dim: 0.0,
            force_size: false,
            resize_refused: None,
            copy_task: None,
            copy_driver: false,
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
        self.layout_changed = true;
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
            if let Some(st) = self.search.as_mut() {
                st.stale = true;
            }
            self.search_drive(cx);
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
            // Titles and cwd come from the backend (it scans the same bytes). A bell in replayed
            // output rang long ago.
            for ev in self.term.drain_events() {
                if matches!(ev, TerminalEvent::Bell) && !self.replaying {
                    self.ring_bell(cx);
                }
            }
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
        if self.resize_refused == Some((rows, cols)) {
            return;
        }
        if !self.resize_vt(rows, cols) {
            return;
        }
        self.resizes += 1;
        if std::mem::take(&mut self.force_size) {
            self.source.reassert_size(rows as u16, cols as u16);
        } else {
            self.source.resize(rows as u16, cols as u16);
        }
        self.sel = None;
    }

    /// Resize the VT. When libghostty refuses, the view keeps the grid the terminal really has
    /// (and the PTY keeps the size it had: nothing is sent), and the same target is not tried
    /// again until the window size changes.
    fn resize_vt(&mut self, rows: usize, cols: usize) -> bool {
        let ok = self.term.resize(rows, cols);
        let (r, c) = if ok { (rows, cols) } else { self.term.grid_size() };
        self.resize_refused = if ok { None } else { Some((rows, cols)) };
        if (r, c) != (self.rows, self.cols) || self.row_hash.len() != r {
            self.rows = r;
            self.cols = c;
            self.row_hash = vec![0; r];
            self.rows_shaped = vec![None; r];
            self.last_gen = 0;
            self.layout_changed = true;
        }
        ok
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
        if self.resize_refused == Some((rows, cols)) {
            return;
        }
        if self.resize_vt(rows, cols) {
            self.resizes += 1;
            self.sel = None;
        }
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

    // ---- bell -------------------------------------------------------------------------

    fn ring_bell(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if !self.bell.ring(now) {
            return;
        }
        self.bells += 1;
        let mode = self.cfg.bell_mode;
        if mode.visual() || mode.audio() {
            cx.emit(ViewEvent::Bell { pane: self.pane_id });
        }
        if mode.visual() {
            self.bell_until = Some(now + bell::FLASH);
            if self.visible {
                cx.notify();
            }
            // One more frame when the flash is over, so it goes away without other input.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(bell::FLASH + Duration::from_millis(20)).await;
                let _ = this.update(cx, |v, cx| {
                    if v.visible {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        if mode.audio() && !self.smoke_mode {
            crate::winsys::beep();
        }
    }

    /// The white wash of a visual bell is on screen.
    pub fn bell_flash(&self) -> bool {
        self.bell_until.is_some_and(|t| Instant::now() < t)
    }

    // ---- links and code blocks -------------------------------------------------------------

    /// The http(s) link that covers the viewport cell, looking through the whole wrapped line.
    pub(crate) fn link_at(&mut self, vrow: usize, col: usize) -> Option<Link> {
        let screen = self.term.screen();
        let rows = screen.rows();
        if vrow >= rows {
            return None;
        }
        let (mut a, mut b) = (vrow, vrow);
        while a > 0 && vrow - a < 16 && screen.is_row_wrapped(a - 1) {
            a -= 1;
        }
        while b + 1 < rows && b - vrow < 16 && screen.is_row_wrapped(b) {
            b += 1;
        }
        let mut line = LogicalLine::new();
        for r in a..=b {
            for (c, cell) in screen.row(r).iter().enumerate() {
                if !cell.attrs.spacer {
                    line.push(r, c, &cell.grapheme);
                }
            }
        }
        links::link_at(&line, vrow, col)
    }

    /// The pointer moved anywhere in the window: update the link and the code block under it.
    fn update_hover(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let (x, y) = self.local_unclamped(pos);
        let grid_w = f32::from(self.metrics.cell_w) * self.cols as f32;
        let grid_h = f32::from(self.metrics.line_h) * self.rows as f32;
        let inside = x >= 0.0 && y >= 0.0 && x < grid_w && y < grid_h && self.bounds.contains(&pos);
        // Apps that track the mouse own the clicks, and a drag in progress owns the pointer.
        if !inside || self.selecting || self.sb_drag.is_some() || self.term.is_mouse_tracking() {
            self.clear_hover(cx);
            return;
        }
        let (vr, col) = self.cell_at(pos);
        let gen = self.term.screen().generation();
        if self.hover_cell == Some((vr, col)) && self.hover_gen == gen {
            return;
        }
        self.hover_cell = Some((vr, col));
        self.hover_gen = gen;
        let link = self.link_at(vr, col);
        let mut changed = link != self.hover_link;
        self.hover_link = link;

        let block = if self.term.is_alternate_screen() { None } else { self.block_at(vr, col, gen) };
        if block != self.hover_block {
            self.hover_block = block;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        self.hover_cell = None;
        if self.hover_link.take().is_some() | self.hover_block.take().is_some() {
            cx.notify();
        }
    }

    /// The code block under a cell. The blocks are found again when the output changed, at
    /// most every `CODE_SCAN_GAP`.
    pub(crate) fn block_at(&mut self, vr: usize, col: usize, gen: u64) -> Option<CodeBlock> {
        if self.code_gen != Some(gen) && self.code_scanned.is_none_or(|t| t.elapsed() >= CODE_SCAN_GAP) {
            self.code_blocks = codeblock::detect_code_blocks(&ScreenGrid(self.term.screen()));
            self.code_gen = Some(gen);
            self.code_scanned = Some(Instant::now());
        }
        self.code_blocks.iter().find(|b| b.contains(vr, col)).cloned()
    }

    /// The link underline for this frame: `(row, first col, one past the last col)`. Empty when
    /// the output changed since the pointer found the link (the cells moved).
    pub fn link_spans(&mut self) -> Vec<(usize, usize, usize)> {
        if self.hover_link.is_none() {
            return Vec::new();
        }
        if self.term.screen().generation() != self.hover_gen {
            return Vec::new();
        }
        self.hover_link.as_ref().map(|l| l.segments.clone()).unwrap_or_default()
    }

    /// The code block to highlight, and whether its button shows the check mark. Like links,
    /// only while the output is the one the block was found in.
    pub fn block_highlight(&mut self) -> Option<(CodeBlock, bool)> {
        let b = self.hover_block.clone()?;
        if self.term.screen().generation() != self.hover_gen {
            return None;
        }
        let copied = self.copied_until.is_some_and(|t| Instant::now() < t);
        Some((b, copied))
    }

    pub fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        if !links::is_openable(url) {
            return;
        }
        if self.smoke_mode {
            self.opened_urls.push(url.to_string());
        } else {
            cx.open_url(url);
        }
    }

    /// A click on the copy button of the hovered code block.
    fn click_code_button(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(b) = self.hover_block.clone() else { return false };
        let (x, y) = self.local_unclamped(pos);
        let (cw, lh) = (f32::from(self.metrics.cell_w), f32::from(self.metrics.line_h));
        let bx = (b.right_col as f32 + 1.0) * cw - CODE_BTN;
        let by = b.top_row as f32 * lh;
        if x < bx || x > bx + CODE_BTN || y < by || y > by + CODE_BTN {
            return false;
        }
        let text = codeblock::extract_content(&ScreenGrid(self.term.screen()), &b);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.copied_until = Some(Instant::now() + COPIED_FOR);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(COPIED_FOR + Duration::from_millis(20)).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
            cx.notify();
        }
        true
    }

    // ---- scrollbar ------------------------------------------------------------------

    /// The track spans the whole pane height. Window y of its top edge and its height.
    fn sb_track(&self) -> (f32, f32) {
        let pad = self.cfg.pane_padding_y;
        (f32::from(self.bounds.origin.y) - pad, f32::from(self.bounds.size.height) + 2.0 * pad)
    }

    /// The thumb for the current scroll position, as `(top, height)` in the track.
    pub(crate) fn sb_thumb(&self) -> Option<scrollbar::Thumb> {
        let (total, off, len) = self.term.scrollbar_state();
        scrollbar::thumb(total, off, len, self.sb_track().1, SB_MIN_THUMB)
    }

    fn on_scrollbar_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if ev.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus, cx);
        let Some(t) = self.sb_thumb() else { return };
        let y = f32::from(ev.position.y) - self.sb_track().0;
        // On the thumb: hold it where it was grabbed. On the track: put the thumb's middle there.
        let grab = if y >= t.top && y <= t.top + t.height { y - t.top } else { t.height / 2.0 };
        self.sb_drag = Some(grab);
        self.scrollbar_drag_to(f32::from(ev.position.y), grab, cx);
    }

    pub(crate) fn scrollbar_drag_to(&mut self, window_y: f32, grab: f32, cx: &mut Context<Self>) {
        let (total, off, len) = self.term.scrollbar_state();
        let (top, h) = self.sb_track();
        let to = scrollbar::offset_at(total, len, h, SB_MIN_THUMB, window_y - top - grab);
        if to != off {
            self.scroll_rows(to as isize - off as isize);
        }
        cx.notify();
    }

    /// The scrollbar: a thin strip over the right edge, only while there is scrollback. It
    /// floats over the text, so it never changes the number of columns (GTK's sits beside it).
    fn scrollbar_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let t = self.sb_thumb()?;
        let dragging = self.sb_drag.is_some();
        let thumb = |a: f32| Hsla { h: 0.0, s: 0.0, l: 1.0, a };
        Some(
            div()
                .id("scrollbar")
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(SB_W))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_scrollbar_down))
                .child(
                    div()
                        .id("scrollbar-thumb")
                        .absolute()
                        .top(px(t.top))
                        .left(px(2.0))
                        .w(px(SB_W - 4.0))
                        .h(px(t.height))
                        .rounded(px(3.0))
                        .bg(thumb(if dragging { 0.5 } else { 0.22 }))
                        .hover(move |s| s.bg(thumb(0.4))),
                ),
        )
    }

    // ---- search ---------------------------------------------------------------------

    pub fn toggle_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.close_search(cx);
            return;
        }
        self.search = Some(SearchState {
            edit: LineEdit::default(),
            job: None,
            current: 0,
            jump: false,
            keep: None,
            stale: false,
            started: Instant::now(),
            driver: false,
            scroll_now: false,
            scratch: RowText::default(),
            fp: Fingerprint::default(),
            restarts: 0,
            extends: 0,
        });
        cx.notify();
    }

    /// Open the find bar with `query` already typed (the smoke does this; so could a command).
    pub fn search_set(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.toggle_search(cx);
        }
        if let Some(st) = self.search.as_mut() {
            st.edit = LineEdit::new(query);
        }
        self.search_restart(cx);
    }

    /// `(scan finished, matches found, index of the current match, rows scanned)`.
    pub fn search_status(&self) -> Option<(bool, usize, usize, usize)> {
        let st = self.search.as_ref()?;
        let j = st.job.as_ref()?;
        Some((j.done(), j.found(), st.current, j.progress().0))
    }

    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        // Dropping the state ends the timer task at its next tick.
        self.search = None;
        cx.notify();
    }

    pub(crate) fn search_key(&mut self, ks: &gpui::Keystroke, prefer_char: bool, cx: &mut Context<Self>) {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "escape" => return self.close_search(cx),
            "enter" => return self.search_goto(!m.shift, cx),
            "pageup" | "pagedown" => {
                let page = self.rows.saturating_sub(1).max(1) as isize;
                self.scroll_rows(if ks.key == "pageup" { -page } else { page });
                return cx.notify();
            }
            "v" if m.control && !m.alt => return self.search_paste(cx),
            _ => {}
        }
        // AltGr layouts report ctrl+alt with the composed character.
        if prefer_char && m.control && m.alt {
            if let Some(t) = ks.key_char.as_deref().filter(|t| !t.is_empty()) {
                return self.search_insert(t, cx);
            }
        }
        let Some(st) = self.search.as_mut() else { return };
        let before = st.edit.text.clone();
        let r = st.edit.key(ks);
        if st.edit.text != before {
            self.search_restart(cx);
        } else if r != EditResult::Ignored {
            cx.notify();
        }
    }

    fn search_insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        if let Some(st) = self.search.as_mut() {
            st.edit.insert(&clean);
        }
        self.search_restart(cx);
    }

    fn search_paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        let first = text.lines().next().unwrap_or("").to_string();
        self.search_insert(&first, cx);
    }

    /// The query changed: scan again from the top.
    fn search_restart(&mut self, cx: &mut Context<Self>) {
        let total = self.term.scrollbar_state().0 as usize;
        let screen = self.rows;
        let Some(st) = self.search.as_mut() else { return };
        st.job = SearchJob::new(&st.edit.text, total);
        st.fp = rows_fp(&self.term, &mut st.scratch, 0, total.saturating_sub(screen));
        st.restarts += 1;
        st.current = 0;
        st.jump = true;
        st.keep = None;
        st.stale = false;
        st.started = Instant::now();
        self.search_drive(cx);
        cx.notify();
    }

    /// Start the timer task that runs the scan in slices, if it is not running.
    fn search_drive(&mut self, cx: &mut Context<Self>) {
        let Some(st) = self.search.as_mut() else { return };
        if st.driver {
            return;
        }
        st.driver = true;
        cx.spawn(async move |this, cx| loop {
            let wait = this.update(cx, |v, cx| {
                let w = v.search_tick();
                if v.visible {
                    cx.notify();
                }
                if v.search_jump_pending() {
                    v.scroll_to_current_match();
                }
                w
            });
            match wait {
                Ok(Some(d)) => cx.background_executor().timer(d).await,
                _ => break,
            }
        })
        .detach();
    }

    fn search_jump_pending(&mut self) -> bool {
        self.search.as_mut().is_some_and(|s| std::mem::take(&mut s.scroll_now))
    }

    /// One slice of work. `Some(wait)` when the task should run again after `wait`.
    fn search_tick(&mut self) -> Option<Duration> {
        let total = self.term.scrollbar_state().0 as usize;
        let top = self.term.scrollbar_state().1 as usize;
        let st = self.search.as_mut()?;
        if st.stale && st.started.elapsed() >= SEARCH_RESTART_GAP {
            st.stale = false;
            st.started = Instant::now();
            if !st.jump {
                st.keep = st.job.as_ref().and_then(|j| j.matches.get(st.current)).map(|m| (m.row, m.col));
            }
            // New output: the running (or finished) scan goes on over the rows added since it
            // started, plus the old screen, which changes in place. It is never thrown away
            // while output keeps coming, so a long scan always completes. A buffer that
            // shrank (clear, reset) starts a new scan.
            let screen = self.rows;
            // The rows that were settled when the scan was made are still the same rows.
            let unmoved = rows_fp(&self.term, &mut st.scratch, 0, st.fp.hi) == st.fp;
            let extended = unmoved
                && st.job.as_mut().is_some_and(|j| {
                    let old_total = j.progress().1;
                    j.extend(total, old_total.saturating_sub(screen))
                });
            if extended {
                st.extends += 1;
            } else {
                st.job = SearchJob::new(&st.edit.text, total);
                st.restarts += 1;
            }
            st.fp = rows_fp(&self.term, &mut st.scratch, 0, total.saturating_sub(screen));
        }
        let Some(job) = st.job.as_mut() else {
            st.driver = false;
            return None;
        };
        if !job.done() {
            let term = &self.term;
            let scratch = &mut st.scratch;
            let mut reader = |abs: usize, text: &mut String, cols: &mut Vec<u16>| {
                if !term.row_text(abs, scratch) {
                    return false;
                }
                std::mem::swap(text, &mut scratch.text);
                std::mem::swap(cols, &mut scratch.cols);
                true
            };
            if job.step(&mut reader, SEARCH_SLICE) {
                let n = job.matches.len();
                if st.jump {
                    st.jump = false;
                    st.current = search::first_at_or_after(&job.matches, top).unwrap_or(0);
                    st.scroll_now = true;
                } else if let Some((r, c)) = st.keep.take() {
                    st.current = job.matches.partition_point(|m| (m.row, m.col) < (r, c)).min(n.saturating_sub(1));
                } else {
                    st.current = st.current.min(n.saturating_sub(1));
                }
            } else {
                return Some(Duration::from_millis(1));
            }
        }
        if st.stale {
            return Some(SEARCH_RESTART_GAP.saturating_sub(st.started.elapsed()).max(Duration::from_millis(1)));
        }
        st.driver = false;
        None
    }

    /// Enter / Shift+Enter: the next or previous match, wrapping around.
    pub(crate) fn search_goto(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(st) = self.search.as_mut() else { return };
        let Some(job) = st.job.as_ref().filter(|j| j.done()) else { return };
        let n = job.matches.len();
        if n == 0 {
            return;
        }
        st.current = if forward { search::next_index(st.current, n) } else { search::prev_index(st.current, n) };
        self.scroll_to_current_match();
        cx.notify();
    }

    /// Scroll so the current match is in view, a third of the way down when it was not.
    fn scroll_to_current_match(&mut self) {
        let Some(m) = self
            .search
            .as_ref()
            .and_then(|s| s.job.as_ref().and_then(|j| j.matches.get(s.current)))
            .copied()
        else {
            return;
        };
        let (total, off, len) = self.term.scrollbar_state();
        if search::in_view(m.row as usize, off as usize, len as usize) {
            return;
        }
        let target = search::offset_for(m.row as usize, len as usize, total as usize);
        self.scroll_rows(target as isize - off as isize);
    }

    /// Matches in the viewport: (viewport row, col, cells, is the current match).
    pub fn search_spans(&self) -> Vec<(usize, usize, usize, bool)> {
        let Some(st) = &self.search else { return Vec::new() };
        let Some(job) = &st.job else { return Vec::new() };
        let top = self.term.scrollbar_state().1 as usize;
        let cur = job.done().then(|| job.matches.get(st.current)).flatten();
        search::visible(&job.matches, top, self.rows)
            .iter()
            .map(|m| {
                let col = (m.col as usize).min(self.cols.saturating_sub(1));
                let len = (m.len as usize).min(self.cols - col);
                (m.row as usize - top, col, len, cur == Some(m))
            })
            .collect()
    }

    /// Frame hook: row numbers moved (resize, reset), so the matches are old.
    pub fn after_layout_change(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.layout_changed) {
            if let Some(st) = self.search.as_mut() {
                st.stale = true;
                st.jump = false;
            }
            self.search_drive(cx);
        }
    }

    /// The find bar, top right of the pane. It floats over the text, so opening it does not
    /// resize the PTY (GTK puts it above the terminal and the grid shrinks).
    fn search_overlay(&self) -> Option<gpui::Div> {
        let st = self.search.as_ref()?;
        let t = &st.edit.text;
        let split = t.char_indices().nth(st.edit.cursor).map(|x| x.0).unwrap_or(t.len());
        let nb = |s: &str| SharedString::from(s.replace(' ', "\u{00a0}"));
        let label = match &st.job {
            Some(j) => search::label(!j.done(), false, st.current, j.matches.len() + j.overflow, j.overflow),
            None => String::new(),
        };
        let accent = hsla(self.cfg.accent);
        let hit = st.job.as_ref().is_some_and(|j| j.done() && j.found() == 0);
        Some(
            div()
                .absolute()
                .top(px(6.0))
                .right(px(18.0))
                .w(px(340.0))
                .h(px(32.0))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .rounded(px(6.0))
                .bg(hsla(0x252526))
                .border_1()
                .border_color(if hit { hsla(0xe06c75) } else { accent })
                .text_color(hsla(0xffffff))
                .font_family(self.metrics.font.family.clone())
                .text_size(self.metrics.font_size)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .items_center()
                        .child(nb(&t[..split]))
                        .child(div().w(px(1.0)).h(px(16.0)).bg(accent))
                        .child(nb(&t[split..])),
                )
                .child(div().flex_none().text_color(hsla(0x9aa0a6)).child(SharedString::from(label))),
        )
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
        self.layout_changed = true;
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
                    let m = &ks.modifiers;
                    if m.control && !m.shift && !m.alt && ks.key == "c" {
                        // The shell often beeps after an interrupt: that bell is not news.
                        self.bell.quiet_after_ctrl_c(Instant::now());
                    }
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
                    if self.search.is_some() {
                        self.search_paste(cx);
                    } else {
                        self.paste(cx);
                    }
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
        // The find bar takes every other key while it is open.
        if self.search.is_some() {
            self.search_key(ks, ev.prefer_character_input, cx);
            cx.stop_propagation();
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
        // Notices can carry text from the server or the wire: always cleaned here.
        let text = forgetty_shell_core::notify::notice_text(&text);
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
        let Some(((r0, c0), (r1, c1))) = self.sel.as_ref().map(|s| s.range()) else { return };
        let total = self.term.scrollbar_state().0 as usize;
        let r1 = r1.min(total.saturating_sub(1));
        if r0 > r1 {
            return;
        }
        let mut job = CopyJob::new(r0, c0, r1, c1);
        // A screen's worth is read in one go. A selection over the scrollback is read in slices
        // (about 100 ms for 100 000 rows in one go), and goes to the clipboard when it is done.
        if job.total_rows() <= COPY_INLINE_ROWS {
            let mut rt = RowText::default();
            job.step(&mut copy_reader(&self.term, &mut rt), Duration::from_secs(5));
            write_copy(&job, smart, cx);
            return;
        }
        // Rows that are still on the live screen change in place and may keep changing. The
        // rows above them are settled: they must be the same rows until the copy is done.
        let settled = total.saturating_sub(self.rows);
        let mut rt = RowText::default();
        let fp = rows_fp(&self.term, &mut rt, r0, (r1 + 1).min(settled));
        let n = job.total_rows();
        let checked = (self.fed_bytes, self.resets);
        self.copy_task = Some(CopyTask { job, smart, fp, checked });
        self.show_notice(format!("Copying {n} lines..."), cx);
        self.copy_drive(cx);
    }

    /// The timer task that reads a big copy in slices, if it is not running.
    fn copy_drive(&mut self, cx: &mut Context<Self>) {
        if self.copy_driver {
            return;
        }
        self.copy_driver = true;
        cx.spawn(async move |this, cx| loop {
            let again = this.update(cx, |v, cx| v.copy_tick(cx));
            match again {
                Ok(true) => cx.background_executor().timer(Duration::from_millis(1)).await,
                _ => break,
            }
        })
        .detach();
    }

    /// One slice of a big copy. True while more is to come.
    fn copy_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let started = Instant::now();
        let gen = (self.fed_bytes, self.resets);
        let Some(task) = self.copy_task.as_mut() else {
            self.copy_driver = false;
            return false;
        };
        let mut rt = RowText::default();
        // The rows must still be the rows that were selected. They can only have moved if the
        // terminal was fed since the last check. Nothing feeds it inside this call (the UI
        // thread is the only one that does), so the check at the start also covers the rows
        // of the last slice.
        if task.checked != gen {
            if rows_fp(&self.term, &mut rt, task.fp.lo, task.fp.hi) != task.fp {
                return self.copy_refused(cx);
            }
            task.checked = gen;
        }
        // The time the check took comes out of the slice (at least 1 ms is left to read).
        let budget = COPY_SLICE.saturating_sub(started.elapsed()).max(Duration::from_millis(1));
        let done = task.job.step(&mut copy_reader(&self.term, &mut rt), budget);
        if !done {
            return true;
        }
        if let Some(t) = self.copy_task.take() {
            write_copy(&t.job, t.smart, cx);
        }
        // The clipboard now holds the whole copy: end the "copying" notice.
        self.notice = None;
        self.copy_driver = false;
        cx.notify();
        false
    }

    /// The scrollback dropped lines while a copy ran. Nothing is put on the clipboard: a text
    /// made of the wrong rows would be worse than none.
    fn copy_refused(&mut self, cx: &mut Context<Self>) -> bool {
        self.copy_task = None;
        self.copy_driver = false;
        self.show_notice("Copy stopped: output moved the lines while copying. Select again and copy.".into(), cx);
        false
    }

    /// A copy in slices is still running.
    pub fn copy_pending(&self) -> bool {
        self.copy_task.is_some()
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

    pub(crate) fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                // Ctrl+click on a link opens it.
                if ev.modifiers.control && !ev.modifiers.shift && !self.term.is_mouse_tracking() {
                    if let Some(l) = self.link_at(vr, col) {
                        self.open_link(&l.url, cx);
                        return;
                    }
                }
                if ev.click_count <= 1 && self.click_code_button(ev.position, cx) {
                    return;
                }
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
                // A right-click opens the menu (GTK). Shift+right-click always does, also over a
                // program that tracks the mouse. `right_click = "copy_paste"` gives a plain
                // right-click the Windows Terminal behavior instead.
                if ev.modifiers.shift || !self.cfg.right_click_copy_paste {
                    self.request_context_menu(ev.position, cx);
                    return;
                }
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

    /// A selection that has at least one cell.
    pub fn has_selection(&self) -> bool {
        self.sel.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// Tell the shell to open this pane's context menu at a window position. It carries the
    /// link under the position and whether Copy has something to copy.
    pub fn request_context_menu(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (vr, col) = self.cell_at(position);
        let url = self.link_at(vr, col).map(|l| l.url).filter(|u| links::is_openable(u));
        cx.emit(ViewEvent::ContextMenu {
            pane: self.pane_id,
            pos: (f32::from(position.x), f32::from(position.y)),
            url,
            has_selection: self.has_selection(),
        });
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
        self.update_hover(ev.position, cx);
        if let Some(grab) = self.sb_drag {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.scrollbar_drag_to(f32::from(ev.position.y), grab, cx);
            } else {
                self.sb_drag = None;
                cx.notify();
            }
            return;
        }
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
        if ev.button == MouseButton::Left && self.sb_drag.take().is_some() {
            cx.notify();
        }
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

    pub(crate) fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
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
        let mut job = CopyJob::new(r0, c0, r1, c1);
        let mut rt = RowText::default();
        job.step(&mut copy_reader(&self.term, &mut rt), Duration::from_secs(3600));
        Some(job.finish(smart))
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

/// A selection of this many rows or fewer is copied on the spot.
const COPY_INLINE_ROWS: usize = 2_000;
/// Time budget of one slice of a big copy.
const COPY_SLICE: Duration = Duration::from_millis(4);

/// Reads rows of the terminal for a `CopyJob`: the row's text and columns, and whether it wraps.
fn copy_reader<'a>(
    term: &'a Terminal,
    rt: &'a mut RowText,
) -> impl FnMut(usize, &mut String, &mut Vec<u16>) -> Option<bool> + 'a {
    move |ar, text, cols| {
        if !term.row_text(ar, rt) {
            return None; // the row is gone (scrollback was trimmed)
        }
        std::mem::swap(text, &mut rt.text);
        std::mem::swap(cols, &mut rt.cols);
        Some(rt.wrapped)
    }
}

fn write_copy(job: &CopyJob, smart: bool, cx: &mut Context<TerminalView>) {
    let text = job.finish(smart);
    if !text.is_empty() {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

/// The visible screen as the code block detector reads it.
struct ScreenGrid<'a>(&'a Screen);

impl codeblock::Grid for ScreenGrid<'_> {
    fn rows(&self) -> usize {
        self.0.rows()
    }
    fn cols(&self) -> usize {
        self.0.cols()
    }
    fn text(&self, row: usize, col: usize) -> &str {
        &self.0.cell(row, col).grapheme
    }
    fn bg(&self, row: usize, col: usize) -> Option<u32> {
        match self.0.cell(row, col).attrs.bg {
            Color::Default => None,
            Color::Rgb(r, g, b) => Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
        }
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
        if self.search.is_some() {
            self.search_insert(text, cx);
            return;
        }
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
            .children(self.scrollbar_overlay(cx))
            .children(self.search_overlay())
            .children(self.paste_overlay())
            .children(self.owner_label())
            .children(self.drop_overlay())
    }
}
