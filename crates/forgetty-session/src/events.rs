//! Session event types.
//!
//! `SessionEvent` is the broadcast channel payload for future consumers (daemon,
//! Android client, MCP server).

use bytes::Bytes;
use forgetty_core::PaneId;
use uuid::Uuid;

/// What a label (`set_label`) is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelTarget {
    Workspace,
    Tab,
    Pane,
}

impl LabelTarget {
    /// Wire name: `workspace`, `tab` or `pane`.
    pub fn as_str(self) -> &'static str {
        match self {
            LabelTarget::Workspace => "workspace",
            LabelTarget::Tab => "tab",
            LabelTarget::Pane => "pane",
        }
    }

    /// Parse a wire name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(LabelTarget::Workspace),
            "tab" => Some(LabelTarget::Tab),
            "pane" => Some(LabelTarget::Pane),
            _ => None,
        }
    }
}

/// Events broadcast to subscribers (daemon, Android, MCP server, etc.).
///
/// In T-048 GTK does not yet consume this channel — it still polls via
/// `drain_output()`. The channel is wired now so T-050 can activate it
/// without further structural changes.
///
/// As of T-063 this channel also carries layout mutation events
/// (`TabCreated`, `TabClosed`, `PaneSplit`, `TabMoved`, `ActiveTabChanged`).
/// T-067 added `WorkspaceCreated`; FIX-001 added `WorkspaceRenamed`;
/// FIX-006 added `WorkspacesReordered`; FIX-005B added `ActivePaneChanged`.
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
    // Pane metadata found by the OSC scanner (AD-019)
    // -----------------------------------------------------------------------
    /// The pane title changed (OSC 0 or 2).
    TitleChanged { pane_id: PaneId, title: String },

    /// The pane working directory changed (OSC 7).
    CwdChanged { pane_id: PaneId, cwd: String },

    /// The pane's program status changed (OSC 7501). `status` is one of
    /// `idle`, `working`, `waiting`, `done`, `failed`.
    StatusChanged { pane_id: PaneId, status: String, message: Option<String> },

    /// The shell state of the pane changed (P7.1): a prompt mark (OSC 133), a
    /// new OSC 7 directory, or a new tty mode. Carries the whole state.
    ShellState { pane_id: PaneId, state: crate::shell::ShellSnapshot },

    /// A program in the pane sent a desktop notification (OSC 9, 99, 777).
    /// Rate limited per pane.
    Notification { pane_id: PaneId, title: Option<String>, body: String },

    /// The pane's process exited. Sent for kept and for closing panes, before
    /// `PaneClosed`.
    PaneExited { pane_id: PaneId, exit_code: Option<i32> },

    /// The pane's PTY size changed (AD-020). Sent only when rows or cols
    /// actually changed.
    PaneResized {
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        /// The pane's size owner when the size changed.
        size_owner: Option<crate::sizes::SizeOwner>,
    },

    /// The pane's size owner changed (AD-020). Sent before the
    /// `PaneResized` the new owner's size causes.
    SizeOwnerChanged { pane_id: PaneId, owner: Option<crate::sizes::SizeOwner> },

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

    /// A label changed (`set_label`). For a workspace the label is its name
    /// (`WorkspaceRenamed` is sent as well). An empty label means cleared.
    LabelChanged { target: LabelTarget, id: Uuid, label: String },

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

    /// FIX-005B: the active pane id changed for a tab. `None` means the
    /// focus pointer was cleared (resets to leftmost leaf on next render).
    /// Emitted by `SessionManager::set_active_pane` only when the value
    /// actually changed (idempotent). Subscribers update the per-tab
    /// `active_pane_id` and (on the GTK side) call `grab_focus` on the
    /// matching DrawingArea for genuinely-external events; self-echoes
    /// are filtered via a provenance queue (see GTK `pending_active_pane`).
    ActivePaneChanged { workspace_idx: usize, tab_id: Uuid, pane_id: Option<PaneId> },
}
