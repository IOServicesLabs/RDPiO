//! TCP transport with TPKT framing plus the optional RDP-UDP side-band socket.
//!
//! Every RDP PDU after (and including) the X.224 layer travels inside a TPKT
//! frame: a 4-byte header (`03 00 <len-be>`) whose length covers the whole
//! frame including the header. [`WireTransport`] owns the socket and turns
//! length-prefixed TPKT frames into plain byte slices for the codec layers.
//!
//! The optional UDP side-band ([`UdpSideband`]) is a low-latency companion
//! socket: datagrams carry a 10-byte frame header with a 32-bit sequence
//! number ([`UdpFrame`]), so the receiver can detect loss and reorder
//! out-of-order arrivals ([`UdpSequencer`]) instead of handing the payload
//! stream a corrupted byte sequence.
//!
//! The `--insecure` flag is honored here: [`TransportOptions::insecure`]
//! selects Standard RDP Security (RC4, no TLS). When it is `false` the
//! transport refuses to connect, because this crate deliberately implements
//! the RDP protocol from scratch and does not vendor a TLS stack — the
//! workspace's TLS-capable path lives in `rdp-client`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use crate::error::{WireError, WireResult};

/// Size of the TPKT header: version, reserved, length (big-endian).
pub const TPKT_HEADER_LEN: usize = 4;
/// TPKT version byte, always 3 for RDP.
pub const TPKT_VERSION: u8 = 3;

/// Magic bytes prefixing every framed UDP side-band datagram (`"RD"`).
pub const UDP_FRAME_MAGIC: u16 = 0x5244;
/// Version byte of the UDP side-band framing.
pub const UDP_FRAME_VERSION: u8 = 1;
/// Size of the fixed UDP side-band frame header: magic(2) version(1) flags(1)
/// sequence(4) length(2), all big-endian.
pub const UDP_FRAME_HEADER_LEN: usize = 10;
/// Largest payload one framed datagram can carry (UDP max datagram minus the
/// frame header).
pub const UDP_MAX_PAYLOAD: usize = 65507 - UDP_FRAME_HEADER_LEN;

/// Flag bits in a [`UdpFrame`] header.
pub mod udp_flags {
    /// The datagram carries source data in its payload.
    pub const DATA: u8 = 0x01;
    /// The datagram is an acknowledgement (sequence number of the peer's data
    /// being acked; no payload).
    pub const ACK: u8 = 0x02;
}

/// Options controlling how [`WireTransport::connect`] opens the connection.
#[derive(Debug, Clone)]
pub struct TransportOptions {
    /// Remote host name or IP literal.
    pub host: String,
    /// Remote TCP port (default 3389).
    pub port: u16,
    /// Honor `--insecure`: use Standard RDP Security instead of TLS.
    /// `false` is rejected by the transport with a clear error.
    pub insecure: bool,
    /// Also bind a UDP socket for the low-latency side-band (RDP-UDP
    /// multitransport). The side-band is managed but data still flows over
    /// TCP until the server negotiates multitransport.
    pub udp: bool,
    /// Timeout for the initial TCP connect.
    pub connect_timeout: Duration,
    /// Read timeout applied to the TCP stream (None = blocking).
    pub recv_timeout: Option<Duration>,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 3389,
            insecure: true,
            udp: false,
            connect_timeout: Duration::from_secs(10),
            recv_timeout: Some(Duration::from_secs(30)),
        }
    }
}

/// One datagram on the UDP side-band: a 10-byte header plus the payload.
///
/// Wire format (all multi-byte fields big-endian):
///
/// ```text
/// +--------+---------+-------+------------+--------+------------------+
/// | magic  | version | flags | sequence   | length | payload          |
/// | u16    | u8      | u8    | u32        | u16    | length bytes     |
/// +--------+---------+-------+------------+--------+------------------+
/// ```
///
/// DATA frames carry `payload`; ACK frames carry no payload and echo the
/// sequence number being acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpFrame {
    /// Monotonic (wrapping) sequence number of this datagram.
    pub seq: u32,
    /// `udp_flags::*` bitmask.
    pub flags: u8,
    /// Payload bytes (empty for ACK frames).
    pub payload: Vec<u8>,
}

