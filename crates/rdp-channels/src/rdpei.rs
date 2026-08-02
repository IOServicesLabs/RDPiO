//! Remote Desktop Protocol: Input Virtual Channel Extension (MS-RDPEI).
//!
//! RDPEI carries multi-touch (and pen) input from client to server over a
//! dynamic virtual channel named `Microsoft::Windows::RDS::Input`. This module
//! implements the initializing phase (server-ready / client-ready handshake) and
//! the minimal touch-frame format needed for basic multi-touch support.
//!
//! The module is sans-I/O: it serializes PDUs but does not read from or write
//! to the channel directly; the DVC manager in [`crate::channel`] wraps the
//! payloads produced here.

/// The dynamic channel name for the RDPEI touch/pen input channel.
pub const RDPINPUT_CHANNEL: &str = "Microsoft::Windows::RDS::Input";

const EVENTID_SC_READY: u16 = 0x0001;
const EVENTID_CS_READY: u16 = 0x0002;
const EVENTID_TOUCH: u16 = 0x0003;

#[cfg(test)]
const PROTOCOL_V100: u32 = 0x0001_0000;
const PROTOCOL_V101: u32 = 0x0001_0001;
// const PROTOCOL_V200: u32 = 0x0002_0000;

const CS_READY_FLAGS_DISABLE_TIMESTAMP_INJECTION: u32 = 0x0000_0002;

/// Contact state flags for an [`RdpInputContact`].
pub const CONTACT_FLAG_DOWN: u32 = 0x0001;
pub const CONTACT_FLAG_UPDATE: u32 = 0x0002;
pub const CONTACT_FLAG_UP: u32 = 0x0004;
pub const CONTACT_FLAG_INRANGE: u32 = 0x0008;
pub const CONTACT_FLAG_INCONTACT: u32 = 0x0010;
pub const CONTACT_FLAG_CANCELED: u32 = 0x0020;

/// One touch contact in virtual-desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdpInputContact {
    /// Contact identifier (0-255). Windows assigns a stable ID per touch point.
    pub id: u8,
    /// X coordinate in virtual-desktop pixels.
    pub x: i32,
    /// Y coordinate in virtual-desktop pixels.
    pub y: i32,
    /// A combination of `CONTACT_FLAG_*` values.
    pub flags: u32,
}

/// State machine for the RDPEI channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RdpInputState {
    /// No channel open yet, or the handshake has not completed.
    #[default]
    Idle,
    /// The server has sent its ready PDU and the client has replied.
    Ready(u32),
}

/// Minimal RDPEI encoder / state machine.
#[derive(Debug, Default)]
pub struct RdpInputChannel {
    state: RdpInputState,
}

impl RdpInputChannel {
    pub fn new() -> Self {
        Self {
            state: RdpInputState::Idle,
        }
    }

    pub fn state(&self) -> RdpInputState {
        self.state
    }

