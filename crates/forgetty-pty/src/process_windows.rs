//! PTY process on Windows: ConPTY, written against the `windows` crate.
//!
//! Same public surface as the Unix `process` module (`PtyProcess`, `PtySize`), so the session
//! crate does not know the difference. What differs from `portable-pty`:
//!
//! - The pseudo console is created without `PSEUDOCONSOLE_INHERIT_CURSOR` (conhost would wait for
//!   an answer to a cursor query that nothing here sends) and without
//!   `PSEUDOCONSOLE_WIN32_INPUT_MODE` (the byte log and the clients must see plain VT bytes).
//!   `CSI ? 9001 h/l` is still cut from the output, as a guard.
//! - Every pane has a Job Object with `KILL_ON_JOB_CLOSE`. Closing the pane ends the shell and
//!   everything it started. If the server dies, the handle closes and the panes die with it.
//!   [`join_server_job`] adds one job for the server process itself.
//! - ConPTY keeps the output pipe open after the child exits. A thread waits on the process
//!   handle and, shortly after the exit, closes the pseudo console. The reader then gets end of
//!   file, which is what the session crate takes as "the pane ended".
//! - Ctrl+C is the byte `0x03` on the input pipe. The console turns it into the control event for
//!   programs that read input with `ENABLE_PROCESSED_INPUT`, and a program in raw mode reads it
//!   as a key.

use std::ffi::{c_void, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forgetty_core::error::{ForgettyError, Result};
use tracing::{debug, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    GetProcessInformation, InitializeProcThreadAttributeList, ProcessPowerThrottling,
    ResumeThread, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// `PSEUDOCONSOLE_RESIZE_QUIRK`: the host does not reflow its own buffer on a resize. The
/// terminal behind the pipe (libghostty-vt) does the reflow.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

/// The `CreatePseudoConsole` flags `FORGETTY_CONPTY_FLAGS` may add: `PSEUDOCONSOLE_INHERIT_CURSOR`
/// (0x1), `PSEUDOCONSOLE_WIN32_INPUT_MODE` (0x4) and `PSEUDOCONSOLE_PASSTHROUGH_MODE` (0x8).
const EXTRA_CONPTY_FLAGS_ALLOWED: u32 = 0x1 | 0x4 | 0x8;

/// Parse the value of `FORGETTY_CONPTY_FLAGS` (decimal or `0x` hex). Bits that are not allowed
/// are dropped and an invalid value gives none; both are returned with a warning text.
fn parse_extra_conpty_flags(value: &str) -> (u32, Option<String>) {
    let v = value.trim();
    if v.is_empty() {
        return (0, None);
    }
    let parsed = match v.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => v.parse(),
    };
    match parsed {
        Err(_) => (0, Some(format!("FORGETTY_CONPTY_FLAGS={v:?} is not a number; ignored"))),
        Ok(n) if n & !EXTRA_CONPTY_FLAGS_ALLOWED != 0 => (
            n & EXTRA_CONPTY_FLAGS_ALLOWED,
            Some(format!(
                "FORGETTY_CONPTY_FLAGS={v:?}: only 0x1, 0x4 and 0x8 are allowed; using {:#x}",
                n & EXTRA_CONPTY_FLAGS_ALLOWED
            )),
        ),
        Ok(n) => (n, None),
    }
}

/// Extra `CreatePseudoConsole` flags for experiments (`FORGETTY_CONPTY_FLAGS`, or-ed onto the
/// default). `0x4` is `PSEUDOCONSOLE_WIN32_INPUT_MODE`, `0x1` is `PSEUDOCONSOLE_INHERIT_CURSOR`
/// (portable-pty sets both). Unset: none. Read once.
fn extra_conpty_flags() -> u32 {
    static FLAGS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *FLAGS.get_or_init(|| {
        let Ok(v) = std::env::var("FORGETTY_CONPTY_FLAGS") else { return 0 };
        let (flags, warning) = parse_extra_conpty_flags(&v);
        if let Some(w) = warning {
            warn!("{w}");
        }
        flags
    })
}

/// Where the power throttling (EcoQoS, "efficiency mode") opt-out applies
/// (`FORGETTY_POWER_THROTTLING`): `off` (the default) leaves the system to decide, `server` opts
/// the server process out, `shells` also opts every shell process out right after it is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerThrottling {
    Off,
    Server,
    Shells,
}

/// The setting of `FORGETTY_POWER_THROTTLING`.
pub fn power_throttling_mode() -> PowerThrottling {
    match std::env::var("FORGETTY_POWER_THROTTLING").unwrap_or_default().as_str() {
        "server" => PowerThrottling::Server,
        "shells" => PowerThrottling::Shells,
        _ => PowerThrottling::Off,
    }
}

/// Turn execution speed throttling off for `process` (a control bit with its state bit clear),
/// then read the setting back. Returns `(ControlMask, StateMask)` as the system reports them.
pub fn opt_out_of_power_throttling(process: HANDLE) -> std::result::Result<(u32, u32), String> {
    let set = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: `set` is a valid PROCESS_POWER_THROTTLING_STATE of the size passed.
    unsafe {
        SetProcessInformation(
            process,
            ProcessPowerThrottling,
            &set as *const _ as *const c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| format!("SetProcessInformation: {e}"))?;
    let mut got = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    // SAFETY: `got` is a valid out buffer of the size passed.
    unsafe {
        GetProcessInformation(
            process,
            ProcessPowerThrottling,
            &mut got as *mut _ as *mut c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| format!("GetProcessInformation: {e}"))?;
    Ok((got.ControlMask, got.StateMask))
}

/// How long after the process exit the pseudo console stays open, so conhost can write the last
/// output (the prompt, the text of a failing command) into the pipe.
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// The exit code `GetExitCodeProcess` returns for a running process.
const STILL_ACTIVE: u32 = 259;

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

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// A handle that is closed on drop.
struct OwnedHandle(HANDLE);

// SAFETY: a kernel handle is a plain value that any thread may use.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is ours and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn win_err(what: &str, e: windows::core::Error) -> ForgettyError {
    ForgettyError::Pty(format!("{what}: {e}"))
}

/// The pseudo console. Closed once, by whoever gets there first.
struct PseudoConsole {
    hpc: HPCON,
    closed: AtomicBool,
}

// SAFETY: HPCON is a handle value; the functions used on it are thread safe.
unsafe impl Send for PseudoConsole {}
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ForgettyError::Pty("failed to resize PTY: the console is closed".into()));
        }
        let size = coord(rows, cols);
        // SAFETY: `hpc` is a live pseudo console handle.
        unsafe { ResizePseudoConsole(self.hpc, size) }
            .map_err(|e| win_err("failed to resize PTY", e))
    }

    /// `ClosePseudoConsole`. On older Windows 10 builds it can wait until the output pipe is
    /// drained, so callers run it on a thread that may block.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            // SAFETY: closed exactly once, guarded by the flag.
            unsafe { ClosePseudoConsole(self.hpc) };
        }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        self.close();
    }
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD { X: cols.clamp(1, i16::MAX as u16) as i16, Y: rows.clamp(1, i16::MAX as u16) as i16 }
}

