//! `PaneSource`: one client's attachment to one pane. The view reads bytes
//! from it and writes input to it. It never knows whether the pane is a local
//! ConPTY or a remote pane behind the server (`attach_pane` plus the input
//! stream plus `resize_pane`).
//!
//! Contract:
//! - `events()` yields `PaneEvent::Output` chunks in order, then at most one
//!   `PaneEvent::Closed`. The channel may be bounded: a slow view applies
//!   backpressure to the source instead of losing bytes.
//! - `write_input` never blocks the caller (queue + writer thread).
//! - `resize` is idempotent; the view calls it at most once per frame.

#[derive(Debug)]
pub enum PaneEvent {
    Output(Vec<u8>),
    /// The pane ended (shell exited, connection lost for good).
    Closed(String),
}

pub trait PaneSource: Send {
    /// Stream of output and the closed signal.
    fn events(&self) -> async_channel::Receiver<PaneEvent>;
    /// Bytes typed or pasted by the user, already encoded for the PTY.
    fn write_input(&self, bytes: &[u8]);
    /// New grid size in cells.
    fn resize(&self, rows: u16, cols: u16);
    /// Detach this client. The backend decides whether the pane lives on.
    fn shutdown(&self);
}
