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

use windows::core::{w, PCWSTR, PWSTR, HRESULT};
use windows::Win32::Foundation::{
    COLORREF, ERROR_INSUFFICIENT_BUFFER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect, GetWindowDC, HBRUSH, HDC,
    HGDIOBJ, InvalidateRect, ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    DT_CENTER, DT_SINGLELINE, DT_VCENTER, TRANSPARENT, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WindowsProgramming::GetUserNameW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, DRAWITEMSTRUCT, EM_SETCUEBANNER, ICC_STANDARD_CLASSES, ICC_WIN95_CLASSES,
    INITCOMMONCONTROLSEX, LVCOLUMNW, LVCOLUMNW_FORMAT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH,
    LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETBKCOLOR, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVM_SETTEXTBKCOLOR,
    LVNI_SELECTED, LVN_ITEMACTIVATE, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_EX_TRACKSELECT,
    LVS_NOCOLUMNHEADER, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMITEMACTIVATE,
    NMCUSTOMDRAW_DRAW_STATE_FLAGS, NM_CUSTOMDRAW, NM_DBLCLK, NMLVCUSTOMDRAW, CDDS_ITEMPREPAINT,
    CDDS_PREPAINT, CDIS_HOT, CDIS_SELECTED, CDRF_DODEFAULT, CDRF_NOTIFYITEMDRAW, ODT_BUTTON,
    WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::model::prefill_username;
use super::{
    format_local_short_datetime, format_recent_identity, theme, ConnectionInput, ConnectionRecord,
    ConnectionStore, ConnectionTarget, HubError, MruStore,
};

/// Control ids for the three activity-rail buttons (delivered as the LOWORD of
/// `WM_COMMAND`'s `wParam`).
const ID_RAIL_RECENT: usize = 1;
const ID_RAIL_SAVED: usize = 2;
const ID_RAIL_NEW: usize = 3;

/// Control ids for the Recent/Saved filter edits and SysListView32 lists.
const ID_FILTER_RECENT: usize = 4;
const ID_FILTER_SAVED: usize = 5;
const ID_LIST_RECENT: usize = 6;
const ID_LIST_SAVED: usize = 7;

/// Custom WM_COMMAND notification the (subclassed) filter edit posts when the
/// user presses Enter — single-line edit controls send no Enter notification
/// of their own.
const NOTIFY_FILTER_ENTER: u32 = 0x4000;

/// Custom message the (subclassed) activity-rail buttons post to the hub when
/// the mouse enters or leaves a rail button. `wParam` = rail index
/// ([`Section::index`]), `lParam` = 1 entered / 0 left. The hub updates its
/// hover state and repaints the affected button so `WM_DRAWITEM` can render
/// the `ROW_HOVER` fill. Private to the hub window, so `WM_APP` is the safe
/// base.
const WM_APP_RAIL_HOVER: u32 = WM_APP + 1;

/// VK_RETURN; kept local to avoid pulling in more windows feature modules.
const VK_RETURN: u16 = 0x0D;

/// Control ids for the New Connection form controls.
const ID_FORM_NAME: usize = 20;
const ID_FORM_HOST: usize = 21;
const ID_FORM_PORT: usize = 22;
const ID_FORM_USER: usize = 23;
const ID_FORM_DOMAIN: usize = 24;
const ID_FORM_PASSWORD: usize = 25;
const ID_FORM_SAVE_PW: usize = 26;
const ID_FORM_SAVE: usize = 27;
const ID_FORM_DELETE: usize = 28;

/// "Edit Selected" button under the Saved list (loads a row into the form).
const ID_BTN_EDIT_SAVED: usize = 29;

/// The primary "Connect" button in the New Connection form: owner-drawn and
/// accent-filled (buttons-dark step). The layout step positions it full-width
/// at the bottom of the form; this step owns its drawing and hit testing.
const ID_FORM_CONNECT: usize = 30;

/// BM_GETCHECK result values (kept local; the BST_* constants live in another
/// windows feature module we do not enable).
const BST_CHECKED: isize = 1;

/// New Connection form geometry (relative to the content area).
const FORM_X: i32 = 32;
const FORM_LABEL_W: i32 = 120;
const FORM_EDIT_H: i32 = 24;
const FORM_ROW_H: i32 = 36;
const FORM_PORT_W: i32 = 140;

/// Default port prefilled into the Port edit every time a fresh blank New
/// Connection form is opened (the standard RDP listener port). Only the blank
/// form gets this default; `load_into_form` sets the port from the saved
/// record and is unaffected.
const FORM_DEFAULT_PORT: &str = "3389";

/// Default hub window size (in pixels; `WM_SIZE` re-lays-out from the real
/// client rect after creation).
const HUB_W: i32 = 940;
const HUB_H: i32 = 620;

/// Slim left activity rail geometry. These are 96-DPI design values: the hub
/// multiplies them through `theme::scale_px` so the rail stays at the same DIP
/// size on high-DPI displays. `RAIL_BTN_H` (40 DIPs) is the minimum hit-target
/// height every rail button keeps, per the dark-rail step.
const RAIL_W: i32 = 152;
const RAIL_PAD_TOP: i32 = 16;
const RAIL_BTN_X: i32 = 10;
const RAIL_BTN_W: i32 = RAIL_W - RAIL_BTN_X * 2;
const RAIL_BTN_H: i32 = 40;
const RAIL_BTN_GAP: i32 = 6;

/// Width of the accent bar drawn on the active section's left edge (DIPs,
/// scaled via `theme::scale_px` like the rest of the rail geometry).
const RAIL_ACCENT_BAR_W: i32 = 2;

/// Height of the filter-edit strip at the top of each list pane.
const FILTER_H: i32 = 26;

/// Section-header geometry: each pane starts with a 10pt-semibold title strip
/// ("Recent" / "Saved" / "New Connection") above its content. Values are
/// 96-DPI design units; `WM_SIZE`/`WM_DPICHANGED` layout scales them through
/// `theme::scale_px`.
const SECTION_HEADER_TOP: i32 = 16;
const SECTION_HEADER_H: i32 = 24;
const SECTION_HEADER_GAP: i32 = 8;

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
}

/// One row of the Recent or Saved list: the four displayed columns plus the
/// payload needed to build a [`ConnectionTarget`] on activation.
#[derive(Debug, Clone)]
struct ListRow {
    display_name: String,
    host: String,
    /// Column text (username or empty).
    user: String,
    /// Pre-formatted "Last Used" column text.
    last_used: String,
    port: u16,
    username: Option<String>,
    domain: Option<String>,
    /// Links to a saved record whose DPAPI password can be reused. Always
    /// `Some` for Saved rows; for MRU rows it is the saved link when present.
    connection_id: Option<uuid::Uuid>,
}

/// The controls + live data of one list section (Recent or Saved).
struct SectionUi {
    /// Section title static ("Recent" / "Saved"), set to the 10pt semibold
    /// Segoe UI face by `apply_fonts`.
    header: HWND,
    /// Filter edit (subclassed to catch Enter).
    filter: HWND,
    /// SysListView32 report view.
    list: HWND,
    /// "Edit Selected" button under the Saved list (None for Recent).
    edit_btn: Option<HWND>,
    /// Current filter text (live-filtered on EN_CHANGE).
    filter_text: String,
    /// Full, unfiltered row set (rebuilt on section refresh).
    rows: Vec<ListRow>,
    /// Indices into `rows` matching the current filter, in list order.
    filtered: Vec<usize>,
}

/// The New Connection form: six labelled edit controls (display name, host,
/// port, username, domain, password), a save-password checkbox, and Save /
/// Delete buttons. `editing_id` is `Some` when the form was loaded from a
/// saved record — Save then updates it and Delete removes it.
struct FormState {
    editing_id: Option<uuid::Uuid>,
    /// "New Connection" title static (10pt semibold) at the top of the form.
    header: HWND,
    /// The six field labels (one per edit row).
    labels: [HWND; 6],
    name: HWND,
    host: HWND,
    port: HWND,
    user: HWND,
    domain: HWND,
    password: HWND,
    save_pw: HWND,
    btn_save: HWND,
    btn_delete: HWND,
}

