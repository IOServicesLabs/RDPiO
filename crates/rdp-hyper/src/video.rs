//! Encoded video frames ↔ FEC-protected packet bodies.
//!
//! A frame is cut into equal-size shards (as large as a datagram allows, but no
//! larger than needed, so a small frame isn't padded out to a full packet),
//! grouped into blocks of at most [`MAX_BLOCK_DATA`] data shards, and each
//! block gets parity shards (see [`crate::fec`]). Each shard rides one packet
//! with a small header so the receiver can rebuild the frame from any `k` of a
//! block's `k + m` shards.
//!
//! Latency over completeness: the receiver never waits for a late frame. When
//! a newer frame completes while an older one is still missing pieces, the
//! older one is declared lost (the decoder then needs a keyframe).

use std::collections::BTreeMap;

use crate::fec;
use crate::wire::MAX_BODY;

/// Bytes of shard header in front of every shard's payload.
pub const SHARD_HEADER: usize = 26;
/// Largest shard payload.
pub const MAX_SHARD: usize = MAX_BODY - SHARD_HEADER;
/// Most data shards per FEC block (bounds the decode matrix and the work per
/// lost packet).
pub const MAX_BLOCK_DATA: usize = 64;
/// Most parity shards per block.
pub const MAX_BLOCK_PARITY: usize = 64;

const FLAG_KEYFRAME: u8 = 1;
const FLAG_LTR: u8 = 2;
const FLAG_RECOVERY: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShardHeader {
    frame_id: u32,
    block: u8,
    blocks: u8,
    shard: u8,
    k: u8,
    m: u8,
    flags: u8,
    frame_len: u32,
    capture_us: u64,
    encode_us: u32,
}

impl ShardHeader {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&[self.block, self.blocks, self.shard, self.k, self.m, self.flags]);
        out.extend_from_slice(&self.frame_len.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.encode_us.to_le_bytes());
    }

    fn read(b: &[u8]) -> Option<Self> {
        if b.len() < SHARD_HEADER {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Some(Self {
            frame_id: u32_at(0),
            block: b[4],
            blocks: b[5],
            shard: b[6],
            k: b[7],
            m: b[8],
            flags: b[9],
            frame_len: u32_at(10),
            capture_us: u64::from_le_bytes(b[14..22].try_into().unwrap()),
            encode_us: u32_at(22),
        })
    }
}

/// One encoded frame handed to [`packetize`].
pub struct OutFrame<'a> {
    pub frame_id: u32,
    pub data: &'a [u8],
    pub keyframe: bool,
    /// The encoder keeps this frame as a long-term reference: a client that
    /// decodes it says so, and a loss later can be repaired from it.
    pub ltr: bool,
    /// Coded from long-term references the client said it has, and nothing
    /// else: decodable after lost frames, like a keyframe.
    pub recovery: bool,
    /// Host clock (µs) when the frame was captured.
    pub capture_us: u64,
    /// How long capture → encoded bytes took on the host (µs).
    pub encode_us: u32,
}

/// Cut a frame into packet bodies: all data shards, then all parity.
/// `fec` is the parity ratio (0.1 = 10% extra packets); any value above 0
/// gives every block at least one parity shard.
pub fn packetize(frame: &OutFrame, fec: f32) -> Vec<Vec<u8>> {
    let mut out = packetize_data(frame, fec);
    let parity = parity(&out);
    out.extend(parity);
    out
}

