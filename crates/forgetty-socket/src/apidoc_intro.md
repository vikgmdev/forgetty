# Forgetty Socket API

The `forgetty` server (`forgetty server run`) serves a JSON-RPC 2.0 control plane
on a Unix domain socket. The `forgetty` CLI, scripts and agents use it. A paired
device uses the same methods over protocol v2 (`forgetty/rpc/2`, see
`docs/architecture/PROTOCOL_V2.md`) where a method is marked `v2: yes`.

This file is generated from the method table in
`crates/forgetty-socket/src/api.rs`. Do not edit it by hand. Regenerate it with:

```sh
forgetty api list --markdown > docs/socket-api.md
```

A test fails when this file is out of date. The same data is served live:
`forgetty api list` and `forgetty api describe METHOD` (JSON Schemas included).

## 1. Connection

The server binds `$XDG_RUNTIME_DIR/forgetty/forgetty.sock`. Without
`XDG_RUNTIME_DIR` it uses `/tmp/forgetty-<uid>/forgetty.sock`.
`forgetty --state-dir DIR` places it under `DIR/run/forgetty/`.
`forgetty server run --socket-path PATH` overrides the path.

```sh
SOCK="${XDG_RUNTIME_DIR:-/tmp/forgetty-$(id -u)}/forgetty/forgetty.sock"
```

Access control:

- The socket directory has mode `0700`. The server refuses to start if the
  directory belongs to another uid, is a symlink or has group or other bits.
- The socket file has mode `0600`.
- The server checks `SO_PEERCRED` on every connection and drops a peer with
  another uid. It reads nothing from that peer.
- The CLI checks that the socket belongs to the current uid and is not a
  symlink before it connects.

Any process of the same user can connect. That is how `socat`, scripts and
agents drive the server. The socket has no network exposure.

## 2. Wire format

Requests and replies are one JSON object per line.

```json
{"jsonrpc":"2.0","method":"list_tabs","params":{},"id":1}
{"jsonrpc":"2.0","result":{"tabs":[]},"id":1}
```

A failed call has `error` instead of `result`:

```json
{"jsonrpc":"2.0","error":{"code":-32602,"message":"missing param: pane_id"},"id":1}
```

Ids are UUID v4 strings. The connection stays open and can carry more requests,
except after a `subscribe_*` ack.

### Streams

- `subscribe_layout` replies with `{"ok":true}`, then sends one JSON-RPC
  notification (no `id`) per line. The connection is server-to-client only.
  Any byte the client writes ends the subscription. Open a second connection
  for more requests.
- `subscribe_output` replies with `{"ok":true}`, then the connection carries
  binary frames: `[u32 big-endian length N][N bytes]`. The cap is 4 MiB
  (`MAX_FRAME_SIZE`). The server drops what the client writes. Close the
  socket to end the stream.

`subscribe_output` replay (AD-013): the first frame can be a replay blob from
the byte-log ring (1 MiB by default). Later frames are live output. Replay and
live frames never overlap, so a client feeds all frames to its VT parser as
they come.

A slow reader of `subscribe_layout` can miss events, because events share the
server's broadcast channel. `forgetty wait` checks `get_pane_info` every 2 s
for that reason.

### Notifications of `subscribe_layout`

