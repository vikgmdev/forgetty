//! CLI half of the `forgetty` binary. Talks to the server over the local
//! Unix socket, or the named pipe on Windows (JSON-RPC, one request per line).
//! Blocking and synchronous.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

#[cfg(windows)]
use forgetty_server::pipe_client::PipeStream;
#[cfg(unix)]
use forgetty_server::sockpath::{self, SocketCheck};
#[cfg(unix)]
use forgetty_server::sockpath::default_socket_path;

/// The connection to the server.
#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = PipeStream;

pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_FOUND: i32 = 3;
pub const EXIT_UNREACHABLE: i32 = 4;
/// `forgetty wait` ran out of time.
pub const EXIT_TIMEOUT: i32 = 5;

/// JSON-RPC error code the server uses for "clients attached".
const SERVER_BUSY: i64 = -32001;

/// Minimum length of a pane id suffix.
const MIN_SUFFIX: usize = 4;

#[derive(Debug)]
pub enum CliError {
    /// Bad arguments that clap could not catch (exit 2).
    Usage(String),
    /// Command ran but failed (exit 1).
    Failed(String),
    /// Pane or device target missing or ambiguous (exit 3).
    NotFound(String),
    /// No server on the socket (exit 4).
    Unreachable(String),
}

impl CliError {
    fn code(&self) -> i32 {
        match self {
            CliError::Usage(_) => EXIT_USAGE,
            CliError::Failed(_) => EXIT_FAILED,
            CliError::NotFound(_) => EXIT_NOT_FOUND,
            CliError::Unreachable(_) => EXIT_UNREACHABLE,
        }
    }

    fn message(&self) -> &str {
        match self {
            CliError::Usage(m)
            | CliError::Failed(m)
            | CliError::NotFound(m)
            | CliError::Unreachable(m) => m,
        }
    }
}

type Result<T> = std::result::Result<T, CliError>;

/// Turn a command result into an exit code, printing the error to stderr.
pub fn finish(result: Result<()>) -> i32 {
    finish_code(result.map(|()| 0))
}

/// Like [`finish`] for commands that choose their own exit code on success
/// (`run --wait`, `wait`).
pub fn finish_code(result: Result<i32>) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("forgetty: {}", e.message());
            e.code()
        }
    }
}

// ---------------------------------------------------------------------------
// Socket client
// ---------------------------------------------------------------------------

/// A socket path as it goes into a message. A pipe name holds a secret, which stays out.
fn shown_path(path: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        forgetty_server::sockpath::redact_pipe_name(&path.to_string_lossy())
    }
    #[cfg(unix)]
    {
        path.display().to_string()
    }
}

struct Client {
    reader: BufReader<Stream>,
    writer: Stream,
    next_id: u64,
}

/// What `server_info` returned.
struct Info(Value);

impl Info {
    fn str(&self, key: &str) -> String {
        self.0.get(key).and_then(Value::as_str).unwrap_or("?").to_string()
    }
    fn num(&self, key: &str) -> u64 {
        self.0.get(key).and_then(Value::as_u64).unwrap_or(0)
    }
}

impl Client {
    /// Connect without any version check.
    fn raw() -> Result<Self> {
        Self::raw_for(false)
    }

    /// `shutdown_only`: Windows only. The connection is for the `shutdown` call alone and accepts
    /// a server at a lower integrity level than this process (an elevated installer stopping
    /// the server of the user). See `PipeStream::connect_for_shutdown`.
    fn raw_for(shutdown_only: bool) -> Result<Self> {
        let _ = shutdown_only;
        #[cfg(unix)]
        let path = default_socket_path();
        // The pipe name holds a secret kept in a file. No file means no server has started here.
        #[cfg(windows)]
        let path = forgetty_server::sockpath::default_socket_path().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::Unreachable(
                    "server not reachable. Start it with `forgetty server start`.".to_string(),
                )
            } else {
                CliError::Failed(format!("cannot find the server pipe: {e}"))
            }
        })?;
        // Never talk to a socket we cannot trust: a squatter could answer with
        // a fake server or read what we send. (On Windows `PipeStream::connect` checks the
        // user of the server process instead.)
        #[cfg(unix)]
        match sockpath::verify_socket_path(&path, sockpath::current_uid()) {
            SocketCheck::Ok => {}
            SocketCheck::Missing => {
                return Err(CliError::Unreachable(format!(
                    "server not reachable at {}. Start it with `forgetty server start`.",
                    shown_path(&path)
                )))
            }
            SocketCheck::Unsafe(why) => {
                return Err(CliError::Failed(format!("refusing to use the server socket: {why}")))
            }
        }
        #[cfg(windows)]
        let connected = if shutdown_only {
            Stream::connect_for_shutdown(&path)
        } else {
            Stream::connect(&path)
        };
        #[cfg(unix)]
        let connected = Stream::connect(&path);
        let stream = connected.map_err(|e| {
            // A pipe server of another user is not "not running": do not start over it.
            #[cfg(windows)]
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return CliError::Failed(format!("refusing to use the server pipe: {e}"));
            }
            CliError::Unreachable(format!(
                "server not reachable at {} ({e}). Start it with `forgetty server start`.",
                shown_path(&path)
            ))
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(15))).ok();
        let writer = stream.try_clone().map_err(|e| CliError::Failed(e.to_string()))?;
        Ok(Self { reader: BufReader::new(stream), writer, next_id: 1 })
    }

    /// Connect, read `server_info` and warn on stderr about a version mismatch.
    fn connect() -> Result<(Self, Info)> {
        let mut client = Self::raw()?;
        let info = Info(client.call("server_info", Value::Null)?);
        let mine = env!("CARGO_PKG_VERSION");
        let theirs = info.str("version");
        if theirs != mine {
            eprintln!(
                "forgetty: warning: server is version {theirs}, this CLI is {mine}. \
                 Restart the server (`forgetty server stop && forgetty server start`)."
            );
        }
        if info.num("proto_major") != u64::from(forgetty_proto_major()) {
            eprintln!(
                "forgetty: warning: server speaks protocol {}, this CLI speaks {}.",
                info.str("proto_version"),
                forgetty_proto_major()
            );
        }
        Ok((client, info))
    }

    /// Send one request and return the raw JSON-RPC reply: the result, or the
    /// error code and message.
    fn call_rpc(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<std::result::Result<Value, (i64, String)>> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id });
        let mut line = req.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.flush())
            .map_err(|e| CliError::Unreachable(format!("server connection lost: {e}")))?;

        let mut buf = String::new();
        let n = self
            .reader
            .read_line(&mut buf)
            .map_err(|e| CliError::Unreachable(format!("server connection lost: {e}")))?;
        if n == 0 {
            return Err(CliError::Unreachable("server closed the connection".into()));
        }
        let resp: Value = serde_json::from_str(buf.trim())
            .map_err(|e| CliError::Failed(format!("bad server reply: {e}")))?;
        if let Some(err) = resp.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            return Ok(Err((code, msg.to_string())));
        }
        Ok(Ok(resp.get("result").cloned().unwrap_or(Value::Null)))
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        match self.call_rpc(method, params)? {
            Ok(v) => Ok(v),
            Err((code, msg)) => Err(if code == SERVER_BUSY {
                CliError::Failed(format!("server is busy: {msg}"))
            } else {
                CliError::Failed(format!("{method}: {msg}"))
            }),
        }
    }
}

