//! Pairing (AD-021, `docs/architecture/PROTOCOL_V2.md` section 7).
//!
//! # Flow
//!
//! 1. `forgetty pair` asks the server to open a pairing window
//!    ([`PairingState::open`]). The window lives 60 s, holds one fresh 128-bit
//!    secret and accepts one device.
//! 2. The QR payload carries the secret (base32). The client dials ALPN
//!    `forgetty/pair/2`, opens a bi stream and sends a framed
//!    [`PairRequest`].
//! 3. The server checks the secret ([`PairingState::verify`]). A wrong, expired
//!    or already used secret gets `PairResult { ok: false, reason }`. Success
//!    consumes the window, stores the device and logs its name and id.
//!
//! Framing is `[u32 BE length][MessagePack]`, same as every v2 stream.
//!
//! ALPN `forgetty/pair/1` no longer pairs. An unknown device on it gets an
//! "upgrade the app" error. A known device keeps the old connectivity check.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_encoding::BASE32_NOPAD;
use iroh::{endpoint::Connection, EndpointId};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    registry::{iso8601_now, DeviceEntry, DeviceRegistry},
    sanitize, SyncEvent,
};

/// Pairing window length (AD-021).
pub const PAIRING_WINDOW_SECS: u64 = 60;

/// Secret length in bytes (128 bit).
pub const SECRET_LEN: usize = 16;

/// Largest pairing frame the server reads.
const MAX_PAIR_FRAME: usize = 16 * 1024;

/// How long a client has to send its `PairRequest` after connecting.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Client to server, first frame on the pair/2 bi stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    /// The secret from the QR payload (base32).
    pub secret: String,
    /// Device name shown in `forgetty devices`.
    pub device_name: String,
    /// `"windows"`, `"android"`, `"cli"` or `"gtk"`.
    pub client_kind: String,
}

/// Server to client reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResult {
    /// `true` when the device is now paired.
    pub ok: bool,
    /// Why the request failed: `bad_secret`, `expired` or `bad_request`.
    pub reason: Option<String>,
}

/// Why a secret check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairFailure {
    /// No window is open, the window timed out, or it was already used.
    Expired,
    /// A window is open but the secret does not match.
    BadSecret,
}

impl PairFailure {
    /// Wire string for `PairResult.reason`.
    pub fn reason(self) -> &'static str {
        match self {
            PairFailure::Expired => "expired",
            PairFailure::BadSecret => "bad_secret",
        }
    }
}

/// A freshly opened window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedWindow {
    /// Window id, used to poll [`PairingState::status`].
    pub id: u64,
    /// The secret, base32 without padding.
    pub secret: String,
    /// Lifetime in seconds.
    pub ttl_secs: u64,
}

/// State of a window, as seen by the `forgetty pair` poller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStatus {
    /// Waiting for a device.
    Open { secs_left: u64 },
    /// A device paired.
    Paired { device_id: String, name: String },
    /// The window timed out or was replaced before a device paired.
    Expired,
}

struct Window {
    id: u64,
    secret: [u8; SECRET_LEN],
    expires: Instant,
}

/// The pairing window state machine. Pure: time is passed in, so tests need no
/// sleeping.
#[derive(Default)]
pub struct PairingState {
    window: Option<Window>,
    next_id: u64,
    paired: Option<(u64, String, String)>,
    /// Window whose secret was accepted and whose device is being saved. The
    /// save fsyncs, so this gap is long enough for a status poll to land in
    /// it. It reads as "open" until `record_paired` or `abandon_pairing`.
    in_progress: Option<u64>,
}

impl PairingState {
    /// Empty state with no open window.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a window with a fresh secret. Replaces any window still open.
    pub fn open(&mut self, ttl: Duration, now: Instant) -> OpenedWindow {
        let secret: [u8; SECRET_LEN] = rand::random();
        self.open_with_secret(secret, ttl, now)
    }

