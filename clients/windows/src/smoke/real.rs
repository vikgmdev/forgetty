//! W4 smoke phases: the Windows client against the real Linux `forgetty server
//! run`, inside WSL on the CI runner.
//!
//! Enabled by `FORGETTY_REAL_WSL_DISTRO`. The smoke starts the server through
//! `wsl.exe` as a child process it owns (so the WSL session stays alive), opens
//! a pairing window, pairs through the dialog's code path, opens two panes,
//! types and reads the echo, measures key-to-frame, kills and restarts the
//! server, pushes a 20 MB burst through a pane, and compares the client's final
//! screen with `forgetty capture` run inside WSL.

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use forgetty_shell_core::{ConnState, MachineKind};

use super::*;

const PORT: u16 = 47297;
const BIN: &str = "/usr/local/bin/forgetty";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `seq 1 2500000` writes 19.7 MB.
const BURST_LINES: u32 = 2_500_000;
const BURST_BYTES: f64 = 19_688_895.0;
const PROBES: usize = 100;

pub fn distro() -> Option<String> {
    std::env::var("FORGETTY_REAL_WSL_DISTRO").ok().filter(|s| !s.is_empty())
}

fn wsl(distro: &str, args: &[&str]) -> Command {
    let mut c = Command::new("wsl.exe");
    c.args(["-d", distro, "-u", "root", "--exec"]).args(args);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Lines a child prints, kept for the report.
fn collect(child: &mut Child, sink: &Arc<Mutex<Vec<String>>>) {
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
    {
        let sink = sink.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let mut g = sink.lock().unwrap();
                if g.len() < 400 {
                    g.push(line);
                }
            }
        });
    }
}

#[derive(Default)]
pub struct Real {
    pub distro: String,
    server: Option<Child>,
    pairer: Option<Child>,
    server_log: Arc<Mutex<Vec<String>>>,
    pair_log: Arc<Mutex<Vec<String>>>,
    last_probe: Option<Instant>,
    t_pair: Option<Instant>,
    t_kill: Option<Instant>,
    t_ready: Option<Instant>,
    panes: Vec<PaneId>,
    made_tabs: bool,
    fed_last: u64,
    fed_t: Option<Instant>,
    resets_before: u64,
    restarts: u8,
    port: u16,
    burst_t0: Option<Instant>,
    burst_fed0: u64,
    burst_last: Option<Instant>,
    burst_max_gap: f64,
    cursor: (usize, usize),
}

impl Real {
    pub fn new(distro: String) -> Self {
        Real { distro, port: PORT, ..Default::default() }
    }
}

