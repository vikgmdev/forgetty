//! Unix domain socket server.
//!
//! Listens on a Unix domain socket for incoming JSON-RPC connections and
//! dispatches requests to the appropriate handlers. Each connection is
//! handled in its own task, reading line-delimited JSON requests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use forgetty_session::{SessionEvent, SessionManager};
use forgetty_sync::SyncEndpoint;

use crate::handlers;
use crate::protocol::{methods, Request, Response};

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
    ///
    /// This method is kept for backward compatibility with the existing
    /// round-trip test. For production use with a real `SessionManager`,
    /// prefer `run_with_streaming`.
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

    /// Run the server with full `SessionManager` integration, including
    /// streaming support for `subscribe_output`.
    ///
    /// For `subscribe_output` requests, the server validates the pane,
    /// subscribes to the broadcast channel, sends an initial `{"ok":true}`
    /// response, and then streams JSON notifications until the pane closes
    /// or the client disconnects.
    ///
    /// All other methods are dispatched synchronously via `handlers::dispatch`.
    ///
    /// `sync_endpoint` is optional so the socket server degrades gracefully
    /// when the iroh endpoint is unavailable (R-6).
    pub async fn run_with_streaming(
        &self,
        sm: Arc<SessionManager>,
        sync_endpoint: Option<Arc<SyncEndpoint>>,
    ) -> std::io::Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Socket server listening on {:?}", self.socket_path);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    debug!("Accepted socket connection");
                    let sm = Arc::clone(&sm);
                    let se = sync_endpoint.as_ref().map(Arc::clone);
                    tokio::spawn(async move {
                        if let Err(e) = handle_streaming_connection(stream, sm, se).await {
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

// ---------------------------------------------------------------------------
// Connection handlers
// ---------------------------------------------------------------------------

/// Handle a single client connection: read line-delimited JSON requests,
/// dispatch them, and write back JSON responses.
///
/// Used by the backward-compatible `run` method.
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

/// Handle a single client connection with streaming support.
///
/// For `subscribe_output`: validates the pane, subscribes, sends `{"ok":true}`,
/// then streams `output` notifications until the pane closes or the client
/// disconnects.
///
/// For all other methods: delegates synchronously to `handlers::dispatch`.
async fn handle_streaming_connection(
    stream: tokio::net::UnixStream,
    sm: Arc<SessionManager>,
    sync_endpoint: Option<Arc<SyncEndpoint>>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request = match Request::parse(&line) {
            Ok(r) => r,
            Err(err_response) => {
                write_response(&mut writer, &err_response).await?;
                continue;
            }
        };

        debug!("Received request: method={}", request.method);

        if request.method == methods::SUBSCRIBE_OUTPUT {
            // Validate pane_id first so we can return a proper error before
            // entering the streaming loop.
            let pane_id_str = match request.params.get("pane_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    let err = Response::error(
                        request.id.clone(),
                        crate::protocol::INVALID_PARAMS,
                        "missing param: pane_id".to_string(),
                    );
                    write_response(&mut writer, &err).await?;
                    return Ok(());
                }
            };

            let uuid = match uuid::Uuid::parse_str(&pane_id_str) {
                Ok(u) => u,
                Err(_) => {
                    let err = Response::error(
                        request.id.clone(),
                        crate::protocol::INVALID_PARAMS,
                        format!("invalid UUID: {pane_id_str}"),
                    );
                    write_response(&mut writer, &err).await?;
                    return Ok(());
                }
            };

            let pane_id = forgetty_core::PaneId(uuid);

            if sm.pane_info(pane_id).is_none() {
                let err = Response::error(
                    request.id.clone(),
                    crate::protocol::INVALID_PARAMS,
                    format!("pane not found: {pane_id_str}"),
                );
                write_response(&mut writer, &err).await?;
                return Ok(());
            }

            // Subscribe to the broadcast channel before sending the initial
            // response so we don't miss any events that arrive during the
            // round-trip.
            let mut rx = sm.subscribe_output();

            // Send the initial acknowledgment.
            let ack = Response::success(request.id.clone(), serde_json::json!({ "ok": true }));
            write_response(&mut writer, &ack).await?;

            // Stream output notifications until the pane closes or the
            // write fails (client disconnected).
            loop {
                match rx.recv().await {
                    Ok(SessionEvent::PtyOutput { pane_id: evt_id, data }) => {
                        if evt_id != pane_id {
                            continue;
                        }
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&data[..]);
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "output",
                            "params": {
                                "pane_id": pane_id.to_string(),
                                "data": encoded,
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::PaneClosed { pane_id: closed_id }) => {
                        if closed_id == pane_id {
                            // Pane exited — end the stream.
                            break;
                        }
                    }
                    Ok(_) => {
                        // Other events (PaneCreated, Notification) are not
                        // forwarded to subscribe_output clients.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Consumer fell behind; continue from here.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Channel shut down (daemon exiting).
                        break;
                    }
                }
            }

            // Connection ends after subscribe_output stream terminates.
            return Ok(());
        }

        if request.method == methods::SUBSCRIBE_LAYOUT {
            // No parameter validation — the layout stream is connection-wide.
            // Reuse the same broadcast channel as subscribe_output; we filter
            // to layout variants in the loop below.
            let mut rx = sm.subscribe_output();

            // Send the initial acknowledgment.
            let ack = Response::success(request.id.clone(), serde_json::json!({ "ok": true }));
            write_response(&mut writer, &ack).await?;

            // Stream layout notifications until the daemon shuts down or the
            // client disconnects.
            loop {
                match rx.recv().await {
                    Ok(SessionEvent::TabCreated { workspace_idx, tab_id, pane_id }) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "tab_created",
                            "params": {
                                "workspace_idx": workspace_idx,
                                "tab_id": tab_id.to_string(),
                                "pane_id": pane_id.to_string(),
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::TabClosed { workspace_idx, tab_id }) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "tab_closed",
                            "params": {
                                "workspace_idx": workspace_idx,
                                "tab_id": tab_id.to_string(),
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::PaneSplit {
                        tab_id,
                        parent_pane_id,
                        new_pane_id,
                        direction,
                    }) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "pane_split",
                            "params": {
                                "tab_id": tab_id.to_string(),
                                "parent_pane_id": parent_pane_id.to_string(),
                                "new_pane_id": new_pane_id.to_string(),
                                "direction": direction,
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::TabMoved { workspace_idx, tab_id, new_index }) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "tab_moved",
                            "params": {
                                "workspace_idx": workspace_idx,
                                "tab_id": tab_id.to_string(),
                                "new_index": new_index,
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::ActiveTabChanged { workspace_idx, tab_idx }) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "active_tab_changed",
                            "params": {
                                "workspace_idx": workspace_idx,
                                "tab_idx": tab_idx,
                            }
                        });
                        let mut out = serde_json::to_string(&notification)
                            .unwrap_or_else(|_| "{}".to_string());
                        out.push('\n');
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {
                        // Output events (PtyOutput, PaneCreated, PaneClosed,
                        // Notification) are not forwarded to subscribe_layout clients.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Consumer fell behind; continue from here.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Channel shut down (daemon exiting).
                        break;
                    }
                }
            }

            // Connection ends after subscribe_layout stream terminates.
            return Ok(());
        }

        // Synchronous handler for all other methods.
        let response =
            handlers::dispatch(&request, Arc::clone(&sm), sync_endpoint.as_ref().map(Arc::clone));
        write_response(&mut writer, &response).await?;
    }

    debug!("Client disconnected");
    Ok(())
}

/// Serialize and write a JSON-RPC response as a newline-terminated line.
async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> std::io::Result<()> {
    let mut out = serde_json::to_string(response).unwrap_or_else(|e| {
        let fallback = Response::error(
            None,
            crate::protocol::INTERNAL_ERROR,
            format!("Failed to serialize response: {e}"),
        );
        serde_json::to_string(&fallback).expect("fallback must serialize")
    });
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    writer.flush().await
}

/// Determine the default socket path.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("forgetty.sock")
    } else {
        PathBuf::from("/tmp/forgetty.sock")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use forgetty_session::SessionManager;

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
        let sm = Arc::new(SessionManager::new());

        // Spawn server accept loop in background.
        let handle = tokio::spawn(async move {
            let sm_inner = Arc::clone(&sm);
            let handler =
                Arc::new(move |req: Request| handlers::dispatch(&req, Arc::clone(&sm_inner), None));
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
