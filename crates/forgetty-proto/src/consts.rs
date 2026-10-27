//! Constants from PROTOCOL_V2: ALPNs, limits, close codes, error codes and
//! capability names.

/// ALPN of the main protocol (section 1).
pub const ALPN_RPC: &[u8] = b"forgetty/rpc/2";
/// ALPN of the pairing protocol (section 7, AD-021).
pub const ALPN_PAIR: &[u8] = b"forgetty/pair/2";

/// Protocol major version. A different major gets `proto_mismatch`.
pub const PROTO_MAJOR: u16 = 2;
/// Protocol minor version implemented by this crate.
pub const PROTO_MINOR: u16 = 0;

/// Frame length cap, 4 MiB (section 1).
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

/// QUIC keep-alive interval on both sides, in seconds (section 1).
pub const KEEP_ALIVE_SECS: u64 = 5;
/// QUIC max idle timeout on both sides, in seconds (section 1).
pub const IDLE_TIMEOUT_SECS: u64 = 30;

/// Length of a pane uuid inside an input frame.
pub const PANE_UUID_LEN: usize = 16;
/// Length of the sequence number inside an input frame.
pub const INPUT_SEQ_LEN: usize = 8;
/// Length of an input frame header: `[16 uuid][u64 BE seq]`.
pub const INPUT_HEADER_LEN: usize = PANE_UUID_LEN + INPUT_SEQ_LEN;
/// Length of the flags byte that follows `seq` when `input_flags` is
/// negotiated (AD-025).
pub const INPUT_FLAGS_LEN: usize = 1;
/// Length of an input frame header with the flags byte:
/// `[16 uuid][u64 BE seq][u8 flags]`.
pub const INPUT_HEADER_LEN_FLAGS: usize = INPUT_HEADER_LEN + INPUT_FLAGS_LEN;
/// Input flag bit 0: the bytes are an automatic terminal reply (DA, CPR,
/// kitty-keyboard answers, focus reports), not something the user typed
/// (AD-025). A frame without this bit is `user` input.
pub const INPUT_FLAG_AUTO: u8 = 0b0000_0001;

/// The client sends `Ping` this often while connected (section 4).
pub const PING_INTERVAL_MS: u64 = 1000;
/// No `Pong` for this long means the connection is dead.
pub const PONG_DEAD_AFTER_MS: u64 = 3000;
/// After `network_changed()` the client waits this long for a `Pong`.
pub const NETWORK_CHANGE_PONG_MS: u64 = 1000;

/// Reconnect backoff in milliseconds (section 9). The first redial is
/// immediate. After that the client waits these delays and then 2 s forever.
pub const RECONNECT_BACKOFF_MS: [u64; 5] = [100, 250, 500, 1000, 2000];
/// Delay used after [`RECONNECT_BACKOFF_MS`] is exhausted.
pub const RECONNECT_STEADY_MS: u64 = 2000;
/// The UI shows "reconnecting" only after this long without a connection.
pub const RECONNECTING_UI_DELAY_MS: u64 = 1000;
/// Input buffered while disconnected is capped at this many bytes.
pub const INPUT_BUFFER_CAP: usize = 64 * 1024;

/// QUIC application close codes (section 1).
pub mod close_code {
    /// Normal close.
    pub const OK: u32 = 0x00;
    /// Unknown node id: not in `authorized_devices.json`.
    pub const NOT_AUTHORIZED: u32 = 0x01;
    /// A frame went over the 4 MiB cap.
    pub const FRAME_TOO_LARGE: u32 = 0x02;
}

/// Stream reset codes on a Pane output stream (section 8).
pub mod stream_code {
    /// The log no longer holds the bytes (rotated or lost).
    pub const LOG_LOST: u32 = 0x11;
    /// The server ended this stream and opened a newer one for the same pane
    /// (snapshot catch-up). Not an error: the client reads the new stream.
    pub const SUPERSEDED: u32 = 0x13;
}

/// String error codes used in `Error` and `Failure` messages.
pub mod error_code {
    /// Different `proto_major` (section 3).
    pub const PROTO_MISMATCH: &str = "proto_mismatch";
    /// The device is not authorized (also close code `0x01`).
    pub const NOT_AUTHORIZED: &str = "not_authorized";
    /// The first frame was not a valid `Hello`, or a request had bad params.
    pub const BAD_REQUEST: &str = "bad_request";
    /// `Failure` for a method the server does not know.
    pub const UNKNOWN_METHOD: &str = "unknown_method";
    /// `Failure` for a pane, tab or device that does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// `Failure` for a method that needs a capability the peer did not list.
    pub const CAPABILITY_MISSING: &str = "capability_missing";
    /// The server is stopping and takes no new connections (handshake `Error`).
    /// The only handshake error the client retries.
    pub const SERVER_STOPPING: &str = "server_stopping";
    /// Any other server side error.
    pub const INTERNAL: &str = "internal";
}

/// Capability names (section 3).
pub mod cap {
    /// Pane streams resume from an offset.
    pub const RESUME: &str = "resume";
    /// `LayoutChanged` and related events.
    pub const LAYOUT_EVENTS: &str = "layout_events";
    /// `StatusChanged` events (P4).
    pub const STATUS_EVENTS: &str = "status_events";
    /// The `capture` method (P4).
    pub const CAPTURE: &str = "capture";
    /// PTY size follows the latest active client (AD-020).
    pub const SIZE_POLICY_LATEST: &str = "size_policy_latest";
    /// A slow pane stream catches up by a VT snapshot (AD-023).
    pub const SNAPSHOT_CATCHUP: &str = "snapshot_catchup";
    /// `SizeOwnerChanged` events (AD-020). `size_owner` fields need no
    /// capability.
    pub const SIZE_OWNER: &str = "size_owner";
    /// Input frames carry a flags byte after `seq` (AD-025).
    pub const INPUT_FLAGS: &str = "input_flags";
    /// `split_pane` takes `before` (the new pane goes left of or above the source).
    pub const SPLIT_BEFORE: &str = "split_before";
    /// `delete_workspace`, `duplicate_workspace`, `move_workspace` and `set_workspace_color`
    /// take workspace ids (`workspace_id`, `source_workspace_id`, `from_id`, `to_id`).
    pub const WORKSPACE_ID: &str = "workspace_id";
    /// `ShellState` events: prompt marks (OSC 133), the shell's directory
    /// (OSC 7) and the tty mode of a pane (P7.1).
    pub const SHELL_STATE: &str = "shell_state";
}

/// Method names that are new in v2 (section 4).
pub mod method {
    /// Attach a pane output stream.
    pub const ATTACH_PANE: &str = "attach_pane";
    /// Detach a pane output stream.
    pub const DETACH_PANE: &str = "detach_pane";
    /// Start the event flow on Control.
    pub const SUBSCRIBE_EVENTS: &str = "subscribe_events";
    /// Capture pane text (cap `capture`).
    pub const CAPTURE: &str = "capture";
    /// List connected clients.
    pub const LIST_CLIENTS: &str = "list_clients";
    /// Report the size this client wants (socket API name).
    pub const RESIZE_PANE: &str = "resize_pane";
}
