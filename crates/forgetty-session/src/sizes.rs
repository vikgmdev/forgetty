//! Size ownership (AD-020).
//!
//! One client owns each pane's size: the PTY has the size it asked for, and
//! its `auto` input reaches the PTY. The policy that picks the owner lives in
//! the server (`rpc_v2`). This module holds what the other crates need: the
//! owner record that `get_pane_info` and `PaneResized` report, and the hook
//! through which a local socket client takes part in the policy.

use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use uuid::Uuid;

/// The client that owns a pane's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeOwner {
    /// Client id.
    pub client_id: Uuid,
    /// Wire name of the client kind (`android`, `windows`, `cli`, `gtk` for
    /// remote clients, `local` for a socket client of the server).
    pub client_kind: String,
    /// Device or client name.
    pub client_name: String,
}

/// The size policy, as local socket clients see it. Set once at start-up by
/// the server. Without one, a local resize goes straight to the PTY.
pub trait SizeGovernor: Send + Sync {
    /// The registered local client `client` asks for `size` on `pane`. It
    /// takes the pane's size ownership when it has a `subscribe_output` there.
    fn local_resize(&self, client: Uuid, pane: PaneId, size: PtySize);

    /// `client` started a `subscribe_output` on `pane`: it has a stream there.
    fn local_attached(&self, client: Uuid, pane: PaneId);

    /// That `subscribe_output` ended.
    fn local_detached(&self, client: Uuid, pane: PaneId);

    /// The last connection of the local client `client` closed.
    fn local_left(&self, client: Uuid);
}
