//! Request handlers for the IPC socket server.
//!
//! Each handler processes a specific type of IPC request, such as
//! creating a new pane, sending input to a pane, or querying state.

// TODO: Phase 8 — implement request handlers
//
// pub async fn handle_request(request: &Request) -> Response { ... }
//
// Planned handlers:
//   - "pane.create" — create a new terminal pane
//   - "pane.send" — send input to a pane
//   - "pane.read" — read output from a pane
//   - "pane.resize" — resize a pane
//   - "pane.close" — close a pane
//   - "workspace.save" — save current workspace state
//   - "workspace.restore" — restore a saved workspace
