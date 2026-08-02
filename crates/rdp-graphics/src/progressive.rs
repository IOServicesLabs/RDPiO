//! RemoteFX Progressive (MS-RDPEGFX §2.2.4) tile codec — decode path.
//!
//! Progressive is the EGFX baseline codec a server uses for photographic and
//! motion content (video) when H.264 is unavailable — e.g. a CPU-only host. It
//! arrives via `RDPGFX_CMDID_WIRETOSURFACE_2` and is the reason the video region
//! was previously never painted: the command was dropped.
//!
//! It shares two primitives with classic RemoteFX ([`crate::rfx`]): RLGR1 entropy
//! coding ([`crate::rfx::rlgr_decode`]) and the BT.601 YCbCr→RGB transform
//! ([`crate::rfx::ycbcr_tile_to_rgba`]). It differs in the wire structure (the
//! `WBT_*` block stream), the inverse DWT (a band-concatenated layout with two
//! variants — the classic 5/3 lifting and the newer *reduce-extrapolate* one),
//! and progressive refinement: `TILE_FIRST` lays a base-quality tile that later
//! `TILE_UPGRADE` passes sharpen, so a per-tile coefficient state persists across
//! frames.
//!
//! This module is a faithful port of FreeRDP `libfreerdp/codec/progressive.c` and
//! `rfx_dwt.c`: `TILE_SIMPLE`/`TILE_FIRST` lay the base layer, `TILE_UPGRADE`
//! refines it, and `TILE_DIFFERENCE`-flagged tiles add deltas onto the persisted
//! per-tile coefficient state. That state lives for the surface's lifetime —
//! `DeleteEncodingContext` deliberately does NOT clear it (FreeRDP parity).
//! Payloads captured with `RDPIO_DUMP_PROG=<dir>` replay offline via
//! `cargo run -p rdp-graphics --example prog_replay -- <dir>`.

/// A tile is always 64×64 samples.
pub use crate::rfx::TILE;

use crate::pool::BufferPool;
use wide::i64x2;

/// Every component buffer is a fixed 64×64 = 4096 coefficient plane.
const COEFFS: usize = 4096;

// Progressive bitstream block types (MS-RDPEGFX 2.2.4.2). These overlap classic
// RFX block-type numbers (`rfx::WBT_*`) but are a separate codec namespace.
const WBT_SYNC: u16 = 0xCCC0;
const WBT_FRAME_BEGIN: u16 = 0xCCC1;
const WBT_FRAME_END: u16 = 0xCCC2;
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_REGION: u16 = 0xCCC4;
const WBT_TILE_SIMPLE: u16 = 0xCCC5;
const WBT_TILE_FIRST: u16 = 0xCCC6;
const WBT_TILE_UPGRADE: u16 = 0xCCC7;

/// `region.flags` bit: the reduce-extrapolate DWT variant is in use.
const RFX_DWT_REDUCE_EXTRAPOLATE: u8 = 0x01;
/// `tile.flags` bit: this tile's coefficients are a delta from the previous frame.
const RFX_TILE_DIFFERENCE: u8 = 0x01;

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

/// True if `d` begins with a recognised progressive block type (`WBT_*`).
#[inline]
fn starts_with_block(d: &[u8]) -> bool {
    matches!(u16le(d, 0), Some(t) if (WBT_SYNC..=WBT_TILE_UPGRADE).contains(&t))
}

/// WireToSurface2's progressive `bitmapData` is `[u32 length][blocks…]` — the
/// observed wire format prefixes the block stream with a 4-byte length of the
/// remainder. Strip it when present; tolerate a stream that already starts at a
/// block (no prefix) so we work either way.
#[inline]
fn strip_length_prefix(data: &[u8]) -> &[u8] {
    if starts_with_block(data) {
        data
    } else if data.len() >= 4 && starts_with_block(&data[4..]) {
        &data[4..]
    } else {
        data
    }
}

#[inline]
fn clampi16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

// ---------------------------------------------------------------------------
// RLGR entropy decoder (faithful port of FreeRDP `rfx_rlgr_decode`)
// ---------------------------------------------------------------------------
//
// NOTE: `crate::rfx::rlgr_decode` is a *different*, self-consistent scheme that is
// not wire-compatible with the MS-RDP/FreeRDP RLGR, so it cannot be reused here.

const KPMAX: u32 = 80;
const LSGR: u32 = 3;
const UP_GR: u32 = 4;
const DN_GR: u32 = 6;
const UQ_GR: u32 = 3;
const DQ_GR: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RlgrMode {
    Rlgr1,
    Rlgr3,
}

/// MSB-first bit reader over the entropy stream, mirroring WinPR's `wBitStream`:
/// `accumulator()` returns the next 32 bits MSB-aligned (zero-padded past the end)
/// and `remaining()` is the bit count left.
///
/// Hot path: the upcoming bits are cached top-aligned in a 64-bit register and
/// refilled bytewise on `shift`, so `accumulator()` (called once or more per
/// decoded bit) is a single shift instead of a 5-byte gather.
struct BitStream<'a> {
    data: &'a [u8],
    pos: usize,    // absolute bit position (for `remaining`)
    acc: u64,      // upcoming bits, MSB-aligned at bit 63, zero past the end
    acc_bits: u32, // valid bit count at the top of `acc`
    next: usize,   // next byte index to pull into `acc`
}

impl<'a> BitStream<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut s = Self {
            data,
            pos: 0,
            acc: 0,
            acc_bits: 0,
            next: 0,
        };
        s.refill();
        s
    }
    #[inline]
    fn refill(&mut self) {
        while self.acc_bits <= 56 && self.next < self.data.len() {
            self.acc |= (self.data[self.next] as u64) << (56 - self.acc_bits);
            self.acc_bits += 8;
            self.next += 1;
        }
    }
    #[inline]
    fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }
    /// The next 32 bits MSB-first from the current position (0-padded at the end).
    #[inline]
    fn accumulator(&self) -> u32 {
        (self.acc >> 32) as u32
    }
    #[inline]
    fn shift(&mut self, n: usize) {
        // Callers never shift more than 32 at once; shifting past the end of the
        // stream legitimately leaves an all-zero accumulator.
        self.pos += n;
        self.acc <<= n as u32;
        self.acc_bits = self.acc_bits.saturating_sub(n as u32);
        self.refill();
    }
}

/// Decode `out.len()` coefficients with the RLGR entropy coder into `out`.
/// Faithful port of FreeRDP `rfx_rlgr_decode` (run-length / Golomb-Rice with
/// adaptive k/kp/kr/krp). `out` is fully written: positions past the end of the
/// entropy stream are zero (trailing-zero omission is legal in RLGR).
fn rlgr_decode(mode: RlgrMode, data: &[u8], out: &mut [i16]) {
    out.fill(0);
    let dst_size = out.len();
    let mut n = 0usize;
    let mut bs = BitStream::new(data);

    let mut k: u32 = 1;
    let mut kp: u32 = k << LSGR;
    let mut kr: u32 = 1;
    let mut krp: u32 = kr << LSGR;

    // Count a unary prefix: leading zeros if `ones` is false, leading ones if true.
    // Consumes the prefix and its terminating bit; returns the prefix length (vk).
    // Returns None if the stream ran out before the terminator.
    let count_unary = |bs: &mut BitStream, ones: bool| -> Option<u32> {
        let lead = |acc: u32| if ones { (!acc).leading_zeros() } else { acc.leading_zeros() };
        let mut cnt = lead(bs.accumulator()).min(bs.remaining() as u32);
        let mut vk = cnt;
        while cnt == 32 && bs.remaining() > 0 {
            bs.shift(32);
            cnt = lead(bs.accumulator()).min(bs.remaining() as u32);
            vk += cnt;
        }
        bs.shift((vk % 32) as usize);
        if bs.remaining() < 1 {
            return None;
        }
        bs.shift(1); // terminating bit
        Some(vk)
    };

    while bs.remaining() > 0 && n < dst_size {
        if k != 0 {
            // ===== run-length mode =====
            let vk = match count_unary(&mut bs, false) {
                Some(v) => v,
                None => break,
            };
            // Accumulate the run, evolving k as each unary count is consumed.
            let mut run: u32 = 0;
            for _ in 0..vk {
                run += 1u32 << k;
                kp += UP_GR;
                if kp > KPMAX {
                    kp = KPMAX;
                }
                k = kp >> LSGR;
            }
            // k-bit run remainder.
            if (bs.remaining() as u32) < k {
                break;
            }
            if k > 0 {
                let mask = (1u32 << k) - 1;
                run += (bs.accumulator() >> (32 - k)) & mask;
                bs.shift(k as usize);
            }
            // sign bit.
            if bs.remaining() < 1 {
                break;
            }
            let sign = (bs.accumulator() & 0x8000_0000) != 0;
            bs.shift(1);
            // magnitude: leading-ones unary + kr-bit remainder.
            let vk2 = match count_unary(&mut bs, true) {
                Some(v) => v,
                None => break,
            };
            if (bs.remaining() as u32) < kr {
                break;
            }
            let mut code: u32 = 0;
            if kr > 0 {
                let mask = (1u32 << kr) - 1;
                code = (bs.accumulator() >> (32 - kr)) & mask;
                bs.shift(kr as usize);
            }
            code |= vk2 << kr;
            update_kr(vk2, &mut kr, &mut krp);
            // update k downward after a run.
            kp = kp.saturating_sub(DN_GR);
            k = kp >> LSGR;
            // emit `run` zeros (already zero in `out`) then the nonzero magnitude.
            let m = code as i32 + 1;
            let mag = if sign { -m } else { m } as i16;
            n = (n + run as usize).min(dst_size);
            if n < dst_size {
                out[n] = mag;
                n += 1;
            }
        } else {
            // ===== Golomb-Rice mode (k == 0) =====
            let vk = match count_unary(&mut bs, true) {
                Some(v) => v,
                None => break,
            };
            if (bs.remaining() as u32) < kr {
                break;
            }
            let mut code: u32 = 0;
            if kr > 0 {
                let mask = (1u32 << kr) - 1;
                code = (bs.accumulator() >> (32 - kr)) & mask;
                bs.shift(kr as usize);
            }
            code |= vk << kr;
            update_kr(vk, &mut kr, &mut krp);

            if mode == RlgrMode::Rlgr1 {
                if code == 0 {
                    kp += UQ_GR;
                    if kp > KPMAX {
                        kp = KPMAX;
                    }
                    k = kp >> LSGR;
                    if n < dst_size {
                        out[n] = 0;
                        n += 1;
                    }
                } else {
                    kp = kp.saturating_sub(DQ_GR);
                    k = kp >> LSGR;
                    let mag = if code & 1 != 0 {
                        -(((code + 1) >> 1) as i32)
                    } else {
                        (code >> 1) as i32
                    };
                    if n < dst_size {
                        out[n] = mag as i16;
                        n += 1;
                    }
                }
            } else {
                // RLGR3: split `code` into two values.
                let mut n_idx = 0u32;
                if code != 0 {
                    n_idx = 32 - code.leading_zeros();
                }
                if (bs.remaining() as u32) < n_idx {
                    break;
                }
                let mut val1 = 0u32;
                if n_idx > 0 {
                    let mask = (1u32 << n_idx) - 1;
                    val1 = (bs.accumulator() >> (32 - n_idx)) & mask;
                    bs.shift(n_idx as usize);
                }
                let val2 = code - val1;
                if val1 != 0 && val2 != 0 {
                    kp = kp.saturating_sub(2 * DQ_GR);
                    k = kp >> LSGR;
                } else if val1 == 0 && val2 == 0 {
                    kp += 2 * UQ_GR;
                    if kp > KPMAX {
                        kp = KPMAX;
                    }
                    k = kp >> LSGR;
                }
                let m1 = if val1 & 1 != 0 {
                    -(((val1 + 1) >> 1) as i32)
                } else {
                    (val1 >> 1) as i32
                };
                if n < dst_size {
                    out[n] = m1 as i16;
                    n += 1;
                }
                let m2 = if val2 & 1 != 0 {
                    -(((val2 + 1) >> 1) as i32)
                } else {
                    (val2 >> 1) as i32
                };
                if n < dst_size {
                    out[n] = m2 as i16;
                    n += 1;
                }
            }
        }
    }
}

