//! Public API checks that need no network.

use std::time::{Duration, Instant};

use forgetty_client::proto::QrPayload;
use forgetty_client::{
    load_or_generate_client_id, load_or_generate_identity, qr_to_addr, ClientError,
    ConnectionState, SecretKey,
};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("forgetty-client-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn identity_file_is_32_raw_bytes_and_stable() {
    let path = scratch("identity.key");
    let _ = std::fs::remove_file(&path);
    let a = load_or_generate_identity(&path).unwrap();
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(raw.len(), 32, "same format as forgetty-sync");
    assert_eq!(raw, a.to_bytes());
    let b = load_or_generate_identity(&path).unwrap();
    assert_eq!(a.public(), b.public());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    std::fs::write(&path, b"short").unwrap();
    assert!(load_or_generate_identity(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn client_id_is_persisted() {
    let path = scratch("client_id");
    let _ = std::fs::remove_file(&path);
    let a = load_or_generate_client_id(&path).unwrap();
    assert_eq!(a, load_or_generate_client_id(&path).unwrap());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn qr_payload_becomes_a_server_address() {
    let key = SecretKey::generate(&mut rand::rng());
    let qr = QrPayload {
        v: 2,
        node_id: key.public().to_string(),
        machine: "ovh".into(),
        relay: "https://relay.example.com/".into(),
        relays: Vec::new(),
        relays_disabled: false,
        direct: Vec::new(),
        secret: "ABC".into(),
    };
    let addr = qr_to_addr(&qr).unwrap();
    assert_eq!(addr.id, key.public());
    assert_eq!(addr.relay_urls().count(), 1);

    let mut bad = qr.clone();
    bad.node_id = "not a node id".into();
    assert!(matches!(qr_to_addr(&bad), Err(ClientError::Invalid(_))));
}

#[test]
fn several_relays_in_a_payload_reach_the_address() {
    let key = SecretKey::generate(&mut rand::rng());
    let json = format!(
        r#"{{"v":2,"node_id":"{}","machine":"ovh","relay":"https://a.example.com/","relays":["https://a.example.com/","https://b.example.com/"],"secret":"ABC"}}"#,
        key.public()
    );
    let qr = QrPayload::parse(&json).unwrap();
    assert_eq!(qr.relay_urls(), vec!["https://a.example.com/", "https://b.example.com/"]);
    assert_eq!(qr_to_addr(&qr).unwrap().relay_urls().count(), 2);

    // A payload from a server with one relay has no `relays`; it still works.
    let one = format!(
        r#"{{"v":2,"node_id":"{}","machine":"ovh","relay":"https://a.example.com/","secret":"ABC"}}"#,
        key.public()
    );
    let qr = QrPayload::parse(&one).unwrap();
    assert!(qr.relays.is_empty());
    assert_eq!(qr.relay_urls(), vec!["https://a.example.com/"]);
    // The payload is written without `relays` again, as before.
    assert!(!qr.to_json().contains("relays"));

    let addr = forgetty_client::server_addr(&key.public().to_string(), &qr.relay_urls()).unwrap();
    assert_eq!(addr.relay_urls().count(), 1);
}

#[test]
fn reconnecting_is_shown_only_after_one_second() {
    let since = Instant::now();
    let s = ConnectionState::Reconnecting { since };
    assert!(!s.show_reconnecting(since + Duration::from_millis(999)));
    assert!(s.show_reconnecting(since + Duration::from_millis(1000)));
    assert!(!ConnectionState::Connected.show_reconnecting(since + Duration::from_secs(60)));
}
