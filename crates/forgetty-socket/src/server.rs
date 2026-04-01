//! Unix domain socket server.
//!
//! Listens on a Unix domain socket for incoming JSON-RPC connections and
//! dispatches requests to the appropriate handlers. Each connection is
//! handled in its own task, reading line-delimited JSON requests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use crate::protocol::{Request, Response};

/// The Forgetty JSON-RPC socket server.
pub struct SocketServer {
    socket_path: PathBuf,
}

impl SocketServer {
    /// Create a new server bound to the default socket path.
    ///
    /// The socket path is `$XDG_RUNTIME_DIR/forgetty.sock` on Linux,
    /// falling back to `/tmp/forgetty.sock`.
    pub fn new() -> std::io::Result<Self> {
        Self::new_with_path(default_socket_path())
    }

    /// Create a new server bound to an explicit socket path.
    ///
    /// Removes any stale socket file and creates the parent directory if
    /// needed. Use this when the caller needs to override the default path
    /// (e.g., `forgetty-daemon --socket-path`).
    pub fn new_with_path(socket_path: PathBuf) -> std::io::Result<Self> {
        // Remove stale socket file if it exists.
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        // Ensure the parent directory exists.
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        Ok(Self { socket_path })
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the server, accepting connections and processing requests.
    ///
    /// The `handler` callback is invoked for each incoming request and must
    /// return a `Response`. The server processes line-delimited JSON on each
    /// connection.
    pub async fn run<F>(&self, handler: F) -> std::io::Result<()>
    where
        F: Fn(Request) -> Response + Send + Sync + 'static,
    {
        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Socket server listening on {:?}", self.socket_path);

        let handler = Arc::new(handler);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    debug!("Accepted socket connection");
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, handler).await {
                            warn!("Connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {e}");
                }
            }
        }
    }
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        // Best-effort cleanup of the socket file.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Handle a single client connection: read line-delimited JSON requests,
/// dispatch them, and write back JSON responses.
async fn handle_connection<F>(
    stream: tokio::net::UnixStream,
    handler: Arc<F>,
) -> std::io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match Request::parse(&line) {
            Ok(request) => {
                debug!("Received request: method={}", request.method);
                handler(request)
            }
            Err(err_response) => err_response,
        };

        let mut out = serde_json::to_string(&response).unwrap_or_else(|e| {
            let fallback = Response::error(
                None,
                crate::protocol::INTERNAL_ERROR,
                format!("Failed to serialize response: {e}"),
            );
            serde_json::to_string(&fallback).expect("fallback must serialize")
        });
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
        writer.flush().await?;
    }

    debug!("Client disconnected");
    Ok(())
}

/// Determine the default socket path.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("forgetty.sock")
    } else {
        PathBuf::from("/tmp/forgetty.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_with_xdg() {
        // Just verify the function returns a path ending with forgetty.sock.
        let path = default_socket_path();
        assert!(path.to_str().unwrap().ends_with("forgetty.sock"));
    }

    #[tokio::test]
    async fn server_round_trip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        let dir = std::env::temp_dir().join(format!("forgetty-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");

        // Clean up from previous test runs.
        let _ = std::fs::remove_file(&sock_path);

        let _server = SocketServer { socket_path: sock_path.clone() };

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn server accept loop in background.
        let handle = tokio::spawn(async move {
            let handler = Arc::new(|req: Request| crate::handlers::dispatch(&req));
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, handler).await.unwrap();
        });

        // Give the listener a moment to be ready (it's already bound, so
        // connect should succeed immediately).
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();

        // Send a valid request.
        let req = r#"{"jsonrpc":"2.0","method":"list_tabs","id":1}"#;
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        // Read response.
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let response_str = std::string::String::from_utf8_lossy(&buf[..n]);
        let resp: Response = serde_json::from_str(response_str.trim()).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.id, Some(serde_json::json!(1)));

        // Send invalid JSON.
        stream.write_all(b"{bad json\n").await.unwrap();
        stream.flush().await.unwrap();

        let n = stream.read(&mut buf).await.unwrap();
        let response_str = std::string::String::from_utf8_lossy(&buf[..n]);
        let resp: Response = serde_json::from_str(response_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, crate::protocol::PARSE_ERROR);

        // Close the stream so the server task finishes.
        drop(stream);
        let _ = handle.await;

        // Cleanup.
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
