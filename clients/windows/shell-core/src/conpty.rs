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

impl LocalConPty {
    pub fn spawn(argv: &[String], cwd: Option<&str>, rows: u16, cols: u16, hooks: Hooks) -> Result<Self> {
        let (shell, args) = argv.split_first().context("empty command")?;
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty (ConPTY)")?;

        let build = |cwd: Option<&str>| {
            let mut cmd = CommandBuilder::new(shell);
            for a in args {
                cmd.arg(a);
            }
            if let Some(cwd) = cwd {
                cmd.cwd(cwd);
            }
            cmd.env("TERM", "xterm-256color");
            cmd
        };
        // Never enter a UNC/device/NT-namespace path: on Windows that can send NTLM credentials
        // to a server named by program output. The folder is not stat-ed here (this runs on the
        // UI thread, and a slow or mapped drive would stall a frame): the process start checks
        // it, and a folder that is gone is started without one, as before.
        let safe_cwd = cwd.filter(|c| crate::osc::is_safe_local_path(c));
        let child = match safe_cwd {
            Some(c) => match pair.slave.spawn_command(build(Some(c))) {
                Ok(child) => child,
                Err(_) => pair.slave.spawn_command(build(None)).with_context(|| format!("spawn {shell}"))?,
            },
            None => pair.slave.spawn_command(build(None)).with_context(|| format!("spawn {shell}"))?,
        };
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
                            let kept = strip_win32_input_mode(&mut buf[..n]);
                            if kept == 0 {
                                continue;
                            }
                            let n = kept;
                            // A bad sequence must never kill the reader: log and drop.
                            let chunk = &buf[..n];
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_output(chunk))).is_err() {
                                eprintln!("forgetty: output hook panicked, chunk dropped from scanning");
                            }
                            if tx.send_blocking(PaneEvent::Output(buf[..n].to_vec())).is_err() {
                                return;
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

    /// Process id of the shell, `None` once it is gone.
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
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

/// ConPTY starts its output with `CSI ? 9001 h` (win32-input-mode). The terminal never sends
/// win32 key records, and libghostty-vt logs "unimplemented mode: 9001" for it, so the
/// sequence is cut from the stream here. It arrives whole in the first read; one split
/// across reads is not held back (holding a lone ESC could stall real output). Returns the
/// new length of `buf`.
fn strip_win32_input_mode(buf: &mut [u8]) -> usize {
    const SEQ: [&[u8]; 2] = [b"\x1b[?9001h", b"\x1b[?9001l"];
    let (mut r, mut w) = (0, 0);
    while r < buf.len() {
        if buf[r] == 0x1b && SEQ.iter().any(|s| buf[r..].starts_with(s)) {
            r += SEQ[0].len();
        } else {
            buf[w] = buf[r];
            w += 1;
            r += 1;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::strip_win32_input_mode;

    fn strip(input: &[u8]) -> Vec<u8> {
        let mut v = input.to_vec();
        let n = strip_win32_input_mode(&mut v);
        v.truncate(n);
        v
    }

    #[test]
    fn cuts_mode_9001_only() {
        assert_eq!(strip(b"\x1b[?9001h\x1b[?25lhi\x1b[?9001l!"), b"\x1b[?25lhi!");
        assert_eq!(strip(b"\x1b[?9001h\x1b[?9001h"), b"");
        assert_eq!(strip(b"\x1b[?90010h plain \x1b[?9001"), b"\x1b[?90010h plain \x1b[?9001");
    }
}
