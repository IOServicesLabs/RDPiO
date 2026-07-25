//! WebView2-based OAuth2 authorization-code login panel for Windows 365 / AVD.
//!
//! This module opens a small WebView2 window at the Microsoft `authorize`
//! endpoint and lets the user sign in. AAD then redirects the WebView to the
//! registered native-client URL carrying `?code=<auth code>`; a
//! `NavigationStarting` handler intercepts that redirect, cancels it, and hands
//! the code back to the UI thread, which exchanges it for an access token.
//!
//! The authorization-code grant is used (not device code) because the AVD/W365
//! first-party client is not enabled for the device-code grant — a device-code
//! token request is rejected with `invalid_client`. See [`crate::w365`].

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

use crate::w365::{AccessToken, AuthError};

const CLASS_NAME: &str = "rdpioWebViewAuth";
const WINDOW_TITLE: &str = "Sign in to Windows 365";

/// Keep the WebView2 controller and core view alive while the auth window is
/// shown. Without this the COM references created in the WebView2 callback
/// thread are dropped as soon as the callback returns, leaving the host window
/// blank.
///
/// COM interfaces are not `Send` by default, but we only store them to keep a
/// reference; the underlying `IUnknown::Release` is thread-safe.
#[allow(dead_code)]
struct WebViewHandle {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}
unsafe impl Send for WebViewHandle {}
unsafe impl Sync for WebViewHandle {}

static WEBVIEW: Mutex<Option<WebViewHandle>> = Mutex::new(None);

/// Errors from the WebView2 auth panel.
#[derive(Debug, thiserror::Error)]
pub enum WebViewAuthError {
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("WebView2 error: {0}")]
    WebView2(String),
    #[error("user closed the login window")]
    Cancelled,
}

impl From<webview2_com::Error> for WebViewAuthError {
    fn from(e: webview2_com::Error) -> Self {
        Self::WebView2(e.to_string())
    }
}

impl From<WindowsError> for WebViewAuthError {
    fn from(e: WindowsError) -> Self {
        Self::WebView2(e.to_string())
    }
}

/// Open a WebView2 login panel, let the user complete authorization-code auth,
/// and return the resulting access token.
pub fn authenticate(
    tenant: &str,
    client_id: Option<&str>,
) -> Result<AccessToken, WebViewAuthError> {
    let authorize_url = crate::w365::build_authorize_url(tenant, client_id, None);
    tracing::info!(%authorize_url, "opening WebView2 login panel (authorization-code flow)");

    // The NavigationStarting handler delivers the captured authorization code
    // (or an OAuth error) through this channel once AAD redirects to the
    // native-client URL.
    let (code_tx, code_rx) = mpsc::channel::<Result<String, WebViewAuthError>>();

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }

    let hwnd = create_host_window()?;
    init_webview(hwnd, &authorize_url, code_tx)?;

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    // Pump messages until the redirect carrying the code is intercepted or the
    // user closes the window.
    let code_result = pump_messages(code_rx);

    unsafe {
        let _ = DestroyWindow(hwnd);
        // DestroyWindow synchronously ran WM_DESTROY → PostQuitMessage, leaving a
        // pending WM_QUIT on this thread. Consume it so it does not immediately
        // cancel the next modal window pumped on the same thread (the Cloud PC
        // picker). WM_QUIT only surfaces once the queue is otherwise empty, so
        // drain everything until it appears.
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
        }
    }

    // Release the WebView2 references now that the window is gone.
    *WEBVIEW.lock().unwrap() = None;

    // Exchange the authorization code for an access token (the login window is
    // already torn down; this is a plain HTTPS round-trip).
    let code = code_result?;
    crate::w365::exchange_auth_code(tenant, client_id, None, &code).map_err(WebViewAuthError::Auth)
}

