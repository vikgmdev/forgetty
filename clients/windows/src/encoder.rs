//! Keyboard and mouse encoding through the libghostty-vt encoders
//! (`forgetty_vt::ffi`): DECCKM, the Kitty keyboard protocol, release events,
//! and the X10/normal/button/any mouse modes with SGR/UTF-8/URxvt formats.
//! Modeled on `forgetty-gtk/src/input.rs`, with GPUI types instead of GDK.

use std::os::raw::c_void;

use forgetty_vt::ffi;
use gpui::{Keystroke, Modifiers};

pub struct Input {
    key_encoder: ffi::GhosttyKeyEncoder,
    key_event: ffi::GhosttyKeyEvent,
    mouse_encoder: ffi::GhosttyMouseEncoder,
    mouse_event: ffi::GhosttyMouseEvent,
    buttons_held: u16,
    last_pos: (f32, f32),
}

impl Input {
    pub fn new() -> Self {
        let mut key_encoder = std::ptr::null_mut();
        let mut key_event = std::ptr::null_mut();
        let mut mouse_encoder = std::ptr::null_mut();
        let mut mouse_event = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                ffi::ghostty_key_encoder_new(std::ptr::null(), &mut key_encoder),
                ffi::GHOSTTY_SUCCESS
            );
            assert_eq!(
                ffi::ghostty_key_event_new(std::ptr::null(), &mut key_event),
                ffi::GHOSTTY_SUCCESS
            );
            assert_eq!(
                ffi::ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse_encoder),
                ffi::GHOSTTY_SUCCESS
            );
            assert_eq!(
                ffi::ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event),
                ffi::GHOSTTY_SUCCESS
            );
        }
        Self { key_encoder, key_event, mouse_encoder, mouse_event, buttons_held: 0, last_pos: (-1.0, -1.0) }
    }

    /// Encode one key event for the terminal's current modes.
    /// `action`: `GHOSTTY_KEY_ACTION_{PRESS,REPEAT,RELEASE}`.
    pub fn encode_key(
        &mut self,
        ks: &Keystroke,
        action: i32,
        terminal: ffi::GhosttyTerminal,
    ) -> Option<Vec<u8>> {
        let gkey = gpui_key_to_ghostty(&ks.key);
        if gkey == ffi::GHOSTTY_KEY_UNIDENTIFIED {
            return None;
        }
        let mods = mods_to_ghostty(&ks.modifiers);
        let ucp = unshifted_codepoint(&ks.key);

        // Text the key produces (only printable, only on press/repeat).
        let text: Vec<u8> = if action != ffi::GHOSTTY_KEY_ACTION_RELEASE {
            match ks.key_char.as_deref() {
                Some(t) if !t.is_empty() && !t.chars().any(|c| c.is_control()) => {
                    t.as_bytes().to_vec()
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        // Shift is "consumed" when it changed the produced text (Shift+1 -> !).
        let shift_consumed = mods & ffi::GHOSTTY_MODS_SHIFT != 0
            && !text.is_empty()
            && ks.key_char.as_deref() != Some(ks.key.as_str());
        let consumed = if shift_consumed { ffi::GHOSTTY_MODS_SHIFT } else { 0 };

        unsafe {
            ffi::ghostty_key_encoder_setopt_from_terminal(self.key_encoder, terminal);
            ffi::ghostty_key_event_set_key(self.key_event, gkey);
            ffi::ghostty_key_event_set_action(self.key_event, action);
            ffi::ghostty_key_event_set_mods(self.key_event, mods);
            ffi::ghostty_key_event_set_unshifted_codepoint(self.key_event, ucp);
            ffi::ghostty_key_event_set_consumed_mods(self.key_event, consumed);
            ffi::ghostty_key_event_set_composing(self.key_event, false);
            if text.is_empty() {
                ffi::ghostty_key_event_set_utf8(self.key_event, std::ptr::null(), 0);
            } else {
                ffi::ghostty_key_event_set_utf8(self.key_event, text.as_ptr(), text.len());
            }
        }

        let mut buf = [0u8; 128];
        let mut written = 0usize;
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
        // Encoder had nothing but the key makes text: send it raw.
        if action != ffi::GHOSTTY_KEY_ACTION_RELEASE && !text.is_empty() && mods & (ffi::GHOSTTY_MODS_CTRL | ffi::GHOSTTY_MODS_SUPER) == 0 {
            return Some(text);
        }
        None
    }

    pub fn encode_focus(gained: bool) -> Option<Vec<u8>> {
        let event = if gained { ffi::GHOSTTY_FOCUS_GAINED } else { ffi::GHOSTTY_FOCUS_LOST };
        let mut buf = [0u8; 8];
        let mut written = 0usize;
        let rc =
            unsafe { ffi::ghostty_focus_encode(event, buf.as_mut_ptr(), buf.len(), &mut written) };
        (rc == ffi::GHOSTTY_SUCCESS && written > 0).then(|| buf[..written].to_vec())
    }

    /// `button`: GHOSTTY_MOUSE_BUTTON_*. `pos`: pixels inside the grid.
    pub fn encode_mouse_button(
        &mut self,
        button: i32,
        pressed: bool,
        pos: (f32, f32),
        mods: &Modifiers,
        terminal: ffi::GhosttyTerminal,
        grid_px: (u32, u32),
        cell_px: (u32, u32),
    ) -> Option<Vec<u8>> {
        if button == ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN {
            return None;
        }
        let bit = 1u16 << button.clamp(0, 15);
        if pressed {
            self.buttons_held |= bit;
        } else {
            self.buttons_held &= !bit;
        }
        self.sync_mouse(terminal, grid_px, cell_px);
        unsafe {
            ffi::ghostty_mouse_event_set_action(
                self.mouse_event,
                if pressed { ffi::GHOSTTY_MOUSE_ACTION_PRESS } else { ffi::GHOSTTY_MOUSE_ACTION_RELEASE },
            );
            ffi::ghostty_mouse_event_set_button(self.mouse_event, button);
            ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods_to_ghostty(mods));
            ffi::ghostty_mouse_event_set_position(
                self.mouse_event,
                ffi::GhosttyMousePosition { x: pos.0, y: pos.1 },
            );
        }
        self.mouse_encode()
    }

    pub fn encode_mouse_motion(
        &mut self,
        pos: (f32, f32),
        mods: &Modifiers,
        terminal: ffi::GhosttyTerminal,
        grid_px: (u32, u32),
        cell_px: (u32, u32),
    ) -> Option<Vec<u8>> {
        if (pos.0 - self.last_pos.0).abs() < 0.5 && (pos.1 - self.last_pos.1).abs() < 0.5 {
            return None;
        }
        self.last_pos = pos;
        self.sync_mouse(terminal, grid_px, cell_px);
        unsafe {
            ffi::ghostty_mouse_event_set_action(self.mouse_event, ffi::GHOSTTY_MOUSE_ACTION_MOTION);
            ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods_to_ghostty(mods));
            ffi::ghostty_mouse_event_set_position(
                self.mouse_event,
                ffi::GhosttyMousePosition { x: pos.0, y: pos.1 },
            );
            match (1..=11).find(|b| self.buttons_held & (1 << b) != 0) {
                Some(b) => ffi::ghostty_mouse_event_set_button(self.mouse_event, b),
                None => ffi::ghostty_mouse_event_clear_button(self.mouse_event),
            }
        }
        self.mouse_encode()
    }

    /// Wheel as button 4/5 press+release for apps that track the mouse.
    pub fn encode_mouse_wheel(
        &mut self,
        up: bool,
        pos: (f32, f32),
        mods: &Modifiers,
        terminal: ffi::GhosttyTerminal,
        grid_px: (u32, u32),
        cell_px: (u32, u32),
    ) -> Option<Vec<u8>> {
        self.sync_mouse(terminal, grid_px, cell_px);
        let btn = if up { ffi::GHOSTTY_MOUSE_BUTTON_FOUR } else { ffi::GHOSTTY_MOUSE_BUTTON_FIVE };
        let mut out = Vec::new();
        for action in [ffi::GHOSTTY_MOUSE_ACTION_PRESS, ffi::GHOSTTY_MOUSE_ACTION_RELEASE] {
            unsafe {
                ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods_to_ghostty(mods));
                ffi::ghostty_mouse_event_set_position(
                    self.mouse_event,
                    ffi::GhosttyMousePosition { x: pos.0, y: pos.1 },
                );
                ffi::ghostty_mouse_event_set_button(self.mouse_event, btn);
                ffi::ghostty_mouse_event_set_action(self.mouse_event, action);
            }
            if let Some(b) = self.mouse_encode() {
                out.extend_from_slice(&b);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn sync_mouse(&self, terminal: ffi::GhosttyTerminal, grid_px: (u32, u32), cell_px: (u32, u32)) {
        let size = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width: grid_px.0,
            screen_height: grid_px.1,
            cell_width: cell_px.0.max(1),
            cell_height: cell_px.1.max(1),
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let any_pressed = self.buttons_held != 0;
        let track_cell = true;
        unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(self.mouse_encoder, terminal);
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &any_pressed as *const bool as *const c_void,
            );
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL,
                &track_cell as *const bool as *const c_void,
            );
        }
    }

    fn mouse_encode(&self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 128];
        let mut written = 0usize;
        let rc = unsafe {
            ffi::ghostty_mouse_encoder_encode(
                self.mouse_encoder,
                self.mouse_event,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        (rc == ffi::GHOSTTY_SUCCESS && written > 0).then(|| buf[..written].to_vec())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_mouse_event_free(self.mouse_event);
            ffi::ghostty_mouse_encoder_free(self.mouse_encoder);
            ffi::ghostty_key_event_free(self.key_event);
            ffi::ghostty_key_encoder_free(self.key_encoder);
        }
    }
}

