//! `--smoke`: the CI gate. Drives a real window and real ConPTY shells through
//! every W1 and W2 feature and prints one JSON object. Exit 0 only if every
//! check passes.
//!
//! The state machine advances from the shell's `after_paint`, i.e. only when a
//! frame was drawn, so it measures the same path a user sees. Keyboard
//! shortcuts go through the same `AppShell::handle_key` the window uses.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use forgetty_shell_core::keys::{Action, Chord, Keymap};
use forgetty_shell_core::layout::{neighbor, Side};
use forgetty_shell_core::{now, LocalBackend, PaneEvent, PaneId, PaneStatus, Uuid};
use forgetty_vt::ffi;
use forgetty_vt::{Screen, Terminal, ANSI_PALETTE_BLACK};
use gpui::{px, size, Context, Keystroke, Modifiers, Window};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::shell::{AppShell, BAR};
use crate::view::TerminalView;

mod real;
mod remote;

pub const KEYSTROKES: usize = 100;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(420);
const PHASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The real-server run (WSL start, a 20 MB burst) gets more time.
const REAL_TOTAL_TIMEOUT: Duration = Duration::from_secs(900);
const REAL_PHASE_TIMEOUT: Duration = Duration::from_secs(240);

pub fn total_timeout() -> Duration {
    if real::distro().is_some() {
        REAL_TOTAL_TIMEOUT
    } else {
        TOTAL_TIMEOUT
    }
}
const CONPTY_LINES: u32 = 25_000;
const CONPTY_BYTES: f64 = CONPTY_LINES as f64 * 90.0;
const SYNTH_LINES: u32 = 250_000;
const SYNTH_BYTES: f64 = SYNTH_LINES as f64 * 90.0;
const BIG_FILE: &str = "C:\\big.txt";
pub const SHOT_FILE: &str = "layout-4pane.png";
const WIN_W: f32 = 1000.0;
const WIN_H: f32 = 680.0;

static RESULTS: Mutex<Option<Map<String, Value>>> = Mutex::new(None);
static PHASE: Mutex<&'static str> = Mutex::new("window");
/// Process id of the W3 test server, so a failure can take it down too.
pub static SERVER_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SCREEN_DUMP: Mutex<String> = Mutex::new(String::new());

pub fn record_pub(key: &str, value: Value) {
    record(key, value)
}

fn record(key: &str, value: Value) {
    let mut g = RESULTS.lock().unwrap();
    g.get_or_insert_with(Map::new).insert(key.to_string(), value);
}

fn set_phase(p: &'static str) {
    *PHASE.lock().unwrap() = p;
}

fn snapshot(ok: bool) -> Value {
    let results = RESULTS.lock().unwrap().clone().unwrap_or_default();
    let mut m = Map::new();
    m.insert("ok".into(), json!(ok));
    for (k, v) in results {
        m.insert(k, v);
    }
    Value::Object(m)
}

/// Print the failure JSON (with the last screen dump) and exit 1.
pub fn fail(msg: &str) -> ! {
    let mut v = snapshot(false);
    if let Value::Object(m) = &mut v {
        m.insert("error".into(), json!(msg));
        m.insert("phase".into(), json!(*PHASE.lock().unwrap()));
        m.insert("screen".into(), json!(SCREEN_DUMP.lock().unwrap().clone()));
    }
    println!("{v}");
    let _ = std::io::stdout().flush();
    eprintln!("forgetty-win smoke FAILED: {msg} (phase {})", *PHASE.lock().unwrap());
    hard_exit(1)
}

/// Leave without running any destructors. `std::process::exit` can hang in
/// `ExitProcess` while the loader lock waits on a thread stuck in a driver or
/// in the network stack.
pub fn hard_exit(code: i32) -> ! {
    // The test server holds a copy of our inherited stdout; until it dies the
    // CI shell keeps waiting for the pipe to close.
    #[cfg(windows)]
    {
        let pid = SERVER_PID.load(std::sync::atomic::Ordering::SeqCst);
        if pid != 0 {
            let _ = std::process::Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
        }
    }
    if let Some(d) = real::distro() {
        real::stop_server(&d);
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), code as u32);
    }
    std::process::exit(code)
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

fn check(name: &str, pass: bool) {
    let mut g = RESULTS.lock().unwrap();
    let m = g.get_or_insert_with(Map::new);
    let checks = m.entry("checks").or_insert_with(|| json!({}));
    checks.as_object_mut().unwrap().insert(name.to_string(), json!(pass));
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Phase {
    // W1: single pane, first tab.
    WaitPrompt,
    WaitEcho,
    Keys,
    ClearLine,
    Wide,
    Gen,
    ResizeWait,
    ResizeAsk,
    Throughput,
    Synth,
    // W2: app shell.
    Restore,
    Tabs4,
    TabNav,
    SplitSteps,
    SplitSettle,
    Echo4,
    Keys4,
    DividerWait,
    Status,
    Shot,
    ClosePane,
    CloseTab,
    PaletteOpen,
    PaletteRun,
    Rename,
    // W3: remote machines.
    RSetup,
    RPair,
    REcho,
    RKeys,
    RKill,
    RRestart,
    RToast,
    RServer,
    // W4: the real Linux server in WSL.
    RealSetup,
    RealPair,
    RealEcho,
    RealKeys,
    RealKill,
    RealRestart,
    RealBurst,
    RealFinal,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::WaitPrompt => "wait-prompt",
            Phase::WaitEcho => "wait-echo",
            Phase::Keys => "keystrokes",
            Phase::ClearLine => "clear-line",
            Phase::Wide => "wide-chars",
            Phase::Gen => "generate-file",
            Phase::ResizeWait => "resize-wait",
            Phase::ResizeAsk => "resize-ask-shell",
            Phase::Throughput => "throughput-conpty",
            Phase::Synth => "throughput-pump",
            Phase::Restore => "w2-restore-window",
            Phase::Tabs4 => "w2-open-4-tabs",
            Phase::TabNav => "w2-tab-navigation",
            Phase::SplitSteps => "w2-split-steps",
            Phase::SplitSettle => "w2-split-settle",
            Phase::Echo4 => "w2-echo-per-pane",
            Phase::Keys4 => "w2-keystrokes-4-panes",
            Phase::DividerWait => "w2-divider-drag",
            Phase::Status => "w2-status",
            Phase::Shot => "w2-screenshot",
            Phase::ClosePane => "w2-close-pane",
            Phase::CloseTab => "w2-close-tab",
            Phase::PaletteOpen => "w2-palette-open",
            Phase::PaletteRun => "w2-palette-run",
            Phase::Rename => "w2-rename",
            Phase::RSetup => "w3-start-server-and-pair",
            Phase::RPair => "w3-wait-for-panes",
            Phase::REcho => "w3-remote-echo",
            Phase::RKeys => "w3-remote-keystrokes",
            Phase::RKill => "w3-kill-server",
            Phase::RRestart => "w3-restart-server",
            Phase::RToast => "w3-toasts",
            Phase::RServer => "w4-server-ops",
            Phase::RealSetup => "w4-real-setup",
            Phase::RealPair => "w4-real-pair",
            Phase::RealEcho => "w4-real-echo",
            Phase::RealKeys => "w4-real-keys",
            Phase::RealKill => "w4-real-kill",
            Phase::RealRestart => "w4-real-restart",
            Phase::RealBurst => "w4-real-burst",
            Phase::RealFinal => "w4-real-final",
        }
    }
}

struct Probe {
    row: usize,
    col: usize,
    ch: char,
    t0: Instant,
}

