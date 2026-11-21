//! Floating auto-hide connection bar (mstsc-style) for full-screen sessions,
//! Remote Desktop and Hyper alike, whose borderless windows have no frame and
//! thus no close button. One bar per process.
//!
//! A small topmost tool window at the top-centre of the session's screen with
//! these buttons: **Pin** (toggle auto-hide), **Minimize** (the session goes to
//! the taskbar), **Window** (a one-window session leaves full screen), **Stats**
//! (a Hyper stream's stats, as Ctrl+Shift+S), **Save** (opens the "Save this
//! connection" dialog, prefilled with the active session's details, when it
//! has them) and **Disconnect** (ends the session). It shows for the session's
//! first seconds so it's found, then only while the cursor is at the top-centre
//! edge of the screen; never while the mouse is locked in, another program is
//! in front or the session is a normal window. That is all driven by [`tick`],
//! which every pump of a session window calls (the bar gets no mouse messages
//! while hidden, so we can't rely on `WM_MOUSELEAVE`): it stays off the
//! picture otherwise, so a full-screen stream keeps its direct path to the
//! screen. `WS_EX_NOACTIVATE` keeps the session window focused when its
//! buttons are clicked, so input + the keyboard hook keep working.
//!
//! Disconnect just posts `WM_QUIT` to the shared UI thread, which the window
//! pump already turns into a clean shutdown (same as Ctrl+Shift+Q).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::hub::{ConnectionInput, ConnectionStore};

/// Whether the bar is pinned (always visible). When false it auto-hides.
static PINNED: AtomicBool = AtomicBool::new(false);
/// Whether the bar window is currently shown (so `tick` only toggles on change).
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// The active session's connection facts (set once at connect time), used to
/// prefill the "Save this connection" dialog.
static SESSION: Mutex<Option<SessionInfo>> = Mutex::new(None);
/// Whether Save is offered (read without [`SESSION`]'s lock): the session's
/// details are known and it isn't a saved connection (yet).
static HAS_INFO: AtomicBool = AtomicBool::new(false);
/// The bar window (0: none yet).
static BAR: AtomicIsize = AtomicIsize::new(0);
/// The bar's buttons, in [`BUTTONS`] order.
static BUTTON_HWNDS: [AtomicIsize; 6] = [const { AtomicIsize::new(0) }; 6];
/// The session window the bar belongs to (with several, the first).
static MAIN: AtomicIsize = AtomicIsize::new(0);
/// Every session window (for Minimize). Locked only on a click.
static SESSION_WINDOWS: Mutex<Vec<isize>> = Mutex::new(Vec::new());
/// The session is one window (it can leave full screen, and the bar follows
/// it to whichever screen it's on).
static SINGLE: AtomicBool = AtomicBool::new(false);
/// The screen the bar sits on: left, top, width.
static MON_LEFT: AtomicI32 = AtomicI32::new(0);
static MON_TOP: AtomicI32 = AtomicI32::new(0);
static MON_W: AtomicI32 = AtomicI32::new(0);
/// The bar's left edge and width as laid out.
static BAR_LEFT: AtomicI32 = AtomicI32::new(0);
static BAR_WIDTH: AtomicI32 = AtomicI32::new(0);
/// The buttons shown (bits in [`BUTTONS`] order; `u32::MAX`: not laid out).
static LAYOUT: AtomicU32 = AtomicU32::new(u32::MAX);
/// `window::display_epoch()` when the bar was last placed (`u32::MAX`: never).
static PLACED_EPOCH: AtomicU32 = AtomicU32::new(u32::MAX);
/// Until when (ms, see [`now_ms`]) the bar shows on its own, and the last tick.
static SHOW_UNTIL: AtomicU64 = AtomicU64::new(0);
static LAST_TICK: AtomicU64 = AtomicU64::new(0);
/// A session window that goes full screen gets a bar (see [`want`]).
static WANTED: AtomicBool = AtomicBool::new(false);
/// Minimize put the session windows in the taskbar (so restoring one
/// restores all of them).
static MINIMIZED: AtomicBool = AtomicBool::new(false);

