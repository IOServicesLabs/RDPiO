//! Host-side bitrate and FEC control from the client's feedback.
//!
//! Delay-based first, like WebRTC's congestion control: the one-way delay's
//! rise above its recent minimum is queueing in some router or Wi-Fi radio,
//! and backing off *then* keeps latency low before loss even starts. Loss that
//! FEC couldn't absorb (a lost frame) is the strongest signal. Increases are
//! slow and only happen while the encoder is actually using its budget, so a
//! static desktop doesn't "earn" a bitrate the link can't carry.

use crate::control::Feedback;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min_bps: u32,
    pub start_bps: u32,
    pub max_bps: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { min_bps: 500_000, start_bps: 20_000_000, max_bps: 80_000_000 }
    }
}

/// Queueing delay (µs) above which we back off gently, and hard.
const QUEUE_SOFT_US: i64 = 10_000;
const QUEUE_HARD_US: i64 = 30_000;
/// After a decrease, hold increases for this long (ms).
const HOLD_MS: u64 = 600;
/// With no decrease for this long, an idle stream drifts back to its resting
/// rate (ms): what a still screen "lost" to Wi-Fi jitter it gets back before
/// the next burst of motion or new text needs it.
const RECOVER_AFTER_MS: u64 = 5_000;
/// Pacing bounds, as fractions of a frame interval.
const PACE_MIN: f32 = 0.03;
const PACE_MAX: f32 = 0.6;
/// Feedback windows of delay minima kept for the baseline (~30 s at 50 ms)…
const BASELINE_WINDOWS: usize = 600;
/// …and no older than this. Long enough that a backlog lasting seconds (a
/// relay that queues rather than drops) can't become the baseline itself.
const BASELINE_MS: u64 = 30_000;
/// The delay is the client's clock minus the host's, and the two drift
/// apart: after a pause this long with no frames (a minimized client, a still
/// desktop) the baseline starts over, or the drift would read as queueing.
const PAUSE_MS: u64 = 2_000;
/// A standing queue: even each window's *fastest* packet this late (µs) for
/// [`STANDING_MS`]. Wi-Fi power save delays some packets up to a beacon
/// interval, never all of them for that long; a relay over TCP (Tailscale's
/// DERP) or a bloated router buffer does, losing nothing, for seconds.
const STANDING_US: i64 = 100_000;
const STANDING_MS: u64 = 500;
/// While it stands, cut this often unless it's draining…
const STANDING_EVERY_MS: u64 = 1_000;
/// …and after this long without draining at all, the path itself got longer
/// (a relay instead of a direct route): that delay is the new baseline.
const STANDING_REBASE_MS: u64 = 15_000;
/// A standing queue that moved no more than this (µs, or a fifth of itself)
/// the whole time is flat: a longer path, not a backlog.
const STANDING_FLAT_US: i64 = 50_000;
/// Bulk transfers (clipboard, files) share the link as a scavenger: they start
/// here, halve at the first sign of queueing and grow only while busy.
const BULK_START: f64 = 8_000_000.0;
const BULK_MIN: f64 = 1_000_000.0;
const BULK_MAX: f64 = 100_000_000.0;
/// A path that lost nothing for this long (ms) counts as clean (see
/// [`RateController::parity_for`]).
const CLEAN_AFTER_MS: u64 = 10_000;
/// On a clean path, frames up to this size go without parity.
pub const PARITY_FREE_BYTES: usize = 4096;
/// On a clean path, bigger non-key frames carry this much parity rather than
/// the 5% floor: over a relay that queues instead of dropping (TCP), parity
/// is pure overhead, and the rare Wi-Fi loss is repaired from a long-term
/// reference. (Every block still gets at least one parity shard.)
const CLEAN_FEC: f32 = 0.02;
/// Drain mode: while a standing queue drains, the host encodes this many
/// frames a second, so the backlog drains at nearly the link's whole rate
/// instead of the sliver a cut bitrate leaves (see [`RateController::drain_fps`]).
pub const DRAIN_FPS: u32 = 8;
/// …until the fastest packet is back within this of the baseline (µs; half
/// the standing threshold, so it doesn't flap at its edge).
const DRAIN_END_US: i64 = STANDING_US / 2;
/// …or once it has stayed under the standing threshold this long (ms).
const DRAIN_TAIL_MS: u64 = 2_000;
/// A keyframe counts as on its way for at most this long (ms): the time it
/// takes at the current rate, twice, plus the standing queue and a margin…
const KEY_WAIT_BASE_MS: f64 = 400.0;
const KEY_WAIT_MIN_MS: f64 = 600.0;
/// …never longer (a lost feedback window can leave the count short).
const KEY_WAIT_MAX_MS: f64 = 10_000.0;
/// A keyframe still on its way after this long (ms) is crossing a slow link,
/// and saturating it: the host sends it at 2.5x the target rate at least.
/// What arrives meanwhile is what the link carries — the rate is cut to
/// that, and few other frames go (see [`RateController::drain_fps`]). It's
/// what a stream's start needs most: nothing tells the host about the path
/// until the first keyframe lands, and at 20 Mbps the frames sent behind a
/// 300 KB keyframe on a 1 Mbps relay were tens of seconds of backlog. A LAN
/// delivers a keyframe well within this.
const KEY_PROBE_MS: u64 = 250;
/// The delivery gap: over this long (ms)…
const GAP_MS: u64 = 500;
/// …delivered under this share of what was sent cuts to what's delivered,
/// and under this one the rate doesn't grow. (Loss alone stays well above:
/// 10% of packets lost is 0.9. A burst starting shows at most the one-way
/// delay's share of the half second missing.)
const GAP_CUT: f64 = 0.5;
const GAP_GROW: f64 = 0.8;
/// Under this target rate (or with a queue standing) the sender paces at
/// 1.2x it (see [`RateController::pace_cap_bps`]).
const SLOW_PACE_BELOW_BPS: f64 = 3_000_000.0;
/// A warm start (see [`RateController::warm_start`]) grows this much a
/// window while the path stays clear, up to the resting rate.
const WARM_RAMP: f64 = 1.06;
/// Feedback windows a sustained rate needs before it's worth remembering.
const SUSTAINED_MIN_WINDOWS: u32 = 40;
/// A path that has shown it carries little (a cut for a standing queue, a
/// slow keyframe, a gap) stays slow this long (ms) after the last such cut,
/// and meanwhile the rate grows to at most [`GROW_HEADROOM`] times the most
/// it delivered over half a second in the last [`CARRIED_MS`]. An idle
/// office stream on a 1 Mbps relay otherwise recovered 0.5 → 11 Mbps in
/// 20 s (nothing pushes back while little is sent) and the next scroll's
/// frames, sized for 11, queued for seconds.
const SLOW_PATH_MS: u64 = 60_000;
const CARRIED_MS: u64 = 20_000;
const GROW_HEADROOM: f64 = 2.0;

/// Behaviours the host can switch off for A/B tests (all on by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// Cut for a standing queue (`standingq`).
    pub standing_queue: bool,
    /// Drain mode while it drains (`drain`).
    pub drain: bool,
    /// One keyframe in flight (`onekey`).
    pub one_keyframe: bool,
    /// The clean path's lower parity (`fecfloor`).
    pub fec_floor: bool,
    /// A slow keyframe measures the link, and few frames follow it
    /// (`keyprobe`; see [`KEY_PROBE_MS`]).
    pub key_probe: bool,
    /// Far more sent than delivered cuts to what's delivered (`gapcut`; see
    /// [`GAP_MS`]).
    pub gap_cut: bool,
    /// Pace at 1.2x the target on a slow or queued link (`slowpace`; see
    /// [`RateController::pace_cap_bps`]). Off by default: simulated on a
    /// 1.2 Mbps relay with a keyframe every 4 s, audio's p95 went 151 → 119
    /// ms but video's 216 → 325 ms (frames wait behind the keyframe here
    /// when the target is under what the link carries).
    pub slow_pace: bool,
    /// Growth on a slow path capped at what it carried (`growcap`; see
    /// [`SLOW_PATH_MS`]).
    pub grow_cap: bool,
    /// No gentle cut under [`SLOW_PACE_BELOW_BPS`] (`softslow`): a relay's
    /// own jitter (10-30 ms) tripped the 10 ms rule on nearly every frame
    /// and held a 1.2 Mbps relay at the 0.5 Mbps floor ("soft:5" every 5 s,
    /// no queue on the client). Its real backlogs stand, and the standing
    /// rule cuts those.
    pub soft_slow: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Self {
            standing_queue: true,
            drain: true,
            one_keyframe: true,
            fec_floor: true,
            key_probe: true,
            gap_cut: true,
            slow_pace: false,
            grow_cap: true,
            soft_slow: true,
        }
    }
}

