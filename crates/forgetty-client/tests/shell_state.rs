//! P7.1 over protocol v2 against the real server: the `shell_state` capability
//! gives a client the prompt marks (OSC 133), the directory (OSC 7) and the tty
//! mode of a pane as `ShellState` events. A client without the capability sees
//! none of it. The test pane runs `cat` on a raw terminal, so what the test
//! types comes back as output, as a shell would write it. Skips itself unless
//! `FORGETTY_REAL_SERVER` is set.

mod common;

use std::time::Duration;

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::{cap, ClientKind, ServerEvent};
use forgetty_client::{Client, ClientHandle, ClientInfo, EventItem, SecretKey};
use uuid::Uuid;

async fn connect(server: &TestServer, with_cap: bool) -> ClientHandle {
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let mut info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "shell-state", "0.0.0");
    // Not in the default list of the client library: a consumer asks for it.
    if with_cap {
        info.capabilities.push(cap::SHELL_STATE.to_string());
    }
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

#[derive(Debug)]
struct Shell {
    rev: u64,
    phase: String,
    exit_code: Option<i32>,
    cwd: Option<String>,
    canonical: Option<bool>,
    echo: Option<bool>,
    mark: Option<String>,
}

fn shell_state(item: Option<EventItem>, pane: Uuid) -> Option<Shell> {
    match item {
        Some(EventItem::Server {
            event:
                ServerEvent::ShellState {
                    pane_id,
                    rev,
                    phase,
                    exit_code,
                    cwd,
                    canonical,
                    echo,
                    mark,
                    ..
                },
            ..
        }) if pane_id == pane => Some(Shell { rev, phase, exit_code, cwd, canonical, echo, mark }),
        _ => None,
    }
}

const MARKS: &[u8] =
    b"\x1b]133;A\x07\x1b]133;B\x07\x1b]7;file://localhost/tmp/shell-state-test\x07\
\x1b]133;C\x07\x1b]133;D;7\x07";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marks_directory_and_tty_mode_reach_a_client_with_the_capability() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server, true).await;
    assert!(h.has_capability(cap::SHELL_STATE));
    let mut events = h.events().await.expect("events");

    // The state as it is, after subscribe_events: no mark yet, the tty is raw
    // with echo off (what the test pane's wrapper sets).
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Some(item) => {
                    if let Some(s) = shell_state(Some(item), pane) {
                        return s;
                    }
                }
                None => panic!("events ended"),
            }
        }
    })
    .await
    .expect("no ShellState after subscribe");
    assert_eq!(first.phase, "unknown", "{first:?}");
    assert_eq!((first.canonical, first.echo), (Some(false), Some(false)), "{first:?}");
    assert!(first.mark.is_none() && first.exit_code.is_none() && first.rev > 0, "{first:?}");

    // The marks come back as output of the pane.
    h.send_input(pane, MARKS).await.expect("send_input");
    let mut got: Vec<Shell> = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = events.next().await {
            if let Some(s) = shell_state(Some(item), pane) {
                let done = s.phase == "done";
                got.push(s);
                if done {
                    break;
                }
            }
        }
    })
    .await;
    let seen: Vec<(&str, Option<&str>)> =
        got.iter().map(|s| (s.phase.as_str(), s.mark.as_deref())).collect();
    // A, B, the directory (no mark), C, D. The tty mode may add states.
    let marks: Vec<&str> = got.iter().filter_map(|s| s.mark.as_deref()).collect();
    assert_eq!(marks, ["A", "B", "C", "D"], "{seen:?}");
    let d = got.iter().find(|s| s.mark.as_deref() == Some("D")).unwrap();
    assert_eq!((d.phase.as_str(), d.exit_code), ("done", Some(7)), "{d:?}");
    assert_eq!(d.cwd.as_deref(), Some("/tmp/shell-state-test"), "{d:?}");
    assert!(got.windows(2).all(|w| w[0].rev < w[1].rev), "revs grow: {seen:?}");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_without_the_capability_gets_no_shell_state() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let old = connect(&server, false).await;
    assert!(!old.has_capability(cap::SHELL_STATE));
    let mut events = old.events().await.expect("events");
    old.send_input(pane, MARKS).await.expect("send_input");
    // The same bytes make a CwdChanged, which needs no capability: the marks
    // were processed, and still no ShellState came.
    let mut cwd_seen = false;
    let mut shell_seen = false;
    let _ =
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(item) = events.next().await {
                match &item {
                    EventItem::Server {
                        event: ServerEvent::CwdChanged { pane_id, .. }, ..
                    } if *pane_id == pane => cwd_seen = true,
                    EventItem::Server { event: ServerEvent::ShellState { .. }, .. } => {
                        shell_seen = true
                    }
                    _ => {}
                }
            }
        })
        .await;
    assert!(cwd_seen, "the OSC 7 in the test input made no CwdChanged");
    assert!(!shell_seen, "ShellState reached a client without the capability");
    old.close().await;
    server.shutdown().await;
}
