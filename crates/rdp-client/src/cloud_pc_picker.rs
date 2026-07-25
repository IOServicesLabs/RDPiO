//! WebView2-based Cloud PC picker for Windows 365 / AVD.
//!
//! After authentication and feed discovery, when the W365 feed lists more than
//! one Cloud PC this opens a small WebView2 window rendering the list. The user
//! clicks a resource and the selected index is returned.
//!
//! Selection is captured without any JavaScript bridge: each row is an anchor
//! pointing at a private `rdpio://select/<index>` URL. The `NavigationStarting`
//! handler intercepts that navigation, cancels it (so the WebView never tries to
//! resolve the bogus scheme) and forwards the parsed index to the UI thread.

use std::sync::{mpsc, Mutex};
use std::time::Duration;

use webview2_com::{
    Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
        ICoreWebView2Environment,
    },
    CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, NavigationStartingEventHandler,
};
use windows::core::{Error as WindowsError, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IsWindow,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SW_SHOW,
    WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::feed::FeedEntry;

const CLASS_NAME: &str = "rdpioCloudPcPicker";
const WINDOW_TITLE: &str = "Choose a Cloud PC";
/// Private URL prefix the rendered anchors navigate to; intercepted and never
/// actually fetched.
const SELECT_MARKER: &str = "select/";

/// Keep the WebView2 controller and core view alive while the picker window is
/// shown. Without this reference the view is destroyed when the creation
/// callback returns, leaving the host window blank.
#[allow(dead_code)]
struct WebViewHandle {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}
unsafe impl Send for WebViewHandle {}
unsafe impl Sync for WebViewHandle {}

static WEBVIEW: Mutex<Option<WebViewHandle>> = Mutex::new(None);

/// Errors from the Cloud PC picker panel.
#[derive(Debug, thiserror::Error)]
pub enum PickerError {
    #[error("WebView2 error: {0}")]
    WebView2(String),
    #[error("Cloud PC selection was cancelled")]
    Cancelled,
}

impl From<webview2_com::Error> for PickerError {
    fn from(e: webview2_com::Error) -> Self {
        Self::WebView2(e.to_string())
    }
}

impl From<WindowsError> for PickerError {
    fn from(e: WindowsError) -> Self {
        Self::WebView2(e.to_string())
    }
}

/// Open a WebView2 window listing `entries` and return the index of the one the
/// user picks. The caller should only invoke this when `entries.len() > 1`.
pub fn choose_cloud_pc(entries: &[FeedEntry]) -> Result<usize, PickerError> {
    let html = build_html(entries);
    let entry_count = entries.len();

    let (select_tx, select_rx) = mpsc::channel::<usize>();

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }

    let hwnd = create_host_window()?;
    init_webview(hwnd, &html, entry_count, select_tx)?;

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let result = pump_messages(select_rx);

    unsafe {
        let _ = DestroyWindow(hwnd);
        // Consume the WM_QUIT that WM_DESTROY's PostQuitMessage posted, so a
        // stale quit cannot leak onto this thread (matches webview_auth).
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
        }
    }

    *WEBVIEW.lock().unwrap() = None;

    result
}

