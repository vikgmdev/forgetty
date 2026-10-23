//! Test server facade: the same scenarios run against the in-process test
//! server (default) and against the real `forgetty server run` process.
//!
//! Set `FORGETTY_REAL_SERVER=/path/to/forgetty` (built with `--features
//! qa-tools`) to run every scenario against the real server. Each real server
//! gets its own temp state dir (`--state-dir`), a wrapper shell that turns
//! every pane into a silent raw echo (`stty raw -echo; exec cat`), and is
//! driven over its local socket with the `qa_*` methods.
#![allow(dead_code)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use forgetty_client::proto::ServerEvent;
use forgetty_client::test_server::TestServer as Mock;
pub use forgetty_client::test_server::TestServerConfig;
use forgetty_client::{EndpointAddr, EndpointId, SecretKey};
use serde_json::{json, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Per-test watchdog
// ---------------------------------------------------------------------------
//
// A test that hangs must fail with its name, not stall a gate until an outer
// limit kills the run. `TestServer::start` arms a watchdog for the calling
// test thread. If the test is still running after `FORGETTY_TEST_TIMEOUT_SECS`
// (default 300), the watchdog prints the test name, kills the real servers of
// this process and exits with status 124.

/// Pids and state dirs of the real servers this process spawned and has not
/// dropped.
static REAL_PIDS: Mutex<Vec<(i32, PathBuf)>> = Mutex::new(Vec::new());

/// Where a test is, by test name and label, for the watchdog to print.
static PROGRESS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

/// Name of the calling test thread. Take it before spawning tasks, then pass
/// it to [`progress_for`] from them.
pub fn test_name() -> String {
    std::thread::current().name().unwrap_or("<unnamed thread>").to_string()
}

/// Record that part `label` of test `test` is now at `what`. The watchdog
/// prints these when the test times out.
pub fn progress_for(test: &str, label: &str, what: &str) {
    if let Ok(mut p) = PROGRESS.lock() {
        match p.iter_mut().find(|(t, l, _)| t == test && l == label) {
            Some(e) => e.2 = what.to_string(),
            None => p.push((test.to_string(), label.to_string(), what.to_string())),
        }
    }
}

struct WatchdogDone(Arc<std::sync::atomic::AtomicBool>);

impl Drop for WatchdogDone {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

thread_local! {
    /// Set once per test thread. Dropped when the thread (the test) ends.
    static WATCHDOG: std::cell::RefCell<Option<WatchdogDone>> =
        const { std::cell::RefCell::new(None) };
}

/// `true` if `pid` is a live child of this process: its parent in
/// `/proc/<pid>/stat` is us. A pid that was reused by another process is not,
/// so no signal of this file can reach a process it did not spawn.
pub fn is_my_child(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    // "pid (comm) S ppid ...": the fields follow the last ')'.
    let ppid = stat.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1));
    ppid.and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id())
}

/// SIGKILL/SIGCONT-style signal to a server child, only if it still is one.
fn signal_my_child(pid: i32, sig: i32) {
    if is_my_child(pid) {
        // SAFETY: plain signal to a process that is our child.
        unsafe { libc::kill(pid, sig) };
    } else {
        raw_stderr(&format!("not signalling {pid}: it is not a child of this process\n"));
    }
}

/// Write to fd 2 directly. The test harness captures `eprintln!` of threads
/// a test spawned, and this message must not be lost.
fn raw_stderr(msg: &str) {
    // SAFETY: a plain write of a valid buffer to fd 2.
    unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
}

/// Ask a real server `method` on its local socket, with 5 s limits. For the
/// watchdog report: it shows whether the server as a whole still answers.
fn probe_socket(dir: &Path, method: &str) -> String {
    let sock = Real::socket(dir);
    let run = || -> std::io::Result<String> {
        let mut stream = UnixStream::connect(&sock)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let req = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{{}},\"id\":1}}\n");
        stream.write_all(req.as_bytes())?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    };
    match run() {
        Ok(l) => l.chars().take(1500).collect(),
        Err(e) => format!("no answer: {e}"),
    }
}

