//! Azure Virtual Desktop / Windows 365 ARM connection brokering — stage 1 of
//! Reverse Connect.
//!
//! Before any WebSocket exists, the modern client brokers a connection through
//! the gateway's ARM endpoint. It `POST`s the opaque `loadBalanceInfo` token
//! (from the `.rdp` file, e.g. `mth://localhost/<id>/<endpointpool>`) to
//! `https://<gateway>/api/arm/v2/connections`; the gateway replies with the
//! **actual** WebSocket URL to connect to plus the RDSTLS reverse-connect auth
//! material that proves the brokered session to the target host.
//!
//! This matches what the Windows App / `msrdc` does (observed in a captured
//! `.rdp`: `gatewayhostname: afdfp-rdgateway-r1.wvd.microsoft.com:443`,
//! `resourceprovider: arm`, `gatewaybrokeringtype: 1`) and FreeRDP's `arm.c` /
//! `wst.c`. The request/response codec here is pure and unit-tested; the actual
//! HTTP POST is a thin wrapper (validated against a live gateway, like the rest
//! of the W365 path).

use serde_json::Value;

/// User-Agent values sent to the gateway. Derived from the observed Windows 365
/// native client (`LaunchPartnerId: Windows365NativeClient`, version 2.0.1193).
/// Easy to override once a capture confirms the exact strings the gateway wants.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) RdClient";
const MS_USER_AGENT: &str = "Windows365NativeClient/2.0.1193.0";

/// The connection details the gateway returns from `/api/arm/v2/connections`.
/// Field names mirror the JSON the gateway emits (see FreeRDP `arm_fill_gateway_parameters`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArmConnection {
    /// Preferred WebSocket URL to connect to (`gatewayLocationPreWebSocket`).
    pub gateway_location_pre_websocket: Option<String>,
    /// Fallback WebSocket URL (`gatewayLocation`).
    pub gateway_location: Option<String>,
    /// Target session host the gateway brokered to (`redirectedServerName`).
    pub redirected_server_name: Option<String>,
    /// Base64 server certificate for RDSTLS validation (`redirectedServerCert`).
    pub redirected_server_cert: Option<String>,
    /// Base64 RDSTLS reverse-connect auth blob (`redirectedAuthBlob`).
    pub redirected_auth_blob: Option<String>,
    /// GUID tying the auth blob to this brokered session (`redirectedAuthGuid`).
    pub redirected_auth_guid: Option<String>,
    /// Username to place in the RDSTLS AuthReq (`redirectedUsername`). The gateway
    /// commonly returns this as `null`, in which case the RDSTLS UserName field
    /// MUST be empty — the cookie (guid + blob) carries the identity.
    pub redirected_username: Option<String>,
    /// RDP Shortpath (UDP) parameters — the ICE/STUN/TURN config and rendezvous
    /// material the gateway returns for bringing up a UDP transport to the Cloud
    /// PC. Empty when the gateway offers no UDP path.
    pub shortpath: ShortpathConfig,
}

/// A TURN relay the gateway offers for Shortpath, with its long-term credentials
/// (from `iceServersConfig.turnServers[]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnServer {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub realm: String,
    pub secure: bool,
}

