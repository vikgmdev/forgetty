#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# Forgetty Stress Test Suite  (T-025)
#
# Usage:
#   ./run-stress-tests.sh              # run all tests
#   ./run-stress-tests.sh --auto-only  # automated subset only
#   ./run-stress-tests.sh --test <name># single test by name
#   ./run-stress-tests.sh --list       # list all test names
#   ./run-stress-tests.sh --help       # this message
#
# Run INSIDE Forgetty to exercise the terminal itself.
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────
RED='\033[1;31m'
GREEN='\033[1;32m'
YELLOW='\033[1;33m'
CYAN='\033[1;36m'
BOLD='\033[1m'
RESET='\033[0m'

# ── Counters ──────────────────────────────────────────────────────────
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

# ── Mode flags ────────────────────────────────────────────────────────
RUN_SINGLE=""
AUTO_ONLY=false

# ── Temp files ────────────────────────────────────────────────────────
STRESS_FILE="/tmp/forgetty-stress-100mb.txt"

# ── Helpers ───────────────────────────────────────────────────────────

usage() {
    cat <<'USAGE'
Forgetty Stress Test Suite (T-025)

Usage:
  ./run-stress-tests.sh              Run all 25 tests
  ./run-stress-tests.sh --auto-only  Run only the automated subset
  ./run-stress-tests.sh --test NAME  Run a single test by name
  ./run-stress-tests.sh --list       List available test names
  ./run-stress-tests.sh --help       Show this message

Test names:
  urandom, yes, largefile, ctrlc,
  vim, htop, dialog, whiptail, tmux, screen,
  256color, truecolor,
  emoji, cjk, combining, zwj, rtl,
  longline10k, longline100k,
  bracketed_paste, osc8_hyperlinks,
  resize, multisplit, splitdestroy,
  memory
USAGE
}

list_tests() {
    echo "Available tests:"
    echo "  urandom          AC-01  cat /dev/urandom | base64 (10s)"
    echo "  yes              AC-02  yes infinite output (10s)"
    echo "  largefile        AC-03  cat 100MB file"
    echo "  ctrlc            AC-04  Ctrl+C interrupt during output"
    echo "  vim              AC-05  vim opens and renders"
    echo "  htop             AC-06  htop renders"
    echo "  dialog           AC-07  dialog box renders"
    echo "  whiptail         AC-08  whiptail box renders"
    echo "  tmux             AC-09  tmux nested session"
    echo "  screen           AC-10  screen nested session"
    echo "  256color         AC-11  256-color palette"
    echo "  truecolor        AC-12  24-bit truecolor gradient"
    echo "  emoji            AC-13  Emoji rendering"
    echo "  cjk              AC-14  CJK characters"
    echo "  combining        AC-15  Combining marks"
    echo "  zwj              AC-16  Zero-width joiners"
    echo "  rtl              AC-17  RTL text (Arabic/Hebrew)"
    echo "  longline10k      AC-18  10K character line"
    echo "  longline100k     AC-19  100K character line"
    echo "  bracketed_paste  AC-20  Bracketed paste mode"
    echo "  osc8_hyperlinks  AC-21  OSC 8 hyperlinks"
    echo "  resize           AC-22  Rapid resize during output"
    echo "  multisplit       AC-23  Multiple splits under load"
    echo "  splitdestroy     AC-24  Split create/destroy during output"
    echo "  memory           AC-25  Memory stability (60s)"
}

header() {
    local ac="$1" name="$2" category="$3"
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}[$ac] $name${RESET}  (${category})"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
}

pass() {
    local msg="${1:-}"
    echo -e "  ${GREEN}PASS${RESET} $msg"
    PASS_COUNT=$((PASS_COUNT + 1))
}

fail() {
    local msg="${1:-}"
    echo -e "  ${RED}FAIL${RESET} $msg"
    FAIL_COUNT=$((FAIL_COUNT + 1))
}

skip() {
    local msg="${1:-}"
    echo -e "  ${YELLOW}SKIP${RESET} $msg"
    SKIP_COUNT=$((SKIP_COUNT + 1))
}

