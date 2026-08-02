//! RDP-UDP datagram framing (MS-RDPEUDP) — the reliable/lossy UDP transport that
//! carries side-band graphics for low-latency sessions.
//!
//! Every datagram begins with an `RDPUDP_FEC_HEADER` { snSourceAck, uReceiverWindowSize,
//! uFlags }. The connection opens with a three-way handshake: the client sends a
//! **SYN** (padded to the path MTU) carrying an `RDPUDP_SYNDATA_PAYLOAD` (its
//! initial sequence number and MTUs); the server replies **SYN+ACK**; the client
//! **ACK**s. After that, source data rides in DATA datagrams and is acknowledged
//! with ACK vectors. The lossy channel additionally sets `SYNLOSSY` and carries
//! FEC-coded payloads.
//!
//! All multi-byte fields here are **big-endian** (network byte order), unlike the
//! rest of RDP — RDP-UDP runs directly over UDP. This module is the wire codec
//! for the handshake + framing; the socket loop, retransmission, and FEC live in
//! the client driver. Everything is pure and unit-tested.

/// `RDPUDP_FLAG_*` bits in the FEC header's `uFlags`.
pub mod flags {
    pub const SYN: u16 = 0x0001;
    pub const FIN: u16 = 0x0002;
    pub const ACK: u16 = 0x0004;
    pub const DATA: u16 = 0x0008;
    pub const FEC: u16 = 0x0010;
    pub const CN: u16 = 0x0020;
    pub const CWR: u16 = 0x0040;
    /// Set on the SYN of the *lossy* channel.
    pub const SYNLOSSY: u16 = 0x0080;
    pub const ACKDELAYED: u16 = 0x0100;
    pub const ACKOFEARLIER: u16 = 0x0200;
    /// Extended SYN (RDP-UDP v2 capability negotiation).
    pub const SYNEX: u16 = 0x0400;
}

/// A SYN datagram is padded so the whole packet reaches the path MTU; 1232 is
/// the conventional value (1280 IPv6 min MTU minus IP/UDP headers).
pub const SYN_PACKET_SIZE: usize = 1232;

/// The common `RDPUDP_FEC_HEADER` carried on every datagram (8 bytes, big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecHeader {
    /// Highest source sequence number the sender has received from its peer.
    pub sn_source_ack: u32,
    /// The sender's receive-window size, in datagrams.
    pub receiver_window: u16,
    /// `flags::*` bitmask.
    pub flags: u16,
}

impl FecHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.sn_source_ack.to_be_bytes());
        out.extend_from_slice(&self.receiver_window.to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
    }

    /// Parse the 8-byte header from the front of `data`.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        Some(Self {
            sn_source_ack: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            receiver_window: u16::from_be_bytes([data[4], data[5]]),
            flags: u16::from_be_bytes([data[6], data[7]]),
        })
    }

    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

/// `RDPUDP_SYNDATA_PAYLOAD` (2.2.2.2): the initial sequence number and MTUs that
/// follow the FEC header on a SYN / SYN+ACK datagram (8 bytes, big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynData {
    pub initial_sequence: u32,
    pub upstream_mtu: u16,
    pub downstream_mtu: u16,
}

impl SynData {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.initial_sequence.to_be_bytes());
        out.extend_from_slice(&self.upstream_mtu.to_be_bytes());
        out.extend_from_slice(&self.downstream_mtu.to_be_bytes());
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        Some(Self {
            initial_sequence: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            upstream_mtu: u16::from_be_bytes([data[4], data[5]]),
            downstream_mtu: u16::from_be_bytes([data[6], data[7]]),
        })
    }
}

