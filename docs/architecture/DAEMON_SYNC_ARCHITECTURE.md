# Forgetty — Daemon + Android Sync Architecture

> **Decided:** 2026-04-01 after full architecture review.
> **Status:** Approved. Implementation starts with T-048.
> **This doc supersedes** the earlier "GTK owns everything" model.

---

## The Product Promise

> "Open the phone app and your terminals are there — exactly as you left them on your laptop.
> No pairing. No syncing. No waiting. Just there."

This requires a fundamental architecture shift: **the daemon owns everything, renderers are thin**.

---

## Core Principle: Daemon-First

```
BEFORE (GTK owns everything):
  forgetty-gtk
    └── TabStateMap (owns all PTYs, VT state, workspace)
          └── forgetty-socket (stub, can't reach GTK state)

AFTER (Daemon owns everything):
  forgetty-daemon
    ├── forgetty-session  (owns ALL PTYs, VT state, workspaces)
    ├── forgetty-socket   (JSON-RPC → GTK connects here)
    └── totem-sync/iroh   (QUIC → Android/Web connect here)

  forgetty-gtk            (thin renderer → connects to daemon)
  forgetty-android        (thin renderer → connects via iroh)
  forgetty-web (future)   (thin renderer → connects via WebSocket)
  forgetty-cli (future)   (thin renderer → attaches to daemon)
```

The daemon is the tmux server. GTK and Android are tmux clients. They render state they don't own.

---

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│  forgetty-daemon (Linux, headless, systemd user service)         │
│                                                                  │
│  ┌──────────────────┐  ┌─────────────────┐  ┌───────────────┐  │
│  │ forgetty-session │  │ forgetty-socket │  │  totem-sync   │  │
│  │                  │  │  (JSON-RPC)     │  │  (iroh/QUIC)  │  │
│  │ SessionManager   │  │                 │  │               │  │
│  │ ├── WorkspacesMgr│  │ list_tabs       │  │ P2P listener  │  │
│  │ ├── TabRegistry  │  │ new_tab         │  │ DERP fallback │  │
│  │ ├── PaneTree     │◄─┤ send_input      │  │ iroh identity │  │
│  │ ├── PtyBridges   │  │ get_screen      │  │ device pairing│  │
│  │ └── VtInstances  │  │ split_pane      │◄─┤ PTY streams   │  │
│  └──────────────────┘  └────────┬────────┘  └──────┬────────┘  │
│                                  │                   │           │
└──────────────────────────────────┼───────────────────┼───────────┘
                                   │                   │
                         Unix socket              QUIC (iroh)
                                   │                   │
                     ┌─────────────┘       ┌───────────┘
                     ▼                     ▼
              ┌─────────────┐    ┌──────────────────────┐
              │ forgetty-gtk│    │  forgetty-android    │
              │             │    │  (Kotlin + Rust JNI) │
              │ GTK renderer│    │                      │
              │ Pango/Cairo │    │  Compose renderer    │
              │ ← renders   │    │  VT parser (Rust)    │
              │ → input RPC │    │  ← renders           │
              └─────────────┘    │  → input over iroh   │
                                 └──────────────────────┘
```

---

## Components

### 1. `forgetty-session` (new crate, no platform deps)

**What moves here from forgetty-gtk:**
- `TerminalState` → `PaneState` (PTY handle + VT instance + metadata)
- `TabStateMap` → `SessionManager`
- `WorkspaceManager` logic
- PTY spawn, kill, read loop
- VT state (libghostty-vt handles)
- Session save/restore (already in forgetty-workspace, wire here)

**Key types:**
```rust
pub struct SessionManager {
    workspaces: Arc<Mutex<WorkspaceRegistry>>,
    panes: Arc<Mutex<HashMap<PaneId, PaneState>>>,
    event_tx: broadcast::Sender<SessionEvent>,
}

pub struct PaneState {
    id: PaneId,
    pty: PtyBridge,
    vt: VtInstance,          // libghostty-vt handle
    cwd: PathBuf,
    metadata: PaneMetadata,
}

pub enum SessionEvent {
    PtyOutput { pane_id: PaneId, data: Bytes },
    PaneCreated { pane_id: PaneId, info: PaneInfo },
    PaneClosed { pane_id: PaneId },
    WorkspaceChanged(WorkspaceState),
    Notification { pane_id: PaneId, kind: NotificationKind },
}
```

**Thread model:** PTY read loops run as tokio tasks. `SessionManager` is `Arc<>` — cloneable, sendable across threads. No `Rc<RefCell<>>` anywhere.

---

### 2. `forgetty-daemon` (new binary)

```
forgetty-daemon [OPTIONS]

