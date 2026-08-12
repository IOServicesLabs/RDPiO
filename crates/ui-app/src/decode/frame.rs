//! The decoded-frame type carried from the GPU decoder to the UI present path.
//!
//! [`DecodedFrame`] is the unit of work flowing through the decode-redirect
//! pipeline: the decoder (on Windows, the D3D11 hardware video decoder over
//! the existing GPU decode surfaces) produces one per access unit, the bounded
//! [`crate::decode::queue::FrameQueue`] carries it to the UI thread, and the
//! [`crate::decode::sink::FrameSink`] implementation on the UI window copies
//! it into the D3D11 swapchain back buffer and presents.
//!
//! The pixel payload is CPU-side BGRA8 in the swapchain's format. Converting
//! a GPU decode surface (NV12) into this format is a color-space copy, not a
//! software decode: the H.264 bitstream is never touched on the CPU.

/// One decoded frame ready for presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Monotonic frame sequence number assigned by the decoder/producer.
    pub frame_number: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// True when this frame is an IDR (a random-access point).
    pub is_keyframe: bool,
    /// The pixel payload for the present path.
    pub payload: FramePayload,
}

/// The pixel payload of a [`DecodedFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramePayload {
    /// CPU-side BGRA8 pixels, bottom-up, exactly `width * height * 4` bytes —
    /// ready to upload into the swapchain back buffer.
    Bgra8(Vec<u8>),
}

impl DecodedFrame {
    /// A BGRA8 frame with the given dimensions. `pixels` must be exactly
    /// `width * height * 4` bytes; the constructor enforces that and returns
    /// `None` on a mismatch, so malformed frames cannot enter the queue.
    pub fn bgra8(
        frame_number: u64,
        width: u32,
        height: u32,
        is_keyframe: bool,
        pixels: Vec<u8>,
    ) -> Option<Self> {
        let expected = (width as u64) * (height as u64) * 4;
        if pixels.len() as u64 != expected {
            return None;
        }
        Some(Self {
            frame_number,
            width,
            height,
            is_keyframe,
            payload: FramePayload::Bgra8(pixels),
        })
    }

    /// A solid-color frame (used by tests and headless harnesses).
    pub fn solid(
        frame_number: u64,
        width: u32,
        height: u32,
        is_keyframe: bool,
        rgba: [u8; 4],
    ) -> Self {
        let pixels = vec![rgba[0], rgba[1], rgba[2], rgba[3]]
            .into_iter()
            .cycle()
            .take((width as usize) * (height as usize) * 4)
            .collect();
        Self::bgra8(frame_number, width, height, is_keyframe, pixels)
            .expect("solid frame dimensions are exact")
    }

    /// The BGRA8 pixels, when the payload is CPU-side.
    pub fn pixels(&self) -> &[u8] {
        match &self.payload {
            FramePayload::Bgra8(pixels) => pixels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra8_accepts_exact_pixel_count() {
        let frame = DecodedFrame::bgra8(1, 2, 2, true, vec![0u8; 16]).expect("exact size");
        assert_eq!(frame.frame_number, 1);
        assert_eq!((frame.width, frame.height), (2, 2));
        assert!(frame.is_keyframe);
        assert_eq!(frame.pixels().len(), 16);
    }

    #[test]
    fn bgra8_rejects_mismatched_pixel_count() {
        assert!(DecodedFrame::bgra8(1, 2, 2, true, vec![0u8; 15]).is_none());
        assert!(DecodedFrame::bgra8(1, 0, 0, true, vec![0u8; 1]).is_none());
    }

    #[test]
    fn solid_frames_are_filled_with_the_color() {
        let frame = DecodedFrame::solid(7, 1, 2, false, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(frame.pixels(), &[0x11, 0x22, 0x33, 0x44, 0x11, 0x22, 0x33, 0x44]);
        assert!(!frame.is_keyframe);
    }
}
