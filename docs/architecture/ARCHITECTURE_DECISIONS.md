# Forgetty — Locked Architectural Decisions

> **This file is the canonical list of locked architectural decisions.**
> Every agent, every session, every phase MUST read this file before writing any code or spec.
> Decisions here override anything in older docs if they conflict.
>
> See `ARCHITECTURE.md` for the prose description of the system.
> See `IMPLEMENTATION_STATUS.md` for what's currently implemented.

---

## AD-001: One daemon per window

**Decision:** Each terminal window spawns its own independent daemon process.

- Window 1 → Daemon 1 → its own socket, its own session file, its own workspaces/tabs/panes.
- Window 2 → Daemon 2 → fully independent, no shared state with Daemon 1.
- Opening a new terminal window = spawning a new daemon.
- Two windows never see each other's tabs or layout changes.

**Identifiers:**
- Each daemon socket: `$XDG_RUNTIME_DIR/forgetty-{uuid}.sock`.
- Each daemon session file: `~/.local/share/forgetty/sessions/{uuid}.json`.

**Session restore on launch:** Forgetty reads all `~/.local/share/forgetty/sessions/*.json` files and restores one daemon + window per file.

**Decided:** 2026-04-03. Still locked.

---

## AD-002: Session hierarchy

**Decision:** Each daemon owns this exact hierarchy:

```
Daemon
└── Workspaces (1..N)
    └── Tabs (1..N per workspace)
        └── Panes / Splits (1..N per tab, binary tree)
            └── PTY (leaf node)
```

VT state is **not** part of this hierarchy (see AD-007).

**Decided:** 2026-04-03. Still locked.

---

## AD-003: Session ownership = device that owns the PTY

**Decision:** A session is always owned by the device running the shell.

- PC session: shell runs on PC, files on PC, PTY on PC daemon — PC is the owner.
- Android local session: shell runs on Android, files on Android, PTY on Android daemon — Android is the owner.
- Android viewing a PC session: PTY still owned by PC daemon — Android is just a client.
- Ownership never transfers between devices.

"Owner" means the device whose daemon holds the PTY fd. Clients render what the owner streams.

**Decided:** 2026-04-03. Still locked.

---

## AD-004: Android pairing is asymmetric (like SSH)

**Decision:** Pairing gives Android bidirectional I/O to a PC session, but the PC daemon remains the sole PTY owner.

- Android can type (input flows back) and see output (bytes flow forward).
- Shell, filesystem, processes, and PTY all live on the PC — never move to Android.
- Disconnecting Android has zero effect on the PC session (it keeps running).
- Reconnecting Android gets a fresh replay from the byte log (AD-013).

Conceptually identical to SSH, but using iroh QUIC instead of an SSH daemon.

**Decided:** 2026-04-03. Still locked.

---

## AD-005: Android pairing rules

**Decision:** Android can operate in two modes, but is **never** a pairing host.

**Android modes:**
1. **Local terminal** — Android daemon owns PTYs (like Termux). Shell runs on Android device.
2. **Paired to PC** — Android is a remote client. Connects to a PC daemon via iroh.

Both modes can coexist in the same Android window as different tabs.

**Hard rules:**
- Android NEVER accepts incoming iroh connections from other devices.
- Android-to-Android pairing: NOT ALLOWED.
- Only PC (desktop/server) daemons run an iroh listener and accept incoming client connections.
- iroh listener code is desktop-only. Android build includes only the iroh client side.

**Decided:** 2026-04-03. Still locked.

---

## AD-006: Daemon is single-tenant

**Decision:** Each daemon serves exactly one GTK window (its owner) plus optional paired devices.

- `SessionLayout` does not route by client identity.
- Socket handlers do not need tenant isolation.
- One daemon + one GTK window + N paired devices all view the same session.

**Decided:** 2026-04-03. Still locked.

---

## AD-007: The daemon is a byte pipe, not a terminal

**Decision:** The daemon is a process supervisor and byte transport. It has no terminal semantics.

