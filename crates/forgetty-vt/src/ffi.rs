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

/// Scrollbar state returned by `GHOSTTY_TERMINAL_DATA_SCROLLBAR`.
/// Matches the C struct `GhosttyTerminalScrollbar` from terminal.h.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalScrollbar {
    /// Total size of the scrollable area in rows.
    pub total: u64,
    /// Offset into the total area that the viewport is at.
    pub offset: u64,
    /// Length of the visible area in rows.
    pub len: u64,
}

// Terminal data types
pub const GHOSTTY_TERMINAL_DATA_COLS: i32 = 1;
pub const GHOSTTY_TERMINAL_DATA_ROWS: i32 = 2;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_X: i32 = 3;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_Y: i32 = 4;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: i32 = 7;
pub const GHOSTTY_TERMINAL_DATA_TITLE: i32 = 12;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBAR: i32 = 9;
pub const GHOSTTY_TERMINAL_DATA_TOTAL_ROWS: i32 = 14;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS: i32 = 15;

// Terminal options
pub const GHOSTTY_TERMINAL_OPT_USERDATA: i32 = 0;
pub const GHOSTTY_TERMINAL_OPT_WRITE_PTY: i32 = 1;
pub const GHOSTTY_TERMINAL_OPT_BELL: i32 = 2;
pub const GHOSTTY_TERMINAL_OPT_XTVERSION: i32 = 4;
pub const GHOSTTY_TERMINAL_OPT_TITLE_CHANGED: i32 = 5;
pub const GHOSTTY_TERMINAL_OPT_SIZE: i32 = 6;
pub const GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES: i32 = 8;

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
pub const GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR: i32 = 7;
pub const GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR_HAS_VALUE: i32 = 8;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE: i32 = 10;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR_BLINKING: i32 = 12;

// Cursor visual style enum values (returned by CURSOR_VISUAL_STYLE)
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR: i32 = 0;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK: i32 = 1;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE: i32 = 2;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW: i32 = 3;

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
// Device attributes types (for DA1/DA2/DA3 callbacks)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyDeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyDeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyDeviceAttributesTertiary {
    pub unit_id: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyDeviceAttributes {
    pub primary: GhosttyDeviceAttributesPrimary,
    pub secondary: GhosttyDeviceAttributesSecondary,
    pub tertiary: GhosttyDeviceAttributesTertiary,
}

