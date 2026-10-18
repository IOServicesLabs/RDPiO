//! The laptop's camera on the host, over Hyper.
//!
//! RDP's camera redirection dies with every other RDP channel when a stream
//! starts, so Hyper carries the camera itself:
//!
//! - **Client** ([`CameraSender`]): says how many cameras it has
//!   (`Control::Cameras`); while the host asks (`Control::Camera { on }`) it
//!   captures the first one at 1280×720, 30 fps, H.264, and sends each frame
//!   as [`Kind::Camera`] shards (framed like the video, with parity). The
//!   camera — and its light — is on only while a host app uses it.
//! - **Host** ([`HostCamera`]): with a client camera, offers apps a virtual
//!   camera, "rdpio Camera" (Windows 11's `MFCreateVirtualCamera`). Its media
//!   source is `rdpio_vcam.dll` (built from `vcam/`, carried inside rdpio.exe,
//!   written under Program Files and registered for Windows' Frame Server);
//!   it reads the newest frame from shared memory, which the host fills with
//!   the decoded camera. When an app starts or stops the camera the source
//!   signals, and the host asks the client to send or stop.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use rdp_hyper::control::Control;
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

/// The media source, built by build.rs (empty where it couldn't be).
static DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rdpio_vcam.dll"));

/// Its COM class (vcam/rdpio_vcam.cpp's `CLSID_RdpioCamera`).
const CLSID: &str = "{9E8E8B5A-5C3B-4C7E-9B0F-5D2B7A1E6C31}";
const SECTION: &str = r"Global\rdpio-camera";
const WANTED_EVENT: &str = r"Global\rdpio-camera-wanted";

/// The camera's picture (both ends; the media source offers exactly this).
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
const FPS: u32 = 30;
const FRAME_BYTES: usize = (WIDTH * HEIGHT * 3 / 2) as usize;

/// The shared frame's header (vcam/rdpio_vcam.cpp's `Shared`; the NV12
/// frame follows it).
#[repr(C)]
struct Shared {
    magic: u32,
    version: u32,
    width: u32,
    height: u32,
    /// Streams running (the media source counts them).
    wanted: AtomicI32,
    /// Even: stable; odd: being written.
    seq: AtomicI32,
    frames: u64,
}
const MAGIC: u32 = 0x4D41_4352; // "RCAM"
const SHARED_SIZE: usize = std::mem::size_of::<Shared>() + FRAME_BYTES;
const _: () = assert!(std::mem::size_of::<Shared>() == 32);

// ------------------------------------------------------------------ client

/// How many cameras this PC can send (0 with `RDPIO_HYPER_CAMERA=0`).
pub fn camera_count() -> u8 {
    if std::env::var("RDPIO_HYPER_CAMERA").is_ok_and(|v| v.trim() == "0") {
        return 0;
    }
    crate::mf_camera::MfCamera::enumerate().len().min(u8::MAX as usize) as u8
}

/// The client's camera, captured and sent while the host asks for it.
pub struct CameraSender {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CameraSender {
    /// Start sending camera shard bodies through `send`; `clock` is the
    /// stream's (capture time, µs).
    pub fn start(send: Arc<dyn Fn(&[u8]) + Send + Sync>, clock: fn() -> u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let (s, k) = (stop.clone(), keyframe.clone());
        let thread = std::thread::Builder::new()
            .name("hyper-camera-tx".into())
            .spawn(move || send_loop(&send, clock, &s, &k))
            .ok();
        tracing::info!("host app is using the camera; sending this PC's camera");
        Self { stop, keyframe, thread }
    }

    /// The host lost frames: the next one a keyframe.
    pub fn keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for CameraSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        tracing::info!("camera released");
    }
}

