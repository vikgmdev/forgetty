//! Forgetty app-shell core. No UI code: the layout model, the
//! `SessionBackend` trait, the ConPTY-backed `LocalBackend`, the keymap and
//! the fuzzy matcher. The Windows GPUI client and the remote backend on
//! `forgetty-client` (`clients/windows/remote`) plug into these types.

pub mod backend;
pub mod conpty;
pub mod fuzzy;
pub mod keys;
pub mod layout;
pub mod local;
pub mod notify;
pub mod update;
pub mod osc;
pub mod source;

pub use backend::*;
pub use forgetty_core::PaneId;
pub use layout::{Divider, Layout, PaneGeometry, PaneTree, Rect, Side, SplitDir, Tab, Workspace};
pub use local::{LocalBackend, LocalConfig};
pub use source::{PaneEvent, PaneSource};
pub use uuid::Uuid;
