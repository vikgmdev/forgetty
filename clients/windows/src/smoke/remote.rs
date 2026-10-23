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
use gpui::AppContext as _;
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
        let mut cmd = Command::new(exe);
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // The app is GUI-subsystem: without this flag the server opens a console window.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn()?;
        super::SERVER_PID.store(child.id(), std::sync::atomic::Ordering::SeqCst);
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

/// A `PaneSource` that records what the view writes: `(auto, bytes)`.
struct Spy {
    rx: async_channel::Receiver<PaneEvent>,
    log: Arc<Mutex<Vec<(bool, Vec<u8>)>>>,
}

impl forgetty_shell_core::PaneSource for Spy {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.rx.clone()
    }
    fn write_input(&self, bytes: &[u8]) {
        self.log.lock().unwrap().push((false, bytes.to_vec()));
    }
    fn write_auto(&self, bytes: &[u8]) {
        self.log.lock().unwrap().push((true, bytes.to_vec()));
    }
    fn resize(&self, _rows: u16, _cols: u16) {}
    fn shutdown(&self) {}
}

#[derive(Default)]
pub struct W5 {
    spy: Option<gpui::Entity<TerminalView>>,
    tx: Option<async_channel::Sender<PaneEvent>>,
    log: Arc<Mutex<Vec<(bool, Vec<u8>)>>>,
    win: (usize, usize),
    small: (usize, usize),
    case_i: usize,
    target: Option<gpui::Entity<TerminalView>>,
}

use super::inject::{self, Step};

struct KeyCase {
    name: &'static str,
    klid: &'static str,
    steps: &'static [Step],
    want: &'static str,
}

/// Dead keys compose through WM_CHAR, and AltGr gives the layout's own character. Keyboard
/// layout ids (KLID): 0409 US, 20409 US-International, 40C French, 40A Spanish, 407 German.
const KEY_CASES: &[KeyCase] = &[
    KeyCase { name: "intl_acute_e", klid: "00020409", steps: &[Step::Tap(inject::VK_OEM_7), Step::Tap(inject::VK_E)], want: "\u{e9}" },
    KeyCase { name: "intl_acute_space", klid: "00020409", steps: &[Step::Tap(inject::VK_OEM_7), Step::Tap(inject::VK_SPACE)], want: "'" },
    KeyCase { name: "fr_circumflex_e", klid: "0000040C", steps: &[Step::Tap(inject::VK_OEM_6), Step::Tap(inject::VK_E)], want: "\u{ea}" },
    KeyCase { name: "es_acute_a", klid: "0000040A", steps: &[Step::Tap(inject::VK_OEM_7), Step::Tap(inject::VK_A)], want: "\u{e1}" },
    KeyCase { name: "de_acute_a", klid: "00000407", steps: &[Step::Tap(inject::VK_OEM_6), Step::Tap(inject::VK_A)], want: "\u{e1}" },
    KeyCase { name: "de_altgr_q", klid: "00000407", steps: &[Step::AltGr(inject::VK_Q)], want: "@" },
    KeyCase { name: "es_altgr_2", klid: "0000040A", steps: &[Step::AltGr(inject::VK_2)], want: "@" },
];

#[derive(Default)]
pub struct W3 {
    server: Option<ServerProc>,
    exe: PathBuf,
    args: Vec<String>,
    node_id: String,
    /// The payload of `forgetty pair` as the test server printed it.
    qr_text: String,
    local_id: Option<Uuid>,
    remote_id: Option<Uuid>,
    panes: Vec<PaneId>,
    t_start: Option<Instant>,
    t_kill: Option<Instant>,
    t_spawn: Option<Instant>,
    t_ready: Option<Instant>,
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
                    self.w3.qr_text = qr.clone();
                    self.w3.local_id = Some(shell.machine.id);
                    self.w3.t_start = Some(Instant::now());
                    record("test_server_addr", json!(addr));
                    let store_empty = shell.remote.as_ref().is_some_and(|r| r.store.machines().is_empty());

                    // Garbage is refused before any dialog.
                    shell.pair_submit_text("this is not a payload", cx);
                    check("pair_rejects_garbage", matches!(shell.pair.as_ref().map(|d| &d.stage), Some(PairStage::Failed(_))));

