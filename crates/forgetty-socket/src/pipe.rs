//! The control socket on Windows: a named pipe, `\\.\pipe\forgetty-<user SID>[-<hash>]`.
//!
//! The same JSON-RPC lines and binary frames as the Unix socket, over a byte-mode duplex pipe.
//! What keeps other users and other machines out:
//!
//! - The pipe is created with the current user as owner and a protected DACL that holds one
//!   entry, the current user (the kernel refuses everyone else at `CreateFile`), and with
//!   `PIPE_REJECT_REMOTE_CLIENTS`. A client reads the owner of the pipe it opened and requires
//!   it to be its own user.
//! - The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that
//!   already owns the name (a squatter) makes the bind fail instead of sharing the name. The
//!   name also holds a secret that is new at every server start (`forgetty-server` `sockpath`):
//!   it keeps another user from preparing a squat before the start. It is not a secret from
//!   other users once the server runs (any user can list pipe names), so it is kept out of logs
//!   and error messages here (see [`redact_pipe_name`]).
//! - The server itself does not run elevated (it refuses to start), so there is no pipe that a
//!   normal process of the same user must be kept away from.
//! - After the first read from a client the server asks Windows for the client's user SID and
//!   integrity level and drops the connection if the user is not the expected one or the level
//!   is lower than the server's. A connection counts as a client only after this check. This is the counterpart of
//!   `SO_PEERCRED` on Unix, and it holds if someone widens the DACL by mistake.
//!
//! The client side (a blocking stream with timeouts and a check of the server's SID) is in
//! `forgetty-server`, where the CLI is.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use forgetty_core::fsutil::acl;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// `name` with the secret cut out, for logs and errors (`\\.\pipe\forgetty-<SID>-<secret>`
/// becomes `\\.\pipe\forgetty-<SID>-<redacted>`). The secret is the last dash-separated part
/// when it is at least 32 hex digits.
pub fn redact_pipe_name(name: &str) -> String {
    match name.rsplit_once('-') {
        Some((head, tail)) if tail.len() >= 32 && tail.bytes().all(|b| b.is_ascii_hexdigit()) => {
            format!("{head}-<redacted>")
        }
        _ => name.to_string(),
    }
}

/// Pipe buffer size in each direction. Output frames are large; a bigger buffer means fewer
/// round trips to the kernel.
const PIPE_BUFFER: u32 = 64 * 1024;

/// Who the server serves.
#[derive(Clone, Debug, Default)]
pub struct Allowed {
    /// The user SID. `None` is the user this process runs as.
    pub sid: Option<String>,
    /// The lowest integrity level of a client. `None` is the level of this process. Tests raise
    /// it to play a client of a lower level.
    pub min_integrity: Option<u32>,
}

/// A security descriptor made from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is plain memory that is only read after it is made.
unsafe impl Send for Descriptor {}

impl Descriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let w: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `w` is NUL-terminated and lives for the call; `sd` is freed in `Drop`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(w.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self(sd))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the convert call with LocalAlloc.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// The listening side: always one unconnected instance waiting for the next client.
pub struct Listener {
    name: String,
    sd: Descriptor,
    /// The instance waiting for a client. `None` after a failed attempt to make a new one.
    next: Option<NamedPipeServer>,
    /// The SID a client must have.
    expected: String,
    /// The integrity level a client must have at least.
    min_integrity: u32,
}

impl Listener {
    /// Create the first instance of the pipe `path` (`\\.\pipe\...`). Fails if a pipe of that
    /// name exists already.
    pub fn bind(path: &Path, allowed: Allowed) -> io::Result<Self> {
        let me = acl::current_user_sid()?;
        let level = acl::current_integrity_rid()?;
        // The pipe is always restricted to this user. `allowed` only changes who the server
        // accepts after the connect (a test uses it to play a foreign user or a lower level).
        let sd = Descriptor::from_sddl(&acl::pipe_sddl(&me))?;
        let mut l = Self {
            name: path.to_string_lossy().into_owned(),
            sd,
            next: None,
            expected: allowed.sid.unwrap_or(me),
            min_integrity: allowed.min_integrity.unwrap_or(level),
        };
        l.next = Some(l.create(true).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("the pipe {} is taken by another process", redact_pipe_name(&l.name)),
                )
            } else {
                e
            }
        })?);
        info!(
            "pipe {} created for {} (integrity {:#x} or higher)",
            redact_pipe_name(&l.name),
            l.expected,
            l.min_integrity
        );
        Ok(l)
    }

    fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0 .0,
            bInheritHandle: false.into(),
        };
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first)
            .reject_remote_clients(true)
            .in_buffer_size(PIPE_BUFFER)
            .out_buffer_size(PIPE_BUFFER);
        // SAFETY: `sa` and the descriptor it points to live for the call; the kernel copies them.
        unsafe {
            opts.create_with_security_attributes_raw(&self.name, &mut sa as *mut _ as *mut c_void)
        }
    }

    /// Wait for the next client. Always `Some`: the user check comes after the first read
    /// (see [`Connection`]).
    pub async fn accept(&mut self) -> io::Result<Option<Connection>> {
        let server = match self.next.take() {
            Some(s) => s,
            None => match self.create(false) {
                Ok(s) => s,
                Err(e) => {
                    // Do not spin: the caller loops on errors.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    return Err(e);
                }
            },
        };
        if let Err(e) = server.connect().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(e);
        }
        // At once, so the next client finds a free instance.
        self.next = self.create(false).map_err(|e| warn!("pipe: cannot create the next instance: {e}")).ok();
        Ok(Some(Connection {
            inner: server,
            verified: false,
            expected: self.expected.clone(),
            min_integrity: self.min_integrity,
        }))
    }
}

/// An accepted client. Reads pass through, and the first bytes from the client are held back
/// until its user SID is known to be the expected one.
pub struct Connection {
    inner: NamedPipeServer,
    verified: bool,
    expected: String,
    min_integrity: u32,
}

impl Connection {
    /// Has the client's user and integrity level been checked? True after the first bytes.
    pub fn is_verified(&self) -> bool {
        self.verified
    }

    fn verify(&self) -> io::Result<()> {
        let handle = HANDLE(self.inner.as_raw_handle());
        let (sid, level) = acl::pipe_client_identity(handle).map_err(|e| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("pipe: cannot read the client's user: {e}"),
            )
        })?;
        if !acl::same_account(&sid, &self.expected) {
            warn!("pipe: refused a client of user {sid} (only {} is served)", self.expected);
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "client is another user"));
        }
        if level < self.min_integrity {
            warn!(
                "pipe: refused a client at integrity {level:#x} (the server needs {:#x})",
                self.min_integrity
            );
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "client runs at a lower integrity level than the server",
            ));
        }
        debug!("pipe: client {sid} accepted at integrity {level:#x}");
        Ok(())
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if !self.verified && buf.filled().len() > before {
                    if let Err(e) = self.verify() {
                        // Nothing the client sent reaches the handler.
                        buf.set_filled(before);
                        return Poll::Ready(Err(e));
                    }
                    self.verified = true;
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // A client that was never verified gets nothing: the server only answers requests it
        // has read, so this is a guard and not a path that runs.
        if !self.verified {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "client not verified",
            )));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
