//! Minimal Win32 window for M0.
//!
//! Registers a class, creates the HWND, and drives a non-blocking message pump
//! so the render loop can present every iteration. Window state (pending resize
//! and the quit flag) is shared through atomics — sufficient for the single
//! top-level window we own at M0; a per-window state pointer in `GWLP_USERDATA`
//! comes when we support multiple monitors.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBRUSH, HDC, HGDIOBJ, HMONITOR, MONITORINFO,
};
// `MONITORINFOF_PRIMARY` lives under WindowsAndMessaging (glob-imported below).
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, GetKeyState, VK_CAPITAL, VK_CONTROL, VK_NUMLOCK, VK_SCROLL, VK_SHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::Input::Touch::{
    CloseTouchInputHandle, GetTouchInputInfo, RegisterTouchWindow, HTOUCHINPUT, TOUCHINPUT,
    TOUCHEVENTF_DOWN, TOUCHEVENTF_MOVE, TOUCHEVENTF_UP,
};
use windows::Win32::System::DataExchange::AddClipboardFormatListener;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Packed `width << 16 | height` of a pending resize, or 0 if none.
static PENDING_RESIZE: AtomicU32 = AtomicU32::new(0);
/// Set when a `WM_QUIT` is observed.
static QUIT: AtomicBool = AtomicBool::new(false);
/// Input events captured in the window procedure, drained by the UI loop.
static INPUT_QUEUE: Mutex<Vec<RawInput>> = Mutex::new(Vec::new());
/// The current server cursor (HCURSOR as isize; 0 = none → default arrow). The
/// window procedure reads this on `WM_SETCURSOR` so Windows doesn't reset the
/// cursor on every mouse move. A single top-level window → process-global is OK.
static CURSOR_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// Set when the server asked to hide the cursor (`SYSPTR_NULL`).
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
/// Mouse-capture ("game") mode, toggled with Ctrl+Shift+M: the cursor is
/// confined to the window (ClipCursor), and — when the server supports
/// TS_RELPOINTER_EVENT — motion is sent as raw relative deltas with the local
/// cursor hidden and recentred, so FPS aiming never pins at a window edge.
static CAPTURE: AtomicBool = AtomicBool::new(false);
/// Raw-input mouse registration is process-wide and done once.
static RAW_REGISTERED: AtomicBool = AtomicBool::new(false);

/// A raw, platform-decoded input event. The UI loop maps these to RDP input
/// PDUs (scaling mouse coordinates from client pixels to the desktop).
#[derive(Debug, Clone, Copy)]
pub enum RawInput {
    /// A key transition, identified by its hardware scancode (`extended` = the
    /// E0 prefix for nav/arrow/right-modifier keys).
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
    /// Absolute pointer move in window client pixels.
    MouseMove { x: i32, y: i32 },
    /// Relative pointer motion in raw device counts (capture mode only; the
    /// server must support TS_RELPOINTER_EVENT).
    MouseRel { dx: i32, dy: i32 },
    /// A mouse button transition (0 = left, 1 = right, 2 = middle).
    MouseButton {
        button: u8,
        x: i32,
        y: i32,
        down: bool,
    },
    /// An extended ("side") mouse button transition (0 = XBUTTON1/back,
    /// 1 = XBUTTON2/forward) — common on gaming mice.
    XButton {
        button: u8,
        x: i32,
        y: i32,
        down: bool,
    },
    /// Vertical wheel notch; `delta` is WHEEL_DELTA units (positive = up/away).
    MouseWheel { delta: i16 },
    /// Horizontal (tilt) wheel notch; `delta` is WHEEL_DELTA units
    /// (positive = right).
    MouseHWheel { delta: i16 },
    /// A Unicode character transition (IME-composed text with no scancode). Sent
    /// as a down/up pair so the server registers a keypress.
    Char { code: u16, down: bool },
    /// Synchronize the server's toggle-key (Caps/Num/Scroll Lock) state to the
    /// local keyboard, e.g. when the window regains focus. `toggle_flags` is a
    /// `TS_SYNC_*` mask.
    SyncLockKeys { toggle_flags: u32 },
    /// A multi-touch contact in window client pixels and its contact id. The
    /// caller maps client coordinates to virtual-desktop coordinates and sends
    /// the frame over RDPEI.
    Touch {
        id: u32,
        x: i32,
        y: i32,
        /// `0` = down, `1` = up, `2` = move/update, matching the RDPEI contact
        /// state transitions.
        phase: u8,
    },
}

