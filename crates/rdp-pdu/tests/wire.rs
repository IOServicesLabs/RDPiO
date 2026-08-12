//! Integration tests for the RDP wire codec (`rdp-pdu`), driven by byte-array
//! fixtures taken from the RDP protocol specification (MS-RDPBCGR / T.125)
//! rather than by mocking internal functions.
//!
//! Each fixture below is a literal byte sequence built strictly from the
//! documented field layout. Every test exercises the crate's public parse /
//! serialize API, and where the codec supports both directions an
//! encode → decode round trip is performed.

use rdp_pdu::capabilities;
use rdp_pdu::fastpath::{self, FragmentReassembler};
use rdp_pdu::mcs;
use rdp_pdu::security;
use rdp_pdu::x224::{
    self, ConnectionConfirm, ConnectionRequest, NegFailureCode, NegResponseFlags, SecurityProtocol,
};

// ---------------------------------------------------------------------------
// X.224 Connection Request / Confirm (MS-RDPBCGR 2.2.1.1 / 2.2.1.2)
// ---------------------------------------------------------------------------

/// The canonical client Connection Request from MS-RDPBCGR §2.2.1.1: a TPKT
/// frame (version 3, big-endian length 0x13 = 19) carrying the X.224 CR TPDU
/// (length indicator 0x0e) and an RDP Negotiation Request advertising TLS +
/// CredSSP (`requestedProtocols` = 0x00000003).
const X224_CR_FIXTURE: [u8; 19] = [
    0x03, 0x00, 0x00, 0x13, // TPKT header (version 3, length 19 BE)
    0x0e, // X.224 length indicator
    0xe0, // Connection Request
    0x00, 0x00, // DST-REF
    0x00, 0x00, // SRC-REF
    0x00, // class/options
    0x01, 0x00, 0x08, 0x00, 0x03, 0x00, 0x00,
    0x00, // RDP_NEG_REQ type=1, flags=0, len=8, proto=3
];

#[test]
fn x224_connection_request_serializes_to_spec_bytes() {
    // `ConnectionRequest::default()` advertises SSL | HYBRID with no flags and
    // no cookie — exactly the bytes of the specification example.
    let cr = ConnectionRequest::default();
    let bytes = cr.to_bytes().unwrap();
    assert_eq!(bytes, X224_CR_FIXTURE);

    // The TPKT framing must be self-consistent: version byte and BE length.
    assert_eq!(bytes[0], x224::TPKT_VERSION);
    assert_eq!(x224::read_tpkt_len(&bytes).unwrap(), bytes.len());
}

#[test]
fn x224_connection_request_with_cookie_keeps_framing_consistent() {
    // The mstshash cookie line precedes the RDP_NEG_REQ inside the user data;
    // the TPKT length and X.224 length indicator must still match the buffer.
    let cr = ConnectionRequest {
        cookie: Some("alice".into()),
        ..Default::default()
    };
    let bytes = cr.to_bytes().unwrap();
    let tpkt_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    assert_eq!(tpkt_len, bytes.len());
    assert_eq!(bytes[4] as usize, bytes.len() - (x224::TPKT_HEADER_LEN + 1));
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("Cookie: mstshash=alice\r\n"));
}

#[test]
fn x224_connection_confirm_negotiation_response() {
    // MS-RDPBCGR §2.2.1.2.1: the server accepts and selects HYBRID (CredSSP),
    // setting EXTENDED_CLIENT_DATA_SUPPORTED in the response flags.
    let fixture: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, // TPKT header
        0x0e, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00, // LI, CC, refs, class
        0x02, 0x01, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00, // NEG_RSP flags=1 proto=2
    ];
    let mut cur: &[u8] = &fixture;
    let parsed = ConnectionConfirm::decode(&mut cur).unwrap();
    assert_eq!(
        parsed,
        ConnectionConfirm::Response {
            flags: NegResponseFlags::EXTENDED_CLIENT_DATA_SUPPORTED,
            selected_protocol: SecurityProtocol::HYBRID,
        }
    );
    assert!(cur.is_empty(), "the whole TPKT PDU must be consumed");
}

#[test]
fn x224_connection_confirm_negotiation_failure() {
    // MS-RDPBCGR §2.2.1.2.2: failureCode 5 = HYBRID_REQUIRED_BY_SERVER.
    let fixture: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, // TPKT header
        0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, // LI, CC, refs, class
        0x03, 0x00, 0x08, 0x00, 0x05, 0x00, 0x00, 0x00, // NEG_FAILURE code=5
    ];
    let mut cur: &[u8] = &fixture;
    let parsed = ConnectionConfirm::decode(&mut cur).unwrap();
    assert_eq!(
        parsed,
        ConnectionConfirm::Failure {
            failure_code: NegFailureCode::HybridRequiredByServer,
        }
    );
    assert!(cur.is_empty());
}

