//! Long-term reference frames, to repair a loss without a keyframe: the
//! host's plan for each picture (pure logic, from `rdp-client`'s host, so
//! `fecsim` runs the same rules). Measured on NVENC (H.264 and HEVC, through
//! Media Foundation): after frames were lost, a frame coded from a kept one
//! decodes exactly on the GPU decoder clients use, at the size of a normal
//! frame — where a keyframe is several times bigger, so on a lossy link it's
//! the frame most likely lost again, and it spikes the bitrate.
//!
//! Every quarter second a frame is kept, in two slots by turns. The client
//! reports the newest kept frame it decoded; a slot counts only once the
//! client has said so (one being refilled counts for nothing until then: the
//! client may still hold the old picture there). A loss is answered with a
//! frame coded from the newest confirmed one only; with none, or a third
//! repair within a second, a keyframe.

use std::time::{Duration, Instant};

/// Slots kept (`rdp_gpu::hyper::LTR_SLOTS`).
pub const SLOTS: usize = 2;

pub struct Ltr {
    /// The encoder (`rdp_gpu::hyper::HwEncoder::id`) these slots are in.
    pub encoder: u64,
    /// Per slot: the frame kept there, and whether the client has it.
    slots: [Option<(u32, bool)>; SLOTS],
    next_slot: usize,
    last_mark: Option<Instant>,
    /// Pictures (encoder inputs) told to be kept, with their slot.
    marking: Vec<(u64, usize)>,
    /// The picture coded as a repair.
    repairing: Option<u64>,
    repairs: Vec<Instant>,
}

pub enum LtrPlan {
    /// Keep the picture in slot `mark`; code it from the references in mask
    /// `from` only (a repair).
    Encode { mark: Option<u32>, from: Option<u32> },
    Keyframe,
}

impl Ltr {
    const EVERY: Duration = Duration::from_millis(250);
    const REPAIRS_PER_SEC: usize = 3;

    pub fn new(encoder: u64) -> Self {
        Self { encoder, slots: [None; SLOTS], next_slot: 0, last_mark: None, marking: Vec::new(), repairing: None, repairs: Vec::new() }
    }

    /// Before picture `input` goes in: what the encoder should do with it.
    /// `recover`: the client lost frames. `key`: a keyframe comes anyway
    /// (or the encoder keeps no references).
    pub fn plan(&mut self, input: u64, recover: bool, key: bool, now: Instant) -> LtrPlan {
        if key {
            return if recover { LtrPlan::Keyframe } else { LtrPlan::Encode { mark: None, from: None } };
        }
        if recover {
            self.repairs.retain(|t| now.duration_since(*t) < Duration::from_secs(1));
            let newest = (0..self.slots.len())
                .filter_map(|s| match self.slots[s] {
                    Some((id, true)) => Some((id, s)),
                    _ => None,
                })
                .max_by(|a, b| if newer(a.0, b.0) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less });
            return match newest {
                Some((_, slot)) if self.repairs.len() < Self::REPAIRS_PER_SEC => {
                    self.repairs.push(now);
                    self.repairing = Some(input);
                    LtrPlan::Encode { mark: None, from: Some(1 << slot) }
                }
                _ => {
                    self.repairs.clear();
                    LtrPlan::Keyframe
                }
            };
        }
        if self.last_mark.is_some_and(|t| now.duration_since(t) < Self::EVERY) {
            return LtrPlan::Encode { mark: None, from: None };
        }
        let slot = self.next_slot;
        self.next_slot = (slot + 1) % self.slots.len();
        self.slots[slot] = None;
        self.marking.push((input, slot));
        self.last_mark = Some(now);
        LtrPlan::Encode { mark: Some(slot as u32), from: None }
    }

    /// Picture `input` went out as frame `frame_id`: whether it's kept, and
    /// whether it's a repair.
    pub fn sent(&mut self, input: u64, frame_id: u32, keyframe: bool) -> (bool, bool) {
        if keyframe {
            // A keyframe clears every reference, the client's too.
            self.slots = [None; SLOTS];
            self.repairs.clear();
        }
        let kept = self.marking.iter().position(|&(i, _)| i == input).map(|p| self.marking.remove(p).1);
        self.marking.retain(|&(i, _)| i > input);
        let kept = kept.filter(|_| !keyframe).map(|slot| self.slots[slot] = Some((frame_id, false))).is_some();
        let repair = self.repairing == Some(input);
        if repair || self.repairing.is_some_and(|i| i < input) {
            self.repairing = None;
        }
        (kept, repair && !keyframe)
    }

    /// The client decoded kept frame `frame_id`.
    pub fn acked(&mut self, frame_id: u32) {
        for (id, acked) in self.slots.iter_mut().flatten() {
            if *id == frame_id {
                *acked = true;
            }
        }
    }
}

/// Frame `a` came after `b` (ids wrap).
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loss_is_repaired_from_a_reference_the_client_has() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut l = Ltr::new(1);
        let plain = |p: LtrPlan| matches!(p, LtrPlan::Encode { mark: None, from: None });
        // The first picture after a keyframe: no reference yet, a keyframe.
        assert!(matches!(l.plan(0, true, false, at(0)), LtrPlan::Keyframe));
        assert_eq!(l.sent(0, 100, true), (false, false));
        // Picture 1 kept in slot 0 (frame 101); the next ones not, for a while.
        assert!(matches!(l.plan(1, false, false, at(10)), LtrPlan::Encode { mark: Some(0), from: None }));
        assert_eq!(l.sent(1, 101, false), (true, false));
        assert!(plain(l.plan(2, false, false, at(20))));
        l.sent(2, 102, false);
        // Lost before the client said it has 101: a keyframe.
        assert!(matches!(l.plan(3, true, false, at(30)), LtrPlan::Keyframe));
        l.sent(3, 103, false);
        // The client confirms 101: a loss is repaired from slot 0.
        l.acked(101);
        assert!(matches!(l.plan(4, true, false, at(40)), LtrPlan::Encode { mark: None, from: Some(1) }));
        assert_eq!(l.sent(4, 104, false), (false, true));
        // A quarter second on, slot 1 is filled; unconfirmed, slot 0 still
        // repairs.
        assert!(matches!(l.plan(5, false, false, at(300)), LtrPlan::Encode { mark: Some(1), from: None }));
        assert_eq!(l.sent(5, 105, false), (true, false));
        assert!(matches!(l.plan(6, true, false, at(310)), LtrPlan::Encode { from: Some(1), .. }));
        l.sent(6, 106, false);
        // Confirmed: the newer slot 1.
        l.acked(105);
        assert!(matches!(l.plan(7, true, false, at(320)), LtrPlan::Encode { from: Some(2), .. }));
        l.sent(7, 107, false);
        // A third repair within the second: a keyframe instead.
        assert!(matches!(l.plan(8, true, false, at(330)), LtrPlan::Keyframe));
        l.sent(8, 108, true);
        // The keyframe cleared everything.
        assert!(matches!(l.plan(9, true, false, at(2000)), LtrPlan::Keyframe));
        // Refilling slot 0 drops what it held until confirmed again.
        let mut l = Ltr::new(1);
        l.plan(1, false, false, at(0));
        l.sent(1, 11, false);
        l.acked(11);
        l.plan(2, false, false, at(300));
        l.sent(2, 12, false);
        l.plan(3, false, false, at(600)); // slot 0 again
        l.sent(3, 13, false);
        assert!(matches!(l.plan(4, true, false, at(610)), LtrPlan::Keyframe), "11 was replaced; 12, 13 unconfirmed");
    }
}
