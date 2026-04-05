# Forgetty — Bugs & Engineering Learnings

Cross-platform knowledge base. Every entry here is a real bug we debugged. If you're implementing Forgetty on a new platform (Windows, Android, Web) or touching PTY/input/signal code, read this first.

---

## BUG-001: Ctrl+C does not interrupt foreground processes

**Platforms affected:** Linux (GTK4), likely all platforms  
**Severity:** Critical — basic terminal interaction broken  
**Status:** Fixed (commit `c39e6eb`)

### Symptoms

- Pressing Ctrl+C while `sleep 1000` is running shows `^C` echoed but the process keeps running
- Only Ctrl+X (or other keys) would actually interrupt the process
- Affects any process that puts the PTY in raw mode: Node.js, pm2, any readline-based app

### Root cause

**Two separate failures, both needed fixing:**

**Failure 1: Writing `0x03` alone is not enough**

Writing the INTR byte (`0x03`) to the PTY master only works when the line discipline has `ISIG` enabled (cooked mode). When a child process (Node.js, pm2, etc.) puts the slave PTY into raw mode (`ISIG` disabled), the `0x03` byte just gets passed through as data and echoed as `^C`. SIGINT is never sent.

Fix: after writing `0x03`, also call `kill(-pgid, SIGINT)` directly.

**Failure 2: Getting the foreground pgrp via `/proc/{pid}/fd/0` silently fails**

The first implementation tried to get the foreground process group by opening the slave PTY via the shell's proc fd symlink and calling `tcgetpgrp` on it:

```rust
// BROKEN
let path = format!("/proc/{shell_pid}/fd/0");
let f = std::fs::File::open(&path)?;  // missing O_NOCTTY → can steal controlling terminal
let pgid = libc::tcgetpgrp(f.as_raw_fd());  // fails silently in practice
```

Problems:
- `std::fs::File::open()` on a TTY device without `O_NOCTTY` can steal forgetty's controlling terminal
- Even with `O_NOCTTY` added, `tcgetpgrp` on a freshly-opened slave fd returned unexpected results on Ubuntu in practice

### Fix

Use `portable-pty`'s `MasterPty::process_group_leader()` which calls `tcgetpgrp` on the **master fd we already hold**. The master fd is always correct, always open, and doesn't require any extra files or O_NOCTTY handling.

```rust
// In PtyProcess (forgetty-pty):
pub fn foreground_pgrp(&self) -> Option<i32> {
    self.master.process_group_leader()
}

// In key handler (terminal.rs):
s.pty.write(&[0x03]).ok();
if let Some(pgid) = s.pty.foreground_pgrp() {
    let my_pid = std::process::id() as libc::pid_t;
    if pgid > 0 && pgid != my_pid {
        libc::kill(-(pgid as libc::c_int), libc::SIGINT);
    }
}
```

### Cross-platform notes

- **Windows (ConPTY):** ConPTY handles Ctrl+C natively via `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)`. The same two-path pattern applies: send the byte AND the signal explicitly.
- **Android:** Same issue — PTY raw mode will suppress line discipline SIGINT. Must use the explicit kill path.
- **Web (WebPTY):** In a browser you can't send signals directly; the PTY must be managed server-side. The server process should expose a signal endpoint that does the same `kill(-pgid, SIGINT)`.

---

## BUG-002: GTK accelerator steals Ctrl+C from terminal

**Platforms affected:** Linux (GTK4 only)  
**Severity:** High — copy shortcut breaks raw-mode apps  
**Status:** Fixed

### Symptoms

Registering `<Control>c` as a GTK accel for `win.copy` causes all Ctrl+C presses to be consumed at the window level, before `EventControllerKey` fires on the terminal widget. Raw-mode apps (vim, etc.) never see Ctrl+C.

### Root cause

GTK processes accelerators at the window level before propagating events to focused widgets. A registered `<Control>c` accel intercepts every Ctrl+C regardless of context.

### Fix

Remove `<Control>c` from the accel registration entirely. In the key handler, check for selection and dispatch copy manually:

```rust
// In key handler — Ctrl+C with selection = copy; without = INTR
if ctrl_only && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
    if s.selection.is_some() {
        drop(s);
        da_for_key.activate_action("win.copy", None).ok();
    } else {
        s.pty.write(&[0x03]).ok();
        send_sigint_to_fg_pgrp(&s.pty);
    }
    return glib::Propagation::Stop;
}
```

### Cross-platform notes

- **Windows (WinUI):** Same pattern applies — keyboard accelerators registered in the UI framework will swallow keystrokes before they reach the terminal widget. Handle Ctrl+C in the key handler, not as a global shortcut.
- **Android:** Not applicable (touch-first, no hardware keyboard accelerators for copy).

---

## BUG-003: BEL flash after Ctrl+C (spurious bell)

