//! Which cursor a pane draws (H10). The terminal reports a shape (DECSCUSR); a program that never
//! sets one leaves the default, block. The `cursor_style` setting replaces that default only: a
//! program that asks for a bar or an underline (an editor in insert mode) keeps what it asked.

use crate::config::CursorStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Block,
    Bar,
    Underline,
    Hollow,
}

/// `vt_style` is the terminal's: BAR 0, BLOCK 1, UNDERLINE 2, BLOCK_HOLLOW 3. A pane without
/// focus always draws the outline, so it can be told from the one that takes the keys.
pub fn shape(vt_style: i32, configured: CursorStyle, focused: bool) -> Shape {
    if !focused {
        return Shape::Hollow;
    }
    match vt_style {
        0 => Shape::Bar,
        2 => Shape::Underline,
        3 => Shape::Hollow,
        _ => match configured {
            CursorStyle::Block => Shape::Block,
            CursorStyle::Bar => Shape::Bar,
            CursorStyle::Underline => Shape::Underline,
            CursorStyle::BlockHollow => Shape::Hollow,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_replaces_only_the_default_block() {
        for (cfg, want) in [
            (CursorStyle::Block, Shape::Block),
            (CursorStyle::Bar, Shape::Bar),
            (CursorStyle::Underline, Shape::Underline),
            (CursorStyle::BlockHollow, Shape::Hollow),
        ] {
            assert_eq!(shape(1, cfg, true), want);
            // A program that asked for something else keeps it.
            assert_eq!(shape(0, cfg, true), Shape::Bar);
            assert_eq!(shape(2, cfg, true), Shape::Underline);
            assert_eq!(shape(3, cfg, true), Shape::Hollow);
            // No focus: the outline, whatever was set.
            assert_eq!(shape(1, cfg, false), Shape::Hollow);
            assert_eq!(shape(0, cfg, false), Shape::Hollow);
        }
    }

    #[test]
    fn an_unknown_shape_is_the_default() {
        assert_eq!(shape(9, CursorStyle::Bar, true), Shape::Bar);
        assert_eq!(shape(-1, CursorStyle::Block, true), Shape::Block);
    }
}
