# Forgetty feature parity: GTK (reference) vs Windows (target)

Repo: `~/Forge/forgetty-v3`, branch `v3/main` (read only, nothing built or run).
GTK paths are under `crates/forgetty-gtk/src/` unless a crate is named. Windows paths are under `clients/windows/`.
Status words: PRESENT, PARTIAL, MISSING. Size: S (under 1 day), M (1 to 3 days), L (more than 3 days or touches protocol/architecture).
Flags: `LINUX-ONLY` = no Windows meaning, `ARCH` = needs an architecture decision before work starts, `OBSOLETE` = GTK feature the V3 plan already retires.

Short names used in cites: `app` = `app.rs`, `term` = `terminal.rs`, `sv` = `settings_view.rs`, `pref` = `preferences.rs`,
`view` = `clients/windows/src/view.rs`, `shell` = `clients/windows/src/shell.rs`, `chrome` = `clients/windows/src/chrome.rs`,
`keys` = `clients/windows/shell-core/src/keys.rs`, `local` = `clients/windows/shell-core/src/local.rs`.

## 0. Context that decides many rows

- V3 plan (`docs/architecture/V3_PLAN.md:20-22`): GTK is "paused", compiles only. The durable server (`forgetty server run`, systemd user service) is the Linux target. Windows is a client of that server plus a local ConPTY mode.
- GTK code comments (`app.rs:2297-2302`, `app.rs:1922-1925`) still describe the old AD-016 model: window close = `shutdown_clean` = daemon exits, session file moves to `sessions/` (pinned) or `sessions/trash/` (unpinned).
- Since commit `04d09d2` the server treats `shutdown_clean`, `shutdown_save` and `disconnect` as "flush byte logs and save layout, do not exit" (`crates/forgetty-socket/src/server.rs:935-941`, AD-017). Pin/trash are compat no-ops (commit message). So on `v3/main` the comments in GTK and the server disagree. AMBIGUOUS: whether GTK still reaches a durable server, because `ensure_daemon` still spawns `forgetty server run --session-id <uuid> --socket-path forgetty-<uuid>.sock` per window (`app.rs:223-310`) while the durable server holds one lock per user (`docs/server.md:57-60`). I did not run it, so I cannot say which wins.
- GTK is NOT a remote client. It only talks to a local Unix socket (`daemon_client.rs`). Its "Devices" page is the server side of pairing (list paired devices, revoke, show QR). Windows is the opposite: it pairs to servers.

## 1. What runs the shells, and what survives closing the app (answer to the key question)

GTK (`app.rs:236-310`, `app.rs:2210-2240`, `app.rs:2312-2345`):
- Every window talks to a daemon over a Unix socket; PTYs live in the daemon, not in the GTK process (`terminal.rs:209-235`, no local PTY).
- On launch it calls `get_layout` and rebuilds tabs, splits, ratios, workspaces, focused pane from the daemon (`app.rs:2210-2240`); the daemon replays each pane's byte log (`terminal.rs:201-208`).
- On close it pushes split ratios and calls `shutdown_clean` (`app.rs:2312-2345`). With the V3 server that call only saves; layout, byte logs and (while the server runs) the processes survive. Server restart restores layout and byte logs, shells are new (`docs/server.md:78-80`).

Windows (`shell-core/src/local.rs:1-4`, `local.rs:63-80`, `shell-core/src/conpty.rs:1-60`, `src/main.rs:80-86`):
- `LocalBackend` is in-process. One `portable-pty` ConPTY per pane (`LocalConPty::spawn`, `conpty.rs:26`), created by `AppShell` inside the same exe. No helper process, no service, no named pipe.
- Layout and panes are plain in-memory structs (`State { layout, panes }`, `local.rs:48-52`). A new `LocalBackend` starts with one empty workspace "Default" (`local.rs:63-80`). Nothing is written to disk. `grep` for persist/restore/session file in `clients/windows` finds nothing outside the remote machine store.
- Closing the last local tab calls `cx.quit()` (`shell.rs:455-458`). `shutdown_all` (kills shells) is only called from the smoke test (`smoke.rs:1328`). No code detaches shells on exit. Inference (not verified, no run): when the process ends, Windows closes the pseudo console and the shells end with it.
- Window size is always `Bounds::centered(1000x680)` (`main.rs:104-112`). No window state is saved.
- What does survive on Windows: sessions on a PAIRED REMOTE machine. Those live in the Linux server. The client reconnects, resumes by byte offset, and a restarted server restores layout plus byte logs (`README.md` W3/W4, `remote/src/backend.rs`). That is the durable path today.

Verdict: local shells on Windows do not survive closing the app. Everything the GTK window gets from the daemon (restore, pin, undo close) has no local equivalent. This is gap A1/A2/A3 and needs a decision, see section 12.

