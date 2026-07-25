//! STUN + TURN client (RFC 5389/8489 + RFC 5766/8656).
//!
//! This is the transport primitive for **W365 RDP Shortpath**: the AVD/W365
//! gateway hands us an `iceServersConfig` with a TURN relay (and long-term
//! credentials) in the ARM `/connections` response. To bring up a UDP path to a
//! Cloud PC that sits behind the gateway (no dialable `host:port`), we:
//!
//!   1. STUN-`Binding` the server to learn our *server-reflexive* candidate, and
//!   2. TURN-`Allocate` a *relayed* candidate on the TURN server,
//!
//! then run ICE connectivity checks and tunnel RDP-UDP ([`crate::udp`]) over the
//! winning path. This module implements the STUN message codec, the long-term
//! authentication (MESSAGE-INTEGRITY / FINGERPRINT), and the TURN allocate /
//! permission / channel-data flows; the ICE glue lives in the Shortpath driver.
//!
//! Pure `std::net` + [`rdp_crypto`]; portable and unit-tested (the codec is
//! checked against RFC vectors, the live flows against the gateway's TURN relay).
#![allow(dead_code)]

use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use rdp_crypto::{hmac_sha1, md5};

/// STUN magic cookie (RFC 5389 §6); also the top of the XOR address masks.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;

// STUN methods (12-bit).
const METHOD_BINDING: u16 = 0x001;
const METHOD_ALLOCATE: u16 = 0x003;
const METHOD_REFRESH: u16 = 0x004;
const METHOD_SEND: u16 = 0x006;
const METHOD_DATA: u16 = 0x007;
const METHOD_CREATE_PERMISSION: u16 = 0x008;
const METHOD_CHANNEL_BIND: u16 = 0x009;

// STUN classes (2-bit): request / indication / success / error.
const CLASS_REQUEST: u16 = 0b00;
const CLASS_INDICATION: u16 = 0b01;
const CLASS_SUCCESS: u16 = 0b10;
const CLASS_ERROR: u16 = 0b11;

// Attribute types.
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_SOFTWARE: u16 = 0x8022;
const ATTR_ALTERNATE_SERVER: u16 = 0x8023;
const ATTR_FINGERPRINT: u16 = 0x8028;
// TURN (RFC 5766).
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_DONT_FRAGMENT: u16 = 0x001A;

/// REQUESTED-TRANSPORT protocol number for UDP (RFC 5766 §14.7).
const TRANSPORT_UDP: u8 = 17;

/// Compose the 16-bit STUN message type from a method and class (RFC 5389 §6):
/// the method bits are split around the two class bits (C0 at bit 4, C1 at bit 8).
fn message_type(method: u16, class: u16) -> u16 {
    (method & 0x000F)
        | ((class & 0x01) << 4)
        | ((method & 0x0070) << 1)
        | ((class & 0x02) << 7)
        | ((method & 0x0F80) << 2)
}

/// Split a message type back into `(method, class)`.
fn split_type(t: u16) -> (u16, u16) {
    let method = (t & 0x000F) | ((t & 0x00E0) >> 1) | ((t & 0x3E00) >> 2);
    let class = ((t & 0x0010) >> 4) | ((t & 0x0100) >> 7);
    (method, class)
}

/// A parsed STUN attribute (borrowed value).
#[derive(Debug, Clone)]
pub struct Attr {
    pub typ: u16,
    pub value: Vec<u8>,
}

/// A STUN message being built or parsed.
#[derive(Debug, Clone)]
pub struct Message {
    pub method: u16,
    pub class: u16,
    pub txid: [u8; 12],
    pub attrs: Vec<Attr>,
}

impl Message {
    fn new(method: u16, class: u16, txid: [u8; 12]) -> Self {
        Self {
            method,
            class,
            txid,
            attrs: Vec::new(),
        }
    }

    fn request(method: u16, txid: [u8; 12]) -> Self {
        Self::new(method, CLASS_REQUEST, txid)
    }

