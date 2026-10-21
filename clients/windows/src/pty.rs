//! Local ConPTY shell via portable-pty. The reader runs on its own thread and
//! hands raw byte chunks to the UI thread over an async channel.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct Pty {
    pub writer: SharedWriter,
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub rx: async_channel::Receiver<Vec<u8>>,
}

pub fn spawn(shell: &str, cwd: Option<&str>, rows: u16, cols: u16) -> Result<Pty> {
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
    let writer = pair.master.take_writer().context("take writer")?;

    let (tx, rx) = async_channel::unbounded::<Vec<u8>>();
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send_blocking(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .context("spawn reader thread")?;

    Ok(Pty { writer: Arc::new(Mutex::new(writer)), master: pair.master, child, rx })
}
