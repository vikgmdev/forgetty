//! Observable connection state.

use std::time::{Duration, Instant};

use forgetty_proto::RECONNECTING_UI_DELAY_MS;

/// State of the connection to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Handshake done, streams open.
    Connected,
    /// No connection. The supervisor is redialing (also the state before the
    /// first connection).
    Reconnecting {
        /// When the connection was lost (or the client started).
        since: Instant,
    },
    /// The supervisor stopped for good: `proto_mismatch`, `not_authorized`.
    Failed(String),
    /// `close()` was called.
    Closed,
}

impl ConnectionState {
    /// `true` when the UI should show "reconnecting": 1 s without a
    /// connection, never sooner (PROTOCOL_V2 section 9).
    pub fn show_reconnecting(&self, now: Instant) -> bool {
        match self {
            ConnectionState::Reconnecting { since } => {
                now.saturating_duration_since(*since)
                    >= Duration::from_millis(RECONNECTING_UI_DELAY_MS)
            }
            _ => false,
        }
    }

    /// `true` for `Connected`.
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }
}