/// Adapt the Golomb-Rice parameter `kr`/`krp` from the observed unary count.
#[inline]
fn update_kr(vk: u32, kr: &mut u32, krp: &mut u32) {
    if vk == 0 {
        *krp = krp.saturating_sub(2);
        *kr = *krp >> LSGR;
    } else if vk != 1 {
        *krp += vk;
        if *krp > KPMAX {
            *krp = KPMAX;
        }
        *kr = *krp >> LSGR;
    }
}

// ---------------------------------------------------------------------------
// Quantization
// ---------------------------------------------------------------------------

/// One component's 10 sub-band quant shifts (RFX_COMPONENT_CODEC_QUANT), kept as
/// `i32` so the per-component add/subtract can't wrap. Field order matches the
/// FreeRDP struct (and the nibble order on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Quant {
    ll3: i32,
    hl3: i32,
    lh3: i32,
    hh3: i32,
    hl2: i32,
    lh2: i32,
    hh2: i32,
    hl1: i32,
    lh1: i32,
    hh1: i32,
}

impl Quant {
    /// Unpack 5 bytes into 10 four-bit sub-band values (low nibble first), in the
    /// wire order LL3,HL3,LH3,HH3,HL2,LH2,HH2,HL1,LH1,HH1.
    fn read(b: &[u8]) -> Quant {
        let g = |i: usize| b.get(i).copied().unwrap_or(0);
        Quant {
            ll3: (g(0) & 0x0F) as i32,
            hl3: (g(0) >> 4) as i32,
            lh3: (g(1) & 0x0F) as i32,
            hh3: (g(1) >> 4) as i32,
            hl2: (g(2) & 0x0F) as i32,
            lh2: (g(2) >> 4) as i32,
            hh2: (g(3) & 0x0F) as i32,
            hl1: (g(3) >> 4) as i32,
            lh1: (g(4) & 0x0F) as i32,
            hh1: (g(4) >> 4) as i32,
        }
    }

    /// Per-sub-band sum (`progressive_rfx_quant_add`).
    fn add(&self, o: &Quant) -> Quant {
        Quant {
            ll3: self.ll3 + o.ll3,
            hl3: self.hl3 + o.hl3,
            lh3: self.lh3 + o.lh3,
            hh3: self.hh3 + o.hh3,
            hl2: self.hl2 + o.hl2,
            lh2: self.lh2 + o.lh2,
            hh2: self.hh2 + o.hh2,
            hl1: self.hl1 + o.hl1,
            lh1: self.lh1 + o.lh1,
            hh1: self.hh1 + o.hh1,
        }
    }

    /// Subtract `val` from every sub-band, failing (like `progressive_rfx_quant_lsub`)
    /// if any would go negative — the result is the dequant left-shift per band.
    fn lsub(&self, val: i32) -> Option<Quant> {
        let s = |q: i32| if q < val { None } else { Some(q - val) };
        Some(Quant {
            ll3: s(self.ll3)?,
            hl3: s(self.hl3)?,
            lh3: s(self.lh3)?,
            hh3: s(self.hh3)?,
            hl2: s(self.hl2)?,
            lh2: s(self.lh2)?,
            hh2: s(self.hh2)?,
            hl1: s(self.hl1)?,
            lh1: s(self.lh1)?,
            hh1: s(self.hh1)?,
        })
    }

    /// Per-sub-band difference `self - o` (`progressive_rfx_quant_sub`), failing if
    /// any band would go negative. Used to derive an UPGRADE pass's per-band
    /// refinement bit count (`numBits = oldBitPos - newBitPos`).
    fn sub(&self, o: &Quant) -> Option<Quant> {
        let s = |a: i32, b: i32| if a < b { None } else { Some(a - b) };
        Some(Quant {
            ll3: s(self.ll3, o.ll3)?,
            hl3: s(self.hl3, o.hl3)?,
            lh3: s(self.lh3, o.lh3)?,
            hh3: s(self.hh3, o.hh3)?,
            hl2: s(self.hl2, o.hl2)?,
            lh2: s(self.lh2, o.lh2)?,
            hh2: s(self.hh2, o.hh2)?,
            hl1: s(self.hl1, o.hl1)?,
            lh1: s(self.lh1, o.lh1)?,
            hh1: s(self.hh1, o.hh1)?,
        })
    }
}

/// One `RFX_PROGRESSIVE_CODEC_QUANT` (16 bytes): a quality byte then Y/Cb/Cr
/// simple-quant triples.
#[derive(Debug, Clone, Copy, Default)]
struct ProgQuant {
    y: Quant,
    cb: Quant,
    cr: Quant,
}

impl ProgQuant {
    fn read(b: &[u8]) -> ProgQuant {
        // byte 0 is `quality`; the three 5-byte quant sets follow.
        ProgQuant {
            y: Quant::read(b.get(1..).unwrap_or(&[])),
            cb: Quant::read(b.get(6..).unwrap_or(&[])),
            cr: Quant::read(b.get(11..).unwrap_or(&[])),
        }
    }
}

// ---------------------------------------------------------------------------
// Inverse DWT — band counts shared by both variants
// ---------------------------------------------------------------------------

#[inline]
fn band_l_count(level: usize) -> usize {
    (64 >> level) + 1
}
#[inline]
fn band_h_count(level: usize) -> usize {
    if level == 1 {
        (64 >> 1) - 1
    } else {
        (64 + (1usize << (level - 1))) >> level
    }
}

// ---------------------------------------------------------------------------
// Inverse DWT — classic (non-extrapolate) variant, `rfx_dwt_2d_decode`
// ---------------------------------------------------------------------------

/// One level of the classic inverse DWT over a band-concatenated `buf[base..]`
/// with sub-band side `sw`. Faithful port of FreeRDP `rfx_dwt_2d_decode_block`
/// (horizontal lift into `idwt`, then vertical lift back into `buf`). Uses
/// floor-rounding `>>1` as in the reference.
fn dwt_block(buf: &mut [i16], base: usize, idwt: &mut [i16], sw: usize) {
    let total = sw << 1;
    let ll = base + sw * sw * 3;
    let hl = base;
    let lh = base + sw * sw;
    let hh = base + sw * sw * 2;

    // Horizontal: (LL,HL)->L at idwt[0..], (LH,HH)->H at idwt[sw*sw*2..].
    for y in 0..sw {
        let (ll, hl, lh, hh) = (ll + y * sw, hl + y * sw, lh + y * sw, hh + y * sw);
        let l_dst = y * total;
        let h_dst = sw * sw * 2 + y * total;
        // Even coefficients.
        idwt[l_dst] = clampi16(buf[ll] as i32 - ((buf[hl] as i32 + buf[hl] as i32 + 1) >> 1));
        idwt[h_dst] = clampi16(buf[lh] as i32 - ((buf[hh] as i32 + buf[hh] as i32 + 1) >> 1));
        for n in 1..sw {
            let x = n << 1;
            idwt[l_dst + x] =
                clampi16(buf[ll + n] as i32 - ((buf[hl + n - 1] as i32 + buf[hl + n] as i32 + 1) >> 1));
            idwt[h_dst + x] =
                clampi16(buf[lh + n] as i32 - ((buf[hh + n - 1] as i32 + buf[hh + n] as i32 + 1) >> 1));
        }
        // Odd coefficients.
        for n in 0..sw - 1 {
            let x = n << 1;
            let ld = ((buf[hl + n] as i32) << 1)
                + (((idwt[l_dst + x] as i32) + (idwt[l_dst + x + 2] as i32)) >> 1);
            let hd = ((buf[hh + n] as i32) << 1)
                + (((idwt[h_dst + x] as i32) + (idwt[h_dst + x + 2] as i32)) >> 1);
            idwt[l_dst + x + 1] = clampi16(ld);
            idwt[h_dst + x + 1] = clampi16(hd);
        }
        let n = sw - 1;
        let x = n << 1;
        idwt[l_dst + x + 1] = clampi16(((buf[hl + n] as i32) << 1) + idwt[l_dst + x] as i32);
        idwt[h_dst + x + 1] = clampi16(((buf[hh + n] as i32) << 1) + idwt[h_dst + x] as i32);
    }

    // Vertical: (L,H) from idwt -> buf, results stored back in the original buffer.
    for x in 0..total {
        let l = x;
        let h = x + sw * total;
        let mut dst = base + x;
        buf[dst] = clampi16(idwt[l] as i32 - (((idwt[h] as i32) * 2 + 1) >> 1));
        let (mut l, mut h) = (l, h);
        for _ in 1..sw {
            l += total;
            h += total;
            let even = idwt[l] as i32 - ((idwt[h - total] as i32 + idwt[h] as i32 + 1) >> 1);
            buf[dst + 2 * total] = clampi16(even);
            let odd = ((idwt[h - total] as i32) << 1)
                + (((buf[dst] as i32) + (buf[dst + 2 * total] as i32)) >> 1);
            buf[dst + total] = clampi16(odd);
            dst += 2 * total;
        }
        let last = ((idwt[h] as i32) << 1) + (((buf[dst] as i32) * 2) >> 1);
        buf[dst + total] = clampi16(last);
    }
}

