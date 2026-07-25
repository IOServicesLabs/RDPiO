//! Server Save Session Info PDU (MS-RDPBCGR 2.2.10.1): the server reports a
//! successful logon — the domain\username it logged the client on as, and the
//! session id. Surfacing it confirms (and logs) that authentication succeeded,
//! the complement to the Set Error Info PDU that reports failures.
//!
//! Sans-I/O: classify the PDU and pull out the logon fields. The auto-reconnect
//! cookie that the extended variant can also carry is not extracted here (the
//! client does not yet perform credential-less reconnect).

use crate::finalization::{data_pdu_type2, PDUTYPE2_SAVE_SESSION_INFO};

const INFOTYPE_LOGON: u32 = 0x0000_0000;
const INFOTYPE_LOGON_LONG: u32 = 0x0000_0001;
const INFOTYPE_LOGON_PLAINNOTIFY: u32 = 0x0000_0002;
const INFOTYPE_LOGON_EXTENDED_INFO: u32 = 0x0000_0003;

/// The logon identity the server reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogonInfo {
    pub domain: String,
    pub username: String,
    pub session_id: u32,
}

/// The server's auto-reconnect cookie (ARC_SC_PRIVATE_PACKET): lets the client
/// reconnect after a transient drop without re-entering credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectCookie {
    pub logon_id: u32,
    pub arc_random: [u8; 16],
}

const LOGON_EX_AUTORECONNECTCOOKIE: u32 = 0x0000_0001;

/// A classified Save Session Info PDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveSessionInfo {
    /// A logon notification (version 1 or 2) carrying the identity + session id.
    Logon(LogonInfo),
    /// A "plain" logon notification with no extra detail.
    PlainNotify,
    /// Extended info; carries the auto-reconnect cookie when the server offered
    /// one (`LOGON_EX_AUTORECONNECTCOOKIE`).
    Extended { cookie: Option<ReconnectCookie> },
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

