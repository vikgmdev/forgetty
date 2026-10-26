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
| A1 | Sessions held by a daemon, one process per window, shells outlive the GUI process (`app.rs:236-310`) | MISSING | In-process ConPTY only (`local.rs:1-4`, `main.rs:80-86`). No background host for local shells. | L | ARCH |
| A2 | Closing the window keeps the session (server flush+save; `app.rs:2312-2345`, `server.rs:935-941`) | MISSING | Last local tab close quits (`shell.rs:455-458`); shells die with the process (inference). Remote machines: PRESENT via server. | L | ARCH |
| A3 | Relaunch rebuilds layout from daemon `get_layout` (tabs, splits, ratios, focused pane, workspaces; `app.rs:2210-2240`, `app.rs:3558`) | PARTIAL | Remote machines: PRESENT (`machines.rs:270` `fetch_layout`). Local machine: starts empty (`local.rs:63-80`). | L | ARCH (same decision as A1) |
| A4 | Screen and scrollback replay after restore (byte log replay, `terminal.rs:201-208`) | PARTIAL | Remote: PRESENT (snapshot catch-up, `README.md` W4 "AD-023"). Local: MISSING. | L | ARCH |
| A5 | `on_launch = restore / new` (`forgetty-config/src/schema.rs:81-88`, `src/main.rs:216-262`) | MISSING | No config key, nothing to restore locally. | S | depends on A1 |
| A6 | Pin session toggle + pin icon (`app.rs:1719-1741`) | MISSING | Server made pin a no-op (`04d09d2`). | S | OBSOLETE (drop it) |
| A7 | Restore Previous Session dialog from trash, Undo Close notification, `--restore-session` (`app.rs:7812-7916`, `app.rs:2575-2628`) | MISSING | Trash model retired by AD-017. | S | OBSOLETE, LINUX-ONLY (notification) |
| A8 | Crash-orphan recovery of `active/*.json`, P-018 migration (`app.rs:319-345`) | MISSING | Linux session-file layout, retired by the durable server. | - | OBSOLETE, LINUX-ONLY |
| A9 | Close Window Permanently (kills daemon and deletes session files; `app.rs:1680-1704`) | MISSING | No daemon to kill. Closest Windows analogue is "close tab". | - | OBSOLETE |
| A10 | New Window (spawns another process with a fresh session id; `app.rs:1642-1657`) | MISSING | Single window, `main.rs:104-130`. | M | ARCH (second window must share or split the backend) |
| A11 | Tab tear-off to a new window (drag out of tab bar, `app.rs:1229-1245`, `app.rs:7688-7740`) and "Move to New Window" (`app.rs:7416`) | MISSING | Single window. | M | depends on A10 |
| A12 | Clean shutdown on SIGTERM/SIGHUP/SIGINT (`app.rs:2351-2385`) | MISSING | No equivalent handling (CTRL_CLOSE not handled). | S | LINUX-ONLY |
| A13 | Command line: `--working-directory`, `-e`, `--class`, `--no-restore`, `--session-id`, `--restore-all`, `--config-file` (`src/cli.rs:1-60`) | MISSING | Only `--smoke` (`main.rs:34`). `forgetty.exe` on PATH is reserved for a future CLI (`README.md` Install). Env: `FORGETTY_CONFIG`, `FORGETTY_SHELL`. | M | ARCH (one `forgetty` CLI for Windows is a product call) |
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
| B8 | Tab attention badge when an unfocused pane rings (`set_needs_attention`, `app.rs:3955-3975`) | PARTIAL | Status icon in tab and sidebar for server/OSC 133 status (`chrome.rs:52-62`). No badge for BEL or OSC 9/99/777 on local panes. | S | - |
| B9 | Tab color dot (presets + custom picker; not persisted; `app.rs:6672`, `app.rs:7195-7360`) | MISSING | No tab color. | S | - |
| B10 | Tab right-click menu: Change color, Rename, Duplicate, Split, Move left/right/new window, Search, Export (disabled stub), Close to the right, Close others, Close (`app.rs:6803-7300`) | MISSING | No context menu anywhere in the Windows UI (grep for right click/menu in `chrome.rs`, `shell.rs`, `machines.rs`: none). | M | - |
| B11 | Duplicate tab (`app.rs:7364-7415`) | MISSING | - | S | - |
| B12 | Close tabs to the right / close other tabs (`app.rs:7120-7190`) | MISSING | - | S | - |
| B13 | New-tab dropdown with profiles + split actions (`app.rs:443-590`) | MISSING | Palette has split actions; no profile menu. | M | - |
| B14 | Profiles: named shell+dir+icon, default profile, Ctrl+Shift+1..9 (`schema.rs:9-26`, `app.rs:401-440`, `app.rs:616-720`) | MISSING | `NewTab.command` exists in the backend (`backend.rs:181-188`) but no config key or UI. `shell` is a single string with no arguments (`main.rs:80-90`, `config.rs:21`). | M | - |
| B15 | Last tab closed closes the window (`app.rs:7585-7603`) | PRESENT | `shell.rs:455-458` (local machine only) | - | - |

