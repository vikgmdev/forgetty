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
//! one the last connection really used. The client dials the ones the local
//! network has a route to, all in one dial, and keeps what it cannot use for
//! the next network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Addresses kept per server. The oldest unused one goes first.
pub const MAX_ADDRS_PER_SERVER: usize = 8;

/// Addresses of one server, newest use first: `addrs[0]` is the one the last
/// connection used.
type Entry = Vec<SocketAddr>;

/// Shared, cloneable in-memory cache. Clones see the same entries.
#[derive(Debug, Clone, Default)]
pub struct AddrCache(Arc<Mutex<HashMap<EndpointId, Entry>>>);

/// One persisted entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAddr {
    /// Server iroh endpoint id, in its string form.
    pub node_id: String,
    /// The address the last connection used.
    pub addr: SocketAddr,
    /// The other addresses the server is known by, newest first. Absent in a
    /// snapshot of an older version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<SocketAddr>,
}

/// Serializable form of the cache. Store it with any serde format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrCacheSnapshot {
    /// Entries, sorted by node id.
    pub entries: Vec<CachedAddr>,
}

impl AddrCache {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore from a snapshot. Entries with an unparsable node id are skipped.
    pub fn from_snapshot(snapshot: &AddrCacheSnapshot) -> Self {
        let cache = Self::new();
        for e in &snapshot.entries {
            if let Ok(id) = e.node_id.parse::<EndpointId>() {
                // Oldest first, so the used address ends up in front.
                for a in e.others.iter().rev() {
                    cache.add(id, *a);
                }
                cache.insert(id, e.addr);
            }
        }
        cache
    }

    /// The address the last connection used.
    pub fn get(&self, id: EndpointId) -> Option<SocketAddr> {
        self.lock().get(&id).and_then(|v| v.first().copied())
    }

    /// Every address the server is known by, the last used one first.
    pub fn get_all(&self, id: EndpointId) -> Vec<SocketAddr> {
        self.lock().get(&id).cloned().unwrap_or_default()
    }

    /// Record the address a connection really used: it goes first.
    pub fn insert(&self, id: EndpointId, addr: SocketAddr) {
        let mut map = self.lock();
        let v = map.entry(id).or_default();
        v.retain(|a| *a != addr);
        v.insert(0, addr);
        v.truncate(MAX_ADDRS_PER_SERVER);
    }

    /// Record an address the server is known by that no connection used yet
    /// (or not now). It does not change which address is first. A known one
    /// stays where it is.
    pub fn add(&self, id: EndpointId, addr: SocketAddr) {
        let mut map = self.lock();
        let v = map.entry(id).or_default();
        if v.contains(&addr) {
            return;
        }
        if v.len() >= MAX_ADDRS_PER_SERVER {
            v.pop();
        }
        v.push(addr);
    }

    /// Copy for persistence.
    pub fn snapshot(&self) -> AddrCacheSnapshot {
        let mut entries: Vec<CachedAddr> = self
            .lock()
            .iter()
            .filter_map(|(id, v)| {
                let (first, rest) = v.split_first()?;
                Some(CachedAddr { node_id: id.to_string(), addr: *first, others: rest.to_vec() })
            })
            .collect();
        entries.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        AddrCacheSnapshot { entries }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
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

    fn id() -> EndpointId {
        iroh::SecretKey::generate(&mut rand::rng()).public()
    }

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_used_address_goes_first_and_the_others_are_kept() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.0.2.1:47297"));
        c.insert(n, a("[2001:db8::1]:47297"));
        assert_eq!(c.get(n), Some(a("[2001:db8::1]:47297")));
        assert_eq!(c.get_all(n), vec![a("[2001:db8::1]:47297"), a("192.0.2.1:47297")]);
        // Using the older one again moves it up and keeps the other.
        c.insert(n, a("192.0.2.1:47297"));
        assert_eq!(c.get_all(n), vec![a("192.0.2.1:47297"), a("[2001:db8::1]:47297")]);
    }

    #[test]
    fn add_does_not_change_the_used_address() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.0.2.1:1"));
        c.add(n, a("192.0.2.2:1"));
        c.add(n, a("192.0.2.1:1"));
        assert_eq!(c.get_all(n), vec![a("192.0.2.1:1"), a("192.0.2.2:1")]);
    }

    #[test]
    fn the_cache_is_bounded() {
        let (c, n) = (AddrCache::new(), id());
        for i in 0..20u8 {
            c.insert(n, a(&format!("192.0.2.{i}:1")));
        }
        let all = c.get_all(n);
        assert_eq!(all.len(), MAX_ADDRS_PER_SERVER);
        assert_eq!(all[0], a("192.0.2.19:1"));
    }

    #[test]
    fn a_snapshot_round_trips_and_an_old_one_still_loads() {
        let (c, n) = (AddrCache::new(), id());
        c.insert(n, a("192.0.2.1:1"));
        c.insert(n, a("[2001:db8::1]:1"));
        let back = AddrCache::from_snapshot(&c.snapshot());
        assert_eq!(back.get_all(n), c.get_all(n));
        // A snapshot written before the list existed has no `others`.
        let old = format!(r#"{{"entries":[{{"node_id":"{n}","addr":"192.0.2.9:7"}}]}}"#);
        let snap: AddrCacheSnapshot = serde_json::from_str(&old).unwrap();
        assert_eq!(AddrCache::from_snapshot(&snap).get_all(n), vec![a("192.0.2.9:7")]);
    }

    #[test]
    fn an_address_without_a_route_is_left_out_of_the_dial() {
        let all = [a("[2001:db8::1]:1"), a("192.0.2.1:1")];
        let v4_only = usable(&all, |x| x.is_ipv4());
        assert_eq!(v4_only, vec![a("192.0.2.1:1")]);
        assert!(usable(&all, |_| false).is_empty());
        // Order is kept.
        assert_eq!(usable(&all, |_| true), all.to_vec());
    }

    #[test]
    fn loopback_has_a_route() {
        assert!(has_route(&a("127.0.0.1:9")));
    }
}
