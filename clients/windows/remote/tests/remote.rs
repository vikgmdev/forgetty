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
    rig_caps(panes, &[]).await
}

/// `extra` adds capabilities (the rig's default server is an older one without the WP2b ones).
async fn rig_caps(panes: u128, extra: &[&str]) -> Rig {
    let server = TestServer::start(TestServerConfig {
        pair_secret: Some(SECRET.into()),
        capabilities: vec![
            forgetty_client::proto::cap::RESUME.to_string(),
            forgetty_client::proto::cap::LAYOUT_EVENTS.to_string(),
            forgetty_client::proto::cap::SIZE_POLICY_LATEST.to_string(),
            forgetty_client::proto::cap::SNAPSHOT_CATCHUP.to_string(),
        ]
        .into_iter()
        .chain(extra.iter().map(|c| c.to_string()))
        .collect(),
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
            PaneEvent::ReplayEnd => {}
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
        .split_pane(SplitPane { pane_id: t.pane_id, direction: SplitDir::Vertical, rows: 12, cols: 80, cwd: None, before: false })
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
        .split_pane(SplitPane { pane_id: PaneId(pane(1)), direction: SplitDir::Horizontal, rows: 24, cols: 40, cwd: None, before: false })
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

async fn ws_id(b: &RemoteBackend, i: usize) -> Uuid {
    b.get_layout().await.unwrap().workspaces[i].id
}

/// WP2: the workspace methods go over the wire with the server's names and shapes, and the
/// answers come back as layout events. Run against a server with and without `workspace_id`.
async fn workspace_ops(r: Rig) {
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let t = b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() }).await.unwrap();
    let n_tabs = b.get_layout().await.unwrap().workspaces[0].tabs.len();
    assert_eq!(n_tabs, 2);
    let w0 = ws_id(b, 0).await;
    let gone = Uuid::new_v4();

    // The last workspace cannot go.
    assert!(b.delete_workspace(w0).await.is_err());

    // A copy: one new tab (and pane) per tab, after the source, named "<source> (copy)".
    let d = b.duplicate_workspace(w0, None, 24, 80).await.unwrap();
    assert_eq!((d.workspace_idx, d.tabs.len()), (1, n_tabs));
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces.len(), 2);
    assert_eq!(l.workspaces[1].name, "Default (copy)");
    assert_eq!(l.workspaces[1].id, d.workspace_id);
    assert_eq!(l.workspaces[1].tabs.iter().map(|t| t.id).collect::<Vec<_>>(), d.tabs.iter().map(|t| t.0).collect::<Vec<_>>());
    assert_eq!(l.workspaces[1].tabs[0].pane_tree.first_leaf(), d.tabs[0].1);
    assert!(l.find_tab(t.tab_id).is_some_and(|(w, _)| w == 0));
    let named = b.duplicate_workspace(d.workspace_id, Some("mine".into()), 24, 80).await.unwrap();
    assert_eq!(b.get_layout().await.unwrap().workspaces[2].name, "mine");
    assert_eq!(named.workspace_idx, 2);
    assert!(b.duplicate_workspace(gone, None, 24, 80).await.is_err());

    // Color: #rrggbb goes out, anything else is refused before it is sent, null clears it.
    b.set_workspace_color(d.workspace_id, Some("#3987E4".into())).await.unwrap();
    assert_eq!(b.get_layout().await.unwrap().workspaces[1].color.as_deref(), Some("#3987e4"));
    assert!(b.set_workspace_color(d.workspace_id, Some("blue".into())).await.is_err());
    assert!(b.set_workspace_color(gone, None).await.is_err());
    b.set_workspace_color(d.workspace_id, None).await.unwrap();
    assert_eq!(b.get_layout().await.unwrap().workspaces[1].color, None);

    // Move swaps two workspaces; the active workspace follows its workspace.
    b.set_active_workspace(2).await.unwrap();
    b.move_workspace(named.workspace_id, w0).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["mine", "Default (copy)", "Default"]);
    assert_eq!(l.active_workspace, 0);
    assert!(b.move_workspace(w0, gone).await.is_err());

    // Delete closes the workspace's panes: the server sends PaneClosed for each.
    let events = b.subscribe();
    let doomed: Vec<_> = l.workspaces[1].tabs.iter().map(|t| t.pane_tree.first_leaf()).collect();
    b.delete_workspace(d.workspace_id).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces.len(), 2);
    assert_eq!(l.workspaces[1].name, "Default");
    assert_eq!(l.active_workspace, 0);
    // The same id again is an error, and the others stay.
    assert!(b.delete_workspace(d.workspace_id).await.is_err());
    assert_eq!(b.get_layout().await.unwrap().workspaces.len(), 2);
    let mut closed = Vec::new();
    until("pane closed events", || {
        while let Ok(e) = events.try_recv() {
            if let forgetty_shell_core::BackendEvent::PaneClosed { pane_id } = e {
                closed.push(pane_id);
            }
        }
        doomed.iter().all(|p| closed.contains(p))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_ops_over_the_wire_with_ids() {
    use forgetty_client::proto::cap;
    workspace_ops(rig_caps(1, &[cap::WORKSPACE_ID, cap::SPLIT_BEFORE]).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_ops_over_the_wire_against_an_older_server() {
    workspace_ops(rig(1).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_workspace_is_still_the_one_acted_on_with_ids() {
    use forgetty_client::proto::cap;
    a_moved_workspace_is_still_the_one_acted_on(rig_caps(1, &[cap::WORKSPACE_ID]).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_workspace_is_still_the_one_acted_on_against_an_older_server() {
    a_moved_workspace_is_still_the_one_acted_on(rig(1).await).await;
}

/// The stale-index case: another client moved a workspace while this request was on its way.
/// The request names the id, so it still acts on the workspace the user chose.
async fn a_moved_workspace_is_still_the_one_acted_on(r: Rig) {
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let w0 = ws_id(b, 0).await;
    let a = b.duplicate_workspace(w0, Some("a".into()), 24, 80).await.unwrap();
    let c = b.duplicate_workspace(a.workspace_id, Some("c".into()), 24, 80).await.unwrap();
    // Order: Default, a, c. The user picked "c" (position 2) ...
    // ... and meanwhile another client swaps Default and c.
    b.move_workspace(w0, c.workspace_id).await.unwrap();
    // Now position 2 is "Default". Deleting "c" by id removes "c" and keeps "Default".
    b.delete_workspace(c.workspace_id).await.unwrap();
    let names: Vec<_> = b.get_layout().await.unwrap().workspaces.into_iter().map(|w| w.name).collect();
    assert_eq!(names, ["a", "Default"]);
}

/// "Duplicate tab" over the wire: the new tab starts where the source pane runs.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_tab_sends_the_spawn_directory_never_the_display_one() {
    let r = rig(1).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    let l = b.get_layout().await.unwrap();
    let src = l.workspaces[0].tabs[0].pane_tree.first_leaf();
    // The test server shows "/shown/by/osc7" (what a program printed) and started the shell
    // in "/real/start".
    let info = b.get_pane_info(src).await.unwrap();
    assert_eq!(info.cwd, "/shown/by/osc7");
    assert_eq!(info.spawn_cwd.as_deref(), Some("/real/start"));
    b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, inherit_cwd_from: Some(src), ..Default::default() }).await.unwrap();
    assert_eq!(r.server.new_tab_cwds().last().cloned().flatten().as_deref(), Some("/real/start"));
    assert_eq!(b.get_layout().await.unwrap().tab_count(), 2);
    // A server from before `spawn_cwd`: no cwd goes out (the display value is not a fallback).
    r.server.set_hide_spawn_cwd(true);
    b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, inherit_cwd_from: Some(src), ..Default::default() }).await.unwrap();
    assert_eq!(r.server.new_tab_cwds().last().cloned(), Some(None));
    // An unknown source pane: the tab still opens, with no cwd.
    let t2 = b
        .new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, inherit_cwd_from: Some(PaneId(Uuid::new_v4())), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(r.server.new_tab_cwds().last().cloned(), Some(None));
    // An explicit cwd wins.
    b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, cwd: Some("/x".into()), inherit_cwd_from: Some(src), ..Default::default() }).await.unwrap();
    assert_eq!(r.server.new_tab_cwds().last().cloned().flatten().as_deref(), Some("/x"));
    let _ = t2;
}

/// Split left/up goes out only to a server that reports `split_before`. An older one would
/// split on the wrong side without saying so, so the backend refuses before sending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_before_needs_the_capability() {
    use forgetty_client::proto::cap;
    let req = |p: PaneId, before| SplitPane { pane_id: p, direction: SplitDir::Horizontal, rows: 24, cols: 40, cwd: None, before };

    let r = rig(1).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    assert!(!b.supports_split_before());
    assert!(b.split_pane(req(PaneId(pane(1)), true)).await.is_err());
    // Nothing was split by the refused call.
    assert_eq!(b.get_layout().await.unwrap().workspaces[0].tabs[0].pane_tree.leaves().len(), 1);
    // Right and down still work.
    b.split_pane(req(PaneId(pane(1)), false)).await.unwrap();
    drop(r);

    let r = rig_caps(1, &[cap::SPLIT_BEFORE]).await;
    let b = &r.backend;
    until("connected", || b.connection().is_connected()).await;
    until("capability", || b.supports_split_before()).await;
    let left = b.split_pane(req(PaneId(pane(1)), true)).await.unwrap();
    let l = b.get_layout().await.unwrap();
    assert_eq!(l.workspaces[0].tabs[0].pane_tree.leaves(), vec![left, PaneId(pane(1))]);
}