/// Classic 3-level inverse DWT over the 4096-coefficient band-concatenated buffer.
fn dwt_2d_decode(buf: &mut [i16], idwt: &mut [i16]) {
    dwt_block(buf, 3840, idwt, 8);
    dwt_block(buf, 3072, idwt, 16);
    dwt_block(buf, 0, idwt, 32);
}

// ---------------------------------------------------------------------------
// Inverse DWT — reduce-extrapolate variant, `rfx_dwt_2d_extrapolate_decode`
// ---------------------------------------------------------------------------

/// Horizontal lifting for the extrapolate DWT (`progressive_rfx_idwt_x`). Reads
/// the low/high bands from `src` and writes the interleaved reconstruction to
/// `dst`. Uses truncating `/2` division (NOT `>>1`) as in the reference.
#[allow(clippy::too_many_arguments)]
fn idwt_x(
    src: &[i16],
    low_off: usize,
    high_off: usize,
    dst: &mut [i16],
    dst_off: usize,
    low_step: usize,
    high_step: usize,
    dst_step: usize,
    n_low: usize,
    n_high: usize,
    n_dst: usize,
) {
    for i in 0..n_dst {
        let mut p_l = low_off + i * low_step;
        let mut p_h = high_off + i * high_step;
        let mut p_x = dst_off + i * dst_step;
        let mut h0 = src[p_h] as i32;
        p_h += 1;
        let mut l0 = src[p_l] as i32;
        p_l += 1;
        let mut x0 = clampi16(l0 - h0) as i32;
        let mut x2 = clampi16(l0 - h0) as i32;

        for _ in 0..n_high.saturating_sub(1) {
            let h1 = src[p_h] as i32;
            p_h += 1;
            l0 = src[p_l] as i32;
            p_l += 1;
            x2 = clampi16(l0 - ((h0 + h1) / 2)) as i32;
            let x1 = clampi16((x0 + x2) / 2 + 2 * h0);
            dst[p_x] = clampi16(x0);
            dst[p_x + 1] = x1;
            p_x += 2;
            x0 = x2;
            h0 = h1;
        }

        if n_low <= n_high + 1 {
            if n_low <= n_high {
                dst[p_x] = clampi16(x2);
                dst[p_x + 1] = clampi16(x2 + 2 * h0);
            } else {
                l0 = src[p_l] as i32;
                x0 = clampi16(l0 - h0) as i32;
                dst[p_x] = clampi16(x2);
                dst[p_x + 1] = clampi16((x0 + x2) / 2 + 2 * h0);
                dst[p_x + 2] = clampi16(x0);
            }
        } else {
            l0 = src[p_l] as i32;
            p_l += 1;
            x0 = clampi16(l0 - (h0 / 2)) as i32;
            dst[p_x] = clampi16(x2);
            dst[p_x + 1] = clampi16((x0 + x2) / 2 + 2 * h0);
            dst[p_x + 2] = clampi16(x0);
            l0 = src[p_l] as i32;
            dst[p_x + 3] = clampi16((x0 + l0) / 2);
        }
    }
}

/// Vertical lifting for the extrapolate DWT (`progressive_rfx_idwt_y`). Same
/// formula as [`idwt_x`] but striding down columns.
#[allow(clippy::too_many_arguments)]
fn idwt_y(
    src: &[i16],
    low_off: usize,
    high_off: usize,
    dst: &mut [i16],
    dst_off: usize,
    low_step: usize,
    high_step: usize,
    dst_step: usize,
    n_low: usize,
    n_high: usize,
    n_dst: usize,
) {
    for i in 0..n_dst {
        let mut p_l = low_off + i;
        let mut p_h = high_off + i;
        let mut p_x = dst_off + i;
        let mut h0 = src[p_h] as i32;
        p_h += high_step;
        let mut l0 = src[p_l] as i32;
        p_l += low_step;
        let mut x0 = clampi16(l0 - h0) as i32;
        let mut x2 = clampi16(l0 - h0) as i32;

        for _ in 0..n_high.saturating_sub(1) {
            let h1 = src[p_h] as i32;
            p_h += high_step;
            l0 = src[p_l] as i32;
            p_l += low_step;
            x2 = clampi16(l0 - ((h0 + h1) / 2)) as i32;
            let x1 = clampi16((x0 + x2) / 2 + 2 * h0);
            dst[p_x] = clampi16(x0);
            p_x += dst_step;
            dst[p_x] = x1;
            p_x += dst_step;
            x0 = x2;
            h0 = h1;
        }

        if n_low <= n_high + 1 {
            if n_low <= n_high {
                dst[p_x] = clampi16(x2);
                p_x += dst_step;
                dst[p_x] = clampi16(x2 + 2 * h0);
            } else {
                l0 = src[p_l] as i32;
                x0 = clampi16(l0 - h0) as i32;
                dst[p_x] = clampi16(x2);
                p_x += dst_step;
                dst[p_x] = clampi16((x0 + x2) / 2 + 2 * h0);
                p_x += dst_step;
                dst[p_x] = clampi16(x0);
            }
        } else {
            l0 = src[p_l] as i32;
            p_l += low_step;
            x0 = clampi16(l0 - (h0 / 2)) as i32;
            dst[p_x] = clampi16(x2);
            p_x += dst_step;
            dst[p_x] = clampi16((x0 + x2) / 2 + 2 * h0);
            p_x += dst_step;
            dst[p_x] = clampi16(x0);
            p_x += dst_step;
            l0 = src[p_l] as i32;
            dst[p_x] = clampi16((x0 + l0) / 2);
        }
    }
}

/// One level of the extrapolate inverse DWT (`progressive_rfx_dwt_2d_decode_block`).
fn dwt_extrapolate_block(buf: &mut [i16], base: usize, temp: &mut [i16], level: usize) {
    let n_l = band_l_count(level);
    let n_h = band_h_count(level);
    let hl = base;
    let lh = base + n_h * n_l;
    let hh = base + 2 * n_l * n_h;
    let ll = base + 2 * n_l * n_h + n_h * n_h;
    let dst_step = n_l + n_h;
    let l = 0usize;
    let h = n_l * dst_step;

    // horizontal (LL + HL -> L)
    idwt_x(buf, ll, hl, temp, l, n_l, n_h, dst_step, n_l, n_h, n_l);
    // horizontal (LH + HH -> H)
    idwt_x(buf, lh, hh, temp, h, n_l, n_h, dst_step, n_l, n_h, n_h);
    // vertical (L + H -> LL, written back to buf at `base`)
    idwt_y(temp, l, h, buf, base, dst_step, dst_step, dst_step, n_l, n_h, n_l + n_h);
}

/// 3-level extrapolate inverse DWT (`rfx_dwt_2d_extrapolate_decode`).
fn dwt_2d_extrapolate_decode(buf: &mut [i16], temp: &mut [i16]) {
    dwt_extrapolate_block(buf, 3807, temp, 3);
    dwt_extrapolate_block(buf, 3007, temp, 2);
    dwt_extrapolate_block(buf, 0, temp, 1);
}

// ---------------------------------------------------------------------------
// Dequant / differential / component decode
// ---------------------------------------------------------------------------

/// In-place left shift of a sub-band by `shift` (`progressive_rfx_decode_block`).
fn dequant(buf: &mut [i16], shift: i32) {
    if shift <= 0 {
        return;
    }
    for v in buf.iter_mut() {
        *v = ((*v as i32) << shift) as i16;
    }
}

/// In-place prefix-sum over a sub-band (`rfx_differential_decode`).
fn differential_decode(buf: &mut [i16]) {
    for i in 1..buf.len() {
        buf[i] = buf[i].wrapping_add(buf[i - 1]);
    }
}

