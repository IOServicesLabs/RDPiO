//! The jitter buffer between the network and an audio device (the desktop's
//! WASAPI player, the Android client's AAudio output): played at a target
//! depth, with bursts and clock drift steered away by playing a fraction of
//! a percent faster or slower, and a standing backlog (a relay's queue let
//! go) cut out at a matched, crossfaded join.

use std::collections::VecDeque;

/// Frames a second.
const RATE: usize = 48_000;

/// Start playing once this much is queued (ms), then hold the queue's low
/// point there: room for the network's jitter, and no more. Where the
/// network's jitter is measured, the owner moves it ([`Player::set_target_ms`]).
pub const TARGET_MS: usize = 30;
/// A backlog this far past the target (a long stall's) is cut back to the
/// target at once.
const MAX_QUEUE_MS: usize = 60;
/// How far the low point may wander from the target before it's corrected.
const SLACK_MS: usize = 10;
/// How long the low point is watched before each decision.
const WINDOW_MS: usize = 500;
/// Corrections play at most 1/SKEW faster or slower: 0.5%, a pitch change of
/// 9 cents nobody hears, which still moves the level 5 ms a second.
const SKEW: usize = 200;
/// With catch-up on ([`Playout::catchup`]): a low point this far over the
/// target is cut out at once instead of played away — at 5 ms a second, the
/// 30-60 ms a bursty relay leaves behind would take 5-10 s to lose.
const SPLICE_OVER_MS: usize = 30;
/// A cut's crossfade, and how far from the exact cut point (either way) the
/// best-matching join is looked for.
const XFADE_MS: usize = 5;
const SEARCH_MS: usize = 5;

/// Frames in `ms` milliseconds.
pub const fn frames(ms: usize) -> usize {
    ms * RATE / 1000
}

/// The jitter buffer between the network and the device, in interleaved
/// 48 kHz frames. Playback starts once `TARGET_MS` is queued; a new player
/// drops any older backlog (what piled up while its device opened). After
/// that, bursts and drift between the sender's clock and the device's would
/// leave a lasting lag or run the queue dry, so the lowest level seen in each
/// `WINDOW_MS` steers it back to the target: playback runs a fraction of a
/// percent faster or slower, interpolated, so no sample is skipped or
/// repeated.
pub struct Playout {
    q: VecDeque<f32>,
    ch: usize,
    primed: bool,
    /// Lowest level (frames) at the start of a fill this window, counting
    /// corrections made since as if they had come before it.
    low: usize,
    /// Frames played this window.
    played: usize,
    /// Frames still to play away (> 0) or stretch in (< 0).
    pending: isize,
    /// Frames played toward the next corrected frame (the `SKEW` limit).
    budget: usize,
    /// The frame before the queue's front (interpolation looks one back).
    prev: [f32; 2],
    /// Playback has started once: only that first start drops a backlog.
    started: bool,
    /// The low point held (frames).
    pub target: usize,
    /// Times playback ran dry.
    pub underruns: u32,
    /// Catch up after a backlog by cutting it out, joined where the sound
    /// before and after the cut match best and crossfaded over 5 ms (no
    /// click), instead of an abrupt cut at the cap and a slow play-away
    /// below it. Small offsets are still steered. On by default; the owner
    /// turns it off for A/B tests (`catchup`).
    pub catchup: bool,
    /// Backlog cut out so far (frames), for the owner's stats.
    pub cut: u64,
}

impl Playout {
    pub fn new(ch: usize) -> Self {
        let ch = ch.clamp(1, 2);
        Self {
            q: VecDeque::with_capacity(frames(MAX_QUEUE_MS + 20) * ch),
            ch,
            primed: false,
            low: usize::MAX,
            played: 0,
            pending: 0,
            budget: 0,
            prev: [0.0; 2],
            started: false,
            target: frames(TARGET_MS),
            underruns: 0,
            catchup: true,
            cut: 0,
        }
    }

    /// Queued frames.
    pub fn level(&self) -> usize {
        self.q.len() / self.ch
    }

    /// A new watch window, with nothing left to correct.
    fn reset(&mut self) {
        self.low = usize::MAX;
        self.played = 0;
        self.pending = 0;
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.q.extend(samples.iter().copied());
        if self.level() > self.target + frames(MAX_QUEUE_MS) {
            let drop = self.level() - self.target;
            if self.catchup {
                self.splice(drop);
            } else {
                self.q.drain(..drop * self.ch);
                self.cut += drop as u64;
            }
            self.reset();
        }
    }

