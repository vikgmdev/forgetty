//! Cache of the last direct address reached per server node id.
//!
//! The server binds a fixed UDP port (PROTOCOL_V2 section 1), so after a
//! server restart the cached address is still right. The client dials it
//! first and lets the relay in the server address run in parallel. The app
//! persists [`AddrCache::snapshot`] and restores it with
//! [`AddrCache::from_snapshot`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Shared, cloneable in-memory cache. Clones see the same entries.
#[derive(Debug, Clone, Default)]
pub struct AddrCache(Arc<Mutex<HashMap<EndpointId, SocketAddr>>>);

/// One persisted entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAddr {
    /// Server iroh endpoint id, in its string form.
    pub node_id: String,
    /// Last direct address that worked.
    pub addr: SocketAddr,
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
                cache.insert(id, e.addr);
            }
        }
        cache
    }

    /// Cached address for a server.
    pub fn get(&self, id: EndpointId) -> Option<SocketAddr> {
        self.lock().get(&id).copied()
    }

    /// Record the address that worked.
    pub fn insert(&self, id: EndpointId, addr: SocketAddr) {
        self.lock().insert(id, addr);
    }

    /// Copy for persistence.
    pub fn snapshot(&self) -> AddrCacheSnapshot {
        let mut entries: Vec<CachedAddr> = self
            .lock()
            .iter()
            .map(|(id, addr)| CachedAddr { node_id: id.to_string(), addr: *addr })
            .collect();
        entries.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        AddrCacheSnapshot { entries }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, SocketAddr>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
