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
    reader: Option<Box<dyn IoRead + Send>>,
    writer: Box<dyn IoWrite + Send>,
}

impl PtyProcess {
    /// Spawn a new shell process in a PTY.
    ///
    /// If `command` is `None`, the user's default shell is detected via the
    /// fallback chain: `$SHELL` -> `/etc/passwd` -> `/bin/sh`. The shell is
    /// invoked as a login shell (argv[0] prefixed with `-`).
    ///
    /// If `command` is `Some` and `login_shell` is `true`, the command is
    /// treated as a shell override (e.g., from config.toml) and gets login
    /// shell semantics.
    ///
    /// If `command` is `Some` and `login_shell` is `false`, the command is
    /// run directly without login shell semantics (matching `-e` flag behavior).
    pub fn spawn(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
        login_shell: bool,
    ) -> Result<Self> {
        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(size.into())
            .map_err(|e| ForgettyError::Pty(format!("failed to open PTY: {e}")))?;

        let mut cmd = match command {
            Some(args) if !args.is_empty() => {
                if login_shell {
                    // Config shell override: use new_default_prog() for login
                    // shell semantics. portable-pty reads SHELL from the
                    // builder's env, uses it as the executable, and sets
                    // argv[0] to "-basename" automatically.
                    debug!(shell = %args[0], "using config shell as login shell");
                    let mut cb = CommandBuilder::new_default_prog();
                    cb.env("SHELL", &args[0]);
                    // Note: args beyond [0] are ignored for config shell
                    // overrides (shell path only, no extra args).
                    cb
                } else {
                    // Explicit -e command: run as-is, no login shell.
                    let mut cb = CommandBuilder::new(&args[0]);
                    if args.len() > 1 {
                        cb.args(&args[1..]);
                    }
                    cb
                }
            }
            _ => {
                // No command specified: detect the user's shell and invoke
                // it as a login shell via new_default_prog(). portable-pty
                // reads SHELL from the builder's env and prefixes argv[0]
                // with "-" for login shell semantics.
                let shell = detect_shell();
                debug!(shell = %shell, "using detected shell as login shell");
                let mut cb = CommandBuilder::new_default_prog();
                cb.env("SHELL", &shell);
                cb
            }
        };

        // Set environment variables.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "forgetty");
        // Version derived from workspace Cargo.toml at compile time.
        // If forgetty-pty ever gets its own version, this will diverge from the binary version.
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));

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

        Ok(Self { master: pair.master, child, reader: Some(reader), writer })
    }

    /// Read bytes from the PTY. This is a blocking call.
    ///
    /// # Panics
    ///
    /// Panics if `take_reader()` has been called previously.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.reader
            .as_mut()
            .expect("reader has been taken via take_reader()")
            .read(buf)
            .map_err(ForgettyError::Io)
    }

    /// Take the reader out of this PtyProcess for use in a separate thread.
    ///
    /// After calling this, `read()` will panic. Use the returned reader directly.
    pub fn take_reader(&mut self) -> Option<Box<dyn IoRead + Send>> {
        self.reader.take()
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

    /// Get the foreground process group of the PTY session.
    ///
    /// Calls `tcgetpgrp` on the master PTY fd, which returns the process group
    /// ID of whatever process is currently in the foreground on this terminal.
    /// Returns `None` if the master does not expose a raw fd or `tcgetpgrp`
    /// fails (e.g., no foreground process group set yet).
    pub fn foreground_pgrp(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    /// Get the `VINTR` character (the byte the kernel's line discipline
    /// translates to `SIGINT` when `ISIG` is enabled).
    ///
    /// Reads `c_cc[VINTR]` via `tcgetattr` on the master PTY fd. Returns
    /// `None` if the master fd is unavailable or `tcgetattr` fails — callers
    /// fall back to the POSIX default `0x03`.
    ///
    /// Used by the daemon's Ctrl+C path (FIX-017) to write a byte that
    /// matches the current `VINTR` setting rather than hardcoded `0x03`.
    /// This makes Ctrl+C work for users who have remapped `VINTR` (via
    /// `stty intr <char>` or shell init) and for SSH sessions where the
    /// remote PTY inherits the local `VINTR` through ssh's `pty-modes`.
    #[cfg(unix)]
    pub fn vintr(&self) -> Option<u8> {
        use std::os::unix::io::RawFd;
        let fd: RawFd = self.master.as_raw_fd()?;
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
            return None;
        }
        Some(termios.c_cc[libc::VINTR])
    }

    /// Non-Unix stub: VINTR is a POSIX termios concept and not meaningful
    /// off Unix. Returns `None` so callers use the POSIX default `0x03`.
    #[cfg(not(unix))]
    pub fn vintr(&self) -> Option<u8> {
        None
    }

    /// Kill the child process.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().map_err(|e| ForgettyError::Pty(format!("failed to kill child: {e}")))
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.is_alive() {
            if let Err(e) = self.child.kill() {
                // Best effort -- the process may have already exited between
                // the is_alive() check and the kill() call.
                debug!("PtyProcess::drop: kill failed: {e}");
            }
        }
    }
}

