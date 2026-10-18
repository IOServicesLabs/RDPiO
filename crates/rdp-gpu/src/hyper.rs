//! Hyper Accelerator host pipeline, all on the GPU:
//!
//! 1. [`DesktopCapture`] — DXGI Desktop Duplication of one monitor. A frame
//!    arrives the moment the compositor presents it, at the display's own
//!    refresh rate (120/144/240 Hz), as a GPU texture. A virtual monitor of
//!    the rdpio Display driver hands its frames over itself instead
//!    ([`driver_frames`]).
//! 2. The D3D11 video processor converts BGRA → NV12 (BT.709 studio range —
//!    what rdpio's decoder side assumes) straight from the duplicated surface,
//!    scales if the stream is smaller than the desktop, and turns a rotated
//!    display's picture upright.
//! 3. [`HwEncoder`] — the GPU vendor's hardware H.264/HEVC encoder (NVENC,
//!    Quick Sync or AMF) through its asynchronous Media Foundation transform,
//!    fed the NV12 texture directly (no system-memory copy), in low-latency CBR
//!    mode with no B-frames and an endless GOP (keyframes only on request).

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Error, Interface, Result as WinResult, GUID, HRESULT};
use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, E_NOTIMPL, E_UNEXPECTED, ERROR_TIMEOUT, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CAPS, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_ROTATION, D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270,
    D3D11_VIDEO_PROCESSOR_ROTATION_90, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

pub mod driver_frames;
pub mod nvenc;

/// A monitor the host can stream.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub adapter: String,
    pub device_name: String,
    pub left: i32,
    pub top: i32,
    /// The display mode's size as the desktop is oriented — what a
    /// [`DesktopCapture`] of it streams by default.
    pub width: u32,
    pub height: u32,
}

/// Monitors attached to the desktop, primary (the one at 0,0) first.
pub fn list_monitors() -> WinResult<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let ad = adapter.GetDesc1()?;
            let adapter_name = String::from_utf16_lossy(&ad.Description).trim_end_matches('\0').to_string();
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let d = output.GetDesc()?;
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = d.DesktopCoordinates;
                let (width, height) = mode_size(&d.DeviceName).unwrap_or_else(|| rect_size(&r));
                out.push(MonitorInfo {
                    adapter: adapter_name.clone(),
                    device_name: String::from_utf16_lossy(&d.DeviceName).trim_end_matches('\0').to_string(),
                    left: r.left,
                    top: r.top,
                    width,
                    height,
                });
            }
        }
    }
    out.sort_by_key(|m| (m.left != 0 || m.top != 0, m.left, m.top));
    Ok(out)
}

/// The display's current mode size, as the desktop is oriented. Unlike
/// `DesktopCoordinates` it's never DPI-virtualized (a process that isn't
/// DPI-aware sees a 125 % display's 2520×1680 as 2016×1344 there).
unsafe fn mode_size(device_name: &[u16; 32]) -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut mode = DEVMODEW { dmSize: core::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    EnumDisplaySettingsW(windows::core::PCWSTR(device_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
        .as_bool()
        .then_some((mode.dmPelsWidth, mode.dmPelsHeight))
        .filter(|&(w, h)| w > 0 && h > 0)
}

fn rect_size(r: &RECT) -> (u32, u32) {
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

/// Switch the calling thread to the desktop that currently receives input (the
/// normal desktop, or the secure one for the lock screen and UAC prompts).
/// Only SYSTEM may attach to the secure desktop; returns whether the thread is
/// on the input desktop.
///
/// Cheap enough to call for every input packet: a thread looks at the input
/// desktop at most every 100 ms, and only switches (keeping a new handle)
/// when it really changed; handles no thread is on any more get closed.
pub fn attach_input_desktop() -> bool {
    input_desktop::attach(false)
}

mod input_desktop {
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;

    /// How long a thread trusts its last look at the input desktop.
    const RECHECK: Duration = Duration::from_millis(100);

    thread_local! {
        /// When this thread last looked, and whether it was on the input desktop.
        static LAST: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
    }

    /// Desktop handles threads were switched to (raw: `HDESK` isn't `Send`).
    /// `CloseDesktop` refuses a handle some thread of ours is on, so each
    /// switch closes the ones that are free and keeps the rest listed.
    static OPENED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    /// [`super::attach_input_desktop`]; `force` skips the 100 ms grace (a
    /// duplication is only recreated after the desktop changed).
    pub(super) fn attach(force: bool) -> bool {
        if let Some((at, ok)) = LAST.get() {
            if !force && at.elapsed() < RECHECK {
                return ok;
            }
        }
        let ok = unsafe { switch() };
        LAST.set(Some((Instant::now(), ok)));
        ok
    }

    unsafe fn switch() -> bool {
        // GENERIC_ALL, as SetThreadDesktop wants.
        let Ok(input) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x1000_0000)) else {
            return false;
        };
        if GetThreadDesktop(GetCurrentThreadId()).is_ok_and(|current| same_desktop(current, input)) {
            // Already there, the usual case.
            let _ = CloseDesktop(input);
            return true;
        }
        let mut opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
        if SetThreadDesktop(input).is_err() {
            let _ = CloseDesktop(input);
            return false;
        }
        opened.retain(|&h| CloseDesktop(HDESK(h as *mut _)).is_err());
        opened.push(input.0 as usize);
        true
    }

    /// Whether two handles name the same desktop (all of ours live in the
    /// interactive window station, where names are unique).
    unsafe fn same_desktop(a: HDESK, b: HDESK) -> bool {
        let name = |h: HDESK| {
            let mut buf = [0u16; 64];
            let len = (buf.len() * 2) as u32;
            GetUserObjectInformationW(HANDLE(h.0), UOI_NAME, Some(buf.as_mut_ptr().cast()), len, None).ok().map(|()| buf)
        };
        matches!((name(a), name(b)), (Some(x), Some(y)) if x == y)
    }
}

/// A captured, converted frame ready for the encoder.
pub struct CapturedFrame {
    /// The picture for the encoder: NV12 at the stream size, or the raw BGRA
    /// desktop in RGB mode ([`DesktopCapture::rgb`]). Valid until the ring of
    /// textures wraps (see [`DesktopCapture::new`]'s `ring` argument).
    pub nv12: ID3D11Texture2D,
    /// QPC time of the present this frame shows, in µs.
    pub present_us: u64,
    /// Presents the compositor made since the previous frame (>1 = we were late).
    pub accumulated: u32,
}

/// A pointer change seen by Desktop Duplication (which leaves the pointer out
/// of the captured image, so the client can draw it locally — no latency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerEvent {
    Visible(bool),
    /// Top-down RGBA8 and hotspot.
    Shape { width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: Vec<u8> },
}

/// How long a capture waits for the driver's first frame before it
/// duplicates the output instead (the stream's start nudges a repaint, so a
/// working driver has one well within this).
const DRIVER_GRACE: Duration = Duration::from_secs(3);

/// Desktop Duplication of one monitor plus the BGRA → NV12 conversion.
pub struct DesktopCapture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    /// Desktop size (the display mode's pixels, as the desktop is oriented)
    /// and the stream (encode) size.
    pub desktop: (u32, u32),
    pub stream: (u32, u32),
    /// The duplicated surface: the display mode in its own, unrotated
    /// orientation (a portrait desktop arrives landscape).
    surface: (u32, u32),
    rotation: DXGI_MODE_ROTATION,
    /// The duplication's `ModeDesc` size when opened: a recreated duplication
    /// reporting another one means the mode changed.
    mode: (u32, u32),
    /// The output's `DesktopCoordinates` size and rotation when opened, which
    /// [`size_changed`](Self::size_changed) compares against.
    opened_as: ((u32, u32), DXGI_MODE_ROTATION),
    /// The duplication stopped matching this capture (a mode or rotation
    /// change): no more frames until the caller rebuilds.
    stale: bool,
    ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    /// The BGRA → NV12 conversion; `None` in RGB mode.
    vp: Option<Converter>,
    qpc_per_us: f64,
    pointer_visible: Option<bool>,
    pointer_events: Vec<PointerEvent>,
    /// RGB mode: frames go to the encoder as BGRA, unconverted (see `new_rgb`).
    pub rgb: bool,
    /// Frames from the rdpio Display driver instead of the duplication.
    driver: Option<driver_frames::DriverFrames>,
    /// When the driver's frames were opened, and whether one came yet: none
    /// within [`DRIVER_GRACE`] (or any failure reading them) and the output is
    /// duplicated instead, rather than streaming nothing.
    driver_since: Instant,
    driver_seen: bool,
    /// The capture device's adapter (as [`adapter_luid`](Self::adapter_luid)).
    luid: i64,
    /// A driver frame seen by [`peek`](Self::peek), not converted yet.
    peeked: Option<driver_frames::NextFrame>,
}

impl DesktopCapture {
    /// Capture the monitor named `device_name` (`\\.\DISPLAY1`, from
    /// [`list_monitors`]) and encode at `stream` size (`None` = desktop size).
    /// `ring` NV12 textures rotate so the encoder can still be reading one
    /// while the next is converted.
    pub fn new(device_name: &str, stream: Option<(u32, u32)>, ring: usize) -> WinResult<Self> {
        Self::open(device_name, stream, ring, false)
    }

    /// Like [`new`](Self::new) at desktop size, but hand the encoder the
    /// captured BGRA as is (for encoders that take RGB input and convert on
    /// their own engine). That keeps the colour conversion off the 3D engine,
    /// where a running game's rendering would otherwise delay it by several ms.
    /// Fails for a rotated display, whose picture needs `new`'s video
    /// processor to stand upright.
    pub fn new_rgb(device_name: &str, ring: usize) -> WinResult<Self> {
        Self::open(device_name, None, ring, true)
    }