fn push_input(ev: RawInput) {
    if let Ok(mut q) = INPUT_QUEUE.lock() {
        // Bound the queue so a stalled UI loop can't grow it without limit.
        if q.len() < 4096 {
            q.push(ev);
        }
    }
}

/// Result of pumping the message queue once.
pub enum Frame {
    /// Keep running; apply `resize` (if any) before presenting.
    Continue { resize: Option<(u32, u32)> },
    /// The window is closing.
    Quit,
}

/// Mark the process Per-Monitor-V2 DPI aware so Win32 reports *true physical*
/// monitor geometry instead of coordinates virtualized to the primary monitor's
/// scale factor. Must run before any window creation or monitor enumeration.
///
/// Without this, a mixed-DPI / mixed-resolution multi-monitor layout is
/// misreported: `EnumDisplayMonitors` hands back scaled sizes/positions that
/// don't match the real pixels the DXGI swapchains present in, so the spanned
/// framebuffer slices (and the remote taskbar living at a monitor's bottom edge)
/// land off-screen. PER_MONITOR_AWARE_V2 also gives borderless windows the full
/// physical monitor rectangle, so they truly cover the local taskbar.
pub fn set_process_dpi_aware() {
    unsafe {
        // Best-effort: if the OS is too old for V2 the call fails harmlessly and
        // we keep the manifest/default awareness.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// `HHOOK` of the installed low-level keyboard hook (0 = none). One per process.
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

/// Install a `WH_KEYBOARD_LL` hook so the reserved system key combos Windows would
/// otherwise consume locally — the Win keys, Alt+Tab/Alt+Shift+Tab, Alt+Esc and
/// Ctrl+Esc — are forwarded to the remote session (and swallowed locally) while an
/// rdpio window is in the foreground. Installed in every mode (windowed and
/// borderless), and self-gated on our window being the foreground window, so
/// Alt+Tab reaches the session whenever the user is focused on it and returns to
/// the local OS as soon as they click another app. Idempotent. Call on the UI
/// (pumping) thread.
pub fn install_keyboard_hook() {
    if KEYBOARD_HOOK.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), Some(HINSTANCE(hmod.0)), 0) {
            Ok(h) => {
                KEYBOARD_HOOK.store(h.0 as isize, Ordering::SeqCst);
                tracing::info!("low-level keyboard hook installed (system keys → remote)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "keyboard hook failed; Win/Alt+Tab stay local")
            }
        }
    }
}

/// True when the foreground window is one of ours (matches our window class), so
/// the hook only captures system keys while the user is actually in the session.
unsafe fn foreground_is_ours() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let mut buf = [0u16; 32];
    let n = GetClassNameW(fg, &mut buf);
    n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == "rdpioWindowClass"
}