## 4. Panes and splits

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| C1 | Split right / split down, Alt+Shift+= and Alt+Shift+- (`app.rs:1366-1416`) | PRESENT | Same default chords (`keys.rs:81-82`) | - | - |
| C2 | Split left / split up (before=true; menu only; `app.rs:1385-1386`) | MISSING | `SplitPane` has only direction, no placement (`backend.rs:205-214`) and `shell.rs:758` `split` takes only a direction. Whether the wire `split_pane` supports it: AMBIGUOUS (not checked in `PROTOCOL_V2.md`). | M | maybe protocol |
| C3 | Directional focus, Alt+Arrow, geometry based (`app.rs:1536-1556`, `app.rs:4814`) | PRESENT | Ctrl+Alt+Arrows (`keys.rs:84-87`) | - | chords differ |
| C4 | Keyboard pane resize, Alt+Shift+Arrow (`app.rs:1560-1580`, `app.rs:4951`) | MISSING | Divider drag only (`shell.rs:1094-1150`). | S | - |
| C5 | Divider drag, ratios persisted (`app.rs:2684-2730`, `app.rs:2312`) | PRESENT | Drag + `update_split_ratios` (`shell.rs:1148`, `README.md` W2) | - | - |
| C6 | Close pane, Ctrl+Shift+W, pane closes when the shell exits (`app.rs:1422-1442`, `app.rs:3927`) | PRESENT | Ctrl+Shift+W (`keys.rs:83`), smoke `close_pane_pty_exit` | - | - |
| C7 | Dim unfocused panes (`term.rs:2813`) | PRESENT | `README.md` W2 "dims unfocused panes" | - | - |
| C8 | Notification ring around an unfocused pane (`term.rs:2834`) | MISSING | Status shows in tab/sidebar icon, no pane ring. | S | - |
| C9 | Per-tab focused pane remembered and restored (`app.rs:73-99`) | PRESENT | `set_active_pane` (`shell.rs:775`) | - | - |

