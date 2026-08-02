//! Direct3D 11 renderer (Windows).

use std::collections::HashMap;

use windows::core::{Interface, PCSTR, Result as WinResult};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, RECT};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
    ID3DBlob,
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
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
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
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED, DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIFactory6, IDXGISwapChain,
    IDXGISwapChain1, IDXGISwapChain2, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
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
    /// Upscale + sharpen pass chain for the primary swapchain (shader kernels,
    /// VideoProcessor scaler, optional RCAS stage), lazily built on the first
    /// present that needs it and rebuilt when sizes change. Failure latches
    /// inside the pipeline; a fully-failed pipeline → crop fallback.
    pipeline: UpscalePipeline,
    /// Which upscaler the smart-size present path uses when the desktop framebuffer
    /// is smaller than the window. Set once at startup via [`Self::set_upscaler`];
    /// default [`Upscaler::Bicubic`].
    upscaler: Upscaler,
    /// RCAS sharpen strength `0.0..=1.0` applied after the upscale (and at 1:1
    /// when no scaling happens). `0.0` = off. Set via [`Self::set_sharpen`].
    sharpen: f32,
    /// The swapchain's frame-latency waitable object (when the chain was created
    /// `FRAME_LATENCY_WAITABLE_OBJECT` with max latency 1). Waiting on it before
    /// each present keeps at most one frame queued, so the displayed frame is at
    /// most ~1 refresh old instead of DXGI's default of up to 3 — the latency
    /// edge that matters for interactive + gaming use.
    frame_wait: Option<HANDLE>,
    /// NV12→RGBA pixel-shader converter — the primary colour-conversion path for
    /// GPU-decoded frames, built lazily on first use.
    nv12_shader: Option<Nv12Shader>,
    /// Latched if the shader converter could not be built or its blit failed, so
    /// we stop retrying it and use the video processor instead.
    nv12_shader_failed: bool,
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
    /// The framebuffer slice size the primary swapchain presents in per-monitor
    /// mode. `None` = same as the swapchain (1:1); `Some(smaller)` under
    /// render-scale, where the slice is upscaled on present.
    primary_src_size: Option<(u32, u32)>,
    /// Optional GPU timing callback; receives `(label, microseconds)` for
    /// completed D3D11 timestamp queries. Wired up by the client to the telemetry
    /// module.
    gpu_timing_cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
    /// Timestamp-query pair for the `Present` call.
    present_timer: Option<GpuTimer>,
    /// Framebuffer regions (x, y, w, h) painted since the last present.
    /// `None` = unbounded ("everything may have changed") — resize, clears,
    /// or more distinct rects than worth tracking. Drives the partial
    /// present: the 1:1 path copies only what a rotated FLIP backbuffer is
    /// missing and hands DWM the changed rects via `Present1`, instead of
    /// pushing the full desktop (~13 MB at laptop resolutions) through the
    /// iGPU every present.
    frame_dirty: Option<Vec<(u32, u32, u32, u32)>>,
    /// Per-present dirty sets of the last `BufferCount - 1` presents, newest
    /// first. A FLIP backbuffer coming back around is missing exactly the
    /// union of these plus the current frame's dirty set.
    recent_dirty: std::collections::VecDeque<Option<Vec<(u32, u32, u32, u32)>>>,
}

/// Past this many distinct rects per frame, tracking costs more than a full
/// copy saves — collapse to "everything dirty".
const MAX_DIRTY_RECTS: usize = 64;

