//! RDPEMT — the multitransport tunnel (MS-RDPEMT) that rides on an RDP-UDP
//! connection and carries the side-band graphics traffic.
//!
//! Once the RDP-UDP handshake ([`crate::drdynvc`]'s sibling [`rdp_pdu::rdpudp`])
//! is up and a TLS session has been negotiated over the reliable channel, the
//! client sends a **Tunnel Create Request** echoing the `requestId` +
//! 16-byte security cookie from the server's multitransport request; the server
//! replies a **Tunnel Create Response** with an `HRESULT`. Thereafter higher
//! layers (RDPGFX) flow as **Tunnel Data** PDUs.
//!
//! Each PDU starts with an `RDP_TUNNEL_HEADER`: an action/flags byte, a 16-bit
//! payload length, and a 1-byte header length (`0x04` with no subheaders). This
//! module is the sans-I/O codec for those PDUs; the TLS + socket work lives in
//! the client driver and only runs on the `--udp` path (TCP otherwise).

/// `RDPTUNNEL_ACTION_*` — the high-level PDU kind (low nibble of byte 0).
pub const ACTION_CREATE_REQUEST: u8 = 0x00;
pub const ACTION_CREATE_RESPONSE: u8 = 0x01;
pub const ACTION_DATA: u8 = 0x02;

/// Header length when there are no tunnel subheaders.
const HEADER_LEN: u8 = 0x04;

/// A parsed RDPEMT PDU: its action and the higher-layer payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelPdu {
    pub action: u8,
    pub payload: Vec<u8>,
}

/// Frame an RDPEMT PDU: `RDP_TUNNEL_HEADER` + `payload`.
fn frame(action: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.push(action & 0x0F); // Action (low nibble); Flags (high nibble) = 0
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes()); // PayloadLength
    out.push(HEADER_LEN); // HeaderLength (no subheaders)
    out.extend_from_slice(payload);
    out
}

/// Parse an RDPEMT PDU, honoring `HeaderLength` so any subheaders are skipped.
///
/// Lenient: if the buffer is shorter than `PayloadLength` promises, the payload
/// is clamped to what's present. Use [`take_pdu`] on a byte stream where PDUs
/// can span reads — it refuses to frame a PDU until every byte has arrived.
pub fn parse(data: &[u8]) -> Option<TunnelPdu> {
    if data.len() < 4 {
        return None;
    }
    let action = data[0] & 0x0F;
    let payload_len = u16::from_le_bytes([data[1], data[2]]) as usize;
    let header_len = (data[3] as usize).max(4);
    if data.len() < header_len {
        return None;
    }
    let body = &data[header_len..];
    let take = payload_len.min(body.len());
    Some(TunnelPdu {
        action,
        payload: body[..take].to_vec(),
    })
}

/// Frame exactly one complete PDU from the front of a byte stream, returning the
/// total number of bytes it occupies (`HeaderLength + PayloadLength`) alongside
/// the parsed PDU. Returns `None` when `data` does not yet hold a whole PDU, so
/// the caller keeps buffering and retries — this is what makes tunnel reads
/// resilient to a Data PDU (`PayloadLength` up to 65535) split across several
/// TLS records / UDP datagrams.
pub fn take_pdu(data: &[u8]) -> Option<(usize, TunnelPdu)> {
    if data.len() < 4 {
        return None;
    }
    let action = data[0] & 0x0F;
    let payload_len = u16::from_le_bytes([data[1], data[2]]) as usize;
    let header_len = (data[3] as usize).max(4);
    let total = header_len + payload_len;
    if data.len() < total {
        return None; // incomplete — need more bytes
    }
    Some((
        total,
        TunnelPdu {
            action,
            payload: data[header_len..total].to_vec(),
        },
    ))
}

