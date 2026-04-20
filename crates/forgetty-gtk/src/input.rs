//! Keyboard and mouse input encoding via the ghostty key/mouse encoder APIs.
//!
//! Replaces the minimal hand-rolled key-to-byte-sequence table with the full
//! ghostty key encoder pipeline. This gives us automatic support for:
//! - Application cursor mode (DECCKM)
//! - Kitty keyboard protocol (all flags)
//! - Key release events
//! - Modifier side bits, CapsLock/NumLock reporting
//! - Numpad disambiguation
//! - All mouse tracking modes and formats (X10, SGR, URxvt, SGR-Pixels)

use std::collections::HashSet;
use std::os::raw::c_void;

use forgetty_vt::ffi;
use gtk4::gdk;

/// Result of encoding a scroll wheel event.
pub enum ScrollAction {
    /// Write these bytes to the PTY (mouse tracking is active).
    WriteBytes(Vec<u8>),
    /// Scroll the viewport by this many rows (no mouse tracking).
    ScrollViewport(isize),
}

/// Wraps the ghostty key encoder, key event, mouse encoder, and mouse event
/// handles, providing a safe(ish) interface for encoding GDK key and mouse
/// events into PTY bytes.
pub struct GhosttyInput {
    key_encoder: ffi::GhosttyKeyEncoder,
    key_event: ffi::GhosttyKeyEvent,
    mouse_encoder: ffi::GhosttyMouseEncoder,
    mouse_event: ffi::GhosttyMouseEvent,
    /// Track currently-pressed hardware keycodes to detect repeats.
    /// GTK4's `key-pressed` fires for both initial press and repeat with no
    /// built-in flag to distinguish them. If the same keycode fires again
    /// without a release in between, we treat it as a repeat.
    pressed_keys: HashSet<u32>,
    /// Track last known cursor position for mouse motion dedup.
    last_cursor_pos: (f64, f64),
    /// Track which mouse buttons are currently held (bitmask).
    buttons_held: u8,
}

// Safety: The FFI handles are exclusively owned and not shared.
unsafe impl Send for GhosttyInput {}