| Notification | Params |
|---|---|
| `tab_created` | `{"workspace_idx":n,"tab_id":"uuid","pane_id":"uuid"}` |
| `tab_closed` | `{"workspace_idx":n,"tab_id":"uuid"}` |
| `pane_split` | `{"tab_id":"uuid","parent_pane_id":"uuid","new_pane_id":"uuid","direction":"horizontal"\|"vertical"}` |
| `tab_moved` | `{"workspace_idx":n,"tab_id":"uuid","new_index":n}` |
| `active_tab_changed` | `{"workspace_idx":n,"tab_idx":n}` |
| `active_workspace_changed` | `{"workspace_idx":n}` |
| `workspace_created` | `{"workspace_idx":n,"workspace_id":"uuid","name":"..."}` |
| `workspace_renamed` | `{"workspace_idx":n,"workspace_id":"uuid","name":"..."}` |
| `workspace_deleted` | `{"workspace_idx":n,"workspace_id":"uuid"}` |
| `workspace_color_changed` | `{"workspace_idx":n,"workspace_id":"uuid","color":"#RRGGBB"\|null}` |
| `workspaces_reordered` | `{"from_idx":n,"to_idx":n,"from_workspace_id":"uuid","to_workspace_id":"uuid"}` |
| `active_pane_changed` | `{"workspace_idx":n,"tab_id":"uuid","pane_id":"uuid"\|null}` |
| `label_changed` | `{"target":"workspace"\|"tab"\|"pane","id":"uuid","label":"..."}` |
| `pane_created` | `{"pane_id":"uuid"}` |
| `pane_exited` | `{"pane_id":"uuid","exit_code":n\|null}`, sent before `pane_closed` (alone for a kept pane) |
| `pane_closed` | `{"pane_id":"uuid"}` |
| `title_changed` | `{"pane_id":"uuid","title":"..."}` |
| `cwd_changed` | `{"pane_id":"uuid","cwd":"/path"}` |
| `status_changed` | `{"pane_id":"uuid","status":"waiting","message":"..."\|null}`, only when the status really changes |
| `notification` | `{"pane_id":"uuid","title":"..."\|null,"body":"..."}` |

`forgetty events --json` prints each notification as `{"event":"StatusChanged", ...params}`.

### Pane state

`list_tabs` and `get_pane_info` carry what the OSC scanner (AD-019) found:

| Field | Description |
|---|---|
| `title` | Last OSC 0 or 2 title, sanitised, 256 characters at most. |
| `cwd` | Last OSC 7 directory. Once a pane reported one it wins over `/proc/<pid>/cwd`. |
| `status` | `{status, message, kind, progress, app, record_id}` from OSC 7501. |
| `last_notification` | `{title, body, at_ms}` of the last OSC 9, 99 or 777 notification, else `null`. |

`status.status` is `idle` (also when nothing was reported), `working`,
`waiting`, `done` or `failed`. With several records (`id=`), the pane shows the
most urgent: waiting, failed, working, done, idle. `working` and `waiting`
end when the process exits or a new shell prompt starts (OSC 133 `A`).
All text is untrusted: control characters and bidi overrides are removed and the
length is capped. Notifications are limited to 10 per second per pane.

## 3. Errors

| Code | Name | When |
|---|---|---|
| `-32700` | `parse_error` | The request is not valid JSON. |
| `-32600` | `bad_request` | The JSON-RPC version is not `"2.0"` or `method` is empty. |
| `-32601` | `method_not_found` | Unknown method. Also the sync methods when the iroh endpoint is not running (the message says `sync endpoint not available`). |
| `-32602` | `bad_request` | A param is missing or malformed, or the pane, tab or workspace does not exist. |
| `-32603` | `internal_error` | The handler failed. |
| `-32001` | `server_busy` | `shutdown` with `if_idle` while clients are attached, or the server is stopping. |

The CLI (`forgetty api call`) prints `bad_request` and `method_not_found` by
name. Exit codes: 0 ok, 1 failed, 2 bad command line or `bad_request`, 3 target
or method not found (or ambiguous target), 4 server unreachable, 5 `wait` timed out.

Over protocol v2 the error codes map to `unknown_method`, `not_found`,
`bad_request` and `internal`. Methods marked `v2: no` are refused over v2 with
`unknown_method`: `send_input`, `resize_pane`, `list_clients` and event
subscription have v2 forms of their own, and a paired device cannot open
pairing windows.

A server built with `--features qa-tools` also answers `qa_*` methods on the
local socket for the client test suites. A normal build returns
`method_not_found`.

## 4. Method reference

Every method takes `params` (an object, or absent when it has none) and returns
`result`. The examples show the `params` and the `result`.
