//! CLI half of the `forgetty` binary. Talks to the server over the local
//! Unix socket (JSON-RPC, one request per line). Blocking and synchronous.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::server::default_socket_path;

pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NOT_FOUND: i32 = 3;
pub const EXIT_UNREACHABLE: i32 = 4;

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
    match result {
        Ok(()) => 0,
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
    if let Ok((_, info)) = Client::connect() {
        println!("server already running (pid {})", info.num("pid"));
        return Ok(());
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
        if let Ok((_, info)) = Client::connect() {
            println!("server started (pid {})", info.num("pid"));
            return Ok(());
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
    println!("{:<36}  {:>7}  {:>7}  CWD", "PANE", "PID", "SIZE");
    for p in panes {
        println!(
            "{:<36}  {:>7}  {:>7}  {}",
            p.get("pane_id").and_then(Value::as_str).unwrap_or("?"),
            p.get("pid").and_then(Value::as_u64).unwrap_or(0),
            format!(
                "{}x{}",
                p.get("cols").and_then(Value::as_u64).unwrap_or(0),
                p.get("rows").and_then(Value::as_u64).unwrap_or(0)
            ),
            p.get("cwd").and_then(Value::as_str).unwrap_or("")
        );
    }
    Ok(())
}

pub fn new_pane(cwd: Option<PathBuf>, command: Vec<String>, json_out: bool) -> Result<()> {
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
            c.get("client_name").and_then(Value::as_str).unwrap_or(""),
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
            d.get("name").and_then(Value::as_str).unwrap_or(""),
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
                    status.get("name").and_then(Value::as_str).unwrap_or("?"),
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
}