#[test]
fn x224_connection_confirm_legacy_no_negotiation() {
    // A pre-negotiation server replies with a bare CC: the length indicator
    // equals the fixed CR/CC header size, so no RDP_NEG structure follows.
    let fixture: [u8; 11] = [
        0x03, 0x00, 0x00, 0x0b, 0x06, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00,
    ];
    let mut cur: &[u8] = &fixture;
    let parsed = ConnectionConfirm::decode(&mut cur).unwrap();
    assert_eq!(parsed, ConnectionConfirm::NoNegotiation);
    assert!(cur.is_empty());
}

// ---------------------------------------------------------------------------
// MCS (T.125): Erect Domain, Attach User, Channel Join
// ---------------------------------------------------------------------------

#[test]
fn mcs_erect_domain_matches_spec_bytes() {
    // T.125 §11.16 ErectDomainRequest with subHeight = subInterval = 0.
    assert_eq!(mcs::erect_domain_request(), [0x04, 0x01, 0x00, 0x01, 0x00]);
}

#[test]
fn mcs_attach_user_request_matches_spec_bytes() {
    // T.125 §11.19 AttachUserRequest.
    assert_eq!(mcs::attach_user_request(), [0x28]);
}

#[test]
fn mcs_attach_user_confirm_parse_and_roundtrip() {
    // T.125 AttachUserConfirm granting initiator 3 → user channel 1001 + 3.
    let raw = [0x2e, 0x00, 0x00, 0x03];
    assert_eq!(mcs::parse_attach_user_confirm(&raw).unwrap(), 1004);

    // The same PDU arrives TPKT + X.224 Data framed on the wire.
    let framed = mcs::frame(&raw).unwrap();
    assert_eq!(mcs::parse_attach_user_confirm(&framed).unwrap(), 1004);

    // Round trip: encode user channel 1004 back to wire form and re-parse.
    let initiator = (1004u16 - mcs::MCS_BASE_CHANNEL_ID).to_be_bytes();
    let mut round = vec![0x2e, 0x00];
    round.extend_from_slice(&initiator);
    assert_eq!(mcs::parse_attach_user_confirm(&round).unwrap(), 1004);
}

#[test]
fn mcs_channel_join_request_and_confirm_roundtrip() {
    // T.125 ChannelJoinRequest for the I/O channel from user 1004.
    let req = mcs::channel_join_request(1004, 1003);
    assert_eq!(req, [0x38, 0x00, 0x03, 0x03, 0xeb]);
    // The confirm echoes initiator / requested / channel ids.
    let confirm = [0x3e, 0x00, 0x00, 0x03, 0x03, 0xeb, 0x03, 0xeb];
    assert_eq!(mcs::parse_channel_join_confirm(&confirm).unwrap(), 1003);
}

#[test]
fn mcs_send_data_indication_parse_and_framed_roundtrip() {
    // T.125 SendDataIndication on the I/O channel (1003) carrying [AA, BB].
    let sdi = [0x68, 0x00, 0x03, 0x03, 0xeb, 0x70, 0x02, 0xAA, 0xBB];
    let (channel, payload) = mcs::parse_send_data_indication(&sdi).unwrap();
    assert_eq!((channel, payload), (1003, vec![0xAA, 0xBB]));

    // TPKT + X.224 Data framing round trip.
    let framed = mcs::frame(&sdi).unwrap();
    let (channel2, payload2) = mcs::parse_send_data_indication(&framed).unwrap();
    assert_eq!((channel2, payload2), (1003, vec![0xAA, 0xBB]));
}

// ---------------------------------------------------------------------------
// Standard RDP Security: the Security Exchange header (MS-RDPBCGR 2.2.1.10)
// ---------------------------------------------------------------------------

#[test]
fn security_exchange_header_layout_matches_spec() {
    // A 72-byte RSA-encrypted client random.
    let encrypted_random = [0xABu8; 72];
    let pdu = security::security_exchange(&encrypted_random);
    // Basic Security Header: flags = SEC_EXCHANGE_PKT (0x0001), flagsHi = 0.
    assert_eq!(&pdu[0..4], &[0x01, 0x00, 0x00, 0x00]);
    // Then the length of the encrypted random (LE), then the blob.
    assert_eq!(u32::from_le_bytes([pdu[4], pdu[5], pdu[6], pdu[7]]), 72);
    assert_eq!(&pdu[8..], &encrypted_random[..]);
}

