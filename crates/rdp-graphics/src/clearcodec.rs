//! ClearCodec (MS-RDPEGFX `RDPGFX_CODECID_CLEARCODEC` = 0x08) decoder.
//!
//! ClearCodec is Windows' CPU desktop codec — the one a host with no GPU (so no
//! H.264 encode) streams the desktop with. A bitmap is composed of up to three
//! layers written in order, last-writer-wins:
//!   1. **residual** — a whole-tile RLE of solid colour runs (the background);
//!   2. **bands** — vertical columns ("vBars") with a two-level vBar cache;
//!   3. **subcodecs** — rectangular regions coded RAW, NSCodec, or RLEX (palette).
//! A small-tile **glyph cache** (4000 entries) lets the server replay repeated
//! tiles by index. This module is the pure decoder; it outputs RGBA8 and holds
//! the glyph/vBar caches across calls (one channel-global [`ClearDecoder`],
//! shared across all surfaces — MS-RDPEGFX/FreeRDP parity).
//!
//! Wire integers are little-endian; on-wire pixels are `B,G,R`. Validated by
//! structural unit tests here; exact server interop is confirmed live.

use wide::u8x16;

const GLYPH_CACHE: usize = 4000;
const VBAR_CACHE: usize = 32768;
const SHORT_VBAR_CACHE: usize = 16384;

const FLAG_GLYPH_INDEX: u8 = 0x01;
const FLAG_GLYPH_HIT: u8 = 0x02;
const FLAG_CACHE_RESET: u8 = 0x04;

const SUBCODEC_RAW: u8 = 0;
const SUBCODEC_NSCODEC: u8 = 1;
const SUBCODEC_RLEX: u8 = 2;

/// A cached column or glyph: `count` pixels of RGBA (`pixels.len() == count*4`).
#[derive(Default, Clone)]
struct Pixels {
    count: usize,
    rgba: Vec<u8>,
}

/// Stateful ClearCodec decoder. Holds the glyph and vBar caches that persist
/// across surface updates on one RDPGFX stream.
pub struct ClearDecoder {
    glyphs: Vec<Pixels>,
    vbar: Vec<Pixels>,
    short_vbar: Vec<Pixels>,
    vbar_cursor: usize,
    short_vbar_cursor: usize,
}

impl Default for ClearDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Little-endian cursor reader returning `None` past the end (never panics).
struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, p: 0 }
    }
    fn remaining(&self) -> usize {
        self.d.len().saturating_sub(self.p)
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes([*self.d.get(self.p)?, *self.d.get(self.p + 1)?]);
        self.p += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes([
            *self.d.get(self.p)?,
            *self.d.get(self.p + 1)?,
            *self.d.get(self.p + 2)?,
            *self.d.get(self.p + 3)?,
        ]);
        self.p += 4;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
    /// A run length with the ClearCodec 0xFF/0xFFFF escalation.
    fn run_length(&mut self, first: u8) -> Option<u32> {
        if first < 0xFF {
            return Some(first as u32);
        }
        let two = self.u16()?;
        if two < 0xFFFF {
            return Some(two as u32);
        }
        self.u32()
    }
}

/// Whether decoding this CLEARCODEC_BITMAP_STREAM into a `width`×`height` tile
/// actually reads the seed (the destination's previous pixels). Building the
/// seed costs a `width*height*4` allocation + copy per tile, so the ClearCodec
/// hot path (hundreds of tiles per video frame) skips it when the composition
/// fully defines every pixel. Conservatively returns `true` (seed needed) for
/// anything it can't prove is fully covered.
///
/// Provably seed-free cases:
/// - a **glyph hit**, which replays a cached tile verbatim and never reads the
///   seed; and
/// - a composition whose only layer is a **single subcodec region covering the
///   whole tile** with a full-fill codec (RAW or NSCodec) — every pixel is
///   written. (RLEX is excluded: a short palette run could leave pixels unset.)
///
/// NOTE: a glyph *being cached* (GLYPH_INDEX set, GLYPH_HIT clear) is NOT
/// seed-free on real servers — observed live that decoding cached glyphs from
/// black blanks all text, so this server composites glyph layers over the
/// existing surface and caches the composited result. So only a glyph HIT skips
/// the seed. A present residual is likewise not treated as full coverage (a
/// residual run can stop before the last pixel), nor are bands.
pub fn needs_seed(data: &[u8], width: u16, height: u16) -> bool {
    let mut r = Reader::new(data);
    let (Some(flags), Some(_seq)) = (r.u8(), r.u8()) else {
        return true;
    };
    if flags & FLAG_GLYPH_INDEX != 0 {
        if r.u16().is_none() {
            return true;
        }
        if flags & FLAG_GLYPH_HIT != 0 {
            return false; // replays a cached tile verbatim; seed unused
        }
    }
    let (Some(residual), Some(bands), Some(sub)) = (r.u32(), r.u32(), r.u32()) else {
        return true;
    };
    // Single whole-tile subcodec region, full-fill codec → every pixel written.
    if residual == 0 && bands == 0 && sub > 0 {
        let (Some(x), Some(y), Some(rw), Some(rh), Some(byte_count), Some(id)) =
            (r.u16(), r.u16(), r.u16(), r.u16(), r.u32(), r.u8())
        else {
            return true;
        };
        let single_region = sub == 13 + byte_count; // 13-byte region header + payload
        let whole_tile = x == 0 && y == 0 && rw == width && rh == height;
        let full_fill = id == SUBCODEC_RAW || id == SUBCODEC_NSCODEC;
        if single_region && whole_tile && full_fill {
            return false;
        }
    }
    true
}

