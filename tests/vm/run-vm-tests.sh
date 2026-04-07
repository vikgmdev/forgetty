#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# Forgetty VM Test Suite  (T-026)
#
# Tests all 61 ACs from the T-026 spec on a fresh Ubuntu 24.04 VM.
#
# Usage:
#   ./run-vm-tests.sh                    # run all 61 tests interactively
#   ./run-vm-tests.sh --auto-only        # run only automated tests (8)
#   ./run-vm-tests.sh --section A        # run section A only
#   ./run-vm-tests.sh --section A-C      # run sections A through C
#   ./run-vm-tests.sh --help             # show usage
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────
RED='\033[1;31m'
GREEN='\033[1;32m'
YELLOW='\033[1;33m'
CYAN='\033[1;36m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

# ── Counters ──────────────────────────────────────────────────────────
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
TOTAL_COUNT=0

# ── Mode flags ────────────────────────────────────────────────────────
AUTO_ONLY=false
SECTION_FILTER=""

# ── Results log ───────────────────────────────────────────────────────
RESULTS_FILE="/tmp/forgetty-vm-test-results-$(date +%Y%m%d-%H%M%S).log"
FAILURES=()

# ── Helpers ───────────────────────────────────────────────────────────

usage() {
    cat <<'USAGE'
Forgetty VM Test Suite (T-026) -- 61 Acceptance Criteria

Usage:
  ./run-vm-tests.sh                    Run all 61 tests interactively
  ./run-vm-tests.sh --auto-only        Run only automated tests (8 ACs)
  ./run-vm-tests.sh --section A        Run a specific section (A-J)
  ./run-vm-tests.sh --section A-C      Run sections A through C
  ./run-vm-tests.sh --help             Show this message

Sections:
  A  Package Install (7 ACs, all automated)
  B  App Launcher Integration (3 ACs, manual)
  C  M1 Features - Terminal Core (20 ACs, manual)
  D  M2 Features - Production Readiness (5 ACs, mixed)
  E  M3 Features - Session Persistence (5 ACs, manual)
  F  Shell Compatibility (5 ACs, manual)
  G  SSH and tmux (5 ACs, manual)
  H  Display Scaling (4 ACs, manual)
  I  Display Server (4 ACs, manual)
  J  Uninstall (3 ACs, mostly automated)
USAGE
}

section_header() {
    local letter="$1" title="$2" count="$3"
    echo ""
    echo -e "${CYAN}================================================================${RESET}"
    echo -e "${BOLD}  Section $letter: $title ($count ACs)${RESET}"
    echo -e "${CYAN}================================================================${RESET}"
}

test_header() {
    local ac="$1" desc="$2" auto="$3"
    TOTAL_COUNT=$((TOTAL_COUNT + 1))
    echo ""
    local tag=""
    if [ "$auto" = "auto" ]; then
        tag="${DIM}[automated]${RESET}"
    else
        tag="${DIM}[manual]${RESET}"
    fi
    echo -e "${CYAN}──${RESET} ${BOLD}$ac${RESET}: $desc  $tag"
}

pass() {
    local ac="$1" msg="${2:-}"
    echo -e "  ${GREEN}PASS${RESET} $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "PASS  $ac  $msg" >> "$RESULTS_FILE"
}

fail() {
    local ac="$1" msg="${2:-}"
    echo -e "  ${RED}FAIL${RESET} $msg"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    FAILURES+=("$ac: $msg")
    echo "FAIL  $ac  $msg" >> "$RESULTS_FILE"
}

skip() {
    local ac="$1" msg="${2:-}"
    echo -e "  ${YELLOW}SKIP${RESET} $msg"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    echo "SKIP  $ac  $msg" >> "$RESULTS_FILE"
}

prompt_result() {
    local ac="$1" desc="$2"
    echo ""
    echo -en "  Result: [${GREEN}p${RESET}]ass / [${RED}f${RESET}]ail / [${YELLOW}s${RESET}]kip? "
    read -r -n1 answer
    echo ""
    case "$answer" in
        p|P) pass "$ac" "$desc" ;;
        f|F) fail "$ac" "$desc" ;;
        *)   skip "$ac" "$desc" ;;
    esac
}

