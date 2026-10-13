//! A small reliable, ordered message channel over the lossy Hyper link, for
//! what must arrive whole: clipboard contents, cursor shapes, file transfers.
//!
//! Messages are cut into chunks that fit a datagram, each with its own
//! sequence number. The receiver acknowledges cumulatively plus a selective
//! bitmap covering the whole in-flight span, and batches its acks. The sender:
//! - keeps at most [`SPAN`] sequence numbers in flight (so every in-flight
//!   chunk can be acknowledged), further limited by a congestion window
//!   (slow start, then +1 per round trip) that halves once per loss episode
//!   — and also when the round trip grows [`QUEUE_LIMIT`] past its minimum,
//!   so a transfer backs off while a router queue builds, before it drops
//!   the stream's packets;
//! - declares a chunk lost once a chunk sent [`REORDER`] transmissions after
//!   it was acknowledged, or after a timeout derived from the measured round
//!   trip — and resends only lost chunks, within the window (never the whole
//!   window at once);
//! - paces everything through a token bucket ([`Sender::set_rate`]), so a big
//!   clipboard image can't flood a link the video stream needs;
//! - does O(span) work per ack at most (the queue is ordered by sequence and
//!   indexed directly).

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Largest chunk payload (a sealed datagram body minus this layer's header and
/// the link's lane byte).
pub const MAX_CHUNK: usize = crate::wire::MAX_BODY - 16;
/// Sequence numbers in flight at most; also the span the ack bitmap covers.
pub const SPAN: u32 = 256;
const BITMAP_BYTES: usize = SPAN as usize / 8;
const MIN_RTO: Duration = Duration::from_millis(40);
const MAX_RTO: Duration = Duration::from_millis(1000);
/// Before any round trip was measured.
const INITIAL_RTO: Duration = Duration::from_millis(200);
/// Congestion window bounds, in chunks.
const CWND_MIN: f64 = 4.0;
const CWND_START: f64 = 32.0;
/// A chunk is lost once one sent this many transmissions after it was
/// acknowledged (tolerates that much reordering).
pub const REORDER: u64 = 3;
/// Round-trip growth over the path's minimum that means a queue is building.
pub const QUEUE_LIMIT: Duration = Duration::from_millis(25);
/// The minimum round trip is re-learned this often (routes change).
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

const TYPE_DATA: u8 = 1;
const TYPE_ACK: u8 = 2;

struct Chunk {
    seq: u32,
    body: Vec<u8>,
    /// When it was last transmitted (`None` = not yet).
    sent: Option<Instant>,
    /// Transmission number of the last send: orders sends across resends.
    tx: u64,
    retries: u32,
    acked: bool,
    /// Declared lost: no longer counted in flight, resent as soon as the
    /// window allows.
    lost: bool,
}

impl Chunk {
    fn in_flight(&self) -> bool {
        self.sent.is_some() && !self.acked && !self.lost
    }
}

/// The sending half.
pub struct Sender {
    next_seq: u32,
    /// Ordered by `seq`, contiguous (`queue[i].seq == front + i`);
    /// acknowledged chunks are popped from the front.
    queue: VecDeque<Chunk>,
    backlog_bytes: usize,
    next_tx: u64,
    /// Highest transmission number acknowledged so far.
    acked_tx: u64,
    /// Losses of transmissions before this one belong to the episode the
    /// window was already cut for.
    recovery_tx: u64,
    srtt: Option<Duration>,
    /// Lowest round trip seen, and when.
    min_rtt: Option<(Duration, Instant)>,
    /// Round-trip growth over the minimum that counts as queueing.
    queue_limit: Duration,
    cwnd: f64,
    /// Below this the window grows per acknowledged chunk; above it, by one
    /// chunk per window.
    ssthresh: f64,
    /// Token bucket: bytes allowed now, refilled at `rate_bps`.
    rate_bps: f64,
    tokens: f64,
    last_refill: Option<Instant>,
}

impl Default for Sender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            queue: VecDeque::new(),
            backlog_bytes: 0,
            next_tx: 1,
            acked_tx: 0,
            recovery_tx: 0,
            srtt: None,
            min_rtt: None,
            queue_limit: QUEUE_LIMIT,
            cwnd: CWND_START,
            ssthresh: SPAN as f64,
            rate_bps: f64::INFINITY,
            tokens: 0.0,
            last_refill: None,
        }
    }

    /// Pace the channel at `bps` (bits per second); `None` = unpaced.
    pub fn set_rate(&mut self, bps: Option<u64>) {
        self.rate_bps = bps.map_or(f64::INFINITY, |b| b.max(64_000) as f64);
    }

    /// Back off once the round trip grows this much over its minimum
    /// (default [`QUEUE_LIMIT`]; lower for traffic that must yield, like bulk
    /// copies sharing an uplink with game input).
    pub fn set_queue_limit(&mut self, limit: Duration) {
        self.queue_limit = limit;
    }

    /// Queue a message (any size).
    pub fn send(&mut self, msg: &[u8]) {
        let chunks: Vec<&[u8]> = if msg.is_empty() { vec![&[][..]] } else { msg.chunks(MAX_CHUNK).collect() };
        let last = chunks.len() - 1;
        for (i, c) in chunks.into_iter().enumerate() {
            let mut body = Vec::with_capacity(6 + c.len());
            body.push(TYPE_DATA);
            body.extend_from_slice(&self.next_seq.to_le_bytes());
            body.push((i == last) as u8);
            body.extend_from_slice(c);
            self.backlog_bytes += body.len();
            self.queue.push_back(Chunk {
                seq: self.next_seq,
                body,
                sent: None,
                tx: 0,
                retries: 0,
                acked: false,
                lost: false,
            });
            self.next_seq = self.next_seq.wrapping_add(1);
        }
    }

    /// Bytes waiting (unacknowledged or unsent).
    pub fn backlog(&self) -> usize {
        self.backlog_bytes
    }

    fn rto(&self) -> Duration {
        self.srtt.map_or(INITIAL_RTO, |r| (r * 2 + Duration::from_millis(10)).clamp(MIN_RTO, MAX_RTO))
    }

    /// The chunks within [`SPAN`] of the front (the only ones that may be in
    /// flight).
    fn window(&mut self) -> impl Iterator<Item = &mut Chunk> {
        let front = self.queue.front().map_or(0, |c| c.seq);
        self.queue.iter_mut().take_while(move |c| c.seq.wrapping_sub(front) < SPAN)
    }

    /// Halve the window, once per congestion episode (a loss or delay sample
    /// of a transmission sent before the last cut is part of the episode
    /// already paid for).
    fn on_congestion(&mut self, tx: u64) {
        if tx >= self.recovery_tx {
            self.ssthresh = (self.cwnd / 2.0).max(CWND_MIN);
            self.cwnd = self.ssthresh;
            self.recovery_tx = self.next_tx;
        }
    }

    /// Chunk bodies due to go out now: resends of lost chunks first, then new
    /// ones, within the span, the congestion window and the pacing budget.
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        // Refill the pacing bucket (at most ~20 ms of burst).
        if self.rate_bps.is_finite() {
            let dt = self.last_refill.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
            let cap = (self.rate_bps / 8.0 * 0.02).max(2.0 * crate::wire::MAX_DATAGRAM as f64);
            self.tokens = (self.tokens + dt * self.rate_bps / 8.0).min(cap);
            self.last_refill = Some(now);
        }
        // Timeouts (each chunk backs off on its own resends).
        let rto = self.rto();
        let mut in_flight = 0.0;
        let mut lost_tx = None::<u64>;
        for c in self.window() {
            if !c.in_flight() {
                continue;
            }
            if c.sent.is_some_and(|t| now.duration_since(t) >= rto * (1 << c.retries.min(4))) {
                c.lost = true;
                lost_tx = lost_tx.max(Some(c.tx));
            } else {
                in_flight += 1.0;
            }
        }
        if let Some(tx) = lost_tx {
            self.on_congestion(tx);
        }
        let (cwnd, paced) = (self.cwnd, self.rate_bps.is_finite());
        let mut tokens = self.tokens;
        let mut next_tx = self.next_tx;
        let mut out = Vec::new();
        for c in self.window() {
            if c.acked || (c.sent.is_some() && !c.lost) {
                continue;
            }
            if in_flight >= cwnd {
                break;
            }
            if paced {
                if tokens < c.body.len() as f64 {
                    break;
                }
                tokens -= c.body.len() as f64;
            }
            if c.sent.is_some() {
                c.retries += 1;
            }
            c.lost = false;
            c.sent = Some(now);
            c.tx = next_tx;
            next_tx += 1;
            in_flight += 1.0;
            out.push(c.body.clone());
        }
        self.tokens = tokens;
        self.next_tx = next_tx;
        out
    }

    /// Fold in an ack body.
    pub fn on_ack(&mut self, body: &[u8], now: Instant) {
        let Some((cum, bits)) = parse_ack(body) else { return };
        // Never acknowledges what wasn't sent yet (a confused peer).
        if cum.wrapping_sub(self.next_seq) as i32 >= 0 {
            return;
        }
        // The freshest round-trip sample: (rtt, transmission).
        let mut sample = None::<(Duration, u64)>;
        let mut newly = 0u32;
        let mut acked_tx = self.acked_tx;
        let mut take = |c: &Chunk, newly: &mut u32| {
            if c.acked || c.sent.is_none() {
                return;
            }
            *newly += 1;
            // Karn: only a chunk sent once says unambiguously which
            // transmission arrived — for timing the round trip, and for
            // judging what was sent before it lost (an ack for a resent
            // chunk may be for its first copy, which went out long ago).
            if let (0, Some(t)) = (c.retries, c.sent) {
                acked_tx = acked_tx.max(c.tx);
                if sample.is_none_or(|(_, tx)| c.tx > tx) {
                    sample = Some((now.duration_since(t), c.tx));
                }
            }
        };
        // Cumulative part: pop from the front.
        while self.queue.front().is_some_and(|c| c.seq.wrapping_sub(cum) as i32 <= 0) {
            let c = self.queue.pop_front().unwrap();
            self.backlog_bytes -= c.body.len();
            take(&c, &mut newly);
        }
        // Selective part: index straight into the span after `cum`.
        if let Some(front) = self.queue.front().map(|c| c.seq) {
            for i in 0..SPAN {
                if bits[(i / 8) as usize] & (1 << (i % 8)) == 0 {
                    continue;
                }
                let seq = cum.wrapping_add(1).wrapping_add(i);
                let Some(c) = self.queue.get_mut(seq.wrapping_sub(front) as usize) else { continue };
                if c.seq == seq {
                    take(c, &mut newly);
                    c.acked = true;
                    c.lost = false;
                }
            }
        }
        self.acked_tx = acked_tx;
        // Loss detection: a chunk sent REORDER transmissions before one that
        // was acknowledged is lost, not late.
        let mut lost_tx = None::<u64>;
        for c in self.window() {
            if c.in_flight() && c.tx + REORDER <= acked_tx {
                c.lost = true;
                lost_tx = lost_tx.max(Some(c.tx));
            }
        }
        // Delay: a round trip well above the path's minimum is a queue
        // building up somewhere — yield before it overflows.
        let mut queued_tx = None;
        if let Some((rtt, tx)) = sample {
            self.srtt = Some(match self.srtt {
                Some(r) => (r * 7 + rtt) / 8,
                None => rtt,
            });
            match self.min_rtt {
                Some((m, at)) if rtt >= m && now.duration_since(at) < MIN_RTT_WINDOW => {
                    if rtt > m + self.queue_limit {
                        queued_tx = Some(tx);
                    }
                }
                _ => self.min_rtt = Some((rtt, now)),
            }
        }
        // The newest transmission that lost or queued decides whether this is
        // a new episode (one sent after the last cut) or the one already cut
        // for.
        match lost_tx.max(queued_tx) {
            Some(tx) => self.on_congestion(tx),
            None if newly > 0 => {
                let grow = if self.cwnd < self.ssthresh { newly as f64 } else { newly as f64 / self.cwnd };
                self.cwnd = (self.cwnd + grow).min(SPAN as f64);
            }
            None => {}
        }
        // Drop acknowledged chunks that reached the front.
        while self.queue.front().is_some_and(|c| c.acked) {
            let c = self.queue.pop_front().unwrap();
            self.backlog_bytes -= c.body.len();
        }
    }
}

