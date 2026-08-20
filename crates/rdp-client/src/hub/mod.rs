//! # Hub — saved connections, MRU history, and the connection-picker UI
//!
//! This module is the integration point between the existing RDPiO connection
//! bootstrap and a native Win32 hub window: it persists saved connections
//! (DPAPI-protected passwords) and a most-recently-used history under
//! `%LOCALAPPDATA%\rdpio\`, and later steps add the hub window that lists
//! Recent/Saved entries and returns a [`ConnectionTarget`] for the existing
//! connect path to consume.
//!
//! The doc comment below is the **step-1 integration map** — the audited,
//! verified call sites and signatures this module must plug into. Every symbol
//! was read from the live sources in `crates/rdp-client/src/` at audit time, so
//! later hub steps can rely on these names instead of re-deriving them.
//!
//! ---
//!
//! ## 1. CLI argument parsing (`src/main.rs`)
//!
//! - `struct Args` (main.rs:599) — hand-rolled parser, no external arg crate.
//!   - `host: Option<String>`, `port: u16` (default `3389`), `user: Option<String>`,
//!     `domain: Option<String>`, `password: Option<String>`, `insecure: bool`,
//!     `drive: Vec<String>`, plus ~40 session/UI knobs (multimon, fullscreen,
//!     width/height, quality, udp, …). Not `Clone` and not `pub` — it lives in
//!     the crate root and is only used by `main`.
//! - `impl Args { fn from_env() -> Self }` (main.rs:741) — walks
//!   `std::env::args().skip(1)`, matches `--host|-h`, `--port`, `--user|-u`,
//!   `--domain|-d`, `--password|-p`, etc.
//! - `fn main()` (main.rs:39) — dispatches:
//!   ```text
//!   args.host.is_some() || args.w365 || args.feed.is_some()
//!       → win::run_connected(&args)     // Windows: connect + paint live desktop
//!       → run_connect(&args)            // non-Windows: headless protocol stack
//!   otherwise
//!       → win::run()                    // Windows: the no-host "M0" demo window
//!       → exit(2) + usage               // non-Windows
//!   ```
//!   **Step-8 target:** the hub launch belongs in the `otherwise` arm — with no
//!   connection-target args, call `hub::run()` instead of (or before) `win::run()`;
//!   a returned `Some(ConnectionTarget)` is converted into the existing
//!   `Args`-equivalent config and handed to the same `win::run_connected` path.
//! - `fn config_from_args(args: &Args) -> ClientConfig` (main.rs:~934, private) —
//!   builds `rdp_core::ClientConfig { hostname, port, credentials:
//!   Credentials { domain, username, password }, … }`, applying
//!   `rdp_core::split_domain_user(&domain, &user)` (rdp-core/src/lib.rs:172) to
//!   split `DOMAIN\user` / `.\user` before the fields reach SSPI. The hub's
//!   "convert a selected target into the connect bootstrap" logic mirrors this.