manual_test() {
    local ac="$1" desc="$2"
    shift 2
    # remaining args are instruction lines
    test_header "$ac" "$desc" "manual"
    if $AUTO_ONLY; then
        skip "$ac" "(skipped in --auto-only mode)"
        return
    fi
    echo ""
    echo -e "  ${BOLD}Instructions:${RESET}"
    for line in "$@"; do
        echo "    $line"
    done
    prompt_result "$ac" "$desc"
}

should_run_section() {
    local letter="$1"
    if [ -z "$SECTION_FILTER" ]; then
        return 0
    fi
    # Support single letter or range like "A-C"
    if [[ "$SECTION_FILTER" == *-* ]]; then
        local start="${SECTION_FILTER%%-*}"
        local end="${SECTION_FILTER##*-}"
        if [[ "$letter" > "$start" || "$letter" == "$start" ]] && \
           [[ "$letter" < "$end" || "$letter" == "$end" ]]; then
            return 0
        fi
        return 1
    fi
    # Single letter
    if [ "$letter" = "$SECTION_FILTER" ]; then
        return 0
    fi
    return 1
}

# ── Find .deb file ────────────────────────────────────────────────────

find_deb() {
    local deb=""
    # Check common locations
    for dir in "." "$HOME" "$HOME/Downloads" "/tmp"; do
        local found
        found=$(find "$dir" -maxdepth 1 -name 'forgetty_*.deb' -type f 2>/dev/null | head -1) || true
        if [ -n "$found" ]; then
            deb="$found"
            break
        fi
    done
    echo "$deb"
}

# ══════════════════════════════════════════════════════════════════════
# Section A: Package Install (7 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_a() {
    section_header "A" "Package Install" "7"

    # AC-01: dpkg install
    test_header "AC-01" "dpkg -i installs without error on fresh Ubuntu 24.04" "auto"
    if dpkg -l forgetty 2>/dev/null | grep -q '^ii'; then
        pass "AC-01" "forgetty package is installed"
    else
        local deb
        deb=$(find_deb)
        if [ -z "$deb" ]; then
            fail "AC-01" "No forgetty .deb found. Place it in CWD, ~/Downloads, or /tmp"
        else
            echo "  Found: $deb"
            echo "  Running: sudo dpkg -i $deb"
            if sudo dpkg -i "$deb" 2>&1; then
                pass "AC-01" "dpkg -i completed successfully"
            else
                fail "AC-01" "dpkg -i failed"
            fi
        fi
    fi

    # AC-02: apt-get -f install
    test_header "AC-02" "apt-get -f install resolves dependencies (no dev packages)" "auto"
    local apt_output
    apt_output=$(sudo apt-get -f install -y 2>&1) || true
    if echo "$apt_output" | grep -qi "error"; then
        fail "AC-02" "apt-get -f install reported errors"
        echo "$apt_output" | tail -5
    else
        # Check that no -dev packages were pulled in
        if echo "$apt_output" | grep -q '\-dev '; then
            fail "AC-02" "Dev packages were installed as dependencies"
        else
            pass "AC-02" "Dependencies resolved cleanly"
        fi
    fi

    # AC-03: --version
    test_header "AC-03" "forgetty --version prints version and exits 0" "auto"
    if command -v forgetty &>/dev/null; then
        local ver_output
        ver_output=$(forgetty --version 2>&1) && rc=$? || rc=$?
        if [ "$rc" -eq 0 ] && [ -n "$ver_output" ]; then
            pass "AC-03" "$ver_output"
        else
            fail "AC-03" "exit code=$rc output='$ver_output'"
        fi
    else
        fail "AC-03" "forgetty not found in PATH"
    fi

    # AC-04: --help flags
    test_header "AC-04" "--help shows expected flags" "auto"
    if command -v forgetty &>/dev/null; then
        local help_output
        help_output=$(forgetty --help 2>&1) || true
        local missing=""
        for flag in --working-directory -e --version --help --class --config-file; do
            if ! echo "$help_output" | grep -q -- "$flag"; then
                missing="$missing $flag"
            fi
        done
        if [ -z "$missing" ]; then
            pass "AC-04" "All expected flags present"
        else
            fail "AC-04" "Missing flags:$missing"
        fi
    else
        fail "AC-04" "forgetty not found in PATH"
    fi

    # AC-05: ldd - no missing libraries
    test_header "AC-05" "ldd shows no 'not found'; libghostty-vt.so resolves" "auto"
    if command -v forgetty &>/dev/null; then
        local ldd_output
        ldd_output=$(ldd "$(which forgetty)" 2>&1) || true
        if echo "$ldd_output" | grep -q "not found"; then
            fail "AC-05" "Missing libraries detected:"
            echo "$ldd_output" | grep "not found" | sed 's/^/    /'
        elif echo "$ldd_output" | grep -q "libghostty"; then
            pass "AC-05" "All libraries resolve, libghostty-vt.so found"
        else
            fail "AC-05" "libghostty-vt.so not in ldd output"
        fi
    else
        fail "AC-05" "forgetty not found in PATH"
    fi

    # AC-06: man page
    test_header "AC-06" "man forgetty has NAME, SYNOPSIS, DESCRIPTION, OPTIONS" "auto"
    if man -w forgetty &>/dev/null; then
        local man_output
        man_output=$(man forgetty 2>&1 | col -b) || true
        local missing=""
        for section in NAME SYNOPSIS DESCRIPTION OPTIONS; do
            if ! echo "$man_output" | grep -q "$section"; then
                missing="$missing $section"
            fi
        done
        if [ -z "$missing" ]; then
            pass "AC-06" "Man page has all required sections"
        else
            fail "AC-06" "Missing sections:$missing"
        fi
    else
        fail "AC-06" "man page not found for forgetty"
    fi

    # AC-07: desktop-file-validate
    test_header "AC-07" "desktop-file-validate reports zero errors" "auto"
    local desktop_file="/usr/share/applications/dev.forgetty.Forgetty.desktop"
    if [ -f "$desktop_file" ]; then
        local validate_output
        validate_output=$(desktop-file-validate "$desktop_file" 2>&1) || true
        if [ -z "$validate_output" ]; then
            pass "AC-07" "Desktop file validates cleanly"
        else
            fail "AC-07" "Validation errors:"
            echo "$validate_output" | sed 's/^/    /'
        fi
    else
        fail "AC-07" "Desktop file not found: $desktop_file"
    fi
}