// ---------------------------------------------------------------------------
// Reader and writer
// ---------------------------------------------------------------------------

/// Output side of the pseudo console. End of file is a broken pipe.
struct PipeReader(OwnedHandle);

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut n = 0u32;
            // SAFETY: `buf` is valid for the call; the handle is a synchronous pipe.
            let r = unsafe { ReadFile(self.0.raw(), Some(buf), Some(&mut n), None) };
            match r {
                Ok(()) => {
                    let kept = strip_win32_input_mode(&mut buf[..n as usize]);
                    if kept > 0 {
                        return Ok(kept);
                    }
                    // The chunk held only the sequence: read on. Zero would read as end of file.
                    if n == 0 {
                        return Ok(0);
                    }
                }
                Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => return Ok(0),
                Err(e) => return Err(io::Error::from_raw_os_error(win32_code(&e))),
            }
        }
    }
}

/// Input side of the pseudo console.
struct PipeWriter(OwnedHandle);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut n = 0u32;
        // SAFETY: `buf` is valid for the call; the handle is a synchronous pipe.
        unsafe { WriteFile(self.0.raw(), Some(buf), Some(&mut n), None) }.map_err(|e| {
            let code = win32_code(&e);
            // `CancelSynchronousIo` ends a write that waits on a full pipe this way. The pane
            // writer reads `Interrupted` as "look at the abort flag and go on or stop", the
            // same as a signal on Unix.
            if code == ERROR_OPERATION_ABORTED.0 as i32 {
                io::Error::from(io::ErrorKind::Interrupted)
            } else {
                io::Error::from_raw_os_error(code)
            }
        })?;
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The Win32 error code inside a `windows::core::Error` made from `GetLastError`.
fn win32_code(e: &windows::core::Error) -> i32 {
    // HRESULT_FROM_WIN32: 0x8007xxxx.
    let h = e.code().0 as u32;
    if h & 0xFFFF_0000 == 0x8007_0000 {
        (h & 0xFFFF) as i32
    } else {
        h as i32
    }
}

/// ConPTY starts its output with `CSI ? 9001 h` (win32-input-mode) when that mode is asked for.
/// The mode is not asked for here, so this is a guard against a host that sends it anyway: the
/// sequence is cut. It arrives whole in the first read; one split across reads is not held back
/// (holding a lone ESC could stall real output). Returns the new length of `buf`.
pub(crate) fn strip_win32_input_mode(buf: &mut [u8]) -> usize {
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

// ---------------------------------------------------------------------------
// Command line, environment, program lookup
// ---------------------------------------------------------------------------

/// Append `arg` to a command line, quoted the way `CommandLineToArgvW` and the C runtime read it.
fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) -> Result<()> {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(ForgettyError::Pty("an argument holds a NUL character".into()));
    }
    let quote = wide.is_empty() || wide.iter().any(|&c| c == b' ' as u16 || c == b'\t' as u16);
    if quote {
        cmd.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else {
            if c == b'"' as u16 {
                // Double the backslashes before a quote, and escape the quote.
                cmd.extend(std::iter::repeat_n(b'\\' as u16, backslashes + 1));
            }
            backslashes = 0;
        }
        cmd.push(c);
    }
    if quote {
        cmd.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        cmd.push(b'"' as u16);
    }
    Ok(())
}

fn command_line(program: &Path, args: &[String]) -> Result<Vec<u16>> {
    let mut line = Vec::new();
    append_arg(&mut line, program.as_os_str())?;
    for a in args {
        line.push(b' ' as u16);
        append_arg(&mut line, OsStr::new(a))?;
    }
    line.push(0);
    Ok(line)
}

/// The environment block of the child: this process's variables plus `extra`, which win.
///
/// A name with `=` or NUL, an empty name, and a NUL in a value are refused: the block is a list of
/// `NAME=value` strings ended by NUL, so such a variable would split into other variables or end
/// the block early (a remote client can send extra environment through the server).
fn environment_block(extra: &[(String, String)]) -> std::result::Result<Vec<u16>, String> {
    for (k, v) in extra {
        if k.is_empty() || k.contains('=') || k.contains('\0') {
            return Err(format!("environment variable name {k:?} is not valid"));
        }
        if v.contains('\0') {
            return Err(format!("the value of environment variable {k:?} holds a NUL"));
        }
    }
    // `extra` in order, a later entry replacing an earlier one with the same name.
    let mut merged: Vec<(&str, &str)> = Vec::new();
    for (k, v) in extra {
        merged.retain(|(have, _)| !have.eq_ignore_ascii_case(k));
        merged.push((k, v));
    }
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (k, _) in &merged {
        vars.retain(|(have, _)| !have.to_string_lossy().eq_ignore_ascii_case(k));
    }
    vars.extend(merged.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))));
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn pathext() -> Vec<String> {
    std::env::var("PATHEXT")
        .ok()
        .map(|v| v.split(';').filter(|e| e.starts_with('.')).map(str::to_string).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()])
}

