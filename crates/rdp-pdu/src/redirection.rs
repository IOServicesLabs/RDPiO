//! Server Redirection PDU parsing (MS-RDPBCGR 2.2.13).
//!
//! AVD / RDS broker redirection sends this PDU during activation instead of a
//! Demand Active. It carries the target host, session id, and an opaque
//! load-balance cookie the client must replay in the next connection attempt.
//!
//! Layout reference: FreeRDP `libfreerdp/core/redirection.c`.

/// Basic Security Header flag identifying a Server Redirection Packet.
pub const SEC_REDIRECTION_PKT: u16 = 0x0400;

pub const REDIRECT_FLAG_TARGET_NET_ADDRESS: u32 = 0x0000_0001;
pub const REDIRECT_FLAG_LOAD_BALANCE_INFO: u32 = 0x0000_0002;
pub const REDIRECT_FLAG_USERNAME: u32 = 0x0000_0004;
pub const REDIRECT_FLAG_DOMAIN: u32 = 0x0000_0008;
pub const REDIRECT_FLAG_PASSWORD: u32 = 0x0000_0010;
pub const REDIRECT_FLAG_TARGET_FQDN: u32 = 0x0000_0020;
pub const REDIRECT_FLAG_TARGET_NETBIOS_NAME: u32 = 0x0000_0040;
pub const REDIRECT_FLAG_TARGET_NET_ADDRESSES: u32 = 0x0000_0080;
pub const REDIRECT_FLAG_CLIENT_TSV_URL: u32 = 0x0000_0100;
pub const REDIRECT_FLAG_SERVER_TSV_CAPABLE: u32 = 0x0000_0200;
pub const REDIRECT_FLAG_TARGET_CERTIFICATE: u32 = 0x0000_0400;
pub const REDIRECT_FLAG_REDIRECTION_GUID: u32 = 0x0000_0800;

/// Server Redirection target information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerRedirection {
    pub session_id: u32,
    pub redir_flags: u32,
    pub target_net_address: Option<String>,
    pub load_balance_info: Vec<u8>,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Vec<u8>,
    pub target_fqdn: Option<String>,
    pub target_netbios_name: Option<String>,
    pub target_net_addresses: Vec<String>,
    pub client_tsv_url: Vec<u8>,
    pub redirection_guid: Vec<u8>,
    pub target_certificate: Vec<u8>,
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

/// Read a `UINT32` length-prefixed byte blob and advance the cursor.
fn read_blob(b: &mut &[u8]) -> Option<Vec<u8>> {
    let len = u32le(b, 0)? as usize;
    if b.len() < 4 + len {
        return None;
    }
    let out = b[4..4 + len].to_vec();
    *b = &b[4 + len..];
    Some(out)
}

/// Read a `UINT32` length-prefixed UTF-16LE string and advance the cursor.
fn read_unicode(b: &mut &[u8], limit: usize) -> Option<String> {
    let len = u32le(b, 0)? as usize;
    if len > limit || b.len() < 4 + len {
        return None;
    }
    let data = &b[4..4 + len];
    *b = &b[4 + len..];
    // Decode up to the first NUL wchar.
    let mut chars = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        let c = u16::from_le_bytes([chunk[0], chunk[1]]);
        if c == 0 {
            break;
        }
        chars.push(c);
    }
    Some(String::from_utf16_lossy(&chars))
}