// Size report types (for SIZE callback)
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySizeReportSize {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

// DA conformance levels
pub const GHOSTTY_DA_CONFORMANCE_VT220: u16 = 62;

// DA feature codes
pub const GHOSTTY_DA_FEATURE_COLUMNS_132: u16 = 1;
pub const GHOSTTY_DA_FEATURE_SELECTIVE_ERASE: u16 = 6;
pub const GHOSTTY_DA_FEATURE_ANSI_COLOR: u16 = 22;

// DA2 device types
pub const GHOSTTY_DA_DEVICE_TYPE_VT220: u16 = 1;

// ---------------------------------------------------------------------------
// Key encoder types
// ---------------------------------------------------------------------------

/// Opaque key encoder handle.
#[repr(C)]
pub struct GhosttyKeyEncoderOpaque {
    _private: [u8; 0],
}
pub type GhosttyKeyEncoder = *mut GhosttyKeyEncoderOpaque;

/// Opaque key event handle.
#[repr(C)]
pub struct GhosttyKeyEventOpaque {
    _private: [u8; 0],
}
pub type GhosttyKeyEvent = *mut GhosttyKeyEventOpaque;

/// Modifier bitmask type.
pub type GhosttyMods = u16;

pub const GHOSTTY_MODS_SHIFT: GhosttyMods = 1 << 0;
pub const GHOSTTY_MODS_CTRL: GhosttyMods = 1 << 1;
pub const GHOSTTY_MODS_ALT: GhosttyMods = 1 << 2;
pub const GHOSTTY_MODS_SUPER: GhosttyMods = 1 << 3;
pub const GHOSTTY_MODS_CAPS_LOCK: GhosttyMods = 1 << 4;
pub const GHOSTTY_MODS_NUM_LOCK: GhosttyMods = 1 << 5;

// Modifier side bits — set when the right-hand modifier is pressed.
// Matches ghostty/vt/key/event.h exactly.
pub const GHOSTTY_MODS_SHIFT_SIDE: GhosttyMods = 1 << 6;
pub const GHOSTTY_MODS_CTRL_SIDE: GhosttyMods = 1 << 7;
pub const GHOSTTY_MODS_ALT_SIDE: GhosttyMods = 1 << 8;
pub const GHOSTTY_MODS_SUPER_SIDE: GhosttyMods = 1 << 9;

/// Key action constants.
pub const GHOSTTY_KEY_ACTION_RELEASE: i32 = 0;
pub const GHOSTTY_KEY_ACTION_PRESS: i32 = 1;
pub const GHOSTTY_KEY_ACTION_REPEAT: i32 = 2;

// GhosttyKey enum values — must match ghostty/vt/key/event.h exactly.
// The C enum is sequential starting from 0.
pub const GHOSTTY_KEY_UNIDENTIFIED: i32 = 0;
// Writing System Keys (W3C 3.1.1)
pub const GHOSTTY_KEY_BACKQUOTE: i32 = 1;
pub const GHOSTTY_KEY_BACKSLASH: i32 = 2;
pub const GHOSTTY_KEY_BRACKET_LEFT: i32 = 3;
pub const GHOSTTY_KEY_BRACKET_RIGHT: i32 = 4;
pub const GHOSTTY_KEY_COMMA: i32 = 5;
pub const GHOSTTY_KEY_DIGIT_0: i32 = 6;
pub const GHOSTTY_KEY_DIGIT_1: i32 = 7;
pub const GHOSTTY_KEY_DIGIT_2: i32 = 8;
pub const GHOSTTY_KEY_DIGIT_3: i32 = 9;
pub const GHOSTTY_KEY_DIGIT_4: i32 = 10;
pub const GHOSTTY_KEY_DIGIT_5: i32 = 11;
pub const GHOSTTY_KEY_DIGIT_6: i32 = 12;
pub const GHOSTTY_KEY_DIGIT_7: i32 = 13;
pub const GHOSTTY_KEY_DIGIT_8: i32 = 14;
pub const GHOSTTY_KEY_DIGIT_9: i32 = 15;
pub const GHOSTTY_KEY_EQUAL: i32 = 16;
pub const GHOSTTY_KEY_INTL_BACKSLASH: i32 = 17;
pub const GHOSTTY_KEY_INTL_RO: i32 = 18;
pub const GHOSTTY_KEY_INTL_YEN: i32 = 19;
pub const GHOSTTY_KEY_A: i32 = 20;
pub const GHOSTTY_KEY_B: i32 = 21;
pub const GHOSTTY_KEY_C: i32 = 22;
pub const GHOSTTY_KEY_D: i32 = 23;
pub const GHOSTTY_KEY_E: i32 = 24;
pub const GHOSTTY_KEY_F: i32 = 25;
pub const GHOSTTY_KEY_G: i32 = 26;
pub const GHOSTTY_KEY_H: i32 = 27;
pub const GHOSTTY_KEY_I: i32 = 28;
pub const GHOSTTY_KEY_J: i32 = 29;
pub const GHOSTTY_KEY_K: i32 = 30;
pub const GHOSTTY_KEY_L: i32 = 31;
pub const GHOSTTY_KEY_M: i32 = 32;
pub const GHOSTTY_KEY_N: i32 = 33;
pub const GHOSTTY_KEY_O: i32 = 34;
pub const GHOSTTY_KEY_P: i32 = 35;
pub const GHOSTTY_KEY_Q: i32 = 36;
pub const GHOSTTY_KEY_R: i32 = 37;
pub const GHOSTTY_KEY_S: i32 = 38;
pub const GHOSTTY_KEY_T: i32 = 39;
pub const GHOSTTY_KEY_U: i32 = 40;
pub const GHOSTTY_KEY_V: i32 = 41;
pub const GHOSTTY_KEY_W: i32 = 42;
pub const GHOSTTY_KEY_X: i32 = 43;
pub const GHOSTTY_KEY_Y: i32 = 44;
pub const GHOSTTY_KEY_Z: i32 = 45;
pub const GHOSTTY_KEY_MINUS: i32 = 46;
pub const GHOSTTY_KEY_PERIOD: i32 = 47;
pub const GHOSTTY_KEY_QUOTE: i32 = 48;
pub const GHOSTTY_KEY_SEMICOLON: i32 = 49;
pub const GHOSTTY_KEY_SLASH: i32 = 50;
// Functional Keys (W3C 3.1.2)
pub const GHOSTTY_KEY_ALT_LEFT: i32 = 51;
pub const GHOSTTY_KEY_ALT_RIGHT: i32 = 52;
pub const GHOSTTY_KEY_BACKSPACE: i32 = 53;
pub const GHOSTTY_KEY_CAPS_LOCK: i32 = 54;
pub const GHOSTTY_KEY_CONTEXT_MENU: i32 = 55;
pub const GHOSTTY_KEY_CONTROL_LEFT: i32 = 56;
pub const GHOSTTY_KEY_CONTROL_RIGHT: i32 = 57;
pub const GHOSTTY_KEY_ENTER: i32 = 58;
pub const GHOSTTY_KEY_META_LEFT: i32 = 59;
pub const GHOSTTY_KEY_META_RIGHT: i32 = 60;
pub const GHOSTTY_KEY_SHIFT_LEFT: i32 = 61;
pub const GHOSTTY_KEY_SHIFT_RIGHT: i32 = 62;
pub const GHOSTTY_KEY_SPACE: i32 = 63;
pub const GHOSTTY_KEY_TAB: i32 = 64;
pub const GHOSTTY_KEY_CONVERT: i32 = 65;
pub const GHOSTTY_KEY_KANA_MODE: i32 = 66;
pub const GHOSTTY_KEY_NON_CONVERT: i32 = 67;
// Control Pad (W3C 3.2)
pub const GHOSTTY_KEY_DELETE: i32 = 68;
pub const GHOSTTY_KEY_END: i32 = 69;
pub const GHOSTTY_KEY_HELP: i32 = 70;
pub const GHOSTTY_KEY_HOME: i32 = 71;
pub const GHOSTTY_KEY_INSERT: i32 = 72;
pub const GHOSTTY_KEY_PAGE_DOWN: i32 = 73;
pub const GHOSTTY_KEY_PAGE_UP: i32 = 74;
// Arrow Pad (W3C 3.3)
pub const GHOSTTY_KEY_ARROW_DOWN: i32 = 75;
pub const GHOSTTY_KEY_ARROW_LEFT: i32 = 76;
pub const GHOSTTY_KEY_ARROW_RIGHT: i32 = 77;
pub const GHOSTTY_KEY_ARROW_UP: i32 = 78;
// Numpad (W3C 3.4)
pub const GHOSTTY_KEY_NUM_LOCK: i32 = 79;
pub const GHOSTTY_KEY_NUMPAD_0: i32 = 80;
pub const GHOSTTY_KEY_NUMPAD_1: i32 = 81;
pub const GHOSTTY_KEY_NUMPAD_2: i32 = 82;
pub const GHOSTTY_KEY_NUMPAD_3: i32 = 83;
pub const GHOSTTY_KEY_NUMPAD_4: i32 = 84;
pub const GHOSTTY_KEY_NUMPAD_5: i32 = 85;
pub const GHOSTTY_KEY_NUMPAD_6: i32 = 86;
pub const GHOSTTY_KEY_NUMPAD_7: i32 = 87;
pub const GHOSTTY_KEY_NUMPAD_8: i32 = 88;
pub const GHOSTTY_KEY_NUMPAD_9: i32 = 89;
pub const GHOSTTY_KEY_NUMPAD_ADD: i32 = 90;
pub const GHOSTTY_KEY_NUMPAD_BACKSPACE: i32 = 91;
pub const GHOSTTY_KEY_NUMPAD_CLEAR: i32 = 92;
pub const GHOSTTY_KEY_NUMPAD_CLEAR_ENTRY: i32 = 93;
pub const GHOSTTY_KEY_NUMPAD_COMMA: i32 = 94;
pub const GHOSTTY_KEY_NUMPAD_DECIMAL: i32 = 95;
pub const GHOSTTY_KEY_NUMPAD_DIVIDE: i32 = 96;
pub const GHOSTTY_KEY_NUMPAD_ENTER: i32 = 97;
pub const GHOSTTY_KEY_NUMPAD_EQUAL: i32 = 98;
pub const GHOSTTY_KEY_NUMPAD_MEMORY_ADD: i32 = 99;
pub const GHOSTTY_KEY_NUMPAD_MEMORY_CLEAR: i32 = 100;
pub const GHOSTTY_KEY_NUMPAD_MEMORY_RECALL: i32 = 101;
pub const GHOSTTY_KEY_NUMPAD_MEMORY_STORE: i32 = 102;
pub const GHOSTTY_KEY_NUMPAD_MEMORY_SUBTRACT: i32 = 103;
pub const GHOSTTY_KEY_NUMPAD_MULTIPLY: i32 = 104;
pub const GHOSTTY_KEY_NUMPAD_PAREN_LEFT: i32 = 105;
pub const GHOSTTY_KEY_NUMPAD_PAREN_RIGHT: i32 = 106;
pub const GHOSTTY_KEY_NUMPAD_SUBTRACT: i32 = 107;
pub const GHOSTTY_KEY_NUMPAD_SEPARATOR: i32 = 108;
pub const GHOSTTY_KEY_NUMPAD_UP: i32 = 109;
pub const GHOSTTY_KEY_NUMPAD_DOWN: i32 = 110;
pub const GHOSTTY_KEY_NUMPAD_RIGHT: i32 = 111;
pub const GHOSTTY_KEY_NUMPAD_LEFT: i32 = 112;
pub const GHOSTTY_KEY_NUMPAD_BEGIN: i32 = 113;
pub const GHOSTTY_KEY_NUMPAD_HOME: i32 = 114;
pub const GHOSTTY_KEY_NUMPAD_END: i32 = 115;
pub const GHOSTTY_KEY_NUMPAD_INSERT: i32 = 116;
pub const GHOSTTY_KEY_NUMPAD_DELETE: i32 = 117;
pub const GHOSTTY_KEY_NUMPAD_PAGE_UP: i32 = 118;
pub const GHOSTTY_KEY_NUMPAD_PAGE_DOWN: i32 = 119;
// Function Section (W3C 3.5)
pub const GHOSTTY_KEY_ESCAPE: i32 = 120;
pub const GHOSTTY_KEY_F1: i32 = 121;
pub const GHOSTTY_KEY_F2: i32 = 122;
pub const GHOSTTY_KEY_F3: i32 = 123;
pub const GHOSTTY_KEY_F4: i32 = 124;
pub const GHOSTTY_KEY_F5: i32 = 125;
pub const GHOSTTY_KEY_F6: i32 = 126;
pub const GHOSTTY_KEY_F7: i32 = 127;
pub const GHOSTTY_KEY_F8: i32 = 128;
pub const GHOSTTY_KEY_F9: i32 = 129;
pub const GHOSTTY_KEY_F10: i32 = 130;
pub const GHOSTTY_KEY_F11: i32 = 131;
pub const GHOSTTY_KEY_F12: i32 = 132;
pub const GHOSTTY_KEY_F13: i32 = 133;
pub const GHOSTTY_KEY_F14: i32 = 134;
pub const GHOSTTY_KEY_F15: i32 = 135;
pub const GHOSTTY_KEY_F16: i32 = 136;
pub const GHOSTTY_KEY_F17: i32 = 137;
pub const GHOSTTY_KEY_F18: i32 = 138;
pub const GHOSTTY_KEY_F19: i32 = 139;
pub const GHOSTTY_KEY_F20: i32 = 140;
pub const GHOSTTY_KEY_F21: i32 = 141;
pub const GHOSTTY_KEY_F22: i32 = 142;
pub const GHOSTTY_KEY_F23: i32 = 143;
pub const GHOSTTY_KEY_F24: i32 = 144;
pub const GHOSTTY_KEY_F25: i32 = 145;
pub const GHOSTTY_KEY_FN: i32 = 146;
pub const GHOSTTY_KEY_FN_LOCK: i32 = 147;
pub const GHOSTTY_KEY_PRINT_SCREEN: i32 = 148;
pub const GHOSTTY_KEY_SCROLL_LOCK: i32 = 149;
pub const GHOSTTY_KEY_PAUSE: i32 = 150;
// Media & Browser Keys (W3C 3.6)
pub const GHOSTTY_KEY_BROWSER_BACK: i32 = 151;
pub const GHOSTTY_KEY_BROWSER_FAVORITES: i32 = 152;
pub const GHOSTTY_KEY_BROWSER_FORWARD: i32 = 153;
pub const GHOSTTY_KEY_BROWSER_HOME: i32 = 154;
pub const GHOSTTY_KEY_BROWSER_REFRESH: i32 = 155;
pub const GHOSTTY_KEY_BROWSER_SEARCH: i32 = 156;
pub const GHOSTTY_KEY_BROWSER_STOP: i32 = 157;
pub const GHOSTTY_KEY_EJECT: i32 = 158;
pub const GHOSTTY_KEY_LAUNCH_APP_1: i32 = 159;
pub const GHOSTTY_KEY_LAUNCH_APP_2: i32 = 160;
pub const GHOSTTY_KEY_LAUNCH_MAIL: i32 = 161;
pub const GHOSTTY_KEY_MEDIA_PLAY_PAUSE: i32 = 162;
pub const GHOSTTY_KEY_MEDIA_SELECT: i32 = 163;
pub const GHOSTTY_KEY_MEDIA_STOP: i32 = 164;
pub const GHOSTTY_KEY_MEDIA_TRACK_NEXT: i32 = 165;
pub const GHOSTTY_KEY_MEDIA_TRACK_PREVIOUS: i32 = 166;
pub const GHOSTTY_KEY_POWER: i32 = 167;
pub const GHOSTTY_KEY_SLEEP: i32 = 168;
pub const GHOSTTY_KEY_AUDIO_VOLUME_DOWN: i32 = 169;
pub const GHOSTTY_KEY_AUDIO_VOLUME_MUTE: i32 = 170;
pub const GHOSTTY_KEY_AUDIO_VOLUME_UP: i32 = 171;
pub const GHOSTTY_KEY_WAKE_UP: i32 = 172;
pub const GHOSTTY_KEY_COPY: i32 = 173;
pub const GHOSTTY_KEY_CUT: i32 = 174;
pub const GHOSTTY_KEY_PASTE: i32 = 175;

// ---------------------------------------------------------------------------
// Mouse encoder types
// ---------------------------------------------------------------------------

/// Opaque mouse encoder handle.
#[repr(C)]
pub struct GhosttyMouseEncoderOpaque {
    _private: [u8; 0],
}
pub type GhosttyMouseEncoder = *mut GhosttyMouseEncoderOpaque;

/// Opaque mouse event handle.
#[repr(C)]
pub struct GhosttyMouseEventOpaque {
    _private: [u8; 0],
}
pub type GhosttyMouseEvent = *mut GhosttyMouseEventOpaque;

/// Mouse action constants.
pub const GHOSTTY_MOUSE_ACTION_PRESS: i32 = 0;
pub const GHOSTTY_MOUSE_ACTION_RELEASE: i32 = 1;
pub const GHOSTTY_MOUSE_ACTION_MOTION: i32 = 2;

/// Mouse button constants.
pub const GHOSTTY_MOUSE_BUTTON_UNKNOWN: i32 = 0;
pub const GHOSTTY_MOUSE_BUTTON_LEFT: i32 = 1;
pub const GHOSTTY_MOUSE_BUTTON_RIGHT: i32 = 2;
pub const GHOSTTY_MOUSE_BUTTON_MIDDLE: i32 = 3;
pub const GHOSTTY_MOUSE_BUTTON_FOUR: i32 = 4;
pub const GHOSTTY_MOUSE_BUTTON_FIVE: i32 = 5;
pub const GHOSTTY_MOUSE_BUTTON_SIX: i32 = 6;
pub const GHOSTTY_MOUSE_BUTTON_SEVEN: i32 = 7;
pub const GHOSTTY_MOUSE_BUTTON_EIGHT: i32 = 8;
pub const GHOSTTY_MOUSE_BUTTON_NINE: i32 = 9;
pub const GHOSTTY_MOUSE_BUTTON_TEN: i32 = 10;
pub const GHOSTTY_MOUSE_BUTTON_ELEVEN: i32 = 11;

/// Mouse position (surface-space pixels).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyMousePosition {
    pub x: f32,
    pub y: f32,
}

