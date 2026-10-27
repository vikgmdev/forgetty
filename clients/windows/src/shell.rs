//! The app shell: tabs, splits, sidebar, palette. It talks only to a
//! `SessionBackend` (layout requests out, `BackendEvent`s in), so it cannot
//! tell a local ConPTY backend from a remote one.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use forgetty_shell_core::fuzzy;
use forgetty_shell_core::keys::{Action, Chord, Keymap, ACTIONS};
use forgetty_shell_core::layout::{neighbor, Divider, PaneGeometry, Rect, Side};
use forgetty_shell_core::notify::ToastLimiter;
use forgetty_shell_core::{
    BackendEvent, BackendFuture, LabelTarget, Layout, MachineInfo, NewTab, PaneId, PaneSource, PaneStatus, SessionBackend,
    SplitDir, SplitPane, SplitRatio, Tab, Uuid,
};
use gpui::{AppContext as _,
    Context, Entity, FocusHandle, Focusable, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Window,
};

use crate::config::{Backdrop, Config};
use crate::lineedit::{EditResult, LineEdit};
use crate::machines::{MachineEntry, MachineSlot, PairDialog, RemoteCtx, ToastRecord};
use crate::smoke::ShellSmoke;
use crate::view::{Metrics, TerminalView};

pub const TITLEBAR_H: f32 = 38.0;
pub const SIDEBAR_W: f32 = 232.0;
/// Divider thickness between panes, in px.
pub const BAR: f32 = 1.0;
pub const MIN_PANE_W: f32 = 120.0;
pub const MIN_PANE_H: f32 = 60.0;
pub const CAPTION_W: f32 = 138.0;
pub const TAB_LEFT: f32 = 10.0;
pub const PLUS_W: f32 = 34.0;
/// The drop-down arrow next to the new-tab button.
pub const CHEVRON_W: f32 = 22.0;
const TAB_MIN_W: f32 = 104.0;
const TAB_MAX_W: f32 = 220.0;

#[derive(Clone, Debug, Default)]
pub struct PaneMeta {
    pub title: String,
    pub cwd: String,
    pub status: PaneStatus,
    pub message: String,
    /// PTY size the backend last reported (`PaneResized`).
    pub size: Option<(u16, u16)>,
    /// The client that owns the PTY size (AD-026).
    pub owner: Option<forgetty_shell_core::SizeOwnerInfo>,
}

/// What Enter does on a palette row.
#[derive(Clone, Debug)]
pub enum Pick {
    Action(Action),
    /// A theme by name (the "Choose theme" list).
    Theme(String),
}

pub struct PaletteItem {
    pub pick: Pick,
    pub title: String,
    pub shortcut: String,
    pub hits: Vec<usize>,
}

pub struct Palette {
    pub edit: LineEdit,
    pub selected: usize,
    pub items: Vec<PaletteItem>,
    /// The theme list is showing instead of the commands: every theme name.
    pub themes: Option<Vec<String>>,
}

pub struct Rename {
    /// What the label names.
    pub target: LabelTarget,
    /// Id of the workspace, tab or pane being named.
    pub id: Uuid,
    /// The tab that shows the editor (nil for a workspace).
    pub tab: Uuid,
    pub edit: LineEdit,
}

pub enum Drag {
    Tab { id: Uuid, index: usize },
    Divider { tab: Uuid, divider: Divider, ratio: f32 },
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: u32,
    pub chrome: u32,
    pub tab_active: u32,
    pub hover: u32,
    pub border: u32,
    pub fg: u32,
    pub dim: u32,
    pub accent: u32,
}

pub fn blend(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let x = ((a >> s) & 0xff) as f32;
        let y = ((b >> s) & 0xff) as f32;
        ((x + (y - x) * t).round() as u32).min(255)
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

impl Theme {
    pub fn from_cfg(cfg: &Config) -> Self {
        let light = {
            let b = cfg.background;
            ((b >> 16) & 0xff) + ((b >> 8) & 0xff) + (b & 0xff) > 3 * 140
        };
        let toward = if light { 0x000000 } else { 0xffffff };
        Theme {
            bg: cfg.background,
            chrome: blend(cfg.background, toward, 0.05),
            tab_active: cfg.background,
            hover: blend(cfg.background, toward, 0.10),
            border: blend(cfg.background, toward, 0.16),
            fg: cfg.foreground,
            dim: blend(cfg.foreground, cfg.background, 0.45),
            accent: cfg.accent,
        }
    }
}

pub struct AppShell {
    /// What is in force: the file plus a theme picked in the palette.
    pub cfg: Config,
    /// The config file as last read (the defaults when it is missing or never loaded).
    pub file_cfg: Config,
    /// A theme picked in the palette. A reload that changes the file's colors drops it.
    pub theme_override: Option<String>,
    /// Every theme, built by the watcher's worker thread (None until its first list arrives).
    pub themes: Option<std::sync::Arc<forgetty_shell_core::config::ThemeIndex>>,
    pub conf_watch: Option<forgetty_shell_core::confwatch::ConfWatch>,
    /// The user asked for the reload now on the way: say so even when nothing changed.
    pub reload_asked: bool,
    /// Said once, on the first pane that exists (warnings about the file at start).
    pub startup_notice: Option<String>,
    /// The local backend, to change its default shell on a reload.
    pub local: Option<forgetty_shell_core::LocalBackend>,
    pub theme: Theme,
    pub keymap: Keymap,
    pub backend: Arc<dyn SessionBackend>,
    pub machine: MachineInfo,
    pub layout: Layout,
    pub views: HashMap<PaneId, Entity<TerminalView>>,
    pub(crate) attaching: HashSet<PaneId>,
    /// Backend messages about a pane that has no view yet; shown when the view appears.
    pending_notices: HashMap<PaneId, String>,
    /// Failed attaches per pane. A new pane can be listed a moment before its server-side
    /// byte log exists, so a failed attach retries a few times.
    attach_tries: HashMap<PaneId, u8>,
    pub meta: HashMap<PaneId, PaneMeta>,
    /// Active pane per tab (UI state; also reported to the backend).
    pub local_active: HashMap<Uuid, PaneId>,
    pub zoomed: HashMap<Uuid, PaneId>,
    pub sidebar_open: bool,
    pub palette: Option<Palette>,
    pub rename: Option<Rename>,
    pub drag: Option<Drag>,
    /// The open context menu and the question that waits for an answer.
    pub menu: Option<crate::menus::OpenMenu>,
    pub confirm: Option<crate::menus::Confirm>,
    /// The Settings window and the shortcuts window (F1), when open.
    pub settings: Option<Box<crate::settings::SettingsWin>>,
    pub shortcuts: Option<crate::settings::ShortcutsWin>,
    /// System font families, listed once.
    pub font_names: Option<Vec<String>>,
    /// The user said yes to the quit question (the window may close).
    pub quit_ok: bool,
    /// Panes whose bell or notification the user has not looked at yet.
    pub attention: HashSet<PaneId>,
    /// Tab colors. Like GTK's they live in memory only (the server has no tab color).
    pub tab_colors: HashMap<Uuid, u32>,
    pub focus: FocusHandle,
    pub metrics: Metrics,
    pub viewport: (f32, f32),
    pub spin: usize,
    pub(crate) sync_dirty: bool,
    pub window_active: bool,
    applied_title: String,
    pub(crate) had_tabs: bool,
    pub frames: u64,
    /// Frame timing probes: render start to end of paint (CPU), and end of
    /// paint to the next render start (present + wait).
    pub render_t0: Option<Instant>,
    pub last_paint_end: Option<Instant>,
    pub cpu_ms: Vec<f32>,
    pub idle_ms: Vec<f32>,
    pub smoke_mode: bool,
    pub smoke: Option<Box<ShellSmoke>>,
    pub mica: bool,
    // ---- machines (W3) ----
    /// Every machine in the sidebar, the local one first.
    pub machines: Vec<MachineEntry>,
    /// State of the machines that are not on screen. The fields above hold the
    /// active machine's state.
    pub(crate) parked: HashMap<Uuid, MachineSlot>,
    pub remote: Option<RemoteCtx>,
    pub hooks: Option<crate::winsys::Hooks>,
    pub pair: Option<PairDialog>,
    /// The clipboard text the pair dialog looked at last (so one text is judged once).
    pub pair_clip_seen: Option<String>,
    pub toast_limiter: ToastLimiter,
    /// Per-machine OSC 52 (clipboard write) policy and the prompt that waits for an answer.
    pub osc52: forgetty_shell_core::osc52::Policies,
    pub clip_prompt: Option<crate::clipboard::ClipPrompt>,
    pub clip_nonce: u64,
    /// The clipboard change counter and text right after our last OSC 52 write.
    pub osc52_clip_seq: Option<u32>,
    pub osc52_last_text: Option<String>,
    pub clip_stats: crate::clipboard::ClipStats,
    pub toast_log: Vec<ToastRecord>,
    /// "Check for updates": idle until the user asks.
    pub update: crate::update::UpdateState,
    /// While false, `apply_layout` does not attach panes (a machine that is not on screen).
    pub(crate) attach_enabled: bool,
    pub win_handle: gpui::AnyWindowHandle,
    /// Times a network change or a resume poked the remote backends.
    pub net_pokes: u64,
    /// Extra direct address for the next pairing (CI smoke, LAN). It is stored as a pinned address.
    pub pair_hint: Option<std::net::SocketAddr>,
    pub(crate) switch_to: Option<Uuid>,
    /// The local server: state, the machine it backs, the watchdog.
    pub lsrv: crate::localserver::LocalSrv,
    /// `--working-directory` / `-e`: the first tab this start opens. Taken once.
    pub launch: Option<Launch>,
}

/// What the command line asked the first tab to be.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub command: Vec<String>,
    pub cwd: Option<String>,
}

