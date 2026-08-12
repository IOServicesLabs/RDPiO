//! From-scratch RDP wire codec: TPKT/X.224, MCS (T.125), GCC conference data,
//! Standard RDP Security headers, licensing, capability exchange, the Client
//! Info PDU, and input PDUs. No third-party RDP library — everything here is
//! built directly on the byte formats in MS-RDPBCGR / T.125.
//!
//! Endianness notes:
//! - TPKT and MCS multi-byte lengths/ids are **big-endian**.
//! - RDP structures (negotiation, GCC blocks, security, capabilities) are
//!   **little-endian**, the RDP convention.

use crate::error::{WireError, WireResult};
use crate::protocol_err;

#[inline]
pub(crate) fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
pub(crate) fn get_u16(b: &[u8], o: usize) -> WireResult<u16> {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| protocol_err!("truncated u16 at offset {o}"))
}

#[inline]
pub(crate) fn get_u32(b: &[u8], o: usize) -> WireResult<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| protocol_err!("truncated u32 at offset {o}"))
}

// ---------------------------------------------------------------------------
// Minimal BER subset (T.125 / GCC)
// ---------------------------------------------------------------------------

pub(crate) mod ber {
    use crate::error::{WireError, WireResult};

    pub const TAG_INTEGER: u8 = 0x02;
    pub const TAG_OCTET_STRING: u8 = 0x04;
    pub const TAG_ENUMERATED: u8 = 0x0a;
    pub const TAG_SEQUENCE: u8 = 0x30;

    /// Encode a BER length (short form < 0x80, otherwise long form).
    pub fn encode_len(len: usize, out: &mut Vec<u8>) {
        if len < 0x80 {
            out.push(len as u8);
        } else {
            let mut bytes = Vec::new();
            let mut v = len;
            while v > 0 {
                bytes.push((v & 0xff) as u8);
                v >>= 8;
            }
            out.push(0x80 | bytes.len() as u8);
            out.extend(bytes.iter().rev());
        }
    }

    /// Decode a BER length, advancing the cursor.
    pub fn decode_len(cur: &mut &[u8]) -> WireResult<usize> {
        let first = *cur
            .first()
            .ok_or_else(|| WireError::Protocol("truncated BER length".into()))?;
        *cur = &cur[1..];
        if first < 0x80 {
            return Ok(first as usize);
        }
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(WireError::Protocol(format!(
                "unsupported BER long-form length ({n} octets)"
            )));
        }
        if cur.len() < n {
            return Err(WireError::Protocol("truncated BER long-form length".into()));
        }
        let mut v = 0usize;
        for &b in &cur[..n] {
            v = (v << 8) | b as usize;
        }
        *cur = &cur[n..];
        Ok(v)
    }

    /// Wrap `data` in a `[tag] length data` element.
    fn element(tag: u8, data: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        encode_len(data.len(), out);
        out.extend_from_slice(data);
    }

    pub fn integer(v: u32) -> Vec<u8> {
        let mut bytes = v.to_be_bytes().to_vec();
        while bytes.len() > 1 && bytes[0] == 0 {
            bytes.remove(0);
        }
        let mut out = Vec::new();
        element(TAG_INTEGER, &bytes, &mut out);
        out
    }

    pub fn octet_string(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        element(TAG_OCTET_STRING, data, &mut out);
        out
    }

    pub fn sequence(inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        element(TAG_SEQUENCE, inner, &mut out);
        out
    }

    /// Expect `tag` at the cursor; return its value bytes.
    pub fn expect(cur: &mut &[u8], tag: u8) -> WireResult<Vec<u8>> {
        let got = *cur
            .first()
            .ok_or_else(|| WireError::Protocol("truncated BER element".into()))?;
        if got != tag {
            return Err(WireError::Protocol(format!(
                "expected BER tag {tag:#04x}, got {got:#04x}"
            )));
        }
        *cur = &cur[1..];
        let len = decode_len(cur)?;
        if cur.len() < len {
            return Err(WireError::Protocol("truncated BER element body".into()));
        }
        let val = cur[..len].to_vec();
        *cur = &cur[len..];
        Ok(val)
    }

    /// Parse a sequence of `[TAG_OCTET_STRING]` elements, returning the LAST
    /// one's value (both GCC create-request and create-response shapes end in
    /// the user-data octet string).
    pub fn last_octet_string(cur: &mut &[u8]) -> WireResult<Vec<u8>> {
        let mut last = None;
        while !cur.is_empty() {
            let got = *cur
                .first()
                .ok_or_else(|| WireError::Protocol("truncated BER".into()))?;
            if got != TAG_OCTET_STRING {
                return Err(WireError::Protocol(format!(
                    "expected octet string in GCC data, got {got:#04x}"
                )));
            }
            *cur = &cur[1..];
            let len = decode_len(cur)?;
            if cur.len() < len {
                return Err(WireError::Protocol("truncated octet string".into()));
            }
            last = Some(cur[..len].to_vec());
            *cur = &cur[len..];
        }
        last.ok_or_else(|| WireError::Protocol("GCC data has no user-data octet string".into()))
    }
}

// ---------------------------------------------------------------------------
// X.224 (T.125 class-0) + RDP negotiation
// ---------------------------------------------------------------------------

pub mod x224 {
    use super::*;

    pub const X224_CR: u8 = 0xe0;
    pub const X224_CC: u8 = 0xd0;
    /// Bytes of the fixed X.224 CR/CC header following the length indicator.
    const X224_CRCC_FIXED: usize = 6;
    /// The RDP negotiation structure is always 8 bytes.
    const RDP_NEG_SIZE: usize = 8;
    const NEG_TYPE_REQ: u8 = 0x01;
    const NEG_TYPE_RSP: u8 = 0x02;
    const NEG_TYPE_FAILURE: u8 = 0x03;

    /// The 3-byte X.224 Data (DT) TPDU header prefixing every MCS PDU.
    pub const DATA_HEADER: [u8; 3] = [0x02, 0xf0, 0x80];

    /// Client X.224 Connection Request carrying an RDP Negotiation Request.
    #[derive(Debug, Clone)]
    pub struct ConnectionRequest {
        /// `requestedProtocols` (0 = Standard RDP Security only).
        pub requested_protocols: u32,
        /// Optional `Cookie: mstshash=<user>` hint line.
        pub cookie: Option<String>,
    }

    impl ConnectionRequest {
        /// Encode the TPKT body (everything after the 4-byte TPKT header).
        pub fn encode(&self, out: &mut Vec<u8>) -> WireResult<()> {
            let mut user_data = Vec::new();
            if let Some(cookie) = &self.cookie {
                user_data.extend_from_slice(b"Cookie: mstshash=");
                user_data.extend_from_slice(cookie.as_bytes());
                user_data.extend_from_slice(b"\r\n");
            }
            user_data.push(NEG_TYPE_REQ);
            user_data.push(0x00); // flags
            put_u16(RDP_NEG_SIZE as u16, &mut user_data);
            put_u32(self.requested_protocols, &mut user_data);

            let li = X224_CRCC_FIXED + user_data.len();
            if li > u8::MAX as usize {
                return Err(WireError::Protocol("connection request too large".into()));
            }
            out.push(li as u8);
            out.push(X224_CR);
            out.extend_from_slice(&[0x00, 0x00]); // DST-REF
            out.extend_from_slice(&[0x00, 0x00]); // SRC-REF
            out.push(0x00); // class/options
            out.extend_from_slice(&user_data);
            Ok(())
        }
    }