The daemon owns:
- PTY processes (spawn, read, write, resize, kill).
- The raw PTY output byte stream (broadcast to clients).
- A byte log per pane (AD-013) for reconnect/cold-start replay.
- Pane/tab/workspace metadata: UUIDs, PIDs, rows/cols, CWDs.
- iroh QUIC endpoint and device pairing.
- The Unix-socket JSON-RPC server.

The daemon does **not** own:
- VT parsing (no `forgetty-vt` dependency).
- Screen buffers, cells, or cursor state.
- Color resolution or theme awareness.
- Scrollback semantics beyond the raw byte log.
- Selection, search, URL detection.
- OSC parsing (notifications, titles, CWD — handled client-side, see AD-008).
- Any rendering concern.

**Decided:** 2026-04-13.

---

## AD-008: Clients own terminal semantics

**Decision:** Every client (GTK, Android, future Web/Windows) carries its own VT parser and owns the full terminal experience locally.

Clients own:
- `forgetty-vt` (libghostty-vt) — the ANSI/VT parser.
- Screen buffer (main + alternate), cursor state, tab stops, keypad mode.
- Scrollback (populated by feeding the daemon's byte log + live stream).
- Selection, search, URL hover, copy, grapheme clustering.
- OSC 0/2 title changes, OSC 7 CWD reporting, OSC 9 notifications.
- Color palette, theme, font, glyph rendering.
- Input encoding (keyboard protocol, mouse reporting).

This means a client is a full terminal emulator. It connects to the daemon over the wire protocol (AD-010) and feeds the received bytes into its own VT parser. The fact that bytes arrive over Unix socket or QUIC is an implementation detail — the terminal semantics are identical either way.

**Why:** Keeping VT in the client preserves terminal correctness (selection, queries, alternate screen, wide chars) that was broken by the broken-foundation experiment's cell-streaming approach. One VT parser per client, one parse per byte — no duplicate work.

**Decided:** 2026-04-13.

---

## AD-009: No polling on the hot path

**Decision:** Every byte from PTY to pixel must be delivered via event-driven wakes. No sleep, no periodic timer, no busy loop on the PTY → client data path.

**Concretely:**
- Daemon: PTY reader uses `tokio::sync::mpsc::UnboundedSender`; drain task wakes on `rx.recv().await`. **No** `tokio::time::sleep` in the drain path.
- GTK: Daemon output arrives via a tokio broadcast; the GTK thread subscribes via `glib::MainContext::channel()` and the GLib main loop wakes on data. **No** `glib::timeout_add_local` on the output path.
- Cursor blink and similar UI timers are not on the hot path — those timers are fine.

**Target:** idle CPU ~0%. Keystroke-to-pixel latency ~1.5 ms typical.

**Decided:** 2026-04-13.

---

## AD-010: Wire protocol is raw PTY bytes in length-prefixed binary frames

**Decision:** Bytes flow over the wire unchanged. No base64, no JSON wrapping, no cell structures.

**Frame format (both local Unix socket streaming and iroh QUIC):**
```
[ 4 bytes: u32 big-endian length ][ N bytes: payload ]
```

Maximum frame size: **4 MiB**. Frames exceeding this close the connection.

Payload types are defined per stream/ALPN:
- Output stream: raw PTY bytes (payload is exactly what came out of the PTY).
- Input stream: raw bytes to feed into the PTY.
- Control stream: MessagePack-serialized JSON-RPC method calls (for non-streaming RPCs like `list_tabs`, `resize_pane`, `disconnect`).

**Why:** `memcpy` only on the hot path. The frame header is 4 bytes. Zero encoding overhead.

**Android compatibility:** the existing Android wire protocol already uses length-prefixed MessagePack frames over iroh (see `~/Forge/forgetty-android/docs/protocol/TERMINAL_CONNECTION.md`). Desktop local-socket streaming adopts the same framing.

**Decided:** 2026-04-13.

---

## AD-011: Daemon always runs — no local-PTY fallback in GTK

**Decision:** The GTK client always connects to a daemon. There is no in-process PTY fallback.

- `ensure_daemon()` spawns a daemon if none exists, then connects.
- If the daemon binary is missing, or fails to start, the GTK client reports the error and exits. It does **not** silently run the PTY itself.
- `TerminalState` never holds a `PtyProcess` directly.
- There is exactly one `create_terminal()` code path.

**Why:** The two-mode design doubled the code path in GTK (4252-line `terminal.rs` in v0.1) and every feature had to be implemented twice or diverged (see v0.1 BUG-005). A platform architecture cannot silently degrade into a single-process terminal.

**Decided:** 2026-04-13.

---

## AD-012: Daemon survives window close

**Decision:** Closing the GTK window does **not** kill the daemon. The daemon keeps the shell process and its children alive.

**Mechanism:**
- Window close → GTK sends `disconnect` RPC → daemon persists state, drops this connection, continues running.
- `shutdown_clean` (save then exit) remains available for explicit "close permanently" actions from the user menu.
- Daemon self-exits only when: (a) all panes have ended naturally, and (b) no client has been attached for the configured idle grace window; or (c) explicit user-initiated shutdown.

**Relaunch flow:**
- GTK launches → probes socket → finds live daemon → attaches → daemon replays recent byte log for instant catch-up.

**Decided:** 2026-04-13.

---

## AD-013: Persistence = byte log, not cell snapshot

**Decision:** The daemon persists raw PTY output bytes. It does **not** persist cells, screen state, or any parsed representation.

**Format:**
- One append-only file per pane: `~/.local/share/forgetty/logs/{pane_uuid}.log`.
- Raw bytes, no framing (this is a log, not a wire).
- Rotated at a configurable size cap (default 10 MiB per pane). Older bytes discarded FIFO.
- In-memory ring buffer (default 1 MiB) for fast catch-up without disk read.

**Replay on client attach:**
- New subscriber opens output stream → daemon writes the last N bytes of the log as the initial frames → then streams live output.
- The client's VT parser processes replay bytes and live bytes identically. No snapshot format, no client-specific reconstruction logic.

**Why:** The VT parser does what VT parsers do: process bytes in, produce screen state. Rebuilding the screen from bytes is the natural, correct operation. No cell serialization format to version. No snapshot/replay divergence bugs.

**Decided:** 2026-04-13.

---

## AD-014: Colors resolved at render time in the client

**Decision:** The client stores palette indices and resolves to RGB at render time using the currently-active theme.

- Theme changes are client-local: swap the palette, rerender → all visible cells update.
- For scrollback: when the theme changes, trigger a re-parse of the byte log with the new palette → scrollback updates retroactively.
- The daemon has no knowledge of themes or colors. It ships raw bytes; palette information is carried in the bytes themselves (as ANSI escape sequences).

**Why:** Themes are a display concern, not a data concern. Resolving at parse time (as v0.1 does) freezes colors into the scrollback and makes theme changes inconsistent.

**Decided:** 2026-04-13.

---

## AD-015: `forgetty-sync` is transport-only; no terminal dependencies

**Decision:** `forgetty-sync` must compile and function without any dependency on `forgetty-session`, `forgetty-vt`, or `forgetty-pty`.

Responsibilities of `forgetty-sync`:
- iroh endpoint lifecycle, Ed25519 identity, ALPN routing.
- Device pairing protocol and device registry.
- QUIC stream multiplexing.

Everything terminal-specific (the stream payload, the RPC schema, byte-log replay) is implemented by the terminal as a **consumer** of `forgetty-sync`, via a thin adapter crate or by wiring at the binary level.

**Why:** The platform vision (future services: chat, clipboard sync, file transfer, port forwarding) requires the P2P transport layer to be usable independently of the terminal. This decision prevents accidentally coupling new services to the terminal via the transport.

**Decided:** 2026-04-13. Implementation tracked as V2-011.

---

## AD-016: Unpinned sessions exit on clean close; pinned sessions persist; daemon does not survive across launches

**Decision:** The daemon's survival scope is bounded by pin state.

- **Unpinned, clean close:** the daemon moves `sessions/active/{uuid}.json` →
  `sessions/trash/{uuid}.json`, then exits. The GTK client shows an "Undo Close"
  toast on any surviving sibling window for 30 seconds. Files in `trash/` are
  retained indefinitely (no new auto-purge in v1) and are recoverable via
  `--restore-session UUID`.
- **Pinned, clean close:** the daemon moves `sessions/active/{uuid}.json` →
  `sessions/{uuid}.json`, then exits. The session is restored on next launch.
- **Daemon never survives window close.** The warm-reattach path (daemon already
  running, client reconnects) is removed. Every relaunch goes through
  `ensure_daemon` (cold spawn, ~50–100 ms).
- **Crash recovery:** any `sessions/active/{uuid}.json` present at startup is
  an orphan (the daemon did not exit cleanly). If `pinned: true`, promote to
  `sessions/{uuid}.json` and restore. If `pinned: false`, delete.
- **`--temp` mode** (process-level ephemeral, no daemon at all) is orthogonal
  and unchanged.

**Trade-off accepted:** ~50–100 ms cold spawn per relaunch vs. indefinitely-surviving
daemons consuming memory for sessions the user considers closed. Cold spawn cost is
below human perception threshold and eliminates the stale-daemon failure mode
(FIX-007).

**Decided:** 2026-05-04. Amends AD-012.

---

## AD-017: One durable server per user (supersedes AD-001, AD-012, AD-016)

**Decision:** `forgetty-server` is one long-lived process per Linux user, run by
`systemd --user` with linger. It holds every workspace, tab and pane for that
user. A client disconnect never ends a session. A session ends only when a
client closes it or the server stops.

- Clients (Windows, Android, CLI) attach and detach freely. Many clients can
  attach at the same time.
- The pinned/trash lifecycle of AD-016 no longer applies to live sessions.
- Server restart: layout is restored and byte logs are replayed. Processes do
  not survive a server restart.
- The stale-daemon problem (FIX-007) is solved by AD-018 version negotiation,
  not by killing the server.

**Decided:** 2026-10-21.

---

## AD-018: Protocol v2 with version and capability negotiation (amends AD-010)

**Decision:** Remote clients speak protocol v2 on iroh ALPN `forgetty/rpc/2`.
The first message on a connection is `Hello { proto_version, capabilities }`
from each side. A peer ignores capabilities it does not know and degrades.
An unknown major version gets a clear error, never a silent failure.

- One QUIC bi-stream carries control (every socket method plus layout events).
- Each pane subscription gets its own QUIC stream. Input goes on its own stream.
- Framing stays `[u32 BE len][payload]`, 4 MiB cap. Payload is MessagePack.
- Every pane stream carries a byte offset. Reconnect resumes from the client's
  last offset (see V3_PLAN 7.2).
- Messages live in the `forgetty-proto` crate, shared by server and clients.
- `forgetty/stream/1` stays for one release for old Android builds, then goes.

**Decided:** 2026-10-21.

---

## AD-019: Server may parse on the cold path and scan OSC status on the hot path (amends AD-007)

**Decision:** The hot path stays a byte pipe with no VT state. Two exceptions:

- An OSC scanner (a byte-level state machine, not a VT parser) runs on PTY
  output and extracts OSC 7501 / 9 / 99 / 777 / 0 / 2 / 7. Cost target: under
  1 % of drain time.
- `capture` builds a VT instance from the byte-log ring on request only, then
  drops it.

**Decided:** 2026-10-21.

---

## AD-020: PTY size policy is "latest active client wins"

**Decision:** The PTY size follows the client that sent input or focus most
recently. Other attached clients render the pane at that size and letterbox.

**Decided:** 2026-10-21.

---

## AD-021: Pairing needs a one-time secret (amends AD-004)

**Decision:** The pairing QR carries a random 128-bit one-time secret. The
pairing window is 60 s and accepts one device that presents the secret. A
pairing request without the right secret is refused. The server logs the
paired device name and id.

**Decided:** 2026-10-21.

---

## AD-022: Windows client is Rust native on GPUI; GTK client is paused

**Decision:** The Windows client is a Rust application on GPUI, pinned to one
git revision, sharing `forgetty-proto`, `forgetty-vt` and `forgetty-client`
with Android. Compose Multiplatform Desktop is rejected (see V3_PLAN 7.1).
The GTK client is paused: it leaves the default workspace build and gets no
new work.

**Decided:** 2026-10-21.

---

## AD-023: A slow client never slows a program; it catches up by snapshot

**Decision:** Only the server's own byte-log writer may hold a PTY (disk queue
full). A remote client's speed never holds a PTY. A client that falls behind
more than half the ring (or stays stalled for 2 s) gets its pane stream
replaced by a snapshot catch-up: the server builds a VT instance from the log
on the cold path (AD-019), formats the current screen and modes as VT bytes,
and sends them with `reset = true` at the current offset, then live bytes. The
client resets that pane and feeds the snapshot. Scrollback older than the
snapshot is fetched on demand.