const ID_PIN: usize = 1;
const ID_DISCONNECT: usize = 2;
const ID_SAVE: usize = 3;
const ID_MINIMIZE: usize = 4;
const ID_WINDOW: usize = 5;
const ID_STATS: usize = 6;
/// The buttons, left to right: id, label, width.
const BUTTONS: [(usize, PCWSTR, i32); 6] = [
    (ID_PIN, w!("Pin"), 56),
    (ID_MINIMIZE, w!("Minimize"), 84),
    (ID_WINDOW, w!("Window"), 72),
    (ID_STATS, w!("Stats"), 60),
    (ID_SAVE, w!("Save"), 56),
    (ID_DISCONNECT, w!("Disconnect"), 92),
];
const BAR_H: i32 = 34;
/// Space around and between the buttons.
const GAP: i32 = 4;
/// How long the bar shows on its own when it's made.
const SHOW_FIRST_MS: u64 = 3_000;
/// How often [`tick`] looks at the pointer (it's called on every pump).
const TICK_MS: u64 = 30;

/// Geometry of the "Save this connection" dialog.
const SAVE_DLG_W: i32 = 440;
const SAVE_DLG_H: i32 = 316;
const DLG_LABEL_X: i32 = 20;
const DLG_EDIT_X: i32 = 130;
const DLG_EDIT_W: i32 = SAVE_DLG_W - DLG_EDIT_X - 24;
const DLG_ROW_H: i32 = 34;
const DLG_TOP: i32 = 16;

/// Save-dialog control ids.
const DLG_NAME: usize = 10;
const DLG_HOST: usize = 11;
const DLG_PORT: usize = 12;
const DLG_USER: usize = 13;
const DLG_DOMAIN: usize = 14;
const DLG_PASSWORD: usize = 15;
const DLG_SAVE: usize = 16;
const DLG_CANCEL: usize = 17;

/// The active session's connection facts, captured at connect time so the bar's
/// "Save this connection" dialog can prefill its fields. The password is the
/// in-memory plaintext when the session still has it; `None` leaves the field
/// blank (the saved record then has no usable password).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Option<String>,
    /// The saved connection this session is (the launcher's `--launcher-id`,
    /// or what the bar's Save made): there is no Save button then, and a save
    /// updates that record rather than adding another.
    pub connection_id: Option<uuid::Uuid>,
}

/// Attach session metadata to the bar so "Save this connection" can prefill the
/// dialog. Called at every (re)connect, before the bar is first shown; a
/// connection the bar saved earlier in this process stays saved. Best-effort:
/// a poisoned lock just means the dialog opens blank.
pub fn set_session_info(mut info: SessionInfo) {
    tracing::debug!(host = %info.host, "connection bar session info set");
    if let Ok(mut s) = SESSION.lock() {
        if info.connection_id.is_none() {
            info.connection_id = s.as_ref().filter(|was| was.host == info.host && was.port == info.port).and_then(|was| was.connection_id);
        }
        HAS_INFO.store(info.connection_id.is_none(), Ordering::Relaxed);
        *s = Some(info);
    }
}

/// Milliseconds since this process first asked.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Give this process's session window a bar once it's full screen (now or
/// after Ctrl+Shift+F): for sessions whose window is made elsewhere (Hyper).
pub fn want() {
    WANTED.store(true, Ordering::Relaxed);
}

/// Where each button goes for the buttons `shown` (bits in [`BUTTONS`]
/// order): `(x, width)` or `None`, and the bar's width.
fn layout(shown: u32) -> ([Option<(i32, i32)>; 6], i32) {
    let mut at = [None; 6];
    let mut x = GAP;
    for (i, (_, _, w)) in BUTTONS.iter().enumerate() {
        if shown & (1 << i) != 0 {
            at[i] = Some((x, *w));
            x += w + GAP;
        }
    }
    (at, x)
}

