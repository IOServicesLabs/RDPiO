//! Server Set Error Info PDU (MS-RDPBCGR 2.2.5.1): the reason the server is
//! disconnecting (or `0` = none). It rides the I/O channel as a Share Data PDU
//! (`PDUTYPE2_SET_ERROR_INFO`) with a single 32-bit `errorInfo` code. Surfacing
//! it turns an opaque "connection closed" into "you were idle-timed-out" /
//! "logged on elsewhere" / "access denied", etc.

use crate::finalization::{data_pdu_type2, PDUTYPE2_SET_ERROR_INFO};

/// `errorInfo == 0`: no error (a normal/locally-initiated close).
pub const ERRINFO_NONE: u32 = 0x0000_0000;

/// Parse a Set Error Info PDU's `errorInfo` code from a Share Data PDU's
/// plaintext (`pduType2 == PDUTYPE2_SET_ERROR_INFO`). Returns `None` if this
/// isn't a Set Error Info PDU. The code follows the 18-byte Share Data Header.
pub fn parse_set_error_info(plaintext: &[u8]) -> Option<u32> {
    if data_pdu_type2(plaintext) != Some(PDUTYPE2_SET_ERROR_INFO) {
        return None;
    }
    let b = plaintext.get(18..22)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A human-readable description of an `errorInfo` code (MS-RDPBCGR 2.2.5.1.1).
/// Covers the disconnect reasons users actually see; the large protocol-error
/// range (`0x10C9+`) collapses to a single "protocol error" line.
pub fn describe(code: u32) -> &'static str {
    match code {
        ERRINFO_NONE => "no error (normal disconnect)",
        0x0000_0001 => "disconnected by server admin (RPC initiated)",
        0x0000_0002 => "logged off by server admin (RPC initiated)",
        0x0000_0003 => "idle session timeout",
        0x0000_0004 => "logon timeout",
        0x0000_0005 => "disconnected by another connection (logged on elsewhere)",
        0x0000_0006 => "server out of memory",
        0x0000_0007 => "server denied the connection",
        0x0000_0009 => "insufficient privileges",
        0x0000_000A => "fresh credentials required",
        0x0000_000B => "disconnected by user (RPC initiated)",
        0x0000_000C => "logged off by user",
        0x0000_0010 => "license: internal error",
        0x0000_0011 => "license: no license server available",
        0x0000_0012 => "license: no license / CAL available for this client",
        0x0000_0013 => "license: bad client message",
        0x0000_0014 => "license: hardware ID does not match the license",
        0x0000_0015 => "license: bad client license",
        0x0000_0016 => "license: cannot finish the licensing protocol",
        0x0000_0017 => "license: client ended the licensing protocol",
        0x0000_0018 => "license: bad client encryption",
        0x0000_0019 => "license: cannot upgrade the license",
        0x0000_001A => "license: no remote connections allowed",
        // 0x1xxx — protocol violations the server detected in our PDUs. Useful
        // to know one occurred; the specific sub-code is for protocol debugging.
        0x0000_10C9..=0x0000_1193 => "server reported a protocol error in a client PDU",
        _ => "unknown disconnect reason",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finalization::share_data;

    #[test]
    fn parses_error_info_code() {
        // Share Data PDU of type SET_ERROR_INFO carrying ERRINFO_IDLE_TIMEOUT.
        let pdu = share_data(0x0001_03EA, 1002, PDUTYPE2_SET_ERROR_INFO, &0x3u32.to_le_bytes());
        assert_eq!(parse_set_error_info(&pdu), Some(0x3));
        assert_eq!(describe(0x3), "idle session timeout");
    }

    #[test]
    fn ignores_other_pdu_types() {
        let pdu = share_data(1, 1002, crate::finalization::PDUTYPE2_FONTMAP, &[0, 0, 0, 0]);
        assert_eq!(parse_set_error_info(&pdu), None);
    }

    #[test]
    fn describes_common_and_protocol_codes() {
        assert_eq!(describe(ERRINFO_NONE), "no error (normal disconnect)");
        assert_eq!(
            describe(0x5),
            "disconnected by another connection (logged on elsewhere)"
        );
        assert_eq!(
            describe(0x10C9),
            "server reported a protocol error in a client PDU"
        );
        assert_eq!(describe(0xDEAD_BEEF), "unknown disconnect reason");
    }
}
