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
        methods::SET_ACTIVE_WORKSPACE => handle_set_active_workspace(request, &sm),
        methods::SPLIT_PANE => handle_split_pane(request, &sm),
        methods::MOVE_TAB => handle_move_tab(request, &sm),
        methods::SEND_INPUT => handle_send_input(request, &sm),
        methods::GET_PANE_INFO => handle_get_pane_info(request, &sm),
        methods::RESIZE_PANE => handle_resize_pane(request, &sm),
        methods::SEND_SIGINT => handle_send_sigint(request, &sm),
        methods::NOTIFY => handle_notify(request, &sm),
        methods::CLOSE_PANE => handle_close_pane(request, &sm),
        methods::CREATE_WORKSPACE => handle_create_workspace(request, &sm),
        methods::RENAME_WORKSPACE => handle_rename_workspace(request, &sm),
        methods::DELETE_WORKSPACE => handle_delete_workspace(request, &sm),
        // Split ratio + pinned session methods (B-002).
        methods::UPDATE_SPLIT_RATIOS => handle_update_split_ratios(request, &sm),
        methods::SET_PINNED => handle_set_pinned(request, &sm),
        methods::GET_PINNED => handle_get_pinned(request, &sm),
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
    let workspace_idx =
        request.params.get("workspace_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let rows =
        request.params.get("rows").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_ROWS as u64) as u16;

    let cols =
        request.params.get("cols").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_COLS as u64) as u16;

    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    let cwd: Option<PathBuf> = request
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir()); // silently ignore nonexistent dirs

    // Optional profile command: an array of strings from the GTK client.
    let command: Option<Vec<String>> = request
        .params
        .get("command")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .filter(|v: &Vec<String>| !v.is_empty());

    match sm.create_tab(workspace_idx, cwd, size, command) {
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

        match sm.close_tab(tab_uuid) {
            Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
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
            // Drop layout before calling close_tab (no reentry).
            drop(layout);

            match sm.close_tab(tab_uuid) {
                Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
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
                Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
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

/// Handle `set_active_workspace` RPC — persist the globally-active workspace
/// index so session-restore brings the correct workspace back on cold start.
fn handle_set_active_workspace(request: &Request, sm: &SessionManager) -> Response {
    let ws_idx = match request.params.get("workspace_idx").and_then(|v| v.as_u64()) {
        Some(n) => n as usize,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: workspace_idx".to_string(),
            )
        }
    };

    match sm.set_active_workspace(ws_idx) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INVALID_PARAMS,
            format!("set_active_workspace failed: {e}"),
        ),
    }
}

/// Handle `close_pane` RPC — close a single pane within a split.
///
/// If the pane is the sole leaf of its tab, the entire tab is closed (same
/// behaviour as `close_tab`). If it is part of a split, only this pane is
/// removed and the sibling promoted.
fn handle_close_pane(request: &Request, sm: &SessionManager) -> Response {
    let pane_id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // Determine if this pane is the sole leaf of its owning tab and find the tab_id.
    let (tab_uuid, is_sole_in_tab) = {
        let layout = sm.layout();
        let mut result = (None, true);
        'outer: for ws in &layout.workspaces {
            for tab in &ws.tabs {
                if tab_contains_pane(tab, pane_id) {
                    let mut leaf_ids = Vec::new();
                    collect_all_pane_ids(&tab.pane_tree, &mut leaf_ids);
                    result = (Some(tab.id), leaf_ids.len() <= 1);
                    break 'outer;
                }
            }
        }
        result
    };

    if is_sole_in_tab {
        // Close the entire tab that owns this pane.
        match tab_uuid {
            Some(tid) => match sm.close_tab(tid) {
                Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
                Err(e) => Response::error(
                    request.id.clone(),
                    protocol::INTERNAL_ERROR,
                    format!("close_pane (via close_tab) failed: {e}"),
                ),
            },
            None => {
                // Pane in registry but not in any tab tree — legacy fallback.
                match sm.close_pane(pane_id) {
                    Ok(()) => {
                        Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
                    }
                    Err(e) => Response::error(
                        request.id.clone(),
                        protocol::INTERNAL_ERROR,
                        format!("close_pane (legacy fallback) failed: {e}"),
                    ),
                }
            }
        }
    } else {
        // Pane is part of a split — kill only this pane.
        match sm.close_pane(pane_id) {
            Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
            Err(e) => Response::error(
                request.id.clone(),
                protocol::INTERNAL_ERROR,
                format!("close_pane (split leaf) failed: {e}"),
            ),
        }
    }
}

