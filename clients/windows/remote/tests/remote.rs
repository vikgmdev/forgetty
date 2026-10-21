//! RemoteBackend against the in-process protocol v2 test server.

use std::time::Duration;

use forgetty_client::proto::ClientKind;
use forgetty_client::test_server::{TestServer, TestServerConfig};
use forgetty_client::{ClientInfo, ClientOptions, RelayChoice};
use forgetty_remote::store::{self, MachineStore};
use forgetty_remote::{RemoteBackend, RemoteParams};
use forgetty_shell_core::{
    BackendEvent, ConnState, LabelTarget, NewTab, PaneEvent, PaneId, PaneStatus, PaneTree, SessionBackend, SplitDir, SplitPane,
    SplitRatio,
};
use uuid::Uuid;

const SECRET: &str = "TESTSECRET0000000000000000";

fn pane(n: u128) -> Uuid {
    Uuid::from_u128(0x7e57_0000_0000_4000_8000_0000_0000_0000 | n)
}

async fn until<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timeout: {what}");
}

struct Rig {
    server: TestServer,
    backend: RemoteBackend,
    _dir: tempfile::TempDir,
}

async fn rig(panes: u128) -> Rig {
    let server = TestServer::start(TestServerConfig {
        pair_secret: Some(SECRET.into()),
        capabilities: vec![
            forgetty_client::proto::cap::RESUME.to_string(),
            forgetty_client::proto::cap::LAYOUT_EVENTS.to_string(),
            forgetty_client::proto::cap::SIZE_POLICY_LATEST.to_string(),
            forgetty_client::proto::cap::SNAPSHOT_CATCHUP.to_string(),
        ],
        panes: (1..=panes).map(|n| (pane(n), format!("hello {n}\r\n$ ").into_bytes())).collect(),
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let st = MachineStore::open(dir.path().join("forgetty"));
    let node = server.addr().id.to_string();
    let hint = server.udp_addr();
    let text = format!(r#"printed by forgetty pair: {{"v":2,"node_id":"{node}","machine":"test-host","relay":"","secret":"{SECRET}"}}"#);
    let offer = store::parse_offer(&text).unwrap();
    let m = store::pair(&st, &offer, "win-test", Some(hint)).await.unwrap();
    assert_eq!(m.name, "test-host");
    let backend = RemoteBackend::start(RemoteParams {
        machine_id: m.id,
        name: m.name.clone(),
        server: store::machine_addr(&m).unwrap(),
        identity: st.identity().unwrap(),
        info: ClientInfo::new(st.client_id().unwrap(), ClientKind::Windows, "win-test", "0.0.1"),
        opts: ClientOptions { relay: RelayChoice::Disabled, addr_cache: st.addr_cache(), ..Default::default() },
    })
    .await
    .unwrap();
    Rig { server, backend, _dir: dir }
}

async fn read_until(rx: &async_channel::Receiver<PaneEvent>, needle: &str) -> (String, usize) {
    let mut text = String::new();
    let mut resets = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !text.contains(needle) {
        let ev = tokio::time::timeout_at(deadline, rx.recv()).await.unwrap_or_else(|_| panic!("timeout for {needle:?}, got {text:?}")).unwrap();
        match ev {
            PaneEvent::Reset { .. } => {
                resets += 1;
                text.clear();
            }
            PaneEvent::Output(b) => text.push_str(&String::from_utf8_lossy(&b)),
            PaneEvent::Closed(r) => panic!("closed: {r}"),
        }
    }
    (text, resets)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layout_attach_input_resize() {
    let r = rig(2).await;
    let b = &r.backend;
    until("connected", || b.connection() == ConnState::Connected).await;
    assert!(b.machine().connected);

    let layout = b.get_layout().await.unwrap();
    assert_eq!(layout.tab_count(), 2);
    assert_eq!(layout.all_panes(), vec![PaneId(pane(1)), PaneId(pane(2))]);

    let src = b.attach_pane(PaneId(pane(1))).await.unwrap();
    let (text, resets) = read_until(&src.events(), "$ ").await;
    assert_eq!(resets, 1, "first attach asks for a VT reset");
    assert!(text.contains("hello 1"));

    src.write_input(b"echo-me");
    read_until(&src.events(), "echo-me").await;
    assert_eq!(r.server.received_input(pane(1)), b"echo-me");

    src.resize(40, 120);
    until("server size", || r.server.pane_size(pane(1)) == (40, 120)).await;
    // Same size again: no new request. A forced one still goes out.
    src.resize(40, 120);
    src.reassert_size(41, 121);
    until("reasserted size", || r.server.pane_size(pane(1)) == (41, 121)).await;

    let info = b.get_pane_info(PaneId(pane(1))).await.unwrap();
    assert_eq!((info.rows, info.cols), (41, 121));
    assert!(matches!(b.get_pane_info(PaneId(Uuid::new_v4())).await, Err(forgetty_shell_core::BackendError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tabs_splits_and_events() {
    let r = rig(1).await;
    let b = &r.backend;
    let events = b.subscribe();
    until("connected", || b.connection().is_connected()).await;

    let t = b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() }).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.tab_count(), 2);
    assert!(l.find_tab(t.tab_id).is_some());

    let p2 = b
        .split_pane(SplitPane { pane_id: t.pane_id, direction: SplitDir::Vertical, rows: 12, cols: 80, cwd: None })
        .await
        .unwrap();
    let l = b.get_layout().await.unwrap();
    let tab = &l.workspaces[0].tabs[l.find_tab(t.tab_id).unwrap().1];
    assert_eq!(tab.pane_tree.leaves(), vec![t.pane_id, p2]);

    b.focus_tab(t.tab_id).await.unwrap();
    b.rename_workspace(0, "work".into()).await.unwrap();
    // The in-process server answers only part of the method set.
    assert!(b.move_tab(t.tab_id, 0).await.is_err());
    // set_active_pane resolves the workspace from the layout it saw; a tab it never saw is not found.
    assert!(matches!(
        b.set_active_pane(Uuid::new_v4(), None).await,
        Err(forgetty_shell_core::BackendError::NotFound(_))
    ));

    b.close_pane(p2).await.unwrap();
    r.server.push_status(pane(1), "waiting", "needs input");
    r.server.push_event(forgetty_client::proto::ServerEvent::Notification {
        pane_id: pane(1),
        title: "build".into(),
        body: "finished".into(),
    });

    let mut saw_layout = false;
    let mut saw_status = false;
    let mut saw_note = false;
    let mut saw_conn = false;
    let mut saw_closed = false;
    until("events", || {
        while let Ok(e) = events.try_recv() {
            match e {
                BackendEvent::LayoutChanged(l) => saw_layout |= l.tab_count() >= 1,
                BackendEvent::StatusChanged { status: PaneStatus::Waiting, message, .. } => saw_status = message == "needs input",
                BackendEvent::Notification { title, body, .. } => saw_note = title == "build" && body == "finished",
                BackendEvent::ConnectionChanged(ConnState::Connected) => saw_conn = true,
                BackendEvent::PaneClosed { pane_id } => saw_closed |= pane_id == p2,
                _ => {}
            }
        }
        saw_layout && saw_status && saw_note && saw_conn && saw_closed
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_survives_a_dropped_connection() {
    let r = rig(1).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let src = b.attach_pane(PaneId(pane(1))).await.unwrap();
    read_until(&src.events(), "$ ").await;

    r.server.drop_connections();
    // Typed while the link is down or coming back: buffered, sent in order.
    src.write_input(b"AAA");
    src.write_input(b"BBB");
    read_until(&src.events(), "AAABBB").await;
    assert_eq!(r.server.received_input(pane(1)), b"AAABBB");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frozen_server_shows_reconnecting() {
    let r = rig(1).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    r.server.drop_connections();
    r.server.pause_handshakes(true);
    until("reconnecting", || matches!(b.connection(), ConnState::Reconnecting { .. })).await;
    let ConnState::Reconnecting { since } = b.connection() else { unreachable!() };
    assert!(!b.connection().show_overlay(since), "never before the delay");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(b.connection().show_overlay(std::time::Instant::now()));
    r.server.pause_handshakes(false);
    until("back", || b.connection().is_connected()).await;
}

fn root_ratio(t: &PaneTree) -> f32 {
    match t {
        PaneTree::Split { ratio, .. } => *ratio,
        PaneTree::Leaf { .. } => panic!("not a split"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focus_active_pane_ratios_and_labels_reach_the_server() {
    let r = rig(2).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let l = b.get_layout().await.unwrap();
    let (t0, t1) = (l.workspaces[0].tabs[0].id, l.workspaces[0].tabs[1].id);
    assert_eq!(l.workspaces[0].active_tab, 0);

    // focus_tab moves the active tab on the server.
    b.focus_tab(t1).await.unwrap();
    assert_eq!(b.get_layout().await.unwrap().workspaces[0].active_tab, 1);

    // set_active_pane: split tab 0, focus the new pane.
    let p3 = b
        .split_pane(SplitPane { pane_id: PaneId(pane(1)), direction: SplitDir::Horizontal, rows: 24, cols: 40, cwd: None })
        .await
        .unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces[0].tabs[0].active_pane_id, Some(PaneId(pane(1))));
    b.set_active_pane(t0, Some(p3)).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces[0].tabs[0].active_pane_id, Some(p3));
    assert!(b.set_active_pane(t0, Some(PaneId(pane(2)))).await.is_err(), "a pane of another tab");

    // update_split_ratios: keyed by the first pane of the split; clamped to 0.05..0.95.
    assert_eq!(root_ratio(&l.workspaces[0].tabs[0].pane_tree), 0.5);
    b.update_split_ratios(vec![SplitRatio { pane_id: PaneId(pane(1)), ratio: 0.3 }]).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert!((root_ratio(&l.workspaces[0].tabs[0].pane_tree) - 0.3).abs() < 1e-6);
    b.update_split_ratios(vec![SplitRatio { pane_id: PaneId(pane(1)), ratio: 4.0 }]).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert!((root_ratio(&l.workspaces[0].tabs[0].pane_tree) - 0.95).abs() < 1e-6);

    // set_label for the three targets.
    let ws = l.workspaces[0].id;
    b.set_label(LabelTarget::Tab, t1, "build".into()).await.unwrap();
    b.set_label(LabelTarget::Pane, p3.0, "logs".into()).await.unwrap();
    b.set_label(LabelTarget::Workspace, ws, "main".into()).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces[0].name, "main");
    assert_eq!(l.workspaces[0].tabs[1].label, "build");
    assert_eq!(l.workspaces[0].tabs[0].pane_label(p3), Some("logs"));
    // Bad input is refused before the wire.
    assert!(b.set_label(LabelTarget::Workspace, ws, " ".into()).await.is_err());
    assert!(b.set_label(LabelTarget::Tab, t1, "x".repeat(129)).await.is_err());
    // Empty clears.
    b.set_label(LabelTarget::Tab, t1, "".into()).await.unwrap();
    assert_eq!(b.get_layout().await.unwrap().workspaces[0].tabs[1].label, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_resets_the_vt_and_shows_the_marker() {
    let r = rig(1).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let src = b.attach_pane(PaneId(pane(1))).await.unwrap();
    read_until(&src.events(), "$ ").await;

    r.server.queue_snapshot(pane(1), b"\x1b[2J\x1b[HSNAPSHOT-SCREEN");
    r.server.drop_connections();
    let (text, resets) = read_until(&src.events(), "SNAPSHOT-SCREEN").await;
    assert_eq!(resets, 1, "the snapshot follows a reset");
    assert!(text.contains(forgetty_remote::SNAPSHOT_MARKER), "{text:?}");
    assert!(text.find(forgetty_remote::SNAPSHOT_MARKER) < text.find("SNAPSHOT-SCREEN"));
    // Live bytes after the snapshot still arrive, at the right place.
    src.write_input(b"after");
    read_until(&src.events(), "after").await;
}

/// Time from "the new server accepts" to `Connected`, after the old server
/// was gone for different times (the backoff and the dial attempts are at
/// different points then). `store::dial_timeout_for` is what the app uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_after_a_restart_is_quick() {
    let key = forgetty_client::SecretKey::generate(&mut rand::rng());
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let cfg = |k: &forgetty_client::SecretKey| TestServerConfig {
        pair_secret: Some(SECRET.into()),
        port: Some(port),
        secret_key: Some(k.clone()),
        panes: vec![(pane(1), b"$ ".to_vec())],
        ..Default::default()
    };
    let mut server = TestServer::start(cfg(&key)).await;
    let dir = tempfile::tempdir().unwrap();
    let st = MachineStore::open(dir.path().join("forgetty"));
    let node = server.addr().id.to_string();
    let text = format!(r#"{{"v":2,"node_id":"{node}","machine":"m","relay":"","secret":"{SECRET}"}}"#);
    let m = store::pair(&st, &store::parse_offer(&text).unwrap(), "win-test", Some(server.udp_addr())).await.unwrap();
    let cache = st.addr_cache();
    let backend = RemoteBackend::start(RemoteParams {
        machine_id: m.id,
        name: m.name.clone(),
        server: store::machine_addr(&m).unwrap(),
        identity: st.identity().unwrap(),
        info: ClientInfo::new(st.client_id().unwrap(), ClientKind::Windows, "win-test", "0.0.1"),
        opts: ClientOptions {
            relay: RelayChoice::Disabled,
            dial_timeout: store::dial_timeout_for(&cache, &m),
            addr_cache: cache,
            ..Default::default()
        },
    })
    .await
    .unwrap();
    until("connected", || backend.connection().is_connected()).await;

    for gap in [300u64, 1200, 2600, 4000] {
        server.shutdown().await;
        until("link loss seen", || !backend.connection().is_connected()).await;
        tokio::time::sleep(Duration::from_millis(gap)).await;
        server = TestServer::start(cfg(&key)).await;
        let t0 = std::time::Instant::now();
        until("reconnected", || backend.connection().is_connected()).await;
        let d = t0.elapsed();
        eprintln!("gap {gap} ms: connected {d:?} after the new server accepts");
        assert!(d < Duration::from_millis(1500), "gap {gap}: reconnect took {d:?}");
    }
}
