# Forgetty VM Test Checklist (T-026)

> **61 Acceptance Criteria for Fresh Ubuntu 24.04 Desktop**
>
> Date: ____________  Tester: ____________  DEB version: ____________
>
> Display server: [ ] Wayland  [ ] X11
>
> Mark each AC: [x] PASS  [-] FAIL  [~] SKIP

---

## Section A: Package Install (7 ACs)

All automated -- run `./run-vm-tests.sh --section A`

- [ ] **AC-01** -- dpkg install completes without error
  ```bash
  sudo dpkg -i forgetty_*.deb
  ```
  **Expected:** Exit code 0, no error output.

- [ ] **AC-02** -- apt resolves dependencies (no dev packages)
  ```bash
  sudo apt-get -f install -y
  ```
  **Expected:** All dependencies resolved. No `-dev` packages pulled in. Only `libgtk-4-1`, `libadwaita-1-0`, `libc6`.

- [ ] **AC-03** -- `forgetty --version` prints version
  ```bash
  forgetty --version
  ```
  **Expected:** Version string printed, exit code 0.

- [ ] **AC-04** -- `forgetty --help` shows all expected flags
  ```bash
  forgetty --help
  ```
  **Expected:** Output includes: `--working-directory`, `-e`, `--version`, `--help`, `--class`, `--config-file`.

- [ ] **AC-05** -- ldd shows no missing libraries
  ```bash
  ldd $(which forgetty)
  ```
  **Expected:** No "not found" entries. `libghostty-vt.so` resolves.

- [ ] **AC-06** -- man page has required sections
  ```bash
  man forgetty
  ```
  **Expected:** Man page displays with NAME, SYNOPSIS, DESCRIPTION, OPTIONS sections.

- [ ] **AC-07** -- desktop-file-validate passes
  ```bash
  desktop-file-validate /usr/share/applications/dev.forgetty.Forgetty.desktop
  ```
  **Expected:** Zero errors (no output = success).

---

## Section B: App Launcher Integration (3 ACs)

- [ ] **AC-08** -- Forgetty appears in GNOME Activities search
  1. Press **Super** key to open Activities
  2. Type **Forgetty**
  3. **Expected:** Forgetty icon and entry appear in search results.

- [ ] **AC-09** -- Clicking Activities entry launches Forgetty
  1. Click the Forgetty entry from AC-08
  2. **Expected:** A Forgetty window opens with a working shell prompt.

- [ ] **AC-10** -- Forgetty icon visible in taskbar/dock
  1. With Forgetty running, observe the GNOME taskbar/dock
  2. **Expected:** Forgetty icon is visible.

---

## Section C: M1 Features -- Terminal Core (20 ACs)

- [ ] **AC-11** -- Rendering: colors, vim, htop, fastfetch
  1. Observe the shell prompt (should have colors)
  2. `vim /etc/hostname` -- renders cleanly
  3. `htop` -- renders with colors and bars
  4. `fastfetch` (or `neofetch`)
  5. **Expected:** No garbled text, no missing glyphs.

- [ ] **AC-12** -- Keyboard: typing, arrows, Ctrl+C, tab completion
  1. Type text -- characters appear correctly
  2. In vim, use arrow keys to navigate
  3. `sleep 100` then **Ctrl+C** -- command interrupted
  4. Type `ls /us` + **Tab** -- completes to `/usr/`
  5. **Expected:** All keyboard input works correctly.

- [ ] **AC-13** -- Mouse: htop clicks, scroll wheel
  1. `htop` -- click a process (should highlight)
  2. Exit htop, run `seq 200`
  3. Scroll up with mouse wheel -- navigates scrollback
  4. **Expected:** Mouse interaction works.

- [ ] **AC-14** -- Tabs: create, switch, close, title
  1. **Ctrl+Shift+T** -- new tab opens
  2. Click first tab -- switches back
  3. Tab title shows directory basename
  4. Click **x** on a tab -- tab closes
  5. Close last tab -- app exits
  6. **Expected:** Tab management works correctly.

- [ ] **AC-15** -- Splits: create, resize, navigate, close
  1. **Alt+Shift+=** -- splits right
  2. **Alt+Shift+-** -- splits down
  3. Drag divider -- resizes
  4. **Alt+Arrow** -- navigates between panes
  5. **Ctrl+Shift+W** -- closes focused pane
  6. **Expected:** Split management works correctly.

