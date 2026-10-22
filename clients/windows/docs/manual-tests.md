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
- [ ] N7 A VPN (WireGuard) up and down while connected: reconnects, no duplicated characters.

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
