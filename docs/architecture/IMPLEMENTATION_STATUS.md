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
| **AD-007** | Daemon is a byte pipe, not a terminal | ❌ Not implemented | Current v0.1 daemon has `VtInstance` per pane and parses ANSI. **V2-008 removes it.** |
| **AD-008** | Clients own terminal semantics | 🟡 Partial | GTK already has its own VT parser (dual-parse with daemon). **V2-008 makes it the only one.** Android already has its own VT parser via JNI (compliant). |
| **AD-009** | No polling on the hot path | ✅ Implemented | 20 ms daemon drain loop removed by V2-001. 8 ms GLib timer on GTK output poll removed by V2-002. Hot path is fully event-driven. |
| **AD-010** | Raw PTY bytes in length-prefixed binary frames | ❌ Not implemented | `subscribe_output` currently sends base64-encoded bytes wrapped in JSON. **V2-003 replaces it.** |
| **AD-011** | Daemon always runs; no local-PTY fallback | ❌ Not implemented | GTK currently has a second `create_terminal()` path for local-PTY mode. **V2-004 deletes it.** |
| **AD-012** | Daemon survives window close | ❌ Not implemented | `shutdown_clean` RPC kills the daemon on window close. **V2-005 adds `disconnect` and changes the close handler.** |
| **AD-013** | Persistence = byte log, not cell snapshot | ❌ Not implemented | Current persistence is `snapshots/{uuid}.bin` VT-state binary. **V2-007 replaces with `logs/{pane_uuid}.log` byte log.** |
| **AD-014** | Client-side color resolution | ❌ Not implemented | Colors resolved in daemon VT layer at parse time. **V2-009 moves resolution to client render time.** |
| **AD-015** | `forgetty-sync` has no terminal deps | ❌ Not implemented | `forgetty-sync` currently depends on `forgetty-session`. **V2-011 decouples.** |

---

## v0.1 components that are preserved under the new architecture

| Component | Status | Notes |
|-----------|--------|-------|
| `forgetty-session` crate structure (SessionManager, SessionLayout) | ✅ Kept | Loses the VtInstance per pane under V2-008; otherwise unchanged. |
| `forgetty-pty` crate | ✅ Kept | Daemon keeps owning PTY processes. |
| `forgetty-vt` crate | ✅ Kept | Still the VT parser library. Dependency moves from `forgetty-session` to clients (GTK, Android). |
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
| base64 + JSON encoding of PTY bytes on `subscribe_output` | ❌ To remove | V2-003 |
| Second `create_terminal()` in GTK (local-PTY path) | ❌ To remove | V2-004 |
| `shutdown_clean` on window close | ❌ To remove (keep for "close permanently" action) | V2-005 |
| Daemon OSC 9 notification parsing | ❌ To remove (moves to client) | V2-006 |
| `snapshots/{uuid}.bin` VT binary snapshots | ❌ To remove | V2-007 |
| `VtInstance` in `PaneState` (daemon-side) | ❌ To remove | V2-008 |
| `get_screen` RPC (daemon has no screen to return) | ❌ To remove | V2-008 |
| `preseed_snapshot` RPC | ❌ To remove | V2-008 |
| `forgetty-vt` dependency in `forgetty-session` | ❌ To remove | V2-008 |
| `forgetty-renderer`, `forgetty-ui`, `forgetty-viewer` crates | ❌ To remove (unused) | V2-012 |

---

## Summary scorecard

| Category | Total | ✅ Done | 🟡 Partial | ❌ Missing |
|----------|-------|---------|-----------|-----------|
| Architectural decisions (AD-001…AD-015) | 15 | 6 | 2 | 7 |

Target: all ❌ → ✅ by the end of the V2 backlog (V2-001 through V2-012).
