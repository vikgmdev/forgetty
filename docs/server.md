# Forgetty server and CLI

`forgetty` is one binary. It is the durable per-user terminal server (AD-017)
and its CLI. It needs no GTK, no display server and no `pkg-config`.

## Install

From a release (no Rust needed): see `packaging.md`.

```sh
curl -fsSL https://raw.githubusercontent.com/vikgmdev/forgetty/main/scripts/install.sh | bash
```

It installs the deb or rpm on a machine that has dpkg or rpm, or the tarball
into `~/.local` with `--tarball` (no root). The packages put the user unit at
`/usr/lib/systemd/user/forgetty-server.service` and print the two commands
that enable it. Nothing runs as root.

## Build and install from source

```sh
cargo build --release -p forgetty-server
install -Dm755 target/release/forgetty ~/.local/bin/forgetty
```

The binary finds `libghostty-vt.so` through its RUNPATH (`$ORIGIN/lib`,
`$ORIGIN`). Keep the `.so` next to the binary or under `~/.local/bin/lib`.

## Run under systemd (user service)

```sh
install -Dm644 dist/systemd/forgetty-server.service ~/.config/systemd/user/forgetty-server.service
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now forgetty-server
```

`enable-linger` keeps the user manager, and so the server, running with no
login session.

```sh
systemctl --user status forgetty-server
journalctl --user -u forgetty-server -f
```

On `systemctl --user stop` or `restart` the unit sends SIGTERM to the server
only (`KillMode=mixed`, `TimeoutStopSec=10`). The server freezes the layout
(a pane exit after that point never removes a pane from the saved layout),
flushes and fsyncs the byte logs and the session file, then exits. The shells
get SIGHUP when the PTY masters close, and systemd kills what is left. A
restart therefore comes back with every pane.

Without systemd: `forgetty server start` runs the server detached and logs to
`~/.local/share/forgetty/server.log`.

## What the server keeps

- One server per user. A second `forgetty server run` exits with "already
  running" (file lock `server-<id>.lock`, exit code 75, which `server start`
  treats as "another start is under way"). First-run state (`server_id`,
  `identity.key`) is created while holding `server.lock` in the state dir, so
  parallel first starts end with one server and one identity.
- The server id lives in `~/.local/share/forgetty/server_id`. A `server_id`
  that is not a UUID is an error: the server refuses to start rather than come
  up with a new id and an empty layout. The layout is in `sessions/<id>.json`
  (written through a unique temp file, fsync, rename, directory fsync; the
  previous good copy is `sessions/<id>.json.bak`; a corrupt file loads the
  `.bak`, logs an error and is kept as `.corrupt`). Byte logs are in `logs/`
  and are written to the file at the end of every output burst, so a
  `kill -9` loses no scrollback. All survive a restart.
- The state directory is mode 0700. `identity.key`, `authorized_devices.json`,
  `server_id` and the session files are created 0600 at open time (not
  chmod-ed afterwards). Files an older build left wider are fixed at startup.
- Pane cwd: `cwd` in `ls` is a display hint (OSC 7 from the shell, accepted
  only for an empty, `localhost` or local host name). `spawn_cwd` is the real
  directory (`/proc/<pid>/cwd` of the foreground process group leader) and the
  only source for the cwd of new tabs and splits.
- A client disconnect never ends a pane or the server. After a server restart
  the layout is restored and byte logs are replayed into the panes. Processes
  do not survive a restart (a restored pane starts a fresh shell).
- Socket: `$XDG_RUNTIME_DIR/forgetty/forgetty.sock` (else
  `/tmp/forgetty-<uid>/forgetty.sock`). The directory is mode 0700 and the
  socket 0600. The server refuses to start on an unsafe directory (wrong owner,
  symlink, group/other bits), drops connections from other uids
  (`SO_PEERCRED`), and the CLI refuses a socket it does not own.
- A pane closes when its process exits. `forgetty new --keep` keeps it
  instead: it shows the exit status, stays listed as `exited N`, and is removed
  by the `close_pane` socket method. The server records every pane exit code.

## Protocol v2 (`forgetty/rpc/2`)

The server accepts the iroh ALPN `forgetty/rpc/2` (see
`docs/architecture/PROTOCOL_V2.md`) next to `forgetty/stream/1`, which stays for
one more release. Control requests go through the same dispatch table as the
local socket, so a method behaves the same on both. Methods that need a local
trust root (`pair_*`, `get_pairing_info`), `send_input` (v2 has its own input
stream) and the `qa_*` test methods are not reachable over v2.

