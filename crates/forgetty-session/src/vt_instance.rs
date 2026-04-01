//! `VtInstance` — a thin wrapper around `forgetty_vt::Terminal` for use
//! inside the session manager.
//!
//! The VT instance lives inside the `Arc<Mutex<>>` so it is only accessed
//! from the thread holding the lock. `Terminal` is `Send` but `!Sync`; the
//! mutex enforces the single-accessor invariant.

use forgetty_vt::Terminal;

/// Wrapper around `forgetty_vt::Terminal` owned by the session layer.
///
/// In T-048 there is one `VtInstance` per pane in the session layer AND a
/// second `Terminal` per pane in the GTK `TerminalState`. Both receive the
/// same byte streams. The duplication is cleaned up in T-051.
pub struct VtInstance {
    pub terminal: Terminal,
}

impl VtInstance {
    /// Create a new VT with the given initial dimensions.
    pub fn new(rows: usize, cols: usize) -> Self {
        let mut terminal = Terminal::new(rows, cols);
        // Mirror the GTK terminal setup: prime cursor style with DECSCUSR 1
        // (blinking block) so cursor_blinking() returns true before the shell
        // sends its own DECSCUSR.
        terminal.feed(b"\x1b[1 q");
        Self { terminal }
    }

    /// Feed bytes to the VT parser and drain any write-PTY responses,
    /// writing them back to the PTY via the provided writer closure.
    ///
    /// This mirrors the `drain_pty_output` loop in `forgetty-gtk`:
    /// - feed bytes to VT
    /// - drain DA / mode-response writes
    /// - write responses back to PTY
    pub fn feed_and_respond<W>(&mut self, data: &[u8], mut pty_write: W)
    where
        W: FnMut(&[u8]),
    {
        self.terminal.feed(data);
        let responses = self.terminal.drain_write_pty();
        for chunk in responses {
            pty_write(&chunk);
        }
    }

    /// Resize the VT.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.terminal.resize(rows, cols);
    }
}
