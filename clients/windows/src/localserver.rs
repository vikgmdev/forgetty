//! The local server supervisor (WP6). The app finds `forgetty.exe` next to itself, starts it
//! detached when it does not run, pairs with it by itself (`forgetty pair` prints a payload
//! while its window is open, the app pairs over loopback with the code the pair dialog uses),
//! and shows it as "This PC" through `RemoteBackend`. Tabs then live in the server and outlive
//! the window. When the server cannot start, or the window is elevated, the in-process ConPTY
//! terminal stays, with a notice.
//!
//! `forgetty.exe` is a console program run as a subprocess (`CREATE_NO_WINDOW`): the app does
//! not link the server library. The pure rules (where the exe is, what the CLI printed, when to
//! restart) are in `forgetty_shell_core::localserver`.

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forgetty_remote::store::{self, MachineStore, StoredMachine};
use forgetty_shell_core::localserver::{self as ls, Fallback, PairLine, Plan};
use forgetty_shell_core::{ConnState, MachineKind, SessionBackend, Uuid};
use forgetty_shell_core::devices::{Avail, DevEffect};
use gpui::Context;

use crate::shell::AppShell;

/// How long `forgetty server start` may take (the CLI itself waits up to 20 s).
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

/// Hide the console window of a child process.
fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

pub struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// `forgetty.exe`, as the app runs it.
#[derive(Clone, Debug)]
pub struct Cli {
    pub exe: PathBuf,
    /// `--state-dir`: the smoke keeps its server away from the user's.
    pub state_dir: Option<PathBuf>,
}

