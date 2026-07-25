//! Bitmap update parsing and pixel conversion for the legacy (non-RDPGFX) path.
//!
//! Parses the Bitmap Update PDU (TS_UPDATE_BITMAP, MS-RDPBCGR 2.2.9.1.1.3.1.2)
//! into rectangles and converts uncompressed pixel data (15/16/24/32 bpp) to
//! RGBA8 for upload to the GPU. Compressed rectangles are decoded by
//! [`decompress_interleaved`] (MS-RDPBCGR interleaved RLE) into the same raw
//! bottom-up layout before conversion.

/// `updateType` value for a Bitmap Update.
pub const UPDATETYPE_BITMAP: u16 = 0x0001;
/// Per-rectangle flag: the bitmap data is interleaved-RLE compressed.
pub const BITMAP_COMPRESSION: u16 = 0x0001;
/// Per-rectangle flag: no 8-byte compression header precedes the data.
pub const NO_BITMAP_COMPRESSION_HDR: u16 = 0x0400;

/// One rectangle from a Bitmap Update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapRect {
    pub dest_left: u16,
    pub dest_top: u16,
    pub dest_right: u16,
    pub dest_bottom: u16,
    pub width: u16,
    pub height: u16,
    pub bits_per_pixel: u16,
    pub compressed: bool,
    /// Pixel data: raw (bottom-up) if not compressed, else the interleaved-RLE
    /// stream (with any 8-byte compression header already stripped).
    pub data: Vec<u8>,
}

/// A parsed Bitmap Update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitmapUpdate {
    pub rectangles: Vec<BitmapRect>,
}

