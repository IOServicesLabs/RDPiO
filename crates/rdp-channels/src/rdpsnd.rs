//! Audio output redirection (MS-RDPEA / RDPSND) over the static `rdpsnd`
//! virtual channel — play the remote session's sound locally.
//!
//! Each PDU is an `SNDPROLOG` header { msgType(1), bPad(1), bodySize(2) } + body.
//! The flow: server announces formats (`SNDC_FORMATS`) → client replies with the
//! formats it can play + a quality mode; the server may send a `SNDC_TRAINING`
//! round (echoed back) to gauge latency; then audio arrives as `SNDC_WAVE2`
//! (header + all PCM) or the legacy `SNDC_WAVE` (a WaveInfo carrying the first 4
//! data bytes, immediately followed by a headerless body = 4 pad bytes + the
//! rest). The client plays it and replies `SNDC_WAVECONFIRM`.
//!
//! Sans-I/O and OS-agnostic: [`RdpsndChannel`] turns inbound messages into the
//! responses to send and drives an [`AudioSink`] for playback; the session
//! frames the output with [`crate::svc`] and the platform supplies the sink.

/// A compressed or PCM audio format the sink may be asked to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Wave format tag (`WAVE_FORMAT_PCM`, `WAVE_FORMAT_AAC`, ...).
    pub tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub bits_per_sample: u16,
    /// Format-specific extra bytes (e.g. HE-AAC AudioSpecificConfig).
    pub extra: Vec<u8>,
}

/// An audio output device. Playback is abstracted so the protocol is testable.
pub trait AudioSink {
    /// Prepare to play PCM in this format (called when the format changes).
    fn set_format(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16);
    /// Queue a buffer of PCM samples in the current format.
    fn play(&mut self, pcm: &[u8]);
    /// Optional: the server selected a compressed format. Sinks that can decode
    /// in hardware (or via Media Foundation) should override this; the default
    /// implementation is a no-op so pure-PCM sinks keep working unchanged.
    fn set_compressed_format(&mut self, _format: &AudioFormat) {}
    /// Optional: play a compressed audio frame. The default drops it.
    fn play_compressed(&mut self, _format: &AudioFormat, _payload: &[u8]) {}
}

/// An [`AudioSink`] that discards audio — the default until a real device is
/// wired in. The protocol still completes (formats, training, wave confirms).
pub struct NullAudio;
impl AudioSink for NullAudio {
    fn set_format(&mut self, _c: u16, _s: u32, _b: u16) {}
    fn play(&mut self, _pcm: &[u8]) {}
}

const SNDC_CLOSE: u8 = 0x01;
const SNDC_WAVE: u8 = 0x02;
const SNDC_WAVECONFIRM: u8 = 0x05;
const SNDC_TRAINING: u8 = 0x06;
const SNDC_FORMATS: u8 = 0x07;
const SNDC_QUALITYMODE: u8 = 0x0C;
const SNDC_WAVE2: u8 = 0x0D;

const WAVE_FORMAT_PCM: u16 = 0x0001;
/// AAC audio (raw ADTS or audio-specific-config). Advertised over RDPSND.
/// Kept for re-enabling compressed audio (see `client_formats`).
#[allow(dead_code)]
const WAVE_FORMAT_AAC: u16 = 0x00FF;
/// MPEG HE-AAC audio with AudioSpecificConfig extra bytes.
#[allow(dead_code)]
const WAVE_FORMAT_MPEG_HEAAC: u16 = 0x1610;
const TSSNDCAPS_ALIVE: u32 = 0x0000_0001;
/// Dynamic quality: let the server choose the format/bitrate from what we
/// advertise. This is mstsc's default; HIGH can push the server toward a
/// compressed format we never advertised.
const SNDQUALITY_DYNAMIC: u16 = 0x0000;

/// A format we offer to receive.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Format {
    tag: u16,
    channels: u16,
    samples_per_sec: u32,
    bits_per_sample: u16,
    extra: Vec<u8>,
}

impl Format {
    fn block_align(&self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }
    fn avg_bytes_per_sec(&self) -> u32 {
        self.samples_per_sec * self.block_align() as u32
    }
    fn to_audio_format(&self) -> AudioFormat {
        AudioFormat {
            tag: self.tag,
            channels: self.channels,
            samples_per_sec: self.samples_per_sec,
            bits_per_sample: self.bits_per_sample,
            extra: self.extra.clone(),
        }
    }
}

