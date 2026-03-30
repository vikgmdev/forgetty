//! PTY multiplexer for managing multiple terminal sessions.
//!
//! Routes I/O between multiple PTY processes and their corresponding
//! terminal panes, handling concurrent reads and dispatching output
//! to the correct VT parser instance.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use forgetty_core::error::{ForgettyError, Result};
use tracing::debug;

use crate::process::{PtyProcess, PtySize};

/// A unique identifier for a PTY session managed by the multiplexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtyId(u64);

impl PtyId {
    /// Returns the raw numeric ID.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PtyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pty-{}", self.0)
    }
}

/// Manages multiple PTY processes.
pub struct PtyMultiplexer {
    sessions: HashMap<PtyId, PtyProcess>,
    next_id: u64,
}

impl PtyMultiplexer {
    /// Create a new, empty multiplexer.
    pub fn new() -> Self {
        Self { sessions: HashMap::new(), next_id: 0 }
    }

    /// Spawn a new PTY process and return its ID.
    pub fn spawn(
        &mut self,
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
    ) -> Result<PtyId> {
        let id = PtyId(self.next_id);
        self.next_id += 1;

        // Multiplexer spawns are always interactive shells (no -e commands),
        // so use login shell semantics.
        let login_shell = command.is_none();
        let process = PtyProcess::spawn(size, working_dir, command, login_shell)?;
        debug!(%id, pid = ?process.pid(), "multiplexer spawned new PTY");
        self.sessions.insert(id, process);

        Ok(id)
    }

    /// Write data to a specific PTY.
    pub fn write(&mut self, id: PtyId, data: &[u8]) -> Result<()> {
        let process = self
            .sessions
            .get_mut(&id)
            .ok_or_else(|| ForgettyError::Pty(format!("no PTY with id {id}")))?;
        process.write(data)
    }

    /// Resize a specific PTY.
    pub fn resize(&self, id: PtyId, size: PtySize) -> Result<()> {
        let process = self
            .sessions
            .get(&id)
            .ok_or_else(|| ForgettyError::Pty(format!("no PTY with id {id}")))?;
        process.resize(size)
    }

    /// Close (kill) a specific PTY and remove it from the multiplexer.
    pub fn close(&mut self, id: PtyId) -> Result<()> {
        let mut process = self
            .sessions
            .remove(&id)
            .ok_or_else(|| ForgettyError::Pty(format!("no PTY with id {id}")))?;
        debug!(%id, "closing PTY");
        process.kill().ok(); // Best effort kill.
        Ok(())
    }

    /// Get a mutable reference to a specific PTY process.
    pub fn get_mut(&mut self, id: PtyId) -> Option<&mut PtyProcess> {
        self.sessions.get_mut(&id)
    }

    /// List all active PTY IDs.
    pub fn active_ids(&self) -> Vec<PtyId> {
        self.sessions.keys().copied().collect()
    }

    /// Close all PTY sessions.
    pub fn close_all(&mut self) {
        let ids: Vec<PtyId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.close(id).ok();
        }
    }
}

impl Default for PtyMultiplexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PtyMultiplexer {
    fn drop(&mut self) {
        self.close_all();
    }
}
