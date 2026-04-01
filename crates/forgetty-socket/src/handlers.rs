//! Request handlers for the JSON-RPC socket server.
//!
//! Each handler processes a specific JSON-RPC method and delegates to the
//! real `SessionManager` for PTY state. The `dispatch` function routes
//! synchronous (non-streaming) methods; `subscribe_output` is handled
//! directly in `server.rs` because it requires an async streaming loop.

use std::sync::Arc;

use base64::Engine as _;
use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::SessionManager;
use uuid::Uuid;

use crate::protocol::{self, methods, Request, Response};

// Default PTY size for new panes created via `new_tab`.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a synchronous JSON-RPC request to the appropriate handler.
///
/// `subscribe_output` is intentionally absent here — it is handled in the
/// streaming path in `server.rs` before `dispatch` is called.
pub fn dispatch(request: &Request, sm: Arc<SessionManager>) -> Response {
    match request.method.as_str() {
        methods::LIST_TABS => handle_list_tabs(request, &sm),
        methods::NEW_TAB => handle_new_tab(request, &sm),
        methods::CLOSE_TAB => handle_close_tab(request, &sm),
        methods::FOCUS_TAB => handle_focus_tab(request),
        methods::SPLIT_PANE => handle_split_pane(request),
        methods::SEND_INPUT => handle_send_input(request, &sm),
        methods::GET_SCREEN => handle_get_screen(request, &sm),
        methods::GET_PANE_INFO => handle_get_pane_info(request, &sm),
        _ => Response::error(
            request.id.clone(),
            protocol::METHOD_NOT_FOUND,
            format!("Unknown method: {}", request.method),
        ),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse and validate a `pane_id` string from request params.
///
/// Returns `Err(Response)` for any of:
/// - missing `pane_id` field  → `-32602` "missing param: pane_id"
/// - non-UUID string          → `-32602` "invalid UUID: <value>"
/// - pane not in live registry → `-32602` "pane not found: <uuid>"
fn require_pane_id(
    request: &Request,
    sm: &SessionManager,
) -> Result<PaneId, Response> {
    let params = &request.params;

    let raw = match params.get("pane_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Err(Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: pane_id".to_string(),
            ))
        }
    };

    let uuid = match Uuid::parse_str(&raw) {
        Ok(u) => u,
        Err(_) => {
            return Err(Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                format!("invalid UUID: {raw}"),
            ))
        }
    };

    let id = PaneId(uuid);

    // Verify pane is alive.
    if sm.pane_info(id).is_none() {
        return Err(Response::error(
            request.id.clone(),
            protocol::INVALID_PARAMS,
            format!("pane not found: {raw}"),
        ));
    }

    Ok(id)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_list_tabs(request: &Request, sm: &SessionManager) -> Response {
    let ids = sm.list_panes();
    let tabs: Vec<serde_json::Value> = ids
        .into_iter()
        .filter_map(|id| {
            sm.pane_info(id).map(|info| {
                serde_json::json!({
                    "pane_id": info.id.to_string(),
                    "pid": info.pid,
                    "rows": info.rows,
                    "cols": info.cols,
                    "cwd": info.cwd.display().to_string(),
                    "title": info.title,
                })
            })
        })
        .collect();

    Response::success(request.id.clone(), serde_json::json!({ "tabs": tabs }))
}

fn handle_new_tab(request: &Request, sm: &SessionManager) -> Response {
    let size = PtySize { rows: DEFAULT_ROWS, cols: DEFAULT_COLS, pixel_width: 0, pixel_height: 0 };

    match sm.create_pane(size, None, None, None, true) {
        Ok(id) => {
            Response::success(request.id.clone(), serde_json::json!({ "tab_id": id.to_string() }))
        }
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to create pane: {e}"),
        ),
    }
}

fn handle_close_tab(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    match sm.close_pane(id) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to close pane: {e}"),
        ),
    }
}

fn handle_focus_tab(request: &Request) -> Response {
    // Stub — requires GTK widget manipulation, deferred to T-051.
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_split_pane(request: &Request) -> Response {
    // Stub — requires GTK layout tree update, deferred to T-051.
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_send_input(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let data_b64 = match request.params.get("data").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: data".to_string(),
            )
        }
    };

    let bytes = match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
        Ok(b) => b,
        Err(_) => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "invalid base64 in data field".to_string(),
            )
        }
    };

    match sm.write_pty(id, &bytes) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to write PTY: {e}"),
        ),
    }
}

