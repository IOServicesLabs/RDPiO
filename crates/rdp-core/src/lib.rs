//! The sans-I/O RDP connection state machine.
//!
//! [`Connector`] owns *only* protocol state. The driver in `rdp-client` owns
//! the socket and TLS, feeds received bytes in, and writes the bytes the
//! connector asks it to send. This separation keeps the entire connection
//! sequence testable without a network.

#![forbid(unsafe_code)]

use rdp_pdu::x224::{ConnectionConfirm, ConnectionRequest, NegRequestFlags, SecurityProtocol};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("protocol error: {0}")]
    Pdu(#[from] rdp_pdu::PduError),
    #[error("connection sequence error: {0}")]
    Sequence(String),
    #[error("server rejected the security negotiation: {0:?}")]
    NegotiationRejected(rdp_pdu::x224::NegFailureCode),
}

/// Reverse Connect parameters for Windows 365 / Azure Virtual Desktop.
///
/// When present, the client connects to the gateway FQDN over a secure
/// WebSocket and tunnels the RDP session through it.
#[derive(Debug, Clone, Default)]
pub struct ReverseConnectConfig {
    /// Gateway host (`gatewayhostname` from the `.rdp`, may include `:443`) that
    /// the stage-1 ARM brokering `POST /api/arm/v2/connections` is sent to.
    pub gateway_fqdn: String,
    pub resource_id: String,
    pub tenant_id: String,
    pub session_id: String,
    /// OAuth2 access token used to authenticate the WebSocket upgrade and the
    /// RDP Client Info PDU.
    pub access_token: String,
    /// Opaque `loadbalanceinfo` token (e.g. `mth://localhost/<id>/<pool>`) from
    /// the `.rdp`, replayed verbatim in the ARM brokering request so the gateway
    /// routes to the right host pool. Empty disables ARM brokering.
    pub load_balance_info: String,
    /// Client application name sent in the ARM brokering request body.
    pub application_name: String,
    /// The `remoteapplicationprogram` from the `.rdp` (e.g. `||<resourceId>`),
    /// sent verbatim as the `application` field of the ARM brokering request.
    /// The gateway dereferences this to resolve the target Cloud PC, so it must
    /// be present and exact (an empty/wrong value yields a 400 NullReference).
    pub remote_application: String,
    /// The user's real (short) logon password for the RDSTLS v3 credential.
    /// AVD/W365 RDSTLS encrypts this with the broker's AES key + the target's
    /// public key (see `rdstls_v3`); it must be the actual account password, NOT
    /// the OAuth access token (which is far too large for the RSA block).
    pub rdstls_password: String,
    /// Logon domain for the RDSTLS v3 credential. Pure Entra/AAD Cloud PCs expect
    /// `"AzureAD"` (the default when `--domain` is not given); hybrid/AD-joined
    /// hosts may need the AD domain (FQDN or NetBIOS) or an empty string (when the
    /// UPN username is self-qualifying).
    pub rdstls_domain: String,
}

/// User-supplied connection parameters.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub hostname: String,
    pub port: u16,
    pub width: u16,
    pub height: u16,
    pub credentials: Credentials,
    /// Fall back to legacy Standard RDP Security if the server refuses TLS/NLA.
    pub allow_legacy_fallback: bool,
    /// Accept an unvalidated/self-signed TLS server certificate. Default `false`
    /// (the chain is validated against the OS trust store). RDP hosts commonly
    /// present self-signed certs, so connecting to those requires opting in.
    pub allow_invalid_certificate: bool,
    /// Local directories/drive roots to share into the session as redirected
    /// drives (rdpdr) — one device per entry (mapped network drives included;
    /// any openable root works). Empty disables drive redirection.
    pub drive_paths: Vec<String>,
    /// Client monitor layout for a spanned multi-monitor desktop. Empty = single
    /// monitor (the `width`/`height` desktop size is used as-is).
    pub monitors: Vec<rdp_pdu::gcc::MonitorDef>,
    /// Skip the TLS/NLA negotiation and connect with legacy Standard RDP
    /// Security directly. Useful for older hosts that only speak it.
    pub force_legacy: bool,
    /// Override the keyboard layout id (e.g. 0x0409 US, 0x0809 UK, 0x0407 DE);
    /// `None` uses the client default. Matters for non-US keyboards.
    pub keyboard_layout: Option<u32>,
    /// Override the session color depth in bits (16/24/32); `None` = default.
    pub color_depth: Option<u16>,
    /// Enable RemoteFX Progressive (RFX Pro) codec support.
    pub enable_rfx: bool,
    /// Opaque load-balance cookie (e.g. AVD broker redirection token) to replay
    /// in the Client Info PDU.
    pub load_balance_info: Option<Vec<u8>>,
    /// Server redirection session id, replayed in the Client Info PDU extended
    /// info and GCC cluster data.
    pub redirected_session_id: Option<u32>,
    /// Reverse Connect configuration for W365/AVD. When `Some`, the client
    /// connects over a WebSocket to `gateway_fqdn` rather than TCP to
    /// `hostname:port`.
    pub reverse_connect: Option<ReverseConnectConfig>,
    /// After the control channel is established, attempt a direct UDP 3390
    /// Shortpath tunnel for graphics/audio.
    pub shortpath: bool,
    /// Advertise the `TS_UD_CS_MULTITRANSPORT` GCC block so the server offers a
    /// side-band UDP transport (RDP multipathing). Only meaningful with a direct
    /// `hostname`/`port` to dial the UDP socket to; Reverse Connect (W365/AVD)
    /// has no direct UDP path and needs Shortpath instead, so this stays `false`
    /// there to avoid soliciting a multitransport request we can't fulfil.
    pub multitransport: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            port: 3389,
            width: 1920,
            height: 1080,
            credentials: Credentials::default(),
            allow_legacy_fallback: true,
            allow_invalid_certificate: false,
            drive_paths: Vec::new(),
            monitors: Vec::new(),
            force_legacy: false,
            keyboard_layout: None,
            color_depth: None,
            enable_rfx: false,
            load_balance_info: None,
            redirected_session_id: None,
            reverse_connect: None,
            shortpath: false,
            multitransport: false,
        }
    }
}

