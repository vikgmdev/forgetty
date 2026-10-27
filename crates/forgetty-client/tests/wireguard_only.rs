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
    assert_eq!(n["portmapper_activity"], 0, "portmapper (UPnP, NAT-PMP, PCP): {n}");
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
    assert_eq!(n["portmapper_activity"], 0, "portmapper after a client: {n}");
    assert_eq!(n["address_lookups"], 0, "{n}");
    assert_eq!(n["relay_urls"].as_array().unwrap().len(), 0, "{n}");
    h.close().await;
    server.shutdown().await;
}

/// Several `--bind` addresses of one family: one socket each on the same port,
/// every one in the pairing payload, and a client that knows only one of them
/// pairs and connects through it. 127.0.0.1 and 127.0.0.2 are both loopback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_bind_addresses_of_one_family_each_pair_and_connect() {
    if !real_only() {
        return;
    }
    init_tracing();
    let server = TestServer::start_real_with_args(&[
        "--bind", "127.0.0.1", "--bind", "127.0.0.2", "--no-relay",
    ])
    .await;
    let n = net(&server);
    let mut sockets: Vec<std::net::SocketAddr> = n["bound_sockets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().parse().unwrap())
        .collect();
    sockets.sort();
    assert_eq!(sockets.len(), 2, "one socket per address: {n}");
    assert_eq!(sockets[0].ip().to_string(), "127.0.0.1", "{sockets:?}");
    assert_eq!(sockets[1].ip().to_string(), "127.0.0.2", "{sockets:?}");
    assert_eq!(sockets[0].port(), sockets[1].port(), "the same port: {sockets:?}");

    let qr = payload(&server);
    assert!(qr.relays_disabled);
    let mut direct = qr.direct_addrs();
    direct.sort();
    assert_eq!(direct, sockets, "every bound address is a direct address");

    for only in &sockets {
        // A fresh pairing window each time (the secret is single use), with
        // only one of the direct addresses in it.
        let mut one = payload(&server);
        one.direct = vec![only.to_string()];
        let key = SecretKey::generate(&mut rand::rng());
        let paired = pair(&one, key.clone(), "multi-bind", ClientKind::Cli)
            .await
            .unwrap_or_else(|e| panic!("pair through {only} failed: {e}"));
        assert_eq!(paired.pinned_addrs, vec![*only]);
        let mut opts = paired.options();
        opts.dial_timeout = Duration::from_secs(5);
        let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "multi-bind", "0.0.0");
        let addr = forgetty_client::server_addr(&qr.node_id, &paired.relays).unwrap();
        let h = tokio::time::timeout(
            Duration::from_secs(10),
            Client::connect_with(addr, key, info, opts),
        )
        .await
        .unwrap_or_else(|_| panic!("connect through {only} timed out"))
        .unwrap_or_else(|e| panic!("connect through {only} failed: {e}"));
        assert!(h.state().is_connected());
        h.request("get_layout", forgetty_client::proto::Value::Nil)
            .await
            .unwrap_or_else(|e| panic!("get_layout through {only}: {e}"));
        h.close().await;
    }
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

/// `--no-relay` without `--bind`: a config error. The server exits at once
/// with code 78 (the unit's `RestartPreventExitStatus`), before it makes any
/// socket or pane.
#[test]
fn no_relay_without_a_bind_address_is_refused_with_exit_78() {
    if !real_only() {
        return;
    }
    let bin = std::env::var_os("FORGETTY_REAL_SERVER").expect("FORGETTY_REAL_SERVER");
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(bin)
        .arg("--state-dir")
        .arg(dir.path().join("state"))
        .args(["server", "run", "--port", "0", "--no-relay"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run forgetty");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "exit status; stderr: {err}");
    assert!(err.contains("no bind address"), "stderr: {err}");
}

/// An address that is not on this machine (TEST-NET-1): the bind fails, and
/// the process stays up with its local socket and panes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bind_address_that_is_not_there_does_not_end_the_server() {
    if !real_only() {
        return;
    }
    let server =
        TestServer::start_real_on(0, &["--bind", "192.0.2.1", "--no-relay"], false).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let pid = server.pid().expect("pid");
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists(), "the server exited");
    // The local socket and the panes work, the iroh side says it is not up.
    let pane = server.add_pane();
    assert!(!pane.is_nil());
    let st = server.qa_state();
    assert!(st["udp_port"].is_null(), "udp_port: {}", st["udp_port"]);
    let err = server.socket_call("pair_open", serde_json::json!({})).unwrap_err();
    assert!(err.contains("unavailable") || err.contains("not"), "pair_open error: {err}");
    // A stop still works without an endpoint.
    server.shutdown().await;
}