pub struct ShellSmoke {
    start: Instant,
    local: LocalBackend,
    phase: Phase,
    phase_t0: Instant,
    first_frame_seen: bool,
    probe: Option<Probe>,
    lat_ms: Vec<f64>,
    base_col: usize,
    max_reshaped_per_key: usize,
    others_reshaped: usize,
    fed4: u64,
    old_dims: (usize, usize),
    resize_t0: Option<Instant>,
    thr_t0: Instant,
    thr_last: Option<Instant>,
    thr_max_gap: f64,
    thr_frames: u32,
    // W2
    p1: Option<PaneId>,
    sub: usize,
    sub_t0: Instant,
    tab2: Option<Uuid>,
    panes4: Vec<PaneId>,
    tokens: Vec<String>,
    before_sizes: Vec<(PaneId, u16, u16)>,
    want_ratio: f32,
    ratio_before: f32,
    pane_to_close: Option<PaneId>,
    tab_to_close: Option<(Uuid, PaneId)>,
    count_before: usize,
    nav_ok: bool,
    shot_frames: u32,
    frames_seen: u64,
    w3: remote::W3,
    real: real::Real,
}

/// Snapshot of one pane's screen.
struct Snap {
    rows: Vec<String>,
    cr: usize,
    cc: usize,
}

impl Snap {
    fn at_prompt(&self) -> bool {
        self.rows.get(self.cr).is_some_and(|r| r.trim_end().ends_with('>'))
    }
    fn find_exact(&self, s: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.trim() == s)
    }
}

fn snap(shell: &AppShell, cx: &gpui::App, p: PaneId) -> Option<Snap> {
    let v = shell.views.get(&p)?.read(cx);
    let screen = v.term.screen();
    let rows = (0..screen.rows()).map(|r| row_text(screen, r)).collect();
    let (cr, cc) = v.term.cursor();
    Some(Snap { rows, cr, cc })
}

fn key(chord: &str) -> Keystroke {
    let c = Chord::parse(chord).expect("chord");
    Keystroke {
        modifiers: Modifiers { control: c.ctrl, alt: c.alt, shift: c.shift, platform: c.win, function: false },
        key: c.key,
        key_char: None,
    }
}

fn typed(ch: char) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::default(),
        key: if ch == ' ' { "space".into() } else { ch.to_string() },
        key_char: Some(ch.to_string()),
    }
}

impl ShellSmoke {
    pub fn new(start: Instant, local: LocalBackend) -> Self {
        run_direct_checks();
        let t = Instant::now();
        let mut real_distro = String::new();
        ShellSmoke {
            start,
            local,
            phase: if let Some(d) = real::distro() {
                real_distro = d;
                Phase::RealSetup
            } else {
                Phase::WaitPrompt
            },
            phase_t0: t,
            first_frame_seen: false,
            probe: None,
            lat_ms: Vec::new(),
            base_col: 0,
            max_reshaped_per_key: 0,
            others_reshaped: 0,
            fed4: 0,
            old_dims: (0, 0),
            resize_t0: None,
            thr_t0: t,
            thr_last: None,
            thr_max_gap: 0.0,
            thr_frames: 0,
            p1: None,
            sub: 0,
            sub_t0: t,
            tab2: None,
            panes4: Vec::new(),
            tokens: Vec::new(),
            before_sizes: Vec::new(),
            want_ratio: 0.4,
            ratio_before: 0.5,
            pane_to_close: None,
            tab_to_close: None,
            count_before: 0,
            nav_ok: true,
            shot_frames: 0,
            frames_seen: 0,
            w3: remote::W3::default(),
            real: real::Real::new(real_distro),
        }
    }

    /// Phases that measure latency or throughput must not get extra frames.
    pub fn wants_ticks(&self) -> bool {
        !matches!(self.phase, Phase::Keys | Phase::Throughput | Phase::Synth | Phase::Keys4 | Phase::RKeys)
    }

    fn goto(&mut self, p: Phase) {
        self.phase = p;
        self.phase_t0 = Instant::now();
        self.sub = 0;
        self.sub_t0 = Instant::now();
        set_phase(p.name());
    }
}

fn row_text(screen: &Screen, r: usize) -> String {
    screen.row(r).iter().filter(|c| !c.attrs.spacer).map(|c| c.grapheme.as_str()).collect()
}

