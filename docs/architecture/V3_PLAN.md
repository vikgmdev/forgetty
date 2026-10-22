# Forgetty V3 — Durable Server Plan

> **Status:** APPROVED by Vick 2026-10-21 (autonomous execution). ADs 017-022
> in `ARCHITECTURE_DECISIONS.md` are locked. Where this plan and older docs
> conflict, this plan wins.

## 1. Goal

Turn Forgetty into a durable terminal server on Linux with remote clients on
Windows and Android, using the ideas Superlogical Rex got right, without
Tailscale. iroh QUIC plus QR pairing stays the only network path.

Target topology:

```
Windows laptop (client)      Android phone (client)
        \                        /
         iroh QUIC, Ed25519 keys, QR-paired
                    |
      forgetty-server (Linux, OVH, systemd user service)
        PTYs · byte logs · layout · OSC status scanner · local CLI socket
```

GTK client: paused. It keeps compiling (`cargo check`), but no new work.

## 2. What stays

| Piece | Why it stays |
|---|---|
| AD-007/008 split: server is a byte pipe, clients parse with libghostty-vt | Same design Rex uses. Proven fast. |
| AD-009/010: no polling, raw bytes in `[u32 len]` frames, 4 MiB cap | Wire stays cheap. |
| AD-013 byte-log persistence + replay on attach | Reconnect and cold restore already work. |
| AD-005: clients never host | Laptop and phone are clients only. |
| `forgetty-pty`, `forgetty-session`, `forgetty-workspace`, `forgetty-config`, `forgetty-core` | Server core, no change of role. |
| `forgetty-sync`: Ed25519 identity, QR pairing, device registry, revoke, `register_alpn` | The differentiator against Rex (no Tailscale, no accounts). |
| `forgetty-vt` (libghostty-vt via Zig) | Every client. Windows target is in `CROSS_COMPILE.md`, not yet built. |
| Local Unix-socket JSON-RPC (`docs/socket-api.md`) | Becomes the CLI's transport on the server. |
| Android app: Compose UI, JNI VT, Room scrollback cache, multi-machine, foreground `ConnectionService` | Client already exists. It moves to protocol v2. |

## 3. What changes or gets added

| # | Rex idea | Forgetty today | V3 change |
|---|---|---|---|
| 1 | Long-lived server, sessions outlive every client | One daemon per window (AD-001), exits on close (AD-016) | One `forgetty-server` per user under `systemd --user` with linger. Sessions end only when closed or the server stops. |
| 2 | Forward/backward API compatibility, restart rarely needed | No version handshake. Stale daemons caused FIX-007. | `hello` handshake: protocol version + capability list. Unknown capability means degrade, not fail. Replaces the "kill daemon on close" fix. |
| 3 | Any client gets full control, many clients at once | iroh wire (`forgetty/stream/1`) has 5 messages: list, subscribe one pane, input, scrollback. No resize, no layout. | Protocol v2 on `forgetty/rpc/2`: the full socket method set over one QUIC control stream, one QUIC stream per pane subscription. Shared `forgetty-proto` crate for server, Android, Windows. |
| 4 | Clients size the PTY | GTK sizes PTYs | Size policy "latest active client wins" (tmux `latest`). Other clients letterbox. |
| 5 | Server can read a screen with no client (`rex capture`) | Removed (`get_screen` is METHOD_NOT_FOUND) | Cold-path `capture`: parse the byte-log ring with forgetty-vt on request only. Hot path stays parse-free. |
| 6 | Program Status Protocol (OSC 7501), status without an attached client | Clients scan OSC 9/99/777 | Server runs a byte scanner (not a VT parser) for OSC 7501/9/99/777. Status goes out on layout events and to notifications. |
| 7 | CLI for people and agents: `ls new split send send-key capture run --wait wait events` | `forgetty-daemon` flags for pairing only | `forgetty` CLI on the server over the local socket. Exit codes like Rex (0/1/2/3/4). `--json` on every command. |
| 8 | Self-describing API: `api list/describe/call` with JSON Schema | Hand-written `socket-api.md` | `schemars` schemas per method, served by `api.describe`. The doc is generated. |
| 9 | Event stream (`rex events`) | `subscribe_layout` (missing pane_closed, P-010) | `events` stream with typed events, fixes P-010. |
| 10 | Agent skill | none | `forgetty skill setup` writes a Claude Code skill that uses the CLI. |
| 11 | Pairing must prove who is connecting | QR holds node id, hostname, relay. No secret. `--allow-pairing` accepts the next unknown device. | QR carries a one-time secret. Pairing window is 60 s and single-use. Server prints the paired device name for confirmation. |
| 12 | Packaging: `.deb`, systemd unit, install script, channels | `.deb` for the GTK app | Server `.deb` with a user unit. Windows installer. Android APK as today. |

