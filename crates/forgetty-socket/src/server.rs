//! Unix domain socket server.
//!
//! Listens on a Unix domain socket for incoming JSON-RPC connections and
//! dispatches requests to the appropriate handlers. Each connection is
//! handled in its own task, reading line-delimited JSON requests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use forgetty_session::{SessionEvent, SessionManager};
use forgetty_sync::SyncEndpoint;
use forgetty_workspace;

use crate::framing::write_frame;
use crate::handlers;
use crate::protocol::{methods, Request, Response};

/// RAII guard that increments a connection counter on construction and
/// decrements it on drop. Used by `run_with_streaming` so the `is_attached`
/// RPC can report whether any *other* client is currently holding a socket.
///
/// Correctness (V2-007 fix cycle 2): the counter is an `Arc<AtomicUsize>`
/// shared between the server accept loop and every spawned connection task.
/// `Relaxed` ordering is sufficient — reads of the counter inside
/// `is_attached` are a best-effort snapshot; they do not synchronise with
/// any other memory. Each `ConnGuard` bumps the counter on `new` and
/// decrements it in `Drop`, even if the task panics (so orphaned entries
/// cannot accumulate).
struct ConnGuard {
    counter: Arc<AtomicUsize>,
}

impl ConnGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // `fetch_sub` with Relaxed: ordering is not required because the
        // counter is only ever read for a best-effort snapshot.
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The Forgetty JSON-RPC socket server.
pub struct SocketServer {
    socket_path: PathBuf,
    /// Session UUID — used by `shutdown_save` to write the correct session file.
    /// `None` in test/legacy contexts where session_id is not relevant.
    session_id: Option<uuid::Uuid>,
}

impl SocketServer {
    /// Create a new server bound to the default socket path (no session_id).
    ///
    /// The socket path is `$XDG_RUNTIME_DIR/forgetty.sock` on Linux,
    /// falling back to `/tmp/forgetty.sock`.
    pub fn new() -> std::io::Result<Self> {
        Self::new_with_path(default_socket_path())
    }

    /// Create a new server bound to an explicit socket path (no session_id).
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

