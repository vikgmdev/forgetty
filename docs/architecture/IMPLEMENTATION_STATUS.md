# Forgetty — Architecture Implementation Status

> **Auto-maintained.** The builder/QA agents update this file as tasks ship.
> Source of truth for architectural decisions: `ARCHITECTURE_DECISIONS.md`.
>
> **Legend:** ✅ Implemented · 🟡 Partial · ❌ Not implemented · ⏸ On hold (deferred)

---

## Architectural decisions (from ARCHITECTURE_DECISIONS.md)

| AD | Decision | Status | Implemented in / Gap |
|----|----------|--------|---------------------|
| **AD-001** | One daemon per window, UUID socket + UUID session file | ✅ Implemented | T-068 (v0.1). UUID per window end-to-end. |
| **AD-002** | Session hierarchy workspaces → tabs → panes | ✅ Implemented | T-059…T-067 (v0.1). SessionLayout in forgetty-session. |
| **AD-003** | Session ownership = device with PTY | ✅ Implemented | T-048, T-051, T-065 (v0.1). |
| **AD-004** | Android pairing is asymmetric (SSH-like) | 🟡 Partial | Pairing works; streaming works end-to-end (Android MA-012). Under AD-007/AD-008 model, data path is already raw bytes → Android's own VT parser. |
| **AD-005** | Android pairing rules; no Android-as-host | ✅ Implemented | iroh listener desktop-only. Android app has no inbound side. |
| **AD-006** | Daemon is single-tenant | ✅ Implemented | Each daemon serves one GTK window + optional paired devices. |
| **AD-007** | Daemon is a byte pipe, not a terminal | ✅ Implemented | V2-008 (2026-04-19). `VtInstance` deleted from `forgetty-session`; `forgetty-vt` dependency removed from `forgetty-session` + `forgetty-socket`; `get_screen` / `preseed_snapshot` RPCs retired (METHOD_NOT_FOUND). `ldd target/release/forgetty-daemon \| grep ghostty` → empty. Daemon is now a process supervisor + byte pipe + metadata server. Iroh `FullSnapshot` becomes a reset-marker sentinel + `PtyBytes` replay of the byte-log ring — Android wire schema unchanged. |
| **AD-008** | Clients own terminal semantics | ✅ Implemented | V2-008 (2026-04-19). GTK is the sole VT parser in the desktop data flow after the daemon-side `VtInstance` deletion. Android already parses via JNI. Every byte of ANSI is parsed exactly once. V2-006 (2026-04-18) prerequisite: client-side OSC 9 / 99 / 777 scanning (alongside existing client-side OSC 0/2/7). |
| **AD-009** | No polling on the hot path | ✅ Implemented | 20 ms daemon drain loop removed by V2-001. 8 ms GLib timer on GTK output poll removed by V2-002. Hot path is fully event-driven. |
| **AD-010** | Raw PTY bytes in length-prefixed binary frames | ✅ Implemented | V2-003 (2026-04-17). `subscribe_output` now streams `[u32 BE length][payload]` frames with a 4 MiB cap, matching the `forgetty-sync` Android wire. Byte-perfect 10 MiB round-trip verified. |
| **AD-011** | Daemon always runs; no local-PTY fallback | ✅ Implemented | V2-004 (2026-04-17). Second `create_terminal()` deleted. `TerminalState.pty`/`pty_rx` fields removed. `forgetty-pty` dep dropped from `forgetty-gtk`. `ensure_daemon()` now exits 1 on failure (no silent fallback). `--temp` mode preserved as scope boundary. |
| **AD-012** | Daemon survives window close | ✅ Implemented | V2-005 (2026-04-17). New `disconnect` JSON-RPC added; GTK X-button, Ctrl+Shift+Q, and SIGTERM/SIGHUP/SIGINT signal handlers now call `disconnect` instead of `shutdown_clean`/`shutdown_save`. Daemon persists state (session JSON + v0.1 VT snapshots) and drops the connection without exiting. Hamburger "Close Window Permanently" still wired to `shutdown()` as the explicit-kill path. |
| **AD-013** | Persistence = byte log, not cell snapshot | ✅ Implemented | V2-007 (2026-04-19). Append-only `~/.local/share/forgetty/logs/{pane_uuid}.log` replaces VT-state binary snapshots. In-memory ring (1 MiB default) + on-disk log (10 MiB cap, rotate-newest-half). Subscribe path does atomic subscribe+snapshot via `SessionManager::subscribe_with_snapshot` — client VT parser rebuilds state from replay bytes naturally, no cell-snapshot format. Cross-daemon orphan safety via `all_persisted_pane_ids()` union across all session JSONs (AD-001-aware). Daemon startup sync-saves session on every layout mutation to close the prune-race window. Seven fix cycles documented in ``. |
| **AD-014** | Client-side color resolution | ❌ Not implemented | Colors resolved in daemon VT layer at parse time. **V2-009 moves resolution to client render time.** |
| **AD-015** | `forgetty-sync` has no terminal deps | ❌ Not implemented | `forgetty-sync` currently depends on `forgetty-session`. **V2-011 decouples.** |