    /// Process a server payload received on the RDPEI channel. Returns the
    /// client response that must be sent back on the same channel, if any.
    pub fn process_server_payload(&mut self, channel_id: u32, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 6 {
            return None;
        }
        let event_id = u16::from_le_bytes([payload[0], payload[1]]);
        // let pdu_length = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);

        match event_id {
            EVENTID_SC_READY => {
                // V100 handshake: header + 4-byte protocolVersion. V300 may add
                // supportedFeatures, but we answer with V100 and ignore extras.
                if payload.len() >= 10 {
                    // let protocol_version = u32::from_le_bytes([payload[6], payload[7], payload[8], payload[9]]);
                    self.state = RdpInputState::Ready(channel_id);
                    tracing::info!(
                        channel_id,
                        "rdpei: server ready; sending client ready (multi-touch enabled)"
                    );
                    Some(client_ready_pdu())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Build a touch-event PDU carrying `contacts`, split into temporal
    /// frames. The caller (the DVC manager) wraps the returned bytes with the
    /// DRDYNVC data header.
    ///
    /// A frame is a *simultaneous* snapshot, so a contact id may appear at
    /// most once per frame — but the caller's queue is temporal: under load
    /// it batches sequential events of the same finger (down + first moves).
    /// Encoding those into one frame is malformed; a server that rejects it
    /// never registers the finger-down, which then invalidates every later
    /// event for that contact (observed as completely dead touch). Contacts
    /// are therefore packed greedily into frames such that each id appears
    /// once per frame, preserving order.
    pub fn touch_event(&self, contacts: &[RdpInputContact]) -> Option<Vec<u8>> {
        if contacts.is_empty() {
            return None;
        }
        let mut frames: Vec<Vec<RdpInputContact>> = Vec::new();
        for c in contacts {
            match frames.last_mut() {
                Some(f) if !f.iter().any(|e| e.id == c.id) => f.push(*c),
                _ => frames.push(vec![*c]),
            }
        }
        let mut body = Vec::with_capacity(6 + 16 + contacts.len() * 16);
        body.extend_from_slice(&EVENTID_TOUCH.to_le_bytes());
        // pduLength is filled in below.
        let length_offset = body.len();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&write_vu32(0)); // encodeTime (timestamp injection disabled)
        body.extend_from_slice(&write_vu16(frames.len() as u16)); // frameCount
        for (i, f) in frames.iter().enumerate() {
            // frameOffset: first frame 0; later frames a nominal 1 ms after
            // the previous (100-microsecond units), keeping same-contact
            // events distinct in time for the server-side injector.
            body.extend_from_slice(&touch_frame(f, if i == 0 { 0 } else { 10 }));
        }
        let pdu_length = body.len() as u32;
        body[length_offset..length_offset + 4].copy_from_slice(&pdu_length.to_le_bytes());
        Some(body)
    }
}

fn client_ready_pdu() -> Vec<u8> {
    let mut body = Vec::with_capacity(6 + 4 + 4 + 2);
    body.extend_from_slice(&EVENTID_CS_READY.to_le_bytes());
    body.extend_from_slice(&((6 + 4 + 4 + 2) as u32).to_le_bytes());
    body.extend_from_slice(&CS_READY_FLAGS_DISABLE_TIMESTAMP_INJECTION.to_le_bytes());
    // V101, not V100: DISABLE_TIMESTAMP_INJECTION is only defined from V101
    // on, and a server validating the pair can junk the whole client-ready —
    // after which every touch frame is silently discarded. The touch wire
    // format is identical across the two versions.
    body.extend_from_slice(&PROTOCOL_V101.to_le_bytes());
    // 10, matching what real clients (and real digitizers) advertise. The
    // earlier 256 was within the field's range but beyond anything a server
    // expects to see, and one more excuse for a validator to junk the PDU.
    body.extend_from_slice(&10u16.to_le_bytes()); // maxTouchContacts
    body
}

fn touch_frame(contacts: &[RdpInputContact], frame_offset: u64) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + contacts.len() * 20);
    frame.extend_from_slice(&write_vu16(contacts.len() as u16)); // contactCount
    frame.extend_from_slice(&write_vu64(frame_offset)); // offset from previous frame, 100us units
    for c in contacts {
        frame.push(c.id);
        frame.extend_from_slice(&write_vu16(0)); // fieldsPresent: no rect/orientation/pressure
        // x/y are FOUR_BYTE_SIGNED_INTEGER on the wire: the sign bit lives in
        // bit 5 of the first byte and the first byte carries only 5 value
        // bits, so the unsigned encoding diverges from coordinate 0x2000 up.
        frame.extend_from_slice(&write_vi32(c.x));
        frame.extend_from_slice(&write_vi32(c.y));
        frame.extend_from_slice(&write_vu32(c.flags));
    }
    frame
}

/// Variable-length unsigned integer, 1-2 bytes (TWO_BYTE_UNSIGNED_INTEGER).
///
/// Encoding follows MS-RDPBCGR 2.2.2.1.1: the high two bits of the first byte
/// select the length class and the remaining bits hold the most-significant
/// bits of the value; subsequent bytes are the rest of the value in
/// big-endian order.
fn write_vu16(value: u16) -> Vec<u8> {
    if value <= 0x3F {
        vec![value as u8]
    } else {
        debug_assert!(value <= 0x3FFF);
        vec![0x40 | ((value >> 8) & 0x3F) as u8, (value & 0xFF) as u8]
    }
}

/// Variable-length signed integer, 1-4 bytes (FOUR_BYTE_SIGNED_INTEGER).
///
/// First byte: 2-bit length class, then the sign bit (0x20), then the top 5
/// bits of the magnitude; remaining bytes are the magnitude in big-endian
/// order. Magnitudes beyond 29 bits are clamped (RDPEI coordinates never
/// approach that).
fn write_vi32(value: i32) -> Vec<u8> {
    let sign = if value < 0 { 0x20u8 } else { 0 };
    let v = value.unsigned_abs();
    if v <= 0x1F {
        vec![sign | v as u8]
    } else if v <= 0x1FFF {
        vec![0x40 | sign | ((v >> 8) & 0x1F) as u8, (v & 0xFF) as u8]
    } else if v <= 0x1F_FFFF {
        vec![
            0x80 | sign | ((v >> 16) & 0x1F) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ]
    } else {
        let v = v.min(0x1FFF_FFFF);
        vec![
            0xC0 | sign | ((v >> 24) & 0x1F) as u8,
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ]
    }
}

/// Variable-length unsigned integer, 1-4 bytes (FOUR_BYTE_UNSIGNED_INTEGER).
fn write_vu32(value: u32) -> Vec<u8> {
    if value <= 0x3F {
        vec![value as u8]
    } else if value <= 0x3FFF {
        vec![0x40 | ((value >> 8) & 0x3F) as u8, (value & 0xFF) as u8]
    } else if value <= 0x3F_FFFF {
        vec![
            0x80 | ((value >> 16) & 0x3F) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ]
    } else if value <= 0x3FFF_FFFF {
        vec![
            0xC0 | ((value >> 24) & 0x3F) as u8,
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ]
    } else {
        // Value does not fit in 30 bits; clamp to the maximum representable value.
        vec![0xC0 | 0x3F, 0xFF, 0xFF, 0xFF]
    }
}

/// Variable-length unsigned integer, 1-8 bytes (EIGHT_BYTE_UNSIGNED_INTEGER).
///
/// RDPEI only uses this for `frameOffset`; in practice the value is 0. Values
/// beyond 30 bits are clamped because the RDP variable-length format only
/// encodes 30 bits in its four-byte form.
fn write_vu64(value: u64) -> Vec<u8> {
    if value <= 0x3F {
        vec![value as u8]
    } else if value <= 0x3FFF {
        vec![0x40 | ((value >> 8) & 0x3F) as u8, (value & 0xFF) as u8]
    } else if value <= 0x3F_FFFF {
        vec![
            0x80 | ((value >> 16) & 0x3F) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ]
    } else if value <= 0x3FFF_FFFF {
        vec![
            0xC0 | ((value >> 24) & 0x3F) as u8,
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ]
    } else {
        vec![0xC0 | 0x3F, 0xFF, 0xFF, 0xFF]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_length_u16() {
        assert_eq!(write_vu16(0x00), vec![0x00]);
        assert_eq!(write_vu16(0x3F), vec![0x3F]);
        // Two-byte form: top 2 bits = 01, low 6 bits = high 6 bits of value.
        assert_eq!(write_vu16(0x40), vec![0x40, 0x40]);
        assert_eq!(write_vu16(0x1234), vec![0x52, 0x34]);
    }

    #[test]
    fn variable_length_u32() {
        assert_eq!(write_vu32(0), vec![0]);
        assert_eq!(write_vu32(0x3F), vec![0x3F]);
        assert_eq!(write_vu32(0x40), vec![0x40, 0x40]);
        // 0x001A1B1C -> {0x9A, 0x1B, 0x1C} per MS-RDPEGDI example.
        assert_eq!(write_vu32(0x001A_1B1C), vec![0x9A, 0x1B, 0x1C]);
        // 0x12345678 -> four-byte form.
        assert_eq!(write_vu32(0x1234_5678), vec![0xD2, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn client_ready_well_formed() {
        let pdu = client_ready_pdu();
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]), EVENTID_CS_READY);
        assert_eq!(u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]) as usize, pdu.len());
        assert_eq!(pdu.len(), 16);
        // DISABLE_TIMESTAMP_INJECTION is only defined from V101 on; the pair
        // must stay consistent or servers may junk the whole client-ready.
        let version = u32::from_le_bytes([pdu[10], pdu[11], pdu[12], pdu[13]]);
        assert_eq!(version, PROTOCOL_V101);
    }

