//! The tablet's microphone and camera, for whichever session asks for them
//! (a Hyper host's apps, or a Cloud PC's).
//!
//! - **Mic** ([`Mic`]): AAudio input with the voice-call preset, so the
//!   tablet's echo canceller keeps the session's own sound (playing from the
//!   tablet's speakers) out of it, and its noise suppression runs.
//! - **Camera**: captured and H.264-encoded by the app (Camera2 into a
//!   MediaCodec encoder's input Surface: no copy of a picture anywhere). A
//!   session asks for it with [`want_camera`]; the app polls [`camera_wanted`]
//!   and hands each encoded frame to [`camera_frame`], which passes it to the
//!   session's sink.
//!
//! Both need the user's permission, which only the app can ask for: a session
//! wanting one that isn't granted yet sets the matching `*_WANTED` flag, the
//! app asks, then says so with [`set_permissions`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder,
};

/// The user let the app use the microphone / the camera.
pub static MIC_ALLOWED: AtomicBool = AtomicBool::new(false);
pub static CAMERA_ALLOWED: AtomicBool = AtomicBool::new(false);
/// A session wants it (the app asks for the permission while it isn't granted).
pub static MIC_WANTED: AtomicBool = AtomicBool::new(false);
/// Cameras this tablet has (the app counts them at start).
pub static CAMERA_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn set_permissions(mic: bool, camera: bool) {
    MIC_ALLOWED.store(mic, Ordering::Relaxed);
    CAMERA_ALLOWED.store(camera, Ordering::Relaxed);
}

/// Captured samples, shared with AAudio's callback.
struct MicBuf {
    samples: Mutex<VecDeque<f32>>,
    ready: Condvar,
    /// Samples a waiting [`Mic::take`] needs before it's worth waking
    /// (`usize::MAX`: nobody waits). AAudio's low-latency bursts are 1-2 ms,
    /// a frame of Opus 10 or 20: waking per frame, not per burst, is a tenth
    /// of the wakeups (and no futex call at all for a consumer that never
    /// waits, like the RDP session's). Changed only under `samples`' lock.
    want: AtomicUsize,
    dead: AtomicBool,
    /// Callbacks, and the wakeups they made (for the log).
    bursts: AtomicU32,
    wakes: AtomicU32,
}

/// An open microphone. Captures until dropped.
pub struct Mic {
    _stream: AudioStream,
    buf: Arc<MicBuf>,
    channels: usize,
}

impl Mic {
    /// Open the default microphone at `rate` Hz, `channels` (Android converts
    /// from what the hardware does).
    pub fn open(rate: u32, channels: u16) -> Result<Self, String> {
        Self::open_with(rate, channels, None)
    }

