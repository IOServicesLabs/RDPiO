//! ZGFX (RDP 8.0) bulk decompression — MS-RDPEGFX 2.2.5.1 / 3.1.5.1.
//!
//! EGFX channel data arrives wrapped in `RDP_SEGMENTED_DATA` (single or
//! multipart) where each segment is `RDP8_BULK_ENCODED_DATA`: a 1-byte header
//! (compression type in the low nibble, `PACKET_COMPRESSED` = 0x20) followed by
//! either raw bytes or a ZGFX token stream. The token stream is a history-based
//! LZ scheme with a static Huffman prefix table; bits are read MSB-first and the
//! final byte of a compressed segment gives the number of unused trailing bits.
//!
//! This is a faithful port of the reference algorithm (token table, bit reader,
//! match-length code, and ring-history with overlapping copy). The history
//! buffer persists across calls because the channel is one continuous stream, so
//! a [`Zgfx`] context is long-lived for the connection.

const SEGMENTED_SINGLE: u8 = 0xE0;
const SEGMENTED_MULTIPART: u8 = 0xE1;
const PACKET_COMPRESSED: u8 = 0x20;
const HISTORY_SIZE: usize = 2_500_000;
/// Per-segment decompressed output cap (matches the reference OutputBuffer).
const MAX_SEGMENT_OUTPUT: usize = 65_536;

/// One static-Huffman token. `token_type` 0 = literal (output `value_base +
/// value_bits`), 1 = match (distance = `value_base + value_bits`).
struct Token {
    prefix_len: u8,
    prefix_code: u16,
    value_bits: u8,
    token_type: u8,
    value_base: u32,
}

const fn t(
    prefix_len: u8,
    prefix_code: u16,
    value_bits: u8,
    token_type: u8,
    value_base: u32,
) -> Token {
    Token {
        prefix_len,
        prefix_code,
        value_bits,
        token_type,
        value_base,
    }
}

/// The ZGFX token table (MS-RDPEGFX 2.2.5.1), ordered by ascending prefix length.
const TOKENS: &[Token] = &[
    t(1, 0, 8, 0, 0),
    t(5, 17, 5, 1, 0),
    t(5, 18, 7, 1, 32),
    t(5, 19, 9, 1, 160),
    t(5, 20, 10, 1, 672),
    t(5, 21, 12, 1, 1696),
    t(5, 24, 0, 0, 0x00),
    t(5, 25, 0, 0, 0x01),
    t(6, 44, 14, 1, 5792),
    t(6, 45, 15, 1, 22176),
    t(6, 52, 0, 0, 0x02),
    t(6, 53, 0, 0, 0x03),
    t(6, 54, 0, 0, 0xFF),
    t(7, 92, 18, 1, 54944),
    t(7, 93, 20, 1, 317088),
    t(7, 110, 0, 0, 0x04),
    t(7, 111, 0, 0, 0x05),
    t(7, 112, 0, 0, 0x06),
    t(7, 113, 0, 0, 0x07),
    t(7, 114, 0, 0, 0x08),
    t(7, 115, 0, 0, 0x09),
    t(7, 116, 0, 0, 0x0A),
    t(7, 117, 0, 0, 0x0B),
    t(7, 118, 0, 0, 0x3A),
    t(7, 119, 0, 0, 0x3B),
    t(7, 120, 0, 0, 0x3C),
    t(7, 121, 0, 0, 0x3D),
    t(7, 122, 0, 0, 0x3E),
    t(7, 123, 0, 0, 0x3F),
    t(7, 124, 0, 0, 0x40),
    t(7, 125, 0, 0, 0x80),
    t(8, 188, 20, 1, 1365664),
    t(8, 189, 21, 1, 2414240),
    t(8, 252, 0, 0, 0x0C),
    t(8, 253, 0, 0, 0x38),
    t(8, 254, 0, 0, 0x39),
    t(8, 255, 0, 0, 0x66),
    t(9, 380, 22, 1, 4511392),
    t(9, 381, 23, 1, 8705696),
    t(9, 382, 24, 1, 17094304),
];

/// Longest token prefix in [`TOKENS`]: the decoder peeks this many bits and
/// resolves the token with one lookup instead of a bit-at-a-time table scan.
const PREFIX_BITS: u32 = 9;

const KIND_INVALID: u8 = 0;
const KIND_LITERAL: u8 = 1;
const KIND_MATCH: u8 = 2;

/// One [`DECODE`] slot. Literal tokens carry at most 8 value bits behind a
/// 1-bit prefix (or none behind longer ones), so the whole literal fits the
/// 9-bit peek: `len` is then prefix + value bits and `base` is the byte itself.
/// Match tokens: `len` is the prefix only, `vbits`/`base` the distance code.
#[derive(Clone, Copy)]
struct Slot {
    base: u32,
    len: u8,
    vbits: u8,
    kind: u8,
}