Options:
  --foreground          Don't daemonize (for debugging)
  --show-pairing-qr     Print QR code to terminal (for headless servers)
  --list-devices        List paired Android devices
  --revoke <device-id>  Unpair a device
  --no-restore          Start fresh (ignore saved sessions)

Starts:
  1. SessionManager (loads saved workspace state)
  2. forgetty-socket server (Unix socket: $XDG_RUNTIME_DIR/forgetty.sock)
  3. totem-sync listener (iroh endpoint, waits for Android connections)
  4. Auto-save timer (every 30s, atomic write)
```

**systemd user service** (`dist/linux/forgetty-daemon.service`):
```ini
[Unit]
Description=Forgetty terminal session daemon
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/forgetty-daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

`install.sh` runs `systemctl --user enable --now forgetty-daemon`.

**Ensure-daemon from GTK** (GTK launch sequence):
```rust
// In forgetty-gtk/src/main.rs
fn ensure_daemon_running() {
    let sock = default_socket_path();
    if sock.exists() && can_connect(&sock) { return; }
    // Daemon not running — spawn it
    Command::new("forgetty-daemon")
        .arg("--foreground")
        .spawn()
        .expect("failed to spawn daemon");
    wait_for_socket(&sock, Duration::from_secs(5));
}
```

---

### 3. `forgetty-socket` (updated — real state)

The 8 JSON-RPC handlers are wired to `SessionManager` via tokio channels (socket server runs on its own tokio thread, sends `SessionCommand` to main runtime, awaits `SessionResponse`):

| Method | Real implementation |
|---|---|
| `list_tabs` | `session.list_panes()` → real pane list with CWD, title, PID |
| `new_tab` | `session.create_pane(cwd, cmd)` → real PTY spawn |
| `close_tab` | `session.close_pane(id)` → SIGTERM + PTY cleanup |
| `focus_tab` | `session.set_active_pane(id)` (daemon tracks active) |
| `split_pane` | `session.split_pane(id, direction)` |
| `send_input` | `session.write_pty(id, bytes)` → writes to PTY master |
| `get_screen` | `session.read_viewport(id)` → VT cell snapshot |
| `get_pane_info` | `session.pane_info(id)` → rows, cols, title, CWD, PID |

New methods added:
- `subscribe_output` → opens a streaming connection, daemon pushes PTY output events
- `get_workspace_state` → full workspace JSON
- `list_notifications` → pending notification rings

---

### 4. `forgetty-gtk` (refactored — thin renderer)

**What stays:** All GTK widget code, Pango/Cairo rendering, input event handling, UI state (selection, scroll position, search, sidebar).

**What changes:**
- No PTY ownership. All PTY ops go via `send_input`, `new_tab`, `close_tab` JSON-RPC.
- Subscribe to `subscribe_output` stream from daemon → apply PTY bytes to local VT mirror.
- Tab/pane structure reconciled from daemon state on startup.
- New tab shortcut → `new_tab` RPC → daemon spawns PTY → GTK creates DrawingArea for it.

**Implication for existing features:** All T-001–T-031 features remain intact. Only the data source changes (daemon instead of in-process).

---

### 5. `totem-sync` / iroh integration

Using **iroh 0.35** (already decided in XD-002). iroh provides:
- Ed25519 identity (device keypair, permanent)
- QUIC transport (Quinn under the hood)
- DERP relay servers (NAT fallback, operated by n0/iroh team, we can also run our own)
- NAT traversal (hole-punching built in)
- `iroh-blobs` for large data transfer (scrollback chunks)
- `iroh-gossip` for real-time events (notifications, workspace changes)

**iroh in the daemon:**
```rust
// Daemon startup
let endpoint = iroh::Endpoint::builder()
    .secret_key(load_or_generate_identity())
    .bind()
    .await?;

// Android connects by dialing our public key
// No IP needed — iroh handles NAT traversal + DERP fallback
println!("Daemon iroh key: {}", endpoint.node_id());
```

**QR code content:**
```json
{
  "v": 1,
  "node_id": "<iroh Ed25519 pubkey, base32>",
  "name": "vick-desktop",
  "relay": "https://euw1-1.relay.iroh.network/"
}
```