impl Launch {
    /// The folder must exist and the program must be found (PATH only, as a profile's), so a
    /// typo is an error box at once and not a tab that closes by itself.
    pub fn from_args(a: &forgetty_shell_core::cli::Args) -> Result<Option<Launch>, String> {
        if !a.has_launch() {
            return Ok(None);
        }
        if let Some(d) = &a.working_directory {
            if !std::path::Path::new(d).is_dir() {
                return Err(format!("--working-directory: {} is not a folder", forgetty_shell_core::notify::sanitize(d, 80)));
            }
        }
        let mut command = a.command.clone();
        if let Some(first) = command.first().cloned() {
            let host = forgetty_shell_core::resolve::Host::real(None);
            command[0] = forgetty_shell_core::resolve::resolve_program(&first, &host)
                .map_err(|e| format!("-e: {}", forgetty_shell_core::notify::sanitize(&e, 160)))?;
        }
        Ok(Some(Launch { command, cwd: a.working_directory.clone() }))
    }
}

pub fn basename(path: &str) -> String {
    let t = path.trim_end_matches(['\\', '/']);
    if t.is_empty() {
        return path.to_string();
    }
    let b = t.rsplit(['\\', '/']).next().unwrap_or(t);
    if b.ends_with(':') {
        return format!("{b}\\");
    }
    b.to_string()
}

fn clean_title(t: &str) -> String {
    let lower = t.to_ascii_lowercase();
    if (lower.ends_with(".exe") || lower.ends_with(".cmd")) && t.contains(['\\', '/']) {
        let b = basename(t);
        return b.rsplit_once('.').map(|x| x.0.to_string()).unwrap_or(b);
    }
    t.to_string()
}

