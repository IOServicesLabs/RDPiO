//! TPKT (T.123) framing, X.224 (T.125 class-0) connection PDUs, and the RDP
//! Negotiation Request/Response they carry (MS-RDPBCGR 2.2.1.1–2.2.1.2).
//!
//! This is the first thing on the wire: the client sends a TPKT-framed X.224
//! Connection Request whose user data is an `RDP_NEG_REQ` advertising the
//! security protocols it supports (we request TLS + CredSSP for NLA). The
//! server replies with a Connection Confirm carrying either an `RDP_NEG_RSP`
//! (selected protocol) or an `RDP_NEG_FAILURE`.
//!
//! Endianness note: the TPKT length is **big-endian**, while the multi-byte
//! fields inside the RDP negotiation structures are **little-endian** (the RDP
//! convention).

use crate::{ensure, PduError, PduResult};

/// TPKT header length in bytes (version, reserved, length16).
pub const TPKT_HEADER_LEN: usize = 4;
/// TPKT version byte (always 3 for RDP).
pub const TPKT_VERSION: u8 = 3;

// X.224 TPDU type codes (the value occupies the high nibble of the type octet).
const X224_TPDU_CONNECTION_REQUEST: u8 = 0xE0;
const X224_TPDU_CONNECTION_CONFIRM: u8 = 0xD0;
const X224_TPDU_DATA: u8 = 0xF0;

/// Bytes of the X.224 CR/CC fixed header that follow the length indicator
/// (type + DST-REF[2] + SRC-REF[2] + class/options).
const X224_CRCC_FIXED: usize = 6;

// RDP negotiation message types (first byte of the structure).
const TYPE_RDP_NEG_REQ: u8 = 0x01;
const TYPE_RDP_NEG_RSP: u8 = 0x02;
const TYPE_RDP_NEG_FAILURE: u8 = 0x03;

/// Every RDP negotiation structure is exactly 8 bytes.
const RDP_NEG_SIZE: usize = 8;

bitflags::bitflags! {
    /// Security protocols advertised/selected in the X.224 exchange
    /// (MS-RDPBCGR 2.2.1.1.1 `requestedProtocols`). Standard RDP Security is the
    /// absence of any flag, i.e. [`SecurityProtocol::empty`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SecurityProtocol: u32 {
        /// TLS 1.x.
        const SSL = 0x0000_0001;
        /// CredSSP (NLA): TLS plus the CredSSP authentication exchange.
        const HYBRID = 0x0000_0002;
        /// Early User Authorization over RDSTLS.
        const RDSTLS = 0x0000_0004;
        /// CredSSP with Early User Authorization Result.
        const HYBRID_EX = 0x0000_0008;
        /// RDS AAD (Azure AD) authentication.
        const RDSAAD = 0x0000_0010;
    }
}

bitflags::bitflags! {
    /// Flags on the RDP Negotiation Request (MS-RDPBCGR 2.2.1.1.1).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct NegRequestFlags: u8 {
        const RESTRICTED_ADMIN_MODE_REQUIRED = 0x01;
        const REDIRECTED_AUTHENTICATION_MODE_REQUIRED = 0x02;
        const CORRELATION_INFO_PRESENT = 0x08;
    }
}

bitflags::bitflags! {
    /// Flags on the RDP Negotiation Response (MS-RDPBCGR 2.2.1.2.1).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct NegResponseFlags: u8 {
        const EXTENDED_CLIENT_DATA_SUPPORTED = 0x01;
        const DYNVC_GFX_PROTOCOL_SUPPORTED = 0x02;
        const RESTRICTED_ADMIN_MODE_SUPPORTED = 0x08;
        const REDIRECTED_AUTHENTICATION_MODE_SUPPORTED = 0x10;
    }
}

/// Reason a server rejected the negotiation (MS-RDPBCGR 2.2.1.2.2 `failureCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegFailureCode {
    SslRequiredByServer,
    SslNotAllowedByServer,
    SslCertNotOnServer,
    InconsistentFlags,
    HybridRequiredByServer,
    SslWithUserAuthRequiredByServer,
    Unknown(u32),
}

impl NegFailureCode {
    fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::SslRequiredByServer,
            2 => Self::SslNotAllowedByServer,
            3 => Self::SslCertNotOnServer,
            4 => Self::InconsistentFlags,
            5 => Self::HybridRequiredByServer,
            6 => Self::SslWithUserAuthRequiredByServer,
            other => Self::Unknown(other),
        }
    }
}

/// Read the total length of a TPKT PDU from its 4-byte header. Useful for the
/// transport read loop (read 4 bytes, learn how many more to read).
pub fn read_tpkt_len(header: &[u8]) -> PduResult<usize> {
    ensure(header, TPKT_HEADER_LEN)?;
    if header[0] != TPKT_VERSION {
        return Err(PduError::InvalidField {
            field: "tpkt_version",
            detail: format!("{:#04x}", header[0]),
        });
    }
    Ok(u16::from_be_bytes([header[2], header[3]]) as usize)
}