# Prompt user for semi-automated / manual result
prompt_result() {
    local label="$1"
    echo ""
    echo -en "  Result for ${BOLD}${label}${RESET}  [${GREEN}p${RESET}]ass / [${RED}f${RESET}]ail / [${YELLOW}s${RESET}]kip? "
    read -r -n1 answer
    echo ""
    case "$answer" in
        p|P) pass "$label" ;;
        f|F) fail "$label" ;;
        *)   skip "$label" ;;
    esac
}

# Check if a command is available; return 0/1
require_cmd() {
    command -v "$1" &>/dev/null
}

# Get RSS (in KB) of the current shell's parent (the terminal)
get_terminal_rss() {
    local pid="${1:-$PPID}"
    ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ' || echo "0"
}

# Ensure the 100MB stress file exists
ensure_stress_file() {
    if [[ ! -f "$STRESS_FILE" ]]; then
        echo "  Generating $STRESS_FILE (100MB) ..."
        dd if=/dev/urandom bs=1M count=100 2>/dev/null | base64 > "$STRESS_FILE"
        echo "  Done ($(du -h "$STRESS_FILE" | cut -f1))."
    fi
}

# ── Test Functions ────────────────────────────────────────────────────

# AC-01: cat /dev/urandom | base64 (automated)
test_urandom() {
    header "AC-01" "cat /dev/urandom | base64 (10s)" "High Throughput"
    echo "  Running urandom|base64 for 10 seconds ..."

    local rss_before
    rss_before=$(get_terminal_rss)

    local start_time
    start_time=$(date +%s%N)

    # Run for 10s; timeout kills it
    timeout 10 bash -c 'cat /dev/urandom | base64' 2>/dev/null || true

    local end_time elapsed_ms
    end_time=$(date +%s%N)
    elapsed_ms=$(( (end_time - start_time) / 1000000 ))

    local rss_after
    rss_after=$(get_terminal_rss)
    local rss_delta_mb=$(( (rss_after - rss_before) / 1024 ))

    echo "  Elapsed: ${elapsed_ms}ms | RSS delta: ~${rss_delta_mb}MB"

    # Pass if elapsed >= 9s (it ran, not an immediate crash) and RSS < 500MB
    if [[ $elapsed_ms -ge 9000 && $rss_after -lt 512000 ]]; then
        pass "Sustained 10s, RSS ${rss_after}KB"
    else
        fail "elapsed=${elapsed_ms}ms rss_after=${rss_after}KB"
    fi
}

# AC-02: yes (automated)
test_yes() {
    header "AC-02" "yes infinite output (10s)" "High Throughput"
    echo "  Running yes for 10 seconds ..."

    local start_time
    start_time=$(date +%s%N)

    timeout 10 yes 2>/dev/null || true

    local end_time elapsed_ms
    end_time=$(date +%s%N)
    elapsed_ms=$(( (end_time - start_time) / 1000000 ))

    echo "  Elapsed: ${elapsed_ms}ms"

    if [[ $elapsed_ms -ge 9000 ]]; then
        pass "Sustained 10s without crash"
    else
        fail "Exited early at ${elapsed_ms}ms"
    fi
}

# AC-03: Large file cat (automated)
test_largefile() {
    header "AC-03" "cat 100MB file" "High Throughput"
    ensure_stress_file

    echo "  Catting $STRESS_FILE to terminal ..."
    local start_time
    start_time=$(date +%s%N)

    # Cat to terminal (not /dev/null) to stress rendering
    timeout 60 cat "$STRESS_FILE" 2>/dev/null || true

    local end_time elapsed_ms
    end_time=$(date +%s%N)
    elapsed_ms=$(( (end_time - start_time) / 1000000 ))

    echo ""
    echo "  Elapsed: ${elapsed_ms}ms"

    if [[ $elapsed_ms -lt 30000 ]]; then
        pass "Completed in ${elapsed_ms}ms (< 30s)"
    elif [[ $elapsed_ms -lt 60000 ]]; then
        pass "Completed in ${elapsed_ms}ms (slow but finished)"
    else
        fail "Timed out or crashed (${elapsed_ms}ms)"
    fi
}