impl Cli {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.exe);
        if let Some(d) = &self.state_dir {
            c.arg("--state-dir").arg(d);
        }
        c.args(args).stdin(Stdio::null());
        hide(&mut c);
        c
    }

    /// Run to the end, or kill after `timeout` (`code` is then `None`).
    pub fn run(&self, args: &[&str], timeout: Duration) -> std::io::Result<Out> {
        let mut child = self.command(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let (tx, rx) = mpsc::channel::<(bool, String)>();
        let read = |r: Option<Box<dyn Read + Send>>, is_out: bool, tx: mpsc::Sender<(bool, String)>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(r) = r {
                    let _ = r.take(1 << 20).read_to_end(&mut buf);
                }
                let _ = tx.send((is_out, String::from_utf8_lossy(&buf).into_owned()));
            });
        };
        read(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), true, tx.clone());
        read(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>), false, tx);
        let deadline = Instant::now() + timeout;
        let code = loop {
            match child.try_wait()? {
                Some(s) => break s.code().or(Some(-1)),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        let (mut stdout, mut stderr) = (String::new(), String::new());
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok((true, s)) => stdout = s,
                Ok((false, s)) => stderr = s,
                Err(_) => break,
            }
        }
        Ok(Out { code, stdout, stderr })
    }

    /// `forgetty server start`: does nothing when a server answers.
    ///
    /// Every try passes no handles to the process: the server it starts must not keep a file or
    /// pipe of this app (or of whoever started it) open for as long as it lives. So there are no
    /// pipes to read an error from. When the second try fails too, the reason is read from the
    /// server's log, which the command writes.
    pub fn start_server(&self) -> Result<(), Fallback> {
        let timeout = || Fallback::Start("the server did not answer within 30 s".into());
        match self.run_clean(&["server", "start"], START_TIMEOUT) {
            Ok(Some(0)) => return Ok(()),
            Ok(None) => return Err(timeout()),
            _ => {}
        }
        // A start can lose a race with a server that is starting at the same moment.
        std::thread::sleep(Duration::from_secs(1));
        match self.run_clean(&["server", "start"], START_TIMEOUT) {
            Ok(Some(0)) => Ok(()),
            Ok(None) => Err(timeout()),
            Ok(code) => Err(Fallback::Start(ls::cli_reason("", &self.server_log_tail(), code))),
            Err(e) => Err(Fallback::Start(format!("cannot run {}: {e}", self.exe.display()))),
        }
    }

    /// The last lines of the server's log (the command writes why it could not start there).
    fn server_log_tail(&self) -> String {
        let log = match &self.state_dir {
            None => Some(forgetty_core::platform::data_dir().join("server.log")),
            Some(d) => find_file(d, "server.log", 5),
        };
        let text = log.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        let lines = &all[all.len().saturating_sub(12)..];
        // The server's own words ("forgetty server: ...") when it said any, else the last lines.
        match lines.iter().rev().find(|l| l.contains("forgetty server: ")) {
            Some(l) => l.to_string(),
            None => lines[lines.len().saturating_sub(3)..].join("\n"),
        }
    }

    /// Run with no inherited handle (`bInheritHandles` false, no standard handles) and no
    /// window. `Some(code)` when it ended, `None` when it was killed after `timeout`.
    #[cfg(windows)]
    fn run_clean(&self, args: &[&str], timeout: Duration) -> std::io::Result<Option<i32>> {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
            PROCESS_INFORMATION, STARTUPINFOW,
        };
        let mut argv: Vec<String> = Vec::new();
        if let Some(d) = &self.state_dir {
            argv.push("--state-dir".into());
            argv.push(d.to_string_lossy().into_owned());
        }
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let line = forgetty_shell_core::argv::command_line(&self.exe.to_string_lossy(), &argv);
        let mut cmdline: Vec<u16> = std::ffi::OsStr::new(&line).encode_wide().chain(std::iter::once(0)).collect();
        let app: Vec<u16> = self.exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: the strings are NUL-terminated and outlive the call, `si` and `pi` are valid.
        // The handles of the new process are closed here.
        unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            let _ = CloseHandle(pi.hThread);
            let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            let code = if WaitForSingleObject(pi.hProcess, ms) == WAIT_OBJECT_0 {
                let mut c = 0u32;
                let _ = GetExitCodeProcess(pi.hProcess, &mut c);
                Some(c as i32)
            } else {
                let _ = TerminateProcess(pi.hProcess, 1);
                None
            };
            let _ = CloseHandle(pi.hProcess);
            Ok(code)
        }
    }

    #[cfg(not(windows))]
    fn run_clean(&self, args: &[&str], timeout: Duration) -> std::io::Result<Option<i32>> {
        self.run(args, timeout).map(|o| o.code)
    }

    /// `forgetty devices --json`.
    pub fn devices(&self) -> Result<Vec<ls::Device>, String> {
        match self.run(&["devices", "--json"], Duration::from_secs(20)) {
            Ok(o) if o.code == Some(0) => ls::parse_devices(&o.stdout),
            Ok(o) => Err(ls::cli_reason(&o.stdout, &o.stderr, o.code)),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn status(&self) -> Result<ls::ServerStatus, Fallback> {
        match self.run(&["server", "status", "--json"], STATUS_TIMEOUT) {
            Ok(o) if o.code == Some(0) => {
                ls::parse_status(&o.stdout).ok_or_else(|| Fallback::Start("the server reported no identity".into()))
            }
            Ok(o) => Err(Fallback::Start(ls::cli_reason(&o.stdout, &o.stderr, o.code))),
            Err(e) => Err(Fallback::Start(e.to_string())),
        }
    }
}

/// The first file called `name` in `dir` or up to `depth` folders below it.
fn find_file(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
        if depth > 0 && p.is_dir() {
            if let Some(f) = find_file(&p, name, depth - 1) {
                return Some(f);
            }
        }
    }
    None
}