/// Mouse encoder size context.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyMouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

impl GhosttyMouseEncoderSize {
    pub fn init_sized() -> Self {
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

/// Mouse encoder option constants.
pub const GHOSTTY_MOUSE_ENCODER_OPT_EVENT: i32 = 0;
pub const GHOSTTY_MOUSE_ENCODER_OPT_FORMAT: i32 = 1;
pub const GHOSTTY_MOUSE_ENCODER_OPT_SIZE: i32 = 2;
pub const GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED: i32 = 3;
pub const GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL: i32 = 4;

// ---------------------------------------------------------------------------
// Terminal scroll viewport types
// ---------------------------------------------------------------------------

/// Scroll viewport tag.
pub const GHOSTTY_SCROLL_VIEWPORT_TOP: i32 = 0;
pub const GHOSTTY_SCROLL_VIEWPORT_BOTTOM: i32 = 1;
pub const GHOSTTY_SCROLL_VIEWPORT_DELTA: i32 = 2;

/// Scroll viewport value union.
#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyTerminalScrollViewportValue {
    pub delta: isize,
    pub _padding: [u64; 2],
}

/// Scroll viewport tagged union.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyTerminalScrollViewport {
    pub tag: i32,
    pub value: GhosttyTerminalScrollViewportValue,
}

