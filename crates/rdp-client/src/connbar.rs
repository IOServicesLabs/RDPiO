//! Floating auto-hide connection bar (mstsc-style) for the borderless
//! fullscreen / multi-monitor modes, where the session windows have no frame and
//! thus no close button.
//!
//! A small topmost tool window pinned to the top-centre of the primary monitor
//! with three child buttons: **Pin** (toggle auto-hide), **Save** (opens the
//! "Save this connection" dialog, prefilled with the active session's details)
//! and **Disconnect** (ends the session). When unpinned it hides and only reappears while the cursor is at
//! the top-centre edge of the screen — driven by [`ConnBar::tick`], polled once
//! per frame from the UI loop (the bar gets no mouse messages while hidden, so we
//! can't rely on `WM_MOUSELEAVE`). `WS_EX_NOACTIVATE` keeps the session window
//! focused when its buttons are clicked, so input + the keyboard hook keep working.
//!
//! Disconnect just posts `WM_QUIT` to the shared UI thread, which the window
//! pump already turns into a clean shutdown (same as Ctrl+Shift+Q).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

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
/// The Pin button's HWND (so `WM_COMMAND` can relabel it). One bar per process.
static PIN_BTN: AtomicIsize = AtomicIsize::new(0);
/// The active session's connection facts (set once at connect time), used to
/// prefill the "Save this connection" dialog.
static SESSION: Mutex<Option<SessionInfo>> = Mutex::new(None);

const ID_PIN: usize = 1;
const ID_DISCONNECT: usize = 2;
const ID_SAVE: usize = 3;
const BAR_W: i32 = 260;
const BAR_H: i32 = 34;

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
}

/// Attach session metadata to the bar so "Save this connection" can prefill the
/// dialog. Call once per session, before the bar is first shown. Best-effort: a
/// poisoned lock just means the dialog opens blank.
pub fn set_session_info(info: SessionInfo) {
    tracing::debug!(host = %info.host, "connection bar session info set");
    if let Ok(mut s) = SESSION.lock() {
        *s = Some(info);
    }
}

/// A floating connection bar bound to the primary monitor.
pub struct ConnBar {
    hwnd: HWND,
    left: i32,
    top: i32,
}

impl ConnBar {
    /// Create the bar centred at the top of the monitor whose screen rectangle is
    /// `(left, top)`..`(left + width, ..)`. Shown initially (so it's discoverable);
    /// it auto-hides on the first `tick` once the cursor moves away (unless pinned).
    pub fn new(primary_left: i32, primary_top: i32, primary_width: i32) -> windows::core::Result<Self> {
        unsafe {
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

            let left = primary_left + (primary_width - BAR_W) / 2;
            let top = primary_top;
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name,
                w!("RDPiO"),
                WS_POPUP,
                left,
                top,
                BAR_W,
                BAR_H,
                None,
                None,
                Some(hinstance),
                None,
            )?;

            // Three push buttons; clicks arrive as WM_COMMAND with these control
            // ids: Pin (toggle auto-hide), Save (open the save dialog), and
            // Disconnect (end the session).
            let pin = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Pin"),
                WS_CHILD | WS_VISIBLE,
                4,
                3,
                80,
                BAR_H - 6,
                Some(hwnd),
                Some(HMENU(ID_PIN as *mut core::ffi::c_void)),
                Some(hinstance),
                None,
            )?;
            PIN_BTN.store(pin.0 as isize, Ordering::SeqCst);
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Save"),
                WS_CHILD | WS_VISIBLE,
                88,
                3,
                72,
                BAR_H - 6,
                Some(hwnd),
                Some(HMENU(ID_SAVE as *mut core::ffi::c_void)),
                Some(hinstance),
                None,
            );
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Disconnect"),
                WS_CHILD | WS_VISIBLE,
                164,
                3,
                BAR_W - 168,
                BAR_H - 6,
                Some(hwnd),
                Some(HMENU(ID_DISCONNECT as *mut core::ffi::c_void)),
                Some(hinstance),
                None,
            );

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            VISIBLE.store(true, Ordering::SeqCst);
            Ok(Self { hwnd, left, top })
        }
    }

    /// Poll the cursor and show/hide the bar. Call once per UI-loop iteration.
    /// Pinned → always shown. Otherwise shown only while the cursor is in the
    /// top-centre reveal strip above the bar.
    pub fn tick(&self) {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let reveal = PINNED.load(Ordering::SeqCst)
                || (pt.y <= self.top + BAR_H
                    && pt.x >= self.left - 48
                    && pt.x <= self.left + BAR_W + 48);
            let visible = VISIBLE.load(Ordering::SeqCst);
            if reveal && !visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                VISIBLE.store(true, Ordering::SeqCst);
            } else if !reveal && visible {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                VISIBLE.store(false, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for ConnBar {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Connection-bar window procedure: handle the two buttons; everything else is
/// default. Pin toggles auto-hide and relabels itself; Disconnect ends the session.
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
                let btn = HWND(PIN_BTN.load(Ordering::SeqCst) as *mut core::ffi::c_void);
                let _ = SetWindowTextW(btn, if pinned { w!("Unpin") } else { w!("Pin") });
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

    let input = ConnectionInput {
        id: None,
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