    fn open(device_name: &str, stream: Option<(u32, u32)>, ring: usize, rgb: bool) -> WinResult<Self> {
        unsafe {
            let (adapter, output) = find_output(device_name)?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.expect("device");
            let context = context.expect("context");
            // The encoder MFT uses this device from its own threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            raise_gpu_priority(&device);
            let d = output.GetDesc()?;
            let opened_as = (rect_size(&d.DesktopCoordinates), d.Rotation);
            let dup = duplicate(&output, &device)?;
            let dd = dup.GetDesc();
            let rotation = dd.Rotation;
            // The desktop as it stands, in real pixels: the display mode
            // (DesktopCoordinates can be DPI-scaled for this process). The
            // duplicated surface is that picture unrotated: a portrait desktop
            // arrives landscape. ModeDesc only stands in for the mode, turned
            // like the desktop: which way round it reports a rotated mode isn't
            // documented (Chromium's capturer finds it as the desktop stands).
            let mode = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let desktop = mode_size(&d.DeviceName)
                .or_else(|| Some(shaped_like(mode, opened_as.0)).filter(|&(w, h)| w > 0 && h > 0))
                .unwrap_or(opened_as.0);
            let surface = oriented(desktop, rotation);
            if rgb && vp_rotation(rotation).is_some() {
                return Err(Error::new(E_NOTIMPL, "the display is rotated; its picture needs the video processor"));
            }
            if rgb && RGB_FRAMES_UNUSABLE.load(Ordering::Relaxed) {
                return Err(Error::new(E_NOTIMPL, "the duplicated desktop isn't 8-bit BGRA; it needs the video processor"));
            }
            // H.264 wants even dimensions.
            let stream = stream.unwrap_or(desktop);
            let stream = if rgb { surface } else { (stream.0 & !1, stream.1 & !1) };
            let ring_format = if rgb { DXGI_FORMAT_B8G8R8A8_UNORM } else { DXGI_FORMAT_NV12 };
            let ring: Vec<ID3D11Texture2D> = (0..ring.max(1))
                .map(|_| texture(&device, stream, ring_format, D3D11_BIND_RENDER_TARGET.0 as u32))
                .collect::<WinResult<_>>()?;
            let vp = if rgb { None } else { Some(Converter::new(&device, &context, surface, stream, rotation, &ring)?) };
            let luid = adapter.GetDesc1().map(|d| ((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64).unwrap_or(0);
            // Our virtual monitor shares its frames: no duplication needed
            // (it comes back if they turn out unusable).
            let driver = if vp_rotation(rotation).is_none() { driver_frames::for_output(device_name) } else { None };
            if driver.is_some() {
                tracing::info!(device = device_name, "frames come straight from the rdpio Display driver");
            }
            let dup = if driver.is_some() { None } else { Some(dup) };

            let mut freq = 0i64;
            let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq);

            Ok(Self {
                device,
                context,
                output,
                dup,
                desktop,
                stream,
                surface,
                rotation,
                mode,
                opened_as,
                stale: false,
                ring,
                ring_next: 0,
                vp,
                qpc_per_us: freq.max(1) as f64 / 1_000_000.0,
                pointer_visible: None,
                pointer_events: Vec::new(),
                rgb,
                driver,
                driver_since: Instant::now(),
                driver_seen: false,
                luid,
                peeked: None,
            })
        }
    }

    /// Whether a new duplication still hands out the surface this capture
    /// was built for: the same mode, turned the same way.
    unsafe fn fits(&self, dup: &IDXGIOutputDuplication) -> bool {
        let dd = dup.GetDesc();
        dd.Rotation == self.rotation && (dd.ModeDesc.Width == 0 || (dd.ModeDesc.Width, dd.ModeDesc.Height) == self.mode)
    }

    fn lose_duplication(&mut self) {
        self.dup = None;
        // Those views hold the old duplication's surfaces.
        if let Some(vp) = &mut self.vp {
            vp.views.clear();
        }
    }

    /// Let go of the monitor's duplication now. A process may duplicate an
    /// output only once, so a replacement capture can't open while this one
    /// still holds it: call this first. This capture yields nothing after.
    pub fn release(&mut self) {
        self.lose_duplication();
        self.driver = None;
        self.stale = true;
    }

    /// Whether frames come from the rdpio Display driver.
    pub fn from_driver(&self) -> bool {
        self.driver.is_some()
    }

    /// Whether a frame can be looked at before it's converted ([`peek`](Self::peek)
    /// then [`take`](Self::take)): the driver's frames, whose newest stays put
    /// until a newer one comes. Desktop Duplication hands its frame back at
    /// once, so [`acquire`](Self::acquire) converts it there and then.
    pub fn defers(&self) -> bool {
        self.driver.is_some() && !self.stale
    }

    /// Driver frames: wait up to `timeout_ms` for a newer frame and say when
    /// it was presented (µs), without converting it — a frame the stream's
    /// rate skips (a 240 Hz monitor streaming 60 fps) costs nothing. `Ok(None)`:
    /// nothing new (or the driver's frames stopped: [`defers`](Self::defers)
    /// then says so, and [`acquire`](Self::acquire) duplicates the output).
    pub fn peek(&mut self, timeout_ms: u32) -> WinResult<Option<u64>> {
        unsafe {
            let Some(next) = self.driver_peek(timeout_ms)? else { return Ok(None) };
            let present_us = (next.state.present_qpc as f64 / self.qpc_per_us) as u64;
            self.peeked = Some(next);
            Ok(Some(present_us))
        }
    }

    /// The newest driver frame, converted: the one [`peek`](Self::peek) saw,
    /// or a newer one that came since. `Ok(None)` if there's none.
    pub fn take(&mut self) -> WinResult<Option<CapturedFrame>> {
        unsafe {
            let next = match self.driver_peek(0)? {
                Some(newer) => Some(newer),
                None => self.peeked.take(),
            };
            self.peeked = None;
            match next {
                Some(next) => self.driver_convert(next),
                None => Ok(None),
            }
        }
    }

    /// Wait up to `timeout_ms` for the next desktop frame. `Ok(None)` on
    /// timeout, a pointer-only update, or a transient loss (mode change, the
    /// secure desktop): the duplication is recreated and the caller retries.
    /// Once the display no longer fits this capture, it's `Ok(None)` for good
    /// and [`size_changed`](Self::size_changed) says to rebuild.
    pub fn acquire(&mut self, timeout_ms: u32) -> WinResult<Option<CapturedFrame>> {
        unsafe {
            if self.stale {
                // Don't spin the caller while it gets around to rebuilding.
                std::thread::sleep(Duration::from_millis(timeout_ms.min(20) as u64));
                return Ok(None);
            }
            if self.driver.is_some() {
                return self.acquire_from_driver(timeout_ms);
            }
            if self.dup.is_none() {
                match duplicate(&self.output, &self.device) {
                    Ok(dup) if self.fits(&dup) => self.dup = Some(dup),
                    Ok(_) => {
                        tracing::info!("display mode changed; the capture needs rebuilding");
                        self.stale = true;
                        return Ok(None);
                    }
                    Err(e) => {
                        std::thread::sleep(Duration::from_millis(50));
                        return if e.code() == E_ACCESSDENIED { Ok(None) } else { Err(e) };
                    }
                }
            }
            let dup = self.dup.clone().expect("duplication");
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    tracing::info!("desktop duplication lost (mode change or secure desktop); recreating");
                    self.lose_duplication();
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
            self.note_pointer(&dup, &info);
            let slot = self.take_frame(resource, &info);
            // Hand the surface back to the compositor as early as possible; the
            // copy or conversion reading it is queued on the GPU ahead of this.
            let _ = dup.ReleaseFrame();
            let Some(i) = slot? else {
                return Ok(None);
            };
            self.ring_next = (i + 1) % self.ring.len();
            Ok(Some(CapturedFrame {
                nv12: self.ring[i].clone(),
                present_us: (info.LastPresentTime as f64 / self.qpc_per_us) as u64,
                accumulated: info.AccumulatedFrames,
            }))
        }
    }

    /// Put the acquired surface into the next ring slot — copied (RGB mode)
    /// or converted — before the frame is released. `None` for a pointer-only
    /// update, or a surface that stopped matching the capture.
    unsafe fn take_frame(&mut self, resource: Option<IDXGIResource>, info: &DXGI_OUTDUPL_FRAME_INFO) -> WinResult<Option<usize>> {
        let Some(resource) = resource.filter(|_| info.LastPresentTime != 0) else {
            return Ok(None); // only the pointer moved
        };
        let frame: ID3D11Texture2D = resource.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.GetDesc(&mut desc);
        // D3D11 silently drops a copy between textures that don't match.
        let unusable_rgb = self.rgb && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM;
        if unusable_rgb {
            RGB_FRAMES_UNUSABLE.store(true, Ordering::Relaxed);
        }
        if (desc.Width, desc.Height) != self.surface || unusable_rgb {
            tracing::info!(width = desc.Width, height = desc.Height, format = desc.Format.0, expected = ?self.surface, "the duplicated surface changed; the capture needs rebuilding");
            self.stale = true;
            return Ok(None);
        }
        let i = self.ring_next;
        match &mut self.vp {
            Some(vp) => vp.convert(&self.device, &self.context, &frame, i, self.surface, self.stream)?,
            None => self.context.CopyResource(&self.ring[i], &frame),
        }
        Ok(Some(i))
    }

    /// [`acquire`](Self::acquire) from the driver's shared textures.
    unsafe fn acquire_from_driver(&mut self, timeout_ms: u32) -> WinResult<Option<CapturedFrame>> {
        let next = match self.driver_peek(timeout_ms)? {
            Some(next) => Some(next),
            None => self.peeked.take(),
        };
        match next {
            Some(next) => self.driver_convert(next),
            None => Ok(None),
        }
    }

    /// The driver's next frame (waiting up to `timeout_ms`), checked to be
    /// usable here, not converted.
    unsafe fn driver_peek(&mut self, timeout_ms: u32) -> WinResult<Option<driver_frames::NextFrame>> {
        let mut next = self.driver_next();
        if matches!(next, Ok(None)) {
            if let Some(d) = &self.driver {
                d.wait(timeout_ms);
            }
            next = self.driver_next();
        }
        let next = match next {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "the driver's frames can't be read; duplicating the output instead");
                self.driver = None;
                return Ok(None);
            }
        };
        self.note_driver_pointer();
        let Some(next) = next else {
            if !self.driver_seen && self.driver_since.elapsed() > DRIVER_GRACE {
                tracing::warn!("no frame from the rdpio Display driver yet; duplicating the output instead");
                self.driver = None;
            }
            return Ok(None);
        };
        self.driver_seen = true;
        let s = next.state;
        if s.adapter != self.luid {
            tracing::warn!(driver = s.adapter, capture = self.luid, "the driver's frames are on another GPU; duplicating the output instead");
            self.driver = None;
            return Ok(None);
        }
        if (s.width, s.height) != self.surface || s.format != DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32 {
            tracing::info!(width = s.width, height = s.height, format = s.format, expected = ?self.surface, "the driver's frames changed; the capture needs rebuilding");
            self.stale = true;
            return Ok(None);
        }
        if next.reopened {
            if let Some(vp) = &mut self.vp {
                vp.views.clear();
            }
        }
        Ok(Some(next))
    }

    /// Convert driver frame `next` into the ring.
    unsafe fn driver_convert(&mut self, next: driver_frames::NextFrame) -> WinResult<Option<CapturedFrame>> {
        let s = next.state;
        // The driver writes other slots meanwhile; this one is ours until
        // the GPU has read it.
        if !driver_frames::lock(&next.mutex, 100) {
            return Ok(None);
        }
        let i = self.ring_next;
        let r = match &mut self.vp {
            Some(vp) => vp.convert(&self.device, &self.context, &next.texture, i, self.surface, self.stream),
            None => {
                self.context.CopyResource(&self.ring[i], &next.texture);
                Ok(())
            }
        };
        let _ = next.mutex.ReleaseSync(0);
        r?;
        self.ring_next = (i + 1) % self.ring.len();
        Ok(Some(CapturedFrame {
            nv12: self.ring[i].clone(),
            present_us: (s.present_qpc as f64 / self.qpc_per_us) as u64,
            accumulated: next.published,
        }))
    }

    fn driver_next(&mut self) -> WinResult<Option<driver_frames::NextFrame>> {
        match &mut self.driver {
            Some(d) => d.next_frame(&self.device),
            None => Ok(None),
        }
    }

    fn note_driver_pointer(&mut self) {
        let Some(d) = &mut self.driver else {
            return;
        };
        for change in d.cursor_changes() {
            match change {
                driver_frames::CursorChange::Visible(v) => {
                    if self.pointer_visible != Some(v) {
                        self.pointer_visible = Some(v);
                        self.pointer_events.push(PointerEvent::Visible(v));
                    }
                }
                driver_frames::CursorChange::Shape(info, buf) => {
                    if let Some(ev) = pointer_shape(&info, &buf) {
                        self.pointer_events.push(ev);
                    }
                }
            }
        }
    }

    unsafe fn note_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            let visible = info.PointerPosition.Visible.as_bool();
            if self.pointer_visible != Some(visible) {
                self.pointer_visible = Some(visible);
                self.pointer_events.push(PointerEvent::Visible(visible));
            }
        }
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut needed = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if dup
                .GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as *mut c_void, &mut needed, &mut shape)
                .is_ok()
            {
                if let Some(ev) = pointer_shape(&shape, &buf) {
                    self.pointer_events.push(ev);
                }
            }
        }
    }

    /// Whether the display no longer matches this capture (a mode, rotation
    /// or scaling change) — the caller must build a new capture and encoder.
    /// True from the moment [`acquire`](Self::acquire) meets a duplicated
    /// surface that doesn't fit, so it's worth asking after each `Ok(None)`.
    pub fn size_changed(&self) -> bool {
        self.stale
            || unsafe {
                self.output
                    .GetDesc()
                    .is_ok_and(|d| (rect_size(&d.DesktopCoordinates), d.Rotation) != self.opened_as)
            }
    }

    /// Pointer changes since the last call.
    pub fn take_pointer_events(&mut self) -> Vec<PointerEvent> {
        std::mem::take(&mut self.pointer_events)
    }

    /// The capture device's DXGI adapter LUID (to pick the encoder on the same GPU).
    pub fn adapter_luid(&self) -> WinResult<i64> {
        unsafe {
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = self.device.cast()?;
            let adapter: IDXGIAdapter = dxgi.GetAdapter()?;
            let d = adapter.GetDesc()?;
            Ok(((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64)
        }
    }
}

/// An RGB capture met a duplicated surface that isn't 8-bit BGRA (which both
/// ways of duplicating promise it is): from then on [`DesktopCapture::new_rgb`]
/// refuses, so the rebuild that follows converts instead of meeting it again.
/// Not judged from `ModeDesc.Format`: that's the display mode's format, which
/// on an HDR desktop needn't be what `DuplicateOutput1` hands out.
static RGB_FRAMES_UNUSABLE: AtomicBool = AtomicBool::new(false);

/// Start duplicating `output` for `device`, from whichever desktop has input
/// (the secure one for the lock screen and UAC prompts — needs SYSTEM).
unsafe fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> WinResult<IDXGIOutputDuplication> {
    input_desktop::attach(true);
    // DuplicateOutput1 with the 8-bit format we handle: on an HDR desktop the
    // plain DuplicateOutput refuses (the desktop is FP16); this way Windows
    // hands us SDR. Older systems lack IDXGIOutput5.
    match output.cast::<windows::Win32::Graphics::Dxgi::IDXGIOutput5>() {
        Ok(o5) => o5
            .DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
            .or_else(|_| output.DuplicateOutput(device)),
        Err(_) => output.DuplicateOutput(device),
    }
}

/// `size` with width and height swapped for a display turned a quarter: a
/// duplicated surface's size gives the desktop's, and the other way round.
fn oriented((w, h): (u32, u32), rotation: DXGI_MODE_ROTATION) -> (u32, u32) {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE270 {
        (h, w)
    } else {
        (w, h)
    }
}

/// `size`, turned if need be to stand the way `like` does (portrait or
/// landscape); a square or empty `like` says nothing.
fn shaped_like((w, h): (u32, u32), like: (u32, u32)) -> (u32, u32) {
    if like.0 == like.1 || (w > h) == (like.0 > like.1) {
        (w, h)
    } else {
        (h, w)
    }
}

/// The turn that stands a rotated display's duplicated surface upright, or
/// `None` when it already is. `ROTATE90` → 90° clockwise: how Microsoft's
/// Desktop Duplication sample and OBS map it too.
fn vp_rotation(rotation: DXGI_MODE_ROTATION) -> Option<D3D11_VIDEO_PROCESSOR_ROTATION> {
    match rotation {
        DXGI_MODE_ROTATION_ROTATE90 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
        DXGI_MODE_ROTATION_ROTATE180 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
        DXGI_MODE_ROTATION_ROTATE270 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
        _ => None,
    }
}

/// The video processor turning the duplicated BGRA surface into the NV12
/// ring: colour conversion, scaling, and rotation for a rotated display.
struct Converter {
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    /// Input views of the duplication's own surfaces (it cycles through a
    /// few), so a frame converts straight from the surface, without a copy.
    views: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    /// For a driver that can't read the duplicated surface directly: a
    /// texture it's copied into first.
    copy: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl Converter {
    unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        surface: (u32, u32),
        stream: (u32, u32),
        rotation: DXGI_MODE_ROTATION,
        ring: &[ID3D11Texture2D],
    ) -> WinResult<Self> {
        let vdevice: ID3D11VideoDevice = device.cast()?;
        let vcontext: ID3D11VideoContext = context.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: surface.0,
            InputHeight: surface.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: stream.0,
            OutputHeight: stream.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = vdevice.CreateVideoProcessorEnumerator(&desc)?;
        let processor = vdevice.CreateVideoProcessor(&enumerator, 0)?;
        let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_views = Vec::new();
        for t in ring {
            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
            vdevice.CreateVideoProcessorOutputView(t, &enumerator, &ov_desc, Some(&mut ov))?;
            output_views.push(ov.expect("output view"));
        }
        // Full-range RGB in; BT.709 studio-range YCbCr out (bit2 = BT.709
        // matrix, bits 4-5 = nominal range 16-235) — the decode side's
        // assumption, so colours round-trip exactly.
        vcontext.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
        vcontext.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 });
        vcontext.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        // Turn off any driver "enhancements" — they cost time and alter pixels.
        vcontext.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        if let Some(turn) = vp_rotation(rotation) {
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            enumerator.GetVideoProcessorCaps(&mut caps)?;
            if caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 == 0 {
                return Err(Error::new(E_NOTIMPL, "the display is rotated and this GPU's video processor can't turn it upright"));
            }
            vcontext.VideoProcessorSetStreamRotation(&processor, 0, true, turn);
        }
        Ok(Self { vdevice, vcontext, enumerator, processor, output_views, views: Vec::new(), copy: None })
    }

    /// Convert `frame` (the acquired duplication surface) into ring slot `i`.
    unsafe fn convert(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &ID3D11Texture2D,
        i: usize,
        surface: (u32, u32),
        stream: (u32, u32),
    ) -> WinResult<()> {
        let view = self.input_view(device, context, frame, surface)?;
        self.blit(&view, i, surface, stream)
    }

    /// The video processor's view of `frame`: a cached view of the duplicated
    /// surface itself, else of a copy of it made now.
    unsafe fn input_view(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &ID3D11Texture2D,
        surface: (u32, u32),
    ) -> WinResult<ID3D11VideoProcessorInputView> {
        if self.copy.is_none() {
            if let Some((_, view)) = self.views.iter().find(|(t, _)| t.as_raw() == frame.as_raw()) {
                return Ok(view.clone());
            }
            match self.view_of(frame) {
                Ok(view) => {
                    // A few at most; more means the old surfaces were replaced.
                    if self.views.len() >= 8 {
                        self.views.remove(0);
                    }
                    self.views.push((frame.clone(), view.clone()));
                    return Ok(view);
                }
                Err(e) => {
                    tracing::info!(error = %e, "the video processor can't read the duplicated surface; converting from a copy");
                    let bind = D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
                    let t = texture(device, surface, DXGI_FORMAT_B8G8R8A8_UNORM, bind)?;
                    let view = self.view_of(&t)?;
                    self.copy = Some((t, view));
                    self.views.clear();
                }
            }
        }
        let (t, view) = self.copy.as_ref().expect("copy texture");
        context.CopyResource(t, frame);
        Ok(view.clone())
    }

    unsafe fn view_of(&self, texture: &ID3D11Texture2D) -> WinResult<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        self.vdevice.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))?;
        view.ok_or_else(|| Error::new(E_FAIL, "no video processor input view"))
    }

    /// Run the video processor from `view` into ring slot `i`.
    unsafe fn blit(&self, view: &ID3D11VideoProcessorInputView, i: usize, surface: (u32, u32), stream: (u32, u32)) -> WinResult<()> {
        // The source rectangle is in the surface's own (unrotated) pixels, the
        // destination in the upright output's.
        let src = RECT { left: 0, top: 0, right: surface.0 as i32, bottom: surface.1 as i32 };
        let dst = RECT { left: 0, top: 0, right: stream.0 as i32, bottom: stream.1 as i32 };
        self.vcontext.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
        self.vcontext.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
        self.vcontext.VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&dst));
        let input = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: core::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(view.clone())),
            ppFutureSurfaces: core::ptr::null_mut(),
            ppPastSurfacesRight: core::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: core::ptr::null_mut(),
        };
        let r = self.vcontext.VideoProcessorBlt(&self.processor, &self.output_views[i], 0, core::slice::from_ref(&input));
        let _ = ManuallyDrop::into_inner(input.pInputSurface);
        r
    }
}

/// Convert a duplication pointer shape to RGBA. Inverting pixels (the text
/// I-beam's XOR parts) can't be expressed in RGBA; they become opaque black,
/// which reads the same on light backgrounds.
fn pointer_shape(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<PointerEvent> {
    let (w, pitch) = (info.Width as usize, info.Pitch as usize);
    let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    let h = if mono { info.Height as usize / 2 } else { info.Height as usize };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = &mut rgba[(y * w + x) * 4..][..4];
            if mono {
                let bit = |row: usize| buf.get(row * pitch + x / 8).map(|v| (v >> (7 - x % 8)) & 1);
                let (and, xor) = (bit(y)?, bit(y + h)?);
                px.copy_from_slice(match (and, xor) {
                    (1, 0) => &[0, 0, 0, 0],
                    (0, 1) => &[255, 255, 255, 255],
                    _ => &[0, 0, 0, 255],
                });
            } else {
                let s = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?;
                let (b, g, r, a) = (s[0], s[1], s[2], s[3]);
                let masked = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
                px.copy_from_slice(&if masked {
                    // Alpha 0: replace with the colour; 0xFF: XOR with the screen.
                    match (a, r as u32 + g as u32 + b as u32) {
                        (0, _) => [r, g, b, 255],
                        (_, 0) => [0, 0, 0, 0],
                        _ => [0, 0, 0, 255],
                    }
                } else {
                    [r, g, b, a]
                });
            }
        }
    }
    Some(PointerEvent::Shape {
        width: w as u16,
        height: h as u16,
        hot_x: info.HotSpot.x.max(0) as u16,
        hot_y: info.HotSpot.y.max(0) as u16,
        rgba,
    })
}