    /// Same as [`open`](Self::open) with a caller-chosen secret (tests).
    pub fn open_with_secret(
        &mut self,
        secret: [u8; SECRET_LEN],
        ttl: Duration,
        now: Instant,
    ) -> OpenedWindow {
        self.next_id += 1;
        let id = self.next_id;
        self.window = Some(Window { id, secret, expires: now + ttl });
        OpenedWindow { id, secret: BASE32_NOPAD.encode(&secret), ttl_secs: ttl.as_secs() }
    }

    /// Check a presented secret. Success consumes the window, so the same
    /// secret never works twice. A wrong secret leaves the window open.
    pub fn verify(&mut self, presented: &str, now: Instant) -> Result<u64, PairFailure> {
        let Some(window) = self.window.as_ref() else {
            return Err(PairFailure::Expired);
        };
        if now >= window.expires {
            self.window = None;
            return Err(PairFailure::Expired);
        }
        let decoded = BASE32_NOPAD.decode(presented.trim().to_ascii_uppercase().as_bytes());
        let matches = match decoded {
            Ok(bytes) => constant_time_eq(&bytes, &window.secret),
            Err(_) => false,
        };
        if !matches {
            return Err(PairFailure::BadSecret);
        }
        let id = window.id;
        self.window = None;
        self.in_progress = Some(id);
        Ok(id)
    }

    /// Close the open window now, so its QR code stops working. With `Some(id)` only the
    /// window with that id is closed, so a late cancel never ends a newer window. Returns
    /// `true` when a window was closed. A window whose device is being saved is past
    /// cancelling: the device is already accepted.
    pub fn close(&mut self, id: Option<u64>) -> bool {
        match &self.window {
            Some(w) if id.is_none_or(|i| i == w.id) => {
                self.window = None;
                true
            }
            _ => false,
        }
    }

    /// Remember which device used window `id`.
    pub fn record_paired(&mut self, id: u64, device_id: &str, name: &str) {
        if self.in_progress == Some(id) {
            self.in_progress = None;
        }
        self.paired = Some((id, device_id.to_string(), name.to_string()));
    }

    /// The device of window `id` could not be saved: the window is spent.
    pub fn abandon_pairing(&mut self, id: u64) {
        if self.in_progress == Some(id) {
            self.in_progress = None;
        }
    }

    /// Status of window `id`.
    pub fn status(&self, id: u64, now: Instant) -> PairStatus {
        if let Some((pid, device_id, name)) = &self.paired {
            if *pid == id {
                return PairStatus::Paired { device_id: device_id.clone(), name: name.clone() };
            }
        }
        if self.in_progress == Some(id) {
            // The secret was accepted a moment ago and the device is being
            // saved. Not expired, not yet paired.
            return PairStatus::Open { secs_left: 1 };
        }
        match &self.window {
            Some(w) if w.id == id && now < w.expires => {
                PairStatus::Open { secs_left: (w.expires - now).as_secs() + 1 }
            }
            _ => PairStatus::Expired,
        }
    }
}

/// Compare without an early exit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// ALPN forgetty/pair/2
// ---------------------------------------------------------------------------

