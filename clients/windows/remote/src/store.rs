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

use crate::acl::write_private;

const MAX_MACHINES: usize = 32;
const MAX_OFFER_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMachine {
    pub id: Uuid,
    pub name: String,
    pub node_id: String,
    #[serde(default)]
    pub relay: String,
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

    /// Load the device key, or create it. The file is written user-only.
    pub fn identity(&self) -> io::Result<SecretKey> {
        let path = self.identity_path();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "identity.key is not 32 bytes"))?;
                Ok(SecretKey::from_bytes(&arr))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key = SecretKey::generate(&mut rand::rng());
                write_private(&path, &key.to_bytes())?;
                Ok(key)
            }
            Err(e) => Err(e),
        }
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
    /// Relay URL as shown (at most 96 chars; empty means none).
    pub relay: String,
}

fn clean(s: &str, max: usize) -> String {
    let text: String = s.chars().filter(|c| !c.is_control()).collect();
    let text = text.trim();
    if text.chars().count() > max {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}...")
    } else {
        text.to_string()
    }
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
    Ok(PairingOffer {
        node_short: qr.node_id.chars().take(8).collect(),
        relay: clean(&qr.relay, 96),
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
        relay: offer.qr.relay.clone(),
    };
    store.add(m).map_err(|e| ClientError::Setup(format!("machines.json: {e}")))
}

/// Address of a stored machine (node id plus relay), for `Client::spawn`.
pub fn machine_addr(m: &StoredMachine) -> Result<forgetty_client::EndpointAddr, ClientError> {
    qr_to_addr(&QrPayload {
        v: QrPayload::VERSION,
        node_id: m.node_id.clone(),
        machine: m.name.clone(),
        relay: m.relay.clone(),
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
    fn long_names_are_cut() {
        let o = parse_offer(&offer_json(&node(), &"m".repeat(200), "")).unwrap();
        assert_eq!(o.machine.chars().count(), 48 + 3);
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

        let m = StoredMachine { id: Uuid::new_v4(), name: "a".into(), node_id: node(), relay: String::new() };
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
            assert!(crate::acl::is_user_only(&p).unwrap(), "{p:?}");
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
}