/// Put our capture/convert/encode work ahead of a running game's rendering in
/// the GPU scheduler. Measured: with a game holding the GPU at ~98%, our tiny
/// per-frame copy/convert waited ~8 ms behind its frames. REALTIME process
/// class plus thread priority 7 (what Sunshine does) needs
/// SeIncreaseBasePriorityPrivilege — administrators and SYSTEM (the Hyper
/// service's agent) hold it; otherwise we settle for HIGH.
unsafe fn raise_gpu_priority(device: &ID3D11Device) {
    let privileged = enable_privilege("SeIncreaseBasePriorityPrivilege");
    let thread = device
        .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()
        .and_then(|d| d.SetGPUThreadPriority(7))
        .is_ok();
    // D3DKMTSetProcessSchedulingPriorityClass lives in gdi32 without a public
    // binding. 5 = REALTIME, 4 = HIGH.
    type SetClass = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, i32) -> i32;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let mut class_status = None;
    if let Ok(gdi) = LoadLibraryW(windows::core::w!("gdi32.dll")) {
        if let Some(f) = GetProcAddress(gdi, windows::core::s!("D3DKMTSetProcessSchedulingPriorityClass")) {
            let set: SetClass = core::mem::transmute(f);
            let process = windows::Win32::System::Threading::GetCurrentProcess();
            let mut st = if privileged { set(process, 5) } else { -1 };
            let class = if st == 0 { "realtime" } else {
                st = set(process, 4);
                "high"
            };
            class_status = Some((class, st));
        }
    }
    tracing::info!(privileged, gpu_thread_priority_7 = thread, process_class = ?class_status, "GPU scheduling priority for capture/encode");
}

/// Enable a privilege the process token holds but has disabled.
unsafe fn enable_privilege(name: &str) -> bool {
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = windows::Win32::Foundation::HANDLE::default();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
        return false;
    }
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut luid = windows::Win32::Foundation::LUID::default();
    let ok = LookupPrivilegeValueW(windows::core::PCWSTR::null(), windows::core::PCWSTR(wide.as_ptr()), &mut luid).is_ok() && {
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        // Succeeds even when not held; ERROR_NOT_ALL_ASSIGNED (1300) says so.
        AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_ok()
            && windows::Win32::Foundation::GetLastError().0 != 1300
    };
    let _ = windows::Win32::Foundation::CloseHandle(token);
    ok
}

unsafe fn find_output(device_name: &str) -> WinResult<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut a = 0;
    while let Ok(adapter) = factory.EnumAdapters1(a) {
        a += 1;
        let mut o = 0;
        while let Ok(output) = adapter.EnumOutputs(o) {
            o += 1;
            let d = output.GetDesc()?;
            let name = String::from_utf16_lossy(&d.DeviceName);
            if name.trim_end_matches('\0').eq_ignore_ascii_case(device_name) {
                return Ok((adapter, output.cast()?));
            }
        }
    }
    Err(windows::core::Error::new(DXGI_ERROR_NOT_FOUND, format!("no monitor named {device_name}")))
}

unsafe fn texture(device: &ID3D11Device, (w, h): (u32, u32), format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: u32) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&desc, None, Some(&mut t))?;
    Ok(t.expect("texture"))
}

/// One encoded access unit (Annex-B).
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Which picture this is: the 0-based count of [`HwEncoder::encode`]
    /// calls when it went in, so an access unit that comes back late still
    /// pairs with its own capture time.
    pub input: u64,
}

/// A vendor hardware H.264/HEVC encoder (async Media Foundation transform) fed
/// D3D11 NV12 (or BGRA) textures.
pub struct MfEncoder {
    /// First, so the transform is shut down before the rest is released.
    mft: Mft,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    /// The transform's events (see [`spawn_event_pump`]).
    events: mpsc::Receiver<WinResult<(u32, HRESULT)>>,
    /// `METransformNeedInput` credits not yet used.
    need_input: u32,
    /// Pictures put in so far: the next one's [`Encoded::input`].
    frames_in: u64,
    /// The picture the next access unit belongs to, unless its time stamp says.
    next_out: u64,
    /// Access units not handed out yet, oldest first.
    ready: VecDeque<Encoded>,
    /// The encoder keeps pictures until it gets more input (see `encode`).
    holds_frames: bool,
    /// Access units taken after a stream change without an announcement of
    /// their own — which may still come, and find nothing.
    unannounced: u32,
    /// What [`renew`](Self::renew) builds the next transform from.
    device: ID3D11Device,
    adapter_luid: Option<i64>,
    size: (u32, u32),
    rgb: bool,
    /// The bitrate the HRD buffer was sized for.
    hrd_bps: u32,
    /// The encoder took an HRD buffer size when made. One that didn't would
    /// refuse it after a rebuild just the same, so there's none to follow.
    hrd_sized: bool,
    /// No new rebuild attempt before this (one just failed).
    renew_after: Option<Instant>,
    /// When this encoder was made (a rebuild is one).
    renewed_at: Instant,
    frame_duration: i64,
    fps: u32,
    force_key: bool,
    /// SPS/PPS from the output type, prepended to keyframes that lack them.
    seq_header: Vec<u8>,
    video_codec: crate::h264::Codec,
    tuning: Tuning,
    pub name: String,
    pub bitrate: u32,
    /// Keeps [`LTR_SLOTS`] long-term reference frames on request
    /// ([`mark_ltr`](Self::mark_ltr), [`use_ltr`](Self::use_ltr)).
    pub ltr: bool,
    /// Unique to this transform (a rebuild for a new bitrate is a new one,
    /// with none of the old one's references).
    pub id: u64,
}

/// Long-term reference frames an encoder keeps for repairing losses.
pub const LTR_SLOTS: u32 = 2;

/// [`LTR_SLOTS`], or none with `RDPIO_HYPER_LTR=0` (the host's repair
/// logic uses exactly two).
fn ltr_slots() -> u32 {
    if std::env::var("RDPIO_HYPER_LTR").is_ok_and(|v| v.trim() == "0") {
        0
    } else {
        LTR_SLOTS
    }
}
static NEXT_ENCODER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

/// A `VT_UI4` variant's value.
fn variant_value_u32(v: &VARIANT) -> Option<u32> {
    unsafe { ((*v.Anonymous.Anonymous).vt == VT_UI4).then(|| (*v.Anonymous.Anonymous).Anonymous.ulVal) }
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal =
            windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    }
    var
}

/// `eAVEncCommonRateControlMode_CBR`.
const RATE_CONTROL_CBR: u32 = 0;
/// `eAVEncH264VProfile_High`.
const H264_PROFILE_HIGH: u32 = 100;
/// `eAVEncH265VProfile_Main_420_8`.
const HEVC_PROFILE_MAIN: u32 = 1;

/// Longest wait on the encoder: far beyond any real encode (a 4K keyframe on
/// a busy GPU takes ~20 ms), short enough that a lost or wedged encoder (a
/// driver reset, a removed device) becomes an error the host acts on rather
/// than a frozen stream.
const ENCODER_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long a picture may take before an encoder that already asks for more
/// input is taken to hold pictures back (see [`HwEncoder::encode`]).
const HOLD_GRACE: Duration = Duration::from_millis(100);
/// Longest a dropped encoder may take to shut down before it's left behind.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The HRD (VBV) buffer for `bitrate` at `fps`: two frames' worth, in bytes —
/// the unit Microsoft documents for H.264, and how NVIDIA's HEVC encoder reads
/// it too (measured: frame sizes top out at it). No frame bursts much past its
/// share, which keeps per-frame network time flat; at exactly one frame NVENC
/// drops the limit altogether.
fn hrd_bytes(bitrate: u32, fps: u32, frames: u32) -> u32 {
    (bitrate as u64 * frames.max(1) as u64 / 8 / fps.max(1) as u64).min(u32::MAX as u64) as u32
}

/// Whether a bitrate of `now` (bps) is worth rebuilding an encoder whose HRD
/// buffer was sized for `sized`, `since` the last rebuild. A rebuild stalls
/// the stream ~30 ms and starts with a keyframe (a burst, and a visible
/// quality pulse) — live sessions did one every few seconds as the rate
/// moved. The mean bitrate changes live without one; only the buffer (the
/// largest frame) needs it. So: a big move (2x up, 2.5x down), at most every
/// 5 s — at once only when the rate collapsed (4x down: a frame of the old
/// size would now flood the path).
fn needs_rebuild(now: u32, sized: u32, since: Duration) -> bool {
    let (now, sized) = (now as u64, sized.max(1) as u64);
    if now * 4 < sized {
        return true;
    }
    since >= Duration::from_secs(5) && (now > sized * 2 || now * 5 < sized * 2)
}

/// How the encoder trades latency for picture quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// The HRD buffer in frames' worth of bitrate: how big one frame may get
    /// (a keyframe, or a whole screen of new text).
    pub buffer_frames: u32,
    /// `CODECAPI_AVEncCommonQualityVsSpeed`: 0 fastest .. 100 best.
    pub quality: u32,
}

impl Tuning {
    /// Every frame small and fast: the lowest latency.
    pub const GAMING: Self = Self { buffer_frames: 2, quality: 0 };

    /// Crisp text at `fps`: a frame may take a quarter second's bits (a whole
    /// screen of new text arrives sharp, not blurred and sharpening over a
    /// second), with the best-quality preset. Measured on 1080p office text:
    /// the first frame's luma 18 dB (game) → 38 (H.264) / 44 dB (HEVC).
    pub fn office(fps: u32) -> Self {
        Self { buffer_frames: (fps / 4).max(2), quality: 100 }
    }
}

