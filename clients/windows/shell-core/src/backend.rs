//! `SessionBackend`: where sessions live. The app shell talks only to this
//! trait. Its types mirror PROTOCOL_V2 (`docs/architecture/PROTOCOL_V2.md`
//! sections 4 and 6) and the local socket API, so a `RemoteBackend` on the
//! `forgetty-client` crate maps each method to the request of the same name
//! and each `BackendEvent` to the `ServerEvent` of the same name.
//!
//! Every method is a future. `LocalBackend` returns ready futures; a remote
//! backend resolves them when the response arrives. The shell never blocks on
//! one and learns about state changes only from `subscribe()` events, so the
//! local and remote paths are the same code.
//!
//! Method map (wire name, then Rust):
//! `get_layout`, `new_tab`, `close_tab`, `focus_tab`, `move_tab`,
//! `create_workspace`, `set_active_workspace`, `rename_workspace`,
//! `split_pane`, `close_pane`, `set_active_pane`, `resize_pane`,
//! `update_split_ratios`, `get_pane_info`, and `attach_pane` (+ input stream)
//! which yields a `PaneSource`.

use std::future::Future;
use std::pin::Pin;

use forgetty_core::PaneId;
use uuid::Uuid;

use crate::layout::{Layout, SplitDir};
use crate::source::PaneSource;

#[derive(Debug, Clone, PartialEq)]
pub enum BackendError {
    NotFound(String),
    Invalid(String),
    Spawn(String),
    Disconnected,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::NotFound(s) => write!(f, "not found: {s}"),
            BackendError::Invalid(s) => write!(f, "invalid: {s}"),
            BackendError::Spawn(s) => write!(f, "spawn failed: {s}"),
            BackendError::Disconnected => write!(f, "disconnected"),
        }
    }
}

impl std::error::Error for BackendError {}

pub type BackendResult<T> = Result<T, BackendError>;
pub type BackendFuture<T> = Pin<Box<dyn Future<Output = BackendResult<T>> + Send + 'static>>;

pub fn ready<T: Send + 'static>(r: BackendResult<T>) -> BackendFuture<T> {
    Box::pin(std::future::ready(r))
}

/// Program status shown on a tab. Wire strings of `StatusChanged.status`:
/// `idle`, `working`, `waiting`, `done`, `failed`. Anything else is `None`.
/// `Idle` (a shell at its prompt) shows no icon and never raises a toast.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PaneStatus {
    #[default]
    None,
    Idle,
    Working,
    Waiting,
    Done,
    Failed,
}

impl PaneStatus {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => PaneStatus::Idle,
            "working" | "running" | "busy" => PaneStatus::Working,
            "waiting" => PaneStatus::Waiting,
            "done" | "ok" | "success" => PaneStatus::Done,
            "failed" | "error" | "fail" => PaneStatus::Failed,
            _ => PaneStatus::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PaneStatus::None => "none",
            PaneStatus::Idle => "idle",
            PaneStatus::Working => "working",
            PaneStatus::Waiting => "waiting",
            PaneStatus::Done => "done",
            PaneStatus::Failed => "failed",
        }
    }

    /// Which of several panes' statuses a tab shows.
    pub fn rank(self) -> u8 {
        match self {
            PaneStatus::Working => 4,
            PaneStatus::Waiting => 3,
            PaneStatus::Failed => 2,
            PaneStatus::Done => 1,
            PaneStatus::Idle | PaneStatus::None => 0,
        }
    }

    /// Statuses that call for the user: a program asks for input, or ended.
    pub fn wants_attention(self) -> bool {
        matches!(self, PaneStatus::Waiting | PaneStatus::Done | PaneStatus::Failed)
    }
}

/// State of the link to a machine. A local backend is always `Connected`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    /// No connection. `since` is when it was lost (or when the client started).
    Reconnecting { since: std::time::Instant },
    /// The link stopped for good (`proto_mismatch`, `not_authorized`).
    Failed(String),
    Closed,
}

