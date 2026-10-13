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
        Self { min_bps: 1_500_000, start_bps: 20_000_000, max_bps: 80_000_000 }
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
/// Feedback windows of delay minima kept for the baseline (~5 s at 50 ms)…
const BASELINE_WINDOWS: usize = 100;
/// …and no older than this. The delay is the client's clock minus the host's,
/// and the two drift apart: a minimum from before a pause (a minimized
/// client, a still desktop: no frames, no samples) would read as queueing.
const BASELINE_MS: u64 = 5_000;
/// Bulk transfers (clipboard, files) share the link as a scavenger: they start
/// here, halve at the first sign of queueing and grow only while busy.
const BULK_START: f64 = 8_000_000.0;
const BULK_MIN: f64 = 1_000_000.0;
const BULK_MAX: f64 = 100_000_000.0;

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
}

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
        }
    }

    /// Account one sent frame: its encoded size and the packet bodies it
    /// took (so delivered bytes can be turned back into encoder terms).
    pub fn note_sent(&mut self, payload: usize, wire: usize) {
        self.sent_payload += payload as u64;
        self.sent_wire += wire as u64;
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
        (self.sent_payload, self.sent_wire) = (0, 0);
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

        let queue_us = if fb.frames > 0 {
            while self.minima.len() >= BASELINE_WINDOWS
                || self.minima.front().is_some_and(|&(t, _)| now_ms.saturating_sub(t) > BASELINE_MS)
            {
                self.minima.pop_front();
            }
            self.minima.push_back((now_ms, fb.delay_min_us));
            let base = self.minima.iter().map(|&(_, d)| d).min().unwrap();
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
        // Using a real share of the budget: only then can a queue be ours.
        let busy = used >= self.bitrate * 0.5;
        // A little queueing while the stream barely sends is the radio's
        // jitter, not our congestion: cutting for it only ratcheted an idle
        // stream down (and starved the next motion or page of text). A lost
        // frame or a long queue counts whatever we sent.
        let decrease = if fb.frames_lost > 0 || queue_us > QUEUE_HARD_US {
            Some(0.75)
        } else if queue_us > QUEUE_SOFT_US && busy {
            Some(0.93)
        } else {
            None
        };
        if let Some(f) = decrease {
            self.bitrate *= f;
            // A lost frame alone (random Wi-Fi loss) shouldn't cut below what
            // demonstrably gets through — in encoder terms: delivered bytes
            // include shard headers and parity. A growing queue gets no
            // floor: it has to drain.
            if queue_us <= QUEUE_SOFT_US {
                let delivered = fb.bytes as f64 * 8.0 * 1000.0 / window_ms.max(1) as f64 / self.overhead;
                self.bitrate = self.bitrate.max(delivered * 0.85).min(self.bitrate / f);
            }
            self.hold_until_ms = now_ms + HOLD_MS;
            self.last_cut_ms = Some(now_ms);
        } else if now_ms >= self.hold_until_ms {
            if busy {
                self.bitrate *= 1.02;
            } else if self.bitrate < self.resting
                && self.last_cut_ms.is_none_or(|t| now_ms.saturating_sub(t) >= RECOVER_AFTER_MS)
            {
                // Idle and clean for a while: back toward the resting rate.
                self.bitrate = (self.bitrate * 1.02).min(self.resting);
            }
        }
        self.bitrate = self.bitrate.clamp(self.limits.min_bps as f64, self.limits.max_bps as f64);
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
        r.update(&fb(10_000, 5_000, 0, 0), 50, 0);
        let before = r.bitrate_bps();
        r.update(&fb(10_000, 5_000 + 40_000, 0, 0), 50, 50);
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

    #[test]
    fn stays_within_limits() {
        let limits = Limits { min_bps: 2_000_000, start_bps: 3_000_000, max_bps: 4_000_000 };
        let mut r = RateController::new(limits);
        for t in 0..200 {
            r.update(&fb(10_000, 5_000, 1, 0), 50, t * 50);
        }
        assert_eq!(r.bitrate_bps(), 2_000_000);
    }
}
