//! RDSTLS authentication driver — the I/O loop around the [`rdp_pdu::rdstls`]
//! codec, run over the inner TLS session of a W365/AVD Reverse Connect once the
//! X.224 negotiation has selected `PROTOCOL_RDSTLS`.
//!
//! Sequence (MS-RDPBCGR 5.4.5.2 "RDSTLS Connection Sequence"): the **server**
//! speaks first, sending its Capabilities PDU; the client then sends the
//! Authentication Request; the server replies with the Authentication Response.
//! The client does NOT send a Capabilities PDU. The request carries the
//! broker-issued `redirectedAuthGuid` (as the RedirectionGuid, base64-in-Unicode)
//! and `redirectedAuthBlob` (as the encrypted password blob).

use std::io::{self, Read, Write};

use rdp_pdu::rdstls;

/// Largest RDSTLS PDU we expect (capabilities and the auth response are tiny).
const RECV_BUF: usize = 1024;

/// Run the RDSTLS handshake over `stream` (an established TLS session with the
/// target host). Returns `Ok(())` only on `RESULT_SUCCESS`.
pub fn authenticate<S: Read + Write>(
    stream: &mut S,
    redirection_guid: &[u8],
    username: &str,
    domain: &str,
    password: &[u8],
) -> io::Result<()> {
    // 1. Server → Client: Capabilities. The server speaks first; we must NOT
    //    send our own Capabilities PDU (doing so lands in the Authentication
    //    Request slot and the server rejects it as SEC_E_INVALID_TOKEN).
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut buf = [0u8; RECV_BUF];
    let n = stream.read(&mut buf)?;
    tracing::info!(
        caps_len = n,
        caps_hex = %hex(&buf[..n.min(64)]),
        "RDSTLS server capabilities raw bytes"
    );
    if n == 0 || !rdstls::is_capabilities(&buf[..n]) {
        return Err(io::Error::other(
            "RDSTLS: expected server Capabilities PDU",
        ));
    }

    // The server's SupportedVersions is a bitmask (AVD/W365 advertise 0x0003 =
    // v1|v2). The AuthReq still declares RDSTLS_VERSION_1 as long as the v1 bit is
    // set — what actually differs for AVD is the *credential* (an AES+RSA encrypted
    // password), not the version field.
    let supported = rdstls::capabilities_version(&buf[..n]).unwrap_or(rdstls::VERSION_1);
    tracing::info!(supported_versions = supported, "RDSTLS server supported-versions bitmask");

    // 2. Client → Server: Authentication Request (password-credentials variant).
    let req = rdstls::build_auth_request_password(
        rdstls::VERSION_1,
        redirection_guid,
        username,
        domain,
        password,
    );
    tracing::info!(
        req_len = req.len(),
        req_head = %hex(&req[..req.len().min(32)]),
        "RDSTLS auth request bytes (Version/PduType/DataType/GuidLen/Guid...)"
    );
    stream.write_all(&req)?;
    stream.flush()?;

    // 3. Server → Client: Authentication Response (result code).
    let n = stream.read(&mut buf)?;
    tracing::info!(
        len = n,
        response_hex = %buf[..n.min(64)].iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "RDSTLS auth response raw bytes"
    );
    match rdstls::parse_auth_response(&buf[..n]) {
        Some(rdstls::RESULT_SUCCESS) => {
            tracing::info!("RDSTLS authentication succeeded");
            Ok(())
        }
        Some(code) => Err(io::Error::other(format!(
            "RDSTLS authentication rejected: {} (0x{code:08X})",
            rdstls::result_name(code)
        ))),
        None => Err(io::Error::other("RDSTLS: malformed authentication response")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted peer: serves one queued inbound message per `read` (modelling
    /// the round-trip-separated RDSTLS PDUs over a record stream), captures writes.
    struct MockPeer {
        inbound: VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }
    impl Read for MockPeer {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.inbound.pop_front() {
                Some(msg) => {
                    let n = msg.len().min(buf.len());
                    buf[..n].copy_from_slice(&msg[..n]);
                    Ok(n)
                }
                None => Ok(0),
            }
        }
    }
    impl Write for MockPeer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn auth_response(code: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&rdstls::VERSION_1.to_le_bytes());
        v.extend_from_slice(&rdstls::TYPE_AUTHRSP.to_le_bytes());
        v.extend_from_slice(&rdstls::DATA_RESULT_CODE.to_le_bytes());
        v.extend_from_slice(&code.to_le_bytes());
        v
    }

    #[test]
    fn full_handshake_success() {
        let inbound = VecDeque::from(vec![
            rdstls::build_capabilities(), // server caps
            auth_response(rdstls::RESULT_SUCCESS),
        ]);
        let mut peer = MockPeer { inbound, written: Vec::new() };
        authenticate(&mut peer, &[0xAB; 16], "user", "", b"blob").unwrap();
        // The client must NOT send a Capabilities PDU — its first (and only)
        // write is the Authentication Request (server speaks first).
        assert_eq!(
            u16::from_le_bytes([peer.written[2], peer.written[3]]),
            rdstls::TYPE_AUTHREQ
        );
        assert!(!rdstls::is_capabilities(&peer.written[..8]));
    }

    #[test]
    fn failure_result_is_an_error() {
        let inbound = VecDeque::from(vec![
            rdstls::build_capabilities(),
            auth_response(0x0000_052E), // LOGON_FAILURE
        ]);
        let mut peer = MockPeer { inbound, written: Vec::new() };
        let err = authenticate(&mut peer, &[0; 16], "u", "", b"x").unwrap_err();
        assert!(err.to_string().contains("LOGON_FAILURE"));
    }

    #[test]
    fn missing_server_capabilities_is_an_error() {
        let mut peer = MockPeer { inbound: VecDeque::new(), written: Vec::new() };
        assert!(authenticate(&mut peer, &[0; 16], "u", "", b"x").is_err());
    }
}