## 2. Process model, sessions, windows

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| A1 | Sessions held by a daemon, one process per window, shells outlive the GUI process (`app.rs:236-310`) | PRESENT | WP6: tabs live in the local server (`forgetty.exe`, per user), shown as "This PC" (`localserver.rs`). The app starts it detached, pairs over loopback by itself and watches it (restart at 3 s, 8 s, 23 s, then the built-in terminal with a notice). An elevated window, a missing `forgetty.exe` or `local_server = false` uses the built-in ConPTY: its tabs end with the window. | - | WP6 done |
| A2 | Closing the window keeps the session (server flush+save; `app.rs:2312-2345`, `server.rs:935-941`) | PRESENT | Closing the window leaves the tabs in the server; the next start shows them again. The built-in terminal (fallback) still ends its shells. `Quit and end all tabs on this PC` (palette) is the permanent close, see A9. | - | WP6 done |
| A3 | Relaunch rebuilds layout from daemon `get_layout` (tabs, splits, ratios, focused pane, workspaces; `app.rs:2210-2240`, `app.rs:3558`) | PRESENT | The local machine is a server machine: relaunch calls `get_layout` (tabs, splits, ratios, workspaces) like any remote one. After a server restart (crash, watchdog) the server restores what it saved. | - | WP6 done |
| A4 | Screen and scrollback replay after restore (byte log replay, `terminal.rs:201-208`) | PRESENT | Snapshot catch-up from the server on attach, as for remote machines. | - | WP6 done |
| A5 | `on_launch = restore / new` (`forgetty-config/src/schema.rs:81-88`, `src/main.rs:216-262`) | PRESENT | `on_launch = "restore"` (default) shows what the server kept. `"new"` adds a new workspace beside the old ones and shows it (GTK opens a fresh window; this client has one window). Settings, General. Needs the local server. | - | WP6 done, DEVIATION noted |
| A6 | Pin session toggle + pin icon (`app.rs:1719-1741`) | MISSING | Server made pin a no-op (`04d09d2`). | S | OBSOLETE (drop it) |
| A7 | Restore Previous Session dialog from trash, Undo Close notification, `--restore-session` (`app.rs:7812-7916`, `app.rs:2575-2628`) | OBSOLETE | Restore is the server's (A3), with no trash and no dialog (AD-017). Nothing to port. | - | OBSOLETE, LINUX-ONLY (notification) |
| A8 | Crash-orphan recovery of `active/*.json`, P-018 migration (`app.rs:319-345`) | MISSING | Linux session-file layout, retired by the durable server. | - | OBSOLETE, LINUX-ONLY |
| A9 | Close Window Permanently (kills daemon and deletes session files; `app.rs:1680-1704`) | PRESENT | Palette: "Quit and end all tabs on this PC": asks, closes every tab, stops the local server (`forgetty server stop`) and quits. | - | WP6 done |
| A10 | New Window (spawns another process with a fresh session id; `app.rs:1642-1657`) | MISSING | Single window, `main.rs:104-130`. | M | ARCH (second window must share or split the backend) |
| A11 | Tab tear-off to a new window (drag out of tab bar, `app.rs:1229-1245`, `app.rs:7688-7740`) and "Move to New Window" (`app.rs:7416`) | MISSING | Single window. | M | depends on A10 |
| A12 | Clean shutdown on SIGTERM/SIGHUP/SIGINT (`app.rs:2351-2385`) | MISSING | No equivalent handling (CTRL_CLOSE not handled). | S | LINUX-ONLY |
| A13 | Command line: `--working-directory`, `-e`, `--class`, `--no-restore`, `--session-id`, `--restore-all`, `--config-file` (`src/cli.rs:1-60`) | PARTIAL | `forgetty-win.exe [--working-directory DIR] [-e PROGRAM ARGS...] [--config-file FILE] [--help] [--version]` (`shell-core/src/cli.rs`, `main.rs`). `-e` takes the rest of the line; a wrong option or a missing folder or program is an error box (and text on the parent console) with exit code 2. The first tab opens with that program and folder. DEVIATION: GTK skips restore and save for such a launch; the local server keeps its tabs, so the new tab is added to the active workspace beside them. `--config-file` is the window's config (the server keeps its own). `--class`, `--no-restore`, `--session-id`, `--restore-all` are Linux window and session concepts: not accepted. | - | WP6b done |
| A14 | Window size and position restore | N/A (GTK lacks it) | GTK: `window_width/height` fields exist but are always `None` (`forgetty-workspace/src/workspace.rs:23`, `forgetty-session/src/manager.rs:2579`). Windows: fixed centered size (`main.rs:104-112`). Listed in section 11, not counted as a gap. | - | - |

## 3. Tabs

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| B1 | New tab: Ctrl+Shift+T, header button, dropdown (`app.rs:1257-1287`, `app.rs:853-857`) | PRESENT | Ctrl+Shift+T, "+" button (`keys.rs:59`, `chrome.rs:187-205`) | - | - |
| B2 | Tab inherits cwd of the focused pane (`app.rs:7364` duplicate; daemon spawn_cwd) | PRESENT | New tabs and splits start in parent pane's spawn dir (`local.rs:37-40`, `README.md` W2 Security) | - | - |
| B3 | Close tab: menu "Close Tab", Ctrl+Shift+W (pane then tab), tab x (`app.rs:1757-1783`, `app.rs:7573-7603`) | PRESENT | Ctrl+Shift+Q, x button, middle click (`keys.rs:60`, `chrome.rs:64-150`) | - | chords differ |
| B4 | Next/previous tab (libadwaita TabView defaults, nothing set in code: `grep set_shortcuts` finds none) | PRESENT | Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+1..9 (`keys.rs:60-71`). AMBIGUOUS: GTK relies on libadwaita defaults. | - | - |
| B5 | Drag to reorder tabs, sends `move_tab` (`app.rs:1206-1227`) | PRESENT | Tab drag (`shell.rs:1080-1150`), Ctrl+Shift+PgUp/PgDn (`keys.rs:72-73`) | - | - |
| B6 | Rename tab ("Change Tab Title", dialog; `app.rs:1786-1803`, `app.rs:6518`) | PRESENT | Inline editor, Ctrl+Shift+R or double click, stored via `set_label` (`shell.rs:885`, `keys.rs:74`) | - | - |
| B7 | Tab title = cwd with `~` collapse, 100 ms poll (`app.rs:5702-5900`) | PRESENT | Label, then pane label, then program title, plus cwd chip (`chrome.rs:106-118`, `shell.rs:604-640`) | - | - |
| B8 | Tab attention badge when an unfocused pane rings (`set_needs_attention`, `app.rs:3955-3975`) | PRESENT | Amber dot in the tab and the sidebar rows (and the workspace row) for a bell or a program notification in a pane nobody looks at; clears when the pane is looked at (`menus.rs` `raise_attention`, `shell-core/src/ops.rs` `raises_attention`). The server status icon stays as before. Smoke `tab_badge_for_background_bell`. | - | WP2 done |
| B9 | Tab color dot (presets + custom picker; not persisted; `app.rs:6672`, `app.rs:7195-7360`) | PARTIAL | The 8 GTK presets and "none" in the tab menu, a dot in the tab and the sidebar row, the active tab's top line takes the color. Not persisted (as GTK). No custom picker. | - | WP2 done, picker left out |
| B10 | Tab right-click menu: Change color, Rename, Duplicate, Split, Move left/right/new window, Search, Export (disabled stub), Close to the right, Close others, Close (`app.rs:6803-7300`) | PRESENT | Right-click on a tab or a sidebar tab row (`shell-core/src/menu.rs` `tab_menu`, `menus.rs`): color, rename, duplicate, split right/down, move left/right, find, close to the right, close others, close. "Move to New Window" (reason at the right edge: one window) and "Export" (not yet; a stub in GTK too) are listed and greyed, in GTK's places. Keyboard: arrows, Enter, Esc. | - | WP6b done |
| B11 | Duplicate tab (`app.rs:7364-7415`) | PRESENT | Menu and palette "Duplicate tab": a new tab right after the source, started in the directory the source shell was started in (`spawn_cwd`, never an OSC 7 path). Local and remote. | - | WP2 done |
| B12 | Close tabs to the right / close other tabs (`app.rs:7120-7190`) | PRESENT | Menu and palette. Smoke `tab_close_to_the_right`, `tab_close_others`. | - | WP2 done |
| B13 | New-tab dropdown with profiles + split actions (`app.rs:443-590`) | PRESENT | The arrow next to "+" (and a right-click on "+") opens a drop-down: the profiles, New Tab, then Split Right, Down, Left, Up (the last two when the machine supports them). Each profile shows its Ctrl+Shift+N chord. A remote machine lists no profiles: a profile is a program on this PC. Smoke `new_tab_menu_lists_profiles_new_tab_and_splits`, `new_tab_menu_opens_a_profile`. | M | WP3 done |
| B14 | Profiles: named shell+dir+icon, default profile, Ctrl+Shift+1..9 (`schema.rs:9-26`, `app.rs:401-440`, `app.rs:616-720`) | PRESENT | `[[profiles]]` with `name`, `command`, `directory`, `icon`, and `default_profile`, read by `forgetty-config`. `command` takes arguments: words split on spaces, "..." groups a path with spaces, and a command that names an existing file is used whole (GTK splits on every space). `~` in `directory` is the home folder; a folder that does not exist starts in the home folder and says so. A plain new tab and the "+" button use the default profile, else the first profile (as GTK) on this PC; a remote machine opens its own shell. `shell = "pwsh.exe -NoLogo"` also takes arguments. The icon is kept and not shown. Smoke `profile_chord_starts_the_program_with_its_arguments`, `profile_starts_in_its_directory`, `plain_new_tab_uses_the_default_profile`, `shell_with_arguments_starts_a_plain_tab`. | M | WP3 done |
| B15 | Last tab closed closes the window (`app.rs:7585-7603`) | PRESENT | `shell.rs:455-458` (local machine only) | - | - |