fn test_timeout() -> Duration {
    let secs = std::env::var("FORGETTY_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    Duration::from_secs(secs)
}

/// Arm the watchdog for the current test thread (once).
fn arm_watchdog() {
    WATCHDOG.with(|w| {
        if w.borrow().is_some() {
            return;
        }
        let name = std::thread::current().name().unwrap_or("<unnamed thread>").to_string();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *w.borrow_mut() = Some(WatchdogDone(done.clone()));
        let limit = test_timeout();
        let spawned = std::thread::Builder::new().name(format!("watchdog:{name}")).spawn(move || {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < limit {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if done.load(Ordering::SeqCst) {
                return;
            }
            raw_stderr(&format!(
                "\nTEST TIMEOUT: `{name}` ran for more than {} s and is stuck \
                 (FORGETTY_TEST_TIMEOUT_SECS sets the limit)\n",
                limit.as_secs()
            ));
            if let Ok(p) = PROGRESS.lock() {
                for (_, label, what) in p.iter().filter(|(t, _, _)| *t == name) {
                    raw_stderr(&format!("  {label}: {what}\n"));
                }
            }
            let pids = REAL_PIDS.lock().map(|p| p.clone()).unwrap_or_default();
            for (pid, dir) in &pids {
                let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.starts_with("State:") || l.starts_with("Threads:"))
                    .collect::<Vec<_>>()
                    .join(" ");
                raw_stderr(&format!("real server {pid}: {status}\n"));
                // `dir` is the server's temp dir (state, config and log in it).
                raw_stderr(&format!("qa_state: {}\n", probe_socket(dir, "qa_state")));
                raw_stderr(&format!("list_tabs: {}\n", probe_socket(dir, "list_tabs")));
                let log = std::fs::read_to_string(dir.join("server.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(40).collect();
                raw_stderr(&format!(
                    "--- last {} lines of server.log ---\n{}\n---\n",
                    tail.len(),
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                ));
                signal_my_child(*pid, libc::SIGKILL);
            }
            std::process::exit(124);
        });
        if let Err(e) = spawned {
            eprintln!("could not start the watchdog: {e}");
        }
    });
}

/// Is the real server under test?
pub fn is_real() -> bool {
    std::env::var_os("FORGETTY_REAL_SERVER").is_some()
}

/// First line of a test that needs the real server: `if !real_only() { return; }`.
///
/// Without `FORGETTY_REAL_SERVER` the test cannot run. It then prints
/// `SKIPPED (FORGETTY_REAL_SERVER unset)` (shown with `--nocapture`, or when
/// the run fails) and returns `false`. A bare `ok` after 0.00 s proves
/// nothing, so with `FORGETTY_REQUIRE_REAL=1` (the gates set it) the test
/// fails instead.
pub fn real_only() -> bool {
    if is_real() {
        return true;
    }
    if std::env::var("FORGETTY_REQUIRE_REAL").is_ok_and(|v| v == "1") {
        panic!(
            "FORGETTY_REQUIRE_REAL=1 but FORGETTY_REAL_SERVER is unset: \
             this test needs the real server and would have been skipped"
        );
    }
    println!("SKIPPED (FORGETTY_REAL_SERVER unset)");
    false
}

/// First line of a test that needs the mock server (a delay or a hold it
/// puts on the `Pong`s: the real server has none). With the real server it
/// prints `SKIPPED (mock only)` and returns `false`.
#[allow(dead_code)]
pub fn mock_only() -> bool {
    if !is_real() {
        return true;
    }
    println!("SKIPPED (mock only)");
    false
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).expect("base64")
}

// ---------------------------------------------------------------------------
// Socket client (blocking, on its own thread)
// ---------------------------------------------------------------------------

struct Op {
    method: String,
    params: Value,
    reply: Option<mpsc::Sender<Result<Value, String>>>,
}

/// JSON-RPC over the server's local socket. One worker thread owns the
/// connection, so a frozen server blocks the worker and not the test.
struct Rpc {
    tx: mpsc::Sender<Op>,
}

/// How long `Rpc::call` waits for an answer.
const RPC_LIMIT: Duration = Duration::from_secs(120);

impl Rpc {
    fn connect(path: &Path) -> std::io::Result<Rpc> {
        let stream = UnixStream::connect(path)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        let (tx, rx) = mpsc::channel::<Op>();
        std::thread::spawn(move || {
            let mut id = 0u64;
            for op in rx {
                id += 1;
                let line =
                    json!({"jsonrpc": "2.0", "method": op.method, "params": op.params, "id": id});
                let mut out = line.to_string();
                out.push('\n');
                let result = (|| -> Result<Value, String> {
                    writer.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
                    let mut buf = String::new();
                    reader.read_line(&mut buf).map_err(|e| e.to_string())?;
                    if buf.is_empty() {
                        return Err("socket closed".into());
                    }
                    let v: Value = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
                    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                        return Err(format!("{} failed: {err}", op.method));
                    }
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                })();
                if let Some(reply) = op.reply {
                    let _ = reply.send(result);
                }
            }
        });
        Ok(Rpc { tx })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Op { method: method.into(), params, reply: Some(tx) })
            .map_err(|_| "worker gone".to_string())?;
        // A call that never gets its answer fails with its name instead of
        // blocking the test for ever.
        match rx.recv_timeout(RPC_LIMIT) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("{method}: no answer from the server in {} s", RPC_LIMIT.as_secs()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("worker gone".to_string()),
        }
    }

    fn call_timeout(&self, method: &str, params: Value, limit: Duration) -> Option<Value> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Op { method: method.into(), params, reply: Some(tx) }).ok()?;
        rx.recv_timeout(limit).ok()?.ok()
    }

    fn fire(&self, method: &str, params: Value) {
        let _ = self.tx.send(Op { method: method.into(), params, reply: None });
    }
}

