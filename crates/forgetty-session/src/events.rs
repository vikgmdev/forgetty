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
/// T-067 added `WorkspaceCreated`; FIX-001 added `WorkspaceRenamed`;
/// FIX-006 added `WorkspacesReordered`.
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

    /// The globally-active workspace index changed.
    ///
    /// Emitted when the client calls `set_active_workspace` so the daemon
    /// persists the new `layout.active_workspace`. Session-restore relies on
    /// this to bring the user's last-focused workspace back to the front.
    ActiveWorkspaceChanged { workspace_idx: usize },

    // -----------------------------------------------------------------------
    // Workspace mutation events (T-067, FIX-001)
    // -----------------------------------------------------------------------
    /// A new workspace was created.
    WorkspaceCreated { workspace_idx: usize, workspace_id: Uuid, name: String },

    /// A workspace was renamed. Carries the index, id, and new name so
    /// subscribers can update their local copy without re-fetching the full
    /// layout. Emitted only when the name actually changed (idempotence).
    WorkspaceRenamed { workspace_idx: usize, workspace_id: Uuid, name: String },

    /// A workspace was deleted (FIX-003). Carries the index valid at emission
    /// time (may be stale if another client shifted indices concurrently) and
    /// the stable workspace id — subscribers should match on `workspace_id`
    /// for idempotency. Emitted AFTER all per-pane `PaneClosed` events for the
    /// deleted workspace's panes, so subscribers unwind pane state before
    /// dropping the workspace row.
    WorkspaceDeleted { workspace_idx: usize, workspace_id: Uuid },

    /// A workspace's accent colour changed (FIX-010). `color` is `Some(hex)`
    /// for a set, `None` for a clear. Subscribers update their local row-style
    /// without re-fetching the layout. Emitted only when the value actually
    /// changed (idempotence — mirror `WorkspaceRenamed`).
    WorkspaceColorChanged { workspace_idx: usize, workspace_id: Uuid, color: Option<String> },

    /// Two workspaces swapped positions in the sidebar order (FIX-006).
    /// Carries both stable workspace IDs (not just indices) so subscribers
    /// can reconcile by UUID if their local indices are stale. Emitted only
    /// when `from_idx != to_idx` (same-index calls are idempotent no-ops).
    WorkspacesReordered {
        from_idx: usize,
        to_idx: usize,
        from_workspace_id: Uuid,
        to_workspace_id: Uuid,
    },
}
