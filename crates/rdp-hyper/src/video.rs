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

use std::sync::{Arc, Mutex};

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

/// One frame's packet bodies, all one size, back to back in one buffer: what
/// [`packetize_shards`] and [`parity_shards`] fill instead of a `Vec` per
/// packet (60 KB at 10% parity was 59 allocations and frees a frame, a
/// keyframe ten times that). One from [`packetize_shards`] goes back to its
/// [`FrameBuffers`] when dropped.
#[derive(Default)]
pub struct Shards {
    buf: Vec<u8>,
    /// Each body's length.
    size: usize,
    home: Option<FrameBuffers>,
}

impl Shards {
    pub fn len(&self) -> usize {
        self.buf.len().checked_div(self.size).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Body `i`.
    pub fn get(&self, i: usize) -> &[u8] {
        &self.buf[i * self.size..(i + 1) * self.size]
    }

    pub fn iter(&self) -> std::slice::ChunksExact<'_, u8> {
        self.buf.chunks_exact(self.size.max(1))
    }
}

impl<'a> IntoIterator for &'a Shards {
    type Item = &'a [u8];
    type IntoIter = std::slice::ChunksExact<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Drop for Shards {
    fn drop(&mut self) {
        if let Some(home) = self.home.take() {
            home.give(std::mem::take(&mut self.buf));
        }
    }
}

/// [`packetize_data`] into one buffer from `buffers` (given back when the
/// [`Shards`] are dropped, e.g. on the sender thread): no allocation a packet,
/// and none a frame once the buffers have grown to the largest frame.
pub fn packetize_shards(frame: &OutFrame, fec: f32, buffers: &FrameBuffers) -> Shards {
    let len = frame.data.len().max(1);
    let shards_needed = len.div_ceil(MAX_SHARD);
    let shard_size = len.div_ceil(shards_needed);
    let blocks = shards_needed.div_ceil(MAX_BLOCK_DATA);
    assert!(blocks <= 255, "frame too large: {} bytes", frame.data.len());
    let size = SHARD_HEADER + shard_size;
    let mut buf = buffers.take(shards_needed * size);
    buf.clear();
    let flags = [(frame.keyframe, FLAG_KEYFRAME), (frame.ltr, FLAG_LTR), (frame.recovery, FLAG_RECOVERY)]
        .iter()
        .fold(0, |f, &(on, bit)| if on { f | bit } else { f });
    for b in 0..blocks {
        let first = b * MAX_BLOCK_DATA;
        let k = (shards_needed - first).min(MAX_BLOCK_DATA);
        let m = if fec <= 0.0 {
            0
        } else {
            ((k as f32 * fec).ceil() as usize).clamp(1, MAX_BLOCK_PARITY)
        };
        for i in 0..k {
            ShardHeader {
                frame_id: frame.frame_id,
                block: b as u8,
                blocks: blocks as u8,
                shard: i as u8,
                k: k as u8,
                m: m as u8,
                flags,
                frame_len: frame.data.len() as u32,
                capture_us: frame.capture_us,
                encode_us: frame.encode_us,
            }
            .write(&mut buf);
            let start = (first + i) * shard_size;
            if start < frame.data.len() {
                let end = (start + shard_size).min(frame.data.len());
                buf.extend_from_slice(&frame.data[start..end]);
            }
            buf.resize((first + i + 1) * size, 0);
        }
    }
    Shards { buf, size, home: Some(buffers.clone()) }
}

/// [`parity`] for [`packetize_shards`]' bodies, into `out` (reused frame to
/// frame). Still `m` allocations a block inside [`fec::encode`].
pub fn parity_shards(data: &Shards, out: &mut Shards) {
    out.buf.clear();
    out.size = data.size;
    let mut refs: [&[u8]; MAX_BLOCK_DATA] = [&[]; MAX_BLOCK_DATA];
    let (n, mut at) = (data.len(), 0);
    while let Some(head) = (at < n).then(|| ShardHeader::read(data.get(at))).flatten() {
        let k = (head.k as usize).clamp(1, (n - at).min(MAX_BLOCK_DATA));
        for (j, r) in refs.iter_mut().take(k).enumerate() {
            *r = &data.get(at + j)[SHARD_HEADER..];
        }
        at += k;
        let m = head.m as usize;
        if m == 0 {
            continue;
        }
        for (i, payload) in fec::encode(&refs[..k], m).into_iter().enumerate() {
            ShardHeader { shard: (k + i) as u8, ..head }.write(&mut out.buf);
            out.buf.extend_from_slice(&payload);
        }
    }
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
    /// Data shards in (bit `i`): written straight into the frame's buffer at
    /// their place, or held in `staged`.
    data: u64,
    /// Data shards' payloads until the block is done, only when staging
    /// ([`Reassembler::stage_shards`]).
    staged: Vec<Option<Vec<u8>>>,
    /// Parity, kept for a rebuild until the block is done.
    parity: Vec<Option<Vec<u8>>>,
    have: usize,
    done: bool,
}

struct Assembly {
    frame_id: u32,
    blocks: Vec<Option<Block>>,
    blocks_done: usize,
    shard_size: usize,
    frame_len: usize,
    flags: u8,
    capture_us: u64,
    encode_us: u32,
    first_rx_us: u64,
    /// The frame's data, `frame_len` bytes, filled in shard by shard.
    data: Vec<u8>,
}

impl Assembly {
    /// Where data shard `shard` of `block` goes in the frame: its range, cut
    /// at the frame's end (the last shard's padding isn't kept).
    fn span(&self, block: usize, shard: usize) -> std::ops::Range<usize> {
        let at = ((block * MAX_BLOCK_DATA + shard) * self.shard_size).min(self.frame_len);
        at..(at + self.shard_size).min(self.frame_len)
    }
}

/// Rebuilds frames from shard packet bodies, in any arrival order.
pub struct Reassembler {
    /// Frames being assembled, in slot `frame_id % MAX_IN_FLIGHT`: all of
    /// them lie in `base..base + MAX_IN_FLIGHT`, so no two share one (a map
    /// lookup a packet before, and a scan of every frame for the oldest).
    slots: Vec<Option<Assembly>>,
    /// Oldest frame that may still be in flight (`next`, once there is one).
    base: Option<u32>,
    /// Lowest frame id not yet delivered or declared lost.
    next: Option<u32>,
    stats: Stats,
    /// Shard buffers (parity, and data for a rebuild), reused for the next
    /// ones (not an allocation and a free per packet).
    pool: Vec<Vec<u8>>,
    /// A block's shards for [`fec::reconstruct`] (kept for its allocation).
    scratch: Vec<Option<Vec<u8>>>,
    buffers: FrameBuffers,
    stage: bool,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self {
            slots: (0..MAX_IN_FLIGHT).map(|_| None).collect(),
            base: None,
            next: None,
            stats: Stats::default(),
            pool: Vec::new(),
            scratch: Vec::new(),
            buffers: FrameBuffers::default(),
            stage: false,
        }
    }
}

