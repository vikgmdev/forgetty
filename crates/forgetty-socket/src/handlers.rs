//! Request handlers for the JSON-RPC socket server.
//!
//! Each handler processes a specific JSON-RPC method and delegates to the
//! real `SessionManager` for PTY state. The `dispatch` function routes
//! synchronous (non-streaming) methods; `subscribe_output` is handled
//! directly in `server.rs` because it requires an async streaming loop.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::layout::{SessionLayout, SessionTab};
use forgetty_session::workspace::PaneTreeLayout;
use forgetty_session::SessionManager;
use forgetty_sync::SyncEndpoint;
use forgetty_vt::{Cell, CellAttributes, Color};
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
///
/// `sync_endpoint` is `None` when the daemon was started without iroh support;
/// sync-related methods return a graceful `METHOD_NOT_FOUND` in that case.
pub fn dispatch(
    request: &Request,
    sm: Arc<SessionManager>,
    sync_endpoint: Option<Arc<SyncEndpoint>>,
) -> Response {
    match request.method.as_str() {
        methods::LIST_TABS => handle_list_tabs(request, &sm),
        methods::GET_LAYOUT => handle_get_layout(request, &sm),
        methods::NEW_TAB => handle_new_tab(request, &sm),
        methods::CLOSE_TAB => handle_close_tab(request, &sm),
        methods::FOCUS_TAB => handle_focus_tab(request, &sm),
        methods::SPLIT_PANE => handle_split_pane(request, &sm),
        methods::MOVE_TAB => handle_move_tab(request, &sm),
        methods::SEND_INPUT => handle_send_input(request, &sm),
        methods::GET_SCREEN => handle_get_screen(request, &sm),
        methods::GET_PANE_INFO => handle_get_pane_info(request, &sm),
        methods::RESIZE_PANE => handle_resize_pane(request, &sm),
        methods::SEND_SIGINT => handle_send_sigint(request, &sm),
        methods::PRESEED_SNAPSHOT => handle_preseed_snapshot(request, &sm),
        // Sync / pairing methods — require sync_endpoint.
        methods::LIST_DEVICES => handle_list_devices(request, sync_endpoint.as_deref()),
        methods::REVOKE_DEVICE => handle_revoke_device(request, sync_endpoint.as_deref()),
        methods::GET_PAIRING_INFO => handle_get_pairing_info(request, sync_endpoint.as_deref()),
        methods::ENABLE_PAIRING => handle_enable_pairing(request, sync_endpoint.as_deref()),
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
fn require_pane_id(request: &Request, sm: &SessionManager) -> Result<PaneId, Response> {
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

fn handle_get_layout(request: &Request, sm: &SessionManager) -> Response {
    let layout = sm.layout();
    match serde_json::to_value(&layout) {
        Ok(v) => Response::success(request.id.clone(), v),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to serialize layout: {e}"),
        ),
    }
}

fn handle_new_tab(request: &Request, sm: &SessionManager) -> Response {
    let workspace_idx = request
        .params
        .get("workspace_idx")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    let rows = request
        .params
        .get("rows")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_ROWS as u64) as u16;

    let cols = request
        .params
        .get("cols")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_COLS as u64) as u16;

    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    let cwd: Option<PathBuf> = request
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir()); // silently ignore nonexistent dirs

    match sm.create_tab(workspace_idx, cwd, size) {
        Ok((pane_id, tab_id)) => Response::success(
            request.id.clone(),
            serde_json::json!({
                "tab_id": tab_id.to_string(),
                "pane_id": pane_id.to_string(),
            }),
        ),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to create tab: {e}"),
        ),
    }
}

