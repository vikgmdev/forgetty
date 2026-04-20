# Forgetty — System Architecture

> **Status:** Canonical architecture for v0.2 onward.
> Every session, every agent, every task MUST read this file before writing any code or spec.
> Conflicts between this doc and older docs are resolved in favor of this doc.
>
> Companion docs:
> - `ARCHITECTURE_DECISIONS.md` — the numbered, locked-in decisions (AD-001+).
> - `IMPLEMENTATION_STATUS.md` — which decisions are currently implemented vs pending.

---

## Core thesis

Forgetty is a **P2P device networking platform** with a terminal emulator as its flagship app.

The `forgetty-*` crates are building blocks — like MCP plugins for AI agents, but for device connectivity. The terminal proves the platform works. Everything else (chat, files, clipboard sync, port forwarding, future services) plugs into the same infrastructure.

---

## The one sentence that defines every responsibility

**The daemon is a byte pipe. The client is a terminal.**

Everything flows from that sentence. If a design question is ambiguous, apply this rule.

---

## Component responsibilities

### Daemon (`forgetty-daemon` binary)

Owns:
- PTY processes (`forgetty-pty`)
- Raw PTY output byte stream (broadcast to attached clients)
- Byte log per pane on disk (for reconnect + cold-start replay)
- Pane/tab/workspace metadata: UUIDs, PIDs, rows/cols, CWDs
- Session layout structure (workspaces → tabs → pane tree)
- iroh QUIC endpoint, device pairing, identity
- Local Unix-socket JSON-RPC server
- Survival across window close

Does **not** own:
- VT parsing (no `forgetty-vt` dependency)
- Screen buffers or cells
- Color resolution
- Scrollback semantics
- Selection, search, URL detection, terminal queries
- Fonts, glyphs, any rendering concern
- OSC parsing (notifications, title, CWD reporting)

### Client (`forgetty` GUI binary; Android app; future Web/Windows clients)

Owns:
- VT parser (`forgetty-vt` → libghostty-vt)
- Live screen buffer (per pane)
- Scrollback (replayed from daemon byte log + live stream)
- Selection, search, URL hover, copy
- Color palette + theme resolution at render time
- OSC 9 notification detection, OSC 0/2 title, OSC 7 CWD
- Input encoding (keyboard protocol, mouse reporting)
- Rendering (Cairo+Pango today; may change per-platform)

Does **not** own:
- PTY processes (daemon spawns them)
- Session persistence beyond in-memory state (daemon stores byte logs)
- Cross-device sync (daemon is the authoritative source)

### Transport (`forgetty-sync`, `forgetty-socket`)

- `forgetty-sync`: iroh QUIC endpoint, Ed25519 identity, pairing, device registry. **No terminal-specific code.** Usable for future non-terminal services.
- `forgetty-socket`: local Unix-socket JSON-RPC server + binary streaming frames. Terminal-specific methods live here, but the framing library is generic.

---

## System diagram

```
                ┌────────────────────────────────────────────────┐
                │              forgetty (GUI binary)             │
                │                                                │
                │  GTK4 + Cairo/Pango rendering                  │
                │  VtInstance per pane  ← THE ONLY ANSI PARSER   │
                │  Scrollback, selection, search, copy           │
                │  Color resolution at render time               │
                │  OSC notification → notify RPC to daemon       │
                └───────────────────┬────────────────────────────┘
                                    │
                      Unix socket:  │  binary frames
                         [u32 len][raw PTY bytes]
                         (zero polling, event-driven both ways)
                                    │
                ┌───────────────────┴────────────────────────────┐
                │         forgetty-daemon (byte pipe)            │
                │                                                │
                │  PtyBridge per pane → real PTY                 │
                │  tokio::broadcast<Bytes>                       │
                │  Byte log per pane (ring + disk)               │
                │  Pane/tab/workspace metadata                   │
                │  iroh QUIC endpoint (paired devices)           │
                │  Survives window close                         │
                │                                                │
                │  NO libghostty-vt, NO cells, NO colors         │
                └───────────────────┬────────────────────────────┘
                                    │
                   iroh QUIC (same byte protocol, over network)
                                    │
                ┌───────────────────┴────────────────────────────┐
                │     forgetty-android (Kotlin + Rust JNI)       │
                │                                                │
                │  Jetpack Compose UI                            │
                │  VtInstance per pane (libghostty-vt via JNI)   │
                │  Scrollback, cells, colors — all client-side   │
                └────────────────────────────────────────────────┘
```

Every client type (GTK, Android, future Web/Windows) brings its own VT parser and connects to the same daemon over the same wire protocol. The daemon never changes to support a new client kind because it doesn't know anything about rendering.

