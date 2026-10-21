//! `RemoteBackend`: a `SessionBackend` on `forgetty-client` (protocol v2).
//!
//! Each trait method is the request of the same name (`docs/socket-api.md`
//! with the PROTOCOL_V2 amendments). Futures run on the tokio runtime that
//! started the backend, so the UI executor can await them without a reactor.

use std::future::Future;
use std::sync::{Arc, Mutex};

use forgetty_client::proto::{ServerEvent, Value};
use forgetty_client::{
    Client, ClientError, ClientHandle, ClientInfo, ClientOptions, ConnectionState, EndpointAddr, EventItem,
    SecretKey,
};
use forgetty_shell_core::{
    valid_label, BackendError, BackendEvent, BackendFuture, BackendResult, ConnState, LabelTarget, Layout, MachineInfo, MachineKind,
    NewTab, NewTabReply, NewWorkspaceReply, PaneId, PaneInfo, PaneSource, PaneStatus, SessionBackend, SizeOwnerInfo,
    SplitDir, SplitPane, SplitRatio,
};
use serde_json::{json, Value as Json};
use tokio::runtime::Handle;
use uuid::Uuid;

use crate::convert::{from_json, to_json};
use crate::source::RemotePaneSource;

pub struct RemoteParams {
    /// Local id of this machine in the sidebar.
    pub machine_id: Uuid,
    pub name: String,
    pub server: EndpointAddr,
    pub identity: SecretKey,
    pub info: ClientInfo,
    /// Carries the `AddrCache` the app persists.
    pub opts: ClientOptions,
}

struct Inner {
    rt: Handle,
    client: ClientHandle,
    machine_id: Uuid,
    name: String,
    tx: async_channel::Sender<BackendEvent>,
    rx: async_channel::Receiver<BackendEvent>,
    /// Last layout seen. Resolves `workspace_idx` for requests that need it.
    layout: Mutex<Layout>,
}

#[derive(Clone)]
pub struct RemoteBackend {
    inner: Arc<Inner>,
}

pub fn map_err(e: ClientError) -> BackendError {
    match e {
        ClientError::Server { code, message } => match code.as_str() {
            "not_found" => BackendError::NotFound(message),
            "bad_request" | "unknown_method" | "capability_missing" => BackendError::Invalid(format!("{code}: {message}")),
            _ => BackendError::Invalid(format!("{code}: {message}")),
        },
        ClientError::Invalid(m) | ClientError::Proto(m) => BackendError::Invalid(m),
        _ => BackendError::Disconnected,
    }
}

pub fn map_conn(s: &ConnectionState) -> ConnState {
    match s {
        ConnectionState::Connected => ConnState::Connected,
        ConnectionState::Reconnecting { since } => ConnState::Reconnecting { since: *since },
        ConnectionState::Failed(r) => ConnState::Failed(r.clone()),
        ConnectionState::Closed => ConnState::Closed,
    }
}

fn parse_layout(v: &Value) -> BackendResult<Layout> {
    serde_json::from_value::<Layout>(to_json(v)).map_err(|e| BackendError::Invalid(format!("layout: {e}")))
}

fn uuid_at(v: &Json, key: &str) -> BackendResult<Uuid> {
    v.get(key)
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| BackendError::Invalid(format!("reply has no {key}")))
}

impl RemoteBackend {
    /// Bind the endpoint and start the reconnect supervisor. Returns at once:
    /// the link comes up in the background (`connection()` shows it). Call
    /// inside a tokio runtime.
    pub async fn start(p: RemoteParams) -> Result<Self, ClientError> {
        let client = Client::spawn(p.server, p.identity, p.info, p.opts).await?;
        let (tx, rx) = async_channel::unbounded();
        let inner = Arc::new(Inner {
            rt: Handle::current(),
            client,
            machine_id: p.machine_id,
            name: p.name,
            tx,
            rx,
            layout: Mutex::new(Layout::default()),
        });
        let this = RemoteBackend { inner };
        this.spawn_pumps();
        Ok(this)
    }

    pub fn client(&self) -> &ClientHandle {
        &self.inner.client
    }

    /// Stop the client and close its endpoint.
    pub async fn close(&self) {
        self.inner.client.close().await;
    }