pub fn mods_to_ghostty(m: &Modifiers) -> ffi::GhosttyMods {
    let mut r = 0;
    if m.shift {
        r |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if m.control {
        r |= ffi::GHOSTTY_MODS_CTRL;
    }
    if m.alt {
        r |= ffi::GHOSTTY_MODS_ALT;
    }
    if m.platform {
        r |= ffi::GHOSTTY_MODS_SUPER;
    }
    r
}

pub fn mouse_button_to_ghostty(b: gpui::MouseButton) -> i32 {
    match b {
        gpui::MouseButton::Left => ffi::GHOSTTY_MOUSE_BUTTON_LEFT,
        gpui::MouseButton::Right => ffi::GHOSTTY_MOUSE_BUTTON_RIGHT,
        gpui::MouseButton::Middle => ffi::GHOSTTY_MOUSE_BUTTON_MIDDLE,
        _ => ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN,
    }
}

/// The unshifted character of a key, for the Kitty protocol.
fn unshifted_codepoint(key: &str) -> u32 {
    let mut it = key.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => c as u32,
        _ if key == "space" => ' ' as u32,
        _ => 0,
    }
}

/// GPUI key name (`Keystroke::key`) to the GhosttyKey enum.
pub fn gpui_key_to_ghostty(key: &str) -> i32 {
    use ffi::*;
    let mut it = key.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        return match c {
            'a'..='z' => GHOSTTY_KEY_A + (c as i32 - 'a' as i32),
            'A'..='Z' => GHOSTTY_KEY_A + (c as i32 - 'A' as i32),
            '0'..='9' => GHOSTTY_KEY_DIGIT_0 + (c as i32 - '0' as i32),
            '`' => GHOSTTY_KEY_BACKQUOTE,
            '\\' => GHOSTTY_KEY_BACKSLASH,
            '[' => GHOSTTY_KEY_BRACKET_LEFT,
            ']' => GHOSTTY_KEY_BRACKET_RIGHT,
            ',' => GHOSTTY_KEY_COMMA,
            '=' => GHOSTTY_KEY_EQUAL,
            '-' => GHOSTTY_KEY_MINUS,
            '.' => GHOSTTY_KEY_PERIOD,
            '\'' => GHOSTTY_KEY_QUOTE,
            ';' => GHOSTTY_KEY_SEMICOLON,
            '/' => GHOSTTY_KEY_SLASH,
            _ => GHOSTTY_KEY_UNIDENTIFIED,
        };
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<i32>().ok()) {
        if (1..=24).contains(&n) {
            return GHOSTTY_KEY_F1 + (n - 1);
        }
    }
    match key {
        "enter" => GHOSTTY_KEY_ENTER,
        "tab" => GHOSTTY_KEY_TAB,
        "space" => GHOSTTY_KEY_SPACE,
        "backspace" => GHOSTTY_KEY_BACKSPACE,
        "escape" => GHOSTTY_KEY_ESCAPE,
        "delete" => GHOSTTY_KEY_DELETE,
        "insert" => GHOSTTY_KEY_INSERT,
        "home" => GHOSTTY_KEY_HOME,
        "end" => GHOSTTY_KEY_END,
        "pageup" => GHOSTTY_KEY_PAGE_UP,
        "pagedown" => GHOSTTY_KEY_PAGE_DOWN,
        "up" => GHOSTTY_KEY_ARROW_UP,
        "down" => GHOSTTY_KEY_ARROW_DOWN,
        "left" => GHOSTTY_KEY_ARROW_LEFT,
        "right" => GHOSTTY_KEY_ARROW_RIGHT,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    }
}

/// DECSET/DECRST state of a private mode, e.g. 1 (DECCKM), 2004 (bracketed paste).
pub fn private_mode(terminal: ffi::GhosttyTerminal, mode: u16) -> bool {
    let mut v = false;
    unsafe {
        ffi::ghostty_terminal_mode_get(terminal, ffi::ghostty_mode_new(mode, false), &mut v);
    }
    v
}
