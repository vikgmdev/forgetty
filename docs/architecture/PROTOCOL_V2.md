# Forgetty Protocol v2 (`forgetty/rpc/2`)

> **Status:** LOCKED 2026-10-21 by AD-018. Server, Android and Windows implement
> exactly this. Changes need a new AD.

## 1. Transport

- iroh QUIC. ALPN `forgetty/rpc/2`. The server accepts only node ids present in
  `authorized_devices.json` (existing registry). Unknown node: close with
  application error code `0x01 not_authorized`.
- Pairing moves to ALPN `forgetty/pair/2` (AD-021, section 7).
- QUIC transport config, both sides: keep-alive 5 s, max idle timeout 30 s.
- The server binds a fixed UDP port (default 47297, `--port`), so its direct
  address stays the same across restarts. Clients cache the last direct
  address and dial it first, with the relay in parallel.
- All framing: `[u32 BE length][payload]`. Length cap 4 MiB. A frame over the
  cap closes the stream with error `0x02 frame_too_large`.

## 2. Streams

| Stream | Opened by | Kind | Payload |
|---|---|---|---|
| Control | client, first stream | bi | MessagePack `ControlMsg` frames |
| Input | client, second stream | uni (client to server) | binary input frames (section 5) |
| Pane output | server, one per attached pane | uni (server to client) | one MessagePack header frame, then raw byte frames |

QUIC streams are independent, so output bursts never delay input or control.

## 3. Handshake

First frame on Control, client to server:

```
Hello {
  proto_major: 2, proto_minor: u16,
  client_id: Uuid,            // persisted by the client, stable across reconnects
  input_epoch: Uuid,          // random per client process start
  client_kind: "windows" | "android" | "cli" | "gtk",
  client_name: String,        // device name shown in `client ls`
  client_version: String,
  capabilities: Vec<String>,
}
```

Server answers with a `HandshakeReply`:

```
HandshakeReply = Welcome { ... } | Error { code: String, message: String }

Welcome {
  proto_major: 2, proto_minor: u16,
  server_id: Uuid, server_version: String, hostname: String,
  capabilities: Vec<String>,
}
```

Rules: a different `proto_major` gets `Error { code: "proto_mismatch" }`. The
server then calls `finish()` on the stream and waits up to 2 s for the client to
close, so the error frame is not lost. `Hello` is a plain struct, not a
`ControlMsg`. Each side ignores capability strings it does not know. A feature is used
only if both sides list its capability. Initial capability names:
`resume`, `layout_events`, `status_events`, `capture`, `size_policy_latest`.

## 3a. Encoding rules

- MessagePack with named fields (`rmp-serde` `to_vec_named`).
- Enums are externally tagged: `{"Request": {...}}`.
- Uuids are 36-char strings everywhere except the input frame header.
- Timestamps are RFC 3339 strings.
- A frame a peer cannot decode (for example an event from a newer server) is
  skipped with a warning, never fatal.

## 4. Control messages

```
ControlMsg =
  | Request  { id: u64, method: String, params: Value }   // client to server
  | Response { id: u64, result: Value }                   // server to client
  | Failure  { id: u64, code: String, message: String }   // server to client
  | Event    { seq: u64, event: ServerEvent }             // server to client
  | Ping     { nonce: u64 }                               // client to server
  | Pong     { nonce: u64, input_acked: u64 }             // server to client
```

Liveness: the client sends `Ping` every 1 s while connected. The server answers
`Pong` at once with the highest input sequence it has written to a PTY for this
`client_id`. If no `Pong` arrives within 3 s, the client treats the connection as
dead and redials at once. It does not wait for the QUIC idle timeout.

`Value` is a MessagePack map. `method` and `params` use the same names and
shapes as the local socket API (`docs/socket-api.md`): `list_tabs`,
`get_layout`, `new_tab`, `close_tab`, `focus_tab`, `move_tab`,
`create_workspace`, `update_split_ratios`, `split_pane`, `close_pane`,
`get_pane_info`, `resize_pane`, `send_sigint`, `list_devices`, `revoke_device`.
Base64 fields of the JSON socket API are raw `bin` in v2.