impl UdpFrame {
    /// Build a DATA frame carrying `payload` at sequence number `seq`.
    pub fn data(seq: u32, payload: Vec<u8>) -> Self {
        Self {
            seq,
            flags: udp_flags::DATA,
            payload,
        }
    }

    /// Build an ACK frame acknowledging sequence number `seq`.
    pub fn ack(seq: u32) -> Self {
        Self {
            seq,
            flags: udp_flags::ACK,
            payload: Vec::new(),
        }
    }

    /// Whether this frame is an acknowledgement (no payload).
    pub fn is_ack(&self) -> bool {
        self.flags & udp_flags::ACK != 0
    }

    /// Encode the frame into its on-wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(UDP_FRAME_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&UDP_FRAME_MAGIC.to_be_bytes());
        out.push(UDP_FRAME_VERSION);
        out.push(self.flags);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse a datagram from the front of `data`. Returns `None` if the magic,
    /// version, or declared length does not match.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < UDP_FRAME_HEADER_LEN {
            return None;
        }
        if u16::from_be_bytes([data[0], data[1]]) != UDP_FRAME_MAGIC {
            return None;
        }
        if data[2] != UDP_FRAME_VERSION {
            return None;
        }
        let len = u16::from_be_bytes([data[8], data[9]]) as usize;
        let payload = data
            .get(UDP_FRAME_HEADER_LEN..UDP_FRAME_HEADER_LEN + len)?
            .to_vec();
        Some(Self {
            seq: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            flags: data[3],
            payload,
        })
    }
}

/// Outcome of feeding a data frame to a [`UdpSequencer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpDelivery {
    /// One or more payloads delivered in sequence order. A gap fill can
    /// deliver several at once (the held datagrams that became contiguous).
    Delivered(Vec<Vec<u8>>),
    /// `seq` arrived but `missing` datagrams precede it; it is held until the
    /// gap fills (the sender retransmits) or the buffer overflows.
    Held { seq: u32, missing: u32 },
    /// Already delivered — a duplicate (the sender's ACK was lost).
    Duplicate(u32),
}

/// Sans-I/O ordered reassembly for the UDP side-band.
///
/// The side-band payload stream feeds a byte stream that one missing or
/// reordered datagram would desynchronize permanently, so delivery is strictly
/// in order: out-of-order frames are held in a bounded buffer, loss is
/// counted, and duplicates are dropped. Pure — no sockets — so it is
/// deterministic and unit-testable without a network.
#[derive(Debug, Default)]
pub struct UdpSequencer {
    /// The next source sequence number expected in order.
    next_expected: u32,
    /// Out-of-order frames held until the gap before them fills (bounded).
    held: BTreeMap<u32, Vec<u8>>,
    /// Payloads delivered in order.
    delivered: u64,
    /// Gaps detected (frames that never arrived, or arrived too late).
    lost: u64,
    /// Duplicate frames dropped.
    duplicates: u64,
}

/// Cap on held out-of-order frames. Past this the sender has stalled far
/// beyond any sane window and the link is effectively dead.
const MAX_HELD: usize = 256;

impl UdpSequencer {
    /// A fresh sequencer expecting sequence number 0 first.
    pub fn new() -> Self {
        Self::with_next(0)
    }

    /// A sequencer expecting `first` as the first in-order sequence number
    /// (useful after a SYN/ISN handshake negotiated a starting point).
    pub fn with_next(first: u32) -> Self {
        Self {
            next_expected: first,
            ..Self::default()
        }
    }

    /// The next sequence number expected in order.
    pub fn next_expected(&self) -> u32 {
        self.next_expected
    }

    /// Feed one data frame. Returns what became deliverable (or why not).
    pub fn push(&mut self, frame: UdpFrame) -> UdpDelivery {
        let seq = frame.seq;
        let ahead = seq.wrapping_sub(self.next_expected);
        if ahead >= 0x8000_0000 {
            // seq < next_expected: already delivered (our ACK was lost).
            self.duplicates += 1;
            return UdpDelivery::Duplicate(seq);
        }
        if ahead == 0 {
            // In order: deliver it, then drain everything the hold buffer now
            // makes contiguous.
            let mut batch = vec![frame.payload];
            self.delivered += 1;
            let mut next = self.next_expected.wrapping_add(1);
            while let Some(payload) = self.held.remove(&next) {
                batch.push(payload);
                self.delivered += 1;
                next = next.wrapping_add(1);
            }
            self.next_expected = next;
            return UdpDelivery::Delivered(batch);
        }
        // A gap: hold the frame (once) until retransmission fills the hole.
        if !self.held.contains_key(&seq) {
            self.lost += 1;
            if self.held.len() < MAX_HELD {
                self.held.insert(seq, frame.payload);
            }
        }
        UdpDelivery::Held {
            seq,
            missing: ahead,
        }
    }

