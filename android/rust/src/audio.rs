//! The host's sound: Opus packets → PCM → AAudio (low-latency output).
//!
//! As on the desktop client (`hyper_audio.rs`): packets are ordered by
//! `rdp_hyper::audio::Sequencer` (which uses the redundant copy each packet
//! carries and asks for concealment of short gaps), decoded on this module's
//! own thread, and played from a small jitter buffer whose depth follows how
//! late packets come — 10 ms on a calm link, more after the output ran dry.
//! The buffer is the desktop's ([`rdp_hyper::playout::Playout`]): a burst or
//! the two clocks' drift is played away a fraction of a percent faster or
//! slower, so the delay stays at the target instead of creeping up.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ndk::audio::{AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage, Clockid};
use rdp_hyper::audio::{AudioPacket, Decode, Sequencer, FRAME_SAMPLES, LONGEST_FRAME_MS, MAX_FRAME_SAMPLES};
use rdp_hyper::playout::{frames, Playout};
use rdp_hyper::control::Control;

const RATE: usize = 48_000;
/// No sound from the host this long: the output pauses (an open one keeps
/// the audio hardware awake playing silence). The next sound restarts it at
/// once, where reopening it would take tens of ms and cut that sound's start.
const OUTPUT_IDLE: Duration = Duration::from_secs(10);
/// Paused this long too: the output closes.
const OUTPUT_CLOSE: Duration = Duration::from_secs(120);
/// At most this deep a device buffer, in bursts, however often it glitches.
const MAX_BURSTS: i32 = 4;

/// The stats line's audio part (empty while no sound plays).
pub static LINE: Mutex<String> = Mutex::new(String::new());

/// An open output and how it's doing.
struct Output {
    stream: AudioStream,
    playout: Arc<Mutex<Playout>>,
    burst: i32,
    /// Underruns and device glitches (xruns) seen so far.
    underruns: u32,
    xruns: i32,
    paused: bool,
    checked: Instant,
}

// AAudio streams may be used and closed from any thread; the receiver that
// owns this one is used under its lock alone.
unsafe impl Send for Output {}

impl Output {
    /// Queue depth to hold (frames): the jitter's.
    fn set_target(&self, frames: usize) {
        self.playout.lock().unwrap().target = frames;
    }

    /// Each second: a device that glitched gets a burst more buffer (AAudio's
    /// own advice: start small, grow on xruns), and the stats line.
    fn check(&mut self, now: Instant) {
        if now.duration_since(self.checked) < Duration::from_secs(1) {
            return;
        }
        self.checked = now;
        let xruns = self.stream.x_run_count();
        let size = self.stream.buffer_size_in_frames();
        if xruns > self.xruns && self.burst > 0 && size < self.burst * MAX_BURSTS {
            if let Ok(n) = self.stream.set_buffer_size_in_frames(size + self.burst) {
                log::info!("audio output glitched ({} xruns): device buffer {size} → {n} frames", xruns - self.xruns);
            }
        }
        self.xruns = xruns;
        let (target, level, underruns, cut) = {
            let p = self.playout.lock().unwrap();
            (p.target, p.level(), p.underruns, p.cut)
        };
        let ms = |f: usize| f as f64 * 1000.0 / RATE as f64;
        // Written but not yet heard: the device's buffer and its own latency.
        let device = self.stream.timestamp(Clockid::Monotonic).ok().map(|t| {
            let heard = t.frame_position as f64 + (monotonic_ns() - t.time_nanoseconds) as f64 * RATE as f64 / 1e9;
            (self.stream.frames_written() as f64 - heard).max(0.0) * 1000.0 / RATE as f64
        });
        *LINE.lock().unwrap() = format!(
            "audio {:.0} ms buffer (target {:.0}) + {} device  dry {underruns}  cut {:.0} ms  xruns {xruns}  {}",
            ms(level),
            ms(target),
            device.map_or("?".to_string(), |d| format!("{d:.0} ms")),
            ms(cut as usize),
            path_name(&self.stream),
        );
    }
}

fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

