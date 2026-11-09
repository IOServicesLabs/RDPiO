//! Teams screen sharing through Microsoft's WebRTC add-in.
//!
//! The add-in shares what the session shows: it asks its host for the screen
//! capture service (the Windows App's, in no SDK; slots read from the add-in and
//! `rdclientax.dll`) and is handed the picture as a D3D11 texture. Without the
//! service, which Teams can require, sharing fails outright.
//!
//! The texture is on a device of its own, because the add-in uses that device's
//! context on whichever thread hands it over. Frames come from the renderer's
//! framebuffer, read back on the UI thread and only while a share is on.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{interface, IUnknown, IUnknown_Vtbl, Interface, Result, GUID, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE, E_NOTIMPL, E_POINTER, HMODULE, HWND, LPARAM, POINT, RECT, S_OK, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_NULL};

use crate::teams_slimcore;

/// `GetService` GUID of the screen capture service, also one of its IIDs.
pub const SCREEN_CAPTURE_SERVICE: GUID = GUID::from_u128(0xba8f38a3_80a1_41ea_99e0_24aec94cea43);
const SCREEN_CAPTURE_SERVICE_2: GUID = GUID::from_u128(0xd2be80a8_6840_47be_aba1_1ba901acaea6);
const SCREEN_CAPTURE_MOUSE: GUID = GUID::from_u128(0xbd1092e9_ba68_4e92_a5ba_368c4f801e5b);

/// 15 frames a second at most: a shared screen is mostly text.
const FRAME_GAP: Duration = Duration::from_millis(66);
/// How long the add-in's preview (start, capture, stop) waits for a real picture.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(200);
/// A still screen is handed over again this often, for whoever just joined.
const STILL_REPEAT: Duration = Duration::from_secs(1);

#[interface("d2b399d1-61c9-429f-8271-b2a3181fcbfc")]
unsafe trait ICaptureCallback: IUnknown {
    unsafe fn OnTerminated(&self, reason: i32) -> HRESULT;
    unsafe fn OnSurfaceBoundsChanged(&self, rect: *const RECT) -> HRESULT;
}

#[interface("7b7a44e2-3220-418d-b3bf-bc3217570b3f")]
unsafe trait ISurfaceConsumer: IUnknown {
    unsafe fn Unused(&self) -> HRESULT;
    unsafe fn AddDirtySurface(&self, rect: *const RECT, texture: *mut c_void) -> HRESULT;
}

struct Frame {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

/// A running share's way back into the add-in.
struct Capture {
    callback: ICaptureCallback,
    consumer: ISurfaceConsumer,
    stop: AtomicBool,
}

// SAFETY: the add-in calls these from its own threads and takes its own locks.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

static SHARING: AtomicBool = AtomicBool::new(false);
static FIRST_FRAME_WANTED: AtomicBool = AtomicBool::new(false);
static PICTURE_CHANGED: AtomicBool = AtomicBool::new(false);
static LAST_FRAME: Mutex<Option<Instant>> = Mutex::new(None);
static NEXT_FRAME: Mutex<Option<Frame>> = Mutex::new(None);
static FRAME_ARRIVED: Condvar = Condvar::new();
static ACTIVE: Mutex<Option<Arc<Capture>>> = Mutex::new(None);

/// Whether a share is on: the UI thread then wakes often enough to send frames.
pub fn is_sharing() -> bool {
    SHARING.load(Ordering::Relaxed)
}

/// The session's picture changed.
pub fn picture_changed() {
    if is_sharing() {
        PICTURE_CHANGED.store(true, Ordering::Relaxed);
    }
}

/// Whether to read the framebuffer back now, for [`offer_frame`].
pub fn frame_due() -> bool {
    if !is_sharing() {
        return false;
    }
    let first = FIRST_FRAME_WANTED.swap(false, Ordering::Relaxed);
    if !first && !PICTURE_CHANGED.load(Ordering::Relaxed) {
        return false;
    }
    let Ok(mut last) = LAST_FRAME.lock() else { return false };
    if !first && last.is_some_and(|t| t.elapsed() < FRAME_GAP) {
        return false;
    }
    *last = Some(Instant::now());
    PICTURE_CHANGED.store(false, Ordering::Relaxed);
    true
}

/// The framebuffer, tightly packed RGBA. An unsent frame is replaced.
pub fn offer_frame(w: u32, h: u32, rgba: Vec<u8>) {
    if let Ok(mut next) = NEXT_FRAME.lock() {
        *next = Some(Frame { w, h, rgba });
        FRAME_ARRIVED.notify_all();
    }
}

/// The session ended: stop without telling the add-in, which is going too.
pub fn end() {
    if let Some(capture) = take_active() {
        tracing::info!("Teams screen share ended with the session");
        drop(capture);
    }
}

fn take_active() -> Option<Arc<Capture>> {
    SHARING.store(false, Ordering::Relaxed);
    let capture = ACTIVE.lock().ok()?.take()?;
    // Under the lock the worker waits on, so it can't miss this.
    let _guard = NEXT_FRAME.lock();
    capture.stop.store(true, Ordering::SeqCst);
    FRAME_ARRIVED.notify_all();
    Some(capture)
}

/// The texture the add-in reads, as big as the picture last handed over.
struct Surface {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    texture: Option<ID3D11Texture2D>,
    size: (u32, u32),
}

// SAFETY: moved to the worker when `begin` is done with it; never used by two at once.
unsafe impl Send for Surface {}

impl Surface {
    fn new() -> Result<Self> {
        let (device, context) = create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        Ok(Self { device, context, texture: None, size: (0, 0) })
    }