/// One additional present surface for per-monitor mode: a swapchain bound to a
/// physical monitor's window, presenting a slice of the shared framebuffer.
/// Shares the renderer's D3D11 device. When `src_size` differs from the window
/// (`width`×`height`) — render-scale under per-monitor — the slice is upscaled
/// through this target's own [`UpscalePipeline`]; otherwise it is a 1:1 copy.
struct PresentTarget {
    swap_chain: IDXGISwapChain,
    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
    frame_wait: Option<HANDLE>,
    tearing: bool,
    /// Top-left offset of this monitor's slice within the framebuffer.
    src: (u32, u32),
    /// Size of this monitor's slice within the framebuffer (its scaled monitor
    /// rectangle under render-scale; equal to `width`×`height` otherwise).
    src_size: (u32, u32),
    /// Per-target upscale + sharpen chain (only exercised when scaling/sharpening).
    pipeline: UpscalePipeline,
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
                // 3, per the DXGI guidance for waitable flip chains: with 2,
                // the frame-latency wait can stall a full refresh per present
                // on the UI thread (which is also the input/blit-apply
                // thread). The third buffer costs one screen-size texture.
                BufferCount: 3,
                OutputWindow: hwnd,
                Windowed: true.into(),
                // SEQUENTIAL, not DISCARD: the partial-present path needs both
                // of its contracts — Present1 with dirty rects returns
                // DXGI_ERROR_INVALID_CALL on a FLIP_DISCARD chain (always, not
                // just post-resize), and only SEQUENTIAL guarantees a reused
                // backbuffer still holds its BufferCount-ago contents, which is
                // what `stale_region` copies against. Windowed DWM composition
                // performs identically under either effect.
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
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
                nv12_shader: None,
                nv12_shader_failed: false,
                pipeline: UpscalePipeline::default(),
                upscaler: Upscaler::default(),
                sharpen: 0.0,
                frame_wait,
                tearing,
                low_latency: false,
                sc_flags,
                gfx_cache: HashMap::new(),
                copy_scratch: None,
                extra_targets: Vec::new(),
                primary_src: (0, 0),
                primary_src_size: None,
                gpu_timing_cb: None,
                present_timer: None,
                frame_dirty: None,
                recent_dirty: std::collections::VecDeque::new(),
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
        self.pipeline.reset();
        for t in &mut self.extra_targets {
            t.pipeline.reset();
        }
        tracing::info!(?mode, "client upscaler selected");
    }

    /// Set the RCAS adaptive-sharpen strength (`0.0` = off, `1.0` = maximum)
    /// applied after the upscale — or straight to the framebuffer when no
    /// scaling happens. Composable with every upscaler, including the
    /// VideoProcessor modes.
    pub fn set_sharpen(&mut self, strength: f32) {
        self.sharpen = strength.clamp(0.0, 1.0);
        self.pipeline.reset();
        for t in &mut self.extra_targets {
            t.pipeline.reset();
        }
        if self.sharpen > 0.0 {
            tracing::info!(strength = self.sharpen, "RCAS sharpen enabled");
        }
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
            self.pipeline.reset();
            // Fresh backbuffers hold garbage: the partial-present history no
            // longer describes them, so the next presents must copy in full.
            self.frame_dirty = None;
            self.recent_dirty.clear();
            self.create_rtv()
        }
    }

    /// Clear the backbuffer to an RGBA color and present. Used before the first
    /// server frame arrives (and as the idle background).
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> WinResult<()> {
        self.wait_for_frame();
        // The clear rewrites the whole backbuffer outside the dirty tracking.
        self.frame_dirty = None;
        self.recent_dirty.clear();
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

    /// Set the framebuffer slice the primary swapchain presents from (per-monitor
    /// mode): top-left offset plus the slice size. A `src_w`/`src_h` of 0 (or
    /// equal to the swapchain size) means a 1:1 slice; a smaller slice —
    /// render-scale under per-monitor — is upscaled on present. The default
    /// `(0, 0)` offset presents the whole framebuffer (single window).
    pub fn set_primary_src(&mut self, x: u32, y: u32, src_w: u32, src_h: u32) {
        self.primary_src = (x, y);
        self.primary_src_size = (src_w != 0 && src_h != 0).then_some((src_w, src_h));
    }

    /// Add a per-monitor present target: a new swapchain on `hwnd_raw`'s window
    /// (sharing this device + framebuffer) that presents the `src_w`×`src_h`
    /// framebuffer slice at `(src_x, src_y)`. A slice smaller than the window
    /// (`--render-scale` under per-monitor) is upscaled on present; `src_w`/`src_h`
    /// of 0 means window-sized (1:1). Used to drive one window per physical
    /// monitor over a single spanned remote desktop.
    #[allow(clippy::too_many_arguments)]
    pub fn add_present_target(
        &mut self,
        hwnd_raw: isize,
        width: u32,
        height: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
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
            let src_size = if src_w != 0 && src_h != 0 {
                (src_w, src_h)
            } else {
                (width, height)
            };
            tracing::info!(
                width,
                height,
                src_x,
                src_y,
                src_w = src_size.0,
                src_h = src_size.1,
                tearing,
                "per-monitor present target added"
            );
            self.extra_targets.push(PresentTarget {
                swap_chain,
                rtv,
                width,
                height,
                frame_wait,
                tearing,
                src: (src_x, src_y),
                src_size,
                pipeline: UpscalePipeline::default(),
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

    // (per-monitor slice presenting lives in the free function
    // [`present_target_slice`] so each target can borrow its own
    // [`UpscalePipeline`] mutably while the renderer's shared fields are read.)

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
        // The NV12 shader caches a render target view on the old framebuffer.
        if let Some(s) = self.nv12_shader.as_mut() {
            s.rtv = None;
        }
        self.pipeline.reset();
        for t in &mut self.extra_targets {
            t.pipeline.reset();
        }
        self.copy_scratch = None;
        // A new framebuffer invalidates every partial-present assumption.
        self.note_dirty_full();
        self.recent_dirty.clear();
        tracing::debug!(width, height, "framebuffer texture (re)allocated");
        Ok(())
    }

    /// Record a framebuffer region painted since the last present, for the
    /// partial-present path. Over-marking is always safe; under-marking never
    /// happens because every painter below reports its clamped destination.
    fn note_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        match self.frame_dirty.as_mut() {
            Some(rects) if rects.len() < MAX_DIRTY_RECTS => rects.push((x, y, w, h)),
            Some(_) => self.frame_dirty = None, // too fragmented — full frame
            None => {}
        }
    }

    /// Everything may have changed (resize, clear, unbounded paint).
    fn note_dirty_full(&mut self) {
        self.frame_dirty = None;
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
        self.note_dirty(x, y, right - x, bottom - y);
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
        self.note_dirty(dx, dy, cw, ch);
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
        self.note_dirty(dx, dy, cw, ch);
    }

    /// Disable the GPU NV12 path (forces CPU YUV conversion). Used by the
    /// `--cpu-yuv` safety valve.
    pub fn disable_gpu_yuv(&mut self) {
        self.video_disabled = true;
        self.videos.clear();
        // `--cpu-yuv` means "convert on the CPU, period" — take down the NV12
        // pixel-shader path too, or `blit_texture` would keep converting on the
        // GPU (it is deliberately not subject to the video-processor latch).
        self.nv12_shader_failed = true;
        self.nv12_shader = None;
    }

    /// Whether the GPU NV12 color-conversion path is currently usable. Callers
    /// check this to decide whether to hand NV12 to [`blit_nv12`] or convert on
    /// the CPU themselves.
    pub fn gpu_yuv_available(&self) -> bool {
        !self.video_disabled
    }

    /// Convert an NV12 frame to RGB on the GPU (Direct3D 11 video processor) and
    /// write its dirty `regions` (frame-relative; empty = whole frame) into the
    /// framebuffer at `(dest_x, dest_y)`. `nv12` is tightly packed: a `w*h` Y
    /// plane followed by a `w*(h/2)` interleaved UV plane.
    ///
    /// Returns `false` (without painting) if the GPU path is unavailable or
    /// fails — the caller must then fall back to CPU conversion + [`update_rect`]
    /// so the picture is never lost. Once a hard failure occurs the GPU path is
    /// latched off for the rest of the session.
    pub fn blit_nv12(
        &mut self,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        nv12: &[u8],
        regions: &[(u32, u32, u32, u32)],
    ) -> bool {
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
        match v.blit(&self.device, &self.context, &fb, dest_x, dest_y, w, h, nv12, regions) {
            Ok(()) => {
                self.note_dirty(dest_x, dest_y, w, h);
                true
            }
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
        // extra monitor's slice — a 1:1 copy when the slice matches the window,
        // an upscale (+ optional sharpen) through the target's pipeline when it
        // doesn't (render-scale under per-monitor).
        if !self.extra_targets.is_empty() {
            let device = self.device.clone();
            let context = self.context.clone();
            let fb = self.framebuffer.clone().expect("framebuffer present");
            let fb_size = (self.fb_width, self.fb_height);
            let (mode, sharpen, low_latency) = (self.upscaler, self.sharpen, self.low_latency);
            let primary_dst = (self.sc_width, self.sc_height);
            let primary_src_size = self.primary_src_size.unwrap_or(primary_dst);
            let primary_sc = self.swap_chain.clone();
            let (primary_wait, primary_tearing, primary_src) =
                (self.frame_wait, self.tearing, self.primary_src);
            let res = unsafe {
                let primary = present_target_slice(
                    &device,
                    &context,
                    &fb,
                    fb_size,
                    &primary_sc,
                    primary_wait,
                    primary_tearing,
                    low_latency,
                    primary_dst,
                    primary_src,
                    primary_src_size,
                    &mut self.pipeline,
                    mode,
                    sharpen,
                );
                match primary {
                    Ok(()) => {
                        let mut r = Ok(());
                        for t in self.extra_targets.iter_mut() {
                            r = present_target_slice(
                                &device,
                                &context,
                                &fb,
                                fb_size,
                                &t.swap_chain,
                                t.frame_wait,
                                t.tearing,
                                low_latency,
                                (t.width, t.height),
                                t.src,
                                t.src_size,
                                &mut t.pipeline,
                                mode,
                                sharpen,
                            );
                            if r.is_err() {
                                break;
                            }
                        }
                        r
                    }
                    Err(e) => Err(e),
                }
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
        let mut painted = false;
        if (fb_w, fb_h) != (sc_w, sc_h) || self.sharpen > 0.0 {
            let device = self.device.clone();
            let context = self.context.clone();
            let fb = self.framebuffer.clone().expect("framebuffer present");
            if let Ok(back_buffer) = unsafe { self.swap_chain.GetBuffer::<ID3D11Texture2D>(0) } {
                painted = self.pipeline.run(
                    &device,
                    &context,
                    self.upscaler,
                    self.sharpen,
                    &fb,
                    (fb_w, fb_h),
                    (0, 0, fb_w, fb_h),
                    &back_buffer,
                    (sc_w, sc_h),
                );
            }
        }
        if !painted {
            // 1:1 (or fallback) copy. A FLIP backbuffer coming back around
            // still holds the frame from `BufferCount` presents ago, so it is
            // missing exactly the union of the recent presents' dirty regions
            // plus this frame's — copy only that instead of the full desktop
            // (~13 MB per present at laptop resolutions on the old path).
            // Falls back to the full copy while the history is cold or any
            // frame's dirty set was unbounded.
            let fb = self.framebuffer.clone().expect("framebuffer present");
            let copy_rects = self.stale_region();
            unsafe {
                let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
                match copy_rects.as_deref() {
                    Some(rects) => {
                        for &(x, y, w, h) in rects {
                            let right = (x.saturating_add(w)).min(fb_w).min(sc_w);
                            let bottom = (y.saturating_add(h)).min(fb_h).min(sc_h);
                            if x >= right || y >= bottom {
                                continue;
                            }
                            let src_box = D3D11_BOX {
                                left: x,
                                top: y,
                                front: 0,
                                right,
                                bottom,
                                back: 1,
                            };
                            self.context.CopySubresourceRegion(
                                &back_buffer,
                                0,
                                x,
                                y,
                                0,
                                &fb,
                                0,
                                Some(&src_box),
                            );
                        }
                    }
                    None => {
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
            }
        }
        let (sync, flags) = self.present_params();
        // Hand DWM this frame's changed rects (`Present1`) so composition also
        // touches only what changed. The scaled/sharpen paths redraw the whole
        // backbuffer, so they present full-frame.
        let dirty_now = if painted { None } else { self.frame_dirty.clone() };
        let res = unsafe { self.present_with_rects(sync, flags, dirty_now.as_deref()) };
        // Rotate the dirty history for the next buffer in the ring.
        self.rotate_dirty(painted);
        if let Some(timer) = self.present_timer.as_mut() {
            timer.end();
        }
        res
    }

    /// The region a reused FLIP backbuffer is missing: the union of the recent
    /// presents' dirty sets plus the current frame's. `None` = unknown → the
    /// caller must copy the full frame.
    fn stale_region(&self) -> Option<Vec<(u32, u32, u32, u32)>> {
        // The buffer being rendered was last current `BufferCount` presents
        // ago; history must cover the presents in between.
        let need = 2; // BufferCount(3) - 1
        if self.recent_dirty.len() < need {
            return None;
        }
        let mut out = self.frame_dirty.clone()?;
        for past in self.recent_dirty.iter().take(need) {
            out.extend(past.as_ref()?.iter().copied());
        }
        Some(out)
    }

    /// Push this frame's dirty set into the per-present history and start the
    /// next frame's accumulation. `full_redraw` = the whole backbuffer was
    /// rewritten this present (scaled/sharpen paths).
    fn rotate_dirty(&mut self, full_redraw: bool) {
        let this = if full_redraw {
            self.frame_dirty = None;
            None
        } else {
            self.frame_dirty.take()
        };
        self.recent_dirty.push_front(this);
        self.recent_dirty.truncate(2); // BufferCount - 1
        self.frame_dirty = Some(Vec::new());
    }

    /// Present, attaching the frame's dirty rects when they are known and
    /// bounded — DWM then recomposes only those. Falls back to a plain
    /// (full-frame) `Present` otherwise.
    unsafe fn present_with_rects(
        &self,
        sync: u32,
        flags: DXGI_PRESENT,
        dirty: Option<&[(u32, u32, u32, u32)]>,
    ) -> WinResult<()> {
        if let Some(rects) = dirty {
            if !rects.is_empty() && rects.len() <= MAX_DIRTY_RECTS {
                if let Ok(sc1) = self
                    .swap_chain
                    .cast::<windows::Win32::Graphics::Dxgi::IDXGISwapChain1>()
                {
                    let mut rs: Vec<windows::Win32::Foundation::RECT> = rects
                        .iter()
                        .map(|&(x, y, w, h)| windows::Win32::Foundation::RECT {
                            left: x.min(self.sc_width) as i32,
                            top: y.min(self.sc_height) as i32,
                            right: (x.saturating_add(w)).min(self.sc_width) as i32,
                            bottom: (y.saturating_add(h)).min(self.sc_height) as i32,
                        })
                        .filter(|r| r.right > r.left && r.bottom > r.top)
                        .collect();
                    if !rs.is_empty() {
                        let params = windows::Win32::Graphics::Dxgi::DXGI_PRESENT_PARAMETERS {
                            DirtyRectsCount: rs.len() as u32,
                            pDirtyRects: rs.as_mut_ptr(),
                            pScrollRect: std::ptr::null_mut(),
                            pScrollOffset: std::ptr::null_mut(),
                        };
                        let hr = sc1.Present1(sync, flags, &params);
                        if hr.is_ok() {
                            return Ok(());
                        }
                        // A refused Present1 presents NOTHING (verified: the
                        // error return leaves the frame unshown), so degrade to
                        // a full present rather than dropping the frame — and
                        // never let a declined optimization end the session.
                        // Known refusals: a FLIP_DISCARD chain (any dirty-rect
                        // present), and the first present after ResizeBuffers.
                        use std::sync::atomic::{AtomicBool, Ordering};
                        static WARNED: AtomicBool = AtomicBool::new(false);
                        if !WARNED.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                hr = format!("0x{:08X}", hr.0 as u32),
                                "Present1 with dirty rects refused; presenting full frames instead (once-per-run notice)"
                            );
                        }
                    }
                    // Every rect clipped away — nothing visible changed, but
                    // still present (pacing, timing) as a full frame below.
                }
            }
        }
        self.swap_chain.Present(sync, flags).ok()
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
        regions: &[(u32, u32, u32, u32)],
    ) -> bool {
        // Deliberately NOT gated on `video_disabled`: that latch records a
        // *video processor* failure, and the shader path below doesn't use the
        // video processor. Gating the whole method on it sent every DXVA frame
        // to the CPU readback fallback (a per-frame pipeline stall on the UI
        // thread) even though the shader path was perfectly healthy.
        if w == 0 || h == 0 {
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
        // Shader conversion first: it samples the NV12 planes directly, so it is
        // immune to the video processor's driver-specific input-view rules and
        // never needs a readback. The video processor stays as a fallback for any
        // driver where plane views misbehave.
        if !self.nv12_shader_failed {
            if self.nv12_shader.is_none() {
                match Nv12Shader::new(&self.device, &self.context) {
                    Ok(s) => {
                        tracing::info!("NV12 colour conversion: pixel shader (plane views)");
                        self.nv12_shader = Some(s);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "NV12 shader unavailable; trying the video processor");
                        self.nv12_shader_failed = true;
                    }
                }
            }
            if let Some(s) = self.nv12_shader.as_mut() {
                match s.blit(&fb, tex, dest_x, dest_y, w, h, (fb_w, fb_h), regions) {
                    Ok(()) => {
                        self.note_dirty(dest_x, dest_y, w, h);
                        return true;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "NV12 shader blit failed; trying the video processor");
                        self.nv12_shader_failed = true;
                        self.nv12_shader = None;
                    }
                }
            }
        }

        // Video processor fallback — only this half is subject to the latch.
        if self.video_disabled {
            return false;
        }

        // Size the converter to the TEXTURE, not to the display rectangle inside
        // it. A hardware H.264 decoder hands back a macroblock-aligned surface —
        // 1920x1088 for a 1080p stream — and the video processor's content
        // description has to describe the surface we actually bind. Permissive
        // drivers tolerate the mismatch; strict ones (Intel) reject
        // `CreateVideoProcessorInputView` outright with E_INVALIDARG, which took
        // the entire GPU path down on UHD Graphics. The display region is picked
        // out per blit by the stream source rect, which `blt_regions` sets.
        let (in_w, in_h) = unsafe {
            let mut td: D3D11_TEXTURE2D_DESC = core::mem::zeroed();
            tex.GetDesc(&mut td);
            (td.Width.max(w), td.Height.max(h))
        };
        if !self.videos.contains_key(&(in_w, in_h)) {
            match VideoConv::new(&self.device, &self.context, in_w, in_h, fb_w, fb_h) {
                Ok(v) => {
                    self.videos.insert((in_w, in_h), v);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "GPU video processor unavailable; using CPU YUV");
                    self.video_disabled = true;
                    return false;
                }
            }
        }
        let v = self.videos.get_mut(&(in_w, in_h)).expect("video present");
        match v.blit_external(&fb, tex, dest_x, dest_y, w, h, regions) {
            Ok(()) => {
                self.note_dirty(dest_x, dest_y, w, h);
                true
            }
            Err(e) => {
                // Stop paying for a video processor this driver won't drive, but
                // the CALLER still has to paint the frame — returning false here
                // is a request for the CPU path, not permission to drop it.
                tracing::warn!(
                    error = %e, in_w, in_h, w, h,
                    "GPU texture blit failed; falling back to CPU YUV readback"
                );
                self.video_disabled = true;
                false
            }
        }
    }

    /// Copy an NV12 GPU texture back to the CPU as tightly-packed NV12: `w*h`
    /// luma bytes followed by `w*h/2` interleaved chroma bytes.
    ///
    /// This is the escape hatch for a frame the video processor refused. It is a
    /// full pipeline stall (staging copy + map), so it is never the path of
    /// choice — but a slow picture beats the blank screen you get from dropping
    /// the frame, which is exactly what happened when a driver rejected the blit.
    pub fn read_nv12(&self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Option<Vec<u8>> {
        if w == 0 || h == 0 {
            return None;
        }
        unsafe {
            let mut src: D3D11_TEXTURE2D_DESC = core::mem::zeroed();
            tex.GetDesc(&mut src);
            if src.Format != DXGI_FORMAT_NV12 || src.Width < w || src.Height < h {
                return None;
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..src
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .ok()?;
            let staging = staging?;
            self.context.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let pitch = mapped.RowPitch as usize;
            // A mapped NV12 surface is a single allocation: `src.Height` luma rows
            // at `pitch`, with the interleaved chroma rows immediately after them.
            let total = pitch * (src.Height as usize) * 3 / 2;
            let base = std::slice::from_raw_parts(mapped.pData as *const u8, total);
            let (lw, lh) = (w as usize, h as usize);
            let mut out = vec![0u8; lw * lh + lw * (lh / 2)];
            for row in 0..lh {
                let s = row * pitch;
                out[row * lw..(row + 1) * lw].copy_from_slice(&base[s..s + lw]);
            }
            let chroma_src = pitch * src.Height as usize;
            let chroma_dst = lw * lh;
            for row in 0..lh / 2 {
                let s = chroma_src + row * pitch;
                let d = chroma_dst + row * lw;
                out[d..d + lw].copy_from_slice(&base[s..s + lw]);
            }
            self.context.Unmap(&staging, 0);
            Some(out)
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
        regions: &[(u32, u32, u32, u32)],
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

            self.blt_regions(input_view, &output_view, dest_x, dest_y, w, h, regions)
        }
    }

    /// Run one VideoProcessorBlt per dirty region (or one whole-frame blt when
    /// `regions` is empty), clamping each to the frame and the framebuffer.
    /// Painting ONLY the region rects matters for correctness, not just cost:
    /// outside them the decoded H.264 picture holds the encoder's reference
    /// content, which goes stale the moment another codec (ClearCodec,
    /// progressive) paints the same surface — blitting the whole frame would
    /// stomp those fresher pixels (visible artifacts under heavy motion).
    /// The output target rect is restricted per blt so the operation never
    /// clears the rest of the framebuffer to the background color.
    #[allow(clippy::too_many_arguments)]
    unsafe fn blt_regions(
        &mut self,
        input_view: ID3D11VideoProcessorInputView,
        output_view: &ID3D11VideoProcessorOutputView,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        regions: &[(u32, u32, u32, u32)],
    ) -> WinResult<()> {
        let (out_w, out_h) = self.out_size;
        let whole = [(0, 0, w, h)];
        let regions = if regions.is_empty() { &whole[..] } else { regions };
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
        let mut result = Ok(());
        for &(rx, ry, rw, rh) in regions {
            // Clamp the region to the frame, then to what fits in the
            // framebuffer from the destination offset (crop, not scale).
            if rx >= w || ry >= h {
                continue;
            }
            let dx = dest_x + rx;
            let dy = dest_y + ry;
            let cw = rw.min(w - rx).min(out_w.saturating_sub(dx));
            let ch = rh.min(h - ry).min(out_h.saturating_sub(dy));
            if cw == 0 || ch == 0 {
                continue;
            }
            let src = RECT {
                left: rx as i32,
                top: ry as i32,
                right: (rx + cw) as i32,
                bottom: (ry + ch) as i32,
            };
            let dst = RECT {
                left: dx as i32,
                top: dy as i32,
                right: (dx + cw) as i32,
                bottom: (dy + ch) as i32,
            };
            self.vcontext
                .VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&dst));
            self.vcontext
                .VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.vcontext
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
            result = self.vcontext.VideoProcessorBlt(
                &self.processor,
                output_view,
                0,
                core::slice::from_ref(&stream),
            );
            if let Err(e) = result.as_ref() {
                tracing::warn!(
                    error = %e,
                    src = ?(src.left, src.top, src.right, src.bottom),
                    dst = ?(dst.left, dst.top, dst.right, dst.bottom),
                    out_w, out_h,
                    "VideoProcessorBlt rejected these rectangles"
                );
                break;
            }
        }
        // Release the input view we moved into the stream descriptor.
        let _ = core::mem::ManuallyDrop::into_inner(stream.pInputSurface);
        result
    }

    /// Like [`blit`], but the NV12 input is an existing GPU texture (from the
    /// DXVA decoder) — no upload. The frame's dirty `regions` are placed at
    /// `(dest_x,dest_y)`, clamped to the framebuffer.
    #[allow(clippy::too_many_arguments)]
    fn blit_external(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        ext_tex: &ID3D11Texture2D,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        regions: &[(u32, u32, u32, u32)],
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
                self.vdevice
                    .CreateVideoProcessorOutputView(
                        framebuffer,
                        &self.enumerator,
                        &od,
                        Some(&mut ov),
                    )
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "CreateVideoProcessorOutputView on the framebuffer failed")
                    })?;
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
                .CreateVideoProcessorInputView(ext_tex, &self.enumerator, &id, Some(&mut iv))
                .inspect_err(|e| {
                    // Which call failed matters: an input-view rejection is about
                    // how the DECODER's texture was created (bind flags, size),
                    // a blt rejection is about the rectangles. Naming it here
                    // saves guessing from a bare E_INVALIDARG.
                    tracing::warn!(
                        error = %e,
                        "CreateVideoProcessorInputView rejected the decoder texture"
                    )
                })?;
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

            self.blt_regions(input_view, &output_view, dest_x, dest_y, w, h, regions)
        }
    }
}

