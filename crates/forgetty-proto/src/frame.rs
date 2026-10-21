//! Frame I/O: `[u32 BE length][payload]`, 4 MiB cap (section 1).
//!
//! Works over any `tokio::io::AsyncRead` / `AsyncWrite`, so iroh streams, TCP
//! and in-memory duplex pipes all fit.
//!
//! The readers are not cancel safe. Run each reader in its own task and do not
//! race it inside `select!`.
//!
//! Three kinds of payload use the same framing:
//!
//! - MessagePack messages ([`write_msg`] / [`read_msg`]).
//! - Raw PTY bytes on a Pane output stream ([`write_output_frame`] /
//!   [`read_output_frame`]). The first frame of such a stream is a MessagePack
//!   `PaneStreamHeader`.
//! - Input frames `[16 byte pane uuid][input bytes]` ([`write_input_frame`] /
//!   [`read_input_frame`]).

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::consts::{
    INPUT_FLAGS_LEN, INPUT_HEADER_LEN, INPUT_HEADER_LEN_FLAGS, INPUT_SEQ_LEN, MAX_FRAME_LEN,
    PANE_UUID_LEN,
};

/// Frames up to this size are written with one `write_all` call.
const COALESCE_LIMIT: usize = 16 * 1024;

/// Errors of the codec and the frame reader/writer.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The stream failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The stream ended inside a frame.
    #[error("stream ended inside a frame")]
    UnexpectedEof,
    /// The length prefix is over [`MAX_FRAME_LEN`]. Close the stream with
    /// `close_code::FRAME_TOO_LARGE`.
    #[error("frame of {len} bytes is over the {MAX_FRAME_LEN} byte cap")]
    TooLarge {
        /// Length that was announced or offered.
        len: usize,
    },
    /// MessagePack encode failed.
    #[error("encode error: {0}")]
    Encode(String),
    /// MessagePack decode failed.
    #[error("decode error: {0}")]
    Decode(String),
    /// An input frame shorter than its 24 byte header.
    #[error("input frame of {len} bytes is shorter than its header")]
    ShortInput {
        /// Frame length.
        len: usize,
    },
}

/// Encode a message as named-field MessagePack.
pub fn encode_msg<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    rmp_serde::to_vec_named(msg).map_err(|e| FrameError::Encode(e.to_string()))
}

/// Decode a message from MessagePack.
pub fn decode_msg<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    rmp_serde::from_slice(bytes).map_err(|e| FrameError::Decode(e.to_string()))
}

/// Write one frame. One `write_all` call for small frames, so a keystroke
/// leaves in a single packet.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> Result<(), FrameError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len: payload.len() });
    }
    let len = (payload.len() as u32).to_be_bytes();
    if payload.len() <= COALESCE_LIMIT {
        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&len);
        buf.extend_from_slice(payload);
        w.write_all(&buf).await?;
    } else {
        w.write_all(&len).await?;
        w.write_all(payload).await?;
    }
    Ok(())
}

/// Read one frame into `buf` (cleared first). Returns `Ok(false)` on a clean
/// end of stream at a frame boundary.
pub async fn read_frame_into<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<bool, FrameError> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut len_buf[got..]).await?;
        if n == 0 {
            return if got == 0 { Ok(false) } else { Err(FrameError::UnexpectedEof) };
        }
        got += n;
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge { len });
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::UnexpectedEof
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(true)
}

/// Read one frame. `Ok(None)` is a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut buf = Vec::new();
    if read_frame_into(r, &mut buf).await? {
        Ok(Some(buf))
    } else {
        Ok(None)
    }
}

/// Encode and write a MessagePack message as one frame.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), FrameError> {
    write_frame(w, &encode_msg(msg)?).await
}

/// Read and decode one MessagePack message. `Ok(None)` is a clean end.
pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> Result<Option<T>, FrameError> {
    match read_frame(r).await? {
        Some(bytes) => Ok(Some(decode_msg(&bytes)?)),
        None => Ok(None),
    }
}

