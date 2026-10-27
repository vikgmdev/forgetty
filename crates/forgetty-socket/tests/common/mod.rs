//! Shared by the socket integration tests: one endpoint and one client type per platform.
//!
//! Unix: a socket file in a temporary folder and a `UnixStream`. Windows: a uniquely named
//! pipe and a `NamedPipeClient`. The server side is the same call on both
//! (`SocketServer::new_with_path`), and the wire is the same JSON-RPC lines.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;
#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Where a test server listens. Dropping it removes the temporary folder (Unix).
pub struct Endpoint {
    pub path: PathBuf,
    #[cfg(unix)]
    _dir: tempfile::TempDir,
}

pub fn endpoint() -> Endpoint {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().expect("temp dir");
        Endpoint { path: dir.path().join("test.sock"), _dir: dir }
    }
    #[cfg(windows)]
    {
        Endpoint { path: PathBuf::from(format!(r"\\.\pipe\forgetty-test-{}", uuid::Uuid::new_v4())) }
    }
}

/// Connect, waiting for the server to bind and for a free pipe instance.
pub async fn connect(path: &Path) -> Stream {
    for _ in 0..200 {
        #[cfg(unix)]
        let attempt = tokio::net::UnixStream::connect(path).await;
        #[cfg(windows)]
        let attempt = tokio::net::windows::named_pipe::ClientOptions::new().open(path);
        match attempt {
            Ok(s) => return s,
            Err(e) => {
                // Unix: NotFound or ConnectionRefused until the listener is up. Windows: 2 (the
                // pipe is not created yet) or 231 (all instances busy).
                let retry = match e.raw_os_error() {
                    #[cfg(windows)]
                    Some(2 | 231) => true,
                    _ => matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ),
                };
                if !retry {
                    panic!("cannot connect to {}: {e}", path.display());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    panic!("the server at {} did not come up", path.display());
}
