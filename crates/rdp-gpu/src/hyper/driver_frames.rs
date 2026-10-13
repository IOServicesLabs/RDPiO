//! Frames straight from the rdpio Display driver (`RDPIO_FRAMES` in
//! drivers/rdpio-display/src/Public.h — keep both in step).
//!
//! For a virtual monitor of ours, the driver copies each frame the compositor
//! hands it into a ring of shared GPU textures and says in a shared section
//! which one is newest. The capture takes it from there instead of through
//! Desktop Duplication: no duplication surface or its pacing, frames from
//! every desktop (the secure one too), and the pointer beside the picture
//! (the driver's hardware cursor) rather than in it.

use std::sync::atomic::{fence, AtomicI32, Ordering};
use std::sync::Mutex;

use windows::core::{Error, Interface, Result as WinResult, HSTRING};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, POINT, S_OK, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Device1, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    IDXGIKeyedMutex, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS};
use windows::Win32::System::Threading::{OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE};

pub const VERSION: u32 = 1;
pub const SLOTS: usize = 3;
pub const NO_SLOT: u32 = u32::MAX;
pub const CURSOR_MAX: usize = 256;
/// Where the driver names a monitor's objects (it runs in session 0).
pub const DRIVER_PREFIX: &str = r"Global\rdpio-display-";

/// `RDPIO_FRAMES`.
#[repr(C)]
pub struct Header {
    pub version: u32,
    pub generation: u32,
    pub frame_lock: AtomicI32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub adapter_low: u32,
    pub adapter_high: i32,
    pub slot: u32,
    pub reserved: u32,
    pub frame_number: u64,
    pub present_qpc: u64,
    pub cursor_lock: AtomicI32,
    pub hardware_cursor: u32,
    pub cursor_visible: u32,
    pub shape_id: u32,
    pub shape_type: u32,
    pub shape_width: u32,
    pub shape_height: u32,
    pub shape_pitch: u32,
    pub x_hot: u32,
    pub y_hot: u32,
    pub shape: [u8; CURSOR_MAX * CURSOR_MAX * 4],
}

/// `IDDCX_CURSOR_SHAPE_TYPE`.
const SHAPE_MASKED_COLOR: u32 = 1;
const SHAPE_ALPHA: u32 = 2;

/// The frame fields, read together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameState {
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// The textures' GPU, as [`super::DesktopCapture::adapter_luid`] gives it.
    pub adapter: i64,
    pub slot: u32,
    pub frame_number: u64,
    pub present_qpc: u64,
}

/// A frame to take.
pub struct NextFrame {
    pub state: FrameState,
    pub texture: ID3D11Texture2D,
    pub mutex: IDXGIKeyedMutex,
    /// Frames the driver published since the last one taken (>1: some were
    /// never encoded).
    pub published: u32,
    /// The textures were opened anew: views of the old ones are stale.
    pub reopened: bool,
}

/// A pointer change.
pub enum CursorChange {
    /// `false` with no hardware cursor too: the pointer is in the frames, so
    /// the client shouldn't draw its own.
    Visible(bool),
    Shape(DXGI_OUTDUPL_POINTER_SHAPE_INFO, Vec<u8>),
}

// ------------------------------------------------------------ which outputs

/// The GDI device names of our virtual monitors and their driver ids, which
/// the agent records when it shows one.
static MONITORS: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

/// `device_name` (`\\.\DISPLAYn`) is the driver's monitor `id`: captures of it
/// take the driver's frames.
pub fn register_monitor(device_name: &str, id: u32) {
    let mut m = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|(n, _)| !n.eq_ignore_ascii_case(device_name));
    m.push((device_name.to_owned(), id));
}

pub fn unregister_monitor(device_name: &str) {
    MONITORS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(n, _)| !n.eq_ignore_ascii_case(device_name));
}

/// The driver's frames for `device_name`, if it's one of its monitors and
/// they're on offer. `RDPIO_DRIVER_FRAMES=0` turns this off (to compare).
pub fn for_output(device_name: &str) -> Option<DriverFrames> {
    if std::env::var("RDPIO_DRIVER_FRAMES").is_ok_and(|v| v == "0") {
        return None;
    }
    let id = MONITORS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(n, _)| n.eq_ignore_ascii_case(device_name))?.1;
    #[cfg(test)]
    let prefix = TEST_PREFIX.lock().unwrap().clone().unwrap_or_else(|| DRIVER_PREFIX.to_owned());
    #[cfg(not(test))]
    let prefix = DRIVER_PREFIX;
    match DriverFrames::open(&prefix, id) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::info!(monitor = id, error = %e, "the driver doesn't share this monitor's frames; duplicating the output");
            None
        }
    }
}

