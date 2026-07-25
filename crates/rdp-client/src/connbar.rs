//! Floating auto-hide connection bar (mstsc-style) for the borderless
//! fullscreen / multi-monitor modes, where the session windows have no frame and
//! thus no close button.
//!
//! A small topmost tool window pinned to the top-centre of the primary monitor
//! with two child buttons: **Pin** (toggle auto-hide) and **Disconnect** (ends
//! the session). When unpinned it hides and only reappears while the cursor is at
//! the top-centre edge of the screen — driven by [`ConnBar::tick`], polled once
//! per frame from the UI loop (the bar gets no mouse messages while hidden, so we
//! can't rely on `WM_MOUSELEAVE`). `WS_EX_NOACTIVATE` keeps the session window
//! focused when its buttons are clicked, so input + the keyboard hook keep working.
//!
//! Disconnect just posts `WM_QUIT` to the shared UI thread, which the window
//! pump already turns into a clean shutdown (same as Ctrl+Shift+Q).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Whether the bar is pinned (always visible). When false it auto-hides.
static PINNED: AtomicBool = AtomicBool::new(false);
/// Whether the bar window is currently shown (so `tick` only toggles on change).
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// The Pin button's HWND (so `WM_COMMAND` can relabel it). One bar per process.
static PIN_BTN: AtomicIsize = AtomicIsize::new(0);

const ID_PIN: usize = 1;
const ID_DISCONNECT: usize = 2;
const BAR_W: i32 = 260;
const BAR_H: i32 = 34;

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

            // Two push buttons; clicks arrive as WM_COMMAND with these control ids.
            let pin = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Pin"),
                WS_CHILD | WS_VISIBLE,
                4,
                3,
                118,
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
                w!("Disconnect"),
                WS_CHILD | WS_VISIBLE,
                126,
                3,
                BAR_W - 130,
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
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
