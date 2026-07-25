//! Network-congestion estimation for the UDP side-band transport.
//!
//! On a clean wired LAN the multitransport's UDP path is a pure win and needs no
//! tuning. On a *lossy* link — a host on Wi-Fi, say — two things have to adapt:
//!
//!  * **The retransmit timeout (RTO).** A fixed 200 ms is absurd when the RTT is
//!    5 ms: a lost reliable datagram then stalls ~40× longer than it should.
//!    [`RttEstimator`] tracks SRTT/RTTVAR (Jacobson/Karels, RFC 6298) and yields
//!    a sane, responsive RTO.
//!
//!  * **The server's send rate.** The only client-side lever RDP gives us is the
//!    RDPGFX frame-ack `queueDepth` — the server paces itself to it. Normally we
//!    report just the decode backlog, but a fast client GPU decodes
//!    instantly, so on a dropping Wi-Fi link the backlog reads ~0 and the server
//!    never slows down even as packets are lost. [`Congestion`] turns observed
//!    loss + RTT inflation into an *additional* `queueDepth` bias so the server
//!    eases off before the link is overrun, then releases it as the link
//!    recovers.
//!
//! Pure and clock-free (it works in [`Duration`]s the caller measures), so it is
//! unit-tested without a network.

use std::time::Duration;

/// RTO is clamped to this floor. The RFC 6298 floor is 1 s — appropriate for TCP
/// over the open internet, but far too sluggish for a low-latency side channel,
/// where we would rather risk an occasional spurious resend than stall a frame.
const MIN_RTO: Duration = Duration::from_millis(15);
/// RTO ceiling: past this the link is hopeless and we should be on TCP anyway.
const MAX_RTO: Duration = Duration::from_millis(250);
/// Clock granularity term `G` in the RTO formula.
const CLOCK_GRANULARITY_US: f64 = 1_000.0;

/// Jacobson/Karels SRTT/RTTVAR estimator (RFC 6298), in microseconds.
#[derive(Debug, Clone, Default)]
pub struct RttEstimator {
    srtt_us: Option<f64>,
    rttvar_us: f64,
}

impl RttEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in one round-trip measurement.
    pub fn sample(&mut self, rtt: Duration) {
        let r = rtt.as_micros() as f64;
        match self.srtt_us {
            None => {
                // First measurement: SRTT = R, RTTVAR = R/2 (RFC 6298 §2.2).
                self.srtt_us = Some(r);
                self.rttvar_us = r / 2.0;
            }
            Some(srtt) => {
                // RTTVAR = 3/4·RTTVAR + 1/4·|SRTT − R|; SRTT = 7/8·SRTT + 1/8·R.
                self.rttvar_us = 0.75 * self.rttvar_us + 0.25 * (srtt - r).abs();
                self.srtt_us = Some(0.875 * srtt + 0.125 * r);
            }
        }
    }

    /// Smoothed round-trip time, or `None` before the first sample.
    pub fn srtt(&self) -> Option<Duration> {
        self.srtt_us.map(|us| Duration::from_micros(us as u64))
    }

    /// Variance of the round-trip time (jitter), 0 before the first sample.
    pub fn jitter(&self) -> Duration {
        Duration::from_micros(self.rttvar_us as u64)
    }

    /// Current retransmit timeout: SRTT + max(G, 4·RTTVAR), clamped. Returns the
    /// conservative [`MAX_RTO`] before any sample so early loss still recovers.
    pub fn rto(&self) -> Duration {
        let Some(srtt) = self.srtt_us else {
            return MAX_RTO;
        };
        let rto_us = srtt + (4.0 * self.rttvar_us).max(CLOCK_GRANULARITY_US);
        Duration::from_micros(rto_us as u64).clamp(MIN_RTO, MAX_RTO)
    }
}

/// EWMA weight for the observed loss fraction (higher = more reactive).
const LOSS_ALPHA: f64 = 0.25;
/// Above this smoothed loss fraction we ratchet pressure *up*.
const LOSS_HIGH: f64 = 0.02;
/// Below this we allow pressure to ease *down* (hysteresis gap vs. `LOSS_HIGH`).
const LOSS_LOW: f64 = 0.005;
/// RTT inflation (current SRTT ÷ baseline) above which we ratchet pressure up —
/// a growing queue shows up as latency before it shows up as loss.
const RTT_INFLATE_HIGH: f64 = 2.0;
/// RTT inflation below which we allow pressure to ease down.
const RTT_INFLATE_LOW: f64 = 1.3;
/// Maximum pressure level.
const MAX_PRESSURE: u32 = 4;
/// Each pressure level adds this much to the reported RDPGFX `queueDepth`.
const BIAS_PER_LEVEL: u32 = 2;

/// Turns observed loss + RTT inflation into back-pressure on the server, via a
/// `queueDepth` bias added to the RDPGFX frame-acknowledge. Hysteresis (separate
/// up/down thresholds and one-level-at-a-time moves) keeps it from oscillating.
#[derive(Debug, Clone)]
pub struct Congestion {
    loss_ewma: f64,
    /// Smallest SRTT seen (the link's floor), used as the inflation baseline.
    baseline_us: Option<f64>,
    pressure: u32,
}

impl Default for Congestion {
    fn default() -> Self {
        Self {
            loss_ewma: 0.0,
            baseline_us: None,
            pressure: 0,
        }
    }
}

