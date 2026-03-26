//! PTY I/O bridged to the GTK main loop.
//!
//! Spawns the user's shell in a pseudoterminal and runs a background reader
//! thread that sends PTY output to the GTK main thread via `std::sync::mpsc`.
//! A periodic GLib timeout on the main thread polls the channel and triggers
//! redraws.

use std::io::Read as IoRead;
use std::sync::mpsc;
use std::thread;

use forgetty_pty::{PtyProcess, PtySize};
use tracing::{debug, warn};

/// Spawn a PTY process and start a background reader thread.
///
/// Returns `(pty, receiver)` where:
/// - `pty` is the `PtyProcess` (used for writing and resizing on the main thread)
/// - `receiver` is an mpsc receiver that delivers `Vec<u8>` chunks of PTY output
pub fn spawn_pty_bridge(
    rows: u16,
    cols: u16,
    shell: Option<&str>,
) -> Result<(PtyProcess, mpsc::Receiver<Vec<u8>>), String> {
    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    let command: Option<Vec<String>> = shell.map(|s| vec![s.to_string()]);
    let command_ref: Option<&[String]> = command.as_deref();

    let mut pty =
        PtyProcess::spawn(size, None, command_ref).map_err(|e| format!("spawn PTY: {e}"))?;

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