    /// Payloads delivered in order so far.
    pub fn delivered_count(&self) -> u64 {
        self.delivered
    }

    /// Gaps detected so far (frames missing at the time their successor
    /// arrived, or never arriving at all).
    pub fn lost_count(&self) -> u64 {
        self.lost
    }

    /// Duplicate frames dropped.
    pub fn duplicate_count(&self) -> u64 {
        self.duplicates
    }

    /// How many out-of-order frames are currently held.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }
}

/// Outcome of one [`UdpSideband::recv_frame`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpRecv {
    /// One or more payloads delivered in sequence order.
    Payloads(Vec<Vec<u8>>),
    /// A data frame arrived out of order and is held for reassembly.
    Held { seq: u32, missing: u32 },
    /// A duplicate of an already-delivered frame.
    Duplicate(u32),
    /// An acknowledgement frame from the peer.
    Ack(u32),
}

/// Snapshot of side-band accounting, for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpStats {
    /// Sequence number the next outbound data frame will carry.
    pub send_seq: u32,
    /// Next inbound sequence number expected in order.
    pub next_expected: u32,
    /// Payloads delivered in order.
    pub delivered: u64,
    /// Gaps detected.
    pub lost: u64,
    /// Duplicate frames dropped.
    pub duplicates: u64,
    /// Out-of-order frames currently held.
    pub held: usize,
}

/// Low-latency UDP side-band socket (RDP-UDP / multitransport).
///
/// The RDP server moves the real-time graphics channel onto this socket once
/// multitransport is negotiated; the TCP stream keeps carrying everything else.
/// Every outbound data datagram is framed with a sequence number
/// ([`UdpFrame`]); inbound datagrams are reassembled strictly in order by an
/// internal [`UdpSequencer`], with loss counted and duplicates dropped.
#[derive(Debug)]
pub struct UdpSideband {
    socket: UdpSocket,
    peer: SocketAddr,
    /// Sequence number for the next outbound data frame.
    send_seq: u32,
    /// Inbound ordered reassembly.
    sequencer: UdpSequencer,
}