/// Build the Tunnel Create Request: the `request_id` and 16-byte `cookie` from
/// the server's multitransport request, proving this UDP connection belongs to
/// the main session.
pub fn create_request(request_id: u32, cookie: &[u8; 16]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&request_id.to_le_bytes());
    payload.extend_from_slice(cookie);
    frame(ACTION_CREATE_REQUEST, &payload)
}

/// Parse a Tunnel Create Response, returning the server's `HRESULT`
/// (`0 == S_OK`). `None` if it isn't a create-response PDU.
pub fn parse_create_response(data: &[u8]) -> Option<u32> {
    let pdu = parse(data)?;
    if pdu.action != ACTION_CREATE_RESPONSE || pdu.payload.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([
        pdu.payload[0],
        pdu.payload[1],
        pdu.payload[2],
        pdu.payload[3],
    ]))
}

/// Wrap higher-layer bytes (e.g. an RDPGFX DVC payload) as a Tunnel Data PDU.
pub fn data(payload: &[u8]) -> Vec<u8> {
    frame(ACTION_DATA, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_carries_id_and_cookie() {
        let cookie = [0x5A; 16];
        let pdu = create_request(0xDEAD_BEEF, &cookie);
        let parsed = parse(&pdu).unwrap();
        assert_eq!(parsed.action, ACTION_CREATE_REQUEST);
        assert_eq!(
            u32::from_le_bytes([parsed.payload[0], parsed.payload[1], parsed.payload[2], parsed.payload[3]]),
            0xDEAD_BEEF
        );
        assert_eq!(&parsed.payload[4..20], &cookie);
    }

    #[test]
    fn create_response_hresult_parses() {
        let resp = frame(ACTION_CREATE_RESPONSE, &0u32.to_le_bytes());
        assert_eq!(parse_create_response(&resp), Some(0));
        // A data PDU isn't a create-response.
        assert_eq!(parse_create_response(&data(&[1, 2, 3])), None);
    }

    #[test]
    fn data_roundtrips_and_skips_header() {
        let pdu = data(&[0xAA, 0xBB, 0xCC]);
        let parsed = parse(&pdu).unwrap();
        assert_eq!(parsed.action, ACTION_DATA);
        assert_eq!(parsed.payload, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn parse_honors_header_length_with_subheaders() {
        // action=DATA, payloadLen=2, headerLength=6 (4 + a 2-byte subheader).
        let mut pdu = vec![ACTION_DATA, 0x02, 0x00, 0x06, 0x77, 0x88];
        pdu.extend_from_slice(&[0x11, 0x22]); // payload after the 6-byte header
        let parsed = parse(&pdu).unwrap();
        assert_eq!(parsed.payload, vec![0x11, 0x22]);
    }

    #[test]
    fn take_pdu_waits_for_full_payload() {
        // A large Data PDU whose PayloadLength exceeds one datagram must not be
        // framed until every byte is present — otherwise the stream desyncs.
        let payload: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        let pdu = data(&payload);
        // Not one byte short of complete yet.
        assert_eq!(take_pdu(&pdu[..pdu.len() - 1]), None);
        // Exactly complete: frames the whole payload and reports its length.
        let (consumed, framed) = take_pdu(&pdu).unwrap();
        assert_eq!(consumed, pdu.len());
        assert_eq!(framed.action, ACTION_DATA);
        assert_eq!(framed.payload, payload);
    }

    #[test]
    fn take_pdu_frames_back_to_back_pdus() {
        // Two PDUs concatenated in one buffer drain one at a time, leaving the
        // remainder intact for the next call.
        let mut stream = data(&[0xAA, 0xBB]);
        stream.extend_from_slice(&data(&[0xCC]));
        let (n1, first) = take_pdu(&stream).unwrap();
        assert_eq!(first.payload, vec![0xAA, 0xBB]);
        let (n2, second) = take_pdu(&stream[n1..]).unwrap();
        assert_eq!(second.payload, vec![0xCC]);
        assert_eq!(n1 + n2, stream.len());
    }
}
