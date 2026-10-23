//! The machines a user paired, the device identity and the address cache,
//! stored in `%APPDATA%\forgetty\` with user-only permissions (`acl`).
//!
//! Files: `identity.key` (32 raw bytes, the iroh secret key), `client_id`
//! (uuid text), `machines.json`, `addr_cache.json`. The one-time pairing
//! secret is never stored.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use forgetty_client::proto::{ClientKind, QrPayload};
use forgetty_client::{
    local_network, pair_with_addr, qr_to_addr, server_addr_direct, AddrCache, AddrCacheSnapshot, ClientError, ClientOptions,
    EndpointId, SecretKey,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::acl::{self, write_private};

const MAX_MACHINES: usize = 32;
/// Version of `machines.json`. 1: no pinned addresses (a direct address was only in the address
/// cache). 2: `pinned_addrs` and `relays_disabled`.
const MACHINES_VERSION: u32 = 2;
/// Most direct addresses kept from one pairing payload.
const MAX_PINNED: usize = 8;
/// The whole output of `forgetty pair` (an ASCII QR block, the payload line, status lines) is accepted.
const MAX_OFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMachine {
    pub id: Uuid,
    pub name: String,
    pub node_id: String,
    /// Every relay of the pairing payload, in order. Older files held one
    /// `relay` string; it reads as a one entry list.
    #[serde(default, alias = "relay", deserialize_with = "relays_de")]
    pub relays: Vec<String>,
    /// The server uses no relay (a WireGuard-only server): the client binds without one and
    /// dials the pinned addresses only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relays_disabled: bool,
    /// Direct addresses (`ip:port`) of the server: from the pairing payload, or typed in by the
    /// user. They are dialed on every network and never evicted (a WireGuard address such as
    /// `10.230.1.14:47297` is private but valid wherever the tunnel is up). Addresses the client
    /// learns itself live in the address cache.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_addrs: Vec<SocketAddr>,
}

fn relays_de<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum One {
        Text(String),
        List(Vec<String>),
    }
    Ok(match One::deserialize(d)? {
        One::Text(t) if t.is_empty() => Vec::new(),
        One::Text(t) => vec![t],
        One::List(l) => l.into_iter().filter(|r| !r.is_empty()).collect(),
    })
}

#[derive(Default, Serialize, Deserialize)]
struct MachinesFile {
    version: u32,
    machines: Vec<StoredMachine>,
}

#[derive(Clone, Debug)]
pub struct MachineStore {
    dir: PathBuf,
}