/// Per-window state for the hub, attached to the main window via
/// `GWLP_USERDATA`. Lives in a `Box` owned by `ui::run`, so the raw pointer the
/// window procedure holds stays valid for the whole message loop.
struct HubWindow {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// Cached theme brushes returned from the WM_CTLCOLOR* handlers. Created
    /// once in create(), deleted in Drop. A child control keeps the brush it
    /// received until its next CTLCOLOR message, so it must outlive the call.
    brush_bg: HBRUSH,
    brush_panel: HBRUSH,
    /// The active section (highlighted rail button + visible content pane).
    section: Section,
    /// The three rail buttons, indexed by [`Section::index`].
    rail: [HWND; 3],
    /// Per-button mouse-hover state, indexed by [`Section::index`]. Maintained
    /// by the rail-button subclass wndproc via [`WM_APP_RAIL_HOVER`]; read by
    /// `on_draw_item` so a hovered button paints `theme::ROW_HOVER`.
    rail_hover: [bool; 3],
    /// The New Connection form controls (created lazily; the New section shows
    /// them instead of the step-5 placeholder pane).
    form: Option<FormState>,
    /// Per-section list controls (Recent/Saved), created lazily.
    sections: [Option<SectionUi>; 3],
    /// The connection the user activated, returned by `run()` to the connect
    /// bootstrap (step-8). `None` until a Recent/Saved row is activated.
    selected: Option<ConnectionTarget>,
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
                // The window background (#1E1E1E). WM_ERASEBKGND paints the
                // same color over the full client rect; the rail and content
                // containers overpaint it with the panel palette.
                hbrBackground: CreateSolidBrush(theme::BG),
                ..Default::default()
            };
            // Registering an already-registered class fails; one window per
            // process, so ignoring "already exists" is fine (as in connbar.rs).
            let _ = RegisterClassExW(&wc);

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

            // Cached CTLCOLOR brushes: BG answers WM_CTLCOLORSTATIC (labels),
            // PANEL answers WM_CTLCOLOREDIT (dark input fields). Both live for
            // the whole hub lifetime and are freed in Drop.
            let brush_bg = CreateSolidBrush(theme::BG);
            let brush_panel = CreateSolidBrush(theme::PANEL);

            let mut hub = Box::new(HubWindow {
                hwnd,
                hinstance,
                brush_bg,
                brush_panel,
                section: Section::Recent,
                rail: [HWND::default(); 3],
                rail_hover: [false; 3],
                form: None,
                sections: [None, None, None],
                selected: None,
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
    /// Each button is subclassed with [`rail_button_proc`] so mouse enter/leave
    /// is reported back to the hub (via [`WM_APP_RAIL_HOVER`]) for the dark
    /// hover state. Geometry is DPI-scaled: every hit target stays at least
    /// 40 DIPs tall.
    fn create_rail(&mut self) -> Result<(), HubError> {
        unsafe {
            let dpi = self.dpi();
            let btn_h = theme::scale_px(RAIL_BTN_H, dpi);
            let gap = theme::scale_px(RAIL_BTN_GAP, dpi);
            let pad_top = theme::scale_px(RAIL_PAD_TOP, dpi);
            for (i, section) in Section::ALL.iter().enumerate() {
                let y = pad_top + i as i32 * (btn_h + gap);
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
                    btn_h,
                    Some(self.hwnd),
                    Some(HMENU(section.id() as *mut c_void)),
                    Some(self.hinstance),
                    None,
                )
                .map_err(|e| HubError::win32(format!("CreateWindowExW(rail {section:?}): {e}")))?;
                // Chain the stock button proc (kept in GWLP_USERDATA) so clicks,
                // focus, and capture still work; ours only observes the mouse.
                let old_proc = GetWindowLongPtrW(btn, GWLP_WNDPROC);
                SetWindowLongPtrW(btn, GWLP_USERDATA, old_proc);
                SetWindowLongPtrW(btn, GWLP_WNDPROC, rail_button_proc as *const () as isize);
                self.rail[i] = btn;
            }
            Ok(())
        }
    }

    /// Switch the visible content to `section`, creating the section's controls
    /// (list panes or the New Connection form) on first visit. List sections
    /// re-read their store on every switch so new saves / MRU entries appear.
    /// Also re-highlights the rail buttons.
    fn show_section(&mut self, section: Section) -> Result<(), HubError> {
        unsafe {
            // Hide whatever is currently showing: the form controls and every
            // list section's filter/list/edit-button set.
            if let Some(f) = &self.form {
                for c in form_hwnds(f) {
                    let _ = ShowWindow(c, SW_HIDE);
                }
            }
            for s in Section::ALL {
                if let Some(ui) = &self.sections[s.index()] {
                    let _ = ShowWindow(ui.header, SW_HIDE);
                    let _ = ShowWindow(ui.filter, SW_HIDE);
                    let _ = ShowWindow(ui.list, SW_HIDE);
                    if let Some(btn) = ui.edit_btn {
                        let _ = ShowWindow(btn, SW_HIDE);
                    }
                }
            }

            match section {
                Section::Recent | Section::Saved => {
                    self.ensure_section_ui(section)?;
                    self.refresh_section(section)?;
                    self.layout_section(section)?;
                    if let Some(ui) = &self.sections[section.index()] {
                        let _ = ShowWindow(ui.header, SW_SHOW);
                        let _ = ShowWindow(ui.filter, SW_SHOW);
                        let _ = ShowWindow(ui.list, SW_SHOW);
                        if let Some(btn) = ui.edit_btn {
                            let _ = ShowWindow(btn, SW_SHOW);
                        }
                    }
                }
                Section::New => {
                    // The connection form (create, edit, delete).
                    self.ensure_form()?;
                    self.layout_form()?;
                    if let Some(f) = &self.form {
                        for c in form_hwnds(f) {
                            let _ = ShowWindow(c, SW_SHOW);
                        }
                    }
                }
            }
            self.section = section;
            self.repaint_rail();
            // Fonts are fanned out after lazy control creation so late-created
            // panes/forms pick up the cached Segoe UI faces too (idempotent).
            self.apply_fonts();
            tracing::debug!(section = ?section, "hub switched section");
            Ok(())
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

    /// The window's effective DPI (per-monitor V2 aware; 96 when the window is
    /// not DPI aware). Every rail metric is scaled through [`theme::scale_px`]
    /// with this value so hit targets and padding keep their DIP size.
    fn dpi(&self) -> u32 {
        let d = unsafe { GetDpiForWindow(self.hwnd) };
        if d == 0 { 96 } else { d }
    }

    /// Re-layout all children for the current client size (WM_SIZE handler):
    /// the rail buttons always, plus whatever content is active.
    fn layout(&mut self) -> Result<(), HubError> {
        unsafe {
            let dpi = self.dpi();
            let btn_h = theme::scale_px(RAIL_BTN_H, dpi);
            let gap = theme::scale_px(RAIL_BTN_GAP, dpi);
            let pad_top = theme::scale_px(RAIL_PAD_TOP, dpi);
            for (i, btn) in self.rail.iter().enumerate() {
                let y = pad_top + i as i32 * (btn_h + gap);
                let _ = MoveWindow(*btn, RAIL_BTN_X, y, RAIL_BTN_W, btn_h, true);
            }
            match self.section {
                Section::Recent | Section::Saved => self.layout_section(self.section)?,
                Section::New => self.layout_form()?,
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

    /// WM_COMMAND: rail-button clicks (LOWORD = rail id), filter-edit text
    /// changes (HIWORD = EN_CHANGE), and the subclassed filter edit's Enter
    /// (HIWORD = NOTIFY_FILTER_ENTER).
    fn on_command(&mut self, wparam: WPARAM, lparam: LPARAM) -> Result<(), HubError> {
        let id = wparam.0 & 0xffff;
        let notify = ((wparam.0 >> 16) & 0xffff) as u32;

        // Activity rail: switch sections.
        if let Some(section) = Section::from_id(id) {
            return self.show_section(section);
        }

        // Filter edits (Recent/Saved).
        let filter_section = match id {
            ID_FILTER_RECENT => Some(Section::Recent),
            ID_FILTER_SAVED => Some(Section::Saved),
            _ => None,
        };
        if let Some(section) = filter_section {
            match notify {
                // Live narrowing as the user types.
                EN_CHANGE => {
                    let text = read_edit_text(HWND(lparam.0 as *mut c_void));
                    self.set_filter(section, text)?;
                }
                // Enter: connect the top (or selected) matching row.
                NOTIFY_FILTER_ENTER => {
                    let edit = HWND(lparam.0 as *mut c_void);
                    let section = match edit {
                        h if Some(h) == self.sections[Section::Recent.index()].as_ref().map(|u| u.filter) => Section::Recent,
                        h if Some(h) == self.sections[Section::Saved.index()].as_ref().map(|u| u.filter) => Section::Saved,
                        _ => return Ok(()),
                    };
                    self.activate_top(section)?;
                }
                _ => {}
            }
        }

        // New Connection form buttons + the Saved list's "Edit Selected".
        match id {
            ID_FORM_SAVE => return self.on_form_save(),
            ID_FORM_DELETE => return self.on_form_delete(),
            ID_BTN_EDIT_SAVED => return self.on_edit_saved(),
            _ => {}
        }
        Ok(())
    }

    /// WM_NOTIFY: list-view notifications. Activation (NM_DBLCLK /
    /// LVN_ITEMACTIVATE) builds a [`ConnectionTarget`] from the activated row
    /// and hands it to the hub runner (posts WM_QUIT; `run()` returns the
    /// target). NM_CUSTOMDRAW (Recent and Saved lists) paints the rows with
    /// the dark palette and returns the CDRF_* response the list view expects.
    fn on_notify(&mut self, lparam: LPARAM) -> Result<LRESULT, HubError> {
        let ptr = lparam.0 as *const NMHDR;
        if ptr.is_null() {
            return Ok(LRESULT(0));
        }
        let hdr = unsafe { &*ptr };
        // Custom-draw painting for the Recent and Saved list views: the CDRF
        // flags must be returned to the control, so this is handled before
        // activation. Both lists route through the same dark-row handler
        // (paint_list_custom_draw) so the two panes cannot drift.
        if hdr.code == NM_CUSTOMDRAW
            && (hdr.idFrom == ID_LIST_RECENT || hdr.idFrom == ID_LIST_SAVED)
        {
            return Ok(paint_list_custom_draw(lparam));
        }
        if hdr.code == NM_DBLCLK || hdr.code == LVN_ITEMACTIVATE {
            let item = unsafe { &*(lparam.0 as *const NMITEMACTIVATE) };
            let section = match hdr.idFrom {
                ID_LIST_RECENT => Some(Section::Recent),
                ID_LIST_SAVED => Some(Section::Saved),
                _ => None,
            };
            if let Some(section) = section {
                self.activate_row(section, item.iItem)?;
            }
        }
        Ok(LRESULT(0))
    }

    /// Create the filter edit + SysListView32 for a list section on first visit.
    fn ensure_section_ui(&mut self, section: Section) -> Result<(), HubError> {
        if self.sections[section.index()].is_some() {
            return Ok(());
        }
        unsafe {
            let rc = self.content_rect()?;
            let x = rc.left + 16;
            let w = (rc.right - rc.left - 32).max(1);
            // The pane's section title strip sits above the filter; controls
            // start below it (same formula as layout_section).
            let header_y = rc.top + SECTION_HEADER_TOP;
            let content_top = header_y + SECTION_HEADER_H + SECTION_HEADER_GAP;

            // Filter edit. Created without WS_EX_CLIENTEDGE so no stock 3D
            // sunken edge shows; the dark-edit subclass paints a 1px
            // theme::BORDER frame and catches Enter (single-line edits send no
            // Enter notification of their own).
            let filter = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | ES_AUTOHSCROLL as u32),
                x,
                content_top,
                w,
                FILTER_H,
                Some(self.hwnd),
                Some(HMENU(section_filter_id(section) as *mut c_void)),
                Some(self.hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(filter {section:?}): {e}")))?;
            // Chain the original wndproc (kept in GWLP_USERDATA), install the
            // dark-edit subclass, and set the cue-banner placeholder shown
            // while the filter is empty.
            subclass_dark_edit(filter);
            let cue: Vec<u16> = "Filter connections..."
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let _ = SendMessageW(
                filter,
                EM_SETCUEBANNER,
                Some(WPARAM(1)), // show the cue even while the filter has focus
                Some(LPARAM(cue.as_ptr() as isize)),
            );

            // SysListView32 report view. Both the Recent and Saved lists hide
            // their column header (LVS_NOCOLUMNHEADER) and paint dark rows via
            // NM_CUSTOMDRAW (see paint_list_custom_draw), so neither pane can
            // show a stock white header or row background.
            let list_style = (WS_CHILD | WS_VISIBLE).0
                | LVS_REPORT
                | LVS_SINGLESEL
                | LVS_SHOWSELALWAYS
                | LVS_NOCOLUMNHEADER;
            let list = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("SysListView32"),
                w!(""),
                WINDOW_STYLE(list_style),
                x,
                content_top + FILTER_H + 14,
                w,
                (rc.bottom - content_top - FILTER_H - 14 - 16).max(1),
                Some(self.hwnd),
                Some(HMENU(section_list_id(section) as *mut c_void)),
                Some(self.hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(list {section:?}): {e}")))?;
            // Track-select hover highlighting + full-row selection + grid
            // separators; LVS_EX_TRACKSELECT is what feeds CDIS_HOT to both
            // lists' shared custom-draw handler on hover.
            let ext_styles = LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_TRACKSELECT;
            let _ = SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                Some(WPARAM(0)),
                Some(LPARAM(ext_styles as isize)),
            );
            // Dark list backdrop: the list area and its text background both
            // use theme::PANEL so no stock white shows through either list.
            let _ = SendMessageW(
                list,
                LVM_SETBKCOLOR,
                None,
                Some(LPARAM(theme::PANEL.0 as isize)),
            );
            let _ = SendMessageW(
                list,
                LVM_SETTEXTBKCOLOR,
                None,
                Some(LPARAM(theme::PANEL.0 as isize)),
            );

            // Columns: Name, Host, User, Last Used.
            const COLS: [(&str, i32); 4] = [
                ("Name", 200),
                ("Host", 170),
                ("User", 130),
                ("Last Used", 150),
            ];
            for (i, (title, width)) in COLS.iter().enumerate() {
                let mut title_buf: Vec<u16> =
                    title.encode_utf16().chain(std::iter::once(0)).collect();
                let mut col = LVCOLUMNW {
                    mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: LVCOLUMNW_FORMAT(0),
                    cx: *width,
                    pszText: PWSTR(title_buf.as_mut_ptr()),
                    iSubItem: i as i32,
                    ..Default::default()
                };
                let _ = SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    Some(WPARAM(i)),
                    Some(LPARAM(&mut col as *mut LVCOLUMNW as isize)),
                );
            }

            // The Saved section gets an "Edit Selected" button that loads the
            // highlighted row into the New Connection form for editing.
            let edit_btn = if section == Section::Saved {
                Some(create_child(
                    self.hwnd,
                    self.hinstance,
                    "BUTTON",
                    "Edit Selected",
                    WS_CHILD | WS_VISIBLE,
                    ID_BTN_EDIT_SAVED,
                )?)
            } else {
                None
            };
            // Section title static (10pt semibold, set by apply_fonts) above
            // the filter strip.
            let header = create_child(
                self.hwnd,
                self.hinstance,
                "STATIC",
                section.title(),
                WS_CHILD | WS_VISIBLE,
                0,
            )?;

            self.sections[section.index()] = Some(SectionUi {
                header,
                filter,
                list,
                edit_btn,
                filter_text: String::new(),
                rows: Vec::new(),
                filtered: Vec::new(),
            });
            Ok(())
        }
    }

    /// Re-read the store for a list section and rebuild the row set + list.
    /// Called on every section switch so the lists always reflect disk.
    fn refresh_section(&mut self, section: Section) -> Result<(), HubError> {
        let rows = match section {
            Section::Recent => {
                let store = MruStore::load()?;
                store
                    .list_most_recent_first()
                    .iter()
                    .map(|m| ListRow {
                        display_name: m.display_name.clone(),
                        host: m.host.clone(),
                        // Identity half of `format_recent_entry`: user@host
                        // when the record has a username, bare host otherwise.
                        user: format_recent_identity(m.username.as_deref(), &m.host),
                        // Timestamp half of `format_recent_entry`: local short
                        // date + time (GetDateFormatW/GetTimeFormatW).
                        last_used: format_local_short_datetime(m.last_connected_at),
                        port: m.port,
                        username: m.username.clone(),
                        domain: m.domain.clone(),
                        connection_id: m.connection_id,
                    })
                    .collect::<Vec<_>>()
            }
            Section::Saved => {
                let store = ConnectionStore::load()?;
                store
                    .list()
                    .iter()
                    .map(|r| ListRow {
                        display_name: r.display_name.clone(),
                        host: r.host.clone(),
                        user: r.username.clone().unwrap_or_default(),
                        last_used: format_ts(r.updated_at),
                        port: r.port,
                        username: r.username.clone(),
                        domain: r.domain.clone(),
                        connection_id: Some(r.id),
                    })
                    .collect::<Vec<_>>()
            }
            Section::New => Vec::new(),
        };
        self.set_rows(section, rows)
    }

    /// Replace the full row set of a list section and re-apply the filter.
    fn set_rows(&mut self, section: Section, rows: Vec<ListRow>) -> Result<(), HubError> {
        let Some(ui) = self.sections[section.index()].as_mut() else {
            return Ok(());
        };
        ui.rows = rows;
        apply_filter(ui);
        self.populate_list(section)
    }

    /// Set the live filter text and narrow the list.
    fn set_filter(&mut self, section: Section, text: String) -> Result<(), HubError> {
        let Some(ui) = self.sections[section.index()].as_mut() else {
            return Ok(());
        };
        ui.filter_text = text;
        apply_filter(ui);
        self.populate_list(section)
    }

    /// Position the filter edit + list view within the content area.
    fn layout_section(&mut self, section: Section) -> Result<(), HubError> {
        let Some(ui) = self.sections[section.index()].as_ref() else {
            return Ok(());
        };
        let rc = self.content_rect()?;
        let x = rc.left + 16;
        let w = (rc.right - rc.left - 32).max(1);
        let header_y = rc.top + SECTION_HEADER_TOP;
        let content_top = header_y + SECTION_HEADER_H + SECTION_HEADER_GAP;
        let list_top = content_top + FILTER_H + 14;
        let list_h = (rc.bottom - list_top - 16).max(1);
        unsafe {
            let _ = MoveWindow(ui.header, x, header_y, w, SECTION_HEADER_H, true);
            let _ = MoveWindow(ui.filter, x, content_top, w, FILTER_H, true);
            let _ = MoveWindow(ui.list, x, list_top, w, list_h, true);
            if let Some(btn) = ui.edit_btn {
                let _ = MoveWindow(btn, x, list_top + list_h + 10, 140, 28, true);
            }
        }
        Ok(())
    }

    /// Rebuild the SysListView32 contents from `ui.filtered`.
    fn populate_list(&self, section: Section) -> Result<(), HubError> {
        let Some(ui) = self.sections[section.index()].as_ref() else {
            return Ok(());
        };
        unsafe {
            let _ = SendMessageW(
                ui.list,
                LVM_DELETEALLITEMS,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
            for (list_idx, &row_idx) in ui.filtered.iter().enumerate() {
                let row = &ui.rows[row_idx];
                let i = list_idx as i32;

                // Column 0 = Name (LVM_INSERTITEMW creates the row).
                let mut name_buf: Vec<u16> = row
                    .display_name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let mut item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: i,
                    iSubItem: 0,
                    pszText: PWSTR(name_buf.as_mut_ptr()),
                    ..Default::default()
                };
                let _ = SendMessageW(
                    ui.list,
                    LVM_INSERTITEMW,
                    Some(WPARAM(0)),
                    Some(LPARAM(&mut item as *mut LVITEMW as isize)),
                );

                // Columns 1..3 = Host, User, Last Used.
                for (col, text) in [
                    (1, row.host.as_str()),
                    (2, row.user.as_str()),
                    (3, row.last_used.as_str()),
                ] {
                    let mut buf: Vec<u16> =
                        text.encode_utf16().chain(std::iter::once(0)).collect();
                    let mut sub = LVITEMW {
                        mask: LVIF_TEXT,
                        iItem: i,
                        iSubItem: col,
                        pszText: PWSTR(buf.as_mut_ptr()),
                        ..Default::default()
                    };
                    let _ = SendMessageW(
                        ui.list,
                        LVM_SETITEMTEXTW,
                        Some(WPARAM(i as usize)),
                        Some(LPARAM(&mut sub as *mut LVITEMW as isize)),
                    );
                }
            }
        }
        Ok(())
    }

    /// Build a [`ConnectionTarget`] for the activated row and hand it to the
    /// hub runner. The saved password is unprotected only here, at connect
    /// time: Saved rows and MRU rows linked to a saved record get the
    /// plaintext; unlinked MRU rows keep `password` unset so the existing
    /// prompt.rs flow asks.
    fn activate_row(&mut self, section: Section, list_index: i32) -> Result<(), HubError> {
        if list_index < 0 {
            return Ok(());
        }
        // Copy the row out of the borrow first (password resolution below needs
        // &mut self to record the selection).
        let row: Option<ListRow> = {
            let Some(ui) = self.sections[section.index()].as_ref() else {
                return Ok(());
            };
            ui.filtered
                .get(list_index as usize)
                .and_then(|&i| ui.rows.get(i))
                .cloned()
        };
        let Some(row) = row else {
            tracing::debug!(section = ?section, list_index, "activation on empty/out-of-range row");
            return Ok(());
        };

        let mut target = ConnectionTarget {
            display_name: row.display_name.clone(),
            host: row.host.clone(),
            port: row.port,
            username: row.username.clone(),
            domain: row.domain.clone(),
            password: None,
            connection_id: row.connection_id,
        };

        if let Some(id) = target.connection_id {
            match ConnectionStore::load() {
                Ok(store) => match store.get(&id) {
                    Some(rec) => match rec.saved_password.unprotect() {
                        Ok(p) => target.password = Some(p),
                        Err(e) => tracing::warn!(error = %e, %id, "could not unprotect saved password"),
                    },
                    None => tracing::debug!(%id, "no saved record for connection link"),
                },
                Err(e) => tracing::warn!(error = %e, "could not load connections to resolve password"),
            }
        }

        self.selected = Some(target);
        unsafe {
            PostQuitMessage(0);
        }
        tracing::info!(section = ?section, "hub selected a connection target; exiting to connect");
        Ok(())
    }

    /// Connect the top matching row (Enter in the filter): the currently
    /// selected row if any, otherwise the first filtered row.
    fn activate_top(&mut self, section: Section) -> Result<(), HubError> {
        let (list, count) = match self.sections[section.index()].as_ref() {
            Some(ui) => (ui.list, ui.filtered.len()),
            None => return Ok(()),
        };
        let idx = unsafe {
            // LVM_GETNEXTITEM with a -1 start searches from the top.
            let sel = SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            )
            .0;
            if sel >= 0 { sel as usize } else { 0 }
        };
        if idx < count {
            self.activate_row(section, idx as i32)
        } else {
            Ok(())
        }
    }

    // --- New Connection form (step-7) ----------------------------------------

    /// Create the form controls on first visit to the New section.
    fn ensure_form(&mut self) -> Result<(), HubError> {
        if self.form.is_some() {
            return Ok(());
        }
        let h = self.hwnd;
        let inst = self.hinstance;
        let edit_style =
            WINDOW_STYLE((WS_CHILD | WS_VISIBLE | WS_BORDER).0 | ES_AUTOHSCROLL as u32);
        let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
        let btn_style = WS_CHILD | WS_VISIBLE;
        let mk = |class: &'static str, text: &'static str, style: WINDOW_STYLE, id: usize| {
            create_child(h, inst, class, text, style, id)
        };

        // Section-title header, rendered in the 10pt semibold face.
        let header = mk("STATIC", "New Connection", label_style, 0)?;

        let labels = [
            mk("STATIC", "Display name", label_style, 0)?,
            mk("STATIC", "Host", label_style, 0)?,
            mk("STATIC", "Port", label_style, 0)?,
            mk("STATIC", "Username", label_style, 0)?,
            mk("STATIC", "Domain", label_style, 0)?,
            mk("STATIC", "Password", label_style, 0)?,
        ];
        let name = mk("EDIT", "", edit_style, ID_FORM_NAME)?;
        let host = mk("EDIT", "", edit_style, ID_FORM_HOST)?;
        let port = mk("EDIT", "", edit_style, ID_FORM_PORT)?;
        let user = mk("EDIT", "", edit_style, ID_FORM_USER)?;
        let domain = mk("EDIT", "", edit_style, ID_FORM_DOMAIN)?;
        // The password edit masks input; the plaintext only exists in memory
        // inside the edit buffer and is DPAPI-protected before any disk write.
        let password = mk(
            "EDIT",
            "",
            WINDOW_STYLE(edit_style.0 | ES_PASSWORD as u32),
            ID_FORM_PASSWORD,
        )?;
        // Every edit gets the dark-edit subclass: a 1px theme::BORDER frame
        // via WM_NCPAINT, with the dark fill and TEXT color coming from the
        // hub's WM_CTLCOLOREDIT handling.
        for edit in [name, host, port, user, domain, password] {
            // ensure_form is not an unsafe fn, so the subclass call (which
            // pokes GWLP_WNDPROC) goes through an explicit unsafe block.
            unsafe { subclass_dark_edit(edit); }
        }
        let save_pw = mk(
            "BUTTON",
            "Save password (DPAPI-protected)",
            WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | BS_AUTOCHECKBOX as u32),
            ID_FORM_SAVE_PW,
        )?;
        let btn_save = mk("BUTTON", "Save", btn_style, ID_FORM_SAVE)?;
        let btn_delete = mk("BUTTON", "Delete", btn_style, ID_FORM_DELETE)?;

        self.form = Some(FormState {
            editing_id: None,
            header,
            labels,
            name,
            host,
            port,
            user,
            domain,
            password,
            save_pw,
            btn_save,
            btn_delete,
        });
        // Start blank with the save-password box checked.
        self.clear_form();
        Ok(())
    }

    /// Position the form controls in the content area (label column + edit
    /// column, then the checkbox and the Save/Delete buttons).
    fn layout_form(&mut self) -> Result<(), HubError> {
        let Some(f) = self.form.as_ref() else {
            return Ok(());
        };
        let rc = self.content_rect()?;
        let left = rc.left + FORM_X;
        let label_x = left;
        let edit_x = left + FORM_LABEL_W;
        let edit_w = (rc.right - edit_x - FORM_X).max(1);
        let header_y = rc.top + SECTION_HEADER_TOP;
        let mut y = header_y + SECTION_HEADER_H + SECTION_HEADER_GAP;
        unsafe {
            let _ = MoveWindow(
                f.header,
                label_x,
                header_y,
                FORM_LABEL_W + edit_w,
                SECTION_HEADER_H,
                true,
            );
            let rows: [(HWND, HWND, i32); 6] = [
                (f.labels[0], f.name, edit_w),
                (f.labels[1], f.host, edit_w),
                (f.labels[2], f.port, FORM_PORT_W),
                (f.labels[3], f.user, edit_w),
                (f.labels[4], f.domain, edit_w),
                (f.labels[5], f.password, edit_w),
            ];
            for (label, edit, ew) in rows {
                let _ = MoveWindow(label, label_x, y, FORM_LABEL_W, FORM_EDIT_H, true);
                let _ = MoveWindow(edit, edit_x, y, ew, FORM_EDIT_H, true);
                y += FORM_ROW_H;
            }
            let _ = MoveWindow(f.save_pw, edit_x, y, 280, FORM_EDIT_H, true);
            y += FORM_ROW_H;
            let _ = MoveWindow(f.btn_save, edit_x, y, 100, 28, true);
            let _ = MoveWindow(f.btn_delete, edit_x + 112, y, 100, 28, true);
        }
        Ok(())
    }

    /// Read the form, validate, and save via [`ConnectionStore::upsert`] (which
    /// protects the password with DPAPI before anything touches disk). A blank
    /// password on edit preserves the existing blob; unchecking "Save password"
    /// stores none.
    fn on_form_save(&mut self) -> Result<(), HubError> {
        let Some(f) = self.form.as_ref() else {
            return Ok(());
        };
        let name = read_edit_text(f.name);
        let host = read_edit_text(f.host);
        let port_text = read_edit_text(f.port);
        let user = read_edit_text(f.user);
        let domain = read_edit_text(f.domain);
        let password = read_edit_text(f.password);
        let save_pw = unsafe { SendMessageW(f.save_pw, BM_GETCHECK, None, None).0 == BST_CHECKED };
        let editing_id = f.editing_id;

        let host = host.trim().to_string();
        if host.is_empty() {
            show_message(self.hwnd, "Host must not be empty.", "Save Connection");
            return Ok(());
        }
        let port: u16 = match port_text.trim().parse::<u16>() {
            Ok(p) if (1..=65535).contains(&p) => p,
            _ => {
                show_message(
                    self.hwnd,
                    "Port must be a number from 1 to 65535.",
                    "Save Connection",
                );
                return Ok(());
            }
        };

        let input = ConnectionInput {
            id: editing_id,
            display_name: if name.trim().is_empty() {
                host.clone()
            } else {
                name.trim().to_string()
            },
            host: host.clone(),
            port,
            username: non_empty(user),
            domain: non_empty(domain),
            password: if save_pw { Some(password) } else { None },
        };
        let mut store = ConnectionStore::load()?;
        match store.upsert(input) {
            Ok(rec) => {
                tracing::info!(
                    id = %rec.id,
                    host = %rec.host,
                    port,
                    stored_password = save_pw,
                    "saved connection"
                );
                self.clear_form();
                if self.sections[Section::Saved.index()].is_some() {
                    self.refresh_section(Section::Saved)?;
                }
            }
            Err(e) => tracing::error!(error = %e, "could not save connection"),
        }
        Ok(())
    }

    /// Delete the record loaded in the form, after a MessageBoxW confirmation.
    fn on_form_delete(&mut self) -> Result<(), HubError> {
        let Some(f) = self.form.as_ref() else {
            return Ok(());
        };
        let Some(id) = f.editing_id else {
            tracing::debug!("delete pressed with no record loaded");
            return Ok(());
        };
        let host = read_edit_text(f.host);
        let display = if host.trim().is_empty() {
            "(unnamed connection)"
        } else {
            host.trim()
        };
        let question = format!("Delete the saved connection '{display}'?");
        let confirmed = unsafe {
            let q_w: Vec<u16> = question.encode_utf16().chain(std::iter::once(0)).collect();
            MessageBoxW(
                Some(self.hwnd),
                PCWSTR(q_w.as_ptr()),
                w!("Delete Connection"),
                MB_YESNO | MB_ICONWARNING,
            ) == IDYES
        };
        if !confirmed {
            tracing::debug!(%id, "delete cancelled");
            return Ok(());
        }
        let mut store = ConnectionStore::load()?;
        match store.delete(&id) {
            Ok(true) => {
                tracing::info!(%id, "deleted saved connection");
                self.clear_form();
                if self.sections[Section::Saved.index()].is_some() {
                    self.refresh_section(Section::Saved)?;
                }
            }
            Ok(false) => tracing::warn!(%id, "delete: no such record"),
            Err(e) => tracing::error!(error = %e, %id, "delete failed"),
        }
        Ok(())
    }

    /// Load the selected Saved row into the edit form, then switch to New.
    fn on_edit_saved(&mut self) -> Result<(), HubError> {
        let row = {
            let Some(ui) = self.sections[Section::Saved.index()].as_ref() else {
                return Ok(());
            };
            let sel = unsafe {
                SendMessageW(
                    ui.list,
                    LVM_GETNEXTITEM,
                    Some(WPARAM(usize::MAX)),
                    Some(LPARAM(LVNI_SELECTED as isize)),
                )
                .0
            };
            if sel < 0 {
                return Ok(());
            }
            ui.filtered
                .get(sel as usize)
                .and_then(|&i| ui.rows.get(i))
                .cloned()
        };
        let Some(row) = row else {
            return Ok(());
        };
        let Some(id) = row.connection_id else {
            tracing::warn!("selected row has no saved record to edit");
            return Ok(());
        };
        let store = ConnectionStore::load()?;
        let Some(rec) = store.get(&id).cloned() else {
            tracing::warn!(%id, "saved record for selected row no longer exists");
            return Ok(());
        };
        self.load_into_form(&rec);
        self.show_section(Section::New)
    }

    /// Fill the form from a saved record. The password field is left blank so
    /// an untouched save preserves the existing DPAPI blob (store semantics).
    fn load_into_form(&mut self, rec: &ConnectionRecord) {
        let Some(f) = self.form.as_mut() else {
            return;
        };
        set_edit_text(f.name, &rec.display_name);
        set_edit_text(f.host, &rec.host);
        set_edit_text(f.port, &rec.port.to_string());
        set_edit_text(f.user, rec.username.as_deref().unwrap_or(""));
        set_edit_text(f.domain, rec.domain.as_deref().unwrap_or(""));
        set_edit_text(f.password, "");
        unsafe {
            let _ = SendMessageW(f.save_pw, BM_SETCHECK, Some(WPARAM(1)), None);
        }
        f.editing_id = Some(rec.id);
        tracing::info!(id = %rec.id, host = %rec.host, "loaded saved connection into edit form");
    }

    /// Reset the form to a blank "new connection" state. The Port edit is set
    /// to the default RDP port (3389) every time a fresh blank form is opened;
    /// if it already reads "3389" it is left unchanged (no redundant
    /// SetWindowTextW). Saved-connection loads go through `load_into_form`,
    /// which sets the port from the record and is not affected.
    fn clear_form(&mut self) {
        let Some(f) = self.form.as_mut() else {
            return;
        };
        set_edit_text(f.name, "");
        set_edit_text(f.host, "");
        set_edit_text(f.user, "");
        set_edit_text(f.domain, "");
        set_edit_text(f.password, "");
        // Port default: a fresh blank form always shows 3389. Read first so an
        // edit that already contains exactly "3389" is left untouched.
        let port_text = read_edit_text(f.port);
        if port_text != FORM_DEFAULT_PORT {
            set_edit_text(f.port, FORM_DEFAULT_PORT);
        }
        unsafe {
            let _ = SendMessageW(f.save_pw, BM_SETCHECK, Some(WPARAM(1)), None);
        }
        f.editing_id = None;
        // Username default: seed a fresh form with the most recently used
        // non-empty username from MRU history; when history has none, fall
        // back to the current Windows user (GetUserNameW). The field stays a
        // normal editable/clearable edit — this only seeds it. Domain stays
        // blank. Saved-connection loads go through `load_into_form`, which
        // sets the username from the record and is not affected.
        self.prefill_username_field();
    }

    /// Prefill the New Connection form's Username field on a fresh form.
    ///
    /// Rule (username-default): MRU wins — the most recently used non-empty
    /// username from history; empty/blank MRU usernames are skipped. When MRU
    /// yields nothing, the current Windows user (`GetUserNameW`) is the final
    /// fallback. Only when both are unavailable does the field stay blank. The
    /// tested pure rule in `model::prefill_username` implements the fallback
    /// chain; this method feeds it from the live store and the Win32 API.
    /// Read-only with respect to the stores: MRU ordering/cap/dedupe are never
    /// touched, and errors are logged with `tracing`, never panicked on.
    fn prefill_username_field(&mut self) {
        let Some(f) = self.form.as_mut() else {
            return;
        };
        let last_used = match MruStore::load() {
            Ok(store) => store.last_used_username().map(str::to_owned),
            Err(e) => {
                tracing::warn!(error = %e, "could not load MRU for username prefill");
                None
            }
        };
        let current_user = current_windows_username();
        let username = prefill_username(last_used.as_deref(), current_user.as_deref());
        set_edit_text(f.user, username.as_deref().unwrap_or(""));
    }

    /// WM_DRAWITEM: paint the owner-draw rail buttons flat dark. Resting
    /// buttons fill with the window background (`theme::BG`), hovered ones lift
    /// to `theme::ROW_HOVER`, and the active section uses the lighter
    /// `theme::ACTIVE_BG` plus a 2px `theme::ACCENT` bar on its left edge. Rail
    /// text is `theme::TEXT` for active/hovered items and `theme::MUTED` for
    /// inactive ones. No stock brushes or `DrawFrameControl` are used.
    fn on_draw_item(&self, lparam: LPARAM) -> Result<(), HubError> {
        let ptr = lparam.0 as *const DRAWITEMSTRUCT;
        if ptr.is_null() {
            return Ok(());
        }
        let dis = unsafe { &*ptr };
        if dis.CtlType != ODT_BUTTON {
            return Ok(());
        }
        // Non-rail hub buttons are owner-drawn here (buttons-dark): the
        // primary Connect button is accent-filled, secondary buttons (Save /
        // Delete / Edit Selected) are dark panel faces with a BORDER outline,
        // and the Save-password checkbox gets a dark glyph + label. Rail
        // buttons fall through to the rail painter below.
        let dpi = self.dpi();
        let pressed = (dis.itemState.0 & windows::Win32::UI::Controls::ODS_SELECTED.0) != 0;
        match dis.CtlID as usize {
            ID_FORM_CONNECT => {
                draw_primary_button(dis, dpi, button_draw_hovered(dis.hwndItem), pressed);
                return Ok(());
            }
            ID_FORM_SAVE | ID_FORM_DELETE | ID_BTN_EDIT_SAVED => {
                draw_secondary_button(dis, dpi, button_draw_hovered(dis.hwndItem), pressed);
                return Ok(());
            }
            ID_FORM_SAVE_PW => {
                draw_checkbox(dis, dpi, button_draw_hovered(dis.hwndItem), pressed);
                return Ok(());
            }
            _ => {}
        }
        let Some(section) = Section::from_id(dis.CtlID as usize) else {
            return Ok(());
        };

        let idx = section.index();
        let active = section == self.section;
        let hovered = self.rail_hover[idx];
        let dpi = self.dpi();

        // Priority: the active section keeps its highlight; otherwise hover
        // lifts the button toward the row-hover tint; resting is the dark
        // window background so the rail reads as flat dark between buttons.
        let fill = if active {
            theme::ACTIVE_BG
        } else if hovered {
            theme::ROW_HOVER
        } else {
            theme::BG
        };
        let text = if active || hovered {
            theme::TEXT
        } else {
            theme::MUTED
        };

        unsafe {
            let brush = CreateSolidBrush(fill);
            let _ = FillRect(dis.hDC, &dis.rcItem, brush);
            let _ = DeleteObject(brush.into());

            // 2px accent bar on the active section's left edge, DIP-scaled.
            if active {
                let bar_w = theme::scale_px(RAIL_ACCENT_BAR_W, dpi);
                let mut bar = dis.rcItem;
                bar.right = bar.left + bar_w;
                let accent = CreateSolidBrush(theme::ACCENT);
                let _ = FillRect(dis.hDC, &bar, accent);
                let _ = DeleteObject(accent.into());
            }

            let _ = SetBkMode(dis.hDC, TRANSPARENT);
            let _ = SetTextColor(dis.hDC, text);
            // Owner-drawn buttons do not repaint with WM_SETFONT; select the
            // control's font (set by apply_fonts) into the DC so the label
            // renders in Segoe UI rather than the default GUI font.
            let font = SendMessageW(dis.hwndItem, WM_GETFONT, None, None).0 as *mut c_void;
            if !font.is_null() {
                let _ = SelectObject(dis.hDC, HGDIOBJ(font));
            }
            let mut text: Vec<u16> = section
                .title()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut rc = dis.rcItem;
            if active {
                // Keep the label optically centered on the area right of the
                // accent bar rather than under it.
                rc.left += theme::scale_px(RAIL_ACCENT_BAR_W, dpi);
            }
            let _ =
                DrawTextW(dis.hDC, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        Ok(())
    }

    /// WM_APP_RAIL_HOVER: a rail button's subclass reports a mouse enter/leave
    /// (wParam = rail index, lParam = 1 entered / 0 left). When the hover state
    /// actually changed, repaint the affected button so WM_DRAWITEM picks up
    /// the `theme::ROW_HOVER` fill / `theme::TEXT` color.
    fn on_rail_hover(&mut self, wparam: WPARAM, lparam: LPARAM) {
        let idx = wparam.0;
        if idx >= 3 {
            return;
        }
        let hovered = lparam.0 != 0;
        if self.rail_hover[idx] == hovered {
            return;
        }
        self.rail_hover[idx] = hovered;
        unsafe {
            if let Some(btn) = self.rail.get(idx) {
                let _ = InvalidateRect(Some(*btn), None, true);
            }
        }
        tracing::debug!(idx, hovered, "rail hover changed");
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

    /// WM_CTLCOLOREDIT / WM_CTLCOLORSTATIC / WM_CTLCOLORBTN: paint the form and
    /// filter controls with the dark palette. Edit controls (the New Connection
    /// fields and the Recent/Saved filter boxes) get `theme::TEXT` on a
    /// `theme::PANEL` background; static labels get `theme::MUTED` on
    /// `theme::BG`; the save-password checkbox gets `theme::TEXT` on
    /// `theme::BG`. Returns the cached brush matching the background — the
    /// control keeps it until its next CTLCOLOR message. No stock
    /// `COLOR_WINDOW` / `COLOR_BTNFACE` brush is ever used.
    fn on_ctl_color(&self, msg: u32, wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
        let hdc = HDC(wparam.0 as *mut c_void);
        unsafe {
            match msg {
                WM_CTLCOLOREDIT => {
                    let _ = SetTextColor(hdc, theme::TEXT);
                    let _ = SetBkColor(hdc, theme::PANEL);
                    LRESULT(self.brush_panel.0 as isize)
                }
                WM_CTLCOLORSTATIC => {
                    let _ = SetTextColor(hdc, theme::MUTED);
                    let _ = SetBkColor(hdc, theme::BG);
                    LRESULT(self.brush_bg.0 as isize)
                }
                WM_CTLCOLORBTN => {
                    let _ = SetTextColor(hdc, theme::TEXT);
                    let _ = SetBkColor(hdc, theme::BG);
                    LRESULT(self.brush_bg.0 as isize)
                }
                _ => LRESULT(0),
            }
        }
    }
}

impl Drop for HubWindow {
    fn drop(&mut self) {
        unsafe {
            // On the normal close path WM_DESTROY already ran (the window and
            // every child are gone); only destroy explicitly when create()
            // failed partway and the Box is dropped with the window still
            // alive. DestroyWindow tears down all children (rail buttons, list
            // views, filter edits, the form) with the parent.
            if !self.destroyed {
                let _ = DestroyWindow(self.hwnd);
            }
            // Free the cached CTLCOLOR brushes. Once the window (and every
            // child) is gone no control can still reference them.
            let _ = DeleteObject(self.brush_bg.into());
            let _ = DeleteObject(self.brush_panel.into());
        }
    }
}

/// Shared NM_CUSTOMDRAW row-painting logic for the Recent and Saved
/// `SysListView32` panes. Returns the background [`COLORREF`] a row should be
/// painted with: `PANEL` with a `ROW_ALT` zebra tint on odd rows, lifted to
/// `ROW_HOVER` while the row is hot (track-select lights the row under the
/// mouse) and to `ROW_SELECT` (accent tint) for the selected row. Hover wins
/// over selection, matching the original Recent-list behavior; both list panes
/// call this same helper so their row rendering cannot drift.
fn list_row_background(item: usize, state: NMCUSTOMDRAW_DRAW_STATE_FLAGS) -> COLORREF {
    let mut bg = if item % 2 == 1 {
        theme::ROW_ALT
    } else {
        theme::PANEL
    };
    if state.contains(CDIS_HOT) {
        bg = theme::ROW_HOVER;
    } else if state.contains(CDIS_SELECTED) {
        bg = theme::ROW_SELECT;
    }
    bg
}

/// NM_CUSTOMDRAW handler shared by the Recent and Saved `SysListView32` panes:
/// dark rows on the theme palette. At `CDDS_PREPAINT` we opt into per-item
/// drawing (`CDRF_NOTIFYITEMDRAW`); at `CDDS_ITEMPREPAINT` we set
/// `NMLVCUSTOMDRAW::clrText` / `clrTextBk` from [`list_row_background`] — base
/// `TEXT` on `PANEL` — then let the control draw with those colors
/// (`CDRF_DODEFAULT`). Returns the `CDRF_*` flags the list view expects; never
/// unwraps or panics on the paint path.
fn paint_list_custom_draw(lparam: LPARAM) -> LRESULT {
    let ptr = lparam.0 as *mut NMLVCUSTOMDRAW;
    if ptr.is_null() {
        return LRESULT(CDRF_DODEFAULT as isize);
    }
    // Copy the fields we need out of the notification before writing the
    // colors back through the raw pointer (no overlapping borrows).
    let (stage, item, state) = unsafe {
        let cd = &*ptr;
        (cd.nmcd.dwDrawStage, cd.nmcd.dwItemSpec, cd.nmcd.uItemState)
    };
    if stage == CDDS_PREPAINT {
        // Ask for per-item draw notifications.
        return LRESULT(CDRF_NOTIFYITEMDRAW as isize);
    }
    if stage == CDDS_ITEMPREPAINT {
        // Write the colors back into the notification struct; the list view
        // paints the row text with them.
        unsafe {
            (*ptr).clrText = theme::TEXT;
            (*ptr).clrTextBk = list_row_background(item, state);
        }
        return LRESULT(CDRF_DODEFAULT as isize);
    }
    LRESULT(CDRF_DODEFAULT as isize)
}

/// Control id of the filter edit for a section (0 for the New placeholder).
fn section_filter_id(section: Section) -> usize {
    match section {
        Section::Recent => ID_FILTER_RECENT,
        Section::Saved => ID_FILTER_SAVED,
        Section::New => 0,
    }
}

/// Control id of the list view for a section (0 for the New placeholder).
fn section_list_id(section: Section) -> usize {
    match section {
        Section::Recent => ID_LIST_RECENT,
        Section::Saved => ID_LIST_SAVED,
        Section::New => 0,
    }
}

// --- buttons-dark: owner-drawn form/action buttons --------------------------
// The activity-rail buttons are owner-drawn and hover-tracked by the rail
// step; every other hub button (Save / Delete / Connect / "Edit Selected" /
// the Save-password checkbox) is owner-drawn here with the same dark palette:
// a BORDER-outlined PANEL face (hover -> ROW_HOVER, press -> darker) for the
// secondary buttons, an ACCENT-filled primary Connect button, and a dark glyph
// checkbox. Hover is tracked by subclassing the buttons (hub_button_proc) with
// TrackMouseEvent; the shared flag below is read by the WM_DRAWITEM painter
// and cross-checked against the real cursor so a hidden/re-shown button can
// never paint a stale highlight.

/// Process-global hover tracker for the owner-drawn form/action buttons, as
/// `(hwnd, drawn_hovered)`. `drawn_hovered` records what WM_DRAWITEM last
/// painted so the button subclass only invalidates on real visual transitions.
/// The raw HWND is stored as `usize` so the mutex stays `Send`/`Sync` for the
/// `static`. One hub per process, so a single slot is enough.
static BTN_HOVER: std::sync::Mutex<Option<(usize, bool)>> = std::sync::Mutex::new(None);

/// WM_DRAWITEM side of the hover tracker: returns whether `btn` should paint
/// its hovered face right now — the tracked flag AND the cursor actually being
/// over the button (belt-and-suspenders against stale state after a hidden
/// button is re-shown, which never receives `WM_MOUSELEAVE`). Records the
/// outcome so the subclass can dedupe repaints.
fn button_draw_hovered(btn: HWND) -> bool {
    let tracked = match BTN_HOVER.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    };
    let tracked_here = tracked.map(|(h, _)| h == btn.0 as usize).unwrap_or(false);
    let hovered = if tracked_here {
        unsafe {
            let mut pt = windows::Win32::Foundation::POINT::default();
            if GetCursorPos(&mut pt).is_err() {
                false
            } else {
                WindowFromPoint(pt) == btn
            }
        }
    } else {
        false
    };
    let mut guard = match BTN_HOVER.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *guard = Some((btn.0 as usize, hovered));
    hovered
}

/// Subclass side of the hover tracker: record that the cursor entered/left
/// `btn` and invalidate only when the painted face actually changes. Entering
/// always repaints when the button was not already drawn hovered (covers the
/// hidden-then-re-shown case); leaving repaints only when the hover face was
/// actually up.
fn button_hover_changed(btn: HWND, hovered: bool) {
    let mut guard = match BTN_HOVER.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let cur = *guard;
    let matches = cur.map(|(h, _)| h == btn.0 as usize).unwrap_or(false);
    let drawn = if matches { cur.unwrap().1 } else { false };
    let repaint = if hovered {
        !matches || !drawn
    } else {
        matches && drawn
    };
    *guard = if hovered {
        Some((btn.0 as usize, drawn))
    } else if matches {
        None
    } else {
        cur
    };
    drop(guard);
    if repaint {
        unsafe {
            let _ = InvalidateRect(Some(btn), None, true);
        }
    }
}

/// Fill `rc` with a solid `color` brush (create, fill, delete).
fn paint_solid_rect(hdc: HDC, rc: &RECT, color: windows::Win32::Foundation::COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let _ = FillRect(hdc, rc, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// Draw `text` into `rc` on `hdc` with the cached Segoe UI face for `dpi`,
/// transparent background, `color` text, and `align` plus single-line vertical
/// centering. Restores the previously selected font afterwards, so every
/// owner-drawn button label renders in the theme font at the window DPI.
fn draw_button_label(
    hdc: HDC,
    rc: &RECT,
    dpi: u32,
    color: windows::Win32::Foundation::COLORREF,
    align: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    text: &str,
) {
    unsafe {
        let font = theme::ui_font(dpi);
        let old = windows::Win32::Graphics::Gdi::SelectObject(
            hdc,
            windows::Win32::Graphics::Gdi::HGDIOBJ::from(font),
        );
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, color);
        let mut buf: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut trc = *rc;
        let _ = DrawTextW(
            hdc,
            &mut buf,
            &mut trc,
            align | DT_SINGLELINE | DT_VCENTER | windows::Win32::Graphics::Gdi::DT_NOPREFIX,
        );
        let _ = windows::Win32::Graphics::Gdi::SelectObject(hdc, old);
    }
}

/// Paint the primary Connect button: a filled `theme::ACCENT` rectangle with
/// white (`theme::ON_ACCENT`) text. Hover lifts the accent slightly, pressing
/// darkens it toward the background so the click reads.
fn draw_primary_button(dis: &DRAWITEMSTRUCT, dpi: u32, hovered: bool, pressed: bool) {
    let fill = if pressed {
        theme::color_mix(theme::ACCENT, theme::BG, 25)
    } else if hovered {
        theme::lighten(theme::ACCENT, 10)
    } else {
        theme::ACCENT
    };
    fill_rect(dis.hDC, &dis.rcItem, fill);
    let label = read_edit_text(dis.hwndItem);
    draw_button_label(dis.hDC, &dis.rcItem, dpi, theme::ON_ACCENT, DT_CENTER, &label);
}

/// Paint a secondary hub button (Save / Delete / Edit Selected): a dark
/// `theme::PANEL` face (hover -> `theme::ROW_HOVER`, press -> darker) framed
/// by a DPI-scaled 1px `theme::BORDER` outline, with a `theme::TEXT` label.
/// Border width and text padding are scaled through [`theme::scale_px`].
fn draw_secondary_button(dis: &DRAWITEMSTRUCT, dpi: u32, hovered: bool, pressed: bool) {
    let fill = if pressed {
        theme::color_mix(theme::PANEL, theme::BG, 40)
    } else if hovered {
        theme::ROW_HOVER
    } else {
        theme::PANEL
    };
    // Frame: fill the outer rect with the outline color, then the interior
    // with the fill color, so the border is exactly `border` px at any DPI.
    let border = theme::scale_px(1, dpi).max(1);
    fill_rect(dis.hDC, &dis.rcItem, theme::BORDER);
    let mut inner = dis.rcItem;
    inner.left += border;
    inner.top += border;
    inner.right -= border;
    inner.bottom -= border;
    fill_rect(dis.hDC, &inner, fill);
    let label = read_edit_text(dis.hwndItem);
    let pad = theme::scale_px(10, dpi).max(4);
    let mut trc = inner;
    trc.left += pad;
    trc.right -= pad;
    draw_button_label(dis.hDC, &trc, dpi, theme::TEXT, DT_CENTER, &label);
}

/// Paint the Save-password checkbox: a BORDER-framed dark glyph square
/// (ACCENT-filled with a white check when checked, `ROW_HOVER` on hover) and
/// the label in `theme::TEXT` to its right. The check state comes from
/// `ODS_CHECKED`, which the system maintains from the BS_AUTOCHECKBOX state.
fn draw_checkbox(dis: &DRAWITEMSTRUCT, dpi: u32, hovered: bool, pressed: bool) {
    let checked = (dis.itemState.0 & windows::Win32::UI::Controls::ODS_CHECKED.0) != 0;
    let pad = theme::scale_px(6, dpi).max(3);
    let gap = theme::scale_px(8, dpi).max(4);
    let size = theme::scale_px(14, dpi).max(12);
    let cy = (dis.rcItem.top + dis.rcItem.bottom) / 2;
    let box_rc = RECT {
        left: dis.rcItem.left + pad,
        top: cy - size / 2,
        right: dis.rcItem.left + pad + size,
        bottom: cy - size / 2 + size,
    };
    let fill = if pressed {
        theme::color_mix(theme::PANEL, theme::BG, 40)
    } else if checked {
        theme::ACCENT
    } else if hovered {
        theme::ROW_HOVER
    } else {
        theme::PANEL
    };
    let border = theme::scale_px(1, dpi).max(1);
    fill_rect(dis.hDC, &box_rc, theme::BORDER);
    let mut inner = box_rc;
    inner.left += border;
    inner.top += border;
    inner.right -= border;
    inner.bottom -= border;
    fill_rect(dis.hDC, &inner, fill);
    if checked {
        // White check glyph (U+2713) centered in the box, in the theme font.
        unsafe {
            let font = theme::ui_font(dpi);
            let old = windows::Win32::Graphics::Gdi::SelectObject(
                dis.hDC,
                windows::Win32::Graphics::Gdi::HGDIOBJ::from(font),
            );
            let _ = SetBkMode(dis.hDC, TRANSPARENT);
            let _ = SetTextColor(dis.hDC, theme::ON_ACCENT);
            let mut mark: Vec<u16> =
                "\u{2713}".encode_utf16().chain(std::iter::once(0)).collect();
            let mut mrc = inner;
            let _ = DrawTextW(dis.hDC, &mut mark, &mut mrc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            let _ = windows::Win32::Graphics::Gdi::SelectObject(dis.hDC, old);
        }
    }
    let label = read_edit_text(dis.hwndItem);
    if !label.is_empty() {
        let trc = RECT {
            left: box_rc.right + gap,
            top: dis.rcItem.top,
            right: dis.rcItem.right - pad,
            bottom: dis.rcItem.bottom,
        };
        paint_button_label(
            dis.hDC,
            &trc,
            dpi,
            theme::TEXT,
            windows::Win32::Graphics::Gdi::DT_LEFT,
            &label,
        );
    }
}

/// Install [`hub_button_proc`] on an owner-drawn hub button, chaining the
/// stock button proc (kept in `GWLP_USERDATA`) so clicks, focus, and the
/// auto-checkbox toggle keep working; ours only observes the mouse and paints
/// a dark erase background.
fn subclass_hub_button(btn: HWND) {
    unsafe {
        let old_proc = GetWindowLongPtrW(btn, GWLP_WNDPROC);
        SetWindowLongPtrW(btn, GWLP_USERDATA, old_proc);
        SetWindowLongPtrW(btn, GWLP_WNDPROC, hub_button_proc as *const () as isize);
    }
}

/// Subclass wndproc for the owner-drawn form/action buttons (Save, Delete,
/// Connect, Edit Selected, the Save-password checkbox): arms
/// `TrackMouseEvent` leave-tracking and flips the shared hover flag on
/// `WM_MOUSEMOVE` / `WM_MOUSELEAVE` (which repaints only on real visual
/// transitions), and answers `WM_ERASEBKGND` with the dark panel fill so no
/// stock `COLOR_BTNFACE` flash shows before `WM_DRAWITEM` paints. Everything
/// else chains to the stock button proc. Never unwraps or panics.
unsafe extern "system" fn hub_button_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            button_hover_changed(hwnd, true);
        }
        WM_MOUSELEAVE => {
            button_hover_changed(hwnd, false);
            return LRESULT(0);
        }
        WM_ERASEBKGND => {
            // Dark resting fill; WM_DRAWITEM overpaints hover/press states.
            let hdc = HDC(wparam.0 as *mut c_void);
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() && rc.right > rc.left && rc.bottom > rc.top {
                paint_solid_rect(hdc, &rc, theme::PANEL);
            }
            return LRESULT(1);
        }
        _ => {}
    }
    let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    CallWindowProcW(old_proc, hwnd, msg, wparam, lparam)
}

/// Read an edit control's text (UTF-16, truncated at the first NUL).
fn read_edit_text(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n.min(buf.len())])
    }
}