## 5. Workspaces

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| D1 | Workspace sidebar toggle, Ctrl+Alt+B (`app.rs:2157-2177`, `app.rs:8452`) | PRESENT | Ctrl+Shift+B, sidebar with MACHINES / WORKSPACES / TABS / ABOUT (`keys.rs:89`, `chrome.rs:248-470`) | - | - |
| D2 | New workspace, Ctrl+Alt+N (`app.rs:1960-1975`) | PRESENT | Ctrl+Shift+N, "+ New workspace" (`keys.rs:91`, `shell.rs:826`) | - | - |
| D3 | Switch workspace: click, Alt+1..9, Ctrl+Alt+PgUp/PgDn (`app.rs:2030-2145`) | PARTIAL | Click works. Next/Previous workspace actions exist with NO default chord (`keys.rs:92-93`). No number keys. | S | - |
| D4 | Rename workspace (dialog; `app.rs:1982`, `app.rs:8175`) | PRESENT | Double click in sidebar, palette (`shell.rs:901`) | - | - |
| D5 | Delete workspace (`app.rs:1998-2025`, `app.rs:9971`) | MISSING | No method in `SessionBackend` (`backend.rs:321-338`). Server socket has `delete_workspace` (`forgetty-socket/src/protocol.rs`). | M | - |
| D6 | Duplicate workspace (`app.rs:9389-9830`) | MISSING | Not in trait. Server socket has `duplicate_workspace`. | M | - |
| D7 | Move workspace up/down (`app.rs:9827-9970`) | MISSING | Not in trait. Server socket has `move_workspace`. | S | - |
| D8 | Workspace color, persisted via daemon (`app.rs:9043-9160`) | MISSING | `Workspace.color` exists in the layout type (`layout.rs:55`) but nothing sets or draws it. Server socket has `set_workspace_color`. | S | - |
| D9 | Workspace right-click menu (color, rename, duplicate, move, delete; `app.rs:8671-8935`) | MISSING | No context menus. | M | depends on D5-D8 |
| D10 | Window title shows workspace name (`app.rs:8347-8400`) | PARTIAL | Title is `<tab> - Forgetty` (`shell.rs:1178`), no workspace. | S | - |

## 6. Input, keyboard, command surface

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| E1 | libghostty key encoder, Kitty protocol, release events (`input.rs:89-134`) | PRESENT | `encoder.rs`, `view.rs:663-760` | - | - |
| E2 | Focus reporting DECSET 1004 (`term.rs:712-757`) | PRESENT | `view.rs:456` | - | - |
| E3 | Mouse reporting, Shift bypasses it (`term.rs:759-975`, `input.rs:160-260`) | PRESENT | `view.rs:959-1060`, `encoder.rs` | - | - |
| E4 | Ctrl+C copies when a selection exists, else SIGINT (`term.rs:600-640`) | PARTIAL | Ctrl+C always goes to the program. Copy is Ctrl+Shift+C, right click, or copy-on-select (`view.rs:690-700`, `view.rs:995-1000`, `view.rs:1053`). Different design by choice. | S | decision: keep Windows Terminal style |
| E5 | Ctrl+V and Ctrl+Shift+V paste (`app.rs:1490-1506`) | PARTIAL | Ctrl+Shift+V and right click only (`view.rs:700-705`). Ctrl+V goes to the program. | S | same decision as E4 |
| E6 | Configurable keybindings in `[keybindings]` (`app.rs:683-720`, `schema.rs` `keybindings`) | PRESENT | `[keybindings]`, multiple chords, `"none"` unbinds (`config.rs:5-24`, `keys.rs:204-260`) | - | - |
| E7 | Keybinding editor UI with conflict detection and reset (`sv.rs:474-510`, `sv.rs:1111-1520`) | MISSING | Config file only. Palette shows current chords (`shell.rs:965-986`). | M | depends on L3 |
| E8 | Keyboard Shortcuts window, F1 (`app.rs:6293-6410`) | PARTIAL | Command palette lists actions with chords (`shell.rs:953-986`). No cheat-sheet window, no F1. | S | - |
| E9 | Chord map divergence (Windows picks different defaults) | PARTIAL | Close tab Ctrl+Shift+Q (GTK: quit); new workspace Ctrl+Shift+N (GTK: new window); focus Ctrl+Alt+Arrow (GTK: Alt+Arrow); palette Ctrl+Shift+P (GTK: shortcuts window). Not a defect, but any "parity" doc must say it. | S | decision |
| E10 | Ctrl+Shift+1..9 open profile N (`app.rs:1308-1360`) | MISSING | No profiles. | S | part of B14 |

