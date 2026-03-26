//! High-level terminal state machine.
//!
//! Wraps a libghostty-vt terminal handle and provides a safe, ergonomic Rust
//! interface for feeding input data and querying terminal state. The public
//! API matches the original `vte`-based implementation so that downstream
//! crates (forgetty-renderer, forgetty-ui) continue to compile unchanged.

use std::cell::UnsafeCell;
use std::os::raw::c_void;

use crate::ffi;
use crate::screen::{Cell, CellAttributes, Color, Screen};

/// Events emitted by the terminal during parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    /// The terminal title changed (via OSC 0 or OSC 2).
    TitleChanged(String),
    /// The terminal bell was triggered.
    Bell,
    /// The terminal requested a mode change (e.g., alt screen).
    ModeChanged,
}

/// Per-terminal userdata carried through C callbacks.
struct CallbackState {
    events: Vec<TerminalEvent>,
}

/// Interior-mutable cache that allows `screen()`, `title()`, and `scrollback()`
/// to operate behind `&self`.
struct Cache {
    screen: Screen,
    title_buf: String,
    scrollback: Vec<Vec<Cell>>,
    /// Whether the screen cache needs rebuilding.
    screen_dirty: bool,
}

/// A virtual terminal that processes VT escape sequences and maintains screen state.
///
/// Backed by libghostty-vt via FFI.
pub struct Terminal {
    handle: ffi::GhosttyTerminal,
    render_state: ffi::GhosttyRenderState,
    row_iter: ffi::GhosttyRenderStateRowIterator,
    row_cells: ffi::GhosttyRenderStateRowCells,
    /// Interior-mutable cache for screen/title/scrollback.
    cache: UnsafeCell<Cache>,
    /// Events collected from callbacks.
    callback_state: Box<CallbackState>,
    /// Terminal dimensions (tracked locally for convenience).
    rows: usize,
    cols: usize,
}

// Safety: Terminal is not Sync (UnsafeCell prevents that), and we only access
// the cache from &self methods that cannot be called concurrently. The FFI
// handles are exclusively owned.
unsafe impl Send for Terminal {}

impl Terminal {
    /// Create a new terminal with the given dimensions.
    pub fn new(rows: usize, cols: usize) -> Self {
        let mut handle: ffi::GhosttyTerminal = std::ptr::null_mut();
        let opts = ffi::GhosttyTerminalOptions {
            cols: cols as u16,
            rows: rows as u16,
            max_scrollback: 10_000,
        };
        let rc = unsafe { ffi::ghostty_terminal_new(std::ptr::null(), &mut handle, opts) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_terminal_new failed: {rc}");

        let mut render_state: ffi::GhosttyRenderState = std::ptr::null_mut();
        let rc = unsafe { ffi::ghostty_render_state_new(std::ptr::null(), &mut render_state) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_render_state_new failed: {rc}");

        let mut row_iter: ffi::GhosttyRenderStateRowIterator = std::ptr::null_mut();
        let rc =
            unsafe { ffi::ghostty_render_state_row_iterator_new(std::ptr::null(), &mut row_iter) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "row_iterator_new failed: {rc}");

        let mut row_cells: ffi::GhosttyRenderStateRowCells = std::ptr::null_mut();
        let rc =
            unsafe { ffi::ghostty_render_state_row_cells_new(std::ptr::null(), &mut row_cells) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "row_cells_new failed: {rc}");

        let callback_state = Box::new(CallbackState { events: Vec::new() });

        // NOTE: Callbacks are disabled for now. Registering them caused a
        // segfault, likely due to FFI calling convention mismatches. The
        // terminal works fine without callbacks — we just poll for title
        // changes manually. TODO: investigate the correct way to pass
        // function pointers via ghostty_terminal_set().

        Terminal {
            handle,
            render_state,
            row_iter,
            row_cells,
            cache: UnsafeCell::new(Cache {
                screen: Screen::new(rows, cols),
                title_buf: String::new(),
                scrollback: Vec::new(),
                screen_dirty: true,
            }),
            callback_state,
            rows,
            cols,
        }
    }

    /// Feed raw bytes from the PTY into the terminal parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        unsafe {
            ffi::ghostty_terminal_vt_write(self.handle, bytes.as_ptr(), bytes.len());
        }
        // Mark cache as dirty
        let cache = self.cache.get_mut();
        cache.screen_dirty = true;
    }