- The UDP port is fixed: `forgetty server run --port N`, default 47297 (`0`
  picks a free port, for tests). A second server on a taken port exits with a
  bind error.
- Every pane has an absolute byte offset. The ring and the disk log know the
  offset of their first byte. The first offset is stored in
  `logs/<pane>.log.off`, so a client resumes at its offset after a server
  restart. The ring is 8 MiB per pane (`byte_log_ring_kb`).
- The byte log never drops output. Only the server's own disk writer can hold a
  PTY: when more than 4 MiB wait for it, the server stops reading that PTY
  until the queue drains (event driven, no polling). No client stream, however
  slow, ever holds a program (AD-023). Output that arrives after shutdown
  starts is not logged, so the log on disk is exactly what clients saw.
- A slow client catches up by snapshot (capability `snapshot_catchup`). A pane
  stream that falls more than half the ring behind, or makes no write progress
  for 2 s, is reset (stream code 0x13) and replaced by a new stream with
  `reset: true`, `start_offset` at the head and `snapshot_len` bytes of VT
  first: the screen, cursor, modes (alternate screen, bracketed paste, mouse,
  keypad, cursor visibility), scroll region, tab stops, pwd, keyboard modes and
  title, built from the ring on the cold path. Then live bytes follow. The
  snapshot has no scrollback and no palette (cells keep palette indexes), and
  only covers what the ring holds. A client without the capability keeps the
  old rule: it reads from the disk log, and a client behind the whole log is
  reset with code 0x11 and resumes with a reset.