/// GPU RGBA scaler (smart-sizing) built on the Direct3D 11 video processor: it
/// scales a slice of the desktop framebuffer to fill the (differently sized)
/// backbuffer. Both surfaces are RGBA, so this is a pure scale — no color
/// conversion. `in_size` is the full framebuffer texture size (the content
/// description must match the bound input view); the slice is chosen per blit
/// via the stream source rectangle.
struct RgbaScaler {
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_size: (u32, u32),
    out_size: (u32, u32),
}

/// PCI vendor IDs, for gating the vendor-specific video-processor extensions.
const VENDOR_NVIDIA: u32 = 0x10de;
const VENDOR_INTEL: u32 = 0x8086;
const VENDOR_AMD: u32 = 0x1002;

/// The PCI vendor ID of the adapter behind `device` (`None` on any DXGI failure).
unsafe fn adapter_vendor_id(device: &ID3D11Device) -> Option<u32> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter = dxgi.GetAdapter().ok()?;
    let desc = adapter.GetDesc().ok()?;
    Some(desc.VendorId)
}

/// Try to enable NVIDIA RTX Video Super Resolution on `processor`'s stream 0 — the
/// Tensor-core AI upscaler Edge/Chrome use for video, exposed through the D3D11
/// video-processor vendor extension. Best-effort: returns `false` (and the blit
/// falls back to the driver's default bilinear scale) on any non-RTX GPU, older
/// driver, or unsupported path.
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