/// Decode `cb` bytes of UTF-16LE at `off` into a `String`, stopping at a NUL.
fn utf16(b: &[u8], off: usize, cb: usize) -> String {
    let slice = match b.get(off..off + cb) {
        Some(s) => s,
        None => return String::new(),
    };
    let units: Vec<u16> = slice
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Classify a Save Session Info PDU from a Share Data PDU's plaintext. Returns
/// `None` if it isn't one. Logon fields follow the 18-byte Share Data Header.
pub fn parse_save_session_info(plaintext: &[u8]) -> Option<SaveSessionInfo> {
    if data_pdu_type2(plaintext) != Some(PDUTYPE2_SAVE_SESSION_INFO) {
        return None;
    }
    let body = plaintext.get(18..)?;
    match u32le(body, 0)? {
        INFOTYPE_LOGON => {
            // TS_LOGON_INFO (v1): cbDomain, Domain[52], cbUserName, UserName[512], SessionId.
            let cb_domain = (u32le(body, 4)? as usize).min(52);
            let cb_user = (u32le(body, 4 + 4 + 52)? as usize).min(512);
            let domain = utf16(body, 4 + 4, cb_domain);
            let username = utf16(body, 4 + 4 + 52 + 4, cb_user);
            let session_id = u32le(body, 4 + 4 + 52 + 4 + 512)?;
            Some(SaveSessionInfo::Logon(LogonInfo {
                domain,
                username,
                session_id,
            }))
        }
        INFOTYPE_LOGON_LONG => {
            // TS_LOGON_INFO_VERSION_2: Version[2], Size[4], SessionId[4],
            // cbDomain[4], cbUserName[4], Pad[558], Domain, UserName.
            let session_id = u32le(body, 4 + 2 + 4)?;
            let cb_domain = u32le(body, 4 + 2 + 4 + 4)? as usize;
            let cb_user = u32le(body, 4 + 2 + 4 + 4 + 4)? as usize;
            let names_off = 4 + 2 + 4 + 4 + 4 + 4 + 558;
            let domain = utf16(body, names_off, cb_domain);
            let username = utf16(body, names_off + cb_domain, cb_user);
            Some(SaveSessionInfo::Logon(LogonInfo {
                domain,
                username,
                session_id,
            }))
        }
        INFOTYPE_LOGON_PLAINNOTIFY => Some(SaveSessionInfo::PlainNotify),
        INFOTYPE_LOGON_EXTENDED_INFO => {
            // TS_LOGON_INFO_EXTENDED: Length(2), FieldsPresent(4), then the
            // present fields. AutoReconnectCookie = cbFieldData(4) +
            // ARC_SC_PRIVATE_PACKET { cbLen(4), Version(4), LogonId(4), Rand[16] }.
            let fields = u32le(body, 6)?;
            let cookie = if fields & LOGON_EX_AUTORECONNECTCOOKIE != 0 {
                let logon_id = u32le(body, 22)?;
                let arc = body.get(26..42)?;
                let mut arc_random = [0u8; 16];
                arc_random.copy_from_slice(arc);
                Some(ReconnectCookie {
                    logon_id,
                    arc_random,
                })
            } else {
                None
            };
            Some(SaveSessionInfo::Extended { cookie })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finalization::share_data;

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn parses_logon_info_v1() {
        let mut body = vec![];
        body.extend_from_slice(&INFOTYPE_LOGON.to_le_bytes());
        let domain = utf16le("CORP\0");
        body.extend_from_slice(&((domain.len() - 2) as u32).to_le_bytes()); // cbDomain (excl NUL)
        let mut dom_field = domain.clone();
        dom_field.resize(52, 0);
        body.extend_from_slice(&dom_field);
        let user = utf16le("alice\0");
        body.extend_from_slice(&((user.len() - 2) as u32).to_le_bytes()); // cbUserName
        let mut user_field = user.clone();
        user_field.resize(512, 0);
        body.extend_from_slice(&user_field);
        body.extend_from_slice(&42u32.to_le_bytes()); // SessionId

        let pdu = share_data(1, 1002, PDUTYPE2_SAVE_SESSION_INFO, &body);
        assert_eq!(
            parse_save_session_info(&pdu),
            Some(SaveSessionInfo::Logon(LogonInfo {
                domain: "CORP".into(),
                username: "alice".into(),
                session_id: 42,
            }))
        );
    }

    #[test]
    fn parses_logon_info_v2() {
        let mut body = vec![];
        body.extend_from_slice(&INFOTYPE_LOGON_LONG.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // Version
        body.extend_from_slice(&0x240u32.to_le_bytes()); // Size
        body.extend_from_slice(&7u32.to_le_bytes()); // SessionId
        let domain = utf16le("D");
        let user = utf16le("bob");
        body.extend_from_slice(&(domain.len() as u32).to_le_bytes()); // cbDomain
        body.extend_from_slice(&(user.len() as u32).to_le_bytes()); // cbUserName
        body.extend_from_slice(&[0u8; 558]); // Pad
        body.extend_from_slice(&domain);
        body.extend_from_slice(&user);

        let pdu = share_data(1, 1002, PDUTYPE2_SAVE_SESSION_INFO, &body);
        assert_eq!(
            parse_save_session_info(&pdu),
            Some(SaveSessionInfo::Logon(LogonInfo {
                domain: "D".into(),
                username: "bob".into(),
                session_id: 7,
            }))
        );
    }

    #[test]
    fn classifies_notify_and_extended_and_rejects_others() {
        let notify = share_data(1, 1002, PDUTYPE2_SAVE_SESSION_INFO, &2u32.to_le_bytes());
        assert_eq!(
            parse_save_session_info(&notify),
            Some(SaveSessionInfo::PlainNotify)
        );
        // Extended with no fields present → no cookie.
        let mut ext_body = 3u32.to_le_bytes().to_vec();
        ext_body.extend_from_slice(&8u16.to_le_bytes()); // Length
        ext_body.extend_from_slice(&0u32.to_le_bytes()); // FieldsPresent = none
        let ext = share_data(1, 1002, PDUTYPE2_SAVE_SESSION_INFO, &ext_body);
        assert_eq!(
            parse_save_session_info(&ext),
            Some(SaveSessionInfo::Extended { cookie: None })
        );
        // A Font Map PDU is not Save Session Info.
        let fm = share_data(1, 1002, crate::finalization::PDUTYPE2_FONTMAP, &[0; 4]);
        assert_eq!(parse_save_session_info(&fm), None);
    }

    #[test]
    fn parses_auto_reconnect_cookie() {
        let mut body = 3u32.to_le_bytes().to_vec(); // INFOTYPE_LOGON_EXTENDED_INFO
        body.extend_from_slice(&50u16.to_le_bytes()); // Length
        body.extend_from_slice(&LOGON_EX_AUTORECONNECTCOOKIE.to_le_bytes());
        body.extend_from_slice(&28u32.to_le_bytes()); // cbFieldData
        body.extend_from_slice(&28u32.to_le_bytes()); // ARC cbLen
        body.extend_from_slice(&1u32.to_le_bytes()); // Version
        body.extend_from_slice(&0xABCDu32.to_le_bytes()); // LogonId
        body.extend_from_slice(&[7u8; 16]); // ArcRandomBits
        let pdu = share_data(1, 1002, PDUTYPE2_SAVE_SESSION_INFO, &body);
        assert_eq!(
            parse_save_session_info(&pdu),
            Some(SaveSessionInfo::Extended {
                cookie: Some(ReconnectCookie {
                    logon_id: 0xABCD,
                    arc_random: [7u8; 16],
                }),
            })
        );
    }
}
