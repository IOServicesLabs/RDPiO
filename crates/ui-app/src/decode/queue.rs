//! Bounded decoded-frame queue with drop-oldest backpressure.
//!
//! The decoder produces frames faster than the UI render loop can present
//! them, so the two are decoupled by a bounded queue. When the queue is full
//! the *oldest* frame is evicted to make room — the correct backpressure
//! policy for live video, where presenting stale frames is worse than skipping
//! them and memory must stay bounded no matter how fast the producer runs.
//! The render loop drains the queue every iteration and repaints whenever at
//! least one frame arrived.

use std::collections::VecDeque;

use super::frame::DecodedFrame;

/// The outcome of pushing a frame into a [`FrameQueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The frame was queued.
    Queued,
    /// The frame was queued and the oldest frame was evicted to make room
    /// (the queue was already at capacity).
    Evicted { evicted: DecodedFrame },
}

/// Aggregate queue telemetry for the UI loop / diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStats {
    /// Total frames pushed since creation.
    pub pushed: u64,
    /// Frames evicted because the queue was full (backpressure drops).
    pub dropped: u64,
    /// Frames currently queued.
    pub queued: usize,
    /// The queue capacity in frames.
    pub capacity: usize,
}

/// A bounded FIFO of decoded frames. Single-threaded by design: the UI loop
/// owns both the producer side (draining the decoder) and the consumer side
/// (draining into the sink), so no locking is required.
#[derive(Debug, Clone)]
pub struct FrameQueue {
    frames: VecDeque<DecodedFrame>,
    capacity: usize,
    pushed: u64,
    dropped: u64,
}

impl FrameQueue {
    /// A bounded queue holding at most `capacity` frames. A capacity of zero
    /// is clamped to one so the queue is always usable.
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            capacity: capacity.max(1),
            pushed: 0,
            dropped: 0,
        }
    }

    /// The maximum number of queued frames.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of frames currently queued.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether no frames are queued.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Whether the queue is at capacity.
    pub fn is_full(&self) -> bool {
        self.frames.len() >= self.capacity
    }

    /// Push a decoded frame. When the queue is full, the oldest frame is
    /// evicted (drop-oldest backpressure) so the render loop always sees the
    /// freshest frames and memory stays bounded.
    pub fn push(&mut self, frame: DecodedFrame) -> PushOutcome {
        self.pushed += 1;
        if self.is_full() {
            self.dropped += 1;
            let evicted = self
                .frames
                .pop_front()
                .expect("a full queue has an oldest frame");
            self.frames.push_back(frame);
            PushOutcome::Evicted { evicted }
        } else {
            self.frames.push_back(frame);
            PushOutcome::Queued
        }
    }

    /// Pop the oldest queued frame, if any.
    pub fn pop(&mut self) -> Option<DecodedFrame> {
        self.frames.pop_front()
    }

    /// Drain all queued frames in order, emptying the queue.
    pub fn drain(&mut self) -> Vec<DecodedFrame> {
        self.frames.drain(..).collect()
    }

    /// Drop every queued frame without presenting it.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Aggregate telemetry.
    pub fn stats(&self) -> FrameStats {
        FrameStats {
            pushed: self.pushed,
            dropped: self.dropped,
            queued: self.len(),
            capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u64) -> DecodedFrame {
        DecodedFrame::solid(n, 2, 2, n == 0, [0, 0, 0, 255])
    }

    #[test]
    fn queue_starts_empty_and_accepts_frames() {
        let mut q = FrameQueue::new(4);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.push(frame(1)), PushOutcome::Queued);
        assert!(!q.is_empty());
        assert_eq!(q.len(), 1);
        assert_eq!(q.stats().pushed, 1);
    }

    #[test]
    fn pop_returns_frames_in_order() {
        let mut q = FrameQueue::new(4);
        q.push(frame(1));
        q.push(frame(2));
        q.push(frame(3));
        assert_eq!(q.pop().unwrap().frame_number, 1);
        assert_eq!(q.pop().unwrap().frame_number, 2);
        assert_eq!(q.pop().unwrap().frame_number, 3);
        assert!(q.pop().is_none());
    }

    #[test]
    fn full_queue_evicts_the_oldest_frame() {
        let mut q = FrameQueue::new(2);
        q.push(frame(1));
        q.push(frame(2));
        let outcome = q.push(frame(3));
        assert_eq!(
            outcome,
            PushOutcome::Evicted {
                evicted: frame(1),
            }
        );
        assert_eq!(q.len(), 2);
        // The oldest surviving frame is #2; #1 was evicted.
        assert_eq!(q.pop().unwrap().frame_number, 2);
        assert_eq!(q.pop().unwrap().frame_number, 3);
    }

    #[test]
    fn backpressure_drops_are_counted() {
        let mut q = FrameQueue::new(2);
        for n in 1..=7 {
            q.push(frame(n));
        }
        let stats = q.stats();
        assert_eq!(stats.pushed, 7);
        assert_eq!(stats.dropped, 5);
        assert_eq!(stats.queued, 2);
        assert_eq!(stats.capacity, 2);
        // The five oldest frames were dropped; the two freshest remain.
        assert_eq!(q.pop().unwrap().frame_number, 6);
        assert_eq!(q.pop().unwrap().frame_number, 7);
    }

    #[test]
    fn memory_stays_bounded_under_a_fast_producer() {
        // A producer running far ahead of the consumer never grows the queue
        // past its capacity.
        let mut q = FrameQueue::new(4);
        for n in 0..10_000u64 {
            q.push(frame(n));
        }
        assert_eq!(q.len(), 4);
        assert_eq!(q.stats().dropped, 9_996);
    }

    #[test]
    fn drain_empties_the_queue_in_order() {
        let mut q = FrameQueue::new(4);
        q.push(frame(1));
        q.push(frame(2));
        let drained = q.drain();
        assert_eq!(drained.iter().map(|f| f.frame_number).collect::<Vec<_>>(), [1, 2]);
        assert!(q.is_empty());
        assert!(q.drain().is_empty());
    }

    #[test]
    fn clear_drops_queued_frames_without_counting_them_as_backpressure() {
        let mut q = FrameQueue::new(4);
        q.push(frame(1));
        q.push(frame(2));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.stats().dropped, 0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let mut q = FrameQueue::new(0);
        assert_eq!(q.capacity(), 1);
        q.push(frame(1));
        let outcome = q.push(frame(2));
        assert!(matches!(outcome, PushOutcome::Evicted { .. }));
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop().unwrap().frame_number, 2);
    }

    #[test]
    fn stats_track_pushed_and_queued() {
        let mut q = FrameQueue::new(3);
        q.push(frame(1));
        let s = q.stats();
        assert_eq!((s.pushed, s.queued, s.dropped), (1, 1, 0));
    }
}