    /// [`Mic::open`], calling `wake` as each burst of sound comes in (for a
    /// consumer that waits on something else, like the RDP session worker).
    pub fn open_with(rate: u32, channels: u16, wake: Option<fn()>) -> Result<Self, String> {
        if !MIC_ALLOWED.load(Ordering::Relaxed) {
            MIC_WANTED.store(true, Ordering::Relaxed);
            return Err("no permission for the microphone (yet)".into());
        }
        let channels = channels.clamp(1, 2) as usize;
        let buf = Arc::new(MicBuf {
            samples: Mutex::new(VecDeque::with_capacity(rate as usize)),
            ready: Condvar::new(),
            want: AtomicUsize::new(usize::MAX),
            dead: AtomicBool::new(false),
            bursts: AtomicU32::new(0),
            wakes: AtomicU32::new(0),
        });
        let (cb_buf, err_buf) = (buf.clone(), buf.clone());
        // At most a second kept: a stalled consumer loses the oldest sound.
        let cap = rate as usize * channels;
        let stream = AudioStreamBuilder::new()
            .map_err(|e| format!("{e:?}"))?
            .direction(AudioDirection::Input)
            .format(AudioFormat::PCM_Float)
            .sample_rate(rate as i32)
            .channel_count(channels as i32)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sharing_mode(AudioSharingMode::Shared)
            .input_preset(AudioInputPreset::VoiceCommunication)
            .data_callback(Box::new(move |_s, data, frames| {
                let pcm = unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize * channels) };
                let mut q = cb_buf.samples.lock().unwrap();
                q.extend(pcm);
                if q.len() > cap {
                    let extra = q.len() - cap;
                    q.drain(..extra);
                }
                let wake_taker = q.len() >= cb_buf.want.load(Ordering::Relaxed);
                drop(q);
                cb_buf.bursts.fetch_add(1, Ordering::Relaxed);
                if wake_taker {
                    cb_buf.wakes.fetch_add(1, Ordering::Relaxed);
                    cb_buf.ready.notify_one();
                }
                if let Some(wake) = wake {
                    wake();
                }
                AudioCallbackResult::Continue
            }))
            .error_callback(Box::new(move |_s, e| {
                log::warn!("microphone error: {e:?}");
                err_buf.dead.store(true, Ordering::Relaxed);
                err_buf.ready.notify_one();
            }))
            .open_stream()
            .map_err(|e| format!("open: {e:?}"))?;
        stream.request_start().map_err(|e| format!("start: {e:?}"))?;
        log::info!("microphone open: {} Hz, {channels} ch", stream.sample_rate());
        MIC_WANTED.store(false, Ordering::Relaxed);
        Ok(Self { _stream: stream, buf, channels })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Move what's been captured into `out` (interleaved), waiting up to
    /// `wait` for some when there's none.
    pub fn take(&self, out: &mut Vec<f32>, wait: Duration) {
        self.take_at_least(out, 1, wait);
    }

    /// [`Mic::take`], waiting (up to `wait`) until there are `min` samples (0:
    /// no wait): one wakeup per frame of the consumer's, not one per burst of
    /// AAudio's.
    pub fn take_at_least(&self, out: &mut Vec<f32>, min: usize, wait: Duration) {
        let mut q = self.buf.samples.lock().unwrap();
        if q.len() < min && !self.dead() {
            self.buf.want.store(min, Ordering::Relaxed);
            q = self.buf.ready.wait_timeout(q, wait).unwrap().0;
            self.buf.want.store(usize::MAX, Ordering::Relaxed);
        }
        out.extend(q.drain(..));
    }

    /// AAudio callbacks so far, and the wakeups of a waiting [`Mic::take`]
    /// they made.
    pub fn bursts(&self) -> (u32, u32) {
        (self.buf.bursts.load(Ordering::Relaxed), self.buf.wakes.load(Ordering::Relaxed))
    }

    /// The device went away (a headset unplugged): open it again.
    pub fn dead(&self) -> bool {
        self.buf.dead.load(Ordering::Relaxed)
    }
}

// ---- Camera ----

/// What a session asks the camera for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// Takes each encoded frame: Annex-B H.264 (parameter sets before each
/// keyframe), whether it's a keyframe.
pub type CameraSink = Box<dyn FnMut(&[u8], bool) + Send>;

struct CameraState {
    want: Option<CameraRequest>,
    sink: Option<CameraSink>,
    /// Bumped for each keyframe asked for (the app compares).
    keyframes: u32,
    /// Bumped for each new request (so the app restarts the encoder even
    /// when the same size is asked for again).
    generation: u32,
}

static CAMERA: Mutex<CameraState> = Mutex::new(CameraState { want: None, sink: None, keyframes: 0, generation: 0 });

/// Stop the camera if request `generation` (from [`want_camera`]) is still
/// the current one: a later request from another session keeps it running.
pub fn release_camera(generation: u32) {
    if CAMERA.lock().unwrap().generation == generation {
        want_camera(None);
    }
}

/// Start sending the camera to `sink` (or stop, with `None`). Returns the
/// request's generation.
pub fn want_camera(want: Option<(CameraRequest, CameraSink)>) -> u32 {
    let mut c = CAMERA.lock().unwrap();
    match want {
        Some((req, sink)) => {
            c.want = Some(req);
            c.sink = Some(sink);
            c.generation = c.generation.wrapping_add(1);
            log::info!("camera wanted: {}x{} @ {} fps", req.width, req.height, req.fps);
        }
        None => {
            if c.want.take().is_some() {
                log::info!("camera released");
            }
            c.sink = None;
        }
    }
    c.generation
}

pub fn camera_keyframe() {
    let mut c = CAMERA.lock().unwrap();
    c.keyframes = c.keyframes.wrapping_add(1);
}

/// For the app: `width,height,fps,bitrate,generation,keyframes`, or "" when
/// no session wants the camera.
pub fn camera_wanted() -> String {
    let c = CAMERA.lock().unwrap();
    c.want.map_or_else(String::new, |r| format!("{},{},{},{},{},{}", r.width, r.height, r.fps, r.bitrate, c.generation, c.keyframes))
}

/// From the app: one encoded frame.
pub fn camera_frame(data: &[u8], keyframe: bool) {
    let mut c = CAMERA.lock().unwrap();
    if let Some(sink) = c.sink.as_mut() {
        sink(data, keyframe);
    }
}
