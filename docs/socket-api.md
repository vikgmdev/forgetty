# Forgetty Socket API

Forgetty exposes a JSON-RPC 2.0 + binary-streaming control plane over a per-window
Unix domain socket. This reference enumerates every method the daemon actually
serves, the exact socket path it binds, the framing used by streaming methods,
and a `socat` walkthrough that works against a running daemon.

The source of truth for method names is
`crates/forgetty-socket/src/protocol.rs`; for per-method behaviour,
`crates/forgetty-socket/src/handlers.rs` and
`crates/forgetty-socket/src/server.rs`. This doc summarises them. An
appendix at the end includes a grep cross-check that QA runs to keep the doc
honest against the code.

---

## 1. Overview

**What this is.** The local control plane for a single Forgetty daemon
(`forgetty-daemon`). It accepts newline-delimited JSON-RPC 2.0 requests and,
for the streaming methods, transitions into a length-prefixed binary frame
mode for raw PTY bytes.

**What this is NOT.**

- **Not the iroh QUIC transport.** Cross-device traffic (Android, future web
  clients) rides on a separate protocol described in the Forgetty Android
  protocol doc. That protocol uses the same `[u32 BE length][payload]`
  framing shape but different payload rules (MessagePack vs. raw bytes).
- **Not a stable public API.** Forgetty is pre-1.0; method names, param
  fields, and error messages may change between releases. There is no
  `version` method and no protocol negotiation. Scripts that depend on the
  wire format should pin a Forgetty release.

**Who this is for.** Agents, shell scripts, and local tooling that drive
Forgetty via `socat`/`ncat` or a thin client library. Also developers on
sibling clients (GTK, Android) cross-referencing local vs. remote wire.

**Linux-only.** The daemon binds a Unix domain socket. There is no TCP
listener and no non-Linux path. `forgetty-daemon` does not build or run
outside Linux.

**One daemon per window — each GUI window has its own socket.** Each
GTK window spawns its own independent daemon process with its own socket,
session UUID, and on-disk session file. Two GUI instances never see each
other's tabs. (This is AD-001 in `docs/architecture/ARCHITECTURE_DECISIONS.md`
for anyone tracking the architecture history.)

**Maintainers:** Appendix A at the end of this doc contains a shell
one-liner that cross-checks every `"method"` string in this doc against
the `protocol.rs` constants. Run it before merging any PR that edits this
file.

---

## 2. Connection

The daemon binds to
`$XDG_RUNTIME_DIR/forgetty-{session_uuid}.sock`,
falling back to `/tmp/forgetty-{session_uuid}.sock` when `XDG_RUNTIME_DIR`
is unset.

Access control is provided by the **parent directory**, not by the socket
file's own mode. `$XDG_RUNTIME_DIR` is mode `0700` per the XDG Base
Directory spec — only the owning user can traverse into it, so only the
owner can reach the socket. The daemon does not `chmod` the socket file
itself; its mode is whatever `UnixListener::bind` produces under the
current umask (typically `0775` or `0664`, *not* `0600`). Any local
process running as the user can connect — this is intentional (it is what
enables `socat`, scripts, and agents to drive the daemon). The socket has
no network exposure.

**`/tmp` fallback is weaker.** When `$XDG_RUNTIME_DIR` is unset (rare;
common on some non-systemd setups), the socket lives in `/tmp`, which is
mode `1777` — sticky but world-traversable. Any local user on the machine
could connect to the socket in that configuration. Ensure `XDG_RUNTIME_DIR`
is set in your environment (systemd user sessions and most desktop
environments set it automatically) so the production path is used.

`{session_uuid}` is a v4 UUID generated at daemon startup (or passed via
`--session-id`). The daemon logs its full socket path on startup:

```
INFO forgetty-daemon started, socket at /run/user/1000/forgetty-<uuid>.sock
```

### Discovery

```sh
# Enumerate running daemons for this user:
ls -t "$XDG_RUNTIME_DIR"/forgetty-*.sock

# Pick the most recent:
SOCK=$(ls -t "$XDG_RUNTIME_DIR"/forgetty-*.sock | head -n1)
```

### Overriding the path

`forgetty-daemon --socket-path <PATH>` overrides the default location.
Parent directories are created on first bind; any stale socket file at the
target path is removed before re-binding.