fn send_loop(send: &Arc<dyn Fn(&[u8]) + Send + Sync>, clock: fn() -> u64, stop: &AtomicBool, keyframe: &AtomicBool) {
    crate::wasapi::com_init();
    use crate::mf_camera::MfCamera;
    use rdp_channels::camera::{CamFormat, MediaType};
    let media = MediaType { format: CamFormat::H264, width: WIDTH, height: HEIGHT, fps_num: FPS, fps_den: 1 };
    // Frames are produced as they're owed: a couple ahead, refilled as
    // they're taken, so none is ever dropped after encoding.
    let camera = MfCamera::start_on_demand(0, media);
    camera.request_frames(3);
    camera.request_keyframe();
    let mut frame_id = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if keyframe.swap(false, Ordering::Relaxed) {
            camera.request_keyframe();
        }
        let mut sent = 0;
        while let Some(frame) = camera.next_frame() {
            let out = rdp_hyper::video::OutFrame {
                frame_id,
                data: &frame,
                keyframe: has_idr(&frame),
                ltr: false,
                recovery: false,
                capture_us: clock(),
                encode_us: 0,
            };
            for body in rdp_hyper::video::packetize(&out, 0.1) {
                send(&body);
            }
            frame_id = frame_id.wrapping_add(1);
            sent += 1;
        }
        if sent > 0 {
            camera.request_frames(sent);
        }
        if camera.is_finished() {
            tracing::warn!("the camera stopped; not sending it");
            return;
        }
        // (A frame comes every 33 ms.)
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Whether Annex-B `h264` holds an IDR picture (NAL type 5).
fn has_idr(h264: &[u8]) -> bool {
    h264.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

// -------------------------------------------------------------------- host

/// The host's side: the virtual camera, fed from the client's.
pub struct HostCamera {
    stop: Arc<AtomicBool>,
    packets: mpsc::Sender<Vec<u8>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl HostCamera {
    /// Offer apps "rdpio Camera" (Windows 11; needs administrator rights,
    /// which the Hyper agent has). `control` sends to the client.
    pub fn start(control: Arc<dyn Fn(&Control) + Send + Sync>) -> Result<Self, String> {
        let vcam = Arc::new(VirtualCamera::create()?);
        let stop = Arc::new(AtomicBool::new(false));
        let (packets, rx) = mpsc::channel::<Vec<u8>>();
        let mut threads = Vec::new();
        {
            let (vcam, stop, control) = (vcam.clone(), stop.clone(), control.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("hyper-camera-wanted".into())
                    .spawn(move || watch_wanted(&vcam, &stop, &control))
                    .map_err(|e| e.to_string())?,
            );
        }
        {
            let (vcam, stop) = (vcam.clone(), stop.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("hyper-camera-rx".into())
                    .spawn(move || decode_loop(&vcam, &rx, &stop, &control))
                    .map_err(|e| e.to_string())?,
            );
        }
        tracing::info!("virtual camera 'rdpio Camera' offered to host apps (the client's camera)");
        Ok(Self { stop, packets, threads })
    }

    /// One [`rdp_hyper::wire::Kind::Camera`] packet body from the client.
    pub fn push(&self, body: &[u8]) {
        let _ = self.packets.send(body.to_vec());
    }
}

impl Drop for HostCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Tell the client to send its camera while an app uses ours.
fn watch_wanted(vcam: &VirtualCamera, stop: &AtomicBool, control: &Arc<dyn Fn(&Control) + Send + Sync>) {
    let mut on = false;
    let mut last_sent = Instant::now() - Duration::from_secs(10);
    while !stop.load(Ordering::Relaxed) {
        let now_on = vcam.wanted();
        if now_on != on || (on && last_sent.elapsed() >= Duration::from_secs(1)) {
            if now_on != on {
                tracing::info!(on = now_on, "host app camera use changed");
            }
            on = now_on;
            control(&Control::Camera { on });
            last_sent = Instant::now();
        }
        vcam.wait_wanted(1000);
    }
    if on {
        for _ in 0..3 {
            control(&Control::Camera { on: false });
        }
    }
}

/// Client camera shards → frames → decoded → the shared frame.
fn decode_loop(vcam: &VirtualCamera, rx: &mpsc::Receiver<Vec<u8>>, stop: &AtomicBool, control: &Arc<dyn Fn(&Control) + Send + Sync>) {
    use rdp_hyper::video::{Event, Reassembler};
    crate::wasapi::com_init();
    let mut reassembler = Reassembler::new();
    let mut decoder: Option<rdp_gpu::h264::H264Decoder> = None;
    let mut awaiting_key = true;
    let mut asked = Instant::now() - Duration::from_secs(1);
    let epoch = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let body = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        for ev in reassembler.push(&body, epoch.elapsed().as_micros() as u64) {
            match ev {
                Event::Lost { .. } => awaiting_key = true,
                Event::Frame(f) => {
                    if awaiting_key && !f.keyframe {
                        continue;
                    }
                    if decoder.is_none() {
                        match rdp_gpu::h264::H264Decoder::new(WIDTH, HEIGHT) {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                tracing::warn!(error = %e, "no H.264 decoder for the camera");
                                return;
                            }
                        }
                    }
                    match decoder.as_mut().unwrap().decode(&f.data) {
                        Ok(pictures) => {
                            awaiting_key = false;
                            for p in pictures {
                                if (p.width, p.height) == (WIDTH, HEIGHT) && p.nv12.len() >= FRAME_BYTES {
                                    vcam.write(&p.nv12[..FRAME_BYTES]);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "camera frame didn't decode");
                            awaiting_key = true;
                        }
                    }
                }
            }
        }
        if awaiting_key && asked.elapsed() >= Duration::from_millis(300) {
            control(&Control::CameraKeyframe);
            asked = Instant::now();
        }
    }
}

/// "rdpio Camera" and the frame it shows.
struct VirtualCamera {
    camera: windows::Win32::Media::MediaFoundation::IMFVirtualCamera,
    section: HANDLE,
    view: *mut Shared,
    wanted: HANDLE,
}

// SAFETY: the view is shared memory, written only under its seqlock (one
// writer: the decode thread) and otherwise read through atomics; the handles
// and the camera object (free-threaded MF) are used from any thread.
unsafe impl Send for VirtualCamera {}
unsafe impl Sync for VirtualCamera {}

impl VirtualCamera {
    fn create() -> Result<Self, String> {
        use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
        use windows::Win32::System::Threading::CreateEventW;
        if DLL.is_empty() {
            return Err("this build has no virtual camera (rdpio_vcam.dll wasn't built)".into());
        }
        crate::wasapi::com_init();
        let create = virtual_camera_api().ok_or("virtual cameras need Windows 11")?;
        let dll = deploy_dll()?;
        register(&dll)?;
        unsafe {
            // The Frame Server (Local Service, Local System) and
            // administrators only: it's the user's camera picture.
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from("D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)"),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(|e| format!("security descriptor: {e}"))?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let section = CreateFileMappingW(
                windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                SHARED_SIZE as u32,
                &HSTRING::from(SECTION),
            )
            .map_err(|e| format!("shared frame: {e}"))?;
            let wanted = CreateEventW(Some(&sa), false, false, &HSTRING::from(WANTED_EVENT));
            let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
            let wanted = match wanted {
                Ok(w) => w,
                Err(e) => {
                    let _ = CloseHandle(section);
                    return Err(format!("camera event: {e}"));
                }
            };
            let view = MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, SHARED_SIZE).Value as *mut Shared;
            if view.is_null() {
                let _ = CloseHandle(section);
                let _ = CloseHandle(wanted);
                return Err("shared frame: no view".into());
            }
            // Black until the client's first frame.
            let frame = std::slice::from_raw_parts_mut((view as *mut u8).add(std::mem::size_of::<Shared>()), FRAME_BYTES);
            frame[..(WIDTH * HEIGHT) as usize].fill(16);
            frame[(WIDTH * HEIGHT) as usize..].fill(128);
            (*view).width = WIDTH;
            (*view).height = HEIGHT;
            (*view).version = 1;
            (*view).magic = MAGIC;
            let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = create(
                0, // MFVirtualCameraType_SoftwareCameraSource
                0, // MFVirtualCameraLifetime_Session: gone with this object
                1, // MFVirtualCameraAccess_AllUsers
                PCWSTR(HSTRING::from("rdpio Camera").as_ptr()),
                PCWSTR(HSTRING::from(CLSID).as_ptr()),
                std::ptr::null(),
                0,
                &mut raw,
            );
            if hr.is_err() || raw.is_null() {
                let _ = windows::Win32::System::Memory::UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as _ });
                let _ = CloseHandle(section);
                let _ = CloseHandle(wanted);
                return Err(format!("MFCreateVirtualCamera: {hr:?}"));
            }
            let me = Self { camera: windows::Win32::Media::MediaFoundation::IMFVirtualCamera::from_raw(raw), section, view, wanted };
            // (On failure, dropping `me` puts everything back.)
            me.camera.Start(None).map_err(|e| format!("starting the virtual camera: {e}"))?;
            Ok(me)
        }
    }

    fn wanted(&self) -> bool {
        unsafe { (*self.view).wanted.load(Ordering::Acquire) > 0 }
    }

    fn wait_wanted(&self, ms: u32) {
        unsafe {
            windows::Win32::System::Threading::WaitForSingleObject(self.wanted, ms);
        }
    }

    /// The newest frame (NV12, `WIDTH`×`HEIGHT`). One writer.
    fn write(&self, nv12: &[u8]) {
        unsafe {
            let seq = &(*self.view).seq;
            let s = seq.load(Ordering::Relaxed);
            seq.store(s.wrapping_add(1), Ordering::Release);
            std::sync::atomic::fence(Ordering::Release);
            let dst = (self.view as *mut u8).add(std::mem::size_of::<Shared>());
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), dst, FRAME_BYTES.min(nv12.len()));
            (*self.view).frames = (*self.view).frames.wrapping_add(1);
            std::sync::atomic::fence(Ordering::Release);
            seq.store(s.wrapping_add(2), Ordering::Release);
        }
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        unsafe {
            let _ = self.camera.Shutdown();
            let _ = windows::Win32::System::Memory::UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ });
            let _ = CloseHandle(self.section);
            let _ = CloseHandle(self.wanted);
        }
        tracing::info!("virtual camera withdrawn");
    }
}

