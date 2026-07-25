//! Direct3D 11 renderer (Windows).

use std::collections::HashMap;

use windows::core::{Interface, PCSTR, Result as WinResult};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, RECT};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_QUERY, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
    D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_VIEWPORT,
};

use crate::Upscaler;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED, DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIFactory6, IDXGISwapChain,
    IDXGISwapChain1, IDXGISwapChain2, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// Lightweight D3D11 timestamp-query pair for one GPU operation per frame.
/// D3D11 timestamps are asynchronous: `begin`/`end` issue commands, and
/// `collect` reads back the *previous* frame's completed queries.
struct GpuTimer {
    context: ID3D11DeviceContext,
    start: ID3D11Query,
    end: ID3D11Query,
    disjoint: ID3D11Query,
    pending: bool,
}

impl GpuTimer {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> WinResult<Self> {
        let start = create_query(device, D3D11_QUERY_TIMESTAMP)?;
        let end = create_query(device, D3D11_QUERY_TIMESTAMP)?;
        let disjoint = create_query(device, D3D11_QUERY_TIMESTAMP_DISJOINT)?;
        Ok(Self {
            context: context.clone(),
            start,
            end,
            disjoint,
            pending: false,
        })
    }

    fn begin(&mut self) {
        unsafe { self.context.End(&self.start) };
        self.pending = true;
    }

    fn end(&mut self) {
        unsafe {
            self.context.End(&self.end);
            self.context.End(&self.disjoint);
        }
    }

    /// Read back the previous frame's timing and call `cb(label, microseconds)`
    /// if the data is available and the timestamps are not disjoint.
    fn collect(&mut self, label: &str, cb: &dyn Fn(&str, u64)) {
        if !self.pending {
            return;
        }
        unsafe {
            let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let disjoint_ok = self
                .context
                .GetData(
                    &self.disjoint,
                    Some(&mut disjoint as *mut _ as *mut _),
                    std::mem::size_of_val(&disjoint) as u32,
                    0,
                )
                .is_ok();
            let mut start: u64 = 0;
            let start_ok = self
                .context
                .GetData(
                    &self.start,
                    Some(&mut start as *mut _ as *mut _),
                    std::mem::size_of::<u64>() as u32,
                    0,
                )
                .is_ok();
            let mut end: u64 = 0;
            let end_ok = self
                .context
                .GetData(
                    &self.end,
                    Some(&mut end as *mut _ as *mut _),
                    std::mem::size_of::<u64>() as u32,
                    0,
                )
                .is_ok();
            if disjoint_ok && !disjoint.Disjoint.as_bool() && start_ok && end_ok {
                let delta = end.saturating_sub(start);
                let us = (delta as f64 / disjoint.Frequency as f64 * 1_000_000.0) as u64;
                cb(label, us);
            }
        }
        self.pending = false;
    }
}

fn create_query(device: &ID3D11Device, query: D3D11_QUERY) -> WinResult<ID3D11Query> {
    let desc = D3D11_QUERY_DESC {
        Query: query,
        MiscFlags: 0,
    };
    unsafe {
        let mut q: Option<ID3D11Query> = None;
        device.CreateQuery(&desc, Some(&mut q))?;
        q.ok_or_else(|| windows::core::Error::from_thread())
    }
}

/// Owns the D3D11 device, immediate context, swapchain, current backbuffer
/// render-target view, and a CPU-updatable desktop framebuffer texture that
/// decoded bitmap rectangles are blitted into before being presented.
pub struct D3D11Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain,
    rtv: Option<ID3D11RenderTargetView>,
    /// Swapchain backbuffer size (client area), kept in sync by `new`/`resize`.
    sc_width: u32,
    sc_height: u32,
    /// Desktop-sized texture the session paints into (RGBA, USAGE_DEFAULT).
    framebuffer: Option<ID3D11Texture2D>,
    fb_width: u32,
    fb_height: u32,
    /// GPU NV12→RGB color converters, one per distinct input size, lazily
    /// created for the H.264 path. Keyed on `(in_w, in_h)` so mixed-resolution
    /// monitors (each an EGFX surface of its own size) don't destroy/recreate a
    /// single shared video processor on every alternating blit. Every converter
    /// outputs to the current framebuffer; [`ensure_framebuffer`](Self::ensure_framebuffer)
    /// clears the map when the framebuffer is reallocated so the cached output
    /// views never dangle.
    videos: HashMap<(u32, u32), VideoConv>,
    /// Latched once the GPU video path is unavailable / failed, so we stop
    /// retrying and fall back to CPU YUV conversion for the rest of the session.
    video_disabled: bool,
    /// GPU RGBA scaler for smart-sizing (desktop → window), lazily created when
    /// the window size differs from the desktop. Used for the `Vsr` and `Bilinear`
    /// upscalers (VideoProcessor, VSR on/off). `None`/failure → crop fallback.
    scaler: Option<RgbaScaler>,
    scaler_disabled: bool,
    /// GPU Catmull-Rom bicubic scaler (custom shader) for the `Bicubic` upscaler —
    /// the default smart-size path. Lazily created; on failure the present path
    /// latches `bicubic_disabled` and falls back to the `Bilinear` `RgbaScaler`.
    bicubic: Option<BicubicScaler>,
    bicubic_disabled: bool,
    /// Which upscaler the smart-size present path uses when the desktop framebuffer
    /// is smaller than the window. Set once at startup via [`Self::set_upscaler`];
    /// default [`Upscaler::Bicubic`].
    upscaler: Upscaler,
    /// The swapchain's frame-latency waitable object (when the chain was created
    /// `FRAME_LATENCY_WAITABLE_OBJECT` with max latency 1). Waiting on it before
    /// each present keeps at most one frame queued, so the displayed frame is at
    /// most ~1 refresh old instead of DXGI's default of up to 3 — the latency
    /// edge that matters for interactive + gaming use.
    frame_wait: Option<HANDLE>,
    /// Whether the swapchain supports `ALLOW_TEARING` (created with the flag).
    /// Required to present without vsync for the lowest possible latency.
    tearing: bool,
    /// Runtime toggle: when set (and `tearing` is available) present tears
    /// instead of waiting for vblank — absolute-minimum-latency "gaming" mode.
    low_latency: bool,
    /// Creation flags, replayed verbatim on `ResizeBuffers` (which must be given
    /// the same flags the chain was created with).
    sc_flags: u32,
    /// EGFX surface cache (slot → its size + a GPU texture), filled by
    /// [`cache_rect`](Self::cache_rect) and replayed by
    /// [`cache_blit`](Self::cache_blit). Lives on the GPU so cached pixels survive
    /// regardless of how they were painted (RGBA, NV12, or DXVA texture).
    gfx_cache: HashMap<u16, (u32, u32, ID3D11Texture2D)>,
    /// Scratch RGBA texture, framebuffer-sized, used to make framebuffer→
    /// framebuffer copies safe even when source/dest rectangles overlap (a
    /// straight self-copy is undefined in D3D11). Rebuilt with the framebuffer.
    copy_scratch: Option<ID3D11Texture2D>,
    /// Per-monitor mode: additional swapchains (one per non-primary monitor)
    /// sharing this device + framebuffer, each presenting its own slice of the
    /// desktop. Empty in the default single-window (spanning / windowed) path.
    extra_targets: Vec<PresentTarget>,
    /// The framebuffer offset the *primary* swapchain presents from. `(0, 0)` in
    /// the single-window path (present the whole framebuffer); set to the primary
    /// monitor's offset within the virtual desktop in per-monitor mode.
    primary_src: (u32, u32),
    /// Optional GPU timing callback; receives `(label, microseconds)` for
    /// completed D3D11 timestamp queries. Wired up by the client to the telemetry
    /// module.
    gpu_timing_cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
    /// Timestamp-query pair for the `Present` call.
    present_timer: Option<GpuTimer>,
}