Skipped Rex ideas: Lua config and scripting, PAM login server, shared multi-user
server, built-in Tailscale, live session sharing, macOS client.

## 4. Architecture decisions to write

| AD | Decision | Effect |
|---|---|---|
| AD-017 | One durable server per user. Supersedes AD-001 and AD-016. | Pinned/trash lifecycle goes away for live sessions. Crash recovery restores layout and replays logs (processes do not survive a server restart). |
| AD-018 | Protocol v2 with version and capability handshake. `forgetty/stream/1` stays one release for old Android builds, then goes. | Amends AD-010 (framing unchanged). |
| AD-019 | Server may parse on the cold path (`capture`) and scan OSC status on the hot path. No VT state on the hot path. | Amends AD-007. `ldd forgetty-server` shows libghostty-vt. |
| AD-020 | Size policy: latest active client wins. | New. |
| AD-021 | Pairing needs a one-time secret from the QR. | Amends AD-004. |
| AD-006 | Single-tenant stays: one Linux user, many clients. | Unchanged. |

## 5. Phases

Each phase ends with a behaviour gate that a script or a real device checks.
A green build is not a gate.

### P0 — Groundwork
- Write AD-017 to AD-021.
- Move the server into its own package (`forgetty-server`) with no GTK
  dependency. The `forgetty` GTK bin and `forgetty-gtk` leave the default build
  members.
- Build and run on the OVH box under `systemd --user` with linger.
- **Gate:** the current Android app pairs with and streams from the headless
  server on OVH.

### P1 — Durable server
- One server, many workspaces. Client disconnect never ends a session.
- `hello` handshake with version and capabilities.
- QR one-time secret (AD-021).
- Minimal CLI: `ls`, `new`, `send`, `capture`, `server status`, `server stop --if-idle`.
- **Gate:** start `sleep 600; echo done` from the phone, kill the app, reconnect
  after 2 min, see the program still running. Restart the server, see the
  layout and scrollback restored.

### P2 — Protocol v2 and shared crate
- `forgetty-proto` crate: messages, framing, handshake, MessagePack encoding.
- Full control plane on `forgetty/rpc/2`: tabs, splits, close, focus, resize, layout events.
- Size policy AD-020.
- Android moves to v2 through the shared crate (its private copy in
  `rust/src/sync_client.rs` goes).
- **Gate:** two clients attached to one pane, each can split, resize and type.
  A round-trip test pins the wire bytes.

### P3 — Windows client
- Spike first, time-boxed to one week: build forgetty-vt for
  `x86_64-windows-gnu`, render one live pane with the chosen UI stack
  (decision 1), measure keystroke-to-pixel latency.
- MVP: pairing by paste (no camera on Windows: the QR is on the laptop's own
  screen, so the user copies the `forgetty pair` output and the dialog takes
  it, whole, from the clipboard or Ctrl+V), machine list, workspaces, tabs,
  splits, input with full keyboard protocol, scrollback, clipboard (OSC 52
  write only, per-machine policy), IME, toast notifications.