    /// Cut about `drop` frames from the front of the queue: the join is moved
    /// up to `SEARCH_MS` either way to where the sound after it best matches
    /// the sound before (a tone's phase, a beat), and crossfaded over
    /// `XFADE_MS`, so the cut is heard as nothing or a soft blur, never a
    /// click. Playback continues seamlessly from the frame before the queue.
    fn splice(&mut self, drop: usize) {
        let (ch, level, f) = (self.ch, self.level(), frames(XFADE_MS));
        let lo = drop.saturating_sub(frames(SEARCH_MS)).max(f);
        let hi = (drop + frames(SEARCH_MS)).min(level.saturating_sub(f));
        if lo > hi {
            let n = drop.min(level);
            self.q.drain(..n * ch);
            self.cut += n as u64;
            return;
        }
        let mono = |k: usize| (0..ch).map(|c| self.q[k * ch + c]).sum::<f32>();
        let head: Vec<f32> = (0..f).map(mono).collect();
        let tail: Vec<f32> = (lo..hi + f).map(mono).collect();
        // Normalized correlation of the head with each candidate join (the
        // exact cut point wins ties: digital silence, noise).
        let score = |k: usize| {
            let b = &tail[k - lo..k - lo + f];
            let (dot, energy) = head.iter().zip(b).fold((0.0f32, 0.0f32), |(d, e), (x, y)| (d + x * y, e + y * y));
            dot / (energy.sqrt() + 1e-6)
        };
        let target = drop.clamp(lo, hi);
        let mut best = (target, score(target));
        for k in lo..=hi {
            let s = score(k);
            if s > best.1 + 1e-4 {
                best = (k, s);
            }
        }
        let k = best.0;
        for i in 0..f {
            let w = 0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 0.5) / f as f32).cos();
            for c in 0..ch {
                let old = self.q[i * ch + c];
                let new = &mut self.q[(k + i) * ch + c];
                *new = old * (1.0 - w) + *new * w;
            }
        }
        self.q.drain(..k * ch);
        self.cut += k as u64;
    }

    /// Fill `out` (whole interleaved frames); returns how many samples came
    /// from the queue. The rest is silence: still priming, or it ran dry.
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let (ch, target) = (self.ch, self.target);
        let n = out.len() / ch;
        if !self.primed {
            if n == 0 || self.level() < target {
                out.fill(0.0);
                return 0;
            }
            // A new player starts at the target, not behind what queued
            // while its device opened. After running dry the late burst is
            // kept: dropping it would leave no room for the next stall like
            // it (underrun after underrun on a bursty link), and the steering
            // below plays it away.
            if !self.started {
                let backlog = (self.level() - target) * ch;
                self.q.drain(..backlog);
                self.started = true;
            }
            self.prev = [0.0; 2];
            self.primed = true;
            self.reset();
        }
        let level = self.level();
        self.low = self.low.min(level);
        self.played += n;
        self.budget = (self.budget + n).min(2 * SKEW);
        if self.played >= frames(WINDOW_MS) {
            // Correct once the low point is off by more than the slack, and
            // then all the way back.
            let error = self.low as isize - target as isize;
            let off = error.unsigned_abs();
            let pending = if off > frames(SLACK_MS) || (self.pending != 0 && off > frames(1)) { error } else { 0 };
            self.reset();
            // Far over (a relay's queue let go): cut it out now rather than
            // play it away for seconds. (The low point was a whole window's:
            // the backlog is standing, not a passing burst.)
            if self.catchup && error > frames(SPLICE_OVER_MS) as isize && level >= error as usize + n {
                self.splice(error as usize);
            } else {
                self.pending = pending;
            }
        }
        // This fill's share: toward the target only, within the skew budget,
        // and with enough queued to interpolate across.
        let step = (self.budget / SKEW) as isize;
        let mut d = match self.pending {
            p if p > 0 && level > target => p.min(step),
            p if p < 0 && level < target => p.max(-step),
            _ => 0,
        };
        if d.unsigned_abs() * 8 > n || level < (n as isize + d) as usize + 2 {
            d = 0;
        }
        let filled = if d == 0 { self.copy(out) } else { self.resample(out, d) };
        if d != 0 {
            self.pending -= d;
            self.budget -= d.unsigned_abs() * SKEW;
            self.low = self.low.saturating_add_signed(-d);
        }
        if filled < out.len() {
            // Ran dry: silence, and prime again before resuming so playback
            // doesn't stutter.
            out[filled..].fill(0.0);
            self.primed = false;
            self.underruns += 1;
        }
        filled
    }

    /// Play the next frames as they are; returns the samples filled.
    fn copy(&mut self, out: &mut [f32]) -> usize {
        let ch = self.ch;
        let take = (out.len() / ch).min(self.level()) * ch;
        if take > 0 {
            for c in 0..ch {
                self.prev[c] = self.q[take - ch + c];
            }
        }
        for (o, s) in out.iter_mut().zip(self.q.drain(..take)) {
            *o = s;
        }
        take
    }

    /// Play `n + d` queued frames as the `n` of `out`, interpolated, so the
    /// queue shrinks (`d` > 0) or grows (`d` < 0) by `d` frames. Each call
    /// starts exactly on a frame, so consecutive ones join without a seam.
    fn resample(&mut self, out: &mut [f32], d: isize) -> usize {
        let ch = self.ch;
        let n = out.len() / ch;
        let span = (n as isize + d) as usize;
        let (q, prev) = (&self.q, self.prev);
        // Frame `k - 1` of the queue; 0 is the one played before it.
        let at = |k: usize, c: usize| if k == 0 { prev[c] } else { q[(k - 1) * ch + c] };
        for j in 0..n {
            let pos = j * span;
            let (k, t) = (pos / n, (pos % n) as f32 / n as f32);
            for c in 0..ch {
                out[j * ch + c] = cubic(at(k, c), at(k + 1, c), at(k + 2, c), at(k + 3, c), t);
            }
        }
        for c in 0..ch {
            self.prev[c] = self.q[(span - 1) * ch + c];
        }
        self.q.drain(..span * ch);
        n * ch
    }
}