/// The buttons this session has now: Window for one window, Stats in a Hyper
/// stream (also after Remote Desktop handed over to one), Save when the
/// session's details are known and it isn't a saved connection.
fn buttons_now() -> u32 {
    let has = [true, true, SINGLE.load(Ordering::Relaxed), crate::window::in_hyper(), HAS_INFO.load(Ordering::Relaxed), true];
    has.iter().enumerate().fold(0, |bits, (i, on)| if *on { bits | 1 << i } else { bits })
}

/// The bar of a Remote Desktop session, which makes its windows itself.
/// Dropping it removes the bar.
pub struct ConnBar {
    hwnd: HWND,
}

impl ConnBar {
    /// A bar at the top centre of the screen `(left, top, width)` for the
    /// session windows `sessions` (the first owns the bar); `single`: they are
    /// one window. It shows for its first seconds, then as [`tick`] says.
    pub fn new(screen: (i32, i32, i32), sessions: &[isize], single: bool) -> windows::core::Result<Self> {
        let hwnd = unsafe { create(screen, sessions, single)? };
        Ok(Self { hwnd })
    }

    /// See [`tick`].
    pub fn tick(&self) {
        tick(0);
    }
}

impl Drop for ConnBar {
    fn drop(&mut self) {
        BAR.store(0, Ordering::Relaxed);
        VISIBLE.store(false, Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Make the bar (hidden; the first [`tick`] shows it).
unsafe fn create(screen: (i32, i32, i32), sessions: &[isize], single: bool) -> windows::core::Result<HWND> {
    let module = GetModuleHandleW(None)?;
    let hinstance = HINSTANCE(module.0);
    let class_name = w!("rdpioConnBarClass");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(connbar_proc),
        hInstance: hinstance,
        lpszClassName: class_name,
        hbrBackground: CreateSolidBrush(COLORREF(0x002B2B2B)),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    let _ = RegisterClassExW(&wc);

    let main = sessions.first().copied().unwrap_or(0);
    // Owned by the session window: it goes with it to the taskbar, and with
    // it in and out of topmost.
    let owner = (main != 0).then(|| HWND(main as *mut core::ffi::c_void));
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name,
        w!("RDPiO"),
        WS_POPUP,
        screen.0,
        screen.1,
        BAR_H * 4,
        BAR_H,
        owner,
        None,
        Some(hinstance),
        None,
    )?;
    // The buttons, laid out (and shown) by `apply_layout`; clicks arrive as
    // WM_COMMAND with their ids.
    for (i, (id, label, w)) in BUTTONS.iter().enumerate() {
        let b = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            *label,
            WS_CHILD,
            0,
            3,
            *w,
            BAR_H - 6,
            Some(hwnd),
            Some(HMENU(*id as *mut core::ffi::c_void)),
            Some(hinstance),
            None,
        )?;
        BUTTON_HWNDS[i].store(b.0 as isize, Ordering::Relaxed);
    }
    if let Ok(mut list) = SESSION_WINDOWS.lock() {
        *list = sessions.to_vec();
    }
    MAIN.store(main, Ordering::Relaxed);
    SINGLE.store(single, Ordering::Relaxed);
    MON_LEFT.store(screen.0, Ordering::Relaxed);
    MON_TOP.store(screen.1, Ordering::Relaxed);
    MON_W.store(screen.2, Ordering::Relaxed);
    LAYOUT.store(u32::MAX, Ordering::Relaxed);
    PLACED_EPOCH.store(u32::MAX, Ordering::Relaxed);
    VISIBLE.store(false, Ordering::Relaxed);
    MINIMIZED.store(false, Ordering::Relaxed);
    SHOW_UNTIL.store(now_ms() + SHOW_FIRST_MS, Ordering::Relaxed);
    LAST_TICK.store(0, Ordering::Relaxed);
    BAR.store(hwnd.0 as isize, Ordering::Relaxed);
    apply_layout(hwnd, buttons_now());
    tick(0);
    Ok(hwnd)
}

/// Show or hide the bar for where the pointer is and what the session is
/// doing. Called by every pump of a session window (`from`: that window; a
/// full-screen one gets the bar [`want`] asked for) and by [`ConnBar::tick`];
/// it looks at most every [`TICK_MS`], and costs two atomic loads when there's
/// no bar to show.
pub fn tick(from: isize) {
    let bar = BAR.load(Ordering::Relaxed);
    if bar == 0 && !(from != 0 && WANTED.load(Ordering::Relaxed)) {
        return;
    }
    let now = now_ms();
    if now.saturating_sub(LAST_TICK.load(Ordering::Relaxed)) < TICK_MS {
        return;
    }
    LAST_TICK.store(now, Ordering::Relaxed);
    unsafe {
        if bar == 0 {
            // (A window in a Workspace tab is a child, never a popup.)
            let session = HWND(from as *mut core::ffi::c_void);
            if (GetWindowLongPtrW(session, GWL_STYLE) as u32) & WS_POPUP.0 != 0 {
                WANTED.store(false, Ordering::Relaxed);
                let screen = crate::window::monitor_rect_of(session).map_or((0, 0, 0), |r| (r.left, r.top, r.right - r.left));
                match create(screen, &[from], true) {
                    Ok(_) => tracing::info!("connection bar: point at the top edge of the screen"),
                    Err(e) => tracing::warn!(error = %e, "no connection bar"),
                }
            }
            return;
        }
        update(HWND(bar as *mut core::ffi::c_void), now);
    }
}

unsafe fn update(bar: HWND, now: u64) {
    let main = HWND(MAIN.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    if !IsWindow(Some(main)).as_bool() {
        return;
    }
    let full = (GetWindowLongPtrW(main, GWL_STYLE) as u32) & WS_POPUP.0 != 0;
    let iconic = IsIconic(main).as_bool();
    // Restored from the taskbar after Minimize: the other windows too.
    if !iconic && MINIMIZED.swap(false, Ordering::Relaxed) {
        let others: Vec<isize> = SESSION_WINDOWS.lock().map(|l| l.clone()).unwrap_or_default();
        for h in others.into_iter().filter(|&h| h != main.0 as isize) {
            let h = HWND(h as *mut core::ffi::c_void);
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
        }
    }
    // One window follows its screen (Ctrl+Shift+F may have made it full
    // screen on another one).
    let epoch = crate::window::display_epoch();
    if SINGLE.load(Ordering::Relaxed) && full && PLACED_EPOCH.swap(epoch, Ordering::Relaxed) != epoch {
        if let Some(r) = crate::window::monitor_rect_of(main) {
            MON_LEFT.store(r.left, Ordering::Relaxed);
            MON_TOP.store(r.top, Ordering::Relaxed);
            MON_W.store(r.right - r.left, Ordering::Relaxed);
            LAYOUT.store(u32::MAX, Ordering::Relaxed);
        }
    }
    let shown = buttons_now();
    if shown != LAYOUT.load(Ordering::Relaxed) {
        apply_layout(bar, shown);
    }
    let reveal = full
        && !iconic
        && !crate::window::capture_mode()
        && ours_in_front()
        && (PINNED.load(Ordering::Relaxed) || now < SHOW_UNTIL.load(Ordering::Relaxed) || pointer_at_bar());
    let visible = VISIBLE.load(Ordering::Relaxed);
    if reveal && !visible {
        let _ = SetWindowPos(
            bar,
            Some(HWND_TOPMOST),
            BAR_LEFT.load(Ordering::Relaxed),
            MON_TOP.load(Ordering::Relaxed),
            BAR_WIDTH.load(Ordering::Relaxed),
            BAR_H,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        VISIBLE.store(true, Ordering::Relaxed);
    } else if !reveal && visible {
        let _ = ShowWindow(bar, SW_HIDE);
        VISIBLE.store(false, Ordering::Relaxed);
    }
}

/// The pointer is in the strip at the top edge above the bar.
unsafe fn pointer_at_bar() -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return false;
    }
    let (left, width) = (BAR_LEFT.load(Ordering::Relaxed), BAR_WIDTH.load(Ordering::Relaxed));
    pt.y <= MON_TOP.load(Ordering::Relaxed) + BAR_H && pt.x >= left - 48 && pt.x <= left + width + 48
}

/// One of this process's windows is in front (the session, the bar's dialog).
unsafe fn ours_in_front() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(fg, Some(&mut pid as *mut u32));
    pid == std::process::id()
}

