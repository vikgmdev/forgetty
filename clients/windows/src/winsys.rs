//! Windows system hooks: toast notifications, network-change and resume
//! callbacks. Everything here reports back through channels and counters; the
//! shell decides what to do (`machines.rs`).
//!
//! - Network: `NotifyIpInterfaceChange` (iphlpapi). The callback runs on a
//!   system thread and only sends a signal.
//! - Resume: `PowerRegisterSuspendResumeNotification` with a callback. It
//!   delivers the same `PBT_APM*` events a window gets in `WM_POWERBROADCAST`,
//!   without subclassing the GPUI window.
//! - Toasts: WinRT `ToastNotification` from a dedicated MTA thread. The
//!   app registers an AppUserModelID under `HKCU` so an unpackaged exe can
//!   show toasts. A click arrives as the toast's launch argument.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use forgetty_shell_core::notify::ToastText;

pub const AUMID: &str = "Forgetty.Windows";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// An IP interface changed (address, link up or down).
    Network,
    /// The PC woke from sleep.
    Resume,
}

#[derive(Default)]
pub struct Stats {
    pub net_registered: AtomicBool,
    pub net_callbacks: AtomicU64,
    pub power_registered: AtomicBool,
    pub resumes: AtomicU64,
    pub toasts_shown: AtomicU64,
    pub toasts_failed: AtomicU64,
    pub last_error: Mutex<String>,
}

impl Stats {
    fn error(&self, what: &str) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = what.to_string();
    }
}

pub struct Hooks {
    pub signals: async_channel::Receiver<Signal>,
    /// Launch arguments of clicked toasts.
    pub clicks: async_channel::Receiver<String>,
    pub stats: Arc<Stats>,
    toasts: std::sync::mpsc::Sender<(ToastText, String)>,
}

impl Hooks {
    /// Show a toast. `target` comes back through `clicks` when the user clicks it.
    pub fn toast(&self, text: ToastText, target: String) {
        let _ = self.toasts.send((text, target));
    }
}

static SIGNALS: OnceLock<async_channel::Sender<Signal>> = OnceLock::new();
static STATS: OnceLock<Arc<Stats>> = OnceLock::new();

pub fn start() -> Hooks {
    let stats = Arc::new(Stats::default());
    let _ = STATS.set(stats.clone());
    let (sig_tx, sig_rx) = async_channel::unbounded();
    let _ = SIGNALS.set(sig_tx);
    let (click_tx, click_rx) = async_channel::unbounded();
    let (toast_tx, toast_rx) = std::sync::mpsc::channel();
    imp::register_hooks(&stats);
    let st = stats.clone();
    std::thread::Builder::new()
        .name("forgetty-toast".into())
        .spawn(move || imp::toast_thread(toast_rx, click_tx, st))
        .ok();
    Hooks { signals: sig_rx, clicks: click_rx, stats, toasts: toast_tx }
}