/// Try to enable Intel VPE Super Resolution on `processor` — Intel's AI video
/// upscaler (iGPU/Arc "Video Processing Engine"), exposed through output
/// extensions on the same GUID + function codes Chromium uses. Best-effort:
/// any failing call returns `false` and the blit stays on the driver's plain
/// bilinear scale.
unsafe fn enable_intel_vpe_super_resolution(
    vcontext: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
) -> bool {
    const INTEL_VPE_INTERFACE: windows::core::GUID = windows::core::GUID::from_values(
        0xedd1d4b9,
        0x8659,
        0x4cbc,
        [0xa4, 0xd6, 0x98, 0x31, 0xa2, 0x16, 0x3a, 0xc3],
    );
    const VPE_FN_VERSION: u32 = 0x01;
    const VPE_FN_MODE: u32 = 0x20;
    const VPE_FN_SCALING: u32 = 0x37;
    const VPE_VERSION_3: u32 = 0x0003;
    const VPE_MODE_PREPROC: u32 = 0x01;
    const VPE_SCALING_SUPER_RESOLUTION: u32 = 0x2;
    /// The in-process Intel extension payload: a function selector plus a
    /// pointer to its parameter value.
    #[repr(C)]
    struct IntelVpeExt {
        function: u32,
        param: *mut core::ffi::c_void,
    }
    let mut param: u32 = 0;
    let mut ext = IntelVpeExt {
        function: 0,
        param: &mut param as *mut u32 as *mut core::ffi::c_void,
    };
    let mut set = |function: u32, value: u32| -> bool {
        ext.function = function;
        param = value;
        let hr = vcontext.VideoProcessorSetOutputExtension(
            processor,
            &INTEL_VPE_INTERFACE,
            std::mem::size_of::<IntelVpeExt>() as u32,
            &ext as *const IntelVpeExt as *const core::ffi::c_void,
        );
        hr >= 0
    };
    set(VPE_FN_VERSION, VPE_VERSION_3)
        && set(VPE_FN_MODE, VPE_MODE_PREPROC)
        && set(VPE_FN_SCALING, VPE_SCALING_SUPER_RESOLUTION)
}

