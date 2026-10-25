//! YUV 4:2:0 → RGBA for the Windows 365 compositor (BT.709 limited range,
//! what RDP's AVC420 uses). No dependencies: its tests and the A/B bench run
//! on any machine (`rustc --test -O yuv.rs`).

/// A decoded picture's planes.
pub struct Planes<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub y_stride: usize,
    /// NV12: interleaved U,V (one plane); I420: U then V planes.
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub uv_stride: usize,
    pub interleaved: bool,
}

#[cfg(test)]
#[inline]
fn rgb(y: i32, u: i32, v: i32) -> [u8; 4] {
    let c = 298 * (y - 16);
    let (d, e) = (u - 128, v - 128);
    let r = (c + 459 * e + 128) >> 8;
    let g = (c - 55 * d - 136 * e + 128) >> 8;
    let b = (c + 541 * d + 128) >> 8;
    [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8, 0xFF]
}

/// The plain per-pixel conversion: the reference the fast one must match
/// exactly.
#[cfg(test)]
pub fn convert_plain(pic: &Planes, x: usize, y: usize, w: usize, h: usize, dst: &mut [u8], dst_stride: usize) {
    let w = w.min(pic.width.saturating_sub(x));
    let h = h.min(pic.height.saturating_sub(y));
    for row in 0..h {
        let py = y + row;
        let Some(out) = dst.get_mut(row * dst_stride..row * dst_stride + w * 4) else { return };
        let Some(yr) = pic.y.get(py * pic.y_stride + x..py * pic.y_stride + x + w) else { return };
        let crow = (py / 2) * pic.uv_stride;
        if pic.interleaved {
            let Some(uv) = pic.u.get(crow..crow + (x + w).div_ceil(2) * 2) else { return };
            for (i, (px, &yy)) in out.chunks_exact_mut(4).zip(yr).enumerate() {
                let cx = ((x + i) / 2) * 2;
                px.copy_from_slice(&rgb(yy as i32, uv[cx] as i32, uv[cx + 1] as i32));
            }
        } else {
            let (Some(ur), Some(vr)) = (pic.u.get(crow..crow + (x + w).div_ceil(2)), pic.v.get(crow..crow + (x + w).div_ceil(2))) else { return };
            for (i, (px, &yy)) in out.chunks_exact_mut(4).zip(yr).enumerate() {
                let cx = (x + i) / 2;
                px.copy_from_slice(&rgb(yy as i32, ur[cx] as i32, vr[cx] as i32));
            }
        }
    }
}

/// Luma's share (298·(Y−16) + 128), by Y.
const LUMA: [i32; 256] = {
    let mut t = [0i32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = 298 * (i as i32 - 16) + 128;
        i += 1;
    }
    t
};

/// 0..=255 for any sum the conversion makes ((−0x100 .. 0x200) after the shift).
#[inline(always)]
fn sat(v: i32) -> u8 {
    // (Branch-free: what the compiler turns into one min/max pair per lane.)
    v.clamp(0, 255) as u8
}