// ---------------------------------------------------------------------------
// Mode types
// ---------------------------------------------------------------------------

/// Packed 16-bit terminal mode (matches GhosttyMode from modes.h).
pub type GhosttyMode = u16;

/// Construct a GhosttyMode from a mode value and ANSI flag.
/// Matches the inline `ghostty_mode_new()` from modes.h.
pub const fn ghostty_mode_new(value: u16, ansi: bool) -> GhosttyMode {
    (value & 0x7FFF) | ((ansi as u16) << 15)
}

// Key mode constants used for queries.
pub const GHOSTTY_MODE_FOCUS_EVENT: GhosttyMode = ghostty_mode_new(1004, false);
pub const GHOSTTY_MODE_NORMAL_MOUSE: GhosttyMode = ghostty_mode_new(1000, false);
pub const GHOSTTY_MODE_BUTTON_MOUSE: GhosttyMode = ghostty_mode_new(1002, false);
pub const GHOSTTY_MODE_ANY_MOUSE: GhosttyMode = ghostty_mode_new(1003, false);
pub const GHOSTTY_MODE_X10_MOUSE: GhosttyMode = ghostty_mode_new(9, false);

// ---------------------------------------------------------------------------
// Focus types
// ---------------------------------------------------------------------------

pub const GHOSTTY_FOCUS_GAINED: i32 = 0;
pub const GHOSTTY_FOCUS_LOST: i32 = 1;