impl RgbaScaler {
    /// `in_w`/`in_h` are the FULL framebuffer texture dimensions; `out_w`/`out_h`
    /// the destination surface. With `enable_vsr` the vendor AI super-resolution
    /// is switched on when the adapter supports one (NVIDIA RTX VSR, Intel VPE SR).
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
            // AI SR is opt-in (the `Vsr` upscaler) and vendor-gated: the NVIDIA
            // and Intel extensions are only offered to their own drivers. When
            // requested but unavailable (AMD, old driver) the blit silently uses
            // the driver's bilinear scale — same path as the `Bilinear` upscaler.
            if enable_vsr {
                let vendor = adapter_vendor_id(device);
                let engaged = match vendor {
                    Some(VENDOR_NVIDIA) => enable_rtx_super_resolution(&vcontext, &processor)
                        .then_some("NVIDIA RTX Video Super Resolution"),
                    Some(VENDOR_INTEL) => enable_intel_vpe_super_resolution(&vcontext, &processor)
                        .then_some("Intel VPE Super Resolution"),
                    _ => None,
                };
                match engaged {
                    Some(name) => {
                        tracing::info!(in_w, in_h, out_w, out_h, "client upscale: {name}");
                    }
                    None => {
                        let hint = match vendor {
                            Some(VENDOR_AMD) => {
                                "no AMD driver AI SR — use --upscale fsr for shader upscaling"
                            }
                            Some(VENDOR_NVIDIA) | Some(VENDOR_INTEL) => {
                                "driver refused AI SR — update the GPU driver, or use --upscale fsr"
                            }
                            _ => "no AI SR on this adapter — use --upscale fsr",
                        };
                        tracing::info!(
                            in_w,
                            in_h,
                            out_w,
                            out_h,
                            "client upscale: bilinear ({hint})"
                        );
                    }
                }
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

    /// Scale the `src_rect` slice of `framebuffer` (RGBA) to fill `backbuffer`
    /// (RGBA, `out_w`×`out_h`).
    fn blit(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        backbuffer: &ID3D11Texture2D,
        src_rect: (u32, u32, u32, u32),
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

            let (sx, sy, sw, sh) = src_rect;
            let src = RECT {
                left: sx as i32,
                top: sy as i32,
                right: (sx + sw) as i32,
                bottom: (sy + sh) as i32,
            };
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

/// Runtime-compiled scale/sharpen shader collection: a fullscreen-triangle
/// vertex shader plus one pixel-shader kernel per [`ScaleKernel`].
///
/// - `ps_bicubic` — 9-tap (3×3 bilinear-sample) Catmull-Rom (Matt Pettineo's
///   well-worn formulation). Sharp without the ringing/edge-hallucination an AI
///   video upscaler produces on text and UI — the default for the mixed
///   desktop + game content RDP carries.
/// - `ps_easu` — AMD FidelityFX Super Resolution 1.0 edge-adaptive spatial
///   upsampling (`--upscale fsr`): a 12-tap window fits a local edge
///   direction/length and stretches an anisotropic lanczos-like kernel along
///   it. Ported from AMD's MIT-licensed `ffx_fsr1.h`, with exact rcp/rsqrt
///   (plus tiny denominator guards) in place of the bit-trick approximations.
/// - `ps_nearest` — point sampling; pixel-perfect at exact integer ratios.
/// - `ps_rcas` — FSR 1.0 robust contrast-adaptive sharpening, run 1:1 after an
///   upscale (or straight on the framebuffer). Per-pixel limited so existing
///   edges don't over-ring, with the noise-detection term enabled so H.264
///   compression noise is not amplified.
///
/// Every kernel reads an absolute `srcOff`/`srcSize` slice of the bound
/// texture and clamps all taps to that slice, so per-monitor slices never
/// bleed pixels across a monitor seam. Alpha is forced opaque (the swapchain
/// is opaque) to match the VideoProcessor path.
const SCALE_HLSL: &str = r#"
Texture2D src : register(t0);
SamplerState samp : register(s0);
cbuffer Params : register(b0) {
    float2 srcOff;      // slice top-left within the bound texture, texels
    float2 srcSize;     // slice size, texels
    float2 outSize;     // destination size, pixels
    float2 invTexSize;  // 1 / full bound-texture size
    float2 texSize;     // full bound-texture size
    float  sharpness;   // RCAS linear intensity (exp2(-stops))
    float  _pad;
};

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut vs_main(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// Load an absolute texel, clamped to the slice (seam-safe edge clamp).
float3 loadTexel(float2 p) {
    float2 c = clamp(p, srcOff, srcOff + srcSize - 1.0);
    return src.Load(int3(int2(c), 0)).rgb;
}

float4 ps_bicubic(VSOut i) : SV_Target {
    // Absolute sample position in corner space (texel centres at .5).
    float2 samplePos = srcOff + i.uv * srcSize;
    float2 texPos1 = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - texPos1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 offset12 = w2 / w12;
    // Clamp every tap to the slice so a seam never bleeds, then to UV.
    float2 lo = srcOff + 0.5;
    float2 hi = srcOff + srcSize - 0.5;
    float2 p0  = clamp(texPos1 - 1.0,      lo, hi) * invTexSize;
    float2 p3  = clamp(texPos1 + 2.0,      lo, hi) * invTexSize;
    float2 p12 = clamp(texPos1 + offset12, lo, hi) * invTexSize;
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

float4 ps_nearest(VSOut i) : SV_Target {
    float2 samplePos = srcOff + i.uv * srcSize;
    return float4(loadTexel(floor(samplePos)), 1.0);
}

// FSR 1.0 luma proxy: 0.5*R + G + 0.5*B (shared by EASU and RCAS).
float fsrLuma(float3 c) { return 0.5 * c.r + c.g + 0.5 * c.b; }

// Accumulate the edge direction and length for one of the 2x2 centre texels,
// weighted by its bilinear weight w. The '+' pattern:  a / b c d / e.
void easuSet(inout float2 dir, inout float len, float w,
             float lA, float lB, float lC, float lD, float lE) {
    float dc = lD - lC;
    float cb = lC - lB;
    float lenX = max(abs(dc), abs(cb));
    lenX = 1.0 / max(lenX, 1.0 / 32768.0);
    float dirX = lD - lB;
    dir.x += dirX * w;
    lenX = saturate(abs(dirX) * lenX);
    lenX *= lenX;
    len += lenX * w;
    float ec = lE - lC;
    float ca = lC - lA;
    float lenY = max(abs(ec), abs(ca));
    lenY = 1.0 / max(lenY, 1.0 / 32768.0);
    float dirY = lE - lA;
    dir.y += dirY * w;
    lenY = saturate(abs(dirY) * lenY);
    lenY *= lenY;
    len += lenY * w;
}

// One EASU accumulation tap: rotate the offset into the edge frame, apply the
// anisotropic window, and weigh with the polynomial lanczos2 approximation.
void easuTap(inout float3 aC, inout float aW, float2 off, float2 dir,
             float2 len, float lob, float clp, float3 c) {
    float2 v;
    v.x = off.x * dir.x + off.y * dir.y;
    v.y = -off.x * dir.y + off.y * dir.x;
    v *= len;
    float d2 = min(v.x * v.x + v.y * v.y, clp);
    float wB = 0.4 * d2 - 1.0;
    float wA = lob * d2 - 1.0;
    wB *= wB;
    wA *= wA;
    wB = 1.5625 * wB - 0.5625;
    float w = wB * wA;
    aC += c * w;
    aW += w;
}

float4 ps_easu(VSOut i) : SV_Target {
    // pp: absolute source position in texel-centre space (texel n centre = n.0).
    float2 pp = srcOff + i.uv * srcSize - 0.5;
    float2 fp = floor(pp);
    float2 f = pp - fp;
    // 12-tap window:    b c
    //                 e f g h
    //                 i j k l
    //                   n o
    float3 cB = loadTexel(fp + float2( 0.0, -1.0));
    float3 cC = loadTexel(fp + float2( 1.0, -1.0));
    float3 cE = loadTexel(fp + float2(-1.0,  0.0));
    float3 cF = loadTexel(fp + float2( 0.0,  0.0));
    float3 cG = loadTexel(fp + float2( 1.0,  0.0));
    float3 cH = loadTexel(fp + float2( 2.0,  0.0));
    float3 cI = loadTexel(fp + float2(-1.0,  1.0));
    float3 cJ = loadTexel(fp + float2( 0.0,  1.0));
    float3 cK = loadTexel(fp + float2( 1.0,  1.0));
    float3 cL = loadTexel(fp + float2( 2.0,  1.0));
    float3 cN = loadTexel(fp + float2( 0.0,  2.0));
    float3 cO = loadTexel(fp + float2( 1.0,  2.0));
    float lB = fsrLuma(cB); float lC = fsrLuma(cC);
    float lE = fsrLuma(cE); float lF = fsrLuma(cF);
    float lG = fsrLuma(cG); float lH = fsrLuma(cH);
    float lI = fsrLuma(cI); float lJ = fsrLuma(cJ);
    float lK = fsrLuma(cK); float lL = fsrLuma(cL);
    float lN = fsrLuma(cN); float lO = fsrLuma(cO);
    float2 dir = float2(0.0, 0.0);
    float len = 0.0;
    easuSet(dir, len, (1.0 - f.x) * (1.0 - f.y), lB, lE, lF, lG, lJ);
    easuSet(dir, len, f.x * (1.0 - f.y),         lC, lF, lG, lH, lK);
    easuSet(dir, len, (1.0 - f.x) * f.y,         lF, lI, lJ, lK, lN);
    easuSet(dir, len, f.x * f.y,                 lG, lJ, lK, lL, lO);
    // Normalise the direction; a flat region gets the neutral (1, 0) frame.
    float dirR = dir.x * dir.x + dir.y * dir.y;
    bool zro = dirR < (1.0 / 32768.0);
    dirR = rsqrt(max(dirR, 1.0 / 32768.0));
    dirR = zro ? 1.0 : dirR;
    dir.x = zro ? 1.0 : dir.x;
    dir *= dirR;
    // Edge length in {0..1}, shaped; kernel stretch along the edge.
    len = len * 0.5;
    len *= len;
    float stretch = 1.0 / max(max(abs(dir.x), abs(dir.y)), 1.0 / 32768.0);
    float2 len2 = float2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    // Negative lobe strength and the matching window clip.
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = 1.0 / max(lob, 1.0 / 32768.0);
    // Deringing bounds from the 2x2 centre quad.
    float3 min4 = min(min(cF, cG), min(cJ, cK));
    float3 max4 = max(max(cF, cG), max(cJ, cK));
    float3 aC = float3(0.0, 0.0, 0.0);
    float aW = 0.0;
    easuTap(aC, aW, float2( 0.0, -1.0) - f, dir, len2, lob, clp, cB);
    easuTap(aC, aW, float2( 1.0, -1.0) - f, dir, len2, lob, clp, cC);
    easuTap(aC, aW, float2(-1.0,  0.0) - f, dir, len2, lob, clp, cE);
    easuTap(aC, aW, float2( 0.0,  0.0) - f, dir, len2, lob, clp, cF);
    easuTap(aC, aW, float2( 1.0,  0.0) - f, dir, len2, lob, clp, cG);
    easuTap(aC, aW, float2( 2.0,  0.0) - f, dir, len2, lob, clp, cH);
    easuTap(aC, aW, float2(-1.0,  1.0) - f, dir, len2, lob, clp, cI);
    easuTap(aC, aW, float2( 0.0,  1.0) - f, dir, len2, lob, clp, cJ);
    easuTap(aC, aW, float2( 1.0,  1.0) - f, dir, len2, lob, clp, cK);
    easuTap(aC, aW, float2( 2.0,  1.0) - f, dir, len2, lob, clp, cL);
    easuTap(aC, aW, float2( 0.0,  2.0) - f, dir, len2, lob, clp, cN);
    easuTap(aC, aW, float2( 1.0,  2.0) - f, dir, len2, lob, clp, cO);
    float3 pix = min(max4, max(min4, aC * (1.0 / aW)));
    return float4(pix, 1.0);
}

float4 ps_rcas(VSOut i) : SV_Target {
    // 1:1 mapping: output pixel -> slice texel.
    float2 ip = srcOff + floor(i.uv * outSize);
    // Cross pattern:   b
    //                d e f
    //                  h
    float3 b = loadTexel(ip + float2( 0.0, -1.0));
    float3 d = loadTexel(ip + float2(-1.0,  0.0));
    float3 e = loadTexel(ip);
    float3 f = loadTexel(ip + float2( 1.0,  0.0));
    float3 h = loadTexel(ip + float2( 0.0,  1.0));
    float bL = fsrLuma(b);
    float dL = fsrLuma(d);
    float eL = fsrLuma(e);
    float fL = fsrLuma(f);
    float hL = fsrLuma(h);
    // Noise detection: back sharpening off where the cross looks like grain
    // (H.264 blocking/ringing), so compression noise is not amplified.
    float nz = 0.25 * (bL + dL + fL + hL) - eL;
    float rangeMax = max(max(max(bL, dL), max(eL, fL)), hL);
    float rangeMin = min(min(min(bL, dL), min(eL, fL)), hL);
    nz = saturate(abs(nz) / max(rangeMax - rangeMin, 1.0 / 32768.0));
    nz = -0.5 * nz + 1.0;
    // Per-channel limiters so the negative ring lobe never clips.
    float3 mn4 = min(min(b, d), min(f, h));
    float3 mx4 = max(max(b, d), max(f, h));
    float2 peakC = float2(1.0, -4.0);
    float3 hitMin = mn4 / max(4.0 * mx4, 1.0 / 32768.0);
    float3 hitMax = (peakC.x - mx4) / (4.0 * mn4 + peakC.y - (1.0 / 32768.0));
    float3 lobeRGB = max(-hitMin, hitMax);
    float lobe = max(-0.1875, min(max(max(lobeRGB.r, lobeRGB.g), lobeRGB.b), 0.0)) * sharpness;
    lobe *= nz;
    float rcpL = 1.0 / (4.0 * lobe + 1.0);
    float3 pix = ((b + d + f + h) * lobe + e) * rcpL;
    return float4(pix, 1.0);
}
"#;

/// Which pixel-shader kernel a [`ShaderScaler`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScaleKernel {
    /// Catmull-Rom bicubic upscale (the [`Upscaler::Bicubic`] default).
    Bicubic,
    /// FSR 1.0 EASU edge-adaptive upscale ([`Upscaler::Fsr`]).
    Easu,
    /// Point-sampled upscale ([`Upscaler::Nearest`]).
    Nearest,
    /// FSR 1.0 RCAS adaptive sharpen (the 1:1 `--sharpen` pass).
    Rcas,
}

impl ScaleKernel {
    fn entry(self) -> PCSTR {
        match self {
            Self::Bicubic => windows::core::s!("ps_bicubic"),
            Self::Easu => windows::core::s!("ps_easu"),
            Self::Nearest => windows::core::s!("ps_nearest"),
            Self::Rcas => windows::core::s!("ps_rcas"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Bicubic => "Catmull-Rom bicubic (shader)",
            Self::Easu => "FSR 1.0 EASU (shader)",
            Self::Nearest => "nearest-neighbour (shader)",
            Self::Rcas => "FSR 1.0 RCAS sharpen (shader)",
        }
    }
}

/// Constant buffer for [`SCALE_HLSL`] — must mirror the HLSL `Params` block.
#[repr(C)]
struct ScaleParams {
    src_off: [f32; 2],
    src_size: [f32; 2],
    out_size: [f32; 2],
    inv_tex_size: [f32; 2],
    tex_size: [f32; 2],
    sharpness: f32,
    _pad: f32,
}

/// Compile one stage of [`SCALE_HLSL`] at runtime via d3dcompiler. Returns the
/// bytecode blob, logging the compiler's diagnostics on failure.
unsafe fn compile_scale_shader(entry: PCSTR, target: PCSTR) -> WinResult<ID3DBlob> {
    compile_hlsl(SCALE_HLSL, windows::core::s!("scale.hlsl"), entry, target)
}

/// Compile one entry point of an HLSL source at runtime via d3dcompiler,
/// logging the compiler's diagnostics on failure.
unsafe fn compile_hlsl(
    source: &str,
    name: PCSTR,
    entry: PCSTR,
    target: PCSTR,
) -> WinResult<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = D3DCompile(
        source.as_ptr() as *const core::ffi::c_void,
        source.len(),
        name,
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
            tracing::warn!(error = %String::from_utf8_lossy(msg), "scale HLSL compile failed");
        }
        return Err(e);
    }
    Ok(code.expect("shader blob present on D3DCompile success"))
}

/// One GPU scale/sharpen pass: renders the `src_rect` slice of an RGBA texture
/// into an RGBA target with a single fullscreen draw of the selected
/// [`ScaleKernel`]. Owns its compiled shaders, a clamped linear sampler, and an
/// immutable constant buffer holding the slice geometry; the input SRV and
/// output RTV are created per blit (cheap, and the pass is rebuilt whenever its
/// geometry changes — see [`UpscalePipeline`]). Holds its own device + context
/// clones so a blit borrows neither while the renderer holds the pass mutably.
struct ShaderScaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    kernel: ScaleKernel,
    src_rect: (u32, u32, u32, u32),
    out_size: (u32, u32),
    tex_size: (u32, u32),
    /// Bit pattern of the linear RCAS sharpness baked into `params`.
    sharp_bits: u32,
}

impl ShaderScaler {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        kernel: ScaleKernel,
        tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        out_size: (u32, u32),
        sharpness: f32,
    ) -> WinResult<Self> {
        unsafe {
            let vs_blob = compile_scale_shader(
                windows::core::s!("vs_main"),
                windows::core::s!("vs_5_0"),
            )?;
            let ps_blob = compile_scale_shader(kernel.entry(), windows::core::s!("ps_5_0"))?;
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

            // Bilinear taps, clamped at the edges (only the bicubic kernel
            // samples; the others use raw loads with their own slice clamp).
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

            let params = ScaleParams {
                src_off: [src_rect.0 as f32, src_rect.1 as f32],
                src_size: [src_rect.2 as f32, src_rect.3 as f32],
                out_size: [out_size.0 as f32, out_size.1 as f32],
                inv_tex_size: [1.0 / tex_size.0.max(1) as f32, 1.0 / tex_size.1.max(1) as f32],
                tex_size: [tex_size.0 as f32, tex_size.1 as f32],
                sharpness,
                _pad: 0.0,
            };
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: core::mem::size_of::<ScaleParams>() as u32,
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

            tracing::info!(
                ?src_rect,
                out_w = out_size.0,
                out_h = out_size.1,
                "client scale pass: {}",
                kernel.label()
            );
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.expect("vertex shader"),
                ps: ps.expect("pixel shader"),
                sampler: sampler.expect("sampler"),
                params: cb.expect("constant buffer"),
                kernel,
                src_rect,
                out_size,
                tex_size,
                sharp_bits: sharpness.to_bits(),
            })
        }
    }

    /// Whether this pass was built for a different geometry/kernel/sharpness.
    fn stale(
        &self,
        kernel: ScaleKernel,
        tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        out_size: (u32, u32),
        sharpness: f32,
    ) -> bool {
        self.kernel != kernel
            || self.tex_size != tex_size
            || self.src_rect != src_rect
            || self.out_size != out_size
            || self.sharp_bits != sharpness.to_bits()
    }

    /// Render the pass: `src` (slice per the constant buffer) → `dst` with one draw.
    fn blit(&mut self, src: &ID3D11Texture2D, dst: &ID3D11Texture2D) -> WinResult<()> {
        unsafe {
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            self.device
                .CreateShaderResourceView(src, None, Some(&mut srv))?;
            let srv = srv.expect("shader resource view");
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(dst, None, Some(&mut rtv))?;
            let rtv = rtv.expect("render target view");

            let ctx = &self.context;
            let rtvs = [Some(rtv.clone())];
            ctx.OMSetRenderTargets(Some(&rtvs), None);
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.out_size.0 as f32,
                Height: self.out_size.1 as f32,
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

            // Unbind the SRV and RTV so the next frame's framebuffer writes
            // (decode blits / bitmap updates) can't collide with a stale binding.
            let no_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.PSSetShaderResources(0, Some(&no_srv));
            let no_rtv: [Option<ID3D11RenderTargetView>; 1] = [None];
            ctx.OMSetRenderTargets(Some(&no_rtv), None);
            Ok(())
        }
    }
}

