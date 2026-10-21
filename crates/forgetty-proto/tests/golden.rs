//! Golden tests: pin the wire bytes of every message type (PROTOCOL_V2).
//! A change here is a protocol change and needs a new AD.

use forgetty_proto::*;
use uuid::Uuid;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn id(n: u8) -> Uuid {
    Uuid::from_bytes([n; 16])
}

/// Encode `value`, compare to the pinned bytes, decode the pinned bytes back.
fn golden<T>(value: T, expected_hex: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = encode_msg(&value).unwrap();
    assert_eq!(hex(&bytes), expected_hex, "wire bytes changed");
    let back: T = decode_msg(&unhex(expected_hex)).unwrap();
    assert_eq!(back, value);
}

#[test]
fn golden_hello() {
    golden::<Hello>(
        Hello::new(id(1), id(9), ClientKind::Android, "pixel", "0.1.0", vec!["resume".into(), "layout_events".into()]),
        "88ab70726f746f5f6d616a6f7202ab70726f746f5f6d696e6f7200a9636c69656e745f6964d92430313031303130312d303130312d303130312d303130312d303130313031303130313031ab696e7075745f65706f6368d92430393039303930392d303930392d303930392d303930392d303930393039303930393039ab636c69656e745f6b696e64a7616e64726f6964ab636c69656e745f6e616d65a5706978656cae636c69656e745f76657273696f6ea5302e312e30ac6361706162696c697469657392a6726573756d65ad6c61796f75745f6576656e7473",
    );
}

#[test]
fn golden_welcome() {
    golden::<HandshakeReply>(
        HandshakeReply::Welcome(Welcome { proto_major: 2, proto_minor: 0, server_id: id(2), server_version: "3.0.0".into(), hostname: "ovh".into(), capabilities: vec!["resume".into()], boot_id: None }),
        "81a757656c636f6d6586ab70726f746f5f6d616a6f7202ab70726f746f5f6d696e6f7200a97365727665725f6964d92430323032303230322d303230322d303230322d303230322d303230323032303230323032ae7365727665725f76657273696f6ea5332e302e30a8686f73746e616d65a36f7668ac6361706162696c697469657391a6726573756d65",
    );
}

#[test]
fn golden_welcome_with_boot_id() {
    let w = Welcome {
        proto_major: 2,
        proto_minor: 0,
        server_id: id(2),
        server_version: "3.0.0".into(),
        hostname: "ovh".into(),
        capabilities: vec!["resume".into()],
        boot_id: Some(id(7)),
    };
    // The new field is the last key. `golden_welcome` (no `boot_id`) is
    // unchanged, so a server that sends none decodes with `None`.
    golden::<HandshakeReply>(HandshakeReply::Welcome(w), "81a757656c636f6d6587ab70726f746f5f6d616a6f7202ab70726f746f5f6d696e6f7200a97365727665725f6964d92430323032303230322d303230322d303230322d303230322d303230323032303230323032ae7365727665725f76657273696f6ea5332e302e30a8686f73746e616d65a36f7668ac6361706162696c697469657391a6726573756d65a7626f6f745f6964d92430373037303730372d303730372d303730372d303730372d303730373037303730373037");
}

#[test]
fn golden_error() {
    golden::<HandshakeReply>(
        HandshakeReply::Error(ErrorMsg { code: "proto_mismatch".into(), message: "need major 2".into() }),
        "81a54572726f7282a4636f6465ae70726f746f5f6d69736d61746368a76d657373616765ac6e656564206d616a6f722032",
    );
}

#[test]
fn golden_request() {
    golden::<ControlMsg>(
        ControlMsg::request(7, "attach_pane", &AttachPaneParams { pane_id: id(3), from_offset: Some(1024) }).unwrap(),
        "81a75265717565737483a2696407a66d6574686f64ab6174746163685f70616e65a6706172616d7382a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033ab66726f6d5f6f6666736574cd0400",
    );
}

#[test]
fn golden_request_none() {
    golden::<ControlMsg>(
        ControlMsg::request(8, "attach_pane", &AttachPaneParams { pane_id: id(3), from_offset: None }).unwrap(),
        "81a75265717565737483a2696408a66d6574686f64ab6174746163685f70616e65a6706172616d7382a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033ab66726f6d5f6f6666736574c0",
    );
}