    fn spawn_pumps(&self) {
        // Connection state -> `ConnectionChanged`.
        let inner = self.inner.clone();
        self.inner.rt.spawn(async move {
            let mut rx = inner.client.watch_state();
            let mut nudger: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                let st = rx.borrow_and_update().clone();
                let _ = inner.tx.try_send(BackendEvent::ConnectionChanged(map_conn(&st)));
                if let Some(n) = nudger.take() {
                    n.abort();
                }
                if let ConnectionState::Reconnecting { since } = st {
                    // The contract backoff grows to 2 s, so a server that comes back
                    // can wait that long for the next dial. For the first minute of an
                    // outage, cut each wait short; after that the plain backoff applies.
                    let client = inner.client.clone();
                    nudger = Some(tokio::spawn(async move {
                        while since.elapsed() < NUDGE_FOR {
                            tokio::time::sleep(NUDGE_EVERY).await;
                            client.reconnect_now();
                        }
                    }));
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
            if let Some(n) = nudger.take() {
                n.abort();
            }
        });

        // Server events -> `BackendEvent`.
        let inner = self.inner.clone();
        self.inner.rt.spawn(async move {
            let mut events = match inner.client.events().await {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("forgetty-win: events for {}: {e}", inner.name);
                    return;
                }
            };
            while let Some(item) = events.next().await {
                match item {
                    EventItem::Resynced => {
                        // Events may have been missed: take the layout again.
                        if let Ok(v) = inner.client.request("get_layout", Value::Map(Vec::new())).await {
                            if let Ok(l) = parse_layout(&v) {
                                *inner.layout.lock().unwrap_or_else(|e| e.into_inner()) = l.clone();
                                let _ = inner.tx.try_send(BackendEvent::LayoutChanged(l));
                            }
                        }
                    }
                    EventItem::Server { event, .. } => {
                        for ev in map_event(&inner, event) {
                            let _ = inner.tx.try_send(ev);
                        }
                    }
                    // The server restarted and the input still queued was dropped.
                    EventItem::InputLost { bytes } => {
                        let _ = inner.tx.try_send(BackendEvent::InputLost { bytes });
                    }
                }
            }
        });
    }

    /// Run `fut` on the backend's runtime; the returned future is safe to
    /// await from any executor.
    fn run<T: Send + 'static>(&self, fut: impl Future<Output = BackendResult<T>> + Send + 'static) -> BackendFuture<T> {
        let jh = self.inner.rt.spawn(fut);
        Box::pin(async move { jh.await.unwrap_or(Err(BackendError::Disconnected)) })
    }

    fn call(&self, method: &'static str, params: Json) -> BackendFuture<Json> {
        let client = self.inner.client.clone();
        self.run(async move {
            client.request(method, from_json(&params)).await.map(|v| to_json(&v)).map_err(map_err)
        })
    }

    /// `call` for methods whose reply carries nothing the shell needs.
    fn call_unit(&self, method: &'static str, params: Json) -> BackendFuture<()> {
        let f = self.call(method, params);
        Box::pin(async move { f.await.map(|_| ()) })
    }
}

/// While reconnecting, end each backoff wait after this long ...
const NUDGE_EVERY: std::time::Duration = std::time::Duration::from_millis(400);
/// ... for this long after the link broke.
const NUDGE_FOR: std::time::Duration = std::time::Duration::from_secs(60);

fn owner_info(o: forgetty_client::proto::SizeOwner) -> SizeOwnerInfo {
    SizeOwnerInfo { client_id: o.client_id, kind: o.client_kind.as_str().to_string(), name: o.client_name }
}

/// `size_owner` of a `get_pane_info` reply: an object, or null/absent.
fn owner_from_json(v: &Json) -> Option<SizeOwnerInfo> {
    let o = v.get("size_owner")?;
    let text = |k: &str| o.get(k).and_then(Json::as_str).map(str::to_string);
    Some(SizeOwnerInfo {
        client_id: text("client_id").and_then(|s| s.parse().ok())?,
        kind: text("client_kind").unwrap_or_else(|| "unknown".into()),
        name: text("client_name").unwrap_or_default(),
    })
}

