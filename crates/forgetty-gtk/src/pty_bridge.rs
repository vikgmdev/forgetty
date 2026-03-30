//! PTY I/O bridged to the GTK main loop.
//!
//! Spawns the user's shell in a pseudoterminal and runs a background reader
//! thread that sends PTY output to the GTK main thread via `std::sync::mpsc`.
//! A periodic GLib timeout on the main thread polls the channel and triggers
//! redraws.

use std::io::Read as IoRead;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use forgetty_pty::{PtyProcess, PtySize};
use tracing::{debug, warn};

/// Spawn a PTY process and start a background reader thread.
///
/// Returns `(pty, receiver)` where:
/// - `pty` is the `PtyProcess` (used for writing and resizing on the main thread)
/// - `receiver` is an mpsc receiver that delivers `Vec<u8>` chunks of PTY output
///
/// `working_dir` overrides the initial CWD for the spawned process.
/// `command` overrides the shell with an explicit command + args.
/// When both are `None`, the default shell starts in the default directory.
pub fn spawn_pty_bridge(
    rows: u16,
    cols: u16,
    shell: Option<&str>,
    working_dir: Option<&Path>,
    command: Option<&[String]>,
) -> Result<(PtyProcess, mpsc::Receiver<Vec<u8>>), String> {
    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    // CLI -e command takes priority over config shell.
    // Login shell semantics: apply to auto-detected shells and config shell
    // overrides, but NOT to -e commands (those are explicit programs).
    let (effective_command, login_shell): (Option<Vec<String>>, bool) = if let Some(cmd) = command {
        // -e flag: run the command directly, no login shell.
        (Some(cmd.to_vec()), false)
    } else if let Some(s) = shell {
        // Config shell override: treat as the user's interactive shell.
        (Some(vec![s.to_string()]), true)
    } else {
        // No override: PtyProcess will detect the shell.
        (None, true)
    };

    let mut pty = PtyProcess::spawn(size, working_dir, effective_command.as_deref(), login_shell)
        .map_err(|e| format!("spawn PTY: {e}"))?;

    let reader = pty
        .take_reader()
        .ok_or_else(|| "PTY reader should be available on fresh PtyProcess".to_string())?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    thread::Builder::new()
        .name("pty-reader".to_string())
        .spawn(move || {
            pty_reader_thread(reader, tx);
        })
        .map_err(|e| format!("failed to spawn PTY reader thread: {e}"))?;

    Ok((pty, rx))
}

/// Background thread that reads from the PTY and sends data via the channel.
fn pty_reader_thread(mut reader: Box<dyn IoRead + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("PTY reader: EOF");
                break;
            }
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    debug!("PTY reader: channel closed, stopping");
                    break;
                }
            }
            Err(e) => {
                warn!("PTY reader error: {e}");
                break;
            }
        }
    }
}