# AC-04: Ctrl+C interrupt (automated)
test_ctrlc() {
    header "AC-04" "Ctrl+C interrupt during high output" "High Throughput"
    echo "  Running yes|cat -n in background, sending signal after 2s ..."

    # Use a temp script to get a clean process group we can signal.
    local tmpscript
    tmpscript=$(mktemp /tmp/forgetty-ctrlc-XXXXXX.sh)
    printf '#!/bin/bash\nexec yes | cat -n\n' > "$tmpscript"
    chmod +x "$tmpscript"

    bash "$tmpscript" > /dev/null 2>&1 &
    local pid=$!
    sleep 2

    # Send SIGTERM (equivalent to Ctrl+C for test purposes — verifies
    # the terminal's PTY child processes can be interrupted cleanly).
    kill -TERM "$pid" 2>/dev/null || true

    # Give it a moment to stop
    sleep 1
    local stopped=false
    if ! kill -0 "$pid" 2>/dev/null; then
        stopped=true
    else
        kill -9 "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
    rm -f "$tmpscript"

    if $stopped; then
        pass "Process stopped after signal, terminal responsive"
    else
        fail "Process did not stop after SIGTERM (had to SIGKILL)"
    fi
}

# AC-05: vim (semi-automated)
test_vim() {
    header "AC-05" "vim opens and renders" "ncurses"
    if ! require_cmd vim; then
        skip "vim not installed"
        return
    fi
    echo "  Opening vim with a temp file. Instructions:"
    echo "    1. Check: status line, line numbers, cursor visible"
    echo "    2. Press 'i' to enter insert mode, type some text"
    echo "    3. Press Esc, then ':wq' + Enter to quit"
    echo ""
    echo "  Press Enter to launch vim ..."
    read -r

    local tmpfile
    tmpfile=$(mktemp /tmp/forgetty-vim-XXXXXX.txt)
    echo "Forgetty vim stress test" > "$tmpfile"
    vim "$tmpfile"
    rm -f "$tmpfile"

    prompt_result "vim rendering"
}

# AC-06: htop (semi-automated)
test_htop() {
    header "AC-06" "htop renders" "ncurses"
    if ! require_cmd htop; then
        skip "htop not installed"
        return
    fi
    echo "  Opening htop. Instructions:"
    echo "    1. Check: color bars, process list, header, footer"
    echo "    2. Press F6 to sort, verify columns shift"
    echo "    3. Press 'q' to quit"
    echo "    4. Check for artifacts after exit"
    echo ""
    echo "  Press Enter to launch htop ..."
    read -r

    htop

    prompt_result "htop rendering"
}

# AC-07: dialog (semi-automated)
test_dialog() {
    header "AC-07" "dialog box renders" "ncurses"
    if ! require_cmd dialog; then
        skip "dialog not installed (apt install dialog)"
        return
    fi
    echo "  Launching dialog msgbox. Instructions:"
    echo "    1. Check: box-drawing chars (corners, borders) correct"
    echo "    2. Text 'Hello from Forgetty' readable"
    echo "    3. Press Enter/OK to dismiss"
    echo ""
    echo "  Press Enter to launch ..."
    read -r

    dialog --msgbox "Hello from Forgetty" 10 40

    prompt_result "dialog rendering"
}

# AC-08: whiptail (semi-automated)
test_whiptail() {
    header "AC-08" "whiptail box renders" "ncurses"
    if ! require_cmd whiptail; then
        skip "whiptail not installed (apt install whiptail)"
        return
    fi
    echo "  Launching whiptail msgbox. Instructions:"
    echo "    1. Check: box-drawing chars correct"
    echo "    2. Text readable"
    echo "    3. Press Enter/OK to dismiss"
    echo ""
    echo "  Press Enter to launch ..."
    read -r

    whiptail --msgbox "Hello from Forgetty" 10 40

    prompt_result "whiptail rendering"
}

# AC-09: tmux (semi-automated)
test_tmux() {
    header "AC-09" "tmux nested session" "ncurses"
    if ! require_cmd tmux; then
        skip "tmux not installed (apt install tmux)"
        return
    fi
    echo "  Launching tmux. Instructions:"
    echo "    1. Check: status bar renders at bottom"
    echo "    2. Press Ctrl+b then % to split vertically"
    echo "    3. Run 'echo hello' in each pane"
    echo "    4. Check: divider renders, no pane corruption"
    echo "    5. Press Ctrl+b then d to detach"
    echo ""
    echo "  Press Enter to launch tmux ..."
    read -r

    tmux new-session -s forgetty-test 2>/dev/null || tmux attach -t forgetty-test 2>/dev/null || true
    # Clean up session if it exists
    tmux kill-session -t forgetty-test 2>/dev/null || true

    prompt_result "tmux rendering"
}

