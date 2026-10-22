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
//! pointer, with the offset in it. The names come from a fixed-size table that
//! `install` builds with `dl_iterate_phdr` (the libraries loaded at that time).
//! The handler only reads that table: no `dladdr`, no allocation and no loader
//! lock, because the faulting thread may hold the allocator lock while another
//! thread holds the loader lock (a `dlopen` that waits for the allocator). An
//! address in a library loaded after `install` prints as `?`.
//!
//! The handler also sets `alarm(3)` first. If anything later hangs (a saved
//! handler that never returns, for example), the default SIGALRM action kills
//! the process, so a supervisor such as systemd sees it die and restarts it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const SIGNALS: [libc::c_int; 5] =
    [libc::SIGSEGV, libc::SIGABRT, libc::SIGBUS, libc::SIGILL, libc::SIGFPE];

/// Old actions, by signal number (the numbers are below 32 on Linux).
static mut OLD: [Option<libc::sigaction>; 32] = [None; 32];
/// Set while the handler runs. A second fault inside it skips the report.
static IN_HANDLER: AtomicUsize = AtomicUsize::new(0);
/// Set once the first thread has written its line. Later threads wait on it (bounded).
static LINE_DONE: AtomicBool = AtomicBool::new(false);
/// How long a second faulting thread waits for the first thread's line.
const SECOND_THREAD_WAIT_MS: u32 = 100;

static INSTALLED: std::sync::Once = std::sync::Once::new();
static MODULES_BUILT: std::sync::Once = std::sync::Once::new();

/// Seconds the handler gets before SIGALRM kills the process.
const HANDLER_DEADLINE_SECS: libc::c_uint = 3;

const MAX_MODULES: usize = 192;
const NAME_LEN: usize = 120;

/// One loaded library: its address range and the load bias (what the offset is relative to).
#[derive(Clone, Copy)]
struct Module {
    start: usize,
    end: usize,
    bias: usize,
    name_len: usize,
    /// The last `NAME_LEN` bytes of the path.
    name: [u8; NAME_LEN],
}

const NO_MODULE: Module = Module { start: 0, end: 0, bias: 0, name_len: 0, name: [0; NAME_LEN] };

