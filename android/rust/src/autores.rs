//! Automatic stream resolution ("Auto" under Game resolution and Office
//! resolution): the stream's size, as a share of the screen, picked from the
//! bits the link actually carries.
//!
//! The measure is bits per pixel per frame: the bitrate the stream can have
//! over the pixels a second it would carry. Too few and HEVC smears (blocks,
//! soft text); fewer pixels then look better scaled up here than many pixels
//! starved of bits. The bitrate is the host's target, except while the link
//! is visibly behind (latency or round trip far up: frames queue somewhere on
//! the way), when it's what actually arrived — on a relayed TCP path the
//! host's target can sit at 15 Mbps while 3 arrive, seconds late.
//!
//! Each change costs the host an encoder re-init and a keyframe, so: down
//! only while the link is behind and the bits are short for ~3 s, up only
//! after ~10 s comfortably clear, at most one change per 10 s, and an up
//! that has to be taken back soon makes the next up wait twice as long.
//! Never down while the link keeps up (a LAN at full size stays there).
//!
//! Pure logic (no Android), tested on the PC: `rustc --test src/autores.rs`.

use std::collections::VecDeque;

/// One second's numbers from the perf line.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Seconds, on any clock that only moves forward.
    pub now_s: f64,
    /// Bits a second that arrived (the perf line's first Mbps).
    pub received_bps: f64,
    /// The host's target (the perf line's second Mbps).
    pub target_bps: f64,
    /// Capture to screen, median (ms).
    pub latency_ms: f64,
    pub rtt_ms: f64,
    /// The host's frame rate (its Status).
    pub fps: f64,
    /// Pictures shown that second: none, no evidence either way.
    pub shown_fps: f64,
    /// The stream's size now (at `current`).
    pub width: u32,
    pub height: u32,
}

/// A change to make.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub scale: u32,
    pub from: u32,
    pub budget_bps: f64,
    /// At the new scale, and at the old.
    pub bpp: f64,
    pub bpp_from: f64,
    pub behind: bool,
}

/// The steps each mode offers (the menu's).
pub fn steps(office: bool) -> &'static [u32] {
    if office {
        &[100, 75, 50, 33]
    } else {
        &[100, 75, 50]
    }
}

/// Bits per pixel per frame below which the picture starves (down, while
/// behind) and above which the next size up would still be comfortable (up).
/// Office: desktop text compresses several times better than games.
fn thresholds(office: bool) -> (f64, f64) {
    if office {
        (0.012, 0.025)
    } else {
        (0.03, 0.06)
    }
}

/// The link is behind: frames queue on the way.
const BEHIND_LATENCY_MS: f64 = 250.0;
const BEHIND_RTT_MS: f64 = 500.0;
/// Down after this long short of bits; up after `up_hold` clear.
const DOWN_AFTER_S: f64 = 3.0;
const UP_AFTER_S: f64 = 10.0;
const UP_AFTER_MAX_S: f64 = 160.0;
/// At most one change this often.
const CHANGE_EVERY_S: f64 = 10.0;
/// After a change, its numbers ignored this long (the host re-inits).
const SETTLE_S: f64 = 2.0;
/// An up taken back within this long: the next waits twice as long.
const FLAP_S: f64 = 30.0;
/// Clear this long: the up wait back to its shortest.
const CALM_S: f64 = 300.0;
/// An up may count on this much more than arrived when the link last fell behind.
const CEILING_ROOM: f64 = 1.25;
/// Arrival averaged over this many seconds (one second's clump after a
/// stall says little).
const RECENT: usize = 5;

#[derive(Debug)]
pub struct AutoRes {
    office: Option<bool>,
    current: u32,
    below_since: Option<f64>,
    above_since: Option<f64>,
    last_change: Option<f64>,
    last_up: Option<f64>,
    last_behind: Option<f64>,
    up_hold_s: f64,
    recent: VecDeque<f64>,
    /// What arrived when an up last had to be taken back (bits a second),
    /// and when: an up must fit under it (with some room) until it's old,
    /// or more arrives — a host target that ignores the queue isn't
    /// believed twice.
    ceiling: Option<(f64, f64)>,
}

impl Default for AutoRes {
    fn default() -> Self {
        Self {
            office: None,
            current: 0,
            below_since: None,
            above_since: None,
            last_change: None,
            last_up: None,
            last_behind: None,
            up_hold_s: UP_AFTER_S,
            recent: VecDeque::new(),
            ceiling: None,
        }
    }
}

