//! Hyper audio: game sound host → client and the client's mic → host, as Opus
//! over the Hyper link (see `rdp_hyper::audio`).
//!
//! On the host, game sound is routed to a virtual output for the length of the
//! stream (Steam Streaming Speakers when present, so it doesn't also blare from
//! the host's own speakers), captured by WASAPI loopback and sent. The client's
//! mic is played into a virtual microphone (Steam Streaming Microphone, or
//! VB-Audio CABLE) that is made the host's default mic meanwhile; like RDP's
//! mic redirection, the client only captures while a host app is listening.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdp_hyper::audio::{AudioPacket, Decode, Sequencer, FRAME_SAMPLES, LONGEST_FRAME_MS, MAX_FRAME_SAMPLES};

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

/// How a sender shapes its packets, from what the receiver reports
/// ([`rdp_hyper::control::Control::AudioReport`]).
#[derive(Default)]
pub struct AudioShape {
    /// The longest frame the receiver decodes (ms); 0 until it says (an
    /// older rdpio never does: 10 ms frames, each with the one before).
    peer_frame_ms: AtomicU32,
    /// 20 ms frames wanted, where the receiver takes them: half the packets
    /// (each wakes both ends' radios and receive threads) for 10 ms more
    /// latency — a call's or a video's sound, not a game's.
    pub long: AtomicBool,
    /// When the receiver last reported losing a frame.
    last_loss: Mutex<Option<Instant>>,
    /// Packets sent since the receiver's last report.
    sent: AtomicU32,
    /// The path's packet loss (%), from the receiver's reports
    /// ([`rdp_hyper::audio::loss_pct`]): what Opus's in-band FEC is sized for.
    loss_pct: AtomicU32,
    /// The video's bitrate now (the host's rate controller): game sound
    /// follows the link (`audiorate`). Unset: the bitrate stays as given.
    pub link_bps: std::sync::OnceLock<Box<dyn Fn() -> u32 + Send + Sync>>,
    /// The link is slow: 20 ms frames too, where the receiver takes them
    /// (half the packets — over a relay, each costs ~100 bytes of headers).
    slow_link: AtomicBool,
}

/// After a reported loss, packets carry the frame before theirs this long.
const REDUNDANT_AFTER_LOSS: Duration = Duration::from_secs(30);

impl AudioShape {
    pub fn report(&self, frame_ms: u8, lost: u16) {
        self.peer_frame_ms.store(frame_ms as u32, Ordering::Relaxed);
        if lost > 0 {
            *self.last_loss.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
        let sent = self.sent.swap(0, Ordering::Relaxed);
        self.loss_pct.store(rdp_hyper::audio::loss_pct(self.loss_pct.load(Ordering::Relaxed), lost as u32, sent), Ordering::Relaxed);
    }

    /// Samples per channel in the next frame.
    fn frame_samples(&self) -> usize {
        let long = self.long.load(Ordering::Relaxed) || self.slow_link.load(Ordering::Relaxed);
        if long && self.peer_frame_ms.load(Ordering::Relaxed) >= 20 {
            FRAME_SAMPLES * 2
        } else {
            FRAME_SAMPLES
        }
    }

    /// Each packet carries the frame before its own too: until the receiver
    /// says it's losing none.
    fn redundant(&self) -> bool {
        self.peer_frame_ms.load(Ordering::Relaxed) == 0
            || self.last_loss.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() < REDUNDANT_AFTER_LOSS)
    }
}

