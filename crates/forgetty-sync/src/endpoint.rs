//! iroh `Endpoint` lifecycle wrapper for Forgetty sync.
//!
//! # API deviation from spec (iroh 0.35 → 0.97)
//!
//! | Spec (0.35)                            | Implementation (0.97)                          |
//! |----------------------------------------|------------------------------------------------|
//! | `NodeId`                               | `iroh::EndpointId`                             |
//! | `iroh::NodeId::from_secret_key(&k)`    | `secret_key.public()` (via Endpoint::id())     |
//! | `Endpoint::builder().secret_key().bind()` | `Endpoint::builder(presets::N0).secret_key().alpns().bind()` |
//! | `SecretKey::generate()`                | `SecretKey::generate(&mut rand::rngs::OsRng)`  |
//! | `Connecting::remote_node_id()`         | `Connection::remote_id()` (post-handshake)     |
//!
//! # V2-011 / AD-015: transport-only with pluggable ALPNs
//!
//! `SyncEndpoint` owns **only** the pairing ALPN (`forgetty/pair/1`) because
//! pairing is the reason `forgetty-sync` exists. Any other ALPN (terminal
//! streaming, clipboard sync, file transfer, …) is registered by the binary
//! via [`SyncEndpointBuilder::register_alpn`]. The accept loop dispatches
//! by ALPN into the registered handler map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::{endpoint::presets, endpoint::Connection, Endpoint, EndpointId, SecretKey};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    pairing::{self, OpenedWindow, PairStatus, PairingState},
    registry::{DeviceEntry, DeviceRegistry},
};

/// ALPN identifier for the retired pairing protocol (v1).
///
/// Still registered so old clients get a clear "upgrade the app" error
/// instead of a bare QUIC failure. It never pairs a device (AD-021).
pub const FORGETTY_PAIRING_ALPN_V1: &[u8] = b"forgetty/pair/1";

/// ALPN identifier for Forgetty's pairing protocol (v2, AD-021).
///
/// Owned by `forgetty-sync`; automatically registered by every
/// `SyncEndpoint` because pairing is the reason this crate exists.
pub const FORGETTY_PAIRING_ALPN: &[u8] = b"forgetty/pair/2";

/// Type-erased handler for a non-pairing ALPN.
///
/// Invoked once per accepted connection on a matching ALPN. Receives the
/// authorized-device registry so the handler can enforce its own
/// authorization policy (the registry is shared across ALPNs; updates made
/// by the pairing handler are visible here without further plumbing).
///
/// The handler returns a future that lives as long as the connection is in
/// use: the accept task awaits it and then closes the connection, so a handler
/// that returns early (or forgets to close) does not leave it open. A handler
/// that wants to keep the connection past its return must not return.
pub type AlpnHandler = Arc<
    dyn Fn(
            Connection,
            Arc<Mutex<DeviceRegistry>>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Events emitted by the sync endpoint to signal pairing and connection state
/// changes. Consumed by the socket RPC handlers for GTK polling.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// A new device successfully completed the pairing handshake.
    DevicePaired { entry: DeviceEntry },
    /// A device was revoked via the socket RPC.
    DeviceRevoked { device_id: String },
    /// A known device opened a new connection.
    DeviceConnected { device_id: String },
    /// A device's connection was closed.
    DeviceDisconnected { device_id: String },
}

/// Errors from binding or operating the sync endpoint.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("Failed to bind iroh endpoint: {0}")]
    Bind(String),
    /// The bind failed because the address is not on this machine
    /// (`io::ErrorKind::AddrNotAvailable`): a tunnel that is not up yet.
    #[error("Failed to bind iroh endpoint: {0}")]
    BindAddrNotAvailable(String),
    #[error("Registry error: {0}")]
    Registry(#[from] crate::registry::RegistryError),
}

/// Turn a failed iroh bind into a [`SyncError`], keeping the message of the
/// whole error chain and telling "address not available" apart by the kind of
/// the `io::Error` in the chain, not by its text.
fn bind_error(e: &(dyn std::error::Error + 'static)) -> SyncError {
    let mut msg = e.to_string();
    let mut not_available = false;
    let mut cur = e.source();
    while let Some(err) = cur {
        msg.push_str(": ");
        msg.push_str(&err.to_string());
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            not_available |= io.kind() == std::io::ErrorKind::AddrNotAvailable;
        }
        cur = err.source();
    }
    if not_available {
        SyncError::BindAddrNotAvailable(msg)
    } else {
        SyncError::Bind(msg)
    }
}

