//! # Launcher — rdpio's start window
//!
//! A WebView2 page (`launcher.html`, built into the exe) in a plain Win32
//! window. The page draws everything; this side owns the data and the
//! actions, and the two talk over WebView2's message channel:
//!
//! - the page sends `{id, cmd, ...}` (`window.chrome.webview.postMessage`) and
//!   gets `{id, ok, data | error}` back;
//! - this side also sends `{event, ...}` on its own (a session started,
//!   ended, failed; an administrator action finished).
//!
//! Saved connections and history are the hub's stores (`connections.json`,
//! `history.json`), so the old hub and the in-session connection bar keep
//! working on the same data; each connection's session options and Hyper
//! password live beside them in `launcher.json` ([`profile`]).
//!
//! Every session runs in its own rdpio process, started with the
//! connection's options on its command line and its passwords in its
//! environment (never on the command line, which other programs can read).
//! The launcher watches each one: a session that fails is reported with the
//! reason it printed, and with "close after connecting" the launcher hides
//! while the session runs and comes back if it fails.
//!
//! Without the WebView2 runtime, [`run`] fails and `main` opens the hub.

mod host;
pub mod profile;

pub use host::LauncherPipe;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Controller2,
    ICoreWebView2Environment, ICoreWebView2Settings3, COREWEBVIEW2_COLOR,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::{w, Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::hub::{ConnectionInput, ConnectionStore, MruStore, ProtectedPassword};
use profile::{Options, Transport};

const PAGE: &str = include_str!("launcher.html");

/// The IO Services typefaces, put into the page in place of `/*@brand-fonts*/`
/// (`NavigateToString` pages can't load files). They're licensed, not
/// published: built in only where they sit in `launcher/fonts/` (kept out of
/// git); elsewhere the page falls back to Segoe UI.
#[cfg(rdpio_brand_fonts)]
const BRAND_FONTS: &[(&str, &str, &[u8])] = &[
    ("Geomisans", "woff2", include_bytes!("fonts/Geomisans.woff2")),
    ("SanDiego", "woff", include_bytes!("fonts/SanDiego.woff")),
    ("GelatoTypewriter", "woff2", include_bytes!("fonts/GelatoTypewriter-Regular.woff2")),
];
#[cfg(not(rdpio_brand_fonts))]
const BRAND_FONTS: &[(&str, &str, &[u8])] = &[];

fn with_brand_fonts(page: &str) -> String {
    use base64::Engine as _;
    let faces: String = BRAND_FONTS
        .iter()
        .map(|(family, format, bytes)| {
            format!(
                "@font-face {{ font-family: \"{family}\"; src: url(data:font/{format};base64,{}) format(\"{format}\"); font-weight: 100 900; font-display: block; }}\n",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        })
        .collect();
    page.replacen("/*@brand-fonts*/", &faces, 1)
}
const CLASS: PCWSTR = w!("rdpioLauncher");
/// Something for the page arrived from another thread (see [`post_event`]).
const WM_LAUNCHER_EVENT: u32 = WM_APP + 0x31;
/// A session that ends within this long ran into trouble starting (bad
/// password, host unreachable): it's reported, and a hidden launcher returns.
const STARTUP: Duration = Duration::from_secs(12);

#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    #[error("WebView2: {0}")]
    WebView2(String),
    #[error("{0}")]
    Hub(#[from] crate::hub::HubError),
}

impl From<windows::core::Error> for LauncherError {
    fn from(e: windows::core::Error) -> Self {
        Self::WebView2(e.to_string())
    }
}

impl From<webview2_com::Error> for LauncherError {
    fn from(e: webview2_com::Error) -> Self {
        Self::WebView2(e.to_string())
    }
}

/// A session process the launcher started.
struct Session {
    key: String,
    started: Instant,
    /// Recorded in the history yet (direct sessions: the launcher does it).
    recorded: bool,
    record: Option<RecordMru>,
}

struct RecordMru {
    host: String,
    port: u16,
    user: Option<String>,
    domain: Option<String>,
    name: String,
    id: Option<uuid::Uuid>,
}

struct State {
    hwnd: HWND,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    connections: ConnectionStore,
    mru: MruStore,
    prefs: profile::Store,
    keep_open: bool,
    mica: bool,
    sessions: HashMap<u32, Session>,
    /// Hidden while a session runs ("close after connecting").
    hidden: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Events from worker threads, waiting for the UI thread.
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

/// Hand an event to the page from any thread.
fn post_event(event: Value) {
    EVENTS.lock().unwrap().push(event);
    let raw = HWND_RAW.load(Ordering::Acquire);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_LAUNCHER_EVENT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Open the launcher and run it until it's closed.
pub fn run() -> Result<(), LauncherError> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    hide_own_console();
    let connections = ConnectionStore::load()?;
    let mru = MruStore::load()?;
    let prefs = profile::Store::load()?;
    let keep_open = crate::hub::store::settings_path().map(|p| crate::hub::store::load_settings(&p).keep_hub_open).unwrap_or(true);
    let dark = match prefs.file.theme.as_str() {
        "dark" => true,
        "light" => false,
        _ => system_dark(),
    };
    let hwnd = create_window(dark)?;
    let mica = apply_backdrop(hwnd, dark);
    let (controller, webview) = match init_webview(hwnd, mica, dark) {
        Ok(v) => v,
        Err(e) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            drain_quit();
            return Err(e);
        }
    };
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            hwnd,
            controller,
            webview,
            connections,
            mru,
            prefs,
            keep_open,
            mica,
            sessions: HashMap::new(),
            hidden: false,
        })
    });
    HWND_RAW.store(hwnd.0 as isize, Ordering::Release);
    resize_webview(hwnd);
    // What earlier updates left behind; then what's new.
    crate::updater::cleanup();
    STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            let u = &st.prefs.file.updates;
            if u.check && std::env::var_os("RDPIO_NO_UPDATE").is_none() {
                check_for_update(&u.channel, false);
            }
        }
    });
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    HWND_RAW.store(0, Ordering::Release);
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().take() {
            unsafe {
                let _ = st.controller.Close();
            }
        }
    });
    Ok(())
}

