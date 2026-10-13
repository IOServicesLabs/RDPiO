//! Control messages: the JSON handshake carried over the RDP side channel, and
//! the small binary messages that ride the UDP session.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// Protocol version both sides must agree on.
pub const PROTOCOL: u32 = 3;

/// The RDP dynamic virtual channel the handshake runs over.
pub const CHANNEL_NAME: &str = "RDPIO.HYPER";

/// Handshake messages over the RDP channel (encrypted by RDP's TLS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Handshake {
    /// Client → host, as soon as the channel opens.
    ClientHello {
        protocol: u32,
        /// Codecs the client decodes in hardware, best first ("h264", "hevc").
        codecs: Vec<String>,
        /// The client window's size and the local display's refresh rate.
        width: u32,
        height: u32,
        refresh_hz: u32,
        /// How many monitors the client has.
        #[serde(default = "one")]
        monitors: u32,
    },
    /// Host → client: where to send UDP and the session key.
    HostOffer {
        protocol: u32,
        port: u16,
        /// 32-byte AES-256-GCM key, base64.
        key: String,
        width: u32,
        height: u32,
        fps: u32,
        codec: String,
    },
    /// Host → client: Hyper isn't available; stay on plain RDP.
    HostDecline { reason: String },
    /// Client → host: the UDP path works; take the session off RDP now.
    ClientReady,
}

fn one() -> u32 {
    1
}

impl Handshake {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("serialize handshake")
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

pub fn encode_key(key: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(key)
}

pub fn decode_key(s: &str) -> Option<[u8; 32]> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()?.try_into().ok()
}

/// Receive-side report the client sends every ~50 ms; drives the host's
/// bitrate and FEC.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Feedback {
    /// Shards received / rebuilt from parity / missing from finished blocks.
    pub shards: u32,
    pub recovered: u32,
    pub frames: u32,
    pub frames_lost: u32,
    pub bytes: u64,
    /// `client receive clock - host capture clock` for the window's frames
    /// (µs): the raw offset includes the clocks' difference, but its rise over
    /// the window's minimum is queueing delay.
    pub delay_min_us: i64,
    pub delay_avg_us: i64,
    /// How long the window these counts cover lasted, on the client (µs) —
    /// the host's spacing between arrivals is skewed by uplink jitter.
    pub window_us: u32,
    /// The newest long-term reference frame the client decoded (see
    /// [`crate::video::OutFrame::ltr`]).
    pub ltr_frame: Option<u32>,
}

/// Binary messages inside [`crate::wire::Kind::Control`] packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Client → host round-trip probe (also the first packet: it tells the
    /// host the client's address).
    Ping { id: u32, client_us: u64 },
    /// Host → client reply.
    Pong { id: u32, client_us: u64, host_us: u64 },
    /// Client → host: decoding broke; send a keyframe now.
    KeyframeRequest,
    /// Client → host: frames were lost, but what it decoded before is
    /// intact: a frame coded from a long-term reference it has will do
    /// (smaller than a keyframe); the host sends a keyframe if it has none.
    Recover,
    Feedback(Feedback),
    /// Host → client: input applied up to here, and the current stream setup.
    Status { input_ack: u32, width: u16, height: u16, fps: u16, bitrate_kbps: u32, fec_permille: u16 },
    /// Either side is leaving.
    Bye,
    /// Host → client: an app on the host is (or stopped) listening to the
    /// microphone, so the client should (or need no longer) capture it.
    Mic { on: bool },
    /// Client → host: the window is minimized (nothing to show: encode
    /// nothing until it's back), or visible again. Repeated while paused; a
    /// keyframe request also ends a pause.
    Paused { paused: bool },
    /// Client → host: office mode (crisp text: 60 fps, frames as big as the
    /// text needs, the best-quality preset) or game mode (the lowest
    /// latency). Repeated every second. `fast_text`: office mode with the
    /// encoder's fast preset (still the big frame budget) — ~6 ms less
    /// latency for slightly softer new text.
    Mode { office: bool, fast_text: bool },
    /// Host → client: the host's console moved to another session (a
    /// sign-in, a user switch); sign in again now — the agent there serves it.
    Reconnect,
}