impl AppShell {
    pub fn new(
        cfg: Config,
        backend: Arc<dyn SessionBackend>,
        smoke: Option<Box<ShellSmoke>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let metrics = Metrics::measure(cx, &cfg);
        let smoke_mode = smoke.is_some();
        let keymap = Keymap::new(cfg.keymap, &cfg.keybindings);
        for w in &keymap.warnings {
            eprintln!("forgetty-win: keybindings: {w}");
        }
        let mica = !smoke_mode && cfg.backdrop != Backdrop::None;
        let vp = window.viewport_size();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let shell = AppShell {
            file_cfg: cfg.clone(),
            theme_override: None,
            themes: None,
            conf_watch: None,
            reload_asked: false,
            startup_notice: None,
            local: None,
            theme: Theme::from_cfg(&cfg),
            sidebar_open: cfg.sidebar_open,
            keymap,
            machine: backend.machine(),
            backend: backend.clone(),
            layout: Layout::default(),
            views: HashMap::new(),
            attaching: HashSet::new(),
            pending_notices: HashMap::new(),
            attach_tries: HashMap::new(),
            meta: HashMap::new(),
            local_active: HashMap::new(),
            zoomed: HashMap::new(),
            palette: None,
            rename: None,
            drag: None,
            menu: None,
            confirm: None,
            settings: None,
            shortcuts: None,
            font_names: None,
            quit_ok: false,
            attention: HashSet::new(),
            tab_colors: HashMap::new(),
            focus,
            metrics,
            viewport: (f32::from(vp.width), f32::from(vp.height)),
            spin: 0,
            sync_dirty: true,
            window_active: true,
            applied_title: String::new(),
            had_tabs: false,
            frames: 0,
            render_t0: None,
            last_paint_end: None,
            cpu_ms: Vec::new(),
            idle_ms: Vec::new(),
            smoke_mode,
            smoke,
            mica,
            cfg,
            machines: Vec::new(),
            parked: HashMap::new(),
            remote: None,
            hooks: None,
            pair: None,
            toast_limiter: ToastLimiter::default(),
            osc52: forgetty_shell_core::osc52::Policies::memory(),
            clip_prompt: None,
            clip_nonce: 0,
            osc52_clip_seq: None,
            osc52_last_text: None,
            clip_stats: Default::default(),
            toast_log: Vec::new(),
            update: crate::update::UpdateState::Idle,
            attach_enabled: true,
            win_handle: window.window_handle(),
            net_pokes: 0,
            pair_hint: None,
            pair_clip_seen: None,
            switch_to: None,
            lsrv: Default::default(),
            launch: None,
        };
        let mut shell = shell;
        // The local machine is on screen first; its events feed the same
        // per-machine pump the remote machines use.
        shell.machines.push(MachineEntry::new(backend.clone()));
        Self::spawn_event_pump(backend.clone(), cx);

        if smoke_mode {
            // The smoke state machine advances per painted frame: keep frames coming.
            let mut n = 0u32;
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(std::time::Duration::from_millis(60)).await;
                n = n.wrapping_add(1);
                // Latency phases get only a slow heartbeat (every 600 ms) so a
                // stalled probe can still be noticed, but real frames dominate.
                if this
                    .update(cx, |s, cx| {
                        let full = s.smoke.as_ref().map_or(true, |m| m.wants_ticks());
                        if full || n % 10 == 0 {
                            cx.notify()
                        }
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        }

        // Spinner: ticks only while some pane reports "working".
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_millis(90)).await;
            let alive = this
                .update(cx, |s, cx| {
                    if s.meta.values().any(|m| m.status == PaneStatus::Working) {
                        s.spin = s.spin.wrapping_add(1);
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        // Bootstrap: take whatever the backend already has, else open one tab.
        let fut = backend.get_layout();
        cx.spawn(async move |this, cx| {
            let layout = fut.await.unwrap_or_default();
            let _ = this.update(cx, |s, cx| {
                s.apply_layout(layout, cx);
                // While the local server comes up the window waits for its tabs.
                if s.layout.tab_count() == 0 && !s.lsrv.pending() {
                    s.new_tab(cx);
                }
            });
        })
        .detach();

        shell
    }

    // ---- backend plumbing -----------------------------------------------------

    /// Run a backend future; apply its result on the UI thread. State changes
    /// arrive separately as events.
    pub fn call<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        fut: BackendFuture<T>,
        then: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        cx.spawn(async move |this, cx| match fut.await {
            Ok(v) => {
                let _ = this.update(cx, |s, cx| then(s, v, cx));
            }
            Err(e) => {
                eprintln!("forgetty-win: backend: {e}");
                // The release exe has no console: show what failed.
                let _ = this.update(cx, |s, cx| s.notice_active(format!("Failed: {e}"), cx));
            }
        })
        .detach();
    }

    /// Show `text` as a notice on the active pane, or on any pane when none is active.
    pub(crate) fn notice_active(&mut self, text: String, cx: &mut Context<Self>) {
        let view = self.active_pane().and_then(|p| self.views.get(&p).cloned()).or_else(|| self.views.values().next().cloned());
        match view {
            Some(v) => v.update(cx, |v, cx| v.show_notice(text, cx)),
            None => eprintln!("forgetty-win: {}", forgetty_shell_core::notify::notice_text(&text)),
        }
    }

    pub(crate) fn on_events(&mut self, evs: Vec<BackendEvent>, cx: &mut Context<Self>) {
        let mut layout = None;
        for ev in evs {
            match ev {
                BackendEvent::LayoutChanged(l) => layout = Some(l),
                BackendEvent::PaneClosed { pane_id } => {
                    self.meta.remove(&pane_id);
                    self.pending_notices.remove(&pane_id);
                }
                BackendEvent::TitleChanged { pane_id, title } => {
                    self.meta.entry(pane_id).or_default().title = title;
                }
                BackendEvent::CwdChanged { pane_id, cwd } => {
                    self.meta.entry(pane_id).or_default().cwd = cwd;
                }
                BackendEvent::PaneResized { pane_id, rows, cols, owner } => {
                    let m = self.meta.entry(pane_id).or_default();
                    m.size = Some((rows, cols));
                    // The event carries the owner it was caused by. Absent means "unchanged
                    // or unknown", the owner events and `get_pane_info` say the rest.
                    if owner.is_some() {
                        m.owner = owner.clone();
                    }
                    // A remote pane follows the PTY size, whoever set it (AD-020).
                    if let Some(v) = self.views.get(&pane_id) {
                        let label = m.owner.as_ref().map(|o| o.label());
                        v.update(cx, |v, cx| {
                            v.set_pty_size(rows, cols, cx);
                            v.set_size_owner(label, cx);
                        });
                    }
                }
                BackendEvent::SizeOwnerChanged { pane_id, owner } => {
                    self.meta.entry(pane_id).or_default().owner = owner.clone();
                    if let Some(v) = self.views.get(&pane_id) {
                        let label = owner.map(|o| o.label());
                        v.update(cx, |v, cx| v.set_size_owner(label, cx));
                    }
                }
                BackendEvent::InputDropped { pane_id, bytes } => {
                    if let Some(v) = self.views.get(&pane_id) {
                        let text = format!("The program did not read its input, {bytes} bytes were dropped");
                        v.update(cx, |v, cx| v.show_notice(text, cx));
                    }
                }
                BackendEvent::InputLost { bytes } => {
                    let text = format!("Server restarted, {bytes} bytes of input not sent");
                    for v in self.views.values() {
                        let text = text.clone();
                        v.update(cx, |v, cx| v.show_notice(text, cx));
                    }
                }
                BackendEvent::PaneNotice { pane_id, text } => {
                    // On the pane it is about. A pane whose view does not exist yet gets it when the
                    // view appears.
                    match self.views.get(&pane_id).cloned() {
                        Some(v) => v.update(cx, |v, cx| v.show_notice(text, cx)),
                        None => {
                            self.pending_notices.insert(pane_id, text);
                        }
                    }
                }
                BackendEvent::StatusChanged { pane_id, status, message } => {
                    let m = self.meta.entry(pane_id).or_default();
                    m.status = status;
                    m.message = message;
                }
                // Handled per machine in `machine_events`.
                BackendEvent::Notification { .. } | BackendEvent::ConnectionChanged(_) => {}
            }
        }
        if let Some(l) = layout {
            self.apply_layout(l, cx);
        }
        cx.notify();
    }

    pub fn apply_layout(&mut self, mut layout: Layout, cx: &mut Context<Self>) {
        // A divider being dragged keeps the user's ratio until release.
        if let Some(Drag::Divider { tab, divider, ratio }) = &self.drag {
            if let Some((w, t)) = layout.find_tab(*tab) {
                layout.workspaces[w].tabs[t].pane_tree.set_ratio_at(&divider.path, *ratio);
            }
        }
        let panes: HashSet<PaneId> = layout.all_panes().into_iter().collect();
        self.views.retain(|id, _| panes.contains(id));
        self.meta.retain(|id, _| panes.contains(id));
        self.attention.retain(|id| panes.contains(id));
        let tabs: HashSet<Uuid> = layout.workspaces.iter().flat_map(|w| w.tabs.iter().map(|t| t.id)).collect();
        self.local_active.retain(|id, p| tabs.contains(id) && panes.contains(p));
        self.zoomed.retain(|id, p| tabs.contains(id) && panes.contains(p));
        self.tab_colors.retain(|id, _| tabs.contains(id));
        let workspaces: HashSet<Uuid> = layout.workspaces.iter().map(|w| w.id).collect();
        if self.rename.as_ref().is_some_and(|r| match r.target {
            LabelTarget::Workspace => !workspaces.contains(&r.id),
            _ => !tabs.contains(&r.tab),
        }) {
            self.rename = None;
        }
        if layout.tab_count() > 0 {
            self.had_tabs = true;
        } else if self.had_tabs && self.machine.kind == forgetty_shell_core::MachineKind::Local {
            // The last local tab closed: the app ends. A remote machine just shows no tabs.
            cx.quit();
            return;
        } else if self.had_tabs && self.lsrv.machine == Some(self.machine.id) {
            // The same for the local server, after a look: a server that restarts can report
            // an empty layout for a moment before it restores its tabs.
            self.quit_if_still_empty(self.machine.id, cx);
        }
        self.layout = layout;
        // A menu or question about something that is gone closes.
        if self.menu.as_ref().is_some_and(|o| match &o.target {
            crate::menus::MenuTarget::Pane(p) => !panes.contains(p),
            crate::menus::MenuTarget::Tab(t) => !tabs.contains(t),
            crate::menus::MenuTarget::Workspace(id) => !workspaces.contains(id),
            crate::menus::MenuTarget::NewTab => false,
        }) {
            self.menu = None;
        }
        if let Some(crate::menus::ConfirmAct::DeleteWorkspace(id)) = self.confirm.as_ref().map(|c| &c.act) {
            if !workspaces.contains(id) {
                self.confirm = None;
            }
        }
        if self.attach_enabled {
            let ids: Vec<PaneId> = panes.into_iter().collect();
            for p in ids {
                self.attach(p, cx);
            }
        }
        self.sync_dirty = true;
        cx.notify();
    }

    pub(crate) fn attach(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.views.contains_key(&pane) || !self.attaching.insert(pane) {
            return;
        }
        let src = self.backend.attach_pane(pane);
        let info = self.backend.get_pane_info(pane);
        cx.spawn(async move |this, cx| {
            let src = src.await;
            let info = info.await.ok();
            let _ = this.update(cx, |s, cx| {
                s.attaching.remove(&pane);
                match src {
                    Ok(src) => {
                        s.attach_tries.remove(&pane);
                        s.add_view(pane, src, info, cx)
                    }
                    Err(e) => {
                        eprintln!("forgetty-win: attach {}: {e}", pane.0);
                        let tries = s.attach_tries.entry(pane).or_insert(0);
                        *tries += 1;
                        if *tries <= 25 && s.layout.all_panes().contains(&pane) {
                            s.retry_attach(pane, cx);
                        } else {
                            s.attach_tries.remove(&pane);
                        }
                    }
                }
            });
        })
        .detach();
    }

    fn retry_attach(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
            let _ = this.update(cx, |s, cx| {
                if s.attach_enabled && s.layout.all_panes().contains(&pane) {
                    s.attach(pane, cx);
                }
            });
        })
        .detach();
    }

    fn add_view(
        &mut self,
        pane: PaneId,
        src: Box<dyn PaneSource>,
        info: Option<forgetty_shell_core::PaneInfo>,
        cx: &mut Context<Self>,
    ) {
        if !self.layout.all_panes().contains(&pane) {
            src.shutdown();
            return;
        }
        let (rows, cols) = info.as_ref().map(|i| (i.rows as usize, i.cols as usize)).unwrap_or((30, 120));
        if let Some(i) = info {
            let m = self.meta.entry(pane).or_default();
            // The "starting" title is a placeholder: it is already in `meta` from its event, and
            // a stale copy must not come back after the start finished.
            if m.title.is_empty() && i.title != forgetty_shell_core::local::STARTING_TITLE {
                m.title = i.title;
            }
            if m.cwd.is_empty() {
                m.cwd = i.cwd;
            }
            if m.status == PaneStatus::None {
                m.status = i.status;
            }
            m.owner = i.size_owner;
        }
        let cfg = self.cfg.clone();
        let smoke_mode = self.smoke_mode;
        let view = cx.new(|cx| TerminalView::new(cfg, pane, src, rows, cols, smoke_mode, cx));
        let this_pc = self.is_this_pc();
        view.update(cx, |v, _| v.image_paste = this_pc);
        if let Some(o) = self.meta.get(&pane).and_then(|m| m.owner.as_ref()) {
            let label = o.label();
            view.update(cx, |v, cx| v.set_size_owner(Some(label), cx));
        }
        let machine = self.machine.id;
        cx.subscribe(&view, move |s, _v, ev: &crate::view::ViewEvent, cx| s.on_view_event(machine, ev, cx)).detach();
        if let Some(text) = self.pending_notices.remove(&pane) {
            view.update(cx, |v, cx| v.show_notice(text, cx));
        }
        self.views.insert(pane, view);
        self.sync_dirty = true;
        cx.notify();
    }

    // ---- derived state ----------------------------------------------------------

    pub fn content_rect(&self) -> Rect {
        let sb = if self.sidebar_open { SIDEBAR_W } else { 0.0 };
        Rect::new(0.0, 0.0, (self.viewport.0 - sb).max(1.0), (self.viewport.1 - TITLEBAR_H).max(1.0))
    }

    /// Window coordinates of the content area's top-left corner.
    pub fn content_origin(&self) -> (f32, f32) {
        (if self.sidebar_open { SIDEBAR_W } else { 0.0 }, TITLEBAR_H)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.layout.active_tab()
    }

    pub fn active_pane_of(&self, tab: &Tab) -> PaneId {
        match self.local_active.get(&tab.id) {
            Some(p) if tab.pane_tree.contains(*p) => *p,
            _ => tab.focused_pane(),
        }
    }

    pub fn active_pane(&self) -> Option<PaneId> {
        self.active_tab().map(|t| self.active_pane_of(t))
    }

    /// Pane rectangles of the active tab, in content coordinates. A zoomed
    /// pane fills the whole area.
    pub fn active_geometry(&self) -> Option<(Uuid, PaneGeometry)> {
        let tab = self.active_tab()?;
        let rect = self.content_rect();
        if let Some(z) = self.zoomed.get(&tab.id) {
            if tab.pane_tree.contains(*z) {
                return Some((tab.id, PaneGeometry { panes: vec![(*z, rect)], dividers: Vec::new() }));
            }
        }
        Some((tab.id, tab.pane_tree.geometry(rect, BAR)))
    }

    pub fn grid_for(&self, w: f32, h: f32) -> (u16, u16) {
        // A pane's text area is inset by the configured padding.
        let (w, h) = ((w - 2.0 * self.cfg.pane_padding_x).max(1.0), (h - 2.0 * self.cfg.pane_padding_y).max(1.0));
        let cols = (w / f32::from(self.metrics.cell_w)).floor().clamp(2.0, 1000.0) as u16;
        let rows = (h / f32::from(self.metrics.line_h)).floor().clamp(1.0, 500.0) as u16;
        (rows, cols)
    }

    /// The tab's title for display. Titles come from programs and from other clients, so
    /// control and bidi characters go and the length is capped.
    pub fn tab_title(&self, tab: &Tab) -> String {
        forgetty_shell_core::notify::sanitize(&self.tab_title_raw(tab), 80)
    }

    fn tab_title_raw(&self, tab: &Tab) -> String {
        // A label the user set wins: the tab's, then the active pane's.
        if !tab.label.is_empty() {
            return tab.label.clone();
        }
        if let Some(l) = tab.pane_label(self.active_pane_of(tab)) {
            return l.to_string();
        }
        if let Some(m) = self.meta.get(&self.active_pane_of(tab)) {
            if !m.title.is_empty() {
                return clean_title(&m.title);
            }
        }
        if !tab.title.is_empty() {
            return tab.title.clone();
        }
        "Terminal".to_string()
    }

    pub fn tab_cwd(&self, tab: &Tab) -> String {
        self.meta
            .get(&self.active_pane_of(tab))
            .map(|m| basename(&m.cwd))
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    }

    pub fn tab_status(&self, tab: &Tab) -> PaneStatus {
        tab.pane_tree
            .leaves()
            .iter()
            .filter_map(|p| self.meta.get(p).map(|m| m.status))
            .max_by_key(|s| s.rank())
            .unwrap_or(PaneStatus::None)
    }

    /// (left offset, tab width) of the tab strip for the active workspace.
    pub fn tab_strip(&self) -> (f32, f32) {
        let n = self.layout.active_workspace().map(|w| w.tabs.len()).unwrap_or(0).max(1) as f32;
        let avail = self.viewport.0 - TAB_LEFT - CAPTION_W - PLUS_W - CHEVRON_W - 48.0;
        (TAB_LEFT, (avail / n).clamp(TAB_MIN_W, TAB_MAX_W))
    }

    // ---- focus / visibility sync -----------------------------------------------

    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.views.is_empty() {
            if let Some(t) = self.startup_notice.take() {
                self.notice_active(t, cx);
            }
        }
        let visible: HashSet<PaneId> =
            self.active_geometry().map(|(_, g)| g.panes.iter().map(|(p, _)| *p).collect()).unwrap_or_default();
        let active = self.active_pane();
        let win_active = window.is_window_active() || self.smoke_mode;
        self.window_active = win_active;
        let multi = visible.len() > 1;
        // Looking at a pane answers its bell.
        if let (true, Some(a)) = (win_active, active) {
            self.attention.remove(&a);
        }
        for (id, v) in &self.views {
            let (vis, foc) = (visible.contains(id), Some(*id) == active && win_active);
            let dim = if multi && vis && Some(*id) != active { 0.28 } else { 0.0 };
            v.update(cx, |v, cx| {
                v.set_visible(vis, cx);
                v.set_focused(foc, cx);
                v.set_dim(dim, cx);
            });
        }
        let target = match active.and_then(|a| self.views.get(&a)) {
            Some(v) => v.read(cx).focus.clone(),
            None => self.focus.clone(),
        };
        if !target.is_focused(window) {
            window.defer(cx, move |window, cx| window.focus(&target, cx));
        }
    }