/// Client X.224 Connection Request carrying an RDP Negotiation Request.
#[derive(Debug, Clone)]
pub struct ConnectionRequest {
    /// Protocols we advertise. For NLA this is `SSL | HYBRID`.
    pub requested_protocols: SecurityProtocol,
    pub flags: NegRequestFlags,
    /// Optional `mstshash` cookie (the username hint), sent as
    /// `Cookie: mstshash=<value>\r\n` ahead of the negotiation request.
    pub cookie: Option<String>,
}

impl Default for ConnectionRequest {
    fn default() -> Self {
        Self {
            requested_protocols: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
            flags: NegRequestFlags::empty(),
            cookie: None,
        }
    }
}

impl ConnectionRequest {
    /// Encode the full TPKT + X.224 CR + RDP_NEG_REQ byte stream.
    pub fn encode(&self, out: &mut Vec<u8>) -> PduResult<()> {
        // X.224 user data = optional cookie line followed by the 8-byte neg req.
        let mut user_data = Vec::new();
        if let Some(cookie) = &self.cookie {
            user_data.extend_from_slice(b"Cookie: mstshash=");
            user_data.extend_from_slice(cookie.as_bytes());
            user_data.extend_from_slice(b"\r\n");
        }
        user_data.push(TYPE_RDP_NEG_REQ);
        user_data.push(self.flags.bits());
        user_data.extend_from_slice(&(RDP_NEG_SIZE as u16).to_le_bytes());
        user_data.extend_from_slice(&self.requested_protocols.bits().to_le_bytes());

        // X.224 length indicator counts the fixed header plus the user data.
        let li = X224_CRCC_FIXED + user_data.len();
        if li > u8::MAX as usize {
            return Err(PduError::InvalidField {
                field: "x224_li",
                detail: "connection request user data too large".into(),
            });
        }
        let tpkt_len = TPKT_HEADER_LEN + 1 /* LI octet */ + li;
        if tpkt_len > u16::MAX as usize {
            return Err(PduError::InvalidField {
                field: "tpkt_length",
                detail: "connection request exceeds TPKT length".into(),
            });
        }

        // TPKT header (big-endian length).
        out.push(TPKT_VERSION);
        out.push(0x00);
        out.extend_from_slice(&(tpkt_len as u16).to_be_bytes());
        // X.224 Connection Request.
        out.push(li as u8);
        out.push(X224_TPDU_CONNECTION_REQUEST);
        out.extend_from_slice(&[0x00, 0x00]); // DST-REF
        out.extend_from_slice(&[0x00, 0x00]); // SRC-REF
        out.push(0x00); // class options
        out.extend_from_slice(&user_data);
        Ok(())
    }

    /// Convenience: encode into a fresh `Vec`.
    pub fn to_bytes(&self) -> PduResult<Vec<u8>> {
        let mut out = Vec::new();
        self.encode(&mut out)?;
        Ok(out)
    }
}

/// Server X.224 Connection Confirm and the negotiation outcome it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionConfirm {
    /// Server accepted and selected a protocol.
    Response {
        flags: NegResponseFlags,
        selected_protocol: SecurityProtocol,
    },
    /// Server rejected the negotiation.
    Failure { failure_code: NegFailureCode },
    /// CC with no RDP negotiation structure (legacy / Standard RDP Security).
    NoNegotiation,
}

impl ConnectionConfirm {
    /// Decode a full TPKT + X.224 Connection Confirm from `src`, advancing the
    /// cursor past the whole PDU on success.
    pub fn decode(src: &mut &[u8]) -> PduResult<Self> {
        let tpkt_len = read_tpkt_len(src)?;
        ensure(src, tpkt_len)?;
        if tpkt_len < TPKT_HEADER_LEN + 1 + X224_CRCC_FIXED {
            return Err(PduError::InvalidField {
                field: "tpkt_length",
                detail: format!("{tpkt_len} too short for an X.224 connection confirm"),
            });
        }

        let li = src[4] as usize;
        let type_code = src[5];
        if type_code & 0xF0 != X224_TPDU_CONNECTION_CONFIRM {
            return Err(PduError::Unsupported {
                what: "x224 tpdu code",
                value: format!("{type_code:#04x}"),
            });
        }

        let result = if li <= X224_CRCC_FIXED {
            // No negotiation structure present.
            Self::NoNegotiation
        } else {
            let nego_off = TPKT_HEADER_LEN + 1 + X224_CRCC_FIXED;
            if tpkt_len < nego_off + RDP_NEG_SIZE {
                return Err(PduError::InvalidField {
                    field: "rdp_neg",
                    detail: "truncated RDP negotiation structure".into(),
                });
            }
            let nego = &src[nego_off..nego_off + RDP_NEG_SIZE];
            let kind = nego[0];
            let flags = nego[1];
            let payload = u32::from_le_bytes([nego[4], nego[5], nego[6], nego[7]]);
            match kind {
                TYPE_RDP_NEG_RSP => Self::Response {
                    flags: NegResponseFlags::from_bits_truncate(flags),
                    selected_protocol: SecurityProtocol::from_bits_truncate(payload),
                },
                TYPE_RDP_NEG_FAILURE => Self::Failure {
                    failure_code: NegFailureCode::from_u32(payload),
                },
                other => {
                    return Err(PduError::Unsupported {
                        what: "rdp negotiation type",
                        value: format!("{other:#04x}"),
                    })
                }
            }
        };

        *src = &src[tpkt_len..];
        Ok(result)
    }
}

