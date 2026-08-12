//! Client UI window module.
//!
//! [`UiWindow`] is the client's native top-level window: a Win32 window with a
//! Direct3D 11 swapchain (the repository's existing `rdp-gpu` renderer) that
//! decoded frames are presented on. It is Windows-only — gated `#[cfg(windows)]`
//! so the client stays headless on Linux and in CI.
//!
//! The platform-neutral pieces ([`UiEvent`], [`WindowSize`]) live in this
//! module unconditionally so the run-loop / resize bookkeeping is unit-testable
//! on any host without a display.

/// The decision from one round of [`UiWindow::handle_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // event surface shared with the Windows connbar; exercised there
pub enum UiEvent {
    /// Keep the loop running. `resize` carries the new client-area size the
    /// swapchain was (or should be) resized to, if it changed.
    Continue { resize: Option<(u32, u32)> },
    /// The window is closing; the run loop must exit.
    Quit,
}

impl UiEvent {
    /// Whether the run loop should stop after this event.
    #[allow(dead_code)] // used by the Windows window/connbar run loops
    pub fn quit(&self) -> bool {
        matches!(self, UiEvent::Quit)
    }
}

/// Client-area size bookkeeping with the minimum clamp the swapchain and RDP
/// desktop share. Pure logic so it is testable without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // used by the Windows window sizing path
pub struct WindowSize {
    width: u32,
    height: u32,
}

impl WindowSize {
    /// A size clamped to at least 1×1 (both D3D11 swapchains and RDP desktops
    /// reject zero).
    #[allow(dead_code)] // used by the Windows window sizing path
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
        }
    }

    /// The current size as `(width, height)`.
    #[allow(dead_code)] // used by the Windows window sizing path
    pub fn get(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Apply a new size, returning `true` if anything actually changed. This is
    /// the bookkeeping the UI loop uses to decide whether the swapchain needs a
    /// resize before the next present.
    #[allow(dead_code)] // used by the Windows window sizing path
    pub fn set(&mut self, width: u32, height: u32) -> bool {
        let next = Self::new(width, height);
        if next == *self {
            return false;
        }
        *self = next;
        true
    }
}

#[cfg(windows)]
mod window;
#[cfg(windows)]
pub use window::UiWindow;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_size_clamps_to_minimum_and_reports_change() {
        let mut size = WindowSize::new(0, 0);
        assert_eq!(size.get(), (1, 1));

        // A real resize is detected…
        assert!(size.set(1920, 1080));
        assert_eq!(size.get(), (1920, 1080));

        // …but setting the same size again is a no-op.
        assert!(!size.set(1920, 1080));
        assert_eq!(size.get(), (1920, 1080));
    }

    #[test]
    fn ui_event_quit_semantics() {
        assert!(UiEvent::Quit.quit());
        assert!(!UiEvent::Continue { resize: None }.quit());
        assert!(!UiEvent::Continue {
            resize: Some((800, 600)),
        }
        .quit());
        // Events are copy + comparable, so the run loop can match on them.
        let ev = UiEvent::Continue {
            resize: Some((800, 600)),
        };
        assert_eq!(
            ev,
            UiEvent::Continue {
                resize: Some((800, 600))
            }
        );
    }

    #[test]
    fn continue_event_carries_the_resize_the_loop_should_apply() {
        let ev = UiEvent::Continue {
            resize: Some((2560, 1440)),
        };
        match ev {
            UiEvent::Continue {
                resize: Some((w, h)),
            } => assert_eq!((w, h), (2560, 1440)),
            _ => panic!("expected Continue with a resize"),
        }
    }

    /// Headless decode→UI handoff: decoded frames pushed through the bounded
    /// channel arrive at the UI side in order, and each converts to the exact
    /// present operation the window's swap chain applies. Runs without a
    /// display (CPU frames only — the GPU surface type cannot even be
    /// constructed on a non-Windows host).
    #[test]
    fn decoded_frames_flow_from_decoder_to_present_op() {
        use rdp_gpu::frame::{frame_channel, present_op, DecodedFrame, PresentOp};

        let (tx, rx) = frame_channel(4);
        // Decoder side: a burst of decoded frames (as the H.264 path emits).
        for i in 0..3i64 {
            let nv12 = vec![i as u8; 128 * 128 * 3 / 2];
            let frame = DecodedFrame::from_nv12(nv12, 128, 128, i).expect("valid NV12");
            assert!(tx.send(frame), "handoff queue has room");
        }
        // UI side: drain everything queued and convert each frame to its
        // present op — the exact dispatch `UiWindow::present_decoded` runs.
        let mut count = 0;
        while let Ok(frame) = rx.try_recv() {
            count += 1;
            match present_op(&frame) {
                PresentOp::Nv12 {
                    width,
                    height,
                    nv12,
                } => {
                    assert_eq!((width, height), (128, 128));
                    assert_eq!(nv12.len(), 128 * 128 * 3 / 2);
                }
                other => panic!("frame must convert to the Nv12 present op, got {other:?}"),
            }
        }
        assert_eq!(count, 3, "all three frames crossed the handoff");
    }
}
