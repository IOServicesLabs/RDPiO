//! Low-latency audio through WASAPI (shared mode): device lookup, the default
//! device switch the Hyper host uses to route game audio, loopback and mic
//! capture, and an event-driven player with a small adaptive jitter buffer.
//!
//! Everything here works in 48 kHz interleaved `f32` (the Opus rate); capture
//! converts from the device's mix format and the player lets the audio engine
//! convert to the device (`AUTOCONVERTPCM`).
//!
//! Devices come and go (a headset unplugged, an HDMI endpoint reset by a
//! display mode change, a new default): a capture reports it by failing
//! `read` or through `default_changed`, a player by `is_alive` turning false,
//! and their owners open new ones.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// The rate everything is converted to.
pub const RATE: u32 = 48_000;

/// Which way audio flows through an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Render,
    Capture,
}

impl Flow {
    fn edataflow(self) -> EDataFlow {
        match self {
            Self::Render => eRender,
            Self::Capture => eCapture,
        }
    }
}

/// An active audio endpoint.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
}

pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    com_init();
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

unsafe fn take_pwstr(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
    s
}

pub(crate) unsafe fn friendly_name(device: &IMMDevice) -> String {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    let Ok(store) = device.OpenPropertyStore(STGM_READ) else { return String::new() };
    let Ok(value) = store.GetValue(&PKEY_Device_FriendlyName) else { return String::new() };
    PropVariantToStringAlloc(&value).map(|p| take_pwstr(p)).unwrap_or_default()
}

/// Active endpoints of `flow`.
pub fn endpoints(flow: Flow) -> Vec<Endpoint> {
    let mut out = Vec::new();
    unsafe {
        let Ok(e) = enumerator() else { return out };
        let Ok(list) = e.EnumAudioEndpoints(flow.edataflow(), DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            if let Ok(d) = list.Item(i) {
                if let Ok(id) = d.GetId() {
                    out.push(Endpoint { id: take_pwstr(id), name: friendly_name(&d) });
                }
            }
        }
    }
    out
}

/// The first active endpoint whose name contains `needle` (case-insensitive).
pub fn find(flow: Flow, needle: &str) -> Option<Endpoint> {
    let needle = needle.to_ascii_lowercase();
    endpoints(flow).into_iter().find(|e| e.name.to_ascii_lowercase().contains(&needle))
}

/// The current default endpoint of `flow` (console role).
pub fn default_endpoint(flow: Flow) -> Option<Endpoint> {
    unsafe {
        let e = enumerator().ok()?;
        let d = e.GetDefaultAudioEndpoint(flow.edataflow(), eConsole).ok()?;
        Some(Endpoint { id: take_pwstr(d.GetId().ok()?), name: friendly_name(&d) })
    }
}

/// The roles a default device is chosen for, in the order `default_ids`
/// reports them. The Sound control panel's "Default Device" is console and
/// multimedia; its "Default Communications Device" (voice chat) is the third.
pub const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// Each role's default endpoint of `flow`, in `ROLES` order.
fn default_endpoints(flow: Flow) -> [Option<Endpoint>; 3] {
    let Ok(e) = enumerator() else { return Default::default() };
    ROLES.map(|role| unsafe {
        let d = e.GetDefaultAudioEndpoint(flow.edataflow(), role).ok()?;
        Some(Endpoint { id: take_pwstr(d.GetId().ok()?), name: friendly_name(&d) })
    })
}

/// Each role's default endpoint id for `flow`, in `ROLES` order: what a crash
/// marker keeps for `restore_default_ids`.
pub fn default_ids(flow: Flow) -> [Option<String>; 3] {
    let Ok(e) = enumerator() else { return Default::default() };
    ROLES.map(|role| unsafe { default_id(&e, flow.edataflow(), role) })
}

/// The default endpoint id of `flow` for `role`; cheap enough to poll.
unsafe fn default_id(e: &IMMDeviceEnumerator, flow: EDataFlow, role: ERole) -> Option<String> {
    let d = e.GetDefaultAudioEndpoint(flow, role).ok()?;
    Some(take_pwstr(d.GetId().ok()?))
}

/// `IPolicyConfig`: the (undocumented but long-stable) interface the Sound
/// control panel itself sets default devices with; there is no public API.
struct PolicyConfig(windows::core::IUnknown);

impl PolicyConfig {
    fn new() -> windows::core::Result<Self> {
        const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
        const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
        com_init();
        unsafe {
            let unknown: windows::core::IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
            let mut raw = core::ptr::null_mut();
            unknown.query(&IID_POLICY_CONFIG, &mut raw).ok()?;
            Ok(Self(windows::core::IUnknown::from_raw(raw)))
        }
    }

    /// Make `id` the default device for `role`.
    fn set(&self, id: &str, role: ERole) -> windows::core::Result<()> {
        /// `IPolicyConfig::SetDefaultEndpoint` — vtable slot 13 (after IUnknown's
        /// three and ten format/period/share-mode/property methods).
        type SetDefault = unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, ERole) -> HRESULT;
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let vtbl = *(self.0.as_raw() as *const *const usize);
            let set: SetDefault = core::mem::transmute(*vtbl.add(13));
            set(self.0.as_raw(), PCWSTR(wide.as_ptr()), role).ok()
        }
    }
}

/// Make `id` the default device for every role.
pub fn set_default_endpoint(id: &str) -> windows::core::Result<()> {
    let policy = PolicyConfig::new()?;
    ROLES.into_iter().try_for_each(|role| policy.set(id, role))
}

/// Put each role back on the device `default_ids` saved for it (roles without
/// one are left alone). Every role is tried; the last error is returned.
pub fn restore_default_ids(ids: &[Option<String>]) -> windows::core::Result<()> {
    let policy = PolicyConfig::new()?;
    let mut result = Ok(());
    for (role, id) in ROLES.into_iter().zip(ids) {
        if let Some(id) = id {
            if let Err(e) = policy.set(id, role) {
                result = Err(e);
            }
        }
    }
    result
}

/// Switches the default endpoint of `flow` (every role) and puts each role's
/// own previous device back on drop, so a separate Default Communications
/// Device (a headset for voice chat) is still that afterwards.
pub struct DefaultSwitch {
    /// The roles switched, each with the device it had before.
    previous: Vec<(ERole, Endpoint)>,
}

