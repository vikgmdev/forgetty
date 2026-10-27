//! Machines in the shell: the local one plus every paired remote, per-machine
//! connection state, pairing, toast notifications and the network/resume
//! pokes. The fields of `AppShell` hold the machine on screen; the others
//! wait in `parked` and are swapped in when selected.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forgetty_client::proto::ClientKind;
use forgetty_client::{AddrCache, ClientInfo};
use forgetty_remote::store::{self, MachineStore, PairingOffer, StoredMachine};
use forgetty_remote::{RemoteBackend, RemoteParams};
use forgetty_shell_core::notify::{self, ToastText, Visibility};
use forgetty_shell_core::{
    BackendEvent, ConnState, Layout, MachineInfo, MachineKind, NewTab, PaneId, PaneStatus, SessionBackend, Uuid,
};
use gpui::{AppContext as _, Context, Entity, Keystroke, Window};

use crate::shell::{AppShell, Drag, PaneMeta, Rename};
use crate::view::TerminalView;
use crate::winsys::{Hooks, Signal};

/// Quiet time that ends a burst of network events.
const NETWORK_QUIET: Duration = Duration::from_millis(400);
/// A burst never delays the poke longer than this.
const NETWORK_QUIET_CAP: Duration = Duration::from_secs(3);

/// The overlay of a machine that is reached on pinned addresses only and does not answer.
pub const UNREACHABLE_TEXT: &str = "Can't reach the server. Is WireGuard on?";

/// Everything the shell needs to reach remote machines.
pub struct RemoteCtx {
    pub rt: tokio::runtime::Handle,
    pub store: MachineStore,
    pub device_name: String,
    pub addr_cache: AddrCache,
}

pub struct MachineEntry {
    pub id: Uuid,
    pub name: String,
    pub kind: MachineKind,
    pub conn: ConnState,
    pub backend: Arc<dyn SessionBackend>,
    /// The first layout arrived (an empty remote machine then gets one tab).
    pub bootstrapped: bool,
    /// The address cache was written once with this machine connected. The first Connected seen
    /// saves it, whether the machine was already connected when it was added or not.
    pub addr_saved: bool,
}

impl MachineEntry {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        let m = backend.machine();
        MachineEntry {
            id: m.id,
            name: m.name,
            kind: m.kind,
            conn: backend.connection(),
            backend,
            bootstrapped: false,
            addr_saved: false,
        }
    }
}

/// State of a machine that is not on screen.
pub struct MachineSlot {
    pub backend: Arc<dyn SessionBackend>,
    pub machine: MachineInfo,
    pub layout: Layout,
    pub views: HashMap<PaneId, Entity<TerminalView>>,
    pub attaching: HashSet<PaneId>,
    pub meta: HashMap<PaneId, PaneMeta>,
    pub local_active: HashMap<Uuid, PaneId>,
    pub zoomed: HashMap<Uuid, PaneId>,
    pub attention: HashSet<PaneId>,
    pub tab_colors: HashMap<Uuid, u32>,
    pub had_tabs: bool,
}

impl MachineSlot {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        MachineSlot {
            machine: backend.machine(),
            backend,
            layout: Layout::default(),
            views: HashMap::new(),
            attaching: HashSet::new(),
            meta: HashMap::new(),
            local_active: HashMap::new(),
            zoomed: HashMap::new(),
            attention: HashSet::new(),
            tab_colors: HashMap::new(),
            had_tabs: false,
        }
    }
}

/// A toast the shell decided to show (kept for the CI smoke).
#[derive(Clone, Debug)]
pub struct ToastRecord {
    pub machine: Uuid,
    pub pane: PaneId,
    pub text: ToastText,
    pub target: String,
}

pub enum PairStage {
    /// Waiting for the pasted payload.
    Input,
    /// Parsed. Nothing is paired until the user confirms.
    Confirm(PairingOffer),
    Working(PairingOffer),
    Failed(String),
}

