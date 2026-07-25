//! Connection establishment for the Windows client: negotiate, optionally
//! upgrade to TLS + CredSSP/NLA, and activate — choosing the security path the
//! server selected, with a runtime fallback to legacy Standard RDP Security.
//!
//! The post-activation session loop is identical for both paths because they
//! share one [`Transport`] (a `TcpStream` for the legacy path, a TLS-wrapped
//! one for the modern path), so `session::activate` / `run_session` are unaware
//! of which security underlay is in use.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rdp_core::ClientConfig;
use rdp_pdu::x224::SecurityProtocol;

use crate::reverse_connect::ReverseConnectStream;
use crate::session::{self, ActiveSession};
use crate::tls::TlsStream;
use crate::transport;

/// Bound activation reads so a silent server cannot hang startup.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);

/// The byte stream under the RDP session: plaintext TCP (legacy Standard RDP
/// Security), a Schannel TLS tunnel (TLS / NLA), or a TLS-secured WebSocket
/// (W365/AVD Reverse Connect). Implements `Read`/`Write` by delegation so the
/// session layer is transport-agnostic.
pub enum Transport {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    WebSocket(Box<ReverseConnectStream>),
    /// W365/AVD Reverse Connect with an inner TLS session (RDSTLS): the RDP
    /// stream runs over a TLS handshake with the target host, tunneled through
    /// the gateway WebSocket.
    WebSocketTls(Box<TlsStream<ReverseConnectStream>>),
}

impl Transport {
    /// Set the read timeout on the underlying socket. The TLS/WebSocket paths
    /// use this so the worker can wake periodically to flush queued client input
    /// between blocking reads; the legacy path carries input on a separate
    /// socket clone instead, so it leaves reads fully blocking.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.set_read_timeout(dur),
            Transport::Tls(s) => s.get_ref().set_read_timeout(dur),
            // Plumb the timeout down to the WebSocket's TCP socket so the graphics
            // loop can flush queued input between reads instead of blocking until
            // the server sends a frame (otherwise clicks feel sluggish on a static
            // desktop). `get_ref()` reaches the ReverseConnectStream through the
            // inner RDSTLS TLS session.
            Transport::WebSocket(s) => s.set_read_timeout(dur),
            Transport::WebSocketTls(s) => s.get_ref().set_read_timeout(dur),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
            Transport::WebSocket(s) => s.read(buf),
            Transport::WebSocketTls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
            Transport::WebSocket(s) => s.write(buf),
            Transport::WebSocketTls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
            Transport::WebSocket(s) => s.flush(),
            Transport::WebSocketTls(s) => s.flush(),
        }
    }
}

/// A connected, activated session ready to stream.
pub struct Established {
    pub transport: Transport,
    pub session: ActiveSession,
    /// A clone of the underlying TCP socket for clearing the read timeout and
    /// shutting the worker down on window close (clones share the socket).
    pub control: Option<TcpStream>,
    /// A second TCP clone for the input sender — present only on the legacy
    /// path, where the plaintext socket can be cloned so the UI thread writes
    /// input directly. On the TLS path this is `None` because a single SChannel
    /// context can't be split across threads; input is instead queued to the
    /// session worker (which owns the tunnel) over an mpsc channel and sent from
    /// there. Both paths are fully interactive.
    pub input_tcp: Option<TcpStream>,
    pub protocol: SecurityProtocol,
}

