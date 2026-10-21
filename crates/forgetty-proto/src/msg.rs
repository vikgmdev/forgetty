//! Message types of protocol v2.
//!
//! Encoding rules (all pinned by golden tests in `tests/golden.rs`):
//!
//! - MessagePack with named struct fields (`rmp_serde::to_vec_named`).
//! - Enums are externally tagged: `{"Variant": {fields...}}`.
//! - Uuids travel as 36 character hyphenated strings, never as 16 byte `bin`.
//!   The one exception is the input stream frame, which is raw bytes.
//! - `Option::None` is MessagePack `nil`.
//! - Base64 fields of the JSON socket API are raw `bin` inside [`Value`].

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

use crate::consts::{PROTO_MAJOR, PROTO_MINOR};
use crate::frame::FrameError;
use crate::Value;

/// Serde helper: a [`Uuid`] as a hyphenated string.
pub mod uuid_str {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    /// Serialize as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
    pub fn serialize<S: Serializer>(id: &Uuid, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&id.hyphenated())
    }

    /// Parse the hyphenated (or simple) string form.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Uuid, D::Error> {
        let s = String::deserialize(d)?;
        Uuid::parse_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Kind of client, sent in `Hello` and `PairRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    /// Windows native client.
    Windows,
    /// Android app.
    Android,
    /// `forgetty` CLI.
    Cli,
    /// GTK client.
    Gtk,
}

impl ClientKind {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Windows => "windows",
            ClientKind::Android => "android",
            ClientKind::Cli => "cli",
            ClientKind::Gtk => "gtk",
        }
    }
}

/// First frame on Control, client to server (section 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Always [`PROTO_MAJOR`].
    pub proto_major: u16,
    /// Minor version of the sender.
    pub proto_minor: u16,
    /// Persisted by the client, stable across reconnects.
    #[serde(with = "uuid_str")]
    pub client_id: Uuid,
    /// Random per client process start. Input `seq` restarts at 1 for every
    /// new epoch; the server dedups on `(client_id, input_epoch)`.
    #[serde(with = "uuid_str")]
    pub input_epoch: Uuid,
    /// Kind of client.
    pub client_kind: ClientKind,
    /// Device name shown in `client ls`.
    pub client_name: String,
    /// Client version string.
    pub client_version: String,
    /// Capability names the client supports.
    pub capabilities: Vec<String>,
}

impl Hello {
    /// A `Hello` for this crate's protocol version.
    pub fn new(
        client_id: Uuid,
        input_epoch: Uuid,
        client_kind: ClientKind,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Self {
        Hello {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            client_id,
            input_epoch,
            client_kind,
            client_name: client_name.into(),
            client_version: client_version.into(),
            capabilities,
        }
    }
}

/// Server answer to `Hello` (section 3), wrapped in [`HandshakeReply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// Always [`PROTO_MAJOR`].
    pub proto_major: u16,
    /// Minor version of the server.
    pub proto_minor: u16,
    /// Stable id of this server.
    #[serde(with = "uuid_str")]
    pub server_id: Uuid,
    /// Server version string.
    pub server_version: String,
    /// Host name of the machine.
    pub hostname: String,
    /// Capability names the server supports.
    pub capabilities: Vec<String>,
}

/// `Error { code, message }` of the handshake and pairing paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMsg {
    /// Machine readable code, see [`crate::error_code`].
    pub code: String,
    /// Human readable text.
    #[serde(default)]
    pub message: String,
}

/// The first server frame on Control: `Welcome`, or `Error` (for example
/// `proto_mismatch`, after which the server closes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeReply {
    /// Handshake accepted.
    Welcome(Welcome),
    /// Handshake refused.
    Error(ErrorMsg),
}

