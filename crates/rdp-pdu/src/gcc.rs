//! GCC conference client data blocks (`TS_UD_CS_*`, MS-RDPBCGR 2.2.1.3).
//!
//! These little-endian structures are concatenated as the client's user data
//! inside the GCC Conference Create Request, which is in turn carried by the
//! MCS Connect-Initial. Each block starts with a 4-byte header: a `u16` type
//! and a `u16` total length (header included).

/// `CS_CORE` — client core data (MS-RDPBCGR 2.2.1.3.2).
pub const CS_CORE: u16 = 0xC001;
/// `CS_SECURITY` — client security data (2.2.1.3.3).
pub const CS_SECURITY: u16 = 0xC002;
/// `CS_NET` — client network data / virtual channels (2.2.1.3.4).
pub const CS_NET: u16 = 0xC003;
/// `CS_CLUSTER` — client cluster data (2.2.1.3.5).
pub const CS_CLUSTER: u16 = 0xC004;
/// `CS_MONITOR` — client monitor data (2.2.1.3.6).
pub const CS_MONITOR: u16 = 0xC005;
/// `CS_MULTITRANSPORT` — client multitransport channel data (2.2.1.3.8).
pub const CS_MULTITRANSPORT: u16 = 0xC00A;

// `TS_UD_CS_CORE.earlyCapabilityFlags` bits (MS-RDPBCGR 2.2.1.3.2).
/// The client supports the Error Info PDU.
pub const RNS_UD_CS_SUPPORT_ERRINFO_PDU: u16 = 0x0001;
/// The `connectionType` field is valid (the server only reads it when set).
pub const RNS_UD_CS_VALID_CONNECTION_TYPE: u16 = 0x0020;
/// The client supports network characteristics auto-detect (MS-RDPBCGR 2.2.14):
/// it will answer the server's RTT/bandwidth Auto-Detect Request PDUs.
pub const RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT: u16 = 0x0080;
/// The client speaks the Graphics Pipeline (MS-RDPEGFX) dynamic channel; without
/// it Windows opens no graphics channel and the desktop stays black.
pub const RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL: u16 = 0x0100;

// `TS_UD_CS_CORE.connectionType` values (MS-RDPBCGR 2.2.1.3.2). Only honored when
// `RNS_UD_CS_VALID_CONNECTION_TYPE` is set. The server uses this hint to pick its
// experience profile (image quality / frame rate). LAN = its richest, lowest
// latency mode.
/// High-speed LAN (the lowest-latency server experience profile).
pub const CONNECTION_TYPE_LAN: u8 = 0x06;
/// Auto-detect: the link characteristics are determined via MS-RDPBCGR 2.2.14.
pub const CONNECTION_TYPE_AUTODETECT: u8 = 0x07;

// `TS_UD_CS_MULTITRANSPORT.flags` bits (MS-RDPBCGR 2.2.1.3.8). Advertising any of
// these is the switch that turns "RDP multipathing" on: only when this block is
// present does the server send a Server Initiate Multitransport Request
// (2.2.15.1) and move the graphics pipeline onto a side-band UDP transport. With
// no block the host stays TCP-only no matter what the rest of the stack can do.
/// `TRANSPORTTYPE_UDPFECR` — reliable UDP (RDP-UDP with retransmission), MS-RDPEUDP.
pub const TRANSPORTTYPE_UDPFECR: u32 = 0x0000_0001;
/// `TRANSPORTTYPE_UDPFECL` — lossy UDP (forward-error-corrected); carries the
/// real-time graphics channel where dropping a stale frame beats retransmitting it.
pub const TRANSPORTTYPE_UDPFECL: u32 = 0x0000_0004;
/// `TRANSPORTTYPE_UDP_PREFERRED` — ask the server to prefer UDP for graphics once
/// the side-band is up (the high-speed hint that keeps EGFX off TCP).
pub const TRANSPORTTYPE_UDP_PREFERRED: u32 = 0x0000_0100;

/// Dynamic virtual channel manager name — carries RDPGFX, RDPSND, CLIPRDR, etc.
pub const DRDYNVC_CHANNEL: &str = "drdynvc";

#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn header(kind: u16, total_len: u16, out: &mut Vec<u8>) {
    put_u16(kind, out);
    put_u16(total_len, out);
}

/// `TS_UD_CS_CORE`. Most fields are fixed at sensible client defaults; the
/// configurable ones cover the desktop size, keyboard, identity, color depth,
/// and the protocol selected during X.224 negotiation (which must be echoed).
#[derive(Debug, Clone)]
pub struct ClientCoreData {
    pub version: u32,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub keyboard_layout: u32,
    pub client_build: u32,
    pub client_name: String,
    pub high_color_depth: u16,
    pub supported_color_depths: u16,
    pub early_capability_flags: u16,
    /// `connectionType` hint (`CONNECTION_TYPE_*`). Only read by the server when
    /// `RNS_UD_CS_VALID_CONNECTION_TYPE` is set in `early_capability_flags`.
    pub connection_type: u8,
    /// The `selectedProtocol` from the X.224 Connection Confirm, echoed here.
    pub server_selected_protocol: u32,
}