/// Capture → Opus → send, on its own thread.
pub struct AudioSender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioSender {
    pub fn start(source: Source, channels: usize, bitrate: i32, clock: fn() -> u64, send: SendFn, shape: Arc<AudioShape>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("hyper-audio-tx".into())
            .spawn(move || {
                // Audio first: this thread wakes every few ms to send ~1%
                // of the stream's bytes, but sound is what a late packet
                // hurts most — a game holding every core at normal priority
                // mustn't hold it up (the video sender and the input thread
                // are raised the same way). `audiofirst=0`: normal priority.
                if crate::host_ab::ab("audiofirst", true) {
                    crate::net_tune::raise_thread();
                }
                if let Err(e) = send_loop(source, channels, bitrate, clock, send, &shape, &s) {
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
    shape: &AudioShape,
    stop: &AtomicBool,
) -> Result<(), Box<dyn std::error::Error>> {
    wasapi::com_init();
    let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
    // A microphone: Opus's speech mode, which can tell a pause in speech
    // from speech (`dtx`) — a mic is never digitally silent, so without it
    // a call sends every frame of its pauses too. (4 ms more look-ahead than
    // the low-delay mode game sound keeps.)
    let mic = matches!(source, Source::Mic(_));
    let open = |app: opus::Application, bits: i32| -> Result<opus::Encoder, opus::Error> {
        let mut enc = opus::Encoder::new(wasapi::RATE, ch, app)?;
        enc.set_bitrate(opus::Bitrate::Bits(bits))?;
        if mic {
            enc.set_dtx(true)?;
        }
        Ok(enc)
    };
    let mut enc = open(if mic { opus::Application::Voip } else { opus::Application::LowDelay }, bitrate)?;
    // What the encoder runs with now (see `tune` below): its bitrate, the
    // loss its FEC is sized for, and (game sound) whether it's in the mode
    // that can code FEC.
    let (mut bits, mut fec_loss, mut fec_mode) = (bitrate, 0i32, false);
    let mut tuned = Instant::now();
    let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2 * channels * 4);
    let mut out = vec![0u8; 1500];
    let mut prev: Option<Vec<u8>> = None;
    let mut seq = 0u32;
    // Silent frames in a row: past the tail, nothing is sent (the receiver
    // takes the pause for silence and lets its output close).
    let mut silent_run = 0u32;
    // Read as the engine signals each captured period; where a device won't
    // signal, polled — the host tighter (game audio latency shows), the
    // client's mic less often (the laptop's CPU idles meanwhile).
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
        cap.wait(poll);
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
        // Twice a second: the bitrate the link affords and the FEC its loss
        // calls for.
        if tuned.elapsed() >= Duration::from_millis(500) {
            tuned = Instant::now();
            let want_bits = match shape.link_bps.get() {
                Some(link) if crate::host_ab::ab("audiorate", true) => game_bitrate(bitrate, bits, link()),
                _ => bitrate,
            };
            shape.slow_link.store(shape.link_bps.get().is_some() && want_bits < GAME_BITRATE_MID, Ordering::Relaxed);
            let want_loss = if crate::host_ab::ab("opusfec", true) { shape.loss_pct.load(Ordering::Relaxed) as i32 } else { 0 };
            // Game sound's low-delay mode codes no FEC (CELT only): while the
            // path loses packets, the general mode (4 ms more look-ahead),
            // whose frames can carry a coarse copy of the one before. The
            // measurement (`loss_recovery_compared`): with the copies packets
            // carry anyway, +4-5 dB at 5-10% loss.
            let want_mode = !mic && (want_loss >= FEC_MODE_LOSS_PCT || fec_mode && want_loss > 0);
            if want_mode != fec_mode {
                match open(if want_mode { opus::Application::Audio } else { opus::Application::LowDelay }, want_bits) {
                    Ok(e) => {
                        tracing::info!(loss_pct = want_loss, fec = want_mode, "game sound encoder: {}", if want_mode { "general mode, with FEC" } else { "low-delay mode" });
                        (enc, fec_mode, bits, fec_loss) = (e, want_mode, want_bits, 0);
                    }
                    Err(e) => tracing::debug!(error = %e, "game sound encoder couldn't change mode"),
                }
            }
            if want_bits != bits && enc.set_bitrate(opus::Bitrate::Bits(want_bits)).is_ok() {
                tracing::info!(kbps = want_bits / 1000, "game sound bitrate follows the link");
                bits = want_bits;
            }
            if want_loss != fec_loss && enc.set_inband_fec(want_loss > 0).and_then(|_| enc.set_packet_loss_perc(want_loss)).is_ok() {
                fec_loss = want_loss;
            }
        }
        loop {
            let frame_len = shape.frame_samples() * channels;
            if pcm.len() < frame_len {
                break;
            }
            // (Encoded even when not sent: the encoder's state runs on.)
            silent_run = if is_silent(&pcm[..frame_len]) { silent_run.saturating_add(1) } else { 0 };
            let n = enc.encode_float(&pcm[..frame_len], &mut out)?;
            pcm.drain(..frame_len);
            // A pause in speech (`dtx`: a frame of a byte or two, nothing to
            // decode — the receiver's concealment fades out), or silence.
            if silent_run > SILENT_TAIL || n <= 2 {
                // (Sequence numbers carry on from the last one sent: a pause,
                // not a loss, to the receiver.)
                prev = None;
                continue;
            }
            SENT.fetch_add(1, Ordering::Relaxed);
            let frame = out[..n].to_vec();
            seq = seq.wrapping_add(1);
            let mut frames = Vec::with_capacity(2);
            if let Some(p) = prev.take().filter(|_| shape.redundant()) {
                frames.push(p);
            }
            frames.push(frame.clone());
            prev = Some(frame);
            let packet = AudioPacket { seq, channels: channels as u8, capture_us: clock(), frames };
            send(&packet.to_bytes());
            shape.sent.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Game sound's bitrate when the link is slow (stereo Opus: still clean
/// music at 64, fine at 48), and the video bitrates (bps) between the steps.
/// Down below 3 and 1.5 Mbps; up only above 4 and 2, so a rate controller
/// hovering at a step doesn't flip the sound every second. At 1 Mbps the
/// full 128 kbps, with the copy of the frame before each packet carries while
/// anything is lost, was a quarter of the link.
const GAME_BITRATE_MID: i32 = 64_000;
const GAME_BITRATE_LOW: i32 = 48_000;

fn game_bitrate(full: i32, now: i32, link_bps: u32) -> i32 {
    let mbps = link_bps as f64 / 1e6;
    let (mid, low) = (GAME_BITRATE_MID.min(full), GAME_BITRATE_LOW.min(full));
    if now >= full {
        if mbps < 1.5 {
            low
        } else if mbps < 3.0 {
            mid
        } else {
            full
        }
    } else if now >= mid {
        if mbps < 1.5 {
            low
        } else if mbps > 4.0 {
            full
        } else {
            mid
        }
    } else if mbps > 4.0 {
        full
    } else if mbps > 2.0 {
        mid
    } else {
        low
    }
}

/// Loss (%) at which game sound switches to the mode that codes FEC; it
/// switches back once none is reported.
const FEC_MODE_LOSS_PCT: i32 = 2;

/// Audio playback on its own thread: the network receive threads hand packets
/// over through a bounded queue and never wait on the audio device (opening
/// one can take seconds, or hang on a broken driver).
pub struct AudioSink {
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Frames concealed (lost) since the last [`report`](Self::report).
    lost: Arc<AtomicU32>,
    last_report: Option<Instant>,
}

impl AudioSink {
    /// Play to `device` (endpoint id) or the default output.
    pub fn start(device: Option<String>) -> Self {
        // ~0.6 s of 10 ms packets: a stalled device drops audio, not video.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        let lost = Arc::new(AtomicU32::new(0));
        let lost_rx = lost.clone();
        let thread = std::thread::Builder::new()
            .name("hyper-audio-rx".into())
            .spawn(move || {
                wasapi::com_init();
                let mut r = AudioReceiver::new(device);
                r.lost = lost_rx;
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(body) => r.on_packet(&body),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => r.idle(),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), thread, lost, last_report: None }
    }

    pub fn push(&self, body: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(body.to_vec());
        }
    }

    /// What to tell the sender (`Control::AudioReport`), when due: at the
    /// first packet, then every 2 s while packets come.
    pub fn report(&mut self) -> Option<rdp_hyper::control::Control> {
        if self.last_report.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return None;
        }
        self.last_report = Some(Instant::now());
        let lost = self.lost.swap(0, Ordering::Relaxed).min(u16::MAX as u32) as u16;
        Some(rdp_hyper::control::Control::AudioReport { frame_ms: LONGEST_FRAME_MS, lost })
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
    /// Samples per channel in the last frame decoded: what concealment
    /// stands in for.
    frame_samples: usize,
    /// Frames concealed, counted up for the sender's report.
    lost: Arc<AtomicU32>,
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
            pcm: vec![0.0; MAX_FRAME_SAMPLES * 2],
            jitter: JitterTarget::new(Instant::now()),
            underruns: 0,
            last_packet: None,
            frame_samples: FRAME_SAMPLES,
            lost: Arc::new(AtomicU32::new(0)),
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
        self.seq.set_fec(crate::host_ab::ab("opusfec", true));
        player.set_catchup(crate::host_ab::ab("catchup", true));
        let missing = self.seq.lost_packets;
        for d in self.seq.push(&p) {
            if let Some(n) = decode(dec, d, &mut self.pcm, self.frame_samples, ch as usize) {
                self.frame_samples = n;
                player.push(&self.pcm[..n * ch as usize]);
            }
        }
        // Packets that never came, before any copy or FEC repaired them: the
        // sender's measure of the path (its copies and FEC follow it).
        self.lost.fetch_add(self.seq.lost_packets.wrapping_sub(missing), Ordering::Relaxed);
    }
}

/// Decode one of a [`Sequencer`](rdp_hyper::audio::Sequencer)'s steps into
/// `pcm`; returns the samples per channel, `None` for nothing to play. A frame
/// decodes to however long it is; concealment and FEC recovery stand in for a
/// lost frame as long as the last one (`frame_samples`).
pub(crate) fn decode(dec: &mut opus::Decoder, d: Decode, pcm: &mut [f32], frame_samples: usize, ch: usize) -> Option<usize> {
    let r = match d {
        Decode::Frame(f) => dec.decode_float(&f, pcm, false),
        Decode::Conceal => dec.decode_float(&[], &mut pcm[..frame_samples * ch], false),
        // The lost frame from the next one's in-band FEC copy (concealed by
        // the decoder where it carries none).
        Decode::Fec(f) => dec.decode_float(&f, &mut pcm[..frame_samples * ch], true),
        // A new stream from the sender (its mic was reopened).
        Decode::Restart => {
            let _ = dec.reset_state();
            return None;
        }
    };
    r.ok()
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

/// Something about a watched endpoint's recording sessions may have changed
/// (see [`ListenerWatch`]); also set to end a [`ListenerWatch::wait`] early.
#[derive(Default)]
pub struct Changed {
    flag: std::sync::Mutex<bool>,
    cond: std::sync::Condvar,
}

impl Changed {
    pub fn notify(&self) {
        *self.flag.lock().unwrap() = true;
        self.cond.notify_all();
    }

    /// Until notified, or `timeout`.
    fn wait(&self, timeout: Duration) {
        let g = self.flag.lock().unwrap();
        let (mut g, _) = self.cond.wait_timeout_while(g, timeout, |set| !*set).unwrap();
        *g = false;
    }
}

#[windows::core::implement(windows::Win32::Media::Audio::IAudioSessionNotification)]
struct SessionCreated(Arc<Changed>);

impl windows::Win32::Media::Audio::IAudioSessionNotification_Impl for SessionCreated_Impl {
    fn OnSessionCreated(&self, _: windows::core::Ref<windows::Win32::Media::Audio::IAudioSessionControl>) -> windows::core::Result<()> {
        self.0.notify();
        Ok(())
    }
}

#[windows::core::implement(windows::Win32::Media::Audio::IAudioSessionEvents)]
struct SessionEvents(Arc<Changed>);

impl windows::Win32::Media::Audio::IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &windows::core::PCWSTR, _: *const windows::core::GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &windows::core::PCWSTR, _: *const windows::core::GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: windows::core::BOOL, _: *const windows::core::GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const windows::core::GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const windows::core::GUID, _: *const windows::core::GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: windows::Win32::Media::Audio::AudioSessionState) -> windows::core::Result<()> {
        self.0.notify();
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: windows::Win32::Media::Audio::AudioSessionDisconnectReason) -> windows::core::Result<()> {
        self.0.notify();
        Ok(())
    }
}

/// Which apps record from a capture endpoint, told by Windows' audio-session
/// notifications (a session made, started, stopped, gone) instead of asking
/// three times a second for the length of a stream.
pub struct ListenerWatch {
    mgr: windows::Win32::Media::Audio::IAudioSessionManager2,
    created: windows::Win32::Media::Audio::IAudioSessionNotification,
    /// Sessions whose state changes we hear about (by session instance id).
    watched: std::collections::HashMap<String, (windows::Win32::Media::Audio::IAudioSessionControl, windows::Win32::Media::Audio::IAudioSessionEvents)>,
    changed: Arc<Changed>,
}

impl ListenerWatch {
    /// Watch capture endpoint `id`; `changed` is notified on every change
    /// (and may be notified by the owner to end a wait). `None` where the
    /// notifications can't be had (the caller polls instead).
    pub fn new(id: &str, changed: Arc<Changed>) -> Option<Self> {
        use windows::Win32::Media::Audio::*;
        use windows::Win32::System::Com::*;
        wasapi::com_init();
        unsafe {
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            let dev = e.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).ok()?;
            let created: IAudioSessionNotification = SessionCreated(changed.clone()).into();
            mgr.RegisterSessionNotification(&created).ok()?;
            Some(Self { mgr, created, watched: Default::default(), changed })
        }
    }

    /// Whether any app is recording from the endpoint now. (Also starts
    /// watching sessions made since the last look: Windows only reports
    /// sessions created after an enumeration.)
    pub fn listening(&mut self) -> bool {
        use windows::core::Interface;
        use windows::Win32::Media::Audio::*;
        let mut any = false;
        unsafe {
            let Ok(list) = self.mgr.GetSessionEnumerator() else { return false };
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(s) = list.GetSession(i) else { continue };
                if s.GetState().is_ok_and(|st| st == AudioSessionStateActive) {
                    any = true;
                }
                let key = s
                    .cast::<IAudioSessionControl2>()
                    .ok()
                    .and_then(|s2| s2.GetSessionInstanceIdentifier().ok())
                    .map(|p| {
                        let id = p.to_string().unwrap_or_default();
                        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
                        id
                    });
                if let Some(key) = key.filter(|k| !self.watched.contains_key(k)) {
                    let events: IAudioSessionEvents = SessionEvents(self.changed.clone()).into();
                    if s.RegisterAudioSessionNotification(&events).is_ok() {
                        self.watched.insert(key, (s, events));
                    }
                }
            }
        }
        any
    }

