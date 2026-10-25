//! Hardware video decoding with MediaCodec, straight onto the screen's Surface.
//!
//! Frames go in as the host's Annex-B access units (parameter sets travel in
//! band with each keyframe); decoded pictures are released to the Surface the
//! moment they come out — no copy, no wait for vsync on our side. The
//! low-latency keys stop the decoder holding pictures back for reordering
//! (the host never sends B-frames).
//!
//! Asynchronous mode: the codec's own callbacks say which input buffer is
//! free and which picture is out, instead of each side asking it (each ask a
//! round trip through the codec's message thread — about 13 wakeups a frame
//! there, polled). (`debug.rdpio.asynccodec=0`: polled; a switch remakes the
//! decoder.)
//!
//! After a stall, pictures come out in a clump (a relayed TCP path delivers
//! a second's frames at once): each is decoded (the next ones refer to it)
//! but only the newest is shown — those behind it are let go unshown, and
//! each shown one is stamped "now", so the display drops one it hasn't
//! shown yet when a newer one lands before the next refresh (stamped with
//! the host's capture time, as before, every one was shown, a refresh each,
//! a clump of 30 adding up to half a second at 60 Hz). (`debug.rdpio.newest=0`:
//! every picture shown, as before; `debug.rdpio.newestsf=0`: the drop here
//! only, not the display's.)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, RwLock};
use std::time::{Duration, Instant};

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk::media::media_codec::{AsyncNotifyCallback, DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "h264" => Some(Self::H264),
            "hevc" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::H264 => "video/avc",
            Self::Hevc => "video/hevc",
            Self::Av1 => "video/av01",
        }
    }

    fn bit(self) -> u32 {
        match self {
            Self::H264 => 1,
            Self::Hevc => 2,
            Self::Av1 => 4,
        }
    }
}

/// Android's own software decoders (what `from_decoder_type` hands out when
/// there's no hardware one).
fn is_software(name: &str) -> bool {
    name.starts_with("c2.android.") || name.starts_with("OMX.google.") || name.contains(".sw.")
}

/// Codecs this device decodes in hardware, best first (as `Want.codecs`);
/// H.264 even in software when nothing else is there. AV1 (the Snapdragon
/// decodes it; the PC's NVENC encodes it: 3-5 dB better than HEVC at the
/// same bitrate) only with `debug.rdpio.av1=1`, read at connect: every loss
/// is mended with a keyframe (no long-term references for AV1 yet).
pub fn device_codecs() -> Vec<String> {
    let av1 = crate::tune::ab("av1", false).then_some(Codec::Av1);
    let hw: Vec<String> = av1
        .into_iter()
        .chain([Codec::Hevc, Codec::H264])
        .filter(|c| default_name(*c).is_some_and(|n| !is_software(&n)))
        .map(|c| c.name().to_string())
        .collect();
    if hw.is_empty() {
        vec![Codec::H264.name().to_string()]
    } else {
        hw
    }
}

/// The component `from_decoder_type` hands out for `kind` (asked once: each
/// ask makes and frees a component, tens of ms on a Snapdragon).
fn default_name(kind: Codec) -> Option<String> {
    static NAMES: Mutex<Vec<(Codec, Option<String>)>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap();
    if let Some((_, n)) = names.iter().find(|(k, _)| *k == kind) {
        return n.clone();
    }
    let n = MediaCodec::from_decoder_type(kind.mime()).and_then(|m| m.name().ok());
    names.push((kind, n.clone()));
    n
}

/// Codecs (bits) with no low-latency component to use: the device has none,
/// or it failed us once.
static NO_LOW_LATENCY: AtomicU32 = AtomicU32::new(0);

