//! UDP side-band transport driver (MS-RDPEUDP + MS-RDPEMT).
//!
//! When the server requests multitransport and `--udp` is set, this dials a UDP
//! socket to the same host, runs the RDP-UDP handshake ([`rdp_pdu::rdpudp`]),
//! negotiates TLS over the reliable channel (reusing the SChannel
//! [`TlsStream`](crate::tls::TlsStream)), and opens an RDPEMT tunnel
//! ([`rdp_channels::emt`]) carrying the multitransport `requestId` + cookie.
//! Higher layers (RDPGFX) then flow as tunnel data with lower latency.
//!
//! Every step returns `io::Result`, and the caller treats any error as "stay on
//! TCP" — the UDP path is a pure enhancement that can never break the session.
//! Blind Windows FFI / network code: type-checked here, validated on hardware
//! (UDP egress is blocked in CI, and multitransport only exists on modern hosts).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdp_pdu::rdpudp::{Action, Connection, FecHeader};

use crate::congestion::RttEstimator;
use crate::tls::TlsStream;

/// Largest datagram we expect (path-MTU-sized RDP-UDP packets).
const RECV_BUF: usize = 1500;
/// How long to wait for each handshake datagram before retrying / giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);
/// SYN retransmit attempts before declaring the UDP path dead (→ TCP).
const SYN_RETRIES: u32 = 4;
/// Steady-state socket read timeout once the tunnel is up. Matches the TCP
/// graphics loop's ~1 ms poll cadence so an idle tunnel returns promptly instead
/// of blocking the session loop (and thus input / clipboard / TCP) for seconds.
/// 1 ms is the practical floor for `SO_RCVTIMEO` on Windows.
const STEADY_POLL_TIMEOUT: Duration = Duration::from_millis(1);
/// Scratch buffer for draining decrypted tunnel plaintext. Sized to a full
/// Schannel record (~16 KiB) so one read empties a decrypted record; PDUs larger
/// than this are reassembled across reads via [`UdpTunnel::rx`].
const TUNNEL_READ_BUF: usize = 16 * 1024;

/// Live transport stats shared between the [`UdpReliable`] driver (buried inside
/// the TLS stream) and the [`UdpTunnel`] the session loop holds, so the session
/// can snapshot them each iteration without reaching through TLS. All cumulative
/// since connect except the gauges (`srtt_us`, `rto_us`), which are last-known.
#[derive(Debug, Default)]
pub struct UdpStats {
    /// Smoothed RTT in microseconds (0 until the first measurement).
    pub srtt_us: AtomicU64,
    /// Smoothed RTT variation (jitter) in microseconds — the signal that most
    /// distinguishes a clean wired link from a flaky Wi-Fi one.
    pub jitter_us: AtomicU64,
    /// Current retransmit timeout in microseconds.
    pub rto_us: AtomicU64,
    /// Total datagrams retransmitted.
    pub retransmits: AtomicU64,
    /// Source datagrams delivered by the peer.
    pub recv_total: AtomicU64,
    /// Source datagrams the peer's sequence numbers show as missing.
    pub recv_lost: AtomicU64,
}

/// A point-in-time snapshot of [`UdpStats`] for the congestion controller.
#[derive(Debug, Clone, Copy)]
pub struct NetSample {
    pub recv_total: u64,
    pub recv_lost: u64,
    /// Smoothed RTT, or `None` before the first measurement.
    pub srtt: Option<Duration>,
    /// Smoothed RTT variation (jitter).
    pub jitter: Duration,
}

/// A best-effort reliable byte stream over RDP-UDP: writes are framed as DATA
/// datagrams, reads pull datagrams and surface delivered payloads, sending the
/// ACKs the connection state machine asks for. Retransmission/FEC are minimal
/// (the enhancement falls back to TCP on loss), which is acceptable for the
/// reliable control channel on a healthy network.
struct UdpReliable {
    socket: UdpSocket,
    conn: Connection,
    inbox: VecDeque<u8>,
    last_retransmit: Instant,
    /// Smoothed RTT, driving the adaptive retransmit timeout.
    rtt: RttEstimator,
    /// The `(source seq, send time)` of an outstanding fresh datagram we are
    /// timing for an RTT sample, or `None`. Cleared (per Karn's algorithm) if the
    /// datagram is retransmitted, so we never time an ambiguous round trip.
    pending: Option<(u32, Instant)>,
    /// Shared with the owning [`UdpTunnel`] so the session can read live stats.
    stats: Arc<UdpStats>,
    /// `--udp-debug`: log every datagram's decoded header.
    debug: bool,
}

impl UdpReliable {
    /// Resend any datagrams the peer hasn't acknowledged once the adaptive RTO
    /// (from the live RTT estimate) has elapsed.
    fn maybe_retransmit(&mut self) {
        if self.last_retransmit.elapsed() < self.rtt.rto() {
            return;
        }
        self.last_retransmit = Instant::now();
        let mut resent = 0u64;
        for datagram in self.conn.retransmit() {
            let _ = self.socket.send(&datagram);
            resent += 1;
        }
        if resent > 0 {
            self.stats.retransmits.fetch_add(resent, Ordering::Relaxed);
            // Karn's algorithm: a retransmitted datagram makes its RTT ambiguous,
            // so abandon any sample in flight.
            self.pending = None;
        }
    }