/// Started from Explorer, a console program gets a console window of its own:
/// the launcher doesn't need it. (From a terminal it's shared: leave it.)
fn hide_own_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 4];
    let n = unsafe { GetConsoleProcessList(&mut pids) };
    if n == 1 {
        unsafe {
            let _ = FreeConsole();
        }
    }
}

/// Apps use the dark theme (Settings → Personalization → Colors).
fn system_dark() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut v = 1u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && v == 0
}

fn create_window(dark: bool) -> Result<HWND, LauncherError> {
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let instance = GetModuleHandleW(None)?;
        // The exe's icon (resource 1).
        let icon = LoadIconW(Some(instance.into()), PCWSTR(1 as _)).unwrap_or_default();
        let bg = if dark { 0x0020_2020 } else { 0x00f3_f3f3 };
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hIcon: icon,
            hIconSm: icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg)),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);
        // 1120×740 at 100 %, centred on the primary screen's work area.
        let monitor = MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = |v: i32| v * dx as i32 / 96;
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let w = scale(1120).min(work.right - work.left);
        let h = scale(740).min(work.bottom - work.top);
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 2;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("rdpio"),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        Ok(hwnd)
    }
}

/// Windows 11's Mica behind the page and a title bar in the page's theme.
/// Returns whether Mica is on (the page then leaves its background clear).
fn apply_backdrop(hwnd: HWND, dark: bool) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
    use windows::Win32::UI::Controls::MARGINS;
    set_dark_title(hwnd, dark);
    if std::env::var_os("RDPIO_MICA").is_none() {
        return false;
    }
    // DWMWA_SYSTEMBACKDROP_TYPE (38) = DWMSBT_MAINWINDOW (2): Windows 11 22H2+.
    let kind: u32 = 2;
    let ok = unsafe { DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(38), &kind as *const u32 as *const _, 4) }.is_ok();
    if ok {
        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        unsafe {
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        }
    }
    ok
}

fn set_dark_title(hwnd: HWND, dark: bool) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
    let v: u32 = dark as u32;
    // DWMWA_CAPTION_COLOR (35), COLORREF 0x00BBGGRR: the page's background
    // (Windows 11; ignored before).
    let caption: u32 = if dark { 0x000a0a0a } else { 0x00fafafa };
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const u32 as *const _, 4);
        let _ = DwmSetWindowAttribute(hwnd, windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(35), &caption as *const u32 as *const _, 4);
    }
}