---

## Data flows

### Keystroke → PTY
```
GTK key event → socket.write(binary frame [InputFrame]) → daemon → pty.write()
```
~0.2 ms end-to-end. Event-driven (no poll).

### PTY output → screen
```
pty.read() → tokio channel wake → daemon broadcast →
    Unix socket (raw bytes in binary frame) →
    GTK glib channel wake → VtInstance.feed(bytes) → queue_draw()
```
~1.5 ms typical. Zero polling, zero encoding, one VT parse.

### Session persistence
```
PTY output bytes → daemon append-only byte log (ring in memory, rotated to disk)
Client cold-start: daemon replays log to new attaching client →
    client VT parser feeds itself → rebuilds screen + scrollback naturally
```
No cell snapshots. The VT parser does what VT parsers do: process bytes in, produce screen state.

### Window close → reopen
```
GTK close → socket.send(Disconnect RPC) → daemon drops this connection, stays alive
GTK launch → ensure_daemon: socket exists → connect → resubscribe →
    daemon replays recent byte log to catch up → seamless
```

### Android pairing + streaming
```
Pairing: QR scan → iroh forgetty/pair/1 ALPN → daemon saves device in registry
Streaming: iroh forgetty/stream/1 ALPN → daemon broadcasts the same raw PTY bytes →
    Android's Rust JNI VT parser feeds itself → renders cells
```
Identical data path as GTK, different transport.

---

## Performance contract

Three hard rules, enforceable in code review:

1. **Zero polling on the hot path.** No `sleep`, no `timeout_add_local`, no periodic wakeup on the PTY → render path. Wakes are event-driven via tokio channels and GLib channels.
2. **Zero encoding of PTY bytes.** Bytes arriving from PTY are the same bytes fed to the client VT parser. `memcpy` only. Frame header is 4 bytes.
3. **Single VT parse per byte.** Parsing happens once, in the client. The daemon never parses ANSI.

Target latency budget (output path, PTY → pixel):
- Event wake: < 0.1 ms
- Socket hop: ~0.1 ms
- Binary frame decode: < 0.1 ms
- GLib channel wake: < 0.1 ms
- VT parse batch: ~0.3 ms
- Cairo/Pango render: ~1–2 ms
- **Total: ~1.5 ms typical, imperceptibly different from in-process terminals on a 60–120 Hz display.**

Idle CPU target: ~0% (no timers firing when nothing is happening).

---

## Security model

- **Identity:** per-device Ed25519 keypair stored at `~/.local/share/forgetty/identity.key`, mode 0600. Never deleted without explicit user action (deletion voids all pairings).
- **Pairing:** out-of-band QR scan. Daemon opens a 60-second pairing window on demand; during that window it accepts one inbound pair handshake. Outside that window, connections from unknown devices get `not_authorized` and are dropped.
- **Authorization:** per-connection lookup against `authorized_devices.json`. Revoking a device cuts all its live streams immediately.
- **Transport encryption:** iroh QUIC (TLS 1.3 under the hood). End-to-end between paired devices, no relay servers can decrypt traffic.
- **Local socket:** Unix socket in `$XDG_RUNTIME_DIR`. Access control comes from the directory (`$XDG_RUNTIME_DIR` is mode `0700` — user-only traversal), not from the socket file's own mode, which is umask-dependent (the daemon does not `chmod` it). Any local process running as the user can connect — intentional (socat, scripts, agents). Fallback path `/tmp/forgetty-*.sock` (used only when `$XDG_RUNTIME_DIR` is unset) is weaker: `/tmp` is `1777`, so other local users could connect.
- **Byte log on disk:** mode 0600. Contains raw PTY output, which may include secrets (passwords, tokens). Respect the same rotation/retention policy as shell history.
- **Input size limits:** every frame has a 4 MiB cap. Line-mode RPCs have a line-length cap. Malformed frames close the connection.

---

## Scalability model

**Per daemon:**
- N panes, each with: one PTY fd, one tokio drain task, one byte log ring buffer.
- Memory: ~2 MB per pane (client-side VT state, scrollback) + ~1 MB per pane byte log.
- CPU: idle ~0%; active proportional to PTY output volume. VT parsing happens in the client, not the daemon, so daemon CPU stays low even under heavy output.

**Per connected client:**
- One subscriber per pane × number of panes the client is viewing.
- `tokio::broadcast` handles fan-out without per-subscriber copies.
- Backpressure: if a client can't keep up, the broadcast channel lags → daemon sends a "catch-up" sequence (replay from byte log) instead of disconnecting.