#[inline]
fn put(dst: &mut [u8], w: usize, h: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
    if x < w && y < h {
        let o = (y * w + x) * 4;
        dst[o] = r;
        dst[o + 1] = g;
        dst[o + 2] = b;
        dst[o + 3] = 0xFF;
    }
}

impl ClearDecoder {
    pub fn new() -> Self {
        Self {
            glyphs: vec![Pixels::default(); GLYPH_CACHE],
            vbar: vec![Pixels::default(); VBAR_CACHE],
            short_vbar: vec![Pixels::default(); SHORT_VBAR_CACHE],
            vbar_cursor: 0,
            short_vbar_cursor: 0,
        }
    }

    /// Decode one CLEARCODEC_BITMAP_STREAM into a `width`×`height` RGBA8 buffer
    /// starting from a black background. Returns `None` on a malformed stream
    /// (the caller skips the update).
    pub fn decode(&mut self, data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
        self.decode_seeded(data, width, height, None)
    }

    /// Decode one CLEARCODEC_BITMAP_STREAM, seeding the output with `seed` — the
    /// current `width`×`height` RGBA contents of the destination region.
    ///
    /// ClearCodec is a *persistent-surface* codec (MS-RDPEGFX 3.1.8): a stream
    /// often omits the residual (background) layer and codes only the pixels
    /// that changed in the bands / subcodec layers — every other pixel is meant
    /// to keep the previous frame. FreeRDP decodes straight into the live
    /// surface buffer for exactly this reason. Without a seed, the untouched
    /// pixels would be black, so partial updates (the norm during window motion
    /// and video) paint black bars / garbage over good content. When the stream
    /// *does* carry a full residual it overwrites the seed entirely, so seeding
    /// is a no-op for full-tile paints. `seed` is ignored unless it is exactly
    /// `width*height*4` bytes; `None` (or a size mismatch) falls back to black.
    pub fn decode_seeded(
        &mut self,
        data: &[u8],
        width: u16,
        height: u16,
        seed: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let w = width as usize;
        let h = height as usize;
        if w == 0 || h == 0 {
            return None;
        }
        let mut dst = match seed {
            Some(s) if s.len() == w * h * 4 => s.to_vec(),
            _ => vec![0u8; w * h * 4],
        };
        let mut r = Reader::new(data);

        let glyph_flags = r.u8()?;
        let _seq = r.u8()?;
        let glyph_index = if glyph_flags & FLAG_GLYPH_INDEX != 0 {
            Some(r.u16()? as usize)
        } else {
            None
        };
        if glyph_flags & FLAG_CACHE_RESET != 0 {
            // Reset only zeroes the vBar cursors (cached pixels are left intact).
            self.vbar_cursor = 0;
            self.short_vbar_cursor = 0;
        }

        if let Some(gi) = glyph_index {
            if gi >= GLYPH_CACHE {
                return None;
            }
            if glyph_flags & FLAG_GLYPH_HIT != 0 {
                // Replay a cached glyph: copy w*h pixels straight out.
                let g = &self.glyphs[gi];
                if g.count < w * h {
                    return None;
                }
                dst.copy_from_slice(&g.rgba[..w * h * 4]);
                return Some(dst);
            }
            if w * h > 1024 * 1024 {
                return None;
            }
        }

        let residual_bytes = r.u32()? as usize;
        let bands_bytes = r.u32()? as usize;
        let subcodec_bytes = r.u32()? as usize;

        // Best-effort: a malformed/under-implemented layer must NOT blank the
        // whole frame — each layer fills what it can, last-writer-wins.
        if residual_bytes > 0 {
            if let Some(s) = r.take(residual_bytes) {
                let _ = residual(s, w, h, &mut dst);
            }
        }
        if bands_bytes > 0 {
            if let Some(s) = r.take(bands_bytes) {
                let _ = self.bands(s, w, h, &mut dst);
            }
        }
        let sc = if subcodec_bytes > 0 {
            r.take(subcodec_bytes)
                .map(|s| subcodecs(s, w, h, &mut dst))
                .unwrap_or([0; 3])
        } else {
            [0; 3]
        };

        // Trace the complex paths (bands / NSCodec). At `trace` level: this fires
        // per tile (hundreds per video frame), so it must not run at `debug` —
        // the per-frame aggregate in the decode loop (`target: "perf"`) is the
        // cheap signal; this is for deep per-tile diagnosis only.
        if bands_bytes > 0 || sc[1] > 0 {
            tracing::trace!(
                w,
                h,
                residual_bytes,
                bands_bytes,
                subcodec_bytes,
                raw = sc[0],
                nscodec = sc[1],
                rlex = sc[2],
                "clearcodec complex decode"
            );
        }

        // Store the finished tile into the glyph cache for later GLYPH_HITs.
        if let Some(gi) = glyph_index {
            if glyph_flags & FLAG_GLYPH_HIT == 0 {
                self.glyphs[gi] = Pixels {
                    count: w * h,
                    rgba: dst.clone(),
                };
            }
        }
        Some(dst)
    }