## 7. Selection, clipboard, paste

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| F1 | Drag select (deferred origin), double-click word, triple-click line (`term.rs:840-925`, `term.rs:2013-2075`) | PRESENT | `view.rs:967-1010`, `view.rs:1101` | - | - |
| F2 | Auto-scroll while dragging past the top/bottom edge (`term.rs:992-1001`) | PRESENT | Window-level mouse listener scrolls and extends while the pointer is held outside the pane (`view.rs` `autoscroll_tick`); smoke `drag_past_top_scrolls_and_extends`. | S | WP1 done
| F3 | Shift+click extends selection (`term.rs:864-872`) | PRESENT | `on_mouse_down` extends from the anchor; smoke `shift_click_extends_selection`. | S | WP1 done
| F4 | Select All (context menu, `app.rs:6227`) | PRESENT | Ctrl+Shift+A and palette action `SelectAll`; context-menu entry waits for WP2. | S | WP1 done
| F5 | Copy via Ctrl+Shift+C (`app.rs:1448-1463`, `app.rs:5186-5372`) | PRESENT | `view.rs:927`, `view.rs:690` | - | - |
| F6 | Smart copy: strip box-drawing U+2500-257F, trailing spaces, CRLF; soft-wrap join with heuristic (`clipboard.rs:1-27`, `app.rs:5290-5360`) | PRESENT | Box-drawing strip, trailing-space trim, soft-wrap join over absolute rows (`shell-core/src/copytext.rs`); `CopyRaw` action and `smart_copy` key. No shell-wrap heuristic beyond GTK rules. | S | WP1 done
| F7 | Paste warning: size over `paste_warn_size`, newlines, preview dialog, config keys (`app.rs:5950-6125`, `schema.rs` `paste_warn_*`) | PRESENT | Adds `paste_warn_size` (default 5120, 0 disables) with the size in the overlay. Windows stays stricter than GTK (bracketed paste, control strip, always confirm). | S | WP1 done
| F8 | Paste an image: saves PNG to cache dir and pastes the path (`app.rs:6128-6165`) | MISSING | `view.rs:766` reads text only. For a REMOTE pane the Windows path means nothing on the Linux server. | M | ARCH for remote panes (needs upload or skip) |
| F9 | Code block detection (box-drawing borders and uniform-background regions) with hover highlight and copy button (`code_block.rs:1-575`, `term.rs:819-857`, `term.rs:1096-1130`, `term.rs:2845-2970`) | PRESENT | Border-box detection in `shell-core/src/codeblock.rs`, hover wash and copy button (`element.rs` `paint_code_block`). Uniform-background regions are not detected (see deviations). | M | WP1 done
| F10 | Selection color from theme, overlay with alpha (`term.rs:2714-2785`) | PRESENT | `theme.selection` (`config.rs:36`, `element.rs`) | - | - |

## 8. Links, search, scrollback, terminal commands

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| G1 | http/https URL detection on hover: underline and pointer cursor (`term.rs:1729-1805`, `term.rs:1080-1100`, `term.rs:2622`) | PRESENT | `shell-core/src/links.rs` (http/https only, trailing punctuation cut, wrapped URLs whole), underline and hand cursor. | M | WP1 done
| G2 | Ctrl+click opens URL in browser; context menu "Open <url>" (`term.rs:798-818`, `app.rs:6271`, `term.rs:1639-1685`) | PARTIAL | Ctrl+click opens via ShellExecute after the scheme allow-list. Context-menu "Open <url>" waits for the WP2 context menu. | S | WP1 done, rest in WP2
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
| H6 | `notification_mode = all / ringonly / none` (`schema.rs`) | MISSING | No key; toasts follow the fixed policy in `notify.rs`. | S | - |
| H7 | Bell: `bell_mode = visual / audio / both / none`, 150 ms flash overlay, 200 ms rate limit, Ctrl+C suppression (`term.rs:410-460`, `term.rs:2821`) | PRESENT | `bell_mode` key, 150 ms flash, 200 ms rate limit, Ctrl+C suppression, `MessageBeep` for audio. | S | WP1 done
| H8 | OSC 12 cursor color honored (`term.rs:2383`) | PRESENT | Cursor uses the OSC 12 color from libghostty, reset by OSC 112. | S | WP1 done
| H9 | Cursor shape from DECSCUSR, blink, hidden in unfocused pane (`term.rs:2366-2380`, `term.rs:2647-2712`) | PRESENT | Bar/underline/hollow/block from DECSCUSR, blink timer (`element.rs:92-110`, `view.rs:491`). Windows draws a hollow cursor when unfocused, GTK hides it. | - | - |
| H10 | `cursor_style` config key and Settings dropdown (`schema.rs`, `sv.rs:847-878`) | MISSING | NOTE: the GTK key is saved but never read by the renderer. `terminal.rs` starts every pane with `CSI 1 SP q` (`term.rs:234`) and only `cursor_visual_style` is used (`term.rs:2368`; grep finds no other `cursor_style` reader). So GTK itself has a dead setting. | S | decide: skip |
| H11 | Window title from cwd / OSC title (`app.rs:5680`, `app.rs:5876`) | PRESENT | `shell.rs:1178` | - | - |