impl Congestion {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in a window of transport stats: how many source datagrams arrived vs.
    /// were detected missing, and the current smoothed RTT (if known). Call once
    /// per service interval.
    pub fn update(&mut self, received: u64, lost: u64, srtt: Option<Duration>) {
        let total = received + lost;
        if total > 0 {
            let instant_loss = lost as f64 / total as f64;
            self.loss_ewma = (1.0 - LOSS_ALPHA) * self.loss_ewma + LOSS_ALPHA * instant_loss;
        }

        let inflation = match (srtt, self.baseline_us) {
            (Some(s), _) => {
                let us = s.as_micros() as f64;
                // Track the floor downward immediately, upward never (it is a
                // baseline, not an average); let it drift up slowly so a genuinely
                // changed path eventually re-baselines.
                let base = match self.baseline_us {
                    Some(b) if us < b => us,
                    Some(b) => 0.999 * b + 0.001 * us,
                    None => us,
                };
                self.baseline_us = Some(base);
                if base > 0.0 { us / base } else { 1.0 }
            }
            (None, _) => 1.0,
        };

        let congested = self.loss_ewma > LOSS_HIGH || inflation > RTT_INFLATE_HIGH;
        let relaxed = self.loss_ewma < LOSS_LOW && inflation < RTT_INFLATE_LOW;
        if congested && self.pressure < MAX_PRESSURE {
            self.pressure += 1;
        } else if relaxed && self.pressure > 0 {
            self.pressure -= 1;
        }
    }

    /// Extra `queueDepth` to add to the RDPGFX frame-ack so the server paces down.
    /// `0` when the link is healthy (the server then streams at full rate).
    pub fn queue_depth_bias(&self) -> u32 {
        self.pressure * BIAS_PER_LEVEL
    }

    /// Current pressure level (0 = healthy), for logging/metrics.
    pub fn pressure(&self) -> u32 {
        self.pressure
    }

    /// Smoothed loss fraction (0.0–1.0), for logging/metrics.
    pub fn loss_fraction(&self) -> f64 {
        self.loss_ewma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rto_defaults_high_before_any_sample() {
        let est = RttEstimator::new();
        assert_eq!(est.rto(), MAX_RTO);
        assert!(est.srtt().is_none());
    }

    #[test]
    fn rto_tracks_a_steady_low_rtt() {
        let mut est = RttEstimator::new();
        for _ in 0..50 {
            est.sample(Duration::from_millis(5));
        }
        let srtt = est.srtt().unwrap();
        assert!(
            (4..=6).contains(&srtt.as_millis()),
            "srtt {srtt:?} should converge near 5ms"
        );
        // A steady 5ms RTT must yield an RTO far below the old fixed 200ms.
        assert!(est.rto() < Duration::from_millis(40), "rto {:?}", est.rto());
        assert!(est.rto() >= MIN_RTO);
    }

    #[test]
    fn rto_is_clamped_to_bounds() {
        let mut est = RttEstimator::new();
        est.sample(Duration::from_micros(1)); // tiny RTT → clamp up to MIN_RTO
        assert!(est.rto() >= MIN_RTO);
        let mut slow = RttEstimator::new();
        for _ in 0..10 {
            slow.sample(Duration::from_secs(2)); // huge RTT → clamp to MAX_RTO
        }
        assert_eq!(slow.rto(), MAX_RTO);
    }

    #[test]
    fn jitter_grows_with_variance() {
        let mut steady = RttEstimator::new();
        let mut jumpy = RttEstimator::new();
        for _ in 0..20 {
            steady.sample(Duration::from_millis(10));
        }
        for i in 0..20 {
            // Alternate 5ms/30ms — same mean-ish, much higher variance.
            jumpy.sample(Duration::from_millis(if i % 2 == 0 { 5 } else { 30 }));
        }
        assert!(jumpy.jitter() > steady.jitter());
    }

    #[test]
    fn no_pressure_on_a_clean_link() {
        let mut c = Congestion::new();
        for _ in 0..20 {
            c.update(500, 0, Some(Duration::from_millis(5)));
        }
        assert_eq!(c.pressure(), 0);
        assert_eq!(c.queue_depth_bias(), 0);
    }

    #[test]
    fn loss_raises_pressure_then_recovery_lowers_it() {
        let mut c = Congestion::new();
        // Sustained 10% loss should ratchet pressure up over several windows.
        for _ in 0..MAX_PRESSURE + 2 {
            c.update(90, 10, Some(Duration::from_millis(5)));
        }
        assert!(c.pressure() > 0, "loss should build pressure");
        assert!(c.queue_depth_bias() > 0);
        // A clean link should walk it back down to zero. Recovery is deliberately
        // unhurried: the loss EWMA (×0.75/window) must fall below LOSS_LOW before
        // pressure releases, so allow enough clean windows for that to happen.
        for _ in 0..30 {
            c.update(500, 0, Some(Duration::from_millis(5)));
        }
        assert_eq!(c.pressure(), 0, "recovery should release back-pressure");
    }

    #[test]
    fn rtt_inflation_raises_pressure_without_loss() {
        let mut c = Congestion::new();
        // Establish a 5ms baseline with no loss.
        for _ in 0..5 {
            c.update(500, 0, Some(Duration::from_millis(5)));
        }
        assert_eq!(c.pressure(), 0);
        // Latency triples (queue building) but zero packets are "lost" yet.
        for _ in 0..3 {
            c.update(500, 0, Some(Duration::from_millis(20)));
        }
        assert!(c.pressure() > 0, "RTT inflation alone should apply back-pressure");
    }

    #[test]
    fn pressure_moves_one_level_at_a_time() {
        let mut c = Congestion::new();
        c.update(50, 50, Some(Duration::from_millis(5))); // 50% loss, one window
        assert_eq!(c.pressure(), 1, "a single bad window should not slam to max");
    }
}
