//! DRDYNVC — the Dynamic Virtual Channel manager (MS-RDPEDYC).
//!
//! DRDYNVC runs over the static `drdynvc` channel and multiplexes dynamic
//! channels, notably the RDPGFX graphics endpoint that carries H.264. Every PDU
//! starts with a one-byte header: `Cmd` (high nibble), `Sp` (bits 3-2), and
//! `Cb` (bits 1-0) which gives the width of the ChannelId field.

/// Create Request / Response.
pub const CMD_CREATE: u8 = 0x01;
/// Fragmented data, first fragment.
pub const CMD_DATA_FIRST: u8 = 0x02;
/// Data.
pub const CMD_DATA: u8 = 0x03;
/// Close.
pub const CMD_CLOSE: u8 = 0x04;
/// Capabilities exchange.
pub const CMD_CAPABILITIES: u8 = 0x05;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DvcError {
    #[error("truncated DRDYNVC PDU")]
    Truncated,
}

/// A decoded inbound DRDYNVC PDU (the subset the client acts on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DvcPdu {
    /// Server capabilities request (we echo the version back).
    CapabilitiesRequest { version: u16 },
    /// Server asks to open a dynamic channel by name.
    CreateRequest { channel_id: u32, name: String },
    /// First fragment of a fragmented message (`total_length` = reassembled size).
    DataFirst {
        channel_id: u32,
        total_length: u32,
        payload: Vec<u8>,
    },
    /// Channel data (e.g. an RDPGFX payload).
    Data { channel_id: u32, payload: Vec<u8> },
    /// Channel close.
    Close { channel_id: u32 },
    /// A command we don't specifically handle yet.
    Other { cmd: u8 },
}

fn read_channel_id(cb: u8, data: &[u8]) -> Option<(u32, usize)> {
    match cb {
        0 => data.first().map(|&b| (b as u32, 1)),
        1 => (data.len() >= 2).then(|| (u16::from_le_bytes([data[0], data[1]]) as u32, 2)),
        2 => {
            (data.len() >= 4).then(|| (u32::from_le_bytes([data[0], data[1], data[2], data[3]]), 4))
        }
        _ => None,
    }
}

/// Read the DATA_FIRST `Length` field, whose width is given by the header `Sp`.
fn read_length(sp: u8, data: &[u8]) -> Option<(u32, usize)> {
    match sp {
        0 => data.first().map(|&b| (b as u32, 1)),
        1 => (data.len() >= 2).then(|| (u16::from_le_bytes([data[0], data[1]]) as u32, 2)),
        2 => {
            (data.len() >= 4).then(|| (u32::from_le_bytes([data[0], data[1], data[2], data[3]]), 4))
        }
        _ => None,
    }
}

/// Append `id` using the narrowest encoding, returning the `Cb` selector.
fn write_channel_id(id: u32, out: &mut Vec<u8>) -> u8 {
    if id <= 0xff {
        out.push(id as u8);
        0
    } else if id <= 0xffff {
        out.extend_from_slice(&(id as u16).to_le_bytes());
        1
    } else {
        out.extend_from_slice(&id.to_le_bytes());
        2
    }
}

/// Parse one inbound DRDYNVC PDU.
pub fn parse(data: &[u8]) -> Result<DvcPdu, DvcError> {
    let (&first, rest) = data.split_first().ok_or(DvcError::Truncated)?;
    let cmd = first >> 4;
    let cb = first & 0x03;

    match cmd {
        CMD_CAPABILITIES => {
            // Pad(1) + Version(2 LE); no ChannelId for the caps PDU.
            if rest.len() < 3 {
                return Err(DvcError::Truncated);
            }
            Ok(DvcPdu::CapabilitiesRequest {
                version: u16::from_le_bytes([rest[1], rest[2]]),
            })
        }
        CMD_CREATE => {
            let (channel_id, n) = read_channel_id(cb, rest).ok_or(DvcError::Truncated)?;
            let name_bytes = &rest[n..];
            let end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            Ok(DvcPdu::CreateRequest {
                channel_id,
                name: String::from_utf8_lossy(&name_bytes[..end]).into_owned(),
            })
        }
        CMD_DATA_FIRST => {
            let sp = (first >> 2) & 0x03;
            let (channel_id, n) = read_channel_id(cb, rest).ok_or(DvcError::Truncated)?;
            let after_id = &rest[n..];
            let (total_length, m) = read_length(sp, after_id).ok_or(DvcError::Truncated)?;
            Ok(DvcPdu::DataFirst {
                channel_id,
                total_length,
                payload: after_id[m..].to_vec(),
            })
        }
        CMD_DATA => {
            let (channel_id, n) = read_channel_id(cb, rest).ok_or(DvcError::Truncated)?;
            Ok(DvcPdu::Data {
                channel_id,
                payload: rest[n..].to_vec(),
            })
        }
        CMD_CLOSE => {
            let (channel_id, _) = read_channel_id(cb, rest).ok_or(DvcError::Truncated)?;
            Ok(DvcPdu::Close { channel_id })
        }
        other => Ok(DvcPdu::Other { cmd: other }),
    }
}

