//! Reader for `RDPIO_WEBRTC_CAPTURE` capture files.
//!
//! The Windows bridge (`rdp-client::webrtc_addin`) can log every logical webrtc.1
//! message, both directions, to a file so the protocol can be studied and — here
//! — replayed offline as a test fixture. This module parses that format.
//!
//! Layout: header `b"WRTC1\0"`, then records, each little-endian:
//! `dir(u8: 'S' inbound server→add-in, 'C' outbound add-in→server)`,
//! `channel_id(u32)`, `seq(u32)`, `t_ms(u32)`, `len(u32)`, `payload[len]`.

/// Direction of a captured message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Server → add-in (the Cloud PC driving the client).
    Inbound,
    /// Add-in → server (the client's responses and events).
    Outbound,
}

/// The capture file's magic header.
pub const MAGIC: &[u8] = b"WRTC1\0";

const DIR_INBOUND: u8 = b'S';
const DIR_OUTBOUND: u8 = b'C';
/// Fixed record header size: dir(1) + channel_id(4) + seq(4) + t_ms(4) + len(4).
const REC_HDR: usize = 17;

/// One captured message.
#[derive(Debug, Clone)]
pub struct CaptureRecord {
    pub dir: Direction,
    pub channel_id: u32,
    pub seq: u32,
    /// Milliseconds since the capture was opened (for A/V timing analysis).
    pub t_ms: u32,
    /// The raw message bytes (still NUL-terminated + slack; run through
    /// [`crate::framing::message_json`] to get the JSON).
    pub payload: Vec<u8>,
}

/// What can go wrong parsing a capture.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("bad capture header: expected WRTC1, got {0:02x?}")]
    BadMagic(Vec<u8>),
    #[error("truncated record at offset {0}")]
    Truncated(usize),
    #[error("unknown direction byte {0:#04x} at offset {1}")]
    BadDirection(u8, usize),
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Parse a whole capture file into its records.
pub fn parse_capture(data: &[u8]) -> Result<Vec<CaptureRecord>, CaptureError> {
    if data.len() < MAGIC.len() || &data[..MAGIC.len()] != MAGIC {
        return Err(CaptureError::BadMagic(
            data.get(..MAGIC.len().min(data.len())).unwrap_or(data).to_vec(),
        ));
    }
    let mut off = MAGIC.len();
    let mut out = Vec::new();
    while off < data.len() {
        if off + REC_HDR > data.len() {
            return Err(CaptureError::Truncated(off));
        }
        let dir = match data[off] {
            DIR_INBOUND => Direction::Inbound,
            DIR_OUTBOUND => Direction::Outbound,
            other => return Err(CaptureError::BadDirection(other, off)),
        };
        let channel_id = u32le(&data[off + 1..]);
        let seq = u32le(&data[off + 5..]);
        let t_ms = u32le(&data[off + 9..]);
        let len = u32le(&data[off + 13..]) as usize;
        off += REC_HDR;
        if off + len > data.len() {
            return Err(CaptureError::Truncated(off));
        }
        out.push(CaptureRecord {
            dir,
            channel_id,
            seq,
            t_ms,
            payload: data[off..off + len].to_vec(),
        });
        off += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(dir: u8, ch: u32, seq: u32, t: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![dir];
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v.extend_from_slice(&t.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn parses_two_records_both_directions() {
        let mut f = MAGIC.to_vec();
        f.extend(rec(b'S', 24, 0, 5, b"{\"a\":1}\x00"));
        f.extend(rec(b'C', 24, 1, 9, b"{\"b\":2}"));
        let recs = parse_capture(&f).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].dir, Direction::Inbound);
        assert_eq!(recs[0].channel_id, 24);
        assert_eq!(recs[1].dir, Direction::Outbound);
        assert_eq!(recs[1].seq, 1);
        assert_eq!(crate::framing::message_json(&recs[0].payload), b"{\"a\":1}");
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(
            parse_capture(b"NOPE..").unwrap_err(),
            CaptureError::BadMagic(_)
        ));
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut f = MAGIC.to_vec();
        f.extend(rec(b'S', 1, 0, 0, b"payload"));
        f.truncate(f.len() - 3); // chop the tail
        assert!(matches!(
            parse_capture(&f).unwrap_err(),
            CaptureError::Truncated(_)
        ));
    }
}