/// Convert three 64×64 i16 Y/Cb/Cr planes to top-down RGBA8. Faithful port of
/// FreeRDP `generic_YCbCrToRGB_16s8u_P3AC4R` (BT.601). The IDWT/dequant output is
/// scaled up by 5 bits, so the final `>> 5` normalises it; `+4096` is the `128<<5`
/// luma DC offset. The fixed-point Cr/Cb multipliers are scaled by 2^16. Output
/// byte order is R,G,B,A to match the rest of this crate's RGBA pipeline.
fn ycbcr_to_rgba(y: &[i16], cb: &[i16], cr: &[i16], pool: &mut BufferPool) -> Vec<u8> {
    let n = TILE * TILE;
    let mut out = pool.acquire(n * 4);
    let clip = |v: i64| v.clamp(0, 255) as u8;

    // SIMD 2-pixel chunks using 64-bit lanes to hold the intermediate scaled
    // products without overflow.
    let mut i = 0usize;
    while i + 2 <= n {
        let yy = i64x2::new([y[i] as i64 + 4096, y[i + 1] as i64 + 4096]);
        let cbv = i64x2::new([cb[i] as i64, cb[i + 1] as i64]);
        let crv = i64x2::new([cr[i] as i64, cr[i + 1] as i64]);
        let yf = yy << 16;

        let r: i64x2 = (crv * i64x2::splat(91916) + yf) >> 16 >> 5;
        let g: i64x2 = (yf - cbv * i64x2::splat(22527) - crv * i64x2::splat(46819)) >> 16 >> 5;
        let b: i64x2 = (cbv * i64x2::splat(115992) + yf) >> 16 >> 5;

        let [r0, r1] = r.to_array();
        let [g0, g1] = g.to_array();
        let [b0, b1] = b.to_array();
        let r0 = r0.clamp(0, 255);
        let r1 = r1.clamp(0, 255);
        let g0 = g0.clamp(0, 255);
        let g1 = g1.clamp(0, 255);
        let b0 = b0.clamp(0, 255);
        let b1 = b1.clamp(0, 255);

        let o = i * 4;
        out[o..o + 8].copy_from_slice(&[
            r0 as u8, g0 as u8, b0 as u8, 0xFF, r1 as u8, g1 as u8, b1 as u8, 0xFF,
        ]);
        i += 2;
    }
    // Scalar tail (or odd final pixel).
    while i < n {
        let yf = ((y[i] as i64) + 4096) << 16;
        let cbv = cb[i] as i64;
        let crv = cr[i] as i64;
        let r = ((crv * 91916 + yf) >> 16) >> 5;
        let g = ((yf - cbv * 22527 - crv * 46819) >> 16) >> 5;
        let b = ((cbv * 115992 + yf) >> 16) >> 5;
        let o = i * 4;
        out[o] = clip(r);
        out[o + 1] = clip(g);
        out[o + 2] = clip(b);
        out[o + 3] = 0xFF;
        i += 1;
    }
    out
}

/// Decode one Y/Cb/Cr component of a base-layer tile into a 64×64 plane.
/// Port of `progressive_rfx_decode_component`: RLGR1 → save signs → per-band
/// dequant (+ LL differential) → 2D inverse DWT into `buffer`. `current` holds
/// the persisted pre-DWT coefficients (for diff tiles / future upgrades); `sign`
/// keeps the pre-dequant signs for Stage 3 upgrades.
#[allow(clippy::too_many_arguments)]
fn decode_component(
    shift: &Quant,
    data: &[u8],
    buffer: &mut [i16],
    temp: &mut [i16],
    current: &mut [i16],
    sign: &mut [i16],
    coeff_diff: bool,
    extrapolate: bool,
    perf: &mut PerfTotals,
) {
    let t0 = std::time::Instant::now();
    rlgr_decode(RlgrMode::Rlgr1, data, &mut buffer[..COEFFS]);
    perf.rlgr += t0.elapsed();
    sign[..COEFFS].copy_from_slice(&buffer[..COEFFS]);

    if !extrapolate {
        differential_decode(&mut buffer[4032..4096]);
        dequant(&mut buffer[0..1024], shift.hl1);
        dequant(&mut buffer[1024..2048], shift.lh1);
        dequant(&mut buffer[2048..3072], shift.hh1);
        dequant(&mut buffer[3072..3328], shift.hl2);
        dequant(&mut buffer[3328..3584], shift.lh2);
        dequant(&mut buffer[3584..3840], shift.hh2);
        dequant(&mut buffer[3840..3904], shift.hl3);
        dequant(&mut buffer[3904..3968], shift.lh3);
        dequant(&mut buffer[3968..4032], shift.hh3);
        dequant(&mut buffer[4032..4096], shift.ll3);
    } else {
        dequant(&mut buffer[0..1023], shift.hl1);
        dequant(&mut buffer[1023..2046], shift.lh1);
        dequant(&mut buffer[2046..3007], shift.hh1);
        dequant(&mut buffer[3007..3279], shift.hl2);
        dequant(&mut buffer[3279..3551], shift.lh2);
        dequant(&mut buffer[3551..3807], shift.hh2);
        dequant(&mut buffer[3807..3879], shift.hl3);
        dequant(&mut buffer[3879..3951], shift.lh3);
        dequant(&mut buffer[3951..4015], shift.hh3);
        differential_decode(&mut buffer[4015..4096]);
        dequant(&mut buffer[4015..4096], shift.ll3);
    }

    // `current` carries the dequantized coefficients across frames. A difference
    // tile saturating-adds its delta onto the previous frame and the running sum
    // is written back into BOTH buffers (FreeRDP `add_16s_inplace` writes both),
    // so successive difference frames accumulate. Otherwise `current` is replaced.
    if coeff_diff {
        for i in 0..COEFFS {
            let v = buffer[i].saturating_add(current[i]);
            buffer[i] = v;
            current[i] = v;
        }
    } else {
        current[..COEFFS].copy_from_slice(&buffer[..COEFFS]);
    }

    let t1 = std::time::Instant::now();
    if extrapolate {
        dwt_2d_extrapolate_decode(buffer, temp);
    } else {
        dwt_2d_decode(buffer, temp);
    }
    perf.dwt += t1.elapsed();
}

// ---------------------------------------------------------------------------
// UPGRADE (progressive refinement) pass
// ---------------------------------------------------------------------------

/// Per-component state for an UPGRADE pass (`RFX_PROGRESSIVE_UPGRADE_STATE`): two
/// bitstreams — SRL (for coefficients still zero) and RAW (for already-nonzero
/// coefficients) — plus the adaptive SRL run state. `non_ll` selects the LL3
/// (RAW-only) vs detail-band behaviour.
struct UpgradeState<'a> {
    srl: BitStream<'a>,
    raw: BitStream<'a>,
    kp: u32,
    nz: i32,
    mode: bool,
    non_ll: bool,
}

/// Read `num_bits` directly from the RAW bitstream (`rawShift`).
#[inline]
fn raw_shift(raw: &mut BitStream, num_bits: u32) -> i16 {
    let mask = (1u32 << num_bits) - 1;
    let input = ((raw.accumulator() >> (32 - num_bits)) & mask) as i16;
    raw.shift(num_bits as usize);
    input
}

/// Read one refinement value from the SRL bitstream (`progressive_rfx_srl_read`),
/// the adaptive zero-run / unary-magnitude coder for newly-significant coeffs.
fn srl_read(state: &mut UpgradeState, num_bits: u32) -> i16 {
    if state.nz != 0 {
        state.nz -= 1;
        return 0;
    }
    let k = state.kp / 8;
    if !state.mode {
        // zero encoding
        let bit = (state.srl.accumulator() & 0x8000_0000) != 0;
        state.srl.shift(1);
        if !bit {
            state.nz = 1i32 << k;
            state.kp += 4;
            if state.kp > 80 {
                state.kp = 80;
            }
            state.nz -= 1;
            return 0;
        } else {
            state.nz = 0;
            state.mode = true; // unary encoding next
            if k > 0 {
                let mask = (1u32 << k) - 1;
                state.nz = ((state.srl.accumulator() >> (32 - k)) & mask) as i32;
                state.srl.shift(k as usize);
            }
            if state.nz != 0 {
                state.nz -= 1;
                return 0;
            }
        }
    }
    state.mode = false; // zero encoding next
    // unary encoding: sign bit then a unary magnitude capped by num_bits.
    let sign = (state.srl.accumulator() & 0x8000_0000) != 0;
    state.srl.shift(1);
    state.kp = state.kp.saturating_sub(6);
    if num_bits == 1 {
        return if sign { -1 } else { 1 };
    }
    let mut mag: u32 = 1;
    let max = (1u32 << num_bits) - 1;
    while mag < max {
        let bit = (state.srl.accumulator() & 0x8000_0000) != 0;
        state.srl.shift(1);
        if bit {
            break;
        }
        mag += 1;
    }
    if mag > i16::MAX as u32 {
        mag = i16::MAX as u32;
    }
    if sign {
        -(mag as i32) as i16
    } else {
        mag as i16
    }
}

/// Refine one sub-band of `current` in place (`progressive_rfx_upgrade_block`).
/// LL3 (`non_ll == false`) reads every coefficient from RAW; detail bands read a
/// RAW refinement bit for already-significant coefficients and an SRL value for
/// ones that were still zero (recording the new sign).
fn upgrade_block(
    state: &mut UpgradeState,
    current: &mut [i16],
    sign: &mut [i16],
    shift: i32,
    num_bits: u32,
) {
    if num_bits < 1 {
        return;
    }
    if !state.non_ll {
        for v in current.iter_mut() {
            let input = raw_shift(&mut state.raw, num_bits) as i32;
            *v = (*v as i32 + (input << shift)) as i16;
        }
        return;
    }
    for i in 0..current.len() {
        let input: i32 = if sign[i] > 0 {
            raw_shift(&mut state.raw, num_bits) as i32
        } else if sign[i] < 0 {
            -(raw_shift(&mut state.raw, num_bits) as i32)
        } else {
            let v = srl_read(state, num_bits);
            sign[i] = v;
            v as i32
        };
        current[i] = (current[i] as i32 + (input << shift)) as i16;
    }
}

