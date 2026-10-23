//! Cache of the direct addresses a server was reached at, per server node id.
//!
//! The server binds a fixed UDP port (PROTOCOL_V2 section 1), so after a
//! server restart the cached addresses are still right. The client dials them
//! first and lets the relay in the server address run in parallel. The app
//! persists [`AddrCache::snapshot`] and restores it with
//! [`AddrCache::from_snapshot`].
//!
//! A server is known by more than one address (IPv4 and IPv6, Wi-Fi and
//! mobile). The cache keeps all of them, newest use first, and remembers which
//! one the last connection really used.
//!
//! An address that only means something on one network (private, ULA, CGNAT,
//! link-local: `192.168.1.10` is a different machine on the next Wi-Fi) is
//! stored with the fingerprint of the network it was learned on
//! ([`local_network`]) and dialed only there. A global address, or a loopback
//! one, is stored without a fingerprint and is valid everywhere.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Addresses kept per server. The oldest unused one goes first.
pub const MAX_ADDRS_PER_SERVER: usize = 8;

/// One cached address and the network it is valid on (`None`: every network).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    addr: SocketAddr,
    net: Option<u64>,
}

/// Addresses of one server, newest use first: `slots[0]` is the one the last
/// connection used.
type Entry = Vec<Slot>;

/// Shared, cloneable in-memory cache. Clones see the same entries.
#[derive(Debug, Clone, Default)]
pub struct AddrCache(Arc<Mutex<HashMap<EndpointId, Entry>>>);

/// An address of a persisted entry, with the network it was learned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedOther {
    /// The address.
    pub addr: SocketAddr,
    /// Fingerprint of the network it is valid on. `None` for a global or
    /// loopback address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<u64>,
}

/// One persisted entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAddr {
    /// Server iroh endpoint id, in its string form.
    pub node_id: String,
    /// The address the last connection used.
    pub addr: SocketAddr,
    /// The network `addr` is valid on (see [`CachedOther::net`]). A snapshot
    /// of an older version has none: a private address in it is dropped on
    /// load, because nobody knows which network it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<u64>,
    /// The other addresses the server is known by, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<CachedOther>,
}

impl CachedAddr {
    /// An entry with one address that is valid on every network.
    pub fn global(node_id: impl Into<String>, addr: SocketAddr) -> Self {
        CachedAddr { node_id: node_id.into(), addr, net: None, others: Vec::new() }
    }
}

/// Serializable form of the cache. Store it with any serde format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrCacheSnapshot {
    /// Entries, sorted by node id.
    pub entries: Vec<CachedAddr>,
}

impl AddrCacheSnapshot {
    /// Drop the entries of servers that are not in `paired`.
    pub fn retain_servers(&mut self, paired: &[EndpointId]) {
        let keep: Vec<String> = paired.iter().map(ToString::to_string).collect();
        self.entries.retain(|e| keep.contains(&e.node_id));
    }
}

/// Is `addr` the same on every network? A global address and a loopback one
/// are. Private, ULA, link-local, CGNAT and other special ranges are not.
/// Documentation ranges count as global: no network owns them.
pub fn network_independent(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(a) => {
            let o = a.octets();
            if a.is_loopback() {
                return true;
            }
            !(a.is_unspecified()
                || a.is_private()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_multicast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15
                || o[0] == 0
                || o[0] >= 240)
        }
        IpAddr::V6(a) => {
            if a.is_loopback() {
                return true;
            }
            let s = a.segments();
            if let Some(v4) = a.to_ipv4_mapped() {
                return network_independent(&SocketAddr::new(IpAddr::V4(v4), addr.port()));
            }
            !(a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (s[0] & 0xffc0) == 0xfec0) // fec0::/10 site-local
        }
    }
}

/// The network `addr` is valid on when learned on network `net`.
fn scope(addr: &SocketAddr, net: u64) -> Option<u64> {
    if network_independent(addr) {
        None
    } else {
        Some(net)
    }
}