---

## v0.1 components that are preserved under the new architecture

| Component | Status | Notes |
|-----------|--------|-------|
| `forgetty-session` crate structure (SessionManager, SessionLayout) | ✅ Kept | V2-008 (2026-04-19) removed the `VtInstance` per pane; otherwise unchanged. |
| `forgetty-pty` crate | ✅ Kept | Daemon keeps owning PTY processes. |
| `forgetty-vt` crate | ✅ Kept | Still the VT parser library. V2-008 (2026-04-19) completed the dependency migration from `forgetty-session` + `forgetty-socket` to clients (GTK, Android). |
| `forgetty-daemon` binary | ✅ Kept | Name stays. Contents get lighter under AD-007. |
| `forgetty` binary (GUI) | ✅ Kept | Stays the GTK client. Loses local-PTY fallback under V2-004. |
| `forgetty-sync` iroh pairing + streaming | ✅ Kept | Wire payload changes under AD-010 (binary frames). |
| Cold-start session restore (workspaces, tabs, pane tree) | ✅ Kept | Metadata restore stays. Screen state restore moves from VT snapshot (v0.1) to byte-log replay (V2-007). |

---

## v0.1 components that go away under the new architecture

| Component | Status | Removed by |
|-----------|--------|-----------|
| 20 ms drain loop (`tokio::time::sleep` in `daemon.rs`) | ✅ Removed | V2-001 |
| 8 ms GLib timer on GTK PTY output poll | ✅ Removed | V2-002 |
| base64 + JSON encoding of PTY bytes on `subscribe_output` | ✅ Removed | V2-003 |
| Second `create_terminal()` in GTK (local-PTY path) | ✅ Removed | V2-004 |
| `shutdown_clean` on window close | ✅ Removed (V2-005) — window close now calls `disconnect`. `shutdown` preserved for "Close Window Permanently"; `shutdown_clean`/`shutdown_save` wrappers kept in `daemon_client.rs` but no longer called from GTK close paths. | V2-005 |
| Daemon OSC 9 notification parsing | ✅ Removed (V2-006, 2026-04-18) — scanner moved to `forgetty-gtk/src/osc_notification.rs`; client detects OSC 9 / 99 / 777 in the VT feed and fires a `notify` RPC (line-mode, cold-path, rate-limited ≤0.5/s/pane) back to the daemon for logging. Android unchanged — it already scans OSC inline in `PtyBytes` via its own JNI VT parser. | V2-006 |
| `snapshots/{uuid}.bin` VT binary snapshots | ✅ Removed (V2-007, 2026-04-19) | V2-007 |
| `VtInstance` in `PaneState` (daemon-side) | ✅ Removed (V2-008, 2026-04-19) — `vt_instance.rs` deleted; `PaneState` fields are `{ id, pty_bridge, cwd, title, rows, cols }`. | V2-008 |
| `get_screen` RPC (daemon has no screen to return) | ✅ Removed (V2-008, 2026-04-19) — METHOD_NOT_FOUND live-verified. | V2-008 |
| `preseed_snapshot` RPC | ✅ Removed (V2-008, 2026-04-19) — METHOD_NOT_FOUND live-verified. | V2-008 |
| `forgetty-vt` dependency in `forgetty-session` | ✅ Removed (V2-008, 2026-04-19) — also removed from `forgetty-socket`. `cargo tree -p forgetty-session \| grep -c forgetty-vt` → 0. | V2-008 |
| `forgetty-renderer`, `forgetty-ui`, `forgetty-viewer` crates | ❌ To remove (unused) | V2-012 |

---

## Summary scorecard

| Category | Total | ✅ Done | 🟡 Partial | ❌ Missing |
|----------|-------|---------|-----------|-----------|
| Architectural decisions (AD-001…AD-015) | 15 | 12 | 1 | 2 |

Target: all ❌ → ✅ by the end of the V2 backlog (V2-001 through V2-012).
