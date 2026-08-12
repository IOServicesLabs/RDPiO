//! [`WireSession`] — the client-facing connection object.
//!
//! [`WireSession::connect`] runs the full connection sequence (see
//! [`crate::handshake`]) and returns a session that can send input and
//! redirection PDUs and receive server updates.

use crate::error::{WireError, WireResult};
use crate::handshake;
use crate::pdu::{caps, gcc, mcs, security, x224, InputEvent};
use crate::transport::{TransportOptions, WireTransport};

/// One monitor in the client's virtual desktop layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub flags: u32,
}

impl From<Monitor> for gcc::Monitor {
    fn from(m: Monitor) -> Self {
        gcc::Monitor {
            left: m.left,
            top: m.top,
            right: m.right,
            bottom: m.bottom,
            flags: m.flags,
        }
    }
}

/// Parameters for [`WireSession::connect`].
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Remote host name or IP.
    pub host: String,
    /// Remote TCP port.
    pub port: u16,
    /// Logon user name (also sent as the X.224 `mstshash` cookie).
    pub username: String,
    /// Logon password.
    pub password: String,
    /// Logon domain (may be empty).
    pub domain: String,
    /// Desktop width.
    pub width: u16,
    /// Desktop height.
    pub height: u16,
    /// Color depth (16, 24, or 32).
    pub color_depth: u16,
    /// Client computer name (sent in CS_CORE).
    pub client_name: String,
    /// Must be `true`: wire-main implements Standard RDP Security only.
    pub insecure: bool,
    /// Bind the UDP side-band socket (multitransport).
    pub udp: bool,
    /// Enable the clipboard static channel.
    pub clipboard: bool,
    /// Enable drive redirection (rdpdr).
    pub drive_redirection: bool,
    /// Enable audio playback (rdpsnd).
    pub audio_playback: bool,
    /// Enable audio input / microphone (via drdynvc).
    pub audio_input: bool,
    /// Enable camera redirection (via drdynvc).
    pub camera: bool,
    /// Enable printer redirection (rdpdr).
    pub printer: bool,
    /// Multi-monitor layout. Empty = single monitor (`width` × `height`).
    pub monitors: Vec<Monitor>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 3389,
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            width: 1920,
            height: 1080,
            color_depth: 24,
            client_name: "rdpio".into(),
            insecure: true,
            udp: false,
            clipboard: true,
            drive_redirection: false,
            audio_playback: true,
            audio_input: false,
            camera: false,
            printer: false,
            monitors: Vec::new(),
        }
    }
}

impl ConnectOptions {
    /// Static channel names this connection declares in CS_NET, in the order
    /// the server will assign channel ids.
    pub fn channel_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.clipboard {
            names.push("cliprdr".to_string());
        }
        if self.drive_redirection || self.printer {
            names.push("rdpdr".to_string());
        }
        if self.audio_playback {
            names.push("rdpsnd".to_string());
        }
        // drdynvc carries the dynamic channels (audio input, camera, and the
        // graphics pipeline), so it is always declared.
        names.push("drdynvc".to_string());
        names
    }

    /// The static channel name for a well-known redirection purpose.
    pub fn channel_name(&self, purpose: ChannelPurpose) -> Option<String> {
        let name = match purpose {
            ChannelPurpose::Clipboard => "cliprdr".to_string(),
            ChannelPurpose::DrivesAndPrinters => "rdpdr".to_string(),
            ChannelPurpose::AudioPlayback => "rdpsnd".to_string(),
            ChannelPurpose::Dynamic => "drdynvc".to_string(),
        };
        if self.channel_names().contains(&name) {
            Some(name)
        } else {
            None
        }
    }
}

/// Well-known static channel purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPurpose {
    /// `cliprdr` — clipboard redirection.
    Clipboard,
    /// `rdpdr` — drive and printer redirection.
    DrivesAndPrinters,
    /// `rdpsnd` — audio playback.
    AudioPlayback,
    /// `drdynvc` — dynamic virtual channel manager.
    Dynamic,
}

