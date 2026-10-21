//! `--smoke`: the CI gate. Drives a real window + ConPTY shell through every
//! W1 feature and prints one JSON object. Exit 0 only if every check passes.
//!
//! The state machine advances from `after_paint`, i.e. only when a frame was
//! drawn, so it measures the same path a user sees.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use forgetty_vt::ffi;
use forgetty_vt::{Screen, Terminal, ANSI_PALETTE_BLACK};
use gpui::{size, Keystroke, Window};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::source::PaneEvent;
use crate::view::TerminalView;

pub const KEYSTROKES: usize = 100;
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const PHASE_TIMEOUT: Duration = Duration::from_secs(45);
const CONPTY_LINES: u32 = 25_000;
const CONPTY_BYTES: f64 = CONPTY_LINES as f64 * 90.0;
const SYNTH_LINES: u32 = 250_000;
const SYNTH_BYTES: f64 = SYNTH_LINES as f64 * 90.0;
const BIG_FILE: &str = "C:\\big.txt";

static RESULTS: Mutex<Option<Map<String, Value>>> = Mutex::new(None);
static PHASE: Mutex<&'static str> = Mutex::new("window");
static SCREEN_DUMP: Mutex<String> = Mutex::new(String::new());

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
    std::process::exit(1);
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
        }
    }
}

struct Probe {
    row: usize,
    col: usize,
    ch: char,
    t0: Instant,
}

pub struct Smoke {
    start: Instant,
    phase: Phase,
    phase_t0: Instant,
    first_frame_seen: bool,
    probe: Option<Probe>,
    lat_ms: Vec<f64>,
    base_col: usize,
    max_reshaped_per_key: usize,
    old_dims: (usize, usize),
    resize_t0: Option<Instant>,
    thr_t0: Instant,
    thr_last: Option<Instant>,
    thr_max_gap: f64,
    thr_frames: u32,
    injector: async_channel::Sender<PaneEvent>,
}

impl Smoke {
    pub fn new(start: Instant, injector: async_channel::Sender<PaneEvent>) -> Self {
        run_direct_checks();
        Smoke {
            start,
            phase: Phase::WaitPrompt,
            phase_t0: Instant::now(),
            first_frame_seen: false,
            probe: None,
            lat_ms: Vec::new(),
            base_col: 0,
            max_reshaped_per_key: 0,
            old_dims: (0, 0),
            resize_t0: None,
            thr_t0: Instant::now(),
            thr_last: None,
            thr_max_gap: 0.0,
            thr_frames: 0,
            injector,
        }
    }

    fn goto(&mut self, p: Phase) {
        self.phase = p;
        self.phase_t0 = Instant::now();
        set_phase(p.name());
    }
}

impl TerminalView {
    pub fn smoke_step(&mut self, window: &mut Window, now: Instant) {
        let Some(mut sm) = self.smoke.take() else { return };
        sm.step(self, window, now);
        self.smoke = Some(sm);
    }
}

fn row_text(screen: &Screen, r: usize) -> String {
    screen.row(r).iter().filter(|c| !c.attrs.spacer).map(|c| c.grapheme.as_str()).collect()
}