/// The data shards' packet bodies only (their headers already announce the
/// parity to come). Send these first and compute [`parity`] while they're on
/// their way: a receiver normally completes a frame from data alone.
pub fn packetize_data(frame: &OutFrame, fec: f32) -> Vec<Vec<u8>> {
    let len = frame.data.len().max(1);
    let shards_needed = len.div_ceil(MAX_SHARD);
    let shard_size = len.div_ceil(shards_needed);
    let blocks = shards_needed.div_ceil(MAX_BLOCK_DATA);
    assert!(blocks <= 255, "frame too large: {} bytes", frame.data.len());
    let mut out = Vec::with_capacity(shards_needed);
    for b in 0..blocks {
        let first = b * MAX_BLOCK_DATA;
        let k = (shards_needed - first).min(MAX_BLOCK_DATA);
        let m = if fec <= 0.0 {
            0
        } else {
            ((k as f32 * fec).ceil() as usize).clamp(1, MAX_BLOCK_PARITY)
        };
        for i in 0..k {
            let mut body = Vec::with_capacity(SHARD_HEADER + shard_size);
            ShardHeader {
                frame_id: frame.frame_id,
                block: b as u8,
                blocks: blocks as u8,
                shard: i as u8,
                k: k as u8,
                m: m as u8,
                flags: [(frame.keyframe, FLAG_KEYFRAME), (frame.ltr, FLAG_LTR), (frame.recovery, FLAG_RECOVERY)]
                    .iter()
                    .fold(0, |f, &(on, bit)| if on { f | bit } else { f }),
                frame_len: frame.data.len() as u32,
                capture_us: frame.capture_us,
                encode_us: frame.encode_us,
            }
            .write(&mut body);
            let start = (first + i) * shard_size;
            if start < frame.data.len() {
                let end = (start + shard_size).min(frame.data.len());
                body.extend_from_slice(&frame.data[start..end]);
            }
            body.resize(SHARD_HEADER + shard_size, 0);
            out.push(body);
        }
    }
    out
}

/// The parity bodies for data bodies from [`packetize_data`] (one frame's,
/// in order).
pub fn parity(data: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = data;
    while let Some(head) = rest.first().and_then(|b| ShardHeader::read(b)) {
        let k = (head.k as usize).clamp(1, rest.len());
        let (block, tail) = rest.split_at(k);
        rest = tail;
        let m = head.m as usize;
        if m == 0 {
            continue;
        }
        let refs: Vec<&[u8]> = block.iter().map(|b| &b[SHARD_HEADER..]).collect();
        for (i, payload) in fec::encode(&refs, m).into_iter().enumerate() {
            let mut body = Vec::with_capacity(SHARD_HEADER + payload.len());
            ShardHeader { shard: (k + i) as u8, ..head }.write(&mut body);
            body.extend_from_slice(&payload);
            out.push(body);
        }
    }
    out
}

/// A frame rebuilt by the [`Reassembler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    /// See [`OutFrame::ltr`].
    pub ltr: bool,
    /// See [`OutFrame::recovery`].
    pub recovery: bool,
    pub data: Vec<u8>,
    pub capture_us: u64,
    pub encode_us: u32,
    /// Receiver clock (µs) when this frame's first shard arrived.
    pub first_rx_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Frame(InFrame),
    /// Frames `first..=last` will never complete.
    Lost { first: u32, last: u32 },
}

/// Receive-side counters, drained into the client's feedback reports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub shards: u32,
    /// Data shards rebuilt from parity.
    pub recovered: u32,
    pub frames: u32,
    pub frames_lost: u32,
    pub bytes: u64,
}

struct Block {
    k: usize,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    done: bool,
}

struct Assembly {
    blocks: Vec<Option<Block>>,
    blocks_done: usize,
    shard_size: usize,
    frame_len: usize,
    flags: u8,
    capture_us: u64,
    encode_us: u32,
    first_rx_us: u64,
}

/// Rebuilds frames from shard packet bodies, in any arrival order.
#[derive(Default)]
pub struct Reassembler {
    frames: BTreeMap<u32, Assembly>,
    /// Lowest frame id not yet delivered or declared lost.
    next: Option<u32>,
    stats: Stats,
}

