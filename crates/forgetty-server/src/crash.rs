//! A last line in the log when the server dies.
//!
//! A panic goes through the tracing log, so it shows in the same file as the
//! rest. A fatal signal (SIGSEGV from native or Zig code, SIGABRT, SIGBUS,
//! SIGILL, SIGFPE) kills the process before any Rust code can report it, and
//! the default handler prints nothing. The handler here writes one line to
//! stderr (the log file, or the journal under systemd) with `write(2)` only,
//! then lets the old handler or the default action go on.

use std::sync::atomic::{AtomicUsize, Ordering};

const SIGNALS: [libc::c_int; 5] =
    [libc::SIGSEGV, libc::SIGABRT, libc::SIGBUS, libc::SIGILL, libc::SIGFPE];

/// Old actions, by signal number (the numbers are below 32 on Linux).
static mut OLD: [Option<libc::sigaction>; 32] = [None; 32];
/// Set while the handler runs. A second fault inside it skips the report.
static IN_HANDLER: AtomicUsize = AtomicUsize::new(0);

static INSTALLED: std::sync::Once = std::sync::Once::new();

/// Install the panic hook and the fatal signal handlers, once. A second call
/// does nothing: it would save our own handler as the "old" one and call it
/// again from inside the handler.
pub fn install() {
    INSTALLED.call_once(|| {
        install_panic_hook();
        install_signal_handlers();
    });
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(thread = thread.name().unwrap_or("<unnamed>"), "server panic: {info}");
        previous(info);
    }));
}

fn install_signal_handlers() {
    for sig in SIGNALS {
        // SAFETY: plain sigaction calls with a zeroed struct we fill in. The
        // handler is `extern "C"` and only calls async-signal-safe functions.
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_fatal_signal as *const () as usize;
            act.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_NODEFER;
            libc::sigemptyset(&mut act.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, &act, &mut old) == 0 {
                let slot = std::ptr::addr_of_mut!(OLD);
                (*slot)[sig as usize] = Some(old);
            }
        }
    }
}

extern "C" fn on_fatal_signal(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    if IN_HANDLER.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut buf = [0u8; 160];
        let mut n = 0;
        n = put(&mut buf, n, b"forgetty-server: fatal signal ");
        n = put_num(&mut buf, n, sig as u64);
        n = put(&mut buf, n, b" (");
        n = put(&mut buf, n, name(sig));
        n = put(&mut buf, n, b") pid ");
        // SAFETY: getpid takes no arguments and is async-signal-safe.
        n = put_num(&mut buf, n, unsafe { libc::getpid() } as u64);
        if !info.is_null() {
            // SAFETY: the kernel gives a valid siginfo with SA_SIGINFO.
            let addr = unsafe { (*info).si_addr() } as usize;
            n = put(&mut buf, n, b" addr 0x");
            n = put_hex(&mut buf, n, addr as u64);
        }
        n = put(&mut buf, n, b"\n");
        // SAFETY: write(2) on fd 2 with a buffer of `n` valid bytes.
        unsafe {
            libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), n);
        }
    }
    // SAFETY: OLD is written once at install, before any signal can arrive
    // here, and only read afterwards.
    let old = unsafe { (*std::ptr::addr_of!(OLD))[sig as usize] };
    // SAFETY: calling a saved action, then restoring the default one and
    // raising the signal again, so the process still dies of it.
    unsafe {
        if let Some(o) = old {
            if o.sa_sigaction != libc::SIG_DFL && o.sa_sigaction != libc::SIG_IGN {
                if o.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                        std::mem::transmute(o.sa_sigaction);
                    f(sig, info, ctx);
                } else {
                    // A plain `sa_handler`: it takes the signal number only.
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(o.sa_sigaction);
                    f(sig);
                }
            }
        }
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut dfl.sa_mask);
        libc::sigaction(sig, &dfl, std::ptr::null_mut());
        libc::raise(sig);
    }
}

fn name(sig: libc::c_int) -> &'static [u8] {
    match sig {
        libc::SIGSEGV => b"SIGSEGV",
        libc::SIGABRT => b"SIGABRT",
        libc::SIGBUS => b"SIGBUS",
        libc::SIGILL => b"SIGILL",
        libc::SIGFPE => b"SIGFPE",
        _ => b"signal",
    }
}

fn put(buf: &mut [u8], at: usize, s: &[u8]) -> usize {
    let n = s.len().min(buf.len() - at);
    buf[at..at + n].copy_from_slice(&s[..n]);
    at + n
}

fn put_num(buf: &mut [u8], at: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    put(buf, at, &tmp[i..])
}

fn put_hex(buf: &mut [u8], at: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 16];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b"0123456789abcdef"[(v & 0xf) as usize];
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    put(buf, at, &tmp[i..])
}
