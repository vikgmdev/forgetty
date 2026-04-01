//! `PtyBridge` — owns a `PtyProcess` and a background reader thread.
//!
//! Mirrors the pattern in `forgetty-gtk/src/pty_bridge.rs` but lives in the
//! platform-agnostic session crate. The reader thread runs independently and
//! sends `Vec<u8>` chunks to the session manager via an mpsc channel.

use std::io::Read as IoRead;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use forgetty_pty::{PtyProcess, PtySize};
use tracing::{debug, warn};

/// PTY-owning bridge with a background reader thread.
///
/// Owns the `PtyProcess` (for writing and resizing on the caller's thread) and
/// the `mpsc::Receiver<Vec<u8>>` that delivers output from the reader thread.
pub struct PtyBridge {
    /// The live PTY process (writing and resizing happen here).
    pub pty: PtyProcess,
    /// Receiver end of the output channel from the reader thread.
    pub pty_rx: mpsc::Receiver<Vec<u8>>,
}

impl PtyBridge {
    /// Spawn a PTY process and start its background reader thread.
    ///
    /// - `size` — initial PTY dimensions.
    /// - `working_dir` — override the initial CWD (or `None` for default).
    /// - `command` — explicit command + args (or `None` to use config/detected shell).
    /// - `shell` — config shell override (or `None` for auto-detect).
    /// - `login_shell` — whether to invoke the command as a login shell.
    pub fn spawn(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
        shell: Option<&str>,
        login_shell: bool,
    ) -> Result<Self, String> {
        // Resolve effective command and login_shell semantics,
        // mirroring the logic in forgetty-gtk/src/pty_bridge.rs.
        let (effective_command, effective_login): (Option<Vec<String>>, bool) =
            if let Some(cmd) = command {
                // Explicit command (-e flag): run directly, no login shell.
                (Some(cmd.to_vec()), false)
            } else if let Some(s) = shell {
                // Config shell override: treat as the user's interactive login shell.
                (Some(vec![s.to_string()]), true)
            } else {
                // No override: PtyProcess detects the shell.
                (None, login_shell)
            };

        let mut pty =
            PtyProcess::spawn(size, working_dir, effective_command.as_deref(), effective_login)
                .map_err(|e| format!("spawn PTY: {e}"))?;

        let reader = pty
            .take_reader()
            .ok_or_else(|| "PTY reader should be available on fresh PtyProcess".to_string())?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        thread::Builder::new()
            .name("session-pty-reader".to_string())
            .spawn(move || {
                pty_reader_thread(reader, tx);
            })
            .map_err(|e| format!("failed to spawn PTY reader thread: {e}"))?;

        Ok(Self { pty, pty_rx: rx })
    }
}

/// Background thread that reads from the PTY and sends data via the channel.
fn pty_reader_thread(mut reader: Box<dyn IoRead + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("session PTY reader: EOF");
                break;
            }
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    debug!("session PTY reader: channel closed, stopping");
                    break;
                }
            }
            Err(e) => {
                warn!("session PTY reader error: {e}");
                break;
            }
        }
    }
}