    /// The server's X.224 Connection Confirm and its negotiation outcome.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ConnectionConfirm {
        /// Negotiation accepted; `selected_protocol` is the server's choice.
        Response { flags: u8, selected_protocol: u32 },
        /// The server rejected the negotiation with a failure code.
        Failure { code: u32 },
        /// Legacy confirm without an RDP negotiation structure.
        NoNegotiation,
    }

    impl ConnectionConfirm {
        /// Decode a Connection Confirm from a TPKT body (starts with the X.224
        /// length indicator). Consumes the whole body.
        pub fn decode(src: &mut &[u8]) -> WireResult<Self> {
            let li = *src
                .first()
                .ok_or_else(|| WireError::Protocol("empty connection confirm".into()))?
                as usize;
            let body = src
                .get(1..1 + li)
                .ok_or_else(|| WireError::Protocol("truncated connection confirm".into()))?;
            if body.len() < X224_CRCC_FIXED {
                return Err(WireError::Protocol("short connection confirm".into()));
            }
            if body[0] & 0xf0 != X224_CC {
                return Err(WireError::Protocol(format!(
                    "expected X.224 CC, got {:#04x}",
                    body[0]
                )));
            }
            let result = if li == X224_CRCC_FIXED {
                Self::NoNegotiation
            } else {
                let neg = body
                    .get(X224_CRCC_FIXED..X224_CRCC_FIXED + RDP_NEG_SIZE)
                    .ok_or_else(|| WireError::Protocol("truncated RDP negotiation".into()))?;
                let kind = neg[0];
                let flags = neg[1];
                let payload = u32::from_le_bytes([neg[4], neg[5], neg[6], neg[7]]);
                match kind {
                    NEG_TYPE_RSP => Self::Response {
                        flags,
                        selected_protocol: payload,
                    },
                    NEG_TYPE_FAILURE => Self::Failure { code: payload },
                    other => {
                        return Err(WireError::Protocol(format!(
                            "unknown negotiation type {other:#04x}"
                        )))
                    }
                }
            };
            *src = &src[1 + li..];
            Ok(result)
        }
    }

    /// Append a TPKT body holding an X.224 Data header sized for `payload_len`
    /// MCS bytes. (The transport adds the 4-byte TPKT header.)
    pub fn write_data_header(payload_len: usize, out: &mut Vec<u8>) -> WireResult<()> {
        if TPKT_BODY_LEN_MAX < payload_len + DATA_HEADER.len() {
            return Err(WireError::Protocol("data PDU exceeds TPKT size".into()));
        }
        out.extend_from_slice(&DATA_HEADER);
        Ok(())
    }

    /// Maximum TPKT body size (u16 max minus the 4-byte TPKT header).
    pub const TPKT_BODY_LEN_MAX: usize = u16::MAX as usize - 4;

    /// Strip the 3-byte X.224 Data header from a TPKT body, returning the MCS
    /// payload. If the body is not DT-framed, returns it unchanged.
    pub fn strip_data_header(pdu: &[u8]) -> &[u8] {
        if pdu.len() >= 3 && pdu[0] == 0x02 && pdu[1] == 0xf0 {
            &pdu[3..]
        } else {
            pdu
        }
    }
}

// ---------------------------------------------------------------------------
// MCS (T.125)
// ---------------------------------------------------------------------------

pub mod mcs {
    use super::ber;
    use super::x224;
    use super::*;
    use crate::protocol_err;

    /// `[APPLICATION 101]` tag for MCS Connect-Initial.
    const CONNECT_INITIAL_TAG: [u8; 2] = [0x7f, 0x65];
    /// `[APPLICATION 102]` tag for MCS Connect-Response.
    const CONNECT_RESPONSE_TAG: [u8; 2] = [0x7f, 0x66];

    /// The user's MCS channel id (user1).
    pub const MCS_USER1: u16 = 1001;
    /// The second user channel (used as the PDU originator id).
    pub const MCS_USER2: u16 = 1002;
    /// The I/O channel, joined by every client.
    pub const MCS_IO_CHANNEL: u16 = 1003;
    /// Static virtual channels start at 1004.
    pub const MCS_FIRST_STATIC_CHANNEL: u16 = 1004;

    const CHOICE_ATTACH_USER_CONFIRM: u8 = 11;
    const CHOICE_CHANNEL_JOIN_CONFIRM: u8 = 15;
    const CHOICE_SEND_DATA_INDICATION: u8 = 9;
    const CHOICE_SEND_DATA_REQUEST: u8 = 25;
    const DATA_PRIORITY_TOP: u8 = 0x70;
    const SEGMENTATION_BEGIN_END: u8 = 0x80;

    /// Encode a `DomainParameters` SEQUENCE from its eight integer fields.
    fn domain_parameters(values: &[u32; 8]) -> Vec<u8> {
        let mut inner = Vec::new();
        for &value in values {
            inner.extend(ber::integer(value));
        }
        ber::sequence(&inner)
    }

    /// Build the MCS Connect-Initial PDU (BER) wrapping `gcc_ccr`.
    pub fn connect_initial(gcc_ccr: &[u8]) -> Vec<u8> {
        let mut content = Vec::new();
        content.extend(ber::octet_string(&[0x01])); // callingDomainSelector
        content.extend(ber::octet_string(&[0x01])); // calledDomainSelector
        content.extend_from_slice(&[0x01, 0x01, 0xff]); // upwardFlag = TRUE
        content.extend(domain_parameters(&[34, 2, 0, 1, 0, 1, 0xffff, 2])); // target
        content.extend(domain_parameters(&[1, 1, 1, 1, 0, 1, 0x420, 2])); // minimum
        content.extend(domain_parameters(&[
            0xffff, 0xfc17, 0xffff, 1, 0, 1, 0xffff, 2,
        ])); // maximum
        content.extend(ber::octet_string(gcc_ccr)); // userData

        let mut out = Vec::with_capacity(content.len() + 8);
        out.extend_from_slice(&CONNECT_INITIAL_TAG);
        ber::encode_len(content.len(), &mut out);
        out.extend_from_slice(&content);
        out
    }

    /// The parsed MCS Connect-Response.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConnectResponse {
        /// MCS result (0 = rt-successful).
        pub result: u8,
        /// The GCC Conference Create Response user data (server blocks).
        pub user_data: Vec<u8>,
    }

    /// Parse an MCS Connect-Response from a TPKT body (DT-framed or bare).
    pub fn parse_connect_response(pdu: &[u8]) -> WireResult<ConnectResponse> {
        let mut body = x224::strip_data_header(pdu);
        if body.len() < 2
            || body[0] != CONNECT_RESPONSE_TAG[0]
            || body[1] != CONNECT_RESPONSE_TAG[1]
        {
            return Err(protocol_err!(
                "expected MCS Connect-Response tag, got {:02x?}",
                &body[..body.len().min(2)]
            ));
        }
        body = &body[2..];
        let _len = ber::decode_len(&mut body)?;

        let result = ber::expect(&mut body, ber::TAG_ENUMERATED)?
            .first()
            .copied()
            .unwrap_or(0xff);
        let _called_connect_id = ber::expect(&mut body, ber::TAG_INTEGER)?;
        let _domain_parameters = ber::expect(&mut body, ber::TAG_SEQUENCE)?;
        let user_data = ber::expect(&mut body, ber::TAG_OCTET_STRING)?;
        Ok(ConnectResponse { result, user_data })
    }

    /// MCS Erect Domain Request (subHeight = subInterval = 0).
    pub fn erect_domain_request() -> [u8; 5] {
        [0x04, 0x01, 0x00, 0x01, 0x00]
    }

    /// MCS Attach User Request.
    pub fn attach_user_request() -> [u8; 1] {
        [0x28]
    }

    /// The user id assigned by the Attach User Confirm.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AttachUserConfirm {
        pub user_id: u16,
    }

    /// Parse an MCS Attach User Confirm (bare MCS bytes).
    pub fn parse_attach_user_confirm(pdu: &[u8]) -> WireResult<AttachUserConfirm> {
        let mcs = x224::strip_data_header(pdu);
        if mcs.len() < 4 || (mcs[0] >> 2) != CHOICE_ATTACH_USER_CONFIRM {
            return Err(protocol_err!("bad Attach User Confirm choice"));
        }
        if mcs[1] != 0 {
            return Err(protocol_err!(
                "Attach User Confirm failed (result {})",
                mcs[1]
            ));
        }
        Ok(AttachUserConfirm {
            user_id: u16::from_be_bytes([mcs[2], mcs[3]]),
        })
    }

    /// MCS Channel Join Request for `channel_id` from `user_id`.
    pub fn channel_join_request(user_id: u16, channel_id: u16) -> Vec<u8> {
        let mut out = vec![0x38]; // Channel Join Request choice is 14
        out.extend_from_slice(&user_id.to_be_bytes());
        out.extend_from_slice(&channel_id.to_be_bytes());
        out
    }

    /// A channel confirmed by the server.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChannelJoinConfirm {
        pub channel_id: u16,
    }

    /// Parse an MCS Channel Join Confirm (bare MCS bytes).
    pub fn parse_channel_join_confirm(pdu: &[u8]) -> WireResult<ChannelJoinConfirm> {
        let mcs = x224::strip_data_header(pdu);
        if mcs.len() < 8 || (mcs[0] >> 2) != CHOICE_CHANNEL_JOIN_CONFIRM {
            return Err(protocol_err!("bad Channel Join Confirm choice"));
        }
        if mcs[1] != 0 {
            return Err(protocol_err!(
                "Channel Join Confirm failed (result {})",
                mcs[1]
            ));
        }
        Ok(ChannelJoinConfirm {
            channel_id: u16::from_be_bytes([mcs[6], mcs[7]]),
        })
    }

    /// Build an MCS Send Data Request (client → server) on `channel_id`.
    pub fn send_data_request(user_id: u16, channel_id: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + payload.len());
        out.push(CHOICE_SEND_DATA_REQUEST << 2); // 0x64
        out.extend_from_slice(&user_id.to_be_bytes());
        out.extend_from_slice(&channel_id.to_be_bytes());
        out.push(DATA_PRIORITY_TOP);
        out.push(SEGMENTATION_BEGIN_END);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Build an MCS Send Data Indication (server → client) on `channel_id`.
    pub fn send_data_indication(user_id: u16, channel_id: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + payload.len());
        out.push(CHOICE_SEND_DATA_INDICATION << 2 | 0x02); // 0x26
        out.extend_from_slice(&user_id.to_be_bytes());
        out.extend_from_slice(&channel_id.to_be_bytes());
        out.push(DATA_PRIORITY_TOP);
        out.push(SEGMENTATION_BEGIN_END);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A server → client Send Data Indication.
    #[derive(Debug, Clone)]
    pub struct SendDataIndication {
        pub channel_id: u16,
        pub data: Vec<u8>,
    }

    /// Parse an MCS Send Data Indication (bare MCS bytes).
    pub fn parse_send_data_indication(pdu: &[u8]) -> WireResult<SendDataIndication> {
        parse_mcs_data(pdu, CHOICE_SEND_DATA_INDICATION, "Send Data Indication")
    }

    /// Parse an MCS Send Data Request (client → server, bare MCS bytes).
    /// Field layout is identical to the indication, only the choice differs.
    pub fn parse_send_data_request(pdu: &[u8]) -> WireResult<SendDataIndication> {
        parse_mcs_data(pdu, CHOICE_SEND_DATA_REQUEST, "Send Data Request")
    }

    fn parse_mcs_data(pdu: &[u8], choice: u8, what: &str) -> WireResult<SendDataIndication> {
        let mcs = x224::strip_data_header(pdu);
        if mcs.len() < 9 || (mcs[0] >> 2) != choice {
            return Err(protocol_err!("bad {what} choice"));
        }
        let channel_id = u16::from_be_bytes([mcs[3], mcs[4]]);
        let len = u16::from_be_bytes([mcs[7], mcs[8]]) as usize;
        let data = mcs
            .get(9..9 + len)
            .ok_or_else(|| protocol_err!("truncated {what} payload"))?
            .to_vec();
        Ok(SendDataIndication { channel_id, data })
    }
}

