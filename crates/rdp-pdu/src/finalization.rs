//! Connection finalization (MS-RDPBCGR 1.3.1.1 step 11): the Synchronize,
//! Control (Cooperate / Request Control), and Font List PDUs the client sends,
//! and detection of the server's Font Map PDU that marks the session Active.
//!
//! All of these are Share Data PDUs (a Share Control Header of type DATAPDU
//! wrapping the Share Data Header and a small body). They are carried to the
//! server inside an MCS Send Data Request on the I/O channel.

use crate::ensure;

// Share Control Header pduType for a Data PDU (PDUTYPE_DATAPDU=7 | version 0x10).
const PDUTYPE_DATA: u16 = 0x17;

/// Share Data Header `pduType2` values.
pub const PDUTYPE2_UPDATE: u8 = 2;
pub const PDUTYPE2_CONTROL: u8 = 20;
pub const PDUTYPE2_POINTER: u8 = 27;
pub const PDUTYPE2_SYNCHRONIZE: u8 = 31;
pub const PDUTYPE2_SAVE_SESSION_INFO: u8 = 38;
pub const PDUTYPE2_FONTLIST: u8 = 39;
pub const PDUTYPE2_FONTMAP: u8 = 40;
pub const PDUTYPE2_SET_ERROR_INFO: u8 = 47;

/// Control PDU actions.
pub const CTRLACTION_REQUEST_CONTROL: u16 = 0x0001;
pub const CTRLACTION_GRANTED_CONTROL: u16 = 0x0002;
pub const CTRLACTION_COOPERATE: u16 = 0x0004;

/// Bytes of a Share Data Header (Share Control Header + the data header fields).
const SHARE_DATA_HEADER_LEN: usize = 18;

#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Wrap `body` in a Share Data Header for `pdu_type2`.
pub(crate) fn share_data(share_id: u32, user_id: u16, pdu_type2: u8, body: &[u8]) -> Vec<u8> {
    let total = (SHARE_DATA_HEADER_LEN + body.len()) as u16;
    let mut out = Vec::with_capacity(total as usize);
    // Share Control Header.
    put_u16(total, &mut out);
    put_u16(PDUTYPE_DATA, &mut out);
    put_u16(user_id, &mut out);
    // Share Data Header.
    put_u32(share_id, &mut out);
    out.push(0); // pad1
    out.push(1); // streamId = STREAM_LOW
    put_u16(total, &mut out); // uncompressedLength = whole packet
    out.push(pdu_type2);
    out.push(0); // compressedType
    put_u16(0, &mut out); // compressedLength
    out.extend_from_slice(body);
    out
}

/// Client Synchronize PDU.
pub fn synchronize_pdu(share_id: u32, user_id: u16, target_user: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(1, &mut body); // messageType = SYNCMSGTYPE_SYNC
    put_u16(target_user, &mut body);
    share_data(share_id, user_id, PDUTYPE2_SYNCHRONIZE, &body)
}

/// Control PDU with the given action (Cooperate / Request Control / …).
pub fn control_pdu(share_id: u32, user_id: u16, action: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(action, &mut body);
    put_u16(0, &mut body); // grantId
    put_u32(0, &mut body); // controlId
    share_data(share_id, user_id, PDUTYPE2_CONTROL, &body)
}

/// Client Font List PDU (we send no fonts; this just advances finalization).
pub fn font_list_pdu(share_id: u32, user_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(0, &mut body); // numberFonts
    put_u16(0, &mut body); // totalNumFonts
    put_u16(0x0003, &mut body); // listFlags = FONTLIST_FIRST | FONTLIST_LAST
    put_u16(50, &mut body); // entrySize
    share_data(share_id, user_id, PDUTYPE2_FONTLIST, &body)
}

/// If `share_pdu` is a Share Data PDU, return its `pduType2`. Used to spot the
/// server's Font Map (PDUTYPE2_FONTMAP), which marks the session Active.
pub fn data_pdu_type2(share_pdu: &[u8]) -> Option<u8> {
    ensure(share_pdu, SHARE_DATA_HEADER_LEN).ok()?;
    let pdu_type = u16::from_le_bytes([share_pdu[2], share_pdu[3]]) & 0x0f;
    if pdu_type != 7 {
        return None; // not a DATAPDU
    }
    Some(share_pdu[14]) // pduType2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_header(pdu: &[u8], expected_type2: u8) {
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]) as usize, pdu.len()); // totalLength
        assert_eq!(u16::from_le_bytes([pdu[2], pdu[3]]), PDUTYPE_DATA);
        assert_eq!(pdu[14], expected_type2); // pduType2
        assert_eq!(data_pdu_type2(pdu), Some(expected_type2));
    }

    #[test]
    fn synchronize_pdu_layout() {
        let pdu = synchronize_pdu(0x0001_03EA, 1007, 1002);
        check_header(&pdu, PDUTYPE2_SYNCHRONIZE);
        // body: messageType = 1, targetUser = 1002.
        assert_eq!(u16::from_le_bytes([pdu[18], pdu[19]]), 1);
        assert_eq!(u16::from_le_bytes([pdu[20], pdu[21]]), 1002);
    }

    #[test]
    fn control_pdu_cooperate_and_request() {
        let coop = control_pdu(1, 1007, CTRLACTION_COOPERATE);
        check_header(&coop, PDUTYPE2_CONTROL);
        assert_eq!(
            u16::from_le_bytes([coop[18], coop[19]]),
            CTRLACTION_COOPERATE
        );

        let req = control_pdu(1, 1007, CTRLACTION_REQUEST_CONTROL);
        assert_eq!(
            u16::from_le_bytes([req[18], req[19]]),
            CTRLACTION_REQUEST_CONTROL
        );
    }

    #[test]
    fn font_list_pdu_layout() {
        let pdu = font_list_pdu(1, 1007);
        check_header(&pdu, PDUTYPE2_FONTLIST);
        // body: numberFonts(18), totalNumFonts(20), listFlags(22), entrySize(24).
        assert_eq!(u16::from_le_bytes([pdu[22], pdu[23]]), 0x0003); // listFlags
        assert_eq!(u16::from_le_bytes([pdu[24], pdu[25]]), 50); // entrySize
    }

    #[test]
    fn detects_server_font_map() {
        // A synthetic server Font Map share data PDU.
        let pdu = share_data(1, 1002, PDUTYPE2_FONTMAP, &[0, 0, 0, 0]);
        assert_eq!(data_pdu_type2(&pdu), Some(PDUTYPE2_FONTMAP));
        // A non-data PDU yields None.
        assert_eq!(
            data_pdu_type2(&[0x00, 0x00, 0x11, 0x00, 0, 0, 0, 0, 0, 0]),
            None
        );
    }
}
