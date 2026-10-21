//! TerminalView: owns the libghostty-vt `Terminal`, the PTY, per-row render
//! caches and the smoke-test state machine.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use forgetty_core::Rgba as CoreRgba;
use forgetty_vt::screen::{CellAttributes, Color};
use forgetty_vt::{Screen, Terminal};
use gpui::{
    div, font, prelude::*, px, rgb, App, Context, Font, FontStyle, FontWeight, FocusHandle,
    Focusable, Hsla, IntoElement, KeyDownEvent, Pixels, Render, SharedString, StrikethroughStyle,
    TextRun, UnderlineStyle, Window,
};

use crate::element::TerminalElement;
use crate::keys;
use crate::pty::{self, Pty, SharedWriter};

pub const ROWS: u16 = 30;
pub const COLS: u16 = 120;
pub const KEYSTROKES: usize = 100;

/// Smoke phase, readable from the watchdog thread.
pub static PHASE: AtomicU8 = AtomicU8::new(0);
const PHASE_NAMES: [&str; 6] =
    ["window", "first-frame", "wait-prompt", "wait-echo", "keystrokes", "done"];
pub fn phase_name() -> &'static str {
    PHASE_NAMES[(PHASE.load(Ordering::Relaxed) as usize).min(PHASE_NAMES.len() - 1)]
}

// Windows Terminal "Campbell" scheme.
const DEFAULT_FG: u32 = 0xcccccc;
const DEFAULT_BG: u32 = 0x0c0c0c;
const CAMPBELL: [u32; 16] = [
    0x0c0c0c, 0xc50f1f, 0x13a10e, 0xc19c00, 0x0037da, 0x881798, 0x3a96dd, 0xcccccc, 0x767676,
    0xe74856, 0x16c60c, 0xf9f1a5, 0x3b78ff, 0xb4009e, 0x61d6d6, 0xf2f2f2,
];

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
    pub fn measure(cx: &App) -> Self {
        let font = font("Consolas");
        let font_size = px(15.0);
        let ts = cx.text_system();
        let id = ts.resolve_font(&font);
        let cell_w = ts.advance(id, font_size, 'm').map(|s| s.width).unwrap_or(px(8.0));
        let line_h = px((f32::from(font_size) * 1.3).round());
        Metrics { font, font_size, cell_w, line_h }
    }
}

/// Cached, ready-to-paint form of one terminal row.
#[derive(Clone, Default)]
pub struct RowCache {
    pub line: Option<gpui::ShapedLine>,
    /// (first col, one past last col, color)
    pub bgs: Vec<(usize, usize, Hsla)>,
}

struct Probe {
    row: usize,
    col: usize,
    ch: char,
    t0: Instant,
}

#[derive(PartialEq)]
enum Phase {
    WaitPrompt,
    WaitEcho,
    Keys,
}

struct Smoke {
    start: Instant,
    phase: Phase,
    t_first_frame_ms: Option<f64>,
    t_prompt_ms: Option<f64>,
    t_echo_ms: Option<f64>,
    probe: Option<Probe>,
    lat_ms: Vec<f64>,
}

