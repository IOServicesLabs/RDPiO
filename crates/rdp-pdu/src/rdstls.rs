//! RDSTLS authentication (MS-RDPBCGR 2.2.17) — the reverse-connect auth used by
//! Azure Virtual Desktop / Windows 365 once the X.224 negotiation selects
//! `PROTOCOL_RDSTLS` (0x04).
//!
//! After a TLS session is established with the target host, the two sides
//! exchange four PDUs:
//!
//! 1. **Client → Server** Capabilities
//! 2. **Server → Client** Capabilities
//! 3. **Client → Server** Authentication Request (password-credentials variant)
//! 4. **Server → Client** Authentication Response (a result code)
//!
//! For W365/AVD the auth request carries the broker-issued material from the ARM
//! `/api/arm/v2/connections` response: the `redirectedAuthGuid` becomes the
//! `RedirectionGuid`, and the `redirectedAuthBlob` becomes the password field.
//!
//! Every field is little-endian; strings are length-prefixed UTF-16LE. This
//! module is the pure wire codec (the socket/TLS I/O lives in the client).

/// `RDSTLS_VERSION_1`.
pub const VERSION_1: u16 = 0x0001;

// PDU types (the `PDUType` field after `Version`).
/// `RDSTLS_TYPE_CAPABILITIES`.
pub const TYPE_CAPABILITIES: u16 = 0x0001;
/// `RDSTLS_TYPE_AUTHREQ`.
pub const TYPE_AUTHREQ: u16 = 0x0002;
/// `RDSTLS_TYPE_AUTHRSP`.
pub const TYPE_AUTHRSP: u16 = 0x0004;

// Data types (the `DataType` field after the header).
/// `RDSTLS_DATA_CAPABILITIES`.
pub const DATA_CAPABILITIES: u16 = 0x0001;
/// `RDSTLS_DATA_PASSWORD_CREDS`.
pub const DATA_PASSWORD_CREDS: u16 = 0x0001;
/// `RDSTLS_DATA_RESULT_CODE`.
pub const DATA_RESULT_CODE: u16 = 0x0001;

/// `RDSTLS_RESULT_SUCCESS` — authentication accepted.
pub const RESULT_SUCCESS: u32 = 0x0000_0000;

/// Append a length-prefixed, null-terminated UTF-16LE string. The length counts
/// bytes and includes the terminator, so an empty string is `02 00 00 00`.
fn write_utf16(out: &mut Vec<u8>, s: &str) {
    let units: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    out.extend_from_slice(&((units.len() * 2) as u16).to_le_bytes());
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
}

/// Build the client's RDSTLS **Capabilities** PDU (8 bytes).
pub fn build_capabilities() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&VERSION_1.to_le_bytes());
    out.extend_from_slice(&TYPE_CAPABILITIES.to_le_bytes());
    out.extend_from_slice(&DATA_CAPABILITIES.to_le_bytes());
    out.extend_from_slice(&VERSION_1.to_le_bytes()); // SupportedVersions
    out
}

/// Extract the server's advertised `SupportedVersions` from its Capabilities PDU
/// (the 4th u16). W365/AVD targets advertise `0x0003`, not the documented
/// `0x0001`; the client should echo the negotiated version in its AuthReq.
pub fn capabilities_version(data: &[u8]) -> Option<u16> {
    if data.len() >= 8 && is_capabilities(data) {
        Some(u16::from_le_bytes([data[6], data[7]]))
    } else {
        None
    }
}

/// Build the **Authentication Request** PDU (password-credentials variant).
///
/// `version` is the RDSTLS version to declare — normally the one the server
/// advertised in its Capabilities PDU ([`capabilities_version`]); pass
/// [`VERSION_1`] for the documented protocol.
/// `redirection_guid` is the raw bytes of the broker's `redirectedAuthGuid`;
/// `password` is the raw bytes of the broker's `redirectedAuthBlob` (for W365)
/// or the user's password otherwise. `username`/`domain` are UTF-16LE encoded.
pub fn build_auth_request_password(
    version: u16,
    redirection_guid: &[u8],
    username: &str,
    domain: &str,
    password: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&TYPE_AUTHREQ.to_le_bytes());
    out.extend_from_slice(&DATA_PASSWORD_CREDS.to_le_bytes());
    // RedirectionGuid: UINT16 length (bytes) then the raw GUID bytes.
    out.extend_from_slice(&(redirection_guid.len() as u16).to_le_bytes());
    out.extend_from_slice(redirection_guid);
    write_utf16(&mut out, username);
    write_utf16(&mut out, domain);
    // Password: UINT16 length then the raw blob (not UTF-16, not terminated).
    out.extend_from_slice(&(password.len() as u16).to_le_bytes());
    out.extend_from_slice(password);
    out
}