impl Client {
    /// Read one line from a stream (after `subscribe_layout`). Waits up to
    /// `wait`. `pending` holds a partial line across calls. `Ok(None)` is a
    /// timeout, an error is a closed connection.
    fn poll_line(&mut self, pending: &mut Vec<u8>, wait: Duration) -> Result<Option<Value>> {
        self.reader.get_ref().set_read_timeout(Some(wait)).ok();
        loop {
            match self.reader.read_until(b'\n', pending) {
                Ok(0) => return Err(CliError::Unreachable("server closed the connection".into())),
                Ok(_) => {
                    if pending.ends_with(b"\n") {
                        let line = std::mem::take(pending);
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim();
                        if text.is_empty() {
                            continue;
                        }
                        return serde_json::from_str(text)
                            .map(Some)
                            .map_err(|e| CliError::Failed(format!("bad server event: {e}")));
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(CliError::Unreachable(format!("server connection lost: {e}")))
                }
            }
        }
    }

    /// Subscribe to the server event stream on a new connection.
    fn subscribe() -> Result<Self> {
        let mut c = Self::raw()?;
        c.call("subscribe_layout", Value::Null)?;
        Ok(c)
    }
}

fn forgetty_proto_major() -> u32 {
    forgetty_socket::protocol::PROTO_MAJOR
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// A thing a target can name: its id and its label (empty when none).
struct Cand {
    id: String,
    label: String,
}

impl Cand {
    fn show(&self) -> String {
        if self.label.is_empty() {
            self.id.clone()
        } else {
            format!("{} ({})", self.id, self.label)
        }
    }
}

/// Resolve `target` against `cands`, in this order: a full id, a label
/// (case-insensitive), a 1-based position, a unique id suffix of 4+ chars.
/// Several labels or suffixes that match give exit 3 with the list.
fn resolve_cands(what: &str, target: &str, cands: &[Cand]) -> Result<String> {
    let target = target.trim();
    let lower = target.to_ascii_lowercase();
    if let Some(exact) = cands.iter().find(|c| c.id.to_ascii_lowercase() == lower) {
        return Ok(exact.id.clone());
    }
    if !target.is_empty() {
        let hits: Vec<&Cand> = cands
            .iter()
            .filter(|c| !c.label.is_empty() && c.label.to_ascii_lowercase() == lower)
            .collect();
        match hits.as_slice() {
            [] => {}
            [one] => return Ok(one.id.clone()),
            many => {
                return Err(CliError::NotFound(format!(
                    "{what} label {target:?} is ambiguous ({} matches: {})",
                    many.len(),
                    many.iter().map(|c| c.show()).collect::<Vec<_>>().join(", ")
                )))
            }
        }
    }
    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = target.parse::<usize>() {
            if (1..=cands.len()).contains(&n) {
                return Ok(cands[n - 1].id.clone());
            }
        }
    }
    if lower.len() < MIN_SUFFIX {
        return Err(CliError::Usage(format!(
            "{what} target {target:?} is not an id, a label or a position. \
             Give the full id, a label, a position or an id suffix of {MIN_SUFFIX}+ characters"
        )));
    }
    let hits: Vec<&Cand> =
        cands.iter().filter(|c| c.id.to_ascii_lowercase().ends_with(&lower)).collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(CliError::NotFound(format!("no {what} matches {target:?}"))),
        many => Err(CliError::NotFound(format!(
            "{what} target {target:?} is ambiguous ({} matches: {})",
            many.len(),
            many.iter().map(|c| c.show()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// Resolve against ids only (devices).
fn resolve_target(what: &str, target: &str, ids: &[String]) -> Result<String> {
    let cands: Vec<Cand> =
        ids.iter().map(|id| Cand { id: id.clone(), label: String::new() }).collect();
    resolve_cands(what, target, &cands)
}

fn pane_cands(client: &mut Client) -> Result<Vec<Cand>> {
    let tabs = client.call("list_tabs", Value::Null)?;
    Ok(tabs
        .get("tabs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let id = t.get("pane_id").and_then(Value::as_str)?;
                    let label = t.get("label").and_then(Value::as_str).unwrap_or("");
                    Some(Cand { id: id.to_string(), label: label.to_string() })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn resolve_pane(client: &mut Client, target: &str) -> Result<String> {
    let cands = pane_cands(client)?;
    resolve_cands("pane", target, &cands)
}

/// Tabs of all workspaces in layout order, and the workspaces.
fn layout_cands(client: &mut Client) -> Result<(Vec<Cand>, Vec<Cand>)> {
    let layout = client.call("get_layout", Value::Null)?;
    let mut tabs = Vec::new();
    let mut workspaces = Vec::new();
    for ws in layout.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        if let Some(id) = ws.get("id").and_then(Value::as_str) {
            let name = ws.get("name").and_then(Value::as_str).unwrap_or("");
            workspaces.push(Cand { id: id.to_string(), label: name.to_string() });
        }
        for tab in ws.get("tabs").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = tab.get("id").and_then(Value::as_str) {
                let label = tab.get("label").and_then(Value::as_str).unwrap_or("");
                tabs.push(Cand { id: id.to_string(), label: label.to_string() });
            }
        }
    }
    Ok((tabs, workspaces))
}

// ---------------------------------------------------------------------------
// server start / stop / status
// ---------------------------------------------------------------------------

/// `forgetty server run` exit code when another server already runs (EX_TEMPFAIL).
const EXIT_ALREADY_RUNNING: i32 = 75;
const EXIT_CONFIG: i32 = 78; // keep in step with server.rs

/// The `forgetty server: ...` error the server wrote to the log after byte
/// `from`, or a pointer to the log.
fn config_error_from_log(log_path: &std::path::Path, from: u64) -> String {
    let text = std::fs::read(log_path)
        .map(|b| String::from_utf8_lossy(b.get(from as usize..).unwrap_or(&[])).into_owned())
        .unwrap_or_default();
    match text.find("forgetty server: ") {
        Some(i) => text[i + "forgetty server: ".len()..].trim_end().to_string(),
        None => format!("server stopped on a configuration error. See {}", log_path.display()),
    }
}

pub fn server_start() -> Result<()> {
    match Client::connect() {
        Ok((_, info)) => {
            println!("server already running (pid {})", info.num("pid"));
            return Ok(());
        }
        // No server yet: go on and start one.
        Err(CliError::Unreachable(_)) => {}
        // An unsafe socket or a broken reply: do not start over it.
        Err(e) => return Err(e),
    }

    let exe = std::env::current_exe().map_err(|e| CliError::Failed(e.to_string()))?;
    let data = forgetty_core::platform::data_dir();
    forgetty_core::fsutil::ensure_private_dir(&data)
        .map_err(|e| CliError::Failed(e.to_string()))?;
    let log_path: PathBuf = data.join("server.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| CliError::Failed(format!("open {}: {e}", log_path.display())))?;
    let log_err = log.try_clone().map_err(|e| CliError::Failed(e.to_string()))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "run"]).stdin(std::process::Stdio::null()).stdout(log).stderr(log_err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    // Where this run's output starts, to read its error back on a config exit.
    let log_start = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let mut child = spawn_detached(&mut cmd)?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match Client::connect() {
            Ok((_, info)) => {
                println!("server started (pid {})", info.num("pid"));
                return Ok(());
            }
            Err(CliError::Unreachable(_)) => {}
            Err(e) => return Err(e),
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Another `server start` won the race: its server is on the way up.
            // Keep waiting for it instead of reporting a failure.
            if status.code() == Some(EXIT_ALREADY_RUNNING) {
                if Instant::now() >= deadline {
                    return Err(CliError::Failed(
                        "another server holds the lock but did not come up within 20 s".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if status.code() == Some(EXIT_CONFIG) {
                // A setting the server refuses: say which, not "see the log".
                return Err(CliError::Failed(config_error_from_log(&log_path, log_start)));
            }
            return Err(CliError::Failed(format!(
                "server exited at startup ({status}). See {}",
                log_path.display()
            )));
        }
        if Instant::now() >= deadline {
            return Err(CliError::Failed(format!(
                "server did not come up within 20 s. See {}",
                log_path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Start the server process so that it outlives this one and the terminal it was started from.
#[cfg(unix)]
fn spawn_detached(cmd: &mut std::process::Command) -> Result<std::process::Child> {
    cmd.spawn().map_err(|e| CliError::Failed(format!("spawn server: {e}")))
}

/// Start the server process so that it outlives this one and the terminal it was started from:
/// no console, its own process group (a Ctrl+C in the shell does not reach it), and out of the
/// job of the caller (a terminal or an installer that kills its job on exit would take it down).
#[cfg(windows)]
fn spawn_detached(cmd: &mut std::process::Command) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    // `CreateProcess` hands every inheritable handle of this process to the child. Our own
    // stdout may be a pipe that a caller reads until it closes (a script, a test, an editor):
    // if the server kept it, that caller would wait for the server to exit. So the standard
    // handles are not inheritable while the server starts. The log file the server writes to
    // is passed explicitly by `cmd`, and std makes its own inheritable copy of it.
    let _guard = StdHandlesPrivate::new();
    match cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(child) => Ok(child),
        // The job of the caller does not allow a breakaway: start without it. The server is
        // then part of that job, which is what the job asked for.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => cmd
            .creation_flags(base)
            .spawn()
            .map_err(|e| CliError::Failed(format!("spawn server: {e}"))),
        Err(e) => Err(CliError::Failed(format!("spawn server: {e}"))),
    }
}

/// Makes the standard handles of this process non-inheritable until dropped.
#[cfg(windows)]
struct StdHandlesPrivate(Vec<(windows::Win32::Foundation::HANDLE, u32)>);

#[cfg(windows)]
impl StdHandlesPrivate {
    fn new() -> Self {
        use windows::Win32::Foundation::{
            GetHandleInformation, SetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
        };
        use windows::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain calls on the handles the system gave this process.
            unsafe {
                let Ok(h) = GetStdHandle(which) else { continue };
                if h.is_invalid() {
                    continue;
                }
                let mut flags = 0u32;
                if GetHandleInformation(h, &mut flags).is_err() || flags & HANDLE_FLAG_INHERIT.0 == 0 {
                    continue;
                }
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).is_ok() {
                    saved.push((h, flags));
                }
            }
        }
        Self(saved)
    }
}

#[cfg(windows)]
impl Drop for StdHandlesPrivate {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
        for (h, flags) in &self.0 {
            // SAFETY: the handles were valid when saved and this process still owns them.
            unsafe {
                let _ = SetHandleInformation(*h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(*flags));
            }
        }
    }
}

/// Does a process with this id exist?
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    forgetty_session::input_watch::child_alive(pid)
}

pub fn server_stop(if_idle: bool) -> Result<()> {
    // Windows: the call is `shutdown` alone, on a connection that accepts a server below our
    // integrity level (msiexec runs elevated, the server never does). The pid comes from the
    // pipe, not from the server's own answer. No `server_info` is asked or trusted.
    #[cfg(windows)]
    let (mut client, pid) = {
        let client = Client::raw_for(true)?;
        let pid = client.writer.server_pid().map_err(|e| CliError::Failed(e.to_string()))?;
        (client, pid)
    };
    #[cfg(unix)]
    let (mut client, pid) = {
        let (client, info) = Client::connect()?;
        (client, info.num("pid") as u32)
    };
    client.call("shutdown", json!({ "if_idle": if_idle }))?;
    drop(client);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let alive = process_alive(pid);
        #[cfg(windows)]
        let gone = Client::raw_for(true).is_err();
        #[cfg(unix)]
        let gone = Client::raw().is_err();
        if !alive && gone {
            println!("server stopped");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Failed(format!("server (pid {pid}) did not stop within 15 s")));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{}s", secs / 60, secs % 60),
        3600..=86399 => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{}h", secs / 86400, (secs % 86400) / 3600),
    }
}

pub fn server_status(json_out: bool) -> Result<()> {
    let (_, info) = match Client::connect() {
        Ok(v) => v,
        Err(e) => {
            if json_out {
                println!("{}", json!({ "running": false }));
            }
            return Err(e);
        }
    };
    if json_out {
        let mut v = info.0.clone();
        v["running"] = json!(true);
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(());
    }
    println!("running");
    println!("pid:      {}", info.num("pid"));
    println!("uptime:   {}", human_duration(info.num("uptime_secs")));
    println!("version:  {}", info.str("version"));
    println!("protocol: {}", info.str("proto_version"));
    println!("panes:    {}", info.num("panes"));
    println!("clients:  {}", info.num("clients"));
    println!("node id:  {}", info.0.get("node_id").and_then(Value::as_str).unwrap_or("none"));
    Ok(())
}

// ---------------------------------------------------------------------------
// panes
// ---------------------------------------------------------------------------

pub fn ls(json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let tabs = client.call("list_tabs", Value::Null)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&tabs).unwrap_or_default());
        return Ok(());
    }
    let empty = Vec::new();
    let panes = tabs.get("tabs").and_then(Value::as_array).unwrap_or(&empty);
    if panes.is_empty() {
        println!("no panes");
        return Ok(());
    }
    println!("{:<36}  {:<14}  {:>7}  {:>7}  {:<10}  CWD", "PANE", "LABEL", "PID", "SIZE", "STATE");
    for p in panes {
        println!(
            "{:<36}  {:<14}  {:>7}  {:>7}  {:<10}  {}",
            p.get("pane_id").and_then(Value::as_str).unwrap_or("?"),
            printable(p.get("label").and_then(Value::as_str).unwrap_or("")),
            p.get("pid").and_then(Value::as_u64).unwrap_or(0),
            format!(
                "{}x{}",
                p.get("cols").and_then(Value::as_u64).unwrap_or(0),
                p.get("rows").and_then(Value::as_u64).unwrap_or(0)
            ),
            pane_state(p),
            p.get("cwd").and_then(Value::as_str).unwrap_or("")
        );
    }
    Ok(())
}

/// `running`, or `exited N` for a kept pane whose process ended.
fn pane_state(p: &Value) -> String {
    if p.get("exited").and_then(Value::as_bool).unwrap_or(false) {
        match p.get("exit_code").and_then(Value::as_i64) {
            Some(c) => format!("exited {c}"),
            None => "exited".to_string(),
        }
    } else {
        "running".to_string()
    }
}

pub fn new_pane(
    cwd: Option<PathBuf>,
    command: Vec<String>,
    keep: bool,
    json_out: bool,
) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir().map_err(|e| CliError::Failed(e.to_string()))?,
    };
    let cwd = std::fs::canonicalize(&cwd)
        .map_err(|e| CliError::Usage(format!("--cwd {}: {e}", cwd.display())))?;
    let mut params = json!({ "workspace_idx": 0, "cwd": cwd.display().to_string() });
    if !command.is_empty() {
        params["command"] = json!(command);
    }
    if keep {
        params["remain_on_exit"] = json!(true);
    }
    let result = client.call("new_tab", params)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
    } else {
        println!("{}", result.get("pane_id").and_then(Value::as_str).unwrap_or("?"));
    }
    Ok(())
}

fn send_bytes(client: &mut Client, pane: &str, bytes: &[u8]) -> Result<()> {
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    client.call("send_input", json!({ "pane_id": pane, "data": data }))?;
    Ok(())
}

pub fn send(target: &str, text: &str, enter: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let pane = resolve_pane(&mut client, target)?;
    let mut bytes = text.as_bytes().to_vec();
    if enter {
        bytes.push(b'\r');
    }
    send_bytes(&mut client, &pane, &bytes)
}

/// Bytes for a named key.
fn key_bytes(key: &str) -> Option<&'static [u8]> {
    match key.trim().to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "esc" | "escape" => Some(b"\x1b"),
        "ctrl+c" | "ctrl-c" => Some(b"\x03"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        _ => None,
    }
}

pub fn send_key(target: &str, key: &str) -> Result<()> {
    let bytes = key_bytes(key).ok_or_else(|| {
        CliError::Usage(format!(
            "unknown key {key:?}. Keys: enter, tab, esc, ctrl+c, up, down, left, right"
        ))
    })?;
    let (mut client, _) = Client::connect()?;
    let pane = resolve_pane(&mut client, target)?;
    if bytes == b"\x03" {
        // Ctrl+C is an interrupt, not text: the server writes the terminal's
        // interrupt key (or 0x03 in raw mode), drops stale queued input, and
        // never signals a program that handles the key itself.
        client.call("send_sigint", json!({ "pane_id": pane }))?;
        return Ok(());
    }
    send_bytes(&mut client, &pane, bytes)
}

// ---------------------------------------------------------------------------
// clients and devices
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn client_ls(json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let result = client.call("list_clients", Value::Null)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        return Ok(());
    }
    let empty = Vec::new();
    let clients = result.get("clients").and_then(Value::as_array).unwrap_or(&empty);
    if clients.is_empty() {
        println!("no clients attached");
        return Ok(());
    }
    println!("{:<12}  {:<8}  {:<24}  CONNECTED", "ID", "KIND", "NAME");
    for c in clients {
        let id = c.get("client_id").and_then(Value::as_str).unwrap_or("?");
        let since =
            now_secs().saturating_sub(c.get("connected_at").and_then(Value::as_u64).unwrap_or(0));
        println!(
            "{:<12}  {:<8}  {:<24}  {} ago",
            &id[..id.len().min(12)],
            c.get("client_kind").and_then(Value::as_str).unwrap_or("?"),
            forgetty_sync::sanitize::untrusted(
                c.get("client_name").and_then(Value::as_str).unwrap_or("")
            ),
            human_duration(since)
        );
    }
    Ok(())
}

fn device_list(client: &mut Client) -> Result<Vec<Value>> {
    let result = client.call("list_devices", Value::Null)?;
    Ok(result.get("devices").and_then(Value::as_array).cloned().unwrap_or_default())
}

pub fn devices(json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let list = device_list(&mut client)?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "devices": list })).unwrap_or_default()
        );
        return Ok(());
    }
    if list.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for d in &list {
        println!(
            "{}  {}  paired {}",
            d.get("device_id").and_then(Value::as_str).unwrap_or("?"),
            forgetty_sync::sanitize::untrusted(d.get("name").and_then(Value::as_str).unwrap_or("")),
            d.get("paired_at").and_then(Value::as_str).unwrap_or("?")
        );
    }
    Ok(())
}