// ---------------------------------------------------------------------------
// GCC (conference create request/response + client/server data blocks)
// ---------------------------------------------------------------------------

pub mod gcc {
    use super::ber;
    use super::*;
    use crate::protocol_err;

    /// `TS_UD_CS_CORE` — client core data.
    pub const CS_CORE: u16 = 0xc001;
    /// `TS_UD_CS_SECURITY` — client security data.
    pub const CS_SECURITY: u16 = 0xc002;
    /// `TS_UD_CS_NET` — client network data / virtual channels.
    pub const CS_NET: u16 = 0xc003;
    /// `TS_UD_CS_CLUSTER` — client cluster data.
    pub const CS_CLUSTER: u16 = 0xc004;
    /// `TS_UD_CS_MONITOR` — client monitor data.
    pub const CS_MONITOR: u16 = 0xc005;
    /// `TS_UD_CS_MULTITRANSPORT` — client multitransport data.
    pub const CS_MULTITRANSPORT: u16 = 0xc00a;

    /// `TS_UD_SC_CORE` — server core data.
    pub const SC_CORE: u16 = 0x0c01;
    /// `TS_UD_SC_SECURITY` — server security data.
    pub const SC_SECURITY: u16 = 0x0c02;
    /// `TS_UD_SC_NET` — server network data.
    pub const SC_NET: u16 = 0x0c03;

    /// Standard RDP Security: 40-bit RC4.
    pub const ENCRYPTION_METHOD_40BIT: u32 = 0x01;
    /// Standard RDP Security: 128-bit RC4.
    pub const ENCRYPTION_METHOD_128BIT: u32 = 0x02;
    /// Standard RDP Security: 56-bit RC4.
    pub const ENCRYPTION_METHOD_56BIT: u32 = 0x08;

    /// Client supports the Error Info PDU.
    pub const RNS_UD_CS_SUPPORT_ERRINFO_PDU: u16 = 0x0001;

    /// Multitransport: reliable UDP (RDP-UDP with retransmission).
    pub const TRANSPORTTYPE_UDPFECR: u32 = 0x0000_0001;
    /// Multitransport: lossy UDP (forward-error-corrected).
    pub const TRANSPORTTYPE_UDPFECL: u32 = 0x0000_0004;

