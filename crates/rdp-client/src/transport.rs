//! TCP transport, the X.224 negotiation step, and the legacy security fallback.
//!
//! Cross-platform on purpose: it's plain `std::net`, so the connect + negotiate
//! path compiles and is unit-tested on any host (the TLS upgrade and CredSSP
//! that follow are the Windows-bound pieces, added in later milestones).

use std::io::{self, Read, Write};
use std::net::TcpStream;

use rdp_core::{ClientConfig, Connector};
use rdp_pdu::x224::{self, NegFailureCode, SecurityProtocol};

/// Errors from the connect/negotiation phase.
#[derive(Debug, thiserror::Error)]
pub enum NegotiateError {
    #[error("network error: {0}")]
    Io(#[from] io::Error),
    #[error("server rejected the security negotiation: {0:?}")]
    Rejected(NegFailureCode),
    #[error("{0}")]
    Protocol(String),
}

/// Read exactly one TPKT-framed PDU (4-byte header + body) from `reader`.
pub fn read_tpkt_pdu<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; x224::TPKT_HEADER_LEN];
    reader.read_exact(&mut header)?;
    let total = x224::read_tpkt_len(&header)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if total < header.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TPKT length smaller than its header",
        ));
    }
    let mut pdu = vec![0u8; total];
    pdu[..header.len()].copy_from_slice(&header);
    reader.read_exact(&mut pdu[header.len()..])?;
    Ok(pdu)
}

/// Send the X.224 Connection Request and process the Connection Confirm,
/// returning the protocol the server selected.
pub fn negotiate<S: Read + Write>(
    stream: &mut S,
    connector: &mut Connector,
) -> Result<SecurityProtocol, NegotiateError> {
    let request = connector.initial_request();
    stream.write_all(&request)?;
    stream.flush()?;

    let confirm = read_tpkt_pdu(stream)?;
    connector
        .handle_negotiation_response(&confirm)
        .map_err(|e| match e {
            rdp_core::CoreError::NegotiationRejected(code) => NegotiateError::Rejected(code),
            other => NegotiateError::Protocol(other.to_string()),
        })
}

/// Connect to `config` advertising `protocols` and complete X.224 negotiation.
fn connect_with(
    config: &ClientConfig,
    protocols: SecurityProtocol,
) -> Result<(TcpStream, Connector, SecurityProtocol), NegotiateError> {
    let addr = format!("{}:{}", config.hostname, config.port);
    let mut stream = TcpStream::connect(&addr)?;
    stream.set_nodelay(true).ok();

    let mut connector = Connector::new(config.clone());
    connector.set_requested_protocols(protocols);
    let selected = negotiate(&mut stream, &mut connector)?;
    Ok((stream, connector, selected))
}

/// Connect and negotiate, advertising TLS + CredSSP. If the server refuses TLS
/// outright (`SSL_NOT_ALLOWED_BY_SERVER`) and `allow_legacy_fallback` is set,
/// transparently reconnect requesting legacy Standard RDP Security.
pub fn connect(
    config: &ClientConfig,
) -> Result<(TcpStream, Connector, SecurityProtocol), NegotiateError> {
    match connect_with(config, SecurityProtocol::SSL | SecurityProtocol::HYBRID) {
        Err(NegotiateError::Rejected(NegFailureCode::SslNotAllowedByServer))
            if config.allow_legacy_fallback =>
        {
            tracing::warn!(
                "server refused TLS/NLA (SSL_NOT_ALLOWED_BY_SERVER); falling back to legacy \
                 Standard RDP Security — this path is weakly encrypted (RC4) and has no GPU/H.264"
            );
            connect_with(config, SecurityProtocol::empty())
        }
        other => other,
    }
}

/// Reconnect requesting only legacy Standard RDP Security (`PROTOCOL_RDP`).
/// Used as a runtime fallback when the TLS/NLA upgrade fails on a server that
/// also allows the legacy path. (Only the Windows connect path invokes this.)
#[cfg(windows)]
pub fn connect_legacy(
    config: &ClientConfig,
) -> Result<(TcpStream, Connector, SecurityProtocol), NegotiateError> {
    connect_with(config, SecurityProtocol::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::thread;

    // X.224 Connection Confirm selecting PROTOCOL_HYBRID (NLA).
    const CC_HYBRID: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00,
        0x02, 0x00, 0x00, 0x00,
    ];
    // RDP_NEG_FAILURE, failureCode = 2 (SSL_NOT_ALLOWED_BY_SERVER).
    const CC_FAIL_SSL_NOT_ALLOWED: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00,
        0x02, 0x00, 0x00, 0x00,
    ];
    // RDP_NEG_RSP selecting PROTOCOL_RDP (0) — Standard RDP Security.
    const CC_RSP_RDP: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x08, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    fn config_for(addr: std::net::SocketAddr) -> ClientConfig {
        ClientConfig {
            hostname: addr.ip().to_string(),
            port: addr.port(),
            ..Default::default()
        }
    }

    #[test]
    fn read_tpkt_pdu_reads_exactly_one_frame() {
        let frame = [0x03u8, 0x00, 0x00, 0x07, 0x02, 0xf0, 0x80];
        let mut data = frame.to_vec();
        data.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let mut cur = Cursor::new(data);
        assert_eq!(read_tpkt_pdu(&mut cur).unwrap(), frame);
    }

    #[test]
    fn negotiate_over_loopback_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _cr = read_tpkt_pdu(&mut sock).unwrap();
            sock.write_all(&CC_HYBRID).unwrap();
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        let mut connector = Connector::new(ClientConfig::default());
        let protocol = negotiate(&mut stream, &mut connector).unwrap();
        assert_eq!(protocol, SecurityProtocol::HYBRID);
        server.join().unwrap();
    }

    #[test]
    fn connect_falls_back_to_standard_security() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            // First attempt: client asks for SSL|HYBRID; server refuses TLS.
            let (mut s1, _) = listener.accept().unwrap();
            let _cr1 = read_tpkt_pdu(&mut s1).unwrap();
            s1.write_all(&CC_FAIL_SSL_NOT_ALLOWED).unwrap();

            // Fallback attempt: client must now request PROTOCOL_RDP (0).
            let (mut s2, _) = listener.accept().unwrap();
            let cr2 = read_tpkt_pdu(&mut s2).unwrap();
            assert_eq!(&cr2[cr2.len() - 4..], &[0, 0, 0, 0]);
            s2.write_all(&CC_RSP_RDP).unwrap();
        });

        let (_stream, _connector, protocol) = connect(&config_for(addr)).unwrap();
        assert_eq!(protocol, SecurityProtocol::empty()); // Standard RDP Security
        server.join().unwrap();
    }

    #[test]
    fn fallback_disabled_surfaces_the_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            let _cr1 = read_tpkt_pdu(&mut s1).unwrap();
            s1.write_all(&CC_FAIL_SSL_NOT_ALLOWED).unwrap();
        });

        let mut config = config_for(addr);
        config.allow_legacy_fallback = false;
        let err = connect(&config).unwrap_err();
        assert!(matches!(
            err,
            NegotiateError::Rejected(NegFailureCode::SslNotAllowedByServer)
        ));
        server.join().unwrap();
    }
}
