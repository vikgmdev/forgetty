//! Real keyboard and mouse input for the smoke (`SendInput`): the keys go through the
//! window's message loop, `TranslateMessage` and WM_CHAR, as a person's would. A keyboard
//! layout is loaded for the UI thread, so dead keys and AltGr can be driven on a runner.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ActivateKeyboardLayout, GetKeyboardLayout, LoadKeyboardLayoutW, MapVirtualKeyExW, SendInput, HKL, INPUT, INPUT_0,
    INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KLF_ACTIVATE,
    MAPVK_VK_TO_VSC, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

pub const VK_E: u16 = 0x45;
pub const VK_A: u16 = 0x41;
pub const VK_Q: u16 = 0x51;
pub const VK_2: u16 = 0x32;
pub const VK_SPACE: u16 = 0x20;
pub const VK_OEM_6: u16 = 0xDD;
pub const VK_OEM_7: u16 = 0xDE;
const VK_RMENU: u16 = 0xA5;

#[derive(Clone, Copy)]
pub enum Step {
    /// Press and release one key.
    Tap(u16),
    /// AltGr held while one key is pressed and released.
    AltGr(u16),
}

/// The window's handle.
pub fn hwnd(window: &gpui::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

/// Load and activate the layout with this KLID (`00020409` is US-International) for the
/// calling thread. Returns the layout handle, or `None` if Windows has no such layout.
pub fn activate_layout(klid: &str) -> Option<isize> {
    let w: Vec<u16> = klid.encode_utf16().chain(std::iter::once(0)).collect();
    let hkl = unsafe { LoadKeyboardLayoutW(PCWSTR(w.as_ptr()), KLF_ACTIVATE) }.ok()?;
    unsafe {
        let _ = ActivateKeyboardLayout(hkl, KLF_ACTIVATE);
    }
    let now = unsafe { GetKeyboardLayout(0) };
    (now.0 as isize == hkl.0 as isize).then_some(hkl.0 as isize)
}

fn key_event(vk: u16, up: bool, extended: bool, hkl: isize) -> INPUT {
    let scan = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC, Some(HKL(hkl as *mut c_void))) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// Inject the steps. Returns false when Windows refused the events.
pub fn send_keys(hkl: isize, steps: &[Step]) -> bool {
    let mut ev = Vec::new();
    for s in steps {
        match *s {
            Step::Tap(vk) => {
                ev.push(key_event(vk, false, false, hkl));
                ev.push(key_event(vk, true, false, hkl));
            }
            Step::AltGr(vk) => {
                ev.push(key_event(VK_RMENU, false, true, hkl));
                ev.push(key_event(vk, false, false, hkl));
                ev.push(key_event(vk, true, false, hkl));
                ev.push(key_event(VK_RMENU, true, true, hkl));
            }
        }
    }
    unsafe { SendInput(&ev, std::mem::size_of::<INPUT>() as i32) as usize == ev.len() }
}

/// Screen position of the window's client origin.
pub fn client_origin(hwnd: HWND) -> Option<(i32, i32)> {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(hwnd, &mut p) }.as_bool().then_some((p.x, p.y))
}

pub fn move_cursor(x: i32, y: i32) -> bool {
    unsafe { SetCursorPos(x, y) }.is_ok()
}

fn mouse(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

pub fn left_down() -> bool {
    unsafe { SendInput(&[mouse(MOUSEEVENTF_LEFTDOWN)], std::mem::size_of::<INPUT>() as i32) == 1 }
}

pub fn left_up() -> bool {
    unsafe { SendInput(&[mouse(MOUSEEVENTF_LEFTUP)], std::mem::size_of::<INPUT>() as i32) == 1 }
}

pub const VK_RETURN: u16 = 0x0D;

/// Tap the letters of `word` (a-z).
pub fn tap_word(hkl: isize, word: &str) -> bool {
    let steps: Vec<Step> = word.bytes().filter(u8::is_ascii_lowercase).map(|b| Step::Tap(u16::from(b.to_ascii_uppercase()))).collect();
    send_keys(hkl, &steps)
}

pub fn current_hkl() -> isize {
    unsafe { GetKeyboardLayout(0) }.0 as isize
}

/// Switch the input method of this process to a text service profile (a TSF IME), as the
/// language bar does. `langid` is the language (0x0411 Japanese, 0x0804 Chinese Simplified).
pub fn activate_text_service(langid: u16, clsid: &str, profile: &str) -> Result<(), String> {
    use windows::core::GUID;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::TextServices::{
        CLSID_TF_InputProcessorProfiles, ITfInputProcessorProfileMgr, TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE, TF_IPPMF_FORPROCESS,
        TF_PROFILETYPE_INPUTPROCESSOR,
    };
    let clsid = GUID::try_from(clsid).map_err(|e| e.to_string())?;
    let profile = GUID::try_from(profile).map_err(|e| e.to_string())?;
    unsafe {
        let mgr: ITfInputProcessorProfileMgr =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER).map_err(|e| format!("CoCreateInstance: {e}"))?;
        mgr.ActivateProfile(
            TF_PROFILETYPE_INPUTPROCESSOR,
            langid,
            &clsid,
            &profile,
            HKL::default(),
            TF_IPPMF_FORPROCESS | TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE,
        )
        .map_err(|e| format!("ActivateProfile: {e}"))
    }
}

/// Turn the IME on in the window's input context, in the native (kana or Chinese) mode. Returns
/// a line saying what Windows reported.
pub fn ime_open_native(hwnd: HWND) -> String {
    use windows::Win32::UI::Input::Ime::{
        ImmGetContext, ImmGetOpenStatus, ImmReleaseContext, ImmSetConversionStatus, ImmSetOpenStatus, IME_CMODE_FULLSHAPE,
        IME_CMODE_NATIVE, IME_SMODE_PHRASEPREDICT,
    };
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_invalid() {
            return "no input context".into();
        }
        let set = ImmSetOpenStatus(himc, true).as_bool();
        let mode = ImmSetConversionStatus(himc, IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE, IME_SMODE_PHRASEPREDICT).as_bool();
        let open = ImmGetOpenStatus(himc).as_bool();
        let _ = ImmReleaseContext(hwnd, himc);
        format!("set_open={set} set_mode={mode} open={open}")
    }
}
