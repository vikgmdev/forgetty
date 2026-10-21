---
name: forgetty
description: Run commands in persistent Forgetty terminal panes and read what they print, with the `forgetty` CLI. Use it for long-running processes, dev servers, a second shell, or when you need to see a terminal's screen.
---

# Forgetty terminals

The `forgetty` CLI controls panes on a running Forgetty server. Panes keep running
after this session ends. Every `-p PANE` accepts, in this order: a full pane id, a
label (case-insensitive), a position (1 is the first pane of `forgetty ls`), or a
unique id suffix of 4+ characters.

| Task | Command |
|---|---|
| List panes | `forgetty ls` (add `--json` for scripts) |
| Open a shell pane | `forgetty new [--cwd DIR] [-- CMD...]` (prints the pane id) |
| Run a command | `forgetty run [--keep] [--wait] [--cwd DIR] -- CMD...` (prints the pane id) |
| Type text | `forgetty send -p PANE "text" --enter` |
| Press a key | `forgetty send-key -p PANE KEY` (enter, tab, esc, ctrl+c, up, down, left, right) |
| Read the screen | `forgetty capture -p PANE --trim [--lines N]` |
| Wait | `forgetty wait -p PANE [--status idle\|working\|waiting\|done\|failed] [--timeout SECONDS]` |
| Watch events | `forgetty events [--json] [-p PANE]` |
| Name a target | `forgetty label -t pane\|tab\|workspace TARGET LABEL` |
| Close a pane | `forgetty close -p PANE` |
| Any server method | `forgetty api list`, `forgetty api describe METHOD`, `forgetty api call [-p PANE] METHOD '{json}'` |

## How to use it

- Run a command and keep its result: `P=$(forgetty run --keep --wait -- make test)`.
  With `--wait` the exit code is the command's. `--keep` leaves the pane listed
  after the command ends, so `forgetty capture -p $P --trim` still works.
  Close it with `forgetty close -p $P`.
- Drive a shell: `P=$(forgetty new)`, then `forgetty send -p $P "cmd" --enter`.
  A plain shell does not report a status, so read the result with
  `forgetty capture -p $P --trim` after a short `sleep`, and repeat until the
  text you expect appears.
- `forgetty wait -p PANE` without `--status` returns when the pane's process exits.
  `--status` only works for programs that report a status (OSC 7501).
- Give long-lived panes a label (`forgetty label -t pane build`) and reuse it as
  `-p build`.
- Close only panes you opened.

## Exit codes

0 ok, 1 failed, 2 bad arguments, 3 target not found or ambiguous (the message lists
the matches), 4 server not running (run `forgetty server start`), 5 `wait` timed out.
