//! `PaneSource`: where a terminal view gets its bytes from.
//!
//! The view never knows whether bytes come from a local ConPTY or from a
//! remote durable-server pane. A remote implementation (forgetty-client over
//! iroh) implements this trait later and plugs in with no view changes.
//!
//! Contract:
//! - `events()` yields `PaneEvent::Output` chunks in order, then at most one
//!   `PaneEvent::Closed`. The channel may be bounded: a slow view applies
//!   backpressure to the source instead of losing bytes.
//! - `write_input` never blocks the caller (queue + writer thread).
//! - `resize` is idempotent; the view calls it at most once per frame.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

#[derive(Debug)]
pub enum PaneEvent {
    Output(Vec<u8>),
    /// The pane ended (shell exited, connection lost for good).
    Closed(String),
}

pub trait PaneSource {
    /// Stream of output and the closed signal.
    fn events(&self) -> async_channel::Receiver<PaneEvent>;
    /// Bytes typed or pasted by the user, already encoded for the PTY.
    fn write_input(&self, bytes: &[u8]);
    /// New grid size in cells.
    fn resize(&self, rows: u16, cols: u16);
    /// Stop the pane (kill the local child, or detach a remote pane).
    fn shutdown(&self);
}

/// Local ConPTY shell through portable-pty.
pub struct LocalConPty {
    rx: async_channel::Receiver<PaneEvent>,
    input_tx: mpsc::Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
}

impl LocalConPty {
    pub fn spawn(shell: &str, cwd: Option<&str>, rows: u16, cols: u16) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty (ConPTY)")?;

        let mut cmd = CommandBuilder::new(shell);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).with_context(|| format!("spawn {shell}"))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let mut writer = pair.master.take_writer().context("take writer")?;

        // Bounded: ~4 MB in flight, then the reader blocks and ConPTY backs off.
        let (tx, rx) = async_channel::bounded::<PaneEvent>(256);
        let closed_sent = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(child));

        {
            let tx = tx.clone();
            let closed_sent = closed_sent.clone();
            std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send_blocking(PaneEvent::Output(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
                if !closed_sent.swap(true, Ordering::SeqCst) {
                    let _ = tx.send_blocking(PaneEvent::Closed("pty closed".into()));
                }
            })?;
        }
        {
            // ConPTY often keeps the output pipe open after the child exits.
            let tx = tx.clone();
            let child = child.clone();
            std::thread::Builder::new().name("pty-wait".into()).spawn(move || loop {
                std::thread::sleep(Duration::from_millis(100));
                let status = match child.lock() {
                    Ok(mut c) => c.try_wait(),
                    Err(_) => return,
                };
                match status {
                    Ok(Some(st)) => {
                        std::thread::sleep(Duration::from_millis(300));
                        if !closed_sent.swap(true, Ordering::SeqCst) {
                            let _ = tx.send_blocking(PaneEvent::Closed(format!("exited: {st:?}")));
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            })?;
        }

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        })?;

        Ok(Self { rx, input_tx, master: Mutex::new(pair.master), child })
    }
}

impl PaneSource for LocalConPty {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.rx.clone()
    }

    fn write_input(&self, bytes: &[u8]) {
        let _ = self.input_tx.send(bytes.to_vec());
    }

    fn resize(&self, rows: u16, cols: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    fn shutdown(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}
