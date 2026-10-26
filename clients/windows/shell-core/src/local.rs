//! `LocalBackend`: sessions that live inside this process, one ConPTY per
//! pane. Owns the layout and emits the same events a server would, so the
//! shell cannot tell it from a remote backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use forgetty_core::PaneId;
use uuid::Uuid;

use crate::backend::*;
use crate::conpty::{Hooks, LocalConPty};
use crate::layout::{Layout, PaneTree, Tab, Workspace};
use crate::osc::{OscEvent, OscScanner};
use crate::source::{PaneEvent, PaneSource};

#[derive(Clone, Debug)]
pub struct LocalConfig {
    /// Default shell argv.
    pub shell: Vec<String>,
    /// Directory for the first tab and for tabs with no source pane.
    pub default_cwd: Option<String>,
    pub machine_name: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self { shell: vec!["cmd.exe".into()], default_cwd: None, machine_name: "This PC".into() }
    }
}

/// Notifications from one pane closer together than this are dropped.
const NOTIFY_GAP: std::time::Duration = std::time::Duration::from_millis(500);

struct Pane {
    pty: Arc<LocalConPty>,
    rows: u16,
    cols: u16,
    title: String,
    /// Display hint reported by the program (OSC 7 / 9;9). Never used to spawn.
    cwd: String,
    /// Where this pane's process was started. New tabs and splits inherit this.
    spawn_cwd: Option<String>,
    status: PaneStatus,
    /// When this pane last raised a notification event (a program in a loop must not flood
    /// the channel; the shell's toast limiter is stricter still).
    last_notify: Option<std::time::Instant>,
}

struct State {
    layout: Layout,
    panes: HashMap<PaneId, Pane>,
    /// Ptys of closed panes, kept so tests can confirm the process exited.
    closed: Vec<(PaneId, Arc<LocalConPty>)>,
}

struct Inner {
    cfg: LocalConfig,
    machine_id: Uuid,
    state: Mutex<State>,
    tx: async_channel::Sender<BackendEvent>,
    rx: async_channel::Receiver<BackendEvent>,
}

#[derive(Clone)]
pub struct LocalBackend {
    inner: Arc<Inner>,
}