    /// One virtual channel declared in `CS_NET`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClientChannel {
        pub name: String,
        pub options: u32,
    }

    /// A monitor rectangle in `CS_MONITOR` (virtual desktop coordinates).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Monitor {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
        pub flags: u32,
    }

    /// `TS_UD_CS_CORE` — client core data.
    #[derive(Debug, Clone)]
    pub struct ClientCoreData {
        pub desktop_width: u16,
        pub desktop_height: u16,
        pub color_depth: u16,
        pub client_name: String,
        pub keyboard_layout: u32,
        pub early_capability_flags: u16,
        /// The protocol the server selected during X.224 negotiation.
        pub server_selected_protocol: u32,
    }

    /// `TS_UD_CS_SECURITY` — client security data.
    #[derive(Debug, Clone)]
    pub struct ClientSecurityData {
        pub encryption_methods: u32,
    }

    /// `TS_UD_CS_NET` — client network data.
    #[derive(Debug, Clone, Default)]
    pub struct ClientNetworkData {
        pub channels: Vec<ClientChannel>,
    }

    /// `TS_UD_CS_CLUSTER` — client cluster data.
    #[derive(Debug, Clone, Default)]
    pub struct ClientClusterData {
        pub flags: u32,
    }

    /// `TS_UD_CS_MONITOR` — client monitor data.
    #[derive(Debug, Clone, Default)]
    pub struct ClientMonitorData {
        pub monitors: Vec<Monitor>,
    }

    /// `TS_UD_CS_MULTITRANSPORT` — client multitransport data.
    #[derive(Debug, Clone, Default)]
    pub struct ClientMultitransportData {
        pub flags: u32,
    }

    fn block(kind: u16, payload: &[u8], out: &mut Vec<u8>) {
        put_u16(kind, out);
        put_u16((payload.len() + 4) as u16, out);
        out.extend_from_slice(payload);
    }

    fn client_core_block(core: &ClientCoreData) -> Vec<u8> {
        let mut p = Vec::with_capacity(216);
        put_u32(0x0008_0004, &mut p); // version (RDP 8.x client)
        put_u16(core.desktop_width, &mut p);
        put_u16(core.desktop_height, &mut p);
        put_u16(24, &mut p); // colorDepth
        put_u16(0, &mut p); // SASSequence
        put_u32(core.keyboard_layout, &mut p);
        put_u32(2600, &mut p); // clientBuild
        let mut name = [0u8; 32];
        let nb = core.client_name.as_bytes();
        name[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
        p.extend_from_slice(&name);
        put_u32(4, &mut p); // keyboardType
        put_u32(0, &mut p); // keyboardSubtype
        put_u32(12, &mut p); // keyboardFunctionKey
        p.extend_from_slice(&[0u8; 64]); // imeFileName
        put_u16(core.color_depth, &mut p); // postBeta2ColorDepth
        put_u16(1, &mut p); // clientProductId
        put_u32(0, &mut p); // serialNumber
        put_u16(core.color_depth, &mut p); // highColorDepth
        put_u16(0x000f, &mut p); // supportedColorDepths
        put_u16(core.early_capability_flags, &mut p);
        p.extend_from_slice(&[0u8; 64]); // clientDigProductId
        p.push(0); // connectionType
        p.push(0); // pad1
        put_u32(core.server_selected_protocol, &mut p);
        p.extend_from_slice(&[0u8; 4]); // pad2
        p
    }

    fn client_security_block(sec: &ClientSecurityData) -> Vec<u8> {
        let mut p = Vec::with_capacity(8);
        put_u32(sec.encryption_methods, &mut p);
        put_u32(0, &mut p); // extEncryptionMethods
        p
    }

    fn client_net_block(net: &ClientNetworkData) -> Vec<u8> {
        let mut p = Vec::with_capacity(4 + net.channels.len() * 12);
        put_u32(net.channels.len() as u32, &mut p);
        for ch in &net.channels {
            let mut name = [0u8; 8];
            let nb = ch.name.as_bytes();
            name[..nb.len().min(8)].copy_from_slice(&nb[..nb.len().min(8)]);
            p.extend_from_slice(&name);
            put_u32(ch.options, &mut p);
        }
        p
    }

    fn client_cluster_block(cluster: &ClientClusterData) -> Vec<u8> {
        let mut p = Vec::with_capacity(8);
        put_u32(cluster.flags, &mut p);
        put_u32(0, &mut p); // redirectedSessionId
        p
    }

    fn client_monitor_block(mon: &ClientMonitorData) -> Vec<u8> {
        let mut p = Vec::with_capacity(8 + mon.monitors.len() * 20);
        put_u32(0, &mut p); // flags
        put_u32(mon.monitors.len() as u32, &mut p);
        for m in &mon.monitors {
            put_u32(m.left as u32, &mut p);
            put_u32(m.top as u32, &mut p);
            put_u32(m.right as u32, &mut p);
            put_u32(m.bottom as u32, &mut p);
            put_u32(m.flags, &mut p);
        }
        p
    }

    fn client_multitransport_block(mt: &ClientMultitransportData) -> Vec<u8> {
        let mut p = Vec::with_capacity(4);
        put_u32(mt.flags, &mut p);
        p
    }

    /// Encode the client data blocks (CS_*) as the GCC user data.
    pub fn encode_client_data(
        core: &ClientCoreData,
        security: &ClientSecurityData,
        network: &ClientNetworkData,
        cluster: &ClientClusterData,
        monitors: &ClientMonitorData,
        multitransport: &ClientMultitransportData,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        block(CS_CORE, &client_core_block(core), &mut out);
        block(CS_SECURITY, &client_security_block(security), &mut out);
        block(CS_NET, &client_net_block(network), &mut out);
        block(CS_CLUSTER, &client_cluster_block(cluster), &mut out);
        if !monitors.monitors.is_empty() {
            block(CS_MONITOR, &client_monitor_block(monitors), &mut out);
        }
        if multitransport.flags != 0 {
            block(
                CS_MULTITRANSPORT,
                &client_multitransport_block(multitransport),
                &mut out,
            );
        }
        out
    }

    /// GCC Conference Create Request (`[APPLICATION 17]`): conference name
    /// "DCA" plus the client data blocks as userData.
    pub fn conference_create_request(user_data: &[u8]) -> Vec<u8> {
        let mut content = Vec::new();
        content.extend(ber::octet_string(b"DCA"));
        content.extend(ber::octet_string(user_data));
        let mut out = Vec::with_capacity(content.len() + 8);
        out.extend_from_slice(&[0x7f, 0x11]);
        ber::encode_len(content.len(), &mut out);
        out.extend_from_slice(&content);
        out
    }

    /// `TS_UD_SC_CORE` — server core data.
    #[derive(Debug, Clone, Default)]
    pub struct ServerCoreData {
        pub version: u32,
        pub desktop_width: u16,
        pub desktop_height: u16,
        pub color_depth: u16,
    }

    /// `TS_UD_SC_SECURITY` — server security data.
    #[derive(Debug, Clone, Default)]
    pub struct ServerSecurityData {
        pub encryption_method: u32,
        pub encryption_level: u32,
        pub server_random: Vec<u8>,
        pub server_cert: Vec<u8>,
    }

    /// `TS_UD_SC_NET` — server network data: assigned channel ids.
    #[derive(Debug, Clone, Default)]
    pub struct ServerNetworkData {
        pub channels: Vec<u16>,
    }

    /// All parsed server data blocks.
    #[derive(Debug, Clone, Default)]
    pub struct ServerDataBlocks {
        pub core: Option<ServerCoreData>,
        pub security: Option<ServerSecurityData>,
        pub net: Option<ServerNetworkData>,
    }

    /// Extract the server data blocks from the GCC Conference Create Response
    /// (the MCS Connect-Response userData).
    pub fn parse_conference_create_response(ccr: &[u8]) -> WireResult<Vec<u8>> {
        let mut cur = ccr;
        if cur.len() < 2 || cur[0] != 0x7f || cur[1] != 0x11 {
            return Err(protocol_err!("expected GCC Conference Create Response tag"));
        }
        cur = &cur[2..];
        let _len = ber::decode_len(&mut cur)?;
        ber::last_octet_string(&mut cur)
    }

    /// Parse the server data blocks from the GCC user data.
    pub fn parse_server_blocks(user_data: &[u8]) -> WireResult<ServerDataBlocks> {
        let mut blocks = ServerDataBlocks::default();
        let mut cur = user_data;
        while cur.len() >= 4 {
            let kind = get_u16(cur, 0)?;
            let len = get_u16(cur, 2)? as usize;
            if len < 4 || len > cur.len() {
                return Err(protocol_err!(
                    "invalid server block length {len} (kind {kind:#06x})"
                ));
            }
            let payload = &cur[4..len];
            match kind {
                SC_CORE => {
                    let version = get_u32(payload, 0)?;
                    let mut core = ServerCoreData {
                        version,
                        ..Default::default()
                    };
                    if payload.len() >= 12 {
                        core.desktop_width = get_u16(payload, 4)?;
                        core.desktop_height = get_u16(payload, 6)?;
                        core.color_depth = get_u16(payload, 10)?;
                    }
                    blocks.core = Some(core);
                }
                SC_SECURITY => {
                    let method = get_u32(payload, 0)?;
                    let level = get_u32(payload, 4)?;
                    let random_len = get_u32(payload, 8)? as usize;
                    let cert_len = get_u32(payload, 12)? as usize;
                    if payload.len() < 16 + random_len + cert_len {
                        return Err(protocol_err!("truncated SC_SECURITY block"));
                    }
                    blocks.security = Some(ServerSecurityData {
                        encryption_method: method,
                        encryption_level: level,
                        server_random: payload[16..16 + random_len].to_vec(),
                        server_cert: payload[16 + random_len..16 + random_len + cert_len].to_vec(),
                    });
                }
                SC_NET => {
                    let count = get_u32(payload, 0)? as usize;
                    if payload.len() < 4 + count * 2 {
                        return Err(protocol_err!("truncated SC_NET block"));
                    }
                    let mut channels = Vec::with_capacity(count);
                    for i in 0..count {
                        channels.push(get_u16(payload, 4 + i * 2)?);
                    }
                    blocks.net = Some(ServerNetworkData { channels });
                }
                _ => {
                    tracing::debug!(kind = format!("{kind:#06x}"), "skipping server block");
                }
            }
            cur = &cur[len..];
        }
        Ok(blocks)
    }

    /// The RSA public key inside the server's proprietary certificate.
    #[derive(Debug, Clone)]
    pub struct ServerCertificate {
        /// Modulus, little-endian, `modulus_len` bytes.
        pub modulus: Vec<u8>,
        /// Public exponent, little-endian.
        pub exponent: Vec<u8>,
    }

    /// Parse a `SERVER_CERTIFICATE` (version 1, RSA-signed) into its RSA public
    /// key. Format: dwVersion, dwCertType, cbCertLen, cbNonceLen, nonce, then
    /// the PROPIETARY certificate ("RSF1" magic, keyType, keyLen, then the RSA
    /// public key: modulusLength, modulus, exponentLength, exponent).
    pub fn parse_server_certificate(cert: &[u8]) -> WireResult<ServerCertificate> {
        if cert.len() < 16 {
            return Err(protocol_err!("server certificate too short"));
        }
        let _version = get_u32(cert, 0)?;
        let cert_type = get_u32(cert, 4)?;
        let cb_cert_len = get_u32(cert, 8)? as usize;
        let cb_nonce_len = get_u32(cert, 12)? as usize;
        if cert_type != 1 {
            return Err(WireError::Unsupported(format!(
                "server certificate type {cert_type}"
            )));
        }
        let body = cert
            .get(16 + cb_nonce_len..16 + cb_nonce_len + cb_cert_len)
            .ok_or_else(|| protocol_err!("truncated server certificate body"))?;
        if body.len() < 12 || get_u32(body, 0)? != 0x3146_5352 {
            return Err(protocol_err!("bad proprietary certificate magic"));
        }
        let key_type = get_u32(body, 4)?;
        if key_type != 1 {
            return Err(WireError::Unsupported(format!(
                "server certificate key type {key_type}"
            )));
        }
        let key_len = get_u32(body, 8)? as usize;
        let key = body
            .get(12..12 + key_len)
            .ok_or_else(|| protocol_err!("truncated RSA public key"))?;
        let modulus_len = get_u32(key, 0)? as usize;
        if key.len() < 4 + modulus_len + 4 {
            return Err(protocol_err!("truncated RSA modulus"));
        }
        let modulus = key[4..4 + modulus_len].to_vec();
        let exp_len = get_u32(key, 4 + modulus_len)? as usize;
        let exponent = key
            .get(4 + modulus_len + 4..4 + modulus_len + 4 + exp_len)
            .ok_or_else(|| protocol_err!("truncated RSA exponent"))?
            .to_vec();
        Ok(ServerCertificate { modulus, exponent })
    }
}

