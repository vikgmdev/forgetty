//! `Terminal::row_text`: rows of the whole screen as text, without moving the viewport.

use forgetty_vt::{RowText, Terminal, ANSI_PALETTE_BLACK};

fn term(rows: usize, cols: usize) -> Terminal {
    Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], 8 * 1024 * 1024)
}

fn screen_line(t: &mut Terminal, r: usize) -> String {
    t.screen().row(r).iter().filter(|c| !c.attrs.spacer).map(|c| c.grapheme.as_str()).collect()
}

#[test]
fn matches_the_viewport_rows_at_every_scroll_offset() {
    let mut t = term(10, 30);
    let mut text = String::new();
    for i in 0..200 {
        text.push_str(&format!("\x1b[3{}mline {i} A日本B e\u{301}\x1b[0m\r\n", i % 8));
    }
    t.feed(text.as_bytes());
    let mut rt = RowText::default();
    for back in [0isize, -1, -7, -50, -150] {
        t.scroll_viewport_bottom();
        t.scroll_viewport_delta(back);
        let off = t.scrollbar_state().1 as usize;
        for r in 0..10 {
            let want = screen_line(&mut t, r);
            assert!(t.row_text(off + r, &mut rt), "row {}", off + r);
            assert_eq!(rt.text, want, "offset {off} row {r}");
            assert_eq!(rt.cols.len(), rt.text.chars().count() + 1);
            assert_eq!(*rt.cols.last().unwrap(), 30);
        }
    }
}

#[test]
fn columns_follow_wide_and_combining_chars() {
    let mut t = term(4, 20);
    t.feed("a日b\u{301}c".as_bytes());
    let mut rt = RowText::default();
    let top = t.scrollbar_state().1 as usize;
    assert!(t.row_text(top, &mut rt));
    // a at 0, the wide glyph at 1 (its tail at 2 adds nothing), b and its accent at 3, c at 4.
    let chars: Vec<char> = rt.text.chars().collect();
    assert_eq!(chars[0], 'a');
    assert_eq!(chars[1], '日');
    assert_eq!(&rt.cols[..5], &[0, 1, 3, 3, 4]);
    assert_eq!(chars[2], 'b');
    assert_eq!(chars[3], '\u{301}');
    assert_eq!(chars[4], 'c');
}

#[test]
fn soft_wrap_is_reported_and_rows_past_the_end_are_not() {
    let mut t = term(4, 10);
    t.feed(b"0123456789abcdef\r\nnext");
    let mut rt = RowText::default();
    let top = t.scrollbar_state().1 as usize;
    assert!(t.row_text(top, &mut rt));
    assert!(rt.wrapped);
    assert!(rt.text.starts_with("0123456789"));
    assert!(t.row_text(top + 1, &mut rt));
    assert!(!rt.wrapped);
    assert!(rt.text.starts_with("abcdef"));
    assert!(t.row_text(top + 2, &mut rt));
    assert!(rt.text.starts_with("next"));
    let total = t.scrollbar_state().0 as usize;
    assert!(!t.row_text(total + 5, &mut rt));
}

#[test]
fn reading_rows_leaves_the_viewport_alone() {
    let mut t = term(5, 20);
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!("{i}\r\n"));
    }
    t.feed(text.as_bytes());
    t.scroll_viewport_delta(-100);
    let before = t.scrollbar_state();
    let mut rt = RowText::default();
    for r in 0..before.0 as usize {
        assert!(t.row_text(r, &mut rt));
    }
    assert_eq!(t.scrollbar_state(), before);
    // The oldest retained row and the newest are both reachable.
    assert!(t.row_text(0, &mut rt));
    assert!(t.row_text(before.0 as usize - 1, &mut rt));
}

/// Not a gate: prints how long a full scan of a big scrollback takes.
#[test]
fn scan_speed_of_a_big_scrollback() {
    let mut t = Terminal::with_scrollback(40, 120, [ANSI_PALETTE_BLACK; 16], 64 * 1024 * 1024);
    let mut text = String::new();
    for i in 0..60_000 {
        text.push_str(&format!("{i:07} the quick brown fox jumps over the lazy dog 0123456789\r\n"));
    }
    t.feed(text.as_bytes());
    let total = t.scrollbar_state().0 as usize;
    let mut rt = RowText::default();
    let t0 = std::time::Instant::now();
    let mut hits = 0;
    for r in 0..total {
        assert!(t.row_text(r, &mut rt));
        if rt.text.contains("lazy") {
            hits += 1;
        }
    }
    eprintln!("scanned {total} rows x 120 cols in {:?} ({hits} hits)", t0.elapsed());
    assert!(hits > 40_000);
}

#[test]
fn rows_stay_inside_the_real_width_after_a_resize() {
    let mut t = term(6, 40);
    t.feed(b"first line of text\r\nsecond\r\n");
    let mut rt = RowText::default();
    for (rows, cols) in [(6, 80), (8, 20), (3, 7), (6, 200), (6, 40)] {
        assert!(t.resize(rows, cols));
        for r in 0..t.scrollbar_state().0 as usize {
            assert!(t.row_text(r, &mut rt));
            assert_eq!(*rt.cols.last().unwrap() as usize, cols, "width after resize to {cols}");
            assert!(rt.cols[..rt.cols.len() - 1].iter().all(|&c| (c as usize) < cols));
        }
    }
}

/// DECCOLM (mode 3, allowed by mode 40) changes the width inside libghostty without a call to
/// `Terminal::resize`: `row_text` must follow the real width, not the one Rust last set.
#[test]
fn rows_stay_inside_the_real_width_after_deccolm() {
    let mut t = term(5, 80);
    t.feed(b"\x1b[?40h\x1b[?3hhello");
    let mut rt = RowText::default();
    let mut widths = Vec::new();
    for r in 0..t.scrollbar_state().0 as usize {
        assert!(t.row_text(r, &mut rt));
        widths.push(*rt.cols.last().unwrap() as usize);
        assert!(rt.cols[..rt.cols.len() - 1].iter().all(|&c| (c as usize) < widths[0]));
    }
    assert!(widths.iter().all(|&w| w == widths[0]));
    assert_eq!(widths[0], 132, "libghostty switched to 132 columns");
    t.feed(b"\x1b[?3l");
    assert!(t.row_text(t.scrollbar_state().1 as usize, &mut rt));
    assert_eq!(*rt.cols.last().unwrap(), 80);
}
