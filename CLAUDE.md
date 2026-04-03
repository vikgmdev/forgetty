# Forgetty — Claude Code Project Context

> **Read this file first.** Then **immediately** read `docs/architecture/ARCHITECTURE_DECISIONS.md` — this contains locked decisions that override anything else in this repo. Then read `docs/harness/BACKLOG.md` for the full backlog.

## CRITICAL: Architecture Decisions

**Every session, every agent, every phase must read `docs/architecture/ARCHITECTURE_DECISIONS.md` before doing any work.** It contains locked decisions (one daemon per window, session ownership model, Android pairing rules, etc.) that override older statements in this file, BACKLOG.md, and DAEMON_SYNC_ARCHITECTURE.md.

## What is Forgetty?

**A workspace-aware terminal where your AI agents, tabs, splits, and sessions persist across reboots and sync across devices.**

Built for developers who work with AI coding agents (Claude Code, etc.) daily. Simple terminals are for the old days.

## Core Architectural Principle

**"Native shell per platform with shared Rust core."**

This means: GTK4 on Linux, WinUI/winit on Windows, Jetpack Compose on Android. The platform shell is thin (~1,300 lines). The shared Rust core is thick (~10,000+ lines, ~70% of code).

We do NOT use a single cross-platform renderer. We learned the hard way that wgpu+glyphon can never match native text rendering quality (no subpixel AA, no Fontconfig, no IME). GTK4+Pango/FreeType gives us identical rendering to Ghostty on Linux.

## Strategic Formula

**"Match everything Ghostty does on Linux, then add what it doesn't have."**

Ghostty wins on: native macOS quality, years of polish, huge user base.
We win on: AI-native features (notifications, smart copy, viewer), session persistence, workspaces, cloud sync, platform reach (Windows/Android/Web).
We don't compete on macOS — cmux and Ghostty own that.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  Platform Shell (THIN — native per platform)          │
│  Linux:   GTK4 + libadwaita (gtk4-rs)    ← CURRENT   │
│  Windows: winit + wgpu + DirectWrite     ← FUTURE     │
│  Android: Jetpack Compose + Rust JNI     ← FUTURE     │
│  Web:     DOM + WebGPU canvas            ← FUTURE     │
├──────────────────────────────────────────────────────┤
│  Shared Rust Core (THICK — platform-independent)      │
│  forgetty-vt       libghostty-vt FFI (Ghostling pattern) │
│  forgetty-pty      PTY management (portable-pty)      │
│  forgetty-input    Key/mouse encoders (ghostty APIs)  │
│  forgetty-core     Shared types, errors               │
│  forgetty-config   Config, theme, defaults             │
│  forgetty-workspace Session save/restore              │
│  forgetty-socket   JSON-RPC API                       │
│  forgetty-watcher  File watcher                       │
│  forgetty-viewer   Markdown/image viewer              │
│  forgetty-clipboard Smart copy pipeline               │
├──────────────────────────────────────────────────────┤
│  libghostty-vt.so (Zig, C API — NOT our code)        │
│  SIMD VT parser, Kitty protocol, Unicode graphemes,   │
│  scrollback, text reflow — proven by millions of users │
└──────────────────────────────────────────────────────┘
```

## Why GTK4 on Linux (not wgpu)

We evaluated 4 paths. See `docs/harness/BACKLOG.md` → "Path Analysis" for the full comparison.

**Short version:**
- Path A (pure wgpu): Can never match native Linux text quality (no subpixel AA, no Fontconfig)
- Path B (fork Ghostty Zig): Dead end for cross-platform (Zig + GTK locks us to Linux)
- **Path C (GTK4 shell + shared core): CHOSEN** — native quality on Linux, cross-platform core
- Path D (full libghostty): Not ready for Linux (API only stable on macOS)

GTK4 gives us for free: Pango/FreeType text (identical to Ghostty), Fontconfig fonts, IME, accessibility, CSD, tab widgets, split widgets, context menus, scrollbars.

## FFI Pattern (CRITICAL — weeks of debugging led to these rules)

Follow Ghostling's exact pattern (the official libghostty-vt reference at `/tmp/ghostling/main.c`):
1. Pass `&mut handle as *mut _ as *mut c_void` (pointer TO handle, NOT handle AS pointer)
2. Use `GRAPHEMES_LEN` + `GRAPHEMES_BUF` for cell text (NOT `grid_ref`)
3. Use `FG_COLOR`/`BG_COLOR` per cell (pre-resolved RGB, NOT manual palette lookup)
4. Call `render_state_update()` every frame before reading cells
5. Use key/mouse encoder APIs (NOT raw byte sequences)
6. Register Write-PTY callback: pass function pointer directly as `*const c_void` (NOT `&fn_ptr`)

## PTY Signal Delivery (CRITICAL — hard-won debugging)

**Ctrl+C must do two things, not one.**

Writing `0x03` to the PTY master is not enough. When a child process (Node.js, pm2, any readline app) puts the slave PTY into raw mode (`ISIG` disabled), the line discipline won't convert `0x03` to `SIGINT`. The byte just echoes as `^C` and the process keeps running.

The fix: after writing `0x03`, also call `kill(-pgid, SIGINT)` directly.

**Getting the foreground pgrp — use the master fd, NOT `/proc/{pid}/fd/0`:**

```rust
// CORRECT: tcgetpgrp via the master PTY fd we already hold
if let Some(pgid) = pty.foreground_pgrp() {  // wraps MasterPty::process_group_leader()
    libc::kill(-(pgid as c_int), libc::SIGINT);
}

