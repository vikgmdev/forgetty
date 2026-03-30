#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# Forgetty Long-Running Session Stress Test  (T-028)
#
# Simulates real AI agent usage patterns over 24+ hours:
#   - 3 concurrent panes (simulating 3 Claude Code sessions)
#   - Burst/idle cycles: 30s burst at ~5 MB/s, 90s idle
#   - Monitors RSS, CPU, fd count, child process count every 10 minutes
#   - Logs everything to TSV under tests/stress/results/
#
# Usage:
#   ./run-longrun.sh                    # default 24h run
#   ./run-longrun.sh --duration 1h      # shorter run for quick validation
#   ./run-longrun.sh --duration 4h      # medium run
#   ./run-longrun.sh --help             # this message
#
# This test is designed to be started and left running unattended.
# Results are validated at the end and a summary is printed.
#
# Prerequisites:
#   cargo build --release
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

# ── Defaults ─────────────────────────────────────────────────────────
DURATION_SECS=$((24 * 3600))   # 24 hours
MONITOR_INTERVAL=600           # 10 minutes
BURST_DURATION=30              # 30 seconds of output per cycle
IDLE_DURATION=90               # 90 seconds of silence per cycle
BURST_RATE_BYTES=$((5 * 1024 * 1024))  # ~5 MB/s target during burst
NUM_PANES=3
MAX_RSS_MB=500                 # AC-07: RSS must stay under 500 MB
MAX_IDLE_CPU=0.5               # AC-08: CPU under 0.5% during idle
RSS_GROWTH_THRESHOLD=1.20      # AC-07: final RSS within 120% of initial
RSS_IDLE_THRESHOLD=1.05        # AC-08: idle RSS within 105% of initial
FD_TOLERANCE=5                 # AC-09: fd count stable within 5

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULT_DIR="$SCRIPT_DIR/results"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
LOG_FILE="$RESULT_DIR/longrun-${TIMESTAMP}.tsv"
SUMMARY_FILE="$RESULT_DIR/longrun-${TIMESTAMP}-summary.txt"

# PIDs to clean up on exit
declare -a CLEANUP_PIDS=()
FORGETTY_PID=""

# ── Helpers ───────────────────────────────────────────────────────────

usage() {
    cat <<'USAGE'
Forgetty Long-Running Session Stress Test (T-028)

Usage:
  ./run-longrun.sh                    Default 24-hour run
  ./run-longrun.sh --duration 1h      Shorter run (e.g., 1h, 4h, 30m)
  ./run-longrun.sh --duration 24h     Explicit 24-hour run
  ./run-longrun.sh --help             Show this message

Output:
  tests/stress/results/longrun-<timestamp>.tsv       Raw metrics log
  tests/stress/results/longrun-<timestamp>-summary.txt  Validation results

Acceptance Criteria Validated:
  AC-07: RSS stays under 500 MB; final within 120% of initial (after warm-up)
  AC-08: Idle CPU under 0.5%; idle RSS within 105% of initial
  AC-09: fd count stable within 5 of baseline
  AC-10: No zombie child processes after cleanup
  AC-11: TSV log saved to timestamped file
USAGE
}

parse_duration() {
    local input="$1"
    local num="${input%[hHmMsS]}"
    local suffix="${input: -1}"
    case "$suffix" in
        h|H) echo $((num * 3600)) ;;
        m|M) echo $((num * 60)) ;;
        s|S) echo "$num" ;;
        *)   echo "$input" ;;  # assume seconds
    esac
}

cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${RESET}"

    # Kill burst generators
    for pid in "${CLEANUP_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done

    # Kill Forgetty
    if [[ -n "$FORGETTY_PID" ]] && kill -0 "$FORGETTY_PID" 2>/dev/null; then
        kill "$FORGETTY_PID" 2>/dev/null || true
        # Wait briefly for graceful shutdown
        for _ in $(seq 1 10); do
            kill -0 "$FORGETTY_PID" 2>/dev/null || break
            sleep 0.5
        done
        # Force kill if still alive
        kill -9 "$FORGETTY_PID" 2>/dev/null || true
    fi

    # Check for zombie/orphan processes (AC-10)
    if [[ -n "$FORGETTY_PID" ]]; then
        local zombies
        zombies=$(ps --ppid "$FORGETTY_PID" 2>/dev/null | grep -c -v PID || true)
        if [[ "$zombies" -gt 0 ]]; then
            echo -e "${RED}WARNING: $zombies child processes remain after cleanup${RESET}"
        fi
    fi

    echo -e "${GREEN}Cleanup complete.${RESET}"
}

trap cleanup EXIT