/// NV12 → RGBA conversion, sampling the decoder's two planes directly.
///
/// The colour matrix is BT.709 limited-range, the same one
/// `rdp_graphics::yuv::yuv_to_rgb` uses on the CPU (coefficients there are
/// scaled by 256: 298 / 459 / 55 / 136 / 541), so the GPU and CPU paths produce
/// identical pixels.
const NV12_HLSL: &str = r#"
Texture2D<float>  Luma   : register(t0);
Texture2D<float2> Chroma : register(t1);
SamplerState samp : register(s0);

cbuffer Nv12Params : register(b0) {
    float2 src_off;    // region origin, normalised to the NV12 texture
    float2 src_scale;  // region size, normalised to the NV12 texture
};

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

// Fullscreen triangle. The viewport is set to the destination rectangle, so
// this covers exactly that rectangle and `uv` runs 0..1 across it.
VSOut vs_main(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 ps_nv12(VSOut i) : SV_Target {
    // One normalised coordinate serves both planes: the chroma plane view is
    // half-size, so the sampler's bilinear filter does the 4:2:0 upsample.
    float2 uv = src_off + i.uv * src_scale;
    float  y  = Luma.Sample(samp, uv);
    float2 c  = Chroma.Sample(samp, uv);
    float  yy = 1.1640625 * (y - 0.0627451);   // 298/256, 16/255
    float  u  = c.x - 0.5019608;               // 128/255
    float  v  = c.y - 0.5019608;
    return float4(
        saturate(yy + 1.7929688 * v),                       // 459/256
        saturate(yy - 0.2148438 * u - 0.5312500 * v),       // 55/256, 136/256
        saturate(yy + 2.1132813 * u),                       // 541/256
        1.0);
}
"#;

/// Constant buffer for [`NV12_HLSL`] — 16 bytes, the constant-buffer minimum.
#[repr(C)]
struct Nv12Params {
    src_off: [f32; 2],
    src_scale: [f32; 2],
}

/// GPU NV12 → RGBA conversion done with a pixel shader rather than the D3D11
/// video processor.
///
/// The video processor is the obvious tool for this and was the original path,
/// but `CreateVideoProcessorInputView` rejects a decoder-copied NV12 surface
/// outright on some drivers: Intel UHD Graphics returns E_INVALIDARG for it
/// regardless of the surface's size or bind flags. Worse, the CPU readback that
/// failure fell back to is not merely slow — a blocking staging `Map` plus a
/// multi-megapixel software colour convert, per frame, ran for *seconds* at
/// 2256x1504 and tripped the GPU watchdog, removing the device and taking the
/// session with it.
///
/// Sampling the planes as ordinary shader resources has none of those
/// constraints. Plane views — R8 for luma, R8G8 for the interleaved chroma — are
/// core D3D11 with no video API anywhere in the path, the bilinear sampler
/// upsamples chroma for free, and the work happens where the pixels already are.
struct Nv12Shader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    /// Render target view on the desktop framebuffer. Cached across frames and
    /// dropped by [`D3D11Renderer::ensure_framebuffer`] when it is reallocated.
    rtv: Option<ID3D11RenderTargetView>,
}