**Why:** a phone on a weak network attached to a build pane must not slow the
build or the laptop's view of it.

**Decided:** 2026-10-21.

---

## AD-024: Input is ordered per pane; a hung pane never blocks other panes (amends protocol v2 sections 5 and 8)

**Decision:**
- Each pane has its own bounded writer queue (256 KiB) and writer thread.
- The server records a frame's `seq` as done when the frame enters that
  pane's queue. It does not wait for the PTY write. `input_acked` follows the
  enqueue.
- Order is kept per pane, in arrival order. There is no ordering across panes.
- When a pane's queue is full, the server drops new frames for that pane and
  counts and logs them. If a dropped frame holds the pane's VINTR byte, the
  server sends SIGINT to the pane's foreground process group.
- One pane that does not read never delays a client's input to other panes,
  Control or Pong.
- Graceful stop closes the queues, waits up to 2 s for them to drain, and then
  saves the dedup state. Input never crosses a restart: processes do not
  survive it (AD-017), so a resent frame would type into a fresh shell. The
  server carries no unwritten frames over. `Welcome.boot_id` (new for each run)
  tells a client that the server restarted, and the client drops the input it
  had not seen acked and shows the user how many bytes were not sent.
- There is one ack counter per client epoch (`Pong.input_acked`), not one per
  pane. Because a dropped frame is acked, the server must not let later input
  of that client reach the same pane with a hole in front of it: after the
  first dropped frame for a (client, pane), it drops every later frame of that
  client for that pane until the pane's queue is empty. It then sends that
  client one `InputDropped { pane_id, bytes }` event (capability
  `input_flags`).