/// Parse an intercepted navigation. Returns `Some(Ok(code))` when `uri` is the
/// native-client redirect carrying an authorization `code`, `Some(Err(msg))`
/// when it carries an OAuth `error`, and `None` for any other navigation (the
/// login pages themselves).
fn parse_auth_redirect(uri: &str) -> Option<Result<String, String>> {
    let query = uri.strip_prefix(crate::w365::NATIVE_REDIRECT_URI)?;
    // The redirect is `<redirect_uri>?code=...` (or `?error=...`). Tolerate an
    // exact match with no query as "not yet".
    let query = query.strip_prefix('?').or_else(|| query.strip_prefix('#'))?;
    let mut code = None;
    let mut error = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(url_decode(v)),
            "error" => error = Some(url_decode(v)),
            "error_description" if error.is_some() => {
                error = Some(format!("{}: {}", error.unwrap(), url_decode(v)));
            }
            _ => {}
        }
    }
    if let Some(c) = code {
        Some(Ok(c))
    } else { error.map(Err) }
}

/// Minimal `application/x-www-form-urlencoded` decoder for redirect query values.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn create_host_window() -> Result<HWND, WebViewAuthError> {
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
            600,
            700,
            None,
            None,
            Some(hinstance),
            None,
        )
        .unwrap_or_default();
        if hwnd.0.is_null() {
            return Err(WebViewAuthError::WebView2(
                "failed to create host window".into(),
            ));
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
    url: &str,
    code_tx: mpsc::Sender<Result<String, WebViewAuthError>>,
) -> Result<(), WebViewAuthError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), WebViewAuthError>>();
    let url = url.to_string();

    // The WebView2 creation callbacks are COM async calls marshaled back to this
    // STA thread. Use a Windows event so the callback can wake the message pump
    // instead of relying on a plain blocking recv.
    let ready_event = unsafe { CreateEventW(None, false, false, None)? };

    let create_result = unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            None,
            None,
            None,
            &CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
                move |_, env| {
                    let env = match env {
                        Some(env) => env,
                        None => {
                            let err = WebViewAuthError::WebView2(
                                "WebView2 runtime failed to create environment".into(),
                            );
                            tracing::error!(%err);
                            signal_ready(&ready_tx, ready_event, Err(err));
                            return Ok(());
                        }
                    };
                    tracing::info!("WebView2 environment created");
                    if let Err(e) =
                        init_controller(env, hwnd, &url, code_tx, ready_tx, ready_event)
                    {
                        tracing::error!(error = %e, "failed to create WebView2 controller");
                    }
                    Ok(())
                },
            )),
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
    ready_rx: &mpsc::Receiver<Result<(), WebViewAuthError>>,
) -> Result<(), WebViewAuthError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let handles = [ready_event];

    loop {
        if let Ok(result) = ready_rx.try_recv() {
            return result;
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            tracing::error!("timed out waiting for WebView2 initialization");
            return Err(WebViewAuthError::Cancelled);
        }
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            return Err(WebViewAuthError::Cancelled);
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
                    return Err(WebViewAuthError::Cancelled);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn signal_ready(
    tx: &mpsc::Sender<Result<(), WebViewAuthError>>,
    event: windows::Win32::Foundation::HANDLE,
    result: Result<(), WebViewAuthError>,
) {
    let _ = tx.send(result);
    unsafe {
        let _ = SetEvent(event);
    }
}

fn init_controller(
    env: ICoreWebView2Environment,
    hwnd: HWND,
    url: &str,
    code_tx: mpsc::Sender<Result<String, WebViewAuthError>>,
    ready_tx: mpsc::Sender<Result<(), WebViewAuthError>>,
    ready_event: windows::Win32::Foundation::HANDLE,
) -> windows::core::Result<()> {
    let url = url.to_string();
    unsafe {
        env.CreateCoreWebView2Controller(
            hwnd,
            &CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |_, controller| {
                    let controller = match controller {
                        Some(c) => c,
                        None => {
                            let err = WebViewAuthError::WebView2(
                                "WebView2 failed to create controller".into(),
                            );
                            tracing::error!(%err);
                            signal_ready(&ready_tx, ready_event, Err(err));
                            return Ok(());
                        }
                    };
                    if let Err(e) =
                        configure_controller(controller, &url, code_tx, ready_tx, ready_event)
                    {
                        tracing::error!(error = %e, "failed to configure WebView2 controller");
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
    url: &str,
    code_tx: mpsc::Sender<Result<String, WebViewAuthError>>,
    ready_tx: mpsc::Sender<Result<(), WebViewAuthError>>,
    ready_event: windows::Win32::Foundation::HANDLE,
) -> windows::core::Result<()> {
    unsafe {
        let webview: ICoreWebView2 = match controller.CoreWebView2() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "failed to get WebView2 core view");
                signal_ready(&ready_tx, ready_event, Err(e.into()));
                return Ok(());
            }
        };

        // Intercept navigations: once the user signs in, AAD redirects to the
        // native-client URL carrying `?code=...`. Capture it, cancel the
        // navigation (the page would just be blank), and hand the code to the
        // UI thread. windows 0.62 / webview2-com 0.39 take the event
        // registration token as a `*mut i64` out-param.
        let mut nav_token: i64 = 0;
        let _ = webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri_pwstr = PWSTR::null();
                    args.Uri(&mut uri_pwstr)?;
                    let uri = webview2_com::take_pwstr(uri_pwstr);
                    match parse_auth_redirect(&uri) {
                        Some(Ok(code)) => {
                            tracing::info!("captured authorization code from redirect");
                            args.SetCancel(true)?;
                            let _ = code_tx.send(Ok(code));
                        }
                        Some(Err(err)) => {
                            tracing::error!(%err, "OAuth redirect returned an error");
                            args.SetCancel(true)?;
                            let _ = code_tx.send(Err(WebViewAuthError::WebView2(err)));
                        }
                        None => {
                            tracing::info!(%uri, "WebView navigation starting");
                        }
                    }
                }
                Ok(())
            })),
            &mut nav_token,
        );

        controller.SetBounds(windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 600,
            bottom: 700,
        })?;

        // Make sure the controller is visible; default is true, but be explicit
        // in case the runtime or host window state changed it.
        controller.SetIsVisible(true)?;

        let hurl = HSTRING::from(url);
        tracing::info!(%url, "navigating WebView2 to login page");
        webview.Navigate(PCWSTR(hurl.as_ptr()))?;

        // Keep the controller and webview alive for the lifetime of the window.
        // Without this reference the view is destroyed when the callback returns
        // and the user sees only a blank host window.
        *WEBVIEW.lock().unwrap() = Some(WebViewHandle {
            controller,
            webview,
        });

        signal_ready(&ready_tx, ready_event, Ok(()));
    }
    Ok(())
}

