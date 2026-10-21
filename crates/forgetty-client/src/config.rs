//! Client configuration.

use std::time::Duration;

use crate::addr_cache::AddrCache;
use forgetty_proto::{cap, ClientKind, IDLE_TIMEOUT_SECS, KEEP_ALIVE_SECS};
use iroh::endpoint::{IdleTimeout, QuicTransportConfig};
use iroh::{EndpointAddr, RelayMode, RelayUrl};
use uuid::Uuid;

/// Who the client says it is in `Hello`.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Stable id, persisted by the app (see `load_or_generate_client_id`).
    pub client_id: Uuid,
    /// Kind of client.
    pub kind: ClientKind,
    /// Device name.
    pub name: String,
    /// App version.
    pub version: String,
    /// Capabilities offered in `Hello`.
    pub capabilities: Vec<String>,
}

impl ClientInfo {
    /// `Hello` fields with the default capability set.
    pub fn new(
        client_id: Uuid,
        kind: ClientKind,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        ClientInfo {
            client_id,
            kind,
            name: name.into(),
            version: version.into(),
            capabilities: vec![
                cap::RESUME.to_string(),
                cap::LAYOUT_EVENTS.to_string(),
                cap::STATUS_EVENTS.to_string(),
                cap::SIZE_POLICY_LATEST.to_string(),
                cap::SNAPSHOT_CATCHUP.to_string(),
            ],
        }
    }
}

/// Which relays the client endpoint may use.
#[derive(Debug, Clone, Default)]
pub enum RelayChoice {
    /// Use the relay URLs inside the server address (from the QR payload). No
    /// relay at all when the address has none. This is the default.
    #[default]
    FromServerAddr,
    /// No relay. Direct addresses only.
    Disabled,
    /// iroh's default relay map.
    IrohDefault,
    /// These relays.
    Custom(Vec<RelayUrl>),
}

impl RelayChoice {
    pub(crate) fn mode(&self, server: &EndpointAddr) -> RelayMode {
        match self {
            RelayChoice::FromServerAddr => {
                let urls: Vec<RelayUrl> = server.relay_urls().cloned().collect();
                if urls.is_empty() {
                    RelayMode::Disabled
                } else {
                    RelayMode::custom(urls)
                }
            }
            RelayChoice::Disabled => RelayMode::Disabled,
            RelayChoice::IrohDefault => RelayMode::Default,
            RelayChoice::Custom(urls) => RelayMode::custom(urls.clone()),
        }
    }
}

/// Tuning knobs. The defaults follow PROTOCOL_V2.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Relay selection.
    pub relay: RelayChoice,
    /// Default timeout of `request`.
    pub request_timeout: Duration,
    /// Dial plus handshake timeout. A dial that hangs is retried.
    pub dial_timeout: Duration,
    /// Items buffered per `PaneStream` before the QUIC stream is back-pressured.
    pub pane_buffer: usize,
    /// Events buffered per `EventStream` before it reports a resync.
    pub event_buffer: usize,
    /// Last direct address per server. Dialed first on every connect.
    pub addr_cache: AddrCache,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            relay: RelayChoice::default(),
            request_timeout: Duration::from_secs(10),
            dial_timeout: Duration::from_secs(5),
            pane_buffer: 64,
            event_buffer: 1024,
            addr_cache: AddrCache::default(),
        }
    }
}

/// QUIC transport config for both sides (section 1): keep-alive 5 s, idle
/// timeout 30 s.
pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(KEEP_ALIVE_SECS))
        .max_idle_timeout(Some(
            IdleTimeout::try_from(Duration::from_secs(IDLE_TIMEOUT_SECS)).expect("30 s fits"),
        ))
        .build()
}