                    // The clipboard watch: the whole output of `forgetty pair` (QR block, payload line,
                    // status lines) is taken from the clipboard while the dialog waits for text.
                    if let Some(d) = shell.pair.as_mut() {
                        d.stage = PairStage::Input;
                    }
                    let block: String = (0..30).map(|_| "\u{2588}\u{2580}\u{2584} ".repeat(30) + "\n").collect();
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(format!(
                        "{block}\nUse {{the camera}} or paste:\npayload: {qr}\nwaiting for pairing...\n"
                    )));
                    shell.pair_clip_seen = None;
                    let more = shell.pair_clipboard_tick(cx);
                    // The watcher only offers it: the dialog stays at the input step.
                    let offered = matches!(shell.pair.as_ref(), Some(d) if matches!(d.stage, PairStage::Input) && d.prefill.as_ref().is_some_and(|(_, o)| o.machine == "ci-remote"));
                    check("pair_clipboard_watch_offers_pair_output", more && offered);
                    // The user presses Pair: now the confirm step shows, with the whole node id.
                    shell.pair_use_prefill(cx);
                    let taken = matches!(shell.pair.as_ref().map(|d| &d.stage), Some(PairStage::Confirm(o)) if o.machine == "ci-remote"
                        && o.node_grouped.replace(' ', "") == node && o.node_grouped.replace(' ', "").len() >= 16);
                    check("pair_prefill_pair_button_goes_to_confirm", taken);

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
                    // A pane in the active tab is focused first. A pane in another tab
                    // gets its input straight into the view: the test server does not
                    // switch tabs, a real server does.
                    let in_active = shell.active_tab().is_some_and(|t| t.pane_tree.contains(p));
                    if in_active {
                        shell.focus_pane(p, window, cx);
                    }
                    let Some(s) = snap(shell, cx, p) else { return };
                    if (!in_active || shell.active_pane() == Some(p)) && s.rows.iter().any(|r| r.contains("forgetty v2 test server")) {
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
                    fail("remote key probe: the first remote pane is not in the active tab");
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
                            self.w3.t_spawn = Some(Instant::now());
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
                        let ready = self.w3.server.as_ref().is_some_and(|s| s.ready());
                        if ready && self.w3.t_ready.is_none() {
                            // READY is printed after the server bound its port and started accepting.
                            self.w3.t_ready = Some(Instant::now());
                            record("server_restart_ready_ms", json!(ms(self.w3.t_spawn.map(|t| t.elapsed()).unwrap_or_default())));
                        }
                        let up = ready && shell.active_conn() == Some(&ConnState::Connected);
                        if up {
                            let after = self.w3.t_ready.map(|t| t.elapsed()).unwrap_or_default();
                            record("reconnect_after_accept_ms", json!(ms(after)));
                            // Target 1 s (the client aborts a dial in flight on a network change and races
                            // attempts). Measured at frame granularity.
                            check("reconnect_under_1s_after_accept", after < Duration::from_millis(1000));
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
                        // A new server run (`boot_id`) drops what was typed while the link was down: the
                        // programs it was for are gone. The pane says so, without a modal.
                        let notice = view.read(cx).notice_text();
                        if let Some(n) = &notice {
                            record("restart_notice_seen", json!(n));
                        }
                        // N counts every unacked byte (the 10 typed offline, and any key not acked yet).
                        if super::input_lost_bytes(notice.as_deref()).is_some_and(|n| n >= 10) {
                            record("input_lost_notice_restart", json!(notice));
                            check("input_lost_notice_after_restart", true);
                            let typed_again = snap(shell, cx, self.w3.panes[0]).is_some_and(|s| s.rows.iter().any(|r| r.contains("OFFLINE-OK")));
                            check("typing_during_restart_not_replayed", !typed_again);
                            let resets = view.read(cx).resets;
                            check("vt_reset_after_restart", resets > self.w3.resets_before);
                            view.update(cx, |v, _| v.send_bytes(b"AFTER-RESTART"));
                            self.sub = 2;
                            self.sub_t0 = Instant::now();
                        }
                    }
                    _ => {
                        let Some(s) = snap(shell, cx, self.w3.panes[0]) else { return };
                        if s.rows.iter().any(|r| r.contains("AFTER-RESTART")) && !s.rows.iter().any(|r| r.contains("OFFLINE-OK")) {
                            check("input_continues_after_restart", true);
                            self.goto(Phase::RToast);
                        }
                    }
                }
            }
            // ---- toasts -----------------------------------------------------------------------
            Phase::RToast => self.w3_toast(shell, window, cx),
            Phase::RServer => self.w3_server_ops(shell, window, cx),
            Phase::W5View => self.w5_view(shell, window, cx),
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
                        record(
                            "toast_debug",
                            json!({
                                "log": shell.toast_log.iter().map(|r| format!("{} {:?} {:?}", r.machine, r.pane, r.text.lines)).collect::<Vec<_>>(),
                                "before": self.w3.toasts_before,
                                "server_lines": self.w3.server.as_ref().map(|s| s.lines.lock().unwrap().clone()),
                                "remote_conn": shell.machines.iter().map(|m| format!("{} {:?}", m.name, m.backend.connection())).collect::<Vec<_>>(),
                                "active_machine": shell.machine.id.to_string(),
                                "window_active": shell.window_active,
                            }),
                        );
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
                // The Action Center: each toast the shell requested is held there. Windows adds a
                // toast to the history a moment after `Show`, so wait for it.
                let want: Vec<(String, String)> = shell.toast_log.iter().map(|r| (r.target.clone(), r.text.title.clone())).collect();
                let hist = crate::winsys::action_center();
                let missing: Vec<&(String, String)> = match &hist {
                    Ok(h) => want.iter().filter(|w| !h.contains(w)).collect(),
                    Err(_) => want.iter().collect(),
                };
                if !missing.is_empty() && self.sub_t0.elapsed() < Duration::from_secs(12) {
                    return;
                }
                record(
                    "action_center",
                    json!({
                        "requested": want.len(),
                        "in_history": hist.as_ref().map(|h| h.len()).unwrap_or(0),
                        "missing": missing.len(),
                        "error": hist.as_ref().err().cloned(),
                        "titles": hist.as_ref().map(|h| h.iter().map(|x| x.1.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                    }),
                );
                check("toast_reaches_action_center", !want.is_empty() && missing.is_empty());
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
                self.goto(Phase::RServer);
            }
        }
    }

    /// The server side of what the shell does: snapshot catch-up, the real tab
    /// switch, pane focus, split ratios and labels, each read back from the
    /// layout the server owns.
    fn w3_server_ops(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        let stuck = |this: &Self, what: &str| {
            if this.sub_t0.elapsed() > Duration::from_secs(15) {
                fail(&format!("remote ops stuck: {what}"));
            }
        };
        let Some(view) = self.remote_view(shell, 0) else { return };
        match self.sub {
            0 => {
                self.w3.resets_before = view.read(cx).resets;
                if let Some(srv) = self.w3.server.as_mut() {
                    srv.send("snapshot SNAP-OK");
                    srv.send("drop");
                }
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            1 => {
                stuck(self, "snapshot");
                let seen = snap(shell, cx, self.w3.panes[0]).is_some_and(|s| s.rows.iter().any(|r| r.contains("SNAP-OK")));
                if view.read(cx).resets > self.w3.resets_before && seen {
                    let top = view.update(cx, |v, _| {
                        v.scroll_rows(-1000);
                        let t = row_text(v.term.screen(), 0);
                        v.term.scroll_viewport_bottom();
                        t
                    });
                    record("remote_snapshot_top_of_scrollback", json!(top));
                    check("remote_snapshot_resets_vt", true);
                    check("remote_snapshot_marker_in_scrollback", top.contains("earlier output not loaded"));
                    self.sub = 2;
                    self.sub_t0 = Instant::now();
                }
            }
            // focus_tab: the server moves the active tab.
            2 => {
                stuck(self, "reconnect after snapshot");
                if shell.active_conn() != Some(&ConnState::Connected) {
                    return;
                }
                let tabs: Vec<Uuid> = shell.layout.workspaces.iter().flat_map(|w| w.tabs.iter().map(|t| t.id)).collect();
                if tabs.len() < 2 {
                    return;
                }
                shell.focus_tab(tabs[1], cx);
                self.sub = 3;
                self.sub_t0 = Instant::now();
            }
            3 => {
                stuck(self, "focus_tab");
                let tabs: Vec<Uuid> = shell.layout.workspaces.iter().flat_map(|w| w.tabs.iter().map(|t| t.id)).collect();
                if shell.active_tab().map(|t| t.id) == tabs.get(1).copied() {
                    check("remote_focus_tab_switches", true);
                    shell.focus_tab(tabs[0], cx);
                    self.sub = 4;
                    self.sub_t0 = Instant::now();
                }
            }
            4 => {
                stuck(self, "focus_tab back");
                let first = shell.layout.workspaces[0].tabs[0].id;
                if shell.active_tab().map(|t| t.id) == Some(first) {
                    shell.split(forgetty_shell_core::SplitDir::Horizontal, cx);
                    self.sub = 5;
                    self.sub_t0 = Instant::now();
                }
            }
            // set_active_pane: a split focuses the new pane, then focus the first one;
            // both are read back from the layout the server owns.
            5 => {
                stuck(self, "split");
                let Some(tab) = shell.active_tab() else { return };
                let leaves = tab.pane_tree.leaves();
                if leaves.len() == 2 && shell.views.contains_key(&leaves[1]) && tab.active_pane_id == Some(leaves[1]) {
                    check("remote_split_focuses_new_pane", true);
                    shell.focus_pane(leaves[0], window, cx);
                    self.sub = 6;
                    self.sub_t0 = Instant::now();
                }
            }
            6 => {
                stuck(self, "set_active_pane");
                let Some(tab) = shell.active_tab() else { return };
                let leaves = tab.pane_tree.leaves();
                if tab.active_pane_id == leaves.first().copied() {
                    check("remote_set_active_pane_roundtrip", true);
                    let fut = shell.backend.update_split_ratios(vec![forgetty_shell_core::SplitRatio { pane_id: leaves[0], ratio: 0.3 }]);
                    shell.call(cx, fut, |_, _, _| {});
                    self.sub = 7;
                    self.sub_t0 = Instant::now();
                }
            }
            7 => {
                stuck(self, "update_split_ratios");
                let Some(tab) = shell.active_tab() else { return };
                let r = match &tab.pane_tree {
                    forgetty_shell_core::PaneTree::Split { ratio, .. } => *ratio,
                    _ => 0.0,
                };
                if (r - 0.3).abs() < 1e-4 {
                    check("remote_split_ratio_roundtrip", true);
                    shell.begin_rename(tab.id, cx);
                    self.sub = 8;
                    self.sub_t0 = Instant::now();
                }
            }
            // Labels: the tab rename editor, a pane label and a workspace label.
            8 => {
                if shell.rename.is_none() {
                    return;
                }
                for _ in 0..64 {
                    shell.handle_key(&key("backspace"), window, cx);
                }
                for ch in "rlab".chars() {
                    shell.handle_key(&typed(ch), window, cx);
                }
                shell.handle_key(&key("enter"), window, cx);
                let (Some(tab), Some(pane)) = (shell.active_tab().map(|t| t.id), shell.active_pane()) else { return };
                let _ = tab;
                let f1 = shell.backend.set_label(forgetty_shell_core::LabelTarget::Pane, pane.0, "plab".into());
                shell.call(cx, f1, |_, _, _| {});
                let ws = shell.layout.workspaces[0].id;
                let f2 = shell.backend.set_label(forgetty_shell_core::LabelTarget::Workspace, ws, "wlab".into());
                shell.call(cx, f2, |_, _, _| {});
                self.sub = 9;
                self.sub_t0 = Instant::now();
            }
            9 => {
                stuck(self, "labels");
                let Some(tab) = shell.active_tab() else { return };
                let pane = shell.active_pane();
                let title = shell.tab_title(tab);
                let ok_tab = tab.label == "rlab" && title == "rlab";
                let ok_pane = pane.is_some_and(|p| tab.pane_label(p) == Some("plab"));
                let ok_ws = shell.layout.workspaces[0].name == "wlab";
                if ok_tab && ok_pane && ok_ws {
                    check("remote_labels_roundtrip", true);
                    self.sub = 10;
                    self.sub_t0 = Instant::now();
                }
            }
            // "Check for updates" never runs by itself, and a click ends with an answer.
            10 => {
                check("update_check_idle_until_asked", shell.update == forgetty_shell_core::update::UpdateState::Idle);
                shell.check_for_updates(cx);
                self.sub = 11;
                self.sub_t0 = Instant::now();
            }
            11 => {
                if shell.update == forgetty_shell_core::update::UpdateState::Checking {
                    stuck(self, "update check");
                    return;
                }
                record("update_check_result", json!(format!("{:?}", shell.update)));
                check("update_check_finishes", true);
                self.goto(Phase::W5View);
            }
            _ => {}
        }
    }

    fn finish_w3(&mut self, shell: &mut AppShell) -> ! {
        kill_server(&mut self.w3);
        self.finish(shell)
    }
}