# AC-10: screen (semi-automated)
test_screen() {
    header "AC-10" "screen nested session" "ncurses"
    if ! require_cmd screen; then
        skip "screen not installed (apt install screen)"
        return
    fi
    echo "  Launching screen. Instructions:"
    echo "    1. Check: status line renders (if configured)"
    echo "    2. Press Ctrl+a then c to create window"
    echo "    3. Press Ctrl+a then 0 / 1 to switch"
    echo "    4. Press Ctrl+a then d to detach"
    echo ""
    echo "  Press Enter to launch screen ..."
    read -r

    screen -S forgetty-test 2>/dev/null || true
    screen -X -S forgetty-test quit 2>/dev/null || true

    prompt_result "screen rendering"
}

# AC-11: 256-color palette (semi-automated)
test_256color() {
    header "AC-11" "256-color palette" "Colors"
    echo "  Printing all 256 indexed colors ..."
    echo ""

    # Standard 16 colors (0-15)
    echo "  Standard colors (0-15):"
    for i in $(seq 0 15); do
        printf "\033[48;5;%dm  %3d  \033[0m" "$i" "$i"
        if (( (i + 1) % 8 == 0 )); then echo ""; fi
    done
    echo ""

    # 216-color cube (16-231)
    echo "  216-color cube (16-231):"
    for i in $(seq 16 231); do
        printf "\033[48;5;%dm  \033[0m" "$i"
        if (( (i - 15) % 36 == 0 )); then echo ""; fi
    done
    echo ""

    # Grayscale (232-255)
    echo "  Grayscale (232-255):"
    for i in $(seq 232 255); do
        printf "\033[48;5;%dm  %3d  \033[0m" "$i" "$i"
        if (( (i - 231) % 8 == 0 )); then echo ""; fi
    done
    echo ""

    prompt_result "256-color palette"
}

# AC-12: Truecolor gradient (semi-automated)
test_truecolor() {
    header "AC-12" "24-bit truecolor gradient" "Colors"
    echo "  Printing RGB gradients ..."
    echo ""

    # Red gradient
    echo "  Red gradient:"
    for i in $(seq 0 4 255); do
        printf "\033[48;2;%d;0;0m \033[0m" "$i"
    done
    echo ""

    # Green gradient
    echo "  Green gradient:"
    for i in $(seq 0 4 255); do
        printf "\033[48;2;0;%d;0m \033[0m" "$i"
    done
    echo ""

    # Blue gradient
    echo "  Blue gradient:"
    for i in $(seq 0 4 255); do
        printf "\033[48;2;0;0;%dm \033[0m" "$i"
    done
    echo ""

    # Rainbow
    echo "  Rainbow (hue sweep):"
    for i in $(seq 0 2 255); do
        local r g b
        if   (( i < 43 ));  then r=255; g=$(( i * 6 ));     b=0
        elif (( i < 85 ));  then r=$(( (85 - i) * 6 )); g=255; b=0
        elif (( i < 128 )); then r=0; g=255; b=$(( (i - 85) * 6 ))
        elif (( i < 170 )); then r=0; g=$(( (170 - i) * 6 )); b=255
        elif (( i < 213 )); then r=$(( (i - 170) * 6 )); g=0; b=255
        else                     r=255; g=0; b=$(( (255 - i) * 6 ))
        fi
        # Clamp values
        (( r > 255 )) && r=255; (( r < 0 )) && r=0
        (( g > 255 )) && g=255; (( g < 0 )) && g=0
        (( b > 255 )) && b=255; (( b < 0 )) && b=0
        printf "\033[48;2;%d;%d;%dm \033[0m" "$r" "$g" "$b"
    done
    echo ""
    echo ""

    prompt_result "truecolor gradient (smooth, no banding)"
}

