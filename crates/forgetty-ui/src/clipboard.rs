//! System clipboard integration.
//!
//! Provides copy and paste functionality using the system clipboard
//! via the arboard crate.

/// A wrapper around the system clipboard.
pub struct Clipboard {
    inner: arboard::Clipboard,
}

impl Clipboard {
    /// Create a new clipboard handle.
    ///
    /// Returns `None` if the clipboard cannot be accessed (e.g., no display
    /// server on Linux).
    pub fn new() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|inner| Self { inner })
    }

    /// Get text from the clipboard.
    pub fn get_text(&mut self) -> Option<String> {
        self.inner.get_text().ok()
    }

    /// Set text on the clipboard. Returns `true` on success.
    pub fn set_text(&mut self, text: &str) -> bool {
        self.inner.set_text(text.to_string()).is_ok()
    }
}