fn map_event(inner: &Arc<Inner>, ev: ServerEvent) -> Vec<BackendEvent> {
    vec![match ev {
        ServerEvent::LayoutChanged { layout } => {
            let Ok(l) = parse_layout(&layout) else { return Vec::new() };
            *inner.layout.lock().unwrap_or_else(|e| e.into_inner()) = l.clone();
            BackendEvent::LayoutChanged(l)
        }
        ServerEvent::PaneClosed { pane_id } => BackendEvent::PaneClosed { pane_id: PaneId(pane_id) },
        ServerEvent::TitleChanged { pane_id, title } => BackendEvent::TitleChanged { pane_id: PaneId(pane_id), title },
        ServerEvent::CwdChanged { pane_id, cwd } => BackendEvent::CwdChanged { pane_id: PaneId(pane_id), cwd },
        ServerEvent::PaneResized { pane_id, rows, cols, size_owner } => {
            BackendEvent::PaneResized { pane_id: PaneId(pane_id), rows, cols, owner: size_owner.map(owner_info) }
        }
        ServerEvent::SizeOwnerChanged { pane_id, owner } => {
            BackendEvent::SizeOwnerChanged { pane_id: PaneId(pane_id), owner: owner.map(owner_info) }
        }
        ServerEvent::StatusChanged { pane_id, status, message } => BackendEvent::StatusChanged {
            pane_id: PaneId(pane_id),
            status: PaneStatus::parse(&status),
            message,
        },
        ServerEvent::Notification { pane_id, title, body } => {
            BackendEvent::Notification { pane_id: PaneId(pane_id), title, body }
        }
        // The pane stays listed only with `remain_on_exit`; `PaneClosed` removes it.
        ServerEvent::InputDropped { pane_id, bytes } => BackendEvent::InputDropped { pane_id: PaneId(pane_id), bytes },
        ServerEvent::PaneExited { .. }
        | ServerEvent::ClientConnected { .. }
        | ServerEvent::ClientDisconnected { .. }
        | ServerEvent::Unknown => return Vec::new(),
    }]
}

fn dir_str(d: SplitDir) -> &'static str {
    match d {
        SplitDir::Horizontal => "horizontal",
        SplitDir::Vertical => "vertical",
    }
}

impl SessionBackend for RemoteBackend {
    fn machine(&self) -> MachineInfo {
        MachineInfo {
            id: self.inner.machine_id,
            name: self.inner.name.clone(),
            kind: MachineKind::Remote,
            connected: self.inner.client.state().is_connected(),
        }
    }

    fn subscribe(&self) -> async_channel::Receiver<BackendEvent> {
        self.inner.rx.clone()
    }

    fn connection(&self) -> ConnState {
        map_conn(&self.inner.client.state())
    }

    fn network_changed(&self) {
        let client = self.inner.client.clone();
        self.inner.rt.spawn(async move { client.network_changed().await });
    }

    fn get_layout(&self) -> BackendFuture<Layout> {
        let inner = self.inner.clone();
        self.run(async move {
            let v = inner.client.request("get_layout", Value::Map(Vec::new())).await.map_err(map_err)?;
            let l = parse_layout(&v)?;
            *inner.layout.lock().unwrap_or_else(|e| e.into_inner()) = l.clone();
            Ok(l)
        })
    }

    fn new_tab(&self, req: NewTab) -> BackendFuture<NewTabReply> {
        let mut p = json!({"workspace_idx": req.workspace_idx, "rows": req.rows, "cols": req.cols});
        if let Some(cwd) = req.cwd {
            p["cwd"] = json!(cwd);
        }
        if !req.command.is_empty() {
            p["command"] = json!(req.command);
        }
        let f = self.call("new_tab", p);
        Box::pin(async move {
            let v = f.await?;
            Ok(NewTabReply { tab_id: uuid_at(&v, "tab_id")?, pane_id: PaneId(uuid_at(&v, "pane_id")?) })
        })
    }

    fn close_tab(&self, tab_id: Uuid) -> BackendFuture<()> {
        self.call_unit("close_tab", json!({"tab_id": tab_id.to_string()}))
    }

    fn focus_tab(&self, tab_id: Uuid) -> BackendFuture<()> {
        self.call_unit("focus_tab", json!({"tab_id": tab_id.to_string()}))
    }

    fn move_tab(&self, tab_id: Uuid, new_index: usize) -> BackendFuture<()> {
        self.call_unit("move_tab", json!({"tab_id": tab_id.to_string(), "new_index": new_index}))
    }

    fn create_workspace(&self, name: Option<String>, _rows: u16, _cols: u16) -> BackendFuture<NewWorkspaceReply> {
        let p = match name {
            Some(n) => json!({"name": n}),
            None => json!({}),
        };
        let f = self.call("create_workspace", p);
        Box::pin(async move {
            let v = f.await?;
            Ok(NewWorkspaceReply {
                workspace_id: uuid_at(&v, "workspace_id")?,
                workspace_idx: v.get("workspace_idx").and_then(Json::as_u64).unwrap_or(0) as usize,
                tab_id: uuid_at(&v, "tab_id")?,
                pane_id: PaneId(uuid_at(&v, "pane_id")?),
            })
        })
    }