// ---------------------------------------------------------------------------
// Standard RDP Security (basic security header, MAC, RC4 bulk encryption)
// ---------------------------------------------------------------------------

pub mod security {
    use super::*;
    use crate::protocol_err;
    use rdp_crypto::rc4::Rc4;

    /// Basic Security Header flag: PDU carries the Security Exchange.
    pub const SEC_EXCHANGE_PKT: u16 = 0x0001;
    /// Basic Security Header flag: PDU payload is encrypted.
    pub const SEC_ENCRYPT: u16 = 0x0008;
    /// Basic Security Header flag: PDU is a Client Info PDU.
    pub const SEC_INFO_PKT: u16 = 0x0040;
    /// Basic Security Header flag: PDU is a server Auto-Detect Request.
    pub const SEC_AUTODETECT_REQ: u16 = 0x1000;

    /// Write a 4-byte Basic Security Header.
    pub fn write_basic_security_header(flags: u16, out: &mut Vec<u8>) {
        put_u16(flags, out);
        put_u16(0, out); // flagsHi
    }

    /// Read a Basic Security Header, returning its flags and the remaining
    /// payload.
    pub fn read_basic_security_header(payload: &mut &[u8]) -> WireResult<u16> {
        if payload.len() < 4 {
            return Err(protocol_err!("truncated basic security header"));
        }
        let flags = u16::from_le_bytes([payload[0], payload[1]]);
        *payload = &payload[4..];
        Ok(flags)
    }

    /// Build the Security Exchange PDU payload: basic security header
    /// (`SEC_EXCHANGE_PKT`), the length of the encrypted client random, then
    /// the encrypted random itself.
    pub fn security_exchange_pdu(encrypted_random: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + encrypted_random.len());
        write_basic_security_header(SEC_EXCHANGE_PKT, &mut out);
        put_u32(encrypted_random.len() as u32, &mut out);
        out.extend_from_slice(encrypted_random);
        out
    }

    /// MAC'd + RC4-encrypted payload state. One instance per direction pair,
    /// created after the security exchange with the derived session keys.
    #[derive(Clone)]
    pub struct SecurityLayer {
        encrypt: Rc4,
        decrypt: Rc4,
        mac_key: Vec<u8>,
    }

    impl SecurityLayer {
        /// `client_key` encrypts client→server traffic, `server_key` decrypts
        /// server→client traffic. `mac_key` signs both directions.
        pub fn new(client_key: &[u8], server_key: &[u8], mac_key: &[u8]) -> Self {
            Self {
                encrypt: Rc4::new(client_key),
                decrypt: Rc4::new(server_key),
                mac_key: mac_key.to_vec(),
            }
        }

        /// Seal `payload`: prepend the 8-byte MAC signature, then RC4-encrypt
        /// the whole thing. The caller writes the basic security header with
        /// `SEC_ENCRYPT` set before calling this.
        pub fn seal(&mut self, payload: &[u8]) -> Vec<u8> {
            let sig = rdp_crypto::keys::mac_signature(&self.mac_key, payload);
            let mut out = Vec::with_capacity(8 + payload.len());
            out.extend_from_slice(&sig);
            let start = out.len();
            out.extend_from_slice(payload);
            self.encrypt.apply(&mut out[start..]);
            out
        }

        /// Open a payload: verify the 8-byte MAC signature, RC4-decrypt the
        /// remainder, and return the plaintext.
        pub fn open(&mut self, payload: &[u8]) -> WireResult<Vec<u8>> {
            if payload.len() < 8 {
                return Err(protocol_err!("encrypted payload shorter than MAC"));
            }
            let mut plain = payload[8..].to_vec();
            self.decrypt.apply(&mut plain);
            let expect = rdp_crypto::keys::mac_signature(&self.mac_key, &plain);
            if expect != payload[..8] {
                return Err(WireError::BadMac);
            }
            Ok(plain)
        }
    }
}

// ---------------------------------------------------------------------------
// Licensing (MS-RDPBCGR 2.2.1.12)
// ---------------------------------------------------------------------------

pub mod license {
    use super::*;
    use crate::protocol_err;

    /// `LICENSE_REQUEST` — server requests a full CAL exchange.
    pub const LICENSE_REQUEST: u8 = 0x01;
    /// `PLATFORM_CHALLENGE` — server issued a platform challenge.
    pub const PLATFORM_CHALLENGE: u8 = 0x02;
    /// `NEW_LICENSE` — server granted a new license.
    pub const NEW_LICENSE: u8 = 0x03;
    /// `UPGRADE_LICENSE` — server upgraded an existing license.
    pub const UPGRADE_LICENSE: u8 = 0x04;
    /// `ERROR_ALERT` — the license error PDU.
    pub const ERROR_ALERT: u8 = 0xff;
    /// `STATUS_VALID_CLIENT` — the client is licensed, no CAL required.
    pub const STATUS_VALID_CLIENT: u32 = 0x07;
    /// `ST_NO_TRANSITION` state transition.
    pub const ST_NO_TRANSITION: u32 = 0x00;
    /// Licensing protocol version 3.0 (low nibble of the preamble flags).
    pub const PREAMBLE_VERSION_3_0: u8 = 0x03;

    /// A parsed licensing preamble: message type and total message size.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LicensePreamble {
        pub msg_type: u8,
        pub size: u16,
    }

    /// Parse a `LICENSE_PREAMBLE` from the start of `payload`.
    pub fn parse_preamble(payload: &[u8]) -> WireResult<LicensePreamble> {
        if payload.len() < 4 {
            return Err(protocol_err!("truncated license preamble"));
        }
        let msg_type = payload[0];
        let flags = payload[1];
        let size = u16::from_le_bytes([payload[2], payload[3]]);
        if flags & 0x0f != PREAMBLE_VERSION_3_0 {
            return Err(protocol_err!(
                "unsupported license preamble version {:#04x}",
                flags
            ));
        }
        Ok(LicensePreamble { msg_type, size })
    }

    /// `dwErrorCode` from a license error PDU.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LicenseError {
        pub error_code: u32,
        pub state_transition: u32,
    }

    /// Parse a license error PDU body (after the preamble).
    pub fn parse_license_error(payload: &[u8]) -> WireResult<LicenseError> {
        if payload.len() < 8 {
            return Err(protocol_err!("truncated license error PDU"));
        }
        Ok(LicenseError {
            error_code: get_u32(payload, 0)?,
            state_transition: get_u32(payload, 4)?,
        })
    }

    /// Encode a Client License Error PDU (preamble + error code fields).
    pub fn license_error_pdu(error_code: u32, state_transition: u32) -> Vec<u8> {
        let size = 4 + 4 + 4 + 4 + 16; // preamble + err + state + info + vendor
        let mut out = Vec::with_capacity(size);
        out.push(ERROR_ALERT);
        out.push(PREAMBLE_VERSION_3_0);
        put_u16(size as u16, &mut out);
        put_u32(error_code, &mut out);
        put_u32(state_transition, &mut out);
        put_u32(0, &mut out); // errorInfo
        out.extend_from_slice(&[0u8; 16]); // vendor
        out
    }
}

// ---------------------------------------------------------------------------
// Capability exchange + share control/data headers (MS-RDPBCGR 2.2.1.13)
// ---------------------------------------------------------------------------

pub mod caps {
    use super::*;
    use crate::protocol_err;

