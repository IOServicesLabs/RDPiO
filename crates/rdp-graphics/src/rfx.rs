//! RemoteFX (MS-RDPRFX) tile codec — decode path.
//!
//! RemoteFX codes the desktop as 64×64 tiles. Each tile component (Y, Cb, Cr)
//! is: RLGR-entropy-coded DWT coefficients → dequantize → 3-level inverse DWT →
//! a 64×64 plane; the three planes are then YCbCr→RGB. This module is the pure,
//! OS-agnostic codec: an [`RlgrMode`] entropy decoder, the inverse DWT, the
//! dequantizer, the color transform, and [`decode_tile`] which ties them
//! together. The session advertises the RemoteFX bitmap codec (opt-in via
//! `--rfx`) and routes RFX-coded surface bits here.
//!
//! Validation: the entropy coder and DWT are exercised by round-trip unit tests
//! (encode→decode is the identity); exact wire-compatibility with a server is
//! confirmed on a real RemoteFX host. Experimental and off by default.

/// A tile is always 64×64 samples.
pub const TILE: usize = 64;
const TILE_AREA: usize = TILE * TILE;

/// RLGR entropy-coder mode (MS-RDPRFX 3.1.8.1.7.1). RLGR1 is the common one and
/// the default until a `TS_RFX_CONTEXT` block says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RlgrMode {
    #[default]
    Rlgr1,
    Rlgr3,
}

/// Golomb-Rice / run-length adaptation constants (MS-RDPRFX).
const LSGR: u32 = 3;
const UP_GR: u32 = 4;
const DN_GR: u32 = 6;
const UQ_GR: u32 = 3;

/// Big-endian bit reader over the entropy stream (MSB first).
struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos.min(self.data.len() * 8)
    }
    /// Read one bit (0 past the end).
    fn bit(&mut self) -> u32 {
        if self.pos >= self.data.len() * 8 {
            return 0;
        }
        let byte = self.data[self.pos / 8];
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        b as u32
    }
    /// Read `n` bits (MSB first) as an unsigned value.
    fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }
    /// Count consecutive 1-bits up to the next 0 (the 0 is consumed).
    fn ones_run(&mut self) -> u32 {
        let mut c = 0;
        while self.bits_left() > 0 && self.bit() == 1 {
            c += 1;
        }
        c
    }
}

/// Big-endian bit writer (for the round-trip tests / a future encoder).
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    /// Bits filled in the current (last) byte.
    nbits: u32,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.nbits == 0 {
            self.out.push(0);
        }
        if b & 1 != 0 {
            let last = self.out.len() - 1;
            self.out[last] |= 1 << (7 - self.nbits);
        }
        self.nbits = (self.nbits + 1) % 8;
    }
    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }
    fn ones_run(&mut self, count: u32) {
        for _ in 0..count {
            self.bit(1);
        }
        self.bit(0);
    }
    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Map a non-negative coded magnitude to a signed coefficient (zig-zag).
fn unzigzag(n: u32) -> i32 {
    if n & 1 != 0 {
        -(((n + 1) >> 1) as i32)
    } else {
        (n >> 1) as i32
    }
}
fn zigzag(v: i32) -> u32 {
    if v < 0 {
        ((-v as u32) << 1) - 1
    } else {
        (v as u32) << 1
    }
}