/// The same conversion, a chroma sample at a time: its three terms are
/// worked out once for the two pixels sharing it, luma comes from a table,
/// and each output pixel is one 4-byte store.
pub fn convert_fast(pic: &Planes, x: usize, y: usize, w: usize, h: usize, dst: &mut [u8], dst_stride: usize) {
    let w = w.min(pic.width.saturating_sub(x));
    let h = h.min(pic.height.saturating_sub(y));
    if w == 0 {
        return;
    }
    for row in 0..h {
        let py = y + row;
        let Some(out) = dst.get_mut(row * dst_stride..row * dst_stride + w * 4) else { return };
        let Some(yr) = pic.y.get(py * pic.y_stride + x..py * pic.y_stride + x + w) else { return };
        let crow = (py / 2) * pic.uv_stride;
        let (c0, c1) = (x / 2, (x + w).div_ceil(2));
        let (mut out, mut yr) = (out, yr);
        let odd = x & 1 == 1;
        if pic.interleaved {
            let Some(mut uv) = pic.u.get(crow + c0 * 2..crow + c1 * 2) else { return };
            if odd {
                out[..4].copy_from_slice(&pixel(yr[0], terms(uv[0], uv[1])));
                (out, yr, uv) = (&mut out[4..], &yr[1..], &uv[2..]);
            }
            // Blocks of 16 pixels, then what's left a pair at a time.
            let n = yr.len() / 16;
            for ((o, y16), uv16) in out.chunks_exact_mut(64).zip(yr.chunks_exact(16)).zip(uv.chunks_exact(16)) {
                #[cfg(target_arch = "aarch64")]
                // SAFETY: NEON is part of AArch64; 64 out, 16 Y and 16 UV bytes (the chunks' sizes).
                unsafe {
                    use std::arch::aarch64::vld2_u8;
                    let uv = vld2_u8(uv16.as_ptr());
                    neon::block16(o, y16, uv.0, uv.1);
                }
                #[cfg(not(target_arch = "aarch64"))]
                block16(o, y16, |i| (uv16[i & !1], uv16[i | 1]));
            }
            let (out, yr, uv) = (&mut out[n * 64..], &yr[n * 16..], &uv[(n * 16).min(uv.len())..]);
            row_pairs(out, yr, uv.chunks_exact(2).map(|p| (p[0], p[1])));
        } else {
            let (Some(mut ur), Some(mut vr)) = (pic.u.get(crow + c0..crow + c1), pic.v.get(crow + c0..crow + c1)) else { return };
            if odd {
                out[..4].copy_from_slice(&pixel(yr[0], terms(ur[0], vr[0])));
                (out, yr, ur, vr) = (&mut out[4..], &yr[1..], &ur[1..], &vr[1..]);
            }
            let n = yr.len() / 16;
            for (((o, y16), u8s), v8s) in out.chunks_exact_mut(64).zip(yr.chunks_exact(16)).zip(ur.chunks_exact(8)).zip(vr.chunks_exact(8)) {
                #[cfg(target_arch = "aarch64")]
                // SAFETY: as above; 8 U and 8 V bytes.
                unsafe {
                    use std::arch::aarch64::vld1_u8;
                    neon::block16(o, y16, vld1_u8(u8s.as_ptr()), vld1_u8(v8s.as_ptr()));
                }
                #[cfg(not(target_arch = "aarch64"))]
                block16(o, y16, |i| (u8s[i / 2], v8s[i / 2]));
            }
            let (out, yr) = (&mut out[n * 64..], &yr[n * 16..]);
            let (ur, vr) = (&ur[(n * 8).min(ur.len())..], &vr[(n * 8).min(vr.len())..]);
            row_pairs(out, yr, ur.iter().copied().zip(vr.iter().copied()));
        }
    }
}

/// 16 pixels in NEON (the tablet's): the same integer arithmetic, written
/// out — the compiler's own vectorizing of [`block16`] ran at 3.9 ms a 1080p
/// frame on the Snapdragon's big core.
#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    /// Pixel pairs' chroma terms (8 samples, 32-bit) for 16 pixels: each
    /// sample's term twice, in two halves of 8.
    #[inline(always)]
    unsafe fn pairs(t_lo: int32x4_t, t_hi: int32x4_t) -> [int32x4_t; 4] {
        [vzip1q_s32(t_lo, t_lo), vzip2q_s32(t_lo, t_lo), vzip1q_s32(t_hi, t_hi), vzip2q_s32(t_hi, t_hi)]
    }

    /// `(c + t) >> 8`, saturated to 0..=255, 16 lanes.
    #[inline(always)]
    unsafe fn channel(c: &[int32x4_t; 4], t: &[int32x4_t; 4]) -> uint8x16_t {
        let n = |i: usize| vqmovun_s32(vshrq_n_s32::<8>(vaddq_s32(c[i], t[i])));
        let lo = vqmovn_u16(vcombine_u16(n(0), n(1)));
        let hi = vqmovn_u16(vcombine_u16(n(2), n(3)));
        vcombine_u8(lo, hi)
    }

    /// 16 RGBA pixels into `o` (64 bytes) from 16 Y and 8 U, 8 V.
    #[inline(always)]
    pub unsafe fn block16(o: &mut [u8], y16: &[u8], u: uint8x8_t, v: uint8x8_t) {
        // Luma: 298·Y − 298·16 + 128, 32-bit.
        let y = vld1q_u8(y16.as_ptr());
        let (yl, yh) = (vmovl_u8(vget_low_u8(y)), vmovl_u8(vget_high_u8(y)));
        let k = vdupq_n_s32(-298 * 16 + 128);
        let luma = |h: uint16x4_t| vmlaq_n_s32(k, vreinterpretq_s32_u32(vmovl_u16(h)), 298);
        let c = [luma(vget_low_u16(yl)), luma(vget_high_u16(yl)), luma(vget_low_u16(yh)), luma(vget_high_u16(yh))];
        // Chroma: d = U − 128, e = V − 128 (−128..127), 32-bit.
        let d = vreinterpretq_s16_u16(vsubl_u8(u, vdup_n_u8(128)));
        let e = vreinterpretq_s16_u16(vsubl_u8(v, vdup_n_u8(128)));
        let (dl, dh) = (vmovl_s16(vget_low_s16(d)), vmovl_s16(vget_high_s16(d)));
        let (el, eh) = (vmovl_s16(vget_low_s16(e)), vmovl_s16(vget_high_s16(e)));
        let r = pairs(vmulq_n_s32(el, 459), vmulq_n_s32(eh, 459));
        let g = pairs(vmlaq_n_s32(vmulq_n_s32(dl, -55), el, -136), vmlaq_n_s32(vmulq_n_s32(dh, -55), eh, -136));
        let b = pairs(vmulq_n_s32(dl, 541), vmulq_n_s32(dh, 541));
        let px = uint8x16x4_t(channel(&c, &r), channel(&c, &g), channel(&c, &b), vdupq_n_u8(0xFF));
        vst4q_u8(o.as_mut_ptr(), px);
    }
}

