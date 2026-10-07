//! Call audio for the native Teams engine (`--teams-native`): the remote side's
//! Opus audio decoded to the speakers, and the local mic encoded to Opus for the
//! call.
//!
//! Plugs into [`rdp_webrtc`] as its [`MediaSink`] (inbound) and
//! [`AudioCaptureSource`] (outbound). The engine calls both from its async tasks,
//! so each keeps its state behind a mutex. Remote *video* is not rendered yet —
//! its track is logged and ignored.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use rdp_channels::rdpsnd::AudioSink;
use rdp_webrtc::{AudioCaptureSource, MediaSink};

use crate::audio::Win32Audio;
use crate::mic::Win32Mic;
use crate::session::MicSource;

/// Opus in RTP always runs a 48 kHz clock.
const RATE: u32 = 48_000;
/// 20 ms at 48 kHz: the packet size we encode (and Teams sends).
const FRAME_SAMPLES: usize = 960;
/// The largest Opus frame (120 ms) per channel: the decoder's output bound.
const MAX_FRAME_SAMPLES: usize = 5_760;
/// Decoded audio held back before playback starts, so one late packet at the
/// start doesn't underrun the device.
const PREBUFFER_MS: usize = 60;
/// Bytes per millisecond of 48 kHz stereo 16-bit PCM.
const PLAYBACK_BYTES_PER_MS: usize = 48 * 2 * 2;

/// Remote call audio → the default speakers.
#[derive(Default)]
pub struct CallAudioSink {
    inner: Mutex<Playback>,
}

#[derive(Default)]
struct Playback {
    out: Option<Win32Audio>,
    /// One decoder per remote audio track (Opus decoding is stateful).
    decoders: HashMap<String, opus::Decoder>,
    /// Decoded PCM held until [`PREBUFFER_MS`] is reached; empty afterwards.
    prebuffer: Vec<u8>,
    started: bool,
    /// Decoder output scratch (interleaved stereo).
    pcm: Vec<i16>,
}

impl MediaSink for CallAudioSink {
    fn on_track(&self, track_id: &str, kind: &str, codec: &str) {
        let Ok(mut p) = self.inner.lock() else { return };
        // Any audio track gets an Opus decoder: its first packet may be comfort
        // noise (`codec` is that packet's), with the speech in Opus after it.
        if kind == "audio" {
            match opus::Decoder::new(RATE, opus::Channels::Stereo) {
                Ok(d) => {
                    p.decoders.insert(track_id.to_string(), d);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not create an Opus decoder; call audio muted");
                    return;
                }
            }
            if p.out.is_none() {
                let mut out = Win32Audio::new();
                out.set_format(2, RATE, 16);
                p.out = Some(out);
            }
            tracing::info!(track_id, first_codec = codec, "remote call audio: decoding Opus to the speakers");
        } else if kind == "video" {
            tracing::info!(track_id, codec, "remote call video track (not rendered yet)");
        } else {
            tracing::warn!(track_id, kind, codec, "remote call track with no decoder; ignoring");
        }
    }

    fn on_rtp(&self, track_id: &str, codec: &str, payload: &[u8]) {
        // Comfort noise (CN) and anything else non-Opus: nothing to play.
        if !codec.eq_ignore_ascii_case("audio/opus") {
            return;
        }
        let Ok(mut guard) = self.inner.lock() else { return };
        let p = &mut *guard;
        let Some(dec) = p.decoders.get_mut(track_id) else {
            return;
        };
        p.pcm.resize(MAX_FRAME_SAMPLES * 2, 0);
        let samples = match dec.decode(payload, &mut p.pcm, false) {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "dropping an undecodable Opus packet");
                return;
            }
        };
        let bytes: Vec<u8> = p.pcm[..samples * 2]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let Some(out) = p.out.as_mut() else {
            return;
        };
        if p.started {
            out.play(&bytes);
            return;
        }
        p.prebuffer.extend_from_slice(&bytes);
        if p.prebuffer.len() >= PREBUFFER_MS * PLAYBACK_BYTES_PER_MS {
            let first = std::mem::take(&mut p.prebuffer);
            out.play(&first);
            p.started = true;
        }
    }

    /// Already-decoded call audio (the libwebrtc engine): straight to the
    /// speakers, in whatever format libwebrtc delivers (48 kHz stereo here).
    fn on_pcm(&self, _track_id: &str, sample_rate: u32, channels: u32, samples: &[i16]) {
        let Ok(mut guard) = self.inner.lock() else { return };
        let p = &mut *guard;
        let out = p.out.get_or_insert_with(|| {
            let mut out = Win32Audio::new();
            out.set_format(channels as u16, sample_rate, 16);
            tracing::info!(sample_rate, channels, "remote call audio: playing decoded PCM to the speakers");
            out
        });
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        if p.started {
            out.play(&bytes);
            return;
        }
        p.prebuffer.extend_from_slice(&bytes);
        let per_ms = (sample_rate as usize / 1000) * channels as usize * 2;
        if p.prebuffer.len() >= PREBUFFER_MS * per_ms {
            let first = std::mem::take(&mut p.prebuffer);
            out.play(&first);
            p.started = true;
        }
    }
}

