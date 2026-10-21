//! Attached-client registry (AD-017).
//!
//! A client is anything that holds a live attachment to the server: an iroh
//! connection or a local socket connection that is streaming output or layout
//! events. Short request/response socket calls (the CLI) are not clients.
//!
//! `forgetty server status`, `forgetty client ls` and `forgetty server stop
//! --if-idle` read this registry.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// One attached client, as reported by `list_clients`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Stable id: the device id for iroh clients, a fresh UUID for sockets.
    pub client_id: String,
    /// `"stream1"` (iroh `forgetty/stream/1`) or `"socket"`.
    pub client_kind: String,
    /// Device name for iroh clients, a short label for sockets.
    pub client_name: String,
    /// Seconds since the Unix epoch when the client attached.
    pub connected_at: u64,
}

#[derive(Default)]
struct Inner {
    next: AtomicU64,
    clients: Mutex<BTreeMap<u64, ClientInfo>>,
}

/// Shared, cheap-to-clone registry of attached clients.
#[derive(Clone, Default)]
pub struct ClientRegistry {
    inner: Arc<Inner>,
}

impl ClientRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a client. The returned guard removes it again on drop, so a
    /// panicking or cancelled connection task cannot leak an entry.
    pub fn register(
        &self,
        client_id: impl Into<String>,
        client_kind: &str,
        client_name: impl Into<String>,
    ) -> ClientGuard {
        let key = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let connected_at =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let info = ClientInfo {
            client_id: client_id.into(),
            client_kind: client_kind.to_string(),
            client_name: client_name.into(),
            connected_at,
        };
        self.inner.clients.lock().unwrap_or_else(|e| e.into_inner()).insert(key, info);
        ClientGuard { registry: self.clone(), key }
    }

    /// Snapshot of all attached clients, oldest first.
    pub fn list(&self) -> Vec<ClientInfo> {
        self.inner.clients.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// Number of attached clients.
    pub fn count(&self) -> usize {
        self.inner.clients.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Removes its client from the registry when dropped.
pub struct ClientGuard {
    registry: ClientRegistry,
    key: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.registry.inner.clients.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_drop_updates_count() {
        let reg = ClientRegistry::new();
        assert_eq!(reg.count(), 0);
        let a = reg.register("a", "socket", "cli");
        let b = reg.register("b", "stream1", "phone");
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.list()[1].client_name, "phone");
        drop(a);
        assert_eq!(reg.count(), 1);
        drop(b);
        assert_eq!(reg.count(), 0);
    }
}