fn leading_number(s: &str) -> Option<u64> {
    let d: String = s.chars().take(7).collect();
    (d.len() == 7 && d.chars().all(|c| c.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

impl Smoke {
    fn step(&mut self, v: &mut TerminalView, window: &mut Window, now: Instant) {
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            record("time_to_first_frame_ms", json!(ms(now - self.start)));
            set_phase(Phase::WaitPrompt.name());
        }

        let (rows, cr, cc) = {
            let screen = v.term.screen();
            let rows: Vec<String> = (0..screen.rows()).map(|r| row_text(screen, r)).collect();
            let (cr, cc) = v.term.cursor();
            (rows, cr, cc)
        };
        *SCREEN_DUMP.lock().unwrap() = rows.join("\n");
        let at_prompt = rows.get(cr).is_some_and(|r| r.trim_end().ends_with('>'));
        let find_exact = |s: &str| rows.iter().position(|r| r.trim() == s);

        if self.phase_t0.elapsed() > PHASE_TIMEOUT {
            fail(&format!("phase {} stuck for {:?}", self.phase.name(), PHASE_TIMEOUT));
        }

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
                // The command line reads "C:\>echo spike-ok": match the output
                // line only (whole row) and wait for the next prompt below it.
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
                    Some(p) => {
                        let screen = v.term.screen();
                        screen.cell(p.row, p.col).grapheme.starts_with(p.ch)
                    }
                    None => false,
                };
                if done {
                    let p = self.probe.take().unwrap();
                    self.lat_ms.push(ms(now - p.t0));
                    self.max_reshaped_per_key = self.max_reshaped_per_key.max(v.rows_reshaped_last);
                    if self.lat_ms.len() >= KEYSTROKES {
                        self.finish_keys(v);
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
                        let target = size(v.metrics.cell_w * 100.0, v.metrics.line_h * 25.0);
                        window.resize(target);
                        self.resize_t0 = Some(Instant::now());
                        self.goto(Phase::ResizeWait);
                    }
                }
            }
            Phase::ResizeWait => {
                if (v.rows, v.cols) != self.old_dims && at_prompt {
                    record("resize_grid", json!([v.rows, v.cols]));
                    record(
                        "resize_applied_ms",
                        json!(ms(self.resize_t0.map(|t| now - t).unwrap_or_default())),
                    );
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
                        let tx = self.injector.clone();
                        std::thread::spawn(move || synth_producer(tx));
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
                    self.finish(v);
                }
            }
        }
    }

    fn start_probe(&mut self, v: &mut TerminalView, row: usize, col: usize) {
        let ch = (b'a' + (self.lat_ms.len() % 26) as u8) as char;
        self.probe = Some(Probe { row, col, ch, t0: Instant::now() });
        v.send_bytes(ch.to_string().as_bytes());
    }

    fn finish_keys(&mut self, v: &mut TerminalView) {
        let mut s = self.lat_ms.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = s.iter().sum::<f64>() / s.len() as f64;
        let p50 = s[s.len() / 2];
        record("keystrokes", json!(s.len()));
        record("key_to_frame_avg_ms", json!((avg * 1000.0).round() / 1000.0));
        record("key_to_frame_p50_ms", json!(p50));
        record("key_to_frame_p95_ms", json!(s[(s.len() * 95) / 100]));
        record("key_to_frame_max_ms", json!(s.last().unwrap()));
        record("rows_reshaped_per_key_max", json!(self.max_reshaped_per_key));
        check("key_p50_le_18ms", p50 <= 18.0);
        check("damage_only_redraw", self.max_reshaped_per_key <= 3);
        // cmd clears the input line on Escape.
        v.send_bytes(&[0x1b]);
        self.goto(Phase::ClearLine);
    }

    fn finish(&mut self, v: &mut TerminalView) -> ! {
        record("total_s", json!(ms(self.start.elapsed()) / 1000.0));
        record("frames_drawn", json!(v.frames));
        record("shape_cache_misses", json!(v.shape_misses));
        record("vt_fed_mb", json!((v.fed_bytes as f64 / 1e6 * 10.0).round() / 10.0));
        record("vt_feed_total_ms", json!(ms(v.feed_time)));
        record("vt_feed_max_ms", json!(ms(v.max_feed)));
        record("font", json!(format!("{} {:.1}pt", v.metrics.font.family, v.cfg.font_size_pt)));
        record("grid", json!([v.rows, v.cols]));
        let all = {
            let g = RESULTS.lock().unwrap();
            g.as_ref()
                .and_then(|m| m.get("checks"))
                .and_then(|c| c.as_object())
                .map(|c| c.values().all(|x| x == &json!(true)))
                .unwrap_or(false)
        };
        let n_checks = RESULTS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get("checks"))
            .and_then(|c| c.as_object())
            .map(|c| c.len())
            .unwrap_or(0);
        let all = all && n_checks >= 10;
        println!("{}", snapshot(all));
        let _ = std::io::stdout().flush();
        v.source.shutdown();
        std::process::exit(if all { 0 } else { 1 });
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
            && c.scrollback_lines == 5000
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
    let last = v.cols - 1;
    v.sel = Some(Sel { mode: SelMode::Line, a0: (abs, 0), a1: (abs, last), h0: (abs, 0), h1: (abs, last) });
    let line = v.selection_text();
    // Two rows: the DONE line and the prompt below it, joined by a newline.
    v.sel = Some(Sel { mode: SelMode::Line, a0: (abs, 0), a1: (abs + 1, last), h0: (abs, 0), h1: (abs + 1, last) });
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