## 10. Appearance, config, settings UI, remote/pairing, platform

| ID | GTK feature (cite) | Windows | Windows evidence / what is missing | Size | Flags |
|---|---|---|---|---|---|
| K1 | Named themes: 486 bundled (`forgetty-config/src/bundled_themes.rs`, `resources/themes/`) + custom `~/.config/forgetty/themes/*.toml` + `theme = "Name"` (`theme.rs:179-420`, `schema.rs`) | MISSING | Inline `[theme]` colors only; default Campbell (`config.rs:29-40`, `config.rs:105-110`). Can `forgetty-config` be reused on Windows? AMBIGUOUS (depends on `forgetty_core::platform::config_dir`, not checked). | M | ARCH (reuse crate vs port) |
| K2 | Theme picker with search (Appearance sidebar, Ctrl+,; `pref.rs:39-224`) | MISSING | - | M | depends on K1, L3 |
| K3 | Font family/size from config | PRESENT | `[font]`, fallback list (`config.rs:42-45`, `README.md` Config) | - | - |
| K4 | Font family picker (monospace list) and size picker (`pref.rs:224-385`) | MISSING | - | S | depends on L3 |
| K5 | Live font zoom per pane: Ctrl+=, Ctrl+-, Ctrl+0, 6..72 pt (`app.rs:1584-1598`, `app.rs:5092-5150`, `term.rs:1922`) | MISSING | `Metrics` is built once from `Config` (`view.rs:62`). Needs per-pane size, PTY resize and letterbox interplay. | M | - |
| K6 | Opacity / transparency | N/A (GTK lacks it) | Windows has Mica/blur backdrop (`config.rs:27`). See section 11. | - | - |
| L1 | `config.toml` keys: font_family, font_size, theme, shell, scrollback_lines, cursor_style, bell_mode, notification_mode, keybindings, on_launch, profiles, default_profile, paste_warn_size, paste_warn_newline, session_trash_days, byte_log_*, relay_urls, relay, bind_addrs (`schema.rs`) | PARTIAL | Windows keys: shell, scrollback_bytes, backdrop, sidebar, pane_padding_x/y, keybindings, font, theme, no others (`config.rs:22-48`). Missing client-relevant keys: theme name, bell_mode, notification_mode, profiles, default_profile, paste_warn_*, on_launch. Server keys (relay, bind, byte_log, trash) do not apply to a client. | S | - |
| L2 | Config hot reload: 500 ms poll of `ConfigWatcher`, applies font, theme, bell, notification mode, keybindings, profiles to all panes (`app.rs:2440-2560`, `forgetty-watcher/src/watcher.rs:147-200`) | MISSING | Config is read once at start (`main.rs:68`). No watcher (grep reload/watch: nothing). | M | - |
| L3 | Full-window Settings UI: General (shell, on launch), Terminal (scrollback, cursor, bell, notifications), Interaction (paste warnings), Keybindings, Devices, About, Reset all (`sv.rs:509-1600`) | MISSING | No settings UI. Only command palette actions and config file. GPUI has no ready dropdown/toggle widgets here (only `LineEdit`, `lineedit.rs`). | L | ARCH (settings UI vs "edit config.toml + reload") |
| L4 | In-app TOML editor with validation and save (`sv.rs:1603-1700`, `forgetty-config/src/loader.rs:197-260`) | MISSING | - | M | depends on L3 |
| L5 | Open Configuration File / Reload Configuration (menu; `app.rs:1841-1862`, `app.rs:6583`) | MISSING | - | S | depends on L2 |
| L6 | Config error handling | PRESENT (ahead) | Windows shows a message box and runs on defaults (`main.rs:68-72`). GTK only logs (V3_PLAN section 7.3 backlog). | - | - |
| L7 | Unknown top-level key warning, typo guard (`forgetty-config/src/loader.rs` `check_unknown_keys`) | MISSING | `FileConfig` ignores unknown keys silently (`config.rs:36-46`). | S | - |
| M1 | Paired Devices list, Revoke, "Pair new device" QR (server side; `pref.rs:385-600`) | MISSING | Windows is a client only (AD-005); it cannot host a pairing. | - | LINUX-ONLY (no Windows meaning) |
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
- Copy on select and right click copy/paste (`view.rs:995-1053`).
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
4. Theme catalog (K1): reuse `forgetty-config` on Windows or copy the theme TOML files into the Windows crate.
5. Chord policy (E4, E5, E9): keep Windows Terminal conventions or copy GTK chords. Pure product call.
6. Image paste for remote panes (F8): needs a server upload path or is limited to local panes.
7. Split left/up (C2): check whether the wire `split_pane` accepts a placement before sizing.