/// Text service profiles of the Microsoft IMEs: (language, CLSID, profile GUID).
/// (language, CLSID, profile GUID, keyboard layout id of the IME).
const IME_JA: (u16, &str, &str, &str) = (0x0411, "03B5835F-F03C-411B-9CE2-AA23E1171E36", "A76C93D9-5523-4E90-AAFA-4DB112F9AC76", "E0010411");
const IME_ZH: (u16, &str, &str, &str) = (0x0804, "81D4E9C9-1D3B-41BC-9E6C-4B40BF79E35E", "FA550B04-5AD7-411F-A5AC-CA038EC515D7", "E00E0804");

impl ShellSmoke {
    /// Real Japanese and Chinese IME through the window's message loop: switch to the text
    /// service, turn it on, type romaji, read the preedit the IME pushed into the view, ask for
    /// candidates, commit, and read the bytes the pane got. The IME is slow and its dictionary is
    /// the runner's, so each step waits for its condition (up to 6 s) instead of a fixed delay.
    /// Steps 80 to 89 of the W5 view phase.
    fn w7_real_ime(&mut self, window: &mut Window, cx: &mut Context<AppShell>) {
        let Some(t) = self.w5.target.clone() else { fail("real ime: no target view") };
        let lap = |this: &Self, ms: u64| this.sub_t0.elapsed() >= Duration::from_millis(ms);
        let hwnd = inject::hwnd(window);
        let state = |cx: &mut Context<AppShell>| -> ImeState {
            t.update(cx, |v, _| ImeState {
                marked: v.marked.clone(),
                log: v.ime_log.clone(),
                bounds_calls: v.ime_bounds_calls,
                bounds_last: v.ime_bounds_last,
                cursor: v.term.cursor(),
                input: String::from_utf8_lossy(v.capture_input.as_deref().unwrap_or_default()).to_string(),
                cell: (f32::from(v.metrics.cell_w), f32::from(v.metrics.line_h)),
            })
        };
        let begin = |name: &str, prof: (u16, &str, &str, &str), cx: &mut Context<AppShell>| {
            t.update(cx, |v, _| {
                v.capture_input = Some(Vec::new());
                v.ime_log.clear();
                v.marked = None;
            });
            // The IME's own keyboard layout handle first (what the language bar switches to),
            // then the text service profile.
            let hkl = inject::activate_layout(prof.3);
            let act = inject::activate_text_service(prof.0, prof.1, prof.2);
            record(&format!("ime_real_{name}_activate"), json!({"layout": hkl.map(|h| format!("{h:x}")), "profile": act.clone().map(|_| "ok".to_string())}));
        };
        // The layout switch lands through the message loop: wait until the thread has it.
        let on_layout = |want: u16| (inject::current_hkl() & 0xffff) as u16 == want;
        let timeout = |this: &Self| this.sub_t0.elapsed() > Duration::from_secs(6);
        let rec = |name: &str, st: &ImeState| {
            record(
                name,
                json!({"marked": st.marked, "log": st.log, "bounds_calls": st.bounds_calls, "bounds_last": st.bounds_last.map(|b| vec![b.0 as f32, b.1 as f32, b.2, b.3, b.4, b.5, b.6, b.7]),
                       "cursor": [st.cursor.0, st.cursor.1], "input": st.input}),
            )
        };
        let non_ascii = |s: &str| !s.is_empty() && s.chars().all(|c| (c as u32) > 0x7f);
        match self.sub {
            80 => {
                begin("ja", IME_JA, cx);
                self.sub = 81;
                self.sub_t0 = Instant::now();
            }
            81 => {
                if !lap(self, 500) || (!on_layout(IME_JA.0) && !timeout(self)) {
                    return;
                }
                check("ime_real_ja_layout_active", on_layout(IME_JA.0));
                let imm = hwnd.map(inject::ime_open_native).unwrap_or_else(|| "no window handle".into());
                let sent = inject::tap_word(inject::current_hkl(), "nihongo");
                record("ime_real_ja_typed", json!({"sent": sent, "imm": imm, "hkl": format!("{:x}", inject::current_hkl())}));
                self.sub = 82;
                self.sub_t0 = Instant::now();
            }
            82 => {
                let st = state(cx);
                if st.marked.as_deref() != Some("\u{306b}\u{307b}\u{3093}\u{3054}") && !timeout(self) {
                    return;
                }
                rec("ime_real_ja_composing", &st);
                check("ime_real_ja_preedit_in_view", st.marked.as_deref() == Some("\u{306b}\u{307b}\u{3093}\u{3054}"));
                // Nothing reaches the pane while composing.
                check("ime_real_ja_preedit_sends_nothing", st.input.is_empty());
                // Where the IME put its candidate window: the cursor cell of the pane.
                let at_cell = st.bounds_last.is_some_and(|(r, c, x, y, _, h, ex, ey)| {
                    (r, c) == st.cursor
                        && (x - (ex + st.cell.0 * c as f32)).abs() < 1.5
                        && (y - (ey + st.cell.1 * r as f32)).abs() < 1.5
                        && (h - st.cell.1).abs() < 1.5
                });
                check("ime_real_ja_candidate_window_at_cursor_cell", st.bounds_calls > 0 && at_cell);
                // Space asks for candidates: the preedit becomes the first conversion.
                inject::send_keys(inject::current_hkl(), &[Step::Tap(inject::VK_SPACE)]);
                self.sub = 83;
                self.sub_t0 = Instant::now();
            }
            83 => {
                let st = state(cx);
                let converted = st.marked.as_deref().is_some_and(|m| m != "\u{306b}\u{307b}\u{3093}\u{3054}" && m.chars().any(|c| (c as u32) >= 0x4e00));
                if !converted && !timeout(self) {
                    return;
                }
                rec("ime_real_ja_candidates", &st);
                check("ime_real_ja_conversion_shows_kanji", converted);
                inject::send_keys(inject::current_hkl(), &[Step::Tap(inject::VK_RETURN)]);
                self.sub = 84;
                self.sub_t0 = Instant::now();
            }
            84 => {
                let st = state(cx);
                if st.input.is_empty() && !timeout(self) {
                    return;
                }
                rec("ime_real_ja_committed", &st);
                let commits = st.log.iter().filter(|l| l.starts_with("commit:")).count();
                check("ime_real_ja_commit_sends_cjk_once", non_ascii(&st.input) && commits == 1);
                check("ime_real_ja_commit_clears_preedit", st.marked.is_none());
                begin("zh", IME_ZH, cx);
                self.sub = 85;
                self.sub_t0 = Instant::now();
            }
            85 => {
                if !lap(self, 500) || (!on_layout(IME_ZH.0) && !timeout(self)) {
                    return;
                }
                check("ime_real_zh_layout_active", on_layout(IME_ZH.0));
                let imm = hwnd.map(inject::ime_open_native).unwrap_or_else(|| "no window handle".into());
                let sent = inject::tap_word(inject::current_hkl(), "zhongwen");
                record("ime_real_zh_typed", json!({"sent": sent, "imm": imm, "hkl": format!("{:x}", inject::current_hkl())}));
                self.sub = 86;
                self.sub_t0 = Instant::now();
            }
            86 => {
                // Pinyin answers more slowly: wait until the preedit has the whole syllable run.
                let st = state(cx);
                let full = st.marked.as_deref().is_some_and(|m| m.replace(['\'', ' '], "") == "zhongwen");
                if !full && !timeout(self) {
                    return;
                }
                rec("ime_real_zh_composing", &st);
                check("ime_real_zh_preedit_in_view", st.marked.is_some() && !st.log.is_empty());
                check("ime_real_zh_preedit_sends_nothing", st.input.is_empty());
                check("ime_real_zh_candidate_window_asked", st.bounds_calls > 0);
                inject::send_keys(inject::current_hkl(), &[Step::Tap(inject::VK_SPACE)]);
                self.sub = 87;
                self.sub_t0 = Instant::now();
            }
            _ => {
                let st = state(cx);
                if st.input.is_empty() && !timeout(self) {
                    return;
                }
                rec("ime_real_zh_committed", &st);
                let commits = st.log.iter().filter(|l| l.starts_with("commit:")).count();
                check("ime_real_zh_commit_sends_cjk_once", non_ascii(&st.input) && commits == 1);
                t.update(cx, |v, _| v.capture_input = None);
                inject::activate_layout("00000409");
                self.sub = 20;
                self.sub_t0 = Instant::now();
            }
        }
    }
}