// NOTE (2026-07-05, from RE of rdpnanoTransport.dll v3.2506): W365 Shortpath is
// NOT MS-RDPEUDP/RDPEMT — it is Microsoft's proprietary "Basix DCT" nano
// transport, layered: (1) rendezvous-WS signaling exchanging an ICE
// SessionDescription {Version, Username(ufrag), Password(pwd), Candidates[],
// PacingMs, StunRetry/Timeout}; (2) ICE host/srflx/relay via stun.rs; (3)
// Pseudo-TLS (BCrypt) link security; (4) URCP reliable-conn + rate controller
// (SYN/SYNACK); (5) Smiles v3 multi-link bonding carrying "smiles+userdata".
// The rendezvous signaling lives in crate::rendezvous (milestone 1).
//
/// RDP Shortpath (UDP) config extracted from the ARM `/connections` response:
/// the ICE server list plus the rendezvous material used to exchange candidates
/// with the Cloud PC (`clientRendezvousLocation`, `protocolConfigResponseAsJson`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortpathConfig {
    pub stun_enabled: bool,
    pub turn_enabled: bool,
    pub turn_servers: Vec<TurnServer>,
    /// STUN servers as `host:port` (often empty — STUN rides the TURN server).
    pub stun_servers: Vec<String>,
    /// UDP port the Cloud PC listens on for RDP-UDP (`udpPort`).
    pub udp_port: Option<u16>,
    /// Opaque rendezvous location for exchanging ICE candidates with the host.
    pub client_rendezvous_location: Option<String>,
    /// Embedded JSON string carrying the multitransport/protocol config.
    pub protocol_config_json: Option<String>,
}

impl ShortpathConfig {
    /// True if the gateway advertised any usable UDP path (a TURN relay or an
    /// explicit UDP port). Drives whether the Shortpath driver even tries.
    pub fn is_offered(&self) -> bool {
        self.udp_port.is_some() || !self.turn_servers.is_empty()
    }
}

impl ArmConnection {
    /// The WebSocket URL to dial: the pre-WebSocket location if present, else the
    /// plain gateway location. `None` if the gateway returned neither.
    pub fn websocket_url(&self) -> Option<&str> {
        self.gateway_location_pre_websocket
            .as_deref()
            .or(self.gateway_location.as_deref())
    }
}

/// Build the JSON body for `POST /api/arm/v2/connections`.
///
/// `application` is the `.rdp`'s `remoteapplicationprogram` (e.g. `||<resourceId>`)
/// — the gateway dereferences it to resolve the target Cloud PC, so it must be
/// the exact value or the orchestrator returns a 400 NullReference.
/// `load_balance_info` is the opaque `loadbalanceinfo` string, replayed verbatim
/// so the gateway routes to the right host pool. The two null token fields match
/// the Windows App / FreeRDP `arm_create_request_json` body exactly.
pub fn build_connection_request(
    application: &str,
    load_balance_info: &str,
    shortpath: bool,
) -> String {
    // The gateway treats loadBalanceInfo as opaque; we must not decode/reencode
    // it. serde_json handles escaping of the `mth://...` value.
    let mut req = serde_json::json!({
        "application": application,
        "loadBalanceInfo": load_balance_info,
        "LogonToken": serde_json::Value::Null,
        "gatewayLoadBalancerToken": serde_json::Value::Null,
    });
    if shortpath {
        // Replicate the Windows App's Shortpath-enabling request. The decisive
        // field is `protocolConfigRequestAsJson`: without it the gateway returns
        // `RendezvousMode=0` / `udpProperties=0` (TCP only); with
        // `RendezvousMode=4` it enables the UDP rendezvous. The remaining fields
        // mirror the captured msrdc body so the gateway takes the same path.
        let obj = req.as_object_mut().expect("json object");
        obj.insert("ssoLogonToken".into(), Value::Null);
        obj.insert(
            "protocolConfigRequestAsJson".into(),
            Value::String(shortpath_protocol_config()),
        );
        obj.insert("geo".into(), Value::String("US__False".into()));
        obj.insert("wcioProtectionSessionNonce".into(), Value::String(String::new()));
        obj.insert("clientCapabilities".into(), serde_json::json!({}));
    }
    req.to_string()
}

