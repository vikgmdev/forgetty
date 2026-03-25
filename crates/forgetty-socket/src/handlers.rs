//! Request handlers for the JSON-RPC socket server.
//!
//! Each handler processes a specific JSON-RPC method. These are currently
//! stubs returning placeholder data; actual integration with the App will
//! happen when the socket server is connected to the main event loop.

use crate::protocol::{self, methods, Request, Response};

/// Dispatch a JSON-RPC request to the appropriate handler.
pub fn dispatch(request: &Request) -> Response {
    match request.method.as_str() {
        methods::LIST_TABS => handle_list_tabs(request),
        methods::NEW_TAB => handle_new_tab(request),
        methods::CLOSE_TAB => handle_close_tab(request),
        methods::FOCUS_TAB => handle_focus_tab(request),
        methods::SPLIT_PANE => handle_split_pane(request),
        methods::SEND_INPUT => handle_send_input(request),
        methods::GET_SCREEN => handle_get_screen(request),
        methods::GET_PANE_INFO => handle_get_pane_info(request),
        _ => Response::error(
            request.id.clone(),
            protocol::METHOD_NOT_FOUND,
            format!("Unknown method: {}", request.method),
        ),
    }
}

fn handle_list_tabs(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "tabs": [] }))
}

fn handle_new_tab(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "tab_id": 0 }))
}

fn handle_close_tab(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_focus_tab(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_split_pane(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "pane_id": 0 }))
}

fn handle_send_input(request: &Request) -> Response {
    Response::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

fn handle_get_screen(request: &Request) -> Response {
    Response::success(
        request.id.clone(),
        serde_json::json!({ "lines": [], "cursor": { "row": 0, "col": 0 } }),
    )
}

fn handle_get_pane_info(request: &Request) -> Response {
    Response::success(
        request.id.clone(),
        serde_json::json!({
            "pane_id": 0,
            "rows": 24,
            "cols": 80,
            "title": ""
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str) -> Request {
        Request {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: serde_json::Value::Null,
            id: Some(serde_json::Value::Number(1.into())),
        }
    }

    #[test]
    fn dispatch_list_tabs() {
        let resp = dispatch(&make_request("list_tabs"));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn dispatch_new_tab() {
        let resp = dispatch(&make_request("new_tab"));
        let result = resp.result.unwrap();
        assert!(result.get("tab_id").is_some());
    }

    #[test]
    fn dispatch_close_tab() {
        let resp = dispatch(&make_request("close_tab"));
        assert!(resp.result.is_some());
    }

    #[test]
    fn dispatch_focus_tab() {
        let resp = dispatch(&make_request("focus_tab"));
        assert!(resp.result.is_some());
    }

    #[test]
    fn dispatch_split_pane() {
        let resp = dispatch(&make_request("split_pane"));
        let result = resp.result.unwrap();
        assert!(result.get("pane_id").is_some());
    }

    #[test]
    fn dispatch_send_input() {
        let resp = dispatch(&make_request("send_input"));
        assert!(resp.result.is_some());
    }

    #[test]
    fn dispatch_get_screen() {
        let resp = dispatch(&make_request("get_screen"));
        let result = resp.result.unwrap();
        assert!(result.get("lines").is_some());
        assert!(result.get("cursor").is_some());
    }

    #[test]
    fn dispatch_get_pane_info() {
        let resp = dispatch(&make_request("get_pane_info"));
        let result = resp.result.unwrap();
        assert!(result.get("rows").is_some());
        assert!(result.get("cols").is_some());
    }

    #[test]
    fn dispatch_unknown_method() {
        let resp = dispatch(&make_request("nonexistent"));
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, protocol::METHOD_NOT_FOUND);
    }

    #[test]
    fn dispatch_preserves_request_id() {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            method: "list_tabs".to_string(),
            params: serde_json::Value::Null,
            id: Some(serde_json::json!("abc-123")),
        };
        let resp = dispatch(&req);
        assert_eq!(resp.id, Some(serde_json::json!("abc-123")));
    }
}
