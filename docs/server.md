# Forgetty server (headless)

`forgetty-server` is the durable per-user terminal server (AD-017). It needs no
GTK, no display server and no `pkg-config`.

## Build

```sh
cargo build --release -p forgetty-server
install -Dm755 target/release/forgetty-server ~/.local/bin/forgetty-server
```

The binary finds `libghostty-vt.so` through its RUNPATH (`$ORIGIN/lib`,
`$ORIGIN`). Keep the `.so` next to the binary or under `~/.local/bin/lib`.

## Install as a systemd user service

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

## Pair a device

```sh
forgetty-server --show-pairing-qr      # ASCII QR and node_id
systemctl --user stop forgetty-server  # only if you need --allow-pairing
forgetty-server --allow-pairing        # accepts the next unknown device
forgetty-server --list-devices
forgetty-server --revoke <DEVICE_ID>
```

The one-time pairing secret (AD-021) is not implemented yet. Until it lands,
run `--allow-pairing` only on a network you trust and only for the pairing
window.

## Isolated instance

`--state-dir <DIR>` keeps identity, paired devices, sessions, byte logs and the
socket under `<DIR>` (`data/`, `config/`, `run/`). Use it for tests and
throwaway instances.

## Behaviour gate

```sh
scripts/p0_gate.sh
```

Starts a server in a temp state dir, drives it over the local socket
(`new_tab`, `send_input`, `subscribe_output`) and over iroh with the QA bins
(pair, list panes, subscribe, send input). Exits 0 only if both paths see the
expected echo. Needs outbound internet for iroh relay and discovery.