/// The 3-byte X.224 Data (DT) header (`LI=2, DT|EOT`) that prefixes every MCS
/// PDU once the connection is established.
pub const X224_DATA_HEADER: [u8; 3] = [0x02, X224_TPDU_DATA, 0x80];

/// Append a TPKT + X.224 Data header sized for a `payload_len`-byte MCS payload.
pub fn write_data_header(payload_len: usize, out: &mut Vec<u8>) -> PduResult<()> {
    let tpkt_len = TPKT_HEADER_LEN + X224_DATA_HEADER.len() + payload_len;
    if tpkt_len > u16::MAX as usize {
        return Err(PduError::InvalidField {
            field: "tpkt_length",
            detail: "data PDU exceeds TPKT length".into(),
        });
    }
    out.push(TPKT_VERSION);
    out.push(0x00);
    out.extend_from_slice(&(tpkt_len as u16).to_be_bytes());
    out.extend_from_slice(&X224_DATA_HEADER);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_request_nla_matches_known_bytes() {
        let cr = ConnectionRequest::default(); // SSL | HYBRID, no cookie
        let bytes = cr.to_bytes().unwrap();
        // TPKT(03 00 00 13) LI(0e) CR(e0) DST(0000) SRC(0000) CLASS(00)
        // RDP_NEG_REQ: type=01 flags=00 len=0008(LE) protocols=00000003(LE)
        let expected = [
            0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08,
            0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn connection_request_with_cookie_lengths_are_consistent() {
        let cr = ConnectionRequest {
            cookie: Some("alice".into()),
            ..Default::default()
        };
        let bytes = cr.to_bytes().unwrap();
        // TPKT length (big-endian) must equal the buffer length.
        let tpkt_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        assert_eq!(tpkt_len, bytes.len());
        // X.224 LI must equal everything after the LI octet.
        let li = bytes[4] as usize;
        assert_eq!(li, bytes.len() - (TPKT_HEADER_LEN + 1));
        // The cookie line is present.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Cookie: mstshash=alice\r\n"));
    }

    #[test]
    fn decode_negotiation_response_selecting_hybrid() {
        let confirm = [
            0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00, // CC + refs
            0x02, 0x01, 0x08, 0x00, 0x02, 0x00, 0x00, 0x00, // NEG_RSP flags=01 proto=HYBRID
        ];
        let mut cur: &[u8] = &confirm;
        let parsed = ConnectionConfirm::decode(&mut cur).unwrap();
        assert_eq!(
            parsed,
            ConnectionConfirm::Response {
                flags: NegResponseFlags::EXTENDED_CLIENT_DATA_SUPPORTED,
                selected_protocol: SecurityProtocol::HYBRID,
            }
        );
        assert!(cur.is_empty(), "whole PDU consumed");
    }

    #[test]
    fn decode_negotiation_failure_hybrid_required() {
        let confirm = [
            0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x03, 0x00, 0x08, 0x00, 0x05, 0x00, 0x00, 0x00, // NEG_FAILURE code=5
        ];
        let mut cur: &[u8] = &confirm;
        let parsed = ConnectionConfirm::decode(&mut cur).unwrap();
        assert_eq!(
            parsed,
            ConnectionConfirm::Failure {
                failure_code: NegFailureCode::HybridRequiredByServer,
            }
        );
    }

    #[test]
    fn decode_rejects_short_buffer() {
        // Claims length 19 but only 10 bytes provided.
        let truncated = [0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00];
        let mut cur: &[u8] = &truncated;
        assert!(ConnectionConfirm::decode(&mut cur).is_err());
    }

    #[test]
    fn tpkt_len_roundtrip() {
        let cr = ConnectionRequest::default().to_bytes().unwrap();
        assert_eq!(read_tpkt_len(&cr).unwrap(), cr.len());
    }
}
