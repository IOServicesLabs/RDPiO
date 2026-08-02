//! RDP security headers and the Client Info PDU (`TS_INFO_PACKET`,
//! MS-RDPBCGR 2.2.1.11). The Client Info carries the logon credentials and is
//! sent right after MCS channel-join, inside an MCS Send Data Request.

/// Basic Security Header flag: the PDU carries the Security Exchange.
pub const SEC_EXCHANGE_PKT: u16 = 0x0001;
/// Basic Security Header flag: the PDU is a Server Initiate Multitransport
/// Request (MS-RDPBCGR 2.2.15.1).
pub const SEC_TRANSPORT_REQ: u16 = 0x0002;
/// Basic Security Header flag: the PDU is a Client Initiate Multitransport
/// Response (MS-RDPBCGR 2.2.15.2).
pub const SEC_TRANSPORT_RSP: u16 = 0x0004;
/// Basic Security Header flag: the PDU is a Client Info PDU.
pub const SEC_INFO_PKT: u16 = 0x0040;
/// Basic Security Header flag: the PDU payload is encrypted (Standard Security).
pub const SEC_ENCRYPT: u16 = 0x0008;
/// Basic Security Header flag: the PDU is a server Auto-Detect Request
/// (MS-RDPBCGR 2.2.14.3 / 2.2.8.1.1.2.1).
pub const SEC_AUTODETECT_REQ: u16 = 0x1000;
/// Basic Security Header flag: the PDU is a client Auto-Detect Response
/// (MS-RDPBCGR 2.2.14.2).
pub const SEC_AUTODETECT_RSP: u16 = 0x2000;

/// Build the Security Exchange PDU payload (MS-RDPBCGR 2.2.1.10.1): a basic
/// security header (`SEC_EXCHANGE_PKT`), the length of the encrypted client
/// random, then the encrypted client random itself (already RSA-encrypted and
/// padded with 8 trailing zero bytes). This is the payload of an MCS Send Data
/// Request on the I/O channel.
pub fn security_exchange(encrypted_client_random: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encrypted_client_random.len() + 8);
    put_u16(SEC_EXCHANGE_PKT, &mut out);
    put_u16(0, &mut out); // flagsHi
    put_u32(encrypted_client_random.len() as u32, &mut out);
    out.extend_from_slice(encrypted_client_random);
    out
}

// TS_INFO_PACKET.flags bits.
pub const INFO_MOUSE: u32 = 0x0000_0001;
pub const INFO_DISABLECTRLALTDEL: u32 = 0x0000_0002;
pub const INFO_AUTOLOGON: u32 = 0x0000_0008;
pub const INFO_UNICODE: u32 = 0x0000_0010;
pub const INFO_MAXIMIZESHELL: u32 = 0x0000_0020;
pub const INFO_LOGONNOTIFY: u32 = 0x0000_0040;
pub const INFO_ENABLEWINDOWSKEY: u32 = 0x0000_0100;

// TS_EXTENDED_INFO_PACKET.performanceFlags bits (MS-RDPBCGR 2.2.1.11.1.1.1):
// "disable" bits tell the server not to render an effect, so it never has to
// encode it — the win on a CPU-only host where every frame is software-encoded.
pub const PERF_DISABLE_WALLPAPER: u32 = 0x01;
pub const PERF_DISABLE_FULLWINDOWDRAG: u32 = 0x02;
pub const PERF_DISABLE_MENUANIMATIONS: u32 = 0x04;
pub const PERF_DISABLE_THEMING: u32 = 0x08;
pub const PERF_DISABLE_CURSOR_SHADOW: u32 = 0x20;
pub const PERF_DISABLE_CURSORSETTINGS: u32 = 0x40;
pub const PERF_ENABLE_FONT_SMOOTHING: u32 = 0x80;

/// Balanced experience flags, sent on every connect: drop the per-frame encode
/// hogs (wallpaper, menu animations) but keep theming, keep window contents
/// visible while dragging (H.264/EGFX encodes a moving window cheaply, and the
/// outline-only drag reads as broken next to mstsc), and turn on font smoothing
/// so text stays crisp. `0x85`.
pub const PERF_BALANCED: u32 =
    PERF_DISABLE_WALLPAPER | PERF_DISABLE_MENUANIMATIONS | PERF_ENABLE_FONT_SMOOTHING;