    /// Until something changed, the owner notified, or `timeout`.
    pub fn wait(&self, timeout: Duration) {
        self.changed.wait(timeout);
    }
}

impl Drop for ListenerWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mgr.UnregisterSessionNotification(&self.created);
            for (s, events) in self.watched.values() {
                let _ = s.UnregisterAudioSessionNotification(events);
            }
        }
    }
}

/// Apps recording from a microphone other than `except` (the stream's), as
/// `(app, microphone)`: their sound can't be the client's, so the user should
/// pick the stream's (or "Default") in them.
pub fn other_mic_users(except: &str) -> Vec<(String, String)> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    let own = std::process::id();
    let mut out = Vec::new();
    unsafe {
        let Ok(e): windows::core::Result<IMMDeviceEnumerator> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return out;
        };
        let Ok(devices) = e.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..devices.GetCount().unwrap_or(0) {
            let Ok(dev) = devices.Item(i) else { continue };
            let id = dev.GetId().ok().map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
                s
            });
            if id.as_deref() == Some(except) {
                continue;
            }
            let Ok(mgr): windows::core::Result<IAudioSessionManager2> = dev.Activate(CLSCTX_ALL, None) else { continue };
            let Ok(list) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..list.GetCount().unwrap_or(0) {
                let Ok(s) = list.GetSession(j) else { continue };
                if !s.GetState().is_ok_and(|st| st == AudioSessionStateActive) {
                    continue;
                }
                let Some(pid) = s.cast::<IAudioSessionControl2>().ok().and_then(|s2| s2.GetProcessId().ok()) else { continue };
                if pid == own || pid == 0 {
                    continue;
                }
                if let Some(app) = process_name(pid) {
                    out.push((app, wasapi::friendly_name(&dev)));
                }
            }
        }
    }
    out
}