**Platforms affected:** Linux, likely all platforms with zsh  
**Severity:** Medium — visual noise, misleading  
**Status:** Fixed

### Symptoms

After pressing Ctrl+C, the terminal briefly flashes (visual bell). Happens even when no bell should ring.

### Root cause

zsh sends `\x07` (BEL) as part of readline cleanup when it receives SIGINT. The BEL arrives on the PTY output stream ~10–50ms after the signal, triggering the terminal's visual bell handler.

### Fix

Set a suppression window in `TerminalState` after sending the signal:

```rust
s.suppress_bell_until = Some(Instant::now() + Duration::from_millis(300));
```

In the PTY output reader, skip BEL if inside the suppression window:

```rust
if s.suppress_bell_until.map_or(false, |t| now < t) {
    s.suppress_bell_until = None;
    continue; // skip this BEL
}
```

300ms is enough to absorb zsh's response without suppressing legitimate bells.

### Cross-platform notes

Applies to any platform where zsh (or bash with readline) is the default shell. The suppression logic lives in `forgetty-core`/the shared PTY reader, so it carries over automatically.

---

## BUG-004: Ghostty encoder returns None for Ctrl+C → visual flash

**Platforms affected:** Linux (GTK4), likely all platforms using the ghostty encoder  
**Severity:** Medium — causes incorrect fallback behavior  
**Status:** Fixed

### Symptoms

When the ghostty key encoder returns `None` for a keypress (no bytes to write), the key handler returns `Propagation::Proceed`, which lets GTK process the event. GTK's default handler for unhandled keys triggers a terminal bell → visual flash.

This specifically happened for Ctrl+C in certain keyboard protocol modes where the ghostty encoder decides not to encode the key.

### Fix

Handle Ctrl+C before reaching the encoder. Write `0x03` directly and return `Propagation::Stop` unconditionally:

```rust
// Bypass encoder entirely for Ctrl+C — always write 0x03
if ctrl_only && (keyval == Key::c || keyval == Key::C) {
    // ... handle copy or INTR ...
    return Propagation::Stop;  // never let GTK see it
}
```

### Cross-platform notes

Any platform using the ghostty encoder APIs should treat Ctrl+C as a special case bypassing the encoder. The encoder's behavior for INTR characters is protocol-dependent and not reliable for this use case.

---

## PATTERN: Two-phase Ctrl+C for reliable signal delivery

The definitive pattern for Ctrl+C across all forgetty platforms:

```
1. Write INTR byte (0x03) to PTY master
   → Works for cooked-mode processes (shell prompt, cat, etc.)
   
2. kill(-foreground_pgrp, SIGINT) via the master fd
   → Works for raw-mode processes (Node.js, pm2, vim in certain modes)
   → Use master fd's tcgetpgrp, NOT /proc/{pid}/fd/0
   
3. Suppress BEL for 300ms
   → Prevents zsh readline cleanup bell from triggering visual flash
```

All three steps must happen together. Skipping any one breaks a class of processes.

---

## BUG-002: Split panes restored as flat tabs after daemon restart

**Platforms affected:** Linux (GTK4)  
**Severity:** High — splits are destroyed on every daemon restart  
**Status:** Fixed (cold-start restore in `src/daemon.rs`)

### Symptoms

- Close Forgetty window (daemon restarts on next open)
- Any tab that had panes split horizontally or vertically shows each pane as a separate top-level tab instead

### Root cause

Cold-start restore in `daemon.rs` called `collect_leaf_cwds()` which walked the saved `PaneTreeState` and returned only the leaf CWDs, discarding all `Split` nodes. It then called `create_tab()` once per leaf, producing N flat tabs.

The `PaneTreeState` serialized to JSON correctly (direction, ratio, first/second preserved), but the restore logic never consumed the split structure.

### Fix

Replaced `collect_leaf_cwds` with two helpers:

1. `first_leaf_cwd(tree)` — returns the CWD of the leftmost leaf to seed `create_tab()` for the tab root.
2. `restore_subtree(sm, anchor_id, tree, size)` — recursively walks the saved tree; for each `Split` node calls `split_pane_with_ratio(anchor_id, direction, ratio, ...)` to create the second child pane, then recurses into both halves.

Also added `SessionManager::split_pane_with_ratio()` (and the underlying `replace_leaf_with_ratio()`) so that saved split ratios are preserved instead of always defaulting to 0.5.

### Key insight

After `create_tab()` creates `root_pane_id`, it is the anchor for the first leaf. Calling `split_pane_with_ratio(root_pane_id, direction, ratio, ...)` inserts `Split { first: Leaf(root_pane_id), second: Leaf(new_id) }` in the tree. Recursing into the first subtree can then further split `root_pane_id` inward — the split_pane lookup finds the leaf by ID regardless of tree depth, so nested splits compose correctly.