/// Shard buffers kept for reuse: a big keyframe's worth.
const POOL_MAX: usize = 1024;

/// Frame buffers handed back for reuse ([`FrameBuffers::give`]).
const FRAME_BUFFERS_MAX: usize = 8;

/// Delivered frames' buffers, given back by whatever consumed them (the
/// decoder's thread) for the next frames: a fresh tens-of-KB allocation per
/// frame is, on Android's allocator, a new mapping and a page fault for each
/// of its pages. Cloned to wherever the frames end up. (The host's
/// [`packetize_shards`] keeps its packet buffers in one too.)
#[derive(Clone, Default)]
pub struct FrameBuffers(Arc<Mutex<Vec<Vec<u8>>>>);

impl FrameBuffers {
    /// A frame's data, done with. (Its length is kept: a frame written in
    /// place over it needn't zero it first.)
    pub fn give(&self, data: Vec<u8>) {
        let mut free = self.0.lock().unwrap();
        if free.len() < FRAME_BUFFERS_MAX {
            free.push(data);
        }
    }

    /// A buffer with room for `len` bytes (its contents and length are
    /// whatever it last had): the smallest given-back one that holds it
    /// (else the largest, grown), or a new one.
    fn take(&self, len: usize) -> Vec<u8> {
        let mut free = self.0.lock().unwrap();
        let fits = free.iter().enumerate().filter(|(_, b)| b.capacity() >= len).min_by_key(|(_, b)| b.capacity()).map(|(i, _)| i);
        let pick = fits.or_else(|| free.iter().enumerate().max_by_key(|(_, b)| b.capacity()).map(|(i, _)| i));
        let mut data = pick.map(|i| free.swap_remove(i)).unwrap_or_default();
        drop(free);
        data.reserve(len.saturating_sub(data.len()));
        data
    }
}