/// Build the picker page. Display names and addresses come from a remote feed,
/// so they are HTML-escaped before interpolation.
fn build_html(entries: &[FeedEntry]) -> String {
    let mut rows = String::new();
    for (i, e) in entries.iter().enumerate() {
        let name = if e.display_name.is_empty() {
            &e.id
        } else {
            &e.display_name
        };
        // Prefer the resource id as the sub-label: Cloud PCs often share a SKU
        // display name (e.g. two "Cloud PC Enterprise 4vCPU/16GB/256GB"), and the
        // id is what tells them apart. Fall back to the gateway/host/address.
        let meta = if !e.resource_id.is_empty() {
            e.resource_id.clone()
        } else if !e.gateway_fqdn.is_empty() {
            e.gateway_fqdn.clone()
        } else if !e.hostname.is_empty() {
            format!("{}:{}", e.hostname, e.port)
        } else {
            e.address.clone()
        };
        rows.push_str(&format!(
            "<a class=\"card\" href=\"rdpio://{marker}{i}\">\
               <span class=\"name\">{name}</span>\
               <span class=\"meta\">{meta}</span>\
             </a>",
            marker = SELECT_MARKER,
            i = i,
            name = html_escape(name),
            meta = html_escape(&meta),
        ));
    }

    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"color-scheme\" content=\"dark\">\
<style>\
:root{{color-scheme:dark}}*{{box-sizing:border-box}}\
body{{margin:0;padding:24px;font:14px/1.45 'Segoe UI',system-ui,sans-serif;background:#1b1b1f;color:#e6e6e6}}\
h1{{font-size:18px;font-weight:600;margin:0 0 4px}}\
p.sub{{margin:0 0 20px;color:#9a9aa2;font-size:13px}}\
.list{{display:flex;flex-direction:column;gap:10px}}\
a.card{{display:flex;flex-direction:column;gap:3px;padding:14px 16px;border-radius:10px;background:#26262b;border:1px solid #34343b;text-decoration:none;color:inherit;transition:background .12s,border-color .12s,transform .04s}}\
a.card:hover{{background:#2f2f37;border-color:#4c6ef5}}\
a.card:active{{transform:scale(.995)}}\
a.card:focus-visible{{outline:2px solid #4c6ef5;outline-offset:2px}}\
.name{{font-weight:600;font-size:15px}}\
.meta{{color:#9a9aa2;font-size:12.5px;word-break:break-all}}\
</style></head><body>\
<h1>Choose a Cloud PC</h1>\
<p class=\"sub\">{count} resources available — select one to connect.</p>\
<div class=\"list\">{rows}</div>\
<script>var c=document.querySelector('a.card');if(c)c.focus();</script>\
</body></html>",
        count = entries.len(),
        rows = rows,
    )
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn create_host_window() -> Result<HWND, PickerError> {
    let class_name: Vec<u16> = CLASS_NAME.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = WINDOW_TITLE.encode_utf16().chain(Some(0)).collect();

    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let hinstance: HINSTANCE = hmodule.into();
        let class = WNDCLASSW {
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        RegisterClassW(&class);

        // windows 0.62: `CreateWindowExW` returns `Result<HWND>` and the nullable
        // parent/menu/instance params are `Option`. `.unwrap_or_default()` maps a
        // creation failure to a null `HWND`, which the check below rejects.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            520,
            620,
            None,
            None,
            Some(hinstance),
            None,
        )
        .unwrap_or_default();
        if hwnd.0.is_null() {
            return Err(PickerError::WebView2("failed to create host window".into()));
        }
        Ok(hwnd)
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
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

fn init_webview(
    hwnd: HWND,
    html: &str,
    entry_count: usize,
    select_tx: mpsc::Sender<usize>,
) -> Result<(), PickerError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PickerError>>();
    let html = html.to_string();

    let ready_event = unsafe { CreateEventW(None, false, false, None)? };

    let create_result = unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            None,
            None,
            None,
            &CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |_, env| {
                let env = match env {
                    Some(env) => env,
                    None => {
                        let err = PickerError::WebView2(
                            "WebView2 runtime failed to create environment".into(),
                        );
                        tracing::error!(%err);
                        signal_ready(&ready_tx, ready_event, Err(err));
                        return Ok(());
                    }
                };
                tracing::info!("Cloud PC picker WebView2 environment created");
                if let Err(e) =
                    init_controller(env, hwnd, &html, entry_count, select_tx, ready_tx, ready_event)
                {
                    tracing::error!(error = %e, "failed to create Cloud PC picker controller");
                }
                Ok(())
            })),
        )
    };
    if let Err(e) = create_result {
        unsafe {
            let _ = CloseHandle(ready_event);
        }
        return Err(e.into());
    }

    let result = pump_init_messages(hwnd, ready_event, &ready_rx);
    unsafe {
        let _ = CloseHandle(ready_event);
    }
    result
}

fn pump_init_messages(
    hwnd: HWND,
    ready_event: windows::Win32::Foundation::HANDLE,
    ready_rx: &mpsc::Receiver<Result<(), PickerError>>,
) -> Result<(), PickerError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let handles = [ready_event];

    loop {
        if let Ok(result) = ready_rx.try_recv() {
            return result;
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            tracing::error!("timed out waiting for Cloud PC picker WebView2 init");
            return Err(PickerError::Cancelled);
        }
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            return Err(PickerError::Cancelled);
        }

        let timeout = (deadline - now).as_millis().min(100) as u32;
        unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&handles),
                timeout,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );

            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Err(PickerError::Cancelled);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn signal_ready(
    tx: &mpsc::Sender<Result<(), PickerError>>,
    event: windows::Win32::Foundation::HANDLE,
    result: Result<(), PickerError>,
) {
    let _ = tx.send(result);
    unsafe {
        let _ = SetEvent(event);
    }
}

fn init_controller(
    env: ICoreWebView2Environment,
    hwnd: HWND,
    html: &str,
    entry_count: usize,
    select_tx: mpsc::Sender<usize>,
    ready_tx: mpsc::Sender<Result<(), PickerError>>,
    ready_event: windows::Win32::Foundation::HANDLE,
) -> windows::core::Result<()> {
    let html = html.to_string();
    unsafe {
        env.CreateCoreWebView2Controller(
            hwnd,
            &CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |_, controller| {
                    let controller = match controller {
                        Some(c) => c,
                        None => {
                            let err = PickerError::WebView2(
                                "WebView2 failed to create controller".into(),
                            );
                            tracing::error!(%err);
                            signal_ready(&ready_tx, ready_event, Err(err));
                            return Ok(());
                        }
                    };
                    if let Err(e) =
                        configure_controller(controller, &html, entry_count, select_tx, ready_tx, ready_event)
                    {
                        tracing::error!(error = %e, "failed to configure Cloud PC picker controller");
                    }
                    Ok(())
                },
            )),
        )?;
    }
    Ok(())
}

