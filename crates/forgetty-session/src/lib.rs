//! `forgetty-session` — platform-agnostic session manager for Forgetty.
//!
//! This crate owns all PTY processes and VT state. It compiles with zero GTK
//! dependencies. The GTK shell imports this crate to get `SessionManager`;
//! the reverse dependency is never allowed.
//!
//! ## Crate layout
//!
//! - [`manager`] — `SessionManager` public API
//! - [`pane`] — `PaneState` (private) + `PaneInfo` (public)
//! - [`pty_bridge`] — `PtyBridge` (owns `PtyProcess` + reader thread)
//! - [`vt_instance`] — `VtInstance` (thin wrapper over `forgetty_vt::Terminal`)
//! - [`events`] — `SessionEvent`, `NotificationPayload`, `NotificationSource`
//! - [`drain_result`] — `DrainResult`
//! - [`workspace`] — `WorkspaceLayout` types and `build_workspace_state()`

pub mod drain_result;
pub mod events;
pub mod manager;
pub mod pane;
pub mod pty_bridge;
pub mod vt_instance;
pub mod workspace;

// Convenient top-level re-exports for downstream crates.
pub use drain_result::DrainResult;
pub use events::{NotificationPayload, NotificationSource, SessionEvent};
pub use manager::SessionManager;
pub use pane::{PaneInfo, PaneState};
pub use pty_bridge::PtyBridge;
pub use vt_instance::VtInstance;
pub use workspace::{
    build_workspace_state, PaneTreeLayout, TabLayoutEntry, WorkspaceLayout, WorkspaceLayoutEntry,
};
