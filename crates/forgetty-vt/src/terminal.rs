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
    /// Whether the scrollback cache needs rebuilding.
    scrollback_dirty: bool,
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
                scrollback_dirty: true,
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
        // Mark caches as dirty
        let cache = self.cache.get_mut();
        cache.screen_dirty = true;
        cache.scrollback_dirty = true;
    }

    /// Get the current screen state.
    ///
    /// Lazily updates the render state from the terminal and rebuilds the
    /// cached cell grid for any dirty rows.
    pub fn screen(&self) -> &Screen {
        // Safety: Terminal is !Sync so no concurrent access is possible.
        let cache = unsafe { &mut *self.cache.get() };
        if cache.screen_dirty {
            self.sync_screen(cache);
            cache.screen_dirty = false;
        }
        &cache.screen
    }

    /// Get cursor position as (row, col).
    pub fn cursor(&self) -> (usize, usize) {
        let mut x: u16 = 0;
        let mut y: u16 = 0;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_CURSOR_Y,
                &mut y as *mut u16 as *mut c_void,
            );
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_CURSOR_X,
                &mut x as *mut u16 as *mut c_void,
            );
        }
        (y as usize, x as usize)
    }

    /// Is the cursor visible?
    pub fn cursor_visible(&self) -> bool {
        let mut visible: bool = true;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE,
                &mut visible as *mut bool as *mut c_void,
            );
        }
        visible
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
        cache.scrollback_dirty = true;
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
    pub fn scrollback(&self) -> &[Vec<Cell>] {
        // Safety: Terminal is !Sync so no concurrent access is possible.
        let cache = unsafe { &mut *self.cache.get() };
        if cache.scrollback_dirty {
            self.rebuild_scrollback_cache(cache);
            cache.scrollback_dirty = false;
        }
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

        // Populate the row iterator from the render state
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                self.row_iter as *mut c_void,
            );
        }

        // Build the cell grid
        let mut grid: Vec<Vec<Cell>> = Vec::with_capacity(num_rows);
        let mut dirty_rows: Vec<bool> = Vec::with_capacity(num_rows);

        while unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iter) } {
            // Check row dirty state
            let mut row_dirty: bool = false;
            unsafe {
                ffi::ghostty_render_state_row_get(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                    &mut row_dirty as *mut bool as *mut c_void,
                );
            }
            dirty_rows.push(row_dirty);

            // Populate cell iterator
            unsafe {
                ffi::ghostty_render_state_row_get(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    self.row_cells as *mut c_void,
                );
            }

            let mut row_cells_vec: Vec<Cell> = Vec::with_capacity(num_cols);

            while unsafe { ffi::ghostty_render_state_row_cells_next(self.row_cells) } {
                // Get raw cell
                let mut raw_cell: ffi::GhosttyCell = 0;
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                        &mut raw_cell as *mut ffi::GhosttyCell as *mut c_void,
                    );
                }

                // Get codepoint
                let mut codepoint: u32 = 0;
                unsafe {
                    ffi::ghostty_cell_get(
                        raw_cell,
                        ffi::GHOSTTY_CELL_DATA_CODEPOINT,
                        &mut codepoint as *mut u32 as *mut c_void,
                    );
                }

                let character =
                    if codepoint == 0 { ' ' } else { char::from_u32(codepoint).unwrap_or(' ') };

                // Get style
                let mut style = ffi::GhosttyStyle::init_sized();
                unsafe {
                    ffi::ghostty_render_state_row_cells_get(
                        self.row_cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                        &mut style as *mut ffi::GhosttyStyle as *mut c_void,
                    );
                }

                let attrs = CellAttributes {
                    fg: style_color_to_color(&style.fg_color),
                    bg: style_color_to_color(&style.bg_color),
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != 0,
                    strikethrough: style.strikethrough,
                    inverse: style.inverse,
                    dim: style.faint,
                };

                row_cells_vec.push(Cell { character, attrs });
            }

            // Pad row to expected width if needed
            while row_cells_vec.len() < num_cols {
                row_cells_vec.push(Cell::default());
            }
            row_cells_vec.truncate(num_cols);

            grid.push(row_cells_vec);
        }

        // Pad to expected number of rows
        while grid.len() < num_rows {
            grid.push((0..num_cols).map(|_| Cell::default()).collect());
            dirty_rows.push(true);
        }
        grid.truncate(num_rows);
        dirty_rows.truncate(num_rows);

        // Fallback: if render state row iteration produced no content,
        // read directly from the terminal via grid_ref.
        let has_content =
            grid.iter().any(|row| row.iter().any(|c| c.character != ' ' && c.character != '\0'));

        if !has_content && num_rows > 0 && num_cols > 0 {
            tracing::debug!("render state yielded no content, falling back to grid_ref");
            grid = self.read_grid_via_grid_ref(num_rows, num_cols);
            dirty_rows = vec![true; grid.len()];
        }

        // Replace grid in screen
        cache.screen.replace_from_grid(grid, &dirty_rows);

        // Reset render state dirty flag so we don't re-process
        let clean = ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        unsafe {
            ffi::ghostty_render_state_set(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                &clean as *const i32 as *const c_void,
            );
        }

        // Also reset per-row dirty flags
        unsafe {
            ffi::ghostty_render_state_get(
                self.render_state,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                self.row_iter as *mut c_void,
            );
            let false_val: bool = false;
            while ffi::ghostty_render_state_row_iterator_next(self.row_iter) {
                ffi::ghostty_render_state_row_set(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                    &false_val as *const bool as *const c_void,
                );
            }
        }
    }

    /// Read the terminal grid directly via grid_ref (fallback when render state is empty).
    fn read_grid_via_grid_ref(&self, num_rows: usize, num_cols: usize) -> Vec<Vec<Cell>> {
        let mut grid = Vec::with_capacity(num_rows);

        for row in 0..num_rows {
            let mut row_cells = Vec::with_capacity(num_cols);
            for col in 0..num_cols {
                let mut grid_ref = ffi::GhosttyGridRef {
                    size: std::mem::size_of::<ffi::GhosttyGridRef>(),
                    node: std::ptr::null_mut(),
                    x: col as u16,
                    y: row as u16,
                };

                let point = ffi::GhosttyPoint {
                    tag: ffi::GHOSTTY_POINT_TAG_ACTIVE,
                    value: ffi::GhosttyPointValue {
                        coordinate: ffi::GhosttyPointCoordinate { x: col as u16, y: row as u32 },
                    },
                };

                let rc =
                    unsafe { ffi::ghostty_terminal_grid_ref(self.handle, point, &mut grid_ref) };

                if rc != ffi::GHOSTTY_SUCCESS {
                    row_cells.push(Cell::default());
                    continue;
                }

                // Get codepoint
                let mut raw_cell: ffi::GhosttyCell = 0;
                let rc = unsafe { ffi::ghostty_grid_ref_cell(&grid_ref, &mut raw_cell) };
                if rc != ffi::GHOSTTY_SUCCESS {
                    row_cells.push(Cell::default());
                    continue;
                }

                let mut codepoint: u32 = 0;
                unsafe {
                    ffi::ghostty_cell_get(
                        raw_cell,
                        ffi::GHOSTTY_CELL_DATA_CODEPOINT,
                        &mut codepoint as *mut u32 as *mut c_void,
                    );
                }

                let character =
                    if codepoint == 0 { ' ' } else { char::from_u32(codepoint).unwrap_or(' ') };

                // Get style
                let mut style = ffi::GhosttyStyle::init_sized();
                let rc = unsafe { ffi::ghostty_grid_ref_style(&grid_ref, &mut style) };
                let attrs = if rc == ffi::GHOSTTY_SUCCESS {
                    CellAttributes {
                        fg: style_color_to_color(&style.fg_color),
                        bg: style_color_to_color(&style.bg_color),
                        bold: style.bold,
                        italic: style.italic,
                        underline: style.underline != 0,
                        strikethrough: style.strikethrough,
                        inverse: style.inverse,
                        dim: style.faint,
                    }
                } else {
                    CellAttributes::default()
                };

                row_cells.push(Cell { character, attrs });
            }
            grid.push(row_cells);
        }

        grid
    }

    /// Rebuild the scrollback cache by reading history lines via grid_ref.
    fn rebuild_scrollback_cache(&self, cache: &mut Cache) {
        let mut scrollback_rows: usize = 0;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS,
                &mut scrollback_rows as *mut usize as *mut c_void,
            );
        }

        if scrollback_rows == 0 {
            cache.scrollback.clear();
            return;
        }

        // Get terminal cols
        let mut t_cols: u16 = 0;
        unsafe {
            ffi::ghostty_terminal_get(
                self.handle,
                ffi::GHOSTTY_TERMINAL_DATA_COLS,
                &mut t_cols as *mut u16 as *mut c_void,
            );
        }
        let cols = t_cols as usize;

        let mut new_scrollback = Vec::with_capacity(scrollback_rows);

        for y in 0..scrollback_rows {
            let mut row_cells = Vec::with_capacity(cols);

            for x in 0..cols {
                let point = ffi::GhosttyPoint {
                    tag: ffi::GHOSTTY_POINT_TAG_HISTORY,
                    value: ffi::GhosttyPointValue {
                        coordinate: ffi::GhosttyPointCoordinate { x: x as u16, y: y as u32 },
                    },
                };

                let mut grid_ref = ffi::GhosttyGridRef::init_sized();
                let rc =
                    unsafe { ffi::ghostty_terminal_grid_ref(self.handle, point, &mut grid_ref) };

                if rc != ffi::GHOSTTY_SUCCESS {
                    row_cells.push(Cell::default());
                    continue;
                }

                // Get graphemes
                let mut buf = [0u32; 16];
                let mut out_len: usize = 0;
                let rc = unsafe {
                    ffi::ghostty_grid_ref_graphemes(
                        &grid_ref,
                        buf.as_mut_ptr(),
                        buf.len(),
                        &mut out_len,
                    )
                };

                let character = if rc == ffi::GHOSTTY_SUCCESS && out_len > 0 {
                    char::from_u32(buf[0]).unwrap_or(' ')
                } else {
                    ' '
                };

                // Get style
                let mut style = ffi::GhosttyStyle::init_sized();
                unsafe {
                    ffi::ghostty_grid_ref_style(&grid_ref, &mut style);
                }

                let attrs = CellAttributes {
                    fg: style_color_to_color(&style.fg_color),
                    bg: style_color_to_color(&style.bg_color),
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != 0,
                    strikethrough: style.strikethrough,
                    inverse: style.inverse,
                    dim: style.faint,
                };

                row_cells.push(Cell { character, attrs });
            }

            new_scrollback.push(row_cells);
        }

        cache.scrollback = new_scrollback;
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
// Color conversion
// ---------------------------------------------------------------------------

fn style_color_to_color(sc: &ffi::GhosttyStyleColor) -> Color {
    match sc.tag {
        ffi::GhosttyStyleColorTag::None => Color::Default,
        ffi::GhosttyStyleColorTag::Palette => Color::Indexed(unsafe { sc.value.palette }),
        ffi::GhosttyStyleColorTag::Rgb => {
            let rgb = unsafe { sc.value.rgb };
            Color::Rgb(rgb.r, rgb.g, rgb.b)
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
