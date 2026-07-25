//! RD Gateway (MS-TSGU) support.
//!
//! External Windows 365 / enterprise access often requires an RD Gateway. A
//! full implementation needs HTTPS/SOAP enrollment plus RPC-over-HTTP v2 to
//! tunnel the RDP connection, which is a large protocol. This module currently
//! parses gateway settings from `.rdp` files and feed entries and exposes them
//! in [`ClientConfig`]; the actual tunnel is stubbed and will be implemented as
//! a later phase (either a pure-Rust MS-TSGU stack or a COM interop bridge with
//! the OS RD Gateway client).

use rdp_core::ClientConfig;

/// How the RD Gateway should authenticate the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayAuth {
    /// Ask the user for credentials at connection time.
    Prompt,
    /// Use the same credentials as the target RDP session.
    Same,
    /// Use a smart card.
    Smartcard,
}

/// Parsed RD Gateway settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    pub hostname: String,
    pub port: u16,
    pub auth: GatewayAuth,
    pub bypass_for_local: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            port: 443,
            auth: GatewayAuth::Same,
            bypass_for_local: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum GatewayError {
    #[error("RD Gateway tunnel is not implemented")]
    NotImplemented,
    #[error("invalid gateway settings: {0}")]
    Invalid(String),
}

/// Parse RD Gateway keys from an `.rdp` file settings map (the output of
/// [`feed::parse_rdp_file`](crate::feed::parse_rdp_file)).
pub fn parse_rdp_settings(settings: &std::collections::HashMap<String, String>) -> Option<GatewayConfig> {
    let hostname = settings.get("gatewayhostname")?;
    if hostname.is_empty() {
        return None;
    }
    let mut cfg = GatewayConfig::default();
    cfg.hostname = hostname.clone();
    if let Some(p) = settings.get("gatewayport").and_then(|s| s.parse().ok()) {
        cfg.port = p;
    }
    cfg.auth = match settings.get("gatewaycredentialssource").map(String::as_str) {
        Some("1") => GatewayAuth::Smartcard,
        Some("4") => GatewayAuth::Prompt,
        _ => GatewayAuth::Same,
    };
    cfg.bypass_for_local = settings
        .get("gatewayusagemethod")
        .map(|s| s == "2")
        .unwrap_or(false);
    Some(cfg)
}

/// Apply gateway settings discovered from a feed or `.rdp` file to `config`.
/// The settings are stashed; the connect path can decide whether to tunnel.
pub fn apply_to_config(config: &mut ClientConfig, cfg: &GatewayConfig) {
    // Store the gateway cookie/hostname in load_balance_info so it survives
    // into the connection layer. The real tunnel will read it from here.
    if !config.load_balance_info.as_ref().map(|v| v.is_empty()).unwrap_or(true) {
        return;
    }
    let cookie = format!("GatewayHostName:{}", cfg.hostname);
    config.load_balance_info = Some(cookie.into_bytes());
}

/// Establish an RDP session through an RD Gateway.
///
/// Currently returns [`GatewayError::NotImplemented`]. A full implementation
/// would:
/// 1. Contact `https://<cfg.hostname>/RDGateway/` with a SOAP connection request.
/// 2. Build an RPC-over-HTTP IN/OUT channel pair.
/// 3. Return a socket-like object the rest of the RDP stack can read/write.
#[allow(dead_code)]
pub fn connect(_cfg: &GatewayConfig, _target: &ClientConfig) -> Result<std::net::TcpStream, GatewayError> {
    Err(GatewayError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_settings_extracts_hostname_and_auth() {
        let mut settings = std::collections::HashMap::new();
        settings.insert("gatewayhostname".into(), "gw.example".into());
        settings.insert("gatewayport".into(), "443".into());
        settings.insert("gatewaycredentialssource".into(), "4".into());

        let cfg = parse_rdp_settings(&settings).unwrap();
        assert_eq!(cfg.hostname, "gw.example");
        assert_eq!(cfg.port, 443);
        assert_eq!(cfg.auth, GatewayAuth::Prompt);
    }

    #[test]
    fn missing_hostname_means_no_gateway() {
        let settings = std::collections::HashMap::new();
        assert!(parse_rdp_settings(&settings).is_none());
    }

    #[test]
    fn connect_is_not_implemented() {
        let cfg = GatewayConfig::default();
        let client = ClientConfig::default();
        assert!(matches!(
            connect(&cfg, &client).unwrap_err(),
            GatewayError::NotImplemented
        ));
    }
}