impl Default for ClientCoreData {
    fn default() -> Self {
        Self {
            version: 0x0008_0004, // RDP 8.x client
            desktop_width: 1920,
            desktop_height: 1080,
            keyboard_layout: 0x0000_0409, // US
            client_build: 2600,
            client_name: "rdpio".into(),
            high_color_depth: 24,
            supported_color_depths: 0x000F, // 24/16/15/32
            // ERRINFO + GFX are mandatory for a working session; the GFX flag tells
            // the server we speak the Graphics Pipeline (MS-RDPEGFX) so it opens the
            // "Microsoft::Windows::RDS::Graphics" dynamic channel and streams the
            // desktop as EGFX (without it Windows 10/11 sends no bitmaps — a black
            // screen). VALID_CONNECTION_TYPE makes the server honor `connection_type`
            // below, and NETCHAR_AUTODETECT tells it we answer its RTT/bandwidth
            // probes (MS-RDPBCGR 2.2.14) — together they keep the host on its
            // lowest-latency LAN profile instead of a conservative default.
            early_capability_flags: RNS_UD_CS_SUPPORT_ERRINFO_PDU
                | RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL
                | RNS_UD_CS_VALID_CONNECTION_TYPE
                | RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT,
            // Known fast LAN to the host. We still answer continuous auto-detect
            // probes (NETCHAR_AUTODETECT above); LAN is the safe baseline if the
            // server skips detection.
            connection_type: CONNECTION_TYPE_LAN,
            server_selected_protocol: 0,
        }
    }
}

impl ClientCoreData {
    /// Encoded payload length (excluding the 4-byte block header).
    const PAYLOAD_LEN: usize = 212;

    pub fn encode(&self, out: &mut Vec<u8>) {
        header(CS_CORE, (4 + Self::PAYLOAD_LEN) as u16, out);
        put_u32(self.version, out);
        put_u16(self.desktop_width, out);
        put_u16(self.desktop_height, out);
        put_u16(0xCA01, out); // colorDepth (legacy 8bpp)
        put_u16(0xAA03, out); // SASSequence (RNS_UD_SAS_DEL)
        put_u32(self.keyboard_layout, out);
        put_u32(self.client_build, out);

        // clientName: 32 bytes, UTF-16LE, NUL-terminated, truncated to 15 chars.
        let mut name: Vec<u16> = self.client_name.encode_utf16().take(15).collect();
        name.push(0);
        let mut name_bytes = Vec::with_capacity(32);
        for unit in name {
            name_bytes.extend_from_slice(&unit.to_le_bytes());
        }
        name_bytes.resize(32, 0);
        out.extend_from_slice(&name_bytes);

        put_u32(4, out); // keyboardType = IBM enhanced (101/102 keys)
        put_u32(0, out); // keyboardSubType
        put_u32(12, out); // keyboardFunctionKey
        out.extend_from_slice(&[0u8; 64]); // imeFileName
        put_u16(0xCA01, out); // postBeta2ColorDepth
        put_u16(1, out); // clientProductId
        put_u32(0, out); // serialNumber
        put_u16(self.high_color_depth, out);
        put_u16(self.supported_color_depths, out);
        put_u16(self.early_capability_flags, out);
        out.extend_from_slice(&[0u8; 64]); // clientDigProductId
        out.push(self.connection_type); // connectionType (read iff VALID_CONNECTION_TYPE)
        out.push(0); // pad1octet
        put_u32(self.server_selected_protocol, out);
    }
}

/// Standard RDP Security encryption methods (`TS_UD_CS_SEC.encryptionMethods`).
pub const ENCRYPTION_METHOD_40BIT: u32 = 0x0000_0001;
pub const ENCRYPTION_METHOD_128BIT: u32 = 0x0000_0002;
pub const ENCRYPTION_METHOD_56BIT: u32 = 0x0000_0008;
pub const ENCRYPTION_METHOD_FIPS: u32 = 0x0000_0010;

/// `TS_UD_CS_SEC`. For TLS/NLA (enhanced security) both method masks are 0
/// (TLS provides confidentiality); for Standard RDP Security the client must
/// advertise the RC4 key strengths it supports or the server drops the link.
#[derive(Debug, Clone, Default)]
pub struct ClientSecurityData {
    pub encryption_methods: u32,
    pub ext_encryption_methods: u32,
}

impl ClientSecurityData {
    pub fn encode(&self, out: &mut Vec<u8>) {
        header(CS_SECURITY, 12, out);
        put_u32(self.encryption_methods, out);
        put_u32(self.ext_encryption_methods, out);
    }
}

