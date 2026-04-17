//! Binary frame codec for the Unix-socket streaming protocol.
//!
//! Implements the `[u32 big-endian length][payload]` framing specified by
//! AD-010. Used by the `subscribe_output` streaming handler in `server.rs` and
//! by the matching reader in `forgetty-gtk::daemon_client`.
//!
//! # Relation to `forgetty-sync`
//!
//! The iroh transport uses the same `[u32 BE length][payload]` shape, paired
//! with MessagePack payloads (see `forgetty-sync/src/stream.rs`). The two
//! framers intentionally live in separate crates: the iroh codec is coupled
//! to `iroh::endpoint::{RecvStream, SendStream}` and MessagePack types; this
//! codec is coupled to `tokio::net::UnixStream` halves and raw byte payloads.
//! The ~20 lines of duplicated framing logic are accepted per V2-003 SPEC §4.1
//! to avoid a dependency inversion that would contradict AD-015.
//!
//! # Maximum frame size
//!
//! `MAX_FRAME_SIZE` matches `forgetty-sync/src/stream.rs::MAX_FRAME_SIZE`
//! exactly (4 MiB). A length prefix exceeding this cap causes `read_frame`
//! to return an error *before* allocating the payload buffer, preventing
//! unbounded allocations on malformed or malicious input.
//!
//! # References
//!
//! - AD-010: raw PTY bytes in length-prefixed binary frames.
//! - AD-015: `forgetty-sync` remains transport-only; no reuse across crates.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum allowed frame payload size (4 MiB).
///
/// Matches `forgetty-sync/src/stream.rs::MAX_FRAME_SIZE` exactly. Frames with
/// a length prefix exceeding this cap are rejected without allocating.
pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// Write one length-prefixed binary frame to `w`.
///
/// Wire shape: `[u32 big-endian length][payload bytes]`.
///
/// Returns `Err(InvalidInput)` if `payload.len() > MAX_FRAME_SIZE`. Uses
/// `write_all` for both the length prefix and the payload so short writes
/// are handled correctly.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame payload {} exceeds MAX_FRAME_SIZE {}", payload.len(), MAX_FRAME_SIZE),
        ));
    }
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    Ok(())
}

/// Read one length-prefixed binary frame from `r` into `buf`.
///
/// `buf` is cleared and refilled with the frame payload. Reads exactly
/// 4 + N bytes (where N is the length prefix). The length is validated
/// against `MAX_FRAME_SIZE` *before* the payload buffer is resized, so a
/// malicious 4 GiB length prefix cannot provoke a large allocation.
///
/// Returns `Err(UnexpectedEof)` on clean stream close before a full frame
/// is read, `Err(InvalidData)` on an oversized length prefix, or any other
/// `io::Error` from the underlying reader.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_FRAME_SIZE {MAX_FRAME_SIZE}"),
        ));
    }

    buf.clear();
    buf.resize(len, 0);
    if len > 0 {
        r.read_exact(buf).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn round_trip(payload: Vec<u8>) {
        let (mut a, mut b) = duplex(MAX_FRAME_SIZE + 1024);
        let written = payload.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a, &written).await.unwrap();
        });

        let mut out = Vec::new();
        read_frame(&mut b, &mut out).await.unwrap();
        writer.await.unwrap();

        assert_eq!(out, payload);
    }

    #[tokio::test]
    async fn round_trip_zero() {
        round_trip(Vec::new()).await;
    }

    #[tokio::test]
    async fn round_trip_one_byte() {
        round_trip(vec![0x42]).await;
    }

    #[tokio::test]
    async fn round_trip_64k() {
        let payload: Vec<u8> = (0..65_536).map(|i| (i & 0xff) as u8).collect();
        round_trip(payload).await;
    }

    #[tokio::test]
    async fn round_trip_just_under_max() {
        // Use MAX - 1 to keep the duplex buffer comfortably sized.
        let payload = vec![0xa5u8; MAX_FRAME_SIZE - 1];
        round_trip(payload).await;
    }

    #[tokio::test]
    async fn write_rejects_oversize() {
        // Use a sink that would accept any write; the cap should reject
        // without touching the underlying writer.
        let mut sink: Vec<u8> = Vec::new();
        let payload = vec![0u8; MAX_FRAME_SIZE + 1];
        let err = write_frame(&mut sink, &payload).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(sink.is_empty(), "no bytes should be written when payload is oversized");
    }

    #[tokio::test]
    async fn read_rejects_oversize_length_prefix() {
        // Hand-craft a length prefix of MAX + 1 with no payload.
        let len = (MAX_FRAME_SIZE as u32) + 1;
        let mut src: &[u8] = &len.to_be_bytes();
        let mut buf = Vec::new();
        let err = read_frame(&mut src, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(buf.is_empty(), "no bytes should be allocated on oversize prefix");
    }

    #[tokio::test]
    async fn read_eof_before_length() {
        let mut src: &[u8] = &[];
        let mut buf = Vec::new();
        let err = read_frame(&mut src, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_eof_mid_payload() {
        // Length prefix says 10, supply only 3 bytes.
        let mut src: Vec<u8> = Vec::new();
        src.extend_from_slice(&10u32.to_be_bytes());
        src.extend_from_slice(&[1, 2, 3]);
        let mut slice: &[u8] = &src;
        let mut buf = Vec::new();
        let err = read_frame(&mut slice, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
