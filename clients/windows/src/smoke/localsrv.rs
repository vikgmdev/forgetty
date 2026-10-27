//! The local server in the `--smoke` gate (WP6). The real `forgetty.exe` (a release build that
//! allows an elevated run, `FORGETTY_SERVER_EXE`) starts with its own `--state-dir`, the app
//! pairs with it by itself, a tab opens in it, keys are timed against the in-process terminal,
//! the server is killed and must come back, and it is stopped at the end. Skipped when
//! `FORGETTY_SERVER_EXE` is not set (the installed copy's smoke).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forgetty_shell_core::localserver::{self as ls, Fallback, Plan};
use forgetty_shell_core::settings::Page;
use forgetty_shell_core::{MachineKind, PaneId, Uuid};
use gpui::{Context, Window};
use serde_json::json;

use super::{check, fail, model_ms, ms, record, snap, summarize_keys, summarize_model, Phase, Probe, ShellSmoke, RESULTS};
use crate::localserver::{Cli, LsState};
use crate::shell::AppShell;

const PROBES: usize = 100;

#[derive(Default)]
pub struct LsPhase {
    cli: Option<Cli>,
    server: Option<Uuid>,
    inproc: Option<Uuid>,
    pane: Option<PaneId>,
    tabs_before: usize,
    base_col: usize,
    pid: u64,
    t0: Option<Instant>,
    rtt_started: bool,
    probe_started: bool,
    tab_focused: bool,
    /// The PowerShell pair: shells (name, argv), the one under test, and its tab and probes.
    ps_shells: Vec<(String, Vec<String>)>,
    ps_idx: usize,
    ps_tabs_before: usize,
    ps_focused: bool,
    ps_tab: Option<Uuid>,
    ps_pane: Option<PaneId>,
    ps_base_col: usize,
    ps_model: Vec<f64>,
    ps_probe: Option<(usize, usize, char, Instant)>,
    ps_log_mark: usize,
    ps_back_switched: bool,
    ps_back_restored: bool,
    ps_segs: Vec<[f64; 4]>,
    /// First key send and last echo of the UI key probes.
    ui_window: Option<(Instant, Instant)>,
    /// Per UI key: ms until a second connection saw the pane's output with that key.
    c2_ms: Vec<f64>,
    /// Per key: ms from the key to the writer task, to the client queue, to the output reaching the
    /// client, to the terminal model.
    segs: Vec<[f64; 4]>,
}

/// Round trips of a small request to the server, in ms, and whether the loop has ended.
static RTT_MS: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());
static RTT_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn p50_of(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.get(s.len() / 2).copied().unwrap_or(f64::NAN)
}

/// The split of one key: stamps are the app's (`forgetty_remote::trace`), `model` is when the echo
/// reached the terminal model. None when a stamp is missing.
fn split_key(t0: Instant, model: Instant) -> Option<[f64; 4]> {
    let log = forgetty_remote::trace::take();
    let first = |kind: &str, after: Instant| log.iter().find(|(k, t)| *k == kind && *t >= after).map(|(_, t)| *t);
    let queued = first("queued", t0)?;
    let popped = first("popped", queued)?;
    let handed = first("handed", popped)?;
    let recv = log.iter().rev().find(|(k, t)| *k == "recv" && *t <= model && *t >= handed).map(|(_, t)| *t)?;
    let d = |a: Instant, b: Instant| ms(b.saturating_duration_since(a));
    Some([d(t0, popped), d(popped, handed), d(handed, recv), d(recv, model)])
}

/// Result of the in-app probe: key to echo medians, ms, and whether it has ended.
static PROBE_MS: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());
static PROBE_ERR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static PROBE_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What `timer_probe` of the server does, inside this process on the app's runtime: its own
/// connection, its own pane, one byte every 20 ms with no UI in the loop, the clock from before
/// `send_input` to the first `PaneItem::Data` with the key. If this is fast, the slow part is in
/// the app's pane source or the UI; if it is slow, it is the link or this process.
#[allow(clippy::too_many_arguments)]
async fn probe_task(
    server: forgetty_client::EndpointAddr,
    identity: forgetty_client::SecretKey,
    info: forgetty_client::ClientInfo,
    opts: forgetty_client::ClientOptions,
    backend: std::sync::Arc<dyn forgetty_shell_core::SessionBackend>,
    ui_pane: Uuid,
) -> Result<(), String> {
    use forgetty_client::PaneItem;
    let c = forgetty_client::Client::connect_with(server, identity, info, opts).await.map_err(|e| format!("connect: {e}"))?;
    let reply = backend
        .new_tab(forgetty_shell_core::NewTab { workspace_idx: 0, rows: 24, cols: 80, command: vec![crate::reload::smoke_cmd()], ..Default::default() })
        .await
        .map_err(|e| format!("new_tab: {e}"))?;
    let pane = reply.pane_id.0;
    let mut stream = c.attach_pane(pane).await.map_err(|e| format!("attach: {e}"))?;
    // Let the prompt settle: read until 1.5 s of quiet.
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(1500), stream.next()).await {}
    for i in 0..60usize {
        let key = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        c.send_input(pane, &[key]).await.map_err(|e| format!("send: {e}"))?;
        loop {
            let left = Duration::from_secs(5).saturating_sub(t0.elapsed());
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(PaneItem::Data { bytes, .. })) if bytes.contains(&key) => break,
                Ok(Some(_)) => {}
                _ => return Err(format!("no echo of {:?}", key as char)),
            }
        }
        PROBE_MS.lock().unwrap().push(ms(t0.elapsed()));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // E5: the own pane at the size of the UI pane (is it the size?).
    if let Ok(info) = backend.get_pane_info(forgetty_shell_core::PaneId(ui_pane)).await {
        let _ = c.resize(pane, info.rows, info.cols).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(300), stream.next()).await {}
        for i in 0..40usize {
            let key = b'a' + (i % 26) as u8;
            let t0 = Instant::now();
            c.send_input(pane, &[key]).await.map_err(|e| format!("send: {e}"))?;
            loop {
                let left = Duration::from_secs(5).saturating_sub(t0.elapsed());
                match tokio::time::timeout(left, stream.next()).await {
                    Ok(Some(PaneItem::Data { bytes, .. })) if bytes.contains(&key) => break,
                    Ok(Some(_)) => {}
                    _ => return Err(format!("resized pane: no echo of {:?}", key as char)),
                }
            }
            E5_MS.lock().unwrap().push(ms(t0.elapsed()));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // The server reports the command a pane was started with (`argv` of the pane info).
    if let Ok(info) = backend.get_pane_info(forgetty_shell_core::PaneId(ui_pane)).await {
        *UI_PANE_ARGV.lock().unwrap() = Some(info.argv);
    }
    // E2: the young connection typing into the pane the UI shows (the line is cleared after).
    {
        let mut ui = c.attach_pane(ui_pane).await.map_err(|e| format!("attach ui pane: {e}"))?;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(1500), ui.next()).await {}
        for i in 0..60usize {
            let key = b'a' + (i % 26) as u8;
            let t0 = Instant::now();
            c.send_input(ui_pane, &[key]).await.map_err(|e| format!("send: {e}"))?;
            loop {
                let left = Duration::from_secs(5).saturating_sub(t0.elapsed());
                match tokio::time::timeout(left, ui.next()).await {
                    Ok(Some(PaneItem::Data { bytes, .. })) if bytes.contains(&key) => break,
                    Ok(Some(_)) => {}
                    _ => return Err(format!("ui pane: no echo of {:?}", key as char)),
                }
            }
            E2_MS.lock().unwrap().push(ms(t0.elapsed()));
            if i % 10 == 9 {
                let _ = c.send_input(ui_pane, &[0x1b]).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = c.send_input(ui_pane, &[0x1b]).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = backend.close_tab(reply.tab_id).await;
    c.close().await;
    Ok(())
}

static UI_PANE_ARGV: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);
static E5_MS: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());
static E2_MS: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());
static E3_MS: std::sync::Mutex<Vec<f64>> = std::sync::Mutex::new(Vec::new());