fn leading_number(s: &str) -> Option<u64> {
    let d: String = s.chars().take(7).collect();
    (d.len() == 7 && d.chars().all(|c| c.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

fn is_w1(p: Phase) -> bool {
    matches!(
        p,
        Phase::WaitPrompt
            | Phase::WaitEcho
            | Phase::Keys
            | Phase::ClearLine
            | Phase::Wide
            | Phase::Gen
            | Phase::ResizeWait
            | Phase::ResizeAsk
            | Phase::Throughput
            | Phase::Synth
    )
}

impl ShellSmoke {
    /// What the shell looks like when a phase sticks: the state a person would
    /// check first.
    fn diag(&self, shell: &AppShell, cx: &gpui::App) -> String {
        let mut o = format!(
            "sub={} active_tab={:?} active_pane={:?} machines={} conn={:?} tabs={} views={}\n",
            self.sub,
            shell.active_tab().map(|t| t.id),
            shell.active_pane(),
            shell.machines.len(),
            shell.backend.connection(),
            shell.layout.tab_count(),
            shell.views.len()
        );
        for p in shell.views.keys() {
            if let (Some(s), Some(v)) = (snap(shell, cx, *p), shell.views.get(p)) {
                let v = v.read(cx);
                o.push_str(&format!("pane {:?} resets={} fed={} cursor=({},{})\n", p, v.resets, v.fed_bytes, s.cr, s.cc));
                for r in s.rows.iter().filter(|r| !r.trim().is_empty()).take(6) {
                    o.push_str(&format!("  | {}\n", r.trim_end()));
                }
            }
        }
        o.push_str(&self.real_diag());
        o
    }

    pub fn step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        let now_t = Instant::now();
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            record("time_to_first_frame_ms", json!(ms(now_t - self.start)));
            set_phase(Phase::WaitPrompt.name());
        }
        if self.frames_seen % 200 == 0 {
            record(&format!("window_active_at_frame_{}", self.frames_seen), json!(window.is_window_active()));
        }
        self.frames_seen += 1;
        let phase_limit = if real::distro().is_some() { REAL_PHASE_TIMEOUT } else { PHASE_TIMEOUT };
        if self.phase_t0.elapsed() > phase_limit {
            if !is_w1(self.phase) {
                *SCREEN_DUMP.lock().unwrap() = self.diag(shell, cx);
            }
            fail(&format!("phase {} stuck for {:?}", self.phase.name(), phase_limit));
        }

        if is_w1(self.phase) {
            // W1 runs against the first tab's only pane.
            let Some(p1) = shell.active_pane() else { return };
            let Some(view) = shell.views.get(&p1).cloned() else { return };
            self.p1 = Some(p1);
            let frames = shell.frames;
            let mut finished = false;
            view.update(cx, |v, _| {
                finished = self.w1_step(v, window, now_t, frames);
            });
            if finished {
                self.goto(Phase::Restore);
            }
            return;
        }
        self.w2_step(shell, window, cx, now_t);
    }

    // ---- W1: unchanged checks on the first pane --------------------------------------

    fn w1_step(&mut self, v: &mut TerminalView, window: &mut Window, now: Instant, frames: u64) -> bool {
        let (rows, cr, cc) = {
            let screen = v.term.screen();
            let rows: Vec<String> = (0..screen.rows()).map(|r| row_text(screen, r)).collect();
            let (cr, cc) = v.term.cursor();
            (rows, cr, cc)
        };
        *SCREEN_DUMP.lock().unwrap() = rows.join("\n");
        let at_prompt = rows.get(cr).is_some_and(|r| r.trim_end().ends_with('>'));
        let find_exact = |s: &str| rows.iter().position(|r| r.trim() == s);

        match self.phase {
            Phase::WaitPrompt => {
                if at_prompt {
                    self.base_col = cc;
                    record("time_to_prompt_ms", json!(ms(now - self.start)));
                    v.send_bytes(b"echo spike-ok\r");
                    self.goto(Phase::WaitEcho);
                }
            }
            Phase::WaitEcho => {
                if let Some(fr) = find_exact("spike-ok") {
                    if at_prompt && cr > fr {
                        record("found", json!("spike-ok"));
                        record("time_to_echo_found_ms", json!(ms(now - self.start)));
                        check("echo_found", true);
                        self.start_probe(v, cr, cc);
                        self.goto(Phase::Keys);
                    }
                }
            }
            Phase::Keys => {
                let done = match &self.probe {
                    Some(p) => v.term.screen().cell(p.row, p.col).grapheme.starts_with(p.ch),
                    None => false,
                };
                if done {
                    let p = self.probe.take().unwrap();
                    self.lat_ms.push(ms(now - p.t0));
                    self.max_reshaped_per_key = self.max_reshaped_per_key.max(v.rows_reshaped_last);
                    if self.lat_ms.len() >= KEYSTROKES {
                        let (p50, reshaped) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "");
                        check("key_p50_le_18ms", p50 <= 18.0);
                        check("damage_only_redraw", reshaped <= 3);
                        self.lat_ms.clear();
                        self.max_reshaped_per_key = 0;
                        // cmd clears the input line on Escape.
                        v.send_bytes(&[0x1b]);
                        self.goto(Phase::ClearLine);
                    } else {
                        self.start_probe(v, cr, cc);
                    }
                }
            }
            Phase::ClearLine => {
                if at_prompt && cc == self.base_col {
                    encoder_checks(v);
                    v.send_bytes("echo 日本語 😀\r".as_bytes());
                    self.goto(Phase::Wide);
                }
            }
            Phase::Wide => {
                if let Some(fr) = find_exact("日本語 😀") {
                    if at_prompt && cr > fr {
                        let screen = v.term.screen();
                        let c = screen.row(fr);
                        let ok = c[0].attrs.wide
                            && c[1].attrs.spacer
                            && c[2].attrs.wide
                            && c[4].attrs.wide
                            && c[6].grapheme == " "
                            && c[7].attrs.wide
                            && c[8].attrs.spacer;
                        check("wide_shell", ok);
                        v.send_bytes(
                            format!(
                                "powershell -NoProfile -Command \"$l='0123456789abcdef'*5; $o=1..{CONPTY_LINES} | ForEach-Object {{ '{{0:D7}} {{1}}' -f $_,$l }}; [IO.File]::WriteAllLines('{BIG_FILE}',$o)\" && echo GEN-OK\r"
                            )
                            .as_bytes(),
                        );
                        self.goto(Phase::Gen);
                    }
                }
            }
            Phase::Gen => {
                if let Some(fr) = find_exact("GEN-OK") {
                    if at_prompt && cr > fr {
                        self.old_dims = (v.rows, v.cols);
                        let target = size(v.metrics.cell_w * 100.0, v.metrics.line_h * 25.0 + px(crate::shell::TITLEBAR_H));
                        window.resize(target);
                        self.resize_t0 = Some(Instant::now());
                        self.goto(Phase::ResizeWait);
                    }
                }
            }
            Phase::ResizeWait => {
                if (v.rows, v.cols) != self.old_dims && at_prompt {
                    record("resize_grid", json!([v.rows, v.cols]));
                    record("resize_applied_ms", json!(ms(self.resize_t0.map(|t| now - t).unwrap_or_default())));
                    v.send_bytes(
                        b"powershell -NoProfile -Command \"$s=$Host.UI.RawUI.WindowSize; 'WS:{0}x{1}' -f $s.Width,$s.Height\"\r",
                    );
                    self.goto(Phase::ResizeAsk);
                }
            }
            Phase::ResizeAsk => {
                if let Some(fr) = rows.iter().position(|r| r.trim().starts_with("WS:")) {
                    if at_prompt && cr > fr {
                        let t = rows[fr].trim().trim_start_matches("WS:").to_string();
                        let want = format!("{}x{}", v.cols, v.rows);
                        record("shell_reported_size", json!(t));
                        record("resize_events", json!(v.resizes));
                        check("resize", t == want && v.resizes >= 1);
                        v.send_bytes(format!("type {BIG_FILE} & echo THRU-DONE\r").as_bytes());
                        self.thr_t0 = Instant::now();
                        self.thr_last = None;
                        self.goto(Phase::Throughput);
                    }
                }
            }
            Phase::Throughput => {
                if let Some(last) = self.thr_last {
                    self.thr_max_gap = self.thr_max_gap.max(ms(now - last));
                }
                self.thr_last = Some(now);
                self.thr_frames += 1;
                if let Some(fr) = find_exact("THRU-DONE") {
                    if at_prompt && cr > fr {
                        let secs = (now - self.thr_t0).as_secs_f64();
                        record("conpty_drain_s", json!((secs * 1000.0).round() / 1000.0));
                        record("conpty_mb_per_s", json!(((CONPTY_BYTES / 1e6 / secs) * 100.0).round() / 100.0));
                        record("conpty_max_frame_gap_ms", json!(self.thr_max_gap));
                        record("conpty_frames", json!(self.thr_frames));
                        check("throughput_conpty", self.thr_max_gap <= 100.0);
                        selection_check(v, &rows);
                        mouse_and_paste_checks(v);
                        scrollback_shell_check(v, &rows);
                        // Now push 22.5 MB through the real pump at full producer speed.
                        if let Some(tx) = self.p1.and_then(|p| self.local.pty(p)).map(|p| p.injector()) {
                            std::thread::spawn(move || synth_producer(tx));
                        } else {
                            fail("no pty for the first pane");
                        }
                        self.thr_t0 = Instant::now();
                        self.thr_last = None;
                        self.thr_max_gap = 0.0;
                        self.thr_frames = 0;
                        self.goto(Phase::Synth);
                    }
                }
            }
            Phase::Synth => {
                if let Some(last) = self.thr_last {
                    self.thr_max_gap = self.thr_max_gap.max(ms(now - last));
                }
                self.thr_last = Some(now);
                self.thr_frames += 1;
                if find_exact("SYN-DONE").is_some() {
                    let secs = (now - self.thr_t0).as_secs_f64();
                    record("pump_drain_s", json!((secs * 1000.0).round() / 1000.0));
                    record("pump_mb_per_s", json!(((SYNTH_BYTES / 1e6 / secs) * 10.0).round() / 10.0));
                    record("pump_max_frame_gap_ms", json!(self.thr_max_gap));
                    record("pump_frames", json!(self.thr_frames));
                    check("throughput_pump", self.thr_max_gap <= 100.0);
                    record("frames_drawn_w1", json!(frames));
                    record("shape_cache_misses", json!(v.shape_misses));
                    record("vt_fed_mb", json!((v.fed_bytes as f64 / 1e6 * 10.0).round() / 10.0));
                    record("vt_feed_total_ms", json!(ms(v.feed_time)));
                    record("vt_feed_max_ms", json!(ms(v.max_feed)));
                    record("font", json!(format!("{} {:.1}pt", v.metrics.font.family, v.cfg.font_size_pt)));
                    record("grid_w1", json!([v.rows, v.cols]));
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    fn start_probe(&mut self, v: &mut TerminalView, row: usize, col: usize) {
        let ch = (b'a' + (self.lat_ms.len() % 26) as u8) as char;
        self.probe = Some(Probe { row, col, ch, t0: Instant::now() });
        v.send_bytes(ch.to_string().as_bytes());
    }
}

/// Record latency stats under `suffix` and return (p50, max reshaped rows).
fn summarize_keys(lat: &[f64], max_reshaped: usize, suffix: &str) -> (f64, usize) {
    let mut s = lat.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = s.iter().sum::<f64>() / s.len() as f64;
    let p50 = s[s.len() / 2];
    record(&format!("keystrokes{suffix}"), json!(s.len()));
    record(&format!("key_to_frame_avg_ms{suffix}"), json!((avg * 1000.0).round() / 1000.0));
    record(&format!("key_to_frame_p50_ms{suffix}"), json!(p50));
    record(&format!("key_to_frame_p95_ms{suffix}"), json!(s[(s.len() * 95) / 100]));
    record(&format!("key_to_frame_max_ms{suffix}"), json!(s.last().unwrap()));
    record(&format!("rows_reshaped_per_key_max{suffix}"), json!(max_reshaped));
    (p50, max_reshaped)
}

// ---- W2: app shell ------------------------------------------------------------------

impl ShellSmoke {
    fn press(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, chord: &str) {
        let handled = shell.handle_key(&key(chord), window, cx);
        if !handled {
            fail(&format!("shortcut {chord} was not handled by the shell"));
        }
        self.sub_t0 = Instant::now();
    }

    fn next_key_char(&self) -> char {
        (b'a' + (self.lat_ms.len() % 26) as u8) as char
    }

    fn tab_pane_count(shell: &AppShell) -> usize {
        shell.active_tab().map(|t| t.pane_tree.pane_count()).unwrap_or(0)
    }

    fn all_at_prompt(shell: &AppShell, cx: &gpui::App, panes: &[PaneId]) -> bool {
        panes.iter().all(|p| snap(shell, cx, *p).is_some_and(|s| s.at_prompt()))
    }

    fn pty_exited(&self, pane: PaneId) -> bool {
        self.local.closed_ptys().iter().any(|(p, pty)| *p == pane && pty.try_exited())
    }

    fn w2_step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, now_t: Instant) {
        match self.phase {
            Phase::Restore => {
                if self.sub == 0 {
                    window.resize(size(px(WIN_W), px(WIN_H)));
                    self.sub = 1;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_millis(700) {
                    record("viewport_w2", json!([shell.viewport.0, shell.viewport.1]));
                    // Three more tabs through the real shortcut path.
                    for _ in 0..3 {
                        self.press(shell, window, cx, "ctrl+shift+t");
                    }
                    self.goto(Phase::Tabs4);
                }
            }
            Phase::Tabs4 => {
                let panes = shell.layout.all_panes();
                let fresh: Vec<PaneId> = panes.iter().copied().filter(|p| Some(*p) != self.p1).collect();
                if shell.layout.tab_count() == 4 && shell.views.len() == 4 && Self::all_at_prompt(shell, cx, &fresh) {
                    let ids: std::collections::HashSet<Uuid> = shell
                        .layout
                        .workspaces
                        .iter()
                        .flat_map(|w| w.tabs.iter().map(|t| t.id))
                        .collect();
                    check("tabs_4", ids.len() == 4 && panes.len() == 4);
                    self.goto(Phase::TabNav);
                }
            }
            Phase::TabNav => {
                // (shortcut, expected active tab index)
                const STEPS: [(&str, usize); 6] = [
                    ("ctrl+1", 0),
                    ("ctrl+tab", 1),
                    ("ctrl+shift+tab", 0),
                    ("ctrl+9", 3),
                    ("ctrl+3", 2),
                    ("ctrl+2", 1),
                ];
                let i = self.sub / 2;
                if i >= STEPS.len() {
                    check("tab_nav", self.nav_ok);
                    self.tab2 = shell.layout.active_workspace().and_then(|w| w.tabs.get(1)).map(|t| t.id);
                    self.goto(Phase::SplitSteps);
                    return;
                }
                if self.sub % 2 == 0 {
                    self.press(shell, window, cx, STEPS[i].0);
                    self.sub += 1;
                } else {
                    let active = shell.layout.active_workspace().map(|w| w.active_tab).unwrap_or(99);
                    if active == STEPS[i].1 {
                        self.sub += 1;
                    } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                        self.nav_ok = false;
                        record("tab_nav_failed_at", json!(STEPS[i].0));
                        self.sub += 1;
                    }
                }
            }
            Phase::SplitSteps => {
                // Build a 2x2 grid in tab 2 with the real shortcuts and focus keys.
                // (shortcut, Some(side) = focus move, None = split), panes wanted after
                const OPS: [(&str, Option<Side>, usize); 5] = [
                    ("alt+shift+=", None, 2),
                    ("alt+left", Some(Side::Left), 2),
                    ("alt+shift+-", None, 3),
                    ("alt+right", Some(Side::Right), 3),
                    ("alt+shift+-", None, 4),
                ];
                let i = self.sub / 2;
                if i >= OPS.len() {
                    self.goto(Phase::SplitSettle);
                    return;
                }
                let (chord, side, want) = OPS[i];
                if self.sub % 2 == 0 {
                    let prev = shell.active_pane();
                    self.panes4 = prev.into_iter().collect(); // remember the pane we pressed in
                    if let Some(side) = side {
                        let (Some((_, geo)), Some(from)) = (shell.active_geometry(), prev) else { return };
                        match neighbor(&geo.panes, from, side) {
                            Some(to) => self.pane_to_close = Some(to),
                            None => fail(&format!("no neighbor for {chord}")),
                        }
                    }
                    self.press(shell, window, cx, chord);
                    self.sub += 1;
                } else {
                    let ok = match side {
                        Some(_) => shell.active_pane() == self.pane_to_close,
                        None => {
                            Self::tab_pane_count(shell) == want
                                && shell.active_pane().is_some()
                                && shell.active_pane() != self.panes4.first().copied()
                        }
                    };
                    if ok {
                        self.sub += 1;
                    } else if self.sub_t0.elapsed() > Duration::from_secs(8) {
                        fail(&format!("split step {chord} did not complete"));
                    }
                }
            }
            Phase::SplitSettle => {
                let Some(tab) = shell.active_tab().cloned() else { return };
                let panes = tab.pane_tree.leaves();
                match self.sub {
                    0 => {
                        let ready = panes.len() == 4
                            && panes.iter().all(|p| shell.views.contains_key(p))
                            && Self::all_at_prompt(shell, cx, &panes);
                        if ready {
                            self.panes4 = panes.clone();
                            let geo = shell.active_geometry().map(|x| x.1).unwrap_or_default();
                            let c = shell.content_rect();
                            let inside = geo.panes.iter().all(|(_, r)| r.x >= 0.0 && r.y >= 0.0 && r.right() <= c.w + 0.5 && r.bottom() <= c.h + 0.5);
                            let disjoint = geo.panes.iter().enumerate().all(|(i, (_, a))| {
                                geo.panes.iter().skip(i + 1).all(|(_, b)| {
                                    a.right() <= b.x + 0.5 || b.right() <= a.x + 0.5 || a.bottom() <= b.y + 0.5 || b.bottom() <= a.y + 0.5
                                })
                            });
                            check("split_4_panes", geo.panes.len() == 4 && geo.dividers.len() == 3 && inside && disjoint);
                            self.press(shell, window, cx, "ctrl+shift+z");
                            self.sub = 1;
                        }
                    }
                    1 => {
                        if shell.zoomed.contains_key(&tab.id) && shell.active_geometry().is_some_and(|g| g.1.panes.len() == 1) {
                            self.nav_ok = true;
                            self.press(shell, window, cx, "ctrl+shift+z");
                            self.sub = 2;
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("zoom did not engage");
                        }
                    }
                    _ => {
                        if !shell.zoomed.contains_key(&tab.id) && shell.active_geometry().is_some_and(|g| g.1.panes.len() == 4) {
                            check("zoom_toggle", self.nav_ok);
                            self.tokens = (0..4).map(|i| format!("PANE{i}-{:x}", 0x51a0 + i * 7)).collect();
                            self.goto(Phase::Echo4);
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("zoom did not release");
                        }
                    }
                }
            }
            Phase::Echo4 => {
                let i = self.sub / 2;
                if i >= 4 {
                    // Each pane shows its own token and none of the others.
                    let mut isolated = true;
                    for (n, p) in self.panes4.clone().iter().enumerate() {
                        let Some(s) = snap(shell, cx, *p) else { fail("pane view vanished") };
                        for (m, t) in self.tokens.iter().enumerate() {
                            let has = s.find_exact(t).is_some();
                            if has != (m == n) {
                                isolated = false;
                            }
                        }
                    }
                    check("echo_per_pane", true);
                    check("echo_isolated", isolated);
                    self.goto(Phase::Keys4);
                    return;
                }
                let p = self.panes4[i];
                if self.sub % 2 == 0 {
                    shell.focus_pane(p, window, cx);
                    let Some(view) = shell.views.get(&p).cloned() else { return };
                    let cmd = format!("echo {}\r", self.tokens[i]);
                    view.update(cx, |v, _| v.send_bytes(cmd.as_bytes()));
                    self.sub += 1;
                } else if let Some(s) = snap(shell, cx, p) {
                    if let Some(fr) = s.find_exact(&self.tokens[i]) {
                        if s.at_prompt() && s.cr > fr {
                            self.sub += 1;
                        }
                    }
                }
            }
            Phase::Keys4 => {
                let p0 = self.panes4[0];
                let Some(view) = shell.views.get(&p0).cloned() else { return };
                if self.sub == 2 {
                    // Waiting for the Escape to clear the input line.
                    let Some(s) = snap(shell, cx, p0) else { return };
                    if s.at_prompt() && s.cc == self.base_col {
                        self.probe = Some(Probe { row: s.cr, col: s.cc, ch: self.next_key_char(), t0: Instant::now() });
                        let ch = self.probe.as_ref().unwrap().ch;
                        view.update(cx, |v, _| v.send_bytes(ch.to_string().as_bytes()));
                        self.sub = 1;
                    }
                    return;
                }
                if self.sub == 0 {
                    shell.focus_pane(p0, window, cx);
                    let Some(s) = snap(shell, cx, p0) else { return };
                    // Late shell repaints after a resize are not what this check measures:
                    // wait until no pane has received output for the settle time.
                    let fed: u64 = self.panes4.iter().filter_map(|p| shell.views.get(p)).map(|v| v.read(cx).fed_bytes).sum();
                    if fed != self.fed4 {
                        self.fed4 = fed;
                        self.sub_t0 = Instant::now();
                    }
                    // Let the focus change settle: dimming recolors (re-shapes) the other panes once.
                    if s.at_prompt() && shell.active_pane() == Some(p0) && self.sub_t0.elapsed() > Duration::from_millis(400) {
                        let (r, c) = (s.cr, s.cc);
                        self.base_col = c;
                        self.probe = Some(Probe { row: r, col: c, ch: 'a', t0: Instant::now() });
                        view.update(cx, |v, _| v.send_bytes(b"a"));
                        self.lat_ms.clear();
                        self.max_reshaped_per_key = 0;
                        self.others_reshaped = 0;
                        self.sub = 1;
                    }
                    return;
                }
                // A probe that never lands (lost key, cursor race) must not hang the run.
                if let Some(p) = &self.probe {
                    if p.t0.elapsed() > Duration::from_millis(2500) {
                        if let Some(s) = snap(shell, cx, p0) {
                            record("keys4_stall_row", json!(s.rows.get(p.row).cloned().unwrap_or_default()));
                            record("keys4_stall_cursor", json!([s.cr, s.cc, p.row, p.col]));
                        }
                        self.probe = None;
                        view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                        self.sub = 2;
                        return;
                    }
                }
                // Every frame: rows re-shaped in panes that did not get the key.
                let others: usize = self.panes4[1..]
                    .iter()
                    .filter_map(|p| shell.views.get(p))
                    .map(|v| v.read(cx).rows_reshaped_last)
                    .sum();
                if others > 0 && self.others_reshaped == 0 {
                    let per: Vec<(usize, u64)> = self.panes4[1..]
                        .iter()
                        .filter_map(|p| shell.views.get(p))
                        .map(|v| (v.read(cx).rows_reshaped_last, v.read(cx).fed_bytes))
                        .collect();
                    record("keys4_unfocused_reshape", json!({"per_pane_rows_fed": per, "keys_done": self.lat_ms.len()}));
                }
                self.others_reshaped += others;
                let (done, focused_reshaped, row, col) = {
                    let v = view.read(cx);
                    match &self.probe {
                        Some(p) => (v.term.screen().cell(p.row, p.col).grapheme.starts_with(p.ch), v.rows_reshaped_last, p.row, p.col),
                        None => (false, 0, 0, 0),
                    }
                };
                if done {
                    let p = self.probe.take().unwrap();
                    self.lat_ms.push(ms(now_t - p.t0));
                    self.max_reshaped_per_key = self.max_reshaped_per_key.max(focused_reshaped);
                    if self.lat_ms.len() >= KEYSTROKES {
                        let (p50, reshaped) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "_4panes");
                        record("rows_reshaped_unfocused_total_4panes", json!(self.others_reshaped));
                        record("visible_panes", json!(4));
                        check("key_p50_4panes_le_18ms", p50 <= 18.0);
                        check("damage_only_4panes", reshaped <= 3 && self.others_reshaped == 0);
                        view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                        self.drag_divider(shell, cx);
                        self.goto(Phase::DividerWait);
                    } else if self.lat_ms.len() % 20 == 0 {
                        view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                        self.sub = 2;
                    } else {
                        // The cursor now sits on the next glyph position.
                        let (r, c) = {
                            let v = view.read(cx);
                            v.term.cursor()
                        };
                        let _ = (row, col);
                        let ch = self.next_key_char();
                        self.probe = Some(Probe { row: r, col: c, ch, t0: Instant::now() });
                        view.update(cx, |v, _| v.send_bytes(ch.to_string().as_bytes()));
                    }
                }
            }
            Phase::DividerWait => {
                let Some(tab) = shell.active_tab().cloned() else { return };
                let backend_ratio = now(shell.backend.get_layout())
                    .ok()
                    .and_then(|l| l.find_tab(tab.id).and_then(|(w, t)| l.workspaces[w].tabs[t].pane_tree.ratio_at(&[])));
                let ui_ratio = tab.pane_tree.ratio_at(&[]);
                let near = |r: Option<f32>| r.is_some_and(|r| (r - self.want_ratio).abs() < 0.012);
                if !(near(backend_ratio) && near(ui_ratio)) {
                    return;
                }
                // PTY sizes follow the views, and moved the right way.
                let geo = shell.active_geometry().map(|x| x.1).unwrap_or_default();
                let divider_x = geo.dividers.iter().find(|d| d.path.is_empty()).map(|d| d.bar.x).unwrap_or(0.0);
                let mut follow = true;
                let mut moved = true;
                for (p, r) in &geo.panes {
                    let Some(v) = shell.views.get(p) else { return };
                    let (vr, vc) = {
                        let v = v.read(cx);
                        (v.rows as u16, v.cols as u16)
                    };
                    let Ok(info) = now(shell.backend.get_pane_info(*p)) else { fail("pane info missing") };
                    if (info.rows, info.cols) != (vr, vc) {
                        follow = false;
                    }
                    let before = self.before_sizes.iter().find(|b| b.0 == *p).copied();
                    if let Some((_, _, bc)) = before {
                        let left = r.x < divider_x;
                        let ok = if left { info.cols < bc } else { info.cols > bc };
                        if !ok {
                            moved = false;
                        }
                    }
                }
                if !follow || !moved {
                    if self.phase_t0.elapsed() > Duration::from_secs(10) {
                        record("divider_follow_debug", json!({"follow": follow, "moved": moved}));
                        check("divider_drag_ratios", true);
                        check("divider_pty_follow", false);
                        self.start_status(shell);
                    }
                    return;
                }
                record("divider_ratio", json!({"before": self.ratio_before, "after": backend_ratio}));
                check("divider_drag_ratios", true);
                check("divider_pty_follow", true);
                self.start_status(shell);
            }
            Phase::Status => {
                let tabs: Vec<_> = shell.layout.active_workspace().map(|w| w.tabs.clone()).unwrap_or_default();
                if tabs.len() < 4 {
                    return;
                }
                let st: Vec<PaneStatus> = tabs.iter().map(|t| shell.tab_status(t)).collect();
                let ok = st[0] == PaneStatus::Working
                    && st[1] == PaneStatus::Done
                    && st[2] == PaneStatus::Failed
                    && st[3] == PaneStatus::Idle
                    && shell.tab_title(&tabs[0]) == "make all"
                    && shell.tab_cwd(&tabs[0]) == "forgetty";
                if ok {
                    check("status_events", true);
                    self.goto(Phase::Shot);
                } else if self.phase_t0.elapsed() > Duration::from_secs(8) {
                    record("status_debug", json!(st.iter().map(|s| s.as_str()).collect::<Vec<_>>()));
                    check("status_events", false);
                    self.goto(Phase::Shot);
                }
            }
            Phase::Shot => {
                self.shot_frames += 1;
                if self.sub == 0 {
                    if !shell.sidebar_open {
                        self.press(shell, window, cx, "ctrl+b");
                    }
                    self.sub = 1;
                    self.shot_frames = 0;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_millis(600) && self.shot_frames > 6 {
                    let r = crate::screenshot::capture(window, SHOT_FILE);
                    match &r {
                        Ok(s) => record("screenshot", json!({"file": SHOT_FILE, "w": s.w, "h": s.h, "method": s.method, "distinct_colors": s.distinct, "non_background_pct": s.busy_pct})),
                        Err(e) => record("screenshot", json!({"error": e})),
                    }
                    check("screenshot", r.as_ref().is_ok_and(|s| s.distinct >= 8 && s.busy_pct > 2.0) && shell.sidebar_open);
                    self.goto(Phase::ClosePane);
                }
            }
            Phase::ClosePane => {
                let before = Self::tab_pane_count(shell);
                if self.sub == 0 {
                    // Tab 2 (the 2x2 tab) is still the active one.
                    if before != 4 {
                        if self.phase_t0.elapsed() > Duration::from_secs(8) {
                            record("close_pane_debug", json!({"panes_in_active_tab": before}));
                            check("close_pane_pty_exit", false);
                            self.goto(Phase::CloseTab);
                        }
                        return;
                    }
                    self.pane_to_close = shell.active_pane();
                    self.count_before = before;
                    self.press(shell, window, cx, "ctrl+shift+w");
                    self.sub = 1;
                } else {
                    let p = self.pane_to_close.unwrap();
                    if before == self.count_before - 1 && self.pty_exited(p) {
                        check("close_pane_pty_exit", true);
                        self.goto(Phase::CloseTab);
                    } else if self.sub_t0.elapsed() > Duration::from_secs(10) {
                        record("close_pane_debug", json!({"panes": before, "exited": self.pty_exited(p)}));
                        check("close_pane_pty_exit", false);
                        self.goto(Phase::CloseTab);
                    }
                }
            }
            Phase::CloseTab => {
                match self.sub {
                    0 => {
                        self.press(shell, window, cx, "ctrl+9");
                        self.sub = 1;
                    }
                    1 => {
                        let n = shell.layout.active_workspace().map(|w| w.tabs.len()).unwrap_or(0);
                        let active = shell.layout.active_workspace().map(|w| w.active_tab).unwrap_or(0);
                        if n == 4 && active == 3 {
                            let t = shell.active_tab().cloned().unwrap();
                            self.tab_to_close = Some((t.id, t.pane_tree.first_leaf()));
                            self.press(shell, window, cx, "ctrl+shift+q");
                            self.sub = 2;
                        }
                    }
                    _ => {
                        let (_, pane) = self.tab_to_close.unwrap();
                        if shell.layout.tab_count() == 3 && self.pty_exited(pane) {
                            check("close_tab_pty_exit", true);
                            self.goto(Phase::PaletteOpen);
                        } else if self.sub_t0.elapsed() > Duration::from_secs(10) {
                            check("close_tab_pty_exit", false);
                            self.goto(Phase::PaletteOpen);
                        }
                    }
                }
            }
            Phase::PaletteOpen => {
                if self.sub == 0 {
                    self.press(shell, window, cx, "ctrl+shift+p");
                    self.sub = 1;
                } else if shell.palette.is_some() {
                    check("palette_opens", true);
                    for ch in "split right".chars() {
                        shell.handle_key(&typed(ch), window, cx);
                    }
                    let (top, count, shortcut) = shell
                        .palette
                        .as_ref()
                        .map(|p| {
                            (
                                p.items.first().map(|i| i.action),
                                p.items.len(),
                                p.items.first().map(|i| i.shortcut.clone()).unwrap_or_default(),
                            )
                        })
                        .unwrap_or((None, 0, String::new()));
                    record("palette_query", json!({"query": "split right", "top": format!("{top:?}"), "results": count, "shortcut": shortcut}));
                    check("palette_fuzzy_split_right", top == Some(Action::SplitRight) && shortcut == "Alt+Shift+Plus");
                    self.count_before = Self::tab_pane_count(shell);
                    self.press(shell, window, cx, "enter");
                    self.goto(Phase::PaletteRun);
                } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                    check("palette_opens", false);
                    check("palette_fuzzy_split_right", false);
                    self.goto(Phase::Rename);
                }
            }
            Phase::PaletteRun => {
                let n = Self::tab_pane_count(shell);
                if shell.palette.is_none() && n == self.count_before + 1 {
                    check("palette_runs_action", true);
                    self.goto(Phase::Rename);
                } else if self.phase_t0.elapsed() > Duration::from_secs(8) {
                    record("palette_run_debug", json!({"before": self.count_before, "now": n}));
                    check("palette_runs_action", false);
                    self.goto(Phase::Rename);
                }
            }
            Phase::Rename => {
                match self.sub {
                    0 => {
                        self.press(shell, window, cx, "ctrl+shift+r");
                        self.sub = 1;
                    }
                    1 => {
                        if shell.rename.is_some() {
                            for _ in 0..64 {
                                shell.handle_key(&key("backspace"), window, cx);
                            }
                            for ch in "abc".chars() {
                                shell.handle_key(&typed(ch), window, cx);
                            }
                            self.press(shell, window, cx, "enter");
                            self.sub = 2;
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            check("tab_rename", false);
                            self.goto(Phase::RSetup);
                        }
                    }
                    _ => {
                        let title = shell.active_tab().map(|t| shell.tab_title(t));
                        let labeled = shell.active_tab().is_some_and(|t| t.label == "abc");
                        if shell.rename.is_none() && title.as_deref() == Some("abc") && labeled {
                            check("tab_rename", true);
                            self.goto(Phase::RSetup);
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            record("rename_debug", json!(title));
                            check("tab_rename", false);
                            self.goto(Phase::RSetup);
                        }
                    }
                }
            }
            Phase::RSetup | Phase::RPair | Phase::REcho | Phase::RKeys | Phase::RKill | Phase::RRestart | Phase::RToast | Phase::RServer => {
                self.w3_step(shell, window, cx, now_t)
            }
            Phase::RealSetup
            | Phase::RealPair
            | Phase::RealEcho
            | Phase::RealKeys
            | Phase::RealKill
            | Phase::RealRestart
            | Phase::RealBurst
            | Phase::RealFinal => self.real_step(shell, window, cx, now_t),
            _ => {}
        }
    }

    /// Drag the root divider of the 2x2 tab from its current ratio to `want_ratio`
    /// with the same calls the mouse handlers make.
    fn drag_divider(&mut self, shell: &mut AppShell, cx: &mut Context<AppShell>) {
        let Some((tab_id, geo)) = shell.active_geometry() else { fail("no geometry for divider drag") };
        let Some(dv) = geo.dividers.iter().find(|d| d.path.is_empty()).cloned() else { fail("no root divider") };
        self.before_sizes = self
            .panes4
            .iter()
            .filter_map(|p| now(shell.backend.get_pane_info(*p)).ok().map(|i| (*p, i.rows, i.cols)))
            .collect();
        self.ratio_before = dv.ratio;
        self.want_ratio = 0.4;
        shell.begin_divider_drag(tab_id, dv.clone(), cx);
        let (ox, oy) = shell.content_origin();
        let avail = dv.area.w - BAR;
        let x = ox + dv.area.x + BAR / 2.0 + self.want_ratio * avail;
        let y = oy + dv.area.y + dv.area.h / 2.0;
        shell.drag_to(x, y, cx);
        shell.end_drag(cx);
    }

    /// Put distinct statuses, titles and a cwd on the four tabs, as a shell
    /// with integration (or a build tool) would.
    fn start_status(&mut self, shell: &mut AppShell) {
        let tabs: Vec<_> = shell.layout.active_workspace().map(|w| w.tabs.clone()).unwrap_or_default();
        if tabs.len() == 4 {
            let pane = |i: usize| shell.active_pane_of(&tabs[i]);
            self.local.inject_osc(pane(0), b"\x1b]0;make all\x07\x1b]7;file:///C:/work/forgetty\x1b\\\x1b]133;C\x07");
            self.local.inject_osc(pane(1), b"\x1b]0;cargo test\x07\x1b]133;C\x07\x1b]133;D;0\x07");
            self.local.inject_osc(pane(2), b"\x1b]0;deploy\x07\x1b]133;C\x07\x1b]133;D;1\x07");
            self.local.inject_osc(pane(3), b"\x1b]0;ssh prod\x07\x1b]133;A\x07");
        }
        self.goto(Phase::Status);
    }

    fn finish(&mut self, shell: &mut AppShell) -> ! {
        record("total_s", json!(ms(self.start.elapsed()) / 1000.0));
        record("frames_drawn", json!(shell.frames));
        for (name, v) in [("frame_cpu", &mut shell.cpu_ms), ("frame_idle", &mut shell.idle_ms)] {
            if !v.is_empty() {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let q = |f: f32| v[((v.len() - 1) as f32 * f) as usize];
                record(&format!("{name}_p50_ms"), json!(q(0.5)));
                record(&format!("{name}_p95_ms"), json!(q(0.95)));
                record(&format!("{name}_max_ms"), json!(q(1.0)));
            }
        }
        let (all, n_checks) = {
            let g = RESULTS.lock().unwrap();
            let checks = g.as_ref().and_then(|m| m.get("checks")).and_then(|c| c.as_object());
            (
                checks.map(|c| c.values().all(|x| x == &json!(true))).unwrap_or(false),
                checks.map(|c| c.len()).unwrap_or(0),
            )
        };
        record("checks_run", json!(n_checks));
        let all = all && n_checks >= if real::distro().is_some() { 10 } else { 30 };
        println!("{}", snapshot(all));
        let _ = std::io::stdout().flush();
        self.local.shutdown_all();
        self.w3_kill_server();
        self.real_kill();
        hard_exit(if all { 0 } else { 1 });
    }
}

// ---- checks that need no shell ---------------------------------------------------

fn scratch(rows: usize, cols: usize) -> Terminal {
    Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], 8 * 1024 * 1024)
}