### Legacy path (tests only)

`SocketServer::new()` — constructed without a session UUID — binds to
`$XDG_RUNTIME_DIR/forgetty.sock`. This constructor is used only by
`forgetty-socket`'s round-trip unit test; the production daemon always
takes the `{session_uuid}` path described above. Do not rely on the legacy
path from scripts.

---

## 3. Protocol overview

The protocol has two modes on the same socket:

- **Line-JSON mode** (default): newline-terminated JSON-RPC 2.0 requests
  from client; newline-terminated JSON-RPC 2.0 responses from daemon.
- **Binary-frame mode**: `[u32 BE length][payload]` frames of raw bytes.
  Only reachable after a successful `subscribe_output` ack, for the
  remaining lifetime of that connection.

Most methods are request/response and stay in line-JSON mode forever.
`subscribe_layout` stays in line-JSON mode but emits JSON-RPC *notifications*
(no `id`) after the ack — it never switches to binary mode.

### State transitions

1. A new connection starts in **line-JSON mode**. The client sends
   `\n`-terminated JSON-RPC requests; the daemon replies with `\n`-terminated
   JSON-RPC responses.
2. Most methods stay in line-JSON mode forever. The client may pipeline
   more requests on the same connection.
3. A successful `subscribe_output` response (the `{"ok":true}` ack) transitions
   the connection to **binary-frame mode** for its remaining lifetime. After
   the ack, the daemon stops reading the client's write half — any bytes the
   client sends are dropped unread. The only way to end the stream is to close
   the socket.
4. A successful `subscribe_layout` response stays in **line-JSON mode**; the
   daemon emits JSON-RPC notification lines. After the ack, the connection
   is **server-to-client only** — the daemon does NOT accept further RPCs on
   that connection. Any write activity from the client (including the half-
   close that produces EOF) terminates the subscription. To issue additional
   requests while subscribed, open a second connection.
5. `disconnect`, `shutdown`, `shutdown_save`, and `shutdown_clean` are
   terminal RPCs: the daemon writes the response line, then either closes the
   connection (`disconnect`) or exits the process (`shutdown*`).

---

## 4. Streaming framing

When the connection is in binary-frame mode, every frame on the wire is:

```
[ 4 bytes: u32 big-endian length N ][ N bytes: payload ]
```

- **Maximum payload:** 4 MiB (`4 * 1024 * 1024` bytes). The cap is defined
  as `MAX_FRAME_SIZE` in `crates/forgetty-socket/src/framing.rs`. A length
  prefix exceeding it is rejected before any payload is allocated, and the
  daemon closes the connection.
- **Zero-length frames** (length prefix `0x00000000`, no payload bytes) are
  valid in the wire format and readers MUST tolerate them. The daemon does
  not currently emit zero-length frames — `subscribe_output` explicitly
  skips empty PTY reads.
- **Identical framing** to the iroh QUIC transport
  (`crates/forgetty-sync/src/stream.rs`). The two codecs share the same
  `MAX_FRAME_SIZE` and length-prefix shape; they differ in payload
  semantics. Iroh frames carry a MessagePack-encoded enum with a message
  type discriminator, so a single stream can multiplex different message
  kinds. Local-socket binary frames carry raw PTY bytes with no in-band
  discriminator — the stream type is established once by the
  `subscribe_output` ack, and every subsequent frame's payload is raw
  PTY bytes for that pane. See AD-010.

---

## 5. Method reference

All methods below take a JSON-RPC 2.0 request of the form:

```json
{"jsonrpc":"2.0","method":"<wire_name>","params":{...},"id":1}
```

and return a response of the form:

```json
{"jsonrpc":"2.0","result":{...},"id":1}
```

On failure, `result` is absent and `error` is present: see §6.

Pane and tab identifiers are UUID v4 strings. Invalid UUIDs, missing
required params, and unknown pane/tab IDs all return `-32602`
(Invalid params).

### 5.1 Session layout

#### `list_tabs`

Return the daemon's flat pane list — one entry per live pane, across all
workspaces. Despite the name, this is a pane list, not a tab list; prefer
`get_layout` for workspace/tab hierarchy.