/// Recompute `filtered` from `rows` + `filter_text`: a case-insensitive
/// substring match on display name, host, or username. An empty filter keeps
/// every row.
fn apply_filter(ui: &mut SectionUi) {
    let needle = ui.filter_text.to_lowercase();
    ui.filtered = ui
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            needle.is_empty()
                || r.display_name.to_lowercase().contains(&needle)
                || r.host.to_lowercase().contains(&needle)
                || r.user.to_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
        .collect();
}

/// Create a child control of the hub window (labels, edits, buttons,
/// checkboxes). System classes ("STATIC"/"EDIT"/"BUTTON") need no registration.
fn create_child(
    parent: HWND,
    inst: HINSTANCE,
    class: &'static str,
    text: &'static str,
    style: WINDOW_STYLE,
    id: usize,
) -> Result<HWND, HubError> {
    unsafe {
        let class_w: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
        let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(id as *mut c_void)),
            Some(inst),
            None,
        )
        .map_err(|e| HubError::win32(format!("CreateWindowExW({class}): {e}")))
    }
}

/// Set a control's text (edits, labels, buttons, checkbox).
fn set_edit_text(hwnd: HWND, text: &str) {
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
    }
}

/// The current Windows user name (`GetUserNameW`), or `None` when the call
/// fails or yields a blank value.
///
/// This is the final fallback for the New Connection form's username prefill
/// when MRU history has no usable username: MRU wins, the current Windows user
/// is the last resort, and the field stays blank only when both are
/// unavailable. The two-call pattern keeps it robust for any name length —
/// `GetUserNameW` fails with `ERROR_INSUFFICIENT_BUFFER` and writes the
/// required size (in `u16`s, including the NUL) into `*pcbbuffer`, so we
/// retry with the exact allocation instead of trusting a fixed cap.
fn current_windows_username() -> Option<String> {
    unsafe {
        // Windows logon names are capped at UNLEN (256) characters, so 256 is
        // the documented safe starting size.
        let mut cap = 256u32;
        loop {
            let mut buf = vec![0u16; cap as usize];
            let mut len = buf.len() as u32;
            match GetUserNameW(Some(PWSTR(buf.as_mut_ptr())), &mut len) {
                Ok(()) => {
                    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    let name = String::from_utf16(&buf[..end]).ok()?;
                    let name = name.trim();
                    return (!name.is_empty()).then(|| name.to_string());
                }
                Err(e) => {
                    if e.code() == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) && len > cap {
                        cap = len;
                        continue;
                    }
                    tracing::warn!(error = %e, "GetUserNameW failed; leaving username blank");
                    return None;
                }
            }
        }
    }
}