/// The rebind fails and the old set cannot be bound again either (a test hook
/// refuses every bind that includes 127.0.0.1). The server must not stay
/// without an endpoint: it keeps trying, binds what it can (127.0.0.2 alone),
/// serves a client there, and binds the full set once the hook lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_rebind_and_fallback_is_retried_until_the_server_binds_again() {
    if !real_only() {
        return;
    }
    init_tracing();
    let server = TestServer::start_real_hiding(
        &["--bind", "127.0.0.1", "--bind", "127.0.0.2", "--no-relay"],
        &["127.0.0.2"],
        true,
    )
    .await;
    let (first, _paired) = paired_client(&server, Duration::from_secs(5)).await;
    assert!(first.state().is_connected());
    let data = server.state_dir().unwrap().join("data/forgetty");
    let sockets = |server: &TestServer| -> Vec<String> {
        net(server)["bound_sockets"]
            .as_array()
            .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    };

    std::fs::write(data.join("qa-fail-bind"), "127.0.0.1\n").unwrap();
    server.show_addresses();
    // The rebind and the fallback fail; the retry binds 127.0.0.2 alone.
    wait_for("the server to bind 127.0.0.2 alone", Duration::from_secs(60), || {
        let s = sockets(&server);
        s.len() == 1 && s[0].starts_with("127.0.0.2:")
    })
    .await;
    let log =
        std::fs::read_to_string(server.state_dir().unwrap().parent().unwrap().join("server.log"))
            .unwrap_or_default();
    assert!(log.contains("retrying in the background"), "the fallback never failed:\n{log}");
    let only = sockets(&server)[0].parse::<std::net::SocketAddr>().unwrap();

    // A client that knows only that address pairs and connects.
    let mut one = payload(&server);
    one.direct = vec![only.to_string()];
    let key = SecretKey::generate(&mut rand::rng());
    let paired = pair(&one, key.clone(), "after-fallback", ClientKind::Cli)
        .await
        .unwrap_or_else(|e| panic!("pair through {only} failed: {e}"));
    let mut opts = paired.options();
    opts.dial_timeout = Duration::from_secs(5);
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "after-fallback", "0.0.0");
    let addr = forgetty_client::server_addr(&one.node_id, &paired.relays).unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_with(addr, key, info, opts),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    assert!(second.state().is_connected());
    second.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");

    // The hook lets go: the watcher adds 127.0.0.1 again.
    std::fs::write(data.join("qa-fail-bind"), "").unwrap();
    wait_for("the server to bind both addresses", Duration::from_secs(90), || {
        sockets(&server).len() == 2
    })
    .await;
    wait_for("the first client to be connected again", Duration::from_secs(30), || {
        first.state().is_connected()
    })
    .await;
    second.close().await;
    first.close().await;
    server.shutdown().await;
}