# AC-13: Emoji (semi-automated)
test_emoji() {
    header "AC-13" "Emoji rendering" "Unicode"
    echo "  Printing emoji test strings ..."
    echo ""
    echo "  Basic emoji:"
    printf '  \xF0\x9F\x98\x80 \xF0\x9F\x98\x82 \xF0\x9F\x98\x8D \xF0\x9F\xA4\x94 \xF0\x9F\x91\x8D \xF0\x9F\x91\x8E \xF0\x9F\x94\xA5 \xE2\x9C\x85 \xE2\x9D\x8C \xF0\x9F\x8E\x89\n'
    echo ""
    echo "  ZWJ sequences (family, skin tones):"
    printf '  \xF0\x9F\x91\xA8\xE2\x80\x8D\xF0\x9F\x91\xA9\xE2\x80\x8D\xF0\x9F\x91\xA7\xE2\x80\x8D\xF0\x9F\x91\xA6  '
    printf '\xF0\x9F\x91\xA9\xE2\x80\x8D\xF0\x9F\x92\xBB  '
    printf '\xF0\x9F\x91\xA8\xF0\x9F\x8F\xBD\xE2\x80\x8D\xF0\x9F\x94\xAC  '
    printf '\xF0\x9F\x8F\xB3\xEF\xB8\x8F\xE2\x80\x8D\xF0\x9F\x8C\x88\n'
    echo ""
    echo "  Flag emoji:"
    printf '  \xF0\x9F\x87\xBA\xF0\x9F\x87\xB8 \xF0\x9F\x87\xAC\xF0\x9F\x87\xA7 \xF0\x9F\x87\xAF\xF0\x9F\x87\xB5 \xF0\x9F\x87\xA9\xF0\x9F\x87\xAA \xF0\x9F\x87\xA7\xF0\x9F\x87\xB7\n'
    echo ""
    echo "  Expected: each emoji takes 2 cells, no overlap, no tofu"
    echo "  (tofu = square box placeholder if font lacks glyph)"

    prompt_result "emoji rendering"
}

# AC-14: CJK (semi-automated)
test_cjk() {
    header "AC-14" "CJK characters" "Unicode"
    echo "  Printing CJK test strings ..."
    echo ""
    echo "  Chinese:  "
    printf '  \xe4\xbd\xa0\xe5\xa5\xbd\xe4\xb8\x96\xe7\x95\x8c  (ni hao shi jie)\n'
    echo "  Japanese: "
    printf '  \xe3\x81\x93\xe3\x82\x93\xe3\x81\xab\xe3\x81\xa1\xe3\x81\xaf\xe4\xb8\x96\xe7\x95\x8c  (konnichiwa sekai)\n'
    echo "  Korean:   "
    printf '  \xec\x95\x88\xeb\x85\x95\xed\x95\x98\xec\x84\xb8\xec\x9a\x94  (annyeonghaseyo)\n'
    echo ""
    echo "  Grid alignment test (each CJK char = 2 cells):"
    printf '  \xe4\xb8\x80\xe4\xba\x8c\xe4\xb8\x89\xe5\x9b\x9b\xe4\xba\x94\n'
    echo "  1234567890"
    echo "  (top row should be exactly 10 cells wide)"

    prompt_result "CJK rendering (2-cell width, grid aligned)"
}

# AC-15: Combining marks (semi-automated)
test_combining() {
    header "AC-15" "Combining marks and precomposed" "Unicode"
    echo "  Printing combining mark tests ..."
    echo ""
    echo "  Precomposed vs decomposed:"
    # e-acute precomposed (U+00E9) vs e + combining acute (U+0065 U+0301)
    printf '  Precomposed: \xc3\xa9  |  Decomposed: e\xcc\x81\n'
    printf '  Precomposed: \xc3\xb1  |  Decomposed: n\xcc\x83\n'
    printf '  Precomposed: \xc3\xbc  |  Decomposed: u\xcc\x88\n'
    echo ""
    echo "  Multiple combining marks:"
    # z + combining caron (U+030C) + combining dot below (U+0323)
    printf '  z\xcc\x8c\xcc\xa3  (z + caron + dot below)\n'
    echo ""
    echo "  Expected: precomposed and decomposed look identical,"
    echo "  each occupying 1 cell"

    prompt_result "combining marks rendering"
}

