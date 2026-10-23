//! The console of a GUI-subsystem build. The release exe has no console of its own. When a
//! shell started it, `attach_parent` joins that shell's console, so `--smoke` output and
//! `eprintln!` still reach it. Without a parent console (Start menu) it does nothing and
//! stderr stays closed, which also drops libghostty-vt's Zig log output. Only the CLI modes
//! (`--smoke`) attach: a normal launch from a terminal must not join its console, or Ctrl+C
//! and closing the tab would kill the app and its shells.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const GENERIC_READ_WRITE: u32 = 0xC000_0000;
    const FILE_SHARE_READ_WRITE: u32 = 3;
    const OPEN_EXISTING: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetStdHandle(which: u32, handle: isize) -> i32;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    const MB_OK_ICONERROR: u32 = 0x10;

    /// An error box. A GUI-subsystem exe has no stderr to show an error on.
    pub fn error_box(text: &str) {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe { MessageBoxW(0, wide(text).as_ptr(), wide("Forgetty").as_ptr(), MB_OK_ICONERROR) };
    }

    pub fn attach_parent() -> bool {
        // SAFETY: plain Win32 calls with valid arguments.
        unsafe {
            // Fails when there is no parent console, or when this process has one already.
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return false;
            }
            // A redirected handle (pipe, file) is already valid: keep it. A missing one now
            // points at the console.
            let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let h = GetStdHandle(which);
                if h == 0 || h == -1 {
                    let con = CreateFileW(conout.as_ptr(), GENERIC_READ_WRITE, FILE_SHARE_READ_WRITE, std::ptr::null(), OPEN_EXISTING, 0, 0);
                    if con != -1 {
                        SetStdHandle(which, con);
                    }
                }
            }
        }
        true
    }

    /// Process ids attached to this process's console. Empty when it has none.
    pub fn process_ids() -> Vec<u32> {
        let mut ids = vec![0u32; 64];
        loop {
            // SAFETY: `ids` holds `len` u32.
            let n = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) } as usize;
            // More processes than room: the return value is the size needed.
            if n > ids.len() {
                ids.resize(n + 16, 0);
                continue;
            }
            ids.truncate(n);
            return ids;
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn error_box(_text: &str) {}
    pub fn attach_parent() -> bool {
        false
    }
    pub fn process_ids() -> Vec<u32> {
        Vec::new()
    }
}

pub use imp::{attach_parent, error_box, process_ids};