**Across devices:**
- One iroh connection per paired device (same channel reused for all services as platform grows).
- iroh QUIC supports stream multiplexing; terminal input/output are prioritized streams; future services (clipboard, files) get lower-priority streams on the same connection.

**Workspace scale (user-visible):**
- 1 daemon per window, unbounded windows.
- 1..N workspaces per daemon.
- 1..N tabs per workspace.
- 1..N panes per tab (binary split tree).

---

## What is explicitly **not** the architecture

These concepts have been considered and rejected. Do not reintroduce without explicit re-decision.

| Rejected concept | Why |
|---|---|
| In-process mode (daemon embedded in GTK) | Removes the platform boundary that enables multi-device, survival, and non-terminal services. Solves a 0.5 ms IPC problem at the cost of the entire platform vision. |
| Custodian fork-exec + SCM_RIGHTS fd handback | Achieves daemon survival with Unix black magic when a `disconnect` RPC does it in 20 lines of code. |
| Cell-grid streaming (CellGrid / ScreenDelta) | Moves terminal semantics into the daemon, forcing reimplementation of selection/copy/search against a new data structure. Proven to break in broken-foundation branch. |
| GTK has no VT parser (daemon is the parser) | Same problem as above — inverts responsibility in the wrong direction. |
| Shared memory IPC (memfd + mmap) | 748 lines of Linux-only complexity to save ~0.3 ms. Not worth it. |
| procfs fd sharing for PTY input | Keystroke path is already sub-millisecond; optimization at the wrong bottleneck. |
| Dual-mode (local PTY fallback in GTK) | Doubles the code path in GTK for every feature. The fallback silently degrades the platform into a single-process terminal. |
| GSK render nodes / custom wgpu renderer | Premature. Cairo+Pango is adequate. Revisit after the protocol is stable. |
| Dead crates in the workspace (`forgetty-renderer`, `forgetty-ui`, `forgetty-viewer`) (deleted 2026-04-20 by V2-012) | Unused code that slowed compilation and confused the tech stack. |

---

## Platform extensibility (future)

The daemon is designed so that new services can be added without modifying core crates. A service is any module that communicates between paired devices over the same iroh connection.

### Service contract (preview — exact API finalized when we build the second service)

```rust
trait ForgettyService {
    /// Unique protocol identifier (e.g. "terminal", "clipboard", "portforward").
    const PROTOCOL_ID: &'static str;

    /// Stream priority: 0 = highest, 10 = lowest. Terminal input = 0. Bulk file transfer = 8.
    const PRIORITY: u8;

    /// Register local JSON-RPC methods (on the Unix socket).
    fn register_rpc(&self, router: &mut RpcRouter);

    /// Handle incoming QUIC streams on paired-device connections.
    async fn on_stream(&mut self, stream: QuicStream) -> Result<()>;

    /// Called when a paired device disconnects.
    async fn on_device_gone(&mut self, device: DeviceId) -> Result<()>;
}
```

Priority: we do not build this scaffolding speculatively. The V2-0xx backlog focuses on the terminal. The service trait is extracted after the terminal refactor is stable, when we design the second service (likely clipboard sync).

---

## File and process footprint

### Binaries

| Binary | Purpose |
|---|---|
| `forgetty` | GUI terminal client (GTK4 + libadwaita on Linux). |
| `forgetty-daemon` | Headless byte pipe. One instance per window. |

The QA test binaries (`forgetty-pair-test`, `forgetty-stream-test`) stay feature-gated.

### On-disk layout

```
~/.config/forgetty/
├── config.toml            User-visible config
└── themes/                Optional custom themes

~/.local/share/forgetty/
├── identity.key           Ed25519 private key (0600)
├── authorized_devices.json  Paired devices
├── sessions/              One file per window
│   └── {uuid}.json        WorkspaceState: tabs, pane tree, CWDs, dimensions
└── logs/                  Byte logs for replay
    └── {pane_uuid}.log    Append-only raw PTY output, rotated at size cap

$XDG_RUNTIME_DIR/
└── forgetty-{uuid}.sock   Per-window Unix socket
```

---

## Relationship to the Android app

The Android app is a **sibling client** to the GTK app. Same protocol, different transport:
- GTK uses a local Unix socket because it's on the same machine.
- Android uses iroh QUIC because it's a different device.

Both carry `forgetty-vt` (via JNI on Android) and parse ANSI locally. The desktop daemon treats them symmetrically — it broadcasts raw PTY bytes over the respective transport.

The Android side's current protocol doc (`~/Forge/forgetty-android/docs/protocol/TERMINAL_CONNECTION.md`) is the source of truth for the wire format. The desktop must stay compatible with it.