- **Gate:** Vick daily-drives the Windows client for one week with no blocker bug.

### P4 — Agent layer
- Server OSC scanner (AD-019). Status in layout events.
- Notifications: Windows toast, Android notification with the app closed.
- Cold-path `capture` with trim and line range.
- **Gate:** Claude Code waits for approval in a pane, the phone shows a
  notification while the app is in the background, a reply typed from the
  notification reaches the pane.

### P5 — CLI and self-describing API
- Full CLI set (section 3, rows 7 to 10). Generated `socket-api.md`.
- **Gate:** a Claude Code session on the server opens a tab, runs tests, waits
  for exit, reads the screen, all through the CLI.

### P6 — Packaging
- Server `.deb` with user unit, install script with channels.
- Windows installer and auto-update.
- **Gate:** clean VM install from the script, pair, use.

P3 can start after P2. P4 and P5 can run in parallel with P3.

## 6. Risks

- **Server sits on the prod VLAN.** A paired device gets a shell as the server
  user, with VLAN reach to prod Postgres, Redis and k3s. Run as an unprivileged
  user, never root. Ship AD-021 before the server listens on iroh in daily use.
- **libghostty-vt on Windows is untested here.** The P3 spike answers it before
  MVP work starts.
- **Android background delivery.** The foreground service keeps a socket open
  and costs battery. If P4 shows missed notifications, choose FCM or a
  self-hosted UnifiedPush/ntfy channel (decision 3).
- **Rex ships Linux server and more clients.** Forgetty's lasting edge: open
  source, Windows and Android clients, P2P with no Tailscale and no accounts.

## 7. Locked choices (2026-10-21)

### 7.1 Windows client: Rust native on GPUI, not Compose Desktop

Compose Multiplatform Desktop is rejected for the Windows client. Reasons:
JVM cold start (about 1 s), GC pauses on the render path, Skia text that does
not match ClearType/DirectWrite, weak TSF IME support, 200-400 MB RSS.

Choice: a Rust client built on GPUI (Zed's UI framework, Win32 windowing,
DirectWrite text, GPU rendering), pinned to one git revision. The terminal
grid is a custom GPUI element: glyph atlas, damage-only redraw, one draw per
frame, driven by libghostty-vt render state. Text input goes through GPUI's
IME path (TSF). Shared with Android: `forgetty-proto`, `forgetty-vt`, and a
new `forgetty-client` crate (iroh connection, resume, pane state). Builds and
smoke tests run on GitHub Actions `windows-latest`.

Fallback if GPUI blocks a gate: winit + wgpu with the same client crates.

### 7.2 Connection speed and reliability rules

1. Direct UDP first. The OVH server has a public IP, so iroh direct paths are
   the normal case. A self-hosted `iroh-relay` on the server is the fallback
   relay, not the n0 public relays.
2. Resume, never replay in full. Every pane stream carries a byte offset.
   On reconnect the client sends its last offset per pane and the server sends
   only the missing bytes from the byte log. Full replay happens only when the
   offset is older than the log.
3. Reconnect in under 1 s after a network change: QUIC keepalive 5 s, idle
   timeout 30 s, immediate redial with backoff capped at 2 s, connection
   migration on Wi-Fi/LTE switch.
4. Input never waits on output: input goes on its own QUIC stream, so a
   large output burst cannot delay a keystroke.
5. Backpressure without loss: a lagging client gets a resume from its offset,
   never a dropped connection.
6. Latency budget, keystroke to pixel, server on the same LAN: under 5 ms
   added by Forgetty. Over WAN: network RTT plus under 5 ms.
7. Every gate that claims "fast" or "reliable" measures it: a latency probe
   and a reconnect test in CI.

### 7.3 Open item

Phone push channel when the app is closed: decide at P4 with data from the
foreground service.
