//! Scripted handshake against an in-process fake RDP server.
//!
//! The fake server plays the server side of the Standard RDP Security
//! connection sequence and asserts every message the client sends, so the test
//! exercises the real [`wire_main::WireSession::connect`] code path end to end:
//! X.224 negotiation, MCS connect, security exchange, channel joins, Client
//! Info, licensing, and the capability exchange.
//!
//! RSA note: the fake server advertises a certificate whose public exponent is
//! 1, so `m^e mod n = m` and the "decrypted" client random is the leading 32
//! bytes of the security exchange payload. The client still performs a real
//! modular exponentiation; only the test double uses a degenerate key.

use std::net::TcpListener;
use std::thread;

use rdp_crypto::keys;

use wire_main::pdu::{caps, gcc, license, mcs, security, x224};
use wire_main::session::ConnectOptions;
use wire_main::transport::WireTransport;
use wire_main::{ServerPdu, WireSession};

const SERVER_RANDOM: [u8; 32] = [0x5e; 32];
const SHARE_ID: u32 = 0x0003_0000;

// --- tiny BER helpers (test-side) ------------------------------------------

fn ber_len(cur: &mut &[u8]) -> usize {
    let first = cur[0];
    *cur = &cur[1..];
    if first < 0x80 {
        return first as usize;
    }
    let n = (first & 0x7f) as usize;
    let mut v = 0usize;
    for &b in &cur[..n] {
        v = (v << 8) | b as usize;
    }
    *cur = &cur[n..];
    v
}

fn ber_element(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if data.len() < 0x80 {
        out.push(data.len() as u8);
    } else {
        let mut bytes = Vec::new();
        let mut v = data.len();
        while v > 0 {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
        }
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes.iter().rev());
    }
    out.extend_from_slice(data);
    out
}

/// Wrap `data` in a multi-byte application tag (`[APPLICATION n]`).
fn application_tag(tag: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    if data.len() < 0x80 {
        out.push(data.len() as u8);
    } else {
        let mut bytes = Vec::new();
        let mut v = data.len();
        while v > 0 {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
        }
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes.iter().rev());
    }
    out.extend_from_slice(data);
    out
}

/// Walk BER elements, recording the last OCTET STRING value.
fn last_octet_string(mut cur: &[u8]) -> Vec<u8> {
    let mut last = Vec::new();
    while !cur.is_empty() {
        let tag = cur[0];
        cur = &cur[1..];
        let len = ber_len(&mut cur);
        if tag == 0x04 {
            last = cur[..len].to_vec();
        }
        cur = &cur[len..];
    }
    last
}

/// Count the static channels the client declared in its CS_NET block.
fn channel_count(client_blocks: &[u8]) -> usize {
    let mut cur = client_blocks;
    while cur.len() >= 4 {
        let kind = u16::from_le_bytes([cur[0], cur[1]]);
        let len = u16::from_le_bytes([cur[2], cur[3]]) as usize;
        if len < 4 || len > cur.len() {
            panic!("bad client block length");
        }
        if kind == gcc::CS_NET {
            return u32::from_le_bytes([cur[4], cur[5], cur[6], cur[7]]) as usize;
        }
        cur = &cur[len..];
    }
    0
}