/// A [`SyncEndpoint::rebind`] that did not happen.
#[derive(Debug)]
pub struct RebindFailed {
    /// The old endpoint was closed already: its connections dropped.
    pub dropped: bool,
    pub error: SyncError,
}

/// What [`SyncEndpoint::revoke_device`] did.
#[derive(Debug)]
pub struct RevokeOutcome {
    /// The device was in the registry (or its removal was started).
    pub found: bool,
    /// Live connections closed.
    pub closed: usize,
    /// Set when the registry file could not be saved. The device is revoked
    /// anyway (denied in memory) and the next successful save persists it.
    pub save_error: Option<crate::registry::RegistryError>,
    /// A restart cannot bring the device back: the revoke record in
    /// `revoked.pending` or the registry file has it on disk. `false` means
    /// the device is denied in memory only.
    pub durable: bool,
}

/// What an endpoint reaches out to besides its peers (see
/// [`SyncEndpoint::outside_services`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideServices {
    /// The relay the endpoint is homed on (none: it has no relay actor).
    pub relay_urls: Vec<String>,
    /// Address lookup services: n0 DNS publishing and resolving.
    pub address_lookups: usize,
    /// Packets sent or received through a relay, relay home changes and relay
    /// paths opened, so far.
    pub relay_activity: u64,
    /// Portmapper work (UPnP, NAT-PMP, PCP probes and mapping attempts) so
    /// far. It talks to the router, so it is zero when relays are disabled.
    pub portmapper_activity: u64,
}

/// Builder for [`SyncEndpoint`].
///
/// Construct via [`SyncEndpoint::builder`], chain zero or more
/// [`register_alpn`](Self::register_alpn) calls to add non-pairing ALPNs,
/// then call [`build`](Self::build) to perform the async `Endpoint::bind`.
///
/// The pairing ALPN is always registered; callers do not need to add it.
pub struct SyncEndpointBuilder {
    secret_key: SecretKey,
    extra_alpns: HashMap<Vec<u8>, AlpnHandler>,
    bind_port: Option<u16>,
    transport: Option<iroh::endpoint::QuicTransportConfig>,
    relay_urls: Vec<iroh::RelayUrl>,
    n0_discovery: bool,
    relays_disabled: bool,
    bind_addrs: Vec<std::net::IpAddr>,
}

impl SyncEndpointBuilder {
    /// Publish this endpoint to, and resolve peers from, the n0 DNS discovery
    /// service (the `N0` preset does). Default `true`. The server turns it off
    /// when it has relays of its own: the apps get relay and direct address from
    /// the pairing payload and the address cache, so nothing needs to be public.
    pub fn n0_discovery(mut self, on: bool) -> Self {
        self.n0_discovery = on;
        self
    }

    /// No relay, no n0: the endpoint is built without iroh's N0 preset, with
    /// `RelayMode::Disabled` and no address lookup service, so it never
    /// contacts a relay or n0 DNS and publishes nothing. For a server that is
    /// reached over a VPN (WireGuard) only. Overrides `relay_urls` and
    /// `n0_discovery`.
    pub fn relays_disabled(mut self, off: bool) -> Self {
        self.relays_disabled = off;
        self
    }

    /// Bind only these IP addresses, on the port of
    /// [`bind_port`](Self::bind_port) (a random one without it). Empty (the
    /// default) binds every address. A failed bind is an error: there is no
    /// IPv4-only fallback for addresses the operator chose.
    pub fn bind_addrs(mut self, addrs: Vec<std::net::IpAddr>) -> Self {
        self.bind_addrs = addrs;
        self
    }

    /// Use these relays instead of iroh's default relay map. The endpoint
    /// connects to the first one as its home relay. Empty keeps the default.
    pub fn relay_urls(mut self, urls: Vec<iroh::RelayUrl>) -> Self {
        self.relay_urls = urls;
        self
    }