/// File names to try for `name` in one folder: `name` itself if it already ends in a PATHEXT
/// extension, then `name` with each extension.
fn candidates(name: &str, exts: &[String]) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let mut v = Vec::new();
    if exts.iter().any(|e| lower.ends_with(&e.to_ascii_lowercase())) {
        v.push(name.to_string());
    }
    v.extend(exts.iter().map(|e| format!("{name}{e}")));
    v
}

/// Why `program` may not be started, or `Ok` when its form is allowed. The server starts what a
/// client sends, so only two forms are let through:
///
/// - a bare name (`pwsh`, `ssh.exe`), which is searched in PATH, and
/// - an absolute path on a local drive (`C:\tools\x.exe`, `C:/tools/x.exe`).
///
/// Refused: a path with a folder part that is relative to the current folder (`.\x.exe`,
/// `sub\x.exe`, `..\x.exe`), a path rooted without a drive (`\x.exe`), a drive-relative path
/// (`C:x.exe`), a network path (`\\server\share\x.exe`), the device and extended forms
/// (`\\?\C:\x.exe`, `\\.\pipe\x`, `\??\C:\x.exe`) and any colon besides the one after the
/// drive letter (an NTFS stream, `x.exe:stream`).
pub(crate) fn check_program_form(program: &str) -> std::result::Result<(), String> {
    if program.contains('\0') {
        return Err("the command holds a NUL character".into());
    }
    let b = program.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        return Err(format!(
            "{program}: network, device and extended paths (\\\\server\\..., \\\\?\\..., \\\\.\\...) are not allowed"
        ));
    }
    if program.starts_with("\\??\\") || program.starts_with("/??/") {
        return Err(format!("{program}: NT object paths (\\??\\...) are not allowed"));
    }
    if !program.contains(['\\', '/', ':']) {
        return Ok(());
    }
    // From here on the form has a folder part or a colon: only `X:\...` and `X:/...` pass.
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2]) {
        if program[2..].contains(':') {
            return Err(format!("{program}: a colon in the path (an NTFS stream?) is not allowed"));
        }
        return Ok(());
    }
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(format!("{program}: a path relative to the current folder of a drive (C:x) is not allowed"));
    }
    if sep(b[0]) {
        return Err(format!("{program}: a path without a drive letter is not allowed"));
    }
    if program.contains(':') {
        return Err(format!("{program}: a colon in the command is not allowed"));
    }
    Err(format!(
        "{program}: a relative path is not allowed; use a bare program name (searched in PATH) or a full path with a drive letter"
    ))
}

/// Characters that `cmd.exe` reads as syntax in the arguments of a batch file, whatever the
/// quoting: `& | < > ^ % ! " ( )` and line breaks.
const CMD_META: &[char] = &['&', '|', '<', '>', '^', '%', '!', '"', '(', ')', '\r', '\n'];

/// A `.bat` or `.cmd` file runs inside `cmd.exe`, which parses its arguments again, with its own
/// rules that the argument quoting of `CreateProcess` does not cover. So such a target is only
/// started when no argument holds a character `cmd.exe` treats as syntax.
pub(crate) fn check_batch_args(program: &Path, args: &[String]) -> std::result::Result<(), String> {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(['.', ' ']).to_ascii_lowercase())
        .unwrap_or_default();
    if !(name.ends_with(".bat") || name.ends_with(".cmd")) {
        return Ok(());
    }
    for a in args {
        if let Some(c) = a.chars().find(|c| CMD_META.contains(c)) {
            let shown = if c == '\r' || c == '\n' { "a line break".to_string() } else { format!("`{c}`") };
            return Err(format!(
                "{}: a batch file is started by cmd.exe, which reads {shown} in an argument as syntax; \
                 remove it or start a program (an .exe) instead",
                program.display()
            ));
        }
    }
    Ok(())
}

/// The file `program` names. A bare name is searched in the PATH folders only (never in the
/// current folder, where `CreateProcess` would look first), each with the PATHEXT endings. Other
/// forms must pass [`check_program_form`].
fn resolve_program(program: &str) -> Result<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return Err(ForgettyError::Pty("the command is empty".into()));
    }
    check_program_form(program).map_err(ForgettyError::Pty)?;
    let exts = pathext();
    if program.contains(['\\', '/']) {
        let p = Path::new(program);
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        for c in candidates(program, &exts) {
            if Path::new(&c).is_file() {
                return Ok(PathBuf::from(c));
            }
        }
        return Err(ForgettyError::Pty(format!("program not found: {program}")));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            // An empty or relative entry means "somewhere under the current folder".
            if !dir.is_absolute() {
                continue;
            }
            for c in candidates(program, &exts) {
                let full = dir.join(&c);
                if full.is_file() {
                    return Ok(full);
                }
            }
        }
    }
    Err(ForgettyError::Pty(format!("program not found in PATH: {program}")))
}

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

/// The shell from the config file, as an argv. Set by the server at start.
static CONFIG_SHELL: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Name the shell that panes without a command start (`shell = "..."` in `config.toml`). `None`
/// goes back to detection.
pub fn set_default_shell(argv: Option<Vec<String>>) {
    *CONFIG_SHELL.lock().unwrap_or_else(|e| e.into_inner()) = argv.filter(|a| !a.is_empty());
}

/// Split the `shell` setting into an argument list: white space separates, `"..."` and `'...'`
/// group, a backslash is a plain character (so `C:\Program Files\...` needs no doubling) except
/// `\"` inside double quotes. A quote that is never closed is an error. (The same rules as the
/// Windows client's `shell-core` `argv::split`, so one config line means the same in both.)
pub(crate) fn split_command_line(s: &str) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' && chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    have = true;
                }
                c if c.is_whitespace() => {
                    if have || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        have = false;
                    }
                }
                c => {
                    cur.push(c);
                    have = true;
                }
            },
        }
    }
    if let Some(q) = quote {
        return Err(format!("a {q} quote is not closed"));
    }
    if have || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Take the `shell` setting of `config.toml` (a command line) as the default shell. A blank