## 4. Panes and splits

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| C1 | Split right / split down, Alt+Shift+= and Alt+Shift+- (`app.rs:1366-1416`) | PRESENT | Same default chords (`keys.rs:81-82`) | - | - |
| C2 | Split left / split up (before=true; menu only; `app.rs:1385-1386`) | PRESENT | Menu items "Split Left" and "Split Up" (pane and tab menus) and palette actions `split_left`, `split_up` (no default chords, as GTK). The server `split_pane` takes an optional `before` (capability `split_before`). A machine whose server does not report the capability hides the two menu items, and the palette action says the server is too old: the client never sends `before` to it. Smoke `split_left_puts_the_new_pane_first`, `split_up_puts_the_new_pane_first`, `real_split_left_puts_the_new_pane_first`. | - | WP2b done, needs the server of this branch |
| C3 | Directional focus, Alt+Arrow, geometry based (`app.rs:1536-1556`, `app.rs:4814`) | PRESENT | Ctrl+Alt+Arrows (`keys.rs:84-87`) | - | chords differ |
| C4 | Keyboard pane resize, Alt+Shift+Arrow (`app.rs:1560-1580`, `app.rs:4951`) | PRESENT | Alt+Shift+Arrow moves the nearest divider 20 px, saved through `update_split_ratios` (`layout::resize_by_key`). Free on Windows, so GTK's chord is kept. Limit: a nested split is saved under its first leaf, like divider drag. | - | WP2 done |
| C5 | Divider drag, ratios persisted (`app.rs:2684-2730`, `app.rs:2312`) | PRESENT | Drag + `update_split_ratios` (`shell.rs:1148`, `README.md` W2) | - | - |
| C6 | Close pane, Ctrl+Shift+W, pane closes when the shell exits (`app.rs:1422-1442`, `app.rs:3927`) | PRESENT | Ctrl+Shift+W (`keys.rs:83`), smoke `close_pane_pty_exit` | - | - |
| C7 | Dim unfocused panes (`term.rs:2813`) | PRESENT | `README.md` W2 "dims unfocused panes" | - | - |
| C8 | Notification ring around an unfocused pane (`term.rs:2834`) | PRESENT | 3 px amber ring on a pane whose bell or notification nobody has looked at; clears when it gets the focus in an active window. | - | WP2 done |
| C9 | Per-tab focused pane remembered and restored (`app.rs:73-99`) | PRESENT | `set_active_pane` (`shell.rs:775`) | - | - |

## 5. Workspaces

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| D1 | Workspace sidebar toggle, Ctrl+Alt+B (`app.rs:2157-2177`, `app.rs:8452`) | PRESENT | Ctrl+Shift+B, sidebar with MACHINES / WORKSPACES / TABS / ABOUT (`keys.rs:89`, `chrome.rs:248-470`) | - | - |
| D2 | New workspace, Ctrl+Alt+N (`app.rs:1960-1975`) | PRESENT | Ctrl+Shift+N, "+ New workspace" (`keys.rs:91`, `shell.rs:826`) | - | - |
| D3 | Switch workspace: click, Alt+1..9, Ctrl+Alt+PgUp/PgDn (`app.rs:2030-2145`) | PRESENT | Click works. Next/Previous workspace are `Ctrl+Alt+PageDown` / `Ctrl+Alt+PageUp` in the `windows` keymap (the `gtk` keymap keeps its own; the Settings keybinding editor can rebind). No number keys for workspaces (Ctrl+1..9 are tabs). | - | WP6b done |
| D4 | Rename workspace (dialog; `app.rs:1982`, `app.rs:8175`) | PRESENT | Double click in sidebar, palette (`shell.rs:901`) | - | - |
| D5 | Delete workspace (`app.rs:1998-2025`, `app.rs:9971`) | PRESENT | `SessionBackend::delete_workspace` in LocalBackend and RemoteBackend. It asks first when the workspace has more than one tab or pane, or a program at work, and always on a remote machine (Enter or Esc cancels, y deletes; GTK asks only for more than one tab, the stricter rule stays). The call names the workspace by id (capability `workspace_id`), so a workspace another client moved is never mistaken for another. The last workspace cannot be deleted (menu item greyed). | - | WP2 done |
| D6 | Duplicate workspace (`app.rs:9389-9830`) | PRESENT | `SessionBackend::duplicate_workspace`: one fresh tab per source tab in the first pane's start directory, name "<source> (copy)", opens after the source. Splits and programs are not copied (server rule, local does the same). | - | WP2 done |
| D7 | Move workspace up/down (`app.rs:9827-9970`) | PRESENT | `SessionBackend::move_workspace` (a swap of two positions, like the server). The active workspace stays the same workspace. | - | WP2 done |
| D8 | Workspace color, persisted via daemon (`app.rs:9043-9160`) | PRESENT | `SessionBackend::set_workspace_color`, only `#rrggbb` is written. The sidebar row bar shows it. | - | WP2 done |
| D9 | Workspace right-click menu (color, rename, duplicate, move, delete; `app.rs:8671-8935`) | PRESENT | Right-click on a workspace row. | - | WP2 done |
| D10 | Window title shows workspace name (`app.rs:8347-8400`) | PRESENT | With several workspaces: `<workspace> - <tab> - Forgetty`. With one: `<tab> - Forgetty` (GTK AC-09). The "- Forgetty" ending stays (GTK shows the bare pane title). | - | WP2 done |