**Request:** `{"jsonrpc":"2.0","method":"list_tabs","id":1}`

**Params:** none.

**Result:**
```json
{"tabs":[{"pane_id":"<uuid>","pid":12345,"rows":24,"cols":80,
         "cwd":"/home/user","title":"zsh"}]}
```

#### `get_layout`

Return the full `SessionLayout`: workspaces → tabs → pane tree.

**Request:** `{"jsonrpc":"2.0","method":"get_layout","id":1}`

**Params:** none.

**Result:** a `SessionLayout` object. See
`crates/forgetty-session/src/layout.rs` for the field shape. Shape:
```json
{"workspaces":[{"name":"Default","tabs":[{"id":"<tab_uuid>",
 "pane_tree":{"Leaf":{"pane_id":"<pane_uuid>"}}}]}]}
```

The `pane_tree` is a tagged enum serialized by serde with three variants.
`Leaf` is a single pane and carries a `pane_id`. `HSplit` and `VSplit`
are internal nodes with `left` and `right` (or `top` and `bottom`) child
subtrees plus a `ratio` between `0.0` and `1.0` that sets the split
position. Walk the tree recursively to reach every pane.

#### `new_tab`

Create a new tab in the given workspace. Spawns a shell in a PTY at the
default size; returns both the new tab UUID and the root pane UUID.

**Request:** `{"jsonrpc":"2.0","method":"new_tab","params":{"workspace_idx":0},"id":1}`

**Params:**
| Field | Type | Required | Description |
|---|---|---|---|
| `workspace_idx` | number | yes (semantically) | Daemon-side workspace index the new tab belongs to. Omission is accepted for wire compat with pre-FIX-004 callers: the daemon logs a `WARN` (marker: `"new_tab: missing workspace_idx"`) and defaults to `0`. New clients MUST pass this field explicitly — the silent default is deprecated. |
| `rows` | number | no | PTY rows; defaults to `24`. |
| `cols` | number | no | PTY columns; defaults to `80`. |
| `cwd` | string | no | Starting directory; silently ignored if not a directory. |
| `command` | string[] | no | Profile command argv; empty array ignored. |

**Result:** `{"tab_id":"<uuid>","pane_id":"<uuid>"}`

**Errors:** out-of-range `workspace_idx` (>= the workspace count) returns
`INTERNAL_ERROR` with the message `"failed to create tab: workspace index
N out of bounds (len=M)"`. No PTY is spawned in that case.

**Example:**
```sh
echo '{"jsonrpc":"2.0","method":"new_tab","params":{"workspace_idx":0},"id":1}' \
  | socat - UNIX-CONNECT:"$SOCK"
```
Expected:
```json
{"jsonrpc":"2.0","result":{"tab_id":"<uuid>","pane_id":"<uuid>"},"id":1}
```

#### `close_tab`

Close a tab, killing its PTYs and removing it from the layout.

**Request:** `{"jsonrpc":"2.0","method":"close_tab","params":{"tab_id":"<uuid>"},"id":1}`

**Params (one of):**
| Field | Type | Required | Description |
|---|---|---|---|
| `tab_id` | string | preferred | UUID of the tab to close. |
| `pane_id` | string | legacy | UUID of any pane in the tab. Accepted for backward compatibility; new scripts should use `tab_id`. |

**Result:** `{"ok":true}`

**Notes:** when only `pane_id` is provided, the daemon looks up the tab
that contains the pane and closes it. If the pane exists in the registry
but not in any tab tree (an unusual legacy state), `close_pane` is called
as a fallback.

#### `focus_tab`

Mark a tab as the active tab in its workspace.

**Request:** `{"jsonrpc":"2.0","method":"focus_tab","params":{"tab_id":"<uuid>"},"id":1}`

**Params:** `{"tab_id":"<uuid>"}` (required).

**Result:** `{"ok":true}`

#### `move_tab`

Reorder tabs within a workspace.

**Request:** `{"jsonrpc":"2.0","method":"move_tab","params":{"tab_id":"<uuid>","new_index":0},"id":1}`

**Params:** `{"tab_id":"<uuid>","new_index":<number>}` (both required).

**Result:** `{"ok":true}`

#### `create_workspace`

Create a new workspace and seed it with one tab.

