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

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
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

/// Optional UUID as a string, for fields an old peer does not send.
pub mod uuid_str_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    /// Serialize `Some` as the hyphenated string (`None` is skipped by the
    /// field's `skip_serializing_if`).
    pub fn serialize<S: Serializer>(id: &Option<Uuid>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.collect_str(&id.hyphenated()),
            None => s.serialize_none(),
        }
    }

    /// Parse the string form, or `None`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Uuid>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => Uuid::parse_str(&s).map(Some).map_err(serde::de::Error::custom),
            None => Ok(None),
        }
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
    /// A local socket client of the server itself (GTK, tools), listed by
    /// `list_clients` and named as a size owner. Never sent in `Hello`.
    Local,
    /// A kind this version does not know (a newer peer). Decoding never
    /// fails on a new kind: it becomes `Unknown`, so one new kind cannot make
    /// a whole `list_clients` or event unreadable.
    #[serde(other)]
    Unknown,
}

impl ClientKind {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Windows => "windows",
            ClientKind::Android => "android",
            ClientKind::Cli => "cli",
            ClientKind::Gtk => "gtk",
            ClientKind::Local => "local",
            ClientKind::Unknown => "unknown",
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
    /// New for every run of the server process (not stable across a restart,
    /// unlike `server_id`). A client that sees a different value than on its
    /// last connection knows the server restarted: its programs are gone, so
    /// it drops the input it had not seen acked. `None` from a server that
    /// does not send it (an old decoder ignores the field).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "uuid_str_opt")]
    pub boot_id: Option<Uuid>,
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

/// Decode an event, turning a variant this version does not know into
/// [`ServerEvent::Unknown`]. A known variant that does not decode stays an
/// error, so a corrupt frame is not hidden.
fn tolerant_event<'de, D: Deserializer<'de>>(d: D) -> Result<ServerEvent, D::Error> {
    let v = Value::deserialize(d)?;
    match from_value::<ServerEvent>(v.clone()) {
        Ok(e) => Ok(e),
        Err(e) => {
            // An externally tagged variant is a map with one entry, the tag.
            // A variant without fields is the bare tag string.
            let tag = match &v {
                Value::Map(m) if m.len() == 1 => m[0].0.as_str(),
                Value::String(_) => v.as_str(),
                _ => None,
            };
            match tag {
                Some(tag) if !KNOWN_EVENTS.contains(&tag) => Ok(ServerEvent::Unknown),
                _ => Err(serde::de::Error::custom(e.to_string())),
            }
        }
    }
}

/// Tags of the events this version knows.
const KNOWN_EVENTS: &[&str] = &[
    "LayoutChanged",
    "PaneClosed",
    "TitleChanged",
    "CwdChanged",
    "PaneResized",
    "SizeOwnerChanged",
    "StatusChanged",
    "Notification",
    "PaneExited",
    "ClientConnected",
    "ClientDisconnected",
    "InputDropped",
];

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
        /// The event. An unknown event becomes [`ServerEvent::Unknown`].
        #[serde(deserialize_with = "tolerant_event")]
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
        /// Who owns the pane's size now. Absent when nobody does, and from
        /// servers that do not send it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_owner: Option<SizeOwner>,
    },
    /// The pane's size owner changed (AD-020, cap `size_owner`). Sent before
    /// the `PaneResized` the new owner's size causes.
    SizeOwnerChanged {
        /// Pane id.
        #[serde(with = "uuid_str")]
        pane_id: Uuid,
        /// The new owner, `None` when the pane has none.
        owner: Option<SizeOwner>,
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
    /// An event this version does not know (a newer server). Only decoding
    /// makes it: inside `ControlMsg::Event` an unknown variant becomes
    /// `Unknown`, so the frame and its `seq` still count. Never sent.
    #[serde(skip_deserializing)]
    Unknown,
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
    /// The server uses no relay and publishes nothing to n0 (a WireGuard-only
    /// server). `relay` is then empty and the client dials `direct` only.
    /// Absent in payloads of servers that use relays, and in older payloads;
    /// an older client ignores it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relays_disabled: bool,
    /// The direct addresses (`ip:port`) the server is bound to, when it was
    /// told which ones to bind. Absent when it listens on every address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct: Vec<String>,
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

    /// The direct addresses of the payload that parse as `ip:port`.
    pub fn direct_addrs(&self) -> Vec<std::net::SocketAddr> {
        let mut out: Vec<std::net::SocketAddr> = Vec::new();
        for d in &self.direct {
            if let Ok(a) = d.trim().parse::<std::net::SocketAddr>() {
                if !out.contains(&a) {
                    out.push(a);
                }
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

/// The client that owns a pane's size (AD-020): the PTY has its size and its
/// `auto` input reaches the PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeOwner {
    /// Client id.
    #[serde(with = "uuid_str")]
    pub client_id: Uuid,
    /// Kind of client. `local` for a socket client of the server.
    pub client_kind: ClientKind,
    /// Device name.
    pub client_name: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `KNOWN_EVENTS` must list every variant a client can decode, or a
    /// malformed known event would pass for an unknown one. serde's own
    /// "expected one of" list for a bogus tag is generated from the enum, so a
    /// variant added without a `KNOWN_EVENTS` entry fails here.
    #[test]
    fn known_events_lists_every_server_event_variant() {
        let bogus = Value::Map(vec![(Value::from("NoSuchEvent"), Value::Nil)]);
        let err = from_value::<ServerEvent>(bogus).unwrap_err().to_string();
        let list = err.split("expected one of").nth(1).unwrap_or_else(|| panic!("no list: {err}"));
        let mut generated: Vec<&str> = list.split('`').skip(1).step_by(2).collect();
        generated.sort_unstable();
        let mut known = KNOWN_EVENTS.to_vec();
        known.sort_unstable();
        assert_eq!(known, generated, "KNOWN_EVENTS and the ServerEvent variants differ");
    }

    /// A bare tag string (an event without fields in a newer server) is an
    /// unknown event too.
    #[test]
    fn a_bare_unknown_tag_is_an_unknown_event() {
        let ev = Value::Map(vec![
            (Value::from("seq"), Value::from(3)),
            (Value::from("event"), Value::from("FutureThing")),
        ]);
        let msg = Value::Map(vec![(Value::from("Event"), ev)]);
        let got: ControlMsg = from_value(msg).unwrap();
        assert_eq!(got, ControlMsg::Event { seq: 3, event: ServerEvent::Unknown });
    }
}