    /// Publish the current RTT estimate and loss counters to the shared stats.
    fn publish(&self) {
        if let Some(srtt) = self.rtt.srtt() {
            self.stats
                .srtt_us
                .store(srtt.as_micros() as u64, Ordering::Relaxed);
        }
        self.stats
            .jitter_us
            .store(self.rtt.jitter().as_micros() as u64, Ordering::Relaxed);
        self.stats
            .rto_us
            .store(self.rtt.rto().as_micros() as u64, Ordering::Relaxed);
        let (total, lost) = self.conn.loss_stats();
        self.stats.recv_total.store(total, Ordering::Relaxed);
        self.stats.recv_lost.store(lost, Ordering::Relaxed);
    }

    /// Pump one inbound datagram through the connection, queuing delivered bytes
    /// and sending any ACKs. Returns `false` on timeout (no datagram).
    fn pump(&mut self) -> io::Result<bool> {
        self.maybe_retransmit();
        let mut buf = [0u8; RECV_BUF];
        match self.socket.recv(&mut buf) {
            Ok(n) => {
                if self.debug {
                    tracing::info!(target: "rdpio::udp", "recv {}", rdp_pdu::rdpudp::describe(&buf[..n]));
                }
                // Karn-safe RTT sample: if the peer's snSourceAck has reached the
                // fresh datagram we were timing, record the round trip.
                if let (Some((seq, sent)), Some(h)) = (self.pending, FecHeader::parse(&buf[..n])) {
                    if h.sn_source_ack.wrapping_sub(seq) < 0x8000_0000 {
                        self.rtt.sample(sent.elapsed());
                        self.pending = None;
                    }
                }
                for action in self.conn.on_receive(&buf[..n]) {
                    match action {
                        Action::Send(bytes) => {
                            let _ = self.socket.send(&bytes);
                        }
                        Action::Deliver(bytes) => self.inbox.extend(bytes),
                        Action::Established | Action::Closed => {}
                    }
                }
                self.publish();
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

impl Read for UdpReliable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Block (within the socket timeout) until we have delivered bytes.
        while self.inbox.is_empty() {
            if !self.pump()? {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "rdp-udp read timeout"));
            }
        }
        let n = buf.len().min(self.inbox.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.inbox.pop_front().expect("inbox non-empty");
        }
        Ok(n)
    }
}

