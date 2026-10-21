//! Pair with a server and connect to it through its relay only.
//!
//! Used by `scripts/p6_gate.sh` to prove that a self-hosted `iroh-relay` works:
//! the client binds no IP socket (`ClientOptions::relay_only`), so the pairing
//! and the connection can only travel over the relay named in the payload.
//!
//! ```text
//! cargo run -p forgetty-client --example relay_probe -- \
//!     --payload '<json from forgetty pair>' --dir /tmp/probe-state
//! ```
//!
//! Options: `--no-pair` (already paired, only connect), `--timeout SECS`
//! (wait for the connection, default 30), `--allow-direct` (bind IP sockets).
//!
//! Prints `RELAY_PROBE_OK ...` and exits 0, or a reason and exits 1.

use std::path::PathBuf;
use std::time::Duration;

use forgetty_client::proto::{ClientKind, QrPayload};
use forgetty_client::{
    load_or_generate_client_id, load_or_generate_identity, pair_with_addr, qr_to_addr, Client,
    ClientInfo, ClientOptions, ConnectionState,
};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match run(&args).await {
        Ok(line) => println!("RELAY_PROBE_OK {line}"),
        Err(e) => {
            eprintln!("RELAY_PROBE_FAIL {e}");
            std::process::exit(1);
        }
    }
}

async fn run(args: &[String]) -> Result<String, String> {
    let payload = arg(args, "--payload").ok_or("missing --payload")?;
    let dir = PathBuf::from(arg(args, "--dir").ok_or("missing --dir")?);
    let direct = args.iter().any(|a| a == "--allow-direct");
    let no_pair = args.iter().any(|a| a == "--no-pair");
    let timeout: u64 = arg(args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(30);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let qr = QrPayload::parse(&payload).map_err(|e| e.to_string())?;
    if qr.relay_urls().is_empty() {
        return Err("the payload names no relay".into());
    }
    let addr = qr_to_addr(&qr).map_err(|e| e.to_string())?;
    let identity =
        load_or_generate_identity(&dir.join("identity.key")).map_err(|e| e.to_string())?;
    let client_id =
        load_or_generate_client_id(&dir.join("client_id")).map_err(|e| e.to_string())?;
    let opts = ClientOptions { relay_only: !direct, ..ClientOptions::default() };

    if !no_pair {
        pair_with_addr(
            addr.clone(),
            &qr.secret,
            identity.clone(),
            "relay-probe",
            ClientKind::Cli,
            &opts,
        )
        .await
        .map_err(|e| format!("pairing: {e}"))?;
    }

    let info = ClientInfo::new(client_id, ClientKind::Cli, "relay-probe", "0");
    let handle =
        Client::spawn(addr, identity, info, opts).await.map_err(|e| format!("connect: {e}"))?;
    let mut state = handle.watch_state();
    let wait = async {
        loop {
            match state.borrow().clone() {
                ConnectionState::Connected => return Ok(()),
                ConnectionState::Failed(why) => return Err(format!("failed: {why}")),
                _ => {}
            }
            if state.changed().await.is_err() {
                return Err("client stopped".to_string());
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(timeout), wait)
        .await
        .map_err(|_| "no connection before the timeout".to_string())??;

    let layout = handle
        .request("get_layout", forgetty_client::proto::Value::Map(vec![]))
        .await
        .map_err(|e| format!("get_layout: {e}"))?;
    handle.close().await;
    Ok(format!(
        "machine={} relays={} direct_paths={} layout_bytes={}",
        qr.machine,
        qr.relay_urls().join(","),
        if direct { "allowed" } else { "disabled" },
        format!("{layout:?}").len()
    ))
}
