//! `SessionLayout` — daemon-owned workspace → tab → pane-tree hierarchy.
//!
//! This module defines the live, in-memory layout that `SessionManagerInner`
//! owns and mutates. It models exactly the hierarchy mandated by AD-002:
//!
//! ```text
//! Daemon
//! └── Workspaces (1..N)          ← SessionLayout / SessionWorkspace
//!     └── Tabs (1..N per ws)     ← SessionTab
//!         └── Pane tree          ← PaneTreeLayout (from crate::workspace)
//! ```
//!
//! T-062 adds `Serialize` (but not `Deserialize`) to these types so they can
//! be serialized to JSON for the `get_layout` RPC. They still do not derive
//! `Deserialize` — they are daemon-internal state, never deserialized from
//! socket input.

use serde::Serialize;
use uuid::Uuid;

use crate::workspace::PaneTreeLayout;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The full daemon-owned layout: an ordered list of workspaces plus the index
/// of the currently active one.
#[derive(Debug, Clone, Serialize)]
pub struct SessionLayout {
    pub workspaces: Vec<SessionWorkspace>,
    pub active_workspace: usize,
}

/// One workspace within the layout: holds an ordered list of tabs.
#[derive(Debug, Clone, Serialize)]
pub struct SessionWorkspace {
    pub id: Uuid,
    pub name: String,
    pub tabs: Vec<SessionTab>,
    /// Index of the visually-active tab. This is UI state advanced by GTK
    /// (via T-060 `set_active_tab`), NOT by `create_pane` (see AD-008).
    pub active_tab: usize,
}

/// One tab within a workspace: holds a pane tree (a leaf for a single pane,
/// or a `Split` tree for a split layout managed by T-060+).
#[derive(Debug, Clone, Serialize)]
pub struct SessionTab {
    pub id: Uuid,
    /// Tab title, populated from OSC sequences in T-060+. Empty until then.
    pub title: String,
    pub pane_tree: PaneTreeLayout,
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

impl SessionLayout {
    /// Create the initial layout: one empty default workspace, ready to accept
    /// tabs as `create_pane` is called.
    pub fn new_default() -> Self {
        Self {
            workspaces: vec![SessionWorkspace {
                id: Uuid::new_v4(),
                name: "Default".to_string(),
                tabs: Vec::new(),
                active_tab: 0,
            }],
            active_workspace: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::SessionManager;
    use forgetty_pty::PtySize;

    // -----------------------------------------------------------------------
    // Lightweight test — no PTY
    // -----------------------------------------------------------------------

    /// AC-2 / AC-6: `SessionLayout::new_default()` creates exactly one default
    /// workspace with the correct initial state, without spawning any PTY.
    #[test]
    fn test_session_layout_default() {
        let layout = SessionLayout::new_default();
        assert_eq!(layout.workspaces.len(), 1);
        assert_eq!(layout.workspaces[0].name, "Default");
        assert_eq!(layout.workspaces[0].tabs.len(), 0);
        assert_eq!(layout.active_workspace, 0);
        assert_eq!(layout.workspaces[0].active_tab, 0);
    }

    // -----------------------------------------------------------------------
    // Real-PTY test
    // -----------------------------------------------------------------------

    /// Extract the `PaneId` from a `Leaf` tab for readable assertions.
    /// Panics if the tab's pane_tree is not a `Leaf`.
    fn leaf_id(tab: &SessionTab) -> forgetty_core::PaneId {
        match &tab.pane_tree {
            PaneTreeLayout::Leaf { pane_id } => *pane_id,
            other => panic!("expected Leaf, got {:?}", other),
        }
    }

    /// AC-8: Exercises the full create → close → create sequence against a real
    /// `SessionManager` with real PTYs. Verifies that `layout()` reflects each
    /// mutation in the correct order.
    #[test]
    fn test_layout_create_close_sequence() {
        let session = SessionManager::new();
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

        // --- Step 1: create 3 panes. ---
        let id1 = session.create_pane(size, None, None, None, true).expect("create pane 1");
        let id2 = session.create_pane(size, None, None, None, true).expect("create pane 2");
        let id3 = session.create_pane(size, None, None, None, true).expect("create pane 3");

        {
            let layout = session.layout();
            let tabs = &layout.workspaces[0].tabs;
            assert_eq!(tabs.len(), 3, "expected 3 tabs after 3 create_pane calls");
            assert_eq!(leaf_id(&tabs[0]), id1, "tabs[0] should be pane 1");
            assert_eq!(leaf_id(&tabs[1]), id2, "tabs[1] should be pane 2");
            assert_eq!(leaf_id(&tabs[2]), id3, "tabs[2] should be pane 3");
        }

        // --- Step 2: close the middle pane (id2). ---
        session.close_pane(id2).expect("close pane 2");

        {
            let layout = session.layout();
            let tabs = &layout.workspaces[0].tabs;
            assert_eq!(tabs.len(), 2, "expected 2 tabs after closing pane 2");
            assert_eq!(leaf_id(&tabs[0]), id1, "tabs[0] should still be pane 1");
            assert_eq!(leaf_id(&tabs[1]), id3, "tabs[1] should now be pane 3");
        }

        // --- Step 3: create a fourth pane. ---
        let id4 = session.create_pane(size, None, None, None, true).expect("create pane 4");

        {
            let layout = session.layout();
            let tabs = &layout.workspaces[0].tabs;
            assert_eq!(tabs.len(), 3, "expected 3 tabs after creating pane 4");
            assert_eq!(leaf_id(&tabs[0]), id1, "tabs[0] should be pane 1");
            assert_eq!(leaf_id(&tabs[1]), id3, "tabs[1] should be pane 3");
            assert_eq!(leaf_id(&tabs[2]), id4, "tabs[2] should be pane 4 (appended last)");
        }

        // --- Teardown: close all remaining panes. ---
        session.close_pane(id1).ok();
        session.close_pane(id3).ok();
        session.close_pane(id4).ok();
    }
}