## 13. Counts

Rows with a GTK feature: 109 (A14, G6, G10, K6, M3 are N/A: GTK lacks them too, listed for completeness).
- PRESENT: 37
- PARTIAL: 13
- MISSING: 54 (9 of them LINUX-ONLY or OBSOLETE: A6, A7, A8, A9, A12, M1, O2, O3, O4; so 45 real gaps)
- N/A: 5
Real gaps (PARTIAL + MISSING, minus Linux-only/obsolete): 58. Sizes of those 58: S 33, M 20, L 5.
Sized L: A1, A2, A3, A4, L3. Flagged ARCH: A1, A2, A3, A4, A10, A13, F8, K1, L3.

## 14. Proposed work packages (build in this order)

1. WP1 Terminal pane parity, no architecture needed (view.rs / element.rs): G3 search, G5 scrollbar, G1+G2 URL hover and click, H7 bell, H8 OSC 12, H3 local OSC 9/99/777, F2+F3+F4 selection extras, F6 smart copy, F7 paste size warning, G9 clear/reset, F9 code blocks. Mostly M and S rows. Windows-only code, easy to smoke-test.
2. WP2 Shell operations and context menus: a small context-menu widget, then B9-B12 tab menu items, D5-D9 workspace ops (new `SessionBackend` methods for delete/duplicate/move/color, wired to LocalBackend and RemoteBackend; the server already has the socket methods), C4 keyboard resize, C2 split left/up (check wire first), C8 pane ring, B8 badge, D3 workspace chords, D10 title.
3. WP3 Config, hot reload, themes, profiles: decide K1 (reuse `forgetty-config` or copy themes), then L1 missing keys, L2 hot reload (watcher plus apply to live views), L5, L7, K5 font zoom, H6 notification mode, B13+B14+E10 profiles with argv, F7 config keys.
4. WP4 Settings UI: needs the L3 decision first. Settings view (General, Terminal, Interaction, Keybindings editor E7, theme/font pickers K2 K4, TOML editor L4, shortcuts window E8). Skip if the decision is "edit config.toml + hot reload".
5. WP5 Local session durability, multi-window, CLI: needs the A1/A2 decision first (section 12, item 1). Then A1-A5 (host process or documented non-goal), A10+A11 multi-window and tear-off, A13 CLI flags, F8 image paste. Biggest and riskiest, last on purpose. If the decision is "local shells stay ephemeral", WP5 shrinks to A10, A11, A13.