impl UdpSideband {
    /// Bind an ephemeral local UDP socket and connect it to `host`'s RDP-UDP
    /// port (the same TCP port). Connected mode filters stray datagrams.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> WireResult<Self> {
        let peer = resolve(host, port)?;
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.connect(peer)?;
        socket.set_read_timeout(Some(timeout))?;
        Ok(Self {
            socket,
            peer,
            send_seq: 0,
            sequencer: UdpSequencer::new(),
        })
    }

    /// Send a raw datagram without framing (used by the handshake probes and
    /// by tests that want to inject frames with explicit sequence numbers).
    pub fn send(&self, payload: &[u8]) -> WireResult<usize> {
        Ok(self.socket.send(payload)?)
    }

    /// Frame `payload` with the next sequence number and send it. Returns the
    /// sequence number the datagram carried (the peer acknowledges it).
    pub fn send_data(&mut self, payload: &[u8]) -> WireResult<u32> {
        if payload.len() > UDP_MAX_PAYLOAD {
            return Err(WireError::Protocol(format!(
                "UDP payload too large: {} bytes (max {UDP_MAX_PAYLOAD})",
                payload.len()
            )));
        }
        let seq = self.send_seq;
        let frame = UdpFrame::data(seq, payload.to_vec());
        self.socket.send(&frame.encode())?;
        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(seq)
    }

    /// Send an ACK frame acknowledging the peer's data frame `ack_seq`.
    pub fn send_ack(&self, ack_seq: u32) -> WireResult<()> {
        let frame = UdpFrame::ack(ack_seq);
        self.socket.send(&frame.encode())?;
        Ok(())
    }

    /// Receive one datagram: parse its frame, ACK it if it is an
    /// acknowledgement, otherwise feed it to the reassembly sequencer.
    pub fn recv(&self, buf: &mut [u8]) -> WireResult<usize> {
        Ok(self.socket.recv(buf)?)
    }

    /// Receive one datagram and run it through the frame parser and the
    /// in-order reassembler. `Payloads` may hold several payloads when a gap
    /// fill makes previously held datagrams contiguous.
    pub fn recv_frame(&mut self) -> WireResult<UdpRecv> {
        let mut buf = [0u8; 65536];
        let n = self.socket.recv(&mut buf)?;
        let frame = UdpFrame::parse(&buf[..n]).ok_or_else(|| {
            WireError::Protocol(format!("malformed UDP side-band datagram ({} bytes)", n))
        })?;
        if frame.is_ack() {
            return Ok(UdpRecv::Ack(frame.seq));
        }
        Ok(match self.sequencer.push(frame) {
            UdpDelivery::Delivered(batch) => UdpRecv::Payloads(batch),
            UdpDelivery::Held { seq, missing } => UdpRecv::Held { seq, missing },
            UdpDelivery::Duplicate(seq) => UdpRecv::Duplicate(seq),
        })
    }

    /// Side-band accounting snapshot.
    pub fn stats(&self) -> UdpStats {
        UdpStats {
            send_seq: self.send_seq,
            next_expected: self.sequencer.next_expected(),
            delivered: self.sequencer.delivered_count(),
            lost: self.sequencer.lost_count(),
            duplicates: self.sequencer.duplicate_count(),
            held: self.sequencer.held_len(),
        }
    }

    /// The connected peer address.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The local address the side-band socket is bound to (the address the
    /// peer should send datagrams to).
    pub fn local_addr(&self) -> WireResult<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }
}

fn resolve(host: &str, port: u16) -> WireResult<SocketAddr> {
    let mut addrs = (host, port).to_socket_addrs()?;
    addrs
        .next()
        .ok_or_else(|| WireError::Protocol(format!("cannot resolve host `{host}`")))
}

/// A connected RDP transport: one TCP stream with TPKT framing, plus an
/// optional UDP side-band.
#[derive(Debug)]
pub struct WireTransport {
    stream: TcpStream,
    peer: SocketAddr,
    udp: Option<UdpSideband>,
    insecure: bool,
}