#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Logon information carried by the Client Info PDU.
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub domain: String,
    pub username: String,
    pub password: String,
    pub alternate_shell: String,
    pub working_dir: String,
    /// Opaque redirection load-balance cookie (e.g. AVD broker token) replayed
    /// after the extended info packet when non-empty.
    pub load_balance_info: Vec<u8>,
    /// Server redirection session id; written into the extended info packet's
    /// `clientSessionId` when non-zero.
    pub redirected_session_id: u32,
}

impl ClientInfo {
    /// TS_INFO_PACKET flags: Unicode + sensible client defaults, plus AUTOLOGON
    /// when a password is supplied.
    pub fn flags(&self) -> u32 {
        let mut flags = INFO_MOUSE
            | INFO_DISABLECTRLALTDEL
            | INFO_UNICODE
            | INFO_MAXIMIZESHELL
            | INFO_ENABLEWINDOWSKEY
            | INFO_LOGONNOTIFY;
        if !self.password.is_empty() {
            flags |= INFO_AUTOLOGON;
        }
        flags
    }

    /// Encode the `TS_INFO_PACKET` (no security header). Strings are UTF-16LE,
    /// each field NUL-terminated; the `cb*` lengths exclude the terminator.
    /// The optional `TS_EXTENDED_INFO_PACKET` is not emitted yet.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let domain = utf16(&self.domain);
        let user = utf16(&self.username);
        let password = utf16(&self.password);
        let shell = utf16(&self.alternate_shell);
        let dir = utf16(&self.working_dir);

        put_u32(0, out); // codePage
        put_u32(self.flags(), out);
        put_u16(domain.len() as u16, out);
        put_u16(user.len() as u16, out);
        put_u16(password.len() as u16, out);
        put_u16(shell.len() as u16, out);
        put_u16(dir.len() as u16, out);
        for field in [&domain, &user, &password, &shell, &dir] {
            out.extend_from_slice(field);
            put_u16(0, out); // UTF-16 NUL terminator
        }
    }
}

/// Build the Client Info share payload: a basic security header
/// (`SEC_INFO_PKT`), the `TS_INFO_PACKET`, then a `TS_EXTENDED_INFO_PACKET`
/// carrying the balanced [`PERF_BALANCED`] performance flags (and no
/// auto-reconnect cookie). This is the payload of an MCS Send Data Request to
/// the I/O channel. mstsc always sends the extended info; doing the same lets a
/// CPU-only host skip encoding wallpaper/drag/menu effects every frame.
pub fn client_info_payload(info: &ClientInfo) -> Vec<u8> {
    let mut out = Vec::new();
    put_u16(SEC_INFO_PKT, &mut out); // flags
    put_u16(0, &mut out); // flagsHi
    info.encode(&mut out);
    out.extend_from_slice(&extended_info_packet(
        PERF_BALANCED,
        None,
        info.redirected_session_id,
    ));
    out
}

/// An `ARC_CS_PRIVATE_PACKET` — the client auto-reconnect cookie (28 bytes).
/// `security_verifier` is `HMAC-MD5(AutoReconnectRandom, ClientRandom)` (the
/// caller computes it, since the crypto lives in another crate).
pub fn auto_reconnect_cookie(logon_id: u32, security_verifier: &[u8; 16]) -> [u8; 28] {
    let mut c = [0u8; 28];
    c[0..4].copy_from_slice(&28u32.to_le_bytes()); // cbLen
    c[4..8].copy_from_slice(&1u32.to_le_bytes()); // Version = AUTO_RECONNECT_VERSION_1
    c[8..12].copy_from_slice(&logon_id.to_le_bytes());
    c[12..28].copy_from_slice(security_verifier);
    c
}

/// A `TS_EXTENDED_INFO_PACKET` (the part that follows the `TS_INFO_PACKET`)

