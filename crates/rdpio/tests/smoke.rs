//! Smoke integration test: assemble the full client from the workspace crates
//! and drive the RDP connection setup over an **in-memory transport**.
//!
//! The test constructs [`rdpio::Client`] (the assembly crate's constructor
//! glue over `rdp-core`'s sans-I/O `Connector`), connects it to a scripted
//! server through a pure in-memory duplex pipe (no sockets, no TLS), and
//! asserts the handshake *advances* — the X.224 negotiation completes and the
//! state machine moves on instead of failing at the transport boundary.

use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use rdp_pdu::x224::{self, SecurityProtocol};

use rdpio::{Client, ClientConfig, ClientError, CoreError, Credentials, Phase};

// ---------------------------------------------------------------------------
// In-memory transport
// ---------------------------------------------------------------------------

/// One direction of an in-memory pipe: a byte queue that [`Read`] blocks on
/// until data arrives.
#[derive(Clone)]
struct MemPipe {
    buf: Arc<(Mutex<Vec<u8>>, Condvar)>,
}

impl MemPipe {
    fn new() -> Self {
        Self {
            buf: Arc::new((Mutex::new(Vec::new()), Condvar::new())),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let (lock, cvar) = &*self.buf;
        let mut buf = lock.lock().unwrap();
        buf.extend_from_slice(bytes);
        cvar.notify_all();
    }

    fn pop(&self, out: &mut [u8]) -> usize {
        let (lock, cvar) = &*self.buf;
        let mut buf = lock.lock().unwrap();
        while buf.is_empty() {
            buf = cvar.wait(buf).unwrap();
        }
        let n = out.len().min(buf.len());
        for (dst, src) in out.iter_mut().zip(buf.drain(..n)) {
            *dst = src;
        }
        n
    }
}

/// A full-duplex in-memory transport: two crossed [`MemPipe`]s. What one end
/// writes is exactly what the other end reads — a real byte transport, just
/// without a socket.
struct MemTransport {
    inbound: MemPipe,
    outbound: MemPipe,
}

impl MemTransport {
    fn pair() -> (Self, Self) {
        let a_to_b = MemPipe::new();
        let b_to_a = MemPipe::new();
        (
            Self {
                inbound: b_to_a.clone(),
                outbound: a_to_b.clone(),
            },
            Self {
                inbound: a_to_b,
                outbound: b_to_a,
            },
        )
    }
}

impl Read for MemTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.inbound.pop(buf))
    }
}

impl Write for MemTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.outbound.push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Scripted server side
// ---------------------------------------------------------------------------

/// Play the server side of the X.224 negotiation on `transport`: read the
/// client's TPKT-framed Connection Request, verify it is one, and reply with
/// the given TPKT-framed Connection Confirm. Returns the request bytes that
/// crossed the pipe, so the caller can assert on what the client sent.
fn serve_negotiation(mut transport: MemTransport, confirm: &'static [u8]) -> Vec<u8> {
    let mut header = [0u8; x224::TPKT_HEADER_LEN];
    transport.read_exact(&mut header).unwrap();
    let total = x224::read_tpkt_len(&header).unwrap();
    assert!(
        total >= x224::TPKT_HEADER_LEN,
        "client must send a TPKT-framed PDU"
    );
    let mut request = vec![0u8; total - x224::TPKT_HEADER_LEN];
    transport.read_exact(&mut request).unwrap();
    // request[0] is the X.224 length indicator; request[1] is the TPDU type
    // (high nibble 0xE = Connection Request).
    assert_eq!(
        request[1] & 0xf0,
        0xe0,
        "client must send an X.224 Connection Request"
    );
    transport.write_all(confirm).unwrap();
    request
}

// Full TPKT frames of X.224 Connection Confirms (MS-RDPBCGR 2.2.1.2).
// TPKT(03 00 00 13) LI(0e) CC(d0) DST(0000) SRC(0000) CLASS(00) + 8-byte
// RDP negotiation structure.

