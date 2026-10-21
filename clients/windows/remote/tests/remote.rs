//! RemoteBackend against the in-process protocol v2 test server.

use std::time::Duration;

use forgetty_client::proto::ClientKind;
use forgetty_client::test_server::{TestServer, TestServerConfig};
use forgetty_client::{ClientInfo, ClientOptions, RelayChoice};
use forgetty_remote::store::{self, MachineStore};
use forgetty_remote::{RemoteBackend, RemoteParams};
use forgetty_shell_core::{BackendEvent, ConnState, NewTab, PaneEvent, PaneId, PaneStatus, SessionBackend, SplitDir, SplitPane};
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