**Request:** `{"jsonrpc":"2.0","method":"create_workspace","params":{"name":"work"},"id":1}`

**Params:** `{"name":"<string>"}` (optional; defaults to `"Workspace"`).

**Result:** `{"workspace_id":"<uuid>","workspace_idx":<number>,
              "pane_id":"<uuid>","tab_id":"<uuid>"}`

#### `update_split_ratios`

Persist updated split ratios after the client has finished a drag. The
daemon stores the ratios so a reconnecting client can restore them.

**Request:** `{"jsonrpc":"2.0","method":"update_split_ratios","params":{"ratios":[...]},"id":1}`

**Params:**
```json
{"ratios":[{"pane_id":"<uuid>","ratio":0.5},...]}
```

**Result:** `{"ok":true}` (entries with missing/invalid fields are silently
skipped).

### 5.2 Panes

#### `split_pane`

Split an existing pane into two, spawning a new shell in the new half.

**Request:** `{"jsonrpc":"2.0","method":"split_pane","params":{"pane_id":"<uuid>","direction":"horizontal"},"id":1}`

**Params:**
| Field | Type | Required | Description |
|---|---|---|---|
| `pane_id` | string | yes | Pane to split. |
| `direction` | string | yes | `"horizontal"` or `"vertical"`. |
| `rows` | number | no | Defaults to `24`. |
| `cols` | number | no | Defaults to `80`. |
| `cwd` | string | no | Defaults to the source pane's CWD. |

**Result:** `{"pane_id":"<new_uuid>"}`

#### `close_pane`

Close a single pane within a split. If the pane is the sole leaf of its
tab, the entire tab is closed (same behaviour as `close_tab`). Otherwise,
only this pane dies and its sibling is promoted.

**Request:** `{"jsonrpc":"2.0","method":"close_pane","params":{"pane_id":"<uuid>"},"id":1}`

**Params:** `{"pane_id":"<uuid>"}` (required).

**Result:** `{"ok":true}`

#### `get_pane_info`

Return metadata for a live pane.

**Request:** `{"jsonrpc":"2.0","method":"get_pane_info","params":{"pane_id":"<uuid>"},"id":1}`

**Params:** `{"pane_id":"<uuid>"}` (required).

**Result:** `{"pane_id":"<uuid>","rows":24,"cols":80,
              "title":"<string>","cwd":"<path>","pid":<number>}`

#### `resize_pane`

Resize the PTY for a pane. The daemon calls `TIOCSWINSZ` on the PTY
master — effect propagates to the shell and its foreground process group
immediately (SIGWINCH).

**Request:** `{"jsonrpc":"2.0","method":"resize_pane","params":{"pane_id":"<uuid>","rows":40,"cols":120},"id":1}`

**Params:** `{"pane_id":"<uuid>","rows":<number>,"cols":<number>}`
(all required).

**Result:** `{"ok":true}`

#### `send_input`

Write bytes to a pane's PTY, as if the user had typed them.

**Request:** `{"jsonrpc":"2.0","method":"send_input","params":{"pane_id":"<uuid>","data":"bHMK"},"id":1}`

**Params:**
| Field | Type | Required | Description |
|---|---|---|---|
| `pane_id` | string | yes | Target pane UUID. |
| `data` | string | yes | **Base64-encoded** bytes to write. |

**Result:** `{"ok":true}`

**Notes:** the `data` field is base64 because JSON strings cannot carry
arbitrary bytes (NUL, invalid UTF-8). The daemon decodes to raw bytes and
writes them to the PTY master fd unchanged. Invalid base64 returns
`-32602`. See §7 for a worked example sending `"ls\n"`.

#### `send_sigint`

Sends `0x03` (ETX/Ctrl+C) to the PTY unconditionally. Additionally calls
`kill(-pgid, SIGINT)` on the foreground process group **unless** the
foreground process is a known signal-forwarder (`ssh`, `mosh-client`,
`telnet`, `rsh`) — those tools forward `0x03` to a remote PTY and would
be killed by a local SIGINT (FIX-017). For shells and most TUIs, the
line discipline handles `0x03` via ISIG; the kill catches raw-mode apps
that swallow `0x03` without acting on it (Node.js, pm2 — BUG-001).