/// `%LOCALAPPDATA%\rdpio\<sub>`, made if missing.
fn local_dir(sub: &str) -> Option<PathBuf> {
    let dir = crate::hub::store::data_dir().ok()?.join(sub);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn init_webview(hwnd: HWND, mica: bool, dark: bool) -> Result<(ICoreWebView2Controller, ICoreWebView2), LauncherError> {
    // The profile (cache, local storage) in our own folder: next to the exe
    // (the default) is often not writable (Program Files, a USB stick).
    let data = local_dir("webview").ok_or_else(|| LauncherError::WebView2("no LOCALAPPDATA".into()))?;
    let data = HSTRING::from(data.as_os_str());
    let (tx, rx) = mpsc::channel::<Result<(ICoreWebView2Controller, ICoreWebView2), String>>();
    let tx_env = tx.clone();
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &data,
            None,
            &CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
                let env: ICoreWebView2Environment = match (result, env) {
                    (Ok(()), Some(env)) => env,
                    (r, _) => {
                        let _ = tx_env.send(Err(format!("no WebView2 environment ({r:?})")));
                        return Ok(());
                    }
                };
                let tx = tx_env.clone();
                let made = env.CreateCoreWebView2Controller(
                    hwnd,
                    &CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
                        let _ = tx.send(match (result, controller) {
                            (Ok(()), Some(c)) => c.CoreWebView2().map(|w| (c, w)).map_err(|e| e.to_string()),
                            (r, _) => Err(format!("no WebView2 controller ({r:?})")),
                        });
                        Ok(())
                    })),
                );
                if let Err(e) = made {
                    let _ = tx_env.send(Err(e.to_string()));
                }
                Ok(())
            })),
        )
        .map_err(|e| LauncherError::WebView2(format!("{e} (is the WebView2 runtime installed?)")))?;
    }
    drop(tx);
    // The environment and controller arrive through window messages.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (controller, webview) = loop {
        if let Ok(r) = rx.try_recv() {
            break r.map_err(LauncherError::WebView2)?;
        }
        if Instant::now() > deadline {
            return Err(LauncherError::WebView2("WebView2 took too long to start".into()));
        }
        unsafe {
            MsgWaitForMultipleObjectsEx(None, 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Err(LauncherError::WebView2("closed while starting".into()));
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    };
    unsafe {
        // Clear behind the page with Mica; else the theme's colour (no white
        // flash before the page paints).
        if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
            let color = if mica {
                COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 }
            } else if dark {
                COREWEBVIEW2_COLOR { A: 255, R: 0x0a, G: 0x0a, B: 0x0a }
            } else {
                COREWEBVIEW2_COLOR { A: 255, R: 0xfa, G: 0xfa, B: 0xfa }
            };
            let _ = c2.SetDefaultBackgroundColor(color);
        }
        let settings = webview.Settings()?;
        let dev = std::env::var_os("RDPIO_DEVTOOLS").is_some();
        let _ = settings.SetAreDevToolsEnabled(dev);
        let _ = settings.SetAreDefaultContextMenusEnabled(dev);
        let _ = settings.SetIsStatusBarEnabled(false);
        let _ = settings.SetIsZoomControlEnabled(false);
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            // No reload, find, print… (copy and paste still work).
            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(dev);
        }
        let mut token = 0i64;
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut p = PWSTR::null();
                    if args.WebMessageAsJson(&mut p).is_ok() {
                        let text = webview2_com::take_pwstr(p);
                        on_message(&text);
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The page never navigates: links open in the browser.
        webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut p = PWSTR::null();
                    args.Uri(&mut p)?;
                    let uri = webview2_com::take_pwstr(p);
                    if !uri.starts_with("data:") && !uri.starts_with("about:") {
                        args.SetCancel(true)?;
                        open_external(&uri);
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut p = PWSTR::null();
                    args.Uri(&mut p)?;
                    open_external(&webview2_com::take_pwstr(p));
                    args.SetHandled(true)?;
                }
                Ok(())
            })),
            &mut token,
        )?;
        controller.SetIsVisible(true)?;
        // (RDPIO_LAUNCHER_HTML: a page to try instead of the built-in one.)
        let page = std::env::var_os("RDPIO_LAUNCHER_HTML").and_then(|p| std::fs::read_to_string(p).ok());
        webview.NavigateToString(&HSTRING::from(with_brand_fonts(page.as_deref().unwrap_or(PAGE))))?;
    }
    Ok((controller, webview))
}

/// Open a web link in the default browser (only http(s)).
fn open_external(uri: &str) {
    if uri.starts_with("https://") || uri.starts_with("http://") {
        shell_open(uri);
    }
}

