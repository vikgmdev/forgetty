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

/// What a pane's process needs from more than one thread. The process is started on a worker
/// thread (see [`Pending::run`]) while the pane already exists, so each part is filled in when
/// the start finishes.
struct Shared {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
    /// The size the pane wants. The process starts at the latest value; a resize that comes
    /// before the start only updates this.
    size: Mutex<(u16, u16)>,
    /// `kill` was called. A start that finishes after it ends the new process at once.
    killed: AtomicBool,
    exited: Arc<AtomicBool>,
}

pub struct LocalConPty {
    rx: async_channel::Receiver<PaneEvent>,
    tx: async_channel::Sender<PaneEvent>,
    /// Input typed before the process exists waits in this channel, and goes out in order once
    /// the writer thread starts.
    input_tx: mpsc::Sender<Vec<u8>>,
    shared: Arc<Shared>,
}

/// The part of a pane that starts the process. Made with [`LocalConPty::begin`], run on a worker
/// thread.
pub struct Pending {
    shared: Arc<Shared>,
    tx: async_channel::Sender<PaneEvent>,
    input_rx: mpsc::Receiver<Vec<u8>>,
    hooks: Hooks,
}

/// The error text of a start that was cancelled because the pane was closed while it started.
/// It is not a failure to tell the user about.
pub const CANCELLED: &str = "closed while starting";

/// How a start went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    /// Index in the folder list of the folder the process started in. `None`: none of them
    /// could be entered, or the list was empty.
    pub folder: Option<usize>,
}

impl LocalConPty {
    /// A pane whose process is not started yet. Its channels work at once: a view can attach and
    /// type, and everything waits for [`Pending::run`].
    pub fn begin(rows: u16, cols: u16, hooks: Hooks) -> (Self, Pending) {
        let (tx, rx) = async_channel::bounded::<PaneEvent>(256);
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let shared = Arc::new(Shared {
            master: Mutex::new(None),
            child: Arc::new(Mutex::new(None)),
            size: Mutex::new((rows, cols)),
            killed: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
        });
        let pending = Pending { shared: shared.clone(), tx: tx.clone(), input_rx, hooks };
        (Self { rx, tx, input_tx, shared }, pending)
    }
}

impl Pending {
    /// The pane was closed (or the app is ending) before the process started.
    pub fn is_killed(&self) -> bool {
        self.shared.killed.load(Ordering::SeqCst)
    }

    /// The start could not even begin (no thread for it). Same effect as a failed `run`.
    pub fn fail(self, why: &str) -> String {
        self.shared.exited.store(true, Ordering::SeqCst);
        let msg = why.to_string();
        let _ = self.tx.try_send(PaneEvent::Output(format!("\x1b[31mforgetty: could not start the shell: {msg}\x1b[0m\r\n").into_bytes()));
        let _ = self.tx.try_send(PaneEvent::Closed(format!("could not start: {msg}")));
        msg
    }

    /// Open the pseudo console and start the process. This blocks (ConPTY, `CreateProcess`, the
    /// folder checks inside portable-pty), so it runs on a worker thread, never on the UI thread.
    ///
    /// `folders` are tried in order as the start folder. A folder the process cannot start in is
    /// skipped; the result says which one worked. If none works the process starts without one.
    /// On an error the pane gets the message as text and a `Closed` event, and the error is
    /// returned.
    pub fn run(self, argv: &[String], folders: &[String]) -> Result<Started, String> {
        let shared = self.shared.clone();
        let tx = self.tx.clone();
        match self.start(argv, folders) {
            Ok(s) => Ok(s),
            Err(e) => {
                let msg = format!("{e:#}");
                shared.exited.store(true, Ordering::SeqCst);
                if msg == CANCELLED {
                    // The pane is gone or the app is ending: nobody needs a message.
                    let _ = tx.try_send(PaneEvent::Closed(CANCELLED.into()));
                    return Err(msg);
                }
                let _ = tx.try_send(PaneEvent::Output(format!("\x1b[31mforgetty: could not start the shell: {msg}\x1b[0m\r\n").into_bytes()));
                let _ = tx.try_send(PaneEvent::Closed(format!("could not start: {msg}")));
                Err(msg)
            }
        }
    }

