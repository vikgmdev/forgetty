//! End to end on Windows: the real `forgetty` binary starts a detached server with its own
//! state folder, and the CLI talks to it over the named pipe. Nothing here touches a server
//! of the user: `--state-dir` moves the data, the config and the pipe name.
#![cfg(windows)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The tests that start a server on the default port 47297 run one at a time. Every other test
/// asks for port 0 (`FORGETTY_PORT=0`), so no two test servers race for the same UDP port.
static DEFAULT_PORT: Mutex<()> = Mutex::new(());

fn command(state: &Path, args: &[&str], default_port: bool) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_forgetty"));
    c.arg("--state-dir")
        .arg(state)
        .args(args)
        // The CI runner is an administrator, and the server refuses to run elevated. This
        // variable lets the tests use it anyway. Real use never sets it.
        .env("FORGETTY_TEST_ALLOW_ELEVATED", "1");
    if default_port {
        c.env_remove("FORGETTY_PORT");
    } else {
        c.env("FORGETTY_PORT", "0");
    }
    c
}

fn forgetty(state: &Path, args: &[&str]) -> Output {
    command(state, args, false).output().expect("run forgetty")
}

/// Same, with the server's own default port (47297), as a user gets it.
fn forgetty_default_port(state: &Path, args: &[&str]) -> Output {
    command(state, args, true).output().expect("run forgetty")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Stops the server when the test ends, also when it fails half way.
struct Server {
    state: tempfile::TempDir,
    pid: Option<u32>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let stop = forgetty(self.state.path(), &["server", "stop"]);
        if !stop.status.success() {
            if let Some(pid) = self.pid {
                let _ =
                    Command::new("taskkill").args(["/PID", &pid.to_string(), "/F", "/T"]).output();
            }
        }
    }
}

#[test]
fn the_cli_drives_a_detached_server_over_the_pipe() {
    let mut srv = Server { state: tempfile::tempdir().unwrap(), pid: None };
    let state = srv.state.path().to_path_buf();

    // No server yet: unreachable (exit 4), and nothing is started by asking.
    let none = forgetty(&state, &["server", "status"]);
    assert_eq!(none.status.code(), Some(4), "{}", text(&none));

    // Start. The server detaches: this call returns while it keeps running.
    let start = forgetty(&state, &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));
    assert!(text(&start).contains("server started"), "{}", text(&start));

    let status = forgetty(&state, &["server", "status", "--json"]);
    assert!(status.status.success(), "{}", text(&status));
    let info: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(info["running"], true);
    srv.pid = info["pid"].as_u64().map(|p| p as u32);
    assert_ne!(srv.pid, Some(std::process::id()));

    // A second start finds the first.
    let again = forgetty(&state, &["server", "start"]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(text(&again).contains("already running"), "{}", text(&again));

    // A pane: type into it and read the screen back.
    let new = forgetty(&state, &["new", "--", "cmd.exe"]);
    assert!(new.status.success(), "{}", text(&new));
    let pane = String::from_utf8_lossy(&new.stdout).trim().to_string();
    assert_eq!(pane.len(), 36, "pane id: {pane:?}");

    let ls = forgetty(&state, &["ls"]);
    assert!(text(&ls).contains(&pane), "{}", text(&ls));

    // `set /a` prints the sum, which the typed line does not hold.
    let send = forgetty(&state, &["send", "-p", &pane, "--enter", "set /a 70000+5"]);
    assert!(send.status.success(), "{}", text(&send));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cap = forgetty(&state, &["capture", "-p", &pane]);
        let screen = text(&cap);
        if screen.lines().any(|l| l.trim() == "70005") {
            break;
        }
        assert!(Instant::now() < deadline, "no 70005 on the screen:\n{screen}");
        std::thread::sleep(Duration::from_millis(200));
    }

    // A command that ends: its exit code comes back through the server.
    let run = forgetty(&state, &["run", "--wait", "--", "cmd.exe", "/c", "exit 3"]);
    assert_eq!(run.status.code(), Some(3), "{}", text(&run));

    let close = forgetty(&state, &["close", "-p", &pane]);
    assert!(close.status.success(), "{}", text(&close));

    // Stop: the call returns when the server is gone, and the pipe with it.
    let stop = forgetty(&state, &["server", "stop"]);
    assert!(stop.status.success(), "{}", text(&stop));
    let gone = forgetty(&state, &["server", "status"]);
    assert_eq!(gone.status.code(), Some(4), "{}", text(&gone));
}