fn block(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A SERVER_CERTIFICATE (v1, RSA-signed) whose public exponent is 1: the
/// "encrypted" client random equals the plaintext, so the fake server can
/// recover it without a private key.
fn fake_server_certificate() -> Vec<u8> {
    let modulus = vec![0xab; 64];
    let exponent = vec![0x01, 0x00, 0x00, 0x00];
    let mut key = Vec::new();
    key.extend_from_slice(&(modulus.len() as u32).to_le_bytes());
    key.extend_from_slice(&modulus);
    key.extend_from_slice(&(exponent.len() as u32).to_le_bytes());
    key.extend_from_slice(&exponent);
    let mut prop = Vec::new();
    prop.extend_from_slice(&0x3146_5352u32.to_le_bytes()); // "RSF1"
    prop.extend_from_slice(&1u32.to_le_bytes()); // RSA_KEY
    prop.extend_from_slice(&(key.len() as u32).to_le_bytes());
    prop.extend_from_slice(&key);
    prop.extend_from_slice(&0u32.to_le_bytes()); // signature len
    let mut cert = Vec::new();
    cert.extend_from_slice(&1u32.to_le_bytes()); // version
    cert.extend_from_slice(&1u32.to_le_bytes()); // CERT_TYPE_RSA_SIGNED
    cert.extend_from_slice(&(prop.len() as u32).to_le_bytes());
    cert.extend_from_slice(&0u32.to_le_bytes()); // nonce len
    cert.extend_from_slice(&prop);
    cert
}

// --- server-side send/recv helpers -----------------------------------------

fn server_send(s: &mut WireTransport, mcs_pdu: &[u8]) {
    let mut body = Vec::with_capacity(x224::DATA_HEADER.len() + mcs_pdu.len());
    x224::write_data_header(mcs_pdu.len(), &mut body).unwrap();
    body.extend_from_slice(mcs_pdu);
    s.send(&body).unwrap();
}

fn server_send_encrypted(
    s: &mut WireTransport,
    sec: &mut security::SecurityLayer,
    user_id: u16,
    channel: u16,
    pdu: &[u8],
) {
    let mut payload = Vec::new();
    security::write_basic_security_header(security::SEC_ENCRYPT, &mut payload);
    payload.extend(sec.seal(pdu));
    let ind = mcs::send_data_indication(user_id, channel, &payload);
    server_send(s, &ind);
}

/// Decrypt a client→server I/O-channel frame, returning (flags, plaintext).
fn recv_io(sec: &mut security::SecurityLayer, frame: &[u8]) -> (u16, Vec<u8>) {
    let parsed = mcs::parse_send_data_request(frame).unwrap();
    assert_eq!(parsed.channel_id, 1003);
    let mut payload = &parsed.data[..];
    let flags = security::read_basic_security_header(&mut payload).unwrap();
    let plain = sec.open(payload).unwrap();
    (flags, plain)
}

// --- fake server -----------------------------------------------------------

fn fake_server(listener: TcpListener, expected_channels: usize) {
    let (stream, _) = listener.accept().unwrap();
    let mut s = WireTransport::from_stream(stream, true).unwrap();

    // 1. X.224 Connection Request → Confirm (Standard RDP Security).
    let cr = s.recv().unwrap();
    assert_eq!(cr[1], x224::X224_CR);
    let mut cc = Vec::new();
    cc.push(0x0e); // LI
    cc.push(x224::X224_CC);
    cc.extend_from_slice(&[0x00, 0x00, 0x12, 0x34, 0x00]); // refs + class
    cc.extend_from_slice(&[0x02, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00]); // NEG_RSP proto=0
    s.send(&cc).unwrap();

    // 2. MCS Connect Initial → Connect Response.
    let initial = s.recv().unwrap();
    let mcs_body = x224::strip_data_header(&initial);
    assert_eq!(&mcs_body[..2], &[0x7f, 0x65]);
    let mut cur = &mcs_body[2..];
    let _len = ber_len(&mut cur);
    let ccr = last_octet_string(cur);

    // Parse the GCC CCR to learn the declared static channels.
    let mut cur = &ccr[2..];
    let _len = ber_len(&mut cur);
    let client_blocks = last_octet_string(cur);
    let declared = channel_count(&client_blocks);
    assert_eq!(declared, expected_channels);

    // Build the server response: SC_CORE + SC_SECURITY + SC_NET.
    let mut blocks = Vec::new();
    let mut core = Vec::new();
    core.extend_from_slice(&0x0008_0004u32.to_le_bytes()); // version
    core.extend_from_slice(&1280u16.to_le_bytes()); // width
    core.extend_from_slice(&800u16.to_le_bytes()); // height
    core.extend_from_slice(&[0u8; 2]); // pad
    core.extend_from_slice(&24u16.to_le_bytes()); // colorDepth
    blocks.extend(block(gcc::SC_CORE, &core));

    let cert = fake_server_certificate();
    let mut sec = Vec::new();
    sec.extend_from_slice(&0x0000_0002u32.to_le_bytes()); // 128-bit RC4
    sec.extend_from_slice(&1u32.to_le_bytes()); // ENCRYPTION_LEVEL_CLIENT_COMPATIBLE
    sec.extend_from_slice(&(SERVER_RANDOM.len() as u32).to_le_bytes());
    sec.extend_from_slice(&(cert.len() as u32).to_le_bytes());
    sec.extend_from_slice(&SERVER_RANDOM);
    sec.extend_from_slice(&cert);
    blocks.extend(block(gcc::SC_SECURITY, &sec));

    let mut net = Vec::new();
    net.extend_from_slice(&((declared + 1) as u32).to_le_bytes());
    for id in 1003..1003 + declared as u16 + 1 {
        net.extend_from_slice(&id.to_le_bytes());
    }
    blocks.extend(block(gcc::SC_NET, &net));

    let mut content = Vec::new();
    content.extend_from_slice(&[0x0a, 0x01, 0x00]); // result = rt-successful
    content.push(0x02);
    content.push(0x01);
    content.push(0x00); // calledConnectId = 0
    content.extend_from_slice(&[0x30, 0x00]); // empty domain params
                                              // GCC Conference Create Response: [APPLICATION 17] wrapping nodeID, tag,
                                              // and the userData (the server data blocks).
    let mut ccr_inner = Vec::new();
    ccr_inner.extend(ber_element(0x04, &[0x01])); // nodeID
    ccr_inner.extend(ber_element(0x04, &[])); // tag
    ccr_inner.extend(ber_element(0x04, &blocks)); // userData
    let ccr = application_tag(&[0x7f, 0x11], &ccr_inner);
    content.extend(ber_element(0x04, &ccr)); // GCC user data
    let resp_body = application_tag(&[0x7f, 0x66], &content); // [APPLICATION 102]
    server_send(&mut s, &resp_body);

    // 3. Erect Domain + Attach User → Attach User Confirm (user id 1001).
    let _erect = s.recv().unwrap();
    let attach = s.recv().unwrap();
    assert_eq!(x224::strip_data_header(&attach), &[0x28]);
    server_send(&mut s, &[0x2c, 0x00, 0x03, 0xe9]); // user id 1001

    // 4. Security Exchange → recover the client random (e = 1).
    let exchange = s.recv().unwrap();
    let parsed = mcs::parse_send_data_request(&exchange).unwrap();
    assert_eq!(parsed.channel_id, 1001);
    let mut payload = &parsed.data[..];
    let flags = security::read_basic_security_header(&mut payload).unwrap();
    assert_eq!(flags, security::SEC_EXCHANGE_PKT);
    // Security Exchange PDU: length(4) + encrypted client random.
    let random_len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    assert_eq!(random_len, 64);
    let client_random = &payload[4..36];

    let derived = keys::derive(client_random, &SERVER_RANDOM, 0x02);
    // Server perspective: encrypt with the server key, decrypt with the client key.
    let mut sec_layer = security::SecurityLayer::new(
        &derived.server_decrypt_key,
        &derived.client_encrypt_key,
        &derived.mac_key,
    );

    // 5. Channel joins: I/O channel + declared static channels.
    for id in 1003..1003 + declared as u16 + 1 {
        let join = s.recv().unwrap();
        let join_req = x224::strip_data_header(&join);
        assert_eq!(join_req[0], 0x38);
        let requested = u16::from_be_bytes([join_req[3], join_req[4]]);
        assert_eq!(requested, id);
        let mut confirm = vec![0x3e, 0x00];
        confirm.extend_from_slice(&1001u16.to_be_bytes()); // initiator
        confirm.extend_from_slice(&id.to_be_bytes()); // requested
        confirm.extend_from_slice(&id.to_be_bytes()); // channel
        server_send(&mut s, &confirm);
    }

    // 6. Client Info PDU (encrypted).
    let info = s.recv().unwrap();
    let parsed = mcs::parse_send_data_request(&info).unwrap();
    assert_eq!(parsed.channel_id, 1003);
    let mut payload = &parsed.data[..];
    let flags = security::read_basic_security_header(&mut payload).unwrap();
    assert_eq!(flags & security::SEC_INFO_PKT, security::SEC_INFO_PKT);
    let plain = sec_layer.open(payload).unwrap();
    let info_flags = u32::from_le_bytes([plain[4], plain[5], plain[6], plain[7]]);
    assert_ne!(info_flags & 0x10, 0, "INFO_UNICODE must be set");

    // 7. Licensing: server says "valid client", client replies.
    let license_pdu =
        license::license_error_pdu(license::STATUS_VALID_CLIENT, license::ST_NO_TRANSITION);
    server_send_encrypted(&mut s, &mut sec_layer, 1001, 1003, &license_pdu);
    let reply = s.recv().unwrap();
    let parsed = mcs::parse_send_data_request(&reply).unwrap();
    assert_eq!(parsed.channel_id, 1003);
    let mut payload = &parsed.data[..];
    let _flags = security::read_basic_security_header(&mut payload).unwrap();
    let plain = sec_layer.open(payload).unwrap();
    let preamble = license::parse_preamble(&plain).unwrap();
    assert_eq!(preamble.msg_type, license::ERROR_ALERT);

    // 8. Capability exchange: Demand Active → Confirm Active + Sync + Controls.
    let caps_bytes = caps::all_caps(1280, 800);
    let mut body = Vec::new();
    body.extend_from_slice(&0x0011u16.to_le_bytes()); // inner share control header
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&7u16.to_le_bytes()); // number of caps
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&caps_bytes);
    body.extend_from_slice(&0u32.to_le_bytes()); // sessionId
    let mut demand = Vec::new();
    caps::write_share_control_header(
        caps::PDUTYPE_DEMAND_ACTIVE,
        1002,
        12 + body.len(),
        &mut demand,
    );
    caps::write_share_data_header(SHARE_ID, 0x11, body.len(), &mut demand);
    demand.extend_from_slice(&body);
    server_send_encrypted(&mut s, &mut sec_layer, 1001, 1003, &demand);

    // Confirm Active.
    let confirm = s.recv().unwrap();
    let (confirm_flags, confirm_plain) = recv_io(&mut sec_layer, &confirm);
    assert_ne!(confirm_flags & security::SEC_ENCRYPT, 0);
    let mut cur = &confirm_plain[..];
    let (pdu_type, _) = caps::read_share_control_header(&mut cur).unwrap();
    assert_eq!(pdu_type, caps::PDUTYPE_CONFIRM_ACTIVE);
    let (share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(share_id, SHARE_ID);
    assert_eq!(pdu_type2, 0x13);

    // Synchronize.
    let sync = s.recv().unwrap();
    let (_, sync_plain) = recv_io(&mut sec_layer, &sync);
    let mut cur = &sync_plain[..];
    let _ = caps::read_share_control_header(&mut cur).unwrap();
    let (share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(share_id, SHARE_ID);
    assert_eq!(pdu_type2, caps::PDUTYPE2_SYNCHRONIZE);

    // Control Cooperate, then Control Request.
    let coop = s.recv().unwrap();
    let (_, coop_plain) = recv_io(&mut sec_layer, &coop);
    assert_control_action(&coop_plain, caps::CONTROL_COOPERATE);
    let req = s.recv().unwrap();
    let (_, req_plain) = recv_io(&mut sec_layer, &req);
    assert_control_action(&req_plain, caps::CONTROL_REQUEST);

    // Control Confirm closes the sequence.
    let mut confirm_ctl = Vec::new();
    caps::write_share_control_header(caps::PDUTYPE_DATA, 1002, 12 + 6, &mut confirm_ctl);
    caps::write_share_data_header(SHARE_ID, caps::PDUTYPE2_CONTROL, 6, &mut confirm_ctl);
    confirm_ctl.extend_from_slice(&caps::CONTROL_CONFIRM.to_le_bytes());
    confirm_ctl.extend_from_slice(&0u16.to_le_bytes());
    confirm_ctl.extend_from_slice(&1u16.to_le_bytes());
    server_send_encrypted(&mut s, &mut sec_layer, 1001, 1003, &confirm_ctl);

    // 9. Post-connect traffic: read the client's input PDU, echo an update.
    let input = s.recv().unwrap();
    let (input_flags, input_plain) = recv_io(&mut sec_layer, &input);
    assert_ne!(input_flags & security::SEC_ENCRYPT, 0);
    let mut cur = &input_plain[..];
    let _ = caps::read_share_control_header(&mut cur).unwrap();
    let (_share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(pdu_type2, caps::PDUTYPE2_INPUT);

    let mut update = Vec::new();
    caps::write_share_control_header(caps::PDUTYPE_DATA, 1002, 12 + 8, &mut update);
    caps::write_share_data_header(SHARE_ID, caps::PDUTYPE2_UPDATE, 8, &mut update);
    update.extend_from_slice(&[0u8; 8]); // dummy update payload
    server_send_encrypted(&mut s, &mut sec_layer, 1001, 1003, &update);
}

fn assert_control_action(plain: &[u8], expected_action: u16) {
    let mut cur = plain;
    let _ = caps::read_share_control_header(&mut cur).unwrap();
    let (share_id, pdu_type2) = caps::read_share_data_header(&mut cur).unwrap();
    assert_eq!(share_id, SHARE_ID);
    assert_eq!(pdu_type2, caps::PDUTYPE2_CONTROL);
    let action = u16::from_le_bytes([cur[0], cur[1]]);
    assert_eq!(action, expected_action);
}

// --- tests -----------------------------------------------------------------

#[test]
fn handshake_scripted_against_fake_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expected = 4; // cliprdr, rdpdr, rdpsnd, drdynvc
    let server = thread::spawn(move || fake_server(listener, expected));

    let opts = ConnectOptions {
        host: "127.0.0.1".into(),
        port,
        username: "alice".into(),
        password: "hunter2".into(),
        width: 1280,
        height: 800,
        color_depth: 24,
        client_name: "test-client".into(),
        insecure: true,
        clipboard: true,
        drive_redirection: true,
        audio_playback: true,
        printer: true,
        ..Default::default()
    };

    let mut session = WireSession::connect(opts).unwrap();
    let settings = session.settings();
    assert_eq!(settings.desktop_width, 1280);
    assert_eq!(settings.desktop_height, 800);
    assert_eq!(settings.io_channel, 1003);
    assert_eq!(settings.user_channel, 1001);
    assert_eq!(settings.share_id, SHARE_ID);
    assert_eq!(settings.selected_protocol, 0);
    assert!(session.insecure());
    let names: Vec<&str> = settings
        .static_channels
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["cliprdr", "rdpdr", "rdpsnd", "drdynvc"]);
    assert_eq!(settings.static_channels[0].id, 1004);

    // Post-connect traffic still works: an input PDU round-trips.
    session
        .send_input(&[wire_main::InputEvent::Mouse {
            flags: wire_main::ptr::MOVE,
            x: 10,
            y: 20,
        }])
        .unwrap();
    assert!(matches!(session.recv().unwrap(), ServerPdu::Update(_)));

    server.join().unwrap();
}

#[test]
fn handshake_rejects_missing_insecure_flag() {
    let opts = ConnectOptions {
        host: "127.0.0.1".into(),
        port: 3389,
        insecure: false,
        ..Default::default()
    };
    let err = match WireSession::connect(opts) {
        Err(e) => e,
        Ok(_) => panic!("expected connect to fail without --insecure"),
    };
    assert!(err.to_string().contains("--insecure"));
}
