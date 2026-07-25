//! YUV → RGBA color conversion (BT.709 limited range) for the H.264 path.
//!
//! The H.264 decoder (Media Foundation / DXVA) yields NV12 frames: a full-res
//! Y plane followed by an interleaved half-res U/V plane. RDP AVC420/444 video
//! is BT.709 limited-range, so we use the integer-approximated BT.709 matrix to
//! produce top-down RGBA for the renderer. Kept pure so it is unit-testable;
//! the GPU does this in a shader on the real path, but the math is the same.
//!
//! AVC444 carries full 4:4:4 chroma across two AVC420 (4:2:0) sub-streams — a
//! main view (real luma + subsampled chroma) and an auxiliary view holding the
//! chroma samples 4:2:0 drops. [`combine_avc444_to_yuv444`] reassembles them
//! into a full YUV444 frame ([`yuv444_to_rgba`] then renders it), per the
//! MS-RDPEGFX 2.2.4.5 layout (ported from FreeRDP `prim_YUV.c`).

use wide::i32x4;

/// Clamp an i32 to a `u8`.
#[inline]
fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// BT.709 limited-range YUV → RGB, coefficients scaled by 256. Shared by the
/// NV12 (4:2:0) and YUV444 conversions so both render identically.
#[inline]
fn yuv_to_rgb(yy: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let c = 298 * (yy - 16);
    let d = u - 128;
    let e = v - 128;
    let r = (c + 459 * e + 128) >> 8;
    let g = (c - 55 * d - 136 * e + 128) >> 8;
    let b = (c + 541 * d + 128) >> 8;
    (clamp(r), clamp(g), clamp(b))
}

/// SIMD YUV → RGBA for four consecutive pixels. `y`/`u`/`v` are already
/// broadcast/loaded as i32 lanes. Returns 16 bytes `[R0,G0,B0,A0, R1,…]`.
#[inline]
fn yuv_to_rgba_4x(y: i32x4, u: i32x4, v: i32x4) -> [u8; 16] {
    let c = (y - i32x4::splat(16)) * i32x4::splat(298);
    let d = u - i32x4::splat(128);
    let e = v - i32x4::splat(128);
    let r: i32x4 = (c + e * i32x4::splat(459) + i32x4::splat(128)) >> 8;
    let g: i32x4 = (c - d * i32x4::splat(55) - e * i32x4::splat(136) + i32x4::splat(128)) >> 8;
    let b: i32x4 = (c + d * i32x4::splat(541) + i32x4::splat(128)) >> 8;
    let r = r.max(i32x4::splat(0)).min(i32x4::splat(255));
    let g = g.max(i32x4::splat(0)).min(i32x4::splat(255));
    let b = b.max(i32x4::splat(0)).min(i32x4::splat(255));

    let [r0, r1, r2, r3] = r.to_array();
    let [g0, g1, g2, g3] = g.to_array();
    let [b0, b1, b2, b3] = b.to_array();
    [
        r0 as u8, g0 as u8, b0 as u8, 0xFF,
        r1 as u8, g1 as u8, b1 as u8, 0xFF,
        r2 as u8, g2 as u8, b2 as u8, 0xFF,
        r3 as u8, g3 as u8, b3 as u8, 0xFF,
    ]
}