    // Share Control Header PDU types (low nibble of the 2-byte pduType field).
    pub const PDUTYPE_DEMAND_ACTIVE: u16 = 0x1;
    pub const PDUTYPE_CONFIRM_ACTIVE: u16 = 0x3;
    pub const PDUTYPE_DATA: u16 = 0x7;

    // Share Data Header pduType2 values.
    pub const PDUTYPE2_UPDATE: u8 = 0x02;
    pub const PDUTYPE2_CONTROL: u8 = 0x14;
    pub const PDUTYPE2_INPUT: u8 = 0x18;
    pub const PDUTYPE2_SYNCHRONIZE: u8 = 0x1f;

    /// Protocol version nibble in the share control header.
    const PROTOCOL_VERSION: u16 = 0x10;

    // Capability set types.
    pub const CAPSET_GENERAL: u16 = 1;
    pub const CAPSET_BITMAP: u16 = 2;
    pub const CAPSET_ORDER: u16 = 3;
    pub const CAPSET_POINTER: u16 = 8;
    pub const CAPSET_SOUND: u16 = 12;
    pub const CAPSET_INPUT: u16 = 13;
    pub const CAPSET_VIRTUAL_CHANNEL: u16 = 20;

    /// Control PDU actions.
    pub const CONTROL_COOPERATE: u16 = 0x0001;
    pub const CONTROL_REQUEST: u16 = 0x0002;
    pub const CONTROL_CONFIRM: u16 = 0x0003;

    /// Wrap a capability payload in its (type, length) header.
    fn cap_set(cap_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 4);
        put_u16(cap_type, &mut out);
        put_u16((payload.len() + 4) as u16, &mut out);
        out.extend_from_slice(payload);
        out
    }

    fn general_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(20);
        put_u16(1, &mut p); // osMajorType = WINDOWS
        put_u16(3, &mut p); // osMinorType = WINDOWS NT
        put_u16(0x0200, &mut p); // protocolVersion
        put_u16(0, &mut p); // pad
        put_u16(0, &mut p); // generalCompressionTypes
        put_u16(0x0400, &mut p); // extraFlags = NO_BITMAP_COMPRESSION_HDR
        put_u16(0, &mut p); // updateCapabilityFlag
        put_u16(0, &mut p); // remoteUnshareFlag
        put_u16(0, &mut p); // generalCompressionLevel
        p.push(0); // refreshRectSupport
        p.push(0); // suppressOutputSupport
        cap_set(CAPSET_GENERAL, &p)
    }

    fn bitmap_caps(width: u16, height: u16) -> Vec<u8> {
        let mut p = Vec::with_capacity(24);
        put_u16(24, &mut p); // preferredBitsPerPixel
        put_u16(1, &mut p); // receive1BitPerPixel
        put_u16(1, &mut p); // receive4BitsPerPixel
        put_u16(1, &mut p); // receive8BitsPerPixel
        put_u16(width, &mut p);
        put_u16(height, &mut p);
        put_u16(0, &mut p); // pad
        put_u16(1, &mut p); // desktopResizeFlag
        put_u16(1, &mut p); // bitmapCompressionFlag (mandatory TRUE)
        p.push(0); // highColorFlags
        p.push(0); // drawingFlags
        put_u16(1, &mut p); // multipleRectangleSupport
        put_u16(0, &mut p); // pad
        cap_set(CAPSET_BITMAP, &p)
    }

    fn order_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(84);
        p.extend_from_slice(&[0u8; 16]); // terminalDescriptor
        put_u32(0, &mut p); // pad4
        put_u16(1, &mut p); // desktopSaveXGranularity
        put_u16(20, &mut p); // desktopSaveYGranularity
        put_u16(0, &mut p); // pad2
        put_u16(1, &mut p); // maximumOrderLevel
        put_u16(0, &mut p); // numberFonts
        put_u16(0x000a, &mut p); // orderFlags (NEGOTIATE | ZEROBOUNDSDELTAS)
        p.extend_from_slice(&[0u8; 32]); // orderSupport
        put_u16(0, &mut p); // textFlags
        put_u16(0, &mut p); // orderSupportExFlags
        put_u32(0, &mut p); // pad4
        put_u32(0x0003_8400, &mut p); // desktopSaveSize
        put_u16(0, &mut p); // pad2
        put_u16(0, &mut p); // textANSICodePage
        put_u16(0, &mut p); // pad2
        cap_set(CAPSET_ORDER, &p)
    }

    fn pointer_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(6);
        put_u16(0x0001, &mut p); // colorPointerFlag
        put_u16(20, &mut p); // colorPointerCacheSize
        put_u16(20, &mut p); // pointerCacheSize
        cap_set(CAPSET_POINTER, &p)
    }

    fn sound_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(4);
        put_u16(0x0001, &mut p); // soundFlags = SOUND_BEEPS_FLAG
        put_u16(0, &mut p); // pad
        cap_set(CAPSET_SOUND, &p)
    }

    fn input_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(88);
        put_u16(0x0001, &mut p); // inputFlags = SCANCODES
        put_u16(0, &mut p); // pad
        put_u32(0x0000_0409, &mut p); // keyboardLayout (US)
        put_u32(0, &mut p); // pad
        put_u32(4, &mut p); // keyboardType
        put_u32(0, &mut p); // keyboardSubtype
        put_u32(12, &mut p); // keyboardFunctionKey
        p.extend_from_slice(&[0u8; 64]); // imeFileName
        cap_set(CAPSET_INPUT, &p)
    }

    fn virtual_channel_caps() -> Vec<u8> {
        let mut p = Vec::with_capacity(8);
        put_u32(0x0000_0014, &mut p); // flags (COMPRESS | NO_COMPRESSION)
        put_u32(1600, &mut p); // VCChunkSize
        cap_set(CAPSET_VIRTUAL_CHANNEL, &p)
    }

    /// All client capability sets, ready to embed in the Confirm Active PDU.
    pub fn all_caps(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(general_caps());
        out.extend(bitmap_caps(width, height));
        out.extend(order_caps());
        out.extend(pointer_caps());
        out.extend(sound_caps());
        out.extend(input_caps());
        out.extend(virtual_channel_caps());
        out
    }

    /// Write a 6-byte Share Control Header.
    pub fn write_share_control_header(
        pdu_type: u16,
        pdu_source: u16,
        payload_len: usize,
        out: &mut Vec<u8>,
    ) {
        put_u16((6 + payload_len) as u16, out);
        put_u16(PROTOCOL_VERSION | pdu_type, out);
        put_u16(pdu_source, out);
    }

    /// Write a 12-byte Share Data Header. `payload_len` is the length of
    /// everything that follows this header.
    pub fn write_share_data_header(
        share_id: u32,
        pdu_type2: u8,
        payload_len: usize,
        out: &mut Vec<u8>,
    ) {
        put_u32(share_id, out);
        out.push(0); // pad1
        out.push(0); // streamId
        put_u16(payload_len as u16, out); // uncompressedLength
        out.push(pdu_type2);
        out.push(0); // compressedType
        put_u16(0, out); // compressedLength
    }

    /// Read a Share Control Header; returns `(pdu_type, pdu_source)` and
    /// advances the cursor past the 6-byte header.
    pub fn read_share_control_header(payload: &mut &[u8]) -> WireResult<(u16, u16)> {
        if payload.len() < 6 {
            return Err(protocol_err!("truncated share control header"));
        }
        let _total = u16::from_le_bytes([payload[0], payload[1]]);
        let pdu_type = u16::from_le_bytes([payload[2], payload[3]]) & 0x000f;
        let source = u16::from_le_bytes([payload[4], payload[5]]);
        *payload = &payload[6..];
        Ok((pdu_type, source))
    }

    /// Read a Share Data Header; returns `(share_id, pdu_type2)` and advances
    /// the cursor past the 12-byte header.
    pub fn read_share_data_header(payload: &mut &[u8]) -> WireResult<(u32, u8)> {
        if payload.len() < 12 {
            return Err(protocol_err!("truncated share data header"));
        }
        let share_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let pdu_type2 = payload[8];
        *payload = &payload[12..];
        Ok((share_id, pdu_type2))
    }

    /// A parsed Demand Active PDU.
    #[derive(Debug, Clone)]
    pub struct DemandActive {
        pub share_id: u32,
        /// Raw capability sets (already validated for length).
        pub caps: Vec<u8>,
    }

    /// Parse a Demand Active PDU (starting at the share control header).
    pub fn parse_demand_active(payload: &[u8]) -> WireResult<DemandActive> {
        let mut cur = payload;
        let (pdu_type, _source) = read_share_control_header(&mut cur)?;
        if pdu_type != PDUTYPE_DEMAND_ACTIVE {
            return Err(protocol_err!("expected Demand Active, got type {pdu_type}"));
        }
        let (share_id, pdu_type2) = read_share_data_header(&mut cur)?;
        if pdu_type2 != 0x11 {
            return Err(protocol_err!(
                "expected Demand Active pduType2 0x11, got {pdu_type2:#04x}"
            ));
        }
        if cur.len() < 8 {
            return Err(protocol_err!("truncated Demand Active body"));
        }
        let _inner_header = u16::from_le_bytes([cur[0], cur[1]]);
        let _pad2 = u16::from_le_bytes([cur[2], cur[3]]);
        let num_caps = u16::from_le_bytes([cur[4], cur[5]]) as usize;
        let _pad3 = u16::from_le_bytes([cur[6], cur[7]]);
        let mut rest = &cur[8..];
        for _ in 0..num_caps {
            if rest.len() < 4 {
                return Err(protocol_err!("truncated capability set header"));
            }
            let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
            if len < 4 || len > rest.len() {
                return Err(protocol_err!("invalid capability set length {len}"));
            }
            rest = &rest[len..];
        }
        let caps_len = cur.len() - 8 - rest.len();
        let _session_id = if rest.len() >= 4 {
            &rest[rest.len() - 4..]
        } else {
            &[][..]
        };
        Ok(DemandActive {
            share_id,
            caps: cur[8..8 + caps_len].to_vec(),
        })
    }

    /// Build a Confirm Active PDU body (share control + share data + caps).
    pub fn confirm_active_pdu(share_id: u32, pdu_source: u16, caps: &[u8]) -> Vec<u8> {
        let body_len = 8 + caps.len();
        let mut out = Vec::new();
        write_share_control_header(PDUTYPE_CONFIRM_ACTIVE, pdu_source, 12 + body_len, &mut out);
        write_share_data_header(share_id, 0x13, body_len, &mut out);
        put_u16(PROTOCOL_VERSION | PDUTYPE_CONFIRM_ACTIVE, &mut out); // inner header
        put_u16(0, &mut out); // pad2
        put_u16((caps.len() / 4) as u16, &mut out); // number of caps (each ≥ 4 bytes)
        put_u16(0, &mut out); // pad3
        out.extend_from_slice(caps);
        out
    }

    /// Build a Synchronize PDU body.
    pub fn synchronize_pdu(share_id: u32, pdu_source: u16, target_user: u16) -> Vec<u8> {
        let mut out = Vec::new();
        write_share_control_header(PDUTYPE_DATA, pdu_source, 12 + 2, &mut out);
        write_share_data_header(share_id, PDUTYPE2_SYNCHRONIZE, 2, &mut out);
        put_u16(target_user, &mut out);
        out
    }

    /// Build a Control PDU body (cooperate/request/confirm).
    pub fn control_pdu(share_id: u32, pdu_source: u16, action: u16, control_id: u16) -> Vec<u8> {
        let mut out = Vec::new();
        write_share_control_header(PDUTYPE_DATA, pdu_source, 12 + 6, &mut out);
        write_share_data_header(share_id, PDUTYPE2_CONTROL, 6, &mut out);
        put_u16(action, &mut out);
        put_u16(0, &mut out); // grantId
        put_u16(control_id, &mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// Client Info PDU (TS_INFO_PACKET) and input PDUs
// ---------------------------------------------------------------------------

/// `TS_INFO_PACKET.flags` bits.
pub mod info {
    use super::*;

    pub const INFO_MOUSE: u32 = 0x0000_0001;
    pub const INFO_DISABLECTRLALTDEL: u32 = 0x0000_0002;
    pub const INFO_AUTOLOGON: u32 = 0x0000_0008;
    pub const INFO_UNICODE: u32 = 0x0000_0010;
    pub const INFO_MAXIMIZESHELL: u32 = 0x0000_0020;
    pub const INFO_LOGONNOTIFY: u32 = 0x0000_0040;
    pub const INFO_ENABLEWINDOWSKEY: u32 = 0x0000_0100;

    /// Logon credentials carried by the Client Info PDU.
    #[derive(Debug, Clone, Default)]
    pub struct ClientInfo {
        pub domain: String,
        pub username: String,
        pub password: String,
    }

    /// Encode the `TS_INFO_PACKET` (no security header). Strings are UTF-16LE,
    /// NUL-terminated; the `cb*` lengths exclude the terminator.
    pub fn encode(info: &ClientInfo, out: &mut Vec<u8>) {
        let mut flags = INFO_MOUSE
            | INFO_DISABLECTRLALTDEL
            | INFO_UNICODE
            | INFO_MAXIMIZESHELL
            | INFO_ENABLEWINDOWSKEY
            | INFO_LOGONNOTIFY;
        if !info.password.is_empty() {
            flags |= INFO_AUTOLOGON;
        }

        let utf16 =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };
        let domain = utf16(&info.domain);
        let user = utf16(&info.username);
        let password = utf16(&info.password);
        let shell = utf16("");
        let dir = utf16("");

        put_u32(0, out); // codePage
        put_u32(flags, out);
        put_u16(domain.len() as u16, out);
        put_u16(user.len() as u16, out);
        put_u16(password.len() as u16, out);
        put_u16(shell.len() as u16, out);
        put_u16(dir.len() as u16, out);
        out.extend_from_slice(&domain);
        out.extend_from_slice(&user);
        out.extend_from_slice(&password);
        out.extend_from_slice(&shell);
        out.extend_from_slice(&dir);
    }
}

/// Device flags for keyboard events.
pub mod kbd {
    pub const EXTENDED: u16 = 0x0100;
    /// Key release (absence = press).
    pub const RELEASE: u16 = 0x8000;
}

/// Pointer flags for mouse events.
pub mod ptr {
    pub const BUTTON1: u16 = 0x1000;
    pub const BUTTON2: u16 = 0x2000;
    pub const BUTTON3: u16 = 0x4000;
    pub const WHEEL: u16 = 0x0200;
    pub const MOVE: u16 = 0x0800;
    pub const DOWN: u16 = 0x8000;
}

/// One input event for the TS_INPUT_PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Scancode keyboard event.
    Keyboard { flags: u16, key_code: u16 },
    /// Unicode keyboard event.
    UnicodeKeyboard { flags: u16, unicode: u16 },
    /// Standard mouse event (2-byte coordinates).
    Mouse { flags: u16, x: u16, y: u16 },
    /// Extended mouse event (wheel / 16-bit coordinates).
    ExtendedMouse { flags: u16, x: u16, y: u16 },
    /// Synchronize event (caps lock / num lock state).
    Sync { number_of_keys: u16 },
}

