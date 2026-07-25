//! Encrypted on-disk cache for the W365 OAuth2 refresh token.
//!
//! Interactive W365 sign-in can require MFA on every launch. To avoid that, the
//! refresh token from a successful sign-in is cached and reused: the next launch
//! mints a fresh access token with the refresh-token grant (silent — no browser,
//! no MFA). Because the cache holds a long-lived credential, it is encrypted at
//! rest with Windows DPAPI (`CryptProtectData`, per-user scope — the same
//! primitive the Windows App uses) and stored under `%LOCALAPPDATA%\rdpio\`.

use core::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

use crate::w365::{self, AccessToken};

const CACHE_FILE: &str = "w365_token.bin";
/// Treat an access token as expired this many seconds before its real expiry, so
/// a connection is never started with a token about to lapse mid-handshake.
const EXPIRY_MARGIN: u64 = 300;

fn cache_path() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok().filter(|s| !s.is_empty())?;
    let dir = PathBuf::from(local).join("rdpio");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join(CACHE_FILE))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// DPAPI-encrypt `plain` for the current user; returns the opaque blob to store.
pub(crate) fn dpapi_protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, 0, &mut out_blob)
            .map_err(|e| io::Error::other(format!("CryptProtectData: {e}")))?;
        let out =
            std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut c_void)));
        Ok(out)
    }
}

/// DPAPI-decrypt a blob previously produced by [`dpapi_protect`].
pub(crate) fn dpapi_unprotect(blob: &[u8]) -> io::Result<Vec<u8>> {
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
            .map_err(|e| io::Error::other(format!("CryptUnprotectData: {e}")))?;
        let out =
            std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut c_void)));
        Ok(out)
    }
}

/// Persist the refresh (and current access) token for silent reuse. Best-effort:
/// failures are logged and ignored — caching is an optimisation, not required
/// for a working connection.
pub fn store(tenant: &str, client_id: Option<&str>, token: &AccessToken) {
    let Some(refresh) = token.refresh_token.as_deref() else {
        tracing::debug!("no refresh token in response; W365 credentials not cached");
        return;
    };
    let expires_at = now_unix() + token.expires_in.as_secs();
    let doc = serde_json::json!({
        "v": 1,
        "tenant": tenant,
        "client_id": client_id,
        "refresh_token": refresh,
        "access_token": token.token,
        "expires_at": expires_at,
        "username": token.username,
    });
    let path = match cache_path() {
        Some(p) => p,
        None => return,
    };
    match dpapi_protect(doc.to_string().as_bytes()).and_then(|blob| std::fs::write(&path, blob)) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "cached W365 refresh token (DPAPI-encrypted)")
        }
        Err(e) => tracing::warn!(error = %e, "could not cache W365 token"),
    }
}

/// Try to obtain a token without prompting the user, using the cached refresh
/// token. Returns `None` when there is no usable cache (so the caller falls back
/// to interactive sign-in). A still-valid cached access token is returned as-is;
/// otherwise the refresh-token grant is used and the cache is refreshed. A
/// rejected refresh token clears the cache and returns `None`.
pub fn load_silent(tenant: &str, client_id: Option<&str>) -> Option<AccessToken> {
    let path = cache_path()?;
    let blob = std::fs::read(&path).ok()?;
    let plain = match dpapi_unprotect(&blob) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "cached W365 token could not be decrypted; ignoring");
            return None;
        }
    };
    let doc: serde_json::Value = serde_json::from_slice(&plain).ok()?;

    // Only reuse a cache minted for the same tenant; a different `--tenant` must
    // authenticate against that directory.
    if doc.get("tenant").and_then(|v| v.as_str()) != Some(tenant) {
        tracing::debug!("cached W365 token is for a different tenant; ignoring");
        return None;
    }

    let username = doc
        .get("username")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_at = doc.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0);
    let refresh = doc.get("refresh_token").and_then(|v| v.as_str());

    // A still-valid access token can be used directly — no network at all.
    if let Some(access) = doc.get("access_token").and_then(|v| v.as_str()) {
        if !access.is_empty() && now_unix() + EXPIRY_MARGIN < expires_at {
            tracing::info!("reusing cached W365 access token (no sign-in / MFA needed)");
            return Some(AccessToken {
                token: access.to_string(),
                refresh_token: refresh.map(String::from),
                expires_in: Duration::from_secs(expires_at.saturating_sub(now_unix())),
                username,
            });
        }
    }

    // Otherwise mint a fresh access token from the refresh token (silent).
    let refresh = refresh?;
    tracing::info!("refreshing W365 access token from cached refresh token (no MFA)");
    match w365::refresh_token(tenant, client_id, refresh) {
        Ok(mut token) => {
            // Refresh responses may omit the id_token; keep the cached username.
            if token.username.is_none() {
                token.username = username;
            }
            store(tenant, client_id, &token);
            Some(token)
        }
        Err(e) => {
            tracing::warn!(error = %e, "cached refresh token rejected; interactive sign-in required");
            let _ = clear();
            None
        }
    }
}

/// Remove the cached token (e.g. `--w365-relogin`, or after the refresh token is
/// rejected). A missing cache is not an error.
pub fn clear() -> io::Result<()> {
    let Some(path) = cache_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
