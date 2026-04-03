# Forgetty Stress Tests Reference (T-025)

Every test that must pass before a Forgetty release. Run them inside Forgetty
itself to exercise the terminal's rendering, PTY handling, and resource management.

```bash
# All tests
./tests/stress/run-stress-tests.sh

# Automated subset only (no user interaction)
./tests/stress/run-stress-tests.sh --auto-only

# Single test
./tests/stress/run-stress-tests.sh --test emoji
```

---

## High Throughput

### AC-01: `cat /dev/urandom | base64` (10 seconds)

| Field | Value |
|-------|-------|
| **Name** | `urandom` |
| **Category** | High Throughput |
| **Type** | Automated |
| **Command** | `timeout 10 bash -c 'cat /dev/urandom | base64'` |

**Expected behavior:** Output streams continuously for 10 seconds. The terminal
remains responsive throughout. After timeout kills the process, the prompt returns
immediately. RSS stays under 500MB. No crash, no freeze, no visual corruption.

**Known limitations:** If the mpsc channel has no backpressure, memory may grow
during sustained output. The scrollback buffer (10K lines) should bound this.

---

### AC-02: `yes` infinite output (10 seconds)

| Field | Value |
|-------|-------|
| **Name** | `yes` |
| **Category** | High Throughput |
| **Type** | Automated |
| **Command** | `timeout 10 yes` |

**Expected behavior:** Screen fills with `y` lines at maximum speed. Terminal
stays responsive. After timeout, prompt returns cleanly. No crash.

**Known limitations:** `yes` produces output faster than most terminals can
render. The key metric is that the terminal does not freeze or OOM.

---

### AC-03: Large file cat (100MB)

| Field | Value |
|-------|-------|
| **Name** | `largefile` |
| **Category** | High Throughput |
| **Type** | Automated |
| **Command** | `cat /tmp/forgetty-stress-100mb.txt` |

**Expected behavior:** File streams to completion in under 30 seconds. No crash,
no hang. The script generates the file automatically if missing.

**Known limitations:** First run creates the file via `dd | base64`, which takes
a few seconds. Requires ~100MB of free disk space in `/tmp`.

---

### AC-04: Ctrl+C interrupt during high output

| Field | Value |
|-------|-------|
| **Name** | `ctrlc` |
| **Category** | High Throughput |
| **Type** | Automated |
| **Command** | `yes \| cat -n` (background) + SIGINT after 2s |

**Expected behavior:** Process stops within 2 seconds of SIGINT. Terminal is
fully usable afterward. Prompt returns.

**Known limitations:** None expected.

---

## ncurses / Full-Screen Applications

### AC-05: vim opens and renders correctly

| Field | Value |
|-------|-------|
| **Name** | `vim` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `vim <tempfile>` |

**Expected behavior:** Status line renders at bottom. Line numbers visible (if
configured). Cursor positioning is correct. Insert mode works. Syntax
highlighting displays. `:wq` saves and exits cleanly. No artifacts remain after
exit.

**Known limitations:** Depends on user's vim configuration. Minimal vimrc is
fine for this test.

---

### AC-06: htop renders correctly

| Field | Value |
|-------|-------|
| **Name** | `htop` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `htop` |

**Expected behavior:** CPU/memory bars render with correct colors. Process list
scrolls. Header and footer (F-key labels) display. Column sorting (F6) works.
`q` exits cleanly with no artifacts.

**Known limitations:** Requires `htop` installed.

---

### AC-07: dialog box renders correctly

| Field | Value |
|-------|-------|
| **Name** | `dialog` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `dialog --msgbox "Hello from Forgetty" 10 40` |

**Expected behavior:** Box-drawing characters (corners, borders) render
correctly. Text is readable and centered. OK button is highlighted and
functional. Screen restores after dismiss.

**Known limitations:** Requires `dialog` installed. Box-drawing depends on the
terminal correctly reporting Unicode support.

---

### AC-08: whiptail box renders correctly

| Field | Value |
|-------|-------|
| **Name** | `whiptail` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `whiptail --msgbox "Hello from Forgetty" 10 40` |

**Expected behavior:** Same as dialog: correct box-drawing, readable text,
functional OK button, clean restore.

**Known limitations:** Requires `whiptail` installed.

---

### AC-09: tmux nested session

| Field | Value |
|-------|-------|
| **Name** | `tmux` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `tmux new-session` |

**Expected behavior:** Status bar renders at bottom with session info. Vertical
split (`Ctrl+b %`) creates a divider line. Both panes accept input. Detach
(`Ctrl+b d`) returns to parent shell cleanly. Reattach works.

**Known limitations:** Requires `tmux` installed. Nested tmux (tmux inside
Forgetty inside tmux) may cause prefix key conflicts.

---

### AC-10: screen nested session

| Field | Value |
|-------|-------|
| **Name** | `screen` |
| **Category** | ncurses |
| **Type** | Semi-automated |
| **Command** | `screen -S forgetty-test` |