const fn build_decode() -> [Slot; 1 << PREFIX_BITS] {
    let mut table = [Slot {
        base: 0,
        len: 0,
        vbits: 0,
        kind: KIND_INVALID,
    }; 1 << PREFIX_BITS];
    // Scan in table order so the first (shortest) matching prefix wins, as in
    // the reference scan.
    let mut i = TOKENS.len();
    while i > 0 {
        i -= 1;
        let tok = &TOKENS[i];
        let shift = PREFIX_BITS - tok.prefix_len as u32;
        let first = (tok.prefix_code as usize) << shift;
        let mut idx = first;
        while idx < first + (1 << shift) {
            table[idx] = if tok.token_type == 0 {
                // Fold the literal's value bits into the peek.
                let value_bits = tok.value_bits as u32;
                let rest = shift - value_bits;
                Slot {
                    base: tok.value_base + ((idx as u32 & ((1 << shift) - 1)) >> rest),
                    len: tok.prefix_len + tok.value_bits,
                    vbits: 0,
                    kind: KIND_LITERAL,
                }
            } else {
                Slot {
                    base: tok.value_base,
                    len: tok.prefix_len,
                    vbits: tok.value_bits,
                    kind: KIND_MATCH,
                }
            };
            idx += 1;
        }
    }
    table
}

static DECODE: [Slot; 1 << PREFIX_BITS] = build_decode();

/// MSB-first bit cursor over one compressed segment: peeks load 8 bytes at
/// once (zero-filled past the end, matching the reference's zero shift-in).
struct Bits<'a> {
    input: &'a [u8],
    bitpos: usize,
}

impl Bits<'_> {
    /// The next 64 stream bits, left-aligned (only the top 57 are guaranteed).
    #[inline(always)]
    fn word(&self) -> u64 {
        let byte = self.bitpos >> 3;
        let w = match self.input.get(byte..byte + 8) {
            Some(b) => u64::from_be_bytes(b.try_into().unwrap()),
            None => {
                let mut b = [0u8; 8];
                let tail = self.input.get(byte..).unwrap_or(&[]);
                b[..tail.len()].copy_from_slice(tail);
                u64::from_be_bytes(b)
            }
        };
        w << (self.bitpos & 7)
    }

    /// Read `n` bits (1..=32).
    #[inline(always)]
    fn read(&mut self, n: u32) -> u32 {
        let v = (self.word() >> (64 - n)) as u32;
        self.bitpos += n as usize;
        v
    }
}

/// MSB-first bit reader over one compressed segment (the reference decoder's).
#[cfg(test)]
struct BitReader<'a> {
    input: &'a [u8],
    pos: usize,
    acc: u32,
    c_bits: u32,
    remaining: u32,
    bits: u32,
}

#[cfg(test)]
impl<'a> BitReader<'a> {
    /// Read `n` bits (0..=24), leaving the value in `self.bits`. Past the input
    /// end, zero bits are shifted in (matching the reference).
    fn get(&mut self, n: u32) {
        while self.c_bits < n {
            self.acc <<= 8;
            if self.pos < self.input.len() {
                self.acc += self.input[self.pos] as u32;
                self.pos += 1;
            }
            self.c_bits += 8;
        }
        self.remaining = self.remaining.saturating_sub(n);
        self.c_bits -= n;
        self.bits = self.acc >> self.c_bits;
        self.acc &= (1u32 << self.c_bits).wrapping_sub(1);
    }
}

/// A streaming ZGFX decompression context (owns the LZ history buffer).
pub struct Zgfx {
    history: Vec<u8>,
    history_index: usize,
}

impl Default for Zgfx {
    fn default() -> Self {
        Self::new()
    }
}

impl Zgfx {
    pub fn new() -> Self {
        Zgfx {
            history: vec![0u8; HISTORY_SIZE],
            history_index: 0,
        }
    }

    /// Decompress one `RDP_SEGMENTED_DATA` blob (a full channel PDU payload) into
    /// the concatenated uncompressed bytes, or `None` if malformed.
    pub fn decompress(&mut self, src: &[u8]) -> Option<Vec<u8>> {
        self.decompress_with(src, Self::decompress_segment)
    }