// ---------------------------------------------------------------------------
// Real server
// ---------------------------------------------------------------------------

/// What survives a server stop: the state dir (identity, layout, byte logs).
#[allow(clippy::large_enum_variant)]
pub enum Saved {
    Mock { key: SecretKey, panes: Vec<(Uuid, Vec<u8>)> },
    Real { dir: Arc<tempfile::TempDir>, open_access: bool },
}

struct Real {
    child: Mutex<Option<Child>>,
    pid: i32,
    dir: Arc<tempfile::TempDir>,
    rpc: Rpc,
    node_id: EndpointId,
    port: u16,
    initial: Vec<Uuid>,
    own_runtime: bool,
    open_access: bool,
    last_handshakes: AtomicU64,
}

fn wrapper_path(dir: &Path) -> PathBuf {
    dir.join("shell.sh")
}

impl Real {
    fn binary() -> PathBuf {
        PathBuf::from(std::env::var_os("FORGETTY_REAL_SERVER").expect("FORGETTY_REAL_SERVER"))
    }

    fn spawn(dir: &Arc<tempfile::TempDir>, port: u16, log_mb: Option<u32>) -> Child {
        let root = dir.path();
        let wrapper = wrapper_path(root);
        if !wrapper.exists() {
            std::fs::write(&wrapper, "#!/bin/sh\nstty raw -echo\nexec cat\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = Command::new(Self::binary());
        cmd.arg("--state-dir").arg(root.join("state"));
        cmd.args(["server", "run", "--port", &port.to_string()]);
        if let Some(mb) = log_mb {
            let cfg = root.join("config.toml");
            std::fs::write(
                &cfg,
                format!("byte_log_max_mb = {mb}\nbyte_log_ring_kb = {}\n", (mb * 256).max(1024)),
            )
            .unwrap();
            cmd.arg("--config-file").arg(cfg);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("server.log"))
            .unwrap();
        cmd.env("SHELL", &wrapper)
            .env("RUST_LOG", std::env::var("QA_RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .stdin(Stdio::null())
            // tracing writes to stdout
            .stdout(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
            .stderr(log);
        cmd.spawn().expect("spawn forgetty")
    }

    fn socket(dir: &Path) -> PathBuf {
        dir.join("state/run/forgetty/forgetty.sock")
    }

    async fn start(
        dir: Arc<tempfile::TempDir>,
        port: u16,
        cfg_panes: &[Vec<u8>],
        authorized: Option<&HashSet<EndpointId>>,
        own_runtime: bool,
        log_mb: Option<u32>,
        open_access_override: Option<bool>,
    ) -> Real {
        let mut child = Self::spawn(&dir, port, log_mb);
        let pid = child.id() as i32;
        if let Ok(mut pids) = REAL_PIDS.lock() {
            pids.push((pid, dir.path().to_path_buf()));
        }
        let sock = Self::socket(dir.path());
        let t0 = std::time::Instant::now();
        let mut last;
        let (rpc, state) = loop {
            match Rpc::connect(&sock) {
                Ok(rpc) => match rpc.call("qa_state", json!({})) {
                    Ok(state) if state.get("udp_port").is_some_and(|p| !p.is_null()) => {
                        break (rpc, state)
                    }
                    Ok(state) => last = format!("qa_state without a UDP port: {state}"),
                    Err(e) => last = format!("qa_state failed: {e}"),
                },
                Err(e) => last = format!("socket connect failed: {e}"),
            }
            if t0.elapsed() >= Duration::from_secs(30) {
                // The temp dir (and the log with it) goes when this panics, so
                // say what the server wrote, and do not leave it running.
                let log =
                    std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(40).collect();
                let exited = child.try_wait().ok().flatten();
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "real server did not start in 30 s (last try: {last}; process exit: \
                     {exited:?}); server.log tail:\n{}",
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let node_id: EndpointId = state["node_id"].as_str().unwrap().parse().expect("node id");
        let port = state["udp_port"].as_u64().unwrap() as u16;
        let open_access = open_access_override.unwrap_or(authorized.is_none());
        rpc.call("qa_open_access", json!({"on": open_access})).unwrap();
        if let Some(set) = authorized {
            for id in set {
                rpc.call("qa_authorize", json!({"device_id": id.to_string()})).unwrap();
            }
        }
        let mut initial = Vec::new();
        if cfg_panes.is_empty() {
            // A restarted server restores its panes from the state dir.
            if let Some(panes) = state["panes"].as_array() {
                for p in panes {
                    initial.push(Uuid::parse_str(p["pane_id"].as_str().unwrap()).unwrap());
                }
            }
        }
        for log in cfg_panes {
            let r = rpc.call("new_tab", json!({"workspace_idx": 0})).expect("new_tab");
            let pane = Uuid::parse_str(r["pane_id"].as_str().unwrap()).unwrap();
            if !log.is_empty() {
                rpc.call("qa_append", json!({"pane_id": pane.to_string(), "data": b64(log)}))
                    .unwrap();
            }
            initial.push(pane);
        }
        Real {
            child: Mutex::new(Some(child)),
            pid,
            dir,
            rpc,
            node_id,
            port,
            initial,
            own_runtime,
            open_access,
            last_handshakes: AtomicU64::new(0),
        }
    }

    fn state(&self) -> Value {
        self.rpc.call("qa_state", json!({})).expect("qa_state")
    }

    fn pane_arg(pane: Uuid) -> Value {
        json!({"pane_id": pane.to_string()})
    }

    /// SIGTERM, then wait for the process to exit (SIGKILL after 15 s).
    async fn terminate(&self) {
        signal_my_child(self.pid, libc::SIGTERM);
        let t0 = std::time::Instant::now();
        loop {
            {
                let mut guard = self.child.lock().unwrap();
                match guard.as_mut() {
                    None => return,
                    Some(child) => {
                        if child.try_wait().ok().flatten().is_some() {
                            guard.take();
                            return;
                        }
                        if t0.elapsed() > Duration::from_secs(15) {
                            let _ = child.kill();
                            let _ = child.wait();
                            guard.take();
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        if let Ok(mut pids) = REAL_PIDS.lock() {
            pids.retain(|(p, _)| *p != self.pid);
        }
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::env::var_os("QA_SHOW_SERVER_LOG").is_some() {
            let log =
                std::fs::read_to_string(self.dir.path().join("server.log")).unwrap_or_default();
            eprintln!("--- server.log ---\n{log}");
        }
    }
}

// ---------------------------------------------------------------------------
// Facade
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum Inner {
    Mock { server: Mock, pair_secret: Option<String>, initial: Vec<Uuid>, key: SecretKey },
    Real(Box<Real>),
}

/// The server under test: mock or real, same methods.
pub struct TestServer {
    inner: Inner,
}

impl TestServer {
    pub async fn start(cfg: TestServerConfig) -> TestServer {
        Self::start_with(cfg, None).await
    }

    /// Like `start`, with the real server's byte log cap set to `log_mb` MiB.
    pub async fn start_with(cfg: TestServerConfig, log_mb: Option<u32>) -> TestServer {
        arm_watchdog();
        if !is_real() {
            let mut cfg = cfg;
            let key =
                cfg.secret_key.get_or_insert_with(|| SecretKey::generate(&mut rand::rng())).clone();
            let pair_secret = cfg.pair_secret.clone();
            let initial = cfg.panes.iter().map(|(id, _)| *id).collect();
            return TestServer {
                inner: Inner::Mock { server: Mock::start(cfg).await, pair_secret, initial, key },
            };
        }
        let dir = Arc::new(tempfile::tempdir().expect("temp dir"));
        let logs: Vec<Vec<u8>> = cfg.panes.iter().map(|(_, l)| l.clone()).collect();
        let real = Real::start(
            dir,
            cfg.port.unwrap_or(0),
            &logs,
            cfg.authorized.as_ref(),
            cfg.own_runtime,
            log_mb,
            None,
        )
        .await;
        TestServer { inner: Inner::Real(Box::new(real)) }
    }

    /// A new server process on `port` with the state of a stopped one.
    pub async fn start_again(saved: Saved, port: u16) -> TestServer {
        arm_watchdog();
        match saved {
            Saved::Mock { key, panes } => {
                let cfg = TestServerConfig {
                    port: Some(port),
                    secret_key: Some(key),
                    panes,
                    ..Default::default()
                };
                Self::start(cfg).await
            }
            Saved::Real { dir, open_access } => {
                let real = Real::start(dir, port, &[], None, false, None, Some(open_access)).await;
                TestServer { inner: Inner::Real(Box::new(real)) }
            }
        }
    }

    /// Stop the server (SIGTERM for the real one) and keep what a restart needs.
    pub async fn stop_for_restart(self) -> Saved {
        match self.inner {
            Inner::Mock { server, initial, key, .. } => {
                let mut panes = Vec::new();
                for id in initial {
                    panes.push((id, server.log(id)));
                }
                server.shutdown().await;
                Saved::Mock { key, panes }
            }
            Inner::Real(real) => {
                real.terminate().await;
                Saved::Real { dir: real.dir.clone(), open_access: real.open_access }
            }
        }
    }

    pub async fn shutdown(self) {
        match self.inner {
            Inner::Mock { server, .. } => server.shutdown().await,
            Inner::Real(real) => real.terminate().await,
        }
    }

    pub fn is_real(&self) -> bool {
        matches!(self.inner, Inner::Real(_))
    }

    /// The real server's `--state-dir` (`None` for the mock).
    pub fn state_dir(&self) -> Option<PathBuf> {
        match &self.inner {
            Inner::Real(r) => Some(r.dir.path().join("state")),
            Inner::Mock { .. } => None,
        }
    }

    pub fn node_id(&self) -> EndpointId {
        match &self.inner {
            Inner::Mock { server, .. } => server.addr().id,
            Inner::Real(r) => r.node_id,
        }
    }

    pub fn addr(&self) -> EndpointAddr {
        match &self.inner {
            Inner::Mock { server, .. } => server.addr(),
            Inner::Real(r) => EndpointAddr::new(r.node_id)
                .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], r.port))),
        }
    }

    pub fn udp_addr(&self) -> std::net::SocketAddr {
        match &self.inner {
            Inner::Mock { server, .. } => server.udp_addr(),
            Inner::Real(r) => std::net::SocketAddr::from(([127, 0, 0, 1], r.port)),
        }
    }

    /// Pane `i` of `TestServerConfig::panes` (the real server picks its own ids).
    pub fn initial_pane(&self, i: usize) -> Uuid {
        match &self.inner {
            Inner::Mock { initial, .. } => initial[i],
            Inner::Real(r) => r.initial[i],
        }
    }

    /// The pairing secret: the configured one (mock) or a fresh window (real).
    pub fn pair_secret(&self) -> String {
        match &self.inner {
            Inner::Mock { pair_secret, .. } => pair_secret.clone().expect("pair_secret configured"),
            Inner::Real(r) => {
                let v = r.rpc.call("pair_open", json!({})).expect("pair_open");
                v["payload"]["secret"].as_str().expect("secret").to_string()
            }
        }
    }

    pub fn add_pane(&self) -> Uuid {
        match &self.inner {
            Inner::Mock { server, .. } => server.add_pane(),
            Inner::Real(r) => {
                let v = r.rpc.call("new_tab", json!({"workspace_idx": 0})).expect("new_tab");
                Uuid::parse_str(v["pane_id"].as_str().unwrap()).unwrap()
            }
        }
    }

    /// A pane running `script` (`sh -c`) in the real server. The test server has
    /// no processes, so it gets a plain pane.
    pub fn add_script_pane(&self, script: &str) -> Uuid {
        match &self.inner {
            Inner::Mock { server, .. } => server.add_pane(),
            Inner::Real(r) => {
                let v = r
                    .rpc
                    .call(
                        "new_tab",
                        json!({"workspace_idx": 0, "command": ["/bin/sh", "-c", script]}),
                    )
                    .expect("new_tab");
                Uuid::parse_str(v["pane_id"].as_str().unwrap()).unwrap()
            }
        }
    }

    /// A pane that writes `bytes` of output when it gets one line of input.
    pub fn add_burst_pane(&self, bytes: usize) -> Uuid {
        match &self.inner {
            Inner::Mock { server, .. } => server.add_pane(),
            Inner::Real(r) => {
                let script = format!(
                    "stty raw -echo; read -r l; yes 0123456789abcdef | head -c {bytes}; exec cat"
                );
                let v = r
                    .rpc
                    .call(
                        "new_tab",
                        json!({"workspace_idx": 0, "command": ["/bin/sh", "-c", script]}),
                    )
                    .expect("new_tab");
                Uuid::parse_str(v["pane_id"].as_str().unwrap()).unwrap()
            }
        }
    }

    /// Start the output of a burst pane.
    pub fn start_burst(&self, pane: Uuid, bytes: usize) {
        match &self.inner {
            Inner::Mock { server, .. } => {
                let data: Vec<u8> = (0..bytes as u64).map(burst_byte).collect();
                server.append(pane, &data)
            }
            Inner::Real(r) => {
                let mut arg = Real::pane_arg(pane);
                arg["data"] = json!(b64(b"\n"));
                r.rpc.call("send_input", arg).expect("send_input");
            }
        }
    }

    pub fn append(&self, pane: Uuid, bytes: &[u8]) {
        match &self.inner {
            Inner::Mock { server, .. } => server.append(pane, bytes),
            Inner::Real(r) => {
                let mut arg = Real::pane_arg(pane);
                arg["data"] = json!(b64(bytes));
                if r.own_runtime {
                    r.rpc.fire("qa_append", arg);
                } else {
                    r.rpc.call("qa_append", arg).expect("qa_append");
                }
            }
        }
    }

    pub fn close_pane(&self, pane: Uuid) {
        match &self.inner {
            Inner::Mock { server, .. } => server.close_pane(pane),
            Inner::Real(r) => {
                r.rpc.call("close_pane", Real::pane_arg(pane)).expect("close_pane");
            }
        }
    }

    pub fn received_input(&self, pane: Uuid) -> Vec<u8> {
        match &self.inner {
            Inner::Mock { server, .. } => server.received_input(pane),
            Inner::Real(r) => {
                let v = r.rpc.call("qa_input", Real::pane_arg(pane)).expect("qa_input");
                unb64(v["data"].as_str().unwrap())
            }
        }
    }

    pub fn log(&self, pane: Uuid) -> Vec<u8> {
        match &self.inner {
            Inner::Mock { server, .. } => server.log(pane),
            Inner::Real(r) => {
                let v = r.rpc.call("qa_read_log", Real::pane_arg(pane)).expect("qa_read_log");
                unb64(v["data"].as_str().unwrap())
            }
        }
    }

    pub fn pane_size(&self, pane: Uuid) -> (u16, u16) {
        match &self.inner {
            Inner::Mock { server, .. } => server.pane_size(pane),
            Inner::Real(r) => {
                let st = r.state();
                let p = st["panes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["pane_id"] == pane.to_string())
                    .expect("pane in qa_state");
                (p["rows"].as_u64().unwrap() as u16, p["cols"].as_u64().unwrap() as u16)
            }
        }
    }

    /// `(first, total)` byte offsets of the pane log.
    pub fn log_bounds(&self, pane: Uuid) -> (u64, u64) {
        match &self.inner {
            Inner::Mock { server, .. } => (0, server.total_written(pane)),
            Inner::Real(r) => {
                let st = r.state();
                let p = st["panes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["pane_id"] == pane.to_string())
                    .expect("pane in qa_state");
                (p["first"].as_u64().unwrap(), p["total"].as_u64().unwrap())
            }
        }
    }

    pub fn attaches(&self) -> Vec<(Uuid, Option<u64>)> {
        match &self.inner {
            Inner::Mock { server, .. } => server.attaches(),
            Inner::Real(r) => r.state()["attaches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| (Uuid::parse_str(a[0].as_str().unwrap()).unwrap(), a[1].as_u64()))
                .collect(),
        }
    }

    pub fn handshakes(&self) -> u64 {
        match &self.inner {
            Inner::Mock { server, .. } => server.handshakes(),
            Inner::Real(r) => {
                // A frozen server cannot answer: report the last known count.
                let v = if r.own_runtime {
                    r.rpc.call_timeout("qa_state", json!({}), Duration::from_millis(20))
                } else {
                    r.rpc.call("qa_state", json!({})).ok()
                };
                match v {
                    Some(v) => {
                        let n = v["handshakes"].as_u64().unwrap();
                        r.last_handshakes.store(n, Ordering::SeqCst);
                        n
                    }
                    None => r.last_handshakes.load(Ordering::SeqCst),
                }
            }
        }
    }

    /// Client connections open at the server now.
    pub fn live_connections(&self) -> usize {
        match &self.inner {
            Inner::Mock { server, .. } => server.live_connections(),
            Inner::Real(r) => r.state()["connections"].as_u64().unwrap() as usize,
        }
    }

    /// Wait until the server holds no client connection. A test that changes
    /// the log while a client is away needs this: a live connection would
    /// see the change as a stream, not as a resume.
    pub async fn wait_no_live_connection(&self) {
        let t0 = std::time::Instant::now();
        while self.live_connections() != 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{} connection(s) still open at the server",
                self.live_connections()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Wait until the program of a pane reads its input raw. A fresh real pane
    /// runs `stty raw -echo; exec cat`; input typed before that is echoed by
    /// the terminal as well as by `cat`. The test server has no program.
    pub async fn wait_pane_ready(&self, pane: Uuid) {
        let Inner::Real(r) = &self.inner else { return };
        let t0 = std::time::Instant::now();
        loop {
            let pid = r.state()["pane_input"]
                .as_array()
                .and_then(|a| {
                    a.iter().find(|p| p["pane_id"].as_str() == Some(&pane.to_string())).cloned()
                })
                .and_then(|p| p["child_pid"].as_i64());
            if let Some(pid) = pid {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                if comm.trim() == "cat" {
                    return;
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "the pane program never started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Handshakes the server answered with `server_stopping` (real server).
    pub fn stopping_refusals(&self) -> u64 {
        match &self.inner {
            Inner::Mock { .. } => 0,
            Inner::Real(r) => r.state()["stopping_refusals"].as_u64().unwrap(),
        }
    }

    pub fn dedup_drops(&self) -> u64 {
        match &self.inner {
            Inner::Mock { server, .. } => server.dedup_drops(),
            Inner::Real(r) => r.state()["dedup_drops"].as_u64().unwrap(),
        }
    }

    pub fn hold_acks(&self, hold: bool) {
        match &self.inner {
            Inner::Mock { server, .. } => server.hold_acks(hold),
            Inner::Real(r) => {
                r.rpc.call("qa_hold_acks", json!({"hold": hold})).unwrap();
            }
        }
    }

    /// Mock only: hold every `Pong` until this long after the `Hello`.
    pub fn set_pong_hold(&self, hold: Duration) {
        match &self.inner {
            Inner::Mock { server, .. } => server.set_pong_hold(hold),
            Inner::Real(_) => panic!("set_pong_hold needs the mock server"),
        }
    }

    /// Mock only: what new connections do after the `Welcome`.
    pub fn set_die_mode(&self, mode: u8) {
        match &self.inner {
            Inner::Mock { server, .. } => server.set_die_mode(mode),
            Inner::Real(_) => panic!("set_die_mode is a mock server option"),
        }
    }

    pub fn pause_handshakes(&self, paused: bool) {
        match &self.inner {
            Inner::Mock { server, .. } => server.pause_handshakes(paused),
            Inner::Real(r) => {
                r.rpc.call("qa_pause_handshakes", json!({"paused": paused})).unwrap();
            }
        }
    }

    pub fn drop_connections(&self) {
        match &self.inner {
            Inner::Mock { server, .. } => server.drop_connections(),
            Inner::Real(r) => {
                r.rpc.call("qa_drop_connections", json!({})).unwrap();
            }
        }
    }

    pub fn push_event(&self, event: ServerEvent) {
        match &self.inner {
            Inner::Mock { server, .. } => server.push_event(event),
            Inner::Real(r) => {
                r.rpc
                    .call("qa_push_event", json!({"event": serde_json::to_value(&event).unwrap()}))
                    .unwrap();
            }
        }
    }

    /// Emit `count` LayoutChanged events at the server (real server only).
    pub fn fire_layout_events(&self, count: u64) {
        if let Inner::Real(r) = &self.inner {
            r.rpc.call("qa_fire_events", json!({"count": count})).unwrap();
        }
    }

    /// Resident memory of the real server process, in KiB.
    pub fn rss_kib(&self) -> u64 {
        let pid = self.pid().expect("real server");
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    }

    pub fn truncate_log_before(&self, pane: Uuid, offset: u64) {
        match &self.inner {
            Inner::Mock { server, .. } => server.truncate_log_before(pane, offset),
            Inner::Real(r) => {
                let mut arg = Real::pane_arg(pane);
                arg["offset"] = json!(offset);
                r.rpc.call("qa_truncate_log_before", arg).unwrap();
            }
        }
    }

    pub fn authorize(&self, id: EndpointId) {
        match &self.inner {
            Inner::Mock { server, .. } => server.authorize(id),
            Inner::Real(r) => {
                r.rpc.call("qa_authorize", json!({"device_id": id.to_string()})).unwrap();
            }
        }
    }

    /// Stop the server for `duration` without closing anything. The mock needs
    /// `own_runtime`; the real one gets SIGSTOP, then SIGCONT.
    pub fn freeze(&self, duration: Duration) {
        match &self.inner {
            Inner::Mock { server, .. } => server.freeze(duration),
            Inner::Real(r) => {
                let pid = r.pid;
                signal_my_child(pid, libc::SIGSTOP);
                // `kill` returns when the signal is queued, not when the
                // process has stopped: on a busy host a Ping can still be
                // answered meanwhile. Return when every thread is stopped.
                let t0 = std::time::Instant::now();
                loop {
                    let stopped = std::fs::read_dir(format!("/proc/{pid}/task")).is_ok_and(|d| {
                        d.flatten().all(|e| {
                            let stat = std::fs::read_to_string(e.path().join("stat"))
                                .unwrap_or_default();
                            // "pid (comm) S ...": the state follows the last ')'.
                            stat.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('T'))
                        })
                    });
                    if stopped {
                        break;
                    }
                    if t0.elapsed() > Duration::from_secs(5) {
                        // Do not leave the server stopped, and do not run a
                        // freeze test against a server that never froze.
                        signal_my_child(pid, libc::SIGCONT);
                        panic!("server {pid} did not stop within 5 s of SIGSTOP");
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                std::thread::spawn(move || {
                    std::thread::sleep(duration);
                    // The server may be gone by now and its pid reused.
                    signal_my_child(pid, libc::SIGCONT);
                });
            }
        }
    }

    /// The real server's pid (SIGTERM tests).
    pub fn pid(&self) -> Option<i32> {
        match &self.inner {
            Inner::Real(r) => Some(r.pid),
            Inner::Mock { .. } => None,
        }
    }

    /// Path of the real server's local socket.
    pub fn socket_path(&self) -> PathBuf {
        match &self.inner {
            Inner::Real(r) => Real::socket(r.dir.path()),
            Inner::Mock { .. } => panic!("mock server has no socket"),
        }
    }

    /// Run the real `forgetty` CLI against this server (`--state-dir`).
    /// Returns its stdout, or its stderr and exit status on failure.
    pub fn cli(&self, args: &[&str]) -> Result<String, String> {
        let Inner::Real(r) = &self.inner else { panic!("mock server has no CLI") };
        let out = Command::new(Real::binary())
            .arg("--state-dir")
            .arg(r.dir.path().join("state"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("{}: {}", out.status, String::from_utf8_lossy(&out.stderr)))
        }
    }

    /// Call a method on the real server's local socket.
    pub fn socket_call(&self, method: &str, params: Value) -> Result<Value, String> {
        match &self.inner {
            Inner::Real(r) => r.rpc.call(method, params),
            Inner::Mock { .. } => Err("mock server has no socket".into()),
        }
    }
}

/// Byte `i` of a burst pane's output: the line `0123456789abcdef\n` repeated.
pub fn burst_byte(i: u64) -> u8 {
    b"0123456789abcdef\n"[(i % 17) as usize]
}