fn shell_open(what: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(what), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn resize_webview(hwnd: HWND) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    with_state(|st| unsafe {
        let _ = st.controller.SetBounds(rc);
    });
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_SIZE => {
                resize_webview(hwnd);
                LRESULT(0)
            }
            WM_MOVE | WM_MOVING => {
                with_state(|st| {
                    let _ = st.controller.NotifyParentWindowPositionChanged();
                });
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_DPICHANGED => {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize.x = 720 * dpi / 96;
                info.ptMinTrackSize.y = 520 * dpi / 96;
                LRESULT(0)
            }
            WM_SETFOCUS => {
                with_state(|st| {
                    let _ = st.controller.MoveFocus(webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                });
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // The Windows theme changed: tell the page (it follows "system").
                post_event(json!({ "event": "system_theme", "dark": system_dark() }));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LAUNCHER_EVENT => {
                let events = std::mem::take(&mut *EVENTS.lock().unwrap());
                for e in events {
                    on_event(e);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn drain_quit() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
        }
    }
}

fn send(st: &State, v: &Value) {
    let text = HSTRING::from(v.to_string());
    unsafe {
        let _ = st.webview.PostWebMessageAsJson(&text);
    }
}

// ------------------------------------------------------------ the page's requests

fn on_message(text: &str) {
    let Ok(req) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    with_state(|st| {
        let reply = match handle(st, &cmd, &req) {
            Ok(data) => json!({ "id": id, "ok": true, "data": data }),
            Err(e) => json!({ "id": id, "ok": false, "error": e }),
        };
        send(st, &reply);
    });
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn handle(st: &mut State, cmd: &str, req: &Value) -> Result<Value, String> {
    match cmd {
        "state" => Ok(state_json(st)),
        "save" => save(st, req),
        "delete" => {
            let id = parse_id(req)?;
            st.connections.delete(&id).map_err(|e| e.to_string())?;
            if st.prefs.file.entries.remove(&id).is_some() {
                st.prefs.save().map_err(|e| e.to_string())?;
            }
            Ok(state_json(st))
        }
        "connect" => connect(st, req),
        "settings" => {
            if let Some(theme) = s(req, "theme").filter(|t| ["system", "dark", "light"].contains(&t.as_str())) {
                st.prefs.file.theme = theme;
            }
            if let Some(d) = req.get("defaults") {
                st.prefs.file.defaults = serde_json::from_value(d.clone()).map_err(|e| e.to_string())?;
            }
            st.prefs.save().map_err(|e| e.to_string())?;
            if let Some(k) = req.get("keep_open").and_then(Value::as_bool) {
                st.keep_open = k;
                let path = crate::hub::store::settings_path().map_err(|e| e.to_string())?;
                let mut set = crate::hub::store::load_settings(&path);
                set.keep_hub_open = k;
                crate::hub::store::save_settings(&path, &set).map_err(|e| e.to_string())?;
            }
            Ok(state_json(st))
        }
        "theme_applied" => {
            // The page resolved its theme: the title bar follows.
            set_dark_title(st.hwnd, req.get("dark").and_then(Value::as_bool).unwrap_or(false));
            Ok(Value::Null)
        }
        "host_status" => {
            // (Reads the installed exe to compare: off the UI thread.)
            std::thread::spawn(|| {
                let status = host::status();
                post_event(json!({ "event": "host_status", "status": status }));
            });
            Ok(Value::Null)
        }
        "host_action" => host_action(st, req),
        "update_check" => {
            check_for_update(&st.prefs.file.updates.channel, true);
            Ok(update_json())
        }
        "update_install" => {
            install_update(st.hwnd);
            Ok(update_json())
        }
        "update_prefs" => {
            let u = &mut st.prefs.file.updates;
            if let Some(v) = req.get("check").and_then(Value::as_bool) {
                u.check = v;
            }
            if let Some(v) = req.get("from_hosts").and_then(Value::as_bool) {
                u.from_hosts = v;
            }
            if let Some(c) = s(req, "channel").filter(|c| c == "stable" || c == "nightly") {
                let changed = c != u.channel;
                u.channel = c;
                if changed {
                    check_for_update(&u.channel.clone(), false);
                }
            }
            st.prefs.save().map_err(|e| e.to_string())?;
            Ok(state_json(st))
        }
        "open" => {
            match req.get("what").and_then(Value::as_str) {
                Some("client_logs") => {
                    if let Some(d) = local_dir("logs") {
                        shell_open(&d.to_string_lossy());
                    }
                }
                Some("host_logs") => shell_open(&crate::hyper_direct::data_dir().to_string_lossy()),
                Some(url) if url.starts_with("https://") => shell_open(url),
                _ => return Err("nothing to open".into()),
            }
            Ok(Value::Null)
        }
        "forget_recent" => {
            let host = s(req, "host").ok_or("no host")?;
            let port = req.get("port").and_then(Value::as_u64).unwrap_or(3389) as u16;
            let user = s(req, "user");
            st.mru.remove(&host, port, user.as_deref()).map_err(|e| e.to_string())?;
            Ok(state_json(st))
        }
        _ => Err(format!("unknown request {cmd:?}")),
    }
}

fn parse_id(req: &Value) -> Result<uuid::Uuid, String> {
    s(req, "id").and_then(|i| i.parse().ok()).ok_or_else(|| "no such connection".to_string())
}

fn state_json(st: &State) -> Value {
    let running: Vec<&str> = st.sessions.values().map(|s| s.key.as_str()).collect();
    let connections: Vec<Value> = st
        .connections
        .list()
        .iter()
        .map(|r| {
            let e = st.prefs.entry(&r.id);
            json!({
                "id": r.id.to_string(),
                "name": r.display_name,
                "host": r.host,
                "port": r.port,
                "user": r.username,
                "domain": r.domain,
                "has_password": r.saved_password.unprotect().is_ok_and(|p| !p.is_empty()),
                "has_hyper_password": e.hyper_password.is_some(),
                "options": e.options,
                "updated": r.updated_at,
                "running": running.contains(&r.id.to_string().as_str()),
            })
        })
        .collect();
    let recent: Vec<Value> = st
        .mru
        .list_most_recent_first()
        .iter()
        .take(12)
        .map(|r| {
            let key = profile::recent_key(&r.host, r.port, r.username.as_deref());
            let saved = r.connection_id.filter(|id| st.connections.get(id).is_some());
            let options = match saved {
                Some(id) => st.prefs.entry(&id).options,
                None => st.prefs.file.recent.get(&key).cloned().unwrap_or_else(|| st.prefs.file.defaults.clone()),
            };
            json!({
                "name": r.display_name,
                "host": r.host,
                "port": r.port,
                "user": r.username,
                "domain": r.domain,
                "when": r.last_connected_at,
                "count": r.connect_count,
                "connection_id": saved.map(|i| i.to_string()),
                "options": options,
                "running": running.contains(&key.as_str()),
            })
        })
        .collect();
    json!({
        "connections": connections,
        "recent": recent,
        "settings": {
            "theme": st.prefs.file.theme,
            "keep_open": st.keep_open,
            "defaults": st.prefs.file.defaults,
            "updates": st.prefs.file.updates,
        },
        "update": update_json(),
        "mica": st.mica,
        "system_dark": system_dark(),
        "version": env!("CARGO_PKG_VERSION"),
        "build": env!("RDPIO_BUILD"),
        "computer": std::env::var("COMPUTERNAME").unwrap_or_default(),
        "user": std::env::var("USERNAME").unwrap_or_default(),
    })
}

/// `{conn: {id?, name, host, port, user, domain, password, clear_password,
/// hyper_password, clear_hyper_password, options}}`.
fn save(st: &mut State, req: &Value) -> Result<Value, String> {
    let c = req.get("conn").ok_or("nothing to save")?;
    let host = s(c, "host").ok_or("Enter the PC's name or address.")?;
    let port = c.get("port").and_then(Value::as_u64).unwrap_or(3389);
    let port = u16::try_from(port).ok().filter(|p| *p != 0).ok_or("The port must be 1–65535.")?;
    let id = s(c, "id").and_then(|i| i.parse::<uuid::Uuid>().ok());
    let options: Options = match c.get("options") {
        Some(o) => serde_json::from_value(o.clone()).map_err(|e| e.to_string())?,
        None => st.prefs.file.defaults.clone(),
    };
    let password = c.get("password").and_then(Value::as_str).map(String::from).filter(|p| !p.is_empty());
    let clear = c.get("clear_password").and_then(Value::as_bool).unwrap_or(false);
    let record = st
        .connections
        .upsert(ConnectionInput {
            id,
            display_name: s(c, "name").unwrap_or_else(|| host.clone()),
            host,
            port,
            username: s(c, "user"),
            domain: s(c, "domain"),
            password,
        })
        .map_err(|e| e.to_string())?;
    if clear {
        // (A blank password field keeps the saved one; this forgets it.)
        let empty = ProtectedPassword::protect("").map_err(|e| e.to_string())?;
        st.connections.replace_password(&record.id, empty).map_err(|e| e.to_string())?;
    }
    let mut entry = st.prefs.entry(&record.id);
    entry.options = options;
    if c.get("clear_hyper_password").and_then(Value::as_bool).unwrap_or(false) {
        entry.hyper_password = None;
    }
    if let Some(hp) = c.get("hyper_password").and_then(Value::as_str).filter(|p| !p.is_empty()) {
        entry.hyper_password = Some(ProtectedPassword::protect(hp).map_err(|e| e.to_string())?);
    }
    st.prefs.file.entries.insert(record.id, entry);
    st.prefs.save().map_err(|e| e.to_string())?;
    Ok(json!({ "id": record.id.to_string(), "state": state_json(st) }))
}

/// `{id}` (a saved connection) or `{target: {host, port, user, domain,
/// options, name}}` (quick connect, a Recent entry), with optional
/// one-time `password` / `hyper_password` (and `remember` to save them on a
/// saved connection). Replies `{need: "password" | "hyper_password"}` when a
/// session would have to ask for one (it has no console to ask on).
fn connect(st: &mut State, req: &Value) -> Result<Value, String> {
    let given_pw = req.get("password").and_then(Value::as_str).map(String::from);
    let given_hyper = req.get("hyper_password").and_then(Value::as_str).map(String::from);
    let remember = req.get("remember").and_then(Value::as_bool).unwrap_or(false);
    let (name, host, port, user, domain, saved_pw, saved_hyper, options, id) = if let Some(id) = s(req, "id") {
        let id: uuid::Uuid = id.parse().map_err(|_| "no such connection")?;
        let r = st.connections.get(&id).cloned().ok_or("That connection no longer exists.")?;
        let e = st.prefs.entry(&id);
        let pw = r.saved_password.unprotect().ok().filter(|p| !p.is_empty());
        let hyper = e.hyper_password.as_ref().and_then(|p| p.unprotect().ok());
        (r.display_name, r.host, r.port, r.username, r.domain, pw, hyper, e.options, Some(id))
    } else {
        let t = req.get("target").ok_or("nothing to connect to")?;
        let host = s(t, "host").ok_or("Enter the PC's name or address.")?;
        let (host, port) = split_host_port(&host, t.get("port").and_then(Value::as_u64).map(|p| p as u16));
        let options: Options = match t.get("options") {
            Some(o) => serde_json::from_value(o.clone()).map_err(|e| e.to_string())?,
            None => st.prefs.file.defaults.clone(),
        };
        (s(t, "name").unwrap_or_else(|| host.clone()), host, port, s(t, "user"), s(t, "domain"), None, None, options, None)
    };
    let password = given_pw.clone().or(saved_pw);
    let hyper_password = given_hyper.clone().or(saved_hyper);
    // Ask the page for what the session would otherwise prompt for.
    if options.transport == Transport::Direct {
        if hyper_password.is_none() && !crate::hyper_client::has_saved_password(&host) {
            return Ok(json!({ "need": "hyper_password" }));
        }
    } else if password.is_none() && user.is_some() {
        return Ok(json!({ "need": "password" }));
    }
    if remember {
        if let Some(id) = id {
            if let Some(pw) = given_pw.as_deref().filter(|p| !p.is_empty()) {
                if let Some(r) = st.connections.get(&id).cloned() {
                    let _ = st.connections.upsert(ConnectionInput {
                        id: Some(id),
                        display_name: r.display_name,
                        host: r.host,
                        port: r.port,
                        username: r.username,
                        domain: r.domain,
                        password: Some(pw.to_string()),
                    });
                }
            }
            if let Some(hp) = given_hyper.as_deref() {
                let mut e = st.prefs.entry(&id);
                e.hyper_password = ProtectedPassword::protect(hp).ok();
                st.prefs.file.entries.insert(id, e);
            }
        }
    }
    let key = match id {
        Some(id) => id.to_string(),
        None => profile::recent_key(&host, port, user.as_deref()),
    };
    if id.is_none() {
        st.prefs.file.recent.insert(profile::recent_key(&host, port, user.as_deref()), options.clone());
    }
    let _ = st.prefs.save();
    if let Some((pid, _)) = st.sessions.iter().find(|(_, s)| s.key == key) {
        // Already open: bring it forward instead of a second session.
        bring_forward(*pid);
        return Ok(json!({ "already": true }));
    }
    let mut args: Vec<String> = vec!["--host".into(), host.clone(), "--port".into(), port.to_string()];
    if let Some(u) = &user {
        args.extend(["--user".into(), u.clone()]);
    }
    if let Some(d) = &domain {
        args.extend(["--domain".into(), d.clone()]);
    }
    args.extend(["--launcher-name".into(), name.clone()]);
    if let Some(id) = id {
        args.extend(["--launcher-id".into(), id.to_string()]);
    }
    args.extend(options.to_args());
    let log = local_dir("logs").map(|d| d.join(format!("{}.log", safe_file_name(&host))));
    if let Some(log) = &log {
        args.extend(["--log-file".into(), log.to_string_lossy().into_owned()]);
    }
    let pid = spawn_session(&args, password.as_deref(), hyper_password.as_deref(), key.clone(), name.clone())?;
    tracing::info!(pid, host = %host, transport = ?options.transport, "launcher started a session");
    let record = (options.transport == Transport::Direct).then(|| RecordMru {
        host: host.clone(),
        port,
        user: user.clone(),
        domain: domain.clone(),
        name: name.clone(),
        id,
    });
    st.sessions.insert(pid, Session { key: key.clone(), started: Instant::now(), recorded: false, record });
    if !st.keep_open {
        unsafe {
            let _ = ShowWindow(st.hwnd, SW_HIDE);
        }
        st.hidden = true;
    }
    send(st, &json!({ "event": "state", "state": state_json(st) }));
    Ok(json!({ "started": true, "key": key }))
}

/// "host:port" typed in one box.
fn split_host_port(text: &str, port: Option<u16>) -> (String, u16) {
    let text = text.trim();
    // (An IPv6 address has colons of its own: only "[v6]:port" carries one.)
    if let Some(rest) = text.strip_prefix('[') {
        if let Some((addr, tail)) = rest.split_once(']') {
            let p = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (addr.to_string(), p.or(port).unwrap_or(3389));
        }
    }
    if text.matches(':').count() == 1 {
        if let Some((h, p)) = text.split_once(':') {
            if let Ok(p) = p.parse::<u16>() {
                return (h.to_string(), p);
            }
        }
    }
    (text.to_string(), port.unwrap_or(3389))
}

fn safe_file_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).take(64).collect()
}

/// Start a session process and a thread that watches it.
fn spawn_session(args: &[String], password: Option<&str>, hyper_password: Option<&str>, key: String, name: String) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
    cmd.env_remove(crate::PASSWORD_ENV).env_remove(crate::HYPER_PASSWORD_ENV);
    if let Some(p) = password {
        cmd.env(crate::PASSWORD_ENV, p);
    }
    if let Some(p) = hyper_password {
        cmd.env(crate::HYPER_PASSWORD_ENV, p);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start the session: {e}"))?;
    let pid = child.id();
    let mut stderr = child.stderr.take();
    std::thread::spawn(move || {
        let started = Instant::now();
        // A session still up after this connected (a direct sign-in gives up
        // after ~5 s; an RDP one has its own history record by then).
        let up_key = key.clone();
        let up = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(10));
            post_event(json!({ "event": "_up", "pid": pid, "key": up_key }));
        });
        let mut err = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut err);
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        drop(up);
        let ran = started.elapsed();
        // The last line printed is the reason (`rdpio: …` / the error).
        let reason = err.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
        post_event(json!({
            "event": "_ended",
            "pid": pid,
            "key": key,
            "name": name,
            "code": code,
            "seconds": ran.as_secs(),
            "reason": reason,
        }));
    });
    Ok(pid)
}