fn run_direct_checks() {
    // Wide chars and grapheme clusters straight through the VT.
    let mut t = scratch(5, 40);
    t.feed("A日本😀B e\u{301} 👨\u{200d}👩\u{200d}👧\r\n".as_bytes());
    let s = t.screen();
    let c = s.row(0);
    let wide_ok = c[0].grapheme == "A"
        && !c[0].attrs.wide
        && c[1].grapheme == "日"
        && c[1].attrs.wide
        && c[2].attrs.spacer
        && c[3].grapheme == "本"
        && c[3].attrs.wide
        && c[5].grapheme == "😀"
        && c[5].attrs.wide
        && c[6].attrs.spacer
        && c[7].grapheme == "B"
        && !c[7].attrs.wide;
    check("wide_direct", wide_ok);
    // Info only: depends on the VT's grapheme-cluster mode.
    record("grapheme_combining_one_cell", json!(c.iter().any(|x| x.grapheme == "e\u{301}")));
    record("grapheme_zwj_one_cell", json!(c.iter().any(|x| x.grapheme.chars().count() >= 5)));

    // Scrollback: 5000 lines, scroll up 1000, read the exact line.
    let mut t = scratch(24, 80);
    let mut text = String::new();
    for i in 0..5000 {
        text.push_str(&format!("{i:07} line\r\n"));
    }
    t.feed(text.as_bytes());
    let before = t.scrollbar_state();
    let r = {
        let s = t.screen();
        (0..s.rows()).rev().find(|&r| leading_number(&row_text(s, r)) == Some(4999))
    };
    let mut ok = r.is_some();
    if let Some(r) = r {
        t.scroll_viewport_delta(-1000);
        let up = row_text(t.screen(), r);
        let after = t.scrollbar_state();
        ok &= leading_number(&up) == Some(3999) && before.1.saturating_sub(after.1) == 1000;
        t.scroll_viewport_bottom();
        ok &= leading_number(&row_text(t.screen(), r)) == Some(4999);
    }
    check("scrollback_direct", ok);

    // Config parsing.
    let c = Config::from_toml(
        "shell='pwsh.exe'\nscrollback=5000\n[font]\nfamily='Fira Code'\nsize=13.5\n[theme]\nforeground='#112233'\ncursor='abcdef'\nansi=['#000001','#000002']\n",
    );
    let d = Config::default();
    let ok = c.as_ref().is_ok_and(|c| {
        c.shell.as_deref() == Some("pwsh.exe")
            && c.scrollback_bytes == 5000 * 2048
            && c.font_family == "Fira Code"
            && c.font_size_pt == 13.5
            && c.foreground == 0x112233
            && c.cursor == 0xabcdef
            && c.ansi[0] == 1
            && c.ansi[1] == 2
            && c.ansi[2] == d.ansi[2]
            && c.background == d.background
    }) && d.font_family == "Cascadia Mono"
        && d.font_size_pt == 11.0
        && Config::from_toml("font = 3").is_err();
    check("config", ok);

    // W2 config: scrollback_bytes default and override, backdrop, keybindings.
    let c2 = Config::from_toml(
        "scrollback_bytes = 2097152\nbackdrop='none'\nsidebar=true\n[theme]\naccent='#ff8800'\n[keybindings]\nnew_tab='ctrl+alt+t'\nsplit_right=['ctrl+shift+d','alt+shift+plus']\nclose_pane='none'\n",
    );
    let km = c2.as_ref().map(|c| Keymap::new(&c.keybindings)).unwrap_or_default();
    let chord = |s: &str| Chord::parse(s).unwrap();
    let ok2 = c2.as_ref().is_ok_and(|c| {
        c.scrollback_bytes == 2 * 1024 * 1024
            && c.backdrop == crate::config::Backdrop::None
            && c.sidebar_open
            && c.accent == 0xff8800
    }) && Config::default().scrollback_bytes == 10 * 1024 * 1024
        && km.lookup(&chord("ctrl+alt+t")) == Some(Action::NewTab)
        && km.lookup(&chord("ctrl+shift+t")).is_none()
        && km.lookup(&chord("ctrl+shift+d")) == Some(Action::SplitRight)
        && km.chords(Action::ClosePane).is_empty()
        && km.lookup(&chord("ctrl+shift+p")) == Some(Action::CommandPalette)
        && Config::from_toml("backdrop='glass'").is_err();
    check("config_w2", ok2);
    // New terminals get the 10 MiB default from forgetty-vt.
    check("vt_default_scrollback", forgetty_vt::DEFAULT_MAX_SCROLLBACK_BYTES == 10 * 1024 * 1024);
}