/// What the view and the IME handler hold at one moment.
struct ImeState {
    marked: Option<String>,
    log: Vec<String>,
    bounds_calls: u32,
    bounds_last: Option<(usize, usize, f32, f32, f32, f32, f32, f32)>,
    cursor: (usize, usize),
    input: String,
    cell: (f32, f32),
}

/// State of the OSC 52 checks.
pub struct W7 {
    /// A machine id of its own, so the checks never touch the policy of a real machine.
    machine: uuid::Uuid,
    auto_before: usize,
    denied_sent: u8,
    nonce: u64,
    dropped_before: u32,
}

impl Default for W7 {
    fn default() -> Self {
        W7 { machine: uuid::Uuid::new_v4(), auto_before: 0, denied_sent: 0, nonce: 0, dropped_before: 0 }
    }
}

// ---- W7: OSC 52 --------------------------------------------------------------------------

fn osc52_seq(data: &str) -> Vec<u8> {
    format!("\x1b]52;c;{data}\x07").into_bytes()
}

fn clip_text(cx: &mut Context<AppShell>) -> Option<String> {
    cx.read_from_clipboard().and_then(|i| i.text())
}

impl ShellSmoke {
    /// OSC 52 through a pane view: the policy per machine (off, ask, allow), the prompt that keeps
    /// its answer, no answer to a read, the 1 MiB cap, a paste-style clean and no effect from
    /// replayed output. Steps 50 to 61 of the W5 view phase; the spy view is the pane.
    fn w7_clipboard(&mut self, shell: &mut AppShell, cx: &mut Context<AppShell>) {
        use forgetty_shell_core::osc52::{encode_base64, Policies, Policy, MAX_BYTES};
        let Some(spy) = self.w5.spy.clone() else { fail("osc52 smoke needs the spy view") };
        let Some(tx) = self.w5.tx.clone() else { fail("osc52 smoke needs the spy channel") };
        let m = self.w7.machine;
        let seen = |cx: &mut Context<AppShell>| spy.read(cx).clip_seen;
        let send = |ev: PaneEvent| {
            let _ = tx.try_send(ev);
        };
        if self.sub_t0.elapsed() > Duration::from_secs(20) {
            fail(&format!("osc52 checks stuck at step {}", self.sub));
        }
        match self.sub {
            50 => {
                let id = m;
                cx.subscribe(&spy, move |s, _v, ev: &crate::view::ViewEvent, cx| s.on_view_event(id, ev, cx)).detach();
                check("osc52_default_policy_is_ask", shell.osc52.get(&m.to_string()) == Policy::Ask);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("SENTINEL".to_string()));
                self.w7.auto_before = self.w5.log.lock().unwrap().len();
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"first"))));
                self.sub = 51;
                self.sub_t0 = Instant::now();
            }
            // ask: a prompt, nothing copied yet. A second request while it shows is dropped.
            51 => {
                if seen(cx).writes < 1 {
                    return;
                }
                check("osc52_ask_does_not_copy_before_answer", clip_text(cx).as_deref() == Some("SENTINEL"));
                check("osc52_ask_shows_prompt", shell.clip_prompt.as_ref().is_some_and(|p| p.machine == m && p.text == "first"));
                self.w7.nonce = shell.clip_prompt.as_ref().map(|p| p.nonce).unwrap_or(0);
                self.w7.dropped_before = shell.clip_stats.dropped_while_prompt;
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"swap"))));
                self.sub = 52;
                self.sub_t0 = Instant::now();
            }
            // The swap attempt, a stale nonce and an early click do nothing. A real answer copies.
            52 => {
                if seen(cx).writes < 2 {
                    return;
                }
                let Some(p) = shell.clip_prompt.clone() else { fail("osc52: the prompt vanished") };
                check(
                    "osc52_prompt_not_swapped_by_later_request",
                    p.text == "first" && p.nonce == self.w7.nonce && shell.clip_stats.dropped_while_prompt == self.w7.dropped_before + 1,
                );
                let ignored_before = shell.clip_stats.answers_ignored;
                shell.clip_answer_at(true, p.nonce + 1, p.shown_at + Duration::from_millis(600), cx);
                let stale_ignored = shell.clip_prompt.is_some() && shell.clip_stats.answers_ignored == ignored_before + 1;
                check("osc52_answer_with_wrong_nonce_ignored", stale_ignored);
                shell.clip_answer_at(true, p.nonce, p.shown_at + Duration::from_millis(100), cx);
                let early_ignored = shell.clip_prompt.is_some() && shell.clip_stats.answers_ignored == ignored_before + 2;
                check("osc52_click_in_first_500ms_ignored", early_ignored && clip_text(cx).as_deref() == Some("SENTINEL"));
                shell.clip_answer_at(true, p.nonce, p.shown_at + Duration::from_millis(600), cx);
                check("osc52_allow_copies", clip_text(cx).as_deref() == Some("first"));
                check("osc52_answer_remembered", shell.osc52.get(&m.to_string()) == Policy::Allow && shell.clip_prompt.is_none());
                let on_disk = shell.remote.as_ref().map(|r| Policies::open(r.store.dir()).get(&m.to_string()));
                check("osc52_answer_persisted", on_disk == Some(Policy::Allow));
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"second"))));
                self.sub = 53;
                self.sub_t0 = Instant::now();
            }
            // allow: no prompt the second time. Then a pairing offer arrives through OSC 52
            // while the pair dialog is open.
            53 => {
                if seen(cx).writes < 3 {
                    return;
                }
                check("osc52_allow_needs_no_prompt", shell.clip_prompt.is_none() && clip_text(cx).as_deref() == Some("second"));
                shell.open_pair(cx);
                let offer = format!("Scan or paste:\npayload: {}\n", self.w3.qr_text);
                send(PaneEvent::Output(osc52_seq(&encode_base64(offer.as_bytes()))));
                self.sub = 54;
                self.sub_t0 = Instant::now();
            }
            54 => {
                // The dialog's own watcher ticks every 400 ms: give it time, then tick by hand.
                if seen(cx).writes < 4 || self.sub_t0.elapsed() < Duration::from_millis(900) {
                    return;
                }
                let on_clip = clip_text(cx).is_some_and(|t| t.contains("payload:"));
                shell.pair_clipboard_tick(cx);
                let (input, prefilled) = match shell.pair.as_ref() {
                    Some(d) => (matches!(d.stage, PairStage::Input), d.prefill.is_some()),
                    None => (false, false),
                };
                check("osc52_pairing_offer_reaches_clipboard", on_clip);
                check("osc52_offer_not_submitted_or_prefilled", input && !prefilled);
                // The user copies the same text themself: it is offered (prefilled), never submitted.
                let user_text = format!("copied by the user\npayload: {}\n", self.w3.qr_text);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(user_text));
                shell.pair_clipboard_tick(cx);
                let (input, prefilled) = match shell.pair.as_ref() {
                    Some(d) => (matches!(d.stage, PairStage::Input), d.prefill.as_ref().is_some_and(|(_, o)| o.machine == "ci-remote")),
                    None => (false, false),
                };
                check("pair_user_copy_is_offered_not_submitted", input && prefilled);
                shell.pair_cancel(cx);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("second".to_string()));
                shell.set_clip_policy(m, Policy::Off, None, cx);
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"third"))));
                self.sub = 55;
                self.sub_t0 = Instant::now();
            }
            // off: dropped.
            55 => {
                if seen(cx).writes < 5 {
                    return;
                }
                check("osc52_off_drops", clip_text(cx).as_deref() == Some("second") && shell.clip_prompt.is_none());
                shell.set_clip_policy(m, Policy::Allow, None, cx);
                // A read is never answered, and even `allow` does not make it a write.
                send(PaneEvent::Output(b"\x1b]52;c;?\x1b\\".to_vec()));
                self.sub = 56;
                self.sub_t0 = Instant::now();
            }
            56 => {
                if seen(cx).reads_denied < 1 {
                    return;
                }
                // Let any (wrong) reply reach the log before looking.
                if self.sub_t0.elapsed() < Duration::from_millis(400) {
                    return;
                }
                check("osc52_read_never_answered", self.w5.log.lock().unwrap().len() == self.w7.auto_before && clip_text(cx).as_deref() == Some("second"));
                // More than 1 MiB is dropped, exactly 1 MiB is not.
                send(PaneEvent::Output(osc52_seq(&encode_base64(&vec![b'a'; MAX_BYTES + 1]))));
                self.sub = 57;
                self.sub_t0 = Instant::now();
            }
            57 => {
                if seen(cx).too_large < 1 {
                    return;
                }
                check("osc52_over_1mib_dropped", clip_text(cx).as_deref() == Some("second"));
                send(PaneEvent::Output(osc52_seq(&encode_base64(&vec![b'b'; MAX_BYTES]))));
                self.sub = 58;
                self.sub_t0 = Instant::now();
            }
            58 => {
                if seen(cx).writes < 6 {
                    return;
                }
                check("osc52_exactly_1mib_copied", clip_text(cx).is_some_and(|t| t.len() == MAX_BYTES));
                // Cleaned like a paste: escapes and bells go, line breaks stay.
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"ls\x1b[31m\x07 -la\r\nok\x7f"))));
                self.sub = 59;
                self.sub_t0 = Instant::now();
            }
            59 => {
                if seen(cx).writes < 7 {
                    return;
                }
                check("osc52_text_sanitized_like_paste", clip_text(cx).as_deref() == Some("ls[31m -la\r\nok"));
                // A replay (after a reset) re-runs old output: it must not touch the clipboard.
                send(PaneEvent::Reset { rows: 10, cols: 40, exact_replay: false });
                send(PaneEvent::Output(osc52_seq(&encode_base64(b"from a replay"))));
                self.sub = 60;
                self.sub_t0 = Instant::now();
            }
            60 => {
                if seen(cx).replayed < 1 {
                    return;
                }
                check("osc52_replay_does_not_copy", clip_text(cx).as_deref() == Some("ls[31m -la\r\nok") && seen(cx).writes == 7);
                // Back to ask. The replay ends when the channel drains: then output is live again.
                shell.set_clip_policy(m, Policy::Ask, None, cx);
                self.sub = 61;
                self.sub_t0 = Instant::now();
            }
            _ => {
                if self.sub_t0.elapsed() < Duration::from_millis(500) {
                    return;
                }
                if self.w7.denied_sent == 0 {
                    self.w7.denied_sent = 1;
                    send(PaneEvent::Output(osc52_seq(&encode_base64(b"echo hi\n"))));
                    return;
                }
                if seen(cx).writes < 8 {
                    return;
                }
                // The prompt shows what the text will do: size, lines, and the Enter at the end.
                let pv = shell.clip_prompt.as_ref().map(|p| p.preview.clone());
                check(
                    "osc52_prompt_flags_trailing_enter",
                    pv.as_ref().is_some_and(|p| p.ends_with_enter && p.lines == 1 && p.bytes == 8 && p.head == vec!["echo hi\u{23ce}".to_string()]),
                );
                check("osc52_ask_prompts_again_after_reset_to_ask", shell.clip_prompt.as_ref().is_some_and(|p| p.text == "echo hi\n"));
                let p = shell.clip_prompt.clone().unwrap_or_else(|| fail("osc52: no prompt"));
                shell.clip_answer_at(false, p.nonce, p.shown_at + Duration::from_millis(600), cx);
                check("osc52_deny_remembered_as_off", shell.osc52.get(&m.to_string()) == Policy::Off && clip_text(cx).as_deref() == Some("ls[31m -la\r\nok"));
                let st = shell.clip_stats;
                record(
                    "osc52_stats",
                    json!({"copied": st.copied, "dropped_off": st.dropped_off, "prompted": st.prompted, "too_large": st.too_large,
                           "reads_denied": st.reads_denied, "dropped_while_prompt": st.dropped_while_prompt, "answers_ignored": st.answers_ignored}),
                );
                self.sub = 70;
                self.sub_t0 = Instant::now();
            }
        }
    }

    /// The IME handler (what TSF drives): preedit, commit, cancel, and the candidate window
    /// anchored on the cursor cell. This is the handler level. A real Japanese or Chinese IME is
    /// tried by the `ime-probe` step of the workflow. Steps 70 to 72 of the W5 view phase.
    fn w7_ime(&mut self, window: &mut Window, cx: &mut Context<AppShell>) {
        use gpui::EntityInputHandler as _;
        let Some(spy) = self.w5.spy.clone() else { fail("ime smoke needs the spy view") };
        let Some(tx) = self.w5.tx.clone() else { fail("ime smoke needs the spy channel") };
        match self.sub {
            70 => {
                // The cursor to row 3, column 5 (zero-based 2, 4).
                let _ = tx.try_send(PaneEvent::Output(b"\x1b[2J\x1b[3;5H".to_vec()));
                self.sub = 71;
                self.sub_t0 = Instant::now();
            }
            71 => {
                let _ = spy.read(cx).term.screen();
                if spy.read(cx).term.cursor() != (2, 4) {
                    return;
                }
                let sent_before = self.w5.log.lock().unwrap().len();
                let (cw, lh) = {
                    let m = &spy.read(cx).metrics;
                    (f32::from(m.cell_w), f32::from(m.line_h))
                };
                let origin = gpui::point(gpui::px(100.0), gpui::px(50.0));
                let eb = gpui::Bounds::new(origin, gpui::size(gpui::px(600.0), gpui::px(300.0)));
                let near = |a: gpui::Pixels, b: f32| (f32::from(a) - b).abs() < 0.01;
                let mut cand_ok = true;
                let mut ranges_ok = true;
                // Composition grows: each step replaces the preedit.
                for (text, cells, units) in [("\u{306b}", 2usize, 1usize), ("\u{306b}\u{307b}", 4, 2), ("\u{306b}\u{307b}\u{3093}", 6, 3), ("\u{20bb7}a", 3, 3)] {
                    let b = spy.update(cx, |v, cx| {
                        v.replace_and_mark_text_in_range(None, text, None, window, cx);
                        let r = v.marked_text_range(window, cx);
                        ranges_ok &= r == Some(0..units);
                        ranges_ok &= v.selected_text_range(false, window, cx).map(|s| s.range) == Some(units..units);
                        v.bounds_for_range(0..units, eb, window, cx)
                    });
                    match b {
                        Some(b) => {
                            cand_ok &= near(b.origin.x, 100.0 + cw * 4.0)
                                && near(b.origin.y, 50.0 + lh * 2.0)
                                && near(b.size.width, cw * cells as f32)
                                && near(b.size.height, lh);
                        }
                        None => cand_ok = false,
                    }
                }
                check("ime_composition_marks_preedit", ranges_ok);
                check("ime_candidate_window_at_cursor_cell", cand_ok);
                check("ime_composition_sends_nothing", self.w5.log.lock().unwrap().len() == sent_before);
                // Cancel (Esc in the IME): the preedit goes, nothing is sent.
                spy.update(cx, |v, cx| v.unmark_text(window, cx));
                check("ime_cancel_clears_preedit", spy.read(cx).marked.is_none() && self.w5.log.lock().unwrap().len() == sent_before);
                // Commit: the composed text goes to the pane once, as typed input (not auto).
                spy.update(cx, |v, cx| {
                    v.replace_and_mark_text_in_range(None, "\u{306b}\u{307b}\u{3093}", None, window, cx);
                    v.replace_text_in_range(None, "\u{65e5}\u{672c}\u{8a9e}", window, cx);
                });
                let log = self.w5.log.lock().unwrap().clone();
                let last = log.last().cloned();
                check("ime_commit_sends_text_once", log.len() == sent_before + 1 && last == Some((false, "\u{65e5}\u{672c}\u{8a9e}".as_bytes().to_vec())));
                check("ime_commit_clears_preedit", spy.read(cx).marked.is_none());
                // With the cursor on the last column the candidate window is still placed at the cell.
                let _ = tx.try_send(PaneEvent::Output(b"\x1b[1;40H".to_vec()));
                self.sub = 72;
                self.sub_t0 = Instant::now();
            }
            _ => {
                let _ = spy.read(cx).term.screen();
                if spy.read(cx).term.cursor() != (0, 39) {
                    return;
                }
                let (cw, lh) = {
                    let m = &spy.read(cx).metrics;
                    (f32::from(m.cell_w), f32::from(m.line_h))
                };
                let eb = gpui::Bounds::new(gpui::point(gpui::px(0.0), gpui::px(0.0)), gpui::size(gpui::px(400.0), gpui::px(200.0)));
                let b = spy.update(cx, |v, cx| {
                    v.replace_and_mark_text_in_range(None, "\u{3042}", None, window, cx);
                    let b = v.bounds_for_range(0..1, eb, window, cx);
                    v.unmark_text(window, cx);
                    b
                });
                let ok = b.is_some_and(|b| (f32::from(b.origin.x) - cw * 39.0).abs() < 0.01 && f32::from(b.origin.y).abs() < 0.01 && f32::from(b.size.height) == lh);
                check("ime_candidate_window_last_column", ok);
                self.w5.spy = None;
                self.w5.tx = None;
                self.sub = 9;
                self.sub_t0 = Instant::now();
            }
        }
    }
}

