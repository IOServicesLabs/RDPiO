//! The video path packet by packet, for A/B tests of FEC sizing, keyframes
//! and repairs (`cargo test -p rdp-hyper fecsim -- --nocapture`).
//!
//! The host: frames from a workload (an office desktop that trickles and now
//! and then scrolls, paused most of the time; or a game), cut into shards and
//! parity as [`crate::video`] cuts them at the [`RateController`]'s ratio,
//! paced as the host's sender paces them, probe trains on a LAN; a long-term
//! reference every 250 ms, a loss repaired from the newest the client has (a
//! keyframe with none, or for a third repair within a second), a keyframe
//! after every resume. The path: a FIFO link with a drop-tail buffer, random
//! loss, and bursts in time (a Wi-Fi radio's interference: the same bursts in
//! both arms). The client: a block rebuilt from any `k` of its shards, a
//! frame lost once a newer one completes first, a repair asked for every
//! 100 ms (two round trips at least) for a second and keyframes after;
//! feedback every 50 ms.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::control::Feedback;
use crate::rate::{Limits, RateController, Switches};
use crate::video::{layout_of, order_of, parity_count, MAX_BLOCK_DATA, SHARD_HEADER};
use crate::wire::OVERHEAD;

const STEP_US: u64 = 100;
const FRAME_US: u64 = 16_667;
/// UDP and IPv4 headers, on the link but not in the wire counts.
const IP_UDP: usize = 28;

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Path {
    pub name: &'static str,
    pub capacity_bps: f64,
    pub one_way_us: u64,
    /// Each packet lost at random with this probability…
    pub loss: f64,
    /// …and bursts: this many a second, each this long on average (µs),
    /// losing this share of what goes meanwhile.
    pub bursts_per_s: f64,
    pub burst_us: f64,
    pub burst_loss: f64,
    /// …and runs of packets lost back to back (a Wi-Fi aggregate's retries
    /// running out, a queue overflowing): this share of packets, in runs this
    /// long on average (Gilbert-Elliott).
    pub run_loss: f64,
    pub run_pkts: f64,
    /// The radio drops what would wait longer than this in its queue (µs).
    pub buffer_us: u64,
    pub lan: bool,
    /// The rate this client's path sustained last time (a warm start).
    pub warm_bps: Option<u32>,
}