**Expected behavior:** Screen launches. Window creation (`Ctrl+a c`) and
switching (`Ctrl+a 0/1`) work. Status line renders if configured. Detach
(`Ctrl+a d`) returns cleanly. No corruption.

**Known limitations:** Requires `screen` installed. Default screen may not show
a status line unless configured.

---

## Colors

### AC-11: 256-color palette test

| Field | Value |
|-------|-------|
| **Name** | `256color` |
| **Category** | Colors |
| **Type** | Semi-automated |
| **Command** | (embedded escape sequence loop) |

**Expected behavior:** All 256 indexed colors display: 16 standard ANSI colors
(0-15), 216-color RGB cube (16-231), and 24-step grayscale ramp (232-255). Each
color is visually distinct from its neighbors. No missing or duplicated entries.

**Known limitations:** Exact color appearance depends on the terminal's color
scheme for the first 16 colors. The 216 cube and grayscale should be consistent
across themes.

---

### AC-12: Truecolor (24-bit) gradient

| Field | Value |
|-------|-------|
| **Name** | `truecolor` |
| **Category** | Colors |
| **Type** | Semi-automated |
| **Command** | (embedded `\e[48;2;R;G;Bm` loop) |

**Expected behavior:** Smooth red, green, blue, and rainbow gradients with no
visible banding or stepping. Each pixel column is a slightly different shade.

**Known limitations:** Terminals that only support 256 colors will show banding.
Forgetty should support full 24-bit color.

---

## Unicode

### AC-13: Emoji rendering (including ZWJ sequences)

| Field | Value |
|-------|-------|
| **Name** | `emoji` |
| **Category** | Unicode |
| **Type** | Semi-automated |
| **Command** | (embedded printf with UTF-8 sequences) |

**Expected behavior:** Basic emoji render at double-cell width. ZWJ family
sequences render as a single combined glyph (or as individual emoji if the font
lacks the ZWJ glyph). Flag emoji display as flags. No tofu (missing glyph
squares) if the system font supports emoji.

**Known limitations:** Rendering quality depends entirely on system font
coverage (Noto Color Emoji, etc.). ZWJ sequences may show as separate emoji
on older fonts. The terminal's job is correct cell-width accounting, not font
rendering.

---

### AC-14: CJK characters

| Field | Value |
|-------|-------|
| **Name** | `cjk` |
| **Category** | Unicode |
| **Type** | Semi-automated |
| **Command** | (embedded printf with UTF-8 CJK) |

**Expected behavior:** Chinese, Japanese, and Korean characters each occupy
exactly 2 terminal cells. Grid alignment is maintained -- a 5-CJK-character
string should be the same width as 10 ASCII characters.

**Known limitations:** Requires a CJK-capable font (Noto CJK, WenQuanYi, etc.).
Without the font, characters appear as tofu but should still take 2 cells.

---

### AC-15: Combining marks and precomposed equivalents

| Field | Value |
|-------|-------|
| **Name** | `combining` |
| **Category** | Unicode |
| **Type** | Semi-automated |
| **Command** | (embedded printf with combining sequences) |

**Expected behavior:** Precomposed characters (e.g., U+00E9 e-acute) and their
decomposed equivalents (e.g., U+0065 + U+0301) render identically. Each
occupies exactly 1 cell. Multiple combining marks stack correctly.

**Known limitations:** Some fonts render combining mark stacks poorly. The
terminal's responsibility is correct cell-width calculation.

---

### AC-16: Zero-width joiners and invisible characters

| Field | Value |
|-------|-------|
| **Name** | `zwj` |
| **Category** | Unicode |
| **Type** | Semi-automated |
| **Command** | (embedded printf with ZWSP, soft hyphen, word joiner) |

**Expected behavior:** Zero-width space (U+200B), word joiner (U+2060), and
soft hyphen (U+00AD) produce no visible glyph. Grid alignment is not disturbed.
Text with invisible characters between letters looks identical to text without.

**Known limitations:** Some terminal emulators show ZWSP as a thin space. The
correct behavior is zero width.

---

### AC-17: RTL text (Arabic/Hebrew)

| Field | Value |
|-------|-------|
| **Name** | `rtl` |
| **Category** | Unicode |
| **Type** | Semi-automated |
| **Command** | (embedded printf with Arabic/Hebrew UTF-8) |

**Expected behavior:** Arabic and Hebrew characters render as readable glyphs.
Terminal displays them left-to-right (terminals do not implement BiDi). No
crash, no grid corruption. Mixed LTR/RTL text on the same line renders without
overlap.

**Known limitations:** True RTL display requires BiDi support, which virtually
no terminal emulator implements. LTR rendering of RTL scripts is the expected
behavior for terminals.

---

## Long Lines

### AC-18: 10K character single line

| Field | Value |
|-------|-------|
| **Name** | `longline10k` |
| **Category** | Long Lines |
| **Type** | Automated |
| **Command** | `printf '%0.sA' $(seq 1 10000)` |