/// Qualcomm's separate low-latency component for `kind`
/// (`c2.qti.hevc.decoder.low_latency` beside `c2.qti.hevc.decoder` on recent
/// Snapdragons), by name. (`debug.rdpio.qtilowlat=0`: the standard one.)
fn low_latency_name(kind: Codec) -> Option<String> {
    if !crate::tune::ab("qtilowlat", true) || NO_LOW_LATENCY.load(Ordering::Relaxed) & kind.bit() != 0 {
        return None;
    }
    let name = default_name(kind)?;
    (name.starts_with("c2.qti.") && !name.ends_with(".low_latency")).then(|| format!("{name}.low_latency"))
}

/// A codec component for `kind`, not configured yet: the low-latency one
/// when asked for and the device has it, else the standard one.
fn component(kind: Codec, low_latency: bool) -> Option<MediaCodec> {
    if low_latency {
        if let Some(name) = low_latency_name(kind) {
            if let Some(c) = MediaCodec::from_codec_name(&name) {
                return Some(c);
            }
            log::info!("decoder: no {name} on this device; the standard component");
            NO_LOW_LATENCY.fetch_or(kind.bit(), Ordering::Relaxed);
        }
    }
    MediaCodec::from_decoder_type(kind.mime())
}

/// A component made ahead (see [`prewarm`]).
struct Warmed(MediaCodec);
// (Made on the helper thread, then only ever used by the one that takes it.)
unsafe impl Send for Warmed {}

enum Warm {
    Making(Codec),
    Ready(Codec, Warmed, Instant),
}

static WARM: Mutex<Option<Warm>> = Mutex::new(None);
static WARM_DONE: Condvar = Condvar::new();

/// Make the decoder's component for `kind` on a helper thread while the
/// sign-in runs (codec2 allocates a component in tens of ms; the size to
/// configure it for comes later): [`Decoder::new`] takes it instead of making
/// one. (`debug.rdpio.prewarm=0`: made once the stream's size is known.)
/// Also lists the device's video decoders in the log, once.
pub fn prewarm(kind: Option<Codec>) {
    let warm = kind.filter(|_| crate::tune::ab("prewarm", true));
    if let Some(k) = warm {
        *WARM.lock().unwrap() = Some(Warm::Making(k));
    }
    let spawned = std::thread::Builder::new().name("decoder-warm".into()).spawn(move || {
        if let Some(k) = warm {
            let t = Instant::now();
            let made = component(k, true);
            log::info!(
                "decoder {} made ahead in {:.1} ms",
                made.as_ref().and_then(|c| c.name().ok()).unwrap_or_else(|| "(none)".into()),
                t.elapsed().as_secs_f64() * 1000.0
            );
            let mut slot = WARM.lock().unwrap();
            // (Given up on or dropped meanwhile: not wanted any more.)
            if matches!(*slot, Some(Warm::Making(_))) {
                *slot = made.map(|c| Warm::Ready(k, Warmed(c), Instant::now()));
            }
            WARM_DONE.notify_all();
        }
        log_codec_list();
    });
    if spawned.is_err() {
        drop_warm();
    }
}

/// Free a component made ahead and not taken (the sign-in failed).
pub fn drop_warm() {
    WARM.lock().unwrap().take();
    WARM_DONE.notify_all();
}

/// The component made ahead for `kind`, if any (waiting a little for one
/// still being made).
fn take_warm(kind: Codec) -> Option<MediaCodec> {
    let mut slot = WARM.lock().unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while matches!(*slot, Some(Warm::Making(k)) if k == kind) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        slot = WARM_DONE.wait_timeout(slot, deadline - now).unwrap().0;
    }
    match slot.take() {
        // (Older: it sat through something else; a fresh one is cheap next to that.)
        Some(Warm::Ready(k, c, at)) if k == kind && at.elapsed() < Duration::from_secs(60) => Some(c.0),
        _ => None,
    }
}