/// Build the client's initial SYN datagram for a new RDP-UDP connection.
/// `lossy` selects the lossy (graphics) channel. The packet is zero-padded to
/// [`SYN_PACKET_SIZE`] so the server can confirm the path MTU.
pub fn build_syn(initial_sequence: u32, receiver_window: u16, lossy: bool) -> Vec<u8> {
    let mut flags = flags::SYN;
    if lossy {
        flags |= flags::SYNLOSSY;
    }
    let header = FecHeader {
        // No data received yet; ack the predecessor of our own initial sequence.
        sn_source_ack: initial_sequence.wrapping_sub(1),
        receiver_window,
        flags,
    };
    let syn = SynData {
        initial_sequence,
        upstream_mtu: SYN_PACKET_SIZE as u16,
        downstream_mtu: SYN_PACKET_SIZE as u16,
    };
    let mut out = Vec::with_capacity(SYN_PACKET_SIZE);
    header.write(&mut out);
    syn.write(&mut out);
    out.resize(SYN_PACKET_SIZE, 0); // pad to the MTU
    out
}

/// `VECTOR_ELEMENT_STATE` (top 2 bits of an ACK-vector element byte).
mod ack_state {
    pub const RECEIVED: u8 = 0x00;
    pub const NOT_YET_RECEIVED: u8 = 0xC0; // state 3 in the high 2 bits
}

/// Build an `RDPUDP_ACK_VECTOR_HEADER` reporting `received` consecutive
/// datagrams as RECEIVED, ending at `snSourceAck`. Each run-length element packs
/// the state in the top 2 bits and the length in the low 6 (max 63 per element),
/// so a long run spans several elements. An empty/0 run yields an empty vector.
pub fn ack_vector(received: u32) -> Vec<u8> {
    let mut elements = Vec::new();
    let mut remaining = received;
    while remaining > 0 && elements.len() < 0xFFFF {
        let run = remaining.min(63) as u8;
        elements.push(ack_state::RECEIVED | run);
        remaining -= run as u32;
    }
    let mut out = Vec::with_capacity(2 + elements.len());
    out.extend_from_slice(&(elements.len() as u16).to_be_bytes()); // uAckVectorSize
    out.extend_from_slice(&elements);
    out
}

/// Build an ACK datagram acknowledging `sn_source_ack`, reporting `received`
/// datagrams as received in the ACK vector (`received == 0` → empty vector).
pub fn build_ack(sn_source_ack: u32, receiver_window: u16) -> Vec<u8> {
    build_ack_with(sn_source_ack, receiver_window, 0)
}

/// Like [`build_ack`] but with an ACK vector covering `received` datagrams.
pub fn build_ack_with(sn_source_ack: u32, receiver_window: u16, received: u32) -> Vec<u8> {
    let mut out = Vec::new();
    FecHeader {
        sn_source_ack,
        receiver_window,
        flags: flags::ACK,
    }
    .write(&mut out);
    out.extend_from_slice(&ack_vector(received));
    out
}

/// Mark `NOT_YET_RECEIVED` available for callers/tests that build gap vectors.
pub use ack_state::NOT_YET_RECEIVED as ACK_STATE_NOT_RECEIVED;

/// Classify a received datagram's handshake role from its header flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramKind {
    /// SYN+ACK — the server accepted our SYN (handshake step 2).
    SynAck,
    /// Carries acknowledgements.
    Ack,
    /// Carries source (or FEC) data.
    Data,
    /// Connection teardown.
    Fin,
    Other,
}

/// Extract the upper-layer payload from a DATA datagram, skipping the FEC
/// header, the optional inbound ACK vector, and the source-payload header
/// (`snCoded` + `snSourceStart`). `flags` is the FEC header's flag word.
/// Returns `None` if the datagram is malformed/too short.
pub fn source_payload(data: &[u8], flags: u16) -> Option<&[u8]> {
    let mut off = 8usize; // past the FEC header
    if flags & flags::ACK != 0 {
        // RDPUDP_ACK_VECTOR_HEADER: uAckVectorSize(2, BE) + that many elements,
        // padded to a 4-byte boundary (relative to the datagram start).
        let size = u16::from_be_bytes([*data.get(off)?, *data.get(off + 1)?]) as usize;
        off += 2 + size;
        off = (off + 3) & !3; // 4-byte align
    }
    if flags & flags::DATA != 0 {
        // RDPUDP_SOURCE_PAYLOAD_HEADER: snCoded(4) + snSourceStart(4).
        off += 8;
    }
    data.get(off..)
}