/// Write raw PTY bytes as one frame of a Pane output stream.
pub async fn write_output_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    bytes: &[u8],
) -> Result<(), FrameError> {
    write_frame(w, bytes).await
}

/// Read one raw PTY byte frame into `buf`. `Ok(false)` is a clean end.
pub async fn read_output_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<bool, FrameError> {
    read_frame_into(r, buf).await
}

/// Build the payload of an input frame: `[16 byte uuid][u64 BE seq][bytes]`.
pub fn encode_input_frame(pane: Uuid, seq: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(INPUT_HEADER_LEN + bytes.len());
    out.extend_from_slice(pane.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Split an input frame payload into pane, sequence number and bytes.
pub fn decode_input_frame(payload: &[u8]) -> Result<(Uuid, u64, &[u8]), FrameError> {
    if payload.len() < INPUT_HEADER_LEN {
        return Err(FrameError::ShortInput { len: payload.len() });
    }
    let mut raw = [0u8; PANE_UUID_LEN];
    raw.copy_from_slice(&payload[..PANE_UUID_LEN]);
    let mut seq = [0u8; INPUT_SEQ_LEN];
    seq.copy_from_slice(&payload[PANE_UUID_LEN..INPUT_HEADER_LEN]);
    Ok((Uuid::from_bytes(raw), u64::from_be_bytes(seq), &payload[INPUT_HEADER_LEN..]))
}

/// Build the payload of an input frame with a flags byte, for a connection
/// that negotiated `input_flags` (AD-025):
/// `[16 byte uuid][u64 BE seq][u8 flags][bytes]`.
pub fn encode_input_frame_flags(pane: Uuid, seq: u64, flags: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(INPUT_HEADER_LEN_FLAGS + bytes.len());
    out.extend_from_slice(pane.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(flags);
    out.extend_from_slice(bytes);
    out
}

/// Split an input frame payload that has a flags byte into pane, sequence
/// number, flags and bytes. Flag bits this version does not know are returned
/// as they are, so the caller can ignore them.
pub fn decode_input_frame_flags(payload: &[u8]) -> Result<(Uuid, u64, u8, &[u8]), FrameError> {
    if payload.len() < INPUT_HEADER_LEN_FLAGS {
        return Err(FrameError::ShortInput { len: payload.len() });
    }
    let (pane, seq, rest) = decode_input_frame(payload)?;
    Ok((pane, seq, rest[0], &rest[INPUT_FLAGS_LEN..]))
}

/// Write an input frame with a flags byte.
pub async fn write_input_frame_flags<W: AsyncWrite + Unpin>(
    w: &mut W,
    pane: Uuid,
    seq: u64,
    flags: u8,
    bytes: &[u8],
) -> Result<(), FrameError> {
    write_frame(w, &encode_input_frame_flags(pane, seq, flags, bytes)).await
}

/// Read an input frame with a flags byte. `Ok(None)` is a clean end of stream.
pub async fn read_input_frame_flags<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<(Uuid, u64, u8, Vec<u8>)>, FrameError> {
    match read_frame(r).await? {
        Some(payload) => {
            let (pane, seq, flags, rest) = decode_input_frame_flags(&payload)?;
            Ok(Some((pane, seq, flags, rest.to_vec())))
        }
        None => Ok(None),
    }
}

/// Write an input frame.
pub async fn write_input_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    pane: Uuid,
    seq: u64,
    bytes: &[u8],
) -> Result<(), FrameError> {
    write_frame(w, &encode_input_frame(pane, seq, bytes)).await
}

/// Read an input frame. `Ok(None)` is a clean end of stream.
pub async fn read_input_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<(Uuid, u64, Vec<u8>)>, FrameError> {
    match read_frame(r).await? {
        Some(payload) => {
            let (pane, seq, rest) = decode_input_frame(&payload)?;
            Ok(Some((pane, seq, rest.to_vec())))
        }
        None => Ok(None),
    }
}