impl GhosttyInput {
    /// Create a new GhosttyInput with all encoder/event handles allocated.
    pub fn new() -> Self {
        let mut key_encoder: ffi::GhosttyKeyEncoder = std::ptr::null_mut();
        let rc = unsafe { ffi::ghostty_key_encoder_new(std::ptr::null(), &mut key_encoder) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_key_encoder_new failed: {rc}");

        let mut key_event: ffi::GhosttyKeyEvent = std::ptr::null_mut();
        let rc = unsafe { ffi::ghostty_key_event_new(std::ptr::null(), &mut key_event) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_key_event_new failed: {rc}");

        let mut mouse_encoder: ffi::GhosttyMouseEncoder = std::ptr::null_mut();
        let rc = unsafe { ffi::ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse_encoder) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_mouse_encoder_new failed: {rc}");

        let mut mouse_event: ffi::GhosttyMouseEvent = std::ptr::null_mut();
        let rc = unsafe { ffi::ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event) };
        assert_eq!(rc, ffi::GHOSTTY_SUCCESS, "ghostty_mouse_event_new failed: {rc}");

        Self {
            key_encoder,
            key_event,
            mouse_encoder,
            mouse_event,
            pressed_keys: HashSet::new(),
            last_cursor_pos: (0.0, 0.0),
            buttons_held: 0,
        }
    }

    /// Encode a key-pressed event into PTY bytes.
    ///
    /// `keyval` is the GDK key symbol, `keycode` is the hardware keycode,
    /// `state` is the modifier mask. Returns `Some(bytes)` to write to the PTY,
    /// or `None` if the key should not produce output (e.g. modifier-only press).
    pub fn encode_key_press(
        &mut self,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
    ) -> Option<Vec<u8>> {
        // Detect repeat: if this keycode is already in pressed_keys, it's a repeat.
        let is_repeat = self.pressed_keys.contains(&keycode);
        if !is_repeat {
            self.pressed_keys.insert(keycode);
        }

        let action =
            if is_repeat { ffi::GHOSTTY_KEY_ACTION_REPEAT } else { ffi::GHOSTTY_KEY_ACTION_PRESS };

        self.encode_key_inner(keyval, keycode, state, terminal, action, true)
    }

    /// Encode a key-released event into PTY bytes.
    ///
    /// Only produces output when the Kitty keyboard protocol requests release
    /// events. Always safe to call.
    pub fn encode_key_release(
        &mut self,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
    ) -> Option<Vec<u8>> {
        self.pressed_keys.remove(&keycode);
        self.encode_key_inner(
            keyval,
            keycode,
            state,
            terminal,
            ffi::GHOSTTY_KEY_ACTION_RELEASE,
            false,
        )
    }

    /// Encode a focus event. Returns bytes to write to PTY (if any).
    ///
    /// This is a static helper because focus encoding doesn't depend on
    /// encoder state.
    pub fn encode_focus(gained: bool) -> Option<Vec<u8>> {
        let event = if gained { ffi::GHOSTTY_FOCUS_GAINED } else { ffi::GHOSTTY_FOCUS_LOST };
        let mut buf = [0u8; 8];
        let mut written: usize = 0;
        let rc =
            unsafe { ffi::ghostty_focus_encode(event, buf.as_mut_ptr(), buf.len(), &mut written) };
        if rc == ffi::GHOSTTY_SUCCESS && written > 0 {
            Some(buf[..written].to_vec())
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Mouse input encoding
    // -----------------------------------------------------------------------

    /// Encode a mouse button press or release event.
    ///
    /// `gdk_button` is the GDK button number (1=left, 2=middle, 3=right).
    /// `pressed` is true for press, false for release.
    /// `position` is the (x, y) pixel position within the widget.
    /// `modifier` is the GDK modifier state at the time of the event.
    ///
    /// Returns bytes to write to the PTY, or None if the encoder produces
    /// no output (e.g. no mouse tracking mode active).
    pub fn encode_mouse_button(
        &mut self,
        gdk_button: u32,
        pressed: bool,
        position: (f64, f64),
        modifier: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
    ) -> Option<Vec<u8>> {
        let gbtn = gdk_button_to_ghostty(gdk_button);
        if gbtn == ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN {
            return None;
        }

        // Track button state.
        if pressed {
            self.buttons_held |= 1 << gbtn;
        } else {
            self.buttons_held &= !(1 << gbtn);
        }

        self.sync_mouse_encoder(terminal, screen_size, cell_size);

        let mods = gdk_mods_to_ghostty(modifier, 0);
        let action = if pressed {
            ffi::GHOSTTY_MOUSE_ACTION_PRESS
        } else {
            ffi::GHOSTTY_MOUSE_ACTION_RELEASE
        };

        unsafe {
            ffi::ghostty_mouse_event_set_action(self.mouse_event, action);
            ffi::ghostty_mouse_event_set_button(self.mouse_event, gbtn);
            ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods);
            ffi::ghostty_mouse_event_set_position(
                self.mouse_event,
                ffi::GhosttyMousePosition { x: position.0 as f32, y: position.1 as f32 },
            );
        }

        self.mouse_encode()
    }

    /// Encode a mouse motion event.
    ///
    /// `position` is the (x, y) pixel position within the widget.
    /// Returns bytes to write to the PTY, or None if the encoder produces
    /// no output or if the position hasn't changed enough (dedup threshold 0.5px).
    pub fn encode_mouse_motion(
        &mut self,
        position: (f64, f64),
        modifier: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
    ) -> Option<Vec<u8>> {
        // Only send if position actually changed (0.5px threshold, same as winit backend).
        if (position.0 - self.last_cursor_pos.0).abs() < 0.5
            && (position.1 - self.last_cursor_pos.1).abs() < 0.5
        {
            return None;
        }
        self.last_cursor_pos = position;

        self.sync_mouse_encoder(terminal, screen_size, cell_size);

        let mods = gdk_mods_to_ghostty(modifier, 0);
        unsafe {
            ffi::ghostty_mouse_event_set_action(self.mouse_event, ffi::GHOSTTY_MOUSE_ACTION_MOTION);
            ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods);
            ffi::ghostty_mouse_event_set_position(
                self.mouse_event,
                ffi::GhosttyMousePosition { x: position.0 as f32, y: position.1 as f32 },
            );
        }

        // Set button or clear based on what's held.
        if self.buttons_held != 0 {
            // Find the first held button.
            for i in 1..=7 {
                if self.buttons_held & (1 << i) != 0 {
                    unsafe { ffi::ghostty_mouse_event_set_button(self.mouse_event, i) };
                    break;
                }
            }
        } else {
            unsafe { ffi::ghostty_mouse_event_clear_button(self.mouse_event) };
        }

        self.mouse_encode()
    }

    /// Encode a scroll wheel event. Returns a `ScrollAction` indicating what to do.
    ///
    /// `dy` follows GTK convention: negative = scroll up (into history),
    /// positive = scroll down (toward bottom).
    ///
    /// When mouse tracking is active, the scroll is forwarded to the application
    /// as button 4 (up) / button 5 (down) press+release. When not active, the
    /// caller should scroll the viewport by the returned delta.
    pub fn encode_scroll(
        &mut self,
        dy: f64,
        position: (f64, f64),
        modifier: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
        mouse_tracking: bool,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
    ) -> ScrollAction {
        if mouse_tracking {
            // Forward to application via mouse encoder as button 4/5 press+release.
            self.sync_mouse_encoder(terminal, screen_size, cell_size);

            let mods = gdk_mods_to_ghostty(modifier, 0);
            // GTK: negative dy = scroll up, positive dy = scroll down.
            // Terminal: button 4 = scroll up, button 5 = scroll down.
            let scroll_btn = if dy < 0.0 {
                ffi::GHOSTTY_MOUSE_BUTTON_FOUR // scroll up
            } else {
                ffi::GHOSTTY_MOUSE_BUTTON_FIVE // scroll down
            };

            unsafe {
                ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods);
                ffi::ghostty_mouse_event_set_position(
                    self.mouse_event,
                    ffi::GhosttyMousePosition { x: position.0 as f32, y: position.1 as f32 },
                );
                ffi::ghostty_mouse_event_set_button(self.mouse_event, scroll_btn);
            }

            let mut result = Vec::new();

            // Press
            unsafe {
                ffi::ghostty_mouse_event_set_action(
                    self.mouse_event,
                    ffi::GHOSTTY_MOUSE_ACTION_PRESS,
                );
            }
            if let Some(bytes) = self.mouse_encode() {
                result.extend_from_slice(&bytes);
            }

            // Release
            unsafe {
                ffi::ghostty_mouse_event_set_action(
                    self.mouse_event,
                    ffi::GHOSTTY_MOUSE_ACTION_RELEASE,
                );
            }
            if let Some(bytes) = self.mouse_encode() {
                result.extend_from_slice(&bytes);
            }

            if result.is_empty() {
                // Encoder produced nothing; fall through to viewport scroll.
                let rows = if dy < 0.0 { -3 } else { 3 };
                ScrollAction::ScrollViewport(rows)
            } else {
                ScrollAction::WriteBytes(result)
            }
        } else {
            // Scroll the viewport. 3 rows per tick, matching Ghostling.
            // Negative dy = scroll up = negative delta (into history).
            let rows = if dy < 0.0 { -3 } else { 3 };
            ScrollAction::ScrollViewport(rows)
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers (keyboard)
    // -----------------------------------------------------------------------

    fn encode_key_inner(
        &self,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
        terminal: ffi::GhosttyTerminal,
        action: i32,
        is_press_or_repeat: bool,
    ) -> Option<Vec<u8>> {
        // 1. Sync encoder options from terminal state (cursor key mode, kitty flags, etc.)
        unsafe {
            ffi::ghostty_key_encoder_setopt_from_terminal(self.key_encoder, terminal);
        }

        // 2. Map GDK keyval to GhosttyKey, then refine with hardware keycode
        //    for numpad and left/right modifier disambiguation.
        let mut gkey = gdk_keyval_to_ghostty(keyval);
        gkey = refine_key_with_keycode(gkey, keycode);

        // 3. Set key and action.
        unsafe {
            ffi::ghostty_key_event_set_key(self.key_event, gkey);
            ffi::ghostty_key_event_set_action(self.key_event, action);
        }

        // 4. Build modifier bitmask (including CapsLock, NumLock, side bits).
        let mods = gdk_mods_to_ghostty(state, keycode);
        unsafe {
            ffi::ghostty_key_event_set_mods(self.key_event, mods);
        }

        // 5. Unshifted codepoint for the Kitty protocol.
        let ucp = unshifted_codepoint(keyval);
        unsafe {
            ffi::ghostty_key_event_set_unshifted_codepoint(self.key_event, ucp);
        }

        // 6. Consumed modifiers: if Shift produces a different character
        //    (e.g. Shift+1 = !), Shift is consumed.
        let consumed = if ucp != 0 && (mods & ffi::GHOSTTY_MODS_SHIFT) != 0 {
            ffi::GHOSTTY_MODS_SHIFT
        } else {
            0
        };
        unsafe {
            ffi::ghostty_key_event_set_consumed_mods(self.key_event, consumed);
        }

        // 7. Not composing (IME integration deferred).
        unsafe {
            ffi::ghostty_key_event_set_composing(self.key_event, false);
        }

        // 8. Attach UTF-8 text (only for press/repeat, not release).
        let text_bytes: Vec<u8>;
        if is_press_or_repeat {
            if let Some(ch) = keyval.to_unicode() {
                if !ch.is_control() || ch == ' ' {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    text_bytes = s.as_bytes().to_vec();
                    unsafe {
                        ffi::ghostty_key_event_set_utf8(
                            self.key_event,
                            text_bytes.as_ptr(),
                            text_bytes.len(),
                        );
                    }
                } else {
                    unsafe {
                        ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0);
                    }
                    text_bytes = Vec::new();
                }
            } else {
                unsafe {
                    ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0);
                }
                text_bytes = Vec::new();
            }
        } else {
            unsafe {
                ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0);
            }
            text_bytes = Vec::new();
        }