/// Low-level keyboard hook procedure: forward the reserved system combos to the
/// session and swallow them locally; everything else flows through untouched.
unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && foreground_is_ours() {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        const VK_TAB: u32 = 0x09;
        const VK_ESCAPE: u32 = 0x1B;
        const VK_LWIN: u32 = 0x5B;
        const VK_RWIN: u32 = 0x5C;
        let alt = kb.flags.0 & LLKHF_ALTDOWN.0 != 0;
        let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        let capture = matches!(kb.vkCode, VK_LWIN | VK_RWIN)
            || (kb.vkCode == VK_TAB && alt)
            || (kb.vkCode == VK_ESCAPE && (alt || ctrl));
        if capture {
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            push_input(RawInput::Key {
                scancode: kb.scanCode as u16,
                extended: kb.flags.0 & LLKHF_EXTENDED.0 != 0,
                down,
            });
            return LRESULT(1); // swallow locally; the remote acts on it
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub struct Window {
    hwnd: HWND,
}

impl Window {
    /// A visible, resizable top-level window of the requested client size
    /// (single-monitor / windowed mode). Position is left to the OS.
    pub fn new(title: &str, width: u32, height: u32) -> windows::core::Result<Self> {
        Self::create(
            title,
            WINDOW_EX_STYLE(0),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width as i32,
            height as i32,
        )
    }

    /// A borderless window placed at `(x, y)` with exactly `width`×`height`
    /// pixels — used to span the virtual desktop across every physical monitor.
    /// `WS_POPUP` has no frame, so the client area equals the window rectangle
    /// and client pixels map 1:1 onto the spanned framebuffer.
    pub fn new_spanning(
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Self> {
        // Topmost so the borderless surface covers the local always-on-top taskbar;
        // otherwise the remote desktop's own taskbar/Start is hidden behind it.
        Self::create(title, WS_EX_TOPMOST, WS_POPUP, x, y, width as i32, height as i32)
    }

    /// A borderless window for one physical monitor at screen position
    /// `(x, y)`, sized `width`×`height`. `offset` is this monitor's top-left
    /// within the shared remote-desktop framebuffer; it's stowed in the window's
    /// `GWLP_USERDATA` so the window procedure can translate mouse coordinates
    /// from this window's client space into absolute desktop space (so input is
    /// continuous across the per-monitor windows, and dragging spans the seam).
    pub fn new_monitor(
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        offset: (i32, i32),
    ) -> windows::core::Result<Self> {
        let win = Self::create(title, WS_EX_TOPMOST, WS_POPUP, x, y, width as i32, height as i32)?;
        unsafe {
            // Pack the non-negative framebuffer offset (each component < 2^31)
            // into the single isize GWLP_USERDATA slot: high 32 bits = x, low = y.
            let packed = (((offset.0 as i64) << 32) | (offset.1 as i64 & 0xffff_ffff)) as isize;
            SetWindowLongPtrW(win.hwnd, GWLP_USERDATA, packed);
        }
        Ok(win)
    }

    /// Register the window class (once per process) and create a visible window
    /// with the given style, position, and size.
    fn create(
        title: &str,
        ex_style: WINDOW_EX_STYLE,
        style: WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> windows::core::Result<Self> {
        unsafe {
            let module = GetModuleHandleW(None)?;
            let hinstance = HINSTANCE(module.0);
            let class_name = w!("rdpioWindowClass");

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: class_name,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            // Registering the same class twice fails; we only ever create one
            // window per process, so a non-zero atom (or first-call success) is
            // all we need. Ignore "class already exists" on the off chance.
            let _ = RegisterClassExW(&wc);

            let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

            // windows 0.58 returns `Result<HWND>` (Err on failure); `?` propagates it.
            let hwnd = CreateWindowExW(
                ex_style,
                class_name,
                PCWSTR(title_w.as_ptr()),
                style,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(hinstance),
                None,
            )?;

            let _ = ShowWindow(hwnd, SW_SHOW);
            // Get WM_CLIPBOARDUPDATE when the local clipboard changes, so we can
            // re-advertise it to the remote session (clipboard redirection).
            let _ = AddClipboardFormatListener(hwnd);
            // Register for WM_TOUCH so the client can forward multi-touch input.
            let _ = RegisterTouchWindow(hwnd, windows::Win32::UI::Input::Touch::REGISTER_TOUCH_WINDOW_FLAGS(0));
            Ok(Self { hwnd })
        }
    }

    /// The raw `HWND` value, for handing to the GPU backend.
    pub fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Update the window title (e.g. to show a reconnecting status).
    pub fn set_title(&self, title: &str) {
        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title_w.as_ptr()));
        }
    }

    /// Drain all pending messages without blocking, returning whether to keep
    /// running and any resize to apply before the next present.
    pub fn pump(&self) -> Frame {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    QUIT.store(true, Ordering::SeqCst);
                } else {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        if QUIT.load(Ordering::SeqCst) {
            return Frame::Quit;
        }

        let packed = PENDING_RESIZE.swap(0, Ordering::SeqCst);
        let resize = (packed != 0).then_some(((packed >> 16) & 0xffff, packed & 0xffff));
        Frame::Continue { resize }
    }

    /// Take all input events captured since the last call.
    pub fn drain_input(&self) -> Vec<RawInput> {
        match INPUT_QUEUE.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }

    /// Realise a server cursor update as the window's cursor. Builds an HCURSOR
    /// from the shape's RGBA, swaps it in (destroying the previous one), and
    /// applies it immediately; `WM_SETCURSOR` re-applies it as the mouse moves.
    /// Must be called on the UI thread (the one that owns the window).
    pub fn set_cursor(&self, update: crate::session::CursorUpdate) {
        use crate::session::CursorUpdate;
        unsafe {
            match update {
                CursorUpdate::Hide => {
                    CURSOR_HIDDEN.store(true, Ordering::SeqCst);
                    let _ = SetCursor(Some(HCURSOR::default()));
                }
                CursorUpdate::Default => {
                    CURSOR_HIDDEN.store(false, Ordering::SeqCst);
                    let old = CURSOR_HANDLE.swap(0, Ordering::SeqCst);
                    if old != 0 {
                        let _ = DestroyCursor(HCURSOR(old as *mut c_void));
                    }
                    if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                        let _ = SetCursor(Some(arrow));
                    }
                }
                CursorUpdate::Shape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    rgba,
                } => match build_cursor(width, height, hot_x, hot_y, &rgba) {
                    Some(hcursor) => {
                        CURSOR_HIDDEN.store(false, Ordering::SeqCst);
                        let new = hcursor.0 as isize;
                        let old = CURSOR_HANDLE.swap(new, Ordering::SeqCst);
                        if old != 0 && old != new {
                            let _ = DestroyCursor(HCURSOR(old as *mut c_void));
                        }
                        let _ = SetCursor(Some(hcursor));
                    }
                    None => tracing::debug!("could not build cursor; keeping the current one"),
                },
            }
        }
    }
}

