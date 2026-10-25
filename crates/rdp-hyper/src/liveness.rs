//! When a client takes the stream for dead, and how soon it signs in again.
//!
//! Over a relayed path (Tailscale through DERP: TCP via a server elsewhere)
//! the link drops now and then. Waiting 6 s of silence and then 2 s between
//! sign-ins cost 10-20 s a drop; the host keeps sending (pongs answer pings
//! every 0.5-1 s, status and path every 2 s), so 1.5 s of silence is already
//! suspect: the client pings harder then, and gives up after 3 s (longer on a
//! path whose round trip is long), signing in again at once and then backing
//! off from 250 ms. The host ends the old stream as soon as the client's new
//! sign-in arrives (`hyper_direct::comeback`), so the two don't wait each
//! other out. `fast`: switch `fastreconnect` (off: 6 s, then 0.3 s / 2 s).

use std::time::Duration;

/// What `silent` (time since the host's last packet) means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Suspect: ping every [`PROBE_EVERY`] until something comes back.
    Probe,
    Lost,
}

/// Silence that starts probing.
pub const PROBE_AFTER: Duration = Duration::from_millis(1500);
/// Ping spacing while probing.
pub const PROBE_EVERY: Duration = Duration::from_millis(200);
/// Silence that ends a stream, before / after its first video.
pub const SLOW_LOST: Duration = Duration::from_secs(6);
pub const SETUP_LOST: Duration = Duration::from_secs(15);
/// The fast verdict's floor (a long round trip lengthens it, up to SLOW_LOST).
pub const FAST_LOST: Duration = Duration::from_secs(3);

/// `heard`: video came (the host's setup is done); `paused`: the client told
/// the host it's hidden (it pings only every 2 s then); `rtt`: the latest
/// round trip (0 unknown).
pub fn verdict(silent: Duration, heard: bool, paused: bool, rtt: Duration, fast: bool) -> Verdict {
    if !heard {
        return if silent > SETUP_LOST { Verdict::Lost } else { Verdict::Fine };
    }
    if !fast || paused {
        return if silent > SLOW_LOST { Verdict::Lost } else { Verdict::Fine };
    }
    let lost = (rtt * 4).clamp(FAST_LOST, SLOW_LOST);
    if silent > lost {
        Verdict::Lost
    } else if silent > PROBE_AFTER {
        Verdict::Probe
    } else {
        Verdict::Fine
    }
}

/// The wait before sign-in attempt `attempt` (1-based) after a drop.
pub fn reconnect_wait(attempt: u32, fast: bool) -> Duration {
    if fast {
        Duration::from_millis(250u64 << attempt.saturating_sub(1).min(3))
    } else if attempt <= 1 {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_silent_relayed_link_is_probed_then_dropped_within_3_s() {
        let v = |s, rtt| verdict(MS(s), true, false, MS(rtt), true);
        assert_eq!(v(1000, 80), Verdict::Fine);
        assert_eq!(v(1600, 80), Verdict::Probe);
        assert_eq!(v(2900, 300), Verdict::Probe);
        assert_eq!(v(3100, 300), Verdict::Lost);
        // A long round trip (a deep queue) gets longer, never past 6 s.
        assert_eq!(v(3100, 1200), Verdict::Probe);
        assert_eq!(v(5000, 1200), Verdict::Lost);
        assert_eq!(v(6100, 9000), Verdict::Lost);
    }

    #[test]
    fn setup_pause_and_the_switch_keep_the_old_timeouts() {
        assert_eq!(verdict(MS(10_000), false, false, MS(50), true), Verdict::Fine);
        assert_eq!(verdict(MS(15_100), false, false, MS(50), true), Verdict::Lost);
        assert_eq!(verdict(MS(4000), true, true, MS(50), true), Verdict::Fine);
        assert_eq!(verdict(MS(4000), true, false, MS(50), false), Verdict::Fine);
        assert_eq!(verdict(MS(6100), true, false, MS(50), false), Verdict::Lost);
    }

    #[test]
    fn reconnects_back_off_from_250_ms() {
        let fast: Vec<_> = (1..=6).map(|a| reconnect_wait(a, true).as_millis()).collect();
        assert_eq!(fast, [250, 500, 1000, 2000, 2000, 2000]);
        assert_eq!(reconnect_wait(1, false), MS(300));
        assert_eq!(reconnect_wait(2, false), MS(2000));
    }
}