/// `forgetty-win.exe --start-server`: what the login Run value runs. GUI subsystem, no window.
/// Returns the process exit code. `local_server = false` in the config means no server: 0.
pub fn start_server_main() -> i32 {
    let enabled = crate::config::load().config.local_server;
    // Elevated: no server, whatever else is set (the server refuses to run elevated, and an
    // elevated start must not hand a normal program a server that does).
    if !ls::login_start_wanted(enabled, elevated(false)) {
        return 0;
    }
    let exe = match find_server_exe(false) {
        Ok(Some(e)) => e,
        _ => return 2,
    };
    match (Cli { exe, state_dir: None }).start_server() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Where the app looks for `forgetty.exe`: next to `forgetty-win.exe`, never `PATH`. The
/// developer override `FORGETTY_SERVER_EXE` (an absolute local path, else an error) is read only
/// when `smoke` is set or in a debug build: a release run of the app ignores the variable.
pub fn find_server_exe(smoke: bool) -> Result<Option<PathBuf>, String> {
    let allow = smoke || cfg!(debug_assertions);
    let app_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    ls::server_exe(allow, std::env::var_os("FORGETTY_SERVER_EXE").as_deref(), app_dir.as_deref())
}

/// Is this window treated as elevated? In `--smoke`, `FORGETTY_TEST_ALLOW_ELEVATED=1` says the
/// elevated CI runner may use a server built to allow it (a release server never does).
pub fn elevated(smoke: bool) -> bool {
    if smoke && std::env::var_os("FORGETTY_TEST_ALLOW_ELEVATED").is_some_and(|v| v == "1") {
        return false;
    }
    ls::is_elevated()
}

pub fn decide(enabled: bool, smoke: bool) -> Plan {
    match find_server_exe(smoke) {
        Ok(exe) => ls::plan(enabled, elevated(smoke), exe),
        // A bad override is said, not skipped: the developer asked for that program.
        Err(why) if enabled && !elevated(smoke) => Plan::InProcess(Fallback::Start(why)),
        Err(_) => ls::plan(enabled, elevated(smoke), None),
    }
}

/// Start the server if needed and make sure this app is paired with it. Blocking: run it on a
/// worker thread. The returned machine is stored with `local` set. At every start the stored
/// pairing is checked against what the server says now (`ls::check_pairing`): a new identity or
/// a device list without this app means pair again, a new port means the stored address follows.
pub fn bring_up(
    cli: &Cli,
    rt: &tokio::runtime::Handle,
    store: &MachineStore,
    device: &str,
    hint: Option<SocketAddr>,
) -> Result<StoredMachine, Fallback> {
    cli.start_server()?;
    let status = cli.status()?;
    if let Some(mut m) = store.local_machine() {
        let own = store.device_id().unwrap_or_default();
        let devices = cli.devices().ok();
        match ls::check_pairing(&m.node_id, &m.pinned_addrs, &status, devices.as_deref(), &own) {
            ls::PairingCheck::Keep => return Ok(m),
            ls::PairingCheck::NewAddress(a) => {
                eprintln!("forgetty-win: local server: now at {a:?}");
                m.pinned_addrs = a;
                m.relays_disabled = true;
                m.relays.clear();
                return store.add(m).map_err(|e| Fallback::Pair(format!("machines.json: {e}")));
            }
            ls::PairingCheck::Repair(why) => {
                eprintln!("forgetty-win: local server: pairing again: {why}");
                let _ = store.remove(m.id);
            }
        }
    }
    pair_local(cli, rt, store, device, hint, &status)
}

/// `forgetty pair --ttl N` opens a pairing window and prints the payload; the app pairs over
/// loopback while the command waits, then the command reports the device.
fn pair_local(
    cli: &Cli,
    rt: &tokio::runtime::Handle,
    store: &MachineStore,
    device: &str,
    hint: Option<SocketAddr>,
    status: &ls::ServerStatus,
) -> Result<StoredMachine, Fallback> {
    let ttl = ls::AUTO_PAIR_TTL_SECS.to_string();
    let mut child = cli
        .command(&["pair", "--ttl", &ttl])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Fallback::Pair(format!("cannot run forgetty pair: {e}")))?;
    let result = (|| {
        let lines = line_channel(&mut child);
        let mut acc = String::new();
        let t0 = Instant::now();
        // Until the payload line: the QR block and the payload.
        loop {
            let left = Duration::from_secs(20).saturating_sub(t0.elapsed());
            match lines.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(l) => {
                    acc.push_str(&l);
                    acc.push('\n');
                    match ls::pair_line(&l) {
                        PairLine::Payload => break,
                        PairLine::Expired => return Err(Fallback::Pair("the pairing window closed".into())),
                        _ => {}
                    }
                }
                Err(_) => return Err(Fallback::Pair("no pairing payload from the server".into())),
            }
        }
        let offer = store::parse_offer(&acc).map_err(Fallback::Pair)?;
        if offer.qr.node_id != status.node_id {
            return Err(Fallback::Pair("the server's identity changed while pairing".into()));
        }
        let m = rt
            .block_on(store::pair_as(store, &offer, device, Some(hint.unwrap_or_else(|| status.pins()[0])), true))
            .map_err(|e| Fallback::Pair(format!("{e} (dialed {:?}, relays off: {})", offer.direct, offer.relays_disabled)))?;
        // The pins are what the server says it is bound to, not the payload's address list.
        let mut m = m;
        if hint.is_none() && m.pinned_addrs != status.pins() {
            m.pinned_addrs = status.pins();
            m = store.add(m).map_err(|e| Fallback::Pair(format!("machines.json: {e}")))?;
        }
        // The command prints `paired:` and ends. Not seeing it is not an error: the pairing worked.
        let t1 = Instant::now();
        while t1.elapsed() < Duration::from_secs(10) {
            match lines.recv_timeout(Duration::from_millis(250)) {
                Ok(l) if matches!(ls::pair_line(&l), PairLine::Paired { .. }) => break,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        Ok(m)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The lines of a child's stdout, as they come.
fn line_channel(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out.take(1 << 20)).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

// ---- state in the shell ------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum LsState {
    /// Not started (before `start_local_server`, or the smoke).
    #[default]
    Off,
    /// Waiting for `forgetty.exe` and the pairing.
    Starting,
    Up,
    /// The in-process terminal is in use, and why.
    Fallback(Fallback),
}

#[derive(Default)]
pub struct LocalSrv {
    pub state: LsState,
    pub cli: Option<Cli>,
    /// The machine backed by the server, once adopted.
    pub machine: Option<Uuid>,
    /// The in-process machine that was on screen at start.
    pub inproc: Option<Uuid>,
    /// Keep the in-process machine after the server one is up (the smoke does).
    pub keep_inproc: bool,
    /// Since when the server machine has had no link.
    pub down_since: Option<Instant>,
    pub tries: u32,
    pub last_try: Option<Instant>,
    pub restarting: bool,
    pub last_error: Option<String>,
    /// When each restart of this session was made (a server that keeps going down is given up).
    pub restarts: Vec<Instant>,
    /// "Retry the local server" is running: the built-in tabs stay on screen.
    pub retry: bool,
    pub watchdog_on: bool,
    /// Set to end the pairing command of the Devices page.
    pub pair_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl LocalSrv {
    /// The server is on its way and the window has no terminal yet.
    pub fn pending(&self) -> bool {
        self.state == LsState::Starting
    }
}

impl AppShell {
    /// Is the machine on screen this PC? The in-process terminal and the local server both are:
    /// profiles start programs here, and closing the last tab ends the app.
    pub fn is_this_pc(&self) -> bool {
        self.machine.kind == MachineKind::Local || self.lsrv.machine == Some(self.machine.id)
    }

    /// Follow `plan`: start the server and adopt it, or stay in-process. Called once at start
    /// (before the first tab opens) and by the smoke.
    pub fn start_local_server(&mut self, plan: Plan, state_dir: Option<PathBuf>, cx: &mut Context<Self>) {
        self.lsrv.inproc = Some(self.machine.id);
        match plan {
            Plan::InProcess(f) => self.server_fallback(f, false, cx),
            Plan::Server(exe) => {
                if self.remote.is_none() {
                    self.server_fallback(Fallback::Start("remote machines are off (no %APPDATA%)".into()), false, cx);
                    return;
                }
                self.lsrv.state = LsState::Starting;
                self.lsrv.cli = Some(Cli { exe, state_dir });
                self.run_bring_up(cx);
                self.start_watchdog(cx);
            }
        }
    }

    /// The palette command "Retry the local server": after a fallback, try the server again with
    /// a fresh restart count. The built-in tabs stay on screen; the server shows as This PC.
    pub fn retry_local_server(&mut self, cx: &mut Context<Self>) {
        match self.lsrv.state {
            LsState::Up => {
                self.notice_active("The local server is running".into(), cx);
                return;
            }
            LsState::Starting => {
                self.notice_active("The local server is starting".into(), cx);
                return;
            }
            _ => {}
        }
        if self.remote.is_none() {
            self.notice_active("The local server needs %APPDATA% (machines are off)".into(), cx);
            return;
        }
        let exe = match decide(self.cfg.local_server, self.smoke_mode) {
            Plan::Server(exe) => exe,
            Plan::InProcess(f) => {
                let t = f.notice().unwrap_or_else(|| "The local server is turned off (local_server = false in the config)".into());
                self.notice_active(t, cx);
                return;
            }
        };
        let state_dir = self.lsrv.cli.as_ref().and_then(|c| c.state_dir.clone());
        self.lsrv.cli = Some(Cli { exe, state_dir });
        self.lsrv.restarts.clear();
        self.lsrv.tries = 0;
        self.lsrv.down_since = None;
        self.lsrv.last_try = None;
        self.lsrv.last_error = None;
        self.lsrv.retry = true;
        self.lsrv.keep_inproc = true;
        self.lsrv.state = LsState::Starting;
        self.notice_active("Starting the local server...".into(), cx);
        self.run_bring_up(cx);
        if !self.lsrv.watchdog_on {
            self.start_watchdog(cx);
        }
    }

    fn run_bring_up(&mut self, cx: &mut Context<Self>) {
        let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
        let (rt, store, device, hint) = (r.rt.clone(), r.store.clone(), r.device_name.clone(), self.pair_hint);
        let rt2 = rt.clone();
        let task = rt.spawn_blocking(move || bring_up(&cli, &rt2, &store, &device, hint));
        cx.spawn(async move |this, cx| {
            let res = task.await.unwrap_or_else(|e| Err(Fallback::Start(e.to_string())));
            let _ = this.update(cx, |s, cx| s.bring_up_done(res, cx));
        })
        .detach();
    }

    fn bring_up_done(&mut self, res: Result<StoredMachine, Fallback>, cx: &mut Context<Self>) {
        match res {
            Ok(mut m) => {
                let retry = std::mem::take(&mut self.lsrv.retry);
                self.lsrv.state = LsState::Up;
                self.lsrv.machine = Some(m.id);
                m.name = ls::THIS_PC.into();
                // After a retry the user is working in the built-in terminal: do not move them.
                self.connect_stored(m, !retry, cx);
                if retry {
                    self.notice_active("The local server is back. Switch to This PC in the sidebar to use it.".into(), cx);
                }
            }
            Err(f) if self.lsrv.retry => {
                self.lsrv.retry = false;
                eprintln!("forgetty-win: local server: {f:?}");
                if let Some(n) = f.notice() {
                    self.notice_active(n, cx);
                }
                self.lsrv.state = LsState::Fallback(f);
            }
            Err(f) => self.server_fallback(f, true, cx),
        }
    }

    /// The in-process terminal is the one in use. `open_tab`: the start-up tab was held back
    /// while the server came up, so open it now.
    fn server_fallback(&mut self, f: Fallback, open_tab: bool, cx: &mut Context<Self>) {
        eprintln!("forgetty-win: local server: {f:?}");
        if let Some(n) = f.notice() {
            self.startup_notice = Some(match self.startup_notice.take() {
                Some(old) => format!("{old} | {n}"),
                None => n,
            });
        }
        self.lsrv.state = LsState::Fallback(f);
        if !open_tab {
            return;
        }
        let Some(local) = self.local.clone() else { return };
        let id = local.machine().id;
        if !self.machines.iter().any(|m| m.id == id) {
            // The built-in machine was dropped when the server came up: bring it back.
            self.switch_to = Some(id);
            self.add_machine(Arc::new(local) as Arc<dyn SessionBackend>, cx);
        } else if self.machine.id != id {
            self.switch_machine(id, cx);
        }
        if self.machine.id == id && self.layout.tab_count() == 0 {
            self.new_tab(cx);
        }
        cx.notify();
    }

    /// The server machine is on screen: the built-in terminal has nothing to show.
    pub(crate) fn adopted(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.lsrv.machine != Some(id) || self.lsrv.keep_inproc {
            return;
        }
        if let Some(inproc) = self.lsrv.inproc {
            self.drop_machine(inproc, cx);
        }
    }

    /// The palette command "Quit and end all tabs on this PC": the counterpart of GTK's Close
    /// Window Permanently. Asks first.
    pub fn ask_end_session(&mut self, cx: &mut Context<Self>) {
        if self.lsrv.machine != Some(self.machine.id) {
            self.notice_active("Tabs of this window already end when it closes. Use Quit. (Switch to This PC to end the local server's tabs.)".into(), cx);
            return;
        }
        let n = self.layout.workspaces.iter().map(|w| w.tabs.len()).sum::<usize>();
        self.confirm = Some(crate::menus::Confirm {
            text: format!("End all {n} tab(s) on this PC and quit? Programs running in them stop and the local server stops."),
            act: crate::menus::ConfirmAct::EndSession,
        });
        cx.notify();
    }

    /// Close every tab, stop the server, quit. The server saves nothing to restore: no tab is left.
    pub(crate) fn end_session_now(&mut self, cx: &mut Context<Self>) {
        let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
        let rt = r.rt.clone();
        // The watchdog must not start the server again.
        self.lsrv.state = LsState::Off;
        let tabs: Vec<Uuid> = self.layout.workspaces.iter().flat_map(|w| w.tabs.iter().map(|t| t.id)).collect();
        let futs: Vec<_> = tabs.into_iter().map(|t| self.backend.close_tab(t)).collect();
        self.quit_ok = true;
        cx.spawn(async move |this, cx| {
            for f in futs {
                let _ = f.await;
            }
            let _ = rt.spawn_blocking(move || cli.run(&["server", "stop"], Duration::from_secs(30))).await;
            let _ = this.update(cx, |_, cx| cx.quit());
        })
        .detach();
    }

    /// The last tab of the server machine closed: end the app unless tabs are back within 1.5 s.
    pub(crate) fn quit_if_still_empty(&mut self, id: Uuid, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |s, cx| {
                let connected = s.machines.iter().any(|m| m.id == id && m.conn.is_connected());
                if connected && s.machine.id == id && s.layout.tab_count() == 0 && s.lsrv.machine == Some(id) {
                    cx.quit();
                }
            });
        })
        .detach();
    }

    /// Remove a machine from the sidebar. Another machine is shown first when it was on screen.
    pub fn drop_machine(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if id == self.machine.id {
            let Some(other) = self.machines.iter().find(|m| m.id != id).map(|m| m.id) else { return };
            self.switch_machine(other, cx);
        }
        self.machines.retain(|m| m.id != id);
        self.parked.remove(&id);
        cx.notify();
    }

    // ---- watchdog ------------------------------------------------------------------

    fn start_watchdog(&mut self, cx: &mut Context<Self>) {
        self.lsrv.watchdog_on = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this.update(cx, |s, cx| s.watch_tick(cx)).is_err() {
                break;
            }
        })
        .detach();
    }

    fn watch_tick(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.lsrv.machine.filter(|_| self.lsrv.state == LsState::Up) else { return };
        let Some(conn) = self.machines.iter().find(|m| m.id == id).map(|m| m.conn.clone()) else { return };
        // The link stopped for good (the server does not accept this app: revoked or reset):
        // restarting the server cannot help. Retry pairs again.
        if let ConnState::Failed(why) = &conn {
            let why = why.clone();
            self.lsrv.machine = None;
            self.server_fallback(Fallback::Pair(why), true, cx);
            self.drop_machine(id, cx);
            return;
        }
        if conn.is_connected() {
            self.lsrv.down_since = None;
            self.lsrv.tries = 0;
            self.lsrv.last_try = None;
            self.lsrv.last_error = None;
            return;
        }
        let now = Instant::now();
        let down = now - *self.lsrv.down_since.get_or_insert(now);
        if self.lsrv.restarting {
            return;
        }
        let since = self.lsrv.last_try.map(|t| now - t);
        match ls::watch(down, self.lsrv.tries, since) {
            ls::Watch::Wait => {}
            ls::Watch::Restart => {
                let ages: Vec<Duration> = self.lsrv.restarts.iter().map(|t| now - *t).collect();
                if ls::too_many_restarts(&ages) {
                    self.lsrv.machine = None;
                    self.server_fallback(Fallback::Stopped("it went down 5 times in 10 minutes".into()), true, cx);
                    self.drop_machine(id, cx);
                    return;
                }
                self.lsrv.restarts.retain(|t| now - *t < ls::RESTART_WINDOW);
                self.lsrv.restarts.push(now);
                self.lsrv.tries += 1;
                self.lsrv.last_try = Some(now);
                self.lsrv.restarting = true;
                let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
                let task = r.rt.spawn_blocking(move || cli.start_server());
                cx.spawn(async move |this, cx| {
                    let res = task.await.unwrap_or_else(|e| Err(Fallback::Start(e.to_string())));
                    let _ = this.update(cx, |s, _| {
                        s.lsrv.restarting = false;
                        s.lsrv.last_try = Some(Instant::now());
                        if let Err(f) = res {
                            s.lsrv.last_error = Some(match f {
                                Fallback::Start(w) => w,
                                other => format!("{other:?}"),
                            });
                        }
                    });
                })
                .detach();
                // The link retries at once now that a server should be there.
                if let Some(m) = self.machines.iter().find(|m| m.id == id) {
                    m.backend.reconnect_now();
                }
            }
            ls::Watch::GiveUp => {
                let why = self.lsrv.last_error.take().unwrap_or_else(|| "no answer".into());
                self.lsrv.machine = None;
                self.server_fallback(Fallback::Stopped(why), true, cx);
                self.drop_machine(id, cx);
            }
        }
    }
}