/// setting goes back to detection. A setting that cannot be read is logged and ignored.
pub fn set_default_shell_from_config(shell: Option<&str>) {
    match shell.map(str::trim).filter(|s| !s.is_empty()).map(split_command_line) {
        None => set_default_shell(None),
        Some(Ok(argv)) => set_default_shell(Some(argv)),
        Some(Err(e)) => {
            warn!("config: `shell` is ignored ({e})");
            set_default_shell(None);
        }
    }
}

/// The argv of the shell a pane starts without a command: the configured shell, `FORGETTY_SHELL`,
/// then PowerShell 7, Windows PowerShell and `%COMSPEC%`. Programs are found by walking PATH, with
/// no helper process (`where` shows a console window).
pub fn default_shell_argv() -> Vec<String> {
    if let Some(argv) = CONFIG_SHELL.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return argv;
    }
    if let Some(s) = std::env::var("FORGETTY_SHELL").ok().filter(|s| !s.trim().is_empty()) {
        return vec![s];
    }
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Ok(p) = resolve_program(name) {
            return vec![p.to_string_lossy().into_owned()];
        }
    }
    if let Some(c) = std::env::var("COMSPEC").ok().filter(|c| !c.is_empty()) {
        return vec![c];
    }
    vec!["cmd.exe".to_string()]
}

// ---------------------------------------------------------------------------
// The server's own job
// ---------------------------------------------------------------------------

static SERVER_JOB: std::sync::OnceLock<OwnedHandle> = std::sync::OnceLock::new();

fn new_kill_on_close_job() -> io::Result<OwnedHandle> {
    // SAFETY: plain Win32 calls with valid arguments.
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| io::Error::from_raw_os_error(win32_code(&e)))?;
        let job = OwnedHandle(job);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // BREAKAWAY_OK: a program may start a child outside the job on purpose (a launcher that
        // wants its app to outlive the pane). Without the flag `CREATE_BREAKAWAY_FROM_JOB` fails.
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| io::Error::from_raw_os_error(win32_code(&e)))?;
        Ok(job)
    }
}

/// Put the server process in a Job Object that kills its members when the last handle closes
/// (that is, when the server dies). The panes' own jobs are nested under it. Call once at start.
/// An error is not fatal: each pane job still kills its processes when the server's handles
/// close.
pub fn join_server_job() -> io::Result<()> {
    if SERVER_JOB.get().is_some() {
        return Ok(());
    }
    let job = new_kill_on_close_job()?;
    // SAFETY: the pseudo handle of the current process is valid for this call.
    unsafe { AssignProcessToJobObject(job.raw(), GetCurrentProcess()) }
        .map_err(|e| io::Error::from_raw_os_error(win32_code(&e)))?;
    let _ = SERVER_JOB.set(job);
    Ok(())
}

// ---------------------------------------------------------------------------
// PtyProcess
// ---------------------------------------------------------------------------

/// A PTY-backed child process.
pub struct PtyProcess {
    console: Arc<PseudoConsole>,
    process: Arc<OwnedHandle>,
    /// `None` if the job could not be made or joined: tree kill degrades to the shell alone.
    job: Option<OwnedHandle>,
    pid: u32,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
}

/// The attribute list that hands the pseudo console to `CreateProcessW`.
struct AttributeList {
    buf: Vec<u8>,
}

impl AttributeList {
    fn with_pseudo_console(hpc: HPCON) -> Result<Self> {
        let mut size = 0usize;
        // SAFETY: the first call only reports the size and fails with "insufficient buffer".
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        }
        let mut buf = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut c_void);
        // SAFETY: `buf` has the size the first call asked for and outlives the list.
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .map_err(|e| win_err("InitializeProcThreadAttributeList", e))?;
        let this = Self { buf };
        // SAFETY: the value is the HPCON itself, passed by value as the API wants.
        let r = unsafe {
            UpdateProcThreadAttribute(
                this.as_list(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )
        };
        r.map_err(|e| win_err("UpdateProcThreadAttribute", e))?;
        Ok(this)
    }

    fn as_list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_ptr() as *mut c_void)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised in `with_pseudo_console`.
        unsafe { DeleteProcThreadAttributeList(self.as_list()) };
    }
}

fn create_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    // SAFETY: out pointers are valid; default security, default buffer size.
    unsafe { CreatePipe(&mut r, &mut w, None, 0) }.map_err(|e| win_err("CreatePipe", e))?;
    Ok((OwnedHandle(r), OwnedHandle(w)))
}

impl PtyProcess {
    /// Spawn a new shell process in a pseudo console.
    ///
    /// If `command` is `None` (or empty), the default shell starts ([`default_shell_argv`]).
    /// `login_shell` has no meaning on Windows: the command always runs as it is written, with
    /// all its arguments.
    pub fn spawn(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
        login_shell: bool,
    ) -> Result<Self> {
        Self::spawn_with_env(size, working_dir, command, login_shell, &[])
    }

