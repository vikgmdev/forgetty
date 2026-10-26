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
  boot_id: Option<Uuid>,   // optional, new for every run of the server process
}
```

`server_id` is stable across restarts. `boot_id` is not: the server makes a new
one at each start and sends it in every `Welcome` of that run. A client that
sees a `boot_id` different from the one it saw on its last connection knows the
server restarted (section 9). The field is optional: an old server does not
send it, an old decoder ignores it, and a client that gets none changes
nothing.

Rules: a different `proto_major` gets `Error { code: "proto_mismatch" }`. The
server then calls `finish()` on the stream and waits up to 2 s for the client to
close, so the error frame is not lost. `Hello` is a plain struct, not a
`ControlMsg`. Each side ignores capability strings it does not know. A feature is used
only if both sides list its capability. A decoder must also accept values it does not
know: an unknown `client_kind` is `unknown` (one entry of a `list_clients` must not
make the list unreadable), and an unknown event inside `Event` is skipped but its
`seq` counts. Initial capability names:
`resume`, `layout_events`, `status_events`, `capture`, `size_policy_latest`,
`snapshot_catchup`, `input_flags`, `size_owner`, `split_before`, `workspace_id`.

Two additive capabilities for the request methods (both sides list them, as for the others):

- `split_before`: `split_pane` takes an optional `before` (bool, default false). With
  `true` the new pane goes first (left of, or above, the pane being split). A server
  without the capability ignores the field and splits after, so a client must not send
  `before: true` to it.
- `workspace_id`: `delete_workspace`, `set_workspace_color`, `duplicate_workspace` and
  `move_workspace` take workspace ids (`workspace_id`, `source_workspace_id`, `from_id`,
  `to_id`) in place of the positions. When an id is given it wins over the position, and
  an id that no longer exists is an error. A position can point at another workspace
  after another client moved or deleted one, an id cannot. A client without the
  capability re-reads the layout and sends the position right away.

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

The server handles requests of one connection side by side, not one after the
other: the control loop only hands each request to a task, and every method
answers when it is done (`Ping` as well). Methods that take locks the program
output also takes (`subscribe_events`, `attach_pane`, `detach_pane`) run on the
blocking pool, so a lock wait does not stall the
runtime thread that reads the control stream. The order of `attach_pane` and
`detach_pane` for one pane still holds: each request has a number in the order
it was read, and one with a lower number than the last one applied to the
pane does nothing.

An `attach_pane` counts as a request in flight (limit 64, section 9) until it
is answered, and no longer. The answer comes when the stream is set up. The
pacing wait and the snapshot build of a far-behind pane happen after it, on the
stream's own task, so they do not hold a slot.

A `detach_pane` is answered within 250 ms at most. The stream stops at once;
the size policy (who owns the pane size) follows, and if a lock it needs is
busy the answer does not wait for it. The server finishes the update later.

Events may arrive before the answer of `subscribe_events`. The server starts
the subscription first and puts the answer in the queue after it, so an event
with a `seq` above the `seq` of the answer can come ahead of the answer. A
client takes events as they arrive and does not wait for the answer.

New methods:

| Method | Params | Result |
|---|---|---|
| `attach_pane` | `pane_id, from_offset: Option<u64>` | `{ ok }`. The server then opens a Pane output stream. |
| `detach_pane` | `pane_id` | `{ ok }`. The server finishes that stream. |
| `subscribe_events` | none | `{ seq }`. Events then flow on Control. |
| `capture` (cap `capture`) | `pane_id, trim: bool, last_lines: Option<u32>` | `{ text }` (P4) |
| `list_clients` | none | `[{ client_id, client_kind, client_name, connected_at }]` (`client_kind` `local` for a local socket client, see section 6) |

`ServerEvent` (all carry ids as strings):
`LayoutChanged { layout }`, `PaneClosed { pane_id }`, `TitleChanged { pane_id, title }`,
`CwdChanged { pane_id, cwd }`, `PaneResized { pane_id, rows, cols, size_owner? }`,
`SizeOwnerChanged { pane_id, owner }` (cap `size_owner`, see section 6),
`StatusChanged { pane_id, status, message }` (P4, cap `status_events`),
`Notification { pane_id, title, body }` (OSC 9/99/777, cap `status_events`),
`PaneExited { pane_id, exit_code }` (the pane stays listed only if `remain_on_exit`),
`ClientConnected { client_id, client_kind, client_name, connected_at }`,
`ClientDisconnected { client_id }`.

## 5. Input stream

Each frame payload: `[16 bytes pane uuid][u64 BE seq][input bytes]`. No
MessagePack, no base64. `seq` starts at 1 per `(client_id, input_epoch)` and increases by 1
per frame, across reconnects of the same process. A new process sends a new
`input_epoch`, so its counter starts fresh.

**Dedup and ack happen at enqueue (AD-024).** The server keeps the last handled
`seq` per `(client_id, input_epoch)` (in memory, kept 10 min after the client
leaves) and drops any frame with `seq <=` that value, so a resent frame is
never queued twice. A frame counts as handled, and `Pong.input_acked` covers
it, once the server has put it in the bounded input queue of its pane (256
KiB). It does not wait for the PTY to take the bytes.

**Ordering is per pane.** Each pane has its own queue and its own writer, so
the bytes of one pane reach its PTY in arrival order, and a pane whose program
does not read input delays only itself. Frames of one client for different
panes are not ordered against each other at the PTY.

**A full queue drops frames.** When the queue of a pane has no room, the
server drops the frame, counts it, logs it, and still acks it (the client must
not resend into a queue that cannot drain). If the dropped bytes contain the
pane's VINTR byte (Ctrl+C by default), the server sends SIGINT to the pane's
foreground process group anyway, so Ctrl+C still ends a program that hangs.
While the server stops, it freezes input, closes the queues, waits up to 2 s
for them to drain, and only then saves the `seq` state. Frames it did not
queue are not acked, so the client resends them to the next server.

**One ack counter, no holes (AD-024).** `Pong.input_acked` is one counter per
client epoch, not per pane. A dropped frame is acked, so the server keeps a
pane's input free of holes: after it drops a frame of a client for a pane, it
drops every later frame of that client for that pane until the pane's queue is
empty. It then sends that client one event
`InputDropped { pane_id, bytes }` (capability `input_flags` only; it goes to
that client alone, carries the current event `seq`, and flows without
`subscribe_events`). `bytes` is what was dropped. Input the client sent before
the event may be missing in part, so a client shows the loss.

**Interrupts.** A frame holds an interrupt when it has the pane's VINTR byte
and the pane's tty has `ISIG` on and VINTR set. If the byte cannot go in order
(the queue is full, the client is in a hole, or the writer is busy with bytes
the program does not read), the server drops the input queued for the pane,
stops what the writer has half written, flushes the tty's unread input, and
sends SIGINT to the foreground process group. Bytes of the frame after the
last interrupt byte are queued. Otherwise the frame goes in order and the tty
does the interrupt. `auto` input never interrupts.

**Graceful stop.** The server freezes input, closes the pane queues, waits up
to 2 s for them to drain, and saves the `seq` state. Input never crosses a
restart: processes do not survive it (AD-017), so a frame resent after it would
type into a fresh shell. The server does not carry unwritten frames over.
Frames still in a queue after the drain limit are lost (logged). A frame that
arrives while the stop is under way is not recorded and not acked. A client
that sees the new `boot_id` drops what it holds (section 9). An old client
without that rule still resends, and the server takes the frames above the
saved `last`.

The client keeps every frame until a `Pong.input_acked` covers it (cap 64 KiB).
After a reconnect it resends all unacked frames in order. Input from a
client also marks that client as the pane's latest active client (section 6),
except `auto` input (below).

**Input flags (AD-025, capability `input_flags`).** When both sides list
`input_flags`, every frame on that connection has one more byte after `seq`:

```
[16 bytes pane uuid][u64 BE seq][u8 flags][input bytes]
```

- Bit 0 set: `auto`. The bytes are an automatic terminal reply (DA, CPR,
  kitty-keyboard answers, focus reports). Bit 0 clear: `user` (keys, paste,
  mouse). Bits 1 to 7 are reserved. Senders set them to 0 and receivers
  ignore them.
- Without the capability the frame has no flags byte, as above, and counts as
  `user`. A client must not send a flags byte unless the server's `Welcome`
  lists `input_flags`.
- An `auto` frame never makes its client the pane's latest active client.
- A client that did not negotiate `input_flags` sends the old frame with no
  flags byte, and the server treats all of it as `user` input, as before. All
  clients of this repo send the capability.
- The server writes an `auto` frame to the PTY only when its sender is the
  pane's size owner (the latest active client, section 6) and that owner has a
  stream attached to the pane. If the owner has none, or the pane has no owner
  yet, the first attached client is the owner. Only a client with a stream on
  the pane can become owner, by `user` input, `resize_pane` or `focus_tab`. `auto` frames from every
  other client are dropped, but they still go through dedup and are acked like
  any other frame.
- When the owner detaches (`detach_pane`) or disconnects, the client among
  those still attached to the pane whose `user` input came most recently
  becomes the owner, and the PTY takes the size that client asked for. If no
  client is attached, the pane has no owner.

## 6. Size policy (AD-020)

The server keeps, per pane, the last size each attached client asked for with
`resize_pane`, and the latest active client (last input frame or `focus_tab`
for that pane). The PTY size is the latest active client's size. A change
emits `PaneResized`. Clients render at the PTY size and letterbox.

**Size owner (AD-020, AD-025).** One client owns each pane: the latest active
client while it has a stream on the pane, otherwise the first client attached.
A pane nobody has a stream on has no owner. The owner is visible:

- `get_pane_info` has `size_owner`: `{ client_id, client_kind, client_name }` or
  `null`.
- `PaneResized` has the same `size_owner` field. The field is absent when there
  is no owner, and an older decoder ignores it. It needs no capability.
- `SizeOwnerChanged { pane_id, owner }` (capability `size_owner`) is sent when
  the owner changes, `owner` is `null` when the pane has none. It comes before
  the `PaneResized` that the new owner's size causes. Events of one pane arrive
  in the order the server made them.
- A client that opens a pane takes the size (`resize_pane`) only when
  `size_owner` is `null`. Otherwise it renders at the PTY size and letterboxes.

A **local socket client** (the GTK app, tools) counts as a client. It names
itself by passing `client_id` (a UUID) and `client_name` to `subscribe_layout`
or `subscribe_output`. That connection is an attached client until it closes,
and `list_clients` returns it with `client_kind: "local"` (one entry per
`client_id`). A local client has a stream on a pane only while it has a
`subscribe_output` on that pane, and only such a client can own the pane. When
that connection closes the client lets go of the pane at once, though it stays
connected. Its `resize_pane` calls pass the same `client_id`. A resize with a
`client_id` makes that client the owner at once, because a user at a local
window is the active user. Without a `subscribe_output` on the pane the size
applies and the owner does not change. A remote `Hello` with `client_kind`
`local` is refused (`bad_request`, message `bad_hello`). `unknown` is accepted and listed as `unknown`. A `client_id` that is not an attached streaming
client is an error (`invalid_params`). A one-shot `resize_pane` without
`client_id` (the CLI) applies the size and does not change the owner. When the
last connection of a local client closes, the owner passes on like any owner
that leaves: to the attached client with the most recent user input.

## 7. Pairing (`forgetty/pair/2`, AD-021)

QR payload (JSON, then QR): `{ v: 2, node_id, machine, relay, secret }` where
`secret` is 16 random bytes, base32. `forgetty-server pair` opens a 60 s window
with a fresh secret and prints the QR as ASCII. Client sends
`PairRequest { secret, device_name, client_kind }` on a bi stream. A wrong or
expired secret gets `PairResult { ok: false, reason }`. One success closes the
window. The registry stores the device. The server logs the device name and id.

## 8. Pane output stream and resume

First frame (MessagePack): `PaneStreamHeader { pane_id, start_offset: u64, reset: bool, rows, cols }`,
plus the optional fields `snapshot_len` (section 12) and `replay_end`.
All following frames are raw PTY bytes. Byte `n` of the stream after the header
is pane offset `start_offset + n`.

`replay_end: u64`, optional: the pane offset where replay ends. For a stream
without a snapshot it is the log's `total_written` at the moment the stream
starts. Pane bytes before that offset are history the client is catching up on
(a client must not answer terminal queries that appear in them), and bytes from
that offset on are live (it answers them). For a snapshot stream it is the
snapshot's offset, which is `start_offset`, so every pane byte after the
snapshot is live. The field is omitted when absent. A decoder that does not know
it ignores it (the header is a map and no decoder uses `deny_unknown_fields`),
and a client that gets no `replay_end` uses its old rule.

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

With `snapshot_catchup` (section 12), a start further back than half the ring
from the head begins with a VT snapshot, whatever the request was. That
includes a fresh attach (`from_offset = None`) on a pane whose log is over half
a ring: it gets the snapshot of the screen as it is now (fast first paint), not
the oldest byte of the log. The scrollback before the screen is not sent (a
known gap, section 12).

## 9. Client reconnect rules (`forgetty-client` crate)

- On connection loss: redial at once, then backoff 100 ms, 250 ms, 500 ms, 1 s,
  2 s, then every 2 s. Never give up while the app is in the foreground.
- **Redial budget.** A session that lasted 5 s or more is redialed at once and
  ends the streak of short sessions. Every other redial takes a token from a
  bucket of 10 that refills at one token per 3 s. While a token is left, a link
  the server answered (a response, an event or pane data, but not a `Failure`
  to a `session_init` request) or that finished `session_init` is redialed at
  once, and a silent link climbs the ladder above. With no token the client
  sleeps the ladder step for the current streak of short sessions, heard or
  not, and the bucket does not refill during that sleep. `reconnect_now()` and
  `network_changed()` never add a token. `reconnect_now()` cuts a normal backoff
  but not the empty-bucket sleep. A `network_changed()` that follows a real
  change of the network (the local address the system picks for a route to
  the internet, over IPv4 or IPv6, is another one than at the last call) earns
  one immediate dial with no token, and that dial cuts the empty-bucket sleep.
  It happens at most once per 3 s. A repeat of the same event cuts nothing and
  does not use up that one cut. So a server that answers once and closes gets
  about 10 dials, then one every 2 s (about 25 in 30 s), not a tight loop, and
  a network that seems to change all the time adds at most one dial per 3 s.
- **Redial speed.** Once the server is reachable again the client is connected
  within 1 s. A QUIC connect that gets no answer repeats its first packet only
  after a timeout that starts near 1 s and doubles, so one long attempt would
  notice a server that came back up to several seconds late. The client
  therefore starts another connect attempt 400 ms, 800 ms, then 1.6 s after
  the dial starts (and every 800 ms after that), each sending a fresh first
  packet at once. The attempts overlap and the first to connect wins. The
  others finish in the background and are closed (2 s at most), so the server
  is not left with half-open connections. Every way a dial ends (its own
  timeout, the 1 s direct limit, `network_changed()`, the other leg winning)
  retires its attempts like this, never drops one half way through the
  handshake. These attempts are inside one dial
  and do not use up the backoff above. `network_changed()` aborts the dial in
  flight and dials at once with the backoff reset. With no connection it also
  binds a fresh endpoint (same key) at once, because the endpoint that has been
  failing since the network went away is the likely fault; an endpoint that was
  itself replaced in the last 3 s is kept. With a silent but open connection the
  fresh endpoint comes 1.5 s after the call if nothing connected before. A cached direct address and
  the addresses of the pairing payload (relay) are dialed in parallel; the
  direct connect gets 1 s.
- On `close()` the client waits up to 300 ms for the server to ack the input it
  sent (a `Ping` asks for the ack), then closes. What is not acked by then is
  dropped.
- After `Welcome`: send `subscribe_events` and then `attach_pane` with the last
  known offset for every pane the UI shows, all at once, without waiting for an
  answer in between. At most 16 `attach_pane` requests wait for an answer at the
  same time: when 16 are out, the client takes the oldest answer before it sends
  the next. With the subscribe that is 17 of the 64 requests in flight, so the
  app's own requests have room during a resume of any number of panes. The server takes them in that order and answers each as it
  is done. A request of the app goes out on the same stream at once and does
  not wait for any of them.
- The UI shows "reconnecting" after 1 s without a connection, never sooner.
- Initial state is `Reconnecting { since }`. States: `Connected`,
  `Reconnecting { since }`, `Failed(reason)`, `Closed` (after `close()`).
- `ClientHandle::network_changed()`: the app calls it on an OS network change
  (Android `ConnectivityManager` callback, Windows `NotifyIpInterfaceChange`).
  The client tells iroh about the change and sends an immediate `Ping`. Without
  a `Pong` in 1 s it redials.
- Input typed while disconnected is buffered (cap 64 KiB) and sent after resume.
- **Restart rule.** The client remembers `Welcome.boot_id`. When a new
  `Welcome` has a `boot_id` different from the last one, the server restarted
  and its programs are gone. The client drops its whole unacked input buffer,
  does not send it, and tells the user how many bytes were not sent (the Rust
  client raises `EventItem::InputLost { bytes }` and counts them in
  `lost_input_bytes()`; the Windows and Android apps show it). Input typed
  after the new `Welcome` is sent as usual. If either `Welcome` has no
  `boot_id`, nothing is dropped.

## 10. Crates

- `forgetty-proto`: types, MessagePack codec, frame reader/writer
  (`tokio::io::AsyncRead/AsyncWrite`), pane-uuid input codec. No iroh dependency.
- `forgetty-client`: iroh endpoint, pairing, connect, handshake, reconnect,
  per-pane offset tracking, event stream. Public async API used by Android
  (through JNI) and Windows. No UI code.
- Server: `forgetty-server` implements v2 with `forgetty-proto`.

## 11. Amendments (2026-10-21, from the P2b implementation)

1. Input dedup table (section 5) is persisted to `input_seq.json` on graceful
   stop and loaded on start. After `kill -9` it is lost. Clients that know
   `boot_id` (section 9) never resend across a restart. An older client can
   repeat input once after a crash. This is accepted.
2. A reader that falls behind the whole log gets its pane stream reset with
   QUIC stream error code `0x11 log_lost` (same name as in section 12). The
   client re-attaches with `from_offset` and gets `reset = true`.
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
8. `Hello.client_name` is peer text. The server strips control characters and
   keeps at most 64 characters before it logs or lists the name.
9. A bad frame on the input stream (over the cap, shorter than the 24 byte
   header, or not decodable) closes the connection with `0x02 frame_too_large`,
   so the client dials again. A clean end of the stream does not.
10. `resize_pane` rows and cols are clamped to `1..=1000` (0 is `bad_request`
    over v2). The local socket clamps the same way.
11. Input to a pane goes through a bounded per-pane queue (256 KiB) and a
    writer thread. A pane whose program does not read stalls the input stream
    of the client that sends to it and nothing else. `Pong.input_acked` covers
    a frame only after the PTY took its bytes.
12. The byte log survives a failed disk write by retrying with backoff. After a
    crash (the log was not sealed by a graceful stop) the server moves the
    retained log forward in offset space, to a first offset past anything the
    old process could have sent. The content and the replay of a restored pane
    stay intact. An offset a client kept from before the crash is older than
    the log: an attach gets `reset = true`, a stream that was reading it gets
    `0x11 log_lost`, never other bytes under that offset.

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
  the log is within 1/8 ring of it. At most 2 concurrent builds server-wide,
  at most 1 per node (the iroh node id of the device, not the peer-chosen
  `client_id`): a node that asks for many panes gets them one after the other.
  The 2 build permits go to the waiting nodes in turn (round robin), so one
  node cannot keep another from a build. A node that joins a build still
  waiting in another node's queue can start it on its own turn. A node also
  has at most 2 snapshot requests in the hub at once, queued or running,
  across all its panes. Its next request waits at the node's own gate and
  takes no place in the queue of another node, so one node with 100 busy
  panes delays the catch-up of another node by 2 build times at most.
- Pacing, per `(node id, pane)`, kept after the stream and the connection end
  (attaching again does not reset it). A far-behind attach (a resume further
  back than half the ring, or a first attach to a pane with that much
  history) spends a resume token: a node and pane have 6, at most. A `Strong`
  claim (below) earns one, at the moment its catch-up ends with that claim.
  Time earns one per 5 minutes, up to the 6, for the phone whose pane went
  quiet after the output it missed: no live byte follows its snapshot, so it
  has no claim to show. Seconds of waiting earn nothing. A peer that never
  reads therefore gets one extra snapshot per pane per 5 minutes. A first
  attach spends a token like any other.
  A phone that sat in a pocket while a build printed 10 MB is the normal case:
  it has its tokens and waits for nothing but the snapshot build. A token does
  not lift the floor: two snapshots of one node and pane start at least 100 ms
  apart. With no token left, the attach waits out the backoff below (2 s,
  doubling to 10 s, at most 10 s of wait). The backoff does not start over
  because the peer waited between attaches: only a `Strong` claim, or 20 s
  without any attach, ends it. Waits of attaches that are replaced before
  their snapshot still add up, so a peer that attaches every 2 s keeps
  waiting while it does. A peer that never reads gets 6 snapshots, then the
  backoff and one more per 5 minutes: a peer that attaches again every 2 s for
  a minute gets at most 8 (6 and 2). A resume with a small gap (less than half
  the ring) never waits on the pacing.
  - A stream whose peer reads (it took live bytes at 1 MiB/s or more since its
    last snapshot, or took a snapshot in under 500 ms that the log outran
    before any live byte) gets a burst of 6 catch-ups at least 100 ms apart,
    then one per second. It never backs off.
  - A stream whose write made no progress for 2 s, or whose peer reads too
    slowly to show it reads, waits at least 1 s between catch-ups, with backoff
    to 10 s while it keeps needing them. A catch-up of the first kind ends
    that backoff only when it proves that the peer reads: it took at least
    512 KiB of live bytes at 1 MiB/s or more, more than the room a peer that
    never reads has left in its QUIC window. A claim without that proof costs a
    token and leaves the backoff alone. A claim of one or two chunks in a few
    microseconds is what a peer that never reads shows too: 4 of them in a
    row, with no proven read between, are believed, the next are treated as a
    stall. After a proven read the next stall
    starts again from 1 s.
- Scrollback for a fresh attach is a known gap. A client that attaches to a
  busy pane sees the screen, not the history above it. The server does not
  build it.
- The snapshot has no scrollback, no palette, no OSC 10/11/12 colours. A client
  without the capability gets `0x11 log_lost` and re-attaches.
- Connection close codes: `0x12` the Control queue was half full (4 MiB) for
  10 s and the client drained less than 32 KiB/s of it, `0x14` more than 64
  requests in flight, `0x15` more than 4 connections from one device (the
  oldest closes).
- Control memory per connection: the 8 MiB budget counts queued bytes plus
  bytes set aside for requests still running (64 KiB each, 4 MiB for
  `capture`). The server stops reading requests while the total is over budget.

## 13. Methods added by P5

- `api.list`, `api.describe { method }`: self-describing API (name,
  description, input/output JSON Schema, examples). Allowed over v2.
- `set_label { target: "workspace"|"tab"|"pane", id, label }`: emits
  `LayoutChanged`. Layout values carry `label` on tabs and a `pane_labels` map.
  Clients show the label when set, else the title.