/// A virtual channel request (8-char ANSI name + option flags).
#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub options: u32,
}

/// `CHANNEL_OPTION_*` flags (MS-RDPBCGR 2.2.1.3.4.1).
pub const CHANNEL_OPTION_INITIALIZED: u32 = 0x8000_0000;
pub const CHANNEL_OPTION_ENCRYPT_RDP: u32 = 0x4000_0000;
pub const CHANNEL_OPTION_SHOW_PROTOCOL: u32 = 0x0020_0000;

/// The clipboard static virtual channel name (MS-RDPECLIP).
pub const CLIPRDR_CHANNEL: &str = "cliprdr";

/// The audio output static virtual channel name (MS-RDPEA).
pub const RDPSND_CHANNEL: &str = "rdpsnd";

/// The device redirection static virtual channel name (MS-RDPEFS).
pub const RDPDR_CHANNEL: &str = "rdpdr";

/// The serial port redirection static virtual channel name (MS-RDPESP).
pub const SERIAL_CHANNEL: &str = "serial";

impl Channel {
    /// The dynamic virtual channel manager, initialized.
    pub fn drdynvc() -> Self {
        Self {
            name: DRDYNVC_CHANNEL.into(),
            options: CHANNEL_OPTION_INITIALIZED,
        }
    }

    /// The clipboard channel. `SHOW_PROTOCOL` keeps the `CHANNEL_PDU_HEADER` on
    /// inbound data (which the reassembler needs); `ENCRYPT_RDP` rides the
    /// connection's security like share data.
    pub fn cliprdr() -> Self {
        Self {
            name: CLIPRDR_CHANNEL.into(),
            options: CHANNEL_OPTION_INITIALIZED
                | CHANNEL_OPTION_ENCRYPT_RDP
                | CHANNEL_OPTION_SHOW_PROTOCOL,
        }
    }

    /// The audio output channel (RDPSND), same option set as the clipboard.
    pub fn rdpsnd() -> Self {
        Self {
            name: RDPSND_CHANNEL.into(),
            options: CHANNEL_OPTION_INITIALIZED
                | CHANNEL_OPTION_ENCRYPT_RDP
                | CHANNEL_OPTION_SHOW_PROTOCOL,
        }
    }

    /// The device redirection channel (RDPDR), same option set.
    pub fn rdpdr() -> Self {
        Self {
            name: RDPDR_CHANNEL.into(),
            options: CHANNEL_OPTION_INITIALIZED
                | CHANNEL_OPTION_ENCRYPT_RDP
                | CHANNEL_OPTION_SHOW_PROTOCOL,
        }
    }

    /// The serial port redirection channel (SERIAL), same option set.
    pub fn serial() -> Self {
        Self {
            name: SERIAL_CHANNEL.into(),
            options: CHANNEL_OPTION_INITIALIZED
                | CHANNEL_OPTION_ENCRYPT_RDP
                | CHANNEL_OPTION_SHOW_PROTOCOL,
        }
    }
}

/// `TS_UD_CS_NET`.
#[derive(Debug, Clone, Default)]
pub struct ClientNetworkData {
    pub channels: Vec<Channel>,
}

