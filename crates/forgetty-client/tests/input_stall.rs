//! The server names a pane whose input stands still (input stall watchdog).
//! Needs the real server (`FORGETTY_REAL_SERVER`).

mod common;

use std::time::{Duration, Instant};

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::ClientKind;
use forgetty_client::{Client, ClientInfo, SecretKey};
use serde_json::json;
use uuid::Uuid;

fn pane_input(server: &TestServer, pane: Uuid) -> Option<serde_json::Value> {
    let state = server.socket_call("qa_state", json!({})).expect("qa_state");
    state["pane_input"]
        .as_array()?
        .iter()
        .find(|p| p["pane_id"].as_str() == Some(&pane.to_string()))
        .cloned()
}

fn server_log(server: &TestServer) -> String {
    let path = server.state_dir().expect("real server").parent().unwrap().join("server.log");
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_whose_program_does_not_read_is_named_after_5_s_and_clears_when_the_child_dies() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec sleep 1000");
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "input-stall", "0.0.0");
    let h = tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed");

    // Type until the PTY buffer is full: 64 KiB is far more than it holds.
    // Whole lines: a tty in canonical mode throws away what it cannot hold
    // of an unfinished line, and only complete lines fill the buffer.
    let line = [vec![b'x'; 63], b"\n".to_vec()].concat();
    for _ in 0..1024 {
        h.send_input(pane, &line).await.unwrap();
    }
    let t0 = Instant::now();

    // Within 6 s of the last byte the PTY took, the WARN names the pane and
    // the blocked write.
    let warn = loop {
        let log = server_log(&server);
        if let Some(line) = log.lines().find(|l| l.contains("pane input stalled")) {
            break line.to_string();
        }
        assert!(t0.elapsed() < Duration::from_secs(8), "no stall WARN after 8 s; log:\n{log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let took = t0.elapsed();
    assert!(took >= Duration::from_secs(4), "WARN too early ({took:?}): {warn}");
    assert!(took <= Duration::from_secs(7), "WARN too late ({took:?}): {warn}");
    println!("stall WARN after {took:?}: {warn}");
    assert!(warn.contains(&pane.to_string()), "{warn}");
    assert!(warn.contains("writer blocked in write()"), "{warn}");
    assert!(warn.contains("pty reader reading"), "{warn}");
    assert!(warn.contains("alive"), "{warn}");
    assert!(warn.contains("frames"), "{warn}");

    // qa_state has the same data.
    let st = pane_input(&server, pane).expect("pane in qa_state");
    assert_eq!(st["stalled"], json!(true), "{st}");
    assert!(st["queued_bytes"].as_u64().unwrap() > 0, "{st}");
    assert!(st["in_write_ms"].as_u64().is_some(), "{st}");
    assert_eq!(st["child_alive"], json!(true), "{st}");
    let pid = st["child_pid"].as_i64().expect("child pid") as i32;

    // One WARN only, not one per tick.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let count = server_log(&server).matches("pane input stalled").count();
    assert_eq!(count, 1, "the stall was logged {count} times");

    // The child dies: the write fails, the pane goes, the stall clears.
    // SAFETY: a plain signal to the child of the test server.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let cleared = Instant::now();
    loop {
        match pane_input(&server, pane) {
            None => break,
            Some(st) if st["stalled"] == json!(false) => break,
            Some(_) => {}
        }
        assert!(cleared.elapsed() < Duration::from_secs(10), "the stall did not clear");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("stall cleared {:?} after the kill", cleared.elapsed());
    // The clear is logged once.
    let logged = Instant::now();
    while !server_log(&server).contains("pane input moves again") {
        assert!(logged.elapsed() < Duration::from_secs(3), "no clear line in the log");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(server_log(&server).matches("pane input moves again").count(), 1);
    h.close().await;
    server.shutdown().await;
}