    pub fn request_sync(&mut self, cx: &mut Context<Self>) {
        self.sync_dirty = true;
        cx.notify();
    }

    pub fn take_sync_dirty(&mut self) -> bool {
        std::mem::take(&mut self.sync_dirty)
    }

    pub fn on_window_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync(window, cx);
        if window.is_window_active() {
            // The window is the active client again: send each visible pane's
            // size so the server's size policy (latest active client) picks it.
            for v in self.views.values() {
                v.read(cx).reassert_size();
            }
        }
        cx.notify();
    }

    // ---- actions ----------------------------------------------------------------

    /// A plain new tab: the default profile on this PC (as GTK: `default_profile`, else the first
    /// profile), else the shell. A remote machine always opens its own shell.
    pub fn new_tab(&mut self, cx: &mut Context<Self>) {
        // The first tab of a start that had `--working-directory` or `-e`.
        if let Some(l) = self.launch.take() {
            self.new_tab_with(l.command, l.cwd, cx);
            return;
        }
        let profile = self.cfg.default_profile().cloned().filter(|_| self.is_this_pc());
        match profile {
            Some(p) => self.start_profile_tab(&p, cx),
            None => self.new_tab_with(Vec::new(), None, cx),
        }
    }

    /// Profile `n` (1-based) in a new tab: Ctrl+Shift+1..9 and the new-tab menu.
    pub fn open_profile(&mut self, n: usize, cx: &mut Context<Self>) {
        let Some(p) = n.checked_sub(1).and_then(|i| self.cfg.profiles.get(i)).cloned() else {
            self.notice_active(format!("There is no profile {n}"), cx);
            return;
        };
        // A profile is a program on this PC. A remote machine has its own shells.
        if !self.is_this_pc() {
            self.notice_active("Profiles start programs on this PC. Switch to this PC to use one.".into(), cx);
            return;
        }
        self.start_profile_tab(&p, cx);
    }

    fn start_profile_tab(&mut self, p: &forgetty_shell_core::config::Profile, cx: &mut Context<Self>) {
        // The directory was expanded and checked when the file was read: no disk access here.
        if let Some(why) = &p.dir_problem {
            self.notice_active(format!("Profile {}: directory {why}, started in the home directory", p.name), cx);
        }
        self.new_tab_with(p.argv.clone(), p.start_dir.clone(), cx);
    }

    pub(crate) fn new_tab_with(&mut self, command: Vec<String>, cwd: Option<String>, cx: &mut Context<Self>) {
        let r = self.content_rect();
        let (rows, cols) = self.grid_for(r.w, r.h);
        // With no cwd the backend decides (it never trusts escape-code paths).
        let req = NewTab { workspace_idx: self.layout.active_workspace, rows, cols, command, cwd, ..Default::default() };
        let fut = self.backend.new_tab(req);
        self.call(cx, fut, |s, reply, cx| {
            s.local_active.insert(reply.tab_id, reply.pane_id);
            cx.notify();
        });
    }

    pub fn close_tab(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let fut = self.backend.close_tab(tab);
        self.call(cx, fut, |_, _, _| {});
    }

    pub fn focus_tab(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let fut = self.backend.focus_tab(tab);
        self.call(cx, fut, |_, _, _| {});
    }

    fn tab_ids(&self) -> Vec<Uuid> {
        self.layout.active_workspace().map(|w| w.tabs.iter().map(|t| t.id).collect()).unwrap_or_default()
    }

    pub fn cycle_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids = self.tab_ids();
        let Some(ws) = self.layout.active_workspace() else { return };
        if ids.len() < 2 {
            return;
        }
        let n = ids.len() as isize;
        let to = ((ws.active_tab as isize + delta) % n + n) % n;
        self.focus_tab(ids[to as usize], cx);
    }

    pub fn goto_tab(&mut self, n: u8, cx: &mut Context<Self>) {
        let ids = self.tab_ids();
        if ids.is_empty() {
            return;
        }
        let i = if n >= 9 { ids.len() - 1 } else { (n as usize - 1).min(ids.len() - 1) };
        self.focus_tab(ids[i], cx);
    }

    pub fn move_tab_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(id) = self.active_tab().map(|t| t.id) {
            self.move_tab_id(id, delta, cx);
        }
    }

    pub fn split(&mut self, dir: SplitDir, cx: &mut Context<Self>) {
        self.split_placed(dir, false, cx);
    }

    /// Split left (`Horizontal`) or up (`Vertical`): the new pane goes first. A machine whose
    /// server cannot do that says so and splits nothing.
    pub fn split_before(&mut self, dir: SplitDir, cx: &mut Context<Self>) {
        self.split_placed(dir, true, cx);
    }

    fn split_placed(&mut self, dir: SplitDir, before: bool, cx: &mut Context<Self>) {
        if before && !self.backend.supports_split_before() {
            // Not knowing is not the same as "too old": a machine that was never reached
            // in this run has told us nothing yet.
            let text = if self.backend.connection().is_connected() {
                "This machine's server is too old to split left or up. Update it to use this."
            } else {
                "Not connected to this machine yet, so split left or up is not known to work. Try again when it is connected."
            };
            self.notice_active(text.into(), cx);
            return;
        }
        let (Some(tab), Some(pane)) = (self.active_tab().map(|t| t.id), self.active_pane()) else { return };
        let rect = self
            .active_geometry()
            .and_then(|(_, g)| g.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r))
            .unwrap_or_else(|| self.content_rect());
        self.split_pane_in(tab, pane, rect, dir, before, cx);
    }

    /// Split `pane` of `tab`, which fills `rect` now. The new pane gets the focus.
    pub(crate) fn split_pane_in(&mut self, tab: Uuid, pane: PaneId, rect: Rect, dir: SplitDir, before: bool, cx: &mut Context<Self>) {
        let (w, h) = match dir {
            SplitDir::Horizontal => (rect.w / 2.0, rect.h),
            SplitDir::Vertical => (rect.w, rect.h / 2.0),
        };
        let (rows, cols) = self.grid_for(w, h);
        // The backend starts the new pane where the parent was started.
        let fut = self.backend.split_pane(SplitPane { pane_id: pane, direction: dir, rows, cols, cwd: None, before });
        // Splitting un-zooms: the new pane must be visible.
        self.zoomed.remove(&tab);
        self.call(cx, fut, move |s, new_pane, cx| {
            s.local_active.insert(tab, new_pane);
            // The new pane has the focus: tell the server, so a restart restores it.
            let focus = s.backend.set_active_pane(tab, Some(new_pane));
            s.call(cx, focus, |_, _, _| {});
            s.request_sync(cx);
        });
    }

    pub fn close_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.active_pane() {
            let fut = self.backend.close_pane(p);
            self.call(cx, fut, |_, _, _| {});
        }
    }

    pub fn focus_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.layout.find_pane(pane).and_then(|(w, t)| self.layout.workspaces.get(w).and_then(|ws| ws.tabs.get(t))) else {
            return;
        };
        let tab_id = tab.id;
        if self.local_active.get(&tab_id) == Some(&pane) && self.active_pane() == Some(pane) {
            return;
        }
        self.local_active.insert(tab_id, pane);
        let fut = self.backend.set_active_pane(tab_id, Some(pane));
        self.call(cx, fut, |_, _, _| {});
        self.sync(window, cx);
        cx.notify();
    }

    pub fn focus_side(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((_, geo)), Some(from)) = (self.active_geometry(), self.active_pane()) else { return };
        if let Some(to) = neighbor(&geo.panes, from, side) {
            self.focus_pane(to, window, cx);
        }
    }

    pub fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        let (Some(tab), Some(pane)) = (self.active_tab().map(|t| t.id), self.active_pane()) else { return };
        if self.zoomed.remove(&tab).is_none() {
            if self.active_tab().is_some_and(|t| t.pane_tree.pane_count() > 1) {
                self.zoomed.insert(tab, pane);
            }
        }
        self.request_sync(cx);
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        self.request_sync(cx);
    }

    pub fn new_workspace(&mut self, cx: &mut Context<Self>) {
        let r = self.content_rect();
        let (rows, cols) = self.grid_for(r.w, r.h);
        let fut = self.backend.create_workspace(None, rows, cols);
        self.call(cx, fut, |s, reply, cx| {
            s.local_active.insert(reply.tab_id, reply.pane_id);
            cx.notify();
        });
    }

    pub fn cycle_workspace(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.layout.workspaces.len() as isize;
        if n < 2 {
            return;
        }
        let to = ((self.layout.active_workspace as isize + delta) % n + n) % n;
        let fut = self.backend.set_active_workspace(to as usize);
        self.call(cx, fut, |_, _, _| {});
    }

    pub fn set_workspace(&mut self, idx: usize, cx: &mut Context<Self>) {
        let fut = self.backend.set_active_workspace(idx);
        self.call(cx, fut, |_, _, _| {});
    }

    /// Ask GitHub Releases whether a newer Windows build exists. Runs only from
    /// the user's click or the palette; nothing is downloaded or installed.
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if self.update == crate::update::UpdateState::Checking {
            return;
        }
        self.update = crate::update::UpdateState::Checking;
        self.sidebar_open = true;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(crate::update::check(env!("CARGO_PKG_VERSION")));
        });
        cx.spawn(async move |this, cx| {
            let st = rx.recv().await.unwrap_or_else(|_| crate::update::UpdateState::Failed("the check stopped".into()));
            let _ = this.update(cx, |s, cx| {
                s.update = st;
                s.request_sync(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The sidebar line was clicked: a found update opens its page, else check again.
    pub fn update_line_clicked(&mut self, cx: &mut Context<Self>) {
        match &self.update {
            crate::update::UpdateState::Available { url, .. } => {
                let _ = crate::update::open_release_page(url);
            }
            _ => self.check_for_updates(cx),
        }
    }

    pub fn begin_rename(&mut self, tab: Uuid, cx: &mut Context<Self>) {
        let Some((w, t)) = self.layout.find_tab(tab) else { return };
        let current = self.tab_title(&self.layout.workspaces[w].tabs[t]);
        self.rename = Some(Rename { target: LabelTarget::Tab, id: tab, tab, edit: LineEdit::new(&current) });
        cx.notify();
    }

    /// Name the focused pane. The editor shows in its tab.
    pub fn begin_rename_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(tab), Some(pane)) = (self.active_tab(), self.active_pane()) else { return };
        let current = tab.pane_label(pane).unwrap_or_default().to_string();
        self.rename = Some(Rename { target: LabelTarget::Pane, id: pane.0, tab: tab.id, edit: LineEdit::new(&current) });
        cx.notify();
    }

    /// Rename a workspace in the sidebar.
    pub fn begin_rename_workspace(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(ws) = self.layout.workspaces.get(idx) else { return };
        self.rename = Some(Rename { target: LabelTarget::Workspace, id: ws.id, tab: Uuid::nil(), edit: LineEdit::new(&ws.name) });
        cx.notify();
    }

    pub fn run_action(&mut self, a: Action, window: &mut Window, cx: &mut Context<Self>) {
        match a {
            Action::NewTab => self.new_tab(cx),
            Action::CloseTab => {
                if let Some(t) = self.active_tab().map(|t| t.id) {
                    self.close_tab(t, cx);
                }
            }
            Action::NextTab => self.cycle_tab(1, cx),
            Action::PrevTab => self.cycle_tab(-1, cx),
            Action::GotoTab(n) => self.goto_tab(n, cx),
            Action::MoveTabLeft => self.move_tab_by(-1, cx),
            Action::MoveTabRight => self.move_tab_by(1, cx),
            Action::RenameTab => {
                if let Some(t) = self.active_tab().map(|t| t.id) {
                    self.begin_rename(t, cx);
                }
            }
            Action::CheckUpdates => self.check_for_updates(cx),
            Action::EndSession => self.ask_end_session(cx),
            Action::ClipboardWriteOff => self.set_active_clip_policy(forgetty_shell_core::osc52::Policy::Off, cx),
            Action::ClipboardWriteAsk => self.set_active_clip_policy(forgetty_shell_core::osc52::Policy::Ask, cx),
            Action::ClipboardWriteAllow => self.set_active_clip_policy(forgetty_shell_core::osc52::Policy::Allow, cx),
            Action::RenamePane => self.begin_rename_pane(cx),
            Action::RenameWorkspace => self.begin_rename_workspace(self.layout.active_workspace, cx),
            Action::SplitRight => self.split(SplitDir::Horizontal, cx),
            Action::SplitDown => self.split(SplitDir::Vertical, cx),
            Action::SplitLeft => self.split_before(SplitDir::Horizontal, cx),
            Action::SplitUp => self.split_before(SplitDir::Vertical, cx),
            Action::ClosePane => self.close_pane(cx),
            Action::FocusLeft => self.focus_side(Side::Left, window, cx),
            Action::FocusRight => self.focus_side(Side::Right, window, cx),
            Action::FocusUp => self.focus_side(Side::Up, window, cx),
            Action::FocusDown => self.focus_side(Side::Down, window, cx),
            Action::ZoomPane => self.toggle_zoom(cx),
            Action::ToggleSidebar => self.toggle_sidebar(cx),
            Action::CommandPalette => self.toggle_palette(cx),
            Action::NewWorkspace => self.new_workspace(cx),
            Action::NextWorkspace => self.cycle_workspace(1, cx),
            Action::PrevWorkspace => self.cycle_workspace(-1, cx),
            Action::PairMachine => self.open_pair(cx),
            Action::NextMachine => self.cycle_machine(1, cx),
            Action::PrevMachine => self.cycle_machine(-1, cx),
            Action::Search => self.on_active_view(window, cx, |v, cx| v.toggle_search(cx)),
            Action::SelectAll => self.on_active_view(window, cx, |v, cx| v.select_all(cx)),
            Action::CopyRaw => self.on_active_view(window, cx, |v, cx| v.copy_selection_as(false, cx)),
            Action::ClearTerminal => self.on_active_view(window, cx, |v, cx| v.clear_terminal(cx)),
            Action::ResetTerminal => self.on_active_view(window, cx, |v, cx| v.reset_terminal(cx)),
            Action::ResizeLeft => self.resize_pane_key(Side::Left, cx),
            Action::ResizeRight => self.resize_pane_key(Side::Right, cx),
            Action::ResizeUp => self.resize_pane_key(Side::Up, cx),
            Action::ResizeDown => self.resize_pane_key(Side::Down, cx),
            Action::DuplicateTab => {
                if let Some(t) = self.active_tab().map(|t| t.id) {
                    self.duplicate_tab(t, cx);
                }
            }
            Action::CloseTabsRight => {
                if let Some(t) = self.active_tab().map(|t| t.id) {
                    self.close_tabs_right(t, cx);
                }
            }
            Action::CloseOtherTabs => {
                if let Some(t) = self.active_tab().map(|t| t.id) {
                    self.close_other_tabs(t, cx);
                }
            }
            Action::DuplicateWorkspace => self.duplicate_workspace(self.layout.active_workspace, cx),
            Action::DeleteWorkspace => self.delete_workspace(self.layout.active_workspace, cx),
            Action::MoveWorkspaceUp => self.move_workspace(self.layout.active_workspace, true, cx),
            Action::MoveWorkspaceDown => self.move_workspace(self.layout.active_workspace, false, cx),
            Action::PaneMenu => self.open_active_pane_menu(cx),
            Action::ZoomIn => self.on_active_view(window, cx, |v, cx| v.zoom(forgetty_shell_core::config::Zoom::In, cx)),
            Action::ZoomOut => self.on_active_view(window, cx, |v, cx| v.zoom(forgetty_shell_core::config::Zoom::Out, cx)),
            Action::ZoomReset => self.on_active_view(window, cx, |v, cx| v.zoom(forgetty_shell_core::config::Zoom::Reset, cx)),
            Action::Profile(n) => self.open_profile(usize::from(n), cx),
            Action::OpenConfig => self.open_config_file(cx),
            Action::ReloadConfig => self.reload_config_now(cx),
            Action::ChooseTheme => self.open_theme_list(cx),
            Action::GotoWorkspace(n) => {
                let i = usize::from(n).saturating_sub(1);
                if i < self.layout.workspaces.len() && i != self.layout.active_workspace {
                    self.set_workspace(i, cx);
                }
            }
            Action::Quit => self.request_quit(cx),
            Action::OpenSettings => self.open_settings(None, cx),
            Action::ShowShortcuts => {
                if self.shortcuts.is_some() {
                    self.close_shortcuts(window, cx);
                } else {
                    self.open_shortcuts(cx);
                }
            }
            Action::PasteCtrlV => self.on_active_view(window, cx, |v, cx| {
                v.key_paste(cx);
            }),
            Action::CopyOrInterrupt => self.on_active_view(window, cx, |v, cx| {
                v.key_copy(cx);
            }),
        }
        cx.notify();
    }

    /// Run `f` on the active pane's view and give it the keyboard focus (a command from the
    /// palette or a chord acts on the pane the user is working in).
    pub(crate) fn on_active_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>),
    ) {
        let Some(view) = self.active_pane().and_then(|p| self.views.get(&p)).cloned() else { return };
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        view.update(cx, |v, cx| f(v, cx));
    }

    // ---- palette ------------------------------------------------------------------

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.palette = None;
        } else {
            self.rename = None;
            self.menu = None;
            let mut p = Palette { edit: LineEdit::default(), selected: 0, items: Vec::new(), themes: None };
            self.refresh_palette(&mut p);
            self.palette = Some(p);
        }
        cx.notify();
    }

    /// The list of themes in the palette. Choosing one applies it to this session.
    pub fn open_theme_list(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.themes.clone() else {
            self.notice_active("The theme list is still loading".into(), cx);
            return;
        };
        self.rename = None;
        self.menu = None;
        let mut p = Palette { edit: LineEdit::default(), selected: 0, items: Vec::new(), themes: Some(index.names.clone()) };
        self.refresh_palette(&mut p);
        self.palette = Some(p);
        cx.notify();
    }

    fn refresh_palette(&self, p: &mut Palette) {
        let mut scored: Vec<(i32, usize, PaletteItem)> = Vec::new();
        if let Some(names) = &p.themes {
            for (order, name) in names.iter().enumerate() {
                if let Some((score, hits)) = fuzzy::score(&p.edit.text, name) {
                    let current = self.cfg.theme_name.as_deref() == Some(name.as_str())
                        || self.theme_override.as_deref() == Some(name.as_str());
                    scored.push((
                        score,
                        order,
                        PaletteItem {
                            pick: Pick::Theme(name.clone()),
                            title: name.clone(),
                            shortcut: if current { "current".into() } else { String::new() },
                            hits,
                        },
                    ));
                }
            }
        } else {
            for (order, info) in ACTIONS.iter().enumerate() {
                // "Open profile N" is listed for the profiles that exist, by name.
                let title = match info.action {
                    Action::Profile(n) => match self.cfg.profiles.get(usize::from(n) - 1) {
                        Some(pr) => format!("Open profile: {}", forgetty_shell_core::notify::sanitize(&pr.name, 40)),
                        None => continue,
                    },
                    // "Go to workspace N" is listed for the workspaces that exist, by name.
                    Action::GotoWorkspace(n) => match self.layout.workspaces.get(usize::from(n) - 1) {
                        Some(w) => format!("Go to workspace {n}: {}", forgetty_shell_core::notify::sanitize(&w.name, 40)),
                        None => continue,
                    },
                    // The Ctrl+V and Ctrl+C keys have their own menu rows.
                    Action::PasteCtrlV | Action::CopyOrInterrupt => continue,
                    _ => info.title.to_string(),
                };
                if let Some((score, hits)) = fuzzy::score(&p.edit.text, &title) {
                    scored.push((
                        score,
                        order,
                        PaletteItem {
                            pick: Pick::Action(info.action),
                            title,
                            shortcut: self.keymap.shortcut(info.action),
                            hits,
                        },
                    ));
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        p.items = scored.into_iter().map(|x| x.2).collect();
        p.selected = 0;
    }

    // ---- keyboard -----------------------------------------------------------------

    pub fn chord_of(ks: &Keystroke) -> Chord {
        let m = &ks.modifiers;
        Chord::new(m.control, m.alt, m.shift, m.platform, &ks.key)
    }

    /// Returns true when the shell consumed the key (the terminal must not see it).
    pub fn handle_key(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let chord = Self::chord_of(ks);
        // A question the shell asks (quit, delete) comes before every other overlay: its answer
        // keys must not reach a dialog behind it.
        if self.confirm.is_some() {
            self.confirm_key(ks, cx);
            return true;
        }
        if self.pair.is_some() {
            self.pair_key(ks, cx);
            return true;
        }
        if self.shortcuts.is_some() {
            self.shortcuts_key(ks, window, cx);
            return true;
        }
        if self.settings.is_some() {
            self.settings_key(ks, window, cx);
            return true;
        }
        if self.menu.is_some() {
            self.menu_key(ks, window, cx);
            return true;
        }
        if self.palette.is_some() {
            self.palette_key(ks, &chord, window, cx);
            return true;
        }
        if self.rename.is_some() {
            self.rename_key(ks, cx);
            return true;
        }
        if let Some(a) = self.keymap.lookup(&chord) {
            // These keys are the program's when the shell has nothing to do with them: Alt+N
            // with no workspace N, Ctrl+V while a paste question waits, Ctrl+C with nothing
            // selected on screen (the interrupt).
            let mine = match a {
                Action::GotoWorkspace(n) => usize::from(n) <= self.layout.workspaces.len(),
                Action::PasteCtrlV | Action::CopyOrInterrupt => {
                    let Some(view) = self.active_pane().and_then(|p| self.views.get(&p)).cloned() else { return false };
                    let v = view.read(cx);
                    if matches!(a, Action::PasteCtrlV) { v.takes_key_paste() } else { v.takes_key_copy() }
                }
                _ => true,
            };
            if !mine {
                return false;
            }
            self.run_action(a, window, cx);
            return true;
        }
        false
    }

    /// Quit (Ctrl+Shift+Q, the window's close button): at once, or after a question while a local
    /// shell is open. Only remote panes open: no question.
    pub fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.close_allowed(cx) {
            cx.quit();
        }
    }

    /// May the window close now? With a local shell open it asks, and the answer is no until the
    /// user says yes.
    pub fn close_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        let n = self.local.as_ref().map_or(0, |l| l.live_panes());
        if n == 0 || self.quit_ok {
            return true;
        }
        self.menu = None;
        self.palette = None;
        self.rename = None;
        // The pair dialog goes first: Enter on the question must not pair a machine.
        self.pair = None;
        self.confirm = Some(crate::menus::Confirm { text: forgetty_shell_core::ops::quit_text(n), act: crate::menus::ConfirmAct::Quit });
        cx.notify();
        false
    }

    fn palette_key(&mut self, ks: &Keystroke, chord: &Chord, window: &mut Window, cx: &mut Context<Self>) {
        let toggles = self.keymap.lookup(chord) == Some(Action::CommandPalette);
        if toggles || ks.key == "escape" {
            self.palette = None;
            cx.notify();
            return;
        }
        let Some(mut p) = self.palette.take() else { return };
        let n = p.items.len();
        match ks.key.as_str() {
            "enter" => {
                let pick = p.items.get(p.selected).map(|i| i.pick.clone());
                cx.notify();
                match pick {
                    Some(Pick::Action(a)) => self.run_action(a, window, cx),
                    Some(Pick::Theme(name)) => self.pick_theme(&name, cx),
                    None => {}
                }
                return;
            }
            "down" if n > 0 => p.selected = (p.selected + 1) % n,
            "up" if n > 0 => p.selected = (p.selected + n - 1) % n,
            _ => {
                if self.palette_edit(&mut p, ks) {
                    // refreshed inside
                }
            }
        }
        self.palette = Some(p);
        cx.notify();
    }

    fn palette_edit(&self, p: &mut Palette, ks: &Keystroke) -> bool {
        if p.edit.key(ks) == EditResult::Edited {
            let old = p.edit.cursor;
            let mut tmp = Palette { edit: p.edit.clone(), selected: 0, items: Vec::new(), themes: p.themes.take() };
            self.refresh_palette(&mut tmp);
            p.items = tmp.items;
            p.themes = tmp.themes;
            p.selected = 0;
            p.edit.cursor = old;
            return true;
        }
        false
    }

    fn rename_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let Some(r) = self.rename.as_mut() else { return };
        match ks.key.as_str() {
            "escape" => self.rename = None,
            "enter" => {
                let (target, id, text) = (r.target, r.id, r.edit.text.trim().to_string());
                self.rename = None;
                // A workspace keeps its name when the text is empty.
                if !(target == LabelTarget::Workspace && text.is_empty()) {
                    let fut = self.backend.set_label(target, id, text);
                    self.call(cx, fut, |_, _, _| {});
                }
            }
            _ => {
                r.edit.key(ks);
            }
        }
        cx.notify();
    }

    // ---- mouse --------------------------------------------------------------------

    pub fn on_tab_mouse_down(&mut self, id: Uuid, index: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Right {
            self.open_tab_menu(id, (f32::from(ev.position.x), f32::from(ev.position.y)), false, cx);
            return;
        }
        if ev.button != MouseButton::Left {
            return;
        }
        if ev.click_count >= 2 {
            self.begin_rename(id, cx);
            return;
        }
        self.rename = None;
        self.drag = Some(Drag::Tab { id, index });
        self.focus_tab(id, cx);
        cx.notify();
    }

    pub fn begin_divider_drag(&mut self, tab: Uuid, divider: Divider, cx: &mut Context<Self>) {
        let ratio = divider.ratio;
        self.drag = Some(Drag::Divider { tab, divider, ratio });
        cx.notify();
    }

    /// Move the dragged divider to a pointer position in window coordinates.
    pub fn drag_to(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let (ox, oy) = self.content_origin();
        match &mut self.drag {
            Some(Drag::Divider { tab, divider, ratio }) => {
                let min = match divider.dir {
                    SplitDir::Horizontal => MIN_PANE_W,
                    SplitDir::Vertical => MIN_PANE_H,
                };
                let r = divider.ratio_for(x - ox, y - oy, BAR, min);
                if (r - *ratio).abs() > f32::EPSILON {
                    *ratio = r;
                    let (tab, path) = (*tab, divider.path.clone());
                    if let Some((w, t)) = self.layout.find_tab(tab) {
                        self.layout.workspaces[w].tabs[t].pane_tree.set_ratio_at(&path, r);
                    }
                    cx.notify();
                }
            }
            Some(Drag::Tab { id, index }) => {
                let (left, tw) = {
                    let n = self.layout.active_workspace().map(|w| w.tabs.len()).unwrap_or(0).max(1) as f32;
                    let avail = self.viewport.0 - TAB_LEFT - CAPTION_W - PLUS_W - CHEVRON_W - 48.0;
                    (TAB_LEFT, (avail / n).clamp(TAB_MIN_W, TAB_MAX_W))
                };
                let count = self.layout.active_workspace().map(|w| w.tabs.len()).unwrap_or(1);
                let to = (((x - left) / tw).floor().max(0.0) as usize).min(count.saturating_sub(1));
                if to != *index {
                    let id = *id;
                    *index = to;
                    // Reorder locally at once, then tell the backend.
                    let aw = self.layout.active_workspace;
                    if let Some(ws) = self.layout.workspaces.get_mut(aw) {
                        if let Some(from) = ws.tabs.iter().position(|t| t.id == id) {
                            let tab = ws.tabs.remove(from);
                            ws.tabs.insert(to.min(ws.tabs.len()), tab);
                            ws.active_tab = to.min(ws.tabs.len().saturating_sub(1));
                        }
                    }
                    let fut = self.backend.move_tab(id, to);
                    self.call(cx, fut, |_, _, _| {});
                    cx.notify();
                }
            }
            None => {}
        }
    }

    /// Finish a drag. A divider drag persists its ratio through the backend.
    pub fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.drag.take() {
            if let Drag::Divider { divider, ratio, .. } = d {
                let fut = self
                    .backend
                    .update_split_ratios(vec![SplitRatio { pane_id: divider.first_leaf, ratio }]);
                self.call(cx, fut, |_, _, _| {});
            }
            cx.notify();
        }
    }

    pub fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag(cx);
            return;
        }
        self.drag_to(f32::from(ev.position.x), f32::from(ev.position.y), cx);
    }

    pub fn on_mouse_up(&mut self, _ev: &MouseUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        self.end_drag(cx);
    }

    // ---- after paint -----------------------------------------------------------------

    pub fn window_title(&self) -> String {
        let tab = self.active_tab().map(|t| self.tab_title(t));
        let ws = self.layout.active_workspace().map(|w| forgetty_shell_core::notify::sanitize(&w.name, 60)).unwrap_or_default();
        forgetty_shell_core::ops::window_title(self.layout.workspaces.len(), &ws, tab.as_deref())
    }

    /// Called at the end of every painted frame.
    pub fn after_paint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.frames += 1;
        let now = Instant::now();
        if self.smoke_mode {
            if let Some(t0) = self.render_t0.take() {
                if self.cpu_ms.len() < 20_000 {
                    self.cpu_ms.push(now.duration_since(t0).as_secs_f32() * 1000.0);
                }
            }
            self.last_paint_end = Some(now);
        }
        let title = self.window_title();
        if title != self.applied_title {
            window.set_window_title(&title);
            self.applied_title = title;
        }
        if let Some(mut sm) = self.smoke.take() {
            sm.step(self, window, cx);
            self.smoke = Some(sm);
        }
    }
}

impl Focusable for AppShell {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}