/// Show the buttons `shown`, side by side, and centre the bar on its screen.
unsafe fn apply_layout(bar: HWND, shown: u32) {
    let (at, width) = layout(shown);
    for (i, place) in at.iter().enumerate() {
        let b = HWND(BUTTON_HWNDS[i].load(Ordering::Relaxed) as *mut core::ffi::c_void);
        match place {
            Some((x, w)) => {
                let _ = SetWindowPos(b, None, *x, 3, *w, BAR_H - 6, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            None => {
                let _ = ShowWindow(b, SW_HIDE);
            }
        }
    }
    let left = MON_LEFT.load(Ordering::Relaxed) + (MON_W.load(Ordering::Relaxed) - width) / 2;
    BAR_LEFT.store(left, Ordering::Relaxed);
    BAR_WIDTH.store(width, Ordering::Relaxed);
    LAYOUT.store(shown, Ordering::Relaxed);
    let _ = SetWindowPos(bar, None, left, MON_TOP.load(Ordering::Relaxed), width, BAR_H, SWP_NOZORDER | SWP_NOACTIVATE);
}

/// Connection-bar window procedure: the buttons; everything else is default.
unsafe extern "system" fn connbar_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COMMAND {
        match wparam.0 & 0xffff {
            ID_PIN => {
                let pinned = !PINNED.load(Ordering::SeqCst);
                PINNED.store(pinned, Ordering::SeqCst);
                let btn = HWND(BUTTON_HWNDS[0].load(Ordering::Relaxed) as *mut core::ffi::c_void);
                let _ = SetWindowTextW(btn, if pinned { w!("Unpin") } else { w!("Pin") });
                return LRESULT(0);
            }
            ID_MINIMIZE => {
                // (The windows are copied out: minimizing sends them messages.)
                let windows: Vec<isize> = SESSION_WINDOWS.lock().map(|l| l.clone()).unwrap_or_default();
                let _ = ShowWindow(hwnd, SW_HIDE);
                VISIBLE.store(false, Ordering::Relaxed);
                for h in windows {
                    let _ = ShowWindow(HWND(h as *mut core::ffi::c_void), SW_MINIMIZE);
                }
                MINIMIZED.store(true, Ordering::Relaxed);
                return LRESULT(0);
            }
            ID_WINDOW => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                VISIBLE.store(false, Ordering::Relaxed);
                crate::window::toggle_fullscreen_of(MAIN.load(Ordering::Relaxed));
                return LRESULT(0);
            }
            ID_STATS => {
                crate::window::toggle_stats();
                return LRESULT(0);
            }
            ID_DISCONNECT => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            ID_SAVE => {
                // "Save this connection": open the prefilled dialog. Errors are
                // logged inside; the session keeps running either way.
                open_save_dialog(hwnd);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Open the "Save this connection" dialog owned by the bar window, prefilled
/// from the session metadata (blank fields when none was set).
fn open_save_dialog(owner: HWND) {
    unsafe {
        let module = match GetModuleHandleW(None) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "no module handle for the save dialog");
                return;
            }
        };
        let hinstance = HINSTANCE(module.0);
        let class_name = w!("rdpioSaveDialogClass");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(save_dialog_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            hbrBackground: CreateSolidBrush(COLORREF(0x002B2B2B)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let dlg = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("Save this connection"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            SAVE_DLG_W,
            SAVE_DLG_H,
            Some(owner),
            None,
            Some(hinstance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "could not create the save dialog");
                return;
            }
        };

        create_dialog_controls(dlg, hinstance);

        let _ = ShowWindow(dlg, SW_SHOWNOACTIVATE);
    }
}

/// Create the dialog's labelled edit controls (prefilled from the session) and
/// its Save/Cancel buttons.
unsafe fn create_dialog_controls(dlg: HWND, inst: HINSTANCE) {
    let session = SESSION.lock().ok().and_then(|s| s.clone());
    let (name, host, port, user, domain, password) = match &session {
        Some(s) => (
            s.display_name.clone(),
            s.host.clone(),
            s.port.to_string(),
            s.username.clone().unwrap_or_default(),
            s.domain.clone().unwrap_or_default(),
            s.password.clone().unwrap_or_default(),
        ),
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ),
    };

    let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
    let edit_style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE | WS_BORDER).0 | ES_AUTOHSCROLL as u32);
    let pw_style = WINDOW_STYLE(edit_style.0 | ES_PASSWORD as u32);
    let btn_style = WS_CHILD | WS_VISIBLE;

    let mut y = DLG_TOP;
    for (label, text, style, id) in [
        ("Display name", name.as_str(), edit_style, DLG_NAME),
        ("Host", host.as_str(), edit_style, DLG_HOST),
        ("Port", port.as_str(), edit_style, DLG_PORT),
        ("Username", user.as_str(), edit_style, DLG_USER),
        ("Domain", domain.as_str(), edit_style, DLG_DOMAIN),
        ("Password", password.as_str(), pw_style, DLG_PASSWORD),
    ] {
        let _ = dlg_child(dlg, inst, "STATIC", label, label_style, DLG_LABEL_X, y, 100, 22, 0);
        let edit = dlg_child(dlg, inst, "EDIT", "", style, DLG_EDIT_X, y, DLG_EDIT_W, 24, id);
        if let Some(e) = edit {
            set_text(e, text);
        }
        y += DLG_ROW_H;
    }
    let _ = dlg_child(
        dlg,
        inst,
        "BUTTON",
        "Save",
        btn_style,
        DLG_EDIT_X,
        y + 8,
        100,
        28,
        DLG_SAVE,
    );
    let _ = dlg_child(
        dlg,
        inst,
        "BUTTON",
        "Cancel",
        btn_style,
        DLG_EDIT_X + 112,
        y + 8,
        100,
        28,
        DLG_CANCEL,
    );
}

