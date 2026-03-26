//! FFI bindings for libghostty-vt.
//!
//! Hand-written `extern "C"` declarations for the subset of the libghostty-vt
//! C API that forgetty-vt needs. All handles are represented as opaque
//! pointers (`*mut c_void`-style newtypes) and must only be used through
//! the safe wrappers in `terminal.rs` and `screen.rs`.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::c_void;

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

pub type GhosttyResult = i32;

pub const GHOSTTY_SUCCESS: GhosttyResult = 0;
pub const GHOSTTY_OUT_OF_MEMORY: GhosttyResult = -1;
pub const GHOSTTY_INVALID_VALUE: GhosttyResult = -2;
pub const GHOSTTY_OUT_OF_SPACE: GhosttyResult = -3;

// ---------------------------------------------------------------------------
// Opaque handles
// ---------------------------------------------------------------------------

/// Opaque terminal handle.
#[repr(C)]
pub struct GhosttyTerminalOpaque {
    _private: [u8; 0],
}
pub type GhosttyTerminal = *mut GhosttyTerminalOpaque;

/// Opaque render state handle.
#[repr(C)]
pub struct GhosttyRenderStateOpaque {
    _private: [u8; 0],
}
pub type GhosttyRenderState = *mut GhosttyRenderStateOpaque;

/// Opaque row iterator handle.
#[repr(C)]
pub struct GhosttyRenderStateRowIteratorOpaque {
    _private: [u8; 0],
}
pub type GhosttyRenderStateRowIterator = *mut GhosttyRenderStateRowIteratorOpaque;

/// Opaque row cells handle.
#[repr(C)]
pub struct GhosttyRenderStateRowCellsOpaque {
    _private: [u8; 0],
}
pub type GhosttyRenderStateRowCells = *mut GhosttyRenderStateRowCellsOpaque;

// ---------------------------------------------------------------------------
// String
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

// ---------------------------------------------------------------------------
// Color types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub type GhosttyColorPaletteIndex = u8;

