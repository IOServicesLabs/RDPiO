//! Audio input redirection (MS-RDPEAI) — stream the client's microphone to the
//! remote session. Runs as a dynamic virtual channel named `AUDIO_INPUT`.
//!
//! Flow (server → client unless noted): the server sends a **Version** PDU and
//! the client echoes its version; the server sends its supported **Formats** and
//! the client replies with the subset it can capture (the server's 16-bit PCM
//! entries); the server sends **Open** picking one of those, and the client
//! confirms it with a **Format Change**, replies **Open Reply** (`S_OK`) and
//! begins capturing. Captured PCM is pushed to the server as a **Data Incoming**
//! marker followed by a **Data** PDU. A **Format Change** can re-select a format.
//!
//! Sans-I/O and OS-agnostic: [`AudioInputChannel`] turns inbound PDUs into the
//! responses to send and tracks the negotiated capture format + open state; the
//! platform layer supplies microphone PCM via [`AudioInputChannel::data_pdus`]
//! and frames everything as DRDYNVC data. If the server never opens the channel
//! (or the negotiation is declined), the session simply runs without a mic.

/// MS-RDPEAI message ids (first byte of every PDU).
const MSG_VERSION: u8 = 0x01;
const MSG_FORMATS: u8 = 0x02;
const MSG_OPEN: u8 = 0x03;
const MSG_OPEN_REPLY: u8 = 0x04;
const MSG_DATA_INCOMING: u8 = 0x05;
const MSG_DATA: u8 = 0x06;
const MSG_FORMAT_CHANGE: u8 = 0x07;

const WAVE_FORMAT_PCM: u16 = 0x0001;
/// The protocol version we advertise (MS-RDPEAI uses 1).
const SNDIN_VERSION: u32 = 0x0000_0001;

/// A PCM capture format. Mirrors a `WAVEFORMATEX` (cbSize = 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub channels: u16,
    pub samples_per_sec: u32,
    pub bits_per_sample: u16,
}

impl Format {
    fn block_align(&self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }
    fn avg_bytes_per_sec(&self) -> u32 {
        self.samples_per_sec * self.block_align() as u32
    }
    /// Serialize as an 18-byte `WAVEFORMATEX` (PCM, cbSize = 0).
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.samples_per_sec.to_le_bytes());
        out.extend_from_slice(&self.avg_bytes_per_sec().to_le_bytes());
        out.extend_from_slice(&self.block_align().to_le_bytes());
        out.extend_from_slice(&self.bits_per_sample.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // cbSize
    }
}

/// The capture formats we offer the server (plain PCM, universally supported).
/// Mono 44.1 kHz/16-bit first — the natural microphone format.
fn client_formats() -> Vec<Format> {
    vec![
        Format {
            channels: 1,
            samples_per_sec: 44_100,
            bits_per_sample: 16,
        },
        Format {
            channels: 1,
            samples_per_sec: 16_000,
            bits_per_sample: 16,
        },
    ]
}

/// The 16-bit PCM mono/stereo entries of a server Sound Formats PDU body
/// (NumFormats(4), cbSizeFormatsPacket(4), then `AUDIO_FORMAT`s — a
/// `WAVEFORMATEX` plus `cbSize` extra bytes each), in the server's order.
fn server_pcm_formats(body: &[u8]) -> Vec<Format> {
    let Some(num) = body.get(0..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) else {
        return Vec::new();
    };
    let mut rest = body.get(8..).unwrap_or(&[]);
    let mut out = Vec::new();
    for _ in 0..num {
        if rest.len() < 18 {
            break;
        }
        let tag = u16::from_le_bytes([rest[0], rest[1]]);
        let channels = u16::from_le_bytes([rest[2], rest[3]]);
        let samples_per_sec = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
        let bits_per_sample = u16::from_le_bytes([rest[14], rest[15]]);
        let cb_size = u16::from_le_bytes([rest[16], rest[17]]) as usize;
        if tag == WAVE_FORMAT_PCM
            && bits_per_sample == 16
            && (1..=2).contains(&channels)
            && samples_per_sec > 0
        {
            out.push(Format {
                channels,
                samples_per_sec,
                bits_per_sample,
            });
        }
        rest = rest.get(18 + cb_size..).unwrap_or(&[]);
    }
    out
}

/// Build one MS-RDPEAI PDU: message id + body.
fn message(id: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + body.len());
    v.push(id);
    v.extend_from_slice(body);
    v
}

/// The audio-input (microphone) channel state machine.
pub struct AudioInputChannel {
    formats: Vec<Format>,
    /// Set once the server has opened capture; carries the chosen format.
    open_format: Option<Format>,
}

impl Default for AudioInputChannel {
    fn default() -> Self {
        Self {
            formats: client_formats(),
            open_format: None,
        }
    }
}

