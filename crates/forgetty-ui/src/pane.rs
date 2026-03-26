//! Terminal pane management.
//!
//! A pane represents a single terminal session with its own VT parser,
//! PTY process, and viewport into the scrollback buffer.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

use forgetty_pty::{PtyProcess, PtySize};
use forgetty_vt::Terminal;
use tracing::{debug, warn};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

/// Global counter for unique pane IDs.
static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

/// A unique identifier for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

impl PaneId {
    /// Generate a new unique PaneId.
    pub fn next() -> Self {
        Self(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// A pane wraps a Terminal + PtyProcess + a read channel.
pub struct Pane {
    pub id: PaneId,
    pub terminal: Terminal,
    pub pty: PtyProcess,
    pub pty_rx: mpsc::Receiver<Vec<u8>>,
    pub title: String,
    pub cwd: String,
    /// Track whether the shell process has exited.
    shell_exited: bool,
}

impl Pane {
    /// Create a new pane, spawning a shell.
    ///
    /// Spawns a PTY reader thread internally that sends output via a channel.
    /// The `proxy` is used to wake the main event loop when PTY data arrives.
    pub fn new(
        id: PaneId,
        rows: usize,
        cols: usize,
        working_dir: Option<&Path>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, String> {
        let terminal = Terminal::new(rows, cols);
        let size =
            PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 };

        let mut pty =
            PtyProcess::spawn(size, working_dir, None).map_err(|e| format!("spawn: {e}"))?;

        let reader = pty
            .take_reader()
            .ok_or_else(|| "reader should be available on fresh PtyProcess".to_string())?;

        let (tx, rx) = mpsc::channel();

        let pane_id = id.0;
        thread::Builder::new()
            .name(format!("pty-reader-{}", pane_id))
            .spawn(move || {
                pane_reader_thread(reader, tx, proxy);
            })
            .map_err(|e| format!("failed to spawn PTY reader thread: {e}"))?;

        let cwd = working_dir.map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

        Ok(Self {
            id,
            terminal,
            pty,
            pty_rx: rx,
            title: String::from("shell"),
            cwd,
            shell_exited: false,
        })
    }

    /// Drain PTY output and feed to terminal.
    pub fn drain_output(&mut self) {
        let mut had_output = false;
        loop {
            match self.pty_rx.try_recv() {
                Ok(data) => {
                    had_output = true;
                    self.terminal.feed(&data);

                    // After each feed(), drain any write-PTY responses
                    // (DA responses, mode queries, etc.) and send them
                    // back to the PTY. This is the accumulator pattern:
                    // the callback appends during feed(), we drain here.
                    let responses = self.terminal.drain_write_pty();
                    for chunk in responses {
                        if let Err(e) = self.pty.write(&chunk) {
                            warn!(pane_id = self.id.0, "failed to write PTY response: {e}");
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.shell_exited {
                        debug!(pane_id = self.id.0, "PTY reader disconnected — shell exited");
                        self.shell_exited = true;
                    }
                    break;
                }
            }
        }

        // Update title from terminal if it changed.
        let t = self.terminal.title();
        if !t.is_empty() {
            self.title = t.to_string();
        }

        // Refresh cwd from /proc/{pid}/cwd when we received output.
        if had_output {
            if let Some(pid) = self.pty.pid() {
                let proc_path = format!("/proc/{}/cwd", pid);
                if let Ok(target) = std::fs::read_link(&proc_path) {
                    let new_cwd = target.to_string_lossy().to_string();
                    if new_cwd != self.cwd {
                        self.cwd = new_cwd;
                    }
                }
            }
        }
    }

    /// Return a display-friendly title for this pane.
    ///
    /// Priority: basename of cwd > OSC title (if meaningful) > "shell".
    pub fn display_title(&self) -> String {
        // Always prefer the actual cwd (from /proc) — it's a clean path.
        if !self.cwd.is_empty() {
            return std::path::Path::new(&self.cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| self.cwd.clone());
        }

        // Fall back to OSC title if cwd is not available.
        if !self.title.is_empty() && self.title != "shell" {
            return self.title.clone();
        }

        "shell".to_string()
    }

    /// Write bytes to PTY.
    pub fn write(&mut self, data: &[u8]) {
        if let Err(e) = self.pty.write(data) {
            warn!(pane_id = self.id.0, "failed to write to PTY: {e}");
        }
    }

    /// Resize terminal and PTY.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.terminal.resize(rows, cols);
        let pty_size =
            PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 };
        if let Err(e) = self.pty.resize(pty_size) {
            warn!(pane_id = self.id.0, "failed to resize PTY: {e}");
        }
    }

    /// Check if shell is still alive.
    pub fn is_alive(&mut self) -> bool {
        self.pty.is_alive()
    }

    /// Whether the shell has exited (detected via reader disconnect).
    pub fn shell_exited(&self) -> bool {
        self.shell_exited
    }
}

/// Background thread that reads from the PTY and sends data via the channel.
fn pane_reader_thread(
    mut reader: Box<dyn std::io::Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("Pane PTY reader: EOF");
                break;
            }
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
                // Wake the event loop to process the new data
                let _ = proxy.send_event(UserEvent::PtyOutput);
            }
            Err(e) => {
                debug!("Pane PTY reader error: {e}");
                break;
            }
        }
    }
}
