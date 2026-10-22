//! P5 scenarios over protocol v2 against the real server: `api.list`,
//! `api.describe`, `set_label` and its `LayoutChanged`. Every test skips
//! itself unless `FORGETTY_REAL_SERVER` is set (the in-process test server has
//! no method table).

mod common;

use std::time::Duration;

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, ServerEvent};
use forgetty_client::{Client, ClientError, ClientHandle, ClientInfo, EventItem, SecretKey};
use serde_json::{json, Value};
use uuid::Uuid;

async fn connect(server: &TestServer) -> ClientHandle {
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "p5", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

async fn call(h: &ClientHandle, method: &str, params: Value) -> Result<Value, ClientError> {
    h.request_typed::<Value, Value>(method, &params).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_list_describe_and_set_label_over_v2() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server).await;

    // api.list and api.describe are in the v2 allow-list.
    let list = call(&h, "api.list", json!({})).await.expect("api.list over v2");
    let names: Vec<&str> =
        list["methods"].as_array().unwrap().iter().filter_map(|m| m["name"].as_str()).collect();
    for want in ["api.list", "api.describe", "set_label", "list_tabs", "capture"] {
        assert!(names.contains(&want), "api.list lacks {want}");
    }
    let d = call(&h, "api.describe", json!({"method": "set_label"})).await.expect("describe");
    assert_eq!(d["name"], "set_label");
    assert!(d["params_schema"].is_object() && d["result_schema"].is_object());
    assert_eq!(d["remote"], true);

    // Unknown method: unknown_method. A method that is not remote: refused.
    match call(&h, "api.describe", json!({"method": "nope"})).await {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "unknown_method"),
        other => panic!("describe nope: {other:?}"),
    }
    match call(&h, "pair_open", json!({})).await {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "unknown_method"),
        other => panic!("pair_open over v2: {other:?}"),
    }

    // set_label sends LayoutChanged that carries the label.
    let mut events = h.events().await.expect("events");
    let r =
        call(&h, "set_label", json!({"target": "pane", "id": pane.to_string(), "label": "build"}))
            .await
            .expect("set_label over v2");
    assert_eq!(r["changed"], true);
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Some(EventItem::Server {
                    event: ServerEvent::LayoutChanged { layout }, ..
                }) => {
                    let text = format!("{layout:?}");
                    if text.contains("build") && text.contains("pane_labels") {
                        return true;
                    }
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(found, Ok(true), "no LayoutChanged with the label");

    // Bad and unknown targets are refused with the v2 error codes.
    match call(&h, "set_label", json!({"target": "pane", "id": pane.to_string(), "label": "a\nb"}))
        .await
    {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "bad_request"),
        other => panic!("bad label: {other:?}"),
    }
    match call(
        &h,
        "set_label",
        json!({"target": "pane", "id": Uuid::new_v4().to_string(), "label": "x"}),
    )
    .await
    {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "not_found"),
        other => panic!("unknown pane: {other:?}"),
    }

    h.close().await;
    server.shutdown().await;
}

/// The watchdog and the freeze helper signal a pid only while it is our child,
/// so a reused pid (the live server's, say) is never hit.
#[test]
fn signals_reach_only_children_of_this_process() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    assert!(common::is_my_child(pid));
    assert!(!common::is_my_child(1), "init is not our child");
    assert!(!common::is_my_child(std::process::id() as i32), "we are not our own child");
    assert!(!common::is_my_child(i32::MAX), "no such process");
    child.kill().unwrap();
    child.wait().unwrap();
    // Reaped: the pid is free, and a reused one would not be ours either.
    assert!(!common::is_my_child(pid));
}
