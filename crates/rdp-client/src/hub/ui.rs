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

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, HDC, InvalidateRect, SetBkMode,
    SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, DRAWITEMSTRUCT, ICC_STANDARD_CLASSES, ICC_WIN95_CLASSES,
    INITCOMMONCONTROLSEX, LVCOLUMNW, LVCOLUMNW_FORMAT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH,
    LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVNI_SELECTED, LVN_ITEMACTIVATE, LVS_EX_FULLROWSELECT,
    LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMITEMACTIVATE, NM_DBLCLK,
    ODT_BUTTON, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{
    theme, ConnectionInput, ConnectionRecord, ConnectionStore, ConnectionTarget, HubError,
    MruStore,
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

/// BM_GETCHECK result values (kept local; the BST_* constants live in another
/// windows feature module we do not enable).
const BST_CHECKED: isize = 1;

/// New Connection form geometry (relative to the content area).
const FORM_X: i32 = 32;
const FORM_TOP: i32 = 28;
const FORM_LABEL_W: i32 = 120;
const FORM_EDIT_H: i32 = 24;
const FORM_ROW_H: i32 = 36;
const FORM_PORT_W: i32 = 140;

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
                // The rail background; the content panes paint over their own
                // area with the theme palette, so the hub brush only shows in
                // the rail.
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

            let mut hub = Box::new(HubWindow {
                hwnd,
                hinstance,
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

    /// WM_NOTIFY: list-view activation — a double-click (NM_DBLCLK) or Enter on
    /// the focused row (LVN_ITEMACTIVATE) builds a [`ConnectionTarget`] from
    /// the activated row and hands it to the hub runner (posts WM_QUIT; `run()`
    /// returns the target).
    fn on_notify(&mut self, lparam: LPARAM) -> Result<(), HubError> {
        let ptr = lparam.0 as *const NMHDR;
        if ptr.is_null() {
            return Ok(());
        }
        let hdr = unsafe { &*ptr };
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
        Ok(())
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

            // Filter edit, subclassed so Enter can be caught (single-line edits
            // send no Enter notification of their own).
            let filter = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | ES_AUTOHSCROLL as u32),
                x,
                rc.top + 16,
                w,
                FILTER_H,
                Some(self.hwnd),
                Some(HMENU(section_filter_id(section) as *mut c_void)),
                Some(self.hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(filter {section:?}): {e}")))?;
            // Chain the original wndproc: keep it in GWLP_USERDATA, install ours.
            let old_proc = GetWindowLongPtrW(filter, GWLP_WNDPROC);
            SetWindowLongPtrW(filter, GWLP_USERDATA, old_proc);
            SetWindowLongPtrW(filter, GWLP_WNDPROC, filter_edit_proc as *const () as isize);

            // SysListView32 report view.
            let list = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("SysListView32"),
                w!(""),
                WINDOW_STYLE(
                    (WS_CHILD | WS_VISIBLE).0 | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
                ),
                x,
                rc.top + 16 + FILTER_H + 14,
                w,
                (rc.bottom - rc.top - 16 - FILTER_H - 14 - 16).max(1),
                Some(self.hwnd),
                Some(HMENU(section_list_id(section) as *mut c_void)),
                Some(self.hinstance),
                None,
            )
            .map_err(|e| HubError::win32(format!("CreateWindowExW(list {section:?}): {e}")))?;
            let _ = SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                Some(WPARAM(0)),
                Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES) as isize)),
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

            self.sections[section.index()] = Some(SectionUi {
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
                        user: m.username.clone().unwrap_or_default(),
                        last_used: format_ts(m.last_connected_at),
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
        let list_top = rc.top + 16 + FILTER_H + 14;
        let list_h = (rc.bottom - list_top - 16).max(1);
        unsafe {
            let _ = MoveWindow(ui.filter, x, rc.top + 16, w, FILTER_H, true);
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
        let mut y = rc.top + FORM_TOP;
        unsafe {
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

    /// Reset the form to a blank "new connection" state.
    fn clear_form(&mut self) {
        let Some(f) = self.form.as_mut() else {
            return;
        };
        set_edit_text(f.name, "");
        set_edit_text(f.host, "");
        set_edit_text(f.port, "");
        set_edit_text(f.user, "");
        set_edit_text(f.domain, "");
        set_edit_text(f.password, "");
        unsafe {
            let _ = SendMessageW(f.save_pw, BM_SETCHECK, Some(WPARAM(1)), None);
        }
        f.editing_id = None;
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
        }
    }
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
fn form_hwnds(f: &FormState) -> [HWND; 15] {
    [
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

/// Subclass wndproc for the filter edits: intercepts Enter (connect the top
/// match) and chains everything else to the original proc, whose pointer is
/// kept in the edit's `GWLP_USERDATA`. Never unwraps or panics.
unsafe extern "system" fn filter_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_KEYDOWN && (wparam.0 & 0xffff) as u16 == VK_RETURN {
        // Tell the hub (the edit's parent) the user pressed Enter; it connects
        // the top filtered row. The edit's HWND rides along in lParam.
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
    let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old_proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    CallWindowProcW(old_proc, hwnd, msg, wparam, lparam)
}

/// Fill the hub client area with the dark palette: the left activity rail
/// strip (0..RAIL_W, full height) in [`theme::PANEL`], the content area to its
/// right in [`theme::BG`]. Called from `WM_ERASEBKGND`; returns nothing so the
/// caller always answers `LRESULT(1)`. Logs on failure rather than panicking.
unsafe fn paint_hub_background(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if let Err(e) = GetClientRect(hwnd, &mut rc) {
        tracing::warn!(error = %e, "paint_hub_background: GetClientRect failed");
        return;
    }
    let mut rail = rc;
    rail.right = rail.left + RAIL_W;
    if rail.right > rail.left {
        let brush = CreateSolidBrush(theme::PANEL);
        let _ = FillRect(hdc, &rail, brush);
        let _ = DeleteObject(brush.into());
    }
    if rc.right > rail.right {
        let mut content = rc;
        content.left = rail.right;
        let brush = CreateSolidBrush(theme::BG);
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
    tracing::info!(selected = selected.is_some(), "hub window closed");
    Ok(selected)
}
