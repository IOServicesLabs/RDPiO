//! AVC420 / AVC444 bitstream framing for the EGFX H.264 path (MS-RDPEGFX
//! 2.2.4.4 / 2.2.4.5) plus H.264 Annex-B NAL-unit splitting.
//!
//! A `RFX_AVC420_BITMAP_STREAM` is a region metablock (the dirty rectangles and
//! their per-region QP/quality) followed by the raw H.264 (Annex-B) bitstream.
//! A `RFX_AVC444_BITMAP_STREAM` prefixes a 32-bit word whose top two bits select
//! which of the two luma/chroma sub-streams are present, then one or two AVC420
//! streams. This module is sans-I/O: it only splits the wire into regions and
//! NAL units; the actual H.264 decode happens on the GPU (Windows).

/// A dirty rectangle (RDPGFX_RECT16): right/bottom exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

/// Per-region quantization parameter + quality (RDPGFX_AVC420_QUANT_QUALITY).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantQuality {
    pub qp: u8,
    pub quality: u8,
}

/// A parsed AVC420 stream: its region metadata and the H.264 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avc420Stream<'a> {
    pub rects: Vec<Rect>,
    pub quants: Vec<QuantQuality>,
    pub h264: &'a [u8],
}

/// AVC444 luma/chroma stream selector (the top 2 bits of the length word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Avc444Lc {
    /// Both sub-streams present (stream1 = luma, stream2 = chroma).
    Both,
    /// Luma sub-stream only.
    LumaOnly,
    /// Chroma sub-stream only.
    ChromaOnly,
}

/// A parsed AVC444 stream: the LC selector and one or two AVC420 sub-streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avc444Stream<'a> {
    pub lc: Avc444Lc,
    pub stream1: Avc420Stream<'a>,
    pub stream2: Option<Avc420Stream<'a>>,
}

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Parse an `RFX_AVC420_BITMAP_STREAM`: the metablock followed by H.264 data.
pub fn parse_avc420(buf: &[u8]) -> Option<Avc420Stream<'_>> {
    let count = u32le(buf, 0)? as usize;
    let mut off = 4;
    let mut rects = Vec::with_capacity(count);
    for _ in 0..count {
        rects.push(Rect {
            left: u16le(buf, off)?,
            top: u16le(buf, off + 2)?,
            right: u16le(buf, off + 4)?,
            bottom: u16le(buf, off + 6)?,
        });
        off += 8;
    }
    let mut quants = Vec::with_capacity(count);
    for _ in 0..count {
        quants.push(QuantQuality {
            qp: *buf.get(off)?,
            quality: *buf.get(off + 1)?,
        });
        off += 2;
    }
    let h264 = buf.get(off..)?;
    Some(Avc420Stream {
        rects,
        quants,
        h264,
    })
}

/// Parse an `RFX_AVC444_BITMAP_STREAM`: a 32-bit `(LC, stream1 length)` word,
/// the first AVC420 stream, and — when `LC == 0` — a second one.
pub fn parse_avc444(buf: &[u8]) -> Option<Avc444Stream<'_>> {
    let word = u32le(buf, 0)?;
    let len1 = (word & 0x3FFF_FFFF) as usize;
    let lc = match word >> 30 {
        0 => Avc444Lc::Both,
        1 => Avc444Lc::LumaOnly,
        2 => Avc444Lc::ChromaOnly,
        _ => return None, // value 3 is reserved
    };
    let stream1_bytes = buf.get(4..4 + len1)?;
    let stream1 = parse_avc420(stream1_bytes)?;
    let stream2 = if lc == Avc444Lc::Both {
        Some(parse_avc420(buf.get(4 + len1..)?)?)
    } else {
        None
    };
    Some(Avc444Stream {
        lc,
        stream1,
        stream2,
    })
}