/// carrying `performance_flags` and, optionally, the auto-reconnect `cookie`.
/// With `cookie = None`, `cbAutoReconnectCookie` is 0 and no cookie follows. On
/// the encrypted legacy path the caller appends this to the `TS_INFO_PACKET`
/// before RC4-wrapping.
pub fn extended_info_packet(
    performance_flags: u32,
    cookie: Option<&[u8; 28]>,
    redirected_session_id: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_u16(0x0002, &mut out); // clientAddressFamily = AF_INET
    put_u16(2, &mut out); // cbClientAddress (a single null wchar)
    put_u16(0, &mut out); // clientAddress = L""
    put_u16(2, &mut out); // cbClientDir
    put_u16(0, &mut out); // clientDir = L""
    out.extend_from_slice(&[0u8; 172]); // clientTimeZone (TIME_ZONE_INFORMATION, zeroed)
    put_u32(redirected_session_id, &mut out); // clientSessionId
    put_u32(performance_flags, &mut out); // performanceFlags
    match cookie {
        Some(c) => {
            put_u16(c.len() as u16, &mut out); // cbAutoReconnectCookie
            out.extend_from_slice(c); // autoReconnectCookie (ARC_CS_PRIVATE_PACKET)
        }
        None => put_u16(0, &mut out), // cbAutoReconnectCookie = 0
    }
    out
}

