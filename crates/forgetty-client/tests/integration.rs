//! Protocol v2 integration tests over real iroh endpoints on localhost
//! (direct addresses, no relay). Every scenario runs against the in-process
//! test server and, with `FORGETTY_REAL_SERVER` set, against the real server.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::{
    error_code, write_msg, ClientKind, ErrorMsg, HandshakeReply, Hello, ServerEvent, Value,
    PROTO_MAJOR,
};
use forgetty_client::{
    Client, ClientError, ClientHandle, ClientInfo, ClientOptions, ConnectionState, EventItem,
    PaneItem, PaneStream, SecretKey,
};
use uuid::Uuid;

fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

fn new_identity() -> SecretKey {
    SecretKey::generate(&mut rand::rng())
}

fn info() -> ClientInfo {
    ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "test-client", "0.0.0")
}

async fn connect(server: &TestServer) -> ClientHandle {
    tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect(server.addr(), new_identity(), info()),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed")
}

async fn wait_state(h: &ClientHandle, want: impl Fn(&ConnectionState) -> bool, what: &str) {
    let mut rx = h.watch_state();
    let res = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if want(&rx.borrow_and_update()) {
                return;
            }
            rx.changed().await.expect("state channel closed");
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting for state: {what}, now {:?}", h.state());
}

fn is_reconnecting(s: &ConnectionState) -> bool {
    matches!(s, ConnectionState::Reconnecting { .. })
}

/// Deterministic test payload: byte `i` of the pane.
fn pattern(i: u64) -> u8 {
    (i.wrapping_mul(31).wrapping_add(7) % 251) as u8
}

fn pattern_bytes(from: u64, to: u64) -> Vec<u8> {
    (from..to).map(pattern).collect()
}