## 6. Input, keyboard, command surface

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| E1 | libghostty key encoder, Kitty protocol, release events (`input.rs:89-134`) | PRESENT | `encoder.rs`, `view.rs:663-760` | - | - |
| E2 | Focus reporting DECSET 1004 (`term.rs:712-757`) | PRESENT | `view.rs:456` | - | - |
| E3 | Mouse reporting, Shift bypasses it (`term.rs:759-975`, `input.rs:160-260`) | PRESENT | `view.rs:959-1060`, `encoder.rs` | - | - |
| E4 | Ctrl+C copies when a selection exists, else SIGINT (`term.rs:600-640`) | PRESENT (gtk set) | With `keymap = "gtk"` (the default) Ctrl+C copies while text is selected, else it goes to the program. With `keymap = "windows"` Ctrl+C always goes to the program. Copy is Ctrl+Shift+C, copy-on-select, or right click with `right_click = "copy_paste"` (the default right click opens the menu, as GTK) (`view.rs:690-700`, `view.rs:995-1000`, `view.rs:1053`). Ctrl+C is the action `copy_or_interrupt` (a selection on screen is copied and cleared, else the program gets the key; `none` in `[keybindings]` always passes it on). Different design by choice. | S | decision: keep Windows Terminal style |
| E5 | Ctrl+V and Ctrl+Shift+V paste (`app.rs:1490-1506`) | PRESENT (gtk set) | Ctrl+V and Ctrl+Shift+V paste with `keymap = "gtk"`; with `keymap = "windows"` only Ctrl+Shift+V, the menu's Paste, and right click with `right_click = "copy_paste"` (`view.rs:700-705`) paste and Ctrl+V goes to the program. | S | same decision as E4 |
| E6 | Configurable keybindings in `[keybindings]` (`app.rs:683-720`, `schema.rs` `keybindings`) | PRESENT | `[keybindings]`, multiple chords, `"none"` unbinds (`config.rs:5-24`, `keys.rs:204-260`) | - | - |
| E7 | Keybinding editor UI with conflict detection and reset (`sv.rs:474-510`, `sv.rs:1111-1520`) | PRESENT | Settings > Keybindings: every action by heading, search, Change / Add / Unbind / Default per row, Reset all. A pressed chord needs Ctrl, Alt or Win (F keys alone are fine). A chord another action has is named, and Enter moves it. The page writes only `[keybindings]` entries (no entry when the chord is the default of the set) and the live reload applies them. Linux tests `shortcuts::`; smoke `settings_keybinding_*`, `settings_conflict_*`, `settings_reset_*`. Keys, not a recording button: press Enter on a row, then the chord. | M | WP4 done |
| E8 | Keyboard Shortcuts window, F1 (`app.rs:6293-6410`) | PRESENT | F1 (both sets) opens a scrollable list of every action that has a chord in the set and entries in force, by heading, plus the find-bar keys and Ctrl+Click on a link; "Change shortcuts..." opens the editor. The palette stays on Ctrl+Shift+P (GTK also opens the list there). Smoke `shortcuts_window_*`. | S | WP4 done |
| E9 | Chord map divergence (Windows picks different defaults) | PRESENT | Decision made: `keymap = "gtk"` (the default) takes the GTK chords (Alt+Arrows focus, Ctrl+Alt+N new workspace, Ctrl+Alt+B sidebar, Alt+1..9 workspace, Ctrl+Alt+PageUp/Down, Ctrl+Shift+Q quit, Ctrl+, themes); `keymap = "windows"` keeps the older set (Ctrl+Shift+Q close tab, Ctrl+Shift+N new workspace, Ctrl+Alt+Arrow focus, Ctrl+1..9 tab jump, Ctrl+Shift+Z zoom pane, Ctrl+Shift+R rename tab, Ctrl+Shift+A select all). The palette stays on Ctrl+Shift+P in both (GTK opens its shortcuts list there; F1 and that list come with WP4). Smoke `keymap_*`; Linux tests in `keys.rs`. Not in the GTK set (GTK has no chord): close tab, tab jump, rename tab, zoom pane, select all; GTK's new window has no meaning here. | S | WP-keymap done |
| E10 | Ctrl+Shift+1..9 open profile N (`app.rs:1308-1360`) | PRESENT | Ctrl+Shift+1..9 open profile N in a new tab (actions `profile_1`..`profile_9`, rebindable). Ctrl+N alone still goes to tab N. The palette lists "Open profile: <name>" for each profile. Smoke `profile_chord_starts_the_program_with_its_arguments`. | S | WP3 done |

