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
const FLAG_PROBE: u8 = 8;

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
    /// Sent unpaced, for the client to time ([`Stats::link_kbps`]).
    pub probe: bool,
}

/// A probe goes out as a train of at least this many full-size packets: its
/// data shards, then parity enough to make up the rest. The client times
/// the whole train, first packet to last (see [`Stats::link_kbps`]). A few
/// small packets' spread is a Wi-Fi radio's channel access and the client's
/// own wake-up, not the link: 2 KB in 8 packets read an AX210's 300 Mbit/s
/// uplink as 1-6. 28 KB a second is 0.23 Mbit/s, and the padding is parity
/// (it repairs the frame, too).
pub const PROBE_TRAIN_PACKETS: usize = 48;

/// Probes go as trains (the default); off (the host's `probetrain=0`, for
/// A/B against it): as before, the frame alone in [`BURST_MIN_SHARDS`] data
/// shards at least, smaller ones if need be.
static PROBE_TRAINS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// See [`PROBE_TRAINS`].
pub fn set_probe_trains(on: bool) {
    PROBE_TRAINS.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn trains() -> bool {
    PROBE_TRAINS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many data shards a frame takes, and their size. A probe's are full
/// size (the train's packets are, see [`PROBE_TRAIN_PACKETS`]). (Any shard
/// size decodes: the receiver takes a frame's from its first shard.)
fn layout(frame: &OutFrame) -> (usize, usize) {
    layout_of(frame.data.len(), frame.probe)
}

pub(crate) fn layout_of(len: usize, probe: bool) -> (usize, usize) {
    let len = len.max(1);
    if probe && trains() {
        return (len.div_ceil(MAX_SHARD), MAX_SHARD);
    }
    let min = if probe { BURST_MIN_SHARDS as usize } else { 1 };
    let size = len.div_ceil(len.div_ceil(MAX_SHARD).max(min));
    (len.div_ceil(size), size)
}

/// Parity shards for block `b` of `k` data shards (of `shards` in all):
/// `fec` of them, and on a probe's last block as many more as make the
/// train [`PROBE_TRAIN_PACKETS`] long.
pub(crate) fn parity_count(k: usize, b: usize, shards: usize, fec: f32, probe: bool) -> usize {
    let m = if fec <= 0.0 { 0 } else { ((k as f32 * fec).ceil() as usize).clamp(1, MAX_BLOCK_PARITY) };
    let last = b + 1 == shards.div_ceil(MAX_BLOCK_DATA);
    let fill = if probe && last && trains() { PROBE_TRAIN_PACKETS.saturating_sub(shards + m) } else { 0 };
    (m + fill).min(MAX_BLOCK_PARITY)
}

/// Packet body bytes a frame of `len` takes, parity included.
fn body_bytes(len: usize, fec: f32, probe: bool) -> usize {
    let (shards, size) = layout_of(len, probe);
    let blocks = shards.div_ceil(MAX_BLOCK_DATA);
    (0..blocks)
        .map(|b| {
            let k = (shards - b * MAX_BLOCK_DATA).min(MAX_BLOCK_DATA);
            (k + parity_count(k, b, shards, fec, probe)) * (SHARD_HEADER + size)
        })
        .sum()
}

/// The body bytes a probe of `len` takes beyond the same frame sent as
/// usual: its train's padding (see
/// [`crate::rate::RateController::note_probe_padding`]).
pub fn probe_padding(len: usize, fec: f32) -> usize {
    if !trains() {
        return 0;
    }
    body_bytes(len, fec, true).saturating_sub(body_bytes(len, fec, false))
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
    let (shards_needed, shard_size) = layout(frame);
    let blocks = shards_needed.div_ceil(MAX_BLOCK_DATA);
    assert!(blocks <= 255, "frame too large: {} bytes", frame.data.len());
    let mut out = Vec::with_capacity(shards_needed);
    for b in 0..blocks {
        let first = b * MAX_BLOCK_DATA;
        let k = (shards_needed - first).min(MAX_BLOCK_DATA);
        let m = parity_count(k, b, shards_needed, fec, frame.probe);
        for i in 0..k {
            let mut body = Vec::with_capacity(SHARD_HEADER + shard_size);
            ShardHeader {
                frame_id: frame.frame_id,
                block: b as u8,
                blocks: blocks as u8,
                shard: i as u8,
                k: k as u8,
                m: m as u8,
                flags: [(frame.keyframe, FLAG_KEYFRAME), (frame.ltr, FLAG_LTR), (frame.recovery, FLAG_RECOVERY), (frame.probe, FLAG_PROBE)]
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

/// The order to send one frame's bodies in (its data, or its parity, laid
/// out block after block as [`packetize_shards`] and [`parity_shards`] lay
/// them): a body of each block in turn (`interleave`, see
/// [`crate::rate::Switches::interleave`]). Packets lost back to back — a
/// Wi-Fi aggregate's retries running out, a queue overflowing — then cost
/// each block a few, which its parity rebuilds, instead of one block all of
/// them: simulated with 1% lost in runs of 8 and a keyframe (300 KB, five
/// blocks) every 5 s, 71 of 461 keyframes were lost block after block, 23
/// in turns (see `fecsim`). A frame of one block goes as it is. The
/// receiver takes shards in any order.
pub fn send_order(bodies: &Shards, interleave: bool) -> SendOrder {
    order_of((0..bodies.len()).map(|i| bodies.get(i)[4]), interleave)
}

/// [`send_order`] for bodies of these blocks, in their order.
pub(crate) fn order_of(blocks: impl ExactSizeIterator<Item = u8> + Clone, interleave: bool) -> SendOrder {
    let n = blocks.len();
    let mut runs = Vec::new();
    if interleave && blocks.clone().next() != blocks.clone().last() {
        let mut prev = None;
        for (i, b) in blocks.enumerate() {
            if prev != Some(b) {
                runs.push((i, i));
                prev = Some(b);
            }
            runs.last_mut().expect("pushed").1 = i + 1;
        }
    }
    SendOrder { runs, plain: 0..n, turn: 0 }
}

/// See [`send_order`]: body indices, a block's in turn with the others'.
pub struct SendOrder {
    /// Each block's bodies yet to go (next, end); empty: in order.
    runs: Vec<(usize, usize)>,
    plain: std::ops::Range<usize>,
    turn: usize,
}

impl Iterator for SendOrder {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.runs.is_empty() {
            return self.plain.next();
        }
        let n = self.runs.len();
        for _ in 0..n {
            let r = &mut self.runs[self.turn];
            self.turn = (self.turn + 1) % n;
            if r.0 < r.1 {
                r.0 += 1;
                return Some(r.0 - 1);
            }
        }
        None
    }
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
    let (shards_needed, shard_size) = layout(frame);
    let blocks = shards_needed.div_ceil(MAX_BLOCK_DATA);
    assert!(blocks <= 255, "frame too large: {} bytes", frame.data.len());
    let size = SHARD_HEADER + shard_size;
    let mut buf = buffers.take(shards_needed * size);
    buf.clear();
    let flags = [(frame.keyframe, FLAG_KEYFRAME), (frame.ltr, FLAG_LTR), (frame.recovery, FLAG_RECOVERY), (frame.probe, FLAG_PROBE)]
        .iter()
        .fold(0, |f, &(on, bit)| if on { f | bit } else { f });
    for b in 0..blocks {
        let first = b * MAX_BLOCK_DATA;
        let k = (shards_needed - first).min(MAX_BLOCK_DATA);
        let m = parity_count(k, b, shards_needed, fec, frame.probe);
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
    /// How fast a probe train (a frame the host sent unpaced, see
    /// [`OutFrame::probe`] and [`PROBE_TRAIN_PACKETS`]) came in, its first
    /// packet to its last, parity after the frame completed included
    /// (kbit/s; the fastest, if several; 0: none): what the link carries —
    /// the host put it all on the wire at once, and a slower link spreads
    /// it. (Not a paced frame: that times the pacer.)
    pub link_kbps: u32,
}

/// Whether frames arrived on time, for timing the link by: one held up on
/// the way (a Wi-Fi radio's stall, a queue) comes out with the frames behind
/// it in a burst, microseconds apart — gigabits a second, on a link that
/// carries 20 megabits. Only a frame within [`PROMPT_US`] of the lowest
/// delay lately is timed.
#[derive(Debug, Clone, Copy)]
pub struct DelayFloor {
    /// The lowest delay in this period and the one before (µs), and when
    /// this one began.
    floor: [i64; 2],
    since_us: u64,
}

/// See [`DelayFloor`].
const PROMPT_US: i64 = 2_000;
/// A period of the delay floor (µs): it follows a path that got longer
/// within two of them.
const FLOOR_PERIOD_US: u64 = 2_500_000;

impl Default for DelayFloor {
    fn default() -> Self {
        Self { floor: [i64::MAX; 2], since_us: 0 }
    }
}

impl DelayFloor {
    /// A frame's delay `delay_us` (its first packet's arrival less its send
    /// time, any clock offset included), arriving at `now_us`: true if it
    /// came about as soon as any lately.
    pub fn prompt(&mut self, delay_us: i64, now_us: u64) -> bool {
        if now_us.saturating_sub(self.since_us) >= FLOOR_PERIOD_US {
            (self.floor, self.since_us) = ([i64::MAX, self.floor[0]], now_us);
        }
        self.floor[0] = self.floor[0].min(delay_us);
        delay_us <= self.floor[0].min(self.floor[1]).saturating_add(PROMPT_US)
    }
}

/// A train of at least this many packets is timed for [`Stats::link_kbps`]
/// (an older host's probes: 8 data shards)…
const BURST_MIN_SHARDS: u32 = 8;
/// …as taking this long (µs) at least: packets handed over in one batch
/// arrive microseconds apart whatever the link (a whole frame "at 4 Gbit/s")…
const BURST_MIN_US: u64 = 150;
/// …and this long a packet (µs: a full one at 1.2 Gbit/s), so a train the
/// receiver read in one coalesced batch doesn't read as gigabits.
const TRAIN_PACKET_MIN_US: u64 = 8;

/// A probe train arriving: its frame, first and last arrival (µs), the
/// bytes after the first packet, packets, and whether it came promptly
/// (see [`DelayFloor`]).
#[derive(Debug, Clone, Copy)]
struct Train {
    frame_id: u32,
    first_us: u64,
    last_us: u64,
    bytes: u64,
    packets: u32,
    prompt: bool,
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
    /// Which frames came on time, for [`Stats::link_kbps`].
    floor: DelayFloor,
    /// The probe train arriving, if any.
    train: Option<Train>,
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
            floor: DelayFloor::default(),
            train: None,
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
        self.finish_train();
        std::mem::take(&mut self.stats)
    }

    /// A probe train is arriving (or the last packet was one's): the
    /// client's receive loop doesn't nap then, or the nap would be timed
    /// as the link (in office mode it read 1-2 ms on every train).
    pub fn train_open(&self) -> bool {
        self.train.is_some()
    }

    /// The train arriving, timed into [`Stats::link_kbps`] if it came
    /// promptly and long enough to tell.
    fn finish_train(&mut self) {
        let Some(t) = self.train.take() else { return };
        if t.packets >= BURST_MIN_SHARDS && t.prompt {
            let spread = t.last_us.saturating_sub(t.first_us).max(BURST_MIN_US).max(t.packets as u64 * TRAIN_PACKET_MIN_US);
            let kbps = (t.bytes * 8 * 1000 / spread).min(u32::MAX as u64) as u32;
            self.stats.link_kbps = self.stats.link_kbps.max(kbps);
        }
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
        // A probe train is timed whole, parity arriving after its frame
        // completed too (before the check below drops that).
        if h.flags & FLAG_PROBE != 0 {
            match &mut self.train {
                Some(t) if t.frame_id == h.frame_id => {
                    (t.last_us, t.bytes, t.packets) = (now_us, t.bytes + body.len() as u64, t.packets + 1);
                }
                _ => {
                    self.finish_train();
                    let prompt = self.floor.prompt(now_us as i64 - (h.capture_us + h.encode_us as u64) as i64, now_us);
                    self.train = Some(Train { frame_id: h.frame_id, first_us: now_us, last_us: now_us, bytes: 0, packets: 1, prompt });
                }
            }
        } else if self.train.is_some_and(|t| t.frame_id != h.frame_id) {
            self.finish_train();
        }
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
        self.floor.prompt(first_rx_us as i64 - (capture_us + encode_us as u64) as i64, first_rx_us);
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
            &OutFrame { frame_id: id, data, keyframe: id == 0, ltr: id % 4 == 1, recovery: id % 5 == 2, capture_us: 1000 + id as u64, encode_us: 3, probe: false },
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

    /// A run of packets lost back to back: sent block after block it takes
    /// one block past its parity; in turns, each block loses a few and the
    /// frame is rebuilt.
    #[test]
    fn interleaved_blocks_survive_a_run_of_losses() {
        let data = frame(0, 300_000);
        let f = OutFrame { frame_id: 0, data: &data, keyframe: true, ltr: false, recovery: false, capture_us: 0, encode_us: 0, probe: false };
        let buffers = FrameBuffers::default();
        let shards = packetize_shards(&f, 0.05, &buffers);
        let mut parity = Shards::default();
        parity_shards(&shards, &mut parity);
        assert!(shards.len() > 4 * MAX_BLOCK_DATA, "five blocks");
        let deliver = |interleave: bool| {
            let order: Vec<&[u8]> = send_order(&shards, interleave)
                .map(|i| shards.get(i))
                .chain(send_order(&parity, interleave).map(|i| parity.get(i)))
                .collect();
            assert_eq!(order.len(), shards.len() + parity.len(), "every body once");
            let mut r = Reassembler::new();
            // 12 lost back to back, a block's worth of parity and more.
            let got: Vec<InFrame> = order.iter().enumerate().filter(|(i, _)| !(100..112).contains(i)).flat_map(|(i, p)| frames(r.push(p, i as u64))).collect();
            got.first().map(|g| g.data == data)
        };
        assert_eq!(deliver(false), None, "block after block: lost");
        assert_eq!(deliver(true), Some(true), "in turns: rebuilt");
        // One block goes as it is.
        let small = OutFrame { data: &data[..20_000], ..f };
        let one = packetize_shards(&small, 0.05, &buffers);
        assert!(send_order(&one, true).eq(0..one.len()));
    }

    /// A probe frame is timed for the link, first packet to last; a paced
    /// frame (it times the pacer) and a probe held up on the way (it comes
    /// out in a burst) aren't.
    #[test]
    fn probes_time_the_link() {
        let data = frame(1, 30_000);
        // Sent at `at`, 1 ms on the way, each packet 10 µs after the last
        // (~1 Gbit/s); `late`: held up that much more.
        let deliver = |r: &mut Reassembler, id: u32, probe: bool, at: u64, late: u64| {
            let f = OutFrame { frame_id: id, data: &data, keyframe: false, ltr: false, recovery: false, capture_us: at, encode_us: 0, probe };
            for (i, p) in packetize(&f, 0.0).iter().enumerate() {
                r.push(p, at + 1_000 + late + i as u64 * 10);
            }
        };
        let mut r = Reassembler::new();
        deliver(&mut r, 0, false, 1_000, 0);
        assert_eq!(r.take_stats().link_kbps, 0, "a paced frame isn't timed");
        deliver(&mut r, 1, true, 20_000, 0);
        let kbps = r.take_stats().link_kbps;
        assert!((800_000..1_300_000).contains(&kbps), "{kbps}");
        deliver(&mut r, 2, true, 40_000, 30_000);
        assert_eq!(r.take_stats().link_kbps, 0, "a held-up probe isn't timed");
    }

    /// A probe of a 1 KB frame goes as a train of full packets, its parity
    /// padding it out; the client times all of it (parity after the frame
    /// completed too), and the padding is what the host leaves out of its
    /// sent bytes.
    #[test]
    fn a_probe_train_is_timed_whole() {
        let data = frame(7, 1_000);
        let f = OutFrame { frame_id: 0, data: &data, keyframe: false, ltr: false, recovery: false, capture_us: 1_000, encode_us: 0, probe: true };
        let packets = packetize(&f, 0.05);
        assert_eq!(packets.len(), PROBE_TRAIN_PACKETS);
        assert!(packets.iter().all(|p| p.len() == MAX_BODY));
        let plain = packetize(&OutFrame { probe: false, ..f }, 0.05);
        let bytes = |ps: &[Vec<u8>]| ps.iter().map(Vec::len).sum::<usize>();
        assert_eq!(probe_padding(1_000, 0.05), bytes(&packets) - bytes(&plain));
        // 50 µs a packet: 1172 bytes at ~190 Mbit/s.
        let mut r = Reassembler::new();
        let mut done = 0;
        for (i, p) in packets.iter().enumerate() {
            done += r.push(p, 2_000 + i as u64 * 50).iter().filter(|e| matches!(e, Event::Frame(f) if f.data == data)).count();
            assert!(r.train_open());
        }
        assert_eq!(done, 1, "the frame decodes (from its first packet)");
        let kbps = r.take_stats().link_kbps;
        assert!((180_000..200_000).contains(&kbps), "{kbps}");
        assert!(!r.train_open());
    }

    #[test]
    fn a_small_probe_is_cut_fine_enough_to_time() {
        let data = frame(1, 3_000);
        let mut r = Reassembler::new();
        let mut got = None;
        for (id, probe) in [(0, false), (1, true)] {
            let f = OutFrame { frame_id: id, data: &data, keyframe: false, ltr: false, recovery: false, capture_us: 1_000 + id as u64 * 20_000, encode_us: 0, probe };
            let packets = packetize(&f, 0.05);
            assert_eq!(packets.len() >= BURST_MIN_SHARDS as usize, probe, "{} packets", packets.len());
            for (i, p) in packets.iter().enumerate() {
                for e in r.push(p, 2_000 + id as u64 * 20_000 + i as u64 * 10) {
                    if let Event::Frame(f) = e {
                        got = Some(f.data.to_vec());
                    }
                }
            }
            assert_eq!(got.take().as_deref(), Some(&data[..]), "frame {id} decodes whole");
        }
        assert!(r.take_stats().link_kbps > 0, "the probe was timed");
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
                        let out = OutFrame { frame_id: id, data: &data, keyframe: false, ltr: false, recovery: false, capture_us: 1, encode_us: 2, probe: false };
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
            let out = OutFrame { frame_id: 5, data: &data, keyframe: true, ltr: false, recovery: true, capture_us: 9, encode_us: 4, probe: false };
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
