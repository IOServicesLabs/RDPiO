//! What Microsoft's SlimCore Teams plugin (`--teams-slimcore`) needs from its host.
//!
//! Unlike the WebRTC add-in it draws for itself: its media engine, a process of
//! its own, puts video in child windows of the session's window. So all it asks
//! of the host is that window: its handle and sizes (a service of the Windows App's
//! that's in no SDK; slots read from the plugin and `rdclientax.dll`), and word
//! when they change.
//!
//! It also says why it's doing what it's doing only on the wire, to Teams: so the
//! handshake and its log lines are copied to the session's log here.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{implement, interface, IUnknown, IUnknown_Vtbl, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, HWND, RECT, S_OK};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

/// `GetService` GUID of the window-info service.
pub const WINDOW_INFO_SERVICE: GUID = GUID::from_u128(0xa9a17a6b_cdb3_4e81_b5c8_bd63822a8d6c);

/// What `GetWindowInfo` fills in. In desktop mode: height before width in the
/// desktop's pair, width before height in the client's.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct WindowInfo {
    hwnd: u64,
    desktop_h: u32,
    desktop_w: u32,
    client_w: u32,
    client_h: u32,
    scroll_x: i32,
    scroll_y: i32,
    scale: f32,
    _padding: u32,
}

#[interface("2A002C7B-0120-4D04-BFB1-3C73110C8581")]
unsafe trait IHostWindowInfo: IUnknown {
    unsafe fn GetWindowInfo(&self, key: u64, out: *mut WindowInfo) -> HRESULT;
    unsafe fn GetProcessId(&self, out: *mut u32) -> HRESULT;
    /// 0: a whole desktop in one window. 1: RemoteApp windows.
    unsafe fn GetMode(&self, out: *mut i32) -> HRESULT;
    unsafe fn Subscribe(&self, key: u64, callback: *mut c_void) -> HRESULT;
    unsafe fn Unsubscribe(&self, key: u64, callback: *mut c_void) -> HRESULT;
}

#[interface("E8A47FD3-1AF6-4D7F-8E15-515CCAE1C00C")]
unsafe trait IWindowChanged: IUnknown {
    unsafe fn OnWindowChanged(&self) -> HRESULT;
}

static SESSION_HWND: AtomicIsize = AtomicIsize::new(0);
static DESKTOP_W: AtomicU32 = AtomicU32::new(0);
static DESKTOP_H: AtomicU32 = AtomicU32::new(0);
/// This session hosts the SlimCore plugin (default-on). While it's set, rdpio
/// keeps its own hands off the physical camera: RDPECAM streams synthesize
/// black frames instead of opening a webcam (which the SlimCore engine
/// `MsTeamsVdi.exe` needs exclusive — an MF open on our side fails its video
/// stage, observed as `StageFailureCallback code=5` on `EngineVideoStart`).
/// The devices are still advertised so the VM sees them.
static PLUGIN_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this session hosts the SlimCore plugin.
pub fn plugin_active() -> bool {
    PLUGIN_ACTIVE.load(Ordering::SeqCst)
}

/// Record whether this session hosts the SlimCore plugin.
pub fn set_plugin_active(active: bool) {
    PLUGIN_ACTIVE.store(active, Ordering::SeqCst);
}

/// The session's window, and the size of the desktop it shows.
pub fn set_session_window(hwnd: isize, desktop_w: u32, desktop_h: u32) {
    SESSION_HWND.store(hwnd, Ordering::SeqCst);
    set_desktop_size(desktop_w, desktop_h);
}

/// The remote desktop changed size.
pub fn set_desktop_size(w: u32, h: u32) {
    DESKTOP_W.store(w, Ordering::SeqCst);
    DESKTOP_H.store(h, Ordering::SeqCst);
}

impl WindowInfo {
    pub fn hwnd(&self) -> isize {
        self.hwnd as isize
    }

    pub fn desktop_size(&self) -> (u32, u32) {
        (self.desktop_w, self.desktop_h)
    }

    pub fn client_size(&self) -> (u32, u32) {
        (self.client_w, self.client_h)
    }
}

/// The session's window as it is now.
pub fn current() -> WindowInfo {
    let hwnd = SESSION_HWND.load(Ordering::SeqCst);
    let (desktop_w, desktop_h) = (DESKTOP_W.load(Ordering::SeqCst), DESKTOP_H.load(Ordering::SeqCst));
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(HWND(hwnd as *mut c_void), &mut client) };
    let (client_w, client_h) = ((client.right - client.left).max(0) as u32, (client.bottom - client.top).max(0) as u32);
    WindowInfo {
        hwnd: hwnd as u64,
        desktop_h,
        desktop_w,
        client_w,
        client_h,
        scale: if desktop_w > 0 { client_w as f32 / desktop_w as f32 } else { 1.0 },
        ..Default::default()
    }
}