/// Client Capabilities Response echoing the negotiated `version`.
pub fn capabilities_response(version: u16) -> Vec<u8> {
    let mut out = vec![CMD_CAPABILITIES << 4, 0x00]; // header, pad
    out.extend_from_slice(&version.to_le_bytes());
    out
}

/// Client Create Response (`status` 0 = success / channel accepted).
pub fn create_response(channel_id: u32, status: u32) -> Vec<u8> {
    let mut tail = Vec::new();
    let cb = write_channel_id(channel_id, &mut tail);
    let mut out = vec![(CMD_CREATE << 4) | cb];
    out.extend_from_slice(&tail);
    out.extend_from_slice(&status.to_le_bytes());
    out
}

/// Wrap `payload` as a DVC Data PDU for `channel_id`.
pub fn data(channel_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut tail = Vec::new();
    let cb = write_channel_id(channel_id, &mut tail);
    let mut out = vec![(CMD_DATA << 4) | cb];
    out.extend_from_slice(&tail);
    out.extend_from_slice(payload);
    out
}

/// Max channel payload per DVC data PDU. DVC PDUs ride inside the static
/// `drdynvc` SVC (chunk length ~1600 B); larger messages must be split into a
/// `DATA_FIRST` (carrying the reassembled total) + `DATA` fragments. Kept well
/// under 1600 to leave room for the DVC + SVC headers.
pub const MAX_DATA_CHUNK: usize = 1500;

/// Append `len` using the narrowest width, returning the `Sp` selector (mirror of
/// [`read_length`]).
fn write_length(len: u32, out: &mut Vec<u8>) -> u8 {
    if len <= 0xff {
        out.push(len as u8);
        0
    } else if len <= 0xffff {
        out.extend_from_slice(&(len as u16).to_le_bytes());
        1
    } else {
        out.extend_from_slice(&len.to_le_bytes());
        2
    }
}

/// Wrap `payload` as one or more DVC PDUs for `channel_id`, fragmenting into a
/// `DATA_FIRST` + `DATA` sequence when it exceeds [`MAX_DATA_CHUNK`] (as a hosted
/// add-in's SDP/candidate messages routinely do). A payload that fits is a single
/// `DATA` PDU identical to [`data`].
pub fn data_message(channel_id: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    if payload.len() <= MAX_DATA_CHUNK {
        return vec![data(channel_id, payload)];
    }
    let mut out = Vec::new();
    let mut chunks = payload.chunks(MAX_DATA_CHUNK);
    // First fragment: DATA_FIRST announcing the full reassembled length.
    let first = chunks.next().unwrap_or(&[]);
    let mut tail = Vec::new();
    let cb = write_channel_id(channel_id, &mut tail);
    let mut len_bytes = Vec::new();
    let sp = write_length(payload.len() as u32, &mut len_bytes);
    let mut pdu = vec![(CMD_DATA_FIRST << 4) | (sp << 2) | cb];
    pdu.extend_from_slice(&tail);
    pdu.extend_from_slice(&len_bytes);
    pdu.extend_from_slice(first);
    out.push(pdu);
    // Remaining fragments as plain DATA PDUs.
    for chunk in chunks {
        out.push(data(channel_id, chunk));
    }
    out
}