    fn rect(&self) -> RECT {
        RECT { left: 0, top: 0, right: self.size.0 as i32, bottom: self.size.1 as i32 }
    }

    /// Put `frame` in the texture and hand it over. A new size goes first: the
    /// add-in takes the next dirty rectangle as the whole picture.
    fn show(&mut self, frame: &Frame, capture: &Capture) -> Result<()> {
        let resized = (frame.w, frame.h) != self.size;
        if resized {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: frame.w,
                Height: frame.h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut texture = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
            self.texture = texture;
            self.size = (frame.w, frame.h);
        }
        let texture = self.texture.as_ref().ok_or(windows::core::Error::from(E_FAIL))?;
        let bgra = to_bgra(&frame.rgba);
        unsafe { self.context.UpdateSubresource(texture, 0, None, bgra.as_ptr().cast(), frame.w * 4, 0) };
        if resized {
            tracing::info!(w = frame.w, h = frame.h, "Teams screen share: picture size");
            let rect = self.rect();
            let _ = unsafe { capture.callback.OnSurfaceBoundsChanged(&rect) };
        }
        self.hand_over(capture);
        Ok(())
    }

    fn hand_over(&self, capture: &Capture) {
        let Some(texture) = &self.texture else { return };
        let rect = self.rect();
        let hr = unsafe { capture.consumer.AddDirtySurface(&rect, texture.as_raw()) };
        if hr.is_err() {
            tracing::debug!(hr = hr.0, "Teams screen share: frame refused");
        }
    }
}

fn create_device(driver: D3D_DRIVER_TYPE) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }?;
    let device: ID3D11Device = device.ok_or(windows::core::Error::from(E_FAIL))?;
    if let Ok(protect) = device.cast::<ID3D11Multithread>() {
        let _ = unsafe { protect.SetMultithreadProtected(true) };
    }
    Ok((device, context.ok_or(windows::core::Error::from(E_FAIL))?))
}

fn to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = rgba.to_vec();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    bgra
}

fn desktop_size() -> (u32, u32) {
    match teams_slimcore::current() {
        info if info.desktop_size().0 > 0 && info.desktop_size().1 > 0 => info.desktop_size(),
        _ => (1280, 720),
    }
}

fn begin(callback: ICaptureCallback, consumer: ISurfaceConsumer) -> Result<()> {
    if take_active().is_some() {
        tracing::info!("Teams screen share: a new share replaces one still running");
    }
    let mut surface = Surface::new()?;
    let capture = Arc::new(Capture { callback, consumer, stop: AtomicBool::new(false) });

    // Ask the UI thread for the picture now, as the add-in may want it before
    // this returns; a blank one if the UI thread is busy.
    if let Ok(mut next) = NEXT_FRAME.lock() {
        *next = None;
    }
    FIRST_FRAME_WANTED.store(true, Ordering::Relaxed);
    SHARING.store(true, Ordering::Relaxed);
    let hwnd = teams_slimcore::current().hwnd();
    if hwnd != 0 {
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), WM_NULL, WPARAM(0), LPARAM(0)) };
    }
    let first = NEXT_FRAME
        .lock()
        .ok()
        .and_then(|next| FRAME_ARRIVED.wait_timeout_while(next, FIRST_FRAME_WAIT, |f| f.is_none()).ok())
        .and_then(|(mut next, _)| next.take());
    let first = first.unwrap_or_else(|| {
        let (w, h) = desktop_size();
        Frame { w, h, rgba: vec![0; (w * h * 4) as usize] }
    });
    if let Err(e) = surface.show(&first, &capture) {
        SHARING.store(false, Ordering::Relaxed);
        return Err(e);
    }

    if let Ok(mut active) = ACTIVE.lock() {
        *active = Some(capture.clone());
    }
    std::thread::Builder::new()
        .name("rdpio-teams-share".into())
        .spawn(move || run_share(surface, capture))
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
    Ok(())
}

