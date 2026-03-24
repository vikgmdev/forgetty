//! File system watcher for Forgetty.
//!
//! Monitors the file system for changes relevant to the terminal,
//! such as configuration file modifications (for live reload) and
//! workspace file changes.

pub mod watcher;

// TODO: Phase 5 — re-export key types once watcher is implemented