    /// Register a handler for a non-pairing ALPN.
    ///
    /// The binary that owns the terminal stream handler (or any future
    /// service) calls this once per ALPN it wants to serve. `handler` is
    /// invoked on each accepted connection whose negotiated ALPN matches
    /// `alpn`; `forgetty-sync` does no further protocol work for the
    /// connection beyond ALPN dispatch.
    ///
    /// Duplicate registrations replace the previous handler.
    pub fn register_alpn(mut self, alpn: &[u8], handler: AlpnHandler) -> Self {
        self.extra_alpns.insert(alpn.to_vec(), handler);
        self
    }

    /// Listen on this UDP port (IPv4 and IPv6) instead of random ports, so the
    /// server's direct address stays the same across restarts. `0` picks a
    /// free port. IPv6 is skipped when the host cannot bind it.
    pub fn bind_port(mut self, port: u16) -> Self {
        self.bind_port = Some(port);
        self
    }

    /// QUIC transport settings (keep-alive, idle timeout).
    pub fn transport_config(mut self, config: iroh::endpoint::QuicTransportConfig) -> Self {
        self.transport = Some(config);
        self
    }

    /// Bind the iroh endpoint and construct the [`SyncEndpoint`].
    ///
    /// All registered ALPNs (pairing + extras) are advertised during the
    /// QUIC handshake. The registry is loaded from disk (or created empty).
    pub async fn build(self) -> Result<SyncEndpoint, SyncError> {
        let mut alpn_list: Vec<Vec<u8>> = Vec::with_capacity(self.extra_alpns.len() + 1);
        alpn_list.push(FORGETTY_PAIRING_ALPN.to_vec());
        alpn_list.push(FORGETTY_PAIRING_ALPN_V1.to_vec());
        alpn_list.extend(self.extra_alpns.keys().cloned());

        let cfg = BindConfig {
            secret_key: self.secret_key.clone(),
            alpn_list,
            relays_disabled: self.relays_disabled,
            relay_urls: self.relay_urls.clone(),
            n0_discovery: self.n0_discovery,
            transport: self.transport.clone(),
            bind_addrs: self.bind_addrs.clone(),
            bind_port: self.bind_port,
        };
        let endpoint = bind_endpoint(&cfg).await?;

        let registry = Arc::new(Mutex::new(DeviceRegistry::load().map_err(SyncError::Registry)?));
        let (event_tx, _) = broadcast::channel(64);

        Ok(SyncEndpoint {
            node_id: endpoint.id(),
            endpoint: std::sync::RwLock::new(Some(endpoint)),
            cfg: Mutex::new(cfg),
            rebind_lock: tokio::sync::Mutex::new(()),
            rebinding: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            registry,
            pairing: Arc::new(Mutex::new(PairingState::new())),
            extra_alpns: Arc::new(self.extra_alpns),
            relay_urls: self.relay_urls.iter().map(|u| u.to_string()).collect(),
            n0_discovery: self.n0_discovery && !self.relays_disabled,
            relays_disabled: self.relays_disabled,
            live: crate::connections::LiveConnections::new(),
            quic_open: Arc::new(AtomicUsize::new(0)),
            handshake_failures: Arc::new(AtomicUsize::new(0)),
            event_tx,
        })
    }
}

/// What it takes to bind the iroh endpoint again: kept by [`SyncEndpoint`] so
/// [`SyncEndpoint::rebind`] can build the same endpoint (same key, same ALPNs,
/// same port) with another set of addresses.
#[derive(Clone)]
struct BindConfig {
    secret_key: SecretKey,
    alpn_list: Vec<Vec<u8>>,
    relays_disabled: bool,
    relay_urls: Vec<iroh::RelayUrl>,
    n0_discovery: bool,
    transport: Option<iroh::endpoint::QuicTransportConfig>,
    bind_addrs: Vec<std::net::IpAddr>,
    bind_port: Option<u16>,
}