    /// [`spawn`](Self::spawn) with extra environment variables for the child. They are merged
    /// into the environment block after the defaults, so they win (names compare without regard
    /// to case, as Windows does).
    pub fn spawn_with_env(
        size: PtySize,
        working_dir: Option<&Path>,
        command: Option<&[String]>,
        login_shell: bool,
        extra_env: &[(String, String)],
    ) -> Result<Self> {
        let _ = login_shell;
        let argv: Vec<String> = match command {
            Some(c) if !c.is_empty() => c.to_vec(),
            _ => default_shell_argv(),
        };
        crate::timer_boost::note_activity();
        let program = resolve_program(&argv[0])?;
        check_batch_args(&program, &argv[1..]).map_err(ForgettyError::Pty)?;
        let mut cmdline = command_line(&program, &argv[1..])?;
        let mut vars: Vec<(String, String)> = vec![
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("TERM_PROGRAM".into(), "forgetty".into()),
            // Version derived from the workspace Cargo.toml at compile time.
            ("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
        ];
        vars.extend(extra_env.iter().cloned());
        let env = environment_block(&vars).map_err(ForgettyError::Pty)?;
        // A folder that is not there: start in the home folder, as the Unix PTY does.
        let cwd: Option<Vec<u16>> = working_dir
            .filter(|d| d.is_dir())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .map(|d| d.as_os_str().encode_wide().chain(std::iter::once(0)).collect());
        let app: Vec<u16> = program.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

        // The pipes: the child's console reads `in_r` and writes `out_w`. We keep `in_w`, `out_r`.
        let (in_r, in_w) = create_pipe()?;
        let (out_r, out_w) = create_pipe()?;
        // SAFETY: handles are valid; flags 0x2 only (see the module comment).
        let hpc = unsafe {
            CreatePseudoConsole(
                coord(size.rows, size.cols),
                in_r.raw(),
                out_w.raw(),
                PSEUDOCONSOLE_RESIZE_QUIRK | extra_conpty_flags(),
            )
        }
        .map_err(|e| win_err("failed to open PTY (ConPTY)", e))?;
        let console = Arc::new(PseudoConsole { hpc, closed: AtomicBool::new(false) });
        // The console host has its own copies of these two ends.
        drop(in_r);
        drop(out_w);

        let attrs = AttributeList::with_pseudo_console(hpc)?;
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Stdio handles are invalid on purpose: a child must not inherit a redirected stdout of
        // this process (a server started with its output in a log file).
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        si.lpAttributeList = attrs.as_list();

        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: every pointer is valid for the call. The process starts suspended, so it can
        // be put in its job before it runs and starts children.
        let created = unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                Some(env.as_ptr() as *const c_void),
                cwd.as_ref().map(|c| PCWSTR(c.as_ptr())).unwrap_or(PCWSTR::null()),
                &si.StartupInfo,
                &mut pi,
            )
        };
        drop(attrs);
        if let Err(e) = created {
            return Err(ForgettyError::Pty(format!(
                "failed to spawn process {}: {e}",
                program.display()
            )));
        }
        let thread = OwnedHandle(pi.hThread);
        let process = Arc::new(OwnedHandle(pi.hProcess));
        // The shell is still suspended: the opt-out is in place before its first instruction.
        if power_throttling_mode() == PowerThrottling::Shells {
            match opt_out_of_power_throttling(process.raw()) {
                Ok((control, state)) => tracing::info!(
                    pid = pi.dwProcessId,
                    "shell power throttling: ControlMask {control:#x}, StateMask {state:#x} read back"
                ),
                Err(e) => warn!(pid = pi.dwProcessId, "shell power throttling opt-out failed: {e}"),
            }
        }