New methods:

| Method | Params | Result |
|---|---|---|
| `attach_pane` | `pane_id, from_offset: Option<u64>` | `{ ok }`. The server then opens a Pane output stream. |
| `detach_pane` | `pane_id` | `{ ok }`. The server finishes that stream. |
| `subscribe_events` | none | `{ seq }`. Events then flow on Control. |
| `capture` (cap `capture`) | `pane_id, trim: bool, last_lines: Option<u32>` | `{ text }` (P4) |
| `list_clients` | none | `[{ client_id, client_kind, client_name, connected_at }]` |

`ServerEvent` (all carry ids as strings):
`LayoutChanged { layout }`, `PaneClosed { pane_id }`, `TitleChanged { pane_id, title }`,
`CwdChanged { pane_id, cwd }`, `PaneResized { pane_id, rows, cols }`,
`StatusChanged { pane_id, status, message }` (P4, cap `status_events`),
`Notification { pane_id, title, body }` (OSC 9/99/777, cap `status_events`),
`PaneExited { pane_id, exit_code }` (the pane stays listed only if `remain_on_exit`),
`ClientConnected { client_id, client_kind, client_name, connected_at }`,
`ClientDisconnected { client_id }`.

## 5. Input stream

Each frame payload: `[16 bytes pane uuid][u64 BE seq][input bytes]`. No
MessagePack, no base64. `seq` starts at 1 per `(client_id, input_epoch)` and increases by 1
per frame, across reconnects of the same process. A new process sends a new
`input_epoch`, so its counter starts fresh. The server keeps the last written
`seq` per `(client_id, input_epoch)` (in memory, kept 10 min after the client
leaves) and drops any frame
with `seq <=` that value, so a resent frame is never written twice. The server
writes the bytes to the PTY in arrival order.

The client keeps every frame until a `Pong.input_acked` covers it (cap 64 KiB).
After a reconnect it resends all unacked frames in order. Input is exactly-once. Input from a
client also marks that client as the pane's latest active client (section 6).

## 6. Size policy (AD-020)

The server keeps, per pane, the last size each attached client asked for with
`resize_pane`, and the latest active client (last input frame or `focus_tab`
for that pane). The PTY size is the latest active client's size. A change
emits `PaneResized`. Clients render at the PTY size and letterbox.

## 7. Pairing (`forgetty/pair/2`, AD-021)

QR payload (JSON, then QR): `{ v: 2, node_id, machine, relay, secret }` where
`secret` is 16 random bytes, base32. `forgetty-server pair` opens a 60 s window
with a fresh secret and prints the QR as ASCII. Client sends
`PairRequest { secret, device_name, client_kind }` on a bi stream. A wrong or
expired secret gets `PairResult { ok: false, reason }`. One success closes the
window. The registry stores the device. The server logs the device name and id.

## 8. Pane output stream and resume

First frame (MessagePack): `PaneStreamHeader { pane_id, start_offset: u64, reset: bool, rows, cols }`.
All following frames are raw PTY bytes. Byte `n` of the stream after the header
is pane offset `start_offset + n`.

Server byte log keeps absolute offsets: `total_written: u64` per pane, and the
ring/disk log remember the offset of their first byte.

- `from_offset = None`: start at the oldest byte still in the log, `reset = true`.
- `from_offset = Some(o)` and `o` is still in the log: start at `o`, `reset = false`.
  The client appends with no screen reset.
- `o` is older than the log: start at the oldest byte, `reset = true`.
  The client resets its VT state and replays.
- `o` greater than `total_written`: treat as None.

A lagging subscriber (broadcast `Lagged`) does not drop: the server rewinds that
stream to the subscriber's own next offset from the log.

## 9. Client reconnect rules (`forgetty-client` crate)

- On connection loss: redial at once, then backoff 100 ms, 250 ms, 500 ms, 1 s,
  2 s, then every 2 s. Never give up while the app is in the foreground.
- After `Welcome`: re-send `subscribe_events`, then `attach_pane` with the last
  known offset for every pane the UI shows.