    /// Get the current screen state.
    ///
    /// Always snapshots the render state from the terminal (like Ghostling
    /// line 1272: `ghostty_render_state_update` every frame). Then rebuilds
    /// the cached cell grid if the render state reports changes.
    pub fn screen(&self) -> &Screen {
        // Safety: Terminal is !Sync so no concurrent access is possible.
        let cache = unsafe { &mut *self.cache.get() };
        // Always snapshot — this is cheap and must happen every frame
        // (Ghostling calls render_state_update unconditionally each frame)
        self.sync_screen(cache);
        cache.screen_dirty = false;
        &cache.screen
    }

    /// Get cursor position as (row, col) from the render state viewport cursor.
    pub fn cursor(&self) -> (usize, usize) {
        let mut has_value: bool = false;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
                &mut has_value as *mut bool as *mut c_void,
            );
        }
        if !has_value {
            return (0, 0);
        }

        let mut x: u16 = 0;
        let mut y: u16 = 0;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y,
                &mut y as *mut u16 as *mut c_void,
            );
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X,
                &mut x as *mut u16 as *mut c_void,
            );
        }
        (y as usize, x as usize)
    }

    /// Is the cursor visible?
    pub fn cursor_visible(&self) -> bool {
        let mut visible: bool = true;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE,
                &mut visible as *mut bool as *mut c_void,
            );
        }

        if !visible {
            return false;
        }

        // Also check if cursor is in the viewport
        let mut has_value: bool = false;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
                &mut has_value as *mut bool as *mut c_void,
            );
        }
        has_value
    }

    /// Resize the terminal to new dimensions.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        unsafe {
            ffi::ghostty_terminal_resize(
                self.handle,
                cols as u16,
                rows as u16,
                8,  // default cell width
                16, // default cell height
            );
        }
        let cache = self.cache.get_mut();
        cache.screen.resize(rows, cols);
        cache.screen_dirty = true;
    }

    /// Get the terminal title (set via OSC).
    pub fn title(&self) -> &str {
        // Safety: Terminal is !Sync so no concurrent access is possible.
        let cache = unsafe { &mut *self.cache.get() };
        let mut gs = ffi::GhosttyString { ptr: std::ptr::null(), len: 0 };
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_TITLE,
                &mut gs as *mut ffi::GhosttyString as *mut c_void,
            )
        };
        if rc == ffi::GHOSTTY_SUCCESS && !gs.ptr.is_null() && gs.len > 0 {
            let bytes = unsafe { std::slice::from_raw_parts(gs.ptr, gs.len) };
            cache.title_buf = String::from_utf8_lossy(bytes).into_owned();
        } else {
            cache.title_buf.clear();
        }
        &cache.title_buf
    }

    /// Drain pending events.
    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.callback_state.events)
    }

    /// Get scrollback lines.
    ///
    /// Scrollback is currently not implemented via the render state API.
    /// It will be re-implemented using the viewport scroll API in a future change.
    pub fn scrollback(&self) -> &[Vec<Cell>] {
        let cache = unsafe { &*self.cache.get() };
        &cache.scrollback
    }

    /// Total lines: scrollback + visible rows.
    pub fn total_lines(&self) -> usize {
        let mut total: usize = 0;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_TOTAL_ROWS,
                &mut total as *mut usize as *mut c_void,
            );
        }
        total
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Synchronize the cached Screen from the libghostty-vt render state.
    ///
    /// This follows the exact same API call sequence as Ghostling's
    /// `render_terminal()` (main.c lines 647-833).
    ///
    /// # Safety
    /// Caller must ensure exclusive access to `cache` (which is guaranteed
    /// because Terminal is !Sync).
    fn sync_screen(&self, cache: &mut Cache) {
        // Update render state from terminal
        let rc = unsafe { ffi::ghostty_render_state_update(self.render_state, self.handle) };
        if rc != ffi::GHOSTTY_SUCCESS {
            tracing::warn!("ghostty_render_state_update failed: {rc}");
            return;
        }

        // Check dirty state — but always extract on the first call (generation == 0)
        let mut dirty: i32 = ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_DIRTY,
                &mut dirty as *mut i32 as *mut c_void,
            );
        }

        let first_sync = cache.screen.generation() == 0;
        tracing::trace!("sync_screen: dirty={dirty}, first_sync={first_sync}");

        if dirty == ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE && !first_sync {
            return;
        }

        // 1. Get colors from render state (matches Ghostling line 657-659)
        let mut colors = ffi::GhosttyRenderStateColors::init_sized();
        let rc = unsafe { ffi::ghostty_render_state_colors_get(self.render_state, &mut colors) };
        if rc != ffi::GHOSTTY_SUCCESS {
            tracing::warn!("ghostty_render_state_colors_get failed: {rc}");
            return;
        }

        // Get dimensions from render state
        let mut rs_cols: u16 = 0;
        let mut rs_rows: u16 = 0;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_COLS,
                &mut rs_cols as *mut u16 as *mut c_void,
            );
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROWS,
                &mut rs_rows as *mut u16 as *mut c_void,
            );
        }

        let num_rows = rs_rows as usize;
        let num_cols = rs_cols as usize;

        // 2. Populate the row iterator from the render state (matches Ghostling line 662-663)
        //
        // IMPORTANT: pass a pointer TO the handle (`&mut self.row_iter`), not the handle
        // itself. The C API dereferences `out` to obtain the pre-allocated iterator handle,
        // then writes row data through it. Passing the handle value directly corrupts
        // the iterator's internal memory and causes intermittent segfaults.
        let mut row_iter = self.row_iter;
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut row_iter as *mut ffi::GhosttyRenderStateRowIterator as *mut c_void,
            );
        }

        // 3. Build the cell grid by iterating rows (matches Ghostling line 670)
        let mut grid: Vec<Vec<Cell>> = Vec::with_capacity(num_rows);
        let mut dirty_rows: Vec<bool> = Vec::with_capacity(num_rows);

        while unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iter) } {
            // Get cells for this row (matches Ghostling line 672-673)
            //
            // Same pointer-to-handle pattern as row_iterator above: pass
            // `&mut row_cells` so the C code can dereference to get the handle.
            let mut row_cells = self.row_cells;
            unsafe {
                ffi::ghostty_render_state_row_get(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    &mut row_cells as *mut ffi::GhosttyRenderStateRowCells as *mut c_void,
                );
            }

            let mut row_cells_vec: Vec<Cell> = Vec::with_capacity(num_cols);

            // 4. Iterate cells (matches Ghostling line 678)
            while unsafe { ffi::ghostty_render_state_row_cells_next(self.row_cells) } {
                // 5. Get grapheme length (matches Ghostling line 680-682)
                let mut grapheme_len: u32 = 0;
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                        &mut grapheme_len as *mut u32 as *mut c_void,
                    );
                }

                if grapheme_len == 0 {
                    // Empty cell — check for BG color (matches Ghostling line 684-698)
                    let mut bg_rgb = ffi::GhosttyColorRgb::default();
                    let has_bg = unsafe {
                        ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                            &mut bg_rgb as *mut ffi::GhosttyColorRgb as *mut c_void,
                        )
                    } == ffi::GHOSTTY_SUCCESS;

                    let bg = if has_bg {
                        Color::Rgb(bg_rgb.r, bg_rgb.g, bg_rgb.b)
                    } else {
                        Color::Default
                    };

                    row_cells_vec.push(Cell {
                        grapheme: " ".to_string(),
                        attrs: CellAttributes { bg, ..CellAttributes::default() },
                    });
                    continue;
                }

                // 6. Get grapheme codepoints (matches Ghostling line 702-705)
                let mut codepoints = [0u32; 16];
                let len = (grapheme_len as usize).min(16);
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                        codepoints.as_mut_ptr() as *mut c_void,
                    );
                }

                // Convert codepoints to String
                let grapheme: String =
                    codepoints[..len].iter().filter_map(|&cp| char::from_u32(cp)).collect();
                let grapheme = if grapheme.is_empty() { " ".to_string() } else { grapheme };

                // 7. Get resolved FG color (matches Ghostling line 723-725)
                let mut fg_rgb = colors.foreground;
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                        &mut fg_rgb as *mut ffi::GhosttyColorRgb as *mut c_void,
                    );
                }

                // 8. Get resolved BG color (matches Ghostling line 727-729)
                let mut bg_rgb = colors.background;
                let has_bg = unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                        &mut bg_rgb as *mut ffi::GhosttyColorRgb as *mut c_void,
                    )
                } == ffi::GHOSTTY_SUCCESS;

                // 9. Get style for boolean flags (matches Ghostling line 733-735)
                let mut style = ffi::GhosttyStyle::init_sized();
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                        &mut style as *mut ffi::GhosttyStyle as *mut c_void,
                    );
                }

                // 10. Handle inverse by swapping fg/bg (matches Ghostling line 738-743)
                let (final_fg, final_bg, final_has_bg) =
                    if style.inverse { (bg_rgb, fg_rgb, true) } else { (fg_rgb, bg_rgb, has_bg) };

                let fg = Color::Rgb(final_fg.r, final_fg.g, final_fg.b);
                let bg = if final_has_bg {
                    Color::Rgb(final_bg.r, final_bg.g, final_bg.b)
                } else {
                    Color::Default
                };

                let attrs = CellAttributes {
                    fg,
                    bg,
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != 0,
                    strikethrough: style.strikethrough,
                    inverse: false, // Already handled above via color swap
                    dim: style.faint,
                };

                row_cells_vec.push(Cell { grapheme, attrs });
            }

            // Pad row to expected width if needed
            while row_cells_vec.len() < num_cols {
                row_cells_vec.push(Cell::default());
            }
            row_cells_vec.truncate(num_cols);

            grid.push(row_cells_vec);

            // 11. Clear per-row dirty flag immediately (matches Ghostling line 770-772)
            let clean: bool = false;
            unsafe {
                ffi::ghostty_render_state_row_set(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                    &clean as *const bool as *const c_void,
                );
            }

            dirty_rows.push(true);
        }

        // Pad to expected number of rows
        while grid.len() < num_rows {
            grid.push((0..num_cols).map(|_| Cell::default()).collect());
            dirty_rows.push(true);
        }
        grid.truncate(num_rows);
        dirty_rows.truncate(num_rows);

        // Replace grid in screen
        cache.screen.replace_from_grid(grid, &dirty_rows);

        // 12. Clear global dirty flag (matches Ghostling line 830-832)
        let clean = ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        unsafe {
            ffi::ghostty_render_state_set(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                &clean as *const i32 as *const c_void,
            );
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_render_state_row_cells_free(self.row_cells);
            ffi::ghostty_render_state_row_iterator_free(self.row_iter);
            ffi::ghostty_render_state_free(self.render_state);
            ffi::ghostty_terminal_free(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// C callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn bell_callback(_terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    let state = unsafe { &mut *(userdata as *mut CallbackState) };
    state.events.push(TerminalEvent::Bell);
}

unsafe extern "C" fn title_changed_callback(terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    let state = unsafe { &mut *(userdata as *mut CallbackState) };

    // Read the title from the terminal
    let mut gs = ffi::GhosttyString { ptr: std::ptr::null(), len: 0 };
    let rc = unsafe {
        ffi::ghostty_terminal_get(
            terminal,
            ffi::GHOSTTY_TERMINAL_DATA_TITLE,
            &mut gs as *mut ffi::GhosttyString as *mut c_void,
        )
    };

    let title = if rc == ffi::GHOSTTY_SUCCESS && !gs.ptr.is_null() && gs.len > 0 {
        let bytes = unsafe { std::slice::from_raw_parts(gs.ptr, gs.len) };
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        String::new()
    };

    state.events.push(TerminalEvent::TitleChanged(title));
}