/// Detect the user's default shell.
///
/// Fallback chain (Unix/Linux/macOS):
///   1. `$SHELL` environment variable (if set and the path exists + is executable)
///   2. `/etc/passwd` entry via `getpwuid(getuid())` (if valid and path exists)
///   3. `/bin/sh` (absolute last resort)
///
/// Fallback chain (Android — Bionic libc, no `/etc/passwd`):
///   1. `$SHELL` environment variable (if set and the path exists)
///   2. `/system/bin/sh` (Android system shell)
fn detect_shell() -> String {
    #[cfg(target_os = "android")]
    {
        // Android: Bionic libc does not provide a usable /etc/passwd.
        // Try $SHELL first, then fall back to the Android system shell.
        if let Ok(shell) = std::env::var("SHELL") {
            if !shell.is_empty() {
                let path = Path::new(&shell);
                if path.exists() {
                    debug!(shell = %shell, "using $SHELL");
                    return shell;
                }
                warn!(
                    shell = %shell,
                    "$SHELL points to a nonexistent path, falling back to /system/bin/sh"
                );
            }
        }
        warn!("$SHELL not set on Android, falling back to /system/bin/sh");
        "/system/bin/sh".to_string()
    }

    #[cfg(all(unix, not(target_os = "android")))]
    {
        // Step 1: Try $SHELL environment variable.
        if let Ok(shell) = std::env::var("SHELL") {
            if !shell.is_empty() {
                let path = Path::new(&shell);
                if path.exists() {
                    debug!(shell = %shell, "using $SHELL");
                    return shell;
                }
                warn!(
                    shell = %shell,
                    "$SHELL points to a nonexistent path, trying /etc/passwd"
                );
            }
        }

        // Step 2: Try /etc/passwd via getpwuid(getuid()).
        if let Some(shell) = passwd_shell() {
            let path = Path::new(&shell);
            if path.exists() {
                debug!(shell = %shell, "using shell from /etc/passwd");
                return shell;
            }
            warn!(
                shell = %shell,
                "/etc/passwd shell does not exist, falling back to /bin/sh"
            );
        }

        // Step 3: Last resort.
        warn!("$SHELL not set and /etc/passwd lookup failed, falling back to /bin/sh");
        "/bin/sh".to_string()
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

/// Read the user's login shell from `/etc/passwd` via `getpwuid(getuid())`.
///
/// Returns `None` if the lookup fails or the shell field cannot be read.
/// The `pw_shell` string is copied immediately -- the pointer from `getpwuid`
/// is to a static buffer and must not be held across calls.
///
/// Not compiled on Android: Bionic libc does not expose a usable `/etc/passwd`.
#[cfg(all(unix, not(target_os = "android")))]
fn passwd_shell() -> Option<String> {
    use std::ffi::CStr;

    // SAFETY: getuid() is always safe. getpwuid() returns a pointer to a
    // static buffer (not thread-safe), but detect_shell() is called from the
    // GTK main thread during terminal creation, which is serialized.
    // We copy pw_shell immediately and do not hold the pointer.
    unsafe {
        let uid = libc::getuid();
        let ent = libc::getpwuid(uid);
        if ent.is_null() {
            warn!("getpwuid({uid}) returned null");
            return None;
        }
        let pw_shell = (*ent).pw_shell;
        if pw_shell.is_null() {
            warn!("getpwuid({uid}).pw_shell is null");
            return None;
        }
        match CStr::from_ptr(pw_shell).to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_owned()),
            Ok(_) => {
                warn!("getpwuid({uid}).pw_shell is empty");
                None
            }
            Err(e) => {
                warn!("getpwuid({uid}).pw_shell is not valid UTF-8: {e}");
                None
            }
        }
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
        let mut proc =
            PtyProcess::spawn(default_size(), None, None, true).expect("failed to spawn");
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
            PtyProcess::spawn(default_size(), None, Some(&cmd), false).expect("failed to spawn");

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

    /// FIX-017 round 2: `vintr()` reads the line discipline's interrupt
    /// character from a freshly spawned PTY. Newly created PTYs use the
    /// kernel default termios, where `c_cc[VINTR] == 0x03` (Ctrl+C).
    /// Verifies the syscall plumbing works end-to-end; it doesn't test the
    /// remapping case (that requires `stty intr <other>` and falls under
    /// AC-10's human test).
    #[cfg(unix)]
    #[test]
    fn test_vintr_default_is_etx() {
        let proc = PtyProcess::spawn(default_size(), None, None, true).expect("failed to spawn");
        let vintr = proc.vintr().expect("vintr() should succeed on a fresh PTY");
        assert_eq!(vintr, 0x03, "fresh PTY's VINTR should be 0x03 (Ctrl+C), got {vintr:#04x}");
        drop(proc);
    }

    #[test]
    fn test_resize_no_panic() {
        let proc = PtyProcess::spawn(default_size(), None, None, true).expect("failed to spawn");

        let new_size = PtySize { rows: 48, cols: 120, pixel_width: 0, pixel_height: 0 };
        proc.resize(new_size).expect("resize should not fail");

        // Spawn returns a mutable proc but resize takes &self, so we need to
        // drop cleanly.
        drop(proc);
    }

    #[test]
    fn test_env_vars_set() {
        let cmd = vec!["env".to_string()];
        let mut proc =
            PtyProcess::spawn(default_size(), None, Some(&cmd), false).expect("failed to spawn");

        let mut output = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match proc.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("TERM=xterm-256color"),
            "expected TERM=xterm-256color in env output, got: {output_str}"
        );
        assert!(
            output_str.contains("COLORTERM=truecolor"),
            "expected COLORTERM=truecolor in env output, got: {output_str}"
        );
        assert!(
            output_str.contains("TERM_PROGRAM=forgetty"),
            "expected TERM_PROGRAM=forgetty in env output, got: {output_str}"
        );
        let expected_version = format!("TERM_PROGRAM_VERSION={}", env!("CARGO_PKG_VERSION"));
        assert!(
            output_str.contains(&expected_version),
            "expected {expected_version} in env output, got: {output_str}"
        );
    }

    #[test]
    fn test_kill_terminates() {
        let mut proc =
            PtyProcess::spawn(default_size(), None, None, true).expect("failed to spawn");
        assert!(proc.is_alive());

        proc.kill().expect("kill should succeed");

        // Give it a moment to actually terminate.
        thread::sleep(Duration::from_millis(200));

        assert!(!proc.is_alive(), "process should not be alive after kill");
    }

    #[test]
    fn test_detect_shell_returns_valid_path() {
        let shell = detect_shell();
        assert!(
            Path::new(&shell).exists(),
            "detect_shell() returned '{shell}' which does not exist"
        );
    }

    #[cfg(all(unix, not(target_os = "android")))]
    #[test]
    fn test_passwd_shell_returns_some() {
        // On a normal system, the current user should have a passwd entry.
        let shell = passwd_shell();
        assert!(shell.is_some(), "passwd_shell() returned None on a normal system");
        let shell = shell.unwrap();
        assert!(
            Path::new(&shell).exists(),
            "passwd_shell() returned '{shell}' which does not exist"
        );
    }

    #[test]
    fn test_command_no_login_shell() {
        // When login_shell=false, argv[0] should be the program name as-is.
        let cmd = vec!["/bin/echo".to_string(), "test".to_string()];
        // This exercises the Some(args) + login_shell=false branch.
        // We cannot directly inspect the CommandBuilder, but we verify no crash.
        let proc = PtyProcess::spawn(default_size(), None, Some(&cmd), false);
        assert!(proc.is_ok(), "spawn with login_shell=false should succeed");
        drop(proc);
    }

    #[test]
    fn test_login_shell_spawn_succeeds() {
        // Spawn with login_shell=true and no command (auto-detect).
        // This exercises the new_default_prog() + SHELL env path.
        let proc = PtyProcess::spawn(default_size(), None, None, true);
        assert!(proc.is_ok(), "spawn with login_shell=true should succeed");
        drop(proc);
    }

    #[test]
    fn test_config_shell_override_login() {
        // Spawn with an explicit shell path and login_shell=true.
        // This simulates the config shell override path.
        let shell = detect_shell();
        let cmd = vec![shell];
        let proc = PtyProcess::spawn(default_size(), None, Some(&cmd), true);
        assert!(proc.is_ok(), "spawn with config shell override should succeed");
        drop(proc);
    }
}