fn status_json(state: &Path) -> serde_json::Value {
    let out = forgetty(state, &["server", "status", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    serde_json::from_slice(&out.stdout).expect("status json")
}

/// The `status --json` contract the app reads: a `port` and the `addresses`, "ip:port" each.
fn bound(info: &serde_json::Value) -> (u16, Vec<String>) {
    let port = info["port"].as_u64().unwrap_or_else(|| panic!("no port in {info}")) as u16;
    let addrs = info["addresses"]
        .as_array()
        .unwrap_or_else(|| panic!("no addresses in {info}"))
        .iter()
        .map(|a| a.as_str().expect("address string").to_string())
        .collect();
    (port, addrs)
}

/// The rows of `netstat -ano -p <proto>` that belong to `pid`: (local, foreign, state).
fn sockets_of(pid: u32, proto: &str) -> Vec<(String, String, String)> {
    let out = Command::new("netstat").args(["-ano", "-p", proto]).output().expect("netstat");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.first().map(|p| p.eq_ignore_ascii_case(proto)) != Some(true) {
                return None;
            }
            if f.last().and_then(|p| p.parse::<u32>().ok()) != Some(pid) {
                return None;
            }
            let state = if f.len() >= 5 { f[3].to_string() } else { String::new() };
            Some((f.get(1)?.to_string(), f.get(2)?.to_string(), state))
        })
        .collect()
}

/// Runs `forgetty pair`, reads the payload line, and stops it again. The window stays open on
/// the server until the caller closes it.
fn pair_payload(state: &Path) -> serde_json::Value {
    pair_window(state).1
}

/// [`pair_payload`] with the `window_id` the command prints.
fn pair_window(state: &Path) -> (u64, serde_json::Value) {
    let mut child = command(state, &["pair"], false)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forgetty pair");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut id = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(n) = line.strip_prefix("window_id: ") {
                id = n.trim().parse::<u64>().ok();
            }
            if let Some(json) = line.strip_prefix("payload: ") {
                let _ = tx.send((id, json.to_string()));
                return;
            }
        }
    });
    let got = rx.recv_timeout(Duration::from_secs(30));
    let _ = child.kill();
    let _ = child.wait();
    let (id, json) = got.expect("forgetty pair printed no payload");
    (id.expect("forgetty pair printed no window_id"), serde_json::from_str(&json).expect("payload json"))
}

/// A default start is local only: loopback, no relay, port 47297. The sockets of the server
/// process are the proof, not the settings it reports.
#[test]
fn a_default_start_listens_on_loopback_only_and_contacts_no_relay() {
    let _one = DEFAULT_PORT.lock().unwrap_or_else(|e| e.into_inner());
    let mut srv = Server { state: tempfile::tempdir().unwrap(), pid: None };
    let state = srv.state.path().to_path_buf();
    let start = forgetty_default_port(&state, &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));

    let info = status_json(&state);
    let pid = info["pid"].as_u64().expect("pid") as u32;
    srv.pid = Some(pid);
    let (port, addresses) = bound(&info);
    assert_eq!(port, 47297, "default port: {info}");
    assert_eq!(addresses, vec!["127.0.0.1:47297".to_string()], "{info}");

    // The real sockets: every UDP socket of the server is on 127.0.0.1, and the iroh one is there.
    let udp = sockets_of(pid, "UDP");
    assert!(udp.iter().any(|(l, _, _)| l == "127.0.0.1:47297"), "udp sockets: {udp:?}");
    for (local, _, _) in &udp {
        assert!(local.starts_with("127.0.0.1:"), "UDP socket not on loopback: {udp:?}");
    }
    // No TCP connection to anything outside the machine (a relay would be one).
    for (local, foreign, state) in sockets_of(pid, "TCP") {
        let loop_only = |a: &str| a.starts_with("127.0.0.1:") || a.starts_with("[::1]:");
        assert!(
            state == "LISTENING" && loop_only(&local) || loop_only(&foreign),
            "TCP socket to the outside: {local} {foreign} {state}"
        );
    }

    // The pairing payload: no relay, and the loopback address to dial.
    let payload = pair_payload(&state);
    assert_eq!(payload["relays_disabled"], true, "{payload}");
    assert_eq!(payload["relay"], "", "{payload}");
    assert_eq!(payload["direct"], serde_json::json!(["127.0.0.1:47297"]), "{payload}");

    // `pair --cancel --window-id N` closes that window (the one the command printed).
    let (wid, _) = pair_window(&state);
    let cancel = forgetty(&state, &["pair", "--cancel", "--window-id", &wid.to_string()]);
    assert!(cancel.status.success(), "{}", text(&cancel));
    assert!(text(&cancel).contains("closed"), "{}", text(&cancel));
    let again = forgetty(&state, &["pair", "--cancel", "--window-id", &wid.to_string()]);
    assert!(text(&again).contains("was not open"), "{}", text(&again));

    // Without an id it closes the window the last `forgetty pair` opened, once.
    let _ = pair_window(&state);
    let cancel = forgetty(&state, &["pair", "--cancel"]);
    assert!(cancel.status.success() && text(&cancel).contains("closed"), "{}", text(&cancel));
    let again = forgetty(&state, &["pair", "--cancel"]);
    assert!(text(&again).contains("no pairing window of this command"), "{}", text(&again));
}