#[test]
fn golden_request_bin() {
    golden::<ControlMsg>(
        ControlMsg::Request {
            id: 9,
            method: "x".into(),
            params: Value::Map(vec![(Value::from("data"), Value::Binary(vec![1, 2, 3]))]),
        },
        "81a75265717565737483a2696409a66d6574686f64a178a6706172616d7381a464617461c403010203",
    );
}

#[test]
fn golden_response() {
    golden::<ControlMsg>(
        ControlMsg::Response { id: 7, result: to_value(&OkResult { ok: true }).unwrap() },
        "81a8526573706f6e736582a2696407a6726573756c7481a26f6bc3",
    );
}

#[test]
fn golden_failure() {
    golden::<ControlMsg>(
        ControlMsg::Failure { id: 7, code: "not_found".into(), message: "no pane".into() },
        "81a74661696c75726583a2696407a4636f6465a96e6f745f666f756e64a76d657373616765a76e6f2070616e65",
    );
}

#[test]
fn golden_ev_layout() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 1, event: ServerEvent::LayoutChanged { layout: Value::Map(vec![(Value::from("tabs"), Value::Array(vec![]))]) } },
        "81a54576656e7482a373657101a56576656e7481ad4c61796f75744368616e67656481a66c61796f757481a47461627390",
    );
}

#[test]
fn golden_ev_closed() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 2, event: ServerEvent::PaneClosed { pane_id: id(3) } },
        "81a54576656e7482a373657102a56576656e7481aa50616e65436c6f73656481a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033",
    );
}

#[test]
fn golden_ev_title() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 3, event: ServerEvent::TitleChanged { pane_id: id(3), title: "vim".into() } },
        "81a54576656e7482a373657103a56576656e7481ac5469746c654368616e67656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a57469746c65a376696d",
    );
}

#[test]
fn golden_ev_cwd() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 4, event: ServerEvent::CwdChanged { pane_id: id(3), cwd: "/tmp".into() } },
        "81a54576656e7482a373657104a56576656e7481aa4377644368616e67656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a3637764a42f746d70",
    );
}

#[test]
fn golden_ev_resized() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 5, event: ServerEvent::PaneResized { pane_id: id(3), rows: 24, cols: 80, size_owner: None } },
        "81a54576656e7482a373657105a56576656e7481ab50616e65526573697a656483a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a4726f777318a4636f6c7350",
    );
}

#[test]
fn golden_ev_status() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 6, event: ServerEvent::StatusChanged { pane_id: id(3), status: "waiting".into(), message: "input needed".into() } },
        "81a54576656e7482a373657106a56576656e7481ad5374617475734368616e67656483a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a6737461747573a777616974696e67a76d657373616765ac696e707574206e6565646564",
    );
}

#[test]
fn golden_ev_cconn() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 7, event: ServerEvent::ClientConnected { client_id: id(1), client_kind: ClientKind::Windows, client_name: "desk".into(), connected_at: "2026-10-21T08:00:00Z".into() } },
        "81a54576656e7482a373657107a56576656e7481af436c69656e74436f6e6e656374656484a9636c69656e745f6964d92430313031303130312d303130312d303130312d303130312d303130313031303130313031ab636c69656e745f6b696e64a777696e646f7773ab636c69656e745f6e616d65a46465736bac636f6e6e65637465645f6174b4323032362d31302d32315430383a30303a30305a",
    );
}

#[test]
fn golden_ev_notification() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 9, event: ServerEvent::Notification { pane_id: id(3), title: "build".into(), body: "done".into() } },
        "81a54576656e7482a373657109a56576656e7481ac4e6f74696669636174696f6e83a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a57469746c65a56275696c64a4626f6479a4646f6e65",
    );
}

#[test]
fn golden_ev_pane_exited() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 10, event: ServerEvent::PaneExited { pane_id: id(3), exit_code: Some(7) } },
        "81a54576656e7482a37365710aa56576656e7481aa50616e6545786974656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a9657869745f636f646507",
    );
}