/// Parse the server's **Authentication Response**, returning its result code
/// (`RESULT_SUCCESS == 0`). `None` if the buffer isn't a well-formed auth
/// response.
pub fn parse_auth_response(data: &[u8]) -> Option<u32> {
    // Version(2) + PDUType(2) + DataType(2) + ResultCode(4) = 10 bytes.
    if data.len() < 10 {
        return None;
    }
    let pdu_type = u16::from_le_bytes([data[2], data[3]]);
    if pdu_type != TYPE_AUTHRSP {
        return None;
    }
    Some(u32::from_le_bytes([data[6], data[7], data[8], data[9]]))
}

/// True if `data` looks like an RDSTLS Capabilities PDU (the server's reply to
/// our capabilities).
pub fn is_capabilities(data: &[u8]) -> bool {
    data.len() >= 4 && u16::from_le_bytes([data[2], data[3]]) == TYPE_CAPABILITIES
}

/// A human-readable name for a non-success RDSTLS result code (Win32-style).
pub fn result_name(code: u32) -> &'static str {
    match code {
        0x0000_0000 => "SUCCESS",
        0x0000_0005 => "ACCESS_DENIED",
        0x0000_052E => "LOGON_FAILURE",
        0x0000_0530 => "INVALID_LOGON_HOURS",
        0x0000_0532 => "PASSWORD_EXPIRED",
        0x0000_0533 => "ACCOUNT_DISABLED",
        0x0000_0773 => "PASSWORD_MUST_CHANGE",
        0x0000_0775 => "ACCOUNT_LOCKED_OUT",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_layout() {
        let pdu = build_capabilities();
        assert_eq!(pdu.len(), 8);
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]), VERSION_1);
        assert_eq!(u16::from_le_bytes([pdu[2], pdu[3]]), TYPE_CAPABILITIES);
        assert_eq!(u16::from_le_bytes([pdu[4], pdu[5]]), DATA_CAPABILITIES);
        assert_eq!(u16::from_le_bytes([pdu[6], pdu[7]]), VERSION_1);
        assert!(is_capabilities(&pdu));
    }

    #[test]
    fn auth_request_layout_and_fields() {
        let guid = [0xABu8; 16];
        let pdu = build_auth_request_password(VERSION_1, &guid, "user", "", b"BLOBBYTES");
        // Header.
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]), VERSION_1);
        assert_eq!(u16::from_le_bytes([pdu[2], pdu[3]]), TYPE_AUTHREQ);
        assert_eq!(u16::from_le_bytes([pdu[4], pdu[5]]), DATA_PASSWORD_CREDS);
        // RedirectionGuid length + bytes.
        assert_eq!(u16::from_le_bytes([pdu[6], pdu[7]]), 16);
        assert_eq!(&pdu[8..24], &guid);
        // Username "user": len = (4+1)*2 = 10 bytes, UTF-16LE.
        let mut off = 24;
        assert_eq!(u16::from_le_bytes([pdu[off], pdu[off + 1]]), 10);
        assert_eq!(&pdu[off + 2..off + 4], &[b'u', 0]);
        off += 2 + 10;
        // Empty domain: len = 2 (just the null terminator).
        assert_eq!(u16::from_le_bytes([pdu[off], pdu[off + 1]]), 2);
        assert_eq!(&pdu[off + 2..off + 4], &[0, 0]);
        off += 2 + 2;
        // Password: len + raw bytes (not UTF-16).
        assert_eq!(u16::from_le_bytes([pdu[off], pdu[off + 1]]), 9);
        assert_eq!(&pdu[off + 2..off + 11], b"BLOBBYTES");
    }

    #[test]
    fn auth_response_parses_success_and_failure() {
        let mut ok = Vec::new();
        ok.extend_from_slice(&VERSION_1.to_le_bytes());
        ok.extend_from_slice(&TYPE_AUTHRSP.to_le_bytes());
        ok.extend_from_slice(&DATA_RESULT_CODE.to_le_bytes());
        ok.extend_from_slice(&RESULT_SUCCESS.to_le_bytes());
        assert_eq!(parse_auth_response(&ok), Some(RESULT_SUCCESS));

        let mut fail = Vec::new();
        fail.extend_from_slice(&VERSION_1.to_le_bytes());
        fail.extend_from_slice(&TYPE_AUTHRSP.to_le_bytes());
        fail.extend_from_slice(&DATA_RESULT_CODE.to_le_bytes());
        fail.extend_from_slice(&0x0000_052Eu32.to_le_bytes());
        assert_eq!(parse_auth_response(&fail), Some(0x0000_052E));
        assert_eq!(result_name(0x0000_052E), "LOGON_FAILURE");
    }

    #[test]
    fn auth_response_rejects_wrong_type_or_short() {
        assert_eq!(parse_auth_response(&[0u8; 4]), None);
        // A capabilities PDU is not an auth response.
        assert_eq!(parse_auth_response(&build_capabilities()), None);
    }
}