/// Output of the UI pane as a second connection saw it: when each `Data` item arrived.
static C2_RECV: std::sync::Mutex<Vec<(Instant, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
static CONC_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// While the key probes of the UI pane run, a second connection attached to the same pane logs
/// when its output arrives: does another connection see the UI pane's echo earlier?
async fn concurrent_task(
    server: forgetty_client::EndpointAddr,
    identity: forgetty_client::SecretKey,
    info: forgetty_client::ClientInfo,
    opts: forgetty_client::ClientOptions,
    backend: std::sync::Arc<dyn forgetty_shell_core::SessionBackend>,
    ui_pane: Uuid,
) -> Result<(), String> {
    use forgetty_client::PaneItem;
    let c = forgetty_client::Client::connect_with(server, identity, info, opts).await.map_err(|e| format!("connect: {e}"))?;
    let mut ui = c.attach_pane(ui_pane).await.map_err(|e| format!("attach ui pane: {e}"))?;
    tokio::spawn(async move {
        while let Some(item) = ui.next().await {
            if let PaneItem::Data { bytes, .. } = item {
                C2_RECV.lock().unwrap().push((Instant::now(), bytes.to_vec()));
            }
        }
    });
    // The own-pane loop of this task answered already (see `local_server_probe_variants_ms`): only
    // the listener on the UI pane stays, until the key probes end.
    while !CONC_STOP.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = backend;
    c.close().await;
    Ok(())
}

/// Medians (microseconds) of the stage gaps the server logged for echoes (`FORGETTY_TRACE_LATENCY`).
fn server_stages(state: &std::path::Path) -> serde_json::Value {
    fn find(dir: &std::path::Path, depth: u32) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
            let p = e.path();
            if p.is_file() && p.file_name().is_some_and(|n| n == "server.log") {
                return Some(p);
            }
            if depth > 0 && p.is_dir() {
                if let Some(f) = find(&p, depth - 1) {
                    return Some(f);
                }
            }
        }
        None
    }
    let text = find(state, 5).and_then(|l| std::fs::read_to_string(l).ok()).unwrap_or_default();
    stages_json(&text.lines().filter(|l| l.contains("key echo:")).map(str::to_string).collect::<Vec<_>>())
}

/// The `key echo:` lines of the server log so far (to mark where a phase starts).
fn key_echo_lines(state: &std::path::Path) -> Vec<String> {
    fn find(dir: &std::path::Path, depth: u32) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
            let p = e.path();
            if p.is_file() && p.file_name().is_some_and(|n| n == "server.log") {
                return Some(p);
            }
            if depth > 0 && p.is_dir() {
                if let Some(f) = find(&p, depth - 1) {
                    return Some(f);
                }
            }
        }
        None
    }
    let text = find(state, 5).and_then(|l| std::fs::read_to_string(l).ok()).unwrap_or_default();
    text.lines().filter(|l| l.contains("key echo:")).map(str::to_string).collect()
}

/// Medians (microseconds) of the five stage numbers on `key echo:` lines.
fn stages_json(lines: &[String]) -> serde_json::Value {
    let mut cols: Vec<Vec<i64>> = vec![Vec::new(); 5];
    for line in lines {
        let nums: Vec<i64> = line.split(|c: char| !(c.is_ascii_digit() || c == '-')).filter_map(|t| t.parse().ok()).collect();
        if nums.len() == 5 {
            for (c, n) in cols.iter_mut().zip(nums) {
                c.push(n);
            }
        }
    }
    if cols[0].is_empty() {
        return json!({"n": 0});
    }
    let med = |v: &mut Vec<i64>| {
        v.sort();
        v[v.len() / 2]
    };
    let m: Vec<i64> = cols.iter_mut().map(med).collect();
    json!({"n": cols[0].len(), "input_to_pty_write": m[0], "to_first_pty_read": m[1], "to_pane_event": m[2], "to_frame_written": m[3], "total": m[4]})
}

fn recorded(key: &str) -> Option<f64> {
    RESULTS.lock().unwrap().as_ref().and_then(|m| m.get(key)).and_then(|v| v.as_f64())
}