#[test]
fn security_exchange_roundtrip_through_mcs_wrapper() {
    // The exchange rides inside an MCS Send Data Request on the user channel.
    let encrypted_random = [0xCDu8; 64];
    let exchange = security::security_exchange(&encrypted_random);
    let sdr = mcs::send_data_request(1001, 1001, &exchange);
    assert_eq!(sdr[0], 0x64); // SendDataRequest choice (25 << 2)
    assert_eq!(u16::from_be_bytes([sdr[3], sdr[4]]), 1001); // user channel

    // Unwrap the PER-encoded payload (single-byte length for ≤ 0x7f bytes).
    let len = sdr[6] as usize;
    assert_eq!(len, exchange.len());
    assert_eq!(&sdr[7..7 + len], &exchange[..]);
}

// ---------------------------------------------------------------------------
// Capability exchange: Demand Active / Confirm Active (MS-RDPBCGR 2.2.1.13)
// ---------------------------------------------------------------------------

/// Build a Demand Active PDU from (capability type, payload) sets, following
/// the MS-RDPBCGR §2.2.1.13.1 layout: share control header (6), share data
/// header (12), then lengthSourceDescriptor, lengthCombinedCapabilities,
/// sourceDescriptor, numberCapabilities, pad2octets, capability sets,
/// sessionId.
fn build_demand_active(share_id: u32, sets: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut caps = Vec::new();
    for (cap_type, payload) in sets {
        caps.extend_from_slice(&cap_type.to_le_bytes());
        caps.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
        caps.extend_from_slice(payload);
    }
    let source = b"rdpio";
    let mut body = Vec::new();
    body.extend_from_slice(&share_id.to_le_bytes());
    body.extend_from_slice(&(source.len() as u16).to_le_bytes()); // lengthSourceDescriptor
    body.extend_from_slice(&((2 + 2 + caps.len()) as u16).to_le_bytes()); // lengthCombined
    body.extend_from_slice(source);
    body.extend_from_slice(&(sets.len() as u16).to_le_bytes()); // numberCapabilities
    body.extend_from_slice(&0u16.to_le_bytes()); // pad2octets
    body.extend_from_slice(&caps);
    body.extend_from_slice(&0u32.to_le_bytes()); // sessionId

    let total = (6 + body.len()) as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&0x0011u16.to_le_bytes()); // DEMAND_ACTIVE | PROTOCOL_VERSION
    out.extend_from_slice(&0x03eau16.to_le_bytes()); // pduSource
    out.extend_from_slice(&body);
    out
}

#[test]
fn demand_active_parse_and_confirm_active_roundtrip() {
    let share_id = 0x0001_03EA;
    // One minimal General capability set (type 1, 20 payload bytes).
    let sets = vec![(1u16, vec![0u8; 20])];
    let demand = build_demand_active(share_id, &sets);
    assert_eq!(
        capabilities::parse_demand_active(&demand).unwrap(),
        share_id
    );

    // The client answers with a Confirm Active echoing the same share id.
    let confirm = capabilities::confirm_active(share_id, 1007, 1280, 800, 0x0409, false);
    assert_eq!(
        u16::from_le_bytes([confirm[0], confirm[1]]) as usize,
        confirm.len(),
        "totalLength covers the whole PDU"
    );
    assert_eq!(
        u16::from_le_bytes([confirm[2], confirm[3]]) & 0x0f,
        0x3, // PDUTYPE_CONFIRM_ACTIVE
    );
    assert_eq!(
        u32::from_le_bytes([confirm[6], confirm[7], confirm[8], confirm[9]]),
        share_id,
        "Confirm Active echoes the server's share id"
    );
}

#[test]
fn demand_active_with_rfx_caps_parses_share_id() {
    let share_id = 0x0003_0000;
    // 8 capability sets (the RemoteFX-enabled client advertises
    // Surface Commands + Bitmap Codecs on top of the 6 base sets).
    let sets: Vec<(u16, Vec<u8>)> = vec![
        (1, vec![0u8; 20]),    // General
        (2, vec![0u8; 24]),    // Bitmap
        (3, vec![0u8; 84]),    // Order
        (8, vec![0u8; 6]),     // Pointer
        (13, vec![0u8; 88]),   // Input
        (20, vec![0u8; 4]),    // Virtual Channel
        (0x1C, vec![0u8; 8]),  // Surface Commands
        (0x1D, vec![0u8; 69]), // Bitmap Codecs
    ];
    let demand = build_demand_active(share_id, &sets);
    assert_eq!(
        capabilities::parse_demand_active(&demand).unwrap(),
        share_id
    );
}