impl Nv12Shader {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> WinResult<Self> {
        unsafe {
            let vs_blob = compile_hlsl(
                NV12_HLSL,
                windows::core::s!("nv12.hlsl"),
                windows::core::s!("vs_main"),
                windows::core::s!("vs_5_0"),
            )?;
            let ps_blob = compile_hlsl(
                NV12_HLSL,
                windows::core::s!("nv12.hlsl"),
                windows::core::s!("ps_nv12"),
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

            let bd = D3D11_BUFFER_DESC {
                ByteWidth: core::mem::size_of::<Nv12Params>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut cb: Option<ID3D11Buffer> = None;
            device.CreateBuffer(&bd, None, Some(&mut cb))?;

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.expect("nv12 vertex shader"),
                ps: ps.expect("nv12 pixel shader"),
                sampler: sampler.expect("nv12 sampler"),
                params: cb.expect("nv12 constant buffer"),
                rtv: None,
            })
        }
    }

    /// A single-plane view onto an NV12 texture: `R8_UNORM` selects the luma
    /// plane, `R8G8_UNORM` the (half-resolution) interleaved chroma plane.
    unsafe fn plane_srv(
        &self,
        tex: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> WinResult<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        self.device
            .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))?;
        Ok(srv.expect("nv12 plane view"))
    }

    /// Convert `tex`'s dirty `regions` into the framebuffer at `(dest_x,dest_y)`,
    /// one draw per region with the viewport clipping to it.
    #[allow(clippy::too_many_arguments)]
    fn blit(
        &mut self,
        framebuffer: &ID3D11Texture2D,
        tex: &ID3D11Texture2D,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        fb_size: (u32, u32),
        regions: &[(u32, u32, u32, u32)],
    ) -> WinResult<()> {
        unsafe {
            let mut td: D3D11_TEXTURE2D_DESC = core::mem::zeroed();
            tex.GetDesc(&mut td);
            // The decoder's surface is macroblock-padded, so normalise against
            // its real size — the display region is a sub-rect of it.
            let (tw, th) = (td.Width.max(1) as f32, td.Height.max(1) as f32);

            if self.rtv.is_none() {
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                self.device
                    .CreateRenderTargetView(framebuffer, None, Some(&mut rtv))?;
                self.rtv = rtv;
            }
            let rtv = self.rtv.clone().expect("nv12 render target view");
            let luma = self.plane_srv(tex, DXGI_FORMAT_R8_UNORM)?;
            let chroma = self.plane_srv(tex, DXGI_FORMAT_R8G8_UNORM)?;

            let ctx = &self.context;
            let rtvs = [Some(rtv)];
            ctx.OMSetRenderTargets(Some(&rtvs), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            let srvs = [Some(luma), Some(chroma)];
            ctx.PSSetShaderResources(0, Some(&srvs));
            let samplers = [Some(self.sampler.clone())];
            ctx.PSSetSamplers(0, Some(&samplers));
            let cbs = [Some(self.params.clone())];
            ctx.PSSetConstantBuffers(0, Some(&cbs));

            let whole = [(0u32, 0u32, w, h)];
            let regions = if regions.is_empty() { &whole[..] } else { regions };
            for &(rx, ry, rw, rh) in regions {
                if rx >= w || ry >= h {
                    continue;
                }
                let dx = dest_x + rx;
                let dy = dest_y + ry;
                let cw = rw.min(w - rx).min(fb_size.0.saturating_sub(dx));
                let ch = rh.min(h - ry).min(fb_size.1.saturating_sub(dy));
                if cw == 0 || ch == 0 {
                    continue;
                }
                let p = Nv12Params {
                    src_off: [rx as f32 / tw, ry as f32 / th],
                    src_scale: [cw as f32 / tw, ch as f32 / th],
                };
                ctx.UpdateSubresource(
                    &self.params,
                    0,
                    None,
                    &p as *const _ as *const core::ffi::c_void,
                    0,
                    0,
                );
                let vp = D3D11_VIEWPORT {
                    TopLeftX: dx as f32,
                    TopLeftY: dy as f32,
                    Width: cw as f32,
                    Height: ch as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                ctx.RSSetViewports(Some(&[vp]));
                ctx.Draw(3, 0);
            }

            // Unbind so the next framebuffer write (a bitmap update, a cache
            // blit) can't collide with a stale SRV/RTV binding.
            let no_srv: [Option<ID3D11ShaderResourceView>; 2] = [None, None];
            ctx.PSSetShaderResources(0, Some(&no_srv));
            let no_rtv: [Option<ID3D11RenderTargetView>; 1] = [None];
            ctx.OMSetRenderTargets(Some(&no_rtv), None);
            Ok(())
        }
    }
}

/// The per-surface upscale + sharpen pass chain run at present time — one
/// instance for the primary swapchain and one per per-monitor target.
///
/// [`Self::run`] scales the `src_rect` slice of the framebuffer to fill the
/// destination with the selected [`Upscaler`], then applies the optional RCAS
/// sharpen:
///
/// - shader kernels (`Bicubic`/`Fsr`/`Nearest`) render via [`ShaderScaler`];
///   a failed build/draw latches them off and drops to VideoProcessor bilinear
/// - `Vsr`/`Bilinear` scale through the [`RgbaScaler`] VideoProcessor path
///   (with the vendor AI super-resolution engaged for `Vsr` where available)
/// - with sharpen, the upscale lands in an intermediate `mid` texture and the
///   RCAS pass writes the final image; RCAS failure ships the unsharpened
///   image and latches sharpening off
///
/// Every stage returns `false` on unrecoverable failure so the caller can drop
/// to the 1:1 crop copy — the picture is never lost.
#[derive(Default)]
struct UpscalePipeline {
    scale: Option<ShaderScaler>,
    scale_disabled: bool,
    vp: Option<RgbaScaler>,
    vp_disabled: bool,
    rcas: Option<ShaderScaler>,
    rcas_disabled: bool,
    /// Intermediate upscale target when sharpening: `(texture, size)`.
    mid: Option<(ID3D11Texture2D, (u32, u32))>,
}

impl UpscalePipeline {
    /// Drop every built pass (mode/sharpen/size change); all rebuild lazily.
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Paint `dst` (`dst_size`) from the `src_rect` slice of `src`
    /// (`src_tex_size` full texels). Returns `false` if nothing was painted —
    /// every usable path failed or nothing needed doing — so the caller can
    /// fall back to the 1:1 crop copy.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        mode: Upscaler,
        sharpen: f32,
        src: &ID3D11Texture2D,
        src_tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        dst: &ID3D11Texture2D,
        dst_size: (u32, u32),
    ) -> bool {
        let (sx, sy, sw, sh) = src_rect;
        if sw == 0 || sh == 0 || dst_size.0 == 0 || dst_size.1 == 0 {
            return false;
        }
        // A server-driven resize can briefly disagree with the present layout;
        // crop-copy until the slice geometry is inside the framebuffer again.
        if sx + sw > src_tex_size.0 || sy + sh > src_tex_size.1 {
            return false;
        }
        let needs_scale = (sw, sh) != dst_size;
        let sharpen_on = sharpen > 0.0 && !self.rcas_disabled;
        if !needs_scale {
            // 1:1 — only the sharpen pass could have work to do.
            if !sharpen_on {
                return false;
            }
            return self.run_rcas(device, context, sharpen, src, src_tex_size, (sx, sy), dst, dst_size);
        }
        if sharpen_on {
            if let Some(mid) = self.ensure_mid(device, dst_size) {
                if !self.run_scale(device, context, mode, src, src_tex_size, src_rect, &mid, dst_size)
                {
                    return false;
                }
                if !self.run_rcas(device, context, sharpen, &mid, dst_size, (0, 0), dst, dst_size) {
                    // RCAS failed: ship the unsharpened upscale rather than nothing.
                    unsafe { context.CopySubresourceRegion(dst, 0, 0, 0, 0, &mid, 0, None) };
                }
                return true;
            }
            // Mid alloc failed: plain unsharpened upscale from here on.
            self.rcas_disabled = true;
        }
        self.run_scale(device, context, mode, src, src_tex_size, src_rect, dst, dst_size)
    }

    /// The upscale stage: shader kernel (with VideoProcessor bilinear fallback)
    /// or the VideoProcessor directly for `Vsr`/`Bilinear`.
    #[allow(clippy::too_many_arguments)]
    fn run_scale(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        mode: Upscaler,
        src: &ID3D11Texture2D,
        src_tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        dst: &ID3D11Texture2D,
        dst_size: (u32, u32),
    ) -> bool {
        let kernel = match mode {
            Upscaler::Bicubic => Some(ScaleKernel::Bicubic),
            Upscaler::Fsr => Some(ScaleKernel::Easu),
            Upscaler::Nearest => Some(ScaleKernel::Nearest),
            Upscaler::Vsr | Upscaler::Bilinear => None,
        };
        if let Some(kernel) = kernel {
            if !self.scale_disabled
                && self.run_shader(device, context, kernel, src, src_tex_size, src_rect, dst, dst_size)
            {
                return true;
            }
            // Shader unavailable → clean bilinear rather than a hard crop.
            return self.run_vp(device, context, false, src, src_tex_size, src_rect, dst, dst_size);
        }
        let want_vsr = matches!(mode, Upscaler::Vsr);
        self.run_vp(device, context, want_vsr, src, src_tex_size, src_rect, dst, dst_size)
    }

    /// Run (building if stale) the shader upscale kernel into `dst`.
    #[allow(clippy::too_many_arguments)]
    fn run_shader(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        kernel: ScaleKernel,
        src: &ID3D11Texture2D,
        src_tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        dst: &ID3D11Texture2D,
        dst_size: (u32, u32),
    ) -> bool {
        let stale = match &self.scale {
            None => true,
            Some(s) => s.stale(kernel, src_tex_size, src_rect, dst_size, 0.0),
        };
        if stale {
            match ShaderScaler::new(device, context, kernel, src_tex_size, src_rect, dst_size, 0.0) {
                Ok(s) => self.scale = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, kernel = kernel.label(), "scale shader unavailable; using bilinear");
                    self.scale_disabled = true;
                    return false;
                }
            }
        }
        let s = self.scale.as_mut().expect("scale shader present");
        match s.blit(src, dst) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, kernel = kernel.label(), "scale shader blit failed; using bilinear");
                self.scale_disabled = true;
                false
            }
        }
    }

    /// Run (building if stale) the 1:1 RCAS sharpen from `src`+`src_off` into `dst`.
    #[allow(clippy::too_many_arguments)]
    fn run_rcas(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        sharpen: f32,
        src: &ID3D11Texture2D,
        src_tex_size: (u32, u32),
        src_off: (u32, u32),
        dst: &ID3D11Texture2D,
        dst_size: (u32, u32),
    ) -> bool {
        // Strength 0..=1 → RCAS attenuation "stops" (0 = maximum) → linear scale.
        let stops = 2.0 * (1.0 - sharpen.clamp(0.0, 1.0));
        let sharpness = (-stops).exp2();
        let src_rect = (src_off.0, src_off.1, dst_size.0, dst_size.1);
        let stale = match &self.rcas {
            None => true,
            Some(s) => s.stale(ScaleKernel::Rcas, src_tex_size, src_rect, dst_size, sharpness),
        };
        if stale {
            match ShaderScaler::new(
                device,
                context,
                ScaleKernel::Rcas,
                src_tex_size,
                src_rect,
                dst_size,
                sharpness,
            ) {
                Ok(s) => self.rcas = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "RCAS sharpen unavailable; presenting unsharpened");
                    self.rcas_disabled = true;
                    return false;
                }
            }
        }
        let s = self.rcas.as_mut().expect("rcas shader present");
        match s.blit(src, dst) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "RCAS blit failed; presenting unsharpened");
                self.rcas_disabled = true;
                false
            }
        }
    }

    /// Run (building if stale) the VideoProcessor scale, optionally with the
    /// vendor AI super-resolution.
    #[allow(clippy::too_many_arguments)]
    fn run_vp(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        enable_vsr: bool,
        src: &ID3D11Texture2D,
        src_tex_size: (u32, u32),
        src_rect: (u32, u32, u32, u32),
        dst: &ID3D11Texture2D,
        dst_size: (u32, u32),
    ) -> bool {
        if self.vp_disabled {
            return false;
        }
        let stale = match &self.vp {
            None => true,
            Some(s) => s.in_size != src_tex_size || s.out_size != dst_size,
        };
        if stale {
            match RgbaScaler::new(
                device,
                context,
                src_tex_size.0,
                src_tex_size.1,
                dst_size.0,
                dst_size.1,
                enable_vsr,
            ) {
                Ok(s) => self.vp = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "GPU scaler unavailable; cropping instead");
                    self.vp_disabled = true;
                    return false;
                }
            }
        }
        let s = self.vp.as_mut().expect("vp scaler present");
        match s.blit(src, dst, src_rect, dst_size.0, dst_size.1) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "GPU scale blit failed; cropping instead");
                self.vp_disabled = true;
                false
            }
        }
    }

    /// The intermediate upscale target for the sharpen chain (RENDER_TARGET so
    /// both the shader RTV and the VideoProcessor output view can write it).
    fn ensure_mid(&mut self, device: &ID3D11Device, size: (u32, u32)) -> Option<ID3D11Texture2D> {
        if let Some((tex, s)) = &self.mid {
            if *s == size {
                return Some(tex.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            if device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
                return None;
            }
            let tex = tex?;
            self.mid = Some((tex.clone(), size));
            Some(tex)
        }
    }
}