/// A fingerprint of the network this device is on now: the local addresses
/// the system picks for a route to the internet over IPv4 and over IPv6 (the
/// IPv6 /64 prefix only: privacy addresses rotate). No packet is sent. A
/// device with no network gives the same number every time. Stable across
/// runs and versions, so the apps can persist what it scopes.
///
/// Two networks that hand out the same local address (`192.168.1.5` at home
/// and at the office) share a fingerprint. The identity check of the dial is
/// what catches a private address that now belongs to someone else.
pub fn local_network() -> u64 {
    let local = |bind: &str, to: &str| -> Option<IpAddr> {
        let sock = std::net::UdpSocket::bind(bind).ok()?;
        sock.connect(to).ok()?;
        sock.local_addr().ok().map(|a| a.ip())
    };
    let v4 = local("0.0.0.0:0", "192.0.2.1:9");
    let v6 = local("[::]:0", "[2001:db8::1]:9");
    // FNV-1a: the std hasher is not promised to stay the same.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    match v4 {
        Some(IpAddr::V4(a)) => {
            eat(&[4]);
            eat(&a.octets());
        }
        _ => eat(&[0]),
    }
    match v6 {
        Some(IpAddr::V6(a)) => {
            eat(&[6]);
            eat(&a.octets()[..8]);
        }
        _ => eat(&[0]),
    }
    h
}

impl AddrCache {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore from a snapshot. Entries with an unparsable node id are
    /// skipped, and so is a private address with no network to belong to.
    pub fn from_snapshot(snapshot: &AddrCacheSnapshot) -> Self {
        let cache = Self::new();
        for e in &snapshot.entries {
            let Ok(id) = e.node_id.parse::<EndpointId>() else { continue };
            let mut slots: Vec<Slot> = Vec::new();
            let all = std::iter::once(CachedOther { addr: e.addr, net: e.net }).chain(e.others.iter().copied());
            for o in all {
                let net = if network_independent(&o.addr) { None } else { o.net };
                if !network_independent(&o.addr) && net.is_none() {
                    continue;
                }
                let slot = Slot { addr: o.addr, net };
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
            slots.truncate(MAX_ADDRS_PER_SERVER);
            if !slots.is_empty() {
                cache.lock().insert(id, slots);
            }
        }
        cache
    }

    /// The address the last connection used, if it is valid on network `net`.
    pub fn get(&self, id: EndpointId, net: u64) -> Option<SocketAddr> {
        let map = self.lock();
        let first = map.get(&id)?.first()?;
        valid_on(first, net).then_some(first.addr)
    }

    /// What a dial on network `net` uses: the address the last connection
    /// used (if it is valid here), then this network's addresses, then the
    /// global ones.
    pub fn get_all(&self, id: EndpointId, net: u64) -> Vec<SocketAddr> {
        let map = self.lock();
        let Some(v) = map.get(&id) else { return Vec::new() };
        let mut out: Vec<SocketAddr> = Vec::new();
        let mut push = |a: SocketAddr| {
            if !out.contains(&a) {
                out.push(a);
            }
        };
        if let Some(first) = v.first().filter(|s| valid_on(s, net)) {
            push(first.addr);
        }
        for s in v.iter().filter(|s| s.net == Some(net)) {
            push(s.addr);
        }
        for s in v.iter().filter(|s| s.net.is_none()) {
            push(s.addr);
        }
        out
    }

    /// Every address the server is known by, on any network, newest first.
    pub fn get_everywhere(&self, id: EndpointId) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for s in self.lock().get(&id).into_iter().flatten() {
            if !out.contains(&s.addr) {
                out.push(s.addr);
            }
        }
        out
    }

    /// Record the address a connection really used, learned on network `net`:
    /// it goes first.
    pub fn insert(&self, id: EndpointId, addr: SocketAddr, net: u64) {
        let slot = Slot { addr, net: scope(&addr, net) };
        let mut map = self.lock();
        let v = map.entry(id).or_default();
        v.retain(|s| *s != slot);
        v.insert(0, slot);
        v.truncate(MAX_ADDRS_PER_SERVER);
    }

    /// Record an address the server is known by that no connection used yet
    /// (or not now). It does not change which address is first. A known one
    /// stays where it is.
    pub fn add(&self, id: EndpointId, addr: SocketAddr, net: u64) {
        let slot = Slot { addr, net: scope(&addr, net) };
        let mut map = self.lock();
        let v = map.entry(id).or_default();
        if v.contains(&slot) {
            return;
        }
        if v.len() >= MAX_ADDRS_PER_SERVER {
            v.pop();
        }
        v.push(slot);
    }

    /// A dial on network `net` found these addresses to belong to someone
    /// else (or to nobody): forget them. A private address is forgotten for
    /// this network only, a global one everywhere. Returns how many went.
    pub fn evict(&self, id: EndpointId, addrs: &[SocketAddr], net: u64) -> usize {
        let mut map = self.lock();
        let Some(v) = map.get_mut(&id) else { return 0 };
        let before = v.len();
        v.retain(|s| !(addrs.contains(&s.addr) && valid_on(s, net)));
        let gone = before - v.len();
        if v.is_empty() {
            map.remove(&id);
        }
        gone
    }