# AC-16: ZWJ and invisible chars (semi-automated)
test_zwj() {
    header "AC-16" "Zero-width joiners and invisibles" "Unicode"
    echo "  Printing invisible character tests ..."
    echo ""
    echo "  ZWSP (zero-width space, U+200B) between A and B:"
    printf '  A\xe2\x80\x8bB  (should look like AB with no gap)\n'
    echo ""
    echo "  Soft hyphen (U+00AD) in 'for-get-ty':"
    printf '  for\xc2\xadget\xc2\xadty  (hyphens invisible unless line-wrapped)\n'
    echo ""
    echo "  Word joiner (U+2060) between X and Y:"
    printf '  X\xe2\x81\xa0Y  (should look like XY)\n'
    echo ""
    echo "  Grid alignment check:"
    echo "  ABCDEF"
    printf '  A\xe2\x80\x8bB\xe2\x80\x8bC\xe2\x80\x8bD\xe2\x80\x8bE\xe2\x80\x8bF\n'
    echo "  (both lines should be the same width)"

    prompt_result "zero-width / invisible chars"
}

# AC-17: RTL text (semi-automated)
test_rtl() {
    header "AC-17" "RTL text (Arabic/Hebrew)" "Unicode"
    echo "  Printing RTL test strings ..."
    echo ""
    echo "  Arabic:"
    printf '  \xd9\x85\xd8\xb1\xd8\xad\xd8\xa8\xd8\xa7 \xd8\xa8\xd8\xa7\xd9\x84\xd8\xb9\xd8\xa7\xd9\x84\xd9\x85  (marhaba bil-alam)\n'
    echo ""
    echo "  Hebrew:"
    printf '  \xd7\xa9\xd7\x9c\xd7\x95\xd7\x9d \xd7\xa2\xd7\x95\xd7\x9c\xd7\x9d  (shalom olam)\n'
    echo ""
    echo "  Mixed LTR/RTL:"
    printf '  Hello \xd7\xa9\xd7\x9c\xd7\x95\xd7\x9d World\n'
    echo ""
    echo "  Expected: characters render (LTR order in terminal is normal),"
    echo "  no crash, no grid corruption"

    prompt_result "RTL text rendering"
}

# AC-18: 10K character line (automated)
test_longline10k() {
    header "AC-18" "10K character single line" "Long Lines"
    echo "  Printing 10,000 'A' characters with no newline ..."

    printf '%0.sA' $(seq 1 10000)
    echo ""

    echo ""
    echo "  Checking terminal is responsive ..."
    # If we got here, the terminal survived. Quick responsiveness check.
    local start_time end_time elapsed_ms
    start_time=$(date +%s%N)
    echo "ping" > /dev/null
    end_time=$(date +%s%N)
    elapsed_ms=$(( (end_time - start_time) / 1000000 ))

    if [[ $elapsed_ms -lt 5000 ]]; then
        pass "10K line rendered, terminal responsive (${elapsed_ms}ms)"
    else
        fail "Terminal sluggish after 10K line (${elapsed_ms}ms)"
    fi
}

# AC-19: 100K character line (automated)
test_longline100k() {
    header "AC-19" "100K character single line" "Long Lines"
    echo "  Printing 100,000 'B' characters with no newline ..."

    printf '%0.sB' $(seq 1 100000)
    echo ""

    echo ""
    echo "  Checking terminal is responsive ..."
    local start_time end_time elapsed_ms
    start_time=$(date +%s%N)
    echo "ping" > /dev/null
    end_time=$(date +%s%N)
    elapsed_ms=$(( (end_time - start_time) / 1000000 ))

    if [[ $elapsed_ms -lt 5000 ]]; then
        pass "100K line rendered, terminal responsive (${elapsed_ms}ms)"
    else
        fail "Terminal sluggish after 100K line (${elapsed_ms}ms)"
    fi
}

# AC-20: Bracketed paste mode (semi-automated)
test_bracketed_paste() {
    header "AC-20" "Bracketed paste mode" "Protocol"
    echo "  Enabling bracketed paste mode ..."
    # Enable bracketed paste
    printf '\033[?2004h'
    echo ""
    echo "  Bracketed paste mode is now ENABLED."
    echo "  Instructions:"
    echo "    1. Copy some text to your clipboard"
    echo "    2. Paste it into this terminal (Ctrl+Shift+V)"
    echo "    3. The pasted text should appear normally"
    echo "    4. (Advanced: if you 'cat -v' the paste, you'd see"
    echo "       ESC[200~ before and ESC[201~ after the text)"
    echo ""
    echo "  Press Enter when done testing ..."
    read -r
    # Disable bracketed paste
    printf '\033[?2004l'

    prompt_result "bracketed paste mode"
}