/// Control stream message (section 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Client to server call.
    Request {
        /// Correlation id chosen by the client.
        id: u64,
        /// Method name.
        method: String,
        /// Parameters, a MessagePack map.
        params: Value,
    },
    /// Server to client success answer.
    Response {
        /// The request id.
        id: u64,
        /// Result value.
        result: Value,
    },
    /// Server to client failure answer.
    Failure {
        /// The request id.
        id: u64,
        /// Error code, see [`crate::error_code`].
        code: String,
        /// Human readable text.
        message: String,
    },
    /// Server to client event, after `subscribe_events`.
    Event {
        /// Event sequence number.
        seq: u64,
        /// The event.
        event: ServerEvent,
    },
    /// Client to server liveness probe, every 1 s while connected.
    Ping {
        /// Echoed in the `Pong`.
        nonce: u64,
    },
    /// Server to client answer to `Ping`.
    Pong {
        /// The `Ping` nonce.
        nonce: u64,
        /// Highest input `seq` written to a PTY for this `client_id`.
        input_acked: u64,
    },
}

impl ControlMsg {
    /// Build a `Request` from typed params. The params become a MessagePack
    /// map (named fields).
    pub fn request<P: Serialize>(
        id: u64,
        method: &str,
        params: &P,
    ) -> Result<ControlMsg, FrameError> {
        Ok(ControlMsg::Request { id, method: method.to_string(), params: to_value(params)? })
    }
}

/// Server pushed event (section 4). Ids are strings on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerEvent {
    /// The layout changed. `layout` has the shape of `get_layout`.
    LayoutChanged {
        /// Full layout value.
        layout: Value,
    },
    /// A pane closed.
    PaneClosed {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
    },
    /// OSC 0/2 title change.
    TitleChanged {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// New title.
        title: String,
    },
    /// OSC 7 working directory change.
    CwdChanged {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// New working directory.
        cwd: String,
    },
    /// The PTY size changed (AD-020).
    PaneResized {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// Rows.
        rows: u16,
        /// Columns.
        cols: u16,
    },
    /// Agent status change (P4, cap `status_events`).
    StatusChanged {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// Status name.
        status: String,
        /// Status text.
        message: String,
    },
    /// A desktop notification from the pane (OSC 9/99/777, cap `status_events`).
    Notification {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// Notification title, empty when the program sent none.
        title: String,
        /// Notification body.
        body: String,
    },
    /// The pane's process ended. The pane stays listed only if it is
    /// `remain_on_exit` (cap `status_events`).
    PaneExited {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// Exit code, `None` when unknown.
        exit_code: Option<i32>,
    },
    /// Input this client sent to a pane was dropped because the pane did not
    /// read it (AD-024). Sent once the pane's queue is empty again, to the
    /// client that lost the input only, with the current event `seq`. Only
    /// with capability `input_flags`. Anything the client typed before this
    /// event may be missing in part.
    InputDropped {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// Bytes dropped.
        bytes: u64,
    },
    /// Another client attached.
    ClientConnected {
        /// Client id.
        #[serde(with = "uuid_str")]
        client_id: Uuid,
        /// Kind of client.
        client_kind: ClientKind,
        /// Device name.
        client_name: String,
        /// RFC 3339 time of the connection. Empty from a server that omits it.
        #[serde(default)]
        connected_at: String,
    },
    /// A client detached.
    ClientDisconnected {
        /// Client id.
        #[serde(with = "uuid_str")]
        client_id: Uuid,
    },
}

/// First frame of a Pane output stream (section 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneStreamHeader {
    /// The pane.
    #[serde(with = "uuid_str")]
    pub pane_id: Uuid,
    /// Absolute pane offset of the first byte after the header.
    pub start_offset: u64,
    /// `true`: the client resets its VT state before applying the bytes.
    pub reset: bool,
    /// PTY rows now.
    pub rows: u16,
    /// PTY columns now.
    pub cols: u16,
    /// Snapshot catch-up (AD-023, capability `snapshot_catchup`): the first
    /// `snapshot_len` bytes after the header are a VT snapshot of the screen,
    /// not pane bytes. They do not advance the pane offset. The pane bytes
    /// that follow start at `start_offset`. `0` for a normal stream.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub snapshot_len: u32,
    /// The pane offset at which the replay ends: the log's `total_written` at
    /// the moment the stream starts. Bytes before it are history the client
    /// is catching up on, bytes from it on are live. A snapshot stream sets it
    /// to the snapshot's offset, so all its pane bytes are live. `None` from a
    /// server that does not send it (an old decoder ignores the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_end: Option<u64>,
}

