//! The control socket on Windows: a named pipe, `\\.\pipe\forgetty-<user SID>[-<hash>]`.
//!
//! The same JSON-RPC lines and binary frames as the Unix socket, over a byte-mode duplex pipe.
//! What keeps other users and other machines out:
//!
//! - The pipe is created with a protected DACL that holds one entry, the current user (the
//!   kernel refuses everyone else at `CreateFile`), and with `PIPE_REJECT_REMOTE_CLIENTS`.
//! - The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that
//!   already owns the name (a squatter) makes the bind fail instead of sharing the name.
//! - After the first read from a client the server asks Windows for the client's user SID and
//!   drops the connection if it is not the expected one. This is the counterpart of
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

/// Pipe buffer size in each direction. Output frames are large; a bigger buffer means fewer
/// round trips to the kernel.
const PIPE_BUFFER: u32 = 64 * 1024;

/// The user the server serves: `None` is the user this process runs as.
pub type Allowed = Option<String>;

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
}

impl Listener {
    /// Create the first instance of the pipe `path` (`\\.\pipe\...`). Fails if a pipe of that
    /// name exists already.
    pub fn bind(path: &Path, allowed: Allowed) -> io::Result<Self> {
        let me = acl::current_user_sid()?;
        // The pipe is always restricted to this user. `allowed` only changes who the server
        // accepts after the connect (a test uses it to play a foreign user).
        let sd = Descriptor::from_sddl(&acl::only_sid_sddl(&me))?;
        let mut l = Self {
            name: path.to_string_lossy().into_owned(),
            sd,
            next: None,
            expected: allowed.unwrap_or(me),
        };
        l.next = Some(l.create(true).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("the pipe {} is taken by another process", l.name),
                )
            } else {
                e
            }
        })?);
        info!("pipe {} created for {}", l.name, l.expected);
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
        Ok(Some(Connection { inner: server, verified: false, expected: self.expected.clone() }))
    }
}

/// An accepted client. Reads pass through, and the first bytes from the client are held back
/// until its user SID is known to be the expected one.
pub struct Connection {
    inner: NamedPipeServer,
    verified: bool,
    expected: String,
}

impl Connection {
    fn verify(&self) -> io::Result<()> {
        let handle = HANDLE(self.inner.as_raw_handle());
        let sid = acl::pipe_client_sid(handle).map_err(|e| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("pipe: cannot read the client's user: {e}"),
            )
        })?;
        if acl::same_account(&sid, &self.expected) {
            debug!("pipe: client {sid} accepted");
            Ok(())
        } else {
            warn!("pipe: refused a client of user {sid} (only {} is served)", self.expected);
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "client is another user"))
        }
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