fn path_name(s: &AudioStream) -> &'static str {
    match (s.performance_mode(), s.sharing_mode()) {
        (AudioPerformanceMode::PowerSaving, _) => "power-saving",
        (AudioPerformanceMode::LowLatency, AudioSharingMode::Exclusive) => "low-latency exclusive",
        (AudioPerformanceMode::LowLatency, _) => "low-latency shared",
        _ => "normal",
    }
}

/// AAudio's path for every output: the low-latency one. (The power-saving
/// one measured 263 ms of its own delay on a OnePlus Pad 2, and ran dry on
/// its big bursts: a video's sound a quarter of a second behind it.)
pub const PERFORMANCE_MODE: AudioPerformanceMode = AudioPerformanceMode::LowLatency;

pub struct AudioSink {
    tx: Option<SyncSender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    receiver: Arc<Mutex<Receiver>>,
    lost: Arc<AtomicU32>,
    last_report: Option<Instant>,
}

impl AudioSink {
    /// `relaxed`: the stream is in office mode (its sound shares the output
    /// path with other apps').
    pub fn start(relaxed: Box<dyn Fn() -> bool + Send>) -> Self {
        let (tx, rx) = sync_channel::<Vec<u8>>(64);
        let lost = Arc::new(AtomicU32::new(0));
        let receiver = Arc::new(Mutex::new(Receiver::new(lost.clone(), relaxed)));
        let r = receiver.clone();
        let thread = std::thread::Builder::new()
            .name("hyper-audio".into())
            .spawn(move || {
                crate::raise_thread(-16);
                let mut placement = crate::tune::Placement::default();
                loop {
                    let msg = rx.recv_timeout(Duration::from_secs(1));
                    let mut r = r.lock().unwrap();
                    placement.follow(!(r.relaxed)());
                    match msg {
                        Ok(body) => r.on_packet(&body),
                        Err(RecvTimeoutError::Timeout) => r.idle(),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), thread, receiver, lost, last_report: None }
    }

    /// One packet from the host. Decoded here, on the receive thread (an
    /// Opus frame takes ~0.1 ms): no hand-over to another thread to wake, nor
    /// a copy of each packet. (`debug.rdpio.audioinline=0`: the audio
    /// thread decodes, as before.)
    pub fn push(&self, body: &[u8]) {
        if crate::tune::ab("audioinline", true) {
            self.receiver.lock().unwrap().on_packet(body);
        } else if let Some(tx) = &self.tx {
            let _ = tx.try_send(body.to_vec());
        }
    }

    /// `Control::AudioReport` for the host, when due (first packet, then
    /// every 2 s): we decode 20 ms frames, and how many we concealed.
    pub fn report(&mut self) -> Option<Control> {
        if self.last_report.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return None;
        }
        self.last_report = Some(Instant::now());
        let lost = self.lost.swap(0, Ordering::Relaxed).min(u16::MAX as u32) as u16;
        Some(Control::AudioReport { frame_ms: LONGEST_FRAME_MS, lost })
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Receiver {
    seq: Sequencer,
    decoder: Option<(u8, opus::Decoder)>,
    output: Option<Output>,
    /// The open output was opened in office mode (shared, not exclusive).
    output_relaxed: bool,
    relaxed: Box<dyn Fn() -> bool + Send>,
    /// Set by AAudio when the output died (headphones in or out, a Bluetooth
    /// switch): it's reopened at the next packet.
    output_dead: Arc<AtomicBool>,
    pcm: Vec<f32>,
    frame_samples: usize,
    lost: Arc<AtomicU32>,
    last_packet: Option<Instant>,
    retry_at: Option<Instant>,
    jitter: Jitter,
}

impl Receiver {
    fn new(lost: Arc<AtomicU32>, relaxed: Box<dyn Fn() -> bool + Send>) -> Self {
        Self {
            seq: Sequencer::new(),
            decoder: None,
            output: None,
            output_relaxed: false,
            relaxed,
            output_dead: Arc::new(AtomicBool::new(false)),
            pcm: vec![0.0; MAX_FRAME_SAMPLES * 2],
            frame_samples: 480,
            lost,
            last_packet: None,
            retry_at: None,
            jitter: Jitter::new(),
        }
    }

    fn idle(&mut self) {
        let quiet = self.last_packet.map_or(Duration::ZERO, |t| t.elapsed());
        if quiet > OUTPUT_IDLE + OUTPUT_CLOSE {
            if self.output.take().is_some() {
                log::debug!("no sound for a while; audio output closed");
                LINE.lock().unwrap().clear();
            }
        } else if quiet > OUTPUT_IDLE {
            if let Some(o) = self.output.as_mut().filter(|o| !o.paused) {
                let _ = o.stream.request_pause();
                o.paused = true;
            }
        }
    }

    fn on_packet(&mut self, body: &[u8]) {
        let Some(p) = AudioPacket::parse(body) else { return };
        let now = Instant::now();
        // A pause this long was the host's silence, not jitter.
        let after_silence = self.last_packet.is_some_and(|t| now.duration_since(t) > Duration::from_millis(150));
        self.last_packet = Some(now);
        let channels = p.channels.clamp(1, 2);
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != channels) {
            let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            match opus::Decoder::new(RATE as u32, ch) {
                Ok(d) => self.decoder = Some((channels, d)),
                Err(e) => {
                    log::warn!("opus decoder: {e}");
                    return;
                }
            }
            self.output = None; // reopened at the new channel count
        }
        if self.output_dead.swap(false, Ordering::Relaxed) && self.output.take().is_some() {
            log::info!("audio output lost (device change); reopening");
            self.retry_at = None;
        }
        // Office mode switched: reopen on the other path.
        let relaxed = (self.relaxed)();
        if self.output.is_some() && relaxed != self.output_relaxed {
            self.output = None;
            self.retry_at = None;
        }
        if self.output.is_none() && self.retry_at.is_none_or(|t| now >= t) {
            match open_output(channels as usize, self.output_dead.clone(), relaxed) {
                Ok(o) => {
                    o.set_target(self.jitter.frames());
                    self.output = Some(o);
                    self.output_relaxed = relaxed;
                    self.retry_at = Some(now + Duration::from_secs(1));
                }
                Err(e) => {
                    log::warn!("audio output: {e}");
                    self.retry_at = Some(now + Duration::from_secs(5));
                }
            }
        }
        if let Some(o) = self.output.as_mut().filter(|o| o.paused) {
            match o.stream.request_start() {
                Ok(()) => o.paused = false,
                Err(e) => {
                    log::warn!("audio output restart: {e:?}");
                    self.output = None;
                }
            }
        }
        if let Some(ms) = self.jitter.arrived(p.capture_us, now) {
            if let Some(o) = &self.output {
                o.set_target(frames(ms as usize));
            }
        }
        let (_, decoder) = self.decoder.as_mut().unwrap();
        let ch = channels as usize;
        // The last frame of a gap from the next one's in-band FEC, where the
        // host codes it (`debug.rdpio.opusfec=0`: concealment only).
        self.seq.set_fec(crate::tune::ab("opusfec", true));
        let missing = self.seq.lost_packets;
        for d in self.seq.push(&p) {
            let n = match d {
                Decode::Restart => {
                    let _ = decoder.reset_state();
                    continue;
                }
                Decode::Frame(f) => decoder.decode_float(&f, &mut self.pcm, false),
                Decode::Conceal => decoder.decode_float(&[], &mut self.pcm[..self.frame_samples * ch], false),
                Decode::Fec(f) => decoder.decode_float(&f, &mut self.pcm[..self.frame_samples * ch], true),
            };
            let Ok(n) = n else { continue };
            self.frame_samples = n.max(120);
            if let Some(o) = &self.output {
                let mut playout = o.playout.lock().unwrap();
                // A relay's backlog cut out at once, matched and crossfaded
                // (`debug.rdpio.catchup=0`: an abrupt cut at the cap, steering
                // below it).
                playout.catchup = crate::tune::ab("catchup", true);
                playout.push(&self.pcm[..n * ch]);
            }
        }
        // Packets that never came, before any copy or FEC repaired them: the
        // host's measure of the path (its copies and FEC follow it).
        self.lost.fetch_add(self.seq.lost_packets.wrapping_sub(missing), Ordering::Relaxed);
        if let Some(o) = self.output.as_mut() {
            let underruns = o.playout.lock().unwrap().underruns;
            // (Running dry in the host's silence isn't the network's doing.)
            if underruns > o.underruns && !after_silence {
                let ms = self.jitter.ran_dry(now);
                o.set_target(frames(ms as usize));
            }
            o.underruns = underruns;
            o.check(now);
        }
    }
}

fn open_output(channels: usize, dead: Arc<AtomicBool>, relaxed: bool) -> Result<Output, String> {
    let playout = Arc::new(Mutex::new(Playout::new(channels)));
    let cb = playout.clone();
    let stream = AudioStreamBuilder::new()
        .map_err(|e| format!("{e:?}"))?
        .direction(AudioDirection::Output)
        .format(AudioFormat::PCM_Float)
        .sample_rate(RATE as i32)
        .channel_count(channels as i32)
        .performance_mode(PERFORMANCE_MODE)
        // A game's sound: the DSP's buffer directly (MMAP), skipping the
        // mixer's; AAudio falls back to shared where it can't. (Office mode
        // shares the path, leaving it to other apps too.)
        .sharing_mode(if relaxed { AudioSharingMode::Shared } else { AudioSharingMode::Exclusive })
        .usage(AudioUsage::Game)
        .data_callback(Box::new(move |_s, data, frames| {
            let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, frames as usize * channels) };
            // (Silence while priming or after running dry; it primes again.)
            cb.lock().unwrap().fill(out);
            AudioCallbackResult::Continue
        }))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("audio output error: {e:?}");
            dead.store(true, Ordering::Relaxed);
        }))
        .open_stream()
        .map_err(|e| format!("open: {e:?}"))?;
    let burst = stream.frames_per_burst();
    if burst > 0 {
        // As small as the device goes: one burst exclusive (the DSP reads
        // the buffer itself), two shared; more only if it glitches.
        let bursts = if stream.sharing_mode() == AudioSharingMode::Exclusive { 1 } else { 2 };
        let _ = stream.set_buffer_size_in_frames(burst * bursts);
    }
    stream.request_start().map_err(|e| format!("start: {e:?}"))?;
    log::info!(
        "audio output open: {channels} ch, burst {burst} frames, buffer {} frames, {} path",
        stream.buffer_size_in_frames(),
        path_name(&stream)
    );
    let xruns = stream.x_run_count();
    Ok(Output { stream, playout, burst, underruns: 0, xruns, paused: false, checked: Instant::now() })
}