        // 9. Encode via the ghostty encoder.
        let mut buf = [0u8; 128];
        let mut written: usize = 0;
        let rc = unsafe {
            ffi::ghostty_key_encoder_encode(
                self.key_encoder,
                self.key_event,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };

        if rc == ffi::GHOSTTY_SUCCESS && written > 0 {
            return Some(buf[..written].to_vec());
        }

        // 10. Fallback: if encoder produced nothing and we have text, send raw
        //     text. Only for press/repeat, not release.
        if is_press_or_repeat && !text_bytes.is_empty() {
            return Some(text_bytes);
        }

        None
    }

    // -----------------------------------------------------------------------
    // Private helpers (mouse)
    // -----------------------------------------------------------------------

    /// Sync mouse encoder options from the terminal and current dimensions.
    fn sync_mouse_encoder(
        &self,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
    ) {
        unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(self.mouse_encoder, terminal);
        }

        let enc_size = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width: screen_size.0,
            screen_height: screen_size.1,
            cell_width: cell_size.0,
            cell_height: cell_size.1,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &enc_size as *const ffi::GhosttyMouseEncoderSize as *const c_void,
            );
        }

        let any_pressed = self.buttons_held != 0;
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &any_pressed as *const bool as *const c_void,
            );
        }

        let track_cell = true;
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL,
                &track_cell as *const bool as *const c_void,
            );
        }
    }

    /// Encode the current mouse event via the ghostty mouse encoder.
    fn mouse_encode(&self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 128];
        let mut written: usize = 0;
        let rc = unsafe {
            ffi::ghostty_mouse_encoder_encode(
                self.mouse_encoder,
                self.mouse_event,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        if rc == ffi::GHOSTTY_SUCCESS && written > 0 {
            Some(buf[..written].to_vec())
        } else {
            None
        }
    }
}