/// Catmull-Rom interpolation between `b` and `c` (`a` and `d` their outer
/// neighbours) at `t` in 0..1: exactly `b` at 0.
#[inline]
fn cubic(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    b + 0.5 * t * (c - a + t * (2.0 * a - 5.0 * b + 4.0 * c - d + t * (3.0 * (b - c) + d - a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 10 ms device period.
    const N: usize = 480;

    /// `n` frames of a 440 Hz tone on `ch` channels, from frame `*at` on.
    fn tone(at: &mut usize, n: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(n * ch);
        for _ in 0..n {
            let s = (0.5 * (*at as f64 * 440.0 * std::f64::consts::TAU / RATE as f64).sin()) as f32;
            v.extend(std::iter::repeat_n(s, ch));
            *at += 1;
        }
        v
    }

    /// The largest second difference of a mono signal: a pure tone's is
    /// tiny and steady, a skipped or repeated sample shows as a spike.
    fn roughness(x: &[f32]) -> f32 {
        x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
    }

    /// 440 Hz at 0.5: 0.5 * (2 pi 440 / 48000)^2 = 0.00166 per sample; a
    /// skip or repeat at the tone's steepest is ~0.029.
    const SMOOTH: f32 = 0.002;

    /// In steady state samples pass through untouched, once primed.
    #[test]
    fn steady_playback_is_bit_exact() {
        let mut p = Playout::new(2);
        let (mut at, mut sent, mut played) = (0, Vec::new(), Vec::new());
        let mut out = vec![0.0; N * 2];
        for _ in 0..300 {
            let chunk = tone(&mut at, N, 2);
            sent.extend_from_slice(&chunk);
            p.push(&chunk);
            let got = p.fill(&mut out);
            played.extend_from_slice(&out[..got]);
        }
        assert_eq!(played.len(), sent.len() - 2 * N * 2, "primed at the target, then one period each");
        assert!(played == sent[..played.len()]);
    }

    /// A burst that didn't run the queue dry is played away within seconds,
    /// back to the target, without a skipped or repeated sample.
    #[test]
    fn a_burst_is_played_away_smoothly() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        let mut level = 0;
        for i in 0..1500 {
            // 30 ms more than usual arrive at once after a second.
            p.push(&tone(&mut at, if i == 100 { N * 4 } else { N }, 1));
            level = p.level();
            let got = p.fill(&mut out);
            assert!(i < 2 || got == N, "ran dry at period {i}");
            played.extend_from_slice(&out[..got]);
        }
        assert!(level.abs_diff(frames(TARGET_MS)) <= frames(2), "level {level} frames");
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }

    /// Clock drift either way (~1000 ppm here, far more than real devices)
    /// keeps the queue near the target instead of climbing to the cap or
    /// running dry, and smoothly.
    #[test]
    fn drift_is_absorbed() {
        for extra in [1isize, -1] {
            let mut p = Playout::new(1);
            let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
            let (mut lo, mut hi) = (usize::MAX, 0);
            for i in 0..6000 {
                // One frame more (or fewer) every other period.
                let n = if i % 2 == 0 { (N as isize + extra) as usize } else { N };
                p.push(&tone(&mut at, n, 1));
                if i > 500 {
                    (lo, hi) = (lo.min(p.level()), hi.max(p.level()));
                }
                let got = p.fill(&mut out);
                // (Priming takes a period longer with the short pushes.)
                assert!(i < 3 || got == N, "ran dry at period {i} (drift {extra})");
                played.extend_from_slice(&out[..got]);
            }
            assert!(hi <= frames(TARGET_MS + SLACK_MS + 2), "drift {extra}: level rose to {hi} frames");
            assert!(lo >= frames(TARGET_MS - SLACK_MS - 2), "drift {extra}: level fell to {lo} frames");
            assert!(roughness(&played) < SMOOTH, "drift {extra}: roughness {}", roughness(&played));
        }
    }

    /// A new player starts at the target with the newest audio, not behind
    /// what queued while its device opened.
    #[test]
    fn a_startup_backlog_is_dropped() {
        let mut p = Playout::new(1);
        let mut out = vec![0.0; N];
        let ramp: Vec<f32> = (0..N * 6).map(|i| i as f32).collect();
        p.push(&ramp);
        assert_eq!(p.fill(&mut out), N);
        assert_eq!(out[0], (N * 3) as f32);
        assert_eq!(p.level(), frames(TARGET_MS) - N);
    }

    /// After running dry, playback resumes where it stopped: the burst a
    /// stall leaves is kept (room for the next stall like it) and played
    /// away back to the target, without running dry again.
    #[test]
    fn a_stall_backlog_is_kept_and_played_away() {
        let mut p = Playout::new(1);
        let mut out = vec![0.0; N];
        let ramp: Vec<f32> = (0..N * 9).map(|i| i as f32).collect();
        p.push(&ramp[..N * 3]);
        for _ in 0..3 {
            assert_eq!(p.fill(&mut out), N);
        }
        assert_eq!(p.fill(&mut out), 0, "dry");
        // A 60 ms burst after the stall: nothing of it is cut.
        p.push(&ramp[N * 3..]);
        assert_eq!(p.fill(&mut out), N);
        assert_eq!(out[0], (N * 3) as f32);
        assert_eq!(p.level(), N * 5);
        let mut level = 0;
        for i in 0..1500 {
            p.push(&[0.0; N]);
            level = p.level();
            assert_eq!(p.fill(&mut out), N, "ran dry at period {i}");
        }
        assert!(level.abs_diff(frames(TARGET_MS)) <= frames(2), "level {level} frames");
    }

    /// Bursts further apart than the target (here 40 ms of packets every
    /// 40 ms) play without gaps once the first has run dry (its start drops
    /// what's over the target): restarts keep what they need, and the queue
    /// is then stretched to hold its low point, seamlessly.
    #[test]
    fn slow_bursts_do_not_keep_running_dry() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        for i in 0..1000 {
            if i % 4 == 0 {
                p.push(&tone(&mut at, N * 4, 1));
            }
            let got = p.fill(&mut out);
            assert!(i < 4 || got == N, "ran dry at period {i}");
            played.extend_from_slice(&out[..got]);
        }
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }

    /// A backlog past the cap is cut back to the target as it arrives.
    #[test]
    fn the_queue_is_capped() {
        let mut p = Playout::new(2);
        p.push(&vec![0.0; frames(100) * 2]);
        assert_eq!(p.level(), frames(TARGET_MS));
    }

    /// Periods of a steady tone, with `extra` frames arriving at once at
    /// period 100, until the queue's level is back within 5 ms of the target
    /// (`None`: never, in 20 s), and the playback's roughness.
    fn catch_up(catchup: bool, extra: usize) -> (Option<usize>, f32) {
        let mut p = Playout::new(1);
        p.catchup = catchup;
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        let mut back = None;
        for i in 0..2000 {
            p.push(&tone(&mut at, if i == 100 { N + extra } else { N }, 1));
            if i > 100 && back.is_none() && p.level() <= p.target + frames(5) {
                back = Some(i - 100);
            }
            let got = p.fill(&mut out);
            assert!(i < 2 || got == N, "ran dry at period {i}");
            played.extend_from_slice(&out[..got]);
        }
        (back, roughness(&played))
    }

    /// A relay's queue letting go: 3 s of sound at once. Cut back to the
    /// target at once, either way; with catch-up the cut is a matched,
    /// crossfaded join (as smooth as steady playback), without it a click.
    #[test]
    fn a_relay_backlog_is_cut_out_without_a_click() {
        let (back, rough) = catch_up(true, frames(3000) + 37);
        let (old_back, old_rough) = catch_up(false, frames(3000) + 37);
        eprintln!("3 s burst: catch-up back in {back:?} periods, roughness {rough:.4}; before: {old_back:?} periods, roughness {old_rough:.4} (smooth < {SMOOTH})");
        assert!(back.is_some_and(|b| b <= 1), "back after {back:?} periods");
        assert!(rough < SMOOTH, "roughness {rough}");
        assert!(old_rough > SMOOTH, "the plain cut was smooth ({old_rough}): the test's tone no longer shows a click");
    }

    /// A standing backlog under the cap (45 ms over the target) is cut out
    /// after one watch window instead of being played away at 5 ms a second.
    #[test]
    fn a_standing_backlog_is_caught_up_within_a_second() {
        let (back, rough) = catch_up(true, frames(45));
        let (old_back, old_rough) = catch_up(false, frames(45));
        eprintln!("45 ms backlog: catch-up back in {back:?} periods (10 ms), roughness {rough:.4}; before: {old_back:?} periods, roughness {old_rough:.4}");
        assert!(back.is_some_and(|b| b <= 100), "back after {back:?} periods");
        assert!(old_back.is_none_or(|b| b > 300), "steering alone took only {old_back:?} periods");
        assert!(rough < SMOOTH, "roughness {rough}");
    }

    /// A cut joins where the sound matches: on a tone, the queue's front
    /// still plays on from the frame before it without a seam, and the
    /// level lands within the search distance of the target.
    #[test]
    fn a_splice_lands_near_the_target() {
        let mut p = Playout::new(2);
        let mut at = 0;
        p.push(&tone(&mut at, frames(30), 2));
        let mut out = vec![0.0; N * 2];
        assert_eq!(p.fill(&mut out), N * 2);
        p.push(&tone(&mut at, frames(200), 2));
        let level = p.level();
        assert!(level.abs_diff(p.target) <= frames(SEARCH_MS), "level {level}");
        assert_eq!(p.cut as usize + level, frames(220));
    }

    /// A lower target (a calm network measured) is reached within seconds,
    /// smoothly and without running dry; a higher one (jitter) the same way.
    #[test]
    fn the_target_moves_smoothly() {
        let mut p = Playout::new(1);
        let (mut at, mut played, mut out) = (0, Vec::new(), vec![0.0; N]);
        for (ms, from) in [(12, 0), (45, 1000)] {
            let mut level = 0;
            for i in from..from + 1000 {
                if i == from + 100 {
                    p.target = frames(ms);
                }
                p.push(&tone(&mut at, N, 1));
                level = p.level();
                let got = p.fill(&mut out);
                assert!(i < 2 || got == N, "ran dry at period {i}");
                played.extend_from_slice(&out[..got]);
            }
            assert!(level.abs_diff(frames(ms)) <= frames(2), "level {level} frames for {ms} ms");
        }
        assert_eq!(p.underruns, 0);
        assert!(roughness(&played) < SMOOTH, "roughness {}", roughness(&played));
    }
}
