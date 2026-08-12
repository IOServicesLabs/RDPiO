//! Reusable RDP connection startup shared by the CLI and the TUI.
//!
//! [`connect_with_profile`] is the single entry point that turns a
//! [`ConnectionProfile`] — built from the `--host`/`--user`/`--password`/
//! `--insecure` CLI flags or picked from the saved-connections UI — into a live
//! RDP session. All blocking connection work lives here, out of the UI: the
//! caller hands over a profile and gets back a result. The Windows build opens
//! a window and paints the live desktop; other platforms run the same protocol
//! stack headless, logging decoded rectangles.
//!
//! Note: the crate also has a Windows-only [`crate::connect_windows`] module
//! holding the low-level establishment machinery (`Transport`,
//! `establish_reconnect`) that the windowed path consumes; this module is the
//! profile-facing layer on top of both platform paths.

use rdp_core::{ClientConfig, Credentials};

use crate::connections::ConnectionProfile;

#[cfg(not(windows))]
use crate::session;
#[cfg(not(windows))]
use crate::tls;
#[cfg(not(windows))]
use crate::transport;

/// Build the [`ClientConfig`] the protocol stack needs from a
/// [`ConnectionProfile`], mirroring the mapping the `--host` CLI flags have
/// always used: host and port come straight across, a Windows-style
/// `DOMAIN\user` logon name is split into domain + user, the password defaults
/// to empty, and `--insecure` becomes `allow_invalid_certificate`.
pub fn config_from_profile(profile: &ConnectionProfile) -> ClientConfig {
    // Split a Windows-style logon name (`DOMAIN\user`, `.\user`, UPN) into the
    // separate domain/user fields RDP needs — passing `.\user` through verbatim
    // makes the server reject it as an unknown account (STATUS_LOGON_FAILURE).
    let (domain, username) = rdp_core::split_domain_user("", &profile.username);
    ClientConfig {
        hostname: profile.host.clone(),
        port: profile.port,
        credentials: Credentials {
            domain,
            username,
            password: profile.password.clone().unwrap_or_default(),
        },
        allow_invalid_certificate: profile.insecure,
        ..Default::default()
    }
}

/// Errors from [`connect_with_profile`].
#[derive(Debug)]
pub enum ConnectError {
    /// The TCP/X.224 negotiation failed before any security layer was reached
    /// (headless / non-Windows path).
    #[cfg(not(windows))]
    Negotiate(transport::NegotiateError),
    /// The windowed (Windows) connect path failed.
    #[cfg(windows)]
    Runtime(Box<dyn std::error::Error>),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(windows))]
            ConnectError::Negotiate(e) => write!(f, "{e}"),
            #[cfg(windows)]
            ConnectError::Runtime(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(not(windows))]
            ConnectError::Negotiate(e) => Some(e),
            #[cfg(windows)]
            ConnectError::Runtime(e) => Some(e.as_ref()),
        }
    }
}

/// Start an RDP connection from a [`ConnectionProfile`].
///
/// This is the single entry point the CLI (`--host ...`) and the saved/TUI
/// connection picker share: the caller builds (or loads) a profile, hands it
/// over, and all blocking connection work happens here. On Windows a window
/// opens and the live desktop is painted; elsewhere the same protocol stack
/// runs headless, logging decoded rectangles.
///
/// The UI never connects directly: it returns a profile to its caller, which
/// is what keeps the blocking RDP session loop out of the terminal UI.
pub fn connect_with_profile(profile: &ConnectionProfile) -> Result<(), ConnectError> {
    #[cfg(not(windows))]
    {
        let config = config_from_profile(profile);
        run_headless(&config).map_err(ConnectError::Negotiate)
    }

    #[cfg(windows)]
    {
        // The windowed path still consumes `Args`; build a default set from the
        // profile (all extended display flags stay at their defaults — the
        // profile is the source of truth for host/user/password/insecure).
        let args = crate::Args::from_profile(profile);
        crate::win::run_connected(&args).map_err(ConnectError::Runtime)
    }
}