- An interrupt (a VINTR byte that the pane's tty would turn into SIGINT, or
  `send_sigint`) also drops the input queued for that pane, what the writer
  has half written, and what the tty holds unread, so a stale paste does not
  run in the shell after Ctrl+C. The server signals the foreground group
  itself when VINTR cannot go in order (the queue is full, in a hole, or the
  writer is busy). VINTR counts only while ISIG is on and VINTR is set.
- The writer thread writes through a plain duplicate of the master fd. A
  library writer that sends `\n` and EOF on drop is never used: it would make
  a shell run what is half typed at its prompt when the pane or the server
  closes.
- A fresh attach (`from_offset = None`) to a pane whose log holds more than
  half a ring starts with a snapshot (AD-023), not at the oldest byte.

**Why:** a client must not lose typing in every pane because one pane runs a
program that never reads stdin. A program that does not read 256 KiB of input
is hung, and Ctrl+C must still reach it. A fresh attach must paint fast.

**Decided:** 2026-10-21.

---

## AD-025: Only the size owner's terminal replies reach the PTY (amends AD-007 and AD-020)

**Decision:**
- Every client keeps its VT at the PTY size (AD-020 letterbox) and answers
  terminal queries, as AD-007 says.
- An input frame carries a flag: `user` (keys, paste, mouse) or `auto` (VT
  replies such as DA, CPR and kitty-keyboard answers, and focus reports).
  Clients send the flag only when both sides list the capability
  `input_flags`.