fn handle_get_screen(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // with_vt holds the mutex for the duration of the closure but does NOT
    // cross any await point (synchronous handler), so this is safe per R-1.
    let result = sm.with_vt(id, |terminal| {
        let screen = terminal.screen();
        let rows = screen.rows();
        let cols = screen.cols();

        let lines: Vec<String> = (0..rows)
            .map(|r| {
                let row = screen.row(r);
                let mut line = String::with_capacity(cols);
                for cell in row.iter().take(cols) {
                    line.push_str(&cell.grapheme);
                }
                // Trim trailing spaces to keep output tidy, but preserve length
                // contract by right-padding with spaces back to `cols`.
                let trimmed_len = line.trim_end_matches(' ').len();
                line.truncate(trimmed_len);
                // Pad to exactly `cols` characters so clients can rely on width.
                while line.chars().count() < cols {
                    line.push(' ');
                }
                line
            })
            .collect();

        let (cur_row, cur_col) = terminal.cursor();
        (lines, cur_row, cur_col)
    });

    match result {
        Ok((lines, cur_row, cur_col)) => Response::success(
            request.id.clone(),
            serde_json::json!({
                "lines": lines,
                "cursor": { "row": cur_row, "col": cur_col },
            }),
        ),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to read VT screen: {e}"),
        ),
    }
}

fn handle_get_pane_info(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // pane_info is guaranteed Some because require_pane_id already verified it.
    let info = sm.pane_info(id).expect("pane_info must be Some after require_pane_id");

    Response::success(
        request.id.clone(),
        serde_json::json!({
            "pane_id": info.id.to_string(),
            "rows": info.rows,
            "cols": info.cols,
            "title": info.title,
            "cwd": info.cwd.display().to_string(),
            "pid": info.pid,
        }),
    )
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use forgetty_session::SessionManager;

    fn make_request(method: &str) -> Request {
        Request {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: serde_json::Value::Null,
            id: Some(serde_json::Value::Number(1.into())),
        }
    }

    fn make_sm() -> Arc<SessionManager> {
        Arc::new(SessionManager::new())
    }

    #[test]
    fn dispatch_list_tabs_empty() {
        let sm = make_sm();
        let resp = dispatch(&make_request("list_tabs"), sm);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let tabs = resp.result.unwrap()["tabs"].as_array().unwrap().len();
        assert_eq!(tabs, 0);
    }

    #[test]
    fn dispatch_focus_tab_stub() {
        let sm = make_sm();
        let resp = dispatch(&make_request("focus_tab"), sm);
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["ok"], true);
    }

    #[test]
    fn dispatch_split_pane_stub() {
        let sm = make_sm();
        let resp = dispatch(&make_request("split_pane"), sm);
        assert!(resp.result.is_some());
    }

    #[test]
    fn dispatch_unknown_method() {
        let sm = make_sm();
        let resp = dispatch(&make_request("nonexistent"), sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::METHOD_NOT_FOUND);
    }

    #[test]
    fn dispatch_preserves_request_id() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "list_tabs".to_string(),
            params: serde_json::Value::Null,
            id: Some(serde_json::json!("abc-123")),
        };
        let resp = dispatch(&req, sm);
        assert_eq!(resp.id, Some(serde_json::json!("abc-123")));
    }

    #[test]
    fn send_input_missing_pane_id_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "send_input".to_string(),
            params: serde_json::json!({ "data": "dGVzdAo=" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn send_input_invalid_uuid_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "send_input".to_string(),
            params: serde_json::json!({ "pane_id": "not-a-uuid", "data": "dGVzdAo=" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn send_input_nonexistent_pane_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "send_input".to_string(),
            params: serde_json::json!({
                "pane_id": "00000000-0000-0000-0000-000000000000",
                "data": "dGVzdAo="
            }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn send_input_invalid_base64_returns_invalid_params() {
        let sm = make_sm();
        // Create a real pane so pane_id validation passes.
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id = sm.create_pane(size, None, None, None, true).expect("create pane");

        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "send_input".to_string(),
            params: serde_json::json!({ "pane_id": id.to_string(), "data": "!!!notbase64!!!" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, Arc::clone(&sm));
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, protocol::INVALID_PARAMS);

        sm.close_pane(id).ok();
    }

    #[test]
    fn get_screen_nonexistent_pane_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "get_screen".to_string(),
            params: serde_json::json!({ "pane_id": "00000000-0000-0000-0000-000000000000" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn get_pane_info_nonexistent_pane_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "get_pane_info".to_string(),
            params: serde_json::json!({ "pane_id": "00000000-0000-0000-0000-000000000000" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn close_tab_nonexistent_pane_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "close_tab".to_string(),
            params: serde_json::json!({ "pane_id": "00000000-0000-0000-0000-000000000000" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }
}