impl DefaultSwitch {
    pub fn to(flow: Flow, target: &Endpoint) -> Option<Self> {
        let switched = PolicyConfig::new()
            .and_then(|policy| switch_roles(default_endpoints(flow), &target.id, |id, role| policy.set(id, role)));
        match switched {
            Ok(previous) => {
                if !previous.is_empty() {
                    let mut from: Vec<&str> = previous.iter().map(|(_, p)| p.name.as_str()).collect();
                    from.dedup();
                    tracing::info!(?flow, to = %target.name, ?from, "default audio device switched");
                }
                Some(Self { previous })
            }
            Err(e) => {
                tracing::warn!(?flow, to = %target.name, error = %e, "could not switch the default audio device");
                None
            }
        }
    }
}

impl Drop for DefaultSwitch {
    fn drop(&mut self) {
        if self.previous.is_empty() {
            return;
        }
        let restored = PolicyConfig::new().map(|policy| restore_roles(&self.previous, |id, role| policy.set(id, role)));
        if let Err(e) = restored {
            tracing::warn!(error = %e, "could not restore the default audio devices");
        }
    }
}

/// Switch every role to `target` through `set`, returning the roles changed
/// with the device each had. Roles already there are left alone; if one
/// fails, those already switched are put back first.
fn switch_roles<E: std::fmt::Display>(
    before: [Option<Endpoint>; 3],
    target: &str,
    mut set: impl FnMut(&str, ERole) -> Result<(), E>,
) -> Result<Vec<(ERole, Endpoint)>, E> {
    let mut previous = Vec::new();
    for (role, old) in ROLES.into_iter().zip(before) {
        if old.as_ref().is_some_and(|p| p.id == target) {
            continue;
        }
        if let Err(e) = set(target, role) {
            restore_roles(&previous, &mut set);
            return Err(e);
        }
        // A role that had no device can't be given one back.
        if let Some(old) = old {
            previous.push((role, old));
        }
    }
    Ok(previous)
}

/// Put each switched role back on its previous device.
fn restore_roles<E: std::fmt::Display>(previous: &[(ERole, Endpoint)], mut set: impl FnMut(&str, ERole) -> Result<(), E>) {
    for (role, p) in previous {
        if let Err(e) = set(&p.id, *role) {
            tracing::warn!(error = %e, device = %p.name, role = role.0, "could not restore the default audio device");
        }
    }
}

/// The device's mix format, reduced to what conversion needs.
#[derive(Debug, Clone, Copy)]
struct MixFormat {
    rate: u32,
    channels: usize,
    float: bool,
    bits: u16,
    /// Speaker positions of the channels, in order (`SPEAKER_*` bits, lowest
    /// first); 0 when the format doesn't say.
    mask: u32,
}

unsafe fn mix_format(client: &IAudioClient) -> windows::core::Result<(*mut WAVEFORMATEX, MixFormat)> {
    let p = client.GetMixFormat()?;
    let f = *p;
    let (tag, mask) = if f.wFormatTag == 0xFFFE && f.cbSize >= 22 {
        // WAVEFORMATEXTENSIBLE: the SubFormat GUID's first field is the tag.
        let ext = core::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
        (ext.SubFormat.data1 as u16, ext.dwChannelMask)
    } else {
        (f.wFormatTag, 0)
    };
    let format = MixFormat { rate: f.nSamplesPerSec, channels: f.nChannels as usize, float: tag == 3, bits: f.wBitsPerSample, mask };
    Ok((p, format))
}

/// Gain of each device channel into each of `out` channels (1 or 2), by
/// speaker position (`mask`, or the usual layout for the channel count).
/// Front left/right pass through, the centre and surrounds join at -3 dB as in
/// the standard (ITU) downmix, LFE stays out as it does there, and a mono
/// device feeds both sides at full level. Unpositioned channels past the first
/// two (taken as left and right) are dropped.
fn mix_gains(channels: usize, mask: u32, out: usize) -> Vec<[f32; 2]> {
    use windows::Win32::Media::KernelStreaming::{
        SPEAKER_BACK_CENTER as BC, SPEAKER_BACK_LEFT as BL, SPEAKER_BACK_RIGHT as BR, SPEAKER_FRONT_CENTER as FC,
        SPEAKER_FRONT_LEFT as FL, SPEAKER_FRONT_LEFT_OF_CENTER as FLC, SPEAKER_FRONT_RIGHT as FR,
        SPEAKER_FRONT_RIGHT_OF_CENTER as FRC, SPEAKER_LOW_FREQUENCY as LFE, SPEAKER_SIDE_LEFT as SL,
        SPEAKER_SIDE_RIGHT as SR, SPEAKER_TOP_BACK_CENTER as TBC, SPEAKER_TOP_BACK_LEFT as TBL,
        SPEAKER_TOP_BACK_RIGHT as TBR, SPEAKER_TOP_CENTER as TC, SPEAKER_TOP_FRONT_CENTER as TFC,
        SPEAKER_TOP_FRONT_LEFT as TFL, SPEAKER_TOP_FRONT_RIGHT as TFR,
    };
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    // No mask (direct out, or a plain WAVEFORMATEX): the usual layout for the
    // count (quad, 5.0, 5.1, 7.1; three as left, right and centre).
    let mut positions = match (mask, channels) {
        (0, 3) => FL | FR | FC,
        (0, 4) => FL | FR | BL | BR,
        (0, 5) => FL | FR | FC | BL | BR,
        (0, 6) => FL | FR | FC | LFE | BL | BR,
        (0, 8) => FL | FR | FC | LFE | BL | BR | SL | SR,
        _ => mask,
    };
    (0..channels)
        .map(|c| {
            // The channels carry the mask's positions in bit order.
            let position = positions & positions.wrapping_neg();
            positions &= !position;
            let [l, r] = match position {
                _ if channels == 1 => [1.0, 1.0],
                FL => [1.0, 0.0],
                FR => [0.0, 1.0],
                FC => [H, H],
                LFE => [0.0, 0.0],
                BL | SL | TFL | TBL => [H, 0.0],
                BR | SR | TFR | TBR => [0.0, H],
                // Halfway between the front side and the centre.
                FLC => [0.924, 0.383],
                FRC => [0.383, 0.924],
                BC | TC | TFC | TBC => [0.5, 0.5],
                _ => match c {
                    0 => [1.0, 0.0],
                    1 => [0.0, 1.0],
                    _ => [0.0, 0.0],
                },
            };
            if out == 1 {
                [(l + r) * 0.5, 0.0]
            } else {
                [l, r]
            }
        })
        .collect()
}