/// The source sequence number (`snSourceStart`) of a DATA datagram, for inbound
/// gap/loss detection. `None` if the datagram carries no source-payload header or
/// is truncated. Mirrors the offset arithmetic in [`source_payload`].
pub fn source_seq(data: &[u8], flags: u16) -> Option<u32> {
    if flags & flags::DATA == 0 {
        return None;
    }
    let mut off = 8usize; // past the FEC header
    if flags & flags::ACK != 0 {
        let size = u16::from_be_bytes([*data.get(off)?, *data.get(off + 1)?]) as usize;
        off += 2 + size;
        off = (off + 3) & !3; // 4-byte align
    }
    // RDPUDP_SOURCE_PAYLOAD_HEADER: snCoded(4) then snSourceStart(4).
    let s = off + 4;
    Some(u32::from_be_bytes([
        *data.get(s)?,
        *data.get(s + 1)?,
        *data.get(s + 2)?,
        *data.get(s + 3)?,
    ]))
}

/// A one-line human description of a datagram for the `--udp-debug` capture
/// mode: kind, decoded flags, ack point, window, and length. Pure, so callers
/// can log inbound and outbound datagrams uniformly.
pub fn describe(data: &[u8]) -> String {
    let Some((h, kind, syn)) = classify(data) else {
        return format!("<malformed {} bytes>", data.len());
    };
    let mut flags = Vec::new();
    for (bit, name) in [
        (flags::SYN, "SYN"),
        (flags::FIN, "FIN"),
        (flags::ACK, "ACK"),
        (flags::DATA, "DATA"),
        (flags::FEC, "FEC"),
        (flags::SYNLOSSY, "LOSSY"),
        (flags::SYNEX, "SYNEX"),
    ] {
        if h.has(bit) {
            flags.push(name);
        }
    }
    let syn = syn
        .map(|s| format!(" isn={} mtu={}", s.initial_sequence, s.downstream_mtu))
        .unwrap_or_default();
    format!(
        "{:?} [{}] ack={} win={} len={}{}",
        kind,
        flags.join("|"),
        h.sn_source_ack,
        h.receiver_window,
        data.len(),
        syn
    )
}

/// Inspect a received datagram and report its role plus any SYN payload.
pub fn classify(data: &[u8]) -> Option<(FecHeader, DatagramKind, Option<SynData>)> {
    let header = FecHeader::parse(data)?;
    let kind = if header.has(flags::SYN) && header.has(flags::ACK) {
        DatagramKind::SynAck
    } else if header.has(flags::FIN) {
        DatagramKind::Fin
    } else if header.has(flags::DATA) {
        DatagramKind::Data
    } else if header.has(flags::ACK) {
        DatagramKind::Ack
    } else {
        DatagramKind::Other
    };
    // SYN(+ACK) datagrams carry an RDPUDP_SYNDATA_PAYLOAD right after the header.
    let syn = header
        .has(flags::SYN)
        .then(|| SynData::parse(&data[8..]))
        .flatten();
    Some((header, kind, syn))
}

/// Where an RDP-UDP connection is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// SYN sent, waiting for the server's SYN+ACK.
    SynSent,
    /// Handshake complete; data may flow.
    Established,
    /// FIN seen / connection torn down.
    Closed,
}

/// An action the driver should take in response to an inbound datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send these bytes back to the peer over the UDP socket.
    Send(Vec<u8>),
    /// Deliver this reassembled upper-layer payload (RDPEMT/TLS) to the tunnel.
    Deliver(Vec<u8>),
    /// The handshake just completed.
    Established,
    /// The connection closed.
    Closed,
}