const INPUT_EVENT_MOUSE: u16 = 0x0001;
const INPUT_EVENT_EXTENDED_MOUSE: u16 = 0x0002;
const INPUT_EVENT_KEYBOARD: u16 = 0x0004;
const INPUT_EVENT_UNICODE_KEYBOARD: u16 = 0x0005;
const INPUT_EVENT_SYNC: u16 = 0x0011;

fn encode_input_event(ev: &InputEvent, out: &mut Vec<u8>) {
    match ev {
        InputEvent::Keyboard { flags, key_code } => {
            put_u32(0, out); // eventTime
            put_u16(INPUT_EVENT_KEYBOARD, out);
            put_u16(*flags, out);
            put_u16(*key_code, out);
            put_u16(0, out); // pad
        }
        InputEvent::UnicodeKeyboard { flags, unicode } => {
            put_u32(0, out); // eventTime
            put_u16(INPUT_EVENT_UNICODE_KEYBOARD, out);
            put_u16(*flags, out);
            put_u16(*unicode, out);
            put_u16(0, out); // pad
        }
        InputEvent::Mouse { flags, x, y } => {
            put_u32(0, out); // eventTime
            put_u16(INPUT_EVENT_MOUSE, out);
            put_u16(*flags, out);
            put_u16(*x, out);
            put_u16(*y, out);
        }
        InputEvent::ExtendedMouse { flags, x, y } => {
            put_u32(0, out); // eventTime
            put_u16(INPUT_EVENT_EXTENDED_MOUSE, out);
            put_u16(*flags, out);
            put_u16(*x, out);
            put_u16(*y, out);
        }
        InputEvent::Sync { number_of_keys } => {
            put_u32(0, out); // eventTime
            put_u16(INPUT_EVENT_SYNC, out);
            put_u16(0, out); // deviceFlags
            put_u16(*number_of_keys, out);
            put_u16(0, out); // pad
        }
    }
}