- The UI shows "reconnecting" after 1 s without a connection, never sooner.
- Initial state is `Reconnecting { since }`. States: `Connected`,
  `Reconnecting { since }`, `Failed(reason)`, `Closed` (after `close()`).
- `ClientHandle::network_changed()`: the app calls it on an OS network change
  (Android `ConnectivityManager` callback, Windows `NotifyIpInterfaceChange`).
  The client tells iroh about the change and sends an immediate `Ping`. Without
  a `Pong` in 1 s it redials.
- Input typed while disconnected is buffered (cap 64 KiB) and sent after resume.

## 10. Crates

- `forgetty-proto`: types, MessagePack codec, frame reader/writer
  (`tokio::io::AsyncRead/AsyncWrite`), pane-uuid input codec. No iroh dependency.
- `forgetty-client`: iroh endpoint, pairing, connect, handshake, reconnect,
  per-pane offset tracking, event stream. Public async API used by Android
  (through JNI) and Windows. No UI code.
- Server: `forgetty-server` implements v2 with `forgetty-proto`.

## 11. Amendments (2026-10-21, from the P2b implementation)

1. Input dedup table (section 5) is persisted to `input_seq.json` on graceful
   stop and loaded on start. After `kill -9` it is lost, and a resend after a
   crash can repeat input once. This is accepted.
2. A reader that falls behind the whole log gets its pane stream reset with
   QUIC stream error code `0x11 behind_log`. The client re-attaches with
   `from_offset` and gets `reset = true`.
3. The byte log never drops bytes to keep up. When its disk queue is full, the
   server stops reading that PTY until the queue drains (PTY flow control), so
   the program blocks on write. The in-memory ring default is 8 MiB per pane.
   Readers behind the ring but inside the disk log are served from disk, not
   reset.
4. Methods with their own v2 form are not in the generic Request allow-list:
   `send_input` (input stream), `resize_pane` is allowed and goes through the
   size policy, `list_clients` returns a bare array. `pair_*` and
   `get_pairing_info` are local-socket only.
5. A stopping server answers `Hello` with `Error { code: "server_stopping" }`.
   The client treats `server_stopping` as retryable (normal backoff). Every
   other handshake error is fatal.
6. Size policy (section 6): when a pane has no active client yet, the first
   `resize_pane` applies.
7. `Notification.title` is a `String`, empty when the sequence has none.

## 12. Snapshot catch-up (AD-023, capability `snapshot_catchup`)

- `PaneStreamHeader` gains `snapshot_len: u32` (default 0, omitted when 0).
- When `snapshot_len > 0`, `reset` is true. The first `snapshot_len` bytes after
  the header are VT snapshot data (screen, cursor, modes, title of the active
  screen, built from the ring). They do not advance the pane offset. The live
  bytes after them start at `start_offset`.
- The server replaces a stream by resetting it with stream code `0x13
  superseded`, then opening a new one. The client keeps a per-pane stream
  counter: a newer stream wins, and stale queued items are dropped on reset.
- Triggers: the stream is more than half the ring behind, makes no write
  progress for 2 s, or a resume starts further back than half the ring.
- Limits: one build shared per pane, a finished snapshot reused only while
  the log is within 1/8 ring of it, at most 2 concurrent builds server-wide.
  A slow or stalled stream waits at least 1 s between catch-ups, with backoff
  to 10 s. A stream that keeps pace (4 MiB/s or more of live bytes since its
  last snapshot, or a snapshot delivered in under 500 ms that the log outran
  before any live byte) waits at most 100 ms and gets no backoff.
- The snapshot has no scrollback, no palette, no OSC 10/11/12 colours. A client
  without the capability gets `0x11 log_lost` and re-attaches.
- Connection close codes: `0x12` control writer stalled 10 s, `0x14` more than
  64 requests in flight.

## 13. Methods added by P5

- `api.list`, `api.describe { method }`: self-describing API (name,
  description, input/output JSON Schema, examples). Allowed over v2.
- `set_label { target: "workspace"|"tab"|"pane", id, label }`: emits
  `LayoutChanged`. Layout values carry `label` on tabs and a `pane_labels` map.
  Clients show the label when set, else the title.
