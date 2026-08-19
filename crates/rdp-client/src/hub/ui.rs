//! Native Win32 hub window: the connection picker that replaces the M0 demo
//! window when rdpio launches without connection arguments (wired in step-8).
//!
//! Layout: a slim left activity rail with three owner-draw buttons — Recent,
//! Saved, New Connection — and a content area to its right. The content area
//! hosts one child "pane" window per section; switching sections hides the
//! current pane and shows the target one. Panes are created lazily and reused,
//! and this step ships the shell with placeholder panes; steps 6/7 replace the
//! Recent/Saved panes with `SysListView32` lists + filter edits and the New
//! pane with the connection form.
//!
//! The Win32 pattern mirrors `connbar.rs`: `InitCommonControlsEx`, a registered
//! window class with a `WNDPROC`, child controls created via `CreateWindowExW`.
//! Unlike connbar (which keeps state in process statics), the hub stores its
//! whole [`HubWindow`] in a `Box` whose pointer is attached to the window with
//! `SetWindowLongPtrW(hwnd, GWLP_USERDATA, …)` and recovered in the window
//! procedure via `GetWindowLongPtrW`. Every message handler is a fallible
//! `Result` method; the window procedure logs any error with `tracing` and
//! never unwraps or panics on the UI path.

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetStockObject, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_TOP, DT_VCENTER, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, DRAWITEMSTRUCT, ICC_STANDARD_CLASSES, ICC_WIN95_CLASSES,
    INITCOMMONCONTROLSEX, ODT_BUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{ConnectionTarget, HubError};

/// Control ids for the three activity-rail buttons (delivered as the LOWORD of
/// `WM_COMMAND`'s `wParam`).
const ID_RAIL_RECENT: usize = 1;
const ID_RAIL_SAVED: usize = 2;
const ID_RAIL_NEW: usize = 3;

/// Default hub window size (in pixels; `WM_SIZE` re-lays-out from the real
/// client rect after creation).
const HUB_W: i32 = 940;
const HUB_H: i32 = 620;

/// Slim left activity rail geometry.
const RAIL_W: i32 = 152;
const RAIL_PAD_TOP: i32 = 16;
const RAIL_BTN_X: i32 = 10;
const RAIL_BTN_W: i32 = RAIL_W - RAIL_BTN_X * 2;
const RAIL_BTN_H: i32 = 40;
const RAIL_BTN_GAP: i32 = 6;

/// Dark-theme palette. `COLORREF` is 0x00BBGGRR.
const COLOR_RAIL: u32 = 0x001E1E1E; // hub background visible behind the rail
const COLOR_BG: u32 = 0x00252525; // content pane background
const COLOR_RAIL_BTN: u32 = 0x002E2E2E; // inactive rail button
const COLOR_RAIL_BTN_ACTIVE: u32 = 0x0080531F; // RGB(0x1F,0x53,0x80) blue accent
const COLOR_TEXT: u32 = 0x00E8E8E8; // bright text
const COLOR_TEXT_DIM: u32 = 0x009A9A9A; // muted text

/// The three hub activity sections, in rail order (index == rail slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Recent,
    Saved,
    New,
}

impl Section {
    const ALL: [Section; 3] = [Section::Recent, Section::Saved, Section::New];

    fn index(self) -> usize {
        match self {
            Section::Recent => 0,
            Section::Saved => 1,
            Section::New => 2,
        }
    }

    fn from_index(i: usize) -> Option<Section> {
        match i {
            0 => Some(Section::Recent),
            1 => Some(Section::Saved),
            2 => Some(Section::New),
            _ => None,
        }
    }

    fn id(self) -> usize {
        match self {
            Section::Recent => ID_RAIL_RECENT,
            Section::Saved => ID_RAIL_SAVED,
            Section::New => ID_RAIL_NEW,
        }
    }

    fn from_id(id: usize) -> Option<Section> {
        match id {
            ID_RAIL_RECENT => Some(Section::Recent),
            ID_RAIL_SAVED => Some(Section::Saved),
            ID_RAIL_NEW => Some(Section::New),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Section::Recent => "Recent",
            Section::Saved => "Saved",
            Section::New => "New Connection",
        }
    }

