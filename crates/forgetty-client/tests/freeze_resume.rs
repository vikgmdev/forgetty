//! A server that hangs (SIGSTOP, then SIGCONT) while a real interactive shell
//! sits in the pane: after the client redials and resumes the pane stream, the
//! typed command must run and its output must come back (Android A2, restart
//! step). Needs the real server (`FORGETTY_REAL_SERVER`).
//!
//! `FREEZE_ROUNDS` sets the number of hangs on one client (default 2);
//! `FREEZE_SHELL` picks the shell (`zsh -f -i` by default).

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::ClientKind;
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, PaneStream, SecretKey};
use uuid::Uuid;

/// Everything the pane stream delivered, snapshots included.
fn follow(mut stream: PaneStream) -> Arc<Mutex<Vec<u8>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = seen.clone();
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                PaneItem::Data { bytes, .. } | PaneItem::Snapshot { bytes, .. } => {
                    out.lock().unwrap().extend_from_slice(&bytes);
                }
                PaneItem::Reset { .. } => {}
                PaneItem::Ended => return,
            }
        }
    });
    seen
}

fn contains(seen: &Arc<Mutex<Vec<u8>>>, needle: &str) -> bool {
    let hay = seen.lock().unwrap();
    let n = needle.as_bytes();
    hay.windows(n.len()).any(|w| w == n)
}

async fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out after {limit:?} waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn client_for(server: &TestServer) -> ClientHandle {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "freeze-resume", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shell_answers_after_the_server_hung_and_the_client_resumed() {
    if !real_only() {
        return;
    }
    let rounds: usize =
        std::env::var("FREEZE_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let shell = std::env::var("FREEZE_SHELL").unwrap_or_else(|_| "zsh -f -i".into());
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&format!("exec {shell}"));
    let h = client_for(&server).await;
    let seen = follow(h.attach_pane(pane).await.unwrap());
    h.resize(pane, 30, 100).await.unwrap();
    h.send_input(pane, b"echo hello-$((20+22))\r").await.unwrap();
    wait_for("the first command", Duration::from_secs(20), || contains(&seen, "hello-42")).await;

    for round in 0..rounds {
        let handshakes = server.handshakes();
        // The server hangs for 5 s, longer than the 3 s without a Pong.
        server.freeze(Duration::from_secs(5));
        wait_for("the redial", Duration::from_secs(30), || {
            server.handshakes() > handshakes && h.state().is_connected()
        })
        .await;
        let tag = format!("round-{round}-line");
        let cmd = format!("seq -f '{tag}-%04g' 1 100\r");
        h.send_input(pane, cmd.as_bytes()).await.unwrap();
        let last = format!("{tag}-0100");
        let t0 = Instant::now();
        let waited = tokio::time::timeout(Duration::from_secs(15), async {
            while !contains(&seen, &last) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if waited.is_err() {
            let got = server.received_input(pane);
            let tail = {
                let s = seen.lock().unwrap();
                String::from_utf8_lossy(&s[s.len().saturating_sub(300)..]).replace('\n', "|")
            };
            panic!(
                "round {round}: no output of the typed command 15 s after the redial; \
                 server queued the input: {}; unacked {} bytes; output tail {tail:?}",
                String::from_utf8_lossy(&got).contains(&tag),
                h.unacked_input_bytes()
            );
        }
        println!("round {round}: output {:?} after the redial", t0.elapsed());
    }
    h.close().await;
    server.shutdown().await;
}