impl ShellSmoke {
    /// One PowerShell, one leg: open a tab with it (in the app's own terminal, or through the
    /// server), wait for the prompt, time `PROBES` keys to the terminal model. Through the server it
    /// is gated against the same shell in-process. The tab closes after. `goto` of the same phase
    /// starts the next shell with a fresh phase timer.
    fn ps_step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, server: bool) {
        let (name, argv) = self.ls.ps_shells[self.ls.ps_idx].clone();
        let leg = match self.phase {
            Phase::LsPsSrv => "server",
            Phase::LsPsBack => "in_process_b",
            _ => "in_process",
        };
        match self.sub {
            0 => {
                self.ls.ps_tabs_before = shell.layout.tab_count();
                self.ls.ps_focused = false;
                self.ls.ps_model.clear();
                self.ls.ps_probe = None;
                self.ls.ps_segs.clear();
                if server {
                    self.ls.ps_log_mark = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()).map(|d| key_echo_lines(d).len()).unwrap_or(0);
                    forgetty_remote::trace::enable(true);
                }
                // The same start folder on both legs: the home folder.
                shell.new_tab_with(argv, std::env::var("USERPROFILE").ok(), cx);
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            1 => {
                if shell.layout.tab_count() <= self.ls.ps_tabs_before {
                    if self.sub_t0.elapsed() > Duration::from_secs(20) {
                        fail(&format!("powershell {leg} {name}: the new tab did not appear"));
                    }
                    return;
                }
                let Some((tab, p)) = shell.layout.active_workspace().and_then(|w| w.tabs.last().map(|t| (t.id, t.pane_tree.first_leaf()))) else { return };
                if shell.active_pane() != Some(p) {
                    if !self.ls.ps_focused {
                        self.ls.ps_focused = true;
                        shell.focus_tab(tab, cx);
                    }
                    if self.sub_t0.elapsed() > Duration::from_secs(30) {
                        fail(&format!("powershell {leg} {name}: the new tab never became active"));
                    }
                    return;
                }
                let Some(s) = snap(shell, cx, p) else { return };
                if let Some(row) = s.rows.iter().find(|r| r.trim_end().ends_with('>')) {
                    record(&format!("powershell_{leg}_{name}_prompt"), json!(row.trim_end()));
                    self.ls.ps_tab = Some(tab);
                    self.ls.ps_pane = Some(p);
                    shell.focus_pane(p, window, cx);
                    self.sub = 2;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_secs(40) {
                    record(&format!("powershell_{leg}_{name}_screen"), json!(s.rows));
                    fail(&format!("powershell {leg} {name}: no prompt in 40 s"));
                }
            }
            2 => {
                // PSReadLine loads a moment after the prompt shows.
                let Some(p) = self.ls.ps_pane else { return };
                if shell.active_pane() != Some(p) {
                    shell.focus_pane(p, window, cx);
                    return;
                }
                if self.sub_t0.elapsed() < Duration::from_millis(1500) {
                    return;
                }
                let Some(view) = shell.views.get(&p).cloned() else { return };
                self.ls.ps_base_col = view.read(cx).term.cursor().1;
                self.sub = 3;
                self.sub_t0 = Instant::now();
            }
            3 | 5 => {
                let Some(p) = self.ls.ps_pane else { return };
                let Some(view) = shell.views.get(&p).cloned() else { return };
                let (cr, cc) = view.read(cx).term.cursor();
                if cc == self.ls.ps_base_col && self.sub_t0.elapsed() > Duration::from_millis(100) {
                    self.ps_send(&view, cx, cr, cc);
                    self.sub = 4;
                } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                    fail(&format!("powershell {leg} {name}: the prompt line did not come back"));
                }
            }
            _ => {
                let Some(p) = self.ls.ps_pane else { return };
                let Some(view) = shell.views.get(&p).cloned() else { return };
                let Some((row, col, ch, t0)) = self.ls.ps_probe else { fail("powershell probe state lost") };
                if t0.elapsed() > Duration::from_millis(3000) {
                    fail(&format!("powershell {leg} {name}: echo did not arrive"));
                }
                if !view.read(cx).term.screen().cell(row, col).grapheme.starts_with(ch) {
                    return;
                }
                self.ls.ps_probe = None;
                self.ls.ps_model.push(model_ms(view.read(cx)));
                if server {
                    if let Some(model) = view.read(cx).probe_watch.as_ref().and_then(|w| w.model_at) {
                        if let Some(seg) = split_key(t0, model) {
                            self.ls.ps_segs.push(seg);
                        }
                    }
                }
                if self.ls.ps_model.len() < PROBES {
                    if self.ls.ps_model.len() % 20 == 0 {
                        view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                        self.sub = 5;
                        self.sub_t0 = Instant::now();
                    } else {
                        let (r, c) = view.read(cx).term.cursor();
                        self.ps_send(&view, cx, r, c);
                    }
                    return;
                }
                view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                let p50 = summarize_model(&self.ls.ps_model, &format!("_powershell_{leg}_{name}"));
                if server {
                    forgetty_remote::trace::enable(false);
                    let col = |i: usize| self.ls.ps_segs.iter().map(|s| s[i]).collect::<Vec<_>>();
                    let tot: Vec<f64> = self.ls.ps_segs.iter().map(|s| s.iter().sum()).collect();
                    record(
                        &format!("powershell_server_{name}_app_split_p50_ms"),
                        json!({"n": self.ls.ps_segs.len(), "key_to_writer_task": p50_of(&col(0)), "writer_task_to_client_queue": p50_of(&col(1)),
                               "queue_to_output_at_client": p50_of(&col(2)), "output_at_client_to_model": p50_of(&col(3)), "total": p50_of(&tot)}),
                    );
                    if let Some(d) = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()) {
                        let lines = key_echo_lines(d);
                        let mine = lines.get(self.ls.ps_log_mark..).unwrap_or(&[]);
                        record(&format!("powershell_server_{name}_stage_us"), stages_json(mine));
                    }
                    // The gate: what Forgetty adds per key. The shell's own time is the server's
                    // "input to first pty read with the key"; the rest of the app's key to model is
                    // the server's dispatch and frame, the link and the app.
                    let stages = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()).map(|d| {
                        let lines = key_echo_lines(d);
                        stages_json(lines.get(self.ls.ps_log_mark..).unwrap_or(&[]))
                    });
                    let shell_ms = stages.as_ref().and_then(|s| s.get("to_first_pty_read")).and_then(|v| v.as_f64()).map(|us| us / 1000.0);
                    let Some(shell_ms) = shell_ms else { fail(&format!("powershell {name}: the server logged no key echo stages (FORGETTY_TRACE_LATENCY)")) };
                    record(
                        &format!("powershell_{name}_forgetty_overhead_ms"),
                        json!({"app_key_to_model_p50": p50, "server_input_to_first_pty_read_p50": shell_ms, "overhead": p50 - shell_ms}),
                    );
                    check(&format!("local_server_powershell_{name}_overhead_within_2ms"), p50 - shell_ms <= 2.0);
                }
                if leg == "in_process_b" {
                    // Report only: the raw pair, in-process, server, in-process.
                    let get = |l: &str| recorded(&format!("key_to_model_p50_ms_powershell_{l}_{name}"));
                    let (a, s, b) = (get("in_process"), get("server"), get("in_process_b"));
                    if let (Some(a), Some(s), Some(b)) = (a, s, b) {
                        record(&format!("powershell_{name}_raw_pair_ms"), json!({"in_process_a": a, "server": s, "in_process_b": b, "delta_vs_mean_of_a_b": s - (a + b) / 2.0}));
                    }
                }
                if let Some(tab) = self.ls.ps_tab.take() {
                    shell.close_tab(tab, cx);
                }
                self.ls.ps_idx += 1;
                let again = self.phase;
                self.goto(again);
            }
        }
    }

    fn ps_send(&mut self, view: &gpui::Entity<crate::view::TerminalView>, cx: &mut Context<AppShell>, row: usize, col: usize) {
        let ch = (b'a' + (self.ls.ps_model.len() % 26) as u8) as char;
        forgetty_remote::trace::clear();
        let t0 = Instant::now();
        self.ls.ps_probe = Some((row, col, ch, t0));
        view.update(cx, |v, _| {
            v.probe_watch = Some(crate::view::ProbeWatch { row, col, ch, t0, model_at: None });
            v.send_bytes(ch.to_string().as_bytes())
        });
    }

    pub(super) fn ls_step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, now_t: Instant) {
        match self.phase {
            Phase::LsPsBase | Phase::LsPsSrv | Phase::LsPsBack => {
                let server = self.phase == Phase::LsPsSrv;
                let back = self.phase == Phase::LsPsBack;
                if back && !self.ls.ps_shells.is_empty() && !self.ls.ps_back_switched {
                    // The third leg of the raw pair runs on the in-process machine again.
                    let Some(inproc) = self.ls.inproc else { fail("local server: no in-process machine to go back to") };
                    shell.switch_machine(inproc, cx);
                    self.ls.ps_back_switched = true;
                    return;
                }
                if self.phase == Phase::LsPsBase && self.sub == 0 && self.ls.ps_idx == 0 && self.ls.ps_shells.is_empty() {
                    if std::env::var_os("FORGETTY_SERVER_EXE").is_none() {
                        self.goto(Phase::LsStart);
                        return;
                    }
                    self.ls.ps_shells = crate::reload::smoke_powershells();
                    record("ps_shells", json!(self.ls.ps_shells.iter().map(|(n, a)| json!({"name": n, "argv": a})).collect::<Vec<_>>()));
                }
                if self.ls.ps_idx >= self.ls.ps_shells.len() {
                    // This leg is done: the next leg starts at shell 0.
                    if back && self.ls.ps_back_switched && !self.ls.ps_back_restored {
                        // Back on the server's machine for the rest of the phases.
                        let Some(srv) = self.ls.server else { fail("local server: no server machine to return to") };
                        shell.switch_machine(srv, cx);
                        self.ls.ps_back_restored = true;
                        return;
                    }
                    self.ls.ps_idx = 0;
                    self.goto(match self.phase {
                        Phase::LsPsBase => Phase::LsStart,
                        Phase::LsPsSrv => Phase::LsPsBack,
                        _ => Phase::LsDevices,
                    });
                    return;
                }
                self.ps_step(shell, window, cx, server);
            }
            Phase::LsStart => {
                let Some(exe) = std::env::var_os("FORGETTY_SERVER_EXE").map(PathBuf::from) else {
                    record("local_server", json!("skipped: FORGETTY_SERVER_EXE is not set"));
                    self.goto(Phase::RSetup);
                    return;
                };
                // The rules, on this runner: an elevated window never uses the server, and the
                // test variable lifts that for the smoke only.
                let elevated = ls::is_elevated();
                record("runner_elevated", json!(elevated));
                if elevated {
                    check("local_server_elevated_window_stays_in_process", crate::localserver::decide(true, false) == Plan::InProcess(Fallback::Elevated));
                }
                check("local_server_off_stays_in_process", crate::localserver::decide(false, true) == Plan::InProcess(Fallback::Disabled));
                let plan = crate::localserver::decide(true, true);
                check("local_server_plan_finds_the_exe", plan == Plan::Server(exe.clone()));

                let state = std::env::temp_dir().join(format!("forgetty-smoke-server-state-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&state);
                if let Ok(mut g) = super::LOCAL_SERVER.lock() {
                    *g = Some((exe.clone(), state.clone()));
                }
                // The server logs the stages of an echo (read back after the key probes).
                std::env::set_var("FORGETTY_TRACE_LATENCY", "1");
                std::env::set_var("RUST_LOG", "latency=info");
                self.ls.cli = Some(Cli { exe: exe.clone(), state_dir: Some(state) });
                self.ls.inproc = Some(shell.machine.id);
                self.ls.tabs_before = shell.layout.tab_count();
                shell.lsrv.keep_inproc = true;
                shell.start_local_server(Plan::Server(exe), self.ls.cli.as_ref().and_then(|c| c.state_dir.clone()), cx);
                check("local_server_starting_holds_the_first_tab", shell.lsrv.pending());
                self.goto(Phase::LsWait);
            }
            // ---- up, paired, shown as This PC ---------------------------------------------
            Phase::LsWait => {
                if let LsState::Fallback(f) = &shell.lsrv.state {
                    let log = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()).map(server_log_tail).unwrap_or_default();
                    fail(&format!("local server: fell back to in-process: {f:?}; server log: {log}"));
                }
                if self.phase_t0.elapsed() > Duration::from_secs(120) {
                    fail(&format!("local server: not up after 120 s ({:?})", shell.lsrv.state));
                }
                let Some(id) = shell.lsrv.machine else { return };
                let Some(e) = shell.machines.iter().find(|m| m.id == id) else { return };
                if !e.conn.is_connected() || shell.machine.id != id {
                    return;
                }
                if self.sub == 0 {
                    // The empty server gets its first tab from the shell (the default shell).
                    // Let that land, so the cmd.exe tab below is the one on screen.
                    if !e.bootstrapped || shell.layout.tab_count() == 0 {
                        if self.phase_t0.elapsed() > Duration::from_secs(60) {
                            fail("local server: the first tab did not open");
                        }
                        return;
                    }
                    self.ls.server = Some(id);
                    let (name, kind) = (e.name.clone(), e.kind);
                    check("local_server_shown_as_this_pc", name == ls::THIS_PC && kind == MachineKind::Remote);
                    check("local_server_is_the_machine_on_screen", shell.machine.id == id && shell.is_this_pc());
                    let stored = shell.remote.as_ref().and_then(|r| r.store.local_machine());
                    let loopback = stored.as_ref().is_some_and(|m| m.relays_disabled && !m.pinned_addrs.is_empty() && m.pinned_addrs.iter().all(|a| a.ip().is_loopback()));
                    check("local_server_paired_over_loopback_and_stored", loopback);
                    record("local_server_stored", json!(stored.map(|m| m.pinned_addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>())));
                    // The smoke keeps the in-process machine; the real start drops it. Both are
                    // the same call, so check the call on a copy of the rule.
                    check("local_server_pair_dialog_untouched", shell.pair.is_none());
                    // A tab with cmd.exe, to time keys on the same program as the in-process pane.
                    self.ls.tabs_before = shell.layout.tab_count();
                    shell.new_tab_with(vec!["cmd.exe".into()], None, cx);
                    self.sub = 1;
                    self.sub_t0 = Instant::now();
                    return;
                }
                if shell.layout.tab_count() <= self.ls.tabs_before {
                    if self.sub_t0.elapsed() > Duration::from_secs(20) {
                        fail("local server: the new tab did not appear");
                    }
                    return;
                }
                // The pane of the tab just opened (the last one), not whatever pane is active: the
                // first tab of the server runs the default shell, which may be PowerShell.
                let Some((tab, p)) = shell.layout.active_workspace().and_then(|w| w.tabs.last().map(|t| (t.id, t.pane_tree.first_leaf()))) else { return };
                if shell.active_pane() != Some(p) {
                    if !self.ls.tab_focused {
                        self.ls.tab_focused = true;
                        shell.focus_tab(tab, cx);
                    }
                    if self.sub_t0.elapsed() > Duration::from_secs(30) {
                        fail(&format!("local server: the new tab never became active (active pane {:?}, new pane {:?})", shell.active_pane(), p));
                    }
                    return;
                }
                let Some(s) = snap(shell, cx, p) else { return };
                if let Some(row) = s.rows.iter().find(|r| r.trim_end().ends_with('>')) {
                    record("local_server_tab_prompt", json!(row.trim_end()));
                    // cmd.exe: its prompt is `C:\...>`, PowerShell's starts with `PS `.
                    check("local_server_tab_runs_cmd", !row.trim_start().starts_with("PS "));
                    self.ls.pane = Some(p);
                    shell.focus_pane(p, window, cx);
                    self.goto(Phase::LsKeys);
                } else if self.sub_t0.elapsed() > Duration::from_secs(30) {
                    record("local_server_screen", json!(s.rows));
                    fail("local server: no cmd prompt in the new tab");
                }
            }
            // ---- key-to-frame against the in-process pane ---------------------------------
            Phase::LsKeys => {
                let Some(p0) = self.ls.pane else { return };
                if shell.active_pane() != Some(p0) {
                    shell.focus_pane(p0, window, cx);
                    return;
                }
                let Some(view) = shell.views.get(&p0).cloned() else { return };
                // cmd.exe: probes go at the cursor of the prompt line, as for the in-process pane.
                // Escape clears the typed line (every 20 keys, so the line never wraps).
                let (cr, cc) = view.read(cx).term.cursor();
                match self.sub {
                    0 => {
                        if !self.ls.rtt_started {
                            // The round trip of a small request, idle: the floor of the link and
                            // the server's dispatch, to tell from the PTY and the output path.
                            self.ls.rtt_started = true;
                            let id = self.ls.server;
                            let backend = id.and_then(|id| shell.machines.iter().find(|m| m.id == id).map(|m| m.backend.clone()));
                            let rt = shell.remote.as_ref().map(|r| r.rt.clone());
                            let (Some(backend), Some(rt)) = (backend, rt) else { fail("local server: no backend for the round trip probe") };
                            rt.spawn(async move {
                                for _ in 0..50 {
                                    let t = Instant::now();
                                    if backend.get_layout().await.is_ok() {
                                        RTT_MS.lock().unwrap().push(ms(t.elapsed()));
                                    }
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                }
                                RTT_DONE.store(true, std::sync::atomic::Ordering::SeqCst);
                            });
                            self.sub_t0 = Instant::now();
                            return;
                        }
                        if !RTT_DONE.load(std::sync::atomic::Ordering::SeqCst) && self.sub_t0.elapsed() < Duration::from_secs(20) {
                            return;
                        }
                        {
                            let v = RTT_MS.lock().unwrap().clone();
                            let mut s = v.clone();
                            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
                            record(
                                "local_server_rpc_rtt_ms",
                                json!({"n": s.len(), "p50": p50_of(&v), "min": s.first(), "p95": s.get(s.len() * 95 / 100), "max": s.last()}),
                            );
                        }
                        // The in-app probe (no UI in the loop), then the key probes below.
                        if !self.ls.probe_started {
                            self.ls.probe_started = true;
                            let id = self.ls.server;
                            let backend = id.and_then(|id| shell.machines.iter().find(|m| m.id == id).map(|m| m.backend.clone()));
                            let parts = shell.remote.as_ref().and_then(|r| {
                                let m = r.store.local_machine()?;
                                let info = forgetty_client::ClientInfo::new(Uuid::new_v4(), forgetty_client::proto::ClientKind::Windows, "latency probe", env!("CARGO_PKG_VERSION"));
                                Some((r.rt.clone(), forgetty_remote::store::machine_addr(&m).ok()?, r.store.identity().ok()?, info, forgetty_remote::store::machine_options(&r.addr_cache, &m)))
                            });
                            let ui_pane = self.ls.pane.map(|p| p.0);
                            let (Some(backend), Some((rt, server, identity, info, opts)), Some(ui_pane)) = (backend, parts, ui_pane) else { fail("local server: no parts for the in-app probe") };
                            rt.spawn(async move {
                                if let Err(e) = probe_task(server, identity, info, opts, backend, ui_pane).await {
                                    *PROBE_ERR.lock().unwrap() = Some(e);
                                }
                                PROBE_DONE.store(true, std::sync::atomic::Ordering::SeqCst);
                            });
                            self.sub_t0 = Instant::now();
                            return;
                        }
                        if !PROBE_DONE.load(std::sync::atomic::Ordering::SeqCst) && self.sub_t0.elapsed() < Duration::from_secs(60) {
                            return;
                        }
                        {
                            let v = PROBE_MS.lock().unwrap().clone();
                            let mut sorted = v.clone();
                            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                            record(
                                "local_server_in_app_probe_ms",
                                json!({"n": sorted.len(), "p50": p50_of(&v), "p95": sorted.get(sorted.len() * 95 / 100), "max": sorted.last(), "error": PROBE_ERR.lock().unwrap().clone()}),
                            );
                            let argv = UI_PANE_ARGV.lock().unwrap().clone();
                            record("local_server_pane_info_argv", json!(argv));
                            // The tab was started with the smoke's cmd.exe: the server says so.
                            check("local_server_pane_info_reports_argv", argv.as_ref().is_some_and(|a| a.first().is_some_and(|p| p.to_ascii_lowercase().ends_with("cmd.exe"))));
                            let (e2, e3) = (E2_MS.lock().unwrap().clone(), E3_MS.lock().unwrap().clone());
                            record(
                                "local_server_probe_variants_ms",
                                json!({"own_pane_own_connection": p50_of(&v),
                                       "app_connection_and_pane_source_own_pane": {"n": e3.len(), "p50": p50_of(&e3)},
                                       "own_connection_into_ui_pane": {"n": e2.len(), "p50": p50_of(&e2)},
                                       "own_pane_resized_to_the_ui_pane": {"n": E5_MS.lock().unwrap().len(), "p50": p50_of(&E5_MS.lock().unwrap())}}),
                            );
                            if let Some(d) = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()) {
                                record("local_server_stage_us_after_in_app_probe", server_stages(d));
                            }
                        }
                        {
                            let id = self.ls.server;
                            let backend = id.and_then(|id| shell.machines.iter().find(|m| m.id == id).map(|m| m.backend.clone()));
                            let parts = shell.remote.as_ref().and_then(|r| {
                                let m = r.store.local_machine()?;
                                let info = forgetty_client::ClientInfo::new(Uuid::new_v4(), forgetty_client::proto::ClientKind::Windows, "latency probe 2", env!("CARGO_PKG_VERSION"));
                                Some((r.rt.clone(), forgetty_remote::store::machine_addr(&m).ok()?, r.store.identity().ok()?, info, forgetty_remote::store::machine_options(&r.addr_cache, &m)))
                            });
                            let ui_pane = self.ls.pane.map(|p| p.0);
                            if let (Some(backend), Some((rt, server, identity, info, opts)), Some(ui_pane)) = (backend, parts, ui_pane) {
                                C2_RECV.lock().unwrap().clear();
                                CONC_STOP.store(false, std::sync::atomic::Ordering::SeqCst);
                                rt.spawn(async move {
                                    if let Err(e) = concurrent_task(server, identity, info, opts, backend, ui_pane).await {
                                        eprintln!("forgetty-win: smoke concurrent probe: {e}");
                                    }
                                });
                            }
                        }
                        self.ls.ui_window = None;
                        forgetty_remote::trace::enable(true);
                        self.ls.segs.clear();
                        self.ls.base_col = cc;
                        self.lat_ms.clear();
                        self.lat_model_ms.clear();
                        self.max_reshaped_per_key = 0;
                        self.sub = 1;
                    }
                    1 | 3 => {
                        if cc == self.ls.base_col && self.sub_t0.elapsed() > Duration::from_millis(100) {
                            let ch = self.next_key_char();
                            forgetty_remote::trace::clear();
                            let t0 = Instant::now();
                            self.probe = Some(Probe { row: cr, col: cc, ch, t0 });
                            view.update(cx, |v, _| {
                                v.probe_watch = Some(crate::view::ProbeWatch { row: cr, col: cc, ch, t0, model_at: None });
                                v.send_bytes(ch.to_string().as_bytes())
                            });
                            self.sub = 4;
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("local server key probe: the prompt line did not come back");
                        }
                    }
                    _ => {
                        if self.probe.as_ref().is_some_and(|p| p.t0.elapsed() > Duration::from_millis(3000)) {
                            fail("local server key probe: echo did not arrive");
                        }
                        let (done, reshaped) = {
                            let v = view.read(cx);
                            match &self.probe {
                                Some(p) => (v.term.screen().cell(p.row, p.col).grapheme.starts_with(p.ch), v.rows_reshaped_last),
                                None => (false, 0),
                            }
                        };
                        if !done {
                            return;
                        }
                        let p = self.probe.take().unwrap();
                        self.lat_ms.push(ms(now_t - p.t0));
                        self.lat_model_ms.push(model_ms(view.read(cx)));
                        {
                            let now = Instant::now();
                            self.ls.ui_window = Some(match self.ls.ui_window {
                                Some((a, _)) => (a, now),
                                None => (p.t0, now),
                            });
                            let seen = C2_RECV.lock().unwrap().iter().find(|(t, b)| *t >= p.t0 && b.contains(&(p.ch as u8))).map(|(t, _)| ms(t.saturating_duration_since(p.t0)));
                            if let Some(v) = seen {
                                self.ls.c2_ms.push(v);
                            }
                        }
                        if let Some(model) = view.read(cx).probe_watch.as_ref().and_then(|w| w.model_at) {
                            if let Some(seg) = split_key(p.t0, model) {
                                self.ls.segs.push(seg);
                            }
                        }
                        self.max_reshaped_per_key = self.max_reshaped_per_key.max(reshaped);
                        if self.lat_ms.len() >= PROBES {
                            let (frame50, _) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "_local_server");
                            let model50 = summarize_model(&self.lat_model_ms, "_local_server");
                            self.lat_model_ms.clear();
                            forgetty_remote::trace::enable(false);
                            CONC_STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                            record(
                                "local_server_second_connection_saw_ui_echo_ms",
                                json!({"n": self.ls.c2_ms.len(), "p50": p50_of(&self.ls.c2_ms)}),
                            );
                            {
                                let col = |i: usize| self.ls.segs.iter().map(|s| s[i]).collect::<Vec<_>>();
                                let tot: Vec<f64> = self.ls.segs.iter().map(|s| s.iter().sum()).collect();
                                record(
                                    "local_server_key_split_p50_ms",
                                    json!({
                                        "n": self.ls.segs.len(),
                                        "key_to_writer_task": p50_of(&col(0)),
                                        "writer_task_to_client_queue": p50_of(&col(1)),
                                        "queue_to_output_at_client": p50_of(&col(2)),
                                        "output_at_client_to_model": p50_of(&col(3)),
                                        "total": p50_of(&tot),
                                    }),
                                );
                            }
                            if let Some(d) = self.ls.cli.as_ref().and_then(|c| c.state_dir.as_deref()) {
                                record("local_server_stage_us_all", server_stages(d));
                            }
                            // A number that was not measured fails the gate: no default.
                            let Some(base_model) = recorded("key_to_model_p50_ms") else { fail("local server: no in-process model latency was measured") };
                            let Some(base_frame) = recorded("key_to_frame_p50_ms") else { fail("local server: no in-process frame latency was measured") };
                            record(
                                "local_server_vs_in_process_ms",
                                json!({
                                    "model": {"in_process_p50": base_model, "server_p50": model50, "delta": model50 - base_model},
                                    "frame": {"in_process_p50": base_frame, "server_p50": frame50, "delta": frame50 - base_frame},
                                }),
                            );
                            // The gate: the median of the echo reaching the terminal model, which has no
                            // frame in it, within 2 ms of the built-in terminal.
                            check("local_server_key_p50_within_2ms_of_in_process", model50 - base_model <= 2.0);
                            // The second check: as the user sees it, a frame is the resolution, so one
                            // frame plus the 2 ms.
                            let frame = self.frame_ms();
                            check("local_server_key_frame_p50_within_one_frame_of_in_process", frame50 - base_frame <= frame + 2.0);
                            view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                            self.goto(Phase::LsPsSrv);
                        } else if self.lat_ms.len() % 20 == 0 {
                            view.update(cx, |v, _| v.send_bytes(&[0x1b]));
                            self.sub = 3;
                            self.sub_t0 = Instant::now();
                        } else {
                            let (r, c) = view.read(cx).term.cursor();
                            let ch = self.next_key_char();
                            forgetty_remote::trace::clear();
                            let t0 = Instant::now();
                            self.probe = Some(Probe { row: r, col: c, ch, t0 });
                            view.update(cx, |v, _| {
                                v.probe_watch = Some(crate::view::ProbeWatch { row: r, col: c, ch, t0, model_at: None });
                                v.send_bytes(ch.to_string().as_bytes())
                            });
                        }
                    }
                }
            }
            // ---- Settings, Devices: the list, and a QR code for another device ---------------------
            Phase::LsDevices => {
                let view = |shell: &AppShell| shell.settings.as_ref().map(|w| w.st.devices.view());
                if self.phase_t0.elapsed() > Duration::from_secs(60) {
                    fail("local server: the Devices page did not finish in 60 s");
                }
                match self.sub {
                    0 => {
                        shell.open_settings(Some(Page::Devices), cx);
                        self.sub = 1;
                    }
                    1 => {
                        let Some(v) = view(shell) else { return };
                        if v.rows.is_empty() {
                            return;
                        }
                        check("devices_page_lists_the_app_as_this_pc_local_app", v.rows.iter().any(|r| r.own && r.title.starts_with("This PC (local app)")));
                        check("devices_page_own_device_has_no_revoke_armed", v.rows.iter().all(|r| !r.armed));
                        shell.settings_press("dev-pair", window, cx);
                        self.sub = 2;
                    }
                    2 => {
                        let Some(v) = view(shell) else { return };
                        if let Some(q) = v.qr {
                            // A QR code of version 1 or more: 21 modules and a quiet zone.
                            check("devices_page_pair_new_device_shows_a_qr", q.w >= 21 && q.h >= 21 && !q.runs.is_empty());
                            shell.settings_press("dev-cancel", window, cx);
                            self.sub = 3;
                        }
                    }
                    _ => {
                        let Some(w) = shell.settings.as_ref() else { return };
                        if !w.st.devices.is_pairing() {
                            check("devices_page_pairing_cancel_ends_it", view(shell).is_some_and(|v| v.qr.is_none() && v.message.is_some()));
                            shell.close_settings(window, cx);
                            self.goto(Phase::LsKill);
                        }
                    }
                }
            }
            // ---- the server dies: the supervisor starts it again -------------------------------
            Phase::LsKill => {
                let Some(id) = self.ls.server else { return };
                let Some(cli) = self.ls.cli.clone() else { return };
                match self.sub {
                    0 => {
                        let Ok(st) = cli.status() else { fail("local server: status before the kill failed") };
                        self.ls.pid = st.pid;
                        record("local_server_pid", json!(st.pid));
                        let killed = std::process::Command::new("taskkill")
                            .args(["/PID", &st.pid.to_string(), "/F", "/T"])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success());
                        check("local_server_kill_sent", killed);
                        self.ls.t0 = Some(Instant::now());
                        self.sub = 1;
                    }
                    1 => {
                        // The link drops.
                        if shell.machines.iter().any(|m| m.id == id && !m.conn.is_connected()) {
                            self.sub = 2;
                        } else if self.sub_t0.elapsed() > Duration::from_secs(20) {
                            fail("local server: the link did not drop after the kill");
                        }
                    }
                    _ => {
                        let up = shell.machines.iter().any(|m| m.id == id && m.conn.is_connected());
                        if up {
                            let secs = self.ls.t0.map_or(0.0, |t| t.elapsed().as_secs_f64());
                            record("local_server_restart_s", json!((secs * 10.0).round() / 10.0));
                            check("local_server_watchdog_restarts_it", shell.lsrv.state == LsState::Up);
                            let again = cli.status().ok();
                            check("local_server_restarted_with_a_new_process", again.is_some_and(|s| s.pid != self.ls.pid && s.pid != 0));
                            self.goto(Phase::LsDone);
                        } else if self.phase_t0.elapsed() > Duration::from_secs(90) {
                            record("local_server_state", json!(format!("{:?} tries={}", shell.lsrv.state, shell.lsrv.tries)));
                            fail("local server: not back within 90 s of the kill");
                        }
                    }
                }
            }
            // ---- stop it, put the shell back as the later phases expect it ---------------------
            Phase::LsDone => {
                let (Some(id), Some(inproc), Some(cli)) = (self.ls.server, self.ls.inproc, self.ls.cli.clone()) else { return };
                // Off first: the watchdog must not start the server we are about to stop.
                shell.lsrv.state = LsState::Off;
                shell.lsrv.machine = None;
                shell.switch_machine(inproc, cx);
                shell.drop_machine(id, cx);
                self.ls_pairing_checks(shell, &cli);
                let stop = cli.run(&["server", "stop"], Duration::from_secs(30));
                if let Ok(mut g) = super::LOCAL_SERVER.lock() {
                    *g = None;
                }
                check("local_server_stops", stop.as_ref().is_ok_and(|o| o.code == Some(0)));
                check("local_server_machine_removed_in_process_back", shell.machines.len() == 1 && shell.machine.id == inproc);
                if let Some(d) = cli.state_dir.as_ref() {
                    let _ = std::fs::remove_dir_all(d);
                }
                // The store keeps the pairing of the smoke's server for the later phases: drop it.
                if let Some(r) = shell.remote.as_ref() {
                    if let Some(m) = r.store.local_machine() {
                        let _ = r.store.remove(m.id);
                    }
                }
                self.goto(Phase::RSetup);
            }
            _ => {}
        }
    }
}