- An `auto` frame never makes its client the latest active client.
- The server writes an `auto` frame to the PTY only when it comes from the
  pane's size owner. If the pane has no owner yet, the first attached client
  counts as owner. The server drops `auto` frames from every other client, and
  still acks them.
- When the owner leaves, the client among those still attached whose input
  came most recently becomes the owner.

**Why:** with several clients attached, every VT answers the same query.
Without this rule, an idle window takes the PTY size from the phone, and the
extra answers show up as typed text at the prompt. The server cannot answer
queries itself, because CPR needs full VT state, which AD-019 keeps off the
hot path.

**Decided:** 2026-10-21.

---

## Superseded / rejected decisions

The following concepts are **explicitly rejected**. Do not reintroduce without a new AD that supersedes the rejection.

### Rejected in the v0.2 dual-mode plan (never shipped)

| Concept | Why rejected |
|---|---|
| In-process mode (embed daemon inside GTK) | Removes the platform boundary that enables multi-device, survival, and non-terminal services. Trades a 0.5 ms IPC hop for the entire platform vision. Inverts AD-007. |
| Custodian fork-exec + SCM_RIGHTS fd handback | Achieves daemon survival via Unix black magic when a `disconnect` RPC does it in 20 lines. See AD-012. |
| Dual-mode (local PTY + daemon-backed) | Doubles the GTK code path. See AD-011. |
| Binaries renamed `forgetty-next` | Dev-isolation measure for the dual-mode branch; not the shipping design. |