/// Like [`client_info_payload`] but the extended info also carries the
/// auto-reconnect `cookie`, used when reconnecting on the plaintext/TLS path.
/// The balanced performance flags ride along the same as the initial connect.
pub fn client_info_payload_reconnect(info: &ClientInfo, cookie: &[u8; 28]) -> Vec<u8> {
    let mut out = Vec::new();
    put_u16(SEC_INFO_PKT, &mut out); // flags
    put_u16(0, &mut out); // flagsHi
    info.encode(&mut out);
    out.extend_from_slice(&extended_info_packet(
        PERF_BALANCED,
        Some(cookie),
        info.redirected_session_id,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_cookie_and_extended_info() {
        let cookie = auto_reconnect_cookie(0xABCD, &[9u8; 16]);
        assert_eq!(u32::from_le_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]), 28);
        assert_eq!(u32::from_le_bytes([cookie[4], cookie[5], cookie[6], cookie[7]]), 1);
        assert_eq!(u32::from_le_bytes([cookie[8], cookie[9], cookie[10], cookie[11]]), 0xABCD);
        assert_eq!(&cookie[12..28], &[9u8; 16]);

        let info = ClientInfo {
            username: "bob".into(),
            ..Default::default()
        };
        let plain = client_info_payload(&info);
        let recon = client_info_payload_reconnect(&info, &cookie);
        // The reconnect payload is the plain one plus the 28-byte cookie (both
        // now append an extended info; only its cbCookie/cookie tail differs).
        assert_eq!(recon.len(), plain.len() + 28);
        assert_eq!(&recon[recon.len() - 28..], &cookie[..]);
        // cbAutoReconnectCookie precedes it.
        let cb = u16::from_le_bytes([recon[recon.len() - 30], recon[recon.len() - 29]]);
        assert_eq!(cb, 28);
    }

    #[test]
    fn client_info_appends_balanced_perf_flags() {
        let payload = client_info_payload(&ClientInfo::default());
        // The extended info ends with performanceFlags(4) + cbAutoReconnectCookie(2).
        let n = payload.len();
        let perf = u32::from_le_bytes([payload[n - 6], payload[n - 5], payload[n - 4], payload[n - 3]]);
        assert_eq!(perf, PERF_BALANCED);
        // wallpaper|menuanim|font_smoothing — full-window-drag stays ENABLED so
        // dragging shows window contents (not just an outline), like mstsc.
        assert_eq!(perf, 0x85);
        // No auto-reconnect cookie on the initial connect.
        let cb = u16::from_le_bytes([payload[n - 2], payload[n - 1]]);
        assert_eq!(cb, 0);
    }

    #[test]
    fn info_packet_lengths_and_unicode() {
        let info = ClientInfo {
            username: "bob".into(),
            ..Default::default()
        };
        let mut out = Vec::new();
        info.encode(&mut out);

        // codePage = 0.
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
        // flags include UNICODE, exclude AUTOLOGON (no password).
        let flags = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!(flags & INFO_UNICODE, INFO_UNICODE);
        assert_eq!(flags & INFO_AUTOLOGON, 0);
        // cbDomain = 0, cbUserName = 6 (3 chars * 2 bytes, no terminator).
        assert_eq!(u16::from_le_bytes([out[8], out[9]]), 0);
        assert_eq!(u16::from_le_bytes([out[10], out[11]]), 6);
        // After the five cb fields (offset 18): domain NUL, then "bob\0".
        assert_eq!(&out[18..20], &[0, 0]); // empty domain → just NUL
        assert_eq!(&out[20..28], &[0x62, 0, 0x6f, 0, 0x62, 0, 0, 0]); // "bob\0"
    }

    #[test]
    fn autologon_set_when_password_present() {
        let info = ClientInfo {
            username: "bob".into(),
            password: "secret".into(),
            ..Default::default()
        };
        assert_eq!(info.flags() & INFO_AUTOLOGON, INFO_AUTOLOGON);
    }

    #[test]
    fn payload_starts_with_info_security_header() {
        let payload = client_info_payload(&ClientInfo::default());
        assert_eq!(&payload[0..4], &[0x40, 0x00, 0x00, 0x00]); // SEC_INFO_PKT, flagsHi 0
    }

    #[test]
    fn security_exchange_layout() {
        let pdu = security_exchange(&[0xAA; 72]);
        // flags=SEC_EXCHANGE_PKT, flagsHi=0, length=72, then the blob.
        assert_eq!(&pdu[0..4], &[0x01, 0x00, 0x00, 0x00]);
        assert_eq!(u32::from_le_bytes([pdu[4], pdu[5], pdu[6], pdu[7]]), 72);
        assert_eq!(&pdu[8..], &[0xAA; 72]);
    }

    #[test]
    fn does_not_set_rail_or_wheel_flags_for_gateway() {
        // 0x8000 is INFO_RAIL (RemoteApp) and 0x20000 is INFO_MOUSE_HAS_WHEEL. A
        // full-desktop gateway/redirection logon must NOT set either, even with
        // load balance info / a redirected session id present (an earlier bug did,
        // which put the server into RemoteApp mode).
        let info = ClientInfo {
            load_balance_info: b"Cookie: msts=12345\r\n".to_vec(),
            redirected_session_id: 0xBEEF,
            ..Default::default()
        };
        let flags = info.flags();
        assert_eq!(flags & 0x0000_8000, 0); // not INFO_RAIL
        assert_eq!(flags & 0x0002_0000, 0); // not INFO_MOUSE_HAS_WHEEL
    }

    #[test]
    fn client_info_does_not_append_load_balance_info() {
        // The load balance / routing token belongs in the X.224 request, NOT the
        // Client Info PDU. Appending it corrupts the extended-info tail — the
        // server misreads the bytes as cbDynamicDSTTimeZoneKeyName and drops the
        // connection with ERRINFO_TIMEZONEKEYNAMELENGTHTOOLONG (0x1131).
        let info = ClientInfo {
            load_balance_info: b"Cookie: msts=12345\r\n".to_vec(),
            ..Default::default()
        };
        let with = client_info_payload(&info);
        let without = client_info_payload(&ClientInfo::default());
        assert_eq!(with.len(), without.len()); // nothing appended
        assert_eq!(&with[with.len() - 2..], &[0, 0]); // ends at cbAutoReconnectCookie=0
    }

    #[test]
    fn extended_info_carries_redirected_session_id() {
        let out = extended_info_packet(PERF_BALANCED, None, 0x1234_5678);
        // clientSessionId sits at offset 182 (2+2+2+2+2+172).
        assert_eq!(
            u32::from_le_bytes([out[182], out[183], out[184], out[185]]),
            0x1234_5678
        );
    }
}