/// The sans-I/O RDP-UDP connection state machine: it owns the handshake and
/// acknowledgement logic and tells the driver what to send / deliver. The driver
/// owns the socket and the retransmission timers. Pure, so it is unit-testable
/// without a network.
#[derive(Debug)]
pub struct Connection {
    state: State,
    /// Our initial/most-recent source sequence number.
    send_seq: u32,
    /// Highest source sequence number we've received from the peer (what we ACK).
    recv_seq: u32,
    /// Count of source datagrams received (reported in the ACK vector, capped at
    /// the receive window).
    recv_count: u32,
    /// Our advertised receive window (datagrams).
    window: u16,
    lossy: bool,
    /// Outbound DATA datagrams not yet acknowledged by the peer, keyed by source
    /// sequence number, for retransmission until the peer's snSourceAck passes
    /// them. Bounded so a stalled peer can't grow it without limit. Always empty
    /// on the lossy channel, which never retransmits.
    unacked: std::collections::BTreeMap<u32, Vec<u8>>,
    /// Source datagrams delivered from the peer (for loss-rate metrics).
    recv_source_total: u64,
    /// Source datagrams the peer's sequence numbers show as missing.
    recv_lost: u64,
    /// Next inbound source sequence number to deliver, established from the
    /// server's SYN+ACK initial sequence (or the first DATA seen). Delivery is
    /// strictly in order — the payload feeds a TLS byte stream, which one
    /// out-of-order or missing datagram desynchronizes permanently.
    recv_next: Option<u32>,
    /// Out-of-order source payloads held until the gap before them fills (the
    /// peer retransmits unacknowledged data on the reliable channel). Bounded.
    reorder: std::collections::BTreeMap<u32, Vec<u8>>,
}

/// Cap on held out-of-order datagrams. Past this the peer has stalled far
/// beyond its send window and the tunnel is effectively dead.
const MAX_REORDER: usize = 256;

impl Connection {
    /// Start a connection: pick an initial sequence number and produce the SYN
    /// datagram to send. `lossy` selects the lossy (graphics) channel.
    pub fn connect(initial_sequence: u32, window: u16, lossy: bool) -> (Self, Vec<u8>) {
        let conn = Self {
            state: State::SynSent,
            send_seq: initial_sequence,
            recv_seq: 0,
            recv_count: 0,
            window,
            lossy,
            unacked: std::collections::BTreeMap::new(),
            recv_source_total: 0,
            recv_lost: 0,
            recv_next: None,
            reorder: std::collections::BTreeMap::new(),
        };
        (conn, build_syn(initial_sequence, window, lossy))
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_established(&self) -> bool {
        self.state == State::Established
    }

    /// Whether this is the lossy (FEC) channel — the one that carries real-time
    /// graphics; the reliable channel carries everything else.
    pub fn is_lossy(&self) -> bool {
        self.lossy
    }

    /// Process one received datagram, advancing the handshake and producing the
    /// actions to take (ACKs to send, payloads to deliver, state transitions).
    ///
    /// Delivery is strictly in-order: the payload stream feeds TLS, which one
    /// missing or reordered datagram desynchronizes permanently. Out-of-order
    /// arrivals are held in a bounded reorder buffer, the cumulative ack only
    /// ever names the last in-order datagram actually received (so the peer's
    /// retransmission repairs holes instead of being suppressed), and FEC
    /// parity datagrams are never delivered as source data.
    pub fn on_receive(&mut self, datagram: &[u8]) -> Vec<Action> {
        let Some((header, kind, syn)) = classify(datagram) else {
            return Vec::new();
        };
        // Every datagram carries snSourceAck = the highest source seq the peer
        // has received; drop everything up to and including it from our
        // retransmit buffer (keep only seqs strictly ahead of the ack).
        self.unacked.retain(|&seq, _| {
            let ahead = seq.wrapping_sub(header.sn_source_ack);
            ahead != 0 && ahead < 0x8000_0000
        });
        match kind {
            DatagramKind::SynAck if self.state == State::SynSent => {
                self.state = State::Established;
                // The server's initial sequence arrives in the SYNDATA payload;
                // its first source datagram is that + 1. The cumulative ack
                // starts at the initial sequence itself.
                if let Some(s) = syn {
                    self.recv_seq = s.initial_sequence;
                    self.recv_next = Some(s.initial_sequence.wrapping_add(1));
                } else {
                    // No SYNDATA (nonconforming peer): learn the numbering from
                    // the first DATA datagram instead.
                    self.recv_seq = header.sn_source_ack;
                }
                vec![
                    Action::Send(build_ack(self.recv_seq, self.window)),
                    Action::Established,
                ]
            }
            DatagramKind::Data if self.state == State::Established => {
                // FEC parity datagrams share the DATA framing but are not
                // source data — delivering their parity block into the TLS
                // stream corrupts it even on a loss-free link. (Recovery from
                // parity is not implemented; the reliable channel repairs by
                // retransmission instead.)
                if header.has(flags::FEC) {
                    return vec![Action::Send(build_ack_with(
                        self.recv_seq,
                        self.window,
                        self.recv_count,
                    ))];
                }
                self.recv_source_total += 1;
                let seq = source_seq(datagram, header.flags);
                let payload = source_payload(datagram, header.flags)
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_vec());
                let mut actions = Vec::new();
                // No sequence/payload → nothing to deliver; just re-ack below.
                if let (Some(seq), Some(payload)) = (seq, payload) {
                    // Learn the numbering from the first DATA if the SYN+ACK
                    // carried no SYNDATA.
                    let next = *self.recv_next.get_or_insert(seq);
                    let ahead = seq.wrapping_sub(next);
                    if ahead >= 0x8000_0000 {
                        // seq < next: a duplicate of something already
                        // delivered (our ack was lost) — re-ack, drop.
                    } else if ahead == 0 {
                        // In order: deliver, then drain everything the reorder
                        // buffer now makes contiguous.
                        let mut next = next;
                        actions.push(Action::Deliver(payload));
                        next = next.wrapping_add(1);
                        self.recv_count = (self.recv_count + 1).min(self.window as u32);
                        while let Some(held) = self.reorder.remove(&next) {
                            actions.push(Action::Deliver(held));
                            next = next.wrapping_add(1);
                            self.recv_count = (self.recv_count + 1).min(self.window as u32);
                        }
                        self.recv_next = Some(next);
                        self.recv_seq = next.wrapping_sub(1);
                    } else {
                        // A gap: hold this datagram until retransmission fills
                        // the hole. The cumulative ack deliberately does NOT
                        // advance — that is what tells the peer to retransmit
                        // the missing datagrams.
                        self.recv_lost += 1;
                        if self.reorder.len() < MAX_REORDER {
                            self.reorder.insert(seq, payload);
                        }
                    }
                }
                actions.push(Action::Send(build_ack_with(
                    self.recv_seq,
                    self.window,
                    self.recv_count,
                )));
                actions
            }
            DatagramKind::Fin => {
                self.state = State::Closed;
                vec![Action::Closed]
            }
            _ => Vec::new(),
        }
    }

