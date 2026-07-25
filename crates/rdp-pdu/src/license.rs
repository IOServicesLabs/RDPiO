//! RDP licensing phase classification (MS-RDPBCGR 2.2.1.12, MS-RDPELE).
//!
//! After the Client Info PDU the server drives a short licensing exchange. For
//! the overwhelmingly common case it sends a `SERVER_LICENSE_ERROR_PDU` with
//! `STATUS_VALID_CLIENT` ("no CAL needed — proceed"), and the client simply
//! continues to capability exchange. A server that enforces per-device/per-user
//! CAL issuance instead drives a full exchange (`LICENSE_REQUEST` →
//! `PLATFORM_CHALLENGE` → `NEW_LICENSE`), which requires the licensing crypto.
//!
//! This module *classifies* an incoming licensing PDU so the session layer can
//! react: proceed when licensing is satisfied, or fail with a precise message
//! when a server demands full CAL issuance (which the client doesn't perform).
//! It is sans-I/O and tolerant of whether a Basic Security Header still precedes
//! the preamble (it does on the TLS path and the unencrypted legacy path; the
//! encrypted legacy path strips it during decryption).

/// Licensing message types (`LICENSE_PREAMBLE.bMsgType`).
pub const LICENSE_REQUEST: u8 = 0x01;
pub const PLATFORM_CHALLENGE: u8 = 0x02;
pub const NEW_LICENSE: u8 = 0x03;
pub const UPGRADE_LICENSE: u8 = 0x04;
pub const LICENSE_INFO: u8 = 0x12;
pub const NEW_LICENSE_REQUEST: u8 = 0x13;
pub const PLATFORM_CHALLENGE_RESPONSE: u8 = 0x15;
pub const ERROR_ALERT: u8 = 0xFF;

/// Low nibble of the preamble `flags` byte: licensing protocol version 3.0.
pub const PREAMBLE_VERSION_3_0: u8 = 0x03;
const PREAMBLE_VERSION_MASK: u8 = 0x0F;

/// `dwErrorCode` values of a `SERVER_LICENSE_ERROR_PDU`.
pub const ERR_INVALID_SERVER_CERTIFICATE: u32 = 0x01;
pub const ERR_NO_LICENSE: u32 = 0x02;
pub const ERR_INVALID_MAC: u32 = 0x03;
pub const ERR_INVALID_SCOPE: u32 = 0x04;
pub const ERR_NO_LICENSE_SERVER: u32 = 0x06;
/// The important one: the client is licensed / no CAL required → proceed.
pub const STATUS_VALID_CLIENT: u32 = 0x07;
pub const ERR_INVALID_CLIENT: u32 = 0x08;

/// A classified licensing PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseMessage {
    /// `SERVER_LICENSE_ERROR_PDU`: an error/status code plus a state transition.
    ErrorAlert {
        error_code: u32,
        state_transition: u32,
    },
    /// Server requests a full CAL exchange (expects a `NEW_LICENSE_REQUEST`).
    Request,
    /// Server issued a platform challenge (full per-device CAL issuance).
    PlatformChallenge,
    /// Server granted a new license — licensing is complete.
    NewLicense,
    /// Server upgraded an existing license — licensing is complete.
    Upgrade,
    /// A recognised preamble this client does not specifically model.
    Other(u8),
}

impl LicenseMessage {
    /// Whether licensing is satisfied and the client should proceed to the
    /// capability exchange (valid-client status, or a granted/upgraded license).
    pub fn is_complete(&self) -> bool {
        match self {
            LicenseMessage::ErrorAlert { error_code, .. } => *error_code == STATUS_VALID_CLIENT,
            LicenseMessage::NewLicense | LicenseMessage::Upgrade => true,
            _ => false,
        }
    }

    /// Whether the server is demanding full CAL issuance, which this client does
    /// not perform (so the connection can't proceed past licensing).
    pub fn demands_cal_issuance(&self) -> bool {
        matches!(
            self,
            LicenseMessage::Request | LicenseMessage::PlatformChallenge
        )
    }
}

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Is `msg` a licensing message type we recognise?
fn known_type(msg: u8) -> bool {
    matches!(
        msg,
        LICENSE_REQUEST
            | PLATFORM_CHALLENGE
            | NEW_LICENSE
            | UPGRADE_LICENSE
            | LICENSE_INFO
            | NEW_LICENSE_REQUEST
            | PLATFORM_CHALLENGE_RESPONSE
            | ERROR_ALERT
    )
}