    /// Forget a server.
    pub fn forget(&self, id: EndpointId) {
        self.lock().remove(&id);
    }

    /// Keep only the servers in `paired`: a server that was unpaired leaves
    /// nothing behind. Returns how many servers went.
    pub fn retain_servers(&self, paired: &[EndpointId]) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|id, _| paired.contains(id));
        before - map.len()
    }

    /// Copy for persistence.
    pub fn snapshot(&self) -> AddrCacheSnapshot {
        let mut entries: Vec<CachedAddr> = self
            .lock()
            .iter()
            .filter_map(|(id, v)| {
                let (first, rest) = v.split_first()?;
                Some(CachedAddr {
                    node_id: id.to_string(),
                    addr: first.addr,
                    net: first.net,
                    others: rest.iter().map(|s| CachedOther { addr: s.addr, net: s.net }).collect(),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        AddrCacheSnapshot { entries }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn valid_on(s: &Slot, net: u64) -> bool {
    s.net.is_none_or(|n| n == net)
}

/// The addresses of `all` that `has_route` accepts, in the same order. An
/// address the local network has no route to (an IPv6 address on a device with
/// no IPv6) would cost a timeout, so it is left out of the dial. It stays in
/// the cache for the next network.
pub fn usable(all: &[SocketAddr], has_route: impl Fn(&SocketAddr) -> bool) -> Vec<SocketAddr> {
    all.iter().filter(|a| has_route(a)).copied().collect()
}

/// Does the system have a route to `addr`? A UDP `connect` picks the route and
/// the source address and sends nothing, so it fails at once with
/// `ENETUNREACH` (or `EHOSTUNREACH`) when there is none.
pub fn has_route(addr: &SocketAddr) -> bool {
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    std::net::UdpSocket::bind(bind).and_then(|s| s.connect(addr)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: u64 = 1;
    const OFFICE: u64 = 2;

    fn id() -> EndpointId {
        iroh::SecretKey::generate(&mut rand::rng()).public()
    }

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_ranges_are_scoped_and_global_ones_are_not() {
        for s in [
            "10.0.0.1:1", "172.16.5.5:1", "192.168.1.10:1", "100.64.3.4:1", "169.254.1.1:1",
            "[fd00::1]:1", "[fe80::1]:1", "[::ffff:192.168.1.1]:1",
        ] {
            assert!(!network_independent(&a(s)), "{s} is not global");
        }
        for s in ["8.8.8.8:1", "203.0.113.7:1", "[2607:5300:203:814e::]:1", "127.0.0.1:1", "[::1]:1"] {
            assert!(network_independent(&a(s)), "{s} is the same everywhere");
        }
    }

    #[test]
    fn a_private_address_is_dialed_only_on_the_network_it_was_learned_on() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.168.1.10:47297"), HOME);
        c.add(n, a("203.0.113.7:47297"), HOME);
        assert_eq!(c.get_all(n, HOME), vec![a("192.168.1.10:47297"), a("203.0.113.7:47297")]);
        // At the office the home address is not a candidate, the global one is.
        assert_eq!(c.get_all(n, OFFICE), vec![a("203.0.113.7:47297")]);
        assert_eq!(c.get(n, OFFICE), None);
        // Back home it is again.
        assert_eq!(c.get(n, HOME), Some(a("192.168.1.10:47297")));
    }

    #[test]
    fn the_dial_set_is_last_used_then_this_network_then_globals() {
        let (c, n) = (AddrCache::new(), id());
        c.add(n, a("203.0.113.7:1"), HOME);
        c.add(n, a("192.168.1.10:1"), HOME);
        c.add(n, a("192.168.1.11:1"), HOME);
        c.insert(n, a("[2001:db8::7]:1"), HOME);
        assert_eq!(
            c.get_all(n, HOME),
            vec![a("[2001:db8::7]:1"), a("192.168.1.10:1"), a("192.168.1.11:1"), a("203.0.113.7:1")]
        );
        // A last-used private address of another network is not first here.
        c.insert(n, a("192.168.1.99:1"), OFFICE);
        assert_eq!(c.get_all(n, HOME)[0], a("192.168.1.10:1"));
        assert_eq!(c.get_all(n, OFFICE)[0], a("192.168.1.99:1"));
    }

    #[test]
    fn the_same_private_address_on_two_networks_is_two_entries() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.168.1.10:1"), HOME);
        c.insert(n, a("192.168.1.10:1"), OFFICE);
        assert_eq!(c.get_everywhere(n), vec![a("192.168.1.10:1")]);
        assert_eq!(c.evict(n, &[a("192.168.1.10:1")], HOME), 1);
        // Evicted at home only.
        assert!(c.get_all(n, HOME).is_empty());
        assert_eq!(c.get_all(n, OFFICE), vec![a("192.168.1.10:1")]);
    }

    #[test]
    fn evicting_a_global_address_removes_it_everywhere_and_an_empty_entry_goes() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("203.0.113.7:1"), HOME);
        assert_eq!(c.evict(n, &[a("203.0.113.7:1")], OFFICE), 1);
        assert!(c.get_all(n, HOME).is_empty());
        assert!(c.snapshot().entries.is_empty());
    }

    #[test]
    fn the_used_address_goes_first_and_add_keeps_it() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.0.2.1:1"), HOME);
        c.add(n, a("192.0.2.2:1"), HOME);
        c.add(n, a("192.0.2.1:1"), HOME);
        assert_eq!(c.get_all(n, HOME), vec![a("192.0.2.1:1"), a("192.0.2.2:1")]);
        c.insert(n, a("192.0.2.2:1"), HOME);
        assert_eq!(c.get(n, HOME), Some(a("192.0.2.2:1")));
    }

    #[test]
    fn the_cache_is_bounded() {
        let (c, n) = (AddrCache::new(), id());
        for i in 0..20u8 {
            c.insert(n, a(&format!("10.0.0.{i}:1")), HOME);
        }
        let all = c.get_all(n, HOME);
        assert_eq!(all.len(), MAX_ADDRS_PER_SERVER);
        assert_eq!(all[0], a("10.0.0.19:1"));
    }

    #[test]
    fn a_snapshot_round_trips_with_its_networks() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.168.1.10:1"), HOME);
        c.insert(n, a("[2001:db8::1]:1"), HOME);
        c.add(n, a("10.1.1.1:1"), OFFICE);
        let json = serde_json::to_string(&c.snapshot()).unwrap();
        let back = AddrCache::from_snapshot(&serde_json::from_str(&json).unwrap());
        assert_eq!(back.get_all(n, HOME), c.get_all(n, HOME));
        assert_eq!(back.get_all(n, OFFICE), c.get_all(n, OFFICE));
        assert_eq!(back.get_all(n, OFFICE), vec![a("[2001:db8::1]:1"), a("10.1.1.1:1")]);
    }

    #[test]
    fn an_old_snapshot_keeps_its_global_address_and_drops_the_private_one() {
        let n = id();
        let old = format!(r#"{{"entries":[{{"node_id":"{n}","addr":"192.168.1.9:7"}}]}}"#);
        let snap: AddrCacheSnapshot = serde_json::from_str(&old).unwrap();
        assert!(AddrCache::from_snapshot(&snap).get_all(n, HOME).is_empty());
        let old = format!(r#"{{"entries":[{{"node_id":"{n}","addr":"203.0.113.9:7"}}]}}"#);
        let snap: AddrCacheSnapshot = serde_json::from_str(&old).unwrap();
        assert_eq!(AddrCache::from_snapshot(&snap).get_all(n, HOME), vec![a("203.0.113.9:7")]);
    }

    #[test]
    fn servers_that_are_not_paired_any_more_are_pruned() {
        let (c, kept, gone) = (AddrCache::new(), id(), id());
        c.insert(kept, a("203.0.113.1:1"), HOME);
        c.insert(gone, a("203.0.113.2:1"), HOME);
        let mut snap = c.snapshot();
        snap.retain_servers(&[kept]);
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(c.retain_servers(&[kept]), 1);
        assert!(c.get_all(gone, HOME).is_empty());
        assert_eq!(c.get_all(kept, HOME).len(), 1);
        c.forget(kept);
        assert!(c.snapshot().entries.is_empty());
    }

    #[test]
    fn an_address_without_a_route_is_left_out_of_the_dial() {
        let all = [a("[2001:db8::1]:1"), a("192.0.2.1:1")];
        let v4_only = usable(&all, |x| x.is_ipv4());
        assert_eq!(v4_only, vec![a("192.0.2.1:1")]);
        assert!(usable(&all, |_| false).is_empty());
        assert_eq!(usable(&all, |_| true), all.to_vec());
    }

    #[test]
    fn loopback_has_a_route() {
        assert!(has_route(&a("127.0.0.1:9")));
    }

    #[test]
    fn the_network_fingerprint_is_stable() {
        assert_eq!(local_network(), local_network());
    }
}
