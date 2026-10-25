//! What each of the Surface's buffers is missing, as rectangles.
//!
//! Android's own way (lock with one dirty rectangle) asks the app to repaint
//! the whole rectangle and copies the rest back from the previous buffer —
//! so two small changes far apart (a caret near the top, a status bar at the
//! bottom) repaint everything between them. Here each buffer keeps the list
//! of what changed since it was last painted, the app locks the whole buffer
//! (so Android copies nothing back) and paints just that list.
//!
//! Buffers are told apart by their memory address. A device that maps them
//! somewhere new each time would look like an endless chain of new buffers;
//! that's noticed ([`Damage::broken`]) and the caller goes back to Android's
//! way. No dependencies: `rustc --test -O damage.rs`.

/// (left, top, right, bottom), right and bottom exclusive.
pub type Rect = (u32, u32, u32, u32);

/// More rectangles than this in a list: it becomes their bounding box.
const MAX_RECTS: usize = 24;
/// More buffers than this seen: their addresses aren't stable.
const MAX_BUFFERS: usize = 6;

#[derive(Default)]
pub struct Damage {
    full: Rect,
    /// Changed since the last show.
    fresh: Vec<Rect>,
    /// Per buffer (its address): what changed since it was last painted.
    buffers: Vec<(usize, Vec<Rect>)>,
    /// Buffer addresses aren't stable: use Android's way.
    pub broken: bool,
}

fn area((l, t, r, b): Rect) -> u64 {
    (r.saturating_sub(l)) as u64 * (b.saturating_sub(t)) as u64
}

fn union(a: Rect, b: Rect) -> Rect {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

/// Add `r` to `list`, merging it with a rectangle it overlaps or touches
/// when that costs no more area than the two apart.
fn add_to(list: &mut Vec<Rect>, full: Rect, mut r: Rect) {
    if area(r) == 0 {
        return;
    }
    loop {
        let close = list.iter().position(|&o| {
            let u = union(o, r);
            let touch = r.0 <= o.2 && o.0 <= r.2 && r.1 <= o.3 && o.1 <= r.3;
            touch && area(u) <= area(o) + area(r)
        });
        match close {
            Some(i) => r = union(list.swap_remove(i), r),
            None => break,
        }
    }
    list.push(r);
    if list.len() > MAX_RECTS || list.iter().map(|&x| area(x)).sum::<u64>() >= area(full) {
        let b = list.iter().copied().reduce(union).unwrap_or(r);
        list.clear();
        list.push(b);
    }
}

impl Damage {
    /// A desktop (or Surface) of a new size: every buffer needs all of it.
    pub fn reset(&mut self, width: u32, height: u32) {
        self.full = (0, 0, width, height);
        self.fresh = vec![self.full];
        self.buffers.clear();
    }

    pub fn add(&mut self, r: Rect) {
        let r = (r.0.min(self.full.2), r.1.min(self.full.3), r.2.min(self.full.2), r.3.min(self.full.3));
        add_to(&mut self.fresh, self.full, r);
    }

    pub fn is_empty(&self) -> bool {
        self.fresh.is_empty()
    }

    /// Android's way: the bounding box of what changed (taken), and every
    /// buffer forgotten (Android tracks them meanwhile).
    pub fn take_bounds(&mut self) -> Option<Rect> {
        self.buffers.clear();
        let b = self.fresh.iter().copied().reduce(union);
        self.fresh.clear();
        b
    }

    /// What to paint into the buffer at `address` (all of it for one not seen
    /// before); it's then up to date.
    pub fn for_buffer(&mut self, address: usize) -> Vec<Rect> {
        let fresh = std::mem::take(&mut self.fresh);
        for (_, list) in &mut self.buffers {
            for &r in &fresh {
                add_to(list, self.full, r);
            }
        }
        match self.buffers.iter().position(|(a, _)| *a == address) {
            Some(i) => std::mem::take(&mut self.buffers[i].1),
            None => {
                self.buffers.push((address, Vec::new()));
                if self.buffers.len() > MAX_BUFFERS {
                    self.broken = true;
                }
                vec![self.full]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain of `n` buffers taken in turn, painted from `fb` as told; each
    /// must equal `fb` after its paint.
    fn simulate(n: usize, frames: &[Vec<Rect>], (w, h): (u32, u32)) -> (u64, u64) {
        let mut fb = vec![0u32; (w * h) as usize];
        let mut bufs = vec![vec![u32::MAX; (w * h) as usize]; n];
        let mut d = Damage::default();
        d.reset(w, h);
        let (mut painted, mut boxed) = (0u64, 0u64);
        for (f, rects) in frames.iter().enumerate() {
            for &(l, t, r, b) in rects {
                for y in t..b {
                    for x in l..r {
                        fb[(y * w + x) as usize] = f as u32 + 1;
                    }
                }
                d.add((l, t, r, b));
            }
            boxed += rects.iter().copied().reduce(union).map_or(0, area);
            let i = f % n;
            for (l, t, r, b) in d.for_buffer(1000 + i) {
                painted += area((l, t, r, b));
                for y in t..b {
                    for x in l..r {
                        bufs[i][(y * w + x) as usize] = fb[(y * w + x) as usize];
                    }
                }
            }
            assert!(bufs[i] == fb, "buffer {i} differs after frame {f}");
        }
        (painted, boxed)
    }

    #[test]
    fn every_buffer_ends_up_whole() {
        // Scattered changes, overlapping ones, the whole screen.
        let frames: Vec<Vec<Rect>> = (0..40u32)
            .map(|i| match i % 5 {
                0 => vec![(1, 1, 4, 3), (50, 40, 60, 48)],
                1 => vec![(2, 2, 10, 10), (5, 5, 20, 12)],
                2 => vec![(0, 0, 64, 48)],
                3 => (0..30).map(|k| (k * 2, k, k * 2 + 1, k + 1)).collect(),
                _ => vec![(i, i, i + 3, i + 2)],
            })
            .collect();
        for n in 1..=4 {
            simulate(n, &frames, (64, 48));
        }
    }

    #[test]
    fn unstable_addresses_are_noticed() {
        let mut d = Damage::default();
        d.reset(10, 10);
        for a in 0..10 {
            d.add((0, 0, 1, 1));
            d.for_buffer(a);
        }
        assert!(d.broken);
    }

    /// A/B: pixels painted per frame against the bounding box, for typing
    /// in a document on a 3000x2120 desktop (the line being typed, the word
    /// count in the status bar, the caret).
    #[test]
    #[ignore]
    fn bench() {
        let frames: Vec<Vec<Rect>> = (0..300u32)
            .map(|i| {
                let x = 400 + (i % 60) * 18;
                vec![(x, 600, x + 18, 630), (x + 18, 602, x + 20, 628), (40, 2060, 260, 2090)]
            })
            .collect();
        // (Counted without the pixel check: the simulation is slow at this size.)
        let mut d = Damage::default();
        d.reset(3000, 2120);
        let (mut painted, mut boxed) = (0u64, 0u64);
        for (f, rects) in frames.iter().enumerate() {
            for &r in rects {
                d.add(r);
            }
            boxed += rects.iter().copied().reduce(union).map_or(0, area);
            painted += d.for_buffer(f % 3).into_iter().map(area).sum::<u64>();
        }
        let n = frames.len() as f64;
        println!(
            "typing: bounding box {:.1} MB a frame, damage lists {:.2} MB a frame",
            boxed as f64 * 4.0 / n / 1e6,
            painted as f64 * 4.0 / n / 1e6
        );
    }
}