/// Convert an NV12 frame to top-down RGBA8.
///
/// `y_plane` is `stride * height` bytes; `uv_plane` is `stride * (height/2)`
/// bytes of interleaved U,V. `stride` is the row pitch (≥ `width`). Returns
/// `None` if the planes are too small.
pub fn nv12_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> Option<Vec<u8>> {
    if stride < width || y_plane.len() < stride * height {
        return None;
    }
    if uv_plane.len() < stride * (height / 2) {
        return None;
    }

    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let y_row = y * stride;
        let uv_row = (y / 2) * stride;
        let mut x = 0usize;
        // SIMD 4-pixel chunks. x stays even, so each chunk spans two UV pairs.
        while x + 4 <= width {
            let yy = i32x4::new([
                y_plane[y_row + x] as i32,
                y_plane[y_row + x + 1] as i32,
                y_plane[y_row + x + 2] as i32,
                y_plane[y_row + x + 3] as i32,
            ]);
            let uv_x = x & !1;
            let u0 = uv_plane[uv_row + uv_x] as i32;
            let v0 = uv_plane[uv_row + uv_x + 1] as i32;
            let u1 = uv_plane[uv_row + uv_x + 2] as i32;
            let v1 = uv_plane[uv_row + uv_x + 3] as i32;
            let u = i32x4::new([u0, u0, u1, u1]);
            let v = i32x4::new([v0, v0, v1, v1]);
            let di = (y * width + x) * 4;
            out[di..di + 16].copy_from_slice(&yuv_to_rgba_4x(yy, u, v));
            x += 4;
        }
        // Scalar tail (also handles widths < 4).
        for x in x..width {
            let yy = y_plane[y_row + x] as i32;
            let uv_x = x & !1;
            let u = uv_plane[uv_row + uv_x] as i32;
            let v = uv_plane[uv_row + uv_x + 1] as i32;
            let (r, g, b) = yuv_to_rgb(yy, u, v);
            let di = (y * width + x) * 4;
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = 0xFF;
        }
    }
    Some(out)
}

/// Repack a strided NV12 buffer (as a hardware/MFT decoder emits it) into a
/// tightly packed `width*height` NV12 frame: stride == width, the UV plane
/// immediately after Y. `stride` is the decoder's real Y row pitch (≥ width,
/// often padded past it) and `coded_h` is the macroblock-aligned Y height (the
/// UV plane begins at `stride*coded_h`, *after* the padding rows). Rows/columns
/// outside the display size are dropped.
///
/// This is the fix for sheared/mis-coloured H.264 frames: a decoder that outputs
/// at a coded size larger than the display size (any width/height not a multiple
/// of the codec's macroblock grid) must not be read as if it were tightly
/// packed. Every read is bounds-checked; a short input yields neutral (128)
/// chroma rather than reading past `src`. Returns `None` for a degenerate size.
pub fn nv12_repack_tight(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    coded_h: usize,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || stride < width {
        return None;
    }
    let mut out = vec![0u8; width * height * 3 / 2];
    // Neutral chroma so a truncated UV plane degrades to grayscale, not green.
    for b in &mut out[width * height..] {
        *b = 128;
    }
    // Y plane: one display-width row per display row, at the real stride.
    for row in 0..height {
        let s = row * stride;
        if s + width > src.len() {
            break;
        }
        out[row * width..row * width + width].copy_from_slice(&src[s..s + width]);
    }
    // UV plane: begins after the coded (padded) Y plane, half the rows.
    let uv_src = stride * coded_h;
    let uv_dst = width * height;
    for row in 0..height / 2 {
        let s = uv_src + row * stride;
        if s + width > src.len() {
            break;
        }
        out[uv_dst + row * width..uv_dst + row * width + width].copy_from_slice(&src[s..s + width]);
    }
    Some(out)
}

/// Split an interleaved NV12 chroma plane into separate U and V planes.
///
/// `uv` is `stride * ch` bytes of `U,V,U,V,…`; `cw`/`ch` are the chroma
/// dimensions (half the luma, rounded up). Returns tightly-packed `U` and `V`
/// planes of `cw * ch` each, or `None` if `uv` is too small.
pub fn nv12_chroma_to_planar(
    uv: &[u8],
    cw: usize,
    ch: usize,
    stride: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if stride < cw * 2 || uv.len() < stride * ch {
        return None;
    }
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for row in 0..ch {
        let src = row * stride;
        for x in 0..cw {
            u[row * cw + x] = uv[src + 2 * x];
            v[row * cw + x] = uv[src + 2 * x + 1];
        }
    }
    Some((u, v))
}