/// A subscriber; only ever called on the plugin's host thread.
struct Subscriber(IWindowChanged);

// SAFETY: kept here from whichever thread subscribed, called on the host thread.
unsafe impl Send for Subscriber {}

/// The service's state, shared with the host thread that watches the window.
#[derive(Default)]
pub struct SessionWindow {
    subscribers: Mutex<Vec<Subscriber>>,
    last: Mutex<WindowInfo>,
    /// Host-loop ticks, to look at the window about ten times a second.
    ticks: AtomicU32,
}

impl SessionWindow {
    /// `GetService`'s answer.
    pub fn service(self: &Arc<Self>) -> *mut c_void {
        let service: IHostWindowInfo = WindowInfoService { window: self.clone() }.into();
        service.into_raw()
    }

    /// Called on every host-loop tick (about 10 ms apart).
    pub fn tick(&self) {
        if self.ticks.fetch_add(1, Ordering::Relaxed) % 10 == 0 {
            self.notify_if_changed();
        }
    }

    fn notify_if_changed(&self) {
        let now = current();
        {
            let Ok(mut last) = self.last.lock() else { return };
            if *last == now {
                return;
            }
            *last = now;
        }
        tracing::info!(client = ?(now.client_w, now.client_h), desktop = ?(now.desktop_w, now.desktop_h), "Teams SlimCore: session window changed");
        let subscribers: Vec<IWindowChanged> =
            self.subscribers.lock().map(|s| s.iter().map(|s| s.0.clone()).collect()).unwrap_or_default();
        for subscriber in subscribers {
            let _ = unsafe { subscriber.OnWindowChanged() };
        }
    }
}

#[implement(IHostWindowInfo)]
struct WindowInfoService {
    window: Arc<SessionWindow>,
}

impl IHostWindowInfo_Impl for WindowInfoService_Impl {
    unsafe fn GetWindowInfo(&self, key: u64, out: *mut WindowInfo) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        if key != 0 {
            return E_INVALIDARG;
        }
        let info = current();
        tracing::info!(client = ?(info.client_w, info.client_h), desktop = ?(info.desktop_w, info.desktop_h), scale = info.scale, "Teams SlimCore: asked for the session window");
        unsafe { *out = info };
        S_OK
    }

    unsafe fn GetProcessId(&self, out: *mut u32) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        unsafe { *out = std::process::id() };
        S_OK
    }

    unsafe fn GetMode(&self, out: *mut i32) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        unsafe { *out = 0 };
        S_OK
    }

    unsafe fn Subscribe(&self, _key: u64, callback: *mut c_void) -> HRESULT {
        let Some(callback) = (unsafe { IWindowChanged::from_raw_borrowed(&callback) }) else {
            return E_POINTER;
        };
        if let Ok(mut subscribers) = self.window.subscribers.lock() {
            subscribers.push(Subscriber(callback.clone()));
        }
        S_OK
    }

    unsafe fn Unsubscribe(&self, _key: u64, callback: *mut c_void) -> HRESULT {
        if let Ok(mut subscribers) = self.window.subscribers.lock() {
            subscribers.retain(|s| s.0.as_raw() != callback);
        }
        S_OK
    }
}

/// Field names never copied to the log, whatever they hold.
const SECRET_WORDS: [&str; 6] = ["token", "secret", "auth", "cookie", "password", "credential"];
/// A longer value is cut: enough to read a version, a code or a URL's host.
const MAX_VALUE: usize = 80;
const MAX_LOG_LINE: usize = 500;
const MAX_FIELDS: usize = 64;

static LOG_LINES: AtomicU32 = AtomicU32::new(0);
static OTHER_MESSAGES: AtomicU32 = AtomicU32::new(0);

/// Copy one message on the plugin's channel to the session's log, if it's one
/// that says how the handshake is going.
pub fn log_message(outbound: bool, bytes: &[u8]) {
    if let Some(line) = summary(bytes) {
        let limit = if line.starts_with("ClientLog") { &LOG_LINES } else { &OTHER_MESSAGES };
        if limit.fetch_add(1, Ordering::Relaxed) < 1000 {
            tracing::info!(direction = if outbound { "to Teams" } else { "from Teams" }, "Teams SlimCore: {line}");
        }
    }
}

fn summary(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Some(format!("message of {} bytes, not JSON", bytes.len()));
    };
    let name = message.get("messageName").and_then(|n| n.as_str()).unwrap_or("?");
    if name == "Ping" || name == "Pong" {
        return None;
    }
    let mut fields = Vec::new();
    flatten("", &message, name == "ClientLog", &mut fields);
    fields.retain(|f| !f.starts_with("contentType=") && !f.starts_with("messageName="));
    Some(format!("{name} {}", fields.join(" ")))
}

