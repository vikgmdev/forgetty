//! `PtyBridge` — owns a `PtyProcess` and a background reader thread.
//!
//! Mirrors the pattern in `forgetty-gtk/src/pty_bridge.rs` but lives in the
//! platform-agnostic session crate. The reader thread runs independently and
//! sends `Vec<u8>` chunks to the session manager via a bounded tokio channel.
//! When the channel is full the thread stops reading, the PTY buffer fills and
//! the program blocks on write (flow control, nothing is dropped).

use std::io::Read as IoRead;
use std::path::Path;
use std::thread;

use forgetty_pty::{PtyProcess, PtySize};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tracing::{debug, warn};

use crate::pane_writer::PaneWriterOwner;

/// Chunks (up to 64 KiB each) the reader thread may queue before it blocks.
const PTY_QUEUE_CHUNKS: usize = 16;

/// PTY-owning bridge with a background reader thread.
///
/// Owns the `PtyProcess` (for writing and resizing on the caller's thread) and
/// the `tokio::sync::mpsc::Receiver<Vec<u8>>` that delivers output from
/// the reader thread. The receiver is stored as `Option` so callers can `take()`
/// it exactly once to hand ownership to a dedicated drain task (V2-001).
pub struct PtyBridge {
    /// The live PTY process (writing and resizing happen here).
    pub pty: PtyProcess,
    /// Receiver end of the output channel from the reader thread.
    ///
    /// `Some` on construction; taken to `None` by `SessionManager::take_pane_output_rx`
    /// when the per-pane drain task is spawned.
    pub pty_rx: Option<Receiver<Vec<u8>>>,
    /// Input queue and writer thread. Writes go here, never to `pty`, so a
    /// program that does not read cannot hold a lock (see [`crate::pane_writer`]).
    pub writer: PaneWriterOwner,
    /// The command the pane was started with: the explicit command, else the shell the config
    /// names. Empty when the default shell was detected.
    pub argv: Vec<String>,
}

impl PtyBridge {
    /// Spawn a PTY process and start its background reader thread.
    ///
    /// - `size` — initial PTY dimensions.
    /// - `working_dir` — override the initial CWD (or `None` for default).
    /// - `command` — explicit command + args (or `None` to use config/detected shell).
    /// - `shell` — config shell override (or `None` for auto-detect).
    /// - `login_shell` — whether to invoke the command as a login shell.
    /// - `integration` — shell integration scripts to load into the user's shell.
    pub fn spawn(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
        shell: Option<&str>,
        login_shell: bool,
        integration: Option<&crate::shell_integration::ShellIntegration>,
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

        // P7.1: the scripts load only into the user's own shell, never into an
        // explicit command.
        let extra_env = match (integration, command) {
            (Some(si), None) => {
                let program = shell.map(str::to_string).unwrap_or_else(forgetty_pty::detect_shell);
                si.env_for(&program, &|k| std::env::var(k).ok())
            }
            _ => Vec::new(),
        };
        let mut pty = PtyProcess::spawn_with_env(
            size,
            working_dir,
            effective_command.as_deref(),
            effective_login,
            &extra_env,
        )
        .map_err(|e| format!("spawn PTY: {e}"))?;

        let reader = pty
            .take_reader()
            .ok_or_else(|| "PTY reader should be available on fresh PtyProcess".to_string())?;

        let out = pty
            .take_writer()
            .ok_or_else(|| "PTY writer should be available on fresh PtyProcess".to_string())?;
        let writer =
            PaneWriterOwner::spawn(out).map_err(|e| format!("spawn PTY writer thread: {e}"))?;

        let (tx, rx) = channel::<Vec<u8>>(PTY_QUEUE_CHUNKS);

        thread::Builder::new()
            .name("session-pty-reader".to_string())
            .spawn(move || {
                pty_reader_thread(reader, tx);
            })
            .map_err(|e| format!("failed to spawn PTY reader thread: {e}"))?;

        let argv = effective_command.unwrap_or_default();
        Ok(Self { pty, pty_rx: Some(rx), writer, argv })
    }
}

/// Background thread that reads from the PTY and sends data via the channel.
fn pty_reader_thread(mut reader: Box<dyn IoRead + Send>, tx: Sender<Vec<u8>>) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("session PTY reader: EOF");
                break;
            }
            Ok(n) => {
                crate::latency_trace::pty_read();
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
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
