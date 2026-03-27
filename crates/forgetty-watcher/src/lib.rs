//! File system watcher for Forgetty.
//!
//! Monitors the file system for changes relevant to the terminal,
//! such as configuration file modifications (for live reload) and
//! workspace file changes.

pub mod watcher;

pub use watcher::{ChangeKind, ConfigWatcher, FileChange, FileWatcher};