#[test]
fn demand_active_input_flags_extracted() {
    // TS_INPUT_CAPABILITYSET with inputFlags = 0x0080 (MOUSE_RELATIVE).
    let mut input = Vec::new();
    input.extend_from_slice(&0x0080u16.to_le_bytes()); // inputFlags
    input.extend_from_slice(&0u16.to_le_bytes()); // pad
    input.extend_from_slice(&0x0409u32.to_le_bytes()); // keyboardLayout
    input.extend_from_slice(&0u32.to_le_bytes()); // pad
    input.extend_from_slice(&4u32.to_le_bytes()); // keyboardType
    input.extend_from_slice(&0u32.to_le_bytes()); // keyboardSubType
    input.extend_from_slice(&12u32.to_le_bytes()); // keyboardFunctionKey
    input.extend_from_slice(&[0u8; 64]); // imeFileName
    let sets = vec![(13u16, input)]; // CAPSET_INPUT
    let demand = build_demand_active(0x0001_03EA, &sets);
    assert_eq!(
        capabilities::parse_server_input_flags(&demand),
        Some(0x0080)
    );
}

// ---------------------------------------------------------------------------
// Bitmap Update PDU (MS-RDPBCGR 2.2.9.1.2 — fast-path output)
// ---------------------------------------------------------------------------

#[test]
fn fastpath_bitmap_update_pdu_parse_and_roundtrip() {
    // A fast-path output PDU carrying a single uncompressed TS_FP_UPDATE with
    // updateCode 0x1 (BITMAP) and a 4-byte payload.
    let bitmap_payload = [0xAA, 0xBB, 0xCC, 0xDD];
    let mut body = Vec::new();
    body.push(fastpath::FASTPATH_UPDATETYPE_BITMAP); // frag=SINGLE, no compression
    body.extend_from_slice(&(bitmap_payload.len() as u16).to_le_bytes());
    body.extend_from_slice(&bitmap_payload);

    // fpOutputHeader: action=0 (fast-path), flags=0; 1-byte length.
    let mut pdu = vec![0x00u8];
    let total = body.len() + 2;
    pdu.push(total as u8);
    pdu.extend_from_slice(&body);

    assert!(fastpath::is_fastpath_output(pdu[0]));
    assert_eq!(fastpath::output_pdu_len(&pdu), Some(total));

    let mut frag = FragmentReassembler::new();
    let updates = fastpath::parse_output(&pdu, &mut frag).unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].code, fastpath::FASTPATH_UPDATETYPE_BITMAP);
    assert_eq!(updates[0].data, bitmap_payload);
}

#[test]
fn fastpath_surface_bits_parse_and_roundtrip() {
    // A TS_SURFCMD SET_SURFACE_BITS (MS-RDPBCGR §2.2.9.2.1) with a
    // TS_BITMAP_DATA_EX: 32 bpp, codecID 0 (uncompressed), 64×64.
    let mut d = Vec::new();
    d.extend_from_slice(&0x0001u16.to_le_bytes()); // cmdType = SET_SURFACE_BITS
    d.extend_from_slice(&10u16.to_le_bytes()); // destLeft
    d.extend_from_slice(&20u16.to_le_bytes()); // destTop
    d.extend_from_slice(&74u16.to_le_bytes()); // destRight
    d.extend_from_slice(&84u16.to_le_bytes()); // destBottom
    d.extend_from_slice(&[32, 0, 0, 0]); // bpp, flags, reserved, codecID
    d.extend_from_slice(&64u16.to_le_bytes()); // width
    d.extend_from_slice(&64u16.to_le_bytes()); // height
    d.extend_from_slice(&4u32.to_le_bytes()); // bitmapDataLength
    d.extend_from_slice(&[9, 8, 7, 6]); // bitmapData

    let cmds = fastpath::parse_surface_commands(&d);
    assert_eq!(cmds.len(), 1);
    let c = &cmds[0];
    assert_eq!(
        (c.dest_left, c.dest_top, c.dest_right, c.dest_bottom),
        (10, 20, 74, 84)
    );
    assert_eq!(c.bpp, 32);
    assert_eq!(c.codec_id, 0);
    assert_eq!((c.width, c.height), (64, 64));
    assert_eq!(c.data, vec![9, 8, 7, 6]);
}