impl WireTransport {
    /// Resolve `opts.host`, open the TCP connection, and apply socket tuning
    /// (Nagle off for latency, read timeout, keepalive).
    pub fn connect(opts: TransportOptions) -> WireResult<Self> {
        if !opts.insecure {
            return Err(WireError::Unsupported(
                "secure (TLS/NLA) transport is not implemented in wire-main; \
                 pass --insecure to use Standard RDP Security"
                    .into(),
            ));
        }
        if opts.host.is_empty() {
            return Err(WireError::Protocol("empty host".into()));
        }
        let peer = resolve(&opts.host, opts.port)?;
        let stream = TcpStream::connect_timeout(&peer, opts.connect_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(opts.recv_timeout)?;
        // NOTE: `TcpStream::set_keepalive` is still gated behind the unstable
        // `tcp_keepalive` feature on the pinned stable toolchain, so TCP
        // keepalive is deliberately not enabled here. Keepalive is a
        // nice-to-have for NAT pinholes; RDP has its own ping/pong frames
        // once the session is up, and the UDP side-band covers low-latency
        // traffic. Revisit when `tcp_keepalive` stabilizes.

        let udp = if opts.udp {
            Some(UdpSideband::connect(
                &opts.host,
                opts.port,
                opts.connect_timeout,
            )?)
        } else {
            None
        };

        tracing::debug!(%peer, udp = opts.udp, "transport connected");
        Ok(Self {
            stream,
            peer,
            udp,
            insecure: opts.insecure,
        })
    }

    /// Wrap an already-accepted TCP stream (used by the in-process fake server
    /// in integration tests). Applies the same socket tuning as `connect`.
    pub fn from_stream(stream: TcpStream, insecure: bool) -> WireResult<Self> {
        let peer = stream.peer_addr()?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            stream,
            peer,
            udp: None,
            insecure,
        })
    }

    /// Wrap `tpkt_body` (everything after the 4-byte TPKT header) in a TPKT
    /// frame and write it to the socket.
    pub fn send(&mut self, tpkt_body: &[u8]) -> WireResult<()> {
        let total = TPKT_HEADER_LEN + tpkt_body.len();
        if total > u16::MAX as usize {
            return Err(WireError::Protocol(format!(
                "TPKT frame too large: {total} bytes"
            )));
        }
        let mut frame = Vec::with_capacity(total);
        frame.push(TPKT_VERSION);
        frame.push(0x00);
        frame.extend_from_slice(&(total as u16).to_be_bytes());
        frame.extend_from_slice(tpkt_body);
        self.stream.write_all(&frame)?;
        Ok(())
    }

    /// Send raw bytes without TPKT framing (used by tests and the UDP path).
    pub fn send_raw(&mut self, bytes: &[u8]) -> WireResult<()> {
        self.stream.write_all(bytes)?;
        Ok(())
    }

    /// Read exactly one TPKT frame and return its body (after the header).
    /// Blocks until the whole frame arrives or the read timeout fires.
    pub fn recv(&mut self) -> WireResult<Vec<u8>> {
        let mut header = [0u8; TPKT_HEADER_LEN];
        self.read_exact(&mut header)?;
        if header[0] != TPKT_VERSION {
            return Err(WireError::Protocol(format!(
                "bad TPKT version byte {:#04x}",
                header[0]
            )));
        }
        let total = u16::from_be_bytes([header[2], header[3]]) as usize;
        if total < TPKT_HEADER_LEN {
            return Err(WireError::Protocol(format!(
                "TPKT length {total} shorter than header"
            )));
        }
        let mut body = vec![0u8; total - TPKT_HEADER_LEN];
        self.read_exact(&mut body)?;
        Ok(body)
    }

    /// Convenience: [`Self::recv`] but returns the full frame including header.
    pub fn recv_frame(&mut self) -> WireResult<Vec<u8>> {
        let body = self.recv()?;
        let mut frame = Vec::with_capacity(body.len() + TPKT_HEADER_LEN);
        frame.push(TPKT_VERSION);
        frame.push(0x00);
        frame.extend_from_slice(&((body.len() + TPKT_HEADER_LEN) as u16).to_be_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> WireResult<()> {
        match self.stream.read_exact(buf) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(WireError::Closed),
            Err(e) => Err(e.into()),
        }
    }

    /// Gracefully shut down the TCP stream and drop the UDP side-band.
    pub fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.udp = None;
    }

    /// Whether this transport runs Standard RDP Security (the `--insecure` path).
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    /// Change the socket read timeout (used to switch the session into
    /// poll mode after the connection sequence completes).
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> WireResult<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    /// The resolved peer address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Access the UDP side-band, if one was requested at connect time.
    pub fn udp(&mut self) -> Option<&mut UdpSideband> {
        self.udp.as_mut()
    }
}

impl Drop for WireTransport {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_flag_gates_transport() {
        let opts = TransportOptions {
            host: "127.0.0.1".into(),
            port: 1,
            insecure: false,
            ..Default::default()
        };
        let err = WireTransport::connect(opts).unwrap_err();
        assert!(matches!(err, WireError::Unsupported(_)));
    }

    #[test]
    fn empty_host_rejected() {
        let opts = TransportOptions {
            host: String::new(),
            insecure: true,
            ..Default::default()
        };
        assert!(matches!(
            WireTransport::connect(opts),
            Err(WireError::Protocol(_))
        ));
    }

    #[test]
    fn udp_frame_roundtrips() {
        let frame = UdpFrame::data(7, vec![0xde, 0xad, 0xbe, 0xef]);
        let bytes = frame.encode();
        assert_eq!(bytes.len(), UDP_FRAME_HEADER_LEN + 4);
        assert_eq!(UdpFrame::parse(&bytes).unwrap(), frame);
        assert!(!UdpFrame::parse(&bytes).unwrap().is_ack());
    }

