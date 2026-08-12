//! Decode → UI frame handoff.
//!
//! The H.264 decode path ([`crate::h264`]) runs on the session worker thread
//! and produces either GPU NV12 textures (zero-copy DXVA) or CPU NV12 buffers;
//! the UI window owns the D3D11 device and swapchain on the UI thread. This
//! module defines the [`DecodedFrame`] that crosses that boundary and the
//! bounded, strictly non-blocking channel ([`frame_channel`]) that carries it
//! — the decoder puts a frame on the channel and never waits for the UI to
//! present it.
//!
//! Everything here is deliberately platform-neutral: the GPU surface is a
//! `#[cfg(windows)]` alias for the D3D11 NV12 texture and an *uninhabited*
//! enum elsewhere, so the conversion/handoff logic stays compiled and
//! unit-tested on headless (Linux/CI) hosts. The Windows-only pieces are the
//! D3D11 type behind [`GpuSurface`] and the renderer calls the UI window makes
//! for the op produced by [`present_op`].

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

/// The D3D11 NV12 texture a GPU-decoded frame lives in (Windows only). On
/// other hosts this is an uninhabited placeholder: there is no GPU decode
/// surface to hand off on a headless host, so the [`DecodedSurface::Gpu`]
/// variant can never be constructed — but the handoff code around it still
/// compiles and is exercised by the CPU-path tests.
#[cfg(windows)]
pub type GpuSurface = windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

/// Non-Windows placeholder for [`GpuSurface`] (see above). An uninhabited enum
/// is the precise model of "no GPU surface exists here": it is a real type the
/// surrounding code can name and match, but no value of it can ever be made.
#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuSurface {}

/// Where a decoded frame's pixels live: on the GPU (zero-copy) or in a CPU
/// NV12 buffer.
pub enum DecodedSurface {
    /// A GPU-resident NV12 texture (D3D11 on Windows). Moving a frame across
    /// the handoff channel moves a COM reference — the pixels never touch the
    /// CPU. The renderer derives a shader-resource view from it on the UI
    /// thread, so the frame carries the texture, not an SRV.
    Gpu(GpuSurface),
    /// Tightly packed NV12 in system memory: a Y plane of `width*height` bytes
    /// followed by interleaved U/V of `width*height/2` bytes (stride == width).
    Cpu(Vec<u8>),
}

/// One decoded frame handed from the decode path to the UI, carrying its
/// pixels either as a GPU texture (zero-copy) or as CPU NV12.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// The input-unit tag this picture decodes (the MFT echoes each input
    /// sample's time onto its output picture), letting the consumer pair a
    /// frame with the metadata of the unit that *encoded* it. `-1` = the
    /// decoder didn't propagate a tag.
    pub unit_id: i64,
    pub surface: DecodedSurface,
}

impl DecodedFrame {
    /// Build a handoff frame from a tightly packed CPU NV12 buffer
    /// (`width*height*3/2` bytes). `None` if the buffer isn't exactly that
    /// size — a truncated or macroblock-padded NV12 frame must not reach the
    /// UI, which would otherwise mis-slice the Y/UV planes.
    pub fn from_nv12(nv12: Vec<u8>, width: u32, height: u32, unit_id: i64) -> Option<Self> {
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(3)
            / 2;
        if nv12.len() != expected {
            return None;
        }
        Some(Self {
            width,
            height,
            unit_id,
            surface: DecodedSurface::Cpu(nv12),
        })
    }

    /// Build a handoff frame from a GPU NV12 texture (Windows, zero-copy).
    #[cfg(windows)]
    pub fn from_gpu(texture: GpuSurface, width: u32, height: u32, unit_id: i64) -> Self {
        Self {
            width,
            height,
            unit_id,
            surface: DecodedSurface::Gpu(texture),
        }
    }

    /// The frame's size as `(width, height)`.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The Y and UV plane slices of a CPU-backed frame (the input layout
    /// [`rdp_graphics::yuv::nv12_to_rgba`] expects); `None` for a GPU frame,
    /// whose pixels never reach the CPU.
    pub fn planes(&self) -> Option<(&[u8], &[u8])> {
        match &self.surface {
            DecodedSurface::Cpu(nv12) => {
                let y_len = (self.width as usize) * (self.height as usize);
                Some(nv12.split_at(y_len))
            }
            DecodedSurface::Gpu(_) => None,
        }
    }
}

/// What the UI window's swap chain must do to present one [`DecodedFrame`],
/// derived by [`present_op`]. Kept platform-neutral so the frame→present
/// conversion is unit-testable without a display.
#[derive(Debug, Clone, Copy)]
pub enum PresentOp<'a> {
    /// Zero-copy: color-convert the GPU NV12 texture into the swapchain
    /// framebuffer (whole frame) and present. `texture` borrows the frame's
    /// surface; the UI thread owns all D3D11 objects, so no CPU copy happens.
    Gpu {
        texture: &'a GpuSurface,
        width: u32,
        height: u32,
    },
    /// Upload the CPU NV12 buffer, color-convert (GPU or CPU fallback), blit
    /// the whole frame into the swapchain framebuffer, and present.
    Nv12 {
        nv12: &'a [u8],
        width: u32,
        height: u32,
    },
}

/// Convert a decoded frame into the present operation the UI window applies.
/// This is the frame-conversion step the decode-to-UI handoff is built on:
/// GPU frames stay on the GPU (no CPU copy, no CPU conversion); CPU frames
/// carry their tightly packed NV12 for the UI thread to upload.
pub fn present_op(frame: &DecodedFrame) -> PresentOp<'_> {
    match &frame.surface {
        DecodedSurface::Gpu(texture) => PresentOp::Gpu {
            texture,
            width: frame.width,
            height: frame.height,
        },
        DecodedSurface::Cpu(nv12) => PresentOp::Nv12 {
            nv12,
            width: frame.width,
            height: frame.height,
        },
    }
}