    /// Frame an upper-layer `payload` (e.g. an RDPEMT tunnel PDU) as a DATA
    /// datagram, advancing our source sequence number and buffering it for
    /// retransmission until the peer acknowledges it.
    pub fn build_data(&mut self, payload: &[u8]) -> Vec<u8> {
        self.send_seq = self.send_seq.wrapping_add(1);
        let mut out = Vec::with_capacity(8 + payload.len());
        FecHeader {
            sn_source_ack: self.recv_seq,
            receiver_window: self.window,
            flags: flags::DATA | flags::ACK,
        }
        .write(&mut out);
        out.extend_from_slice(payload);
        // The lossy (real-time graphics) channel never retransmits: a re-sent
        // video datagram arrives too late to matter and only adds head-of-line
        // latency, so we keep nothing buffered and rely on FEC / the next frame.
        // The reliable channel buffers for retransmission, bounded — dropping the
        // oldest if the peer stalls so a dead peer can't grow the map without end.
        if !self.lossy {
            const MAX_UNACKED: usize = 512;
            if self.unacked.len() >= MAX_UNACKED {
                if let Some(&oldest) = self.unacked.keys().next() {
                    self.unacked.remove(&oldest);
                }
            }
            self.unacked.insert(self.send_seq, out.clone());
        }
        out
    }

    /// `(source datagrams delivered, datagrams detected missing)` since connect.
    /// On the reliable channel a "missing" datagram is usually recovered by
    /// retransmission, so this mostly reflects reorder / initial loss there; on
    /// the lossy channel it is the true loss the congestion controller reacts to.
    pub fn loss_stats(&self) -> (u64, u64) {
        (self.recv_source_total, self.recv_lost)
    }

