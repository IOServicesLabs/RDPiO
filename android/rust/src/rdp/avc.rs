//! H.264 for the Cloud PC's desktop (MS-RDPEGFX AVC420): MediaCodec decodes
//! into memory rather than onto the screen, because each picture is only
//! partly new — the server names the changed rectangles, and the rest of the
//! desktop comes from other codecs, so the compositor copies just those.
//!
//! One decoder per EGFX surface (each carries its own H.264 stream). The
//! server never sends B-frames, and with the low-latency keys a picture comes
//! out for each access unit, so `decode` waits for it: the caller paints it
//! before the commands after it.

use std::time::{Duration, Instant};

use ndk::media::media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;

/// `COLOR_FormatYUV420Planar` (I420) and `COLOR_FormatYUV420SemiPlanar` (NV12);
/// "flexible" asks the decoder for one of them in memory.
const I420: i32 = 19;
const NV12: i32 = 21;
const FLEXIBLE: i32 = 0x7F42_0888;

/// A decoded picture, borrowed from the decoder until the closure returns.
pub use super::yuv::Planes as Picture;

#[derive(Default, Clone, Copy)]
struct Layout {
    width: usize,
    height: usize,
    stride: usize,
    slice_height: usize,
    crop: (usize, usize),
    color: i32,
}

pub struct AvcDecoder {
    codec: MediaCodec,
    layout: Layout,
    pts: u64,
    name: String,
}

// Fed and drained from the one decode thread.
unsafe impl Send for AvcDecoder {}

