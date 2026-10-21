//! Frame reader/writer behaviour.

use forgetty_proto::*;
use tokio::io::{duplex, AsyncWriteExt};
use uuid::Uuid;

#[tokio::test]
async fn frame_round_trip_and_clean_eof() {
    let (mut a, mut b) = duplex(1 << 20);
    write_frame(&mut a, b"hello").await.unwrap();
    write_frame(&mut a, b"").await.unwrap();
    write_frame(&mut a, &vec![7u8; 100_000]).await.unwrap();
    drop(a);
    assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"hello");
    assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"");
    assert_eq!(read_frame(&mut b).await.unwrap().unwrap().len(), 100_000);
    assert!(read_frame(&mut b).await.unwrap().is_none());
}

#[tokio::test]
async fn cap_is_inclusive_4_mib() {
    let (mut a, mut b) = duplex(1 << 16);
    let ok = vec![1u8; MAX_FRAME_LEN];
    let w = tokio::spawn(async move {
        write_frame(&mut a, &ok).await.unwrap();
        let too_big = vec![1u8; MAX_FRAME_LEN + 1];
        assert!(matches!(write_frame(&mut a, &too_big).await, Err(FrameError::TooLarge { .. })));
    });
    assert_eq!(read_frame(&mut b).await.unwrap().unwrap().len(), MAX_FRAME_LEN);
    w.await.unwrap();
}

#[tokio::test]
async fn reader_rejects_oversize_prefix_without_allocating() {
    let (mut a, mut b) = duplex(64);
    a.write_all(&((MAX_FRAME_LEN as u32 + 1).to_be_bytes())).await.unwrap();
    match read_frame(&mut b).await {
        Err(FrameError::TooLarge { len }) => assert_eq!(len, MAX_FRAME_LEN + 1),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn truncated_frame_is_unexpected_eof() {
    let (mut a, mut b) = duplex(64);
    a.write_all(&[0, 0, 0, 10, 1, 2, 3]).await.unwrap();
    drop(a);
    assert!(matches!(read_frame(&mut b).await, Err(FrameError::UnexpectedEof)));

    let (mut a, mut b) = duplex(64);
    a.write_all(&[0, 0]).await.unwrap();
    drop(a);
    assert!(matches!(read_frame(&mut b).await, Err(FrameError::UnexpectedEof)));
}

#[tokio::test]
async fn msg_round_trip_through_stream() {
    let (mut a, mut b) = duplex(1 << 16);
    let msg = ControlMsg::request(
        1,
        "resize_pane",
        &ResizePaneParams { pane_id: Uuid::from_bytes([9; 16]), rows: 30, cols: 100 },
    )
    .unwrap();
    write_msg(&mut a, &msg).await.unwrap();
    let back: ControlMsg = read_msg(&mut a_peer(&mut b)).await.unwrap().unwrap();
    assert_eq!(back, msg);
    if let ControlMsg::Request { params, .. } = back {
        let p: ResizePaneParams = from_value(params).unwrap();
        assert_eq!((p.rows, p.cols), (30, 100));
    }
}

fn a_peer<T>(t: &mut T) -> &mut T {
    t
}

#[tokio::test]
async fn input_frames_round_trip() {
    let (mut a, mut b) = duplex(1 << 16);
    let p1 = Uuid::from_bytes([1; 16]);
    let p2 = Uuid::from_bytes([2; 16]);
    write_input_frame(&mut a, p1, 1, b"a").await.unwrap();
    write_input_frame(&mut a, p2, u64::MAX, &vec![0u8; 1024]).await.unwrap();
    drop(a);
    assert_eq!(read_input_frame(&mut b).await.unwrap().unwrap(), (p1, 1, b"a".to_vec()));
    let (p, seq, bytes) = read_input_frame(&mut b).await.unwrap().unwrap();
    assert_eq!((p, seq, bytes.len()), (p2, u64::MAX, 1024));
    assert!(read_input_frame(&mut b).await.unwrap().is_none());
}

#[test]
fn short_input_frame_is_rejected() {
    assert!(matches!(decode_input_frame(&[0u8; 23]), Err(FrameError::ShortInput { len: 23 })));
    // Exactly the header and no bytes is legal.
    assert!(decode_input_frame(&[0u8; 24]).unwrap().2.is_empty());
}

#[tokio::test]
async fn pane_stream_shape() {
    // header frame, then raw frames
    let (mut a, mut b) = duplex(1 << 16);
    let h = PaneStreamHeader {
        pane_id: Uuid::from_bytes([3; 16]),
        start_offset: 10,
        reset: false,
        rows: 24,
        cols: 80,
    };
    write_msg(&mut a, &h).await.unwrap();
    write_output_frame(&mut a, b"\x1b[31mred").await.unwrap();
    drop(a);
    let got: PaneStreamHeader = read_msg(&mut b).await.unwrap().unwrap();
    assert_eq!(got, h);
    let mut buf = Vec::new();
    assert!(read_output_frame(&mut b, &mut buf).await.unwrap());
    assert_eq!(buf, b"\x1b[31mred");
    assert!(!read_output_frame(&mut b, &mut buf).await.unwrap());
}

#[test]
fn unknown_event_variant_fails_decode_cleanly() {
    // A frame from a newer server with an event this crate does not know.
    let bytes = rmp_unknown_event();
    assert!(matches!(decode_msg::<ControlMsg>(&bytes), Err(FrameError::Decode(_))));
}

fn rmp_unknown_event() -> Vec<u8> {
    let v = Value::Map(vec![(
        Value::from("Event"),
        Value::Map(vec![
            (Value::from("seq"), Value::from(1)),
            (
                Value::from("event"),
                Value::Map(vec![(Value::from("FutureThing"), Value::Map(vec![]))]),
            ),
        ]),
    )]);
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &v).unwrap();
    out
}