#[derive(Clone, Default)]
pub struct Credentials {
    pub domain: String,
    pub username: String,
    pub password: String,
}

// Never print the password.
impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credentials")
            .field("domain", &self.domain)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Split a Windows-style logon name into `(domain, user)`, the way the mstsc
/// credential prompt does. RDP/NTLM expect the domain and user in separate
/// fields, so a name typed as `DOMAIN\user` or `.\user` must be split before it
/// reaches the SSPI identity — otherwise the server looks up a user literally
/// named `DOMAIN\user` and returns `STATUS_LOGON_FAILURE` (0xC000006D).
///
/// Rules:
/// - `DOMAIN\user` → (`DOMAIN`, `user`). The down-level `.` ("this machine")
///   maps to an empty domain, which Windows NTLM resolves against the target's
///   *local* accounts — the right behaviour for a local account on the host.
/// - `user@domain` (UPN) → kept whole as the user with an empty domain; the
///   Negotiate/Kerberos provider parses the UPN itself.
/// - bare `user` → paired with the explicitly supplied `domain` (possibly empty).
///
/// A `\` in the username always wins over an explicit `domain` argument, matching
/// how typing `CORP\alice` overrides a separate domain box.
pub fn split_domain_user(domain: &str, username: &str) -> (String, String) {
    if let Some((dom, user)) = username.split_once('\\') {
        let dom = if dom == "." { "" } else { dom };
        (dom.to_string(), user.to_string())
    } else {
        (domain.to_string(), username.to_string())
    }
}

/// Where we are in the RDP connection sequence (MS-RDPBCGR 1.3.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// X.224 Connection Request/Confirm and security-protocol selection.
    Negotiation,
    /// CredSSP / NLA (the token loop is driven by `rdp-nla`).
    Authentication,
    /// MCS Connect-Initial/Response carrying the GCC conference data.
    BasicSettings,
    /// Erect-Domain, Attach-User, and per-channel Channel-Join.
    ChannelJoin,
    /// License exchange (usually a "no license required" response).
    Licensing,
    /// Demand-Active / Confirm-Active capability negotiation.
    CapabilityExchange,
    /// Synchronize, Control (cooperate/request), Font List/Map.
    Finalization,
    /// Session is up; fast-path output and EGFX commands flow.
    Active,
}

/// Drives the RDP connection sequence over an already-connected transport.
#[derive(Debug)]
pub struct Connector {
    config: ClientConfig,
    phase: Phase,
    selected_protocol: SecurityProtocol,
    requested_protocols: SecurityProtocol,
}