/// The client's physical monitor layout, enumerated from Win32, plus the
/// virtual-screen origin and bounding-box size. Used to span the RDP desktop
/// across every monitor.
#[derive(Debug, Clone)]
pub struct VirtualDesktop {
    /// Raw `rcMonitor` rectangles (right/bottom exclusive; origin may be
    /// negative) with the primary flag, in enumeration order.
    pub rects: Vec<rdp_pdu::gcc::VirtualScreenRect>,
    /// Virtual-screen origin (`SM_XVIRTUALSCREEN`, `SM_YVIRTUALSCREEN`); the
    /// top-left corner of the bounding box, which may be negative.
    pub origin: (i32, i32),
    /// Bounding-box size (`SM_CXVIRTUALSCREEN`, `SM_CYVIRTUALSCREEN`).
    pub size: (u32, u32),
}

impl VirtualDesktop {
    /// The RDP monitor layout (primary at `(0, 0)`, inclusive edges; see
    /// [`rdp_pdu::gcc::normalize_monitor_layout`]).
    pub fn monitor_defs(&self) -> Vec<rdp_pdu::gcc::MonitorDef> {
        rdp_pdu::gcc::normalize_monitor_layout(&self.rects)
    }

    /// The primary monitor's rectangle (the one flagged primary, else the first
    /// enumerated). `None` if no monitors were found.
    pub fn primary_rect(&self) -> Option<rdp_pdu::gcc::VirtualScreenRect> {
        self.rects
            .iter()
            .find(|r| r.primary)
            .or_else(|| self.rects.first())
            .copied()
    }
}

