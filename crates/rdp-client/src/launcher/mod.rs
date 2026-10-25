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
//! Everything the launcher and its sessions log is on the page too (the
//! Activity page, a Workspace tab's log): the launcher's own lines through
//! [`crate::log_ring`], each session's by tailing its log file.
//!
//! The Workspace opens sessions inside this window, one per tab: the session
//! process creates its window as a child of this one (`RDPIO_EMBED_PARENT`),
//! says so ([`crate::window::WM_RDPIO_EMBEDDED`]), and this side puts it
//! where the page's tab is, hiding the ones whose tab isn't in front.
//!
//! Without the WebView2 runtime, [`run`] fails and `main` opens the hub.

pub(crate) mod host;
pub mod profile;

pub use host::LauncherPipe;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Controller2,
    ICoreWebView2Environment, ICoreWebView2Settings3, ICoreWebView2_3, COREWEBVIEW2_COLOR, COREWEBVIEW2_PROCESS_FAILED_KIND,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, ProcessFailedEventHandler, TrySuspendCompletedHandler, WebMessageReceivedEventHandler,
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
    name: String,
    started: Instant,
    /// Recorded in the history yet (direct sessions: the launcher does it).
    recorded: bool,
    record: Option<RecordMru>,
    /// Its log file (tailed onto the page).
    log: Option<PathBuf>,
    /// Opened in a Workspace tab (a child of this window)…
    embedded: bool,
    /// …and its window, once it said so (`WM_RDPIO_EMBEDDED`).
    child: Option<isize>,
    /// It had a window of its own once it was up (see `connect`).
    seen_window: bool,
}

/// Where the Workspace's tab in front is (this window's client pixels), and
/// whose it is.
#[derive(Default)]
struct Embed {
    active: Option<String>,
    rect: (i32, i32, i32, i32),
    /// Off while the page covers it (a dialog, another page).
    visible: bool,
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
    dark: bool,
    sessions: HashMap<u32, Session>,
    /// Hidden while a session runs ("close after connecting").
    hidden: bool,
    minimized: bool,
    /// The newest log line the page has.
    log_sent: Cell<u64>,
    embed: Embed,
}

impl State {
    fn page_shown(&self) -> bool {
        !self.hidden && !self.minimized
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Events from worker threads, waiting for the UI thread.
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// New log lines wait for the page (one `_logs` event at a time).
static LOGS_DUE: AtomicBool = AtomicBool::new(false);
/// The page is in sight: sessions' logs are read often (else now and then).
static PAGE_SHOWN: AtomicBool = AtomicBool::new(true);

/// A log line was kept (any thread): the page gets it soon.
fn notify_logs() {
    if !LOGS_DUE.swap(true, Ordering::AcqRel) {
        post_event(json!({ "event": "_logs" }));
    }
}

/// The lines the page doesn't have yet, if it's in sight (else they wait:
/// nothing wakes the launcher for a page no one sees).
fn flush_logs(st: &State) {
    if !st.page_shown() {
        return;
    }
    LOGS_DUE.store(false, Ordering::Release);
    let lines = crate::log_ring::since(st.log_sent.get(), 1000);
    if let Some(last) = lines.last() {
        st.log_sent.set(last.seq);
        send(st, &json!({ "event": "log", "lines": lines }));
    }
}

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
    crate::log_ring::enable(notify_logs);
    // (In a file too, for what happened before a restart.)
    if let Some(d) = local_dir("logs") {
        crate::log_ring::keep_file(&d.join("launcher.log"));
    }
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "launcher open");
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
            dark,
            sessions: HashMap::new(),
            hidden: false,
            minimized: false,
            log_sent: Cell::new(0),
            embed: Embed::default(),
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
        // (Clipping its children: sessions in Workspace tabs live in it.)
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("rdpio"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
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
        // A process of the page's died — a resume from hibernation can take
        // the GPU's or the renderer's with it, leaving a black window that
        // answers nothing: the page comes back (see `on_event`).
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(|_, args| {
                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                if let Some(args) = args {
                    let _ = args.ProcessFailedKind(&mut kind);
                }
                post_event(json!({ "event": "_webview_failed", "kind": kind.0 }));
                Ok(())
            })),
            &mut token,
        )?;
        controller.SetIsVisible(true)?;
        webview.NavigateToString(&HSTRING::from(page_html()))?;
    }
    Ok((controller, webview))
}