/// The device's video decoders by name (its media_codecs*.xml), once: what
/// the morning check reads.
fn log_codec_list() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut names: Vec<String> = Vec::new();
        for dir in ["/vendor/etc", "/odm/etc", "/system/etc", "/apex/com.android.media.swcodec/etc"] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for e in entries.flatten() {
                let file = e.file_name().to_string_lossy().into_owned();
                if !file.starts_with("media_codecs") || !file.ends_with(".xml") || file.contains("performance") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
                for tag in text.split("<MediaCodec ").skip(1) {
                    let Some(name) = tag.split("name=\"").nth(1).and_then(|r| r.split('"').next()) else { continue };
                    let l = name.to_ascii_lowercase();
                    let video = ["avc", "hevc", "av1", "h264", "h265"].iter().any(|k| l.contains(k));
                    if video && l.contains("dec") && !names.iter().any(|n| n == name) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        if names.is_empty() {
            log::info!("video decoders on this device: (codec list not readable)");
        } else {
            log::info!("video decoders on this device: {}", names.join(", "));
        }
    });
}

pub struct Decoder {
    codec: MediaCodec,
    /// Cleared by [`Decoder::release`]: every call holds it for reading, so
    /// a release waits out the call in progress and the Surface can go.
    alive: RwLock<bool>,
    pub width: u32,
    pub height: u32,
    pub kind: Codec,
    /// The component's name.
    pub name: String,
    /// What the codec's callbacks hand over, in asynchronous mode.
    events: Option<Arc<Events>>,
    /// While there's no Surface on screen: the placeholder it decodes onto
    /// (see [`Decoder::park`]), and since when. (After `codec`: it outlives
    /// the codec's use of it.)
    parked: Mutex<Option<(ImageReader, Instant)>>,
    /// Frames queued and pictures out, ever (shown, and let go unshown
    /// behind a newer one).
    queued: AtomicU64,
    shown: AtomicU64,
    stale: AtomicU64,
}

/// Buffer indices from the codec's callbacks (asynchronous mode).
#[derive(Default)]
struct Events {
    inputs: Mutex<VecDeque<usize>>,
    input_ready: Condvar,
    /// (index, presentation time)
    outputs: Mutex<VecDeque<(usize, u64)>>,
    output_ready: Condvar,
}

/// The front of `q` once there is one, waiting up to `wait`.
fn pop_within<T>(mut q: MutexGuard<'_, VecDeque<T>>, ready: &Condvar, wait: Duration) -> Option<T> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(v) = q.pop_front() {
            return Some(v);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        q = ready.wait_timeout(q, deadline - now).unwrap().0;
    }
}

// AMediaCodec may be fed from one thread while another takes its output (the
// documented way to drive it); `alive` keeps release exclusive.
unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

/// A picture came out of the decoder (and went to the screen).
pub struct Shown {
    pub pts: u64,
    /// Decoded with it and let go unshown (older): their times.
    pub skipped: Vec<u64>,
}

/// Show only the newest of the pictures out (see the module's notes).
fn newest() -> bool {
    crate::tune::ab("newest", true)
}

/// CLOCK_MONOTONIC (the display's clock), ns.
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

impl Decoder {
    /// `rate`: the frames a second it must keep up with, or None for flat out.
    pub fn new(window: &NativeWindow, kind: Codec, width: u32, height: u32, rate: Option<u32>) -> Result<Self, String> {
        // With every low-latency key first; a decoder that refuses one of
        // them is configured again with only the standard ones. Qualcomm's
        // low-latency component first when there is one, the standard one
        // when that fails both ways.
        let asynchronous = Self::wants_async();
        let started = Instant::now();
        let mut warm = take_warm(kind);
        let warmed = warm.is_some();
        let mut last = String::new();
        for low_latency in [true, false] {
            let mut was_low = false;
            for tuned in [true, false] {
                let codec = match warm.take() {
                    Some(c) => c,
                    None => component(kind, low_latency).ok_or_else(|| format!("no {} decoder on this device", kind.name()))?,
                };
                was_low |= codec.name().is_ok_and(|n| n.ends_with(".low_latency"));
                match Self::configure(codec, window, kind, width, height, tuned, rate, asynchronous) {
                    Ok(d) => {
                        log::info!(
                            "decoder ready in {:.1} ms ({})",
                            started.elapsed().as_secs_f64() * 1000.0,
                            if warmed { "component made ahead" } else { "component made now" }
                        );
                        return Ok(d);
                    }
                    Err(e) if tuned => log::warn!("{e}; trying again without the vendor keys"),
                    Err(e) => last = e,
                }
            }
            if !was_low {
                break;
            }
            log::warn!("{last}; the standard component instead of the low-latency one");
            NO_LOW_LATENCY.fetch_or(kind.bit(), Ordering::Relaxed);
        }
        Err(last)
    }