/// Frames kept in flight before the oldest are given up on.
const MAX_IN_FLIGHT: usize = 32;

/// A pooled copy of `bytes`, zero-padded to `len`.
fn pooled(pool: &mut Vec<Vec<u8>>, bytes: &[u8], len: usize) -> Vec<u8> {
    let mut v = pool.pop().unwrap_or_default();
    v.clear();
    v.extend_from_slice(bytes);
    v.resize(len, 0);
    v
}

fn recycle(pool: &mut Vec<Vec<u8>>, v: Vec<u8>) {
    if pool.len() < POOL_MAX {
        pool.push(v);
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// One whose frames come in buffers from (and can be given back to)
    /// `buffers`.
    pub fn with_buffers(buffers: FrameBuffers) -> Self {
        Self { buffers, ..Self::default() }
    }

    /// Hold each data shard in a buffer of its own and copy it into the frame
    /// once its block is whole (the old way, for comparison): two copies of
    /// every byte instead of one.
    pub fn stage_shards(mut self, on: bool) -> Self {
        self.stage = on;
        self
    }

    /// Where delivered frames' data can be given back for reuse (optional:
    /// without it, each frame gets a new buffer).
    pub fn buffers(&self) -> FrameBuffers {
        self.buffers.clone()
    }

    /// Counters since the last call.
    pub fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }

    /// Frames being assembled.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Feed one video packet body (already decrypted); `now_us` is the local
    /// clock. Returns any frames completed or given up on as a result.
    pub fn push(&mut self, body: &[u8], now_us: u64) -> Vec<Event> {
        let Some(h) = ShardHeader::read(body) else {
            return Vec::new();
        };
        let payload = &body[SHARD_HEADER..];
        let (k, m) = (h.k as usize, h.m as usize);
        if k == 0 || k > MAX_BLOCK_DATA || m > MAX_BLOCK_PARITY || h.shard as usize >= k + m || h.block >= h.blocks || payload.is_empty() {
            return Vec::new();
        }
        // Every authentic shard's bytes count as delivered, even parity
        // arriving after its frame completed: the host's rate controller
        // compares them with everything it sent. (Not in `shards`, the loss
        // estimate's denominator: late parity would dilute it.)
        self.stats.bytes += body.len() as u64;
        if self.base.is_some_and(|b| h.frame_id.wrapping_sub(b) > u32::MAX / 2) {
            return Vec::new(); // older than what we've already delivered
        }
        self.stats.shards += 1;
        let mut events = Vec::new();
        let slot = h.frame_id as usize % MAX_IN_FLIGHT;
        if self.slots[slot].as_ref().map_or(true, |a| a.frame_id != h.frame_id) {
            // Under sustained loss nothing completes, so nothing else would
            // ever trim the partial frames: give up on any a whole window
            // older than this one.
            let base = *self.base.get_or_insert(h.frame_id);
            if h.frame_id.wrapping_sub(base) >= MAX_IN_FLIGHT as u32 {
                let new_base = h.frame_id.wrapping_sub(MAX_IN_FLIGHT as u32 - 1);
                for s in &mut self.slots {
                    if s.as_ref().is_some_and(|a| a.frame_id.wrapping_sub(new_base) > u32::MAX / 2) {
                        let a = s.take().expect("checked");
                        self.buffers.give(Self::scrap(&mut self.pool, a));
                    }
                }
                let next = self.next.unwrap_or(base);
                events.push(Event::Lost { first: next, last: new_base.wrapping_sub(1) });
                self.stats.frames_lost += new_base.wrapping_sub(next);
                (self.next, self.base) = (Some(new_base), Some(new_base));
            }
            let blocks = h.blocks as usize;
            if h.frame_len as usize > blocks * MAX_BLOCK_DATA * payload.len() {
                return events; // more than its shards could carry
            }
            let mut data = self.buffers.take(h.frame_len as usize);
            data.resize(h.frame_len as usize, 0);
            self.slots[slot] = Some(Assembly {
                frame_id: h.frame_id,
                blocks: (0..blocks).map(|_| None).collect(),
                blocks_done: 0,
                shard_size: payload.len(),
                frame_len: h.frame_len as usize,
                flags: h.flags,
                capture_us: h.capture_us,
                encode_us: h.encode_us,
                first_rx_us: now_us,
                data,
            });
        }
        let a = self.slots[slot].as_mut().expect("in flight");
        if payload.len() != a.shard_size || a.blocks.len() != h.blocks as usize {
            return events; // inconsistent with this frame's other shards
        }
        let stage = self.stage;
        let span = a.span(h.block as usize, h.shard as usize);
        let block = a.blocks[h.block as usize].get_or_insert_with(|| Block {
            k,
            data: 0,
            staged: if stage { vec![None; k] } else { Vec::new() },
            parity: vec![None; m],
            have: 0,
            done: false,
        });
        if block.done || block.k != k || block.parity.len() != m {
            return events;
        }
        let i = h.shard as usize;
        if i < k {
            if block.data & 1 << i != 0 {
                return events;
            }
            block.data |= 1 << i;
            if stage {
                block.staged[i] = Some(pooled(&mut self.pool, payload, payload.len()));
            } else {
                a.data[span.clone()].copy_from_slice(&payload[..span.len()]);
            }
        } else {
            if block.parity[i - k].is_some() {
                return events;
            }
            block.parity[i - k] = Some(pooled(&mut self.pool, payload, payload.len()));
        }
        block.have += 1;
        if block.have < block.k {
            return events;
        }
        let missing = k - block.data.count_ones() as usize;
        if (missing > 0 || stage) && !Self::rebuild(a, h.block as usize, &mut self.pool, &mut self.scratch, stage) {
            return events;
        }
        self.stats.recovered += missing as u32;
        let block = a.blocks[h.block as usize].as_mut().expect("block");
        block.done = true;
        for p in block.parity.drain(..).flatten() {
            recycle(&mut self.pool, p);
        }
        a.blocks_done += 1;
        if a.blocks_done < a.blocks.len() {
            return events;
        }
        events.extend(self.complete(h.frame_id));
        events
    }

    /// Block `b` of `a` from any `k` of its shards: the data shards missing
    /// (all of them, when staging) rebuilt and written into the frame.
    /// Data shards already in the frame are copied out for
    /// [`fec::reconstruct`], which wants each in a buffer of its own.
    fn rebuild(a: &mut Assembly, b: usize, pool: &mut Vec<Vec<u8>>, scratch: &mut Vec<Option<Vec<u8>>>, stage: bool) -> bool {
        let size = a.shard_size;
        let k = a.blocks[b].as_ref().map_or(0, |bl| bl.k);
        let spans: [_; MAX_BLOCK_DATA] = std::array::from_fn(|i| if i < k { a.span(b, i) } else { 0..0 });
        let block = a.blocks[b].as_mut().expect("block");
        scratch.clear();
        for (i, span) in spans.iter().take(k).enumerate() {
            scratch.push(match stage {
                true => block.staged[i].take(),
                false => (block.data & 1 << i != 0).then(|| pooled(pool, &a.data[span.clone()], size)),
            });
        }
        scratch.extend(block.parity.iter_mut().map(Option::take));
        let ok = fec::reconstruct(scratch, k);
        for (i, s) in scratch.drain(..).enumerate() {
            let Some(s) = s else { continue };
            if !ok && i >= k {
                block.parity[i - k] = Some(s); // (kept for another try)
            } else if !ok && stage {
                block.staged[i] = Some(s);
            } else {
                if ok && i < k && (stage || block.data & 1 << i == 0) {
                    a.data[spans[i].clone()].copy_from_slice(&s[..spans[i].len()]);
                }
                recycle(pool, s);
            }
        }
        ok
    }

    /// A frame given up on (or delivered): its shard buffers back in the
    /// pool, and its data for the caller.
    fn scrap(pool: &mut Vec<Vec<u8>>, a: Assembly) -> Vec<u8> {
        for block in a.blocks.into_iter().flatten() {
            for s in block.staged.into_iter().chain(block.parity).flatten() {
                recycle(pool, s);
            }
        }
        a.data
    }

    fn complete(&mut self, frame_id: u32) -> Vec<Event> {
        let mut events = Vec::new();
        let next = self.next.unwrap_or(frame_id);
        if frame_id != next {
            events.push(Event::Lost { first: next, last: frame_id.wrapping_sub(1) });
            self.stats.frames_lost += frame_id.wrapping_sub(next);
        }
        let a = self.slots[frame_id as usize % MAX_IN_FLIGHT].take().expect("assembly");
        // Drop everything older: all in the window below it.
        let base = self.base.unwrap_or(frame_id);
        for id in (0..frame_id.wrapping_sub(base).min(MAX_IN_FLIGHT as u32)).map(|i| base.wrapping_add(i)) {
            let s = &mut self.slots[id as usize % MAX_IN_FLIGHT];
            if s.as_ref().is_some_and(|o| o.frame_id == id) {
                let o = s.take().expect("checked");
                self.buffers.give(Self::scrap(&mut self.pool, o));
            }
        }
        (self.next, self.base) = (Some(frame_id.wrapping_add(1)), Some(frame_id.wrapping_add(1)));
        let (flags, capture_us, encode_us, first_rx_us) = (a.flags, a.capture_us, a.encode_us, a.first_rx_us);
        let data = Self::scrap(&mut self.pool, a);
        self.stats.frames += 1;
        events.push(Event::Frame(InFrame {
            frame_id,
            keyframe: flags & FLAG_KEYFRAME != 0,
            ltr: flags & FLAG_LTR != 0,
            recovery: flags & FLAG_RECOVERY != 0,
            data,
            capture_us,
            encode_us,
            first_rx_us,
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

    /// Sustained loss (no FEC, every frame missing a shard): 32 frames always
    /// in flight, and each new one gives up on the oldest.
    /// `cargo test -p rdp-hyper --release in_flight_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn in_flight_speed() {
        let packets: Vec<Vec<Vec<u8>>> = (0..20_000u32).map(|id| send(id, &frame(id, 5000), 0.0)[1..].to_vec()).collect();
        let shards: usize = packets.iter().map(|p| p.len()).sum();
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut r = Reassembler::new();
            let t = std::time::Instant::now();
            let mut lost = 0;
            for p in packets.iter().flatten() {
                lost += r.push(p, 0).len();
            }
            assert!(lost > 19_000);
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("reassembly (sustained loss): {:.1} ns a shard, {:.1} us a frame", best * 1e9 / shards as f64, best * 1e6 / 20_000.0);
    }

    /// Throughput of the receive path: 2000 frames of 60 KB, 10% parity, with
    /// no loss and with every 20th or every 10th shard lost (rebuilt from
    /// parity). Each also staged (`stage_shards`, the old way: every byte
    /// copied twice). "Hot": 8 frames' packets reused (their ids rewritten),
    /// in cache like packets just received and decrypted; otherwise 130 MB of
    /// packets come from RAM, and that dominates.
    /// `cargo test -p rdp-hyper --release reassembly_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn reassembly_speed() {
        let data = frame(7, 60_000);
        let packets: Vec<Vec<Vec<u8>>> = (0..2000u32).map(|id| send(id, &data, 0.1)).collect();
        let shards: usize = packets.iter().map(|p| p.len()).sum();
        let mut hot_set: Vec<Vec<Vec<u8>>> = packets[..8].to_vec();
        let cases = [
            (0, false, false),
            (0, true, false),
            (20, false, false),
            (20, true, false),
            (10, false, false),
            (10, true, false),
            (0, true, true),
            (20, true, true),
            (10, true, true),
        ];
        for (stage, (every, give, hot)) in [false, true].into_iter().flat_map(|s| cases.map(|c| (s, c))) {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let mut r = Reassembler::new().stage_shards(stage);
                let buffers = r.buffers();
                let t = std::time::Instant::now();
                let mut n = 0;
                for id in 0..packets.len() {
                    let ps = match hot {
                        true => {
                            let set = &mut hot_set[id % 8];
                            set.iter_mut().for_each(|p| p[..4].copy_from_slice(&(id as u32).to_le_bytes()));
                            &set[..]
                        }
                        false => &packets[id][..],
                    };
                    for (i, p) in ps.iter().enumerate() {
                        if every > 0 && (i + id) % every == 3 {
                            continue;
                        }
                        for f in frames(r.push(p, 5)) {
                            // (Touched, as a decoder's copy would.)
                            n += 1 + (f.data[f.data.len() / 2] == 0x5a) as usize * 0;
                            if give {
                                buffers.give(f.data);
                            }
                        }
                    }
                }
                assert_eq!(n, 2000);
                best = best.min(t.elapsed().as_secs_f64());
            }
            eprintln!(
                "reassembly ({}{}{}{}): {:.1} ns a shard, {:.1} us a frame",
                match every {
                    0 => "no loss",
                    20 => "5% lost",
                    _ => "10% lost",
                },
                if give { ", buffers given back" } else { "" },
                if hot { ", hot" } else { "" },
                if stage { ", staged" } else { "" },
                best * 1e9 / shards as f64,
                best * 1e6 / 2000.0
            );
        }
        let p = &packets[0];
        eprintln!("({} shards a frame)", p.len());
    }

    /// The host's send path per frame: data shards, then parity (10%, and
    /// none: the part that isn't Reed-Solomon), for a 60 KB frame and a 600 KB
    /// keyframe, a `Vec` a packet and in one reused buffer.
    /// `cargo test -p rdp-hyper --release packetize_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn packetize_speed() {
        for len in [60_000usize, 600_000] {
            let data = frame(9, len);
            let rounds = 3_000_000 / len * 20;
            let buffers = FrameBuffers::default();
            let mut parity_out = Shards::default();
            for (fec, shards) in [(0.1, false), (0.1, true), (0.0, false), (0.0, true)] {
                let mut best = f64::MAX;
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    let mut n = 0;
                    for id in 0..rounds as u32 {
                        let out = OutFrame { frame_id: id, data: &data, keyframe: false, ltr: false, recovery: false, capture_us: 1, encode_us: 2 };
                        if shards {
                            let d = packetize_shards(&out, fec, &buffers);
                            parity_shards(&d, &mut parity_out);
                            n += d.len() + parity_out.len();
                        } else {
                            let d = packetize_data(&out, fec);
                            let p = parity(&d);
                            n += d.len() + p.len();
                        }
                    }
                    assert!(n > 0);
                    best = best.min(t.elapsed().as_secs_f64());
                }
                eprintln!(
                    "packetize {} KB, {}% parity{}: {:.1} us a frame",
                    len / 1000,
                    (fec * 100.0) as u32,
                    if shards { ", one buffer" } else { ", a Vec a packet" },
                    best * 1e6 / rounds as f64
                );
            }
        }
    }

    #[test]
    fn given_back_buffers_are_reused() {
        let mut r = Reassembler::new();
        let buffers = r.buffers();
        let mut last_ptr = None;
        for (id, len) in [(0u32, 50_000usize), (1, 20_000), (2, 49_000), (3, 70_000)] {
            let data = frame(id, len);
            let got: Vec<InFrame> = send(id, &data, 0.1).iter().flat_map(|p| frames(r.push(p, 5))).collect();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].data, data, "frame {id}");
            if id == 1 || id == 2 {
                assert_eq!(Some(got[0].data.as_ptr()), last_ptr, "frame {id} reuses the given-back buffer");
            }
            last_ptr = Some(got[0].data.as_ptr());
            buffers.give(got.into_iter().next().unwrap().data);
        }
    }

    #[test]
    fn shards_match_the_vec_a_packet_bodies() {
        let buffers = FrameBuffers::default();
        let mut p = Shards::default();
        for (len, fec) in [(1, 0.1), (900, 0.0), (60_000, 0.1), (250_000, 0.2)] {
            let data = frame(5, len);
            let out = OutFrame { frame_id: 5, data: &data, keyframe: true, ltr: false, recovery: true, capture_us: 9, encode_us: 4 };
            let d = packetize_shards(&out, fec, &buffers);
            parity_shards(&d, &mut p);
            let all: Vec<&[u8]> = d.iter().chain(&p).collect();
            assert_eq!(all, packetize(&out, fec).iter().map(Vec::as_slice).collect::<Vec<_>>(), "len={len}");
        }
    }

    #[test]
    fn staged_shards_rebuild_the_same_frames() {
        for stage in [false, true] {
            let mut r = Reassembler::new().stage_shards(stage);
            for id in 0..4u32 {
                let data = frame(id, 100_000);
                let got: Vec<InFrame> = send(id, &data, 0.2)
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| i % 7 != id as usize)
                    .flat_map(|(_, p)| frames(r.push(p, 0)))
                    .collect();
                assert_eq!(got.len(), 1, "stage={stage} id={id}");
                assert_eq!(got[0].data, data, "stage={stage} id={id}");
            }
            assert!(r.take_stats().recovered > 0);
        }
    }

    #[test]
    fn frames_interleaved_across_the_window_complete() {
        // Two frames a window apart can't share a slot: the older is given up.
        let mut r = Reassembler::new();
        let f0 = send(0, &frame(0, 5000), 0.0);
        let f32 = send(32, &frame(32, 5000), 0.0);
        let f33 = send(33, &frame(33, 5000), 0.0);
        assert!(r.push(&f0[0], 0).is_empty());
        assert_eq!(r.push(&f32[0], 0), vec![Event::Lost { first: 0, last: 0 }]);
        assert!(r.push(&f0[1], 0).is_empty(), "given up on");
        // Interleaved frames inside the window both complete, in any order.
        let mixed = f33.iter().zip(&f32[1..]).flat_map(|(a, b)| [a, b]).chain(&f33[f32.len() - 1..]);
        let got: Vec<u32> = mixed.flat_map(|p| frames(r.push(p, 0))).map(|f| f.frame_id).collect();
        assert_eq!(got, vec![32, 33]);
        // Wrap-around: ids past u32::MAX keep working.
        let mut r = Reassembler::new();
        for id in (u32::MAX - 40..=u32::MAX).chain(0..40) {
            let got = frames(send(id, &frame(id, 3000), 0.0).iter().flat_map(|p| r.push(p, 0)).collect());
            assert_eq!(got.len(), 1, "id={id}");
        }
        assert_eq!(r.take_stats().frames_lost, 0);
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
            assert!(r.in_flight() <= MAX_IN_FLIGHT);
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