impl AutoRes {
    /// The next second's numbers at scale `current` (% of the screen);
    /// `Some`: change to that scale now.
    pub fn step(&mut self, s: &Sample, current: u32, office: bool) -> Option<Decision> {
        // A new mode or a scale changed elsewhere: start the watch over.
        if self.office != Some(office) || self.current != current {
            self.below_since = None;
            self.above_since = None;
            if self.office != Some(office) {
                self.recent.clear();
            }
            self.office = Some(office);
            self.current = current;
        }
        if s.shown_fps < 1.0 || s.width == 0 || s.height == 0 || current == 0 {
            return None;
        }
        self.recent.push_back(s.received_bps.max(0.0));
        while self.recent.len() > RECENT {
            self.recent.pop_front();
        }
        let arrived = self.recent.iter().sum::<f64>() / self.recent.len() as f64;
        let behind = s.latency_ms > BEHIND_LATENCY_MS || s.rtt_ms > BEHIND_RTT_MS;
        if behind {
            self.last_behind = Some(s.now_s);
        } else if self.last_behind.is_none_or(|t| s.now_s - t >= CALM_S) {
            self.up_hold_s = UP_AFTER_S;
        }
        let target = if s.target_bps > 0.0 { s.target_bps } else { arrived };
        let budget = if behind { target.min(arrived) } else { target };
        // (Past 60 fps each frame differs less from the last and needs
        // fewer bits: counted as 60.)
        let fps = if s.fps >= 1.0 { s.fps.min(60.0) } else { 60.0 };
        let pixels_now = s.width as f64 * s.height as f64;
        let bpp_at = |scale: u32| {
            let r = scale as f64 / current as f64;
            budget / (pixels_now * r * r * fps)
        };
        let (down_bpp, up_bpp) = thresholds(office);
        let steps = steps(office);
        let smallest = *steps.last().unwrap();
        let bpp_now = bpp_at(current);
        let changed_ok = self.last_change.is_none_or(|t| s.now_s - t >= CHANGE_EVERY_S);
        // (Just changed: the stream's size and bitrate still the old ones.)
        if self.last_change.is_some_and(|t| s.now_s - t < SETTLE_S) {
            return None;
        }

        // Down: behind, and too few bits for this size.
        if behind && current > smallest && bpp_now < down_bpp {
            let since = *self.below_since.get_or_insert(s.now_s);
            if s.now_s - since >= DOWN_AFTER_S && changed_ok {
                // The largest size with some room above the floor (else the smallest).
                let to = steps.iter().copied().filter(|&p| p < current).find(|&p| bpp_at(p) >= down_bpp * 1.3).unwrap_or(smallest);
                // An up taken back: the target over-promised.
                if self.last_up.is_some_and(|t| s.now_s - t < FLAP_S) {
                    self.up_hold_s = (self.up_hold_s * 2.0).min(UP_AFTER_MAX_S);
                    self.ceiling = Some((arrived, s.now_s));
                }
                return Some(self.changed(s.now_s, to, current, budget, bpp_at(to), bpp_now, behind));
            }
        } else {
            self.below_since = None;
        }

        // Up: keeping up, and the next size would still have plenty.
        if self.ceiling.is_some_and(|(_, at)| s.now_s - at >= CALM_S) {
            self.ceiling = None;
        }
        let room = match self.ceiling {
            Some((bps, _)) => budget.min(bps.max(arrived) * CEILING_ROOM) / budget.max(1.0),
            None => 1.0,
        };
        let up = steps.iter().copied().filter(|&p| p > current).min();
        match up {
            Some(to) if !behind && bpp_at(to) * room >= up_bpp => {
                let since = *self.above_since.get_or_insert(s.now_s);
                if s.now_s - since >= self.up_hold_s && changed_ok {
                    self.last_up = Some(s.now_s);
                    return Some(self.changed(s.now_s, to, current, budget, bpp_at(to), bpp_now, behind));
                }
            }
            _ => self.above_since = None,
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn changed(&mut self, now: f64, to: u32, from: u32, budget: f64, bpp: f64, bpp_from: f64, behind: bool) -> Decision {
        self.last_change = Some(now);
        self.below_since = None;
        self.above_since = None;
        self.current = to;
        Decision { scale: to, from, budget_bps: budget, bpp, bpp_from, behind }
    }

    /// The wait before an up (for the log).
    pub fn up_hold_s(&self) -> f64 {
        self.up_hold_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tablet: 3000x2120.
    const W: u32 = 3000;
    const H: u32 = 2120;

    fn at(scale: u32) -> (u32, u32) {
        (W * scale / 100, H * scale / 100)
    }

    fn sample(t: f64, scale: u32, rx_mbps: f64, target_mbps: f64, latency: f64, fps: f64) -> Sample {
        let (width, height) = at(scale);
        Sample {
            now_s: t,
            received_bps: rx_mbps * 1e6,
            target_bps: target_mbps * 1e6,
            latency_ms: latency,
            rtt_ms: latency.min(300.0),
            fps,
            shown_fps: fps.min(30.0),
            width,
            height,
        }
    }

    /// Run `secs` seconds of the same link; the scale it ends at, and the changes.
    fn run(a: &mut AutoRes, start: f64, secs: usize, mut scale: u32, office: bool, link: impl Fn(u32) -> (f64, f64, f64, f64)) -> (u32, Vec<(f64, u32)>) {
        let mut changes = vec![];
        for i in 0..secs {
            let t = start + i as f64;
            let (rx, target, latency, fps) = link(scale);
            if let Some(d) = a.step(&sample(t, scale, rx, target, latency, fps), scale, office) {
                changes.push((t, d.scale));
                scale = d.scale;
            }
        }
        (scale, changes)
    }

    #[test]
    fn the_measured_relay_link_goes_to_half() {
        // "2250x1590 HEVC 28 fps latency 4672 ms ... 2.9 / 15 Mbps (game mode)"
        let mut a = AutoRes::default();
        let (end, changes) = run(&mut a, 0.0, 60, 75, false, |_| (2.9, 15.0, 4672.0, 60.0));
        assert_eq!(end, 50);
        assert_eq!(changes.len(), 1, "{changes:?}");
        // After ~3 s of evidence.
        assert!(changes[0].0 >= 2.0 && changes[0].0 <= 4.0, "{changes:?}");
    }

    #[test]
    fn a_lan_at_full_size_never_moves() {
        // 144 fps at 30 Mbps is only ~0.011 bpp at full size, but nothing queues.
        let mut a = AutoRes::default();
        let (end, changes) = run(&mut a, 0.0, 600, 100, false, |_| (12.0, 30.0, 18.0, 144.0));
        assert_eq!(end, 100);
        assert!(changes.is_empty());
    }

    #[test]
    fn up_again_once_the_link_clears() {
        let mut a = AutoRes::default();
        // Behind at 1 Mbps: down to 50 %.
        let (scale, _) = run(&mut a, 0.0, 30, 100, false, |_| (1.0, 15.0, 3000.0, 60.0));
        assert_eq!(scale, 50);
        // A good link again (60 Mbps target, quick): back up, a step at a time, 10 s apart at least.
        let (end, changes) = run(&mut a, 30.0, 60, scale, false, |_| (20.0, 60.0, 20.0, 60.0));
        assert_eq!(end, 100, "{changes:?}");
        assert_eq!(changes.len(), 2);
        assert!(changes[1].0 - changes[0].0 >= CHANGE_EVERY_S);
    }

    #[test]
    fn a_blind_host_target_backs_off_instead_of_flapping() {
        // The host's target stays at 20 Mbps whatever arrives; the link
        // carries 3: at 75 % it queues, at 50 % it keeps up.
        let mut a = AutoRes::default();
        let link = |scale: u32| if scale > 50 { (3.0, 20.0, 2000.0, 60.0) } else { (2.5, 20.0, 40.0, 60.0) };
        let (_, changes) = run(&mut a, 0.0, 600, 75, false, link);
        // Without the back-off: a change every ~13 s (~45 in 10 min).
        assert!(changes.len() <= 14,"{} changes: {changes:?}", changes.len());
        assert!(a.up_hold_s() > UP_AFTER_S);
    }

    #[test]
    fn a_lan_hiccup_comes_back_to_full_size() {
        // 144 fps, the host at 30 Mbps; five seconds of a stalled Wi-Fi.
        let mut a = AutoRes::default();
        let (scale, changes) = run(&mut a, 0.0, 5, 100, false, |_| (2.0, 30.0, 600.0, 144.0));
        assert_eq!(changes.len(), 1);
        let (end, _) = run(&mut a, 5.0, 120, scale, false, |_| (12.0, 30.0, 18.0, 144.0));
        assert_eq!(end, 100);
    }

    #[test]
    fn office_goes_down_to_a_third_on_one_megabit() {
        let mut a = AutoRes::default();
        let (end, _) = run(&mut a, 0.0, 60, 100, true, |_| (1.0, 15.0, 1500.0, 60.0));
        assert_eq!(end, 33);
    }

    #[test]
    fn no_pictures_no_evidence() {
        let mut a = AutoRes::default();
        for i in 0..30 {
            let mut s = sample(i as f64, 100, 0.0, 15.0, 5000.0, 60.0);
            s.shown_fps = 0.0;
            assert_eq!(a.step(&s, 100, false), None);
        }
    }

    #[test]
    fn a_brief_stall_is_ridden_out() {
        // Two seconds behind, then fine: no change.
        let mut a = AutoRes::default();
        let (end, changes) = run(&mut a, 0.0, 2, 100, false, |_| (1.0, 15.0, 3000.0, 60.0));
        let (end2, changes2) = run(&mut a, 2.0, 30, end, false, |_| (10.0, 15.0, 30.0, 60.0));
        assert!(changes.is_empty() && changes2.is_empty());
        assert_eq!(end2, 100);
    }
}