impl MachineStore {
    /// `%APPDATA%\forgetty`, or `FORGETTY_DATA_DIR` when set.
    pub fn default_dir() -> Option<PathBuf> {
        if let Some(d) = std::env::var_os("FORGETTY_DATA_DIR") {
            return Some(PathBuf::from(d));
        }
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("forgetty"))
    }

    pub fn open(dir: impl Into<PathBuf>) -> Self {
        MachineStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn identity_path(&self) -> PathBuf {
        self.dir.join("identity.key")
    }

    pub fn client_id_path(&self) -> PathBuf {
        self.dir.join("client_id")
    }

    pub fn machines_path(&self) -> PathBuf {
        self.dir.join("machines.json")
    }

    pub fn addr_cache_path(&self) -> PathBuf {
        self.dir.join("addr_cache.json")
    }

    /// Load the device key, or create it. The file is created user-only in one step
    /// (`CREATE_NEW`), so two first runs at once cannot overwrite each other: the loser reads
    /// the winner's key. A key file that other users could read is locked down on load.
    pub fn identity(&self) -> io::Result<SecretKey> {
        let path = self.identity_path();
        let mut last = io::Error::new(io::ErrorKind::InvalidData, "identity.key is not 32 bytes");
        for _ in 0..50 {
            match std::fs::read(&path) {
                Ok(bytes) => match <[u8; 32]>::try_from(bytes) {
                    Ok(arr) => {
                        if matches!(acl::is_user_only(&path), Ok(false)) {
                            eprintln!("forgetty-win: identity.key was readable by others, restricting it");
                            acl::restrict_to_user(&path)?;
                        }
                        return Ok(SecretKey::from_bytes(&arr));
                    }
                    // Another process is writing it right now.
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                },
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let key = SecretKey::generate(&mut rand::rng());
                    match acl::create_private_new(&path, &key.to_bytes()) {
                        Ok(()) => return Ok(key),
                        // Lost the race: read the other process's key.
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Load the stable `client_id`, or create it.
    pub fn client_id(&self) -> io::Result<Uuid> {
        let path = self.client_id_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(id) = Uuid::parse_str(text.trim()) {
                return Ok(id);
            }
        }
        let id = Uuid::new_v4();
        write_private(&path, id.hyphenated().to_string().as_bytes())?;
        Ok(id)
    }

    /// Paired machines. A missing or unreadable file is an empty list. An entry that does not
    /// parse is skipped and logged, the rest load. A version 1 file is migrated once (see
    /// `migrate_v1`).
    pub fn machines(&self) -> Vec<StoredMachine> {
        let bytes = match std::fs::read(self.machines_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                eprintln!("forgetty-win: machines.json: {e}");
                return Vec::new();
            }
        };
        let Some(file) = parse_machines(&bytes, true) else {
            eprintln!("forgetty-win: machines.json is not a machine list, ignoring it (a copy is kept before the next write)");
            return Vec::new();
        };
        if file.version >= MACHINES_VERSION {
            return file.machines;
        }
        self.migrate_v1(file.machines)
    }

    /// Before version 2 a direct address given at pairing (`direct_hint`: LAN, tests) was only
    /// stored in the address cache, where it follows the network and eviction rules. Every
    /// version 1 payload carried the default relay, so a machine whose only relay is that one
    /// (or none) has its private cached addresses (LAN, WireGuard, loopback) moved to its pinned
    /// list, once. Whether the server runs without a relay is unknowable here:
    /// `relays_disabled` stays false (a server moved to `--no-relay` is paired again). The cache
    /// is left as it is, so a downgrade followed by an upgrade migrates again. A machine with a
    /// relay of its own is left alone: what its cache holds was learned, and pinning a learned
    /// address would keep a stale one for good.
    fn migrate_v1(&self, mut machines: Vec<StoredMachine>) -> Vec<StoredMachine> {
        let cache = self.addr_cache();
        for m in &mut machines {
            if !m.pinned_addrs.is_empty() || !only_default_relay(&m.relays) {
                continue;
            }
            let Ok(id) = m.node_id.parse::<EndpointId>() else { continue };
            m.pinned_addrs = cache.get_everywhere(id).into_iter().filter(is_private_addr).collect();
        }
        if let Err(e) = self.save_machines(&machines) {
            eprintln!("forgetty-win: machines.json migration: {e}");
        }
        machines
    }

    /// Add a machine, or replace the entry with the same node id (keeping its
    /// local id).
    pub fn add(&self, mut m: StoredMachine) -> io::Result<StoredMachine> {
        let mut list = self.machines();
        if let Some(old) = list.iter_mut().find(|x| x.node_id == m.node_id) {
            m.id = old.id;
            *old = m.clone();
        } else {
            if list.len() >= MAX_MACHINES {
                return Err(io::Error::other("too many paired machines"));
            }
            list.push(m.clone());
        }
        self.save_machines(&list)?;
        Ok(m)
    }

    pub fn remove(&self, id: Uuid) -> io::Result<()> {
        let mut list = self.machines();
        list.retain(|m| m.id != id);
        self.save_machines(&list)
    }

    fn save_machines(&self, list: &[StoredMachine]) -> io::Result<()> {
        self.keep_unreadable_copy()?;
        let file = MachinesFile { version: MACHINES_VERSION, machines: list.to_vec() };
        let text = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
        write_private(&self.machines_path(), text.as_bytes())
    }

    /// Before `machines.json` is replaced: when the file on disk is not a clean machine list
    /// (it does not parse, or an entry was skipped), copy it to `machines.json.bad-<seconds>`
    /// first. The new list is shorter than what was there, and the copy is the only way back.
    /// When the copy fails the write does not happen.
    fn keep_unreadable_copy(&self) -> io::Result<()> {
        let bytes = match std::fs::read(self.machines_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if parse_machines(&bytes, false).is_some_and(|f| f.skipped == 0) {
            return Ok(());
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        for n in 0u32.. {
            let suffix = if n == 0 { String::new() } else { format!("-{n}") };
            let bad = self.dir.join(format!("machines.json.bad-{stamp}{suffix}"));
            if bad.exists() {
                continue;
            }
            write_private(&bad, &bytes)?;
            eprintln!("forgetty-win: kept a copy of the unreadable machines.json as {}", bad.display());
            return Ok(());
        }
        unreachable!("the loop ends by return")
    }

    /// The persisted direct-address cache (empty when there is none).
    pub fn addr_cache(&self) -> AddrCache {
        let snap = std::fs::read_to_string(self.addr_cache_path())
            .ok()
            .and_then(|t| serde_json::from_str::<AddrCacheSnapshot>(&t).ok())
            .unwrap_or_default();
        AddrCache::from_snapshot(&snap)
    }

    pub fn save_addr_cache(&self, cache: &AddrCache) -> io::Result<()> {
        let text = serde_json::to_string_pretty(&cache.snapshot()).map_err(io::Error::other)?;
        write_private(&self.addr_cache_path(), text.as_bytes())
    }
}

/// What a `machines.json` held.
struct LoadedMachines {
    version: u32,
    machines: Vec<StoredMachine>,
    /// Entries that did not parse.
    skipped: usize,
}

/// Parse a `machines.json` entry by entry: one bad entry is skipped (and logged when `log`),
/// the others load. `None` when the file is not a machine list at all.
fn parse_machines(bytes: &[u8], log: bool) -> Option<LoadedMachines> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = v.as_object()?;
    let version = obj.get("version").and_then(serde_json::Value::as_u64).map_or(1, |n| n.min(u64::from(u32::MAX)) as u32);
    let entries = obj.get("machines")?.as_array()?;
    let mut machines = Vec::new();
    let mut skipped = 0;
    for (i, e) in entries.iter().enumerate() {
        match serde_json::from_value::<StoredMachine>(e.clone()) {
            Ok(m) => machines.push(m),
            Err(err) => {
                skipped += 1;
                if log {
                    eprintln!("forgetty-win: machines.json entry {i} skipped: {err}");
                }
            }
        }
    }
    Some(LoadedMachines { version, machines, skipped })
}

/// The relay every version 1 pairing payload carried (`forgetty_sync::qr::DEFAULT_RELAY`).
const DEFAULT_RELAY: &str = "https://euw1-1.relay.iroh.network/";

/// No relay, or only the default one.
fn only_default_relay(relays: &[String]) -> bool {
    relays.iter().all(|r| r.trim_end_matches('/') == DEFAULT_RELAY.trim_end_matches('/'))
}

/// An address that only some networks have: private, link-local, CGNAT, ULA, loopback.
fn is_private_addr(a: &SocketAddr) -> bool {
    match a.ip() {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_loopback() || v.is_link_local() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(v) => {
            let s = v.segments();
            v.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

// ---- pairing ---------------------------------------------------------------

/// What the user sees before confirming: parsed and sanitized, nothing paired yet.
#[derive(Clone, Debug)]
pub struct PairingOffer {
    pub qr: QrPayload,
    /// Machine name with control characters removed, at most 48 chars.
    pub machine: String,
    /// First 8 characters of the node id.
    pub node_short: String,
    /// The whole node id in groups of 8, to compare with what `forgetty pair` prints.
    pub node_grouped: String,
    /// Relay URLs as shown, joined with ", " (each at most 96 chars; empty
    /// means none).
    pub relay: String,
    /// Every relay URL of the payload, to store with the machine. Empty when the server uses no
    /// relay.
    pub relays: Vec<String>,
    /// The payload says the server uses no relay (a WireGuard-only server).
    pub relays_disabled: bool,
    /// The direct addresses of the payload (at most `MAX_PINNED`), to pin.
    pub direct: Vec<SocketAddr>,
}

/// Text from a QR payload is untrusted: no control or bidi characters, bounded length.
fn clean(s: &str, max: usize) -> String {
    forgetty_shell_core::notify::sanitize(s, max)
}

/// The first JSON object in `text` that is a pairing payload. Lines around it (the QR block,
/// "payload:", "expires in...") may hold braces of their own, so every `{` is tried.
fn find_payload(text: &str) -> Result<QrPayload, String> {
    let mut first_err: Option<String> = None;
    for (i, _) in text.match_indices('{') {
        let mut it = serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        let Some(Ok(v)) = it.next() else { continue };
        if !v.is_object() {
            continue;
        }
        let end = i + it.byte_offset();
        match QrPayload::parse(&text[i..end]) {
            Ok(q) => return Ok(q),
            Err(e) => {
                first_err.get_or_insert(e.to_string());
            }
        }
    }
    Err(first_err.unwrap_or_else(|| "No pairing payload found. Paste the text that `forgetty pair` prints.".into()))
}

/// `abcdefghij` in groups of 4 becomes `abcd efgh ij`.
pub fn group_id(id: &str, n: usize) -> String {
    let chars: Vec<char> = id.chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect();
    chars.chunks(n).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// Parse the text `forgetty pair` prints (the QR payload JSON, possibly with
/// other lines around it). Nothing is contacted.
pub fn parse_offer(text: &str) -> Result<PairingOffer, String> {
    if text.len() > MAX_OFFER_BYTES {
        return Err("The pasted text is too long for a pairing payload.".into());
    }
    let qr = find_payload(text)?;
    // Reject a node id or relay that cannot be dialed before showing a dialog.
    qr_to_addr(&qr).map_err(|e| e.to_string())?;
    let machine = clean(&qr.machine, 48);
    if machine.is_empty() {
        return Err("The payload has no machine name.".into());
    }
    let relays = if qr.relays_disabled { Vec::new() } else { qr.relay_urls() };
    let direct = usable_direct(qr.direct_addrs());
    Ok(PairingOffer {
        node_short: qr.node_id.chars().take(8).collect(),
        node_grouped: group_id(&qr.node_id, 8),
        relay: relays.iter().map(|r| clean(r, 96)).collect::<Vec<_>>().join(", "),
        relays,
        relays_disabled: qr.relays_disabled,
        direct,
        machine,
        qr,
    })
}

/// Direct addresses worth pinning: no wildcard, no multicast, no port 0, no duplicates, a
/// handful at most.
fn usable_direct(list: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for a in list {
        if a.port() == 0 || a.ip().is_unspecified() || a.ip().is_multicast() || out.contains(&a) {
            continue;
        }
        out.push(a);
        if out.len() == MAX_PINNED {
            break;
        }
    }
    out
}

/// What `pair` dials and stores for `offer`: the pinned addresses (the payload's direct
/// addresses plus `manual`, an address the user typed in or a test passes) and the address and
/// options to dial them with.
fn pair_plan(offer: &PairingOffer, manual: Option<SocketAddr>) -> Result<(forgetty_client::EndpointAddr, ClientOptions, Vec<SocketAddr>), ClientError> {
    let mut pinned = offer.direct.clone();
    if let Some(h) = manual {
        if !pinned.contains(&h) {
            pinned.insert(0, h);
        }
    }
    let addr = server_addr_direct(&offer.qr.node_id, &offer.relays, &pinned)?;
    let opts = ClientOptions { relays_disabled: offer.relays_disabled, pinned_addrs: pinned.clone(), ..Default::default() };
    Ok((addr, opts, pinned))
}

/// Pair with the server in `offer`, then save the machine. The direct addresses of the payload
/// and `manual` (an address typed in by the user; tests, LAN) are stored as the machine's
/// pinned addresses, together with `relays_disabled`.
pub async fn pair(
    store: &MachineStore,
    offer: &PairingOffer,
    device_name: &str,
    manual: Option<SocketAddr>,
) -> Result<StoredMachine, ClientError> {
    let identity = store.identity().map_err(|e| ClientError::Setup(format!("identity: {e}")))?;
    let (addr, opts, pinned) = pair_plan(offer, manual)?;
    pair_with_addr(addr, &offer.qr.secret, identity, device_name, ClientKind::Windows, &opts).await?;
    let m = StoredMachine {
        id: Uuid::new_v4(),
        name: offer.machine.clone(),
        node_id: offer.qr.node_id.clone(),
        relays: offer.relays.clone(),
        relays_disabled: offer.relays_disabled,
        pinned_addrs: pinned,
    };
    store.add(m).map_err(|e| ClientError::Setup(format!("machines.json: {e}")))
}

/// How long one dial attempt may take: the dial and the Hello/Welcome behind it.
///
/// - A machine with pinned addresses, or one that uses no relay: 3 s. The dial goes to a fixed
///   address, possibly through a tunnel that has just come up, and a 1 s limit would cut off a
///   handshake that is only slow.
/// - A machine reached before on a LAN address the client learned (no pinned list): 1 s. It
///   answers a handshake in one round trip, so a short limit makes the client send a fresh
///   first packet often, and a server that restarts is found within about a second.
/// - Otherwise the dial goes through a relay and gets the library default.
pub fn dial_timeout_for(cache: &AddrCache, m: &StoredMachine) -> std::time::Duration {
    if !m.pinned_addrs.is_empty() || m.relays_disabled || m.relays.is_empty() {
        return std::time::Duration::from_secs(3);
    }
    let learned = m.node_id.parse::<EndpointId>().ok().and_then(|id| cache.get(id, local_network())).is_some();
    if learned {
        std::time::Duration::from_secs(1)
    } else {
        ClientOptions::default().dial_timeout
    }
}

/// Address of a stored machine (node id, relays unless it uses none, pinned addresses), for
/// `Client::spawn`.
pub fn machine_addr(m: &StoredMachine) -> Result<forgetty_client::EndpointAddr, ClientError> {
    let relays: &[String] = if m.relays_disabled { &[] } else { &m.relays };
    server_addr_direct(&m.node_id, relays, &m.pinned_addrs)
}

/// Client options for a stored machine: the pinned addresses and `relays_disabled` as paired,
/// the shared address cache and the dial timeout for what is known about the machine.
pub fn machine_options(cache: &AddrCache, m: &StoredMachine) -> ClientOptions {
    ClientOptions {
        relays_disabled: m.relays_disabled,
        pinned_addrs: m.pinned_addrs.clone(),
        dial_timeout: dial_timeout_for(cache, m),
        addr_cache: cache.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer_json(node: &str, name: &str, relay: &str) -> String {
        serde_json::json!({"v": 2, "node_id": node, "machine": name, "relay": relay, "secret": "ABCDEF"}).to_string()
    }

    fn node() -> String {
        SecretKey::generate(&mut rand::rng()).public().to_string()
    }

    #[test]
    fn parse_offer_accepts_surrounding_text_and_sanitizes() {
        let n = node();
        let text = format!("Scan or paste:\n{}\nexpires in 5 min", offer_json(&n, "bu\u{1b}ild\u{7}-box\n", "https://relay.example.com/"));
        let o = parse_offer(&text).unwrap();
        assert_eq!(o.machine, "build-box");
        assert_eq!(o.node_short, n[..8]);
        assert!(o.node_grouped.replace(' ', "") == n && o.node_grouped.split(' ').all(|g| g.len() <= 8));
        assert_eq!(o.relay, "https://relay.example.com/");
    }

    #[test]
    fn parse_offer_takes_the_whole_pair_output() {
        let n = node();
        // An ASCII QR block (3-byte characters, as `qr_to_ascii` prints), a status line with
        // braces, the payload, then more lines.
        let block: String = (0..40).map(|_| "\u{2588}\u{2580}\u{2584} ".repeat(30) + "\n").collect();
        let text = format!(
            "{block}\nUse {{the camera}} or paste:\npayload: {}\nwaiting for {{pairing}} ...\nexpires in 5 min\n",
            offer_json(&n, "build-box", "https://relay.example.com/")
        );
        assert!(text.len() > 4096);
        let o = parse_offer(&text).unwrap();
        assert_eq!(o.machine, "build-box");
        assert_eq!(o.node_short, n[..8]);
    }

    #[test]
    fn parse_offer_rejects_garbage() {
        assert!(parse_offer("hello").is_err());
        assert!(parse_offer("{\"v\":3}").is_err());
        assert!(parse_offer(&offer_json("not-a-node-id", "x", "")).is_err());
        assert!(parse_offer(&offer_json(&node(), "x", "not a url")).is_err());
        assert!(parse_offer(&offer_json(&node(), "\u{7}\u{1b}", "")).is_err());
        assert!(parse_offer(&"x".repeat(5000)).is_err());
    }

    #[test]
    fn every_relay_is_kept() {
        let n = node();
        let j = serde_json::json!({"v": 2, "node_id": n, "machine": "m", "relay": "https://a.example.com/", "relays": ["https://a.example.com/", "https://b.example.com/"], "secret": "ABCDEF"});
        let o = parse_offer(&j.to_string()).unwrap();
        assert_eq!(o.relays, vec!["https://a.example.com/", "https://b.example.com/"]);
        assert_eq!(o.relay, "https://a.example.com/, https://b.example.com/");
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        let m = s.add(StoredMachine { id: Uuid::new_v4(), name: "m".into(), node_id: n.clone(), relays: o.relays.clone(), relays_disabled: false, pinned_addrs: Vec::new() }).unwrap();
        assert_eq!(s.machines()[0].relays, o.relays);
        let addr = machine_addr(&m).unwrap();
        assert_eq!(addr.relay_urls().count(), 2);
        // A file from W3 held one `relay` string.
        std::fs::write(
            s.machines_path(),
            serde_json::json!({"version": 1, "machines": [{"id": m.id, "name": "m", "node_id": n, "relay": "https://a.example.com/"}]}).to_string(),
        )
        .unwrap();
        assert_eq!(s.machines()[0].relays, vec!["https://a.example.com/"]);
    }

    #[test]
    fn long_names_are_cut() {
        let o = parse_offer(&offer_json(&node(), &"m".repeat(200), "")).unwrap();
        assert_eq!(o.machine.chars().count(), 48);
    }

    #[test]
    fn store_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path().join("forgetty"));
        let k1 = s.identity().unwrap();
        let k2 = s.identity().unwrap();
        assert_eq!(k1.public(), k2.public());
        let c1 = s.client_id().unwrap();
        assert_eq!(c1, s.client_id().unwrap());
        assert!(s.machines().is_empty());

        let m = StoredMachine { id: Uuid::new_v4(), name: "a".into(), node_id: node(), relays: Vec::new(), relays_disabled: false, pinned_addrs: Vec::new() };
        let saved = s.add(m.clone()).unwrap();
        assert_eq!(s.machines(), vec![saved.clone()]);
        // Same node id again: replaced, local id kept.
        let again = s.add(StoredMachine { id: Uuid::new_v4(), name: "renamed".into(), ..m.clone() }).unwrap();
        assert_eq!(again.id, saved.id);
        assert_eq!(s.machines().len(), 1);
        assert_eq!(s.machines()[0].name, "renamed");

        let cache = AddrCache::new();
        let id: forgetty_client::EndpointId = m.node_id.parse().unwrap();
        cache.insert(id, "127.0.0.1:47297".parse().unwrap(), local_network());
        s.save_addr_cache(&cache).unwrap();
        assert_eq!(s.addr_cache().get(id, local_network()), Some("127.0.0.1:47297".parse().unwrap()));

        for p in [s.identity_path(), s.client_id_path(), s.machines_path(), s.addr_cache_path()] {
            assert!(crate::acl::is_user_only(&p).unwrap(), "{p:?} {:?}", crate::acl::describe(&p));
        }
        s.remove(saved.id).unwrap();
        assert!(s.machines().is_empty());
    }

    #[test]
    fn a_corrupt_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        std::fs::write(s.machines_path(), "{nope").unwrap();
        assert!(s.machines().is_empty());
        assert!(s.identity().is_ok());
        std::fs::write(s.identity_path(), b"short").unwrap();
        assert!(s.identity().is_err());
    }

    #[test]
    fn two_first_runs_share_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let keys: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || MachineStore::open(p).identity().unwrap().to_bytes())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert!(keys.windows(2).all(|w| w[0] == w[1]), "every first run got the key that is on disk");
        let on_disk = std::fs::read(MachineStore::open(&path).identity_path()).unwrap();
        assert_eq!(on_disk, keys[0]);
    }

    fn wg_offer(n: &str) -> String {
        serde_json::json!({
            "v": 2, "node_id": n, "machine": "wg-box", "relay": "", "relays_disabled": true,
            "direct": ["10.230.1.14:47297", "not an address", "0.0.0.0:47297", "10.230.1.14:47297", "[fd00::1]:47297"],
            "secret": "ABCDEF"
        })
        .to_string()
    }

    #[test]
    fn a_wireguard_offer_pins_its_direct_addresses_and_has_no_relay() {
        let n = node();
        let o = parse_offer(&wg_offer(&n)).unwrap();
        assert!(o.relays_disabled && o.relays.is_empty());
        // The junk, the wildcard and the duplicate are gone.
        assert_eq!(o.direct, vec!["10.230.1.14:47297".parse::<SocketAddr>().unwrap(), "[fd00::1]:47297".parse().unwrap()]);

        let manual: SocketAddr = "10.230.1.99:47297".parse().unwrap();
        let (addr, opts, pinned) = pair_plan(&o, Some(manual)).unwrap();
        assert_eq!(pinned[0], manual);
        assert_eq!(pinned.len(), 3);
        assert_eq!(opts.pinned_addrs, pinned);
        assert!(opts.relays_disabled);
        assert_eq!(addr.relay_urls().count(), 0);
        assert_eq!(addr.ip_addrs().count(), 3);
        // The same address twice is kept once.
        assert_eq!(pair_plan(&o, Some(o.direct[0])).unwrap().2.len(), 2);

        let m = StoredMachine {
            id: Uuid::new_v4(),
            name: o.machine.clone(),
            node_id: n,
            relays: o.relays.clone(),
            relays_disabled: true,
            pinned_addrs: pinned.clone(),
        };
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        s.add(m.clone()).unwrap();
        let back = s.machines();
        assert_eq!(back, vec![m.clone()]);
        let text = std::fs::read_to_string(s.machines_path()).unwrap();
        assert!(text.contains("\"pinned_addrs\"") && text.contains("\"relays_disabled\": true") && text.contains("\"version\": 2"), "{text}");
        // What the client gets: the pinned addresses, no relay.
        let a = machine_addr(&back[0]).unwrap();
        assert_eq!(a.ip_addrs().count(), 3);
        assert_eq!(a.relay_urls().count(), 0);
        let cache = AddrCache::new();
        let o = machine_options(&cache, &back[0]);
        assert!(o.relays_disabled);
        assert_eq!(o.pinned_addrs, pinned);
        assert_eq!(o.dial_timeout, std::time::Duration::from_secs(3));
    }

    #[test]
    fn a_relay_machine_without_pinned_addresses_still_loads() {
        let n = node();
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        let id = Uuid::new_v4();
        std::fs::write(
            s.machines_path(),
            serde_json::json!({"version": 2, "machines": [{"id": id, "name": "m", "node_id": n, "relays": ["https://a.example.com/"]}]}).to_string(),
        )
        .unwrap();
        let m = &s.machines()[0];
        assert!(m.pinned_addrs.is_empty() && !m.relays_disabled);
        assert_eq!(machine_addr(m).unwrap().relay_urls().count(), 1);
    }

    fn v1_file(s: &MachineStore, machines: serde_json::Value) {
        std::fs::write(s.machines_path(), serde_json::json!({"version": 1, "machines": machines}).to_string()).unwrap();
    }

    fn bad_copies(s: &MachineStore) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(s.dir())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("machines.json.bad-")))
            .collect()
    }

    #[test]
    fn a_version_1_machine_with_the_default_relay_pins_its_private_cached_address_once() {
        let (default_relay, own_relay, no_cache) = (node(), node(), node());
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        // Every version 1 payload carried the default relay.
        v1_file(
            &s,
            serde_json::json!([
                {"id": a, "name": "lan", "node_id": default_relay, "relay": DEFAULT_RELAY},
                {"id": b, "name": "relayed", "node_id": own_relay, "relay": "https://a.example.com/"},
                {"id": c, "name": "plain", "node_id": no_cache, "relay": DEFAULT_RELAY},
            ]),
        );
        let cache = AddrCache::new();
        let (lan, wg, public, learned): (SocketAddr, SocketAddr, SocketAddr, SocketAddr) =
            ("192.168.1.20:47297".parse().unwrap(), "10.230.1.14:47297".parse().unwrap(), "203.0.113.7:4000".parse().unwrap(), "192.168.9.9:1".parse().unwrap());
        let (id_a, id_b): (EndpointId, EndpointId) = (default_relay.parse().unwrap(), own_relay.parse().unwrap());
        cache.insert(id_a, public, local_network());
        cache.add(id_a, wg, local_network());
        cache.add(id_a, lan, local_network());
        cache.insert(id_b, learned, local_network());
        s.save_addr_cache(&cache).unwrap();

        let list = s.machines();
        let mut pinned = list[0].pinned_addrs.clone();
        pinned.sort();
        let mut want = vec![lan, wg];
        want.sort();
        assert_eq!(pinned, want, "private addresses only: the public one was learned");
        assert!(!list[0].relays_disabled, "unknowable, so false");
        assert_eq!(list[0].relays, vec![DEFAULT_RELAY.to_string()]);
        assert!(list[1].pinned_addrs.is_empty(), "a relay of its own: what the cache holds was learned");
        assert!(list[2].pinned_addrs.is_empty(), "nothing cached");
        assert!(std::fs::read_to_string(s.machines_path()).unwrap().contains("\"version\": 2"));
        // The cache is untouched.
        let after = s.addr_cache();
        assert_eq!(after.get_everywhere(id_a).len(), 3);
        assert_eq!(after.get_everywhere(id_b), vec![learned]);

        // Once: a later load does not pin what the client learns afterwards.
        after.add(id_a, "172.16.0.5:1".parse().unwrap(), local_network());
        s.save_addr_cache(&after).unwrap();
        assert_eq!(s.machines()[0].pinned_addrs.len(), 2);

        // A downgrade writes a version 1 file again; the next upgrade migrates again.
        v1_file(&s, serde_json::json!([{"id": a, "name": "lan", "node_id": default_relay, "relay": DEFAULT_RELAY}]));
        assert_eq!(s.machines()[0].pinned_addrs.len(), 3, "the cache still held the address");
        assert!(bad_copies(&s).is_empty());
    }

    #[test]
    fn a_corrupt_entry_is_skipped_and_the_rest_load() {
        let (good, other) = (node(), node());
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        let text = serde_json::json!({"version": 2, "machines": [
            {"id": Uuid::new_v4(), "name": "good", "node_id": good},
            {"id": "not-a-uuid", "name": 7},
            {"id": Uuid::new_v4(), "name": "bad addr", "node_id": node(), "pinned_addrs": ["nope"]},
            {"id": Uuid::new_v4(), "name": "other", "node_id": other},
        ]})
        .to_string();
        std::fs::write(s.machines_path(), &text).unwrap();
        let names: Vec<_> = s.machines().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["good", "other"]);
        // Reading changes nothing on disk.
        assert_eq!(std::fs::read_to_string(s.machines_path()).unwrap(), text);
        assert!(bad_copies(&s).is_empty());
        // The first write keeps a copy of the file with the bad entries, then the file is clean.
        s.add(StoredMachine { id: Uuid::new_v4(), name: "new".into(), node_id: node(), relays: Vec::new(), relays_disabled: false, pinned_addrs: Vec::new() }).unwrap();
        let copies = bad_copies(&s);
        assert_eq!(copies.len(), 1);
        assert_eq!(std::fs::read_to_string(&copies[0]).unwrap(), text);
        assert!(crate::acl::is_user_only(&copies[0]).unwrap());
        assert_eq!(s.machines().len(), 3);
        // A clean file makes no more copies.
        s.add(StoredMachine { id: Uuid::new_v4(), name: "again".into(), node_id: node(), relays: Vec::new(), relays_disabled: false, pinned_addrs: Vec::new() }).unwrap();
        assert_eq!(bad_copies(&s).len(), 1);
    }

    #[test]
    fn a_file_that_does_not_parse_is_copied_before_it_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        for junk in ["{nope", "", "[1,2]", "{\"version\":2}", "{\"version\":2,\"machines\":[{\"x\":1},{\"y\":2}]}"] {
            for c in bad_copies(&s) {
                std::fs::remove_file(c).unwrap();
            }
            std::fs::write(s.machines_path(), junk).unwrap();
            assert!(s.machines().is_empty(), "{junk}");
            assert_eq!(std::fs::read_to_string(s.machines_path()).unwrap(), junk, "reading does not write");
            s.add(StoredMachine { id: Uuid::new_v4(), name: "m".into(), node_id: node(), relays: Vec::new(), relays_disabled: false, pinned_addrs: Vec::new() }).unwrap();
            let copies = bad_copies(&s);
            assert_eq!(copies.len(), 1, "{junk}");
            assert_eq!(std::fs::read_to_string(&copies[0]).unwrap(), junk);
            assert_eq!(s.machines().len(), 1);
        }
    }

    #[test]
    fn a_v1_file_with_a_bad_entry_is_copied_before_the_migration_rewrites_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = MachineStore::open(dir.path());
        let n = node();
        v1_file(&s, serde_json::json!([{"id": Uuid::new_v4(), "name": "ok", "node_id": n}, {"id": 5}]));
        let before = std::fs::read_to_string(s.machines_path()).unwrap();
        assert_eq!(s.machines().len(), 1);
        let copies = bad_copies(&s);
        assert_eq!(copies.len(), 1);
        assert_eq!(std::fs::read_to_string(&copies[0]).unwrap(), before);
    }

    #[test]
    fn dial_timeouts_follow_what_is_known_about_the_machine() {
        use std::time::Duration;
        let n = node();
        let id: EndpointId = n.parse().unwrap();
        let cache = AddrCache::new();
        let mut m = StoredMachine { id: Uuid::new_v4(), name: "m".into(), node_id: n, relays: vec!["https://a.example.com/".into()], relays_disabled: false, pinned_addrs: Vec::new() };
        assert_eq!(dial_timeout_for(&cache, &m), ClientOptions::default().dial_timeout, "relay only");
        cache.insert(id, "192.168.1.5:47297".parse().unwrap(), local_network());
        assert_eq!(dial_timeout_for(&cache, &m), Duration::from_secs(1), "a learned LAN address and no pinned list");
        m.pinned_addrs = vec!["10.230.1.14:47297".parse().unwrap()];
        assert_eq!(dial_timeout_for(&cache, &m), Duration::from_secs(3), "pinned");
        m.pinned_addrs.clear();
        m.relays_disabled = true;
        assert_eq!(dial_timeout_for(&AddrCache::new(), &m), Duration::from_secs(3), "relayless");
    }

    #[test]
    fn machine_names_lose_bidi_and_control_characters() {
        let o = parse_offer(&offer_json(&node(), "ok\u{202e}\u{7}na\u{200b}me\u{2066}", "")).unwrap();
        assert_eq!(o.machine, "okname");
    }
}