impl MfEncoder {
    /// A hardware encoder on `device` (prefer the one on `adapter_luid`).
    /// `rgb`: feed BGRA textures (the encoder converts) instead of NV12.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ID3D11Device,
        adapter_luid: Option<i64>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        rgb: bool,
        video_codec: crate::h264::Codec,
    ) -> WinResult<Self> {
        Self::new_tuned(device, adapter_luid, width, height, fps, bitrate, rgb, video_codec, Tuning::GAMING)
    }

    /// [`new`](Self::new) with a [`Tuning`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_tuned(
        device: &ID3D11Device,
        adapter_luid: Option<i64>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        rgb: bool,
        video_codec: crate::h264::Codec,
        tuning: Tuning,
    ) -> WinResult<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
            let activate = pick_hardware_encoder(adapter_luid, video_codec)?;
            let mut pname = windows::core::PWSTR::null();
            let mut plen = 0u32;
            let name = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pname, &mut plen).is_ok() {
                let s = pname.to_string().unwrap_or_default();
                CoTaskMemFree(Some(pname.0 as *const c_void));
                s
            } else {
                "hardware H.264 encoder".into()
            };
            let transform: IMFTransform = match activate.ActivateObject() {
                Ok(t) => t,
                Err(e) => {
                    let _ = activate.ShutdownObject();
                    return Err(e);
                }
            };
            // From here on, every way out (an error below, or dropping the
            // encoder) shuts the transform down.
            let mut mft = Mft { transform: transform.clone(), activate, streaming: false, closed: false };
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.expect("device manager");
            manager.ResetDevice(device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let codec = transform.cast::<ICodecAPI>().ok();
            let mut hrd_sized = false;
            let mut ltr = false;
            if let Some(c) = &codec {
                let set = |key: &GUID, v: VARIANT, what: &str| match c.SetValue(key, &v) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::debug!(%what, error = %e, "encoder setting not supported");
                        false
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, variant_bool(true), "low latency");
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(RATE_CONTROL_CBR), "CBR");
                set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate), "bitrate");
                hrd_sized = set(&CODECAPI_AVEncCommonBufferSize, variant_u32(hrd_bytes(bitrate, fps, tuning.buffer_frames)), "HRD buffer");
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "no B-frames");
                set(&CODECAPI_AVEncMPVGOPSize, variant_u32(fps.max(1) * 3600), "long GOP");
                // 0 = fastest. Measured at 1440p on NVENC: ~1.5 ms lower median
                // capture→decoded latency than 20, for both H.264 and HEVC.
                // RDPIO_HYPER_QVS overrides it for tuning.
                let qvs = std::env::var("RDPIO_HYPER_QVS").ok().and_then(|v| v.parse().ok()).unwrap_or(tuning.quality);
                set(&CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(qvs), "speed");
                if video_codec == crate::h264::Codec::H264 {
                    set(&CODECAPI_AVEncH264CABACEnable, variant_bool(true), "CABAC");
                }
                // Two long-term reference frames the stream controls (trust
                // mode 1), to repair losses from (`RDPIO_HYPER_LTR=0`: none).
                let slots = ltr_slots();
                ltr = slots > 0 && set(&CODECAPI_AVEncVideoLTRBufferControl, variant_u32((1 << 16) | slots), "LTR buffers");
            }

            let frame_size = ((width as u64) << 32) | height as u64;
            let frame_rate = ((fps as u64) << 32) | 1;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &video_codec.subtype())?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, if video_codec == crate::h264::Codec::H264 { H264_PROFILE_HIGH } else { HEVC_PROFILE_MAIN })?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetOutputType(0, &out, 0)?;

            // Which inputs this encoder takes (an RGB input would let it convert
            // on its own engine instead of queueing behind a game's 3D work).
            let mut subtypes = Vec::new();
            for i in 0..32 {
                let Ok(t) = transform.GetInputAvailableType(0, i) else { break };
                if let Ok(g) = t.GetGUID(&MF_MT_SUBTYPE) {
                    let fourcc = g.data1.to_le_bytes();
                    subtypes.push(if fourcc.iter().all(|b| b.is_ascii_graphic()) {
                        String::from_utf8_lossy(&fourcc).into_owned()
                    } else {
                        format!("{:08x}", g.data1)
                    });
                }
            }
            tracing::info!(?subtypes, "encoder input formats");
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, if rgb { &MFVideoFormat_ARGB32 } else { &MFVideoFormat_NV12 })?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;

            let seq_header = transform
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER))
                .unwrap_or_default();

            let events: IMFMediaEventGenerator = transform.cast()?;
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.streaming = true;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = spawn_event_pump(events)?;
            tracing::info!(encoder = %name, width, height, fps, bitrate, rgb_input = rgb, codec = video_codec.name(), ltr, "hardware encoder ready");
            Ok(Self {
                mft,
                codec,
                _manager: manager,
                events,
                need_input: 0,
                frames_in: 0,
                next_out: 0,
                ready: VecDeque::new(),
                holds_frames: false,
                unannounced: 0,
                device: device.clone(),
                adapter_luid,
                size: (width, height),
                rgb,
                hrd_bps: bitrate,
                hrd_sized,
                renew_after: None,
                renewed_at: Instant::now(),
                frame_duration: 10_000_000 / fps.max(1) as i64,
                fps: fps.max(1),
                force_key: true,
                seq_header,
                video_codec,
                tuning,
                name,
                bitrate,
                ltr,
                id: NEXT_ENCODER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            })
        }
    }

    /// Change the target bitrate on the fly. The HRD buffer follows, so it
    /// keeps a frame to a couple of frames' worth at the new rate — where the
    /// encoder won't resize it while streaming (NVENC accepts the call and
    /// keeps the old size), a move past 1.5× up or 2× down rebuilds the
    /// encoder (~30 ms, and the next frame is a keyframe): up, the old buffer
    /// would cap the bitrate (a frame can't outgrow it); down, it would let
    /// frames burst.
    pub fn set_bitrate(&mut self, bps: u32) {
        let Some(c) = self.codec.clone() else { return };
        unsafe {
            if c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bps)).is_err() {
                return;
            }
            self.bitrate = bps;
            let hrd = hrd_bytes(bps, self.fps, self.tuning.buffer_frames);
            let resized = c.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(hrd)).is_ok()
                && c.GetValue(&CODECAPI_AVEncCommonBufferSize).map_or(true, |v| variant_value_u32(&v) == Some(hrd));
            if resized {
                self.hrd_bps = bps;
            } else if self.hrd_sized && needs_rebuild(bps, self.hrd_bps, self.renewed_at.elapsed()) {
                self.renew();
            }
        }
    }

    /// Swap the transform for a fresh one sized for the current bitrate. The
    /// access units already out are kept and the picture count carries on;
    /// the new one starts with a keyframe. If that fails the old one carries
    /// on (and no retry for 5 s).
    unsafe fn renew(&mut self) {
        if self.renew_after.is_some_and(|t| Instant::now() < t) {
            return;
        }
        // Keep what the old transform already handed back.
        while let Ok(true) = self.next_event(None) {}
        let (w, h) = self.size;
        match Self::new_tuned(&self.device, self.adapter_luid, w, h, self.fps, self.bitrate, self.rgb, self.video_codec, self.tuning) {
            Ok(mut fresh) => {
                tracing::info!(bps = self.bitrate, "rebuilt the encoder for the new bitrate (its HRD buffer is fixed once streaming)");
                fresh.frames_in = self.frames_in;
                fresh.next_out = self.frames_in;
                fresh.ready = std::mem::take(&mut self.ready);
                *self = fresh;
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't rebuild the encoder for the new bitrate");
                self.renew_after = Some(Instant::now() + Duration::from_secs(5));
            }
        }
    }

    /// Change the quality/speed trade-off (0 fastest .. 100 best) for the
    /// frames from now on. `false`: the encoder didn't take it.
    pub fn set_quality(&mut self, quality: u32) -> bool {
        let Some(c) = &self.codec else { return false };
        unsafe { c.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &variant_u32(quality)).is_ok() }
    }

    /// The next picture's [`Encoded::input`].
    pub fn next_input(&self) -> u64 {
        self.frames_in
    }

    /// Keep the next frame as long-term reference `index`.
    pub fn mark_ltr(&mut self, index: u32) -> bool {
        let Some(c) = &self.codec else { return false };
        unsafe { c.SetValue(&CODECAPI_AVEncVideoMarkLTRFrame, &variant_u32(index)).is_ok() }
    }

    /// Code the next frame from only the long-term references in `mask`.
    pub fn use_ltr(&mut self, mask: u32) -> bool {
        let Some(c) = &self.codec else { return false };
        unsafe { c.SetValue(&CODECAPI_AVEncVideoUseLTRFrame, &variant_u32((1 << 16) | (mask & 0xffff))).is_ok() }
    }

    /// Make the next frame a keyframe (IDR).
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Encode one picture (NV12, or BGRA for an RGB-input encoder) and wait
    /// for its access unit, which is what comes back in the normal case.
    /// Nothing the encoder hands back is dropped and access units come out in
    /// order: if an encoder holds pictures until it gets more input, it gets
    /// it — this then returns the oldest access unit waiting (`None` before
    /// there is one) and [`take_output`](Self::take_output) any further ones.
    ///
    /// An error (the encoder reported a failure, or went silent for a second:
    /// a lost device, a driver reset) means this encoder is done; build a new
    /// one.
    pub fn encode(&mut self, nv12: &ID3D11Texture2D) -> WinResult<Option<Encoded>> {
        unsafe {
            let deadline = Instant::now() + ENCODER_TIMEOUT;
            // What already happened: input credits, access units that came late.
            while self.next_event(None)? {}
            // An encoder whose newest picture already came out holds nothing back.
            if self.ready.back().is_some_and(|o| o.input + 1 == self.frames_in) {
                self.holds_frames = false;
            }
            while self.need_input == 0 {
                if !self.next_event(Some(deadline))? {
                    return Err(stalled("an input slot"));
                }
            }
            if self.force_key {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
                self.force_key = false;
            }
            let input = self.frames_in;
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(input as i64 * self.frame_duration)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.mft.transform.ProcessInput(0, &sample, 0)?;
            self.frames_in += 1;
            self.need_input -= 1;
            // Low-latency mode: this picture's access unit comes next.
            let grace = Instant::now() + HOLD_GRACE;
            while !self.ready.back().is_some_and(|o| o.input >= input) {
                if self.need_input > 0 {
                    // It wants more input before this picture comes out.
                    if self.holds_frames && !self.ready.is_empty() {
                        break;
                    }
                    if Instant::now() >= grace {
                        if !self.holds_frames {
                            tracing::info!(encoder = %self.name, "the encoder holds pictures until it gets more; streaming behind");
                            self.holds_frames = true;
                        }
                        break;
                    }
                }
                let until = if self.need_input > 0 { grace.min(deadline) } else { deadline };
                if !self.next_event(Some(until))? && Instant::now() >= deadline {
                    return Err(stalled("the picture's access unit"));
                }
            }
            Ok(self.ready.pop_front())
        }
    }

    /// The next access unit waiting beyond the one [`encode`](Self::encode)
    /// returned — there's one only when an encoder handed back several at
    /// once. Doesn't wait.
    /// Give the GPU's encoder session back now: sessions are limited, so a
    /// replacement may not open while this one still holds its own. The
    /// encoder is unusable after.
    pub fn release(&mut self) {
        self.mft.close();
    }

    pub fn take_output(&mut self) -> Option<Encoded> {
        self.ready.pop_front()
    }

    /// Handle one event from the transform: wait for it until `until`, or
    /// (`None`) only take one that's already there. `Ok(false)`: none came.
    unsafe fn next_event(&mut self, until: Option<Instant>) -> WinResult<bool> {
        let gone = || Error::new(MF_E_SHUTDOWN, "the encoder's event thread ended");
        let event = match until {
            None => match self.events.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => return Err(gone()),
            },
            Some(t) => match self.events.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(gone()),
            },
        };
        // `GetEvent` failing (the queue shut down) comes through as an error.
        let (kind, status) = event?;
        if kind == MEError.0 as u32 || status.is_err() {
            let code = if status.is_err() { status } else { E_FAIL };
            return Err(Error::new(code, format!("the hardware encoder failed (event {kind})")));
        }
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.collect_output()?;
        }
        Ok(true)
    }

    /// Collect the access unit an `METransformHaveOutput` announced.
    unsafe fn collect_output(&mut self) -> WinResult<()> {
        for attempt in 0..4 {
            match self.process_output() {
                Ok(sample) => {
                    if attempt > 0 {
                        self.unannounced += 1;
                    }
                    return self.keep(&sample);
                }
                // A new output type (some encoders publish their final
                // parameter sets this way): take it and ask again — some hand
                // the picture to that very call, others announce it anew.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.new_output_type()?,
                Err(_) if attempt > 0 => return Ok(()),
                // Nothing there: the late announcement of a picture a retry
                // already took (or an encoder saying so the synchronous way).
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT || (e.code() == E_UNEXPECTED && self.unannounced > 0) => {
                    self.unannounced = self.unannounced.saturating_sub(1);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// One `ProcessOutput` call (the transform provides the sample).
    unsafe fn process_output(&self) -> WinResult<IMFSample> {
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.mft.transform.ProcessOutput(0, &mut out, &mut status);
        let sample = ManuallyDrop::take(&mut out[0].pSample);
        let _ = ManuallyDrop::take(&mut out[0].pEvents);
        r?;
        sample.ok_or_else(|| Error::new(E_UNEXPECTED, "the encoder returned no sample"))
    }

    /// Accept the output type an `MF_E_TRANSFORM_STREAM_CHANGE` asked for.
    unsafe fn new_output_type(&mut self) -> WinResult<()> {
        let t = self.mft.transform.GetOutputAvailableType(0, 0)?;
        self.mft.transform.SetOutputType(0, &t, 0)?;
        if let Some(h) = blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER) {
            self.seq_header = h;
        }
        Ok(())
    }

    /// Queue an access unit, with the parameter sets put in front of a
    /// keyframe that lacks them.
    unsafe fn keep(&mut self, sample: &IMFSample) -> WinResult<()> {
        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        // Its picture: by time stamp (which also steps over a picture the
        // encoder dropped), else the next one in line.
        let stamped = sample
            .GetSampleTime()
            .ok()
            .map(|t| ((t + self.frame_duration / 2) / self.frame_duration).max(0) as u64);
        let input = stamped.filter(|i| (self.next_out..self.frames_in).contains(i)).unwrap_or(self.next_out);
        self.next_out = input + 1;
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = core::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let bytes: &[u8] = if ptr.is_null() { &[] } else { core::slice::from_raw_parts(ptr, len as usize) };
        // Only keyframes need the parameter sets, and those lead the access
        // unit: the scan stops at the first slice instead of reading it all.
        let header = keyframe && !has_parameter_sets(bytes, self.video_codec);
        let mut data = Vec::with_capacity(bytes.len() + if header { self.seq_header.len() } else { 0 });
        if header {
            data.extend_from_slice(&self.seq_header);
        }
        data.extend_from_slice(bytes);
        buffer.Unlock()?;
        self.ready.push_back(Encoded { data, keyframe, input });
        Ok(())
    }
}

/// The GPU's hardware encoder: NVIDIA's directly where it opens (see
/// [`nvenc`]: the bitrate changes in place, references are set per picture),
/// else the vendor's Media Foundation transform ([`MfEncoder`]).
/// `RDPIO_NVENC=0` on the host keeps to Media Foundation.
pub struct HwEncoder {
    inner: Inner,
    pub name: String,
    pub bitrate: u32,
    /// Keeps [`LTR_SLOTS`] long-term references on request.
    pub ltr: bool,
    /// Unique to this encoder's references (a rebuild has none of them).
    pub id: u64,
}

enum Inner {
    Mf(MfEncoder),
    Nv(Nvenc),
}

struct Nvenc {
    /// `None` once released.
    session: Option<nvenc::Session>,
    fps: u32,
    tuning: Tuning,
    frames_in: u64,
    force_key: bool,
    next: nvenc::Picture,
    /// A bitrate the driver refused, and when: not asked again for a while.
    refused: Option<(u32, Instant)>,
}

/// NVENC's preset: P1, the fastest. Measured on a screen of new office text
/// (HEVC, 1080p): P3-P7 code it no sharper (luma 32.9 dB against P1's 32.7)
/// for 3-4.3 ms a frame against 1.7, and neither the high-quality or
/// low-latency tuning, a first pass nor adaptive quantization comes near what
/// Media Foundation's quality setting gets (42 dB, ~8-11 ms) — so a
/// [`Tuning`] asking for quality goes there instead. `RDPIO_NVENC_PRESET=n`
/// picks another preset, for measuring.
fn nvenc_preset() -> u32 {
    std::env::var("RDPIO_NVENC_PRESET").ok().and_then(|v| v.parse::<u32>().ok()).map_or(1, |p| p.clamp(1, 7))
}

/// The quality ([`Tuning::quality`]) from which Media Foundation's NVIDIA
/// encoder codes new text sharper than NVENC directly can.
const MF_QUALITY: u32 = 80;

impl HwEncoder {
    /// A hardware encoder on `device` (prefer the one on `adapter_luid`).
    /// `rgb`: feed BGRA textures (the encoder converts) instead of NV12.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ID3D11Device,
        adapter_luid: Option<i64>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        rgb: bool,
        video_codec: crate::h264::Codec,
    ) -> WinResult<Self> {
        Self::new_tuned(device, adapter_luid, width, height, fps, bitrate, rgb, video_codec, Tuning::GAMING)
    }

    /// [`new`](Self::new) with a [`Tuning`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_tuned(
        device: &ID3D11Device,
        adapter_luid: Option<i64>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        rgb: bool,
        video_codec: crate::h264::Codec,
        tuning: Tuning,
    ) -> WinResult<Self> {
        if tuning.quality < MF_QUALITY && std::env::var("RDPIO_NVENC").map_or(true, |v| v != "0") {
            let preset = nvenc_preset();
            let slots = ltr_slots();
            let hevc = video_codec == crate::h264::Codec::Hevc;
            let vbv = hrd_bytes(bitrate, fps, tuning.buffer_frames);
            match nvenc::Session::open(device, hevc, (width, height), fps, bitrate, vbv, preset, slots, rgb) {
                Ok(session) => {
                    let name = format!("NVIDIA NVENC {} (direct, P{preset})", video_codec.name().to_uppercase());
                    tracing::info!(encoder = %name, width, height, fps, bitrate, rgb_input = rgb, ltr = slots > 0, "hardware encoder ready");
                    return Ok(Self {
                        inner: Inner::Nv(Nvenc {
                            session: Some(session),
                            fps: fps.max(1),
                            tuning,
                            frames_in: 0,
                            force_key: true,
                            next: nvenc::Picture::default(),
                            refused: None,
                        }),
                        name,
                        bitrate,
                        ltr: slots > 0,
                        id: NEXT_ENCODER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    });
                }
                Err(e) => tracing::info!(reason = %e, "NVIDIA's encoder directly: not here; through Media Foundation"),
            }
        }
        let mf = MfEncoder::new_tuned(device, adapter_luid, width, height, fps, bitrate, rgb, video_codec, tuning)?;
        Ok(Self { name: mf.name.clone(), bitrate: mf.bitrate, ltr: mf.ltr, id: mf.id, inner: Inner::Mf(mf) })
    }

    /// Change the target bitrate on the fly (NVENC: in place, the largest
    /// frame following; Media Foundation: see [`MfEncoder::set_bitrate`]).
    pub fn set_bitrate(&mut self, bps: u32) {
        match &mut self.inner {
            Inner::Mf(mf) => {
                mf.set_bitrate(bps);
                (self.bitrate, self.id, self.ltr) = (mf.bitrate, mf.id, mf.ltr);
            }
            Inner::Nv(nv) => {
                let Some(s) = nv.session.as_mut() else { return };
                if nv.refused.is_some_and(|(r, t)| t.elapsed() < Duration::from_secs(5) && (r as i64 - bps as i64).unsigned_abs() < r as u64 / 10) {
                    return;
                }
                match s.set_bitrate(bps, hrd_bytes(bps, nv.fps, nv.tuning.buffer_frames)) {
                    Ok(()) => {
                        self.bitrate = bps;
                        nv.refused = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, bps, "NVENC kept its bitrate");
                        nv.refused = Some((bps, Instant::now()));
                    }
                }
            }
        }
    }

    /// See [`MfEncoder::set_quality`] (NVENC: the preset is fixed per session).
    pub fn set_quality(&mut self, quality: u32) -> bool {
        match &mut self.inner {
            Inner::Mf(mf) => mf.set_quality(quality),
            Inner::Nv(_) => false,
        }
    }

    /// Keep the next frame as long-term reference `index`.
    pub fn mark_ltr(&mut self, index: u32) -> bool {
        match &mut self.inner {
            Inner::Mf(mf) => mf.mark_ltr(index),
            Inner::Nv(nv) => {
                nv.next.mark = Some(index);
                true
            }
        }
    }

    /// Code the next frame from only the long-term references in `mask`.
    pub fn use_ltr(&mut self, mask: u32) -> bool {
        match &mut self.inner {
            Inner::Mf(mf) => mf.use_ltr(mask),
            Inner::Nv(nv) => {
                nv.next.from = Some(mask);
                true
            }
        }
    }

    /// The next picture's [`Encoded::input`].
    pub fn next_input(&self) -> u64 {
        match &self.inner {
            Inner::Mf(mf) => mf.next_input(),
            Inner::Nv(nv) => nv.frames_in,
        }
    }

    /// Make the next frame a keyframe (IDR).
    pub fn request_keyframe(&mut self) {
        match &mut self.inner {
            Inner::Mf(mf) => mf.request_keyframe(),
            Inner::Nv(nv) => nv.force_key = true,
        }
    }

    /// Encode one picture; see [`MfEncoder::encode`]. NVENC hands back each
    /// picture's access unit before returning, so nothing is ever held.
    pub fn encode(&mut self, texture: &ID3D11Texture2D) -> WinResult<Option<Encoded>> {
        match &mut self.inner {
            Inner::Mf(mf) => mf.encode(texture),
            Inner::Nv(nv) => {
                let Some(s) = nv.session.as_mut() else {
                    return Err(Error::new(MF_E_SHUTDOWN, "the encoder was released"));
                };
                let pic = nvenc::Picture { keyframe: nv.force_key, ..nv.next };
                let input = nv.frames_in;
                let (data, keyframe, _) = s.encode(texture, pic, input).map_err(|e| Error::new(E_FAIL, e))?;
                nv.frames_in += 1;
                nv.force_key = false;
                nv.next = nvenc::Picture::default();
                Ok(Some(Encoded { data, keyframe, input }))
            }
        }
    }

    /// Give the GPU's encoder session back now (sessions are limited); the
    /// encoder is unusable after.
    pub fn release(&mut self) {
        match &mut self.inner {
            Inner::Mf(mf) => mf.release(),
            Inner::Nv(nv) => nv.session = None,
        }
    }

    /// Any further access unit waiting (Media Foundation only).
    pub fn take_output(&mut self) -> Option<Encoded> {
        match &mut self.inner {
            Inner::Mf(mf) => mf.take_output(),
            Inner::Nv(_) => None,
        }
    }
}

fn stalled(what: &str) -> Error {
    Error::new(ERROR_TIMEOUT.to_hresult(), format!("the hardware encoder stopped responding (waited for {what})"))
}