/// Bind an iroh endpoint as `cfg` says.
async fn bind_endpoint(cfg: &BindConfig) -> Result<Endpoint, SyncError> {
        // Several addresses on port 0 must share one port: find a port that is
        // free on every one of them first (the endpoint binds it right after).
        let bind_port = {
            let port = cfg.bind_port.unwrap_or(0);
            if port == 0 && cfg.bind_addrs.len() > 1 {
                common_free_port(&cfg.bind_addrs)
            } else {
                port
            }
        };
        let make = |dual: bool| -> Result<iroh::endpoint::Builder, SyncError> {
            let mut builder = if cfg.relays_disabled {
                // Nothing of the N0 preset: no relays, no DNS publishing.
                // The portmapper (UPnP, NAT-PMP, PCP) is on by default and
                // asks the router: off too.
                Endpoint::empty_builder()
                    .relay_mode(iroh::RelayMode::Disabled)
                    .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
            } else {
                Endpoint::builder(presets::N0)
            }
            .secret_key(cfg.secret_key.clone())
            .alpns(cfg.alpn_list.clone());
            if !cfg.relays_disabled && !cfg.relay_urls.is_empty() {
                builder = builder.relay_mode(iroh::RelayMode::custom(cfg.relay_urls.clone()));
            }
            if !cfg.relays_disabled && !cfg.n0_discovery {
                builder = builder.clear_address_lookup();
            }
            if let Some(t) = &cfg.transport {
                builder = builder.transport_config(t.clone());
            }
            if !cfg.bind_addrs.is_empty() {
                builder = builder.clear_ip_transports();
                // iroh 0.97 takes one default socket per IP family
                // (`bind_addr`), and its duplicate check looks at the sockets
                // added so far: a default socket may not follow another
                // user-defined default. Further addresses of a family are
                // therefore added first, as extra sockets (full-length prefix,
                // no default route: each one serves its own address only, so
                // the routing stays defined), and the last address of the
                // family goes in as the default. Every address gets its own
                // socket on the same port.
                for v4 in [true, false] {
                    let family: Vec<&std::net::IpAddr> =
                        cfg.bind_addrs.iter().filter(|ip| ip.is_ipv4() == v4).collect();
                    for (i, ip) in family.iter().enumerate() {
                        let sa = std::net::SocketAddr::new(**ip, bind_port);
                        let result = if i + 1 < family.len() {
                            builder.bind_addr_with_opts(
                                sa,
                                iroh::endpoint::BindOpts::default()
                                    .set_prefix_len(if v4 { 32 } else { 128 })
                                    .set_is_default_route(false),
                            )
                        } else {
                            builder.bind_addr(sa.to_string().as_str())
                        };
                        builder = result.map_err(|e| SyncError::Bind(format!("{sa}: {e}")))?;
                    }
                }
            } else if let Some(port) = cfg.bind_port {
                builder = builder
                    .clear_ip_transports()
                    .bind_addr(format!("0.0.0.0:{port}").as_str())
                    .map_err(|e| SyncError::Bind(e.to_string()))?;
                if dual {
                    builder = builder
                        .bind_addr(format!("[::]:{port}").as_str())
                        .map_err(|e| SyncError::Bind(e.to_string()))?;
                }
            }
            Ok(builder)
        };
        // IPv6 on the same port. A host without IPv6 binds IPv4 only. Addresses
        // the operator chose are bound as they are or not at all.
        let chose_addrs = !cfg.bind_addrs.is_empty();
        let endpoint = match make(cfg.bind_port.is_some())?.bind().await {
            Ok(ep) => ep,
            Err(e) if cfg.bind_port.is_some() && !chose_addrs => {
                warn!(
                    "totem-sync: dual-stack bind on port {:?} failed ({e}), using IPv4 only",
                    cfg.bind_port
                );
                // The failed dual-stack attempt can still hold the IPv4 port for a moment
                // (WSL1 releases it late), so the IPv4-only bind retries briefly.
                let mut last: Option<SyncError> = None;
                let mut bound = None;
                for attempt in 0..8u32 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    match make(false)?.bind().await {
                        Ok(ep) => {
                            bound = Some(ep);
                            break;
                        }
                        Err(e) => last = Some(bind_error(&e)),
                    }
                }
                match bound {
                    Some(ep) => ep,
                    None => return Err(last.unwrap_or_else(|| SyncError::Bind(String::new()))),
                }
            }
            Err(e) => return Err(bind_error(&e)),
        };

        Ok(endpoint)
}