/// `EnumDisplayMonitors` callback: append each monitor's `rcMonitor` rectangle
/// and primary flag to the `Vec<VirtualScreenRect>` passed via `lparam`.
unsafe extern "system" fn monitor_enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let rects = &mut *(lparam.0 as *mut Vec<rdp_pdu::gcc::VirtualScreenRect>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut info).as_bool() {
        let r = info.rcMonitor;
        rects.push(rdp_pdu::gcc::VirtualScreenRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
        });
    }
    BOOL(1) // keep enumerating
}

/// Enumerate the client's physical monitors and the virtual-screen geometry.
pub fn enumerate_monitors() -> VirtualDesktop {
    unsafe {
        let mut rects: Vec<rdp_pdu::gcc::VirtualScreenRect> = Vec::new();
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum_proc),
            LPARAM(&mut rects as *mut _ as isize),
        );
        let origin = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        );
        let size = (
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32,
        );
        VirtualDesktop {
            rects,
            origin,
            size,
        }
    }
}

/// Build a colour HCURSOR from top-down RGBA8 (`width*height*4` bytes) with a
/// hotspot. Uses a 32-bpp top-down DIB section for the colour plane (alpha
/// preserved) plus an all-zero monochrome AND mask. Returns `None` on bad
/// dimensions or any GDI failure.
unsafe fn build_cursor(width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: &[u8]) -> Option<HCURSOR> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return None;
    }

    // 32-bpp top-down DIB (negative height) for the colour plane.
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w as i32;
    bmi.bmiHeader.biHeight = -(h as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm_color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        return None;
    }
    // RGBA (our layout) → BGRA (Windows DIB layout), alpha preserved.
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, w * h * 4);
    for i in 0..w * h {
        dst[i * 4] = rgba[i * 4 + 2]; // B
        dst[i * 4 + 1] = rgba[i * 4 + 1]; // G
        dst[i * 4 + 2] = rgba[i * 4]; // R
        dst[i * 4 + 3] = rgba[i * 4 + 3]; // A
    }

    // All-zero monochrome AND mask (alpha governs transparency for the colour).
    let hbm_mask = CreateBitmap(w as i32, h as i32, 1, 1, None);

    let icon_info = ICONINFO {
        fIcon: BOOL(0), // FALSE → a cursor (not an icon)
        xHotspot: hot_x as u32,
        yHotspot: hot_y as u32,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let cursor = CreateIconIndirect(&icon_info).ok();

    // CreateIconIndirect copies the bitmaps; free our originals either way.
    let _ = DeleteObject(HGDIOBJ(hbm_color.0));
    if !hbm_mask.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
    }
    cursor.map(|hicon| HCURSOR(hicon.0))
}

/// Signed LOWORD / HIWORD of an `LPARAM` (mouse coordinates can be negative).
#[inline]
fn loword_i32(v: isize) -> i32 {
    (v & 0xffff) as u16 as i16 as i32
}
#[inline]
fn hiword_i32(v: isize) -> i32 {
    ((v >> 16) & 0xffff) as u16 as i16 as i32
}

/// The framebuffer offset stowed in a window's `GWLP_USERDATA` (see
/// [`Window::new_monitor`]). `(0, 0)` for ordinary single windows, which leaves
/// their client coordinates unchanged.
#[inline]
unsafe fn window_offset(hwnd: HWND) -> (i32, i32) {
    let v = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    (((v >> 32) & 0xffff_ffff) as i32, (v & 0xffff_ffff) as i32)
}