//! ## 2. Connection bootstrap (`src/connect.rs`)
//!
//! - `pub fn establish_reconnect(
//!        config: &mut rdp_core::ClientConfig,
//!        reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
//!    ) -> Result<Established, Box<dyn std::error::Error>>` (connect.rs:112)
//!   — the single entry point every real connection goes through. Internally:
//!   `transport::connect(config)` → X.224 negotiation → `secure()` (Schannel TLS
//!   + optional CredSSP/NLA via `rdp_nla::sspi::authenticate`) or `legacy()`
//!   (Standard RDP Security) → `session::activate(&mut transport, config,
//!   protocol, reconnect)`.
//! - `pub struct Established { pub transport: Transport, pub session:
//!   ActiveSession, pub control: Option<TcpStream>, pub input_tcp:
//!   Option<TcpStream>, pub protocol: SecurityProtocol }` (connect.rs:96).
//! - `pub enum Transport { Tcp(TcpStream), Tls(Box<TlsStream<TcpStream>>),
//!   WebSocket(Box<ReverseConnectStream>), WebSocketTls(Box<TlsStream<ReverseConnectStream>>) }`
//!   (connect.rs:30).
//! - `Established` is consumed at main.rs:3703:
//!   ```text
//!   let connect::Established { transport, mut session, control, input_tcp, protocol } = conn;
//!   ```
//!   then the blocking session runs on a worker thread via
//!   `session::run_graphics_session(...)` (graphics path) or
//!   `session::run_session(&mut transport, &mut session, &mut sink)` (legacy),
//!   with a `ChannelSink` bridging decoded frames back to the UI thread.
//!
//! ## 3. Where a connection is considered successful
//!
//! - `connect::establish_reconnect(...)` returning `Ok(Established)` —
//!   activation has completed at that point (`session::activate` inside). Called
//!   from `win::run_connected` at main.rs:3467 (first connect, with 3 retries
//!   + backoff) and main.rs:3674 (auto-reconnect inside the `'session` loop).
//! - The explicit success marker (main.rs:3704):
//!   `tracing::info!(?protocol, info = ?session.info(), "RDP session ACTIVE");`
//!   — **this is the hook point for step-9's MRU recording** (after
//!   `establish_reconnect` returns `Ok`, before/after the ACTIVE log).
//! - Session end: `session::run_session` / `run_graphics_session` returning logs
//!   `"session ended"` (main.rs:3919) and the `'session` loop may reconnect.
//!
//! ## 4. Win32 window creation / message-loop pattern (`src/window.rs`,
//!    `src/connbar.rs`)
//!
//! Uses the `windows` crate **v0.62** (NOT `windows-sys` — the plan's "windows-sys"
//! wording should be read as "the raw Win32 FFI", modeled on these two files):
//! - `window.rs`: `Window::new(title, width, height) -> windows::core::Result<Self>`
//!   → private `Window::create` does: `GetModuleHandleW(None)` →
//!   `WNDCLASSEXW { cbSize, lpfnWndProc: Some(wndproc), hInstance, lpszClassName:
//!   w!("rdpioWindowClass"), hCursor: LoadCursorW(None, IDC_ARROW)?, … }` →
//!   `let _ = RegisterClassExW(&wc);` (class-exists ignored) →
//!   `CreateWindowExW(ex_style, class, title, style, x, y, w, h, None, None,
//!   Some(hinstance), None)?` → `ShowWindow(hwnd, SW_SHOW)`.
//! - `Window::pump(&self) -> Frame` (window.rs:443) — non-blocking pump:
//!   `PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE)` loop; `WM_QUIT` sets a
//!   static `QUIT` flag; else `TranslateMessage` + `DispatchMessageW`. Returns
//!   `Frame::Quit` or `Frame::Continue { resize: Option<(u32, u32)> }`.
//! - `wndproc` (window.rs:711): `WM_DESTROY` → `PostQuitMessage(0)`; `WM_SIZE`
//!   packs `(width << 16) | height` into a static `PENDING_RESIZE`; Ctrl+Shift+Q
//!   → `PostQuitMessage(0)`. State lives in process statics (atomics); the
//!   multi-monitor case stores an offset via `SetWindowLongPtrW(hwnd,
//!   GWLP_USERDATA, packed)` (window.rs:369) — the pattern step-5's hub state
//!   should follow.
//! - `connbar.rs` (the closest small-window model): `WNDCLASSEXW` with
//!   `lpfnWndProc: Some(connbar_proc)`; `CreateWindowExW(WS_EX_TOPMOST |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class, …, WS_POPUP, …)`; child
//!   controls via `CreateWindowExW(…, w!("BUTTON"), …, Some(hwnd),
//!   Some(HMENU(ID_PIN as *mut core::ffi::c_void)), Some(hinstance), None)`;
//!   `WM_COMMAND` dispatched on `wparam.0 & 0xffff` with `ID_*` constants; all
//!   other messages → `DefWindowProcW`. Buttons are created with
//!   `WS_CHILD | WS_VISIBLE` and pixel positions; `ShowWindow(hwnd,
//!   SW_SHOWNOACTIVATE)`; `Drop` → `DestroyWindow`.
//! - Top-level `win::run()` (main.rs:2956) is the no-host demo loop the hub
//!   replaces: create window → `loop { match window.pump() { Frame::Quit =>
//!   break, Frame::Continue { resize } => … } }`.
//!
//! ## 5. DPAPI protect/unprotect (`src/token_cache.rs`)
//!
//! - `pub(crate) fn dpapi_protect(plain: &[u8]) -> io::Result<Vec<u8>>`
//!   (token_cache.rs:57) — `CryptProtectData` (per-user scope, `CRYPTPROTECT_UI_FORBIDDEN`
//!   semantics via flags 0), blob freed with `LocalFree`.
//! - `pub(crate) fn dpapi_unprotect(blob: &[u8]) -> io::Result<Vec<u8>>`
//!   (token_cache.rs:75) — `CryptUnprotectData`.
//! - Both are already `pub(crate)` (not private), so any module in `rdp-client`
//!   can call them — no visibility change needed. `password_cache.rs` already
//!   re-exports them: `use crate::token_cache::{dpapi_protect, dpapi_unprotect};`.
//!   The hub re-exports them below so `hub::model` / `hub::store` reach them
//!   through one path. Errors surface as `io::Error::other("CryptProtectData: …")`
//!   — wrap into [`HubError::Dpapi`] for storage-layer propagation.
//!
//! ## 6. The LOCALAPPDATA path helper (`src/token_cache.rs`,
//!    `src/password_cache.rs`)
//!
//! Both files define a private `fn cache_path() -> Option<PathBuf>` with the
//! same shape — the hub's store MUST reuse this convention, not invent a new
//! location:
//! ```text
//! std::env::var("LOCALAPPDATA").ok().filter(|s| !s.is_empty())?
//!     → PathBuf::from(local).join("rdpio")
//!     → std::fs::create_dir_all(&dir)
//!     → dir.join(CACHE_FILE)
//! ```
//! - `token_cache.rs` → `w365_token.bin`; `password_cache.rs` →
//!   `w365_password.bin`. The hub will add `connections.json` and `history.json`
//!   in the same `%LOCALAPPDATA%\rdpio\` directory.
//!
//! ## 7. Interactive password prompt (`src/prompt.rs`)
//!
//! - `pub fn read_password(prompt: &str) -> io::Result<String>` — hidden console
//!   read (`SetConsoleMode` clears `ENABLE_ECHO_INPUT` on Windows), strips
//!   trailing CR/LF. MRU entries without a saved password fall back to this
//!   (per step-6) exactly like `resolve_rdstls_password` does today.
//!
//! ## 8. Missing Cargo dependencies (audit result — step-2 fixes these)
//!
//! - Already present in `[workspace.dependencies]`: `thiserror` 1.0, `tracing`
//!   0.1, `serde_json` 1.0, `windows` 0.62 (feature-gated per crate), `ureq`,
//!   `url`, `http`, `mimalloc`.
//! - **Missing** for the hub's model/store: `serde` (with `derive`) and `uuid`
//!   (`v4` + `serde`). Both already exist in `Cargo.lock` as transitive deps
//!   (via serde_json / webview2-com), so adding them as direct workspace deps
//!   resolves without version churn. No UI framework is added — the hub uses the
//!   existing `windows` crate surface like `connbar.rs`/`window.rs`.