pub struct RateController {
    limits: Limits,
    bitrate: f64,
    fec: f32,
    loss: f32,
    /// Fraction of a frame interval to spread each frame's packets over.
    pace: f32,
    /// (when, the window's lowest delay).
    minima: std::collections::VecDeque<(u64, i64)>,
    hold_until_ms: u64,
    /// When the last decrease was.
    last_cut_ms: Option<u64>,
    /// The rate an idle stream returns to (the starting one, or what the mode
    /// asks for).
    resting: f64,
    /// Bytes sent since the last feedback: encoder payload, and what went on
    /// the wire for it (shard headers and parity included).
    sent_payload: u64,
    sent_wire: u64,
    /// Wire bytes per payload byte, smoothed.
    overhead: f64,
    bulk: f64,
    bulk_busy: bool,
    /// Since when no packet was lost (ms; `None` while losing).
    clean_since_ms: Option<u64>,
    clean: bool,
    /// The last window's use of the budget (bits per second, encoder terms).
    last_used: f64,
    /// When the last window with frames was (for [`PAUSE_MS`]).
    last_frames_ms: Option<u64>,
    /// The standing queue: since when, its size then, and the last cut (when,
    /// its size then).
    standing_since_ms: Option<u64>,
    standing_start_us: i64,
    standing_cut: Option<(u64, i64)>,
    /// The lowest and highest it has been since (µs).
    standing_span_us: (i64, i64),
    switches: Switches,
    /// Drain mode is on (see [`DRAIN_FPS`]), and since when the queue has
    /// been under the standing threshold (ms).
    draining: bool,
    drain_tail_since_ms: Option<u64>,
    /// The last window with frames: its fastest packet's lateness, and its
    /// average (µs).
    queue_min_us: i64,
    last_queue_us: i64,
    /// Frames sent, and frames the client has resolved (decoded or given
    /// up on), as its feedback counts them.
    sent_frames: u64,
    resolved: u64,
    /// The resolved count to wait for the newest keyframe, and until when
    /// at most (ms).
    key_wait: Option<(u64, u64)>,
    /// Keyframe requests collapsed into one in flight (for the log).
    collapsed: u32,
    /// Since when nothing was sent and nothing arrived (ms): then every frame
    /// sent is resolved.
    quiet_since_ms: Option<u64>,
    sent_at_last_update: u64,
    /// The rate the stream sustains, smoothed over windows that used their
    /// budget, and how many there were.
    sustained: f64,
    sustained_n: u32,
    /// A warm start's ceiling for an idle stream's recovery: the highest rate
    /// the path has shown it carries (see [`Self::warm_start`]).
    warm_cap: Option<f64>,
    /// When the keyframe in flight went out (ms), and the bytes delivered
    /// (and ms) since the first arrived after it (see [`KEY_PROBE_MS`]).
    key_sent_ms: u64,
    key_probe: (u64, u64),
    /// The same while a queue stands: what the link delivers while it's
    /// full is its capacity.
    standing_delivered: (u64, u64),
    /// Per feedback window: when, wire bytes sent since the one before,
    /// bytes delivered, and its length (ms) — for [`GAP_MS`].
    gap: std::collections::VecDeque<(u64, u64, u64, u64)>,
    /// Until when the path counts as slow (ms; see [`SLOW_PATH_MS`]), and
    /// per window: when, and what it delivered over the half second before
    /// (bits per second, encoder terms).
    slow_until_ms: u64,
    carried: std::collections::VecDeque<(u64, f64)>,
    /// Cuts since the last [`Self::take_cuts`], by reason (see [`CUT_NAMES`]).
    cuts: [u32; CUT_NAMES.len()],
}

/// The reasons the rate is cut, as the host's perf line names them.
pub const CUT_NAMES: [&str; 7] = ["probe", "gap", "standing", "drain", "loss", "queue", "soft"];
const CUT_PROBE: usize = 0;
const CUT_GAP: usize = 1;
const CUT_STANDING: usize = 2;
const CUT_DRAIN: usize = 3;
const CUT_LOSS: usize = 4;
const CUT_QUEUE: usize = 5;
const CUT_SOFT: usize = 6;

