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

## Status (WP0 to WP3)
Done: WP0 to WP3. Server crates check and test in the `server-crates` job of `.github/workflows/windows-client.yml`.

- The control pipe is `\\.\pipe\forgetty-<SID>-<secret>[-elevated]`. `<secret>` is 128 random bits (hex) in `pipe.secret` in the server's data folder (a user-only file made with `create_new`; the server creates it, the CLI reads it, and a damaged file is an error, never a silent new secret). A guessable name could be taken first by another user to block the server, so the name cannot be known to others. A server started with `--state-dir` has its own data folder, so its own secret and pipe. Code: `crates/forgetty-socket/src/pipe.rs` (server), `crates/forgetty-server/src/pipe_client.rs` (CLI), `crates/forgetty-server/src/sockpath_windows.rs` (names and secret).
- Elevation: a process at high integrity (an elevated shell) uses the `-elevated` name, and the elevated server creates the pipe with the mandatory label `S:(ML;;NWNRNX;;;HI)` and refuses a client whose integrity level (read from the impersonated client token) is below its own. So a normal process of the same user can neither find nor open an elevated server (no UAC bypass), and an elevated CLI does not find a normal server. One data folder still means one server (the lock): an elevated and a normal server need separate `--state-dir`s. CI runs elevated, so the CI tests exercise the labelled pipe.
- The server check of the client SID runs after the first read from the connection, because Windows refuses `ImpersonateNamedPipeClient` before that. Until then nothing the client sent reaches a handler and nothing is written back. The impersonation reads the token of an elevated client too, which a process-id lookup cannot.
- The client check of the server runs in `PipeStream::connect` before the first byte is sent: it reads the user SID and the integrity level of the process behind `GetNamedPipeServerProcessId`. The SID must be the current user's, and the server's integrity level must not be lower than the client's. So a normal process that read the pipe secret and made the `-elevated` pipe before the elevated server did cannot stand in for it: an elevated CLI refuses it with "runs at a lower integrity level". The test `connect_checks_the_server_level_in_both_directions` (`pipe_client.rs`) covers both directions with a client level override. A server it cannot identify (for example an elevated server and a normal CLI) is refused, with the reason in the error.
- Default shell: `shell` in `config.toml`, then `FORGETTY_SHELL`, then `pwsh.exe`, `powershell.exe`, `%COMSPEC%`. Programs are found by walking PATH in Rust (no `where`).

### Manual test: a client of another user is refused
CI has one account, so the cross-user case is by hand (needs an administrator to make the second account):
1. In an elevated shell: `net user fgtest P@ssw0rd-Test1 /add`.
2. As your own user: `forgetty server start`, then `forgetty server status` (running).
3. As the other user: `runas /user:fgtest cmd` and in that shell run `forgetty server status`. Expected: the open of `\\.\pipe\forgetty-<your SID>` fails with "Access is denied" (the DACL), and the CLI says the server is not reachable. The other user also computes a different pipe name (its own SID), so it does not even try yours.
4. To see the server-side SID check as well, start the server with a DACL that is too wide (a test build) or use the unit test `a_client_of_another_user_gets_no_service` in `crates/forgetty-socket/tests/pipe_server.rs`, which makes the server expect the SYSTEM SID.
5. Cleanup: `forgetty server stop`, `net user fgtest /delete`.

### Manual test: a normal process cannot reach an elevated server
1. In an elevated shell: `forgetty --state-dir C:\fgtest server start`.
2. In a normal (not elevated) shell: `forgetty --state-dir C:\fgtest server status`. Expected: "server not reachable" (the normal CLI looks for the name without `-elevated`).
3. To see the label itself, in the normal shell open `\\.\pipe\forgetty-<SID>-<secret>-elevated` (the secret is in `C:\fgtest\data\forgetty\pipe.secret`, readable by the user) with any client: the open fails with "Access is denied" because of the mandatory label. The unit test `a_client_at_a_lower_integrity_level_gets_no_service` covers the server-side level check.
4. Cleanup: `forgetty --state-dir C:\fgtest server stop` from the elevated shell.

### Manual test: a normal process cannot stand in for the elevated server
1. In a normal shell: `forgetty --state-dir C:\fgtest2 server start`, read the secret in `C:\fgtest2\data\forgetty\pipe.secret`.
2. Stop it, then start a small pipe server in the normal shell that creates `\\.\pipe\forgetty-<SID>-<secret>-elevated` (any tool that makes a named pipe, for example a PowerShell `NamedPipeServerStream`).
3. In an elevated shell: `forgetty --state-dir C:\fgtest2 server status`. Expected: the CLI refuses the connection with "the pipe server (pid N) runs at a lower integrity level" before sending anything.