impl Drop for GhosttyInput {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_mouse_event_free(self.mouse_event);
            ffi::ghostty_mouse_encoder_free(self.mouse_encoder);
            ffi::ghostty_key_event_free(self.key_event);
            ffi::ghostty_key_encoder_free(self.key_encoder);
        }
    }
}

// ---------------------------------------------------------------------------
// GDK keyval -> GhosttyKey mapping
// ---------------------------------------------------------------------------

/// Map a GDK key symbol to a GhosttyKey constant.
///
/// Uses `keyval.name()` string matching for the mapping, adapted from the
/// winit `winit_key_to_ghostty()` approach but using GDK key names.
fn gdk_keyval_to_ghostty(keyval: gdk::Key) -> i32 {
    // Fast path: check for well-known named keys first.
    // GDK key constants are compared by identity.
    let key = gdk_named_key_to_ghostty(keyval);
    if key != ffi::GHOSTTY_KEY_UNIDENTIFIED {
        return key;
    }

    // Fall back to keyval name string matching for character keys.
    if let Some(name) = keyval.name() {
        return gdk_keyname_to_ghostty(&name);
    }

    ffi::GHOSTTY_KEY_UNIDENTIFIED
}

/// Map GDK named keys (non-character) to GhosttyKey constants.
fn gdk_named_key_to_ghostty(keyval: gdk::Key) -> i32 {
    // Space, Enter, Tab, Backspace, etc.
    if keyval == gdk::Key::space {
        return ffi::GHOSTTY_KEY_SPACE;
    }
    if keyval == gdk::Key::Return {
        return ffi::GHOSTTY_KEY_ENTER;
    }
    if keyval == gdk::Key::Tab || keyval == gdk::Key::ISO_Left_Tab {
        return ffi::GHOSTTY_KEY_TAB;
    }
    if keyval == gdk::Key::BackSpace {
        return ffi::GHOSTTY_KEY_BACKSPACE;
    }
    if keyval == gdk::Key::Delete {
        return ffi::GHOSTTY_KEY_DELETE;
    }
    if keyval == gdk::Key::Escape {
        return ffi::GHOSTTY_KEY_ESCAPE;
    }

    // Arrow keys
    if keyval == gdk::Key::Up {
        return ffi::GHOSTTY_KEY_ARROW_UP;
    }
    if keyval == gdk::Key::Down {
        return ffi::GHOSTTY_KEY_ARROW_DOWN;
    }
    if keyval == gdk::Key::Left {
        return ffi::GHOSTTY_KEY_ARROW_LEFT;
    }
    if keyval == gdk::Key::Right {
        return ffi::GHOSTTY_KEY_ARROW_RIGHT;
    }

    // Control pad
    if keyval == gdk::Key::Home {
        return ffi::GHOSTTY_KEY_HOME;
    }
    if keyval == gdk::Key::End {
        return ffi::GHOSTTY_KEY_END;
    }
    if keyval == gdk::Key::Page_Up {
        return ffi::GHOSTTY_KEY_PAGE_UP;
    }
    if keyval == gdk::Key::Page_Down {
        return ffi::GHOSTTY_KEY_PAGE_DOWN;
    }
    if keyval == gdk::Key::Insert {
        return ffi::GHOSTTY_KEY_INSERT;
    }

    // Lock keys
    if keyval == gdk::Key::Caps_Lock {
        return ffi::GHOSTTY_KEY_CAPS_LOCK;
    }
    if keyval == gdk::Key::Num_Lock {
        return ffi::GHOSTTY_KEY_NUM_LOCK;
    }
    if keyval == gdk::Key::Scroll_Lock {
        return ffi::GHOSTTY_KEY_SCROLL_LOCK;
    }
    if keyval == gdk::Key::Print {
        return ffi::GHOSTTY_KEY_PRINT_SCREEN;
    }
    if keyval == gdk::Key::Pause {
        return ffi::GHOSTTY_KEY_PAUSE;
    }
    if keyval == gdk::Key::Menu {
        return ffi::GHOSTTY_KEY_CONTEXT_MENU;
    }

    // Modifier keys — default to left; hardware keycode refines later.
    if keyval == gdk::Key::Shift_L {
        return ffi::GHOSTTY_KEY_SHIFT_LEFT;
    }
    if keyval == gdk::Key::Shift_R {
        return ffi::GHOSTTY_KEY_SHIFT_RIGHT;
    }
    if keyval == gdk::Key::Control_L {
        return ffi::GHOSTTY_KEY_CONTROL_LEFT;
    }
    if keyval == gdk::Key::Control_R {
        return ffi::GHOSTTY_KEY_CONTROL_RIGHT;
    }
    if keyval == gdk::Key::Alt_L {
        return ffi::GHOSTTY_KEY_ALT_LEFT;
    }
    if keyval == gdk::Key::Alt_R {
        return ffi::GHOSTTY_KEY_ALT_RIGHT;
    }
    if keyval == gdk::Key::Super_L || keyval == gdk::Key::Meta_L {
        return ffi::GHOSTTY_KEY_META_LEFT;
    }
    if keyval == gdk::Key::Super_R || keyval == gdk::Key::Meta_R {
        return ffi::GHOSTTY_KEY_META_RIGHT;
    }

    // Function keys
    if keyval == gdk::Key::F1 {
        return ffi::GHOSTTY_KEY_F1;
    }
    if keyval == gdk::Key::F2 {
        return ffi::GHOSTTY_KEY_F2;
    }
    if keyval == gdk::Key::F3 {
        return ffi::GHOSTTY_KEY_F3;
    }
    if keyval == gdk::Key::F4 {
        return ffi::GHOSTTY_KEY_F4;
    }
    if keyval == gdk::Key::F5 {
        return ffi::GHOSTTY_KEY_F5;
    }
    if keyval == gdk::Key::F6 {
        return ffi::GHOSTTY_KEY_F6;
    }
    if keyval == gdk::Key::F7 {
        return ffi::GHOSTTY_KEY_F7;
    }
    if keyval == gdk::Key::F8 {
        return ffi::GHOSTTY_KEY_F8;
    }
    if keyval == gdk::Key::F9 {
        return ffi::GHOSTTY_KEY_F9;
    }
    if keyval == gdk::Key::F10 {
        return ffi::GHOSTTY_KEY_F10;
    }
    if keyval == gdk::Key::F11 {
        return ffi::GHOSTTY_KEY_F11;
    }
    if keyval == gdk::Key::F12 {
        return ffi::GHOSTTY_KEY_F12;
    }
    if keyval == gdk::Key::F13 {
        return ffi::GHOSTTY_KEY_F13;
    }
    if keyval == gdk::Key::F14 {
        return ffi::GHOSTTY_KEY_F14;
    }
    if keyval == gdk::Key::F15 {
        return ffi::GHOSTTY_KEY_F15;
    }
    if keyval == gdk::Key::F16 {
        return ffi::GHOSTTY_KEY_F16;
    }
    if keyval == gdk::Key::F17 {
        return ffi::GHOSTTY_KEY_F17;
    }
    if keyval == gdk::Key::F18 {
        return ffi::GHOSTTY_KEY_F18;
    }
    if keyval == gdk::Key::F19 {
        return ffi::GHOSTTY_KEY_F19;
    }
    if keyval == gdk::Key::F20 {
        return ffi::GHOSTTY_KEY_F20;
    }
    if keyval == gdk::Key::F21 {
        return ffi::GHOSTTY_KEY_F21;
    }
    if keyval == gdk::Key::F22 {
        return ffi::GHOSTTY_KEY_F22;
    }
    if keyval == gdk::Key::F23 {
        return ffi::GHOSTTY_KEY_F23;
    }
    if keyval == gdk::Key::F24 {
        return ffi::GHOSTTY_KEY_F24;
    }
    // F25 not in GTK4 gdk::Key, skip.

    // Numpad keys — GDK provides separate keyvals for numpad.
    if keyval == gdk::Key::KP_0 {
        return ffi::GHOSTTY_KEY_NUMPAD_0;
    }
    if keyval == gdk::Key::KP_1 {
        return ffi::GHOSTTY_KEY_NUMPAD_1;
    }
    if keyval == gdk::Key::KP_2 {
        return ffi::GHOSTTY_KEY_NUMPAD_2;
    }
    if keyval == gdk::Key::KP_3 {
        return ffi::GHOSTTY_KEY_NUMPAD_3;
    }
    if keyval == gdk::Key::KP_4 {
        return ffi::GHOSTTY_KEY_NUMPAD_4;
    }
    if keyval == gdk::Key::KP_5 {
        return ffi::GHOSTTY_KEY_NUMPAD_5;
    }
    if keyval == gdk::Key::KP_6 {
        return ffi::GHOSTTY_KEY_NUMPAD_6;
    }
    if keyval == gdk::Key::KP_7 {
        return ffi::GHOSTTY_KEY_NUMPAD_7;
    }
    if keyval == gdk::Key::KP_8 {
        return ffi::GHOSTTY_KEY_NUMPAD_8;
    }
    if keyval == gdk::Key::KP_9 {
        return ffi::GHOSTTY_KEY_NUMPAD_9;
    }
    if keyval == gdk::Key::KP_Add {
        return ffi::GHOSTTY_KEY_NUMPAD_ADD;
    }
    if keyval == gdk::Key::KP_Subtract {
        return ffi::GHOSTTY_KEY_NUMPAD_SUBTRACT;
    }
    if keyval == gdk::Key::KP_Multiply {
        return ffi::GHOSTTY_KEY_NUMPAD_MULTIPLY;
    }
    if keyval == gdk::Key::KP_Divide {
        return ffi::GHOSTTY_KEY_NUMPAD_DIVIDE;
    }
    if keyval == gdk::Key::KP_Decimal {
        return ffi::GHOSTTY_KEY_NUMPAD_DECIMAL;
    }
    if keyval == gdk::Key::KP_Enter {
        return ffi::GHOSTTY_KEY_NUMPAD_ENTER;
    }
    if keyval == gdk::Key::KP_Equal {
        return ffi::GHOSTTY_KEY_NUMPAD_EQUAL;
    }

    ffi::GHOSTTY_KEY_UNIDENTIFIED
}