/// One device frame (`sample(channel)`) mixed by `gains` from `mix_gains`.
#[inline]
fn mix(gains: &[[f32; 2]], sample: impl Fn(usize) -> f32) -> [f32; 2] {
    let mut acc = [0.0; 2];
    for (c, g) in gains.iter().enumerate() {
        let s = sample(c);
        acc[0] += s * g[0];
        acc[1] += s * g[1];
    }
    acc
}

/// Sample `idx` of a device buffer in `format`, as `f32`.
#[inline]
unsafe fn sample_at(data: *const u8, idx: usize, format: &MixFormat) -> f32 {
    match (format.float, format.bits) {
        (true, _) => *(data as *const f32).add(idx),
        (false, 16) => *(data as *const i16).add(idx) as f32 / 32768.0,
        (false, 32) => *(data as *const i32).add(idx) as f32 / 2147483648.0,
        (false, 24) => {
            let b = data.add(idx * 3);
            (((*b as i32) << 8 | (*b.add(1) as i32) << 16 | (*b.add(2) as i32) << 24) >> 8) as f32 / 8388608.0
        }
        _ => 0.0,
    }
}

/// Shared-mode capture: loopback of a render device (what it plays), or a
/// microphone. Delivers 48 kHz `f32` with `channels` (1 or 2) interleaved.
pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: MixFormat,
    channels: usize,
    /// Each device channel's gain into each of ours (`mix_gains`).
    gains: Vec<[f32; 2]>,
    /// Fractional source position for resampling.
    phase: f64,
    last: Vec<f32>,
    /// One packet in our channels; kept, so reading doesn't allocate.
    converted: Vec<f32>,
    /// Opened as the default device: where to ask for the default, and the
    /// id it had then.
    default: Option<(IMMDeviceEnumerator, EDataFlow, String)>,
    /// Signalled by the audio engine each time it has captured a period
    /// (event-driven mode); `None` where the device refused it (polled).
    ready: Option<windows::Win32::Foundation::HANDLE>,
}

impl Capture {
    /// `device`: an endpoint id, or `None` for the default (see
    /// `default_changed`). `loopback` captures a render device's output.
    pub fn new(device: Option<&str>, flow: Flow, loopback: bool, channels: usize) -> windows::core::Result<Self> {
        let channels = channels.clamp(1, 2);
        unsafe {
            let e = enumerator()?;
            let dev = match device {
                Some(id) => {
                    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                    e.GetDevice(PCWSTR(wide.as_ptr()))?
                }
                None => e.GetDefaultAudioEndpoint(flow.edataflow(), eConsole)?,
            };
            let default = match device {
                Some(_) => None,
                None => Some((e.clone(), flow.edataflow(), take_pwstr(dev.GetId()?))),
            };
            let mut client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let (fmt_ptr, format) = mix_format(&client)?;
            let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            // 20 ms engine buffer, read as the engine signals each period
            // (no polling every few ms); polled where a device won't signal.
            let mut r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 200_000, 0, fmt_ptr, None);
            let mut ready = None;
            if r.is_ok() {
                match windows::Win32::System::Threading::CreateEventW(None, false, false, None) {
                    Ok(ev) if client.SetEventHandle(ev).is_ok() => ready = Some(ev),
                    Ok(ev) => {
                        let _ = windows::Win32::Foundation::CloseHandle(ev);
                        r = Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
                    }
                    Err(e) => r = Err(e),
                }
            }
            if r.is_err() {
                // (A client initializes once: a fresh one, polled.)
                client = dev.Activate(CLSCTX_ALL, None)?;
                r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, fmt_ptr, None);
            }
            CoTaskMemFree(Some(fmt_ptr as *const core::ffi::c_void));
            r?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            tracing::info!(name = %friendly_name(&dev), loopback, rate = format.rate, ch = format.channels, mask = format.mask, float = format.float, "audio capture started");
            Ok(Self {
                client,
                capture,
                format,
                channels,
                gains: mix_gains(format.channels, format.mask, channels),
                phase: 0.0,
                last: vec![0.0; channels],
                converted: Vec::new(),
                default,
                ready,
            })
        }
    }

    /// Whether the engine signals captured data (else it's polled).
    #[cfg(test)]
    pub fn event_driven(&self) -> bool {
        self.ready.is_some()
    }

    /// Until the engine has something to read (event-driven: at most
    /// 100 ms, so the owner still checks in), or `poll` where it doesn't signal.
    pub fn wait(&self, poll: std::time::Duration) {
        match self.ready {
            Some(h) => unsafe {
                let _ = windows::Win32::System::Threading::WaitForSingleObject(h, 100);
            },
            None => std::thread::sleep(poll),
        }
    }

    /// Opened as the default device, and the default has moved to another
    /// one since: the owner should open a new capture. Cheap; for polling
    /// every second or so.
    pub fn default_changed(&self) -> bool {
        let Some((e, flow, id)) = &self.default else { return false };
        unsafe { default_id(e, *flow, eConsole) }.is_some_and(|now| now != *id)
    }

    /// Append everything captured so far (48 kHz, interleaved) to `out`. An
    /// error means the device is gone (`AUDCLNT_E_DEVICE_INVALIDATED`) or
    /// broken: open a new capture.
    pub fn read(&mut self, out: &mut Vec<f32>) -> windows::core::Result<()> {
        unsafe {
            loop {
                let n = self.capture.GetNextPacketSize()?;
                if n == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = core::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let mut converted = std::mem::take(&mut self.converted);
                converted.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    converted.resize(frames as usize * self.channels, 0.0);
                } else {
                    let (format, ch) = (self.format, self.format.channels.max(1));
                    for i in 0..frames as usize {
                        let frame = mix(&self.gains, |c| sample_at(data, i * ch + c, &format));
                        converted.extend_from_slice(&frame[..self.channels]);
                    }
                }
                let released = self.capture.ReleaseBuffer(frames);
                self.resample_into(&converted, out);
                self.converted = converted;
                released?;
            }
        }
    }

    /// Linear resample from the device rate to 48 kHz (a copy when equal).
    fn resample_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if self.format.rate == RATE {
            out.extend_from_slice(input);
            return;
        }
        let step = self.format.rate as f64 / RATE as f64;
        let frames = input.len() / ch;
        let at = |i: isize, c: usize, last: &[f32]| if i < 0 { last[c] } else { input[i as usize * ch + c] };
        while self.phase < frames as f64 {
            let i = self.phase.floor() as isize;
            let t = (self.phase - i as f64) as f32;
            for c in 0..ch {
                let a = at(i - 1, c, &self.last);
                let b = at(i, c, &self.last);
                out.push(a + (b - a) * t);
            }
            self.phase += step;
        }
        self.phase -= frames as f64;
        if frames > 0 {
            self.last.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(h) = self.ready.take() {
                let _ = windows::Win32::Foundation::CloseHandle(h);
            }
        }
    }
}

