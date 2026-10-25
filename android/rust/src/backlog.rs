//! When frames waiting for the decoder are thrown away for a fresh keyframe.
//!
//! A decoder slower than the stream falls further behind each frame: catching
//! up only adds latency, so the frames go and a keyframe starts it afresh.
//! But a relayed TCP path (Tailscale's DERP) delivers in clumps: nothing for a
//! second, then that second's frames at once. That backlog isn't the
//! decoder's: it decodes the clump in a few ms a frame (the screen shows only
//! the newest: see the decoder's stale-frame drop). Throwing it away instead
//! asks for a keyframe — tens to hundreds of KB over a link that carries a
//! few Mbit/s, seconds of it, which arrives as the next clump, and so on.
//!
//! So: a backlog is thrown away only when it hasn't shrunk over a while
//! (the decoder isn't draining it), or when it's huge (catching up would take
//! longer than a keyframe). `debug.rdpio.burstok=0`: thrown away at once, as
//! before.
//!
//! Pure logic, tested on the PC: `rustc --test src/backlog.rs`.

use std::time::{Duration, Instant};

/// A backlog that hasn't shrunk in this long is the decoder's.
pub const DRAIN: Duration = Duration::from_millis(300);
/// Seconds of frames beyond which catching up costs more than a keyframe.
const CATCH_UP_S: u32 = 2;

#[derive(Default, Debug)]
pub struct Backlog {
    /// Since when it has stood over the limit, and how big it was then.
    over_since: Option<(Instant, u32)>,
}

impl Backlog {
    /// `backlog`: frames received and not yet handed to the decoder; `fps`:
    /// the stream's. True: throw them away and wait for a keyframe.
    pub fn skip(&mut self, now: Instant, backlog: u32, fps: u32, burst_ok: bool) -> bool {
        // (~50 ms of frames: the old rule's limit.)
        let fps = fps.max(30);
        let limit = (fps / 20).max(3);
        if backlog <= limit {
            self.over_since = None;
            return false;
        }
        if !burst_ok || backlog > fps * CATCH_UP_S {
            self.over_since = None;
            return true;
        }
        let (since, then) = *self.over_since.get_or_insert((now, backlog));
        if now.duration_since(since) < DRAIN {
            return false;
        }
        // Draining: watch the next stretch. Not: the decoder's behind.
        if backlog < then {
            self.over_since = Some((now, backlog));
            return false;
        }
        self.over_since = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames arrive as `arrivals` (time ms, count); the decoder takes one
    /// each `decode_ms`. How many backlogs were thrown away.
    fn simulate(arrivals: &[(u64, u32)], decode_ms: u64, fps: u32, burst_ok: bool) -> (u32, u64) {
        let t0 = Instant::now();
        let mut b = Backlog::default();
        let mut waiting = 0u32;
        let mut skips = 0;
        let mut decoded = 0;
        let end = arrivals.last().unwrap().0 + 3000;
        let mut next = arrivals.iter().peekable();
        let mut busy_until = 0;
        for ms in 0..end {
            while let Some(&&(at, n)) = next.peek() {
                if at > ms {
                    break;
                }
                waiting += n;
                next.next();
            }
            if waiting > 0 && ms >= busy_until {
                waiting -= 1;
                // (The loop looks at the backlog behind the frame it took.)
                if b.skip(t0 + Duration::from_millis(ms), waiting, fps, burst_ok) {
                    skips += 1;
                    waiting = 0;
                } else {
                    decoded += 1;
                    busy_until = ms + decode_ms;
                }
            }
        }
        (skips, decoded)
    }

    #[test]
    fn a_clump_after_a_stall_is_decoded_not_dropped() {
        // 60 fps, a second's stall, then 60 frames at once (and again).
        let arrivals: Vec<(u64, u32)> = (0..5).map(|i| (1000 * i, 60)).collect();
        let (skips_old, _) = simulate(&arrivals, 4, 60, false);
        let (skips_new, decoded) = simulate(&arrivals, 4, 60, true);
        assert!(skips_old >= 5, "the old rule drops every clump ({skips_old})");
        assert_eq!(skips_new, 0);
        assert_eq!(decoded, 300);
    }

    #[test]
    fn a_big_clump_at_144_fps_too() {
        // A second's stall at 144 fps: 144 frames, ~0.6 s to decode.
        let arrivals: Vec<(u64, u32)> = (0..5).map(|i| (1500 * i, 144)).collect();
        let (skips, decoded) = simulate(&arrivals, 4, 144, true);
        assert_eq!(skips, 0);
        assert_eq!(decoded, 720);
    }

    #[test]
    fn a_slow_decoder_still_skips() {
        // 60 fps arriving evenly, a decoder at 25 ms a frame: the backlog
        // grows for good; it goes after DRAIN.
        let arrivals: Vec<(u64, u32)> = (0..300).map(|i| (i * 1000 / 60, 1)).collect();
        let (skips, _) = simulate(&arrivals, 25, 60, true);
        assert!(skips >= 3, "{skips}");
    }

    #[test]
    fn a_huge_backlog_goes_at_once() {
        let t0 = Instant::now();
        let mut b = Backlog::default();
        assert!(b.skip(t0, 200, 60, true));
        assert!(!b.skip(t0, 3, 60, true));
        assert!(!b.skip(t0, 50, 60, true));
        // Shrinking: kept.
        assert!(!b.skip(t0 + DRAIN, 40, 60, true));
        // Not shrinking over the next stretch: gone.
        assert!(b.skip(t0 + DRAIN * 2, 45, 60, true));
    }
}