/// The local mic → 20 ms Opus packets for the call.
#[derive(Default)]
pub struct CallMicSource {
    inner: Mutex<MicCapture>,
}

#[derive(Default)]
struct MicCapture {
    mic: Option<Win32Mic>,
    encoder: Option<opus::Encoder>,
    /// Captured mono samples not yet a whole 20 ms frame.
    pcm: Vec<i16>,
    /// Encoded packets waiting to be taken.
    ready: VecDeque<Vec<u8>>,
    /// Mic level diagnostics (see `poll_pcm`).
    level_peak: u16,
    level_frames: u64,
}

impl AudioCaptureSource for CallMicSource {
    fn start(&self, source_id: &str) -> bool {
        let Ok(mut c) = self.inner.lock() else { return false };
        if c.mic.is_some() {
            return true;
        }
        // `rdpio-audioinput-N` is waveIn device N (see `webrtc_devices`).
        let device = source_id
            .strip_prefix("rdpio-audioinput-")
            .and_then(|n| n.parse::<u32>().ok());
        let Some(mut mic) = Win32Mic::new().map(|m| m.with_device(device)) else {
            tracing::warn!("no microphone; the call sends no audio");
            return false;
        };
        let mut encoder =
            match opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "could not create an Opus encoder; the call sends no audio");
                    return false;
                }
            };
        // Teams' answer asks for DTX (`usedtx=1`): during silence the encoder
        // emits 1–2 byte frames, which the engine doesn't send.
        if let Err(e) = encoder.set_dtx(true) {
            tracing::warn!(error = %e, "could not enable Opus DTX");
        }
        mic.start(1, RATE, 16);
        *c = MicCapture {
            mic: Some(mic),
            encoder: Some(encoder),
            ..Default::default()
        };
        tracing::info!(source_id, ?device, "call mic capture started (48 kHz mono)");
        true
    }

    fn poll_frame(&self) -> Option<Vec<u8>> {
        let Ok(mut guard) = self.inner.lock() else { return None };
        let c = &mut *guard;
        if let Some(packet) = c.ready.pop_front() {
            return Some(packet);
        }
        let (Some(mic), Some(encoder)) = (c.mic.as_mut(), c.encoder.as_mut()) else {
            return None;
        };
        let captured = mic.poll();
        c.pcm.extend(
            captured
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );
        let mut packet = [0u8; 1500];
        while c.pcm.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = c.pcm.drain(..FRAME_SAMPLES).collect();
            match encoder.encode(&frame, &mut packet) {
                Ok(n) => c.ready.push_back(packet[..n].to_vec()),
                Err(e) => tracing::debug!(error = %e, "dropping a mic frame Opus couldn't encode"),
            }
        }
        c.ready.pop_front()
    }

    /// 10 ms (480 samples) of the captured 48 kHz mono PCM, for the libwebrtc
    /// engine (which does its own Opus encoding, DTX included).
    fn poll_pcm(&self) -> Option<Vec<i16>> {
        const FRAME_10MS: usize = 480;
        let Ok(mut guard) = self.inner.lock() else { return None };
        let c = &mut *guard;
        if c.pcm.len() < FRAME_10MS {
            let mic = c.mic.as_mut()?;
            let captured = mic.poll();
            c.pcm.extend(captured.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        }
        if c.pcm.len() < FRAME_10MS {
            return None;
        }
        let frame: Vec<i16> = c.pcm.drain(..FRAME_10MS).collect();
        // Diagnostic: the captured level, every ~3 s (0 = digital silence).
        let peak = frame.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        c.level_peak = c.level_peak.max(peak);
        c.level_frames += 1;
        if c.level_frames % 300 == 0 {
            tracing::info!(
                peak = c.level_peak,
                "call mic level (raw max |sample| of the last 3 s, of 32767)"
            );
            c.level_peak = 0;
        }
        Some(frame)
    }

    fn stop(&self) {
        if let Ok(mut c) = self.inner.lock() {
            if c.mic.is_some() {
                tracing::info!("call mic capture stopped");
            }
            // Dropping the `Win32Mic` closes the capture device.
            *c = MicCapture::default();
        }
    }
}