/// The session window of process `pid` to the front.
fn bring_forward(pid: u32) {
    struct Find {
        pid: u32,
        found: HWND,
    }
    extern "system" fn each(hwnd: HWND, l: LPARAM) -> windows::core::BOOL {
        let f = unsafe { &mut *(l.0 as *mut Find) };
        let mut p = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut p));
            if p == f.pid && IsWindowVisible(hwnd).as_bool() {
                f.found = hwnd;
                return false.into();
            }
        }
        true.into()
    }
    let mut f = Find { pid, found: HWND::default() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize));
        if !f.found.0.is_null() {
            if IsIconic(f.found).as_bool() {
                let _ = ShowWindow(f.found, SW_RESTORE);
            }
            let _ = SetForegroundWindow(f.found);
        }
    }
}

/// Events from the worker threads, on the UI thread.
fn on_event(e: Value) {
    let kind = e.get("event").and_then(Value::as_str).unwrap_or("").to_string();
    with_state(|st| match kind.as_str() {
        "_up" => {
            let pid = e.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            // (After it ended, the `_ended` already took it out: not recorded.)
            if let Some(sess) = st.sessions.get_mut(&pid) {
                if !sess.recorded {
                    sess.recorded = true;
                    if let Some(r) = sess.record.take() {
                        let _ = st.mru.record(r.host, r.port, r.user, r.domain, r.name, r.id);
                    }
                }
            }
            // (The child records RDP sessions in the history itself.)
            if let Ok(m) = MruStore::load() {
                st.mru = m;
            }
            send(st, &json!({ "event": "state", "state": state_json(st) }));
        }
        "_ended" => {
            let pid = e.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            let sess = st.sessions.remove(&pid);
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let quick = sess.as_ref().is_some_and(|s| s.started.elapsed() < STARTUP);
            let failed = code != 0 && quick;
            if let Ok(m) = MruStore::load() {
                st.mru = m;
            }
            if st.hidden && st.sessions.is_empty() {
                if failed {
                    unsafe {
                        let _ = ShowWindow(st.hwnd, SW_SHOW);
                        let _ = SetForegroundWindow(st.hwnd);
                    }
                    st.hidden = false;
                } else {
                    // "Close after connecting": the session's done, so are we.
                    unsafe {
                        let _ = PostMessageW(Some(st.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    return;
                }
            }
            let mut out = e.clone();
            out["event"] = json!("session_ended");
            out["failed"] = json!(failed || (code != 0 && code != 1 && !quick));
            send(st, &out);
            send(st, &json!({ "event": "state", "state": state_json(st) }));
        }
        _ => send(st, &e),
    });
}

// ------------------------------------------------------------ updates

/// Where this launcher stands on updates (shown in the page).
#[derive(Debug, Clone, Default)]
enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// On GitHub.
    Available(crate::updater::Remote),
    Downloading { version: String, progress: f32 },
    /// Downloaded (here, or from a host): installs with a restart.
    Ready { exe: std::path::PathBuf, version: String },
    /// Installing: the launcher closes and the new one opens.
    Installing,
    Failed(String),
}

static UPDATE: Mutex<UpdateState> = Mutex::new(UpdateState::Idle);

fn update_json() -> Value {
    // Installed while this ran (a session took it from a host): a restart
    // brings it up.
    if let Some(v) = crate::updater::replaced_by_newer() {
        return json!({ "state": "restart", "version": v });
    }
    let u = UPDATE.lock().unwrap().clone();
    // Downloaded by a session (from a host) and not installed yet.
    if matches!(u, UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_)) {
        if let Some((exe, m)) = crate::updater::staged() {
            *UPDATE.lock().unwrap() = UpdateState::Ready { exe, version: m.version.clone() };
            return json!({ "state": "ready", "version": m.version });
        }
    }
    match u {
        UpdateState::Idle => json!({ "state": "idle" }),
        UpdateState::Checking => json!({ "state": "checking" }),
        UpdateState::UpToDate => json!({ "state": "current" }),
        UpdateState::Available(r) => json!({ "state": "available", "version": r.manifest.version, "build": r.manifest.build, "page": r.page }),
        UpdateState::Downloading { version, progress } => json!({ "state": "downloading", "version": version, "progress": progress }),
        UpdateState::Ready { version, .. } => json!({ "state": "ready", "version": version }),
        UpdateState::Installing => json!({ "state": "installing" }),
        UpdateState::Failed(why) => json!({ "state": "error", "error": why }),
    }
}

fn post_update() {
    post_event(json!({ "event": "update", "update": update_json() }));
}

/// Look on GitHub (off the UI thread). `manual`: say "checking" meanwhile.
fn check_for_update(channel: &str, manual: bool) {
    {
        let mut u = UPDATE.lock().unwrap();
        if matches!(*u, UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Installing) {
            return;
        }
        if manual {
            *u = UpdateState::Checking;
        }
    }
    if manual {
        post_update();
    }
    let channel = channel.to_string();
    std::thread::spawn(move || {
        let found = crate::updater::check_github(&channel);
        {
            let mut u = UPDATE.lock().unwrap();
            if matches!(*u, UpdateState::Ready { .. }) && !matches!(found, Ok(Some(_))) {
                return post_update();
            }
            *u = match found {
                Ok(Some(r)) => {
                    tracing::info!(version = %r.manifest.version, "a newer rdpio is out");
                    UpdateState::Available(r)
                }
                Ok(None) => UpdateState::UpToDate,
                Err(e) if manual => UpdateState::Failed(e),
                Err(e) => {
                    tracing::info!(error = %e, "couldn't look for updates");
                    UpdateState::Idle
                }
            };
        }
        post_update();
    });
}

/// Install what's available: download it if need be, then the launcher
/// closes and the new rdpio opens in its place.
fn install_update(hwnd: HWND) {
    let current = UPDATE.lock().unwrap().clone();
    let owner = hwnd.0 as isize;
    std::thread::spawn(move || {
        let finish = |exe: &std::path::Path| {
            *UPDATE.lock().unwrap() = UpdateState::Installing;
            post_update();
            match crate::updater::install(exe, Some(Vec::new())) {
                Ok(()) => unsafe {
                    let _ = PostMessageW(Some(HWND(owner as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                },
                Err(e) => {
                    *UPDATE.lock().unwrap() = UpdateState::Failed(format!("couldn't start the install: {e}"));
                    post_update();
                }
            }
        };
        if crate::updater::replaced_by_newer().is_some() {
            // Already in place: just start it.
            if let Ok(exe) = std::env::current_exe() {
                if std::process::Command::new(exe).spawn().is_ok() {
                    unsafe {
                        let _ = PostMessageW(Some(HWND(owner as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            return;
        }
        match current {
            UpdateState::Ready { exe, .. } => finish(&exe),
            UpdateState::Available(remote) => {
                let version = remote.manifest.version.clone();
                let mut last = Instant::now();
                let got = crate::updater::download(&remote, |have, total| {
                    if last.elapsed() > Duration::from_millis(200) {
                        last = Instant::now();
                        *UPDATE.lock().unwrap() =
                            UpdateState::Downloading { version: version.clone(), progress: have as f32 / total.max(1) as f32 };
                        post_update();
                    }
                });
                match got.and_then(|p| crate::updater::stage(&p).map_err(|e| e.to_string())) {
                    Ok(exe) => finish(&exe),
                    Err(e) => {
                        *UPDATE.lock().unwrap() = UpdateState::Failed(e);
                        post_update();
                    }
                }
            }
            _ => {}
        }
    });
}

/// `{action: "install" | "update" | "uninstall" | "password", password?}`.
fn host_action(st: &mut State, req: &Value) -> Result<Value, String> {
    let action = s(req, "action").ok_or("no action")?;
    let password = req.get("password").and_then(Value::as_str).map(String::from);
    let args: Vec<String> = match action.as_str() {
        "install" | "update" => vec!["--hyper-service-install".into()],
        "uninstall" => vec!["--hyper-service-uninstall".into()],
        "password" => vec!["--hyper-set-password".into()],
        "display_install" => vec!["--hyper-display-install".into()],
        "display_uninstall" => vec!["--hyper-display-uninstall".into()],
        // `{settings: {name: bool}}`: only the known names, as 0/1.
        "settings" => {
            let given = req.get("settings").and_then(Value::as_object).ok_or("no settings")?;
            let spec: Vec<String> = ["virtual_display", "match_resolution", "fast_refresh", "keep_monitors", "auto_update", "nightly"]
                .iter()
                .filter_map(|k| given.get(*k).and_then(Value::as_bool).map(|on| format!("{k}={}", on as u8)))
                .collect();
            if spec.is_empty() {
                return Err("no settings".into());
            }
            vec!["--hyper-host-settings".into(), spec.join(",")]
        }
        _ => return Err(format!("unknown action {action:?}")),
    };
    if action == "password" && password.is_none() {
        return Err("no password".into());
    }
    let owner = HWND_RAW.load(Ordering::Acquire);
    let _ = st;
    // The administrator prompt is modal: off the UI thread.
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = host::run_elevated(&args, password, HWND(owner as *mut _));
        let (ok, message) = match result {
            Ok(None) => (true, String::new()),
            Ok(Some(why)) => (false, why),
            Err(why) => (false, why),
        };
        post_event(json!({ "event": "host_action_done", "action": action, "ok": ok, "message": message }));
        post_event(json!({ "event": "host_status", "status": host::status() }));
    });
    Ok(json!({ "started": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port_split_from_one_box() {
        assert_eq!(split_host_port("pc.lan", None), ("pc.lan".into(), 3389));
        assert_eq!(split_host_port(" pc.lan:3390 ", None), ("pc.lan".into(), 3390));
        assert_eq!(split_host_port("pc.lan:x", Some(3391)), ("pc.lan:x".into(), 3391));
        assert_eq!(split_host_port("[fe80::1]:3390", None), ("fe80::1".into(), 3390));
        assert_eq!(split_host_port("fe80::1", Some(3389)), ("fe80::1".into(), 3389));
        assert_eq!(split_host_port("100.64.1.2", Some(4000)), ("100.64.1.2".into(), 4000));
    }

    #[test]
    fn log_names_stay_in_their_folder() {
        assert_eq!(safe_file_name(r"..\..\evil:host"), ".._.._evil_host");
        assert!(!safe_file_name(r"a\b/c").contains(['\\', '/']));
    }

    /// The page is self-contained: nothing loaded from the network.
    #[test]
    fn the_page_loads_nothing_from_outside() {
        for bad in ["src=\"http", "href=\"http", "@import", "url(http", "<script src"] {
            assert!(!PAGE.contains(bad), "{bad}");
        }
        assert!(PAGE.contains("window.chrome.webview") && PAGE.contains("bridge.postMessage"));
    }
}