    #[inline(always)]
    fn decompress_with(
        &mut self,
        src: &[u8],
        segment: fn(&mut Self, &[u8], &mut Vec<u8>) -> Option<()>,
    ) -> Option<Vec<u8>> {
        let descriptor = *src.first()?;
        let mut out = Vec::new();
        match descriptor {
            SEGMENTED_SINGLE => {
                segment(self, &src[1..], &mut out)?;
            }
            SEGMENTED_MULTIPART => {
                // segmentCount(2) + uncompressedSize(4) + [segmentSize(4) + seg]...
                let count = u16::from_le_bytes([*src.get(1)?, *src.get(2)?]) as usize;
                let uncompressed =
                    u32::from_le_bytes([*src.get(3)?, *src.get(4)?, *src.get(5)?, *src.get(6)?])
                        as usize;
                let mut off = 7;
                for _ in 0..count {
                    let seg_size = u32::from_le_bytes([
                        *src.get(off)?,
                        *src.get(off + 1)?,
                        *src.get(off + 2)?,
                        *src.get(off + 3)?,
                    ]) as usize;
                    off += 4;
                    let seg = src.get(off..off + seg_size)?;
                    off += seg_size;
                    segment(self, seg, &mut out)?;
                }
                if out.len() != uncompressed {
                    return None;
                }
            }
            _ => return None,
        }
        Some(out)
    }

    /// Append `src` to the ring history (used for literals runs / uncompressed
    /// data and match feedback). Chunked `copy_from_slice` — at most two
    /// segments around the wrap — instead of a per-byte loop: every EGFX byte
    /// flows through here, so the per-byte wrap check was a measurable
    /// constant factor on the whole graphics stream.
    fn history_write(&mut self, src: &[u8]) {
        let n = src.len();
        if n == 0 {
            return;
        }
        if n >= HISTORY_SIZE {
            // Only the trailing window survives; lay it out so the final
            // write position matches the byte-at-a-time result exactly.
            let tail = &src[n - HISTORY_SIZE..];
            let end = (self.history_index + n) % HISTORY_SIZE;
            let first_len = HISTORY_SIZE - end;
            self.history[end..].copy_from_slice(&tail[..first_len]);
            self.history[..end].copy_from_slice(&tail[first_len..]);
            self.history_index = end;
            return;
        }
        let idx = self.history_index;
        let first = n.min(HISTORY_SIZE - idx);
        self.history[idx..idx + first].copy_from_slice(&src[..first]);
        let rest = n - first;
        self.history[..rest].copy_from_slice(&src[first..]);
        self.history_index = (idx + n) % HISTORY_SIZE;
    }

    /// Copy a `count`-byte match at `distance` from the history into `out`,
    /// reproducing the overlapping-copy behaviour when `count > distance`.
    fn match_into(&self, distance: u32, count: usize, out: &mut Vec<u8>) {
        if distance == 0 || count == 0 {
            return;
        }
        let start = out.len();
        let dist = (distance as usize) % HISTORY_SIZE;
        let index = (self.history_index + HISTORY_SIZE - dist) % HISTORY_SIZE;
        // First chunk: up to `distance` bytes straight from history — at most
        // two contiguous runs around the ring wrap.
        let first = count.min(dist);
        let run1 = first.min(HISTORY_SIZE - index);
        out.extend_from_slice(&self.history[index..index + run1]);
        out.extend_from_slice(&self.history[..first - run1]);
        // Remainder repeats the just-produced bytes (LZ overlap): doubling
        // block copies of the leading period.
        while out.len() - start < count {
            let produced = out.len() - start;
            let n = produced.min(count - produced);
            out.extend_from_within(start..start + n);
        }
    }

    fn decompress_segment(&mut self, seg: &[u8], out: &mut Vec<u8>) -> Option<()> {
        if seg.len() < 2 {
            return None;
        }
        let flags = seg[0];
        let body = &seg[1..]; // cbSegment bytes
        let seg_start = out.len();

        if flags & PACKET_COMPRESSED == 0 {
            if body.len() > MAX_SEGMENT_OUTPUT {
                return None;
            }
            self.history_write(body);
            out.extend_from_slice(body);
            return Some(());
        }

        // Compressed: the final body byte is the count of unused bits.
        let last = *body.last()? as u32;
        let input = &body[..body.len() - 1];
        let total_bits = 8u32.checked_mul(input.len() as u32)?;
        if total_bits < last {
            return None;
        }
        let mut r = Bits { input, bitpos: 0 };
        // The ring lags `out`: literals, raw runs and match output are fed
        // back in bulk right before the next match reads it, and on exit —
        // including a malformed exit, so the ring ends where the reference's does.
        let mut synced = seg_start;
        let res = self.decode_tokens(&mut r, total_bits - last, out, seg_start, &mut synced);
        self.history_write(&out[synced..]);
        res
    }

