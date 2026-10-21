//! Client errors.

use forgetty_proto::FrameError;

/// Error returned by the client API.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ClientError {
    /// No connection became available before the deadline.
    #[error("not connected")]
    NotConnected,
    /// The server did not answer in time.
    #[error("request timed out")]
    Timeout,
    /// The connection dropped while the call was in flight. The call may or may
    /// not have run on the server.
    #[error("connection lost")]
    ConnectionLost,
    /// The connection failed for good (`proto_mismatch`, `not_authorized`).
    #[error("connection failed: {0}")]
    Failed(String),
    /// The server answered with a `Failure`.
    #[error("server error {code}: {message}")]
    Server {
        /// Error code.
        code: String,
        /// Server text.
        message: String,
    },
    /// Input buffered while disconnected hit the 64 KiB cap.
    #[error("input buffer full")]
    InputBufferFull,
    /// The pane already has a live `PaneStream`.
    #[error("pane already attached")]
    PaneAlreadyAttached,
    /// The client was closed.
    #[error("client closed")]
    Closed,
    /// Pairing was refused by the server.
    #[error("pairing refused: {0}")]
    PairRejected(String),
    /// A frame or message could not be encoded or decoded.
    #[error("protocol error: {0}")]
    Proto(String),
    /// Bad argument, for example an unparsable QR payload.
    #[error("invalid argument: {0}")]
    Invalid(String),
    /// Local setup failed (endpoint bind, identity file).
    #[error("setup error: {0}")]
    Setup(String),
}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        ClientError::Proto(e.to_string())
    }
}
