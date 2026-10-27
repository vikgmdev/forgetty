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

fn recorded(key: &str) -> Option<f64> {
    RESULTS.lock().unwrap().as_ref().and_then(|m| m.get(key)).and_then(|v| v.as_f64())
}

impl ShellSmoke {
    pub(super) fn ls_step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, now_t: Instant) {
        match self.phase {
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
                let Some(p) = shell.active_pane() else { return };
                let Some(s) = snap(shell, cx, p) else { return };
                if s.rows.iter().any(|r| r.trim_end().ends_with('>')) {
                    check("local_server_tab_runs_cmd", true);
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
                            self.goto(Phase::LsDevices);
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