/// The jitter buffer's depth (ms): the 99th percentile of how much later than
/// the quickest packet the others came over the last 10 s, plus 3 ms; 10 more
/// for 30 s after running dry. (`hyper_audio::JitterTarget` on the desktop.)
struct Jitter {
    base: Instant,
    seen: VecDeque<(Instant, i64)>,
    floor: Option<(Instant, u32)>,
    next_update: Instant,
    started: Instant,
    ms: u32,
}

impl Jitter {
    const MIN_MS: u32 = 10;
    const MAX_MS: u32 = 80;

    fn new() -> Self {
        let now = Instant::now();
        Self { base: now, seen: VecDeque::new(), floor: None, next_update: now, started: now, ms: 30 }
    }

    fn frames(&self) -> usize {
        self.ms as usize * RATE / 1000
    }

    fn arrived(&mut self, capture_us: u64, now: Instant) -> Option<u32> {
        let local_us = now.duration_since(self.base).as_micros() as i64;
        self.seen.push_back((now, local_us - capture_us as i64));
        while self.seen.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(10)) {
            self.seen.pop_front();
        }
        if now < self.next_update || now.duration_since(self.started) < Duration::from_secs(2) {
            return None;
        }
        self.next_update = now + Duration::from_millis(500);
        let min = self.seen.iter().map(|s| s.1).min()?;
        let mut late: Vec<i64> = self.seen.iter().map(|s| s.1 - min).collect();
        late.sort_unstable();
        let p99 = late[(late.len() - 1) * 99 / 100];
        let mut ms = ((p99 as u64).div_ceil(1000) as u32 + 3).clamp(Self::MIN_MS, Self::MAX_MS);
        match self.floor {
            Some((until, floor)) if now < until => ms = ms.max(floor),
            Some(_) => self.floor = None,
            None => {}
        }
        (ms != self.ms).then(|| {
            self.ms = ms;
            ms
        })
    }

    fn ran_dry(&mut self, now: Instant) -> u32 {
        self.ms = (self.ms + 10).min(Self::MAX_MS);
        self.floor = Some((now + Duration::from_secs(30), self.ms));
        self.ms
    }
}