fn encoder_checks(v: &mut TerminalView) {
    let ks = |s: &str| Keystroke::parse(s).expect("keystroke");
    let press = ffi::GHOSTTY_KEY_ACTION_PRESS;
    let handle = v.term.raw_handle();
    let decckm = crate::encoder::private_mode(handle, 1);

    // Same path as a real key press, written to a capture buffer.
    let sent = |v: &mut TerminalView, k: &str| -> Vec<u8> {
        v.capture_input = Some(Vec::new());
        v.send_keystroke(&ks(k), press);
        v.capture_input.take().unwrap_or_default()
    };
    let up = sent(v, "up");
    let ctrl_left = sent(v, "ctrl-left");
    let ctrl_c = sent(v, "ctrl-c");
    let enter = sent(v, "enter");
    let bs = sent(v, "backspace");
    let want_up: &[u8] = if decckm { b"\x1bOA" } else { b"\x1b[A" };
    let ok = up == want_up
        && ctrl_left == b"\x1b[1;5D"
        && ctrl_c == [0x03]
        && enter == b"\r"
        && bs == [0x7f];
    record(
        "key_encoder_bytes",
        json!({"up": hexs(&up), "ctrl_left": hexs(&ctrl_left), "decckm": decckm}),
    );
    check("key_encoder", ok);

    // Kitty keyboard protocol: push "disambiguate" and Escape becomes CSI 27 u.
    let mut k = scratch(5, 20);
    k.feed(b"\x1b[>1u");
    let kh = k.raw_handle();
    let esc = v.input.encode_key(&ks("escape"), press, kh).unwrap_or_default();
    let cl = v.input.encode_key(&ks("ctrl-left"), press, kh).unwrap_or_default();
    record("kitty_escape", json!(hexs(&esc)));
    check("kitty_encoder", esc == b"\x1b[27u" && cl == b"\x1b[1;5D");
}