# ══════════════════════════════════════════════════════════════════════
# Section B: App Launcher Integration (3 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_b() {
    section_header "B" "App Launcher Integration" "3"

    manual_test "AC-08" "Forgetty appears in GNOME Activities search" \
        "1. Press the Super key to open Activities" \
        "2. Type 'Forgetty'" \
        "3. Verify: Forgetty icon and entry appear in search results"

    manual_test "AC-09" "Clicking Activities entry launches Forgetty" \
        "1. Click the Forgetty entry in Activities" \
        "2. Verify: A Forgetty window opens with a working shell prompt" \
        "3. Verify: You can type commands and get output"

    manual_test "AC-10" "Forgetty icon visible in taskbar/dock" \
        "1. With Forgetty running, look at the GNOME taskbar/dock" \
        "2. Verify: Forgetty icon is visible while running"
}

# ══════════════════════════════════════════════════════════════════════
# Section C: M1 Features -- Terminal Core (20 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_c() {
    section_header "C" "M1 Features - Terminal Core" "20"

    manual_test "AC-11" "Rendering: colors, vim, htop render correctly" \
        "1. Observe the shell prompt -- it should have colors" \
        "2. Run: vim /etc/hostname  (should render cleanly, no garbled text)" \
        "3. Run: htop  (should render with colors and bars)" \
        "4. Run: fastfetch  (or neofetch if installed)" \
        "5. Verify: No missing glyphs, no garbled text"

    manual_test "AC-12" "Keyboard: typing, arrows, Ctrl+C, tab completion" \
        "1. Type some text -- characters appear correctly" \
        "2. Open vim, use arrow keys to navigate" \
        "3. Run: sleep 100  then press Ctrl+C -- command interrupted" \
        "4. Type 'ls /us' then press Tab -- completes to /usr/"

    manual_test "AC-13" "Mouse: htop clicks, scroll wheel" \
        "1. Run: htop" \
        "2. Click on a process -- it should highlight" \
        "3. Exit htop (q)" \
        "4. Generate scrollback: seq 200" \
        "5. Scroll up with mouse wheel -- scrollback navigates" \
        "6. Open vim with mouse mode: vim, then use scroll wheel"

    manual_test "AC-14" "Tabs: create, switch, close, title shows CWD" \
        "1. Press Ctrl+Shift+T -- new tab opens" \
        "2. Click first tab -- switches back" \
        "3. Verify tab title shows directory basename" \
        "4. Click x on a tab -- tab closes" \
        "5. Close the last tab -- app exits"

    manual_test "AC-15" "Splits: create, resize, navigate, close" \
        "1. Press Alt+Shift+= -- splits right (vertical split)" \
        "2. Press Alt+Shift+- -- splits down (horizontal split)" \
        "3. Drag the divider between splits -- resizes" \
        "4. Press Alt+Arrow keys -- navigates between panes" \
        "5. Press Ctrl+Shift+W -- closes focused pane"

    manual_test "AC-16" "Selection/Copy: click-drag, double/triple click, paste" \
        "1. Run: echo 'hello world test'" \
        "2. Click-drag to select text -- visual highlight appears" \
        "3. Press Ctrl+Shift+C -- copies to clipboard" \
        "4. Double-click a word -- selects word only" \
        "5. Triple-click -- selects entire line" \
        "6. Ctrl+Shift+V to paste -- text is clean, no garbage"

    manual_test "AC-17" "Scrollbar: appears, drags, hides" \
        "1. Run: seq 500  (generate scrollback)" \
        "2. Verify: scrollbar appears on the right" \
        "3. Drag the scrollbar -- viewport scrolls" \
        "4. Run: clear" \
        "5. Verify: scrollbar hides when content fits viewport"

    manual_test "AC-18" "Search: Ctrl+Shift+F, highlight, navigate, escape" \
        "1. Run: echo 'findme one findme two findme three'" \
        "2. Press Ctrl+Shift+F -- search bar opens" \
        "3. Type 'findme' -- matches highlight" \
        "4. Press Enter -- navigates to next match" \
        "5. Press Escape -- search bar closes"

    manual_test "AC-19" "Context menu: right-click shows Copy, Paste, etc." \
        "1. Select some text" \
        "2. Right-click in terminal -- popover appears" \
        "3. Verify menu has: Copy, Paste, Select All, Search" \
        "4. Click Copy -- copies selected text" \
        "5. Click Paste -- inserts clipboard content"

    manual_test "AC-20" "Font zoom: Ctrl+=, Ctrl+-, Ctrl+0" \
        "1. Press Ctrl+= several times -- text gets bigger" \
        "2. Press Ctrl+- several times -- text gets smaller" \
        "3. Press Ctrl+0 -- resets to default size" \
        "4. Verify: grid reflows after zoom (no clipping or overlap)"

    manual_test "AC-21" "URL detection: underline on hover, Ctrl+Click opens" \
        "1. Run: echo 'Visit https://example.com today'" \
        "2. Hover mouse over the URL -- it should underline" \
        "3. Ctrl+Click the URL -- browser should open"

    manual_test "AC-22" "Cursor blink/style: blink in shell, bar in vim insert" \
        "1. Observe cursor in shell -- it should blink" \
        "2. Open vim" \
        "3. In normal mode -- cursor is a block" \
        "4. Press i for insert mode -- cursor changes to bar/beam" \
        "5. Press Escape -- cursor changes back to block"

    manual_test "AC-23" "Bell: visual flash or audio" \
        "1. Run: echo -e '\\a'" \
        "2. Verify: visual flash or audio bell occurs"

    manual_test "AC-24" "Config: config.toml respected, hot reload" \
        "1. Edit ~/.config/forgetty/config.toml" \
        "2. Change font_size to a different value" \
        "3. Save the file" \
        "4. Verify: terminal updates font size live (no restart needed)"

    manual_test "AC-25" "Shortcuts display: F1 opens help window" \
        "1. Press F1" \
        "2. Verify: keyboard shortcuts help window appears" \
        "3. Verify: lists all keybindings"

    manual_test "AC-26" "Hamburger menu: shows all items with shortcuts" \
        "1. Click the hamburger menu (top right)" \
        "2. Verify menu items: Copy, Paste, New Window, Preferences, About, etc." \
        "3. Verify: keyboard shortcuts are displayed next to each item"

    manual_test "AC-27" "Command palette: Ctrl+Shift+P, filter, execute" \
        "1. Press Ctrl+Shift+P -- command palette opens" \
        "2. Type 'split' -- list filters to split-related actions" \
        "3. Press Enter -- executes the selected action" \
        "4. Press Ctrl+Shift+P again, then Escape -- palette closes"

    manual_test "AC-28" "Preferences window: opens, live updates" \
        "1. Click hamburger menu -> Preferences" \
        "2. Verify: settings window opens" \
        "3. Adjust font size slider -- terminal updates live" \
        "4. Change theme -- applies without restart"

    manual_test "AC-29" "Theme browser: preview, apply, revert" \
        "1. Open Preferences -> Themes" \
        "2. Verify: bundled themes are listed" \
        "3. Arrow through the list -- preview applies in real time" \
        "4. Press Enter -- applies the selected theme" \
        "5. Press Escape -- reverts to previous theme"

    manual_test "AC-30" "Paste warning: dialog for multi-line paste" \
        "1. Copy multi-line text to clipboard (e.g., two lines from a text editor)" \
        "2. Paste into terminal with Ctrl+Shift+V" \
        "3. Verify: warning dialog appears about newlines" \
        "4. Click 'Paste anyway' -- text is pasted"
}