    #[test]
    fn variable_length_i32() {
        // One byte: 5 value bits + sign.
        assert_eq!(write_vi32(0), vec![0x00]);
        assert_eq!(write_vi32(0x1F), vec![0x1F]);
        assert_eq!(write_vi32(-1), vec![0x21]);
        // Two bytes from 0x20 (the unsigned form switches at 0x40).
        assert_eq!(write_vi32(0x20), vec![0x40, 0x20]);
        assert_eq!(write_vi32(-0x20), vec![0x60, 0x20]);
        // 8192 (0x2000) is where a signed value no longer fits two bytes —
        // the old unsigned encoding kept it in two and corrupted the stream.
        assert_eq!(write_vi32(0x1FFF), vec![0x5F, 0xFF]);
        assert_eq!(write_vi32(0x2000), vec![0x80, 0x20, 0x00]);
        assert_eq!(write_vi32(-0x2000), vec![0xA0, 0x20, 0x00]);
        // Four-byte form.
        assert_eq!(write_vi32(0x0200_0000), vec![0xC2, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn touch_frame_encodes_signed_coordinates() {
        // A coordinate past 0x1FFF must take the three-byte signed form.
        let contacts = [RdpInputContact {
            id: 0,
            x: 0x2000,
            y: 3,
            flags: CONTACT_FLAG_UPDATE | CONTACT_FLAG_INCONTACT | CONTACT_FLAG_INRANGE,
        }];
        let frame = touch_frame(&contacts, 0);
        // contactCount(1) + frameOffset(1) + id(1) + fieldsPresent(1)
        //   + x(3) + y(1) + flags(1)
        assert_eq!(frame.len(), 9);
        assert_eq!(&frame[4..7], &[0x80, 0x20, 0x00]);
    }

    #[test]
    fn touch_event_splits_same_contact_into_frames() {
        // A down + move of the SAME finger batched into one call (worker lag)
        // must become two frames — one frame may hold a contact id only once.
        let ch = RdpInputChannel::new();
        let down = RdpInputContact {
            id: 0,
            x: 10,
            y: 10,
            flags: CONTACT_FLAG_DOWN | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
        };
        let mv = RdpInputContact {
            id: 0,
            x: 12,
            y: 14,
            flags: CONTACT_FLAG_UPDATE | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
        };
        let pdu = ch.touch_event(&[down, mv]).unwrap();
        assert_eq!(pdu[6], 0); // encodeTime
        assert_eq!(pdu[7], 2); // frameCount == 2
        // Frame 1: contactCount=1, frameOffset=0, then the down contact.
        assert_eq!(pdu[8], 1);
        assert_eq!(pdu[9], 0);
        // Frame 2 begins after frame 1 (1+1+1+1+1+1+1 = 7 bytes for small
        // coords): contactCount=1, frameOffset=10 (1 ms).
        assert_eq!(pdu[15], 1);
        assert_eq!(pdu[16], 10);
        assert_eq!(u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]) as usize, pdu.len());
    }

    #[test]
    fn touch_event_keeps_simultaneous_contacts_in_one_frame() {
        // Two DIFFERENT fingers (pinch) stay in a single frame.
        let ch = RdpInputChannel::new();
        let mk = |id: u8| RdpInputContact {
            id,
            x: 10,
            y: 10,
            flags: CONTACT_FLAG_DOWN | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
        };
        let pdu = ch.touch_event(&[mk(0), mk(1)]).unwrap();
        assert_eq!(pdu[7], 1); // frameCount == 1
        assert_eq!(pdu[8], 2); // contactCount == 2
    }

    #[test]
    fn handshake_advances_to_ready() {
        let mut ch = RdpInputChannel::new();
        let mut sc = vec![0u8; 10];
        sc[0..2].copy_from_slice(&EVENTID_SC_READY.to_le_bytes());
        sc[2..6].copy_from_slice(&10u32.to_le_bytes());
        sc[6..10].copy_from_slice(&PROTOCOL_V100.to_le_bytes());
        assert!(ch.process_server_payload(7, &sc).is_some());
        assert_eq!(ch.state(), RdpInputState::Ready(7));
    }

    #[test]
    fn touch_event_has_one_frame() {
        let ch = RdpInputChannel::new();
        let contacts = [RdpInputContact {
            id: 1,
            x: 100,
            y: 200,
            flags: CONTACT_FLAG_DOWN | CONTACT_FLAG_INCONTACT | CONTACT_FLAG_INRANGE,
        }];
        let pdu = ch.touch_event(&contacts).unwrap();
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]), EVENTID_TOUCH);
        assert_eq!(u32::from_le_bytes([pdu[2], pdu[3], pdu[4], pdu[5]]) as usize, pdu.len());
        // After the 6-byte header: encodeTime (vu32) == 0, frameCount (vu16) == 1.
        assert_eq!(pdu[6], 0);
        assert_eq!(pdu[7], 1);
    }
}
