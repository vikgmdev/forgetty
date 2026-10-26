//! Code block detection and content extraction (port of `forgetty-gtk/src/code_block.rs`).
//!
//! Two detection strategies:
//! 1. **Border detection**: rectangular regions bounded by box-drawing characters
//!    (U+2500-U+257F), which catches the bordered code blocks of Claude Code.
//! 2. **Background color detection**: rectangles with one non-default background color, which
//!    catches fenced blocks and any TUI that highlights code with a background.
//!
//! The detector reads the screen through the `Grid` trait, so it has no terminal types and the
//! tests below run on any OS. Runs when the pointer moves over a pane whose content changed:
//! O(rows * cols) for the visible screen, never per byte.

use crate::copytext;

/// A read-only view of the visible screen.
pub trait Grid {
    fn rows(&self) -> usize;
    fn cols(&self) -> usize;
    /// The grapheme of a cell. Empty for an empty cell.
    fn text(&self, row: usize, col: usize) -> &str;
    /// The background of a cell as `0xRRGGBB`, or `None` for the default background.
    fn bg(&self, row: usize, col: usize) -> Option<u32>;
}

/// A detected code block in the terminal viewport.
///
/// Coordinates are screen-relative (viewport rows and columns), inclusive on
/// all four sides. The border cells themselves are included in the rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub top_row: usize,
    pub bottom_row: usize,
    pub left_col: usize,
    pub right_col: usize,
    /// `true` for border-detected blocks (content excludes border row/col).
    /// `false` for color-detected blocks (content is the full rectangle).
    pub has_border: bool,
}

impl CodeBlock {
    /// Check whether a screen cell (row, col) falls within this block's
    /// bounding rectangle (border cells included).
    pub fn contains(&self, row: usize, col: usize) -> bool {
        row >= self.top_row
            && row <= self.bottom_row
            && col >= self.left_col
            && col <= self.right_col
    }
}