fn handle_create_workspace(request: &Request, sm: &SessionManager) -> Response {
    let name =
        request.params.get("name").and_then(|v| v.as_str()).unwrap_or("Workspace").to_string();

    let (workspace_id, workspace_idx) = sm.create_workspace(&name);

    let default_size =
        PtySize { rows: DEFAULT_ROWS, cols: DEFAULT_COLS, pixel_width: 0, pixel_height: 0 };
    match sm.create_tab(workspace_idx, None, default_size, None) {
        Ok((pane_id, tab_id)) => Response::success(
            request.id.clone(),
            serde_json::json!({
                "workspace_id": workspace_id.to_string(),
                "workspace_idx": workspace_idx,
                "pane_id": pane_id.to_string(),
                "tab_id": tab_id.to_string(),
            }),
        ),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("create_workspace: create_tab failed: {e}"),
        ),
    }
}

/// Handle `rename_workspace` RPC (FIX-001).
///
/// Params: `{ "workspace_idx": u64, "name": "..." }`.
/// Success: `{ "ok": true }`. Out-of-range index returns `INTERNAL_ERROR` with
/// the bounds message from `SessionManager::rename_workspace`.
fn handle_rename_workspace(request: &Request, sm: &SessionManager) -> Response {
    let workspace_idx = match request.params.get("workspace_idx").and_then(|v| v.as_u64()) {
        Some(n) => n as usize,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: workspace_idx (u64)".to_string(),
            )
        }
    };

    let name = match request.params.get("name").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: name".to_string(),
            )
        }
    };

    match sm.rename_workspace(workspace_idx, name) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("rename_workspace failed: {e}"),
        ),
    }
}

/// Handle `delete_workspace` RPC (FIX-003).
///
/// Params: `{ "workspace_idx": u64 }`.
/// Success: `{ "ok": true }`. Out-of-range index or last-workspace deletion
/// return `INTERNAL_ERROR` with the message from `SessionManager::delete_workspace`.
fn handle_delete_workspace(request: &Request, sm: &SessionManager) -> Response {
    let workspace_idx = match request.params.get("workspace_idx").and_then(|v| v.as_u64()) {
        Some(n) => n as usize,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: workspace_idx (u64)".to_string(),
            )
        }
    };

    match sm.delete_workspace(workspace_idx) {
        Ok(()) => Response::success(request.id.clone(), serde_json::json!({ "ok": true })),
        Err(e) => Response::error(
            request.id.clone(),
            protocol::INTERNAL_ERROR,
            format!("delete_workspace failed: {e}"),
        ),
    }
}

fn handle_update_split_ratios(request: &Request, sm: &SessionManager) -> Response {
    let ratios = match request.params.get("ratios").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: ratios (array of {pane_id, ratio})".to_string(),
            )
        }
    };

    let mut updates = Vec::new();
    for entry in ratios {
        let pane_id_str = match entry.get("pane_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let uuid = match Uuid::parse_str(pane_id_str) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let ratio = match entry.get("ratio").and_then(|v| v.as_f64()) {
            Some(r) => r as f32,
            None => continue,
        };
        updates.push((PaneId(uuid), ratio));
    }

    sm.update_split_ratios(&updates);
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_set_pinned(request: &Request, sm: &SessionManager) -> Response {
    let pinned = match request.params.get("pinned").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => {
            return Response::error(
                request.id.clone(),
                protocol::INVALID_PARAMS,
                "missing param: pinned (bool)".to_string(),
            )
        }
    };
    sm.set_pinned(pinned);
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_get_pinned(request: &Request, sm: &SessionManager) -> Response {
    let pinned = sm.is_pinned();
    Response::success(request.id.clone(), serde_json::json!({ "pinned": pinned }))
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

    let rows =
        request.params.get("rows").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_ROWS as u64) as u16;

    let cols =
        request.params.get("cols").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_COLS as u64) as u16;

    let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };

    // Use the explicitly-provided CWD, or inherit the source pane's CWD so that
    // the new split opens in the same directory as the pane being split.
    let cwd: Option<PathBuf> = request
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| sm.pane_info(pane_id).map(|info| info.cwd));

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
// Client-side OSC notification log (V2-006)
// ---------------------------------------------------------------------------