    /// Placeholder text shown under the title until step-6/7 fill the pane
    /// with the real controls.
    fn placeholder_subtitle(self) -> &'static str {
        match self {
            Section::Recent => "Connections you have used will appear here.",
            Section::Saved => "Saved connections will appear here once you create one.",
            Section::New => "Create and save a connection from the form on this pane.",
        }
    }
}

/// Per-window state for the hub, attached to the main window via
/// `GWLP_USERDATA`. Lives in a `Box` owned by `ui::run`, so the raw pointer the
/// window procedure holds stays valid for the whole message loop.
struct HubWindow {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// The active section (highlighted rail button + visible content pane).
    section: Section,
    /// The three rail buttons, indexed by [`Section::index`].
    rail: [HWND; 3],
    /// The content pane per section; created lazily on first visit and reused.
    panes: [Option<HWND>; 3],
    /// The pane currently visible (mirrors `section`; kept so layout can move
    /// it without re-deriving from the section).
    content: Option<HWND>,
    /// Set once `WM_DESTROY` runs so `Drop` never double-destroys windows.
    destroyed: bool,
}

impl HubWindow {
    /// Register the classes, create the main window, attach the state, build
    /// the rail, and show the initial (Recent) pane. Returns the boxed state
    /// whose pointer is stored in the window's `GWLP_USERDATA`.
    fn create() -> Result<Box<HubWindow>, HubError> {
        unsafe {
            // Common controls must be initialised before any SysListView32 (or
            // other common control) is created; the owner-draw button messages
            // also go through the common-controls dispatch. Best-effort: on
            // every supported Windows this has been a no-op success for years.
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES | ICC_STANDARD_CLASSES,
            };
            if InitCommonControlsEx(&icc).0 == 0 {
                tracing::warn!("InitCommonControlsEx failed; common controls may misbehave");
            }

            let module = GetModuleHandleW(None)
                .map_err(|e| HubError::win32(format!("GetModuleHandleW: {e}")))?;
            let hinstance = HINSTANCE(module.0);

            // Main hub window class.
            let class_name = w!("rdpioHubWindowClass");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(hub_proc),
                hInstance: hinstance,
                lpszClassName: class_name,
                hCursor: LoadCursorW(None, IDC_ARROW)
                    .map_err(|e| HubError::win32(format!("LoadCursorW: {e}")))?,
                // The rail background; the content panes paint over their own
                // area with COLOR_BG, so the hub brush only shows in the rail.
                hbrBackground: CreateSolidBrush(COLORREF(COLOR_RAIL)),
                ..Default::default()
            };
            // Registering an already-registered class fails; one window per
            // process, so ignoring "already exists" is fine (as in connbar.rs).
            let _ = RegisterClassExW(&wc);