/// Jitter-buffered output: callers push 48 kHz interleaved `f32`; a render
/// thread feeds the device on its own clock. That thread ends when the device
/// goes away or fails, or (playing to the default) when the default output
/// changes; `is_alive` then tells the owner to open a new player.
pub struct Player {
    queue: Arc<Mutex<Playout>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Render threads whose device didn't open in time (a driver stuck in
/// Activate or Initialize), with the device's id. They're told to stop, and
/// no new attempt on that device starts until they're gone, so a hung driver
/// holds one thread rather than one per retry.
static STALLED: Mutex<Vec<(Option<String>, JoinHandle<()>)>> = Mutex::new(Vec::new());

impl Player {
    /// Play to `device` (endpoint id) or the default output.
    pub fn new(device: Option<String>, channels: usize) -> windows::core::Result<Self> {
        let channels = channels.clamp(1, 2);
        // Which device this is, so a stuck default doesn't hold up a new one.
        let id = match &device {
            Some(id) => Some(id.clone()),
            None => enumerator().ok().and_then(|e| unsafe { default_id(&e, eRender, eConsole) }),
        };
        {
            let mut stalled = STALLED.lock().unwrap();
            stalled.retain(|(_, t)| !t.is_finished());
            if stalled.iter().any(|(stuck, _)| *stuck == id) {
                return Err(windows::core::Error::new(E_FAIL, "audio output still stuck opening"));
            }
        }
        let queue = Arc::new(Mutex::new(Playout::new(channels)));
        let stop = Arc::new(AtomicBool::new(false));
        let (q, s) = (queue.clone(), stop.clone());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("wasapi-render".into())
            .spawn(move || {
                let r = unsafe { render_thread(device, channels, q, s, &ready_tx) };
                if let Err(e) = r {
                    let _ = ready_tx.send(Err(e.clone()));
                    tracing::warn!(error = %e, "audio output stopped");
                }
            })
            .map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self { queue, stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // Stuck in the driver: it ends by itself once that returns.
                stop.store(true, Ordering::Relaxed);
                if !thread.is_finished() {
                    STALLED.lock().unwrap().push((id, thread));
                }
                Err(windows::core::Error::new(E_FAIL, "audio output didn't start"))
            }
        }
    }

    /// Queue interleaved samples.
    pub fn push(&self, samples: &[f32]) {
        self.queue.lock().unwrap().push(samples);
    }

    /// Hold the queue's low point at `ms` from now on (it was `TARGET_MS`):
    /// as much room as the network's jitter needs, and no more delay.
    pub fn set_target_ms(&self, ms: u32) {
        self.queue.lock().unwrap().target = frames(ms as usize);
    }

    /// How many times playback has run dry so far.
    pub fn underruns(&self) -> u32 {
        self.queue.lock().unwrap().underruns
    }

    /// False once the render thread has ended (the device was removed or
    /// failed, or the default output changed): open a new player.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start playing once this much is queued (ms), then hold the queue's low
/// point there: room for the network's jitter, and no more. Where the
/// network's jitter is measured, the owner moves it ([`Player::set_target_ms`]).
const TARGET_MS: usize = 30;
/// A backlog this far past the target (a long stall's) is cut back to the
/// target at once.
const MAX_QUEUE_MS: usize = 60;
/// How far the low point may wander from the target before it's corrected.
const SLACK_MS: usize = 10;
/// How long the low point is watched before each decision.
const WINDOW_MS: usize = 500;
/// Corrections play at most 1/SKEW faster or slower: 0.5%, a pitch change of
/// 9 cents nobody hears, which still moves the level 5 ms a second.
const SKEW: usize = 200;

/// Frames in `ms` milliseconds.
const fn frames(ms: usize) -> usize {
    ms * RATE as usize / 1000
}

/// The jitter buffer between the network and the device, in interleaved
/// 48 kHz frames. Playback starts once `TARGET_MS` is queued; a new player
/// drops any older backlog (what piled up while its device opened). After
/// that, bursts and drift between the sender's clock and the device's would
/// leave a lasting lag or run the queue dry, so the lowest level seen in each
/// `WINDOW_MS` steers it back to the target: playback runs a fraction of a
/// percent faster or slower, interpolated, so no sample is skipped or
/// repeated.
struct Playout {
    q: VecDeque<f32>,
    ch: usize,
    primed: bool,
    /// Lowest level (frames) at the start of a fill this window, counting
    /// corrections made since as if they had come before it.
    low: usize,
    /// Frames played this window.
    played: usize,
    /// Frames still to play away (> 0) or stretch in (< 0).
    pending: isize,
    /// Frames played toward the next corrected frame (the `SKEW` limit).
    budget: usize,
    /// The frame before the queue's front (interpolation looks one back).
    prev: [f32; 2],
    /// Playback has started once: only that first start drops a backlog.
    started: bool,
    /// The low point held (frames).
    target: usize,
    /// Times playback ran dry.
    underruns: u32,
}

impl Playout {
    fn new(ch: usize) -> Self {
        let ch = ch.clamp(1, 2);
        Self {
            q: VecDeque::with_capacity(frames(MAX_QUEUE_MS + 20) * ch),
            ch,
            primed: false,
            low: usize::MAX,
            played: 0,
            pending: 0,
            budget: 0,
            prev: [0.0; 2],
            started: false,
            target: frames(TARGET_MS),
            underruns: 0,
        }
    }