/// One additional present surface for per-monitor mode: a swapchain bound to a
/// physical monitor's window, presenting a `width`×`height` slice of the shared
/// framebuffer taken from offset `src`. Shares the renderer's D3D11 device.
struct PresentTarget {
    swap_chain: IDXGISwapChain,
    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
    frame_wait: Option<HANDLE>,
    tearing: bool,
    /// Top-left offset of this monitor's slice within the framebuffer.
    src: (u32, u32),
}

impl D3D11Renderer {
    /// The system's highest-performance graphics adapter (the discrete GPU on a
    /// hybrid laptop), via `IDXGIFactory6::EnumAdapterByGpuPreference`. Returns
    /// `None` on any failure — an older DXGI without `IDXGIFactory6`, or no
    /// adapter — so the caller falls back to the runtime's default adapter.
    fn high_performance_adapter() -> Option<IDXGIAdapter> {
        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory
                .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
                .ok()?;
            if let Ok(desc) = adapter.GetDesc() {
                let end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                let name = String::from_utf16_lossy(&desc.Description[..end]);
                tracing::info!(adapter = %name, "selecting high-performance GPU adapter");
            }
            Some(adapter)
        }
    }

    /// Create a device and a low-latency flip-model swapchain for the raw `HWND`.
    ///
    /// We ask for a `FRAME_LATENCY_WAITABLE_OBJECT` chain (and `ALLOW_TEARING`
    /// when the platform supports it) and fall back gracefully — waitable-only,
    /// then a plain flip chain — so an older system never fails to start. The
    /// waitable object plus max-frame-latency 1 is what gives us a tighter
    /// present-to-photon path than the stock DXGI queue every other client uses.
    pub fn new(hwnd_raw: isize, width: u32, height: u32) -> WinResult<Self> {
        unsafe {
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            // BGRA support is required for later Direct2D / Media Foundation interop.
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

            let make_desc = |flags: u32| DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: width,
                    Height: height,
                    RefreshRate: DXGI_RATIONAL {
                        Numerator: 60,
                        Denominator: 1,
                    },
                    // RGBA so decoded bitmap rectangles upload without a swizzle.
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                    Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: true.into(),
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Flags: flags,
            };

            // Prefer the discrete GPU on hybrid (laptop) systems: the default
            // adapter is often the power-saving integrated GPU, which decodes
            // H.264 and runs the video processor far slower. `None` → fall back
            // to the runtime's default adapter (driver type HARDWARE). When an
            // explicit adapter is supplied, the driver type must be UNKNOWN.
            let hp_adapter = Self::high_performance_adapter();
            let driver_type = if hp_adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            };

            // Attempt creation with a given flag set. Each call builds its own
            // device/context so a failed attempt leaves nothing behind to undo.
            let attempt = |flags: u32| -> WinResult<(
                ID3D11Device,
                ID3D11DeviceContext,
                IDXGISwapChain,
                D3D_FEATURE_LEVEL,
            )> {
                let desc = make_desc(flags);
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                let mut swap_chain: Option<IDXGISwapChain> = None;
                let mut obtained: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;
                D3D11CreateDeviceAndSwapChain(
                    hp_adapter.as_ref(),
                    driver_type,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&desc),
                    Some(&mut swap_chain),
                    Some(&mut device),
                    Some(&mut obtained),
                    Some(&mut context),
                )?;
                Ok((
                    device.expect("device on success"),
                    context.expect("context on success"),
                    swap_chain.expect("swapchain on success"),
                    obtained,
                ))
            };

            let waitable = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            let tearing_flag = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            // Best → good → safe: waitable+tearing, waitable, plain flip.
            let (device, context, swap_chain, obtained, sc_flags, tearing) =
                match attempt(waitable | tearing_flag) {
                    Ok((d, c, s, o)) => (d, c, s, o, waitable | tearing_flag, true),
                    Err(_) => match attempt(waitable) {
                        Ok((d, c, s, o)) => (d, c, s, o, waitable, false),
                        Err(_) => {
                            let (d, c, s, o) = attempt(0)?;
                            (d, c, s, o, 0u32, false)
                        }
                    },
                };

            // If we got a waitable chain, cap the queue to one frame and grab the
            // wait handle. Any failure here just disables the waitable path.
            let mut frame_wait = None;
            if sc_flags & waitable != 0 {
                if let Ok(sc2) = swap_chain.cast::<IDXGISwapChain2>() {
                    let _ = sc2.SetMaximumFrameLatency(1);
                    let h = sc2.GetFrameLatencyWaitableObject();
                    if !h.is_invalid() {
                        frame_wait = Some(h);
                    }
                }
            }

            let mut renderer = Self {
                device,
                context,
                swap_chain,
                rtv: None,
                sc_width: width,
                sc_height: height,
                framebuffer: None,
                fb_width: 0,
                fb_height: 0,
                videos: HashMap::new(),
                video_disabled: false,
                scaler: None,
                scaler_disabled: false,
                bicubic: None,
                bicubic_disabled: false,
                upscaler: Upscaler::default(),
                frame_wait,
                tearing,
                low_latency: false,
                sc_flags,
                gfx_cache: HashMap::new(),
                copy_scratch: None,
                extra_targets: Vec::new(),
                primary_src: (0, 0),
                gpu_timing_cb: None,
                present_timer: None,
            };
            renderer.create_rtv()?;
            // Enable multithread protection so the session worker can use the
            // device (DXVA decode + texture copies) while the UI thread presents.
            if let Ok(mt) = renderer.device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            tracing::info!(
                ?obtained,
                width,
                height,
                waitable = frame_wait.is_some(),
                tearing,
                "D3D11 device + swapchain created"
            );
            Ok(renderer)
        }
    }

    /// Enable (or disable) the no-vsync tearing present path — absolute-minimum
    /// latency for gaming. A no-op (with a warning) when the swapchain doesn't
    /// support tearing; the default is smooth vsync, which suits desktop work.
    pub fn set_low_latency(&mut self, on: bool) {
        if on && !self.tearing {
            tracing::warn!("low-latency tearing present requested but unsupported; using vsync");
        }
        self.low_latency = on && self.tearing;
        tracing::info!(low_latency = self.low_latency, "present mode set");
    }

    /// Choose the GPU upscaler used when the remote desktop is rendered smaller
    /// than the window (`--render-scale`) and scaled up on present. Set once at
    /// startup; the actual scaler is built lazily on the first scaled present (and
    /// rebuilt on resize). Any scaler from a previous mode is dropped here so the
    /// next present rebuilds with the new one.
    pub fn set_upscaler(&mut self, mode: Upscaler) {
        self.upscaler = mode;
        self.scaler = None;
        self.scaler_disabled = false;
        self.bicubic = None;
        self.bicubic_disabled = false;
        tracing::info!(?mode, "client upscaler selected");
    }

    /// Install a callback that receives `(label, microseconds)` for completed
    /// GPU timestamp queries. Pass `None` to disable timing.
    pub fn set_gpu_timing_callback(
        &mut self,
        cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
    ) {
        self.gpu_timing_cb = cb;
        self.present_timer = None;
    }

    fn ensure_present_timer(&mut self) -> WinResult<&mut GpuTimer> {
        if self.present_timer.is_none() {
            self.present_timer = Some(GpuTimer::new(&self.device, &self.context)?);
        }
        Ok(self.present_timer.as_mut().unwrap())
    }

    /// Wait on the frame-latency object (if any) so we never queue more than one
    /// frame ahead of the display. Bounded so a lost/abandoned signal can't hang
    /// the UI thread.
    fn wait_for_frame(&self) {
        if let Some(h) = self.frame_wait {
            // Non-alertable: only the frame signal (or the 100 ms guard) wakes us,
            // so an APC can't slip an extra frame past the latency cap.
            unsafe {
                let _ = WaitForSingleObjectEx(h, 100, false);
            }
        }
    }

    /// The `(SyncInterval, Flags)` pair for `Present`: tear with no vsync in
    /// low-latency mode, otherwise sync to vblank.
    fn present_params(&self) -> (u32, DXGI_PRESENT) {
        if self.low_latency && self.tearing {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (1, DXGI_PRESENT(0))
        }
    }

    fn create_rtv(&mut self) -> WinResult<()> {
        unsafe {
            let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))?;
            self.rtv = rtv;
            Ok(())
        }
    }

    /// Resize the swapchain backbuffers to match the client area.
    pub fn resize(&mut self, width: u32, height: u32) -> WinResult<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        unsafe {
            // The RTV must be released before resizing the buffers.
            self.rtv = None;
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                // ResizeBuffers must be given the same flags the chain was
                // created with, or it silently drops the waitable/tearing modes.
                DXGI_SWAP_CHAIN_FLAG(self.sc_flags as i32),
            )?;
            self.sc_width = width;
            self.sc_height = height;
            // The scalers were sized for the old backbuffer; rebuild on demand.
            self.scaler = None;
            self.bicubic = None;
            self.create_rtv()
        }
    }

    /// Clear the backbuffer to an RGBA color and present. Used before the first
    /// server frame arrives (and as the idle background).
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> WinResult<()> {
        self.wait_for_frame();
        unsafe {
            if let Some(rtv) = self.rtv.as_ref() {
                self.context.ClearRenderTargetView(rtv, &rgba);
            }
            let (sync, flags) = self.present_params();
            self.swap_chain.Present(sync, flags).ok()?;
            // Clear and present every per-monitor target too, so secondary
            // monitors don't show uninitialised backbuffers before the first frame.
            for t in &self.extra_targets {
                if let Some(h) = t.frame_wait {
                    let _ = WaitForSingleObjectEx(h, 100, false);
                }
                if let Some(rtv) = t.rtv.as_ref() {
                    self.context.ClearRenderTargetView(rtv, &rgba);
                }
                let (sync, flags) = if self.low_latency && t.tearing {
                    (0, DXGI_PRESENT_ALLOW_TEARING)
                } else {
                    (1, DXGI_PRESENT(0))
                };
                let _ = t.swap_chain.Present(sync, flags);
            }
            Ok(())
        }
    }

    /// Set the framebuffer offset the primary swapchain presents from (per-monitor
    /// mode). The default `(0, 0)` presents the whole framebuffer (single window).
    pub fn set_primary_src(&mut self, x: u32, y: u32) {
        self.primary_src = (x, y);
    }

    /// Add a per-monitor present target: a new swapchain on `hwnd_raw`'s window
    /// (sharing this device + framebuffer) that presents a `width`×`height` slice
    /// of the framebuffer taken from `(src_x, src_y)`. Used to drive one window
    /// per physical monitor over a single spanned remote desktop.
    pub fn add_present_target(
        &mut self,
        hwnd_raw: isize,
        width: u32,
        height: u32,
        src_x: u32,
        src_y: u32,
    ) -> WinResult<()> {
        unsafe {
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            let (swap_chain, frame_wait, flags) =
                self.build_swapchain_for_hwnd(hwnd, width, height)?;
            let tearing = flags & (DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32) != 0;
            let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))?;
            tracing::info!(width, height, src_x, src_y, tearing, "per-monitor present target added");
            self.extra_targets.push(PresentTarget {
                swap_chain,
                rtv,
                width,
                height,
                frame_wait,
                tearing,
                src: (src_x, src_y),
            });
            Ok(())
        }
    }

    /// Build a flip-model swapchain for `hwnd` on the existing device (so it
    /// shares the framebuffer and decode device). Mirrors `new`'s
    /// best→good→safe flag fallback (waitable+tearing → waitable → plain).
    unsafe fn build_swapchain_for_hwnd(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> WinResult<(IDXGISwapChain, Option<HANDLE>, u32)> {
        let dxgi_device: IDXGIDevice = self.device.cast()?;
        let adapter = dxgi_device.GetAdapter()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;
        let waitable = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        let tearing_flag = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        let make = |flags: u32| -> WinResult<IDXGISwapChain1> {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                Flags: flags,
            };
            factory.CreateSwapChainForHwnd(&self.device, hwnd, &desc, None, None)
        };
        let (sc1, flags) = match make(waitable | tearing_flag) {
            Ok(s) => (s, waitable | tearing_flag),
            Err(_) => match make(waitable) {
                Ok(s) => (s, waitable),
                Err(_) => (make(0)?, 0u32),
            },
        };
        let mut frame_wait = None;
        if flags & waitable != 0 {
            if let Ok(sc2) = sc1.cast::<IDXGISwapChain2>() {
                let _ = sc2.SetMaximumFrameLatency(1);
                let h = sc2.GetFrameLatencyWaitableObject();
                if !h.is_invalid() {
                    frame_wait = Some(h);
                }
            }
        }
        let sc: IDXGISwapChain = sc1.cast()?;
        Ok((sc, frame_wait, flags))
    }

    /// Present one monitor's slice: copy the `w`×`h` framebuffer region at
    /// `(src_x, src_y)` into `swap_chain`'s backbuffer 1:1 (no scale — the window
    /// matches the monitor's pixels), then present.
    #[allow(clippy::too_many_arguments)]
    unsafe fn present_slice(
        &self,
        swap_chain: &IDXGISwapChain,
        frame_wait: Option<HANDLE>,
        tearing: bool,
        w: u32,
        h: u32,
        src_x: u32,
        src_y: u32,
    ) -> WinResult<()> {
        if let Some(handle) = frame_wait {
            let _ = WaitForSingleObjectEx(handle, 100, false);
        }
        if let Some(fb) = self.framebuffer.as_ref() {
            let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let right = (src_x + w).min(self.fb_width);
            let bottom = (src_y + h).min(self.fb_height);
            if right > src_x && bottom > src_y {
                let src_box = D3D11_BOX {
                    left: src_x,
                    top: src_y,
                    front: 0,
                    right,
                    bottom,
                    back: 1,
                };
                self.context
                    .CopySubresourceRegion(&back_buffer, 0, 0, 0, 0, fb, 0, Some(&src_box));
            }
        }
        let (sync, flags) = if self.low_latency && tearing {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (1, DXGI_PRESENT(0))
        };
        swap_chain.Present(sync, flags).ok()
    }

    /// Allocate (or reallocate) the desktop framebuffer to `width`x`height`.
    /// Called once the negotiated desktop size is known. A no-op if the size
    /// is unchanged, so it is cheap to call defensively.
    pub fn ensure_framebuffer(&mut self, width: u32, height: u32) -> WinResult<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        if self.framebuffer.is_some() && self.fb_width == width && self.fb_height == height {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // SHADER_RESOURCE for present's copy path; RENDER_TARGET so the GPU
            // video processor can write decoded frames straight into it.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            // A fresh DEFAULT-usage texture has undefined contents; clear it so a
            // desktop resize shows black until repainted, not GPU memory garbage.
            if let Some(t) = &tex {
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                if self
                    .device
                    .CreateRenderTargetView(t, None, Some(&mut rtv))
                    .is_ok()
                {
                    if let Some(rtv) = rtv.as_ref() {
                        self.context
                            .ClearRenderTargetView(rtv, &[0.0f32, 0.0, 0.0, 1.0]);
                    }
                }
            }
            self.framebuffer = tex;
        }
        self.fb_width = width;
        self.fb_height = height;
        // The video processors' output views + sizing were bound to the old
        // framebuffer; drop them all so they rebuild against the new one. The copy
        // scratch is framebuffer-sized, so drop it too.
        self.videos.clear();
        self.scaler = None;
        self.bicubic = None;
        self.copy_scratch = None;
        tracing::debug!(width, height, "framebuffer texture (re)allocated");
        Ok(())
    }

    /// Upload one decoded RGBA rectangle into the framebuffer at (`x`,`y`).
    /// `rgba` must be tightly packed (`w*h*4` bytes, row stride `w*4`). The
    /// destination box is clamped to the framebuffer; rectangles fully outside
    /// it are dropped. Does not present — call [`present_frame`] to show it.
    pub fn update_rect(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        // Lazily size the framebuffer to contain this rect if we have not yet
        // learned the desktop size (defensive — the session normally calls
        // ensure_framebuffer with the negotiated dimensions first).
        if self.framebuffer.is_none() {
            let _ =
                self.ensure_framebuffer((x as u32 + w as u32).max(1), (y as u32 + h as u32).max(1));
        }
        let (Some(fb), x, y, w, h) = (
            self.framebuffer.as_ref(),
            x as u32,
            y as u32,
            w as u32,
            h as u32,
        ) else {
            return;
        };
        if w == 0 || h == 0 || x >= self.fb_width || y >= self.fb_height {
            return;
        }
        let row_pitch = w * 4;
        if (rgba.len() as u32) < row_pitch * h {
            tracing::warn!(
                have = rgba.len(),
                need = row_pitch * h,
                "short bitmap buffer; dropping rect"
            );
            return;
        }
        // Clamp the destination box to the framebuffer. The source stride stays
        // `w*4`, so clipping just copies fewer pixels per row / fewer rows.
        let right = (x + w).min(self.fb_width);
        let bottom = (y + h).min(self.fb_height);
        let dst_box = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right,
            bottom,
            back: 1,
        };
        unsafe {
            self.context.UpdateSubresource(
                fb,
                0,
                Some(&dst_box),
                rgba.as_ptr() as *const core::ffi::c_void,
                row_pitch,
                row_pitch * h,
            );
        }
    }

    /// Create a plain RGBA `w`x`h` GPU texture usable as a copy source/dest
    /// (no bind flags needed for `CopySubresourceRegion`).
    fn create_copy_texture(&self, w: u32, h: u32) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex)).ok()?;
            tex
        }
    }

    /// Copy a `w`x`h` framebuffer rectangle from (`sx`,`sy`) to (`dx`,`dy`),
    /// entirely on the GPU (EGFX SurfaceToSurface). Goes through a framebuffer-
    /// sized scratch texture so overlapping source/dest rectangles (scrolling)
    /// are well-defined. Out-of-bounds rectangles are clamped; nothing is
    /// presented until [`present_frame`](Self::present_frame).
    pub fn copy_rect(&mut self, sx: u16, sy: u16, w: u16, h: u16, dx: u16, dy: u16) {
        let Some(fb) = self.framebuffer.clone() else {
            return;
        };
        let (sx, sy, dx, dy, w, h) = (
            sx as u32, sy as u32, dx as u32, dy as u32, w as u32, h as u32,
        );
        if w == 0 || h == 0 {
            return;
        }
        if sx >= self.fb_width || sy >= self.fb_height || dx >= self.fb_width || dy >= self.fb_height
        {
            return;
        }
        // Clamp so neither the source nor the destination rectangle leaves the FB.
        let cw = w
            .min(self.fb_width - sx)
            .min(self.fb_width - dx);
        let ch = h
            .min(self.fb_height - sy)
            .min(self.fb_height - dy);
        if cw == 0 || ch == 0 {
            return;
        }
        if self.copy_scratch.is_none() {
            self.copy_scratch = self.create_copy_texture(self.fb_width, self.fb_height);
        }
        let Some(scratch) = self.copy_scratch.clone() else {
            return;
        };
        unsafe {
            let src_box = D3D11_BOX {
                left: sx,
                top: sy,
                front: 0,
                right: sx + cw,
                bottom: sy + ch,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&scratch, 0, 0, 0, 0, &fb, 0, Some(&src_box));
            let scratch_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: cw,
                bottom: ch,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&fb, 0, dx, dy, 0, &scratch, 0, Some(&scratch_box));
        }
    }

    /// Stash a `w`x`h` framebuffer rectangle from (`sx`,`sy`) into GPU cache
    /// `slot` (EGFX SurfaceToCache).
    pub fn cache_rect(&mut self, slot: u16, sx: u16, sy: u16, w: u16, h: u16) {
        let Some(fb) = self.framebuffer.clone() else {
            return;
        };
        let (sx, sy, w, h) = (sx as u32, sy as u32, w as u32, h as u32);
        if w == 0 || h == 0 || sx >= self.fb_width || sy >= self.fb_height {
            return;
        }
        let cw = w.min(self.fb_width - sx);
        let ch = h.min(self.fb_height - sy);
        let Some(tex) = self.create_copy_texture(cw, ch) else {
            return;
        };
        unsafe {
            let src_box = D3D11_BOX {
                left: sx,
                top: sy,
                front: 0,
                right: sx + cw,
                bottom: sy + ch,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&tex, 0, 0, 0, 0, &fb, 0, Some(&src_box));
        }
        self.gfx_cache.insert(slot, (cw, ch, tex));
    }

    /// Blit GPU cache `slot` onto the framebuffer at (`dx`,`dy`) (EGFX
    /// CacheToSurface). A miss (unknown slot) is a no-op.
    pub fn cache_blit(&mut self, slot: u16, dx: u16, dy: u16) {
        let Some(fb) = self.framebuffer.clone() else {
            return;
        };
        let Some((cw, ch, tex)) = self
            .gfx_cache
            .get(&slot)
            .map(|(w, h, t)| (*w, *h, t.clone()))
        else {
            return;
        };
        let (dx, dy) = (dx as u32, dy as u32);
        if dx >= self.fb_width || dy >= self.fb_height {
            return;
        }
        let cw = cw.min(self.fb_width - dx);
        let ch = ch.min(self.fb_height - dy);
        if cw == 0 || ch == 0 {
            return;
        }
        unsafe {
            let src_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: cw,
                bottom: ch,
                back: 1,
            };
            self.context
                .CopySubresourceRegion(&fb, 0, dx, dy, 0, &tex, 0, Some(&src_box));
        }
    }

    /// Disable the GPU NV12 path (forces CPU YUV conversion). Used by the
    /// `--cpu-yuv` safety valve.
    pub fn disable_gpu_yuv(&mut self) {
        self.video_disabled = true;
        self.videos.clear();
    }

    /// Whether the GPU NV12 color-conversion path is currently usable. Callers
    /// check this to decide whether to hand NV12 to [`blit_nv12`] or convert on
    /// the CPU themselves.
    pub fn gpu_yuv_available(&self) -> bool {
        !self.video_disabled
    }

    /// Convert an NV12 frame to RGB on the GPU (Direct3D 11 video processor) and
    /// write it into the framebuffer at `(dest_x, dest_y)`. `nv12` is tightly
    /// packed: a `w*h` Y plane followed by a `w*(h/2)` interleaved UV plane.
    ///
    /// Returns `false` (without painting) if the GPU path is unavailable or
    /// fails — the caller must then fall back to CPU conversion + [`update_rect`]
    /// so the picture is never lost. Once a hard failure occurs the GPU path is
    /// latched off for the rest of the session.
    pub fn blit_nv12(&mut self, dest_x: u32, dest_y: u32, w: u32, h: u32, nv12: &[u8]) -> bool {
        if self.video_disabled {
            return false;
        }
        // NV12 requires even dimensions, and we need the whole frame present.
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
            return false;
        }
        if (nv12.len() as u32) < w * h + w * (h / 2) {
            return false;
        }
        if self.framebuffer.is_none() {
            let _ = self.ensure_framebuffer((dest_x + w).max(1), (dest_y + h).max(1));
        }
        let (fb_w, fb_h) = (self.fb_width, self.fb_height);
        let Some(fb) = self.framebuffer.clone() else {
            return false;
        };
        if fb_w == 0 || fb_h == 0 {
            return false;
        }
        // Look up (or lazily create) the converter for this input size. The
        // processor's content description is size-specific, so one converter is
        // cached per distinct frame size instead of recreating a shared one
        // whenever the input size alternates between monitors.
        if !self.videos.contains_key(&(w, h)) {
            match VideoConv::new(&self.device, &self.context, w, h, fb_w, fb_h) {
                Ok(v) => {
                    self.videos.insert((w, h), v);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "GPU video processor unavailable; using CPU YUV");
                    self.video_disabled = true;
                    return false;
                }
            }
        }
        let v = self.videos.get_mut(&(w, h)).expect("video present after init");
        match v.blit(&self.device, &self.context, &fb, dest_x, dest_y, w, h, nv12) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "GPU YUV blit failed; falling back to CPU YUV");
                self.video_disabled = true;
                false
            }
        }
    }

    /// Present the framebuffer. When the window matches the desktop size this is
    /// a straight copy. When it differs (the user resized), "smart-sizing"
    /// scales the whole desktop to fit the window via the GPU video processor;
    /// if scaling is unavailable it falls back to a top-left crop copy.
    pub fn present_frame(&mut self) -> WinResult<()> {
        // Collect the previous frame's GPU present timing, then begin timing this one.
        if let Some(cb) = self.gpu_timing_cb.as_ref() {
            if let Some(timer) = self.present_timer.as_mut() {
                timer.collect("present", cb);
            }
        }
        let timing_present = self.gpu_timing_cb.is_some();
        if timing_present {
            let _ = self.ensure_present_timer().map(|t| t.begin());
        }

        if self.framebuffer.is_none() {
            let res = self.present_clear([0.06, 0.09, 0.16, 1.0]);
            if let Some(timer) = self.present_timer.as_mut() {
                timer.end();
            }
            return res;
        }
        // Per-monitor mode: present the primary swapchain's slice plus every
        // extra monitor's slice, each a 1:1 copy of its framebuffer region.
        if !self.extra_targets.is_empty() {
            let res = unsafe {
                let (px, py) = self.primary_src;
                self.present_slice(
                    &self.swap_chain,
                    self.frame_wait,
                    self.tearing,
                    self.sc_width,
                    self.sc_height,
                    px,
                    py,
                )?;
                for t in &self.extra_targets {
                    self.present_slice(
                        &t.swap_chain,
                        t.frame_wait,
                        t.tearing,
                        t.width,
                        t.height,
                        t.src.0,
                        t.src.1,
                    )?;
                }
                Ok(())
            };
            if let Some(timer) = self.present_timer.as_mut() {
                timer.end();
            }
            return res;
        }
        // Block until the swapchain can take a new frame (max latency 1) so we
        // present the freshest possible frame rather than one queued behind two.
        self.wait_for_frame();
        let (fb_w, fb_h) = (self.fb_width, self.fb_height);
        let (sc_w, sc_h) = (self.sc_width, self.sc_height);
        let scaled = (fb_w, fb_h) != (sc_w, sc_h) && self.try_smart_size(fb_w, fb_h, sc_w, sc_h);
        if !scaled {
            // 1:1 (or fallback) copy: clip to the smaller of framebuffer/backbuffer.
            let fb = self.framebuffer.clone().expect("framebuffer present");
            unsafe {
                let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
                let src_box = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: fb_w.min(sc_w),
                    bottom: fb_h.min(sc_h),
                    back: 1,
                };
                self.context
                    .CopySubresourceRegion(&back_buffer, 0, 0, 0, 0, &fb, 0, Some(&src_box));
            }
        }
        let (sync, flags) = self.present_params();
        let res = unsafe { self.swap_chain.Present(sync, flags).ok() };
        if let Some(timer) = self.present_timer.as_mut() {
            timer.end();
        }
        res
    }

    /// Scale the desktop framebuffer onto the full backbuffer with the selected
    /// [`Upscaler`]. Returns `false` (so the caller crops instead) if every usable
    /// scaler is unavailable/fails. `Bicubic` falls back to `Bilinear` on failure
    /// (so a missing shader compiler never loses the picture); both VideoProcessor
    /// modes fall back to the crop copy.
    fn try_smart_size(&mut self, fb_w: u32, fb_h: u32, sc_w: u32, sc_h: u32) -> bool {
        match self.upscaler {
            Upscaler::Bicubic => {
                if !self.bicubic_disabled && self.try_bicubic(fb_w, fb_h, sc_w, sc_h) {
                    return true;
                }
                // Bicubic shader unavailable → clean bilinear rather than a hard crop.
                self.try_rgba_scale(fb_w, fb_h, sc_w, sc_h, false)
            }
            Upscaler::Vsr => self.try_rgba_scale(fb_w, fb_h, sc_w, sc_h, true),
            Upscaler::Bilinear => self.try_rgba_scale(fb_w, fb_h, sc_w, sc_h, false),
        }
    }

    /// VideoProcessor scale path (the `Vsr`/`Bilinear` upscalers). `enable_vsr`
    /// turns on NVIDIA RTX Video Super Resolution; otherwise the driver's plain
    /// bilinear scale is used. Returns `false` (crop fallback) on unavailability.
    fn try_rgba_scale(
        &mut self,
        fb_w: u32,
        fb_h: u32,
        sc_w: u32,
        sc_h: u32,
        enable_vsr: bool,
    ) -> bool {
        if self.scaler_disabled {
            return false;
        }
        let stale = match &self.scaler {
            None => true,
            Some(s) => s.in_size != (fb_w, fb_h) || s.out_size != (sc_w, sc_h),
        };
        if stale {
            match RgbaScaler::new(&self.device, &self.context, fb_w, fb_h, sc_w, sc_h, enable_vsr) {
                Ok(s) => self.scaler = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "GPU scaler unavailable; cropping instead");
                    self.scaler_disabled = true;
                    return false;
                }
            }
        }
        let fb = self.framebuffer.clone().expect("framebuffer present");
        let back_buffer: ID3D11Texture2D = match unsafe { self.swap_chain.GetBuffer(0) } {
            Ok(b) => b,
            Err(_) => return false,
        };
        let s = self.scaler.as_mut().expect("scaler present");
        match s.blit(&fb, &back_buffer, fb_w, fb_h, sc_w, sc_h) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "GPU scale blit failed; cropping instead");
                self.scaler_disabled = true;
                false
            }
        }
    }

    /// Catmull-Rom bicubic shader path (the default `Bicubic` upscaler). Returns
    /// `false` (and latches `bicubic_disabled`) if the shader can't be built/run,
    /// so [`try_smart_size`](Self::try_smart_size) can drop to bilinear.
    fn try_bicubic(&mut self, fb_w: u32, fb_h: u32, sc_w: u32, sc_h: u32) -> bool {
        let stale = match &self.bicubic {
            None => true,
            Some(b) => b.in_size != (fb_w, fb_h) || b.out_size != (sc_w, sc_h),
        };
        if stale {
            match BicubicScaler::new(&self.device, &self.context, fb_w, fb_h, sc_w, sc_h) {
                Ok(b) => self.bicubic = Some(b),
                Err(e) => {
                    tracing::warn!(error = %e, "bicubic upscaler unavailable; using bilinear");
                    self.bicubic_disabled = true;
                    return false;
                }
            }
        }
        let fb = self.framebuffer.clone().expect("framebuffer present");
        let back_buffer: ID3D11Texture2D = match unsafe { self.swap_chain.GetBuffer(0) } {
            Ok(b) => b,
            Err(_) => return false,
        };
        let b = self.bicubic.as_mut().expect("bicubic present");
        match b.blit(&fb, &back_buffer, sc_w, sc_h) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "bicubic blit failed; using bilinear");
                self.bicubic_disabled = true;
                false
            }
        }
    }

    /// Clone the device + immediate context so the session worker can run a DXVA
    /// decoder on the same device (the device is multithread-protected).
    /// Diagnostic: read the live framebuffer back to CPU as tightly-packed RGBA
    /// (`w*h*4`). Used to dump exactly what's being presented. Returns `None` if
    /// there's no framebuffer or the staging copy/map fails.
    pub fn readback_framebuffer(&self) -> Option<(u32, u32, Vec<u8>)> {
        let fb = self.framebuffer.as_ref()?;
        let (w, h) = (self.fb_width, self.fb_height);
        if w == 0 || h == 0 {
            return None;
        }
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .ok()?;
            let staging = staging?;
            self.context.CopyResource(&staging, fb);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h as usize);
            let mut out = vec![0u8; (w * h * 4) as usize];
            for row in 0..h as usize {
                let s = row * pitch;
                let d = row * w as usize * 4;
                out[d..d + w as usize * 4].copy_from_slice(&src[s..s + w as usize * 4]);
            }
            self.context.Unmap(&staging, 0);
            Some((w, h, out))
        }
    }

    pub fn device_context_clone(&self) -> (ID3D11Device, ID3D11DeviceContext) {
        (self.device.clone(), self.context.clone())
    }

    /// Color-convert a GPU NV12 texture (from the DXVA decoder, zero-copy) into
    /// the framebuffer at `(dest_x, dest_y)`. Returns `false` if the GPU video
    /// path is unavailable, so the caller can fall back to CPU conversion.
    pub fn blit_texture(
        &mut self,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        tex: &ID3D11Texture2D,
    ) -> bool {
        if self.video_disabled || w == 0 || h == 0 {
            return false;
        }
        if self.framebuffer.is_none() {
            let _ = self.ensure_framebuffer((dest_x + w).max(1), (dest_y + h).max(1));
        }
        let (fb_w, fb_h) = (self.fb_width, self.fb_height);
        let Some(fb) = self.framebuffer.clone() else {
            return false;
        };
        if fb_w == 0 || fb_h == 0 {
            return false;
        }
        if !self.videos.contains_key(&(w, h)) {
            match VideoConv::new(&self.device, &self.context, w, h, fb_w, fb_h) {
                Ok(v) => {
                    self.videos.insert((w, h), v);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "GPU video processor unavailable; using CPU YUV");
                    self.video_disabled = true;
                    return false;
                }
            }
        }
        let v = self.videos.get_mut(&(w, h)).expect("video present");
        match v.blit_external(&fb, tex, dest_x, dest_y, w, h) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "GPU texture blit failed; falling back to CPU YUV");
                self.video_disabled = true;
                false
            }
        }
    }
}