/// The client transport-capability declaration that turns on RDP Shortpath, sent
/// as the stringified `protocolConfigRequestAsJson`. Mirrors the captured Windows
/// App body; `TransportCapabilities.RDPLegacy.RendezvousMode = 4` is what makes
/// the gateway enable the UDP rendezvous (0 = TCP only).
fn shortpath_protocol_config() -> String {
    serde_json::json!({
        "ProtocolConfigVersion": 1,
        "ClientType": "com.microsoft.rdc.windows.wa.msrdc.msix.x64",
        "ClientVersion": "1.2.7214.0",
        "TransportCapabilities": {
            "RDPLegacy": {
                "ProtocolVersion": "8.17",
                "SmilesVersion": 3,
                "TurnCloningMode": 1,
                "RendezvousMode": 4,
                "TransportAutoReconnectSupported": 1,
                "TransportAutoReconnectMode": 2,
                "SkipLegacyProtocolSupportedVersion": 3,
                "ReplaceStaticVCs": 1,
                "ListenerInSmiles": 0,
                "FinPktMode": 2,
                "EnableDelayTraceAtSequencer": 1,
                "EnableSequencerHeader": 1,
                "UseSidebandSmilesOperations": 1
            },
            "NanoTransportStackPrototype": { "ProtocolVersion": 1, "ProtocolVersionMin": 1 }
        },
        "SecurityCapabilities": {
            "SupportedAuthenticationProtocols": ["CredSSP", "StandardTLS"],
            "AuthOverConnectionControl": true
        }
    })
    .to_string()
}

/// Parse the gateway's `/api/arm/v2/connections` JSON response.
pub fn parse_connection_response(body: &str) -> Result<ArmConnection, serde_json::Error> {
    let v: Value = serde_json::from_str(body)?;
    let s = |key: &str| v.get(key).and_then(Value::as_str).map(String::from);
    Ok(ArmConnection {
        gateway_location_pre_websocket: s("gatewayLocationPreWebSocket"),
        gateway_location: s("gatewayLocation"),
        redirected_server_name: s("redirectedServerName"),
        redirected_server_cert: s("redirectedServerCert"),
        redirected_auth_blob: s("redirectedAuthBlob"),
        redirected_auth_guid: s("redirectedAuthGuid"),
        redirected_username: s("redirectedUsername"),
        shortpath: parse_shortpath(&v),
    })
}

