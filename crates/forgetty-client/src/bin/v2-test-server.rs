//! Host test server for the Android and Windows gates (feature `test-util`).
//!
//! Runs the in-process protocol v2 test server on a fixed UDP port with a
//! fixed identity, so a killed and restarted process is the same server at the
//! same address. Prints the node id, the direct address and the v2 pairing
//! payload, then `READY`.
//!
//! ```text
//! v2-test-server [--port 47297] [--key-file PATH] [--secret SECRET]
//!                [--machine NAME] [--panes N] [--relay URL]
//! ```
//!
//! Commands on stdin, one per line:
//! `status <waiting|done|failed> [message]` sends `StatusChanged` for the
//! first pane, `snapshot <text>` queues a VT snapshot (AD-023) for the first pane,
//! `size <rows> <cols> [name]` makes another client own the PTY size of the first pane
//! (AD-020), `drop` closes every connection and `quit` stops the server.

use std::path::PathBuf;

use forgetty_client::proto::QrPayload;
use forgetty_client::test_server::{TestServer, TestServerConfig};
use forgetty_client::{load_or_generate_identity, EndpointId};
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

/// Pane ids are stable across restarts, so a client can re-attach.
fn pane_id(n: u32) -> Uuid {
    Uuid::from_u128(0x7e57_0000_0000_4000_8000_0000_0000_0000 | u128::from(n))
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = arg(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(47297);
    let key_file =
        PathBuf::from(arg(&args, "--key-file").unwrap_or_else(|| "v2-test-server.key".into()));
    let secret = arg(&args, "--secret").unwrap_or_else(|| "TESTSECRET0000000000000000".into());
    let machine = arg(&args, "--machine").unwrap_or_else(|| "test-host".into());
    let relay = arg(&args, "--relay").unwrap_or_default();
    let panes: u32 = arg(&args, "--panes").and_then(|p| p.parse().ok()).unwrap_or(1);

    let key = load_or_generate_identity(&key_file).expect("identity file");
    let node_id: EndpointId = key.public();
    let cfg = TestServerConfig {
        port: Some(port),
        secret_key: Some(key),
        pair_secret: Some(secret.clone()),
        echo_input: true,
        capabilities: {
            let mut caps = TestServerConfig::default().capabilities;
            caps.push(forgetty_client::proto::cap::SNAPSHOT_CATCHUP.to_string());
            caps
        },
        own_runtime: false,
        panes: (1..=panes)
            .map(|n| (pane_id(n), format!("forgetty v2 test server, pane {n}\r\n$ ").into_bytes()))
            .collect(),
        ..Default::default()
    };
    let server = TestServer::start(cfg).await;
    let qr = QrPayload {
        v: QrPayload::VERSION,
        node_id: node_id.to_string(),
        machine,
        relay,
        relays: Vec::new(),
        secret,
    };
    println!("NODE_ID={node_id}");
    println!("ADDR={}", server.udp_addr());
    println!("PANE={}", pane_id(1));
    println!("QR={}", qr.to_json());
    println!("READY");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut parts = line.trim().splitn(3, ' ');
        match parts.next() {
            Some("status") => {
                let status = parts.next().unwrap_or("waiting");
                let message = parts.next().unwrap_or("");
                server.push_status(pane_id(1), status, message);
                println!("OK status {status}");
            }
            Some("snapshot") => {
                let mut bytes = b"\x1b[2J\x1b[H".to_vec();
                bytes.extend_from_slice(line.trim().strip_prefix("snapshot").unwrap_or("").trim().as_bytes());
                server.queue_snapshot(pane_id(1), &bytes);
                println!("OK snapshot");
            }
            Some("size") => {
                // size <rows> <cols> [owner name]: another client takes the PTY size (AD-020).
                let rows: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
                let cols: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
                let owner = parts.next().unwrap_or("other-client");
                server.force_size(pane_id(1), rows, cols, owner);
                println!("OK size {rows} {cols}");
            }
            Some("drop") => {
                server.drop_connections();
                println!("OK drop");
            }
            Some("quit") => {
                server.shutdown().await;
                return;
            }
            _ => {}
        }
    }
    // stdin closed: run until killed.
    std::future::pending::<()>().await;
}