        let job = match new_kill_on_close_job() {
            Ok(job) => {
                // SAFETY: both handles are valid.
                match unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } {
                    Ok(()) => Some(job),
                    Err(e) => {
                        warn!("PTY: cannot put the shell in a job object ({e}); a tree kill only ends the shell");
                        None
                    }
                }
            }
            Err(e) => {
                warn!("PTY: cannot create a job object ({e}); a tree kill only ends the shell");
                None
            }
        };
        // SAFETY: the thread handle is valid and the thread is suspended.
        if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
            let err = io::Error::last_os_error();
            // SAFETY: valid process handle.
            unsafe {
                let _ = TerminateProcess(process.raw(), 1);
            }
            return Err(ForgettyError::Pty(format!("failed to start the process: {err}")));
        }
        drop(thread);

        let pid = pi.dwProcessId;
        debug!(pid, program = %program.display(), "spawned PTY process");

        // The exit watcher.
        {
            let (process, console) = (Arc::clone(&process), Arc::clone(&console));
            std::thread::Builder::new()
                .name("conpty-exit".into())
                .spawn(move || {
                    // SAFETY: the Arc keeps the handle open.
                    unsafe { WaitForSingleObject(process.raw(), INFINITE) };
                    std::thread::sleep(EXIT_GRACE);
                    console.close();
                })
                .map_err(|e| ForgettyError::Pty(format!("failed to start the exit watcher: {e}")))?;
        }

        Ok(Self {
            console,
            process,
            job,
            pid,
            reader: Some(Box::new(PipeReader(out_r))),
            writer: Some(Box::new(PipeWriter(in_w))),
        })
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
    pub fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    /// Write bytes to the PTY. Blocks while the pipe is full (a program that does not read).
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| ForgettyError::Pty("PTY writer has been taken".to_string()))?;
        w.write_all(data).map_err(ForgettyError::Io)?;
        w.flush().map_err(ForgettyError::Io)
    }

    /// Take the writer out of this PtyProcess, for a writer thread.
    pub fn take_writer(&mut self) -> Option<Box<dyn Write + Send>> {
        self.writer.take()
    }

    /// Resize the PTY.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.console.resize(size.rows, size.cols)
    }

    fn exit_status(&self) -> Option<u32> {
        // SAFETY: valid process handle.
        unsafe {
            if WaitForSingleObject(self.process.raw(), 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process.raw(), &mut code).ok()?;
            Some(code)
        }
    }

    /// Check if the child process is still running.
    pub fn is_alive(&mut self) -> bool {
        self.exit_status().is_none()
    }

    /// Exit code of the child if it has already exited, without blocking.
    pub fn exit_code(&mut self) -> Option<u32> {
        self.exit_status()
    }

    /// Wait for the process to exit and return its exit code.
    pub fn wait(&mut self) -> Result<u32> {
        // SAFETY: valid process handle.
        unsafe {
            WaitForSingleObject(self.process.raw(), INFINITE);
            let mut code = STILL_ACTIVE;
            GetExitCodeProcess(self.process.raw(), &mut code)
                .map_err(|e| win_err("failed to wait on child", e))?;
            Ok(code)
        }
    }

    /// Get the child process ID, if available.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    /// Windows has no foreground process group.
    pub fn foreground_pgrp(&self) -> Option<i32> {
        None
    }

    /// The interrupt setting `(isig, interrupt byte)`. On Windows the pseudo console turns
    /// `0x03` into Ctrl+C for a program that has processed input on, and delivers it as a key
    /// to one that has not (the raw mode case). So the answer is always "byte 3", and the caller
    /// writes it and signals nothing.
    pub fn intr_setting(&self) -> Option<(bool, u8)> {
        Some((true, 0x03))
    }

    /// The byte that interrupts the foreground program (see [`Self::intr_setting`]).
    pub fn vintr(&self) -> Option<u8> {
        match self.intr_setting()? {
            (true, intr) if intr != 0 => Some(intr),
            _ => None,
        }
    }

    /// Throw away unread input. There is no call for it on a pseudo console. Best effort: none.
    pub fn flush_pending_input(&self) {}

    /// The terminal's line settings. Windows has no termios, so there is nothing to read: the
    /// session code treats `None` as "unknown".
    pub fn tty_flags(&self) -> Option<(bool, bool)> {
        None
    }

    /// Wait until the shell has ended, at most `ms` milliseconds. True when it ended.
    ///
    /// `kill` returns at once, and `TerminateProcess` finishes asynchronously: the exit code reads
    /// as still active for a moment. This call blocks the thread, so async code runs it in
    /// `spawn_blocking`.
    pub fn wait_ended(&self, ms: u32) -> bool {
        // SAFETY: valid process handle.
        unsafe { WaitForSingleObject(self.process.raw(), ms) == WAIT_OBJECT_0 }
    }

    /// End the shell and everything it started. Does not wait: see [`Self::wait_ended`].
    pub fn kill(&mut self) -> Result<()> {
        self.kill_tree();
        Ok(())
    }

    fn kill_tree(&self) {
        // SAFETY: valid handles. Errors mean "already gone".
        unsafe {
            if let Some(job) = &self.job {
                let _ = TerminateJobObject(job.raw(), 1);
            }
            let _ = TerminateProcess(self.process.raw(), 1);
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        // The whole tree ends with the pane, the shell alive or not (a child of a shell that
        // exited is still ours). The job handle closes right after and would do it as well.
        self.kill_tree();
        // `ClosePseudoConsole` may wait for the output to be read on older Windows: not on the
        // thread that drops a pane.
        let console = Arc::clone(&self.console);
        let _ = std::thread::Builder::new().name("conpty-close".into()).spawn(move || console.close());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn extra_conpty_flags_are_masked_and_a_bad_value_is_refused() {
        use super::parse_extra_conpty_flags as parse;
        assert_eq!(parse(""), (0, None));
        assert_eq!(parse("0x4"), (4, None));
        assert_eq!(parse("5"), (5, None));
        assert_eq!(parse(" 0x1 "), (1, None));
        let (f, w) = parse("0x7");
        assert_eq!(f, 5);
        assert!(w.unwrap().contains("allowed"));
        let (f, w) = parse("0x1f");
        assert_eq!(f, 0xd);
        assert!(w.is_some());
        let (f, w) = parse("nope");
        assert_eq!(f, 0);
        assert!(w.unwrap().contains("not a number"));
    }

    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    /// The configured shell is global: the tests that set it take turns.
    static SHELL_SETTING: Mutex<()> = Mutex::new(());

    fn size() -> PtySize {
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }

    fn cmd_exe() -> Vec<String> {
        vec!["cmd.exe".to_string()]
    }

    /// Output of a pane, read on a thread: a channel of chunks, closed at end of file.
    struct Output {
        rx: mpsc::Receiver<Vec<u8>>,
        text: Vec<u8>,
        eof: bool,
    }

    impl Output {
        fn start(proc: &mut PtyProcess) -> Self {
            let mut reader = proc.take_reader().expect("reader");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Self { rx, text: Vec::new(), eof: false }
        }

        /// Read until `pred(text)` holds. Panics with the text so far after `secs`.
        fn until(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> String {
            let end = Instant::now() + Duration::from_secs(secs);
            loop {
                let s = String::from_utf8_lossy(&self.text).into_owned();
                if pred(&s) {
                    return s;
                }
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() || self.eof {
                    panic!("timed out (eof: {}) with output: {s:?}", self.eof);
                }
                match self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                    Ok(chunk) => self.text.extend(chunk),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => self.eof = true,
                }
            }
        }

        /// Read until the reader reports end of file.
        fn until_eof(&mut self, secs: u64) -> String {
            let end = Instant::now() + Duration::from_secs(secs);
            loop {
                let left = end.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "no end of file; output: {:?}", String::from_utf8_lossy(&self.text));
                match self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                    Ok(chunk) => self.text.extend(chunk),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.eof = true;
                        return String::from_utf8_lossy(&self.text).into_owned();
                    }
                }
            }
        }
    }

    /// `set /a` prints a sum the typed line does not hold, so seeing the sum proves the shell
    /// ran the line and not only echoed it.
    fn marker_line(n: u32) -> (String, String) {
        (format!("set /a 70000+{n}\r\n"), format!("{}", 70000 + n))
    }

    #[test]
    fn spawn_a_shell_and_echo_a_marker() {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        assert!(p.pid().is_some());
        let mut out = Output::start(&mut p);
        let (line, want) = marker_line(1);
        p.write(line.as_bytes()).unwrap();
        let text = out.until(20, |t| t.contains(&want));
        // No win32-input-mode request reaches the reader.
        assert!(!text.contains("\x1b[?9001"), "{text:?}");
        assert!(p.is_alive());
        p.kill().unwrap();
    }

    #[test]
    fn the_terminal_environment_is_set() {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"echo T=%TERM%;P=%TERM_PROGRAM%;V=%TERM_PROGRAM_VERSION%;C=%COLORTERM%\r\n").unwrap();
        let want = format!("T=xterm-256color;P=forgetty;V={};C=truecolor", env!("CARGO_PKG_VERSION"));
        out.until(20, |t| t.contains(&want));
    }

    #[test]
    fn the_environment_block_refuses_names_and_values_that_would_split_it() {
        let ok = |k: &str, v: &str| environment_block(&[(k.to_string(), v.to_string())]);
        assert!(ok("GOOD_NAME", "a value with spaces and = signs").is_ok());
        assert!(ok("", "x").is_err(), "empty name");
        assert!(ok("A=B", "x").is_err(), "= in a name");
        assert!(ok("A\0B", "x").is_err(), "NUL in a name");
        assert!(ok("A", "x\0Y=z").is_err(), "NUL in a value");
        // The refusal reaches the caller of spawn.
        let bad = [("X=Y".to_string(), "1".to_string())];
        assert!(PtyProcess::spawn_with_env(size(), None, Some(&cmd_exe()), false, &bad).is_err());
    }

    #[test]
    fn extra_env_reaches_the_child_and_wins_over_the_defaults() {
        let extra = [
            ("FORGETTY_TEST_EXTRA".to_string(), "seen-by-pane".to_string()),
            // The names are not case sensitive: this replaces the default TERM_PROGRAM.
            ("term_program".to_string(), "other".to_string()),
        ];
        let mut p = PtyProcess::spawn_with_env(size(), None, Some(&cmd_exe()), false, &extra)
            .expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"echo X=%FORGETTY_TEST_EXTRA%;P=%TERM_PROGRAM%\r\n").unwrap();
        out.until(20, |t| t.contains("X=seen-by-pane;P=other"));
        assert_eq!(p.tty_flags(), None);
    }

    #[test]
    fn resize_changes_the_console_size() {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        p.resize(PtySize { rows: 40, cols: 100, pixel_width: 0, pixel_height: 0 }).expect("resize");
        // `mode con` prints the size of the console the shell sees.
        p.write(b"mode con\r\n").unwrap();
        let text = out.until(20, |t| t.contains("Columns:") && t.contains("Lines:"));
        let number_after = |key: &str| -> u32 {
            let rest = &text[text.rfind(key).unwrap() + key.len()..];
            rest.trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
        };
        assert_eq!(number_after("Columns:"), 100, "{text:?}");
        assert_eq!(number_after("Lines:"), 40, "{text:?}");
    }

    #[test]
    fn an_idle_shell_that_runs_exit_is_seen_as_ended() {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        // Wait for the prompt first: the shell is idle, not starting.
        let (line, want) = marker_line(2);
        p.write(line.as_bytes()).unwrap();
        out.until(20, |t| t.contains(&want));
        p.write(b"exit\r\n").unwrap();
        // The reader must reach end of file although ConPTY would keep the pipe open.
        out.until_eof(15);
        let t0 = Instant::now();
        while p.is_alive() {
            assert!(t0.elapsed() < Duration::from_secs(5), "still alive after end of file");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.exit_code(), Some(0));
        assert_eq!(p.wait().unwrap(), 0);
    }

    #[test]
    fn a_nonzero_exit_code_is_reported() {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"exit 7\r\n").unwrap();
        out.until_eof(15);
        assert_eq!(p.wait().unwrap(), 7);
    }

    /// The first process named `exe` (lowercase) whose parent is `parent`.
    fn find_child(parent: u32, exe: &str) -> Option<u32> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        // SAFETY: a snapshot handle and an entry struct with its size set.
        unsafe {
            let snap = OwnedHandle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut more = Process32FirstW(snap.raw(), &mut e).is_ok();
            while more {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                if e.th32ParentProcessID == parent && name == exe {
                    return Some(e.th32ProcessID);
                }
                more = Process32NextW(snap.raw(), &mut e).is_ok();
            }
            None
        }
    }

    fn open_for_wait(pid: u32) -> OwnedHandle {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // SAFETY: plain call.
        OwnedHandle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("open the grandchild"))
    }

    fn ended(h: &OwnedHandle, secs: u64) -> bool {
        // SAFETY: valid handle.
        unsafe { WaitForSingleObject(h.raw(), (secs * 1000) as u32) == WAIT_OBJECT_0 }
    }

    /// A shell that starts `ping -t` (a grandchild of the pane). Returns the pane and the ping.
    fn pane_with_grandchild() -> (PtyProcess, Output, OwnedHandle) {
        let mut p = PtyProcess::spawn(size(), None, Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"ping -t 127.0.0.1\r\n").unwrap();
        out.until(20, |t| t.contains("Reply from"));
        let ping = find_child(p.pid().unwrap(), "ping.exe").expect("ping.exe is a child of the shell");
        (p, out, open_for_wait(ping))
    }

    #[test]
    fn closing_the_pane_ends_the_whole_tree() {
        let (p, _out, ping) = pane_with_grandchild();
        assert!(!ended(&ping, 0), "ping runs while the pane is open");
        drop(p);
        assert!(ended(&ping, 10), "the grandchild is gone after the pane is dropped");
    }

    #[test]
    fn kill_ends_the_whole_tree() {
        let (mut p, _out, ping) = pane_with_grandchild();
        p.kill().unwrap();
        assert!(ended(&ping, 10), "the grandchild is gone after kill");
        assert!(p.wait_ended(10_000), "the shell ended");
        assert!(!p.is_alive());
    }

    #[test]
    fn ctrl_c_as_0x03_interrupts_the_foreground_program_and_not_the_shell() {
        let (mut p, mut out, ping) = pane_with_grandchild();
        assert_eq!(p.vintr(), Some(0x03));
        p.write(&[0x03]).unwrap();
        assert!(ended(&ping, 15), "ping ended on Ctrl+C");
        // The shell lives and takes the next line.
        assert!(p.is_alive());
        let (line, want) = marker_line(3);
        p.write(line.as_bytes()).unwrap();
        out.until(20, |t| t.contains(&want));
        p.kill().unwrap();
    }

    #[test]
    fn a_program_that_is_not_there_is_an_error() {
        let cmd = vec!["definitely-not-a-program-forgetty.exe".to_string()];
        let err = PtyProcess::spawn(size(), None, Some(&cmd), false).err().expect("an error");
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn a_missing_start_folder_falls_back_to_the_home_folder() {
        let mut p = PtyProcess::spawn(
            size(),
            Some(Path::new(r"C:\forgetty-no-such-folder-for-the-test")),
            Some(&cmd_exe()),
            false,
        )
        .expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"echo CWD[%CD%]\r\n").unwrap();
        let home = std::env::var("USERPROFILE").unwrap();
        out.until(20, |t| t.to_lowercase().contains(&format!("cwd[{}]", home.to_lowercase())));
    }

    #[test]
    fn the_start_folder_is_used() {
        let dir = std::env::temp_dir();
        let mut p = PtyProcess::spawn(size(), Some(&dir), Some(&cmd_exe()), false).expect("spawn");
        let mut out = Output::start(&mut p);
        p.write(b"cd\r\n").unwrap();
        let want = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
        // The temp folder may come as a short (8.3) name; compare the last folder name.
        let last = want.rsplit('\\').next().unwrap().to_string();
        let _ = out.until(20, |t| t.to_lowercase().contains(&last));
    }

    #[test]
    fn default_shell_is_a_program_on_this_machine() {
        let argv = default_shell_argv();
        assert!(resolve_program(&argv[0]).is_ok(), "{argv:?}");
    }

    #[test]
    fn the_configured_shell_wins_and_none_goes_back_to_detection() {
        let _turn = SHELL_SETTING.lock().unwrap_or_else(|e| e.into_inner());
        set_default_shell(Some(vec!["cmd.exe".into(), "/k".into()]));
        assert_eq!(default_shell_argv(), vec!["cmd.exe".to_string(), "/k".to_string()]);
        set_default_shell(None);
        assert_ne!(default_shell_argv(), vec!["cmd.exe".to_string(), "/k".to_string()]);
    }

    #[test]
    fn the_shell_setting_is_split_like_the_client_does() {
        let sp = |s: &str| split_command_line(s).unwrap();
        assert_eq!(sp("pwsh.exe -NoLogo"), ["pwsh.exe", "-NoLogo"]);
        assert_eq!(sp(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#), [r"C:\Program Files\PowerShell\7\pwsh.exe", "-NoLogo"]);
        assert_eq!(sp("  "), Vec::<String>::new());
        assert_eq!(sp(r#"a "" b"#), ["a", "", "b"]);
        assert!(split_command_line("echo 'oops").is_err());
    }

    #[test]
    fn the_shell_setting_sets_the_default_and_a_bad_one_is_ignored() {
        let _turn = SHELL_SETTING.lock().unwrap_or_else(|e| e.into_inner());
        set_default_shell_from_config(Some("cmd.exe /k"));
        assert_eq!(default_shell_argv(), ["cmd.exe", "/k"]);
        set_default_shell_from_config(Some("cmd.exe 'broken"));
        assert_ne!(default_shell_argv(), ["cmd.exe", "/k"]);
        set_default_shell_from_config(None);
    }

    #[test]
    fn quoting_follows_the_c_runtime_rules() {
        let line = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let v = command_line(Path::new(r"C:\Program Files\x.exe"), &args).unwrap();
            String::from_utf16(&v[..v.len() - 1]).unwrap()
        };
        assert_eq!(line(&[]), r#""C:\Program Files\x.exe""#);
        assert_eq!(line(&["a b", "", r"c\d", r#"e"f"#, r"g h\"]), r#""C:\Program Files\x.exe" "a b" "" c\d e\"f "g h\\""#);
    }

    #[test]
    fn program_forms_that_are_not_a_bare_name_or_a_drive_path_are_refused() {
        for ok in ["pwsh", "ssh.exe", r"C:\tools\x.exe", "C:/tools/x.exe", r"d:\a b\c.exe"] {
            assert!(check_program_form(ok).is_ok(), "{ok}");
        }
        let refused = [
            r".\x.exe",
            r"..\x.exe",
            r"sub\x.exe",
            "sub/x.exe",
            "./x.exe",
            r"\x.exe",
            "/x.exe",
            r"\\server\share\x.exe",
            "//server/share/x.exe",
            r"\\?\C:\x.exe",
            r"\\.\pipe\x",
            r"\??\C:\x.exe",
            "C:x.exe",
            "C:",
            r"C:\x.exe:stream",
            "x.exe:stream",
            "a\0b.exe",
        ];
        for bad in refused {
            assert!(check_program_form(bad).is_err(), "{bad:?} should be refused");
            assert!(resolve_program(bad).is_err(), "{bad:?} should not resolve");
        }
        let err = check_program_form(r"\\server\share\x.exe").unwrap_err();
        assert!(err.contains("network"), "{err}");
    }

    #[test]
    fn a_spawn_with_a_refused_form_is_an_error() {
        for bad in [r".\cmd.exe", r"\\?\C:\Windows\System32\cmd.exe", "C:cmd.exe"] {
            let cmd = vec![bad.to_string()];
            assert!(PtyProcess::spawn(size(), None, Some(&cmd), false).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_batch_file_with_cmd_syntax_in_an_argument_is_refused() {
        let bat = Path::new(r"C:\x\run.bat");
        let cmdf = Path::new(r"C:\x\RUN.CMD");
        for c in ["&", "|", "<", ">", "^", "%", "!", "\"", "(", ")", "\n", "\r"] {
            let args = vec![format!("a{c}b")];
            let err = check_batch_args(bat, &args).unwrap_err();
            assert!(err.contains("cmd.exe"), "{err}");
            assert!(check_batch_args(cmdf, &args).is_err(), "{c:?}");
        }
        // Plain arguments pass, and an .exe takes anything.
        assert!(check_batch_args(bat, &["one".into(), "two words".into(), "-x=1".into()]).is_ok());
        assert!(check_batch_args(Path::new(r"C:\x\run.exe"), &["a&b".into()]).is_ok());
        // A trailing dot or space does not hide the extension.
        assert!(check_batch_args(Path::new(r"C:\x\run.bat."), &["a&b".into()]).is_err());
    }

    #[test]
    fn a_bare_name_is_not_taken_from_the_current_folder() {
        // `resolve_program` only walks absolute PATH entries.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("forgetty-only-here.exe"), b"").unwrap();
        let prior = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let r = resolve_program("forgetty-only-here.exe");
        std::env::set_current_dir(prior).unwrap();
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn mode_9001_is_cut_from_the_output() {
        let strip = |input: &[u8]| {
            let mut v = input.to_vec();
            let n = strip_win32_input_mode(&mut v);
            v.truncate(n);
            v
        };
        assert_eq!(strip(b"\x1b[?9001h\x1b[?25lhi\x1b[?9001l!"), b"\x1b[?25lhi!");
        assert_eq!(strip(b"\x1b[?9001h\x1b[?9001h"), b"");
        assert_eq!(strip(b"\x1b[?90010h plain \x1b[?9001"), b"\x1b[?90010h plain \x1b[?9001");
    }
}
