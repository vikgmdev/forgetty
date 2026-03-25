//! Tab bar rendering and interaction.
//!
//! Renders the tab strip at the top of the window and handles tab
//! switching, creation, closing, and drag-to-reorder.

use crate::tab::TabId;

/// An entry in the tab bar.
#[derive(Debug, Clone)]
pub struct TabBarEntry {
    pub id: TabId,
    pub title: String,
    pub active: bool,
}

/// The tab bar state.
pub struct TabBar {
    pub entries: Vec<TabBarEntry>,
}

impl TabBar {
    /// Create a new empty tab bar.
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Update the tab bar entries from the current tab state.
    pub fn update(&mut self, tabs: &[(TabId, String)], active_id: TabId) {
        self.entries = tabs
            .iter()
            .map(|(id, title)| TabBarEntry {
                id: *id,
                title: title.clone(),
                active: *id == active_id,
            })
            .collect();
    }
}

impl Default for TabBar {
    fn default() -> Self {
        Self::new()
    }
}
