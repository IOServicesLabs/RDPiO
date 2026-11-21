//! When a paused stream lets the GPU go, and when it takes it back.
//!
//! A hidden client pauses the stream: the host encodes nothing. Yet the open
//! encoder session, the capture's D3D11 device and its NV12 converter kept a
//! hybrid laptop's discrete GPU at P3 with its memory at full clock —
//! measured on an RTX 2070 Max-Q: 25 W of a 35 W laptop, encoder 0%, one
//! session open — and streams sit paused 93% of the time (133 pauses in a
//! day). So after [`RELEASE_AFTER`] paused the host lets that go (switch
//! `pauserelease`), and rebuilds it, starting with a keyframe, when the
//! client looks again or asks for a picture. Ten seconds: a glance at another
//! window and back costs nothing; a rebuild is a new device, capture and
//! encoder session, a few hundred ms at most, while the client still shows
//! its last picture.
//!
//! What's let go depends on the power source ([`release_whole`]). On
//! battery, the whole pipeline — the capture's D3D11 device with it, as
//! [`Released::Pipeline`]: a live device with nothing to do doesn't hold a
//! P-state, but it does keep the GPU in use, which Optimus needs gone before
//! it can power the GPU off. On mains only the encoder session goes
//! ([`Released::Encoder`]; an open one keeps NVIDIA's memory clock up, like a
//! CUDA context) and the capture keeps its device: creating that again was
//! most of every comeback — measured over 382 comebacks on a desktop host,
//! 258 ms (p50) of a 297 ms wait for the first picture, where the session
//! and its keyframe take about 40. A desktop's GPU drives its monitors
//! anyway. Switch `pausereleasedev=0` keeps the device on battery too — to
//! tell with nvidia-smi which of the two holds the GPU up.

use std::time::{Duration, Instant};

/// How long a stream is paused before the host lets its GPU pipeline go.
pub const RELEASE_AFTER: Duration = Duration::from_secs(10);

/// Whether a long pause lets the whole pipeline go (the `whole` of
/// [`PauseRelease::step`]): only on battery, and not with switch
/// `pausereleasedev` (`switch`) off.
pub fn release_whole(switch: bool, on_battery: bool) -> bool {
    switch && on_battery
}

/// What a long pause let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// The encoder session only (on mains, or `pausereleasedev=0`).
    Encoder,
    /// Encoder, capture, converter and the D3D11 device.
    Pipeline,
}

/// What the host's stream loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Go on as usual (paused or not: the stream's own pause handling).
    Run,
    /// Let go of this now.
    Release(Released),
    /// Let go and still paused: nothing to capture or encode.
    Wait,
    /// Back (the client looks again, asks for a picture, or the switch went
    /// off): rebuild what was let go and start with a keyframe.
    Rebuild(Released),
}

/// The pause's clock and what it let go: one [`Step::Rebuild`] for every
/// [`Step::Release`], whatever the client toggles.
#[derive(Debug, Default)]
pub struct PauseRelease {
    /// Since when the stream has been paused (`None`: it isn't).
    since: Option<Instant>,
    released: Option<Released>,
}

impl PauseRelease {
    /// `paused`: the client's window is hidden; `key`: a keyframe is wanted
    /// (a client's request unpauses as well); `on`, `whole`: switches
    /// `pauserelease` and `pausereleasedev`.
    pub fn step(&mut self, paused: bool, key: bool, on: bool, whole: bool, now: Instant) -> Step {
        if let Some(what) = self.released {
            if paused && !key && on {
                return Step::Wait;
            }
            self.released = None;
            // Still paused (a keyframe wanted, the switch turned off): the
            // pause counts afresh, so a rebuilt pipeline gets its 10 s.
            self.since = paused.then_some(now);
            return Step::Rebuild(what);
        }
        if !paused {
            self.since = None;
            return Step::Run;
        }
        let since = *self.since.get_or_insert(now);
        if on && !key && now.saturating_duration_since(since) >= RELEASE_AFTER {
            let what = if whole { Released::Pipeline } else { Released::Encoder };
            self.released = Some(what);
            return Step::Release(what);
        }
        Step::Run
    }

    /// What's let go now, if anything.
    pub fn released(&self) -> Option<Released> {
        self.released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    #[test]
    fn a_short_pause_keeps_the_pipeline() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        for s in 0..10 {
            assert_eq!(p.step(true, false, true, true, secs(t0, s as f64 + 0.5)), Step::Run);
        }
        // Back after 9.5 s: nothing was let go, nothing to rebuild.
        assert_eq!(p.step(false, false, true, true, secs(t0, 10.0)), Step::Run);
        assert_eq!(p.released(), None);
    }