#[test]
fn golden_ev_cdisc() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 8, event: ServerEvent::ClientDisconnected { client_id: id(1) } },
        "81a54576656e7482a373657108a56576656e7481b2436c69656e74446973636f6e6e656374656481a9636c69656e745f6964d92430313031303130312d303130312d303130312d303130312d303130313031303130313031",
    );
}

#[test]
fn golden_header_with_replay_end() {
    let h = PaneStreamHeader {
        pane_id: id(3),
        start_offset: 65536,
        reset: false,
        rows: 24,
        cols: 80,
        snapshot_len: 0,
        replay_end: Some(70000),
    };
    // The new field is the last key. The header without it is unchanged
    // (`golden_header`), and a decoder that does not know the field skips it.
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    golden::<PaneStreamHeader>(h.clone(), "86a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033ac73746172745f6f6666736574ce00010000a57265736574c2a4726f777318a4636f6c7350aa7265706c61795f656e64ce00011170");
    // Old header (no field) decodes with `None`.
    let old = unhex("85a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033ac73746172745f6f6666736574ce00010000a57265736574c2a4726f777318a4636f6c7350");
    let d = rmp_serde::from_slice::<PaneStreamHeader>(&old).unwrap();
    assert_eq!(d.replay_end, None);
    // A decoder from before the field reads the new header: it is a plain
    // map with one more key. Mimic it with a struct that lacks the field.
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Old {
        start_offset: u64,
        reset: bool,
        rows: u16,
        cols: u16,
    }
    let o = rmp_serde::from_slice::<Old>(&bytes).unwrap();
    assert_eq!((o.start_offset, o.reset, o.rows, o.cols), (65536, false, 24, 80));
}

#[test]
fn golden_ev_input_dropped() {
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 11, event: ServerEvent::InputDropped { pane_id: id(3), bytes: 4096 } },
        "81a54576656e7482a37365710ba56576656e7481ac496e70757444726f7070656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a56279746573cd1000",
    );
}

#[test]
fn golden_header() {
    golden::<PaneStreamHeader>(
        PaneStreamHeader { pane_id: id(3), start_offset: 65536, reset: false, rows: 24, cols: 80, snapshot_len: 0, replay_end: None },
        "85a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033ac73746172745f6f6666736574ce00010000a57265736574c2a4726f777318a4636f6c7350",
    );
}

#[test]
fn golden_pair_req() {
    golden::<PairRequest>(
        PairRequest { secret: "AAAAAAAAAAAAAAAAAAAAAAAAAA".into(), device_name: "pixel".into(), client_kind: ClientKind::Android },
        "83a6736563726574ba4141414141414141414141414141414141414141414141414141ab6465766963655f6e616d65a5706978656cab636c69656e745f6b696e64a7616e64726f6964",
    );
}

#[test]
fn golden_pair_ok() {
    golden::<PairResult>(PairResult { ok: true, reason: None }, "82a26f6bc3a6726561736f6ec0");
}

#[test]
fn golden_pair_bad() {
    golden::<PairResult>(
        PairResult { ok: false, reason: Some("bad_secret".into()) },
        "82a26f6bc2a6726561736f6eaa6261645f736563726574",
    );
}

#[test]
fn golden_cliententry() {
    golden::<ClientEntry>(
        ClientEntry { client_id: id(1), client_kind: ClientKind::Cli, client_name: "c".into(), connected_at: "2026-10-21T10:00:00Z".into() },
        "84a9636c69656e745f6964d92430313031303130312d303130312d303130312d303130312d303130313031303130313031ab636c69656e745f6b696e64a3636c69ab636c69656e745f6e616d65a163ac636f6e6e65637465645f6174b4323032362d31302d32315431303a30303a30305a",
    );
}

#[test]
fn golden_input_frame() {
    let payload = encode_input_frame(id(3), 258, b"ls\r");
    assert_eq!(hex(&payload), "0303030303030303030303030303030300000000000001026c730d");
    let (pane, seq, bytes) = decode_input_frame(&payload).unwrap();
    assert_eq!(pane, id(3));
    assert_eq!(seq, 258);
    assert_eq!(bytes, b"ls\r");
}