    /// The datagrams still awaiting acknowledgement, to resend on a timer. The
    /// driver decides the retransmission interval; this just reports what's
    /// outstanding (oldest first).
    pub fn retransmit(&self) -> Vec<Vec<u8>> {
        self.unacked.values().cloned().collect()
    }

    /// How many datagrams are awaiting acknowledgement.
    pub fn unacked_len(&self) -> usize {
        self.unacked.len()
    }

    /// Our most-recent outbound source sequence number (for RTT timing).
    pub fn send_seq(&self) -> u32 {
        self.send_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syn_ack(initial: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        FecHeader {
            sn_source_ack: initial,
            receiver_window: 64,
            flags: flags::SYN | flags::ACK,
        }
        .write(&mut buf);
        SynData {
            initial_sequence: initial,
            upstream_mtu: 1232,
            downstream_mtu: 1232,
        }
        .write(&mut buf);
        buf
    }

    #[test]
    fn handshake_completes_on_syn_ack() {
        let (mut conn, syn) = Connection::connect(1000, 64, true);
        assert_eq!(conn.state(), State::SynSent);
        assert_eq!(syn.len(), SYN_PACKET_SIZE);
        let actions = conn.on_receive(&syn_ack(5000));
        assert!(conn.is_established());
        assert!(matches!(actions[0], Action::Send(_)));
        assert_eq!(actions[1], Action::Established);
    }

    #[test]
    fn data_is_delivered_and_acked() {
        let (mut conn, _) = Connection::connect(1, 64, false);
        conn.on_receive(&syn_ack(10)); // establish
        // A realistic DATA datagram: FEC header (DATA only, no ACK) + source
        // payload header (snCoded, snSourceStart) + the payload.
        let mut dg = Vec::new();
        FecHeader {
            sn_source_ack: 1,
            receiver_window: 64,
            flags: flags::DATA,
        }
        .write(&mut dg);
        dg.extend_from_slice(&11u32.to_be_bytes()); // snCoded
        dg.extend_from_slice(&11u32.to_be_bytes()); // snSourceStart
        dg.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let actions = conn.on_receive(&dg);
        assert_eq!(actions[0], Action::Deliver(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        // The ACK now carries a non-empty vector (one RECEIVED datagram).
        match &actions[1] {
            Action::Send(ack) => {
                assert_eq!(u16::from_be_bytes([ack[8], ack[9]]), 1); // uAckVectorSize
                assert_eq!(ack[10] & 0xC0, 0x00); // state = RECEIVED
                assert_eq!(ack[10] & 0x3F, 1); // run length 1
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn ack_vector_splits_long_runs() {
        // 0 received → empty vector.
        assert_eq!(ack_vector(0), vec![0x00, 0x00]);
        // 100 received → two elements (63 + 37), both RECEIVED.
        let v = ack_vector(100);
        assert_eq!(u16::from_be_bytes([v[0], v[1]]), 2);
        assert_eq!(v[2], 63); // RECEIVED | 63
        assert_eq!(v[3], 37); // RECEIVED | 37
    }

    #[test]
    fn unacked_data_is_buffered_then_pruned_by_ack() {
        let (mut conn, _) = Connection::connect(100, 64, false);
        conn.on_receive(&syn_ack(10)); // establish; send_seq=100
        // Send three DATA datagrams → seqs 101,102,103 buffered.
        conn.build_data(&[1]);
        conn.build_data(&[2]);
        conn.build_data(&[3]);
        assert_eq!(conn.unacked_len(), 3);
        assert_eq!(conn.retransmit().len(), 3);
        // A datagram from the peer acking up to seq 102 prunes 101 and 102.
        let mut ack = Vec::new();
        FecHeader {
            sn_source_ack: 102,
            receiver_window: 64,
            flags: flags::ACK,
        }
        .write(&mut ack);
        ack.extend_from_slice(&0u16.to_be_bytes()); // empty ack vector
        conn.on_receive(&ack);
        assert_eq!(conn.unacked_len(), 1); // only seq 103 remains
    }

    /// Build a DATA datagram carrying source seq `src` and `payload` (DATA-only,
    /// no inbound ack vector — matching the `data_is_delivered_and_acked` shape).
    fn data_datagram(src: u32, payload: &[u8]) -> Vec<u8> {
        let mut dg = Vec::new();
        FecHeader {
            sn_source_ack: 1,
            receiver_window: 64,
            flags: flags::DATA,
        }
        .write(&mut dg);
        dg.extend_from_slice(&src.to_be_bytes()); // snCoded
        dg.extend_from_slice(&src.to_be_bytes()); // snSourceStart
        dg.extend_from_slice(payload);
        dg
    }

    #[test]
    fn lossy_channel_does_not_buffer_for_retransmit() {
        let (mut conn, _) = Connection::connect(100, 64, true); // lossy
        conn.on_receive(&syn_ack(10));
        conn.build_data(&[1]);
        conn.build_data(&[2]);
        // Nothing is kept for resend on the real-time channel.
        assert_eq!(conn.unacked_len(), 0);
        assert!(conn.retransmit().is_empty());
    }

    /// The delivered payloads of a list of actions, in order.
    fn delivered(actions: &[Action]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Deliver(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// The `snSourceAck` of the ACK an action list sends, if any.
    fn ack_point(actions: &[Action]) -> Option<u32> {
        actions.iter().find_map(|a| match a {
            Action::Send(dg) => Some(u32::from_be_bytes([dg[0], dg[1], dg[2], dg[3]])),
            _ => None,
        })
    }

    /// A hole in the source sequence must (a) count as detected loss, (b) hold
    /// later datagrams back rather than deliver them out of order into the TLS
    /// stream, and (c) keep the cumulative ack at the last in-order datagram —
    /// which is what makes the peer retransmit the missing one. When the hole
    /// fills, everything held delivers in order.
    #[test]
    fn gap_holds_delivery_until_retransmission_fills_it() {
        let (mut conn, _) = Connection::connect(1, 64, false);
        conn.on_receive(&syn_ack(10)); // server ISN 10 → first data is 11
        let a = conn.on_receive(&data_datagram(11, &[0xAA]));
        assert_eq!(delivered(&a), vec![vec![0xAA]]);
        assert_eq!(ack_point(&a), Some(11));
        let b = conn.on_receive(&data_datagram(12, &[0xBB]));
        assert_eq!(delivered(&b), vec![vec![0xBB]]);
        // 13 is lost; 14 arrives — held, NOT delivered, ack stays at 12.
        let c = conn.on_receive(&data_datagram(14, &[0xDD]));
        assert!(delivered(&c).is_empty());
        assert_eq!(ack_point(&c), Some(12));
        let (received, lost) = conn.loss_stats();
        assert_eq!(received, 3);
        assert_eq!(lost, 1); // the gap at seq 13
        // The retransmitted 13 arrives: 13 and the held 14 deliver in order.
        let d = conn.on_receive(&data_datagram(13, &[0xCC]));
        assert_eq!(delivered(&d), vec![vec![0xCC], vec![0xDD]]);
        assert_eq!(ack_point(&d), Some(14));
    }

    /// A duplicate of an already-delivered datagram (our ACK was lost) is
    /// re-acked but never re-delivered.
    #[test]
    fn duplicate_is_reacked_not_redelivered() {
        let (mut conn, _) = Connection::connect(1, 64, false);
        conn.on_receive(&syn_ack(10));
        conn.on_receive(&data_datagram(11, &[0xAA]));
        let again = conn.on_receive(&data_datagram(11, &[0xAA]));
        assert!(delivered(&again).is_empty());
        assert_eq!(ack_point(&again), Some(11));
    }

    /// FEC parity datagrams share the DATA framing but are not source data;
    /// delivering the parity block into the TLS stream corrupts it even on a
    /// loss-free link.
    #[test]
    fn fec_parity_is_never_delivered() {
        let (mut conn, _) = Connection::connect(1, 64, false);
        conn.on_receive(&syn_ack(10));
        let mut dg = Vec::new();
        FecHeader {
            sn_source_ack: 1,
            receiver_window: 64,
            flags: flags::DATA | flags::FEC,
        }
        .write(&mut dg);
        dg.extend_from_slice(&11u32.to_be_bytes()); // snCoded
        dg.extend_from_slice(&11u32.to_be_bytes()); // snSourceStart
        dg.extend_from_slice(&[0x55; 16]); // parity block
        let actions = conn.on_receive(&dg);
        assert!(delivered(&actions).is_empty());
        // Real source data afterwards still flows normally.
        let a = conn.on_receive(&data_datagram(11, &[0xAA]));
        assert_eq!(delivered(&a), vec![vec![0xAA]]);
    }

    #[test]
    fn source_seq_parses_data_only() {
        let dg = data_datagram(42, &[1, 2, 3]);
        assert_eq!(source_seq(&dg, flags::DATA), Some(42));
        // No source-payload header when the DATA flag is clear.
        assert_eq!(source_seq(&dg, flags::ACK), None);
    }

    #[test]
    fn fin_closes() {
        let (mut conn, _) = Connection::connect(1, 64, false);
        conn.on_receive(&syn_ack(10));
        let mut dg = Vec::new();
        FecHeader {
            sn_source_ack: 1,
            receiver_window: 64,
            flags: flags::FIN,
        }
        .write(&mut dg);
        assert_eq!(conn.on_receive(&dg), vec![Action::Closed]);
        assert_eq!(conn.state(), State::Closed);
    }

    #[test]
    fn syn_is_padded_and_well_formed() {
        let syn = build_syn(1000, 64, true);
        assert_eq!(syn.len(), SYN_PACKET_SIZE);
        let (h, kind, payload) = classify(&syn).unwrap();
        // build_syn isn't a SYN+ACK, but classify only tags SynAck when ACK is
        // also set; a bare SYN tags as Other — assert the flags directly instead.
        assert!(h.has(flags::SYN));
        assert!(h.has(flags::SYNLOSSY));
        assert_eq!(kind, DatagramKind::Other);
        assert_eq!(payload.unwrap().initial_sequence, 1000);
        assert_eq!(payload.unwrap().upstream_mtu, SYN_PACKET_SIZE as u16);
    }

    #[test]
    fn header_roundtrips_big_endian() {
        let h = FecHeader {
            sn_source_ack: 0x0102_0304,
            receiver_window: 0x0506,
            flags: flags::ACK,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert_eq!(&buf[0..4], &[0x01, 0x02, 0x03, 0x04]); // big-endian
        assert_eq!(FecHeader::parse(&buf).unwrap(), h);
    }

    #[test]
    fn classifies_syn_ack_with_payload() {
        let mut buf = Vec::new();
        FecHeader {
            sn_source_ack: 999,
            receiver_window: 64,
            flags: flags::SYN | flags::ACK,
        }
        .write(&mut buf);
        SynData {
            initial_sequence: 5000,
            upstream_mtu: 1232,
            downstream_mtu: 1232,
        }
        .write(&mut buf);
        let (_, kind, payload) = classify(&buf).unwrap();
        assert_eq!(kind, DatagramKind::SynAck);
        assert_eq!(payload.unwrap().initial_sequence, 5000);
    }

    #[test]
    fn describe_summarizes_a_syn() {
        let syn = build_syn(1000, 64, true);
        let d = describe(&syn);
        assert!(d.contains("SYN"));
        assert!(d.contains("LOSSY"));
        assert!(d.contains("isn=1000"));
        assert_eq!(describe(&[0u8; 2]), "<malformed 2 bytes>");
    }

    #[test]
    fn ack_has_empty_vector() {
        let ack = build_ack(42, 64);
        let (h, kind, _) = classify(&ack).unwrap();
        assert_eq!(kind, DatagramKind::Ack);
        assert_eq!(h.sn_source_ack, 42);
        assert_eq!(&ack[8..10], &[0x00, 0x00]); // uAckVectorSize = 0
    }
}