    #[test]
    fn ten_seconds_paused_lets_go_once_then_waits() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        assert_eq!(p.step(true, false, true, true, t0), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 9.9)), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 10.0)), Step::Release(Released::Pipeline));
        assert_eq!(p.released(), Some(Released::Pipeline));
        for s in 11..4000 {
            assert_eq!(p.step(true, false, true, true, secs(t0, s as f64)), Step::Wait);
        }
    }

    #[test]
    fn coming_back_rebuilds_what_went() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, false, t0);
        assert_eq!(p.step(true, false, true, false, secs(t0, 10.0)), Step::Release(Released::Encoder));
        assert_eq!(p.step(false, false, true, true, secs(t0, 30.0)), Step::Rebuild(Released::Encoder));
        assert_eq!(p.released(), None);
        assert_eq!(p.step(false, false, true, true, secs(t0, 30.1)), Step::Run);
    }

    #[test]
    fn a_keyframe_request_rebuilds_and_the_pause_counts_afresh() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, true, t0);
        p.step(true, false, true, true, secs(t0, 10.0));
        assert_eq!(p.step(true, true, true, true, secs(t0, 20.0)), Step::Rebuild(Released::Pipeline));
        // Still paused once the keyframe went: another 10 s before letting go.
        assert_eq!(p.step(true, false, true, true, secs(t0, 25.0)), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 30.0)), Step::Release(Released::Pipeline));
    }

    #[test]
    fn a_keyframe_wanted_holds_the_release_off() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, true, t0);
        assert_eq!(p.step(true, true, true, true, secs(t0, 12.0)), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 12.1)), Step::Release(Released::Pipeline));
    }

    #[test]
    fn the_switch_off_never_lets_go_and_brings_it_back() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        for s in 0..100 {
            assert_eq!(p.step(true, false, false, true, secs(t0, s as f64)), Step::Run);
        }
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, true, t0);
        p.step(true, false, true, true, secs(t0, 10.0));
        // Turned off mid-pause (a live switch): rebuilt, and kept.
        assert_eq!(p.step(true, false, false, true, secs(t0, 15.0)), Step::Rebuild(Released::Pipeline));
        assert_eq!(p.step(true, false, false, true, secs(t0, 60.0)), Step::Run);
    }

    #[test]
    fn a_pause_that_ends_starts_the_clock_again() {
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, true, t0);
        p.step(false, false, true, true, secs(t0, 8.0));
        assert_eq!(p.step(true, false, true, true, secs(t0, 9.0)), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 18.0)), Step::Run);
        assert_eq!(p.step(true, false, true, true, secs(t0, 19.0)), Step::Release(Released::Pipeline));
    }

    #[test]
    fn many_toggles_rebuild_every_release_once() {
        // A day's 133 toggles, at every pace: never two releases without a
        // rebuild between them (nothing let go twice, nothing leaked).
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        let (mut released, mut rebuilt, mut t) = (0, 0, 0.0);
        for i in 0..133 {
            let paused = i % 2 == 0;
            let hold = [0.2, 3.0, 9.99, 10.0, 47.0, 600.0][i % 6];
            let (end, mut first) = (t + hold, true);
            while t < end {
                // Now and then the first step brings a keyframe request too.
                let key = i % 7 == 0 && std::mem::take(&mut first);
                match p.step(paused, key, true, i % 3 != 0, secs(t0, t)) {
                    Step::Release(_) => {
                        released += 1;
                        assert_eq!(released, rebuilt + 1);
                    }
                    Step::Rebuild(_) => {
                        rebuilt += 1;
                        assert_eq!(released, rebuilt);
                    }
                    Step::Wait => assert!(paused),
                    Step::Run => {}
                }
                t += 0.05;
            }
        }
        if p.step(false, false, true, true, secs(t0, t)) != Step::Run {
            rebuilt += 1;
        }
        assert!(released > 10);
        assert_eq!(released, rebuilt);
    }

    #[test]
    fn mains_keeps_the_device_and_battery_lets_it_go() {
        assert!(release_whole(true, true), "on battery: everything");
        assert!(!release_whole(true, false), "on mains: the encoder session only");
        assert!(!release_whole(false, true), "pausereleasedev=0: the session only");
        let (mut p, t0) = (PauseRelease::default(), Instant::now());
        p.step(true, false, true, release_whole(true, false), t0);
        assert_eq!(p.step(true, false, true, release_whole(true, false), secs(t0, 10.0)), Step::Release(Released::Encoder));
    }
}
