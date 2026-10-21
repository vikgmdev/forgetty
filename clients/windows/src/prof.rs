//! Diagnostics only (smoke runs): samples the UI thread while it paints terminal
//! text and reports which modules it sits in. Used to find where a slow frame waits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub static SAMPLE_ON: AtomicBool = AtomicBool::new(false);
static MAIN_TID: AtomicU32 = AtomicU32::new(0);
static HITS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
    fn SuspendThread(h: isize) -> u32;
    fn ResumeThread(h: isize) -> u32;
    fn GetThreadContext(h: isize, ctx: *mut u8) -> i32;
    fn GetModuleHandleExW(flags: u32, addr: *const u8, out: *mut isize) -> i32;
    fn GetModuleFileNameW(h: isize, buf: *mut u16, n: u32) -> u32;
}

#[repr(C, align(16))]
struct Ctx([u8; 1232]);

fn module_of(addr: u64, cache: &mut HashMap<isize, String>) -> Option<(String, u64)> {
    let mut h: isize = 0;
    // FROM_ADDRESS (4) | UNCHANGED_REFCOUNT (2)
    // SAFETY: out pointer is valid; the address is only used as a lookup key.
    if unsafe { GetModuleHandleExW(6, addr as *const u8, &mut h) } == 0 || h == 0 {
        return None;
    }
    let name = cache.entry(h).or_insert_with(|| {
        let mut buf = [0u16; 260];
        // SAFETY: buffer length passed matches.
        let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), 260) } as usize;
        let s = String::from_utf16_lossy(&buf[..n]);
        s.rsplit('\\').next().unwrap_or("?").to_string()
    });
    Some((name.clone(), addr.wrapping_sub(h as u64)))
}

pub fn start() {
    // SAFETY: plain query of the calling thread.
    MAIN_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    *HITS.lock().unwrap() = Some(HashMap::new());
    std::thread::Builder::new()
        .name("prof".into())
        .spawn(|| {
            // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION
            // SAFETY: opens a handle to our own UI thread.
            let h = unsafe { OpenThread(0x0002 | 0x0008 | 0x0040, 0, MAIN_TID.load(Ordering::SeqCst)) };
            if h == 0 {
                return;
            }
            let mut cache = HashMap::new();
            loop {
                std::thread::sleep(Duration::from_millis(1));
                if !SAMPLE_ON.load(Ordering::Acquire) {
                    continue;
                }
                let mut raw = [0u64; 48];
                let mut n = 0usize;
                let mut ctx = Ctx([0; 1232]);
                // CONTEXT_CONTROL | CONTEXT_INTEGER for AMD64
                ctx.0[0x30..0x34].copy_from_slice(&0x100003u32.to_le_bytes());
                // SAFETY: the thread is suspended while its context and the top of its
                // stack are read, and nothing allocates or locks in between.
                unsafe {
                    if SuspendThread(h) != u32::MAX {
                        if GetThreadContext(h, ctx.0.as_mut_ptr()) != 0 {
                            let rsp = u64::from_le_bytes(ctx.0[0x98..0xA0].try_into().unwrap());
                            let rip = u64::from_le_bytes(ctx.0[0xF8..0x100].try_into().unwrap());
                            raw[0] = rip;
                            n = 1;
                            let p = rsp as *const u64;
                            for i in 0..47 {
                                raw[n] = std::ptr::read_volatile(p.add(i));
                                n += 1;
                            }
                        }
                        ResumeThread(h);
                    }
                }
                if n == 0 {
                    continue;
                }
                let mut chain: Vec<String> = Vec::new();
                for (i, a) in raw[..n].iter().enumerate() {
                    if let Some((m, rva)) = module_of(*a, &mut cache) {
                        let item = if i == 0 { format!("{m}+{rva:x}") } else { m };
                        let base = item.split('+').next().unwrap().to_string();
                        if chain.last().map(|l| l.split('+').next().unwrap()) != Some(base.as_str()) {
                            chain.push(item);
                        }
                    }
                }
                chain.truncate(6);
                let key = chain.join(" < ");
                if let Some(m) = HITS.lock().unwrap().as_mut() {
                    *m.entry(key).or_insert(0) += 1;
                }
            }
        })
        .ok();
}

/// Top sampled stacks, most frequent first.
pub fn report() -> Vec<String> {
    let g = HITS.lock().unwrap();
    let Some(m) = g.as_ref() else { return Vec::new() };
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    v.into_iter().take(14).map(|(k, n)| format!("{n}x {k}")).collect()
}
