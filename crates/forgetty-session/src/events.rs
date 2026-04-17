//! Session event types.
//!
//! `SessionEvent` is the broadcast channel payload for future consumers (daemon,
//! Android client, MCP server).

use bytes::Bytes;
use forgetty_core::PaneId;
use uuid::Uuid;

/// Events broadcast to subscribers (daemon, Android, MCP server, etc.).
///
/// In T-048 GTK does not yet consume this channel — it still polls via
/// `drain_output()`. The channel is wired now so T-050 can activate it
/// without further structural changes.
///
/// As of T-063 this channel also carries layout mutation events
/// (`TabCreated`, `TabClosed`, `PaneSplit`, `TabMoved`, `ActiveTabChanged`).
/// Subscribers that only care about one class of event should filter in the
/// receive loop.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Raw output bytes from a pane's PTY.
    PtyOutput { pane_id: PaneId, data: Bytes },
    /// A new pane was created.
    PaneCreated { pane_id: PaneId },
    /// A pane was closed.
    PaneClosed { pane_id: PaneId },

    // -----------------------------------------------------------------------
    // Layout mutation events (T-063)
    // -----------------------------------------------------------------------
    /// A new tab was created in the given workspace.
    TabCreated { workspace_idx: usize, tab_id: Uuid, pane_id: PaneId },

    /// A tab was closed (all its panes have been killed).
    TabClosed { workspace_idx: usize, tab_id: Uuid },

    /// An existing pane was split, producing a new sibling pane.
    PaneSplit {
        tab_id: Uuid,
        parent_pane_id: PaneId,
        new_pane_id: PaneId,
        /// "horizontal" | "vertical"
        direction: String,
    },

    /// A tab was moved to a new position within its workspace.
    TabMoved { workspace_idx: usize, tab_id: Uuid, new_index: usize },

    /// The active tab index changed for a workspace.
    ActiveTabChanged { workspace_idx: usize, tab_idx: usize },

    // -----------------------------------------------------------------------
    // Workspace mutation events (T-067)
    // -----------------------------------------------------------------------
    /// A new workspace was created.
    WorkspaceCreated { workspace_idx: usize, workspace_id: Uuid, name: String },
}