# ══════════════════════════════════════════════════════════════════════
# Section D: M2 Features -- Production Readiness (5 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_d() {
    section_header "D" "M2 Features - Production Readiness" "5"

    manual_test "AC-31" "Shell exit auto-close: exit closes tab" \
        "1. Open a new tab (Ctrl+Shift+T)" \
        "2. Type: exit" \
        "3. Verify: tab closes" \
        "4. If it was the last tab, verify: app exits"

    manual_test "AC-32" "Multi-instance: independent windows" \
        "1. Open a terminal and run: forgetty &" \
        "2. A second Forgetty window should appear" \
        "3. Create tabs in each window independently" \
        "4. Close one window -- the other remains unaffected"

    manual_test "AC-33" "CLI flags: --working-directory and -e" \
        "1. Run: forgetty --working-directory /tmp" \
        "2. Verify: new window opens with CWD = /tmp (run pwd)" \
        "3. Close it. Run: forgetty -e htop" \
        "4. Verify: htop opens directly in the window"

    # AC-34: partially automated
    test_header "AC-34" "TERM/terminfo: TERM, COLORTERM, TERM_PROGRAM" "auto"
    if command -v forgetty &>/dev/null; then
        echo -e "  ${BOLD}Check these inside a Forgetty terminal:${RESET}"
        echo "    echo \$TERM          -> expect xterm-256color"
        echo "    echo \$COLORTERM     -> expect truecolor"
        echo "    echo \$TERM_PROGRAM  -> expect forgetty"
        if $AUTO_ONLY; then
            skip "AC-34" "(requires running inside Forgetty)"
        else
            prompt_result "AC-34" "TERM/COLORTERM/TERM_PROGRAM correct"
        fi
    else
        fail "AC-34" "forgetty not installed"
    fi

    manual_test "AC-35" "Signal handling: TERM saves session, no orphans" \
        "1. Open Forgetty with some tabs" \
        "2. Run: kill -TERM \$(pgrep forgetty-daemon)" \
        "3. Verify: daemon saves session and exits cleanly" \
        "4. Run: ps aux | grep -c defunct" \
        "5. Verify: result is 0 (no zombie processes)"
}

