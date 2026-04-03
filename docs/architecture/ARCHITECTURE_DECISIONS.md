# Forgetty — Locked Architectural Decisions

> **This file is the canonical source of truth for all locked architectural decisions.**
> Every agent, every session, every phase MUST read this file before writing any code or spec.
> Decisions here override anything in CLAUDE.md, BACKLOG.md, or older docs if they conflict.

---

## AD-001: One Daemon Per Window (not per machine)

**Decision:** Each terminal window spawns its own independent daemon process.

**What this means:**
- Window 1 → Daemon 1 → its own socket, its own session file, its own workspaces/tabs/panes
- Window 2 → Daemon 2 → fully independent, no shared state with Daemon 1
- Opening a new terminal window = spawning a new daemon
- Two windows never see each other's tabs or layout changes

**Identifiers:**
- Each daemon socket: `/tmp/forgetty-{uuid}.sock` (or `$XDG_RUNTIME_DIR/forgetty-{uuid}.sock`)
- Each daemon session file: `~/.config/forgetty/sessions/{uuid}.json`

**Session restore on launch:** Forgetty reads all `~/.config/forgetty/sessions/*.json` files and restores one daemon + window per file.

**What this kills:** The "two GTK windows see each other's changes via subscribe_layout" AC in T-065 is dropped. Each daemon is single-tenant.

**Decided:** 2026-04-03

---

## AD-002: Session Hierarchy (per daemon)

**Decision:** Each daemon owns this exact hierarchy:

```
Daemon
└── Workspaces (1..N)
    └── Tabs (1..N per workspace)
        └── Panes / Splits (1..N per tab, tree structure)
            └── PTY + VT instance (leaf node)
```

**What this means for T-059:** `SessionLayout` must model workspaces as a first-class concept from day one. A "tab" always belongs to a workspace. A "pane" always belongs to a tab.

**Decided:** 2026-04-03

---

## AD-003: Session Ownership = Device That Owns the PTY

**Decision:** A session is always owned by the device running the shell. Renderers (GTK, Android, Web) own nothing — they render what the daemon streams.

**What this means:**
- PC session: shell runs on PC, files on PC, PTY on PC daemon — PC is the owner
- Android local session: shell runs on Android, files on Android, PTY on Android daemon — Android is the owner
- Android viewing a PC session: PTY still owned by PC daemon — Android is just a renderer
- Ownership never transfers between devices

**Decided:** 2026-04-03

---

## AD-004: Android Pairing Is Asymmetric (Like SSH)

**Decision:** Pairing gives Android bidirectional I/O to a PC session, but the PC daemon remains the sole owner.

**What this means:**
- Android can type and see output (bidirectional I/O)
- Shell, filesystem, processes, and PTY all live on the PC — never move to Android
- Disconnecting Android has zero effect on the PC session (it keeps running)
- Reconnecting Android gets a fresh snapshot of the current state
- Conceptually identical to SSH, but using iroh instead of SSH daemon

**Decided:** 2026-04-03

---

## AD-005: Android Pairing Rules

**Decision:** Android can operate in two modes, but is NEVER a pairing host.

**Android modes:**
1. **Local terminal** — Android daemon owns PTYs (like Termux). Shell runs on Android device.
2. **Paired to PC** — Android is a remote client. Connects to a PC daemon via iroh. Views/controls PC panes.

Both modes can coexist in the same Android window as different tabs.

**Hard rules:**
- Android NEVER accepts incoming iroh connections from other devices
- Android-to-Android pairing: NOT ALLOWED
- Only PC (desktop/server) daemons run an iroh listener and accept incoming client connections
- iroh listener code is desktop-only. Android build only includes iroh client.

**Why no Android-to-Android:** Android background process limits make a persistent inbound server impractical. The use case doesn't exist.

**Decided:** 2026-04-03

---

## AD-006: Daemon Is Single-Tenant

**Decision:** Each daemon serves exactly one GTK window (its owner). No multi-client routing needed for basic operation.

**What this means for implementation:**
- `SessionLayout` in T-059 does NOT need workspace-scoped tenant isolation
- `subscribe_layout` (T-063) is useful for future Android remote-view, but not for multi-window sync
- Socket handlers do not need to route by client identity
- This simplifies SessionManager significantly

**Exception:** Android pairing (T-052–T-054) opens a second connection to the daemon via iroh. The daemon must handle one GTK socket client + one Android iroh client simultaneously for the same session. But this is "one window + one phone", not "two windows".

**Decided:** 2026-04-03

---

## AD-007: libghostty-vt Is Per-Pane Only

**Decision:** libghostty-vt is a per-pane VT engine. It has no concept of tabs, splits, or layout. We own the layout layer entirely.

**What libghostty-vt owns (per pane):**
- VT parser (ANSI/Kitty sequences)
- Screen buffer (cells, colors, attributes)
- Scrollback
- Cursor position

**What we own (our Rust code):**
- `forgetty-session::SessionLayout` — workspaces → tabs → pane tree
- `forgetty-session::SessionManager` — flat `HashMap<PaneId, PaneState>` + layout on top
- `forgetty-socket` — RPCs to read/mutate layout and panes
- `forgetty-gtk` — GTK widget tree mirrors the layout, never owns state

No changes to libghostty-vt needed for any M4 task (T-059–T-065).

**Decided:** 2026-04-03 (confirmed from prior sessions)

---

## AD-008: GTK Is a Stateless Renderer (Post M4)

**Decision:** After T-065, GTK never writes session files, never owns PTYs, never makes layout decisions unilaterally.

**GTK's role:**
- Connects to daemon socket on startup
- Calls `get_layout` → builds widget tree from response
- User actions (new tab, split, close) → send RPC → daemon mutates state → GTK updates widget
- Subscribes to layout events → reflects changes from other clients (Android pairing)
- UI-only state stays in GTK: scroll position, text selection, search highlight, sidebar open/closed

**What GTK does NOT do (post M4):**
- Write `default.json` or any session file (daemon writes it)
- Spawn PTYs directly (daemon spawns them via `new_tab`/`split_pane` RPCs)
- Store tab/pane hierarchy as source of truth (daemon's `SessionLayout` is the truth)

**Decided:** 2026-04-01 (daemon-first), reconfirmed 2026-04-03

---

## Superseded Decisions

The following statements in older docs are **incorrect** and overridden by this file:

| Old statement | Where | Correct statement |
|---|---|---|
| "Multiple GTK windows attach to the same daemon simultaneously" | DAEMON_SYNC_ARCHITECTURE.md | Each window has its own daemon (AD-001) |
| "Two GTK windows see each other's changes via subscribe_layout" | BACKLOG.md T-065 AC | Dropped — each daemon is single-tenant (AD-006) |
