//! The present-path sink: where decoded frames go after the queue.
//!
//! [`FrameSink`] is the trait the UI window implements: each frame drained
//! from the [`crate::decode::queue::FrameQueue`] is handed to the sink, which
//! copies it into the D3D11 swapchain back buffer and presents it.
//! [`drain_and_present`] is the shared render-loop step — drain the queue,
//! repaint on every frame — so it is unit-testable without a display.
//! [`NullSink`] is the headless implementation: it drops every frame (there is
//! no swapchain) while counting them for diagnostics.

use super::frame::DecodedFrame;
use super::queue::FrameQueue;

/// The destination of decoded frames: the UI window's present path.
pub trait FrameSink {
    /// Errors from presenting (device loss, swapchain failure, ...).
    type Error: std::fmt::Display;

    /// Copy `frame` into the back buffer and present it. Called by the UI
    /// render loop for every frame drained from the queue.
    fn present(&mut self, frame: DecodedFrame) -> Result<(), Self::Error>;

    /// The current back-buffer size in pixels (0 × 0 when there is none).
    fn size(&self) -> (u32, u32);
}

/// Drain every queued frame into `sink` and repaint — the UI render loop's
/// repaint step. Returns the number of frames presented, or the first sink
/// error. Empty queues are a no-op, so callers can run this every iteration.
pub fn drain_and_present<S>(queue: &mut FrameQueue, sink: &mut S) -> Result<u64, S::Error>
where
    S: FrameSink,
{
    let mut presented = 0u64;
    while let Some(frame) = queue.pop() {
        sink.present(frame)?;
        presented += 1;
    }
    Ok(presented)
}

/// A sink that drops every frame — used by headless front-ends that have no
/// swapchain to present into. Counts the frames it was handed for telemetry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullSink {
    presented: u64,
}

impl NullSink {
    /// A new headless sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many frames were handed to this sink since creation.
    pub fn presented(&self) -> u64 {
        self.presented
    }
}

impl FrameSink for NullSink {
    type Error = std::convert::Infallible;

    fn present(&mut self, _frame: DecodedFrame) -> Result<(), Self::Error> {
        self.presented += 1;
        Ok(())
    }

    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test sink that records the frames it presented, in order.
    #[derive(Debug, Default)]
    struct RecordingSink {
        presented: Vec<u64>,
        size: (u32, u32),
    }

    impl FrameSink for RecordingSink {
        type Error = std::convert::Infallible;

        fn present(&mut self, frame: DecodedFrame) -> Result<(), Self::Error> {
            self.presented.push(frame.frame_number);
            Ok(())
        }

        fn size(&self) -> (u32, u32) {
            self.size
        }
    }

    /// A sink that fails on the `fail_at`-th present call.
    struct FailingSink {
        calls: u64,
        fail_at: u64,
        presented: Vec<u64>,
    }

    impl FailingSink {
        fn new(fail_at: u64) -> Self {
            Self {
                calls: 0,
                fail_at,
                presented: Vec::new(),
            }
        }
    }

    impl FrameSink for FailingSink {
        type Error = &'static str;

        fn present(&mut self, frame: DecodedFrame) -> Result<(), Self::Error> {
            self.calls += 1;
            if self.calls == self.fail_at {
                return Err("present failed");
            }
            self.presented.push(frame.frame_number);
            Ok(())
        }

        fn size(&self) -> (u32, u32) {
            (0, 0)
        }
    }

    fn frame(n: u64) -> DecodedFrame {
        DecodedFrame::solid(n, 2, 2, n == 0, [0, 0, 0, 255])
    }

    #[test]
    fn drain_and_present_delivers_all_frames_in_order() {
        let mut queue = FrameQueue::new(4);
        queue.push(frame(1));
        queue.push(frame(2));
        queue.push(frame(3));
        let mut sink = RecordingSink::default();
        let presented = drain_and_present(&mut queue, &mut sink).unwrap();
        assert_eq!(presented, 3);
        assert_eq!(sink.presented, [1, 2, 3]);
        assert!(queue.is_empty());
    }

    #[test]
    fn drain_and_present_on_an_empty_queue_is_a_noop() {
        let mut queue = FrameQueue::new(4);
        let mut sink = RecordingSink::default();
        assert_eq!(drain_and_present(&mut queue, &mut sink).unwrap(), 0);
        assert!(sink.presented.is_empty());
    }

    #[test]
    fn sink_errors_propagate_and_stop_the_drain() {
        let mut queue = FrameQueue::new(4);
        queue.push(frame(1));
        queue.push(frame(2));
        queue.push(frame(3));
        let mut sink = FailingSink::new(2);
        let err = drain_and_present(&mut queue, &mut sink).unwrap_err();
        assert_eq!(err, "present failed");
        // The first frame was presented, the second failed; the third stays
        // queued for the next drain attempt.
        assert_eq!(sink.presented, [1]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop().unwrap().frame_number, 3);
    }

    #[test]
    fn null_sink_counts_presented_frames() {
        let mut queue = FrameQueue::new(4);
        queue.push(frame(1));
        queue.push(frame(2));
        let mut sink = NullSink::new();
        assert_eq!(sink.size(), (0, 0));
        assert_eq!(drain_and_present(&mut queue, &mut sink).unwrap(), 2);
        assert_eq!(sink.presented(), 2);
        assert_eq!(sink.presented(), 2); // idempotent
    }

    #[test]
    fn render_loop_repaints_when_frames_arrive_and_idles_otherwise() {
        // The loop pattern: pump, drain, repaint. A steady producer keeps the
        // sink busy; an idle producer leaves it untouched.
        let mut queue = FrameQueue::new(4);
        let mut sink = RecordingSink::default();

        // No frames yet — no repaint.
        assert_eq!(drain_and_present(&mut queue, &mut sink).unwrap(), 0);

        // Frames arrive — every drained frame is repainted.
        for n in 1..=3 {
            queue.push(frame(n));
        }
        assert_eq!(drain_and_present(&mut queue, &mut sink).unwrap(), 3);
        assert_eq!(sink.presented, [1, 2, 3]);

        // Back to idle.
        assert_eq!(drain_and_present(&mut queue, &mut sink).unwrap(), 0);
    }
}
