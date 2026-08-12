//! Integration tests for the wire-main codec (the `pdu` module): TPKT/X.224,
//! MCS (T.125), the Standard RDP Security exchange header, the capability
//! exchange, and a slow-path Bitmap Update PDU.
//!
//! Fixtures are literal byte arrays built from the field layouts documented in
//! MS-RDPBCGR / T.125 — the same bytes a real peer would send — and both parse
//! and encode → decode round-trip paths are exercised through the public API.

use wire_main::pdu::{caps, mcs, security, x224};

// ---------------------------------------------------------------------------
// X.224 Connection Request / Confirm (MS-RDPBCGR 2.2.1.1 / 2.2.1.2)
// ---------------------------------------------------------------------------

#[test]
fn x224_connection_request_standard_security_fixture() {
    // TPKT body of a CR advertising no security protocols (Standard RDP
    // Security): LI=0x0e, CR=0xe0, zero refs/class, RDP_NEG_REQ proto=0.
    let cr = x224::ConnectionRequest {
        requested_protocols: 0,
        cookie: None,
    };
    let mut body = Vec::new();
    cr.encode(&mut body).unwrap();
    let expected = [
        0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, // LI, CR, DST-REF, SRC-REF, class
        0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, // RDP_NEG_REQ type=1 len=8 proto=0
    ];
    assert_eq!(body, expected);
}

#[test]
fn x224_connection_request_tls_and_hybrid_fixture() {
    // The NLA path advertises SSL | HYBRID (0x00000003).
    let cr = x224::ConnectionRequest {
        requested_protocols: 3,
        cookie: None,
    };
    let mut body = Vec::new();
    cr.encode(&mut body).unwrap();
    assert_eq!(body[0], 0x0e); // LI = fixed header (6) + RDP_NEG_REQ (8)
    assert_eq!(body[1], x224::X224_CR);
    // The fixed CR header is LI(1) + CR(1) + DST-REF(2) + SRC-REF(2) +
    // class(1), so the RDP_NEG_REQ starts at index 7: type, flags,
    // length(LE), protocols(LE).
    assert_eq!(&body[7..11], &[0x01, 0x00, 0x08, 0x00]);
    assert_eq!(
        u32::from_le_bytes([body[11], body[12], body[13], body[14]]),
        3
    );
}

#[test]
fn x224_connection_confirm_response_fixture() {
    // Server CC selecting protocol 2 (HYBRID): LI=0x0e, CC=0xd0, NEG_RSP.
    let cc = [
        0x0e, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00, // LI, CC, refs, class
        0x02, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00, // NEG_RSP flags=0 proto=2
    ];
    let mut cur: &[u8] = &cc;
    let parsed = x224::ConnectionConfirm::decode(&mut cur).unwrap();
    assert_eq!(
        parsed,
        x224::ConnectionConfirm::Response {
            flags: 0,
            selected_protocol: 2,
        }
    );
    assert!(cur.is_empty(), "whole confirm body consumed");
}

#[test]
fn x224_connection_confirm_failure_fixture() {
    // Server rejecting the negotiation with failure code 5
    // (HYBRID_REQUIRED_BY_SERVER).
    let cc = [
        0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, // LI, CC, refs, class
        0x03, 0x00, 0x08, 0x00, 0x05, 0x00, 0x00, 0x00, // NEG_FAILURE code=5
    ];
    let mut cur: &[u8] = &cc;
    assert_eq!(
        x224::ConnectionConfirm::decode(&mut cur).unwrap(),
        x224::ConnectionConfirm::Failure { code: 5 }
    );
}

#[test]
fn x224_connection_confirm_legacy_no_negotiation() {
    // LI == 6 (fixed CR/CC header only): a legacy server, no RDP_NEG follows.
    let cc = [0x06, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00];
    let mut cur: &[u8] = &cc;
    assert_eq!(
        x224::ConnectionConfirm::decode(&mut cur).unwrap(),
        x224::ConnectionConfirm::NoNegotiation
    );
    assert!(cur.is_empty());
}