/// A UDP port that is free on every one of `addrs` (a few tries), or `0` when
/// none is found: then each address gets a port of its own.
fn common_free_port(addrs: &[std::net::IpAddr]) -> u16 {
    for _ in 0..16 {
        let Ok(first) = std::net::UdpSocket::bind((addrs[0], 0)) else { return 0 };
        let Ok(port) = first.local_addr().map(|a| a.port()) else { return 0 };
        let rest_free = addrs[1..].iter().all(|ip| std::net::UdpSocket::bind((*ip, port)).is_ok());
        if rest_free {
            return port;
        }
    }
    0
}

/// Wrapper around an iroh `Endpoint` that manages the pairing accept loop,
/// the device registry, and ALPN dispatch.
///
/// Construct via [`SyncEndpoint::builder`].
pub struct SyncEndpoint {
    /// The iroh endpoint. [`rebind`](Self::rebind) swaps it; everything else
    /// takes a clone (cheap) through `ep()`.
    endpoint: std::sync::RwLock<Option<Endpoint>>,
    /// The node id (the key never changes, a rebind keeps it).
    node_id: EndpointId,
    /// How the endpoint was bound, with the addresses it has now.
    cfg: Mutex<BindConfig>,
    /// One rebind at a time.
    rebind_lock: tokio::sync::Mutex<()>,
    /// A rebind is between the close of the old endpoint and the new one.
    rebinding: AtomicBool,
    /// [`close`](Self::close) was called: no rebind any more.
    closed: AtomicBool,
    registry: Arc<Mutex<DeviceRegistry>>,
    pairing: Arc<Mutex<PairingState>>,
    extra_alpns: Arc<HashMap<Vec<u8>, AlpnHandler>>,
    /// Every open connection by device id, so a revoke can close them all.
    live: crate::connections::LiveConnections,
    /// Incoming QUIC connections, from the first packet until they close: the
    /// handshakes in progress plus the open connections.
    quic_open: Arc<AtomicUsize>,
    /// Incoming QUIC handshakes that failed: the peer went away half way.
    handshake_failures: Arc<AtomicUsize>,
    /// Configured relay URLs (empty: iroh's default relays).
    relay_urls: Vec<String>,
    /// Whether the n0 DNS discovery service is in use.
    n0_discovery: bool,
    /// No relay and no n0 at all (see `SyncEndpointBuilder::relays_disabled`).
    relays_disabled: bool,
    /// Broadcast channel for pairing/connection events. Receivers are vended to
    /// socket RPC handlers via `subscribe()`.
    pub event_tx: broadcast::Sender<SyncEvent>,
}

impl SyncEndpoint {
    /// Incoming QUIC connections that are not closed yet, handshakes in
    /// progress included. A client that gave up half way through a handshake
    /// must not leave one behind.
    pub fn quic_open(&self) -> usize {
        self.quic_open.load(Ordering::SeqCst)
    }

    /// Incoming QUIC handshakes that failed so far (the peer gave up half way).
    pub fn handshake_failures(&self) -> usize {
        self.handshake_failures.load(Ordering::SeqCst)
    }

    /// The endpoint, or `None` during a rebind and after a failed one. A
    /// clone keeps the sockets open: hold it only as long as needed.
    fn ep(&self) -> Option<Endpoint> {
        self.endpoint.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The addresses the endpoint binds now (empty: every address).
    pub fn bind_addrs(&self) -> Vec<std::net::IpAddr> {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).bind_addrs.clone()
    }