/// Written once by `build_module_table` before any handler is installed, read by the handler.
static mut MODULES: [Module; MAX_MODULES] = [NO_MODULE; MAX_MODULES];
static MODULE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Install the panic hook and the fatal signal handlers, once. A second call
/// does nothing: it would save our own handler as the "old" one and call it
/// again from inside the handler.
pub fn install() {
    INSTALLED.call_once(|| {
        build_module_table();
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

/// Fill `MODULES` from the loader's list. Runs once, outside any signal handler.
fn build_module_table() {
    MODULES_BUILT.call_once(|| {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "main".into());
        let _ = MAIN_NAME.set(exe);
        // SAFETY: the callback only writes to `MODULES` and `MODULE_COUNT`, and this runs once,
        // before the handlers exist, so nothing reads them yet.
        unsafe {
            libc::dl_iterate_phdr(Some(collect_module), std::ptr::null_mut());
        }
    });
}

/// `dl_iterate_phdr` callback: record one library (the loader reports an empty name for the
/// main program).
unsafe extern "C" fn collect_module(
    info: *mut libc::dl_phdr_info,
    _size: libc::size_t,
    _data: *mut libc::c_void,
) -> libc::c_int {
    let n = MODULE_COUNT.load(Ordering::Relaxed);
    if n >= MAX_MODULES || info.is_null() {
        return 0;
    }
    let info = &*info;
    let (mut start, mut end) = (usize::MAX, 0usize);
    for i in 0..info.dlpi_phnum as usize {
        let ph = &*info.dlpi_phdr.add(i);
        if ph.p_type == libc::PT_LOAD {
            let lo = info.dlpi_addr as usize + ph.p_vaddr as usize;
            start = start.min(lo);
            end = end.max(lo + ph.p_memsz as usize);
        }
    }
    if start >= end {
        return 0;
    }
    let name: &[u8] = if info.dlpi_name.is_null() || *info.dlpi_name == 0 {
        MAIN_NAME.get().map(|s| s.as_bytes()).unwrap_or(b"main")
    } else {
        std::ffi::CStr::from_ptr(info.dlpi_name).to_bytes()
    };
    let tail = &name[name.len().saturating_sub(NAME_LEN)..];
    let mut m = NO_MODULE;
    m.start = start;
    m.end = end;
    m.bias = info.dlpi_addr as usize;
    m.name_len = tail.len();
    m.name[..tail.len()].copy_from_slice(tail);
    let slot = std::ptr::addr_of_mut!(MODULES);
    (*slot)[n] = m;
    MODULE_COUNT.store(n + 1, Ordering::Release);
    0
}

static MAIN_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

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
        // First thing: a deadline. If any step below hangs, SIGALRM (default action) kills us.
        // Make sure SIGALRM really has the default action and is not blocked on this thread,
        // even if other code changed either one.
        // SAFETY: sigaction, sigemptyset, sigaddset, pthread_sigmask and alarm are
        // async-signal-safe, and the structs are local and zeroed.
        unsafe {
            let mut dfl: libc::sigaction = std::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut dfl.sa_mask);
            libc::sigaction(libc::SIGALRM, &dfl, std::ptr::null_mut());
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGALRM);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::alarm(HANDLER_DEADLINE_SECS);
        }
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
        LINE_DONE.store(true, Ordering::SeqCst);
    } else {
        // A later fault must not re-raise before the first thread's line is written: a
        // heap-corruption cascade can kill the process within microseconds. Wait, bounded.
        let ts = libc::timespec { tv_sec: 0, tv_nsec: 1_000_000 };
        let mut waited = 0;
        while !LINE_DONE.load(Ordering::SeqCst) && waited < SECOND_THREAD_WAIT_MS {
            // SAFETY: nanosleep is async-signal-safe and `ts` is valid.
            unsafe {
                libc::nanosleep(&ts, std::ptr::null_mut());
            }
            waited += 1;
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

/// ` in <library>+0x<offset>` for the library that holds `addr`, or ` in ?`. Reads the table
/// built at install: no lock, no allocation.
fn put_module(buf: &mut [u8], at: usize, addr: usize) -> usize {
    let mut n = put(buf, at, b" in ");
    let count = MODULE_COUNT.load(Ordering::Acquire).min(MAX_MODULES);
    // SAFETY: MODULES is written before MODULE_COUNT is published (Release/Acquire) and never
    // changes afterwards.
    let table = unsafe { &*std::ptr::addr_of!(MODULES) };
    for m in &table[..count] {
        if addr >= m.start && addr < m.end {
            n = put(buf, n, &m.name[..m.name_len]);
            n = put(buf, n, b"+0x");
            return put_hex(buf, n, addr.wrapping_sub(m.bias) as u64);
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
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    #[test]
    fn module_and_offset_name_the_library() {
        build_module_table();
        let mut buf = [0u8; 512];
        // `getpid` lives in libc, a shared library.
        let n = put_module(&mut buf, 0, libc::getpid as *const () as usize);
        let line = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(line.starts_with(" in "), "{line}");
        assert!(line.contains("+0x"), "{line}");
        assert!(line.contains("libc"), "{line}");
        // The main program is in the table too.
        let n = put_module(&mut buf, 0, module_and_offset_name_the_library as *const () as usize);
        assert!(String::from_utf8_lossy(&buf[..n]).contains("+0x"));
        // An address in no library is marked, not garbage.
        let n = put_module(&mut buf, 0, 8);
        assert_eq!(&buf[..n], b" in ?");
    }

    /// Run this test binary again with one child test and `FORGETTY_CRASH_CHILD=<mode>`. Returns
    /// stderr, the exit status and how long the child lived (it is killed after 5 s, then the
    /// test fails on the time).
    fn run_child(mode: &str, test: &str) -> (String, std::process::ExitStatus, Duration) {
        let exe = std::env::current_exe().unwrap();
        let t0 = Instant::now();
        let mut child = std::process::Command::new(exe)
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env("FORGETTY_CRASH_CHILD", mode)
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut err = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut err, &mut s).ok();
            s
        });
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            if t0.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let st = child.wait().unwrap();
                return (reader.join().unwrap(), st, Duration::from_secs(99));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (reader.join().unwrap(), status, t0.elapsed())
    }

    fn die_of_sigill() {
        // No core file: a crash handler such as apport would add seconds to the test.
        // SAFETY: setrlimit with a valid struct.
        unsafe {
            let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::setrlimit(libc::RLIMIT_CORE, &none);
        }
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

    /// The child half of the tests below, picked by `FORGETTY_CRASH_CHILD`.
    #[test]
    fn child_dies() {
        let Ok(mode) = std::env::var("FORGETTY_CRASH_CHILD") else { return };
        match mode.as_str() {
            "plain" => {
                install();
                die_of_sigill();
            }
            // Another thread sits inside `dl_iterate_phdr` for good: it holds the loader lock.
            "loader_lock" => {
                install();
                static INSIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                unsafe extern "C" fn hold(_: *mut libc::dl_phdr_info, _: libc::size_t, _: *mut libc::c_void) -> libc::c_int {
                    INSIDE.store(true, Ordering::SeqCst);
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                std::thread::spawn(|| unsafe {
                    libc::dl_iterate_phdr(Some(hold), std::ptr::null_mut());
                });
                while !INSIDE.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                die_of_sigill();
            }
            // Another thread is inside `dlopen`, in a library constructor that never returns. glibc
            // runs constructors with `dl_load_lock` held, the lock `dladdr` takes.
            "dlopen_constructor" => {
                install();
                let so = std::env::var("FORGETTY_CRASH_SO").unwrap();
                std::thread::spawn(move || {
                    let c = std::ffi::CString::new(so).unwrap();
                    unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
                });
                std::thread::sleep(Duration::from_millis(500));
                die_of_sigill();
            }
            // A handler saved from before `install` that never returns.
            "stuck_old_handler" => {
                extern "C" fn stuck(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
                    loop {
                        unsafe { libc::pause() };
                    }
                }
                unsafe {
                    let mut act: libc::sigaction = std::mem::zeroed();
                    act.sa_sigaction = stuck as *const () as usize;
                    act.sa_flags = libc::SA_SIGINFO;
                    libc::sigemptyset(&mut act.sa_mask);
                    libc::sigaction(libc::SIGILL, &act, std::ptr::null_mut());
                }
                install();
                die_of_sigill();
            }
            // Some other code ignores SIGALRM and blocks it on this thread. The deadline must hold.
            "alarm_hostile" => {
                extern "C" fn stuck(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
                    loop {
                        unsafe { libc::pause() };
                    }
                }
                unsafe {
                    let mut act: libc::sigaction = std::mem::zeroed();
                    act.sa_sigaction = stuck as *const () as usize;
                    act.sa_flags = libc::SA_SIGINFO;
                    libc::sigemptyset(&mut act.sa_mask);
                    libc::sigaction(libc::SIGILL, &act, std::ptr::null_mut());
                }
                install();
                unsafe {
                    libc::signal(libc::SIGALRM, libc::SIG_IGN);
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGALRM);
                    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                }
                die_of_sigill();
            }
            // A first thread is "in the handler" (a stand-in: the counter is set by hand) and writes
            // its line 50 ms later. This thread faults meanwhile and must wait for that line.
            "second_thread_waits" => {
                install();
                IN_HANDLER.store(1, Ordering::SeqCst);
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(50));
                    let msg = b"first-thread-line\n";
                    unsafe { libc::write(libc::STDERR_FILENO, msg.as_ptr().cast(), msg.len()) };
                    LINE_DONE.store(true, Ordering::SeqCst);
                });
                die_of_sigill();
            }
            // The first thread never finishes: the second one gives up after the bounded wait.
            "second_thread_gives_up" => {
                install();
                IN_HANDLER.store(1, Ordering::SeqCst);
                die_of_sigill();
            }
            _ => {}
        }
    }

    #[test]
    fn a_fatal_signal_line_names_module_and_pc() {
        let (err, status, _) = run_child("plain", "crash::tests::child_dies");
        let line = err.lines().find(|l| l.contains("fatal signal")).unwrap_or_else(|| panic!("no crash line in: {err}"));
        assert!(line.contains("(SIGILL)"), "{line}");
        assert!(line.contains(" pc 0x") && line.contains(" in ") && line.contains("+0x") && !line.contains(" in ?"), "{line}");
        assert_eq!(status.signal(), Some(libc::SIGILL));
    }

    #[test]
    fn the_handler_does_not_wait_for_the_loader_lock() {
        let (err, status, took) = run_child("loader_lock", "crash::tests::child_dies");
        assert!(took < Duration::from_secs(5), "the child hung ({took:?}): {err}");
        assert!(err.contains("fatal signal 4 (SIGILL)"), "{err}");
        assert_eq!(status.signal(), Some(libc::SIGILL));
    }

    /// The same, with the real loader lock: a library whose constructor never returns holds
    /// `dl_load_lock` for as long as the `dlopen` runs. Needs a C compiler (skipped without one).
    #[test]
    fn the_handler_does_not_wait_for_dl_load_lock() {
        let dir = std::env::temp_dir().join(format!("forgetty-crash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hold.c");
        let so = dir.join("libhold.so");
        std::fs::write(&src, "#include <unistd.h>\n__attribute__((constructor)) static void hold(void) { for (;;) sleep(1); }\n").unwrap();
        let built = std::process::Command::new("cc").args(["-shared", "-fPIC", "-o"]).arg(&so).arg(&src).status();
        if !built.is_ok_and(|s| s.success()) {
            eprintln!("no C compiler: skipping the dl_load_lock test");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let t0 = Instant::now();
        let mut child = std::process::Command::new(exe)
            .args(["--exact", "crash::tests::child_dies", "--nocapture", "--test-threads=1"])
            .env("FORGETTY_CRASH_CHILD", "dlopen_constructor")
            .env("FORGETTY_CRASH_SO", &so)
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut err = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut err, &mut s).ok();
            s
        });
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            if t0.elapsed() > Duration::from_secs(8) {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let took = t0.elapsed();
        let err = reader.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // A blocked handler would be killed by the 3 s deadline (SIGALRM), not die of SIGILL.
        assert!(took < Duration::from_secs(5), "the child hung ({took:?}): {err}");
        assert!(err.contains("fatal signal 4 (SIGILL)"), "{err}");
        assert_eq!(status.signal(), Some(libc::SIGILL), "{err}");
    }

    #[test]
    fn a_hung_handler_is_killed_by_the_deadline() {
        let (err, status, took) = run_child("stuck_old_handler", "crash::tests::child_dies");
        assert!(took < Duration::from_secs(5), "no deadline ({took:?}): {err}");
        assert!(took >= Duration::from_secs(2), "died too early ({took:?})");
        assert!(err.contains("fatal signal 4 (SIGILL)"), "{err}");
        assert_eq!(status.signal(), Some(libc::SIGALRM));
    }

    #[test]
    fn a_hostile_sigalrm_setup_does_not_defeat_the_deadline() {
        let (err, status, took) = run_child("alarm_hostile", "crash::tests::child_dies");
        assert!(took < Duration::from_secs(5), "no deadline ({took:?}): {err}");
        assert!(took >= Duration::from_secs(2), "died too early ({took:?})");
        assert_eq!(status.signal(), Some(libc::SIGALRM), "{err}");
    }

    #[test]
    fn a_second_faulting_thread_waits_for_the_first_line() {
        let (err, status, _) = run_child("second_thread_waits", "crash::tests::child_dies");
        assert!(err.contains("first-thread-line"), "the process died before the first line: {err}");
        assert_eq!(status.signal(), Some(libc::SIGILL), "{err}");
    }

    #[test]
    fn a_second_faulting_thread_gives_up_after_the_bounded_wait() {
        let (err, status, took) = run_child("second_thread_gives_up", "crash::tests::child_dies");
        // Without the bound the child would hang until `run_child` kills it (SIGKILL).
        assert_eq!(status.signal(), Some(libc::SIGILL), "waited too long ({took:?}): {err}");
    }
}