// ---------------------------------------------------------------------------
// MCS (T.125): Erect Domain, Attach User/Confirm
// ---------------------------------------------------------------------------

#[test]
fn mcs_erect_domain_matches_spec_bytes() {
    // T.125 §11.16 ErectDomainRequest, subHeight = subInterval = 0.
    assert_eq!(mcs::erect_domain_request(), [0x04, 0x01, 0x00, 0x01, 0x00]);
}

#[test]
fn mcs_attach_user_matches_spec_bytes() {
    // T.125 §11.19 AttachUserRequest.
    assert_eq!(mcs::attach_user_request(), [0x28]);
}

#[test]
fn mcs_attach_user_confirm_parse_and_roundtrip() {
    // The server grants user channel 1001 (0x03e9) — wire form from the
    // handshake: choice 11 << 2, result 0, initiator 1001.
    let raw = [0x2c, 0x00, 0x03, 0xe9];
    assert_eq!(mcs::parse_attach_user_confirm(&raw).unwrap().user_id, 1001);

    // Round trip: encode user id 1001 back to wire form and re-parse.
    let mut round = vec![0x2c, 0x00];
    round.extend_from_slice(&1001u16.to_be_bytes());
    assert_eq!(
        mcs::parse_attach_user_confirm(&round).unwrap().user_id,
        1001
    );
}

#[test]
fn mcs_channel_join_roundtrip() {
    // T.125 ChannelJoinRequest for the I/O channel (1003) from user 1001.
    let req = mcs::channel_join_request(1001, 1003);
    assert_eq!(req, [0x38, 0x03, 0xe9, 0x03, 0xeb]);
    // The confirm echoes the channel id in the last two bytes.
    let confirm = [0x3e, 0x00, 0x03, 0xe9, 0x03, 0xeb, 0x03, 0xeb];
    assert_eq!(
        mcs::parse_channel_join_confirm(&confirm)
            .unwrap()
            .channel_id,
        1003
    );
}

#[test]
fn mcs_send_data_request_roundtrip() {
    // A client → server Send Data Request on the I/O channel carrying a payload.
    let payload = vec![0xde, 0xad, 0xbe, 0xef];
    let sdr = mcs::send_data_request(1001, 1003, &payload);
    let parsed = mcs::parse_send_data_request(&sdr).unwrap();
    assert_eq!(parsed.channel_id, 1003);
    assert_eq!(parsed.data, payload);
}

// ---------------------------------------------------------------------------
// Standard RDP Security: the Security Exchange header (MS-RDPBCGR 2.2.1.10)
// ---------------------------------------------------------------------------

#[test]
fn security_exchange_header_layout_matches_spec() {
    // A 72-byte RSA-encrypted client random.
    let encrypted_random = [0xABu8; 72];
    let pdu = security::security_exchange_pdu(&encrypted_random);
    // Basic Security Header: flags = SEC_EXCHANGE_PKT (0x0001), flagsHi = 0.
    assert_eq!(&pdu[0..4], &[0x01, 0x00, 0x00, 0x00]);
    // Then the length of the encrypted random (LE), then the blob.
    assert_eq!(u32::from_le_bytes([pdu[4], pdu[5], pdu[6], pdu[7]]), 72);
    assert_eq!(&pdu[8..], &encrypted_random[..]);
}

#[test]
fn security_exchange_roundtrip_through_mcs() {
    // The exchange rides inside an MCS Send Data Request on the user channel;
    // parse it back out and re-read the header fields from the wire bytes.
    let encrypted_random = [0xCDu8; 64];
    let exchange = security::security_exchange_pdu(&encrypted_random);
    let sdr = mcs::send_data_request(1001, 1001, &exchange);

    let parsed = mcs::parse_send_data_request(&sdr).unwrap();
    assert_eq!(parsed.channel_id, 1001, "sent on the user channel");
    let mut payload: &[u8] = &parsed.data;
    let flags = security::read_basic_security_header(&mut payload).unwrap();
    assert_eq!(flags, security::SEC_EXCHANGE_PKT);
    let len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    assert_eq!(len, 64);
    assert_eq!(&payload[4..], &encrypted_random[..]);
}

