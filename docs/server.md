# Forgetty server and CLI

`forgetty` is one binary. It is the durable per-user terminal server (AD-017)
and its CLI. It needs no GTK, no display server and no `pkg-config`.

## Build and install

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

Without systemd: `forgetty server start` runs the server detached and logs to
`~/.local/share/forgetty/server.log`.

## What the server keeps

- One server per user. A second `forgetty server run` exits with "already
  running" (file lock `server-<id>.lock`).
- The server id lives in `~/.local/share/forgetty/server_id`. The layout is in
  `sessions/<id>.json`, byte logs in `logs/`. All survive a restart.
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

## CLI

```text
forgetty server run                 foreground (what systemd runs)
forgetty server start               start detached if not running
forgetty server stop [--if-idle]    --if-idle refuses while a client is attached
forgetty server status [--json]     pid, uptime, version, protocol, panes, clients, iroh node id
forgetty ls [--json]
forgetty new [--cwd DIR] [--keep] [-- CMD...]   prints the pane id; --keep = remain on exit
forgetty send -p PANE TEXT [--enter]
forgetty send-key -p PANE KEY       enter tab esc ctrl+c up down left right
forgetty client ls [--json]
forgetty pair                       QR + payload, waits 60 s for one device
forgetty devices [--json]
forgetty revoke ID
```

`PANE` is a full id or a unique id suffix of 4+ characters.

Exit codes: 0 ok, 1 failed, 2 bad command line, 3 target not found or
ambiguous, 4 server unreachable.

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
lists paired devices, `forgetty revoke ID` removes one.

Old apps that dial `forgetty/pair/1` get an "upgrade the app" error.

## Behaviour gates

```sh
scripts/p0_gate.sh   # local socket and iroh echo
scripts/p1_gate.sh   # durability, pairing secret, restart, stop --if-idle
```

Both run a server in a temp state dir as the current user and need outbound
internet for iroh relay and discovery.
