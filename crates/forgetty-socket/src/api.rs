//! The method table: one entry per socket method.
//!
//! This table is the single source of truth. [`crate::handlers::dispatch`]
//! looks the handler up here, `api.list` and `api.describe` read the
//! descriptions, schemas and examples from here, `docs/socket-api.md` is
//! generated from here ([`crate::apidoc`]), and the protocol v2 allow-list
//! uses the `remote` flag. A method cannot be dispatched without an entry.

use std::sync::Arc;

use forgetty_session::SessionManager;
use forgetty_sync::SyncEndpoint;
use schemars::JsonSchema;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::handlers as h;
use crate::protocol::{self, Request, Response};
use crate::types as t;

/// What a handler needs from the server.
pub struct Ctx {
    pub sm: Arc<SessionManager>,
    /// `None` when the iroh endpoint is not running.
    pub sync: Option<Arc<SyncEndpoint>>,
    /// Stable id of the server session (AD-017).
    pub session_id: Option<Uuid>,
}

/// A synchronous method handler.
pub type Handler = fn(&Request, &Ctx) -> Response;

/// How a method behaves on a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// One request, one reply.
    Call,
    /// The reply is followed by a stream on the same connection.
    Stream,
    /// Handled by the connection loop, only on the local socket.
    Lifecycle,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Call => "call",
            Kind::Stream => "stream",
            Kind::Lifecycle => "lifecycle",
        }
    }
}

/// One method.
pub struct MethodSpec {
    pub name: &'static str,
    pub kind: Kind,
    /// A paired device may call it over protocol v2 (`forgetty/rpc/2`).
    pub remote: bool,
    /// One line.
    pub summary: &'static str,
    /// A few sentences.
    pub description: &'static str,
    /// JSON Schema of `params`.
    pub params: fn() -> Value,
    /// JSON Schema of `result`.
    pub result: fn() -> Value,
    /// Example `params` as JSON text.
    pub request_example: &'static str,
    /// Example `result` as JSON text.
    pub response_example: &'static str,
    /// `None` for methods the connection loop handles.
    pub handler: Option<Handler>,
}

fn schema_of<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Find a method by name.
pub fn find(name: &str) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|m| m.name == name)
}

/// All methods, in documentation order.
pub fn all() -> &'static [MethodSpec] {
    METHODS
}

/// A paired device may call `name` over protocol v2.
pub fn remote_allowed(name: &str) -> bool {
    find(name).is_some_and(|m| m.remote)
}

macro_rules! pane {
    () => {
        "6f1c2a40-5b7e-4d3a-9c21-8e0d4b7a1f35"
    };
}
macro_rules! tab {
    () => {
        "b2e9d1c4-0a67-4f58-8d13-5c9e7a3b6f20"
    };
}
macro_rules! ws {
    () => {
        "0d4a8e17-92c3-4b65-a1f8-3e7c5d9b2a64"
    };
}

const OK: &str = r#"{"ok":true}"#;
const EMPTY: &str = "{}";

macro_rules! spec {
    (
        $name:expr, $kind:ident, $remote:expr, $summary:expr, $desc:expr,
        $p:ty, $r:ty, $req:expr, $resp:expr, $handler:expr
    ) => {
        MethodSpec {
            name: $name,
            kind: Kind::$kind,
            remote: $remote,
            summary: $summary,
            description: $desc,
            params: schema_of::<$p>,
            result: schema_of::<$r>,
            request_example: $req,
            response_example: $resp,
            handler: $handler,
        }
    };
}

