
## 5. Examples

With the CLI:

```sh
forgetty api list -q                       # method names
forgetty api describe capture              # schemas and examples
forgetty api call server_info               # one call
forgetty api call -p build capture '{"trim":true}'   # -p fills pane_id
forgetty api call new_tab @params.json      # params from a file ("-" reads stdin)
```

With `socat`:

```sh
SOCK="${XDG_RUNTIME_DIR:-/tmp/forgetty-$(id -u)}/forgetty/forgetty.sock"
echo '{"jsonrpc":"2.0","method":"api.list","id":1}' | socat - UNIX-CONNECT:"$SOCK"
PANE=$(echo '{"jsonrpc":"2.0","method":"new_tab","params":{},"id":2}' \
  | socat - UNIX-CONNECT:"$SOCK" | jq -r '.result.pane_id')
echo "{\"jsonrpc\":\"2.0\",\"method\":\"send_input\",\"params\":{\"pane_id\":\"$PANE\",\"data\":\"bHMK\"},\"id\":3}" \
  | socat - UNIX-CONNECT:"$SOCK"
```

Watch the binary stream of one pane:

```sh
( echo "{\"jsonrpc\":\"2.0\",\"method\":\"subscribe_output\",\"params\":{\"pane_id\":\"$PANE\"},\"id\":1}"; sleep 2 ) \
  | socat - UNIX-CONNECT:"$SOCK" > /tmp/sub.bin
head -c 200 /tmp/sub.bin | xxd | head
```

## 6. Out of scope

- Rust types. Read `crates/forgetty-socket/src/types.rs` for the schema types.
- The iroh transport. See `docs/architecture/PROTOCOL_V2.md`.
- OSC parsing and rendering. The server scans OSC status on the hot path and
  parses the screen only for `capture`. Clients own the full VT parser.