/// Negotiated session settings returned by [`WireSession::connect`].
#[derive(Debug, Clone)]
pub struct SessionSettings {
    pub host: String,
    pub port: u16,
    /// Desktop width negotiated in CS_CORE.
    pub desktop_width: u16,
    /// Desktop height negotiated in CS_CORE.
    pub desktop_height: u16,
    /// Color depth requested by the client.
    pub color_depth: u16,
    /// Security protocol selected during X.224 negotiation (0 = Standard).
    pub selected_protocol: u32,
    /// The MCS user channel id (attach-user confirm).
    pub user_channel: u16,
    /// The MCS I/O channel id (1003).
    pub io_channel: u16,
    /// Static virtual channels joined, with their assigned ids.
    pub static_channels: Vec<StaticChannel>,
    /// The share id from the Demand Active PDU.
    pub share_id: u32,
    /// Server capability sets from the Demand Active PDU (unparsed).
    pub server_caps: Vec<u8>,
    /// The server's CS_CORE version.
    pub server_core_version: u32,
}

/// A static virtual channel joined during the connection sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticChannel {
    pub name: String,
    pub id: u16,
}

/// A PDU received from the server after the connection sequence.
#[derive(Debug, Clone)]
pub enum ServerPdu {
    /// Data on a static/dynamic virtual channel (already decrypted).
    ChannelData { channel_id: u16, data: Vec<u8> },
    /// A graphics/fast-path update (payload as received, unparsed).
    GraphicsUpdate(Vec<u8>),
    /// A slow-path update PDU on the I/O channel (unparsed).
    Update(Vec<u8>),
    /// The server requested multitransport over the UDP side-band.
    MultitransportRequest,
    /// The server closed the connection.
    Disconnect,
    /// Any other I/O-channel PDU (unparsed).
    Other(Vec<u8>),
}

/// A connected RDP session.
pub struct WireSession {
    transport: WireTransport,
    settings: SessionSettings,
    security: Option<security::SecurityLayer>,
}

impl WireSession {
    /// Open the TCP connection and run the full RDP connection sequence.
    /// Returns a ready session on success.
    pub fn connect(opts: ConnectOptions) -> WireResult<WireSession> {
        let transport_opts = TransportOptions {
            host: opts.host.clone(),
            port: opts.port,
            insecure: opts.insecure,
            udp: opts.udp,
            ..Default::default()
        };
        let mut transport = WireTransport::connect(transport_opts)?;
        let handshake = handshake::run(&mut transport, &opts)?;
        Ok(WireSession {
            transport,
            settings: handshake.settings,
            security: Some(handshake.security),
        })
    }

    /// The negotiated session settings.
    pub fn settings(&self) -> &SessionSettings {
        &self.settings
    }

    /// Whether the underlying transport uses Standard RDP Security.
    pub fn insecure(&self) -> bool {
        self.transport.insecure()
    }

    /// Switch the session into poll mode: [`Self::recv`] then returns
    /// `Io(TimedOut)` after `timeout` of silence instead of blocking for the
    /// connection-level timeout. The UI controller uses this to drive the
    /// session from its message loop.
    pub fn set_poll_timeout(&mut self, timeout: std::time::Duration) -> WireResult<()> {
        self.transport.set_read_timeout(Some(timeout))
    }

    /// Send keyboard/mouse/other input events as a slow-path TS_INPUT_PDU.
    pub fn send_input(&mut self, events: &[InputEvent]) -> WireResult<()> {
        let pdu = crate::pdu::encode_input_pdu(
            self.settings.share_id,
            self.settings.user_channel,
            events,
        );
        self.send_io_pdu(&pdu)
    }