pub struct PairDialog {
    pub stage: PairStage,
    /// What was pasted, as typed (never shown in full).
    pub raw: String,
    /// A pairing offer found on the clipboard while the dialog waits. It is only SHOWN: the user
    /// presses Pair to review it. The watcher never moves the dialog on by itself.
    pub prefill: Option<(String, store::PairingOffer)>,
}

impl AppShell {
    // ---- setup -----------------------------------------------------------------

    /// Start the remote side: stored machines connect in the background and the
    /// system hooks (network change, resume, toasts) start feeding the shell.
    pub fn init_remote(&mut self, remote: RemoteCtx, hooks: Hooks, cx: &mut Context<Self>) {
        let stored = remote.store.machines();
        self.osc52 = forgetty_shell_core::osc52::Policies::open(remote.store.dir());
        self.remote = Some(remote);

        let signals = hooks.signals.clone();
        let clicks = hooks.clicks.clone();
        self.hooks = Some(hooks);

        cx.spawn(async move |this, cx| {
            while let Ok(first) = signals.recv().await {
                // A change comes as a burst of interface events: wait for quiet, but never
                // longer than `NETWORK_QUIET_CAP`, so an address that keeps flapping still
                // pokes at least once in that time.
                let mut resume = first == Signal::Resume;
                let started = Instant::now();
                loop {
                    cx.background_executor().timer(NETWORK_QUIET).await;
                    let mut more = false;
                    while let Ok(s) = signals.try_recv() {
                        more = true;
                        resume |= s == Signal::Resume;
                    }
                    if !more || started.elapsed() >= NETWORK_QUIET_CAP {
                        break;
                    }
                }
                let _ = resume;
                if this.update(cx, |s, cx| s.poke_network(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            while let Ok(arg) = clicks.recv().await {
                let handle = this.update(cx, |s, _| s.win_handle).ok();
                let Some(handle) = handle else { break };
                let _ = cx.update_window(handle, |_, window, cx| {
                    window.activate_window();
                    let _ = this.update(cx, |s, cx| s.on_toast_click(&arg, window, cx));
                });
            }
        })
        .detach();

        // The "reconnecting" overlay appears 1 s after the link drops: repaint
        // while any machine is without a connection so it shows on time.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let alive = this
                .update(cx, |s, cx| {
                    if s.machines.iter().any(|m| matches!(m.conn, ConnState::Reconnecting { .. } | ConnState::UnreachableDirectOnly { .. })) {
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        // The local server's machine is the supervisor's: it starts the server first.
        for m in stored.into_iter().filter(|m| !m.local) {
            self.connect_stored(m, false, cx);
        }
    }

    /// One task per machine: backend events to `machine_events`.
    pub(crate) fn spawn_event_pump(backend: Arc<dyn SessionBackend>, cx: &mut Context<Self>) {
        let id = backend.machine().id;
        let rx = backend.subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                let mut evs = vec![first];
                while let Ok(e) = rx.try_recv() {
                    evs.push(e);
                }
                // A machine that was dropped from the list ends its pump.
                let alive = this
                    .update(cx, |s, cx| {
                        let known = s.machines.iter().any(|m| m.id == id);
                        if known {
                            s.machine_events(id, evs, cx);
                        }
                        known
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- connecting ------------------------------------------------------------

    /// Connect to a stored machine. `switch` selects it when it is added.
    pub fn connect_stored(&mut self, m: StoredMachine, switch: bool, cx: &mut Context<Self>) {
        let Some(r) = self.remote.as_ref() else { return };
        let (rt, store, device, cache) = (r.rt.clone(), r.store.clone(), r.device_name.clone(), r.addr_cache.clone());
        if self.machines.iter().any(|e| e.id == m.id) {
            return;
        }
        if switch {
            self.switch_to = Some(m.id);
        }
        let task = rt.spawn(async move {
            let params = RemoteParams {
                machine_id: m.id,
                name: m.name.clone(),
                server: store::machine_addr(&m).map_err(|e| e.to_string())?,
                identity: store.identity().map_err(|e| e.to_string())?,
                info: ClientInfo::new(
                    store.client_id().map_err(|e| e.to_string())?,
                    ClientKind::Windows,
                    device,
                    env!("CARGO_PKG_VERSION"),
                ),
                opts: store::machine_options(&cache, &m),
            };
            RemoteBackend::start(params).await.map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(Ok(b)) => {
                    let backend: Arc<dyn SessionBackend> = Arc::new(b);
                    let _ = this.update(cx, |s, cx| s.add_machine(backend, cx));
                }
                Ok(Err(e)) => eprintln!("forgetty-win: connect: {e}"),
                Err(e) => eprintln!("forgetty-win: connect task: {e}"),
            }
        })
        .detach();
    }

    pub fn add_machine(&mut self, backend: Arc<dyn SessionBackend>, cx: &mut Context<Self>) {
        let info = backend.machine();
        if self.machines.iter().any(|m| m.id == info.id) {
            return;
        }
        self.machines.push(MachineEntry::new(backend.clone()));
        if let Some(e) = self.machines.last_mut() {
            if e.conn.is_connected() && e.kind == MachineKind::Remote {
                e.addr_saved = true;
                self.save_addr_cache();
            }
        }
        self.parked.insert(info.id, MachineSlot::new(backend.clone()));
        Self::spawn_event_pump(backend.clone(), cx);
        self.fetch_layout(info.id, cx);
        if self.switch_to == Some(info.id) {
            self.switch_to = None;
            self.switch_machine(info.id, cx);
        }
        self.adopted(info.id, cx);
        cx.notify();
    }

    /// Ask a machine for its layout and feed the answer through the event path.
    fn fetch_layout(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(e) = self.machines.iter().find(|m| m.id == id) else { return };
        let fut = e.backend.get_layout();
        cx.spawn(async move |this, cx| match fut.await {
            Ok(l) => {
                let _ = this.update(cx, |s, cx| s.machine_events(id, vec![BackendEvent::LayoutChanged(l)], cx));
            }
            Err(e) => eprintln!("forgetty-win: get_layout: {e}"),
        })
        .detach();
    }

    // ---- events ------------------------------------------------------------------

    /// Backend events of one machine, on screen or not.
    pub(crate) fn machine_events(&mut self, id: Uuid, evs: Vec<BackendEvent>, cx: &mut Context<Self>) {
        let shown = self.machine.id;
        let mut rest = Vec::with_capacity(evs.len());
        for ev in evs {
            match ev {
                BackendEvent::ConnectionChanged(st) => self.on_conn(id, st, cx),
                BackendEvent::Notification { pane_id, title, body } => {
                    let mode = self.cfg.notification_mode;
                    if notify::mode_allows_ring(mode) {
                        self.raise_attention(id, pane_id, cx);
                    }
                    self.with_machine(id, |s| {
                        let vis = s.visibility(shown, id, pane_id);
                        if notify::mode_allows_toast(mode) && notify::notification_wants_toast(vis) {
                            let tab = s.tab_title_of_pane(pane_id);
                            let text = notify::note_text(&s.machine.name, &tab, &title, &body);
                            s.show_toast(id, pane_id, text);
                        }
                    });
                }
                BackendEvent::StatusChanged { pane_id, status, message } => {
                    self.with_machine(id, |s| {
                        let prev = s.meta.get(&pane_id).map(|m| m.status).unwrap_or_default();
                        let vis = s.visibility(shown, id, pane_id);
                        if notify::mode_allows_toast(s.cfg.notification_mode) && notify::status_wants_toast(prev, status, vis) {
                            let tab = s.tab_title_of_pane(pane_id);
                            let text = notify::status_text(&s.machine.name, &tab, status, &message);
                            s.show_toast(id, pane_id, text);
                        }
                    });
                    rest.push(BackendEvent::StatusChanged { pane_id, status, message });
                }
                other => rest.push(other),
            }
        }
        if rest.is_empty() {
            return;
        }
        let first_layout = rest.iter().any(|e| matches!(e, BackendEvent::LayoutChanged(_)));
        if id == self.machine.id {
            self.on_events(rest, cx);
        } else {
            let was = std::mem::replace(&mut self.attach_enabled, false);
            self.with_machine(id, |s| s.on_events(rest, cx));
            self.attach_enabled = was;
        }
        if first_layout {
            self.after_layout(id, cx);
        }
    }

    /// A remote machine's first layout: with no tab at all, open one.
    fn after_layout(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(e) = self.machines.iter_mut().find(|m| m.id == id) else { return };
        if e.bootstrapped || e.kind == MachineKind::Local {
            return;
        }
        e.bootstrapped = true;
        // The tab `--working-directory` / `-e` asked for belongs to the local server's machine.
        let launch = if self.lsrv.machine == Some(id) { self.launch.take() } else { None };
        let (empty, active_ws) = if id == self.machine.id {
            (self.layout.tab_count() == 0, self.layout.active_workspace)
        } else {
            self.parked.get(&id).map(|p| (p.layout.tab_count() == 0, p.layout.active_workspace)).unwrap_or((false, 0))
        };
        let r = self.content_rect();
        let (rows, cols) = self.grid_for(r.w, r.h);
        let Some(backend) = self.machines.iter().find(|m| m.id == id).map(|m| m.backend.clone()) else { return };
        if empty || launch.is_some() {
            // No tab at all, or a command line that asks for one: open it (beside the tabs the
            // server kept; GTK starts without them, the server's tabs outlive the window).
            let l = launch.unwrap_or_default();
            let fut = backend.new_tab(NewTab {
                workspace_idx: if empty { 0 } else { active_ws },
                rows,
                cols,
                command: l.command,
                cwd: l.cwd,
                ..Default::default()
            });
            cx.spawn(async move |_, _| {
                if let Err(e) = fut.await {
                    eprintln!("forgetty-win: first tab: {e}");
                }
            })
            .detach();
        } else if self.lsrv.machine == Some(id) && self.cfg.on_launch == forgetty_shell_core::config::OnLaunch::New {
            // `on_launch = "new"`: the server keeps its workspaces, and this start opens a new
            // one beside them and shows it (GTK opens a fresh window; there is one window here).
            let fut = backend.create_workspace(None, rows, cols);
            cx.spawn(async move |_, _| match fut.await {
                Ok(reply) => {
                    if let Err(e) = backend.set_active_workspace(reply.workspace_idx).await {
                        eprintln!("forgetty-win: on_launch new, show it: {e}");
                    }
                }
                Err(e) => eprintln!("forgetty-win: on_launch new: {e}"),
            })
            .detach();
        }
    }

    fn on_conn(&mut self, id: Uuid, st: ConnState, cx: &mut Context<Self>) {
        let was_connected = self.machines.iter().find(|m| m.id == id).is_some_and(|m| m.conn.is_connected());
        if let Some(e) = self.machines.iter_mut().find(|m| m.id == id) {
            e.conn = st.clone();
        }
        let remote = self.machines.iter().any(|m| m.id == id && m.kind == MachineKind::Remote);
        // A machine added while already connected never has a "was not connected" step: its
        // first Connected still writes the address cache.
        let first = st.is_connected() && remote && self.machines.iter().any(|m| m.id == id && !m.addr_saved);
        if first {
            if let Some(e) = self.machines.iter_mut().find(|m| m.id == id) {
                e.addr_saved = true;
            }
            self.save_addr_cache();
        }
        if st.is_connected() && !was_connected && remote {
            // A new connection has no size on record at the server (a restarted server
            // restores its panes at the default size, and a resumed stream has no reset):
            // send each pane's grid again so the PTY follows this client (AD-020).
            self.with_machine(id, |s| {
                for v in s.views.values() {
                    v.read(cx).reassert_size();
                }
            });
            // Events may have been missed while the link was down.
            self.fetch_layout(id, cx);
            if !first {
                self.save_addr_cache();
            }
        }
        cx.notify();
    }

    fn save_addr_cache(&self) {
        if let Some(r) = &self.remote {
            let (store, cache) = (r.store.clone(), r.addr_cache.clone());
            std::thread::spawn(move || {
                if let Err(e) = store.save_addr_cache(&cache) {
                    eprintln!("forgetty-win: address cache: {e}");
                }
            });
        }
    }

    /// Run `f` with machine `id`'s state in the shell fields.
    pub(crate) fn with_machine<R>(&mut self, id: Uuid, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        if id == self.machine.id {
            return Some(f(self));
        }
        let mut slot = self.parked.remove(&id)?;
        self.swap_slot(&mut slot);
        let r = f(self);
        self.swap_slot(&mut slot);
        self.parked.insert(id, slot);
        Some(r)
    }

    fn swap_slot(&mut self, slot: &mut MachineSlot) {
        std::mem::swap(&mut self.backend, &mut slot.backend);
        std::mem::swap(&mut self.machine, &mut slot.machine);
        std::mem::swap(&mut self.layout, &mut slot.layout);
        std::mem::swap(&mut self.views, &mut slot.views);
        std::mem::swap(&mut self.attaching, &mut slot.attaching);
        std::mem::swap(&mut self.meta, &mut slot.meta);
        std::mem::swap(&mut self.local_active, &mut slot.local_active);
        std::mem::swap(&mut self.zoomed, &mut slot.zoomed);
        std::mem::swap(&mut self.attention, &mut slot.attention);
        std::mem::swap(&mut self.tab_colors, &mut slot.tab_colors);
        std::mem::swap(&mut self.had_tabs, &mut slot.had_tabs);
    }

    // ---- switching --------------------------------------------------------------

    pub fn switch_machine(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if id == self.machine.id {
            return;
        }
        let Some(mut slot) = self.parked.remove(&id) else { return };
        // The old machine's panes stop asking for frames.
        for v in self.views.values() {
            v.update(cx, |v, cx| {
                v.set_visible(false, cx);
                v.set_focused(false, cx);
            });
        }
        self.swap_slot(&mut slot);
        self.parked.insert(slot.machine.id, slot);
        self.palette = None;
        self.menu = None;
        self.confirm = None;
        self.rename = None::<Rename>;
        self.drag = None::<Drag>;
        let panes = self.layout.all_panes();
        for p in panes {
            self.attach(p, cx);
        }
        self.sync_dirty = true;
        cx.notify();
    }

    pub fn cycle_machine(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.machines.len() as isize;
        if n < 2 {
            return;
        }
        let cur = self.machines.iter().position(|m| m.id == self.machine.id).unwrap_or(0) as isize;
        let to = ((cur + delta) % n + n) % n;
        let id = self.machines[to as usize].id;
        self.switch_machine(id, cx);
    }

    /// Connection state of the machine on screen.
    pub fn active_conn(&self) -> Option<&ConnState> {
        self.machines.iter().find(|m| m.id == self.machine.id).map(|m| &m.conn)
    }

    /// Text for the "reconnecting" overlay on the active machine's panes, or
    /// `None` while the link is fine or has been down for less than 1 s.
    pub fn overlay_text(&self, now: Instant) -> Option<String> {
        let conn = self.active_conn()?;
        if !conn.show_overlay(now) {
            return None;
        }
        Some(match conn {
            ConnState::Failed(why) => format!("Cannot connect to {}: {}", self.machine.name, why),
            ConnState::UnreachableDirectOnly { .. } => UNREACHABLE_TEXT.to_string(),
            _ => format!("Reconnecting to {}...", self.machine.name),
        })
    }

    /// The overlay offers a Retry button: the machine is reached on its pinned addresses only
    /// and none answered.
    pub fn overlay_retry(&self) -> bool {
        matches!(self.active_conn(), Some(ConnState::UnreachableDirectOnly { .. }))
    }

    /// The Retry button: dial the machine on screen now.
    pub fn retry_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = self.machines.iter().find(|m| m.id == self.machine.id) {
            m.backend.reconnect_now();
        }
        cx.notify();
    }

    // ---- network and power ------------------------------------------------------

    /// The OS reported a network change or the PC resumed: make every remote
    /// backend probe its connection.
    pub fn poke_network(&mut self, _cx: &mut Context<Self>) {
        self.net_pokes += 1;
        for m in &self.machines {
            if m.kind == MachineKind::Remote {
                m.backend.network_changed();
            }
        }
    }

    // ---- toasts --------------------------------------------------------------------

    /// `shown` is the machine on screen. Inside `with_machine` the shell fields
    /// hold the pane's machine, so the caller reads `shown` before the swap.
    fn visibility(&self, shown: Uuid, machine: Uuid, pane: PaneId) -> Visibility {
        Visibility {
            window_active: self.window_active,
            machine_active: machine == shown,
            pane_focused: self.active_pane() == Some(pane),
        }
    }

    fn tab_title_of_pane(&self, pane: PaneId) -> String {
        match self.layout.find_pane(pane).and_then(|(w, t)| self.layout.workspaces.get(w)?.tabs.get(t)) {
            Some(tab) => self.tab_title(tab),
            None => String::new(),
        }
    }

    /// Show a toast unless the rate limit says no. Called with the pane's
    /// machine in the shell fields (`with_machine`).
    fn show_toast(&mut self, machine: Uuid, pane: PaneId, text: ToastText) {
        if !self.toast_limiter.allow(machine, pane, Instant::now()) {
            return;
        }
        let target = notify::encode_target(machine, pane);
        if self.toast_log.len() < 64 {
            self.toast_log.push(ToastRecord { machine, pane, text: text.clone(), target: target.clone() });
        }
        if let Some(h) = &self.hooks {
            h.toast(text, target);
        }
    }

    /// A toast was clicked: show its machine, tab and pane.
    pub fn on_toast_click(&mut self, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((machine, pane)) = notify::parse_target(arg) else { return };
        self.focus_target(machine, pane, window, cx);
    }

    pub fn focus_target(&mut self, machine: Uuid, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_machine(machine, cx);
        if self.machine.id != machine {
            return;
        }
        let Some((w, t)) = self.layout.find_pane(pane) else { return };
        let tab = self.layout.workspaces[w].tabs[t].id;
        if self.layout.active_workspace != w {
            self.set_workspace(w, cx);
        }
        self.focus_tab(tab, cx);
        self.focus_pane(pane, window, cx);
        self.zoomed.remove(&tab);
        self.request_sync(cx);
    }

    // ---- pairing ---------------------------------------------------------------

    pub fn open_pair(&mut self, cx: &mut Context<Self>) {
        self.palette = None;
        self.pair = Some(PairDialog { stage: PairStage::Input, raw: String::new(), prefill: None });
        self.pair_clip_seen = None;
        // While the dialog is open, a pairing payload that lands on the clipboard (the user
        // copied the output of `forgetty pair`) is taken without a paste.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            match this.update(cx, |s, cx| s.pair_clipboard_tick(cx)) {
                Ok(true) => {}
                _ => break,
            }
        })
        .detach();
        cx.notify();
    }

    /// One look at the clipboard for the pair dialog. `false` once the dialog is gone. A text that
    /// parses as a pairing payload is only OFFERED in the dialog (`prefill`): the user presses
    /// Pair, then Confirm. The watcher never submits.
    ///
    /// Text that our own OSC 52 path put on the clipboard is ignored: a program in a pane could
    /// otherwise plant an offer for a server of its own. That is checked by the clipboard change
    /// counter (the number right after our write) and by the text itself.
    pub fn pair_clipboard_tick(&mut self, cx: &mut Context<Self>) -> bool {
        match &self.pair {
            None => return false,
            Some(d) if !matches!(d.stage, PairStage::Input) => return true,
            _ => {}
        }
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else { return true };
        if self.pair_clip_seen.as_deref() == Some(text.as_str()) {
            return true;
        }
        self.pair_clip_seen = Some(text.clone());
        if self.clipboard_is_from_osc52(&text) {
            return true;
        }
        if let Ok(offer) = store::parse_offer(&text) {
            let t: String = text.chars().take(65536).collect();
            if let Some(d) = self.pair.as_mut() {
                d.prefill = Some((t, offer));
            }
            cx.notify();
        }
        true
    }

    /// The user pressed Pair on the offered text: review it (still nothing contacted).
    pub fn pair_use_prefill(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.pair.as_mut().and_then(|d| d.prefill.take()).map(|(t, _)| t) else { return };
        self.pair_submit_text(&text, cx);
    }

    /// The pasted text: parse it and ask for confirmation. Nothing is contacted.
    pub fn pair_submit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let stage = match store::parse_offer(text) {
            Ok(offer) => PairStage::Confirm(offer),
            Err(e) => PairStage::Failed(e),
        };
        match &mut self.pair {
            Some(d) => {
                d.raw = text.to_string();
                d.stage = stage;
            }
            None => self.pair = Some(PairDialog { stage, raw: text.to_string(), prefill: None }),
        }
        cx.notify();
    }

    /// The user confirmed: pair now.
    pub fn pair_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.pair.as_mut() else { return };
        let offer = match &d.stage {
            PairStage::Confirm(o) => o.clone(),
            _ => return,
        };
        let Some(r) = self.remote.as_ref() else {
            d.stage = PairStage::Failed("Remote machines are not available in this build.".into());
            return;
        };
        d.stage = PairStage::Working(offer.clone());
        let (rt, store, device) = (r.rt.clone(), r.store.clone(), r.device_name.clone());
        let hint = self.pair_hint;
        let task = rt.spawn(async move { store::pair(&store, &offer, &device, hint).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let res = task.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |s, cx| s.pair_done(res, cx));
        })
        .detach();
        cx.notify();
    }

    fn pair_done(&mut self, res: Result<StoredMachine, String>, cx: &mut Context<Self>) {
        match res {
            Ok(m) => {
                self.pair = None;
                self.connect_stored(m, true, cx);
            }
            Err(e) => {
                if let Some(d) = self.pair.as_mut() {
                    d.stage = PairStage::Failed(format!("Pairing failed: {e}"));
                }
            }
        }
        cx.notify();
    }

    pub fn pair_cancel(&mut self, cx: &mut Context<Self>) {
        self.pair = None;
        cx.notify();
    }

    pub(crate) fn pair_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let key = ks.key.as_str();
        let m = &ks.modifiers;
        let Some(d) = self.pair.as_mut() else { return };
        match &d.stage {
            PairStage::Input => {
                let paste = (m.control && key == "v") || (m.shift && key == "insert");
                if key == "escape" {
                    self.pair = None;
                } else if paste {
                    if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        let t: String = t.chars().take(65536).collect();
                        self.pair_submit_text(&t, cx);
                    }
                } else if key == "enter" && !d.raw.is_empty() {
                    let raw = d.raw.clone();
                    self.pair_submit_text(&raw, cx);
                }
            }
            PairStage::Confirm(_) => match key {
                "enter" => self.pair_confirm(cx),
                "escape" => self.pair = None,
                _ => {}
            },
            PairStage::Working(_) => {
                if key == "escape" {
                    self.pair = None;
                }
            }
            PairStage::Failed(_) => match key {
                "enter" => d.stage = PairStage::Input,
                "escape" => self.pair = None,
                _ => {}
            },
        }
        cx.notify();
    }

    /// Smoke hook: the status shown for a pane (for tests).
    #[allow(dead_code)]
    pub fn status_of(&self, pane: PaneId) -> PaneStatus {
        self.meta.get(&pane).map(|m| m.status).unwrap_or_default()
    }
}