/// What a machine's server can do is remembered while the link is down. A request held over
/// the outage goes out on the new session, and the capability is checked on that one: if the
/// server that came back is older, a split left/up is refused and nothing is split.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_are_remembered_while_the_link_is_down() {
    use forgetty_client::proto::cap;
    let key = forgetty_client::SecretKey::generate(&mut rand::rng());
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let cfg = |k: &forgetty_client::SecretKey, caps: &[&str]| TestServerConfig {
        pair_secret: Some(SECRET.into()),
        port: Some(port),
        secret_key: Some(k.clone()),
        panes: vec![(pane(1), b"$ ".to_vec())],
        capabilities: [cap::RESUME, cap::LAYOUT_EVENTS].iter().chain(caps.iter()).map(|c| c.to_string()).collect(),
        ..Default::default()
    };
    let server = TestServer::start(cfg(&key, &[cap::SPLIT_BEFORE, cap::WORKSPACE_ID])).await;
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
    // Never reached yet: nothing is known.
    assert!(!backend.supports_split_before());
    until("connected", || backend.connection().is_connected()).await;
    assert!(backend.supports_split_before());

    // The link goes down: the answer stays.
    server.shutdown().await;
    until("link loss seen", || !backend.connection().is_connected()).await;
    assert!(backend.supports_split_before());

    // A split left held over the outage ...
    let held = {
        let b = backend.clone();
        tokio::spawn(async move {
            b.split_pane(SplitPane { pane_id: PaneId(pane(1)), direction: SplitDir::Horizontal, rows: 24, cols: 40, cwd: None, before: true })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    // ... meets an older server: refused, nothing split, and the answer follows the new session.
    let old = TestServer::start(cfg(&key, &[])).await;
    let r = held.await.unwrap();
    assert!(r.is_err_and(|e| e.to_string().contains("capability_missing")));
    until("connected again", || backend.connection().is_connected()).await;
    until("capability gone", || !backend.supports_split_before()).await;
    assert_eq!(backend.get_layout().await.unwrap().workspaces[0].tabs[0].pane_tree.leaves().len(), 1);
    old.shutdown().await;
}

/// A workspace call held over an outage meets a server that lacks `workspace_id`: it takes the
/// position path (the id is looked up in the new layout), not an id the server cannot read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_call_after_a_reconnect_to_an_older_server_uses_positions() {
    use forgetty_client::proto::cap;
    let key = forgetty_client::SecretKey::generate(&mut rand::rng());
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let cfg = |caps: &[&str]| TestServerConfig {
        pair_secret: Some(SECRET.into()),
        port: Some(port),
        secret_key: Some(key.clone()),
        panes: vec![(pane(1), b"$ ".to_vec())],
        capabilities: [cap::RESUME, cap::LAYOUT_EVENTS].iter().chain(caps.iter()).map(|c| c.to_string()).collect(),
        ..Default::default()
    };
    let server = TestServer::start(cfg(&[cap::WORKSPACE_ID])).await;
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
    let ws = backend.get_layout().await.unwrap().workspaces[0].id;
    server.shutdown().await;
    until("link loss seen", || !backend.connection().is_connected()).await;
    // The call is made while the link is down, on the answer of the old session ...
    let held = {
        let b = backend.clone();
        tokio::spawn(async move { b.set_workspace_color(ws, Some("#112233".into())).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    // ... and the server that comes back has no `workspace_id`: an id in the request would be
    // refused as a missing position.
    let old = TestServer::start(cfg(&[])).await;
    held.await.unwrap().expect("the call took the position path");
    until("connected again", || backend.connection().is_connected()).await;
    let l = backend.get_layout().await.unwrap();
    assert_eq!(l.workspaces[0].color.as_deref(), Some("#112233"));
    old.shutdown().await;
}