impl ConnState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected)
    }

    /// The "reconnecting" overlay shows only after this long without a
    /// connection (PROTOCOL_V2 section 9), never sooner.
    pub const OVERLAY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn show_overlay(&self, now: std::time::Instant) -> bool {
        match self {
            ConnState::Reconnecting { since } => now.saturating_duration_since(*since) >= Self::OVERLAY_DELAY,
            ConnState::Failed(_) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BackendEvent {
    LayoutChanged(Layout),
    PaneClosed { pane_id: PaneId },
    TitleChanged { pane_id: PaneId, title: String },
    CwdChanged { pane_id: PaneId, cwd: String },
    PaneResized { pane_id: PaneId, rows: u16, cols: u16 },
    StatusChanged { pane_id: PaneId, status: PaneStatus, message: String },
    /// A desktop notification from the pane (OSC 9/99/777).
    Notification { pane_id: PaneId, title: String, body: String },
    /// The link to the machine changed state.
    ConnectionChanged(ConnState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineKind {
    Local,
    Remote,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MachineInfo {
    pub id: Uuid,
    pub name: String,
    pub kind: MachineKind,
    pub connected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NewTab {
    pub workspace_idx: usize,
    pub rows: u16,
    pub cols: u16,
    pub cwd: Option<String>,
    /// Profile argv. Empty means the backend's default shell.
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewTabReply {
    pub tab_id: Uuid,
    pub pane_id: PaneId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewWorkspaceReply {
    pub workspace_id: Uuid,
    pub workspace_idx: usize,
    pub tab_id: Uuid,
    pub pane_id: PaneId,
}

#[derive(Clone, Debug)]
pub struct SplitPane {
    pub pane_id: PaneId,
    pub direction: SplitDir,
    pub rows: u16,
    pub cols: u16,
    /// Defaults to the source pane's cwd.
    pub cwd: Option<String>,
}

/// `pane_id` is the leftmost leaf of the split's first child, as in
/// `update_split_ratios` on the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitRatio {
    pub pane_id: PaneId,
    pub ratio: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneInfo {
    pub pane_id: PaneId,
    pub rows: u16,
    pub cols: u16,
    pub title: String,
    pub cwd: String,
    pub status: PaneStatus,
    /// True once the process ended (the pane is about to be removed).
    pub exited: bool,
}

pub trait SessionBackend: Send + Sync {
    /// The machine this backend serves, for the sidebar.
    fn machine(&self) -> MachineInfo;

    /// Event stream (`subscribe_events`). One consumer: the app shell.
    fn subscribe(&self) -> async_channel::Receiver<BackendEvent>;

    /// Current link state. A local backend is always connected.
    fn connection(&self) -> ConnState {
        ConnState::Connected
    }

    /// The OS reported a network change or the PC woke from sleep. A remote
    /// backend probes its connection and redials when it is dead.
    fn network_changed(&self) {}

    fn get_layout(&self) -> BackendFuture<Layout>;
    fn new_tab(&self, req: NewTab) -> BackendFuture<NewTabReply>;
    fn close_tab(&self, tab_id: Uuid) -> BackendFuture<()>;
    fn focus_tab(&self, tab_id: Uuid) -> BackendFuture<()>;
    fn move_tab(&self, tab_id: Uuid, new_index: usize) -> BackendFuture<()>;
    fn create_workspace(&self, name: Option<String>, rows: u16, cols: u16) -> BackendFuture<NewWorkspaceReply>;
    fn set_active_workspace(&self, workspace_idx: usize) -> BackendFuture<()>;
    fn rename_workspace(&self, workspace_idx: usize, name: String) -> BackendFuture<()>;
    fn split_pane(&self, req: SplitPane) -> BackendFuture<PaneId>;
    fn close_pane(&self, pane_id: PaneId) -> BackendFuture<()>;
    fn set_active_pane(&self, tab_id: Uuid, pane_id: Option<PaneId>) -> BackendFuture<()>;
    fn resize_pane(&self, pane_id: PaneId, rows: u16, cols: u16) -> BackendFuture<()>;
    fn update_split_ratios(&self, ratios: Vec<SplitRatio>) -> BackendFuture<()>;
    fn get_pane_info(&self, pane_id: PaneId) -> BackendFuture<PaneInfo>;
    /// `attach_pane` plus the pane's input and resize path.
    fn attach_pane(&self, pane_id: PaneId) -> BackendFuture<Box<dyn PaneSource>>;
}

/// Poll a future that must already be ready (every `LocalBackend` method).
/// For tests and the smoke harness only.
pub fn now<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("future was not ready"),
    }
}