fn pump_messages(
    code_rx: mpsc::Receiver<Result<String, WebViewAuthError>>,
) -> Result<String, WebViewAuthError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);

    loop {
        // The NavigationStarting handler delivers the authorization code (or an
        // error) through this channel; it is not tied to any window message, so
        // it must be polled.
        if let Ok(result) = code_rx.try_recv() {
            return result;
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(WebViewAuthError::Cancelled);
        }
        // Block for a message, but wake at least every 100 ms so the channel and
        // the deadline are still checked while the login window sits idle. A
        // plain blocking `GetMessageW` would never return on an idle window, so
        // neither the token nor the timeout would ever be observed.
        let slice = (deadline - now).as_millis().min(100) as u32;

        unsafe {
            MsgWaitForMultipleObjectsEx(None, slice, QS_ALLINPUT, MWMO_INPUTAVAILABLE);

            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    // The user closed the login window.
                    return Err(WebViewAuthError::Cancelled);
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
    fn redirect_with_code_is_captured() {
        let uri = format!("{}?code=ABC123&session_state=xyz", crate::w365::NATIVE_REDIRECT_URI);
        assert_eq!(parse_auth_redirect(&uri), Some(Ok("ABC123".to_string())));
    }

    #[test]
    fn redirect_with_error_is_reported() {
        let uri = format!(
            "{}?error=access_denied&error_description=user%20cancelled",
            crate::w365::NATIVE_REDIRECT_URI
        );
        assert_eq!(
            parse_auth_redirect(&uri),
            Some(Err("access_denied: user cancelled".to_string()))
        );
    }

    #[test]
    fn login_pages_are_not_treated_as_redirect() {
        assert_eq!(parse_auth_redirect("https://login.microsoftonline.com/common/login"), None);
        // The bare redirect URI with no query is not yet a result.
        assert_eq!(parse_auth_redirect(crate::w365::NATIVE_REDIRECT_URI), None);
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a%20b+c%2Fd"), "a b c/d");
    }
}