**Request:** `{"jsonrpc":"2.0","method":"send_sigint","params":{"pane_id":"<uuid>"},"id":1}`

**Params:** `{"pane_id":"<uuid>"}` (required).

**Result:** `{"ok":true}`

### 5.3 Streaming

Both streaming methods return a JSON-RPC ack (line mode) first.
`subscribe_output` then switches the connection to binary-frame mode;
`subscribe_layout` stays in line-JSON mode and emits notifications.

There is no `unsubscribe` RPC on local sockets — close the socket to end
the stream.

#### `subscribe_output` (binary-mode stream)

Subscribe to raw PTY bytes for a single pane.

**Request:** `{"jsonrpc":"2.0","method":"subscribe_output","params":{"pane_id":"<uuid>"},"id":1}`

**Params:** `{"pane_id":"<uuid>"}` (required).

**Ack (line mode):** `{"jsonrpc":"2.0","result":{"ok":true},"id":<n>}\n`

**Wire transition:** after the ack newline, the connection is in
binary-frame mode for its remaining lifetime. The daemon stops reading
the client's write half; any bytes the client writes are dropped unread.
Socket EOF is the only way to end the stream.

**Frame format:** `[u32 BE length][raw PTY bytes]`. See §4.

**Replay semantics (AD-013).**

- The first binary frame MAY be a replay blob from the byte log's in-memory
  ring. Default ring size is 1 MiB (well under the 4 MiB cap). If the ring
  is empty, no replay frame is emitted and the next frame is live PTY output.
- Subsequent frames are live PTY output chunks as they arrive from the
  PTY master.
- The daemon guarantees zero overlap between replay bytes and subsequent
  live frames: clients can feed both verbatim into their VT parser without
  deduplication. See `SessionManager::subscribe_with_snapshot` for the
  handover proof.

**Termination.** The server ends the stream when (a) the pane's PTY
closes (the shell exits) or (b) the client closes the socket. In both
cases the daemon drops the write half, which signals EOF to the peer.

**Example:**
```sh
echo '{"jsonrpc":"2.0","method":"subscribe_output","params":{"pane_id":"<uuid>"},"id":1}' \
  | socat - UNIX-CONNECT:"$SOCK" > /tmp/stream.bin
# /tmp/stream.bin starts with one JSON ack line, then [u32 BE length][payload] frames.
```

#### `subscribe_layout` (JSON notification stream)

Subscribe to layout mutation events. Useful for agents that need to
react when tabs/panes open or close without polling.

**Request:** `{"jsonrpc":"2.0","method":"subscribe_layout","id":1}`

**Params:** none.

**Ack (line mode):** `{"jsonrpc":"2.0","result":{"ok":true},"id":<n>}\n`

**Stream (line mode):** the daemon emits JSON-RPC 2.0 notification lines
(no `id`), one per event, each terminated by `\n`. The connection does
**not** switch to binary mode.

**Event methods emitted by the daemon** (notifications; no request from
client, no `id` in the message):

| Notification method | Params |
|---|---|
| `tab_created` | `{"workspace_idx":<n>,"tab_id":"<uuid>","pane_id":"<uuid>"}` |
| `tab_closed` | `{"workspace_idx":<n>,"tab_id":"<uuid>"}` |
| `pane_split` | `{"tab_id":"<uuid>","parent_pane_id":"<uuid>","new_pane_id":"<uuid>","direction":"horizontal"\|"vertical"}` |
| `tab_moved` | `{"workspace_idx":<n>,"tab_id":"<uuid>","new_index":<n>}` |
| `active_tab_changed` | `{"workspace_idx":<n>,"tab_idx":<n>}` |

Example emitted line — on the wire this is a single line terminated by
`\n`; wrapped here for readability:

```
{"jsonrpc":"2.0",
 "method":
   "tab_created",
 "params":{"workspace_idx":0,"tab_id":"<uuid>","pane_id":"<uuid>"}}
```

**Known limitations.** The subscription does not currently emit events for
pane-closure (there is no pane_closed notification) or workspace lifecycle
events such as workspace_created or workspace_switched. The underlying
broadcast channel inside the daemon does carry `PaneCreated`, `PaneClosed`,
and `Notification` events, but the `subscribe_layout` handler intentionally
filters to the five layout events above. To detect pane closure or
workspace changes, call `get_layout` on a new connection when you need a
fresh snapshot, or open an additional subscription. This is the current
shape of the API, not a permanent contract — a future task may broaden the
event set.