/// Stop the server inside WSL (used on every exit path).
/// Stop the server and wait until it is gone, so the UDP port is free for a restart.
/// Falls back to SIGKILL after a few seconds.
pub fn stop_server(distro: &str) {
    let _ = wsl(distro, &["pkill", "-TERM", "-f", "forgetty server run"]).output();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(6) {
        let gone = wsl(distro, &["pgrep", "-f", "forgetty server run"]).stdin(Stdio::null()).output().map(|o| !o.status.success()).unwrap_or(true);
        if gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = wsl(distro, &["pkill", "-KILL", "-f", "forgetty server run"]).output();
}

fn kill_children(r: &mut Real) {
    for c in [r.pairer.as_mut(), r.server.as_mut()].into_iter().flatten() {
        let _ = c.kill();
        let _ = c.wait();
    }
    r.pairer = None;
    r.server = None;
    if !r.distro.is_empty() {
        let _ = wsl(&r.distro, &["pkill", "-TERM", "-f", "forgetty server run"]).output();
    }
}

impl ShellSmoke {
    pub(super) fn real_kill(&mut self) {
        kill_children(&mut self.real);
    }

    fn real_start_server(&mut self) {
        let d = self.real.distro.clone();
        let mut c = wsl(&d, &[BIN, "server", "run", "--port", &self.real.port.to_string()]);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match c.spawn() {
            Ok(mut ch) => {
                collect(&mut ch, &self.real.server_log);
                self.real.server = Some(ch);
            }
            Err(e) => fail(&format!("cannot start wsl.exe: {e}")),
        }
    }

    fn real_server_up(&self) -> bool {
        wsl(&self.real.distro, &[BIN, "server", "status", "--json"])
            .stdin(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// True once no byte reached `pane` for `quiet`.
    fn real_quiet(&mut self, shell: &AppShell, cx: &gpui::App, pane: PaneId, quiet: Duration) -> bool {
        let fed = shell.views.get(&pane).map(|v| v.read(cx).fed_bytes).unwrap_or(0);
        if fed != self.real.fed_last || self.real.fed_t.is_none() {
            self.real.fed_last = fed;
            self.real.fed_t = Some(Instant::now());
            return false;
        }
        self.real.fed_t.is_some_and(|t| t.elapsed() >= quiet) && fed > 0
    }

    /// Extra lines for the stuck-phase dump: server and pair output, and what `status` says.
    pub(super) fn real_diag(&self) -> String {
        if self.real.distro.is_empty() {
            return String::new();
        }
        let mut o = format!("server log: {:?}\n", self.real.server_log.lock().unwrap().iter().rev().take(30).collect::<Vec<_>>());
        let st = wsl(&self.real.distro, &[BIN, "server", "status", "--json"]).stdin(Stdio::null()).output();
        match st {
            Ok(out) => o.push_str(&format!(
                "status: {:?} out={} err={}\n",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => o.push_str(&format!("status: cannot run: {e}\n")),
        }
        o
    }

    fn real_dump(&self, shell: &AppShell, cx: &gpui::App) {
        let mut o = String::new();
        for p in &self.real.panes {
            if let Some(s) = snap(shell, cx, *p) {
                o.push_str(&format!("pane {p:?} cursor=({},{})\n", s.cr, s.cc));
                for r in s.rows.iter().filter(|r| !r.trim().is_empty()).take(8) {
                    o.push_str(&format!("  | {}\n", r.trim_end()));
                }
            }
        }
        o.push_str(&format!("server log: {:?}\n", self.real.server_log.lock().unwrap().iter().rev().take(12).collect::<Vec<_>>()));
        *SCREEN_DUMP.lock().unwrap() = o;
    }

    pub(super) fn real_step(&mut self, shell: &mut AppShell, _window: &mut Window, cx: &mut Context<AppShell>, now_t: Instant) {
        // A burst of frames while a phase waits for the network: show why it stuck.
        if self.phase_t0.elapsed() > Duration::from_secs(200) {
            self.real_dump(shell, cx);
        }
        match self.phase {
            // ---- server, pairing window, pairing -------------------------------------------
            Phase::RealSetup => match self.sub {
                0 => {
                    self.real_start_server();
                    self.sub = 1;
                }
                1 => {
                    // `status` takes a moment: poll once a second.
                    if self.real.last_probe.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                        return;
                    }
                    self.real.last_probe = Some(Instant::now());
                    // The runner can hold the port back (Windows reserves port ranges): the
                    // server exits at once, so try the next port.
                    let exited = self.real.server.as_mut().is_some_and(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(false));
                    if exited && self.real.restarts < 8 {
                        self.real.restarts += 1;
                        self.real.port += 13;
                        record("real_port_retries", json!(self.real.restarts));
                        self.real_start_server();
                        return;
                    }
                    if !self.real_server_up() {
                        return;
                    }
                    self.real.restarts = 0;
                    record("real_server_start_ms", json!(ms(self.phase_t0.elapsed())));
                    let d = self.real.distro.clone();
                    let mut c = wsl(&d, &[BIN, "pair", "--ttl", "900"]);
                    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                    match c.spawn() {
                        Ok(mut ch) => {
                            collect(&mut ch, &self.real.pair_log);
                            self.real.pairer = Some(ch);
                        }
                        Err(e) => fail(&format!("cannot run forgetty pair: {e}")),
                    }
                    self.sub = 2;
                }
                _ => {
                    let payload = self.real.pair_log.lock().unwrap().iter().find_map(|l| l.strip_prefix("payload: ").map(str::to_string));
                    let Some(payload) = payload else { return };
                    self.real.t_pair = Some(Instant::now());
                    shell.pair_hint = format!("127.0.0.1:{}", self.real.port).parse().ok();
                    shell.pair_submit_text(&payload, cx);
                    shell.pair_confirm(cx);
                    self.goto(Phase::RealPair);
                }
            },
            Phase::RealPair => {
                if shell.machines.len() == 2
                    && shell.machine.kind == MachineKind::Remote
                    && !self.real.made_tabs
                    && shell.pair.is_none()
                    && matches!(shell.active_conn(), Some(ConnState::Connected))
                    && self.real.t_pair.is_some_and(|t| t.elapsed() > Duration::from_millis(2500))
                {
                    // A fresh server has at most its default tab: top up to two.
                    self.real.made_tabs = true;
                    for _ in shell.layout.tab_count()..2 {
                        shell.new_tab(cx);
                    }
                }
                let up = shell.machines.len() == 2
                    && shell.machine.kind == MachineKind::Remote
                    && shell.layout.tab_count() >= 2
                    && shell.views.len() >= 2
                    && shell.pair.is_none();
                if !up {
                    return;
                }
                record("real_pair_to_panes_ms", json!(ms(self.real.t_pair.map(|t| t.elapsed()).unwrap_or_default())));
                self.real.panes = shell.layout.workspaces.iter().flat_map(|w| w.tabs.iter().map(|t| t.pane_tree.first_leaf())).take(2).collect();
                check("real_paired_and_two_panes", self.real.panes.len() == 2);
                check("real_sidebar_lists_machines", shell.machines.len() == 2 && shell.machines[1].kind == MachineKind::Remote);
                self.goto(Phase::RealEcho);
            }
            // ---- type into both panes ---------------------------------------------------------
            Phase::RealEcho => {
                let i = self.sub / 2;
                if i >= 2 {
                    let mut isolated = true;
                    for n in 0..2 {
                        let Some(s) = snap(shell, cx, self.real.panes[n]) else { fail("real pane view vanished") };
                        for m in 0..2 {
                            let has = s.find_exact(&format!("W4-ECHO-{m}")).is_some();
                            if has != (m == n) {
                                isolated = false;
                            }
                        }
                    }
                    check("real_echo_2_panes", true);
                    check("real_echo_isolated", isolated);
                    let resets = self.real.panes.iter().all(|p| shell.views.get(p).is_some_and(|v| v.read(cx).resets >= 1));
                    check("real_vt_reset_on_attach", resets);
                    self.goto(Phase::RealKeys);
                    return;
                }
                let p = self.real.panes[i];
                if self.sub % 2 == 0 {
                    // The shell prints its prompt; wait for output to stop.
                    if !self.real_quiet(shell, cx, p, Duration::from_millis(500)) {
                        return;
                    }
                    let Some(view) = shell.views.get(&p).cloned() else { return };
                    view.update(cx, |v, _| v.send_bytes(format!("echo W4-ECHO-{i}\r").as_bytes()));
                    self.sub += 1;
                    self.sub_t0 = Instant::now();
                } else if snap(shell, cx, p).is_some_and(|s| s.find_exact(&format!("W4-ECHO-{i}")).is_some()) {
                    self.sub += 1;
                    self.real.fed_t = None;
                }
            }
            // ---- key to frame over the real server ----------------------------------------------
            Phase::RealKeys => {
                let p0 = self.real.panes[0];
                if shell.active_tab().map(|t| t.pane_tree.contains(p0)) != Some(true) {
                    fail("real keys: the first pane is not in the active tab");
                }
                let Some(view) = shell.views.get(&p0).cloned() else { return };
                match self.sub {
                    0 => {
                        if !self.real_quiet(shell, cx, p0, Duration::from_millis(120)) {
                            return;
                        }
                        let (r, c) = view.read(cx).term.cursor();
                        let ch = self.next_key_char();
                        self.probe = Some(Probe { row: r, col: c, ch, t0: Instant::now() });
                        self.real.cursor = (r, c);
                        view.update(cx, |v, _| v.send_bytes(ch.to_string().as_bytes()));
                        self.sub = 1;
                        self.sub_t0 = Instant::now();
                    }
                    1 => {
                        let Some(pr) = &self.probe else { return };
                        let (done, reshaped) = {
                            let v = view.read(cx);
                            (v.term.screen().cell(pr.row, pr.col).grapheme.starts_with(pr.ch), v.rows_reshaped_last)
                        };
                        if done {
                            let pr = self.probe.take().unwrap();
                            self.lat_ms.push(ms(now_t - pr.t0));
                            self.max_reshaped_per_key = self.max_reshaped_per_key.max(reshaped);
                            // Erase the character: backspace.
                            view.update(cx, |v, _| v.send_bytes(&[0x7f]));
                            self.sub = 2;
                            self.sub_t0 = Instant::now();
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("real key probe: echo did not arrive");
                        }
                    }
                    _ => {
                        let cur = view.read(cx).term.cursor();
                        if cur == self.real.cursor {
                            if self.lat_ms.len() >= PROBES {
                                let (p50, _) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "_real");
                                check("real_key_p50_le_30ms", p50 <= 30.0);
                                self.goto(Phase::RealKill);
                            } else {
                                self.sub = 0;
                            }
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("real key probe: the erase did not arrive");
                        }
                    }
                }
            }
            // ---- kill the server, restart it ----------------------------------------------------
            Phase::RealKill => {
                let Some(view) = shell.views.get(&self.real.panes[0]).cloned() else { return };
                match self.sub {
                    0 => {
                        self.real.resets_before = view.read(cx).resets;
                        let d = self.real.distro.clone();
                        // Stop the server process. Its panes' shells go with it.
                        stop_server(&d);
                        if let Some(mut c) = self.real.server.take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        self.real.t_kill = Some(Instant::now());
                        self.sub = 1;
                    }
                    1 => {
                        if !matches!(shell.active_conn(), Some(ConnState::Reconnecting { .. })) {
                            if self.sub_t0.elapsed() > Duration::from_secs(30) {
                                fail("real: the link loss was never seen");
                            }
                            return;
                        }
                        record("real_link_loss_detected_ms", json!(ms(self.real.t_kill.map(|t| t.elapsed()).unwrap_or_default())));
                        view.update(cx, |v, _| v.send_bytes(b"OFFLINE-OK"));
                        self.sub = 2;
                        self.sub_t0 = Instant::now();
                    }
                    _ => {
                        if shell.overlay_text(Instant::now()).is_some() {
                            check("real_overlay_after_loss", true);
                            self.real_start_server();
                            self.real.t_ready = None;
                            self.goto(Phase::RealRestart);
                        } else if self.sub_t0.elapsed() > Duration::from_secs(20) {
                            fail("real: the reconnecting overlay never showed");
                        }
                    }
                }
            }
            Phase::RealRestart => {
                let Some(view) = shell.views.get(&self.real.panes[0]).cloned() else { return };
                match self.sub {
                    0 => {
                        if self.real.t_ready.is_none() {
                            if self.real.last_probe.is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
                                return;
                            }
                            self.real.last_probe = Some(Instant::now());
                            // A server that exited (the port was not free yet) is started again.
                            let exited = self.real.server.as_mut().is_some_and(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(false));
                            if exited && self.real.restarts < 5 {
                                self.real.restarts += 1;
                                self.real_start_server();
                                return;
                            }
                            if self.real_server_up() {
                                self.real.t_ready = Some(Instant::now());
                                record("real_server_restart_ms", json!(ms(self.phase_t0.elapsed())));
                            }
                            return;
                        }
                        if shell.active_conn() == Some(&ConnState::Connected) {
                            let after = self.real.t_ready.map(|t| t.elapsed()).unwrap_or_default();
                            record("real_reconnect_after_accept_ms", json!(ms(after)));
                            check("real_reconnected_after_restart", true);
                            self.sub = 1;
                            self.sub_t0 = Instant::now();
                        }
                    }
                    _ => {
                        // The typed text was buffered and goes to the restored pane's new shell.
                        if snap(shell, cx, self.real.panes[0]).is_some_and(|s| s.rows.iter().any(|r| r.contains("OFFLINE-OK"))) {
                            check("real_typing_buffered_across_restart", true);
                            // The restarted server reopens the saved byte log under the same pane id, so the
                            // client resumes from its offset and no reset is needed. Record what happened.
                            record("real_resets_after_restart", json!(view.read(cx).resets - self.real.resets_before));
                            // Clear the line, then the burst.
                            self.real.fed_t = None;
                            self.goto(Phase::RealBurst);
                        } else if self.sub_t0.elapsed() > Duration::from_secs(30) {
                            fail("real: buffered input did not arrive after the restart");
                        }
                    }
                }
            }
            // ---- 20 MB through one pane -----------------------------------------------------------
            Phase::RealBurst => {
                let p0 = self.real.panes[0];
                let Some(view) = shell.views.get(&p0).cloned() else { return };
                match self.sub {
                    0 => {
                        if !self.real_quiet(shell, cx, p0, Duration::from_millis(500)) {
                            return;
                        }
                        self.real.burst_fed0 = view.read(cx).fed_bytes;
                        self.real.burst_t0 = Some(Instant::now());
                        self.real.burst_last = None;
                        view.update(cx, |v, _| v.send_bytes(format!("\x15seq 1 {BURST_LINES}; echo BURST-DONE-$((40+2))\r").as_bytes()));
                        self.sub = 1;
                    }
                    _ => {
                        if let Some(last) = self.real.burst_last {
                            let gap = ms(now_t - last);
                            if gap > self.real.burst_max_gap {
                                self.real.burst_max_gap = gap;
                            }
                        }
                        self.real.burst_last = Some(now_t);
                        if snap(shell, cx, p0).is_some_and(|s| s.find_exact("BURST-DONE-42").is_some()) {
                            let secs = self.real.burst_t0.map(|t| t.elapsed().as_secs_f64()).unwrap_or(1.0);
                            let fed = view.read(cx).fed_bytes - self.real.burst_fed0;
                            record("real_burst_s", json!(secs));
                            record("real_burst_mb_per_s", json!((fed as f64 / 1e6 / secs * 10.0).round() / 10.0));
                            record("real_burst_fed_mb", json!(fed as f64 / 1e6));
                            record("real_burst_max_frame_gap_ms", json!(self.real.burst_max_gap));
                            check("real_burst_complete", fed as f64 >= BURST_BYTES * 0.99);
                            self.real.fed_t = None;
                            self.goto(Phase::RealFinal);
                        }
                    }
                }
            }
            // ---- the client's screen against `forgetty capture` ---------------------------------------
            Phase::RealFinal => {
                let p0 = self.real.panes[0];
                if !self.real_quiet(shell, cx, p0, Duration::from_millis(1500)) {
                    return;
                }
                let Some(s) = snap(shell, cx, p0) else { fail("real pane view vanished") };
                let mut mine: Vec<String> = s.rows.iter().map(|r| r.trim_end().to_string()).collect();
                while mine.last().is_some_and(|r| r.is_empty()) {
                    mine.pop();
                }
                let id = p0.0.to_string();
                let out = wsl(&self.real.distro, &[BIN, "capture", "-p", &id, "--trim"]).stdin(Stdio::null()).output();
                match out {
                    Ok(o) if o.status.success() => {
                        let text = String::from_utf8_lossy(&o.stdout).to_string();
                        let mut theirs: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
                        while theirs.last().is_some_and(|r| r.is_empty()) {
                            theirs.pop();
                        }
                        // The client grid can be taller than the server's pane: compare the last rows
                        // both have, and require at least 20 of them.
                        let n = mine.len().min(theirs.len());
                        record("real_screen_rows", json!([mine.len(), theirs.len()]));
                        let same = n >= 20 && mine[mine.len() - n..] == theirs[theirs.len() - n..];
                        check("real_final_screen_matches_capture", same);
                        if !same {
                            record("real_screen_client", json!(mine));
                            record("real_screen_capture", json!(theirs));
                        }
                    }
                    Ok(o) => {
                        record("real_capture_error", json!(String::from_utf8_lossy(&o.stderr).to_string()));
                        check("real_final_screen_matches_capture", false);
                    }
                    Err(e) => {
                        record("real_capture_error", json!(e.to_string()));
                        check("real_final_screen_matches_capture", false);
                    }
                }
                record("real_server_log_tail", json!(self.real.server_log.lock().unwrap().iter().rev().take(15).collect::<Vec<_>>()));
                kill_children(&mut self.real);
                self.finish(shell)
            }
            _ => {}
        }
    }
}