/// Refine one Y/Cb/Cr component for an UPGRADE tile, then re-run the inverse DWT
/// (`progressive_rfx_upgrade_component`). The 10 sub-bands use the extrapolate
/// layout offsets (as FreeRDP does for upgrades regardless of variant); only the
/// final DWT honours `extrapolate`.
#[allow(clippy::too_many_arguments)]
fn upgrade_component(
    shift: &Quant,
    num_bits: &Quant,
    buffer: &mut [i16],
    temp: &mut [i16],
    current: &mut [i16],
    sign: &mut [i16],
    srl_data: &[u8],
    raw_data: &[u8],
    extrapolate: bool,
    perf: &mut PerfTotals,
) {
    let t0 = std::time::Instant::now();
    let mut state = UpgradeState {
        srl: BitStream::new(srl_data),
        raw: BitStream::new(raw_data),
        kp: 8,
        nz: 0,
        mode: false,
        non_ll: true,
    };
    // Detail bands (RAW for significant, SRL for newly-significant).
    let bands: [(usize, usize, i32, u32); 9] = [
        (0, 1023, shift.hl1, num_bits.hl1 as u32),
        (1023, 1023, shift.lh1, num_bits.lh1 as u32),
        (2046, 961, shift.hh1, num_bits.hh1 as u32),
        (3007, 272, shift.hl2, num_bits.hl2 as u32),
        (3279, 272, shift.lh2, num_bits.lh2 as u32),
        (3551, 256, shift.hh2, num_bits.hh2 as u32),
        (3807, 72, shift.hl3, num_bits.hl3 as u32),
        (3879, 72, shift.lh3, num_bits.lh3 as u32),
        (3951, 64, shift.hh3, num_bits.hh3 as u32),
    ];
    for (off, len, sh, nb) in bands {
        upgrade_block(
            &mut state,
            &mut current[off..off + len],
            &mut sign[off..off + len],
            sh,
            nb,
        );
    }
    // LL3 (RAW only).
    state.non_ll = false;
    upgrade_block(
        &mut state,
        &mut current[4015..4096],
        &mut sign[4015..4096],
        shift.ll3,
        num_bits.ll3 as u32,
    );

    perf.upgrade += t0.elapsed();

    // reverse: buffer = current, then inverse DWT.
    let t1 = std::time::Instant::now();
    buffer[..COEFFS].copy_from_slice(&current[..COEFFS]);
    if extrapolate {
        dwt_2d_extrapolate_decode(buffer, temp);
    } else {
        dwt_2d_decode(buffer, temp);
    }
    perf.dwt += t1.elapsed();
}

// ---------------------------------------------------------------------------
// Tile state + decoder
// ---------------------------------------------------------------------------

/// Persisted per-component coefficient state for one tile position. `current` is
/// the accumulated post-dequant coefficients, `sign` the pre-dequant signs (both
/// in the band-concatenated extrapolate layout), and `bit_pos` the per-sub-band
/// bit position that each UPGRADE pass decrements to derive its refinement bits.
#[derive(Clone)]
struct CompState {
    current: Vec<i16>,
    sign: Vec<i16>,
    bit_pos: Quant,
}

impl Default for CompState {
    fn default() -> Self {
        Self {
            current: vec![0i16; COEFFS],
            sign: vec![0i16; COEFFS],
            bit_pos: Quant::default(),
        }
    }
}

#[derive(Default, Clone)]
struct TileState {
    comps: [CompState; 3],
}

/// A decoded progressive update placed on the surface: a `w`×`h` RGBA buffer at
/// surface pixel `(x, y)`. Tiles are decoded 64×64 but emitted CLIPPED to the
/// region's dirty rects (FreeRDP parity) — painting a full tile would stomp up
/// to 63 px of neighbouring content owned by other codecs with stale pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressiveTile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// One dirty rect from `RFX_PROGRESSIVE_REGION` (x, y, width, height).
#[derive(Debug, Clone, Copy)]
struct RegionRect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

/// Emit `rgba` (a full 64×64 tile at `(x, y)`) clipped to the region rects. With
/// no rects (defensive; the spec always sends them) the tile passes through
/// whole. A tile fully inside one rect moves the buffer without copying.
fn emit_clipped(out: &mut Vec<ProgressiveTile>, x: u32, y: u32, rgba: Vec<u8>, rects: &[RegionRect]) {
    let t = TILE as u32;
    if rects.is_empty() {
        out.push(ProgressiveTile { x, y, w: t, h: t, rgba });
        return;
    }
    for r in rects {
        let (rx, ry) = (r.x as u32, r.y as u32);
        if x >= rx && y >= ry && x + t <= rx + r.w as u32 && y + t <= ry + r.h as u32 {
            out.push(ProgressiveTile { x, y, w: t, h: t, rgba });
            return;
        }
    }
    for r in rects {
        let (rx, ry) = (r.x as u32, r.y as u32);
        let (rr, rb) = (rx + r.w as u32, ry + r.h as u32);
        let ix = x.max(rx);
        let iy = y.max(ry);
        let ir = (x + t).min(rr);
        let ib = (y + t).min(rb);
        if ir <= ix || ib <= iy {
            continue;
        }
        let (cw, ch) = ((ir - ix) as usize, (ib - iy) as usize);
        let mut crop = Vec::with_capacity(cw * ch * 4);
        for row in 0..ch {
            let sy = (iy - y) as usize + row;
            let so = (sy * TILE + (ix - x) as usize) * 4;
            crop.extend_from_slice(&rgba[so..so + cw * 4]);
        }
        out.push(ProgressiveTile {
            x: ix,
            y: iy,
            w: cw as u32,
            h: ch as u32,
            rgba: crop,
        });
    }
}

/// Tile-mix counters for one [`ProgressiveDecoder::decode`] call. Also traced at
/// debug level under the `prog` target, so a live run with `RUST_LOG=prog=debug`
/// shows the per-frame composition (and any skips, which mean client/server
/// coefficient state has diverged for those tiles).
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameStats {
    pub simple: u32,
    pub first: u32,
    pub upgrade: u32,
    /// SIMPLE/FIRST tiles carrying TILE_DIFFERENCE (delta vs the persisted state).
    pub diff: u32,
    pub skipped: u32,
}

/// Cumulative decode-stage wall time across a decoder's lifetime — cheap
/// always-on instrumentation (a few `Instant::now()` per tile vs ~70µs of work)
/// so the hot stage is measurable in offline replay and live sessions alike.
#[derive(Debug, Default, Clone, Copy)]
pub struct PerfTotals {
    pub rlgr: std::time::Duration,
    pub dwt: std::time::Duration,
    pub upgrade: std::time::Duration,
    pub ycbcr: std::time::Duration,
    pub tiles: u64,
}

/// Stateful RemoteFX Progressive decoder for one surface stream. Holds the
/// persistent per-tile coefficient state that difference/upgrade passes build on.
/// Each `WireToSurface2` payload is one [`Self::decode`] call.
pub struct ProgressiveDecoder {
    tiles: std::collections::HashMap<(u16, u16), TileState>,
    // Scratch reused across tiles: one decoded plane per component (held
    // simultaneously for the YCbCr→RGB pass) plus the DWT temp.
    planes: [Vec<i16>; 3],
    temp: Vec<i16>,
    stats: FrameStats,
    perf: PerfTotals,
    /// Reused 64×64 RGBA output buffers; avoids per-tile heap allocation on
    /// the decode thread.
    pool: BufferPool,
    /// Persistent scratch for the parallel batch workers, one entry per extra
    /// worker thread. Each worker previously allocated fresh planes/temp/pool
    /// on every frame (~40 KB per worker per frame of pure churn, and a cold
    /// RGBA pool each time); now the scratch survives across frames.
    worker_scratch: Vec<WorkerScratch>,
}

/// Per-worker decode scratch for the parallel batch path.
struct WorkerScratch {
    planes: [Vec<i16>; 3],
    temp: Vec<i16>,
    pool: BufferPool,
}

impl WorkerScratch {
    fn new() -> Self {
        Self {
            planes: [
                vec![0i16; COEFFS],
                vec![0i16; COEFFS],
                vec![0i16; COEFFS],
            ],
            temp: vec![0i16; COEFFS],
            pool: BufferPool::new(),
        }
    }
}

impl Default for ProgressiveDecoder {
    fn default() -> Self {
        Self {
            tiles: std::collections::HashMap::new(),
            planes: [
                vec![0i16; COEFFS],
                vec![0i16; COEFFS],
                vec![0i16; COEFFS],
            ],
            temp: vec![0i16; COEFFS],
            stats: FrameStats::default(),
            perf: PerfTotals::default(),
            pool: BufferPool::new(),
            worker_scratch: Vec::new(),
        }
    }
}