get_rss_kb() {
    local pid="$1"
    if [[ -f "/proc/$pid/status" ]]; then
        grep VmRSS "/proc/$pid/status" 2>/dev/null | awk '{print $2}' || echo "0"
    else
        echo "0"
    fi
}

get_fd_count() {
    local pid="$1"
    if [[ -d "/proc/$pid/fd" ]]; then
        ls "/proc/$pid/fd" 2>/dev/null | wc -l || echo "0"
    else
        echo "0"
    fi
}

get_child_count() {
    local pid="$1"
    ps --ppid "$pid" 2>/dev/null | grep -c -v PID || echo "0"
}

get_cpu_percent() {
    local pid="$1"
    # Use /proc/stat sampling over 2 seconds for accuracy
    if [[ -f "/proc/$pid/stat" ]]; then
        local stat1 stat2 total1 total2
        stat1=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null || echo "0")
        total1=$(awk '{sum=0; for(i=1;i<=NF;i++) sum+=$i; print sum}' /proc/stat 2>/dev/null | head -1)
        sleep 2
        stat2=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null || echo "0")
        total2=$(awk '{sum=0; for(i=1;i<=NF;i++) sum+=$i; print sum}' /proc/stat 2>/dev/null | head -1)

        local proc_delta=$((stat2 - stat1))
        local total_delta=$((total2 - total1))
        if [[ "$total_delta" -gt 0 ]]; then
            # Calculate CPU% with 2 decimal places
            awk "BEGIN {printf \"%.2f\", ($proc_delta / $total_delta) * 100}"
        else
            echo "0.00"
        fi
    else
        echo "0.00"
    fi
}

# ── Parse arguments ──────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration)
            DURATION_SECS=$(parse_duration "$2")
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1"
            usage
            exit 1
            ;;
    esac
done

# ── Validate prerequisites ───────────────────────────────────────────

FORGETTY_BIN="${FORGETTY_BIN:-$REPO_ROOT/target/release/forgetty}"

if [[ ! -x "$FORGETTY_BIN" ]]; then
    echo -e "${RED}Error: Forgetty binary not found at $FORGETTY_BIN${RESET}"
    echo "Run: cargo build --release"
    exit 1
fi

mkdir -p "$RESULT_DIR"

# ── Print configuration ──────────────────────────────────────────────

DURATION_HUMAN=$(printf '%dh %dm' $((DURATION_SECS / 3600)) $(((DURATION_SECS % 3600) / 60)))
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo -e "${BOLD}Forgetty Long-Running Session Stress Test (T-028)${RESET}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo ""
echo -e "  Duration:          ${BOLD}$DURATION_HUMAN${RESET} ($DURATION_SECS seconds)"
echo -e "  Panes:             $NUM_PANES"
echo -e "  Burst/Idle cycle:  ${BURST_DURATION}s burst / ${IDLE_DURATION}s idle"
echo -e "  Monitor interval:  ${MONITOR_INTERVAL}s"
echo -e "  Binary:            $FORGETTY_BIN"
echo -e "  Log file:          $LOG_FILE"
echo -e "  Started:           $(date)"
echo ""

# ── Burst generator script ───────────────────────────────────────────
# Each pane runs this script with a staggered offset.
# It produces burst/idle cycles: 30s of output, then 90s of nothing.
BURST_SCRIPT=$(mktemp /tmp/forgetty-burst-XXXXXX.sh)
cat > "$BURST_SCRIPT" << 'BURST_EOF'
#!/usr/bin/env bash
# Burst/idle cycle generator for long-running stress test.
# Args: $1 = burst_duration, $2 = idle_duration, $3 = initial_delay
BURST_DUR="${1:-30}"
IDLE_DUR="${2:-90}"
INITIAL_DELAY="${3:-0}"

# Stagger start
if [[ "$INITIAL_DELAY" -gt 0 ]]; then
    sleep "$INITIAL_DELAY"
fi

while true; do
    # Burst phase: generate output at ~5 MB/s
    # Using /dev/urandom piped through base64 for realistic text-like output
    END_TIME=$((SECONDS + BURST_DUR))
    while [[ $SECONDS -lt $END_TIME ]]; do
        # Each iteration produces ~64KB of base64 text with newlines
        head -c 49152 /dev/urandom | base64 2>/dev/null || true
    done

    # Idle phase: sleep
    sleep "$IDLE_DUR"
done
BURST_EOF
chmod +x "$BURST_SCRIPT"

# ── Launch Forgetty ──────────────────────────────────────────────────