impl Connector {
    pub fn new(config: ClientConfig) -> Self {
        // `--legacy` advertises Standard RDP Security (PROTOCOL_RDP) only.
        let requested_protocols = if config.force_legacy {
            SecurityProtocol::empty()
        } else {
            SecurityProtocol::SSL | SecurityProtocol::HYBRID
        };
        Self {
            config,
            phase: Phase::Negotiation,
            selected_protocol: SecurityProtocol::empty(),
            requested_protocols,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The protocol the server selected during negotiation (valid once the
    /// negotiation response has been processed).
    pub fn selected_protocol(&self) -> SecurityProtocol {
        self.selected_protocol
    }

    /// Protocols advertised in the next Connection Request.
    pub fn requested_protocols(&self) -> SecurityProtocol {
        self.requested_protocols
    }

    /// Change the advertised protocols and restart the negotiation phase (used
    /// for the legacy Standard RDP Security fallback).
    pub fn set_requested_protocols(&mut self, protocols: SecurityProtocol) {
        self.requested_protocols = protocols;
        self.phase = Phase::Negotiation;
    }

    /// The first bytes to send: an X.224 Connection Request advertising the
    /// currently-requested protocols. Includes the `mstshash` cookie when a
    /// username is set.
    pub fn initial_request(&self) -> Vec<u8> {
        let cookie = if self.config.credentials.username.is_empty() {
            None
        } else {
            Some(self.config.credentials.username.clone())
        };
        let request = ConnectionRequest {
            requested_protocols: self.requested_protocols,
            flags: NegRequestFlags::empty(),
            cookie,
        };
        // Encoding only fails on absurdly large cookies, which we never build.
        request.to_bytes().expect("connection request encodes")
    }

    /// Process the server's X.224 Connection Confirm and advance the state
    /// machine. Returns the selected security protocol.
    pub fn handle_negotiation_response(
        &mut self,
        data: &[u8],
    ) -> Result<SecurityProtocol, CoreError> {
        let mut cursor = data;
        match ConnectionConfirm::decode(&mut cursor)? {
            ConnectionConfirm::Response {
                selected_protocol, ..
            } => {
                self.selected_protocol = selected_protocol;
                // With TLS/NLA the next step is the security upgrade + auth;
                // Standard RDP Security would jump straight to MCS.
                self.phase = if selected_protocol.is_empty() {
                    Phase::BasicSettings
                } else {
                    Phase::Authentication
                };
                Ok(selected_protocol)
            }
            ConnectionConfirm::Failure { failure_code } => {
                Err(CoreError::NegotiationRejected(failure_code))
            }
            ConnectionConfirm::NoNegotiation => {
                self.selected_protocol = SecurityProtocol::empty();
                self.phase = Phase::BasicSettings;
                Ok(SecurityProtocol::empty())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_user(user: &str) -> ClientConfig {
        ClientConfig {
            hostname: "host".into(),
            credentials: Credentials {
                username: user.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn starts_in_negotiation() {
        let c = Connector::new(ClientConfig::default());
        assert_eq!(c.phase(), Phase::Negotiation);
    }

    #[test]
    fn split_domain_user_handles_windows_logon_forms() {
        // Down-level DOMAIN\user.
        assert_eq!(
            split_domain_user("", "CORP\\alice"),
            ("CORP".into(), "alice".into())
        );
        // Local-machine `.\user` → empty domain (local account lookup).
        assert_eq!(
            split_domain_user("", ".\\carol"),
            (String::new(), "carol".into())
        );
        // A `\` prefix overrides an explicit domain argument.
        assert_eq!(
            split_domain_user("IGNORED", "CORP\\bob"),
            ("CORP".into(), "bob".into())
        );
        // UPN is left whole with an empty domain.
        assert_eq!(
            split_domain_user("", "alice@corp.com"),
            (String::new(), "alice@corp.com".into())
        );
        // Bare user keeps the explicit domain.
        assert_eq!(
            split_domain_user("CORP", "alice"),
            ("CORP".into(), "alice".into())
        );
        // Bare user, no domain.
        assert_eq!(
            split_domain_user("", "carol"),
            (String::new(), "carol".into())
        );
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let creds = Credentials {
            domain: "CORP".into(),
            username: "alice".into(),
            password: "hunter2".into(),
        };
        let printed = format!("{creds:?}");
        assert!(printed.contains("alice"));
        assert!(!printed.contains("hunter2"));
    }

    #[test]
    fn initial_request_is_a_valid_tpkt_advertising_nla() {
        let c = Connector::new(config_with_user(""));
        let bytes = c.initial_request();
        assert_eq!(rdp_pdu::x224::read_tpkt_len(&bytes).unwrap(), bytes.len());
        let proto = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
        assert_eq!(proto, 0x03); // SSL | HYBRID
    }

    #[test]
    fn initial_request_includes_cookie_when_user_set() {
        let c = Connector::new(config_with_user("alice"));
        let bytes = c.initial_request();
        assert!(String::from_utf8_lossy(&bytes).contains("Cookie: mstshash=alice"));
    }

    #[test]
    fn legacy_fallback_advertises_standard_security() {
        let mut c = Connector::new(ClientConfig::default());
        c.set_requested_protocols(SecurityProtocol::empty());
        let bytes = c.initial_request();
        let proto = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
        assert_eq!(proto, 0); // PROTOCOL_RDP (Standard RDP Security)
        assert_eq!(c.phase(), Phase::Negotiation);
    }

    #[test]
    fn negotiation_response_selecting_hybrid_advances_to_auth() {
        let mut c = Connector::new(ClientConfig::default());
        let cc = [
            0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x08,
            0x00, 0x02, 0x00, 0x00, 0x00,
        ];
        let proto = c.handle_negotiation_response(&cc).unwrap();
        assert_eq!(proto, SecurityProtocol::HYBRID);
        assert_eq!(c.phase(), Phase::Authentication);
    }

    #[test]
    fn negotiation_failure_is_reported() {
        let mut c = Connector::new(ClientConfig::default());
        let cc = [
            0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x08,
            0x00, 0x05, 0x00, 0x00, 0x00,
        ];
        assert!(matches!(
            c.handle_negotiation_response(&cc),
            Err(CoreError::NegotiationRejected(_))
        ));
    }
}