/// Connect to `config`, pick the security path the server negotiated, and
/// activate. Falls back to legacy Standard RDP Security if the TLS/NLA upgrade
/// fails on a server that also allows it. Pass `reconnect` to resume a prior
/// session without a fresh logon.
///
/// `config` is mutable so a server redirection PDU can update the target host,
/// port, load-balance cookie, and redirected session id before retrying.
pub fn establish_reconnect(
    config: &mut ClientConfig,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<Established, Box<dyn std::error::Error>> {
    const MAX_REDIRECTS: u32 = 3;
    for redirect_count in 0..=MAX_REDIRECTS {
        match try_establish(config, reconnect) {
            Ok(established) => return Ok(established),
            Err(e) => {
                if let Some(session::ActivateError::Redirect(r)) =
                    e.downcast_ref::<session::ActivateError>()
                {
                    if redirect_count == MAX_REDIRECTS {
                        return Err("server redirected too many times".into());
                    }
                    apply_redirection(config, r);
                    tracing::info!(
                        host = %config.hostname,
                        port = config.port,
                        redirect_count = redirect_count + 1,
                        "following server redirection"
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err("server redirected too many times".into())
}

fn try_establish(
    config: &ClientConfig,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<Established, Box<dyn std::error::Error>> {
    if let Some(rc) = config.reverse_connect.as_ref() {
        return try_establish_reverse_connect(rc, config, reconnect);
    }

    tracing::info!(host = %config.hostname, port = config.port, "connecting over TCP");
    let (tcp, _connector, protocol) = transport::connect(config)?;
    tracing::info!(?protocol, "X.224 negotiation complete");

    if protocol.contains(SecurityProtocol::SSL) || protocol.contains(SecurityProtocol::HYBRID) {
        match secure(tcp, config, protocol, reconnect) {
            Ok(established) => return Ok(established),
            Err(err) if config.allow_legacy_fallback => {
                tracing::warn!(
                    error = %err,
                    "TLS/NLA path failed; retrying legacy Standard RDP Security"
                );
                let (tcp2, _c2, proto2) = transport::connect_legacy(config)?;
                return legacy(tcp2, config, proto2, reconnect);
            }
            Err(err) => return Err(err),
        }
    }
    legacy(tcp, config, protocol, reconnect)
}

/// Connect through a W365/AVD Reverse Connect WebSocket gateway.
///
/// Two stages (matching the Windows App / FreeRDP): **(1)** broker the
/// connection through the gateway's ARM endpoint to obtain the real WebSocket
/// URL plus RDSTLS auth material, then **(2)** open that WebSocket, negotiate
/// X.224, and — when the gateway selects RDSTLS — run an inner TLS handshake
/// with the target host and the RDSTLS authentication exchange before activating.
fn try_establish_reverse_connect(
    rc: &rdp_core::ReverseConnectConfig,
    config: &ClientConfig,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<Established, Box<dyn std::error::Error>> {
    // Stage 1: ARM brokering. Yields the actual WebSocket URL + RDSTLS material.
    let broker = if rc.load_balance_info.is_empty() {
        tracing::warn!(
            "Reverse Connect without loadBalanceInfo: skipping ARM brokering (manual-test fallback)"
        );
        None
    } else {
        // The gateway resolves the target Cloud PC from `application` (the
        // `.rdp`'s `remoteapplicationprogram`); without it the orchestrator 400s
        // with a NullReference.
        if rc.remote_application.is_empty() {
            tracing::warn!(
                "Reverse Connect brokering without remoteApplicationProgram; the gateway will likely reject with a 400"
            );
        }
        let b = crate::arm_broker::broker_connection(
            &rc.gateway_fqdn,
            &rc.access_token,
            &rc.remote_application,
            &rc.load_balance_info,
            config.shortpath,
        )?;
        tracing::info!(
            ws_url = ?b.websocket_url(),
            target = ?b.redirected_server_name,
            has_auth_blob = b.redirected_auth_blob.is_some(),
            has_auth_guid = b.redirected_auth_guid.is_some(),
            has_server_cert = b.redirected_server_cert.is_some(),
            "ARM /connections brokered"
        );

        // W365 Shortpath milestone 1: if the gateway offered a rendezvous
        // location, connect it on a side thread and log the nano-transport
        // signaling frames (secret-safe) to learn their wire format. Purely
        // additive — gated on `--shortpath`, never touches the RDP transport.
        if config.shortpath {
            match b.shortpath.client_rendezvous_location.clone() {
                Some(loc) => {
                    let accept_invalid = config.allow_invalid_certificate;
                    let token = rc.access_token.clone();
                    let shortpath = b.shortpath.clone();
                    std::thread::Builder::new()
                        .name("shortpath-rendezvous".into())
                        .spawn(move || {
                            crate::rendezvous::probe(
                                &loc,
                                &token,
                                &shortpath,
                                accept_invalid,
                                std::time::Duration::from_secs(30),
                            );
                        })
                        .ok();
                    tracing::info!("spawned Shortpath rendezvous probe (30s)");
                }
                None => tracing::info!("Shortpath: gateway returned no clientRendezvousLocation"),
            }
        }
        Some(b)
    };

    // Stage 2: open the (brokered) WebSocket.
    let ws_url = broker.as_ref().and_then(|b| b.websocket_url());
    let ws = ReverseConnectStream::connect(rc, ws_url, config.allow_invalid_certificate)?;
    tracing::info!("Reverse Connect WebSocket established");

    let mut connector = rdp_core::Connector::new(config.clone());
    // Reverse Connect authenticates with RDSTLS; advertise it (plus SSL, which
    // some gateway builds may select instead).
    connector.set_requested_protocols(SecurityProtocol::SSL | SecurityProtocol::RDSTLS);
    let mut ws_boxed = Box::new(ws);
    let protocol = transport::negotiate(&mut ws_boxed, &mut connector)?;
    tracing::info!(?protocol, "X.224 negotiation over WebSocket complete");

    if protocol.contains(SecurityProtocol::HYBRID) {
        // HYBRID/NLA inside Reverse Connect is not supported: the gateway TLS
        // certificate is not exposed to the RDP layer, so CredSSP public-key
        // binding cannot be computed.
        return Err(
            "HYBRID/NLA over Reverse Connect is not supported; the gateway selected an unexpected security protocol"
                .into(),
        );
    }

    if protocol.contains(SecurityProtocol::RDSTLS) {
        let broker = broker.ok_or(
            "gateway selected RDSTLS but no ARM brokering material is available (need loadBalanceInfo)",
        )?;
        // Inner TLS with the target session host, tunneled through the WebSocket.
        // The target commonly presents a cert validated out-of-band via
        // redirectedServerCert; we accept it here (pinning is a later refinement).
        let sni = broker
            .redirected_server_name
            .clone()
            .unwrap_or_else(|| rc.gateway_fqdn.clone());
        let mut tls = TlsStream::connect(*ws_boxed, &sni, true)?;
        tracing::info!(%sni, "inner TLS for RDSTLS established");

        // AVD/W365 RDSTLS credential (matches msrdc; see `rdstls_v3`):
        //  - RedirectionGuid = UTF-16LE(redirectedAuthGuid base64 string) + NUL.
        //  - UserName        = account UPN; Domain = "AzureAD" (Entra requires it).
        //  - Password        = base64+UTF16( RSA_pkcs1( targetCert,
        //                        AES256_CBC( blobKey, IV=0, UTF16(password)+NUL ) ) ).
        //    The broker's `redirectedAuthBlob` is an AES *key* to encrypt with (not
        //    a blob to relay), and `redirectedServerCert` is the target's X.509. The
        //    plaintext is the user's real logon password (`--password`).
        let guid_str = broker.redirected_auth_guid.as_deref().unwrap_or_default();
        let mut guid: Vec<u8> = guid_str.encode_utf16().flat_map(u16::to_le_bytes).collect();
        guid.extend_from_slice(&[0, 0]); // UTF-16 NUL terminator

        let aes_key = broker
            .redirected_auth_blob
            .as_deref()
            .and_then(crate::rdstls_v3::peel_b64_utf16_b64)
            .and_then(|b| crate::rdstls_v3::aes_key_from_blob(&b))
            .ok_or("RDSTLS: could not extract AES key from redirectedAuthBlob")?;

        let cert_der = broker
            .redirected_server_cert
            .as_deref()
            .and_then(crate::rdstls_v3::peel_b64_utf16_b64)
            .and_then(|c| crate::rdstls_v3::der_from_cert_container(&c))
            .ok_or("RDSTLS: could not extract target certificate from redirectedServerCert")?;

        let rdstls_user = config.credentials.username.as_str();
        let rdstls_domain = rc.rdstls_domain.as_str();
        let password =
            crate::rdstls_v3::encode_redirect_password(&cert_der, &aes_key, &rc.rdstls_password)
                .map_err(|e| format!("RDSTLS v3 password encoding failed: {e}"))?;

        tracing::info!(
            guid_field_len = guid.len(),
            aes_key_len = aes_key.len(),
            cert_der_len = cert_der.len(),
            password_field_len = password.len(),
            rdstls_password_present = !rc.rdstls_password.is_empty(),
            username = %rdstls_user,
            domain = rdstls_domain,
            "RDSTLS v3 credential (AES+RSA encrypted password, domain=AzureAD)"
        );

        crate::rdstls_auth::authenticate(&mut tls, &guid, rdstls_user, rdstls_domain, &password)?;

        let mut transport = Transport::WebSocketTls(Box::new(tls));
        let session = session::activate(&mut transport, config, protocol, reconnect)?;
        tracing::info!("activated over Reverse Connect (RDSTLS)");
        return Ok(Established {
            transport,
            session,
            control: None,
            input_tcp: None,
            protocol,
        });
    }

    // SSL (or Standard) path: activate directly over the WebSocket.
    let mut transport = Transport::WebSocket(ws_boxed);
    let session = session::activate(&mut transport, config, protocol, reconnect)?;
    tracing::info!("activated over Reverse Connect");
    Ok(Established {
        transport,
        session,
        control: None,
        input_tcp: None,
        protocol,
    })
}

/// Apply a server redirection descriptor to `config` so the next connection
/// attempt targets the assigned session host and replays the broker token.
fn apply_redirection(
    config: &mut ClientConfig,
    redir: &rdp_pdu::redirection::ServerRedirection,
) {
    if let Some(addr) = redir
        .target_net_address
        .as_ref()
        .or(redir.target_fqdn.as_ref())
        .or(redir.target_netbios_name.as_ref())
    {
        if let Some((host, port_str)) = addr.rsplit_once(':') {
            config.hostname = host.to_string();
            if let Ok(p) = port_str.parse::<u16>() {
                config.port = p;
            }
        } else {
            config.hostname = addr.to_string();
        }
    }
    if !redir.load_balance_info.is_empty() {
        config.load_balance_info = Some(redir.load_balance_info.clone());
    }
    config.redirected_session_id = Some(redir.session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_redirection_updates_host_and_cookie() {
        let mut config = ClientConfig {
            hostname: "broker.example".into(),
            port: 3389,
            ..Default::default()
        };
        let redir = rdp_pdu::redirection::ServerRedirection {
            session_id: 0x1234_5678,
            target_net_address: Some("192.0.2.42:3390".into()),
            load_balance_info: b"Cookie: msts=token".to_vec(),
            ..Default::default()
        };
        apply_redirection(&mut config, &redir);
        assert_eq!(config.hostname, "192.0.2.42");
        assert_eq!(config.port, 3390);
        assert_eq!(config.load_balance_info, Some(b"Cookie: msts=token".to_vec()));
        assert_eq!(config.redirected_session_id, Some(0x1234_5678));
    }

    #[test]
    fn apply_redirection_falls_back_to_fqdn_without_port() {
        let mut config = ClientConfig {
            hostname: "broker".into(),
            port: 3389,
            ..Default::default()
        };
        let redir = rdp_pdu::redirection::ServerRedirection {
            target_fqdn: Some("sessionhost.example".into()),
            ..Default::default()
        };
        apply_redirection(&mut config, &redir);
        assert_eq!(config.hostname, "sessionhost.example");
        assert_eq!(config.port, 3389); // unchanged
    }
}

/// Activate over plaintext TCP (Standard RDP Security; RC4 lives in the session
/// layer). Input is supported because the TCP socket can be cloned per thread.
fn legacy(
    tcp: TcpStream,
    config: &ClientConfig,
    protocol: SecurityProtocol,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<Established, Box<dyn std::error::Error>> {
    let control = tcp.try_clone().ok();
    let input_tcp = tcp.try_clone().ok();
    if let Some(c) = &control {
        c.set_read_timeout(Some(ACTIVATION_TIMEOUT)).ok();
    }
    let mut transport = Transport::Tcp(tcp);
    let session = session::activate(&mut transport, config, protocol, reconnect)?;
    if let Some(c) = &control {
        c.set_read_timeout(None).ok();
    }
    tracing::info!("activated over Standard RDP Security");
    Ok(Established {
        transport,
        session,
        control,
        input_tcp,
        protocol,
    })
}

/// Upgrade to TLS (Schannel), run CredSSP/NLA when the server requires it
/// (HYBRID), then activate over the tunnel. Input is carried on the session
/// worker's mpsc channel (no socket clone — see [`Established::input_tcp`]),
/// so the TLS path is fully interactive.
fn secure(
    tcp: TcpStream,
    config: &ClientConfig,
    protocol: SecurityProtocol,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<Established, Box<dyn std::error::Error>> {
    let control = tcp.try_clone().ok();
    if let Some(c) = &control {
        c.set_read_timeout(Some(ACTIVATION_TIMEOUT)).ok();
    }

    let mut tls = TlsStream::connect(tcp, &config.hostname, config.allow_invalid_certificate)?;
    tracing::info!("TLS established");

    if protocol.contains(SecurityProtocol::HYBRID) {
        let cert = tls
            .remote_cert_der()
            .ok_or("server presented no certificate for NLA")?;
        let spn = format!("TERMSRV/{}", config.hostname);
        rdp_nla::sspi::authenticate(
            &mut tls,
            &spn,
            &cert,
            &config.credentials.domain,
            &config.credentials.username,
            &config.credentials.password,
        )?;
        tracing::info!("CredSSP/NLA complete");
    }

    let mut transport = Transport::Tls(Box::new(tls));
    let session = session::activate(&mut transport, config, protocol, reconnect)?;
    if let Some(c) = &control {
        c.set_read_timeout(None).ok();
    }
    tracing::info!("activated over TLS/NLA");
    Ok(Established {
        transport,
        session,
        control,
        input_tcp: None,
        protocol,
    })
}