/// `bind_addrs` adds to 127.0.0.1 on Windows, it does not replace it: the local app keeps its
/// path. 127.0.0.2 stands for "another address" (it is on this machine and needs no network).
#[test]
fn bind_addrs_add_to_loopback_and_never_replace_it() {
    let mut srv = Server { state: tempfile::tempdir().unwrap(), pid: None };
    let dir = srv.state.path().join("config").join("forgetty");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "bind_addrs = [\"127.0.0.2\"]\n").unwrap();
    let start = forgetty(srv.state.path(), &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));
    let info = status_json(srv.state.path());
    let pid = info["pid"].as_u64().expect("pid") as u32;
    srv.pid = Some(pid);
    let (port, mut addresses) = bound(&info);
    addresses.sort();
    assert_eq!(
        addresses,
        vec![format!("127.0.0.1:{port}"), format!("127.0.0.2:{port}")],
        "{info}"
    );
    let udp = sockets_of(pid, "UDP");
    for want in &addresses {
        assert!(udp.iter().any(|(l, _, _)| l == want), "no socket on {want}: {udp:?}");
    }
    // The payload lists both, so the local app finds its address.
    let direct = pair_payload(srv.state.path())["direct"].clone();
    let mut direct: Vec<String> = serde_json::from_value(direct).unwrap();
    direct.sort();
    assert_eq!(direct, addresses);
}

/// A `FORGETTY_PORT` that is not a port is an error, not the default port.
#[test]
fn a_bad_forgetty_port_is_an_error_and_starts_nothing() {
    let state = tempfile::tempdir().unwrap();
    let out = command(state.path(), &["server", "start"], false)
        .env("FORGETTY_PORT", "not-a-port")
        .output()
        .expect("run forgetty");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("FORGETTY_PORT"), "{}", text(&out));
    let none = forgetty(state.path(), &["server", "status"]);
    assert_eq!(none.status.code(), Some(4), "{}", text(&none));
}

/// A second user of the same PC (their own state folder and pipe) finds 47297 taken. Their
/// server still starts, on a free port that `status --json` reports, and still on loopback only.
#[test]
fn a_second_server_takes_a_free_port_when_the_default_is_taken() {
    let _one = DEFAULT_PORT.lock().unwrap_or_else(|e| e.into_inner());
    let mut first = Server { state: tempfile::tempdir().unwrap(), pid: None };
    let mut second = Server { state: tempfile::tempdir().unwrap(), pid: None };

    let start = forgetty_default_port(first.state.path(), &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));
    let a = status_json(first.state.path());
    first.pid = a["pid"].as_u64().map(|p| p as u32);
    assert_eq!(bound(&a).0, 47297, "{a}");

    let start = forgetty_default_port(second.state.path(), &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));
    let b = status_json(second.state.path());
    second.pid = b["pid"].as_u64().map(|p| p as u32);
    let (port, addresses) = bound(&b);
    assert_ne!(port, 47297, "the second server must not share the port: {b}");
    assert_ne!(port, 0, "{b}");
    assert_eq!(addresses, vec![format!("127.0.0.1:{port}")], "{b}");
    let udp = sockets_of(second.pid.unwrap(), "UDP");
    assert!(udp.iter().any(|(l, _, _)| *l == format!("127.0.0.1:{port}")), "udp sockets: {udp:?}");

    // Its payload carries the port it really has.
    let payload = pair_payload(second.state.path());
    assert_eq!(payload["direct"], serde_json::json!([format!("127.0.0.1:{port}")]), "{payload}");

    // The first one is untouched.
    assert_eq!(bound(&status_json(first.state.path())).0, 47297);
}

#[test]
fn two_state_folders_have_two_pipes() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut first = Server { state: a, pid: None };
    let start = forgetty(first.state.path(), &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));
    first.pid = serde_json::from_slice::<serde_json::Value>(
        &forgetty(first.state.path(), &["server", "status", "--json"]).stdout,
    )
    .ok()
    .and_then(|v| v["pid"].as_u64())
    .map(|p| p as u32);
    // The other folder sees no server.
    let other = forgetty(b.path(), &["server", "status"]);
    assert_eq!(other.status.code(), Some(4), "{}", text(&other));
}

/// An elevated process must not run the server: any normal program of the user could reach it
/// and run commands as administrator, around the UAC prompt. The refusal is exit 78 with a
/// message, before any state is made. On a runner that is not elevated there is nothing to test.
#[test]
fn an_elevated_server_refuses_to_run() {
    if !forgetty_core::fsutil::acl::is_elevated() {
        eprintln!("skipped: this process is not elevated");
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_forgetty"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["server", "run"])
        .env_remove("FORGETTY_TEST_ALLOW_ELEVATED")
        .output()
        .expect("run forgetty");
    assert_eq!(out.status.code(), Some(78), "{}", text(&out));
    assert!(text(&out).contains("elevated"), "{}", text(&out));
    // Nothing started: no server answers.
    let status = forgetty(state.path(), &["server", "status"]);
    assert_eq!(status.status.code(), Some(4), "{}", text(&status));
}