/// Whether mouse-capture (game) mode is engaged.
pub fn capture_mode() -> bool {
    CAPTURE.load(Ordering::SeqCst)
}

/// The window's client rectangle in SCREEN coordinates (None if degenerate).
unsafe fn client_rect_on_screen(hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() || rc.right <= rc.left || rc.bottom <= rc.top {
        return None;
    }
    let mut tl = windows::Win32::Foundation::POINT { x: rc.left, y: rc.top };
    let mut br = windows::Win32::Foundation::POINT { x: rc.right, y: rc.bottom };
    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut tl);
    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut br);
    Some(RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y })
}

/// Confine the cursor to the window's client area (capture mode, focused).
unsafe fn capture_clip(hwnd: HWND) {
    if let Some(rc) = client_rect_on_screen(hwnd) {
        let _ = ClipCursor(Some(&rc as *const RECT));
    }
}

/// Park the local cursor at the window centre so relative motion never runs out
/// of room. (`SetCursorPos` injects an ABSOLUTE raw-input packet, which the
/// WM_INPUT handler ignores — no feedback loop.)
unsafe fn capture_recenter(hwnd: HWND) {
    if let Some(rc) = client_rect_on_screen(hwnd) {
        let _ = SetCursorPos((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
    }
}

/// Register for raw mouse input (usage page 1, usage 2) delivered as WM_INPUT
/// to this window. Process-wide, once.
unsafe fn ensure_raw_mouse(hwnd: HWND) {
    if RAW_REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RAWINPUTDEVICE_FLAGS(0),
        hwndTarget: hwnd,
    };
    if let Err(e) = RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
    {
        tracing::warn!(?e, "raw mouse registration failed; capture falls back to clip-only");
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            let _ = ClipCursor(None);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_SIZE => {
            // LOWORD = new client width, HIWORD = new client height.
            let width = (lparam.0 & 0xffff) as u32;
            let height = ((lparam.0 >> 16) & 0xffff) as u32;
            if width > 0 && height > 0 {
                PENDING_RESIZE.store((width << 16) | height, Ordering::SeqCst);
            }
            if CAPTURE.load(Ordering::SeqCst) && GetFocus() == hwnd {
                capture_clip(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            // VK_PROCESSKEY (0xE5): the IME is composing this keystroke — don't
            // forward the raw scancode; the composed text arrives via WM_IME_CHAR
            // (so we never double-send IME input alongside scancodes).
            const VK_PROCESSKEY: usize = 0xE5;
            if wparam.0 == VK_PROCESSKEY {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            // Local escape hatch: Ctrl+Shift+Q closes the client. Borderless
            // spanning/fullscreen windows have no frame or system menu (so Alt+F4
            // does nothing) and every other key is forwarded to the server, which
            // otherwise leaves no way to quit. Consumed locally — not forwarded.
            const VK_Q: usize = 0x51;
            if down
                && wparam.0 == VK_Q
                && (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0
                && (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0
            {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            // Ctrl+Shift+M toggles mouse-capture (game) mode. Consumed locally.
            const VK_M: usize = 0x4D;
            if down
                && wparam.0 == VK_M
                && (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0
                && (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0
            {
                let on = !CAPTURE.load(Ordering::SeqCst);
                CAPTURE.store(on, Ordering::SeqCst);
                if on {
                    ensure_raw_mouse(hwnd);
                    capture_clip(hwnd);
                    if crate::session::rel_mouse_supported() {
                        capture_recenter(hwnd);
                    }
                } else {
                    let _ = ClipCursor(None);
                }
                // Re-evaluate cursor visibility immediately.
                let _ = PostMessageW(Some(hwnd), WM_SETCURSOR, WPARAM(hwnd.0 as usize), LPARAM(HTCLIENT as isize));
                tracing::info!(
                    on,
                    relative = crate::session::rel_mouse_supported(),
                    "mouse capture toggled (Ctrl+Shift+M)"
                );
                return LRESULT(0);
            }
            push_input(RawInput::Key {
                scancode: ((lparam.0 >> 16) & 0xff) as u16,
                extended: (lparam.0 & (1 << 24)) != 0,
                down,
            });
            // System keys still reach DefWindowProc so Alt+F4 etc. work locally;
            // ordinary keys are consumed (forwarded to the remote session).
            if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            } else {
                LRESULT(0)
            }
        }
        WM_IME_CHAR => {
            // An IME-composed Unicode character (wParam = the UTF-16 code unit),
            // which has no scancode. Forward it as a Unicode down/up pair.
            let code = (wparam.0 & 0xffff) as u16;
            push_input(RawInput::Char { code, down: true });
            push_input(RawInput::Char { code, down: false });
            LRESULT(0)
        }
        WM_INPUT => {
            // Raw mouse motion for capture mode: read the RAWINPUT packet and
            // forward RELATIVE deltas (absolute packets — injected moves, tablet
            // mice — are ignored). Outside capture mode raw input is unused.
            if CAPTURE.load(Ordering::SeqCst) && crate::session::rel_mouse_supported() {
                let mut raw = RAWINPUT::default();
                let mut size = std::mem::size_of::<RAWINPUT>() as u32;
                let got = GetRawInputData(
                    HRAWINPUT(lparam.0 as *mut c_void),
                    RID_INPUT,
                    Some(&mut raw as *mut RAWINPUT as *mut c_void),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                );
                if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
                    let m = raw.data.mouse;
                    if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0
                        && (m.lLastX != 0 || m.lLastY != 0)
                    {
                        push_input(RawInput::MouseRel {
                            dx: m.lLastX,
                            dy: m.lLastY,
                        });
                        // Keep the (hidden) local cursor parked mid-window so it
                        // never reaches the clip edge.
                        capture_recenter(hwnd);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEMOVE => {
            // In relative capture mode absolute moves are noise (the cursor is
            // parked at the window centre); motion flows via WM_INPUT instead.
            if CAPTURE.load(Ordering::SeqCst) && crate::session::rel_mouse_supported() {
                return LRESULT(0);
            }
            let (ox, oy) = window_offset(hwnd);
            push_input(RawInput::MouseMove {
                x: loword_i32(lparam.0) + ox,
                y: hiword_i32(lparam.0) + oy,
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => {
            let (button, down) = match msg {
                WM_LBUTTONDOWN => (0u8, true),
                WM_LBUTTONUP => (0, false),
                WM_RBUTTONDOWN => (1, true),
                WM_RBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                _ => (2, false),
            };
            let (ox, oy) = window_offset(hwnd);
            push_input(RawInput::MouseButton {
                button,
                x: loword_i32(lparam.0) + ox,
                y: hiword_i32(lparam.0) + oy,
                down,
            });
            LRESULT(0)
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // HIWORD(wparam) tells which X button (XBUTTON1=0x0001, XBUTTON2=0x0002).
            let down = msg == WM_XBUTTONDOWN;
            let which = ((wparam.0 >> 16) & 0xffff) as u16;
            let button = match which {
                0x0001 => 0u8, // XBUTTON1 (back)
                0x0002 => 1,   // XBUTTON2 (forward)
                _ => return LRESULT(1),
            };
            let (ox, oy) = window_offset(hwnd);
            push_input(RawInput::XButton {
                button,
                x: loword_i32(lparam.0) + ox,
                y: hiword_i32(lparam.0) + oy,
                down,
            });
            // MSDN: return TRUE when an X button message is processed.
            LRESULT(1)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
            push_input(RawInput::MouseWheel { delta });
            LRESULT(0)
        }
        WM_MOUSEHWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
            push_input(RawInput::MouseHWheel { delta });
            LRESULT(0)
        }
        WM_TOUCH => {
            // Multi-touch input: wParam holds the contact count and lParam a touch
            // input handle that we must close after reading.
            let count = (wparam.0 & 0xffff) as u32;
            let htouch = HTOUCHINPUT(lparam.0 as *mut c_void);
            let mut inputs = vec![TOUCHINPUT::default(); count as usize];
            let cb = std::mem::size_of::<TOUCHINPUT>() as i32;
            let ok = GetTouchInputInfo(htouch, &mut inputs, cb).is_ok();
            if ok {
                let (ox, oy) = window_offset(hwnd);
                for ti in inputs {
                    // TOUCHINPUT coordinates are in hundredths of a pixel.
                    let x = (ti.x as i32) / 100 + ox;
                    let y = (ti.y as i32) / 100 + oy;
                    let flags = ti.dwFlags;
                    let phase = if (flags.0 & TOUCHEVENTF_DOWN.0) != 0 {
                        0u8
                    } else if (flags.0 & TOUCHEVENTF_UP.0) != 0 {
                        1
                    } else if (flags.0 & TOUCHEVENTF_MOVE.0) != 0 {
                        2
                    } else {
                        continue;
                    };
                    push_input(RawInput::Touch {
                        id: ti.dwID,
                        x,
                        y,
                        phase,
                    });
                }
            }
            let _ = CloseTouchInputHandle(htouch);
            LRESULT(1)
        }
        WM_KILLFOCUS => {
            // Never hold the cursor hostage while another window has focus.
            if CAPTURE.load(Ordering::SeqCst) {
                let _ = ClipCursor(None);
            }
            LRESULT(0)
        }
        WM_MOVE => {
            // A moved window invalidates the capture clip rectangle.
            if CAPTURE.load(Ordering::SeqCst) && GetFocus() == hwnd {
                capture_clip(hwnd);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETFOCUS => {
            // Focus regained: restore the capture confinement.
            if CAPTURE.load(Ordering::SeqCst) {
                capture_clip(hwnd);
                if crate::session::rel_mouse_supported() {
                    capture_recenter(hwnd);
                }
            }
            // Re-sync the server's lock-key state to the local keyboard so Caps/
            // Num/Scroll Lock match after focus changes (TS_SYNC_* flags).
            const TS_SYNC_SCROLL_LOCK: u32 = 0x01;
            const TS_SYNC_NUM_LOCK: u32 = 0x02;
            const TS_SYNC_CAPS_LOCK: u32 = 0x04;
            let mut flags = 0u32;
            if GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 {
                flags |= TS_SYNC_CAPS_LOCK;
            }
            if GetKeyState(VK_NUMLOCK.0 as i32) & 1 != 0 {
                flags |= TS_SYNC_NUM_LOCK;
            }
            if GetKeyState(VK_SCROLL.0 as i32) & 1 != 0 {
                flags |= TS_SYNC_SCROLL_LOCK;
            }
            push_input(RawInput::SyncLockKeys { toggle_flags: flags });
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            // The local clipboard changed; flag it for the session to re-advertise.
            crate::session::clipboard_changed();
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Re-apply the server cursor while the pointer is over the client
            // area (LOWORD == HTCLIENT); let the frame keep system cursors.
            if (lparam.0 & 0xffff) as u32 == HTCLIENT {
                // Relative capture: the local cursor is a parked artifact —
                // hide it (the remote cursor is what the game shows, if any).
                if CAPTURE.load(Ordering::SeqCst) && crate::session::rel_mouse_supported() {
                    let _ = SetCursor(Some(HCURSOR::default()));
                    return LRESULT(1);
                }
                if CURSOR_HIDDEN.load(Ordering::SeqCst) {
                    let _ = SetCursor(Some(HCURSOR::default()));
                    return LRESULT(1);
                }
                let h = CURSOR_HANDLE.load(Ordering::SeqCst);
                if h != 0 {
                    let _ = SetCursor(Some(HCURSOR(h as *mut c_void)));
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