/// An activated encoder transform, shut down when dropped: asynchronous MFTs
/// keep their GPU session (NVENC/AMF/QSV), the D3D device and their worker
/// threads until told to.
struct Mft {
    transform: IMFTransform,
    activate: IMFActivate,
    /// Streaming started, so it's ended first.
    streaming: bool,
    /// Already shut down (by `close`).
    closed: bool,
}

impl Drop for Mft {
    fn drop(&mut self) {
        self.close();
    }
}

impl Mft {
    /// Shut the transform down now (dropping does it too, once).
    fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        // On a helper thread with a bounded wait: a transform that failed (a
        // hung driver) mustn't hang whoever drops it.
        let parts = FreeThreaded((self.transform.clone(), self.activate.clone()));
        let streaming = self.streaming;
        let (done_tx, done) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new().name("hyper-encoder-shutdown".into()).spawn(move || {
            let parts = parts;
            unsafe {
                with_mta(move || {
                    shut_down(&parts.0 .0, &parts.0 .1, streaming);
                    drop(parts);
                })
            };
            let _ = done_tx.send(());
        });
        match spawned {
            Ok(_) => {
                if done.recv_timeout(SHUTDOWN_TIMEOUT).is_err() {
                    tracing::warn!("the hardware encoder didn't shut down in time; leaving it behind");
                }
            }
            Err(_) => unsafe { shut_down(&self.transform, &self.activate, self.streaming) },
        }
    }
}

/// End streaming and shut the transform down; its activation object then lets
/// go of the transform and everything it holds.
unsafe fn shut_down(transform: &IMFTransform, activate: &IMFActivate, streaming: bool) {
    if streaming {
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    // This also ends the event thread: its `GetEvent` fails with MF_E_SHUTDOWN.
    if let Ok(s) = transform.cast::<IMFShutdown>() {
        let _ = s.Shutdown();
    }
    let _ = activate.ShutdownObject();
}

/// Carries Media Foundation objects to a helper thread: transforms and their
/// event queues are free-threaded, the bindings just don't mark them `Send`.
struct FreeThreaded<T>(T);
unsafe impl<T> Send for FreeThreaded<T> {}

/// Run `f` in COM's multithreaded apartment (on a helper thread).
unsafe fn with_mta<R>(f: impl FnOnce() -> R) -> R {
    let entered = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
    let r = f();
    if entered {
        CoUninitialize();
    }
    r
}

/// Encoder event threads running (tests check they end with their encoder).
static EVENT_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Receive the transform's events on a thread of their own. `GetEvent` waits
/// with no timeout, so the encoding thread waits on the returned channel
/// instead and can give up on an encoder that went silent. The thread ends
/// when the transform is shut down (`GetEvent` then fails, and that error is
/// the last thing sent).
fn spawn_event_pump(events: IMFMediaEventGenerator) -> WinResult<mpsc::Receiver<WinResult<(u32, HRESULT)>>> {
    let (tx, rx) = mpsc::channel();
    let events = FreeThreaded(events);
    EVENT_THREADS.fetch_add(1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("hyper-encoder-events".into()).spawn(move || {
        let events = events;
        unsafe {
            // It only waits: but when a game holds every core, a normal-priority
            // waiter adds its own scheduling delay to every encoded frame.
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            with_mta(move || loop {
                let event = events
                    .0
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
                    .and_then(|e| Ok((e.GetType()?, e.GetStatus()?)));
                let last = event.is_err();
                if tx.send(event).is_err() || last {
                    break;
                }
            })
        };
        EVENT_THREADS.fetch_sub(1, Ordering::Relaxed);
    });
    if let Err(e) = spawned {
        EVENT_THREADS.fetch_sub(1, Ordering::Relaxed);
        return Err(Error::new(E_FAIL, format!("encoder event thread: {e}")));
    }
    Ok(rx)
}

unsafe fn pick_hardware_encoder(adapter_luid: Option<i64>, codec: crate::h264::Codec) -> WinResult<IMFActivate> {
    let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
    let mut activates: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&in_info),
        Some(&out_info),
        &mut activates,
        &mut count,
    )?;
    if count == 0 || activates.is_null() {
        return Err(windows::core::Error::new(MF_E_NOT_FOUND, format!("no hardware {} encoder on this machine", codec.name())));
    }
    let list: Vec<IMFActivate> = (0..count as usize)
        .filter_map(|i| core::ptr::read(activates.add(i)))
        .collect();
    CoTaskMemFree(Some(activates as *const c_void));
    let on_adapter = |a: &IMFActivate| {
        adapter_luid.is_some_and(|luid| {
            let mut v = [0u8; 8];
            let mut got = 0u32;
            a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut v, Some(&mut got)).is_ok() && i64::from_le_bytes(v) == luid
        })
    };
    let chosen = list.iter().find(|a| on_adapter(a)).or(list.first()).cloned();
    chosen.ok_or_else(|| windows::core::Error::new(MF_E_NOT_FOUND, "no hardware H.264 encoder"))
}

unsafe fn blob(attrs: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    let size = attrs.GetBlobSize(key).ok()?;
    let mut v = vec![0u8; size as usize];
    attrs.GetBlob(key, &mut v, None).ok()?;
    Some(v)
}

/// Whether an Annex-B HEVC stream contains a NAL unit of `nal_type` (32 = VPS).
pub fn has_hevc_nal(annex_b: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            if (annex_b[i + 3] >> 1) & 0x3f == nal_type {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Does this machine have a hardware encoder for `codec`? Asked for every
/// sign-in, so the answer is kept: a yes for good, a no for 30 s (a driver
/// reset can hide the encoder for a moment).
pub fn can_encode(codec: crate::h264::Codec) -> bool {
    static KNOWN: Mutex<Vec<(crate::h264::Codec, bool, Instant)>> = Mutex::new(Vec::new());
    let known = KNOWN.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|k| k.0 == codec).map(|k| (k.1, k.2));
    if let Some((yes, at)) = known {
        if yes || at.elapsed() < Duration::from_secs(30) {
            return yes;
        }
    }
    let yes = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok() && pick_hardware_encoder(None, codec).is_ok() };
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    known.retain(|k| k.0 != codec);
    known.push((codec, yes, Instant::now()));
    yes
}

/// The client's decodable codecs, best first. `forced` limits it to one.
/// What the GPU decodes comes first; with no GPU decoder at all (a missing
/// GPU driver) H.264, the lighter one for the CPU.
pub fn client_codecs(forced: Option<crate::h264::Codec>) -> Vec<String> {
    use crate::h264::Codec;
    if let Some(c) = forced {
        return vec![c.name().into()];
    }
    let mut codecs: Vec<Codec> = [Codec::Hevc, Codec::H264].into_iter().filter(|&c| gpu_can_decode(c)).collect();
    for c in [Codec::H264, Codec::Hevc] {
        if !codecs.contains(&c) && can_decode(c, None) {
            codecs.push(c);
        }
    }
    if codecs.is_empty() {
        codecs.push(Codec::H264);
    }
    codecs.into_iter().map(|c| c.name().into()).collect()
}

/// Can the GPU this PC renders with decode `codec`? Not where the GPU driver
/// is missing: Windows' software renderer decodes nothing. Probed once per
/// codec (a device and a decoder: tens of milliseconds).
pub fn gpu_can_decode(codec: crate::h264::Codec) -> bool {
    // (Per GPU: on battery it's the display's.)
    let gpu = crate::d3d11::prefers_display_adapter();
    if let Some(&(_, _, yes)) = GPU_DECODERS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|k| (k.0, k.1) == (codec, gpu)) {
        return yes;
    }
    let yes = render_device().is_some_and(|(d, c)| can_decode(codec, Some((&d, &c))));
    GPU_DECODERS.lock().unwrap_or_else(|e| e.into_inner()).push((codec, gpu, yes));
    yes
}

/// What [`gpu_can_decode`] found, per codec and GPU choice (the display's or
/// the fastest).
static GPU_DECODERS: Mutex<Vec<(crate::h264::Codec, bool, bool)>> = Mutex::new(Vec::new());

/// Probe the GPU again next time: its driver came or went (a renderer had to
/// be rebuilt).
pub fn forget_gpu_decoders() {
    GPU_DECODERS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A device like the client's renderer makes: on the adapter it would choose
/// (none when it's told to render in software, `RDPIO_WARP`).
fn render_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    if std::env::var_os("RDPIO_WARP").is_some() {
        return None;
    }
    unsafe {
        // (Without IDXGIFactory6 — Windows 10 before 1803 — the renderer takes
        // the default adapter: so does this.)
        let adapter: Option<IDXGIAdapter> = crate::d3d11::chosen_adapter();
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE
        };
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            adapter.as_ref(),
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .ok()?;
        Some((device?, context?))
    }
}

/// The host's pick: the client's first preference we can encode at `size`
/// (H.264 encoders and decoders stop at 4096 pixels a side; HEVC goes on).
pub fn choose_codec(client: &[String], (w, h): (u32, u32)) -> Option<crate::h264::Codec> {
    use crate::h264::Codec;
    client
        .iter()
        .filter_map(|c| Codec::from_name(c))
        .filter(|&c| c != Codec::H264 || (w <= 4096 && h <= 4096))
        .find(|&c| can_encode(c))
}

/// Can this machine decode `codec` (GPU when `device` is given, else CPU)?
pub fn can_decode(codec: crate::h264::Codec, device: Option<(&ID3D11Device, &ID3D11DeviceContext)>) -> bool {
    match device {
        Some((d, c)) => crate::h264::H264GpuDecoder::with_codec(1280, 720, d, c, codec).is_ok(),
        None => crate::h264::H264Decoder::with_codec(1280, 720, codec).is_ok(),
    }
}

/// Whether an Annex-B stream contains a NAL unit of `nal_type`.
pub fn has_nal(annex_b: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            if annex_b[i + 3] & 0x1f == nal_type {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Whether an Annex-B access unit carries its parameter sets (an SPS for
/// H.264, a VPS for HEVC). They come before the first slice, so the scan stops
/// there rather than reading through the whole picture.
fn has_parameter_sets(annex_b: &[u8], codec: crate::h264::Codec) -> bool {
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            let header = annex_b[i + 3];
            match codec {
                crate::h264::Codec::H264 => match header & 0x1f {
                    7 => return true,
                    1..=5 => return false,
                    _ => {}
                },
                crate::h264::Codec::Hevc => match (header >> 1) & 0x3f {
                    32 => return true,
                    0..=31 => return false,
                    _ => {}
                },
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::Codec;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED};

    #[test]
    fn parameter_sets_are_looked_for_before_the_first_slice() {
        // H.264: AUD, SPS, PPS, IDR slice.
        let key = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88];
        assert!(has_parameter_sets(&key, Codec::H264));
        // A slice first: nothing after it is looked at.
        assert!(!has_parameter_sets(&[0, 0, 1, 0x41, 0x9a, 0, 0, 1, 0x67, 0x64], Codec::H264));
        assert!(!has_parameter_sets(&[0, 0, 1, 0x65, 0x88, 0x84], Codec::H264));
        assert!(!has_parameter_sets(&[], Codec::H264));
        // HEVC: AUD (35), VPS (32), SPS, PPS, IDR_W_RADL (19).
        let key = [0, 0, 0, 1, 0x46, 0x01, 0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01, 0, 0, 1, 0x44, 0x01, 0, 0, 1, 0x26, 0x01];
        assert!(has_parameter_sets(&key, Codec::Hevc));
        // A TRAIL_R slice (1) first.
        assert!(!has_parameter_sets(&[0, 0, 1, 0x02, 0x01, 0, 0, 1, 0x40, 0x01], Codec::Hevc));
        // The same answers the full scans give for those keyframes.
        assert!(has_hevc_nal(&key, 32));
        assert!(has_nal(&[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 0x64], 7));
    }

    #[test]
    fn hrd_buffer_is_two_frames_in_bytes() {
        assert_eq!(hrd_bytes(20_000_000, 120, 2), 41_666);
        assert_eq!(hrd_bytes(80_000_000, 60, 2), 333_333);
        assert_eq!(hrd_bytes(4_000_000_000, 1, 2), 1_000_000_000);
        assert_eq!(hrd_bytes(5_000_000, 0, 2), 1_250_000);
        assert_eq!(hrd_bytes(40_000_000, 60, 30), 2_500_000);
        // Rebuilds: big moves only, and not in quick succession…
        let s = Duration::from_secs;
        assert!(!needs_rebuild(30_000_000, 20_000_000, s(60)), "1.5x up isn't worth one");
        assert!(needs_rebuild(41_000_000, 20_000_000, s(6)));
        assert!(!needs_rebuild(41_000_000, 20_000_000, s(2)), "too soon after the last");
        assert!(needs_rebuild(7_000_000, 20_000_000, s(6)));
        assert!(!needs_rebuild(9_000_000, 20_000_000, s(60)), "2.2x down: the live bitrate is enough");
        // …except a collapse.
        assert!(needs_rebuild(4_000_000, 20_000_000, s(0)));
    }

    #[test]
    fn rotated_displays_swap_sides_and_turn_upright() {
        assert_eq!(oriented((1920, 1080), DXGI_MODE_ROTATION_ROTATE90), (1080, 1920));
        assert_eq!(oriented((1920, 1080), DXGI_MODE_ROTATION_ROTATE270), (1080, 1920));
        assert_eq!(oriented((1920, 1080), DXGI_MODE_ROTATION_ROTATE180), (1920, 1080));
        assert_eq!(oriented((1920, 1080), DXGI_MODE_ROTATION_IDENTITY), (1920, 1080));
        assert_eq!(oriented((1920, 1080), DXGI_MODE_ROTATION_UNSPECIFIED), (1920, 1080));
        assert_eq!(vp_rotation(DXGI_MODE_ROTATION_ROTATE90), Some(D3D11_VIDEO_PROCESSOR_ROTATION_90));
        assert_eq!(vp_rotation(DXGI_MODE_ROTATION_ROTATE180), Some(D3D11_VIDEO_PROCESSOR_ROTATION_180));
        assert_eq!(vp_rotation(DXGI_MODE_ROTATION_ROTATE270), Some(D3D11_VIDEO_PROCESSOR_ROTATION_270));
        assert_eq!(vp_rotation(DXGI_MODE_ROTATION_IDENTITY), None);
        assert_eq!(vp_rotation(DXGI_MODE_ROTATION_UNSPECIFIED), None);
        // A rotated mode reported either way round stands like the desktop
        // (DesktopCoordinates may be DPI-scaled, never turned).
        assert_eq!(shaped_like((1920, 1080), (864, 1536)), (1080, 1920));
        assert_eq!(shaped_like((1080, 1920), (864, 1536)), (1080, 1920));
        assert_eq!(shaped_like((2560, 1440), (2048, 1152)), (2560, 1440));
        assert_eq!(shaped_like((2560, 1440), (0, 0)), (2560, 1440));
        assert_eq!(oriented(shaped_like((1920, 1080), (1080, 1920)), DXGI_MODE_ROTATION_ROTATE90), (1920, 1080));
    }

    #[test]
    fn following_the_input_desktop_keeps_no_handles() {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        // On a thread of its own, like the host's receive thread.
        std::thread::spawn(|| {
            let count = || {
                let mut n = 0u32;
                unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) }.unwrap();
                n
            };
            // False where there's no interactive desktop (a service): then
            // this only shows nothing leaks on the failure path.
            let attached = input_desktop::attach(true);
            let before = count();
            for _ in 0..20_000 {
                input_desktop::attach(true);
                attach_input_desktop();
            }
            let after = count();
            eprintln!("on the input desktop: {attached}; handles {before} -> {after}");
            // It used to keep one desktop handle per call.
            assert!(after < before + 1_000, "handles went from {before} to {after} (attached: {attached})");
        })
        .join()
        .unwrap();
    }
}