Phone scans QR → dials node_id via iroh → iroh handles NAT traversal → QUIC connection established → Noise handshake (built into iroh's QUIC) → session streams begin.

---

## The Connection Model

### Pairing (one-time, ever)

```
Desktop:
  1. Generate persistent identity (or load existing)
  2. Display QR: { node_id, machine_name, relay_hint }
  3. Wait for incoming iroh connection on this node_id
  4. Validate new device → store in authorized_devices
  5. QR dismissed

Android:
  1. Camera scans QR → parse { node_id, machine_name }
  2. Dial node_id via iroh (NAT traversal automatic)
  3. Noise handshake (iroh handles this)
  4. Store: { desktop_node_id, machine_name }
  5. Connection established — begin session streaming
```

**After pairing:** Android has the desktop's node_id. That's all it needs forever. iroh finds the desktop by node_id regardless of IP. No re-pairing unless explicitly unpaired.

---

### Connection Lifecycle (Android)

```
DISCONNECTED
  │
  │  app.onResume() — user opens app
  ▼
CONNECTING (~200-500ms)
  ├── iroh.connect(desktop_node_id)
  ├── NAT traversal / hole-punch / DERP fallback (automatic)
  ├── Receive WorkspaceState (tab/pane layout)
  └── Receive FullSnapshot per visible pane (viewport cells)
  │
  ▼
CONNECTED — live, foreground
  ├── PTY output streams: desktop → Android (raw bytes)
  ├── PTY input streams: Android → desktop (keystrokes)
  ├── iroh connection migration (WiFi→cellular = transparent)
  └── QUIC keepalive every 25s
  │
  │  app.onPause() — user switches to another app
  ▼
BACKGROUND (foreground service running, connection maintained)
  ├── QUIC streams stay alive
  ├── PTY output buffered (ring buffer, last 1000 lines per pane)
  ├── Inactivity timer starts (default: 60 min, configurable)
  │
  │  app.onResume() before timer fires → instant, no loading
  │
  │  timer fires (60 min of no user)
  ▼
DISCONNECTING (graceful)
  ├── Close QUIC streams
  ├── Stop Android foreground service (notification dismissed)
  └── Cache last workspace snapshot locally
  │
  ▼
DISCONNECTED (SQLite cache holds last known state for display)
  │
  │  app.onResume() after timer
  ▼
CONNECTING (brief loading indicator, ~200-500ms)
```

**Key properties:**
- User never sees a "connect" button after initial pairing
- Loading indicator is the only UX cost after long inactivity
- Network switch (WiFi→cellular): QUIC connection migration = transparent
- Desktop reboots/daemon restart: Android auto-reconnects (exponential backoff, max 60s interval)

---

## Terminal Data Protocol

### On Connect: Full Snapshot

```rust
struct FullSnapshot {
    pane_id: PaneId,
    rows: u16,
    cols: u16,
    cells: Vec<CellSnapshot>,      // viewport only (rows×cols cells)
    cursor: CursorPos,
    scrollback_line_count: u32,    // how many lines exist (don't send yet)
    theme: TerminalTheme,
}

struct CellSnapshot {
    char: char,
    fg: Rgb,
    bg: Rgb,
    attrs: CellAttrs,              // bold, italic, underline, etc.
}
```

Snapshot is sent once on connect per visible pane. Viewport only (~80×24 = 1,920 cells ≈ 50-200KB). Scrollback is on demand.

### Ongoing: Raw PTY Bytes

```rust
struct PtyOutput {
    pane_id: PaneId,
    seq: u64,           // sequence number for ordering
    data: Bytes,        // raw PTY output bytes (may be zstd compressed)
    compressed: bool,
}
```

Android runs its own VT parser (forgetty-vt via JNI, same library as desktop) to apply bytes to its local VT mirror. This is the most compact format — identical bytes to what the desktop's VT parser receives.

**Backpressure:** If Android's receive buffer fills (slow connection), daemon drops intermediate frames and sends a new `FullSnapshot` instead. Android never falls behind — it just gets the current frame.

### PTY Input (Android → Desktop)

```rust
struct PtyInput {
    pane_id: PaneId,
    data: Bytes,        // raw bytes: printable chars, escape sequences, etc.
}
```

Android keyboard → encode to correct bytes (using the same key encoder logic as desktop) → send over QUIC stream → daemon writes to PTY master → process receives input.

### Lazy Scrollback

```rust
// Android requests:
struct ScrollbackRequest {
    pane_id: PaneId,
    from_line: u32,     // 0 = oldest
    count: u32,         // max 500 lines per request
}

// Daemon responds:
struct ScrollbackChunk {
    pane_id: PaneId,
    from_line: u32,
    lines: Vec<Vec<CellSnapshot>>,
    total_lines: u32,
}
```

Android fetches scrollback pages only as user scrolls up. Never sent proactively.

---

## Daemon Lifecycle: What Happens When

| Event | What daemon does |
|---|---|
| System login | systemd starts daemon, loads saved session state |
| GTK opens | GTK connects via Unix socket, receives current state |
| User creates tab in GTK | GTK sends `new_tab` RPC → daemon spawns PTY → broadcasts to all clients |
| Android opens app | iroh connection established → snapshots sent |
| User types in GTK | GTK sends `send_input` RPC → daemon writes to PTY |
| User types on Android | Android sends PTY input over iroh → daemon writes to PTY |
| PTY produces output | Daemon broadcasts raw bytes to GTK + Android simultaneously |
| GTK closes | GTK disconnects from daemon socket. Daemon and PTYs keep running. |
| Android loses WiFi | iroh QUIC migrates to cellular. If fails: reconnects via DERP relay. |
| Laptop sleeps | PTYs survive (kernel suspends everything). Network connections drop. |
| Laptop wakes | iroh reconnects automatically. Daemon sends fresh snapshots. |
| Daemon crashes | systemd restarts it (Restart=on-failure). Session state reloaded from last auto-save. |
| GTK opens, daemon not running | GTK spawns daemon as subprocess before connecting. |

---

## Security Model

- **Device identity:** Ed25519 keypair per device, stored at `~/.local/share/forgetty/identity.key`. Never leaves the machine.
- **E2E encryption:** All iroh connections use Noise protocol (built into iroh's QUIC). DERP relay servers see only ciphertext.
- **Authorization:** Daemon maintains `~/.local/share/forgetty/authorized_devices.json`. Only listed devices can connect. Pairing adds a device; unpair removes it.
- **No accounts:** No Totem cloud required. Works fully offline / LAN-only.
- **Local socket:** Unix socket (`$XDG_RUNTIME_DIR/forgetty.sock`) is user-only (chmod 0600). Only processes running as the same user can connect.

---

## Settings Exposed in GTK

```
Background & Sync:
  [✓] Keep sessions running when app is closed
      systemctl --user enable forgetty-daemon
  [✓] Start automatically on login (requires above)

Android Connection:
  [ ] Close connection after [60] minutes of phone inactivity
      (configurable: 15 / 30 / 60 / 120 min / Never)

Paired Devices:
  vick-pixel-9     Last seen: just now     [Revoke]
  work-pixel-fold  Last seen: 3 days ago   [Revoke]
  [Pair new device]  → shows QR code
```

---

## Crate Dependency Graph (final)

```
forgetty-core         ←── everything depends on this
forgetty-config       ←── config, themes
forgetty-vt           ←── libghostty-vt FFI
forgetty-pty          ←── PTY management
forgetty-workspace    ←── session persistence
forgetty-session      ←── NEW: SessionManager (uses core/pty/vt/workspace)
forgetty-watcher      ←── file watcher
forgetty-socket       ←── JSON-RPC (uses core, wired to session)
forgetty-viewer       ←── markdown/image viewer
forgetty-clipboard    ←── smart copy

totem-sync            ←── iroh wrapper (uses core)

[binaries]
forgetty-daemon       ←── uses session + socket + totem-sync
forgetty-gtk          ←── uses socket client + totem-sync client + GTK rendering
forgetty-android      ←── uses totem-sync (JNI) + Kotlin Compose rendering
```

---

## Implementation Order

```
1. T-048: forgetty-session crate (extract from GTK)
2. T-049: forgetty-daemon binary + systemd service
3. T-050: Wire forgetty-socket to real session state
4. T-051: GTK refactor as daemon client
5. T-052: totem-sync / iroh identity + pairing QR
6. T-053: Full terminal stream to Android
7. T-054: Full interactive from Android (bidirectional)

Then original M3/M4 features build on top of this foundation.
```

---

## What This Unlocks Beyond Android

| Feature | How daemon enables it |
|---|---|
| "Run in background" right-click | Already true — all panes are daemon panes, closing GTK tab doesn't kill PTY |
| Multiple GTK windows | Both attach to same daemon simultaneously |
| Web client | WebSocket → daemon (same protocol as iroh, different transport) |
| CLI attach | `forgetty attach <pane-id>` → dump PTY output to stdout |
| MCP server (T-042) | MCP calls `send_input`/`get_screen` via socket API |
| Monarch/Peasant (T-041) | Multiple GTK instances all talk to same daemon, daemon IS the monarch |
| SSH-based remote use | `forgetty-daemon` on a server, Android connects via iroh regardless of NAT |