impl ClientNetworkData {
    /// Network data advertising just the dynamic VC manager (carries RDPGFX).
    pub fn with_drdynvc() -> Self {
        Self {
            channels: vec![Channel::drdynvc()],
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let total = 4 + 4 + self.channels.len() * 12;
        header(CS_NET, total as u16, out);
        put_u32(self.channels.len() as u32, out);
        for channel in &self.channels {
            let mut name = [0u8; 8];
            let bytes = channel.name.as_bytes();
            let n = bytes.len().min(8);
            name[..n].copy_from_slice(&bytes[..n]);
            out.extend_from_slice(&name);
            put_u32(channel.options, out);
        }
    }
}

/// `TS_UD_CS_CLUSTER`.
#[derive(Debug, Clone)]
pub struct ClientClusterData {
    pub flags: u32,
    pub redirected_session_id: u32,
}

impl Default for ClientClusterData {
    fn default() -> Self {
        // REDIRECTION_SUPPORTED (0x01) | REDIRECTION_VERSION4 (0x03 << 2).
        Self {
            flags: 0x0000_000D,
            redirected_session_id: 0,
        }
    }
}

impl ClientClusterData {
    pub fn encode(&self, out: &mut Vec<u8>) {
        header(CS_CLUSTER, 12, out);
        put_u32(self.flags, out);
        put_u32(self.redirected_session_id, out);
    }
}

/// One monitor in `TS_UD_CS_MONITOR`. Coordinates are in virtual-screen space
/// with the **primary monitor's top-left at `(0, 0)`** (the RDP convention);
/// monitors to the left of / above the primary therefore have negative `left`/
/// `top`. `right`/`bottom` are inclusive (i.e. `left + width - 1`). Exactly one
/// monitor should have `primary = true`. The server offsets this layout into a
/// single framebuffer using the minimum coordinate, so negatives are expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorDef {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// A raw monitor rectangle as the OS reports it (Win32 `rcMonitor`: `right`/
/// `bottom` are *exclusive*, and coordinates are in the virtual screen whose
/// origin may be negative when a monitor sits left of / above the primary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// Convert OS monitor rectangles into the RDP monitor layout. Windows already
/// reports the primary monitor's top-left as the virtual-screen origin `(0, 0)`
/// with other monitors positioned relative to it (left/upper monitors get
/// negative coordinates), which is exactly the convention `TS_UD_CS_MONITOR`
/// expects — so we pass the coordinates through unshifted and only convert
/// `right`/`bottom` from exclusive to inclusive. The server derives the
/// framebuffer offset from the minimum coordinate, so negative values are
/// correct (do **not** shift them to a non-negative origin: that misplaces every
/// monitor whenever the primary is not the top-left one).
pub fn normalize_monitor_layout(rects: &[VirtualScreenRect]) -> Vec<MonitorDef> {
    rects
        .iter()
        .map(|r| MonitorDef {
            left: r.left,
            top: r.top,
            right: r.right - 1,
            bottom: r.bottom - 1,
            primary: r.primary,
        })
        .collect()
}

/// `TS_UD_CS_MONITOR` — the client's monitor layout (for spanned multi-monitor
/// sessions). Empty = single-monitor (the block is omitted).
#[derive(Debug, Clone, Default)]
pub struct ClientMonitorData {
    pub monitors: Vec<MonitorDef>,
}

impl ClientMonitorData {
    pub fn encode(&self, out: &mut Vec<u8>) {
        // Omit the block entirely for the single-monitor case (≤1 monitor): the
        // desktop size in CS_CORE already covers it, and some servers reject a
        // 1-monitor CS_MONITOR.
        if self.monitors.len() < 2 {
            return;
        }
        let count = self.monitors.len().min(16);
        let total = 4 + 4 + 4 + count * 20;
        header(CS_MONITOR, total as u16, out);
        put_u32(0, out); // flags
        put_u32(count as u32, out); // monitorCount
        for m in self.monitors.iter().take(16) {
            put_u32(m.left as u32, out);
            put_u32(m.top as u32, out);
            put_u32(m.right as u32, out);
            put_u32(m.bottom as u32, out);
            put_u32(if m.primary { 1 } else { 0 }, out); // TS_MONITOR_PRIMARY
        }
    }
}

/// `TS_UD_CS_MULTITRANSPORT` — advertises the side-band UDP transports the client
/// can bring up (MS-RDPBCGR 2.2.1.3.8). The server only sends a Server Initiate
/// Multitransport Request when this block is present, so it is the switch that
/// turns RDP "multipathing" on. `flags == 0` omits the block (the host stays
/// TCP-only), so a client that can't dial UDP simply leaves it default.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientMultitransportData {
    /// `TRANSPORTTYPE_*` bitmask. Zero = don't advertise multitransport.
    pub flags: u32,
}

