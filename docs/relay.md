# Self-hosted relay

Forgetty phones and PCs reach the server over a direct UDP path (QUIC, port
47297). A relay is the fallback when no direct path works: a phone behind
carrier NAT, a firewall that blocks UDP, the first second of a new connection.
The relay only forwards encrypted packets. It cannot read terminal data.

By default Forgetty uses the public relays of the iroh project (n0). This page
shows how to run your own `iroh-relay` on the server instead, as a
`systemd --user` service, and how to make Forgetty use it.

Rule (V3 plan 7.2): direct UDP first, a relay on your own server as the
fallback, not a public relay.

## 1. Install iroh-relay (pinned to 0.97)

The `iroh-relay` version must match the `iroh` crate in Forgetty. Forgetty 0.1
uses iroh 0.97, so install the 0.97 line of the relay:

```sh
cargo install iroh-relay --version '=0.97.0' --features server --locked --root ~/.local
iroh-relay --version        # iroh-relay 0.97.0
```

This puts the binary in `~/.local/bin/iroh-relay`. Do not use `@latest`: a
newer relay may speak a newer relay protocol than the Forgetty build in your
pocket.

## 2. Config file and user unit

```sh
mkdir -p ~/.config/forgetty ~/.local/share/forgetty/iroh-relay
cp /usr/share/doc/forgetty-server/examples/iroh-relay.toml.example ~/.config/forgetty/iroh-relay.toml
install -Dm644 /usr/share/doc/forgetty-server/examples/iroh-relay.service \
    ~/.config/systemd/user/iroh-relay.service
```

(From a source checkout the files are `dist/relay/iroh-relay.toml.example` and
`dist/relay/iroh-relay.service`.) Edit `~/.config/forgetty/iroh-relay.toml`:
set `hostname` and `contact`, or switch to your own certificate (section 3).
Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now iroh-relay
journalctl --user -u iroh-relay -f
```

`loginctl enable-linger "$USER"` keeps it running without a login session (the
same command as for the Forgetty server).

## 3. HTTPS

The relay speaks HTTPS: phones check its certificate like a browser does. You
need a DNS name (`relay.example.org`) that points to the server.

**Option A: Let's Encrypt, done by iroh-relay itself.** In `[tls]`:

```toml
cert_mode = "LetsEncrypt"
hostname = "relay.example.org"
contact = "you@example.org"
prod_tls = true
```

It issues and renews the certificate with the TLS-ALPN-01 challenge. Let's
Encrypt connects to **port 443** of the name, so port 443/tcp must reach the
relay (see "Ports"). Account key and certificates are stored in the working
directory of the service, `~/.local/share/forgetty/iroh-relay`. Start with
`prod_tls = false` (staging) if you test, then switch.

**Option B: a certificate you provide.** Use certbot (DNS-01), acme.sh, or any
CA, and point the relay at the files:

```toml
cert_mode = "Reloading"      # or "Manual": the same, without re-reading
manual_cert_path = "/home/you/.config/forgetty/relay/fullchain.pem"
manual_key_path  = "/home/you/.config/forgetty/relay/privkey.pem"
```

`Reloading` picks up a renewed certificate without a restart. With your own
certificate the relay may listen on any port, for example
`https_bind_addr = "[::]:8443"`; the relay URL then carries the port:
`https://relay.example.org:8443`.

**A user service cannot bind a port below 1024.** Pick one:

- Allow it once, as root: `sysctl -w net.ipv4.ip_unprivileged_port_start=80`
  (persist it in `/etc/sysctl.d/90-unprivileged-ports.conf`). Then ports 80 and
  443 work as in the example config.
- Listen on 8443 and 8080 and redirect in the firewall:
  `nft add rule inet nat prerouting tcp dport 443 redirect to :8443`. Let's
  Encrypt (option A) still validates through the redirect.

## 4. Ports and firewall

| Port | Proto | What | Open it? |
|------|-------|------|----------|
| 443 (or your `https_bind_addr` port) | TCP | the relay over HTTPS, and the Let's Encrypt check | yes |
| 7842 | UDP | QUIC address discovery (`quic_bind_addr`); clients learn their public address and hole-punch | yes |
| 80 | TCP | plain HTTP, only a captive-portal check when TLS is on | optional |
| 47297 | UDP | the Forgetty server itself (direct paths) | yes, this is the fast path |
| 9090 | TCP | relay metrics | no, keep `127.0.0.1` |

```sh
# ufw
sudo ufw allow 443/tcp && sudo ufw allow 7842/udp && sudo ufw allow 47297/udp
# firewalld
sudo firewall-cmd --permanent --add-port=443/tcp --add-port=7842/udp --add-port=47297/udp && sudo firewall-cmd --reload
```

Cloud providers (OVH, AWS, ...) often have a second firewall in front of the
machine. Open the same ports there.

## 5. Point Forgetty at the relay

Either pass the URL (repeat the flag for more than one relay):

```sh
forgetty server run --relay-url https://relay.example.org
```

or set it in `~/.config/forgetty/config.toml`:

```toml
relay_urls = ["https://relay.example.org"]
```

`--relay-url` replaces the list in the file. With neither, behaviour is
unchanged: iroh's default relays.

Under systemd use a drop-in so the package unit stays untouched:

```sh
systemctl --user edit forgetty-server
```

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/forgetty server run --relay-url https://relay.example.org
```

(Tarball install: `%h/.local/bin/forgetty`.) The config file route needs no
drop-in. Restart with `systemctl --user restart forgetty-server`. The log line
`relays: https://relay.example.org/` confirms it.

An invalid URL stops the server at start with a clear error.

### Pairing

`forgetty pair` now writes your relay into the pairing payload (`relay`, and
`relays` when there are several). The app reads the payload, stores the relay
URLs with the paired machine and uses them for every later connection. Pair
again after changing the relay: a machine paired earlier keeps the relay it was
paired with.

## 6. Check it

```sh
curl -sI https://relay.example.org/ | head -1      # HTTP/2 200
forgetty pair                                      # payload shows your relay
```

The gate `scripts/p6_gate.sh` runs the same path on localhost: it starts
`iroh-relay --dev`, starts the server with `--relay-url http://127.0.0.1:3340`
and connects a `forgetty-client` that binds no UDP socket, so every packet must
go through the relay.

## Notes

- With a custom relay the endpoint still publishes its id and relay URL to the
  iroh discovery service (the `N0` preset). Pairing and reconnecting do not
  depend on that: the URL comes from the QR payload.
- Limit who may relay with `access = { allowlist = ["<endpoint id>"] }` in the
  relay config: the server id (`forgetty server status`) and each phone.
- `iroh-relay --dev` runs plain HTTP on port 3340 for tests. Never expose it.