    /// Bands layer: vertical columns with the two-level vBar cache.
    fn bands(&mut self, data: &[u8], w: usize, h: usize, dst: &mut [u8]) -> Option<()> {
        let mut r = Reader::new(data);
        while r.remaining() >= 11 {
            let x_start = r.u16()? as usize;
            let x_end = r.u16()? as usize;
            let y_start = r.u16()? as usize;
            let y_end = r.u16()? as usize;
            let (cb, cg, cr) = (r.u8()?, r.u8()?, r.u8()?);
            if x_end < x_start || y_end < y_start {
                return None;
            }
            let vbar_count = x_end - x_start + 1;
            let vbar_height = y_end - y_start + 1;

            for i in 0..vbar_count {
                let header = r.u16()?;
                let dx = x_start + i;

                if header & 0x8000 != 0 {
                    // Full vBar cache hit — emit straight from the cache slot, no
                    // clone (the cache outlives this call; `dst` is a separate
                    // buffer, so the immutable cache borrow and the dst write
                    // don't conflict). This is the common path once warm.
                    let idx = (header & 0x7FFF) as usize;
                    let entry = self.vbar.get(idx)?;
                    if entry.count == 0 {
                        let bg = background_column(vbar_height, cr, cg, cb);
                        emit_column(dst, w, h, dx, y_start, vbar_height, &bg);
                    } else {
                        emit_column(dst, w, h, dx, y_start, vbar_height, &entry.rgba);
                    }
                    continue;
                }

                // Short vBar (hit or miss) → compose a full column, emit it, cache it.
                let (short_rgba, short_count, y_on): (Vec<u8>, usize, usize);
                if header & 0xC000 == 0x4000 {
                    // Short vBar cache hit; a fresh yOn byte follows.
                    let idx = (header & 0x3FFF) as usize;
                    let e = self.short_vbar.get(idx)?;
                    short_rgba = e.rgba.clone();
                    short_count = e.count;
                    y_on = r.u8()? as usize;
                } else {
                    // Short vBar cache miss: header packs yOn(8) and yOff(6),
                    // then `count` raw B,G,R pixels follow.
                    y_on = (header & 0x00FF) as usize;
                    let y_off = ((header >> 8) & 0x3F) as usize;
                    if y_off < y_on {
                        return None;
                    }
                    short_count = y_off - y_on;
                    let mut sp = vec![0u8; short_count * 4];
                    for k in 0..short_count {
                        let (b, g, rr) = (r.u8()?, r.u8()?, r.u8()?);
                        sp[k * 4] = rr;
                        sp[k * 4 + 1] = g;
                        sp[k * 4 + 2] = b;
                        sp[k * 4 + 3] = 0xFF;
                    }
                    let cur = self.short_vbar_cursor;
                    self.short_vbar_cursor = (cur + 1) % SHORT_VBAR_CACHE;
                    short_rgba = sp.clone();
                    self.short_vbar[cur] = Pixels {
                        count: short_count,
                        rgba: sp,
                    };
                }

                // Compose the full column: bg, then the short pixels.
                let mut v = background_column(vbar_height, cr, cg, cb);
                for k in 0..short_count {
                    let y = y_on + k;
                    if y >= vbar_height {
                        break;
                    }
                    let (s, o) = (k * 4, y * 4);
                    if s + 3 < short_rgba.len() {
                        v[o..o + 4].copy_from_slice(&short_rgba[s..s + 4]);
                    }
                }
                // Emit, then move the composed column into the cache (no clone).
                emit_column(dst, w, h, dx, y_start, vbar_height, &v);
                let cur = self.vbar_cursor;
                self.vbar_cursor = (cur + 1) % VBAR_CACHE;
                self.vbar[cur] = Pixels {
                    count: vbar_height,
                    rgba: v,
                };
            }
        }
        Some(())
    }
}