impl Control {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(48);
        match self {
            Self::Ping { id, client_us } => {
                o.push(1);
                o.extend_from_slice(&id.to_le_bytes());
                o.extend_from_slice(&client_us.to_le_bytes());
            }
            Self::Pong { id, client_us, host_us } => {
                o.push(2);
                o.extend_from_slice(&id.to_le_bytes());
                o.extend_from_slice(&client_us.to_le_bytes());
                o.extend_from_slice(&host_us.to_le_bytes());
            }
            Self::KeyframeRequest => o.push(3),
            Self::Feedback(f) => {
                o.push(4);
                for v in [f.shards, f.recovered, f.frames, f.frames_lost] {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.extend_from_slice(&f.bytes.to_le_bytes());
                o.extend_from_slice(&f.delay_min_us.to_le_bytes());
                o.extend_from_slice(&f.delay_avg_us.to_le_bytes());
                o.extend_from_slice(&f.window_us.to_le_bytes());
                o.extend_from_slice(&f.ltr_frame.unwrap_or(u32::MAX).to_le_bytes());
            }
            Self::Status { input_ack, width, height, fps, bitrate_kbps, fec_permille } => {
                o.push(5);
                o.extend_from_slice(&input_ack.to_le_bytes());
                for v in [width, height, fps] {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.extend_from_slice(&bitrate_kbps.to_le_bytes());
                o.extend_from_slice(&fec_permille.to_le_bytes());
            }
            Self::Bye => o.push(6),
            Self::Mic { on } => {
                o.push(7);
                o.push(*on as u8);
            }
            Self::Paused { paused } => {
                o.push(8);
                o.push(*paused as u8);
            }
            Self::Mode { office, fast_text } => {
                o.push(9);
                o.push(*office as u8);
                o.push(*fast_text as u8);
            }
            Self::Reconnect => o.push(10),
            Self::Recover => o.push(11),
        }
        o
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        let u32_at = |i: usize| b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
        let u64_at = |i: usize| b.get(i..i + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
        Some(match *b.first()? {
            1 => Self::Ping { id: u32_at(1)?, client_us: u64_at(5)? },
            2 => Self::Pong { id: u32_at(1)?, client_us: u64_at(5)?, host_us: u64_at(13)? },
            3 => Self::KeyframeRequest,
            4 => Self::Feedback(Feedback {
                shards: u32_at(1)?,
                recovered: u32_at(5)?,
                frames: u32_at(9)?,
                frames_lost: u32_at(13)?,
                bytes: u64_at(17)?,
                delay_min_us: u64_at(25)? as i64,
                delay_avg_us: u64_at(33)? as i64,
                window_us: u32_at(41).unwrap_or(0),
                ltr_frame: u32_at(45).filter(|&v| v != u32::MAX),
            }),
            5 => Self::Status {
                input_ack: u32_at(1)?,
                width: u16_at(5)?,
                height: u16_at(7)?,
                fps: u16_at(9)?,
                bitrate_kbps: u32_at(11)?,
                fec_permille: u16_at(15)?,
            },
            6 => Self::Bye,
            7 => Self::Mic { on: *b.get(1)? != 0 },
            8 => Self::Paused { paused: *b.get(1)? != 0 },
            9 => Self::Mode { office: *b.get(1)? != 0, fast_text: b.get(2).is_some_and(|&v| v != 0) },
            10 => Self::Reconnect,
            11 => Self::Recover,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trips_as_json() {
        let key = crate::wire::random_key();
        let m = Handshake::HostOffer {
            protocol: PROTOCOL,
            port: 47998,
            key: encode_key(&key),
            width: 2560,
            height: 1440,
            fps: 120,
            codec: "h264".into(),
        };
        let back = Handshake::parse(&m.to_bytes()).unwrap();
        assert_eq!(back, m);
        let Handshake::HostOffer { key: k, .. } = back else { unreachable!() };
        assert_eq!(decode_key(&k), Some(key));
        assert_eq!(Handshake::parse(br#"{"type":"client_ready"}"#), Some(Handshake::ClientReady));
        assert_eq!(Handshake::parse(b"nonsense"), None);
    }

    #[test]
    fn control_messages_round_trip() {
        let all = [
            Control::Ping { id: 9, client_us: 123 },
            Control::Pong { id: 9, client_us: 123, host_us: 456 },
            Control::KeyframeRequest,
            Control::Feedback(Feedback {
                shards: 1,
                recovered: 2,
                frames: 3,
                frames_lost: 4,
                bytes: 5,
                delay_min_us: -6,
                delay_avg_us: 7,
                window_us: 50_000,
                ltr_frame: Some(77),
            }),
            Control::Feedback(Feedback { frames: 3, ..Default::default() }),
            Control::Recover,
            Control::Status { input_ack: 1, width: 2560, height: 1440, fps: 120, bitrate_kbps: 30_000, fec_permille: 100 },
            Control::Bye,
            Control::Mic { on: true },
            Control::Paused { paused: true },
            Control::Mode { office: true, fast_text: false },
            Control::Mode { office: true, fast_text: true },
            Control::Mode { office: false, fast_text: false },
            Control::Reconnect,
        ];
        for c in all {
            assert_eq!(Control::parse(&c.to_bytes()), Some(c.clone()), "{c:?}");
        }
        assert_eq!(Control::parse(&[4, 1, 2]), None);
    }
}
