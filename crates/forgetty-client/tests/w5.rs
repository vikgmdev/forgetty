//! W5 scenarios: the AD-025 flags byte on the wire, whole-buffer input, and a link that drops
//! in the middle of a snapshot. They run against the in-process test server.

use std::time::Duration;

use forgetty_client::proto::{cap, ClientKind};
use forgetty_client::test_server::{TestServer, TestServerConfig};
use forgetty_client::{Client, ClientError, ClientHandle, ClientInfo, ConnectionState, PaneItem, SecretKey};
use uuid::Uuid;

async fn connect(server: &TestServer) -> ClientHandle {
    let key = SecretKey::generate(&mut rand::rng());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "w5", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: {what}"));
}

#[tokio::test]
async fn auto_frames_carry_the_flag_when_both_sides_list_it() {
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    assert!(h.has_capability(cap::INPUT_FLAGS));
    h.send_input(pane, b"ls\r").await.unwrap();
    h.send_input_flagged(pane, b"\x1b[?62c", true).await.unwrap();
    h.send_input(pane, b"pwd\r").await.unwrap();
    until("input arrives", || server.received_input(pane).len() == 7 && !server.received_auto_input(pane).is_empty()).await;
    assert_eq!(server.received_input(pane), b"ls\rpwd\r");
    assert_eq!(server.received_auto_input(pane), b"\x1b[?62c");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn without_the_capability_input_has_no_flags_byte() {
    let mut cfg = TestServerConfig::default();
    cfg.capabilities.retain(|c| c != cap::INPUT_FLAGS);
    let server = TestServer::start(cfg).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    assert!(!h.has_capability(cap::INPUT_FLAGS));
    h.send_input(pane, b"ls\r").await.unwrap();
    h.send_input_flagged(pane, b"\x1b[I", true).await.unwrap();
    until("input arrives", || server.received_input(pane).len() == 6).await;
    // An old server cannot tell the kinds apart: both are plain input, byte for byte.
    assert_eq!(server.received_input(pane), b"ls\r\x1b[I");
    assert!(server.received_auto_input(pane).is_empty());
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn a_refused_buffer_is_refused_whole_and_accepted_ones_arrive_whole() {
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;
    // Keep all input unacked, then lose the link: the queue fills while disconnected.
    server.hold_acks(true);
    server.pause_handshakes(true);
    server.drop_connections();
    let mut st = h.watch_state();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(*st.borrow_and_update(), ConnectionState::Reconnecting { .. }) {
            st.changed().await.unwrap();
        }
    })
    .await
    .expect("reconnecting");
    // 3-byte characters: the 16 KiB cut falls inside one unless it is moved.
    let first = "\u{20ac}".repeat(10_000);
    h.send_input(pane, first.as_bytes()).await.expect("fits the empty buffer");
    let second = vec![b'z'; 40_000];
    match h.send_input(pane, &second).await {
        Err(ClientError::InputBufferFull) => {}
        other => panic!("the second buffer must be refused whole: {other:?}"),
    }
    assert_eq!(h.unacked_input_bytes(), first.len(), "nothing of the refused buffer was queued");
    server.hold_acks(false);
    server.pause_handshakes(false);
    until("first buffer arrives", || server.received_input(pane).len() >= first.len()).await;
    assert_eq!(server.received_input(pane), first.as_bytes(), "whole, in order, nothing of the second");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn a_link_that_drops_mid_snapshot_resumes_with_a_full_reset() {
    let mut cfg = TestServerConfig::default();
    cfg.capabilities.push(cap::SNAPSHOT_CATCHUP.to_string());
    let server = TestServer::start(cfg).await;
    let pane = server.add_pane();
    server.append(pane, b"hello world\r\n");
    // 100 bytes announced, 40 sent, then the link drops.
    server.queue_partial_snapshot(pane, 100, &[b'S'; 40]);
    let h = connect(&server).await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    assert!(matches!(next_item(&mut stream).await, PaneItem::Reset { .. }));
    assert!(matches!(next_item(&mut stream).await, PaneItem::Snapshot { .. }));
    assert_eq!(stream.offset(), None, "an incomplete snapshot leaves no resume offset");
    server.drop_connections();
    // The re-attach carries no offset, so the server starts over with a reset and the whole log.
    assert!(matches!(next_item(&mut stream).await, PaneItem::Reset { .. }), "a full reset follows");
    match next_item(&mut stream).await {
        PaneItem::Data { offset, bytes } => {
            assert_eq!(offset, 0);
            assert_eq!(bytes, b"hello world\r\n");
        }
        other => panic!("expected the log from byte 0, got {other:?}"),
    }
    let attaches = server.attaches();
    assert_eq!(attaches.len(), 2, "{attaches:?}");
    assert_eq!(attaches[1], (pane, None), "the re-attach must not resume at an offset");
    h.close().await;
    server.shutdown().await;
}

async fn next_item(s: &mut forgetty_client::PaneStream) -> PaneItem {
    tokio::time::timeout(Duration::from_secs(10), s.next()).await.expect("timed out").expect("stream ended")
}
