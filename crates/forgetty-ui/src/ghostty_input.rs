//! Input encoding via libghostty-vt key and mouse encoders.
//!
//! Replaces the hand-rolled `input.rs` escape tables with the proper
//! ghostty encoder APIs. This gives us automatic support for:
//! - Application cursor mode (DECCKM)
//! - Kitty keyboard protocol
//! - All mouse tracking modes and formats
//! - Focus reporting

use std::os::raw::c_void;

use forgetty_vt::ffi;
use winit::event::KeyEvent;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// Result of encoding a scroll wheel event.
pub enum ScrollAction {
    /// Write these bytes to the PTY (mouse tracking is active).
    WriteBytes(Vec<u8>),
    /// Scroll the viewport by this many rows (no mouse tracking).
    ScrollViewport(isize),
}

/// Wraps the ghostty key encoder, key event, mouse encoder, and mouse event
/// handles, providing a safe(ish) interface for the app event loop.
pub struct GhosttyInput {
    key_encoder: ffi::GhosttyKeyEncoder,
    key_event: ffi::GhosttyKeyEvent,
    mouse_encoder: ffi::GhosttyMouseEncoder,
    mouse_event: ffi::GhosttyMouseEvent,
    /// Track last known cursor position for mouse motion dedup.
    last_cursor_pos: (f64, f64),
    /// Track which mouse buttons are currently held.
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
            last_cursor_pos: (0.0, 0.0),
            buttons_held: 0,
        }
    }

    /// Encode a keyboard event into PTY bytes.
    ///
    /// Syncs encoder options from the terminal, maps the winit key to a
    /// GhosttyKey, sets up the key event, and encodes. Falls back to raw
    /// UTF-8 text if the encoder produces no output.
    pub fn encode_key(
        &self,
        event: &KeyEvent,
        modifiers: ModifiersState,
        terminal: ffi::GhosttyTerminal,
        is_press: bool,
        is_repeat: bool,
    ) -> Option<Vec<u8>> {
        // Sync encoder options from terminal state (cursor key mode, kitty flags, etc.)
        unsafe {
            ffi::ghostty_key_encoder_setopt_from_terminal(self.key_encoder, terminal);
        }

        // Map winit key to ghostty key, then refine via physical key for numpad.
        let mut gkey = winit_key_to_ghostty(&event.logical_key);
        gkey = refine_key_with_physical(gkey, &event.physical_key);

        // Determine action.
        let action = if !is_press {
            ffi::GHOSTTY_KEY_ACTION_RELEASE
        } else if is_repeat {
            ffi::GHOSTTY_KEY_ACTION_REPEAT
        } else {
            ffi::GHOSTTY_KEY_ACTION_PRESS
        };

        unsafe {
            ffi::ghostty_key_event_set_key(self.key_event, gkey);
            ffi::ghostty_key_event_set_action(self.key_event, action);
        }

        // Build modifier bitmask.
        let mods = winit_mods_to_ghostty(modifiers);
        unsafe {
            ffi::ghostty_key_event_set_mods(self.key_event, mods);
        }

        // Unshifted codepoint for Kitty protocol.
        let ucp = unshifted_codepoint(&event.logical_key);
        unsafe {
            ffi::ghostty_key_event_set_unshifted_codepoint(self.key_event, ucp);
        }

        // Consumed mods: for printable keys, shift is consumed.
        let consumed = if ucp != 0 && (mods & ffi::GHOSTTY_MODS_SHIFT) != 0 {
            ffi::GHOSTTY_MODS_SHIFT
        } else {
            0
        };
        unsafe {
            ffi::ghostty_key_event_set_consumed_mods(self.key_event, consumed);
        }

        // Attach UTF-8 text (only for press/repeat, not release).
        let text_bytes: Vec<u8>;
        if is_press || is_repeat {
            if let Some(text) = &event.text {
                let s = text.as_str();
                if !s.is_empty() {
                    text_bytes = s.as_bytes().to_vec();
                    unsafe {
                        ffi::ghostty_key_event_set_utf8(
                            self.key_event,
                            text_bytes.as_ptr(),
                            text_bytes.len(),
                        );
                    }
                } else {
                    unsafe { ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0) };
                    text_bytes = Vec::new();
                }
            } else {
                unsafe { ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0) };
                text_bytes = Vec::new();
            }
        } else {
            unsafe { ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0) };
            text_bytes = Vec::new();
        }

        // Encode.
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

        // Fallback: if encoder produced nothing and we have text, send raw text.
        // Only for press/repeat, skip for release.
        if (is_press || is_repeat)
            && !text_bytes.is_empty()
            && gkey != ffi::GHOSTTY_KEY_UNIDENTIFIED
        {
            // Don't fallback for modifier-only or non-text keys
        }
        if (is_press || is_repeat) && !text_bytes.is_empty() {
            return Some(text_bytes);
        }

        None
    }

    /// Handle a mouse button press/release and return bytes to write to PTY (if any).
    pub fn handle_mouse_button(
        &mut self,
        button: winit::event::MouseButton,
        pressed: bool,
        position: (f64, f64),
        modifiers: ModifiersState,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
        padding_top: u32,
    ) -> Option<Vec<u8>> {
        let gbtn = winit_mouse_button_to_ghostty(button);
        if gbtn == ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN {
            return None;
        }

        // Track button state.
        if pressed {
            self.buttons_held |= 1 << gbtn;
        } else {
            self.buttons_held &= !(1 << gbtn);
        }

        self.sync_mouse_encoder(terminal, screen_size, cell_size, padding_top);

        let mods = winit_mods_to_ghostty(modifiers);
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

    /// Handle mouse motion and return bytes to write to PTY (if any).
    pub fn handle_mouse_move(
        &mut self,
        position: (f64, f64),
        modifiers: ModifiersState,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
        padding_top: u32,
    ) -> Option<Vec<u8>> {
        // Only send if position actually changed.
        if (position.0 - self.last_cursor_pos.0).abs() < 0.5
            && (position.1 - self.last_cursor_pos.1).abs() < 0.5
        {
            return None;
        }
        self.last_cursor_pos = position;

        self.sync_mouse_encoder(terminal, screen_size, cell_size, padding_top);

        let mods = winit_mods_to_ghostty(modifiers);
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

    /// Handle scroll wheel. Returns a ScrollAction indicating what to do.
    pub fn handle_scroll(
        &mut self,
        delta_lines: f32,
        modifiers: ModifiersState,
        position: (f64, f64),
        terminal: ffi::GhosttyTerminal,
        mouse_tracking: bool,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
        padding_top: u32,
    ) -> ScrollAction {
        if mouse_tracking {
            // Forward to application via mouse encoder as button 4/5 press+release.
            self.sync_mouse_encoder(terminal, screen_size, cell_size, padding_top);

            let mods = winit_mods_to_ghostty(modifiers);
            let scroll_btn = if delta_lines > 0.0 {
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
                let rows = if delta_lines > 0.0 { -3 } else { 3 };
                ScrollAction::ScrollViewport(rows)
            } else {
                ScrollAction::WriteBytes(result)
            }
        } else {
            // Scroll the viewport. 3 rows per tick, matching Ghostling.
            let rows = if delta_lines > 0.0 { -3 } else { 3 };
            ScrollAction::ScrollViewport(rows)
        }
    }

    /// Encode a focus event. Returns bytes to write to PTY (if any).
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
    // Private helpers
    // -----------------------------------------------------------------------

    fn sync_mouse_encoder(
        &self,
        terminal: ffi::GhosttyTerminal,
        screen_size: (u32, u32),
        cell_size: (u32, u32),
        padding_top: u32,
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
            padding_top,
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
// Key mapping: winit -> GhosttyKey
// ---------------------------------------------------------------------------

fn winit_key_to_ghostty(key: &Key) -> i32 {
    match key {
        Key::Named(named) => winit_named_key_to_ghostty(*named),
        Key::Character(s) => {
            let s = s.as_str();
            // Map single-character strings to their GhosttyKey.
            // We use lowercase to match physical keys.
            match s.to_ascii_lowercase().as_str() {
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
                "0" | ")" => ffi::GHOSTTY_KEY_DIGIT_0,
                "1" | "!" => ffi::GHOSTTY_KEY_DIGIT_1,
                "2" | "@" => ffi::GHOSTTY_KEY_DIGIT_2,
                "3" | "#" => ffi::GHOSTTY_KEY_DIGIT_3,
                "4" | "$" => ffi::GHOSTTY_KEY_DIGIT_4,
                "5" | "%" => ffi::GHOSTTY_KEY_DIGIT_5,
                "6" | "^" => ffi::GHOSTTY_KEY_DIGIT_6,
                "7" | "&" => ffi::GHOSTTY_KEY_DIGIT_7,
                "8" | "*" => ffi::GHOSTTY_KEY_DIGIT_8,
                "9" | "(" => ffi::GHOSTTY_KEY_DIGIT_9,
                "-" | "_" => ffi::GHOSTTY_KEY_MINUS,
                "=" | "+" => ffi::GHOSTTY_KEY_EQUAL,
                "[" | "{" => ffi::GHOSTTY_KEY_BRACKET_LEFT,
                "]" | "}" => ffi::GHOSTTY_KEY_BRACKET_RIGHT,
                "\\" | "|" => ffi::GHOSTTY_KEY_BACKSLASH,
                ";" | ":" => ffi::GHOSTTY_KEY_SEMICOLON,
                "'" | "\"" => ffi::GHOSTTY_KEY_QUOTE,
                "," | "<" => ffi::GHOSTTY_KEY_COMMA,
                "." | ">" => ffi::GHOSTTY_KEY_PERIOD,
                "/" | "?" => ffi::GHOSTTY_KEY_SLASH,
                "`" | "~" => ffi::GHOSTTY_KEY_BACKQUOTE,
                _ => ffi::GHOSTTY_KEY_UNIDENTIFIED,
            }
        }
        _ => ffi::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

fn winit_named_key_to_ghostty(key: NamedKey) -> i32 {
    match key {
        NamedKey::Space => ffi::GHOSTTY_KEY_SPACE,
        NamedKey::Enter => ffi::GHOSTTY_KEY_ENTER,
        NamedKey::Tab => ffi::GHOSTTY_KEY_TAB,
        NamedKey::Backspace => ffi::GHOSTTY_KEY_BACKSPACE,
        NamedKey::Delete => ffi::GHOSTTY_KEY_DELETE,
        NamedKey::Escape => ffi::GHOSTTY_KEY_ESCAPE,
        NamedKey::ArrowUp => ffi::GHOSTTY_KEY_ARROW_UP,
        NamedKey::ArrowDown => ffi::GHOSTTY_KEY_ARROW_DOWN,
        NamedKey::ArrowLeft => ffi::GHOSTTY_KEY_ARROW_LEFT,
        NamedKey::ArrowRight => ffi::GHOSTTY_KEY_ARROW_RIGHT,
        NamedKey::Home => ffi::GHOSTTY_KEY_HOME,
        NamedKey::End => ffi::GHOSTTY_KEY_END,
        NamedKey::PageUp => ffi::GHOSTTY_KEY_PAGE_UP,
        NamedKey::PageDown => ffi::GHOSTTY_KEY_PAGE_DOWN,
        NamedKey::Insert => ffi::GHOSTTY_KEY_INSERT,
        NamedKey::CapsLock => ffi::GHOSTTY_KEY_CAPS_LOCK,
        NamedKey::NumLock => ffi::GHOSTTY_KEY_NUM_LOCK,
        NamedKey::ScrollLock => ffi::GHOSTTY_KEY_SCROLL_LOCK,
        NamedKey::PrintScreen => ffi::GHOSTTY_KEY_PRINT_SCREEN,
        NamedKey::Pause => ffi::GHOSTTY_KEY_PAUSE,
        NamedKey::ContextMenu => ffi::GHOSTTY_KEY_CONTEXT_MENU,
        NamedKey::F1 => ffi::GHOSTTY_KEY_F1,
        NamedKey::F2 => ffi::GHOSTTY_KEY_F2,
        NamedKey::F3 => ffi::GHOSTTY_KEY_F3,
        NamedKey::F4 => ffi::GHOSTTY_KEY_F4,
        NamedKey::F5 => ffi::GHOSTTY_KEY_F5,
        NamedKey::F6 => ffi::GHOSTTY_KEY_F6,
        NamedKey::F7 => ffi::GHOSTTY_KEY_F7,
        NamedKey::F8 => ffi::GHOSTTY_KEY_F8,
        NamedKey::F9 => ffi::GHOSTTY_KEY_F9,
        NamedKey::F10 => ffi::GHOSTTY_KEY_F10,
        NamedKey::F11 => ffi::GHOSTTY_KEY_F11,
        NamedKey::F12 => ffi::GHOSTTY_KEY_F12,
        NamedKey::F13 => ffi::GHOSTTY_KEY_F13,
        NamedKey::F14 => ffi::GHOSTTY_KEY_F14,
        NamedKey::F15 => ffi::GHOSTTY_KEY_F15,
        NamedKey::F16 => ffi::GHOSTTY_KEY_F16,
        NamedKey::F17 => ffi::GHOSTTY_KEY_F17,
        NamedKey::F18 => ffi::GHOSTTY_KEY_F18,
        NamedKey::F19 => ffi::GHOSTTY_KEY_F19,
        NamedKey::F20 => ffi::GHOSTTY_KEY_F20,
        NamedKey::F21 => ffi::GHOSTTY_KEY_F21,
        NamedKey::F22 => ffi::GHOSTTY_KEY_F22,
        NamedKey::F23 => ffi::GHOSTTY_KEY_F23,
        NamedKey::F24 => ffi::GHOSTTY_KEY_F24,
        NamedKey::F25 => ffi::GHOSTTY_KEY_F25,
        // Modifier keys (default to left variant; physical key can refine later)
        NamedKey::Shift => ffi::GHOSTTY_KEY_SHIFT_LEFT,
        NamedKey::Control => ffi::GHOSTTY_KEY_CONTROL_LEFT,
        NamedKey::Alt => ffi::GHOSTTY_KEY_ALT_LEFT,
        NamedKey::Super => ffi::GHOSTTY_KEY_META_LEFT,
        // Media keys
        NamedKey::MediaPlayPause => ffi::GHOSTTY_KEY_MEDIA_PLAY_PAUSE,
        NamedKey::MediaStop => ffi::GHOSTTY_KEY_MEDIA_STOP,
        NamedKey::MediaTrackNext => ffi::GHOSTTY_KEY_MEDIA_TRACK_NEXT,
        NamedKey::MediaTrackPrevious => ffi::GHOSTTY_KEY_MEDIA_TRACK_PREVIOUS,
        // NamedKey::MediaSelect not available in winit 0.30
        NamedKey::AudioVolumeDown => ffi::GHOSTTY_KEY_AUDIO_VOLUME_DOWN,
        NamedKey::AudioVolumeMute => ffi::GHOSTTY_KEY_AUDIO_VOLUME_MUTE,
        NamedKey::AudioVolumeUp => ffi::GHOSTTY_KEY_AUDIO_VOLUME_UP,
        // Browser keys
        NamedKey::BrowserBack => ffi::GHOSTTY_KEY_BROWSER_BACK,
        NamedKey::BrowserFavorites => ffi::GHOSTTY_KEY_BROWSER_FAVORITES,
        NamedKey::BrowserForward => ffi::GHOSTTY_KEY_BROWSER_FORWARD,
        NamedKey::BrowserHome => ffi::GHOSTTY_KEY_BROWSER_HOME,
        NamedKey::BrowserRefresh => ffi::GHOSTTY_KEY_BROWSER_REFRESH,
        NamedKey::BrowserSearch => ffi::GHOSTTY_KEY_BROWSER_SEARCH,
        NamedKey::BrowserStop => ffi::GHOSTTY_KEY_BROWSER_STOP,
        // Launch / power keys
        NamedKey::Eject => ffi::GHOSTTY_KEY_EJECT,
        NamedKey::LaunchApplication1 => ffi::GHOSTTY_KEY_LAUNCH_APP_1,
        NamedKey::LaunchApplication2 => ffi::GHOSTTY_KEY_LAUNCH_APP_2,
        NamedKey::LaunchMail => ffi::GHOSTTY_KEY_LAUNCH_MAIL,
        NamedKey::Power => ffi::GHOSTTY_KEY_POWER,
        // NamedKey::Sleep not available in winit 0.30
        NamedKey::WakeUp => ffi::GHOSTTY_KEY_WAKE_UP,
        // Clipboard keys
        NamedKey::Copy => ffi::GHOSTTY_KEY_COPY,
        NamedKey::Cut => ffi::GHOSTTY_KEY_CUT,
        NamedKey::Paste => ffi::GHOSTTY_KEY_PASTE,
        _ => ffi::GHOSTTY_KEY_UNIDENTIFIED,
    }
}

/// Refine a GhosttyKey using the physical key, primarily for numpad keys.
///
/// Winit delivers numpad digit presses as `Key::Character("0")` etc., which
/// maps to GHOSTTY_KEY_DIGIT_*. When the physical key is a numpad key we
/// override to the correct GHOSTTY_KEY_NUMPAD_* constant. This also handles
/// modifier keys that need left/right disambiguation.
fn refine_key_with_physical(gkey: i32, physical: &PhysicalKey) -> i32 {
    match physical {
        PhysicalKey::Code(code) => match code {
            KeyCode::Numpad0 => ffi::GHOSTTY_KEY_NUMPAD_0,
            KeyCode::Numpad1 => ffi::GHOSTTY_KEY_NUMPAD_1,
            KeyCode::Numpad2 => ffi::GHOSTTY_KEY_NUMPAD_2,
            KeyCode::Numpad3 => ffi::GHOSTTY_KEY_NUMPAD_3,
            KeyCode::Numpad4 => ffi::GHOSTTY_KEY_NUMPAD_4,
            KeyCode::Numpad5 => ffi::GHOSTTY_KEY_NUMPAD_5,
            KeyCode::Numpad6 => ffi::GHOSTTY_KEY_NUMPAD_6,
            KeyCode::Numpad7 => ffi::GHOSTTY_KEY_NUMPAD_7,
            KeyCode::Numpad8 => ffi::GHOSTTY_KEY_NUMPAD_8,
            KeyCode::Numpad9 => ffi::GHOSTTY_KEY_NUMPAD_9,
            KeyCode::NumpadAdd => ffi::GHOSTTY_KEY_NUMPAD_ADD,
            KeyCode::NumpadSubtract => ffi::GHOSTTY_KEY_NUMPAD_SUBTRACT,
            KeyCode::NumpadMultiply => ffi::GHOSTTY_KEY_NUMPAD_MULTIPLY,
            KeyCode::NumpadDivide => ffi::GHOSTTY_KEY_NUMPAD_DIVIDE,
            KeyCode::NumpadDecimal => ffi::GHOSTTY_KEY_NUMPAD_DECIMAL,
            KeyCode::NumpadEnter => ffi::GHOSTTY_KEY_NUMPAD_ENTER,
            KeyCode::NumpadEqual => ffi::GHOSTTY_KEY_NUMPAD_EQUAL,
            // Disambiguate left/right modifier keys.
            KeyCode::ShiftRight => ffi::GHOSTTY_KEY_SHIFT_RIGHT,
            KeyCode::ControlRight => ffi::GHOSTTY_KEY_CONTROL_RIGHT,
            KeyCode::AltRight => ffi::GHOSTTY_KEY_ALT_RIGHT,
            KeyCode::SuperRight => ffi::GHOSTTY_KEY_META_RIGHT,
            _ => gkey,
        },
        _ => gkey,
    }
}

/// Map winit modifiers to GhosttyMods bitmask.
///
/// NOTE: Winit 0.30's `ModifiersState` does not expose CapsLock or NumLock
/// state. Those bits (GHOSTTY_MODS_CAPS_LOCK / GHOSTTY_MODS_NUM_LOCK) will
/// remain unset until winit adds support or we query X11/Wayland directly.
fn winit_mods_to_ghostty(mods: ModifiersState) -> ffi::GhosttyMods {
    let mut result: ffi::GhosttyMods = 0;
    if mods.shift_key() {
        result |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if mods.control_key() {
        result |= ffi::GHOSTTY_MODS_CTRL;
    }
    if mods.alt_key() {
        result |= ffi::GHOSTTY_MODS_ALT;
    }
    if mods.super_key() {
        result |= ffi::GHOSTTY_MODS_SUPER;
    }
    // CapsLock and NumLock: not exposed by winit 0.30 ModifiersState.
    // TODO: query platform-specific lock key state when winit adds support.
    result
}

/// Return the unshifted codepoint for a key (the character with no modifiers
/// on a US layout). The Kitty keyboard protocol needs this.
fn unshifted_codepoint(key: &Key) -> u32 {
    match key {
        Key::Named(NamedKey::Space) => ' ' as u32,
        Key::Character(s) => {
            let s = s.as_str();
            match s.to_ascii_lowercase().as_str() {
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
                "0" | ")" => '0' as u32,
                "1" | "!" => '1' as u32,
                "2" | "@" => '2' as u32,
                "3" | "#" => '3' as u32,
                "4" | "$" => '4' as u32,
                "5" | "%" => '5' as u32,
                "6" | "^" => '6' as u32,
                "7" | "&" => '7' as u32,
                "8" | "*" => '8' as u32,
                "9" | "(" => '9' as u32,
                "-" | "_" => '-' as u32,
                "=" | "+" => '=' as u32,
                "[" | "{" => '[' as u32,
                "]" | "}" => ']' as u32,
                "\\" | "|" => '\\' as u32,
                ";" | ":" => ';' as u32,
                "'" | "\"" => '\'' as u32,
                "," | "<" => ',' as u32,
                "." | ">" => '.' as u32,
                "/" | "?" => '/' as u32,
                "`" | "~" => '`' as u32,
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Map winit mouse button to ghostty mouse button constant.
fn winit_mouse_button_to_ghostty(button: winit::event::MouseButton) -> i32 {
    match button {
        winit::event::MouseButton::Left => ffi::GHOSTTY_MOUSE_BUTTON_LEFT,
        winit::event::MouseButton::Right => ffi::GHOSTTY_MOUSE_BUTTON_RIGHT,
        winit::event::MouseButton::Middle => ffi::GHOSTTY_MOUSE_BUTTON_MIDDLE,
        winit::event::MouseButton::Back => ffi::GHOSTTY_MOUSE_BUTTON_FOUR,
        winit::event::MouseButton::Forward => ffi::GHOSTTY_MOUSE_BUTTON_FIVE,
        winit::event::MouseButton::Other(n) => {
            // Map additional buttons; 6+ are less common
            match n {
                5 => ffi::GHOSTTY_MOUSE_BUTTON_SIX,
                6 => ffi::GHOSTTY_MOUSE_BUTTON_SEVEN,
                7 => ffi::GHOSTTY_MOUSE_BUTTON_EIGHT,
                _ => ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN,
            }
        }
    }
}
