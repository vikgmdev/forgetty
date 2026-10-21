//! CLI half of the `forgetty` binary. Talks to the server over the local
//! Unix socket (JSON-RPC, one request per line). Blocking and synchronous.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use forgetty_server::sockpath::{self, default_socket_path, SocketCheck};

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

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
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
        let path = default_socket_path();
        // Never talk to a socket we cannot trust: a squatter could answer with
        // a fake server or read what we send.
        match sockpath::verify_socket_path(&path, sockpath::current_uid()) {
            SocketCheck::Ok => {}
            SocketCheck::Missing => {
                return Err(CliError::Unreachable(format!(
                    "server not reachable at {}. Start it with `forgetty server start`.",
                    path.display()
                )))
            }
            SocketCheck::Unsafe(why) => {
                return Err(CliError::Failed(format!("refusing to use the server socket: {why}")))
            }
        }
        let stream = UnixStream::connect(&path).map_err(|e| {
            CliError::Unreachable(format!(
                "server not reachable at {} ({e}). Start it with `forgetty server start`.",
                path.display()
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

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
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
            return Err(if code == SERVER_BUSY {
                CliError::Failed(format!("server is busy: {msg}"))
            } else {
                CliError::Failed(format!("{method}: {msg}"))
            });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
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

/// Resolve `target` against `ids`: a full id, or a unique suffix of 4+ chars.
fn resolve_target(what: &str, target: &str, ids: &[String]) -> Result<String> {
    let target = target.trim().to_ascii_lowercase();
    if let Some(exact) = ids.iter().find(|id| id.to_ascii_lowercase() == target) {
        return Ok(exact.clone());
    }
    if target.len() < MIN_SUFFIX {
        return Err(CliError::Usage(format!(
            "{what} target {target:?} is too short: give the full id or a suffix of {MIN_SUFFIX}+ characters"
        )));
    }
    let hits: Vec<&String> =
        ids.iter().filter(|id| id.to_ascii_lowercase().ends_with(&target)).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(CliError::NotFound(format!("no {what} matches {target:?}"))),
        many => Err(CliError::NotFound(format!(
            "{what} target {target:?} is ambiguous ({} matches: {})",
            many.len(),
            many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn pane_ids(client: &mut Client) -> Result<Vec<String>> {
    let tabs = client.call("list_tabs", Value::Null)?;
    Ok(tabs
        .get("tabs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("pane_id").and_then(Value::as_str))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default())
}

fn resolve_pane(client: &mut Client, target: &str) -> Result<String> {
    let ids = pane_ids(client)?;
    resolve_target("pane", target, &ids)
}

// ---------------------------------------------------------------------------
// server start / stop / status
// ---------------------------------------------------------------------------

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
    std::fs::create_dir_all(&data).map_err(|e| CliError::Failed(e.to_string()))?;
    let log_path: PathBuf = data.join("server.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| CliError::Failed(format!("open {}: {e}", log_path.display())))?;
    let log_err = log.try_clone().map_err(|e| CliError::Failed(e.to_string()))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "run"]).stdin(std::process::Stdio::null()).stdout(log).stderr(log_err);
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
    let mut child = cmd.spawn().map_err(|e| CliError::Failed(format!("spawn server: {e}")))?;

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

pub fn server_stop(if_idle: bool) -> Result<()> {
    let (mut client, info) = Client::connect()?;
    let pid = info.num("pid") as i32;
    client.call("shutdown", json!({ "if_idle": if_idle }))?;
    drop(client);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive && Client::raw().is_err() {
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
    println!("{:<36}  {:>7}  {:>7}  {:<10}  CWD", "PANE", "PID", "SIZE", "STATE");
    for p in panes {
        println!(
            "{:<36}  {:>7}  {:>7}  {:<10}  {}",
            p.get("pane_id").and_then(Value::as_str).unwrap_or("?"),
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
    client.call("revoke_device", json!({ "device_id": id }))?;
    println!("revoked {id}");
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
    if json_out {
        println!("{}", json!({ "pane_id": pane, "text": text }));
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
