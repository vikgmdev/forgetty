//! Parameter and result types of the socket methods.
//!
//! These types only produce JSON Schemas (`api.describe`). The handlers read
//! `request.params` directly. A test runs the examples of every method
//! against the live handlers and checks the replies against these schemas, so
//! a drift fails the build.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A method without parameters. `params` may be absent, `null` or `{}`.
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct NoParams {}

/// `{ "ok": true }`.
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct OkResult {
    /// Always `true`.
    pub ok: bool,
}

// ---------------------------------------------------------------------------
// Layout and tabs
// ---------------------------------------------------------------------------

/// Program status of a pane (OSC 7501).
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PaneStatus {
    /// `idle`, `working`, `waiting`, `done` or `failed`.
    pub status: String,
    /// Free text from the program.
    pub message: Option<String>,
    /// `permission`, `question` or `auth` (only while `waiting`).
    pub kind: Option<String>,
    /// Progress, 0 to 100.
    pub progress: Option<u8>,
    /// Program name.
    pub app: Option<String>,
    /// Id of the status record. `null` is the root record.
    pub record_id: Option<String>,
}

/// Last desktop notification of a pane.
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct LastNotification {
    pub title: Option<String>,
    pub body: String,
    /// Unix time in milliseconds.
    pub at_ms: u64,
}

