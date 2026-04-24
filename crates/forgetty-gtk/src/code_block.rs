//! Code block detection and content extraction for terminal viewports.
//!
//! Two detection strategies:
//! 1. **Border detection**: rectangular regions bounded by box-drawing characters
//!    (U+2500-U+257F) — catches Claude Code's bordered code blocks.
//! 2. **Background color detection**: rectangular regions with a uniform non-default
//!    background color — catches Claude Code's ``` fenced blocks and any TUI that
//!    highlights code regions with background color.

use forgetty_vt::{Color, Screen};

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
fn first_char(screen: &Screen, row: usize, col: usize) -> char {
    if row >= screen.rows() || col >= screen.cols() {
        return '\0';
    }
    screen.cell(row, col).grapheme.chars().next().unwrap_or('\0')
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
pub fn detect_code_blocks(screen: &Screen) -> Vec<CodeBlock> {
    let num_rows = screen.rows();
    let num_cols = screen.cols();
    let mut blocks: Vec<CodeBlock> = Vec::new();

    for row in 0..num_rows {
        let cells = screen.row(row);
        for col in 0..num_cols {
            let c = cells[col].grapheme.chars().next().unwrap_or('\0');
            if !is_top_left_corner(c) {
                continue;
            }

            // Found a candidate top-left corner at (row, col).
            // Scan right along the same row for horizontal border characters
            // until we find a top-right corner.
            let mut right_col = None;
            for (rc, cell) in cells.iter().enumerate().take(num_cols).skip(col + 1) {
                let rc_char = cell.grapheme.chars().next().unwrap_or('\0');
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
fn detect_color_blocks(screen: &Screen, blocks: &mut Vec<CodeBlock>) {
    let num_rows = screen.rows();
    let num_cols = screen.cols();

    // For each row, find the dominant non-default bg span (if any).
    // A "span" is (left_col, right_col, Color::Rgb).
    #[derive(Clone, PartialEq)]
    struct BgSpan {
        left: usize,
        right: usize,
        color: Color,
    }

    let mut row_spans: Vec<Option<BgSpan>> = Vec::with_capacity(num_rows);

    for row in 0..num_rows {
        let cells = screen.row(row);
        // Find the longest contiguous run of cells with the same non-default bg.
        let mut best_span: Option<BgSpan> = None;
        let mut cur_start = 0usize;
        let mut cur_color = Color::Default;
        let mut cur_len = 0usize;

        for (col, cell) in cells.iter().enumerate().take(num_cols.min(cells.len())) {
            let bg = cell.attrs.bg;
            if bg != Color::Default && bg == cur_color {
                cur_len += 1;
            } else {
                // Save previous run if it's the longest
                if cur_color != Color::Default
                    && cur_len >= 10
                    && best_span.as_ref().is_none_or(|s| cur_len > s.right - s.left + 1)
                {
                    best_span = Some(BgSpan {
                        left: cur_start,
                        right: cur_start + cur_len - 1,
                        color: cur_color,
                    });
                }
                if bg != Color::Default {
                    cur_start = col;
                    cur_color = bg;
                    cur_len = 1;
                } else {
                    cur_color = Color::Default;
                    cur_len = 0;
                }
            }
        }
        // Check final run
        if cur_color != Color::Default
            && cur_len >= 10
            && best_span.as_ref().is_none_or(|s| cur_len > s.right - s.left + 1)
        {
            best_span =
                Some(BgSpan { left: cur_start, right: cur_start + cur_len - 1, color: cur_color });
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
/// Reads graphemes from cells in the inner rectangle (excluding border
/// row/column on all four sides). Joins rows with `\n`. Runs the result
/// through `smart_copy_pipeline()` to strip any stray box-drawing characters,
/// trailing whitespace, and normalize line endings. Trims leading/trailing
/// blank lines.
pub fn extract_content(screen: &Screen, block: &CodeBlock) -> String {
    let mut lines = Vec::new();

    // For border blocks, the content excludes the border row/col on all sides.
    // For color blocks, the content is the full rectangle.
    let (row_start, row_end, col_start_offset, col_end_offset) = if block.has_border {
        (block.top_row + 1, block.bottom_row, 1usize, 0usize)
    } else {
        (block.top_row, block.bottom_row + 1, 0, 1)
    };

    for row in row_start..row_end {
        if row >= screen.rows() {
            break;
        }
        let cells = screen.row(row);
        let mut line = String::new();
        let start_col = block.left_col + col_start_offset;
        let end_col = (block.right_col + col_end_offset).min(cells.len());
        for cell in cells.iter().take(end_col).skip(start_col) {
            line.push_str(&cell.grapheme);
        }
        lines.push(line);
    }

    let raw = lines.join("\n");

    // Run through the smart copy pipeline: strip box-drawing, trailing whitespace,
    // normalize line endings.
    let cleaned = crate::clipboard::smart_copy_pipeline(&raw);

    // Trim leading and trailing blank lines (AC-20)
    trim_blank_lines(&cleaned)
}

/// Trim leading and trailing lines that are entirely whitespace.
/// Interior blank lines are preserved.
fn trim_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();

    // Find first non-blank line
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());

    // Find last non-blank line
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(start);

    if start >= end {
        return String::new();
    }

    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_box_drawing() {
        assert!(is_box_drawing('\u{2500}')); // ─
        assert!(is_box_drawing('\u{2502}')); // │
        assert!(is_box_drawing('\u{250C}')); // ┌
        assert!(is_box_drawing('\u{2518}')); // ┘
        assert!(is_box_drawing('\u{256D}')); // ╭
        assert!(is_box_drawing('\u{257F}')); // end of range
        assert!(!is_box_drawing('A'));
        assert!(!is_box_drawing(' '));
        assert!(!is_box_drawing('\u{2580}')); // just outside range
    }

    #[test]
    fn test_corner_detection() {
        assert!(is_top_left_corner('\u{250C}')); // ┌
        assert!(is_top_left_corner('\u{256D}')); // ╭ rounded
        assert!(is_top_left_corner('\u{2554}')); // ╔ double
        assert!(!is_top_left_corner('\u{2500}')); // ─ not a corner

        assert!(is_top_right_corner('\u{2510}')); // ┐
        assert!(is_top_right_corner('\u{256E}')); // ╮ rounded
        assert!(is_top_right_corner('\u{2557}')); // ╗ double

        assert!(is_bottom_left_corner('\u{2514}')); // └
        assert!(is_bottom_left_corner('\u{2570}')); // ╰ rounded
        assert!(is_bottom_left_corner('\u{255A}')); // ╚ double

        assert!(is_bottom_right_corner('\u{2518}')); // ┘
        assert!(is_bottom_right_corner('\u{256F}')); // ╯ rounded
        assert!(is_bottom_right_corner('\u{255D}')); // ╝ double
    }

    #[test]
    fn test_trim_blank_lines() {
        assert_eq!(trim_blank_lines("  \n  \nhello\nworld\n  \n  "), "hello\nworld");
        assert_eq!(trim_blank_lines("hello\nworld"), "hello\nworld");
        assert_eq!(trim_blank_lines("  \n  "), "");
        assert_eq!(trim_blank_lines(""), "");
        assert_eq!(trim_blank_lines("hello\n\nworld"), "hello\n\nworld");
    }

    #[test]
    fn test_detect_simple_block() {
        // Build a 5x7 screen with a simple box:
        // ┌─────┐
        // │ hi  │
        // │ bye │
        // └─────┘
        let mut screen = Screen::new(5, 8);
        let chars = ["┌──────┐", "│ hi   │", "│ bye  │", "└──────┘", "        "];

        for (r, line) in chars.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let cell = forgetty_vt::Cell {
                    grapheme: ch.to_string(),
                    attrs: forgetty_vt::CellAttributes::default(),
                };
                screen.set_cell(r, c, cell);
            }
        }

        let blocks = detect_code_blocks(&screen);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].top_row, 0);
        assert_eq!(blocks[0].bottom_row, 3);
        assert_eq!(blocks[0].left_col, 0);
        assert_eq!(blocks[0].right_col, 7);
    }

    #[test]
    fn test_detect_rounded_corners() {
        // ╭──────╮
        // │ code │
        // ╰──────╯
        let mut screen = Screen::new(4, 8);
        let chars = ["╭──────╮", "│ code │", "╰──────╯", "        "];

        for (r, line) in chars.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let cell = forgetty_vt::Cell {
                    grapheme: ch.to_string(),
                    attrs: forgetty_vt::CellAttributes::default(),
                };
                screen.set_cell(r, c, cell);
            }
        }

        let blocks = detect_code_blocks(&screen);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn test_block_too_small() {
        // Block too narrow (3 cols, need at least 5):
        // ┌─┐
        // │x│
        // └─┘
        let mut screen = Screen::new(3, 3);
        let chars = ["┌─┐", "│x│", "└─┘"];

        for (r, line) in chars.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let cell = forgetty_vt::Cell {
                    grapheme: ch.to_string(),
                    attrs: forgetty_vt::CellAttributes::default(),
                };
                screen.set_cell(r, c, cell);
            }
        }

        let blocks = detect_code_blocks(&screen);
        assert_eq!(blocks.len(), 0, "Block too narrow should not be detected");
    }

    #[test]
    fn test_extract_content_simple() {
        // ┌──────┐
        // │ hi   │
        // │ bye  │
        // └──────┘
        let mut screen = Screen::new(4, 8);
        let chars = ["┌──────┐", "│ hi   │", "│ bye  │", "└──────┘"];

        for (r, line) in chars.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let cell = forgetty_vt::Cell {
                    grapheme: ch.to_string(),
                    attrs: forgetty_vt::CellAttributes::default(),
                };
                screen.set_cell(r, c, cell);
            }
        }

        let block =
            CodeBlock { top_row: 0, bottom_row: 3, left_col: 0, right_col: 7, has_border: true };
        let content = extract_content(&screen, &block);
        assert_eq!(content, " hi\n bye");
    }
}