/// Extract the RDP Shortpath (UDP) config from the ARM `/connections` response.
fn parse_shortpath(v: &Value) -> ShortpathConfig {
    let ice = v.get("iceServersConfig");
    let mut cfg = ShortpathConfig {
        udp_port: v
            .get("udpPort")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .filter(|&p| p != 0),
        client_rendezvous_location: v
            .get("clientRendezvousLocation")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        protocol_config_json: v
            .get("protocolConfigResponseAsJson")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        ..Default::default()
    };
    if let Some(ice) = ice {
        cfg.stun_enabled = ice.get("stunEnabled").and_then(Value::as_bool).unwrap_or(false);
        cfg.turn_enabled = ice.get("turnEnabled").and_then(Value::as_bool).unwrap_or(false);
        if let Some(arr) = ice.get("stunServers").and_then(Value::as_array) {
            for s in arr {
                if let Some(url) = s.get("url") {
                    if let (Some(h), Some(p)) = (
                        url.get("host").and_then(Value::as_str),
                        url.get("port").and_then(Value::as_u64),
                    ) {
                        cfg.stun_servers.push(format!("{h}:{p}"));
                    }
                }
            }
        }
        if let Some(arr) = ice.get("turnServers").and_then(Value::as_array) {
            for t in arr {
                let url = t.get("url");
                let host = url
                    .and_then(|u| u.get("host"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let port = url
                    .and_then(|u| u.get("port"))
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .unwrap_or(3478);
                if host.is_empty() {
                    continue;
                }
                cfg.turn_servers.push(TurnServer {
                    host,
                    port,
                    username: t.get("username").and_then(Value::as_str).unwrap_or_default().to_string(),
                    password: t.get("password").and_then(Value::as_str).unwrap_or_default().to_string(),
                    realm: t.get("realm").and_then(Value::as_str).unwrap_or_default().to_string(),
                    secure: url
                        .and_then(|u| u.get("secure"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
        }
    }
    cfg
}

/// Log the Shortpath config secret-safely, decoding the rendezvous fields enough
/// to reveal their schema (candidate addresses show; long tokens are truncated)
/// so the ICE driver can be built against real data.
fn log_shortpath(cfg: &ShortpathConfig) {
    if !cfg.is_offered() && cfg.client_rendezvous_location.is_none() {
        tracing::info!("Shortpath: gateway offered no UDP path (staying on TCP)");
        return;
    }
    for (i, t) in cfg.turn_servers.iter().enumerate() {
        tracing::info!(
            index = i,
            host = %t.host,
            port = t.port,
            realm = %t.realm,
            secure = t.secure,
            username_len = t.username.len(),
            password_len = t.password.len(),
            "Shortpath TURN server"
        );
    }
    tracing::info!(
        stun_enabled = cfg.stun_enabled,
        turn_enabled = cfg.turn_enabled,
        stun_servers = ?cfg.stun_servers,
        udp_port = ?cfg.udp_port,
        rendezvous_len = cfg.client_rendezvous_location.as_ref().map(|s| s.len()),
        "Shortpath ICE config"
    );
    // The rendezvous location + protocol-config schema are undocumented; surface
    // their structure (short values shown, long tokens truncated) so the exact
    // candidate-exchange format can be built precisely from a live run.
    if let Some(r) = &cfg.client_rendezvous_location {
        log_opaque_field("clientRendezvousLocation", r);
    }
    if let Some(p) = &cfg.protocol_config_json {
        log_opaque_field("protocolConfigResponseAsJson", p);
    }
}

/// Reveal an opaque string field's schema: if it (or its base64 decoding) is
/// JSON, flatten it (long strings truncated); otherwise show a short prefix.
fn log_opaque_field(name: &str, raw: &str) {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        let mut fields = Vec::new();
        collect_json_fields("", &v, &mut fields);
        tracing::info!(field = name, kind = "json", data = %fields.join("  "), "Shortpath rendezvous field");
        return;
    }
    if let Some(bytes) = decode_b64(raw) {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            let mut fields = Vec::new();
            collect_json_fields("", &v, &mut fields);
            tracing::info!(field = name, kind = "base64-json", data = %fields.join("  "), "Shortpath rendezvous field");
            return;
        }
    }
    let prefix: String = raw.chars().take(96).collect();
    tracing::info!(field = name, kind = "opaque", len = raw.len(), prefix = %prefix, "Shortpath rendezvous field");
}

/// Broker a Reverse Connect session through the AVD/W365 gateway.
///
/// `gateway` is the `gatewayhostname` from the `.rdp` (host or `host:port`);
/// `token` is the AVD/ARM bearer token; `application` is the `.rdp`'s
/// `remoteapplicationprogram`; `load_balance_info` is the opaque
/// `loadbalanceinfo` token. Returns the connection details (WebSocket URL +
/// RDSTLS auth) on success.
pub fn broker_connection(
    gateway: &str,
    token: &str,
    application: &str,
    load_balance_info: &str,
    shortpath: bool,
) -> Result<ArmConnection, crate::w365::AuthError> {
    let host = gateway.trim();
    // `gatewayhostname` may already include `:443`; build a clean https URL.
    let url = if host.contains("://") {
        format!("{host}/api/arm/v2/connections")
    } else {
        format!("https://{host}/api/arm/v2/connections")
    };
    let body = build_connection_request(application, load_balance_info, shortpath);

    tracing::info!(%url, "brokering W365 Reverse Connect via ARM /connections");

    let http_resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("User-Agent", USER_AGENT)
        .set("X-Ms-User-Agent", MS_USER_AGENT)
        .send_string(&body);

    let text = match http_resp {
        Ok(r) => r.into_string()?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            tracing::error!(status = code, %body, "ARM /connections returned an error");
            return Err(crate::w365::AuthError::Failed(format!(
                "ARM /connections returned {code}: {body}"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    // Diagnostic: surface EVERY field the gateway returned (recursively) so we can
    // spot RDSTLS material we don't yet parse — most importantly a username/domain
    // for the AuthReq (the broker so far gave none, and a wrong username is a prime
    // suspect for the RDSTLS INVALID_TOKEN). Secret-safe: string values longer than
    // 48 chars (tokens, blobs, certs) are shown as `str[len]`, not dumped.
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        let mut fields = Vec::new();
        collect_json_fields("", &v, &mut fields);
        tracing::info!(fields = %fields.join("  "), "ARM /connections raw response fields");
    }

    let conn = parse_connection_response(&text)
        .map_err(|e| crate::w365::AuthError::Failed(format!("ARM response parse error: {e}")))?;

    log_shortpath(&conn.shortpath);

    if conn.websocket_url().is_none() {
        return Err(crate::w365::AuthError::Failed(
            "ARM /connections response had no gatewayLocation".into(),
        ));
    }
    Ok(conn)
}

/// Recursively flatten a JSON value into `path=value` strings for diagnostics,
/// truncating long string values so secrets aren't logged in full. Also used by
/// the Shortpath rendezvous broker ([`crate::rendezvous`]) to inventory its
/// response secret-safely.
pub(crate) fn collect_json_fields(prefix: &str, v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_json_fields(&path, val, out);
            }
        }
        Value::Array(items) => {
            for (i, val) in items.iter().enumerate() {
                collect_json_fields(&format!("{prefix}[{i}]"), val, out);
            }
        }
        Value::String(s) if s.len() > 48 => out.push(format!("{prefix}=str[{}]", s.len())),
        Value::String(s) => out.push(format!("{prefix}=\"{s}\"")),
        Value::Null => out.push(format!("{prefix}=null")),
        other => out.push(format!("{prefix}={other}")),
    }
}

/// Decode standard/url-safe base64 (the encoding the gateway uses for
/// `redirectedAuthBlob` / `redirectedAuthGuid` / `redirectedServerCert`).
/// Whitespace and `=` padding are ignored; returns `None` on an invalid symbol.
pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62), // '-' for url-safe
            b'/' | b'_' => Some(63), // '_' for url-safe
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_blob() {
        // "BLOB" → "QkxPQg=="
        assert_eq!(decode_b64("QkxPQg==").as_deref(), Some(&b"BLOB"[..]));
        // url-safe and whitespace tolerated.
        assert_eq!(decode_b64("Q k x P Qg").as_deref(), Some(&b"BLOB"[..]));
        assert!(decode_b64("not base64 !!!").is_none());
    }

    #[test]
    fn request_carries_loadbalanceinfo_and_application_verbatim() {
        let lbi = "mth://localhost/1ee5c7d4-48b3-4e2a-a4e3-1e58b9973b48/c5b9ed5b";
        let app = "||3b84bb08-b174-42f4-fd04-08dd76946c79";
        let body = build_connection_request(app, lbi, false);
        let v: Value = serde_json::from_str(&body).unwrap();
        // Field names/shape must match the Windows App / FreeRDP body exactly.
        assert_eq!(v["loadBalanceInfo"], lbi);
        assert_eq!(v["application"], app);
        assert!(v["LogonToken"].is_null());
        assert!(v["gatewayLoadBalancerToken"].is_null());
        // The old (wrong) key must not be present.
        assert!(v.get("applicationName").is_none());
        // Default (non-Shortpath) request omits the transport-capability block.
        assert!(v.get("protocolConfigRequestAsJson").is_none());
    }

    #[test]
    fn shortpath_request_carries_rendezvous_capabilities() {
        let body = build_connection_request("||app", "mth://x", true);
        let v: Value = serde_json::from_str(&body).unwrap();
        // The stringified protocol config must be present and declare RendezvousMode 4.
        let pc = v["protocolConfigRequestAsJson"].as_str().expect("present");
        let pcv: Value = serde_json::from_str(pc).unwrap();
        assert_eq!(pcv["TransportCapabilities"]["RDPLegacy"]["RendezvousMode"], 4);
        assert!(pcv["TransportCapabilities"].get("NanoTransportStackPrototype").is_some());
        // Core routing fields still carried verbatim.
        assert_eq!(v["application"], "||app");
        assert_eq!(v["loadBalanceInfo"], "mth://x");
    }

    #[test]
    fn response_extracts_url_and_auth() {
        let json = r#"{
            "gatewayLocationPreWebSocket": "wss://rdgateway-r1.wvd.microsoft.com/api/arm/v2/connect?ConnectionId=abc",
            "gatewayLocation": "wss://rdgateway-r1.wvd.microsoft.com/fallback",
            "redirectedServerName": "host.pool.local",
            "redirectedAuthBlob": "QkxPQg==",
            "redirectedAuthGuid": "11111111-2222-3333-4444-555555555555"
        }"#;
        let conn = parse_connection_response(json).unwrap();
        assert_eq!(
            conn.websocket_url(),
            Some("wss://rdgateway-r1.wvd.microsoft.com/api/arm/v2/connect?ConnectionId=abc")
        );
        assert_eq!(conn.redirected_server_name.as_deref(), Some("host.pool.local"));
        assert_eq!(conn.redirected_auth_blob.as_deref(), Some("QkxPQg=="));
        assert!(conn.redirected_auth_guid.is_some());
    }

    #[test]
    fn websocket_url_falls_back_to_plain_location() {
        let json = r#"{ "gatewayLocation": "wss://gw/only" }"#;
        let conn = parse_connection_response(json).unwrap();
        assert_eq!(conn.websocket_url(), Some("wss://gw/only"));
    }

    #[test]
    fn missing_locations_yields_none() {
        let conn = parse_connection_response(r#"{ "foo": "bar" }"#).unwrap();
        assert_eq!(conn.websocket_url(), None);
    }

    #[test]
    fn parses_shortpath_ice_config() {
        // The iceServersConfig / udpPort shape the AVD gateway returns (from a
        // live W365 broker response).
        let json = r#"{
            "gatewayLocation": "wss://gw/only",
            "udpPort": 3390,
            "iceServersConfig": {
                "stunEnabled": true,
                "stunServers": null,
                "turnEnabled": true,
                "turnServers": [
                    {
                        "url": { "host": "51.5.255.240", "port": 3478, "secure": false, "transport": null },
                        "username": "1751659483:abc",
                        "password": "SDWgWy253A4h4HJIy1QtkrJxtHs=",
                        "realm": "rtcmedia",
                        "expiresOn": "2026-07-08T23:44:43Z"
                    }
                ]
            }
        }"#;
        let conn = parse_connection_response(json).unwrap();
        let sp = &conn.shortpath;
        assert!(sp.is_offered());
        assert_eq!(sp.udp_port, Some(3390));
        assert!(sp.stun_enabled && sp.turn_enabled);
        assert_eq!(sp.turn_servers.len(), 1);
        let t = &sp.turn_servers[0];
        assert_eq!(t.host, "51.5.255.240");
        assert_eq!(t.port, 3478);
        assert_eq!(t.realm, "rtcmedia");
        assert_eq!(t.username, "1751659483:abc");
        assert_eq!(t.password, "SDWgWy253A4h4HJIy1QtkrJxtHs=");
        assert!(!t.secure);
    }

    #[test]
    fn no_shortpath_when_absent() {
        let conn = parse_connection_response(r#"{ "gatewayLocation": "wss://gw" }"#).unwrap();
        assert!(!conn.shortpath.is_offered());
    }
}