/// Present one target's slice of the framebuffer: a 1:1 copy when the slice
/// matches the destination, an upscale (+ optional sharpen) through `pipeline`
/// when it doesn't. Any pipeline failure falls back to the clipped 1:1 copy so
/// the picture is never lost. Free function (not a method) so per-monitor
/// targets can lend out their own pipelines mutably while the renderer's
/// shared device/context/framebuffer are read.
#[allow(clippy::too_many_arguments)]
unsafe fn present_target_slice(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    fb: &ID3D11Texture2D,
    fb_size: (u32, u32),
    swap_chain: &IDXGISwapChain,
    frame_wait: Option<HANDLE>,
    tearing: bool,
    low_latency: bool,
    dst_size: (u32, u32),
    src: (u32, u32),
    src_size: (u32, u32),
    pipeline: &mut UpscalePipeline,
    mode: Upscaler,
    sharpen: f32,
) -> WinResult<()> {
    if let Some(handle) = frame_wait {
        let _ = WaitForSingleObjectEx(handle, 100, false);
    }
    let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
    let painted = (src_size != dst_size || sharpen > 0.0)
        && pipeline.run(
            device,
            context,
            mode,
            sharpen,
            fb,
            fb_size,
            (src.0, src.1, src_size.0, src_size.1),
            &back_buffer,
            dst_size,
        );
    if !painted {
        // 1:1 (or fallback) copy, clipped to the framebuffer and backbuffer.
        let right = (src.0 + src_size.0.min(dst_size.0)).min(fb_size.0);
        let bottom = (src.1 + src_size.1.min(dst_size.1)).min(fb_size.1);
        if right > src.0 && bottom > src.1 {
            let src_box = D3D11_BOX {
                left: src.0,
                top: src.1,
                front: 0,
                right,
                bottom,
                back: 1,
            };
            context.CopySubresourceRegion(&back_buffer, 0, 0, 0, 0, fb, 0, Some(&src_box));
        }
    }
    let (sync, flags) = if low_latency && tearing {
        (0, DXGI_PRESENT_ALLOW_TEARING)
    } else {
        (1, DXGI_PRESENT(0))
    };
    swap_chain.Present(sync, flags).ok()
}

#[cfg(test)]
mod shader_tests {
    use super::*;

    /// The runtime-compiled HLSL must actually compile — this validates every
    /// kernel entry point via d3dcompiler (pure software; no GPU needed).
    #[test]
    fn scale_hlsl_compiles_for_every_kernel() {
        unsafe {
            compile_scale_shader(windows::core::s!("vs_main"), windows::core::s!("vs_5_0"))
                .expect("vertex shader");
            for kernel in [
                ScaleKernel::Bicubic,
                ScaleKernel::Easu,
                ScaleKernel::Nearest,
                ScaleKernel::Rcas,
            ] {
                compile_scale_shader(kernel.entry(), windows::core::s!("ps_5_0"))
                    .unwrap_or_else(|e| panic!("{}: {e}", kernel.label()));
            }
        }
    }
}
