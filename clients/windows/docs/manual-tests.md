# Windows client: manual tests for Vick

Two parts. Part A is a one-time run (about 45 minutes) on the Windows PC. Part B is the checklist for
a week of daily use. Part C says what CI already covers, so you do not repeat it.

Mark each item `[x]` pass, `[!]` fail, or `[-]` not tried. For every `[!]` write down: what you did,
what you expected, what happened, and the time (the app writes nothing to disk about this yet, so notes help).

Build to test: the `forgetty-win-msi` artifact of the latest green `windows-client` run on `v3/win-spike`
(file `forgetty-win-<version>-x64.msi`). Write the CI run number here: ______

## Part C: what CI already checks (skip these)

Window, ConPTY shell, rendering through the WARP software renderer, key encoder, wide characters,
scrollback, paste sanitizing, selection, pairing dialog logic, reconnect after a server restart,
letterbox, `size_owner` label, toast requests, the Action Center holding the toast, OSC 52 policy,
find in the scrollback (also over a 40000 row scrollback while frames keep coming), the scrollbar maths, link detection and the
scheme allow-list, copy text (smart copy, soft wraps, scrollback rows), shift-click and drag auto-scroll, bell rate limit and
Ctrl+C rule, OSC 12, OSC 9/99/777 raising a toast from a real ConPTY pane, code block detection and copy button, paste size
warning, clear and reset,
the IME handler (preedit, commit, cancel, candidate window anchor at the cursor cell), a real Microsoft IME
(Japanese) and Microsoft Pinyin driven by injected keys (preedit, conversion, commit, anchor), MSI install and uninstall.

CI cannot check: real GPU rendering, real fonts on your monitors, the IME candidate window on screen, sleep and resume,
real wifi to tether changes, several monitors, Win-key shell behavior, SmartScreen. Those are below.

---

## Part A: one-time run

### A1. Install

- [ ] A1.1 Download the MSI. Double-click it. Windows shows SmartScreen ("Windows protected your PC", publisher
  "Unknown publisher"). Click "More info", then "Run anyway". Expected: no UAC (admin) prompt.
- [ ] A1.2 Start menu has "Forgetty". Start it. A window opens with a shell prompt in under 2 seconds.
- [ ] A1.3 Open a new terminal (Windows Terminal or PowerShell) and run `where forgetty`. Expected: a path in
  `%LOCALAPPDATA%\Programs\Forgetty`.
- [ ] A1.4 Install the same MSI again over the top. Expected: no error, still one Start menu entry.
- [ ] A1.5 Uninstall from Settings, Apps. Expected: the Start entry and the folder are gone and `%APPDATA%\forgetty`
  is still there (paired machines stay). Install again for the next tests.
- [ ] A1.6 Sidebar, "Check for updates". Expected: one request, a line saying you are up to date or a newer version.
  Nothing downloads on its own.

### A2. Pairing (paste only, no camera)

On the Linux server run `forgetty pair`. It prints a QR block and a payload line.

- [ ] A2.1 In the client open the palette (Ctrl+Shift+P), type "pair", Enter. A dialog opens.
- [ ] A2.2 Select the WHOLE output of `forgetty pair` in the SSH terminal (QR block and all) and copy it. Do not paste.
  Expected within about a second: the dialog shows "Found on the clipboard" with the machine name and a Pair button. It
  does NOT move on by itself.
- [ ] A2.3 Press Pair. The confirm step shows the machine name, the WHOLE node id in groups of 8, the relay, and the line
  "Compare the Node ID with `forgetty pair` on the server". Compare the node id with what `forgetty pair` printed, group by
  group. Nothing has contacted the server yet. Click Confirm.
- [ ] A2.4 The machine appears in the sidebar and goes to "connected". Open a pane on it and type.
- [ ] A2.5 Repeat with the dialog open and Ctrl+V instead of the clipboard watch. Same result.
- [ ] A2.6 Copy some unrelated text (a URL) while the dialog is open. Expected: nothing happens, no error flash.
- [ ] A2.6b With the dialog open and a machine on "allow" (A3), have a program in a pane write a pairing offer with OSC 52
  (`printf '\e]52;c;%s\a' "$(forgetty pair | base64 -w0)"` on a server you own, then Ctrl+C the pair command). Expected: the
  dialog shows NOTHING (no "Found on the clipboard"). Pasting it yourself with Ctrl+V still works.
- [ ] A2.7 Cancel the dialog, then copy a pair output. Expected: nothing is picked up when no dialog is open.
- [ ] A2.8 Pair the same machine twice. Expected: it is not listed twice, or you get a clear message.