type CreateVirtualCamera = unsafe extern "system" fn(
    i32,
    i32,
    i32,
    PCWSTR,
    PCWSTR,
    *const windows::core::GUID,
    u32,
    *mut *mut core::ffi::c_void,
) -> windows::core::HRESULT;

/// `MFCreateVirtualCamera`, looked up at run time: on Windows 10 it doesn't
/// exist, and a link-time import would keep rdpio from starting there at all.
fn virtual_camera_api() -> Option<CreateVirtualCamera> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let module = LoadLibraryW(&HSTRING::from("mfsensorgroup.dll")).ok()?;
        let f = GetProcAddress(module, windows::core::s!("MFCreateVirtualCamera"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateVirtualCamera>(f))
    }
}

/// Write the media source where only administrators can change it (Frame
/// Server loads it as Local System): `%ProgramFiles%\rdpio\camera`, named by
/// its contents so a loaded old one is never overwritten.
fn deploy_dll() -> Result<std::path::PathBuf, String> {
    let dir = std::path::PathBuf::from(std::env::var_os("ProgramFiles").ok_or("no Program Files")?).join("rdpio").join("camera");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let hash = DLL.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let name = format!("rdpio_vcam-{:08x}.dll", hash as u32);
    let path = dir.join(&name);
    if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(DLL.len() as u64) {
        std::fs::write(&path, DLL).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // Older ones (Frame Server may still hold one: tried again next time).
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("rdpio_vcam-") && n != name {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(path)
}

/// Register the media source's COM class for every process (Frame Server
/// looks only in HKLM).
fn register(dll: &std::path::Path) -> Result<(), String> {
    use windows::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ};
    let key_path = format!(r"SOFTWARE\Classes\CLSID\{CLSID}\InprocServer32");
    unsafe {
        let mut key = HKEY::default();
        let r = RegCreateKeyExW(HKEY_LOCAL_MACHINE, &HSTRING::from(key_path), None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None);
        if r.is_err() {
            return Err(format!("registering the camera needs administrator rights ({r:?})"));
        }
        let set = |name: Option<&str>, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let name = name.map(HSTRING::from);
            RegSetValueExW(key, name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())), None, REG_SZ, Some(bytes))
        };
        let a = set(None, &dll.to_string_lossy());
        let b = set(Some("ThreadingModel"), "Both");
        let _ = RegCloseKey(key);
        if a.is_err() || b.is_err() {
            return Err("couldn't register the camera's media source".into());
        }
    }
    Ok(())
}