impl ClientMultitransportData {
    /// Advertise reliable + lossy UDP and ask the server to prefer UDP for the
    /// graphics channel — the high-speed profile that lets EGFX ride the lossy
    /// side-band instead of TCP. (`SOFTSYNC_TCP_TO_UDP` is intentionally left
    /// out: we don't implement transitioning live TCP channels onto UDP.)
    pub fn enabled() -> Self {
        Self {
            flags: TRANSPORTTYPE_UDPFECR | TRANSPORTTYPE_UDPFECL | TRANSPORTTYPE_UDP_PREFERRED,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        // Omit the block when nothing is advertised: a zero-flags block would
        // still invite a multitransport request we couldn't service.
        if self.flags == 0 {
            return;
        }
        header(CS_MULTITRANSPORT, 8, out);
        put_u32(self.flags, out);
    }
}

/// Concatenate the client data blocks into the user-data payload that the GCC
/// Conference Create Request carries (block order matters: core, security,
/// network, cluster, monitor, multitransport).
pub fn encode_client_data(
    core: &ClientCoreData,
    security: &ClientSecurityData,
    network: &ClientNetworkData,
    cluster: &ClientClusterData,
    monitors: &ClientMonitorData,
    multitransport: &ClientMultitransportData,
) -> Vec<u8> {
    let mut out = Vec::new();
    core.encode(&mut out);
    security.encode(&mut out);
    network.encode(&mut out);
    cluster.encode(&mut out);
    monitors.encode(&mut out);
    multitransport.encode(&mut out);
    out
}

/// Append a PER length: one byte below 128, otherwise a 2-byte big-endian value
/// with the high bit set (the ALIGNED PER lengths RDP uses in the GCC CCR).
fn per_length(value: usize, out: &mut Vec<u8>) {
    if value > 0x7f {
        out.push(0x80 | (value >> 8) as u8);
        out.push((value & 0xff) as u8);
    } else {
        out.push(value as u8);
    }
}

/// Wrap the concatenated client data blocks in a GCC Conference Create Request
/// (T.124). The preamble is the fixed RDP CCR header; the H.221 client key is
/// `"Duca"`. This is what the MCS Connect-Initial carries as its `userData`.
pub fn conference_create_request(user_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(user_data.len() + 24);
    // ConnectData: key = OBJECT IDENTIFIER { 0 0 20 124 0 1 } (ITU-T T.124).
    out.extend_from_slice(&[0x00, 0x05, 0x00, 0x14, 0x7c, 0x00, 0x01]);
    // connectPDU length = the 14 fixed bytes that follow + the user data.
    per_length(user_data.len() + 14, &mut out);
    // ConferenceCreateRequest: choice + selection(0x08) + conferenceName "1" +
    // padding + one UserData set + choice(0xC0) + h221NonStandard "Duca".
    out.extend_from_slice(&[
        0x00, 0x08, 0x00, 0x10, 0x00, 0x01, 0xc0, 0x00, 0x44, 0x75, 0x63, 0x61,
    ]);
    // userData OCTET STRING length, then the blocks themselves.
    per_length(user_data.len(), &mut out);
    out.extend_from_slice(user_data);
    out
}

// --- Server data blocks (TS_UD_SC_*) ----------------------------------------

/// `SC_CORE` — server core data.
pub const SC_CORE: u16 = 0x0C01;
/// `SC_SECURITY` — server security data.
pub const SC_SECURITY: u16 = 0x0C02;
/// `SC_NET` — server network data (MCS channel ids).
pub const SC_NET: u16 = 0x0C03;

/// An RSA public key extracted from the server's Proprietary Certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RsaPublicKey {
    /// Modulus, little-endian, `bit_len / 8` bytes (padding stripped).
    pub modulus_le: Vec<u8>,
    /// Public exponent, little-endian (4 bytes).
    pub exponent_le: Vec<u8>,
    pub bit_len: u32,
}

/// The interesting fields parsed from the server's GCC data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerData {
    /// The MCS I/O channel id (the global channel for share data).
    pub io_channel_id: u16,
    /// MCS channel ids assigned to the virtual channels we requested, in order.
    pub channel_ids: Vec<u16>,
    pub version: u32,
    pub encryption_method: u32,
    pub encryption_level: u32,
    /// Server random (Standard RDP Security key exchange input).
    pub server_random: Vec<u8>,
    /// Server RSA public key (Standard RDP Security), if a Proprietary
    /// Certificate was supplied. `None` for TLS/NLA or X.509 certs.
    pub public_key: Option<RsaPublicKey>,
}

#[inline]
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Parse an RDP Proprietary Certificate (MS-RDPBCGR 2.2.1.4.3.1.1) into its RSA
/// public key. Returns `None` for X.509 chains or malformed input.
fn parse_proprietary_certificate(cert: &[u8]) -> Option<RsaPublicKey> {
    if cert.len() < 16 {
        return None;
    }
    // dwVersion (low 31 bits): 1 = Proprietary Certificate.
    if le_u32(&cert[0..4]) & 0x7FFF_FFFF != 1 {
        return None;
    }
    // Skip dwSigAlgId(4) + dwKeyAlgId(4) -> offset 12.
    let blob_type = u16::from_le_bytes([cert[12], cert[13]]);
    let blob_len = u16::from_le_bytes([cert[14], cert[15]]) as usize;
    if blob_type != 0x0006 || cert.len() < 16 + blob_len {
        return None; // not a BB_RSA_KEY_BLOB
    }
    let blob = &cert[16..16 + blob_len];
    // RSA_PUBLIC_KEY: magic "RSA1"(4) keylen(4) bitlen(4) datalen(4) pubExp(4) modulus(keylen).
    if blob.len() < 20 || &blob[0..4] != b"RSA1" {
        return None;
    }
    let keylen = le_u32(&blob[4..8]) as usize;
    let bitlen = le_u32(&blob[8..12]) as usize;
    let exponent_le = blob[16..20].to_vec();
    if blob.len() < 20 + keylen || bitlen / 8 > keylen {
        return None;
    }
    let modulus_le = blob[20..20 + bitlen / 8].to_vec();
    Some(RsaPublicKey {
        modulus_le,
        exponent_le,
        bit_len: bitlen as u32,
    })
}

/// Read an ALIGNED PER length (1 or 2 bytes), advancing the cursor.
fn per_read_length(input: &mut &[u8]) -> Option<usize> {
    let (&b0, rest) = input.split_first()?;
    if b0 & 0x80 != 0 {
        let (&b1, rest2) = rest.split_first()?;
        *input = rest2;
        Some((((b0 & 0x7f) as usize) << 8) | b1 as usize)
    } else {
        *input = rest;
        Some(b0 as usize)
    }
}