impl AvcDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let codec = MediaCodec::from_decoder_type("video/avc").ok_or("no H.264 decoder")?;
        let mut format = MediaFormat::new();
        format.set_str("mime", "video/avc");
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
        format.set_i32("color-format", FLEXIBLE);
        format.set_i32("max-input-size", (width * height).max(1 << 20) as i32);
        format.set_i32("low-latency", 1);
        format.set_i32("priority", 0);
        // Flat out, or (that switch on) clocked for twice the desktop's 60
        // fps: each picture well within its frame, at lower clocks.
        format.set_i32("operating-rate", 120);
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        codec.configure(&format, None, MediaCodecDirection::Decoder).map_err(|e| format!("configure H.264: {e:?}"))?;
        codec.start().map_err(|e| format!("start H.264: {e:?}"))?;
        let name = codec.name().unwrap_or_default();
        log::info!("desktop H.264 decoder {name}: {width}x{height}");
        let layout = Layout { width: width as usize, height: height as usize, stride: width as usize, slice_height: height as usize, crop: (0, 0), color: NV12 };
        Ok(Self { codec, layout, pts: 0, name })
    }

    /// Decode one access unit and hand its picture to `paint` (not called
    /// when none came out: a parameter-set-only unit, or the decoder fell
    /// behind by more than `wait`).
    pub fn decode(&mut self, au: &[u8], wait: Duration, paint: impl FnOnce(&Picture)) -> Result<(), String> {
        self.pts += 33_333;
        let pts = self.pts;
        match self.codec.dequeue_input_buffer(wait).map_err(|e| format!("dequeue input: {e:?}"))? {
            DequeuedInputBufferResult::TryAgainLater => return Err("decoder input full".into()),
            DequeuedInputBufferResult::Buffer(mut buf) => {
                let room = buf.buffer_mut().len();
                if room < au.len() {
                    let _ = self.codec.queue_input_buffer(buf, 0, 0, pts, 0);
                    return Err(format!("access unit of {} bytes > input buffer {room}", au.len()));
                }
                let dst = buf.buffer_mut();
                unsafe { std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr() as *mut u8, au.len()) };
                self.codec.queue_input_buffer(buf, 0, au.len(), pts, 0).map_err(|e| format!("queue input: {e:?}"))?;
            }
        }
        let deadline = Instant::now() + wait;
        let mut paint = Some(paint);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.codec.dequeue_output_buffer(left).map_err(|e| format!("dequeue output: {e:?}"))? {
                DequeuedOutputBufferInfoResult::Buffer(buf) => {
                    let out_pts = buf.info().presentation_time_us() as u64;
                    let size = buf.info().size().max(0) as usize;
                    let offset = buf.info().offset().max(0) as usize;
                    if size > 0 {
                        if let Some(p) = paint.take_if(|_| out_pts == pts) {
                            let data = &buf.buffer()[offset.min(buf.buffer().len())..];
                            match self.picture(data) {
                                Some(pic) => p(&pic),
                                None => log::warn!("{}: picture smaller than its format ({} bytes)", self.name, data.len()),
                            }
                        }
                    }
                    self.codec.release_output_buffer(buf, false).map_err(|e| format!("release output: {e:?}"))?;
                    if out_pts >= pts {
                        return Ok(());
                    }
                }
                DequeuedOutputBufferInfoResult::OutputFormatChanged => self.read_format(),
                DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(()),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    fn read_format(&mut self) {
        let f = self.codec.output_format();
        let get = |k: &str| f.i32(k).filter(|&v| v > 0).map(|v| v as usize);
        let width = get("width").unwrap_or(self.layout.width);
        let height = get("height").unwrap_or(self.layout.height);
        let (left, top) = (f.i32("crop-left").unwrap_or(0).max(0) as usize, f.i32("crop-top").unwrap_or(0).max(0) as usize);
        let crop_w = f.i32("crop-right").map(|r| r as usize + 1 - left).unwrap_or(width);
        let crop_h = f.i32("crop-bottom").map(|b| b as usize + 1 - top).unwrap_or(height);
        self.layout = Layout {
            width: crop_w,
            height: crop_h,
            stride: get("stride").unwrap_or(width).max(width),
            slice_height: get("slice-height").unwrap_or(height).max(height),
            crop: (left, top),
            color: f.i32("color-format").unwrap_or(NV12),
        };
        log::info!("{}: output {f}", self.name);
    }

    fn picture<'a>(&self, data: &'a [u8]) -> Option<Picture<'a>> {
        let l = self.layout;
        let y_size = l.stride * l.slice_height;
        let (cx, cy) = l.crop;
        let y = data.get(cy * l.stride + cx..y_size)?;
        match l.color {
            I420 => {
                let cs = l.stride / 2;
                let csize = cs * (l.slice_height / 2);
                let coff = (cy / 2) * cs + cx / 2;
                let u = data.get(y_size + coff..y_size + csize)?;
                let v = data.get(y_size + csize + coff..y_size + 2 * csize)?;
                Some(Picture { width: l.width, height: l.height, y, y_stride: l.stride, u, v, uv_stride: cs, interleaved: false })
            }
            _ => {
                // NV12 and the vendor semi-planar formats laid out like it.
                let uv = data.get(y_size + (cy / 2) * l.stride + (cx & !1)..)?;
                Some(Picture { width: l.width, height: l.height, y, y_stride: l.stride, u: uv, v: &[], uv_stride: l.stride, interleaved: true })
            }
        }
    }
}

impl Drop for AvcDecoder {
    fn drop(&mut self) {
        let _ = self.codec.stop();
    }
}

/// Convert the picture's (`x`, `y`, `w`, `h`) into RGBA rows of `dst`
/// (`dst_stride` bytes apart, the rectangle's first pixel at `dst[0]`).
pub fn convert(pic: &Picture, x: usize, y: usize, w: usize, h: usize, dst: &mut [u8], dst_stride: usize) {
    super::yuv::convert_fast(pic, x, y, w, h, dst, dst_stride)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_a_rectangle_of_nv12() {
        // 4x2 picture: Y = 16 (black) except one 235 (white); neutral chroma.
        let y = [16, 16, 235, 16, 16, 16, 16, 16];
        let uv = [128u8; 4];
        let pic = Picture { width: 4, height: 2, y: &y, y_stride: 4, u: &uv, v: &[], uv_stride: 4, interleaved: true };
        let mut out = [0u8; 2 * 4];
        convert(&pic, 1, 0, 2, 1, &mut out, 8);
        assert_eq!(&out[..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..], &[255, 255, 255, 255]);
    }
}
