//! Hyper audio: game sound host → client and the client's mic → host, as Opus
//! over the Hyper link (see `rdp_hyper::audio`).
//!
//! On the host, game sound is routed to a virtual output for the length of the
//! stream (Steam Streaming Speakers when present, so it doesn't also blare from
//! the host's own speakers), captured by WASAPI loopback and sent. The client's
//! mic is played into a virtual microphone (Steam Streaming Microphone, or
//! VB-Audio CABLE) that is made the host's default mic meanwhile; like RDP's
//! mic redirection, the client only captures while a host app is listening.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdp_hyper::audio::{AudioPacket, Decode, Sequencer, FRAME_SAMPLES};

use crate::wasapi::{self, Capture, DefaultSwitch, Flow, Player};

/// Sends one sealed `Kind::Audio` body.
pub type SendFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// What to capture.
pub enum Source {
    /// Whatever the default output device plays.
    Loopback,
    /// A microphone (endpoint id, or the default).
    Mic(Option<String>),
}

/// Capture → Opus → send, on its own thread.
pub struct AudioSender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioSender {
    pub fn start(source: Source, channels: usize, bitrate: i32, clock: fn() -> u64, send: SendFn) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hyper-audio-tx".into())
            .spawn(move || {
                if let Err(e) = send_loop(source, channels, bitrate, clock, send, &s) {
                    tracing::warn!(error = %e, "Hyper audio capture stopped");
                }
            })
            .ok();
        Self { stop, thread }
    }

    /// The capture thread has ended. Device losses don't end it (it reopens
    /// the device), so only a hard failure does; the owner may start anew.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Wake it from waiting to reopen a device (the client drops its
            // mic sender on the network thread).
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Audio packets sent since the last [`take_sent`] (for the host's log).
static SENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn take_sent() -> u32 {
    SENT.swap(0, Ordering::Relaxed)
}

/// Silent frames sent after a sound ends (the decoder's tail), before the
/// sender goes quiet.
const SILENT_TAIL: u32 = 2;

/// Digital silence: what an app holding the device open plays when it plays
/// nothing.
fn is_silent(pcm: &[f32]) -> bool {
    pcm.iter().all(|s| s.abs() < 1.0e-5)
}

fn send_loop(
    source: Source,
    channels: usize,
    bitrate: i32,
    clock: fn() -> u64,
    send: SendFn,
    stop: &AtomicBool,
) -> Result<(), Box<dyn std::error::Error>> {
    wasapi::com_init();
    let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
    let mut enc = opus::Encoder::new(wasapi::RATE, ch, opus::Application::LowDelay)?;
    enc.set_bitrate(opus::Bitrate::Bits(bitrate))?;
    let frame_len = FRAME_SAMPLES * channels;
    let mut pcm = Vec::with_capacity(frame_len * 4);
    let mut out = vec![0u8; 1500];
    let mut prev: Option<Vec<u8>> = None;
    let mut seq = 0u32;
    // Silent frames in a row: past the tail, nothing is sent (the receiver
    // takes the pause for silence and lets its output close).
    let mut silent_run = 0u32;
    // The host polls tighter (it has headroom and game audio latency shows);
    // the client's mic wakes less often to keep the laptop's CPU idle.
    let poll = Duration::from_millis(if matches!(source, Source::Loopback) { 2 } else { 5 });
    // Devices come and go (unplugged, an HDMI endpoint reset by a display mode
    // change, a new default): the capture is reopened, at most once a second
    // or every few seconds while that fails, and the stream carries on.
    let mut capture: Option<Capture> = None;
    let (mut opened_at, mut checked, mut retry_at) = (Instant::now(), Instant::now(), Instant::now());
    let mut failures = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let Some(cap) = capture.as_mut() else {
            let now = Instant::now();
            if now < retry_at {
                std::thread::park_timeout(retry_at - now);
                continue;
            }
            let opened = match &source {
                Source::Loopback => Capture::new(None, Flow::Render, true, channels),
                Source::Mic(dev) => Capture::new(dev.as_deref(), Flow::Capture, false, channels),
            };
            match opened {
                Ok(c) => {
                    capture = Some(c);
                    opened_at = Instant::now();
                    checked = opened_at;
                    failures = 0;
                    pcm.clear();
                }
                Err(e) => {
                    if failures == 0 {
                        tracing::warn!(error = %e, "Hyper audio capture unavailable; retrying every 3 s");
                    }
                    failures += 1;
                    retry_at = Instant::now() + Duration::from_secs(3);
                }
            }
            continue;
        };
        std::thread::sleep(poll);
        let mut lost = false;
        if let Err(e) = cap.read(&mut pcm) {
            tracing::info!(error = %e, "Hyper audio capture device lost; reopening");
            lost = true;
        } else if checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            if cap.default_changed() {
                tracing::info!("default audio device changed; capturing the new one");
                lost = true;
            }
        }
        if lost {
            capture = None;
            retry_at = opened_at + Duration::from_secs(1);
        }
        while pcm.len() >= frame_len {
            // (Encoded even when not sent: the encoder's state runs on.)
            silent_run = if is_silent(&pcm[..frame_len]) { silent_run.saturating_add(1) } else { 0 };
            let n = enc.encode_float(&pcm[..frame_len], &mut out)?;
            pcm.drain(..frame_len);
            if silent_run > SILENT_TAIL {
                // (Sequence numbers carry on from the last one sent: a pause,
                // not a loss, to the receiver.)
                prev = None;
                continue;
            }
            SENT.fetch_add(1, Ordering::Relaxed);
            let frame = out[..n].to_vec();
            seq = seq.wrapping_add(1);
            let mut frames = Vec::with_capacity(2);
            if let Some(p) = prev.take() {
                frames.push(p);
            }
            frames.push(frame.clone());
            prev = Some(frame);
            let packet = AudioPacket { seq, channels: channels as u8, capture_us: clock(), frames };
            send(&packet.to_bytes());
        }
    }
    Ok(())
}