/// GPU NV12→RGB color converter built on the Direct3D 11 video processor. The
/// driver performs the color-space conversion (and any scaling) in fixed-
/// function hardware, so there is no CPU per-pixel work and no hand-written
/// color matrix to get wrong. Created lazily for a specific input (decoded
/// frame) and output (framebuffer) size.
struct VideoConv {
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    out_size: (u32, u32),
    /// Reusable NV12 input texture (sized to the converter's input size).
    nv12: Option<ID3D11Texture2D>,
    /// Cached output view on the framebuffer (rebuilt when the framebuffer is).
    output_view: Option<ID3D11VideoProcessorOutputView>,
    /// Set once the input/output color spaces have been configured.
    color_set: bool,
}

impl VideoConv {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> WinResult<Self> {
        unsafe {
            let vdevice: ID3D11VideoDevice = device.cast()?;
            let vcontext: ID3D11VideoContext = context.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = vdevice.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdevice.CreateVideoProcessor(&enumerator, 0)?;
            Ok(Self {
                vdevice,
                vcontext,
                enumerator,
                processor,
                out_size: (out_w, out_h),
                nv12: None,
                output_view: None,
                color_set: false,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        framebuffer: &ID3D11Texture2D,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        nv12: &[u8],
    ) -> WinResult<()> {
        unsafe {
            // Reusable NV12 input texture.
            if self.nv12.is_none() {
                let td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t: Option<ID3D11Texture2D> = None;
                device.CreateTexture2D(&td, None, Some(&mut t))?;
                self.nv12 = t;
            }
            let nv12_tex = self.nv12.clone().expect("nv12 texture");
            // Upload NV12. Row pitch is the Y-plane width; the runtime reads the
            // interleaved UV plane immediately after the Y rows at the same pitch.
            context.UpdateSubresource(
                &nv12_tex,
                0,
                None,
                nv12.as_ptr() as *const core::ffi::c_void,
                w,
                w * h,
            );

            // Output view on the framebuffer (cached across frames).
            if self.output_view.is_none() {
                let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
                self.vdevice.CreateVideoProcessorOutputView(
                    framebuffer,
                    &self.enumerator,
                    &od,
                    Some(&mut ov),
                )?;
                self.output_view = ov;
            }
            let output_view = self.output_view.clone().expect("output view");

            // Fresh input view for this frame's NV12 texture.
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            self.vdevice
                .CreateVideoProcessorInputView(&nv12_tex, &self.enumerator, &id, Some(&mut iv))?;
            let input_view = iv.expect("input view");

            // Color spaces (once): RDP AVC video is BT.709 studio-range YCbCr;
            // output is full-range RGB. The bitfields mirror the CPU path so the
            // two render identically.
            if !self.color_set {
                // bit2 = YCbCr_Matrix(BT.709), bits4-5 = Nominal_Range(16-235).
                let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 };
                // Full-range RGB output (all-zero = full range, playback).
                let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x00 };
                self.vcontext
                    .VideoProcessorSetStreamColorSpace(&self.processor, 0, &in_cs);
                self.vcontext
                    .VideoProcessorSetOutputColorSpace(&self.processor, &out_cs);
                self.vcontext.VideoProcessorSetStreamFrameFormat(
                    &self.processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.color_set = true;
            }

            // Clamp the frame to what fits in the framebuffer from (dest_x,dest_y),
            // cropping (not scaling) at the edges: source and dest rects are the
            // same size. Restrict the output target rect to the destination so the
            // blt doesn't clear the rest of the framebuffer (other surfaces /
            // bitmap updates) to the background color.
            let (out_w, out_h) = self.out_size;
            let cw = w.min(out_w.saturating_sub(dest_x));
            let ch = h.min(out_h.saturating_sub(dest_y));
            if cw == 0 || ch == 0 {
                drop(input_view);
                return Ok(());
            }
            let src = RECT {
                left: 0,
                top: 0,
                right: cw as i32,
                bottom: ch as i32,
            };
            let dst = RECT {
                left: dest_x as i32,
                top: dest_y as i32,
                right: (dest_x + cw) as i32,
                bottom: (dest_y + ch) as i32,
            };
            self.vcontext
                .VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&dst));
            self.vcontext
                .VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.vcontext
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: core::ptr::null_mut(),
                pInputSurface: core::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: core::ptr::null_mut(),
                ppPastSurfacesRight: core::ptr::null_mut(),
                pInputSurfaceRight: core::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: core::ptr::null_mut(),
            };
            let result =
                self.vcontext
                    .VideoProcessorBlt(&self.processor, &output_view, 0, core::slice::from_ref(&stream));
            // Release the input view we moved into the stream descriptor.
            let _ = core::mem::ManuallyDrop::into_inner(stream.pInputSurface);
            result
        }
    }

    /// Like [`blit`], but the NV12 input is an existing GPU texture (from the
    /// DXVA decoder) — no upload. The whole frame is placed at `(dest_x,dest_y)`,
    /// clamped to the framebuffer.
    #[allow(clippy::too_many_arguments)]
    fn blit_external(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        ext_tex: &ID3D11Texture2D,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
    ) -> WinResult<()> {
        unsafe {
            // Output view on the framebuffer (cached across frames).
            if self.output_view.is_none() {
                let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
                self.vdevice.CreateVideoProcessorOutputView(
                    framebuffer,
                    &self.enumerator,
                    &od,
                    Some(&mut ov),
                )?;
                self.output_view = ov;
            }
            let output_view = self.output_view.clone().expect("output view");

            // Input view directly on the decoder's NV12 texture (array slice 0).
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            self.vdevice
                .CreateVideoProcessorInputView(ext_tex, &self.enumerator, &id, Some(&mut iv))?;
            let input_view = iv.expect("input view");

            if !self.color_set {
                let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 };
                let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x00 };
                self.vcontext
                    .VideoProcessorSetStreamColorSpace(&self.processor, 0, &in_cs);
                self.vcontext
                    .VideoProcessorSetOutputColorSpace(&self.processor, &out_cs);
                self.vcontext.VideoProcessorSetStreamFrameFormat(
                    &self.processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.color_set = true;
            }

            let (out_w, out_h) = self.out_size;
            let cw = w.min(out_w.saturating_sub(dest_x));
            let ch = h.min(out_h.saturating_sub(dest_y));
            if cw == 0 || ch == 0 {
                drop(input_view);
                return Ok(());
            }
            let src = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
            let dst = RECT {
                left: dest_x as i32,
                top: dest_y as i32,
                right: (dest_x + cw) as i32,
                bottom: (dest_y + ch) as i32,
            };
            self.vcontext
                .VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&dst));
            self.vcontext
                .VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.vcontext
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: core::ptr::null_mut(),
                pInputSurface: core::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: core::ptr::null_mut(),
                ppPastSurfacesRight: core::ptr::null_mut(),
                pInputSurfaceRight: core::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: core::ptr::null_mut(),
            };
            let result = self.vcontext.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                core::slice::from_ref(&stream),
            );
            let _ = core::mem::ManuallyDrop::into_inner(stream.pInputSurface);
            result
        }
    }
}