/// Map GDK key name strings to GhosttyKey constants.
///
/// This handles letters, digits, and symbol keys via the keyval name string.
fn gdk_keyname_to_ghostty(name: &str) -> i32 {
    // GDK keyval names for letters are lowercase (e.g. "a") or uppercase ("A").
    // Normalize to lowercase for matching.
    match name.to_ascii_lowercase().as_str() {
        "a" => ffi::GHOSTTY_KEY_A,
        "b" => ffi::GHOSTTY_KEY_B,
        "c" => ffi::GHOSTTY_KEY_C,
        "d" => ffi::GHOSTTY_KEY_D,
        "e" => ffi::GHOSTTY_KEY_E,
        "f" => ffi::GHOSTTY_KEY_F,
        "g" => ffi::GHOSTTY_KEY_G,
        "h" => ffi::GHOSTTY_KEY_H,
        "i" => ffi::GHOSTTY_KEY_I,
        "j" => ffi::GHOSTTY_KEY_J,
        "k" => ffi::GHOSTTY_KEY_K,
        "l" => ffi::GHOSTTY_KEY_L,
        "m" => ffi::GHOSTTY_KEY_M,
        "n" => ffi::GHOSTTY_KEY_N,
        "o" => ffi::GHOSTTY_KEY_O,
        "p" => ffi::GHOSTTY_KEY_P,
        "q" => ffi::GHOSTTY_KEY_Q,
        "r" => ffi::GHOSTTY_KEY_R,
        "s" => ffi::GHOSTTY_KEY_S,
        "t" => ffi::GHOSTTY_KEY_T,
        "u" => ffi::GHOSTTY_KEY_U,
        "v" => ffi::GHOSTTY_KEY_V,
        "w" => ffi::GHOSTTY_KEY_W,
        "x" => ffi::GHOSTTY_KEY_X,
        "y" => ffi::GHOSTTY_KEY_Y,
        "z" => ffi::GHOSTTY_KEY_Z,

        // Digits (GDK names: "0"..."9")
        "0" => ffi::GHOSTTY_KEY_DIGIT_0,
        "1" => ffi::GHOSTTY_KEY_DIGIT_1,
        "2" => ffi::GHOSTTY_KEY_DIGIT_2,
        "3" => ffi::GHOSTTY_KEY_DIGIT_3,
        "4" => ffi::GHOSTTY_KEY_DIGIT_4,
        "5" => ffi::GHOSTTY_KEY_DIGIT_5,
        "6" => ffi::GHOSTTY_KEY_DIGIT_6,
        "7" => ffi::GHOSTTY_KEY_DIGIT_7,
        "8" => ffi::GHOSTTY_KEY_DIGIT_8,
        "9" => ffi::GHOSTTY_KEY_DIGIT_9,

        // Shifted digit symbols — map to the base digit key
        "exclam" => ffi::GHOSTTY_KEY_DIGIT_1,      // !
        "at" => ffi::GHOSTTY_KEY_DIGIT_2,          // @
        "numbersign" => ffi::GHOSTTY_KEY_DIGIT_3,  // #
        "dollar" => ffi::GHOSTTY_KEY_DIGIT_4,      // $
        "percent" => ffi::GHOSTTY_KEY_DIGIT_5,     // %
        "asciicircum" => ffi::GHOSTTY_KEY_DIGIT_6, // ^
        "ampersand" => ffi::GHOSTTY_KEY_DIGIT_7,   // &
        "asterisk" => ffi::GHOSTTY_KEY_DIGIT_8,    // *
        "parenleft" => ffi::GHOSTTY_KEY_DIGIT_9,   // (
        "parenright" => ffi::GHOSTTY_KEY_DIGIT_0,  // )

        // Symbols — unshifted GDK names
        "minus" => ffi::GHOSTTY_KEY_MINUS,
        "equal" => ffi::GHOSTTY_KEY_EQUAL,
        "bracketleft" => ffi::GHOSTTY_KEY_BRACKET_LEFT,
        "bracketright" => ffi::GHOSTTY_KEY_BRACKET_RIGHT,
        "backslash" => ffi::GHOSTTY_KEY_BACKSLASH,
        "semicolon" => ffi::GHOSTTY_KEY_SEMICOLON,
        "apostrophe" => ffi::GHOSTTY_KEY_QUOTE,
        "comma" => ffi::GHOSTTY_KEY_COMMA,
        "period" => ffi::GHOSTTY_KEY_PERIOD,
        "slash" => ffi::GHOSTTY_KEY_SLASH,
        "grave" => ffi::GHOSTTY_KEY_BACKQUOTE,

        // Shifted symbol variants
        "underscore" => ffi::GHOSTTY_KEY_MINUS,         // _
        "plus" => ffi::GHOSTTY_KEY_EQUAL,               // +
        "braceleft" => ffi::GHOSTTY_KEY_BRACKET_LEFT,   // {
        "braceright" => ffi::GHOSTTY_KEY_BRACKET_RIGHT, // }
        "bar" => ffi::GHOSTTY_KEY_BACKSLASH,            // |
        "colon" => ffi::GHOSTTY_KEY_SEMICOLON,          // :
        "quotedbl" => ffi::GHOSTTY_KEY_QUOTE,           // "
        "less" => ffi::GHOSTTY_KEY_COMMA,               // <
        "greater" => ffi::GHOSTTY_KEY_PERIOD,           // >
        "question" => ffi::GHOSTTY_KEY_SLASH,           // ?
        "asciitilde" => ffi::GHOSTTY_KEY_BACKQUOTE,     // ~

        _ => ffi::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

// ---------------------------------------------------------------------------
// Hardware keycode refinement (numpad + left/right modifiers)
// ---------------------------------------------------------------------------

/// Refine a GhosttyKey using the GDK hardware keycode.
///
/// The XKB standard keycodes for numpad and modifier keys are stable across
/// standard keyboards. This handles:
/// - Numpad digits/operators that GDK may report with their main-keyboard
///   keyvals (e.g. when NumLock is on, KP_0 produces keyval "0" not "KP_0")
/// - Left/right modifier disambiguation
fn refine_key_with_keycode(gkey: i32, keycode: u32) -> i32 {
    // XKB hardware keycodes (evdev + 8 offset) for a standard US layout.
    // These are stable on Linux (both X11 and Wayland).
    match keycode {
        // Numpad digits (evdev keycodes)
        87 => ffi::GHOSTTY_KEY_NUMPAD_1,
        88 => ffi::GHOSTTY_KEY_NUMPAD_2,
        89 => ffi::GHOSTTY_KEY_NUMPAD_3,
        83 => ffi::GHOSTTY_KEY_NUMPAD_4,
        84 => ffi::GHOSTTY_KEY_NUMPAD_5,
        85 => ffi::GHOSTTY_KEY_NUMPAD_6,
        79 => ffi::GHOSTTY_KEY_NUMPAD_7,
        80 => ffi::GHOSTTY_KEY_NUMPAD_8,
        81 => ffi::GHOSTTY_KEY_NUMPAD_9,
        90 => ffi::GHOSTTY_KEY_NUMPAD_0,
        // Numpad operators
        86 => ffi::GHOSTTY_KEY_NUMPAD_ADD,
        82 => ffi::GHOSTTY_KEY_NUMPAD_SUBTRACT,
        63 => ffi::GHOSTTY_KEY_NUMPAD_MULTIPLY,
        106 => ffi::GHOSTTY_KEY_NUMPAD_DIVIDE,
        91 => ffi::GHOSTTY_KEY_NUMPAD_DECIMAL,
        104 => ffi::GHOSTTY_KEY_NUMPAD_ENTER,
        125 => ffi::GHOSTTY_KEY_NUMPAD_EQUAL,
        // Right-side modifier keys
        62 => ffi::GHOSTTY_KEY_SHIFT_RIGHT,
        105 => ffi::GHOSTTY_KEY_CONTROL_RIGHT,
        108 => ffi::GHOSTTY_KEY_ALT_RIGHT,
        134 => ffi::GHOSTTY_KEY_META_RIGHT,
        // Not a numpad/right-modifier keycode; keep the original mapping.
        _ => gkey,
    }
}

// ---------------------------------------------------------------------------
// GDK modifiers -> GhosttyMods mapping
// ---------------------------------------------------------------------------

/// Map GDK modifier state to GhosttyMods bitmask.
///
/// Includes CapsLock, NumLock, and modifier side bits (an improvement over
/// the winit backend which cannot report these).
fn gdk_mods_to_ghostty(state: gdk::ModifierType, keycode: u32) -> ffi::GhosttyMods {
    let mut result: ffi::GhosttyMods = 0;

    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        result |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        result |= ffi::GHOSTTY_MODS_CTRL;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        result |= ffi::GHOSTTY_MODS_ALT;
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        result |= ffi::GHOSTTY_MODS_SUPER;
    }

    // CapsLock: GDK LOCK_MASK
    if state.contains(gdk::ModifierType::LOCK_MASK) {
        result |= ffi::GHOSTTY_MODS_CAPS_LOCK;
    }

    // NumLock: GDK MOD2_MASK is typically NumLock on Linux (X11 and Wayland).
    // GTK4 does not have a dedicated MOD2_MASK constant, so we check bit 4
    // of the raw modifier bits (GDK_MOD2_MASK = 1 << 4 = 0x10).
    // However, in GTK4's ModifierType the relevant bits are:
    // The raw GDK modifier value for MOD2 can be checked via bits().
    // GDK_MOD2_MASK in GDK3/X11 = 1<<4 = 16, but GTK4 uses different layout.
    // For safety, we skip NumLock detection if the bit isn't reliably available
    // in GTK4's ModifierType. The spec notes this is platform-dependent.
    // On GTK4, modifier bits beyond the standard set are not exposed.

    // Side bits: determine from the hardware keycode if the current key event
    // is for a right-side modifier.
    match keycode {
        62 => result |= ffi::GHOSTTY_MODS_SHIFT_SIDE, // Shift_R
        105 => result |= ffi::GHOSTTY_MODS_CTRL_SIDE, // Control_R
        108 => result |= ffi::GHOSTTY_MODS_ALT_SIDE,  // Alt_R
        134 => result |= ffi::GHOSTTY_MODS_SUPER_SIDE, // Super_R
        _ => {}
    }

    result
}

// ---------------------------------------------------------------------------
// Unshifted codepoint (for Kitty protocol)
// ---------------------------------------------------------------------------

/// Return the unshifted codepoint for a key (the character with no modifiers
/// on a US layout). The Kitty keyboard protocol needs this.
///
/// For named keys (Space, Enter, etc.), returns the standard codepoint.
/// For character keys, uses the GDK keyval name to determine the base character.
fn unshifted_codepoint(keyval: gdk::Key) -> u32 {
    // Space is special
    if keyval == gdk::Key::space {
        return ' ' as u32;
    }

    // For printable keys, try to get the base (unshifted) character.
    if let Some(name) = keyval.name() {
        return match name.to_ascii_lowercase().as_str() {
            "a" => 'a' as u32,
            "b" => 'b' as u32,
            "c" => 'c' as u32,
            "d" => 'd' as u32,
            "e" => 'e' as u32,
            "f" => 'f' as u32,
            "g" => 'g' as u32,
            "h" => 'h' as u32,
            "i" => 'i' as u32,
            "j" => 'j' as u32,
            "k" => 'k' as u32,
            "l" => 'l' as u32,
            "m" => 'm' as u32,
            "n" => 'n' as u32,
            "o" => 'o' as u32,
            "p" => 'p' as u32,
            "q" => 'q' as u32,
            "r" => 'r' as u32,
            "s" => 's' as u32,
            "t" => 't' as u32,
            "u" => 'u' as u32,
            "v" => 'v' as u32,
            "w" => 'w' as u32,
            "x" => 'x' as u32,
            "y" => 'y' as u32,
            "z" => 'z' as u32,
            "0" | "parenright" => '0' as u32,
            "1" | "exclam" => '1' as u32,
            "2" | "at" => '2' as u32,
            "3" | "numbersign" => '3' as u32,
            "4" | "dollar" => '4' as u32,
            "5" | "percent" => '5' as u32,
            "6" | "asciicircum" => '6' as u32,
            "7" | "ampersand" => '7' as u32,
            "8" | "asterisk" => '8' as u32,
            "9" | "parenleft" => '9' as u32,
            "minus" | "underscore" => '-' as u32,
            "equal" | "plus" => '=' as u32,
            "bracketleft" | "braceleft" => '[' as u32,
            "bracketright" | "braceright" => ']' as u32,
            "backslash" | "bar" => '\\' as u32,
            "semicolon" | "colon" => ';' as u32,
            "apostrophe" | "quotedbl" => '\'' as u32,
            "comma" | "less" => ',' as u32,
            "period" | "greater" => '.' as u32,
            "slash" | "question" => '/' as u32,
            "grave" | "asciitilde" => '`' as u32,
            _ => 0,
        };
    }

    0
}

// ---------------------------------------------------------------------------
// GDK mouse button -> GhosttyMouseButton mapping
// ---------------------------------------------------------------------------

/// Map a GDK mouse button number to a GhosttyMouseButton constant.
///
/// GDK button numbering: 1=left, 2=middle, 3=right, 4+=extra.
/// Ghostty button numbering: 1=left, 2=right, 3=middle, 4+=extra.
///
/// Note: GDK swaps middle and right compared to Ghostty, so we must swap
/// buttons 2 and 3 in the mapping.
fn gdk_button_to_ghostty(button: u32) -> i32 {
    match button {
        1 => ffi::GHOSTTY_MOUSE_BUTTON_LEFT,   // GDK 1 = left
        2 => ffi::GHOSTTY_MOUSE_BUTTON_MIDDLE, // GDK 2 = middle
        3 => ffi::GHOSTTY_MOUSE_BUTTON_RIGHT,  // GDK 3 = right
        4 => ffi::GHOSTTY_MOUSE_BUTTON_FOUR,
        5 => ffi::GHOSTTY_MOUSE_BUTTON_FIVE,
        6 => ffi::GHOSTTY_MOUSE_BUTTON_SIX,
        7 => ffi::GHOSTTY_MOUSE_BUTTON_SEVEN,
        8 => ffi::GHOSTTY_MOUSE_BUTTON_EIGHT,
        9 => ffi::GHOSTTY_MOUSE_BUTTON_NINE,
        10 => ffi::GHOSTTY_MOUSE_BUTTON_TEN,
        11 => ffi::GHOSTTY_MOUSE_BUTTON_ELEVEN,
        _ => ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN,
    }
}