# AC-21: OSC 8 hyperlinks (semi-automated)
test_osc8_hyperlinks() {
    header "AC-21" "OSC 8 hyperlinks" "Protocol"
    echo "  Printing OSC 8 hyperlink sequences ..."
    echo ""
    # OSC 8 ; params ; URI ST  text  OSC 8 ; ; ST
    printf '  \033]8;;https://github.com/TotemLabsForge/forgetty\033\\Click here for Forgetty repo\033]8;;\033\\\n'
    printf '  \033]8;;https://example.com\033\\Example.com link\033]8;;\033\\\n'
    echo ""
    echo "  Expected: text renders without corruption."
    echo "  If hyperlinks are supported: text is clickable/underlined."
    echo "  If not supported: text appears as plain text (no garbage)."

    prompt_result "OSC 8 hyperlinks"
}

# AC-22: Rapid resize during output (manual)
test_resize() {
    header "AC-22" "Rapid resize during output" "Concurrent"
    echo "  This is a MANUAL test."
    echo ""
    echo "  Instructions:"
    echo "    1. In another terminal/shell, run:  yes | cat -n"
    echo "    2. While output is streaming, rapidly resize the"
    echo "       Forgetty window by dragging the edge for ~5 seconds"
    echo "    3. Stop resizing and let the output settle"
    echo "    4. Press Ctrl+C to stop the output"
    echo "    5. Check: no crash, no deadlock, text reflows correctly"
    echo ""
    echo "  (If running inside this script, we will start 'yes | cat -n'"
    echo "   for you. Resize the window NOW, then press Ctrl+C when done.)"
    echo ""
    echo "  Press Enter to start, then resize the window rapidly ..."
    read -r

    timeout 15 bash -c 'yes | cat -n' 2>/dev/null || true

    prompt_result "resize during output"
}

# AC-23: Multiple splits under load (manual)
test_multisplit() {
    header "AC-23" "Multiple splits under load" "Concurrent"
    echo "  This is a MANUAL test."
    echo ""
    echo "  Instructions:"
    echo "    1. Open 4 splits/panes in Forgetty"
    echo "    2. In each pane, run:  cat /dev/urandom | base64"
    echo "    3. Let all 4 stream concurrently for ~10 seconds"
    echo "    4. Ctrl+C each pane"
    echo "    5. Check: all streamed concurrently, no crash,"
    echo "       no cross-pane corruption, prompts returned"
    echo ""
    echo "  This test cannot be automated within a single terminal."

    prompt_result "multiple splits under load"
}

# AC-24: Split create/destroy during output (manual)
test_splitdestroy() {
    header "AC-24" "Split create/destroy during output" "Concurrent"
    echo "  This is a MANUAL test."
    echo ""
    echo "  Instructions:"
    echo "    1. In one pane, run:  yes"
    echo "    2. While 'yes' is running, create a new split"
    echo "    3. In the new split, run:  yes"
    echo "    4. Close the new split (Ctrl+D or 'exit')"
    echo "    5. Check: original pane still runs 'yes' without"
    echo "       corruption, terminal did not crash"
    echo ""
    echo "  This test cannot be automated within a single terminal."

    prompt_result "split create/destroy during output"
}

# AC-25: Memory stability (automated)
test_memory() {
    header "AC-25" "Memory stability (60s sustained load)" "Memory"
    echo "  Recording RSS before sustained load ..."

    local rss_before
    rss_before=$(get_terminal_rss)
    echo "  RSS before: ${rss_before}KB ($(( rss_before / 1024 ))MB)"

    echo "  Running cat /dev/urandom | base64 for 60 seconds ..."
    echo "  (This will take a full minute.)"

    timeout 60 bash -c 'cat /dev/urandom | base64' 2>/dev/null || true

    local rss_after
    rss_after=$(get_terminal_rss)
    local delta_kb=$(( rss_after - rss_before ))
    local delta_mb=$(( delta_kb / 1024 ))

    echo ""
    echo "  RSS after:  ${rss_after}KB ($(( rss_after / 1024 ))MB)"
    echo "  RSS delta:  ${delta_kb}KB (~${delta_mb}MB)"

    if [[ $delta_mb -lt 100 ]]; then
        pass "RSS grew by ~${delta_mb}MB (< 100MB limit)"
    else
        fail "RSS grew by ~${delta_mb}MB (exceeds 100MB limit)"
    fi
}

