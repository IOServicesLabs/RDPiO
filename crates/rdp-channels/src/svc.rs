//! Static virtual channel chunking (MS-RDPBCGR 2.2.6.1).
//!
//! Data on a statically-joined virtual channel (cliprdr, rdpsnd, rdpdr, …) is
//! framed with a `CHANNEL_PDU_HEADER` { length, flags } and split into ≤1600-byte
//! chunks, the first flagged `CHANNEL_FLAG_FIRST` and the last `CHANNEL_FLAG_LAST`
//! (`length` is the total across chunks). This module builds outbound chunks and
//! reassembles inbound ones; it's sans-I/O so the session layer owns the socket.

/// First chunk of a channel PDU.
pub const CHANNEL_FLAG_FIRST: u32 = 0x0000_0001;
/// Last chunk of a channel PDU.
pub const CHANNEL_FLAG_LAST: u32 = 0x0000_0002;
/// The channel's data is delivered to the endpoint with its PDU header intact.
pub const CHANNEL_FLAG_SHOW_PROTOCOL: u32 = 0x0000_0010;

/// Maximum bytes of channel data per chunk (the conventional VC chunk size).
const CHUNK_SIZE: usize = 1600;

/// Frame `data` as one or more static-VC chunks for a channel opened with
/// `CHANNEL_OPTION_SHOW_PROTOCOL` (cliprdr, rdpsnd, rdpdr): each chunk carries
/// `CHANNEL_FLAG_SHOW_PROTOCOL` so the server keeps the `CHANNEL_PDU_HEADER`
/// intact for the endpoint.
pub fn chunks(data: &[u8]) -> Vec<Vec<u8>> {
    chunks_with(data, CHANNEL_FLAG_SHOW_PROTOCOL)
}

/// Frame `data` for a channel *not* opened with `CHANNEL_OPTION_SHOW_PROTOCOL`
/// — notably `drdynvc`. The `CHANNEL_FLAG_SHOW_PROTOCOL` bit is omitted, so the
/// server's channel manager strips the `CHANNEL_PDU_HEADER` before handing the
/// payload to the DVC manager (which expects a bare DVC PDU). Sending DVC data
/// with the SHOW_PROTOCOL flag — or with no header at all — makes the server
/// report ERRINFO_VCHANNELDATATOOSHORT and drop the connection.
pub fn chunks_dvc(data: &[u8]) -> Vec<Vec<u8>> {
    chunks_with(data, 0)
}

/// Frame `data` into static-VC chunks (each prefixed with a `CHANNEL_PDU_HEADER`),
/// OR-ing `base_flags` into every chunk's flags. The `length` field of every
/// chunk is the *total* length; `FIRST`/`LAST` flags bound the sequence.
fn chunks_with(data: &[u8], base_flags: u32) -> Vec<Vec<u8>> {
    let total = data.len() as u32;
    let mut out = Vec::new();
    let mut off = 0;
    loop {
        let end = (off + CHUNK_SIZE).min(data.len());
        let mut flags = base_flags;
        if off == 0 {
            flags |= CHANNEL_FLAG_FIRST;
        }
        if end == data.len() {
            flags |= CHANNEL_FLAG_LAST;
        }
        let mut chunk = Vec::with_capacity(8 + (end - off));
        chunk.extend_from_slice(&total.to_le_bytes());
        chunk.extend_from_slice(&flags.to_le_bytes());
        chunk.extend_from_slice(&data[off..end]);
        out.push(chunk);
        off = end;
        // Stop once we've emitted the LAST chunk (handles the empty-data case,
        // which produces a single FIRST|LAST chunk with no payload).
        if off >= data.len() {
            break;
        }
    }
    out
}

/// Reassembles inbound static-VC chunks into whole channel messages.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound chunk (`CHANNEL_PDU_HEADER` + data). Returns the complete
    /// channel message once `CHANNEL_FLAG_LAST` is seen, else `None`.
    pub fn push(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if chunk.len() < 8 {
            return None;
        }
        let flags = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        if flags & CHANNEL_FLAG_FIRST != 0 {
            self.buf.clear();
        }
        self.buf.extend_from_slice(&chunk[8..]);
        if flags & CHANNEL_FLAG_LAST != 0 {
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payload_is_one_first_last_chunk() {
        let cs = chunks(b"hello");
        assert_eq!(cs.len(), 1);
        let flags = u32::from_le_bytes([cs[0][4], cs[0][5], cs[0][6], cs[0][7]]);
        assert_eq!(flags & CHANNEL_FLAG_FIRST, CHANNEL_FLAG_FIRST);
        assert_eq!(flags & CHANNEL_FLAG_LAST, CHANNEL_FLAG_LAST);
        assert_eq!(u32::from_le_bytes([cs[0][0], cs[0][1], cs[0][2], cs[0][3]]), 5);
        assert_eq!(&cs[0][8..], b"hello");
    }

    #[test]
    fn empty_payload_still_emits_one_chunk() {
        let cs = chunks(b"");
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].len(), 8); // header only
    }

    #[test]
    fn dvc_chunks_omit_show_protocol() {
        // drdynvc isn't opened with CHANNEL_OPTION_SHOW_PROTOCOL, so its chunks
        // must carry FIRST|LAST but NOT SHOW_PROTOCOL.
        let cs = chunks_dvc(b"hi");
        assert_eq!(cs.len(), 1);
        let flags = u32::from_le_bytes([cs[0][4], cs[0][5], cs[0][6], cs[0][7]]);
        assert_eq!(flags & CHANNEL_FLAG_FIRST, CHANNEL_FLAG_FIRST);
        assert_eq!(flags & CHANNEL_FLAG_LAST, CHANNEL_FLAG_LAST);
        assert_eq!(flags & CHANNEL_FLAG_SHOW_PROTOCOL, 0);
        // The reassembler still round-trips DVC chunks.
        let mut r = Reassembler::new();
        assert_eq!(r.push(&cs[0]), Some(b"hi".to_vec()));
    }

    #[test]
    fn large_payload_chunks_and_reassembles() {
        let data: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        let cs = chunks(&data);
        assert_eq!(cs.len(), 3); // 1600 + 1600 + 800
        // First chunk FIRST-but-not-LAST; last chunk LAST-but-not-FIRST.
        let f0 = u32::from_le_bytes([cs[0][4], cs[0][5], cs[0][6], cs[0][7]]);
        assert_eq!(f0 & CHANNEL_FLAG_FIRST, CHANNEL_FLAG_FIRST);
        assert_eq!(f0 & CHANNEL_FLAG_LAST, 0);
        let mut r = Reassembler::new();
        let mut done = None;
        for c in &cs {
            done = r.push(c);
        }
        assert_eq!(done, Some(data));
    }
}