impl LocalBackend {
    /// One empty workspace named "Default".
    pub fn new(cfg: LocalConfig) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let layout = Layout {
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                name: "Default".into(),
                tabs: Vec::new(),
                active_tab: 0,
                color: None,
            }],
            active_workspace: 0,
        };
        LocalBackend {
            inner: Arc::new(Inner {
                cfg,
                machine_id: Uuid::new_v4(),
                state: Mutex::new(State { layout, panes: HashMap::new(), closed: Vec::new() }),
                tx,
                rx,
            }),
        }
    }

    // ---- test and smoke hooks (not part of SessionBackend) --------------------

    /// The pty behind a live pane, e.g. for `injector()` in the smoke test.
    pub fn pty(&self, pane: PaneId) -> Option<Arc<LocalConPty>> {
        self.inner.lock().panes.get(&pane).map(|p| p.pty.clone())
    }

    /// Ptys of panes that were closed.
    pub fn closed_ptys(&self) -> Vec<(PaneId, Arc<LocalConPty>)> {
        self.inner.lock().closed.clone()
    }

    pub fn live_panes(&self) -> usize {
        self.inner.lock().panes.len()
    }

    /// Run bytes through the OSC scanner as if the pane had printed them, so a
    /// test can drive title, cwd and status events without a cooperating shell.
    /// The bytes do not reach the pane's output stream.
    pub fn inject_osc(&self, pane: PaneId, bytes: &[u8]) {
        let events = OscScanner::new().feed(bytes);
        self.inner.apply_osc(pane, events);
    }

    /// Kill every shell.
    pub fn shutdown_all(&self) {
        for p in self.inner.lock().panes.values() {
            p.pty.kill();
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, ev: BackendEvent) {
        let _ = self.tx.try_send(ev);
    }

    fn emit_layout(&self, st: &State) {
        self.emit(BackendEvent::LayoutChanged(st.layout.clone()));
    }

    fn spawn_pane(
        self: &Arc<Self>,
        st: &mut State,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) -> BackendResult<PaneId> {
        let id = PaneId::new();
        let argv = if argv.is_empty() { self.cfg.shell.clone() } else { argv };
        let weak: Weak<Inner> = Arc::downgrade(self);
        let mut scanner = OscScanner::new();
        let w_out = weak.clone();
        let w_exit = weak;
        let hooks = Hooks {
            on_output: Box::new(move |bytes| {
                // A panic inside the scanner may leave it half-updated: log,
                // drop this chunk, and start from a fresh scanner.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scanner.feed(bytes)));
                let events = match r {
                    Ok(e) => e,
                    Err(_) => {
                        eprintln!("forgetty: OSC scanner panicked, sequence dropped");
                        scanner = OscScanner::new();
                        return;
                    }
                };
                if events.is_empty() {
                    return;
                }
                if let Some(inner) = w_out.upgrade() {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.apply_osc(id, events))).is_err() {
                        eprintln!("forgetty: OSC handler panicked, events dropped");
                    }
                }
            }),
            on_exit: Box::new(move |_why| {
                if let Some(inner) = w_exit.upgrade() {
                    inner.remove_pane(id, false);
                }
            }),
        };
        let rows = rows.max(1);
        let cols = cols.max(2);
        // Spawn directory: the caller's (the parent pane's spawn cwd), else the
        // configured default, else the user profile. Never taken from escape
        // codes, and never a UNC/device path.
        let spawn_cwd = [cwd, self.cfg.default_cwd.clone(), user_profile_dir()]
            .into_iter()
            .flatten()
            .find(|c| crate::osc::is_safe_local_path(c));
        let pty = LocalConPty::spawn(&argv, spawn_cwd.as_deref(), rows, cols, hooks)
            .map_err(|e| BackendError::Spawn(format!("{e:#}")))?;
        let cwd_s = spawn_cwd.clone().unwrap_or_default();
        st.panes.insert(
            id,
            Pane {
                pty: Arc::new(pty),
                rows,
                cols,
                title: String::new(),
                cwd: cwd_s,
                spawn_cwd,
                status: PaneStatus::None,
                last_notify: None,
            },
        );
        Ok(id)
    }

    fn apply_osc(&self, id: PaneId, events: Vec<OscEvent>) {
        let mut out = Vec::new();
        {
            let mut st = self.lock();
            let Some(p) = st.panes.get_mut(&id) else { return };
            for e in events {
                match e {
                    OscEvent::Title(t) => {
                        if p.title != t {
                            p.title = t.clone();
                            out.push(BackendEvent::TitleChanged { pane_id: id, title: t });
                        }
                    }
                    OscEvent::Cwd(c) => {
                        if p.cwd != c {
                            p.cwd = c.clone();
                            out.push(BackendEvent::CwdChanged { pane_id: id, cwd: c });
                        }
                    }
                    OscEvent::Status(s, message) => {
                        p.status = s;
                        out.push(BackendEvent::StatusChanged { pane_id: id, status: s, message });
                    }
                    OscEvent::Notify { title, body } => {
                        let now = std::time::Instant::now();
                        if p.last_notify.map_or(true, |t| now.duration_since(t) >= NOTIFY_GAP) {
                            p.last_notify = Some(now);
                            out.push(BackendEvent::Notification { pane_id: id, title, body });
                        }
                    }
                }
            }
        }
        for e in out {
            self.emit(e);
        }
    }

    /// Remove a pane from its tab. Closes the tab when it was the last pane.
    fn remove_pane(&self, id: PaneId, kill: bool) {
        let mut st = self.lock();
        let Some(pane) = st.panes.remove(&id) else { return };
        if kill {
            pane.pty.kill();
        }
        st.closed.push((id, pane.pty.clone()));
        if st.closed.len() > 64 {
            st.closed.remove(0);
        }
        let mut closed_tab = None;
        if let Some((w, t)) = st.layout.find_pane(id) {
            let tab = &mut st.layout.workspaces[w].tabs[t];
            let tree = std::mem::replace(&mut tab.pane_tree, PaneTree::leaf(id));
            match tree.remove_leaf(id) {
                Some((rest, focus)) => {
                    tab.pane_tree = rest;
                    if tab.active_pane_id == Some(id) || tab.active_pane_id.is_none() {
                        tab.active_pane_id = Some(focus);
                    }
                }
                None => closed_tab = Some((w, t)),
            }
        }
        if let Some((w, t)) = closed_tab {
            remove_tab_at(&mut st.layout, w, t);
        }
        self.emit(BackendEvent::PaneClosed { pane_id: id });
        self.emit_layout(&st);
    }
}

fn user_profile_dir() -> Option<String> {
    ["USERPROFILE", "HOME"].iter().find_map(|k| std::env::var(k).ok()).filter(|s| !s.is_empty())
}

fn remove_tab_at(layout: &mut Layout, w: usize, t: usize) {
    let ws = &mut layout.workspaces[w];
    ws.tabs.remove(t);
    if ws.active_tab > t || ws.active_tab >= ws.tabs.len() {
        ws.active_tab = if ws.active_tab > t { ws.active_tab - 1 } else { ws.tabs.len().saturating_sub(1) };
    }
    if ws.tabs.is_empty() && layout.workspaces.len() > 1 {
        layout.workspaces.remove(w);
        if layout.active_workspace > w || layout.active_workspace >= layout.workspaces.len() {
            layout.active_workspace = if layout.active_workspace > w {
                layout.active_workspace - 1
            } else {
                layout.workspaces.len().saturating_sub(1)
            };
        }
    }
}

struct LocalPaneHandle {
    pty: Arc<LocalConPty>,
    inner: Weak<Inner>,
    id: PaneId,
}

impl PaneSource for LocalPaneHandle {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.pty.events()
    }

    fn write_input(&self, bytes: &[u8]) {
        self.pty.write_input(bytes);
    }

    fn resize(&self, rows: u16, cols: u16) {
        if let Some(inner) = self.inner.upgrade() {
            do_resize(&inner, self.id, rows, cols);
        }
    }

    fn shutdown(&self) {}
}