    #[test]
    fn udp_frame_ack_has_no_payload() {
        let frame = UdpFrame::ack(42);
        assert!(frame.is_ack());
        assert!(frame.payload.is_empty());
        let bytes = frame.encode();
        assert_eq!(bytes.len(), UDP_FRAME_HEADER_LEN);
        let parsed = UdpFrame::parse(&bytes).unwrap();
        assert!(parsed.is_ack());
        assert_eq!(parsed.seq, 42);
    }

    #[test]
    fn udp_frame_rejects_malformed_datagrams() {
        assert!(UdpFrame::parse(&[]).is_none());
        assert!(UdpFrame::parse(&[0u8; 9]).is_none()); // truncated header
                                                       // Wrong magic.
        let mut bytes = UdpFrame::data(1, vec![0x01]).encode();
        bytes[0] = 0x00;
        assert!(UdpFrame::parse(&bytes).is_none());
        // Declared length longer than the datagram.
        let mut bytes = UdpFrame::data(1, vec![0x01]).encode();
        bytes[9] = 0x7f; // length = 0x7f01
        assert!(UdpFrame::parse(&bytes).is_none());
    }

    #[test]
    fn udp_sequencer_delivers_in_order() {
        let mut seq = UdpSequencer::new();
        assert_eq!(
            seq.push(UdpFrame::data(0, b"a".to_vec())),
            UdpDelivery::Delivered(vec![b"a".to_vec()])
        );
        assert_eq!(
            seq.push(UdpFrame::data(1, b"b".to_vec())),
            UdpDelivery::Delivered(vec![b"b".to_vec()])
        );
        assert_eq!(seq.delivered_count(), 2);
        assert_eq!(seq.lost_count(), 0);
        assert_eq!(seq.next_expected(), 2);
    }

    #[test]
    fn udp_sequencer_holds_gaps_and_drains_on_fill() {
        let mut seq = UdpSequencer::new();
        // seq 1 arrives before seq 0: held, one loss recorded.
        assert_eq!(
            seq.push(UdpFrame::data(1, b"b".to_vec())),
            UdpDelivery::Held { seq: 1, missing: 1 }
        );
        assert_eq!(seq.held_len(), 1);
        assert_eq!(seq.lost_count(), 1);
        // A second frame for the same gap is held but not double-counted.
        assert_eq!(
            seq.push(UdpFrame::data(2, b"c".to_vec())),
            UdpDelivery::Held { seq: 2, missing: 2 }
        );
        assert_eq!(seq.lost_count(), 2);
        // seq 0 arrives: 0, 1, and 2 all deliver in order.
        assert_eq!(
            seq.push(UdpFrame::data(0, b"a".to_vec())),
            UdpDelivery::Delivered(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
        );
        assert_eq!(seq.delivered_count(), 3);
        assert_eq!(seq.held_len(), 0);
        assert_eq!(seq.next_expected(), 3);
    }

    #[test]
    fn udp_sequencer_drops_duplicates() {
        let mut seq = UdpSequencer::new();
        seq.push(UdpFrame::data(0, b"a".to_vec()));
        assert_eq!(
            seq.push(UdpFrame::data(0, b"a".to_vec())),
            UdpDelivery::Duplicate(0)
        );
        assert_eq!(seq.duplicate_count(), 1);
        assert_eq!(seq.delivered_count(), 1);
    }

    #[test]
    fn udp_sequencer_handles_wraparound() {
        // Start near the top of the sequence space; delivery wraps to 0.
        let mut seq = UdpSequencer::with_next(u32::MAX - 1);
        assert_eq!(
            seq.push(UdpFrame::data(u32::MAX - 1, b"x".to_vec())),
            UdpDelivery::Delivered(vec![b"x".to_vec()])
        );
        assert_eq!(
            seq.push(UdpFrame::data(u32::MAX, b"y".to_vec())),
            UdpDelivery::Delivered(vec![b"y".to_vec()])
        );
        // The successor of u32::MAX wraps to 0 — still in order.
        assert_eq!(
            seq.push(UdpFrame::data(0, b"z".to_vec())),
            UdpDelivery::Delivered(vec![b"z".to_vec()])
        );
        assert_eq!(seq.next_expected(), 1);
        // A frame from before the wrap is a duplicate, not held forever.
        assert_eq!(
            seq.push(UdpFrame::data(u32::MAX - 1, b"x".to_vec())),
            UdpDelivery::Duplicate(u32::MAX - 1)
        );
    }
}