impl AudioInputChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// The negotiated capture format once the channel is open, for configuring
    /// the platform's audio-capture device. `None` until the server sends Open.
    pub fn capture_format(&self) -> Option<Format> {
        self.open_format
    }

    /// Whether the server has opened the microphone channel (capture should run).
    pub fn is_open(&self) -> bool {
        self.open_format.is_some()
    }

    /// Client Version PDU echoing our protocol version.
    fn version_reply() -> Vec<u8> {
        message(MSG_VERSION, &SNDIN_VERSION.to_le_bytes())
    }

    /// Client Sound Formats PDU advertising the formats we can capture.
    fn formats_reply(&self) -> Vec<u8> {
        let mut formats_blob = Vec::new();
        for f in &self.formats {
            f.write(&mut formats_blob);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&(self.formats.len() as u32).to_le_bytes()); // NumFormats
        body.extend_from_slice(&(formats_blob.len() as u32).to_le_bytes()); // cbSizeFormatsPacket
        body.extend_from_slice(&formats_blob);
        message(MSG_FORMATS, &body)
    }

    /// Client Open Reply PDU (`Result` = `S_OK`).
    fn open_reply_ok() -> Vec<u8> {
        message(MSG_OPEN_REPLY, &0u32.to_le_bytes()) // HRESULT S_OK
    }

    /// Client Format Change PDU confirming the capture format (an index into
    /// the formats we advertised).
    fn format_change(index: u32) -> Vec<u8> {
        message(MSG_FORMAT_CHANGE, &index.to_le_bytes())
    }

    /// Process one complete inbound PDU, returning the responses to send (each
    /// already an MS-RDPEAI PDU; the caller frames them as DRDYNVC data).
    pub fn process(&mut self, pdu: &[u8]) -> Vec<Vec<u8>> {
        let Some((&id, body)) = pdu.split_first() else {
            return Vec::new();
        };
        match id {
            MSG_VERSION => vec![Self::version_reply()],
            MSG_FORMATS => {
                // Reply with the formats both sides support — the server's own
                // 16-bit PCM entries, in its order (`waveIn`'s mapper captures
                // any PCM rate/channel count) — so Open's `initialFormat` picks
                // a format the server offered. Our defaults only if it offered
                // no PCM at all.
                let offered = server_pcm_formats(body);
                self.formats = if offered.is_empty() {
                    client_formats()
                } else {
                    offered
                };
                vec![self.formats_reply()]
            }
            MSG_OPEN => {
                // Open: FramesPerPacket(4), initialFormat(4), WAVEFORMATEX...
                // `initialFormat` indexes the formats we advertised; fall back to
                // the first format if the index is out of range.
                let initial = body
                    .get(4..8)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                    .unwrap_or(0) as usize;
                let index = if initial < self.formats.len() { initial } else { 0 };
                self.open_format = self.formats.get(index).copied();
                // Confirm the format, then accept (FreeRDP's order, which Windows
                // hosts expect); capture starts once the platform reads
                // capture_format().
                vec![Self::format_change(index as u32), Self::open_reply_ok()]
            }
            MSG_FORMAT_CHANGE => {
                // NewFormat(4): re-select the capture format by index and
                // confirm it back.
                if let Some(idx) = body
                    .get(0..4)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                {
                    if let Some(f) = self.formats.get(idx as usize).copied() {
                        self.open_format = Some(f);
                        return vec![Self::format_change(idx)];
                    }
                }
                Vec::new()
            }
            // Data flows client→server only; the server shouldn't send these.
            MSG_DATA_INCOMING | MSG_DATA | MSG_OPEN_REPLY => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Build the PDUs that carry one buffer of captured microphone `pcm` to the
    /// server: a Data Incoming marker followed by the Data PDU. Empty if the
    /// channel isn't open (nothing should be captured yet).
    pub fn data_pdus(&self, pcm: &[u8]) -> Vec<Vec<u8>> {
        if !self.is_open() || pcm.is_empty() {
            return Vec::new();
        }
        vec![message(MSG_DATA_INCOMING, &[]), message(MSG_DATA, pcm)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_echoed() {
        let mut ch = AudioInputChannel::new();
        let out = ch.process(&message(MSG_VERSION, &1u32.to_le_bytes()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], MSG_VERSION);
        assert_eq!(&out[0][1..5], &SNDIN_VERSION.to_le_bytes());
    }

    #[test]
    fn formats_reply_lists_pcm_formats() {
        let mut ch = AudioInputChannel::new();
        // A minimal server Formats PDU (contents don't affect our reply).
        let out = ch.process(&message(MSG_FORMATS, &[0u8; 8]));
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r[0], MSG_FORMATS);
        let num = u32::from_le_bytes([r[1], r[2], r[3], r[4]]);
        assert_eq!(num, 2);
        // cbSizeFormatsPacket = 2 formats * 18 bytes.
        let cb = u32::from_le_bytes([r[5], r[6], r[7], r[8]]);
        assert_eq!(cb, 36);
        // First format: PCM, mono, 44100, 16-bit.
        assert_eq!(u16::from_le_bytes([r[9], r[10]]), WAVE_FORMAT_PCM);
        assert_eq!(u16::from_le_bytes([r[11], r[12]]), 1); // channels
        assert_eq!(u32::from_le_bytes([r[13], r[14], r[15], r[16]]), 44_100);
    }

    #[test]
    fn open_selects_format_and_replies_ok() {
        let mut ch = AudioInputChannel::new();
        assert!(!ch.is_open());
        let mut body = Vec::new();
        body.extend_from_slice(&960u32.to_le_bytes()); // FramesPerPacket
        body.extend_from_slice(&1u32.to_le_bytes()); // initialFormat = index 1
        // (a WAVEFORMATEX would follow; we key off the index)
        let out = ch.process(&message(MSG_OPEN, &body));
        // Format Change confirming index 1, then Open Reply S_OK.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0], MSG_FORMAT_CHANGE);
        assert_eq!(&out[0][1..5], &1u32.to_le_bytes());
        assert_eq!(out[1][0], MSG_OPEN_REPLY);
        assert_eq!(&out[1][1..5], &0u32.to_le_bytes()); // S_OK
        assert!(ch.is_open());
        assert_eq!(
            ch.capture_format(),
            Some(Format {
                channels: 1,
                samples_per_sec: 16_000,
                bits_per_sample: 16,
            })
        );
    }

    #[test]
    fn data_pdus_only_after_open() {
        let mut ch = AudioInputChannel::new();
        assert!(ch.data_pdus(&[1, 2, 3, 4]).is_empty()); // not open yet
        let mut body = Vec::new();
        body.extend_from_slice(&960u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        ch.process(&message(MSG_OPEN, &body));
        let pdus = ch.data_pdus(&[1, 2, 3, 4]);
        assert_eq!(pdus.len(), 2);
        assert_eq!(pdus[0][0], MSG_DATA_INCOMING);
        assert_eq!(pdus[1][0], MSG_DATA);
        assert_eq!(&pdus[1][1..], &[1, 2, 3, 4]);
    }

    #[test]
    fn format_change_reselects() {
        let mut ch = AudioInputChannel::new();
        let mut open = Vec::new();
        open.extend_from_slice(&960u32.to_le_bytes());
        open.extend_from_slice(&0u32.to_le_bytes());
        ch.process(&message(MSG_OPEN, &open));
        assert_eq!(ch.capture_format().unwrap().samples_per_sec, 44_100);
        let out = ch.process(&message(MSG_FORMAT_CHANGE, &1u32.to_le_bytes()));
        assert_eq!(ch.capture_format().unwrap().samples_per_sec, 16_000);
        // The change is confirmed back to the server.
        assert_eq!(out, vec![message(MSG_FORMAT_CHANGE, &1u32.to_le_bytes())]);
    }

    /// One server `AUDIO_FORMAT`: a WAVEFORMATEX with `extra` trailing bytes.
    fn server_format(tag: u16, channels: u16, rate: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
        let block = channels * (bits / 8).max(1);
        let mut v = Vec::new();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * block as u32).to_le_bytes());
        v.extend_from_slice(&block.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn formats_reply_is_the_servers_pcm_subset() {
        // Server offers a compressed format (with extra bytes), stereo 48 kHz
        // PCM, 8-bit PCM and mono 22.05 kHz PCM. We keep the two 16-bit PCM
        // entries in the server's order, so Open's index lands on one of them.
        let entries = [
            server_format(0x0002, 2, 44_100, 4, &[0xAA; 32]), // ADPCM
            server_format(WAVE_FORMAT_PCM, 2, 48_000, 16, &[]),
            server_format(WAVE_FORMAT_PCM, 1, 8_000, 8, &[]),
            server_format(WAVE_FORMAT_PCM, 1, 22_050, 16, &[]),
        ];
        let blob: Vec<u8> = entries.concat();
        let mut body = Vec::new();
        body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        body.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        body.extend_from_slice(&blob);

        let mut ch = AudioInputChannel::new();
        let out = ch.process(&message(MSG_FORMATS, &body));
        let r = &out[0];
        assert_eq!(u32::from_le_bytes([r[1], r[2], r[3], r[4]]), 2);

        let mut open = Vec::new();
        open.extend_from_slice(&960u32.to_le_bytes());
        open.extend_from_slice(&1u32.to_le_bytes());
        ch.process(&message(MSG_OPEN, &open));
        assert_eq!(
            ch.capture_format(),
            Some(Format {
                channels: 1,
                samples_per_sec: 22_050,
                bits_per_sample: 16,
            })
        );
    }
}