**Pipelining contract.** After the ack, the connection is
server-to-client only: the daemon stops reading the client's write half,
so any bytes the client writes (including the half-close that produces
EOF) terminate the subscription. Open a second connection to issue
additional RPCs while this one is subscribed.

**Termination.** The stream ends when the daemon exits or the client
closes the socket.

### 5.4 Pairing / devices

All four methods below require iroh support to be live. When the daemon's
iroh endpoint failed to bind (or was intentionally disabled), these RPCs
return error code `-32601` with the message
`"sync endpoint not available (daemon started without --allow-pairing?)"`.
See §6 note R-2 — the same code means "method not found" elsewhere.

#### `list_devices`

List paired devices (from `authorized_devices.json`).

**Request:** `{"jsonrpc":"2.0","method":"list_devices","id":1}`

**Params:** none.

**Result:**
```json
{"devices":[{"device_id":"<id>","name":"<string>",
             "paired_at":"<iso-8601>","last_seen":"<iso-8601>"}]}
```

#### `revoke_device`

Remove a paired device. Live streams from that device are cut immediately
and a `DeviceRevoked` event is emitted on the local sync event channel.

**Request:** `{"jsonrpc":"2.0","method":"revoke_device","params":{"device_id":"<id>"},"id":1}`

**Params:** `{"device_id":"<id>"}` (required).

**Result:** `{"ok":<bool>}` — `true` if the device was in the registry,
`false` if not found.

#### `get_pairing_info`

Return a fresh QR pairing payload encoding this daemon's iroh node ID and
host machine name, along with a PNG of the QR code.

**Request:** `{"jsonrpc":"2.0","method":"get_pairing_info","id":1}`

**Params:** none.

**Result:**
```json
{"node_id":"<hex>","machine":"<hostname>","qr_png_base64":"<base64 PNG>"}
```

#### `enable_pairing`

Open a time-limited pairing window. During the window, the daemon accepts
one inbound pair handshake from an unknown device.

**Request:** `{"jsonrpc":"2.0","method":"enable_pairing","params":{"secs":120},"id":1}`

**Params:** `{"secs":<number>}` (optional; defaults to `120`).

**Result:** `{"ok":true,"secs":<number>}`

### 5.5 Notifications

#### `notify`

Client-side notification log. The GTK client has already detected an
OSC 9/99/777 notification locally and surfaced it (tab badge, desktop
notification, click-to-focus). The RPC exists so the daemon can log the
event for audit and planned MCP observability. It does not modify session
state, emit broadcast events, or touch the sync endpoint.

**Request:** `{"jsonrpc":"2.0","method":"notify","params":{"pane_id":"<uuid>","title":"done"},"id":1}`

**Params:**
| Field | Type | Required | Description |
|---|---|---|---|
| `pane_id` | string | yes | Source pane UUID. |
| `title` | string | no | Notification title. |
| `body` | string | no | Notification body (accepted but not logged). |
| `source` | string | no | Origin tag (e.g. `"osc9"`). |

**Result:** `{"ok":true}`

### 5.6 Pinning

#### `set_pinned`

Mark the session as pinned (persists across `shutdown_clean`; pinned
sessions are not moved to trash).

**Request:** `{"jsonrpc":"2.0","method":"set_pinned","params":{"pinned":true},"id":1}`

**Params:** `{"pinned":<bool>}` (required).

**Result:** `{"ok":true}`

#### `get_pinned`

Return the current pinned state.

**Request:** `{"jsonrpc":"2.0","method":"get_pinned","id":1}`

**Params:** none.

**Result:** `{"pinned":<bool>}`

### 5.7 Lifecycle

The four shutdown modes and `disconnect` are summarised in this table. All
are terminal: the daemon sends its ack line before acting.

| Method | Saves session file? | Moves to trash? | Kills PTYs? | Daemon exits? |
|---|---|---|---|---|
| `disconnect` | yes | no | no | no |
| `shutdown` | no | no | yes | yes |
| `shutdown_save` | yes | no | yes | yes |
| `shutdown_clean` | yes | unless pinned | yes | yes |