/// Tests stand in for the driver in the session namespace (`Global\` needs
/// a service's privilege).
#[cfg(test)]
pub(crate) static TEST_PREFIX: Mutex<Option<String>> = Mutex::new(None);

// ------------------------------------------------------------ the reader

pub struct DriverFrames {
    prefix: String,
    id: u32,
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    ready: HANDLE,
    generation: u32,
    slots: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
    last_frame: u64,
    hardware_cursor: Option<bool>,
    visible: Option<bool>,
    shape_id: Option<u32>,
}

// The handles and the mapping are plain process-wide resources.
unsafe impl Send for DriverFrames {}

impl DriverFrames {
    /// Open monitor `id`'s section and event, named `{prefix}{id}…`.
    pub fn open(prefix: &str, id: u32) -> WinResult<Self> {
        unsafe {
            let mapping = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(format!("{prefix}{id}")))?;
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, core::mem::size_of::<Header>());
            if view.Value.is_null() {
                let e = Error::from_thread();
                let _ = CloseHandle(mapping);
                return Err(e);
            }
            let ready = match OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(format!("{prefix}{id}-ready"))) {
                Ok(h) => h,
                Err(e) => {
                    let _ = UnmapViewOfFile(view);
                    let _ = CloseHandle(mapping);
                    return Err(e);
                }
            };
            let me = Self {
                prefix: prefix.to_owned(),
                id,
                mapping,
                view,
                ready,
                generation: 0,
                slots: Vec::new(),
                last_frame: 0,
                hardware_cursor: None,
                visible: None,
                shape_id: None,
            };
            let version = core::ptr::read_volatile(core::ptr::addr_of!((*me.header()).version));
            if version != VERSION {
                return Err(Error::new(E_FAIL, format!("the driver shares frames as version {version}, not {VERSION}")));
            }
            Ok(me)
        }
    }

    fn header(&self) -> *const Header {
        self.view.Value as *const Header
    }

    /// Wait up to `ms` for a frame or pointer change.
    pub fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.ready, ms) == WAIT_OBJECT_0 }
    }

    pub fn state(&self) -> Option<FrameState> {
        let h = self.header();
        unsafe {
            read_locked(&(*h).frame_lock, || FrameState {
                generation: vol(core::ptr::addr_of!((*h).generation)),
                width: vol(core::ptr::addr_of!((*h).width)),
                height: vol(core::ptr::addr_of!((*h).height)),
                format: vol(core::ptr::addr_of!((*h).format)),
                adapter: ((vol(core::ptr::addr_of!((*h).adapter_high)) as i64) << 32)
                    | vol(core::ptr::addr_of!((*h).adapter_low)) as i64,
                slot: vol(core::ptr::addr_of!((*h).slot)),
                frame_number: vol(core::ptr::addr_of!((*h).frame_number)),
                present_qpc: vol(core::ptr::addr_of!((*h).present_qpc)),
            })
        }
    }

    /// The newest frame, if there's one not taken yet. Open on `device`,
    /// which must be on the frame's GPU (`state.adapter`) — check before
    /// using it. Lock its `mutex` with [`lock`] while reading it.
    pub fn next_frame(&mut self, device: &ID3D11Device) -> WinResult<Option<NextFrame>> {
        let Some(state) = self.state() else {
            return Ok(None);
        };
        if state.generation == 0 || state.slot as usize >= SLOTS || state.frame_number == self.last_frame {
            return Ok(None);
        }
        let reopened = state.generation != self.generation;
        if reopened {
            self.slots.clear();
            self.generation = 0;
            let device1: ID3D11Device1 = device.cast()?;
            for i in 0..SLOTS {
                let name = HSTRING::from(format!("{}{}-{}-{}", self.prefix, self.id, state.generation, i));
                let texture: ID3D11Texture2D = unsafe {
                    device1.OpenSharedResourceByName(&name, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0)?
                };
                let mutex: IDXGIKeyedMutex = texture.cast()?;
                self.slots.push((texture, mutex));
            }
            self.generation = state.generation;
        }
        let published = if self.last_frame == 0 { 1 } else { state.frame_number.saturating_sub(self.last_frame).min(u32::MAX as u64) as u32 };
        self.last_frame = state.frame_number;
        let (texture, mutex) = self.slots[state.slot as usize].clone();
        Ok(Some(NextFrame { state, texture, mutex, published, reopened }))
    }

    /// What changed about the pointer since the last call.
    pub fn cursor_changes(&mut self) -> Vec<CursorChange> {
        let h = self.header();
        let snap = unsafe {
            read_locked(&(*h).cursor_lock, || {
                (
                    vol(core::ptr::addr_of!((*h).hardware_cursor)) != 0,
                    vol(core::ptr::addr_of!((*h).cursor_visible)) != 0,
                    vol(core::ptr::addr_of!((*h).shape_id)),
                )
            })
        };
        let Some((hardware, visible, shape_id)) = snap else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if self.hardware_cursor != Some(hardware) {
            self.hardware_cursor = Some(hardware);
            if !hardware {
                tracing::info!("the driver has no hardware cursor: the pointer is in the frames");
                self.visible = Some(false);
                out.push(CursorChange::Visible(false));
            }
        }
        if !hardware {
            return out;
        }
        if shape_id != 0 && self.shape_id != Some(shape_id) {
            if let Some(shape) = self.read_shape() {
                self.shape_id = Some(shape_id);
                out.push(shape);
            }
        }
        if self.visible != Some(visible) {
            self.visible = Some(visible);
            out.push(CursorChange::Visible(visible));
        }
        out
    }

    fn read_shape(&self) -> Option<CursorChange> {
        let h = self.header();
        unsafe {
            read_locked(&(*h).cursor_lock, || {
                let (w, height, pitch) = (
                    vol(core::ptr::addr_of!((*h).shape_width)),
                    vol(core::ptr::addr_of!((*h).shape_height)),
                    vol(core::ptr::addr_of!((*h).shape_pitch)),
                );
                let kind = match vol(core::ptr::addr_of!((*h).shape_type)) {
                    SHAPE_MASKED_COLOR => DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32,
                    SHAPE_ALPHA => DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
                    _ => return None,
                };
                let len = pitch as usize * height as usize;
                if w as usize > CURSOR_MAX || height as usize > CURSOR_MAX || len > CURSOR_MAX * CURSOR_MAX * 4 {
                    return None;
                }
                let mut buf = vec![0u8; len];
                // Checked against the lock afterwards, like the fields.
                core::ptr::copy_nonoverlapping(core::ptr::addr_of!((*h).shape) as *const u8, buf.as_mut_ptr(), len);
                let info = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
                    Type: kind,
                    Width: w,
                    Height: height,
                    Pitch: pitch,
                    HotSpot: POINT {
                        x: vol(core::ptr::addr_of!((*h).x_hot)) as i32,
                        y: vol(core::ptr::addr_of!((*h).y_hot)) as i32,
                    },
                };
                Some(CursorChange::Shape(info, buf))
            })
            .flatten()
        }
    }
}