# ══════════════════════════════════════════════════════════════════════
# Section E: M3 Features -- Session Persistence + Workspaces (5 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_e() {
    section_header "E" "M3 Features - Session Persistence + Workspaces" "5"

    manual_test "AC-36" "Session persistence: layout survives close/reopen" \
        "1. Open Forgetty with 3 tabs and a split in one tab" \
        "2. cd to different directories in each tab" \
        "3. Close Forgetty" \
        "4. Reopen Forgetty" \
        "5. Verify: same layout restored with correct CWDs"

    manual_test "AC-37" "Workspace manager: create, switch, independent tabs" \
        "1. Create a new workspace named 'Test'" \
        "2. Switch to it" \
        "3. Create tabs in the new workspace" \
        "4. Switch back to the original workspace" \
        "5. Verify: both workspaces maintain independent tab sets"

    manual_test "AC-38" "Workspace persistence: workspaces survive close/reopen" \
        "1. Create 2 workspaces with different tabs" \
        "2. Close Forgetty" \
        "3. Reopen Forgetty" \
        "4. Verify: both workspaces and their tabs are restored"

    manual_test "AC-39" "Daemon architecture: daemon survives GTK close" \
        "1. Open Forgetty, create some tabs" \
        "2. Close the GTK window (click X or Ctrl+Q)" \
        "3. Run: pgrep forgetty-daemon" \
        "4. Verify: daemon process is still running" \
        "5. Reopen Forgetty" \
        "6. Verify: session is alive (not a cold-start with fresh tabs)"

    manual_test "AC-40" "Notification rings: bell in background tab" \
        "1. Open two tabs" \
        "2. In tab 1, run: sleep 3 && echo -e '\\a'" \
        "3. Immediately switch to tab 2" \
        "4. Wait 3 seconds for the bell" \
        "5. Verify: tab 1 shows a notification indicator"
}

