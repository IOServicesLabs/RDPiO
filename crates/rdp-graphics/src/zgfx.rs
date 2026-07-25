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

/// MSB-first bit reader over one compressed segment.
struct BitReader<'a> {
    input: &'a [u8],
    pos: usize,
    acc: u32,
    c_bits: u32,
    remaining: u32,
    bits: u32,
}

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
        let descriptor = *src.first()?;
        let mut out = Vec::new();
        match descriptor {
            SEGMENTED_SINGLE => {
                self.decompress_segment(&src[1..], &mut out)?;
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
                    self.decompress_segment(seg, &mut out)?;
                }
                if out.len() != uncompressed {
                    return None;
                }
            }
            _ => return None,
        }
        Some(out)
    }

    fn history_push(&mut self, c: u8) {
        self.history[self.history_index] = c;
        self.history_index += 1;
        if self.history_index == HISTORY_SIZE {
            self.history_index = 0;
        }
    }

    /// Append `src` to the ring history (used for literals runs / uncompressed).
    fn history_write(&mut self, src: &[u8]) {
        for &b in src {
            self.history_push(b);
        }
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
        // First chunk: up to `distance` bytes straight from history.
        let first = count.min(dist);
        for i in 0..first {
            out.push(self.history[(index + i) % HISTORY_SIZE]);
        }
        // Remainder repeats the just-produced bytes (LZ overlap).
        let mut produced = first;
        while out.len() - start < count {
            let want = count - (out.len() - start);
            let n = produced.min(want);
            for i in 0..n {
                out.push(out[start + i]);
            }
            produced <<= 1;
            if n == 0 {
                break; // defensive: distance==0 already handled, never reached
            }
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
                        let produced: Vec<u8> = out[mstart..].to_vec();
                        self.history_write(&produced);
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
}