    fn push(&mut self, typ: u16, value: Vec<u8>) {
        self.attrs.push(Attr { typ, value });
    }

    fn get(&self, typ: u16) -> Option<&[u8]> {
        self.attrs.iter().find(|a| a.typ == typ).map(|a| &a.value[..])
    }

    fn is_success(&self) -> bool {
        self.class == CLASS_SUCCESS
    }

    fn is_error(&self) -> bool {
        self.class == CLASS_ERROR
    }

    /// ERROR-CODE as `(code, reason)` if present (RFC 5389 §15.6).
    fn error_code(&self) -> Option<(u16, String)> {
        let v = self.get(ATTR_ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        let class = v[2] as u16 & 0x07;
        let number = v[3] as u16;
        let code = class * 100 + number;
        let reason = String::from_utf8_lossy(&v[4..]).into_owned();
        Some((code, reason))
    }

    /// Serialize attributes (each 4-byte aligned). Not the full message — see
    /// [`Self::encode`], which prepends the header and applies MI/FINGERPRINT.
    fn encode_attrs(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for a in &self.attrs {
            out.extend_from_slice(&a.typ.to_be_bytes());
            out.extend_from_slice(&(a.value.len() as u16).to_be_bytes());
            out.extend_from_slice(&a.value);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        out
    }

    /// Encode the full wire message. If `key` is set, a MESSAGE-INTEGRITY
    /// attribute (long-term HMAC-SHA1) is appended; if `fingerprint`, a
    /// FINGERPRINT (CRC-32) is appended last (RFC 5389 §15.4/§15.5).
    fn encode(&self, key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        let mut msg = Vec::with_capacity(20 + 64);
        let typ = message_type(self.method, self.class);
        msg.extend_from_slice(&typ.to_be_bytes());
        msg.extend_from_slice(&[0, 0]); // length placeholder
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&self.txid);
        msg.extend_from_slice(&self.encode_attrs());

        if let Some(key) = key {
            // The HMAC covers the message with the length field set as if the MI
            // attribute (4 header + 20 value) were already present.
            let mi_len = (msg.len() - 20 + 24) as u16;
            set_length(&mut msg, mi_len);
            let mac = hmac_sha1(key, &msg);
            msg.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
            msg.extend_from_slice(&20u16.to_be_bytes());
            msg.extend_from_slice(&mac);
        }
        if fingerprint {
            // CRC-32 over the message with the length field set to include the
            // FINGERPRINT attribute (4 header + 4 value), XORed with 0x5354554E.
            let fp_len = (msg.len() - 20 + 8) as u16;
            set_length(&mut msg, fp_len);
            let crc = crc32(&msg) ^ 0x5354_554E;
            msg.extend_from_slice(&ATTR_FINGERPRINT.to_be_bytes());
            msg.extend_from_slice(&4u16.to_be_bytes());
            msg.extend_from_slice(&crc.to_be_bytes());
        }
        // Finalize the real length (attributes only).
        let final_len = (msg.len() - 20) as u16;
        set_length(&mut msg, final_len);
        msg
    }

    /// Parse a STUN message from the wire (attribute values borrowed into owned
    /// vecs). Returns `None` if it isn't a well-formed STUN message.
    fn decode(buf: &[u8]) -> Option<Message> {
        if buf.len() < 20 || u32::from_be_bytes(buf[4..8].try_into().ok()?) != MAGIC_COOKIE {
            return None;
        }
        let typ = u16::from_be_bytes(buf[0..2].try_into().ok()?);
        let len = u16::from_be_bytes(buf[2..4].try_into().ok()?) as usize;
        if 20 + len > buf.len() {
            return None;
        }
        let (method, class) = split_type(typ);
        let mut txid = [0u8; 12];
        txid.copy_from_slice(&buf[8..20]);
        let mut msg = Message::new(method, class, txid);

        let mut p = 20;
        let end = 20 + len;
        while p + 4 <= end {
            let atyp = u16::from_be_bytes(buf[p..p + 2].try_into().ok()?);
            let alen = u16::from_be_bytes(buf[p + 2..p + 4].try_into().ok()?) as usize;
            p += 4;
            if p + alen > end {
                return None;
            }
            msg.push(atyp, buf[p..p + alen].to_vec());
            p += alen;
            p += (4 - (alen % 4)) % 4; // 4-byte alignment padding
        }
        Some(msg)
    }

    /// Decode a XOR-MAPPED/RELAYED/PEER address attribute (RFC 5389 §15.2).
    fn xor_address(&self, typ: u16) -> Option<SocketAddr> {
        decode_xor_addr(self.get(typ)?, &self.txid)
    }
}

/// Overwrite the 2-byte STUN message-length field (bytes 2..4).
fn set_length(msg: &mut [u8], len: u16) {
    msg[2..4].copy_from_slice(&len.to_be_bytes());
}

/// The long-term-credential key: `MD5(username ":" realm ":" password)`
/// (RFC 5389 §15.4).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut buf = Vec::new();
    buf.extend_from_slice(username.as_bytes());
    buf.push(b':');
    buf.extend_from_slice(realm.as_bytes());
    buf.push(b':');
    buf.extend_from_slice(password.as_bytes());
    md5(&buf)
}