# ══════════════════════════════════════════════════════════════════════
# Section F: Shell Compatibility (5 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_f() {
    section_header "F" "Shell Compatibility" "5"

    manual_test "AC-41" "bash: prompt colors, tab completion, Ctrl+R" \
        "1. Open a tab (should be bash by default)" \
        "2. Verify: prompt has colors" \
        "3. Type 'ls /us' + Tab -- completes to /usr/" \
        "4. Press Ctrl+R, type a previous command -- reverse search works"

    manual_test "AC-42" "zsh: prompt, tab completion, themes" \
        "1. Run: forgetty -e zsh" \
        "2. Verify: zsh prompt renders correctly" \
        "3. Type 'ls /us' + Tab -- completes to /usr/" \
        "4. If Oh-My-Zsh is installed, verify themes render"

    manual_test "AC-43" "fish: autosuggestions, tab completion, syntax highlight" \
        "1. Run: forgetty -e fish" \
        "2. Start typing a command -- autosuggestions appear (grey text)" \
        "3. Press Tab -- completion works" \
        "4. Type an invalid command -- syntax highlighting shows red"

    manual_test "AC-44" "Shell in config: config.toml shell setting" \
        "1. Edit ~/.config/forgetty/config.toml" \
        "2. Set: shell = \"/usr/bin/fish\"" \
        "3. Open a new tab" \
        "4. Verify: new tab launches fish (check with: echo \$SHELL or ps)"

    manual_test "AC-45" "Shell profiles: multiple profiles in config" \
        "1. Edit ~/.config/forgetty/config.toml" \
        "2. Create two profiles (bash and fish) if supported" \
        "3. Both should appear in a new-tab dropdown" \
        "4. Select each -- verify the correct shell opens"
}

# ══════════════════════════════════════════════════════════════════════
# Section G: SSH and tmux (5 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_g() {
    section_header "G" "SSH and tmux" "5"

    manual_test "AC-46" "SSH to localhost: colors and arrow keys" \
        "1. Run: ssh localhost" \
        "2. Accept host key if prompted" \
        "3. Verify: colors display correctly" \
        "4. Run: vim /etc/hostname -- arrow keys work"

    manual_test "AC-47" "SSH TERM propagation: TERM=xterm-256color over SSH" \
        "1. SSH to localhost: ssh localhost" \
        "2. Run: echo \$TERM" \
        "3. Verify: output is xterm-256color" \
        "4. Run a 256-color test: for i in \$(seq 0 255); do printf '\\e[48;5;\${i}m  '; done; echo"

    manual_test "AC-48" "tmux inside Forgetty: splits, windows, no glitches" \
        "1. Run: tmux" \
        "2. Create tmux splits: Ctrl+B % (vertical), Ctrl+B \" (horizontal)" \
        "3. Create tmux windows: Ctrl+B c" \
        "4. Navigate: Ctrl+B arrow keys" \
        "5. Verify: no rendering glitches, Ctrl+B keybindings work"

    manual_test "AC-49" "tmux mouse mode: clicks and scroll" \
        "1. Inside tmux, run: tmux set -g mouse on" \
        "2. Click on different tmux panes -- pane selection works" \
        "3. Generate scrollback in a pane: seq 200" \
        "4. Scroll with mouse wheel inside tmux -- works"

    manual_test "AC-50" "screen inside Forgetty: windows, detach/reattach" \
        "1. Run: screen" \
        "2. Create screen windows: Ctrl+A c" \
        "3. Switch windows: Ctrl+A n" \
        "4. Detach: Ctrl+A d" \
        "5. Reattach: screen -r" \
        "6. Verify: session restored correctly"
}

