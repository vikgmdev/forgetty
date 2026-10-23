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
    /// A server that is reached on its direct addresses only (the pairing
    /// payload said it uses no relay, `ClientOptions::relays_disabled`) did
    /// not answer for a while. The supervisor goes on redialing with the same
    /// backoff; the app can say "Can't reach the server. Is WireGuard on?"
    /// instead of spinning. Any connection ends it.
    UnreachableDirectOnly {
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
            ConnectionState::Reconnecting { since }
            | ConnectionState::UnreachableDirectOnly { since } => {
                now.saturating_duration_since(*since)
                    >= Duration::from_millis(RECONNECTING_UI_DELAY_MS)
            }
            _ => false,
        }
    }

    /// A stable name of the state, for logs and for the apps' strings:
    /// `connected`, `reconnecting`, `unreachable_direct_only`, `failed`,
    /// `closed`.
    pub fn name(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "connected",
            ConnectionState::Reconnecting { .. } => "reconnecting",
            ConnectionState::UnreachableDirectOnly { .. } => "unreachable_direct_only",
            ConnectionState::Failed(_) => "failed",
            ConnectionState::Closed => "closed",
        }
    }

    /// `true` for `Connected`.
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }
}