/// GPU RGBA scaler (smart-sizing) built on the Direct3D 11 video processor: it
/// scales the desktop framebuffer to fill the (differently sized) backbuffer.
/// Both surfaces are RGBA, so this is a pure scale — no color conversion.
struct RgbaScaler {
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_size: (u32, u32),
    out_size: (u32, u32),
}

/// Try to enable NVIDIA RTX Video Super Resolution on `processor`'s stream 0 — the
/// Tensor-core AI upscaler Edge/Chrome use for video, exposed through the D3D11
/// video-processor vendor extension. Best-effort: returns `false` (and the blit
/// falls back to the driver's default bilinear scale) on any non-RTX GPU, older
/// driver, or unsupported path. Intel/AMD expose the same idea under their own GUIDs
/// (not wired here).
unsafe fn enable_rtx_super_resolution(
    vcontext: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
) -> bool {
    // NVIDIA "PPE" interface GUID + the super-resolution stream-extension method
    // (matches the RTX VSR enablement browsers use).
    const NVIDIA_PPE_INTERFACE: windows::core::GUID = windows::core::GUID::from_values(
        0xd43ce1b3,
        0x1f4b,
        0x48ac,
        [0xba, 0xee, 0xc3, 0xc2, 0x53, 0x2e, 0x53, 0x64],
    );
    #[repr(C)]
    struct NvSuperResStreamExt {
        version: u32,
        method: u32,
        enable: u32,
    }
    let ext = NvSuperResStreamExt { version: 1, method: 2, enable: 1 };
    let hr = vcontext.VideoProcessorSetStreamExtension(
        processor,
        0,
        &NVIDIA_PPE_INTERFACE,
        std::mem::size_of::<NvSuperResStreamExt>() as u32,
        &ext as *const _ as *const core::ffi::c_void,
    );
    hr >= 0 // SUCCEEDED(hr)
}

