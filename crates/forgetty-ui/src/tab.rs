//! Tab management.
//!
//! Each tab contains a pane tree and a title derived from the active
//! pane's working directory or running process.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::pane::PaneId;
use crate::pane_tree::{PaneNode, SplitDirection};

/// Global counter for unique tab IDs.
static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

/// A unique identifier for a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

impl TabId {
    /// Generate a new unique TabId.
    pub fn next() -> Self {
        Self(NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// A tab containing a pane tree and focus tracking.
pub struct Tab {
    pub id: TabId,
    pub pane_tree: PaneNode,
    pub focused_pane: PaneId,
    pub title: String,
}

impl Tab {
    /// Create a new tab with a single pane.
    pub fn new(id: TabId, initial_pane: PaneId) -> Self {
        Self {
            id,
            pane_tree: PaneNode::Leaf(initial_pane),
            focused_pane: initial_pane,
            title: String::from("shell"),
        }
    }

    /// Split the focused pane in the given direction.
    /// The new pane becomes the second child.
    pub fn split(&mut self, direction: SplitDirection, new_pane: PaneId) {
        self.pane_tree.split(self.focused_pane, direction, new_pane);
    }

    /// Close a pane. If it's the last pane in the tab, returns true
    /// to indicate the tab should close.
    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let ids = self.pane_tree.pane_ids();
        if ids.len() <= 1 {
            return true; // Last pane — tab should close.
        }

        // If the focused pane is being closed, move focus to the next one first.
        if self.focused_pane == pane_id {
            self.focus_next();
        }

        self.pane_tree.remove(pane_id);
        false
    }

    /// Cycle focus to the next pane in tree order.
    pub fn focus_next(&mut self) {
        let ids = self.pane_tree.pane_ids();
        if ids.is_empty() {
            return;
        }
        let current_idx = ids.iter().position(|id| *id == self.focused_pane);
        let next_idx = match current_idx {
            Some(i) => (i + 1) % ids.len(),
            None => 0,
        };
        self.focused_pane = ids[next_idx];
    }

    /// Navigate focus in a direction.
    pub fn focus_direction(&mut self, direction: SplitDirection, forward: bool) {
        if let Some(next) = self.pane_tree.find_adjacent(self.focused_pane, direction, forward) {
            self.focused_pane = next;
        }
    }
}