mod error;

// The hub data model (step-3): serde-serializable records, DPAPI-protected
// password, MRU history, and the connection target. Its items are exercised by
// the unit tests in this file today; `hub::store` (step-4) and the hub UI
// (steps 5-8) consume them next, so silence the not-yet-used dead-code
// warnings rather than shipping a noisy build.
#[allow(dead_code)]
pub mod model;
// The JSON persistence layer (step-4): ConnectionStore + MruStore under
// %LOCALAPPDATA%\rdpio\. Consumed by the hub UI steps (5-7, 10) and the MRU
// recording hook (step-9); exercised by its unit tests today.
#[allow(dead_code)]
pub mod store;

// The native Win32 hub window (step-5): the connection picker with the slim
// activity rail and switchable content panes. `run()` below owns the window
// and its message loop; step-8 wires main.rs to call it when no connection
// target args are given.
mod ui;

// The dark-theme palette and spacing/DPI helpers (step-2): the single source
// of truth for every hub UI color; ui.rs paints from these constants, never
// from raw color literals. Derived constants (HOVER_BG, SELECTION_BG, GRID,
// scale, …) are consumed by the steps that follow (3-8); like `model` and
// `store` above, silence the not-yet-used warnings rather than shipping a
// noisy build.
#[allow(dead_code)]
mod theme;

// The items below are the public API surface the later hub steps (ui) consume.
// Nothing in the binary references the re-exports yet, so silence the
// not-yet-used warnings rather than shipping a noisy build.
#[allow(unused_imports)]
pub use model::{ConnectionRecord, ConnectionTarget, MruRecord, ProtectedPassword};
#[allow(unused_imports)]
pub use store::{ConnectionInput, ConnectionStore, MruStore};
#[allow(unused_imports)]
pub use store::{format_local_short_datetime, format_recent_identity};