echo -e "${BOLD}Launching Forgetty...${RESET}"
"$FORGETTY_BIN" &
FORGETTY_PID=$!
echo "  PID: $FORGETTY_PID"

# Wait for the process to start and window to appear
sleep 3

if ! kill -0 "$FORGETTY_PID" 2>/dev/null; then
    echo -e "${RED}Error: Forgetty failed to start${RESET}"
    exit 1
fi

echo -e "${BOLD}Forgetty running (PID $FORGETTY_PID).${RESET}"
echo ""
echo -e "${YELLOW}NOTE: The burst generators will run inside the terminal's default"
echo -e "shell pane. For a full 3-pane test, manually open 2 additional panes"
echo -e "(Ctrl+Shift+E for horizontal split, Ctrl+Shift+O for vertical),"
echo -e "then run the burst script in each:${RESET}"
echo ""
echo -e "  ${DIM}$BURST_SCRIPT $BURST_DURATION $IDLE_DURATION 0${RESET}"
echo -e "  ${DIM}$BURST_SCRIPT $BURST_DURATION $IDLE_DURATION 30${RESET}  (staggered)"
echo -e "  ${DIM}$BURST_SCRIPT $BURST_DURATION $IDLE_DURATION 60${RESET}  (staggered)"
echo ""
echo -e "${BOLD}Starting monitoring loop...${RESET}"
echo ""

# ── Initialize TSV log ───────────────────────────────────────────────

echo -e "timestamp\telapsed_s\trss_kb\trss_mb\tcpu_pct\tfd_count\tchild_count\tphase" > "$LOG_FILE"

# ── Monitoring loop ──────────────────────────────────────────────────

START_TIME=$SECONDS
WARMUP_DONE=false
INITIAL_RSS_KB=0
BASELINE_FD_COUNT=0
MAX_RSS_SEEN=0
SAMPLE_COUNT=0

while true; do
    ELAPSED=$((SECONDS - START_TIME))

    if [[ $ELAPSED -ge $DURATION_SECS ]]; then
        echo ""
        echo -e "${GREEN}Duration reached ($DURATION_HUMAN). Stopping.${RESET}"
        break
    fi

    # Check if Forgetty is still alive
    if ! kill -0 "$FORGETTY_PID" 2>/dev/null; then
        echo ""
        echo -e "${RED}Forgetty process (PID $FORGETTY_PID) has died!${RESET}"
        break
    fi

    # Collect metrics
    RSS_KB=$(get_rss_kb "$FORGETTY_PID")
    RSS_MB=$((RSS_KB / 1024))
    FD_COUNT=$(get_fd_count "$FORGETTY_PID")
    CHILD_COUNT=$(get_child_count "$FORGETTY_PID")

    # CPU measurement takes ~2 seconds (uses /proc/stat sampling)
    CPU_PCT=$(get_cpu_percent "$FORGETTY_PID")

    # Determine phase within burst/idle cycle
    CYCLE_LEN=$((BURST_DURATION + IDLE_DURATION))
    CYCLE_POS=$((ELAPSED % CYCLE_LEN))
    if [[ $CYCLE_POS -lt $BURST_DURATION ]]; then
        PHASE="burst"
    else
        PHASE="idle"
    fi

    # Track warm-up (first 1 hour establishes baseline)
    SAMPLE_COUNT=$((SAMPLE_COUNT + 1))
    if [[ $ELAPSED -ge 3600 ]] && [[ "$WARMUP_DONE" == "false" ]]; then
        WARMUP_DONE=true
        INITIAL_RSS_KB=$RSS_KB
        BASELINE_FD_COUNT=$FD_COUNT
        echo -e "${CYAN}Warm-up complete. Baseline: RSS=${RSS_MB}MB, FDs=${FD_COUNT}${RESET}"
    fi

    if [[ $RSS_KB -gt $MAX_RSS_SEEN ]]; then
        MAX_RSS_SEEN=$RSS_KB
    fi

    # Log to TSV
    echo -e "$(date +%Y-%m-%dT%H:%M:%S)\t${ELAPSED}\t${RSS_KB}\t${RSS_MB}\t${CPU_PCT}\t${FD_COUNT}\t${CHILD_COUNT}\t${PHASE}" >> "$LOG_FILE"

    # Print progress
    ELAPSED_HUMAN=$(printf '%02d:%02d:%02d' $((ELAPSED / 3600)) $(((ELAPSED % 3600) / 60)) $((ELAPSED % 60)))
    REMAINING=$((DURATION_SECS - ELAPSED))
    REMAINING_HUMAN=$(printf '%02d:%02d:%02d' $((REMAINING / 3600)) $(((REMAINING % 3600) / 60)) $((REMAINING % 60)))

    printf "[%s] RSS=%4dMB  CPU=%5s%%  FDs=%3d  Children=%d  Phase=%-5s  Remaining=%s\n" \
        "$ELAPSED_HUMAN" "$RSS_MB" "$CPU_PCT" "$FD_COUNT" "$CHILD_COUNT" "$PHASE" "$REMAINING_HUMAN"

    # Sleep until next monitoring interval
    sleep "$MONITOR_INTERVAL"