**Expected behavior:** 10,000 'A' characters print and wrap naturally across
multiple terminal rows. Scrolling works. Terminal remains responsive afterward.

**Known limitations:** None expected. This is well within normal terminal
capacity.

---

### AC-19: 100K character single line

| Field | Value |
|-------|-------|
| **Name** | `longline100k` |
| **Category** | Long Lines |
| **Type** | Automated |
| **Command** | `printf '%0.sB' $(seq 1 100000)` |

**Expected behavior:** 100,000 characters wrap and display. Terminal may be
briefly sluggish during rendering but recovers. No crash.

**Known limitations:** Some terminals struggle with extremely long single lines.
A brief pause during rendering is acceptable; a hang or crash is not.

---

## Terminal Protocol Features

### AC-20: Bracketed paste mode

| Field | Value |
|-------|-------|
| **Name** | `bracketed_paste` |
| **Category** | Protocol |
| **Type** | Semi-automated |
| **Command** | `printf '\033[?2004h'` (enable) / `printf '\033[?2004l'` (disable) |

**Expected behavior:** When bracketed paste is enabled, pasting text from the
clipboard works normally. The terminal wraps pasted text in `ESC[200~` ...
`ESC[201~` sequences (verifiable with `cat -v`). The script disables bracketed
paste after the test.

**Known limitations:** Bracketed paste support may be a known limitation in
early builds. The test documents current behavior.

---

### AC-21: OSC 8 hyperlinks

| Field | Value |
|-------|-------|
| **Name** | `osc8_hyperlinks` |
| **Category** | Protocol |
| **Type** | Semi-automated |
| **Command** | `printf '\033]8;;URL\033\\text\033]8;;\033\\'` |

**Expected behavior:** Link text renders without corruption or escape sequence
garbage. If hyperlinks are supported, text may be underlined and clickable. If
not supported, text appears as plain text.

**Known limitations:** OSC 8 support is not yet implemented in Forgetty. The
test verifies that unknown sequences are handled gracefully (no rendering
corruption).

---

## Concurrent Stress

### AC-22: Rapid resize during output

| Field | Value |
|-------|-------|
| **Name** | `resize` |
| **Category** | Concurrent |
| **Type** | Manual |
| **Command** | `yes \| cat -n` while resizing window |

**Expected behavior:** Terminal does not crash or deadlock during rapid resize.
Output continues streaming. After resize settles, text reflows correctly to the
new window dimensions. Prompt returns after Ctrl+C.

**Known limitations:** Resize during output may trigger `try_borrow_mut()`
contention in the Rust core. Performance may degrade briefly but must not
deadlock.

---

### AC-23: Multiple splits under load

| Field | Value |
|-------|-------|
| **Name** | `multisplit` |
| **Category** | Concurrent |
| **Type** | Manual |
| **Command** | 4 splits, each: `cat /dev/urandom \| base64` |

**Expected behavior:** All 4 panes stream concurrently. No cross-pane rendering
corruption. Each pane's output stays within its bounds. Ctrl+C in each pane
stops that pane's output. No crash.

**Known limitations:** 4 concurrent high-throughput streams is a significant
load. Frame rate may drop but the terminal must remain functional.

---

### AC-24: Split creation/destruction during output

| Field | Value |
|-------|-------|
| **Name** | `splitdestroy` |
| **Category** | Concurrent |
| **Type** | Manual |
| **Command** | `yes` in one pane, create/destroy another |

**Expected behavior:** Creating a new split while output streams in an existing
pane does not crash. Closing the new split returns focus to the original pane,
which continues streaming. No corruption.

**Known limitations:** Split lifecycle events during active PTY output may
stress the event loop. Must not deadlock.

---

## Memory

### AC-25: Memory stability under sustained load (60 seconds)

| Field | Value |
|-------|-------|
| **Name** | `memory` |
| **Category** | Memory |
| **Type** | Automated |
| **Command** | `cat /dev/urandom \| base64` for 60s, measure RSS delta |

**Expected behavior:** RSS of the terminal process does not grow by more than
100MB over 60 seconds of sustained high-throughput output. Scrollback buffer
(bounded at 10K lines) prevents unbounded memory growth.

**Known limitations:** RSS measurement uses `ps -o rss= -p $PPID`, which
measures the parent process (assumed to be the terminal). If the script is run
inside tmux or another wrapper, `$PPID` may point to the wrong process. For
accurate results, run directly inside Forgetty.

---

## Prerequisites

Install all optional dependencies for full coverage:

```bash
sudo apt install dialog whiptail tmux screen vim htop
```

Tests for missing tools are automatically skipped with a `SKIP` result.

## Running in CI

Use `--auto-only` for headless/CI environments. This runs only the automated
tests (AC-01 through AC-04, AC-18, AC-19, AC-25) that need no human
interaction. Semi-automated and manual tests are skipped.

```bash
./tests/stress/run-stress-tests.sh --auto-only
```

Exit code is 0 if all executed tests pass, 1 if any test fails.