impl RgbaScaler {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        enable_vsr: bool,
    ) -> WinResult<Self> {
        unsafe {
            let vdevice: ID3D11VideoDevice = device.cast()?;
            let vcontext: ID3D11VideoContext = context.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = vdevice.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdevice.CreateVideoProcessor(&enumerator, 0)?;
            // VSR is opt-in (the `Vsr` upscaler). When requested but unavailable
            // (non-RTX GPU / old driver) the blit silently uses the driver's
            // bilinear scale — same path as the `Bilinear` upscaler.
            if enable_vsr && enable_rtx_super_resolution(&vcontext, &processor) {
                tracing::info!(
                    in_w,
                    in_h,
                    out_w,
                    out_h,
                    "client upscale: NVIDIA RTX Video Super Resolution"
                );
            } else if enable_vsr {
                tracing::info!(
                    in_w,
                    in_h,
                    out_w,
                    out_h,
                    "client upscale: bilinear (RTX VSR requested but unavailable — update GPU driver)"
                );
            } else {
                tracing::info!(in_w, in_h, out_w, out_h, "client upscale: bilinear (video processor)");
            }
            Ok(Self {
                vdevice,
                vcontext,
                enumerator,
                processor,
                in_size: (in_w, in_h),
                out_size: (out_w, out_h),
            })
        }
    }

    /// Scale `framebuffer` (RGBA, `in`) to fill `backbuffer` (RGBA, `out`).
    fn blit(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        backbuffer: &ID3D11Texture2D,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> WinResult<()> {
        unsafe {
            let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            self.vdevice
                .CreateVideoProcessorInputView(framebuffer, &self.enumerator, &iv_desc, Some(&mut iv))?;
            let input_view = iv.expect("input view");

            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
            self.vdevice
                .CreateVideoProcessorOutputView(backbuffer, &self.enumerator, &ov_desc, Some(&mut ov))?;
            let output_view = ov.expect("output view");

            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            self.vcontext
                .VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.vcontext
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
            self.vcontext
                .VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&dst));

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: core::ptr::null_mut(),
                pInputSurface: core::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: core::ptr::null_mut(),
                ppPastSurfacesRight: core::ptr::null_mut(),
                pInputSurfaceRight: core::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: core::ptr::null_mut(),
            };
            let result = self.vcontext.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                core::slice::from_ref(&stream),
            );
            let _ = core::mem::ManuallyDrop::into_inner(stream.pInputSurface);
            result
        }
    }
}