// Terminal data for mouse tracking query.
pub const GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN: i32 = 6;
pub const GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING: i32 = 11;

// ---------------------------------------------------------------------------
// Callback types for terminal effects
// ---------------------------------------------------------------------------

pub type GhosttyTerminalWritePtyFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
);

pub type GhosttyTerminalSizeFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    out_size: *mut GhosttySizeReportSize,
) -> bool;

pub type GhosttyTerminalDeviceAttributesFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    out_attrs: *mut GhosttyDeviceAttributes,
) -> bool;

pub type GhosttyTerminalXtversionFn =
    unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void) -> GhosttyString;

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

    // -----------------------------------------------------------------------
    // Key encoder
    // -----------------------------------------------------------------------

    pub fn ghostty_key_encoder_new(
        allocator: *const c_void,
        encoder: *mut GhosttyKeyEncoder,
    ) -> GhosttyResult;

    pub fn ghostty_key_encoder_free(encoder: GhosttyKeyEncoder);

    pub fn ghostty_key_encoder_setopt(
        encoder: GhosttyKeyEncoder,
        option: i32,
        value: *const c_void,
    );

    pub fn ghostty_key_encoder_setopt_from_terminal(
        encoder: GhosttyKeyEncoder,
        terminal: GhosttyTerminal,
    );

    pub fn ghostty_key_encoder_encode(
        encoder: GhosttyKeyEncoder,
        event: GhosttyKeyEvent,
        out_buf: *mut u8,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    // Key event
    pub fn ghostty_key_event_new(
        allocator: *const c_void,
        event: *mut GhosttyKeyEvent,
    ) -> GhosttyResult;

    pub fn ghostty_key_event_free(event: GhosttyKeyEvent);

    pub fn ghostty_key_event_set_action(event: GhosttyKeyEvent, action: i32);

    pub fn ghostty_key_event_set_key(event: GhosttyKeyEvent, key: i32);

    pub fn ghostty_key_event_set_mods(event: GhosttyKeyEvent, mods: GhosttyMods);

    pub fn ghostty_key_event_set_consumed_mods(event: GhosttyKeyEvent, consumed_mods: GhosttyMods);

    pub fn ghostty_key_event_set_composing(event: GhosttyKeyEvent, composing: bool);

    pub fn ghostty_key_event_set_utf8(event: GhosttyKeyEvent, utf8: *const u8, len: usize);

    pub fn ghostty_key_event_set_unshifted_codepoint(event: GhosttyKeyEvent, codepoint: u32);

    // -----------------------------------------------------------------------
    // Mouse encoder
    // -----------------------------------------------------------------------

    pub fn ghostty_mouse_encoder_new(
        allocator: *const c_void,
        encoder: *mut GhosttyMouseEncoder,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_encoder_free(encoder: GhosttyMouseEncoder);

    pub fn ghostty_mouse_encoder_setopt(
        encoder: GhosttyMouseEncoder,
        option: i32,
        value: *const c_void,
    );

    pub fn ghostty_mouse_encoder_setopt_from_terminal(
        encoder: GhosttyMouseEncoder,
        terminal: GhosttyTerminal,
    );

    pub fn ghostty_mouse_encoder_encode(
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
        out_buf: *mut u8,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_encoder_reset(encoder: GhosttyMouseEncoder);

    // Mouse event
    pub fn ghostty_mouse_event_new(
        allocator: *const c_void,
        event: *mut GhosttyMouseEvent,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_event_free(event: GhosttyMouseEvent);

    pub fn ghostty_mouse_event_set_action(event: GhosttyMouseEvent, action: i32);

    pub fn ghostty_mouse_event_set_button(event: GhosttyMouseEvent, button: i32);

    pub fn ghostty_mouse_event_clear_button(event: GhosttyMouseEvent);

    pub fn ghostty_mouse_event_set_mods(event: GhosttyMouseEvent, mods: GhosttyMods);

    pub fn ghostty_mouse_event_set_position(
        event: GhosttyMouseEvent,
        position: GhosttyMousePosition,
    );

    // -----------------------------------------------------------------------
    // Terminal control (scroll, reset, modes)
    // -----------------------------------------------------------------------

    pub fn ghostty_terminal_scroll_viewport(
        terminal: GhosttyTerminal,
        behavior: GhosttyTerminalScrollViewport,
    );

    pub fn ghostty_terminal_reset(terminal: GhosttyTerminal);

    pub fn ghostty_terminal_mode_get(
        terminal: GhosttyTerminal,
        mode: GhosttyMode,
        out_value: *mut bool,
    ) -> GhosttyResult;

    pub fn ghostty_terminal_mode_set(
        terminal: GhosttyTerminal,
        mode: GhosttyMode,
        value: bool,
    ) -> GhosttyResult;

    // -----------------------------------------------------------------------
    // Focus encoding
    // -----------------------------------------------------------------------

    pub fn ghostty_focus_encode(
        event: i32,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
}
