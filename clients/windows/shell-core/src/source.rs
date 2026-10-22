//! `PaneSource`: one client's attachment to one pane. The view reads bytes
//! from it and writes input to it. It never knows whether the pane is a local
//! ConPTY or a remote pane behind the server (`attach_pane` plus the input
//! stream plus `resize_pane`).
//!
//! Contract:
//! - `events()` yields `PaneEvent::Output` chunks in order (a `Reset` first on
//!   a remote pane), then at most one
//!   `PaneEvent::Closed`. The channel may be bounded: a slow view applies
//!   backpressure to the source instead of losing bytes.
//! - `write_input` never blocks the caller (queue + writer thread).
//! - `resize` is idempotent; the view calls it at most once per frame.

#[derive(Debug)]
pub enum PaneEvent {
    /// Clear the pane's VT state and use this PTY size. The output that follows
    /// is a replay from the server, not new typing (first attach, or the
    /// server's log no longer holds the offset we asked for).
    ///
    /// `exact_replay` is true when the source also sends `ReplayEnd` where the replay stops,
    /// so the view does not guess it.
    Reset { rows: u16, cols: u16, exact_replay: bool },
    Output(Vec<u8>),
    /// The replay after the last `Reset` is over: the bytes from here on are live. Only sent
    /// when that `Reset` had `exact_replay`.
    ReplayEnd,
    /// The pane ended (shell exited, connection lost for good).
    Closed(String),
}

pub trait PaneSource: Send {
    /// Stream of output and the closed signal.
    fn events(&self) -> async_channel::Receiver<PaneEvent>;
    /// Bytes typed or pasted by the user, already encoded for the PTY.
    fn write_input(&self, bytes: &[u8]);
    /// A terminal reply (DA, CPR, kitty answers) or a focus report: bytes the program did
    /// not get from the user (AD-025). Never counts as typing. Defaults to plain input.
    fn write_auto(&self, bytes: &[u8]) {
        self.write_input(bytes);
    }
    /// New grid size in cells.
    fn resize(&self, rows: u16, cols: u16);
    /// True when the PTY size belongs to whichever client typed last (AD-020): the view
    /// renders at the size the server reports and centers it, and only tells the server
    /// what fits its window. False for a local PTY the window owns.
    fn letterbox(&self) -> bool {
        false
    }
    /// Input bytes this source had to drop (full buffer, no connection). Only grows.
    fn dropped_input_bytes(&self) -> u64 {
        0
    }
    /// Send the size again even if it did not change. A window that becomes
    /// the active client again uses this to win the size policy back (AD-020).
    fn reassert_size(&self, rows: u16, cols: u16) {
        self.resize(rows, cols);
    }
    /// Detach this client. The backend decides whether the pane lives on.
    fn shutdown(&self);
}
