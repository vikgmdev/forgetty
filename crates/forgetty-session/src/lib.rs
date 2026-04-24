//! `forgetty-session` — platform-agnostic session manager for Forgetty.
//!
//! Per AD-007 this crate is the daemon-side byte pipe: it owns PTY processes
//! and tees raw output into per-pane byte-log rings (AD-013). It does not
//! parse VT sequences — clients own all terminal semantics (AD-008). This
//! crate compiles with zero GTK dependencies; the GTK shell imports it to
//! get `SessionManager`. The reverse dependency is never allowed.
//!
//! ## Crate layout
//!
//! - [`manager`] — `SessionManager` public API
//! - [`layout`] — `SessionLayout`, `SessionWorkspace`, `SessionTab`
//! - [`pane`] — `PaneState` (private) + `PaneInfo` (public)
//! - [`pty_bridge`] — `PtyBridge` (owns `PtyProcess` + reader thread)
//! - [`byte_log`] — `ByteLog` (per-pane ring + append-only disk log)
//! - [`events`] — `SessionEvent`
//! - [`drain_result`] — `DrainResult`
//! - [`workspace`] — `WorkspaceLayout` types and `build_workspace_state()`

pub mod byte_log;
pub mod drain_result;
pub mod events;
pub mod layout;
pub mod manager;
pub mod pane;
pub mod pty_bridge;
pub mod workspace;

// Convenient top-level re-exports for downstream crates.
pub use byte_log::ByteLog;
pub use drain_result::DrainResult;
pub use events::SessionEvent;
pub use layout::{SessionLayout, SessionTab, SessionWorkspace};
pub use manager::SessionManager;
pub use pane::{DuplicatedTab, PaneInfo, PaneState};
pub use pty_bridge::PtyBridge;
pub use workspace::{
    build_workspace_state, PaneTreeLayout, TabLayoutEntry, WorkspaceLayout, WorkspaceLayoutEntry,
};
