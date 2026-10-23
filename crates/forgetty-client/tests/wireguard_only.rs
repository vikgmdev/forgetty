//! A WireGuard-only server (`--bind 127.0.0.1 --no-relay`): it binds only the
//! given address, never talks to a relay or to n0, and puts the bound address
//! and a "no relays" flag into the pairing payload. A client paired with it
//! uses no relay, dials the pinned address, and says
//! `unreachable_direct_only` when nothing answers. Needs the real server
//! (`FORGETTY_REAL_SERVER`, built with `--features qa-tools`).

mod common;

use std::time::{Duration, Instant};

use common::{real_only, TestServer};
use forgetty_client::proto::{ClientKind, QrPayload};
use forgetty_client::{pair, Client, ClientHandle, ClientInfo, PairedServer, SecretKey};
use uuid::Uuid;

const ARGS: [&str; 3] = ["--bind", "127.0.0.1", "--no-relay"];

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

async fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out after {limit:?} waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A pairing window and its payload, as the app would scan it.
fn payload(server: &TestServer) -> QrPayload {
    let v = server.socket_call("pair_open", serde_json::json!({})).expect("pair_open");
    serde_json::from_value(v["payload"].clone()).expect("QrPayload")
}

fn net(server: &TestServer) -> serde_json::Value {
    server.socket_call("qa_net_services", serde_json::json!({})).expect("qa_net_services")
}

async fn paired_client(server: &TestServer, dial_timeout: Duration) -> (ClientHandle, PairedServer) {
    init_tracing();
    let qr = payload(server);
    let key = SecretKey::generate(&mut rand::rng());
    let paired = pair(&qr, key.clone(), "wg-test", ClientKind::Cli).await.expect("pair failed");
    let mut opts = paired.options();
    opts.dial_timeout = dial_timeout;
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "wireguard-only", "0.0.0");
    // The server address the app stores is the node id alone: only the pinned
    // addresses lead there.
    let addr = forgetty_client::server_addr(&qr.node_id, &paired.relays).unwrap();
    let h = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_with(addr, key, info, opts),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    (h, paired)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_binds_only_its_address_and_reaches_nobody_outside() {
    if !real_only() {
        return;
    }
    let server = TestServer::start_real_with_args(&ARGS).await;
    let n = net(&server);
    assert_eq!(n["relays_disabled"], true, "{n}");
    assert_eq!(n["relay_urls"].as_array().unwrap().len(), 0, "relay urls: {n}");
    assert_eq!(n["address_lookups"], 0, "address lookups (n0 DNS / pkarr): {n}");
    assert_eq!(n["relay_activity"], 0, "relay activity: {n}");
    let sockets: Vec<String> =
        n["bound_sockets"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().into()).collect();
    assert!(!sockets.is_empty(), "{n}");
    assert!(sockets.iter().all(|s| s.starts_with("127.0.0.1:")), "bound sockets: {sockets:?}");

    // The payload: no relay, the flag, the bound address.
    let qr = payload(&server);
    assert_eq!(qr.relay, "", "payload relay");
    assert!(qr.relays.is_empty(), "payload relays: {:?}", qr.relays);
    assert!(qr.relays_disabled);
    assert_eq!(qr.direct, sockets, "payload direct addresses");

    // Traffic changes nothing: still no relay, no lookup.
    let (h, paired) = paired_client(&server, Duration::from_secs(5)).await;
    assert!(paired.relays_disabled);
    assert!(paired.relays.is_empty());
    assert_eq!(paired.pinned_addrs.iter().map(ToString::to_string).collect::<Vec<_>>(), sockets);
    assert!(h.state().is_connected());
    h.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");
    let n = net(&server);
    assert_eq!(n["relay_activity"], 0, "relay activity after a client: {n}");
    assert_eq!(n["address_lookups"], 0, "{n}");
    assert_eq!(n["relay_urls"].as_array().unwrap().len(), 0, "{n}");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frozen_server_is_unreachable_direct_only_and_the_client_comes_back() {
    if !real_only() {
        return;
    }
    let server = TestServer::start_real_with_args(&ARGS).await;
    let pane = server.initial_pane(0);
    let (h, _paired) = paired_client(&server, Duration::from_secs(2)).await;
    assert!(h.state().is_connected());
    let before = server.handshakes();

    server.freeze(Duration::from_secs(14));
    // Something to send: the client notices the dead link while it waits for
    // the answer.
    let _ = h.send_input(pane, b"x").await;
    let seen_unreachable = {
        let mut rx = h.watch_state();
        let t0 = Instant::now();
        let mut found = false;
        while t0.elapsed() < Duration::from_secs(13) {
            let name = rx.borrow().name();
            if name == "unreachable_direct_only" {
                found = true;
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        }
        found
    };
    assert!(seen_unreachable, "state after the freeze: {}", h.state().name());
    assert!(!h.state().is_connected());

    // The server resumes: the client reconnects by itself.
    wait_for("the reconnect", Duration::from_secs(40), || h.state().is_connected()).await;
    assert!(server.handshakes() > before, "no new handshake after the resume");
    assert_eq!(net(&server)["relay_activity"], 0);
    h.close().await;
    server.shutdown().await;
}

/// The client half, on the mock or the real server: a client with
/// `relays_disabled` and a pinned address connects on that address alone,
/// says `unreachable_direct_only` while the server hangs, and is back after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relayless_client_connects_on_its_pinned_address_and_reports_a_hung_server() {
    init_tracing();
    let server = TestServer::start(common::TestServerConfig { own_runtime: true, ..Default::default() }).await;
    let pane = server.add_pane();
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let opts = forgetty_client::ClientOptions {
        relays_disabled: true,
        pinned_addrs: vec![server.udp_addr()],
        dial_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "relayless", "0.0.0");
    let h = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_with(forgetty_client::EndpointAddr::new(server.node_id()), key, info, opts),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    assert!(h.state().is_connected());

    server.freeze(Duration::from_secs(12));
    let _ = h.send_input(pane, b"x").await;
    let mut rx = h.watch_state();
    let t0 = Instant::now();
    while rx.borrow().name() != "unreachable_direct_only" {
        assert!(
            t0.elapsed() < Duration::from_secs(11),
            "no unreachable_direct_only; state: {}",
            h.state().name()
        );
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
    }
    wait_for("the reconnect", Duration::from_secs(40), || h.state().is_connected()).await;
    h.close().await;
    server.shutdown().await;
}