impl RateController {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            bitrate: limits.start_bps as f64,
            fec: 0.1,
            loss: 0.0,
            pace: PACE_MIN,
            minima: Default::default(),
            hold_until_ms: 0,
            last_cut_ms: None,
            resting: limits.start_bps as f64,
            sent_payload: 0,
            sent_wire: 0,
            overhead: 1.15,
            bulk: BULK_START,
            bulk_busy: false,
            clean_since_ms: None,
            clean: false,
            last_used: 0.0,
            last_frames_ms: None,
            standing_since_ms: None,
            standing_start_us: 0,
            standing_cut: None,
            standing_span_us: (0, 0),
            switches: Switches::default(),
            draining: false,
            drain_tail_since_ms: None,
            queue_min_us: 0,
            last_queue_us: 0,
            sent_frames: 0,
            resolved: 0,
            key_wait: None,
            collapsed: 0,
            quiet_since_ms: None,
            sent_at_last_update: 0,
            sustained: 0.0,
            sustained_n: 0,
            warm_cap: None,
            key_sent_ms: 0,
            key_probe: (0, 0),
            standing_delivered: (0, 0),
            gap: Default::default(),
            slow_until_ms: 0,
            carried: Default::default(),
            cuts: Default::default(),
        }
    }

    /// The cuts since the last call, by reason ([`CUT_NAMES`]).
    pub fn take_cuts(&mut self) -> [u32; CUT_NAMES.len()] {
        std::mem::take(&mut self.cuts)
    }

    pub fn set_switches(&mut self, switches: Switches) {
        self.switches = switches;
        if !switches.standing_queue || !switches.drain {
            self.draining = false;
        }
    }

    /// Start at `bps` instead (what this client's path sustained last time,
    /// a little more): the first keyframe at 20 Mbps floods a 1 Mbps relay
    /// for seconds. While nothing pushes back it grows fast
    /// ([`WARM_RAMP`]) toward the resting rate, and an idle stream recovers
    /// only to the highest rate the path has shown it carries.
    pub fn warm_start(&mut self, bps: u32) {
        let bps = (bps as f64).clamp(self.limits.min_bps as f64, self.limits.max_bps as f64);
        if bps < self.bitrate {
            self.bitrate = bps;
            self.warm_cap = Some(bps);
            // (A relay's rate, remembered: slow from the first window on.)
            if bps < SLOW_PACE_BELOW_BPS {
                self.slow_until_ms = u64::MAX;
            }
        }
    }

    /// The rate this stream sustained while it used its budget (bits per
    /// second; `None` until it has for a couple of seconds): what to warm
    /// start the next stream from this client at.
    pub fn sustained_bps(&self) -> Option<u32> {
        (self.sustained_n >= SUSTAINED_MIN_WINDOWS).then_some(self.sustained as u32)
    }

    fn note_sustained(&mut self) {
        self.sustained = if self.sustained_n == 0 { self.bitrate } else { self.sustained * 0.95 + self.bitrate * 0.05 };
        self.sustained_n = self.sustained_n.saturating_add(1);
    }

    /// Drain mode: the frame rate to encode at while a standing queue drains
    /// (`None`: normal). Skipping frames *before* they're encoded keeps every
    /// reference valid; the bitrate cut alone left the backlog draining at a
    /// fifth of the link's rate, seconds of latency for seconds.
    ///
    /// Likewise while a keyframe is on its way slowly (see
    /// [`KEY_PROBE_MS`]): the client shows nothing before it lands, and every
    /// frame sent meanwhile only queues behind it.
    pub fn drain_fps(&self, now_ms: u64) -> Option<u32> {
        let draining = self.draining && self.switches.drain && self.switches.standing_queue;
        let probing = self.switches.key_probe && self.key_in_flight(now_ms) && now_ms.saturating_sub(self.key_sent_ms) >= KEY_PROBE_MS;
        (draining || probing).then_some(DRAIN_FPS)
    }

    /// The fastest the sender may put a frame's packets out (bits per
    /// second; `None`: its usual 2.5x the target or more, for the lowest
    /// latency on a clean path). On a slow or queued link a burst at 2.5x
    /// only moves the queue from the host into the relay — where it sits in
    /// front of the audio: a 36 KB keyframe at 1 Mbps was ~290 ms of video
    /// ahead of every audio packet. At 1.2x the target, the frame waits here
    /// instead, and audio and input acks slot in between its packets.
    pub fn pace_cap_bps(&self) -> Option<u32> {
        let constrained = self.draining || self.standing_since_ms.is_some() || self.bitrate < SLOW_PACE_BELOW_BPS;
        (self.switches.slow_pace && constrained).then_some((self.bitrate * 1.2) as u32)
    }

    /// Over the last [`GAP_MS`]: bytes delivered per byte sent, and the rate
    /// delivered (encoder terms). `None` until there's that much to go by,
    /// or while the stream sends next to nothing.
    fn delivery_gap(&self) -> Option<(f64, f64)> {
        let (sent, got, ms) = self.gap.iter().fold((0u64, 0u64, 0u64), |(s, g, m), e| (s + e.1, g + e.2, m + e.3));
        let sent_bps = sent as f64 * 8.0 * 1000.0 / ms.max(1) as f64;
        if ms < GAP_MS * 4 / 5 || got == 0 || sent_bps < self.limits.min_bps as f64 {
            return None;
        }
        Some((got as f64 / sent as f64, got as f64 * 8.0 * 1000.0 / ms as f64 / self.overhead))
    }

    /// Whether the newest keyframe has yet to reach the client (or be given
    /// up on there), as far as its feedback says.
    fn key_in_flight(&self, now_ms: u64) -> bool {
        self.key_wait.is_some_and(|(need, until)| self.resolved < need && now_ms < until)
    }

    /// Keyframe `frame_id` (the stream's frames count from 0), `bytes` long,
    /// went out.
    pub fn note_keyframe(&mut self, frame_id: u32, bytes: usize, now_ms: u64) {
        let transmit_ms = bytes as f64 * 8.0 * 1000.0 / self.bitrate.max(1.0);
        let queue_ms = self.queue_min_us.max(0) as f64 / 1000.0;
        // The stream's first: nothing is known about the path yet (and no
        // count can be off yet), so it's waited for as long as it takes.
        let wait = if self.resolved == 0 {
            KEY_WAIT_MAX_MS
        } else {
            (KEY_WAIT_BASE_MS + 2.0 * transmit_ms + queue_ms).clamp(KEY_WAIT_MIN_MS, KEY_WAIT_MAX_MS)
        };
        self.key_wait = Some((frame_id as u64 + 1, now_ms + wait as u64));
        (self.key_sent_ms, self.key_probe) = (now_ms, (0, 0));
    }

    /// The client asks for a keyframe (or a repair only a keyframe can give)
    /// while the last one hasn't reached it yet: true if the request is
    /// collapsed into that one. Its requests repeat every ~100 ms until a
    /// keyframe decodes, and on a relay a 300 KB keyframe takes seconds: each
    /// answered was another 300 KB queued behind the first (the host's log:
    /// three in 400 ms). Once the client has the keyframe — or gave up on
    /// it (lost) — a new request is answered at once.
    pub fn collapse_keyframe_request(&mut self, now_ms: u64) -> bool {
        let waiting = self.switches.one_keyframe && self.key_in_flight(now_ms);
        if waiting {
            self.collapsed += 1;
        }
        waiting
    }

    /// Keyframe requests collapsed since the last call.
    pub fn take_collapsed(&mut self) -> u32 {
        std::mem::take(&mut self.collapsed)
    }

    /// The parity ratio for one frame. On a path that has lost nothing for a
    /// while, a small non-key frame (office trickling, a caret, a small
    /// change) goes without: its single parity shard would be a copy of it —
    /// twice the packets, twice the radio time at both ends — and the rare
    /// loss is repaired from a long-term reference instead. Big non-key
    /// frames carry less there ([`CLEAN_FEC`]); keyframes always carry the
    /// full ratio. The first loss brings it back for all.
    pub fn parity_for(&self, bytes: usize, keyframe: bool) -> f32 {
        if self.clean && !keyframe && bytes <= PARITY_FREE_BYTES {
            0.0
        } else if self.clean && !keyframe && self.switches.fec_floor {
            self.fec.min(CLEAN_FEC)
        } else {
            self.fec
        }
    }

    /// Account one sent frame: its encoded size and the packet bodies it
    /// took (so delivered bytes can be turned back into encoder terms).
    pub fn note_sent(&mut self, payload: usize, wire: usize) {
        self.sent_payload += payload as u64;
        self.sent_wire += wire as u64;
        self.sent_frames += 1;
    }

    /// The rate bulk transfers may use now (bits per second).
    pub fn bulk_bps(&self) -> u64 {
        self.bulk as u64
    }

    /// Whether bulk data is waiting to go (only then does its rate grow).
    pub fn set_bulk_busy(&mut self, busy: bool) {
        self.bulk_busy = busy;
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate as u32
    }

    /// The rate an idle stream drifts back to (office mode wants more).
    pub fn set_resting(&mut self, bps: u32) {
        self.resting = (bps as f64).clamp(self.limits.min_bps as f64, self.limits.max_bps as f64);
    }

    /// Parity ratio for the next frames.
    pub fn fec(&self) -> f32 {
        self.fec
    }

    /// Fraction of a frame interval to spread a frame's packets over. Tight
    /// (lowest latency) while the path is clean; it widens only when loss
    /// shows a radio or router queue can't take the bursts.
    pub fn pace(&self) -> f32 {
        self.pace
    }

    /// Fold one feedback window (`window_ms` long) in.
    pub fn update(&mut self, fb: &Feedback, window_ms: u64, now_ms: u64) {
        if self.sent_payload > 0 {
            let ratio = (self.sent_wire as f64 / self.sent_payload as f64).clamp(1.0, 3.0);
            self.overhead = self.overhead * 0.8 + ratio * 0.2;
        }
        // Sent and delivered over the last half second (see [`GAP_MS`]).
        self.gap.push_back((now_ms, self.sent_wire, fb.bytes, window_ms));
        while self.gap.front().is_some_and(|&(t, ..)| now_ms.saturating_sub(t) > GAP_MS) {
            self.gap.pop_front();
        }
        let (bytes, ms) = self.gap.iter().fold((0, 0), |(b, m), &(_, _, d, w)| (b + d, m + w));
        if ms >= GAP_MS / 2 {
            self.carried.push_back((now_ms, bytes as f64 * 8.0 * 1000.0 / ms as f64 / self.overhead));
        }
        while self.carried.front().is_some_and(|&(t, _)| now_ms.saturating_sub(t) > CARRIED_MS) {
            self.carried.pop_front();
        }
        (self.sent_payload, self.sent_wire) = (0, 0);
        if self.slow_until_ms == u64::MAX {
            self.slow_until_ms = now_ms + SLOW_PATH_MS;
        }
        // Frames the client resolved. A lost feedback window leaves the count
        // short for good, so a quiet second (nothing sent, nothing arriving)
        // squares it: everything sent has been resolved by then.
        self.resolved += (fb.frames + fb.frames_lost) as u64;
        if fb.frames == 0 && fb.frames_lost == 0 && self.sent_frames == self.sent_at_last_update {
            let since = *self.quiet_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= 1_000 {
                self.resolved = self.resolved.max(self.sent_frames);
            }
        } else {
            self.quiet_since_ms = None;
        }
        self.sent_at_last_update = self.sent_frames;
        // Loss: rebuilt data shards are packets the network dropped.
        let total = fb.shards + fb.recovered;
        let mut raw = 0.0;
        if total > 0 {
            raw = fb.recovered as f32 / total as f32;
            self.loss = self.loss * 0.85 + raw * 0.15;
        }
        if raw > 0.01 || fb.frames_lost > 0 {
            self.pace = (self.pace * 1.5).min(PACE_MAX);
        } else if self.loss < 0.002 {
            self.pace = (self.pace * 0.97).max(PACE_MIN);
        }
        self.fec = (0.05 + self.loss * 3.0).clamp(0.05, 0.5);
        if fb.frames_lost > 0 {
            self.fec = self.fec.max(0.25);
        }
        if raw > 0.0 || fb.frames_lost > 0 {
            self.clean_since_ms = None;
        } else if self.clean_since_ms.is_none() {
            self.clean_since_ms = Some(now_ms);
        }
        self.clean = self.clean_since_ms.is_some_and(|t| now_ms.saturating_sub(t) >= CLEAN_AFTER_MS);

        let mut queue_min_us = 0;
        let queue_us = if fb.frames > 0 {
            if self.last_frames_ms.is_some_and(|t| now_ms.saturating_sub(t) > PAUSE_MS) {
                self.minima.clear();
            }
            self.last_frames_ms = Some(now_ms);
            while self.minima.len() >= BASELINE_WINDOWS
                || self.minima.front().is_some_and(|&(t, _)| now_ms.saturating_sub(t) > BASELINE_MS)
            {
                self.minima.pop_front();
            }
            self.minima.push_back((now_ms, fb.delay_min_us));
            let base = self.minima.iter().map(|&(_, d)| d).min().unwrap();
            queue_min_us = fb.delay_min_us - base;
            fb.delay_avg_us - base
        } else {
            0
        };

        // Bulk yields first: it halves at the first sign of queueing or a
        // lost frame, and grows only while something is waiting to go.
        if fb.frames_lost > 0 || queue_us > QUEUE_SOFT_US / 2 {
            self.bulk = (self.bulk * 0.5).max(BULK_MIN);
        } else if self.bulk_busy {
            self.bulk = (self.bulk * 1.15).min(BULK_MAX);
        }

        // In encoder terms (parity isn't the encoder using its budget).
        let used = fb.bytes as f64 * 8.0 * 1000.0 / window_ms.max(1) as f64 / self.overhead;
        // Using a real share of the budget, now or in the window before (a
        // big frame's tail still draining shows up as the next one's queue):
        // only then can a queue be ours.
        let busy = used.max(self.last_used) >= self.bitrate * 0.5;
        self.last_used = used;
        // Queueing while the stream barely sends is the radio's jitter (Wi-Fi
        // power save holds packets up to a beacon interval), not our
        // congestion: cutting for it, however long, ratcheted an office
        // stream down to 1 Mbps with nothing lost, and starved the next page
        // of text. A lost frame counts whatever we sent.
        // A standing queue is ours however little we send: the link carries
        // less than that. Cut below what gets through, until it drains.
        let delivered = fb.bytes as f64 * 8.0 * 1000.0 / window_ms.max(1) as f64 / self.overhead;
        let rate_of = |(bytes, ms): (u64, u64), overhead: f64| bytes as f64 * 8.0 * 1000.0 / ms.max(1) as f64 / overhead;
        // A keyframe slow to arrive: what the link delivers meanwhile (from the
        // first bytes on) is what it carries.
        if self.switches.key_probe && self.key_in_flight(now_ms) {
            if fb.bytes > 0 || self.key_probe.1 > 0 {
                self.key_probe.0 += fb.bytes;
                self.key_probe.1 += window_ms;
            }
            if now_ms.saturating_sub(self.key_sent_ms) >= KEY_PROBE_MS && self.key_probe.1 >= 100 {
                let link = rate_of(self.key_probe, self.overhead) * 0.85;
                if link < self.bitrate {
                    self.bitrate = link.max(self.limits.min_bps as f64);
                    self.hold_until_ms = now_ms + HOLD_MS;
                    self.last_cut_ms = Some(now_ms);
                    self.slow_until_ms = now_ms + SLOW_PATH_MS;
                    self.cuts[CUT_PROBE] += 1;
                }
            }
        }
        // Far more sent than delivered, for half a second: the link carries
        // less than we send, whatever the delay says yet (on a relay, the
        // delay of the frames that tell is seconds away).
        let gap = self.delivery_gap();
        if let Some((_, link)) = gap.filter(|&(ratio, _)| self.switches.gap_cut && ratio < GAP_CUT) {
            if link * 0.85 < self.bitrate {
                self.bitrate = (link * 0.85).max(self.limits.min_bps as f64);
                self.hold_until_ms = now_ms + HOLD_MS;
                self.last_cut_ms = Some(now_ms);
                self.slow_until_ms = now_ms + SLOW_PATH_MS;
                self.cuts[CUT_GAP] += 1;
                self.bulk = BULK_MIN;
                // (Once: the next check needs a fresh half second.)
                self.gap.clear();
            }
        }
        if self.standing_since_ms.is_some() || self.draining {
            self.standing_delivered.0 += fb.bytes;
            self.standing_delivered.1 += window_ms;
        }
        let mut standing_cut = false;
        // What the link delivered while the queue stood, since the last cut.
        let mut link_bps = None;
        if fb.frames > 0 {
            (self.queue_min_us, self.last_queue_us) = (queue_min_us, queue_us);
        }
        if self.switches.standing_queue && fb.frames > 0 && queue_min_us > STANDING_US {
            let since = *self.standing_since_ms.get_or_insert_with(|| {
                self.standing_start_us = queue_min_us;
                self.standing_span_us = (queue_min_us, queue_min_us);
                now_ms
            });
            self.drain_tail_since_ms = None;
            let shrinking = self.standing_cut.is_some_and(|(_, q)| queue_min_us < q * 9 / 10);
            let due = self.standing_cut.is_none_or(|(t, _)| now_ms.saturating_sub(t) >= STANDING_EVERY_MS);
            self.standing_span_us = (self.standing_span_us.0.min(queue_min_us), self.standing_span_us.1.max(queue_min_us));
            let (lo, hi) = self.standing_span_us;
            let flat = hi - lo <= (self.standing_start_us / 5).max(STANDING_FLAT_US);
            if now_ms.saturating_sub(since) >= STANDING_REBASE_MS && queue_min_us >= self.standing_start_us * 9 / 10 && flat {
                // Never drained, never grew: the path is longer now, not
                // queued. (A queue still growing after 15 s is a backlog
                // the cuts haven't caught up with, not a path.)
                self.minima.clear();
                self.minima.push_back((now_ms, fb.delay_min_us));
                (self.standing_since_ms, self.standing_cut) = (None, None);
                (self.draining, self.standing_delivered) = (false, (0, 0));
            } else if now_ms.saturating_sub(since) >= STANDING_MS {
                // It stands: drain it (fewer frames until it's gone).
                self.draining = self.switches.drain;
                if due {
                    self.standing_cut = Some((now_ms, queue_min_us));
                    standing_cut = !shrinking;
                    // (Over the last second: a window at a few frames a
                    // second may hold none of the queue's bytes.)
                    let acc = std::mem::take(&mut self.standing_delivered);
                    link_bps = Some(if acc.1 >= 200 { rate_of(acc, self.overhead) } else { delivered });
                }
            }
        } else if fb.frames > 0 {
            (self.standing_since_ms, self.standing_cut) = (None, None);
            // Under the standing threshold: drained once it's near the
            // baseline — or after a while there anyway (a path a little
            // longer than before, not a queue).
            let near = *self.drain_tail_since_ms.get_or_insert(now_ms);
            if queue_min_us < DRAIN_END_US || !self.switches.standing_queue || now_ms.saturating_sub(near) >= DRAIN_TAIL_MS {
                self.draining = false;
            }
            if !self.draining {
                self.standing_delivered = (0, 0);
            }
        }
        if !standing_cut && self.draining {
            // Draining: no cut needed to drain it, but the rate to come back
            // to is what the link carried meanwhile (it was full: that's its
            // capacity) — or the queue builds again as soon as drain mode ends.
            if let Some(link) = link_bps {
                if link * 0.85 < self.bitrate {
                    self.cuts[CUT_DRAIN] += 1;
                }
                self.bitrate = self.bitrate.min(link * 0.85).max(self.limits.min_bps as f64);
            }
        }
        if standing_cut {
            let link = link_bps.unwrap_or(delivered);
            self.bitrate = (self.bitrate * 0.75).min(link * 0.85);
            self.hold_until_ms = now_ms + HOLD_MS;
            self.last_cut_ms = Some(now_ms);
            self.slow_until_ms = now_ms + SLOW_PATH_MS;
            self.cuts[CUT_STANDING] += 1;
            self.bulk = BULK_MIN;
            self.bitrate = self.bitrate.clamp(self.limits.min_bps as f64, self.limits.max_bps as f64);
            self.note_sustained();
            return;
        }
        let decrease = if fb.frames_lost > 0 {
            Some((0.75, CUT_LOSS))
        } else if queue_us > QUEUE_HARD_US && busy {
            Some((0.75, CUT_QUEUE))
        } else if queue_us > QUEUE_SOFT_US && busy && !(self.switches.soft_slow && self.bitrate < SLOW_PACE_BELOW_BPS) {
            Some((0.93, CUT_SOFT))
        } else {
            None
        };
        if let Some((f, why)) = decrease {
            self.cuts[why] += 1;
            self.bitrate *= f;
            // A lost frame alone (random Wi-Fi loss) shouldn't cut below what
            // demonstrably gets through — in encoder terms: delivered bytes
            // include shard headers and parity. A growing queue gets no
            // floor: it has to drain.
            if queue_us <= QUEUE_SOFT_US {
                self.bitrate = self.bitrate.max(delivered * 0.85).min(self.bitrate / f);
            }
            self.hold_until_ms = now_ms + HOLD_MS;
            self.last_cut_ms = Some(now_ms);
        } else if now_ms >= self.hold_until_ms {
            // A warm start grows fast until something first pushes back.
            let warm = self.warm_cap.is_some() && self.last_cut_ms.is_none() && self.bitrate < self.resting;
            // (A warm start's idle stream recovers only as far as the path
            // has shown it carries.)
            let resting = self.warm_cap.map_or(self.resting, |c| c.min(self.resting));
            // Never into a queue. A window with no frame done but bytes
            // arriving has a frame crawling over a slow link: no delay to
            // read, so the last one counts. (A link carrying less than we
            // send shows as an idle stream too — little delivered — and
            // recovering in those windows grew 9 → 13 Mbps into a 0.8 Mbps
            // relay before the standing-queue rule could act.)
            let calm = if fb.frames > 0 {
                queue_us <= QUEUE_SOFT_US
            } else {
                fb.bytes == 0 || self.last_queue_us <= QUEUE_SOFT_US
            } && !(self.switches.gap_cut && gap.is_some_and(|(ratio, _)| ratio < GAP_GROW));
            // On a slow path, never past what it has shown it carries (with
            // headroom), however clean an idle stream looks.
            let ceiling = if self.switches.grow_cap && now_ms < self.slow_until_ms {
                // (This window too: a path keeping up as the rate ramps
                // shows it before the half second does.)
                let carried = self.carried.iter().map(|&(_, b)| b).fold(used, f64::max);
                (carried * GROW_HEADROOM).max(self.limits.min_bps as f64).max(self.bitrate)
            } else {
                f64::INFINITY
            };
            if busy && calm {
                self.bitrate = (self.bitrate * if warm { WARM_RAMP } else { 1.02 }).min(ceiling);
                if let Some(c) = &mut self.warm_cap {
                    *c = c.max(self.bitrate);
                }
            } else if !busy
                && calm
                && self.bitrate < resting
                && self.last_cut_ms.is_none_or(|t| now_ms.saturating_sub(t) >= RECOVER_AFTER_MS)
            {
                // Idle and clean for a while: back toward the resting rate.
                self.bitrate = (self.bitrate * 1.02).min(resting).min(ceiling);
            }
        }
        self.bitrate = self.bitrate.clamp(self.limits.min_bps as f64, self.limits.max_bps as f64);
        if busy && fb.frames > 0 {
            self.note_sustained();
        }
    }
}

