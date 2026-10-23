//! Time from a replaced endpoint to Connected (the airplane-mode redial).
//!
//! Pairs with a real server (as `relay_probe` does), connects, then makes the
//! socket of the client's endpoint useless (`blackhole_endpoint_for_tests`) and
//! calls `network_changed()`: the client waits 1.5 s, binds a fresh endpoint with
//! the same key and dials again. Prints `MARK` lines with the time since the
//! network change. With `RUST_LOG=forgetty_client=debug` the client's own dial
//! log shows where the time goes.
//!
//! `--direct-addr IP:PORT` seeds the direct address cache (a server that is
//! reachable directly). Point the payload relay at a closed port to model a relay
//! that never comes up.
//!
//! `--gate FILE --usable-after-ms N` models a network that is not usable yet: the
//! file is created before `network_changed()` and removed N ms later (a test
//! relay proxy refuses connections while the file exists). The line
//! `MARK network usable` is that moment; the last line gives the time from it
//! to Connected.
//!
//! ```text
//! RUST_LOG=forgetty_client=debug cargo run -p forgetty-client --example replace_timeline -- \
//!     --payload '<json from forgetty pair>' --dir /tmp/tl-state [--relay-only] [--no-pair]
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .init();
    if let Err(e) = run(&args).await {
        eprintln!("TIMELINE_FAIL {e}");
        std::process::exit(1);
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let payload = arg(args, "--payload").ok_or("missing --payload")?;
    let dir = PathBuf::from(arg(args, "--dir").ok_or("missing --dir")?);
    let relay_only = args.iter().any(|a| a == "--relay-only");
    let no_pair = args.iter().any(|a| a == "--no-pair");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let qr = QrPayload::parse(&payload).map_err(|e| e.to_string())?;
    let addr = qr_to_addr(&qr).map_err(|e| e.to_string())?;
    let identity =
        load_or_generate_identity(&dir.join("identity.key")).map_err(|e| e.to_string())?;
    let client_id =
        load_or_generate_client_id(&dir.join("client_id")).map_err(|e| e.to_string())?;
    let opts = ClientOptions { relay_only, ..ClientOptions::default() };
    // `--direct-addr IP:PORT` seeds the address cache, as a previous direct
    // connection would have left it (the pairing payload names no direct address).
    let cache = opts.addr_cache.clone();
    let direct: Option<std::net::SocketAddr> = match arg(args, "--direct-addr") {
        Some(a) => Some(a.parse().map_err(|e| format!("--direct-addr: {e}"))?),
        None => None,
    };
    if let Some(a) = direct {
        cache.insert(addr.id, a, forgetty_client::local_network());
    }
    let server_id = addr.id;
    if !no_pair {
        pair_with_addr(
            addr.clone(),
            &qr.secret,
            identity.clone(),
            "timeline",
            ClientKind::Cli,
            &opts,
        )
        .await
        .map_err(|e| format!("pairing: {e}"))?;
    }
    let info = ClientInfo::new(client_id, ClientKind::Cli, "timeline", "0");
    let t_spawn = Instant::now();
    let h = Client::spawn(addr, identity, info, opts).await.map_err(|e| format!("spawn: {e}"))?;
    let mut state = h.watch_state();
    wait_connected(&mut state, Duration::from_secs(30)).await?;
    println!("MARK first connect {:?} after spawn", t_spawn.elapsed());
    // `--settle-ms N`: wait this long after the first connect (default 2000). A
    // short wait keeps a seeded direct address in the cache: later iroh learns
    // the server's real addresses and the cache follows the selected path.
    let settle: u64 = arg(args, "--settle-ms").and_then(|v| v.parse().ok()).unwrap_or(2000);
    tokio::time::sleep(Duration::from_millis(settle)).await;

    // The first connection may have taught the cache the server's real address
    // (iroh learns it in the handshake). Put the seeded one back, so the replace
    // dials it (a delaying test proxy then shows the cost of the direct path).
    if let Some(a) = direct {
        cache.insert(server_id, a, forgetty_client::local_network());
    }
    let gate = arg(args, "--gate").map(PathBuf::from);
    let usable_after: Option<u64> = arg(args, "--usable-after-ms").and_then(|v| v.parse().ok());
    if let Some(g) = &gate {
        std::fs::write(g, b"closed").map_err(|e| e.to_string())?;
    }
    let usable_at = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
    if let (Some(g), Some(ms)) = (gate.clone(), usable_after) {
        let usable_at = usable_at.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let _ = std::fs::remove_file(&g);
            *usable_at.lock().unwrap() = Some(Instant::now());
            println!("MARK network usable");
        });
    }
    h.blackhole_endpoint_for_tests();
    // Airplane mode: by the time the network is back, the connection is gone.
    // `--live-link` calls network_changed() right away instead.
    if !args.iter().any(|a| a == "--live-link") {
        let mut w = h.watch_state();
        while matches!(*w.borrow(), ConnectionState::Connected) {
            if w.changed().await.is_err() {
                return Err("client stopped".into());
            }
        }
    }
    let t0 = Instant::now();
    println!("MARK +0 ms: socket gone, no connection, network_changed()");
    h.network_changed().await;
    loop {
        if state.changed().await.is_err() {
            return Err("client stopped".into());
        }
        let s = state.borrow().clone();
        println!("MARK +{:?}: state {s:?}", t0.elapsed());
        if matches!(s, ConnectionState::Connected) {
            break;
        }
        if t0.elapsed() > Duration::from_secs(60) {
            return Err("no reconnect in 60 s".into());
        }
    }
    println!("TIMELINE_OK connected {:?} after network_changed", t0.elapsed());
    if let Some(u) = *usable_at.lock().unwrap() {
        println!("FROM_USABLE {:?} from network usable to Connected", u.elapsed());
    }
    h.close().await;
    Ok(())
}

async fn wait_connected(
    state: &mut tokio::sync::watch::Receiver<ConnectionState>,
    limit: Duration,
) -> Result<(), String> {
    let wait = async {
        loop {
            if matches!(*state.borrow(), ConnectionState::Connected) {
                return Ok(());
            }
            if state.changed().await.is_err() {
                return Err("client stopped".to_string());
            }
        }
    };
    tokio::time::timeout(limit, wait).await.map_err(|_| "no connection in time".to_string())?
}