### A3. Clipboard writes from programs (OSC 52, write only)

On a paired machine, in a pane, run (bash):

```
printf '\e]52;c;%s\a' "$(printf 'from osc52' | base64)"
```

- [ ] A3.1 First time on this machine (policy "ask"): a small prompt appears at the bottom right of the window.
  It does not take the keyboard and does not block typing. Paste somewhere else: the clipboard is unchanged.
  The prompt shows the number of lines and bytes. Its buttons look dim and do nothing for the first half second.
- [ ] A3.1b Multi-line text: `printf '\e]52;c;%s\a' "$(printf 'one\ntwo\tx\n3\n4\n5\n6\n7\n8\n' | base64 -w0)"`. The prompt shows the
  first 3 and last 3 lines, `...N more lines...`, line breaks as the return symbol and the tab as an arrow, and the line
  "Ends with Enter: pasting it runs a command".
- [ ] A3.1c While the prompt is showing, run another copy command with different text (even in another machine's pane).
  Expected: the prompt text does not change, and after you answer, only the first text is copied.
- [ ] A3.2 Click Allow. Paste in Notepad: "from osc52". Run the command again: it copies with no prompt.
- [ ] A3.3 Palette, "Clipboard writes from programs on this machine: off". Run it again with different text.
  Expected: clipboard unchanged, no prompt.
- [ ] A3.4 Palette, "... ask". Run it. The prompt shows again. Click Deny. Run it again: no prompt, no copy (Deny is
  remembered as off).
- [ ] A3.5 Restart the app. The policy per machine is still what you set (stored in `%APPDATA%\forgetty\osc52.json`).
- [ ] A3.6 A read must never answer: `printf '\e]52;c;?\a'; sleep 1`. Expected: nothing typed into the shell, nothing
  pasted. (Some programs print garbage if the terminal replies. You should see none.)
- [ ] A3.7 Inside tmux with `set -g set-clipboard on`, or in `nvim` with the OSC 52 provider, yank text. Expected:
  with policy allow, the text reaches the Windows clipboard.
- [ ] A3.8 Escape characters and bells inside the copied text are removed (copy `printf 'a\e[31mb\a'` through OSC 52
  and paste: "a[31mb"). Line breaks stay.
- [ ] A3.9 Output replayed after a reconnect does not copy again. Allow the machine, run the printf, disconnect and
  reconnect the server (or restart it). Put other text on the clipboard first. Expected: the clipboard keeps your text.

### A4. IME (Japanese and Chinese)

CI drives the real Microsoft IME and Pinyin with injected keys and checks what the pane receives, but a runner shows no
screen to look at. This part is what only your eyes can check: where the candidate window appears, fonts, and feel. Settings, Time and language,
Language and region: add Japanese (Microsoft IME) and Chinese Simplified (Microsoft Pinyin). Switch with Win+Space.

Japanese, Microsoft IME in hiragana mode (the key to switch on/off is the tilde key left of 1, or Alt+tilde):

- [ ] A4.1 Type `nihongo`. Expected: the text "にほんご" shows underlined AT THE CURSOR CELL while composing, not at the
  window corner and not in the title bar.
- [ ] A4.2 Press Space. The candidate window opens right under the composing text, on the cursor cell. Move the
  cursor to the bottom row of the window (run `clear`, press Enter 30 times) and check the candidates still fit on
  the screen (they open above the line when there is no room).
- [ ] A4.3 Press Enter to commit "日本語". The text lands in the shell once, no leftover underlined text.
- [ ] A4.4 Type `nihon`, press Esc. Expected: the preedit goes away and nothing is sent to the shell.
- [ ] A4.5 Compose while a program prints output (`ping -t localhost` in another pane of the same window is fine; in
  the same pane use `while sleep 0.2; do date; done`). Expected: the composing text does not jump or vanish.
- [ ] A4.6 Compose in a split pane that is not the first pane. The candidate window follows THAT pane's cursor.
- [ ] A4.7 Compose at the last column of the line. The text wraps or stays inside the window, no crash.
- [ ] A4.8 Resize the window while composing. Expected: no crash. The preedit may be cancelled.
- [ ] A4.9 Try in `vim` (insert mode) and in `claude` or another full-screen TUI. Commit works, the cursor ends after
  the committed text.
- [ ] A4.10 Wide characters keep the grid: after committing 日本語 type `abc`. Backspace removes one character at a
  time (a wide character takes 2 cells).

Chinese, Microsoft Pinyin:

- [ ] A4.11 Type `zhongwen`, check the candidate window position as in A4.2, pick candidate 1 with the Space bar or
  the number key. The text commits.
- [ ] A4.12 Shift toggles Chinese and English mode. In English mode letters go straight to the shell.
- [ ] A4.13 Full-width punctuation and the Win+. emoji picker: pick an emoji. It commits once.

### A5. Daily flows (quick pass)

- [ ] A5.1 Tabs: Ctrl+Shift+T new tab, Ctrl+Tab and Ctrl+Shift+Tab switch, Ctrl+1..9 jump, Ctrl+Shift+Q close.
- [ ] A5.2 Splits: Alt+Shift+Plus and Alt+Shift+Minus split, Ctrl+Alt+arrows move focus, Ctrl+Shift+Z zoom, drag a divider.
- [ ] A5.3 Workspaces: Ctrl+Shift+N new workspace, rename by double click in the sidebar.
- [ ] A5.4 Copy and paste: select with the mouse (double click word, triple click line), copy, paste with Ctrl+Shift+V
  and Ctrl+V. Multi-line paste in bash shows bracketed paste, not a run of commands.
- [ ] A5.5 Scrollback: `seq 1 100000`, scroll with the wheel and with Shift+PageUp. Selection across the scrollback copies
  the right lines.
- [ ] A5.6 `claude` (or another TUI): colors, mouse, and resizing the window redraw correctly.
- [ ] A5.7 Toasts: in a remote pane run something that waits for input or ends, switch the window to the background
  (click another app). A toast shows, and it is in the Action Center (Win+N). Click it: the window comes
  forward on that machine, tab and pane.
- [ ] A5.8 No toast for the pane you are looking at.

---

## Win-key chords (part of A, repeat once on each monitor)

The Windows key belongs to Windows. The terminal must not eat these and must not type anything for them.

- [ ] W1 Win+Left and Win+Right snap the window. Win+Up maximizes, Win+Down restores. No characters appear in the shell.
- [ ] W2 Win+Shift+Left moves the window to the other monitor. The text re-renders sharp at once (see D2).
- [ ] W3 Win+V (clipboard history): pick an item and it pastes into the pane. (The paste is sanitized like Ctrl+V.)
- [ ] W4 Win+. opens the emoji picker and the emoji is inserted (see A4.13).
- [ ] W5 Win+Space switches the input language. The IME indicator in the tray changes.
- [ ] W6 Win+D, then click the taskbar icon: the window returns with the same content and focus.
- [ ] W7 Win+L lock, unlock. The window still works (see S1).
- [ ] W8 Win+Tab, Alt+Tab: the window appears in the switcher with a live preview, not black.
- [ ] W9 Press the Win key alone: the Start menu opens and the terminal receives nothing.
- [ ] W10 A chord bound to a Win key in `%APPDATA%\forgetty\config.toml` under `[keybindings]` (for example
  `"win+shift+t" = "new_tab"`), if you try one: it works, and Windows keeps the ones it owns (Win+L, Win+D).

---

## Network change: wifi to tether

Needs a paired remote machine with a pane running `ping` or `while sleep 1; do date; done`.

- [ ] N1 On wifi, with the pane live, switch the PC to your phone's USB or wifi tether (turn wifi off). Expected: the sidebar
  shows "reconnecting" at most about a second after the last data stops, then "connected" again within about 1 to 3 seconds of
  the tether being up (CI measured 65 to 160 ms after the server accepts). The pane resumes without a screen redraw glitch.
- [ ] N2 Type while it is reconnecting. Expected: the characters are buffered and appear after reconnecting, in order.
- [ ] N3 Turn wifi back on and the tether off. Same result.
- [ ] N4 Airplane mode on for 30 seconds, then off. The sidebar shows "offline" or "reconnecting", not a crash or a hang,
  and it comes back by itself.
- [ ] N5 Change network while a long command is running on the server (`seq 1 1000000 | tail`). The output after reconnect has no
  gap and no repeated block.
- [ ] N6 Type something, pull the network cable/disable wifi, and restart the server. After reconnect a notice
  "Server restarted, N bytes of input not sent" shows in the pane. The typed bytes are NOT replayed into the new session.
- [ ] N7 A VPN (WireGuard) up and down while connected: reconnects, no duplicated characters. For a WireGuard-only
  server see the next section.

---

## WireGuard-only server (no relay)

Needs a server started with `--bind <its WireGuard IP> --no-relay` (see `docs/server.md`, section "WireGuard-only
server") and a WireGuard tunnel on this PC that reaches it. Do not use the server that is already running on the default
port for this. Start a second one with its own `--state-dir` and port.

- [ ] G1 With the tunnel UP, run `forgetty pair` on the server. The output has a `direct` list with the WireGuard address.
  Pair as in A2. Expected: the confirm step shows "Relay: none (WireGuard only)" and an "Address" row with the
  WireGuard IP and port. After Confirm the machine goes to "connected" and a pane works.
- [ ] G2 Close the client and open it again. Expected: it connects without the QR (the address is stored with the machine).
- [ ] G3 Turn the tunnel OFF in the WireGuard app. Expected: within a few seconds the sidebar shows "reconnecting", and
  after the dial has failed for a while the pane overlay says "Can't reach the server. Is WireGuard on?" with a Retry
  button. The sidebar says "unreachable". Typing is buffered, nothing crashes.
- [ ] G4 With the tunnel still off, press Retry. Expected: nothing visible changes except that the app dials again
  (the message stays). No error dialog.
- [ ] G5 Turn the tunnel ON. Expected: the message goes away on its own within a few seconds, the sidebar goes to
  "connected", and the buffered keystrokes arrive in order. You do not press Retry.
- [ ] G6 Turn the tunnel off and on three times in a row, quickly. Expected: it ends "connected", no duplicated characters.
- [ ] G7 Sleep the PC with the tunnel on, wake it. Expected: "connected" again within a few seconds of the tunnel being up.
- [ ] G8 Switch the PC from wifi to tether with the tunnel on. Expected: as N1 (the tunnel follows the network).
- [ ] G9 An upgrade from an older build: a machine paired on that build keeps working. Nothing is pinned for it:
  a direct address it was reached on stays in the address cache and follows the network. A server that is moved to
  `--no-relay` (WireGuard only) needs a re-pair on this PC: remove the machine and pair again with the new
  `forgetty pair` output.
- [ ] G10 Break `machines.json` on purpose (put `{` in one entry, or cut the file in half) and start the app. Expected: the
  good machines still load (a half file loads none). Pair or remove a machine, then look in
  `%APPDATA%\forgetty\`: a `machines.json.bad-<seconds>` file holds the broken original. Nothing is lost silently.

---

## Sleep and resume

- [ ] S1 Sleep the PC (Start, Power, Sleep) for 2 minutes with a remote pane live. Wake it. Expected: the sidebar returns to
  "connected" within a few seconds without you touching anything. Typing works at once.
- [ ] S2 Same after a long sleep (1 hour or more, overnight). The server may have closed the session: you get a clear state
  (offline or reconnecting then connected), not a frozen window.
- [ ] S3 Sleep with the lid closed and wake with the lid open on another network (home to office). Reconnects.
- [ ] S4 Sleep during a pane that prints continuously. After wake there is no corrupted screen (stray characters, half
  lines). Press Ctrl+L if it is: if that fixes it, note it as a bug.
- [ ] S5 Lock (Win+L) and unlock: nothing to do, the window is as it was.
- [ ] S6 Hibernate and resume. Same as S1.
- [ ] S7 After wake the local shell pane (ConPTY) is still alive and still accepts input.

---

## Multiple monitors with different DPI

Use two monitors with different scaling, for example 100% and 150% or 175% (Settings, System, Display).

- [ ] D1 Open the window on monitor 1. Check the text is sharp and the cursor aligns with the characters (type `iiiiii` and
  `WWWWWW`: the columns line up).
- [ ] D2 Drag the window to monitor 2 (or Win+Shift+Left). Expected: text re-renders sharp at the new scale, the grid is
  recomputed (the number of columns changes with the window size), the cursor and selection still line up. No blurry text left
  behind, no clipped last column.
- [ ] D3 Straddle the window across both monitors. Expected: no crash. Text may be scaled by Windows until you drop it.
- [ ] D4 Drag back. Same as D1. Do this 10 times in a row quickly: no leak, no crash, no stale size.
- [ ] D5 Change the scale of a monitor while the window is open (Settings, scale 100% to 150%). The window re-renders. This may
  need a moment.
- [ ] D6 Maximize on each monitor. The grid fills the work area up to the cell size, with at most a small bar of leftover space.
- [ ] D7 Mouse: click and drag select on each monitor. The selected cell is the one under the pointer (check at the far right
  column, where scale errors show first). Mouse reporting in `htop` or `vim` clicks the right cell.
- [ ] D8 IME candidate window on monitor 2 at 150%: it opens at the cursor cell, not offset (see A4.2).
- [ ] D9 Unplug monitor 2 while the window is on it, and plug it back in. The window moves to monitor 1, usable, then you can
  move it again.
- [ ] D10 RDP session in and out (if you use it): the window renders after reconnecting the session.
- [ ] D11 The letterbox: connect another client (the Android client or a second Windows window) so it owns a smaller PTY. Expected: the
  terminal is centered with a label "Size set by <client name>", and it grows back when you press a key (you take the size).

---

## WP1: terminal pane features (find, scrollbar, links, bell, copy, code blocks)

Run in a local pane first, then once in a pane of a paired Linux server. Config keys are in
`%APPDATA%\forgetty\config.toml`: `bell_mode`, `paste_warn_size`, `smart_copy`.
Prepare some scrollback: `1..3000 | % { "line $_ marker" }` in PowerShell, or `seq 1 3000` on the server.

### WP1.1 Find in the scrollback (Ctrl+Shift+F)

- [ ] WP1.1.1 Press Ctrl+Shift+F. A bar floats at the top right of the pane. The terminal does not resize (the shell
  prompt does not move). Type `line 1500`. Expected: the label reads "Searching..." for a moment, then "1 of N", and the
  matches in view are tinted amber, the current one orange.
- [ ] WP1.1.2 With 40000+ lines of scrollback (`seq 1 200000` on the server, or a long build log), type a word that is
  far up. Keep typing and deleting letters. Expected: typing never stalls, the label counts up while it scans.
- [ ] WP1.1.3 Enter goes to the next match, Shift+Enter to the previous one, both wrap around. When the match is above the
  screen the view scrolls so the match sits about a third of the way down. A match already on screen does not move the view.
- [ ] WP1.1.4 Case: search for `MARKER` when the text says `marker`. Expected: found (there is no case toggle, as in GTK).
- [ ] WP1.1.5 Esc closes the bar and the tint goes away. Ctrl+Shift+F again toggles it. Ctrl+Shift+V pastes the first line
  of the clipboard into the field. Home, End, Left, Right, Backspace, Delete edit the field.
- [ ] WP1.1.6 While the bar is open, run a command that prints. Expected: the bar stays, the count updates about every half
  second, the view does not jump.
- [ ] WP1.1.7 A wide character: `echo "a日本b"` then search `本`. Expected: the tint covers the two cells of the glyph.
- [ ] WP1.1.8 Open the bar in two panes of a split. Each keeps its own query. The keys go to the pane with focus.

### WP1.2 Scrollbar

- [ ] WP1.2.1 With no scrollback (new tab, `clear`) there is no bar. After output scrolls, a thin strip appears at the right edge.
  The columns do not change when it appears (it floats over the text).
- [ ] WP1.2.2 Drag the thumb to the top and the bottom. Expected: the text follows the pointer, also when the pointer leaves the
  strip sideways or leaves the pane. Release outside the window: the drag ends.
- [ ] WP1.2.3 Click on the track above or below the thumb. Expected: the thumb centers there.
- [ ] WP1.2.4 The thumb gets brighter under the pointer and while dragged. A very long scrollback still leaves a thumb you can grab.
- [ ] WP1.2.5 Run `vim` or `less` (alternate screen). Expected: no bar there. Leave it: the bar is back.

### WP1.3 Links

- [ ] WP1.3.1 Print `echo "docs: https://example.com/a_(b). done"`. Hover the URL. Expected: underline under
  `https://example.com/a_(b)` (not the final dot) and the hand cursor. Move away: both go.
- [ ] WP1.3.2 Ctrl+click it. Expected: the default browser opens that URL. A plain click only starts a selection.
- [ ] WP1.3.3 Print a URL longer than the pane width so it wraps. Expected: hover underlines both rows, Ctrl+click opens the whole URL.
- [ ] WP1.3.4 Print `file:///C:/Windows/System32/calc.exe`, `javascript:alert(1)` and `ms-msdt:x`. Expected: no underline, Ctrl+click does nothing.
- [ ] WP1.3.5 In `vim` with `:set mouse=a` (mouse reporting on): no underline, clicks go to vim. Hold Shift: selection works again.
- [ ] WP1.3.6 Scroll while the pointer rests on a link. Expected: no underline stuck on the wrong text.
- [ ] WP1.3.7 Not done on purpose: OSC 8 hyperlinks (GTK ignores them too) and the "Open <url>" context menu entry (no context menu yet).

### WP1.4 Bell

- [ ] WP1.4.1 `echo ^G` style test: PowerShell `[char]7` or `printf '\a'`. Default (`bell_mode = "visual"`): a white flash of the pane for about
  0.15 s, no sound.
- [ ] WP1.4.2 Set `bell_mode = "audio"` and restart: the Windows default beep, no flash. `both`: both. `none`: nothing.
- [ ] WP1.4.3 `1..5 | % { [char]7 }` prints five bells at once. Expected: one flash, not five.
- [ ] WP1.4.4 Press Ctrl+C in a shell that beeps after an interrupt. Expected: no flash for that beep. The next bell flashes.
- [ ] WP1.4.5 Reconnect to a server with old bells in the pane history. Expected: no flash from the replay.

### WP1.5 Cursor color (OSC 12)

- [ ] WP1.5.1 `printf '\e]12;#ff8800\a'` (or PowerShell `"$([char]27)]12;#ff8800$([char]7)"`). Expected: the cursor turns orange.
  `printf '\e]112\a'` brings the theme color back.

### WP1.6 Notifications from programs (OSC 9, 99, 777)

Look at another tab or another window first, so the pane is not the one you are in.

- [ ] WP1.6.1 In a local pane: `"$([char]27)]9;build done$([char]7)"` (PowerShell). Expected: a Windows toast "build done" within a second.
- [ ] WP1.6.2 `"$([char]27)]777;notify;CI;all green$([char]7)"`: a toast titled "CI" with "all green".
- [ ] WP1.6.3 `"$([char]27)]99;;hello kitty$([char]27)\"`: a toast with "hello kitty". `printf '\e]99;i=1:d=0;Title\e\\\e]99;i=1:p=body;Text\e\\'`: one toast, title "Title", body "Text".
- [ ] WP1.6.4 Send ten in a loop. Expected: at most one toast per 3 seconds for the pane.
- [ ] WP1.6.5 Click the toast: the window comes up on that pane. With the pane in focus and the window active: no toast.
- [ ] WP1.6.6 ConEmu progress `"$([char]27)]9;4;1;50$([char]7)" ` does NOT produce a toast.

### WP1.7 Selection

- [ ] WP1.7.1 Drag a selection and keep the pointer below the last row, then above the first row. Expected: the view scrolls
  by itself, faster the farther you are from the edge, and keeps going while you hold still. The selection grows. Release
  anywhere: it stops and the text is on the clipboard.
- [ ] WP1.7.2 Click, then Shift+click further down. Expected: the selection extends to the clicked cell. Double click a word,
  Shift+click elsewhere: the word stays inside the selection.
- [ ] WP1.7.3 Ctrl+Shift+A selects what is on screen (the scrollback is not included, as in GTK). Ctrl+Shift+C copies it.
- [ ] WP1.7.4 Selection plus Ctrl+Shift+C of a bordered box (for example the Claude Code input box, or `tree` output).
  Expected: the pasted text has no box-drawing characters and no trailing spaces. Palette "Copy selection as is" keeps the box
  characters. With `smart_copy = false` the normal copy keeps them.
- [ ] WP1.7.5 Copy a long line that the terminal wrapped. Expected: it pastes as one line. Copy two short lines: two lines.
  (Difference to GTK: GTK also joins lines that are more than 80% full. Here only real terminal wraps are joined.)
- [ ] WP1.7.6 Select a range that starts in the scrollback and ends on screen. Expected: all of it is copied.

### WP1.8 Paste warning

- [ ] WP1.8.1 Copy about 10 KB of text (one long line, so no newline) and paste with Ctrl+Shift+V. Expected: a confirm prompt at the
  top of the pane saying the size ("Paste 1 line(s), 9.8 KB?"). Enter pastes, Esc cancels. Nothing reached the shell before Enter.
- [ ] WP1.8.2 `paste_warn_size = 0` turns the size question off. A 3 line paste at a plain prompt still asks (Windows is stricter
  than GTK here: GTK pastes raw; this client always asks before text with newlines goes to a program that did not ask for bracketed paste).

### WP1.9 Clear and reset (palette: "Clear terminal", "Reset terminal")

- [ ] WP1.9.1 Fill the screen, run "Clear terminal". Expected: the shell clears and redraws its prompt (it is Ctrl+L, so a program
  in the foreground gets Ctrl+L too).
- [ ] WP1.9.2 `printf '\e[31m\e[?1000h'` (red text, mouse mode on), then "Reset terminal". Expected: the colors are default again, mouse
  mode is off, the screen is empty, the prompt is back.

### WP1.10 Code blocks

- [ ] WP1.10.1 Show a box: `printf '\u250c\u2500\u2500\u2500\u2500\u2500\u2500\u2510\n\u2502 hi() \u2502\n\u2514\u2500\u2500\u2500\u2500\u2500\u2500\u2518\n'`
  or open Claude Code. Hover inside. Expected: a faint wash on the border cells and a small copy button in the top right corner of the box.
- [ ] WP1.10.2 Click the button. Expected: the clipboard holds the text inside the box, without the border, and the button shows a check mark for about 1.5 s.
- [ ] WP1.10.3 A block with a colored background (a fenced code block in a TUI with a different background): same hover and button.
- [ ] WP1.10.4 In `vim` (alternate screen) or with mouse reporting on: no block highlight.
- [ ] WP1.10.5 Move the pointer over fast-changing output (`1..100000 | % { $_ }`). Expected: no slowdown, no flicker of the button.

## WP2: menus, workspace and tab operations, pane resize, attention

Run once on the local machine and once on a paired Linux server (the workspace items need the server of this branch).
Config key: `right_click = "copy_paste"` (default) or `"menu"` in `%APPDATA%\forgetty\config.toml`.

### WP2.1 Pane menu

- [ ] WP2.1.1 Shift+right-click inside a pane. Expected: a menu opens at the pointer: Copy (grey with no selection), Paste, Select All,
  Find, Clear, Reset, Split Right, Split Down. Each shows its chord on the right where it has one.
- [ ] WP2.1.2 Select some text, Shift+right-click. Copy is enabled. Click it: the text is on the clipboard. Paste, Select All, Find (opens the find bar),
  Clear, Reset, Split Right and Split Down each do what the palette command does, in the pane you clicked (also when it is not the focused one).
- [ ] WP2.1.3 Print a URL, Shift+right-click on it. Expected: the last item reads "Open https://...". Click it: the browser opens that URL.
  Right-click on plain text: no such item. A `file://` or `javascript:` text never shows it.
- [ ] WP2.1.4 A plain right-click still copies a selection or pastes (Windows Terminal style). With `right_click = "menu"` it opens the menu instead.
- [ ] WP2.1.5 In `vim` with `:set mouse=a`, Shift+right-click still opens the menu. A plain right-click goes to vim.
- [ ] WP2.1.6 Open the menu near the right and bottom edge. Expected: it flips to stay inside the window. Click outside, or press Esc: it closes. The terminal gets the keyboard back.
- [ ] WP2.1.7 Palette: "Pane menu" opens it at the top left of the active pane. Up and Down move (grey rows are skipped), Enter runs the row.

### WP2.2 Tab menu

- [ ] WP2.2.1 Right-click a tab, then a row of the sidebar's TABS list. Expected: a color row (8 colors and a cross), Rename Tab, Duplicate Tab,
  Split Right, Split Down, Move Left, Move Right, Find, Close Tabs to the Right, Close Other Tabs, Close Tab. Move Left is grey on the first tab,
  Move Right and Close Tabs to the Right on the last, Close Other Tabs with one tab.
- [ ] WP2.2.2 Pick a color. A dot shows in the tab and the sidebar row, and the active tab's top line takes that color. The cross clears it. Colors are not kept after a restart (as in GTK).
- [ ] WP2.2.3 Duplicate Tab on a tab whose shell is in `C:\Users` (`cd` there first). Expected: a new tab right after it, in the same directory. In a pane that printed
  `printf '\033]7;file:///etc\007'` the copy still starts in the directory the shell was started in.
- [ ] WP2.2.4 Close Tabs to the Right and Close Other Tabs on a menu of a tab that is not the active one: they act on the tab you clicked.
- [ ] WP2.2.5 Right-click a background tab, choose Split Right: that tab opens and splits.
- [ ] WP2.2.6 Left arrow and Right arrow in the color row move the cursor, Enter picks.

### WP2.3 Workspace menu

- [ ] WP2.3.1 Open the sidebar (Ctrl+Shift+B). Right-click a workspace row. Expected: colors, Rename Workspace, Duplicate Workspace, Move Up, Move Down, Delete Workspace.
  Move Up is grey on the first row, Move Down on the last, Delete when only one workspace is left.
- [ ] WP2.3.2 Pick a color. The small bar on the row takes it. The cross clears it. Restart the server: the color is still there (remote and server-hosted workspaces).
- [ ] WP2.3.3 Duplicate Workspace on a workspace with 3 tabs and a split. Expected: a new workspace named "<name> (copy)" right after it, with 3 fresh tabs (no split, no running program), each in
  the directory of the source tab. It opens.
- [ ] WP2.3.4 Move Down then Move Up. Expected: two rows swap, the highlighted (active) workspace stays the one you were in.
- [ ] WP2.3.5 Delete a workspace with 1 tab: it goes at once. With 2 or more tabs: a question shows, Enter or Esc cancels, "y" or the Delete button deletes. Its shells end.
- [ ] WP2.3.6 Window title: with two or more workspaces it reads `<workspace> - <tab> - Forgetty`. With one it reads `<tab> - Forgetty`.
- [ ] WP2.3.7 Two apps on one server: delete or move a workspace in one. The other follows within a second.

### WP2.4 Keyboard pane resize

- [ ] WP2.4.1 Split a pane. Press Alt+Shift+Left several times. Expected: the divider moves left by 20 px a press. Alt+Shift+Right moves it back. Up and Down do the same for a horizontal divider.
- [ ] WP2.4.2 Keep pressing until the divider stops (a pane keeps at least 120 px wide or 60 px high). Expected: no jump, no crash.
- [ ] WP2.4.3 Restart the app (remote) or reconnect: the ratio is kept. In a 3-pane layout, the middle pane resizes the divider on the side of the arrow.
- [ ] WP2.4.4 Zoomed pane: Alt+Shift+Arrow does nothing.

### WP2.5 Attention (bell and notification)

- [ ] WP2.5.1 Open two tabs. In tab 1 run `Start-Sleep 5; [char]7` (or `sleep 5; printf '\a'`), then switch to tab 2. After 5 s: tab 1 shows an amber dot, the sidebar row too.
  Switch to tab 1: the pane has an amber ring for as long as you do not look at it, and both clear when it has the focus.
- [ ] WP2.5.2 Split a pane, ring the bell in the pane without focus: that pane gets a 3 px amber ring. Click it: the ring goes.
- [ ] WP2.5.3 A bell in the pane you are typing in does not leave a ring. With the window in the background, it does (until you come back).
- [ ] WP2.5.4 `bell_mode = "none"`: no ring from a bell. A program notification (OSC 9) still raises the badge.

### Not done in WP2

- Split left / split up: the wire has no placement. Waits for the coordinator's decision.
- Tab menu "Move to New Window" and Export (no multi-window, GTK's Export is a stub).
- A custom color picker (the 8 presets only).

---

## Part B: a week of daily use

Use the Windows client as the main terminal for real work for 5 working days. Keep Windows Terminal installed as a fallback and write down each
time you switched back and why.

Daily, in the first minute of the day:

- [ ] Open the app. Machines are `connected` without action from you.
- [ ] If an update line shows in the sidebar, note the version.

Every day, tick what you used and note problems in the log table:

| Day | Tabs | Splits | claude / TUI | ssh-less remote panes | Toast | Paste (multi-line) | IME | Win-key | Sleep and resume | Network change |
|-----|------|--------|--------------|-----------------------|-------|--------------------|-----|---------|------------------|----------------|
| Mon |      |        |              |                       |       |                    |     |         |                  |                |
| Tue |      |        |              |                       |       |                    |     |         |                  |                |
| Wed |      |        |              |                       |       |                    |     |         |                  |                |
| Thu |      |        |              |                       |       |                    |     |         |                  |                |
| Fri |      |        |              |                       |       |                    |     |         |                  |                |

Things to watch for all week:

1. Typing latency feels as good as Windows Terminal. Note the first time you notice a lag, and what was running.
2. Memory: Task Manager, Details, `forgetty-win.exe`. Note the value at 09:00 and 17:00. A rise of more than 500 MB over a day with a few
   panes is a leak.
3. CPU when idle should be near 0%. Note if the fan spins with all panes idle.
4. Crashes: any window that disappears. Note what you were doing. (Nothing writes a crash log yet.)
5. Stuck states: a pane that does not accept input, a machine stuck on `reconnecting` for more than a minute, a toast that never comes.
6. Wrong screen contents after reconnect, resize, or sleep (stray characters, missing lines, duplicated blocks). Press Ctrl+L and
   note whether that fixed it.
7. Copy and paste mistakes: a paste that ran a command you did not intend, or a paste with strange characters.
8. OSC 52: any unexpected clipboard change. Any prompt you did not expect.
9. Pairing and data: the machine list and settings survive a restart of the app and a reboot.
10. Anything that made you go back to Windows Terminal.

End of the week, answer in 5 lines:

- Would you keep using it as your main terminal on Windows? (yes, no, with changes)
- The 3 worst problems, in order.
- Anything missing that you needed (feature, key, setting).
- Did IME work well enough for real use?
- Did sleep, resume and network changes work without your help?
