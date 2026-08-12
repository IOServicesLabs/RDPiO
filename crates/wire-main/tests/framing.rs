//! Integration tests for the wire-main transport: TPKT framing round-trips
//! over an in-process TCP pair, and the UDP side-band socket.

use std::net::{TcpListener, UdpSocket};
use std::thread;

use wire_main::pdu::{mcs, x224};
use wire_main::transport::{TransportOptions, UdpFrame, UdpSideband, WireTransport, TPKT_VERSION};

fn pair() -> (WireTransport, WireTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_thread = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        WireTransport::from_stream(stream, true).unwrap()
    });
    let client = WireTransport::connect(TransportOptions {
        host: "127.0.0.1".into(),
        port: addr.port(),
        insecure: true,
        ..Default::default()
    })
    .unwrap();
    let server = server_thread.join().unwrap();
    (client, server)
}

#[test]
fn transport_tpkt_roundtrip_both_directions() {
    let (mut client, mut server) = pair();

    let body = b"hello from client";
    client.send(body).unwrap();
    let got = server.recv().unwrap();
    assert_eq!(got, body);

    let reply = b"hello from server";
    server.send(reply).unwrap();
    let got = client.recv().unwrap();
    assert_eq!(got, reply);
}

#[test]
fn transport_frames_are_length_prefixed() {
    let (mut client, mut server) = pair();

    // Two back-to-back frames must arrive as two distinct frames.
    client.send(b"first").unwrap();
    client.send(b"second").unwrap();
    assert_eq!(server.recv().unwrap(), b"first");
    assert_eq!(server.recv().unwrap(), b"second");
}

#[test]
fn transport_recv_frame_includes_header() {
    let (mut client, mut server) = pair();
    client.send(b"abcd").unwrap();
    let frame = server.recv_frame().unwrap();
    assert_eq!(frame[0], TPKT_VERSION);
    assert_eq!(
        u16::from_be_bytes([frame[2], frame[3]]) as usize,
        frame.len()
    );
    assert_eq!(&frame[4..], b"abcd");
}

#[test]
fn transport_oversized_frame_rejected() {
    let (mut client, _server) = pair();
    let huge = vec![0u8; u16::MAX as usize];
    let err = client.send(&huge).unwrap_err();
    assert!(err.to_string().contains("too large"));
}

#[test]
fn transport_mcs_data_roundtrip_framing() {
    let (mut client, mut server) = pair();

    // Send a fake "Send Data Request" the way the session layer would.
    let payload = vec![0xde, 0xad, 0xbe, 0xef];
    let req = mcs::send_data_request(1001, 1003, &payload);
    let mut body = Vec::new();
    x224::write_data_header(req.len(), &mut body).unwrap();
    body.extend_from_slice(&req);
    client.send(&body).unwrap();

    let frame = server.recv().unwrap();
    let parsed = mcs::parse_send_data_request(&frame).unwrap();
    assert_eq!(parsed.channel_id, 1003);
    assert_eq!(parsed.data, payload);
}

#[test]
fn transport_udp_sideband_sends_datagrams() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sink.local_addr().unwrap();
    let sideband =
        UdpSideband::connect("127.0.0.1", addr.port(), std::time::Duration::from_secs(2)).unwrap();
    sideband.send(b"ping").unwrap();

    let mut buf = [0u8; 64];
    let (n, _from) = sink.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping");
}

#[test]
fn transport_udp_sideband_sequence_framing() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sink.local_addr().unwrap();
    let mut sideband =
        UdpSideband::connect("127.0.0.1", addr.port(), std::time::Duration::from_secs(2)).unwrap();

    let seq = sideband.send_data(b"frame-0").unwrap();
    assert_eq!(seq, 0);
    let seq = sideband.send_data(b"frame-1").unwrap();
    assert_eq!(seq, 1);

    // The wire carries a 10-byte framed header, not the raw payload.
    let mut buf = [0u8; 128];
    let (n, _from) = sink.recv_from(&mut buf).unwrap();
    let frame = UdpFrame::parse(&buf[..n]).unwrap();
    assert_eq!(frame.seq, 0);
    assert_eq!(frame.payload, b"frame-0");
    let (n, _from) = sink.recv_from(&mut buf).unwrap();
    let frame = UdpFrame::parse(&buf[..n]).unwrap();
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.payload, b"frame-1");
}

#[test]
fn transport_udp_sideband_ack_roundtrip() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sink.local_addr().unwrap();
    let sideband =
        UdpSideband::connect("127.0.0.1", addr.port(), std::time::Duration::from_secs(2)).unwrap();
    sideband.send_ack(9).unwrap();

    let mut buf = [0u8; 64];
    let (n, _from) = sink.recv_from(&mut buf).unwrap();
    let frame = UdpFrame::parse(&buf[..n]).unwrap();
    assert!(frame.is_ack());
    assert_eq!(frame.seq, 9);
    assert!(frame.payload.is_empty());
}

/// An out-of-order datagram is held and only delivered once the gap fills;
/// the receiver's reassembler hands the batch back in sequence order.
#[test]
fn transport_udp_sideband_reassembles_out_of_order() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sink.local_addr().unwrap();
    let mut sideband =
        UdpSideband::connect("127.0.0.1", addr.port(), std::time::Duration::from_secs(2)).unwrap();
    let client_addr = sideband.local_addr().unwrap();

    // The peer sends seq 1 before seq 0 (encoded by hand).
    let late = UdpFrame::data(1, b"b".to_vec()).encode();
    let early = UdpFrame::data(0, b"a".to_vec()).encode();
    sink.send_to(&late, client_addr).unwrap();
    sink.send_to(&early, client_addr).unwrap();

    assert_eq!(
        sideband.recv_frame().unwrap(),
        wire_main::UdpRecv::Held { seq: 1, missing: 1 }
    );
    assert_eq!(
        sideband.recv_frame().unwrap(),
        wire_main::UdpRecv::Payloads(vec![b"a".to_vec(), b"b".to_vec()])
    );
    let stats = sideband.stats();
    assert_eq!(stats.delivered, 2);
    assert_eq!(stats.lost, 1);
    assert_eq!(stats.held, 0);
}

#[test]
fn transport_udp_sideband_drops_duplicates() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sink.local_addr().unwrap();
    let mut sideband =
        UdpSideband::connect("127.0.0.1", addr.port(), std::time::Duration::from_secs(2)).unwrap();
    let client_addr = sideband.local_addr().unwrap();

    let datagram = UdpFrame::data(0, b"a".to_vec()).encode();
    sink.send_to(&datagram, client_addr).unwrap();
    sink.send_to(&datagram, client_addr).unwrap();

    assert_eq!(
        sideband.recv_frame().unwrap(),
        wire_main::UdpRecv::Payloads(vec![b"a".to_vec()])
    );
    assert_eq!(
        sideband.recv_frame().unwrap(),
        wire_main::UdpRecv::Duplicate(0)
    );
    let stats = sideband.stats();
    assert_eq!(stats.duplicates, 1);
    assert_eq!(stats.delivered, 1);
}