// ---- the Devices page (M1) -------------------------------------------------------------

enum PairMsg {
    Output(String),
    Done(Result<String, String>),
}

/// `forgetty pair --ttl N` for a device of the user: send the output up to the payload (the QR
/// block) when it comes, then the result. Ends when `cancel` is set.
fn pair_for_user(cli: &Cli, cancel: &std::sync::atomic::AtomicBool, tx: &async_channel::Sender<PairMsg>) {
    use std::sync::atomic::Ordering;
    let ttl = ls::USER_PAIR_TTL_SECS.to_string();
    let mut child = match cli.command(&["pair", "--ttl", &ttl]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send_blocking(PairMsg::Done(Err(format!("cannot run forgetty pair: {e}"))));
            return;
        }
    };
    let lines = line_channel(&mut child);
    let mut acc = String::new();
    // The window this command opened, once it says so: a late cancel must not end a newer one.
    let mut window: Option<u64> = None;
    let end = loop {
        if cancel.load(Ordering::SeqCst) {
            // `pair --cancel` ends the window at once, so the code stops working.
            let id = window.map(|w| w.to_string());
            let mut args = vec!["pair", "--cancel"];
            if let Some(id) = &id {
                args.extend(["--window-id", id]);
            }
            let closed = matches!(cli.run(&args, Duration::from_secs(10)), Ok(o) if o.code == Some(0));
            break Err(if closed {
                "Cancelled. The code no longer works.".to_string()
            } else {
                "Cancelled. The code stays valid until it expires (2 minutes).".to_string()
            });
        }
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => {
                acc.push_str(&l);
                acc.push('\n');
                window = window.or_else(|| ls::pair_window_id(&l));
                match ls::pair_line(&l) {
                    PairLine::Payload => {
                        let _ = tx.send_blocking(PairMsg::Output(acc.clone()));
                    }
                    PairLine::Paired { name, .. } => break Ok(name),
                    PairLine::Expired => break Err("The pairing window closed. No device paired.".to_string()),
                    PairLine::Other => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("forgetty pair ended before a device paired.".to_string()),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = tx.send_blocking(PairMsg::Done(end));
}

impl AppShell {
    fn devices_avail(&self) -> Avail {
        match &self.lsrv.state {
            LsState::Up if self.lsrv.cli.is_some() => Avail::Yes,
            LsState::Fallback(Fallback::Disabled) => Avail::No("The local server is turned off (local_server = false in the config).".into()),
            LsState::Fallback(f) => Avail::No(f.notice().unwrap_or_default()),
            LsState::Starting => Avail::No("The local server is starting.".into()),
            _ => Avail::No("This window does not use the local server.".into()),
        }
    }

    /// The Devices page asked for work.
    pub(crate) fn devices_effect(&mut self, e: DevEffect, cx: &mut Context<Self>) {
        let avail = self.devices_avail();
        if let Some(w) = self.settings.as_mut() {
            w.st.devices.set_avail(avail.clone());
        }
        if avail != Avail::Yes {
            return;
        }
        match e {
            DevEffect::Refresh => {
                if let Some(w) = self.settings.as_mut() {
                    w.st.devices.begin_refresh();
                }
                self.devices_refresh(cx);
            }
            DevEffect::Revoke(id) => self.devices_revoke(id, cx),
            DevEffect::StartPair => self.devices_pair(cx),
            DevEffect::CancelPair => {
                if let Some(c) = self.lsrv.pair_cancel.take() {
                    c.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            DevEffect::None | DevEffect::Close | DevEffect::CopyText(_) => {}
        }
    }

    fn devices_refresh(&mut self, cx: &mut Context<Self>) {
        let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
        let store = r.store.clone();
        let task = r.rt.spawn_blocking(move || {
            let res = match cli.run(&["devices", "--json"], Duration::from_secs(20)) {
                Ok(o) if o.code == Some(0) => ls::parse_devices(&o.stdout),
                Ok(o) => Err(ls::cli_reason(&o.stdout, &o.stderr, o.code)),
                Err(e) => Err(e.to_string()),
            };
            (res, store.device_id().unwrap_or_default())
        });
        cx.spawn(async move |this, cx| {
            let (res, own) = task.await.unwrap_or_else(|e| (Err(e.to_string()), String::new()));
            let _ = this.update(cx, |s, cx| {
                if let Some(w) = s.settings.as_mut() {
                    w.st.devices.loaded(res, own);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn devices_revoke(&mut self, id: String, cx: &mut Context<Self>) {
        let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
        let name = self
            .settings
            .as_ref()
            .and_then(|w| w.st.devices.view().rows.into_iter().find(|row| id.starts_with(&row.short_id)).map(|row| row.title))
            .unwrap_or_default();
        let task = r.rt.spawn_blocking(move || match cli.run(&["revoke", &id], Duration::from_secs(20)) {
            Ok(o) if o.code == Some(0) => Ok(()),
            Ok(o) => Err(ls::cli_reason(&o.stdout, &o.stderr, o.code)),
            Err(e) => Err(e.to_string()),
        });
        cx.spawn(async move |this, cx| {
            let res = task.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |s, cx| {
                if let Some(w) = s.settings.as_mut() {
                    w.st.devices.revoked(res.map(|_| name));
                    w.st.devices.begin_refresh();
                }
                s.devices_refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn devices_pair(&mut self, cx: &mut Context<Self>) {
        let (Some(cli), Some(r)) = (self.lsrv.cli.clone(), self.remote.as_ref()) else { return };
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.lsrv.pair_cancel = Some(cancel.clone());
        let (tx, rx) = async_channel::unbounded::<PairMsg>();
        r.rt.spawn_blocking(move || pair_for_user(&cli, &cancel, &tx));
        cx.spawn(async move |this, cx| {
            while let Ok(m) = rx.recv().await {
                let done = matches!(m, PairMsg::Done(_));
                let alive = this
                    .update(cx, |s, cx| {
                        if let Some(w) = s.settings.as_mut() {
                            match m {
                                PairMsg::Output(t) => w.st.devices.pair_output(&t),
                                PairMsg::Done(r) => {
                                    let ok = r.is_ok();
                                    w.st.devices.pair_finished(r);
                                    if ok {
                                        w.st.devices.begin_refresh();
                                    }
                                }
                            }
                        }
                        if done {
                            s.lsrv.pair_cancel = None;
                            s.devices_refresh(cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive || done {
                    break;
                }
            }
        })
        .detach();
    }
}