impl Write for UdpReliable {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let datagram = self.conn.build_data(buf);
        if self.debug {
            tracing::info!(target: "rdpio::udp", "send {}", rdp_pdu::rdpudp::describe(&datagram));
        }
        self.socket.send(&datagram)?;
        // Time at most one fresh datagram at a time for an RTT sample (cleared on
        // retransmit per Karn's algorithm).
        if self.pending.is_none() {
            self.pending = Some((self.conn.send_seq(), Instant::now()));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What the session loop needs to dial the UDP transport when the server's
/// multitransport request arrives: the same host:port + TLS parameters as the
/// main connection. `None` unless `--udp` is set.
#[derive(Debug, Clone)]
pub struct UdpDial {
    pub server: String,
    pub hostname: String,
    pub accept_invalid_cert: bool,
    /// `--udp-debug`: log every RDP-UDP datagram's decoded header.
    pub debug: bool,
}

/// A connected UDP multitransport tunnel ready to carry RDPGFX.
pub struct UdpTunnel {
    tls: TlsStream<UdpReliable>,
    /// Live transport stats, updated by the driver inside `tls`.
    stats: Arc<UdpStats>,
    /// Reassembly buffer for the tunnel byte stream: an EMT Data PDU
    /// (`PayloadLength` up to 65535) can span several TLS records / datagrams, so
    /// [`Self::recv`] buffers here and only yields whole PDUs.
    rx: Vec<u8>,
}

impl UdpTunnel {
    /// Dial the UDP transport and bring up the RDP-UDP + TLS + RDPEMT tunnel.
    /// `server` is the same `host:port` as the main connection; `cookie` and
    /// `request_id` come from the server's multitransport request. Any failure
    /// returns `Err` so the caller stays on TCP.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        server: &str,
        hostname: &str,
        accept_invalid_cert: bool,
        request_id: u32,
        cookie: &[u8; 16],
        lossy: bool,
        debug: bool,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        socket.connect(server)?;
        socket.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        // Random initial sequence number for the handshake.
        let mut seed = [0u8; 4];
        crate::rng::fill(&mut seed);
        let initial = u32::from_le_bytes(seed) | 1;

        let (mut conn, syn) = Connection::connect(initial, 64, lossy);
        if debug {
            tracing::info!(target: "rdpio::udp", "send {}", rdp_pdu::rdpudp::describe(&syn));
        }
        let stats = Arc::new(UdpStats::default());
        let mut rtt = RttEstimator::new();
        let reliable = {
            // Drive the three-way handshake before handing the socket to TLS,
            // timing the SYN→SYN+ACK round trip to seed the RTT estimator (skipped
            // if we have to retransmit the SYN — an ambiguous sample, per Karn).
            let syn_sent = Instant::now();
            let mut syn_retransmitted = false;
            socket.send(&syn)?;
            let mut established = false;
            'handshake: for _ in 0..SYN_RETRIES {
                let mut buf = [0u8; RECV_BUF];
                match socket.recv(&mut buf) {
                    Ok(n) => {
                        for action in conn.on_receive(&buf[..n]) {
                            match action {
                                Action::Send(bytes) => {
                                    let _ = socket.send(&bytes);
                                }
                                Action::Established => established = true,
                                _ => {}
                            }
                        }
                        if established || conn.is_established() {
                            if !syn_retransmitted {
                                rtt.sample(syn_sent.elapsed());
                            }
                            break 'handshake;
                        }
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        syn_retransmitted = true;
                        let _ = socket.send(&syn); // retransmit SYN
                    }
                    Err(e) => return Err(e),
                }
            }
            if !conn.is_established() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "rdp-udp handshake did not complete",
                ));
            }
            UdpReliable {
                socket,
                conn,
                inbox: VecDeque::new(),
                last_retransmit: Instant::now(),
                rtt,
                pending: None,
                stats: Arc::clone(&stats),
                debug,
            }
        };
        // Longer timeout for the TLS + tunnel exchange.
        reliable
            .socket
            .set_read_timeout(Some(Duration::from_secs(5)))?;

        // TLS over the reliable channel, then the RDPEMT tunnel create handshake.
        let mut tls = TlsStream::connect(reliable, hostname, accept_invalid_cert)?;
        tls.write_all(&rdp_channels::emt::create_request(request_id, cookie))?;
        tls.flush()?;

        let mut resp = [0u8; RECV_BUF];
        let n = tls.read(&mut resp)?;
        match rdp_channels::emt::parse_create_response(&resp[..n]) {
            Some(0) => {
                // Tunnel is up. Drop the 5 s handshake timeout down to the steady
                // poll cadence so `recv()` returns promptly when the tunnel is
                // idle instead of stalling the whole session loop.
                tls.get_ref()
                    .socket
                    .set_read_timeout(Some(STEADY_POLL_TIMEOUT))?;
                Ok(Self {
                    tls,
                    stats,
                    rx: Vec::new(),
                })
            }
            Some(hr) => Err(io::Error::other(format!(
                "RDPEMT tunnel create rejected: HRESULT 0x{hr:08X}"
            ))),
            None => Err(io::Error::other("malformed RDPEMT create response")),
        }
    }

    /// Snapshot the live transport stats for the congestion controller. Counters
    /// are cumulative since connect; the caller tracks per-window deltas.
    pub fn net_stats(&self) -> NetSample {
        let srtt_us = self.stats.srtt_us.load(Ordering::Relaxed);
        NetSample {
            recv_total: self.stats.recv_total.load(Ordering::Relaxed),
            recv_lost: self.stats.recv_lost.load(Ordering::Relaxed),
            srtt: (srtt_us > 0).then(|| Duration::from_micros(srtt_us)),
            jitter: Duration::from_micros(self.stats.jitter_us.load(Ordering::Relaxed)),
        }
    }

    /// Receive the next higher-layer (RDPGFX) payload from the tunnel,
    /// reassembling an EMT Data PDU that spans multiple TLS records / datagrams.
    /// Returns one complete Data PDU's payload; control PDUs are consumed and
    /// skipped. A socket read timeout (idle tunnel) surfaces as an `Err`, which
    /// the session loop treats as "nothing this poll" and falls through to TCP.
    pub fn recv(&mut self) -> io::Result<Vec<u8>> {
        loop {
            // Frame anything already buffered before touching the socket, so
            // several PDUs delivered in one record drain across successive calls.
            if let Some((consumed, pdu)) = rdp_channels::emt::take_pdu(&self.rx) {
                self.rx.drain(..consumed);
                if pdu.action == rdp_channels::emt::ACTION_DATA {
                    return Ok(pdu.payload);
                }
                continue; // control PDU — skip it and frame the next
            }
            // Incomplete: pull one more chunk of decrypted plaintext. A timeout
            // propagates out as an error and ends the poll.
            let mut buf = [0u8; TUNNEL_READ_BUF];
            let n = self.tls.read(&mut buf)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "udp tunnel closed",
                ));
            }
            self.rx.extend_from_slice(&buf[..n]);
        }
    }

    /// Send a higher-layer payload (e.g. a frame-ack) over the tunnel.
    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        self.tls.write_all(&rdp_channels::emt::data(payload))?;
        self.tls.flush()
    }
}