/// 16 pixels, arithmetic only (no table, no branches): the shape the
/// compiler turns into SSE lanes (on ARM: [`neon::block16`]). `uv(i)` is
/// pixel i's chroma.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[inline(always)]
fn block16(o: &mut [u8], y16: &[u8], uv: impl Fn(usize) -> (u8, u8)) {
    let (mut r, mut g, mut b) = ([0i32; 16], [0i32; 16], [0i32; 16]);
    for i in 0..16 {
        let c = 298 * y16[i] as i32 - 298 * 16 + 128;
        let (u, v) = uv(i);
        let (d, e) = (u as i32 - 128, v as i32 - 128);
        r[i] = (c + 459 * e) >> 8;
        g[i] = (c - 55 * d - 136 * e) >> 8;
        b[i] = (c + 541 * d) >> 8;
    }
    for i in 0..16 {
        o[i * 4] = sat(r[i]);
        o[i * 4 + 1] = sat(g[i]);
        o[i * 4 + 2] = sat(b[i]);
        o[i * 4 + 3] = 0xFF;
    }
}

/// The rest of a row, starting on a pair: `chroma` gives a (U, V) per pair.
#[inline(always)]
fn row_pairs(out: &mut [u8], yr: &[u8], mut chroma: impl Iterator<Item = (u8, u8)>) {
    let mut pairs = out.chunks_exact_mut(8);
    let mut ys = yr.chunks_exact(2);
    for ((o, y2), (u, v)) in (&mut pairs).zip(&mut ys).zip(&mut chroma) {
        let t = terms(u, v);
        o[..4].copy_from_slice(&pixel(y2[0], t));
        o[4..].copy_from_slice(&pixel(y2[1], t));
    }
    // An odd end: one pixel left of its pair.
    let (o, y1) = (pairs.into_remainder(), ys.remainder());
    if let (Some(&yy), true) = (y1.first(), o.len() >= 4) {
        if let Some((u, v)) = chroma.next() {
            o[..4].copy_from_slice(&pixel(yy, terms(u, v)));
        }
    }
}

/// A chroma sample's share of R, G and B.
#[inline(always)]
fn terms(u: u8, v: u8) -> (i32, i32, i32) {
    let (d, e) = (u as i32 - 128, v as i32 - 128);
    (459 * e, -55 * d - 136 * e, 541 * d)
}