/// The last lines of the smoke server's `server.log` (under its state folder), one string.
fn server_log_tail(state: &std::path::Path) -> String {
    fn find(dir: &std::path::Path, depth: u32) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
            let p = e.path();
            if p.is_file() && p.file_name().is_some_and(|n| n == "server.log") {
                return Some(p);
            }
            if depth > 0 && p.is_dir() {
                if let Some(f) = find(&p, depth - 1) {
                    return Some(f);
                }
            }
        }
        None
    }
    let Some(log) = find(state, 5) else { return "no server.log".into() };
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join(" | ")
}

impl ShellSmoke {
    /// The stored pairing is checked at every start: a changed port moves the stored address, and
    /// a device list without this app means pairing again. Both run `bring_up`, as a start does.
    fn ls_pairing_checks(&mut self, shell: &mut AppShell, cli: &Cli) {
        let Some(r) = shell.remote.as_ref() else { return };
        let (rt, store, device) = (r.rt.clone(), r.store.clone(), r.device_name.clone());
        let own = store.device_id().unwrap_or_default();
        let run = |cli: &Cli| {
            let (cli, rt, store, device) = (cli.clone(), rt.clone(), store.clone(), device.clone());
            std::thread::spawn(move || crate::localserver::bring_up(&cli, &rt, &store, &device, None)).join().ok().and_then(Result::ok)
        };
        let Some(first) = store.local_machine() else { fail("local server: no stored machine to check") };
        let want = cli.status().map(|s| s.pins()).unwrap_or_else(|_| vec![ls::loopback_addr(47297)]);

        // 1. The stored address is wrong (the server was somewhere else): it follows the server,
        // and the pairing is kept (same machine id).
        let mut moved = first.clone();
        moved.pinned_addrs = vec![ls::loopback_addr(1)];
        let _ = store.add(moved);
        let kept = run(cli);
        check(
            "local_server_stored_address_follows_the_server",
            kept.as_ref().is_some_and(|m| m.id == first.id && m.pinned_addrs == want) && store.local_machine().is_some_and(|m| m.pinned_addrs == want),
        );

        // 2. The server forgets this app (revoked): the next start pairs again.
        let revoked = cli.run(&["revoke", &own], Duration::from_secs(20));
        record("local_server_revoke_own", json!(revoked.as_ref().map(|o| o.code).map_err(|e| e.to_string())));
        let listed = cli.devices().map(|d| d.iter().any(|x| ls::is_own_device(x, &own))).unwrap_or(true);
        check("local_server_revoked_app_is_gone_from_the_list", !listed);
        let again = run(cli);
        let listed = cli.devices().map(|d| d.iter().any(|x| ls::is_own_device(x, &own))).unwrap_or(false);
        check(
            "local_server_pairs_again_when_the_server_refuses_the_app",
            again.as_ref().is_some_and(|m| m.id != first.id && m.pinned_addrs == want) && listed,
        );
    }
}