#[test]
fn input_frame_with_flags_round_trips() {
    // user: bit 0 clear. The flags byte sits after seq, before the bytes.
    let user = encode_input_frame_flags(id(3), 258, 0, b"ls\r");
    assert_eq!(hex(&user), "030303030303030303030303030303030000000000000102006c730d");
    let (pane, seq, flags, bytes) = decode_input_frame_flags(&user).unwrap();
    assert_eq!((pane, seq, flags, bytes), (id(3), 258, 0, &b"ls\r"[..]));
    // auto: bit 0 set.
    let auto = encode_input_frame_flags(id(3), 259, INPUT_FLAG_AUTO, b"\x1b[?62c");
    let (pane, seq, flags, bytes) = decode_input_frame_flags(&auto).unwrap();
    assert_eq!((pane, seq), (id(3), 259));
    assert_ne!(flags & INPUT_FLAG_AUTO, 0);
    assert_eq!(bytes, b"\x1b[?62c");
    // Unknown bits come back untouched; empty bytes are fine.
    let odd = encode_input_frame_flags(id(1), 1, 0xfe, b"");
    let (_, _, flags, bytes) = decode_input_frame_flags(&odd).unwrap();
    assert_eq!((flags, bytes.len()), (0xfe, 0));
    // A frame without the flags byte is too short for the flags decoder.
    let plain = encode_input_frame(id(3), 1, b"");
    assert!(decode_input_frame_flags(&plain).is_err());
    // The plain decoder reads the flags byte as input, so peers must agree on
    // the capability.
    assert_eq!(decode_input_frame(&user).unwrap().2[0], 0);
}

#[tokio::test]
async fn input_frame_with_flags_round_trips_on_a_stream() {
    let mut buf = Vec::new();
    write_input_frame_flags(&mut buf, id(5), 9, INPUT_FLAG_AUTO, b"x").await.unwrap();
    write_input_frame_flags(&mut buf, id(5), 10, 0, b"yz").await.unwrap();
    let mut r = &buf[..];
    assert_eq!(
        read_input_frame_flags(&mut r).await.unwrap(),
        Some((id(5), 9, INPUT_FLAG_AUTO, b"x".to_vec()))
    );
    assert_eq!(read_input_frame_flags(&mut r).await.unwrap(), Some((id(5), 10, 0, b"yz".to_vec())));
    assert_eq!(read_input_frame_flags(&mut r).await.unwrap(), None);
}

#[test]
fn golden_qr_payload() {
    let q = QrPayload {
        v: 2,
        node_id: "abc".into(),
        machine: "ovh".into(),
        relay: "https://r/".into(),
        relays: Vec::new(),
        secret: "S".into(),
    };
    assert_eq!(
        q.to_json(),
        r#"{"v":2,"node_id":"abc","machine":"ovh","relay":"https://r/","secret":"S"}"#
    );
    assert_eq!(QrPayload::parse(&q.to_json()).unwrap(), q);
    assert!(matches!(
        QrPayload::parse(r#"{"v":1,"node_id":"a","machine":"m","relay":"r","secret":"s"}"#),
        Err(QrError::Version(1))
    ));
}

#[test]
fn golden_frame_layout() {
    // [u32 BE length][payload]
    let mut out = Vec::new();
    futures_block(write_frame(&mut out, b"abc")).unwrap();
    assert_eq!(hex(&out), "00000003616263");
}

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
}

#[test]
fn constants_match_contract() {
    assert_eq!(ALPN_RPC, b"forgetty/rpc/2");
    assert_eq!(ALPN_PAIR, b"forgetty/pair/2");
    assert_eq!(MAX_FRAME_LEN, 4 * 1024 * 1024);
    assert_eq!(close_code::NOT_AUTHORIZED, 0x01);
    assert_eq!(close_code::FRAME_TOO_LARGE, 0x02);
    assert_eq!(error_code::PROTO_MISMATCH, "proto_mismatch");
    assert_eq!(
        [
            cap::RESUME,
            cap::LAYOUT_EVENTS,
            cap::STATUS_EVENTS,
            cap::CAPTURE,
            cap::SIZE_POLICY_LATEST
        ],
        ["resume", "layout_events", "status_events", "capture", "size_policy_latest"]
    );
    assert_eq!(RECONNECT_BACKOFF_MS, [100, 250, 500, 1000, 2000]);
    assert_eq!(INPUT_BUFFER_CAP, 65536);
}