fn mouse_and_paste_checks(v: &mut TerminalView) {
    // SGR mouse (1000 + 1006): left press/release at cell (col 6, row 4), 1-based.
    let mut m = scratch(25, 100);
    m.feed(b"\x1b[?1000h\x1b[?1006h");
    let h = m.raw_handle();
    let mods = gpui::Modifiers::default();
    let (grid, cell) = ((800, 400), (8, 16));
    let pos = (5.0 * 8.0 + 1.0, 3.0 * 16.0 + 1.0);
    let b = ffi::GHOSTTY_MOUSE_BUTTON_LEFT;
    let press = v.input.encode_mouse_button(b, true, pos, &mods, h, grid, cell).unwrap_or_default();
    let release = v.input.encode_mouse_button(b, false, pos, &mods, h, grid, cell).unwrap_or_default();
    // 1002 button-motion: drag to the next cell reports motion with the button held.
    m.feed(b"\x1b[?1002h");
    let _ = v.input.encode_mouse_button(b, true, pos, &mods, h, grid, cell);
    let drag = v
        .input
        .encode_mouse_motion((6.0 * 8.0 + 1.0, pos.1), &mods, h, grid, cell)
        .unwrap_or_default();
    let _ = v.input.encode_mouse_button(b, false, pos, &mods, h, grid, cell);
    record("mouse_bytes", json!({"press": hexs(&press), "release": hexs(&release), "drag": hexs(&drag)}));
    let ok = press == b"\x1b[<0;6;4M" && release == b"\x1b[<0;6;4m" && drag == b"\x1b[<32;7;4M";
    check("mouse_encoder", ok);

    // Plain line pastes at once; multi-line unbracketed parks behind a confirm prompt.
    v.capture_input = Some(Vec::new());
    let prompt_plain = v.send_paste("hello");
    let sent_plain = v.capture_input.take().unwrap_or_default();
    v.capture_input = Some(Vec::new());
    let prompt_multi = v.send_paste("a\nb");
    let held = v.capture_input.take().unwrap_or_default();
    v.resolve_paste(false);
    let cancelled = v.pending_paste.is_none();
    v.send_paste("a\nb");
    v.capture_input = Some(Vec::new());
    v.resolve_paste(true);
    let confirmed = v.capture_input.take().unwrap_or_default();
    let ok = !prompt_plain
        && sent_plain == b"hello"
        && prompt_multi
        && held.is_empty()
        && cancelled
        && confirmed == b"a\rb";
    check("paste", ok);
}