fn flatten(path: &str, value: &serde_json::Value, is_log: bool, out: &mut Vec<String>) {
    use serde_json::Value;
    if out.len() >= MAX_FIELDS {
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                let lower = key.to_ascii_lowercase();
                if SECRET_WORDS.iter().any(|w| lower.contains(w)) {
                    continue;
                }
                let path = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                flatten(&path, field, is_log, out);
            }
        }
        Value::Array(items) => out.push(format!("{path}=[{} items]", items.len())),
        Value::String(s) => {
            let max = if is_log { MAX_LOG_LINE } else { MAX_VALUE };
            out.push(format!("{path}={}", s.chars().take(max).collect::<String>()));
        }
        Value::Null => {}
        other => out.push(format!("{path}={other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    #[test]
    fn the_window_info_lays_out_as_the_plugin_reads_it() {
        assert_eq!(std::mem::size_of::<WindowInfo>(), 0x28);
        assert_eq!(std::mem::offset_of!(WindowInfo, desktop_h), 0x08);
        assert_eq!(std::mem::offset_of!(WindowInfo, desktop_w), 0x0C);
        assert_eq!(std::mem::offset_of!(WindowInfo, client_w), 0x10);
        assert_eq!(std::mem::offset_of!(WindowInfo, client_h), 0x14);
        assert_eq!(std::mem::offset_of!(WindowInfo, scale), 0x20);
    }

    #[implement(IWindowChanged)]
    struct Counter(Arc<AtomicU32>);

    impl IWindowChanged_Impl for Counter_Impl {
        unsafe fn OnWindowChanged(&self) -> HRESULT {
            self.0.fetch_add(1, Ordering::SeqCst);
            S_OK
        }
    }

    #[test]
    fn the_service_describes_the_window_and_says_when_it_changes() {
        let hwnd = unsafe { GetDesktopWindow() }.0 as isize;
        set_session_window(hwnd, 1920, 1080);
        let window = Arc::new(SessionWindow::default());
        let raw = window.service();
        let service = unsafe { IHostWindowInfo::from_raw(raw) };
        // The plugin QIs what `GetService` returned for this IID.
        assert!(service.cast::<IHostWindowInfo>().is_ok());

        let mut info = WindowInfo::default();
        unsafe { service.GetWindowInfo(0, &mut info) }.ok().unwrap();
        assert_eq!((info.hwnd, info.desktop_w, info.desktop_h), (hwnd as u64, 1920, 1080));
        assert!(info.client_w > 0 && info.client_h > 0);
        assert_eq!(unsafe { service.GetWindowInfo(1, &mut info) }, E_INVALIDARG);
        let mut mode = -1;
        unsafe { service.GetMode(&mut mode) }.ok().unwrap();
        assert_eq!(mode, 0);

        let calls = Arc::new(AtomicU32::new(0));
        let callback: IWindowChanged = Counter(calls.clone()).into();
        unsafe { service.Subscribe(0, callback.as_raw()) }.ok().unwrap();
        window.notify_if_changed();
        window.notify_if_changed();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only a change is news");
        set_desktop_size(2560, 1440);
        window.notify_if_changed();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        unsafe { service.Unsubscribe(0, callback.as_raw()) }.ok().unwrap();
        set_desktop_size(1280, 720);
        window.notify_if_changed();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_log_shows_the_handshake_without_secrets() {
        let hello = br#"{"contentType":"scpie","messageName":"ClientHello","load_errc":16002,"caps":{"screenShareFallback":true},"accessToken":"abc","nested":{"AuthHeader":"xyz"},"version":"2026.29.1.4"}"#;
        let line = summary(hello).unwrap();
        assert!(line.starts_with("ClientHello "), "{line}");
        assert!(line.contains("load_errc=16002") && line.contains("caps.screenShareFallback=true"), "{line}");
        assert!(line.contains("version=2026.29.1.4"), "{line}");
        assert!(!line.contains("abc") && !line.contains("xyz") && !line.contains("contentType"), "{line}");
    }

    #[test]
    fn the_log_keeps_whole_log_lines_and_cuts_other_long_values() {
        let long = "x".repeat(300);
        let log = format!(r#"{{"messageName":"ClientLog","line":"{long}"}}"#);
        assert!(summary(log.as_bytes()).unwrap().contains(&long));
        let hello = format!(r#"{{"messageName":"VMHello","url":"{long}"}}"#);
        assert!(!summary(hello.as_bytes()).unwrap().contains(&long));
    }

    #[test]
    fn pings_stay_out_of_the_log_and_a_trailing_nul_is_fine() {
        assert_eq!(summary(br#"{"messageName":"Ping"}"#), None);
        assert_eq!(summary(b"{\"messageName\":\"VMGoodbye\"}\0").unwrap().trim(), "VMGoodbye");
        assert_eq!(summary(&[1, 2, 3]).unwrap(), "message of 3 bytes, not JSON");
    }
}