/// Validate and read a `LICENSE_PREAMBLE` at `off`: `bMsgType(1)`, `flags(1)`,
/// `wMsgSize(2, LE)`. A valid preamble has a known message type, the 3.0 version
/// nibble, and a size that fits the buffer.
fn read_preamble(data: &[u8], off: usize) -> Option<(u8, usize)> {
    let msg = *data.get(off)?;
    let flags = *data.get(off + 1)?;
    let size = u16le(data, off + 2)? as usize;
    if known_type(msg)
        && (flags & PREAMBLE_VERSION_MASK) == PREAMBLE_VERSION_3_0
        && size >= 4
        && off + size <= data.len()
    {
        Some((msg, off))
    } else {
        None
    }
}

/// Classify a licensing PDU's plaintext. Accepts the preamble either at the
/// start (the encrypted legacy path strips the Basic Security Header during
/// decryption) or 4 bytes in (a Basic Security Header still precedes it on the
/// TLS path and the unencrypted legacy path). Returns `None` if it isn't a
/// recognisable licensing message — the caller should then try other PDU types.
pub fn parse_license_message(plaintext: &[u8]) -> Option<LicenseMessage> {
    // Try the preamble directly, then after a 4-byte Basic Security Header.
    let (msg, off) = read_preamble(plaintext, 0).or_else(|| read_preamble(plaintext, 4))?;
    let body = off + 4; // first byte after the preamble
    Some(match msg {
        ERROR_ALERT => LicenseMessage::ErrorAlert {
            error_code: u32le(plaintext, body)?,
            state_transition: u32le(plaintext, body + 4)?,
        },
        LICENSE_REQUEST => LicenseMessage::Request,
        PLATFORM_CHALLENGE => LicenseMessage::PlatformChallenge,
        NEW_LICENSE => LicenseMessage::NewLicense,
        UPGRADE_LICENSE => LicenseMessage::Upgrade,
        other => LicenseMessage::Other(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a preamble + body with a 3.0 version flag.
    fn pdu(msg: u8, body: &[u8]) -> Vec<u8> {
        let size = (4 + body.len()) as u16;
        let mut v = vec![msg, PREAMBLE_VERSION_3_0];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn error_alert_body(error: u32, state: u32) -> Vec<u8> {
        let mut b = vec![];
        b.extend_from_slice(&error.to_le_bytes());
        b.extend_from_slice(&state.to_le_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]); // empty LICENSE_BINARY_BLOB
        b
    }

    #[test]
    fn valid_client_is_complete() {
        let pdu = pdu(ERROR_ALERT, &error_alert_body(STATUS_VALID_CLIENT, 0x02));
        let msg = parse_license_message(&pdu).unwrap();
        assert_eq!(
            msg,
            LicenseMessage::ErrorAlert {
                error_code: STATUS_VALID_CLIENT,
                state_transition: 0x02,
            }
        );
        assert!(msg.is_complete());
        assert!(!msg.demands_cal_issuance());
    }

    #[test]
    fn other_error_is_not_complete() {
        let pdu = pdu(ERROR_ALERT, &error_alert_body(ERR_NO_LICENSE_SERVER, 0x01));
        let msg = parse_license_message(&pdu).unwrap();
        assert!(!msg.is_complete());
        assert!(!msg.demands_cal_issuance());
    }

    #[test]
    fn request_and_challenge_demand_cal() {
        assert!(parse_license_message(&pdu(LICENSE_REQUEST, &[0u8; 40]))
            .unwrap()
            .demands_cal_issuance());
        assert!(parse_license_message(&pdu(PLATFORM_CHALLENGE, &[0u8; 40]))
            .unwrap()
            .demands_cal_issuance());
    }

    #[test]
    fn new_and_upgrade_license_complete() {
        assert!(parse_license_message(&pdu(NEW_LICENSE, &[0u8; 8]))
            .unwrap()
            .is_complete());
        assert!(parse_license_message(&pdu(UPGRADE_LICENSE, &[0u8; 8]))
            .unwrap()
            .is_complete());
    }

    #[test]
    fn skips_a_basic_security_header() {
        // SEC_LICENSE_PKT (0x0080) flags + flagsHi, then the preamble.
        let mut wire = vec![0x80, 0x00, 0x00, 0x00];
        wire.extend_from_slice(&pdu(ERROR_ALERT, &error_alert_body(STATUS_VALID_CLIENT, 0x02)));
        assert!(parse_license_message(&wire).unwrap().is_complete());
    }

    #[test]
    fn rejects_non_license_pdus() {
        // A Demand Active-ish share control header: not a licensing preamble.
        assert_eq!(
            parse_license_message(&[0x11, 0x00, 0x11, 0x00, 0xea, 0x03, 1, 0, 0, 0]),
            None
        );
        assert_eq!(parse_license_message(&[]), None);
    }
}
