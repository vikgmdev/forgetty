//! IPC protocol definitions.
//!
//! Defines the JSON-based message format used for communication between
//! external clients and the Forgetty socket server.

// TODO: Phase 8 — implement protocol types
//
// use serde::{Deserialize, Serialize};
//
// #[derive(Debug, Serialize, Deserialize)]
// pub struct Request {
//     pub id: u64,
//     pub method: String,
//     pub params: serde_json::Value,
// }
//
// #[derive(Debug, Serialize, Deserialize)]
// pub struct Response {
//     pub id: u64,
//     pub result: Option<serde_json::Value>,
//     pub error: Option<String>,
// }
