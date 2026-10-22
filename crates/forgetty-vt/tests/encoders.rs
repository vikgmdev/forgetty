//! Key and mouse encoder behavior through the C API (mirrors what the Windows
//! client relies on).

use forgetty_vt::{ffi, Terminal, ANSI_PALETTE_BLACK};
use std::os::raw::c_void;

fn term() -> Terminal {
    Terminal::new(25, 100, [ANSI_PALETTE_BLACK; 16])
}

fn encode_key(t: &Terminal, key: i32, mods: u16, text: &[u8]) -> Vec<u8> {
    unsafe {
        let mut enc = std::ptr::null_mut();
        let mut ev = std::ptr::null_mut();
        assert_eq!(ffi::ghostty_key_encoder_new(std::ptr::null(), &mut enc), ffi::GHOSTTY_SUCCESS);
        assert_eq!(ffi::ghostty_key_event_new(std::ptr::null(), &mut ev), ffi::GHOSTTY_SUCCESS);
        ffi::ghostty_key_encoder_setopt_from_terminal(enc, t.raw_handle());
        ffi::ghostty_key_event_set_key(ev, key);
        ffi::ghostty_key_event_set_action(ev, ffi::GHOSTTY_KEY_ACTION_PRESS);
        ffi::ghostty_key_event_set_mods(ev, mods);
        ffi::ghostty_key_event_set_utf8(ev, text.as_ptr(), text.len());
        let mut buf = [0u8; 64];
        let mut n = 0usize;
        ffi::ghostty_key_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut n);
        ffi::ghostty_key_event_free(ev);
        ffi::ghostty_key_encoder_free(enc);
        buf[..n].to_vec()
    }
}

#[test]
fn arrows_and_kitty() {
    let mut t = term();
    assert_eq!(encode_key(&t, ffi::GHOSTTY_KEY_ARROW_UP, 0, b""), b"\x1b[A");
    assert_eq!(encode_key(&t, ffi::GHOSTTY_KEY_ARROW_LEFT, ffi::GHOSTTY_MODS_CTRL, b""), b"\x1b[1;5D");
    assert_eq!(encode_key(&t, ffi::GHOSTTY_KEY_C, ffi::GHOSTTY_MODS_CTRL, b""), [3]);
    t.feed(b"\x1b[?1h"); // DECCKM
    assert_eq!(encode_key(&t, ffi::GHOSTTY_KEY_ARROW_UP, 0, b""), b"\x1bOA");
    t.feed(b"\x1b[?1l\x1b[>1u"); // kitty disambiguate
    assert_eq!(encode_key(&t, ffi::GHOSTTY_KEY_ESCAPE, 0, b""), b"\x1b[27u");
}

#[test]
fn sgr_mouse() {
    let mut t = term();
    t.feed(b"\x1b[?1000h\x1b[?1006h");
    unsafe {
        let mut enc = std::ptr::null_mut();
        let mut ev = std::ptr::null_mut();
        ffi::ghostty_mouse_encoder_new(std::ptr::null(), &mut enc);
        ffi::ghostty_mouse_event_new(std::ptr::null(), &mut ev);
        ffi::ghostty_mouse_encoder_setopt_from_terminal(enc, t.raw_handle());
        let size = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width: 800,
            screen_height: 400,
            cell_width: 8,
            cell_height: 16,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        ffi::ghostty_mouse_encoder_setopt(enc, ffi::GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size as *const _ as *const c_void);
        ffi::ghostty_mouse_event_set_action(ev, ffi::GHOSTTY_MOUSE_ACTION_PRESS);
        ffi::ghostty_mouse_event_set_button(ev, ffi::GHOSTTY_MOUSE_BUTTON_LEFT);
        ffi::ghostty_mouse_event_set_mods(ev, 0);
        ffi::ghostty_mouse_event_set_position(ev, ffi::GhosttyMousePosition { x: 41.0, y: 49.0 });
        let mut buf = [0u8; 64];
        let mut n = 0usize;
        ffi::ghostty_mouse_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut n);
        assert_eq!(&buf[..n], b"\x1b[<0;6;4M");
        ffi::ghostty_mouse_event_free(ev);
        ffi::ghostty_mouse_encoder_free(enc);
    }
}