/// On-hardware checks, run by hand on a machine with a hardware encoder and a
/// desktop to duplicate:
/// `cargo test -p rdp-gpu --lib hyper::hardware -- --ignored --nocapture --test-threads=1`
#[cfg(test)]
mod hardware {
    use super::*;
    use crate::h264::Codec;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
    };

    fn primary() -> String {
        list_monitors().unwrap()[0].device_name.clone()
    }

    /// A gradient with `amp` levels of noise on top, varied by `seed`: NV12,
    /// or BGRA for `rgb`.
    unsafe fn picture(device: &ID3D11Device, (w, h): (u32, u32), rgb: bool, seed: u32, amp: u32) -> ID3D11Texture2D {
        let (len, pitch, format) = if rgb {
            ((w * h * 4) as usize, w * 4, DXGI_FORMAT_B8G8R8A8_UNORM)
        } else {
            ((w * h * 3 / 2) as usize, w, DXGI_FORMAT_NV12)
        };
        let mut data = vec![0u8; len];
        let mut x = seed.wrapping_mul(2_654_435_761) | 1;
        for (i, b) in data.iter_mut().enumerate() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let base = ((i % pitch as usize) * 255 / pitch as usize) as u32;
            *b = (base * (256 - amp.min(256)) / 256 + (x >> 24) % amp.max(1)).min(255) as u8;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 };
        let mut t = None;
        device.CreateTexture2D(&desc, Some(&init), Some(&mut t)).unwrap();
        t.unwrap()
    }

    fn stats(label: &str, v: &mut [f64]) {
        if v.is_empty() {
            println!("{label:>22}: (none)");
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!("{label:>22}: n={:<4} p50={:8.2} p90={:8.2} max={:8.2}", v.len(), p(0.5), p(0.9), v[v.len() - 1]);
    }

    /// A screen of office text (ClearType, as Windows draws it: black, grey and
    /// coloured, 11-16 px) as BGRA.
    unsafe fn text_picture(w: u32, h: u32) -> Vec<u8> {
        use windows::core::w;
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::Graphics::Gdi::*;
        let dc = CreateCompatibleDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = core::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        let old = SelectObject(dc, bmp.into());
        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        FillRect(dc, &RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, white);
        SetBkMode(dc, TRANSPARENT);
        let colors = [0x0000_0000u32, 0x0055_5555, 0x00AD_4506, 0x0000_00CC, 0x0000_8800];
        let line = "The quick brown fox jumps over the lazy dog: 0123456789, Office text, menus & tables. ";
        let wide: Vec<u16> = line.repeat(4).encode_utf16().collect();
        // TEXT_GRAYSCALE=1: greyscale smoothing instead of ClearType.
        let smoothing = if std::env::var_os("TEXT_GRAYSCALE").is_some() { ANTIALIASED_QUALITY } else { CLEARTYPE_QUALITY };
        let (mut y, mut i) = (8i32, 0usize);
        while y < h as i32 - 20 {
            let size = [11, 12, 13, 15, 16][i % 5];
            let font = CreateFontW(-size, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, smoothing, 0, w!("Segoe UI"));
            let old_font = SelectObject(dc, font.into());
            SetTextColor(dc, COLORREF(colors[i % colors.len()]));
            let _ = TextOutW(dc, 8, y, &wide);
            SelectObject(dc, old_font);
            let _ = DeleteObject(font.into());
            y += size + 6;
            i += 1;
        }
        let _ = GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteObject(white.into());
        let _ = DeleteDC(dc);
        out
    }

    /// PSNR (dB) of a decoded NV12 picture against the BGRA original, over
    /// the colour channels, the way the client shows it (BT.709 limited).
    fn psnr(src: &[u8], nv12: &[u8], w: u32, h: u32) -> f64 {
        let (w, h) = (w as usize, h as usize);
        let mut se = 0f64;
        for y in 0..h {
            for x in 0..w {
                let yy = nv12[y * w + x] as f64 - 16.0;
                let uv = w * h + (y / 2) * w + (x & !1);
                let (u, v) = (nv12[uv] as f64 - 128.0, nv12[uv + 1] as f64 - 128.0);
                let r = 1.164 * yy + 1.793 * v;
                let g = 1.164 * yy - 0.213 * u - 0.533 * v;
                let b = 1.164 * yy + 2.112 * u;
                let s = &src[(y * w + x) * 4..];
                for (got, want) in [(r, s[2]), (g, s[1]), (b, s[0])] {
                    let d = got.clamp(0.0, 255.0) - want as f64;
                    se += d * d;
                }
            }
        }
        let mse = se / (w * h * 3) as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    }

    /// The source's own BT.709 limited-range NV12 (chroma averaged 2x2): what
    /// any 4:2:0 stream could at best carry.
    fn nv12_of(src: &[u8], w: u32, h: u32) -> Vec<u8> {
        let (w, h) = (w as usize, h as usize);
        let px = |x: usize, y: usize| {
            let s = &src[(y * w + x) * 4..];
            let (r, g, b) = (s[2] as f64, s[1] as f64, s[0] as f64);
            let yy = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
            let u = 128.0 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224.0 / 255.0;
            let v = 128.0 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224.0 / 255.0;
            (yy, u, v)
        };
        let mut out = vec![0u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                out[y * w + x] = px(x, y).0.round() as u8;
            }
        }
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let (mut u, mut v) = (0.0, 0.0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let p = px(x + dx, y + dy);
                    u += p.1 / 4.0;
                    v += p.2 / 4.0;
                }
                let i = w * h + (y / 2) * w + x;
                (out[i], out[i + 1]) = (u.round() as u8, v.round() as u8);
            }
        }
        out
    }

    /// Luma-only PSNR (dB) of two NV12 pictures.
    fn psnr_luma(a: &[u8], b: &[u8], w: u32, h: u32) -> f64 {
        let n = (w * h) as usize;
        let se: f64 = a[..n].iter().zip(&b[..n]).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        10.0 * (255.0f64 * 255.0 / (se / n as f64).max(1e-9)).log10()
    }

    /// H.264 against HEVC on the same moving picture (a page of text
    /// scrolling and panning, 1080p, 120 fps) at the same bitrates: encode and
    /// GPU-decode time per frame, the decoded picture's luma PSNR, and the
    /// bytes sent. `cargo test --release -p rdp-gpu --lib hyper::hardware::codecs_compared -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn codecs_compared() {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(ms: u32) -> u32;
        }
        unsafe {
            timeBeginPeriod(1);
            let (w, h) = (1920u32, 1080u32);
            let (bw, bh) = (w + 400, h + 800);
            let page = text_picture(bw, bh);
            let frame_bgra = |i: u32| {
                let (ox, oy) = ((i * 3) % 400, (i * 6) % 800);
                let mut out = vec![0u8; (w * h * 4) as usize];
                for y in 0..h {
                    let src = (((y + oy) * bw + ox) * 4) as usize;
                    out[(y * w * 4) as usize..((y + 1) * w * 4) as usize].copy_from_slice(&page[src..src + (w * 4) as usize]);
                }
                out
            };
            let frames: Vec<Vec<u8>> = (0..90).map(|i| nv12_of(&frame_bgra(i), w, h)).collect();
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
            let tex: ID3D11Texture2D = tex.unwrap();
            for mbps in [6u32, 12, 25] {
                for codec in [Codec::H264, Codec::Hevc] {
                    let mut enc = HwEncoder::new(&device, None, w, h, 120, mbps * 1_000_000, false, codec).unwrap();
                    let mut dec = crate::h264::H264GpuDecoder::with_codec(w, h, &device, &context, codec).unwrap();
                    let (mut enc_ms, mut dec_ms, mut psnrs, mut bytes) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
                    for (i, f) in frames.iter().enumerate() {
                        context.UpdateSubresource(&tex, 0, None, f.as_ptr() as *const c_void, w, 0);
                        let t = Instant::now();
                        let Some(u) = enc.encode(&tex).unwrap() else { continue };
                        enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
                        bytes += u.data.len();
                        let t = Instant::now();
                        let out = dec.decode(&u.data).unwrap();
                        dec_ms.push(t.elapsed().as_secs_f64() * 1e3);
                        if i >= 10 {
                            for d in out {
                                let mut td = D3D11_TEXTURE2D_DESC::default();
                                d.texture.GetDesc(&mut td);
                                let all = read_back(&device, &context, &d.texture, (td.Width, td.Height));
                                let mut px = Vec::with_capacity((w * h * 3 / 2) as usize);
                                for row in (0..h).chain(td.Height..td.Height + h / 2) {
                                    let at = (row * td.Width) as usize;
                                    px.extend_from_slice(&all[at..at + w as usize]);
                                }
                                psnrs.push(psnr_luma(f, &px, w, h));
                            }
                        }
                    }
                    let mean = psnrs.iter().sum::<f64>() / psnrs.len().max(1) as f64;
                    enc_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    dec_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    println!(
                        "{mbps:>2} Mbps {:>4}: luma {mean:5.1} dB | encode p50 {:.2} ms | GPU decode p50 {:.2} ms | {:.1} Mbps sent",
                        codec.name(),
                        enc_ms[enc_ms.len() / 2],
                        dec_ms[dec_ms.len() / 2],
                        bytes as f64 * 8.0 * 120.0 / frames.len() as f64 / 1e6
                    );
                }
            }
        }
    }

    /// BGRA in, the colours out right: colour bars through the encoder's own
    /// RGB conversion and back, against BT.709 limited range (what clients
    /// decode with), NVENC directly and through Media Foundation. A wrong
    /// matrix (BT.601) shifts saturated colours by 10+ levels.
    /// `cargo test --release -p rdp-gpu --lib hyper::hardware::rgb_input_keeps_its_colours -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn rgb_input_keeps_its_colours() {
        unsafe {
            let (w, h) = (1280u32, 720u32);
            let bars: [[u8; 3]; 8] = [[255, 255, 255], [255, 255, 0], [0, 255, 255], [0, 255, 0], [255, 0, 255], [255, 0, 0], [0, 0, 255], [40, 40, 40]];
            let mut src = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let [r, g, b] = bars[(x * 8 / w) as usize];
                    let i = ((y * w + x) * 4) as usize;
                    src[i..i + 4].copy_from_slice(&[b, g, r, 255]);
                }
            }
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let device: ID3D11Device = device.unwrap();
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: src.as_ptr() as *const c_void, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).unwrap();
            let tex = tex.unwrap();
            let mut scores = Vec::new();
            for direct in ["1", "0"] {
                std::env::set_var("RDPIO_NVENC", direct);
                let mut enc = HwEncoder::new(&device, None, w, h, 60, 40_000_000, true, Codec::H264).unwrap();
                let mut dec = crate::h264::H264Decoder::with_codec(w, h, Codec::H264).unwrap();
                let mut last = None;
                for _ in 0..30 {
                    if let Some(u) = enc.encode(&tex).unwrap() {
                        last = dec.decode(&u.data).unwrap().pop().or(last);
                    }
                }
                let f = last.expect("a decoded picture");
                // Mid-bar samples: what each colour came back as.
                let at = |x: u32| {
                    let (yy, uv) = ((h / 2 * w + x) as usize, (w * h + (h / 4) * w + (x & !1)) as usize);
                    (f.nv12[yy], f.nv12[uv], f.nv12[uv + 1])
                };
                let reference = nv12_of(&src, w, h);
                let want = |x: u32| {
                    let (yy, uv) = ((h / 2 * w + x) as usize, (w * h + (h / 4) * w + (x & !1)) as usize);
                    (reference[yy], reference[uv], reference[uv + 1])
                };
                let worst = (0..8).map(|b| {
                    let x = b * w / 8 + w / 16;
                    let (g, e) = (at(x), want(x));
                    [g.0 as i32 - e.0 as i32, g.1 as i32 - e.1 as i32, g.2 as i32 - e.2 as i32].iter().map(|d| d.abs()).max().unwrap()
                }).max().unwrap();
                let db = psnr(&src, &f.nv12, w, h);
                println!("{}: colour PSNR {db:.1} dB, worst mid-bar YUV error {worst} levels", enc.name);
                scores.push((db, worst));
            }
            std::env::remove_var("RDPIO_NVENC");
            assert!(scores[0].1 <= 3, "NVENC's own RGB conversion is off by {} levels", scores[0].1);
        }
    }

    /// Office text through the encoder and back under each tuning: how close
    /// the decoded picture is to the original — the first frame (all new
    /// text) and after a second of a still screen — and what it cost.
    #[test]
    #[ignore]
    fn text_quality_by_tuning() {
        unsafe {
            let (w, h) = (1920u32, 1080u32);
            let src = text_picture(w, h);
            let reference = nv12_of(&src, w, h);
            println!("4:2:0 alone (no codec): {:.1} dB (luma exact)", psnr(&src, &reference, w, h));
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
            let device: ID3D11Device = device.unwrap();
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: src.as_ptr() as *const c_void, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).unwrap();
            let tex = tex.unwrap();
            let cases: Vec<(String, Codec, u32, u32, Tuning)> = std::env::var("TEXT_CASES")
                .ok()
                .map(|v| {
                    v.split(';')
                        .map(|c| {
                            let p: Vec<&str> = c.split(',').collect();
                            let codec = if p[0] == "hevc" { Codec::Hevc } else { Codec::H264 };
                            let t = Tuning { buffer_frames: p[3].parse().unwrap(), quality: p[4].parse().unwrap() };
                            (c.to_string(), codec, p[1].parse().unwrap(), p[2].parse::<u32>().unwrap() * 1_000_000, t)
                        })
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec![
                        ("gaming 240fps 17Mbps".into(), Codec::H264, 240, 17_000_000, Tuning::GAMING),
                        ("60fps 17Mbps".into(), Codec::H264, 60, 17_000_000, Tuning::GAMING),
                        ("60fps 40Mbps".into(), Codec::H264, 60, 40_000_000, Tuning::GAMING),
                        ("60fps 40Mbps buf30".into(), Codec::H264, 60, 40_000_000, Tuning { buffer_frames: 30, quality: 0 }),
                        ("60fps 40Mbps buf30 q100".into(), Codec::H264, 60, 40_000_000, Tuning { buffer_frames: 30, quality: 100 }),
                        ("hevc 60fps 40Mbps buf30 q100".into(), Codec::Hevc, 60, 40_000_000, Tuning { buffer_frames: 30, quality: 100 }),
                    ]
                });
            for (label, codec, fps, bps, tuning) in cases {
                let mut enc = HwEncoder::new_tuned(&device, None, w, h, fps, bps, true, codec, tuning).unwrap();
                let mut dec = crate::h264::H264Decoder::with_codec(w, h, codec).unwrap();
                let (mut scores, mut sizes, mut lumas, mut ms) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for _ in 0..fps {
                    let t = Instant::now();
                    let mut units: Vec<Encoded> = enc.encode(&tex).unwrap().into_iter().collect();
                    ms.push(t.elapsed().as_secs_f64() * 1e3);
                    while let Some(u) = enc.take_output() {
                        units.push(u);
                    }
                    for u in units {
                        sizes.push(u.data.len());
                        for f in dec.decode(&u.data).unwrap() {
                            scores.push(psnr(&src, &f.nv12, w, h));
                            lumas.push(psnr_luma(&reference, &f.nv12, w, h));
                        }
                    }
                }
                let at = |i: usize| scores.get(i).copied().unwrap_or(0.0);
                // How the picture sharpens as the same frame goes in again.
                let series: Vec<String> = [0usize, 1, 2, 3, 5, 8, 15, 30]
                    .iter()
                    .filter_map(|&i| Some(format!("{i}:{:.1}", lumas.get(i)?)))
                    .collect();
                println!("{label:>32}: luma by frame {}", series.join(" "));
                println!(
                    "{label:>32}: first frame {:5.1} dB, luma {:5.1} ({:>4} KB, {:4.1} ms) | 0.25 s {:5.1} | 1 s {:5.1} dB, luma {:5.1} | {} frames, {:.1} MB",
                    at(0),
                    lumas.first().copied().unwrap_or(0.0),
                    sizes.first().copied().unwrap_or(0) / 1024,
                    ms.first().copied().unwrap_or(0.0),
                    at(scores.len() / 4),
                    scores.last().copied().unwrap_or(0.0),
                    lumas.last().copied().unwrap_or(0.0),
                    scores.len(),
                    sizes.iter().sum::<usize>() as f64 / 1e6
                );
            }
        }
    }

    /// Long-term references: frame 0 kept, frames 1-4 lost on the way, frame
    /// 5 coded from frame 0 only. A decoder that got just frames 0 and 5
    /// should show 5 right, at the size of a normal frame, not a keyframe's.
    /// `cargo test --release -p rdp-gpu --lib hyper::hardware::ltr_recovers_a_loss -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn ltr_recovers_a_loss() {
        unsafe {
            let (w, h) = (1920u32, 1080u32);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let device: ID3D11Device = device.unwrap();
            let context: ID3D11DeviceContext = context.unwrap();
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            // A drifting gradient with a square moving across it: frames alike
            // enough to code from each other.
            let frame = |i: u32| -> Vec<u8> {
                let mut v = vec![128u8; (w * h * 3 / 2) as usize];
                for y in 0..h {
                    for x in 0..w {
                        let sq = x.wrapping_sub(100 + i * 40) < 200 && y.wrapping_sub(300 + i * 10) < 200;
                        v[(y * w + x) as usize] = if sq { 235 } else { (16 + (x / 4 + y / 8 + i * 3) % 200) as u8 };
                    }
                }
                v
            };
            let tex = |data: &[u8]| {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, CPUAccessFlags: 0, MiscFlags: 0,
                };
                let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: w, SysMemSlicePitch: 0 };
                let mut t = None;
                device.CreateTexture2D(&desc, Some(&init), Some(&mut t)).unwrap();
                t.unwrap()
            };
            for codec in [Codec::H264, Codec::Hevc] {
                let mut enc = HwEncoder::new_tuned(&device, None, w, h, 60, 20_000_000, false, codec, Tuning::GAMING).unwrap();
                let mut dec = crate::h264::H264Decoder::with_codec(w, h, codec).unwrap();
                // The GPU decoder clients use, read back to compare.
                let mut gpu = crate::h264::H264GpuDecoder::with_codec(w, h, &device, &context, codec).unwrap();
                let context = context.clone();
                let mut line = Vec::new();
                for i in 0..9u32 {
                    let src = frame(i);
                    let (mut note, mut deliver) = (String::new(), true);
                    match i {
                        0 => note = format!(" mark0={}", enc.mark_ltr(0)),
                        1..=4 => deliver = false,
                        5 => note = format!(" use0={}", enc.use_ltr(1)),
                        _ => {}
                    }
                    let mut units: Vec<Encoded> = enc.encode(&tex(&src)).unwrap().into_iter().collect();
                    while let Some(u) = enc.take_output() {
                        units.push(u);
                    }
                    let kb: usize = units.iter().map(|u| u.data.len()).sum::<usize>() / 1024;
                    let key = units.iter().any(|u| u.keyframe);
                    let shown = if deliver {
                        let mut out = Vec::new();
                        for u in &units {
                            match dec.decode(&u.data) {
                                // Matched against every frame so far: a decoder may hand pictures out late.
                                Ok(fs) => out.extend(fs.into_iter().map(|f| {
                                    let (best, k) = (0..=i).map(|k| (psnr_luma(&frame(k), &f.nv12, w, h), k)).fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a });
                                    format!("frame {k} at {best:.1}dB")
                                })),
                                Err(e) => out.push(format!("error {e}")),
                            }
                        }
                        for u in &units {
                            match gpu.decode(&u.data) {
                                Ok(ts) => out.extend(ts.into_iter().map(|t| {
                                    // (The decoder's textures may be taller: 1088 rows.)
                                    let mut d = D3D11_TEXTURE2D_DESC::default();
                                    t.texture.GetDesc(&mut d);
                                    let all = read_back(&device, &context, &t.texture, (d.Width, d.Height));
                                    let mut px = Vec::with_capacity((w * h * 3 / 2) as usize);
                                    for row in (0..h).chain(d.Height..d.Height + h / 2) {
                                        let at = (row * d.Width) as usize;
                                        px.extend_from_slice(&all[at..at + w as usize]);
                                    }
                                    let (best, k) = (0..=i).map(|k| (psnr_luma(&frame(k), &px, w, h), k)).fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a });
                                    format!("GPU frame {k} at {best:.1}dB")
                                })),
                                Err(e) => out.push(format!("GPU error {e}")),
                            }
                        }
                        out.join(",")
                    } else {
                        "lost".into()
                    };
                    line.push(format!("{i}{note}: {kb}KB{} {shown}", if key { " KEY" } else { "" }));
                }
                println!("{}:
  {}", codec.name(), line.join("
  "));
            }
        }
    }

    /// Does the quality setting take effect mid-stream? A screen of new text
    /// at the fastest setting, then the same picture again at the best.
    /// `cargo test --release -p rdp-gpu --lib hyper::hardware::quality_changes_mid_stream -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn quality_changes_mid_stream() {
        unsafe {
            let (w, h) = (1920u32, 1080u32);
            let src = text_picture(w, h);
            let reference = nv12_of(&src, w, h);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let device: ID3D11Device = device.unwrap();
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: src.as_ptr() as *const c_void, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).unwrap();
            let tex = tex.unwrap();
            // Fresh pictures every frame: the time each takes shows the setting.
            let pics: Vec<_> = (0..4).map(|i| picture(&device, (w, h), false, i, 24)).collect();
            for codec in [Codec::Hevc, Codec::H264] {
                let mut enc = HwEncoder::new_tuned(&device, None, w, h, 60, 30_000_000, false, codec, Tuning { buffer_frames: 15, quality: 0 }).unwrap();
                let mut by_q = [Vec::new(), Vec::new()];
                for i in 0..48usize {
                    let q = if (i / 6) % 2 == 0 { 0 } else { 100 };
                    enc.set_quality(q);
                    std::thread::sleep(Duration::from_millis(20));
                    let t = Instant::now();
                    let _ = enc.encode(&pics[i % pics.len()]).unwrap();
                    while enc.take_output().is_some() {}
                    if i % 6 != 0 {
                        by_q[(q == 100) as usize].push(t.elapsed().as_secs_f64() * 1e3);
                    }
                }
                stats(&format!("{} switched to q0", codec.name()), &mut by_q[0]);
                stats(&format!("{} switched to q100", codec.name()), &mut by_q[1]);
            }
            for codec in [Codec::Hevc, Codec::H264] {
                let mut enc = HwEncoder::new_tuned(&device, None, w, h, 60, 30_000_000, true, codec, Tuning { buffer_frames: 15, quality: 0 }).unwrap();
                let mut dec = crate::h264::H264Decoder::with_codec(w, h, codec).unwrap();
                let mut line = Vec::new();
                for i in 0..8 {
                    let q = [0, 0, 100, 100, 0, 0, 100, 0][i];
                    let took = enc.set_quality(q);
                    std::thread::sleep(Duration::from_millis(30));
                    let t = Instant::now();
                    let mut units: Vec<Encoded> = enc.encode(&tex).unwrap().into_iter().collect();
                    let ms = t.elapsed().as_secs_f64() * 1e3;
                    while let Some(u) = enc.take_output() {
                        units.push(u);
                    }
                    let kb: usize = units.iter().map(|u| u.data.len()).sum::<usize>() / 1024;
                    let luma = units.iter().flat_map(|u| dec.decode(&u.data).unwrap()).last().map(|f| psnr_luma(&reference, &f.nv12, w, h)).unwrap_or(0.0);
                    line.push(format!("q{q}{} {ms:.1}ms {kb}KB {luma:.1}dB", if took { "" } else { "(refused)" }));
                }
                println!("{}: {}", codec.name(), line.join(" | "));
            }
        }
    }

    /// What the encoder costs per frame on a quiet desktop (a change every
    /// 200 ms, the GPU idling in between), by preset, with and without the
    /// 1 ms system timer. `cargo test --release -p rdp-gpu --lib hyper::hardware::encode_cost_idle -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn encode_cost_idle() {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(ms: u32) -> u32;
            fn timeEndPeriod(ms: u32) -> u32;
        }
        unsafe {
            let (w, h) = (2256u32, 1268u32);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let device: ID3D11Device = device.unwrap();
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let pics: Vec<_> = (0..4).map(|i| picture(&device, (w, h), false, i, 24)).collect();
            for fine in [false, true] {
                if fine { timeBeginPeriod(1); }
                // IDLE_QVS=0,50,100 sweeps the office tuning's quality instead.
                let tunings: Vec<(String, Tuning)> = match std::env::var("IDLE_QVS") {
                    Ok(v) => v.split(',').map(|q| (format!("qvs {q}"), Tuning { quality: q.trim().parse().unwrap(), ..Tuning::office(60) })).collect(),
                    Err(_) => vec![("speed".into(), Tuning::GAMING), ("quality".into(), Tuning::office(60))],
                };
                for (label, tuning) in tunings {
                    let mut enc = HwEncoder::new_tuned(&device, None, w, h, 60, 30_000_000, false, Codec::Hevc, tuning).unwrap();
                    let mut ms = Vec::new();
                    for i in 0..25usize {
                        std::thread::sleep(Duration::from_millis(200));
                        let t = Instant::now();
                        let _ = enc.encode(&pics[i % pics.len()]).unwrap();
                        while enc.take_output().is_some() {}
                        if i > 0 { ms.push(t.elapsed().as_secs_f64() * 1e3); }
                    }
                    stats(&format!("{label}, timer {}", if fine { "1 ms" } else { "default" }), &mut ms);
                }
                if fine { timeEndPeriod(1); }
            }
        }
    }

    /// Each access unit in, its picture out at once (a decoder that holds one
    /// back adds a whole frame of latency), on the GPU decoder clients use
    /// and the CPU one. `cargo test --release -p rdp-gpu --lib hyper::hardware::decoders_hold_nothing_back -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn decoders_hold_nothing_back() {
        unsafe {
            let (w, h) = (1920u32, 1080u32);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let pics: Vec<_> = (0..4).map(|i| picture(&device, (w, h), false, i, 24)).collect();
            for codec in [Codec::H264, Codec::Hevc] {
                let mut enc = HwEncoder::new(&device, None, w, h, 60, 20_000_000, false, codec).unwrap();
                let mut gpu = crate::h264::H264GpuDecoder::with_codec(w, h, &device, &context, codec).unwrap();
                let mut cpu = crate::h264::H264Decoder::with_codec(w, h, codec).unwrap();
                let (mut g, mut c) = (Vec::new(), Vec::new());
                for i in 0..10usize {
                    let mut units: Vec<Encoded> = enc.encode(&pics[i % pics.len()]).unwrap().into_iter().collect();
                    while let Some(u) = enc.take_output() {
                        units.push(u);
                    }
                    let (mut gn, mut cn) = (0, 0);
                    for u in &units {
                        gn += gpu.decode(&u.data).map_or(0, |v| v.len());
                        cn += cpu.decode(&u.data).map_or(0, |v| v.len());
                    }
                    g.push(gn.to_string());
                    c.push(cn.to_string());
                }
                println!("{}: pictures out per frame in: GPU [{}] CPU [{}]", codec.name(), g.join(" "), c.join(" "));
            }
        }
    }

    /// What a client's GPU decoder costs per frame for game-like motion
    /// (fresh noise every frame) at 1080p, by codec and bitrate.
    /// `cargo test --release -p rdp-gpu --lib hyper::hardware::decode_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn decode_cost_per_frame() {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(ms: u32) -> u32;
        }
        unsafe {
            // DECODE_TIMER_1MS=1: the system timer at 1 ms, as games set it.
            if std::env::var_os("DECODE_TIMER_1MS").is_some() {
                timeBeginPeriod(1);
            }
            let (w, h) = (1920u32, 1080u32);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut context)).unwrap();
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            let _ = device.cast::<ID3D11Multithread>().map(|m| m.SetMultithreadProtected(true));
            let pics: Vec<_> = (0..8).map(|i| picture(&device, (w, h), false, i, 96)).collect();
            for (codec, mbps) in [(Codec::Hevc, 50u32), (Codec::H264, 50), (Codec::Hevc, 15)] {
                let mut enc = HwEncoder::new(&device, None, w, h, 240, mbps * 1_000_000, false, codec).unwrap();
                let mut units = Vec::new();
                for i in 0..240usize {
                    units.extend(enc.encode(&pics[i % pics.len()]).unwrap());
                    while let Some(u) = enc.take_output() { units.push(u); }
                }
                let mut dec = crate::h264::H264GpuDecoder::with_codec(w, h, &device, &context, codec).unwrap();
                let mut ms = Vec::new();
                for u in &units {
                    let t = Instant::now();
                    let _ = dec.decode(&u.data).unwrap();
                    ms.push(t.elapsed().as_secs_f64() * 1e3);
                }
                let kb = units.iter().map(|u| u.data.len()).sum::<usize>() / units.len() / 1024;
                stats(&format!("{} {mbps} Mbps ({kb} KB/frame) decode ms", codec.name()), &mut ms);
            }
        }
    }

    /// Moving pictures, then a still one: every access unit comes back for its
    /// own picture, keyframes carry their parameter sets. Prints timings.
    fn round_trip(codec: Codec, rgb: bool) {
        unsafe {
            let cap = if rgb { DesktopCapture::new_rgb(&primary(), 1) } else { DesktopCapture::new(&primary(), None, 1) }.unwrap();
            let size = cap.stream;
            let mut enc = HwEncoder::new(&cap.device, cap.adapter_luid().ok(), size.0, size.1, 120, 30_000_000, rgb, codec).unwrap();
            println!("{} {}x{} rgb={rgb}", enc.name, size.0, size.1);
            let a = picture(&cap.device, size, rgb, 1, 256);
            let b = picture(&cap.device, size, rgb, 2, 256);
            let (mut key_ms, mut motion_ms, mut still_ms, mut key_kb, mut motion_kb) = (vec![], vec![], vec![], vec![], vec![]);
            for i in 0..360u64 {
                if i % 60 == 0 {
                    enc.request_keyframe();
                }
                let still = i >= 240;
                let t = Instant::now();
                let out = enc.encode(if still || i % 2 == 0 { &a } else { &b }).unwrap().expect("an access unit per picture");
                let ms = t.elapsed().as_secs_f64() * 1e3;
                assert_eq!(out.input, i);
                assert!(enc.take_output().is_none());
                assert!(!out.data.is_empty());
                assert!(i != 0 || out.keyframe, "the first access unit is a keyframe");
                let kb = out.data.len() as f64 / 1024.0;
                if out.keyframe {
                    assert!(has_parameter_sets(&out.data, codec), "a keyframe without parameter sets");
                    key_ms.push(ms);
                    key_kb.push(kb);
                } else if still {
                    still_ms.push(ms);
                } else {
                    motion_ms.push(ms);
                    motion_kb.push(kb);
                }
            }
            stats("keyframe encode ms", &mut key_ms);
            stats("motion encode ms", &mut motion_ms);
            stats("still encode ms", &mut still_ms);
            stats("keyframe KB", &mut key_kb);
            stats("motion frame KB", &mut motion_kb);
        }
    }

    #[test]
    #[ignore]
    fn h264_round_trip() {
        round_trip(Codec::H264, false);
    }

    #[test]
    #[ignore]
    fn hevc_round_trip() {
        round_trip(Codec::Hevc, false);
    }

    #[test]
    #[ignore]
    fn rgb_round_trip() {
        round_trip(Codec::H264, true);
    }

    /// An encoder that was shut down under us (as a lost device leaves it)
    /// fails at once instead of waiting forever.
    #[test]
    #[ignore]
    fn a_dead_encoder_fails_fast() {
        unsafe {
            let cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).unwrap();
            let pic = picture(&cap.device, cap.stream, false, 3, 64);
            let mut enc = MfEncoder::new(&cap.device, cap.adapter_luid().ok(), 1280, 720, 60, 10_000_000, false, Codec::H264).unwrap();
            assert!(enc.encode(&pic).unwrap().is_some());
            enc.mft.transform.cast::<IMFShutdown>().unwrap().Shutdown().unwrap();
            let t = Instant::now();
            let e = enc.encode(&pic).err().expect("an error from a shut-down encoder");
            println!("failed in {:.2} ms: {e}", t.elapsed().as_secs_f64() * 1e3);
            assert!(t.elapsed() < Duration::from_millis(500));
            assert!(enc.encode(&pic).is_err(), "and keeps failing");
        }
    }

    /// An encoder that goes silent (no events at all) ends in a timeout error
    /// after [`ENCODER_TIMEOUT`] instead of blocking the stream for good.
    #[test]
    #[ignore]
    fn a_silent_encoder_times_out() {
        unsafe {
            let cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).unwrap();
            let pic = picture(&cap.device, cap.stream, false, 5, 64);
            let mut enc = MfEncoder::new(&cap.device, cap.adapter_luid().ok(), 1280, 720, 60, 10_000_000, false, Codec::H264).unwrap();
            // Its events stop arriving (the sender stays alive, sending nothing).
            let (_quiet, events) = mpsc::channel();
            enc.events = events;
            let t = Instant::now();
            let e = enc.encode(&pic).err().expect("a timeout");
            let waited = t.elapsed();
            println!("gave up after {:.0} ms: {e}", waited.as_secs_f64() * 1e3);
            assert_eq!(e.code(), ERROR_TIMEOUT.to_hresult());
            assert!(waited >= ENCODER_TIMEOUT && waited < ENCODER_TIMEOUT + Duration::from_millis(500));
        }
    }

    /// A dropped encoder is shut down: its event thread ends (so the
    /// transform's event queue was), and encoders can be made and dropped over
    /// and over without running out of hardware sessions.
    #[test]
    #[ignore]
    fn dropped_encoders_shut_down() {
        unsafe {
            let cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).unwrap();
            let pic = picture(&cap.device, cap.stream, false, 7, 64);
            let base = EVENT_THREADS.load(Ordering::Relaxed);
            for round in 0..12 {
                let mut enc = MfEncoder::new(&cap.device, cap.adapter_luid().ok(), 1280, 720, 60, 10_000_000, false, Codec::H264).unwrap();
                assert!(enc.encode(&pic).unwrap().is_some());
                assert_eq!(EVENT_THREADS.load(Ordering::Relaxed), base + 1);
                let t = Instant::now();
                drop(enc);
                let dropped_ms = t.elapsed().as_secs_f64() * 1e3;
                while EVENT_THREADS.load(Ordering::Relaxed) > base {
                    assert!(t.elapsed() < Duration::from_secs(2), "round {round}: the event thread outlived its encoder");
                    std::thread::sleep(Duration::from_millis(1));
                }
                if round == 0 {
                    println!("drop took {dropped_ms:.2} ms; event thread gone after {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
                }
            }
        }
    }

    /// A process may duplicate a monitor only once: a pipeline rebuild has to
    /// let go of the old capture (and its encoder session) before it opens
    /// the new ones.
    #[test]
    #[ignore]
    fn a_released_pipeline_can_be_reopened() {
        unsafe {
            let mut cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).unwrap();
            let pic = picture(&cap.device, cap.stream, false, 7, 64);
            let mut enc = HwEncoder::new(&cap.device, cap.adapter_luid().ok(), 1280, 720, 60, 10_000_000, false, Codec::H264).unwrap();
            assert!(enc.encode(&pic).unwrap().is_some());
            assert!(DesktopCapture::new(&primary(), Some((1280, 720)), 1).is_err(), "a second duplication of the same monitor");
            cap.release();
            enc.release();
            let cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).expect("reopened after release");
            let pic = picture(&cap.device, cap.stream, false, 7, 64);
            let mut enc = HwEncoder::new(&cap.device, cap.adapter_luid().ok(), 1280, 720, 60, 10_000_000, false, Codec::H264).unwrap();
            assert!(enc.encode(&pic).unwrap().is_some());
        }
    }

    /// Encode `n` of `pics` in turn; (largest frame, mean Mbps at 120 fps)
    /// over all but the first 60. Checks pictures keep their numbers.
    unsafe fn run(enc: &mut HwEncoder, pics: &[ID3D11Texture2D], n: usize) -> (usize, f64) {
        let (mut max, mut sum) = (0, 0);
        for i in 0..n {
            let expected = enc.next_input();
            let out = enc.encode(&pics[i % pics.len()]).unwrap().unwrap();
            assert_eq!(out.input, expected);
            if i >= 60 {
                max = max.max(out.data.len());
                sum += out.data.len();
            }
        }
        (max, (sum / (n - 60)) as f64 * 8.0 * 120.0 / 1e6)
    }

    /// Raised past what the HRD buffer was sized for, the output keeps up
    /// (NVENC keeps the buffer it started with, which capped a 20 Mbps start
    /// at ~40 Mbps; the encoder is rebuilt instead).
    #[test]
    #[ignore]
    fn hrd_buffer_follows_the_bitrate_up() {
        unsafe {
            let cap = DesktopCapture::new(&primary(), None, 1).unwrap();
            let size = cap.stream;
            let pics: Vec<_> = (0..4).map(|s| picture(&cap.device, size, false, s, 64)).collect();
            let mut enc = HwEncoder::new(&cap.device, cap.adapter_luid().ok(), size.0, size.1, 120, 20_000_000, false, Codec::H264).unwrap();
            let start = run(&mut enc, &pics, 180);
            let t = Instant::now();
            enc.set_bitrate(60_000_000);
            let took = t.elapsed().as_secs_f64() * 1e3;
            let first = enc.encode(&pics[0]).unwrap().unwrap();
            let up = run(&mut enc, &pics, 240);
            println!("(largest frame B, Mbps) at 20 Mbps {start:?}; raised to 60 {up:?}; set_bitrate took {took:.1} ms");
            if enc.name.contains("direct") {
                assert!(!first.keyframe, "NVENC directly changes the bitrate in place: no keyframe");
                assert!(took < 5.0, "and at once");
            } else {
                assert!(first.keyframe, "a rebuilt encoder starts with a keyframe");
            }
            assert!(up.1 > 50.0, "the output didn't follow the bitrate up");
        }
    }

    /// Lowered well below what the HRD buffer was sized for, frames stay as
    /// small as an encoder started at the low rate keeps them.
    #[test]
    #[ignore]
    fn hrd_buffer_follows_the_bitrate_down() {
        unsafe {
            let cap = DesktopCapture::new(&primary(), Some((1280, 720)), 1).unwrap();
            let size = cap.stream;
            let pics: Vec<_> = (0..4).map(|s| picture(&cap.device, size, false, s, 16)).collect();
            let make = |bps| HwEncoder::new(&cap.device, cap.adapter_luid().ok(), size.0, size.1, 120, bps, false, Codec::H264).unwrap();
            let fresh = run(&mut make(5_000_000), &pics, 300);
            let mut enc = make(30_000_000);
            run(&mut enc, &pics, 120);
            enc.set_bitrate(5_000_000);
            let lowered = run(&mut enc, &pics, 300);
            println!("(largest frame B, Mbps) at 5 Mbps from the start {fresh:?}; lowered from 30 {lowered:?}");
            assert!(lowered.0 as f64 <= fresh.0 as f64 * 1.25, "frames at 5 Mbps burst past what a 5 Mbps encoder allows");
        }
    }

    /// The stream's NV12 ring slot `i`, read back (luma then chroma rows).
    unsafe fn read_nv12(cap: &DesktopCapture, i: usize) -> Vec<u8> {
        read_back(&cap.device, &cap.context, &cap.ring[i], cap.stream)
    }

    /// An NV12 texture of `(w, h)`, read back (luma then chroma rows).
    unsafe fn read_back(device: &ID3D11Device, context: &ID3D11DeviceContext, nv12: &ID3D11Texture2D, (w, h): (u32, u32)) -> Vec<u8> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
        let staging = staging.unwrap();
        context.CopyResource(&staging, nv12);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
        let mut out = Vec::with_capacity((w * h * 3 / 2) as usize);
        for row in 0..(h + h / 2) as usize {
            let p = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            out.extend_from_slice(core::slice::from_raw_parts(p, w as usize));
        }
        context.Unmap(&staging, 0);
        out
    }

    /// The video processor stands each rotation upright the way
    /// [`vp_rotation`] expects (the desktop's top-left corner, wherever the
    /// rotation put it in the surface, lands at the stream's top-left), and
    /// converting straight from a surface matches converting a copy of it.
    #[test]
    #[ignore]
    fn converter_turns_rotated_surfaces_upright() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
        unsafe {
            let cap = DesktopCapture::new(&primary(), None, 1).unwrap();
            let surface = (320u32, 192u32);
            for rotation in [
                DXGI_MODE_ROTATION_IDENTITY,
                DXGI_MODE_ROTATION_ROTATE90,
                DXGI_MODE_ROTATION_ROTATE180,
                DXGI_MODE_ROTATION_ROTATE270,
            ] {
                let stream = oriented(surface, rotation);
                // Where the desktop's top-left corner sits in the surface.
                let (sw, sh) = (surface.0 as usize, surface.1 as usize);
                let (cx, cy) = match rotation {
                    DXGI_MODE_ROTATION_ROTATE90 => (0, sh - 16),
                    DXGI_MODE_ROTATION_ROTATE180 => (sw - 16, sh - 16),
                    DXGI_MODE_ROTATION_ROTATE270 => (sw - 16, 0),
                    _ => (0, 0),
                };
                let mut bgra = vec![0u8; sw * sh * 4];
                for y in cy..cy + 16 {
                    for x in cx..cx + 16 {
                        bgra[(y * sw + x) * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
                    }
                }
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: surface.0,
                    Height: surface.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let init = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr() as *const c_void, SysMemPitch: surface.0 * 4, SysMemSlicePitch: 0 };
                let mut frame = None;
                cap.device.CreateTexture2D(&desc, Some(&init), Some(&mut frame)).unwrap();
                let frame = frame.unwrap();
                let ring: Vec<_> = (0..2).map(|_| texture(&cap.device, stream, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32).unwrap()).collect();
                let mut vp = Converter::new(&cap.device, &cap.context, surface, stream, rotation, &ring).unwrap();
                vp.convert(&cap.device, &cap.context, &frame, 0, surface, stream).unwrap();
                assert_eq!(vp.views.len(), 1, "converted straight from the surface");
                let copy = texture(&cap.device, surface, DXGI_FORMAT_B8G8R8A8_UNORM, desc.BindFlags).unwrap();
                cap.context.CopyResource(&copy, &frame);
                let copied = vp.view_of(&copy).unwrap();
                vp.blit(&copied, 1, surface, stream).unwrap();
                let direct = read_back(&cap.device, &cap.context, &ring[0], stream);
                assert!(direct == read_back(&cap.device, &cap.context, &ring[1], stream), "direct and copied conversions differ");
                let (w, h) = (stream.0 as usize, stream.1 as usize);
                let y_plane = &direct[..w * h];
                let luma = |x0: usize, y0: usize| (y0..y0 + 8).flat_map(|y| (x0..x0 + 8).map(move |x| y_plane[y * w + x] as u32)).sum::<u32>() / 64;
                let corners = [luma(4, 4), luma(w - 12, 4), luma(4, h - 12), luma(w - 12, h - 12)];
                println!("rotation {}: stream {stream:?}, corner luma (TL, TR, BL, BR) {corners:?}", rotation.0);
                assert!(corners[0] > 200 && corners[1..].iter().all(|&l| l < 40), "rotation {} isn't upright", rotation.0);
            }
        }
    }

    /// A capture of a monitor the driver shares takes its frames, not the
    /// duplication's (a stand-in driver on this PC's first monitor), in RGB
    /// and NV12 mode; and drops back to duplicating on another GPU's frames.
    #[test]
    #[ignore]
    fn capture_takes_the_driver_frames() {
        use driver_frames::tests::{first_pixel, FakeDriver};
        let m = list_monitors().unwrap().into_iter().next().expect("a monitor");
        let probe = DesktopCapture::new_rgb(&m.device_name, 2).unwrap();
        let (surface, luid) = (probe.surface, probe.adapter_luid().unwrap());
        let (device, context) = (probe.device.clone(), probe.context.clone());
        drop(probe);
        let prefix = format!(r"Local\rdpio-test-cap-{}-", std::process::id());
        *driver_frames::TEST_PREFIX.lock().unwrap() = Some(prefix.clone());
        driver_frames::register_monitor(&m.device_name, 8);
        unsafe {
            let fake = FakeDriver::new(&prefix, 8, &device, surface, 5, luid);
            let mut cap = DesktopCapture::new_rgb(&m.device_name, 2).unwrap();
            assert!(cap.from_driver() && cap.dup.is_none(), "no duplication alongside");
            fake.publish(&context, 0, [9, 99, 199, 255], surface);
            context.Flush();
            let f = cap.acquire(500).unwrap().expect("the driver's frame");
            assert_eq!(f.accumulated, 1);
            assert_eq!(first_pixel(&cap.device, &cap.context, &f.nv12), [9, 99, 199, 255]);
            assert!(cap.acquire(20).unwrap().is_none(), "nothing new");
            // Deferred: a frame seen isn't converted, and what's taken later is
            // the newest (one that came after the look, too).
            assert!(cap.defers());
            fake.publish(&context, 1, [1, 1, 1, 255], surface);
            context.Flush();
            assert!(cap.peek(500).unwrap().is_some(), "seen");
            fake.publish(&context, 2, [7, 77, 177, 255], surface);
            context.Flush();
            let f = cap.take().unwrap().expect("the newest frame");
            assert_eq!(first_pixel(&cap.device, &cap.context, &f.nv12), [7, 77, 177, 255]);
            assert!(cap.take().unwrap().is_none(), "nothing left to take");
            drop(cap);

            let mut cap = DesktopCapture::new(&m.device_name, None, 2).unwrap();
            assert!(cap.from_driver());
            fake.publish(&context, 1, [0, 0, 0, 255], surface);
            context.Flush();
            assert!(cap.acquire(500).unwrap().is_some(), "converted to NV12");
            drop(fake);

            let fake = FakeDriver::new(&prefix, 9, &device, surface, 6, luid + 1);
            driver_frames::register_monitor(&m.device_name, 9);
            let mut cap = DesktopCapture::new_rgb(&m.device_name, 2).unwrap();
            fake.publish(&context, 0, [1, 2, 3, 255], surface);
            context.Flush();
            assert!(cap.acquire(500).unwrap().is_none());
            assert!(!cap.from_driver(), "another GPU's frames: back to duplicating");
        }
        driver_frames::unregister_monitor(&m.device_name);
        *driver_frames::TEST_PREFIX.lock().unwrap() = None;
    }

    /// Acquire a frame and convert it twice: straight from the duplicated
    /// surface into ring slot 0, and from a copy of it into slot 1.
    unsafe fn acquire_twice(cap: &mut DesktopCapture) -> bool {
        let dup = cap.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        if dup.AcquireNextFrame(50, &mut info, &mut resource).is_err() {
            return false;
        }
        let got = info.LastPresentTime != 0 && resource.is_some();
        if got {
            let frame: ID3D11Texture2D = resource.unwrap().cast().unwrap();
            let vp = cap.vp.as_mut().unwrap();
            let direct = vp.view_of(&frame).unwrap();
            vp.blit(&direct, 0, cap.surface, cap.stream).unwrap();
            let bind = D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
            let copy = texture(&cap.device, cap.surface, DXGI_FORMAT_B8G8R8A8_UNORM, bind).unwrap();
            cap.context.CopyResource(&copy, &frame);
            let copied = vp.view_of(&copy).unwrap();
            vp.blit(&copied, 1, cap.surface, cap.stream).unwrap();
        }
        let _ = dup.ReleaseFrame();
        got
    }

    /// Invert a 4×4 block at the monitor's top-left corner (doing it twice
    /// restores it), then convert the next frame both ways: (straight from the
    /// surface, from a copy of it), both read back.
    unsafe fn flip_and_grab(cap: &mut DesktopCapture, m: &MonitorInfo) -> Option<(Vec<u8>, Vec<u8>)> {
        use windows::Win32::Graphics::Gdi::{GetDC, PatBlt, ReleaseDC, DSTINVERT};
        let dc = GetDC(None);
        let _ = PatBlt(dc, m.left + 2, m.top + 2, 4, 4, DSTINVERT);
        ReleaseDC(None, dc);
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(1) {
            if acquire_twice(cap) {
                return Some((read_nv12(cap, 0), read_nv12(cap, 1)));
            }
        }
        None
    }

    /// Converting straight from the duplicated surface matches converting a
    /// copy of it, and every monitor comes out upright and at its real size:
    /// the block flipped at the desktop's top-left corner changes the
    /// stream's top-left corner and no other.
    #[test]
    #[ignore]
    fn capture_converts_exactly_and_upright() {
        for m in list_monitors().unwrap() {
            unsafe {
                let mut cap = DesktopCapture::new(&m.device_name, None, 2).unwrap();
                println!(
                    "{}: listed {}x{}, desktop {:?}, surface {:?}, rotation {}, stream {:?}",
                    m.device_name, m.width, m.height, cap.desktop, cap.surface, cap.rotation.0, cap.stream
                );
                assert_eq!(cap.desktop, (m.width, m.height));
                let flipped = flip_and_grab(&mut cap, &m);
                let restored = flip_and_grab(&mut cap, &m);
                let (Some((a, a_copied)), Some((b, b_copied))) = (flipped, restored) else {
                    println!("  no frames came (display off?)");
                    continue;
                };
                assert!(a == a_copied && b == b_copied, "converting from the duplicated surface differs from converting a copy");
                let (w, h) = (cap.stream.0 as usize, cap.stream.1 as usize);
                let corner = |x0: usize, y0: usize| {
                    (y0..y0 + 32).flat_map(|y| (x0..x0 + 32).map(move |x| y * w + x)).filter(|&i| a[i].abs_diff(b[i]) > 8).count()
                };
                let corners = [corner(0, 0), corner(w - 32, 0), corner(0, h - 32), corner(w - 32, h - 32)];
                println!("  changed pixels per corner (TL, TR, BL, BR): {corners:?}");
                assert!(corners[0] > 0, "the flipped block isn't at the top-left");
                assert_eq!(corners[1..], [0, 0, 0], "the flipped block showed up in another corner");
            }
        }
    }
}
