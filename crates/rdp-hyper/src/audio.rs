//! Opus audio over lossy UDP (both directions: game sound to the client, the
//! client's mic to the host).
//!
//! Each packet carries the newest 10 ms Opus frame plus the one before it, so
//! a single lost packet costs nothing (its frame rides the next packet). Longer
//! gaps are filled by the decoder's packet-loss concealment, briefly, then
//! silence — never by waiting.

/// Samples per channel in one 10 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;

/// One audio packet body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// Sequence number of the newest frame (the last in `frames`).
    pub seq: u32,
    pub channels: u8,
    /// Sender clock (µs) when the newest frame was captured.
    pub capture_us: u64,
    /// Consecutive Opus frames, oldest first.
    pub frames: Vec<Vec<u8>>,
}

impl AudioPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(16 + self.frames.iter().map(|f| f.len() + 2).sum::<usize>());
        o.extend_from_slice(&self.seq.to_le_bytes());
        o.push(self.channels);
        o.extend_from_slice(&self.capture_us.to_le_bytes());
        o.push(self.frames.len() as u8);
        for f in &self.frames {
            o.extend_from_slice(&(f.len() as u16).to_le_bytes());
            o.extend_from_slice(f);
        }
        o
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let seq = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?);
        let channels = *b.get(4)?;
        let capture_us = u64::from_le_bytes(b.get(5..13)?.try_into().ok()?);
        let count = *b.get(13)? as usize;
        let mut pos = 14;
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u16::from_le_bytes(b.get(pos..pos + 2)?.try_into().ok()?) as usize;
            pos += 2;
            frames.push(b.get(pos..pos + len)?.to_vec());
            pos += len;
        }
        (count > 0).then_some(Self { seq, channels, capture_us, frames })
    }
}

/// What the receiver should decode next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decode {
    Frame(Vec<u8>),
    /// A frame that never arrived: run packet-loss concealment.
    Conceal,
    /// The sender started a new stream (its numbering restarted): reset the
    /// decoder before the frames that follow.
    Restart,
}

/// A packet this far behind what was already played is a restarted sender,
/// not a late one (reordering is a frame or two at most).
const RESTART_BEHIND: i32 = 16;

/// Concealed frames played at most before giving up and waiting for the
/// stream (beyond ~30 ms concealment sounds worse than a gap).
const MAX_CONCEAL: u32 = 3;

/// Orders frames, uses the redundant copies, and conceals short gaps.
#[derive(Default)]
pub struct Sequencer {
    next: Option<u32>,
    /// Capture time of the newest packet played: a restarted sender's
    /// numbering starts over but its clock doesn't, which tells a restart
    /// from a very late packet.
    newest_capture_us: u64,
}

impl Sequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames to decode, in order, for one received packet.
    pub fn push(&mut self, p: &AudioPacket) -> Vec<Decode> {
        let n = p.frames.len() as u32;
        let first = p.seq.wrapping_sub(n - 1);
        let mut out = Vec::new();
        let mut next = self.next.unwrap_or(first);
        let ahead = p.seq.wrapping_sub(next) as i32;
        if ahead < -RESTART_BEHIND && p.capture_us > self.newest_capture_us {
            // The sender restarted (the mic was reopened): a new stream.
            out.push(Decode::Restart);
            next = first;
        } else if ahead < 0 {
            return out; // everything in this packet already played (or late)
        }
        // Frames between `next` and what this packet carries are lost.
        let gap = first.wrapping_sub(next) as i32;
        if gap > 0 {
            for _ in 0..(gap as u32).min(MAX_CONCEAL) {
                out.push(Decode::Conceal);
            }
        }
        for (i, f) in p.frames.iter().enumerate() {
            let seq = first.wrapping_add(i as u32);
            if seq.wrapping_sub(next) as i32 >= 0 {
                out.push(Decode::Frame(f.clone()));
            }
        }
        self.next = Some(p.seq.wrapping_add(1));
        self.newest_capture_us = self.newest_capture_us.max(p.capture_us);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u32) -> AudioPacket {
        let frames = if seq == 0 { vec![vec![0]] } else { vec![vec![(seq - 1) as u8], vec![seq as u8]] };
        AudioPacket { seq, channels: 2, capture_us: seq as u64 * 10_000, frames }
    }

    fn frames(d: Vec<Decode>) -> Vec<Option<u8>> {
        d.into_iter()
            .filter_map(|d| match d {
                Decode::Frame(f) => Some(Some(f[0])),
                Decode::Conceal => Some(None),
                Decode::Restart => None,
            })
            .collect()
    }

    #[test]
    fn packets_round_trip() {
        let p = packet(9);
        assert_eq!(AudioPacket::parse(&p.to_bytes()), Some(p));
        assert_eq!(AudioPacket::parse(&[1, 2, 3]), None);
    }

    #[test]
    fn in_order_packets_play_each_frame_once() {
        let mut s = Sequencer::new();
        assert_eq!(frames(s.push(&packet(1))), vec![Some(0), Some(1)]);
        assert_eq!(frames(s.push(&packet(2))), vec![Some(2)]);
        assert_eq!(frames(s.push(&packet(3))), vec![Some(3)]);
    }

    #[test]
    fn one_lost_packet_is_covered_by_redundancy() {
        let mut s = Sequencer::new();
        s.push(&packet(1));
        // Packet 2 lost; packet 3 carries frames 2 and 3.
        assert_eq!(frames(s.push(&packet(3))), vec![Some(2), Some(3)]);
    }

    #[test]
    fn longer_gaps_are_concealed_briefly_and_late_packets_dropped() {
        let mut s = Sequencer::new();
        s.push(&packet(1));
        // 2..=9 lost → frames 2..=8 missing (9 arrives inside packet 10).
        assert_eq!(frames(s.push(&packet(10))), vec![None, None, None, Some(9), Some(10)]);
        assert!(s.push(&packet(5)).is_empty());
    }

    #[test]
    fn a_restarted_sender_is_heard_again() {
        let mut s = Sequencer::new();
        for seq in 1..=5000 {
            s.push(&packet(seq));
        }
        // The sender restarts its numbering at 1 (its clock carries on).
        let restarted = |seq: u32| AudioPacket { capture_us: (5000 + seq) as u64 * 10_000, ..packet(seq) };
        let d = s.push(&restarted(1));
        assert_eq!(d.first(), Some(&Decode::Restart));
        assert_eq!(frames(d), vec![Some(0), Some(1)]);
        assert_eq!(frames(s.push(&restarted(2))), vec![Some(2)]);
    }

    #[test]
    fn a_very_late_packet_is_not_a_restart() {
        let mut s = Sequencer::new();
        for seq in 1..=100 {
            s.push(&packet(seq));
        }
        assert!(s.push(&packet(80)).is_empty(), "late: dropped");
        assert_eq!(frames(s.push(&packet(101))), vec![Some(101)]);
    }
}