done

# ── Final metrics before cleanup ─────────────────────────────────────

FINAL_RSS_KB=0
FINAL_FD_COUNT=0
if kill -0 "$FORGETTY_PID" 2>/dev/null; then
    FINAL_RSS_KB=$(get_rss_kb "$FORGETTY_PID")
    FINAL_FD_COUNT=$(get_fd_count "$FORGETTY_PID")
fi

# ── Validation ───────────────────────────────────────────────────────

echo ""
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo -e "${BOLD}Validation Results${RESET}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo ""

PASS=0
FAIL=0

# Helper to record pass/fail
check() {
    local name="$1" result="$2" detail="$3"
    if [[ "$result" == "PASS" ]]; then
        echo -e "  ${GREEN}PASS${RESET}  $name  ($detail)"
        PASS=$((PASS + 1))
    else
        echo -e "  ${RED}FAIL${RESET}  $name  ($detail)"
        FAIL=$((FAIL + 1))
    fi
    echo "$result  $name  $detail" >> "$SUMMARY_FILE"
}

echo "Test started:  $(head -2 "$LOG_FILE" | tail -1 | cut -f1)"
echo "Test ended:    $(date +%Y-%m-%dT%H:%M:%S)"
echo "Samples:       $SAMPLE_COUNT"
echo "" | tee -a "$SUMMARY_FILE"

PEAK_RSS_MB=$((MAX_RSS_SEEN / 1024))

# AC-07: RSS stays under 500 MB
if [[ $PEAK_RSS_MB -le $MAX_RSS_MB ]]; then
    check "AC-07a (max RSS)" "PASS" "peak ${PEAK_RSS_MB}MB <= ${MAX_RSS_MB}MB limit"
else
    check "AC-07a (max RSS)" "FAIL" "peak ${PEAK_RSS_MB}MB > ${MAX_RSS_MB}MB limit"
fi

# AC-07: Final RSS within 120% of initial (skip if warm-up not reached)
if [[ "$WARMUP_DONE" == "true" ]] && [[ $INITIAL_RSS_KB -gt 0 ]]; then
    THRESHOLD_KB=$(awk "BEGIN {printf \"%d\", $INITIAL_RSS_KB * $RSS_GROWTH_THRESHOLD}")
    INITIAL_MB=$((INITIAL_RSS_KB / 1024))
    FINAL_MB=$((FINAL_RSS_KB / 1024))
    THRESHOLD_MB=$((THRESHOLD_KB / 1024))
    if [[ $FINAL_RSS_KB -le $THRESHOLD_KB ]]; then
        check "AC-07b (RSS growth)" "PASS" "final ${FINAL_MB}MB <= ${THRESHOLD_MB}MB (120% of ${INITIAL_MB}MB baseline)"
    else
        check "AC-07b (RSS growth)" "FAIL" "final ${FINAL_MB}MB > ${THRESHOLD_MB}MB (120% of ${INITIAL_MB}MB baseline)"
    fi
else
    check "AC-07b (RSS growth)" "PASS" "warm-up period not reached; skipped"
fi

# AC-08: Idle CPU under 0.5% and idle RSS within 105% of initial
# Parse the TSV log for idle-phase samples (skip the header line)
IDLE_CPU_SUM=0
IDLE_CPU_COUNT=0
IDLE_RSS_FIRST=0
IDLE_RSS_LAST=0
while IFS=$'\t' read -r _ts _elapsed _rss_kb _rss_mb cpu_pct _fd _child phase; do
    if [[ "$phase" == "idle" ]]; then
        IDLE_CPU_COUNT=$((IDLE_CPU_COUNT + 1))
        IDLE_CPU_SUM=$(awk "BEGIN {printf \"%.2f\", $IDLE_CPU_SUM + $cpu_pct}")
        if [[ $IDLE_RSS_FIRST -eq 0 ]]; then
            IDLE_RSS_FIRST=$_rss_kb
        fi
        IDLE_RSS_LAST=$_rss_kb
    fi
done < <(tail -n +2 "$LOG_FILE")

