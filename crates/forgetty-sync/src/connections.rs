//! Live iroh connections, by device id.
//!
//! The device registry is checked once, when a connection opens. Removing a
//! device from the registry therefore does not end a connection that is
//! already open. This table closes that gap: every accepted connection, on
//! every ALPN, is recorded here under its remote device id, so a revoke can
//! close all of them.
//!
//! The hook sits in the accept loop, not in a protocol handler. Any protocol
//! added later (`forgetty/rpc/2`) is covered without extra code.

use std::sync::{Arc, Mutex};

use iroh::endpoint::Connection;

/// QUIC application error code sent to a connection closed by a revoke.
pub const REVOKED_CLOSE_CODE: u32 = 403;
/// Reason bytes sent with [`REVOKED_CLOSE_CODE`].
pub const REVOKED_CLOSE_REASON: &[u8] = b"device-revoked";

struct Entry {
    key: u64,
    device_id: String,
    conn: Connection,
}

#[derive(Default)]
struct Table {
    next: u64,
    entries: Vec<Entry>,
}

/// Shared table of the connections that are open right now.
#[derive(Clone, Default)]
pub struct LiveConnections {
    table: Arc<Mutex<Table>>,
}

impl LiveConnections {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `conn` under `device_id`. The entry removes itself when the
    /// connection closes, for any reason (event driven, no polling).
    pub fn track(&self, device_id: String, conn: Connection) {
        let key = {
            let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
            let key = t.next;
            t.next += 1;
            t.entries.push(Entry { key, device_id, conn: conn.clone() });
            key
        };
        let table = Arc::clone(&self.table);
        tokio::spawn(async move {
            let _ = conn.closed().await;
            table.lock().unwrap_or_else(|e| e.into_inner()).entries.retain(|e| e.key != key);
        });
    }

    /// Close every live connection of `device_id`. Returns how many it closed.
    pub fn revoke(&self, device_id: &str) -> usize {
        let doomed: Vec<Connection> = {
            let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
            let (gone, keep): (Vec<Entry>, Vec<Entry>) =
                std::mem::take(&mut t.entries).into_iter().partition(|e| e.device_id == device_id);
            t.entries = keep;
            gone.into_iter().map(|e| e.conn).collect()
        };
        for c in &doomed {
            c.close(REVOKED_CLOSE_CODE.into(), REVOKED_CLOSE_REASON);
        }
        doomed.len()
    }

    /// Number of tracked connections (tests, diagnostics).
    pub fn len(&self) -> usize {
        self.table.lock().unwrap_or_else(|e| e.into_inner()).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