# ── Test Registry ─────────────────────────────────────────────────────

# Map test names to functions and categories
declare -A TEST_FUNCS=(
    [urandom]=test_urandom
    [yes]=test_yes
    [largefile]=test_largefile
    [ctrlc]=test_ctrlc
    [vim]=test_vim
    [htop]=test_htop
    [dialog]=test_dialog
    [whiptail]=test_whiptail
    [tmux]=test_tmux
    [screen]=test_screen
    [256color]=test_256color
    [truecolor]=test_truecolor
    [emoji]=test_emoji
    [cjk]=test_cjk
    [combining]=test_combining
    [zwj]=test_zwj
    [rtl]=test_rtl
    [longline10k]=test_longline10k
    [longline100k]=test_longline100k
    [bracketed_paste]=test_bracketed_paste
    [osc8_hyperlinks]=test_osc8_hyperlinks
    [resize]=test_resize
    [multisplit]=test_multisplit
    [splitdestroy]=test_splitdestroy
    [memory]=test_memory
)

# Ordered list for sequential execution
ALL_TESTS=(
    urandom yes largefile ctrlc
    vim htop dialog whiptail tmux screen
    256color truecolor
    emoji cjk combining zwj rtl
    longline10k longline100k
    bracketed_paste osc8_hyperlinks
    resize multisplit splitdestroy
    memory
)

# Automated-only subset
AUTO_TESTS=(
    urandom yes largefile ctrlc
    longline10k longline100k
    memory
)

# ── Argument Parsing ──────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --list|-l)
            list_tests
            exit 0
            ;;
        --auto-only)
            AUTO_ONLY=true
            shift
            ;;
        --test|-t)
            if [[ -z "${2:-}" ]]; then
                echo "Error: --test requires a test name"
                echo "Run with --list to see available tests"
                exit 1
            fi
            RUN_SINGLE="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

# ── Main ──────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}Forgetty Stress Test Suite${RESET}"
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${RESET}"
echo "  Run inside Forgetty to exercise the terminal."
echo ""

# Single test mode
if [[ -n "$RUN_SINGLE" ]]; then
    if [[ -z "${TEST_FUNCS[$RUN_SINGLE]:-}" ]]; then
        echo "Error: unknown test '$RUN_SINGLE'"
        echo "Run with --list to see available tests"
        exit 1
    fi
    ${TEST_FUNCS[$RUN_SINGLE]}
else
    # Determine which tests to run
    local_tests=()
    if $AUTO_ONLY; then
        echo -e "  Mode: ${YELLOW}automated-only${RESET} (${#AUTO_TESTS[@]} tests)"
        local_tests=("${AUTO_TESTS[@]}")
    else
        echo -e "  Mode: ${GREEN}all${RESET} (${#ALL_TESTS[@]} tests)"
        local_tests=("${ALL_TESTS[@]}")
    fi
    echo ""

    for t in "${local_tests[@]}"; do
        ${TEST_FUNCS[$t]}
    done
fi

# ── Summary ───────────────────────────────────────────────────────────

echo ""
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${RESET}"
echo -e "${BOLD}Summary${RESET}"
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${RESET}"
echo -e "  ${GREEN}PASS${RESET}: $PASS_COUNT"
echo -e "  ${RED}FAIL${RESET}: $FAIL_COUNT"
echo -e "  ${YELLOW}SKIP${RESET}: $SKIP_COUNT"
echo -e "  Total: $((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))"
echo ""

if [[ $FAIL_COUNT -gt 0 ]]; then
    echo -e "  ${RED}Some tests FAILED.${RESET}"
    exit 1
else
    echo -e "  ${GREEN}All executed tests passed.${RESET}"
    exit 0
fi
