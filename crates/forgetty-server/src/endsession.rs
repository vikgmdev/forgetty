//! Windows logoff and shutdown.
//!
//! The server has no console, so it does not get the console control events for logoff and
//! shutdown. Windows tells a process with a top-level window instead: `WM_QUERYENDSESSION` (may
//! we end?) and `WM_ENDSESSION` (we are ending, or the end was cancelled). A hidden window on
//! its own thread listens for them.
//!
//! The window is a top-level window that is never shown. A message-only window (`HWND_MESSAGE`)
//! would not do: the end-of-session messages are broadcast to top-level windows only.
//!
//! When the session really ends, the shells die with it, and each exit would shrink the saved
//! layout. So `WM_ENDSESSION` with `wParam != 0` runs a callback that saves the layout and
//! freezes it (`SessionManager::begin_shutdown`), and the server then comes up with all its panes
//! at the next login. `WM_QUERYENDSESSION` only agrees: another program can still cancel the
//! end, and a layout frozen for nothing would never be saved again.

use std::io;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, CW_USEDEFAULT, MSG, WINDOW_EX_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION,
    WNDCLASSW, WS_POPUP,
};

/// What the window does with a message.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Reaction {
    /// Answer "yes, the session may end".
    Agree,
    /// The session is ending now: save and freeze the layout.
    Freeze,
    /// Not ours: default handling.
    Ignore,
}

/// The decision for one message. `wparam` of `WM_ENDSESSION` is true when the session really ends
/// and false when another program cancelled it.
pub fn react(msg: u32, wparam: usize) -> Reaction {
    match msg {
        WM_QUERYENDSESSION => Reaction::Agree,
        WM_ENDSESSION if wparam != 0 => Reaction::Freeze,
        _ => Reaction::Ignore,
    }
}

type Callback = Box<dyn Fn() + Send + Sync>;

static ON_END: OnceLock<Callback> = OnceLock::new();

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match react(msg, wparam.0) {
        // Non-zero: the session may end.
        Reaction::Agree => LRESULT(1),
        Reaction::Freeze => {
            if let Some(f) = ON_END.get() {
                // A panic must not unwind through the window procedure.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()));
            }
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows gave this procedure.
        Reaction::Ignore => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Start the listener thread. `on_end` runs when the session is ending, on that thread. Call it
/// once. Returns when the window exists, or with the reason it could not be made.
pub fn spawn(on_end: impl Fn() + Send + Sync + 'static) -> io::Result<()> {
    if ON_END.set(Box::new(on_end)).is_err() {
        return Err(io::Error::other("the end-of-session listener is already running"));
    }
    let (tx, rx) = std::sync::mpsc::channel::<io::Result<()>>();
    std::thread::Builder::new().name("end-session".into()).spawn(move || {
        // SAFETY: plain window calls on this thread; the loop below is the only user.
        unsafe {
            let made = (|| -> io::Result<()> {
                let instance = GetModuleHandleW(PCWSTR::null())
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: instance.into(),
                    lpszClassName: w!("ForgettyServerEndSession"),
                    ..Default::default()
                };
                if RegisterClassW(&class) == 0 {
                    return Err(io::Error::last_os_error());
                }
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("ForgettyServerEndSession"),
                    w!("Forgetty server"),
                    WS_POPUP,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    0,
                    0,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .map_err(|e| io::Error::other(e.to_string()))?;
                Ok(())
            })();
            let ok = made.is_ok();
            let _ = tx.send(made);
            if !ok {
                return;
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    })?;
    rx.recv().map_err(|_| io::Error::other("the end-of-session thread stopped at start"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_agrees_and_a_real_end_freezes_and_a_cancelled_end_does_nothing() {
        assert_eq!(react(WM_QUERYENDSESSION, 0), Reaction::Agree);
        assert_eq!(react(WM_QUERYENDSESSION, 1), Reaction::Agree);
        assert_eq!(react(WM_ENDSESSION, 1), Reaction::Freeze);
        // Another program cancelled the end: the layout must stay open for saves.
        assert_eq!(react(WM_ENDSESSION, 0), Reaction::Ignore);
        assert_eq!(react(0x0001 /* WM_CREATE */, 1), Reaction::Ignore);
    }

    /// The real window procedure, called the way Windows calls it.
    #[test]
    fn the_window_procedure_runs_the_callback_once_for_a_real_end_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        spawn(move || {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .expect("start the listener");
        // SAFETY: a null window handle is fine for the procedure, which does not use it for
        // these messages.
        unsafe {
            let hwnd = HWND::default();
            assert_eq!(wnd_proc(hwnd, WM_QUERYENDSESSION, WPARAM(0), LPARAM(0)).0, 1);
            assert_eq!(hits.load(Ordering::SeqCst), 0, "a query does not freeze");
            let _ = wnd_proc(hwnd, WM_ENDSESSION, WPARAM(0), LPARAM(0));
            assert_eq!(hits.load(Ordering::SeqCst), 0, "a cancelled end does not freeze");
            assert_eq!(wnd_proc(hwnd, WM_ENDSESSION, WPARAM(1), LPARAM(0)).0, 0);
            assert_eq!(hits.load(Ordering::SeqCst), 1, "a real end freezes once");
        }
        // A second listener is refused (one per process).
        assert!(spawn(|| {}).is_err());
    }
}