- Snapshot work is bounded. All streams share one build per pane (a build in
  progress is joined, a result younger than 250 ms is reused), at most 2
  builds run at once server wide, and a build nobody waits for any more is
  dropped before it starts. A result is reused only while the log has not
  moved more than an eighth of the ring past it. One stream takes a catch-up
  at most once per second, and waits twice as long each time it needs another
  soon, up to 10 s. A stream that keeps pace with the program is exempt from
  the backoff and waits 100 ms at most: since its last snapshot it took live
  bytes at 4 MiB/s or more, or the log ran past that snapshot before any live
  byte went out although the snapshot reached the peer within 500 ms (the
  build alone took longer than the ring holds at the program's speed). A write
  that stalls for 2 s never counts as keeping pace. Bytes accepted into the
  QUIC send buffer prove nothing about a peer that never reads, so the
  exception is capped: 6 per (client, pane) in a burst, then one per 10 s;
  after that the normal backoff applies. The pacing state is kept per
  (client_id, pane) across attaches and connections, so attaching again does
  not reset it.
  A pane has one live stream per connection: a new `attach_pane` replaces the
  old one. An attach counts as a request in flight until its stream is open.
- A stopping server answers `Hello` with `Error { code: "server_stopping" }`.
  The client retries it with its normal backoff.
- Each v2 connection queues at most 8 MiB of Control messages (counted in
  bytes, with a fixed cost per message). It holds at most one `LayoutChanged`,
  and a newer one replaces the pending one in place. Past the budget the
  connection stops reading requests (QUIC flow control pushes back on the
  client) and pulls no more events. A request that runs on its own task sets
  bytes aside against the budget until its answer is queued (64 KiB, 4 MiB for
  `capture`), so requests in flight cannot owe more than the budget. When the
  queue is half full and the client drains less than 32 KiB/s over 10 s, the
  server closes the connection with code 0x12. At most 64 requests run per
  connection, one more closes it with code 0x14. A device holds at most 4
  connections: a fifth closes the oldest with code 0x15. Responses and Pongs
  are never dropped, and nothing spawns a task per queued message.
- Input is deduplicated by `(client_id, input_epoch)` and `seq`. The server
  writes the dedup table to `input_seq.json` in the state dir when it stops
  and loads it at start, so a frame the client did not see acked is not written
  to the new shell twice. A `kill -9` loses the table.
- The pane size follows the last active client (AD-020). The first client that
  resizes a pane without an active owner becomes the owner. Any later client
  becomes the owner when it sends input or focuses the tab.
- Each v2 connection is in the client registry (`client ls`). A stopping server
  closes a new v2 connection without a reply, and the client retries.
- The release build with `--features qa-tools` adds `qa_*` socket methods that
  the client test suites use to drive a real server (drop connections, hold
  acks, read the byte log). They are not compiled into a normal build.

## CLI

```text
forgetty server run                 foreground (what systemd runs)
forgetty server start               start detached if not running
forgetty server stop [--if-idle]    --if-idle refuses while a client is attached
                                    (a stop that begins refuses new attaches)
forgetty server status [--json]     pid, uptime, version, protocol, panes, clients, iroh node id
forgetty ls [--json]
forgetty new [--cwd DIR] [--keep] [-- CMD...]   prints the pane id; --keep = remain on exit
forgetty send -p PANE TEXT [--enter]
forgetty send-key -p PANE KEY       enter tab esc ctrl+c up down left right
forgetty close -p PANE              close a pane (also a kept, exited one)
forgetty capture -p PANE [--trim] [--lines N] [--json]   visible screen as text
forgetty events [--json] [-p PANE]  stream events until Ctrl+C
forgetty run [--keep] [--wait] [--cwd DIR] -- CMD...   new pane running CMD;
                                    --wait blocks and exits with CMD's exit code
forgetty wait -p PANE [--status done|waiting|...] [--timeout S]
forgetty label [-t pane|tab|workspace] TARGET LABEL   set a label ("" clears it)
forgetty api list [-q] [--json] [--markdown]   every method (--markdown prints docs/socket-api.md)
forgetty api describe METHOD [--json]   description, JSON Schemas, examples
forgetty api call [-p PANE] METHOD [JSON | @file | -]   call a method, print the result
forgetty skill setup [--agent claude|codex] [--scope user|project]
forgetty skill status | show | remove
forgetty client ls [--json]
forgetty pair                       QR + payload, waits 60 s for one device
forgetty devices [--json]
forgetty revoke ID
```

A pane, tab or workspace target is, in this order: a full id, a label
(case-insensitive), a 1-based position, or a unique id suffix of 4+ characters.
Several labels or suffixes that match give exit 3 and the list of matches.

Exit codes: 0 ok, 1 failed, 2 bad command line (`api call`: `bad_request`),
3 target not found or ambiguous (`api call`: `method_not_found`), 4 server
unreachable, 5 `wait` timed out (`run --wait` returns the command's own exit
code instead).

### Self-describing API, labels and the agent skill

Every socket method has a description, input and output JSON Schemas and
examples in one table (`crates/forgetty-socket/src/api.rs`). The local socket
and protocol v2 serve the same table: `api.list` and `api.describe { method }`.
`docs/socket-api.md` is generated from it (`forgetty api list --markdown`), and
a test fails when the file is stale (`UPDATE_DOCS=1 cargo test -p forgetty-socket
socket_api_doc` rewrites it).

`set_label { target: "workspace"|"tab"|"pane", id, label }` names a target. The
server saves labels in the session file, sends a layout change to subscribers
(`LayoutChanged` over v2, `label_changed` on the local socket) and restores them
after a restart. A workspace label is its name. `rename_workspace` stays for old
clients.

`forgetty skill setup` writes a short skill file (`~/.claude/skills/forgetty/SKILL.md`,
or `.claude/skills/forgetty/` with `--scope project`; Codex: `~/.codex/skills/forgetty/`
and `.agents/skills/forgetty/`) that teaches a coding agent the commands above.

`wait` without `--status` waits for the pane process to exit. Statuses are
`idle`, `working`, `waiting`, `done`, `failed`.

## Pane state from the output (AD-019)

The server scans PTY output for a few OSC sequences with a byte-level state
machine (not a VT parser; it keeps state across read boundaries and buffers at
most 4 KiB per sequence):

- OSC 0 and 2: pane title
- OSC 7: working directory
- OSC 9, 99, 777: notifications (rate limited to 10 per second per pane)
- OSC 7501: program status, see
  <https://www.superlogical.com/rex/docs/build/program-status.md>
  (`blocked` shows as `waiting`, `error` as `failed`)
- OSC 133 `A`: a new prompt ends `working` and `waiting` status

Everything in a sequence is untrusted and sanitised before it is stored. State
shows in `forgetty ls --json`, `get_pane_info` and the event stream.
`forgetty capture` builds a terminal from the pane's byte-log ring only when
asked.

Benchmark: `cargo bench -p forgetty-session --bench osc_scanner` times the
scanner over a 100 MiB mixed stream through a real PTY. It adds under 1 % to
drain time. A stream with an OSC of interest in every few lines costs more (see
the bench output).

The CLI warns on stderr when the running server has another version than the
CLI. Restart the server then.

`--state-dir DIR` (any command) keeps data, config and the socket under `DIR`.
Use it for tests and throwaway instances.

## Pair a device (AD-021)

```sh
forgetty pair
```

This opens a 60 s window with a fresh 128-bit secret and prints the QR code and
the payload text. One device can pair in the window. A wrong, expired or reused
secret is refused. The server logs the device name and id. `forgetty devices`
lists paired devices, `forgetty revoke ID` removes one and closes every open
connection of that device, on every ALPN, within a second (the client leaves
`client ls`).

Old apps that dial `forgetty/pair/1` get an "upgrade the app" error.

## Behaviour gates

```sh
scripts/p0_gate.sh   # local socket and iroh echo
scripts/p1_gate.sh   # durability, pairing secret, restart, stop --if-idle
scripts/p4a_gate.sh  # OSC scanner, status, capture, agent CLI
scripts/p1qa_gate.sh # revoke, concurrent start, SIGTERM, kill -9, session file, modes
scripts/p2b_gate.sh  # protocol v2: client suites against the real server, exactly-once,
                     # size policy, 50 MiB burst latency, restart reconnect, port 47297
scripts/p2c_gate.sh  # AD-023: slow client never slows a program, snapshot catch-up,
                     # request flood, stalled Control reader, then all earlier gates
scripts/p5_gate.sh   # api list/describe/call, labels (resolve, persist), doc staleness,
                     # skill files, v2 api and labels, agent test with `claude -p`
```

Both run a server in a temp state dir as the current user and need outbound
internet for iroh relay and discovery.

## Relay

Without a relay of your own the server uses iroh's default relays. To use your
own `iroh-relay`, pass `--relay-url URL` (repeatable) to `forgetty server run`
or set `relay_urls = ["https://relay.example.org"]` in
`~/.config/forgetty/config.toml`. The pairing QR then carries the relay and the
apps use it. How to run `iroh-relay` as a user service, with HTTPS and the
firewall ports: `relay.md`.

## WireGuard-only server

A server that only the WireGuard tunnel reaches needs no relay and no outside
service. Start it with a bind address and no relay:

```sh
forgetty server run --port 47297 --bind 10.230.1.14 --no-relay
```

or put the same in `~/.config/forgetty/config.toml`:

```toml
bind_addrs = ["10.230.1.14"]   # the WireGuard address of this machine
relay = "none"
```

- `--bind IP` (repeatable) binds the iroh endpoint to these addresses only, on
  the fixed `--port`. Without it the server binds every address. Several
  addresses, also of one IP family (`--bind 10.230.1.14 --bind 10.240.1.112`),
  each get their own socket on the same port, and the pairing payload lists
  every one as a direct address. With `--port 0` the server picks one free
  port for all of them.
- `--no-relay` (or `relay = "none"`) turns relays off. The server never
  contacts a relay or n0: no relay connection, no address publishing, no
  discovery. It also switches off iroh's port mapping (UPnP, NAT-PMP, PCP), so
  it does not talk to the router either. It cannot be combined with
  `--relay-url`, and `relay = "none"` cannot be combined with `relay_urls` in
  the file.
- A command-line flag wins over the file. With `--no-relay` the `relay` and
  `relay_urls` of `config.toml` are ignored (not even parsed), with `--relay-url`
  its `relay`, and with `--bind` its `bind_addrs`. The server logs a WARN for
  each, for example "config relay settings ignored: --no-relay given", and
  starts. Exit 78 stays for a conflict inside one source.
- `--no-relay` needs `--bind`. Without a bind address the server refuses to
  start, prints the reason and exits with code 78 (`EX_CONFIG`, a setting that
  a restart cannot fix). `forgetty server start` prints that error at once. The
  unit has `RestartPreventExitStatus=75 78`, so systemd does not restart-loop on
  it.
- A loopback-only bind (`--bind 127.0.0.1`) with `--no-relay` is allowed, because
  the tests need it. No remote device can reach such a server, so it logs a WARN
  at startup: "bound to loopback only: no remote device can pair".
- The pairing QR then carries no relay, a `relays_disabled` flag and the bound
  addresses as `direct`. A client of an older version ignores the flag.
- A paired client uses no relay either, and no port mapping. It dials those
  addresses on every network. When nothing answers it reports
  `unreachable_direct_only` so the app can say "Can't reach the server. Is
  WireGuard on?".
- A `config.toml` that exists but does not parse stops the server with exit 78
  and an error that names the file and the line. It never falls back to the
  defaults, so a typo cannot turn the relays back on. A missing file means the
  defaults.
- Unknown keys in `config.toml` are warned about and kept when the file is saved.
  A key within edit distance 2 of `relay`, `relay_urls` or `bind_addrs` (for
  example `rellay` or `bind_addr`) stops the server with exit 78 when its value
  would be valid for that setting (`"none"` or a URL for `relay`, URLs for
  `relay_urls`, IP literals for `bind_addrs`): the setting would be ignored. A
  near key with another value (`delay = "500ms"`, `retry = "always"`) only warns.
  The settings save writes to the file the config was loaded from
  (`--config-file`, also when the load failed) and refuses to overwrite a file
  that does not load. The settings text editor runs the server's own check
  (`forgetty_config::validate_remote_settings`, with no command-line options)
  on the new text, and writes it only when the server would start with it. So
  it refuses `rellay = "none"`, a bad `relay`, bad URLs or addresses, and
  a `bind_addrs` list with no pairable address. `relay = "none"` with no
  `bind_addrs` at all is saved with the warning "relay = none needs --bind or
  bind_addrs where the server runs", because `--bind` can come from the
  command line. It does not look at the file
  on disk, so it can repair a file that does not load. Values are trimmed and
  not case sensitive: `Relay = "None"` and `relay = " NONE"` are the same
  as `"none"`. Every other configuration error (a bad `relay` value, a bad
  `--bind` address, a bad relay URL, `--no-relay` with `--relay-url`) also exits 78.
- Devices paired before this change have no `relays_disabled` flag and no
  direct address. They must pair again.
- The network watcher of the client (`link-watch`) does nothing for a client
  without relays: iroh runs no network report when it has no relay. Only the
  app's `network_changed` call wakes such a client after a network change, so
  an app must call it (Android: the connectivity callback; Windows: the
  network-change notification).

An example unit: `dist/systemd/forgetty-server-wireguard.service`.

### The tunnel is down at boot

If the bind address is not on the machine yet (the tunnel starts after the
server), the iroh bind fails. The server does not exit. The local socket and
the panes stay up, and the server retries the bind in the background (after 1
s, then 2, 4, 8 and every 10 s) and logs a WARN once a minute. When the
address appears the endpoint comes up on its own and `forgetty pair` works.
Until then `forgetty pair` and the other sync commands answer "unavailable".
The WARN says "remote access is off until <address> is available".

The retry applies when `--bind` (or `bind_addrs`) is set, and with the default
bind only when the error is "address not available". A busy port with the
default bind still ends the process with a non-zero status, so the service
manager restarts it.

A systemd user unit cannot be ordered after `wg-quick@wg0` (that is a system
unit), so the unit has no `After=` for the tunnel. The retry covers it.

### Manual checks (need root, not in the test suite)

- Tunnel up after the server: stop the tunnel, start the server, then run
  `sudo wg-quick up wg0`. Within 10 s the log shows `iroh endpoint bound` and a
  client connects. (The test suite covers the same retry with a busy port,
  because adding an address needs root.)
- Tunnel restart while the server runs: run
  `sudo wg-quick down wg0 && sudo wg-quick up wg0` and confirm a connected
  client is back within 5 s. The server's socket is bound to the address, so
  check in the log that it keeps answering after the interface returns. If it
  does not, restart the server and report it.

## Shell integration

For zsh, bash and fish the server can make the shell write the standard prompt
marks (OSC 133: prompt start, command line start, command start, command end with
the exit code) and the working directory (OSC 7). Clients get them, with the tty
mode of the pane, as `ShellState` events (`PROTOCOL_V2.md`, section 14).

It is on by default for panes the server creates after it reads `config.toml`:

```toml
shell_integration = false   # turn it off
```

The server never edits your rc files. It writes its scripts to
`<data dir>/shell-integration` at start, and sets `ZDOTDIR` (zsh), `PROMPT_COMMAND`
(bash) or `XDG_DATA_DIRS` (fish) for the shell it starts, so the scripts load by
themselves. Your own startup files run as before. A bash startup file that
replaces `PROMPT_COMMAND` instead of adding to it turns the integration off for
that shell. A shell that was started before the server read the setting keeps
running without it.

Limits:

- The integration is for the shell the server starts. `exec zsh`, a nested zsh
  or bash, `su`, `script`, vim `:sh` and tmux started from the pane run without
  it (the scripts take their variables out of the environment so those shells do
  not run hooks that exist only in the first one).
- A system `/etc/zsh/zshenv` that sets `ZDOTDIR` takes the zsh trampoline away
  and turns the integration off.
- A prompt theme that rebuilds `PS1` on every prompt (powerlevel10k, starship)
  keeps the marks: they are added after the theme runs. The first prompt of the
  shell can lack them.
- A pane that writes marks in a loop gets its states merged (a burst of 8, then
  one per 10 ms per pane, and the newest pending one per client connection), so a
  client sees where the shell is, not every step.