impl ProgressiveDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one `WireToSurface2` progressive payload into positioned RGBA tiles.
    pub fn decode(&mut self, codec_context_id: u32, data: &[u8]) -> Vec<ProgressiveTile> {
        self.stats = FrameStats::default();
        let mut tiles = Vec::new();
        self.walk_blocks(strip_length_prefix(data), &mut tiles);
        let s = self.stats;
        tracing::debug!(
            target: "prog",
            ctx = codec_context_id,
            bytes = data.len(),
            simple = s.simple,
            first = s.first,
            upgrade = s.upgrade,
            diff = s.diff,
            skipped = s.skipped,
            tiles_out = tiles.len(),
            "progressive decode"
        );
        tiles
    }

    /// Tile-mix counters from the most recent [`Self::decode`] call.
    pub fn last_stats(&self) -> FrameStats {
        self.stats
    }

    /// Cumulative per-stage decode times since this decoder was created.
    pub fn perf_totals(&self) -> PerfTotals {
        self.perf
    }

    /// Return an RGBA buffer to the decoder's pool so a later tile can reuse its
    /// capacity. Callers that copy tile data into a larger band should return the
    /// now-unused source buffers; buffers that are forwarded to the UI thread are
    /// simply dropped (the pool caps its retention).
    pub fn return_buffer(&mut self, buf: Vec<u8>) {
        self.pool.release(buf);
    }

    /// Acquire a zeroed RGBA buffer of at least `len` bytes from the pool.
    pub fn acquire_buffer(&mut self, len: usize) -> Vec<u8> {
        self.pool.acquire(len)
    }

    /// RDPGFX_CMDID_DELETEENCODINGCONTEXT: deliberately a NO-OP, matching FreeRDP
    /// (`gdi_DeleteEncodingContext` ignores it). The per-tile coefficient state
    /// must outlive context bookkeeping: servers issue deletes for contexts they
    /// are finished with while difference/upgrade tiles for the live stream still
    /// build on the persisted per-tile state. An earlier version cleared the map
    /// here, which zeroed the reference under every later difference tile — the
    /// "flat gray with embossed edges" video corruption. State is freed when the
    /// surface is deleted (the whole decoder is dropped).
    pub fn delete_context(&mut self, _codec_context_id: u32) {}

    /// Walk the top-level `WBT_*` blocks. Unknown/short blocks are skipped by
    /// their declared length; a malformed stream must never panic or over-read.
    fn walk_blocks(&mut self, data: &[u8], out: &mut Vec<ProgressiveTile>) {
        let mut off = 0usize;
        while off + 6 <= data.len() {
            let block_type = match u16le(data, off) {
                Some(t) => t,
                None => break,
            };
            let block_len = match u32le(data, off + 2) {
                Some(l) if l as usize >= 6 => l as usize,
                _ => break,
            };
            if off + block_len > data.len() {
                break;
            }
            // One-time diagnostic: the first block type/len we walk, so we can see
            // the stream framing even if no REGION is reached.
            {
                use std::sync::atomic::{AtomicBool, Ordering};
                static LOGGED: AtomicBool = AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    tracing::info!(
                        block_type = format!("0x{block_type:04x}"),
                        block_len,
                        total = data.len(),
                        "RFX Progressive first block"
                    );
                }
            }
            let block = &data[off..off + block_len];
            match block_type {
                WBT_SYNC | WBT_FRAME_BEGIN | WBT_FRAME_END | WBT_CONTEXT => {
                    // Stream/frame/context framing carries no tiles. The context's
                    // RFX_SUBBAND_DIFFING flag does not affect decode (it is unused
                    // in the reference), so nothing to track here for the base layer.
                }
                WBT_REGION => self.decode_region(&block[6..], out),
                _ => {}
            }
            off += block_len;
        }
    }

    /// Parse a `WBT_REGION` payload (after its 6-byte block header) and decode its
    /// tiles. `RFX_PROGRESSIVE_REGION`: tileSize(1) numRects(2) numQuant(1)
    /// numProgQuant(1) flags(1) numTiles(2) tileDataSize(4) then rects, quantVals,
    /// quantProgVals, and the tile blocks.
    fn decode_region(&mut self, b: &[u8], out: &mut Vec<ProgressiveTile>) {
        let num_rects = match u16le(b, 1) {
            Some(n) => n as usize,
            None => return,
        };
        let num_quant = b.get(3).copied().unwrap_or(0) as usize;
        let num_prog_quant = b.get(4).copied().unwrap_or(0) as usize;
        let flags = b.get(5).copied().unwrap_or(0);
        let num_tiles = match u16le(b, 6) {
            Some(n) => n as usize,
            None => return,
        };
        let tile_data_size = match u32le(b, 8) {
            Some(n) => n as usize,
            None => return,
        };
        let extrapolate = flags & RFX_DWT_REDUCE_EXTRAPOLATE != 0;

        // One-time diagnostic: which DWT variant + table sizes the server uses.
        // Tells us, on the first video frame, whether the extrapolate or classic
        // path is exercised (and that regions are arriving at all).
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    extrapolate,
                    num_tiles,
                    num_quant,
                    num_prog_quant,
                    "RFX Progressive region (first)"
                );
            }
        }

        let mut off = 12usize;
        // Dirty rects: tile placement uses xIdx/yIdx, but emitted pixels are
        // CLIPPED to these (the parts of each 64×64 tile outside them belong to
        // other codecs' content and must not be repainted with stale tile data).
        if off + num_rects * 8 > b.len() {
            return;
        }
        let rects: Vec<RegionRect> = (0..num_rects)
            .map(|i| {
                let o = off + i * 8;
                RegionRect {
                    x: u16::from_le_bytes([b[o], b[o + 1]]),
                    y: u16::from_le_bytes([b[o + 2], b[o + 3]]),
                    w: u16::from_le_bytes([b[o + 4], b[o + 5]]),
                    h: u16::from_le_bytes([b[o + 6], b[o + 7]]),
                }
            })
            .collect();
        off += num_rects * 8;

        // Simple quant table.
        if off + num_quant * 5 > b.len() {
            return;
        }
        let quants: Vec<Quant> = (0..num_quant)
            .map(|i| Quant::read(&b[off + i * 5..]))
            .collect();
        off += num_quant * 5;

        // Progressive quant table.
        if off + num_prog_quant * 16 > b.len() {
            return;
        }
        let prog_quants: Vec<ProgQuant> = (0..num_prog_quant)
            .map(|i| ProgQuant::read(&b[off + i * 16..]))
            .collect();
        off += num_prog_quant * 16;

        // Tile blocks.
        let end = (off + tile_data_size).min(b.len());
        let tile_data = &b[off..end];
        self.decode_tiles(tile_data, num_tiles, &quants, &prog_quants, &rects, extrapolate, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_tiles(
        &mut self,
        data: &[u8],
        num_tiles: usize,
        quants: &[Quant],
        prog_quants: &[ProgQuant],
        rects: &[RegionRect],
        extrapolate: bool,
        out: &mut Vec<ProgressiveTile>,
    ) {
        // Tiles in a region touch disjoint per-tile state, so they decode in
        // PARALLEL: parse + take state serially, fan the heavy math (RLGR + DWT +
        // YCbCr, ~70µs/tile) across threads, then restore state and emit in wire
        // order. A duplicate tile position flushes the batch first, preserving
        // strict sequential semantics in that (never observed) case.
        let mut batch: Vec<Slot> = Vec::new();
        let mut in_batch: std::collections::HashSet<(u16, u16)> = std::collections::HashSet::new();
        let mut off = 0usize;
        for _ in 0..num_tiles {
            if off + 6 > data.len() {
                break;
            }
            let block_type = u16le(data, off).unwrap_or(0);
            let block_len = u32le(data, off + 2).unwrap_or(0) as usize;
            if block_len < 6 || off + block_len > data.len() {
                break;
            }
            let body = &data[off + 6..off + block_len];
            off += block_len;
            let parsed = match block_type {
                WBT_TILE_SIMPLE | WBT_TILE_FIRST => {
                    if block_type == WBT_TILE_SIMPLE {
                        self.stats.simple += 1;
                    } else {
                        self.stats.first += 1;
                    }
                    if body.get(7).copied().unwrap_or(0) & RFX_TILE_DIFFERENCE != 0 {
                        self.stats.diff += 1;
                    }
                    parse_tile_first(block_type, body, quants, prog_quants)
                }
                WBT_TILE_UPGRADE => {
                    self.stats.upgrade += 1;
                    parse_tile_upgrade(body, quants, prog_quants)
                }
                _ => continue,
            };
            let bound = parsed.and_then(|p| {
                if in_batch.contains(&p.key()) {
                    self.flush_batch(&mut batch, extrapolate, rects, out);
                    in_batch.clear();
                }
                self.bind(p)
            });
            match bound {
                Ok(slot) => {
                    in_batch.insert(slot.parsed.key());
                    batch.push(slot);
                }
                Err(reason) => {
                    self.stats.skipped += 1;
                    // A skipped tile means our coefficient state has diverged from
                    // the server's model for that position — it will render stale
                    // or wrong until a non-diff FIRST re-keys it. Always worth a
                    // (rate-limited) warning, not just a debug count.
                    use std::sync::atomic::{AtomicU32, Ordering};
                    static WARNED: AtomicU32 = AtomicU32::new(0);
                    let n = WARNED.fetch_add(1, Ordering::Relaxed);
                    if n < 16 || n % 256 == 0 {
                        tracing::warn!(
                            target: "prog",
                            reason,
                            block_type = format!("0x{block_type:04x}"),
                            x_idx = u16le(body, 3).unwrap_or(0xFFFF),
                            y_idx = u16le(body, 5).unwrap_or(0xFFFF),
                            nth = n + 1,
                            "progressive tile skipped"
                        );
                    }
                }
            }
        }
        self.flush_batch(&mut batch, extrapolate, rects, out);

        // One-time diagnostic: tile-type composition of the first frame — confirms
        // whether difference/upgrade tiles are present and how many tiles we skip.
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                let s = self.stats;
                tracing::info!(
                    simple = s.simple,
                    first = s.first,
                    upgrade = s.upgrade,
                    difference = s.diff,
                    skipped = s.skipped,
                    "RFX Progressive tile mix (first frame)"
                );
            }
        }
    }

    /// Move a parsed tile's persisted state out of the map and advance its
    /// bit positions, validating BEFORE mutating anything (an upgrade whose
    /// numBits underflows must leave the state untouched). The state is restored
    /// to the map by [`Self::flush_batch`] after decode.
    fn bind<'a>(&mut self, parsed: ParsedTile<'a>) -> Result<Slot<'a>, &'static str> {
        match &parsed {
            ParsedTile::First { x_idx, y_idx, bitpos, .. } => {
                let entry = self.tiles.entry((*x_idx, *y_idx)).or_default();
                let mut state = std::mem::take(entry);
                for ci in 0..3 {
                    state.comps[ci].bit_pos = bitpos[ci];
                }
                Ok(Slot {
                    parsed,
                    num_bits: [Quant::default(); 3],
                    state,
                    rgba: None,
                })
            }
            ParsedTile::Upgrade {
                x_idx,
                y_idx,
                new_bitpos,
                ..
            } => {
                // The tile must already exist from a FIRST pass, else there's
                // nothing to refine — skip rather than guess.
                let entry = self
                    .tiles
                    .get_mut(&(*x_idx, *y_idx))
                    .ok_or("upgrade: unknown tile (no FIRST seen)")?;
                // numBits = oldBitPos - newBitPos (per sub-band); validate all
                // three components before touching any state.
                let nb = "upgrade: numbits underflow (quality went backwards)";
                let num_bits = [
                    entry.comps[0].bit_pos.sub(&new_bitpos[0]).ok_or(nb)?,
                    entry.comps[1].bit_pos.sub(&new_bitpos[1]).ok_or(nb)?,
                    entry.comps[2].bit_pos.sub(&new_bitpos[2]).ok_or(nb)?,
                ];
                let mut state = std::mem::take(entry);
                for ci in 0..3 {
                    state.comps[ci].bit_pos = new_bitpos[ci];
                }
                Ok(Slot {
                    parsed,
                    num_bits,
                    state,
                    rgba: None,
                })
            }
        }
    }

    /// Decode every slot in the batch (in parallel when it's large enough),
    /// restore the mutated tile states into the map, and emit the decoded tiles
    /// clipped to the region rects, in wire order.
    fn flush_batch(
        &mut self,
        batch: &mut Vec<Slot>,
        extrapolate: bool,
        rects: &[RegionRect],
        out: &mut Vec<ProgressiveTile>,
    ) {
        if batch.is_empty() {
            return;
        }
        // Below this, thread spawn overhead outweighs the win.
        const PAR_MIN: usize = 24;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(batch.len().div_ceil(8))
            .min(16);
        if batch.len() >= PAR_MIN && threads > 1 {
            let per = batch.len().div_ceil(threads);
            // The calling thread decodes the first chunk itself (one fewer
            // spawn+join), and each spawned worker gets a persistent scratch
            // entry — planes/temp and a warmed RGBA pool reused across frames
            // instead of reallocated per frame per worker. (A fully persistent
            // thread pool would need lifetime-erased pointers — `Slot` borrows
            // the wire buffer — and this crate deliberately denies unsafe
            // code, so the threads themselves remain scoped.)
            while self.worker_scratch.len() + 1 < threads {
                self.worker_scratch.push(WorkerScratch::new());
            }
            let mut chunks = batch.chunks_mut(per);
            let own = chunks.next();
            let own_planes = &mut self.planes;
            let own_temp = &mut self.temp;
            let own_pool = &mut self.pool;
            let own_perf = &mut self.perf;
            let worker_scratch = &mut self.worker_scratch;
            let perfs: Vec<PerfTotals> = std::thread::scope(|s| {
                let handles: Vec<_> = chunks
                    .zip(worker_scratch.iter_mut())
                    .map(|(chunk, scratch)| {
                        s.spawn(move || {
                            let mut perf = PerfTotals::default();
                            for slot in chunk {
                                decode_slot(
                                    slot,
                                    extrapolate,
                                    &mut scratch.planes,
                                    &mut scratch.temp,
                                    &mut scratch.pool,
                                    &mut perf,
                                );
                            }
                            perf
                        })
                    })
                    .collect();
                // Our own share decodes here while the workers run theirs.
                if let Some(chunk) = own {
                    for slot in chunk {
                        decode_slot(slot, extrapolate, own_planes, own_temp, own_pool, own_perf);
                    }
                }
                handles
                    .into_iter()
                    .map(|h| h.join().expect("progressive decode worker panicked"))
                    .collect()
            });
            for p in perfs {
                self.perf.rlgr += p.rlgr;
                self.perf.dwt += p.dwt;
                self.perf.upgrade += p.upgrade;
                self.perf.ycbcr += p.ycbcr;
                self.perf.tiles += p.tiles;
            }
        } else {
            for slot in batch.iter_mut() {
                decode_slot(
                    slot,
                    extrapolate,
                    &mut self.planes,
                    &mut self.temp,
                    &mut self.pool,
                    &mut self.perf,
                );
            }
        }
        for slot in batch.drain(..) {
            let (x_idx, y_idx) = slot.parsed.key();
            self.tiles.insert((x_idx, y_idx), slot.state);
            if let Some(rgba) = slot.rgba {
                emit_clipped(
                    out,
                    x_idx as u32 * TILE as u32,
                    y_idx as u32 * TILE as u32,
                    rgba,
                    rects,
                );
            }
        }
    }

}