#### `disconnect`

V2-005 / AD-012: the client connection goes away but the daemon keeps
running with all PTYs alive. The daemon flushes byte logs, saves the
session file, and drops the connection. The `UnixListener` keeps accepting
new connections.

**Request:** `{"jsonrpc":"2.0","method":"disconnect","id":1}`

**Params:** none.

**Result:** `{"ok":true}` (written before the connection closes).

**Notes:** use `disconnect` when you want to drop your connection but
leave the session alive for reattachment; use `shutdown_save` when you
want a normal close with persistence (session saved, daemon exits).

#### `shutdown`

Permanent close: daemon exits immediately without saving. Use for
explicit "quit" from the UI when the user does not want the session
restored.

**Request:** `{"jsonrpc":"2.0","method":"shutdown","id":1}`

**Params:** none.

**Result:** `{"ok":true}` (written before `std::process::exit(0)`).

#### `shutdown_save`

Normal close: daemon flushes byte logs, saves the session JSON, and
exits. The next launch will restore this session.

**Request:** `{"jsonrpc":"2.0","method":"shutdown_save","id":1}`

**Params:** none.

**Result:** `{"ok":true}`

#### `shutdown_clean`

Browser-model close: daemon flushes byte logs, saves the session JSON,
moves the session JSON to trash, and exits. If the session is pinned
(see `set_pinned`), the trash step is skipped.

**Request:** `{"jsonrpc":"2.0","method":"shutdown_clean","id":1}`

**Params:** none.

**Result:** `{"ok":true}`

#### `is_attached`

Probe: "is any *other* local client currently holding a socket?" Used by
the launcher to distinguish "daemon running but orphaned after
`disconnect`" from "daemon running and actively attached to a GUI".

Returns `true` if at least one other local client is connected; `false`
if the daemon is orphaned (the probe's own connection is not counted as
attachment). iroh peer connections are not counted either — Android
clients are a separate seat (AD-004/AD-005) and do not block local-GUI
reattach.

**Request:** `{"jsonrpc":"2.0","method":"is_attached","id":1}`

**Params:** none.

**Result:** `{"attached":<bool>}`

---

## 6. Errors

Error responses follow JSON-RPC 2.0:

```json
{"jsonrpc":"2.0","error":{"code":-32602,"message":"missing param: pane_id"},"id":1}
```

The daemon emits exactly these five codes:

| Code | Meaning | When |
|---|---|---|
| `-32700` | Parse error | Request is not valid JSON. |
| `-32600` | Invalid request | JSON-RPC version is not `"2.0"`, or `method` field is empty. |
| `-32601` | Method not found | Unknown method name; also returned for sync RPCs when no iroh endpoint (see R-2). |
| `-32602` | Invalid params | Missing required param, malformed UUID, unknown pane/tab, invalid direction string, invalid base64, etc. |
| `-32603` | Internal error | Handler failed (session manager error, serialization failure, etc.). |

**R-2 note.** Code `-32601` is overloaded: the daemon returns it both for
unknown method names *and* for sync-dependent RPCs (`list_devices`,
`revoke_device`, `get_pairing_info`, `enable_pairing`) when the iroh
endpoint is not available. Callers should use the `error.message` text to
disambiguate: `"sync endpoint not available ..."` for the latter vs.
`"Unknown method: ..."` for the former.

No other numeric error codes are used. Previous revisions of this doc
documented `1001`/`1002`/`1003` (tab/pane/workspace not found) — those do
not exist in the daemon; pane/tab/workspace lookups return `-32602` with a
descriptive message instead.

---

## 7. Examples — `socat` walkthrough

End-to-end: start daemon, list tabs, create a tab, send `ls\n`, list again,
shut down.