/// Create one child control of the save dialog.
unsafe fn dlg_child(
    dlg: HWND,
    inst: HINSTANCE,
    class: &'static str,
    text: &'static str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
) -> Option<HWND> {
    let class_w: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
    let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class_w.as_ptr()),
        PCWSTR(text_w.as_ptr()),
        style,
        x,
        y,
        w,
        h,
        Some(dlg),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(inst),
        None,
    )
    .ok()
}

/// Set a control's text (UTF-16).
unsafe fn set_text(hwnd: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
}

/// Read a control's text (UTF-16, truncated at the first NUL).
unsafe fn edit_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// Trim `s`; `None` when empty (optional username/domain fields).
fn opt_nonempty(s: String) -> Option<String> {
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Save-dialog window procedure: Save validates and upserts into the hub store
/// (DPAPI-protecting the password); Cancel/Close just close. Never panics.
unsafe extern "system" fn save_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => match wparam.0 & 0xffff {
            DLG_SAVE => {
                save_from_dialog(hwnd);
                LRESULT(0)
            }
            DLG_CANCEL => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        },
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Read the dialog fields, validate, and save a new connection through
/// [`ConnectionStore::upsert`] (which protects the password with DPAPI before
/// it ever reaches disk). The dialog closes on success.
unsafe fn save_from_dialog(dlg: HWND) {
    let name = edit_text(GetDlgItem(Some(dlg), DLG_NAME as i32).unwrap_or_default());
    let host = edit_text(GetDlgItem(Some(dlg), DLG_HOST as i32).unwrap_or_default());
    let port_text = edit_text(GetDlgItem(Some(dlg), DLG_PORT as i32).unwrap_or_default());
    let user = edit_text(GetDlgItem(Some(dlg), DLG_USER as i32).unwrap_or_default());
    let domain = edit_text(GetDlgItem(Some(dlg), DLG_DOMAIN as i32).unwrap_or_default());
    let password = edit_text(GetDlgItem(Some(dlg), DLG_PASSWORD as i32).unwrap_or_default());

    let host = host.trim().to_string();
    if host.is_empty() {
        let _ = MessageBoxW(
            Some(dlg),
            w!("Host must not be empty."),
            w!("Save Connection"),
            MB_OK | MB_ICONWARNING,
        );
        return;
    }
    let port: u16 = match port_text.trim().parse::<u16>() {
        Ok(p) if (1..=65535).contains(&p) => p,
        _ => {
            let _ = MessageBoxW(
                Some(dlg),
                w!("Port must be a number from 1 to 65535."),
                w!("Save Connection"),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    // (Copied out: the store's file I/O runs without the lock.)
    let saved_as = SESSION.lock().ok().and_then(|s| s.as_ref().and_then(|s| s.connection_id));
    let input = ConnectionInput {
        id: saved_as,
        display_name: if name.trim().is_empty() {
            host.clone()
        } else {
            name.trim().to_string()
        },
        host: host.clone(),
        port,
        username: opt_nonempty(user),
        domain: opt_nonempty(domain),
        // A blank password means the plaintext is no longer available (or the
        // user chose not to store one); a non-blank one is DPAPI-protected
        // inside upsert via ProtectedPassword::protect.
        password: if password.trim().is_empty() {
            None
        } else {
            Some(password)
        },
    };

    match ConnectionStore::load().and_then(|mut store| store.upsert(input)) {
        Ok(rec) => {
            tracing::info!(id = %rec.id, host = %rec.host, "saved connection from session bar");
            // Saved now: no Save button from the next tick on, and a later
            // save (after a reconnect) updates this record.
            if let Ok(mut s) = SESSION.lock() {
                if let Some(s) = s.as_mut() {
                    s.connection_id = Some(rec.id);
                }
            }
            HAS_INFO.store(false, Ordering::Relaxed);
            let _ = DestroyWindow(dlg);
        }
        Err(e) => {
            tracing::error!(error = %e, "could not save connection from session bar");
            let _ = MessageBoxW(
                Some(dlg),
                w!("Could not save the connection."),
                w!("Save Connection"),
                MB_OK | MB_ICONWARNING,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_connection_has_no_save_button() {
        let info = |host: &str, id: Option<uuid::Uuid>| SessionInfo {
            display_name: host.into(),
            host: host.into(),
            port: 3389,
            username: None,
            domain: None,
            password: None,
            connection_id: id,
        };
        let saved_as = |host| SESSION.lock().unwrap().as_ref().filter(|s| s.host == host).and_then(|s| s.connection_id);
        // A quick connect can be saved.
        set_session_info(info("quick.corp", None));
        assert!(HAS_INFO.load(Ordering::Relaxed));
        // Once the bar saved it (as save_from_dialog notes it), a reconnect's
        // facts keep it saved: no second Save, no duplicate.
        let id = uuid::Uuid::new_v4();
        SESSION.lock().unwrap().as_mut().unwrap().connection_id = Some(id);
        set_session_info(info("quick.corp", None));
        assert!(!HAS_INFO.load(Ordering::Relaxed));
        assert_eq!(saved_as("quick.corp"), Some(id));
        // Started from a saved connection: nothing to save.
        let launched = uuid::Uuid::new_v4();
        set_session_info(info("saved.corp", Some(launched)));
        assert!(!HAS_INFO.load(Ordering::Relaxed));
        assert_eq!(saved_as("saved.corp"), Some(launched));
        // Another PC is not the one saved before.
        set_session_info(info("other.corp", None));
        assert!(HAS_INFO.load(Ordering::Relaxed));
        assert_eq!(saved_as("other.corp"), None);
    }

    #[test]
    fn the_bar_is_as_wide_as_its_buttons() {
        // Pin, Minimize and Disconnect: a Remote Desktop session over several
        // screens with nothing to save.
        let (at, width) = layout(0b100011);
        assert_eq!(at[0], Some((4, 56)));
        assert_eq!(at[1], Some((64, 84)));
        assert_eq!(at[2], None);
        assert_eq!(at[5], Some((152, 92)));
        assert_eq!(width, 248);
        // Everything.
        let (at, width) = layout(0b111111);
        assert!(at.iter().all(Option::is_some));
        assert_eq!(width, GAP + BUTTONS.iter().map(|b| b.2 + GAP).sum::<i32>());
    }
}