/// Handle the `notify` RPC — a client → daemon advisory log entry.
///
/// The GTK client has already detected an OSC 9/99/777 notification locally
/// and presented it (tab badge, desktop notification, click-to-focus). This
/// RPC is fire-and-forget from the client's perspective: it exists so the
/// daemon can log the event for audit, planned MCP observability, and as a
/// seam for future cross-device fanout (per SPEC §10 follow-up).
///
/// The handler is intentionally minimal: validate params, log, return `ok`.
/// It does **not** modify session state, emit broadcast events, or touch
/// `SyncEndpoint` (AD-015 preserved).
fn handle_notify(request: &Request, sm: &SessionManager) -> Response {
    let pane_id = match require_pane_id(request, sm) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // `body` is accepted in params but intentionally not logged —
    // it can be long; keep log lines bounded.
    let title = request.params.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let source = request.params.get("source").and_then(|v| v.as_str()).unwrap_or("");

    tracing::info!(
        target: "forgetty_socket::notify",
        pane = %pane_id, source, title,
        "OSC notification reported by client"
    );

    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
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

    #[tokio::test]
    async fn dispatch_focus_tab_real_tab_succeeds() {
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

    #[tokio::test]
    async fn dispatch_split_pane_invalid_direction_returns_invalid_params() {
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

    #[tokio::test]
    async fn send_input_invalid_base64_returns_invalid_params() {
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

    #[tokio::test]
    async fn dispatch_new_tab_returns_tab_id_and_pane_id() {
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
        assert!(
            result.get("pane_id").and_then(|v| v.as_str()).is_some(),
            "pane_id must be present"
        );

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

    // -----------------------------------------------------------------------
    // FIX-003 — delete_workspace RPC dispatch tests
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_delete_workspace_missing_idx_returns_invalid_params() {
        let sm = make_sm();
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "delete_workspace".to_string(),
            params: serde_json::json!({}),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, sm, None);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_delete_workspace_last_workspace_rejected() {
        let sm = make_sm();
        // Default session has exactly one workspace (idx 0) — deleting it
        // must be rejected by the daemon even though the UI also disables
        // the menu entry (defense-in-depth).
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "delete_workspace".to_string(),
            params: serde_json::json!({ "workspace_idx": 0 }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, Arc::clone(&sm), None);
        assert!(resp.error.is_some(), "last-workspace delete must error");
        let err = resp.error.unwrap();
        assert_eq!(err.code, protocol::INTERNAL_ERROR);
        assert!(
            err.message.contains("last remaining workspace"),
            "error message must surface the bounds reason; got: {}",
            err.message
        );
        // Workspace list unchanged.
        assert_eq!(sm.layout().workspaces.len(), 1);
    }

    #[test]
    fn dispatch_delete_workspace_out_of_bounds_returns_error() {
        let sm = make_sm();
        sm.create_workspace("Second");
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "delete_workspace".to_string(),
            params: serde_json::json!({ "workspace_idx": 99 }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, Arc::clone(&sm), None);
        assert!(resp.error.is_some(), "out-of-range delete must error");
        assert_eq!(resp.error.unwrap().code, protocol::INTERNAL_ERROR);
        assert_eq!(sm.layout().workspaces.len(), 2, "workspace list must be untouched");
    }

    #[test]
    fn dispatch_delete_workspace_success() {
        let sm = make_sm();
        let (_, ws_idx) = sm.create_workspace("DropMe");
        assert_eq!(sm.layout().workspaces.len(), 2);

        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "delete_workspace".to_string(),
            params: serde_json::json!({ "workspace_idx": ws_idx }),
            id: Some(serde_json::json!(1)),
        };
        let resp = dispatch(&req, Arc::clone(&sm), None);
        assert!(resp.result.is_some(), "successful delete must return a result");
        let result = resp.result.unwrap();
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(sm.layout().workspaces.len(), 1);
    }
}
