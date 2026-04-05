# Forgetty — Architecture Implementation Status

> **Auto-maintained.** The builder agent updates this file at the end of every task that touches
> an architectural decision. Do NOT edit manually unless correcting an error.
>
> Source of truth for architectural decisions: `docs/architecture/ARCHITECTURE_DECISIONS.md`
> and `docs/architecture/DAEMON_SYNC_ARCHITECTURE.md`.
>
> **Legend:** ✅ Implemented · 🟡 Partial · ❌ Not implemented · ⏸ On hold (deferred milestone)

---

## Architectural Decisions (from ARCHITECTURE_DECISIONS.md)

| AD | Decision | Required | Status | Implemented in | Gap / Notes |
|----|----------|----------|--------|---------------|-------------|
| **AD-001** | One daemon per window — UUID socket + UUID session file | Each window has own daemon; socket `forgetty-{uuid}.sock`; session `sessions/{uuid}.json`; restore all windows on launch | ✅ IMPLEMENTED | T-068 | UUID socket + session file per daemon. GTK generates or restores session UUID. `--restore-all` spawns one window per saved session. |
| **AD-002** | Session hierarchy: Workspaces 1..N → Tabs → Panes | `SessionManager::create_workspace()`; `create_workspace` RPC; multi-workspace cold-start restore | ✅ IMPLEMENTED | T-059–T-065, T-067 | Data model, mutation API, RPC, and cold-start restore all complete. |
| **AD-003** | Session ownership = device that owns the PTY | GTK/Android are stateless renderers; daemon owns all PTYs | ✅ IMPLEMENTED | T-048, T-051, T-065 | Daemon owns PTYs. GTK subscribes output. Exception: new GTK workspaces still use local PTYs (AD-002 gap). |
| **AD-004** | Android pairing is asymmetric (like SSH) | Bidirectional I/O; PTY stays on PC; Android is renderer only | ⏸ ON HOLD | — | T-052–T-054 deferred until Linux GTK client is complete. |
| **AD-005** | Android pairing rules | Android never runs iroh listener; no Android-to-Android pairing | ⏸ ON HOLD | — | iroh listener compiled desktop-only. Android client side not started. |
| **AD-006** | Daemon is single-tenant | One GTK window per daemon; no cross-window routing | ✅ IMPLEMENTED | T-049, T-051, T-065 | Each daemon process serves one GTK window. AC "Two windows see each other's changes" was dropped. |
| **AD-007** | libghostty-vt is per-pane only | Layout owned by forgetty-session, not by libghostty-vt | ✅ IMPLEMENTED | T-048, T-059 | `SessionLayout` in forgetty-session owns workspace/tab/pane hierarchy. libghostty-vt operates per-pane only. |
| **AD-008** | GTK is a stateless renderer | GTK never writes session files; never spawns PTYs; never owns layout state | ✅ IMPLEMENTED | T-051, T-065, T-067 | Daemon mode: session file writes blocked. New workspaces go through `create_workspace` RPC; daemon owns all PTYs. |

---

## DAEMON_SYNC_ARCHITECTURE.md Components

| Component | Required | Status | Implemented in | Gap / Notes |
|-----------|----------|--------|---------------|-------------|
| `forgetty-session` crate | Platform-agnostic `SessionManager`, no GTK dep | ✅ IMPLEMENTED | T-048 | `crates/forgetty-session/` — zero GTK deps confirmed. |
| `forgetty-session::SessionManager` | Owns PTYs, VT instances, workspace registry | ✅ IMPLEMENTED | T-048–T-067 | Owns PTYs + VT. `create_workspace()` added (T-067). |
| `forgetty-daemon` binary | Headless; systemd service; socket server; iroh listener | ✅ IMPLEMENTED | T-049, T-050 | Binary exists. `--foreground` flag. Socket server runs. iroh endpoint binds. systemd service file present. |
| `forgetty-socket` JSON-RPC | All 8 original methods + layout RPCs wired to real state | ✅ IMPLEMENTED | T-050, T-060–T-067 | All methods wired. Layout RPCs added. `create_workspace` added (T-067). |
| `subscribe_output` streaming | Daemon pushes PTY output to connected clients | ✅ IMPLEMENTED | T-050, T-051 | Streaming via mpsc + GLib poll timer. |
| `subscribe_layout` streaming | Daemon pushes layout events to connected clients | ✅ IMPLEMENTED | T-063, T-065 | LayoutEvent enum; background tokio task; GLib poll; idempotent handler. |
| `get_layout` RPC | Returns full `SessionLayout` as JSON | ✅ IMPLEMENTED | T-062 | `get_layout` handler returns all workspaces/tabs/pane-trees. |
| GTK as thin renderer | GTK connects to daemon socket; sends RPCs for all actions | ✅ IMPLEMENTED | T-051, T-064, T-065 | `ensure_daemon()` pattern. All tab/split/close actions go through RPCs. |
| Cold-start restore | Daemon loads `{uuid}.json` on startup, recreates PTYs | ✅ IMPLEMENTED | T-064, T-067, T-068, BUG-002 | CWDs restore. All workspaces restored (T-067). UUID session file used (T-068). Split tree fully restored with original ratios (BUG-002 fix). |
| UUID-based socket + session | Per-window isolation via `forgetty-{uuid}.sock` | ✅ IMPLEMENTED | T-068 | `socket_path_for(uuid)` in GTK; `--session-id` flag in daemon; UUID socket + session file paths. |
| Multi-window restore on login | Enumerate `sessions/*.json`, open one window per file | ✅ IMPLEMENTED | T-068 | `forgetty --restore-all` enumerates `list_sessions()` and spawns one process per UUID. |
| Split tree restore on cold-start | Pane splits and ratios survive daemon restart | ✅ IMPLEMENTED | BUG-002 fix | `restore_subtree()` + `split_pane_with_ratio()` reconstruct full pane trees with original ratios. |
| totem-sync / iroh identity | Persistent Ed25519 keypair; QR payload generation | ✅ IMPLEMENTED | T-052 (partial) | `load_or_generate()` key management done. `QrPayload` struct done. `--show-pairing-qr` flag works. |
| iroh endpoint + accept loop | Daemon listens for incoming Android iroh connections | ✅ IMPLEMENTED | T-052 (partial) | `SyncEndpoint::bind()` and `accept_loop()` implemented. No actual session streaming yet. |
| Full terminal stream to Android | Raw PTY bytes over iroh QUIC to Android client | ⏸ ON HOLD | — | T-053 deferred. |
| Bidirectional PTY input from Android | Android keystrokes routed to daemon PTY | ⏸ ON HOLD | — | T-054 deferred. |

---

## Summary Scorecard

| Category | Total | ✅ Done | 🟡 Partial | ❌ Missing | ⏸ On hold |
|----------|-------|---------|-----------|----------|----------|
| Architecture Decisions (AD-001–AD-008) | 8 | 6 | 0 | 0 | 2 |
| Daemon/Sync components | 14 | 13 | 0 | 0 | 2 |

**Last updated by:** T-M1-extra-007 (2026-04-05)

---

## Pending Tasks That Fix Gaps

No outstanding gaps for M4 architecture decisions. AD-004 and AD-005 (Android pairing) remain on hold pending Android client work.