fn signal(s: Signal) {
    if let Some(tx) = SIGNALS.get() {
        let _ = tx.try_send(s);
    }
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// What the Action Center holds for this app: `(launch argument, title)` of each toast.
/// The smoke uses it to see that a toast really arrived, not only that one was requested.
pub fn action_center() -> Result<Vec<(String, String)>, String> {
    imp::history()
}

/// Toast XML. Text was sanitized by `notify`; it is escaped again here.
pub fn toast_xml(text: &ToastText, target: &str) -> String {
    let mut x = format!(
        "<toast launch=\"{}\" activationType=\"foreground\"><visual><binding template=\"ToastGeneric\"><text>{}</text>",
        xml_escape(target),
        xml_escape(&text.title)
    );
    for l in text.lines.iter().take(2) {
        x.push_str(&format!("<text>{}</text>", xml_escape(l)));
    }
    x.push_str("</binding></visual></toast>");
    x
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;

    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
    use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        MibInitialNotification, NotifyIpInterfaceChange, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;
    use windows::Win32::System::Power::{
        PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    };
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    };

    unsafe extern "system" fn on_ip_change(_ctx: *const c_void, _row: *const MIB_IPINTERFACE_ROW, kind: MIB_NOTIFICATION_TYPE) {
        if let Some(s) = STATS.get() {
            s.net_callbacks.fetch_add(1, Ordering::Relaxed);
        }
        // The initial notification only proves the callback works.
        if kind != MibInitialNotification {
            signal(Signal::Network);
        }
    }

    unsafe extern "system" fn on_power(_ctx: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
        if kind == PBT_APMRESUMEAUTOMATIC || kind == PBT_APMRESUMESUSPEND {
            if let Some(s) = STATS.get() {
                s.resumes.fetch_add(1, Ordering::Relaxed);
            }
            signal(Signal::Resume);
        }
        0
    }

    pub fn register_hooks(stats: &Arc<Stats>) {
        unsafe {
            let mut handle = HANDLE::default();
            let rc = NotifyIpInterfaceChange(AF_UNSPEC, Some(on_ip_change), None, true, &mut handle);
            if rc == ERROR_SUCCESS {
                stats.net_registered.store(true, Ordering::SeqCst);
            } else {
                stats.error(&format!("NotifyIpInterfaceChange: {}", rc.0));
            }

            // The registration keeps pointers to this struct only while the call runs.
            let mut params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power),
                Context: std::ptr::null_mut(),
            };
            let mut reg: *mut c_void = std::ptr::null_mut();
            let rc = PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(&mut params as *mut _ as *mut c_void),
                &mut reg,
            );
            if rc == ERROR_SUCCESS {
                stats.power_registered.store(true, Ordering::SeqCst);
            } else {
                stats.error(&format!("PowerRegisterSuspendResumeNotification: {}", rc.0));
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Without a registered AppUserModelID Windows drops the toasts of an
    /// unpackaged exe. A DisplayName under HKCU is enough.
    fn register_aumid() -> windows::core::Result<()> {
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide(AUMID).as_ptr()))?;
            let key = wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
            let name = wide("DisplayName");
            let value = wide("Forgetty");
            let rc = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(value.as_ptr() as *const c_void),
                (value.len() * 2) as u32,
            );
            if rc != ERROR_SUCCESS {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(rc.0)));
            }
        }
        Ok(())
    }

    fn show(
        text: &ToastText,
        target: &str,
        clicks: &async_channel::Sender<String>,
    ) -> windows::core::Result<ToastNotification> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(toast_xml(text, target)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        let tx = clicks.clone();
        toast.Activated(&TypedEventHandler::new(move |_sender, args: windows::core::Ref<'_, windows::core::IInspectable>| {
            if let Some(a) = args.as_ref() {
                if let Ok(a) = a.cast::<ToastActivatedEventArgs>() {
                    if let Ok(s) = a.Arguments() {
                        let _ = tx.try_send(s.to_string());
                    }
                }
            }
            Ok(())
        }))?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&toast)?;
        Ok(toast)
    }

    pub fn history() -> Result<Vec<(String, String)>, String> {
        let run = || -> windows::core::Result<Vec<(String, String)>> {
            let list = ToastNotificationManager::History()?.GetHistoryWithId(&HSTRING::from(AUMID))?;
            let mut out = Vec::new();
            for t in list {
                let doc = t.Content()?;
                let launch = doc.DocumentElement()?.GetAttribute(&HSTRING::from("launch"))?.to_string();
                let texts = doc.GetElementsByTagName(&HSTRING::from("text"))?;
                let title = match texts.Item(0) {
                    Ok(n) => n.InnerText()?.to_string(),
                    Err(_) => String::new(),
                };
                out.push((launch, title));
            }
            Ok(out)
        };
        run().map_err(|e| e.to_string())
    }

    pub fn toast_thread(
        rx: std::sync::mpsc::Receiver<(ToastText, String)>,
        clicks: async_channel::Sender<String>,
        stats: Arc<Stats>,
    ) {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        if let Err(e) = register_aumid() {
            stats.error(&format!("AppUserModelID: {e}"));
        }
        // A shown toast must stay alive for its Activated handler to fire.
        let mut alive: std::collections::VecDeque<ToastNotification> = Default::default();
        while let Ok((text, target)) = rx.recv() {
            match show(&text, &target, &clicks) {
                Ok(t) => {
                    stats.toasts_shown.fetch_add(1, Ordering::SeqCst);
                    alive.push_back(t);
                    if alive.len() > 32 {
                        alive.pop_front();
                    }
                }
                Err(e) => {
                    stats.toasts_failed.fetch_add(1, Ordering::SeqCst);
                    stats.error(&format!("toast: {e}"));
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn history() -> Result<Vec<(String, String)>, String> {
        Err("not Windows".into())
    }

    pub fn register_hooks(_stats: &Arc<Stats>) {}

    pub fn toast_thread(
        rx: std::sync::mpsc::Receiver<(ToastText, String)>,
        _clicks: async_channel::Sender<String>,
        stats: Arc<Stats>,
    ) {
        while rx.recv().is_ok() {
            stats.toasts_failed.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_escaped() {
        let t = ToastText { title: "a<b>&\"'".into(), lines: vec!["x</text><text>y".into(), "2".into(), "3".into()] };
        let x = toast_xml(&t, "forgetty:focus?m=1&p=2");
        assert!(x.contains("launch=\"forgetty:focus?m=1&amp;p=2\""));
        assert!(x.contains("<text>a&lt;b&gt;&amp;&quot;&apos;</text>"));
        assert!(x.contains("x&lt;/text&gt;&lt;text&gt;y"));
        assert_eq!(x.matches("<text>").count(), 3, "title and two lines only");
    }
}
