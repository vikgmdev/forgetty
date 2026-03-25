//! PTY process spawning and lifecycle management.
//!
//! Handles creating pseudoterminal pairs, spawning shell or command processes,
//! reading output, writing input, and detecting process exit.

use std::io::{Read as IoRead, Write as IoWrite};
use std::path::Path;

use forgetty_core::error::{ForgettyError, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};
use tracing::{debug, warn};

/// PTY dimensions.
#[derive(Debug, Clone, Copy)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }
}

impl From<PtySize> for portable_pty::PtySize {
    fn from(s: PtySize) -> Self {
        portable_pty::PtySize {
            rows: s.rows,
            cols: s.cols,
            pixel_width: s.pixel_width,
            pixel_height: s.pixel_height,
        }
    }
}

/// A PTY-backed child process.
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn IoRead + Send>,
    writer: Box<dyn IoWrite + Send>,
}

impl PtyProcess {
    /// Spawn a new shell process in a PTY.
    ///
    /// If `command` is `None`, the user's default shell is detected from
    /// the `$SHELL` environment variable (Unix) or PowerShell/cmd (Windows).
    pub fn spawn(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(size.into())
            .map_err(|e| ForgettyError::Pty(format!("failed to open PTY: {e}")))?;

        let mut cmd = match command {
            Some(args) if !args.is_empty() => {
                let mut cb = CommandBuilder::new(&args[0]);
                if args.len() > 1 {
                    cb.args(&args[1..]);
                }
                cb
            }
            _ => {
                let shell = detect_shell();
                debug!(shell = %shell, "using shell");
                CommandBuilder::new(shell)
            }
        };

        // Set environment variables.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "forgetty");

        if let Some(dir) = working_dir {
            cmd.cwd(dir);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ForgettyError::Pty(format!("failed to spawn process: {e}")))?;

        debug!(pid = ?child.process_id(), "spawned PTY process");

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ForgettyError::Pty(format!("failed to clone PTY reader: {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ForgettyError::Pty(format!("failed to take PTY writer: {e}")))?;

        Ok(Self { master: pair.master, child, reader, writer })
    }

    /// Read bytes from the PTY. This is a blocking call.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.reader.read(buf).map_err(ForgettyError::Io)
    }

    /// Write bytes to the PTY.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data).map_err(ForgettyError::Io)?;
        self.writer.flush().map_err(ForgettyError::Io)
    }

    /// Resize the PTY.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master
            .resize(size.into())
            .map_err(|e| ForgettyError::Pty(format!("failed to resize PTY: {e}")))
    }

    /// Check if the child process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait for the process to exit and return its exit code.
    pub fn wait(&mut self) -> Result<u32> {
        let status = self
            .child
            .wait()
            .map_err(|e| ForgettyError::Pty(format!("failed to wait on child: {e}")))?;
        Ok(status.exit_code())
    }

    /// Get the child process ID, if available.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Kill the child process.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().map_err(|e| ForgettyError::Pty(format!("failed to kill child: {e}")))
    }
}

/// Detect the user's default shell.
fn detect_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| {
            warn!("$SHELL not set, falling back to /bin/sh");
            "/bin/sh".to_string()
        })
    }

    #[cfg(windows)]
    {
        // Prefer PowerShell if available.
        if which_exists("pwsh.exe") {
            "pwsh.exe".to_string()
        } else if which_exists("powershell.exe") {
            "powershell.exe".to_string()
        } else {
            "cmd.exe".to_string()
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        "sh".to_string()
    }
}

#[cfg(windows)]
fn which_exists(name: &str) -> bool {
    std::process::Command::new("where")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn default_size() -> PtySize {
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }

    #[test]
    fn test_spawn_and_read() {
        let mut proc = PtyProcess::spawn(default_size(), None, None).expect("failed to spawn");
        assert!(proc.pid().is_some());

        // Give the shell a moment to start and produce output.
        thread::sleep(Duration::from_millis(500));

        // Check that the process is alive.
        assert!(proc.is_alive());

        proc.kill().ok();
    }

    #[test]
    fn test_write_and_read_echo() {
        // Spawn `echo` directly as the command to avoid interactive shell issues.
        let cmd = vec!["echo".to_string(), "hello_forgetty_test".to_string()];
        let mut proc =
            PtyProcess::spawn(default_size(), None, Some(&cmd)).expect("failed to spawn");

        // Read output — the process runs `echo` and exits, so we read until EOF.
        let mut output = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match proc.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("hello_forgetty_test"),
            "expected output to contain 'hello_forgetty_test', got: {output_str}"
        );
    }

    #[test]
    fn test_resize_no_panic() {
        let proc = PtyProcess::spawn(default_size(), None, None).expect("failed to spawn");

        let new_size = PtySize { rows: 48, cols: 120, pixel_width: 0, pixel_height: 0 };
        proc.resize(new_size).expect("resize should not fail");

        // Spawn returns a mutable proc but resize takes &self, so we need to
        // drop cleanly.
        drop(proc);
    }

    #[test]
    fn test_kill_terminates() {
        let mut proc = PtyProcess::spawn(default_size(), None, None).expect("failed to spawn");
        assert!(proc.is_alive());

        proc.kill().expect("kill should succeed");

        // Give it a moment to actually terminate.
        thread::sleep(Duration::from_millis(200));

        assert!(!proc.is_alive(), "process should not be alive after kill");
    }
}