fn handle_close_tab(request: &Request, sm: &SessionManager) -> Response {
    // Try tab_id first (preferred path).
    if let Some(tab_id_str) = request.params.get("tab_id").and_then(|v| v.as_str()) {
        let tab_uuid = match Uuid::parse_str(tab_id_str) {
            Ok(u) => u,
            Err(_) => {
                return Response::error(
                    request.id.clone(),
                    protocol::INVALID_PARAMS,
                    format!("invalid UUID: {tab_id_str}"),
                )
            }
        };

        // Collect pane IDs from this tab BEFORE closing, for snapshot cleanup.
        let pane_ids_to_clean: Vec<PaneId> = {
            let layout = sm.layout();
            let mut ids = Vec::new();
            'outer: for ws in &layout.workspaces {
                for tab in &ws.tabs {
                    if tab.id == tab_uuid {
                        collect_all_pane_ids(&tab.pane_tree, &mut ids);
                        break 'outer;
                    }
                }
            }
            ids
        };

        match sm.close_tab(tab_uuid) {
            Ok(()) => {
                for pid in pane_ids_to_clean {
                    forgetty_workspace::delete_vt_snapshot(pid.0);
                }
                Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
            }
            Err(e) => Response::error(
                request.id.clone(),
                protocol::INTERNAL_ERROR,
                format!("failed to close tab: {e}"),
            ),
        }
    } else if let Some(pane_id_str) = request.params.get("pane_id").and_then(|v| v.as_str()) {
        // Legacy path: pane_id was provided.
        let pane_uuid = match Uuid::parse_str(pane_id_str) {
            Ok(u) => u,
            Err(_) => {
                return Response::error(
                    request.id.clone(),
                    protocol::INVALID_PARAMS,
                    format!("invalid UUID: {pane_id_str}"),
                )
            }
        };
        let pane_id = PaneId(pane_uuid);

        // Verify pane is alive.
        if sm.pane_info(pane_id).is_none() {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                format!("pane not found: {pane_id_str}"),
            );
        }

        // Try to find the tab that owns this pane.
        let layout = sm.layout();
        if let Some(tab_uuid) = find_tab_for_pane(&layout, pane_id) {
            // Collect all pane IDs in the tab for snapshot cleanup.
            let pane_ids_to_clean: Vec<PaneId> = {
                let mut ids = Vec::new();
                'outer: for ws in &layout.workspaces {
                    for tab in &ws.tabs {
                        if tab.id == tab_uuid {
                            collect_all_pane_ids(&tab.pane_tree, &mut ids);
                            break 'outer;
                        }
                    }
                }
                ids
            };
            // Drop layout before calling close_tab (no reentry).
            drop(layout);

            match sm.close_tab(tab_uuid) {
                Ok(()) => {
                    for pid in pane_ids_to_clean {
                        forgetty_workspace::delete_vt_snapshot(pid.0);
                    }
                    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
                }
                Err(e) => Response::error(
                    request.id.clone(),
                    protocol::INTERNAL_ERROR,
                    format!("failed to close tab: {e}"),
                ),
            }
        } else {
            // Pane exists in registry but not in any tab tree — legacy fallback.
            drop(layout);
            match sm.close_pane(pane_id) {
                Ok(()) => {
                    forgetty_workspace::delete_vt_snapshot(pane_id.0);
                    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
                }
                Err(e) => Response::error(
                    request.id.clone(),
                    protocol::INTERNAL_ERROR,
                    format!("failed to close pane: {e}"),
                ),
            }
        }
    } else {
        Response::error(
            request.id.clone(),
            protocol::INVALID_PARAMS,
            "missing param: tab_id or pane_id".to_string(),
        )
    }
}

fn handle_focus_tab(request: &Request, sm: &SessionManager) -> Response {
    let tab_id_str = match request.params.get("tab_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: tab_id".to_string(),
            )
        }
    };

    let tab_uuid = match Uuid::parse_str(&tab_id_str) {
        Ok(u) => u,
        Err(_) => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                format!("invalid UUID: {tab_id_str}"),
            )
        }
    };

    // Find which workspace and tab index the tab lives in.
    let layout = sm.layout();
    let mut found: Option<(usize, usize)> = None;
    'outer: for (ws_idx, ws) in layout.workspaces.iter().enumerate() {
        for (tab_idx, tab) in ws.tabs.iter().enumerate() {
            if tab.id == tab_uuid {
                found = Some((ws_idx, tab_idx));
                break 'outer;
            }
        }
    }
    drop(layout);

    let (ws_idx, tab_idx) = match found {
        Some(pair) => pair,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                format!("tab not found: {tab_id_str}"),
            )
        }
    };

    match sm.set_active_tab(ws_idx, tab_idx) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("set_active_tab failed: {e}"),
        ),
    }
}

