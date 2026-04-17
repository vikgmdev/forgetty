//! JSON-RPC 2.0 protocol definitions.
//!
//! Defines the message types used for communication between external clients
//! and the Forgetty socket server, following the JSON-RPC 2.0 specification.

use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 request.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub id: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub id: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// Standard JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

impl Response {
    /// Create a successful response.
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), result: Some(result), error: None, id }
    }

    /// Create an error response.
    pub fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(RpcError { code, message, data: None }),
            id,
        }
    }
}

impl Request {
    /// Parse a JSON string into a Request.
    ///
    /// Returns an error `Response` if parsing fails or the request is invalid.
    pub fn parse(input: &str) -> Result<Self, Response> {
        let request: Self = serde_json::from_str(input)
            .map_err(|e| Response::error(None, PARSE_ERROR, format!("Parse error: {e}")))?;

        if request.jsonrpc != "2.0" {
            return Err(Response::error(
                request.id,
                INVALID_REQUEST,
                "Invalid JSON-RPC version, expected \"2.0\"".to_string(),
            ));
        }

        if request.method.is_empty() {
            return Err(Response::error(
                request.id,
                INVALID_REQUEST,
                "Method must not be empty".to_string(),
            ));
        }

        Ok(request)
    }
}

/// API method names.
pub mod methods {
    pub const LIST_TABS: &str = "list_tabs";
    pub const NEW_TAB: &str = "new_tab";
    pub const CLOSE_TAB: &str = "close_tab";
    pub const FOCUS_TAB: &str = "focus_tab";
    pub const SPLIT_PANE: &str = "split_pane";
    pub const SEND_INPUT: &str = "send_input";
    pub const GET_SCREEN: &str = "get_screen";
    pub const GET_PANE_INFO: &str = "get_pane_info";
    pub const SUBSCRIBE_OUTPUT: &str = "subscribe_output";
    pub const SUBSCRIBE_LAYOUT: &str = "subscribe_layout";
    pub const RESIZE_PANE: &str = "resize_pane";
    pub const SEND_SIGINT: &str = "send_sigint";
    // Sync / pairing methods (T-052).
    pub const LIST_DEVICES: &str = "list_devices";
    pub const REVOKE_DEVICE: &str = "revoke_device";
    pub const GET_PAIRING_INFO: &str = "get_pairing_info";
    pub const ENABLE_PAIRING: &str = "enable_pairing";
    // VT snapshot methods (T-058).
    pub const PRESEED_SNAPSHOT: &str = "preseed_snapshot";
    // Layout query + mutation methods (T-062).
    pub const GET_LAYOUT: &str = "get_layout";
    pub const MOVE_TAB: &str = "move_tab";
    // Single-pane close (T-065): closes only one pane within a split.
    pub const CLOSE_PANE: &str = "close_pane";
    // Workspace management (T-067).
    pub const CREATE_WORKSPACE: &str = "create_workspace";
    // Split ratio sync (B-002).
    pub const UPDATE_SPLIT_RATIOS: &str = "update_split_ratios";
    // Pinned sessions (B-002).
    pub const SET_PINNED: &str = "set_pinned";
    pub const GET_PINNED: &str = "get_pinned";
    // Daemon lifecycle (T-070, T-072, B-002, V2-005).
    pub const SHUTDOWN: &str = "shutdown"; // permanent close: exit immediately, no save
    pub const SHUTDOWN_SAVE: &str = "shutdown_save"; // normal close: save session then exit
    pub const SHUTDOWN_CLEAN: &str = "shutdown_clean"; // browser close: save → trash → exit
    pub const DISCONNECT: &str = "disconnect"; // V2-005 / AD-012: daemon survives window close

    // V2-006 (AD-007/AD-008): client → daemon OSC notification log.
    pub const NOTIFY: &str = "notify";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_request() {
        let input = r#"{"jsonrpc":"2.0","method":"list_tabs","params":{},"id":1}"#;
        let req = Request::parse(input).unwrap();
        assert_eq!(req.method, "list_tabs");
        assert_eq!(req.id, Some(serde_json::Value::Number(1.into())));
    }

    #[test]
    fn parse_request_without_params() {
        let input = r#"{"jsonrpc":"2.0","method":"list_tabs","id":1}"#;
        let req = Request::parse(input).unwrap();
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn parse_notification_no_id() {
        let input = r#"{"jsonrpc":"2.0","method":"new_tab"}"#;
        let req = Request::parse(input).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn parse_invalid_json() {
        let input = r#"{not valid json"#;
        let err = Request::parse(input).unwrap_err();
        assert_eq!(err.error.unwrap().code, PARSE_ERROR);
    }

    #[test]
    fn parse_wrong_version() {
        let input = r#"{"jsonrpc":"1.0","method":"list_tabs","id":1}"#;
        let err = Request::parse(input).unwrap_err();
        assert_eq!(err.error.unwrap().code, INVALID_REQUEST);
    }

    #[test]
    fn parse_empty_method() {
        let input = r#"{"jsonrpc":"2.0","method":"","id":1}"#;
        let err = Request::parse(input).unwrap_err();
        assert_eq!(err.error.unwrap().code, INVALID_REQUEST);
    }

    #[test]
    fn response_success_serialization() {
        let resp = Response::success(
            Some(serde_json::Value::Number(1.into())),
            serde_json::json!({"tabs": []}),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn response_error_serialization() {
        let resp = Response::error(
            Some(serde_json::Value::Number(1.into())),
            METHOD_NOT_FOUND,
            "not found".to_string(),
        );
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("\"result\""));
        assert!(json.contains("\"error\""));
        assert!(json.contains("-32601"));
    }
}