// WRONG: opening slave via /proc/{shell_pid}/fd/0
// - Unreliable proc symlink resolution
// - Requires O_NOCTTY to avoid stealing forgetty's controlling terminal
// - tcgetpgrp on that fd still fails silently in practice
```

`portable-pty`'s `MasterPty` trait exposes `process_group_leader()` which calls `tcgetpgrp(master_fd)`. Use `PtyProcess::foreground_pgrp()` which wraps this. The master fd is always correct and already open — no extra files to open, no permission issues, no O_NOCTTY needed.

**GTK accelerator conflict:** Registering `<Control>c` as a GTK accel causes ALL Ctrl+C presses to be consumed at the window level before `EventControllerKey` fires. Instead: remove it from accels and call `da.activate_action("win.copy", None)` directly from the key handler when there is a selection.

**BEL suppression:** zsh sends `\x07` (BEL) when it receives SIGINT (readline cleanup). Set `suppress_bell_until = Some(Instant::now() + Duration::from_millis(300))` after sending the signal to eat the spurious BEL before it triggers a visual flash.

## Development Workflow — MANDATORY

**Every task follows the harness methodology. No exceptions.**

1. Read `docs/harness/SESSION_STATE.md` — find where we left off
2. Read `docs/harness/BACKLOG.md` — pick the next uncompleted `[ ]` task
3. Run 3 sequential subagents per task:
   - **Planner** → writes `docs/harness/SPEC.md`
   - **Builder** → implements, commits, verifies no crashes
   - **QA** → tests every AC, scores, writes `docs/harness/QA_REPORT.md`
4. If QA fails (any score < 7), fix cycle (max 3 rounds)
5. Mark task `[x]` in BACKLOG.md, update SESSION_STATE.md

Full rules: `docs/harness/METHODOLOGY.md`

## Build

```bash
# Requires: Rust stable, Zig 0.15+ (/home/vick/.local/zig/zig)
# For GTK4 pivot: sudo apt install libgtk-4-dev libadwaita-1-dev
cargo build --release
cargo run --release
```

## Current Status

**GTK4 pivot starting.** Next task: T-001 (GTK4 window skeleton).
See `docs/harness/SESSION_STATE.md` for details.