/// Reassemble a full YUV444 chroma pair from an AVC444 main + auxiliary frame
/// (MS-RDPEGFX 2.2.4.5, `ChromaV1` layout; ported from FreeRDP `prim_YUV.c`).
///
/// The full-resolution luma is just the main view's Y plane (the caller uses it
/// directly). This builds the `w*h` U and V planes:
///  1. replicate the main view's 4:2:0 chroma across each 2×2 block (the base),
///  2. overwrite even rows' odd columns with the aux view's chroma planes,
///  3. overwrite the odd rows (full width) from the aux view's *luma* plane,
///     which packs U and V rows in alternating 16-row bands.
///
/// `main_u`/`main_v`/`aux_u`/`aux_v` are `hw*hh` (`hw=(w+1)/2`, `hh=(h+1)/2`);
/// `aux_y` is `w*aux_h` (the aux luma, padded to a multiple of 16 rows). All
/// indexing is bounds-checked, so a malformed/short input yields `None` rather
/// than panicking. Returns `(u444, v444)`, each `w*h`.
#[allow(clippy::too_many_arguments)]
pub fn combine_avc444_to_yuv444(
    main_u: &[u8],
    main_v: &[u8],
    aux_y: &[u8],
    aux_u: &[u8],
    aux_v: &[u8],
    w: usize,
    h: usize,
    aux_h: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if w == 0 || h == 0 {
        return None;
    }
    let hw = w.div_ceil(2);
    let hh = h.div_ceil(2);
    if main_u.len() < hw * hh
        || main_v.len() < hw * hh
        || aux_u.len() < hw * hh
        || aux_v.len() < hw * hh
        || aux_y.len() < w * aux_h
    {
        return None;
    }

    let mut u444 = vec![0u8; w * h];
    let mut v444 = vec![0u8; w * h];

    // 1) Base: replicate each main chroma sample across its 2×2 block.
    for cy in 0..hh {
        for cx in 0..hw {
            let um = main_u[cy * hw + cx];
            let vm = main_v[cy * hw + cx];
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let yy = 2 * cy + dy;
                let xx = 2 * cx + dx;
                if yy < h && xx < w {
                    u444[yy * w + xx] = um;
                    v444[yy * w + xx] = vm;
                }
            }
        }
    }

    // 2) Aux chroma planes fill the odd columns of even rows (B6/B7).
    for cy in 0..hh {
        for cx in 0..hw {
            let yy = 2 * cy;
            let xx = 2 * cx + 1;
            if yy < h && xx < w {
                u444[yy * w + xx] = aux_u[cy * hw + cx];
                v444[yy * w + xx] = aux_v[cy * hw + cx];
            }
        }
    }

    // 3) Aux luma plane fills the odd rows, full width (B4/B5). Source rows
    //    alternate U/V in 16-row bands; `uy`/`vy` walk the destination odd rows.
    let mut uy = 0usize;
    let mut vy = 0usize;
    for sy in 0..aux_h {
        let is_u = (sy % 16) < 8;
        let pos = if is_u {
            let p = 2 * uy + 1;
            uy += 1;
            p
        } else {
            let p = 2 * vy + 1;
            vy += 1;
            p
        };
        if pos >= h {
            continue;
        }
        let src = &aux_y[sy * w..sy * w + w];
        let dst = if is_u { &mut u444 } else { &mut v444 };
        dst[pos * w..pos * w + w].copy_from_slice(src);
    }

    Some((u444, v444))
}

