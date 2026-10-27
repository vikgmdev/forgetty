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

use super::{check, fail, ms, record, snap, summarize_keys, Phase, Probe, ShellSmoke, RESULTS};
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
    fed_before: u64,
    pid: u64,
    t0: Option<Instant>,
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
                    fail(&format!("local server: fell back to in-process: {f:?}"));
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
                match self.sub {
                    0 | 2 => {
                        self.ls.fed_before = view.read(cx).fed_bytes;
                        view.update(cx, |v, _| v.send_bytes(b"\r\n"));
                        self.sub += 1;
                        self.sub_t0 = Instant::now();
                    }
                    1 | 3 => {
                        let (cc, fed) = {
                            let v = view.read(cx);
                            (v.term.cursor().1, v.fed_bytes)
                        };
                        if cc == 0 && fed > self.ls.fed_before && self.sub_t0.elapsed() > Duration::from_millis(100) {
                            if self.sub == 1 {
                                self.lat_ms.clear();
                                self.max_reshaped_per_key = 0;
                            }
                            let (r, c) = view.read(cx).term.cursor();
                            let ch = self.next_key_char();
                            self.probe = Some(Probe { row: r, col: c, ch, t0: Instant::now() });
                            view.update(cx, |v, _| v.send_bytes(ch.to_string().as_bytes()));
                            self.sub = 4;
                        } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                            fail("local server key probe: fresh line did not arrive");
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
                        self.max_reshaped_per_key = self.max_reshaped_per_key.max(reshaped);
                        if self.lat_ms.len() >= PROBES {
                            let (p50, _) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "_local_server");
                            let base = recorded("key_to_frame_p50_ms").unwrap_or(f64::MAX);
                            record("local_server_vs_in_process_ms", json!({"in_process_p50": base, "server_p50": p50, "delta": p50 - base}));
                            check("local_server_key_p50_within_2ms_of_in_process", p50 <= base + 2.0);
                            self.goto(Phase::LsDevices);
                        } else if self.lat_ms.len() % 20 == 0 {
                            self.ls.fed_before = view.read(cx).fed_bytes;
                            view.update(cx, |v, _| v.send_bytes(b"\r\n"));
                            self.sub = 3;
                            self.sub_t0 = Instant::now();
                        } else {
                            let (r, c) = view.read(cx).term.cursor();
                            let ch = self.next_key_char();
                            self.probe = Some(Probe { row: r, col: c, ch, t0: Instant::now() });
                            view.update(cx, |v, _| v.send_bytes(ch.to_string().as_bytes()));
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
                let stop = cli.run(&["server", "stop"], Duration::from_secs(30));
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
