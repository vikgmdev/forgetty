//! Forgetty protocol v2 (`forgetty/rpc/2`, `forgetty/pair/2`).
//!
//! Wire types, the MessagePack codec and frame I/O for the contract in
//! `docs/architecture/PROTOCOL_V2.md`. This crate has no iroh dependency, so
//! the server, the Android client and the Windows client share it unchanged.
//!
//! - [`consts`]: ALPNs, limits, close codes, error codes, capability names.
//! - [`msg`]: every message type.
//! - [`frame`]: `[u32 BE length][payload]` reader and writer over
//!   `tokio::io::AsyncRead` / `AsyncWrite`, the raw pane-output frame helpers
//!   and the input frame codec `[16-byte uuid][bytes]`.

pub mod consts;
pub mod frame;
pub mod msg;

pub use consts::*;
pub use frame::{
    decode_input_frame, decode_msg, encode_input_frame, encode_msg, read_frame, read_frame_into,
    read_input_frame, read_msg, read_output_frame, write_frame, write_input_frame, write_msg,
    write_output_frame, FrameError,
};
pub use msg::*;
/// MessagePack dynamic value used for `params` and `result`.
pub use rmpv::Value;