/// While the server has no endpoint and retries in the background, a pairing
/// payload does not wait for a rebind that is not going to end: it answers at
/// once (with no direct address), and the server binds again when the hook lets
/// go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pairing_payload_does_not_wait_while_the_server_retries_its_bind() {
    if !real_only() {
        return;
    }
    init_tracing();
    let server = TestServer::start_real_hiding(
        &["--bind", "127.0.0.1", "--bind", "127.0.0.2", "--no-relay"],
        &["127.0.0.2"],
        true,
    )
    .await;
    let data = server.state_dir().unwrap().join("data/forgetty");
    std::fs::write(data.join("qa-fail-bind"), "127.0.0.1\n127.0.0.2\n").unwrap();
    server.show_addresses();
    wait_for("the server to have no socket", Duration::from_secs(40), || {
        net(&server)["bound_sockets"].as_array().is_some_and(|a| a.is_empty())
    })
    .await;
    // Past the 4 s of the rebind itself, the background retry runs.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let t0 = Instant::now();
    let qr = payload(&server);
    assert!(t0.elapsed() < Duration::from_secs(2), "the payload waited {:?}", t0.elapsed());
    assert!(qr.direct_addrs().is_empty(), "{:?}", qr.direct_addrs());
    std::fs::write(data.join("qa-fail-bind"), "").unwrap();
    wait_for("the server to bind again", Duration::from_secs(40), || {
        net(&server)["bound_sockets"].as_array().is_some_and(|a| !a.is_empty())
    })
    .await;
    server.shutdown().await;
}

/// Neither address is there at start: the server waits, then binds the ones
/// that appeared (both here, in one go).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_bind_address_there_the_server_waits_and_binds_when_they_appear() {
    if !real_only() {
        return;
    }
    let server = TestServer::start_real_hiding(
        &["--bind", "127.0.0.1", "--bind", "127.0.0.2", "--no-relay"],
        &["127.0.0.1", "127.0.0.2"],
        false,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(server.qa_state()["udp_port"].is_null(), "bound with no address there");
    server.show_addresses();
    wait_for("the endpoint to come up", Duration::from_secs(25), || {
        !server.qa_state()["udp_port"].is_null()
    })
    .await;
    let n = net(&server);
    assert_eq!(n["bound_sockets"].as_array().unwrap().len(), 2, "{n}");
    let (h, _paired) = paired_client(&server, Duration::from_secs(5)).await;
    assert!(h.state().is_connected());
    h.close().await;
    server.shutdown().await;
}