# ══════════════════════════════════════════════════════════════════════
# Section H: Display Scaling (4 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_h() {
    section_header "H" "Display Scaling" "4"

    manual_test "AC-51" "100% scale: text crisp, no grid gaps" \
        "1. Ensure scaling is at default:" \
        "   gsettings set org.gnome.desktop.interface text-scaling-factor 1.0" \
        "2. Open Forgetty" \
        "3. Verify: text is crisp and correctly sized" \
        "4. Verify: cell grid has no visible gaps or overlaps"

    manual_test "AC-52" "150% scale: crisp rendering, no blur" \
        "1. Run: gsettings set org.gnome.desktop.interface text-scaling-factor 1.5" \
        "2. Open Forgetty (or observe existing window)" \
        "3. Verify: text renders crisply (no blurry text)" \
        "4. Verify: no layout overflow or clipping"

    manual_test "AC-53" "200% scale: crisp rendering, no blur" \
        "1. Run: gsettings set org.gnome.desktop.interface text-scaling-factor 2.0" \
        "2. Open Forgetty (or observe existing window)" \
        "3. Verify: text renders crisply (no blurry text)" \
        "4. Verify: no layout overflow or clipping"

    manual_test "AC-54" "Dynamic scale change: redraws without restart" \
        "1. With Forgetty open, change scaling:" \
        "   gsettings set org.gnome.desktop.interface text-scaling-factor 1.5" \
        "2. Verify: terminal redraws correctly without restart" \
        "3. Reset: gsettings reset org.gnome.desktop.interface text-scaling-factor"
}

# ══════════════════════════════════════════════════════════════════════
# Section I: Display Server (4 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_i() {
    section_header "I" "Display Server" "4"

    manual_test "AC-55" "Wayland session: all features work" \
        "1. Log in to a GNOME Wayland session (default on Ubuntu 24.04)" \
        "2. Verify: echo \$XDG_SESSION_TYPE  shows 'wayland'" \
        "3. Launch Forgetty" \
        "4. Verify: all basic features work (tabs, splits, typing)"

    manual_test "AC-56" "X11 session: all features work" \
        "1. Log out, on login screen click gear icon" \
        "2. Select 'GNOME on Xorg'" \
        "3. Log in" \
        "4. Verify: echo \$XDG_SESSION_TYPE  shows 'x11'" \
        "5. Launch Forgetty" \
        "6. Verify: all basic features work"

    manual_test "AC-57" "Wayland clipboard: copy/paste works" \
        "1. In a Wayland session, open Forgetty" \
        "2. Run: echo 'clipboard test'" \
        "3. Select the text, Ctrl+Shift+C to copy" \
        "4. Open another app (e.g., gedit), paste with Ctrl+V" \
        "5. Verify: text pastes correctly" \
        "6. Copy text from gedit, paste into Forgetty with Ctrl+Shift+V"

    manual_test "AC-58" "X11 clipboard: copy/paste works" \
        "1. In an X11 session, open Forgetty" \
        "2. Run: echo 'clipboard test'" \
        "3. Select the text, Ctrl+Shift+C to copy" \
        "4. Open another app, paste with Ctrl+V" \
        "5. Verify: text pastes correctly" \
        "6. Copy text from another app, paste into Forgetty"
}

# ══════════════════════════════════════════════════════════════════════
# Section J: Uninstall (3 ACs)
# ══════════════════════════════════════════════════════════════════════