/// A fully parsed tile block, holding resolved quant values and slices into the
/// region's tile data — everything the (possibly parallel) decode needs except
/// the persisted tile state.
enum ParsedTile<'a> {
    First {
        x_idx: u16,
        y_idx: u16,
        /// quant + progQuant per component; persisted as the tile's bit position.
        bitpos: [Quant; 3],
        /// Dequant left-shifts (= bitpos - 1).
        shift: [Quant; 3],
        coeff_diff: bool,
        /// Y/Cb/Cr RLGR1 streams.
        data: [&'a [u8]; 3],
    },
    Upgrade {
        x_idx: u16,
        y_idx: u16,
        new_bitpos: [Quant; 3],
        shift: [Quant; 3],
        /// Y/Cb/Cr SRL streams.
        srl: [&'a [u8]; 3],
        /// Y/Cb/Cr RAW streams.
        raw: [&'a [u8]; 3],
    },
}

impl ParsedTile<'_> {
    fn key(&self) -> (u16, u16) {
        match self {
            ParsedTile::First { x_idx, y_idx, .. } | ParsedTile::Upgrade { x_idx, y_idx, .. } => {
                (*x_idx, *y_idx)
            }
        }
    }
}

/// One tile's unit of decode work: the parsed block plus its persisted state
/// (taken out of the decoder's map so workers hold disjoint &mut state).
struct Slot<'a> {
    parsed: ParsedTile<'a>,
    /// Upgrade refinement bit counts (oldBitPos - newBitPos); zeros for FIRST.
    num_bits: [Quant; 3],
    state: TileState,
    rgba: Option<Vec<u8>>,
}


/// Decode one slot: run the component transforms against the slot's own state
/// and produce the tile's RGBA. Pure with respect to everything but the slot and
/// the caller-owned scratch, so slots decode safely in parallel.
fn decode_slot(
    slot: &mut Slot,
    extrapolate: bool,
    planes: &mut [Vec<i16>; 3],
    temp: &mut [i16],
    pool: &mut BufferPool,
    perf: &mut PerfTotals,
) {
    match &slot.parsed {
        ParsedTile::First {
            shift,
            coeff_diff,
            data,
            ..
        } => {
            for ci in 0..3 {
                let comp = &mut slot.state.comps[ci];
                decode_component(
                    &shift[ci],
                    data[ci],
                    &mut planes[ci],
                    temp,
                    &mut comp.current,
                    &mut comp.sign,
                    *coeff_diff,
                    extrapolate,
                    perf,
                );
            }
        }
        ParsedTile::Upgrade {
            shift, srl, raw, ..
        } => {
            for ci in 0..3 {
                let comp = &mut slot.state.comps[ci];
                upgrade_component(
                    &shift[ci],
                    &slot.num_bits[ci],
                    &mut planes[ci],
                    temp,
                    &mut comp.current,
                    &mut comp.sign,
                    srl[ci],
                    raw[ci],
                    extrapolate,
                    perf,
                );
            }
        }
    }
    let t0 = std::time::Instant::now();
    slot.rgba = Some(ycbcr_to_rgba(&planes[0],
        &planes[1],
        &planes[2],
        pool,
    ));
    perf.ycbcr += t0.elapsed();
    perf.tiles += 1;
}

/// Parse a SIMPLE or FIRST tile block (the base-quality layer). Header port of
/// `progressive_decompress_tile_first`. SIMPLE has no `quality` byte and uses
/// the implicit full-quality progressive quant (all zero); FIRST selects a
/// coarser one from the region table.
fn parse_tile_first<'a>(
    block_type: u16,
    body: &'a [u8],
    quants: &[Quant],
    prog_quants: &[ProgQuant],
) -> Result<ParsedTile<'a>, &'static str> {
    let is_first = block_type == WBT_TILE_FIRST;
    let hdr = "first/simple: short header";
    let q_idx_y = *body.first().ok_or(hdr)?;
    let q_idx_cb = *body.get(1).ok_or(hdr)?;
    let q_idx_cr = *body.get(2).ok_or(hdr)?;
    let x_idx = u16le(body, 3).ok_or(hdr)?;
    let y_idx = u16le(body, 5).ok_or(hdr)?;
    let flags = *body.get(7).ok_or(hdr)?;
    let (quality, mut p) = if is_first {
        (*body.get(8).ok_or(hdr)?, 9usize)
    } else {
        (0xFFu8, 8usize)
    };
    let y_len = u16le(body, p).ok_or(hdr)? as usize;
    let cb_len = u16le(body, p + 2).ok_or(hdr)? as usize;
    let cr_len = u16le(body, p + 4).ok_or(hdr)? as usize;
    let _tail_len = u16le(body, p + 6).ok_or(hdr)? as usize;
    p += 8;
    let short = "first/simple: short component data";
    let y_data = body.get(p..p + y_len).ok_or(short)?;
    let cb_data = body.get(p + y_len..p + y_len + cb_len).ok_or(short)?;
    let cr_data = body
        .get(p + y_len + cb_len..p + y_len + cb_len + cr_len)
        .ok_or(short)?;

    // Resolve quant tables (out-of-range indices = corrupt tile → skip).
    let oor = "first/simple: quant index out of range";
    let quant_y = quants.get(q_idx_y as usize).ok_or(oor)?;
    let quant_cb = quants.get(q_idx_cb as usize).ok_or(oor)?;
    let quant_cr = quants.get(q_idx_cr as usize).ok_or(oor)?;
    let prog = if quality == 0xFF {
        ProgQuant::default()
    } else {
        *prog_quants
            .get(quality as usize)
            .ok_or("first: prog quality out of range")?
    };

    // bitPos = quant + progQuant; shift = bitPos - 1 (the dequant left shifts).
    // bitPos is persisted so a later UPGRADE can derive its refinement bits.
    let bitpos = [
        quant_y.add(&prog.y),
        quant_cb.add(&prog.cb),
        quant_cr.add(&prog.cr),
    ];
    let under = "first/simple: bitpos underflow";
    let shift = [
        bitpos[0].lsub(1).ok_or(under)?,
        bitpos[1].lsub(1).ok_or(under)?,
        bitpos[2].lsub(1).ok_or(under)?,
    ];

    Ok(ParsedTile::First {
        x_idx,
        y_idx,
        bitpos,
        shift,
        coeff_diff: flags & RFX_TILE_DIFFERENCE != 0,
        data: [y_data, cb_data, cr_data],
    })
}