/// Handle one connection on `forgetty/pair/2`.
pub async fn handle_pair_v2(
    conn: Connection,
    registry: Arc<Mutex<DeviceRegistry>>,
    state: Arc<Mutex<PairingState>>,
    event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
) {
    let remote_id: EndpointId = conn.remote_id();
    let device_id = remote_id.to_string();

    let (mut send, mut recv) = match tokio::time::timeout(REQUEST_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok(s)) => s,
        _ => {
            warn!("pair: {device_id} opened no stream, closing");
            conn.close(2u8.into(), b"no-stream");
            return;
        }
    };

    let request: Option<PairRequest> =
        match tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut recv)).await {
            Ok(Some(bytes)) => rmp_serde::from_slice(&bytes).ok(),
            _ => None,
        };

    let result = match request {
        None => {
            warn!("pair: {device_id} sent no valid PairRequest");
            PairResult { ok: false, reason: Some("bad_request".to_string()) }
        }
        Some(req) => {
            // Everything in the request except the secret is attacker text.
            let req_name = sanitize::untrusted(&req.device_name);
            let req_kind = sanitize::untrusted(&req.client_kind);
            let verdict =
                state.lock().unwrap_or_else(|e| e.into_inner()).verify(&req.secret, Instant::now());
            match verdict {
                Err(failure) => {
                    warn!("pair: refused {device_id} ({req_name}): {}", failure.reason());
                    PairResult { ok: false, reason: Some(failure.reason().to_string()) }
                }
                Ok(window_id) => {
                    let name = name_or_fallback(&req_name, &device_id);
                    let entry = DeviceEntry {
                        device_id: device_id.clone(),
                        name: name.clone(),
                        paired_at: iso8601_now(),
                        last_seen: None,
                    };
                    let saved = {
                        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                        // A re-pair replaces the old entry for the same device.
                        let _ = reg.remove(&device_id);
                        reg.add(entry.clone())
                    };
                    match saved {
                        Ok(()) => {
                            info!("pair: paired device name={name:?} id={device_id} kind={req_kind:?}");
                            state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .record_paired(window_id, &device_id, &name);
                            let _ = event_tx.send(SyncEvent::DevicePaired { entry });
                            PairResult { ok: true, reason: None }
                        }
                        Err(e) => {
                            warn!("pair: failed to save device {device_id}: {e}");
                            state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .abandon_pairing(window_id);
                            PairResult { ok: false, reason: Some("server_error".to_string()) }
                        }
                    }
                }
            }
        }
    };

    if let Ok(payload) = rmp_serde::to_vec_named(&result) {
        let _ = write_frame(&mut send, &payload).await;
    }
    let _ = send.finish();
    // Let the reply drain: wait for the client to close the connection.
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    conn.close(0u8.into(), b"pair-done");
}

/// A sanitised device name, or `device-<id prefix>` when nothing is left.
fn name_or_fallback(clean: &str, device_id: &str) -> String {
    if clean.is_empty() {
        format!("device-{}", &device_id[..device_id.len().min(8)])
    } else {
        clean.to_string()
    }
}

async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_PAIR_FRAME {
        return None;
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

async fn write_frame(send: &mut iroh::endpoint::SendStream, payload: &[u8]) -> std::io::Result<()> {
    send.write_all(&(payload.len() as u32).to_be_bytes()).await.map_err(std::io::Error::other)?;
    send.write_all(payload).await.map_err(std::io::Error::other)
}

/// Client side of `forgetty/pair/2`: dial, send a `PairRequest`, read the
/// `PairResult`. Used by the QA tools; the Android and Windows clients
/// implement the same exchange.
pub async fn request_pairing(
    endpoint: &iroh::Endpoint,
    addr: iroh::EndpointAddr,
    secret: &str,
    device_name: &str,
    client_kind: &str,
) -> anyhow::Result<PairResult> {
    let conn = endpoint
        .connect(addr, crate::FORGETTY_PAIRING_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
    let (mut send, mut recv) =
        conn.open_bi().await.map_err(|e| anyhow::anyhow!("open_bi failed: {e}"))?;
    let request = PairRequest {
        secret: secret.to_string(),
        device_name: device_name.to_string(),
        client_kind: client_kind.to_string(),
    };
    write_frame(&mut send, &rmp_serde::to_vec_named(&request)?).await?;
    let _ = send.finish();
    let bytes = tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut recv))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for PairResult"))?
        .ok_or_else(|| anyhow::anyhow!("server closed the stream without a PairResult"))?;
    let result: PairResult = rmp_serde::from_slice(&bytes)?;
    conn.close(0u8.into(), b"done");
    Ok(result)
}

// ---------------------------------------------------------------------------
// ALPN forgetty/pair/1 (retired for pairing)
// ---------------------------------------------------------------------------

/// Message sent to unknown devices that still dial `forgetty/pair/1`.
pub const UPGRADE_REQUIRED_MSG: &str = "pairing needs the new protocol (forgetty/pair/2): upgrade the Forgetty app and scan a fresh QR code";