/// Read items until `total` bytes of data arrived. Checks offsets are
/// contiguous. Returns the bytes and the number of `Reset` items seen.
async fn read_exact_bytes(
    stream: &mut PaneStream,
    start: u64,
    total: u64,
    timeout: Duration,
) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut resets = 0;
    let mut next = start;
    tokio::time::timeout(timeout, async {
        while (out.len() as u64) < total {
            match stream.next().await.expect("stream ended early") {
                PaneItem::Reset { .. } => resets += 1,
                PaneItem::Data { offset, bytes } => {
                    assert_eq!(offset, next, "gap or duplicate before offset {offset}");
                    next += bytes.len() as u64;
                    out.extend_from_slice(&bytes);
                }
                PaneItem::Ended => panic!("unexpected Ended"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: got {} of {total} bytes", out.len()));
    (out, resets)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_and_requests() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let h = connect(&server).await;
    assert_eq!(h.state(), ConnectionState::Connected);

    let w = h.welcome().expect("welcome");
    assert_eq!(w.proto_major, PROTO_MAJOR);
    assert!(!w.hostname.is_empty());
    assert!(h.has_capability("resume"));
    if server.is_real() {
        // The real server offers every capability the client lists, and capture.
        assert!(h.has_capability("status_events"));
        assert!(h.has_capability("layout_events"));
        assert!(h.has_capability("size_policy_latest"));
        assert!(!h.has_capability("capture"), "the client does not list capture");
    } else {
        // Negotiated: both sides listed `resume`; only the client lists status_events.
        assert_eq!(w.hostname, "test-host");
        assert!(!h.has_capability("status_events"));
        assert!(!h.has_capability("capture"));
    }

    // list_clients shows this client.
    let clients = h.request("list_clients", Value::Map(vec![])).await.unwrap();
    assert_eq!(clients.as_array().unwrap().len(), 1);

    // Unknown method comes back as a Failure.
    match h.request("no_such_method", Value::Map(vec![])).await {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, error_code::UNKNOWN_METHOD),
        other => panic!("expected unknown_method, got {other:?}"),
    }

    // resize reaches the server.
    let pane = server.add_pane();
    h.resize(pane, 40, 120).await.unwrap();
    assert_eq!(server.pane_size(pane), (40, 120));

    // Unknown pane: attach fails cleanly and leaves nothing registered.
    match h.attach_pane(Uuid::new_v4()).await {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, error_code::NOT_FOUND),
        Ok(_) => panic!("attach of unknown pane succeeded"),
        Err(e) => panic!("unexpected error {e:?}"),
    }
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn proto_mismatch_server_side_error_frame() {
    init_log();
    // A raw client that claims proto_major 3 gets `Error { proto_mismatch }`.
    let server = TestServer::start(TestServerConfig::default()).await;
    let ep = iroh::Endpoint::empty_builder().secret_key(new_identity()).bind().await.unwrap();
    let conn = ep.connect(server.addr(), forgetty_client::proto::ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut hello = Hello::new(Uuid::new_v4(), Uuid::new_v4(), ClientKind::Cli, "raw", "0", vec![]);
    hello.proto_major = 3;
    write_msg(&mut send, &hello).await.unwrap();
    let reply: HandshakeReply = forgetty_client::proto::read_msg(&mut recv).await.unwrap().unwrap();
    match reply {
        HandshakeReply::Error(ErrorMsg { code, .. }) => {
            assert_eq!(code, error_code::PROTO_MISMATCH)
        }
        other => panic!("expected Error, got {other:?}"),
    }
    conn.close(0u8.into(), b"done");
    ep.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn proto_mismatch_client_side_fails_for_good() {
    init_log();
    if common::is_real() {
        // Needs a server that answers with another major; the real one never does.
        println!("skipped against the real server: it always speaks major 2");
        return;
    }
    // A server that answers with a different major: the client must stop.
    let cfg = TestServerConfig { proto_major: 3, ..Default::default() };
    let server = TestServer::start(cfg).await;
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect(server.addr(), new_identity(), info()),
    )
    .await
    .expect("must not hang");
    match res {
        Err(ClientError::Failed(reason)) => {
            assert!(reason.contains(error_code::PROTO_MISMATCH), "reason: {reason}")
        }
        Ok(_) => panic!("connected to a v3 server"),
        Err(e) => panic!("unexpected error {e:?}"),
    }
    server.shutdown().await;
}

#[tokio::test]
async fn not_authorized_then_pair_then_connect() {
    init_log();
    let cfg = TestServerConfig {
        authorized: Some(HashSet::new()),
        pair_secret: Some("SECRETSECRET".into()),
        ..Default::default()
    };
    let server = TestServer::start(cfg).await;
    let secret = server.pair_secret();
    let key = new_identity();

    // Unknown node: close code 0x01, the client stops with a clear reason.
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect(server.addr(), key.clone(), info()),
    )
    .await
    .expect("must not hang");
    match res {
        Err(ClientError::Failed(reason)) => {
            assert!(reason.contains(error_code::NOT_AUTHORIZED), "reason: {reason}")
        }
        other => panic!("expected not_authorized, got {:?}", other.map(|_| ())),
    }

    // Wrong secret is refused.
    let err = forgetty_client::pair_with_addr(
        server.addr(),
        "WRONG",
        key.clone(),
        "pixel",
        ClientKind::Android,
        &ClientOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ClientError::PairRejected(ref r) if r == "bad_secret"), "{err:?}");

    // Right secret pairs; the same identity now connects.
    forgetty_client::pair_with_addr(
        server.addr(),
        &secret,
        key.clone(),
        "pixel",
        ClientKind::Android,
        &ClientOptions::default(),
    )
    .await
    .unwrap();
    let h = Client::connect(server.addr(), key.clone(), info()).await.unwrap();
    assert_eq!(h.state(), ConnectionState::Connected);

    // The window closed after one device.
    let err = forgetty_client::pair_with_addr(
        server.addr(),
        &secret,
        new_identity(),
        "other",
        ClientKind::Android,
        &ClientOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ClientError::PairRejected(ref r) if r == "expired"), "{err:?}");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn attach_from_none_then_exactly_once_across_drops() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();

    // Continuous producer: 300 chunks of 3000 bytes, 1 ms apart.
    const CHUNKS: u64 = 300;
    const CHUNK: u64 = 3000;
    let producer = {
        let server_ref = &server;
        async move {
            for i in 0..CHUNKS {
                server_ref.append(pane, &pattern_bytes(i * CHUNK, (i + 1) * CHUNK));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    };
    // Drop the connection three times while data flows.
    let dropper = {
        let server_ref = &server;
        async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                server_ref.drop_connections();
            }
        }
    };
    let consumer = read_exact_bytes(&mut stream, 0, CHUNKS * CHUNK, Duration::from_secs(30));
    let ((), (), (got, resets)) = tokio::join!(producer, dropper, consumer);

    assert_eq!(got.len() as u64, CHUNKS * CHUNK);
    assert_eq!(got, pattern_bytes(0, CHUNKS * CHUNK), "bytes differ: gap, duplicate or reorder");
    assert_eq!(resets, 1, "only the initial attach may reset; resumes must not");
    assert_eq!(stream.offset(), Some(CHUNKS * CHUNK));

    let attaches = server.attaches();
    assert_eq!(attaches[0], (pane, None));
    assert!(attaches.len() >= 3, "expected re-attaches, got {attaches:?}");
    assert!(attaches[1..].iter().all(|(_, o)| o.is_some()), "re-attach must carry an offset");
    println!("exactly-once: {} attaches, {} bytes", attaches.len(), got.len());
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn offset_older_than_log_gives_reset() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    server.append(pane, &pattern_bytes(0, 10_000));
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    let (got, resets) = read_exact_bytes(&mut stream, 0, 10_000, Duration::from_secs(10)).await;
    assert_eq!((got, resets), (pattern_bytes(0, 10_000), 1));

    // Server unreachable for a moment; the log moves on past the client.
    server.pause_handshakes(true);
    server.drop_connections();
    wait_state(&h, is_reconnecting, "reconnecting").await;
    server.append(pane, &pattern_bytes(10_000, 110_000));
    server.truncate_log_before(pane, 50_000);
    server.pause_handshakes(false);

    // Reset, then the log from its first byte.
    let mut resets = 0;
    let mut next = 50_000u64;
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while got.len() < 60_000 {
            match stream.next().await.unwrap() {
                PaneItem::Reset { .. } => resets += 1,
                PaneItem::Data { offset, bytes } => {
                    assert_eq!(offset, next);
                    next += bytes.len() as u64;
                    got.extend_from_slice(&bytes);
                }
                PaneItem::Ended => panic!("ended"),
            }
        }
    })
    .await
    .expect("timed out waiting for replay");
    assert_eq!(resets, 1, "an old offset must reset");
    assert_eq!(got, pattern_bytes(50_000, 110_000));
    assert_eq!(server.attaches()[1], (pane, Some(10_000)));
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn input_sent_while_disconnected_arrives_in_order() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();

    // Connected input works and echoes.
    h.send_input(pane, b"hello ").await.unwrap();
    let (got, _) = read_exact_bytes(&mut stream, 0, 6, Duration::from_secs(5)).await;
    assert_eq!(got, b"hello ");

    server.pause_handshakes(true);
    server.drop_connections();
    wait_state(&h, is_reconnecting, "reconnecting").await;

    let mut expected = b"hello ".to_vec();
    for i in 0..10 {
        let piece = format!("[{i}]");
        h.send_input(pane, piece.as_bytes()).await.unwrap();
        expected.extend_from_slice(piece.as_bytes());
    }
    // Nothing reached the server yet.
    assert_eq!(server.received_input(pane), b"hello ");

    server.pause_handshakes(false);
    let want = expected.len() as u64;
    let (rest, _) = read_exact_bytes(&mut stream, 6, want - 6, Duration::from_secs(10)).await;
    assert_eq!(rest, expected[6..], "echo order");
    assert_eq!(server.received_input(pane), expected, "server saw input out of order");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn input_buffer_is_capped_at_64_kib_while_disconnected() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    server.pause_handshakes(true);
    server.drop_connections();
    wait_state(&h, is_reconnecting, "reconnecting").await;

    h.send_input(pane, &vec![b'x'; 64 * 1024]).await.expect("exactly 64 KiB fits");
    assert!(matches!(h.send_input(pane, b"y").await, Err(ClientError::InputBufferFull)));

    server.pause_handshakes(false);
    // The buffered 64 KiB arrives, nothing else.
    let t0 = Instant::now();
    while server.received_input(pane).len() < 64 * 1024 {
        assert!(t0.elapsed() < Duration::from_secs(10), "buffered input never arrived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(server.received_input(pane), vec![b'x'; 64 * 1024]);
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn events_resubscribe_after_reconnect() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    let mut events = h.events().await.unwrap();

    server.push_event(ServerEvent::TitleChanged { pane_id: pane, title: "one".into() });
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap().unwrap() {
        EventItem::Server { event: ServerEvent::TitleChanged { title, .. }, .. } => {
            assert_eq!(title, "one")
        }
        other => panic!("unexpected {other:?}"),
    }

    server.drop_connections();
    // After the redial the client re-subscribes and tells the consumer.
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap().unwrap() {
        EventItem::Resynced => {}
        other => panic!("expected Resynced, got {other:?}"),
    }
    server.push_event(ServerEvent::TitleChanged { pane_id: pane, title: "two".into() });
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap().unwrap() {
        EventItem::Server { event: ServerEvent::TitleChanged { title, .. }, .. } => {
            assert_eq!(title, "two")
        }
        other => panic!("unexpected {other:?}"),
    }
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn reconnect_time_after_forced_drop_is_under_one_second() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    let mut events = h.events().await.unwrap();
    let _ = &mut events;

    let mut times_connected = Vec::new();
    let mut times_data = Vec::new();
    for round in 0..8u64 {
        let offset = round; // one byte per round
        let handshakes = server.handshakes();
        let t0 = Instant::now();
        server.drop_connections();
        // Connected again with a fresh handshake.
        let mut rx = h.watch_state();
        loop {
            if server.handshakes() > handshakes && rx.borrow_and_update().is_connected() {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "never reconnected");
            let _ = tokio::time::timeout(Duration::from_millis(2), rx.changed()).await;
        }
        times_connected.push(t0.elapsed());
        // A byte written now reaches the client over the resumed stream.
        server.append(pane, &[pattern(offset)]);
        let (got, resets) = read_exact_bytes(&mut stream, offset, 1, Duration::from_secs(5)).await;
        assert_eq!(got, vec![pattern(offset)], "round {round}");
        assert_eq!(resets, if round == 0 { 1 } else { 0 }, "round {round}");
        times_data.push(t0.elapsed());
    }
    let max_conn = times_connected.iter().max().unwrap();
    let max_data = times_data.iter().max().unwrap();
    println!("reconnect to Connected: {times_connected:?}");
    println!("reconnect to data flowing: {times_data:?}");
    assert!(*max_conn < Duration::from_secs(1), "slowest reconnect {max_conn:?}");
    assert!(*max_data < Duration::from_secs(1), "slowest resume {max_data:?}");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn pane_close_ends_the_stream_and_reconnect_now_redials() {
    init_log();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    server.append(pane, b"bye");
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    let (got, _) = read_exact_bytes(&mut stream, 0, 3, Duration::from_secs(5)).await;
    assert_eq!(got, b"bye");

    server.close_pane(pane);
    match tokio::time::timeout(Duration::from_secs(5), stream.next()).await.unwrap() {
        Some(PaneItem::Ended) => {}
        other => panic!("expected Ended, got {other:?}"),
    }
    assert!(stream.next().await.is_none(), "nothing after Ended");
    drop(stream);
    assert!(matches!(h.attach_pane(pane).await, Err(ClientError::Server { .. })));

    // A forced redial from the client side.
    let before = server.handshakes();
    h.reconnect_now();
    let t0 = Instant::now();
    while server.handshakes() == before {
        assert!(t0.elapsed() < Duration::from_secs(5), "no redial");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.close().await;
    assert_eq!(h.state(), ConnectionState::Closed);
    server.shutdown().await;
}

fn find<'a>(v: &'a Value, key: &str) -> &'a Value {
    &v.as_map()
        .unwrap_or_else(|| panic!("not a map: {v:?}"))
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .unwrap_or_else(|| panic!("no key {key} in {v:?}"))
        .1
}

fn params(entries: &[(&str, &str)]) -> Value {
    Value::Map(entries.iter().map(|(k, v)| (Value::from(*k), Value::from(*v))).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn layout_methods_drive_a_ui() {
    init_log();
    let server = TestServer::start(TestServerConfig {
        panes: vec![(Uuid::new_v4(), b"hello".to_vec())],
        ..Default::default()
    })
    .await;
    let h = connect(&server).await;
    let mut events = h.events().await.expect("events");

    let layout = h.request("get_layout", Value::Nil).await.expect("get_layout");
    let ws = &find(&layout, "workspaces").as_array().expect("workspaces")[0];
    assert_eq!(find(ws, "name").as_str(), Some("Default"));
    assert_eq!(find(ws, "tabs").as_array().expect("tabs").len(), 1);

    // New tab, split, rename: each emits LayoutChanged.
    let tab = h.request("new_tab", params(&[])).await.expect("new_tab");
    let new_pane = find(&tab, "pane_id").as_str().expect("pane_id").to_string();
    let split = h
        .request("split_pane", params(&[("pane_id", &new_pane), ("direction", "vertical")]))
        .await
        .expect("split_pane");
    let split_pane = find(&split, "pane_id").as_str().expect("pane_id").to_string();
    // The socket API names the workspace by index.
    let rename = Value::Map(vec![
        (Value::from("workspace_idx"), Value::from(0u64)),
        (Value::from("name"), Value::from("work")),
    ]);
    h.request("rename_workspace", rename).await.expect("rename");

    let tabs = h.request("list_tabs", Value::Nil).await.expect("list_tabs");
    assert_eq!(find(&tabs, "tabs").as_array().expect("tabs").len(), 3);

    h.request("close_pane", params(&[("pane_id", &split_pane)])).await.expect("close_pane");
    let layout = h.request("get_layout", Value::Nil).await.expect("get_layout");
    let ws = &find(&layout, "workspaces").as_array().expect("workspaces")[0];
    assert_eq!(find(ws, "name").as_str(), Some("work"));
    assert_eq!(find(ws, "tabs").as_array().expect("tabs").len(), 2);

    // The events arrived in order, at least one LayoutChanged and one PaneClosed.
    let mut layout_changed = 0;
    let mut closed = false;
    while let Ok(Some(EventItem::Server { event, .. })) =
        tokio::time::timeout(Duration::from_millis(300), events.next()).await
    {
        match event {
            ServerEvent::LayoutChanged { .. } => layout_changed += 1,
            ServerEvent::PaneClosed { pane_id } => closed = pane_id.to_string() == split_pane,
            _ => {}
        }
    }
    assert!(layout_changed >= 3, "layout events: {layout_changed}");
    assert!(closed, "no PaneClosed for the split pane");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_pane_emits_pane_resized() {
    init_log();
    let server = TestServer::start(TestServerConfig {
        panes: vec![(Uuid::new_v4(), Vec::new())],
        ..Default::default()
    })
    .await;
    let pane = server.initial_pane(0);
    let h = connect(&server).await;
    let mut events = h.events().await.expect("events");
    h.resize(pane, 40, 100).await.expect("resize");
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(EventItem::Server {
                event: ServerEvent::PaneResized { rows, cols, .. },
                ..
            }) = events.next().await
            {
                return (rows, cols);
            }
        }
    })
    .await
    .expect("PaneResized");
    assert_eq!(got, (40, 100));
    h.close().await;
    server.shutdown().await;
}