// ---- W5: letterbox, AD-025 flags, replay replies, copy with scrollback ---------------------

impl ShellSmoke {
    fn w5_view(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        let Some(view) = self.remote_view(shell, 0) else { return };
        if self.sub_t0.elapsed() > Duration::from_secs(20) {
            fail(&format!("w5 view checks stuck at step {}", self.sub));
        }
        let spy_view = self.w5.spy.clone();
        let send = |w5: &W5, ev: PaneEvent| {
            if let Some(tx) = &w5.tx {
                let _ = tx.try_send(ev);
            }
        };
        match self.sub {
            // Another client owns a smaller PTY: the VT follows it and is centered.
            0 => {
                self.w5.win = view.read(cx).win_grid;
                // Another client owns a PTY of about half the window grid.
                let small = ((self.w5.win.0 / 2).max(3), (self.w5.win.1 / 2).max(10));
                self.w5.small = small;
                record("letterbox_grids", json!({"window": [self.w5.win.0, self.w5.win.1], "pty": [small.0, small.1]}));
                if let Some(srv) = self.w3.server.as_mut() {
                    srv.send(&format!("size {} {} Victor Phone", small.0, small.1));
                }
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            1 => {
                let (rows, cols, pad, resizes_win) = {
                    let v = view.read(cx);
                    (v.rows, v.cols, v.letterbox_pad(), v.win_grid)
                };
                if (rows, cols) != self.w5.small {
                    return;
                }
                record("letterbox_pad_px", json!([pad.0, pad.1]));
                check("letterbox_vt_follows_pty", true);
                // AD-026: the client that set the size is named in the letterbox area.
                let label = view.read(cx).size_owner_label();
                record("size_owner_label", json!(label));
                check("size_owner_label_shown", label.as_deref() == Some("Size set by Victor Phone (Windows)"));
                check("letterbox_window_grid_kept", resizes_win == self.w5.win && resizes_win.0 > rows && resizes_win.1 > cols);
                check("letterbox_centered", pad.0 > 0.0 && pad.1 > 0.0);
                // The owner gives it back at the window size.
                if let Some(srv) = self.w3.server.as_mut() {
                    srv.send(&format!("size {} {}", self.w5.win.0, self.w5.win.1));
                }
                self.sub = 2;
                self.sub_t0 = Instant::now();
            }
            2 => {
                let (rows, cols, pad) = {
                    let v = view.read(cx);
                    (v.rows, v.cols, v.letterbox_pad())
                };
                if (rows, cols) != self.w5.win {
                    return;
                }
                check("letterbox_fills_when_sizes_match", pad.0 < 8.0 && pad.1 < 20.0);
                // The server restarted and input was dropped: a notice in the pane, no modal.
                shell.on_events(vec![forgetty_shell_core::BackendEvent::InputLost { bytes: 7 }], cx);
                let notice = view.read(cx).notice_text();
                record("input_lost_notice", json!(notice));
                check("input_lost_notice_shown", notice.as_deref() == Some("Server restarted, 7 bytes of input not sent"));
                // A spy view: what does the view write, and as which kind?
                let (tx, rx) = async_channel::unbounded();
                let cfg = shell.cfg.clone();
                let log = self.w5.log.clone();
                let spy = cx.new(|cx| TerminalView::new(cfg, PaneId::new(), Box::new(Spy { rx, log }), 10, 40, true, cx));
                self.w5.spy = Some(spy);
                self.w5.tx = Some(tx);
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 3;
                self.sub_t0 = Instant::now();
            }
            // AD-025: a terminal reply is `auto`, never typing.
            3 => {
                let log = self.w5.log.lock().unwrap().clone();
                if log.is_empty() {
                    return;
                }
                record("vt_reply_bytes", json!(String::from_utf8_lossy(&log[0].1).to_string()));
                check("vt_reply_is_auto", log.len() == 1 && log[0].0);
                // A reset starts a replay: the reply to a query inside it is dropped.
                send(&self.w5, PaneEvent::Reset { rows: 10, cols: 40, exact_replay: false });
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 4;
                self.sub_t0 = Instant::now();
            }
            4 => {
                if self.sub_t0.elapsed() < Duration::from_millis(400) {
                    return;
                }
                let n = self.w5.log.lock().unwrap().len();
                check("replay_reply_suppressed", n == 1);
                // After the replay, the next query is answered again.
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 5;
                self.sub_t0 = Instant::now();
            }
            5 => {
                let n = self.w5.log.lock().unwrap().len();
                if n < 2 {
                    return;
                }
                check("live_reply_after_replay", n == 2 && self.w5.log.lock().unwrap()[1].0);
                // A server that sends `replay_end` ends the replay exactly there, not by timer.
                send(&self.w5, PaneEvent::Reset { rows: 10, cols: 40, exact_replay: true });
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 40;
                self.sub_t0 = Instant::now();
            }
            40 => {
                // Past the 2 s of the heuristic the replay is still on: the server did not end it.
                if self.sub_t0.elapsed() < Duration::from_millis(2600) {
                    return;
                }
                check("replay_end_exact_ignores_timer", self.w5.log.lock().unwrap().len() == 2);
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 41;
                self.sub_t0 = Instant::now();
            }
            41 => {
                if self.sub_t0.elapsed() < Duration::from_millis(300) {
                    return;
                }
                check("replay_end_exact_still_replaying", self.w5.log.lock().unwrap().len() == 2);
                send(&self.w5, PaneEvent::ReplayEnd);
                send(&self.w5, PaneEvent::Output(b"\x1b[c".to_vec()));
                self.sub = 42;
                self.sub_t0 = Instant::now();
            }
            42 => {
                if self.w5.log.lock().unwrap().len() < 3 {
                    return;
                }
                check("replay_end_from_server_ends_replay", self.w5.log.lock().unwrap().len() == 3);
                self.w5.log.lock().unwrap().clear();
                // Focus reports are `auto` too.
                send(&self.w5, PaneEvent::Output(b"\x1b[?1004h".to_vec()));
                self.sub = 6;
                self.sub_t0 = Instant::now();
            }
            6 => {
                if self.sub_t0.elapsed() < Duration::from_millis(300) {
                    return;
                }
                if let Some(v) = &spy_view {
                    v.update(cx, |v, cx| v.set_focused(true, cx));
                }
                self.sub = 7;
                self.sub_t0 = Instant::now();
            }
            7 => {
                let log = self.w5.log.lock().unwrap().clone();
                if log.is_empty() {
                    return;
                }
                check("focus_report_is_auto", log[0].0 && log[0].1 == b"\x1b[I");
                check("no_reply_counted_as_typing", log.iter().all(|(auto, _)| *auto));
                // 40 numbered lines through a 10 row VT, then copy rows that are in scrollback.
                let mut text = Vec::new();
                for i in 1..=40 {
                    text.extend_from_slice(format!("L{i:02}\r\n").as_bytes());
                }
                send(&self.w5, PaneEvent::Output(text));
                self.sub = 8;
                self.sub_t0 = Instant::now();
            }
            8 => {
                let Some(v) = &spy_view else { return };
                let ready = v.read(cx).fed_bytes > 160;
                if !ready || self.sub_t0.elapsed() < Duration::from_millis(300) {
                    return;
                }
                let (text, before, after) = v.update(cx, |v, _| {
                    use crate::view::{Sel, SelMode};
                    let before = v.term.scrollbar_state().1;
                    // Absolute rows 2..=20: row 0 is the first line.
                    v.sel = Some(Sel { mode: SelMode::Normal, a0: (2, 0), a1: (20, 3), h0: (2, 0), h1: (20, 3) });
                    let t = v.selection_text().unwrap_or_default();
                    let after = v.term.scrollbar_state().1;
                    v.sel = None;
                    (t, before, after)
                });
                record("copy_scrollback_text", json!(text));
                let lines: Vec<&str> = text.lines().collect();
                check("copy_includes_scrollback", lines.len() == 19 && lines[0] == "L03" && lines[18] == "L21");
                check("copy_restores_viewport", before == after);
                check("no_input_dropped", view.read(cx).dropped_input() == 0);
                self.sub = 50;
                self.sub_t0 = Instant::now();
            }
            // ---- OSC 52 (a program sets the clipboard): write only, per machine ----------------
            50..=61 => self.w7_clipboard(shell, cx),
            70..=72 => self.w7_ime(window, cx),
            // Real keys through SendInput: does the injection reach the pane at all?
            9 => {
                let Some(pane) = shell.active_pane() else { return };
                let Some(target) = shell.views.get(&pane).cloned() else { return };
                if !window.is_window_active() && self.sub_t0.elapsed() < Duration::from_secs(5) {
                    return;
                }
                target.update(cx, |v, _| v.capture_input = Some(Vec::new()));
                self.w5.target = Some(target);
                let ok = inject::activate_layout("00000409").map(|hkl| inject::send_keys(hkl, &[Step::Tap(inject::VK_A)])).unwrap_or(false);
                record("sendinput_sent", json!(ok));
                self.sub = 10;
                self.sub_t0 = Instant::now();
            }
            10 => {
                let Some(t) = self.w5.target.clone() else { return };
                let got = t.read(cx).capture_input.clone().unwrap_or_default();
                if got == b"a" {
                    record("sendinput_works", json!(true));
                    self.w5.case_i = 0;
                    self.sub = 11;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_secs(3) {
                    // The runner's desktop does not deliver injected keys: say so, skip the rest.
                    record("sendinput_works", json!(false));
                    record("sendinput_probe_bytes", json!(String::from_utf8_lossy(&got).to_string()));
                    t.update(cx, |v, _| v.capture_input = None);
                    self.sub = 20;
                    self.sub_t0 = Instant::now();
                }
            }
            11 => {
                let Some(t) = self.w5.target.clone() else { return };
                let Some(case) = KEY_CASES.get(self.w5.case_i) else {
                    t.update(cx, |v, _| v.capture_input = None);
                    inject::activate_layout("00000409");
                    // The runner has Microsoft IME (Japanese) and Pinyin installed: drive them.
                    self.sub = 80;
                    self.sub_t0 = Instant::now();
                    return;
                };
                t.update(cx, |v, _| v.capture_input = Some(Vec::new()));
                match inject::activate_layout(case.klid) {
                    Some(hkl) => {
                        inject::send_keys(hkl, case.steps);
                        self.sub = 12;
                    }
                    None => {
                        record(&format!("dead_key_{}_skipped", case.name), json!("layout not installed"));
                        self.w5.case_i += 1;
                    }
                }
                self.sub_t0 = Instant::now();
            }
            12 => {
                if self.sub_t0.elapsed() < Duration::from_millis(500) {
                    return;
                }
                let Some(t) = self.w5.target.clone() else { return };
                let case = &KEY_CASES[self.w5.case_i];
                let got = t.read(cx).capture_input.clone().unwrap_or_default();
                let ok = got == case.want.as_bytes();
                record(&format!("key_case_{}_bytes", case.name), json!(String::from_utf8_lossy(&got).to_string()));
                check(&format!("layout_{}", case.name), ok);
                if !ok {
                    // Flush a dead key that is still waiting for its letter.
                    if let Some(hkl) = inject::activate_layout(case.klid) {
                        inject::send_keys(hkl, &[Step::Tap(inject::VK_SPACE)]);
                    }
                }
                self.w5.case_i += 1;
                self.sub = 11;
                self.sub_t0 = Instant::now();
            }
            // ---- a real IME (Microsoft IME Japanese, Microsoft Pinyin) driven by SendInput ---------
            80..=89 => self.w7_real_ime(window, cx),
            // A button released outside the pane still ends the selection.
            20 => {
                shell.sidebar_open = true;
                if self.sub_t0.elapsed() < Duration::from_millis(400) {
                    return;
                }
                let Some(t) = self.w5.target.clone() else { self.finish_w3(shell) };
                let origin = inject::hwnd(window).and_then(inject::client_origin);
                let scale = window.scale_factor();
                let b = t.read(cx).bounds;
                let ok = origin.is_some_and(|(ox, oy)| {
                    let x = ox as f32 + (f32::from(b.origin.x) + f32::from(b.size.width) / 2.0) * scale;
                    let y = oy as f32 + (f32::from(b.origin.y) + f32::from(b.size.height) / 2.0) * scale;
                    inject::move_cursor(x as i32, y as i32) && inject::left_down()
                });
                record("mouse_press_sent", json!(ok));
                self.sub = 21;
                self.sub_t0 = Instant::now();
            }
            21 => {
                if self.sub_t0.elapsed() < Duration::from_millis(300) {
                    return;
                }
                let Some(t) = self.w5.target.clone() else { self.finish_w3(shell) };
                if !t.read(cx).selecting {
                    // The press did not land in the pane: no verdict.
                    record("mouse_injection_works", json!(false));
                    inject::left_up();
                    self.finish_w3(shell);
                }
                record("mouse_injection_works", json!(true));
                // Release over the sidebar, outside the pane.
                if let Some((ox, oy)) = inject::hwnd(window).and_then(inject::client_origin) {
                    let scale = window.scale_factor();
                    let b = t.read(cx).bounds;
                    let x = ox as f32 + (crate::shell::SIDEBAR_W / 2.0) * scale;
                    let y = oy as f32 + (f32::from(b.origin.y) + f32::from(b.size.height) / 2.0) * scale;
                    inject::move_cursor(x as i32, y as i32);
                }
                self.sub = 22;
                self.sub_t0 = Instant::now();
            }
            22 => {
                if self.sub_t0.elapsed() < Duration::from_millis(200) {
                    return;
                }
                inject::left_up();
                self.sub = 23;
                self.sub_t0 = Instant::now();
            }
            23 => {
                if self.sub_t0.elapsed() < Duration::from_millis(400) {
                    return;
                }
                if let Some(t) = self.w5.target.clone() {
                    check("mouse_up_outside_pane_ends_selection", !t.read(cx).selecting);
                }
                self.finish_w3(shell);
            }
            _ => {}
        }
    }
}