    /// Queued frames.
    fn level(&self) -> usize {
        self.q.len() / self.ch
    }

    /// A new watch window, with nothing left to correct.
    fn reset(&mut self) {
        self.low = usize::MAX;
        self.played = 0;
        self.pending = 0;
    }

    fn push(&mut self, samples: &[f32]) {
        self.q.extend(samples.iter().copied());
        if self.level() > self.target + frames(MAX_QUEUE_MS) {
            let drop = (self.level() - self.target) * self.ch;
            self.q.drain(..drop);
            self.reset();
        }
    }

    /// Fill `out` (whole interleaved frames); returns how many samples came
    /// from the queue. The rest is silence: still priming, or it ran dry.
    fn fill(&mut self, out: &mut [f32]) -> usize {
        let (ch, target) = (self.ch, self.target);
        let n = out.len() / ch;
        if !self.primed {
            if n == 0 || self.level() < target {
                out.fill(0.0);
                return 0;
            }
            // A new player starts at the target, not behind what queued
            // while its device opened. After running dry the late burst is
            // kept: dropping it would leave no room for the next stall like
            // it (underrun after underrun on a bursty link), and the steering
            // below plays it away.
            if !self.started {
                let backlog = (self.level() - target) * ch;
                self.q.drain(..backlog);
                self.started = true;
            }
            self.prev = [0.0; 2];
            self.primed = true;
            self.reset();
        }
        let level = self.level();
        self.low = self.low.min(level);
        self.played += n;
        self.budget = (self.budget + n).min(2 * SKEW);
        if self.played >= frames(WINDOW_MS) {
            // Correct once the low point is off by more than the slack, and
            // then all the way back.
            let error = self.low as isize - target as isize;
            let off = error.unsigned_abs();
            let pending = if off > frames(SLACK_MS) || (self.pending != 0 && off > frames(1)) { error } else { 0 };
            self.reset();
            self.pending = pending;
        }
        // This fill's share: toward the target only, within the skew budget,
        // and with enough queued to interpolate across.
        let step = (self.budget / SKEW) as isize;
        let mut d = match self.pending {
            p if p > 0 && level > target => p.min(step),
            p if p < 0 && level < target => p.max(-step),
            _ => 0,
        };
        if d.unsigned_abs() * 8 > n || level < (n as isize + d) as usize + 2 {
            d = 0;
        }
        let filled = if d == 0 { self.copy(out) } else { self.resample(out, d) };
        if d != 0 {
            self.pending -= d;
            self.budget -= d.unsigned_abs() * SKEW;
            self.low = self.low.saturating_add_signed(-d);
        }
        if filled < out.len() {
            // Ran dry: silence, and prime again before resuming so playback
            // doesn't stutter.
            out[filled..].fill(0.0);
            self.primed = false;
            self.underruns += 1;
        }
        filled
    }

    /// Play the next frames as they are; returns the samples filled.
    fn copy(&mut self, out: &mut [f32]) -> usize {
        let ch = self.ch;
        let take = (out.len() / ch).min(self.level()) * ch;
        if take > 0 {
            for c in 0..ch {
                self.prev[c] = self.q[take - ch + c];
            }
        }
        for (o, s) in out.iter_mut().zip(self.q.drain(..take)) {
            *o = s;
        }
        take
    }

    /// Play `n + d` queued frames as the `n` of `out`, interpolated, so the
    /// queue shrinks (`d` > 0) or grows (`d` < 0) by `d` frames. Each call
    /// starts exactly on a frame, so consecutive ones join without a seam.
    fn resample(&mut self, out: &mut [f32], d: isize) -> usize {
        let ch = self.ch;
        let n = out.len() / ch;
        let span = (n as isize + d) as usize;
        let (q, prev) = (&self.q, self.prev);
        // Frame `k - 1` of the queue; 0 is the one played before it.
        let at = |k: usize, c: usize| if k == 0 { prev[c] } else { q[(k - 1) * ch + c] };
        for j in 0..n {
            let pos = j * span;
            let (k, t) = (pos / n, (pos % n) as f32 / n as f32);
            for c in 0..ch {
                out[j * ch + c] = cubic(at(k, c), at(k + 1, c), at(k + 2, c), at(k + 3, c), t);
            }
        }
        for c in 0..ch {
            self.prev[c] = self.q[(span - 1) * ch + c];
        }
        self.q.drain(..span * ch);
        n * ch
    }
}

/// Catmull-Rom interpolation between `b` and `c` (`a` and `d` their outer
/// neighbours) at `t` in 0..1: exactly `b` at 0.
#[inline]
fn cubic(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    b + 0.5 * t * (c - a + t * (2.0 * a - 5.0 * b + 4.0 * c - d + t * (3.0 * (b - c) + d - a)))
}

/// Closes an event handle on drop, whichever way the render thread ends.
struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Stops a started audio client on drop.
struct Started<'a>(&'a IAudioClient);

impl Drop for Started<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Stop();
        }
    }
}