/// The page, with the brand fonts in.
fn page_html() -> String {
    // (RDPIO_LAUNCHER_HTML: a page to try instead of the built-in one.)
    let page = std::env::var_os("RDPIO_LAUNCHER_HTML").and_then(|p| std::fs::read_to_string(p).ok());
    with_brand_fonts(page.as_deref().unwrap_or(PAGE))
}

/// The page's renderer died or hangs: load the page again (its state is all
/// on this side; it asks for it, and for the log, when it starts).
fn reload_page(st: &State) {
    unsafe {
        let _ = st.webview.NavigateToString(&HSTRING::from(page_html()));
    }
}

/// WebView2's browser process is gone: a new one in the same window. (At most
/// a few times a minute: one that dies at once each time isn't retried forever.)
fn rebuild_webview() {
    static TRIES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
    {
        let mut t = TRIES.lock().unwrap();
        t.retain(|i| i.elapsed() < Duration::from_secs(60));
        if t.len() >= 3 {
            tracing::error!("the window's web view keeps failing; not restarting it again");
            return;
        }
        t.push(Instant::now());
    }
    let Some((hwnd, mica, dark)) = with_state(|st| {
        unsafe {
            let _ = st.controller.Close();
        }
        (st.hwnd, st.mica, st.dark)
    }) else {
        return;
    };
    match init_webview(hwnd, mica, dark) {
        Ok((controller, webview)) => {
            with_state(|st| {
                st.controller = controller;
                st.webview = webview;
                st.log_sent.set(0);
            });
            resize_webview(hwnd);
            with_state(|st| set_page_visible(st, st.page_shown()));
            tracing::info!("the window's web view restarted");
        }
        Err(e) => tracing::error!(error = %e, "could not restart the window's web view"),
    }
}

/// After a resume: WebView2 may still show what it drew before the machine
/// slept (or nothing): hidden and shown again, it draws afresh.
fn redraw_page(st: &State) {
    if !st.page_shown() {
        return;
    }
    unsafe {
        let _ = st.controller.SetIsVisible(false);
        let _ = st.controller.SetIsVisible(true);
        let _ = st.controller.NotifyParentWindowPositionChanged();
    }
    send(st, &json!({ "event": "resumed" }));
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

/// The page shown (`true`) or out of sight — the window hidden while a
/// session runs, or minimized. Out of sight, WebView2 is told so and
/// suspended: it stops drawing (the "Connected" dots pulse for as long as a
/// session runs) and its processes go idle, instead of animating behind the
/// session for hours. Shown again, it resumes where it was.
fn set_page_visible(st: &State, visible: bool) {
    PAGE_SHOWN.store(visible, Ordering::Relaxed);
    unsafe {
        let _ = st.controller.SetIsVisible(visible);
        if let Ok(w3) = st.webview.cast::<ICoreWebView2_3>() {
            if visible {
                let _ = w3.Resume();
            } else {
                let _ = w3.TrySuspend(&TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(()))));
            }
        }
    }
    if visible {
        flush_logs(st);
    }
}