/// Parse a TILE_UPGRADE refinement block. Header port of
/// `progressive_decompress_tile_upgrade`: quantIdx Y/Cb/Cr(3) xIdx(2) yIdx(2)
/// quality(1) then six u16 lengths, then the six SRL/RAW blobs.
fn parse_tile_upgrade<'a>(
    body: &'a [u8],
    quants: &[Quant],
    prog_quants: &[ProgQuant],
) -> Result<ParsedTile<'a>, &'static str> {
    let hdr = "upgrade: short header";
    let q_idx_y = *body.first().ok_or(hdr)?;
    let q_idx_cb = *body.get(1).ok_or(hdr)?;
    let q_idx_cr = *body.get(2).ok_or(hdr)?;
    let x_idx = u16le(body, 3).ok_or(hdr)?;
    let y_idx = u16le(body, 5).ok_or(hdr)?;
    let quality = *body.get(7).ok_or(hdr)?;
    let y_srl = u16le(body, 8).ok_or(hdr)? as usize;
    let y_raw = u16le(body, 10).ok_or(hdr)? as usize;
    let cb_srl = u16le(body, 12).ok_or(hdr)? as usize;
    let cb_raw = u16le(body, 14).ok_or(hdr)? as usize;
    let cr_srl = u16le(body, 16).ok_or(hdr)? as usize;
    let cr_raw = u16le(body, 18).ok_or(hdr)? as usize;
    let mut p = 20usize;
    let mut take = |len: usize| -> Option<&'a [u8]> {
        let s = body.get(p..p + len)?;
        p += len;
        Some(s)
    };
    let short = "upgrade: short srl/raw data";
    let y_srl_d = take(y_srl).ok_or(short)?;
    let y_raw_d = take(y_raw).ok_or(short)?;
    let cb_srl_d = take(cb_srl).ok_or(short)?;
    let cb_raw_d = take(cb_raw).ok_or(short)?;
    let cr_srl_d = take(cr_srl).ok_or(short)?;
    let cr_raw_d = take(cr_raw).ok_or(short)?;

    let oor = "upgrade: quant index out of range";
    let quant_y = quants.get(q_idx_y as usize).ok_or(oor)?;
    let quant_cb = quants.get(q_idx_cb as usize).ok_or(oor)?;
    let quant_cr = quants.get(q_idx_cr as usize).ok_or(oor)?;
    let prog = if quality == 0xFF {
        ProgQuant::default()
    } else {
        *prog_quants
            .get(quality as usize)
            .ok_or("upgrade: prog quality out of range")?
    };
    let new_bitpos = [
        quant_y.add(&prog.y),
        quant_cb.add(&prog.cb),
        quant_cr.add(&prog.cr),
    ];
    let under = "upgrade: bitpos underflow";
    let shift = [
        new_bitpos[0].lsub(1).ok_or(under)?,
        new_bitpos[1].lsub(1).ok_or(under)?,
        new_bitpos[2].lsub(1).ok_or(under)?,
    ];

    Ok(ParsedTile::Upgrade {
        x_idx,
        y_idx,
        new_bitpos,
        shift,
        srl: [y_srl_d, cb_srl_d, cr_srl_d],
        raw: [y_raw_d, cb_raw_d, cr_raw_d],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_counts_tile_correctly() {
        // Each level's L+H spans its parent block: 64, 33, 17 (reduce-extrapolate).
        assert_eq!((band_l_count(1), band_h_count(1)), (33, 31));
        assert_eq!((band_l_count(2), band_h_count(2)), (17, 16));
        assert_eq!((band_l_count(3), band_h_count(3)), (9, 8));
    }

    #[test]
    fn extrapolate_blocks_fill_4096() {
        // The three nested extrapolate blocks must exactly cover the 4096 buffer.
        for (base, level) in [(3807usize, 3usize), (3007, 2), (0, 1)] {
            let n_l = band_l_count(level);
            let n_h = band_h_count(level);
            let size = (n_l + n_h) * (n_l + n_h);
            assert_eq!(base + size, COEFFS, "level {level} block must end at 4096");
        }
    }

    #[test]
    fn dwt_round_to_flat_is_stable_classic() {
        // A buffer that's all-zero must inverse-DWT to all-zero (both variants).
        let mut buf = vec![0i16; COEFFS];
        let mut tmp = vec![0i16; COEFFS];
        dwt_2d_decode(&mut buf, &mut tmp);
        assert!(buf.iter().all(|&v| v == 0));
        let mut buf = vec![0i16; COEFFS];
        dwt_2d_extrapolate_decode(&mut buf, &mut tmp);
        assert!(buf.iter().all(|&v| v == 0));
    }

    #[test]
    fn quant_read_unpacks_nibbles_in_order() {
        // bytes -> LL3,HL3,LH3,HH3,HL2,LH2,HH2,HL1,LH1,HH1
        let q = Quant::read(&[0x21, 0x43, 0x65, 0x87, 0xA9]);
        assert_eq!(q.ll3, 1);
        assert_eq!(q.hl3, 2);
        assert_eq!(q.hh1, 0xA);
    }

    #[test]
    fn quant_lsub_fails_when_underflow() {
        let q = Quant {
            ll3: 0,
            ..Default::default()
        };
        assert!(q.lsub(1).is_none());
        let q = Quant {
            ll3: 6,
            hl3: 6,
            lh3: 6,
            hh3: 6,
            hl2: 6,
            lh2: 6,
            hh2: 6,
            hl1: 6,
            lh1: 6,
            hh1: 6,
        };
        assert_eq!(q.lsub(1).unwrap().ll3, 5);
    }

    #[test]
    fn strips_4byte_length_prefix_before_sync() {
        // Real wire form: [u32 len][WBT_SYNC ...]. The stream after stripping must
        // begin at the SYNC block; a stream already at a block is left untouched.
        let blocks = [0xC0u8, 0xCC, 0x0C, 0x00, 0x00, 0x00, 0xCA, 0xAC, 0xCC, 0xCA, 0x00, 0x01];
        let mut prefixed = (blocks.len() as u32).to_le_bytes().to_vec();
        prefixed.extend_from_slice(&blocks);
        assert_eq!(strip_length_prefix(&prefixed), &blocks);
        assert_eq!(strip_length_prefix(&blocks), &blocks);
    }

    #[test]
    fn decoder_never_panics_on_garbage() {
        let patterns: [&[u8]; 6] = [
            &[],
            &[0xFF],
            &[0xFF; 7],
            &[0x00; 64],
            &[0xC0, 0xCC, 0xFF, 0xFF, 0xFF, 0x7F], // SYNC header, absurd len
            &[0xC4, 0xCC, 0x06, 0x00, 0x00, 0x00], // REGION header, len==6 (empty)
        ];
        let mut dec = ProgressiveDecoder::new();
        for p in patterns {
            let _ = dec.decode(0, p);
        }
        dec.delete_context(0);
    }

    #[test]
    fn region_with_one_empty_tile_positions_it() {
        // Build a REGION with one SIMPLE tile at (xIdx=2,yIdx=3) with empty
        // component data. It must decode (to a uniform tile) and be positioned.
        let mut tile = Vec::new();
        tile.extend_from_slice(&WBT_TILE_SIMPLE.to_le_bytes());
        let tile_body_len = 16u32; // 6 header + 16 payload below... computed after
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0]); // quantIdx Y/Cb/Cr = 0
        body.extend_from_slice(&2u16.to_le_bytes()); // xIdx
        body.extend_from_slice(&3u16.to_le_bytes()); // yIdx
        body.push(0); // flags
        body.extend_from_slice(&0u16.to_le_bytes()); // yLen
        body.extend_from_slice(&0u16.to_le_bytes()); // cbLen
        body.extend_from_slice(&0u16.to_le_bytes()); // crLen
        body.extend_from_slice(&0u16.to_le_bytes()); // tailLen
        let _ = tile_body_len;
        tile.extend_from_slice(&((6 + body.len()) as u32).to_le_bytes());
        tile.extend_from_slice(&body);

        // REGION payload: tileSize, numRects=0, numQuant=1, numProgQuant=0,
        // flags=0, numTiles=1, tileDataSize, then one quant set + the tile.
        let mut region = Vec::new();
        region.push(64); // tileSize
        region.extend_from_slice(&0u16.to_le_bytes()); // numRects
        region.push(1); // numQuant
        region.push(0); // numProgQuant
        region.push(0); // flags (non-extrapolate)
        region.extend_from_slice(&1u16.to_le_bytes()); // numTiles
        region.extend_from_slice(&(tile.len() as u32).to_le_bytes()); // tileDataSize
        // One quant set: every nibble = 1 so `lsub(1)` succeeds (shift 0 = no-op).
        region.extend_from_slice(&[0x11u8, 0x11, 0x11, 0x11, 0x11]);
        region.extend_from_slice(&tile);

        let mut block = Vec::new();
        block.extend_from_slice(&WBT_REGION.to_le_bytes());
        block.extend_from_slice(&((6 + region.len()) as u32).to_le_bytes());
        block.extend_from_slice(&region);

        let mut dec = ProgressiveDecoder::new();
        let tiles = dec.decode(0, &block);
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].x, tiles[0].y), (2 * 64, 3 * 64));
        assert_eq!(tiles[0].rgba.len(), TILE * TILE * 4);
    }
}