    /// Send a payload on a static virtual channel by name.
    pub fn send_channel_data(&mut self, name: &str, payload: &[u8]) -> WireResult<()> {
        let channel = self
            .settings
            .static_channels
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| WireError::Sequence(format!("channel `{name}` not joined")))?;
        self.send_channel_data_on(channel.id, payload)
    }

    /// Send a payload on a static virtual channel by id. The payload is sealed
    /// with the session keys (the channels are declared ENCRYPT_RDP) and
    /// prefixed with its 4-byte length, per the Virtual Channel PDU format.
    pub fn send_channel_data_on(&mut self, channel_id: u16, payload: &[u8]) -> WireResult<()> {
        let security = self
            .security
            .as_mut()
            .ok_or_else(|| WireError::Sequence("security layer not initialized".into()))?;
        let mut sealed = Vec::new();
        security::write_basic_security_header(security::SEC_ENCRYPT, &mut sealed);
        sealed.extend(security.seal(payload));

        let mut pdu = Vec::with_capacity(4 + sealed.len());
        pdu.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        pdu.extend_from_slice(&sealed);
        handshake::send_mcs_raw(
            &mut self.transport,
            self.settings.user_channel,
            channel_id,
            &pdu,
        )
    }

    /// Send an encrypted slow-path PDU on the I/O channel.
    fn send_io_pdu(&mut self, pdu: &[u8]) -> WireResult<()> {
        let security = self
            .security
            .as_mut()
            .ok_or_else(|| WireError::Sequence("security layer not initialized".into()))?;
        handshake::send_encrypted(
            &mut self.transport,
            security,
            self.settings.user_channel,
            self.settings.io_channel,
            pdu,
        )
    }

    /// Receive and classify the next server PDU.
    pub fn recv(&mut self) -> WireResult<ServerPdu> {
        let frame = self.transport.recv()?;
        match frame.first() {
            // Fast-path PDU: the first byte's high bit is set and it is not
            // an X.224 Data header.
            Some(&0x02) => self.recv_slow_path(&frame),
            Some(b) if b & 0x80 != 0 => Ok(ServerPdu::GraphicsUpdate(frame)),
            _ => Err(WireError::Protocol("unrecognized PDU framing".into())),
        }
    }

    fn recv_slow_path(&mut self, frame: &[u8]) -> WireResult<ServerPdu> {
        let mcs_pdu = mcs::parse_send_data_indication(frame)?;
        if mcs_pdu.channel_id != self.settings.io_channel {
            // Static channel data: strip the security header if the payload
            // carries one, otherwise hand it over raw.
            let data = self.open_channel_payload(&mcs_pdu.data)?;
            return Ok(ServerPdu::ChannelData {
                channel_id: mcs_pdu.channel_id,
                data,
            });
        }

        let mut payload = &mcs_pdu.data[..];
        let flags = security::read_basic_security_header(&mut payload)?;
        if flags & security::SEC_AUTODETECT_REQ != 0 {
            return Ok(ServerPdu::MultitransportRequest);
        }
        let plain = if flags & security::SEC_ENCRYPT != 0 {
            let sec = self
                .security
                .as_mut()
                .ok_or_else(|| WireError::Sequence("security layer not initialized".into()))?;
            sec.open(payload)?
        } else {
            payload.to_vec()
        };

        let mut cur = &plain[..];
        let (pdu_type, _source) = caps::read_share_control_header(&mut cur)?;
        if pdu_type != caps::PDUTYPE_DATA {
            return Ok(ServerPdu::Other(plain));
        }
        let (_share_id, pdu_type2) = caps::read_share_data_header(&mut cur)?;
        match pdu_type2 {
            caps::PDUTYPE2_UPDATE => Ok(ServerPdu::Update(cur.to_vec())),
            _ => Ok(ServerPdu::Other(plain)),
        }
    }

    fn open_channel_payload(&self, data: &[u8]) -> WireResult<Vec<u8>> {
        if data.len() >= 4 {
            let flags = u16::from_le_bytes([data[0], data[1]]);
            if flags & security::SEC_ENCRYPT != 0 {
                let sec = self
                    .security
                    .as_ref()
                    .ok_or_else(|| WireError::Sequence("security layer not initialized".into()))?;
                return sec.clone().open(&data[4..]);
            }
        }
        Ok(data.to_vec())
    }

    /// Gracefully close the session.
    pub fn close(&mut self) {
        self.transport.close();
    }

    /// The underlying transport (for the UDP side-band etc.).
    pub fn transport(&mut self) -> &mut WireTransport {
        &mut self.transport
    }
}

impl Drop for WireSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Convenience: length-prefix `payload` as a Virtual Channel PDU body
/// (used by tests and the redirection modules).
pub fn virtual_channel_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Strip the 4-byte length prefix from a Virtual Channel PDU body.
pub fn virtual_channel_payload(frame: &[u8]) -> WireResult<&[u8]> {
    if frame.len() < 4 {
        return Err(WireError::Protocol("short virtual channel frame".into()));
    }
    let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    frame
        .get(4..4 + len)
        .ok_or_else(|| WireError::Protocol("virtual channel frame length mismatch".into()))
}

// Silence an unused import warning for x224 (used by re-export consumers).
#[allow(unused_imports)]
use x224 as _x224;