## 7. Selection, clipboard, paste

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| F1 | Drag select (deferred origin), double-click word, triple-click line (`term.rs:840-925`, `term.rs:2013-2075`) | PRESENT | `view.rs:967-1010`, `view.rs:1101` | - | - |
| F2 | Auto-scroll while dragging past the top/bottom edge (`term.rs:992-1001`) | PRESENT | Window-level mouse listener scrolls and extends while the pointer is held outside the pane (`view.rs` `autoscroll_tick`); smoke `drag_past_top_scrolls_and_extends`. | S | WP1 done
| F3 | Shift+click extends selection (`term.rs:864-872`) | PRESENT | `on_mouse_down` extends from the anchor; smoke `shift_click_extends_selection`. | S | WP1 done
| F4 | Select All (context menu, `app.rs:6227`) | PRESENT | Ctrl+Shift+A and palette action `SelectAll`; the pane context menu has Select All. | S | WP1, WP2 done
| F5 | Copy via Ctrl+Shift+C (`app.rs:1448-1463`, `app.rs:5186-5372`) | PRESENT | `view.rs:927`, `view.rs:690` | - | - |
| F6 | Smart copy: strip box-drawing U+2500-257F, trailing spaces, CRLF; soft-wrap join with heuristic (`clipboard.rs:1-27`, `app.rs:5290-5360`) | PRESENT | Box-drawing strip, trailing-space trim, soft-wrap join over absolute rows (`shell-core/src/copytext.rs`); `CopyRaw` action and `smart_copy` key. No shell-wrap heuristic beyond GTK rules. | S | WP1 done
| F7 | Paste warning: size over `paste_warn_size`, newlines, preview dialog, config keys (`app.rs:5950-6125`, `schema.rs` `paste_warn_*`) | PRESENT | `paste_warn_size` (default 5120, 0 disables) with the size in the overlay, and now `paste_warn_newline` (default true). With it false, a multi-line paste into a program without bracketed paste does not ask; hidden control bytes still ask. Both change on a config reload. Windows stays stricter than GTK (bracketed paste, control strip, always confirm). Smoke `paste_newline_warning_can_be_turned_off`, `paste_warn_size_follows_the_file`, `paste_newline_warning_comes_back_on_reload`. | S | WP1 + WP3 done |
| F8 | Paste an image: saves PNG to cache dir and pastes the path (`app.rs:6128-6165`) | PRESENT | Paste (Ctrl+Shift+V, Ctrl+V in the gtk keymap, the menu) with no text on the clipboard but a picture: the picture is saved as `%LOCALAPPDATA%\forgetty\cache\clipboard\paste-YYYYMMDD-HHmmss-<id>.png` (a Windows bitmap is converted to PNG; other formats keep their extension) and the path plus a space is pasted, as GTK does, quoted for the shell the pane runs when it needs it (cmd `"..."`, PowerShell `'...'`, a WSL shell `'/mnt/c/...'`; read from the prompt line). The folder and file are user-only. Tabs on this PC only: in a pane of another machine a notice says so (the path would mean nothing there). `shell-core/src/imagepaste.rs`, `clipboard.rs`. | - | WP6b done |
| F9 | Code block detection (box-drawing borders and uniform-background regions) with hover highlight and copy button (`code_block.rs:1-575`, `term.rs:819-857`, `term.rs:1096-1130`, `term.rs:2845-2970`) | PRESENT | Border-box detection in `shell-core/src/codeblock.rs`, hover wash and copy button (`element.rs` `paint_code_block`). Uniform-background regions are not detected (see deviations). | M | WP1 done
| F10 | Selection color from theme, overlay with alpha (`term.rs:2714-2785`) | PRESENT | `theme.selection` (`config.rs:36`, `element.rs`) | - | - |

## 8. Links, search, scrollback, terminal commands

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| G1 | http/https URL detection on hover: underline and pointer cursor (`term.rs:1729-1805`, `term.rs:1080-1100`, `term.rs:2622`) | PRESENT | `shell-core/src/links.rs` (http/https only, trailing punctuation cut, wrapped URLs whole), underline and hand cursor. | M | WP1 done
| G2 | Ctrl+click opens URL in browser; context menu "Open <url>" (`term.rs:798-818`, `app.rs:6271`, `term.rs:1639-1685`) | PRESENT | Ctrl+click opens via ShellExecute after the scheme allow-list. The pane menu (right-click) shows "Open <url>" when the pointer was on a link, same allow-list. | - | WP1, WP2 done |
| G3 | In-terminal search bar, Ctrl+Shift+F: whole scrollback, case-insensitive, "N of M", Enter/Shift+Enter, Esc, highlights, scroll to match (`term.rs:1340-1440`, `term.rs:1863-1920`, `term.rs:2077-2340`, `term.rs:2787`, `app.rs:1468-1486`) | PRESENT | Ctrl+Shift+F find bar, whole scrollback, case-insensitive, "N of M", Enter/Shift+Enter/Esc, highlights in GTK colors. Time-sliced (4 ms) off the key path (`shell-core/src/search.rs`). | M | WP1 done
| G4 | Mouse wheel scroll, 3 rows per notch (`input.rs:261-335`) | PRESENT | `view.rs:1061-1100` | - | - |
| G5 | Scrollbar widget with drag (`term.rs:1224-1340`) | PRESENT | 10 px overlay strip with draggable thumb (`view.rs` `scrollbar_overlay`, `shell-core/src/scrollbar.rs`). | S | WP1 done
| G6 | Shift+PgUp/PgDn scroll | N/A (GTK lacks it) | Windows has it (`view.rs:709-730`). See section 11. | - | - |
| G7 | Scroll to bottom on typing; scroll lock while scrolled up (`term.rs:612-690`) | PRESENT | `view.rs:636-650` | - | - |
| G8 | Scrollback size config `scrollback_lines` (`schema.rs`) | PRESENT | `scrollback_bytes` (libghostty bytes, different unit) and legacy `scrollback` in lines (`config.rs:76-90`) | - | - |
| G9 | Clear (Ctrl+L to PTY) and Reset (RIS then Ctrl+L), menu only (`app.rs:1806-1838`, `app.rs:6411`) | PRESENT | Palette actions `ClearTerminal` (Ctrl+L) and `ResetTerminal` (RIS then Ctrl+L). | S | WP1 done
| G10 | Export Text (menu item, greyed out stub; `app.rs:7108`) | N/A (GTK lacks it) | Stub only. Not counted. | - | - |

