//! End to end on Windows: the real `forgetty` binary starts a detached server with its own
//! state folder, and the CLI talks to it over the named pipe. Nothing here touches a server
//! of the user: `--state-dir` moves the data, the config and the pipe name.
#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn forgetty(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forgetty"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        // The CI runner is an administrator, and the server refuses to run elevated. This
        // variable lets the tests use it anyway. Real use never sets it.
        .env("FORGETTY_TEST_ALLOW_ELEVATED", "1")
        .output()
        .expect("run forgetty")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
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
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F", "/T"])
                    .output();
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
