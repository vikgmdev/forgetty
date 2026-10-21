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
  restart. A client that is behind the ring reads from the disk log. A client
  behind the whole log is reset to the first available byte.
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
forgetty client ls [--json]
forgetty pair                       QR + payload, waits 60 s for one device
forgetty devices [--json]
forgetty revoke ID
```

`PANE` is a full id or a unique id suffix of 4+ characters.

Exit codes: 0 ok, 1 failed, 2 bad command line, 3 target not found or
ambiguous, 4 server unreachable, 5 `wait` timed out (`run --wait` returns the
command's own exit code instead).

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