/// Bits per pixel per frame under which the frame rate steps down, and over
/// which (at the next rate up) it steps back. At 1 Mbps a 2250x1590 stream
/// at 60 fps has 0.005: every frame a smear, and 60 frames' packets a second
/// for it. Fewer frames get the same bits each a sharper picture.
const BPP_LOW: f64 = 0.015;
const BPP_HIGH: f64 = 0.025;
/// The rates stepped through: the full one divided by these…
const FPS_DIVISORS: [u32; 5] = [1, 2, 3, 4, 6];
/// …never below this…
const FPS_FLOOR: u32 = 20;
/// …or this in office mode (see [`FrameRate::follow_office`]).
const OFFICE_FPS_FLOOR: u32 = 10;
/// How long a step's condition has to hold (ms): down soon (the frames are
/// smeared now), up only once the rate has held.
const FPS_DOWN_AFTER_MS: u64 = 1_000;
const FPS_UP_AFTER_MS: u64 = 3_000;

/// The frame rate follows the bitrate (see [`BPP_LOW`]): 60 → 30 → 20 as it
/// falls, back up as it rises.
#[derive(Debug, Default)]
pub struct FrameRate {
    /// Index into [`FPS_DIVISORS`].
    step: usize,
    /// Since when a step down / up has been due.
    low_since_ms: Option<u64>,
    high_since_ms: Option<u64>,
}

impl FrameRate {
    /// The rate to encode at now, for `bps` over `pixels` a frame and up to
    /// `full` frames a second.
    pub fn follow(&mut self, bps: u32, pixels: u64, full: u32, now_ms: u64) -> u32 {
        self.follow_floor(bps, pixels, full, now_ms, FPS_FLOOR)
    }

    /// The same for office mode: down to 10 fps. Text on a 1 Mbps relay at
    /// 20 fps was quick but soft next to the Windows App's slower, crisper
    /// pictures; at 10 each frame gets twice the bits, and input still sends
    /// the next picture at once (the host's credit).
    pub fn follow_office(&mut self, bps: u32, pixels: u64, full: u32, now_ms: u64) -> u32 {
        self.follow_floor(bps, pixels, full, now_ms, OFFICE_FPS_FLOOR)
    }