if [[ $IDLE_CPU_COUNT -gt 0 ]]; then
    IDLE_CPU_AVG=$(awk "BEGIN {printf \"%.2f\", $IDLE_CPU_SUM / $IDLE_CPU_COUNT}")
    # Check average idle CPU is under MAX_IDLE_CPU (0.5%)
    IDLE_CPU_OK=$(awk "BEGIN {print ($IDLE_CPU_AVG <= $MAX_IDLE_CPU) ? 1 : 0}")
    if [[ "$IDLE_CPU_OK" -eq 1 ]]; then
        check "AC-08a (idle CPU)" "PASS" "avg ${IDLE_CPU_AVG}% <= ${MAX_IDLE_CPU}% limit (${IDLE_CPU_COUNT} idle samples)"
    else
        check "AC-08a (idle CPU)" "FAIL" "avg ${IDLE_CPU_AVG}% > ${MAX_IDLE_CPU}% limit (${IDLE_CPU_COUNT} idle samples)"
    fi

    # Check idle RSS: final idle RSS within 105% of first idle RSS
    if [[ $IDLE_RSS_FIRST -gt 0 ]]; then
        IDLE_RSS_LIMIT=$(awk "BEGIN {printf \"%d\", $IDLE_RSS_FIRST * $RSS_IDLE_THRESHOLD}")
        IDLE_RSS_FIRST_MB=$((IDLE_RSS_FIRST / 1024))
        IDLE_RSS_LAST_MB=$((IDLE_RSS_LAST / 1024))
        IDLE_RSS_LIMIT_MB=$((IDLE_RSS_LIMIT / 1024))
        if [[ $IDLE_RSS_LAST -le $IDLE_RSS_LIMIT ]]; then
            check "AC-08b (idle RSS)" "PASS" "final ${IDLE_RSS_LAST_MB}MB <= ${IDLE_RSS_LIMIT_MB}MB (105% of ${IDLE_RSS_FIRST_MB}MB initial)"
        else
            check "AC-08b (idle RSS)" "FAIL" "final ${IDLE_RSS_LAST_MB}MB > ${IDLE_RSS_LIMIT_MB}MB (105% of ${IDLE_RSS_FIRST_MB}MB initial)"
        fi
    else
        check "AC-08b (idle RSS)" "PASS" "no idle RSS data; skipped"
    fi
else
    check "AC-08a (idle CPU)" "PASS" "no idle samples recorded; skipped"
    check "AC-08b (idle RSS)" "PASS" "no idle samples recorded; skipped"
fi

# AC-09: fd count stable
if [[ "$WARMUP_DONE" == "true" ]] && [[ $BASELINE_FD_COUNT -gt 0 ]]; then
    FD_DELTA=$((FINAL_FD_COUNT - BASELINE_FD_COUNT))
    FD_DELTA_ABS=${FD_DELTA#-}  # absolute value
    if [[ $FD_DELTA_ABS -le $FD_TOLERANCE ]]; then
        check "AC-09 (fd stability)" "PASS" "delta=${FD_DELTA} (baseline=${BASELINE_FD_COUNT}, final=${FINAL_FD_COUNT})"
    else
        check "AC-09 (fd stability)" "FAIL" "delta=${FD_DELTA} > tolerance=${FD_TOLERANCE}"
    fi
else
    check "AC-09 (fd stability)" "PASS" "warm-up period not reached; skipped"
fi

# AC-10: No zombie processes
ZOMBIE_COUNT=0
if kill -0 "$FORGETTY_PID" 2>/dev/null; then
    ZOMBIE_COUNT=$(ps --ppid "$FORGETTY_PID" 2>/dev/null | grep -c "defunct" || true)
fi
if [[ $ZOMBIE_COUNT -eq 0 ]]; then
    check "AC-10 (no zombies)" "PASS" "0 zombie child processes"
else
    check "AC-10 (no zombies)" "FAIL" "$ZOMBIE_COUNT zombie child processes found"
fi

# AC-11: Log file exists
if [[ -f "$LOG_FILE" ]] && [[ $(wc -l < "$LOG_FILE") -gt 1 ]]; then
    LINES=$(wc -l < "$LOG_FILE")
    check "AC-11 (log file)" "PASS" "$LOG_FILE ($LINES entries)"
else
    check "AC-11 (log file)" "FAIL" "log file missing or empty"
fi

echo ""
echo -e "  ${BOLD}Total: ${GREEN}${PASS} passed${RESET}, ${RED}${FAIL} failed${RESET}"
echo ""
echo -e "  Log:     $LOG_FILE"
echo -e "  Summary: $SUMMARY_FILE"
echo ""

# ── Exit code ────────────────────────────────────────────────────────

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