fn handle_split_pane(request: &Request, sm: &SessionManager) -> Response {
    let pane_id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let direction = match request.params.get("direction").and_then(|v| v.as_str()) {
        Some(d) => d.to_string(),
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: direction".to_string(),
            )
        }
    };

    if direction != "horizontal" && direction != "vertical" {
        return Response::error(
            request.id.clone(),
            protocol::INVALID_PARAMS,
            "direction must be 'horizontal' or 'vertical'".to_string(),
        );
    }

    let rows = request
        .params
        .get("rows")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_ROWS as u64) as u16;

    let cols = request
        .params
        .get("cols")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_COLS as u64) as u16;

    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    let cwd: Option<PathBuf> = request
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir());

    match sm.split_pane(pane_id, &direction, size, cwd) {
        Ok(new_pane_id) => Response::success(
            request.id.clone(),
            serde_json::json!({ "pane_id": new_pane_id.to_string() }),
        ),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("split_pane failed: {e}"),
        ),
    }
}

fn handle_move_tab(request: &Request, sm: &SessionManager) -> Response {
    let tab_id_str = match request.params.get("tab_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: tab_id".to_string(),
            )
        }
    };

    let tab_uuid = match Uuid::parse_str(&tab_id_str) {
        Ok(u) => u,
        Err(_) => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                format!("invalid UUID: {tab_id_str}"),
            )
        }
    };

    let new_index = match request.params.get("new_index").and_then(|v| v.as_u64()) {
        Some(n) => n as usize,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: new_index".to_string(),
            )
        }
    };

    match sm.move_tab(tab_uuid, new_index) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("move_tab failed: {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Private helpers for layout walking
// ---------------------------------------------------------------------------

/// Walk a `PaneTreeLayout` recursively, collecting all leaf pane IDs into `out`.
fn collect_all_pane_ids(tree: &PaneTreeLayout, out: &mut Vec<PaneId>) {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => out.push(*pane_id),
        PaneTreeLayout::Split { first, second, .. } => {
            collect_all_pane_ids(first, out);
            collect_all_pane_ids(second, out);
        }
    }
}

/// Walk the `SessionLayout` to find the `tab.id` (Uuid) of the tab whose pane
/// tree contains the given `pane_id`. Returns `None` if not found.
fn find_tab_for_pane(layout: &SessionLayout, pane_id: PaneId) -> Option<Uuid> {
    for ws in &layout.workspaces {
        for tab in &ws.tabs {
            if tab_contains_pane(tab, pane_id) {
                return Some(tab.id);
            }
        }
    }
    None
}

fn tab_contains_pane(tab: &SessionTab, pane_id: PaneId) -> bool {
    tree_contains_pane(&tab.pane_tree, pane_id)
}