/// Catmull-Rom bicubic upscale shader. A fullscreen-triangle vertex shader plus a
/// 9-tap (3×3 bilinear-sample) Catmull-Rom pixel shader (Matt Pettineo's well-worn
/// formulation). Catmull-Rom is sharp without the ringing/edge-hallucination an AI
/// video upscaler (VSR) produces on text and UI — so this is the default for the
/// mixed desktop + game content RDP carries. Alpha is forced opaque (the swapchain
/// is opaque) to match the VideoProcessor path.
const BICUBIC_HLSL: &str = r#"
Texture2D src : register(t0);
SamplerState samp : register(s0);
cbuffer Params : register(b0) { float2 texSize; float2 invTexSize; };
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}
float4 ps_main(VSOut i) : SV_Target {
    float2 samplePos = i.uv * texSize;
    float2 texPos1 = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - texPos1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 offset12 = w2 / w12;
    float2 p0  = (texPos1 - 1.0) * invTexSize;
    float2 p3  = (texPos1 + 2.0) * invTexSize;
    float2 p12 = (texPos1 + offset12) * invTexSize;
    float4 r = float4(0.0, 0.0, 0.0, 0.0);
    r += src.SampleLevel(samp, float2(p0.x,  p0.y),  0.0) * (w0.x  * w0.y);
    r += src.SampleLevel(samp, float2(p12.x, p0.y),  0.0) * (w12.x * w0.y);
    r += src.SampleLevel(samp, float2(p3.x,  p0.y),  0.0) * (w3.x  * w0.y);
    r += src.SampleLevel(samp, float2(p0.x,  p12.y), 0.0) * (w0.x  * w12.y);
    r += src.SampleLevel(samp, float2(p12.x, p12.y), 0.0) * (w12.x * w12.y);
    r += src.SampleLevel(samp, float2(p3.x,  p12.y), 0.0) * (w3.x  * w12.y);
    r += src.SampleLevel(samp, float2(p0.x,  p3.y),  0.0) * (w0.x  * w3.y);
    r += src.SampleLevel(samp, float2(p12.x, p3.y),  0.0) * (w12.x * w3.y);
    r += src.SampleLevel(samp, float2(p3.x,  p3.y),  0.0) * (w3.x  * w3.y);
    r.a = 1.0;
    return r;
}
"#;