fn do_resize(inner: &Arc<Inner>, id: PaneId, rows: u16, cols: u16) -> bool {
    let (rows, cols) = (rows.max(1), cols.max(2));
    let changed = {
        let mut st = inner.lock();
        match st.panes.get_mut(&id) {
            Some(p) => {
                let changed = p.rows != rows || p.cols != cols;
                p.rows = rows;
                p.cols = cols;
                p.pty.resize(rows, cols);
                changed
            }
            None => return false,
        }
    };
    if changed {
        inner.emit(BackendEvent::PaneResized { pane_id: id, rows, cols, owner: None });
    }
    true
}

impl SessionBackend for LocalBackend {
    fn machine(&self) -> MachineInfo {
        MachineInfo {
            id: self.inner.machine_id,
            name: self.inner.cfg.machine_name.clone(),
            kind: MachineKind::Local,
            connected: true,
        }
    }

    fn subscribe(&self) -> async_channel::Receiver<BackendEvent> {
        self.inner.rx.clone()
    }

    fn get_layout(&self) -> BackendFuture<Layout> {
        ready(Ok(self.inner.lock().layout.clone()))
    }

    fn new_tab(&self, req: NewTab) -> BackendFuture<NewTabReply> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let r = (|| {
            if req.workspace_idx >= st.layout.workspaces.len() {
                return Err(BackendError::Invalid(format!(
                    "workspace index {} out of bounds (len={})",
                    req.workspace_idx,
                    st.layout.workspaces.len()
                )));
            }
            // "Duplicate tab": start where that pane was started (never a path a program printed).
            let cwd = req
                .cwd
                .clone()
                .or_else(|| req.inherit_cwd_from.and_then(|p| st.panes.get(&p).and_then(|p| p.spawn_cwd.clone())));
            let pane = inner.spawn_pane(&mut st, req.command.clone(), cwd, req.rows, req.cols)?;
            let tab_id = Uuid::new_v4();
            let ws = &mut st.layout.workspaces[req.workspace_idx];
            ws.tabs.push(Tab {
                id: tab_id,
                title: String::new(),
                pane_tree: PaneTree::leaf(pane),
                active_pane_id: Some(pane),
                label: String::new(),
                pane_labels: Default::default(),
            });
            ws.active_tab = ws.tabs.len() - 1;
            st.layout.active_workspace = req.workspace_idx;
            inner.emit_layout(&st);
            Ok(NewTabReply { tab_id, pane_id: pane })
        })();
        ready(r)
    }

    fn close_tab(&self, tab_id: Uuid) -> BackendFuture<()> {
        let panes = {
            let st = self.inner.lock();
            st.layout.find_tab(tab_id).map(|(w, t)| st.layout.workspaces[w].tabs[t].pane_tree.leaves())
        };
        match panes {
            None => ready(Err(BackendError::NotFound(format!("tab {tab_id}")))),
            Some(panes) => {
                // Each removal emits; the last one closes the tab.
                for p in panes {
                    self.inner.remove_pane(p, true);
                }
                ready(Ok(()))
            }
        }
    }

    fn focus_tab(&self, tab_id: Uuid) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        match st.layout.find_tab(tab_id) {
            None => ready(Err(BackendError::NotFound(format!("tab {tab_id}")))),
            Some((w, t)) => {
                st.layout.workspaces[w].active_tab = t;
                st.layout.active_workspace = w;
                self.inner.emit_layout(&st);
                ready(Ok(()))
            }
        }
    }

    fn move_tab(&self, tab_id: Uuid, new_index: usize) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        let Some((w, t)) = st.layout.find_tab(tab_id) else {
            return ready(Err(BackendError::NotFound(format!("tab {tab_id}"))));
        };
        let ws = &mut st.layout.workspaces[w];
        let active_id = ws.tabs.get(ws.active_tab).map(|t| t.id);
        let tab = ws.tabs.remove(t);
        let at = new_index.min(ws.tabs.len());
        ws.tabs.insert(at, tab);
        if let Some(a) = active_id {
            ws.active_tab = ws.tabs.iter().position(|t| t.id == a).unwrap_or(0);
        }
        self.inner.emit_layout(&st);
        ready(Ok(()))
    }

    fn create_workspace(&self, name: Option<String>, rows: u16, cols: u16) -> BackendFuture<NewWorkspaceReply> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let r = (|| {
            let pane = inner.spawn_pane(&mut st, Vec::new(), None, rows, cols)?;
            let (workspace_id, tab_id) = (Uuid::new_v4(), Uuid::new_v4());
            let n = st.layout.workspaces.len();
            st.layout.workspaces.push(Workspace {
                id: workspace_id,
                name: name.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("Workspace {}", n + 1)),
                tabs: vec![Tab {
                    id: tab_id,
                    title: String::new(),
                    pane_tree: PaneTree::leaf(pane),
                    active_pane_id: Some(pane),
                    label: String::new(),
                    pane_labels: Default::default(),
                }],
                active_tab: 0,
                color: None,
            });
            st.layout.active_workspace = n;
            inner.emit_layout(&st);
            Ok(NewWorkspaceReply { workspace_id, workspace_idx: n, tab_id, pane_id: pane })
        })();
        ready(r)
    }

    fn set_active_workspace(&self, workspace_idx: usize) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        if workspace_idx >= st.layout.workspaces.len() {
            return ready(Err(BackendError::Invalid(format!("workspace {workspace_idx}"))));
        }
        st.layout.active_workspace = workspace_idx;
        self.inner.emit_layout(&st);
        ready(Ok(()))
    }

    fn set_label(&self, target: LabelTarget, id: Uuid, label: String) -> BackendFuture<()> {
        let label = match valid_label(target, &label) {
            Ok(l) => l,
            Err(e) => return ready(Err(e)),
        };
        let mut st = self.inner.lock();
        let changed = match target {
            LabelTarget::Workspace => match st.layout.workspaces.iter_mut().find(|w| w.id == id) {
                None => return ready(Err(BackendError::NotFound(format!("workspace {id}")))),
                Some(w) => std::mem::replace(&mut w.name, label.clone()) != label,
            },
            LabelTarget::Tab => match st.layout.workspaces.iter_mut().flat_map(|w| w.tabs.iter_mut()).find(|t| t.id == id) {
                None => return ready(Err(BackendError::NotFound(format!("tab {id}")))),
                Some(t) => std::mem::replace(&mut t.label, label.clone()) != label,
            },
            LabelTarget::Pane => {
                let pane = PaneId(id);
                match st.layout.workspaces.iter_mut().flat_map(|w| w.tabs.iter_mut()).find(|t| t.pane_tree.contains(pane)) {
                    None => return ready(Err(BackendError::NotFound(format!("pane {id}")))),
                    Some(t) => {
                        let key = id.to_string();
                        let old = if label.is_empty() { t.pane_labels.remove(&key) } else { t.pane_labels.insert(key, label.clone()) };
                        old.unwrap_or_default() != label
                    }
                }
            }
        };
        if changed {
            self.inner.emit_layout(&st);
        }
        ready(Ok(()))
    }

    fn rename_workspace(&self, workspace_idx: usize, name: String) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        match st.layout.workspaces.get_mut(workspace_idx) {
            None => ready(Err(BackendError::Invalid(format!("workspace {workspace_idx}")))),
            Some(ws) => {
                if ws.name != name {
                    ws.name = name;
                    self.inner.emit_layout(&st);
                }
                ready(Ok(()))
            }
        }
    }

    fn delete_workspace(&self, workspace_idx: usize) -> BackendFuture<()> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let n = st.layout.workspaces.len();
        if workspace_idx >= n {
            return ready(Err(BackendError::Invalid(format!("workspace index {workspace_idx} out of bounds (len={n})"))));
        }
        if n <= 1 {
            return ready(Err(BackendError::Invalid("cannot delete the last remaining workspace".into())));
        }
        let ws = st.layout.workspaces.remove(workspace_idx);
        // Same clamp as the server: the same workspace stays active, or the one next to it.
        let new_len = st.layout.workspaces.len();
        if st.layout.active_workspace > workspace_idx {
            st.layout.active_workspace -= 1;
        } else if st.layout.active_workspace >= new_len {
            st.layout.active_workspace = new_len - 1;
        }
        let mut closed = Vec::new();
        for tab in &ws.tabs {
            for id in tab.pane_tree.leaves() {
                if let Some(pane) = st.panes.remove(&id) {
                    pane.pty.kill();
                    st.closed.push((id, pane.pty.clone()));
                    closed.push(id);
                }
            }
        }
        while st.closed.len() > 64 {
            st.closed.remove(0);
        }
        for id in closed {
            inner.emit(BackendEvent::PaneClosed { pane_id: id });
        }
        inner.emit_layout(&st);
        ready(Ok(()))
    }

    fn duplicate_workspace(
        &self,
        source_idx: usize,
        new_name: Option<String>,
        rows: u16,
        cols: u16,
    ) -> BackendFuture<DuplicatedWorkspace> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let r = (|| {
            let Some(src) = st.layout.workspaces.get(source_idx) else {
                return Err(BackendError::Invalid(format!(
                    "duplicate_workspace: workspace index {source_idx} out of bounds (len={})",
                    st.layout.workspaces.len()
                )));
            };
            let name = match new_name.as_deref() {
                Some(n) => n.to_string(),
                None => format!("{} (copy)", src.name),
            };
            // The directory each source tab was started in (its first pane).
            let cwds: Vec<Option<String>> = src
                .tabs
                .iter()
                .map(|t| st.panes.get(&t.pane_tree.first_leaf()).and_then(|p| p.spawn_cwd.clone()))
                .collect();
            // All or nothing: a shell that fails to start undoes the ones started before it.
            let mut spawned: Vec<PaneId> = Vec::new();
            for cwd in cwds {
                match inner.spawn_pane(&mut st, Vec::new(), cwd, rows, cols) {
                    Ok(p) => spawned.push(p),
                    Err(e) => {
                        for p in spawned {
                            if let Some(pane) = st.panes.remove(&p) {
                                pane.pty.kill();
                            }
                        }
                        return Err(e);
                    }
                }
            }
            let (workspace_id, at) = (Uuid::new_v4(), source_idx + 1);
            let tabs: Vec<Tab> = spawned
                .iter()
                .map(|p| Tab {
                    id: Uuid::new_v4(),
                    title: String::new(),
                    pane_tree: PaneTree::leaf(*p),
                    active_pane_id: None,
                    label: String::new(),
                    pane_labels: Default::default(),
                })
                .collect();
            let ids: Vec<(Uuid, PaneId)> = tabs.iter().map(|t| t.id).zip(spawned.iter().copied()).collect();
            st.layout.workspaces.insert(at, Workspace { id: workspace_id, name, tabs, active_tab: 0, color: None });
            // The source stays active; an active workspace after the insert point moves with it.
            if st.layout.active_workspace >= at {
                st.layout.active_workspace += 1;
            }
            inner.emit_layout(&st);
            Ok(DuplicatedWorkspace { workspace_id, workspace_idx: at, tabs: ids })
        })();
        ready(r)
    }

    fn move_workspace(&self, from_idx: usize, to_idx: usize) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        let n = st.layout.workspaces.len();
        if from_idx >= n || to_idx >= n {
            return ready(Err(BackendError::Invalid(format!("move_workspace: index out of bounds (len={n})"))));
        }
        if from_idx == to_idx {
            return ready(Ok(()));
        }
        st.layout.workspaces.swap(from_idx, to_idx);
        if st.layout.active_workspace == from_idx {
            st.layout.active_workspace = to_idx;
        } else if st.layout.active_workspace == to_idx {
            st.layout.active_workspace = from_idx;
        }
        self.inner.emit_layout(&st);
        ready(Ok(()))
    }

    fn set_workspace_color(&self, workspace_idx: usize, color: Option<String>) -> BackendFuture<()> {
        let color = match valid_workspace_color(color.as_deref()) {
            Ok(c) => c,
            Err(e) => return ready(Err(e)),
        };
        let mut st = self.inner.lock();
        match st.layout.workspaces.get_mut(workspace_idx) {
            None => ready(Err(BackendError::Invalid(format!("workspace {workspace_idx}")))),
            Some(ws) => {
                if ws.color != color {
                    ws.color = color;
                    self.inner.emit_layout(&st);
                }
                ready(Ok(()))
            }
        }
    }

    fn split_pane(&self, req: SplitPane) -> BackendFuture<PaneId> {
        let inner = &self.inner;
        let mut st = inner.lock();
        let r = (|| {
            let Some((w, t)) = st.layout.find_pane(req.pane_id) else {
                return Err(BackendError::NotFound(format!("pane {}", req.pane_id.0)));
            };
            // The parent's spawn cwd, not the display cwd the program reported.
            let cwd = req.cwd.clone().or_else(|| st.panes.get(&req.pane_id).and_then(|p| p.spawn_cwd.clone()));
            let new_pane = inner.spawn_pane(&mut st, Vec::new(), cwd, req.rows, req.cols)?;
            let tab = &mut st.layout.workspaces[w].tabs[t];
            tab.pane_tree.split_leaf(req.pane_id, req.direction, new_pane);
            tab.active_pane_id = Some(new_pane);
            inner.emit_layout(&st);
            Ok(new_pane)
        })();
        ready(r)
    }

    fn close_pane(&self, pane_id: PaneId) -> BackendFuture<()> {
        if !self.inner.lock().panes.contains_key(&pane_id) {
            return ready(Err(BackendError::NotFound(format!("pane {}", pane_id.0))));
        }
        self.inner.remove_pane(pane_id, true);
        ready(Ok(()))
    }

    fn set_active_pane(&self, tab_id: Uuid, pane_id: Option<PaneId>) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        let Some((w, t)) = st.layout.find_tab(tab_id) else {
            return ready(Err(BackendError::NotFound(format!("tab {tab_id}"))));
        };
        let tab = &mut st.layout.workspaces[w].tabs[t];
        if let Some(p) = pane_id {
            if !tab.pane_tree.contains(p) {
                return ready(Err(BackendError::Invalid(format!("pane {} is not in the tab", p.0))));
            }
        }
        if tab.active_pane_id != pane_id {
            tab.active_pane_id = pane_id;
            self.inner.emit_layout(&st);
        }
        ready(Ok(()))
    }

    fn resize_pane(&self, pane_id: PaneId, rows: u16, cols: u16) -> BackendFuture<()> {
        if do_resize(&self.inner, pane_id, rows, cols) {
            ready(Ok(()))
        } else {
            ready(Err(BackendError::NotFound(format!("pane {}", pane_id.0))))
        }
    }

    fn update_split_ratios(&self, ratios: Vec<SplitRatio>) -> BackendFuture<()> {
        let mut st = self.inner.lock();
        let mut changed = false;
        for r in ratios {
            if !r.ratio.is_finite() {
                continue; // the server skips entries it cannot read
            }
            let clamped = r.ratio.clamp(0.05, 0.95);
            for ws in st.layout.workspaces.iter_mut() {
                for tab in ws.tabs.iter_mut() {
                    changed |= tab.pane_tree.set_ratio(r.pane_id, clamped);
                }
            }
        }
        if changed {
            self.inner.emit_layout(&st);
        }
        ready(Ok(()))
    }

    fn get_pane_info(&self, pane_id: PaneId) -> BackendFuture<PaneInfo> {
        let st = self.inner.lock();
        match st.panes.get(&pane_id) {
            None => ready(Err(BackendError::NotFound(format!("pane {}", pane_id.0)))),
            Some(p) => ready(Ok(PaneInfo {
                pane_id,
                rows: p.rows,
                cols: p.cols,
                title: p.title.clone(),
                cwd: p.cwd.clone(),
                status: p.status,
                exited: p.pty.has_exited(),
                size_owner: None,
            })),
        }
    }

    fn attach_pane(&self, pane_id: PaneId) -> BackendFuture<Box<dyn PaneSource>> {
        let st = self.inner.lock();
        match st.panes.get(&pane_id) {
            None => ready(Err(BackendError::NotFound(format!("pane {}", pane_id.0)))),
            Some(p) => ready(Ok(Box::new(LocalPaneHandle {
                pty: p.pty.clone(),
                inner: Arc::downgrade(&self.inner),
                id: pane_id,
            }) as Box<dyn PaneSource>)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::layout::SplitDir;
    use std::time::{Duration, Instant};

    fn backend() -> LocalBackend {
        LocalBackend::new(LocalConfig {
            shell: vec!["/bin/sh".into()],
            default_cwd: Some("/tmp".into()),
            machine_name: "test".into(),
        })
    }

    fn drain(rx: &async_channel::Receiver<BackendEvent>) -> Vec<BackendEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    fn wait_for(mut f: impl FnMut() -> bool, what: &str) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timeout waiting for {what}");
    }

    fn read_until(rx: &async_channel::Receiver<PaneEvent>, needle: &str) -> String {
        let mut acc = String::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if let Ok(PaneEvent::Output(b)) = rx.try_recv() {
                acc.push_str(&String::from_utf8_lossy(&b));
                if acc.contains(needle) {
                    return acc;
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        panic!("timeout reading {needle:?}, got {acc:?}");
    }

    #[test]
    fn tabs_splits_ratios_close() {
        let b = backend();
        let rx = b.subscribe();
        let t1 = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let t2 = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs.len(), 2);
        assert_eq!(l.workspaces[0].active_tab, 1);

        let p2 = now(b.split_pane(SplitPane { pane_id: t2.pane_id, direction: SplitDir::Horizontal, rows: 24, cols: 40, cwd: None })).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs[1].pane_tree.leaves(), vec![t2.pane_id, p2]);
        assert_eq!(l.workspaces[0].tabs[1].focused_pane(), p2);

        now(b.update_split_ratios(vec![SplitRatio { pane_id: t2.pane_id, ratio: 0.3 }])).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs[1].pane_tree.ratio_at(&[]), Some(0.3));

        now(b.move_tab(t2.tab_id, 0)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs[0].id, t2.tab_id);
        assert_eq!(l.workspaces[0].active_tab, 0);

        // Resize is reported and visible in pane info.
        now(b.resize_pane(p2, 10, 33)).unwrap();
        let info = now(b.get_pane_info(p2)).unwrap();
        assert_eq!((info.rows, info.cols), (10, 33));

        // Close one pane: the sibling is promoted, the process is gone.
        now(b.close_pane(p2)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs[0].pane_tree, PaneTree::leaf(t2.pane_id));
        // Close a tab: its pty exits.
        now(b.close_tab(t1.tab_id)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs.len(), 1);
        wait_for(|| b.closed_ptys().len() == 2 && b.closed_ptys().iter().all(|(_, p)| p.try_exited()), "ptys exit");
        assert_eq!(b.live_panes(), 1);

        let evs = drain(&rx);
        assert!(evs.iter().any(|e| matches!(e, BackendEvent::PaneClosed { pane_id } if *pane_id == p2)));
        assert!(evs.iter().any(|e| matches!(e, BackendEvent::PaneResized { rows: 10, cols: 33, .. })));
        assert!(matches!(evs.last(), Some(BackendEvent::LayoutChanged(_))));
        b.shutdown_all();
    }

    #[test]
    fn echo_title_cwd_status_and_exit() {
        let b = backend();
        let rx = b.subscribe();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let src = now(b.attach_pane(t.pane_id)).unwrap();
        src.write_input(b"printf '\\033]0;hello\\007\\033]7;file:///tmp/x\\033\\\\\\033]133;C\\007\\033]133;D;3\\007'; echo ECHO-$((6*7))\n");
        let out = read_until(&src.events(), "ECHO-42");
        assert!(out.contains("ECHO-42"));
        wait_for(
            || {
                let i = now(b.get_pane_info(t.pane_id)).unwrap();
                i.title == "hello" && i.cwd == "/tmp/x" && i.status == PaneStatus::Failed
            },
            "osc state",
        );
        let evs = drain(&rx);
        assert!(evs.iter().any(|e| matches!(e, BackendEvent::TitleChanged { title, .. } if title == "hello")));
        assert!(evs.iter().any(|e| matches!(e, BackendEvent::StatusChanged { status: PaneStatus::Working, .. })));
        // Shell exit removes the pane and the (only) tab.
        src.write_input(b"exit\n");
        wait_for(|| now(b.get_layout()).unwrap().tab_count() == 0, "tab removed after exit");
        assert!(matches!(now(b.get_pane_info(t.pane_id)), Err(BackendError::NotFound(_))));
    }

    #[test]
    fn osc_notifications_become_events_at_a_limited_rate() {
        let b = backend();
        let rx = b.subscribe();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let notes = |rx: &async_channel::Receiver<BackendEvent>| -> Vec<(String, String)> {
            drain(rx)
                .into_iter()
                .filter_map(|e| match e {
                    BackendEvent::Notification { title, body, .. } => Some((title, body)),
                    _ => None,
                })
                .collect()
        };
        b.inject_osc(t.pane_id, b"\x1b]777;notify;T;B\x07");
        // A second one right after is dropped (a loop must not flood the channel).
        b.inject_osc(t.pane_id, b"\x1b]9;again\x07");
        assert_eq!(notes(&rx), vec![("T".to_string(), "B".to_string())]);
        std::thread::sleep(std::time::Duration::from_millis(600));
        b.inject_osc(t.pane_id, b"\x1b]9;later\x07");
        assert_eq!(notes(&rx), vec![("".to_string(), "later".to_string())]);
        b.shutdown_all();
    }

    #[test]
    fn escape_code_cwd_never_spawns() {
        let b = backend();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let src = now(b.attach_pane(t.pane_id)).unwrap();
        // Hostile OSC 7 (remote host), OSC 9;9 (UNC) and a device path.
        src.write_input(
            b"printf '\\033]7;file://evil/share\\007\\033]9;9;\"\\\\\\\\evil\\\\share\"\\007\\033]7;file:///%%2F%%2Fevil/x\\007'; echo HOSTILE-DONE\n",
        );
        read_until(&src.events(), "HOSTILE-DONE");
        let info = now(b.get_pane_info(t.pane_id)).unwrap();
        assert_eq!(info.cwd, "/tmp", "hostile cwd must not become the display cwd");
        // A safe local OSC 7 updates the display only.
        src.write_input(b"printf '\\033]7;file:///usr\\007'; echo SAFE-DONE\n");
        read_until(&src.events(), "SAFE-DONE");
        wait_for(|| now(b.get_pane_info(t.pane_id)).unwrap().cwd == "/usr", "display cwd");
        // The split starts where the parent was started, not in /usr and not on a share.
        let p2 = now(b.split_pane(SplitPane { pane_id: t.pane_id, direction: SplitDir::Vertical, rows: 24, cols: 80, cwd: None })).unwrap();
        let s2 = now(b.attach_pane(p2)).unwrap();
        s2.write_input(b"echo PWD=$(pwd)=END\n");
        let mut out = read_until(&s2.events(), "PWD=/");
        std::thread::sleep(Duration::from_millis(300));
        while let Ok(PaneEvent::Output(b)) = s2.events().try_recv() {
            out.push_str(&String::from_utf8_lossy(&b));
        }
        assert!(out.contains("PWD=/tmp=END"), "split cwd: {out:?}");
        b.shutdown_all();
    }

    #[test]
    fn labels() {
        let b = backend();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let l = now(b.get_layout()).unwrap();
        let ws = l.workspaces[0].id;
        now(b.set_label(LabelTarget::Tab, t.tab_id, "build".into())).unwrap();
        now(b.set_label(LabelTarget::Pane, t.pane_id.0, "logs".into())).unwrap();
        now(b.set_label(LabelTarget::Workspace, ws, "main".into())).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].name, "main");
        assert_eq!(l.workspaces[0].tabs[0].label, "build");
        assert_eq!(l.workspaces[0].tabs[0].pane_label(t.pane_id), Some("logs"));
        // Empty clears a tab or pane label, never a workspace label.
        now(b.set_label(LabelTarget::Tab, t.tab_id, " ".into())).unwrap();
        now(b.set_label(LabelTarget::Pane, t.pane_id.0, "".into())).unwrap();
        assert!(now(b.set_label(LabelTarget::Workspace, ws, "".into())).is_err());
        assert!(now(b.set_label(LabelTarget::Tab, t.tab_id, "a\u{7}b".into())).is_err());
        assert!(now(b.set_label(LabelTarget::Tab, Uuid::new_v4(), "x".into())).is_err());
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].tabs[0].label, "");
        assert_eq!(l.workspaces[0].tabs[0].pane_label(t.pane_id), None);
        b.shutdown_all();
    }

    #[test]
    fn workspaces() {
        let b = backend();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        let w = now(b.create_workspace(Some("work".into()), 24, 80)).unwrap();
        assert_eq!(w.workspace_idx, 1);
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.active_workspace, 1);
        assert_eq!(l.workspaces[1].name, "work");
        now(b.rename_workspace(1, "renamed".into())).unwrap();
        now(b.set_active_workspace(0)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!((l.active_workspace, l.workspaces[1].name.as_str()), (0, "renamed"));
        assert!(now(b.new_tab(NewTab { workspace_idx: 9, ..Default::default() })).is_err());
        // Closing the last tab of a non-last workspace drops that workspace.
        now(b.close_tab(w.tab_id)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces.len(), 1);
        assert_eq!(l.workspaces[0].tabs[0].id, t.tab_id);
        b.shutdown_all();
    }

    #[test]
    fn workspace_delete_duplicate_move_color() {
        let b = backend();
        let rx = b.subscribe();
        let t0 = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, ..Default::default() })).unwrap();
        // The last workspace cannot go.
        assert!(now(b.delete_workspace(0)).is_err());
        assert!(now(b.delete_workspace(5)).is_err());

        // A copy: one fresh shell per tab, named "<source> (copy)", right after the source, no
        // color, and the source stays active.
        now(b.set_workspace_color(0, Some("#E0493A".into()))).unwrap();
        let d = now(b.duplicate_workspace(0, None, 24, 80)).unwrap();
        assert_eq!((d.workspace_idx, d.tabs.len()), (1, 2));
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces.len(), 2);
        assert_eq!(l.workspaces[1].name, "Default (copy)");
        assert_eq!(l.workspaces[1].id, d.workspace_id);
        assert_eq!(l.workspaces[1].color, None, "the color does not carry over");
        assert_eq!(l.workspaces[0].color.as_deref(), Some("#e0493a"));
        assert_eq!(l.active_workspace, 0);
        let copy_panes: Vec<PaneId> = d.tabs.iter().map(|t| t.1).collect();
        assert_eq!(l.workspaces[1].tabs.iter().map(|t| t.pane_tree.first_leaf()).collect::<Vec<_>>(), copy_panes);
        assert!(copy_panes.iter().all(|p| b.pty(*p).is_some()));
        assert!(!l.workspaces[0].tabs.iter().any(|t| copy_panes.contains(&t.pane_tree.first_leaf())));
        let named = now(b.duplicate_workspace(1, Some("mine".into()), 24, 80)).unwrap();
        assert_eq!(now(b.get_layout()).unwrap().workspaces[2].name, "mine");
        assert_eq!(named.workspace_idx, 2);

        // Move swaps, and the active workspace follows its workspace.
        now(b.set_active_workspace(2)).unwrap();
        now(b.move_workspace(2, 0)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces[0].name, "mine");
        assert_eq!(l.workspaces[2].name, "Default");
        assert_eq!(l.active_workspace, 0);
        assert!(now(b.move_workspace(0, 9)).is_err());
        now(b.move_workspace(1, 1)).unwrap();

        // Color: only #rrggbb, and clearing works. An unchanged color sends no event.
        assert!(now(b.set_workspace_color(1, Some("red".into()))).is_err());
        assert!(now(b.set_workspace_color(9, None)).is_err());
        now(b.set_workspace_color(1, Some("#3987e4".into()))).unwrap();
        assert_eq!(now(b.get_layout()).unwrap().workspaces[1].color.as_deref(), Some("#3987e4"));
        while rx.try_recv().is_ok() {}
        now(b.set_workspace_color(1, Some("#3987E4".into()))).unwrap();
        assert!(rx.try_recv().is_err());
        now(b.set_workspace_color(1, None)).unwrap();
        assert_eq!(now(b.get_layout()).unwrap().workspaces[1].color, None);

        // Delete: its panes close (one PaneClosed each, before the layout), the active
        // workspace stays the same workspace.
        while rx.try_recv().is_ok() {}
        let doomed: Vec<PaneId> = now(b.get_layout()).unwrap().workspaces[1].tabs.iter().map(|t| t.pane_tree.first_leaf()).collect();
        now(b.set_active_workspace(2)).unwrap();
        now(b.delete_workspace(1)).unwrap();
        let l = now(b.get_layout()).unwrap();
        assert_eq!(l.workspaces.len(), 2);
        assert_eq!(l.active_workspace, 1, "the active workspace moved up one place with the list");
        assert!(doomed.iter().all(|p| b.pty(*p).is_none()));
        assert!(b.closed_ptys().iter().any(|(p, _)| *p == doomed[0]));
        let evs = drain(&rx);
        let closed = evs.iter().filter(|e| matches!(e, BackendEvent::PaneClosed { .. })).count();
        assert_eq!(closed, doomed.len());
        assert!(matches!(evs.last(), Some(BackendEvent::LayoutChanged(_))));
        // The first workspace still has its own tabs.
        assert!(l.find_tab(t0.tab_id).is_some());
        b.shutdown_all();
    }

    #[test]
    fn duplicate_tab_starts_where_the_source_pane_started() {
        let b = backend();
        let t = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, cwd: Some("/tmp".into()), ..Default::default() })).unwrap();
        let t2 = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, inherit_cwd_from: Some(t.pane_id), ..Default::default() })).unwrap();
        let (a, c) = (now(b.get_pane_info(t.pane_id)).unwrap(), now(b.get_pane_info(t2.pane_id)).unwrap());
        assert_eq!(a.cwd, "/tmp");
        assert_eq!(c.cwd, "/tmp");
        // An explicit cwd wins over the inherited one.
        let t3 = now(b.new_tab(NewTab { workspace_idx: 0, rows: 24, cols: 80, cwd: Some("/".into()), inherit_cwd_from: Some(t.pane_id), ..Default::default() })).unwrap();
        assert_eq!(now(b.get_pane_info(t3.pane_id)).unwrap().cwd, "/");
        b.shutdown_all();
    }
}