#[test]
fn uuids_are_strings_not_bin() {
    // 0xd9 0x24 = str8 of 36 bytes. A bin would be 0xc4 0x10.
    let h = PaneStreamHeader {
        pane_id: id(3),
        start_offset: 0,
        reset: true,
        rows: 1,
        cols: 1,
        snapshot_len: 0,
        replay_end: None,
    };
    let bytes = encode_msg(&h).unwrap();
    assert!(bytes.windows(2).any(|w| w == [0xd9, 0x24]));
}

#[test]
fn params_are_maps() {
    let m = to_value(&AttachPaneParams { pane_id: id(3), from_offset: None }).unwrap();
    assert!(matches!(m, Value::Map(_)));
}

#[test]
fn golden_ping() {
    golden::<ControlMsg>(ControlMsg::Ping { nonce: 5 }, "81a450696e6781a56e6f6e636505");
}

#[test]
fn golden_pong() {
    golden::<ControlMsg>(
        ControlMsg::Pong { nonce: 5, input_acked: 1000 },
        "81a4506f6e6782a56e6f6e636505ab696e7075745f61636b6564cd03e8",
    );
}

#[test]
fn snapshot_header_roundtrips_and_a_plain_one_has_no_snapshot_field() {
    let plain = PaneStreamHeader {
        pane_id: id(3),
        start_offset: 7,
        reset: false,
        rows: 24,
        cols: 80,
        snapshot_len: 0,
        replay_end: None,
    };
    let snap = PaneStreamHeader { reset: true, snapshot_len: 5000, ..plain.clone() };
    let a = rmp_serde::to_vec_named(&plain).unwrap();
    let b = rmp_serde::to_vec_named(&snap).unwrap();
    assert!(b.len() > a.len());
    assert_eq!(rmp_serde::from_slice::<PaneStreamHeader>(&a).unwrap(), plain);
    assert_eq!(rmp_serde::from_slice::<PaneStreamHeader>(&b).unwrap(), snap);
}

fn owner() -> SizeOwner {
    SizeOwner { client_id: id(4), client_kind: ClientKind::Local, client_name: "gtk".into() }
}

#[test]
fn golden_ev_resized_with_owner() {
    golden::<ControlMsg>(
        ControlMsg::Event {
            seq: 5,
            event: ServerEvent::PaneResized { pane_id: id(3), rows: 24, cols: 80, size_owner: Some(owner()) },
        },
        "81a54576656e7482a373657105a56576656e7481ab50616e65526573697a656484a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a4726f777318a4636f6c7350aa73697a655f6f776e657283a9636c69656e745f6964d92430343034303430342d303430342d303430342d303430342d303430343034303430343034ab636c69656e745f6b696e64a56c6f63616cab636c69656e745f6e616d65a367746b",
    );
}

#[test]
fn golden_ev_size_owner_changed() {
    golden::<ControlMsg>(
        ControlMsg::Event {
            seq: 6,
            event: ServerEvent::SizeOwnerChanged { pane_id: id(3), owner: Some(owner()) },
        },
        "81a54576656e7482a373657106a56576656e7481b053697a654f776e65724368616e67656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a56f776e657283a9636c69656e745f6964d92430343034303430342d303430342d303430342d303430342d303430343034303430343034ab636c69656e745f6b696e64a56c6f63616cab636c69656e745f6e616d65a367746b",
    );
    golden::<ControlMsg>(
        ControlMsg::Event { seq: 7, event: ServerEvent::SizeOwnerChanged { pane_id: id(3), owner: None } },
        "81a54576656e7482a373657107a56576656e7481b053697a654f776e65724368616e67656482a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a56f776e6572c0",
    );
}