/// Handle one connection on the old `forgetty/pair/1` ALPN.
///
/// Known devices keep the connectivity check. Unknown devices get an
/// "upgrade the app" error and are never paired.
pub async fn handle_pair_v1(
    conn: Connection,
    registry: Arc<Mutex<DeviceRegistry>>,
    event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
) {
    let remote_id: EndpointId = conn.remote_id();
    let device_id = remote_id.to_string();

    let is_authorized =
        registry.lock().unwrap_or_else(|e| e.into_inner()).is_authorized(&remote_id);

    if is_authorized {
        {
            let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            let _ = reg.update_last_seen(&device_id);
        }
        info!("pair/1: known device connected, device_id={device_id}");
        let _ = event_tx.send(SyncEvent::DeviceConnected { device_id });
        conn.close(0u8.into(), b"connected-ok");
        return;
    }

    if let Ok((mut send, mut recv)) = conn.open_bi().await {
        let msg = serde_json::json!({
            "v": 1,
            "error": "upgrade_required",
            "message": UPGRADE_REQUIRED_MSG,
        });
        let mut line = msg.to_string();
        line.push('\n');
        let _ = send.write_all(line.as_bytes()).await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_millis(300), recv.read(&mut [0u8; 16])).await;
    }
    warn!("pair/1: refused unknown device {device_id} (upgrade required)");
    conn.close(1u8.into(), b"upgrade-required");
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; SECRET_LEN] = [7u8; SECRET_LEN];

    fn open(state: &mut PairingState, now: Instant) -> OpenedWindow {
        state.open_with_secret(SECRET, Duration::from_secs(PAIRING_WINDOW_SECS), now)
    }

    #[test]
    fn a_closed_window_refuses_its_secret_and_reads_as_expired() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert!(st.close(None));
        assert_eq!(st.verify(&w.secret, t0), Err(PairFailure::Expired));
        assert_eq!(st.status(w.id, t0), PairStatus::Expired);
        // Closing again finds nothing.
        assert!(!st.close(None));
    }

    #[test]
    fn close_with_an_id_ends_only_that_window() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let old = open(&mut st, t0);
        let new = st.open_with_secret([5u8; SECRET_LEN], Duration::from_secs(60), t0);
        // A late cancel for the replaced window leaves the new one alone.
        assert!(!st.close(Some(old.id)));
        assert_eq!(st.verify(&new.secret, t0), Ok(new.id));
        let again = open(&mut st, t0);
        assert!(st.close(Some(again.id)));
    }

    #[test]
    fn a_device_being_saved_is_past_cancelling() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert_eq!(st.verify(&w.secret, t0), Ok(w.id));
        assert!(!st.close(None));
        assert!(matches!(st.status(w.id, t0), PairStatus::Open { .. }));
    }

    #[test]
    fn right_secret_succeeds() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert_eq!(st.verify(&w.secret, t0 + Duration::from_secs(1)), Ok(w.id));
    }

    #[test]
    fn wrong_secret_is_refused_and_window_stays_open() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        let wrong = BASE32_NOPAD.encode(&[9u8; SECRET_LEN]);
        assert_eq!(st.verify(&wrong, t0), Err(PairFailure::BadSecret));
        assert_eq!(st.verify("not base32 !!", t0), Err(PairFailure::BadSecret));
        assert_eq!(st.verify("", t0), Err(PairFailure::BadSecret));
        // The right secret still works afterwards.
        assert_eq!(st.verify(&w.secret, t0), Ok(w.id));
    }

    #[test]
    fn secret_of_wrong_length_is_refused() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        open(&mut st, t0);
        let short = BASE32_NOPAD.encode(&SECRET[..8]);
        assert_eq!(st.verify(&short, t0), Err(PairFailure::BadSecret));
    }

    #[test]
    fn expired_window_is_refused_even_with_right_secret() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        let late = t0 + Duration::from_secs(PAIRING_WINDOW_SECS);
        assert_eq!(st.verify(&w.secret, late), Err(PairFailure::Expired));
        assert_eq!(st.status(w.id, late), PairStatus::Expired);
        // Closed for good, not just late.
        assert_eq!(st.verify(&w.secret, t0), Err(PairFailure::Expired));
    }

    #[test]
    fn secret_cannot_be_reused_after_success() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert!(st.verify(&w.secret, t0).is_ok());
        assert_eq!(st.verify(&w.secret, t0), Err(PairFailure::Expired));
    }

    #[test]
    fn no_window_means_expired() {
        let mut st = PairingState::new();
        assert_eq!(st.verify("AAAA", Instant::now()), Err(PairFailure::Expired));
    }

    #[test]
    fn new_window_replaces_old_secret() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let first = st.open(Duration::from_secs(60), t0);
        let second = st.open(Duration::from_secs(60), t0);
        assert_ne!(first.secret, second.secret, "secrets are random");
        assert_eq!(st.verify(&first.secret, t0), Err(PairFailure::BadSecret));
        assert_eq!(st.status(first.id, t0), PairStatus::Expired);
        assert_eq!(st.verify(&second.secret, t0), Ok(second.id));
    }

    #[test]
    fn status_tracks_open_paired_and_expired() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert!(matches!(st.status(w.id, t0), PairStatus::Open { .. }));
        let id = st.verify(&w.secret, t0).unwrap();
        st.record_paired(id, "abc", "phone");
        assert_eq!(
            st.status(w.id, t0),
            PairStatus::Paired { device_id: "abc".into(), name: "phone".into() }
        );
    }

    /// The gap between "secret accepted" and "device saved" must not read as
    /// expired: `forgetty pair` polls status and gave up in that gap.
    #[test]
    fn status_is_open_while_the_device_is_being_saved() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        let id = st.verify(&w.secret, t0).unwrap();
        assert!(matches!(st.status(w.id, t0), PairStatus::Open { .. }), "not expired mid-save");
        st.abandon_pairing(id);
        assert_eq!(st.status(w.id, t0), PairStatus::Expired, "a failed save ends the window");
    }

    #[test]
    fn secret_is_128_bit_base32() {
        let mut st = PairingState::new();
        let w = st.open(Duration::from_secs(60), Instant::now());
        assert_eq!(BASE32_NOPAD.decode(w.secret.as_bytes()).unwrap().len(), 16);
    }

    #[test]
    fn secret_check_ignores_case() {
        let mut st = PairingState::new();
        let t0 = Instant::now();
        let w = open(&mut st, t0);
        assert!(st.verify(&w.secret.to_ascii_lowercase(), t0).is_ok());
    }

    #[test]
    fn hostile_names_never_reach_the_stored_or_logged_form() {
        let id = "b8aaa7768b3a624d";
        for raw in [
            "evil\nINFO pair: paired device name=\"admin\"".to_string(),
            "\u{1b}[31mred\u{1b}[0m".to_string(),
            "A".repeat(10 * 1024),
            "\u{1b}[2J\n\r".repeat(500),
        ] {
            let name = name_or_fallback(&sanitize::untrusted(&raw), id);
            assert!(name.chars().count() <= sanitize::MAX_UNTRUSTED_CHARS.max(16));
            assert!(!name.chars().any(|c| c.is_control()), "control char in {name:?}");
        }
        // Nothing left after cleaning: fall back to an id-based name.
        let name = name_or_fallback(&sanitize::untrusted("\n\u{1b}[0m"), id);
        assert_eq!(name, "device-b8aaa776");
    }

    #[test]
    fn registry_add_sanitises_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = crate::registry::DeviceRegistry::empty_at(tmp.path().join("authorized.json"));
        reg.add(DeviceEntry {
            device_id: "abc".into(),
            name: "x\ny\u{1b}[31m".into(),
            paired_at: "now".into(),
            last_seen: None,
        })
        .unwrap();
        assert_eq!(reg.list()[0].name, "xy");
    }

    #[test]
    fn pair_result_round_trips_msgpack() {
        let r = PairResult { ok: false, reason: Some("expired".into()) };
        let bytes = rmp_serde::to_vec_named(&r).unwrap();
        assert_eq!(rmp_serde::from_slice::<PairResult>(&bytes).unwrap(), r);
    }
}