    /// The token loop of [`Self::decompress_segment`]: a 9-bit peek resolves
    /// each token through [`DECODE`]. `remaining` mirrors the reference's
    /// saturating bit budget exactly (tokens may run into the pad bits).
    #[inline(always)]
    fn decode_tokens(
        &mut self,
        r: &mut Bits,
        mut remaining: u32,
        out: &mut Vec<u8>,
        seg_start: usize,
        synced: &mut usize,
    ) -> Option<()> {
        while remaining > 0 {
            if out.len() - seg_start > MAX_SEGMENT_OUTPUT {
                return None;
            }
            let w = r.word();
            let slot = DECODE[(w >> (64 - PREFIX_BITS)) as usize];
            if slot.kind == KIND_LITERAL {
                if remaining >= 6 * PREFIX_BITS && out.len() - seg_start + 5 <= MAX_SEGMENT_OUTPUT {
                    // Literal burst: up to 6 literals (<= 54 bits) from this one
                    // peek. The guards above make the per-token budget and
                    // output-cap checks of the reference pass for all of them.
                    let mut buf = [0u8; 6];
                    let mut n = 0;
                    let mut used = 0u32;
                    let mut s = slot;
                    loop {
                        buf[n] = s.base as u8;
                        n += 1;
                        used += s.len as u32;
                        if n == buf.len() {
                            break;
                        }
                        s = DECODE[((w << used) >> (64 - PREFIX_BITS)) as usize];
                        if s.kind != KIND_LITERAL {
                            break;
                        }
                    }
                    out.extend_from_slice(&buf[..n]);
                    r.bitpos += used as usize;
                    remaining -= used;
                    continue;
                }
                r.bitpos += slot.len as usize;
                remaining = remaining.saturating_sub(slot.len as u32);
                out.push(slot.base as u8);
                continue;
            }
            if slot.kind != KIND_MATCH {
                return None; // no token matched a malformed prefix
            }
            // Prefix (<= 9) + distance bits (<= 24) fit the 57-bit peek.
            let vbits = slot.vbits as u32;
            let used = slot.len as u32 + vbits;
            let distance = slot.base + ((w << slot.len) >> (64 - vbits)) as u32;
            r.bitpos += used as usize;
            remaining = remaining.saturating_sub(used);
            if distance != 0 {
                // Length code: "0" = 3, else "1", k more ones, a "0", then
                // 2 + k extra bits over a base of 4 << k.
                let w = r.word();
                let count = if w >> 63 == 0 {
                    r.bitpos += 1;
                    remaining = remaining.saturating_sub(1);
                    3usize
                } else {
                    let k = (w << 1).leading_ones();
                    // k >= 15 means count >= 128 Ki, which the reference
                    // rejects too (it only diverges at k >= 30, where its
                    // u32 arithmetic overflows).
                    if k >= 15 {
                        return None;
                    }
                    let extra = 2 + k;
                    let used = 2 + k + extra; // <= 32 bits: fits the peek
                    let bits = ((w << (2 + k)) >> (64 - extra)) as usize;
                    r.bitpos += used as usize;
                    remaining = remaining.saturating_sub(used);
                    (4usize << k) + bits
                };
                if count > MAX_SEGMENT_OUTPUT {
                    return None;
                }
                self.history_write(&out[*synced..]);
                *synced = out.len();
                self.match_into(distance, count, out);
            } else {
                // Unencoded run: 15-bit count, then raw bytes, byte-aligned.
                let count = r.read(15) as usize;
                remaining = remaining.saturating_sub(15);
                // Drop the rest of the current byte, as the reference does.
                remaining = remaining.saturating_sub(((8 - (r.bitpos & 7)) & 7) as u32);
                let pos = r.bitpos.div_ceil(8).min(r.input.len());
                let raw = r.input.get(pos..pos + count)?;
                out.extend_from_slice(raw);
                r.bitpos = (pos + count) * 8;
                remaining = remaining.saturating_sub(8 * count as u32);
            }
        }
        Some(())
    }
}

/// The reference (bit-at-a-time, linear token scan) decoder, kept to prove the
/// table-driven one byte-identical and to benchmark against.
#[cfg(test)]
impl Zgfx {
    fn decompress_ref(&mut self, src: &[u8]) -> Option<Vec<u8>> {
        self.decompress_with(src, Self::decompress_segment_ref)
    }

    fn history_push(&mut self, c: u8) {
        self.history[self.history_index] = c;
        self.history_index += 1;
        if self.history_index == HISTORY_SIZE {
            self.history_index = 0;
        }
    }