/// Constant buffer for [`BICUBIC_HLSL`]: source size and its reciprocal, in texels.
#[repr(C)]
struct BicubicParams {
    tex_size: [f32; 2],
    inv_tex_size: [f32; 2],
}

/// Compile one stage of [`BICUBIC_HLSL`] at runtime via d3dcompiler. Returns the
/// bytecode blob, logging the compiler's diagnostics on failure.
unsafe fn compile_bicubic_shader(entry: PCSTR, target: PCSTR) -> WinResult<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = D3DCompile(
        BICUBIC_HLSL.as_ptr() as *const core::ffi::c_void,
        BICUBIC_HLSL.len(),
        windows::core::s!("bicubic.hlsl"),
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = res {
        if let Some(err) = &errors {
            let msg = core::slice::from_raw_parts(
                err.GetBufferPointer() as *const u8,
                err.GetBufferSize(),
            );
            tracing::warn!(error = %String::from_utf8_lossy(msg), "bicubic HLSL compile failed");
        }
        return Err(e);
    }
    Ok(code.expect("shader blob present on D3DCompile success"))
}

/// GPU Catmull-Rom bicubic scaler: scales the desktop framebuffer (RGBA) to fill
/// the (larger) backbuffer with a single fullscreen draw. Owns its compiled
/// shaders, a clamped linear sampler, and an immutable constant buffer holding the
/// source size; the input SRV and output RTV are created per blit (cheap, and the
/// scaler is rebuilt whenever either size changes — see
/// [`D3D11Renderer::try_bicubic`]). Holds its own device + context clones so a blit
/// borrows neither while the renderer holds the scaler mutably.
struct BicubicScaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    in_size: (u32, u32),
    out_size: (u32, u32),
}