    fn follow_floor(&mut self, bps: u32, pixels: u64, full: u32, now_ms: u64, floor: u32) -> u32 {
        let full = full.max(1);
        let rate = |step: usize| full / FPS_DIVISORS[step];
        let usable = |step: usize| step == 0 || (step < FPS_DIVISORS.len() && rate(step) >= floor);
        // (The full rate changed under us: the nearest step still allowed.)
        while !usable(self.step) {
            self.step -= 1;
        }
        let bpp = |fps: u32| bps as f64 / (pixels.max(1) as f64 * fps.max(1) as f64);
        if usable(self.step + 1) && bpp(rate(self.step)) < BPP_LOW {
            self.high_since_ms = None;
            if now_ms.saturating_sub(*self.low_since_ms.get_or_insert(now_ms)) >= FPS_DOWN_AFTER_MS {
                self.step += 1;
                self.low_since_ms = None;
            }
        } else if self.step > 0 && bpp(rate(self.step - 1)) >= BPP_HIGH {
            self.low_since_ms = None;
            if now_ms.saturating_sub(*self.high_since_ms.get_or_insert(now_ms)) >= FPS_UP_AFTER_MS {
                self.step -= 1;
                self.high_since_ms = None;
            }
        } else {
            (self.low_since_ms, self.high_since_ms) = (None, None);
        }
        rate(self.step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(bytes_per_window: u64, delay_avg: i64, frames_lost: u32, recovered: u32) -> Feedback {
        Feedback {
            ltr_frame: None,
            shards: 100,
            recovered,
            frames: 6,
            frames_lost,
            bytes: bytes_per_window,
            delay_min_us: 5_000,
            delay_avg_us: delay_avg,
            window_us: 50_000,
        }
    }

    #[test]
    fn small_frames_skip_parity_only_on_a_clean_path() {
        let mut r = RateController::new(Limits::default());
        r.update(&fb(10_000, 5_000, 0, 0), 50, 0);
        assert!(r.parity_for(1000, false) > 0.0, "not clean yet");
        r.update(&fb(10_000, 5_000, 0, 0), 50, CLEAN_AFTER_MS);
        assert_eq!(r.parity_for(1000, false), 0.0);
        assert!(r.parity_for(1000, true) > 0.0, "keyframes keep parity");
        assert!(r.parity_for(PARITY_FREE_BYTES + 1, false) > 0.0, "big frames keep parity");
        r.update(&fb(10_000, 5_000, 0, 1), 50, CLEAN_AFTER_MS + 50);
        assert!(r.parity_for(1000, false) > 0.0, "a loss brings parity back");
    }

    #[test]
    fn grows_while_the_budget_is_used_and_the_path_is_clear() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..50 {
            let used = r.bitrate_bps() as u64 / 8 / 20; // all of a 50 ms budget
            r.update(&fb(used, 5_500, 0, 0), 50, t * 50);
        }
        assert!(r.bitrate_bps() > start);
    }

    #[test]
    fn an_idle_desktop_does_not_earn_bitrate() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..50 {
            r.update(&fb(1_000, 5_500, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), start);
    }

    #[test]
    fn queueing_delay_backs_off_before_loss() {
        let mut r = RateController::new(Limits::default());
        let full = r.bitrate_bps() as u64 / 8 / 20;
        r.update(&fb(full, 5_000, 0, 0), 50, 0);
        let before = r.bitrate_bps();
        r.update(&fb(full, 5_000 + 40_000, 0, 0), 50, 50);
        assert!((r.bitrate_bps() as f64) <= before as f64 * 0.76);
    }

    /// The tablet's log: an office stream sending a sliver of its budget, the
    /// tablet's Wi-Fi dozing (delay bumps of 30-100 ms), nothing lost. It used
    /// to sit at 1 Mbps; it keeps its rate.
    #[test]
    fn long_delay_bumps_on_an_idle_stream_are_not_congestion() {
        let mut r = RateController::new(Limits { start_bps: 60_000_000, ..Limits::default() });
        for t in 0..600 {
            let bump = if t % 3 == 0 { 5_000 + 30_000 + (t % 7) * 10_000 } else { 5_500 };
            r.update(&fb(2_000, bump as i64, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), 60_000_000);
    }

    /// A big frame (a page of new text) fills a window; the next window, with
    /// little sent, sees the queue it left draining. That queue is ours.
    #[test]
    fn the_queue_a_burst_leaves_behind_backs_off() {
        let mut r = RateController::new(Limits::default());
        let before = r.bitrate_bps();
        r.update(&fb(before as u64 / 8 / 20 * 3, 5_000, 0, 0), 50, 0);
        r.update(&fb(2_000, 5_000 + 60_000, 0, 0), 50, 50);
        assert!(r.bitrate_bps() < before);
    }

    #[test]
    fn loss_raises_fec_and_a_lost_frame_cuts_rate() {
        let mut r = RateController::new(Limits::default());
        for t in 0..20 {
            r.update(&fb(10_000, 5_000, 0, 8), 50, t * 50);
        }
        assert!(r.fec() > 0.15, "fec {}", r.fec());
        let before = r.bitrate_bps();
        r.update(&fb(10_000, 5_000, 1, 8), 50, 2_000);
        assert!(r.bitrate_bps() < before);
    }

    #[test]
    fn pacing_widens_under_loss_and_tightens_when_clean() {
        let mut r = RateController::new(Limits::default());
        let tight = r.pace();
        for t in 0..10 {
            r.update(&fb(10_000, 5_000, 0, 8), 50, t * 50);
        }
        assert!(r.pace() > tight * 2.0, "pace {}", r.pace());
        for t in 10..400 {
            r.update(&fb(10_000, 5_000, 0, 0), 50, t * 50);
        }
        assert!((r.pace() - tight).abs() < 1e-3, "pace {}", r.pace());
    }

    #[test]
    fn a_lost_frame_cuts_in_encoder_terms_despite_heavy_fec() {
        let mut r = RateController::new(Limits::default());
        let before = r.bitrate_bps() as f64;
        // Everything sent gets through, but half of it is parity and headers
        // (an idle budget, so the bitrate holds still meanwhile).
        let payload = (before / 8.0 / 20.0 / 4.0) as usize;
        for t in 0..30 {
            r.note_sent(payload, payload * 2);
            r.update(&fb(payload as u64 * 2, 5_000, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps() as f64, before);
        // Now a full budget, and a frame is lost.
        let payload = (before / 8.0 / 20.0) as usize;
        r.note_sent(payload, payload * 2);
        r.update(&fb(payload as u64 * 2, 5_000, 1, 0), 50, 1_500);
        assert!((r.bitrate_bps() as f64) < before * 0.9, "{} vs {before}", r.bitrate_bps());
    }

    #[test]
    fn a_growing_queue_is_not_floored_by_delivered_bytes() {
        let mut r = RateController::new(Limits::default());
        r.update(&fb(10_000, 5_000, 0, 0), 50, 0);
        let before = r.bitrate_bps() as f64;
        let delivered = (before / 8.0 / 20.0) as u64; // the whole budget arrives…
        r.update(&fb(delivered, 5_000 + 40_000, 0, 0), 50, 50); // …but queued
        assert!((r.bitrate_bps() as f64) <= before * 0.76);
    }

    #[test]
    fn bulk_yields_to_queueing_and_grows_only_while_busy() {
        let mut r = RateController::new(Limits::default());
        let start = r.bulk_bps();
        for t in 0..20 {
            r.update(&fb(10_000, 5_000, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bulk_bps(), start, "idle bulk doesn't grow");
        r.set_bulk_busy(true);
        for t in 20..40 {
            r.update(&fb(10_000, 5_000, 0, 0), 50, t * 50);
        }
        let grown = r.bulk_bps();
        assert!(grown > start * 4, "{grown}");
        r.update(&fb(10_000, 5_000 + 8_000, 0, 0), 50, 2_000);
        assert!(r.bulk_bps() <= grown / 2 + 1, "queueing halves bulk");
    }

    #[test]
    fn clock_drift_over_a_pause_is_not_queueing() {
        let mut r = RateController::new(Limits::default());
        for t in 0..40 {
            r.update(&fb(10_000, 5_000, 0, 0), 50, t * 50);
        }
        let before = r.bitrate_bps();
        // Thirty minutes without frames; meanwhile the clocks drift 15 ms
        // (every delay reads 15 ms higher, the path itself unchanged).
        let resume = 30 * 60 * 1000;
        let drifted = Feedback { delay_min_us: 20_000, ..fb(10_000, 20_500, 0, 0) };
        for t in 0..20 {
            r.update(&drifted, 50, resume + t * 50);
        }
        assert!(r.bitrate_bps() >= before, "{} after the pause (was {before})", r.bitrate_bps());
    }

    /// The live log's pattern: a mostly still screen (a sliver of the budget
    /// used) with Wi-Fi delay bumps now and then. It used to ratchet 17 → 7
    /// Mbps; jitter on an idle stream is not congestion.
    #[test]
    fn jitter_on_an_idle_stream_does_not_ratchet_it_down() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..600 {
            let bump = if t % 40 == 0 { 5_000 + 15_000 } else { 5_500 };
            r.update(&fb(3_000, bump, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), start);
    }

    /// Cut by a burst of real trouble, then idle and clean: back to the
    /// resting rate within seconds, not stuck low for the next motion.
    #[test]
    fn an_idle_stream_recovers_its_resting_rate() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..4 {
            r.update(&fb(3_000, 5_000, 1, 0), 50, t * 50);
        }
        let cut = r.bitrate_bps();
        assert!(cut < start / 2, "{cut}");
        // Not straight away (real trouble may still be there)…
        for t in 4..60 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), cut);
        // …but after a clean spell.
        for t in 60..400 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), start);
        // Office mode rests higher.
        r.set_resting(30_000_000);
        for t in 400..800 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), 30_000_000);
    }

    /// While the stream really uses its budget, a modest queue still backs
    /// it off (that queue is ours).
    #[test]
    fn a_queue_under_load_still_backs_off() {
        let mut r = RateController::new(Limits::default());
        let before = r.bitrate_bps();
        let full = before as u64 / 8 / 20;
        r.update(&fb(full, 5_000, 0, 0), 50, 0);
        r.update(&fb(full, 5_000 + 15_000, 0, 0), 50, 50);
        assert!(r.bitrate_bps() < before);
    }

    /// The tablet over Tailscale's relay (DERP, TCP): nothing lost, a
    /// sliver of the budget sent, and frames arriving seconds late. The
    /// baseline used to swallow the backlog within 5 s and the idle-stream
    /// rule ignored it; the bitrate climbed back to 20 Mbps.
    #[test]
    fn a_standing_queue_on_a_quiet_stream_backs_off_until_it_drains() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps() as f64;
        for t in 0..20 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        // A backlog builds and stands (every packet late), 1 Mbps arriving.
        let mut t = 20;
        for i in 0..200i64 {
            let q = 5_000 + (300_000 + i * 20_000).min(3_000_000);
            let f = Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) };
            r.update(&f, 50, t * 50);
            t += 1;
        }
        let cut = r.bitrate_bps() as f64;
        assert!(cut < 1_000_000.0, "{cut} (was {start})");
        // It drains: no more cuts, and once clear, back up.
        for i in 0..60i64 {
            let q = 5_000 + (3_000_000 - i * 60_000).max(0);
            let f = Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) };
            r.update(&f, 50, t * 50);
            t += 1;
        }
        assert!(r.bitrate_bps() as f64 >= cut);
        // Idle for 20 s: no further than twice what the path carried (a
        // scroll at 20 Mbps would queue for seconds again)…
        for _ in 0..400 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
            t += 1;
        }
        assert!((r.bitrate_bps() as f64) < 2_500_000.0, "{}", r.bitrate_bps());
        // …and once the path has been quiet for a minute, back up.
        for _ in 0..1_200 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
            t += 1;
        }
        assert_eq!(r.bitrate_bps() as f64, start);
        // Switched off: back up within the 20 s.
        let mut r = RateController::new(Limits::default());
        r.set_switches(Switches { grow_cap: false, ..Switches::default() });
        let mut t = 0;
        for i in 0..680i64 {
            let q = if (20..220).contains(&i) { 5_000 + (300_000 + (i - 20) * 20_000).min(3_000_000) } else { 5_000 };
            let f = Feedback { delay_min_us: q, ..fb(if i < 20 || i >= 280 { 3_000 } else { 6_250 }, q + 2_000, 0, 0) };
            r.update(&f, 50, t * 50);
            t += 1;
        }
        assert_eq!(r.bitrate_bps() as f64, start);
    }

    /// The route got longer (a relay took over from a direct path): every
    /// packet 160 ms later than before, for good. Not a queue: after a while
    /// it's the baseline and the rate comes back.
    #[test]
    fn a_longer_path_becomes_the_baseline() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..20 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        let far = Feedback { delay_min_us: 5_000 + 160_000, ..fb(3_000, 5_500 + 160_000, 0, 0) };
        for t in 20..2_000 {
            r.update(&far, 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), start);
    }

    /// Wi-Fi power save holding a whole window now and then (the fastest
    /// packet late too), never half a second running: not a standing queue.
    #[test]
    fn a_dozing_radio_is_not_a_standing_queue() {
        let mut r = RateController::new(Limits::default());
        let start = r.bitrate_bps();
        for t in 0..600 {
            let q = if t % 5 == 0 { 5_000 + 250_000 } else { 5_000 };
            r.update(&Feedback { delay_min_us: q, ..fb(3_000, q + 500, 0, 0) }, 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), start);
    }

    #[test]
    fn stays_within_limits() {
        let limits = Limits { min_bps: 2_000_000, start_bps: 3_000_000, max_bps: 4_000_000 };
        let mut r = RateController::new(limits);
        for t in 0..200 {
            r.update(&fb(10_000, 5_000, 1, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), 2_000_000);
    }

    /// A standing queue turns drain mode on; once the queue is gone it's off.
    /// Switched off, never.
    #[test]
    fn a_standing_queue_drains_at_a_few_frames_a_second() {
        for drain in [true, false] {
            let mut r = RateController::new(Limits::default());
            r.set_switches(Switches { drain, ..Switches::default() });
            for t in 0..20 {
                r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
            }
            assert_eq!(r.drain_fps(0), None);
            let mut t = 20;
            for _ in 0..20 {
                let q = 5_000 + 400_000;
                r.update(&Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) }, 50, t * 50);
                t += 1;
            }
            assert_eq!(r.drain_fps(0), drain.then_some(DRAIN_FPS));
            // Draining: still on while it's above the end mark…
            let q = 5_000 + 80_000;
            r.update(&Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) }, 50, t * 50);
            t += 1;
            assert_eq!(r.drain_fps(0), drain.then_some(DRAIN_FPS));
            // …off once it's gone.
            r.update(&fb(6_250, 5_500, 0, 0), 50, t * 50);
            assert_eq!(r.drain_fps(0), None);
        }
    }

    /// A path 70 ms longer than before (under the standing threshold, over
    /// drain mode's end mark) doesn't keep the stream at a few frames.
    #[test]
    fn drain_mode_ends_on_a_slightly_longer_path() {
        let mut r = RateController::new(Limits::default());
        for t in 0..20 {
            r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
        }
        let mut t = 20;
        for _ in 0..20 {
            let q = 5_000 + 400_000;
            r.update(&Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) }, 50, t * 50);
            t += 1;
        }
        assert!(r.drain_fps(0).is_some());
        for _ in 0..(DRAIN_TAIL_MS / 50 + 1) {
            let q = 5_000 + 70_000;
            r.update(&Feedback { delay_min_us: q, ..fb(6_250, q + 2_000, 0, 0) }, 50, t * 50);
            t += 1;
        }
        assert_eq!(r.drain_fps(0), None);
    }

    /// Requests while a keyframe is on its way are collapsed into it; once
    /// the client's counts show it arrived (or was lost), or it's overdue,
    /// a request is answered.
    #[test]
    fn one_keyframe_in_flight() {
        let mut r = RateController::new(Limits { start_bps: 1_000_000, ..Limits::default() });
        // Frames 0..10 went out, 10 is a 300 KB keyframe (2.4 s at 1 Mbps).
        for _ in 0..11 {
            r.note_sent(1_000, 1_100);
        }
        r.note_keyframe(10, 300_000, 0);
        assert!(r.collapse_keyframe_request(100));
        assert!(r.collapse_keyframe_request(2_000));
        // The client has resolved frames 0..9: still waiting for the keyframe.
        r.update(&Feedback { frames: 10, ..fb(10_000, 5_000, 0, 0) }, 50, 2_100);
        assert!(r.collapse_keyframe_request(2_200));
        // It arrived.
        r.update(&Feedback { frames: 1, ..fb(10_000, 5_000, 0, 0) }, 50, 2_500);
        assert!(!r.collapse_keyframe_request(2_600));
        assert_eq!(r.take_collapsed(), 3);
        // A keyframe the client lost resolves too.
        r.note_sent(1_000, 1_100);
        r.note_keyframe(11, 300_000, 3_000);
        assert!(r.collapse_keyframe_request(3_100));
        r.update(&Feedback { frames: 0, frames_lost: 1, ..fb(10_000, 5_000, 0, 0) }, 50, 3_200);
        assert!(!r.collapse_keyframe_request(3_300));
        // Overdue (feedback lost): answered after its deadline.
        r.note_sent(1_000, 1_100);
        r.note_keyframe(12, 300_000, 4_000);
        assert!(r.collapse_keyframe_request(4_100));
        assert!(!r.collapse_keyframe_request(4_000 + KEY_WAIT_MAX_MS as u64));
        // On a fast path it's on its way for a moment only (once the path
        // is known: the stream's first keyframe is waited for).
        let mut fast = RateController::new(Limits::default());
        fast.note_sent(1_000, 1_100);
        fast.update(&Feedback { frames: 1, ..fb(1_100, 5_000, 0, 0) }, 50, 0);
        fast.note_sent(300_000, 330_000);
        fast.note_keyframe(1, 300_000, 0);
        assert!(fast.collapse_keyframe_request(100));
        assert!(!fast.collapse_keyframe_request(700));
        // Switched off: every request answered.
        fast.set_switches(Switches { one_keyframe: false, ..Switches::default() });
        fast.note_keyframe(0, 300_000, 1_000);
        assert!(!fast.collapse_keyframe_request(1_100));
    }

    /// A lost feedback window leaves the client's count short; a quiet
    /// second squares it, so a later keyframe isn't waited on for nothing.
    #[test]
    fn a_quiet_second_squares_the_frame_count() {
        let mut r = RateController::new(Limits::default());
        for _ in 0..5 {
            r.note_sent(1_000, 1_100);
        }
        r.note_keyframe(4, 1_000, 0);
        // Its feedback never came; then nothing is sent or arrives.
        for t in 1..=25 {
            r.update(&Feedback { frames: 0, ..fb(0, 5_000, 0, 0) }, 50, t * 50);
        }
        assert!(!r.collapse_keyframe_request(1_300));
    }

    #[test]
    fn a_clean_path_lowers_parity_for_big_non_key_frames() {
        let mut r = RateController::new(Limits::default());
        for t in 0..=(CLEAN_AFTER_MS / 50) {
            r.update(&fb(10_000, 5_000, 0, 0), 50, t * 50);
        }
        assert_eq!(r.parity_for(30_000, false), CLEAN_FEC);
        assert!(r.parity_for(30_000, true) >= 0.05, "keyframes keep theirs");
        r.set_switches(Switches { fec_floor: false, ..Switches::default() });
        assert!(r.parity_for(30_000, false) >= 0.05);
        r.set_switches(Switches::default());
        r.update(&fb(10_000, 5_000, 0, 1), 50, CLEAN_AFTER_MS + 100);
        assert!(r.parity_for(30_000, false) >= 0.05, "a loss brings it back");
    }

    /// What the parity costs a frame at 5% and at the clean floor, in
    /// packets (every block gets at least one parity shard).
    #[test]
    fn clean_path_parity_overhead() {
        use crate::video::{MAX_BLOCK_DATA, MAX_SHARD};
        let packets = |bytes: usize, fec: f32| {
            let shards = bytes.div_ceil(MAX_SHARD);
            let blocks = shards.div_ceil(MAX_BLOCK_DATA);
            let parity: usize = (0..blocks)
                .map(|b| {
                    let k = (shards - b * MAX_BLOCK_DATA).min(MAX_BLOCK_DATA);
                    ((k as f32 * fec).ceil() as usize).max(1)
                })
                .sum();
            (shards, parity)
        };
        for bytes in [8_000usize, 20_000, 40_000, 80_000, 150_000] {
            let (k, p5) = packets(bytes, 0.05);
            let (_, p2) = packets(bytes, CLEAN_FEC);
            println!("{:>6} B: {k:>3} data, parity {p5} at 5% ({:.1}%) vs {p2} at 2% ({:.1}%)", bytes, p5 as f64 * 100.0 / k as f64, p2 as f64 * 100.0 / k as f64);
            assert!(p2 <= p5);
        }
    }

    /// Warm started: lower, fast up while the path is clear, and an idle
    /// stream recovers only as far as the path has carried.
    #[test]
    fn a_warm_start_ramps_fast_and_rests_where_the_path_carried() {
        let mut r = RateController::new(Limits::default());
        r.warm_start(1_200_000);
        assert_eq!(r.bitrate_bps(), 1_200_000);
        // Idle: no recovery to 20 Mbps.
        for t in 0..400 {
            r.update(&fb(1_000, 5_500, 0, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), 1_200_000);
        // Busy on a clear path: past 10 Mbps within 2 s (1.02 a window
        // would take ~5 s).
        let mut t = 400;
        while r.bitrate_bps() < 10_000_000 {
            let used = r.bitrate_bps() as u64 / 8 / 20;
            r.update(&fb(used, 5_500, 0, 0), 50, t * 50);
            t += 1;
            assert!(t < 400 + 40, "{} after {} windows", r.bitrate_bps(), t - 400);
        }
        // Switched off (no warm start): the default start.
        assert_eq!(RateController::new(Limits::default()).bitrate_bps(), Limits::default().start_bps);
    }

    #[test]
    fn the_sustained_rate_is_what_busy_windows_held() {
        let mut r = RateController::new(Limits { start_bps: 2_000_000, ..Limits::default() });
        assert_eq!(r.sustained_bps(), None);
        let mut t = 0;
        // A 1.5 Mbps link: the stream uses its budget, queues past it.
        for _ in 0..600 {
            let used = r.bitrate_bps().min(1_500_000) as u64 / 8 / 20;
            let q = if r.bitrate_bps() > 1_500_000 { 40_000 } else { 0 };
            r.update(&fb(used, 5_000 + q, 0, 0), 50, t * 50);
            t += 1;
        }
        let s = r.sustained_bps().unwrap();
        assert!((1_000_000..=1_700_000).contains(&s), "{s}");
    }

    #[test]
    fn the_frame_rate_follows_the_bitrate() {
        let px = 2250 * 1590;
        // The rates chosen, a call every 100 ms, `bps(ms)` from `from_ms`
        // for `ms`; the rate at the end, and the times it changed.
        let drive = |f: &mut FrameRate, full: u32, from_ms: u64, ms: u64, bps: &dyn Fn(u64) -> u32| {
            let (mut last, mut changes) = (None, Vec::new());
            for t in (from_ms..from_ms + ms).step_by(100) {
                let fps = f.follow(bps(t), px, full, t);
                if last.is_some_and(|l| l != fps) {
                    changes.push((t - from_ms, fps));
                }
                last = Some(fps);
            }
            (last.unwrap(), changes)
        };
        let mut f = FrameRate::default();
        // 1 Mbps: 60 → 30 after a second, → 20 after another, no lower.
        let (fps, changes) = drive(&mut f, 60, 0, 10_000, &|_| 1_000_000);
        assert_eq!((fps, changes), (20, vec![(1_000, 30), (2_100, 20)]));
        // Between the marks: it stays (no flapping).
        assert_eq!(drive(&mut f, 60, 10_000, 10_000, &|t| if t % 2_000 < 1_000 { 1_700_000 } else { 2_600_000 }), (20, vec![]));
        // Up only once the rate has held.
        assert_eq!(drive(&mut f, 60, 20_000, 10_000, &|_| 20_000_000), (60, vec![(3_000, 30), (6_100, 60)]));
        // A short dip doesn't step down.
        assert_eq!(drive(&mut f, 60, 30_000, 5_000, &|t| if t % 1_000 < 800 { 1_000_000 } else { 20_000_000 }), (60, vec![]));
        // 120 fps steps 120 → 60 → 40 → 30 → 20; a 30 fps stream stays.
        let mut g = FrameRate::default();
        assert_eq!(drive(&mut g, 120, 0, 10_000, &|_| 500_000).0, 20);
        assert_eq!(drive(&mut FrameRate::default(), 30, 0, 10_000, &|_| 500_000).0, 30);
        // Office mode goes on to 15 and 10 (at 0.5 Mbps), back up as it rises.
        let mut o = FrameRate::default();
        let office = |o: &mut FrameRate, from_ms: u64, bps: u32| {
            (from_ms..from_ms + 15_000).step_by(100).map(|t| o.follow_office(bps, px, 60, t)).last().unwrap()
        };
        assert_eq!(office(&mut o, 0, 500_000), 10);
        assert_eq!(office(&mut o, 15_000, 20_000_000), 60);
        assert_eq!(office(&mut FrameRate::default(), 0, 1_000_000), 15);
        // A LAN's rates never step down.
        assert_eq!(drive(&mut FrameRate::default(), 60, 0, 10_000, &|_| 15_000_000), (60, vec![]));
    }

    // ---- The tablet's relay, simulated ------------------------------------

    /// The host's behaviour: switches, the frame rate following the bitrate,
    /// and a remembered rate to warm start from.
    #[derive(Clone, Copy)]
    struct Setup {
        switches: Switches,
        fps_follow: bool,
        warm_bps: Option<u32>,
        /// A keyframe this often (a decoder reset, a loss no reference can
        /// repair).
        key_every_ms: Option<u64>,
        /// An office screen: little changes except while this says (a
        /// scroll); `None`, every frame uses its budget.
        active: Option<fn(u64) -> bool>,
    }

    impl Setup {
        /// Before these items: only the standing-queue rule.
        fn before() -> Self {
            Self {
                switches: Switches {
                    standing_queue: true,
                    drain: false,
                    one_keyframe: false,
                    fec_floor: false,
                    key_probe: false,
                    gap_cut: false,
                    slow_pace: false,
                    grow_cap: false,
                    soft_slow: false,
                },
                fps_follow: false,
                warm_bps: None,
                key_every_ms: None,
                active: None,
            }
        }

        fn after() -> Self {
            Self { switches: Switches::default(), fps_follow: true, warm_bps: None, key_every_ms: None, active: None }
        }
    }

    #[derive(Default)]
    struct Run {
        keyframes: u32,
        first_picture_ms: Option<u64>,
        /// Every picture shown: (when, ms; capture to arrival, ms; bytes).
        shown: Vec<(u64, f64, usize)>,
        /// Every audio packet: (when sent, ms; its one-way delay, ms).
        audio: Vec<(u64, f64)>,
    }

    impl Run {
        fn window(&self, from_ms: u64, to_ms: u64) -> Vec<&(u64, f64, usize)> {
            self.shown.iter().filter(|s| s.0 >= from_ms && s.0 < to_ms).collect()
        }

        /// Mean and 95th percentile latency (ms), frames a second, and
        /// bytes a frame between two times.
        fn stats(&self, from_ms: u64, to_ms: u64) -> (f64, f64, f64, f64) {
            let w = self.window(from_ms, to_ms);
            if w.is_empty() {
                return (f64::NAN, f64::NAN, 0.0, 0.0);
            }
            let mut lat: Vec<f64> = w.iter().map(|s| s.1).collect();
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = lat.iter().sum::<f64>() / lat.len() as f64;
            let p95 = lat[(lat.len() * 95 / 100).min(lat.len() - 1)];
            let fps = w.len() as f64 * 1000.0 / (to_ms - from_ms) as f64;
            let bytes = w.iter().map(|s| s.2 as f64).sum::<f64>() / w.len() as f64;
            (mean, p95, fps, bytes)
        }

        /// The first time from `from_ms` after which every picture arrived
        /// within `ms`.
        fn settled(&self, from_ms: u64, ms: f64) -> Option<u64> {
            let last_bad = self.shown.iter().filter(|s| s.0 >= from_ms && s.1 > ms).map(|s| s.0).max();
            Some(last_bad.unwrap_or(from_ms))
        }

        fn print(&self, name: &str, phases: &[(u64, u64)]) {
            print!("{name:<28} keyframes {:>3}  first picture {:>5} ms", self.keyframes, self.first_picture_ms.map_or(-1, |t| t as i64));
            for &(a, b) in phases {
                let (mean, p95, fps, bytes) = self.stats(a, b);
                print!(
                    "  | {}-{} s: video {mean:>5.0} / p95 {p95:>5.0} ms, {fps:>4.1} fps, {:>5.1} KB/frame, audio p95 {:>5.0} ms",
                    a / 1000,
                    b / 1000,
                    bytes / 1000.0,
                    self.audio_p95(a, b)
                );
            }
            println!();
        }

        fn audio_p95(&self, from_ms: u64, to_ms: u64) -> f64 {
            let mut d: Vec<f64> = self.audio.iter().filter(|a| a.0 >= from_ms && a.0 < to_ms).map(|a| a.1).collect();
            d.sort_by(|a, b| a.partial_cmp(b).unwrap());
            d.get(d.len() * 95 / 100).copied().unwrap_or(f64::NAN)
        }
    }

    /// The tablet's stream: 2250x1590 at 60 fps.
    const SIM_PIXELS: u64 = 2250 * 1590;
    const SIM_FPS: u32 = 60;
    /// One way, either direction (an RTT of 80 ms through Chicago).
    const SIM_ONE_WAY_US: u64 = 40_000;

    /// A keyframe's size at a bitrate: the log's 240-380 KB at 20 Mbps, down
    /// to a floor (even the coarsest keyframe of a whole screen is tens of KB).
    fn sim_keyframe_bytes(bps: u32) -> usize {
        ((bps as f64 * 0.12 / 8.0) as usize).clamp(40_000, 300_000)
    }

    /// A relay that queues and never drops (Tailscale's DERP over TCP)
    /// carrying `capacity(ms)` bits per second; the host's loop (capture at
    /// 60 Hz, the controller's rate, drain mode, the frame rate) on one end,
    /// the client (a keyframe request every 100 ms until one decodes,
    /// feedback every 50 ms) on the other.
    fn simulate(setup: Setup, secs: u64, capacity: impl Fn(u64) -> f64) -> Run {
        use std::collections::VecDeque;
        let mut r = RateController::new(Limits::default());
        r.set_switches(setup.switches);
        if let Some(w) = setup.warm_bps {
            r.warm_start(w);
        }
        let mut fr = FrameRate::default();
        let mut run = Run::default();
        // The relay: busy until, and what's on it (arrival, capture, bytes, keyframe).
        let mut free_at = 0u64;
        // The host's pacer: each frame's time leaving it.
        let mut pacing: VecDeque<(u64, u64, usize)> = VecDeque::new();
        let mut wire: VecDeque<(u64, u64, usize, bool, u64)> = VecDeque::new();
        // Each frame's first and last byte's arrival, and its size.
        let mut arriving: VecDeque<(u64, u64, usize)> = VecDeque::new();
        let mut requests: VecDeque<u64> = VecDeque::new();
        let mut feedback: VecDeque<(u64, Feedback)> = VecDeque::new();
        let (mut want_key, mut frame_id, mut last_encoded) = (true, 0u32, None::<u64>);
        let mut next_capture = 0u64;
        let (mut awaiting, mut last_request) = (true, None::<u64>);
        let (mut win, mut delay_sum, mut window_start) = (Feedback::default(), 0i64, 0u64);
        for t in (0..secs * 1_000_000).step_by(1_000) {
            let now_ms = t / 1000;
            while requests.front().is_some_and(|&a| a <= t) {
                requests.pop_front();
                if !r.collapse_keyframe_request(now_ms) {
                    want_key = true;
                }
            }
            while feedback.front().is_some_and(|(a, _)| *a <= t) {
                let (_, f) = feedback.pop_front().unwrap();
                r.update(&f, 50, now_ms);
            }
            if setup.key_every_ms.is_some_and(|e| now_ms > 0 && now_ms % e == 0 && t % 1000 == 0) {
                want_key = true;
            }
            if t >= next_capture {
                next_capture += 1_000_000 / SIM_FPS as u64;
                let bps = r.bitrate_bps();
                if std::env::var_os("SIM_TRACE").is_some() && t % 1_000_000 < 16_667 {
                    println!("  {:>3} s: {:>6} kbps, drain {:?}, queue {} ms", t / 1_000_000, bps / 1000, r.drain_fps(now_ms), r.queue_min_us / 1000);
                }
                let fps = if setup.fps_follow { fr.follow(bps, SIM_PIXELS, SIM_FPS, now_ms) } else { SIM_FPS };
                let mut interval = 1_000_000 / fps as u64;
                if let Some(d) = r.drain_fps(now_ms) {
                    interval = interval.max(1_000_000 / d as u64);
                }
                // (The host's credit tolerates a little capture jitter.)
                if want_key || last_encoded.is_none_or(|l| t + 2_000 >= l + interval) {
                    // The encoder's budget a frame: its rate over the frames
                    // it gets (the host scales its bitrate by the skipped ones).
                    let (bytes, key) = if want_key {
                        (sim_keyframe_bytes(bps), true)
                    } else {
                        let budget = (bps as u64 / 8 / fps as u64) as usize;
                        // (A cursor blink, a clock: a KB or so.)
                        (if setup.active.is_some_and(|a| !a(now_ms)) { budget.min(1_200) } else { budget }, false)
                    };
                    (want_key, last_encoded) = (false, Some(t));
                    if key {
                        r.note_keyframe(frame_id, bytes, now_ms);
                        run.keyframes += 1;
                    }
                    let on_wire = (bytes as f32 * (1.0 + r.parity_for(bytes, key))) as usize + 26 * bytes.div_ceil(1150);
                    r.note_sent(bytes, on_wire);
                    // The host's pacer (2.5x the target, or the cap), then the relay.
                    // (A frame still being paced goes at once when the next
                    // one comes, as in the host's sender — except under the
                    // cap, where the next one waits its turn.)
                    let cap = r.pace_cap_bps();
                    let pace = cap.map_or(bps as f64 * 2.5, |c| c as f64);
                    let mut host_start = t;
                    if let Some(p) = pacing.back_mut() {
                        if cap.is_some() {
                            host_start = host_start.max(p.1);
                        } else {
                            p.1 = p.1.min(t);
                        }
                    }
                    let pacer_free = host_start + (on_wire as f64 * 8e6 / pace) as u64;
                    pacing.push_back((host_start, pacer_free, on_wire));
                    let start = free_at.max(host_start);
                    free_at = (start + (on_wire as f64 * 8e6 / capacity(now_ms)) as u64).max(pacer_free);
                    wire.push_back((free_at + SIM_ONE_WAY_US, t, on_wire, key, start + SIM_ONE_WAY_US));
                    arriving.push_back((start + SIM_ONE_WAY_US, free_at + SIM_ONE_WAY_US, on_wire));
                    frame_id += 1;
                }
            }
            while wire.front().is_some_and(|w| w.0 <= t) {
                let (arrive, capture, bytes, key, first) = wire.pop_front().unwrap();
                // (Feedback times a frame's first packet, as the clients do.)
                let delay = (first - capture) as i64;
                if win.frames == 0 || delay < win.delay_min_us {
                    win.delay_min_us = delay;
                }
                delay_sum += delay;
                win.frames += 1;
                win.shards += bytes.div_ceil(1176) as u32;
                if key {
                    awaiting = false;
                    run.first_picture_ms.get_or_insert(now_ms);
                }
                if !awaiting {
                    run.shown.push((now_ms, (arrive - capture) as f64 / 1000.0, bytes));
                }
            }
            // An audio packet every 20 ms (it skips the video pacer): it waits
            // out the video already in the relay.
            if t % 20_000 == 0 {
                while pacing.front().is_some_and(|p| p.1 <= t) {
                    pacing.pop_front();
                }
                let in_host: f64 =
                    pacing.iter().map(|&(a, b, n)| n as f64 * b.saturating_sub(t.max(a)) as f64 / (b - a).max(1) as f64).sum();
                let queued_us = free_at.saturating_sub(t) as f64 - in_host * 8e6 / capacity(now_ms);
                run.audio.push((now_ms, (SIM_ONE_WAY_US as f64 + queued_us.max(0.0)) / 1000.0));
            }
            if awaiting && last_request.is_none_or(|l| t >= l + 100_000) {
                last_request = Some(t);
                requests.push_back(t + SIM_ONE_WAY_US);
            }
            if t >= window_start + 50_000 {
                // The bytes that arrived in the window (packets arrive all
                // along a frame's time on the link, not at its end).
                let mut bytes = 0.0;
                for &(a, b, n) in &arriving {
                    let overlap = b.min(t).saturating_sub(a.max(window_start));
                    bytes += n as f64 * overlap as f64 / (b - a).max(1) as f64;
                }
                while arriving.front().is_some_and(|f| f.1 <= t) {
                    arriving.pop_front();
                }
                win.bytes = bytes as u64;
                if win.frames > 0 {
                    win.delay_avg_us = delay_sum / win.frames as i64;
                }
                win.window_us = 50_000;
                feedback.push_back((t + SIM_ONE_WAY_US, win));
                (win, delay_sum, window_start) = (Feedback::default(), 0, t);
            }
        }
        run
    }

    /// The first connection over the relay (1 Mbps, nothing remembered),
    /// then the next one warm started.
    #[test]
    fn relay_cold_and_warm_start() {
        let link = |_| 1_000_000.0;
        let phases = [(0, 10_000), (10_000, 30_000)];
        let before = simulate(Setup::before(), 30, link);
        let after = simulate(Setup::after(), 30, link);
        let no_probe = simulate(Setup { switches: Switches { key_probe: false, ..Switches::default() }, ..Setup::after() }, 30, link);
        let only_onekey = simulate(Setup { switches: Switches { one_keyframe: true, ..Setup::before().switches }, ..Setup::before() }, 30, link);
        let warm = simulate(Setup { warm_bps: Some(1_200_000), ..Setup::after() }, 30, link);
        before.print("before", &phases);
        only_onekey.print("before + onekey", &phases);
        no_probe.print("after, cold, keyprobe off", &phases);
        after.print("after, cold", &phases);
        warm.print("after, warm (1.0 remembered)", &phases);
        assert!(after.keyframes < before.keyframes, "{} vs {}", after.keyframes, before.keyframes);
        assert!(after.stats(10_000, 30_000).1 < before.stats(10_000, 30_000).1);
        assert!(warm.first_picture_ms.unwrap() < after.first_picture_ms.unwrap());
        assert!(warm.stats(0, 10_000).1 < after.stats(0, 10_000).1);
    }

    /// A steady 1.2 Mbps relay, a keyframe every 4 s: what each keyframe's
    /// burst costs the audio queued behind it.
    #[test]
    fn relay_keyframes_and_audio() {
        let link = |_| 1_200_000.0;
        let phases = [(10_000, 40_000)];
        let base = Setup { warm_bps: Some(1_200_000), key_every_ms: Some(4_000), ..Setup::after() };
        let fast = simulate(base, 40, link);
        let slow = simulate(Setup { switches: Switches { slow_pace: true, ..Switches::default() }, ..base }, 40, link);
        fast.print("keyframes, slowpace off", &phases);
        slow.print("keyframes, slowpace on", &phases);
        assert!(slow.audio_p95(10_000, 40_000) < fast.audio_p95(10_000, 40_000));
    }

    /// Streaming at 6 Mbps, then the relay drops to 0.8 for 30 s and comes
    /// back. Drain mode's part: the same with only it switched off.
    #[test]
    fn relay_capacity_drop() {
        let link = |ms: u64| if (20_000..50_000).contains(&ms) { 800_000.0 } else { 6_000_000.0 };
        let phases = [(20_000, 30_000), (30_000, 50_000), (50_000, 60_000)];
        let before = simulate(Setup::before(), 60, link);
        let without = |s: Switches| simulate(Setup { switches: s, ..Setup::after() }, 60, link);
        let no_drain = without(Switches { drain: false, ..Switches::default() });
        let no_gap = without(Switches { gap_cut: false, ..Switches::default() });
        let slow = without(Switches { slow_pace: true, ..Switches::default() });
        let after = simulate(Setup::after(), 60, link);
        before.print("before", &phases);
        no_drain.print("after, drain off", &phases);
        no_gap.print("after, gapcut off", &phases);
        slow.print("after, slowpace on", &phases);
        after.print("after", &phases);
        let settle = |r: &Run| r.settled(20_000, 250.0).unwrap().saturating_sub(20_000);
        println!(
            "pictures late by >250 ms until {} ms after the drop (before), {} (drain off), {} (after)",
            settle(&before),
            settle(&no_drain),
            settle(&after)
        );
        assert!(settle(&after) < settle(&no_drain));
        assert!(after.stats(20_000, 30_000).1 < no_drain.stats(20_000, 30_000).1);
        assert!(after.stats(20_000, 30_000).1 < no_gap.stats(20_000, 30_000).1);
        assert!(after.stats(20_000, 30_000).1 < before.stats(20_000, 30_000).1);
    }

    /// A relay's jitter (20 ms over the baseline, busy, nothing standing)
    /// doesn't ratchet a slow stream down; switched off, it does.
    #[test]
    fn relay_jitter_does_not_ratchet_a_slow_stream() {
        let run = |soft_slow| {
            let mut r = RateController::new(Limits::default());
            r.set_switches(Switches { soft_slow, ..Switches::default() });
            r.warm_start(1_000_000);
            for t in 0..20 {
                r.update(&fb(3_000, 5_500, 0, 0), 50, t * 50);
            }
            for t in 20..220 {
                let d = if t % 2 == 0 { 25_500 } else { 5_500 };
                let f = Feedback { delay_min_us: 5_000, ..fb(6_000, d, 0, 0) };
                r.update(&f, 50, t * 50);
            }
            r.bitrate_bps()
        };
        let (on, off) = (run(true), run(false));
        assert!(on >= 1_000_000, "{on}");
        assert!(off < 800_000, "{off}");
    }

    /// An office screen on the 1 Mbps relay: idle but for a 3 s scroll at
    /// 25 and at 50 s, cold and warm started.
    #[test]
    fn relay_idle_then_scroll() {
        let link = |_| 1_000_000.0;
        let scroll: fn(u64) -> bool = |ms| (25_000..28_000).contains(&ms) || (50_000..53_000).contains(&ms);
        let phases = [(25_000, 30_000), (50_000, 55_000)];
        let office = Setup { active: Some(scroll), ..Setup::after() };
        let off = Switches { grow_cap: false, ..Switches::default() };
        let mut worse = 0.0;
        for (name, warm) in [("cold", None), ("warm", Some(1_000_000))] {
            let on = simulate(Setup { warm_bps: warm, ..office }, 60, link);
            let no = simulate(Setup { warm_bps: warm, switches: off, ..office }, 60, link);
            no.print(&format!("{name}, growcap off"), &phases);
            on.print(&format!("{name}, growcap on"), &phases);
            for &(a, b) in &phases {
                let (p_on, p_no) = (on.stats(a, b).1, no.stats(a, b).1);
                assert!(p_on <= p_no * 1.05, "{name} {a}: {p_on} vs {p_no}");
                worse += p_no - p_on;
            }
        }
        assert!(worse > 0.0);
    }
}