/// Audio playback on its own thread: the network receive threads hand packets
/// over through a bounded queue and never wait on the audio device (opening
/// one can take seconds, or hang on a broken driver).
pub struct AudioSink {
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioSink {
    /// Play to `device` (endpoint id) or the default output.
    pub fn start(device: Option<String>) -> Self {
        // ~0.6 s of 10 ms packets: a stalled device drops audio, not video.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        let thread = std::thread::Builder::new()
            .name("hyper-audio-rx".into())
            .spawn(move || {
                wasapi::com_init();
                let mut r = AudioReceiver::new(device);
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(body) => r.on_packet(&body),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => r.idle(),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), thread }
    }

    pub fn push(&self, body: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(body.to_vec());
        }
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

/// How much the jitter buffer must hold: how much later than the quickest
/// packet the latest ones came (the 99th percentile, host capture clock
/// against ours, over the last 10 s), plus a margin — 10 ms on a calm wired
/// link instead of a fixed 30. Running dry anyway adds 10 ms for 30 s.
pub(crate) struct JitterTarget {
    base: Instant,
    seen: std::collections::VecDeque<(Instant, i64)>,
    floor: Option<(Instant, u32)>,
    next_update: Instant,
    started: Instant,
    pub(crate) ms: u32,
}

#[cfg(test)]
mod silence_tests {
    #[test]
    fn only_digital_silence_counts() {
        assert!(super::is_silent(&[0.0; 960]));
        assert!(super::is_silent(&[1.0e-7; 960]));
        let mut quiet_noise = vec![0.0f32; 960];
        quiet_noise[500] = 0.001; // a -60 dB sample: sound
        assert!(!super::is_silent(&quiet_noise));
    }
}

/// A pause in the packets this long is the host's silence, not jitter.
const SILENCE: Duration = Duration::from_millis(150);

impl JitterTarget {
    const MIN_MS: u32 = 10;
    const MAX_MS: u32 = 80;
    const MARGIN_MS: u32 = 3;
    const WINDOW: Duration = Duration::from_secs(10);
    const DRY_HOLD: Duration = Duration::from_secs(30);

    pub(crate) fn new(now: Instant) -> Self {
        Self { base: now, seen: Default::default(), floor: None, next_update: now, started: now, ms: 30 }
    }

    /// A packet captured at `capture_us` (host clock) arrived `now`. Returns
    /// a new target when it changed.
    pub(crate) fn arrived(&mut self, capture_us: u64, now: Instant) -> Option<u32> {
        let local_us = now.duration_since(self.base).as_micros() as i64;
        self.seen.push_back((now, local_us - capture_us as i64));
        while self.seen.front().is_some_and(|(t, _)| now.duration_since(*t) > Self::WINDOW) {
            self.seen.pop_front();
        }
        // Two seconds of packets before the first estimate; then twice a second.
        if now < self.next_update || now.duration_since(self.started) < Duration::from_secs(2) {
            return None;
        }
        self.next_update = now + Duration::from_millis(500);
        let min = self.seen.iter().map(|s| s.1).min()?;
        let mut late: Vec<i64> = self.seen.iter().map(|s| s.1 - min).collect();
        late.sort_unstable();
        let p99 = late[(late.len() - 1) * 99 / 100];
        let mut ms = ((p99 as u64).div_ceil(1000) as u32 + Self::MARGIN_MS).clamp(Self::MIN_MS, Self::MAX_MS);
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

    /// Playback ran dry: 10 ms more at once, held for a while.
    pub(crate) fn ran_dry(&mut self, now: Instant) -> u32 {
        self.ms = (self.ms + 10).min(Self::MAX_MS);
        self.floor = Some((now + Self::DRY_HOLD, self.ms));
        self.ms
    }
}

/// Receive → order → Opus decode → play.
pub struct AudioReceiver {
    device: Option<String>,
    seq: Sequencer,
    decoder: Option<(u8, opus::Decoder)>,
    player: Option<Player>,
    pcm: Vec<f32>,
    jitter: JitterTarget,
    /// The player's count of times it ran dry, as last seen.
    underruns: u32,
    /// When the last packet came.
    last_packet: Option<Instant>,
    /// No (re)opening the output before this: 5 s after a failure, 1 s after
    /// a start (a device that dies at once isn't reopened on every packet).
    retry_at: Option<Instant>,
}

/// No sound from the host this long: the output closes (an open one keeps
/// Windows' audio engine running, playing silence); the next sound opens it
/// again, a few tens of milliseconds late.
const OUTPUT_IDLE: Duration = Duration::from_secs(10);

impl AudioReceiver {
    /// Call when no packet came for a while: closes an output that's had
    /// nothing to play for [`OUTPUT_IDLE`].
    pub fn idle(&mut self) {
        if self.player.is_some() && self.last_packet.is_some_and(|t| t.elapsed() > OUTPUT_IDLE) {
            tracing::debug!("no sound for a while; audio output closed until the next");
            self.player = None;
        }
    }

    /// Play to `device` (endpoint id) or the default output.
    pub fn new(device: Option<String>) -> Self {
        Self {
            device,
            seq: Sequencer::new(),
            decoder: None,
            player: None,
            pcm: vec![0.0; FRAME_SAMPLES * 2],
            jitter: JitterTarget::new(Instant::now()),
            underruns: 0,
            last_packet: None,
            retry_at: None,
        }
    }

    pub fn on_packet(&mut self, body: &[u8]) {
        let Some(p) = AudioPacket::parse(body) else { return };
        let ch = p.channels.clamp(1, 2);
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != ch) {
            let channels = if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            match opus::Decoder::new(wasapi::RATE, channels) {
                Ok(d) => self.decoder = Some((ch, d)),
                Err(e) => {
                    tracing::warn!(error = %e, "Opus decoder unavailable");
                    return;
                }
            }
            self.player = None;
        }
        // Its device went away, or (playing to the default) the default output
        // changed: open whatever is current now.
        if self.player.as_ref().is_some_and(|p| !p.is_alive()) {
            tracing::info!("audio output closed; reopening");
            self.player = None;
        }
        if self.player.is_none() {
            let now = Instant::now();
            if self.retry_at.is_some_and(|t| now < t) {
                return;
            }
            wasapi::com_init();
            match Player::new(self.device.clone(), ch as usize) {
                Ok(player) => {
                    player.set_target_ms(self.jitter.ms);
                    self.underruns = 0;
                    self.player = Some(player);
                    self.retry_at = Some(now + Duration::from_secs(1));
                    // A fresh start: nothing to conceal from before it.
                    self.seq = Sequencer::new();
                    if let Some((_, dec)) = self.decoder.as_mut() {
                        let _ = dec.reset_state();
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "audio output unavailable; retrying in 5 s");
                    self.retry_at = Some(Instant::now() + Duration::from_secs(5));
                    return;
                }
            }
        }
        let (_, dec) = self.decoder.as_mut().unwrap();
        let player = self.player.as_ref().unwrap();
        // The buffer as deep as the network's jitter needs.
        let now = Instant::now();
        let mut target = self.jitter.arrived(p.capture_us, now);
        let gap = self.last_packet.replace(now).map(|t| now.duration_since(t));
        let underruns = player.underruns();
        if underruns > self.underruns {
            self.underruns = underruns;
            // Ran dry with packets still coming: a deeper buffer. Not after a
            // pause in them: the host sends nothing while nothing plays (every
            // sound's end runs the queue dry), and that's no jitter.
            if gap.is_some_and(|g| g < SILENCE) {
                target = Some(self.jitter.ran_dry(now));
            }
        }
        if let Some(ms) = target {
            tracing::debug!(ms, "audio jitter buffer");
            player.set_target_ms(ms);
        }
        // Exactly one 10 ms frame per decode: concealment sizes its output
        // from the buffer it's given.
        let pcm = &mut self.pcm[..FRAME_SAMPLES * ch as usize];
        for d in self.seq.push(&p) {
            let r = match d {
                Decode::Frame(f) => dec.decode_float(&f, pcm, false),
                Decode::Conceal => dec.decode_float(&[], pcm, false),
                // A new stream from the sender (its mic was reopened).
                Decode::Restart => {
                    let _ = dec.reset_state();
                    continue;
                }
            };
            if let Ok(n) = r {
                player.push(&pcm[..n * ch as usize]);
            }
        }
    }
}

/// Host-side audio routing for the length of a stream; restored on drop.
pub struct HostRouting {
    _speakers: Option<DefaultSwitch>,
    _mic: Option<DefaultSwitch>,
    /// Where the client's mic is played (the virtual mic's input side).
    pub mic_sink: Option<String>,
    /// The virtual mic apps record from (to watch for listeners).
    pub mic_source: Option<String>,
}

impl HostRouting {
    /// Leave the host's audio devices alone (the loopback selftest).
    pub fn none() -> Self {
        Self { _speakers: None, _mic: None, mic_sink: None, mic_source: None }
    }

    pub fn set_up() -> Self {
        wasapi::com_init();
        let speakers = wasapi::find(Flow::Render, "Steam Streaming Speakers")
            .and_then(|ep| DefaultSwitch::to(Flow::Render, &ep));
        if speakers.is_none() {
            tracing::info!("no virtual speakers (install Steam for 'Steam Streaming Speakers'): game audio will also play on the host");
        }
        // A virtual mic is two endpoints: we render into one, apps capture from the other.
        let pairs = [("Steam Streaming Microphone", "Steam Streaming Microphone"), ("CABLE Input", "CABLE Output")];
        let mut mic = None;
        for (sink, source) in pairs {
            if let (Some(sink), Some(source)) = (wasapi::find(Flow::Render, sink), wasapi::find(Flow::Capture, source)) {
                mic = Some((sink, source));
                break;
            }
        }
        let (mic_switch, mic_sink, mic_source) = match mic {
            Some((sink, source)) => (DefaultSwitch::to(Flow::Capture, &source), Some(sink.id), Some(source.id)),
            None => {
                tracing::info!("no virtual microphone (Steam Streaming Microphone or VB-Audio CABLE): the client's mic can't reach host apps");
                (None, None, None)
            }
        };
        Self { _speakers: speakers, _mic: mic_switch, mic_sink, mic_source }
    }
}

/// Whether any app is currently recording from capture endpoint `id`.
pub fn endpoint_has_listeners(id: &str) -> bool {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    wasapi::com_init();
    unsafe {
        let Ok(e): windows::core::Result<IMMDeviceEnumerator> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return false;
        };
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        let Ok(dev) = e.GetDevice(windows::core::PCWSTR(wide.as_ptr())) else { return false };
        let Ok(mgr): windows::core::Result<IAudioSessionManager2> = dev.Activate(CLSCTX_ALL, None) else {
            return false;
        };
        let Ok(list) = mgr.GetSessionEnumerator() else { return false };
        for i in 0..list.GetCount().unwrap_or(0) {
            if let Ok(s) = list.GetSession(i) {
                if s.GetState().is_ok_and(|st| st == AudioSessionStateActive) {
                    return true;
                }
            }
        }
        false
    }
}

/// `rdpio --hyper-audio-test`: exercise the host audio path without making a
/// sound — a 440 Hz tone goes into the virtual speakers and the virtual mic's
/// input, is captured back (loopback / mic), Opus encoded and decoded, and its
/// level checked. The default devices are switched for a few seconds and then
/// restored.
pub fn selftest() -> Result<(), Box<dyn std::error::Error>> {
    wasapi::com_init();
    let names = (wasapi::default_endpoint(Flow::Render), wasapi::default_endpoint(Flow::Capture));
    println!("default output: {:?}\ndefault mic:    {:?}", names.0.as_ref().map(|e| &e.name), names.1.as_ref().map(|e| &e.name));
    // Every role (console, multimedia, communications) must come back as it was.
    let before = (wasapi::default_ids(Flow::Render), wasapi::default_ids(Flow::Capture));
    let tone = |n: usize, ch: usize, phase: &mut f32| -> Vec<f32> {
        let mut v = Vec::with_capacity(n * ch);
        for _ in 0..n {
            let s = (*phase * std::f32::consts::TAU).sin() * 0.5;
            *phase = (*phase + 440.0 / wasapi::RATE as f32) % 1.0;
            v.extend(std::iter::repeat_n(s, ch));
        }
        v
    };
    let measure = |label: &str, sink: Option<String>, source: Option<&str>, flow: Flow, loopback: bool, ch: usize| -> Result<(), Box<dyn std::error::Error>> {
        let player = Player::new(sink, ch)?;
        let mut cap = Capture::new(source, flow, loopback, ch)?;
        let mut enc = opus::Encoder::new(wasapi::RATE, if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo }, opus::Application::LowDelay)?;
        let mut dec = opus::Decoder::new(wasapi::RATE, if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo })?;
        let (mut phase, mut got, mut packets, mut bytes) = (0f32, Vec::new(), 0, 0);
        let start = std::time::Instant::now();
        let mut out_pcm = vec![0f32; FRAME_SAMPLES * 2];
        let mut buf = vec![0u8; 1500];
        let mut decoded_rms = (0f64, 0usize);
        while start.elapsed() < Duration::from_millis(1500) {
            player.push(&tone(480, ch, &mut phase));
            std::thread::sleep(Duration::from_millis(10));
            cap.read(&mut got)?;
            while got.len() >= FRAME_SAMPLES * ch {
                let n = enc.encode_float(&got[..FRAME_SAMPLES * ch], &mut buf)?;
                got.drain(..FRAME_SAMPLES * ch);
                packets += 1;
                bytes += n;
                let d = dec.decode_float(&buf[..n], &mut out_pcm, false)?;
                if start.elapsed() > Duration::from_millis(500) {
                    for s in &out_pcm[..d * ch] {
                        decoded_rms.0 += (*s as f64).powi(2);
                        decoded_rms.1 += 1;
                    }
                }
            }
        }
        let rms = (decoded_rms.0 / decoded_rms.1.max(1) as f64).sqrt();
        let ok = rms > 0.1;
        println!(
            "{label}: {packets} Opus frames, {:.0} kbps, tone level after round trip {rms:.3} (sent 0.354) → {}",
            bytes as f64 * 8.0 / 1.5 / 1000.0,
            if ok { "OK" } else { "NOT HEARD" }
        );
        Ok(())
    };
    {
        let routing = HostRouting::set_up();
        let speakers = wasapi::default_endpoint(Flow::Render);
        println!("during stream, output → {:?}", speakers.as_ref().map(|e| &e.name));
        if speakers.as_ref().is_some_and(|e| e.name.contains("Steam Streaming Speakers")) {
            measure("game audio (virtual speakers → loopback → Opus)", None, None, Flow::Render, true, 2)?;
        } else {
            println!("game audio: skipped (no virtual speakers; it would be audible)");
        }
        match (&routing.mic_sink, &routing.mic_source) {
            (Some(sink), Some(source)) => {
                measure("client mic (→ virtual mic → host apps)", Some(sink.clone()), Some(source), Flow::Capture, false, 1)?
            }
            _ => println!("client mic: no virtual microphone"),
        }
    }
    let after = (wasapi::default_ids(Flow::Render), wasapi::default_ids(Flow::Capture));
    println!("defaults restored (every role): {}", if before == after { "yes" } else { "NO" });
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Packets every 10 ms; `late(i)` ms of extra delay on packet `i`.
    fn jitter_after(secs: u64, late: impl Fn(u64) -> u64) -> JitterTarget {
        let t0 = Instant::now();
        let mut j = JitterTarget::new(t0);
        for i in 0..secs * 100 {
            let capture_us = 5_000_000 + i * 10_000;
            j.arrived(capture_us, t0 + Duration::from_micros(i * 10_000 + late(i) * 1000));
        }
        j
    }

    #[test]
    fn a_calm_link_gets_a_shallow_audio_buffer() {
        assert_eq!(jitter_after(12, |i| i % 3).ms, 10);
    }

    #[test]
    fn jitter_deepens_the_audio_buffer() {
        // One packet in twenty 25 ms late (Wi-Fi): 25 + the margin.
        assert_eq!(jitter_after(12, |i| if i % 20 == 0 { 25 } else { 0 }).ms, 28);
        // A rare spike (one in 400) isn't waited for.
        assert_eq!(jitter_after(12, |i| if i % 400 == 0 { 60 } else { 0 }).ms, 10);
        // Never past the cap.
        assert_eq!(jitter_after(12, |i| if i % 10 == 0 { 200 } else { 0 }).ms, JitterTarget::MAX_MS);
    }

    #[test]
    fn running_dry_holds_a_deeper_buffer_for_a_while() {
        let t0 = Instant::now();
        let mut j = JitterTarget::new(t0);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        for i in 0..300 {
            j.arrived(i * 10_000, at(i * 10));
        }
        assert_eq!(j.ms, 10);
        assert_eq!(j.ran_dry(at(3000)), 20);
        for i in 300..2000 {
            j.arrived(i * 10_000, at(i * 10));
        }
        assert_eq!(j.ms, 20, "held for 30 s");
        for i in 2000..3500 {
            j.arrived(i * 10_000, at(i * 10));
        }
        assert_eq!(j.ms, 10, "then back to what the link needs");
    }

    use super::*;

    /// A capture device that can't be opened doesn't end the sender (it
    /// retries, for when one appears), and dropping the sender doesn't wait
    /// out the retry.
    #[test]
    fn a_missing_device_is_retried_and_dropping_is_prompt() {
        let sender = AudioSender::start(Source::Mic(Some("no such device".into())), 1, 32_000, || 0, Arc::new(|_: &[u8]| {}));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!sender.is_finished(), "the sender gave up");
        let t = Instant::now();
        drop(sender);
        assert!(t.elapsed() < Duration::from_millis(200), "dropping took {:?}", t.elapsed());
    }

    /// The whole path on real devices, silently and leaving the defaults
    /// alone: a tone played into the virtual mic is captured, Opus encoded,
    /// sent, decoded and played into the virtual speakers, whose loopback
    /// hears it. `cargo test -p rdp-client hyper_audio -- --ignored --nocapture`
    #[test]
    #[ignore = "needs Steam Streaming Speakers and Microphone"]
    fn a_tone_makes_it_through_the_whole_path() {
        wasapi::com_init();
        // Not while they're in use: the default output (a stream may be on),
        // or a mic some app is recording from.
        let speakers = wasapi::find(Flow::Render, "Steam Streaming Speakers")
            .filter(|e| wasapi::default_endpoint(Flow::Render).is_none_or(|d| d.id != e.id));
        let mic_input = wasapi::find(Flow::Render, "Steam Streaming Microphone");
        let mic = wasapi::find(Flow::Capture, "Steam Streaming Microphone").filter(|m| !endpoint_has_listeners(&m.id));
        let (Some(speakers), Some(mic_input), Some(mic)) = (speakers, mic_input, mic) else {
            eprintln!("virtual devices missing or in use; skipped");
            return;
        };
        let sink = Arc::new(AudioSink::start(Some(speakers.id.clone())));
        let to_sink = sink.clone();
        let sender = AudioSender::start(Source::Mic(Some(mic.id)), 1, 64_000, || 0, Arc::new(move |b: &[u8]| to_sink.push(b)));
        let tone_out = Player::new(Some(mic_input.id), 1).expect("player");
        let mut ear = Capture::new(Some(&speakers.id), Flow::Render, true, 2).expect("loopback");
        let (mut phase, mut heard, start) = (0f32, Vec::new(), Instant::now());
        let mut pushed = 0usize;
        while start.elapsed() < Duration::from_secs(2) {
            // Keep ~40 ms queued whatever the sleeps do.
            while pushed < (start.elapsed().as_secs_f64() * wasapi::RATE as f64) as usize + 1920 {
                let chunk: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|_| {
                        phase = (phase + 440.0 / wasapi::RATE as f32) % 1.0;
                        (phase * std::f32::consts::TAU).sin() * 0.5
                    })
                    .collect();
                tone_out.push(&chunk);
                pushed += FRAME_SAMPLES;
            }
            std::thread::sleep(Duration::from_millis(10));
            ear.read(&mut heard).expect("loopback read");
            assert!(!sender.is_finished(), "the capture thread ended");
        }
        drop(sender);
        let tail = &heard[heard.len().saturating_sub(wasapi::RATE as usize)..];
        let rms = (tail.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / tail.len().max(1) as f64).sqrt();
        eprintln!("heard back {} samples, rms over the last 0.5 s {rms:.3} (sent 0.354)", heard.len());
        assert!(rms > 0.1, "the tone didn't make it through (rms {rms:.3})");
    }
}