/// Trim `s`; `None` when empty (for the optional username/domain fields).
fn non_empty(s: String) -> Option<String> {
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Every form control hwnd, for the show/hide pass on section switches.
fn form_hwnds(f: &FormState) -> [HWND; 16] {
    [
        f.header,
        f.labels[0],
        f.labels[1],
        f.labels[2],
        f.labels[3],
        f.labels[4],
        f.labels[5],
        f.name,
        f.host,
        f.port,
        f.user,
        f.domain,
        f.password,
        f.save_pw,
        f.btn_save,
        f.btn_delete,
    ]
}

/// A small modal warning box (validation failures).
fn show_message(parent: HWND, text: &str, caption: &str) {
    unsafe {
        let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let cap_w: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = MessageBoxW(
            Some(parent),
            PCWSTR(text_w.as_ptr()),
            PCWSTR(cap_w.as_ptr()),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// True when `hwnd` is one of the two list-section filter edits (which connect
/// the top match on Enter). Form edits return false so Enter stays inert there.
fn is_filter_edit(hwnd: HWND) -> bool {
    unsafe {
        match GetDlgCtrlID(hwnd) as usize {
            ID_FILTER_RECENT | ID_FILTER_SAVED => true,
            _ => false,
        }
    }
}

/// Install the dark-edit subclass on `edit`: the original class proc is kept
/// in `GWLP_USERDATA` and `GWLP_WNDPROC` becomes [`dark_edit_proc`], which
/// paints a 1px `theme::BORDER` frame on `WM_NCPAINT` (the edit is created
/// without `WS_BORDER` / `WS_EX_CLIENTEDGE`, so no stock sunken edge shows)
/// and reports Enter for the filter edits. Every hub edit goes through this so
/// no stock white or 3D background ever appears.
unsafe fn subclass_dark_edit(edit: HWND) {
    let old_proc = GetWindowLongPtrW(edit, GWLP_WNDPROC);
    SetWindowLongPtrW(edit, GWLP_USERDATA, old_proc);
    SetWindowLongPtrW(edit, GWLP_WNDPROC, dark_edit_proc as *const () as isize);
}

/// Subclass wndproc for every hub edit control: the Recent/Saved filter edits
/// and the six New Connection form fields. On `WM_NCPAINT` it paints a 1px
/// `theme::BORDER` frame around the whole window; for the filter edits it
/// intercepts Enter (connect the top match). Everything else chains to the
/// original class proc kept in `GWLP_USERDATA`. Never unwraps or panics.
unsafe extern "system" fn dark_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCPAINT => {
            // Run the standard non-client handling first (a no-op for our
            // border-less edits), then paint the dark frame over the edge.
            let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            let old_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
            let _ = CallWindowProcW(old_proc, hwnd, msg, wparam, lparam);
            paint_dark_edit_border(hwnd);
            return LRESULT(0);
        }
        WM_KEYDOWN if (wparam.0 & 0xffff) as u16 == VK_RETURN => {
            if is_filter_edit(hwnd) {
                // Tell the hub (the edit's parent) the user pressed Enter; it
                // connects the top filtered row. The edit's HWND rides along
                // in lParam.
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(
                        Some(parent),
                        WM_COMMAND,
                        WPARAM((NOTIFY_FILTER_ENTER as usize) << 16),
                        LPARAM(hwnd.0 as isize),
                    );
                }
                return LRESULT(0);
            }
            // Not a filter edit: fall through to the default edit behavior
            // (the hub has no default button, so Enter does nothing).
        }
        _ => {}
    }
    let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    CallWindowProcW(old_proc, hwnd, msg, wparam, lparam)
}

/// Paint a 1px `theme::BORDER` frame around the whole window (client +
/// non-client) of an edit control. A window DC has its origin at the window's
/// top-left corner, so the frame rect is (0,0,width,height) — the exact outer
/// edge. Logs on failure rather than panicking.
unsafe fn paint_dark_edit_border(hwnd: HWND) {
    let hdc = GetWindowDC(Some(hwnd));
    if hdc.is_invalid() {
        tracing::warn!("paint_dark_edit_border: GetWindowDC failed");
        return;
    }
    let mut rc = RECT::default();
    if let Err(e) = GetWindowRect(hwnd, &mut rc) {
        tracing::warn!(error = %e, "paint_dark_edit_border: GetWindowRect failed");
        let _ = ReleaseDC(Some(hwnd), hdc);
        return;
    }
    let frame = RECT {
        left: 0,
        top: 0,
        right: rc.right - rc.left,
        bottom: rc.bottom - rc.top,
    };
    let brush = CreateSolidBrush(theme::BORDER);
    let _ = FrameRect(hdc, &frame, brush);
    let _ = DeleteObject(brush.into());
    let _ = ReleaseDC(Some(hwnd), hdc);
}

/// Fill the hub client area with the dark palette. The base fill is the window
/// background (`theme::BG`, matching the class brush); the left activity rail
/// container (0..RAIL_W, full height) is painted `theme::PANEL`; a 1px
/// `theme::BORDER` divider line separates the rail from the content area; and
/// the main list container (everything right of the divider) is `theme::PANEL`
/// so every container pane reads as a `#252526` panel on the `#1E1E1E` window
/// background. Called from `WM_ERASEBKGND`; returns nothing so the caller
/// always answers `LRESULT(1)`. Logs on failure rather than panicking.
unsafe fn paint_hub_background(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if let Err(e) = GetClientRect(hwnd, &mut rc) {
        tracing::warn!(error = %e, "paint_hub_background: GetClientRect failed");
        return;
    }

    // Base: the window background (#1E1E1E). The containers below overpaint it.
    let bg = CreateSolidBrush(theme::BG);
    let _ = FillRect(hdc, &rc, bg);
    let _ = DeleteObject(bg.into());

    // Activity rail container: the left strip, full height.
    let mut rail = rc;
    rail.right = rail.left + RAIL_W;
    if rail.right > rail.left {
        let brush = CreateSolidBrush(theme::PANEL);
        let _ = FillRect(hdc, &rail, brush);
        let _ = DeleteObject(brush.into());
    }

    // 1px divider line between the rail and the main list container.
    if rc.right > rail.right {
        let mut divider = rc;
        divider.left = rail.right;
        divider.right = divider.left + 1;
        let brush = CreateSolidBrush(theme::BORDER);
        let _ = FillRect(hdc, &divider, brush);
        let _ = DeleteObject(brush.into());
    }

    // Main list container: everything right of the divider.
    if rc.right > rail.right + 1 {
        let mut content = rc;
        content.left = rail.right + 1;
        let brush = CreateSolidBrush(theme::PANEL);
        let _ = FillRect(hdc, &content, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// Post a rail-hover change to the hub window: `wParam` = the button's rail
/// index ([`Section::index`]), `lParam` = 1 entered / 0 left. Uses the button's
/// control id (its section id) to recover the index, so no per-button state is
/// needed in the subclass.
unsafe fn post_rail_hover(btn: HWND, hovered: bool) {
    let id = GetDlgCtrlID(btn) as usize;
    let Some(section) = Section::from_id(id) else {
        return;
    };
    if let Ok(parent) = GetParent(btn) {
        let _ = PostMessageW(
            Some(parent),
            WM_APP_RAIL_HOVER,
            WPARAM(section.index()),
            LPARAM(hovered as isize),
        );
    }
}

/// Subclass wndproc for the activity-rail buttons. It only observes the mouse:
/// on `WM_MOUSEMOVE` it arms `TrackMouseEvent` leave tracking and reports the
/// enter to the hub; on `WM_MOUSELEAVE` it reports the leave. It also answers
/// `WM_ERASEBKGND` with a dark fill so no stock `COLOR_BTNFACE` flash ever
/// shows before `WM_DRAWITEM` paints the button. Everything else chains to the
/// stock button proc (kept in `GWLP_USERDATA`) so clicks, focus, and capture
/// keep working. Never unwraps or panics.
unsafe extern "system" fn rail_button_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            post_rail_hover(hwnd, true);
        }
        WM_MOUSELEAVE => {
            post_rail_hover(hwnd, false);
            return LRESULT(0);
        }
        WM_ERASEBKGND => {
            // Dark button background: resting fill is the window BG. WM_DRAWITEM
            // overpaints hover/active states on top of this.
            let hdc = HDC(wparam.0 as *mut c_void);
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() && rc.right > rc.left && rc.bottom > rc.top {
                let brush = CreateSolidBrush(theme::BG);
                let _ = FillRect(hdc, &rc, brush);
                let _ = DeleteObject(brush.into());
            }
            return LRESULT(1);
        }
        _ => {}
    }
    let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    CallWindowProcW(old_proc, hwnd, msg, wparam, lparam)
}

/// Format a unix timestamp as "YYYY-MM-DD HH:MM". Pure arithmetic (no chrono
/// dependency), using the standard civil-from-days conversion.
fn format_ts(unix: u64) -> String {
    let days = unix / 86400;
    let secs = unix % 86400;
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
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
    // Dark form/filter controls: WM_CTLCOLOR* asks the parent for the brush
    // and text colors an edit, static label, or checkbox paints with. Answer
    // with the cached theme brushes so no stock white/gray background shows.
    if msg == WM_CTLCOLOREDIT || msg == WM_CTLCOLORSTATIC || msg == WM_CTLCOLORBTN {
        if let Some(state) = hub_state_mut(hwnd) {
            return state.on_ctl_color(msg, wparam, lparam);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
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
                match state.on_notify(lparam) {
                    // NM_CUSTOMDRAW returns the CDRF_* flags the list view
                    // needs; ordinary notifications return LRESULT(0).
                    Ok(result) => return result,
                    Err(e) => {
                        tracing::error!(error = %e, "hub WM_NOTIFY handler failed");
                        return LRESULT(0);
                    }
                }
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
        WM_ERASEBKGND => {
            // Paint the whole client area ourselves: the left activity rail in
            // theme::PANEL, the content area in theme::BG (matching the class
            // brush). Returning 1 tells DefWindowProc to skip the class-background
            // erase so no stock brush ever shows behind the rail buttons.
            unsafe {
                paint_hub_background(hwnd, HDC(wparam.0 as *mut c_void));
            }
            LRESULT(1)
        }
        WM_APP_RAIL_HOVER => {
            if let Some(state) = hub_state_mut(hwnd) {
                state.on_rail_hover(wparam, lparam);
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

/// Run the hub window and its message loop until the user closes it. Returns
/// the selected [`ConnectionTarget`] once steps 6/8 wire the selection through;
/// `None` today (the shell has no selectable rows yet) and whenever the hub is
/// closed without choosing a connection.
pub fn run() -> Result<Option<ConnectionTarget>, HubError> {
    // The Box keeps the HubWindow alive (and its GWLP_USERDATA pointer valid)
    // for the whole loop; it is dropped after WM_QUIT ends the loop.
    let hub = HubWindow::create()?;
    tracing::info!("hub window open; entering message loop");
    let mut msg = MSG::default();
    unsafe {
        // GetMessageW returns 0 on WM_QUIT (posted from WM_DESTROY or from a
        // row activation) and -1 on error; both end the loop cleanly.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let selected = hub.selected.clone();
    // Drop the hub state before releasing the cached fonts: on the row-activation
    // path WM_DESTROY never ran, and Drop destroys the still-alive window (and
    // every child control) so nothing can paint with a deleted HFONT.
    drop(hub);
    // The hub owns the process-lifetime Segoe UI fonts cached in theme.rs;
    // release them now that no control can repaint.
    let freed = theme::delete_cached_fonts();
    tracing::info!(selected = selected.is_some(), freed, "hub window closed");
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Even rows rest on `PANEL`; odd rows get the `ROW_ALT` zebra tint.
    #[test]
    fn list_row_background_zebras_on_odd_rows() {
        let rest = NMCUSTOMDRAW_DRAW_STATE_FLAGS(0);
        assert_eq!(list_row_background(0, rest), theme::PANEL);
        assert_eq!(list_row_background(1, rest), theme::ROW_ALT);
        assert_eq!(list_row_background(2, rest), theme::PANEL);
        assert_eq!(list_row_background(3, rest), theme::ROW_ALT);
    }

    /// Hover (track-select) lifts the row to `ROW_HOVER`, overriding both the
    /// zebra tint and — matching the original Recent-list behavior — any
    /// selection state.
    #[test]
    fn list_row_background_hover_overrides_zebra_and_selection() {
        assert_eq!(
            list_row_background(1, CDIS_HOT),
            theme::ROW_HOVER,
            "hover wins over the zebra tint"
        );
        assert_eq!(
            list_row_background(0, CDIS_HOT | CDIS_SELECTED),
            theme::ROW_HOVER,
            "hover wins over selection (original Recent behavior)"
        );
        assert_eq!(
            list_row_background(1, CDIS_HOT | CDIS_SELECTED),
            theme::ROW_HOVER,
            "hover wins over selection on zebra rows too"
        );
    }

    /// The selected (non-hot) row uses the accent-tinted `ROW_SELECT`, on
    /// both even and odd rows.
    #[test]
    fn list_row_background_selected_uses_accent_tint() {
        assert_eq!(list_row_background(0, CDIS_SELECTED), theme::ROW_SELECT);
        assert_eq!(list_row_background(1, CDIS_SELECTED), theme::ROW_SELECT);
    }
}

// --- fonts-typography: cached Segoe UI faces fanned out to every control ---
// The SECTION_HEADER_* layout constants live with the other geometry at the
// top of this file (single definition).

/// Send WM_SETFONT (with a redraw) so `hwnd` renders its text with `font`.
fn set_font(hwnd: HWND, font: HFONT) {
    unsafe {
        let _ = SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

/// The column-header control of a report-view list (child window id 0).
fn list_header(list: HWND) -> Option<HWND> {
    unsafe { GetDlgItem(Some(list), 0).ok() }
}

impl HubWindow {
    /// Fan the cached Segoe UI fonts out to the hub window and every live
    /// child control: 9pt base everywhere, 10pt semibold on section headers.
    fn apply_fonts(&self) {
        let base = theme::ui_font(self.dpi());
        let semibold = theme::ui_font_semibold(self.dpi());
        set_font(self.hwnd, base);
        for btn in &self.rail {
            set_font(*btn, base);
        }
        if let Some(f) = &self.form {
            set_font(f.header, semibold);
            for label in &f.labels {
                set_font(*label, base);
            }
            for edit in [f.name, f.host, f.port, f.user, f.domain, f.password] {
                set_font(edit, base);
            }
            for btn in [f.save_pw, f.btn_save, f.btn_delete] {
                set_font(btn, base);
            }
        }
        for s in Section::ALL {
            if let Some(ui) = &self.sections[s.index()] {
                set_font(ui.header, semibold);
                set_font(ui.filter, base);
                set_font(ui.list, base);
                if let Some(hdr) = list_header(ui.list) {
                    set_font(hdr, base);
                }
                if let Some(btn) = ui.edit_btn {
                    set_font(btn, base);
                }
            }
        }
    }
}