/// One pane as `list_tabs` returns it.
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PaneEntry {
    pub pane_id: Uuid,
    /// Process id of the child, if known.
    pub pid: Option<u32>,
    pub rows: u16,
    pub cols: u16,
    /// Working directory to show (OSC 7 hint, else the real directory).
    pub cwd: String,
    /// Real working directory (used for new tabs and splits).
    pub spawn_cwd: String,
    /// Last title the program set.
    pub title: String,
    /// Pane stays listed after its process exits.
    pub remain_on_exit: bool,
    /// Process has exited and the pane was kept.
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub status: PaneStatus,
    pub last_notification: Option<LastNotification>,
    /// User label (`set_label`). Empty when none.
    pub label: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ListTabsResult {
    pub tabs: Vec<PaneEntry>,
}

/// A node of a pane tree.
#[derive(JsonSchema, Serialize, Deserialize)]
pub enum PaneTree {
    /// One pane.
    Leaf { pane_id: Uuid },
    /// Two children.
    Split {
        /// `horizontal` or `vertical`.
        direction: String,
        /// Share of the first child, 0.0 to 1.0.
        ratio: f32,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct LayoutTab {
    pub id: Uuid,
    /// Title the program set. Empty until known.
    pub title: String,
    pub pane_tree: PaneTree,
    /// Focused pane of the tab, if set.
    pub active_pane_id: Option<Uuid>,
    /// User label (`set_label`). Empty when none.
    pub label: String,
    /// Pane labels of this tab by pane id (`set_label`).
    pub pane_labels: std::collections::BTreeMap<Uuid, String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct LayoutWorkspace {
    pub id: Uuid,
    /// Name, which is also its label (`set_label`).
    pub name: String,
    pub tabs: Vec<LayoutTab>,
    pub active_tab: usize,
    /// Accent colour `#RRGGBB`.
    pub color: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct Layout {
    pub workspaces: Vec<LayoutWorkspace>,
    pub active_workspace: usize,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct NewTabParams {
    /// Workspace to add the tab to. Default 0.
    pub workspace_idx: Option<u64>,
    /// Initial rows. Default 24.
    pub rows: Option<u16>,
    /// Initial columns. Default 80.
    pub cols: Option<u16>,
    /// Starting directory. Ignored when it is not a directory.
    pub cwd: Option<String>,
    /// Program and arguments to run instead of the shell.
    pub command: Option<Vec<String>>,
    /// Keep the pane listed after its process exits. Default false.
    pub remain_on_exit: Option<bool>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct NewTabResult {
    pub tab_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct CloseTabParams {
    /// Tab to close. Preferred.
    pub tab_id: Option<Uuid>,
    /// Pane whose tab to close. Used when `tab_id` is absent.
    pub pane_id: Option<Uuid>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct FocusTabParams {
    pub tab_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct MoveTabParams {
    pub tab_id: Uuid,
    /// New position in the workspace, from 0.
    pub new_index: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SetActiveWorkspaceParams {
    pub workspace_idx: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SetActivePaneParams {
    pub workspace_idx: u64,
    pub tab_id: Uuid,
    /// Pane to focus. `null` clears the focus pointer.
    pub pane_id: Option<Uuid>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SplitRatio {
    pub pane_id: Uuid,
    /// Share of the first child, 0.0 to 1.0.
    pub ratio: f64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct UpdateSplitRatiosParams {
    /// Entries that are malformed are skipped.
    pub ratios: Vec<SplitRatio>,
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

/// What a label is attached to.
#[derive(JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelTarget {
    Workspace,
    Tab,
    Pane,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SetLabelParams {
    pub target: LabelTarget,
    /// Id of the workspace, tab or pane.
    pub id: Uuid,
    /// New label, at most 128 characters, no control characters. Empty clears
    /// a tab or pane label. A workspace label cannot be empty.
    pub label: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SetLabelResult {
    pub ok: bool,
    /// `false` when the label already had this value (no event sent).
    pub changed: bool,
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct CreateWorkspaceParams {
    /// Workspace name. Default `Workspace`.
    pub name: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct CreateWorkspaceResult {
    pub workspace_id: Uuid,
    pub workspace_idx: usize,
    /// First tab of the new workspace.
    pub tab_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct RenameWorkspaceParams {
    pub workspace_idx: u64,
    pub name: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct DeleteWorkspaceParams {
    pub workspace_idx: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct DuplicateWorkspaceParams {
    pub source_workspace_idx: u64,
    /// Name of the copy. Default derived from the source name.
    pub new_name: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct DuplicatedTab {
    pub tab_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct DuplicateWorkspaceResult {
    pub workspace_id: Uuid,
    pub workspace_idx: usize,
    /// One entry per tab of the source, in order.
    pub tabs: Vec<DuplicatedTab>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SetWorkspaceColorParams {
    pub workspace_idx: u64,
    /// `#RRGGBB`. `null` or absent clears the colour.
    pub color: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct MoveWorkspaceParams {
    pub from_idx: u64,
    pub to_idx: u64,
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PaneParams {
    pub pane_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SplitPaneParams {
    pub pane_id: Uuid,
    /// `horizontal` or `vertical`.
    pub direction: String,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    /// Starting directory. Default: the directory of `pane_id`.
    pub cwd: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SplitPaneResult {
    pub pane_id: Uuid,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SendInputParams {
    pub pane_id: Uuid,
    /// Bytes to write to the terminal, base64 (standard alphabet).
    pub data: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PaneInfoResult {
    pub pane_id: Uuid,
    pub rows: u16,
    pub cols: u16,
    pub title: String,
    pub cwd: String,
    pub spawn_cwd: String,
    pub pid: Option<u32>,
    pub remain_on_exit: bool,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub status: PaneStatus,
    pub last_notification: Option<LastNotification>,
    /// The client that owns the pane's size (AD-020), `null` when none does.
    pub size_owner: Option<SizeOwnerInfo>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SizeOwnerInfo {
    pub client_id: Uuid,
    /// `android`, `windows`, `cli`, `gtk` for remote clients, `local` for a
    /// socket client of the server.
    pub client_kind: String,
    pub client_name: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ResizePaneParams {
    pub pane_id: Uuid,
    pub rows: u16,
    pub cols: u16,
    /// A streaming client's own id (the `client_id` it gave to
    /// `subscribe_layout` or `subscribe_output`). Such a client can become the
    /// size owner. Without it the size applies and ownership does not change.
    pub client_id: Option<Uuid>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SubscribeOutputParams {
    pub pane_id: Uuid,
    /// The client's stable id. A fresh one is made when absent.
    pub client_id: Option<Uuid>,
    /// Name shown by `list_clients` and `size_owner`.
    pub client_name: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct SubscribeLayoutParams {
    /// The client's stable id. A fresh one is made when absent.
    pub client_id: Option<Uuid>,
    /// Name shown by `list_clients` and `size_owner`.
    pub client_name: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct NotifyParams {
    pub pane_id: Uuid,
    pub title: Option<String>,
    /// Not logged.
    pub body: Option<String>,
    /// Name of the reporting client.
    pub source: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PaneExitStatusResult {
    pub running: bool,
    /// Exit code once the process has ended. `null` while running or unknown.
    pub exit_code: Option<i32>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct CaptureParams {
    pub pane_id: Uuid,
    /// Remove trailing spaces and trailing blank lines. Default false.
    pub trim: Option<bool>,
    /// Keep only the last N lines (at most 100000).
    pub last_lines: Option<u32>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct CaptureResult {
    /// Visible screen as text, one line per row.
    pub text: String,
    /// `true` when the 1 s work limit ran out before the whole byte log was
    /// read: `text` is the screen as far as it got. Always present.
    pub incomplete: bool,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ServerInfoResult {
    /// Server version.
    pub version: String,
    pub proto_major: u32,
    pub proto_minor: u32,
    /// `major.minor`.
    pub proto_version: String,
    /// Stable id of the server session.
    pub server_id: Option<Uuid>,
    pub pid: u32,
    pub uptime_secs: u64,
    /// Live panes.
    pub panes: u64,
    /// Attached clients.
    pub clients: u64,
    /// iroh node id. `null` when the iroh endpoint is not running.
    pub node_id: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ClientEntry {
    pub client_id: String,
    /// `stream1` or `socket`.
    pub client_kind: String,
    pub client_name: String,
    /// Unix time in seconds.
    pub connected_at: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ListClientsResult {
    pub clients: Vec<ClientEntry>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct GetPinnedResult {
    /// Always `false`.
    pub pinned: bool,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ShutdownParams {
    /// Refuse while any client is attached. Default false.
    pub if_idle: Option<bool>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct IsAttachedResult {
    /// Another connection is open.
    pub attached: bool,
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub device_id: String,
    pub name: String,
    /// ISO 8601.
    pub paired_at: String,
    /// ISO 8601. `null` if never seen after pairing.
    pub last_seen: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ListDevicesResult {
    pub devices: Vec<DeviceEntry>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct RevokeDeviceParams {
    pub device_id: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct RevokeDeviceResult {
    /// The device was in the registry.
    pub ok: bool,
    /// Live connections that were closed.
    pub closed_connections: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairOpenParams {
    /// Window length in seconds, 1 to 600. Default 60.
    pub ttl_secs: Option<u64>,
}

/// The pairing QR payload (PROTOCOL_V2 section 7).
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairPayload {
    /// Always 2.
    pub v: u32,
    pub node_id: String,
    pub machine: String,
    /// First relay URL.
    pub relay: String,
    /// All relay URLs. Left out when the server uses no own relays.
    pub relays: Option<Vec<String>>,
    /// One-time secret, base32.
    pub secret: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairOpenResult {
    pub window_id: u64,
    pub ttl_secs: u64,
    pub payload: PairPayload,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairStatusParams {
    pub window_id: u64,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairStatusResult {
    /// `open`, `paired` or `expired`.
    pub state: String,
    /// Seconds left (`open`).
    pub secs_left: Option<u64>,
    /// Device that paired (`paired`).
    pub device_id: Option<String>,
    /// Device name (`paired`).
    pub name: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct PairingInfoResult {
    pub node_id: String,
    pub machine: String,
    /// QR code as PNG, base64.
    pub qr_png_base64: String,
}

// ---------------------------------------------------------------------------
// The API itself
// ---------------------------------------------------------------------------

/// One entry of `api.list`.
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ApiListEntry {
    pub name: String,
    pub summary: String,
    /// `call`, `stream` or `lifecycle`.
    pub kind: String,
    /// A paired device may call it over protocol v2.
    pub remote: bool,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ApiListResult {
    pub methods: Vec<ApiListEntry>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ApiDescribeParams {
    /// Method name, for example `list_tabs`.
    pub method: String,
}

#[derive(JsonSchema, Serialize, Deserialize)]
pub struct ApiDescribeResult {
    pub name: String,
    pub summary: String,
    pub description: String,
    /// `call`, `stream` or `lifecycle`.
    pub kind: String,
    /// A paired device may call it over protocol v2.
    pub remote: bool,
    /// JSON Schema of `params`.
    pub params_schema: serde_json::Value,
    /// JSON Schema of `result`.
    pub result_schema: serde_json::Value,
    /// Example `params`.
    pub request_example: serde_json::Value,
    /// Example `result`.
    pub response_example: serde_json::Value,
}