/// Returns `true` if the character falls in the box-drawing range U+2500-U+257F.
fn is_box_drawing(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// Returns `true` if the character is a horizontal border character.
fn is_horizontal_border(c: char) -> bool {
    matches!(
        c,
        '\u{2500}' // ─ thin horizontal
        | '\u{2501}' // ━ thick horizontal
        | '\u{2550}' // ═ double horizontal
    )
}

/// Returns `true` if the character is a vertical border character.
fn is_vertical_border(c: char) -> bool {
    matches!(
        c,
        '\u{2502}' // │ thin vertical
        | '\u{2503}' // ┃ thick vertical
        | '\u{2551}' // ║ double vertical
    )
}

/// Returns `true` if the character is a top-left corner.
fn is_top_left_corner(c: char) -> bool {
    matches!(
        c,
        '\u{250C}' // ┌ thin
        | '\u{250D}' // ┍
        | '\u{250E}' // ┎
        | '\u{250F}' // ┏ thick
        | '\u{256D}' // ╭ rounded
        | '\u{2552}' // ╒ double-horizontal
        | '\u{2553}' // ╓ double-vertical
        | '\u{2554}' // ╔ double
    )
}

/// Returns `true` if the character is a top-right corner.
fn is_top_right_corner(c: char) -> bool {
    matches!(
        c,
        '\u{2510}' // ┐ thin
        | '\u{2511}' // ┑
        | '\u{2512}' // ┒
        | '\u{2513}' // ┓ thick
        | '\u{256E}' // ╮ rounded
        | '\u{2555}' // ╕ double-horizontal
        | '\u{2556}' // ╖ double-vertical
        | '\u{2557}' // ╗ double
    )
}

/// Returns `true` if the character is a bottom-left corner.
fn is_bottom_left_corner(c: char) -> bool {
    matches!(
        c,
        '\u{2514}' // └ thin
        | '\u{2515}' // ┕
        | '\u{2516}' // ┖
        | '\u{2517}' // ┗ thick
        | '\u{2570}' // ╰ rounded
        | '\u{2558}' // ╘ double-horizontal
        | '\u{2559}' // ╙ double-vertical
        | '\u{255A}' // ╚ double
    )
}

/// Returns `true` if the character is a bottom-right corner.
fn is_bottom_right_corner(c: char) -> bool {
    matches!(
        c,
        '\u{2518}' // ┘ thin
        | '\u{2519}' // ┙
        | '\u{251A}' // ┚
        | '\u{251B}' // ┛ thick
        | '\u{256F}' // ╯ rounded
        | '\u{255B}' // ╛ double-horizontal
        | '\u{255C}' // ╜ double-vertical
        | '\u{255D}' // ╝ double
    )
}

/// Get the first character of a cell's grapheme, or '\0' if empty.
fn first_char(screen: &impl Grid, row: usize, col: usize) -> char {
    if row >= screen.rows() || col >= screen.cols() {
        return '\0';
    }
    screen.text(row, col).chars().next().unwrap_or('\0')
}

/// Scan the visible viewport for code blocks bounded by box-drawing characters.
///
/// Detection algorithm:
/// 1. Scan every cell for top-left corner characters.
/// 2. For each candidate, scan right for horizontal border to a top-right corner.
/// 3. Scan downward checking vertical borders on both sides.
/// 4. When a bottom-left + bottom-right corner pair is found, validate size.
/// 5. Discard nested blocks (outer wins).
///
/// Runs at most once per screen generation change. O(rows * cols) worst case.
pub fn detect_code_blocks(screen: &impl Grid) -> Vec<CodeBlock> {
    let num_rows = screen.rows();
    let num_cols = screen.cols();
    let mut blocks: Vec<CodeBlock> = Vec::new();

    for row in 0..num_rows {
        for col in 0..num_cols {
            let c = first_char(screen, row, col);
            if !is_top_left_corner(c) {
                continue;
            }

            // Found a candidate top-left corner at (row, col).
            // Scan right along the same row for horizontal border characters
            // until we find a top-right corner.
            let mut right_col = None;
            for rc in (col + 1)..num_cols {
                let rc_char = first_char(screen, row, rc);
                if is_top_right_corner(rc_char) {
                    right_col = Some(rc);
                    break;
                }
                if !is_horizontal_border(rc_char) && !is_box_drawing(rc_char) {
                    // Allow any box-drawing character in the top border (e.g., T-junctions)
                    // but break on non-box-drawing characters.
                    break;
                }
            }

            let right_col = match right_col {
                Some(rc) => rc,
                None => continue,
            };

            // Minimum width check: 5 cols (left border + 3 content + right border)
            if right_col - col < 4 {
                continue;
            }

            // Scan downward from the top-left corner looking for vertical borders
            // on both sides, then a bottom-left + bottom-right corner.
            let mut bottom_row = None;
            for br in (row + 1)..num_rows {
                let left_char = first_char(screen, br, col);
                let right_char = first_char(screen, br, right_col);

                if is_bottom_left_corner(left_char) && is_bottom_right_corner(right_char) {
                    // Validate that the bottom border between the corners is
                    // horizontal border characters.
                    let mut valid_bottom = true;
                    for bc in (col + 1)..right_col {
                        let bc_char = first_char(screen, br, bc);
                        if !is_horizontal_border(bc_char) && !is_box_drawing(bc_char) {
                            valid_bottom = false;
                            break;
                        }
                    }
                    if valid_bottom {
                        bottom_row = Some(br);
                    }
                    break;
                }

                // Content rows must have vertical borders on both sides
                if !is_vertical_border(left_char) && !is_box_drawing(left_char) {
                    break;
                }
                if !is_vertical_border(right_char) && !is_box_drawing(right_char) {
                    break;
                }
            }

            let bottom_row = match bottom_row {
                Some(br) => br,
                None => continue,
            };

            // Minimum height check: 3 rows (top border + 1 content + bottom border)
            if bottom_row - row < 2 {
                continue;
            }

            let candidate =
                CodeBlock { top_row: row, bottom_row, left_col: col, right_col, has_border: true };

            // Nesting dedup: discard if fully contained within an already-detected block.
            let is_nested = blocks.iter().any(|existing| {
                candidate.top_row >= existing.top_row
                    && candidate.bottom_row <= existing.bottom_row
                    && candidate.left_col >= existing.left_col
                    && candidate.right_col <= existing.right_col
            });

            if !is_nested {
                blocks.push(candidate);
            }
        }
    }

    // --- Phase 2: Background color detection ---
    // Scan for rectangular regions with uniform non-default background color.
    detect_color_blocks(screen, &mut blocks);

    blocks
}

/// Detect code blocks by background color.
///
/// Scans each row for contiguous spans of cells sharing the same non-default
/// background color. Groups consecutive rows with matching spans (same bg color,
/// same column boundaries) into rectangular blocks. Filters by minimum size
/// and deduplicates against already-detected border blocks.
fn detect_color_blocks(screen: &impl Grid, blocks: &mut Vec<CodeBlock>) {
    let num_rows = screen.rows();
    let num_cols = screen.cols();

    // For each row, find the dominant non-default bg span (if any).
    // A "span" is (left_col, right_col, bg color).
    #[derive(Clone, PartialEq)]
    struct BgSpan {
        left: usize,
        right: usize,
        color: u32,
    }

    let mut row_spans: Vec<Option<BgSpan>> = Vec::with_capacity(num_rows);

    for row in 0..num_rows {
        // Find the longest contiguous run of cells with the same non-default bg.
        let mut best_span: Option<BgSpan> = None;
        let mut cur_start = 0usize;
        let mut cur_color: Option<u32> = None;
        let mut cur_len = 0usize;

        for col in 0..num_cols {
            let bg = screen.bg(row, col);
            if bg.is_some() && bg == cur_color {
                cur_len += 1;
            } else {
                // Save previous run if it's the longest
                if let Some(color) = cur_color {
                    if cur_len >= 10 && best_span.as_ref().is_none_or(|s| cur_len > s.right - s.left + 1) {
                        best_span = Some(BgSpan { left: cur_start, right: cur_start + cur_len - 1, color });
                    }
                }
                if bg.is_some() {
                    cur_start = col;
                    cur_color = bg;
                    cur_len = 1;
                } else {
                    cur_color = None;
                    cur_len = 0;
                }
            }
        }
        // Check final run
        if let Some(color) = cur_color {
            if cur_len >= 10 && best_span.as_ref().is_none_or(|s| cur_len > s.right - s.left + 1) {
                best_span = Some(BgSpan { left: cur_start, right: cur_start + cur_len - 1, color });
            }
        }

        row_spans.push(best_span);
    }

    // Group consecutive rows with matching spans into blocks.
    let mut row = 0;
    while row < num_rows {
        let Some(ref span) = row_spans[row] else {
            row += 1;
            continue;
        };

        let block_left = span.left;
        let block_right = span.right;
        let block_color = span.color;
        let block_top = row;
        let mut block_bottom = row;

        // Extend downward while consecutive rows have a matching span
        for (next_row, next_span_opt) in row_spans.iter().enumerate().take(num_rows).skip(row + 1) {
            match next_span_opt {
                Some(next_span)
                    if next_span.color == block_color
                        && next_span.left == block_left
                        && next_span.right == block_right =>
                {
                    block_bottom = next_row;
                }
                _ => break,
            }
        }

        row = block_bottom + 1;

        // Minimum width: 10 columns (filters out inline code like `var`)
        if block_right - block_left < 9 {
            continue;
        }

        let candidate = CodeBlock {
            top_row: block_top,
            bottom_row: block_bottom,
            left_col: block_left,
            right_col: block_right,
            has_border: false,
        };

        // Dedup: skip if overlaps with an existing (border-detected) block
        let overlaps = blocks.iter().any(|existing| {
            candidate.top_row <= existing.bottom_row
                && candidate.bottom_row >= existing.top_row
                && candidate.left_col <= existing.right_col
                && candidate.right_col >= existing.left_col
        });

        if !overlaps {
            blocks.push(candidate);
        }
    }
}

/// Extract the inner content of a code block from the screen buffer.
///
/// Reads graphemes from the inner rectangle (excluding the border row and column on all four
/// sides for a bordered block) and joins rows with `\n`. The result goes through the smart
/// copy pipeline to strip stray box-drawing characters and trailing white space, and leading
/// and trailing blank lines are removed.
pub fn extract_content(screen: &impl Grid, block: &CodeBlock) -> String {
    let mut lines = Vec::new();

    // A bordered block's content leaves out the border; a color block is the whole rectangle.
    let (row_start, row_end, col_start_offset, col_end_offset) = if block.has_border {
        (block.top_row + 1, block.bottom_row, 1usize, 0usize)
    } else {
        (block.top_row, block.bottom_row + 1, 0, 1)
    };

    for row in row_start..row_end {
        if row >= screen.rows() {
            break;
        }
        let mut line = String::new();
        let start_col = block.left_col + col_start_offset;
        let end_col = (block.right_col + col_end_offset).min(screen.cols());
        for col in start_col..end_col {
            let g = screen.text(row, col);
            line.push_str(if g.is_empty() { " " } else { g });
        }
        lines.push(line);
    }

    copytext::trim_blank_lines(&copytext::smart_copy(&lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid of one char per cell, with an optional background per cell.
    struct G {
        rows: Vec<Vec<String>>,
        bg: Vec<Vec<Option<u32>>>,
    }

    impl G {
        fn text_rows(lines: &[&str]) -> G {
            let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let rows: Vec<Vec<String>> = lines
                .iter()
                .map(|l| l.chars().map(|c| c.to_string()).chain(std::iter::repeat(" ".to_string())).take(w).collect())
                .collect();
            let bg = vec![vec![None; w]; rows.len()];
            G { rows, bg }
        }
        fn paint(&mut self, r0: usize, r1: usize, c0: usize, c1: usize, color: u32) {
            for r in r0..=r1 {
                for c in c0..=c1 {
                    self.bg[r][c] = Some(color);
                }
            }
        }
    }

    impl Grid for G {
        fn rows(&self) -> usize {
            self.rows.len()
        }
        fn cols(&self) -> usize {
            self.rows.first().map_or(0, |r| r.len())
        }
        fn text(&self, row: usize, col: usize) -> &str {
            &self.rows[row][col]
        }
        fn bg(&self, row: usize, col: usize) -> Option<u32> {
            self.bg[row][col]
        }
    }

    #[test]
    fn is_box_drawing_range() {
        assert!(is_box_drawing('\u{2500}'));
        assert!(is_box_drawing('\u{257F}'));
        assert!(!is_box_drawing('A'));
        assert!(!is_box_drawing(' '));
        assert!(!is_box_drawing('\u{2580}'));
    }

    #[test]
    fn corners() {
        assert!(is_top_left_corner('\u{250C}') && is_top_left_corner('\u{256D}') && is_top_left_corner('\u{2554}'));
        assert!(!is_top_left_corner('\u{2500}'));
        assert!(is_top_right_corner('\u{2510}') && is_top_right_corner('\u{256E}'));
        assert!(is_bottom_left_corner('\u{2514}') && is_bottom_left_corner('\u{2570}'));
        assert!(is_bottom_right_corner('\u{2518}') && is_bottom_right_corner('\u{256F}'));
    }

    #[test]
    fn simple_box_is_found_and_its_content_extracted() {
        let g = G::text_rows(&["\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}", "\u{2502} hi   \u{2502}", "\u{2502} bye  \u{2502}", "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}", "        "]);
        let blocks = detect_code_blocks(&g);
        assert_eq!(blocks.len(), 1);
        assert_eq!((blocks[0].top_row, blocks[0].bottom_row, blocks[0].left_col, blocks[0].right_col), (0, 3, 0, 7));
        assert!(blocks[0].has_border);
        assert!(blocks[0].contains(1, 3) && !blocks[0].contains(4, 3));
        assert_eq!(extract_content(&g, &blocks[0]), " hi\n bye");
    }

    #[test]
    fn rounded_corners_and_too_small_boxes() {
        let g = G::text_rows(&["\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}", "\u{2502} cod  \u{2502}", "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}", "        "]);
        assert_eq!(detect_code_blocks(&g).len(), 1);
        let g = G::text_rows(&["\u{250c}\u{2500}\u{2510}", "\u{2502}x\u{2502}", "\u{2514}\u{2500}\u{2518}"]);
        assert_eq!(detect_code_blocks(&g).len(), 0, "too narrow");
        // Two rows only: no content row.
        let g = G::text_rows(&["\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}", "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}"]);
        assert_eq!(detect_code_blocks(&g).len(), 0, "too short");
    }

    #[test]
    fn a_nested_box_is_dropped_and_a_broken_one_is_not_found() {
        let g = G::text_rows(&[
            "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}",
            "\u{2502}\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\u{2502}",
            "\u{2502}\u{2502} abc   \u{2502}\u{2502}",
            "\u{2502}\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}\u{2502}",
            "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}",
        ]);
        let b = detect_code_blocks(&g);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].top_row, b[0].bottom_row), (0, 4));
        // A text row inside the borders ends the box: no bottom, no block.
        let g = G::text_rows(&["\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}", "\u{2502} a   \u{2502}", " broken ", "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}"]);
        assert_eq!(detect_code_blocks(&g).len(), 0);
    }

    #[test]
    fn a_uniform_background_rectangle_is_a_block() {
        let mut g = G::text_rows(&["            ", " code line 1 ", " code line 2 ", "            ", "tail        "]);
        g.paint(1, 2, 0, 11, 0x202020);
        let b = detect_code_blocks(&g);
        assert_eq!(b.len(), 1);
        assert!(!b[0].has_border);
        assert_eq!((b[0].top_row, b[0].bottom_row, b[0].left_col, b[0].right_col), (1, 2, 0, 11));
        assert_eq!(extract_content(&g, &b[0]), " code line 1\n code line 2");
        // Narrow runs (inline code) are not blocks.
        let mut g = G::text_rows(&["a code b          "]);
        g.paint(0, 0, 2, 5, 0x202020);
        assert_eq!(detect_code_blocks(&g).len(), 0);
    }

    /// The scan runs on pointer moves over changing output, so a full screen of busy text
    /// (many horizontal rules, many colored cells) must stay cheap.
    #[test]
    fn a_busy_full_screen_is_scanned_fast() {
        let (rows, cols) = (60usize, 240usize);
        let lines: Vec<String> = (0..rows)
            .map(|r| match r % 4 {
                0 => "\u{2500}".repeat(cols),
                1 => format!("\u{2502}{}\u{2502}", "x".repeat(cols - 2)),
                2 => "ab cd ".repeat(cols / 6 + 1).chars().take(cols).collect(),
                _ => "\u{250c}\u{2500}\u{2510}".repeat(cols / 3),
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut g = G::text_rows(&refs);
        for r in (0..rows).step_by(3) {
            g.paint(r, r, 10, 200, 0x112233 + r as u32);
        }
        let t = std::time::Instant::now();
        let _ = detect_code_blocks(&g);
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_millis(100), "scan took {took:?}");
    }

    /// Real libghostty cells: an empty cell has an empty grapheme, a box edge is one char.
    #[test]
    fn finds_a_box_on_a_real_screen() {
        use forgetty_vt::{Color, Screen, Terminal, ANSI_PALETTE_BLACK};
        struct S<'a>(&'a Screen);
        impl Grid for S<'_> {
            fn rows(&self) -> usize {
                self.0.rows()
            }
            fn cols(&self) -> usize {
                self.0.cols()
            }
            fn text(&self, r: usize, c: usize) -> &str {
                &self.0.cell(r, c).grapheme
            }
            fn bg(&self, r: usize, c: usize) -> Option<u32> {
                match self.0.cell(r, c).attrs.bg {
                    Color::Default => None,
                    Color::Rgb(r, g, b) => Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
                }
            }
        }
        let mut t = Terminal::with_scrollback(10, 40, [ANSI_PALETTE_BLACK; 16], 1 << 20);
        t.feed(
            "see\r\n\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\r\n\u{2502} code(x) \u{2502}\r\n\u{2502} more    \u{2502}\r\n\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}\r\nend"
                .as_bytes(),
        );
        let screen = t.screen();
        let found = detect_code_blocks(&S(screen));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].top_row, found[0].bottom_row, found[0].left_col, found[0].right_col), (1, 4, 0, 10));
        assert_eq!(extract_content(&S(screen), &found[0]), " code(x)\n more");
    }
}