    /// Its keyframes keep failing: a low-latency component isn't tried
    /// again (the next decoder is the standard one).
    pub fn distrust(&self) {
        if self.name.ends_with(".low_latency") && NO_LOW_LATENCY.fetch_or(self.kind.bit(), Ordering::Relaxed) & self.kind.bit() == 0 {
            log::warn!("decoder {} fails keyframes; the standard component from now on", self.name);
        }
    }

    /// Whether new decoders run asynchronously (see the module's notes).
    pub fn wants_async() -> bool {
        crate::tune::ab("asynccodec", true)
    }

    pub fn is_async(&self) -> bool {
        self.events.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    fn configure(
        mut codec: MediaCodec,
        window: &NativeWindow,
        kind: Codec,
        width: u32,
        height: u32,
        tuned: bool,
        rate: Option<u32>,
        asynchronous: bool,
    ) -> Result<Self, String> {
        // (Before `configure`, as the API wants.)
        let events = asynchronous.then(|| Arc::new(Events::default()));
        if let Some(ev) = &events {
            let (i, o) = (ev.clone(), ev.clone());
            let callbacks = AsyncNotifyCallback {
                on_input_available: Some(Box::new(move |index| {
                    i.inputs.lock().unwrap().push_back(index);
                    i.input_ready.notify_one();
                })),
                on_output_available: Some(Box::new(move |index, info| {
                    o.outputs.lock().unwrap().push_back((index, info.presentation_time_us() as u64));
                    o.output_ready.notify_one();
                })),
                on_format_changed: Some(Box::new(|format| log::info!("decoder output format: {format}"))),
                on_error: Some(Box::new(|e, action, detail| log::warn!("decoder error: {e:?} ({action:?}) {detail:?}"))),
            };
            codec.set_async_notify_callback(Some(callbacks)).map_err(|e| format!("asynchronous {}: {e:?}", kind.name()))?;
        }
        let mut format = MediaFormat::new();
        format.set_str("mime", kind.mime());
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
        // A keyframe of a busy 4K desktop can be a few megabytes.
        format.set_i32("max-input-size", (width * height).max(2 << 20) as i32);
        // Android 11+: output each picture as soon as it's decoded.
        format.set_i32("low-latency", 1);
        // Realtime priority.
        format.set_i32("priority", 0);
        if tuned {
            // "As fast as you can": the decoder otherwise clocks itself for
            // the stream's frame rate, a frame behind. (Office mode: twice
            // the rate — each picture done in half a frame, at lower clocks.)
            format.set_i32("operating-rate", rate.map_or(i16::MAX as i32, |r| (r * 2).min(i16::MAX as u32) as i32));
            // Qualcomm's own low-latency switches (Snapdragon tablets and
            // phones); other decoders ignore keys they don't know.
            format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
            format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        }
        codec.configure(&format, Some(window), MediaCodecDirection::Decoder).map_err(|e| format!("configure {}: {e:?}", kind.name()))?;
        codec.start().map_err(|e| format!("start {}: {e:?}", kind.name()))?;
        let name = codec.name().unwrap_or_default();
        log::info!(
            "decoder {name} ({}) {width}x{height}{}, {}{}",
            kind.name(),
            if tuned { "" } else { " (standard keys)" },
            rate.map_or("flat out".to_string(), |r| format!("clocked for {} fps", r * 2)),
            if events.is_some() { ", asynchronous" } else { "" }
        );
        Ok(Self {
            codec,
            alive: RwLock::new(true),
            width,
            height,
            kind,
            name,
            events,
            parked: Mutex::new(None),
            queued: AtomicU64::new(0),
            shown: AtomicU64::new(0),
            stale: AtomicU64::new(0),
        })
    }

    /// Queue one access unit. `Ok(false)`: the decoder had no room within
    /// `wait` (it's behind).
    pub fn queue(&self, data: &[u8], pts: u64, wait: Duration) -> Result<bool, String> {
        let alive = self.alive.read().unwrap();
        if !*alive {
            return Err("released".into());
        }
        if let Some(ev) = &self.events {
            let Some(index) = pop_within(ev.inputs.lock().unwrap(), &ev.input_ready, wait) else { return Ok(false) };
            let Some(dst) = self.codec.input_buffer(index) else { return Err(format!("no input buffer {index}")) };
            if dst.len() < data.len() {
                let room = dst.len();
                let _ = self.codec.queue_input_buffer_by_index(index, 0, 0, pts, 0);
                return Err(format!("frame of {} bytes > input buffer {room}", data.len()));
            }
            // SAFETY: MaybeUninit<u8> has u8's layout; we write all of it.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst.as_mut_ptr() as *mut u8, data.len()) };
            self.codec.queue_input_buffer_by_index(index, 0, data.len(), pts, 0).map_err(|e| format!("queue input: {e:?}"))?;
            self.queued.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        match self.codec.dequeue_input_buffer(wait).map_err(|e| format!("dequeue input: {e:?}"))? {
            DequeuedInputBufferResult::TryAgainLater => Ok(false),
            DequeuedInputBufferResult::Buffer(mut buf) => {
                let dst = buf.buffer_mut();
                let room = dst.len();
                if room < data.len() {
                    // Hand the buffer back empty rather than keep it.
                    let _ = self.codec.queue_input_buffer(buf, 0, 0, pts, 0);
                    return Err(format!("frame of {} bytes > input buffer {room}", data.len()));
                }
                // SAFETY: MaybeUninit<u8> has u8's layout; we write all of it.
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst.as_mut_ptr() as *mut u8, data.len()) };
                self.codec.queue_input_buffer(buf, 0, data.len(), pts, 0).map_err(|e| format!("queue input: {e:?}"))?;
                self.queued.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }
    }

    /// The next decoded picture, rendered to the Surface at once.
    pub fn render_next(&self, wait: Duration) -> Result<Option<Shown>, String> {
        let alive = self.alive.read().unwrap();
        if !*alive {
            std::thread::sleep(wait);
            return Ok(None);
        }
        let newest = newest();
        let stamp = newest && crate::tune::ab("newestsf", true);
        if let Some(ev) = &self.events {
            let Some((mut index, mut pts)) = pop_within(ev.outputs.lock().unwrap(), &ev.output_ready, wait) else { return Ok(None) };
            let mut skipped = Vec::new();
            if newest {
                // Newer ones already out: this one goes unshown. (Taken
                // under the lock, let go outside it: the codec's callback
                // thread pushes under it.)
                let newer: Vec<(usize, u64)> = ev.outputs.lock().unwrap().drain(..).collect();
                for (i, p) in newer {
                    self.codec.release_output_buffer_by_index(index, false).map_err(|e| format!("release output: {e:?}"))?;
                    skipped.push(pts);
                    (index, pts) = (i, p);
                }
            }
            if stamp {
                self.codec.release_output_buffer_at_time_by_index(index, monotonic_ns()).map_err(|e| format!("release output: {e:?}"))?;
            } else {
                self.codec.release_output_buffer_by_index(index, true).map_err(|e| format!("release output: {e:?}"))?;
            }
            self.shown.fetch_add(1, Ordering::Relaxed);
            self.stale.fetch_add(skipped.len() as u64, Ordering::Relaxed);
            return Ok(Some(Shown { pts, skipped }));
        }
        match self.codec.dequeue_output_buffer(wait).map_err(|e| format!("dequeue output: {e:?}"))? {
            DequeuedOutputBufferInfoResult::Buffer(mut buf) => {
                let mut pts = buf.info().presentation_time_us() as u64;
                let mut skipped = Vec::new();
                while newest {
                    let DequeuedOutputBufferInfoResult::Buffer(next) = self.codec.dequeue_output_buffer(Duration::ZERO).map_err(|e| format!("dequeue output: {e:?}"))? else { break };
                    self.codec.release_output_buffer(buf, false).map_err(|e| format!("release output: {e:?}"))?;
                    skipped.push(pts);
                    pts = next.info().presentation_time_us() as u64;
                    buf = next;
                }
                if stamp {
                    self.codec.release_output_buffer_at_time(buf, monotonic_ns()).map_err(|e| format!("release output: {e:?}"))?;
                } else {
                    self.codec.release_output_buffer(buf, true).map_err(|e| format!("release output: {e:?}"))?;
                }
                self.shown.fetch_add(1, Ordering::Relaxed);
                self.stale.fetch_add(skipped.len() as u64, Ordering::Relaxed);
                Ok(Some(Shown { pts, skipped }))
            }
            DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                log::info!("decoder output format: {}", self.codec.output_format());
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

impl Decoder {
    /// Frames queued and pictures shown so far.
    pub fn counts(&self) -> (u64, u64) {
        (self.queued.load(Ordering::Relaxed), self.shown.load(Ordering::Relaxed))
    }
    /// Keep the decoder while its Surface goes away: its pictures go to a
    /// placeholder that throws them away (an ImageReader) until
    /// [`Decoder::unpark`] gives it the next Surface — no new component, no
    /// configure, and no keyframe to wait for when no frame was skipped.
    /// Returns once the codec no longer draws on the old Surface.
    pub fn park(&self) -> Result<(), String> {
        let alive = self.alive.read().unwrap();
        if !*alive {
            return Err("released".into());
        }
        let mut reader = ImageReader::new_with_usage(self.width as i32, self.height as i32, ImageFormat::PRIVATE, HardwareBufferUsage::GPU_SAMPLED_IMAGE, 4)
            .map_err(|e| format!("placeholder: {e:?}"))?;
        // (Each picture let go at once: the codec never runs out of buffers.)
        reader
            .set_image_listener(Box::new(|r: &ImageReader| {
                while let Ok(AcquireResult::Image(image)) = r.acquire_next_image() {
                    drop(image);
                }
            }))
            .map_err(|e| format!("placeholder listener: {e:?}"))?;
        let window = reader.window().map_err(|e| format!("placeholder surface: {e:?}"))?;
        self.codec.set_output_surface(&window).map_err(|e| format!("park: {e:?}"))?;
        *self.parked.lock().unwrap() = Some((reader, Instant::now()));
        Ok(())
    }

    /// Decode onto `window` from now on (after [`Decoder::park`]).
    pub fn unpark(&self, window: &NativeWindow) -> Result<(), String> {
        let alive = self.alive.read().unwrap();
        if !*alive {
            return Err("released".into());
        }
        self.codec.set_output_surface(window).map_err(|e| format!("unpark: {e:?}"))?;
        self.parked.lock().unwrap().take();
        Ok(())
    }

    /// How long it has gone without a Surface on screen, while parked.
    pub fn parked_for(&self) -> Option<Duration> {
        self.parked.lock().unwrap().as_ref().map(|(_, at)| at.elapsed())
    }

    /// Stop decoding now (its Surface is going away), whoever else still
    /// holds it; their calls fail from here on.
    pub fn release(&self) {
        let mut alive = self.alive.write().unwrap();
        if std::mem::replace(&mut *alive, false) {
            let _ = self.codec.stop();
            self.parked.lock().unwrap().take();
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.release();
    }
}