## 9. OSC, bell, notifications, cursor

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| H1 | OSC 0/2 title (`app.rs:5819-5900`) | PRESENT | `osc.rs:99` | - | - |
| H2 | cwd for new tabs (daemon `spawn_cwd`; OSC 7 display hint) | PRESENT | OSC 7 and OSC 9;9 as display hint, spawn dir tracked separately (`osc.rs:101-110`, `local.rs:37-40`) | - | - |
| H3 | OSC 9 / 99 / 777 desktop notifications, scanned client side (`osc_notification.rs:36-190`, `term.rs:326-336`) | PRESENT | Local OSC 9, 99 and 777 raise a toast through the existing limiter (1 per pane per 3 s). | S | WP1 done
| H4 | Notification rate limit 2 s (`term.rs:354-372`) | PRESENT | 1 per pane per 3 s, 10 per 30 s (`notify.rs`) | - | - |
| H5 | Click the notification to focus the pane (`app.rs:4000-4055`, `app.rs:4083`) | PRESENT | `forgetty:focus?m=&p=` toast arg (`machines.rs:539`) | - | - |
| H6 | `notification_mode = all / ringonly / none` (`schema.rs`) | PRESENT | `notification_mode = "all" | "ringonly" | "none"`, as GTK: `all` rings the pane, badges the tab and raises the desktop toast, `ringonly` drops the toast, `none` drops all three (a program notification and a status toast alike). Changes on reload. Smoke `ringonly_rings_the_pane`, `ringonly_raises_no_toast`, `mode_none_has_no_ring_and_no_toast`, `mode_all_toasts_and_rings`. | S | WP3 done |
| H7 | Bell: `bell_mode = visual / audio / both / none`, 150 ms flash overlay, 200 ms rate limit, Ctrl+C suppression (`term.rs:410-460`, `term.rs:2821`) | PRESENT | `bell_mode` key, 150 ms flash, 200 ms rate limit, Ctrl+C suppression, `MessageBeep` for audio. | S | WP1 done
| H8 | OSC 12 cursor color honored (`term.rs:2383`) | PRESENT | Cursor uses the OSC 12 color from libghostty, reset by OSC 112. | S | WP1 done
| H9 | Cursor shape from DECSCUSR, blink, hidden in unfocused pane (`term.rs:2366-2380`, `term.rs:2647-2712`) | PRESENT | Bar/underline/hollow/block from DECSCUSR, blink timer (`element.rs:92-110`, `view.rs:491`). Windows draws a hollow cursor when unfocused, GTK hides it. | - | - |
| H10 | `cursor_style` config key and Settings dropdown (`schema.rs`, `sv.rs:847-878`) | PRESENT | `cursor_style = "block" \| "bar" \| "underline" \| "block_hollow"` and Settings, Terminal, Cursor. DEVIATION (an improvement): GTK saves the key and never reads it. Here it is the shape of a program that set none; a program that asks for a bar or underline (DECSCUSR) keeps its own, and an unfocused pane draws the outline. Applies at once on save. `shell-core/src/cursor.rs`. | - | WP6b done |
| H11 | Window title from cwd / OSC title (`app.rs:5680`, `app.rs:5876`) | PRESENT | `shell.rs:1178` | - | - |