    fn set_active_workspace(&self, workspace_idx: usize) -> BackendFuture<()> {
        self.call_unit("set_active_workspace", json!({"workspace_idx": workspace_idx}))
    }

    fn set_label(&self, target: LabelTarget, id: Uuid, label: String) -> BackendFuture<()> {
        let label = match valid_label(target, &label) {
            Ok(l) => l,
            Err(e) => return forgetty_shell_core::ready(Err(e)),
        };
        self.call_unit("set_label", json!({"target": target.wire(), "id": id.to_string(), "label": label}))
    }

    fn rename_workspace(&self, workspace_idx: usize, name: String) -> BackendFuture<()> {
        self.call_unit("rename_workspace", json!({"workspace_idx": workspace_idx, "name": name}))
    }

    fn split_pane(&self, req: SplitPane) -> BackendFuture<PaneId> {
        let mut p = json!({
            "pane_id": req.pane_id.0.to_string(),
            "direction": dir_str(req.direction),
            "rows": req.rows,
            "cols": req.cols,
        });
        if let Some(cwd) = req.cwd {
            p["cwd"] = json!(cwd);
        }
        let f = self.call("split_pane", p);
        Box::pin(async move { Ok(PaneId(uuid_at(&f.await?, "pane_id")?)) })
    }

    fn close_pane(&self, pane_id: PaneId) -> BackendFuture<()> {
        self.call_unit("close_pane", json!({"pane_id": pane_id.0.to_string()}))
    }

    fn set_active_pane(&self, tab_id: Uuid, pane_id: Option<PaneId>) -> BackendFuture<()> {
        // The request names the workspace; take it from the last layout.
        let ws = self
            .inner
            .layout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .find_tab(tab_id)
            .map(|(w, _)| w);
        match ws {
            None => forgetty_shell_core::ready(Err(BackendError::NotFound(format!("tab {tab_id}")))),
            Some(w) => self.call_unit(
                "set_active_pane",
                json!({
                    "workspace_idx": w,
                    "tab_id": tab_id.to_string(),
                    "pane_id": pane_id.map(|p| p.0.to_string()),
                }),
            ),
        }
    }

    fn resize_pane(&self, pane_id: PaneId, rows: u16, cols: u16) -> BackendFuture<()> {
        let client = self.inner.client.clone();
        self.run(async move { client.resize(pane_id.0, rows, cols).await.map_err(map_err) })
    }

    fn update_split_ratios(&self, ratios: Vec<SplitRatio>) -> BackendFuture<()> {
        let list: Vec<Json> = ratios
            .into_iter()
            .filter(|r| r.ratio.is_finite())
            .map(|r| json!({"pane_id": r.pane_id.0.to_string(), "ratio": f64::from(r.ratio.clamp(0.05, 0.95))}))
            .collect();
        self.call_unit("update_split_ratios", json!({ "ratios": list }))
    }

    fn get_pane_info(&self, pane_id: PaneId) -> BackendFuture<PaneInfo> {
        let f = self.call("get_pane_info", json!({"pane_id": pane_id.0.to_string()}));
        Box::pin(async move {
            let v = f.await?;
            let num = |k: &str, d: u64| v.get(k).and_then(Json::as_u64).unwrap_or(d) as u16;
            let text = |k: &str| v.get(k).and_then(Json::as_str).unwrap_or("").to_string();
            Ok(PaneInfo {
                pane_id,
                rows: num("rows", 24),
                cols: num("cols", 80),
                title: text("title"),
                cwd: text("cwd"),
                status: v.get("status").and_then(Json::as_str).map(PaneStatus::parse).unwrap_or_default(),
                exited: v.get("exited").and_then(Json::as_bool).unwrap_or(false),
                size_owner: owner_from_json(&v),
            })
        })
    }

    fn attach_pane(&self, pane_id: PaneId) -> BackendFuture<Box<dyn PaneSource>> {
        let inner = self.inner.clone();
        self.run(async move {
            let stream = inner.client.attach_pane(pane_id.0).await.map_err(map_err)?;
            let src = RemotePaneSource::spawn(&inner.rt, inner.client.clone(), pane_id.0, stream);
            Ok(Box::new(src) as Box<dyn PaneSource>)
        })
    }
}