### Rejected in the broken-foundation experiment (reverted)

| Concept | Why rejected |
|---|---|
| Cell-grid streaming (`ScreenDelta`, `CellGrid`) | Moves terminal semantics into the daemon. Forced reimplementing selection/copy/search against a new data structure. Proven to break in practice (3 immediate regression commits after T-079). |
| GTK has no VT parser (daemon is sole parser) | Inverts AD-008. Root cause of broken-foundation failure. |
| Viewport-scroll dance for scrollback extraction | Fragile workaround for libghostty-vt's missing scrollback API. Exclusive VT access required for the whole scroll, resets generation counters, interacts badly with concurrent output. |
| Shared-memory IPC (memfd + mmap + eventfd) | 748 lines of Linux-only code to save ~0.3 ms. Six follow-up hardening fixes required. Socket IPC is already sub-millisecond. |
| procfs fd sharing for PTY input | Optimization at the wrong bottleneck. Input path is already fast. |
| GPU renderer (GLArea + custom shaders) | Premature. Cairo+Pango is adequate until the protocol is stable. |
| Triple IPC fallback path (shmem → binary → JSON) | Over-engineering. Every path needs its own error handling. |

### Superseded from M4-era decisions

| Old decision | Where | New decision |
|---|---|---|
| "Multiple GTK windows attach to the same daemon simultaneously" | DAEMON_SYNC_ARCHITECTURE.md | Superseded by AD-001 (one daemon per window) in 2026-04-03. |
| Old AD-007: "libghostty-vt is per-pane only" in the daemon | (This file, pre-2026-04-13) | Superseded by AD-007 (daemon has no VT) + AD-008 (VT lives in client). libghostty-vt is still per-pane, but only the client links it. |
| Old AD-008: "GTK is a stateless renderer, no terminal semantics" | (This file, pre-2026-04-13) | Superseded by AD-008 (clients own terminal semantics). The M4-era intent was "GTK doesn't own layout state"; that part is still true. What changed: GTK owns VT parsing. |