/// How the mic is sent, from what the host reports (`Control::AudioReport`;
/// `hyper_audio::AudioShape` on the desktop).
#[derive(Default)]
pub struct MicShape {
    /// The longest frame the host decodes (ms); 0 until it says.
    peer_frame_ms: AtomicU32,
    /// Office mode: 20 ms frames are fine (a call's). In game mode (a game's
    /// voice chat) each frame's 10 ms more wait would show.
    pub long: AtomicBool,
    /// When the host last reported losing a frame.
    last_loss: Mutex<Option<Instant>>,
    /// Packets sent since the host's last report.
    sent: AtomicU32,
    /// The path's packet loss (%), from the host's reports: what the Opus
    /// encoder's in-band FEC is sized for.
    loss_pct: AtomicU32,
}

impl MicShape {
    pub fn report(&self, frame_ms: u8, lost: u16) {
        self.peer_frame_ms.store(frame_ms as u32, Ordering::Relaxed);
        if lost > 0 {
            *self.last_loss.lock().unwrap() = Some(Instant::now());
        }
        let sent = self.sent.swap(0, Ordering::Relaxed);
        self.loss_pct.store(rdp_hyper::audio::loss_pct(self.loss_pct.load(Ordering::Relaxed), lost as u32, sent), Ordering::Relaxed);
    }