/// `--bind A --bind B` with B not on the machine at start: the server serves on
/// A at once. When B appears the endpoint is bound again with both: the same
/// node id and port, a client pins B and connects, and the client that was on A
/// reconnects. (127.0.0.2 is hidden from the server by a test hook, so no root
/// is needed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bind_address_that_comes_up_later_is_added_without_a_restart() {
    if !real_only() {
        return;
    }
    init_tracing();
    let server = TestServer::start_real_hiding(
        &["--bind", "127.0.0.1", "--bind", "127.0.0.2", "--no-relay"],
        &["127.0.0.2"],
        true,
    )
    .await;
    let before: Vec<std::net::SocketAddr> = net(&server)["bound_sockets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(before.len(), 1, "only the existing address is bound: {before:?}");
    assert_eq!(before[0].ip().to_string(), "127.0.0.1");
    let log = std::fs::read_to_string(server.state_dir().unwrap().parent().unwrap().join("server.log"))
        .unwrap_or_default();
    assert!(log.contains("--bind 127.0.0.2 is not on this machine yet"), "no WARN:\n{log}");
    let node_id = server.node_id();

    // A client on the first address.
    let (first, _paired) = paired_client(&server, Duration::from_secs(5)).await;
    assert!(first.state().is_connected());
    first.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");
    let pane = server.add_pane();
    assert!(!pane.is_nil());

    // The second address appears.
    server.show_addresses();
    wait_for("the endpoint to bind both addresses", Duration::from_secs(30), || {
        net(&server)["bound_sockets"].as_array().is_some_and(|a| a.len() == 2)
    })
    .await;
    let mut after: Vec<std::net::SocketAddr> = net(&server)["bound_sockets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().parse().unwrap())
        .collect();
    after.sort();
    assert_eq!(after[0].port(), before[0].port(), "the port stays: {after:?}");
    assert_eq!(after[1].port(), before[0].port(), "the port stays: {after:?}");
    assert_eq!(after[1].ip().to_string(), "127.0.0.2");
    assert_eq!(server.node_id(), node_id);
    assert_eq!(payload(&server).node_id, node_id.to_string());

    // The client that was connected comes back by itself, with its pane.
    wait_for("the first client to be connected again", Duration::from_secs(30), || {
        first.state().is_connected()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    first
        .request("get_layout", forgetty_client::proto::Value::Nil)
        .await
        .expect("get_layout after the rebind");

    // A client that knows only the new address pairs and connects through it.
    let mut one = payload(&server);
    one.direct = vec![after[1].to_string()];
    let key = SecretKey::generate(&mut rand::rng());
    let paired = pair(&one, key.clone(), "late-bind", ClientKind::Cli)
        .await
        .unwrap_or_else(|e| panic!("pair through {} failed: {e}", after[1]));
    assert_eq!(paired.pinned_addrs, vec![after[1]]);
    let mut opts = paired.options();
    opts.dial_timeout = Duration::from_secs(5);
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "late-bind", "0.0.0");
    let addr = forgetty_client::server_addr(&one.node_id, &paired.relays).unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_with(addr, key, info, opts),
    )
    .await
    .expect("connect through the new address timed out")
    .expect("connect through the new address failed");
    assert!(second.state().is_connected());
    second.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");
    second.close().await;
    first.close().await;
    server.shutdown().await;
}


/// The address appears later (here: the port is held by another socket at
/// first). The server retries in the background and comes up by itself, and a
/// client can pair with it. A real tunnel address needs root to add, so the
/// "address appears" case is the same code path with a busy port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_endpoint_binds_in_the_background_when_the_address_becomes_usable() {
    if !real_only() {
        return;
    }
    let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let server =
        TestServer::start_real_on(port, &["--bind", "127.0.0.1", "--no-relay"], false).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(server.qa_state()["udp_port"].is_null(), "bound while the port was taken");
    let pane = server.add_pane();
    assert!(!pane.is_nil());

    drop(holder);
    // Backoff is 1, 2, 4, 8, 10 s: up within the first 10 s tick.
    wait_for("the endpoint to come up", Duration::from_secs(25), || {
        !server.qa_state()["udp_port"].is_null()
    })
    .await;
    assert_eq!(server.qa_state()["udp_port"], port);
    let n = net(&server);
    assert_eq!(n["relay_activity"], 0, "{n}");
    assert_eq!(n["portmapper_activity"], 0, "{n}");
    // And it serves: pair, connect, use the pane that was made before.
    let (h, _paired) = paired_client(&server, Duration::from_secs(5)).await;
    assert!(h.state().is_connected());
    h.close().await;
    server.shutdown().await;
}

/// A config file that exists but does not parse stops the server with exit
/// 78 and an error that names the file and the line. It never falls back to
/// the defaults: a typo in `relay = "none"` would turn the relays back on.
#[test]
fn a_config_file_that_does_not_parse_stops_the_server_with_exit_78() {
    if !real_only() {
        return;
    }
    let bin = std::env::var_os("FORGETTY_REAL_SERVER").expect("FORGETTY_REAL_SERVER");
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "bind_addrs = [\"127.0.0.1\"]\nrelay = \"none\n").unwrap();
    let out = std::process::Command::new(bin)
        .arg("--state-dir")
        .arg(dir.path().join("state"))
        .args(["server", "run", "--port", "0", "--config-file"])
        .arg(&cfg)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run forgetty");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "exit status; stderr: {err}");
    assert!(err.contains("config.toml") && err.contains("line 2"), "stderr: {err}");
}