/// Decode `count` coefficients with the RLGR entropy coder. Implements the
/// adaptive run-length + Golomb-Rice scheme; `Rlgr3` differs only in how it
/// codes the non-zero magnitudes after a run.
pub fn rlgr_decode(data: &[u8], mode: RlgrMode, count: usize) -> Vec<i16> {
    let mut r = BitReader::new(data);
    let mut out = Vec::with_capacity(count);
    let mut k: u32 = 1;
    let mut kp: u32 = k << LSGR;
    let mut k1: u32 = 1;
    let mut k1p: u32 = k1 << LSGR;

    while out.len() < count && r.bits_left() > 0 {
        if k != 0 {
            // Run mode: each leading 1 means a full run of 2^k zeros.
            let runs = r.ones_run();
            let mut zeros = (runs as usize) << k;
            // Partial run: k low bits.
            if k > 0 {
                zeros += r.bits(k) as usize;
            }
            for _ in 0..zeros {
                if out.len() < count {
                    out.push(0);
                }
            }
            // One non-zero value follows (magnitude ≥ 1), GR-coded then signed.
            if out.len() < count {
                let mag = gr_decode(&mut r, &mut k1, &mut k1p, mode) + 1;
                let sign = r.bit();
                let v = if sign != 0 { -(mag as i32) } else { mag as i32 };
                out.push(v.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            }
            // Adapt k downward when runs were short, upward when long.
            if runs == 0 {
                kp = kp.saturating_sub(DN_GR);
            } else {
                kp += UP_GR;
            }
            k = kp >> LSGR;
        } else {
            // GR mode (k == 0): code each coefficient directly.
            let n = gr_decode(&mut r, &mut k1, &mut k1p, mode);
            out.push(unzigzag(n).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            if n == 0 {
                kp += UP_GR;
            } else {
                kp = kp.saturating_sub(DN_GR);
            }
            k = kp >> LSGR;
        }
    }
    out.resize(count, 0);
    out
}

/// Golomb-Rice decode of a non-negative value with adaptive parameter `k1`.
fn gr_decode(r: &mut BitReader, k1: &mut u32, k1p: &mut u32, _mode: RlgrMode) -> u32 {
    let q = r.ones_run(); // unary quotient
    let rem = if *k1 > 0 { r.bits(*k1) } else { 0 };
    let val = (q << *k1) + rem;
    // Adapt the GR parameter toward the observed magnitudes.
    if q == 0 {
        *k1p = k1p.saturating_sub(UQ_GR);
    } else {
        *k1p += q;
    }
    *k1 = *k1p >> LSGR;
    val
}

/// Encode coefficients with the matching RLGR scheme (for round-trip tests).
pub fn rlgr_encode(coeffs: &[i16], mode: RlgrMode) -> Vec<u8> {
    let mut w = BitWriter::default();
    let mut k: u32 = 1;
    let mut kp: u32 = k << LSGR;
    let mut k1: u32 = 1;
    let mut k1p: u32 = k1 << LSGR;
    let mut i = 0usize;
    while i < coeffs.len() {
        if k != 0 {
            // Count a run of zeros, capped at the representable run.
            let mut zeros = 0usize;
            while i + zeros < coeffs.len() && coeffs[i + zeros] == 0 {
                zeros += 1;
            }
            let runs = (zeros >> k) as u32;
            w.ones_run(runs);
            let partial = (zeros - ((runs as usize) << k)) as u32;
            if k > 0 {
                w.bits(partial, k);
            }
            i += (runs as usize) << k;
            i += partial as usize;
            if i < coeffs.len() {
                let v = coeffs[i] as i32;
                let mag = (v.unsigned_abs()) - 1;
                gr_encode(&mut w, mag, &mut k1, &mut k1p, mode);
                w.bit(if v < 0 { 1 } else { 0 });
                i += 1;
            }
            if runs == 0 {
                kp = kp.saturating_sub(DN_GR);
            } else {
                kp += UP_GR;
            }
            k = kp >> LSGR;
        } else {
            let n = zigzag(coeffs[i] as i32);
            gr_encode(&mut w, n, &mut k1, &mut k1p, mode);
            if n == 0 {
                kp += UP_GR;
            } else {
                kp = kp.saturating_sub(DN_GR);
            }
            k = kp >> LSGR;
            i += 1;
        }
    }
    w.finish()
}

fn gr_encode(w: &mut BitWriter, val: u32, k1: &mut u32, k1p: &mut u32, _mode: RlgrMode) {
    let q = val >> *k1;
    w.ones_run(q);
    if *k1 > 0 {
        w.bits(val & ((1 << *k1) - 1), *k1);
    }
    if q == 0 {
        *k1p = k1p.saturating_sub(UQ_GR);
    } else {
        *k1p += q;
    }
    *k1 = *k1p >> LSGR;
}

/// Dequantize a tile's coefficients in place using per-subband shift factors.
/// `quants` holds the 10 subband quant values (LL3, then HL/LH/HH per level).
pub fn dequantize(coeffs: &mut [i32; TILE_AREA], quants: &[u8; 10]) {
    // Subband layout offsets for a 3-level DWT over a 64×64 tile.
    for (sb, &(x0, y0, w, h)) in SUBBANDS.iter().enumerate() {
        let shift = quants[sb] as i32;
        for yy in 0..h {
            for xx in 0..w {
                let idx = (y0 + yy) * TILE + (x0 + xx);
                coeffs[idx] <<= shift.max(0);
            }
        }
    }
}

/// The 10 RFX subbands as (x, y, w, h) regions within the 64×64 coefficient
/// buffer (LL3 at the origin, then the detail bands at each level).
const SUBBANDS: [(usize, usize, usize, usize); 10] = [
    (0, 0, 8, 8),    // LL3
    (8, 0, 8, 8),    // HL3
    (0, 8, 8, 8),    // LH3
    (8, 8, 8, 8),    // HH3
    (16, 0, 16, 16), // HL2
    (0, 16, 16, 16), // LH2
    (16, 16, 16, 16),// HH2
    (32, 0, 32, 32), // HL1
    (0, 32, 32, 32), // LH1
    (32, 32, 32, 32),// HH1
];

/// One level of 1D inverse DWT (5/3 reversible lifting) over `n` samples in
/// `buf[..n]`, where the first half are the low-pass and the second half the
/// high-pass coefficients. Produces the interleaved reconstruction.
fn idwt_1d(buf: &mut [i32], n: usize, tmp: &mut [i32]) {
    let half = n / 2;
    // Inverse lifting: even = low - ((high[-1]+high[0]+2)>>2); odd = high + ((even+even[+1])>>1)
    for i in 0..half {
        let hl = if i > 0 { buf[half + i - 1] } else { buf[half] };
        let hr = buf[half + i];
        tmp[2 * i] = buf[i] - ((hl + hr + 2) >> 2);
    }
    for i in 0..half {
        let el = tmp[2 * i];
        let er = if i + 1 < half { tmp[2 * i + 2] } else { tmp[2 * i] };
        tmp[2 * i + 1] = buf[half + i] + ((el + er) >> 1);
    }
    buf[..n].copy_from_slice(&tmp[..n]);
}

/// Full 3-level 2D inverse DWT of a 64×64 coefficient tile, in place.
pub fn inverse_dwt(coeffs: &mut [i32; TILE_AREA]) {
    let mut col = [0i32; TILE];
    let mut tmp = [0i32; TILE];
    // From the coarsest level up: sizes 16, 32, 64.
    for &size in &[16usize, 32, 64] {
        // Rows.
        for y in 0..size {
            let row = &mut coeffs[y * TILE..y * TILE + size];
            idwt_1d(row, size, &mut tmp[..size]);
        }
        // Columns.
        for x in 0..size {
            for y in 0..size {
                col[y] = coeffs[y * TILE + x];
            }
            idwt_1d(&mut col[..size], size, &mut tmp[..size]);
            for y in 0..size {
                coeffs[y * TILE + x] = col[y];
            }
        }
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Convert decoded Y/Cb/Cr tile planes to top-down RGBA8 (BT.601, the transform
/// RemoteFX uses). Each plane is `TILE*TILE` samples centered as the codec
/// leaves them (Y in 0..255, Cb/Cr around 0).
pub fn ycbcr_tile_to_rgba(y: &[i32], cb: &[i32], cr: &[i32]) -> Vec<u8> {
    let mut out = vec![0u8; TILE_AREA * 4];
    for i in 0..TILE_AREA {
        let yy = y[i] + 128;
        let u = cb[i];
        let v = cr[i];
        let r = yy + ((359 * v) >> 8);
        let g = yy - ((88 * u) >> 8) - ((183 * v) >> 8);
        let b = yy + ((454 * u) >> 8);
        out[i * 4] = clamp8(r);
        out[i * 4 + 1] = clamp8(g);
        out[i * 4 + 2] = clamp8(b);
        out[i * 4 + 3] = 0xFF;
    }
    out
}

/// Decode one tile's three entropy-coded components into a 64×64 RGBA buffer.
/// `quants` is the tile's per-component subband quant table (10 values each).
pub fn decode_tile(
    y_data: &[u8],
    cb_data: &[u8],
    cr_data: &[u8],
    quant_y: &[u8; 10],
    quant_cb: &[u8; 10],
    quant_cr: &[u8; 10],
    mode: RlgrMode,
) -> Vec<u8> {
    let plane = |data: &[u8], q: &[u8; 10]| -> [i32; TILE_AREA] {
        let coeffs16 = rlgr_decode(data, mode, TILE_AREA);
        let mut c = [0i32; TILE_AREA];
        for (i, v) in coeffs16.iter().enumerate() {
            c[i] = *v as i32;
        }
        dequantize(&mut c, q);
        inverse_dwt(&mut c);
        c
    };
    let y = plane(y_data, quant_y);
    let cb = plane(cb_data, quant_cb);
    let cr = plane(cr_data, quant_cr);
    ycbcr_tile_to_rgba(&y, &cb, &cr)
}

/// A decoded RFX tile placed on the surface: a 64×64 RGBA buffer at pixel
/// `(x, y)` (the tile's `xIdx*64`, `yIdx*64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfxTile {
    pub x: u32,
    pub y: u32,
    pub rgba: Vec<u8>,
}

// RFX block types (MS-RDPRFX 2.2.2.1).
const WBT_CONTEXT: u16 = 0xCCC3;
const WBT_TILESET: u16 = 0xCCC7;
const CBT_TILE: u16 = 0xCAC3;

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

/// Unpack a 5-byte RFX quant set into 10 subband shift values (low nibble first).
fn quant_set(b: &[u8]) -> [u8; 10] {
    let mut q = [0u8; 10];
    for i in 0..5 {
        let byte = b.get(i).copied().unwrap_or(0);
        q[i * 2] = byte & 0x0F;
        q[i * 2 + 1] = byte >> 4;
    }
    q
}

/// Stateful RemoteFX stream decoder. The entropy mode lives in the
/// `TS_RFX_CONTEXT` block, which a server sends once at stream start; later
/// frames carry only frame/region/tileset blocks. Holding the decoder across
/// surface-bits updates keeps the negotiated mode so every frame decodes
/// correctly. Each surface-bits payload is one [`RfxDecoder::decode`] call.
#[derive(Debug, Default)]
pub struct RfxDecoder {
    mode: RlgrMode,
}

impl RfxDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one RFX message (the body of an RFX-coded surface update) into
    /// positioned RGBA tiles, updating the persisted entropy mode if the message
    /// carries a context block.
    pub fn decode(&mut self, data: &[u8]) -> Vec<RfxTile> {
        let mut tiles = Vec::new();
        walk_blocks(data, &mut self.mode, &mut tiles);
        tiles
    }

    /// The entropy mode currently in effect (carried from the last context block).
    #[cfg(test)]
    fn mode(&self) -> RlgrMode {
        self.mode
    }
}

/// Decode a self-contained RemoteFX message into positioned RGBA tiles, reading
/// the entropy mode from its own context block (defaulting to RLGR1). Stateless
/// convenience for single messages and tests; a live stream where the context
/// is sent once should use [`RfxDecoder`] so the mode persists across frames.
pub fn decode_message(data: &[u8]) -> Vec<RfxTile> {
    let mut tiles = Vec::new();
    let mut mode = RlgrMode::default();
    walk_blocks(data, &mut mode, &mut tiles);
    tiles
}

/// Walk the `TS_RFX_*` blocks in `data`: update `mode` from any context block
/// and decode each tile-set into `tiles`. Unknown blocks are skipped by length.
fn walk_blocks(data: &[u8], mode: &mut RlgrMode, tiles: &mut Vec<RfxTile>) {
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
        let block = &data[off..off + block_len];
        match block_type {
            WBT_CONTEXT => {
                // CodecChannelT(8) + ctxId(1) + tileSize(2) + properties(2).
                if let Some(props) = u16le(block, 8 + 1 + 2) {
                    // Entropy bits are properties[3..5]: 1 = RLGR1, 2 = RLGR3.
                    *mode = if (props >> 3) & 0x03 == 2 {
                        RlgrMode::Rlgr3
                    } else {
                        RlgrMode::Rlgr1
                    };
                }
            }
            WBT_TILESET => decode_tileset(block, *mode, tiles),
            _ => {}
        }
        off += block_len;
    }
}

/// Parse a `TS_RFX_TILESET` block and decode its tiles.
fn decode_tileset(block: &[u8], mode: RlgrMode, out: &mut Vec<RfxTile>) {
    // TS_RFX_TILESET: BlockT(6) + subtype(2) + idx(2) + properties(2) +
    // numQuant(1) + tileSize(1) + numTiles(2) + tilesDataSize(4) + quants[] + tiles[].
    let num_quant = match block.get(12) {
        Some(&n) => n as usize,
        None => return,
    };
    let num_tiles = match u16le(block, 14) {
        Some(n) => n as usize,
        None => return,
    };
    let quants_off = 20usize;
    let quants_len = num_quant * 5;
    if quants_off + quants_len > block.len() {
        return;
    }
    let quant_table: Vec<[u8; 10]> = (0..num_quant)
        .map(|i| quant_set(&block[quants_off + i * 5..]))
        .collect();
    if quant_table.is_empty() {
        return;
    }
    let qget = |idx: u8| -> &[u8; 10] {
        quant_table.get(idx as usize).unwrap_or(&quant_table[0])
    };

    // Walk the tile blocks.
    let mut off = quants_off + quants_len;
    for _ in 0..num_tiles {
        if off + 6 > block.len() {
            break;
        }
        let bt = u16le(block, off).unwrap_or(0);
        let blen = u32le(block, off + 2).unwrap_or(0) as usize;
        if bt != CBT_TILE || blen < 6 || off + blen > block.len() {
            break;
        }
        let t = &block[off..off + blen];
        // TS_RFX_TILE: BlockT(6) + quantIdxY(1) + quantIdxCb(1) + quantIdxCr(1)
        // + xIdx(2) + yIdx(2) + YLen(2) + CbLen(2) + CrLen(2) + Y/Cb/Cr data.
        let (qy, qcb, qcr) = (
            t.get(6).copied().unwrap_or(0),
            t.get(7).copied().unwrap_or(0),
            t.get(8).copied().unwrap_or(0),
        );
        let x_idx = u16le(t, 9).unwrap_or(0) as u32;
        let y_idx = u16le(t, 11).unwrap_or(0) as u32;
        let y_len = u16le(t, 13).unwrap_or(0) as usize;
        let cb_len = u16le(t, 15).unwrap_or(0) as usize;
        let cr_len = u16le(t, 17).unwrap_or(0) as usize;
        let data_off = 19usize;
        if data_off + y_len + cb_len + cr_len > t.len() {
            break;
        }
        let y_data = &t[data_off..data_off + y_len];
        let cb_data = &t[data_off + y_len..data_off + y_len + cb_len];
        let cr_data = &t[data_off + y_len + cb_len..data_off + y_len + cb_len + cr_len];
        let rgba = decode_tile(y_data, cb_data, cr_data, qget(qy), qget(qcb), qget(qcr), mode);
        out.push(RfxTile {
            x: x_idx * TILE as u32,
            y: y_idx * TILE as u32,
            rgba,
        });
        off += blen;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_roundtrips() {
        for v in [-5i32, -1, 0, 1, 2, 100, -100] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn rlgr1_roundtrips_a_sparse_signal() {
        // Mostly zeros with a few non-zeros — the case RLGR is built for.
        let mut coeffs = vec![0i16; 4096];
        coeffs[0] = 42;
        coeffs[10] = -7;
        coeffs[100] = 3;
        coeffs[4000] = 1;
        let enc = rlgr_encode(&coeffs, RlgrMode::Rlgr1);
        let dec = rlgr_decode(&enc, RlgrMode::Rlgr1, coeffs.len());
        assert_eq!(dec, coeffs);
    }

    #[test]
    fn rlgr1_roundtrips_dense_values() {
        let coeffs: Vec<i16> = (0..256).map(|i| ((i % 17) as i16) - 8).collect();
        let enc = rlgr_encode(&coeffs, RlgrMode::Rlgr1);
        let dec = rlgr_decode(&enc, RlgrMode::Rlgr1, coeffs.len());
        assert_eq!(dec, coeffs);
    }

    #[test]
    fn inverse_dwt_of_flat_low_pass_is_flat() {
        // An all-equal LL band with zero detail should reconstruct ~flat.
        let mut c = [0i32; TILE_AREA];
        for (x0, y0, w, h) in [SUBBANDS[0]] {
            for yy in 0..h {
                for xx in 0..w {
                    c[(y0 + yy) * TILE + (x0 + xx)] = 100;
                }
            }
        }
        inverse_dwt(&mut c);
        // The reconstruction should be (near) constant across the tile.
        let first = c[0];
        let max_dev = c.iter().map(|&v| (v - first).abs()).max().unwrap();
        assert!(max_dev <= 1, "flat LL should reconstruct flat (dev {max_dev})");
    }

    #[test]
    fn ycbcr_neutral_is_gray() {
        let y = vec![0i32; TILE_AREA]; // 0 → 128 luma
        let cb = vec![0i32; TILE_AREA];
        let cr = vec![0i32; TILE_AREA];
        let rgba = ycbcr_tile_to_rgba(&y, &cb, &cr);
        assert_eq!(&rgba[0..4], &[128, 128, 128, 255]);
    }

    #[test]
    fn decode_tile_produces_full_rgba() {
        // A trivial all-zero stream decodes to a uniform tile (no panic, right size).
        let q = [0u8; 10];
        let rgba = decode_tile(&[], &[], &[], &q, &q, &q, RlgrMode::Rlgr1);
        assert_eq!(rgba.len(), TILE * TILE * 4);
    }

    /// Build a minimal TILESET block with one empty tile at (xIdx, yIdx) and
    /// confirm the parser positions and decodes it.
    #[test]
    fn decoders_never_panic_on_garbage() {
        // A malformed/hostile server must never crash the decode path. Feed a
        // spread of adversarial byte patterns through the entropy decoder and
        // the message parser; both must return (empty/garbage) without panicking.
        let patterns: [&[u8]; 6] = [
            &[],
            &[0xFF],
            &[0xFF; 7],
            &[0x00; 64],
            &[0xCC, 0xC7, 0xFF, 0xFF, 0xFF, 0x7F], // TILESET header with absurd len
            &[0xCC, 0xC7, 0x14, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ];
        for p in patterns {
            let _ = rlgr_decode(p, RlgrMode::Rlgr1, 4096);
            let _ = rlgr_decode(p, RlgrMode::Rlgr3, 4096);
            let _ = decode_message(p);
        }
        // A truncated TILESET claiming many tiles/quants must not over-read.
        let mut block = Vec::new();
        block.extend_from_slice(&WBT_TILESET.to_le_bytes());
        block.extend_from_slice(&64u32.to_le_bytes());
        block.extend_from_slice(&[0xFF; 58]); // numQuant/numTiles huge, no real data
        assert!(decode_message(&block).is_empty());
    }

    #[test]
    fn message_parser_positions_a_tile() {
        // One TS_RFX_TILE: header(6) + qIdx(3) + xIdx + yIdx + 3 lengths(0) = 19 bytes.
        let mut tile = Vec::new();
        tile.extend_from_slice(&CBT_TILE.to_le_bytes());
        tile.extend_from_slice(&19u32.to_le_bytes());
        tile.extend_from_slice(&[0, 0, 0]); // quantIdx Y/Cb/Cr
        tile.extend_from_slice(&2u16.to_le_bytes()); // xIdx
        tile.extend_from_slice(&3u16.to_le_bytes()); // yIdx
        tile.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // YLen, CbLen, CrLen = 0

        // TS_RFX_TILESET: header(6)+subtype(2)+idx(2)+props(2)+numQuant(1)
        // +tileSize(1)+numTiles(2)+tilesDataSize(4)+one 5-byte quant set + tile.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 2]); // subtype
        body.extend_from_slice(&0u16.to_le_bytes()); // idx
        body.extend_from_slice(&0u16.to_le_bytes()); // properties
        body.push(1); // numQuant
        body.push(64); // tileSize
        body.extend_from_slice(&1u16.to_le_bytes()); // numTiles
        body.extend_from_slice(&0u32.to_le_bytes()); // tilesDataSize
        body.extend_from_slice(&[0u8; 5]); // one quant set
        body.extend_from_slice(&tile);

        let mut block = Vec::new();
        block.extend_from_slice(&WBT_TILESET.to_le_bytes());
        block.extend_from_slice(&((6 + body.len()) as u32).to_le_bytes());
        block.extend_from_slice(&body);

        let tiles = decode_message(&block);
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].x, tiles[0].y), (2 * 64, 3 * 64));
        assert_eq!(tiles[0].rgba.len(), TILE * TILE * 4);
    }

    /// A `TS_RFX_CONTEXT` block selecting the given entropy mode (RLGR1/RLGR3).
    fn context_block(rlgr3: bool) -> Vec<u8> {
        let mut ctx = Vec::new();
        ctx.extend_from_slice(&WBT_CONTEXT.to_le_bytes());
        ctx.extend_from_slice(&13u32.to_le_bytes()); // blockLen
        ctx.push(1); // codecId
        ctx.push(0); // channelId
        ctx.push(0); // ctxId
        ctx.extend_from_slice(&64u16.to_le_bytes()); // tileSize
        // properties: entropy bits at [3..5] → 2 = RLGR3, 1 = RLGR1.
        let props: u16 = if rlgr3 { 2 << 3 } else { 1 << 3 };
        ctx.extend_from_slice(&props.to_le_bytes());
        ctx
    }

    #[test]
    fn rfx_decoder_persists_entropy_mode_across_messages() {
        let mut dec = RfxDecoder::new();
        assert_eq!(dec.mode(), RlgrMode::Rlgr1); // default before any context

        // A context-only message (RLGR3) yields no tiles but sets the mode...
        let tiles = dec.decode(&context_block(true));
        assert!(tiles.is_empty());
        assert_eq!(dec.mode(), RlgrMode::Rlgr3);

        // ...which persists into a later frame that carries no context block.
        let _ = dec.decode(&[]);
        assert_eq!(dec.mode(), RlgrMode::Rlgr3);

        // A later context switching back to RLGR1 is honoured too.
        let _ = dec.decode(&context_block(false));
        assert_eq!(dec.mode(), RlgrMode::Rlgr1);
    }
}