/// The formats we advertise (the server references these by index). PCM only:
/// universal, and the server resamples its 48 kHz output to one of these.
///
/// AAC/HE-AAC were tried but broke the handshake: a Windows server parsing a
/// `WAVE_FORMAT_MPEG_HEAAC` entry expects a full `HEAACWAVEINFO` block (payload
/// type, profile-level, struct type, then the AudioSpecificConfig), not the bare
/// 2-byte ASC we sent. The malformed entry made the server reject the whole
/// Client Audio Formats PDU and loop formats→training→formats without ever
/// streaming a wave. To re-enable compressed audio, emit a correct HEAACWAVEINFO
/// in `formats_reply` for those tags; the `set_compressed_format`/`AacDecoder`
/// playback path is already in place.
fn client_formats() -> Vec<Format> {
    vec![
        Format {
            tag: WAVE_FORMAT_PCM,
            channels: 2,
            samples_per_sec: 44_100,
            bits_per_sample: 16,
            extra: Vec::new(),
        },
        Format {
            tag: WAVE_FORMAT_PCM,
            channels: 2,
            samples_per_sec: 22_050,
            bits_per_sample: 16,
            extra: Vec::new(),
        },
    ]
}

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Frame an RDPSND PDU (`SNDPROLOG` header + body).
fn message(msg_type: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + body.len());
    v.push(msg_type);
    v.push(0); // bPad
    v.extend_from_slice(&(body.len() as u16).to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// The audio channel state machine.
pub struct RdpsndChannel {
    formats: Vec<Format>,
    /// A legacy `SNDC_WAVE` WaveInfo awaiting its headerless data body:
    /// (timestamp, block_no, format index, first 4 audio bytes).
    pending_wave: Option<(u16, u8, u16, Vec<u8>)>,
    /// How many wave buffers have been played — `0` until the server actually
    /// streams audio. Logged so a silent session is unambiguous: handshake-only
    /// (count stays 0 → server isn't sending waves) vs audio flowing (count climbs).
    waves_played: u64,
}

impl Default for RdpsndChannel {
    fn default() -> Self {
        Self {
            formats: client_formats(),
            pending_wave: None,
            waves_played: 0,
        }
    }
}

impl RdpsndChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply `format_no` (an index into our advertised list) to the sink.
    fn select_format(&self, format_no: u16, sink: &mut dyn AudioSink) {
        if let Some(f) = self.formats.get(format_no as usize) {
            if f.tag == WAVE_FORMAT_PCM {
                sink.set_format(f.channels, f.samples_per_sec, f.bits_per_sample);
            } else {
                sink.set_compressed_format(&f.to_audio_format());
            }
        }
    }

    /// Build the client's audio-format reply.
    pub fn formats_reply(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&TSSNDCAPS_ALIVE.to_le_bytes()); // dwFlags
        b.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwVolume (full)
        b.extend_from_slice(&0u32.to_le_bytes()); // dwPitch
        b.extend_from_slice(&0u16.to_le_bytes()); // wDGramPort (we use the VC)
        b.extend_from_slice(&(self.formats.len() as u16).to_le_bytes());
        b.push(0); // cLastBlockConfirmed
        b.extend_from_slice(&0x0008u16.to_le_bytes()); // wVersion (Windows 8+ for DVC audio)
        b.push(0); // bPad
        for f in &self.formats {
            b.extend_from_slice(&f.tag.to_le_bytes());
            b.extend_from_slice(&f.channels.to_le_bytes());
            b.extend_from_slice(&f.samples_per_sec.to_le_bytes());
            b.extend_from_slice(&f.avg_bytes_per_sec().to_le_bytes());
            b.extend_from_slice(&f.block_align().to_le_bytes());
            b.extend_from_slice(&f.bits_per_sample.to_le_bytes());
            b.extend_from_slice(&(f.extra.len() as u16).to_le_bytes());
            b.extend_from_slice(&f.extra);
        }
        message(SNDC_FORMATS, &b)
    }

    /// The Client Quality Mode PDU (sent right after the formats reply).
    fn quality_mode() -> Vec<u8> {
        message(SNDC_QUALITYMODE, &[
            SNDQUALITY_DYNAMIC as u8,
            (SNDQUALITY_DYNAMIC >> 8) as u8,
            0,
            0,
        ])
    }

    /// Initial PDUs to send after the dynamic audio channel opens. Modern Windows
    /// hosts use AUDIO_PLAYBACK_DVC and wait for the client to advertise formats
    /// before streaming audio.
    pub fn initial_formats(&self) -> Vec<Vec<u8>> {
        vec![self.formats_reply(), Self::quality_mode()]
    }

    /// Parse the server's offered formats and keep the PCM ones we can play,
    /// echoing them back so the server has a format it can produce natively —
    /// the way mstsc/FreeRDP negotiate. The fixed header preceding the format
    /// array is 20 bytes (dwFlags/dwVolume/dwPitch/wDGramPort/wNumberOfFormats/
    /// cLastBlockConfirmed/wVersion/bPad); each `AUDIO_FORMAT` is 18 bytes +
    /// `cbSize` extra.
    fn parse_server_formats(body: &[u8]) -> Vec<Format> {
        let count = u16le(body, 14).unwrap_or(0) as usize;
        let mut out = Vec::new();
        let mut off = 20;
        for _ in 0..count {
            let Some(tag) = u16le(body, off) else { break };
            let channels = u16le(body, off + 2).unwrap_or(0);
            let samples_per_sec = u32le(body, off + 4).unwrap_or(0);
            let bits_per_sample = u16le(body, off + 14).unwrap_or(0);
            let cb = u16le(body, off + 16).unwrap_or(0) as usize;
            // Keep plain 16-bit PCM, mono/stereo — what waveOut plays directly.
            if tag == WAVE_FORMAT_PCM
                && bits_per_sample == 16
                && (1..=2).contains(&channels)
                && samples_per_sec > 0
            {
                out.push(Format {
                    tag,
                    channels,
                    samples_per_sec,
                    bits_per_sample,
                    extra: Vec::new(),
                });
            }
            off += 18 + cb;
        }
        out
    }

    fn wave_confirm(timestamp: u16, block_no: u8) -> Vec<u8> {
        message(SNDC_WAVECONFIRM, &[
            timestamp as u8,
            (timestamp >> 8) as u8,
            block_no,
            0, // bPad
        ])
    }

    /// Play a complete wave and return its confirm PDU.
    fn play_wave(
        &mut self,
        timestamp: u16,
        block_no: u8,
        format_no: u16,
        payload: &[u8],
        sink: &mut dyn AudioSink,
    ) -> Vec<u8> {
        self.waves_played += 1;
        if self.waves_played == 1 {
            tracing::info!(
                format_no,
                bytes = payload.len(),
                "rdpsnd: first wave received — audio is streaming"
            );
        } else if self.waves_played % 200 == 0 {
            tracing::info!(waves = self.waves_played, "rdpsnd: waves played");
        } else {
            tracing::debug!(format_no, block_no, bytes = payload.len(), "rdpsnd: wave");
        }
        self.select_format(format_no, sink);
        if let Some(f) = self.formats.get(format_no as usize) {
            if f.tag == WAVE_FORMAT_PCM {
                sink.play(payload);
            } else {
                sink.play_compressed(&f.to_audio_format(), payload);
            }
        }
        Self::wave_confirm(timestamp, block_no)
    }

    /// Process one complete inbound RDPSND message, returning responses to send.
    pub fn process(&mut self, msg: &[u8], sink: &mut dyn AudioSink) -> Vec<Vec<u8>> {
        // A pending legacy WaveInfo is followed by a headerless data body
        // (4 pad bytes + the rest of the audio). It starts with 0x00, which is
        // not a valid msgType, so this is unambiguous.
        if let Some((timestamp, block_no, format_no, first4)) = self.pending_wave.take() {
            if msg.first() == Some(&0x00) {
                let mut pcm = first4;
                pcm.extend_from_slice(msg.get(4..).unwrap_or(&[]));
                return vec![self.play_wave(timestamp, block_no, format_no, &pcm, sink)];
            }
            // Otherwise fall through and treat `msg` as a fresh PDU.
        }

        if msg.len() < 4 {
            return Vec::new();
        }
        let msg_type = msg[0];
        let body = &msg[4..];
        tracing::debug!(
            msg_type = format!("{msg_type:#04x}"),
            body_len = body.len(),
            "rdpsnd: inbound PDU"
        );
        match msg_type {
            SNDC_FORMATS => {
                // wNumberOfFormats sits after dwFlags/dwVolume/dwPitch/wDGramPort.
                let server_formats = u16le(body, 14).unwrap_or(0);
                // Echo back the PCM subset of the server's OWN formats (mstsc/
                // FreeRDP behavior). Advertising a client-invented format the
                // server didn't offer (e.g. 44100 when it only has 48000/22050)
                // leaves the server with nothing it can stream → silent audio.
                // Fall back to our default PCM list only if the server offered no
                // usable PCM format.
                let selected = Self::parse_server_formats(body);
                if !selected.is_empty() {
                    self.formats = selected;
                }
                let rates: Vec<u32> = self.formats.iter().map(|f| f.samples_per_sec).collect();
                tracing::info!(
                    server_formats,
                    client_formats = self.formats.len(),
                    ?rates,
                    "rdpsnd: audio formats negotiated"
                );
                vec![self.formats_reply(), Self::quality_mode()]
            }
            SNDC_TRAINING => {
                // Echo wTimeStamp + wPackSize so the server can time the round trip.
                let echo = body.get(0..4).unwrap_or(&[0, 0, 0, 0]).to_vec();
                vec![message(SNDC_TRAINING, &echo)]
            }
            SNDC_WAVE => {
                // WaveInfo: wTimeStamp, wFormatNo, cBlockNo, bPad[3], Data[4].
                let timestamp = u16le(body, 0).unwrap_or(0);
                let format_no = u16le(body, 2).unwrap_or(0);
                let block_no = body.get(4).copied().unwrap_or(0);
                let first4 = body.get(8..12).unwrap_or(&[]).to_vec();
                self.pending_wave = Some((timestamp, block_no, format_no, first4));
                Vec::new() // confirm after the data body arrives
            }
            SNDC_WAVE2 => {
                // wTimeStamp, wFormatNo, cBlockNo, bPad[3], dwAudioTimeStamp[4], Data.
                let timestamp = u16le(body, 0).unwrap_or(0);
                let format_no = u16le(body, 2).unwrap_or(0);
                let block_no = body.get(4).copied().unwrap_or(0);
                let pcm = body.get(12..).unwrap_or(&[]);
                vec![self.play_wave(timestamp, block_no, format_no, pcm, sink)]
            }
            SNDC_CLOSE => Vec::new(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockSink {
        format: Option<(u16, u32, u16)>,
        played: Vec<u8>,
    }
    impl AudioSink for MockSink {
        fn set_format(&mut self, c: u16, s: u32, b: u16) {
            self.format = Some((c, s, b));
        }
        fn play(&mut self, pcm: &[u8]) {
            self.played.extend_from_slice(pcm);
        }
    }

    #[test]
    fn formats_pdu_is_answered_with_formats_and_quality() {
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        let out = snd.process(&message(SNDC_FORMATS, &[0u8; 20]), &mut sink);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0], SNDC_FORMATS);
        assert_eq!(out[1][0], SNDC_QUALITYMODE);
    }

    /// Build one `AUDIO_FORMAT` (18 bytes + extra).
    fn audio_format(tag: u16, ch: u16, sps: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
        let block = ch * (bits / 8);
        let mut v = Vec::new();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&sps.to_le_bytes());
        v.extend_from_slice(&(sps * block as u32).to_le_bytes()); // nAvgBytesPerSec
        v.extend_from_slice(&block.to_le_bytes()); // nBlockAlign
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(extra);
        v
    }

    /// Build a Server Audio Formats PDU body from a list of `AUDIO_FORMAT`s.
    fn server_formats_body(formats: &[Vec<u8>]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
        b.extend_from_slice(&0u32.to_le_bytes()); // dwVolume
        b.extend_from_slice(&0u32.to_le_bytes()); // dwPitch
        b.extend_from_slice(&0u16.to_le_bytes()); // wDGramPort
        b.extend_from_slice(&(formats.len() as u16).to_le_bytes()); // wNumberOfFormats
        b.push(0); // cLastBlockConfirmed
        b.extend_from_slice(&0x0008u16.to_le_bytes()); // wVersion
        b.push(0); // bPad
        for f in formats {
            b.extend_from_slice(f);
        }
        b
    }

    #[test]
    fn echoes_back_server_pcm_formats_and_dynamic_quality() {
        // Server offers PCM 48000/2/16, a non-PCM format (skipped, has extra
        // bytes to exercise cbSize advance), then PCM 22050/1/16.
        let body = server_formats_body(&[
            audio_format(WAVE_FORMAT_PCM, 2, 48_000, 16, &[]),
            audio_format(0x0102, 2, 44_100, 16, &[1, 2, 3, 4]),
            audio_format(WAVE_FORMAT_PCM, 1, 22_050, 16, &[]),
        ]);
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        let out = snd.process(&message(SNDC_FORMATS, &body), &mut sink);

        // We now advertise exactly the server's two PCM formats, in order.
        assert_eq!(snd.formats.len(), 2);
        assert_eq!(
            (snd.formats[0].samples_per_sec, snd.formats[0].channels),
            (48_000, 2)
        );
        assert_eq!(
            (snd.formats[1].samples_per_sec, snd.formats[1].channels),
            (22_050, 1)
        );
        // Reply = formats + DYNAMIC quality (0x0000), not HIGH.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0], SNDC_FORMATS);
        assert_eq!(out[1][0], SNDC_QUALITYMODE);
        assert_eq!(&out[1][4..6], &[0x00, 0x00]);

        // A WAVE2 with formatNo=0 must now resolve to the server's 48000/2/16.
        let mut w = Vec::new();
        w.extend_from_slice(&0u16.to_le_bytes()); // timestamp
        w.extend_from_slice(&0u16.to_le_bytes()); // formatNo 0
        w.push(1); // blockNo
        w.extend_from_slice(&[0, 0, 0]); // bPad
        w.extend_from_slice(&[0, 0, 0, 0]); // dwAudioTimeStamp
        w.extend_from_slice(&[9, 9, 9, 9]); // PCM
        let _ = snd.process(&message(SNDC_WAVE2, &w), &mut sink);
        assert_eq!(sink.format, Some((2, 48_000, 16)));
    }

    #[test]
    fn server_formats_without_pcm_keeps_default() {
        // If the server offers no usable PCM, we keep our default PCM list so the
        // handshake still completes.
        let body = server_formats_body(&[audio_format(0x0102, 2, 44_100, 16, &[9, 9])]);
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        let before = snd.formats.clone();
        let _ = snd.process(&message(SNDC_FORMATS, &body), &mut sink);
        assert_eq!(snd.formats, before);
    }

    #[test]
    fn advertises_pcm_formats() {
        // PCM-only: AAC/HE-AAC advertisement is disabled until a correct
        // HEAACWAVEINFO is emitted (a malformed entry made the server reject the
        // whole Client Audio Formats PDU and never stream waves — see
        // `client_formats`). Every advertised format must therefore be PCM.
        let snd = RdpsndChannel::new();
        let reply = snd.formats_reply();
        assert_eq!(reply[0], SNDC_FORMATS);
        // Number of formats is embedded in the reply body at offset 14.
        let body = &reply[4..];
        let count = u16::from_le_bytes([body[14], body[15]]);
        assert_eq!(count, 2, "expected the two PCM formats");
        for f in &snd.formats {
            assert_eq!(f.tag, WAVE_FORMAT_PCM);
        }
    }

    #[test]
    fn training_is_echoed() {
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        let body = [0x11, 0x22, 0x33, 0x44, 0xAA, 0xBB];
        let out = snd.process(&message(SNDC_TRAINING, &body), &mut sink);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], SNDC_TRAINING);
        assert_eq!(&out[0][4..8], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn wave2_plays_and_confirms() {
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        let mut body = Vec::new();
        body.extend_from_slice(&0x0102u16.to_le_bytes()); // timestamp
        body.extend_from_slice(&0u16.to_le_bytes()); // formatNo 0 → 44100/2/16
        body.push(7); // blockNo
        body.extend_from_slice(&[0, 0, 0]); // bPad
        body.extend_from_slice(&[0, 0, 0, 0]); // dwAudioTimeStamp
        body.extend_from_slice(&[1, 2, 3, 4, 5, 6]); // PCM
        let out = snd.process(&message(SNDC_WAVE2, &body), &mut sink);
        assert_eq!(sink.format, Some((2, 44_100, 16)));
        assert_eq!(sink.played, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], SNDC_WAVECONFIRM);
        assert_eq!(out[0][4], 0x02); // timestamp low
        assert_eq!(out[0][6], 7); // confirmed block
    }

    #[test]
    fn legacy_wave_reassembles_across_two_pdus() {
        let mut snd = RdpsndChannel::new();
        let mut sink = MockSink::default();
        // WaveInfo with the first 4 audio bytes.
        let mut info = Vec::new();
        info.extend_from_slice(&0x00FFu16.to_le_bytes()); // timestamp
        info.extend_from_slice(&1u16.to_le_bytes()); // formatNo 1 → 22050/2/16
        info.push(3); // blockNo
        info.extend_from_slice(&[0, 0, 0]); // bPad
        info.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // first 4 data bytes
        let out = snd.process(&message(SNDC_WAVE, &info), &mut sink);
        assert!(out.is_empty()); // no confirm yet
        // Headerless data body: 4 pad bytes + the rest.
        let data = [0, 0, 0, 0, 0x11, 0x22];
        let out = snd.process(&data, &mut sink);
        assert_eq!(sink.format, Some((2, 22_050, 16)));
        assert_eq!(sink.played, vec![0xDE, 0xAD, 0xBE, 0xEF, 0x11, 0x22]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], SNDC_WAVECONFIRM);
        assert_eq!(out[0][6], 3);
    }
}