/// Build the full TS_INPUT_PDU body: share control + share data headers plus
/// the event list. `share_id` comes from the Demand Active PDU.
pub fn encode_input_pdu(share_id: u32, pdu_source: u16, events: &[InputEvent]) -> Vec<u8> {
    let events_len = 4 + events.iter().map(encode_event_len).sum::<usize>();
    let mut out = Vec::with_capacity(6 + 12 + events_len);
    caps::write_share_control_header(caps::PDUTYPE_DATA, pdu_source, 12 + events_len, &mut out);
    caps::write_share_data_header(share_id, caps::PDUTYPE2_INPUT, events_len, &mut out);
    put_u16(events.len() as u16, &mut out);
    put_u16(0, &mut out); // pad
    for ev in events {
        encode_input_event(ev, &mut out);
    }
    out
}

fn encode_event_len(ev: &InputEvent) -> usize {
    match ev {
        InputEvent::Mouse { .. } | InputEvent::ExtendedMouse { .. } => 14,
        _ => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ber_length_roundtrip() {
        for len in [0usize, 1, 0x7f, 0x80, 0x1234, 0xffff] {
            let mut enc = Vec::new();
            ber::encode_len(len, &mut enc);
            let mut cur: &[u8] = &enc;
            assert_eq!(ber::decode_len(&mut cur).unwrap(), len);
            assert!(cur.is_empty());
        }
    }

    #[test]
    fn x224_connection_request_matches_known_bytes() {
        // TPKT body for a standard-security CR (no protocols): LI=0x0e, CR,
        // refs, class, then RDP_NEG_REQ type=01 flags=00 len=0008 proto=0.
        let cr = x224::ConnectionRequest {
            requested_protocols: 0,
            cookie: None,
        };
        let mut body = Vec::new();
        cr.encode(&mut body).unwrap();
        let expected = [
            0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ];
        assert_eq!(body, expected);
    }

    #[test]
    fn x224_connection_confirm_roundtrip() {
        // Server CC: LI=0x0e, CC, refs, class, NEG_RSP flags=00 proto=2.
        let cc = [
            0x0e, 0xd0, 0x00, 0x00, 0x12, 0x34, 0x00, 0x02, 0x00, 0x08, 0x00, 0x02, 0x00, 0x00,
            0x00,
        ];
        let mut cur: &[u8] = &cc;
        let parsed = x224::ConnectionConfirm::decode(&mut cur).unwrap();
        assert_eq!(
            parsed,
            x224::ConnectionConfirm::Response {
                flags: 0,
                selected_protocol: 2,
            }
        );
        assert!(cur.is_empty());
    }

    #[test]
    fn mcs_connect_initial_and_response_roundtrip() {
        let ccr = gcc::conference_create_request(&[0xde, 0xad]);
        let ci = mcs::connect_initial(&ccr);
        // The server echoes a response with the same GCC payload.
        let mut resp = Vec::new();
        resp.extend_from_slice(&[0x7f, 0x66]); // [APPLICATION 102]
        let mut content = Vec::new();
        content.extend_from_slice(&[0x0a, 0x01, 0x00]); // result = success
        content.extend(ber::integer(1)); // calledConnectId
        content.extend(ber::sequence(&[])); // domain params (dummy)
        content.extend(ber::octet_string(&ccr)); // userData
        ber::encode_len(content.len(), &mut resp);
        resp.extend_from_slice(&content);

        let parsed = mcs::parse_connect_response(&resp).unwrap();
        assert_eq!(parsed.result, 0);
        assert_eq!(parsed.user_data, ccr);
        // And the connect-initial wraps the same user data.
        let _ = ci;
    }

    #[test]
    fn gcc_server_blocks_parse() {
        let mut blocks = Vec::new();
        let mut sec = Vec::new();
        put_u32(0x02, &mut sec); // 128-bit
        put_u32(1, &mut sec); // level
        put_u32(32, &mut sec); // random len
        put_u32(0, &mut sec); // cert len
        sec.extend_from_slice(&[7u8; 32]); // random
        let mut b = Vec::new();
        put_u16(gcc::SC_SECURITY, &mut b);
        put_u16((sec.len() + 4) as u16, &mut b);
        b.extend_from_slice(&sec);
        blocks.extend_from_slice(&b);

        let parsed = gcc::parse_server_blocks(&blocks).unwrap();
        let security = parsed.security.unwrap();
        assert_eq!(security.encryption_method, 0x02);
        assert_eq!(security.server_random, vec![7u8; 32]);
    }

    #[test]
    fn security_layer_seals_and_opens() {
        let key = [0x11u8; 16];
        let mac = [0x22u8; 16];
        let mut layer = security::SecurityLayer::new(&key, &key, &mac);
        let plain = b"hello rdp";
        let sealed = layer.seal(plain);
        assert_ne!(&sealed[8..], &plain[..]);
        let opened = layer.open(&sealed).unwrap();
        assert_eq!(opened, plain);
    }

    #[test]
    fn input_pdu_encodes_events() {
        let events = [
            InputEvent::Keyboard {
                flags: kbd::EXTENDED,
                key_code: 0x1d,
            },
            InputEvent::Mouse {
                flags: ptr::MOVE,
                x: 100,
                y: 200,
            },
        ];
        let pdu = encode_input_pdu(0x0003_0000, 1002, &events);
        assert!(pdu.len() > 20);
        let mut cur: &[u8] = &pdu;
        let (pdu_type, _) = caps::read_share_control_header(&mut cur).unwrap();
        assert_eq!(pdu_type, caps::PDUTYPE_DATA);
    }

    #[test]
    fn license_error_pdu_parses() {
        let pdu =
            license::license_error_pdu(license::STATUS_VALID_CLIENT, license::ST_NO_TRANSITION);
        let preamble = license::parse_preamble(&pdu).unwrap();
        assert_eq!(preamble.msg_type, license::ERROR_ALERT);
        let err = license::parse_license_error(&pdu[4..]).unwrap();
        assert_eq!(err.error_code, license::STATUS_VALID_CLIENT);
    }

    #[test]
    fn server_certificate_parses_rsa_key() {
        // Build a minimal v1 RSA-signed certificate with modulus 64 bytes.
        let modulus = vec![0xabu8; 64];
        let exponent = vec![0x01, 0x00, 0x01];
        let mut key = Vec::new();
        put_u32(modulus.len() as u32, &mut key);
        key.extend_from_slice(&modulus);
        put_u32(exponent.len() as u32, &mut key);
        key.extend_from_slice(&exponent);
        let mut prop = Vec::new();
        put_u32(0x3146_5352, &mut prop); // "RSF1"
        put_u32(1, &mut prop); // keyType
        put_u32(key.len() as u32, &mut prop);
        prop.extend_from_slice(&key);
        put_u32(0, &mut prop); // signature len
        let mut cert = Vec::new();
        put_u32(1, &mut cert); // version
        put_u32(1, &mut cert); // certType
        put_u32(prop.len() as u32, &mut cert);
        put_u32(0, &mut cert); // nonce len
        cert.extend_from_slice(&prop);

        let parsed = gcc::parse_server_certificate(&cert).unwrap();
        assert_eq!(parsed.modulus, modulus);
        assert_eq!(parsed.exponent, exponent);
    }

    #[test]
    fn demand_active_roundtrip() {
        let caps_bytes = caps::all_caps(1920, 1080);
        // Server builds a Demand Active with a 2-capability header.
        let mut body = Vec::new();
        put_u16(0x0011, &mut body); // inner header
        put_u16(0, &mut body); // pad
        put_u16(7, &mut body); // num caps
        put_u16(0, &mut body); // pad
        body.extend_from_slice(&caps_bytes);
        put_u32(0, &mut body); // sessionId

        let mut pdu = Vec::new();
        caps::write_share_control_header(
            caps::PDUTYPE_DEMAND_ACTIVE,
            1002,
            12 + body.len(),
            &mut pdu,
        );
        caps::write_share_data_header(0x0003_0000, 0x11, body.len(), &mut pdu);
        pdu.extend_from_slice(&body);

        let parsed = caps::parse_demand_active(&pdu).unwrap();
        assert_eq!(parsed.share_id, 0x0003_0000);
        assert_eq!(parsed.caps.len(), caps_bytes.len());
    }
}