pub struct TerminalView {
    pub term: Terminal,
    pub metrics: Metrics,
    pub rows_cache: Vec<RowCache>,
    pub last_gen: u64,
    writer: SharedWriter,
    #[allow(dead_code)]
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    focus: FocusHandle,
    smoke: Option<Smoke>,
    pub frames: u64,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pty_write(w: &SharedWriter, bytes: &[u8]) {
    if let Ok(mut w) = w.lock() {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

pub fn row_text(screen: &Screen, row: usize) -> String {
    screen.row(row).iter().map(|c| c.grapheme.as_str()).collect()
}

impl TerminalView {
    pub fn new(smoke_start: Option<Instant>, cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let metrics = Metrics::measure(cx);
        let palette: [CoreRgba; 16] = std::array::from_fn(|i| {
            let h = CAMPBELL[i];
            CoreRgba::rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
        });
        let term = Terminal::new(ROWS as usize, COLS as usize, palette);

        let shell = if smoke_start.is_some() {
            "cmd.exe".to_string()
        } else {
            std::env::var("FORGETTY_SHELL")
                .or_else(|_| std::env::var("COMSPEC"))
                .unwrap_or_else(|_| "cmd.exe".into())
        };
        let cwd = if smoke_start.is_some() { Some("C:\\") } else { None };
        let Pty { writer, master, child, rx } = pty::spawn(&shell, cwd, ROWS, COLS)?;

        // Pump PTY output into the terminal on the UI thread. Chunks that
        // arrived while we were busy are coalesced into one feed + one redraw.
        cx.spawn(async move |this, cx| {
            while let Ok(mut batch) = rx.recv().await {
                while let Ok(more) = rx.try_recv() {
                    batch.extend_from_slice(&more);
                }
                if this.update(cx, |v, cx| v.feed(&batch, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        Ok(Self {
            term,
            metrics,
            rows_cache: vec![RowCache::default(); ROWS as usize],
            last_gen: 0,
            writer,
            master,
            child,
            focus: cx.focus_handle(),
            smoke: smoke_start.map(|start| Smoke {
                start,
                phase: Phase::WaitPrompt,
                t_first_frame_ms: None,
                t_prompt_ms: None,
                t_echo_ms: None,
                probe: None,
                lat_ms: Vec::new(),
            }),
            frames: 0,
        })
    }

    fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.term.feed(bytes);
        // Terminal replies (DA, CPR, mode queries). ConPTY waits on these.
        for resp in self.term.drain_write_pty() {
            pty_write(&self.writer, &resp);
        }
        let _ = self.term.drain_events();
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        if let Some(bytes) = keys::encode(&ev.keystroke) {
            pty_write(&self.writer, &bytes);
        }
    }

    /// Re-shape only rows the VT reports dirty since the last frame.
    pub fn prepare_rows(&mut self, window: &mut Window) {
        let screen = self.term.screen();
        let gen_now = screen.generation();
        let m = &self.metrics;
        for r in 0..screen.rows().min(self.rows_cache.len()) {
            let cache = &mut self.rows_cache[r];
            if cache.line.is_some() && !screen.row_dirty_since(r, self.last_gen) {
                continue;
            }
            let (text, runs, bgs) = build_row(screen, r, &m.font);
            cache.line =
                Some(window.text_system().shape_line(text, m.font_size, &runs, Some(m.cell_w)));
            cache.bgs = bgs;
        }
        self.last_gen = gen_now;
    }

    /// Called right after the element painted a frame (scene built, present follows).
    pub fn after_paint(&mut self) {
        self.frames += 1;
        let now = Instant::now();
        let Some(sm) = self.smoke.as_mut() else { return };

        if sm.t_first_frame_ms.is_none() {
            sm.t_first_frame_ms = Some(ms(now - sm.start));
            PHASE.store(2, Ordering::Relaxed);
        }

        let screen = self.term.screen();
        let (cur_row, cur_col) = self.term.cursor();
        let at_prompt = row_text(screen, cur_row).trim_end().ends_with('>');

        match sm.phase {
            Phase::WaitPrompt => {
                if at_prompt {
                    sm.t_prompt_ms = Some(ms(now - sm.start));
                    pty_write(&self.writer, b"echo spike-ok\r");
                    sm.phase = Phase::WaitEcho;
                    PHASE.store(3, Ordering::Relaxed);
                }
            }
            Phase::WaitEcho => {
                // The command line itself reads "C:\>echo spike-ok", so match
                // the output line only (whole row == spike-ok) and wait for the
                // next prompt below it.
                let found_row = (0..screen.rows())
                    .find(|&r| row_text(screen, r).trim() == "spike-ok");
                if let Some(fr) = found_row {
                    if at_prompt && cur_row > fr {
                        sm.t_echo_ms = Some(ms(now - sm.start));
                        sm.phase = Phase::Keys;
                        PHASE.store(4, Ordering::Relaxed);
                        start_probe(sm, &self.writer, cur_row, cur_col);
                    }
                }
            }
            Phase::Keys => {
                let done = match &sm.probe {
                    Some(p) => screen.cell(p.row, p.col).grapheme.starts_with(p.ch),
                    None => false,
                };
                if done {
                    let p = sm.probe.take().unwrap();
                    sm.lat_ms.push(ms(now - p.t0));
                    if sm.lat_ms.len() >= KEYSTROKES {
                        self.finish_ok();
                    } else {
                        start_probe(sm, &self.writer, cur_row, cur_col);
                    }
                }
            }
        }
    }

    fn finish_ok(&mut self) -> ! {
        let sm = self.smoke.as_ref().unwrap();
        let mut sorted = sm.lat_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let json = serde_json::json!({
            "ok": true,
            "found": "spike-ok",
            "time_to_first_frame_ms": round(sm.t_first_frame_ms.unwrap_or(-1.0)),
            "time_to_prompt_ms": round(sm.t_prompt_ms.unwrap_or(-1.0)),
            "time_to_echo_found_ms": round(sm.t_echo_ms.unwrap_or(-1.0)),
            "keystrokes": sorted.len(),
            "key_to_frame_avg_ms": round(avg),
            "key_to_frame_p50_ms": round(sorted[sorted.len() / 2]),
            "key_to_frame_p95_ms": round(sorted[(sorted.len() * 95) / 100]),
            "key_to_frame_max_ms": round(*sorted.last().unwrap()),
            "frames_drawn": self.frames,
            "note": "key_to_frame = PTY write until the frame whose scene contains the echoed glyph finished painting (present follows in the same draw call)",
        });
        println!("{json}");
        let _ = std::io::stdout().flush();
        let _ = self.child.kill();
        std::process::exit(0);
    }
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn start_probe(sm: &mut Smoke, writer: &SharedWriter, row: usize, col: usize) {
    let ch = (b'a' + (sm.lat_ms.len() % 26) as u8) as char;
    sm.probe = Some(Probe { row, col, ch, t0: Instant::now() });
    pty_write(writer, ch.to_string().as_bytes());
}

#[derive(PartialEq, Clone, Copy)]
struct RunKey {
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn resolve(attrs: &CellAttributes) -> (Hsla, Hsla) {
    let fg = match attrs.fg {
        Color::Default => hsla(DEFAULT_FG),
        Color::Rgb(r, g, b) => rgb8(r, g, b),
    };
    let bg = match attrs.bg {
        Color::Default => hsla(DEFAULT_BG),
        Color::Rgb(r, g, b) => rgb8(r, g, b),
    };
    let (mut fg, bg) = if attrs.inverse { (bg, fg) } else { (fg, bg) };
    if attrs.dim {
        fg.a *= 0.6;
    }
    (fg, bg)
}

/// One row -> (text, text runs, background runs). Adjacent cells with the same
/// style merge, so a typical row is a handful of runs.
fn build_row(
    screen: &Screen,
    r: usize,
    base: &Font,
) -> (SharedString, Vec<TextRun>, Vec<(usize, usize, Hsla)>) {
    let default_bg = hsla(DEFAULT_BG);
    let mut text = String::with_capacity(screen.cols());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut bgs: Vec<(usize, usize, Hsla)> = Vec::new();
    let mut last_key: Option<RunKey> = None;

    for (c, cell) in screen.row(r).iter().enumerate() {
        let (fg, bg) = resolve(&cell.attrs);
        let g = if cell.grapheme.is_empty() { " " } else { cell.grapheme.as_str() };
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
                underline: key.underline.then(|| UnderlineStyle {
                    color: Some(fg),
                    thickness: px(1.0),
                    wavy: false,
                }),
                strikethrough: key
                    .strike
                    .then(|| StrikethroughStyle { color: Some(fg), thickness: px(1.0) }),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(hsla(DEFAULT_BG))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(TerminalElement::new(cx.entity()))
    }
}
