//! Wide-cell flags, grapheme clusters and viewport scrolling through libghostty-vt.

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

fn term(rows: usize, cols: usize) -> Terminal {
    Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], 8 * 1024 * 1024)
}

#[test]
fn wide_cells_are_flagged() {
    let mut t = term(5, 40);
    t.feed("A日本😀B e\u{301} 👨\u{200d}👩\u{200d}👧\r\n".as_bytes());
    let s = t.screen();
    let c = s.row(0);
    assert_eq!(c[0].grapheme, "A");
    assert!(!c[0].attrs.wide && !c[0].attrs.spacer);
    assert_eq!(c[1].grapheme, "日");
    assert!(c[1].attrs.wide);
    assert!(c[2].attrs.spacer);
    assert_eq!(c[3].grapheme, "本");
    assert!(c[3].attrs.wide && c[4].attrs.spacer);
    assert_eq!(c[5].grapheme, "😀");
    assert!(c[5].attrs.wide && c[6].attrs.spacer);
    assert_eq!(c[7].grapheme, "B");
    assert!(!c[7].attrs.wide);
    eprintln!("combining: {}", c.iter().any(|x| x.grapheme == "e\u{301}"));
    eprintln!("zwj: {}", c.iter().any(|x| x.grapheme.chars().count() >= 5));
}

#[test]
fn scroll_up_1000_rows_reads_exact_line() {
    let mut t = term(24, 80);
    let mut text = String::new();
    for i in 0..5000 {
        text.push_str(&format!("{i:07} line\r\n"));
    }
    t.feed(text.as_bytes());
    let before = t.scrollbar_state();
    let row = {
        let s = t.screen();
        (0..s.rows())
            .rev()
            .find(|&r| s.row(r).iter().map(|c| c.grapheme.as_str()).collect::<String>().starts_with("0004999"))
            .expect("last line in viewport")
    };
    t.scroll_viewport_delta(-1000);
    let line: String = t.screen().row(row).iter().map(|c| c.grapheme.as_str()).collect();
    assert!(line.starts_with("0003999"), "got {line:?}");
    assert_eq!(before.1 - t.scrollbar_state().1, 1000);
    t.scroll_viewport_bottom();
    let line: String = t.screen().row(row).iter().map(|c| c.grapheme.as_str()).collect();
    assert!(line.starts_with("0004999"));
}