run_section_j() {
    section_header "J" "Uninstall" "3"

    # AC-59: apt remove
    test_header "AC-59" "apt remove forgetty removes cleanly" "auto"
    if dpkg -l forgetty 2>/dev/null | grep -q '^ii'; then
        echo "  Running: sudo apt remove -y forgetty"
        if sudo apt remove -y forgetty 2>&1; then
            pass "AC-59" "apt remove completed without errors"
        else
            fail "AC-59" "apt remove reported errors"
        fi
    else
        if $AUTO_ONLY; then
            skip "AC-59" "forgetty not currently installed"
        else
            echo "  forgetty is not currently installed."
            echo "  Install it first (Section A), then re-run Section J."
            skip "AC-59" "forgetty not installed"
        fi
    fi

    # AC-60: binary removed
    test_header "AC-60" "forgetty command no longer found after removal" "auto"
    # Refresh hash table
    hash -r 2>/dev/null || true
    if command -v forgetty &>/dev/null; then
        fail "AC-60" "forgetty still found at: $(which forgetty)"
    else
        pass "AC-60" "forgetty command not found (correctly removed)"
    fi

    # AC-61: desktop entry removed
    manual_test "AC-61" "Desktop entry gone from Activities search" \
        "1. Press Super to open Activities" \
        "2. Type 'Forgetty'" \
        "3. Verify: Forgetty no longer appears in search results"
}

# ══════════════════════════════════════════════════════════════════════
# Summary
# ══════════════════════════════════════════════════════════════════════

print_summary() {
    local total=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
    echo ""
    echo -e "${CYAN}================================================================${RESET}"
    echo -e "${BOLD}  SUMMARY${RESET}"
    echo -e "${CYAN}================================================================${RESET}"
    echo ""
    echo -e "  ${GREEN}PASS:${RESET}  $PASS_COUNT"
    echo -e "  ${RED}FAIL:${RESET}  $FAIL_COUNT"
    echo -e "  ${YELLOW}SKIP:${RESET}  $SKIP_COUNT"
    echo -e "  ${BOLD}TOTAL:${RESET} $total / 61"
    echo ""

    if [ ${#FAILURES[@]} -gt 0 ]; then
        echo -e "  ${RED}${BOLD}Failures:${RESET}"
        for f in "${FAILURES[@]}"; do
            echo -e "    ${RED}-${RESET} $f"
        done
        echo ""
    fi

    echo "  Results logged to: $RESULTS_FILE"

    if [ "$FAIL_COUNT" -eq 0 ] && [ "$SKIP_COUNT" -eq 0 ]; then
        echo ""
        echo -e "  ${GREEN}${BOLD}ALL TESTS PASSED${RESET}"
    elif [ "$FAIL_COUNT" -gt 0 ]; then
        echo ""
        echo -e "  ${RED}${BOLD}SOME TESTS FAILED -- see failures above${RESET}"
    fi
    echo ""
}

# ══════════════════════════════════════════════════════════════════════
# Main
# ══════════════════════════════════════════════════════════════════════

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --auto-only)
            AUTO_ONLY=true
            shift
            ;;
        --section)
            SECTION_FILTER="${2:-}"
            if [ -z "$SECTION_FILTER" ]; then
                echo "Error: --section requires a letter (A-J) or range (A-C)"
                exit 1
            fi
            # Uppercase
            SECTION_FILTER=$(echo "$SECTION_FILTER" | tr '[:lower:]' '[:upper:]')
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

# Header
echo -e "${BOLD}"
echo "  ╔═══════════════════════════════════════════════════════════╗"
echo "  ║        Forgetty VM Test Suite  (T-026)                   ║"
echo "  ║        61 Acceptance Criteria on Fresh Ubuntu 24.04      ║"
echo "  ╚═══════════════════════════════════════════════════════════╝"
echo -e "${RESET}"

if $AUTO_ONLY; then
    echo -e "  Mode: ${CYAN}--auto-only${RESET} (running 8 automated tests only)"
fi
if [ -n "$SECTION_FILTER" ]; then
    echo -e "  Filter: ${CYAN}Section $SECTION_FILTER${RESET}"
fi

echo ""
echo "  Results will be logged to: $RESULTS_FILE"
echo "  Date: $(date)"
echo "" > "$RESULTS_FILE"
echo "Forgetty VM Test Results -- $(date)" >> "$RESULTS_FILE"
echo "========================================" >> "$RESULTS_FILE"

# Run sections
should_run_section "A" && run_section_a
should_run_section "B" && run_section_b
should_run_section "C" && run_section_c
should_run_section "D" && run_section_d
should_run_section "E" && run_section_e
should_run_section "F" && run_section_f
should_run_section "G" && run_section_g
should_run_section "H" && run_section_h
should_run_section "I" && run_section_i
should_run_section "J" && run_section_j

print_summary