impl Path {
    /// Both laptops on one LAN's Wi-Fi, nothing lost.
    pub const CLEAN_LAN: Path = Path {
        name: "clean LAN",
        capacity_bps: 300e6,
        one_way_us: 2_000,
        loss: 0.0,
        bursts_per_s: 0.0,
        burst_us: 0.0,
        burst_loss: 0.0,
        run_loss: 0.0,
        run_pkts: 0.0,
        buffer_us: 50_000,
        lan: true,
        warm_bps: None,
    };
    /// 1% of packets lost at random.
    pub const WIFI_RANDOM: Path = Path { name: "Wi-Fi 1% random", loss: 0.01, ..Self::CLEAN_LAN };
    /// 1% of packets lost in runs of 8 on average.
    pub const WIFI_BURSTY: Path = Path { name: "Wi-Fi bursty", run_loss: 0.01, run_pkts: 8.0, ..Self::CLEAN_LAN };
    /// Fades: 3 a second of 3 ms on average, 90% lost in them (~1% of the
    /// time), and 0.1% at random.
    pub const WIFI_FADES: Path =
        Path { name: "Wi-Fi fades", loss: 0.001, bursts_per_s: 3.0, burst_us: 3_000.0, burst_loss: 0.9, ..Self::CLEAN_LAN };
    /// A 3 Mbit/s relay 40 ms away that queues (up to 2 s) rather than
    /// drops, 0.3% lost on the way, warm started at what it carried.
    pub const SLOW: Path = Path {
        name: "slow 3 Mbps relay",
        capacity_bps: 3e6,
        one_way_us: 40_000,
        loss: 0.003,
        bursts_per_s: 0.0,
        burst_us: 0.0,
        burst_loss: 0.0,
        run_loss: 0.0,
        run_pkts: 0.0,
        buffer_us: 2_000_000,
        lan: false,
        warm_bps: Some(2_500_000),
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Load {
    /// Office: a small change (0.2-1.5 KB) every 100-500 ms, a scroll (3-20
    /// KB frames at 60 fps for 0.2-1 s, then 15 frames' sharpening) every
    /// 3-12 s: ~0.3 Mbit/s. Active 8 s, paused 20 s (a keyframe on each
    /// resume).
    Office,
    /// 60 fps of 30-50 KB (~19 Mbit/s), never paused.
    Game,
    /// The office desktop looked at in glances: active 2 s, paused 3 s, a
    /// keyframe on each resume.
    Glances,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Key,
    Repair,
    Delta,
}

/// The parity ratio a frame of `bytes` and `kind` goes with.
pub(crate) fn parity(r: &RateController, bytes: usize, kind: Kind) -> f32 {
    r.parity_for(bytes, kind == Kind::Key)
}

struct Frame {
    capture_us: u64,
    kind: Kind,
    ltr: bool,
    probe: bool,
    /// Per block: k, m, data shards in, parity in.
    blocks: Vec<(usize, usize, usize, usize)>,
    /// Packets neither arrived nor lost yet.
    outstanding: usize,
    last_us: u64,
    complete: bool,
    resolved: bool,
    first_rx: Option<u64>,
    /// A probe train: first and last arrival, bytes after the first, packets.
    train: (u64, u64, u64, u64),
}

/// What a run sent and showed (summed over seeds).
#[derive(Default)]
pub(crate) struct Tally {
    pub frames: u64,
    pub shown: u64,
    pub data_pkts: u64,
    pub parity_pkts: u64,
    /// Of those, a probe train's padding.
    pub pad_pkts: u64,
    pub wire_bytes: u64,
    /// Frames FEC couldn't rebuild, keyframes among them.
    pub lost: u64,
    pub lost_keys: u64,
    pub keyframes: u64,
    pub repairs: u64,
    /// Each time the picture broke: ms until one was shown again.
    pub freezes: Vec<f64>,
    /// Each resume: ms to the first picture.
    pub resumes: Vec<f64>,
    /// Capture to shown (ms).
    pub latency: Vec<f64>,
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn pct(v: &[f64], p: usize) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s.get((s.len() * p / 100).min(s.len().saturating_sub(1))).copied().unwrap_or(0.0)
}

impl Tally {
    pub fn pkts_per_shown(&self) -> f64 {
        (self.data_pkts + self.parity_pkts) as f64 / self.shown.max(1) as f64
    }

    pub fn bytes_per_shown(&self) -> f64 {
        self.wire_bytes as f64 / self.shown.max(1) as f64
    }

    pub fn parity_pct(&self) -> f64 {
        self.parity_pkts as f64 * 100.0 / self.data_pkts.max(1) as f64
    }

    /// Sent and never shown.
    pub fn unshown(&self) -> u64 {
        self.frames - self.shown
    }

    pub fn freeze_ms(&self) -> f64 {
        self.freezes.iter().sum()
    }

    pub fn latency_mean(&self) -> f64 {
        mean(&self.latency)
    }

    pub fn latency_p95(&self) -> f64 {
        pct(&self.latency, 95)
    }

    pub fn resume_mean(&self) -> f64 {
        mean(&self.resumes)
    }

    pub fn header() {
        println!(
            "  {:<10} {:>7} {:>8} {:>7} {:>6} {:>5} {:>5} {:>5} {:>5} {:>7} {:>8} {:>7} {:>7} {:>7} {:>7}",
            "arm", "pkt/fr", "B/fr", "parity%", "pad%", "lost", "lostK", "keys", "rep", "unshown", "freeze", "fr p95", "res avg", "lat avg", "lat p95"
        );
    }

    pub fn row(&self, arm: &str) {
        println!(
            "  {:<10} {:>7.2} {:>8.0} {:>6.1}% {:>5.1}% {:>5} {:>5} {:>5} {:>5} {:>7} {:>6.0}ms {:>5.0}ms {:>5.0}ms {:>5.1}ms {:>5.1}ms",
            arm,
            self.pkts_per_shown(),
            self.bytes_per_shown(),
            self.parity_pct(),
            self.pad_pkts as f64 * 100.0 / self.data_pkts.max(1) as f64,
            self.lost,
            self.lost_keys,
            self.keyframes,
            self.repairs,
            self.unshown(),
            self.freeze_ms(),
            pct(&self.freezes, 95),
            self.resume_mean(),
            self.latency_mean(),
            self.latency_p95(),
        );
    }

    fn add(&mut self, o: Tally) {
        self.frames += o.frames;
        self.shown += o.shown;
        self.data_pkts += o.data_pkts;
        self.parity_pkts += o.parity_pkts;
        self.pad_pkts += o.pad_pkts;
        self.wire_bytes += o.wire_bytes;
        self.lost += o.lost;
        self.lost_keys += o.lost_keys;
        self.keyframes += o.keyframes;
        self.repairs += o.repairs;
        self.freezes.extend(o.freezes);
        self.resumes.extend(o.resumes);
        self.latency.extend(o.latency);
    }
}

/// `seeds` runs of `secs` each, summed.
pub(crate) fn run(switches: Switches, load: Load, path: Path, secs: u64, seeds: u64) -> Tally {
    let mut t = Tally::default();
    for seed in 0..seeds {
        t.add(simulate(switches, load, path, secs, seed));
    }
    t
}

/// The office workload's next frame at `t` (bytes), if any.
struct Office {
    rng: Rng,
    next_small: u64,
    next_scroll: u64,
    scroll_until: u64,
    sharpen: u32,
}

impl Office {
    fn new(seed: u64) -> Self {
        Self { rng: Rng(seed ^ 0x0FF1CE), next_small: 0, next_scroll: 3_000_000, scroll_until: 0, sharpen: 0 }
    }

    fn frame(&mut self, t: u64) -> Option<usize> {
        if t >= self.next_scroll {
            self.scroll_until = t + self.rng.range(200_000, 1_000_000);
            self.next_scroll = self.scroll_until + self.rng.range(3_000_000, 12_000_000);
        }
        if t < self.scroll_until {
            self.sharpen = 15;
            return Some(self.rng.range(3_000, 20_000) as usize);
        }
        if self.sharpen > 0 {
            self.sharpen -= 1;
            return Some(self.rng.range(1_000, 4_000) as usize);
        }
        if t >= self.next_small {
            self.next_small = t + self.rng.range(100_000, 500_000);
            return Some(self.rng.range(200, 1_500) as usize);
        }
        None
    }
}

fn simulate(switches: Switches, load: Load, path: Path, secs: u64, seed: u64) -> Tally {
    let mut tally = Tally::default();
    let mut r = RateController::new(Limits::default());
    r.set_switches(switches);
    r.set_lan(path.lan);
    if let Some(w) = path.warm_bps {
        r.warm_start(w);
    }
    let mut office = Office::new(seed);
    let mut game = Rng(seed ^ 0x6A3E);
    // The radio's interference bursts, the same in both arms.
    let mut bursts: VecDeque<(u64, u64)> = VecDeque::new();
    if path.bursts_per_s > 0.0 {
        let mut b = Rng(seed ^ 0xB0057);
        let mut at = 0.0;
        loop {
            at += -(1.0 - b.unit()).ln() / path.bursts_per_s * 1e6;
            if at >= (secs * 1_000_000) as f64 {
                break;
            }
            let len = -(1.0 - b.unit()).ln() * path.burst_us;
            bursts.push_back((at as u64, (at + len) as u64));
        }
    }
    let cycle = if load == Load::Glances { (2_000_000u64, 3_000_000u64) } else { (8_000_000, 20_000_000) };
    let paused_at = |t: u64| load != Load::Game && t % (cycle.0 + cycle.1) >= cycle.0;

    let mut frames: Vec<Frame> = Vec::new();
    // The sender's queue: (when, frame, block, parity, body bytes, lost on
    // the way). Which of a frame's packets the path loses (at random, in
    // runs) goes by the capture tick and the place in the frame's order, so
    // both arms lose the same places of the same tick's frame however they
    // fill them, and however their runs diverged before.
    let mut sending: VecDeque<(u64, usize, usize, bool, usize, bool)> = VecDeque::new();
    // On the way: (arrival, seq) → (frame, block, parity, body bytes, lost).
    let mut arriving: BinaryHeap<Reverse<(u64, u64, usize, usize, bool, usize, bool)>> = BinaryHeap::new();
    let mut seq = 0u64;
    let mut free_at = 0u64;
    // Host → client and back: (arrival, message).
    enum Msg {
        Feedback(Feedback),
        Key,
        Recover,
    }
    let mut to_host: VecDeque<(u64, Msg)> = VecDeque::new();

    // The host.
    let (mut want_key, mut recover) = (true, false);
    let mut next_capture = 0u64;
    let mut last_encoded: Option<u64> = None;
    // Long-term references: (frame, the client has it), two by turns.
    let mut slots: [Option<(usize, bool)>; 2] = [None; 2];
    let (mut next_slot, mut last_mark) = (0usize, None::<u64>);
    let mut repairs: VecDeque<u64> = VecDeque::new();
    let mut was_paused = false;
    let mut resumed_at: Option<u64> = None;

    // The client.
    let mut next_resolve = 0usize;
    let mut chain = false;
    let mut ltr_have: Option<usize> = None;
    let mut broken_at: Option<u64> = None;
    let mut last_ask: Option<u64> = None;
    let mut win = Feedback::default();
    let (mut delay_sum, mut delay_n) = (0i64, 0i64);
    let mut window_start = 0u64;
    let rtt = 2 * path.one_way_us;

    for t in (0..secs * 1_000_000).step_by(STEP_US as usize) {
        let now_ms = t / 1000;
        let paused = paused_at(t);

        // The host hears the client.
        while to_host.front().is_some_and(|m| m.0 <= t) {
            match to_host.pop_front().unwrap().1 {
                Msg::Feedback(fb) => {
                    if let Some(id) = fb.ltr_frame {
                        for s in slots.iter_mut().flatten() {
                            if s.0 == id as usize {
                                s.1 = true;
                            }
                        }
                    }
                    r.update(&fb, 50, now_ms);
                }
                Msg::Key => {
                    if !r.collapse_keyframe_request(now_ms) {
                        want_key = true;
                    }
                }
                Msg::Recover => {
                    if !r.collapse_keyframe_request(now_ms) {
                        recover = true;
                    }
                }
            }
        }

        // The host captures, encodes and queues a frame.
        if paused != was_paused {
            was_paused = paused;
            r.set_paused(paused);
            if paused {
                // (The client keeps its last picture, asks for nothing.)
                broken_at = None;
            } else {
                // The pipeline was let go: a new encoder, a keyframe.
                want_key = true;
                recover = false;
                slots = [None; 2];
                resumed_at = Some(t);
                chain = false;
                ltr_have = None;
            }
        }
        if t >= next_capture {
            next_capture += FRAME_US;
            let content = if paused {
                None
            } else if load != Load::Game {
                office.frame(t)
            } else {
                Some(game.range(30_000, 50_000) as usize)
            };
            let interval = r.drain_fps(now_ms).map_or(0, |d| 1_000_000 / d as u64);
            let due = last_encoded.is_none_or(|l| t + 2_000 >= l + interval);
            let bps = r.bitrate_bps();
            let pick = if paused {
                None
            } else if want_key {
                Some((Kind::Key, ((bps as f64 * 0.12 / 8.0) as usize).clamp(40_000, 300_000)))
            } else if recover {
                repairs.retain(|&a| t < a + 1_000_000);
                let newest = slots.iter().flatten().filter(|s| s.1).map(|s| s.0).max();
                if newest.is_some() && repairs.len() < 3 {
                    repairs.push_back(t);
                    Some((Kind::Repair, (2 * content.unwrap_or(0)).max(4_000)))
                } else {
                    repairs.clear();
                    Some((Kind::Key, ((bps as f64 * 0.12 / 8.0) as usize).clamp(40_000, 300_000)))
                }
            } else {
                content.filter(|_| due).map(|b| (Kind::Delta, b))
            };
            if let Some((kind, bytes)) = pick {
                (want_key, recover) = (false, false);
                last_encoded = Some(t);
                let id = frames.len();
                let mut ltr = false;
                match kind {
                    Kind::Key => {
                        slots = [None; 2];
                        repairs.clear();
                        r.note_keyframe(id as u32, bytes, now_ms);
                        tally.keyframes += 1;
                    }
                    Kind::Repair => tally.repairs += 1,
                    Kind::Delta => {
                        if last_mark.is_none_or(|m| t >= m + 250_000) {
                            slots[next_slot] = Some((id, false));
                            next_slot = (next_slot + 1) % 2;
                            last_mark = Some(t);
                            ltr = true;
                        }
                    }
                }
                let fec = parity(&r, bytes, kind);
                let probe = r.probe_due(bytes, now_ms);
                let (shards, size) = layout_of(bytes, probe);
                let (plain, _) = layout_of(bytes, false);
                let nblocks = shards.div_ceil(MAX_BLOCK_DATA);
                let mut blocks = Vec::new();
                let (mut pad, mut bodies) = (0usize, 0usize);
                for b in 0..nblocks {
                    let k = (shards - b * MAX_BLOCK_DATA).min(MAX_BLOCK_DATA);
                    let m = parity_count(k, b, shards, fec, probe);
                    if probe {
                        // (What the same frame takes unpadded: its own parity.)
                        let kp = (plain.saturating_sub(b * MAX_BLOCK_DATA)).min(MAX_BLOCK_DATA);
                        let own = if kp > 0 { parity_count(kp, b, plain, fec, false) } else { 0 };
                        pad += (k + m).saturating_sub(kp + own);
                    }
                    blocks.push((k, m, 0, 0));
                    bodies += (k + m) * (SHARD_HEADER + size);
                }
                let pad_bytes = pad * (SHARD_HEADER + size);
                r.note_sent(bytes, bodies - pad_bytes.min(bodies));
                r.note_probe_padding(pad_bytes);
                let packets: usize = blocks.iter().map(|b| b.0 + b.1).sum();
                tally.frames += 1;
                tally.data_pkts += shards as u64;
                tally.parity_pkts += (packets - shards) as u64;
                tally.pad_pkts += pad as u64;
                // The sender: a frame still going goes at once now.
                for p in sending.iter_mut() {
                    p.0 = p.0.min(t);
                }
                let body = SHARD_HEADER + size;
                let data_bytes = shards * (body + OVERHEAD);
                let window = (r.pace() as f64 / 60.0).max(1e-4);
                let total = data_bytes as f64 * (1.0 + fec as f64);
                let rate = if probe { f64::INFINITY } else { (bps as f64 * 2.5).max(total * 8.0 / window) / 8.0 };
                let mut sent = 0usize;
                let mut places = Rng(seed << 32 ^ t / FRAME_US);
                let mut in_run = false;
                // Data first, then parity, in the host's order.
                for parity_pass in [false, true] {
                    let ids: Vec<u8> =
                        blocks.iter().enumerate().flat_map(|(b, &(k, m, ..))| std::iter::repeat_n(b as u8, if parity_pass { m } else { k })).collect();
                    for i in order_of(ids.iter().copied(), r.switches().interleave) {
                        if path.run_pkts > 0.0 {
                            let u = places.unit();
                            in_run = if in_run { u >= 1.0 / path.run_pkts } else { u < path.run_loss / path.run_pkts / (1.0 - path.run_loss) };
                        }
                        let lost = in_run || places.unit() < path.loss;
                        sending.push_back((t + (sent as f64 / rate * 1e6) as u64, id, ids[i] as usize, parity_pass, body, lost));
                        sent += body + OVERHEAD;
                    }
                }
                frames.push(Frame {
                    capture_us: t,
                    kind,
                    ltr,
                    probe,
                    blocks,
                    outstanding: packets,
                    last_us: t,
                    complete: false,
                    resolved: false,
                    first_rx: None,
                    train: (0, 0, 0, 0),
                });
            }
        }

        // Onto the link.
        while sending.front().is_some_and(|p| p.0 <= t) {
            let (at, id, b, par, body, lost) = sending.pop_front().unwrap();
            let on_link = (body + OVERHEAD + IP_UDP) as f64 * 8e6 / path.capacity_bps;
            tally.wire_bytes += (body + OVERHEAD + IP_UDP) as u64;
            let start = free_at.max(at);
            let (arrive, lost) = if start - at > path.buffer_us {
                (at + path.one_way_us, true)
            } else {
                free_at = start + on_link as u64;
                while bursts.front().is_some_and(|x| x.1 < at) {
                    bursts.pop_front();
                }
                let faded = bursts.front().is_some_and(|x| x.0 <= at) && Rng(seed << 40 ^ at).unit() < path.burst_loss;
                (free_at + path.one_way_us, lost || faded)
            };
            seq += 1;
            arriving.push(Reverse((arrive, seq, id, b, par, body, lost)));
        }

        // The client.
        let mut completed: Option<usize> = None;
        while arriving.peek().is_some_and(|a| a.0 .0 <= t) {
            let Reverse((arrive, _, id, b, par, body, lost)) = arriving.pop().unwrap();
            let f = &mut frames[id];
            f.outstanding -= 1;
            f.last_us = f.last_us.max(arrive);
            if lost {
                continue;
            }
            win.shards += 1;
            win.bytes += body as u64;
            if f.probe {
                if f.train.3 == 0 {
                    f.train.0 = arrive;
                } else {
                    f.train.2 += body as u64;
                }
                (f.train.1, f.train.3) = (arrive, f.train.3 + 1);
                if f.outstanding == 0 {
                    let spread = (f.train.1 - f.train.0).max(150).max(f.train.3 * 8);
                    win.link_kbps = win.link_kbps.max((f.train.2 * 8 * 1000 / spread) as u32);
                }
            }
            f.first_rx.get_or_insert(arrive);
            let blk = &mut f.blocks[b];
            if par {
                blk.3 += 1;
            } else {
                blk.2 += 1;
            }
            if !f.complete && !f.resolved && f.blocks.iter().all(|x| x.2 + x.3 >= x.0) {
                f.complete = true;
                win.recovered += f.blocks.iter().map(|x| x.0.saturating_sub(x.2) as u32).sum::<u32>();
                completed = Some(completed.map_or(id, |c: usize| c.max(id)));
            }
        }
        // Frames resolve in order: complete ones decode; one still missing
        // pieces when a newer one completes (or long after its last packet) is lost.
        while next_resolve < frames.len() {
            let id = next_resolve;
            let f = &frames[id];
            let ok = f.complete;
            let give_up = completed.is_some_and(|c| c > id) || (f.outstanding == 0 && t >= f.last_us + 300_000);
            if !ok && !give_up {
                break;
            }
            next_resolve += 1;
            let f = &mut frames[id];
            f.resolved = true;
            if ok {
                win.frames += 1;
                let delay = (f.first_rx.unwrap() - f.capture_us) as i64;
                if delay_n == 0 || delay < win.delay_min_us {
                    win.delay_min_us = delay;
                }
                (delay_sum, delay_n) = (delay_sum + delay, delay_n + 1);
                let shows = match f.kind {
                    Kind::Key => true,
                    Kind::Repair => ltr_have.is_some(),
                    Kind::Delta => chain,
                };
                if f.kind == Kind::Key {
                    ltr_have = None;
                }
                if shows {
                    chain = true;
                    tally.shown += 1;
                    tally.latency.push((t - f.capture_us) as f64 / 1000.0);
                    if f.ltr {
                        ltr_have = Some(id);
                        win.ltr_frame = Some(id as u32);
                    }
                    if let Some(b) = broken_at.take() {
                        tally.freezes.push((t - b) as f64 / 1000.0);
                    }
                    if let Some(r) = resumed_at.take() {
                        tally.resumes.push((t - r) as f64 / 1000.0);
                    }
                }
            } else {
                win.frames_lost += 1;
                tally.lost += 1;
                tally.lost_keys += (f.kind == Kind::Key) as u64;
                chain = false;
                if broken_at.is_none() && !paused {
                    broken_at = Some(t);
                    last_ask = None;
                }
            }
        }
        // A broken picture: a repair while one can come, else a keyframe.
        if let Some(since) = broken_at {
            let repair = ltr_have.is_some() && t < since + 1_000_000;
            let every = if repair { 100_000.max(2 * rtt) } else { 100_000 };
            if last_ask.is_none_or(|a| t >= a + every) {
                last_ask = Some(t);
                to_host.push_back((t + path.one_way_us, if repair { Msg::Recover } else { Msg::Key }));
            }
        }
        if t >= window_start + 50_000 {
            if delay_n > 0 {
                win.delay_avg_us = delay_sum / delay_n;
            }
            win.window_us = 50_000;
            to_host.push_back((t + path.one_way_us, Msg::Feedback(win)));
            (win, delay_sum, delay_n, window_start) = (Feedback::default(), 0, 0, t);
        }
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECS: u64 = 120;
    const SEEDS: u64 = 16;

    /// Every load on every path, as both arms see them: (load, path, off, on).
    fn ab(name: &str, off: Switches, on: Switches, cases: &[(Load, Path)]) -> Vec<(Load, Path, Tally, Tally)> {
        println!("== {name} ==");
        cases
            .iter()
            .map(|&(load, path)| {
                println!("{load:?} on {}:", path.name);
                Tally::header();
                let (a, b) = (run(off, load, path, SECS, SEEDS), run(on, load, path, SECS, SEEDS));
                a.row("off");
                b.row("on");
                (load, path, a, b)
            })
            .collect()
    }

    /// Nothing worse: no more frames unshown (lost, or behind a loss until
    /// the repair), no more packets or bytes a frame shown, no longer to a
    /// picture (a resume or a repair), no slower. Within noise: 1%, 1 ms,
    /// frames lost 2% and unshown 1%, and a frame either way (an arm that
    /// differs in one frame's packets differs from then on in which frames
    /// the path's losses hit: a different repair, a different rate).
    fn no_worse(load: Load, path: Path, off: &Tally, on: &Tally) {
        let what = format!("{load:?} on {}", path.name);
        assert!(on.lost <= off.lost + off.lost / 50 + 1, "{what}: frames lost {} → {}", off.lost, on.lost);
        assert!(on.unshown() <= off.unshown() + off.unshown() / 100 + 1, "{what}: unshown {} → {}", off.unshown(), on.unshown());
        assert!(on.pkts_per_shown() <= off.pkts_per_shown() * 1.01, "{what}: packets a frame {:.2} → {:.2}", off.pkts_per_shown(), on.pkts_per_shown());
        assert!(on.bytes_per_shown() <= off.bytes_per_shown() * 1.01, "{what}: bytes a frame {:.0} → {:.0}", off.bytes_per_shown(), on.bytes_per_shown());
        assert!(on.freeze_ms() <= off.freeze_ms() * 1.01 + 1.0, "{what}: frozen {:.0} → {:.0} ms", off.freeze_ms(), on.freeze_ms());
        assert!(on.resume_mean() <= off.resume_mean() + 1.0, "{what}: to a picture {:.0} → {:.0} ms", off.resume_mean(), on.resume_mean());
        assert!(on.latency_p95() <= off.latency_p95() + 1.0, "{what}: p95 {:.1} → {:.1} ms", off.latency_p95(), on.latency_p95());
    }

    const PATHS: [Path; 5] = [Path::CLEAN_LAN, Path::WIFI_RANDOM, Path::WIFI_BURSTY, Path::WIFI_FADES, Path::SLOW];

    /// A frame of several blocks in turns: a run of losses costs each block
    /// a few. Keyframes (300 KB, five blocks) are what it's for.
    #[test]
    fn fecsim_interleave() {
        let on = Switches::default();
        let off = Switches { interleave: false, ..on };
        let mut cases: Vec<(Load, Path)> = PATHS.iter().map(|&p| (Load::Glances, p)).collect();
        cases.extend([(Load::Office, Path::WIFI_BURSTY), (Load::Game, Path::WIFI_BURSTY)]);
        let runs = ab("interleave", off, on, &cases);
        for (load, path, a, b) in &runs {
            no_worse(*load, *path, a, b);
        }
        let (_, _, a, b) = &runs[2];
        assert!(b.lost_keys * 2 <= a.lost_keys, "keyframes lost {} → {}", a.lost_keys, b.lost_keys);
        assert!(b.resume_mean() < a.resume_mean() * 0.7, "to a picture {:.0} → {:.0} ms", a.resume_mean(), b.resume_mean());
    }

    /// Padded probe trains every 5 s instead of every second while the
    /// stream carries a twentieth of the link or less: an idle office
    /// stream's packets were mostly trains.
    #[test]
    fn fecsim_probe_idle() {
        let on = Switches::default();
        let off = Switches { probe_idle: false, ..on };
        let mut cases: Vec<(Load, Path)> = PATHS.iter().map(|&p| (Load::Office, p)).collect();
        cases.extend([(Load::Glances, Path::CLEAN_LAN), (Load::Glances, Path::WIFI_BURSTY), (Load::Game, Path::CLEAN_LAN)]);
        let runs = ab("probeidle", off, on, &cases);
        for (load, path, a, b) in &runs {
            no_worse(*load, *path, a, b);
        }
        let (_, _, a, b) = &runs[0];
        assert!(b.pkts_per_shown() < a.pkts_per_shown() * 0.9, "office, clean LAN: {:.2} → {:.2}", a.pkts_per_shown(), b.pkts_per_shown());
    }

    /// The baseline: what every path and load costs today.
    #[test]
    fn fecsim_baseline() {
        for (load, path) in [
            (Load::Office, Path::CLEAN_LAN),
            (Load::Office, Path::WIFI_RANDOM),
            (Load::Office, Path::WIFI_BURSTY),
            (Load::Office, Path::WIFI_FADES),
            (Load::Office, Path::SLOW),
            (Load::Glances, Path::WIFI_BURSTY),
            (Load::Glances, Path::WIFI_FADES),
            (Load::Game, Path::CLEAN_LAN),
            (Load::Game, Path::WIFI_RANDOM),
            (Load::Game, Path::WIFI_BURSTY),
            (Load::Game, Path::WIFI_FADES),
        ] {
            println!("{load:?} on {}:", path.name);
            Tally::header();
            run(Switches::default(), load, path, SECS, SEEDS).row("default");
        }
    }
}