/// Frames kept in flight before the oldest are given up on.
const MAX_IN_FLIGHT: usize = 32;

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counters since the last call.
    pub fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }

    /// Feed one video packet body (already decrypted); `now_us` is the local
    /// clock. Returns any frames completed or given up on as a result.
    pub fn push(&mut self, body: &[u8], now_us: u64) -> Vec<Event> {
        let Some(h) = ShardHeader::read(body) else {
            return Vec::new();
        };
        let payload = &body[SHARD_HEADER..];
        if h.k == 0 || h.shard as usize >= h.k as usize + h.m as usize || h.block >= h.blocks || payload.is_empty() {
            return Vec::new();
        }
        // Every authentic shard's bytes count as delivered, even parity
        // arriving after its frame completed: the host's rate controller
        // compares them with everything it sent. (Not in `shards`, the loss
        // estimate's denominator: late parity would dilute it.)
        self.stats.bytes += body.len() as u64;
        let older = |next: Option<u32>| next.is_some_and(|n| h.frame_id.wrapping_sub(n) > u32::MAX / 2);
        if older(self.next) {
            return Vec::new(); // older than what we've already delivered
        }
        self.stats.shards += 1;
        let mut events = Vec::new();
        if !self.frames.contains_key(&h.frame_id) {
            // Under sustained loss nothing completes, so nothing else would
            // ever trim the partial frames: give up on the oldest ones here.
            while self.frames.len() >= MAX_IN_FLIGHT {
                let oldest = self.oldest().expect("non-empty");
                self.frames.remove(&oldest);
                let next = self.next.unwrap_or(oldest);
                if oldest.wrapping_sub(next) < u32::MAX / 2 {
                    events.push(Event::Lost { first: next, last: oldest });
                    self.stats.frames_lost += oldest.wrapping_sub(next) + 1;
                    self.next = Some(oldest.wrapping_add(1));
                }
            }
            // Giving up on the oldest may have passed this frame too.
            if older(self.next) {
                return events;
            }
        }
        let a = self.frames.entry(h.frame_id).or_insert_with(|| Assembly {
            blocks: (0..h.blocks).map(|_| None).collect(),
            blocks_done: 0,
            shard_size: payload.len(),
            frame_len: h.frame_len as usize,
            flags: h.flags,
            capture_us: h.capture_us,
            encode_us: h.encode_us,
            first_rx_us: now_us,
        });
        if payload.len() != a.shard_size || a.blocks.len() != h.blocks as usize {
            return events; // inconsistent with this frame's other shards
        }
        let block = a.blocks[h.block as usize].get_or_insert_with(|| Block {
            k: h.k as usize,
            shards: vec![None; h.k as usize + h.m as usize],
            have: 0,
            done: false,
        });
        if block.done || block.k != h.k as usize || block.shards.len() != h.k as usize + h.m as usize {
            return events;
        }
        let slot = &mut block.shards[h.shard as usize];
        if slot.is_some() {
            return events;
        }
        *slot = Some(payload.to_vec());
        block.have += 1;
        if block.have < block.k {
            return events;
        }
        let missing = block.shards[..block.k].iter().filter(|s| s.is_none()).count();
        if !fec::reconstruct(&mut block.shards, block.k) {
            return events;
        }
        self.stats.recovered += missing as u32;
        block.done = true;
        a.blocks_done += 1;
        if a.blocks_done < a.blocks.len() {
            return events;
        }
        events.extend(self.complete(h.frame_id));
        events
    }

    /// The oldest frame still being assembled (wrap-aware).
    fn oldest(&self) -> Option<u32> {
        let any = *self.frames.keys().next()?;
        Some(self.frames.keys().copied().fold(any, |o, id| if id.wrapping_sub(o) > u32::MAX / 2 { id } else { o }))
    }

    fn complete(&mut self, frame_id: u32) -> Vec<Event> {
        let mut events = Vec::new();
        let next = self.next.unwrap_or(frame_id);
        if frame_id != next {
            events.push(Event::Lost { first: next, last: frame_id.wrapping_sub(1) });
            self.stats.frames_lost += frame_id.wrapping_sub(next);
        }
        // Drop everything up to and including this frame.
        let a = self.frames.remove(&frame_id).expect("assembly");
        self.frames.retain(|&id, _| id.wrapping_sub(frame_id) < u32::MAX / 2 && id != frame_id);
        self.next = Some(frame_id.wrapping_add(1));
        let mut data = Vec::with_capacity(a.frame_len);
        for block in a.blocks.into_iter().flatten() {
            for s in block.shards.into_iter().take(block.k).flatten() {
                data.extend_from_slice(&s);
            }
        }
        data.truncate(a.frame_len);
        self.stats.frames += 1;
        events.push(Event::Frame(InFrame {
            frame_id,
            keyframe: a.flags & FLAG_KEYFRAME != 0,
            ltr: a.flags & FLAG_LTR != 0,
            recovery: a.flags & FLAG_RECOVERY != 0,
            data,
            capture_us: a.capture_us,
            encode_us: a.encode_us,
            first_rx_us: a.first_rx_us,
        }));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u32 ^ id.wrapping_mul(2654435761)) as u8).collect()
    }

    fn send(id: u32, data: &[u8], fec: f32) -> Vec<Vec<u8>> {
        packetize(
            &OutFrame { frame_id: id, data, keyframe: id == 0, ltr: id % 4 == 1, recovery: id % 5 == 2, capture_us: 1000 + id as u64, encode_us: 3 },
            fec,
        )
    }

    fn frames(events: Vec<Event>) -> Vec<InFrame> {
        events
            .into_iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn frame_flags_travel() {
        let mut r = Reassembler::new();
        for id in 0..6u32 {
            let got = send(id, &frame(id, 900), 0.1).iter().flat_map(|p| frames(r.push(p, 5))).collect::<Vec<_>>();
            assert_eq!(got.len(), 1);
            assert_eq!((got[0].keyframe, got[0].ltr, got[0].recovery), (id == 0, id % 4 == 1, id % 5 == 2), "frame {id}");
        }
    }

    #[test]
    fn small_frames_use_small_packets() {
        let p = send(1, &frame(1, 300), 0.0);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].len(), SHARD_HEADER + 300);
        // Two shards share a just-big-enough size instead of padding to MAX.
        let p = send(1, &frame(1, MAX_SHARD + 10), 0.0);
        assert_eq!(p.len(), 2);
        assert!(p[0].len() < MAX_BODY);
    }

    #[test]
    fn frames_round_trip_in_any_order() {
        for len in [1, 500, MAX_SHARD, 70_000, 250_000] {
            let data = frame(7, len);
            let mut packets = send(7, &data, 0.1);
            packets.reverse();
            let mut r = Reassembler::new();
            let got: Vec<InFrame> = packets.iter().flat_map(|p| frames(r.push(p, 5))).collect();
            assert_eq!(got.len(), 1, "len={len}");
            assert_eq!(got[0].data, data, "len={len}");
            assert_eq!(got[0].capture_us, 1007);
        }
    }

    #[test]
    fn lost_packets_are_rebuilt_from_parity() {
        let data = frame(3, 100_000);
        let packets = send(3, &data, 0.2);
        let mut r = Reassembler::new();
        // Drop every 7th packet: within each block's parity budget.
        let got: Vec<InFrame> = packets
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 7 != 3)
            .flat_map(|(_, p)| frames(r.push(p, 0)))
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, data);
        assert!(r.take_stats().recovered > 0);
    }

    #[test]
    fn a_frame_overtaken_by_a_newer_one_is_reported_lost() {
        let mut r = Reassembler::new();
        let f1 = send(1, &frame(1, 5000), 0.0);
        let f2 = send(2, &frame(2, 5000), 0.0);
        let f3 = send(3, &frame(3, 5000), 0.0);
        assert!(frames(f1.iter().flat_map(|p| r.push(p, 0)).collect()).len() == 1);
        // Frame 2 loses a packet (no FEC); frame 3 arrives whole.
        for p in &f2[1..] {
            assert!(r.push(p, 0).is_empty());
        }
        let ev: Vec<Event> = f3.iter().flat_map(|p| r.push(p, 0)).collect();
        assert_eq!(ev[0], Event::Lost { first: 2, last: 2 });
        assert!(matches!(&ev[1], Event::Frame(f) if f.frame_id == 3));
        // Frame 2's straggler arriving now is ignored.
        assert!(r.push(&f2[0], 0).is_empty());
        assert_eq!(r.take_stats().frames_lost, 1);
    }

    #[test]
    fn sustained_loss_cannot_grow_memory_without_bound() {
        let mut r = Reassembler::new();
        let mut lost = 0;
        // 1000 frames, each missing a packet (no FEC): none ever completes.
        for id in 0..1000u32 {
            let p = send(id, &frame(id, 5000), 0.0);
            for ev in r.push(&p[1], 0) {
                if let Event::Lost { first, last } = ev {
                    lost += last - first + 1;
                }
            }
            assert!(r.frames.len() <= MAX_IN_FLIGHT);
        }
        assert!(lost >= 1000 - MAX_IN_FLIGHT as u32, "lost {lost}");
        // A frame that does arrive whole afterwards is still delivered.
        let whole = send(1000, &frame(1000, 5000), 0.0);
        let got: Vec<Event> = whole.iter().flat_map(|p| r.push(p, 0)).collect();
        assert!(got.iter().any(|e| matches!(e, Event::Frame(f) if f.frame_id == 1000)));
    }

    #[test]
    fn garbage_is_ignored() {
        let mut r = Reassembler::new();
        assert!(r.push(&[1, 2, 3], 0).is_empty());
        let mut p = send(1, &frame(1, 10), 0.0).remove(0);
        p[7] = 0; // k = 0
        assert!(r.push(&p, 0).is_empty());
    }
}