    /// 20 ms frames in office mode where the host takes them (what call apps
    /// send: half the packets, each a radio wake-up on both ends), else 10 ms.
    fn frame_samples(&self) -> usize {
        if self.long.load(Ordering::Relaxed) && self.peer_frame_ms.load(Ordering::Relaxed) >= 20 {
            FRAME_SAMPLES * 2
        } else {
            FRAME_SAMPLES
        }
    }

    /// Each packet carries the frame before its own too, until the host
    /// says it's losing none (and for 30 s after it lost one).
    fn redundant(&self) -> bool {
        self.peer_frame_ms.load(Ordering::Relaxed) == 0
            || self.last_loss.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(30))
    }
}

/// The tablet's mic → Opus → the host, on its own thread, while a host app
/// listens (`hyper_audio::AudioSender` on the desktop).
pub struct MicSender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicSender {
    pub fn start(send: Arc<dyn Fn(&[u8]) + Send + Sync>, shape: Arc<MicShape>, clock: fn() -> u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hyper-mic".into())
            .spawn(move || {
                crate::raise_thread(-16);
                if let Err(e) = mic_loop(&send, &shape, clock, &s) {
                    log::warn!("microphone stopped: {e}");
                }
            })
            .ok();
        log::info!("a PC app is listening; sending this tablet's microphone");
        Self { stop, thread }
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for MicSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        log::info!("microphone released");
    }
}