    /// Bind the same endpoint again (same key, same node id, same port, same
    /// ALPNs) with another set of addresses: for an address that was not on
    /// the machine at start and has come up. iroh 0.97 cannot add a socket to
    /// a running endpoint, so the old endpoint is closed and a new one bound.
    /// Every open connection drops once (the clients reconnect); the device
    /// registry, the pairing window and the table of connections stay.
    ///
    /// The port is checked free on every new address first: a failure there
    /// leaves everything as it was (`dropped: false`). A failure after the
    /// close binds the old set again, so the endpoint serves on (`dropped:
    /// true`).
    pub async fn rebind(&self, addrs: Vec<std::net::IpAddr>) -> Result<(), RebindFailed> {
        let fail = |dropped: bool, error: SyncError| RebindFailed { dropped, error };
        let _one = self.rebind_lock.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err(fail(false, SyncError::Bind("the endpoint is closed".into())));
        }
        let old_cfg = self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if old_cfg.bind_addrs.is_empty() {
            return Err(fail(false, SyncError::Bind("the endpoint binds every address".into())));
        }
        let Some(port) = self.bound_sockets().first().map(|s| s.port()) else {
            return Err(fail(false, SyncError::Bind("the endpoint has no socket".into())));
        };
        for ip in addrs.iter().filter(|ip| !old_cfg.bind_addrs.contains(ip)) {
            if let Err(e) = std::net::UdpSocket::bind((*ip, port)) {
                let msg = format!("{ip}:{port}: {e}");
                return Err(fail(
                    false,
                    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                        SyncError::BindAddrNotAvailable(msg)
                    } else {
                        SyncError::Bind(msg)
                    },
                ));
            }
        }
        let mut new_cfg = old_cfg.clone();
        new_cfg.bind_addrs = addrs;
        new_cfg.bind_port = Some(port);
        let mut old_cfg = old_cfg;
        old_cfg.bind_port = Some(port);

        self.rebinding.store(true, Ordering::SeqCst);
        // Take the old endpoint out and close it. The sockets are released when
        // the last clone is gone.
        let old = self.endpoint.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(old) = old {
            if tokio::time::timeout(std::time::Duration::from_secs(2), old.close()).await.is_err() {
                warn!("totem-sync: rebind: closing the old endpoint timed out after 2 s");
            }
        }
        // The old sockets can stay a moment: bind again for a short while.
        let bind_with_retry = |cfg: BindConfig| async move {
            let mut last = SyncError::Bind(String::new());
            for attempt in 0..8u32 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                match bind_endpoint(&cfg).await {
                    Ok(ep) => return Ok(ep),
                    Err(e) => last = e,
                }
            }
            Err(last)
        };
        let result = match bind_with_retry(new_cfg.clone()).await {
            Ok(ep) => {
                *self.endpoint.write().unwrap_or_else(|e| e.into_inner()) = Some(ep);
                *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = new_cfg;
                Ok(())
            }
            Err(error) => {
                // Serve on with what was there.
                match bind_with_retry(old_cfg).await {
                    Ok(ep) => {
                        *self.endpoint.write().unwrap_or_else(|e| e.into_inner()) = Some(ep);
                    }
                    Err(e) => {
                        tracing::error!("totem-sync: rebind: the old addresses cannot be bound again: {e}");
                    }
                }
                Err(fail(true, error))
            }
        };
        self.rebinding.store(false, Ordering::SeqCst);
        result
    }

    /// Start configuring a new `SyncEndpoint`.
    pub fn builder(secret_key: SecretKey) -> SyncEndpointBuilder {
        SyncEndpointBuilder {
            secret_key,
            extra_alpns: HashMap::new(),
            bind_port: None,
            transport: None,
            relay_urls: Vec::new(),
            n0_discovery: true,
            relays_disabled: false,
            bind_addrs: Vec::new(),
        }
    }

    /// Returns the local `EndpointId` (iroh 0.97 equivalent of spec's `NodeId`).
    pub fn node_id(&self) -> EndpointId {
        self.node_id
    }

    /// Relay URLs this endpoint was configured with, as written in the pairing
    /// QR. Empty when it uses iroh's default relays.
    pub fn relay_urls(&self) -> &[String] {
        &self.relay_urls
    }

    /// Whether the endpoint publishes to and resolves from n0 DNS discovery.
    pub fn n0_discovery(&self) -> bool {
        self.n0_discovery
    }

    /// `true` when the endpoint uses no relay and no n0 at all.
    pub fn relays_disabled(&self) -> bool {
        self.relays_disabled
    }

    /// The direct addresses clients can dial, when the endpoint was told which
    /// addresses to bind: those, with the port it really listens on. Empty
    /// when it listens on every address.
    pub fn direct_addrs(&self) -> Vec<std::net::SocketAddr> {
        let addrs = self.bind_addrs();
        if addrs.is_empty() {
            return Vec::new();
        }
        self.bound_sockets()
            .into_iter()
            .filter(|s| addrs.contains(&s.ip()) && !s.ip().is_unspecified())
            .collect()
    }

    /// What the endpoint reaches out to besides its peers. All of it is
    /// empty or zero when relays are disabled.
    pub fn outside_services(&self) -> OutsideServices {
        let Some(ep) = self.ep() else {
            return OutsideServices {
                relay_urls: Vec::new(),
                address_lookups: 0,
                portmapper_activity: 0,
                relay_activity: 0,
            };
        };
        let m = ep.metrics();
        let s = &m.socket;
        OutsideServices {
            relay_urls: ep.addr().relay_urls().map(ToString::to_string).collect(),
            address_lookups: ep.address_lookup().map_or(0, |l| l.len()),
            portmapper_activity: {
                let p = &m.portmapper;
                p.probes_started.get() + p.mapping_attempts.get() + p.local_port_updates.get()
                    + p.upnp_probes.get()
            },
            relay_activity: s.send_relay.get()
                + s.recv_data_relay.get()
                + s.relay_home_change.get()
                + s.transport_relay_paths_added.get(),
        }
    }

    /// The pairing payload for this endpoint and a pairing `secret`: its
    /// relays, or the flag that it has none and the direct addresses it is
    /// bound to.
    pub fn qr_payload(&self, secret: String) -> crate::qr::QrPayload {
        let node_id = self.node_id().to_string();
        if self.relays_disabled {
            crate::qr::QrPayload::without_relays(node_id, secret, &self.direct_addrs())
        } else {
            let mut p = crate::qr::QrPayload::with_relays(node_id, secret, &self.relay_urls);
            p.direct = self.direct_addrs().iter().map(ToString::to_string).collect();
            p
        }
    }

    /// The local UDP sockets the endpoint listens on.
    pub fn bound_sockets(&self) -> Vec<std::net::SocketAddr> {
        self.ep().map(|e| e.bound_sockets()).unwrap_or_default()
    }

    /// The table of open connections.
    pub fn live_connections(&self) -> crate::connections::LiveConnections {
        self.live.clone()
    }

    /// Revoke a device: remove it from the registry AND close every open
    /// connection it has, on every ALPN.
    ///
    /// A revoke fails closed. The connections are closed and the event is sent
    /// whatever the save result is, and a device whose removal could not be
    /// saved stays denied in memory (see [`DeviceRegistry::remove`]). The
    /// connections are closed even when the device was not in the registry
    /// (for example it was removed by hand-editing the file).
    pub fn revoke_device(&self, device_id: &str) -> RevokeOutcome {
        let removed = self.registry.lock().unwrap_or_else(|e| e.into_inner()).revoke(device_id);
        let closed = self.live.revoke(device_id);
        // Removed in memory counts as found even when the save failed.
        let (found, save_error, durable) = (removed.found, removed.save_error, removed.durable);
        if found {
            let _ =
                self.event_tx.send(SyncEvent::DeviceRevoked { device_id: device_id.to_string() });
        }
        if save_error.is_some() {
            // Denied in memory meanwhile. Keep trying to put it on disk.
            crate::registry::spawn_save_retry(
                Arc::clone(&self.registry),
                std::time::Duration::from_secs(5),
            );
        }
        RevokeOutcome { found, closed, save_error, durable }
    }

    /// Returns a clone of the shared device registry.
    pub fn registry(&self) -> Arc<Mutex<DeviceRegistry>> {
        Arc::clone(&self.registry)
    }

    /// Subscribe to sync events.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    /// Open a pairing window with a fresh one-time secret (AD-021).
    ///
    /// The window lasts `ttl` and accepts one device. Opening a new window
    /// replaces the old one. Safe to call from any thread.
    pub fn open_pairing(&self, ttl: Duration) -> OpenedWindow {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner()).open(ttl, Instant::now())
    }

    /// Status of pairing window `id`.
    pub fn pairing_status(&self, id: u64) -> PairStatus {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner()).status(id, Instant::now())
    }

    /// Close the iroh endpoint gracefully.
    ///
    /// Must be called before dropping the endpoint to avoid the iroh error:
    /// `ERROR Endpoint dropped without calling Endpoint::close. Aborting ungracefully.`
    ///
    /// Bounded to 2 s: a peer that gave up mid-handshake leaves a half-open
    /// connection that would otherwise hold up shutdown.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // A rebind in progress ends first.
        let _one = self.rebind_lock.lock().await;
        let Some(ep) = self.ep() else { return };
        if tokio::time::timeout(std::time::Duration::from_secs(2), ep.close()).await.is_err() {
            tracing::warn!("iroh endpoint close timed out after 2 s");
        }
    }

    /// Run the accept loop.
    ///
    /// Accepts incoming iroh connections indefinitely, reads the negotiated
    /// ALPN via `Accepting::alpn()`, and dispatches to the appropriate
    /// handler:
    /// - `forgetty/pair/2` → [`pairing::handle_pair_v2`].
    /// - `forgetty/pair/1` → [`pairing::handle_pair_v1`] (refuses unknown devices).
    /// - Any registered non-pairing ALPN → the closure supplied to
    ///   [`SyncEndpointBuilder::register_alpn`].
    /// - Unknown ALPNs → logged and closed with code 1 (`unknown-alpn`).
    ///
    /// In iroh 0.97 `Endpoint::accept()` returns `Option<Incoming>`: `None`
    /// means the endpoint has been closed, at which point the loop exits.
    pub async fn accept_loop(self: Arc<Self>) {
        info!("totem-sync: accept loop started");
        loop {
            // No clone of the endpoint is kept while waiting for the next
            // connection that is not the one `accept` borrows: a rebind needs
            // the old sockets released.
            let next = match self.ep() {
                Some(ep) => {
                    let n = ep.accept().await;
                    drop(ep);
                    n
                }
                None => None,
            };
            let incoming = match next {
                Some(i) => i,
                None => {
                    // A rebind closes the old endpoint on purpose: wait for
                    // the new one and go on.
                    while self.rebinding.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if self.ep().is_some_and(|e| !e.is_closed()) {
                        continue;
                    }
                    info!("totem-sync: endpoint closed, accept loop exiting");
                    break;
                }
            };

            // Begin QUIC handshake — get the Accepting future.
            let mut accepting = match incoming.accept() {
                Ok(a) => a,
                Err(e) => {
                    warn!("totem-sync: incoming.accept() error: {e}");
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let pairing_state = Arc::clone(&self.pairing);
            let event_tx = self.event_tx.clone();
            let extras = Arc::clone(&self.extra_alpns);
            let live = self.live.clone();
            let open = OpenGuard::new(&self.quic_open);
            let failures = self.handshake_failures.clone();

            // Everything that waits on the peer happens in the spawned task:
            // a peer that stalls its handshake (up to the 30 s idle timeout)
            // must not hold up the next one.
            tokio::spawn(async move {
                let _open = open;
                // Read the negotiated ALPN before completing the handshake.
                // `Accepting::alpn()` is async and takes `&mut self`; after it
                // returns the `Accepting` can still be awaited for the Connection.
                let alpn = match accepting.alpn().await {
                    Ok(a) => a,
                    Err(e) => {
                        failures.fetch_add(1, Ordering::SeqCst);
                        warn!("totem-sync: failed to read ALPN: {e}");
                        return;
                    }
                };
                match accepting.await {
                    Ok(conn) => {
                        let watch = conn.clone();
                        // Track before dispatch: a revoke that lands while the
                        // handler starts still finds this connection.
                        live.track(conn.remote_id().to_string(), conn.clone());
                        if alpn == FORGETTY_PAIRING_ALPN {
                            pairing::handle_pair_v2(conn, registry, pairing_state, event_tx).await;
                        } else if alpn == FORGETTY_PAIRING_ALPN_V1 {
                            pairing::handle_pair_v1(conn, registry, event_tx).await;
                        } else if let Some(handler) = extras.get(alpn.as_slice()) {
                            handler(conn, registry).await;
                        } else {
                            warn!("totem-sync: unknown ALPN {:?}, closing connection", alpn);
                            conn.close(1u8.into(), b"unknown-alpn");
                        }
                        // The handler is done: close, whatever it did. A
                        // close after the handler's own is a no-op.
                        watch.close(0u8.into(), b"done");
                        watch.closed().await;
                    }
                    Err(e) => {
                        failures.fetch_add(1, Ordering::SeqCst);
                        warn!("totem-sync: connection handshake error: {e}");
                    }
                }
            });
        }
    }
}

/// Counts one incoming QUIC connection while it lives.
struct OpenGuard(Arc<AtomicUsize>);

impl OpenGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        OpenGuard(counter.clone())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