/// Reassembles fragmented dynamic-channel messages. A `DataFirst` announces the
/// total size and starts buffering; subsequent `Data` PDUs append until the
/// message is complete. Standalone `Data` PDUs (no preceding `DataFirst`) are
/// complete messages on their own.
#[derive(Default)]
pub struct Reassembler {
    pending: std::collections::HashMap<u32, (usize, Vec<u8>)>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a parsed PDU; returns `(channel_id, message)` once a full message is
    /// assembled, otherwise `None`.
    pub fn accept(&mut self, pdu: DvcPdu) -> Option<(u32, Vec<u8>)> {
        match pdu {
            DvcPdu::DataFirst {
                channel_id,
                total_length,
                payload,
            } => {
                let total = total_length as usize;
                if payload.len() >= total {
                    Some((channel_id, payload))
                } else {
                    self.pending.insert(channel_id, (total, payload));
                    None
                }
            }
            DvcPdu::Data {
                channel_id,
                payload,
            } => {
                if let Some((total, mut buf)) = self.pending.remove(&channel_id) {
                    buf.extend_from_slice(&payload);
                    if buf.len() >= total {
                        Some((channel_id, buf))
                    } else {
                        self.pending.insert(channel_id, (total, buf));
                        None
                    }
                } else {
                    Some((channel_id, payload))
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capabilities_request() {
        assert_eq!(
            parse(&[0x50, 0x00, 0x01, 0x00]).unwrap(),
            DvcPdu::CapabilitiesRequest { version: 1 }
        );
        assert_eq!(capabilities_response(1), vec![0x50, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn parse_create_request_with_graphics_name() {
        let name = super::super::names::GRAPHICS;
        let mut pdu = vec![0x10, 0x03]; // CREATE, cb=0, channelId=3
        pdu.extend_from_slice(name.as_bytes());
        pdu.push(0x00);
        assert_eq!(
            parse(&pdu).unwrap(),
            DvcPdu::CreateRequest {
                channel_id: 3,
                name: name.to_string(),
            }
        );
    }

    #[test]
    fn create_response_success() {
        assert_eq!(
            create_response(3, 0),
            vec![0x10, 0x03, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn data_roundtrip() {
        let pdu = data(3, &[0xAA, 0xBB]);
        assert_eq!(pdu, vec![0x30, 0x03, 0xAA, 0xBB]);
        assert_eq!(
            parse(&pdu).unwrap(),
            DvcPdu::Data {
                channel_id: 3,
                payload: vec![0xAA, 0xBB],
            }
        );
    }

    #[test]
    fn two_byte_channel_id_roundtrips() {
        let pdu = data(0x1234, &[0x01]);
        assert_eq!(pdu[0] & 0x03, 1); // Cb = 1 (2-byte id)
        assert_eq!(
            parse(&pdu).unwrap(),
            DvcPdu::Data {
                channel_id: 0x1234,
                payload: vec![0x01],
            }
        );
    }

    #[test]
    fn parse_data_first_with_length() {
        // DATA_FIRST, cb=0 (1-byte id), sp=0 (1-byte length): header 0x20.
        // channelId=3, length=5, first 2 bytes of payload.
        let pdu = [0x20, 0x03, 0x05, 0xAA, 0xBB];
        assert_eq!(
            parse(&pdu).unwrap(),
            DvcPdu::DataFirst {
                channel_id: 3,
                total_length: 5,
                payload: vec![0xAA, 0xBB],
            }
        );
    }

    #[test]
    fn data_message_fragments_and_reassembles() {
        // A payload larger than one chunk must split into DATA_FIRST + DATA and
        // reassemble byte-identically through the peer's Reassembler.
        let payload: Vec<u8> = (0..(MAX_DATA_CHUNK * 2 + 7) as u32).map(|i| i as u8).collect();
        let pdus = data_message(9, &payload);
        assert!(pdus.len() >= 3, "expected fragmentation, got {}", pdus.len());
        assert_eq!(pdus[0][0] >> 4, CMD_DATA_FIRST);
        let mut r = Reassembler::new();
        let mut done = None;
        for pdu in &pdus {
            done = r.accept(parse(pdu).unwrap());
        }
        assert_eq!(done, Some((9, payload)));
    }

    #[test]
    fn data_message_small_is_single_data_pdu() {
        let pdus = data_message(3, &[0xAA, 0xBB]);
        assert_eq!(pdus, vec![data(3, &[0xAA, 0xBB])]);
    }

    #[test]
    fn reassembles_fragmented_message() {
        let mut r = Reassembler::new();
        // DATA_FIRST: total 5, first 2 bytes.
        let first = parse(&[0x20, 0x03, 0x05, 0xAA, 0xBB]).unwrap();
        assert_eq!(r.accept(first), None);
        // DATA: next 2 bytes — still incomplete (4/5).
        assert_eq!(r.accept(parse(&data(3, &[0xCC, 0xDD])).unwrap()), None);
        // DATA: final byte — completes the 5-byte message.
        assert_eq!(
            r.accept(parse(&data(3, &[0xEE])).unwrap()),
            Some((3, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]))
        );
    }

    #[test]
    fn standalone_data_is_complete() {
        let mut r = Reassembler::new();
        assert_eq!(
            r.accept(DvcPdu::Data {
                channel_id: 7,
                payload: vec![1, 2, 3],
            }),
            Some((7, vec![1, 2, 3]))
        );
    }
}