            // Placeholder pane class: custom-painted child windows that draw
            // the section title + subtitle. Steps 6/7 replace these with real
            // control hosts, but the shell needs a class to switch between.
            let pane_class = w!("rdpioHubPaneClass");
            let pane_wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(hub_pane_proc),
                hInstance: hinstance,
                lpszClassName: pane_class,
                hbrBackground: CreateSolidBrush(COLORREF(COLOR_BG)),
                ..Default::default()
            };
            let _ = RegisterClassExW(&pane_wc);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("RDPiO"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                HUB_W,
                HUB_H,
                None,
                None,
                Some(hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(hub): {e}")))?;

            let mut hub = Box::new(HubWindow {
                hwnd,
                hinstance,
                section: Section::Recent,
                rail: [HWND::default(); 3],
                panes: [None, None, None],
                content: None,
                destroyed: false,
            });

            // Attach the state before creating any child: WM_SIZE / WM_DRAWITEM
            // can arrive as soon as a control exists.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *hub as *mut HubWindow as isize);

            // Child creation is fallible; on failure destroy the window so the
            // error path leaves no orphaned HWND behind.
            if let Err(e) = hub.create_rail() {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
            if let Err(e) = hub.show_section(Section::Recent) {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }

            let _ = ShowWindow(hwnd, SW_SHOW);
            tracing::info!("hub window created");
            Ok(hub)
        }
    }

    /// Create the three owner-draw rail buttons as children of the hub window.
    fn create_rail(&mut self) -> Result<(), HubError> {
        unsafe {
            for (i, section) in Section::ALL.iter().enumerate() {
                let y = RAIL_PAD_TOP + i as i32 * (RAIL_BTN_H + RAIL_BTN_GAP);
                // BS_OWNERDRAW is a plain i32, WS_* are WINDOW_STYLE — combine
                // through the raw u32 so the style value stays well-typed.
                let style = WINDOW_STYLE(
                    (WS_CHILD | WS_VISIBLE).0 | BS_OWNERDRAW as u32 | BS_NOTIFY as u32,
                );
                let btn = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    w!(""),
                    style,
                    RAIL_BTN_X,
                    y,
                    RAIL_BTN_W,
                    RAIL_BTN_H,
                    Some(self.hwnd),
                    Some(HMENU(section.id() as *mut c_void)),
                    Some(self.hinstance),
                    None,
                )
                .map_err(|e| HubError::win32(format!("CreateWindowExW(rail {section:?}): {e}")))?;
                self.rail[i] = btn;
            }
            Ok(())
        }
    }

    /// Switch the visible content pane to `section`, creating its pane on first
    /// visit. Also re-highlights the rail buttons.
    fn show_section(&mut self, section: Section) -> Result<(), HubError> {
        unsafe {
            if let Some(cur) = self.content {
                let _ = ShowWindow(cur, SW_HIDE);
            }
            let idx = section.index();
            let pane = match self.panes[idx] {
                Some(p) => p,
                None => {
                    let p = self.create_pane(section)?;
                    self.panes[idx] = Some(p);
                    p
                }
            };
            let rc = self.content_rect()?;
            let _ = MoveWindow(
                pane,
                rc.left,
                rc.top,
                (rc.right - rc.left).max(1),
                (rc.bottom - rc.top).max(1),
                true,
            );
            let _ = ShowWindow(pane, SW_SHOW);
            self.content = Some(pane);
            self.section = section;
            self.repaint_rail();
            tracing::debug!(section = ?section, "hub switched section");
            Ok(())
        }
    }

    /// Create the placeholder content pane for `section` (step-5 shell; steps
    /// 6/7 replace this with the real per-section control sets).
    fn create_pane(&mut self, section: Section) -> Result<HWND, HubError> {
        unsafe {
            let rc = self.content_rect()?;
            let pane = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("rdpioHubPaneClass"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                rc.left,
                rc.top,
                (rc.right - rc.left).max(1),
                (rc.bottom - rc.top).max(1),
                Some(self.hwnd),
                None,
                Some(self.hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(pane {section:?}): {e}")))?;
            // The pane window procedure reads this to pick the placeholder text.
            SetWindowLongPtrW(pane, GWLP_USERDATA, section.index() as isize);
            Ok(pane)
        }
    }

    /// The content area rect: the full client rect minus the left rail.
    fn content_rect(&self) -> Result<RECT, HubError> {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(self.hwnd, &mut rc)
                .map_err(|e| HubError::win32(format!("GetClientRect: {e}")))?;
            rc.left += RAIL_W;
            Ok(rc)
        }
    }

    /// Re-layout all children for the current client size (WM_SIZE handler).
    fn layout(&mut self) -> Result<(), HubError> {
        unsafe {
            for (i, btn) in self.rail.iter().enumerate() {
                let y = RAIL_PAD_TOP + i as i32 * (RAIL_BTN_H + RAIL_BTN_GAP);
                let _ = MoveWindow(*btn, RAIL_BTN_X, y, RAIL_BTN_W, RAIL_BTN_H, true);
            }
            if let Some(pane) = self.content {
                let rc = self.content_rect()?;
                let _ = MoveWindow(
                    pane,
                    rc.left,
                    rc.top,
                    (rc.right - rc.left).max(1),
                    (rc.bottom - rc.top).max(1),
                    true,
                );
            }
            Ok(())
        }
    }

    /// Force every rail button to repaint so the active-section highlight
    /// reflects `self.section`.
    fn repaint_rail(&self) {
        unsafe {
            for btn in &self.rail {
                let _ = InvalidateRect(Some(*btn), None, true);
            }
        }
    }

    // --- Fallible message handlers -------------------------------------------
    // Each returns Result so the window procedure can log the failure and keep
    // running; none of them unwrap or panic.

    /// WM_COMMAND: rail-button clicks arrive here with the control id in the
    /// LOWORD of `wParam`.
    fn on_command(&mut self, wparam: WPARAM, _lparam: LPARAM) -> Result<(), HubError> {
        let id = wparam.0 & 0xffff;
        if let Some(section) = Section::from_id(id) {
            self.show_section(section)?;
        }
        Ok(())
    }

    /// WM_NOTIFY: common-control notifications (list-view activation, etc.).
    /// The step-5 shell has no common controls to handle yet — step-6 routes
    /// NM_DBLCLK / LVN_ITEMACTIVATE here.
    fn on_notify(&mut self, _lparam: LPARAM) -> Result<(), HubError> {
        Ok(())
    }

    /// WM_DRAWITEM: paint the owner-draw rail buttons.
    fn on_draw_item(&self, lparam: LPARAM) -> Result<(), HubError> {
        let ptr = lparam.0 as *const DRAWITEMSTRUCT;
        if ptr.is_null() {
            return Ok(());
        }
        let dis = unsafe { &*ptr };
        if dis.CtlType != ODT_BUTTON {
            return Ok(());
        }
        let Some(section) = Section::from_id(dis.CtlID as usize) else {
            return Ok(());
        };

        let active = section == self.section;
        let bg = if active {
            COLOR_RAIL_BTN_ACTIVE
        } else {
            COLOR_RAIL_BTN
        };
        unsafe {
            let brush = CreateSolidBrush(COLORREF(bg));
            let _ = FillRect(dis.hDC, &dis.rcItem, brush);
            let _ = DeleteObject(brush.into());

            let _ = SetBkMode(dis.hDC, TRANSPARENT);
            let _ = SetTextColor(
                dis.hDC,
                COLORREF(if active { COLOR_TEXT } else { COLOR_TEXT_DIM }),
            );
            let mut text: Vec<u16> = section
                .title()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut rc = dis.rcItem;
            let _ =
                DrawTextW(dis.hDC, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        Ok(())
    }

    /// WM_SIZE: re-layout the rail and the visible pane for the new client size.
    fn on_size(&mut self, _lparam: LPARAM) -> Result<(), HubError> {
        self.layout()
    }

    /// WM_DESTROY: the window is going away. Marks the state destroyed (so
    /// `Drop` won't double-destroy) and lets the caller post WM_QUIT.
    fn on_destroy(&mut self) {
        self.destroyed = true;
        tracing::info!("hub window destroyed");
    }
}

impl Drop for HubWindow {
    fn drop(&mut self) {
        unsafe {
            // On the normal close path WM_DESTROY already ran (children and the
            // main window are gone); only destroy explicitly when create()
            // failed partway and the Box is dropped with the window still alive.
            if !self.destroyed {
                for pane in self.panes.iter().flatten() {
                    let _ = DestroyWindow(*pane);
                }
                for btn in &self.rail {
                    let _ = DestroyWindow(*btn);
                }
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

/// Recover the per-window state pointer stashed with `SetWindowLongPtrW`.
/// Returns `None` before the state is attached or after `WM_NCDESTROY` clears
/// it, so the procedure can fall back to `DefWindowProcW`.
unsafe fn hub_state_mut(hwnd: HWND) -> Option<&'static mut HubWindow> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr == 0 {
        None
    } else {
        Some(&mut *(ptr as *mut HubWindow))
    }
}

/// Hub main window procedure. Every handled message routes to a fallible method
/// whose errors are logged; the procedure itself never unwraps or panics.
unsafe extern "system" fn hub_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            if let Some(state) = hub_state_mut(hwnd) {
                if let Err(e) = state.on_command(wparam, lparam) {
                    tracing::error!(error = %e, "hub WM_COMMAND handler failed");
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NOTIFY => {
            if let Some(state) = hub_state_mut(hwnd) {
                if let Err(e) = state.on_notify(lparam) {
                    tracing::error!(error = %e, "hub WM_NOTIFY handler failed");
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DRAWITEM => {
            if let Some(state) = hub_state_mut(hwnd) {
                if let Err(e) = state.on_draw_item(lparam) {
                    tracing::error!(error = %e, "hub WM_DRAWITEM handler failed");
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SIZE => {
            if let Some(state) = hub_state_mut(hwnd) {
                if let Err(e) = state.on_size(lparam) {
                    tracing::error!(error = %e, "hub WM_SIZE handler failed");
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            if let Some(state) = hub_state_mut(hwnd) {
                state.on_destroy();
            }
            // Ends the GetMessageW loop in `ui::run`, which returns Ok(None).
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // The window is gone; detach the state pointer so no late message
            // can reach the (still alive, about-to-be-dropped) HubWindow.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Placeholder pane window procedure: paints the section title + subtitle on a
/// dark background. Steps 6/7 replace this with the real control hosts.
unsafe extern "system" fn hub_pane_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            let section = match Section::from_index(idx as usize) {
                Some(s) => s,
                None => Section::Recent,
            };
            paint_placeholder(hwnd, hdc, &ps.rcPaint, section);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // Everything is painted in WM_PAINT; skip the erase pass to avoid
        // flicker.
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Draw the placeholder pane content: background fill (over the invalid clip
/// rect), then a title and a dim subtitle positioned against the full client
/// rect so partial repaints still land in the right place.
fn paint_placeholder(hwnd: HWND, hdc: HDC, clip: &RECT, section: Section) {
    unsafe {
        let bg = CreateSolidBrush(COLORREF(COLOR_BG));
        let _ = FillRect(hdc, clip, bg);
        let _ = DeleteObject(bg.into());

        let mut full = RECT::default();
        if GetClientRect(hwnd, &mut full).is_err() {
            full = *clip;
        }

        let _ = SetBkMode(hdc, TRANSPARENT);
        let title_font = GetStockObject(DEFAULT_GUI_FONT);
        let old_font = SelectObject(hdc, title_font);

        let _ = SetTextColor(hdc, COLORREF(COLOR_TEXT));
        let title = section.title();
        let mut title_buf: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let mut title_rc = RECT {
            left: full.left + 28,
            top: full.top + 24,
            right: full.right - 28,
            bottom: full.top + 72,
        };
        let _ = DrawTextW(hdc, &mut title_buf, &mut title_rc, DT_LEFT | DT_TOP | DT_SINGLELINE);

        let _ = SetTextColor(hdc, COLORREF(COLOR_TEXT_DIM));
        let subtitle = section.placeholder_subtitle();
        let mut sub_buf: Vec<u16> = subtitle.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sub_rc = RECT {
            left: full.left + 28,
            top: full.top + 76,
            right: full.right - 28,
            bottom: full.top + 120,
        };
        let _ = DrawTextW(hdc, &mut sub_buf, &mut sub_rc, DT_LEFT | DT_TOP | DT_SINGLELINE);

        let _ = SelectObject(hdc, old_font);
    }
}

/// Run the hub window and its message loop until the user closes it. Returns
/// the selected [`ConnectionTarget`] once steps 6/8 wire the selection through;
/// `None` today (the shell has no selectable rows yet) and whenever the hub is
/// closed without choosing a connection.
pub fn run() -> Result<Option<ConnectionTarget>, HubError> {
    // The Box keeps the HubWindow alive (and its GWLP_USERDATA pointer valid)
    // for the whole loop; it is dropped after WM_QUIT ends the loop.
    let _hub = HubWindow::create()?;
    tracing::info!("hub window open; entering message loop");
    let mut msg = MSG::default();
    unsafe {
        // GetMessageW returns 0 on WM_QUIT (posted from WM_DESTROY) and -1 on
        // error; both end the loop cleanly.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tracing::info!("hub window closed");
    Ok(None)
}