        Ok(Self { socket_path, session_id: None })
    }

    /// Create a new server bound to an explicit socket path with a session UUID.
    ///
    /// The session UUID is used by the `shutdown_save` RPC to write the
    /// correct `sessions/{uuid}.json` file before the daemon exits.
    pub fn new_with_session(socket_path: PathBuf, session_id: uuid::Uuid) -> std::io::Result<Self> {
        let mut server = Self::new_with_path(socket_path)?;
        server.session_id = Some(session_id);
        Ok(server)
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
    /// response, and then streams length-prefixed binary frames of raw PTY
    /// bytes until the pane closes or the client disconnects (AD-010). The
    /// framing is `[u32 BE length][payload]`; see `crate::framing`.
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

        let session_id = self.session_id;
        // Shared live-connection counter, used by `is_attached` to tell
        // "daemon orphaned after V2-005 disconnect" from "daemon actively
        // serving a GUI". One counter per server; incremented per spawned
        // connection task via ConnGuard.
        let conn_counter: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    debug!("Accepted socket connection");
                    let sm = Arc::clone(&sm);
                    let se = sync_endpoint.as_ref().map(Arc::clone);
                    let counter = Arc::clone(&conn_counter);
                    tokio::spawn(async move {
                        let _guard = ConnGuard::new(Arc::clone(&counter));
                        if let Err(e) =
                            handle_streaming_connection(stream, sm, se, session_id, counter).await
                        {
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
/// then streams `[u32 BE length][raw PTY bytes]` binary frames until the
/// pane closes or the client disconnects. After the ack is written, the
/// server stops reading from the client half of this connection; any bytes
/// the client writes are dropped unread (V2-003 SPEC §4.4).
///
/// For all other methods: delegates synchronously to `handlers::dispatch`.
async fn handle_streaming_connection(
    stream: tokio::net::UnixStream,
    sm: Arc<SessionManager>,
    sync_endpoint: Option<Arc<SyncEndpoint>>,
    session_id: Option<uuid::Uuid>,
    conn_counter: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    // V2-007 fix cycle 3: hold `BufReader<OwnedReadHalf>` as a reusable
    // binding rather than collapsing it to a `Lines` adapter. This lets us
    // switch from line-mode `read_line` (control RPCs) to raw `read()` for
    // EOF detection once we enter a streaming arm (subscribe_output /
    // subscribe_layout). Before the fix, `Lines` consumed the reader and
    // the streaming loop awaited only the broadcast receiver — so an idle
    // pane whose GUI closed left the handler blocked forever, leaking the
    // ConnGuard counter and breaking V2-007 AC-18 under AD-012.
    let mut reader = BufReader::new(reader);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf).await?;
        if n == 0 {
            // Orderly EOF on the client read half.
            break;
        }
        let line = line_buf.trim().to_string();
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

            // ---------------------------------------------------------------
            // V2-007 AC-13 + fix cycle 6: zero-gap, zero-overlap replay handover.
            //
            // 1. Atomically subscribe AND snapshot under a single lock. The
            //    `SessionManager::subscribe_with_snapshot` contract guarantees
            //    the new receiver will not deliver any event whose bytes are
            //    already in `replay_bytes` — the overlap between replay and
            //    the live broadcast stream is provably zero bytes. See the
            //    doc comment on `subscribe_with_snapshot` and V2-007
            //    BUILDER_NOTES §"Fix cycle 6" for the proof.
            // 2. Send ack.
            // 3. Emit replay as a single binary frame (ring ≤ 1 MiB, well
            //    under V2-003's 4 MiB frame cap).
            // 4. Live loop — every `PtyOutput` for this pane is forwarded
            //    verbatim. No cursor, no skip arithmetic.
            //
            // Cycle-1 history: the original code used separate
            // `subscribe_output()` + `get_ring_snapshot()` calls with a
            // byte-counting cursor that compared against `replay_len`. That
            // logic was wrong in the idle-pane reattach case: the actual
            // overlap can be less than `replay_len`, so the cursor silently
            // skipped live output until it "caught up." The fix is to
            // eliminate the overlap at its source, which makes the cursor
            // unnecessary. See V2-007 BUILDER_NOTES §"Fix cycle 6".
            // ---------------------------------------------------------------
            let (mut rx, replay_bytes, _replay_hwm) = sm.subscribe_with_snapshot(pane_id);

            // Send the initial acknowledgment (last line-mode JSON response
            // on this connection).
            let ack = Response::success(request.id.clone(), serde_json::json!({ "ok": true }));
            write_response(&mut writer, &ack).await?;

            // The server is now in binary output mode for this connection.
            // We keep `reader` alive — V2-003 SPEC §4.4 says the client does
            // not send further data on this connection, but we still need to
            // notice if the client closes the socket (V2-007 fix cycle 3 /
            // AD-012). Without EOF detection, an idle-pane subscribe_output
            // loop would block forever on `rx.recv()` after the GUI closes,
            // leaking the ConnGuard counter and breaking `is_attached`.

            // Emit replay as a single frame if non-empty.
            if !replay_bytes.is_empty() {
                if let Err(e) = write_frame(&mut writer, &replay_bytes).await {
                    debug!("subscribe_output: replay write_frame failed for {pane_id}: {e}");
                    return Ok(());
                }
            }

            // Live loop. Forward every PtyOutput for this pane verbatim —
            // `subscribe_with_snapshot` guarantees zero overlap with the
            // already-emitted replay frame.
            //
            // V2-007 fix cycle 3: `tokio::select!` between the broadcast
            // receiver and a raw read on the client half. Both arms are
            // cancel-safe:
            //   - `broadcast::Receiver::recv` is cancel-safe (tokio docs).
            //   - `AsyncRead::read` into a stack buffer is cancel-safe.
            // AD-009 preserved: no timer, no polling — both arms are
            // event-driven awaits. AD-012 preserved: the daemon still
            // outlives the GUI; we just detect the client's socket close
            // and release the ConnGuard promptly.
            let mut eof_buf = [0u8; 1];
            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        match recv_result {
                            Ok(SessionEvent::PtyOutput { pane_id: evt_id, data }) => {
                                if evt_id != pane_id {
                                    continue;
                                }
                                if data.is_empty() {
                                    // Avoid sending zero-length frames (SPEC §4.2).
                                    continue;
                                }
                                if let Err(e) = write_frame(&mut writer, &data).await {
                                    debug!(
                                        "subscribe_output: write_frame failed for {pane_id}: {e}"
                                    );
                                    break;
                                }
                            }
                            Ok(SessionEvent::PaneClosed { pane_id: closed_id }) => {
                                if closed_id == pane_id {
                                    // Pane exited — end the stream. Dropping
                                    // `writer` at function exit closes the
                                    // write half, signaling EOF to the client.
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
                    read_result = reader.read(&mut eof_buf) => {
                        // V2-003 §4.4: the client does not send on this
                        // connection after the ack. Any outcome here means
                        // the connection is done:
                        //   - Ok(0)  → orderly EOF (peer closed).
                        //   - Ok(n)  → unexpected bytes — protocol violation;
                        //              we drop them and close.
                        //   - Err(_) → I/O error — treat as close.
                        debug!(
                            "subscribe_output: client read half closed for {pane_id} (res={:?})",
                            read_result
                        );
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
            //
            // V2-007 fix cycle 3: same shape as subscribe_output — `tokio::select!`
            // between the broadcast receiver and a raw read on the client half,
            // so idle layout subscribers don't leak the ConnGuard counter when
            // their GUI closes. Both arms cancel-safe; AD-009 (no polling) and
            // AD-012 (daemon survives window close) preserved.
            let mut eof_buf = [0u8; 1];
            loop {
                tokio::select! {
                    recv_result = rx.recv() => {
                        match recv_result {
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
                            Ok(SessionEvent::ActiveWorkspaceChanged { workspace_idx }) => {
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "active_workspace_changed",
                                    "params": {
                                        "workspace_idx": workspace_idx,
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
                            Ok(SessionEvent::WorkspaceCreated {
                                workspace_idx,
                                workspace_id,
                                name,
                            }) => {
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "workspace_created",
                                    "params": {
                                        "workspace_idx": workspace_idx,
                                        "workspace_id": workspace_id.to_string(),
                                        "name": name,
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
                            Ok(SessionEvent::WorkspaceRenamed {
                                workspace_idx,
                                workspace_id,
                                name,
                            }) => {
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "workspace_renamed",
                                    "params": {
                                        "workspace_idx": workspace_idx,
                                        "workspace_id": workspace_id.to_string(),
                                        "name": name,
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
                            Ok(SessionEvent::WorkspaceDeleted {
                                workspace_idx,
                                workspace_id,
                            }) => {
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "workspace_deleted",
                                    "params": {
                                        "workspace_idx": workspace_idx,
                                        "workspace_id": workspace_id.to_string(),
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
                            Ok(SessionEvent::WorkspaceColorChanged {
                                workspace_idx,
                                workspace_id,
                                color,
                            }) => {
                                // FIX-010: fan out colour-change notifications to
                                // subscribed clients. `color` is `Option<String>` →
                                // serialises as `"#RRGGBB"` or `null`.
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "workspace_color_changed",
                                    "params": {
                                        "workspace_idx": workspace_idx,
                                        "workspace_id": workspace_id.to_string(),
                                        "color": color,
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
                    read_result = reader.read(&mut eof_buf) => {
                        // The layout stream is server-to-client only. Any
                        // read activity here — EOF, unexpected bytes, or
                        // error — signals the connection is done.
                        debug!(
                            "subscribe_layout: client read half closed (res={:?})",
                            read_result
                        );
                        break;
                    }
                }
            }

            // Connection ends after subscribe_layout stream terminates.
            return Ok(());
        }

        // Synchronous handler for all other methods.
        if request.method == methods::SHUTDOWN_SAVE {
            // Acknowledge before saving so the client unblocks immediately.
            let resp = Response::success(request.id, serde_json::json!({ "ok": true }));
            write_response(&mut writer, &resp).await?;
            info!("Received shutdown_save RPC — saving session and exiting");
            // Flush byte-log ring to disk (V2-007 / AD-013).
            sm.flush_all_byte_logs().await;
            info!("shutdown_save: byte logs flushed");
            // Save the session layout so restore-by-default can bring it back.
            if let Some(sid) = session_id {
                let state = sm.snapshot_to_workspace_state();
                match forgetty_workspace::save_session_for(sid, &state) {
                    Ok(()) => info!("shutdown_save: session {sid} saved"),
                    Err(e) => warn!("shutdown_save: failed to save session: {e}"),
                }
            }
            std::process::exit(0);
        }

        if request.method == methods::SHUTDOWN_CLEAN {
            // Browser-model close: save session, move to trash, then exit.
            let resp = Response::success(request.id, serde_json::json!({ "ok": true }));
            write_response(&mut writer, &resp).await?;
            info!("Received shutdown_clean RPC — saving, trashing, and exiting");
            // Flush byte-log ring to disk (V2-007 / AD-013).
            sm.flush_all_byte_logs().await;
            info!("shutdown_clean: byte logs flushed");
            if let Some(sid) = session_id {
                // Check if pinned — pinned sessions do NOT get trashed.
                let is_pinned = sm.is_pinned();
                if is_pinned {
                    // Pinned: just save the session file (no trash).
                    let state = sm.snapshot_to_workspace_state();
                    match forgetty_workspace::save_session_for(sid, &state) {
                        Ok(()) => info!("shutdown_clean: pinned session {sid} saved"),
                        Err(e) => warn!("shutdown_clean: failed to save session: {e}"),
                    }
                } else {
                    // Unpinned: save then move to trash.
                    let state = sm.snapshot_to_workspace_state();
                    match forgetty_workspace::save_session_for(sid, &state) {
                        Ok(()) => {
                            info!("shutdown_clean: session {sid} saved");
                            match forgetty_workspace::trash_session_for(sid) {
                                Ok(()) => info!("shutdown_clean: session {sid} moved to trash"),
                                Err(e) => warn!("shutdown_clean: trash failed: {e}"),
                            }
                        }
                        Err(e) => warn!("shutdown_clean: failed to save session: {e}"),
                    }
                }
            }
            std::process::exit(0);
        }

        if request.method == methods::DISCONNECT {
            // V2-005 (AD-012): daemon survives window close.
            //
            // Acknowledge before saving so the client unblocks immediately.
            let resp = Response::success(request.id, serde_json::json!({ "ok": true }));
            write_response(&mut writer, &resp).await?;
            info!("Received disconnect RPC — saving session, daemon continues running");
            // Flush byte-log ring to disk (V2-007 / AD-013).
            sm.flush_all_byte_logs().await;
            info!("disconnect: byte logs flushed");
            // Flush the session layout so reconnecting GTK clients can restore state.
            if let Some(sid) = session_id {
                let state = sm.snapshot_to_workspace_state();
                match forgetty_workspace::save_session_for(sid, &state) {
                    Ok(()) => info!("disconnect: session {sid} saved"),
                    Err(e) => warn!("disconnect: failed to save session: {e}"),
                }
            }
            // Connection ends here. The UnixListener loop in run_with_streaming
            // continues accepting new connections. PTY processes and panes remain
            // alive. AD-012.
            return Ok(());
        }

        if request.method == methods::SHUTDOWN {
            // Acknowledge before exiting so the client doesn't see a broken pipe.
            let resp = Response::success(request.id, serde_json::json!({ "ok": true }));
            write_response(&mut writer, &resp).await?;
            info!("Received shutdown RPC — exiting daemon");
            std::process::exit(0);
        }

        if request.method == methods::IS_ATTACHED {
            // V2-007 fix cycle 2: answer "is any OTHER local client attached?"
            //
            // `conn_counter` is incremented by the accept loop's ConnGuard
            // before this task runs, so it includes the caller's own
            // connection. A count > 1 therefore means at least one other
            // connection is live. A lone probe sees count == 1 and reports
            // `attached: false`; a GUI-held session sees count > 1 and
            // reports `attached: true`.
            //
            // iroh peer connections are NOT counted — they don't go through
            // this Unix-socket accept loop. Android/QUIC clients are a
            // different seat (AD-004/AD-005) and do not block local-GUI
            // reattach.
            let total = conn_counter.load(Ordering::Relaxed);
            let attached = total > 1;
            let resp =
                Response::success(request.id.clone(), serde_json::json!({ "attached": attached }));
            write_response(&mut writer, &resp).await?;
            continue;
        }

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

        let _server = SocketServer { socket_path: sock_path.clone(), session_id: None };

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