/// Residual layer: whole-tile RLE of solid colour runs, raster row-major.
fn residual(data: &[u8], w: usize, h: usize, dst: &mut [u8]) -> Option<()> {
    let mut r = Reader::new(data);
    let total = w * h;
    let mut idx = 0usize;
    while r.remaining() >= 4 && idx < total {
        let (b, g, rr) = (r.u8()?, r.u8()?, r.u8()?);
        let first = r.u8()?;
        let run = r.run_length(first)? as usize;
        let run = run.min(total - idx);
        let pixel = [rr, g, b, 0xFF];
        let mut i = 0usize;
        // Write 4 pixels (16 bytes) at a time when aligned.
        while i + 4 <= run {
            let o = (idx + i) * 4;
            dst[o..o + 16].copy_from_slice(&[
                rr, g, b, 0xFF, rr, g, b, 0xFF, rr, g, b, 0xFF, rr, g, b, 0xFF,
            ]);
            i += 4;
        }
        while i < run {
            let o = (idx + i) * 4;
            dst[o..o + 4].copy_from_slice(&pixel);
            i += 1;
        }
        idx += run;
    }
    Some(())
}

/// Subcodec layer: rectangular regions coded RAW / NSCodec / RLEX. Best-effort
/// (stops at the first malformed record). Returns how many regions of each kind
/// were decoded: `[raw, nscodec, rlex]` (for diagnostics).
fn subcodecs(data: &[u8], w: usize, h: usize, dst: &mut [u8]) -> [usize; 3] {
    let mut counts = [0usize; 3];
    let mut r = Reader::new(data);
    while r.remaining() >= 13 {
        let (Some(x_start), Some(y_start), Some(rw), Some(rh)) = (
            r.u16().map(|v| v as usize),
            r.u16().map(|v| v as usize),
            r.u16().map(|v| v as usize),
            r.u16().map(|v| v as usize),
        ) else {
            break;
        };
        let (Some(byte_count), Some(subcodec_id)) = (r.u32().map(|v| v as usize), r.u8()) else {
            break;
        };
        let Some(sub) = r.take(byte_count) else {
            break;
        };
        match subcodec_id {
            SUBCODEC_RAW => {
                raw_region(sub, x_start, y_start, rw, rh, w, h, dst);
                counts[0] += 1;
            }
            SUBCODEC_NSCODEC => {
                let _ = nscodec_region(sub, x_start, y_start, rw, rh, w, h, dst);
                counts[1] += 1;
            }
            SUBCODEC_RLEX => {
                let _ = rlex_region(sub, x_start, y_start, rw, rh, w, h, dst);
                counts[2] += 1;
            }
            _ => {}
        }
    }
    counts
}

/// RAW subcodec: `rw*rh` B,G,R triples, row-major, into the region.
fn raw_region(
    data: &[u8],
    x0: usize,
    y0: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    dst: &mut [u8],
) {
    let mut r = Reader::new(data);
    for y in 0..rh {
        for x in 0..rw {
            let (Some(b), Some(g), Some(rr)) = (r.u8(), r.u8(), r.u8()) else {
                return;
            };
            put(dst, w, h, x0 + x, y0 + y, rr, g, b);
        }
    }
}

/// RLEX subcodec: a small palette plus index runs with a trailing "suite".
fn rlex_region(
    data: &[u8],
    x0: usize,
    y0: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    dst: &mut [u8],
) -> Option<()> {
    let mut r = Reader::new(data);
    let palette_count = r.u8()? as usize;
    if palette_count == 0 || palette_count > 127 {
        return None;
    }
    let mut palette = vec![[0u8; 3]; palette_count]; // stored R,G,B
    for p in palette.iter_mut() {
        let (b, g, rr) = (r.u8()?, r.u8()?, r.u8()?);
        *p = [rr, g, b];
    }
    // Bits needed to hold an index: floor(log2(count-1)) + 1, with count==1 → 1.
    let num_bits: u32 = if palette_count <= 1 {
        1
    } else {
        32 - (palette_count as u32 - 1).leading_zeros()
    };
    let mask = |n: u32| -> u8 { ((1u32 << n) - 1) as u8 };
    let index_mask = mask(num_bits);
    let depth_mask = mask(8 - num_bits);

    let region = rw * rh;
    let mut idx = 0usize;
    let place = |dst: &mut [u8], idx: usize, col: [u8; 3]| {
        put(dst, w, h, x0 + idx % rw, y0 + idx / rw, col[0], col[1], col[2]);
    };

    while r.remaining() >= 2 && idx < region {
        let tmp = r.u8()?;
        let first = r.u8()?;
        let suite_depth = ((tmp >> num_bits) & depth_mask) as usize;
        let stop_index = (tmp & index_mask) as usize;
        let start_index = stop_index.checked_sub(suite_depth)?;
        let run = r.run_length(first)?;
        if start_index >= palette_count || stop_index >= palette_count {
            return None;
        }
        // (a) `run` pixels of the start colour.
        for _ in 0..run {
            if idx >= region {
                break;
            }
            place(dst, idx, palette[start_index]);
            idx += 1;
        }
        // (b) the "suite": one pixel per index from start_index..=stop_index.
        for si in start_index..=stop_index {
            if idx >= region {
                break;
            }
            place(dst, idx, palette[si]);
            idx += 1;
        }
    }
    Some(())
}

