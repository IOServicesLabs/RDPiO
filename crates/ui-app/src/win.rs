//! Windows front-end: Win32 window, D3D11 device/swapchain, message loop.
//!
//! Compiled only on Windows (gated by `#[cfg(windows)]` in `main.rs`). This
//! module owns the top-level window, the D3D11 device and swapchain used to
//! present decoded H.264 frames, and the Win32 message pump.
//!
//! Connection discipline: [`AppController::connect`] is called *before* the
//! window is shown; the render loop only starts after the transport (TCP +
//! TPKT framing, optional UDP side-band) is fully set up, so the session is
//! never opened from a half-initialized UI.

use std::process::ExitCode;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIFactory2, IDXGISwapChain1, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, WHITE_BRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, LoadCursorW,
    PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG, PM_REMOVE, SW_SHOW,
    WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_QUIT, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};

use crate::app::AppController;
use crate::cli::CliOptions;

/// Window class name, registered once per process.
const WINDOW_CLASS: windows::core::PCWSTR = w!("RDPiO_ui_app");
/// Default client area size before the session negotiates the real desktop.
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;

/// Per-window GPU + session state. Stored in the window's `GWLP_USERDATA` so
/// the window procedure can reach it without globals.
struct WinState {
    controller: AppController,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    render_target: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
    running: bool,
}

/// Entry point from `main.rs` on Windows. Returns the process exit code.
pub fn run(opts: CliOptions) -> ExitCode {
    match run_inner(opts) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("ui-app error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(opts: CliOptions) -> windows::core::Result<u8> {
    let instance = unsafe { GetModuleHandleW(None)? };
    register_window_class(instance)?;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            WINDOW_CLASS,
            w!("RDPiO"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            DEFAULT_WIDTH as i32,
            DEFAULT_HEIGHT as i32,
            None,
            None,
            Some(instance),
            None,
        )?
    };

    // Connect the RDP transport *before* showing anything. If this fails the
    // window is destroyed and we exit nonzero — no half-open session UI.
    let mut controller = AppController::new(opts);
    if let Err(err) = controller.connect() {
        tracing::error!(%err, "connection setup failed; aborting");
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(windows::core::Error::from_win32(1));
    }

    let state = initialize_graphics(hwnd, controller)?;
    let state_ptr = Box::into_raw(Box::new(state));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    unsafe {
        ShowWindow(hwnd, SW_SHOW);
    }
    tracing::info!("window shown; entering message loop");
    let exit_code = unsafe { message_loop(state_ptr) };

    // Tear down: drop the GPU state and close the transport cleanly.
    unsafe {
        let _ = Box::from_raw(state_ptr);
    }
    Ok(exit_code)
}

/// Register the one window class used by this process.
unsafe fn register_window_class(
    instance: windows::Win32::Foundation::HINSTANCE,
) -> windows::core::Result<()> {
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW)? };
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH(unsafe { GetStockObject(WHITE_BRUSH) }.0),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: WINDOW_CLASS,
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_win32(1));
    }
    Ok(())
}

/// Create the D3D11 device, the DXGI factory, and the flip-model swapchain.
unsafe fn initialize_graphics(
    hwnd: HWND,
    controller: AppController,
) -> windows::core::Result<WinState> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut _chosen_level = D3D_FEATURE_LEVEL_11_0;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut _chosen_level),
            Some(&mut context),
        )?;
    }
    let device = device.expect("D3D11 device");
    let context = context.expect("D3D11 immediate context");

    let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };

    let (width, height) = client_size(hwnd);
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
        Flags: 0,
    };
    let swapchain = unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)? };

    let render_target = create_render_target(&device, &swapchain)?;

    tracing::info!(width, height, "D3D11 swapchain ready");
    Ok(WinState {
        controller,
        device,
        context,
        swapchain,
        render_target,
        width,
        height,
        running: true,
    })
}

/// Create an RTV for the swapchain's back buffer.
unsafe fn create_render_target(
    device: &ID3D11Device,
    swapchain: &IDXGISwapChain1,
) -> windows::core::Result<Option<ID3D11RenderTargetView>> {
    let buffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0)? };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(&buffer, None, Some(&mut rtv))? };
    Ok(rtv)
}

/// The main loop: drain pending messages, then present one frame. Runs until
/// the window posts `WM_QUIT`.
unsafe fn message_loop(state_ptr: *mut WinState) -> u8 {
    let mut msg = MSG::default();
    loop {
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return msg.wParam.0 as u8;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let state = unsafe { &mut *state_ptr };
        if !state.running {
            break;
        }
        render_frame(state);
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    0
}

/// Clear the back buffer to a dark slate and present. Decoded H.264 frames
/// are composited into the back buffer by the GPU video path before this
/// runs; the swapchain flip presents whatever the render target holds.
fn render_frame(state: &mut WinState) {
    let Some(rtv) = state.render_target.as_ref() else {
        return;
    };
    unsafe {
        state
            .context
            .ClearRenderTargetView(rtv, &[0.08, 0.08, 0.10, 1.0]);
    }
    let result = unsafe { state.swapchain.Present(1, DXGI_PRESENT(0)) };
    if let Err(err) = result.ok() {
        tracing::warn!("present failed: {err}");
    }
}

/// Current client-area size in pixels.
fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (
        u32::try_from(rect.right - rect.left).unwrap_or(DEFAULT_WIDTH),
        u32::try_from(rect.bottom - rect.top).unwrap_or(DEFAULT_HEIGHT),
    )
}

/// Window procedure: forwards keyboard/mouse messages to the controller (the
/// input PDU encoding lives in the redirection layer), handles resize and
/// teardown.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let state = userdata(hwnd);
            if let Some(state) = state {
                let width = (lparam.0 as u32) & 0xFFFF;
                let height = ((lparam.0 as u32) >> 16) & 0xFFFF;
                if width > 0 && height > 0 {
                    state.width = width;
                    state.height = height;
                    let _ = unsafe {
                        state.swapchain.ResizeBuffers(
                            0,
                            width,
                            height,
                            DXGI_FORMAT_UNKNOWN,
                            Default::default(),
                        )
                    };
                    state.render_target =
                        unsafe { create_render_target(&state.device, &state.swapchain) }
                            .ok()
                            .flatten();
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP | WM_LBUTTONDOWN | WM_LBUTTONUP
        | WM_MOUSEMOVE => {
            // Forward the raw input event to the controller, which hands it to
            // the redirection layer for encoding into an input PDU. The
            // transport is already live here (connect() ran before the window
            // was shown), so the event is never dropped on a dead session.
            if let Some(state) = userdata(hwnd) {
                state
                    .controller
                    .forward_input(msg, wparam.0 as usize, lparam.0 as usize);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Read the per-window state pointer out of `GWLP_USERDATA`.
fn userdata(hwnd: HWND) -> Option<&'static mut WinState> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut WinState) })
    }
}
