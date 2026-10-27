//! The CLI's side of the control pipe on Windows: a blocking stream with read and write timeouts,
//! shaped like `std::os::unix::net::UnixStream` (`connect`, `try_clone`, `set_read_timeout`,
//! `set_write_timeout`, `Read`, `Write`), so the CLI code is the same on both systems.
//!
//! Before the first byte is sent the client checks who made and who runs the server on the other
//! end of the pipe:
//!
//! - the owner of the pipe object (`GetSecurityInfo` on the handle) must be the current user;
//! - the user SID of the server process (`GetNamedPipeServerProcessId`) must be the current user;
//! - the integrity level of the server process must not be lower than the client's own, so a
//!   normal process cannot stand in for a server when the client runs elevated.
//!
//! A process of another user that got the pipe name first cannot receive what the CLI sends (the
//! counterpart of the server's check of its clients). The connection also asks Windows for the identification
//! impersonation level only, so the server can read the client's identity but not act as it.

use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forgetty_core::fsutil::acl;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, HANDLE, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

/// How long `connect` waits for a busy pipe (all instances taken for a moment).
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// No timeout.
const NONE: u64 = u64::MAX;

struct Inner {
    handle: HANDLE,
    read_ms: AtomicU64,
    write_ms: AtomicU64,
}

// SAFETY: a kernel handle is a plain value; overlapped calls on it from several threads are
// allowed, each with its own OVERLAPPED.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A connection to the control pipe. Clones share the connection and its timeouts.
#[derive(Clone)]
pub struct PipeStream {
    inner: Arc<Inner>,
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn os_err(e: windows::core::Error) -> io::Error {
    let h = e.code().0 as u32;
    if h & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((h & 0xFFFF) as i32)
    } else {
        io::Error::other(e.to_string())
    }
}

fn timeout_ms(d: Option<Duration>) -> u64 {
    match d {
        // A zero timeout is an error on Unix; here it would poll. Keep the same rule.
        Some(d) if d.is_zero() => 1,
        Some(d) => d.as_millis().min(u128::from(u32::MAX - 1)) as u64,
        None => NONE,
    }
}

impl PipeStream {
    /// Connect to the pipe at `path` and check that its server runs as this user.
    ///
    /// `NotFound` when no server has created the pipe.
    pub fn connect(path: &Path) -> io::Result<Self> {
        Self::connect_as_level(path, None)
    }