/// NSCodec (MS-RDPNSC) subcodec region: three RLE-compressed YCoCg planes
/// (optionally chroma-subsampled), inverse-transformed to RGBA. Used for smooth
/// / photographic / gradient content (window chrome, images, video frames).
/// Output is opaque (the alpha plane isn't applied — the desktop is opaque).
fn nscodec_region(
    data: &[u8],
    x0: usize,
    y0: usize,
    rw: usize,
    rh: usize,
    w: usize,
    h: usize,
    dst: &mut [u8],
) -> Option<()> {
    if rw == 0 || rh == 0 || data.len() < 20 {
        return None;
    }
    let u32at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let luma_bc = u32at(0) as usize;
    let co_bc = u32at(4) as usize;
    let cg_bc = u32at(8) as usize;
    let color_loss = data[16];
    let subsample = data[17] != 0;
    if !(1..=7).contains(&color_loss) {
        return None;
    }
    let shift = (color_loss - 1) as i16;

    // Plane dimensions. Subsampled: luma padded to 8-wide, chroma is half-res.
    let temp_w = (rw + 7) & !7;
    let temp_h = (rh + 1) & !1;
    let (luma_stride, luma_orig) = if subsample {
        (temp_w, temp_w * rh)
    } else {
        (rw, rw * rh)
    };
    let (chroma_stride, chroma_orig) = if subsample {
        (temp_w / 2, (temp_w / 2) * (temp_h / 2))
    } else {
        (rw, rw * rh)
    };

    // The three plane payloads sit back-to-back after the 20-byte header.
    let mut off = 20usize;
    let mut next = |bc: usize| -> &[u8] {
        let end = (off + bc).min(data.len());
        let s = data.get(off..end).unwrap_or(&[]);
        off += bc;
        s
    };
    let luma = nsc_plane(next(luma_bc), luma_orig);
    let co = nsc_plane(next(co_bc), chroma_orig);
    let cg = nsc_plane(next(cg_bc), chroma_orig);

    for y in 0..rh {
        for x in 0..rw {
            let yv = *luma.get(y * luma_stride + x).unwrap_or(&0) as i16;
            let ci = if subsample {
                (y >> 1) * chroma_stride + (x >> 1)
            } else {
                y * rw + x
            };
            let cov = (((*co.get(ci).unwrap_or(&0) as i16) << shift) as i8) as i16;
            let cgv = (((*cg.get(ci).unwrap_or(&0) as i16) << shift) as i8) as i16;
            let r = (yv + cov - cgv).clamp(0, 255) as u8;
            let g = (yv + cgv).clamp(0, 255) as u8;
            let b = (yv - cov - cgv).clamp(0, 255) as u8;
            put(dst, w, h, x0 + x, y0 + y, r, g, b);
        }
    }
    Some(())
}

/// Decode one NSCodec plane payload (`rle`, `planeSize` bytes on the wire) into
/// `orig_size` bytes: empty → all-`0xFF`; already `>= orig_size` → stored raw;
/// otherwise RLE-decompressed.
fn nsc_plane(rle: &[u8], orig_size: usize) -> Vec<u8> {
    if orig_size == 0 {
        return Vec::new();
    }
    if rle.is_empty() {
        return vec![0xFF; orig_size];
    }
    if rle.len() >= orig_size {
        return rle[..orig_size].to_vec();
    }
    nsc_rle(rle, orig_size)
}

/// NSCodec RLE: a run is two equal bytes followed by a count (`< 0xFF` → +2
/// bias; `0xFF` → an exact 32-bit length follows). The final 4 bytes of every
/// plane are stored literally. Robust to truncated/garbage input.
fn nsc_rle(input: &[u8], orig_size: usize) -> Vec<u8> {
    let mut out = vec![0u8; orig_size];
    if orig_size <= 4 {
        for k in 0..orig_size {
            out[k] = *input.get(k).unwrap_or(&0);
        }
        return out;
    }
    let mut i = 0usize;
    let mut pos = 0usize;
    while pos + 4 < orig_size {
        let Some(&value) = input.get(i) else { break };
        i += 1;
        if pos + 5 == orig_size {
            out[pos] = value;
            pos += 1;
        } else if input.get(i) == Some(&value) {
            i += 1; // consume the duplicate byte
            let Some(&n) = input.get(i) else { break };
            i += 1;
            let len = if n < 0xFF {
                n as usize + 2
            } else {
                let Some(b) = input.get(i..i + 4) else { break };
                i += 4;
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
            };
            let end = (pos + len).min(orig_size);
            for p in &mut out[pos..end] {
                *p = value;
            }
            pos = end;
        } else {
            out[pos] = value;
            pos += 1;
        }
    }
    // Tail: the last 4 bytes are stored literally.
    for k in 0..4 {
        out[orig_size - 4 + k] = *input.get(i + k).unwrap_or(&0);
    }
    out
}