impl BicubicScaler {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> WinResult<Self> {
        unsafe {
            let vs_blob = compile_bicubic_shader(
                windows::core::s!("vs_main"),
                windows::core::s!("vs_5_0"),
            )?;
            let ps_blob = compile_bicubic_shader(
                windows::core::s!("ps_main"),
                windows::core::s!("ps_5_0"),
            )?;
            let vs_code = core::slice::from_raw_parts(
                vs_blob.GetBufferPointer() as *const u8,
                vs_blob.GetBufferSize(),
            );
            let ps_code = core::slice::from_raw_parts(
                ps_blob.GetBufferPointer() as *const u8,
                ps_blob.GetBufferSize(),
            );
            let mut vs: Option<ID3D11VertexShader> = None;
            device.CreateVertexShader(vs_code, None, Some(&mut vs))?;
            let mut ps: Option<ID3D11PixelShader> = None;
            device.CreatePixelShader(ps_code, None, Some(&mut ps))?;

            // Bilinear taps, clamp at the edges (the Catmull-Rom kernel does the
            // sharpening between taps).
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: f32::MAX,
            };
            let mut sampler: Option<ID3D11SamplerState> = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;

            let params = BicubicParams {
                tex_size: [in_w as f32, in_h as f32],
                inv_tex_size: [1.0 / in_w as f32, 1.0 / in_h as f32],
            };
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: core::mem::size_of::<BicubicParams>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let srd = D3D11_SUBRESOURCE_DATA {
                pSysMem: &params as *const _ as *const core::ffi::c_void,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut cb: Option<ID3D11Buffer> = None;
            device.CreateBuffer(&bd, Some(&srd), Some(&mut cb))?;

            tracing::info!(in_w, in_h, out_w, out_h, "client upscale: Catmull-Rom bicubic (shader)");
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.expect("vertex shader"),
                ps: ps.expect("pixel shader"),
                sampler: sampler.expect("sampler"),
                params: cb.expect("constant buffer"),
                in_size: (in_w, in_h),
                out_size: (out_w, out_h),
            })
        }
    }

    /// Upscale `framebuffer` into `backbuffer` (`out_w`×`out_h`) with one draw.
    fn blit(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        backbuffer: &ID3D11Texture2D,
        out_w: u32,
        out_h: u32,
    ) -> WinResult<()> {
        unsafe {
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            self.device
                .CreateShaderResourceView(framebuffer, None, Some(&mut srv))?;
            let srv = srv.expect("shader resource view");
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(backbuffer, None, Some(&mut rtv))?;
            let rtv = rtv.expect("render target view");

            let ctx = &self.context;
            let rtvs = [Some(rtv.clone())];
            ctx.OMSetRenderTargets(Some(&rtvs), None);
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: out_w as f32,
                Height: out_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            let srvs = [Some(srv.clone())];
            ctx.PSSetShaderResources(0, Some(&srvs));
            let samplers = [Some(self.sampler.clone())];
            ctx.PSSetSamplers(0, Some(&samplers));
            let cbs = [Some(self.params.clone())];
            ctx.PSSetConstantBuffers(0, Some(&cbs));
            ctx.Draw(3, 0);

            // Unbind the framebuffer SRV and the backbuffer RTV so the next frame's
            // framebuffer writes (decode blits / bitmap updates) can't collide with
            // a stale input/output binding.
            let no_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.PSSetShaderResources(0, Some(&no_srv));
            let no_rtv: [Option<ID3D11RenderTargetView>; 1] = [None];
            ctx.OMSetRenderTargets(Some(&no_rtv), None);
            Ok(())
        }
    }
}