    /// [`connect`](Self::connect), judging the server against `own_level` (an integrity RID)
    /// instead of the level of this process. For tests: the level of a client above the server.
    pub fn connect_as_level(path: &Path, own_level: Option<u32>) -> io::Result<Self> {
        let name = wide(path);
        let deadline = Instant::now() + BUSY_WAIT;
        let handle = loop {
            // SAFETY: `name` is NUL-terminated and lives for the call.
            let r = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match r {
                Ok(h) => break h,
                Err(e) => {
                    let code = e.code();
                    if code == ERROR_FILE_NOT_FOUND.to_hresult() {
                        return Err(io::Error::new(io::ErrorKind::NotFound, os_err(e)));
                    }
                    if code == ERROR_PIPE_BUSY.to_hresult() && Instant::now() < deadline {
                        // Wait for an instance to be free (the server makes the next one at
                        // once), then try again.
                        // SAFETY: as above.
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 500) };
                        continue;
                    }
                    return Err(os_err(e));
                }
            }
        };
        let stream = Self {
            inner: Arc::new(Inner {
                handle,
                read_ms: AtomicU64::new(NONE),
                write_ms: AtomicU64::new(NONE),
            }),
        };
        stream.check_server(own_level)?;
        Ok(stream)
    }

    /// The pipe must be owned by this user and the server must be a process of this user, at an
    /// integrity level at least as high as ours.
    fn check_server(&self, own_level: Option<u32>) -> io::Result<()> {
        let refuse = |why: String| io::Error::new(io::ErrorKind::PermissionDenied, why);
        let me = acl::current_user_sid()?;
        let owner = acl::object_owner_sid(self.inner.handle)
            .map_err(|e| refuse(format!("cannot read who owns the pipe: {e}")))?;
        if !acl::same_account(&me, &owner) {
            return Err(refuse(format!("the pipe is owned by another user ({owner})")));
        }
        let mut pid = 0u32;
        // SAFETY: valid pipe handle and out pointer.
        unsafe { GetNamedPipeServerProcessId(self.inner.handle, &mut pid) }.map_err(os_err)?;
        let my_level = match own_level {
            Some(l) => l,
            None => acl::current_integrity_rid()?,
        };
        let (theirs, their_level) = acl::process_identity(pid)
            .map_err(|e| refuse(format!("cannot tell who runs the server (pid {pid}): {e}")))?;
        if !acl::same_account(&me, &theirs) {
            return Err(refuse(format!("the pipe server (pid {pid}) runs as another user ({theirs})")));
        }
        levels_match(my_level, their_level)
            .map_err(|why| refuse(format!("the pipe server (pid {pid}) {why}")))
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.inner.read_ms.store(timeout_ms(d), Ordering::SeqCst);
        Ok(())
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.inner.write_ms.store(timeout_ms(d), Ordering::SeqCst);
        Ok(())
    }

    /// Run one overlapped read or write and wait for it, up to `ms`.
    fn transfer(
        &self,
        ms: u64,
        start: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        // SAFETY: the event and the OVERLAPPED live until the operation is over: on a timeout
        // the call is cancelled and waited for before returning.
        unsafe {
            let event = CreateEventW(None, true, false, PCWSTR::null()).map_err(os_err)?;
            let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
            let result = (|| {
                match start(&mut ov) {
                    Ok(()) => {}
                    Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                        let wait = if ms == NONE { INFINITE } else { ms as u32 };
                        if WaitForSingleObject(event, wait) == WAIT_TIMEOUT {
                            let _ = CancelIoEx(self.inner.handle, Some(&ov as *const OVERLAPPED));
                            // Wait for the cancel to finish: `ov` and the buffer must not be
                            // used by the kernel after this function returns. The call may
                            // also have completed in the meantime.
                            let mut n = 0u32;
                            return match GetOverlappedResult(self.inner.handle, &ov, &mut n, true) {
                                Ok(()) => Ok(n as usize),
                                Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {
                                    Err(io::Error::from(io::ErrorKind::TimedOut))
                                }
                                Err(e) => Err(os_err(e)),
                            };
                        }
                    }
                    Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => return Ok(0),
                    Err(e) => return Err(os_err(e)),
                }
                let mut n = 0u32;
                match GetOverlappedResult(self.inner.handle, &ov, &mut n, false) {
                    Ok(()) => Ok(n as usize),
                    Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
                    Err(e) => Err(os_err(e)),
                }
            })();
            let _ = CloseHandle(event);
            result
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Read for &PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let h = self.inner.handle;
        let ms = self.inner.read_ms.load(Ordering::SeqCst);
        self.transfer(ms, |ov| {
            // SAFETY: `buf` and `ov` outlive the operation (see `transfer`).
            unsafe { ReadFile(h, Some(&mut *buf), None, Some(ov as *mut OVERLAPPED)) }
        })
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let h = self.inner.handle;
        let ms = self.inner.write_ms.load(Ordering::SeqCst);
        self.transfer(ms, |ov| {
            // SAFETY: `buf` and `ov` outlive the operation (see `transfer`).
            unsafe { WriteFile(h, Some(buf), None, Some(ov as *mut OVERLAPPED)) }
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A server may not run at a lower integrity level than its client: a normal process could
/// otherwise stand in for the server of an elevated client (it can read the pipe secret and make
/// the pipe). The server itself never runs elevated, so an elevated client has no server here:
/// it uses its own in-process terminal.
fn levels_match(client: u32, server: u32) -> Result<(), String> {
    if server >= client {
        Ok(())
    } else {
        Err(format!(
            "runs at a lower integrity level ({server:#x}) than this client ({client:#x}); \
             start the server from this shell, or use a normal shell"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgetty_socket::SocketServer;
    use std::sync::Arc;

    #[test]
    fn a_server_below_the_client_is_refused_and_an_equal_or_higher_one_is_not() {
        assert!(levels_match(0x2000, 0x2000).is_ok());
        assert!(levels_match(0x2000, 0x3000).is_ok());
        assert!(levels_match(0x3000, 0x3000).is_ok());
        let err = levels_match(0x3000, 0x2000).unwrap_err();
        assert!(err.contains("lower integrity"), "{err}");
    }

    /// A real pipe server in this process, and a blocking client against it.
    #[test]
    fn connect_checks_the_server_level_in_both_directions() {
        let name = std::path::PathBuf::from(format!(r"\\.\pipe\forgetty-test-{}", uuid::Uuid::new_v4()));
        let n = name.clone();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let server = SocketServer::new_with_path(n).unwrap();
        let sm = Arc::new(forgetty_session::SessionManager::new());
        rt.spawn(async move {
            let _ = server.run_with_streaming(sm, None).await;
        });
        let mine = acl::current_integrity_rid().unwrap();
        // Wait until the pipe exists.
        let mut ok = None;
        for _ in 0..200 {
            match PipeStream::connect(&name) {
                Ok(s) => {
                    ok = Some(s);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(e) => panic!("connect: {e}"),
            }
        }
        // Same level: accepted, and the stream works.
        let mut s = ok.expect("the server came up");
        s.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"list_tabs\",\"id\":1}\n").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("result"));
        // A client one level above the server: refused before any byte is sent.
        let err = PipeStream::connect_as_level(&name, Some(mine + 0x1000)).err().expect("refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(err.to_string().contains("lower integrity"), "{err}");
        // A client below the server: accepted (the server only needs to be as high).
        assert!(PipeStream::connect_as_level(&name, Some(0x1000)).is_ok());
        rt.shutdown_background();
    }
}
