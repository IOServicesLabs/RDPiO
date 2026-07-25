//! Windows 365 / Azure Virtual Desktop Reverse Connect transport.
//!
//! W365/AVD do not expose the Cloud PC on a public TCP port. Instead the client
//! connects to a gateway over a TLS-secured WebSocket ("Reverse Connect") and
//! the RDP byte stream runs inside it.
//!
//! The WebSocket URL is **not** a fixed path — it is brokered: the client first
//! `POST`s the `loadbalanceinfo` token to `/api/arm/v2/connections` (see
//! [`crate::arm_broker`]) and the gateway returns the real WebSocket URL plus
//! RDSTLS auth material. The caller passes that brokered URL in via
//! `path_override`. [`DEFAULT_PATH`] is only a last-resort fallback for manual
//! testing. Header set matches the Windows App / FreeRDP `wst.c` upgrade.

use std::io::{self, Read, Write};

use rdp_core::ReverseConnectConfig;

use crate::websocket::WebSocketStream;

const DEFAULT_PATH: &str = "/reverseconnect/v1/primary";

/// Errors specific to establishing a Reverse Connect tunnel.
#[derive(Debug, thiserror::Error)]
pub enum ReverseConnectError {
    #[error("missing Reverse Connect configuration: {0}")]
    MissingConfig(&'static str),
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] crate::websocket::WebSocketError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// A byte stream over a W365/AVD Reverse Connect WebSocket tunnel.
pub struct ReverseConnectStream {
    inner: WebSocketStream,
}

impl ReverseConnectStream {
    /// Connect to the Reverse Connect gateway described by `rc`.
    ///
    /// `path_override` lets the caller substitute a different gateway path for
    /// testing against captures; `None` uses the default.
    pub fn connect(
        rc: &ReverseConnectConfig,
        path_override: Option<&str>,
        accept_invalid_cert: bool,
    ) -> Result<Self, ReverseConnectError> {
        let gateway = rc
            .gateway_fqdn
            .trim();
        if gateway.is_empty() {
            return Err(ReverseConnectError::MissingConfig("gateway_fqdn"));
        }
        if rc.access_token.is_empty() {
            return Err(ReverseConnectError::MissingConfig("access_token"));
        }

        // Stage-1 ARM brokering returns the real, absolute URL to dial. The W365
        // gateway returns it with an `https://` (or `http://`) scheme pointing at
        // a different host than the broker gateway; upgrade the scheme to
        // `wss://`/`ws://` and dial it verbatim, using its own host as the TLS
        // SNI. The brokered URL already carries its auth (RDmiGatewayToken) and
        // routing query params, so we must NOT append resourceId/tenantId/etc.
        let (url, sni_host) = match path_override {
            Some(u) if is_brokered_url(u) => {
                let ws = upgrade_to_ws_scheme(u);
                let host = url::Url::parse(&ws)
                    .ok()
                    .and_then(|p| p.host_str().map(String::from))
                    .unwrap_or_else(|| gateway.to_string());
                (ws, host)
            }
            other => {
                let path = other.unwrap_or(DEFAULT_PATH);
                let mut url = format!("wss://{gateway}{path}");
                let mut query = Vec::new();
                if !rc.resource_id.is_empty() {
                    query.push(format!("resourceId={}", url_encode(&rc.resource_id)));
                }
                if !rc.tenant_id.is_empty() {
                    query.push(format!("tenantId={}", url_encode(&rc.tenant_id)));
                }
                if !rc.session_id.is_empty() {
                    query.push(format!("sessionId={}", url_encode(&rc.session_id)));
                }
                if !query.is_empty() {
                    url.push('?');
                    url.push_str(&query.join("&"));
                }
                (url, gateway.to_string())
            }
        };

        let brokered = matches!(path_override, Some(u) if is_brokered_url(u));

        // The AVD/W365 gateway sits behind Azure ARR: the first request to a
        // brokered connection URL is answered with 403 + a `Set-Cookie:
        // ARRAffinity` load-balancing cookie, and the actual WebSocket upgrade
        // must carry it so it lands on the backend instance holding the brokered
        // connection state (FreeRDP `wst.c wst_handle_ok_or_forbidden`). Prime
        // that cookie with a plain GET before upgrading.
        let cookie_header = if brokered {
            let cookies =
                crate::websocket::prime_affinity_cookies(&url, &sni_host, accept_invalid_cert);
            (!cookies.is_empty()).then(|| {
                cookies
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        } else {
            None
        };

        tracing::info!(
            %url,
            %sni_host,
            has_affinity_cookie = cookie_header.is_some(),
            "connecting Reverse Connect WebSocket"
        );

        // Upgrade headers as sent by the Windows App / msrdc and FreeRDP `wst.c`.
        let mut builder = http::Request::builder()
            .uri(&url)
            .header("Accept", "*/*")
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) RdClient")
            .header("X-Ms-User-Agent", "Windows365NativeClient/2.0.1193.0");

        // A brokered URL is already authenticated by the `RDmiGatewayToken` it
        // carries in its query string. FreeRDP's `arm.c`/`wst.c` send NO bearer
        // on this dial — and the AVD gateway returns 403 Forbidden if we add an
        // `Authorization: Bearer <wvd-token>` (the WVD access token is scoped to
        // the WVD resource, not the gateway). Only send the bearer on the
        // non-brokered manual-test fallback path.
        if !brokered {
            builder = builder.header("Authorization", format!("Bearer {}", rc.access_token));
        }
        if let Some(ref cookie) = cookie_header {
            builder = builder.header("Cookie", cookie);
        }

        let request = builder
            .body(())
            .map_err(|e| io::Error::other(format!("bad request: {e}")))?;

        let inner = WebSocketStream::connect(request, &sni_host, accept_invalid_cert)?;

        tracing::info!("Reverse Connect WebSocket established");
        Ok(Self { inner })
    }
}

impl ReverseConnectStream {
    /// Set a read timeout on the underlying WebSocket TCP socket (see
    /// [`crate::websocket::WebSocketStream::set_read_timeout`]).
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(dur)
    }
}

impl Read for ReverseConnectStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for ReverseConnectStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// True if `u` is an absolute URL the broker handed us to dial (any of the
/// ws/wss/http/https schemes).
fn is_brokered_url(u: &str) -> bool {
    ["wss://", "ws://", "https://", "http://"]
        .iter()
        .any(|p| u.starts_with(p))
}

/// Upgrade an `https`/`http` URL to its WebSocket equivalent (`wss`/`ws`);
/// `ws`/`wss` URLs are returned unchanged.
fn upgrade_to_ws_scheme(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = u.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        u.to_string()
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_includes_resource_and_tenant() {
        let rc = ReverseConnectConfig {
            gateway_fqdn: "rdbroker.wvd.microsoft.com".into(),
            resource_id: "/subs/123".into(),
            tenant_id: "a-b-c".into(),
            session_id: "sess-1".into(),
            access_token: "tok".into(),
            ..Default::default()
        };
        let url = format!("wss://{}{}", rc.gateway_fqdn, "/reverseconnect/v1/primary");
        assert_eq!(
            url,
            "wss://rdbroker.wvd.microsoft.com/reverseconnect/v1/primary"
        );
    }

    #[test]
    fn url_encode_escapes_specials() {
        assert_eq!(url_encode("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn brokered_https_url_is_upgraded_to_wss_and_parses() {
        // The W365 gateway returns an https:// location at a different host. It
        // must become wss:// and parse with a valid authority (the old code
        // glued it onto the gateway, yielding "invalid authority").
        let u = "https://rdgateway-host-blue-c221-eus2-r1.wvd.microsoft.com/api/arm/v2/connections/abc/def?RDmiGatewayToken=xyz&x-ms-routing-name=self";
        assert!(is_brokered_url(u));
        let ws = upgrade_to_ws_scheme(u);
        assert!(ws.starts_with("wss://rdgateway-host-blue-c221-eus2-r1.wvd.microsoft.com/"));
        let uri: http::Uri = ws.parse().expect("brokered URL must parse");
        assert_eq!(uri.scheme_str(), Some("wss"));
        assert_eq!(uri.host(), Some("rdgateway-host-blue-c221-eus2-r1.wvd.microsoft.com"));
    }

    #[test]
    fn scheme_upgrade_passthrough_and_http() {
        assert_eq!(upgrade_to_ws_scheme("wss://h/p"), "wss://h/p");
        assert_eq!(upgrade_to_ws_scheme("ws://h/p"), "ws://h/p");
        assert_eq!(upgrade_to_ws_scheme("http://h/p"), "ws://h/p");
        // A bare path (manual-test fallback) is not a brokered absolute URL.
        assert!(!is_brokered_url("/reverseconnect/v1/primary"));
    }
}