#[inline]
fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Parse a Bitmap Update from the update body (starting at the `updateType`
/// field of a slow-path Update PDU, or just after the fast-path update header).
pub fn parse_bitmap_update(body: &[u8]) -> Option<BitmapUpdate> {
    if body.len() < 4 || u16le(body, 0) != UPDATETYPE_BITMAP {
        return None;
    }
    let count = u16le(body, 2) as usize;
    let mut off = 4;
    let mut rectangles = Vec::with_capacity(count);

    for _ in 0..count {
        if off + 18 > body.len() {
            break;
        }
        let dest_left = u16le(body, off);
        let dest_top = u16le(body, off + 2);
        let dest_right = u16le(body, off + 4);
        let dest_bottom = u16le(body, off + 6);
        let width = u16le(body, off + 8);
        let height = u16le(body, off + 10);
        let bits_per_pixel = u16le(body, off + 12);
        let flags = u16le(body, off + 14);
        let bitmap_length = u16le(body, off + 16) as usize;
        off += 18;

        if off + bitmap_length > body.len() {
            break;
        }
        let mut blob = &body[off..off + bitmap_length];
        off += bitmap_length;

        let compressed = flags & BITMAP_COMPRESSION != 0;
        // Strip the 8-byte TS_CD_HEADER when present (compressed + header flag clear).
        if compressed && flags & NO_BITMAP_COMPRESSION_HDR == 0 && blob.len() >= 8 {
            blob = &blob[8..];
        }

        rectangles.push(BitmapRect {
            dest_left,
            dest_top,
            dest_right,
            dest_bottom,
            width,
            height,
            bits_per_pixel,
            compressed,
            data: blob.to_vec(),
        });
    }

    Some(BitmapUpdate { rectangles })
}

/// Convert uncompressed RDP pixel data to RGBA8 (4 bytes/pixel, top-down).
///
/// RDP uncompressed bitmap rows are stored bottom-up, so set `bottom_up` to flip
/// them into top-down display order. Supports 15/16/24/32 bits per pixel.
pub fn to_rgba(data: &[u8], width: u16, height: u16, bpp: u16, bottom_up: bool) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    // Defensive cap: refuse absurd dimensions a malformed server could claim,
    // so we never attempt a huge allocation (the `data.len()` check below also
    // bounds it, but reject early).
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return None;
    }
    let bytes_per_pixel = match bpp {
        32 => 4,
        24 => 3,
        16 | 15 => 2,
        _ => return None,
    };
    if data.len() < w * h * bytes_per_pixel {
        return None;
    }

    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let src_row = if bottom_up { h - 1 - row } else { row };
        for col in 0..w {
            let si = (src_row * w + col) * bytes_per_pixel;
            let (r, g, b) = match bpp {
                32 => (data[si + 2], data[si + 1], data[si]), // BGRA -> RGB
                24 => (data[si + 2], data[si + 1], data[si]), // BGR  -> RGB
                16 => {
                    let v = u16::from_le_bytes([data[si], data[si + 1]]);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    (
                        (r << 3) | (r >> 2),
                        (g << 2) | (g >> 4),
                        (b << 3) | (b >> 2),
                    )
                }
                _ => {
                    // 15bpp RGB555
                    let v = u16::from_le_bytes([data[si], data[si + 1]]);
                    let r = ((v >> 10) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x1f) as u8;
                    let b = (v & 0x1f) as u8;
                    (
                        (r << 3) | (r >> 2),
                        (g << 3) | (g >> 2),
                        (b << 3) | (b >> 2),
                    )
                }
            };
            let di = (row * w + col) * 4;
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = 0xFF;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Interleaved-RLE decompression (MS-RDPBCGR 2.2.9.1.1.3.1.2.4).
//
// The compressed stream is a sequence of orders. Each order's header byte
// selects a "code" (run/image kind) and, for most kinds, a run length. Pixels
// are reconstructed relative to the previously decoded scanline (the pixel
// directly "above" in the bottom-up output buffer): background pixels copy the
// pixel above, foreground pixels are `above XOR fgPel`. Color runs/images carry
// absolute pixels. On the first scanline there is no pixel above, so "above" is
// treated as 0 — which makes background = black and foreground = fgPel, exactly
// per spec. Supported pixel sizes: 1/2/3 bytes (8/15-16/24 bpp).
// ---------------------------------------------------------------------------

// Code identifiers returned by `extract_code_id`.
const C_BG_RUN: u8 = 0x00;
const C_FG_RUN: u8 = 0x01;
const C_FGBG_IMAGE: u8 = 0x02;
const C_COLOR_RUN: u8 = 0x03;
const C_COLOR_IMAGE: u8 = 0x04;
const C_LITE_SET_FG_FG_RUN: u8 = 0x0C;
const C_LITE_SET_FG_FGBG_IMAGE: u8 = 0x0D;
const C_LITE_DITHERED_RUN: u8 = 0x0E;
const C_MEGA_BG_RUN: u8 = 0xF0;
const C_MEGA_FG_RUN: u8 = 0xF1;
const C_MEGA_FGBG_IMAGE: u8 = 0xF2;
const C_MEGA_COLOR_RUN: u8 = 0xF3;
const C_MEGA_COLOR_IMAGE: u8 = 0xF4;
const C_MEGA_SET_FG_RUN: u8 = 0xF6;
const C_MEGA_SET_FGBG_IMAGE: u8 = 0xF7;
const C_MEGA_DITHERED_RUN: u8 = 0xF8;
const C_SPECIAL_FGBG_1: u8 = 0xF9;
const C_SPECIAL_FGBG_2: u8 = 0xFA;
const C_SPECIAL_WHITE: u8 = 0xFD;
const C_SPECIAL_BLACK: u8 = 0xFE;

/// Classify an order header byte into a code identifier.
fn extract_code_id(h: u8) -> u8 {
    if h & 0xC0 != 0xC0 {
        h >> 5 // REGULAR (0..=5): code in the top 3 bits
    } else if h & 0x30 != 0x30 {
        h >> 4 // LITE (0xC/0xD/0xE): code in the top 4 bits
    } else {
        h // MEGA_MEGA / SPECIAL (0xF0..=0xFF): full byte
    }
}

/// Decode the run length for `code` starting at `src[s]`. Returns
/// `(run_length, advance)` where `advance` is the number of header bytes
/// consumed. `None` if the stream is too short.
fn extract_run_length(code: u8, src: &[u8], s: usize) -> Option<(usize, usize)> {
    let h = *src.get(s)?;
    match code {
        C_FGBG_IMAGE => {
            let r = (h & 0x1F) as usize;
            if r == 0 {
                Some((*src.get(s + 1)? as usize + 1, 2))
            } else {
                Some((r * 8, 1))
            }
        }
        C_LITE_SET_FG_FGBG_IMAGE => {
            let r = (h & 0x0F) as usize;
            if r == 0 {
                Some((*src.get(s + 1)? as usize + 1, 2))
            } else {
                Some((r * 8, 1))
            }
        }
        C_BG_RUN | C_FG_RUN | C_COLOR_RUN | C_COLOR_IMAGE => {
            let r = (h & 0x1F) as usize;
            if r == 0 {
                Some((*src.get(s + 1)? as usize + 32, 2))
            } else {
                Some((r, 1))
            }
        }
        C_LITE_SET_FG_FG_RUN | C_LITE_DITHERED_RUN => {
            let r = (h & 0x0F) as usize;
            if r == 0 {
                Some((*src.get(s + 1)? as usize + 16, 2))
            } else {
                Some((r, 1))
            }
        }
        C_MEGA_BG_RUN
        | C_MEGA_FG_RUN
        | C_MEGA_SET_FG_RUN
        | C_MEGA_DITHERED_RUN
        | C_MEGA_COLOR_RUN
        | C_MEGA_FGBG_IMAGE
        | C_MEGA_SET_FGBG_IMAGE
        | C_MEGA_COLOR_IMAGE => {
            let lo = *src.get(s + 1)? as usize;
            let hi = *src.get(s + 2)? as usize;
            Some((lo | (hi << 8), 3))
        }
        _ => None,
    }
}

/// The pixel `p` bytes directly above `d` in the bottom-up output, or zeros for
/// the first scanline (no previous line).
fn above_pel(dst: &[u8], d: usize, row_delta: usize, p: usize) -> [u8; 4] {
    let mut a = [0u8; 4];
    if d >= row_delta {
        a[..p].copy_from_slice(&dst[d - row_delta..d - row_delta + p]);
    }
    a
}

/// Write one pixel `pel` at the cursor, advancing it. `None` on overflow.
fn write_abs(dst: &mut [u8], d: &mut usize, p: usize, pel: &[u8]) -> Option<()> {
    if *d + p > dst.len() {
        return None;
    }
    dst[*d..*d + p].copy_from_slice(&pel[..p]);
    *d += p;
    Some(())
}

/// Write a background pixel (copy the pixel above), advancing the cursor.
fn write_bg(dst: &mut [u8], d: &mut usize, row_delta: usize, p: usize) -> Option<()> {
    if *d + p > dst.len() {
        return None;
    }
    let a = above_pel(dst, *d, row_delta, p);
    dst[*d..*d + p].copy_from_slice(&a[..p]);
    *d += p;
    Some(())
}

/// Write a foreground pixel (`above XOR fg`), advancing the cursor.
fn write_fg(dst: &mut [u8], d: &mut usize, row_delta: usize, p: usize, fg: &[u8]) -> Option<()> {
    if *d + p > dst.len() {
        return None;
    }
    let a = above_pel(dst, *d, row_delta, p);
    for i in 0..p {
        dst[*d + i] = a[i] ^ fg[i];
    }
    *d += p;
    Some(())
}

/// Write `cbits` pixels using one FGBG bitmask byte (bit set → foreground).
fn write_fgbg(
    dst: &mut [u8],
    d: &mut usize,
    row_delta: usize,
    p: usize,
    fg: &[u8],
    mask: u8,
    cbits: usize,
) -> Option<()> {
    for i in 0..cbits {
        if (mask >> i) & 1 == 1 {
            write_fg(dst, d, row_delta, p, fg)?;
        } else {
            write_bg(dst, d, row_delta, p)?;
        }
    }
    Some(())
}

/// Decompress an interleaved-RLE bitmap rectangle into raw (bottom-up) pixel
/// bytes — the same layout as an uncompressed rect, ready for [`to_rgba`].
/// Returns `None` for unsupported pixel sizes or a malformed (overrunning)
/// stream. Supports 8/15/16/24 bpp (1/2/3 bytes per pixel).
pub fn decompress_interleaved(src: &[u8], width: u16, height: u16, bpp: u16) -> Option<Vec<u8>> {
    let p: usize = match bpp {
        8 => 1,
        15 | 16 => 2,
        24 => 3,
        _ => return None,
    };
    let (w, h) = (width as usize, height as usize);
    let total = w.checked_mul(h)?.checked_mul(p)?;
    if total == 0 {
        return Some(Vec::new());
    }
    let row_delta = w * p;
    let mut dst = vec![0u8; total];
    let mut d = 0usize;
    let mut s = 0usize;
    let mut fg = vec![0xFFu8; p]; // foreground pel, initialised to white
    let white = vec![0xFFu8; p];
    let black = vec![0x00u8; p];
    let mut prev_bg = false;

    while s < src.len() {
        let code = extract_code_id(src[s]);
        match code {
            C_BG_RUN | C_MEGA_BG_RUN => {
                let (mut run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                // Two adjacent background runs are separated by one foreground
                // pixel that the compressor omitted (it would otherwise merge
                // them); re-insert it and shorten the run by one.
                if prev_bg {
                    write_fg(&mut dst, &mut d, row_delta, p, &fg)?;
                    run = run.saturating_sub(1);
                }
                for _ in 0..run {
                    write_bg(&mut dst, &mut d, row_delta, p)?;
                }
                prev_bg = true;
                continue;
            }
            C_FG_RUN | C_MEGA_FG_RUN | C_LITE_SET_FG_FG_RUN | C_MEGA_SET_FG_RUN => {
                let (run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                if matches!(code, C_LITE_SET_FG_FG_RUN | C_MEGA_SET_FG_RUN) {
                    fg = src.get(s..s + p)?.to_vec();
                    s += p;
                }
                for _ in 0..run {
                    write_fg(&mut dst, &mut d, row_delta, p, &fg)?;
                }
            }
            C_COLOR_RUN | C_MEGA_COLOR_RUN => {
                let (run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                let color = src.get(s..s + p)?.to_vec();
                s += p;
                for _ in 0..run {
                    write_abs(&mut dst, &mut d, p, &color)?;
                }
            }
            C_COLOR_IMAGE | C_MEGA_COLOR_IMAGE => {
                let (run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                for _ in 0..run {
                    let px = src.get(s..s + p)?.to_vec();
                    s += p;
                    write_abs(&mut dst, &mut d, p, &px)?;
                }
            }
            C_FGBG_IMAGE | C_MEGA_FGBG_IMAGE | C_LITE_SET_FG_FGBG_IMAGE | C_MEGA_SET_FGBG_IMAGE => {
                let (run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                if matches!(code, C_LITE_SET_FG_FGBG_IMAGE | C_MEGA_SET_FGBG_IMAGE) {
                    fg = src.get(s..s + p)?.to_vec();
                    s += p;
                }
                let mut remaining = run;
                while remaining > 0 {
                    let cbits = remaining.min(8);
                    let mask = *src.get(s)?;
                    s += 1;
                    write_fgbg(&mut dst, &mut d, row_delta, p, &fg, mask, cbits)?;
                    remaining -= cbits;
                }
            }
            C_LITE_DITHERED_RUN | C_MEGA_DITHERED_RUN => {
                let (run, adv) = extract_run_length(code, src, s)?;
                s += adv;
                let pel1 = src.get(s..s + p)?.to_vec();
                s += p;
                let pel2 = src.get(s..s + p)?.to_vec();
                s += p;
                for _ in 0..run {
                    write_abs(&mut dst, &mut d, p, &pel1)?;
                    write_abs(&mut dst, &mut d, p, &pel2)?;
                }
            }
            C_SPECIAL_FGBG_1 => {
                s += 1;
                write_fgbg(&mut dst, &mut d, row_delta, p, &fg, 0x03, 8)?;
            }
            C_SPECIAL_FGBG_2 => {
                s += 1;
                write_fgbg(&mut dst, &mut d, row_delta, p, &fg, 0x05, 8)?;
            }
            C_SPECIAL_WHITE => {
                s += 1;
                write_abs(&mut dst, &mut d, p, &white)?;
            }
            C_SPECIAL_BLACK => {
                s += 1;
                write_abs(&mut dst, &mut d, p, &black)?;
            }
            _ => return None,
        }
        prev_bg = false;
    }

    Some(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_uncompressed_rect() {
        // updateType=1, count=1, then an 18-byte rect header + 16 bytes (2x2 @32bpp).
        let mut body = vec![0x01, 0x00, 0x01, 0x00];
        body.extend_from_slice(&[
            0x00, 0x00, // destLeft
            0x00, 0x00, // destTop
            0x01, 0x00, // destRight
            0x01, 0x00, // destBottom
            0x02, 0x00, // width
            0x02, 0x00, // height
            0x20, 0x00, // bitsPerPixel = 32
            0x00, 0x00, // flags (uncompressed)
            0x10, 0x00, // bitmapLength = 16
        ]);
        body.extend_from_slice(&[0u8; 16]);

        let update = parse_bitmap_update(&body).unwrap();
        assert_eq!(update.rectangles.len(), 1);
        let r = &update.rectangles[0];
        assert_eq!((r.width, r.height, r.bits_per_pixel), (2, 2, 32));
        assert!(!r.compressed);
        assert_eq!(r.data.len(), 16);
    }

    #[test]
    fn compressed_rect_strips_8byte_header() {
        let mut body = vec![0x01, 0x00, 0x01, 0x00];
        body.extend_from_slice(&[
            0, 0, 0, 0, 0, 0, 0, 0, // dest rect
            0x04, 0x00, // width
            0x01, 0x00, // height
            0x10, 0x00, // 16 bpp
            0x01, 0x00, // flags = BITMAP_COMPRESSION (header present)
            0x0C, 0x00, // bitmapLength = 12 (8 hdr + 4 data)
        ]);
        body.extend_from_slice(&[0xAA; 8]); // compression header (stripped)
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // RLE data
        let update = parse_bitmap_update(&body).unwrap();
        let r = &update.rectangles[0];
        assert!(r.compressed);
        assert_eq!(r.data, vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn to_rgba_32bpp_bgra_to_rgba() {
        // One pixel, BGRA = (B=0x10, G=0x20, R=0x30, A=0xFF).
        let px = to_rgba(&[0x10, 0x20, 0x30, 0xFF], 1, 1, 32, false).unwrap();
        assert_eq!(px, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    #[test]
    fn to_rgba_bottom_up_flips_rows() {
        // 1x2 @32bpp: row0 = red, row1 = blue (BGRA). bottom_up flips them.
        let data = [
            0x00, 0x00, 0xFF, 0xFF, // row0 BGRA = red
            0xFF, 0x00, 0x00, 0xFF, // row1 BGRA = blue
        ];
        let px = to_rgba(&data, 1, 2, 32, true).unwrap();
        // Top-down output: top row should be the *last* source row (blue).
        assert_eq!(&px[0..4], &[0x00, 0x00, 0xFF, 0xFF]); // blue
        assert_eq!(&px[4..8], &[0xFF, 0x00, 0x00, 0xFF]); // red
    }

    #[test]
    fn to_rgba_rejects_short_data() {
        assert!(to_rgba(&[0u8; 3], 2, 2, 32, false).is_none());
    }

    // --- interleaved-RLE decompression ---------------------------------------

    #[test]
    fn rle_color_image_copies_literal_pixels() {
        // REGULAR_COLOR_IMAGE (code 4 → 0x80) | runLength 4 = 0x84, then 4×3 bytes.
        let mut s = vec![0x84];
        let pixels = [
            1, 2, 3, // px0
            4, 5, 6, // px1
            7, 8, 9, // px2
            10, 11, 12, // px3
        ];
        s.extend_from_slice(&pixels);
        let out = decompress_interleaved(&s, 4, 1, 24).unwrap();
        assert_eq!(out, pixels);
    }

    #[test]
    fn rle_color_run_repeats_one_color() {
        // REGULAR_COLOR_RUN (code 3 → 0x60) | runLength 4 = 0x64, then 1×3 color.
        let s = [0x64, 0xAA, 0xBB, 0xCC];
        let out = decompress_interleaved(&s, 4, 1, 24).unwrap();
        assert_eq!(out, [0xAA, 0xBB, 0xCC].repeat(4));
    }

    #[test]
    fn rle_bg_run_first_line_is_black() {
        // REGULAR_BG_RUN (0x00) | runLength 4 → all-black first scanline.
        let s = [0x04];
        let out = decompress_interleaved(&s, 4, 1, 24).unwrap();
        assert_eq!(out, vec![0u8; 12]);
    }

    #[test]
    fn rle_set_fg_run_first_line_is_fgpel() {
        // LITE_SET_FG_FG_RUN (code 0xC → 0xC0) | runLength 4 = 0xC4, then fg pel.
        let s = [0xC4, 0x11, 0x22, 0x33];
        let out = decompress_interleaved(&s, 4, 1, 24).unwrap();
        // First line: above is 0, so fg = 0 ^ fgPel = fgPel, repeated.
        assert_eq!(out, [0x11, 0x22, 0x33].repeat(4));
    }

    #[test]
    fn rle_bg_run_copies_previous_scanline() {
        // Line 0: color image of 4 distinct pixels. Line 1: BG run of 4 → copy.
        let line0 = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut s = vec![0x84];
        s.extend_from_slice(&line0);
        s.push(0x04); // BG run, length 4 (whole second scanline)
        let out = decompress_interleaved(&s, 4, 2, 24).unwrap();
        assert_eq!(&out[0..12], &line0);
        assert_eq!(&out[12..24], &line0); // second line copied from the first
    }

    #[test]
    fn rle_fgbg_image_first_line_uses_mask() {
        // REGULAR_FGBG_IMAGE (code 2 → 0x40) | nibble 1 = 0x41 → runLength 8.
        // fgPel defaults to white (0xFF). One mask byte 0xAA: bits set at odd
        // positions → those pixels white, even positions black. 8 px @ 8bpp.
        let s = [0x41, 0xAA];
        let out = decompress_interleaved(&s, 8, 1, 8).unwrap();
        assert_eq!(out, vec![0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn rle_special_white_and_black() {
        // SPECIAL_WHITE (0xFD) then SPECIAL_BLACK (0xFE), 8bpp, 2×1.
        let s = [0xFD, 0xFE];
        let out = decompress_interleaved(&s, 2, 1, 8).unwrap();
        assert_eq!(out, vec![0xFF, 0x00]);
    }

    #[test]
    fn rle_consecutive_bg_runs_insert_fg_pixel() {
        // Set fg to 0x55 via a 1-px SET_FG run, then two BG runs on the first
        // line. The second BG run must begin with one foreground pixel.
        // LITE_SET_FG_FG_RUN len 1 = 0xC1 + fg(0x55); BG len2 (0x02); BG len2 (0x02).
        let s = [0xC1, 0x55, 0x02, 0x02];
        let out = decompress_interleaved(&s, 5, 1, 8).unwrap();
        // px0 = fg (0x55). BG run #1 (len2, first line): black, black.
        // BG run #2 (len2): insert fg (0x55) then 1 black.
        assert_eq!(out, vec![0x55, 0x00, 0x00, 0x55, 0x00]);
    }

    #[test]
    fn rle_rejects_overrun() {
        // COLOR_IMAGE claiming 4 px but only 1 px of data present.
        let s = [0x84, 1, 2, 3];
        assert!(decompress_interleaved(&s, 4, 1, 24).is_none());
    }

    #[test]
    fn rle_unsupported_bpp_returns_none() {
        assert!(decompress_interleaved(&[0x04], 4, 1, 32).is_none());
    }
}
