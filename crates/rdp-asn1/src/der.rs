//! Distinguished Encoding Rules: length codec plus a small TLV reader/writer.
//!
//! Only what CredSSP (MS-CSSP) needs: SEQUENCE, INTEGER, OCTET STRING, and
//! EXPLICIT context-specific tags `[n]`. Writers build values inner-first and
//! wrap them; readers take a `&mut &[u8]` cursor and advance it.

use super::Asn1Error;

/// Universal tag: INTEGER.
pub const TAG_INTEGER: u8 = 0x02;
/// Universal tag: BIT STRING.
pub const TAG_BIT_STRING: u8 = 0x03;
/// Universal tag: OCTET STRING.
pub const TAG_OCTET_STRING: u8 = 0x04;
/// Universal tag: SEQUENCE / SEQUENCE OF (constructed).
pub const TAG_SEQUENCE: u8 = 0x30;

/// The constructed context-specific tag byte for `[n]` (EXPLICIT tagging).
#[inline]
pub fn context_tag(n: u8) -> u8 {
    0xA0 | (n & 0x1F)
}

// --- Length codec -----------------------------------------------------------

/// Encode an ASN.1 definite length in DER form, appending to `out`.
///
/// Lengths below `0x80` use the single-byte short form; larger values use the
/// long form with the minimum number of significant bytes.
pub fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first_significant = bytes
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(bytes.len() - 1);
        let significant = &bytes[first_significant..];
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
}

/// Decode an ASN.1 definite length from the front of `input`, advancing the
/// cursor. Rejects indefinite and non-minimal encodings, as DER requires.
pub fn decode_length(input: &mut &[u8]) -> Result<usize, Asn1Error> {
    let (&first, rest) = input.split_first().ok_or(Asn1Error::UnexpectedEof)?;
    *input = rest;

    if first < 0x80 {
        return Ok(first as usize);
    }
    if first == 0x80 {
        return Err(Asn1Error::IndefiniteLength);
    }

    let n = (first & 0x7f) as usize;
    if n > core::mem::size_of::<usize>() {
        return Err(Asn1Error::LengthOverflow);
    }
    if input.len() < n {
        return Err(Asn1Error::UnexpectedEof);
    }
    let (head, rest) = input.split_at(n);
    *input = rest;

    if head[0] == 0 {
        return Err(Asn1Error::NonMinimalLength);
    }
    let mut value: usize = 0;
    for &b in head {
        value = (value << 8) | b as usize;
    }
    if value < 0x80 {
        return Err(Asn1Error::NonMinimalLength);
    }
    Ok(value)
}

// --- Writers (return a complete TLV as bytes) -------------------------------

/// A complete tag-length-value element.
pub fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    out.push(tag);
    encode_length(value.len(), &mut out);
    out.extend_from_slice(value);
    out
}

/// Minimal DER content octets for a non-negative integer (positive two's
/// complement: a leading `0x00` is prepended when the MSB is set).
pub fn integer_value(v: u32) -> Vec<u8> {
    if v == 0 {
        return vec![0x00];
    }
    let be = v.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap();
    let mut bytes = be[first..].to_vec();
    if bytes[0] & 0x80 != 0 {
        bytes.insert(0, 0x00);
    }
    bytes
}

/// `INTEGER` element.
pub fn integer(v: u32) -> Vec<u8> {
    tlv(TAG_INTEGER, &integer_value(v))
}

/// `OCTET STRING` element.
pub fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, bytes)
}

/// `SEQUENCE { .. }` wrapping already-encoded inner elements.
pub fn sequence(inner: &[u8]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, inner)
}

/// EXPLICIT context tag `[n] { inner }`.
pub fn context(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(context_tag(n), inner)
}

// --- Readers (advance a cursor) ---------------------------------------------

/// Read one TLV: returns `(tag, value)` and advances `input` past it.
pub fn read_element<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8]), Asn1Error> {
    let (&tag, rest) = input.split_first().ok_or(Asn1Error::UnexpectedEof)?;
    *input = rest;
    let len = decode_length(input)?;
    if input.len() < len {
        return Err(Asn1Error::UnexpectedEof);
    }
    let (value, rest) = input.split_at(len);
    *input = rest;
    Ok((tag, value))
}

/// Read one TLV and require a specific tag, returning its value.
pub fn expect<'a>(input: &mut &'a [u8], tag: u8) -> Result<&'a [u8], Asn1Error> {
    let (found, value) = read_element(input)?;
    if found != tag {
        return Err(Asn1Error::UnexpectedTag {
            expected: tag,
            found,
        });
    }
    Ok(value)
}

/// Interpret integer content octets as a `u32`.
pub fn read_u32(value: &[u8]) -> Result<u32, Asn1Error> {
    if value.len() > 5 {
        return Err(Asn1Error::LengthOverflow);
    }
    let mut v: u64 = 0;
    for &b in value {
        v = (v << 8) | b as u64;
    }
    if v > u32::MAX as u64 {
        return Err(Asn1Error::LengthOverflow);
    }
    Ok(v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short_and_long_lengths() {
        for len in [
            0usize, 1, 0x7f, 0x80, 0xff, 0x100, 0x1234, 0xffff, 0x1_0000, 0x12_3456,
        ] {
            let mut buf = Vec::new();
            encode_length(len, &mut buf);
            let mut cur: &[u8] = &buf;
            let decoded = decode_length(&mut cur).expect("decode");
            assert_eq!(decoded, len, "roundtrip failed for {len:#x}");
            assert!(cur.is_empty(), "trailing bytes for {len:#x}");
        }
    }

    #[test]
    fn rejects_indefinite_and_non_minimal() {
        assert!(decode_length(&mut (&[0x80][..])).is_err());
        assert!(decode_length(&mut (&[0x81, 0x7f][..])).is_err());
    }

    #[test]
    fn integer_value_is_minimal_and_positive() {
        assert_eq!(integer_value(0), vec![0x00]);
        assert_eq!(integer_value(6), vec![0x06]);
        assert_eq!(integer_value(0x80), vec![0x00, 0x80]); // MSB set → leading zero
        assert_eq!(integer_value(0x1234), vec![0x12, 0x34]);
    }

    #[test]
    fn integer_element_bytes() {
        assert_eq!(integer(6), vec![0x02, 0x01, 0x06]);
    }

    #[test]
    fn context_explicit_wraps_inner() {
        // [0] EXPLICIT INTEGER 6  ->  a0 03 02 01 06
        assert_eq!(context(0, &integer(6)), vec![0xa0, 0x03, 0x02, 0x01, 0x06]);
    }

    #[test]
    fn read_element_walks_a_sequence() {
        let seq = sequence(&[octet_string(b"hi"), integer(1)].concat());
        let mut cur: &[u8] = &seq;
        let body = expect(&mut cur, TAG_SEQUENCE).unwrap();
        let mut b = body;
        let (t0, v0) = read_element(&mut b).unwrap();
        assert_eq!(t0, TAG_OCTET_STRING);
        assert_eq!(v0, b"hi");
        let (t1, v1) = read_element(&mut b).unwrap();
        assert_eq!(t1, TAG_INTEGER);
        assert_eq!(read_u32(v1).unwrap(), 1);
        assert!(b.is_empty());
    }

    #[test]
    fn expect_reports_wrong_tag() {
        let bytes = integer(1);
        let mut cur: &[u8] = &bytes;
        assert!(matches!(
            expect(&mut cur, TAG_OCTET_STRING),
            Err(Asn1Error::UnexpectedTag { .. })
        ));
    }
}