fn mic_loop(send: &Arc<dyn Fn(&[u8]) + Send + Sync>, shape: &MicShape, clock: fn() -> u64, stop: &AtomicBool) -> Result<(), String> {
    // Opus's speech mode, which tells a pause in speech from speech (`dtx`):
    // a mic is never digitally silent, so without it every pause is sent.
    let mut enc = opus::Encoder::new(RATE as u32, opus::Channels::Mono, opus::Application::Voip).map_err(|e| e.to_string())?;
    enc.set_bitrate(opus::Bitrate::Bits(48_000)).map_err(|e| e.to_string())?;
    enc.set_dtx(true).map_err(|e| e.to_string())?;
    // Complexity 5 (libopus's default is 9 or 10): about a quarter less CPU a
    // frame on the tablet's little cores (5.1 -> 3.8% of an A520 at 10 ms
    // frames), for no difference you'd hear in speech at this bitrate. What
    // WebRTC uses on phones. (`debug.rdpio.opuscx5=0`: the default.)
    let mut complexity = 0;
    let mut fec_loss = 0;
    let mut mic: Option<crate::capture::Mic> = None;
    let mut retry_at = Instant::now();
    let mut pcm: Vec<f32> = Vec::with_capacity(RATE);
    let mut out = vec![0u8; 1500];
    let mut prev: Option<Vec<u8>> = None;
    let mut seq = 0u32;
    // Wakeups of this thread against AAudio's bursts and the frames sent, for
    // the log (`micinline`'s measurement).
    let (mut loops, mut frames_out, mut logged) = (0u32, 0u32, (Instant::now(), 0u32, 0u32));
    while !stop.load(Ordering::Relaxed) {
        let Some(m) = mic.as_ref() else {
            // Opened when it can be (the permission may still be coming).
            if Instant::now() >= retry_at {
                match crate::capture::Mic::open(RATE as u32, 1) {
                    Ok(m) => {
                        mic = Some(m);
                        pcm.clear();
                        logged = (Instant::now(), 0, 0);
                        loops = 0;
                        frames_out = 0;
                    }
                    Err(e) => {
                        log::debug!("microphone: {e}");
                        retry_at = Instant::now() + Duration::from_secs(1);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        // Woken once a whole frame is in, not at each of AAudio's 1-2 ms
        // bursts (`debug.rdpio.micinline=0`: the old way). Encoding inside
        // AAudio's callback would save this wakeup too, but it would run the
        // encoder (up to ~0.4 ms on an A520) and the socket send in the
        // realtime callback, whose budget is the burst.
        let need = if crate::tune::ab("micinline", true) { shape.frame_samples().saturating_sub(pcm.len()) } else { 1 };
        m.take_at_least(&mut pcm, need, Duration::from_millis(50));
        loops += 1;
        if logged.0.elapsed() >= Duration::from_secs(10) {
            let (bursts, wakes) = m.bursts();
            let secs = logged.0.elapsed().as_secs_f32();
            log::info!(
                "microphone: {:.0} wakeups/s ({:.0} signalled) for {:.0} bursts/s, {:.0} frames/s sent",
                loops as f32 / secs,
                wakes.wrapping_sub(logged.2) as f32 / secs,
                bursts.wrapping_sub(logged.1) as f32 / secs,
                frames_out as f32 / secs
            );
            (loops, frames_out, logged) = (0, 0, (Instant::now(), bursts, wakes));
        }
        let want = if crate::tune::ab("opuscx5", true) { 5 } else { 10 };
        if want != complexity && enc.set_complexity(want).is_ok() {
            complexity = want;
        }
        // In-band FEC sized for the loss the host reports (speech mode codes
        // a coarse copy of each frame into the next; the host decodes a lost
        // frame from it). Off while nothing is lost: no bits spent on it.
        // (`debug.rdpio.opusfec=0`: never.)
        let want_loss = if crate::tune::ab("opusfec", true) { shape.loss_pct.load(Ordering::Relaxed) as i32 } else { 0 };
        if want_loss != fec_loss && enc.set_inband_fec(want_loss > 0).and_then(|_| enc.set_packet_loss_perc(want_loss)).is_ok() {
            fec_loss = want_loss;
        }
        if m.dead() {
            log::info!("microphone lost; opening it again");
            mic = None;
            continue;
        }
        loop {
            let frame_len = shape.frame_samples();
            if pcm.len() < frame_len {
                break;
            }
            let n = enc.encode_float(&pcm[..frame_len], &mut out).map_err(|e| e.to_string())?;
            pcm.drain(..frame_len);
            // A pause in speech (`dtx`: a byte or two, nothing to decode).
            if n <= 2 {
                prev = None;
                continue;
            }
            let frame = out[..n].to_vec();
            seq = seq.wrapping_add(1);
            let mut frames = Vec::with_capacity(2);
            if let Some(p) = prev.take().filter(|_| shape.redundant()) {
                frames.push(p);
            }
            frames.push(frame.clone());
            prev = Some(frame);
            send(&AudioPacket { seq, channels: 1, capture_us: clock(), frames }.to_bytes());
            shape.sent.fetch_add(1, Ordering::Relaxed);
            frames_out += 1;
        }
    }
    Ok(())
}