static METHODS: &[MethodSpec] = &[
    // --- API ---------------------------------------------------------------
    spec!(
        "api.list",
        Call,
        true,
        "List every method with a one-line summary.",
        "Returns the name, summary, kind and v2 availability of each method. \
         Use `api.describe` for the schemas and examples of one method.",
        t::NoParams,
        t::ApiListResult,
        EMPTY,
        r#"{"methods":[{"name":"list_tabs","summary":"List all panes.","kind":"call","remote":true}]}"#,
        Some(api_list)
    ),
    spec!(
        "api.describe",
        Call,
        true,
        "Describe one method: schemas and examples.",
        "Returns the description, the JSON Schema of the params and of the result, and an \
         example request and response. An unknown method gives `method_not_found` \
         (-32601). A missing `method` gives `bad_request` (-32602).",
        t::ApiDescribeParams,
        t::ApiDescribeResult,
        r#"{"method":"list_tabs"}"#,
        r#"{"name":"list_tabs","summary":"List all panes.","description":"...","kind":"call","remote":true,"params_schema":{},"result_schema":{},"request_example":{},"response_example":{"tabs":[]}}"#,
        Some(api_describe)
    ),
    // --- Panes and tabs ----------------------------------------------------
    spec!(
        "list_tabs",
        Call,
        true,
        "List all panes.",
        "Returns one entry per live pane in creation order, with size, directory, title, \
         exit state, program status (OSC 7501), last notification and label. A pane that \
         exited and was kept (`remain_on_exit`) stays listed with `exited: true`.",
        t::NoParams,
        t::ListTabsResult,
        EMPTY,
        concat!(
            r#"{"tabs":[{"pane_id":""#,
            pane!(),
            r#"","pid":4242,"rows":24,"cols":80,"cwd":"/home/vick","spawn_cwd":"/home/vick","title":"bash","remain_on_exit":false,"exited":false,"exit_code":null,"status":{"status":"idle","message":null,"kind":null,"progress":null,"app":null,"record_id":null},"last_notification":null,"label":"build"}]}"#
        ),
        Some(|r, c| h::handle_list_tabs(r, &c.sm))
    ),
    spec!(
        "get_layout",
        Call,
        true,
        "Get workspaces, tabs, pane trees and labels.",
        "Returns the full layout owned by the server: workspaces in order, the tabs of each, \
         the pane tree of each tab (leaf or split with ratio), the focused pane and the \
         labels.",
        t::NoParams,
        t::Layout,
        EMPTY,
        concat!(
            r#"{"workspaces":[{"id":""#,
            ws!(),
            r#"","name":"Default","tabs":[{"id":""#,
            tab!(),
            r#"","title":"bash","pane_tree":{"Leaf":{"pane_id":""#,
            pane!(),
            r#""}},"active_pane_id":null,"label":"","pane_labels":{}}],"active_tab":0,"color":null}],"active_workspace":0}"#
        ),
        Some(|r, c| h::handle_get_layout(r, &c.sm))
    ),
    spec!(
        "new_tab",
        Call,
        true,
        "Open a tab with one new pane.",
        "Starts the shell (or `command`) in a new PTY and adds a tab to the workspace. \
         The pane runs without any client attached. A directory that does not exist is \
         ignored.",
        t::NewTabParams,
        t::NewTabResult,
        r#"{"workspace_idx":0,"cwd":"/tmp","command":["htop"],"remain_on_exit":true}"#,
        concat!(r#"{"tab_id":""#, tab!(), r#"","pane_id":""#, pane!(), r#""}"#),
        Some(|r, c| h::handle_new_tab(r, &c.sm))
    ),
    spec!(
        "close_tab",
        Call,
        true,
        "Close a tab and kill its panes.",
        "Give `tab_id` or `pane_id`. A kept pane that already exited is removed too.",
        t::CloseTabParams,
        t::OkResult,
        concat!(r#"{"tab_id":""#, tab!(), r#""}"#),
        OK,
        Some(|r, c| h::handle_close_tab(r, &c.sm))
    ),
    spec!(
        "focus_tab",
        Call,
        true,
        "Make a tab the active tab of its workspace.",
        "Moves the active tab of the owning workspace to this tab.",
        t::FocusTabParams,
        t::OkResult,
        concat!(r#"{"tab_id":""#, tab!(), r#""}"#),
        OK,
        Some(|r, c| h::handle_focus_tab(r, &c.sm))
    ),
    spec!(
        "move_tab",
        Call,
        true,
        "Move a tab to a position in its workspace.",
        "`new_index` counts from 0 and is clamped to the last position.",
        t::MoveTabParams,
        t::OkResult,
        concat!(r#"{"tab_id":""#, tab!(), r#"","new_index":0}"#),
        OK,
        Some(|r, c| h::handle_move_tab(r, &c.sm))
    ),
    spec!(
        "split_pane",
        Call,
        true,
        "Split a pane and open a new pane next to it.",
        "`direction` is `horizontal` or `vertical`. The new pane starts in the directory of \
         the source pane unless `cwd` is given. `before` (default false) puts the new pane \
         left of or above the source pane. Returns the id of the new pane.",
        t::SplitPaneParams,
        t::SplitPaneResult,
        concat!(r#"{"pane_id":""#, pane!(), r#"","direction":"vertical"}"#),
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        Some(|r, c| h::handle_split_pane(r, &c.sm))
    ),
    spec!(
        "close_pane",
        Call,
        true,
        "Close one pane.",
        "Kills the pane. In a split the sibling takes its place. The last pane of a tab \
         closes the tab.",
        t::PaneParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        OK,
        Some(|r, c| h::handle_close_pane(r, &c.sm))
    ),
    spec!(
        "get_pane_info",
        Call,
        true,
        "Get the state of one pane.",
        "Size, title, directory, process id, exit state, program status and last \
         notification.",
        t::PaneParams,
        t::PaneInfoResult,
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        concat!(
            r#"{"pane_id":""#,
            pane!(),
            r#"","rows":24,"cols":80,"title":"bash","cwd":"/home/vick","spawn_cwd":"/home/vick","pid":4242,"remain_on_exit":false,"exited":false,"exit_code":null,"status":{"status":"idle","message":null,"kind":null,"progress":null,"app":null,"record_id":null},"last_notification":null}"#
        ),
        Some(|r, c| h::handle_get_pane_info(r, &c.sm))
    ),
    spec!(
        "resize_pane",
        Call,
        false,
        "Set the PTY size of a pane.",
        "Sends the new size to the PTY. A size change is reported to subscribers as a \
         `pane_resized` event. With `client_id` (a streaming client's own id) the caller \
         takes the pane's size ownership when it has a `subscribe_output` on that pane \
         (AD-020). Without either, the size applies and ownership stays as it is.",
        t::ResizePaneParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#"","rows":40,"cols":120}"#),
        OK,
        Some(|r, c| h::handle_resize_pane(r, &c.sm))
    ),
    spec!(
        "send_input",
        Call,
        false,
        "Write bytes to a pane.",
        "`data` is base64. The bytes go to the terminal as typed input. Over protocol v2 \
         input goes on the Input stream instead.",
        t::SendInputParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#"","data":"ZWNobyBoaQo="}"#),
        OK,
        Some(|r, c| h::handle_send_input(r, &c.sm))
    ),
    spec!(
        "send_sigint",
        Call,
        true,
        "Send Ctrl+C to the foreground process of a pane.",
        "Drops input queued for the pane, then sends the interrupt key. With ISIG on the tty turns c_cc[VINTR] into one SIGINT (the group is signalled directly only when the key could not be queued). With VINTR disabled the key is 0x03, plain input, and nothing is signalled. With ISIG off the program is in raw mode: it gets 0x03 (the remapped VINTR only if the foreground process is ssh or mosh) and nothing is signalled.",
        t::PaneParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        OK,
        Some(|r, c| h::handle_send_sigint(r, &c.sm))
    ),
    spec!(
        "capture",
        Call,
        true,
        "Read the visible screen of a pane as text.",
        "The server builds the screen from the pane's byte log on request. It needs no \
         attached client. `trim` removes trailing spaces and blank lines. `last_lines` \
         keeps the last N lines (past the screen height they come from history). The work \
         has a 1 s limit. If it runs out, `incomplete` is true and `text` is the screen as far \
         as it got, which may be older than the current screen. The limit \
         covers the work only. Captures wait in line first (first come, first served, one \
         capture at a time, and never ahead of a snapshot build) and that wait is not counted.",
        t::CaptureParams,
        t::CaptureResult,
        concat!(r#"{"pane_id":""#, pane!(), r#"","trim":true,"last_lines":20}"#),
        r#"{"text":"$ echo hi\nhi\n$","incomplete":false}"#,
        Some(|r, c| h::handle_capture(r, &c.sm))
    ),
    spec!(
        "pane_exit_status",
        Call,
        true,
        "Get whether a pane's process runs and its exit code.",
        "Works for live panes and for panes that closed after an exit was recorded.",
        t::PaneParams,
        t::PaneExitStatusResult,
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        r#"{"running":false,"exit_code":0}"#,
        Some(|r, c| h::handle_pane_exit_status(r, &c.sm))
    ),
    spec!(
        "set_active_pane",
        Call,
        true,
        "Set the focused pane of a tab.",
        "The server saves the focus so a restart restores it. `pane_id: null` clears it.",
        t::SetActivePaneParams,
        t::OkResult,
        concat!(r#"{"workspace_idx":0,"tab_id":""#, tab!(), r#"","pane_id":""#, pane!(), r#""}"#),
        OK,
        Some(|r, c| h::handle_set_active_pane(r, &c.sm))
    ),
    spec!(
        "update_split_ratios",
        Call,
        true,
        "Save split ratios.",
        "Each entry names the first pane of a split and the share it takes. Malformed \
         entries are skipped.",
        t::UpdateSplitRatiosParams,
        t::OkResult,
        concat!(r#"{"ratios":[{"pane_id":""#, pane!(), r#"","ratio":0.5}]}"#),
        OK,
        Some(|r, c| h::handle_update_split_ratios(r, &c.sm))
    ),
    spec!(
        "notify",
        Call,
        true,
        "Report a notification that a client already showed.",
        "A client that detected a desktop notification (OSC 9, 99, 777) tells the server for \
         the audit log. The call changes no state.",
        t::NotifyParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#"","title":"Build done","source":"android"}"#),
        OK,
        Some(|r, c| h::handle_notify(r, &c.sm))
    ),
    // --- Labels ------------------------------------------------------------
    spec!(
        "set_label",
        Call,
        true,
        "Set the label of a workspace, tab or pane.",
        "A label names a target for people and agents. The CLI accepts a label wherever it \
         accepts an id. A workspace label is its name and cannot be empty. An empty tab or \
         pane label clears it. The server saves labels in the session file and sends a \
         layout change to subscribers.",
        t::SetLabelParams,
        t::SetLabelResult,
        concat!(r#"{"target":"pane","id":""#, pane!(), r#"","label":"build"}"#),
        r#"{"ok":true,"changed":true}"#,
        Some(|r, c| h::handle_set_label(r, &c.sm))
    ),
    // --- Workspaces --------------------------------------------------------
    spec!(
        "create_workspace",
        Call,
        true,
        "Create a workspace with one tab.",
        "Adds a workspace at the end and opens a tab in it.",
        t::CreateWorkspaceParams,
        t::CreateWorkspaceResult,
        r#"{"name":"deploy"}"#,
        concat!(
            r#"{"workspace_id":""#,
            ws!(),
            r#"","workspace_idx":1,"tab_id":""#,
            tab!(),
            r#"","pane_id":""#,
            pane!(),
            r#""}"#
        ),
        Some(|r, c| h::handle_create_workspace(r, &c.sm))
    ),
    spec!(
        "rename_workspace",
        Call,
        true,
        "Rename a workspace by index. Use set_label.",
        "Kept for older clients. `set_label` with `target: \"workspace\"` does the same by id.",
        t::RenameWorkspaceParams,
        t::OkResult,
        r#"{"workspace_idx":0,"name":"work"}"#,
        OK,
        Some(|r, c| h::handle_rename_workspace(r, &c.sm))
    ),
    spec!(
        "delete_workspace",
        Call,
        true,
        "Delete a workspace and close its panes.",
        "The last workspace cannot be deleted. Name the workspace by `workspace_id` when other clients may reorder (a position can point to another workspace by the time the call runs).",
        t::DeleteWorkspaceParams,
        t::OkResult,
        r#"{"workspace_idx":1}"#,
        OK,
        Some(|r, c| h::handle_delete_workspace(r, &c.sm))
    ),
    spec!(
        "duplicate_workspace",
        Call,
        true,
        "Copy a workspace with fresh shells.",
        "Opens one new shell per tab of the source, in the directory of the source tab. \
         Running programs are not copied. `source_workspace_id` names the source by id.",
        t::DuplicateWorkspaceParams,
        t::DuplicateWorkspaceResult,
        r#"{"source_workspace_idx":0,"new_name":"copy"}"#,
        concat!(
            r#"{"workspace_id":""#,
            ws!(),
            r#"","workspace_idx":1,"tabs":[{"tab_id":""#,
            tab!(),
            r#"","pane_id":""#,
            pane!(),
            r#""}]}"#
        ),
        Some(|r, c| h::handle_duplicate_workspace(r, &c.sm))
    ),
    spec!(
        "set_workspace_color",
        Call,
        true,
        "Set or clear the accent colour of a workspace.",
        "`color` is `#rrggbb` or null (clears it); any other value is INVALID_PARAMS. `workspace_id` names the workspace by id.",
        t::SetWorkspaceColorParams,
        t::OkResult,
        r##"{"workspace_idx":0,"color":"#3b82f6"}"##,
        OK,
        Some(|r, c| h::handle_set_workspace_color(r, &c.sm))
    ),
    spec!(
        "move_workspace",
        Call,
        true,
        "Swap two workspaces.",
        "Swaps the positions of two workspaces. The same index twice does nothing. `from_id` and `to_id` name them by id.",
        t::MoveWorkspaceParams,
        t::OkResult,
        r#"{"from_idx":0,"to_idx":1}"#,
        OK,
        Some(|r, c| h::handle_move_workspace(r, &c.sm))
    ),
    spec!(
        "set_active_workspace",
        Call,
        true,
        "Set the active workspace.",
        "The server saves the index so a restart restores it.",
        t::SetActiveWorkspaceParams,
        t::OkResult,
        r#"{"workspace_idx":0}"#,
        OK,
        Some(|r, c| h::handle_set_active_workspace(r, &c.sm))
    ),
    // --- Server ------------------------------------------------------------
    spec!(
        "server_info",
        Call,
        true,
        "Get the server version, protocol, pid, uptime and counts.",
        "Use it as a health check. `node_id` is the iroh id a paired device dials. `port` and `addresses` are what the iroh endpoint listens on.",
        t::NoParams,
        t::ServerInfoResult,
        EMPTY,
        concat!(
            r#"{"version":"0.1.0-beta","proto_major":2,"proto_minor":0,"proto_version":"2.0","server_id":""#,
            ws!(),
            r#"","pid":4100,"uptime_secs":3600,"panes":3,"clients":1,"node_id":"f3k2c7a9","port":47297,"addresses":["127.0.0.1:47297"]}"#
        ),
        Some(|r, c| h::handle_server_info(r, &c.sm, c.sync.as_deref(), c.session_id))
    ),
    spec!(
        "list_clients",
        Call,
        false,
        "List attached clients.",
        "Lists iroh connections and streaming sockets.",
        t::NoParams,
        t::ListClientsResult,
        EMPTY,
        r#"{"clients":[{"client_id":"2kq7","client_kind":"stream1","client_name":"Pixel 8","connected_at":1790000000}]}"#,
        Some(|r, c| h::handle_list_clients(r, &c.sm))
    ),
    spec!(
        "set_pinned",
        Call,
        true,
        "Compatibility no-op.",
        "Sessions are durable, so there is nothing to pin. Always returns `ok`.",
        t::NoParams,
        t::OkResult,
        EMPTY,
        OK,
        Some(|r, _| h::handle_set_pinned(r))
    ),
    spec!(
        "get_pinned",
        Call,
        true,
        "Compatibility no-op.",
        "Always returns `pinned: false`.",
        t::NoParams,
        t::GetPinnedResult,
        EMPTY,
        r#"{"pinned":false}"#,
        Some(|r, _| h::handle_get_pinned(r))
    ),
    // --- Pairing -----------------------------------------------------------
    spec!(
        "pair_open",
        Call,
        false,
        "Open a pairing window and get the QR payload.",
        "A window lasts 60 seconds by default and takes one device. The payload holds the \
         one-time secret. `relays` lists the relay URLs when the server uses its own.",
        t::PairOpenParams,
        t::PairOpenResult,
        r#"{"ttl_secs":60}"#,
        r#"{"window_id":1,"ttl_secs":60,"payload":{"v":2,"node_id":"f3k2c7a9","machine":"devbox","relay":"https://relay.example.com","relays":["https://relay.example.com"],"secret":"MFRGGZDFMZTWQ2LK"}}"#,
        Some(|r, c| h::handle_pair_open(r, c.sync.as_deref()))
    ),
    spec!(
        "pair_close",
        Call,
        false,
        "Close the open pairing window now.",
        "The QR code of the window stops working at once, and a later pair attempt with its \
         secret is refused. With `window_id` only that window is closed, so a late cancel never \
         ends a newer one. `closed` is `false` when no matching window was open (expired, \
         paired or already closed). Closing is always a success.",
        t::PairCloseParams,
        t::PairCloseResult,
        r#"{"window_id":1}"#,
        r#"{"ok":true,"closed":true}"#,
        Some(|r, c| h::handle_pair_close(r, c.sync.as_deref()))
    ),
    spec!(
        "pair_status",
        Call,
        false,
        "Poll a pairing window.",
        "`state` is `open`, `paired` or `expired`.",
        t::PairStatusParams,
        t::PairStatusResult,
        r#"{"window_id":1}"#,
        r#"{"state":"open","secs_left":42}"#,
        Some(|r, c| h::handle_pair_status(r, c.sync.as_deref()))
    ),
    spec!(
        "get_pairing_info",
        Call,
        false,
        "Get the QR code as a PNG. Opens a pairing window.",
        "Returns the node id, the host name and the QR image as base64.",
        t::NoParams,
        t::PairingInfoResult,
        EMPTY,
        r#"{"node_id":"f3k2c7a9","machine":"devbox","qr_png_base64":"iVBORw0KGgo="}"#,
        Some(|r, c| h::handle_get_pairing_info(r, c.sync.as_deref()))
    ),
    spec!(
        "list_devices",
        Call,
        true,
        "List paired devices.",
        "Each entry has the device id, name and times.",
        t::NoParams,
        t::ListDevicesResult,
        EMPTY,
        r#"{"devices":[{"device_id":"f3k2c7a9","name":"Pixel 8","paired_at":"2026-10-01T12:00:00Z","last_seen":null}]}"#,
        Some(|r, c| h::handle_list_devices(r, c.sync.as_deref()))
    ),
    spec!(
        "revoke_device",
        Call,
        true,
        "Revoke a paired device and close its connections.",
        "Removes the device from the registry and closes every live connection on every \
         protocol.",
        t::RevokeDeviceParams,
        t::RevokeDeviceResult,
        r#"{"device_id":"f3k2c7a9"}"#,
        r#"{"ok":true,"closed_connections":1}"#,
        Some(|r, c| h::handle_revoke_device(r, c.sync.as_deref()))
    ),
    // --- Connection-level methods ------------------------------------------
    spec!(
        "subscribe_output",
        Stream,
        false,
        "Stream the raw output of one pane (binary frames).",
        "After the JSON ack the connection switches to frames: a 4-byte big-endian length \
         followed by the payload. The connection ends when the pane closes. Input and \
         resize go on other connections.",
        t::SubscribeOutputParams,
        t::OkResult,
        concat!(r#"{"pane_id":""#, pane!(), r#""}"#),
        OK,
        None
    ),
    spec!(
        "subscribe_layout",
        Stream,
        false,
        "Stream layout and pane events as JSON lines.",
        "After the ack the server sends one JSON-RPC notification per line: `pane_created`, \
         `pane_closed`, `pane_exited`, `title_changed`, `cwd_changed`, `status_changed`, \
         `notification`, `label_changed`, and the layout changes (`tab_created`, \
         `tab_closed`, `pane_split`, `tab_moved`, `active_tab_changed`, \
         `active_workspace_changed`, `workspace_created`, `workspace_renamed`, \
         `workspace_deleted`, `workspace_color_changed`, `workspaces_reordered`, \
         `active_pane_changed`). `client_id` and `client_name` name the connection in \
         `list_clients` and make it a client that can own a pane's size.",
        t::SubscribeLayoutParams,
        t::OkResult,
        EMPTY,
        OK,
        None
    ),
    spec!(
        "shutdown",
        Lifecycle,
        false,
        "Stop the server.",
        "Flushes byte logs, saves the layout and exits. With `if_idle` it fails with \
         `server_busy` (-32001) while any client is attached.",
        t::ShutdownParams,
        t::OkResult,
        r#"{"if_idle":true}"#,
        OK,
        None
    ),
    spec!(
        "shutdown_save",
        Lifecycle,
        false,
        "Flush byte logs and save the layout. Never exits.",
        "Kept for older clients.",
        t::NoParams,
        t::OkResult,
        EMPTY,
        OK,
        None
    ),
    spec!(
        "shutdown_clean",
        Lifecycle,
        false,
        "Same as shutdown_save.",
        "Kept for older clients.",
        t::NoParams,
        t::OkResult,
        EMPTY,
        OK,
        None
    ),
    spec!(
        "disconnect",
        Lifecycle,
        false,
        "Same as shutdown_save.",
        "A client that leaves never ends the server. Kept for older clients.",
        t::NoParams,
        t::OkResult,
        EMPTY,
        OK,
        None
    ),
    spec!(
        "is_attached",
        Lifecycle,
        false,
        "Tell whether another local client is connected.",
        "Does not count the caller's own connection or iroh clients.",
        t::NoParams,
        t::IsAttachedResult,
        EMPTY,
        r#"{"attached":false}"#,
        None
    ),
];

// ---------------------------------------------------------------------------
// api.list and api.describe
// ---------------------------------------------------------------------------

fn api_list(request: &Request, _ctx: &Ctx) -> Response {
    let methods: Vec<Value> = METHODS
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "summary": m.summary,
                "kind": m.kind.as_str(),
                "remote": m.remote,
            })
        })
        .collect();
    Response::success(request.id.clone(), json!({ "methods": methods }))
}

fn api_describe(request: &Request, _ctx: &Ctx) -> Response {
    let Some(name) = request.params.get("method").and_then(Value::as_str) else {
        return Response::error(
            request.id.clone(),
            protocol::INVALID_PARAMS,
            "missing param: method".to_string(),
        );
    };
    let Some(m) = find(name) else {
        return Response::error(
            request.id.clone(),
            protocol::METHOD_NOT_FOUND,
            format!("Unknown method: {name}"),
        );
    };
    Response::success(request.id.clone(), describe(m))
}

/// The `api.describe` result for one method.
pub fn describe(m: &MethodSpec) -> Value {
    json!({
        "name": m.name,
        "summary": m.summary,
        "description": m.description,
        "kind": m.kind.as_str(),
        "remote": m.remote,
        "params_schema": (m.params)(),
        "result_schema": (m.result)(),
        "request_example": parse_example(m.request_example),
        "response_example": parse_example(m.response_example),
    })
}

fn parse_example(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::dispatch;

    fn validator(schema: &Value) -> jsonschema::Validator {
        jsonschema::validator_for(schema).expect("schema compiles")
    }

    fn check(what: &str, schema: &Value, instance: &Value) {
        let v = validator(schema);
        let errors: Vec<String> = v.iter_errors(instance).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{what}: {errors:?}\ninstance: {instance}");
    }

    fn req(method: &str, params: Value) -> Request {
        Request { jsonrpc: "2.0".into(), method: method.into(), params, id: Some(json!(1)) }
    }

    #[test]
    fn names_are_unique_and_documented() {
        let mut seen = std::collections::HashSet::new();
        for m in all() {
            assert!(seen.insert(m.name), "duplicate method {}", m.name);
            assert!(!m.summary.is_empty() && !m.description.is_empty(), "{} lacks text", m.name);
            assert!(m.summary.len() <= 100, "{} summary is long", m.name);
        }
    }

    #[test]
    fn examples_are_valid_json_and_match_their_schemas() {
        for m in all() {
            let params: Value = serde_json::from_str(m.request_example)
                .unwrap_or_else(|e| panic!("{}: request example: {e}", m.name));
            let result: Value = serde_json::from_str(m.response_example)
                .unwrap_or_else(|e| panic!("{}: response example: {e}", m.name));
            check(&format!("{} request", m.name), &(m.params)(), &params);
            check(&format!("{} response", m.name), &(m.result)(), &result);
        }
    }

    #[test]
    fn every_protocol_constant_has_an_entry() {
        let src = include_str!("protocol.rs");
        let body = src.split("pub mod methods {").nth(1).expect("methods module");
        let mut count = 0;
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("pub const ") else { continue };
            let Some((_, value)) = rest.split_once(": &str = \"") else { continue };
            let name = value.trim_end_matches("\";");
            // qa_* methods exist only in test builds.
            if name.starts_with("qa_") {
                continue;
            }
            assert!(find(name).is_some(), "protocol method {name} has no entry in api.rs");
            count += 1;
        }
        assert!(count > 30, "parsed too few constants: {count}");
    }

    #[tokio::test]
    async fn every_handler_entry_is_dispatched() {
        let sm = Arc::new(SessionManager::new());
        for m in all().iter().filter(|m| m.handler.is_some()) {
            let resp = dispatch(&req(m.name, Value::Null), sm.clone(), None, None);
            let unknown =
                resp.error.as_ref().is_some_and(|e| e.message.starts_with("Unknown method"));
            assert!(!unknown, "{} is in the table but dispatch does not know it", m.name);
        }
    }

    #[test]
    fn connection_level_entries_have_no_handler() {
        for m in all().iter().filter(|m| m.handler.is_none()) {
            assert!(m.kind != Kind::Call, "{} is a call without a handler", m.name);
            assert!(!m.remote, "{} cannot be remote", m.name);
        }
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let sm = Arc::new(SessionManager::new());
        let resp = dispatch(&req("nope", Value::Null), sm.clone(), None, None);
        assert_eq!(resp.error.unwrap().code, protocol::METHOD_NOT_FOUND);
        let resp =
            dispatch(&req("api.describe", json!({"method": "nope"})), sm.clone(), None, None);
        assert_eq!(resp.error.unwrap().code, protocol::METHOD_NOT_FOUND);
        let resp = dispatch(&req("api.describe", json!({})), sm, None, None);
        assert_eq!(resp.error.unwrap().code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn api_list_covers_the_table_and_describe_works_for_each() {
        let sm = Arc::new(SessionManager::new());
        let resp = dispatch(&req("api.list", Value::Null), sm.clone(), None, None);
        let list = resp.result.unwrap();
        let names: Vec<&str> =
            list["methods"].as_array().unwrap().iter().filter_map(|m| m["name"].as_str()).collect();
        assert_eq!(names.len(), all().len());
        for m in all() {
            assert!(names.contains(&m.name));
            let resp =
                dispatch(&req("api.describe", json!({"method": m.name})), sm.clone(), None, None);
            let d = resp.result.unwrap_or_else(|| panic!("describe {} failed", m.name));
            assert_eq!(d["name"], m.name);
            assert!(d["params_schema"].is_object() && d["result_schema"].is_object());
            assert!(!d["request_example"].is_null() && !d["response_example"].is_null());
        }
    }

    /// Run real calls and check every reply against the result schema.
    #[tokio::test]
    async fn live_replies_match_the_result_schemas() {
        let sm = Arc::new(SessionManager::new());
        let sid = Uuid::new_v4();
        let call = |method: &str, params: Value| -> Value {
            let resp = dispatch(&req(method, params), sm.clone(), None, Some(sid));
            if let Some(e) = &resp.error {
                panic!("{method}: {}", e.message);
            }
            let result = resp.result.unwrap_or(Value::Null);
            let spec = find(method).unwrap();
            check(&format!("{method} live result"), &(spec.result)(), &result);
            result
        };

        let t = call("new_tab", json!({"workspace_idx": 0, "remain_on_exit": true}));
        let pane = t["pane_id"].as_str().unwrap().to_string();
        let tab = t["tab_id"].as_str().unwrap().to_string();
        call("list_tabs", Value::Null);
        call("get_layout", Value::Null);
        call("get_pane_info", json!({"pane_id": pane}));
        call("pane_exit_status", json!({"pane_id": pane}));
        call("capture", json!({"pane_id": pane, "trim": true}));
        call("server_info", Value::Null);
        call("list_clients", Value::Null);
        call("get_pinned", Value::Null);
        call("set_pinned", Value::Null);
        call("set_label", json!({"target": "pane", "id": pane, "label": "build"}));
        call("set_label", json!({"target": "tab", "id": tab, "label": "main"}));
        let layout = call("get_layout", Value::Null);
        assert_eq!(layout["workspaces"][0]["tabs"][0]["label"], "main");
        assert_eq!(layout["workspaces"][0]["tabs"][0]["pane_labels"][&pane], "build");
        let listed = call("list_tabs", Value::Null);
        assert_eq!(listed["tabs"][0]["label"], "build");
        let ws = layout["workspaces"][0]["id"].as_str().unwrap().to_string();
        call("set_label", json!({"target": "workspace", "id": ws, "label": "home"}));
        let split = call("split_pane", json!({"pane_id": pane, "direction": "vertical"}));
        assert!(split["pane_id"].is_string());
        call("get_layout", Value::Null);
        call("create_workspace", json!({"name": "two"}));
        call("duplicate_workspace", json!({"source_workspace_idx": 0}));
        call("api.list", Value::Null);
        call("api.describe", json!({"method": "list_tabs"}));
    }

    #[tokio::test]
    async fn set_label_rules() {
        let sm = Arc::new(SessionManager::new());
        let call = |params: Value| dispatch(&req("set_label", params), sm.clone(), None, None);
        let t = dispatch(&req("new_tab", json!({})), sm.clone(), None, None).result.unwrap();
        let pane = t["pane_id"].as_str().unwrap().to_string();

        let r = call(json!({"target": "pane", "id": pane, "label": "  a  "})).result.unwrap();
        assert_eq!(r["changed"], true);
        let r = call(json!({"target": "pane", "id": pane, "label": "a"})).result.unwrap();
        assert_eq!(r["changed"], false);
        let r = call(json!({"target": "pane", "id": pane, "label": ""})).result.unwrap();
        assert_eq!(r["changed"], true);

        for bad in [
            json!({"target": "pane", "id": pane, "label": "a\nb"}),
            json!({"target": "pane", "id": pane, "label": "x".repeat(129)}),
            json!({"target": "cat", "id": pane, "label": "a"}),
            json!({"target": "pane", "id": "nope", "label": "a"}),
            json!({"target": "pane", "id": Uuid::new_v4().to_string(), "label": "a"}),
            json!({"target": "tab", "id": pane, "label": "a"}),
            json!({"target": "pane", "id": pane}),
        ] {
            let e = call(bad.clone()).error.unwrap_or_else(|| panic!("accepted {bad}"));
            assert_eq!(e.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let ws = sm.layout().workspaces[0].id.to_string();
        let e = call(json!({"target": "workspace", "id": ws, "label": ""})).error.unwrap();
        assert_eq!(e.code, protocol::INVALID_PARAMS);
    }

    /// The v2 allow-list in `rpc_v2.rs` must match the `remote` flags here.
    /// `capture` is allowed there by a separate clause.
    #[test]
    fn v2_allow_list_matches_the_remote_flags() {
        let consts: std::collections::HashMap<String, String> = include_str!("protocol.rs")
            .lines()
            .filter_map(|l| {
                let rest = l.trim().strip_prefix("pub const ")?;
                let (name, value) = rest.split_once(": &str = \"")?;
                Some((name.to_string(), value.trim_end_matches("\";").to_string()))
            })
            .collect();
        let rpc = include_str!("../../forgetty-server/src/rpc_v2.rs");
        let block = rpc
            .split("const DISPATCH_ALLOWED")
            .nth(1)
            .and_then(|r| r.split("];").next())
            .expect("DISPATCH_ALLOWED in rpc_v2.rs");
        let mut allowed: Vec<String> = block
            .lines()
            .filter_map(|l| l.trim().strip_prefix("methods::"))
            .map(|n| consts[n.trim_end_matches(',')].clone())
            .collect();
        allowed.sort();
        let mut want: Vec<String> = all()
            .iter()
            .filter(|m| m.remote && m.name != "capture")
            .map(|m| m.name.to_string())
            .collect();
        want.sort();
        assert_eq!(allowed, want, "rpc_v2.rs DISPATCH_ALLOWED and api.rs `remote` flags differ");
    }

    /// `docs/socket-api.md` must equal the generated text. Run
    /// `UPDATE_DOCS=1 cargo test -p forgetty-socket socket_api_doc` or
    /// `forgetty api list --markdown > docs/socket-api.md` to refresh it.
    #[test]
    fn socket_api_doc_is_not_stale() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/socket-api.md");
        let want = crate::apidoc::render();
        if std::env::var_os("UPDATE_DOCS").is_some() {
            std::fs::write(&path, &want).expect("write docs/socket-api.md");
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "docs/socket-api.md is stale. Run: forgetty api list --markdown > docs/socket-api.md \
             (or UPDATE_DOCS=1 cargo test -p forgetty-socket socket_api_doc)"
        );
    }
}