/// Split an H.264 Annex-B byte stream into its NAL units (without the 3- or
/// 4-byte start codes). Bytes before the first start code are ignored.
pub fn nal_units(h264: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= h264.len() {
        if h264[i] == 0 && h264[i + 1] == 0 && h264[i + 2] == 1 {
            starts.push((i, i + 3)); // (start-code offset, NAL payload offset)
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (idx, &(_, payload)) in starts.iter().enumerate() {
        // A NAL ends just before the next start code. A 4-byte start code is a
        // 3-byte one preceded by a 0x00, so trim a trailing zero before it.
        let end = if idx + 1 < starts.len() {
            let next_sc = starts[idx + 1].0;
            if next_sc > payload && h264[next_sc - 1] == 0 {
                next_sc - 1
            } else {
                next_sc
            }
        } else {
            h264.len()
        };
        if payload <= end {
            nals.push(&h264[payload..end]);
        }
    }
    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc420_metablock_then_h264() {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes()); // numRegionRects = 1
        b.extend_from_slice(&[0, 0, 0, 0, 0x80, 0x07, 0x38, 0x04]); // 0,0..1920,1080
        b.extend_from_slice(&[51, 100]); // qp/quality
        b.extend_from_slice(&[0x00, 0x00, 0x01, 0x67, 0xAA]); // H.264 (start code + NAL)
        let s = parse_avc420(&b).unwrap();
        assert_eq!(s.rects.len(), 1);
        assert_eq!(s.rects[0].right, 1920);
        assert_eq!(
            s.quants[0],
            QuantQuality {
                qp: 51,
                quality: 100
            }
        );
        assert_eq!(s.h264, &[0x00, 0x00, 0x01, 0x67, 0xAA]);
    }

    #[test]
    fn avc444_luma_only_has_one_substream() {
        // length word: LC=1 (luma only) in top 2 bits, len1 in low 30.
        let stream1 = {
            let mut b = Vec::new();
            b.extend_from_slice(&0u32.to_le_bytes()); // 0 region rects
            b.extend_from_slice(&[0x00, 0x00, 0x01, 0x65]); // H.264
            b
        };
        let mut buf = Vec::new();
        let word = (1u32 << 30) | (stream1.len() as u32);
        buf.extend_from_slice(&word.to_le_bytes());
        buf.extend_from_slice(&stream1);
        let s = parse_avc444(&buf).unwrap();
        assert_eq!(s.lc, Avc444Lc::LumaOnly);
        assert!(s.stream2.is_none());
        assert_eq!(s.stream1.h264, &[0x00, 0x00, 0x01, 0x65]);
    }

    #[test]
    fn avc444_both_splits_two_substreams() {
        let mk = |nal: u8| {
            let mut b = Vec::new();
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&[0x00, 0x00, 0x01, nal]);
            b
        };
        let s1 = mk(0x67);
        let s2 = mk(0x68);
        let mut buf = Vec::new();
        let word = s1.len() as u32; // LC=0 (top two bits clear) → both substreams
        buf.extend_from_slice(&word.to_le_bytes());
        buf.extend_from_slice(&s1);
        buf.extend_from_slice(&s2);
        let s = parse_avc444(&buf).unwrap();
        assert_eq!(s.lc, Avc444Lc::Both);
        assert_eq!(s.stream1.h264, &[0, 0, 1, 0x67]);
        assert_eq!(s.stream2.unwrap().h264, &[0, 0, 1, 0x68]);
    }

    #[test]
    fn nal_split_three_and_four_byte_start_codes() {
        // 4-byte start, NAL [0x67,0x42], then 3-byte start, NAL [0x68,0xCE].
        let stream = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // 4-byte start + NAL
            0x00, 0x00, 0x01, 0x68, 0xCE, // 3-byte start + NAL
        ];
        let nals = nal_units(&stream);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0xCE]);
    }

    #[test]
    fn nal_split_single_unit() {
        let stream = [0x00, 0x00, 0x01, 0x09, 0x10];
        assert_eq!(nal_units(&stream), vec![&[0x09, 0x10][..]]);
    }
}
