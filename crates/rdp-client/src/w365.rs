//! Windows 365 / Azure Virtual Desktop modern authentication.
//!
//! W365 does not use CredSSP/NTLM. Instead the client obtains an OAuth2 access
//! token (via device-code flow) and passes it to the RDP stack, typically
//! through the RDWeb feed and then as the logon password in the Client Info PDU.
//!
//! This module implements the device-code grant from RFC 8628 against the
//! Microsoft identity platform. It is intentionally synchronous so it fits the
//! existing CLI/GUI startup flow without pulling in an async runtime.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

// Azure Virtual Desktop / Windows 365 client application ID. This is the
// first-party app used by the Windows App / Microsoft Remote Desktop clients
// when authenticating to AVD/W365 resources. It can be overridden with
// `--client-id` if a tenant requires a different registration.
const DEFAULT_CLIENT_ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";
// The AVD/W365 feed needs a token for the Windows Virtual Desktop resource
// (app id 9cdead84-a844-4324-93f2-b2e6bb768d07, identifier URI
// https://www.wvd.microsoft.com), NOT Azure Resource Manager. The Remote
// Desktop client (DEFAULT_CLIENT_ID) is preauthorized for this resource, so
// `.default` works without an admin consent prompt.
const DEFAULT_SCOPE: &str = "https://www.wvd.microsoft.com/.default offline_access";

/// Result of a completed authentication (device-code or authorization-code).
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
    #[allow(dead_code)]
    pub expires_in: Duration,
    /// User principal name parsed from the `id_token`, when an OpenID Connect
    /// scope (`openid profile`) was requested. Used to default the RDSTLS logon
    /// username so the user need not pass `--user`.
    pub username: Option<String>,
}

/// In-flight OAuth2 device-code flow. The caller displays `verification_uri`
/// (optionally with `user_code` pre-filled) to the user and polls
/// [`DeviceCodeFlow::poll`] until it returns a token or expires.
#[derive(Debug, Clone)]
pub struct DeviceCodeFlow {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub expires_in: Duration,
    pub interval: Duration,
    tenant: String,
    client_id: String,
    #[allow(dead_code)]
    scope: String,
}

impl DeviceCodeFlow {
    /// Poll the token endpoint once. Returns `Ok(None)` while the user has not
    /// yet completed the prompt; returns `Ok(Some(token))` once authentication
    /// succeeds. Errors are terminal.
    pub fn poll_once(&self) -> Result<Option<AccessToken>, AuthError> {
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant
        );