// ---------------------------------------------------------------------------
// Style types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GhosttyStyleColorTag {
    None = 0,
    Palette = 1,
    Rgb = 2,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyStyleColorValue {
    pub palette: GhosttyColorPaletteIndex,
    pub rgb: GhosttyColorRgb,
    pub _padding: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyleColor {
    pub tag: GhosttyStyleColorTag,
    pub value: GhosttyStyleColorValue,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyle {
    pub size: usize,
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: i32,
}

impl GhosttyStyle {
    /// Create a zeroed style with the `size` field set.
    pub fn init_sized() -> Self {
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

// ---------------------------------------------------------------------------
// Terminal types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalOptions {
    pub cols: u16,
    pub rows: u16,
    pub max_scrollback: usize,
}

// Terminal data types
pub const GHOSTTY_TERMINAL_DATA_COLS: i32 = 1;
pub const GHOSTTY_TERMINAL_DATA_ROWS: i32 = 2;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_X: i32 = 3;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_Y: i32 = 4;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: i32 = 7;
pub const GHOSTTY_TERMINAL_DATA_TITLE: i32 = 12;
pub const GHOSTTY_TERMINAL_DATA_TOTAL_ROWS: i32 = 14;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS: i32 = 15;

// Terminal options
pub const GHOSTTY_TERMINAL_OPT_USERDATA: i32 = 0;
pub const GHOSTTY_TERMINAL_OPT_BELL: i32 = 2;
pub const GHOSTTY_TERMINAL_OPT_TITLE_CHANGED: i32 = 5;

// Cell data types
pub type GhosttyCell = u64;
pub type GhosttyRow = u64;

pub const GHOSTTY_CELL_DATA_CODEPOINT: i32 = 1;
pub const GHOSTTY_CELL_DATA_CONTENT_TAG: i32 = 2;
pub const GHOSTTY_CELL_DATA_WIDE: i32 = 3;
pub const GHOSTTY_CELL_DATA_HAS_TEXT: i32 = 4;

// Cell content tags
pub const GHOSTTY_CELL_CONTENT_CODEPOINT: i32 = 0;
pub const GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME: i32 = 1;

// Cell wide values
pub const GHOSTTY_CELL_WIDE_NARROW: i32 = 0;
pub const GHOSTTY_CELL_WIDE_SPACER_TAIL: i32 = 2;
pub const GHOSTTY_CELL_WIDE_SPACER_HEAD: i32 = 3;

// Render state data types
pub const GHOSTTY_RENDER_STATE_DATA_COLS: i32 = 1;
pub const GHOSTTY_RENDER_STATE_DATA_ROWS: i32 = 2;
pub const GHOSTTY_RENDER_STATE_DATA_DIRTY: i32 = 3;
pub const GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR: i32 = 4;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE: i32 = 11;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE: i32 = 14;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X: i32 = 15;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y: i32 = 16;

// Render state dirty values
pub const GHOSTTY_RENDER_STATE_DIRTY_FALSE: i32 = 0;
#[allow(dead_code)]
pub const GHOSTTY_RENDER_STATE_DIRTY_PARTIAL: i32 = 1;
#[allow(dead_code)]
pub const GHOSTTY_RENDER_STATE_DIRTY_FULL: i32 = 2;

// Render state option
pub const GHOSTTY_RENDER_STATE_OPTION_DIRTY: i32 = 0;

// Row data types
pub const GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY: i32 = 1;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_CELLS: i32 = 3;

// Row option
pub const GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY: i32 = 0;

// Row cells data types
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW: i32 = 1;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE: i32 = 2;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN: i32 = 3;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF: i32 = 4;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR: i32 = 5;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR: i32 = 6;

// Point types
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyPointCoordinate {
    pub x: u16,
    pub y: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyPointValue {
    pub coordinate: GhosttyPointCoordinate,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyPoint {
    pub tag: i32,
    pub value: GhosttyPointValue,
}

pub const GHOSTTY_POINT_TAG_ACTIVE: i32 = 0;
pub const GHOSTTY_POINT_TAG_VIEWPORT: i32 = 1;
pub const GHOSTTY_POINT_TAG_SCREEN: i32 = 2;
pub const GHOSTTY_POINT_TAG_HISTORY: i32 = 3;

// Grid ref
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyGridRef {
    pub size: usize,
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

impl GhosttyGridRef {
    pub fn init_sized() -> Self {
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

// ---------------------------------------------------------------------------
// Render state colors (sized-struct pattern)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyRenderStateColors {
    pub size: usize,
    pub background: GhosttyColorRgb,
    pub foreground: GhosttyColorRgb,
    pub cursor: GhosttyColorRgb,
    pub cursor_has_value: bool,
    pub palette: [GhosttyColorRgb; 256],
}

impl GhosttyRenderStateColors {
    /// Create a zeroed colors struct with the `size` field set.
    pub fn init_sized() -> Self {
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

// ---------------------------------------------------------------------------
// Callback types for terminal effects
// ---------------------------------------------------------------------------

pub type GhosttyTerminalBellFn =
    Option<unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void)>;

pub type GhosttyTerminalTitleChangedFn =
    Option<unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void)>;

// ---------------------------------------------------------------------------
// Extern declarations
// ---------------------------------------------------------------------------

extern "C" {
    // Terminal
    pub fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut GhosttyTerminal,
        options: GhosttyTerminalOptions,
    ) -> GhosttyResult;

    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);

    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);

    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;

    pub fn ghostty_terminal_get(
        terminal: GhosttyTerminal,
        data: i32,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_terminal_set(
        terminal: GhosttyTerminal,
        option: i32,
        value: *const c_void,
    ) -> GhosttyResult;

    pub fn ghostty_terminal_grid_ref(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyGridRef,
    ) -> GhosttyResult;

    // Grid ref
    pub fn ghostty_grid_ref_cell(
        grid_ref: *const GhosttyGridRef,
        out_cell: *mut GhosttyCell,
    ) -> GhosttyResult;

    pub fn ghostty_grid_ref_style(
        grid_ref: *const GhosttyGridRef,
        out_style: *mut GhosttyStyle,
    ) -> GhosttyResult;

    pub fn ghostty_grid_ref_graphemes(
        grid_ref: *const GhosttyGridRef,
        buf: *mut u32,
        buf_len: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    // Cell / Row
    pub fn ghostty_cell_get(cell: GhosttyCell, data: i32, out: *mut c_void) -> GhosttyResult;

    pub fn ghostty_row_get(row: GhosttyRow, data: i32, out: *mut c_void) -> GhosttyResult;

    // Render state
    pub fn ghostty_render_state_new(
        allocator: *const c_void,
        state: *mut GhosttyRenderState,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_free(state: GhosttyRenderState);

    pub fn ghostty_render_state_update(
        state: GhosttyRenderState,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_get(
        state: GhosttyRenderState,
        data: i32,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_set(
        state: GhosttyRenderState,
        option: i32,
        value: *const c_void,
    ) -> GhosttyResult;

    // Row iterator
    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const c_void,
        out_iterator: *mut GhosttyRenderStateRowIterator,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);

    pub fn ghostty_render_state_row_iterator_next(iterator: GhosttyRenderStateRowIterator) -> bool;

    pub fn ghostty_render_state_row_get(
        iterator: GhosttyRenderStateRowIterator,
        data: i32,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_set(
        iterator: GhosttyRenderStateRowIterator,
        option: i32,
        value: *const c_void,
    ) -> GhosttyResult;

    // Row cells
    pub fn ghostty_render_state_row_cells_new(
        allocator: *const c_void,
        out_cells: *mut GhosttyRenderStateRowCells,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_cells_next(cells: GhosttyRenderStateRowCells) -> bool;

    pub fn ghostty_render_state_row_cells_get(
        cells: GhosttyRenderStateRowCells,
        data: i32,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);

    // Render state colors
    pub fn ghostty_render_state_colors_get(
        state: GhosttyRenderState,
        out_colors: *mut GhosttyRenderStateColors,
    ) -> GhosttyResult;
}