## 10. Appearance, config, settings UI, remote/pairing, platform

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| K1 | Named themes: 486 bundled (`forgetty-config/src/bundled_themes.rs`, `resources/themes/`) + custom `~/.config/forgetty/themes/*.toml` + `theme = "Name"` (`theme.rs:179-420`, `schema.rs`) | PRESENT | Decision K1: `forgetty-config` is reused (it builds for Windows unchanged). `theme = "Name"` takes a bundled theme (486) or a file in the `themes` folder next to the config file (`%APPDATA%\forgetty\themes\*.toml`, or beside `FORGETTY_CONFIG`; the folder is watched, a new or edited theme file shows up live). The list and the colors are kept in memory by a worker thread, so opening the list or picking a theme reads no file; the `[theme]` colors table still works and overrides nothing when a name is set. An unknown name warns with a suggestion and keeps the default colors. The command palette has "Choose theme...": the full list, fuzzy filter, Enter applies it to this session (not written to the file; a reload that changes the file's colors replaces it). Smoke `reload_applies_theme_by_name`, `theme_list_has_the_catalog`, `theme_list_filters_by_name`, `choosing_a_theme_applies_it_to_the_session`, `file_colors_replace_a_picked_theme`. No theme preview swatches (a Settings window item). | M | WP3 done |
| K2 | Theme picker with search (Appearance sidebar, Ctrl+,; `pref.rs:39-224`) | PRESENT | Settings > Appearance: the full theme list with a filter, a live preview (the colors of the highlighted theme in the highlighted font), Enter or "Apply theme" writes `theme = "Name"`. Refused with the reason when the file sets colors in a `[theme]` table. Ctrl+, still opens the palette theme list (this session only). Smoke `settings_theme_*`. | M | WP4 done |
| K3 | Font family/size from config | PRESENT | `[font]`, fallback list (`config.rs:42-45`, `README.md` Config) | - | - |
| K4 | Font family picker (monospace list) and size picker (`pref.rs:224-385`) | PRESENT | Settings > Appearance: font families with a filter (a guess from the name keeps the short list to monospace fonts, Ctrl+M shows all), a size stepper in 0.5 pt steps (6 to 72) and the same preview. Writes `[font]` keys when the file has that table (it wins over `font_family`/`font_size`), else the top-level keys. Smoke `settings_font_*`. | S | WP4 done |
| K5 | Live font zoom per pane: Ctrl+=, Ctrl+-, Ctrl+0, 6..72 pt (`app.rs:1584-1598`, `app.rs:5092-5150`, `term.rs:1922`) | PRESENT | Ctrl+= / Ctrl+Plus, Ctrl+-, Ctrl+0 on the active pane: 1 pt a step, 6 to 72, reset to the configured size, per pane. A pane opened later starts at the configured size. A reload that changes `font_size` or `font_family` drops every zoom (as GTK). The grid follows through the normal resize path (the PTY is told). Smoke `zoom_in_chord`, `zoom_out_chord`, `zoom_reset_chord_returns_to_the_configured_size`, `zoom_stops_at_72`, `zoom_stops_at_6`, `font_reload_drops_the_zoom`, `bigger_font_gives_fewer_columns`. | M | WP3 done |
| K6 | Opacity / transparency | N/A (GTK lacks it) | Windows has Mica/blur backdrop (`config.rs:27`). See section 11. | - | - |
| L1 | `config.toml` keys: font_family, font_size, theme, shell, scrollback_lines, cursor_style, bell_mode, notification_mode, keybindings, on_launch, profiles, default_profile, paste_warn_size, paste_warn_newline, session_trash_days, byte_log_*, relay_urls, relay, bind_addrs (`schema.rs`) | PRESENT | All client keys are read: font_family, font_size, theme, shell, scrollback_lines, bell_mode, notification_mode, keybindings, profiles, default_profile, paste_warn_size, paste_warn_newline, plus the Windows keys. `cursor_style` and `on_launch` are accepted and ignored (GTK ignores the first too; Windows has no session restore). The server keys (session_trash_days, byte_log_*, relay*, bind_addrs) are accepted and ignored by a client. `scrollback_lines` is turned into the libghostty byte budget (2 KiB a line, as the older `scrollback`). `sidebar` and `scrollback_bytes` apply at the next start and to new panes. | S | WP3 done |
| L2 | Config hot reload: 500 ms poll of `ConfigWatcher`, applies font, theme, bell, notification mode, keybindings, profiles to all panes (`app.rs:2440-2560`, `forgetty-watcher/src/watcher.rs:147-200`) | PRESENT | The config file is watched with the operating system's file notifications (no polling: a thread sleeps until the folder reports a change; 150 ms of quiet, then it reads and parses off the UI thread). A saved file applies live: font, colors and theme, bell, notification mode, keybindings, profiles, paste rules, shell for new tabs, backdrop, padding, copy and mouse options. The notice names what changed. A file that does not parse (or has a wrong value such as `bell_mode = "loud"`) changes nothing and shows "Configuration not applied, the last good settings stay" with the first line of the error. A deleted file also keeps the last good settings and says so (GTK would fall back to defaults). Smoke `reload_applies_*`, `bad_file_keeps_the_last_good_settings`, `syntax_error_keeps_the_last_good_settings`, `deleted_file_keeps_the_last_good_settings`; Linux unit tests in `confwatch` (burst is one reload, atomic rename is seen, other files in the folder are ignored, nothing arrives while idle). | M | WP3 done |
| L3 | Full-window Settings UI: General (shell, on launch), Terminal (scrollback, cursor, bell, notifications), Interaction (paste warnings), Keybindings, Devices, About, Reset all (`sv.rs:509-1600`) | PRESENT | Ctrl+. (GTK set), the palette ("Settings...") or the sidebar link opens a full-window view with pages General, Appearance, Terminal, Keybindings, TOML editor, About. Each row reads `config.toml` through `forgetty-config` and a change edits the file text with `toml_edit`: comments, unknown keys, key order and tables stay, the result is checked with the same parser before an atomic write, and the file is never written when it does not load (the TOML editor is the repair). Writes run on the background executor; the same file watcher applies them live. Not in the page: cursor style and on-launch (as in WP3), Devices (the sidebar has machines), a global "Reset all" (per row Default, Keybindings Reset all). Writes use a per-write temp file with the security of the current file, one at a time. Keybinding edits keep a way to Settings and refuse AltGr chords. Linux tests `settings::`, `settings_file::`, `settings_ui::`; smoke `settings_*`. | L | WP4 done |
| L4 | In-app TOML editor with validation and save (`sv.rs:1603-1700`, `forgetty-config/src/loader.rs:197-260`) | PRESENT | Settings > TOML editor: line numbers, caret, undo, paste, Ctrl+S saves only text that loads (the old file is kept once as `config.toml.bak`), an error moves the caret to the line the parser names, Esc asks before it drops unsaved text. The editor follows the file when it has no edits; with edits it offers Reload or Keep, and a save over a file that changed is refused. No mouse selection and no syntax colors. Smoke `settings_toml_*`. | M | WP4 done |
| L5 | Open Configuration File / Reload Configuration (menu; `app.rs:1841-1862`, `app.rs:6583`) | PRESENT | Palette actions "Open configuration file" (creates a commented file when missing, opens it in the default editor, else Notepad) and "Reload configuration" (reads now, says "nothing changed" when so). Menu-only in GTK, so no chords. `open_config`, `reload_config`. | S | WP3 done |
| L6 | Config error handling | PRESENT (ahead) | Windows shows a message box and runs on defaults (`main.rs:68-72`). GTK only logs (V3_PLAN section 7.3 backlog). | - | - |
| L7 | Unknown top-level key warning, typo guard (`forgetty-config/src/loader.rs` `check_unknown_keys`) | PRESENT | Unknown top-level keys, unknown keys in `[font]` and `[theme]`, and unknown keybinding actions warn with "did you mean" when the key is within two edits of a known one. The warning is in the notice after a reload and in the log at start; the file still loads. (GTK refuses a typo of `relay`/`bind_addrs` because a server would run unprotected; a client has no such key to protect.) Linux unit test `unknown_keys_warn_and_suggest`. | S | WP3 done |
| M1 | Paired Devices list, Revoke, "Pair new device" QR (server side; `pref.rs:385-600`) | PRESENT | Settings, Devices: the devices paired with the local server (`forgetty devices --json`), Revoke (twice to confirm; the app itself is listed as "This PC (local app)" and has no Revoke), "Pair new device" (QR of `forgetty pair --ttl 120`, copy payload). Needs the local server. | - | WP6 done |
| M2 | About window (`app.rs:1622-1640`) | PRESENT | ABOUT section with version and update check (`chrome.rs:435-460`) | - | - |
| M3 | Terminal Inspector (disabled stub; `app.rs:2191`) | N/A (GTK lacks it) | Not counted. | - | - |
| O1 | Linux desktop notification via D-Bus (`notify-rust`; `app.rs:4010-4050`) | PRESENT | WinRT toast, AUMID registered (`winsys.rs`) | - | - |
| O2 | `malloc_trim` after 5 s idle (`term.rs:551-590`) | MISSING | n/a | - | LINUX-ONLY |
| O3 | `WM_CLASS` / `--class`, .desktop file | MISSING | n/a | - | LINUX-ONLY |
| O4 | Open URL with `xdg-open` (`app.rs:6271`) | MISSING | see G2 (ShellExecute) | - | LINUX-ONLY mechanism |

## 11. Windows has it, GTK does not (and "neither has it")

Windows only:
- Command palette with fuzzy search (`shell.rs:953-986`, `shell-core/src/fuzzy.rs`).
- Remote machines: paired server list, pairing by clipboard paste with confirm step, reconnect overlay, network-change and resume hooks, per-machine parked state, letterbox + "size set by" label (AD-020/026) (`machines.rs`, `remote/`, `winsys.rs`).
- Pane zoom (maximize a pane in its tab, Ctrl+Shift+Z; `shell.rs:811`).
- Rename pane and workspace through server `set_label` (`shell.rs:893-905`).
- OSC 52 clipboard write with per-machine policy off/ask/allow (`shell-core/src/osc52.rs`, `clipboard.rs`).
- OSC 133 program status (working/waiting/done/failed) with tab and sidebar icons and status toasts (`osc.rs:111-129`, `chrome.rs:52`).
- Bracketed paste, paste sanitizing against pastejacking, confirm overlay (`view.rs:775-800`). GTK writes pasted text raw (no `2004` handling: grep `bracketed` in `forgetty-gtk` is empty).
- Copy on select. Right click copy/paste is the `right_click = "copy_paste"` option (`view.rs`).
- Shift+PgUp/PgDn scrollback keys (`view.rs:709-730`).
- IME through TSF, dead keys, AltGr handling (`view.rs:728-760`, `view.rs:1309-1410`). GTK has no input-method code (grep `IMContext`: none).
- Mica/Mica Alt/blur backdrop, custom titlebar, snap layouts (`main.rs:94-122`, `chrome.rs:156-246`).
- Pane padding config, Ctrl+1..9 go to tab.
- "Check for updates" (GitHub releases, no auto-download; `update.rs`), per-user MSI installer (`installer/Package.wxs`).
- Config error message box (`main.rs:68-72`), "Input dropped" notice while offline (`view.rs:802-840`).

Neither has: window size/position/maximized restore (A14), file drag and drop into a pane (grep for DropTarget/FileDrop in both: none), OSC 8 hyperlinks (grep: none in both), clickable file paths, middle-click/primary-selection paste, Export Text, opacity setting, CLI on Windows beyond `--smoke`.

## 12. Architecture decisions needed before building

1. Local session durability on Windows (A1-A4). Options, not chosen here:
   - (a) Keep in-process ConPTY. Local shells die with the app. Document it. Durable work is done on a paired Linux server (already works). Cheapest.
   - (b) A per-user background host process (or Windows service) that owns the ConPTYs and exposes the same `SessionBackend` over a named pipe; the GUI becomes a client of it. This is what GTK has with its daemon. Needs layout persistence file, byte log, hide-from-taskbar lifecycle, uninstall story. L.
   - (c) Point users at WSL or a remote `forgetty server` and treat Windows local as ephemeral. Same code as (a) plus a "default machine" setting.
   - `forgetty-pty` is built on `portable-pty` (cross-platform) with a unix-only `libc` part (`crates/forgetty-pty/Cargo.toml:9-13`), so the PTY layer does not rule out a Windows-hosted server. Whether `forgetty-server` itself (Unix sockets, `sockpath.rs`, systemd unit) builds on Windows was NOT checked. Option (b) is either a Windows build of the server or a second host implementation of `SessionBackend` in `shell-core`.
2. Settings: in-app Settings UI (L3, L) versus config file + palette + hot reload (L2, M). Everything under E7, K2, K4, L4 depends on this.
3. Multi-window (A10, A11): one process with several windows sharing a `LocalBackend`, or one process per window as on Linux.
4. Theme catalog (K1): decided, `forgetty-config` is reused on Windows (WP3).
5. Chord policy (E4, E5, E9): keep Windows Terminal conventions or copy GTK chords. Pure product call.
6. Image paste for remote panes (F8): needs a server upload path or is limited to local panes.
7. Split left/up (C2): built in WP2b with an optional `before` param on the server (capability `split_before`).

## 13. Counts

Rows with a GTK feature: 109 (A14, G6, G10, K6, M3 are N/A: GTK lacks them too, listed for completeness).
- PRESENT: 93 (including 1 ahead of GTK and 2 gtk-set only)
- PARTIAL: 2
- MISSING: 8 (A6, A8, A12, O2, O3, O4 are LINUX-ONLY or OBSOLETE; A10 and A11 are the multi-window item)
- OBSOLETE: 1 (A7)
- N/A: 5
(Counts after WP6b. The earlier text said 73 / 9 / 22 / 5 after WP3 and was not kept up.)
Real gaps (PARTIAL + MISSING, minus Linux-only/obsolete): A10 and A11 (multi-window), A12 (console close), B9 (custom color picker), and A13 for the options that have no Windows meaning.
Still open: multi-window (A10, A11).

## 14. Proposed work packages (build in this order)

1. WP1 Terminal pane parity, no architecture needed (view.rs / element.rs): G3 search, G5 scrollbar, G1+G2 URL hover and click, H7 bell, H8 OSC 12, H3 local OSC 9/99/777, F2+F3+F4 selection extras, F6 smart copy, F7 paste size warning, G9 clear/reset, F9 code blocks. Mostly M and S rows. Windows-only code, easy to smoke-test.
2. WP2 Shell operations and context menus: a small context-menu widget, then B9-B12 tab menu items, D5-D9 workspace ops (new `SessionBackend` methods for delete/duplicate/move/color, wired to LocalBackend and RemoteBackend; the server already has the socket methods), C4 keyboard resize, C2 split left/up (check wire first), C8 pane ring, B8 badge, D3 workspace chords, D10 title.
3. WP3 (done) Config, hot reload, themes, profiles: decide K1 (reuse `forgetty-config` or copy themes), then L1 missing keys, L2 hot reload (watcher plus apply to live views), L5, L7, K5 font zoom, H6 notification mode, B13+B14+E10 profiles with argv, F7 config keys.
4. WP4 Settings UI: needs the L3 decision first. Settings view (General, Terminal, Interaction, Keybindings editor E7, theme/font pickers K2 K4, TOML editor L4, shortcuts window E8). Skip if the decision is "edit config.toml + hot reload".
5. WP5 Local session durability, multi-window, CLI: needs the A1/A2 decision first (section 12, item 1). Then A1-A5 (host process or documented non-goal), A10+A11 multi-window and tear-off, A13 CLI flags, F8 image paste. Biggest and riskiest, last on purpose. If the decision is "local shells stay ephemeral", WP5 shrinks to A10, A11, A13.
