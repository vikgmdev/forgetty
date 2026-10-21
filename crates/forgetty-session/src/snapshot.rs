//! Snapshot catch-up (AD-023): the current screen of a pane as VT bytes.
//!
//! A slow client does not replay the bytes it missed. The server builds a
//! `forgetty-vt` terminal from the pane's byte-log ring on the cold path (as
//! `capture` does, AD-019), formats its state with the libghostty formatter,
//! and sends that in place of the missed bytes. Nothing is kept between calls
//! and nothing runs on the PTY drain path.
//!
//! What the snapshot carries: the visible screen of the active screen (main or
//! alternate) with styles and hyperlinks, the cursor,
//! the modes that differ from their defaults (alternate screen, bracketed
//! paste, mouse modes, cursor visibility, keypad, focus reporting), scrolling
//! region, tab stops, pwd, keyboard modes, charsets, and the title (OSC 2).
//! The 256 colour palette is not sent: it belongs to the viewer's theme, and
//! cells keep palette indexes.
//!
//! What it does not carry: scrollback (fetched on demand), the main screen
//! while the alternate one is active, default foreground/background/cursor
//! colours set by OSC 10/11/12, and state set before the oldest byte in the
//! ring.

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

/// Feed size per `vt_write` call.
const FEED_CHUNK: usize = 64 * 1024;

/// Scrollback of the throwaway terminal: none. The formatter prints scrollback
/// lines before the screen, and with blank rows at the bottom of the screen that
/// would push the screen content up by the number of blank rows.
const SCRATCH_SCROLLBACK: usize = 0;

/// Build the snapshot of `replay` (raw PTY bytes) on a `rows` x `cols` screen.
pub fn build_snapshot(replay: &[u8], rows: u16, cols: u16) -> Vec<u8> {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let mut term =
        Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], SCRATCH_SCROLLBACK);
    for chunk in replay.chunks(FEED_CHUNK) {
        term.feed(chunk);
    }
    let mut out = term.format_vt();
    // The formatter sets tab stops after it places the cursor, and setting a
    // tab stop moves the cursor. Place it again as the last screen operation.
    let _ = term.screen(); // refreshes the render state `cursor` reads
    let (row, col) = term.cursor();
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    let title = term.title();
    if !title.is_empty() {
        out.extend_from_slice(b"\x1b]2;");
        // A title cannot carry control bytes into the OSC.
        out.extend(title.bytes().filter(|b| *b >= 0x20 && *b != 0x7f));
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &Terminal, rows: usize) -> Vec<String> {
        (0..rows)
            .map(|r| t.screen().row(r).iter().map(|c| c.grapheme.as_str()).collect::<String>())
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    #[test]
    fn a_snapshot_rebuilds_the_screen_modes_and_title() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"$ ls\r\nfile\r\n\x1b[?2004h\x1b[?1049h\x1b[?1000h\x1b[?25l");
        raw.extend_from_slice(b"\x1b]2;editor\x07\x1b[2J\x1b[H\x1b[1mhello\x1b[0m\r\n~\x1b[5;3Hx");
        let snap = build_snapshot(&raw, 8, 30);
        let pal = [ANSI_PALETTE_BLACK; 16];
        let mut orig = Terminal::new(8, 30, pal);
        orig.feed(&raw);
        let mut copy = Terminal::new(8, 30, pal);
        copy.feed(&snap);
        assert_eq!(text(&orig, 8), text(&copy, 8));
        assert!(copy.is_alternate_screen());
        assert!(copy.is_mouse_tracking());
        assert!(copy.dec_mode(2004));
        assert!(!copy.cursor_visible());
        assert_eq!(copy.cursor(), orig.cursor());
        assert_eq!(copy.title(), "editor");
    }

    /// A snapshot cut anywhere in a stream of lines, followed by the rest of
    /// the stream, gives the screen the whole stream gives.
    #[test]
    fn a_snapshot_then_the_rest_equals_the_whole_stream() {
        let mut stream = Vec::new();
        for _ in 0..200 {
            stream.extend_from_slice(b"0123456789abcdef\r\n");
        }
        let pal = [ANSI_PALETTE_BLACK; 16];
        let mut whole = Terminal::new(24, 80, pal);
        whole.feed(&stream);
        let want = text(&whole, 24);
        for cut in (0..stream.len()).step_by(7).chain([
            18 * 23,
            18 * 24,
            18 * 25,
            18 * 100,
            18 * 200 - 1,
            18 * 200,
        ]) {
            let snap = build_snapshot(&stream[..cut], 24, 80);
            let mut t = Terminal::new(24, 80, pal);
            t.feed(&snap);
            t.feed(&stream[cut..]);
            assert_eq!(text(&t, 24), want, "cut at {cut}");
            assert_eq!(t.cursor(), whole.cursor(), "cursor, cut at {cut}");
        }
    }

    #[test]
    fn build_time_from_an_8_mib_ring() {
        // Mixed output: styled lines, cursor moves, a full redraw at the end.
        let mut raw = Vec::with_capacity(9 << 20);
        let mut i = 0u32;
        while raw.len() < 8 << 20 {
            raw.extend_from_slice(
                format!(
                    "\x1b[3{}mline {i} the quick brown fox jumps over the lazy dog\x1b[0m\r\n",
                    i % 8
                )
                .as_bytes(),
            );
            i += 1;
        }
        let t0 = std::time::Instant::now();
        let snap = build_snapshot(&raw, 50, 200);
        let took = t0.elapsed();
        println!("snapshot of {} MiB ring: {} bytes in {took:?}", raw.len() >> 20, snap.len());
        assert!(!snap.is_empty());
        // Debug builds are slow: the release gate checks the real bound.
        if !cfg!(debug_assertions) {
            assert!(took < std::time::Duration::from_millis(500), "built in {took:?}");
        }
    }
}