- [ ] **AC-16** -- Selection/Copy: click-drag, double/triple click, paste
  1. `echo 'hello world test'`
  2. Click-drag to select -- visual highlight
  3. **Ctrl+Shift+C** -- copies
  4. Double-click -- selects word
  5. Triple-click -- selects line
  6. **Ctrl+Shift+V** -- pastes clean text
  7. **Expected:** Selection and clipboard work.

- [ ] **AC-17** -- Scrollbar: appears, drags, hides
  1. `seq 500` -- scrollbar appears
  2. Drag scrollbar -- viewport scrolls
  3. `clear` -- scrollbar hides
  4. **Expected:** Scrollbar behavior correct.

- [ ] **AC-18** -- Search: open, highlight, navigate, close
  1. `echo 'findme one findme two findme three'`
  2. **Ctrl+Shift+F** -- search bar opens
  3. Type `findme` -- matches highlight
  4. **Enter** -- next match
  5. **Escape** -- closes search
  6. **Expected:** Search works correctly.

- [ ] **AC-19** -- Context menu: right-click popover
  1. Select some text
  2. Right-click -- popover with Copy, Paste, Select All, Search
  3. Click Copy -- copies selected text
  4. Click Paste -- inserts clipboard
  5. **Expected:** Context menu works.

- [ ] **AC-20** -- Font zoom: Ctrl+=, Ctrl+-, Ctrl+0
  1. **Ctrl+=** -- text bigger
  2. **Ctrl+-** -- text smaller
  3. **Ctrl+0** -- reset
  4. **Expected:** Grid reflows after zoom. No clipping.

- [ ] **AC-21** -- URL detection: underline, Ctrl+Click
  1. `echo 'Visit https://example.com today'`
  2. Hover over URL -- underlines
  3. **Ctrl+Click** URL -- browser opens
  4. **Expected:** URL detection and opening work.

- [ ] **AC-22** -- Cursor blink/style: blink, bar in vim insert
  1. Cursor blinks in shell
  2. Open vim -- block cursor in normal mode
  3. Press **i** -- bar/beam cursor in insert mode
  4. Press **Escape** -- back to block
  5. **Expected:** Cursor style changes correctly.

- [ ] **AC-23** -- Bell: visual flash or audio
  ```bash
  echo -e '\a'
  ```
  **Expected:** Visual flash or audio bell.

- [ ] **AC-24** -- Config: config.toml respected, hot reload
  1. Edit `~/.config/forgetty/config.toml`
  2. Change `font_size` to a different value
  3. Save
  4. **Expected:** Terminal updates font size live.

- [ ] **AC-25** -- Shortcuts display: F1 opens help
  1. Press **F1**
  2. **Expected:** Keyboard shortcuts help window opens with all keybindings.

- [ ] **AC-26** -- Hamburger menu: items with shortcuts
  1. Click hamburger menu (top right)
  2. **Expected:** Shows Copy, Paste, New Window, Preferences, About, etc. with keyboard shortcuts.

- [ ] **AC-27** -- Command palette: Ctrl+Shift+P
  1. **Ctrl+Shift+P** -- palette opens
  2. Type `split` -- filters
  3. **Enter** -- executes
  4. **Escape** -- closes
  5. **Expected:** Command palette works.

- [ ] **AC-28** -- Preferences window: opens, live updates
  1. Hamburger -> Preferences
  2. Adjust font size slider -- terminal updates live
  3. Change theme -- applies without restart
  4. **Expected:** Preferences update live.

- [ ] **AC-29** -- Theme browser: preview, apply, revert
  1. Preferences -> Themes
  2. Arrow through list -- preview in real time
  3. **Enter** -- applies
  4. **Escape** -- reverts
  5. **Expected:** Theme browsing works.

- [ ] **AC-30** -- Paste warning: dialog for multi-line paste
  1. Copy multi-line text to clipboard
  2. **Ctrl+Shift+V** to paste
  3. **Expected:** Warning dialog appears. "Paste anyway" proceeds.

---

## Section D: M2 Features -- Production Readiness (5 ACs)

- [ ] **AC-31** -- Shell exit auto-close
  1. Open new tab (**Ctrl+Shift+T**)
  2. Type `exit`
  3. **Expected:** Tab closes. Last tab close exits app.

