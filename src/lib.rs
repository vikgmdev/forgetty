//! `forgetty_daemon` — library half of the `forgetty-daemon` crate.
//!
//! Under V2-011 (AD-015) the daemon binary owns the terminal-specific iroh
//! stream handler. Exposing it from a library target lets QA binaries
//! (`forgetty-stream-test`) import the exact `ClientMsg` / `DaemonMsg`
//! definitions the daemon serves, avoiding enum duplication drift with
//! Android's wire format.
//!
//! This crate intentionally has a tiny public surface; it exists to share
//! protocol types between the daemon binary and the QA tools in the same
//! package.

pub mod iroh_terminal;