/// An old client decodes `PaneResized` from the new server: the `size_owner`
/// key is unknown to it and is ignored.
#[test]
fn an_old_decoder_ignores_size_owner_on_pane_resized() {
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum OldEvent {
        PaneResized {
            #[serde(with = "uuid_str")]
            pane_id: Uuid,
            rows: u16,
            cols: u16,
        },
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum OldMsg {
        Event { seq: u64, event: OldEvent },
    }
    let new = ControlMsg::Event {
        seq: 5,
        event: ServerEvent::PaneResized {
            pane_id: id(3),
            rows: 24,
            cols: 80,
            size_owner: Some(owner()),
        },
    };
    let old: OldMsg = decode_msg(&encode_msg(&new).unwrap()).unwrap();
    assert_eq!(
        old,
        OldMsg::Event {
            seq: 5,
            event: OldEvent::PaneResized { pane_id: id(3), rows: 24, cols: 80 }
        }
    );
    // And the other way: an old server's event decodes with no owner.
    let bytes = unhex("81a54576656e7482a373657105a56576656e7481ab50616e65526573697a656483a770616e655f6964d92430333033303330332d303330332d303330332d303330332d303330333033303330333033a4726f777318a4636f6c7350");
    let back: ControlMsg = decode_msg(&bytes).unwrap();
    assert_eq!(
        back,
        ControlMsg::Event {
            seq: 5,
            event: ServerEvent::PaneResized {
                pane_id: id(3),
                rows: 24,
                cols: 80,
                size_owner: None
            }
        }
    );
}

/// What an old client decodes. `ClientKind` and the events are strict in a
/// decoder that has no `Unknown` (the shape of the pre-`local` Android code).
mod old_shape {
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum OldKind {
        Windows,
        Android,
        Cli,
        Gtk,
        #[serde(other)]
        Unknown,
    }

    #[derive(Debug, Deserialize)]
    pub struct OldEntry {
        pub client_name: String,
        pub client_kind: OldKind,
    }
}

/// `list_clients` from a newer server has a kind this version does not know.
/// The other entries still decode, and the unknown one becomes `Unknown`.
#[test]
fn a_list_clients_with_an_unknown_kind_still_decodes() {
    #[derive(serde::Serialize)]
    struct Raw {
        client_id: String,
        client_kind: &'static str,
        client_name: &'static str,
        connected_at: &'static str,
    }
    let raw = |kind, name| Raw {
        client_id: id(5).to_string(),
        client_kind: kind,
        client_name: name,
        connected_at: "2026-10-21T00:00:00Z",
    };
    let list = vec![raw("android", "phone"), raw("hologram", "future"), raw("local", "gtk")];
    let bytes = encode_msg(&list).unwrap();
    let back: Vec<ClientEntry> = decode_msg(&bytes).unwrap();
    let kinds: Vec<ClientKind> = back.iter().map(|c| c.client_kind).collect();
    assert_eq!(kinds, [ClientKind::Android, ClientKind::Unknown, ClientKind::Local]);
    assert_eq!(back[1].client_name, "future");
    // The same holds for a decoder that predates `local`.
    let old: Vec<old_shape::OldEntry> = decode_msg(&bytes).unwrap();
    assert_eq!(old[0].client_kind, old_shape::OldKind::Android);
    assert_eq!(old[1].client_kind, old_shape::OldKind::Unknown);
    assert_eq!(old[2].client_kind, old_shape::OldKind::Unknown);
    assert_eq!(old[2].client_name, "gtk");
}

/// An event this version does not know does not break the frame: it decodes
/// as `Unknown` with its `seq`. A known event with a bad payload is still an
/// error.
#[test]
fn an_unknown_event_decodes_as_unknown_and_keeps_its_seq() {
    #[derive(serde::Serialize)]
    enum Future {
        BrandNew { pane_id: String, x: u32 },
        PaneResized { pane_id: String },
    }
    #[derive(serde::Serialize)]
    enum FutureMsg {
        Event { seq: u64, event: Future },
    }
    let msg = FutureMsg::Event { seq: 9, event: Future::BrandNew { pane_id: "p".into(), x: 1 } };
    let got: ControlMsg = decode_msg(&encode_msg(&msg).unwrap()).unwrap();
    assert_eq!(got, ControlMsg::Event { seq: 9, event: ServerEvent::Unknown });

    let bad = FutureMsg::Event { seq: 10, event: Future::PaneResized { pane_id: "p".into() } };
    assert!(decode_msg::<ControlMsg>(&encode_msg(&bad).unwrap()).is_err());
}