#[inline(always)]
fn pixel(y: u8, (rr, gg, bb): (i32, i32, i32)) -> [u8; 4] {
    let c = LUMA[y as usize];
    [sat((c + rr) >> 8), sat((c + gg) >> 8), sat((c + bb) >> 8), 0xFF]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator (no crates).
    fn noise(seed: &mut u64, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                *seed as u8
            })
            .collect()
    }

    fn planes(w: usize, h: usize, interleaved: bool, seed: &mut u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let y = noise(seed, w * h);
        if interleaved {
            (y, noise(seed, w * h.div_ceil(2) + 2), Vec::new())
        } else {
            let cw = w.div_ceil(2);
            (y, noise(seed, cw * h.div_ceil(2) + 1), noise(seed, cw * h.div_ceil(2) + 1))
        }
    }

    #[test]
    fn fast_matches_plain_everywhere() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for &interleaved in &[true, false] {
            let (w, h) = (37, 23);
            let (y, u, v) = planes(w, h, interleaved, &mut seed);
            let uv_stride = if interleaved { w + 1 } else { w.div_ceil(2) };
            let pic = Planes { width: w, height: h, y: &y, y_stride: w, u: &u, v: &v, uv_stride, interleaved };
            // Every start parity and odd/even width.
            for &(x, yy, rw, rh) in &[(0, 0, 37, 23), (1, 1, 10, 5), (2, 3, 11, 7), (5, 0, 1, 1), (36, 22, 4, 4), (3, 2, 2, 9), (1, 0, 36, 3), (3, 1, 33, 4), (2, 5, 32, 2), (0, 7, 16, 1)] {
                let mut a = vec![0u8; rw * rh * 4];
                let mut b = vec![0u8; rw * rh * 4];
                convert_plain(&pic, x, yy, rw, rh, &mut a, rw * 4);
                convert_fast(&pic, x, yy, rw, rh, &mut b, rw * 4);
                assert_eq!(a, b, "interleaved {interleaved} rect {x},{yy} {rw}x{rh}");
            }
        }
    }

    /// Every Y, U, V value (the saturation edges included), through the NEON
    /// blocks: 16-pixel rows of every combination.
    #[test]
    fn fast_matches_plain_on_every_value() {
        for &interleaved in &[true, false] {
            for u in (0..=255u8).step_by(5).chain([255]) {
                for v in (0..=255u8).step_by(3).chain([255]) {
                    let w = 256;
                    let y: Vec<u8> = (0..w).map(|i| i as u8).collect();
                    let (uu, vv): (Vec<u8>, Vec<u8>) = if interleaved {
                        ((0..w).map(|i| if i % 2 == 0 { u } else { v }).collect(), Vec::new())
                    } else {
                        (vec![u; w / 2], vec![v; w / 2])
                    };
                    let pic = Planes { width: w, height: 1, y: &y, y_stride: w, u: &uu, v: &vv, uv_stride: if interleaved { w } else { w / 2 }, interleaved };
                    let (mut a, mut b) = (vec![0u8; w * 4], vec![0u8; w * 4]);
                    convert_plain(&pic, 0, 0, w, 1, &mut a, w * 4);
                    convert_fast(&pic, 0, 0, w, 1, &mut b, w * 4);
                    assert_eq!(a, b, "u {u} v {v} interleaved {interleaved}");
                }
            }
        }
    }

    #[test]
    fn white_and_black() {
        let y = [16u8, 235];
        let uv = [128u8, 128];
        let pic = Planes { width: 2, height: 1, y: &y, y_stride: 2, u: &uv, v: &[], uv_stride: 2, interleaved: true };
        let mut out = [0u8; 8];
        convert_fast(&pic, 0, 0, 2, 1, &mut out, 8);
        assert_eq!(out, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    /// A/B: `rustc --test -O yuv.rs && ./yuv --ignored --nocapture bench`.
    #[test]
    #[ignore]
    fn bench() {
        let (w, h) = (1920, 1080);
        let mut seed = 1u64;
        let (y, u, _) = planes(w, h, true, &mut seed);
        let pic = Planes { width: w, height: h, y: &y, y_stride: w, u: &u, v: &[], uv_stride: w, interleaved: true };
        let mut out = vec![0u8; w * h * 4];
        for (name, f) in [("plain", convert_plain as fn(&Planes, usize, usize, usize, usize, &mut [u8], usize)), ("fast", convert_fast)] {
            f(&pic, 0, 0, w, h, &mut out, w * 4);
            let n = 60;
            let t = std::time::Instant::now();
            for _ in 0..n {
                f(&pic, 0, 0, w, h, &mut out, w * 4);
                std::hint::black_box(&out);
            }
            println!("{name}: {:.2} ms per 1080p frame", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
    }
}
