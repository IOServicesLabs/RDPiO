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

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetStockObject, InvalidateRect, SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_LEFT,
    DT_SINGLELINE, DT_TOP, DT_VCENTER, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, DRAWITEMSTRUCT, ICC_STANDARD_CLASSES, ICC_WIN95_CLASSES,
    INITCOMMONCONTROLSEX, LVCOLUMNW, LVCOLUMNW_FORMAT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH,
    LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVNI_SELECTED, LVN_ITEMACTIVATE, LVS_EX_FULLROWSELECT,
    LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMITEMACTIVATE, NM_DBLCLK,
    ODT_BUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{ConnectionStore, ConnectionTarget, HubError, MruStore};

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

/// VK_RETURN; kept local to avoid pulling in more windows feature modules.
const VK_RETURN: u16 = 0x0D;

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

/// Height of the filter-edit strip at the top of each list pane.
const FILTER_H: i32 = 26;

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
    /// Current filter text (live-filtered on EN_CHANGE).
    filter_text: String,
    /// Full, unfiltered row set (rebuilt on section refresh).
    rows: Vec<ListRow>,
    /// Indices into `rows` matching the current filter, in list order.
    filtered: Vec<usize>,
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
    /// Only the New section uses a pane (the step-5 placeholder); the Recent/
    /// Saved sections use `sections` (filter edit + list view) instead.
    panes: [Option<HWND>; 3],
    /// The placeholder pane currently visible (mirrors `section` for New).
    content: Option<HWND>,
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

    /// Switch the visible content to `section`, creating the section's controls
    /// (filter edit + list view) or the placeholder pane on first visit. List
    /// sections re-read their store on every switch so new saves / MRU entries
    /// appear. Also re-highlights the rail buttons.
    fn show_section(&mut self, section: Section) -> Result<(), HubError> {
        unsafe {
            // Hide whatever is currently showing: the placeholder pane and
            // every list section's filter/list pair.
            if let Some(cur) = self.content {
                let _ = ShowWindow(cur, SW_HIDE);
                self.content = None;
            }
            for s in Section::ALL {
                if let Some(ui) = &self.sections[s.index()] {
                    let _ = ShowWindow(ui.filter, SW_HIDE);
                    let _ = ShowWindow(ui.list, SW_HIDE);
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
                    }
                }
                Section::New => {
                    // Placeholder pane for now; step-7 replaces it with the form.
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
                }
            }
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

    /// Re-layout all children for the current client size (WM_SIZE handler):
    /// the rail buttons always, plus whatever content is active.
    fn layout(&mut self) -> Result<(), HubError> {
        unsafe {
            for (i, btn) in self.rail.iter().enumerate() {
                let y = RAIL_PAD_TOP + i as i32 * (RAIL_BTN_H + RAIL_BTN_GAP);
                let _ = MoveWindow(*btn, RAIL_BTN_X, y, RAIL_BTN_W, RAIL_BTN_H, true);
            }
            match self.section {
                Section::Recent | Section::Saved => self.layout_section(self.section)?,
                Section::New => {
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
                }
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

            self.sections[section.index()] = Some(SectionUi {
                filter,
                list,
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