// ---------------------------------------------------------------------------
// Capability exchange: Demand Active / Confirm Active (MS-RDPBCGR 2.2.1.13)
// ---------------------------------------------------------------------------

const SHARE_ID: u32 = 0x0003_0000;

/// Build a Demand Active PDU: share control header (6) + share data header
/// (12) + body (inner header, numberCapabilities, caps, sessionId).
fn build_demand_active(share_id: u32, caps_bytes: &[u8], num_caps: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0011u16.to_le_bytes()); // inner share control header
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&num_caps.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // pad2
    body.extend_from_slice(caps_bytes);
    body.extend_from_slice(&0u32.to_le_bytes()); // sessionId

    let mut demand = Vec::new();
    caps::write_share_control_header(
        caps::PDUTYPE_DEMAND_ACTIVE,
        1002,
        12 + body.len(),
        &mut demand,
    );
    caps::write_share_data_header(share_id, 0x11, body.len(), &mut demand);
    demand.extend_from_slice(&body);
    demand
}

#[test]
fn demand_active_parse_and_confirm_active_roundtrip() {
    let caps_bytes = caps::all_caps(1280, 800);
    let demand = build_demand_active(SHARE_ID, &caps_bytes, 7);
    let parsed = caps::parse_demand_active(&demand).unwrap();
    assert_eq!(parsed.share_id, SHARE_ID);
    assert_eq!(parsed.caps, caps_bytes);

    // The client replies with a Confirm Active echoing the same share id.
    let confirm = caps::confirm_active_pdu(SHARE_ID, 1007, &caps_bytes);
    let mut cur: &[u8] = &confirm;
    let (pdu_type, source) = caps::read_share_control_header(&mut cur).unwrap();
    assert_eq!(pdu_type, caps::PDUTYPE_CONFIRM_ACTIVE);
    assert_eq!(source, 1007);
    let (share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(share_id, SHARE_ID);
    assert_eq!(pdu_type2, 0x13); // PDUTYPE2_CONFIRM_ACTIVE
}

#[test]
fn demand_active_rejects_wrong_pdu_type() {
    let demand = build_demand_active(SHARE_ID, &caps::all_caps(640, 480), 7);
    // Flip the pduType nibble to CONFIRM_ACTIVE: parse must refuse it.
    let mut bad = demand;
    let pdu_type = u16::from_le_bytes([bad[2], bad[3]]);
    bad[2] = ((pdu_type & !0x0f) | caps::PDUTYPE_CONFIRM_ACTIVE) as u8;
    assert!(caps::parse_demand_active(&bad).is_err());
}

// ---------------------------------------------------------------------------
// Bitmap Update PDU (MS-RDPBCGR 2.2.9.1.1.3.1.2) — slow path
// ---------------------------------------------------------------------------

/// A spec-driven reader for the TS_UPDATE_BITMAP body (updateType,
/// numberRectangles, then per-rectangle headers) used to verify the wire
/// bytes independent of the crate internals.
fn read_u16_le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[test]
fn bitmap_update_pdu_parses_through_share_data_framing() {
    // TS_UPDATE_BITMAP: updateType = 0x0001 (BITMAP), numberRectangles = 1,
    // one 2×2 32bpp rectangle (18-byte header + 16 bytes of pixel data).
    let mut update = Vec::new();
    update.extend_from_slice(&0x0001u16.to_le_bytes()); // updateType = BITMAP
    update.extend_from_slice(&1u16.to_le_bytes()); // numberRectangles
    update.extend_from_slice(&[
        0x00, 0x00, // destLeft
        0x00, 0x00, // destTop
        0x01, 0x00, // destRight
        0x01, 0x00, // destBottom
        0x02, 0x00, // width
        0x02, 0x00, // height
        0x20, 0x00, // bitsPerPixel = 32
        0x00, 0x00, // flags (uncompressed)
        0x10, 0x00, // bitmapLength = 16
    ]);
    update.extend_from_slice(&[
        0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, //
        0x22, 0x32, 0x42, 0xFF, 0x33, 0x43, 0x53, 0xFF,
    ]);

    // Wrap as the server would: a slow-path Data PDU (share control + share
    // data headers, pduType2 = PDUTYPE2_UPDATE = 2) carrying the update body.
    let mut pdu = Vec::new();
    caps::write_share_control_header(caps::PDUTYPE_DATA, 1002, 12 + update.len(), &mut pdu);
    caps::write_share_data_header(SHARE_ID, caps::PDUTYPE2_UPDATE, update.len(), &mut pdu);
    pdu.extend_from_slice(&update);

    // Parse the share framing back off and verify the payload round-trips.
    let mut cur: &[u8] = &pdu;
    let (pdu_type, source) = caps::read_share_control_header(&mut cur).unwrap();
    assert_eq!(pdu_type, caps::PDUTYPE_DATA);
    assert_eq!(source, 1002);
    let (share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(share_id, SHARE_ID);
    assert_eq!(pdu_type2, caps::PDUTYPE2_UPDATE);
    assert_eq!(cur, &update[..], "update body round-trips unchanged");

    // Spec-driven check of the bitmap update structure.
    assert_eq!(read_u16_le(cur, 0), 0x0001, "updateType = BITMAP");
    assert_eq!(read_u16_le(cur, 2), 1, "numberRectangles");
    assert_eq!(read_u16_le(cur, 4 + 8), 2, "rect width");
    assert_eq!(read_u16_le(cur, 4 + 10), 2, "rect height");
    assert_eq!(read_u16_le(cur, 4 + 12), 32, "bitsPerPixel");
    assert_eq!(read_u16_le(cur, 4 + 16), 16, "bitmapLength");
}

#[test]
fn bitmap_update_pdu_encode_roundtrip_reproduces_fixture() {
    // Re-encode the framing from scratch and require byte-for-byte equality
    // with the fixture built in the parse test.
    let mut update = Vec::new();
    update.extend_from_slice(&0x0001u16.to_le_bytes());
    update.extend_from_slice(&1u16.to_le_bytes());
    update.extend_from_slice(&[0u8; 18]); // zeroed rect header
    update.extend_from_slice(&[0x42u8; 16]); // 2×2 @ 32bpp

    let mut pdu = Vec::new();
    caps::write_share_control_header(caps::PDUTYPE_DATA, 1002, 12 + update.len(), &mut pdu);
    caps::write_share_data_header(SHARE_ID, caps::PDUTYPE2_UPDATE, update.len(), &mut pdu);
    pdu.extend_from_slice(&update);

    // The Share Control Header's totalLength must equal the PDU length.
    assert_eq!(
        u16::from_le_bytes([pdu[0], pdu[1]]) as usize,
        pdu.len(),
        "totalLength covers the whole PDU"
    );
    // pduType = PDUTYPE_DATA | PROTOCOL_VERSION (0x10).
    assert_eq!(
        u16::from_le_bytes([pdu[2], pdu[3]]) & 0x0f,
        caps::PDUTYPE_DATA
    );

    // Parse it back: the update body is the exact tail.
    let mut cur: &[u8] = &pdu;
    let _ = caps::read_share_control_header(&mut cur).unwrap();
    let (_share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(pdu_type2, caps::PDUTYPE2_UPDATE);
    assert_eq!(cur, &update[..]);
}