fn configure_controller(
    controller: ICoreWebView2Controller,
    html: &str,
    entry_count: usize,
    select_tx: mpsc::Sender<usize>,
    ready_tx: mpsc::Sender<Result<(), PickerError>>,
    ready_event: windows::Win32::Foundation::HANDLE,
) -> windows::core::Result<()> {
    unsafe {
        let webview: ICoreWebView2 = match controller.CoreWebView2() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "failed to get Cloud PC picker core view");
                signal_ready(&ready_tx, ready_event, Err(e.into()));
                return Ok(());
            }
        };

        // Intercept clicks: each row navigates to `rdpio://select/<index>`, which
        // we cancel and translate into a selection. windows 0.62 / webview2-com
        // 0.39 take the registration token as `*mut i64` and `SetCancel(bool)`.
        let mut nav_token: i64 = 0;
        let _ = webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri_pwstr = PWSTR::null();
                    args.Uri(&mut uri_pwstr)?;
                    let uri = webview2_com::take_pwstr(uri_pwstr);
                    if let Some(index) = parse_selection(&uri, entry_count) {
                        // Cancel the bogus navigation and report the choice.
                        args.SetCancel(true)?;
                        let _ = select_tx.send(index);
                    }
                }
                Ok(())
            })),
            &mut nav_token,
        );

        controller.SetBounds(windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 520,
            bottom: 620,
        })?;

        controller.SetIsVisible(true)?;

        let hhtml = HSTRING::from(html);
        tracing::info!("navigating Cloud PC picker to selection page");
        webview.NavigateToString(PCWSTR(hhtml.as_ptr()))?;

        // Keep the controller and webview alive for the lifetime of the window.
        *WEBVIEW.lock().unwrap() = Some(WebViewHandle {
            controller,
            webview,
        });

        signal_ready(&ready_tx, ready_event, Ok(()));
    }
    Ok(())
}

/// Extract a selection index from an intercepted `rdpio://select/<index>` URL.
/// Returns `None` for ordinary navigations (the initial document load) or an
/// out-of-range index.
fn parse_selection(uri: &str, entry_count: usize) -> Option<usize> {
    let (_, rest) = uri.split_once(SELECT_MARKER)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let index = digits.parse::<usize>().ok()?;
    (index < entry_count).then_some(index)
}

fn pump_messages(select_rx: mpsc::Receiver<usize>) -> Result<usize, PickerError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);

    loop {
        // The chosen index arrives through this channel from the
        // NavigationStarting handler (which runs inside DispatchMessageW below).
        if let Ok(index) = select_rx.try_recv() {
            return Ok(index);
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(PickerError::Cancelled);
        }
        // Block for a message, but wake at least every 100 ms so the deadline is
        // still observed when the window sits idle. A plain blocking `GetMessageW`
        // would never return on an idle window, so the timeout could never fire.
        let slice = (deadline - now).as_millis().min(100) as u32;

        unsafe {
            MsgWaitForMultipleObjectsEx(None, slice, QS_ALLINPUT, MWMO_INPUTAVAILABLE);

            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    // The window was closed without a selection.
                    return Err(PickerError::Cancelled);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_selection_extracts_index() {
        assert_eq!(parse_selection("rdpio://select/0", 3), Some(0));
        assert_eq!(parse_selection("rdpio://select/2", 3), Some(2));
        // Trailing slash / query normalisation still parses.
        assert_eq!(parse_selection("rdpio://select/1/", 3), Some(1));
    }

    #[test]
    fn parse_selection_rejects_out_of_range_and_non_marker() {
        assert_eq!(parse_selection("rdpio://select/5", 3), None);
        assert_eq!(parse_selection("https://login.microsoftonline.com/", 3), None);
        assert_eq!(parse_selection("about:blank", 3), None);
    }

    #[test]
    fn html_escape_neutralises_markup() {
        assert_eq!(
            html_escape("<b>a&\"'</b>"),
            "&lt;b&gt;a&amp;&quot;&#39;&lt;/b&gt;"
        );
    }

    #[test]
    fn build_html_lists_every_entry() {
        let entries = vec![
            FeedEntry {
                display_name: "Alpha".into(),
                gateway_fqdn: "gw.example.com".into(),
                ..Default::default()
            },
            FeedEntry {
                display_name: "Beta".into(),
                hostname: "10.0.0.5".into(),
                port: 3390,
                ..Default::default()
            },
        ];
        let html = build_html(&entries);
        assert!(html.contains("rdpio://select/0"));
        assert!(html.contains("rdpio://select/1"));
        assert!(html.contains("Alpha"));
        assert!(html.contains("gw.example.com"));
        assert!(html.contains("10.0.0.5:3390"));
    }
}