/// `forgetty server start` reads the exit status 78 of the server it spawned
/// and prints the real error at once (file and line), where it used to read
/// 75 as "another start is under way" and wait 20 s. The config sits at the
/// default location of a throwaway `--state-dir`.
#[test]
fn server_start_prints_a_config_error_at_once() {
    if !real_only() {
        return;
    }
    let bin = std::env::var_os("FORGETTY_REAL_SERVER").expect("FORGETTY_REAL_SERVER");
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cfg_dir = state.join("config/forgetty");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "bind_addrs = [\"127.0.0.1\"]\nrelay = \"none\n")
        .unwrap();
    let t0 = Instant::now();
    let out = std::process::Command::new(bin)
        .arg("--state-dir")
        .arg(&state)
        .args(["server", "start"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run forgetty");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "server start succeeded; stderr: {err}");
    assert!(t0.elapsed() < Duration::from_secs(10), "took {:?}", t0.elapsed());
    assert!(err.contains("config.toml") && err.contains("line 2"), "stderr: {err}");
}

/// Unusable pinned addresses (unspecified, multicast, port 0) given through
/// `ClientOptions::pinned_addrs` are dropped by the library: the client
/// connects on the good one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unusable_pinned_addresses_do_not_get_in_the_way() {
    init_tracing();
    let server = TestServer::start(common::TestServerConfig::default()).await;
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let opts = forgetty_client::ClientOptions {
        relays_disabled: true,
        pinned_addrs: vec![
            "0.0.0.0:47297".parse().unwrap(),
            "224.0.0.1:47297".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
            server.udp_addr(),
        ],
        ..Default::default()
    };
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "pins", "0.0.0");
    let h = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect_with(forgetty_client::EndpointAddr::new(server.node_id()), key, info, opts),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    assert!(h.state().is_connected());
    h.close().await;
    server.shutdown().await;
}

/// Run `forgetty server run` with `args` (and an optional config file) and
/// return its exit code and stderr.
fn run_server_once(args: &[&str], config: Option<&str>) -> (Option<i32>, String) {
    let bin = std::env::var_os("FORGETTY_REAL_SERVER").expect("FORGETTY_REAL_SERVER");
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--state-dir").arg(dir.path().join("state")).args(["server", "run", "--port", "0"]);
    if let Some(text) = config {
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, text).unwrap();
        cmd.arg("--config-file").arg(cfg);
    }
    let out = cmd.args(args).stdin(std::process::Stdio::null()).output().expect("run forgetty");
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Every configuration error is exit 78: a typo of a guarded key, a bad
/// `relay` value, a port with `--bind`, a bad relay URL, and the
/// `--no-relay` plus relay URL conflict.
#[test]
fn every_configuration_error_exits_78() {
    // The settings editor refuses the same cases as config text: see
    // `every_server_configuration_error_is_refused_by_the_editor_too` in
    // crates/forgetty-config/src/remote.rs. Keep the two lists in step.
    if !real_only() {
        return;
    }
    let cases: Vec<(&[&str], Option<&str>, &str)> = vec![
        (&["--bind", "127.0.0.1"], Some("rellay = \"none\"\n"), "did you mean `relay`"),
        (&["--bind", "127.0.0.1"], Some("bind_addr = [\"127.0.0.1\"]\n"), "bind_addrs"),
        (&["--bind", "127.0.0.1"], Some("relay = \"nne\"\n"), "invalid relay setting"),
        (&["--no-relay", "--bind", "0.0.0.0"], None, "no address a device can pair with"),
        (&["--no-relay", "--bind", "169.254.1.1"], None, "no address a device can pair with"),
        (&["--no-relay", "--bind", "127.0.0.1:47297"], None, "invalid bind address"),
        (&["--bind", "127.0.0.1", "--relay-url", "ftp://x.example.org"], None, "relay URL"),
        (
            &["--no-relay", "--bind", "127.0.0.1", "--relay-url", "https://r.example.org"],
            None,
            "relay URLs are set",
        ),
    ];
    for (args, config, needle) in cases {
        let (code, err) = run_server_once(args, config);
        assert_eq!(code, Some(78), "args {args:?} config {config:?}; stderr: {err}");
        assert!(err.contains(needle), "args {args:?}: wanted {needle:?} in: {err}");
    }
}