unsafe fn render_thread(
    device: Option<String>,
    channels: usize,
    queue: Arc<Mutex<Playout>>,
    stop: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<windows::core::Result<()>>,
) -> windows::core::Result<()> {
    // Made first, so it's closed last: after the client that signals it.
    let event = Event(CreateEventW(None, false, false, None)?);
    let e = enumerator()?;
    let dev = match &device {
        Some(id) => {
            let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            e.GetDevice(PCWSTR(wide.as_ptr()))?
        }
        None => e.GetDefaultAudioEndpoint(eRender, eConsole)?,
    };
    // Playing to the default: its id now, to notice when the default changes.
    let default = match &device {
        Some(_) => None,
        None => Some(take_pwstr(dev.GetId()?)),
    };
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    let block = (channels * 4) as u16;
    let fmt = WAVEFORMATEX {
        wFormatTag: 3, // IEEE float
        nChannels: channels as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        100_000, // 10 ms
        0,
        &fmt,
        None,
    )?;
    client.SetEventHandle(event.0)?;
    let render: IAudioRenderClient = client.GetService()?;
    let size = client.GetBufferSize()?;
    if stop.load(Ordering::Relaxed) {
        return Ok(()); // `Player::new` gave up waiting for it
    }
    client.Start()?;
    let _started = Started(&client);
    tracing::info!(name = %friendly_name(&dev), buffer_frames = size, "audio output started");
    let _ = ready.send(Ok(()));
    let mut checked = Instant::now();
    let mut quiet = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let signalled = WaitForSingleObject(event.0, 100) == WAIT_OBJECT_0;
        // Asked on timeouts too: a removed device says so here
        // (AUDCLNT_E_DEVICE_INVALIDATED), while its event may just stop.
        let padding = client.GetCurrentPadding()?;
        if let Some(id) = &default {
            if checked.elapsed() >= Duration::from_secs(1) {
                checked = Instant::now();
                if default_id(&e, eRender, eConsole).is_some_and(|now| now != *id) {
                    tracing::info!("default audio output changed");
                    return Ok(());
                }
            }
        }
        if !signalled {
            quiet += 1;
            if quiet >= 20 {
                return Err(windows::core::Error::new(E_FAIL, "audio output stalled: no device events for 2 s"));
            }
            continue;
        }
        quiet = 0;
        let free = size.saturating_sub(padding) as usize;
        if free == 0 {
            continue;
        }
        let buf = render.GetBuffer(free as u32)? as *mut f32;
        let out = core::slice::from_raw_parts_mut(buf, free * channels);
        queue.lock().unwrap().fill(out);
        render.ReleaseBuffer(free as u32, 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str) -> Option<Endpoint> {
        Some(Endpoint { id: id.into(), name: id.to_uppercase() })
    }

    /// Speakers as the default device and a headset for communications:
    /// every role is switched, and each gets its own device back.
    #[test]
    fn each_role_gets_its_own_device_back() {
        let mut calls = Vec::new();
        let previous = switch_roles([ep("spk"), ep("spk"), ep("headset")], "virtual", |id: &str, role: ERole| {
            calls.push(format!("{id}@{}", role.0));
            Ok::<(), String>(())
        })
        .unwrap();
        assert_eq!(calls, ["virtual@0", "virtual@1", "virtual@2"]);
        calls.clear();
        restore_roles(&previous, |id: &str, role: ERole| {
            calls.push(format!("{id}@{}", role.0));
            Ok::<(), String>(())
        });
        assert_eq!(calls, ["spk@0", "spk@1", "headset@2"]);
    }

    /// A role already on the target is neither switched nor restored.
    #[test]
    fn roles_already_on_the_target_are_left_alone() {
        let mut calls = Vec::new();
        let previous = switch_roles([ep("virtual"), ep("virtual"), ep("headset")], "virtual", |id: &str, role: ERole| {
            calls.push(format!("{id}@{}", role.0));
            Ok::<(), String>(())
        })
        .unwrap();
        assert_eq!(calls, ["virtual@2"]);
        assert_eq!(previous.len(), 1);
        assert_eq!((previous[0].0, previous[0].1.id.as_str()), (eCommunications, "headset"));
    }

    /// A failure part-way puts back the roles already switched.
    #[test]
    fn a_half_done_switch_is_undone() {
        let mut calls = Vec::new();
        let r = switch_roles([ep("spk"), ep("spk"), ep("headset")], "virtual", |id: &str, role: ERole| {
            calls.push(format!("{id}@{}", role.0));
            if id == "virtual" && role == eMultimedia {
                Err("denied".to_string())
            } else {
                Ok(())
            }
        });
        assert!(r.is_err());
        assert_eq!(calls, ["virtual@0", "virtual@1", "spk@0"]);
    }

    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;

    /// `frame` from a device with `channels` and `mask`, mixed to `out` channels.
    fn mixed(channels: usize, mask: u32, out: usize, frame: &[f32]) -> [f32; 2] {
        mix(&mix_gains(channels, mask, out), |c| frame[c])
    }

    fn assert_close(got: [f32; 2], want: [f32; 2]) {
        assert!((got[0] - want[0]).abs() < 1e-5 && (got[1] - want[1]).abs() < 1e-5, "got {got:?}, want {want:?}");
    }

    /// 5.1 and 7.1: centre and surrounds fold in at -3 dB, LFE stays out.
    #[test]
    fn surround_folds_into_stereo() {
        // FL FR FC LFE BL BR
        let frame = [0.1, 0.2, 0.3, 0.9, 0.4, 0.5];
        let want = [0.1 + H * (0.3 + 0.4), 0.2 + H * (0.3 + 0.5)];
        assert_close(mixed(6, 0x3F, 2, &frame), want);
        // The same with side speakers, and without a mask at all.
        assert_close(mixed(6, 0x60F, 2, &frame), want);
        assert_close(mixed(6, 0, 2, &frame), want);
        // FL FR FC LFE BL BR SL SR
        let frame = [0.1, 0.2, 0.3, 0.9, 0.4, 0.5, 0.6, 0.7];
        assert_close(mixed(8, 0x63F, 2, &frame), [0.1 + H * (0.3 + 0.4 + 0.6), 0.2 + H * (0.3 + 0.5 + 0.7)]);
    }

    /// Quad's back left isn't a centre, nor is 2.1's LFE.
    #[test]
    fn rear_and_lfe_channels_are_not_a_centre() {
        assert_close(mixed(4, 0x33, 2, &[0.1, 0.2, 0.4, 0.5]), [0.1 + H * 0.4, 0.2 + H * 0.5]);
        assert_close(mixed(3, 0x0B, 2, &[0.1, 0.2, 0.9]), [0.1, 0.2]);
        // Positions it doesn't know: the first two are left and right.
        assert_close(mixed(4, 0x8000_0000, 2, &[0.1, 0.2, 0.4, 0.5]), [0.1, 0.2]);
    }

    /// A mono device is heard on both sides at full level; stereo passes
    /// through, or averages into a mono capture.
    #[test]
    fn mono_and_stereo() {
        assert_close(mixed(1, 0x4, 2, &[0.5]), [0.5, 0.5]);
        assert_close(mixed(1, 0, 1, &[0.5]), [0.5, 0.0]);
        assert_close(mixed(2, 0x3, 2, &[0.1, 0.2]), [0.1, 0.2]);
        assert_close(mixed(2, 0, 1, &[0.1, 0.2]), [0.15, 0.0]);
        // 5.1 to mono: the stereo mix, averaged.
        let m = mixed(6, 0x3F, 1, &[0.1, 0.2, 0.3, 0.9, 0.4, 0.5]);
        assert_close(m, [(0.3 + H * (0.6 + 0.9)) * 0.5, 0.0]);
    }

    /// One 10 ms device period.
    const N: usize = 480;

    /// `n` frames of a 440 Hz tone on `ch` channels, from frame `*at` on.
    fn tone(at: &mut usize, n: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(n * ch);
        for _ in 0..n {
            let s = (0.5 * (*at as f64 * 440.0 * std::f64::consts::TAU / RATE as f64).sin()) as f32;
            v.extend(std::iter::repeat_n(s, ch));
            *at += 1;
        }
        v
    }

    /// The largest second difference of a mono signal: a pure tone's is
    /// tiny and steady, a skipped or repeated sample shows as a spike.
    fn roughness(x: &[f32]) -> f32 {
        x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
    }

    /// 440 Hz at 0.5: 0.5 * (2 pi 440 / 48000)^2 = 0.00166 per sample; a
    /// skip or repeat at the tone's steepest is ~0.029.
    const SMOOTH: f32 = 0.002;

    /// In steady state samples pass through untouched, once primed.
    #[test]
    fn steady_playback_is_bit_exact() {
        let mut p = Playout::new(2);
        let (mut at, mut sent, mut played) = (0, Vec::new(), Vec::new());
        let mut out = vec![0.0; N * 2];
        for _ in 0..300 {
            let chunk = tone(&mut at, N, 2);
            sent.extend_from_slice(&chunk);
            p.push(&chunk);
            let got = p.fill(&mut out);
            played.extend_from_slice(&out[..got]);
        }
        assert_eq!(played.len(), sent.len() - 2 * N * 2, "primed at the target, then one period each");
        assert!(played == sent[..played.len()]);
    }

    /// A burst that didn't run the queue dry is played away within seconds,
    /// back to the target, without a skipped or repeated sample.
    #[test]
    fn a_burst_is_played_away_smoothly() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        let mut level = 0;
        for i in 0..1500 {
            // 30 ms more than usual arrive at once after a second.
            p.push(&tone(&mut at, if i == 100 { N * 4 } else { N }, 1));
            level = p.level();
            let got = p.fill(&mut out);
            assert!(i < 2 || got == N, "ran dry at period {i}");
            played.extend_from_slice(&out[..got]);
        }
        assert!(level.abs_diff(frames(TARGET_MS)) <= frames(2), "level {level} frames");
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }

    /// Clock drift either way (~1000 ppm here, far more than real devices)
    /// keeps the queue near the target instead of climbing to the cap or
    /// running dry, and smoothly.
    #[test]
    fn drift_is_absorbed() {
        for extra in [1isize, -1] {
            let mut p = Playout::new(1);
            let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
            let (mut lo, mut hi) = (usize::MAX, 0);
            for i in 0..6000 {
                // One frame more (or fewer) every other period.
                let n = if i % 2 == 0 { (N as isize + extra) as usize } else { N };
                p.push(&tone(&mut at, n, 1));
                if i > 500 {
                    (lo, hi) = (lo.min(p.level()), hi.max(p.level()));
                }
                let got = p.fill(&mut out);
                // (Priming takes a period longer with the short pushes.)
                assert!(i < 3 || got == N, "ran dry at period {i} (drift {extra})");
                played.extend_from_slice(&out[..got]);
            }
            assert!(hi <= frames(TARGET_MS + SLACK_MS + 2), "drift {extra}: level rose to {hi} frames");
            assert!(lo >= frames(TARGET_MS - SLACK_MS - 2), "drift {extra}: level fell to {lo} frames");
            assert!(roughness(&played) < SMOOTH, "drift {extra}: roughness {}", roughness(&played));
        }
    }

    /// A new player starts at the target with the newest audio, not behind
    /// what queued while its device opened.
    #[test]
    fn a_startup_backlog_is_dropped() {
        let mut p = Playout::new(1);
        let mut out = vec![0.0; N];
        let ramp: Vec<f32> = (0..N * 6).map(|i| i as f32).collect();
        p.push(&ramp);
        assert_eq!(p.fill(&mut out), N);
        assert_eq!(out[0], (N * 3) as f32);
        assert_eq!(p.level(), frames(TARGET_MS) - N);
    }

    /// After running dry, playback resumes where it stopped: the burst a
    /// stall leaves is kept (room for the next stall like it) and played
    /// away back to the target, without running dry again.
    #[test]
    fn a_stall_backlog_is_kept_and_played_away() {
        let mut p = Playout::new(1);
        let mut out = vec![0.0; N];
        let ramp: Vec<f32> = (0..N * 9).map(|i| i as f32).collect();
        p.push(&ramp[..N * 3]);
        for _ in 0..3 {
            assert_eq!(p.fill(&mut out), N);
        }
        assert_eq!(p.fill(&mut out), 0, "dry");
        // A 60 ms burst after the stall: nothing of it is cut.
        p.push(&ramp[N * 3..]);
        assert_eq!(p.fill(&mut out), N);
        assert_eq!(out[0], (N * 3) as f32);
        assert_eq!(p.level(), N * 5);
        let mut level = 0;
        for i in 0..1500 {
            p.push(&[0.0; N]);
            level = p.level();
            assert_eq!(p.fill(&mut out), N, "ran dry at period {i}");
        }
        assert!(level.abs_diff(frames(TARGET_MS)) <= frames(2), "level {level} frames");
    }

    /// Bursts further apart than the target (here 40 ms of packets every
    /// 40 ms) play without gaps once the first has run dry (its start drops
    /// what's over the target): restarts keep what they need, and the queue
    /// is then stretched to hold its low point, seamlessly.
    #[test]
    fn slow_bursts_do_not_keep_running_dry() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        for i in 0..1000 {
            if i % 4 == 0 {
                p.push(&tone(&mut at, N * 4, 1));
            }
            let got = p.fill(&mut out);
            assert!(i < 4 || got == N, "ran dry at period {i}");
            played.extend_from_slice(&out[..got]);
        }
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }

    /// A backlog past the cap is cut back to the target as it arrives.
    #[test]
    fn the_queue_is_capped() {
        let mut p = Playout::new(2);
        p.push(&vec![0.0; frames(100) * 2]);
        assert_eq!(p.level(), frames(TARGET_MS));
    }

    /// A lower target (a calm network measured) is reached within seconds,
    /// smoothly and without running dry; a higher one (jitter) the same way.
    #[test]
    fn the_target_moves_smoothly() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        for (ms, from) in [(12, 0), (45, 1000)] {
            let mut level = 0;
            for i in from..from + 1000 {
                if i == from + 100 {
                    p.target = frames(ms);
                }
                p.push(&tone(&mut at, N, 1));
                level = p.level();
                let got = p.fill(&mut out);
                assert!(i < 2 || got == N, "ran dry at period {i}");
                played.extend_from_slice(&out[..got]);
            }
            assert!(level.abs_diff(frames(ms)) <= frames(2), "level {level} frames for {ms} ms");
        }
        assert_eq!(p.underruns, 0);
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }

    /// On real devices, without a sound or touching the defaults: a tone
    /// played into the virtual speakers (by id) comes back through their
    /// loopback, and one played into the virtual mic's input comes out of
    /// the mic. `cargo test -p rdp-client wasapi -- --ignored --nocapture`
    #[test]
    #[ignore = "needs Steam Streaming Speakers / Microphone"]
    fn virtual_devices_round_trip() {
        com_init();
        // Not while they're in use: the default output (a stream may be
        // on), or a mic some app is recording from.
        let speakers = find(Flow::Render, "Steam Streaming Speakers")
            .filter(|e| default_endpoint(Flow::Render).is_none_or(|d| d.id != e.id));
        let mic = find(Flow::Render, "Steam Streaming Microphone")
            .zip(find(Flow::Capture, "Steam Streaming Microphone"))
            .filter(|(_, m)| !crate::hyper_audio::endpoint_has_listeners(&m.id));
        for (label, sink, source, loopback, ch) in [
            ("virtual speakers -> loopback", speakers.clone(), speakers, true, 2),
            ("virtual mic input -> mic", mic.as_ref().map(|m| m.0.clone()), mic.map(|m| m.1), false, 1),
        ] {
            let (Some(sink), Some(source)) = (sink, source) else {
                eprintln!("{label}: device missing or in use; skipped");
                continue;
            };
            let player = Player::new(Some(sink.id.clone()), ch).expect("player");
            let flow = if loopback { Flow::Render } else { Flow::Capture };
            let mut cap = Capture::new(Some(&source.id), flow, loopback, ch).expect("capture");
            let (mut at, mut got, start) = (0, Vec::new(), Instant::now());
            while start.elapsed() < Duration::from_millis(1500) {
                // Keep ~40 ms queued whatever the sleeps do.
                while at < (start.elapsed().as_secs_f64() * RATE as f64) as usize + frames(40) {
                    player.push(&tone(&mut at, N, ch));
                }
                std::thread::sleep(Duration::from_millis(10));
                cap.read(&mut got).expect("read");
                assert!(player.is_alive(), "{label}: the render thread ended");
            }
            let tail = &got[got.len().saturating_sub(RATE as usize / 2 * ch)..];
            let rms = (tail.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / tail.len().max(1) as f64).sqrt();
            eprintln!("{label}: device format {:?}, {} samples captured, rms {rms:.3} (sent 0.354)", cap.format, got.len());
            assert!(rms > 0.1, "{label}: the tone didn't come back (rms {rms:.3})");
        }
    }

    /// On real devices: a player and a loopback capture that follow the
    /// default output don't take an unchanged default for a new one. The
    /// player stays silent (nothing is pushed).
    /// While audio plays, event-driven loopback gets its data as each period
    /// is captured (an inaudible tone into an output nobody uses: S/PDIF).
    #[test]
    #[ignore = "needs a Realtek Digital Output"]
    fn event_driven_loopback_hears_what_plays() {
        com_init();
        let out = find(Flow::Render, "Realtek Digital Output").expect("Realtek Digital Output");
        let player = Player::new(Some(out.id.clone()), 2).expect("player");
        let mut cap = Capture::new(Some(&out.id), Flow::Render, true, 2).expect("loopback");
        assert!(cap.event_driven());
        let (start, mut got, mut phase) = (std::time::Instant::now(), Vec::new(), 0f32);
        let mut waits = 0;
        while start.elapsed() < Duration::from_secs(1) {
            let tone: Vec<f32> = (0..960).flat_map(|_| {
                phase = (phase + 440.0 / RATE as f32) % 1.0;
                let s = (phase * std::f32::consts::TAU).sin() * 0.001;
                [s, s]
            }).collect();
            player.push(&tone);
            cap.wait(Duration::from_millis(5));
            waits += 1;
            cap.read(&mut got).unwrap();
        }
        eprintln!("{waits} waits in 1 s, {} samples captured", got.len());
        assert!(got.len() > RATE as usize, "loopback heard too little: {}", got.len());
        assert!(waits > 50, "the engine's events didn't come ({waits} waits in a second)");
    }

    /// Loopback capture of the default output reads as the engine signals,
    /// not on a polling timer. (Leaves every device setting alone.)
    #[test]
    #[ignore = "needs an audio output"]
    fn loopback_capture_is_event_driven() {
        com_init();
        let mut cap = Capture::new(None, Flow::Render, true, 2).expect("loopback capture");
        assert!(cap.event_driven(), "the engine refused event-driven capture");
        let (start, mut got) = (std::time::Instant::now(), Vec::new());
        for _ in 0..20 {
            cap.wait(Duration::from_millis(5));
            cap.read(&mut got).unwrap();
        }
        eprintln!("20 waits in {:?}, {} samples", start.elapsed(), got.len());
    }

    #[test]
    #[ignore = "needs a default output device"]
    fn following_the_default_output_is_stable() {
        com_init();
        let player = Player::new(None, 2).expect("player");
        let mut cap = Capture::new(None, Flow::Render, true, 2).expect("loopback");
        let mut got = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(2500) {
            std::thread::sleep(Duration::from_millis(50));
            cap.read(&mut got).expect("loopback read");
            assert!(!cap.default_changed(), "the capture saw a new default");
            assert!(player.is_alive(), "the player saw a new default, or failed");
        }
        eprintln!("default output {:?}: format {:?}", default_endpoint(Flow::Render).map(|e| e.name), cap.format);
    }
}