fn tree_contains_pane(tree: &PaneTreeLayout, pane_id: PaneId) -> bool {
    match tree {
        PaneTreeLayout::Leaf { pane_id: id } => *id == pane_id,
        PaneTreeLayout::Split { first, second, .. } => {
            tree_contains_pane(first, pane_id) || tree_contains_pane(second, pane_id)
        }
    }
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

/// Serialize a single terminal row to an ANSI-escaped string.
///
/// Trailing blank cells with default attributes are omitted to keep payloads
/// small. Any open SGR sequence is closed at the end of each line so that
/// rows don't bleed into each other when replayed into the VT.
fn serialize_row_ansi(row: &[Cell]) -> String {
    // Find the last cell with non-default content so we can stop
    // emitting escape codes after it (trailing blank cells are
    // skipped to keep snapshot payloads small).
    let content_end = row
        .iter()
        .rposition(|c| c.grapheme != " " || c.attrs != CellAttributes::default())
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut line = String::new();
    let mut prev_fg = Color::Default;
    let mut prev_bg = Color::Default;
    let mut prev_bold = false;
    let mut prev_italic = false;
    let mut prev_underline = false;
    let mut prev_strike = false;
    let mut prev_inverse = false;
    let mut prev_dim = false;
    let mut emitted_escape = false;

    for cell in row.iter().take(content_end) {
        let a = &cell.attrs;
        let changed = a.fg != prev_fg
            || a.bg != prev_bg
            || a.bold != prev_bold
            || a.italic != prev_italic
            || a.underline != prev_underline
            || a.strikethrough != prev_strike
            || a.inverse != prev_inverse
            || a.dim != prev_dim;

        if changed {
            // Reset then re-emit all non-default attributes.
            line.push_str("\x1b[0m");
            if a.bold {
                line.push_str("\x1b[1m");
            }
            if a.dim {
                line.push_str("\x1b[2m");
            }
            if a.italic {
                line.push_str("\x1b[3m");
            }
            if a.underline {
                line.push_str("\x1b[4m");
            }
            if a.inverse {
                line.push_str("\x1b[7m");
            }
            if a.strikethrough {
                line.push_str("\x1b[9m");
            }
            if let Color::Rgb(r, g, b) = a.fg {
                line.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
            }
            if let Color::Rgb(r, g, b) = a.bg {
                line.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
            }
            prev_fg = a.fg;
            prev_bg = a.bg;
            prev_bold = a.bold;
            prev_italic = a.italic;
            prev_underline = a.underline;
            prev_strike = a.strikethrough;
            prev_inverse = a.inverse;
            prev_dim = a.dim;
            emitted_escape = true;
        }

        line.push_str(&cell.grapheme);
    }

    // Terminate any open SGR sequence so lines don't bleed into
    // each other when the snapshot is replayed into the VT.
    if emitted_escape {
        line.push_str("\x1b[0m");
    }

    line
}

/// Save VT snapshots for all live panes to disk.
///
/// Called by the daemon on SIGTERM/SIGINT before killing PTY processes.
/// Returns the number of snapshots successfully written.
pub fn save_all_snapshots(sm: &SessionManager) -> usize {
    let pane_ids = sm.list_panes();
    let mut saved = 0usize;
    for id in &pane_ids {
        let result = sm.with_vt(*id, |terminal| {
            let screen = terminal.screen();
            let rows = screen.rows();
            let lines: Vec<String> = (0..rows).map(|r| serialize_row_ansi(screen.row(r))).collect();
            let (cur_row, cur_col) = terminal.cursor();
            (lines, cur_row, cur_col)
        });
        if let Ok((lines, cur_row, cur_col)) = result {
            if forgetty_workspace::save_vt_snapshot(id.0, &lines, cur_row, cur_col).is_ok() {
                saved += 1;
            }
        }
    }
    saved
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
        let lines: Vec<String> = (0..rows).map(|r| serialize_row_ansi(screen.row(r))).collect();
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

    let info = match sm.pane_info(id) {
        Some(i) => i,
        None => {
            return Response::error(
                request.id.clone(),
                crate::protocol::INVALID_PARAMS,
                format!("pane not found: {id}"),
            );
        }
    };

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

fn handle_resize_pane(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let rows = match request.params.get("rows").and_then(|v| v.as_u64()) {
        Some(r) => r as u16,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: rows".to_string(),
            )
        }
    };

    let cols = match request.params.get("cols").and_then(|v| v.as_u64()) {
        Some(c) => c as u16,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: cols".to_string(),
            )
        }
    };

    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    match sm.resize_pane(id, size) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to resize pane: {e}"),
        ),
    }
}