fn parse_ack(b: &[u8]) -> Option<(u32, [u8; BITMAP_BYTES])> {
    if b.first() != Some(&TYPE_ACK) || b.len() < 5 + BITMAP_BYTES {
        return None;
    }
    let cum = u32::from_le_bytes(b[1..5].try_into().ok()?);
    Some((cum, b[5..5 + BITMAP_BYTES].try_into().ok()?))
}

/// Whether a reliable-channel body is an ack (for the sender) or data (for
/// the receiver).
pub fn is_ack(body: &[u8]) -> bool {
    body.first() == Some(&TYPE_ACK)
}

/// The receiving half: delivers whole messages, in order, exactly once.
#[derive(Default)]
pub struct Receiver {
    /// Highest sequence number delivered in order (0 = none yet).
    cum: u32,
    pending: BTreeMap<u32, (bool, Vec<u8>)>,
    partial: Vec<u8>,
    /// The message being assembled outgrew [`MAX_MESSAGE`]: drop it whole.
    oversize: bool,
    /// Chunks received since the last ack, and when the first of them came.
    unacked: u32,
    first_unacked: Option<Instant>,
    /// Something happened that the sender should hear about at once (a hole,
    /// a duplicate, a finished message).
    urgent: bool,
}

/// Messages larger than this are dropped (protects memory).
pub const MAX_MESSAGE: usize = 256 * 1024 * 1024;
/// Ack at least every this many chunks…
const ACK_EVERY: u32 = 4;
/// …or this long after the first unacknowledged one.
pub const ACK_DELAY: Duration = Duration::from_millis(5);

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in a data body; returns completed messages.
    pub fn on_data(&mut self, body: &[u8], now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if body.first() != Some(&TYPE_DATA) || body.len() < 6 {
            return out;
        }
        let seq = u32::from_le_bytes(body[1..5].try_into().unwrap());
        let last = body[5] != 0;
        self.unacked += 1;
        self.first_unacked.get_or_insert(now);
        let ahead = seq.wrapping_sub(self.cum);
        if ahead as i32 <= 0 || ahead > SPAN {
            // A duplicate (our ack was probably lost) or nonsense.
            self.urgent = true;
            return out;
        }
        if ahead != 1 {
            self.urgent = true; // a hole: tell the sender which chunks we have
        }
        self.pending.entry(seq).or_insert_with(|| (last, body[6..].to_vec()));
        while let Some((last, data)) = self.pending.remove(&self.cum.wrapping_add(1)) {
            self.cum = self.cum.wrapping_add(1);
            if self.partial.len() + data.len() > MAX_MESSAGE {
                self.oversize = true;
                self.partial = Vec::new();
            }
            if !self.oversize {
                self.partial.extend_from_slice(&data);
            }
            if last {
                let msg = std::mem::take(&mut self.partial);
                if !std::mem::take(&mut self.oversize) {
                    out.push(msg);
                }
                self.urgent = true;
            }
        }
        out
    }

    /// When [`Self::ack`] will have an ack to send (`None`: nothing to
    /// acknowledge). Already passed when one is due now.
    pub fn ack_deadline(&self) -> Option<Instant> {
        let first = self.first_unacked.filter(|_| self.unacked > 0)?;
        Some(if self.urgent || self.unacked >= ACK_EVERY { first } else { first + ACK_DELAY })
    }

    /// An ack to send now, if one is due (batched: every few chunks, after a
    /// short delay, or at once for holes, duplicates and finished messages).
    /// Call after every packet and by [`Self::ack_deadline`].
    pub fn ack(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self.unacked == 0 {
            return None;
        }
        let due = self.urgent
            || self.unacked >= ACK_EVERY
            || self.first_unacked.is_some_and(|t| now.duration_since(t) >= ACK_DELAY);
        if !due {
            return None;
        }
        self.unacked = 0;
        self.first_unacked = None;
        self.urgent = false;
        let mut bits = [0u8; BITMAP_BYTES];
        for &s in self.pending.keys() {
            let i = s.wrapping_sub(self.cum.wrapping_add(1));
            if i < SPAN {
                bits[(i / 8) as usize] |= 1 << (i % 8);
            }
        }
        let mut b = Vec::with_capacity(5 + BITMAP_BYTES);
        b.push(TYPE_ACK);
        b.extend_from_slice(&self.cum.to_le_bytes());
        b.extend_from_slice(&bits);
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator for loss patterns.
    struct Lcg(u64);
    impl Lcg {
        fn chance(&mut self, p: f64) -> bool {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64 / (1u64 << 31) as f64) < p
        }
    }

    /// Shuttle chunks between a sender and receiver through a lossy pipe
    /// (`drop(i, is_ack)` decides per transmission) until everything is
    /// delivered. The receiver acks the way the link does:
    /// after each packet (batched inside) and on a 5 ms tick. Returns the
    /// messages and how many chunk transmissions it took.
    fn transfer(msgs: &[Vec<u8>], mut drop: impl FnMut(usize, bool) -> bool) -> (Vec<Vec<u8>>, usize) {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        for m in msgs {
            s.send(m);
        }
        let mut got = Vec::new();
        let mut now = Instant::now();
        let (mut n, mut sent) = (0, 0);
        for _ in 0..20_000 {
            let mut acks = Vec::new();
            for chunk in s.poll(now) {
                n += 1;
                sent += 1;
                if !drop(n, false) {
                    got.extend(r.on_data(&chunk, now));
                    acks.extend(r.ack(now));
                }
            }
            acks.extend(r.ack(now));
            for a in acks {
                n += 1;
                if !drop(n, true) {
                    s.on_ack(&a, now);
                }
            }
            if got.len() == msgs.len() && s.backlog() == 0 {
                break;
            }
            now += Duration::from_millis(5);
        }
        (got, sent)
    }

    #[test]
    fn messages_arrive_whole_and_in_order() {
        let msgs = vec![b"hello".to_vec(), vec![7u8; MAX_CHUNK * 3 + 5], Vec::new(), b"bye".to_vec()];
        assert_eq!(transfer(&msgs, |_, _| false).0, msgs);
    }

    #[test]
    fn heavy_loss_is_recovered() {
        let msgs: Vec<Vec<u8>> = (0..20).map(|i| vec![i as u8; 3000 + i * 100]).collect();
        // Drop every third transmission, data and acks alike.
        assert_eq!(transfer(&msgs, |n, _| n % 3 == 0).0, msgs);
        // Half of everything, at random.
        let mut rng = Lcg(7);
        assert_eq!(transfer(&msgs, |_, _| rng.chance(0.5)).0, msgs);
    }

    #[test]
    fn random_loss_is_recovered_without_much_waste() {
        let msgs: Vec<Vec<u8>> = (0..50).map(|i| vec![i as u8; 5000 + i * 37]).collect();
        let chunks: usize = msgs.iter().map(|m| m.len().div_ceil(MAX_CHUNK)).sum();
        for (seed, p) in [(1u64, 0.05), (2, 0.2), (3, 0.4)] {
            let mut rng = Lcg(seed);
            let (got, sent) = transfer(&msgs, |_, _| rng.chance(p));
            assert_eq!(got, msgs, "loss {p}");
            // Each chunk needs 1/(1-p) sends on average; allow slack for lost
            // acks and timeouts.
            let bound = (chunks as f64 / (1.0 - p) * 2.0) as usize + 20;
            assert!(sent <= bound, "{sent} transmissions for {chunks} chunks at loss {p}");
        }
    }

    #[test]
    fn an_ack_blackout_does_not_stall_the_transfer() {
        let msgs = vec![vec![9u8; MAX_CHUNK * 100]];
        // Every ack lost for the first 300 transmissions.
        let (got, _) = transfer(&msgs, |n, ack| ack && n < 300);
        assert_eq!(got, msgs);
    }

    #[test]
    fn one_lost_chunk_is_resent_once_not_the_whole_window() {
        let msgs = vec![vec![1u8; MAX_CHUNK * 200]];
        let (got, sent) = transfer(&msgs, |n, _| n == 2);
        assert_eq!(got, msgs);
        assert!(sent <= 202, "{sent} transmissions for 200 chunks with one loss");
    }

    #[test]
    fn a_timeout_resends_within_the_window_not_everything() {
        let mut s = Sender::new();
        s.send(&vec![0u8; MAX_CHUNK * 64]);
        let t0 = Instant::now();
        let first = s.poll(t0).len();
        assert_eq!(first as f64, CWND_START);
        // Nothing acknowledged: after the timeout only half the window goes
        // again.
        let again = s.poll(t0 + INITIAL_RTO).len();
        assert_eq!(again as f64, CWND_START / 2.0, "resent {again}");
    }

    #[test]
    fn a_growing_round_trip_shrinks_the_window() {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        // Enough that the transfer outlasts the test.
        s.send(&vec![0u8; MAX_CHUNK * 20_000]);
        let mut now = Instant::now();
        // Acks come back after `rtt`; a queue builds up halfway through.
        let mut in_air: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
        let mut cwnd_clean = 0.0;
        for step in 0..400 {
            let rtt = Duration::from_millis(if step < 200 { 10 } else { 60 });
            for c in s.poll(now) {
                r.on_data(&c, now);
                if let Some(a) = r.ack(now) {
                    in_air.push_back((now + rtt, a));
                }
            }
            if let Some(a) = r.ack(now) {
                in_air.push_back((now + rtt, a));
            }
            while in_air.front().is_some_and(|(t, _)| *t <= now) {
                let (_, a) = in_air.pop_front().unwrap();
                s.on_ack(&a, now);
            }
            if step == 199 {
                cwnd_clean = s.cwnd;
            }
            now += Duration::from_millis(1);
        }
        assert!(cwnd_clean >= CWND_START, "clean path: window {cwnd_clean}");
        assert!(s.cwnd < cwnd_clean / 2.0, "queueing: window {} (was {cwnd_clean})", s.cwnd);
    }

    #[test]
    fn ack_deadline_tracks_batching() {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        s.send(&vec![0u8; MAX_CHUNK * 8]);
        let now = Instant::now();
        assert_eq!(r.ack_deadline(), None);
        let chunks = s.poll(now);
        r.on_data(&chunks[0], now);
        assert_eq!(r.ack_deadline(), Some(now + ACK_DELAY));
        r.on_data(&chunks[2], now); // a hole: due at once
        assert_eq!(r.ack_deadline(), Some(now));
        assert!(r.ack(now).is_some());
        assert_eq!(r.ack_deadline(), None);
    }

    #[test]
    fn in_flight_stays_within_the_span_and_window() {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        s.send(&vec![0u8; MAX_CHUNK * 2000]);
        let mut now = Instant::now();
        // Hold back chunk 1 forever: the window can't run past SPAN of it.
        let mut max_seq = 0;
        for _ in 0..200 {
            for c in s.poll(now) {
                let seq = u32::from_le_bytes(c[1..5].try_into().unwrap());
                max_seq = max_seq.max(seq);
                if seq != 1 {
                    r.on_data(&c, now);
                }
            }
            if let Some(a) = r.ack(now) {
                s.on_ack(&a, now);
            }
            now += Duration::from_millis(5);
        }
        assert!(max_seq <= SPAN, "sent seq {max_seq} while seq 1 was outstanding");
    }

    #[test]
    fn pacing_limits_the_rate() {
        let mut s = Sender::new();
        s.set_rate(Some(8_000_000)); // 1 MB/s
        s.send(&vec![0u8; MAX_CHUNK * 1000]);
        let mut r = Receiver::new();
        let t0 = Instant::now();
        let mut bytes = 0;
        let mut now = t0;
        for _ in 0..200 {
            // 1 s in 5 ms steps
            for c in s.poll(now) {
                bytes += c.len();
                r.on_data(&c, now);
            }
            if let Some(a) = r.ack(now) {
                s.on_ack(&a, now);
            }
            now += Duration::from_millis(5);
        }
        assert!(bytes < 1_100_000 && bytes > 800_000, "{bytes} bytes in 1 s at 1 MB/s");
    }

    #[test]
    fn duplicates_are_delivered_once() {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        s.send(b"once");
        let now = Instant::now();
        let c = s.poll(now).remove(0);
        assert_eq!(r.on_data(&c, now).len(), 1);
        assert!(r.on_data(&c, now).is_empty());
    }

    #[test]
    fn acks_are_batched() {
        let mut s = Sender::new();
        let mut r = Receiver::new();
        s.send(&vec![0u8; MAX_CHUNK * 3]);
        let now = Instant::now();
        let chunks = s.poll(now);
        r.on_data(&chunks[0], now);
        assert!(r.ack(now).is_none(), "one in-order chunk: wait");
        assert!(r.ack(now + ACK_DELAY).is_some(), "until the delay passes");
    }

    #[test]
    fn an_ack_for_unsent_chunks_is_ignored() {
        let mut s = Sender::new();
        s.send(&vec![0u8; MAX_CHUNK * 3]);
        let mut bogus = vec![TYPE_ACK];
        bogus.extend_from_slice(&1000u32.to_le_bytes());
        bogus.extend_from_slice(&[0u8; BITMAP_BYTES]);
        s.on_ack(&bogus, Instant::now());
        assert!(s.backlog() > 0);
    }
}