/// Encode a socket address as an XOR-MAPPED-ADDRESS attribute value.
fn encode_xor_addr(addr: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.push(0); // reserved
    let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
    match addr.ip() {
        IpAddr::V4(v4) => {
            out.push(0x01);
            out.extend_from_slice(&xport.to_be_bytes());
            let x = u32::from(v4) ^ MAGIC_COOKIE;
            out.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(v6) => {
            out.push(0x02);
            out.extend_from_slice(&xport.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            let seg = v6.octets();
            for i in 0..16 {
                out.push(seg[i] ^ mask[i]);
            }
        }
    }
    out
}

/// Decode a XOR-MAPPED-ADDRESS attribute value into a socket address.
fn decode_xor_addr(v: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let family = v[1];
    let xport = u16::from_be_bytes([v[2], v[3]]);
    let port = xport ^ (MAGIC_COOKIE >> 16) as u16;
    match family {
        0x01 if v.len() >= 8 => {
            let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            let ip = Ipv4Addr::from(x ^ MAGIC_COOKIE);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 if v.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            let mut seg = [0u8; 16];
            for i in 0..16 {
                seg[i] = v[4 + i] ^ mask[i];
            }
            Some(SocketAddr::new(IpAddr::V6(seg.into()), port))
        }
        _ => None,
    }
}

/// CRC-32 (ITU V.42 / zlib polynomial 0xEDB88320) for STUN FINGERPRINT.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The candidates learned from STUN/TURN: our reflexive address (as the TURN
/// server sees us) and the relayed transport address it allocated for us.
#[derive(Debug, Clone)]
pub struct TurnAllocation {
    /// Server-reflexive: our public `ip:port` as seen by the TURN server.
    pub mapped: Option<SocketAddr>,
    /// Relayed: the address the TURN server relays for us (our relay candidate).
    pub relayed: SocketAddr,
    /// Granted lifetime (seconds) before the allocation must be refreshed.
    pub lifetime: u32,
}

/// A TURN client over a bound [`UdpSocket`], authenticating to one relay with
/// long-term credentials (username / realm / password from `iceServersConfig`).
pub struct TurnClient {
    socket: UdpSocket,
    server: SocketAddr,
    username: String,
    realm: String,
    password: String,
    /// Server-provided NONCE (RFC 5389 long-term auth); refreshed on 438.
    nonce: Vec<u8>,
    /// Cached MI key `MD5(user:realm:pass)`.
    key: [u8; 16],
    next_txid: u64,
}

impl TurnClient {
    pub fn new(
        socket: UdpSocket,
        server: SocketAddr,
        username: &str,
        realm: &str,
        password: &str,
    ) -> Self {
        Self {
            socket,
            server,
            username: username.to_string(),
            realm: realm.to_string(),
            password: password.to_string(),
            nonce: Vec::new(),
            key: long_term_key(username, realm, password),
            next_txid: 0x5244_5049_0000_0001, // "RDPI" + counter, varied per request
        }
    }

    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    fn txid(&mut self) -> [u8; 12] {
        // Deterministic-but-unique transaction ids (no RNG dependency here; the
        // UDP 4-tuple + magic cookie already scope the exchange).
        self.next_txid = self.next_txid.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut id = [0u8; 12];
        id[..8].copy_from_slice(&self.next_txid.to_be_bytes());
        id[8..].copy_from_slice(&(self.next_txid as u32).to_be_bytes());
        id
    }

    /// Add USERNAME / REALM / NONCE and (implicitly) the MESSAGE-INTEGRITY key
    /// for an authenticated request.
    fn authenticate(&self, msg: &mut Message) {
        msg.push(ATTR_USERNAME, self.username.clone().into_bytes());
        msg.push(ATTR_REALM, self.realm.clone().into_bytes());
        msg.push(ATTR_NONCE, self.nonce.clone());
    }

    /// Send `msg` and wait for a matching-txid reply, retransmitting per RFC 5389
    /// (500 ms base, doubling) up to ~5 s total.
    fn transact(&self, wire: &[u8], txid: &[u8; 12]) -> io::Result<Message> {
        self.transact_within(wire, txid, Duration::from_secs(5))
    }

    /// [`Self::transact`] with a caller-chosen total deadline — used by the fast
    /// redirect probe ([`Self::resolve_backend`]), which must return well inside a
    /// live call-setup window rather than the full 5 s.
    fn transact_within(&self, wire: &[u8], txid: &[u8; 12], total: Duration) -> io::Result<Message> {
        let mut rto = Duration::from_millis(500).min(total);
        let deadline = Instant::now() + total;
        let mut buf = [0u8; 2048];
        loop {
            self.socket.send_to(wire, self.server)?;
            self.socket.set_read_timeout(Some(rto))?;
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) if from == self.server => {
                    if let Some(m) = Message::decode(&buf[..n]) {
                        if m.txid == *txid {
                            return Ok(m);
                        }
                    }
                    // Stray datagram (e.g. relayed Data) — ignore, keep waiting.
                }
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(ErrorKind::TimedOut, "STUN transaction timed out"));
            }
            rto = (rto * 2).min(Duration::from_millis(1600));
        }
    }

    /// STUN Binding: discover our server-reflexive address via this TURN server.
    pub fn binding(&mut self) -> io::Result<SocketAddr> {
        let txid = self.txid();
        let msg = Message::request(METHOD_BINDING, txid);
        let wire = msg.encode(None, true);
        let resp = self.transact(&wire, &txid)?;
        resp.xor_address(ATTR_XOR_MAPPED_ADDRESS)
            .or_else(|| resp.get(ATTR_MAPPED_ADDRESS).and_then(decode_plain_addr))
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "Binding without a mapped address"))
    }

    /// TURN Allocate a UDP relay. Handles the mandatory 401 (unauthenticated
    /// probe → learn REALM/NONCE → authenticated retry), 438 nonce refresh, and
    /// **300 Try Alternate** (Azure's TURN entry `51.5.255.240` is an anycast
    /// front-end that redirects to a unicast backend via `ALTERNATE-SERVER`).
    pub fn allocate(&mut self) -> io::Result<TurnAllocation> {
        // Outer loop follows ALTERNATE-SERVER redirects (bounded to avoid loops).
        for _redirect in 0..4 {
            // Unauthenticated Allocate → REALM + NONCE (or a 300 redirect).
            let txid = self.txid();
            let mut probe = Message::request(METHOD_ALLOCATE, txid);
            probe.push(ATTR_REQUESTED_TRANSPORT, vec![TRANSPORT_UDP, 0, 0, 0]);
            let resp = self.transact(&probe.encode(None, true), &txid)?;
            if matches!(resp.error_code(), Some((300, _))) {
                if self.follow_alternate(&resp)? {
                    continue;
                }
                return Err(io::Error::new(
                    ErrorKind::Other,
                    "300 Try Alternate without an ALTERNATE-SERVER address",
                ));
            }
            if let Some(nonce) = resp.get(ATTR_NONCE) {
                self.nonce = nonce.to_vec();
            }
            if let Some(realm) = resp.get(ATTR_REALM) {
                // Adopt the server's realm if it differs (recompute the MI key).
                let r = String::from_utf8_lossy(realm).into_owned();
                if r != self.realm {
                    self.realm = r;
                    self.key = long_term_key(&self.username, &self.realm, &self.password);
                }
            }

            // Authenticated Allocate (retry once on 438 Stale Nonce; a 300 here
            // re-enters the outer redirect loop).
            let mut redirected = false;
            for _ in 0..2 {
                let txid = self.txid();
                let mut req = Message::request(METHOD_ALLOCATE, txid);
                req.push(ATTR_REQUESTED_TRANSPORT, vec![TRANSPORT_UDP, 0, 0, 0]);
                self.authenticate(&mut req);
                let resp = self.transact(&req.encode(Some(&self.key), true), &txid)?;
                if resp.is_success() {
                    let relayed = resp.xor_address(ATTR_XOR_RELAYED_ADDRESS).ok_or_else(|| {
                        io::Error::new(ErrorKind::InvalidData, "Allocate without relay")
                    })?;
                    let lifetime = resp
                        .get(ATTR_LIFETIME)
                        .and_then(|v| v.get(0..4))
                        .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
                        .unwrap_or(600);
                    return Ok(TurnAllocation {
                        mapped: resp.xor_address(ATTR_XOR_MAPPED_ADDRESS),
                        relayed,
                        lifetime,
                    });
                }
                match resp.error_code() {
                    Some((438, _)) => {
                        if let Some(nonce) = resp.get(ATTR_NONCE) {
                            self.nonce = nonce.to_vec();
                        }
                        continue; // retry with fresh nonce
                    }
                    Some((300, _)) => {
                        if self.follow_alternate(&resp)? {
                            redirected = true;
                            break; // re-enter outer loop against the new server
                        }
                        return Err(io::Error::new(
                            ErrorKind::Other,
                            "300 Try Alternate without an ALTERNATE-SERVER address",
                        ));
                    }
                    Some((code, reason)) => {
                        return Err(io::Error::new(
                            ErrorKind::PermissionDenied,
                            format!("TURN Allocate failed: {code} {reason}"),
                        ));
                    }
                    None => break,
                }
            }
            if !redirected {
                break;
            }
        }
        Err(io::Error::new(
            ErrorKind::Other,
            "TURN Allocate failed (exhausted redirects)",
        ))
    }

    /// Follow `300 Try Alternate` redirects with unauthenticated Allocate probes to
    /// find the *unicast* TURN backend behind an anycast front-end, WITHOUT
    /// allocating anything. This exists to hand a redirect-free `turn:<ip>:<port>`
    /// URL to a stack that can't follow the redirect itself — webrtc-rs 0.17's TURN
    /// client gives up on a 300, which is exactly how Teams' (and W365's) Azure
    /// anycast relays answer the first Allocate. `per_probe` bounds each round trip
    /// so this returns well inside the caller's setup deadline; the 300 comes back
    /// on the first, unauthenticated probe, so no credentials are needed here.
    pub fn resolve_backend(&mut self, per_probe: Duration) -> io::Result<SocketAddr> {
        for _redirect in 0..4 {
            let txid = self.txid();
            let mut probe = Message::request(METHOD_ALLOCATE, txid);
            probe.push(ATTR_REQUESTED_TRANSPORT, vec![TRANSPORT_UDP, 0, 0, 0]);
            let resp = self.transact_within(&probe.encode(None, true), &txid, per_probe)?;
            if matches!(resp.error_code(), Some((300, _))) {
                if self.follow_alternate(&resp)? {
                    continue;
                }
                return Err(io::Error::new(
                    ErrorKind::Other,
                    "300 Try Alternate without an ALTERNATE-SERVER address",
                ));
            }
            // Any non-300 (a 401 challenge, or success) means this server
            // terminates the redirect chain: it's the backend to target.
            return Ok(self.server);
        }
        Err(io::Error::new(ErrorKind::Other, "exhausted TURN redirects"))
    }

    /// Follow a `300 Try Alternate`: point `self.server` at the `ALTERNATE-SERVER`
    /// address (plain-encoded like MAPPED-ADDRESS, RFC 5389 §15.11) and drop the
    /// stale nonce so the new server issues its own. Returns `true` if a redirect
    /// target was present.
    fn follow_alternate(&mut self, resp: &Message) -> io::Result<bool> {
        match resp.get(ATTR_ALTERNATE_SERVER).and_then(decode_plain_addr) {
            Some(alt) => {
                tracing::info!(from = %self.server, to = %alt, "TURN 300 Try Alternate — following redirect");
                self.server = alt;
                self.nonce.clear();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Install a permission so the relay will forward packets to/from `peer`
    /// (RFC 5766 §9). Required before Send/Data or ChannelData for that peer.
    pub fn create_permission(&mut self, peer: SocketAddr) -> io::Result<()> {
        let txid = self.txid();
        let mut req = Message::request(METHOD_CREATE_PERMISSION, txid);
        req.push(ATTR_XOR_PEER_ADDRESS, encode_xor_addr(peer, &txid));
        self.authenticate(&mut req);
        let resp = self.transact(&req.encode(Some(&self.key), true), &txid)?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("CreatePermission failed: {:?}", resp.error_code()),
            ))
        }
    }

    /// Bind a 16-bit channel number to `peer` (RFC 5766 §11) so bulk data can use
    /// the compact 4-byte ChannelData framing instead of Send/Data indications.
    pub fn channel_bind(&mut self, peer: SocketAddr, channel: u16) -> io::Result<()> {
        let txid = self.txid();
        let mut req = Message::request(METHOD_CHANNEL_BIND, txid);
        req.push(ATTR_CHANNEL_NUMBER, vec![(channel >> 8) as u8, channel as u8, 0, 0]);
        req.push(ATTR_XOR_PEER_ADDRESS, encode_xor_addr(peer, &txid));
        self.authenticate(&mut req);
        let resp = self.transact(&req.encode(Some(&self.key), true), &txid)?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("ChannelBind failed: {:?}", resp.error_code()),
            ))
        }
    }

    /// Frame `payload` as TURN ChannelData (RFC 5766 §11.5) for a bound channel:
    /// `[channel(2)][length(2)][payload]`, 4-byte padded on the wire.
    pub fn channel_data(channel: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&channel.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }
}

