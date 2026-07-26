//! DPAPI-encrypted on-disk cache for the AVD/W365 RDSTLS logon password.
//!
//! The RDSTLS v3 credential (see [`crate::rdstls_v3`]) encrypts the user's real
//! account password. Rather than require `--password` on every launch, the
//! password is prompted once (hidden), then cached at rest with Windows DPAPI
//! (`CryptProtectData`, per-user scope) under `%LOCALAPPDATA%\rdpio\`, keyed by the
//! account UPN. A wrong/changed password is cleared with `--w365-relogin`.

use std::io;
use std::path::PathBuf;

use crate::token_cache::{dpapi_protect, dpapi_unprotect};

const CACHE_FILE: &str = "w365_password.bin";

fn cache_path() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok().filter(|s| !s.is_empty())?;
    let dir = PathBuf::from(local).join("rdpio");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join(CACHE_FILE))
}

/// Cache `password` for `account` (DPAPI-encrypted). Best-effort: failures are
/// logged and ignored (caching is a convenience, not required to connect).
pub fn store(account: &str, password: &str) {
    let Some(path) = cache_path() else { return };
    let doc = serde_json::json!({ "v": 1, "account": account, "password": password });
    match dpapi_protect(doc.to_string().as_bytes()).and_then(|blob| std::fs::write(&path, blob)) {
        Ok(()) => tracing::info!(path = %path.display(), "cached Cloud PC password (DPAPI-encrypted)"),
        Err(e) => tracing::warn!(error = %e, "could not cache Cloud PC password"),
    }
}

/// Return the cached password for `account`, or `None` if there is no usable
/// cache (missing, undecryptable, or stored for a different account).
pub fn load(account: &str) -> Option<String> {
    let path = cache_path()?;
    let blob = std::fs::read(&path).ok()?;
    let plain = dpapi_unprotect(&blob).ok()?;
    let doc: serde_json::Value = serde_json::from_slice(&plain).ok()?;
    if doc.get("account").and_then(|v| v.as_str()) != Some(account) {
        return None;
    }
    doc.get("password")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Remove the cached password (e.g. `--w365-relogin`). A missing cache is not an
/// error.
pub fn clear() -> io::Result<()> {
    let Some(path) = cache_path() else { return Ok(()) };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