/// Client to server, first frame on a `forgetty/pair/2` bi stream (section 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    /// The secret from the QR payload (base32).
    pub secret: String,
    /// Device name.
    pub device_name: String,
    /// Kind of client.
    pub client_kind: ClientKind,
}

/// Server reply to [`PairRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    /// `true` when the device is now paired.
    pub ok: bool,
    /// Why it failed: `bad_secret`, `expired`, `bad_request`, `server_error`.
    pub reason: Option<String>,
}

/// QR payload version 2 (section 7). Encoded as compact JSON inside the QR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrPayload {
    /// Always 2.
    pub v: u32,
    /// Server iroh endpoint id.
    pub node_id: String,
    /// Host name of the machine.
    pub machine: String,
    /// Relay URL. With several relays this is the first one.
    pub relay: String,
    /// All relay URLs of the server, in order, including `relay`. Absent in
    /// payloads from servers with one relay and in older payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    /// One-time secret, 16 random bytes as base32.
    pub secret: String,
}

/// Error from [`QrPayload::parse`].
#[derive(Debug, thiserror::Error)]
pub enum QrError {
    /// Not valid JSON of the expected shape.
    #[error("invalid QR payload: {0}")]
    Json(#[from] serde_json::Error),
    /// A version other than 2.
    #[error("unsupported QR payload version {0}")]
    Version(u32),
}

impl QrPayload {
    /// Current QR payload version.
    pub const VERSION: u32 = 2;

    /// Compact JSON for the QR code.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("QrPayload serializes")
    }

    /// Every relay URL in the payload: `relays`, or `relay` when `relays` is
    /// empty. Empty strings and duplicates are dropped.
    pub fn relay_urls(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in std::iter::once(&self.relay).chain(self.relays.iter()) {
            if !r.is_empty() && !out.contains(r) {
                out.push(r.clone());
            }
        }
        out
    }

    /// Parse and check the version.
    pub fn parse(json: &str) -> Result<Self, QrError> {
        let p: QrPayload = serde_json::from_str(json)?;
        if p.v != Self::VERSION {
            return Err(QrError::Version(p.v));
        }
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// Typed params and results of the new v2 methods (section 4)
// ---------------------------------------------------------------------------

/// Params of `attach_pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneParams {
    /// The pane.
    #[serde(with = "uuid_str")]
    pub pane_id: Uuid,
    /// Resume offset, `None` for "from the oldest byte".
    pub from_offset: Option<u64>,
}

/// Params of `detach_pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachPaneParams {
    /// The pane.
    #[serde(with = "uuid_str")]
    pub pane_id: Uuid,
}

/// Params of `resize_pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizePaneParams {
    /// The pane.
    #[serde(with = "uuid_str")]
    pub pane_id: Uuid,
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

/// Result of `attach_pane` and `detach_pane`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResult {
    /// Always `true` on a `Response`.
    pub ok: bool,
}

/// Result of `subscribe_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeResult {
    /// Sequence number of the last event before the subscription.
    pub seq: u64,
}

/// One element of the `list_clients` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEntry {
    /// Client id.
    #[serde(with = "uuid_str")]
    pub client_id: Uuid,
    /// Kind of client.
    pub client_kind: ClientKind,
    /// Device name.
    pub client_name: String,
    /// RFC 3339 time of the connection.
    pub connected_at: String,
}

/// Decode a [`Value`] into a typed result or params struct.
pub fn from_value<T: DeserializeOwned>(v: Value) -> Result<T, FrameError> {
    let bytes = rmp_serde::to_vec(&v).map_err(|e| FrameError::Encode(e.to_string()))?;
    rmp_serde::from_slice(&bytes).map_err(|e| FrameError::Decode(e.to_string()))
}

/// Encode a typed struct into a [`Value`]. Structs become maps with named
/// fields.
pub fn to_value<T: Serialize>(v: &T) -> Result<Value, FrameError> {
    let bytes = rmp_serde::to_vec_named(v).map_err(|e| FrameError::Encode(e.to_string()))?;
    rmp_serde::from_slice(&bytes).map_err(|e| FrameError::Decode(e.to_string()))
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}