- [ ] **AC-32** -- Multi-instance: independent windows
  1. From another terminal: `forgetty &`
  2. **Expected:** Second window opens. Independent tabs. Closing one does not affect the other.

- [ ] **AC-33** -- CLI flags: --working-directory and -e
  ```bash
  forgetty --working-directory /tmp    # should start in /tmp
  forgetty -e htop                     # should open htop directly
  ```
  **Expected:** Each flag works correctly.

- [ ] **AC-34** -- TERM/terminfo environment variables
  Inside a Forgetty terminal:
  ```bash
  echo $TERM           # expect: xterm-256color
  echo $COLORTERM      # expect: truecolor
  echo $TERM_PROGRAM   # expect: forgetty
  ```
  **Expected:** All three values correct.

- [ ] **AC-35** -- Signal handling: TERM, no orphans
  1. Open Forgetty with tabs
  2. `kill -TERM $(pgrep forgetty-daemon)`
  3. **Expected:** Daemon saves session and exits. `ps aux | grep -c defunct` is 0.

---

## Section E: M3 Features -- Session Persistence + Workspaces (5 ACs)

- [ ] **AC-36** -- Session persistence: layout survives close/reopen
  1. Open 3 tabs + a split, cd to different dirs
  2. Close and reopen Forgetty
  3. **Expected:** Same layout, same CWDs.

- [ ] **AC-37** -- Workspace manager: create, switch, independent tabs
  1. Create workspace "Test", switch to it
  2. Create tabs, switch back
  3. **Expected:** Both workspaces have independent tab sets.

- [ ] **AC-38** -- Workspace persistence: workspaces survive close/reopen
  1. Create 2 workspaces with different tabs
  2. Close and reopen
  3. **Expected:** Both workspaces and tabs restored.

- [ ] **AC-39** -- Daemon survives GTK close
  1. Open Forgetty, create tabs
  2. Close GTK window
  3. `pgrep forgetty-daemon` -- still running
  4. Reopen Forgetty -- session alive
  5. **Expected:** Daemon persists. Session reconnects.

- [ ] **AC-40** -- Notification rings: bell in background tab
  1. Tab 1: `sleep 3 && echo -e '\a'`
  2. Switch to tab 2 immediately
  3. Wait 3 seconds
  4. **Expected:** Tab 1 shows notification indicator.

---

## Section F: Shell Compatibility (5 ACs)

- [ ] **AC-41** -- bash: colors, tab completion, Ctrl+R
  1. Default shell (bash) -- prompt has colors
  2. `ls /us` + Tab -- completes
  3. **Ctrl+R** + type -- reverse search works
  4. **Expected:** bash works fully.

- [ ] **AC-42** -- zsh: prompt, completion
  ```bash
  forgetty -e zsh
  ```
  1. Prompt renders, tab completion works
  2. **Expected:** zsh works correctly.

- [ ] **AC-43** -- fish: autosuggestions, completion, syntax highlight
  ```bash
  forgetty -e fish
  ```
  1. Start typing -- grey autosuggestions
  2. Tab completion works
  3. Invalid command shows red
  4. **Expected:** fish works fully.

- [ ] **AC-44** -- Shell in config: config.toml shell setting
  1. Set `shell = "/usr/bin/fish"` in config.toml
  2. New tab should launch fish
  3. **Expected:** Config shell setting respected.

- [ ] **AC-45** -- Shell profiles: multiple profiles in config
  1. Create bash and fish profiles in config.toml
  2. Both appear in new-tab dropdown
  3. Selecting each opens correct shell
  4. **Expected:** Shell profiles work.

---

## Section G: SSH and tmux (5 ACs)

- [ ] **AC-46** -- SSH to localhost: colors, arrow keys
  ```bash
  ssh localhost
  ```
  1. Colors display correctly
  2. `vim /etc/hostname` -- arrow keys work
  3. **Expected:** SSH session works in Forgetty.

- [ ] **AC-47** -- SSH TERM propagation
  ```bash
  ssh localhost
  echo $TERM    # expect: xterm-256color
  # 256-color test:
  for i in $(seq 0 255); do printf "\e[48;5;${i}m  "; done; echo
  ```
  **Expected:** TERM is xterm-256color. 256-color palette renders.