fn run_share(mut surface: Surface, capture: Arc<Capture>) {
    loop {
        let Ok(next) = NEXT_FRAME.lock() else { return };
        let Ok((mut next, _)) = FRAME_ARRIVED
            .wait_timeout_while(next, STILL_REPEAT, |f| f.is_none() && !capture.stop.load(Ordering::SeqCst))
        else {
            return;
        };
        let frame = next.take();
        drop(next);
        if capture.stop.load(Ordering::SeqCst) {
            return;
        }
        match frame {
            Some(frame) => {
                if let Err(e) = surface.show(&frame, &capture) {
                    tracing::warn!(error = %e, "Teams screen share: couldn't update the picture");
                }
            }
            None => surface.hand_over(&capture),
        }
    }
}

// The service itself: one object for three IIDs, which the add-in expects to
// behave as one. A stateless singleton like `webrtc_addin`'s stub, as there's one
// session a process.

#[repr(C)]
struct ServiceVtbl {
    query_interface: unsafe extern "system" fn(*mut Service, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Service) -> u32,
    release: unsafe extern "system" fn(*mut Service) -> u32,
    unused: [unsafe extern "system" fn(*mut Service) -> HRESULT; 2],
    start: unsafe extern "system" fn(*mut Service, u32, u32, u32, *mut c_void, *mut c_void) -> HRESULT,
    stop: unsafe extern "system" fn(*mut Service) -> HRESULT,
    monitor_count: unsafe extern "system" fn(*mut Service, *mut u32) -> HRESULT,
    monitor_rect: unsafe extern "system" fn(*mut Service, u32, *mut RECT, i32) -> HRESULT,
    mouse_position: unsafe extern "system" fn(*mut Service, *mut i32, *mut i32) -> HRESULT,
    ignore_server_mouse_moves: unsafe extern "system" fn(*mut Service, i32) -> HRESULT,
}

#[repr(C)]
struct Service {
    vtbl: &'static ServiceVtbl,
}

static SERVICE_VTBL: ServiceVtbl = ServiceVtbl {
    query_interface,
    add_ref,
    release,
    unused: [not_implemented; 2],
    start,
    stop,
    monitor_count,
    monitor_rect,
    mouse_position,
    ignore_server_mouse_moves,
};
static SERVICE: Service = Service { vtbl: &SERVICE_VTBL };

// SAFETY: holds only a reference to a `'static` table.
unsafe impl Sync for Service {}

/// `GetService`'s answer.
pub fn service() -> *mut c_void {
    &SERVICE as *const Service as *mut c_void
}

unsafe extern "system" fn query_interface(this: *mut Service, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if riid.is_null() || out.is_null() {
        return E_POINTER;
    }
    let iid = unsafe { *riid };
    if [IUnknown::IID, SCREEN_CAPTURE_SERVICE, SCREEN_CAPTURE_SERVICE_2, SCREEN_CAPTURE_MOUSE].contains(&iid) {
        unsafe { *out = this.cast() };
        return S_OK;
    }
    unsafe { *out = core::ptr::null_mut() };
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(_: *mut Service) -> u32 {
    2
}

unsafe extern "system" fn release(_: *mut Service) -> u32 {
    1
}

unsafe extern "system" fn not_implemented(_: *mut Service) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn start(
    _: *mut Service,
    _monitor: u32,
    _border_width: u32,
    _border_color: u32,
    callback: *mut c_void,
    consumer: *mut c_void,
) -> HRESULT {
    let (Some(callback), Some(consumer)) =
        (unsafe { IUnknown::from_raw_borrowed(&callback) }, unsafe { IUnknown::from_raw_borrowed(&consumer) })
    else {
        return E_POINTER;
    };
    let started = callback
        .cast::<ICaptureCallback>()
        .and_then(|callback| Ok((callback, consumer.cast::<ISurfaceConsumer>()?)))
        .and_then(|(callback, consumer)| begin(callback, consumer));
    match started {
        Ok(()) => {
            tracing::info!("Teams screen share started");
            S_OK
        }
        Err(e) => {
            tracing::warn!(error = %e, "Teams screen share couldn't start");
            e.code()
        }
    }
}

unsafe extern "system" fn stop(_: *mut Service) -> HRESULT {
    if let Some(capture) = take_active() {
        tracing::info!("Teams screen share stopped");
        let _ = unsafe { capture.callback.OnTerminated(1) };
    }
    S_OK
}

unsafe extern "system" fn monitor_count(_: *mut Service, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    tracing::info!("Teams asked what it can share");
    unsafe { *out = 1 };
    S_OK
}

unsafe extern "system" fn monitor_rect(_: *mut Service, _monitor: u32, out: *mut RECT, _flag: i32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let (w, h) = desktop_size();
    unsafe { *out = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 } };
    S_OK
}