/// The Workspace's sessions where the page says: the one whose tab is in
/// front over the tab, the rest hidden (and so paused: see
/// `Window::output_hidden`). Asynchronous: a session that's busy (or hung)
/// never holds up this window.
fn apply_embed(st: &State) {
    let e = &st.embed;
    for s in st.sessions.values() {
        let Some(child) = s.child else { continue };
        let child = HWND(child as *mut _);
        unsafe {
            if e.visible && e.active.as_deref() == Some(s.key.as_str()) && e.rect.2 > 0 && e.rect.3 > 0 {
                let (x, y, w, h) = e.rect;
                let _ = SetWindowPos(child, Some(HWND_TOP), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
            } else {
                let _ = ShowWindowAsync(child, SW_HIDE);
            }
        }
    }
}

/// Close a session: its window is asked to (the session says goodbye to the
/// host); one that doesn't go within a few seconds — hung, say, after a
/// resume — is ended, and one with no window yet (still connecting) at once.
fn close_session(pid: u32, child: Option<isize>) {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};
    let windows = match child {
        Some(c) => vec![HWND(c as *mut _)],
        None => windows_of(pid),
    };
    let grace = if windows.is_empty() { 0 } else { 6000 };
    for w in windows {
        unsafe {
            let _ = PostMessageW(Some(w), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    std::thread::spawn(move || unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) else { return };
        if WaitForSingleObject(h, grace) == WAIT_TIMEOUT {
            tracing::warn!(pid, "the session didn't close; ending it");
            let _ = TerminateProcess(h, 1);
        }
        let _ = CloseHandle(h);
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
                // Minimized: out of sight (see `set_page_visible`).
                let minimized = wparam.0 == SIZE_MINIMIZED as usize;
                with_state(|st| {
                    st.minimized = minimized;
                    if !st.hidden {
                        set_page_visible(st, !minimized);
                    }
                });
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                // Back from sleep or hibernation.
                if matches!(wparam.0, 0x7 | 0x12) {
                    post_event(json!({ "event": "_resumed" }));
                }
                LRESULT(1)
            }
            crate::window::WM_RDPIO_EMBEDDED => {
                // A session for a Workspace tab made its window (in this one).
                let (child, pid) = (wparam.0 as isize, lparam.0 as u32);
                let mut owner = 0u32;
                GetWindowThreadProcessId(HWND(child as *mut _), Some(&mut owner));
                let known = owner == pid
                    && with_state(|st| {
                        let Some(s) = st.sessions.get_mut(&pid).filter(|s| s.embedded) else { return false };
                        s.child = Some(child);
                        let key = s.key.clone();
                        apply_embed(st);
                        send(st, &json!({ "event": "embedded", "key": key }));
                        true
                    })
                    .unwrap_or(false);
                if !known && owner != 0 {
                    // (Not one of ours, or no longer in a tab: a window of its own.)
                    let _ = PostMessageW(Some(HWND(child as *mut _)), crate::window::WM_RDPIO_POPOUT, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // In front or not: the page pauses its endless animations
                // (a live session's pulsing dot) while another window has the
                // user's attention.
                let active = (wparam.0 & 0xFFFF) != 0;
                with_state(|st| send(st, &json!({ "event": "active", "on": active })));
                DefWindowProcW(hwnd, msg, wparam, lparam)
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
                // Sessions in Workspace tabs carry on, each in a window of its own.
                let children: Vec<isize> = with_state(|st| st.sessions.values().filter_map(|s| s.child).collect()).unwrap_or_default();
                for c in children {
                    let _ = SendMessageTimeoutW(HWND(c as *mut _), crate::window::WM_RDPIO_POPOUT, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 3000, None);
                }
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
                Some("session_log") => {
                    // (The running session's file, else the last one for that PC.)
                    let key = s(req, "key").unwrap_or_default();
                    let log = st.sessions.values().find(|s| s.key == key).and_then(|s| s.log.clone()).or_else(|| {
                        let host = s(req, "host")?;
                        local_dir("logs").map(|d| d.join(format!("{}.log", safe_file_name(&host))))
                    });
                    match log.filter(|p| p.exists()) {
                        Some(p) => shell_open(&p.to_string_lossy()),
                        None => return Err("No log for that session yet.".into()),
                    }
                }
                Some(url) if url.starts_with("https://") => shell_open(url),
                _ => return Err("nothing to open".into()),
            }
            Ok(Value::Null)
        }
        "logs" => {
            // (The page starting: everything kept; after that, what's new.)
            let since = req.get("since").and_then(Value::as_u64).unwrap_or(0);
            let lines = crate::log_ring::since(since, 4000);
            if let Some(last) = lines.last() {
                st.log_sent.set(st.log_sent.get().max(last.seq));
            }
            Ok(json!({ "lines": lines }))
        }
        "logs_clear" => {
            crate::log_ring::clear();
            Ok(Value::Null)
        }
        "disconnect" => {
            let key = s(req, "key").ok_or("no session")?;
            let (pid, child) = st.sessions.iter().find(|(_, s)| s.key == key).map(|(p, s)| (*p, s.child)).ok_or("That session isn't running.")?;
            tracing::info!(pid, "disconnecting a session");
            close_session(pid, child);
            Ok(Value::Null)
        }
        "embed_layout" => {
            // (CSS pixels from the page; the window's are `dpr` times as many.)
            let dpr = req.get("dpr").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.5, 8.0);
            let px = |k: &str| (req.get(k).and_then(Value::as_f64).unwrap_or(0.0) * dpr).round() as i32;
            st.embed = Embed {
                active: s(req, "key"),
                rect: (px("x"), px("y"), px("w"), px("h")),
                visible: req.get("visible").and_then(Value::as_bool).unwrap_or(true),
            };
            apply_embed(st);
            Ok(Value::Null)
        }
        "embed_popout" => {
            let key = s(req, "key").ok_or("no session")?;
            let sess = st.sessions.values_mut().find(|s| s.key == key).ok_or("That session isn't running.")?;
            sess.embedded = false;
            if let Some(c) = sess.child.take() {
                unsafe {
                    let _ = PostMessageW(Some(HWND(c as *mut _)), crate::window::WM_RDPIO_POPOUT, WPARAM(0), LPARAM(0));
                }
            }
            Ok(state_json(st))
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
    let sessions: Vec<Value> = st
        .sessions
        .iter()
        .map(|(pid, s)| {
            json!({
                "key": s.key,
                "name": s.name,
                "pid": pid,
                "embedded": s.embedded,
                "shown": s.child.is_some(),
                "seconds": s.started.elapsed().as_secs(),
            })
        })
        .collect();
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
                "folder": e.folder,
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
        "sessions": sessions,
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
    if let Some(f) = c.get("folder").and_then(Value::as_str) {
        // ("Work / Servers" → "Work/Servers".)
        entry.folder = f.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/");
    }
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
/// `embed: {w, h}` (this window's pixels) opens it in a Workspace tab of
/// that size: windowed, inside this window.
fn connect(st: &mut State, req: &Value) -> Result<Value, String> {
    let embed = req.get("embed").filter(|e| e.is_object()).map(|e| {
        let px = |k: &str| e.get(k).and_then(Value::as_f64).unwrap_or(0.0).round().clamp(200.0, 8192.0) as u32;
        (px("w"), px("h"))
    });
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
    if let Some((&pid, s)) = st.sessions.iter().find(|(_, s)| s.key == key) {
        // Already open: its tab, or its window brought forward, instead of a
        // second session.
        if s.embedded {
            return Ok(json!({ "already": true, "key": key, "embedded": true }));
        }
        // Unless the window it had is gone while the process isn't (stuck
        // closing, or after a resume): bringing it forward would show
        // nothing. It's ended, and a new one starts.
        if !s.seen_window || !windows_of(pid).is_empty() {
            bring_forward(pid);
            return Ok(json!({ "already": true, "key": key }));
        }
        tracing::warn!(pid, "that session's window is gone but its process isn't; ending it and starting a new one");
        st.sessions.remove(&pid);
        close_session(pid, None);
    }
    // In a tab: a window of the tab's size, never full screen.
    let mut options = options;
    let mut args_size = None;
    if let Some((w, h)) = embed {
        options.fullscreen = false;
        options.multimon = false;
        args_size = Some(format!("{w}x{h}"));
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
    if let Some(size) = args_size {
        args.extend(["--size".into(), size]);
    }
    // One log per PC (two sessions to one PC at once: one each).
    let log = local_dir("logs").map(|d| {
        let base = safe_file_name(&host);
        (1..)
            .map(|n| d.join(if n == 1 { format!("{base}.log") } else { format!("{base}-{n}.log") }))
            .find(|p| !st.sessions.values().any(|s| s.log.as_ref() == Some(p)))
            .unwrap()
    });
    if let Some(log) = &log {
        // The last one is kept as `.prev` here, before the session starts:
        // the page then reads only this session's lines from the file.
        if std::fs::metadata(log).is_ok_and(|m| m.len() > 0) {
            let _ = std::fs::rename(log, format!("{}.prev", log.display()));
        }
        args.extend(["--log-file".into(), log.to_string_lossy().into_owned()]);
    }
    let parent = embed.map(|_| st.hwnd.0 as isize);
    let pid = spawn_session(&args, password.as_deref(), hyper_password.as_deref(), key.clone(), name.clone(), log.clone(), parent)?;
    tracing::info!(pid, host = %host, transport = ?options.transport, in_tab = embed.is_some(), "launcher started a session");
    let record = (options.transport == Transport::Direct).then(|| RecordMru {
        host: host.clone(),
        port,
        user: user.clone(),
        domain: domain.clone(),
        name: name.clone(),
        id,
    });
    st.sessions.insert(
        pid,
        Session { key: key.clone(), name: name.clone(), started: Instant::now(), recorded: false, record, log, embedded: embed.is_some(), child: None, seen_window: false },
    );
    // (A session in a tab is in this window: it stays.)
    if !st.keep_open && embed.is_none() {
        unsafe {
            let _ = ShowWindow(st.hwnd, SW_HIDE);
        }
        st.hidden = true;
        set_page_visible(st, false);
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

/// Start a session process and threads that watch it (and read its log).
/// `parent`: open its window inside that one (a Workspace tab).
fn spawn_session(
    args: &[String],
    password: Option<&str>,
    hyper_password: Option<&str>,
    key: String,
    name: String,
    log: Option<PathBuf>,
    parent: Option<isize>,
) -> Result<u32, String> {
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
    match parent {
        Some(p) => cmd.env("RDPIO_EMBED_PARENT", p.to_string()),
        None => cmd.env_remove("RDPIO_EMBED_PARENT"),
    };
    let mut child = cmd.spawn().map_err(|e| format!("could not start the session: {e}"))?;
    let pid = child.id();
    let mut stderr = child.stderr.take();
    let ended = Arc::new(AtomicBool::new(false));
    let tail = log.and_then(|path| {
        let (src, name, ended) = (key.clone(), name.clone(), ended.clone());
        std::thread::Builder::new().name("launcher-log".into()).spawn(move || tail_log(&path, &src, &name, &ended)).ok()
    });
    std::thread::spawn(move || {
        let started = Instant::now();
        // A session still up after this connected (a direct sign-in gives up
        // after ~5 s; an RDP one has its own history record by then).
        let up_key = key.clone();
        let up = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(10));
            post_event(json!({ "event": "_up", "pid": pid, "key": up_key }));
        });
        // (What it prints there is its last word: an error. On the page too.)
        let mut err = String::new();
        if let Some(e) = stderr.take() {
            for line in std::io::BufReader::new(e).lines().map_while(Result::ok) {
                crate::log_ring::push_plain(&key, &name, "ERROR", &line);
                err.push_str(&line);
                err.push('\n');
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        ended.store(true, Ordering::Release);
        if let Some(t) = tail {
            let _ = t.join();
        }
        drop(up);
        let ran = started.elapsed();
        // The last line printed is the reason (`rdpio: …` / the error); else
        // the last error it logged.
        let mut reason = err.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
        if reason.is_empty() && code != 0 {
            reason = crate::log_ring::last_error(&key).unwrap_or_default();
        }
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

/// Read a session's log file as it grows, into the page's log. Often while
/// the page is in sight, now and then while it isn't; once more when the
/// session has ended.
fn tail_log(path: &std::path::Path, src: &str, name: &str, ended: &AtomicBool) {
    use std::io::{Seek, SeekFrom};
    crate::net_tune::eco_thread();
    let (mut pos, mut partial) = (0u64, Vec::<u8>::new());
    loop {
        let last = ended.load(Ordering::Acquire);
        if let Ok(mut f) = std::fs::File::open(path) {
            let len = f.metadata().map_or(0, |m| m.len());
            if len < pos {
                // (A new file: from its start.)
                (pos, partial) = (0, Vec::new());
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = Vec::new();
                if (&mut f).take(len - pos).read_to_end(&mut buf).is_ok() {
                    pos += buf.len() as u64;
                    partial.extend_from_slice(&buf);
                    while let Some(i) = partial.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = partial.drain(..=i).collect();
                        crate::log_ring::push(src, name, &String::from_utf8_lossy(&line));
                    }
                }
            }
        }
        if last {
            crate::log_ring::push(src, name, &String::from_utf8_lossy(&partial));
            return;
        }
        std::thread::sleep(if PAGE_SHOWN.load(Ordering::Relaxed) { Duration::from_millis(250) } else { Duration::from_secs(2) });
    }
}

/// Process `pid`'s visible top-level windows.
fn windows_of(pid: u32) -> Vec<HWND> {
    struct Find {
        pid: u32,
        found: Vec<HWND>,
    }
    extern "system" fn each(hwnd: HWND, l: LPARAM) -> windows::core::BOOL {
        let f = unsafe { &mut *(l.0 as *mut Find) };
        let mut p = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut p));
            if p == f.pid && IsWindowVisible(hwnd).as_bool() {
                f.found.push(hwnd);
            }
        }
        true.into()
    }
    let mut f = Find { pid, found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize));
    }
    f.found
}

/// The session window of process `pid` to the front.
fn bring_forward(pid: u32) {
    if let Some(&w) = windows_of(pid).first() {
        unsafe {
            if IsIconic(w).as_bool() {
                let _ = ShowWindow(w, SW_RESTORE);
            }
            let _ = SetForegroundWindow(w);
        }
    }
}

/// Events from the worker threads, on the UI thread.
fn on_event(e: Value) {
    let kind = e.get("event").and_then(Value::as_str).unwrap_or("").to_string();
    if kind == "_webview_failed" {
        // (COREWEBVIEW2_PROCESS_FAILED_KIND: 0 the browser, 1 the page's
        // renderer, 2 it hangs, 3 a frame's renderer, 6 the GPU process.)
        let k = e.get("kind").and_then(Value::as_i64).unwrap_or(-1);
        tracing::warn!(kind = k, "a process of the window's web view failed");
        match k {
            0 => rebuild_webview(),
            1 | 2 => {
                with_state(|st| reload_page(st));
            }
            // (WebView2 starts a new GPU process itself: draw with it.)
            6 => {
                with_state(|st| redraw_page(st));
            }
            _ => {}
        }
        return;
    }
    with_state(|st| match kind.as_str() {
        "_logs" => flush_logs(st),
        "_resumed" => {
            tracing::info!(sessions = st.sessions.len(), "this PC resumed from sleep; redrawing the window");
            redraw_page(st);
        }
        "_up" => {
            let pid = e.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            // (After it ended, the `_ended` already took it out: not recorded.)
            if let Some(sess) = st.sessions.get_mut(&pid) {
                // (A sign-in still going on after 10 s, a Cloud PC's say, has
                // none yet: never taken for stuck.)
                sess.seen_window |= !sess.embedded && !windows_of(pid).is_empty();
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
            let (session, reason) = (e.get("name").and_then(Value::as_str).unwrap_or(""), e.get("reason").and_then(Value::as_str).unwrap_or(""));
            let seconds = e.get("seconds").and_then(Value::as_u64).unwrap_or(0);
            tracing::info!(session, code, seconds, reason, "session ended");
            if st.hidden && st.sessions.is_empty() {
                if failed {
                    st.hidden = false;
                    set_page_visible(st, true);
                    unsafe {
                        let _ = ShowWindow(st.hwnd, SW_SHOW);
                        let _ = SetForegroundWindow(st.hwnd);
                    }
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
                if crate::updater::detached(&exe).spawn().is_ok() {
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