/// RDP_NEG_RSP selecting PROTOCOL_HYBRID (NLA): `02 00 08 00 02 00 00 00`.
const CC_HYBRID: &[u8] = &[
    0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00, 0x02,
    0x00, 0x00, 0x00,
];

/// RDP_NEG_RSP selecting PROTOCOL_RDP (0) — Standard RDP Security:
/// `02 00 08 00 00 00 00 00`.
const CC_STANDARD: &[u8] = &[
    0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00, 0x00,
    0x00, 0x00, 0x00,
];

/// RDP_NEG_FAILURE, failureCode = 2 (SSL_NOT_ALLOWED_BY_SERVER):
/// `03 00 08 00 02 00 00 00`.
const CC_FAILURE: &[u8] = &[
    0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x02,
    0x00, 0x00, 0x00,
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The client's connection setup completes over the in-memory transport: the
/// X.224 Connection Request crosses the pipe, the server's Connection Confirm
/// comes back, and the handshake advances from Negotiation to Authentication
/// (NLA) instead of failing at the transport boundary.
#[test]
fn handshake_advances_past_the_transport_boundary() {
    let config = ClientConfig {
        hostname: "host.example".into(),
        credentials: Credentials {
            username: "alice".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut client = Client::new(config);
    assert_eq!(client.phase(), Phase::Negotiation);

    let (mut client_end, server_end) = MemTransport::pair();
    let server = thread::spawn(move || serve_negotiation(server_end, CC_HYBRID));

    let selected = client.drive_negotiation(&mut client_end).unwrap();
    assert_eq!(selected, SecurityProtocol::HYBRID);
    // The state machine advanced past negotiation into NLA: the handshake did
    // not stop at the transport boundary.
    assert_eq!(client.phase(), Phase::Authentication);

    // The request really crossed the pipe: the server side saw a TPKT-framed
    // Connection Request advertising SSL | HYBRID with the mstshash cookie.
    let request = server.join().unwrap();
    assert_eq!(
        u32::from_le_bytes(request[request.len() - 4..].try_into().unwrap()),
        0x03, // SSL | HYBRID
        "client must advertise TLS + CredSSP"
    );
    assert!(
        String::from_utf8_lossy(&request).contains("Cookie: mstshash=alice"),
        "client must send the username as the mstshash cookie"
    );
}

/// A `--legacy` client (Standard RDP Security only) advertises protocol 0 and
/// advances straight to the MCS Basic Settings phase once the server confirms.
#[test]
fn legacy_selection_advances_to_basic_settings() {
    let mut client = Client::new(ClientConfig {
        force_legacy: true,
        ..Default::default()
    });

    let (mut client_end, server_end) = MemTransport::pair();
    let server = thread::spawn(move || serve_negotiation(server_end, CC_STANDARD));

    let selected = client.drive_negotiation(&mut client_end).unwrap();
    assert_eq!(selected, SecurityProtocol::empty());
    assert_eq!(client.phase(), Phase::BasicSettings);

    let request = server.join().unwrap();
    assert_eq!(
        u32::from_le_bytes(request[request.len() - 4..].try_into().unwrap()),
        0, // PROTOCOL_RDP (Standard RDP Security)
        "legacy client must not advertise TLS/NLA"
    );
}

/// A server rejection is surfaced at the protocol layer, not as a transport
/// error: the failure frame crossed the in-memory transport and was decoded
/// into a negotiation rejection.
#[test]
fn negotiation_rejection_surfaces_at_protocol_layer() {
    let mut client = Client::new(ClientConfig::default());

    let (mut client_end, server_end) = MemTransport::pair();
    let server = thread::spawn(move || serve_negotiation(server_end, CC_FAILURE));

    let err = client.drive_negotiation(&mut client_end).unwrap_err();
    assert!(
        matches!(err, ClientError::Core(CoreError::NegotiationRejected(_))),
        "expected a protocol-layer negotiation rejection, got {err}"
    );
    // The connector stays in negotiation, ready for a fallback retry
    // (e.g. legacy Standard RDP Security).
    assert_eq!(client.phase(), Phase::Negotiation);
    server.join().unwrap();
}