impl Drop for DriverFrames {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.ready);
        }
    }
}

/// Take a texture's keyed mutex (key 0), waiting up to `ms`. `AcquireSync`'s
/// timeout is a success code, which the wrapper would report as `Ok`.
pub fn lock(mutex: &IDXGIKeyedMutex, ms: u32) -> bool {
    unsafe { (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), 0, ms) == S_OK }
}

unsafe fn vol<T: Copy>(p: *const T) -> T {
    core::ptr::read_volatile(p)
}

/// Read seqlock-guarded fields: retry while the writer is mid-change. `None`
/// if it seems stuck there (a writer that died mid-write).
fn read_locked<T>(lock: &AtomicI32, read: impl Fn() -> T) -> Option<T> {
    for _ in 0..100_000 {
        let before = lock.load(Ordering::Acquire);
        if before & 1 == 0 {
            let v = read();
            fence(Ordering::Acquire);
            if lock.load(Ordering::Relaxed) == before {
                return Some(v);
            }
        }
        std::hint::spin_loop();
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn header_matches_public_h() {
        assert_eq!(offset_of!(Header, frame_lock), 8);
        assert_eq!(offset_of!(Header, adapter_low), 24);
        assert_eq!(offset_of!(Header, slot), 32);
        assert_eq!(offset_of!(Header, frame_number), 40);
        assert_eq!(offset_of!(Header, present_qpc), 48);
        assert_eq!(offset_of!(Header, cursor_lock), 56);
        assert_eq!(offset_of!(Header, y_hot), 92);
        assert_eq!(offset_of!(Header, shape), 96);
        assert_eq!(size_of::<Header>(), 96 + CURSOR_MAX * CURSOR_MAX * 4);
    }

    #[test]
    fn a_stuck_writer_reads_as_none() {
        let lock = AtomicI32::new(1);
        assert_eq!(read_locked(&lock, || 5), None);
        lock.store(2, Ordering::Relaxed);
        assert_eq!(read_locked(&lock, || 5), Some(5));
    }

    #[test]
    fn only_registered_outputs_take_driver_frames() {
        register_monitor(r"\\.\DISPLAY77", 9);
        register_monitor(r"\\.\display77", 10);
        let id = |n: &str| MONITORS.lock().unwrap().iter().find(|(m, _)| m.eq_ignore_ascii_case(n)).map(|x| x.1);
        assert_eq!(id(r"\\.\DISPLAY77"), Some(10), "re-registering replaces");
        unregister_monitor(r"\\.\DISPLAY77");
        assert_eq!(id(r"\\.\DISPLAY77"), None);
        assert!(for_output(r"\\.\DISPLAY77").is_none());
    }

    /// The driver's side, as Driver.cpp's FrameShare does it, in-process.
    pub(crate) struct FakeDriver {
        section: HANDLE,
        header: *mut Header,
        ready: HANDLE,
        slots: Vec<(ID3D11Texture2D, IDXGIKeyedMutex, HANDLE)>,
    }

    impl FakeDriver {
        pub(crate) unsafe fn new(prefix: &str, id: u32, device: &ID3D11Device, size: (u32, u32), generation: u32, adapter: i64) -> Self {
            use windows::Win32::Graphics::Direct3D11::*;
            use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
            use windows::Win32::Graphics::Dxgi::IDXGIResource1;
            use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, PAGE_READWRITE};
            use windows::Win32::System::Threading::CreateEventW;
            let section = CreateFileMappingW(
                windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size_of::<Header>() as u32,
                &HSTRING::from(format!("{prefix}{id}")),
            )
            .unwrap();
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, size_of::<Header>());
            let header = view.Value as *mut Header;
            (*header).version = VERSION;
            (*header).slot = NO_SLOT;
            let ready = CreateEventW(None, false, false, &HSTRING::from(format!("{prefix}{id}-ready"))).unwrap();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
            };
            let mut slots = Vec::new();
            for i in 0..SLOTS {
                let mut t = None;
                device.CreateTexture2D(&desc, None, Some(&mut t)).unwrap();
                let t: ID3D11Texture2D = t.unwrap();
                let r: IDXGIResource1 = t.cast().unwrap();
                let name = HSTRING::from(format!("{prefix}{id}-{generation}-{i}"));
                let shared = r.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0, &name).unwrap();
                slots.push((t.clone(), t.cast().unwrap(), shared));
            }
            (*header).frame_lock.fetch_add(1, Ordering::SeqCst);
            (*header).generation = generation;
            (*header).width = size.0;
            (*header).height = size.1;
            (*header).format = DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32;
            (*header).adapter_low = adapter as u32;
            (*header).adapter_high = (adapter >> 32) as i32;
            (*header).frame_lock.fetch_add(1, Ordering::SeqCst);
            Self { section, header, ready, slots }
        }

        /// Fill slot `i` with one colour and publish it.
        pub(crate) unsafe fn publish(&self, context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext, i: usize, bgra: [u8; 4], size: (u32, u32)) {
            let (t, m, _) = &self.slots[i];
            assert!(lock(m, 1000));
            let px: Vec<u8> = bgra.iter().copied().cycle().take((size.0 * size.1 * 4) as usize).collect();
            context.UpdateSubresource(t, 0, None, px.as_ptr() as *const _, size.0 * 4, 0);
            m.ReleaseSync(0).unwrap();
            let h = &mut *self.header;
            h.frame_lock.fetch_add(1, Ordering::SeqCst);
            h.slot = i as u32;
            h.frame_number += 1;
            h.present_qpc = 1234;
            h.frame_lock.fetch_add(1, Ordering::SeqCst);
            windows::Win32::System::Threading::SetEvent(self.ready).unwrap();
        }

        unsafe fn cursor(&self, hardware: bool, visible: bool, shape: Option<(u32, [u8; 4])>) {
            let h = &mut *self.header;
            h.cursor_lock.fetch_add(1, Ordering::SeqCst);
            h.hardware_cursor = hardware as u32;
            h.cursor_visible = visible as u32;
            if let Some((id, bgra)) = shape {
                h.shape_id = id;
                h.shape_type = SHAPE_ALPHA;
                h.shape_width = 2;
                h.shape_height = 2;
                h.shape_pitch = 8;
                h.x_hot = 1;
                h.y_hot = 0;
                for p in 0..4 {
                    h.shape[p * 4..p * 4 + 4].copy_from_slice(&bgra);
                }
            }
            h.cursor_lock.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for FakeDriver {
        fn drop(&mut self) {
            unsafe {
                for (_, _, h) in &self.slots {
                    let _ = CloseHandle(*h);
                }
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.header as *mut _ });
                let _ = CloseHandle(self.section);
                let _ = CloseHandle(self.ready);
            }
        }
    }

    /// The first pixel of `texture`, read back through a staging copy.
    pub(crate) unsafe fn first_pixel(device: &ID3D11Device, context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext, texture: &ID3D11Texture2D) -> [u8; 4] {
        use windows::Win32::Graphics::Direct3D11::*;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut s = None;
        device.CreateTexture2D(&desc, None, Some(&mut s)).unwrap();
        let s: ID3D11Texture2D = s.unwrap();
        context.CopyResource(&s, texture);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&s, 0, D3D11_MAP_READ, 0, Some(&mut map)).unwrap();
        let p = core::slice::from_raw_parts(map.pData as *const u8, 4);
        let out = [p[0], p[1], p[2], p[3]];
        context.Unmap(&s, 0);
        out
    }

    /// Two devices on one GPU, like the driver host and the agent: frames
    /// and the pointer cross through the named objects.
    #[test]
    fn frames_and_pointer_cross_from_the_driver() {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        let make = || unsafe {
            let (mut d, mut c) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d),
                None,
                Some(&mut c),
            )
            .map(|()| (d.unwrap(), c.unwrap()))
        };
        let Ok((driver_dev, driver_ctx)) = make() else {
            eprintln!("no hardware D3D11 device; skipped");
            return;
        };
        let (agent_dev, agent_ctx) = make().unwrap();
        let prefix = format!(r"Local\rdpio-test-{}-", std::process::id());
        let size = (64, 32);
        unsafe {
            let fake = FakeDriver::new(&prefix, 3, &driver_dev, size, 41, 77);
            let mut frames = DriverFrames::open(&prefix, 3).expect("open");
            assert!(frames.next_frame(&agent_dev).unwrap().is_none(), "nothing published yet");

            fake.publish(&driver_ctx, 0, [10, 20, 30, 255], size);
            driver_ctx.Flush();
            assert!(frames.wait(1000));
            let f = frames.next_frame(&agent_dev).unwrap().expect("a frame");
            assert_eq!((f.state.width, f.state.height, f.state.adapter, f.state.slot), (64, 32, 77, 0));
            assert!(f.reopened);
            assert_eq!(f.published, 1);
            assert!(lock(&f.mutex, 1000));
            assert_eq!(first_pixel(&agent_dev, &agent_ctx, &f.texture), [10, 20, 30, 255]);
            f.mutex.ReleaseSync(0).unwrap();
            assert!(frames.next_frame(&agent_dev).unwrap().is_none(), "taken already");

            // Two more before the agent looks: it gets the newest, and knows
            // one went by.
            fake.publish(&driver_ctx, 1, [1, 1, 1, 255], size);
            fake.publish(&driver_ctx, 2, [50, 60, 70, 255], size);
            driver_ctx.Flush();
            let f = frames.next_frame(&agent_dev).unwrap().expect("a frame");
            assert_eq!((f.state.slot, f.published, f.reopened), (2, 2, false));
            assert!(lock(&f.mutex, 1000));
            assert_eq!(first_pixel(&agent_dev, &agent_ctx, &f.texture), [50, 60, 70, 255]);
            f.mutex.ReleaseSync(0).unwrap();

            // The pointer: no hardware cursor means it's in the frames.
            fake.cursor(false, true, None);
            assert!(matches!(frames.cursor_changes().as_slice(), [CursorChange::Visible(false)]));
            fake.cursor(true, true, Some((5, [0, 0, 255, 255])));
            let changes = frames.cursor_changes();
            assert_eq!(changes.len(), 2, "shape, then visible");
            match &changes[0] {
                CursorChange::Shape(info, buf) => {
                    assert_eq!((info.Width, info.Height, info.HotSpot.x, info.Type), (2, 2, 1, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32));
                    assert_eq!(&buf[..4], &[0, 0, 255, 255]);
                }
                _ => panic!("expected the shape"),
            }
            assert!(matches!(changes[1], CursorChange::Visible(true)));
            assert!(frames.cursor_changes().is_empty(), "nothing new");
            fake.cursor(true, false, None);
            assert!(matches!(frames.cursor_changes().as_slice(), [CursorChange::Visible(false)]));
        }
    }
}
