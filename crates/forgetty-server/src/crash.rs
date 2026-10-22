//! A last line in the log when the server dies.
//!
//! A panic goes through the tracing log, so it shows in the same file as the
//! rest. A fatal signal (SIGSEGV from native or Zig code, SIGABRT, SIGBUS,
//! SIGILL, SIGFPE) kills the process before any Rust code can report it, and
//! the default handler prints nothing. The handler here writes one line to
//! stderr (the log file, or the journal under systemd) with `write(2)` only,
//! then lets the old handler or the default action go on.
//!
//! The line names the library that holds the fault address and the instruction
//! pointer, with the offset in it (`dladdr`). A crash in a shared library (the
//! Zig terminal library, for example) then says which one and where, and the
//! offset can be looked up with `objdump` or `addr2line`. `dladdr` is not
//! strictly async-signal-safe (it takes the loader lock). This is a one-shot
//! line written just before the process dies, so the small risk is accepted.

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
        let mut buf = [0u8; 640];
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
            n = put_module(&mut buf, n, addr);
        }
        if let Some(pc) = fault_pc(ctx) {
            n = put(&mut buf, n, b" pc 0x");
            n = put_hex(&mut buf, n, pc as u64);
            n = put_module(&mut buf, n, pc);
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

/// The instruction pointer at the time of the fault, from the signal context.
#[allow(unused_variables)]
fn fault_pc(ctx: *mut libc::c_void) -> Option<usize> {
    if ctx.is_null() {
        return None;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        // SAFETY: with SA_SIGINFO the third argument points at a ucontext_t.
        let uc = unsafe { &*(ctx as *const libc::ucontext_t) };
        return Some(uc.uc_mcontext.gregs[libc::REG_RIP as usize] as usize);
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        // SAFETY: as above.
        let uc = unsafe { &*(ctx as *const libc::ucontext_t) };
        return Some(uc.uc_mcontext.pc as usize);
    }
    #[allow(unreachable_code)]
    None
}

/// ` in <library>+0x<offset>` for the library that holds `addr`, or ` in ?`.
fn put_module(buf: &mut [u8], at: usize, addr: usize) -> usize {
    let mut n = put(buf, at, b" in ");
    // SAFETY: dladdr only reads the loader's tables and fills `info`. `dli_fname`, when set,
    // is a NUL-terminated string owned by the loader.
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(addr as *const libc::c_void, &mut info) != 0 && !info.dli_fname.is_null() {
            let name = std::ffi::CStr::from_ptr(info.dli_fname).to_bytes();
            n = put(buf, n, name);
            n = put(buf, n, b"+0x");
            n = put_hex(buf, n, addr.wrapping_sub(info.dli_fbase as usize) as u64);
            return n;
        }
    }
    put(buf, n, b"?")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_and_offset_name_the_library() {
        let mut buf = [0u8; 512];
        // `getpid` lives in libc, a shared library.
        let n = put_module(&mut buf, 0, libc::getpid as usize);
        let line = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(line.starts_with(" in "), "{line}");
        assert!(line.contains("+0x"), "{line}");
        assert!(line.contains("libc") || line.contains("libpthread"), "{line}");
        // An address in no library is marked, not garbage.
        let n = put_module(&mut buf, 0, 8);
        assert_eq!(&buf[..n], b" in ?");
    }

    /// The child half of `a_fatal_signal_line_names_module_and_pc`. It dies of SIGILL.
    #[test]
    fn child_dies_of_sigill() {
        if std::env::var_os("FORGETTY_CRASH_CHILD").is_none() {
            return;
        }
        install();
        // SAFETY: an undefined instruction on purpose, to raise SIGILL.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!("ud2");
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("udf #0");
        }
    }

    #[test]
    fn a_fatal_signal_line_names_module_and_pc() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", "crash::tests::child_dies_of_sigill", "--nocapture", "--test-threads=1"])
            .env("FORGETTY_CRASH_CHILD", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        let line = err.lines().find(|l| l.contains("fatal signal")).unwrap_or_else(|| panic!("no crash line in: {err}"));
        assert!(line.contains("(SIGILL)"), "{line}");
        assert!(line.contains(" pc 0x") && line.contains(" in ") && line.contains("+0x"), "{line}");
        assert!(!out.status.success());
    }
}