fn synth_producer(tx: async_channel::Sender<PaneEvent>) {
    let tail = "0123456789abcdef".repeat(5);
    let mut chunk = Vec::with_capacity(17_000);
    for i in 0..SYNTH_LINES {
        chunk.extend_from_slice(format!("{i:07} {tail}\r\n").as_bytes());
        if chunk.len() >= 16_000 {
            if tx.send_blocking(PaneEvent::Output(std::mem::take(&mut chunk))).is_err() {
                return;
            }
        }
    }
    chunk.extend_from_slice(b"SYN-DONE\r\n");
    let _ = tx.send_blocking(PaneEvent::Output(chunk));
}

fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn selection_check(v: &mut TerminalView, rows: &[String]) {
    use crate::view::{Sel, SelMode};
    let Some(fr) = rows.iter().position(|r| r.trim() == "THRU-DONE") else {
        check("selection", false);
        return;
    };
    let abs = v.term.scrollbar_state().1 as usize + fr;
    let word = v.word_bounds(fr, 3);
    let cur_row = v.term.cursor().0.max(fr);
    let last = v.cols - 1;
    v.sel = Some(Sel { mode: SelMode::Line, a0: (abs, 0), a1: (abs, last), h0: (abs, 0), h1: (abs, last) });
    let line = v.selection_text();
    // The DONE line through the prompt row, joined by newlines.
    v.sel = Some(Sel { mode: SelMode::Line, a0: (abs, 0), a1: (abs + (cur_row - fr), last), h0: (abs, 0), h1: (abs + (cur_row - fr), last) });
    let two = v.selection_text().unwrap_or_default();
    v.sel = None;
    let wrapped = v.term.screen().is_row_wrapped(fr);
    record(
        "selection_debug",
        json!({"fr": fr, "word": [word.0, word.1], "line": line, "two": two, "wrapped": wrapped, "offset": v.term.scrollbar_state().1}),
    );
    check(
        "selection",
        word == (0, 8) && line.as_deref() == Some("THRU-DONE") && two.starts_with("THRU-DONE\n") && two.ends_with('>'),
    );
}

fn scrollback_shell_check(v: &mut TerminalView, rows: &[String]) {
    // Last numbered line in the viewport after `type big.txt`.
    let Some((r, n_end)) =
        rows.iter().enumerate().rev().find_map(|(i, t)| leading_number(t).map(|n| (i, n)))
    else {
        record("scrollback_shell_note", json!("no numbered line in viewport"));
        check("scrollback_shell", false);
        return;
    };
    let before = v.term.scrollbar_state();
    v.scroll_rows(-1000);
    let after = v.term.scrollbar_state();
    let n_after = {
        let s = v.term.screen();
        leading_number(&row_text(s, r))
    };
    // ConPTY may drop lines that scroll off between its render ticks, so the
    // number moves by >= 1000. The row offset moves by exactly 1000.
    let ok = before.1.saturating_sub(after.1) == 1000 && n_after.is_some_and(|n| n <= n_end - 1000 && n > 0);
    record("scrollback_shell_last_line", json!(n_end));
    record("scrollback_shell_line_after_up_1000", json!(n_after));
    if let Some(n) = n_after {
        record("conpty_dropped_lines_est", json!((n_end as i64 - n as i64) - 1000));
    }
    check("scrollback_shell", ok);
    v.term.scroll_viewport_bottom();
}
