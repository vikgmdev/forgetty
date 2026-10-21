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
struct State {
    clients: BTreeMap<u64, ClientInfo>,
    /// The server is stopping: no new client may attach.
    stopping: bool,
}

#[derive(Default)]
struct Inner {
    next: AtomicU64,
    state: Mutex<State>,
    /// Called with the client id when its last registration is dropped.
    leave_hook: Mutex<Option<LeaveHook>>,
}

type LeaveHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The server is shutting down and takes no new attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopping;

impl std::fmt::Display for Stopping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server is stopping")
    }
}

impl std::error::Error for Stopping {}

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
    ///
    /// Fails with [`Stopping`] once the server has begun to stop.
    pub fn register(
        &self,
        client_id: impl Into<String>,
        client_kind: &str,
        client_name: impl Into<String>,
    ) -> Result<ClientGuard, Stopping> {
        let key = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let connected_at =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let info = ClientInfo {
            client_id: client_id.into(),
            client_kind: client_kind.to_string(),
            client_name: client_name.into(),
            connected_at,
        };
        let mut st = self.state();
        if st.stopping {
            return Err(Stopping);
        }
        st.clients.insert(key, info);
        Ok(ClientGuard { registry: self.clone(), key })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Call `hook` with the client id each time the last registration of a
    /// client id is dropped. The hook runs outside the registry lock.
    pub fn set_leave_hook(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *self.inner.leave_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// The oldest registration with this client id.
    pub fn find(&self, client_id: &str) -> Option<ClientInfo> {
        self.state().clients.values().find(|c| c.client_id == client_id).cloned()
    }

    /// Snapshot of all attached clients, oldest first.
    pub fn list(&self) -> Vec<ClientInfo> {
        self.state().clients.values().cloned().collect()
    }

    /// Number of attached clients.
    pub fn count(&self) -> usize {
        self.state().clients.len()
    }

    /// Begin stopping unconditionally: new attaches are refused from now on.
    pub fn begin_stop(&self) {
        self.state().stopping = true;
    }

    /// `stop --if-idle`: if no client is attached, enter the stopping state
    /// and return `Ok`; otherwise leave the state alone and return the number
    /// of attached clients.
    ///
    /// The idle check and the switch to "stopping" happen under one lock. A
    /// client that attaches while the server shuts down is therefore either
    /// counted (the stop is refused) or refused (it cannot attach). It can
    /// never be attached and then dropped by the exit.
    pub fn begin_stop_if_idle(&self) -> Result<(), usize> {
        let mut st = self.state();
        if st.clients.is_empty() {
            st.stopping = true;
            Ok(())
        } else {
            Err(st.clients.len())
        }
    }

    /// Leave the stopping state (a stop that did not go through).
    pub fn cancel_stop(&self) {
        self.state().stopping = false;
    }

    /// Whether the server is stopping.
    pub fn is_stopping(&self) -> bool {
        self.state().stopping
    }
}

/// Removes its client from the registry when dropped.
pub struct ClientGuard {
    registry: ClientRegistry,
    key: u64,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let left = {
            let mut st = self.registry.state();
            let removed = st.clients.remove(&self.key);
            removed.filter(|c| !st.clients.values().any(|o| o.client_id == c.client_id))
        };
        if let Some(info) = left {
            let hook =
                self.registry.inner.leave_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(hook) = hook {
                hook(&info.client_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_drop_updates_count() {
        let reg = ClientRegistry::new();
        assert_eq!(reg.count(), 0);
        let a = reg.register("a", "socket", "cli").unwrap();
        let b = reg.register("b", "stream1", "phone").unwrap();
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.list()[1].client_name, "phone");
        drop(a);
        assert_eq!(reg.count(), 1);
        drop(b);
        assert_eq!(reg.count(), 0);
    }

    /// The leave hook runs when the last registration of a client id goes, not
    /// before, and `find` sees the client until then.
    #[test]
    fn leave_hook_fires_for_the_last_registration_of_an_id() {
        use std::sync::Mutex;
        let reg = ClientRegistry::new();
        let left = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = left.clone();
        reg.set_leave_hook(move |id| sink.lock().unwrap().push(id.to_string()));
        let a1 = reg.register("a", "socket", "layout").unwrap();
        let a2 = reg.register("a", "socket", "output").unwrap();
        let b = reg.register("b", "socket", "other").unwrap();
        assert_eq!(reg.find("a").unwrap().client_name, "layout");
        drop(a1);
        assert!(left.lock().unwrap().is_empty(), "a is still attached");
        drop(b);
        drop(a2);
        assert_eq!(*left.lock().unwrap(), ["b", "a"]);
        assert!(reg.find("a").is_none());
    }

    /// P1-QA item 7: `stop --if-idle` and an attach cannot both win.
    #[test]
    fn stopping_state_refuses_attaches_and_if_idle_is_atomic() {
        let reg = ClientRegistry::new();
        // Idle: the stop begins and a later attach is refused.
        assert_eq!(reg.begin_stop_if_idle(), Ok(()));
        assert!(reg.is_stopping());
        assert_eq!(reg.register("late", "socket", "late").err(), Some(Stopping));
        assert_eq!(reg.count(), 0);

        // Not idle: the stop is refused, the state stays clear, attaches work.
        let reg = ClientRegistry::new();
        let _a = reg.register("a", "socket", "cli").unwrap();
        assert_eq!(reg.begin_stop_if_idle(), Err(1));
        assert!(!reg.is_stopping());
        assert!(reg.register("b", "socket", "cli").is_ok());

        // A stop that did not go through can be cancelled.
        reg.begin_stop();
        assert!(reg.register("c", "socket", "cli").is_err());
        reg.cancel_stop();
        assert!(reg.register("d", "socket", "cli").is_ok());
    }

    /// Under contention, every attach either counts against the stop or is
    /// refused. An attach that succeeded after a successful idle stop is the bug.
    #[test]
    fn racing_attaches_never_succeed_after_an_idle_stop() {
        for _ in 0..200 {
            let reg = ClientRegistry::new();
            let r2 = reg.clone();
            let t = std::thread::spawn(move || r2.register("x", "socket", "x"));
            let stop = reg.begin_stop_if_idle();
            let attach = t.join().unwrap();
            if stop.is_ok() {
                assert!(attach.is_err(), "attached after the idle stop began");
            }
        }
    }
}