/// The decode side of a [`frame_channel`] handoff. `send` is strictly
/// non-blocking: a backlogged UI causes the frame to be dropped (with a debug
/// trace — dropping a GPU frame just releases the COM reference back to the
/// decoder's texture-reuse pool) rather than stalling the decoder.
#[derive(Clone)]
pub struct FrameSender {
    tx: SyncSender<DecodedFrame>,
    capacity: usize,
}

impl FrameSender {
    /// Queue `frame` for the UI thread. Returns `true` when queued; `false`
    /// when the UI is backed up (the frame is dropped) or gone. Never blocks.
    pub fn send(&self, frame: DecodedFrame) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(frame)) => {
                tracing::debug!(
                    capacity = self.capacity,
                    "frame handoff queue full; dropping frame (UI must be slower than decode)"
                );
                drop(frame);
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }
}

/// The UI side of a [`frame_channel`] handoff. The UI thread drains this
/// before each present; the decoder never blocks on the drain.
pub struct FrameReceiver {
    rx: Receiver<DecodedFrame>,
}

impl FrameReceiver {
    /// The next queued frame, if any. Non-blocking: `Empty` means "nothing new
    /// since the last poll", `Disconnected` means the decoder went away.
    pub fn try_recv(&self) -> Result<DecodedFrame, TryRecvError> {
        self.rx.try_recv()
    }
}

/// Create a bounded decode→UI frame handoff channel. The decoder puts decoded
/// frames on it with [`FrameSender::send`] (never blocking) and the UI thread
/// drains it with [`FrameReceiver::try_recv`] before each present. `capacity`
/// bounds how many frames may be in flight (the decoder's texture-reuse pool
/// is sized to the same order of magnitude); anything beyond is dropped.
pub fn frame_channel(capacity: usize) -> (FrameSender, FrameReceiver) {
    let capacity = capacity.max(1);
    let (tx, rx) = mpsc::sync_channel(capacity);
    (FrameSender { tx, capacity }, FrameReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_nv12_accepts_exact_display_sized_buffer() {
        let (w, h) = (64u32, 48u32);
        let nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        let frame = DecodedFrame::from_nv12(nv12, w, h, 7).expect("exact NV12 is accepted");
        assert_eq!(frame.size(), (64, 48));
        assert_eq!(frame.unit_id, 7);
        assert!(matches!(frame.surface, DecodedSurface::Cpu(_)));
        // The Y/UV plane split lands exactly on the display-size boundary.
        let (y, uv) = frame.planes().expect("CPU frame has planes");
        assert_eq!(y.len(), (w * h) as usize);
        assert_eq!(uv.len(), (w * h / 2) as usize);
    }

    #[test]
    fn from_nv12_rejects_undersized_and_padded_buffers() {
        assert!(DecodedFrame::from_nv12(Vec::new(), 64, 48, 1).is_none());
        // One byte short of a full frame.
        let short = vec![0u8; 64 * 48 * 3 / 2 - 1];
        assert!(DecodedFrame::from_nv12(short, 64, 48, 1).is_none());
        // Macroblock-padded (oversized) buffers are rejected too — the handoff
        // contract is tightly packed display-size NV12.
        let padded = vec![0u8; 64 * 48 * 2];
        assert!(DecodedFrame::from_nv12(padded, 64, 48, 1).is_none());
    }

    #[test]
    fn channel_handoff_preserves_order_and_unit_tags() {
        let (tx, rx) = frame_channel(8);
        for i in 0..5i64 {
            let frame = DecodedFrame::from_nv12(vec![i as u8; 64 * 48 * 3 / 2], 64, 48, i).unwrap();
            assert!(tx.send(frame), "queue has room for all five frames");
        }
        for i in 0..5i64 {
            let frame = rx.try_recv().expect("frame is queued");
            assert_eq!(frame.unit_id, i, "handoff preserves decode order");
            assert!(matches!(frame.surface, DecodedSurface::Cpu(_)));
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn full_channel_drops_instead_of_blocking_the_decoder() {
        let (tx, rx) = frame_channel(2);
        // Fill the bounded queue.
        for _ in 0..2 {
            assert!(tx.send(DecodedFrame::from_nv12(vec![0; 96], 8, 8, 0).unwrap()));
        }
        // The decoder never blocks: a third frame is dropped, not queued.
        assert!(
            !tx.send(DecodedFrame::from_nv12(vec![0; 96], 8, 8, 1).unwrap()),
            "overflow frame must be dropped so the decoder never stalls"
        );
        // The two queued frames still drain in order.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn present_op_maps_cpu_frames_to_nv12_blit_args() {
        let nv12 = vec![7u8; 32 * 32 * 3 / 2];
        let frame = DecodedFrame::from_nv12(nv12.clone(), 32, 32, 3).unwrap();
        match present_op(&frame) {
            PresentOp::Nv12 {
                nv12: data,
                width,
                height,
            } => {
                assert_eq!((width, height), (32, 32));
                assert_eq!(
                    data,
                    &nv12[..],
                    "present op borrows the frame's NV12, no copy"
                );
            }
            other => panic!("CPU frame must convert to the Nv12 present op, got {other:?}"),
        }
    }

    #[test]
    fn handoff_survives_receiver_drop_without_blocking() {
        let (tx, rx) = frame_channel(4);
        drop(rx);
        // The UI went away; the decoder keeps going and simply reports the
        // channel is gone instead of blocking on a full queue.
        assert!(!tx.send(DecodedFrame::from_nv12(vec![0; 96], 8, 8, 0).unwrap()));
    }
}