    fn start(self, argv: &[String], folders: &[String]) -> Result<Started> {
        let Pending { shared, tx, input_rx, hooks } = self;
        let (shell, args) = argv.split_first().context("empty command")?;
        let cancelled = || shared.killed.load(Ordering::SeqCst);
        // A pane closed while it waited for its turn never starts its program.
        if cancelled() {
            anyhow::bail!(CANCELLED);
        }
        let (rows, cols) = *shared.size.lock().unwrap_or_else(|e| e.into_inner());
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty (ConPTY)")?;

        let profile_fix = {
            let var = |k: &str| std::env::var(k).ok();
            profile_override(var("USERPROFILE").as_deref(), var("HOMEDRIVE").as_deref(), var("HOMEPATH").as_deref(), var("SystemDrive").as_deref())
        };
        let build = |cwd: Option<&str>| {
            let mut cmd = CommandBuilder::new(shell);
            for a in args {
                cmd.arg(a);
            }
            if let Some(cwd) = cwd {
                cmd.cwd(cwd);
            }
            cmd.env("TERM", "xterm-256color");
            // portable-pty stats USERPROFILE on every start. When it is a share, give the child
            // a local one instead, so no start touches the network.
            if let Some(p) = &profile_fix {
                cmd.env("USERPROFILE", p);
            }
            cmd
        };
        // The caller checked the folders for UNC/device/NT-namespace paths (entering one can send
        // NTLM credentials to a server named by program output). The stat below can be slow on a
        // dead mapped drive, so it runs here on the worker thread. It is also needed: portable-pty
        // starts in the home folder, without an error, when the folder is not a directory, and the
        // caller must know that the wanted folder was not used.
        let mut child = None;
        let mut folder = None;
        for (i, f) in folders.iter().enumerate() {
            if cancelled() {
                anyhow::bail!(CANCELLED);
            }
            if !std::path::Path::new(f).is_dir() {
                continue;
            }
            if let Ok(c) = pair.slave.spawn_command(build(Some(f))) {
                child = Some(c);
                folder = Some(i);
                break;
            }
        }
        if child.is_none() && cancelled() {
            anyhow::bail!(CANCELLED);
        }
        let child = match child {
            Some(c) => c,
            None => pair.slave.spawn_command(build(None)).with_context(|| format!("spawn {shell}"))?,
        };
        drop(pair.slave);

        // From here the process exists: publish it, so a `kill` that came earlier (or comes now)
        // reaches it.
        *shared.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let rest = (|| -> Result<()> {
            if shared.killed.load(Ordering::SeqCst) {
                if let Some(c) = shared.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = c.kill();
                }
            }
            let mut reader = pair.master.try_clone_reader().context("clone reader")?;
            let mut writer = pair.master.take_writer().context("take writer")?;
            {
                // A resize that came while the process was starting.
                let mut m = shared.master.lock().unwrap_or_else(|e| e.into_inner());
                let want = *shared.size.lock().unwrap_or_else(|e| e.into_inner());
                if want != (rows, cols) {
                    let _ = pair.master.resize(PtySize { rows: want.0, cols: want.1, pixel_width: 0, pixel_height: 0 });
                }
                *m = Some(pair.master);
            }

            let closed_sent = Arc::new(AtomicBool::new(false));
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
            {
                // ConPTY often keeps the output pipe open after the child exits.
                let child = shared.child.clone();
                let exited = shared.exited.clone();
                std::thread::Builder::new().name("pty-wait".into()).spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let status = match child.lock() {
                        Ok(mut c) => match c.as_mut() {
                            Some(c) => c.try_wait(),
                            None => return,
                        },
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
            // Everything typed so far is already queued: it goes out first, in order.
            std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
                while let Ok(bytes) = input_rx.recv() {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            })?;
            Ok(())
        })();
        if let Err(e) = rest {
            if let Some(c) = shared.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                let _ = c.kill();
            }
            return Err(e);
        }
        Ok(Started { folder })
    }
}

impl LocalConPty {
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
        *self.shared.size.lock().unwrap_or_else(|e| e.into_inner()) = (rows, cols);
        if let Some(m) = self.shared.master.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    pub fn kill(&self) {
        self.shared.killed.store(true, Ordering::SeqCst);
        if let Some(c) = self.shared.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = c.kill();
        }
    }

    /// True once the child process ended (checked by the wait thread), or could not start.
    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::SeqCst)
    }

    /// Process id of the shell, `None` while it is starting and once it is gone.
    pub fn pid(&self) -> Option<u32> {
        self.shared.child.lock().ok().and_then(|c| c.as_ref().and_then(|c| c.process_id()))
    }

    /// Poll the child directly. Used by tests to confirm a kill took effect. A pane that was
    /// killed before its process started counts as ended: the start ends the process at once.
    pub fn try_exited(&self) -> bool {
        if self.has_exited() {
            return true;
        }
        match self.shared.child.lock() {
            Ok(mut c) => match c.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => self.shared.killed.load(Ordering::SeqCst),
            },
            Err(_) => true,
        }
    }
}

/// A local `USERPROFILE` for the child when the inherited one is a network or device path (the
/// start would stat it). `None`: the inherited value is fine, or there is none.
pub fn profile_override(userprofile: Option<&str>, homedrive: Option<&str>, homepath: Option<&str>, systemdrive: Option<&str>) -> Option<String> {
    let up = userprofile.filter(|u| !u.is_empty())?;
    if crate::osc::is_safe_local_path(up) {
        return None;
    }
    let home = format!("{}{}", homedrive.unwrap_or(""), homepath.unwrap_or(""));
    if !home.is_empty() && crate::osc::is_safe_local_path(&home) {
        return Some(home);
    }
    let sys = format!("{}\\", systemdrive.unwrap_or("C:").trim_end_matches('\\'));
    Some(if crate::osc::is_safe_local_path(&sys) { sys } else { "C:\\".into() })
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
    #[test]
    fn a_profile_on_a_share_is_replaced_for_the_child() {
        use super::profile_override as f;
        assert_eq!(f(Some(r"C:\Users\v"), None, None, None), None);
        assert_eq!(f(None, None, None, None), None);
        assert_eq!(f(Some(r"\\srv\share\v"), Some("D:"), Some(r"\Users\v"), Some("C:")), Some(r"D:\Users\v".into()));
        assert_eq!(f(Some(r"\\srv\share\v"), None, None, Some("E:")), Some(r"E:\".into()));
        assert_eq!(f(Some(r"\\?\UNC\srv\share"), Some(r"\\srv"), Some(r"\x"), None), Some(r"C:\".into()));
        assert_eq!(f(Some(r"\??\UNC\srv\share"), None, None, None), Some(r"C:\".into()));
    }

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