/// Decode a (non-XOR) MAPPED-ADDRESS attribute value.
fn decode_plain_addr(v: &[u8]) -> Option<SocketAddr> {
    if v.len() < 8 || v[1] != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]);
    let ip = Ipv4Addr::new(v[4], v[5], v[6], v[7]);
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_roundtrip() {
        // Known STUN/TURN message types (RFC 5389/5766).
        assert_eq!(message_type(METHOD_BINDING, CLASS_REQUEST), 0x0001);
        assert_eq!(message_type(METHOD_BINDING, CLASS_SUCCESS), 0x0101);
        assert_eq!(message_type(METHOD_ALLOCATE, CLASS_REQUEST), 0x0003);
        assert_eq!(message_type(METHOD_ALLOCATE, CLASS_SUCCESS), 0x0103);
        assert_eq!(message_type(METHOD_ALLOCATE, CLASS_ERROR), 0x0113);
        assert_eq!(message_type(METHOD_DATA, CLASS_INDICATION), 0x0017);
        assert_eq!(message_type(METHOD_SEND, CLASS_INDICATION), 0x0016);
        for &(m, c) in &[
            (METHOD_BINDING, CLASS_REQUEST),
            (METHOD_ALLOCATE, CLASS_ERROR),
            (METHOD_CHANNEL_BIND, CLASS_SUCCESS),
            (METHOD_CREATE_PERMISSION, CLASS_INDICATION),
        ] {
            assert_eq!(split_type(message_type(m, c)), (m, c));
        }
    }

    #[test]
    fn crc32_check_value() {
        // The canonical CRC-32 check value of "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn xor_mapped_address_roundtrip() {
        let txid = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        for addr in [
            "192.0.2.1:32853".parse().unwrap(),
            "1.2.3.4:5678".parse().unwrap(),
            "[2001:db8::1]:9999".parse().unwrap(),
        ] {
            let enc = encode_xor_addr(addr, &txid);
            assert_eq!(decode_xor_addr(&enc, &txid), Some(addr));
        }
    }

    #[test]
    fn xor_mapped_address_known_vector() {
        // RFC 5769 §2.2: XOR-MAPPED-ADDRESS decoding to 192.0.2.1:32853.
        // X-Port = 32853 ^ 0x2112 = 0x9cd5 → bytes 0x9cd5; X-Addr = 192.0.2.1 ^
        // 0x2112A442.
        let txid = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let value = [0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43];
        assert_eq!(
            decode_xor_addr(&value, &txid),
            Some("192.0.2.1:32853".parse().unwrap())
        );
    }

    #[test]
    fn encode_decode_roundtrip_with_integrity_and_fingerprint() {
        let txid = [1u8; 12];
        let key = long_term_key("user", "realm", "pass");
        let mut m = Message::request(METHOD_ALLOCATE, txid);
        m.push(ATTR_REQUESTED_TRANSPORT, vec![TRANSPORT_UDP, 0, 0, 0]);
        m.push(ATTR_USERNAME, b"user".to_vec());
        let wire = m.encode(Some(&key), true);

        let parsed = Message::decode(&wire).expect("decodes");
        assert_eq!(parsed.method, METHOD_ALLOCATE);
        assert_eq!(parsed.class, CLASS_REQUEST);
        assert_eq!(parsed.txid, txid);
        assert!(parsed.get(ATTR_MESSAGE_INTEGRITY).is_some());
        assert!(parsed.get(ATTR_FINGERPRINT).is_some());
        assert_eq!(parsed.get(ATTR_USERNAME), Some(&b"user"[..]));
        // FINGERPRINT must be the final attribute.
        assert_eq!(parsed.attrs.last().unwrap().typ, ATTR_FINGERPRINT);
    }

    #[test]
    fn error_code_parsing() {
        let txid = [2u8; 12];
        let mut m = Message::new(METHOD_ALLOCATE, CLASS_ERROR, txid);
        // 401 Unauthorized: class=4, number=1.
        let mut ec = vec![0u8, 0u8, 4u8, 1u8];
        ec.extend_from_slice(b"Unauthorized");
        m.push(ATTR_ERROR_CODE, ec);
        let wire = m.encode(None, false);
        let parsed = Message::decode(&wire).unwrap();
        assert_eq!(parsed.error_code().unwrap().0, 401);
    }

    #[test]
    fn channel_data_framing() {
        let framed = TurnClient::channel_data(0x4000, &[0xaa, 0xbb, 0xcc]);
        assert_eq!(&framed[0..2], &[0x40, 0x00]); // channel
        assert_eq!(&framed[2..4], &[0x00, 0x03]); // length
        assert_eq!(&framed[4..7], &[0xaa, 0xbb, 0xcc]);
        assert_eq!(framed.len(), 8); // padded to 4-byte boundary
    }
}