pub fn revoke(target: &str) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let ids: Vec<String> = device_list(&mut client)?
        .iter()
        .filter_map(|d| d.get("device_id").and_then(Value::as_str).map(String::from))
        .collect();
    let id = resolve_target("device", target, &ids)?;
    let out = client.call("revoke_device", json!({ "device_id": id }))?;
    println!("revoked {id}");
    if let Some(w) = out.get("warning").and_then(Value::as_str) {
        eprintln!("forgetty: warning: {w}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pairing
// ---------------------------------------------------------------------------

pub fn pair(ttl: u64) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let opened = client.call("pair_open", json!({ "ttl_secs": ttl }))?;
    let window_id = opened
        .get("window_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliError::Failed("pair_open returned no window_id".into()))?;
    let payload_value = opened.get("payload").cloned().unwrap_or(Value::Null);
    let payload: forgetty_sync::QrPayload = serde_json::from_value(payload_value.clone())
        .map_err(|e| CliError::Failed(format!("bad pairing payload: {e}")))?;
    let ascii = forgetty_sync::qr::qr_to_ascii(&payload)
        .map_err(|e| CliError::Failed(format!("QR render failed: {e}")))?;

    println!("{ascii}");
    println!("payload: {payload_value}");
    println!(
        "Scan the QR code in the Forgetty app. The window is open for {} s and accepts one device.",
        opened.get("ttl_secs").and_then(Value::as_u64).unwrap_or(ttl)
    );
    let _ = std::io::stdout().flush();

    loop {
        let status = client.call("pair_status", json!({ "window_id": window_id }))?;
        match status.get("state").and_then(Value::as_str) {
            Some("paired") => {
                println!(
                    "paired: {} ({})",
                    forgetty_sync::sanitize::untrusted(
                        status.get("name").and_then(Value::as_str).unwrap_or("?")
                    ),
                    status.get("device_id").and_then(Value::as_str).unwrap_or("?")
                );
                return Ok(());
            }
            Some("open") => std::thread::sleep(Duration::from_millis(300)),
            _ => {
                println!("expired");
                return Err(CliError::Failed("pairing window expired, no device paired".into()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// close, capture, events, run, wait (P4a)
// ---------------------------------------------------------------------------

pub fn close(target: &str) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let pane = resolve_pane(&mut client, target)?;
    client.call("close_pane", json!({ "pane_id": pane }))?;
    Ok(())
}

pub fn capture(target: &str, trim: bool, lines: Option<u32>, json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let pane = resolve_pane(&mut client, target)?;
    let mut params = json!({ "pane_id": pane, "trim": trim });
    if let Some(n) = lines {
        params["last_lines"] = json!(n);
    }
    let result = client.call("capture", params)?;
    let text = result.get("text").and_then(Value::as_str).unwrap_or("");
    let incomplete = result.get("incomplete").and_then(Value::as_bool).unwrap_or(false);
    if incomplete {
        eprintln!("warning: capture ran out of time, the screen below is incomplete");
    }
    if json_out {
        println!("{}", json!({ "pane_id": pane, "text": text, "incomplete": incomplete }));
    } else if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// `pane_closed` becomes `PaneClosed`, as in PROTOCOL_V2 `ServerEvent`.
fn event_name(method: &str) -> String {
    method
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// One event as the `--json` line: `{"event":"StatusChanged", ...params}`.
fn event_json(msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let mut obj = serde_json::Map::new();
    obj.insert("event".into(), json!(event_name(method)));
    if let Some(params) = msg.get("params").and_then(Value::as_object) {
        for (k, v) in params {
            obj.insert(k.clone(), v.clone());
        }
    }
    Some(Value::Object(obj))
}

/// Clean text for a terminal: the server already sanitises, this is belt and
/// braces for what gets printed to the operator's tty.
fn printable(s: &str) -> String {
    forgetty_core::sanitize::untrusted_max(s, 512)
}

fn event_line(ev: &Value) -> String {
    let name = ev.get("event").and_then(Value::as_str).unwrap_or("?");
    let mut line = name.to_string();
    if let Some(p) = ev.get("pane_id").and_then(Value::as_str) {
        line.push_str(&format!(" pane={}", &p[p.len().saturating_sub(8)..]));
    }
    if let Some(obj) = ev.as_object() {
        for (k, v) in obj {
            if k == "event" || k == "pane_id" || v.is_null() {
                continue;
            }
            let text = match v {
                Value::String(s) => printable(s),
                other => other.to_string(),
            };
            line.push_str(&format!(" {k}={text:?}"));
        }
    }
    line
}

pub fn events(pane: Option<&str>, json_out: bool) -> Result<()> {
    let (mut main, _) = Client::connect()?;
    let filter = match pane {
        Some(t) => Some(resolve_pane(&mut main, t)?),
        None => None,
    };
    drop(main);
    let mut sub = Client::subscribe()?;
    let mut pending = Vec::new();
    loop {
        let Some(msg) = sub.poll_line(&mut pending, Duration::from_secs(3600))? else { continue };
        let Some(ev) = event_json(&msg) else { continue };
        if let Some(want) = &filter {
            if ev.get("pane_id").and_then(Value::as_str) != Some(want.as_str()) {
                continue;
            }
        }
        if json_out {
            println!("{ev}");
        } else {
            println!("{}", event_line(&ev));
        }
    }
}

/// Exit code to report for a process exit code. `None` (unknown) is a failure.
fn exit_value(code: Option<i64>) -> i32 {
    match code {
        Some(c) => (c & 0xff) as i32,
        None => EXIT_FAILED,
    }
}

pub fn run(keep: bool, wait: bool, cwd: Option<PathBuf>, command: Vec<String>) -> Result<i32> {
    if command.is_empty() {
        return Err(CliError::Usage("run needs a command after `--`".into()));
    }
    // Subscribe before the pane exists, so a fast exit cannot be missed.
    let mut sub = if wait { Some(Client::subscribe()?) } else { None };

    let (mut client, _) = Client::connect()?;
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir().map_err(|e| CliError::Failed(e.to_string()))?,
    };
    let cwd = std::fs::canonicalize(&cwd)
        .map_err(|e| CliError::Usage(format!("--cwd {}: {e}", cwd.display())))?;
    let mut params = json!({
        "workspace_idx": 0,
        "cwd": cwd.display().to_string(),
        "command": command,
    });
    if keep {
        params["remain_on_exit"] = json!(true);
    }
    let result = client.call("new_tab", params)?;
    let pane = result.get("pane_id").and_then(Value::as_str).unwrap_or("").to_string();
    println!("{pane}");
    let Some(sub) = sub.as_mut() else { return Ok(0) };

    let mut pending = Vec::new();
    let mut last_check = Instant::now();
    loop {
        let msg = sub.poll_line(&mut pending, Duration::from_millis(500))?;
        if let Some(msg) = msg {
            let params = msg.get("params");
            let for_pane = params.and_then(|p| p.get("pane_id")).and_then(Value::as_str)
                == Some(pane.as_str());
            match msg.get("method").and_then(Value::as_str) {
                Some("pane_exited") if for_pane => {
                    let code = params.and_then(|p| p.get("exit_code")).and_then(Value::as_i64);
                    return Ok(exit_value(code));
                }
                // Closed without an exit event (closed by hand): use the record.
                Some("pane_closed") if for_pane => return exit_from_record(&mut client, &pane),
                _ => {}
            }
        }
        // Safety net if the event stream missed something.
        if last_check.elapsed() >= Duration::from_secs(2) {
            last_check = Instant::now();
            if let Some(code) = exit_if_done(&mut client, &pane)? {
                return Ok(code);
            }
        }
    }
}

fn exit_from_record(client: &mut Client, pane: &str) -> Result<i32> {
    match exit_if_done(client, pane)? {
        Some(c) => Ok(c),
        None => Err(CliError::Failed(format!("pane {pane} closed without an exit status"))),
    }
}

/// Exit code of the pane's process if it has ended.
fn exit_if_done(client: &mut Client, pane: &str) -> Result<Option<i32>> {
    let st = client.call("pane_exit_status", json!({ "pane_id": pane }))?;
    if st.get("running").and_then(Value::as_bool).unwrap_or(true) {
        return Ok(None);
    }
    Ok(Some(exit_value(st.get("exit_code").and_then(Value::as_i64))))
}

pub fn wait(target: &str, status: Option<&str>, timeout: Option<f64>) -> Result<i32> {
    let want = match status {
        Some(s) => Some(
            forgetty_session::PaneStatus::parse(s)
                .ok_or_else(|| {
                    CliError::Usage(format!(
                        "unknown status {s:?}. Use idle, working, waiting, done or failed."
                    ))
                })?
                .as_str(),
        ),
        None => None,
    };
    let (mut client, _) = Client::connect()?;
    let pane = resolve_pane(&mut client, target)?;
    // Subscribe first, then look at the current state: nothing falls in between.
    let mut sub = Client::subscribe()?;
    let deadline = timeout.map(|t| Instant::now() + Duration::from_secs_f64(t.max(0.0)));
    let mut pending = Vec::new();
    let mut last_check = Instant::now() - Duration::from_secs(10);

    loop {
        if last_check.elapsed() >= Duration::from_secs(2) {
            last_check = Instant::now();
            match client.call("get_pane_info", json!({ "pane_id": pane })) {
                Ok(info) => match want {
                    Some(w) => {
                        let cur = info
                            .pointer("/status/status")
                            .and_then(Value::as_str)
                            .unwrap_or("idle");
                        if cur == w {
                            return Ok(0);
                        }
                    }
                    None => {
                        if exit_if_done(&mut client, &pane)?.is_some() {
                            return Ok(0);
                        }
                    }
                },
                // The pane is gone.
                Err(_) => {
                    return match (want, exit_if_done(&mut client, &pane)) {
                        (None, Ok(Some(_))) => Ok(0),
                        _ => Err(CliError::Failed(format!(
                            "pane {pane} closed before reaching the wanted state"
                        ))),
                    };
                }
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!("forgetty: wait timed out");
                return Ok(EXIT_TIMEOUT);
            }
        }
        let Some(msg) = sub.poll_line(&mut pending, Duration::from_millis(500))? else { continue };
        let params = msg.get("params");
        if params.and_then(|p| p.get("pane_id")).and_then(Value::as_str) != Some(pane.as_str()) {
            continue;
        }
        match (msg.get("method").and_then(Value::as_str), want) {
            (Some("status_changed"), Some(w)) => {
                if params.and_then(|p| p.get("status")).and_then(Value::as_str) == Some(w) {
                    return Ok(0);
                }
            }
            (Some("pane_exited"), None) => return Ok(0),
            (Some("pane_closed"), None) => return Ok(0),
            (Some("pane_closed"), Some(_)) => {
                return Err(CliError::Failed(format!(
                    "pane {pane} closed before reaching the wanted state"
                )))
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// labels (P5)
// ---------------------------------------------------------------------------

/// What `forgetty label -t` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LabelKind {
    Workspace,
    Tab,
    Pane,
}

impl LabelKind {
    fn as_str(self) -> &'static str {
        match self {
            LabelKind::Workspace => "workspace",
            LabelKind::Tab => "tab",
            LabelKind::Pane => "pane",
        }
    }
}

pub fn label(kind: LabelKind, target: &str, label: &str, json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let id = match kind {
        LabelKind::Pane => resolve_pane(&mut client, target)?,
        LabelKind::Tab => {
            let (tabs, _) = layout_cands(&mut client)?;
            resolve_cands("tab", target, &tabs)?
        }
        LabelKind::Workspace => {
            let (_, workspaces) = layout_cands(&mut client)?;
            resolve_cands("workspace", target, &workspaces)?
        }
    };
    let result = match client
        .call_rpc("set_label", json!({ "target": kind.as_str(), "id": id, "label": label }))?
    {
        Ok(v) => v,
        // A bad label is a bad command line.
        Err((code, msg)) if code == i64::from(forgetty_socket::protocol::INVALID_PARAMS) => {
            return Err(CliError::Usage(format!("bad_request: {msg}")))
        }
        Err((_, msg)) => return Err(CliError::Failed(format!("set_label: {msg}"))),
    };
    if json_out {
        println!(
            "{}",
            json!({ "target": kind.as_str(), "id": id, "label": label.trim(), "changed": result["changed"] })
        );
    } else if label.trim().is_empty() {
        println!("cleared the label of {} {id}", kind.as_str());
    } else {
        println!("{} {id} is now {:?}", kind.as_str(), label.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// api list / describe / call (P5)
// ---------------------------------------------------------------------------

/// Name of a JSON-RPC error code, as the docs use it.
fn rpc_error_name(code: i64) -> &'static str {
    match code {
        -32700 => "parse_error",
        -32600 | -32602 => "bad_request",
        -32601 => "method_not_found",
        -32603 => "internal_error",
        SERVER_BUSY => "server_busy",
        _ => "error",
    }
}

/// Exit code for a JSON-RPC error: `bad_request` is a usage error (2),
/// `method_not_found` is not found (3), anything else is a failure (1).
fn rpc_error(code: i64, msg: String) -> CliError {
    let text = format!("{}: {msg}", rpc_error_name(code));
    match code {
        -32700 | -32600 | -32602 => CliError::Usage(text),
        -32601 => CliError::NotFound(text),
        _ => CliError::Failed(text),
    }
}

pub fn api_list(quiet: bool, json_out: bool, markdown: bool) -> Result<()> {
    if markdown {
        // Needs no server: the table is compiled in.
        print!("{}", forgetty_socket::apidoc::render());
        return Ok(());
    }
    let (mut client, _) = Client::connect()?;
    let result = match client.call_rpc("api.list", Value::Null)? {
        Ok(v) => v,
        Err((code, msg)) => return Err(rpc_error(code, msg)),
    };
    let empty = Vec::new();
    let methods = result.get("methods").and_then(Value::as_array).unwrap_or(&empty);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
    } else if quiet {
        for m in methods {
            println!("{}", m.get("name").and_then(Value::as_str).unwrap_or("?"));
        }
    } else {
        println!("{:<22}  {:<9}  {:<3}  SUMMARY", "METHOD", "KIND", "V2");
        for m in methods {
            println!(
                "{:<22}  {:<9}  {:<3}  {}",
                m.get("name").and_then(Value::as_str).unwrap_or("?"),
                m.get("kind").and_then(Value::as_str).unwrap_or("?"),
                if m.get("remote").and_then(Value::as_bool).unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                },
                m.get("summary").and_then(Value::as_str).unwrap_or("")
            );
        }
    }
    Ok(())
}

pub fn api_describe(method: &str, json_out: bool) -> Result<()> {
    let (mut client, _) = Client::connect()?;
    let d = match client.call_rpc("api.describe", json!({ "method": method }))? {
        Ok(v) => v,
        Err((code, msg)) => return Err(rpc_error(code, msg)),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
        return Ok(());
    }
    let text = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let pretty = |k: &str| serde_json::to_string_pretty(&d[k]).unwrap_or_default();
    println!("{}: {}", text("name"), text("summary"));
    println!();
    println!("{}", text("description"));
    println!();
    println!(
        "kind: {}   protocol v2: {}",
        text("kind"),
        if d.get("remote").and_then(Value::as_bool).unwrap_or(false) { "yes" } else { "no" }
    );
    println!();
    println!("params schema:\n{}", pretty("params_schema"));
    println!();
    println!("result schema:\n{}", pretty("result_schema"));
    println!();
    println!("request example:\n{}", pretty("request_example"));
    println!();
    println!("response example:\n{}", pretty("response_example"));
    Ok(())
}

/// Read the params argument of `api call`: JSON text, `@file` or `-` (stdin).
fn read_params(arg: Option<&str>) -> Result<Value> {
    let text = match arg {
        None => return Ok(Value::Null),
        Some("-") => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| CliError::Usage(format!("bad_request: cannot read stdin: {e}")))?;
            buf
        }
        Some(a) if a.starts_with('@') => std::fs::read_to_string(&a[1..])
            .map_err(|e| CliError::Usage(format!("bad_request: cannot read {}: {e}", &a[1..])))?,
        Some(a) => a.to_string(),
    };
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| CliError::Usage(format!("bad_request: params are not valid JSON: {e}")))
}

pub fn api_call(pane: Option<&str>, method: &str, params: Option<&str>) -> Result<()> {
    // Read the params before connecting, so a bad argument fails the same way
    // with or without a server.
    let mut params = read_params(params)?;
    let (mut client, _) = Client::connect()?;
    if let Some(target) = pane {
        let id = resolve_pane(&mut client, target)?;
        match &mut params {
            Value::Null => params = json!({ "pane_id": id }),
            Value::Object(map) => {
                map.entry("pane_id").or_insert(json!(id));
            }
            _ => {
                return Err(CliError::Usage(
                    "bad_request: -p needs the params to be a JSON object".into(),
                ))
            }
        }
    }
    match client.call_rpc(method, params)? {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(())
        }
        Err((code, msg)) => Err(rpc_error(code, msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec![
            "28f3648d-d2c2-42d8-a9fa-3b14a1ea5999".to_string(),
            "8cbbc727-60cd-44c5-b9c8-077779315e2d".to_string(),
            "11111111-2222-3333-4444-5555a1ea5999".to_string(),
        ]
    }

    #[test]
    fn full_id_resolves() {
        assert_eq!(resolve_target("pane", &ids()[1], &ids()).unwrap(), ids()[1]);
    }

    #[test]
    fn unique_suffix_resolves() {
        assert_eq!(resolve_target("pane", "315e2d", &ids()).unwrap(), ids()[1]);
        assert_eq!(resolve_target("pane", "315E2D", &ids()).unwrap(), ids()[1]);
    }

    #[test]
    fn ambiguous_suffix_is_not_found() {
        let err = resolve_target("pane", "a1ea5999", &ids()).unwrap_err();
        assert_eq!(err.code(), EXIT_NOT_FOUND);
        assert!(err.message().contains("ambiguous"));
    }

    #[test]
    fn unknown_suffix_is_not_found() {
        assert_eq!(resolve_target("pane", "ffff", &ids()).unwrap_err().code(), EXIT_NOT_FOUND);
    }

    #[test]
    fn short_target_is_a_usage_error() {
        assert_eq!(resolve_target("pane", "999", &ids()).unwrap_err().code(), EXIT_USAGE);
    }

    fn cands() -> Vec<Cand> {
        vec![
            Cand { id: "28f3648d-d2c2-42d8-a9fa-3b14a1ea5999".into(), label: "Build".into() },
            Cand { id: "8cbbc727-60cd-44c5-b9c8-077779315e2d".into(), label: "logs".into() },
            Cand { id: "11111111-2222-3333-4444-5555a1ea5999".into(), label: "logs".into() },
            Cand { id: "99999999-2222-3333-4444-5555aaaaaaaa".into(), label: "2".into() },
        ]
    }

    #[test]
    fn label_matches_case_insensitively() {
        assert_eq!(resolve_cands("pane", "build", &cands()).unwrap(), cands()[0].id);
        assert_eq!(resolve_cands("pane", "BUILD", &cands()).unwrap(), cands()[0].id);
    }

    #[test]
    fn duplicate_label_is_ambiguous_with_a_list() {
        let err = resolve_cands("pane", "logs", &cands()).unwrap_err();
        assert_eq!(err.code(), EXIT_NOT_FOUND);
        assert!(err.message().contains("ambiguous"));
        assert!(err.message().contains(&cands()[1].id) && err.message().contains(&cands()[2].id));
    }

    #[test]
    fn full_id_beats_label_beats_position_beats_suffix() {
        let mut c = cands();
        // A label equal to another item's full id: the id wins.
        c[0].label = c[1].id.clone();
        assert_eq!(resolve_cands("pane", &c[1].id, &c).unwrap(), c[1].id);
        // A label "2" beats position 2.
        assert_eq!(resolve_cands("pane", "2", &cands()).unwrap(), cands()[3].id);
        // Position 1 (no label "1"), 1-based.
        assert_eq!(resolve_cands("pane", "1", &cands()).unwrap(), cands()[0].id);
        assert_eq!(resolve_cands("pane", "3", &cands()).unwrap(), cands()[2].id);
        // A label that is also a suffix: label first.
        let mut c = cands();
        c[3].label = "315e2d".into();
        assert_eq!(resolve_cands("pane", "315e2d", &c).unwrap(), c[3].id);
        // No label: the suffix.
        assert_eq!(resolve_cands("pane", "315e2d", &cands()).unwrap(), cands()[1].id);
    }

    #[test]
    fn position_out_of_range_falls_through() {
        // 5 is no position (4 items) and too short for a suffix.
        assert_eq!(resolve_cands("pane", "5", &cands()).unwrap_err().code(), EXIT_USAGE);
        // 5999 is no position, but it is a suffix shared by two ids.
        assert_eq!(resolve_cands("pane", "5999", &cands()).unwrap_err().code(), EXIT_NOT_FOUND);
    }

    #[test]
    fn rpc_errors_map_to_exit_codes() {
        assert_eq!(rpc_error(-32602, "x".into()).code(), EXIT_USAGE);
        assert_eq!(rpc_error(-32601, "x".into()).code(), EXIT_NOT_FOUND);
        assert_eq!(rpc_error(-32603, "x".into()).code(), EXIT_FAILED);
        assert!(rpc_error(-32602, "x".into()).message().starts_with("bad_request: "));
        assert!(rpc_error(-32601, "x".into()).message().starts_with("method_not_found: "));
    }

    #[test]
    fn api_call_params_come_from_text_file_or_nothing() {
        assert_eq!(read_params(None).unwrap(), Value::Null);
        assert_eq!(read_params(Some("{\"a\":1}")).unwrap(), json!({"a": 1}));
        assert_eq!(read_params(Some("  ")).unwrap(), Value::Null);
        assert_eq!(read_params(Some("{nope")).unwrap_err().code(), EXIT_USAGE);
        let dir = std::env::temp_dir().join(format!("fgt-p5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.json");
        std::fs::write(&f, "{\"b\":2}").unwrap();
        assert_eq!(read_params(Some(&format!("@{}", f.display()))).unwrap(), json!({"b": 2}));
        assert_eq!(read_params(Some("@/nonexistent/x")).unwrap_err().code(), EXIT_USAGE);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keys_map_to_bytes() {
        assert_eq!(key_bytes("enter"), Some(&b"\r"[..]));
        assert_eq!(key_bytes("Ctrl+C"), Some(&b"\x03"[..]));
        assert_eq!(key_bytes("up"), Some(&b"\x1b[A"[..]));
        assert_eq!(key_bytes("nope"), None);
    }

    #[test]
    fn error_codes_match_the_contract() {
        assert_eq!(CliError::Failed(String::new()).code(), 1);
        assert_eq!(CliError::Usage(String::new()).code(), 2);
        assert_eq!(CliError::NotFound(String::new()).code(), 3);
        assert_eq!(CliError::Unreachable(String::new()).code(), 4);
    }

    #[test]
    fn event_names_follow_serverevent() {
        assert_eq!(event_name("status_changed"), "StatusChanged");
        assert_eq!(event_name("pane_closed"), "PaneClosed");
        assert_eq!(event_name("cwd_changed"), "CwdChanged");
        assert_eq!(event_name("notification"), "Notification");
    }

    #[test]
    fn event_json_flattens_params() {
        let msg = json!({"jsonrpc":"2.0","method":"status_changed",
            "params":{"pane_id":"p","status":"waiting","message":"m"}});
        let ev = event_json(&msg).unwrap();
        assert_eq!(ev["event"], "StatusChanged");
        assert_eq!(ev["status"], "waiting");
        assert_eq!(ev["message"], "m");
        assert!(event_json(&json!({"result":{}})).is_none());
    }

    #[test]
    fn event_line_hides_control_characters() {
        let ev = json!({"event":"Notification","pane_id":"0123456789abcdef","body":"a\u{1b}[31mb"});
        let line = event_line(&ev);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(line.contains("pane=89abcdef"));
    }

    #[test]
    fn exit_values() {
        assert_eq!(exit_value(Some(7)), 7);
        assert_eq!(exit_value(Some(256 + 3)), 3);
        assert_eq!(exit_value(None), EXIT_FAILED);
    }
}