    fn decompress_segment_ref(&mut self, seg: &[u8], out: &mut Vec<u8>) -> Option<()> {
        if seg.len() < 2 {
            return None;
        }
        let flags = seg[0];
        let body = &seg[1..]; // cbSegment bytes
        let seg_start = out.len();

        if flags & PACKET_COMPRESSED == 0 {
            if body.len() > MAX_SEGMENT_OUTPUT {
                return None;
            }
            self.history_write(body);
            out.extend_from_slice(body);
            return Some(());
        }

        // Compressed: the final body byte is the count of unused bits.
        let last = *body.last()? as u32;
        let input = &body[..body.len() - 1];
        let total_bits = 8u32.checked_mul(input.len() as u32)?;
        if total_bits < last {
            return None;
        }
        let mut r = BitReader {
            input,
            pos: 0,
            acc: 0,
            c_bits: 0,
            remaining: total_bits - last,
            bits: 0,
        };

        while r.remaining > 0 {
            if out.len() - seg_start > MAX_SEGMENT_OUTPUT {
                return None;
            }
            let mut have = 0u32;
            let mut prefix = 0u32;
            let mut matched = false;
            for tok in TOKENS {
                while have < tok.prefix_len as u32 {
                    r.get(1);
                    prefix = (prefix << 1) + r.bits;
                    have += 1;
                }
                if prefix != tok.prefix_code as u32 {
                    continue;
                }
                matched = true;
                if tok.token_type == 0 {
                    // Literal: value_base + value_bits extra bits.
                    r.get(tok.value_bits as u32);
                    let c = (tok.value_base + r.bits) as u8;
                    self.history_push(c);
                    out.push(c);
                } else {
                    r.get(tok.value_bits as u32);
                    let distance = tok.value_base + r.bits;
                    if distance != 0 {
                        // Match: decode the length code, then copy.
                        r.get(1);
                        let count = if r.bits == 0 {
                            3usize
                        } else {
                            let mut count = 4u32;
                            let mut extra = 2u32;
                            r.get(1);
                            while r.bits == 1 {
                                count *= 2;
                                extra += 1;
                                r.get(1);
                            }
                            r.get(extra);
                            (count + r.bits) as usize
                        };
                        if count > MAX_SEGMENT_OUTPUT {
                            return None;
                        }
                        let mstart = out.len();
                        self.match_into(distance, count, out);
                        // Feed the produced bytes straight back into the ring
                        // (the old heap clone per match token was pure waste —
                        // `out` and `self.history` never alias).
                        self.history_write(&out[mstart..]);
                    } else {
                        // Unencoded run: 15-bit count, then raw bytes, byte-aligned.
                        r.get(15);
                        let count = r.bits as usize;
                        r.remaining = r.remaining.saturating_sub(r.c_bits);
                        r.c_bits = 0;
                        r.acc = 0;
                        let raw = r.input.get(r.pos..r.pos + count)?;
                        out.extend_from_slice(raw);
                        self.history_write(raw);
                        r.pos += count;
                        r.remaining = r.remaining.saturating_sub(8 * count as u32);
                    }
                }
                break;
            }
            if !matched {
                return None; // no token matched a malformed prefix
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chunked `history_write` must lay bytes out exactly like the old
    /// byte-at-a-time push, across every wrap case (including a source bigger
    /// than the whole ring, where only the tail survives).
    #[test]
    fn history_write_matches_byte_at_a_time_model() {
        let mut z = Zgfx::new();
        let mut model = vec![0u8; HISTORY_SIZE];
        let mut mi = 0usize;
        let chunks: Vec<Vec<u8>> = vec![
            (0..1000u32).map(|i| (i % 251) as u8).collect(),
            vec![7u8; HISTORY_SIZE - 500],
            (0..(HISTORY_SIZE as u32 + 12_345))
                .map(|i| (i % 249) as u8)
                .collect(),
            vec![9u8; 3],
        ];
        for c in &chunks {
            z.history_write(c);
            for &b in c {
                model[mi] = b;
                mi = (mi + 1) % HISTORY_SIZE;
            }
            assert_eq!(z.history_index, mi);
            assert_eq!(z.history, model);
        }
    }

    /// A match whose source range spans the ring wrap point must come out
    /// contiguous, and an overlapping match (count > distance) must repeat the
    /// leading period.
    #[test]
    fn match_into_copies_across_the_ring_wrap() {
        let mut z = Zgfx::new();
        z.history_write(&vec![0u8; HISTORY_SIZE - 4]);
        z.history_write(&[1, 2, 3, 4, 5, 6, 7, 8]); // last 4 bytes wrap to the front
        let mut out = Vec::new();
        z.match_into(8, 8, &mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        let mut out2 = Vec::new();
        z.match_into(2, 7, &mut out2);
        assert_eq!(out2, [7, 8, 7, 8, 7, 8, 7]);
    }

    /// Single uncompressed segment: descriptor 0xE0 + flags(uncompressed) + raw.
    #[test]
    fn uncompressed_single_segment() {
        let mut z = Zgfx::new();
        let pdu = [0xE0, 0x04, b'h', b'i', b'!'];
        assert_eq!(z.decompress(&pdu).unwrap(), b"hi!");
    }

    /// Multipart with two uncompressed segments concatenates and length-checks.
    #[test]
    fn uncompressed_multipart() {
        let mut z = Zgfx::new();
        let mut pdu = vec![0xE1];
        pdu.extend_from_slice(&2u16.to_le_bytes()); // segmentCount
        pdu.extend_from_slice(&6u32.to_le_bytes()); // uncompressedSize
                                                    // segment 1: size, flags=uncompressed, "abc"
        pdu.extend_from_slice(&4u32.to_le_bytes());
        pdu.extend_from_slice(&[0x04, b'a', b'b', b'c']);
        // segment 2: size, flags=uncompressed, "def"
        pdu.extend_from_slice(&4u32.to_le_bytes());
        pdu.extend_from_slice(&[0x04, b'd', b'e', b'f']);
        assert_eq!(z.decompress(&pdu).unwrap(), b"abcdef");
    }

    /// Compressed literal-only stream (two literal tokens), hand-encoded:
    /// "0 01000001" + "0 01000010" → bytes 0x20,0x90,0x80 with 6 pad bits.
    #[test]
    fn compressed_two_literals() {
        let mut z = Zgfx::new();
        let pdu = [0xE0, 0x24, 0x20, 0x90, 0x80, 0x06];
        assert_eq!(z.decompress(&pdu).unwrap(), b"AB");
    }

    /// Compressed literals + a back-reference match: "ABC" then match
    /// distance=3, count=3 → "ABCABC" (exercises the match + overlap path).
    #[test]
    fn compressed_match_backreference() {
        let mut z = Zgfx::new();
        let pdu = [0xE0, 0x24, 0x20, 0x90, 0x88, 0x71, 0x18, 0x02];
        assert_eq!(z.decompress(&pdu).unwrap(), b"ABCABC");
    }

    /// History persists across PDUs: a match in a second PDU can reference bytes
    /// emitted by the first.
    #[test]
    fn history_persists_across_pdus() {
        let mut z = Zgfx::new();
        assert_eq!(
            z.decompress(&[0xE0, 0x04, b'A', b'B', b'C']).unwrap(),
            b"ABC"
        );
        // Second PDU: a single match distance=3,count=3 referencing "ABC".
        // 10001 00011 0 → bytes 0x88,0xC0 with pad. Build precisely:
        // bits: 1,0,0,0,1, 0,0,0,1,1, 0  (11 bits) → 0x88, 0xC0(=11000000); used 11, pad 5.
        let pdu = [0xE0, 0x24, 0x88, 0xC0, 0x05];
        assert_eq!(z.decompress(&pdu).unwrap(), b"ABC");
    }

    #[test]
    fn rejects_unknown_descriptor() {
        let mut z = Zgfx::new();
        assert!(z.decompress(&[0x99, 0x00]).is_none());
    }

    /// Deterministic xorshift for the generated corpora.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// MSB-first bit writer for the test encoder.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        acc: u64,
        n: u32,
    }
    impl BitWriter {
        fn put(&mut self, v: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.acc = (self.acc << 1) | ((v >> i) & 1) as u64;
                self.n += 1;
                if self.n == 8 {
                    self.bytes.push(self.acc as u8);
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
        fn align(&mut self) -> u8 {
            let pad = (8 - self.n) % 8;
            self.put(0, pad);
            pad as u8
        }
    }

    /// A greedy LZ encoder producing valid ZGFX token streams (literals with
    /// their shortest code, matches over the whole persistent history, and
    /// raw runs for incompressible stretches), so the decoder can be
    /// exercised and timed without captured EGFX traffic.
    struct Encoder {
        seen: Vec<u8>,
        head: Vec<u32>,
    }
    impl Encoder {
        fn new() -> Self {
            Encoder {
                seen: Vec::new(),
                head: vec![u32::MAX; 1 << 16],
            }
        }
        fn hash(b: &[u8]) -> usize {
            let v = (b[0] as u32) | (b[1] as u32) << 8 | (b[2] as u32) << 16;
            (v.wrapping_mul(2654435761) >> 16) as usize
        }
        fn literal(w: &mut BitWriter, c: u8) {
            for tok in TOKENS {
                if tok.token_type == 0 && tok.value_bits == 0 && tok.value_base == c as u32 {
                    return w.put(tok.prefix_code as u32, tok.prefix_len as u32);
                }
            }
            w.put(0, 1);
            w.put(c as u32, 8);
        }
        fn distance(w: &mut BitWriter, d: u32) {
            let tok = TOKENS
                .iter()
                .find(|t| {
                    t.token_type == 1 && d >= t.value_base && d - t.value_base < 1 << t.value_bits
                })
                .unwrap();
            w.put(tok.prefix_code as u32, tok.prefix_len as u32);
            w.put(d - tok.value_base, tok.value_bits as u32);
        }
        fn length(w: &mut BitWriter, count: u32) {
            if count == 3 {
                return w.put(0, 1);
            }
            let k = 31 - (count / 4).leading_zeros();
            w.put(1, 1);
            for _ in 0..k {
                w.put(1, 1);
            }
            w.put(0, 1);
            w.put(count - (4 << k), 2 + k);
        }
        fn raw_run(w: &mut BitWriter, data: &[u8]) {
            w.put(17, 5);
            w.put(0, 5);
            w.put(data.len() as u32, 15);
            w.align();
            w.bytes.extend_from_slice(data);
        }
        /// One compressed segment (flags + bits + pad byte) for `data`.
        fn segment(&mut self, data: &[u8], raw_runs: bool) -> Vec<u8> {
            let mut w = BitWriter::default();
            let mut i = 0;
            let mut lit_run = 0usize;
            while i < data.len() {
                let abs = self.seen.len();
                let mut best = (0usize, 0usize);
                if i + 3 <= data.len() {
                    let h = Self::hash(&data[i..]);
                    let cand = self.head[h];
                    if cand != u32::MAX && abs - cand as usize <= 2_000_000 {
                        let c = cand as usize;
                        let mut l = 0;
                        while i + l < data.len() && l < 4096 {
                            let src = c + l;
                            let b = if src < abs {
                                self.seen[src]
                            } else {
                                data[i + src - abs]
                            };
                            if b != data[i + l] {
                                break;
                            }
                            l += 1;
                        }
                        if l >= 3 {
                            best = (abs - c, l);
                        }
                    }
                    self.head[h] = abs as u32;
                }
                if best.1 >= 3 {
                    Self::distance(&mut w, best.0 as u32);
                    Self::length(&mut w, best.1 as u32);
                    lit_run = 0;
                    for k in 0..best.1 {
                        self.seen.push(data[i + k]);
                        if k > 0 && i + k + 3 <= data.len() {
                            let h = Self::hash(&data[i + k..]);
                            self.head[h] = (abs + k) as u32;
                        }
                    }
                    i += best.1;
                } else if raw_runs && lit_run >= 16 {
                    // Incompressible: switch to a byte-aligned raw run.
                    let n = (data.len() - i).min(256);
                    Self::raw_run(&mut w, &data[i..i + n]);
                    self.seen.extend_from_slice(&data[i..i + n]);
                    i += n;
                    lit_run = 0;
                } else {
                    Self::literal(&mut w, data[i]);
                    self.seen.push(data[i]);
                    lit_run += 1;
                    i += 1;
                }
            }
            let pad = w.align();
            let mut seg = vec![0x24];
            seg.extend_from_slice(&w.bytes);
            seg.push(pad);
            seg
        }
        /// A full `RDP_SEGMENTED_DATA` PDU (single or multipart).
        fn pdu(&mut self, data: &[u8], raw_runs: bool) -> Vec<u8> {
            if data.len() <= MAX_SEGMENT_OUTPUT {
                let mut p = vec![0xE0];
                p.extend(self.segment(data, raw_runs));
                return p;
            }
            let chunks: Vec<&[u8]> = data.chunks(MAX_SEGMENT_OUTPUT).collect();
            let mut p = vec![0xE1];
            p.extend_from_slice(&(chunks.len() as u16).to_le_bytes());
            p.extend_from_slice(&(data.len() as u32).to_le_bytes());
            for c in chunks {
                let seg = self.segment(c, raw_runs);
                p.extend_from_slice(&(seg.len() as u32).to_le_bytes());
                p.extend(seg);
            }
            p
        }
    }

    /// EGFX-like UI traffic: command headers, ClearCodec-ish palette/run
    /// bitmaps and repeated glyph rows (text) with small variations.
    fn ui_payload(rng: &mut Rng, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let palette: Vec<[u8; 4]> = (0..6)
            .map(|_| [rng.next() as u8, rng.next() as u8, rng.next() as u8, 0xFF])
            .collect();
        let glyphs: Vec<Vec<u8>> = (0..24)
            .map(|_| {
                (0..rng.below(40) as usize + 8)
                    .map(|_| (rng.below(4) * 85) as u8)
                    .collect()
            })
            .collect();
        while v.len() < len {
            match rng.below(4) {
                0 => {
                    // Command header + rect.
                    v.extend_from_slice(&[0x0A, 0x00, 0x00, 0x00]);
                    v.extend_from_slice(&(rng.below(4000) as u32).to_le_bytes());
                    v.extend_from_slice(&(rng.below(1920) as u16).to_le_bytes());
                    v.extend_from_slice(&(rng.below(1080) as u16).to_le_bytes());
                }
                1 => {
                    // Palette run row.
                    let c = palette[rng.below(6) as usize];
                    for _ in 0..rng.below(64) + 1 {
                        v.extend_from_slice(&c);
                    }
                }
                2 => {
                    for _ in 0..rng.below(12) + 1 {
                        v.extend_from_slice(&glyphs[rng.below(24) as usize]);
                    }
                }
                _ => {
                    // A little noise (anti-aliasing, counters).
                    for _ in 0..rng.below(24) {
                        v.push(rng.next() as u8);
                    }
                }
            }
        }
        v.truncate(len);
        v
    }

    fn payload(rng: &mut Rng, kind: u8, len: usize) -> Vec<u8> {
        match kind {
            0 => ui_payload(rng, len),
            _ => (0..len).map(|_| rng.next() as u8).collect(),
        }
    }

    /// A stream of PDUs. Kind 0 = compressible UI traffic; 1 = AVC-like
    /// high-entropy payloads as literals; 2 = the same as raw runs.
    fn corpus(seed: u64, pdus: usize, kind: u8) -> (Vec<Vec<u8>>, usize) {
        let mut rng = Rng(seed);
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        let mut total = 0;
        for _ in 0..pdus {
            let len = rng.below(90_000) as usize + 32;
            let data = payload(&mut rng, kind, len);
            total += data.len();
            out.push(enc.pdu(&data, kind == 2));
        }
        (out, total)
    }

    /// The encoder round-trips through the reference decoder (the encoder
    /// is only trusted because of this).
    #[test]
    fn encoder_round_trips_through_reference() {
        for kind in 0..3u8 {
            let mut rng = Rng(77 + kind as u64);
            let mut enc = Encoder::new();
            let mut z = Zgfx::new();
            for _ in 0..20 {
                let len = rng.below(150_000) as usize + 1;
                let data = payload(&mut rng, kind, len);
                let pdu = enc.pdu(&data, kind == 2);
                assert_eq!(z.decompress_ref(&pdu).unwrap(), data);
            }
        }
    }

    /// The table-driven decoder must match the reference byte for byte —
    /// output, `None`s and the ring state after every PDU — on valid streams
    /// and on corrupted ones (bit flips, truncation, random garbage).
    #[test]
    fn table_decoder_matches_reference() {
        let mut a = Zgfx::new();
        let mut b = Zgfx::new();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let check = |a: &mut Zgfx, b: &mut Zgfx, pdu: &[u8]| {
            assert_eq!(a.decompress(pdu), b.decompress_ref(pdu));
            assert_eq!(a.history_index, b.history_index);
        };
        for kind in 0..3u8 {
            let (pdus, _) = corpus(1000 + kind as u64, 40, kind);
            for p in &pdus {
                check(&mut a, &mut b, p);
                // Corrupted copies: flipped bits, then a truncation.
                let mut q = p.clone();
                for _ in 0..rng.below(4) + 1 {
                    let i = 1 + rng.below(q.len() as u64 - 1) as usize;
                    q[i] ^= 1 << rng.below(8);
                }
                check(&mut a, &mut b, &q);
                let cut = 1 + rng.below(p.len() as u64) as usize;
                check(&mut a, &mut b, &p[..cut]);
            }
        }
        for _ in 0..3000 {
            let len = rng.below(400) as usize + 3;
            let mut g: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            g[0] = 0xE0;
            g[1] |= PACKET_COMPRESSED;
            if rng.below(2) == 0 {
                g[len - 1] &= 7;
            }
            check(&mut a, &mut b, &g);
        }
        assert!(a.history == b.history);
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    /// `cargo test -p rdp-graphics --release -- --ignored --nocapture bench_zgfx`
    #[test]
    #[ignore]
    fn bench_zgfx() {
        for (kind, name) in [(0u8, "ui"), (1, "avc-literals"), (2, "avc-rawruns")] {
            let (pdus, total) = corpus(42 + kind as u64, 120, kind);
            let comp: usize = pdus.iter().map(|p| p.len()).sum();
            let mut times = [Vec::new(), Vec::new()];
            for _ in 0..9 {
                for (which, t) in times.iter_mut().enumerate() {
                    let mut z = Zgfx::new();
                    let t0 = std::time::Instant::now();
                    for p in &pdus {
                        let o = if which == 0 {
                            z.decompress_ref(p)
                        } else {
                            z.decompress(p)
                        };
                        std::hint::black_box(o.unwrap());
                    }
                    t.push(total as f64 / t0.elapsed().as_secs_f64() / 1e6);
                }
            }
            let (old, new) = (median(times[0].clone()), median(times[1].clone()));
            println!(
                "zgfx {name:12} {:.1} MB -> {:.1} MB  ref {old:7.1} MB/s  table {new:7.1} MB/s  x{:.2}",
                comp as f64 / 1e6,
                total as f64 / 1e6,
                new / old
            );
        }
    }
}