/// A process's executable name, without `.exe`.
fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        ok.then(|| {
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(path)
        })
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

    /// Long frames and no copies only once the receiver says it takes them
    /// and loses nothing; a loss brings the copies back.
    #[test]
    fn packets_follow_what_the_receiver_reports() {
        let shape = AudioShape::default();
        shape.long.store(true, Ordering::Relaxed);
        assert_eq!((shape.frame_samples(), shape.redundant()), (FRAME_SAMPLES, true), "an older receiver");
        shape.report(20, 0);
        assert_eq!((shape.frame_samples(), shape.redundant()), (FRAME_SAMPLES * 2, false));
        shape.report(20, 2);
        assert!(shape.redundant(), "after a loss");
        shape.long.store(false, Ordering::Relaxed);
        assert_eq!(shape.frame_samples(), FRAME_SAMPLES, "a game's sound");
        let ten_only = AudioShape::default();
        ten_only.long.store(true, Ordering::Relaxed);
        ten_only.report(10, 0);
        assert_eq!(ten_only.frame_samples(), FRAME_SAMPLES);
    }

    /// Game sound steps down as the video's bitrate falls, and back up only
    /// well above each step.
    #[test]
    fn game_sound_follows_the_link_with_hysteresis() {
        let full = 128_000;
        let mut now = full;
        let mut walk = |mbps: f64| {
            now = game_bitrate(full, now, (mbps * 1e6) as u32);
            now / 1000
        };
        assert_eq!(walk(20.0), 128);
        assert_eq!(walk(2.9), 64);
        assert_eq!(walk(3.5), 64, "not back up at 3.5");
        assert_eq!(walk(1.0), 48);
        assert_eq!(walk(1.8), 48, "not back up at 1.8");
        assert_eq!(walk(2.5), 64);
        assert_eq!(walk(4.5), 128);
        assert_eq!(walk(0.8), 48, "straight down");
        // A mic's 48 kbps is never raised.
        assert_eq!(game_bitrate(48_000, 48_000, 50_000_000), 48_000);
        // The slow link's 20 ms frames, where the receiver takes them.
        let shape = AudioShape::default();
        shape.report(20, 0);
        shape.slow_link.store(true, Ordering::Relaxed);
        assert_eq!(shape.frame_samples(), FRAME_SAMPLES * 2);
    }

    /// A receiver plays 20 ms frames whole and conceals a lost one as long.
    #[test]
    fn long_frames_decode_and_conceal_whole() {
        let mut enc = opus::Encoder::new(wasapi::RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let tone: Vec<f32> = (0..FRAME_SAMPLES * 2).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let mut out = vec![0u8; 1500];
        let n = enc.encode_float(&tone, &mut out).unwrap();
        let mut dec = opus::Decoder::new(wasapi::RATE, opus::Channels::Mono).unwrap();
        let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * 2];
        assert_eq!(dec.decode_float(&out[..n], &mut pcm, false).unwrap(), FRAME_SAMPLES * 2);
        assert_eq!(dec.decode_float(&[], &mut pcm[..FRAME_SAMPLES * 2], false).unwrap(), FRAME_SAMPLES * 2);
    }

    /// What each way of riding out loss is worth, on a simulated lossy link:
    /// the decoded sound against the same stream decoded without loss (SNR,
    /// dB; higher is better), for game sound (stereo, music-like) and a mic
    /// (mono, speech-like), 10 ms frames. `cargo test -p rdp-client --bin
    /// rdpio --release loss_recovery -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement (~25 s)"]
    fn loss_recovery_compared() {
        // A seeded generator, so runs compare.
        let mut seed = 0x2545_f491u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        let secs = 20;
        let n = wasapi::RATE as usize * secs;
        // Music-like: a chord whose notes change every 250 ms, a hi-hat-ish
        // noise burst on each beat; the right channel a little different.
        let mut music = Vec::with_capacity(n * 2);
        let notes = [220.0f32, 246.9, 261.6, 293.7, 329.6, 349.2, 392.0];
        for i in 0..n {
            let t = i as f32 / wasapi::RATE as f32;
            let beat = (t * 4.0) as usize;
            let root = notes[beat * 3 % notes.len()];
            let mut s = 0.0;
            for (k, mul) in [1.0f32, 1.25, 1.5, 2.0].iter().enumerate() {
                s += (std::f32::consts::TAU * root * mul * t).sin() * 0.12 / (k as f32 + 1.0);
            }
            let since_beat = t * 4.0 - beat as f32;
            let hat = if since_beat < 0.03 { (rnd() - 0.5) * 0.3 * (1.0 - since_beat / 0.03) } else { 0.0 };
            music.push(s + hat);
            music.push(s * 0.8 + hat + (std::f32::consts::TAU * root * 3.0 * t).sin() * 0.03);
        }
        // Speech-like: a voice whose pitch glides, in syllables (4 a second),
        // with pauses between phrases.
        let mut speech = Vec::with_capacity(n);
        let mut phase = 0.0f32;
        for i in 0..n {
            let t = i as f32 / wasapi::RATE as f32;
            let f0 = 140.0 + 40.0 * (t * 1.3).sin();
            phase = (phase + f0 / wasapi::RATE as f32) % 1.0;
            let syllable = (std::f32::consts::PI * (t * 4.0).fract()).sin().powi(2);
            let phrase = if (t % 3.0) < 2.4 { 1.0 } else { 0.0 };
            let mut s = 0.0;
            for h in 1..=12 {
                let formant = (-((h as f32 * f0 - 700.0) / 500.0).powi(2)).exp() + 0.5 * (-((h as f32 * f0 - 1500.0) / 400.0).powi(2)).exp();
                s += (std::f32::consts::TAU * h as f32 * phase).sin() * formant * 0.08;
            }
            speech.push(s * syllable * phrase + (rnd() - 0.5) * 0.002);
        }
        let frames_total = n / FRAME_SAMPLES;
        // Loss patterns: random 5% and 10%, and 10% in bursts (a loss makes
        // the next one 40% likely). Runs are capped at 3 (longer ones are
        // concealed by every scheme alike, and would misalign the comparison).
        let mut pattern = |rate: f32, bursty: bool| {
            let mut lost = vec![false; frames_total];
            let mut run = 0;
            for i in 1..frames_total {
                let p = if bursty && lost[i - 1] { 0.4 } else { rate };
                lost[i] = run < 3 && rnd() < p;
                run = if lost[i] { run + 1 } else { 0 };
            }
            lost
        };
        let patterns = [("5% random", pattern(0.05, false)), ("10% random", pattern(0.10, false)), ("10% bursty", pattern(0.067, true))];
        struct Scheme {
            name: &'static str,
            app: opus::Application,
            fec: bool,
            redundant: bool,
        }
        let schemes = |app: opus::Application| {
            [
                Scheme { name: "concealment only", app, fec: false, redundant: false },
                Scheme { name: "copy of the frame before (now)", app, fec: false, redundant: true },
                Scheme { name: "in-band FEC", app: opus::Application::Audio, fec: true, redundant: false },
                Scheme { name: "in-band FEC + copy", app: opus::Application::Audio, fec: true, redundant: true },
            ]
        };
        let run = |signal: &[f32], ch: usize, bitrate: i32, s: &Scheme, lost: &[bool], loss_pct: i32| -> (f64, f64) {
            let channels = if ch == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            let mut enc = opus::Encoder::new(wasapi::RATE, channels, if ch == 1 && s.app == opus::Application::Audio { opus::Application::Voip } else { s.app }).unwrap();
            enc.set_bitrate(opus::Bitrate::Bits(bitrate)).unwrap();
            if s.fec {
                enc.set_inband_fec(true).unwrap();
                enc.set_packet_loss_perc(loss_pct).unwrap();
            }
            let mut out = vec![0u8; 1500];
            let mut coded = Vec::with_capacity(frames_total);
            let mut bytes = 0usize;
            for f in signal.chunks_exact(FRAME_SAMPLES * ch) {
                let k = enc.encode_float(f, &mut out).unwrap();
                bytes += k;
                coded.push(out[..k].to_vec());
            }
            let decode_all = |lost: &[bool]| {
                let mut dec = opus::Decoder::new(wasapi::RATE, channels).unwrap();
                let mut seq = rdp_hyper::audio::Sequencer::new();
                seq.set_fec(s.fec);
                let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * 2];
                let mut played = Vec::with_capacity(signal.len());
                for (i, frame) in coded.iter().enumerate() {
                    if lost[i] {
                        continue;
                    }
                    let mut frames = Vec::new();
                    if s.redundant && i > 0 {
                        frames.push(coded[i - 1].clone());
                    }
                    frames.push(frame.clone());
                    let p = AudioPacket { seq: i as u32 + 1, channels: ch as u8, capture_us: i as u64 * 10_000, frames };
                    for d in seq.push(&p) {
                        if let Some(k) = decode(&mut dec, d, &mut pcm, FRAME_SAMPLES, ch) {
                            played.extend_from_slice(&pcm[..k * ch]);
                        }
                    }
                }
                played
            };
            let clean = decode_all(&vec![false; frames_total]);
            let lossy = decode_all(lost);
            let len = clean.len().min(lossy.len());
            assert!(clean.len().abs_diff(lossy.len()) <= FRAME_SAMPLES * ch * 4, "misaligned: {} vs {}", clean.len(), lossy.len());
            let (sig, err) = clean[..len].iter().zip(&lossy[..len]).fold((0f64, 0f64), |(s, e), (a, b)| (s + (*a as f64).powi(2), e + ((a - b) as f64).powi(2)));
            let packet_bytes = if s.redundant { bytes * 2 } else { bytes };
            (10.0 * (sig / err.max(1e-12)).log10(), packet_bytes as f64 * 8.0 / secs as f64 / 1000.0)
        };
        for (label, signal, ch, bitrate, app) in [
            ("game sound, stereo 128 kbps", &music, 2usize, 128_000, opus::Application::LowDelay),
            ("game sound, stereo 64 kbps", &music, 2, 64_000, opus::Application::LowDelay),
            ("mic, mono 48 kbps", &speech, 1, 48_000, opus::Application::Voip),
        ] {
            eprintln!("{label}:");
            for s in schemes(app) {
                let mut line = format!("  {:32}", s.name);
                for (name, lost) in &patterns {
                    let pct = (lost.iter().filter(|l| **l).count() * 100 / frames_total) as i32;
                    let (snr, kbps) = run(signal, ch, bitrate, &s, lost, pct.max(1));
                    line += &format!("  {name}: {snr:5.1} dB ({kbps:.0} kbps)");
                }
                eprintln!("{line}");
            }
        }
    }

    /// A capture device that can't be opened doesn't end the sender (it
    /// retries, for when one appears), and dropping the sender doesn't wait
    /// out the retry.
    #[test]
    fn a_missing_device_is_retried_and_dropping_is_prompt() {
        let sender = AudioSender::start(Source::Mic(Some("no such device".into())), 1, 32_000, || 0, Arc::new(|_: &[u8]| {}), Default::default());
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
    /// Which apps record from which microphones now (read only). `cargo test -p rdp-client --bin rdpio other_mics -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn other_mics_in_use() {
        wasapi::com_init();
        eprintln!("recording now: {:?}", other_mic_users(""));
    }

    /// An app starting to record from the virtual mic is heard of at once,
    /// from Windows, not found by polling. `cargo test -p rdp-client --bin rdpio recording_app -- --ignored --nocapture`
    #[test]
    #[ignore = "needs Steam Streaming Microphone"]
    fn a_recording_app_is_heard_of_without_polling() {
        wasapi::com_init();
        let mic = wasapi::find(Flow::Capture, "Steam Streaming Microphone").expect("Steam Streaming Microphone");
        let mut watch = ListenerWatch::new(&mic.id, Arc::new(Changed::default())).expect("session notifications");
        let before = watch.listening();
        let started = Instant::now();
        let capture = wasapi::Capture::new(Some(&mic.id), Flow::Capture, false, 1).unwrap();
        watch.wait(Duration::from_secs(3));
        let heard = started.elapsed();
        eprintln!("listening before: {before}; heard of a recording app after {heard:?}");
        assert!(heard < Duration::from_secs(3), "no notification");
        assert!(watch.listening());
        drop(capture);
    }

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
        // As an older host hears it (10 ms frames, each with the one before),
        // then as this build's does (20 ms, no copies), which also leaves out
        // the pause after the tone (faint noise: a room between words).
        for long in [false, true] {
            let sink = Arc::new(AudioSink::start(Some(speakers.id.clone())));
            let to_sink = sink.clone();
            let shape = Arc::new(AudioShape::default());
            if long {
                shape.long.store(true, Ordering::Relaxed);
                shape.report(LONGEST_FRAME_MS, 0);
            }
            let (sent, bytes) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
            let (s, b) = (sent.clone(), bytes.clone());
            let send = Arc::new(move |p: &[u8]| {
                s.fetch_add(1, Ordering::Relaxed);
                b.fetch_add(p.len() as u32, Ordering::Relaxed);
                to_sink.push(p)
            });
            let sender = AudioSender::start(Source::Mic(Some(mic.id.clone())), 1, 64_000, || 0, send, shape);
            let tone_out = Player::new(Some(mic_input.id.clone()), 1).expect("player");
            let mut ear = Capture::new(Some(&speakers.id), Flow::Render, true, 2).expect("loopback");
            let (mut phase, mut heard, start) = (0f32, Vec::new(), Instant::now());
            let (mut pushed, mut noise) = (0usize, 1u32);
            let mut during_tone = 0;
            while start.elapsed() < Duration::from_secs(4) {
                let tone = start.elapsed() < Duration::from_secs(2);
                // Keep ~40 ms queued whatever the sleeps do.
                while pushed < (start.elapsed().as_secs_f64() * wasapi::RATE as f64) as usize + 1920 {
                    let chunk: Vec<f32> = (0..FRAME_SAMPLES)
                        .map(|_| {
                            phase = (phase + 440.0 / wasapi::RATE as f32) % 1.0;
                            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            if tone {
                                (phase * std::f32::consts::TAU).sin() * 0.5
                            } else {
                                (noise >> 8) as f32 / (1u32 << 24) as f32 * 0.002 - 0.001
                            }
                        })
                        .collect();
                    tone_out.push(&chunk);
                    pushed += FRAME_SAMPLES;
                }
                std::thread::sleep(Duration::from_millis(10));
                if tone {
                    ear.read(&mut heard).expect("loopback read");
                    during_tone = sent.load(Ordering::Relaxed);
                }
                assert!(!sender.is_finished(), "the capture thread ended");
            }
            drop(sender);
            let after = sent.load(Ordering::Relaxed) - during_tone;
            let tail = &heard[heard.len().saturating_sub(wasapi::RATE as usize)..];
            let rms = (tail.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / tail.len().max(1) as f64).sqrt();
            eprintln!(
                "{}: heard back {} samples, rms over the last 0.5 s {rms:.3} (sent 0.354); {during_tone} packets in 2 s of tone ({} B), {after} in 2 s of pause",
                if long { "20 ms" } else { "10 ms" },
                heard.len(),
                bytes.load(Ordering::Relaxed),
            );
            assert!(rms > 0.1, "the tone didn't make it through (rms {rms:.3})");
            if long {
                assert!(during_tone <= 110, "{during_tone} packets for 2 s of 20 ms frames");
                assert!(after < 40, "{after} packets for 2 s of a pause");
            }
        }
    }
}