/// Parse the server GCC Conference Create Response user data — the `SC_*`
/// blocks that follow the server H.221 key `"McDn"`.
pub fn parse_server_data(user_data: &[u8]) -> Option<ServerData> {
    let pos = user_data.windows(4).position(|w| w == b"McDn")?;
    let mut after_key = &user_data[pos + 4..];
    let _user_data_len = per_read_length(&mut after_key)?;

    let mut data = after_key;
    let mut sd = ServerData::default();
    while data.len() >= 4 {
        let kind = u16::from_le_bytes([data[0], data[1]]);
        let len = u16::from_le_bytes([data[2], data[3]]) as usize;
        if len < 4 || len > data.len() {
            break;
        }
        let payload = &data[4..len];
        match kind {
            SC_CORE if payload.len() >= 4 => {
                sd.version = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            }
            SC_SECURITY if payload.len() >= 8 => {
                sd.encryption_method = le_u32(&payload[0..4]);
                sd.encryption_level = le_u32(&payload[4..8]);
                // When encryption is in effect, the server random + certificate follow.
                if payload.len() >= 16 {
                    let random_len = le_u32(&payload[8..12]) as usize;
                    let cert_len = le_u32(&payload[12..16]) as usize;
                    let rand_start = 16;
                    let cert_start = rand_start + random_len;
                    if payload.len() >= cert_start {
                        sd.server_random = payload[rand_start..cert_start].to_vec();
                    }
                    if payload.len() >= cert_start + cert_len {
                        sd.public_key = parse_proprietary_certificate(
                            &payload[cert_start..cert_start + cert_len],
                        );
                    }
                }
            }
            SC_NET if payload.len() >= 4 => {
                sd.io_channel_id = u16::from_le_bytes([payload[0], payload[1]]);
                let count = u16::from_le_bytes([payload[2], payload[3]]) as usize;
                let mut off = 4;
                for _ in 0..count {
                    if off + 2 > payload.len() {
                        break;
                    }
                    sd.channel_ids
                        .push(u16::from_le_bytes([payload[off], payload[off + 1]]));
                    off += 2;
                }
            }
            _ => {}
        }
        data = &data[len..];
    }
    Some(sd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_len(bytes: &[u8]) -> u16 {
        u16::from_le_bytes([bytes[2], bytes[3]])
    }

    #[test]
    fn cs_security_exact_bytes_for_tls() {
        let mut out = Vec::new();
        ClientSecurityData::default().encode(&mut out);
        assert_eq!(
            out,
            vec![0x02, 0xC0, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn cs_core_header_length_and_selected_protocol() {
        let core = ClientCoreData {
            server_selected_protocol: 2, // HYBRID
            ..Default::default()
        };
        let mut out = Vec::new();
        core.encode(&mut out);
        // Header: type = CS_CORE (LE), length = 216.
        assert_eq!(&out[0..2], &[0x01, 0xC0]);
        assert_eq!(block_len(&out), 216);
        assert_eq!(out.len(), 216);
        // serverSelectedProtocol is the last 4 bytes, little-endian.
        assert_eq!(&out[out.len() - 4..], &[0x02, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn cs_core_advertises_lan_connection_type_and_autodetect() {
        let mut out = Vec::new();
        ClientCoreData::default().encode(&mut out);
        // Layout tail: ..., earlyCapabilityFlags (u16), clientDigProductId (64),
        // connectionType (1), pad1octet (1), serverSelectedProtocol (4).
        // earlyCapabilityFlags sits 64 + 1 + 1 + 4 = 70 bytes before the end.
        let flags = u16::from_le_bytes([out[out.len() - 72], out[out.len() - 71]]);
        assert_eq!(flags & RNS_UD_CS_VALID_CONNECTION_TYPE, RNS_UD_CS_VALID_CONNECTION_TYPE);
        assert_eq!(
            flags & RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT,
            RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT
        );
        // The GFX flag must survive (a black-screen regression otherwise).
        assert_eq!(
            flags & RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL,
            RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL
        );
        // connectionType is 6 bytes before the end (pad1octet + selectedProtocol).
        assert_eq!(out[out.len() - 6], CONNECTION_TYPE_LAN);
    }

    #[test]
    fn cs_net_advertises_drdynvc() {
        let mut out = Vec::new();
        ClientNetworkData::with_drdynvc().encode(&mut out);
        assert_eq!(&out[0..2], &[0x03, 0xC0]); // CS_NET
        assert_eq!(block_len(&out), 20);
        assert_eq!(u32::from_le_bytes([out[4], out[5], out[6], out[7]]), 1); // channelCount
        assert_eq!(&out[8..15], b"drdynvc");
        assert_eq!(out[15], 0); // NUL pad in the 8-byte name field
    }

    #[test]
    fn cs_monitor_layout() {
        // Single monitor (or none) → block omitted.
        let mut single = Vec::new();
        ClientMonitorData {
            monitors: vec![MonitorDef { left: 0, top: 0, right: 1919, bottom: 1079, primary: true }],
        }
        .encode(&mut single);
        assert!(single.is_empty());

        // Two monitors → CS_MONITOR block with 2 entries.
        let mut out = Vec::new();
        ClientMonitorData {
            monitors: vec![
                MonitorDef { left: 0, top: 0, right: 1919, bottom: 1079, primary: true },
                MonitorDef { left: 1920, top: 0, right: 3839, bottom: 1079, primary: false },
            ],
        }
        .encode(&mut out);
        assert_eq!(&out[0..2], &[0x05, 0xC0]); // CS_MONITOR
        assert_eq!(block_len(&out), 4 + 4 + 4 + 2 * 20);
        assert_eq!(u32::from_le_bytes([out[8], out[9], out[10], out[11]]), 2); // monitorCount
        // First monitor is primary (flags=1 at end of its 20-byte def).
        assert_eq!(u32::from_le_bytes([out[28], out[29], out[30], out[31]]), 1);
        // Second monitor left = 1920.
        assert_eq!(u32::from_le_bytes([out[32], out[33], out[34], out[35]]), 1920);
    }

    #[test]
    fn normalize_keeps_primary_at_origin_and_makes_inclusive() {
        // Primary 1920x1080 at virtual (0,0); a second monitor to the LEFT at
        // (-1920,0) — the layout Windows reports when the primary is on the right.
        let rects = [
            VirtualScreenRect { left: 0, top: 0, right: 1920, bottom: 1080, primary: true },
            VirtualScreenRect { left: -1920, top: 0, right: 0, bottom: 1080, primary: false },
        ];
        let defs = normalize_monitor_layout(&rects);
        // Primary stays at (0,0); the left monitor keeps its negative coordinate.
        assert_eq!(
            defs[0],
            MonitorDef { left: 0, top: 0, right: 1919, bottom: 1079, primary: true }
        );
        assert_eq!(
            defs[1],
            MonitorDef { left: -1920, top: 0, right: -1, bottom: 1079, primary: false }
        );
    }

    #[test]
    fn cs_cluster_defaults() {
        let mut out = Vec::new();
        ClientClusterData::default().encode(&mut out);
        assert_eq!(&out[0..2], &[0x04, 0xC0]); // CS_CLUSTER
        assert_eq!(block_len(&out), 12);
        assert_eq!(u32::from_le_bytes([out[4], out[5], out[6], out[7]]), 0x0D);
    }

    #[test]
    fn client_data_block_order() {
        let bytes = encode_client_data(
            &ClientCoreData::default(),
            &ClientSecurityData::default(),
            &ClientNetworkData::with_drdynvc(),
            &ClientClusterData::default(),
            &ClientMonitorData::default(),
            &ClientMultitransportData::default(),
        );
        // Walk the blocks by their length fields and confirm the type order.
        let mut off = 0usize;
        let mut kinds = Vec::new();
        while off + 4 <= bytes.len() {
            let kind = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
            let len = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) as usize;
            kinds.push(kind);
            off += len;
        }
        // Single (default) monitor and no multitransport → only the four blocks.
        assert_eq!(kinds, vec![CS_CORE, CS_SECURITY, CS_NET, CS_CLUSTER]);
        assert_eq!(off, bytes.len(), "blocks tile the buffer exactly");
    }

    #[test]
    fn cs_multitransport_advertised_when_enabled() {
        // Default = no UDP transports → block omitted entirely (TCP-only).
        let mut omitted = Vec::new();
        ClientMultitransportData::default().encode(&mut omitted);
        assert!(omitted.is_empty());

        // Enabled → an 8-byte CS_MULTITRANSPORT block with reliable + lossy + the
        // UDP-preferred hint, which is what makes the server offer multitransport.
        let mut out = Vec::new();
        ClientMultitransportData::enabled().encode(&mut out);
        assert_eq!(&out[0..2], &[0x0A, 0xC0]); // CS_MULTITRANSPORT (LE)
        assert_eq!(block_len(&out), 8);
        let flags = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!(
            flags,
            TRANSPORTTYPE_UDPFECR | TRANSPORTTYPE_UDPFECL | TRANSPORTTYPE_UDP_PREFERRED
        );

        // And it tiles on after the monitor block in the full client data.
        let bytes = encode_client_data(
            &ClientCoreData::default(),
            &ClientSecurityData::default(),
            &ClientNetworkData::with_drdynvc(),
            &ClientClusterData::default(),
            &ClientMonitorData::default(),
            &ClientMultitransportData::enabled(),
        );
        let mut off = 0usize;
        let mut kinds = Vec::new();
        while off + 4 <= bytes.len() {
            let len = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) as usize;
            kinds.push(u16::from_le_bytes([bytes[off], bytes[off + 1]]));
            off += len;
        }
        assert_eq!(
            kinds,
            vec![CS_CORE, CS_SECURITY, CS_NET, CS_CLUSTER, CS_MULTITRANSPORT]
        );
        assert_eq!(off, bytes.len());
    }

    #[test]
    fn ccr_has_rdp_preamble_and_duca() {
        let ccr = conference_create_request(&[0xAA; 10]);
        assert_eq!(&ccr[0..7], &[0x00, 0x05, 0x00, 0x14, 0x7c, 0x00, 0x01]);
        assert_eq!(ccr[7], (10 + 14) as u8); // connectPDU length (single byte)
        assert_eq!(
            &ccr[8..20],
            &[0x00, 0x08, 0x00, 0x10, 0x00, 0x01, 0xc0, 0x00, 0x44, 0x75, 0x63, 0x61]
        );
        assert_eq!(ccr[20], 10); // userData length (single byte)
        assert_eq!(&ccr[21..], &[0xAA; 10]);
    }

    #[test]
    fn ccr_uses_two_byte_per_length_for_large_user_data() {
        let ccr = conference_create_request(&vec![0u8; 260]);
        assert_eq!(&ccr[7..9], &[0x81, 0x12]); // connectPDU length 274
        assert_eq!(&ccr[21..23], &[0x81, 0x04]); // userData length 260
    }

    #[test]
    fn parse_server_data_extracts_channel_ids() {
        // "McDn" + PER length(10) + SC_NET{ io=1003, count=1, channel=1004 }.
        let mut ud = b"McDn".to_vec();
        ud.push(0x0a); // PER length of the SC_NET block
        ud.extend_from_slice(&[
            0x03, 0x0c, // SC_NET (LE)
            0x0a, 0x00, // block length = 10
            0xeb, 0x03, // io channel = 1003
            0x01, 0x00, // channelCount = 1
            0xec, 0x03, // channel = 1004
        ]);
        let sd = parse_server_data(&ud).unwrap();
        assert_eq!(sd.io_channel_id, 1003);
        assert_eq!(sd.channel_ids, vec![1004]);
    }

    #[test]
    fn parse_server_data_extracts_rsa_key() {
        let bitlen = 512u32;
        let keylen = bitlen / 8 + 8; // 72
        let mut blob = b"RSA1".to_vec();
        blob.extend_from_slice(&keylen.to_le_bytes());
        blob.extend_from_slice(&bitlen.to_le_bytes());
        blob.extend_from_slice(&(bitlen / 8 - 1).to_le_bytes()); // datalen
        blob.extend_from_slice(&[0x01, 0x00, 0x01, 0x00]); // pubExp = 65537
        blob.extend(std::iter::repeat(0x11u8).take(64)); // modulus
        blob.extend_from_slice(&[0u8; 8]); // padding

        let mut cert = Vec::new();
        cert.extend_from_slice(&1u32.to_le_bytes()); // dwVersion = Proprietary
        cert.extend_from_slice(&1u32.to_le_bytes()); // dwSigAlgId
        cert.extend_from_slice(&1u32.to_le_bytes()); // dwKeyAlgId
        cert.extend_from_slice(&0x0006u16.to_le_bytes()); // wPublicKeyBlobType
        cert.extend_from_slice(&(blob.len() as u16).to_le_bytes());
        cert.extend_from_slice(&blob);
        cert.extend_from_slice(&0x0008u16.to_le_bytes()); // wSignatureBlobType
        cert.extend_from_slice(&0u16.to_le_bytes()); // wSignatureBlobLen

        let mut payload = Vec::new();
        payload.extend_from_slice(&2u32.to_le_bytes()); // encryptionMethod
        payload.extend_from_slice(&2u32.to_le_bytes()); // encryptionLevel
        payload.extend_from_slice(&32u32.to_le_bytes()); // serverRandomLen
        payload.extend_from_slice(&(cert.len() as u32).to_le_bytes());
        payload.extend_from_slice(&[0xAB; 32]); // serverRandom
        payload.extend_from_slice(&cert);

        let mut block = SC_SECURITY.to_le_bytes().to_vec();
        block.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
        block.extend_from_slice(&payload);

        let mut ud = b"McDn".to_vec();
        ud.push(0x00); // PER length placeholder (parser ignores the value)
        ud.extend_from_slice(&block);

        let sd = parse_server_data(&ud).unwrap();
        assert_eq!(sd.encryption_method, 2);
        assert_eq!(sd.server_random, vec![0xAB; 32]);
        let key = sd.public_key.expect("rsa key parsed");
        assert_eq!(key.bit_len, 512);
        assert_eq!(key.modulus_le, vec![0x11; 64]);
        assert_eq!(key.exponent_le, vec![0x01, 0x00, 0x01, 0x00]);
    }
}
