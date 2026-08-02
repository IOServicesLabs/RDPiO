//! Multitransport bootstrap (MS-RDPBCGR 2.2.15) — the handshake that lets the
//! server move graphics onto a side-band UDP transport for lower latency.
//!
//! After the main (TCP) connection is up, a server that supports multitransport
//! sends a **Server Initiate Multitransport Request** naming a UDP protocol
//! (reliable `UDPFECR` or lossy `UDPFECL`) and a 16-byte security cookie. The
//! client opens a UDP socket, runs the RDP-UDP ([`crate::rdpudp`]) + RDPEMT
//! handshakes carrying that cookie, and replies with a **Client Initiate
//! Multitransport Response** carrying an `HRESULT` (success means it will use
//! the transport; an error means stay on TCP).
//!
//! This module is just the (un)marshalling of those two small PDUs; the socket
//! work and the TCP fallback live in the client driver. Declining is always
//! safe — the session continues on TCP.

/// `INITIATE_REQUEST_PROTOCOL_UDPFECR` — reliable UDP (lossless, retransmitted).
pub const PROTOCOL_UDPFECR: u16 = 0x0001;
/// `INITIATE_REQUEST_PROTOCOL_UDPFECL` — lossy UDP (forward-error-corrected).
pub const PROTOCOL_UDPFECL: u16 = 0x0002;

/// `S_OK` — the client accepts and will attempt the requested transport.
pub const HR_S_OK: u32 = 0x0000_0000;
/// `E_ABORT` — the client declines the transport (stay on TCP).
pub const HR_E_ABORT: u32 = 0x8000_4004;

/// A parsed Server Initiate Multitransport Request (2.2.15.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitiateRequest {
    pub request_id: u32,
    /// `PROTOCOL_UDPFECR` (reliable) or `PROTOCOL_UDPFECL` (lossy).
    pub requested_protocol: u16,
    /// 16-byte cookie echoed into the RDPEMT tunnel handshake.
    pub security_cookie: [u8; 16],
}

impl InitiateRequest {
    /// Whether this request is for the lossy (FEC) UDP channel, used for
    /// real-time graphics; the reliable channel carries everything else.
    pub fn is_lossy(&self) -> bool {
        self.requested_protocol == PROTOCOL_UDPFECL
    }

    /// Parse the 24-byte request body (requestId, requestedProtocol, reserved,
    /// securityCookie). On the TLS path the server prefixes the 4-byte Basic
    /// Security Header carrying `SEC_TRANSPORT_REQ` (2.2.15.1); that framing is
    /// tried first — but only committed when the stripped bytes actually parse,
    /// because a headerless requestId with bit 1 set looks identical up front.
    /// Returns `None` if too short or the protocol is unknown.
    pub fn parse(body: &[u8]) -> Option<Self> {
        if let (Some(flags), Some(0)) = (u16le(body, 0), u16le(body, 2)) {
            if flags & crate::security::SEC_TRANSPORT_REQ != 0 {
                if let Some(req) = Self::parse_body(&body[4..]) {
                    return Some(req);
                }
            }
        }
        Self::parse_body(body)
    }

    /// Parse a bare (header-stripped) 24-byte request body.
    fn parse_body(body: &[u8]) -> Option<Self> {
        if body.len() < 24 {
            return None;
        }
        let request_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let requested_protocol = u16::from_le_bytes([body[4], body[5]]);
        if requested_protocol != PROTOCOL_UDPFECR && requested_protocol != PROTOCOL_UDPFECL {
            return None;
        }
        // body[6..8] = reserved.
        let mut security_cookie = [0u8; 16];
        security_cookie.copy_from_slice(&body[8..24]);
        Some(Self {
            request_id,
            requested_protocol,
            security_cookie,
        })
    }
}

#[inline]
fn u16le(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

/// Build a Client Initiate Multitransport Response (2.2.15.2), ready to send on
/// the channel the request arrived on: the Basic Security Header carrying
/// `SEC_TRANSPORT_RSP`, then the echoed `request_id` and an `HRESULT`
/// (`HR_S_OK` to accept, `HR_E_ABORT` to decline). Sending the decline promptly
/// matters: a server that never hears back waits out its own multitransport
/// timeout before settling the session onto TCP at full rate.
pub fn response(request_id: u32, hr: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&crate::security::SEC_TRANSPORT_RSP.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flagsHi
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&hr.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_body(id: u32, proto: u16, cookie: [u8; 16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&proto.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // reserved
        b.extend_from_slice(&cookie);
        b
    }

    #[test]
    fn parses_lossy_request() {
        let cookie = [0xAB; 16];
        let req = InitiateRequest::parse(&request_body(0x1234, PROTOCOL_UDPFECL, cookie)).unwrap();
        assert_eq!(req.request_id, 0x1234);
        assert!(req.is_lossy());
        assert_eq!(req.security_cookie, cookie);
    }

    #[test]
    fn parses_reliable_request() {
        let req = InitiateRequest::parse(&request_body(7, PROTOCOL_UDPFECR, [0; 16])).unwrap();
        assert_eq!(req.requested_protocol, PROTOCOL_UDPFECR);
        assert!(!req.is_lossy());
    }

    #[test]
    fn rejects_unknown_protocol_or_short() {
        assert!(InitiateRequest::parse(&request_body(1, 0x00FF, [0; 16])).is_none());
        assert!(InitiateRequest::parse(&[0u8; 10]).is_none());
    }

    /// The TLS path delivers the request with its Basic Security Header
    /// (`SEC_TRANSPORT_REQ`) still attached; parse must strip it, or every
    /// field lands 4 bytes off (and the tunnel dials with a garbage cookie).
    #[test]
    fn parses_request_behind_security_header() {
        let cookie = [0xCD; 16];
        let mut wire = Vec::new();
        wire.extend_from_slice(&crate::security::SEC_TRANSPORT_REQ.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes()); // flagsHi
        wire.extend_from_slice(&request_body(0x0042, PROTOCOL_UDPFECL, cookie));
        let req = InitiateRequest::parse(&wire).unwrap();
        assert_eq!(req.request_id, 0x0042);
        assert!(req.is_lossy());
        assert_eq!(req.security_cookie, cookie);
    }

    #[test]
    fn response_roundtrip() {
        let r = response(0x1234, HR_E_ABORT);
        assert_eq!(r.len(), 12);
        // Basic Security Header: SEC_TRANSPORT_RSP, flagsHi 0.
        assert_eq!(&r[0..4], &[0x04, 0x00, 0x00, 0x00]);
        assert_eq!(u32::from_le_bytes([r[4], r[5], r[6], r[7]]), 0x1234);
        assert_eq!(u32::from_le_bytes([r[8], r[9], r[10], r[11]]), HR_E_ABORT);
    }
}
