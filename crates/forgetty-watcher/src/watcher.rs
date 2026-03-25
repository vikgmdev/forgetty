//! Core file watcher implementation.
//!
//! Uses the `notify` crate to watch directories and files for changes,
//! debounces events, and dispatches notifications to subscribers.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The set of file extensions considered previewable.
const PREVIEWABLE_EXTENSIONS: &[&str] =
    &["md", "markdown", "png", "jpg", "jpeg", "gif", "svg", "webp"];

/// Debounce interval for duplicate file change events.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(100);

/// Describes the kind of file change detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// A new file was created.
    Created,
    /// An existing file was modified.
    Modified,
    /// A file was removed.
    Removed,
}

/// A single file change event.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// The path to the file that changed.
    pub path: PathBuf,
    /// What kind of change occurred.
    pub kind: ChangeKind,
}

/// Watches a directory for changes to previewable files.
///
/// Uses `notify`'s recommended watcher under the hood and provides a
/// simple polling API with built-in debouncing.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    /// Tracks the last event time per path for debouncing.
    last_events: Vec<(PathBuf, Instant)>,
}

impl FileWatcher {
    /// Watch a directory for previewable file changes.
    ///
    /// Starts watching the given path recursively. Use [`poll`](Self::poll)
    /// to retrieve pending change events.
    pub fn new(path: &Path) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())?;
        watcher.watch(path, RecursiveMode::Recursive)?;

        Ok(Self { _watcher: watcher, rx, last_events: Vec::new() })
    }

    /// Poll for file changes (non-blocking).
    ///
    /// Returns a list of debounced file changes for previewable files.
    /// Duplicate events for the same path within the debounce interval
    /// are collapsed.
    pub fn poll(&mut self) -> Vec<FileChange> {
        let mut changes = Vec::new();
        let now = Instant::now();

        // Drain all pending events from the channel.
        while let Ok(event_result) = self.rx.try_recv() {
            let event = match event_result {
                Ok(e) => e,
                Err(_) => continue,
            };

            let change_kind = match &event.kind {
                EventKind::Create(_) => ChangeKind::Created,
                EventKind::Modify(_) => ChangeKind::Modified,
                EventKind::Remove(_) => ChangeKind::Removed,
                _ => continue,
            };

            for path in event.paths {
                if !Self::is_previewable(&path) {
                    continue;
                }

                // Debounce: skip if we recently processed an event for this path.
                let dominated = self
                    .last_events
                    .iter()
                    .any(|(p, t)| p == &path && now.duration_since(*t) < DEBOUNCE_INTERVAL);
                if dominated {
                    continue;
                }

                // Update last event time for this path.
                if let Some(entry) = self.last_events.iter_mut().find(|(p, _)| p == &path) {
                    entry.1 = now;
                } else {
                    self.last_events.push((path.clone(), now));
                }

                changes.push(FileChange { path, kind: change_kind.clone() });
            }
        }

        // Prune stale entries from last_events to prevent unbounded growth.
        let cutoff = Duration::from_secs(60);
        self.last_events.retain(|(_, t)| now.duration_since(*t) < cutoff);

        changes
    }

    /// Check if a file is previewable based on its extension.
    pub fn is_previewable(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| {
                let ext = ext.to_lowercase();
                PREVIEWABLE_EXTENSIONS.contains(&ext.as_str())
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_files_are_previewable() {
        assert!(FileWatcher::is_previewable(Path::new("README.md")));
        assert!(FileWatcher::is_previewable(Path::new("guide.markdown")));
    }

    #[test]
    fn image_files_are_previewable() {
        assert!(FileWatcher::is_previewable(Path::new("photo.png")));
        assert!(FileWatcher::is_previewable(Path::new("photo.jpg")));
        assert!(FileWatcher::is_previewable(Path::new("photo.jpeg")));
        assert!(FileWatcher::is_previewable(Path::new("anim.gif")));
        assert!(FileWatcher::is_previewable(Path::new("icon.svg")));
        assert!(FileWatcher::is_previewable(Path::new("banner.webp")));
    }

    #[test]
    fn non_previewable_files() {
        assert!(!FileWatcher::is_previewable(Path::new("main.rs")));
        assert!(!FileWatcher::is_previewable(Path::new("data.json")));
        assert!(!FileWatcher::is_previewable(Path::new("Cargo.toml")));
        assert!(!FileWatcher::is_previewable(Path::new("Makefile")));
    }

    #[test]
    fn case_insensitive_extensions() {
        assert!(FileWatcher::is_previewable(Path::new("README.MD")));
        assert!(FileWatcher::is_previewable(Path::new("PHOTO.PNG")));
        assert!(FileWatcher::is_previewable(Path::new("photo.Jpg")));
    }

    #[test]
    fn no_extension_is_not_previewable() {
        assert!(!FileWatcher::is_previewable(Path::new("LICENSE")));
    }

    #[test]
    fn change_kind_debug() {
        // Ensure ChangeKind derives work correctly.
        assert_eq!(ChangeKind::Created, ChangeKind::Created);
        assert_ne!(ChangeKind::Created, ChangeKind::Modified);
        assert_ne!(ChangeKind::Modified, ChangeKind::Removed);
    }

    #[test]
    fn watcher_watches_temp_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let watcher = FileWatcher::new(dir.path());
        assert!(watcher.is_ok(), "Should be able to watch temp directory");
    }

    #[test]
    fn poll_returns_empty_when_no_changes() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut watcher = FileWatcher::new(dir.path()).unwrap();
        let changes = watcher.poll();
        // Fresh directory should have no changes.
        assert!(changes.is_empty());
    }
}
