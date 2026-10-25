//! A Cloud PC's sound (RDPSND PCM) to AAudio, and the tablet's microphone
//! for its apps (MS-RDPEAI PCM).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ndk::audio::{AudioCallbackResult, AudioDirection, AudioFormat, AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage};

use rdp_hyper::playout::Playout;

use crate::capture::Mic;

/// No sound this long: the output closes (the next sound reopens it).
const OUTPUT_IDLE: Duration = Duration::from_secs(10);
/// The queue's depth: RDPSND sends its sound in bursts of tens of
/// milliseconds. Held there by the Hyper stream's jitter buffer (a burst or
/// the clocks' drift played away 0.5% faster or slower), where a plain queue
/// let the delay pile up to its cap.
const TARGET_MS: usize = 60;

pub struct PcmOut {
    format: Option<(u16, u32, u16)>,
    output: Option<(AudioStream, Arc<Mutex<Playout>>)>,
    dead: Arc<AtomicBool>,
    last: Instant,
    /// Converted samples, reused.
    pcm: Vec<f32>,
}

impl PcmOut {
    pub fn new() -> Self {
        Self { format: None, output: None, dead: Arc::new(AtomicBool::new(false)), last: Instant::now(), pcm: Vec::new() }
    }

    fn open(&mut self) -> Option<&Arc<Mutex<Playout>>> {
        if self.dead.swap(false, Ordering::Relaxed) {
            self.output = None;
        }
        if self.output.is_none() {
            let (channels, rate, _) = self.format?;
            let ch = channels.clamp(1, 2) as usize;
            let mut playout = Playout::new(ch);
            playout.target = rate as usize * TARGET_MS / 1000;
            let ring = Arc::new(Mutex::new(playout));
            let cb = ring.clone();
            let dead = self.dead.clone();
            let stream = AudioStreamBuilder::new()
                .ok()?
                .direction(AudioDirection::Output)
                .format(AudioFormat::PCM_Float)
                .sample_rate(rate as i32)
                .channel_count(ch as i32)
                .usage(AudioUsage::Media)
                .performance_mode(crate::audio::PERFORMANCE_MODE)
                .sharing_mode(AudioSharingMode::Shared)
                .data_callback(Box::new(move |_s, data, frames| {
                    let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, frames as usize * ch) };
                    cb.lock().unwrap().fill(out);
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(move |_s, e| {
                    log::warn!("sound output error: {e:?}");
                    dead.store(true, Ordering::Relaxed);
                }))
                .open_stream()
                .map_err(|e| log::warn!("sound output: {e:?}"))
                .ok()?;
            let burst = stream.frames_per_burst();
            if burst > 0 {
                let _ = stream.set_buffer_size_in_frames(burst * 2);
            }
            stream.request_start().ok()?;
            log::info!("Cloud PC sound: {rate} Hz, {ch} ch, burst {burst} frames, {TARGET_MS} ms queued");
            self.output = Some((stream, ring));
        }
        self.output.as_ref().map(|(_, r)| r)
    }
}

// AAudio streams may be used and closed from any thread; the session hands
// the sink and the mic to its worker thread, which alone uses them.
unsafe impl Send for PcmOut {}
unsafe impl Send for MicIn {}

impl rdp_channels::rdpsnd::AudioSink for PcmOut {
    fn set_format(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16) {
        let f = Some((channels, samples_per_sec, bits_per_sample));
        if self.format != f {
            self.format = f;
            self.output = None;
        }
    }

    fn play(&mut self, pcm: &[u8]) {
        let Some((_, _, bits)) = self.format else { return };
        self.last = Instant::now();
        let mut samples = std::mem::take(&mut self.pcm);
        samples.clear();
        match bits {
            16 => samples.extend(pcm.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)),
            8 => samples.extend(pcm.iter().map(|&s| (s as f32 - 128.0) / 128.0)),
            _ => {}
        }
        if let Some(ring) = self.open() {
            ring.lock().unwrap().push(&samples);
        }
        self.pcm = samples;
    }

    fn idle(&mut self) {
        if self.output.is_some() && self.last.elapsed() > OUTPUT_IDLE {
            log::info!("Cloud PC sound idle: output closed");
            self.output = None;
        }
    }
}

/// The microphone for the session (opened when its apps start recording).
pub struct MicIn {
    mic: Option<Mic>,
    format: (u16, u32),
    buf: Vec<f32>,
    /// Asked for and not open (no permission yet, or no device): try again
    /// from this time.
    retry_at: Option<Instant>,
}

impl MicIn {
    pub fn new() -> Self {
        Self { mic: None, format: (1, 48_000), buf: Vec::new(), retry_at: None }
    }

    fn open(&mut self) {
        // Each burst of sound wakes the session worker, which sends it.
        self.mic = Mic::open_with(self.format.1, self.format.0, Some(crate::net_wait::worker_wake::signal))
            .map_err(|e| log::info!("microphone: {e}"))
            .ok();
        self.retry_at = self.mic.is_none().then(|| Instant::now() + Duration::from_secs(1));
    }
}

impl crate::session::MicSource for MicIn {
    fn start(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16) {
        if bits_per_sample != 16 {
            log::warn!("microphone: {bits_per_sample}-bit asked for; sending 16");
        }
        self.format = (channels, samples_per_sec);
        self.open();
    }

    fn poll(&mut self) -> Vec<u8> {
        let dead = self.mic.as_ref().is_some_and(|m| m.dead());
        if dead || (self.mic.is_none() && self.retry_at.is_some_and(|t| Instant::now() >= t)) {
            // (Permission granted since, or the device came back.)
            self.open();
        }
        let Some(mic) = self.mic.as_ref() else { return Vec::new() };
        self.buf.clear();
        mic.take(&mut self.buf, Duration::ZERO);
        let mut out = Vec::with_capacity(self.buf.len() * 2);
        for s in &self.buf {
            out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        out
    }
}
