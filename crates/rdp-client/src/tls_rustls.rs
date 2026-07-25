//! rustls TLS client for the Enhanced RDP Security path on non-Windows hosts.
//!
//! The counterpart to the Windows SChannel [`crate::tls`] module: it wraps an
//! inner byte stream (a `TcpStream`) in a rustls-negotiated TLS session and
//! exposes the same `connect` / [`Read`] / [`Write`] / `get_ref` surface, so the
//! `Read + Write`-generic RDP stack runs unchanged over the tunnel.
//!
//! `--insecure` installs a verifier that accepts any certificate (RDP hosts very
//! commonly present self-signed certs); otherwise the webpki-roots trust anchors
//! are used. NLA/CredSSP public-key binding is Windows-only, so `remote_cert_der`
//! is exposed for parity but the headless Linux path does not use it.

use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

/// A rustls TLS session over an inner byte stream `S`.
pub struct TlsStream<S: Read + Write> {
    inner: StreamOwned<ClientConnection, S>,
}

impl<S: Read + Write> TlsStream<S> {
    /// Perform the rustls client handshake over `stream` for `server_name`.
    /// `accept_invalid` (from `--insecure`) disables certificate validation.
    pub fn connect(stream: S, server_name: &str, accept_invalid: bool) -> io::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let config = if accept_invalid {
            ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification(provider)))
                .with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?
                .with_root_certificates(roots)
                .with_no_client_auth()
        };

        // RDP server names are often bare IPs or non-DNS labels; own the string so
        // the connection isn't borrowed from a temporary.
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|e| io::Error::other(format!("invalid TLS server name: {e}")))?;
        let conn = ClientConnection::new(Arc::new(config), name).map_err(io::Error::other)?;
        Ok(Self {
            inner: StreamOwned::new(conn, stream),
        })
    }

    /// The inner stream (e.g. to set a socket read timeout).
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref()
    }

    /// The negotiated peer certificate (DER), for parity with the SChannel path.
    pub fn remote_cert_der(&self) -> Option<Vec<u8>> {
        self.inner
            .conn
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.as_ref().to_vec())
    }
}

impl<S: Read + Write> Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Read + Write> Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A certificate verifier that accepts everything (for `--insecure`). Signature
/// checks are still delegated to the crypto provider so the handshake is
/// well-formed; only the trust-chain/hostname check is skipped.
#[derive(Debug)]
struct NoVerification(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