// Error type shared by every fallible hub operation (model, store, ui). Used by
// `hub::model` today and by the store/UI steps after it.
pub use error::HubError;

// Per-monitor DPI awareness is set at the top of [`run`] before any window or
// font exists. `Win32_UI_HiDpi` is already enabled in this crate's `windows`
// feature list (the connect window uses the same pair via `window.rs`).
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// Open the hub window and pump its message loop until the user closes it or
/// selects a connection.
///
/// With the keep-open pin OFF (the pre-step-6 behavior) selecting a
/// Recent/Saved/New connection returns that [`ConnectionTarget`] so `main()`
/// runs the session in-process. With the pin ON the hub launches the session
/// as a detached child process and keeps pumping — so `run()` returns `None`
/// whenever the user closes the hub window, and `Some(target)` only on the
/// keep-open-OFF selection path (or its in-process fallback). Called by
/// `main()` when no connection-target args are given.
pub fn run() -> Result<Option<ConnectionTarget>, HubError> {
    // Per-monitor DPI awareness must be requested before any HWND or GDI font
    // is created: the window class and every child control inherit the process
    // DPI mode, and the hub lays out on an 8px grid scaled by the window DPI.
    // Best-effort — on Windows releases older than 10 1607 the call fails
    // harmlessly and we keep the default awareness so the hub still opens.
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        {
            tracing::warn!(
                error = %e,
                "could not set per-monitor V2 DPI awareness; hub will use system DPI"
            );
        }
    }
    // Load the user's keep-open preference once at startup: the rail pin button
    // is initialized from it, and toggling the pin persists back to the same
    // Settings store. Missing/unset LOCALAPPDATA or a missing, unreadable, or
    // corrupt settings file falls back to [`store::Settings::default`]
    // (keep-hub-open ON), so a fresh install keeps the hub open after connect
    // with no configuration and a bad file can never panic the hub.
    let keep_hub_open = match store::settings_path() {
        Ok(path) => store::load_settings(&path).keep_hub_open,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not resolve settings path; using default keep-open=ON"
            );
            store::Settings::default().keep_hub_open
        }
    };
    ui::run(keep_hub_open)
}

/// Launch a detached child `rdpio` process for `target` and return immediately.
///
/// The child is spawned from the current executable (`std::env::current_exe()`)
/// with `crate::target_to_args(target)` appended as argv, so the child's
/// `Args::from_env()` re-parses the exact connection the hub selected and runs
/// the normal `win::run_connected` session path. On Windows the child is
/// created with `DETACHED_PROCESS` (0x00000008) so it gets no console of its
/// own and is not tied to the hub's window or the parent console's lifetime;
/// the resulting `Child` handle is dropped without waiting, keeping the hub
/// message loop responsive while the session runs detached.
///
/// Invoked by the hub's select-a-connection points (step-6-wire-keep-open)
/// when the keep-open pin is ON: the child makes the same MRU record at its
/// connect-success point that the in-process path makes, so the Recent list
/// stays current either way. The child's argv carries no hub-only metadata
/// (display name / saved-record link), so `record_mru` in the child falls back
/// to host-as-display-name and no connection link — the price of keeping
/// `target_to_args` the exact inverse of `parse_connection_args` and `main.rs`
/// unchanged.
pub fn spawn_connection_child(target: &ConnectionTarget) -> std::io::Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(crate::target_to_args(target));
    #[cfg(windows)]
    {
        // DETACHED_PROCESS: the child has no console window and survives on its
        // own; `CommandExt` is the std Windows extension trait that adds
        // `creation_flags` to `std::process::Command`.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008);
    }
    // Spawn and drop the handle: we intentionally never wait on the child, so
    // the hub keeps pumping messages while the session runs detached.
    cmd.spawn()?;
    Ok(())
}

// Thin re-export so hub submodules (model, store) reach DPAPI through one path;
// the functions are already `pub(crate)` in token_cache.rs — this is a
// convenience alias, not a visibility change. `hub::model` uses it today.
pub(crate) use crate::token_cache::{dpapi_protect, dpapi_unprotect};