```sh
# 1. Launch a daemon in the foreground.
./target/release/forgetty-daemon --foreground &
DAEMON_PID=$!

# Wait for the socket to appear (up to ~5s), then pick the most recent.
for _ in $(seq 1 50); do
  SOCK=$(ls -t "$XDG_RUNTIME_DIR"/forgetty-*.sock 2>/dev/null | head -n1)
  [ -n "$SOCK" ] && [ -S "$SOCK" ] && break
  sleep 0.1
done
echo "Socket: $SOCK"

# 2. list_tabs — empty on a fresh daemon.
echo '{"jsonrpc":"2.0","method":"list_tabs","id":1}' \
  | socat - UNIX-CONNECT:"$SOCK"
# → {"jsonrpc":"2.0","result":{"tabs":[]},"id":1}

# 3. new_tab — creates a shell in a default-sized PTY and captures the
#    new pane_id with jq. (jq is optional on Debian-family: `apt install jq`.
#    Without jq, eyeball the response and set PANE=<paste pane_id> manually.)
PANE=$(echo '{"jsonrpc":"2.0","method":"new_tab","params":{"workspace_idx":0},"id":2}' \
  | socat - UNIX-CONNECT:"$SOCK" | jq -r '.result.pane_id')
echo "pane_id: $PANE"

# 4. send_input — write "ls\n" (base64 "bHMK") to that pane.
echo "{\"jsonrpc\":\"2.0\",\"method\":\"send_input\",\"params\":{\"pane_id\":\"$PANE\",\"data\":\"bHMK\"},\"id\":3}" \
  | socat - UNIX-CONNECT:"$SOCK"
# → {"jsonrpc":"2.0","result":{"ok":true},"id":3}

# 5. list_tabs again — the pane_id is now in the tabs array.
echo '{"jsonrpc":"2.0","method":"list_tabs","id":4}' \
  | socat - UNIX-CONNECT:"$SOCK"
# → {"jsonrpc":"2.0","result":{"tabs":[{"pane_id":"<uuid>","pid":<n>,
#     "rows":24,"cols":80,"cwd":"<path>","title":"<string>"}]},"id":4}

# 6. Clean shutdown.
echo '{"jsonrpc":"2.0","method":"shutdown","id":5}' \
  | socat - UNIX-CONNECT:"$SOCK"
# → {"jsonrpc":"2.0","result":{"ok":true},"id":5}
wait $DAEMON_PID 2>/dev/null
```

### Observing the binary-frame mode switch

```sh
# With $SOCK set and $PANE known from above.
# The `sleep 2` holds the connection open for ~2s after the request so the
# daemon has time to emit frames before `socat` closes on EOF from stdin.
( echo "{\"jsonrpc\":\"2.0\",\"method\":\"subscribe_output\",\"params\":{\"pane_id\":\"$PANE\"},\"id\":1}"; \
  sleep 2 ) \
  | socat - UNIX-CONNECT:"$SOCK" > /tmp/sub.bin

# The file starts with the JSON ack line, then one or more [u32 BE length][payload] frames.
head -c 200 /tmp/sub.bin | xxd | head
```

---

## 8. Out of scope

- **Rust type reference.** Shapes are prose and JSON in this doc. For
  exact Rust types, read `crates/forgetty-socket/src/handlers.rs` and
  `crates/forgetty-session/src/layout.rs`.
- **iroh QUIC / Android wire format.** Documented in the Forgetty Android
  protocol doc in the sibling repo.
- **OSC escape sequences and terminal rendering.** The daemon is a byte
  pipe (AD-007). Clients own the VT parser and OSC handling (AD-008).
- **Internal crate APIs** — `forgetty-vt`, `forgetty-session`,
  `forgetty-workspace`, etc.
- **Daemon CLI flags.** Run `forgetty-daemon --help` for the canonical
  list; defaults noted in this doc may drift.

---

## Appendix A — Grep cross-check

QA runs this one-liner from the repo root. It must print `OK`:

```sh
# Every method "method": "<name>" in the doc must match a pub const in protocol.rs.
diff -u \
  <(grep -oE '"method":[[:space:]]*"[a-z_]+"' docs/socket-api.md \
      | sed -E 's/.*"([a-z_]+)".*/\1/' | sort -u) \
  <(grep -oE 'pub const [A-Z_]+: &str = "[a-z_]+"' \
      crates/forgetty-socket/src/protocol.rs \
      | sed -E 's/.*"([a-z_]+)".*/\1/' | sort -u) \
  && echo OK
```

If the diff shows removed lines, the doc mentions a method that does not
exist in code. If it shows added lines, a method was added to
`protocol.rs` without being documented. Both conditions are regressions.