/// Parse a Server Redirection PDU from the plaintext payload of an MCS Send
/// Data Indication. Returns `None` if the bytes are not a redirection PDU.
pub fn parse(plaintext: &[u8]) -> Option<ServerRedirection> {
    // Minimum: Share Control Header (4) + pad2Octets (2) + flags(2) + length(2) +
    // sessionID(4) + redirFlags(4).
    if plaintext.len() < 18 {
        return None;
    }

    // Share Control Header: totalLength (2), pduType (2). The low nibble of
    // pduType is PDUTYPE_SERVER_REDIR_PKT (0xA).
    let pdu_type = u16le(plaintext, 2)?;
    if (pdu_type & 0x000F) != 0x000A {
        return None;
    }

    // Skip the Share Control Header and the 2-octet pad that precedes the
    // redirection packet proper.
    let mut b = plaintext.get(6..)?;

    let flags = u16le(b, 0)?;
    if flags != SEC_REDIRECTION_PKT {
        return None;
    }
    let _length = u16le(b, 2)?; // total redirection packet length
    let session_id = u32le(b, 4)?;
    let redir_flags = u32le(b, 8)?;
    b = &b[12..];

    let mut out = ServerRedirection {
        session_id,
        redir_flags,
        ..Default::default()
    };

    if redir_flags & REDIRECT_FLAG_TARGET_NET_ADDRESS != 0 {
        out.target_net_address = read_unicode(&mut b, 80);
    }
    if redir_flags & REDIRECT_FLAG_LOAD_BALANCE_INFO != 0 {
        out.load_balance_info = read_blob(&mut b)?;
    }
    if redir_flags & REDIRECT_FLAG_USERNAME != 0 {
        out.username = read_unicode(&mut b, 512);
    }
    if redir_flags & REDIRECT_FLAG_DOMAIN != 0 {
        out.domain = read_unicode(&mut b, 52);
    }
    if redir_flags & REDIRECT_FLAG_PASSWORD != 0 {
        out.password = read_blob(&mut b)?;
    }
    if redir_flags & REDIRECT_FLAG_TARGET_FQDN != 0 {
        out.target_fqdn = read_unicode(&mut b, 512);
    }
    if redir_flags & REDIRECT_FLAG_TARGET_NETBIOS_NAME != 0 {
        out.target_netbios_name = read_unicode(&mut b, 32);
    }
    if redir_flags & REDIRECT_FLAG_TARGET_NET_ADDRESSES != 0 {
        let field_len = u32le(b, 0)? as usize;
        let count = u32le(b, 4)? as usize;
        b = &b[8..];
        // Sanity: each address is at least one WCHAR plus a length prefix.
        let mut addrs = Vec::with_capacity(count);
        for _ in 0..count {
            addrs.push(read_unicode(&mut b, 80)?);
        }
        out.target_net_addresses = addrs;
        // Some Windows builds append an optional 8-byte pad; ignore trailing bytes.
        let _ = field_len;
    }
    if redir_flags & REDIRECT_FLAG_CLIENT_TSV_URL != 0 {
        out.client_tsv_url = read_blob(&mut b)?;
    }
    if redir_flags & REDIRECT_FLAG_REDIRECTION_GUID != 0 {
        out.redirection_guid = read_blob(&mut b)?;
    }
    if redir_flags & REDIRECT_FLAG_TARGET_CERTIFICATE != 0 {
        out.target_certificate = read_blob(&mut b)?;
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal redirection PDU: Share Control Header + pad2 + packet.
    fn pdu(redir_flags: u32, fields: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&SEC_REDIRECTION_PKT.to_le_bytes());
        packet.extend_from_slice(&((12 + fields.len()) as u16).to_le_bytes());
        packet.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        packet.extend_from_slice(&redir_flags.to_le_bytes());
        packet.extend_from_slice(fields);

        let total = (6 + packet.len()) as u16;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&0x000Au16.to_le_bytes()); // PDUTYPE_SERVER_REDIR_PKT
        out.extend_from_slice(&[0u8; 2]); // pad2Octets
        out.extend_from_slice(&packet);
        out
    }

    fn unicode(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for c in s.encode_utf16() {
            v.extend_from_slice(&c.to_le_bytes());
        }
        v.extend_from_slice(&[0u8; 2]); // NUL terminator
        v
    }

    #[test]
    fn parses_target_address_and_session_id() {
        let mut fields = Vec::new();
        let addr = unicode("192.0.2.42");
        fields.extend_from_slice(&(addr.len() as u32).to_le_bytes());
        fields.extend_from_slice(&addr);

        let r = parse(&pdu(REDIRECT_FLAG_TARGET_NET_ADDRESS, &fields)).unwrap();
        assert_eq!(r.session_id, 0xDEAD_BEEF);
        assert_eq!(r.target_net_address.as_deref(), Some("192.0.2.42"));
    }

    #[test]
    fn parses_load_balance_cookie() {
        let cookie = b"Cookie: msts=12345\r\n";
        let mut fields = Vec::new();
        fields.extend_from_slice(&(cookie.len() as u32).to_le_bytes());
        fields.extend_from_slice(cookie);

        let r = parse(&pdu(REDIRECT_FLAG_LOAD_BALANCE_INFO, &fields)).unwrap();
        assert_eq!(r.load_balance_info, cookie);
    }

    #[test]
    fn parses_net_addresses() {
        let mut fields = Vec::new();
        let a1 = unicode("10.0.0.1");
        let a2 = unicode("10.0.0.2");
        let field_len: u32 = 8 + a1.len() as u32 + a2.len() as u32;
        fields.extend_from_slice(&field_len.to_le_bytes());
        fields.extend_from_slice(&2u32.to_le_bytes());
        fields.extend_from_slice(&(a1.len() as u32).to_le_bytes());
        fields.extend_from_slice(&a1);
        fields.extend_from_slice(&(a2.len() as u32).to_le_bytes());
        fields.extend_from_slice(&a2);

        let r = parse(&pdu(REDIRECT_FLAG_TARGET_NET_ADDRESSES, &fields)).unwrap();
        assert_eq!(r.target_net_addresses, vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]);
    }

    #[test]
    fn rejects_non_redirection_pdu() {
        // A normal Share Data Header: totalLength, pduType=0x17 (data PDU).
        let share = [0x1Cu8, 0x00, 0x17, 0x00, 0xEA, 0x03];
        assert!(parse(&share).is_none());
    }
}
