# Forgetty server on Windows (design, decided 2026-10-26)

Goal: local shells on Windows live in a per-user Forgetty server, as on Linux, so they survive closing the window. Phones and other devices can attach to it over iroh when the user opts in.

## Decisions
1. Data plane: protocol v2 over iroh on loopback (`127.0.0.1:<port>`, relays off, address pinned). No new IPC data protocol. The in-process ConPTY `LocalBackend` stays as a fallback and for the smoke.
2. Control plane: a named pipe `\\.\pipe\forgetty-<user SID or name>` serving the existing forgetty-socket JSON-RPC (made transport-generic). User-only DACL, `reject_remote_clients`, first-pipe-instance, client token SID check on the server, server PID token SID check on the client (squatting).
3. Local trust: the app auto-pairs through the pipe (`pair_open` with a short ttl, then pair/2 over loopback). The local app device shows as "This PC (local app)" and is not offered for revoke (same-user processes can always re-pair, same trust domain as the Linux socket).
4. ConPTY: a small own wrapper in forgetty-pty for Windows (the `windows` crate), with no INHERIT_CURSOR and no WIN32_INPUT_MODE. Still strip `CSI ?9001 h/l` before the byte log as a guard. Job Object per pane with KILL_ON_JOB_CLOSE (tree kill), plus one server-wide job so a server crash kills children. Exit watcher on the process handle. Ctrl+C as 0x03. `CancelSynchronousIo` for a blocked writer.
5. State: `%LOCALAPPDATA%\forgetty\server`, separate from the client folder (`%APPDATA%\forgetty`). Protected user-only DACLs through a Windows implementation in `forgetty-core::fsutil` (move the helpers from `clients/windows/remote/src/acl.rs`). Fix the wrong doc at `platform.rs:313`.
6. Binaries: the server/CLI is `forgetty.exe` (same CLI as Linux: `forgetty server run|start|stop|status`, `forgetty pair`, `forgetty ls`, ...). The app is `forgetty-win.exe`. The MSI stops installing a copy of the app as `forgetty.exe`. `ghostty-vt.dll` next to both.
7. Reachability: loopback only by default. Binding other addresses (`bind_addrs`) is an explicit opt-in in config, with the firewall prompt as the user's choice.
8. Lifetime: a HKCU Run key starts `forgetty server start` at login. The app also starts it on demand and restarts it when connect fails. No scheduled task, no service.
9. Logoff and reboot: panes die, cold restore re-spawns from the saved layout (same as Linux without linger).
10. Cwd: spawn cwd only in the first release. Live cwd via the PEB is a later item.
11. Server-side argv rules on Windows: PATH-only lookup, no UNC, device or `\??\` paths (port `clients/windows/shell-core/src/resolve.rs` rules), because remote clients can send argv.

## Work packages (build order)
- WP0 (S): Windows CI job (`windows-latest`) running `cargo check` and `cargo test` for forgetty-pty, -session, -socket, -sync, -workspace, -server. Gate `crates/forgetty-server/build.rs` linker args on target OS.
- WP1 (M): portability. fsutil ACLs, platform dirs and runtime dir, state dir separation, `File::try_lock` for both locks, `libc` gating, `home_dir_fallback` via `dirs`, `skill.rs` flags, crash handler stub (panic hook kept), Windows ctrl events instead of unix signals, `byte_log.rs` and `registry.rs` (`seek_write`, no dir fsync), `foreground_pgrp` cfg.
- WP2 (M-L): forgetty-pty Windows (decision 4), shell selection that honors config and `detect_shell`, no `where` console flash.
- WP3 (M): pipe transport for forgetty-socket, `sockpath` Windows, CLI client over the pipe, `server start/stop/status` with DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB.
- WP4 (S): server-side argv rules (decision 11).
- WP5 (M): lifecycle and packaging (decisions 6, 8), MSI changes.
- WP6 (M): Windows client integration. Local-server supervisor (find, start, auto-pair via pipe, connect over loopback), the local machine backed by RemoteBackend, first-run UX, fallback to in-process ConPTY.
- WP7 (S-M): smoke. Close the window and panes survive. Kill and restart the server (cold restore). Key-to-frame latency against the local server (target: within 2 ms of the in-process path). Non-admin firewall check.
