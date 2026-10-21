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

/// Is the real server under test?
pub fn is_real() -> bool {
    std::env::var_os("FORGETTY_REAL_SERVER").is_some()
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
        rx.recv().map_err(|_| "worker gone".to_string())?
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
        let child = Self::spawn(&dir, port, log_mb);
        let pid = child.id() as i32;
        let sock = Self::socket(dir.path());
        let t0 = std::time::Instant::now();
        let (rpc, state) = loop {
            if let Ok(rpc) = Rpc::connect(&sock) {
                if let Ok(state) = rpc.call("qa_state", json!({})) {
                    if state.get("udp_port").is_some_and(|p| !p.is_null()) {
                        break (rpc, state);
                    }
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "real server did not start; see {}",
                dir.path().join("server.log").display()
            );
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
        // SAFETY: plain signal to a child we spawned.
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
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
                // SAFETY: plain signals to a child we spawned.
                unsafe { libc::kill(pid, libc::SIGSTOP) };
                std::thread::spawn(move || {
                    std::thread::sleep(duration);
                    unsafe { libc::kill(pid, libc::SIGCONT) };
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