/// Convert a planar YUV444 frame (one chroma sample per pixel) to top-down
/// RGBA8 using the same BT.709 limited-range matrix as [`nv12_to_rgba`].
/// `stride` is the row pitch of every plane (≥ `width`). `None` if too small.
pub fn yuv444_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> Option<Vec<u8>> {
    if stride < width
        || y_plane.len() < stride * height
        || u_plane.len() < stride * height
        || v_plane.len() < stride * height
    {
        return None;
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let row = y * stride;
        let mut x = 0usize;
        while x + 4 <= width {
            let yy = i32x4::new([
                y_plane[row + x] as i32,
                y_plane[row + x + 1] as i32,
                y_plane[row + x + 2] as i32,
                y_plane[row + x + 3] as i32,
            ]);
            let u = i32x4::new([
                u_plane[row + x] as i32,
                u_plane[row + x + 1] as i32,
                u_plane[row + x + 2] as i32,
                u_plane[row + x + 3] as i32,
            ]);
            let v = i32x4::new([
                v_plane[row + x] as i32,
                v_plane[row + x + 1] as i32,
                v_plane[row + x + 2] as i32,
                v_plane[row + x + 3] as i32,
            ]);
            let di = (y * width + x) * 4;
            out[di..di + 16].copy_from_slice(&yuv_to_rgba_4x(yy, u, v));
            x += 4;
        }
        for x in x..width {
            let (r, g, b) = yuv_to_rgb(
                y_plane[row + x] as i32,
                u_plane[row + x] as i32,
                v_plane[row + x] as i32,
            );
            let di = (y * width + x) * 4;
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = 0xFF;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a solid-color NV12 frame (all pixels share one Y/U/V).
    fn solid(width: usize, height: usize, yv: u8, uv: (u8, u8)) -> (Vec<u8>, Vec<u8>) {
        let y = vec![yv; width * height];
        let mut uv_plane = Vec::with_capacity(width * (height / 2));
        for _ in 0..(width / 2) * (height / 2) {
            uv_plane.push(uv.0);
            uv_plane.push(uv.1);
        }
        (y, uv_plane)
    }

    #[test]
    fn black_and_white_endpoints() {
        // Y=16, U=V=128 → black; Y=235 → white (limited range endpoints).
        let (y, uv) = solid(4, 4, 16, (128, 128));
        let black = nv12_to_rgba(&y, &uv, 4, 4, 4).unwrap();
        assert_eq!(&black[0..4], &[0, 0, 0, 0xFF]);

        let (y, uv) = solid(4, 4, 235, (128, 128));
        let white = nv12_to_rgba(&y, &uv, 4, 4, 4).unwrap();
        assert_eq!(&white[0..4], &[255, 255, 255, 0xFF]);
    }

    #[test]
    fn neutral_chroma_is_grayscale() {
        // Y=125, U=V=128 → equal R=G=B (no chroma contribution).
        let (y, uv) = solid(2, 2, 125, (128, 128));
        let px = nv12_to_rgba(&y, &uv, 2, 2, 2).unwrap();
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
        assert_eq!(px[3], 0xFF);
        // 298*(125-16) = 32482; (32482+128)>>8 = 127.
        assert_eq!(px[0], 127);
    }

    #[test]
    fn rejects_short_planes() {
        assert!(nv12_to_rgba(&[0u8; 3], &[0u8; 8], 4, 4, 4).is_none());
    }

    #[test]
    fn honors_stride_padding() {
        // 2x2 image in a stride-4 buffer; padding bytes must be ignored.
        let y = vec![235, 235, 0, 0, 235, 235, 0, 0];
        let uv = vec![128, 128, 0, 0]; // one UV row (height/2 = 1), stride 4
        let px = nv12_to_rgba(&y, &uv, 2, 2, 4).unwrap();
        assert_eq!(&px[0..4], &[255, 255, 255, 0xFF]);
        assert_eq!(px.len(), 2 * 2 * 4);
    }

    #[test]
    fn repack_drops_row_padding_when_stride_exceeds_width() {
        // 2x2 display in a stride-4, coded-height-2 buffer. Y rows are padded to
        // 4 bytes; the UV plane starts at stride*coded_h = 8. The padding bytes
        // (0xEE) and any trailing garbage must be excluded.
        let src = vec![
            10, 11, 0xEE, 0xEE, // Y row 0 (+pad)
            12, 13, 0xEE, 0xEE, // Y row 1 (+pad)
            20, 21, 0xEE, 0xEE, // UV row 0 (one chroma row for height 2) (+pad)
        ];
        let out = nv12_repack_tight(&src, 2, 2, 4, 2).unwrap();
        // Tightly packed: Y[10,11,12,13] then UV[20,21].
        assert_eq!(out, vec![10, 11, 12, 13, 20, 21]);
    }

    #[test]
    fn repack_finds_uv_after_coded_height_padding() {
        // The classic 1080-style case in miniature: display height 2, but the
        // decoder padded the coded Y height to 4. The UV plane therefore starts
        // at stride*coded_h = 2*4 = 8, NOT at width*height = 4. Reading at the
        // wrong offset is what turned luma rows into chroma garbage.
        let src = vec![
            1, 2, // Y display row 0
            3, 4, // Y display row 1
            9, 9, // padded coded row 2 (NOT chroma)
            9, 9, // padded coded row 3 (NOT chroma)
            7, 8, // real UV row 0
        ];
        let out = nv12_repack_tight(&src, 2, 2, 2, 4).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 7, 8]);
    }

    #[test]
    fn repack_truncated_uv_is_neutral_gray() {
        // Y present, UV plane missing → chroma fills with 128 (gray), no panic.
        let src = vec![5, 6, 7, 8]; // only the Y plane for a 2x2 frame
        let out = nv12_repack_tight(&src, 2, 2, 2, 2).unwrap();
        assert_eq!(out, vec![5, 6, 7, 8, 128, 128]);
        // Degenerate sizes / too-small stride → None.
        assert!(nv12_repack_tight(&src, 0, 2, 2, 2).is_none());
        assert!(nv12_repack_tight(&src, 4, 2, 2, 2).is_none());
    }

    #[test]
    fn chroma_deinterleave_splits_uv() {
        // stride 6 = 2 UV pairs + 2 padding bytes; padding must be ignored.
        let uv = vec![1, 2, 3, 4, 0, 0];
        let (u, v) = nv12_chroma_to_planar(&uv, 2, 1, 6).unwrap();
        assert_eq!(u, vec![1, 3]);
        assert_eq!(v, vec![2, 4]);
        // too small → None.
        assert!(nv12_chroma_to_planar(&[0u8; 3], 2, 1, 4).is_none());
    }

    #[test]
    fn yuv444_endpoints_and_gray() {
        let white = yuv444_to_rgba(&[235; 4], &[128; 4], &[128; 4], 2, 2, 2).unwrap();
        assert_eq!(&white[0..4], &[255, 255, 255, 0xFF]);
        let black = yuv444_to_rgba(&[16; 4], &[128; 4], &[128; 4], 2, 2, 2).unwrap();
        assert_eq!(&black[0..4], &[0, 0, 0, 0xFF]);
        // Neutral chroma → grayscale (matches nv12 path's 125 → 127).
        let gray = yuv444_to_rgba(&[125; 4], &[128; 4], &[128; 4], 2, 2, 2).unwrap();
        assert_eq!(&gray[0..3], &[127, 127, 127]);
    }

    #[test]
    fn avc444_combine_places_all_chroma_samples() {
        // 2x2 frame: one main chroma sample, the aux providing the other three.
        // hw=hh=1. main U=10/V=20 fill the block base; aux U/V planes fill the
        // top-right; the aux *luma* fills the bottom row (U band row 0, V band
        // row 8 for a 16-row-padded aux frame).
        let main_u = [10u8];
        let main_v = [20u8];
        let aux_u = [11u8];
        let aux_v = [21u8];
        let mut aux_y = vec![0u8; 16 * 2];
        aux_y[0] = 31; // row 0 (U band) → U444 row 1
        aux_y[1] = 32;
        aux_y[8 * 2] = 41; // row 8 (V band) → V444 row 1
        aux_y[8 * 2 + 1] = 42;

        let (u444, v444) =
            combine_avc444_to_yuv444(&main_u, &main_v, &aux_y, &aux_u, &aux_v, 2, 2, 16).unwrap();
        // U444: [base, aux_u; aux_luma_row0]
        assert_eq!(u444, vec![10, 11, 31, 32]);
        // V444: [base, aux_v; aux_luma_row8]
        assert_eq!(v444, vec![20, 21, 41, 42]);

        // Short aux luma → None (no panic on malformed input).
        assert!(
            combine_avc444_to_yuv444(&main_u, &main_v, &[0u8; 2], &aux_u, &aux_v, 2, 2, 16)
                .is_none()
        );
    }
}