/// Where the pointer is on the remote desktop; an error shares no pointer.
unsafe extern "system" fn mouse_position(_: *mut Service, x: *mut i32, y: *mut i32) -> HRESULT {
    if x.is_null() || y.is_null() {
        return E_POINTER;
    }
    let info = teams_slimcore::current();
    let (Some((px, py)), (w, h)) = (cursor_in_client(info.hwnd()), info.client_size()) else {
        return E_FAIL;
    };
    let (dw, dh) = info.desktop_size();
    if w == 0 || h == 0 || px < 0 || py < 0 || px as u32 >= w || py as u32 >= h {
        return E_FAIL;
    }
    unsafe {
        *x = (i64::from(px) * i64::from(dw) / i64::from(w)) as i32;
        *y = (i64::from(py) * i64::from(dh) / i64::from(h)) as i32;
    }
    S_OK
}

fn cursor_in_client(hwnd: isize) -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point).ok()?;
        ScreenToClient(HWND(hwnd as *mut c_void), &mut point).ok().ok()?;
    }
    Some((point.x, point.y))
}

unsafe extern "system" fn ignore_server_mouse_moves(_: *mut Service, _ignore: i32) -> HRESULT {
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use windows::core::implement;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Resource, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
    };

    /// How the add-in sees the service: slots as its own calls use them.
    #[interface("d2be80a8-6840-47be-aba1-1ba901acaea6")]
    unsafe trait IAsSeenByTheAddIn: IUnknown {
        unsafe fn Unused3(&self) -> HRESULT;
        unsafe fn Unused4(&self) -> HRESULT;
        unsafe fn Start(&self, monitor: u32, a: u32, b: u32, callback: *mut c_void, consumer: *mut c_void) -> HRESULT;
        unsafe fn Stop(&self) -> HRESULT;
        unsafe fn MonitorCount(&self, out: *mut u32) -> HRESULT;
        unsafe fn MonitorRect(&self, monitor: u32, out: *mut RECT, flag: i32) -> HRESULT;
        unsafe fn MousePosition(&self, x: *mut i32, y: *mut i32) -> HRESULT;
        unsafe fn IgnoreServerMouseMoves(&self, ignore: i32) -> HRESULT;
    }

    #[derive(Default)]
    struct Seen {
        /// What happened, in order.
        calls: Mutex<Vec<String>>,
        terminated: AtomicU32,
    }

    #[implement(ICaptureCallback)]
    struct Callback(Arc<Seen>);

    impl ICaptureCallback_Impl for Callback_Impl {
        unsafe fn OnTerminated(&self, reason: i32) -> HRESULT {
            assert_eq!(reason, 1);
            self.0.terminated.fetch_add(1, Ordering::SeqCst);
            S_OK
        }
        unsafe fn OnSurfaceBoundsChanged(&self, rect: *const RECT) -> HRESULT {
            let r = unsafe { *rect };
            self.0.calls.lock().unwrap().push(format!("bounds {}x{}", r.right, r.bottom));
            S_OK
        }
    }

    #[implement(ISurfaceConsumer)]
    struct Consumer(Arc<Seen>);

    /// The texture's first pixel, read back on its own device as the add-in does.
    fn first_pixel(texture: &ID3D11Texture2D) -> [u8; 4] {
        unsafe {
            let device = texture.GetDevice().unwrap();
            let context = device.GetImmediateContext().unwrap();
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
            let staging: ID3D11Resource = staging.unwrap().cast().unwrap();
            context.CopyResource(&staging, &texture.cast::<ID3D11Resource>().unwrap());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let pixel = *(mapped.pData as *const [u8; 4]);
            context.Unmap(&staging, 0);
            pixel
        }
    }

    impl ISurfaceConsumer_Impl for Consumer_Impl {
        unsafe fn Unused(&self) -> HRESULT {
            S_OK
        }
        unsafe fn AddDirtySurface(&self, rect: *const RECT, texture: *mut c_void) -> HRESULT {
            let r = unsafe { *rect };
            let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture) }.unwrap();
            let [b, g, r_, a] = first_pixel(texture);
            self.0.calls.lock().unwrap().push(format!("dirty {}x{} {b},{g},{r_},{a}", r.right, r.bottom));
            S_OK
        }
    }

    fn wait_for(seen: &Seen, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !seen.calls.lock().unwrap().iter().any(|c| c == line) {
            assert!(Instant::now() < deadline, "no {line:?} in {:?}", seen.calls.lock().unwrap());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // One test, as a share is one per process.
    #[test]
    fn a_share_hands_the_picture_over_the_way_the_add_in_needs() {
        let unknown = unsafe { IUnknown::from_raw_borrowed(&service()) }.unwrap().clone();
        let as_seen: IAsSeenByTheAddIn = unknown.cast().unwrap();
        // One object for all of its IIDs.
        for iid in [IUnknown::IID, SCREEN_CAPTURE_SERVICE, SCREEN_CAPTURE_SERVICE_2, SCREEN_CAPTURE_MOUSE] {
            let mut out = core::ptr::null_mut();
            unsafe { unknown.query(&iid, &mut out) }.ok().unwrap();
            assert_eq!(out, service());
        }
        let mut out = core::ptr::null_mut();
        assert_eq!(unsafe { unknown.query(&GUID::from_u128(1), &mut out) }, E_NOINTERFACE);

        let mut count = 0;
        unsafe { as_seen.MonitorCount(&mut count) }.ok().unwrap();
        assert_eq!(count, 1);
        let mut rect = RECT::default();
        unsafe { as_seen.MonitorRect(0, &mut rect, 0) }.ok().unwrap();
        assert!(rect.right > 0 && rect.bottom > 0);
        unsafe { as_seen.IgnoreServerMouseMoves(1) }.ok().unwrap();

        assert!(!frame_due(), "nothing's wanted until a share starts");
        let seen = Arc::new(Seen::default());
        let callback: ICaptureCallback = Callback(seen.clone()).into();
        let consumer: ISurfaceConsumer = Consumer(seen.clone()).into();
        // No UI thread here: the first picture is a blank one, whole, before Start returns.
        unsafe { as_seen.Start(0, 5, 0xFF00_00FF, callback.as_raw(), consumer.as_raw()) }.ok().unwrap();
        {
            let calls = seen.calls.lock().unwrap();
            assert!(calls[0].starts_with("bounds "), "{calls:?}");
            assert!(calls[1].starts_with("dirty ") && calls[1].ends_with(" 0,0,0,0"), "{calls:?}");
        }
        assert!(is_sharing());
        assert!(frame_due(), "the UI thread is asked for the first picture at once");
        assert!(!frame_due(), "and nothing more until the picture changes");
        picture_changed();
        std::thread::sleep(FRAME_GAP);
        assert!(frame_due());
        picture_changed();
        assert!(!frame_due(), "at most so many frames a second");

        // A new size is announced first; colours arrive as BGRA.
        let rgba = [10u8, 20, 30, 255].repeat(4 * 2);
        offer_frame(4, 2, rgba);
        wait_for(&seen, "dirty 4x2 30,20,10,255");
        {
            let calls = seen.calls.lock().unwrap();
            let bounds = calls.iter().position(|c| c == "bounds 4x2").unwrap();
            let dirty = calls.iter().position(|c| c == "dirty 4x2 30,20,10,255").unwrap();
            assert!(bounds < dirty, "{calls:?}");
        }
        // Same size: no new bounds.
        offer_frame(4, 2, [1u8, 2, 3, 255].repeat(4 * 2));
        wait_for(&seen, "dirty 4x2 3,2,1,255");
        assert_eq!(seen.calls.lock().unwrap().iter().filter(|c| *c == "bounds 4x2").count(), 1);
        // A still screen is handed over again.
        let before = seen.calls.lock().unwrap().len();
        let deadline = Instant::now() + STILL_REPEAT * 3;
        while seen.calls.lock().unwrap().len() == before {
            assert!(Instant::now() < deadline, "a still screen wasn't repeated");
            std::thread::sleep(Duration::from_millis(20));
        }

        unsafe { as_seen.Stop() }.ok().unwrap();
        assert_eq!(seen.terminated.load(Ordering::SeqCst), 1);
        assert!(!is_sharing() && !frame_due());
        // Stopping twice, as the add-in's teardown may, tells it once.
        unsafe { as_seen.Stop() }.ok().unwrap();
        assert_eq!(seen.terminated.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rgba_turns_to_bgra() {
        assert_eq!(to_bgra(&[1, 2, 3, 4, 5, 6, 7, 8]), [3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
