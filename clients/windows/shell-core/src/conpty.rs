//! Local shell on a pseudo console through portable-pty (ConPTY on Windows).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::source::PaneEvent;

/// Callbacks that run on the PTY threads, so the backend sees every byte and
/// the exit even when no view is attached.
pub struct Hooks {
    pub on_output: Box<dyn FnMut(&[u8]) + Send>,
    /// Called once, when the pane ended.
    pub on_exit: Box<dyn FnOnce(String) + Send>,
}

pub struct LocalConPty {
    rx: async_channel::Receiver<PaneEvent>,
    tx: async_channel::Sender<PaneEvent>,
    input_tx: mpsc::Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    exited: Arc<AtomicBool>,
}

/// Diagnostics: when enabled, the reader thread logs (read returned, bytes, sent).
pub static READ_TRACE: AtomicBool = AtomicBool::new(false);
pub static WRITE_LOG: Mutex<Vec<(std::time::Instant, Vec<u8>)>> = Mutex::new(Vec::new());
pub static READ_LOG: Mutex<Vec<(std::time::Instant, usize, std::time::Instant)>> = Mutex::new(Vec::new());

impl LocalConPty {
    pub fn spawn(argv: &[String], cwd: Option<&str>, rows: u16, cols: u16, hooks: Hooks) -> Result<Self> {
        let (shell, args) = argv.split_first().context("empty command")?;
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty (ConPTY)")?;

        let mut cmd = CommandBuilder::new(shell);
        for a in args {
            cmd.arg(a);
        }
        if let Some(cwd) = cwd {
            // Never stat or enter a UNC/device path: on Windows that can send
            // NTLM credentials to a server named by program output.
            if crate::osc::is_safe_local_path(cwd) && std::path::Path::new(cwd).is_dir() {
                cmd.cwd(cwd);
            }
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
        let Hooks { mut on_output, on_exit } = hooks;
        let on_exit = Arc::new(Mutex::new(Some(on_exit)));
        let fire_exit = {
            let tx = tx.clone();
            let closed_sent = closed_sent.clone();
            Arc::new(move |why: String| {
                if !closed_sent.swap(true, Ordering::SeqCst) {
                    if let Some(f) = on_exit.lock().ok().and_then(|mut g| g.take()) {
                        f(why.clone());
                    }
                    // A view may be gone already: do not block on a full channel.
                    let _ = tx.try_send(PaneEvent::Closed(why));
                }
            })
        };

        {
            let tx = tx.clone();
            let fire_exit = fire_exit.clone();
            std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let t_read = std::time::Instant::now();
                            // A bad sequence must never kill the reader: log and drop.
                            let chunk = &buf[..n];
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_output(chunk))).is_err() {
                                eprintln!("forgetty: output hook panicked, chunk dropped from scanning");
                            }
                            if tx.send_blocking(PaneEvent::Output(buf[..n].to_vec())).is_err() {
                                return;
                            }
                            if READ_TRACE.load(Ordering::Relaxed) {
                                if let Ok(mut g) = READ_LOG.lock() {
                                    if g.len() < 600 {
                                        g.push((t_read, n, std::time::Instant::now()));
                                    }
                                }
                            }
                        }
                    }
                }
                fire_exit("pty closed".into());
            })?;
        }
        let exited = Arc::new(AtomicBool::new(false));
        {
            // ConPTY often keeps the output pipe open after the child exits.
            let child = child.clone();
            let exited = exited.clone();
            std::thread::Builder::new().name("pty-wait".into()).spawn(move || loop {
                std::thread::sleep(Duration::from_millis(100));
                let status = match child.lock() {
                    Ok(mut c) => c.try_wait(),
                    Err(_) => return,
                };
                match status {
                    Ok(Some(st)) => {
                        exited.store(true, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(300));
                        fire_exit(format!("exited: {st:?}"));
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

        Ok(Self { rx, tx, input_tx, master: Mutex::new(pair.master), child, exited })
    }

    /// Sender into the same bounded channel the PTY reader uses. The smoke test
    /// uses it to push synthetic output through the real pump.
    pub fn injector(&self) -> async_channel::Sender<PaneEvent> {
        self.tx.clone()
    }

    pub fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.rx.clone()
    }

    pub fn write_input(&self, bytes: &[u8]) {
        if READ_TRACE.load(Ordering::Relaxed) {
            if let Ok(mut g) = WRITE_LOG.lock() {
                if g.len() < 300 {
                    g.push((std::time::Instant::now(), bytes.iter().take(24).copied().collect()));
                }
            }
        }
        let _ = self.input_tx.send(bytes.to_vec());
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }

    /// True once the child process ended (checked by the wait thread).
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Poll the child directly. Used by tests to confirm a kill took effect.
    pub fn try_exited(&self) -> bool {
        if self.has_exited() {
            return true;
        }
        match self.child.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(Some(_))),
            Err(_) => true,
        }
    }
}