fn handle_send_sigint(request: &Request, sm: &SessionManager) -> Response {
    let id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // Write 0x03 (ETX / Ctrl+C) to the PTY.
    // The daemon owns the master PTY fd; it can also do the kill(-pgid, SIGINT).
    match sm.write_pty(id, &[0x03]) {
        Ok(()) => {
            // Also send SIGINT to the foreground process group via the session manager.
            sm.send_sigint(id);
            Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to send SIGINT: {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// VT snapshot handler (T-058)
// ---------------------------------------------------------------------------

fn handle_preseed_snapshot(request: &Request, sm: &SessionManager) -> Response {
    // params: { "pane_id": "<new live pane>", "snapshot_id": "<old saved pane>" }
    let new_pane_id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    let snapshot_uuid = match request
        .params
        .get("snapshot_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing param: snapshot_id".to_string())
        .and_then(|s| Uuid::parse_str(s).map_err(|e| format!("invalid snapshot_id UUID: {e}")))
    {
        Ok(u) => u,
        Err(msg) => {
            return Response::error(request.id.clone(), protocol::INVALID_PARAMS, msg);
        }
    };

    let Some((lines, cur_row, cur_col)) = forgetty_workspace::load_vt_snapshot(snapshot_uuid)
    else {
        return Response::success(
            request.id.clone(),
            serde_json::json!({ "ok": true, "seeded": false }),
        );
    };

    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(b"\x1b[2J\x1b[H"); // clear + home
    for (i, line) in lines.iter().enumerate() {
        payload.extend_from_slice(format!("\x1b[{};1H{}", i + 1, line).as_bytes());
    }
    payload.extend_from_slice(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).as_bytes());

    match sm.with_vt_mut(new_pane_id, |t| t.feed(&payload)) {
        Ok(()) => {
            forgetty_workspace::delete_vt_snapshot(snapshot_uuid);
            Response::success(request.id.clone(), serde_json::json!({ "ok": true, "seeded": true }))
        }
        Err(e) => Response::error(request.id.clone(), protocol::INTERNAL_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Sync / pairing handlers (T-052)
// ---------------------------------------------------------------------------

/// Return a "sync not available" error when the daemon was started without iroh.
fn sync_unavailable(request: &Request) -> Response {
    Response::error(
        request.id.clone(),
        protocol::METHOD_NOT_FOUND,
        "sync endpoint not available (daemon started without --allow-pairing?)".to_string(),
    )
}

fn handle_list_devices(request: &Request, se: Option<&SyncEndpoint>) -> Response {
    let Some(se) = se else { return sync_unavailable(request) };
    let registry = se.registry();
    let reg = registry.lock().unwrap();
    let devices: Vec<serde_json::Value> = reg
        .list()
        .iter()
        .map(|d| {
            serde_json::json!({
                "device_id": d.device_id,
                "name": d.name,
                "paired_at": d.paired_at,
                "last_seen": d.last_seen,
            })
        })
        .collect();
    Response::success(request.id.clone(), serde_json::json!({ "devices": devices }))
}

fn handle_revoke_device(request: &Request, se: Option<&SyncEndpoint>) -> Response {
    let Some(se) = se else { return sync_unavailable(request) };

    let device_id = match request.params.get("device_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: device_id".to_string(),
            )
        }
    };

    let registry = se.registry();
    let mut reg = registry.lock().unwrap();
    match reg.remove(&device_id) {
        Ok(found) => {
            if found {
                // Emit revoke event (best-effort; ignore if no receivers).
                let _ = se
                    .event_tx
                    .send(forgetty_sync::SyncEvent::DeviceRevoked { device_id: device_id.clone() });
            }
            Response::success(request.id.clone(), serde_json::json!({ "ok": found }))
        }
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("failed to revoke device: {e}"),
        ),
    }
}

fn handle_enable_pairing(request: &Request, se: Option<&SyncEndpoint>) -> Response {
    let Some(se) = se else { return sync_unavailable(request) };
    let secs = request.params.get("secs").and_then(|v| v.as_u64()).unwrap_or(120);
    se.enable_pairing(secs);
    Response::success(request.id.clone(), serde_json::json!({ "ok": true, "secs": secs }))
}

fn handle_get_pairing_info(request: &Request, se: Option<&SyncEndpoint>) -> Response {
    let Some(se) = se else { return sync_unavailable(request) };

    let node_id = se.node_id().to_string();
    let machine = hostname::get()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());

    let payload = forgetty_sync::QrPayload::new(node_id.clone());
    let png_bytes = match forgetty_sync::qr::qr_to_png(&payload, 8) {
        Ok(b) => b,
        Err(e) => {
            return Response::error(
                request.id.clone(),
                protocol::INTERNAL_ERROR,
                format!("QR PNG generation failed: {e}"),
            )
        }
    };

    let qr_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Response::success(
        request.id.clone(),
        serde_json::json!({
            "node_id": node_id,
            "machine": machine,
            "qr_png_base64": qr_b64,
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
        let resp = dispatch(&make_request("list_tabs"), sm, None);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let tabs = resp.result.unwrap()["tabs"].as_array().unwrap().len();
        assert_eq!(tabs, 0);
    }

    #[test]
    fn dispatch_focus_tab_missing_tab_id_returns_invalid_params() {
        let sm = make_sm();
        // No tab_id in params → INVALID_PARAMS.
        let resp = dispatch(&make_request("focus_tab"), sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_focus_tab_unknown_uuid_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "focus_tab".to_string(),
            params: serde_json::json!({ "tab_id": "00000000-0000-0000-0000-000000000000" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_focus_tab_real_tab_succeeds() {
        let sm = make_sm();
        // Create a tab via new_tab RPC.
        let new_req = Request {
            jsonrpc: "2.0".to_string(),
            method: "new_tab".to_string(),
            params: serde_json::json!({}),
            id: Some(serde_json::json!(1)),
        };
        let new_resp = dispatch(&new_req, Arc::clone(&sm), None);
        assert!(new_resp.result.is_some(), "new_tab should succeed");
        let tab_id = new_resp.result.unwrap()["tab_id"].as_str().unwrap().to_string();

        let focus_req = Request {
            jsonrpc: "2.0".to_string(),
            method: "focus_tab".to_string(),
            params: serde_json::json!({ "tab_id": tab_id }),
            id: Some(serde_json::json!(2)),
        };
        let resp = dispatch(&focus_req, Arc::clone(&sm), None);
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["ok"], true);

        // Cleanup.
        let layout = sm.layout();
        let tab_uuid = uuid::Uuid::parse_str(&tab_id).unwrap();
        sm.close_tab(tab_uuid).ok();
        drop(layout);
    }

    #[test]
    fn dispatch_split_pane_missing_pane_id_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "split_pane".to_string(),
            params: serde_json::json!({ "direction": "horizontal" }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_split_pane_invalid_direction_returns_invalid_params() {
        let sm = make_sm();
        // Create a real pane first.
        let new_req = Request {
            jsonrpc: "2.0".to_string(),
            method: "new_tab".to_string(),
            params: serde_json::json!({}),
            id: Some(serde_json::json!(1)),
        };
        let new_resp = dispatch(&new_req, Arc::clone(&sm), None);
        let pane_id = new_resp.result.unwrap()["pane_id"].as_str().unwrap().to_string();

        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "split_pane".to_string(),
            params: serde_json::json!({ "pane_id": pane_id, "direction": "diagonal" }),
            id: Some(serde_json::json!(2)),
        };
        let resp = dispatch(&req, Arc::clone(&sm), None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_unknown_method() {
        let sm = make_sm();
        let resp = dispatch(&make_request("nonexistent"), sm, None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, Arc::clone(&sm), None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, sm, None);
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
        let resp = dispatch(&req, sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn close_tab_missing_params_returns_invalid_params() {
        let sm = make_sm();
        let resp = dispatch(&make_request("close_tab"), sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_get_layout_empty() {
        let sm = make_sm();
        let resp = dispatch(&make_request("get_layout"), sm, None);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        // Fresh daemon: one default workspace, zero tabs.
        let workspaces = result["workspaces"].as_array().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["name"], "Default");
        assert_eq!(workspaces[0]["tabs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn dispatch_new_tab_returns_tab_id_and_pane_id() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "new_tab".to_string(),
            params: serde_json::json!({}),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, Arc::clone(&sm), None);
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        assert!(result.get("tab_id").and_then(|v| v.as_str()).is_some(), "tab_id must be present");
        assert!(result.get("pane_id").and_then(|v| v.as_str()).is_some(), "pane_id must be present");

        // Cleanup.
        let tab_id_str = result["tab_id"].as_str().unwrap();
        let tab_uuid = uuid::Uuid::parse_str(tab_id_str).unwrap();
        sm.close_tab(tab_uuid).ok();
    }

    #[test]
    fn dispatch_move_tab_missing_params_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "move_tab".to_string(),
            params: serde_json::json!({ "tab_id": "00000000-0000-0000-0000-000000000000" }),
            id: Some(serde_json::json!(1)),
        };
        // missing new_index
        let resp = dispatch(&req, sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }
}
