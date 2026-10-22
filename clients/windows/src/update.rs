//! "Check for updates" in the app: the one HTTPS request (WinHTTP, system
//! proxy and certificate store) and the browser hand-off. Runs only when the
//! user clicks. The app never downloads or installs anything by itself, and
//! sends no telemetry: the request carries a User-Agent with the app version.

pub use forgetty_shell_core::update::UpdateState;
use forgetty_shell_core::update::{newest_release, API_HOST, API_PATH, MAX_BODY, RELEASES_URL};

/// Blocking: run on a worker thread.
pub fn check(current: &str) -> UpdateState {
    match fetch().and_then(|body| newest_release(&body, current)) {
        Ok(Some((version, url))) => UpdateState::Available { version, url },
        Ok(None) => UpdateState::UpToDate,
        Err(e) => UpdateState::Failed(e),
    }
}

/// Open a release page in the browser. Anything else is refused.
pub fn open_release_page(url: &str) -> bool {
    if !url.starts_with(RELEASES_URL) || url.chars().any(|c| c.is_control() || c == ' ') {
        return false;
    }
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    h.0 as isize > 32
}

fn fetch() -> Result<String, String> {
    use std::ffi::c_void;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Networking::WinHttp::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }
    unsafe {
        let agent = wide(&format!("forgetty-win/{}", env!("CARGO_PKG_VERSION")));
        let session = Handle(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err("cannot start the HTTP client".into());
        }
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000);
        let host = wide(API_HOST);
        let conn = Handle(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
        if conn.0.is_null() {
            return Err("cannot reach github.com".into());
        }
        let path = wide(API_PATH);
        let req = Handle(WinHttpOpenRequest(conn.0, w!("GET"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err("cannot build the request".into());
        }
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n".encode_utf16().collect();
        let _ = WinHttpAddRequestHeaders(req.0, &headers, WINHTTP_ADDREQ_FLAG_ADD);
        WinHttpSendRequest(req.0, None, None, 0, 0, 0).map_err(|e| format!("request failed: {e}"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("no answer: {e}"))?;
        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("no status: {e}"))?;
        if status != 200 {
            return Err(format!("GitHub answered HTTP {status}"));
        }
        let mut body = Vec::new();
        loop {
            let mut avail = 0u32;
            WinHttpQueryDataAvailable(req.0, &mut avail).map_err(|e| format!("read failed: {e}"))?;
            if avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail as usize];
            let mut read = 0u32;
            WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, avail, &mut read).map_err(|e| format!("read failed: {e}"))?;
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > MAX_BODY {
                return Err("the answer is too large".into());
            }
        }
        String::from_utf8(body).map_err(|_| "the answer is not text".to_string())
    }
}