/// Emit a composed vBar `column` (RGBA pixels) down column `dx` of the
/// destination, starting at row `y_start`, for up to `vbar_height` rows.
fn emit_column(
    dst: &mut [u8],
    w: usize,
    h: usize,
    dx: usize,
    y_start: usize,
    vbar_height: usize,
    column: &[u8],
) {
    if dx >= w {
        return;
    }
    // `y_start`/`vbar_height` come straight off the wire; clamp to the tile so
    // a malformed band cannot write past `dst` (which would panic the decode
    // thread and kill the session).
    let rows = vbar_height
        .min(column.len() / 4)
        .min(h.saturating_sub(y_start));
    for y in 0..rows {
        let s = y * 4;
        let d = ((y_start + y) * w + dx) * 4;
        dst[d..d + 4].copy_from_slice(&column[s..s + 4]);
    }
}

/// A `count`-pixel RGBA column filled with the band background colour.
fn background_column(count: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut v = vec![0u8; count * 4];
    let pixel = [r, g, b, 0xFF];
    let pattern = u8x16::new([
        r, g, b, 0xFF, r, g, b, 0xFF, r, g, b, 0xFF, r, g, b, 0xFF,
    ]);
    let pattern = pattern.to_array();
    let mut chunks = v.chunks_exact_mut(16);
    for chunk in &mut chunks {
        chunk.copy_from_slice(&pattern);
    }
    for px in chunks.into_remainder().chunks_exact_mut(4) {
        px.copy_from_slice(&pixel);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wire-controlled band whose `yStart + height` exceeds the tile must be
    /// clipped, not panic (the band fields are attacker-controlled input).
    #[test]
    fn emit_column_clips_overflowing_band_to_tile() {
        let (w, h) = (4usize, 4usize);
        let mut dst = vec![0u8; w * h * 4];
        let column = vec![0xAAu8; 8 * 4]; // 8 rows offered
        // yStart 2 in a 4-row tile → only rows 2..4 may be written.
        emit_column(&mut dst, w, h, 1, 2, 8, &column);
        assert_eq!(&dst[(2 * w + 1) * 4..(2 * w + 1) * 4 + 4], &[0xAA; 4]);
        assert_eq!(&dst[(3 * w + 1) * 4..(3 * w + 1) * 4 + 4], &[0xAA; 4]);
        // yStart beyond the tile writes nothing at all.
        emit_column(&mut dst, w, h, 0, 4, 8, &column);
        assert!(dst[(3 * w) * 4..(3 * w) * 4 + 4].iter().all(|&b| b == 0));
    }

    /// Build a CLEARCODEC_BITMAP_STREAM with the given layer payloads.
    fn stream(residual: &[u8], bands: &[u8], subcodec: &[u8]) -> Vec<u8> {
        let mut s = vec![0u8, 0u8]; // glyphFlags=0, seqNumber=0
        s.extend_from_slice(&(residual.len() as u32).to_le_bytes());
        s.extend_from_slice(&(bands.len() as u32).to_le_bytes());
        s.extend_from_slice(&(subcodec.len() as u32).to_le_bytes());
        s.extend_from_slice(residual);
        s.extend_from_slice(bands);
        s.extend_from_slice(subcodec);
        s
    }

    #[test]
    fn residual_solid_fill() {
        // One run of 4 red pixels (B=0,G=0,R=255, run=4) over a 2x2 tile.
        let res = [0x00, 0x00, 0xFF, 0x04];
        let out = ClearDecoder::new().decode(&stream(&res, &[], &[]), 2, 2).unwrap();
        assert_eq!(out.len(), 16);
        for px in out.chunks_exact(4) {
            assert_eq!(px, [0xFF, 0x00, 0x00, 0xFF]); // RGBA red
        }
    }

    #[test]
    fn residual_run_escalation_and_partial() {
        // Blue (B=255) run of 3 then green (G=255) run of 1 over a 2x2 tile.
        let res = [0xFF, 0x00, 0x00, 0x03, 0x00, 0xFF, 0x00, 0x01];
        let out = ClearDecoder::new().decode(&stream(&res, &[], &[]), 2, 2).unwrap();
        assert_eq!(&out[0..4], [0x00, 0x00, 0xFF, 0xFF]); // blue
        assert_eq!(&out[12..16], [0x00, 0xFF, 0x00, 0xFF]); // last pixel green
    }

    #[test]
    fn raw_subcodec_region() {
        // 1x1 RAW region at (0,0): B,G,R = 10,20,30.
        let mut sub = Vec::new();
        sub.extend_from_slice(&0u16.to_le_bytes()); // xStart
        sub.extend_from_slice(&0u16.to_le_bytes()); // yStart
        sub.extend_from_slice(&1u16.to_le_bytes()); // width
        sub.extend_from_slice(&1u16.to_le_bytes()); // height
        sub.extend_from_slice(&3u32.to_le_bytes()); // byteCount
        sub.push(SUBCODEC_RAW);
        sub.extend_from_slice(&[10, 20, 30]); // B,G,R
        let out = ClearDecoder::new().decode(&stream(&[], &[], &sub), 1, 1).unwrap();
        assert_eq!(out, vec![30, 20, 10, 0xFF]); // RGBA
    }

    #[test]
    fn rlex_two_colour_palette() {
        // 2x1 region, palette [red, blue]; one segment: run=1 of start, suite to stop.
        // paletteCount=2 → num_bits=1. tmp: stopIndex(bit0)=1, suiteDepth(bits1..)=1
        // → start_index = 1-1 = 0. run byte = 0 (no leading run), so the output is
        // just the suite palette[0..=1] = red, blue over the 2-pixel region.
        let mut sub = Vec::new();
        sub.extend_from_slice(&0u16.to_le_bytes()); // xStart
        sub.extend_from_slice(&0u16.to_le_bytes()); // yStart
        sub.extend_from_slice(&2u16.to_le_bytes()); // width
        sub.extend_from_slice(&1u16.to_le_bytes()); // height
        let mut payload = Vec::new();
        payload.push(2); // paletteCount
        payload.extend_from_slice(&[0x00, 0x00, 0xFF]); // palette[0] = red (B,G,R)
        payload.extend_from_slice(&[0xFF, 0x00, 0x00]); // palette[1] = blue
        payload.push(0b11); // tmp: stopIndex=1, suiteDepth=1 → startIndex=0
        payload.push(0); // runLength = 0 (suite only)
        sub.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        sub.push(SUBCODEC_RLEX);
        sub.extend_from_slice(&payload);
        let out = ClearDecoder::new().decode(&stream(&[], &[], &sub), 2, 1).unwrap();
        // pixel0 = run(start=red), pixel1 = suite[1]=blue.
        assert_eq!(&out[0..4], [0xFF, 0x00, 0x00, 0xFF]); // red
        assert_eq!(&out[4..8], [0x00, 0x00, 0xFF, 0xFF]); // blue
    }

    #[test]
    fn glyph_store_then_hit() {
        let res = [0x00, 0x00, 0xFF, 0x01]; // 1 red pixel
        let mut s = vec![FLAG_GLYPH_INDEX, 0]; // glyphFlags=GLYPH_INDEX, seq=0
        s.extend_from_slice(&7u16.to_le_bytes()); // glyphIndex=7
        s.extend_from_slice(&(res.len() as u32).to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&res);
        let mut dec = ClearDecoder::new();
        let first = dec.decode(&s, 1, 1).unwrap();
        assert_eq!(first, vec![0xFF, 0, 0, 0xFF]);
        // Now a GLYPH_HIT for index 7 (no composite payload).
        let hit = vec![FLAG_GLYPH_INDEX | FLAG_GLYPH_HIT, 0, 7, 0];
        let out = dec.decode(&hit, 1, 1).unwrap();
        assert_eq!(out, vec![0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn nscodec_raw_pixel_ycocg() {
        // 1x1, no subsampling, raw planes: Y=128, Co=100, Cg=0 →
        // r=Y+Co-Cg=228, g=Y+Cg=128, b=Y-Co-Cg=28.
        let mut nsc = Vec::new();
        nsc.extend_from_slice(&1u32.to_le_bytes()); // luma byte count
        nsc.extend_from_slice(&1u32.to_le_bytes()); // Co
        nsc.extend_from_slice(&1u32.to_le_bytes()); // Cg
        nsc.extend_from_slice(&0u32.to_le_bytes()); // alpha
        nsc.push(1); // colorLossLevel (shift 0)
        nsc.push(0); // chromaSubsampling off
        nsc.extend_from_slice(&[0, 0]); // reserved
        nsc.extend_from_slice(&[128, 100, 0]); // Y, Co, Cg planes (raw, 1 byte each)
        let mut dst = vec![0u8; 4];
        nscodec_region(&nsc, 0, 0, 1, 1, 1, 1, &mut dst).unwrap();
        assert_eq!(dst, vec![228, 128, 28, 255]);
    }

    #[test]
    fn seed_preserved_when_no_layers() {
        // An empty composition (no residual / bands / subcodec layers) must
        // return the seed untouched — the persistent-surface contract a partial
        // ClearCodec update relies on. Before the seed fix this came back black.
        let seed = vec![1, 2, 3, 0xFF, 4, 5, 6, 0xFF, 7, 8, 9, 0xFF, 10, 11, 12, 0xFF];
        let out = ClearDecoder::new()
            .decode_seeded(&stream(&[], &[], &[]), 2, 2, Some(&seed))
            .unwrap();
        assert_eq!(out, seed);
    }

    #[test]
    fn seed_preserved_outside_subcodec_region() {
        // A 1x1 RAW region at (1,0) in a 2x1 tile only touches pixel 1; pixel 0
        // must keep the seed instead of going black.
        let mut sub = Vec::new();
        sub.extend_from_slice(&1u16.to_le_bytes()); // xStart = 1
        sub.extend_from_slice(&0u16.to_le_bytes()); // yStart
        sub.extend_from_slice(&1u16.to_le_bytes()); // width
        sub.extend_from_slice(&1u16.to_le_bytes()); // height
        sub.extend_from_slice(&3u32.to_le_bytes()); // byteCount
        sub.push(SUBCODEC_RAW);
        sub.extend_from_slice(&[10, 20, 30]); // B,G,R at (1,0)
        let seed = vec![100, 101, 102, 0xFF, 0, 0, 0, 0];
        let out = ClearDecoder::new()
            .decode_seeded(&stream(&[], &[], &sub), 2, 1, Some(&seed))
            .unwrap();
        assert_eq!(&out[0..4], &[100, 101, 102, 0xFF]); // pixel 0 retained from seed
        assert_eq!(&out[4..8], &[30, 20, 10, 0xFF]); // pixel 1 = RAW (R,G,B,A)
    }

    #[test]
    fn nsc_rle_runs_and_literals() {
        // value 5 repeated: [5,5,count=3 → len 5] then literal tail of 4 bytes.
        let out = nsc_rle(&[5, 5, 3, 9, 8, 7, 6], 9);
        assert_eq!(out, vec![5, 5, 5, 5, 5, 9, 8, 7, 6]);
    }

    #[test]
    fn bands_short_vbar_miss_emits_pixels() {
        // One 1-wide, 2-tall band; a short vBar cache MISS (yOn=0, yOff=2) with
        // two raw pixels. Exercises the refactored compose→emit→cache path.
        let mut bands = Vec::new();
        bands.extend_from_slice(&0u16.to_le_bytes()); // x_start
        bands.extend_from_slice(&0u16.to_le_bytes()); // x_end
        bands.extend_from_slice(&0u16.to_le_bytes()); // y_start
        bands.extend_from_slice(&1u16.to_le_bytes()); // y_end → height 2
        bands.extend_from_slice(&[0, 0, 0]); // bg B,G,R = black
        bands.extend_from_slice(&0x0200u16.to_le_bytes()); // vBarHeader: yOn=0, yOff=2
        bands.extend_from_slice(&[0, 0, 255]); // pixel0 B,G,R = red
        bands.extend_from_slice(&[0, 255, 0]); // pixel1 B,G,R = green
        let out = ClearDecoder::new()
            .decode(&stream(&[], &bands, &[]), 1, 2)
            .unwrap();
        assert_eq!(&out[0..4], &[255, 0, 0, 0xFF]); // red
        assert_eq!(&out[4..8], &[0, 255, 0, 0xFF]); // green
    }

    #[test]
    fn needs_seed_glyph_hit_is_false() {
        let hit = vec![FLAG_GLYPH_INDEX | FLAG_GLYPH_HIT, 0, 7, 0];
        assert!(!needs_seed(&hit, 1, 1));
    }

    #[test]
    fn needs_seed_full_tile_nscodec_region_is_false() {
        // One NSCodec region at (0,0) covering the whole 2x2 tile, no residual/bands.
        let mut sub = Vec::new();
        sub.extend_from_slice(&0u16.to_le_bytes()); // x
        sub.extend_from_slice(&0u16.to_le_bytes()); // y
        sub.extend_from_slice(&2u16.to_le_bytes()); // w
        sub.extend_from_slice(&2u16.to_le_bytes()); // h
        sub.extend_from_slice(&4u32.to_le_bytes()); // byteCount
        sub.push(SUBCODEC_NSCODEC);
        sub.extend_from_slice(&[0, 0, 0, 0]); // payload
        assert!(!needs_seed(&stream(&[], &[], &sub), 2, 2));
    }

    #[test]
    fn needs_seed_residual_and_partial_region_are_true() {
        // A residual may be a partial run → seed needed.
        assert!(needs_seed(&stream(&[0x00, 0x00, 0xFF, 0x04], &[], &[]), 2, 2));
        // A RAW region not covering the whole tile → seed needed.
        let mut sub = Vec::new();
        sub.extend_from_slice(&1u16.to_le_bytes()); // x = 1 (not whole tile)
        sub.extend_from_slice(&0u16.to_le_bytes());
        sub.extend_from_slice(&1u16.to_le_bytes());
        sub.extend_from_slice(&1u16.to_le_bytes());
        sub.extend_from_slice(&3u32.to_le_bytes());
        sub.push(SUBCODEC_RAW);
        sub.extend_from_slice(&[10, 20, 30]);
        assert!(needs_seed(&stream(&[], &[], &sub), 2, 1));
    }

    #[test]
    fn garbage_never_panics() {
        let mut dec = ClearDecoder::new();
        for seed in 0u16..1500 {
            let b = seed.to_le_bytes();
            let junk = [b[0], b[1], b[1] ^ 0x5a, b[0].wrapping_add(9), 0xFF, 0x00, b[0]];
            let _ = dec.decode(&junk, 4, 4);
        }
    }
}
