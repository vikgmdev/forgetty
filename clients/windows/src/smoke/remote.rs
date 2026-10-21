//! W3 smoke phases: the client against a real `v2-test-server` process.
//!
//! The server runs as a separate process on the runner. The phases pair with
//! its payload through the code the dialog uses (`pair_submit_text`,
//! `pair_confirm`), open two remote panes, type and read the echo, measure
//! key-to-frame latency over localhost, kill and restart the server, and
//! drive the toast paths.

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use forgetty_remote::acl;
use forgetty_shell_core::{BackendEvent, ConnState, MachineKind, PaneStatus};

use super::*;
use crate::machines::PairStage;

const SECRET: &str = "TESTSECRET0000000000000000";
const PORT: u16 = 47297;
const PROBES: usize = 100;

pub struct ServerProc {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    stdin: Option<ChildStdin>,
}

impl ServerProc {
    fn start(exe: &Path, args: &[String]) -> std::io::Result<Self> {
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().expect("piped stdout");
        let l2 = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                l2.lock().unwrap().push(line);
            }
        });
        let stdin = child.stdin.take();
        Ok(ServerProc { child, lines, stdin })
    }

    fn ready(&self) -> bool {
        self.lines.lock().unwrap().iter().any(|l| l == "READY")
    }

    fn value(&self, key: &str) -> Option<String> {
        let p = format!("{key}=");
        self.lines.lock().unwrap().iter().find_map(|l| l.strip_prefix(&p).map(str::to_string))
    }

    fn send(&mut self, cmd: &str) {
        if let Some(i) = &mut self.stdin {
            let _ = writeln!(i, "{cmd}");
            let _ = i.flush();
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct W3 {
    server: Option<ServerProc>,
    exe: PathBuf,
    args: Vec<String>,
    node_id: String,
    local_id: Option<Uuid>,
    remote_id: Option<Uuid>,
    panes: Vec<PaneId>,
    t_start: Option<Instant>,
    t_kill: Option<Instant>,
    since: Option<Instant>,
    resets_before: u64,
    fed_before: u64,
    toasts_before: usize,
    target: String,
    toast_pane: Option<PaneId>,
}

fn kill_server(w3: &mut W3) {
    if let Some(s) = w3.server.as_mut() {
        s.kill();
    }
    w3.server = None;
}

impl ShellSmoke {
    pub(super) fn w3_kill_server(&mut self) {
        kill_server(&mut self.w3);
    }

    fn remote_view(&self, shell: &AppShell, i: usize) -> Option<gpui::Entity<TerminalView>> {
        shell.views.get(self.w3.panes.get(i)?).cloned()
    }

    pub(super) fn w3_step(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, now_t: Instant) {
        match self.phase {
            // ---- pair through the dialog's code path ---------------------------------
            Phase::RSetup => match self.sub {
                0 => {
                    // Pane padding: the grid starts 6 px right of and 4 px below the pane edge.
                    if let (Some((_, geo)), Some(p)) = (shell.active_geometry(), shell.active_pane()) {
                        if let (Some(r), Some(v)) = (geo.panes.iter().find(|(q, _)| *q == p).map(|x| x.1), shell.views.get(&p)) {
                            let v = v.read(cx);
                            let (ox, oy) = shell.content_origin();
                            let dx = f32::from(v.bounds.origin.x) - (ox + r.x);
                            let dy = f32::from(v.bounds.origin.y) - (oy + r.y);
                            let dw = r.w - f32::from(v.bounds.size.width);
                            record("pane_padding_px", json!({"left": dx, "top": dy, "width_loss": dw}));
                            let (px_, py_) = (v.cfg.pane_padding_x, v.cfg.pane_padding_y);
                            check(
                                "pane_padding",
                                (dx - px_).abs() < 0.6 && (dy - py_).abs() < 0.6 && (dw - 2.0 * px_).abs() < 0.6 && px_ == 6.0 && py_ == 4.0,
                            );
                        }
                    }
                    let exe = std::env::var_os("FORGETTY_TEST_SERVER").map(PathBuf::from).unwrap_or_else(|| {
                        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("v2-test-server.exe"))).unwrap_or_default()
                    });
                    let key = std::env::temp_dir().join(format!("forgetty-smoke-server-{}.key", std::process::id()));
                    let _ = std::fs::remove_file(&key);
                    self.w3.exe = exe;
                    self.w3.args = vec![
                        "--port".into(),
                        PORT.to_string(),
                        "--key-file".into(),
                        key.to_string_lossy().into_owned(),
                        "--secret".into(),
                        SECRET.into(),
                        "--machine".into(),
                        "ci-remote".into(),
                        "--panes".into(),
                        "2".into(),
                    ];
                    match ServerProc::start(&self.w3.exe, &self.w3.args) {
                        Ok(s) => self.w3.server = Some(s),
                        Err(e) => fail(&format!("cannot start {}: {e}", self.w3.exe.display())),
                    }
                    self.sub = 1;
                }
                _ => {
                    let Some(srv) = self.w3.server.as_ref() else { return };
                    if !srv.ready() {
                        return;
                    }
                    let (Some(node), Some(addr), Some(qr)) = (srv.value("NODE_ID"), srv.value("ADDR"), srv.value("QR")) else {
                        fail("test server printed READY without NODE_ID, ADDR and QR");
                    };
                    self.w3.node_id = node.clone();
                    self.w3.local_id = Some(shell.machine.id);
                    self.w3.t_start = Some(Instant::now());
                    record("test_server_addr", json!(addr));
                    let store_empty = shell.remote.as_ref().is_some_and(|r| r.store.machines().is_empty());

                    // Garbage is refused before any dialog.
                    shell.pair_submit_text("this is not a payload", cx);
                    check("pair_rejects_garbage", matches!(shell.pair.as_ref().map(|d| &d.stage), Some(PairStage::Failed(_))));

                    // The real payload: parse and show the offer. Nothing is paired yet.
                    shell.pair_hint = addr.parse().ok();
                    shell.pair_submit_text(&format!("Scan the QR code or paste this payload:\n{qr}\n"), cx);
                    let offer_ok = match shell.pair.as_ref().map(|d| &d.stage) {
                        Some(PairStage::Confirm(o)) => o.machine == "ci-remote" && o.node_short == node[..8],
                        _ => false,
                    };
                    check("pair_dialog_shows_offer", offer_ok);
                    let waits = store_empty
                        && shell.machines.len() == 1
                        && shell.remote.as_ref().is_some_and(|r| r.store.machines().is_empty());
                    check("pair_waits_for_confirm", waits);
                    shell.pair_confirm(cx);
                    self.goto(Phase::RPair);
                }
            },
            Phase::RPair => {
                let up = shell.machines.len() == 2
                    && shell.machine.kind == MachineKind::Remote
                    && shell.layout.tab_count() == 2
                    && shell.views.len() == 2
                    && shell.pair.is_none();
                if !up {
                    return;
                }
                record("pair_to_panes_ms", json!(ms(self.w3.t_start.map(|t| t.elapsed()).unwrap_or_default())));
                let names: Vec<String> = shell.machines.iter().map(|m| m.name.clone()).collect();
                check("sidebar_lists_machines", names.len() == 2 && names[1] == "ci-remote" && shell.machines[0].kind == MachineKind::Local);
                self.w3.remote_id = Some(shell.machine.id);
                // Panes in tab order: tab 0 = server pane 1, tab 1 = server pane 2.
                self.w3.panes = shell
                    .layout
                    .workspaces
                    .iter()
                    .flat_map(|w| w.tabs.iter().map(|t| t.pane_tree.first_leaf()))
                    .collect();
                check("remote_two_panes", self.w3.panes.len() == 2);

                let (stored, files_ok, sddl) = match shell.remote.as_ref() {
                    Some(r) => {
                        let st = &r.store;
                        let files = [st.identity_path(), st.client_id_path(), st.machines_path(), st.addr_cache_path()];
                        let ok = files.iter().all(|f| f.exists() && acl::is_user_only(f).unwrap_or(false));
                        let sddl = acl::describe(&files[0]).unwrap_or_else(|e| format!("error: {e}"));
                        (st.machines(), ok, sddl)
                    }
                    None => (Vec::new(), false, String::new()),
                };
                record("identity_acl", json!(sddl));
                check("machine_stored", stored.len() == 1 && stored[0].name == "ci-remote" && stored[0].node_id == self.w3.node_id);
                check("store_files_user_only", files_ok);
                self.goto(Phase::REcho);
            }
            // ---- type into both panes, read the echo ------------------------------------
            Phase::REcho => {
                let i = self.sub / 2;
                if i >= 2 {
                    let tok = |n: usize| format!("W3-ECHO-{n}");
                    let mut isolated = true;
                    for n in 0..2 {
                        let Some(s) = self.w3.panes.get(n).and_then(|p| snap(shell, cx, *p)) else { fail("remote pane view vanished") };
                        for m in 0..2 {
                            let has = s.rows.iter().any(|r| r.contains(&tok(m)));
                            if has != (m == n) {
                                isolated = false;
                            }
                        }
                    }
                    let resets = self.w3.panes.iter().all(|p| shell.views.get(p).is_some_and(|v| v.read(cx).resets >= 1));
                    check("remote_echo_2_panes", true);
                    check("remote_echo_isolated", isolated);
                    check("remote_vt_reset_on_attach", resets);
                    self.goto(Phase::RKeys);
                    return;
                }
                let p = self.w3.panes[i];
                let tok = format!("W3-ECHO-{i}");
                if self.sub % 2 == 0 {
                    // Make the pane visible and focused: its tab, then the pane.
                    if let Some((w, t)) = shell.layout.find_pane(p) {
                        let tab = shell.layout.workspaces[w].tabs[t].id;
                        if shell.active_tab().map(|x| x.id) != Some(tab) {
                            shell.focus_tab(tab, cx);
                            return;
                        }
                    }
                    shell.focus_pane(p, window, cx);
                    let Some(s) = snap(shell, cx, p) else { return };
                    if shell.active_pane() == Some(p) && s.rows.iter().any(|r| r.contains("forgetty v2 test server")) {
                        let Some(view) = shell.views.get(&p).cloned() else { return };
                        view.update(cx, |v, _| v.send_bytes(tok.as_bytes()));
                        self.sub += 1;
                    }
                } else if let Some(s) = snap(shell, cx, p) {
                    if s.rows.iter().any(|r| r.contains(&tok)) {
                        self.sub += 1;
                    }
                }
            }
            // ---- key-to-frame over localhost ------------------------------------------------
            Phase::RKeys => {
                let p0 = self.w3.panes[0];
                if shell.active_tab().map(|t| t.pane_tree.contains(p0)) != Some(true) {
                    if let Some((w, t)) = shell.layout.find_pane(p0) {
                        let tab = shell.layout.workspaces[w].tabs[t].id;
                        shell.focus_tab(tab, cx);
                    }
                    return;
                }
                shell.focus_pane(p0, window, cx);
                let Some(view) = shell.views.get(&p0).cloned() else { return };
                match self.sub {
                    // Start a fresh line so probes begin at column 0.
                    0 | 2 => {
                        if self.sub == 0 && shell.active_pane() != Some(p0) {
                            return;
                        }
                        self.w3.fed_before = view.read(cx).fed_bytes;
                        view.update(cx, |v, _| v.send_bytes(b"\r\n"));
                        self.sub += 1;
                        self.sub_t0 = Instant::now();
                    }
                    1 | 3 => {
                        let (cc, fed) = {
                            let v = view.read(cx);
                            (v.term.cursor().1, v.fed_bytes)
                        };
                        if cc == 0 && fed > self.w3.fed_before && self.sub_t0.elapsed() > Duration::from_millis(100) {
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
                            fail("remote key probe: fresh line did not arrive");
                        }
                    }
                    _ => {
                        if let Some(p) = &self.probe {
                            if p.t0.elapsed() > Duration::from_millis(3000) {
                                fail("remote key probe: echo did not arrive");
                            }
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
                            let (p50, _) = summarize_keys(&self.lat_ms, self.max_reshaped_per_key, "_remote");
                            check("key_p50_remote_le_25ms", p50 <= 25.0);
                            // The server saw the pane's size (the view's grid).
                            let v = view.read(cx);
                            let size = shell.meta.get(&p0).and_then(|m| m.size);
                            record("remote_pane_size", json!({"view": [v.rows, v.cols], "server": size}));
                            check("remote_resize_reaches_server", size == Some((v.rows as u16, v.cols as u16)));
                            self.w3.resets_before = v.resets;
                            self.goto(Phase::RKill);
                        } else if self.lat_ms.len() % 20 == 0 {
                            self.w3.fed_before = view.read(cx).fed_bytes;
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
            // ---- kill the server: overlay after 1 s, typing is buffered -------------------------
            Phase::RKill => {
                let Some(view) = self.remote_view(shell, 0) else { return };
                match self.sub {
                    0 => {
                        kill_server(&mut self.w3);
                        self.w3.t_kill = Some(Instant::now());
                        self.sub = 1;
                    }
                    1 => {
                        let since = match shell.active_conn() {
                            Some(ConnState::Reconnecting { since }) => *since,
                            _ => return,
                        };
                        self.w3.since = Some(since);
                        record("link_loss_detected_ms", json!(ms(self.w3.t_kill.map(|t| t.elapsed()).unwrap_or_default())));
                        check("no_overlay_before_1s", shell.overlay_text(Instant::now()).is_none());
                        // Typed while the link is down: the client buffers it.
                        view.update(cx, |v, _| v.send_bytes(b"OFFLINE-OK"));
                        self.sub = 2;
                    }
                    _ => {
                        let Some(since) = self.w3.since else { return };
                        if shell.overlay_text(Instant::now()).is_some() {
                            let d = since.elapsed();
                            record("overlay_after_ms", json!(ms(d)));
                            check("overlay_after_1s", d >= Duration::from_millis(1000) && d < Duration::from_millis(2500));
                            let label = shell.machines.iter().any(|m| m.kind == MachineKind::Remote && matches!(m.conn, ConnState::Reconnecting { .. }));
                            check("sidebar_shows_reconnecting", label);
                            // What the network and resume hooks do.
                            let pokes = shell.net_pokes;
                            shell.poke_network(cx);
                            check("network_poke_reaches_backends", shell.net_pokes == pokes + 1);
                            match ServerProc::start(&self.w3.exe, &self.w3.args) {
                                Ok(s) => self.w3.server = Some(s),
                                Err(e) => fail(&format!("cannot restart the test server: {e}")),
                            }
                            self.goto(Phase::RRestart);
                        } else if self.phase_t0.elapsed() > Duration::from_secs(20) {
                            fail("the reconnecting overlay never showed");
                        }
                    }
                }
            }
            // ---- restart: reconnect, replayed state, buffered input arrives ----------------------
            Phase::RRestart => {
                let Some(view) = self.remote_view(shell, 0) else { return };
                match self.sub {
                    0 => {
                        let up = self.w3.server.as_ref().is_some_and(|s| s.ready()) && shell.active_conn() == Some(&ConnState::Connected);
                        if up {
                            if let Some(since) = self.w3.since {
                                record("reconnect_ms", json!(ms(since.elapsed())));
                            }
                            check("reconnected_after_restart", true);
                            check("overlay_gone_after_reconnect", shell.overlay_text(Instant::now()).is_none());
                            self.sub = 1;
                            self.sub_t0 = Instant::now();
                        }
                    }
                    1 => {
                        let Some(s) = snap(shell, cx, self.w3.panes[0]) else { return };
                        if s.rows.iter().any(|r| r.contains("OFFLINE-OK")) {
                            check("typing_during_reconnect_buffered", true);
                            let resets = view.read(cx).resets;
                            check("vt_reset_after_restart", resets > self.w3.resets_before);
                            view.update(cx, |v, _| v.send_bytes(b"AFTER-RESTART"));
                            self.sub = 2;
                            self.sub_t0 = Instant::now();
                        }
                    }
                    _ => {
                        let Some(s) = snap(shell, cx, self.w3.panes[0]) else { return };
                        if s.rows.iter().any(|r| r.contains("OFFLINE-OKAFTER-RESTART")) {
                            check("input_continues_after_restart", true);
                            self.goto(Phase::RToast);
                        }
                    }
                }
            }
            // ---- toasts -----------------------------------------------------------------------
            Phase::RToast => self.w3_toast(shell, window, cx),
            _ => {}
        }
    }

    fn w3_toast(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        let remote = self.w3.remote_id.expect("remote machine id");
        let local = self.w3.local_id.expect("local machine id");
        let (pa, pb) = (self.w3.panes[0], self.w3.panes[1]);
        match self.sub {
            0 => {
                // The remote machine is off screen: a status from it must toast.
                shell.switch_machine(local, cx);
                self.w3.toasts_before = shell.toast_log.len();
                self.sub = 1;
            }
            1 => {
                if shell.machine.id != local {
                    return;
                }
                if let Some(s) = self.w3.server.as_mut() {
                    s.send("status waiting needs-input");
                }
                self.sub = 2;
                self.sub_t0 = Instant::now();
            }
            2 => {
                if shell.toast_log.len() <= self.w3.toasts_before {
                    if self.sub_t0.elapsed() > Duration::from_secs(10) {
                        check("toast_waiting_off_screen_machine", false);
                        record("toast_debug", json!({"log": shell.toast_log.len()}));
                        self.finish_w3(shell);
                    }
                    return;
                }
                let rec = shell.toast_log[self.w3.toasts_before].clone();
                record("toast_text", json!({"title": rec.text.title, "lines": rec.text.lines, "target": rec.target}));
                let ok = rec.machine == remote
                    && rec.text.lines.first().map(String::as_str) == Some("Waiting for input: needs-input")
                    && rec.text.lines.iter().any(|l| l == "on ci-remote");
                check("toast_waiting_off_screen_machine", ok);
                self.w3.target = rec.target.clone();
                self.w3.toast_pane = Some(rec.pane);
                // The click.
                shell.on_toast_click(&rec.target, window, cx);
                self.sub = 3;
                self.sub_t0 = Instant::now();
            }
            3 => {
                let want = self.w3.toast_pane;
                if shell.machine.id == remote && shell.active_pane() == want && want.is_some() {
                    check("toast_click_focuses_pane", true);
                    // A bad argument does nothing.
                    let before = shell.machine.id;
                    shell.on_toast_click("calc.exe", window, cx);
                    check("toast_click_ignores_foreign_args", shell.machine.id == before);
                    // The user is looking at this pane: no toast for it.
                    self.w3.toasts_before = shell.toast_log.len();
                    shell.machine_events(
                        remote,
                        vec![BackendEvent::StatusChanged { pane_id: want.unwrap(), status: PaneStatus::Failed, message: "x".into() }],
                        cx,
                    );
                    self.sub = 4;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_secs(5) {
                    check("toast_click_focuses_pane", false);
                    record("toast_click_debug", json!({"machine_ok": shell.machine.id == remote, "pane": shell.active_pane().map(|p| p.0.to_string())}));
                    self.finish_w3(shell);
                }
            }
            4 => {
                if self.sub_t0.elapsed() < Duration::from_millis(500) {
                    return;
                }
                check("no_toast_when_focused", shell.toast_log.len() == self.w3.toasts_before);
                // The window is in the background: the focused pane's status toasts. Use the
                // other pane so the per-pane rate limit stays out of the way.
                let other = if Some(pb) == self.w3.toast_pane { pa } else { pb };
                shell.window_active = false;
                self.w3.toasts_before = shell.toast_log.len();
                shell.machine_events(
                    remote,
                    vec![BackendEvent::StatusChanged { pane_id: other, status: PaneStatus::Done, message: "exit 0".into() }],
                    cx,
                );
                shell.window_active = true;
                self.w3.toast_pane = Some(other);
                self.sub = 5;
                self.sub_t0 = Instant::now();
            }
            5 => {
                // `sync` may have reset `window_active`; the decision was made in the call above.
                let n = shell.toast_log.len() - self.w3.toasts_before.min(shell.toast_log.len());
                let got = shell.toast_log.last().map(|r| r.text.lines.first().cloned().unwrap_or_default());
                check("toast_done_window_in_background", n == 1 && got.as_deref() == Some("Finished: exit 0"));
                self.sub = 6;
                self.sub_t0 = Instant::now();
            }
            6 => {
                // The per-pane limit is 3 s: wait it out, then a program notification.
                if self.sub_t0.elapsed() < Duration::from_millis(3200) {
                    return;
                }
                let other = self.w3.toast_pane.unwrap();
                self.w3.toasts_before = shell.toast_log.len();
                shell.machine_events(
                    remote,
                    vec![BackendEvent::Notification { pane_id: other, title: "build\u{1b}]0;x\u{7}".into(), body: "all\r\ngreen".into() }],
                    cx,
                );
                self.sub = 7;
            }
            7 => {
                let n = shell.toast_log.len() - self.w3.toasts_before.min(shell.toast_log.len());
                let rec = shell.toast_log.last().cloned();
                let ok = n == 1
                    && rec.as_ref().is_some_and(|r| !r.text.title.contains('\u{1b}') && !r.text.lines.iter().any(|l| l.contains('\n') || l.contains('\u{1b}')))
                    && rec.as_ref().is_some_and(|r| r.text.lines.first().map(String::as_str) == Some("all green"));
                check("toast_notification_sanitized", ok);
                // Right away again: rate limited.
                self.w3.toasts_before = shell.toast_log.len();
                let other = self.w3.toast_pane.unwrap();
                shell.machine_events(
                    remote,
                    vec![BackendEvent::Notification { pane_id: other, title: "again".into(), body: "".into() }],
                    cx,
                );
                check("toast_rate_limited", shell.toast_log.len() == self.w3.toasts_before);
                self.sub = 8;
                self.sub_t0 = Instant::now();
            }
            _ => {
                // The OS side: the toast thread handled the requests, the hooks are registered.
                if self.sub_t0.elapsed() < Duration::from_millis(1500) {
                    return;
                }
                if let Some(h) = &shell.hooks {
                    let st = &h.stats;
                    let (shown, failed) = (st.toasts_shown.load(Ordering::SeqCst), st.toasts_failed.load(Ordering::SeqCst));
                    record(
                        "windows_hooks",
                        json!({
                            "net_registered": st.net_registered.load(Ordering::SeqCst),
                            "net_callbacks": st.net_callbacks.load(Ordering::SeqCst),
                            "power_registered": st.power_registered.load(Ordering::SeqCst),
                            "toasts_shown": shown,
                            "toasts_failed": failed,
                            "last_error": st.last_error.lock().map(|e| e.clone()).unwrap_or_default(),
                        }),
                    );
                    check("toast_requests_processed", shown + failed >= shell.toast_log.len() as u64);
                    check("net_change_hook_registered", st.net_registered.load(Ordering::SeqCst) && st.net_callbacks.load(Ordering::SeqCst) >= 1);
                    check("power_resume_hook_registered", st.power_registered.load(Ordering::SeqCst));
                } else {
                    check("toast_requests_processed", false);
                }
                self.finish_w3(shell);
            }
        }
    }

    fn finish_w3(&mut self, shell: &mut AppShell) -> ! {
        kill_server(&mut self.w3);
        self.finish(shell)
    }
}