- [ ] **AC-48** -- tmux inside Forgetty
  ```bash
  tmux
  ```
  1. **Ctrl+B %** -- vertical split
  2. **Ctrl+B "** -- horizontal split
  3. **Ctrl+B c** -- new window
  4. **Ctrl+B arrow** -- navigate
  5. **Expected:** No rendering glitches. Ctrl+B works.

- [ ] **AC-49** -- tmux mouse mode
  ```bash
  tmux set -g mouse on
  ```
  1. Click on tmux panes -- selection works
  2. `seq 200` then scroll -- works in tmux
  3. **Expected:** Mouse mode works.

- [ ] **AC-50** -- screen inside Forgetty
  ```bash
  screen
  ```
  1. **Ctrl+A c** -- new window
  2. **Ctrl+A n** -- next window
  3. **Ctrl+A d** -- detach
  4. `screen -r` -- reattach
  5. **Expected:** screen works fully.

---

## Section H: Display Scaling (4 ACs)

- [ ] **AC-51** -- 100% scale: crisp text, no grid gaps
  ```bash
  gsettings set org.gnome.desktop.interface text-scaling-factor 1.0
  ```
  **Expected:** Text crisp, cell grid clean.

- [ ] **AC-52** -- 150% scale: crisp rendering
  ```bash
  gsettings set org.gnome.desktop.interface text-scaling-factor 1.5
  ```
  **Expected:** No blurry text, no layout overflow.

- [ ] **AC-53** -- 200% scale: crisp rendering
  ```bash
  gsettings set org.gnome.desktop.interface text-scaling-factor 2.0
  ```
  **Expected:** No blurry text, no layout overflow.

- [ ] **AC-54** -- Dynamic scale change: redraws without restart
  1. With Forgetty open, run the 150% command above
  2. **Expected:** Terminal redraws correctly, no restart needed.
  3. Reset:
  ```bash
  gsettings reset org.gnome.desktop.interface text-scaling-factor
  ```

---

## Section I: Display Server (4 ACs)

- [ ] **AC-55** -- Wayland session: all features work
  1. Log in to GNOME Wayland (default)
  2. `echo $XDG_SESSION_TYPE` -- shows `wayland`
  3. Launch Forgetty, test tabs/splits/typing
  4. **Expected:** All features work.

- [ ] **AC-56** -- X11 session: all features work
  1. Log out, click gear on login screen, select "GNOME on Xorg"
  2. `echo $XDG_SESSION_TYPE` -- shows `x11`
  3. Launch Forgetty, test tabs/splits/typing
  4. **Expected:** All features work.

- [ ] **AC-57** -- Wayland clipboard: copy/paste across apps
  1. In Wayland session, copy text from Forgetty (**Ctrl+Shift+C**)
  2. Paste in another app (**Ctrl+V**)
  3. Copy from another app, paste in Forgetty (**Ctrl+Shift+V**)
  4. **Expected:** Clipboard works bidirectionally.

- [ ] **AC-58** -- X11 clipboard: copy/paste across apps
  1. In X11 session, same as AC-57
  2. **Expected:** Clipboard works bidirectionally.

---

## Section J: Uninstall (3 ACs)

- [ ] **AC-59** -- apt remove forgetty removes cleanly
  ```bash
  sudo apt remove -y forgetty
  ```
  **Expected:** No errors.

- [ ] **AC-60** -- forgetty command no longer found
  ```bash
  which forgetty
  ```
  **Expected:** "not found" or no output.

- [ ] **AC-61** -- Desktop entry removed from Activities
  1. Press **Super**, type "Forgetty"
  2. **Expected:** Forgetty no longer appears.

---

## Results Summary

| Section | Total | Pass | Fail | Skip |
|---------|-------|------|------|------|
| A - Package Install | 7 | | | |
| B - App Launcher | 3 | | | |
| C - Terminal Core | 20 | | | |
| D - Production Readiness | 5 | | | |
| E - Session Persistence | 5 | | | |
| F - Shell Compatibility | 5 | | | |
| G - SSH and tmux | 5 | | | |
| H - Display Scaling | 4 | | | |
| I - Display Server | 4 | | | |
| J - Uninstall | 3 | | | |
| **Total** | **61** | | | |

### Failures (detail)

| AC | Description | Error / Screenshot |
|----|-------------|--------------------|
| | | |

### Notes

_Record any observations, environment details, or bugs found here._