/// `rdpio --hyper-camera-test` (as administrator, on Windows 11): offer
/// "rdpio Camera", feed it a known picture, open it the way an app does, and
/// check that the picture arrives and that the host hears an app using it —
/// the whole host side, without a client.
pub fn selftest() -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::*;
    crate::wasapi::com_init();
    unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    }
    let vcam = Arc::new(VirtualCamera::create()?);
    println!("virtual camera created; feeding a grey picture (Y 200)");
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (vcam, stop) = (vcam.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut frame = vec![128u8; FRAME_BYTES];
            frame[..(WIDTH * HEIGHT) as usize].fill(200);
            while !stop.load(Ordering::Relaxed) {
                vcam.write(&frame);
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let result = (|| -> Result<(), String> {
        // Windows takes a moment to list a new camera.
        let started = Instant::now();
        let activate = loop {
            if let Some(a) = unsafe { find_camera("rdpio Camera") } {
                break a;
            }
            if started.elapsed() > Duration::from_secs(10) {
                return Err("'rdpio Camera' never appeared among the cameras".into());
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        println!("listed after {:?}; opening it", started.elapsed());
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject().map_err(|e| format!("activating: {e}"))?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, None).map_err(|e| format!("reader: {e}"))?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let mut got = None;
            for _ in 0..30 {
                let (mut flags, mut sample) = (0u32, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).map_err(|e| format!("reading: {e}"))?;
                if let Some(s) = sample {
                    let buffer = s.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
                    let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
                    buffer.Lock(&mut data, None, Some(&mut len)).map_err(|e| e.to_string())?;
                    let luma = if data.is_null() || len == 0 { None } else { Some(*data.add((len as usize / 3) as usize)) };
                    let _ = buffer.Unlock();
                    got = luma;
                    if got == Some(200) {
                        break;
                    }
                }
            }
            println!("an app's view: luma {got:?} (fed 200); host hears it in use: {}", vcam.wanted());
            if !vcam.wanted() {
                return Err("the camera ran but the host didn't hear an app using it".into());
            }
            if got.is_none_or(|y| y.abs_diff(200) > 8) {
                return Err(format!("the picture didn't arrive (luma {got:?})"));
            }
            let _ = source.Shutdown();
        }
        drop(activate);
        std::thread::sleep(Duration::from_millis(500));
        println!("closed; host hears it in use: {}", vcam.wanted());
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    result
}

/// The video capture device named like `name`.
unsafe fn find_camera(name: &str) -> Option<windows::Win32::Media::MediaFoundation::IMFActivate> {
    use windows::Win32::Media::MediaFoundation::*;
    let attrs = crate::mf_camera::vidcap_attributes().ok()?;
    let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attrs, &mut devices, &mut count).ok()?;
    let list = std::slice::from_raw_parts(devices, count as usize);
    let found = list.iter().flatten().find(|a| crate::mf_camera::device_name(a).contains(name)).cloned();
    for i in 0..count as usize {
        drop(std::ptr::read(devices.add(i)));
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(devices as *const core::ffi::c_void));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_pictures_are_found() {
        assert!(has_idr(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65, 9]));
        assert!(!has_idr(&[0, 0, 0, 1, 0x41, 1, 2]));
    }

    #[test]
    fn the_media_source_is_built_in() {
        assert!(!DLL.is_empty(), "rdpio_vcam.dll wasn't built");
        assert_eq!(&DLL[..2], b"MZ");
    }
}
