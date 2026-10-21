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
use forgetty_client::{pair_with_addr, qr_to_addr, AddrCache, AddrCacheSnapshot, ClientError, ClientOptions, SecretKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::acl::{self, write_private};

const MAX_MACHINES: usize = 32;
const MAX_OFFER_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMachine {
    pub id: Uuid,
    pub name: String,
    pub node_id: String,
    /// Every relay of the pairing payload, in order. Older files held one
    /// `relay` string; it reads as a one entry list.
    #[serde(default, alias = "relay", deserialize_with = "relays_de")]
    pub relays: Vec<String>,
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

    /// Paired machines. A missing or unreadable file is an empty list.
    pub fn machines(&self) -> Vec<StoredMachine> {
        std::fs::read_to_string(self.machines_path())
            .ok()
            .and_then(|t| serde_json::from_str::<MachinesFile>(&t).ok())
            .map(|f| f.machines)
            .unwrap_or_default()
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
        let file = MachinesFile { version: 1, machines: list.to_vec() };
        let text = serde_json::to_string_pretty(&file).map_err(io::Error::other)?;
        write_private(&self.machines_path(), text.as_bytes())
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

// ---- pairing ---------------------------------------------------------------

/// What the user sees before confirming: parsed and sanitized, nothing paired yet.
#[derive(Clone, Debug)]
pub struct PairingOffer {
    pub qr: QrPayload,
    /// Machine name with control characters removed, at most 48 chars.
    pub machine: String,
    /// First 8 characters of the node id.
    pub node_short: String,
    /// Relay URLs as shown, joined with ", " (each at most 96 chars; empty
    /// means none).
    pub relay: String,
    /// Every relay URL of the payload, to store with the machine.
    pub relays: Vec<String>,
}

/// Text from a QR payload is untrusted: no control or bidi characters, bounded length.
fn clean(s: &str, max: usize) -> String {
    forgetty_shell_core::notify::sanitize(s, max)
}

/// Parse the text `forgetty pair` prints (the QR payload JSON, possibly with
/// other lines around it). Nothing is contacted.
pub fn parse_offer(text: &str) -> Result<PairingOffer, String> {
    if text.len() > MAX_OFFER_BYTES {
        return Err("The pasted text is too long for a pairing payload.".into());
    }
    let (start, end) = match (text.find('{'), text.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err("No pairing payload found. Paste the text that `forgetty pair` prints.".into()),
    };
    let qr = QrPayload::parse(&text[start..=end]).map_err(|e| e.to_string())?;
    // Reject a node id or relay that cannot be dialed before showing a dialog.
    qr_to_addr(&qr).map_err(|e| e.to_string())?;
    let machine = clean(&qr.machine, 48);
    if machine.is_empty() {
        return Err("The payload has no machine name.".into());
    }
    let relays = qr.relay_urls();
    Ok(PairingOffer {
        node_short: qr.node_id.chars().take(8).collect(),
        relay: relays.iter().map(|r| clean(r, 96)).collect::<Vec<_>>().join(", "),
        relays,
        machine,
        qr,
    })
}

/// Pair with the server in `offer`, then save the machine. `direct_hint` is an
/// extra direct address to try (tests, LAN); it also goes into the address
/// cache so later connects dial it first.
pub async fn pair(
    store: &MachineStore,
    offer: &PairingOffer,
    device_name: &str,
    direct_hint: Option<SocketAddr>,
) -> Result<StoredMachine, ClientError> {
    let identity = store.identity().map_err(|e| ClientError::Setup(format!("identity: {e}")))?;
    let mut addr = qr_to_addr(&offer.qr)?;
    if let Some(h) = direct_hint {
        addr = addr.with_ip_addr(h);
    }
    pair_with_addr(addr.clone(), &offer.qr.secret, identity, device_name, ClientKind::Windows, &ClientOptions::default())
        .await?;
    if let Some(h) = direct_hint {
        let cache = store.addr_cache();
        cache.insert(addr.id, h);
        store.save_addr_cache(&cache).map_err(|e| ClientError::Setup(format!("address cache: {e}")))?;
    }
    let m = StoredMachine {
        id: Uuid::new_v4(),
        name: offer.machine.clone(),
        node_id: offer.qr.node_id.clone(),
        relays: offer.relays.clone(),
    };
    store.add(m).map_err(|e| ClientError::Setup(format!("machines.json: {e}")))
}

/// How long one dial attempt may take. A machine we reached directly before
/// (the address cache has its address) answers a handshake in one round trip,
/// so a short timeout makes the client send a fresh first packet often, and a
/// server that restarts is found within about a second. Without a direct
/// address the dial goes through a relay and gets the library default.
pub fn dial_timeout_for(cache: &AddrCache, m: &StoredMachine) -> std::time::Duration {
    let direct = m.node_id.parse::<forgetty_client::EndpointId>().ok().and_then(|id| cache.get(id)).is_some();
    if direct {
        std::time::Duration::from_secs(1)
    } else {
        ClientOptions::default().dial_timeout
    }
}

/// Address of a stored machine (node id plus relay), for `Client::spawn`.
pub fn machine_addr(m: &StoredMachine) -> Result<forgetty_client::EndpointAddr, ClientError> {
    qr_to_addr(&QrPayload {
        v: QrPayload::VERSION,
        node_id: m.node_id.clone(),
        machine: m.name.clone(),
        relay: m.relays.first().cloned().unwrap_or_default(),
        relays: m.relays.clone(),
        secret: String::new(),
    })
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
        assert_eq!(o.relay, "https://relay.example.com/");
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
        let m = s.add(StoredMachine { id: Uuid::new_v4(), name: "m".into(), node_id: n.clone(), relays: o.relays.clone() }).unwrap();
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

        let m = StoredMachine { id: Uuid::new_v4(), name: "a".into(), node_id: node(), relays: Vec::new() };
        let saved = s.add(m.clone()).unwrap();
        assert_eq!(s.machines(), vec![saved.clone()]);
        // Same node id again: replaced, local id kept.
        let again = s.add(StoredMachine { id: Uuid::new_v4(), name: "renamed".into(), ..m.clone() }).unwrap();
        assert_eq!(again.id, saved.id);
        assert_eq!(s.machines().len(), 1);
        assert_eq!(s.machines()[0].name, "renamed");

        let cache = AddrCache::new();
        let id: forgetty_client::EndpointId = m.node_id.parse().unwrap();
        cache.insert(id, "127.0.0.1:47297".parse().unwrap());
        s.save_addr_cache(&cache).unwrap();
        assert_eq!(s.addr_cache().get(id), Some("127.0.0.1:47297".parse().unwrap()));

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

    #[test]
    fn machine_names_lose_bidi_and_control_characters() {
        let o = parse_offer(&offer_json(&node(), "ok\u{202e}\u{7}na\u{200b}me\u{2066}", "")).unwrap();
        assert_eq!(o.machine, "okname");
    }
}