/// Run a headless session against `config` (non-Windows): negotiate, activate,
/// and log decoded bitmap rectangles. Exercises the entire protocol stack
/// without a GPU. Shared by the direct `--host` path, the W365/feed paths, and
/// TUI-selected profiles.
#[cfg(not(windows))]
pub(crate) fn run_headless(config: &ClientConfig) -> Result<(), transport::NegotiateError> {
    use rdp_pdu::x224::SecurityProtocol;

    tracing::info!(host = %config.hostname, port = config.port, "connecting over TCP");
    let (mut stream, _connector, protocol) = transport::connect(config)?;
    tracing::info!(?protocol, "X.224 negotiation complete");

    // A read timeout prevents hangs against a silent server (set before the TLS
    // handshake, which also does I/O; it persists on the moved socket).
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    // Enhanced RDP Security (SSL) and NLA (HYBRID) both run inside a TLS tunnel;
    // Standard RDP Security runs directly over the socket.
    if protocol.contains(SecurityProtocol::SSL) || protocol.contains(SecurityProtocol::HYBRID) {
        let mut tls = match tls::TlsStream::connect(
            stream,
            &config.hostname,
            config.allow_invalid_certificate,
        ) {
            Ok(tls) => {
                tracing::info!("TLS established (rustls)");
                tls
            }
            Err(err) => {
                tracing::warn!(error = %err, "TLS handshake failed");
                return Ok(());
            }
        };

        if protocol.contains(SecurityProtocol::HYBRID) {
            // NLA/CredSSP (MS-CSSP) authenticates over the TLS channel — binding to
            // the server certificate's public key — before the MCS connection.
            let cert = match tls.remote_cert_der() {
                Some(cert) => cert,
                None => {
                    tracing::error!("no server certificate available for NLA channel binding");
                    return Ok(());
                }
            };
            let spn = format!("TERMSRV/{}", config.hostname);
            let creds = &config.credentials;
            match rdp_nla::credssp::authenticate(
                &mut tls,
                &spn,
                &cert,
                &creds.domain,
                &creds.username,
                &creds.password,
            ) {
                Ok(()) => tracing::info!("NLA/CredSSP complete"),
                Err(err) => {
                    tracing::error!(error = %err, "NLA/CredSSP failed");
                    return Ok(());
                }
            }
        }

        headless_run(&mut tls, config, protocol);
    } else {
        // Standard RDP Security (no TLS): run directly over the socket.
        headless_run(&mut stream, config, protocol);
    }
    Ok(())
}

/// Activate and run a headless session over any `Read + Write` transport, logging
/// decoded rectangles. Shared by the plaintext and rustls-TLS paths.
#[cfg(not(windows))]
fn headless_run<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    config: &ClientConfig,
    protocol: rdp_pdu::x224::SecurityProtocol,
) {
    match session::activate(stream, config, protocol, None) {
        Ok(mut active) => {
            tracing::info!(info = ?active.info(), "RDP session ACTIVE");
            let mut sink = LogSink::default();
            if let Err(err) = session::run_session(stream, &mut active, &mut sink) {
                tracing::info!(error = %err, "session ended");
            }
        }
        Err(err) => tracing::warn!(error = %err, "activation stopped"),
    }
}

/// Headless frame sink: logs decoded bitmap rectangles (non-Windows builds).
#[cfg(not(windows))]
#[derive(Default)]
struct LogSink {
    rects: u64,
}

#[cfg(not(windows))]
impl session::FrameSink for LogSink {
    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        self.rects += 1;
        tracing::debug!(x, y, w, h, bytes = rgba.len(), "paint rect");
    }

    fn present(&mut self) {
        tracing::info!(painted_rects = self.rects, "frame presented");
    }

    fn cursor(&mut self, update: session::CursorUpdate) {
        match update {
            session::CursorUpdate::Hide => tracing::debug!("cursor update: hide"),
            session::CursorUpdate::Default => tracing::debug!("cursor update: default arrow"),
            session::CursorUpdate::Shape {
                width,
                height,
                hot_x,
                hot_y,
                rgba,
            } => tracing::debug!(
                width,
                height,
                hot_x,
                hot_y,
                bytes = rgba.len(),
                "cursor update: shape"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal profile with the given connection parameters.
    fn profile(
        host: &str,
        username: &str,
        password: Option<&str>,
        insecure: bool,
    ) -> ConnectionProfile {
        ConnectionProfile {
            name: host.to_string(),
            host: host.to_string(),
            port: 3389,
            username: username.to_string(),
            password: password.map(str::to_string),
            insecure,
            saved: false,
            last_connected_at: None,
        }
    }

    #[test]
    fn config_from_profile_carries_host_port_and_insecure() {
        let p = profile("10.0.0.5", "alice", Some("s3cret"), true);
        let config = config_from_profile(&p);
        assert_eq!(config.hostname, "10.0.0.5");
        assert_eq!(config.port, 3389);
        assert_eq!(config.credentials.username, "alice");
        assert_eq!(config.credentials.password, "s3cret");
        assert!(config.allow_invalid_certificate);
        // Never leaks the password into the Debug representation.
        assert!(!format!("{config:?}").contains("s3cret"));
    }

    #[test]
    fn config_from_profile_defaults_password_and_cert_validation() {
        let p = profile("server.corp", "bob", None, false);
        let config = config_from_profile(&p);
        assert_eq!(config.hostname, "server.corp");
        assert_eq!(config.port, 3389);
        assert_eq!(config.credentials.domain, "");
        assert_eq!(config.credentials.username, "bob");
        assert_eq!(config.credentials.password, "");
        assert!(!config.allow_invalid_certificate);
    }

    #[test]
    fn config_from_profile_splits_windows_logon_names() {
        let p = profile("10.0.0.9", r"CORP\carol", None, false);
        let config = config_from_profile(&p);
        assert_eq!(config.credentials.domain, "CORP");
        assert_eq!(config.credentials.username, "carol");
    }

    #[test]
    fn config_from_profile_uses_profile_port() {
        let mut p = profile("host.example", "dave", None, false);
        p.port = 3390;
        let config = config_from_profile(&p);
        assert_eq!(config.port, 3390);
    }
}