        // Microsoft returns HTTP 400 while the user has not yet completed the
        // prompt, with an `error` field such as `authorization_pending`. We must
        // read the body in that case instead of treating the status as fatal.
        let http_resp = ureq::post(&token_url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&form_encode(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &self.client_id),
                ("device_code", &self.device_code),
            ]));

        let token_resp: serde_json::Value = match http_resp {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                tracing::debug!(status = code, %body, "token poll returned non-success status");
                serde_json::from_str(&body)?
            }
            Err(e) => return Err(e.into()),
        };

        if let Some(err) = token_resp.get("error") {
            match err.as_str() {
                Some("authorization_pending") => return Ok(None),
                Some("authorization_declined") => {
                    return Err(AuthError::Failed("authorization declined".into()));
                }
                Some("expired_token") => {
                    tracing::error!("device code expired before authentication completed");
                    return Err(AuthError::Expired);
                }
                Some("bad_verification_code") => return Ok(None),
                Some(other) => {
                    let desc = token_resp["error_description"].as_str().unwrap_or("");
                    tracing::error!(error = %other, %desc, "token endpoint returned a terminal error");
                    return Err(AuthError::Failed(format!("{other}: {desc}")));
                }
                None => return Err(AuthError::Failed("unknown token error".into())),
            }
        }

        let token = token_resp["access_token"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing access_token".into()))?
            .to_string();
        let refresh_token = token_resp["refresh_token"].as_str().map(String::from);
        let expires_in = token_resp["expires_in"].as_u64().unwrap_or(3600);

        Ok(Some(AccessToken {
            token,
            refresh_token,
            expires_in: Duration::from_secs(expires_in),
            username: token_resp["id_token"].as_str().and_then(parse_id_token_upn),
        }))
    }

    /// Block and poll the token endpoint until the user completes the prompt
    /// or the flow expires.
    pub fn poll(&self) -> Result<AccessToken, AuthError> {
        let deadline = Instant::now() + self.expires_in;
        loop {
            thread::sleep(self.interval);
            if Instant::now() > deadline {
                return Err(AuthError::Expired);
            }
            if let Some(token) = self.poll_once()? {
                return Ok(token);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("network error during authentication: {0}")]
    Network(#[from] ureq::Error),
    #[error("I/O error reading authentication response: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("authentication failed: {0}")]
    Failed(String),
    #[error("device code expired before authentication completed")]
    Expired,
    #[allow(dead_code)]
    #[error("authorization pending; user has not completed the prompt")]
    Pending,
}

/// Authenticate via OAuth2 device-code flow.
///
/// `tenant` is the Microsoft tenant id or `common`/`organizations`. The default
/// `client_id` is the Windows Virtual Desktop / Microsoft Remote Desktop client
/// id; override it if your tenant requires a different application registration.
///
/// This function blocks, prints the user code/verification URL via `tracing`,
/// and polls the token endpoint until the user completes the prompt or the code
/// expires.
pub fn authenticate_device_code(
    tenant: &str,
    client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<AccessToken, AuthError> {
    let flow = start_device_code_flow(tenant, client_id, scope)?;
    tracing::info!(
        user_code = %flow.user_code,
        verification_uri = %flow.verification_uri,
        "complete authentication in your browser, then return to rdpio"
    );
    flow.poll()
}

/// Start an OAuth2 device-code flow and return the in-flight context.
///
/// The caller is responsible for showing `verification_uri` to the user (with
/// `user_code` pre-filled if desired) and calling [`DeviceCodeFlow::poll`].
pub fn start_device_code_flow(
    tenant: &str,
    client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<DeviceCodeFlow, AuthError> {
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID).to_string();
    let scope = scope.unwrap_or(DEFAULT_SCOPE).to_string();

    let device_url = format!(
        "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode"
    );

    tracing::info!(%device_url, %client_id, %scope, "requesting OAuth2 device code");

    let http_resp = ureq::post(&device_url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form_encode(&[
            ("client_id", &client_id),
            ("scope", &scope),
        ]));

    let resp: serde_json::Value = match http_resp {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            tracing::error!(status = code, %body, "device-code endpoint returned an error");
            return Err(AuthError::Failed(format!(
                "device-code endpoint returned {code}: {body}"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    if let Some(err) = resp.get("error") {
        let desc = resp["error_description"].as_str().unwrap_or("");
        tracing::error!(error = %err, %desc, "device-code endpoint returned OAuth error");
        return Err(AuthError::Failed(format!(
            "{}: {}",
            err.as_str().unwrap_or("unknown"),
            desc
        )));
    }

    Ok(DeviceCodeFlow {
        user_code: resp["user_code"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing user_code".into()))?
            .to_string(),
        device_code: resp["device_code"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing device_code".into()))?
            .to_string(),
        verification_uri: resp["verification_uri"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing verification_uri".into()))?
            .to_string(),
        expires_in: Duration::from_secs(resp["expires_in"].as_u64().unwrap_or(900)),
        interval: Duration::from_secs(resp["interval"].as_u64().unwrap_or(5).max(1)),
        tenant: tenant.to_string(),
        client_id,
        scope,
    })
}

fn form_encode(items: &[(&str, &str)]) -> String {
    items
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
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

/// Refresh an access token with the refresh token, if available.
#[allow(dead_code)]
pub fn refresh_token(
    tenant: &str,
    client_id: Option<&str>,
    refresh: &str,
) -> Result<AccessToken, AuthError> {
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID);
    let token_url = format!(
        "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
    );

    let mut body = HashMap::new();
    body.insert("grant_type", "refresh_token");
    body.insert("client_id", client_id);
    body.insert("refresh_token", refresh);

    let resp: serde_json::Value = ureq::post(&token_url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect::<Vec<_>>()
            .join("&"))?
        .into_json()?;

    if let Some(err) = resp.get("error") {
        return Err(AuthError::Failed(err.as_str().unwrap_or("unknown").into()));
    }

    Ok(AccessToken {
        token: resp["access_token"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing access_token".into()))?
            .to_string(),
        refresh_token: resp["refresh_token"].as_str().map(String::from),
        expires_in: Duration::from_secs(resp["expires_in"].as_u64().unwrap_or(3600)),
        username: resp["id_token"].as_str().and_then(parse_id_token_upn),
    })
}

// --- OAuth2 authorization-code flow (RFC 6749 §4.1) -------------------------
//
// The AVD/W365 first-party client (`DEFAULT_CLIENT_ID`) is registered as a
// public client for the authorization-code grant with the native redirect URI
// below, but it does NOT have the device-code (mobile & desktop) grant enabled —
// a device-code token request is rejected with `invalid_client` (AADSTS7000218).
// FreeRDP and the Windows App therefore use the authorization-code flow: the
// login page is shown in an embedded WebView, the browser is redirected to the
// `nativeclient` URL carrying `?code=...`, and that code is exchanged for a
// token. No client secret and no PKCE — exactly as FreeRDP's `client.c` does.

/// Native-client redirect URI registered for `DEFAULT_CLIENT_ID`. AAD redirects
/// the browser here with the authorization code; the WebView intercepts it.
pub const NATIVE_REDIRECT_URI: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";
/// Scope for the authorization-code flow. `openid profile` yield an `id_token`
/// (so we can default the logon username); `offline_access` yields a refresh
/// token. Matches FreeRDP's `GatewayAvdScope` default.
const AUTH_CODE_SCOPE: &str =
    "https://www.wvd.microsoft.com/.default openid profile offline_access";

/// Build the authorization-code request URL to load in the login WebView.
pub fn build_authorize_url(tenant: &str, client_id: Option<&str>, scope: Option<&str>) -> String {
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID);
    let scope = scope.unwrap_or(AUTH_CODE_SCOPE);
    format!(
        "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize\
         ?client_id={cid}&response_type=code&scope={scope}&redirect_uri={redir}",
        tenant = tenant,
        cid = url_encode(client_id),
        scope = url_encode(scope),
        redir = url_encode(NATIVE_REDIRECT_URI),
    )
}

/// Exchange an authorization `code` (captured from the `nativeclient` redirect)
/// for an access token at the token endpoint.
pub fn exchange_auth_code(
    tenant: &str,
    client_id: Option<&str>,
    scope: Option<&str>,
    code: &str,
) -> Result<AccessToken, AuthError> {
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID);
    let scope = scope.unwrap_or(AUTH_CODE_SCOPE);
    let token_url =
        format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");

    tracing::info!(%token_url, "exchanging authorization code for token");

    let http_resp = ureq::post(&token_url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form_encode(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("scope", scope),
            ("redirect_uri", NATIVE_REDIRECT_URI),
        ]));

    let resp: serde_json::Value = match http_resp {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(status, r)) => {
            let body = r.into_string().unwrap_or_default();
            tracing::error!(status, %body, "token endpoint rejected authorization code");
            serde_json::from_str(&body)?
        }
        Err(e) => return Err(e.into()),
    };

    if let Some(err) = resp.get("error") {
        let desc = resp["error_description"].as_str().unwrap_or("");
        return Err(AuthError::Failed(format!(
            "{}: {}",
            err.as_str().unwrap_or("unknown"),
            desc
        )));
    }

    Ok(AccessToken {
        token: resp["access_token"]
            .as_str()
            .ok_or_else(|| AuthError::Failed("missing access_token".into()))?
            .to_string(),
        refresh_token: resp["refresh_token"].as_str().map(String::from),
        expires_in: Duration::from_secs(resp["expires_in"].as_u64().unwrap_or(3600)),
        username: resp["id_token"].as_str().and_then(parse_id_token_upn),
    })
}

/// Extract the user principal name from an OIDC `id_token` (the unverified JWT
/// payload — we only read a display/login hint from it, we do not trust it for
/// authorization). Prefers `upn`, then `preferred_username`, then `email`.
fn parse_id_token_upn(id_token: &str) -> Option<String> {
    let payload_b64 = id_token.split('.').nth(1)?;
    let bytes = crate::arm_broker::decode_b64(payload_b64)?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    for key in ["upn", "preferred_username", "email", "unique_name"] {
        if let Some(v) = claims.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

// The modern AVD/W365 feed discovery endpoint is ARM-based. The non-ARM
// `/api/feeddiscovery` path returns 404/redirect for Entra-ID tenants.
const DEFAULT_FEED_URL: &str = "https://rdweb.wvd.microsoft.com/api/arm/feeddiscovery";

/// Fetch the W365/AVD feed for `tenant_id` using the authenticated access token.
///
/// `client_id` is the AAD application id used for the feed request; `None`
/// uses the same default as device-code authentication.
pub fn fetch_feed(
    token: &AccessToken,
    tenant_id: &str,
    client_id: Option<&str>,
    feed_url: Option<&str>,
) -> Result<Vec<crate::feed::FeedEntry>, AuthError> {
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID);
    let url = feed_url.map(String::from).unwrap_or_else(|| {
        format!(
            "{}?tenantId={}&appId={}",
            DEFAULT_FEED_URL,
            url_encode(tenant_id),
            url_encode(client_id)
        )
    });

    tracing::info!(%url, "fetching W365 feed");
    let auth = format!("Bearer {}", token.token);
    let body = ureq::get(&url)
        .set("Authorization", &auth)
        .set("Accept", "application/json, application/xml, text/*")
        .call()?
        .into_string()?;

    crate::feed::parse(&body).map_err(|e| AuthError::Failed(format!("feed parse error: {e}")))
}

/// Package family name of the Microsoft "Windows App" (formerly Windows 365)
/// MSIX package. Its `LocalCache\ResourceCache` holds one signed ARM `.rdp` per
/// subscribed Cloud PC.
const WINDOWS_APP_PACKAGE: &str = "MicrosoftCorporationII.Windows365_8wekyb3d8bbwe";

/// Discover the user's Cloud PCs from the Windows App's local resource cache.
///
/// The Windows App stores every subscribed resource as `LocalCache\ResourceCache\
/// <id>.rdp`, each a JSON envelope `{"cached_item":"<.rdp contents>", ...}` whose
/// payload is a signed ARM Reverse-Connect `.rdp` (`resourceprovider:arm`,
/// `gatewayhostname`, `loadbalanceinfo`). We surface each as a [`FeedEntry`] whose
/// `rdp_file` carries the payload verbatim, so selection drives the same validated
/// ARM-broker path as `--rdp-file`. Returns an empty list if the Windows App is
/// not installed / has never subscribed. Duplicate entries (the same Cloud PC
/// cached under several ids) are collapsed by `loadbalanceinfo`.
pub fn discover_cached_cloud_pcs() -> Vec<crate::feed::FeedEntry> {
    let local_appdata = match std::env::var("LOCALAPPDATA") {
        Ok(p) if !p.is_empty() => p,
        _ => return Vec::new(),
    };
    let dir = std::path::Path::new(&local_appdata)
        .join("Packages")
        .join(WINDOWS_APP_PACKAGE)
        .join("LocalCache")
        .join("ResourceCache");

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "no Windows App resource cache");
            return Vec::new();
        }
    };

    let mut entries = Vec::new();
    let mut seen_lbi = std::collections::HashSet::new();
    for dirent in read_dir.flatten() {
        let path = dirent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rdp") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "skip unreadable cache file");
                continue;
            }
        };
        // The cache file is a JSON envelope; the actual `.rdp` is `cached_item`.
        let rdp_contents = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => v
                .get("cached_item")
                .and_then(|c| c.as_str())
                .map(String::from),
            // Tolerate a bare `.rdp` (some builds wrote them unwrapped).
            Err(_) if raw.contains("resourceprovider") => Some(raw.clone()),
            Err(_) => None,
        };
        let Some(rdp_contents) = rdp_contents else { continue };

        let settings = crate::feed::parse_rdp_file(&rdp_contents);
        // Only ARM Reverse-Connect resources can be brokered by rdpio.
        if settings.get("resourceprovider").map(String::as_str) != Some("arm") {
            continue;
        }
        let lbi = match settings.get("loadbalanceinfo") {
            Some(l) if !l.is_empty() => l.clone(),
            _ => continue,
        };
        if !seen_lbi.insert(lbi.clone()) {
            continue; // same Cloud PC under a different cache id
        }

        let mut entry = crate::feed::FeedEntry::default();
        entry.display_name = settings
            .get("remotedesktopname")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| "Cloud PC".to_string());
        // `remoteapplicationprogram` is `||<resourceId>`; the GUID distinguishes
        // Cloud PCs that share a SKU display name. Used only as a picker label.
        entry.resource_id = settings
            .get("remoteapplicationprogram")
            .map(|s| s.trim_start_matches('|').to_string())
            .unwrap_or_default();
        entry.tenant_id = settings.get("aadtenantid").cloned().unwrap_or_default();
        entry.gateway_fqdn = settings.get("gatewayhostname").cloned().unwrap_or_default();
        entry.load_balance_info = Some(lbi.into_bytes());
        entry.rdp_file = Some(rdp_contents);
        entries.push(entry);
    }

    entries.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.resource_id.cmp(&b.resource_id))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_escapes_space_and_slash() {
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn form_encode_joins_pairs() {
        assert_eq!(
            form_encode(&[("client_id", "id"), ("scope", "a b")]),
            "client_id=id&scope=a%20b"
        );
    }

    #[test]
    fn authorize_url_uses_code_flow_and_native_redirect() {
        let url = build_authorize_url("common", None, None);
        assert!(url.starts_with(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"
        ));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={DEFAULT_CLIENT_ID}")));
        // redirect_uri is URL-encoded.
        assert!(url.contains("redirect_uri=https%3A%2F%2Flogin.microsoftonline.com%2Fcommon%2Foauth2%2Fnativeclient"));
        // wvd scope present (encoded).
        assert!(url.contains("www.wvd.microsoft.com"));
    }

    #[test]
    fn id_token_upn_parsed_from_jwt_payload() {
        // header.payload.signature — only the payload matters. Payload base64url
        // of {"preferred_username":"nick@contoso.com"} (no padding).
        let payload = "eyJwcmVmZXJyZWRfdXNlcm5hbWUiOiJuaWNrQGNvbnRvc28uY29tIn0";
        let jwt = format!("aaa.{payload}.bbb");
        assert_eq!(parse_id_token_upn(&jwt).as_deref(), Some("nick@contoso.com"));
    }
}
