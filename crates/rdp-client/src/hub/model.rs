//! Hub data model: saved connections, DPAPI-protected passwords, MRU history,
//! and the connection target handed to the existing connect bootstrap.
//!
//! Everything in this module is plain data plus small, pure logic so it can be
//! unit-tested without a window. The only platform-specific dependency is DPAPI
//! (`CryptProtectData` / `CryptUnprotectData`, via [`crate::token_cache`]),
//! which is deliberately confined to [`ProtectedPassword`] — no other type in
//! this module ever holds or touches a plaintext password.
//!
//! Serialization contract: [`ConnectionRecord`] and [`MruRecord`] are persisted
//! as JSON under `%LOCALAPPDATA%\rdpio\` by `hub::store` (step-4). A password
//! is only ever the base64 encoding of the opaque DPAPI blob, so the on-disk
//! JSON contains no plaintext. [`ConnectionTarget`] keeps its password in
//! memory only and marks it `#[serde(skip)]`, so even a blanket serialization
//! cannot write it out.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::hub::{dpapi_protect, dpapi_unprotect, HubError};

/// Unix seconds since the epoch — the timestamp convention already used by
/// `token_cache.rs` / `password_cache.rs`. Exposed to `hub::store` (step-4) for
/// `created_at` / `updated_at` / `last_connected_at`.
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A password encrypted at rest with Windows DPAPI, held as the base64 encoding
/// of the opaque `CryptProtectData` blob. The plaintext exists only transiently
/// inside [`ProtectedPassword::protect`] / [`ProtectedPassword::unprotect`];
/// the value stored by this struct is never the password itself.
///
/// Serializes as a bare base64 string, so `connections.json` never contains a
/// recoverable password outside the current user's DPAPI context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedPassword(String);

impl ProtectedPassword {
    /// DPAPI-protect `plain` for the current user and keep the base64-encoded
    /// blob. This is the only entry point that ever sees the plaintext.
    pub fn protect(plain: &str) -> Result<Self, HubError> {
        let blob = dpapi_protect(plain.as_bytes()).map_err(HubError::dpapi)?;
        Ok(Self::from_blob(&blob))
    }

    /// Build directly from an already-encrypted DPAPI blob. No plaintext ever
    /// passes through this path — used when constructing records in tests or
    /// when a password was protected outside the record flow.
    pub fn from_blob(blob: &[u8]) -> Self {
        Self(B64.encode(blob))
    }

    /// The base64-encoded DPAPI blob (still encrypted). Useful for store-level
    /// tests that must assert "no plaintext on disk" without decrypting.
    pub fn as_base64(&self) -> &str {
        &self.0
    }

    /// The raw DPAPI blob, still encrypted (base64-decoded). No decryption.
    pub fn blob(&self) -> Result<Vec<u8>, HubError> {
        B64.decode(&self.0).map_err(|e| HubError::dpapi(format!("base64 decode: {e}")))
    }

    /// Recover the plaintext password. This is the only place the password
    /// exists in memory; callers must never log or serialize the result.
    pub fn unprotect(&self) -> Result<String, HubError> {
        let plain = dpapi_unprotect(&self.blob()?).map_err(HubError::dpapi)?;
        String::from_utf8(plain)
            .map_err(|_| HubError::dpapi("decrypted password is not valid UTF-8"))
    }
}

/// A saved connection, persisted as JSON. The only credential field is
/// [`saved_password`](ConnectionRecord::saved_password), a DPAPI-protected
/// blob — serializing this struct never emits the plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    /// Stable unique id (uuid v4). Also the `connection_id` that MRU records
    /// link to so a Recent entry can reuse a saved password.
    pub id: uuid::Uuid,
    /// Human-readable name shown in the hub's Saved list and the connection bar.
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// DPAPI-protected password — never the plaintext.
    pub saved_password: ProtectedPassword,
    /// Unix seconds at creation.
    pub created_at: u64,
    /// Unix seconds of the most recent upsert.
    pub updated_at: u64,
}

/// One most-recently-used connection. Passwords are deliberately absent: an MRU
/// entry only carries an optional `connection_id` link to a saved record when
/// one exists; otherwise the existing `prompt.rs` flow asks at connect time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MruRecord {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub display_name: String,
    /// Unix seconds of the most recent successful connection.
    pub last_connected_at: u64,
    /// How many times this host/user combination has been connected.
    pub connect_count: u64,
    /// Links to a saved [`ConnectionRecord::id`] when the connection came from
    /// the Saved list; `None` for CLI/prompted connections without a saved
    /// entry.
    pub connection_id: Option<uuid::Uuid>,
}

impl MruRecord {
    /// Build a fresh MRU entry (count 1, timestamp now) for a first connection.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: Option<String>,
        domain: Option<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username,
            domain,
            display_name: display_name.into(),
            last_connected_at: now_unix(),
            connect_count: 1,
            connection_id: None,
        }
    }

    /// Record one successful connection: bump the counter and refresh the
    /// timestamp. Called by `MruStore::record` (step-4) on every reconnect, so
    /// the same host/user climbs the count and moves to the top of the list.
    pub fn touch(&mut self) {
        self.last_connected_at = now_unix();
        self.connect_count += 1;
    }
}

/// Everything the existing connect bootstrap needs for one session, produced by
/// the hub UI and consumed by `main.rs` (step-8). The password exists only in
/// memory — the field is `#[serde(skip)]` so even a blanket serialization
/// cannot write it out, and deserialization always yields `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionTarget {
    /// Display name (also used for the MRU entry and the connection bar).
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Plaintext password, in memory only. Never serialized.
    #[serde(skip)]
    pub password: Option<String>,
    /// Set when the target came from a Saved entry; step-9 records it on the
    /// MRU entry so a later Recent launch can reuse the saved password.
    #[serde(skip)]
    pub connection_id: Option<uuid::Uuid>,
}

impl ConnectionTarget {
    /// Minimal target for a bare host[:port]; the display name defaults to the
    /// host and all optional fields start unset.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        let host = host.into();
        Self {
            display_name: host.clone(),
            host,
            port,
            username: None,
            domain: None,
            password: None,
            connection_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Protect a plaintext password with the real DPAPI primitive (per-user
    /// scope), mirroring what the store does for every upsert.
    fn protect(plain: &str) -> ProtectedPassword {
        ProtectedPassword::protect(plain).expect("DPAPI protect must succeed in tests")
    }

    #[test]
    fn protected_password_round_trips() {
        let p = protect("hunter2-秘密-🔑");
        assert_eq!(p.unprotect().unwrap(), "hunter2-秘密-🔑");
        // The held value is a base64 blob, never the plaintext.
        assert_ne!(p.as_base64(), "hunter2-秘密-🔑");
        assert!(!p.as_base64().is_empty());
        // The blob method returns the still-encrypted bytes without decrypting.
        let blob = p.blob().unwrap();
        assert!(!blob.is_empty());
        assert_ne!(blob, "hunter2-秘密-🔑".as_bytes());
    }

    #[test]
    fn protected_password_serializes_as_opaque_blob() {
        let p = protect("SuperSecret123!");
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("SuperSecret123!"),
            "serialized ProtectedPassword leaked the plaintext: {json}"
        );
        let back: ProtectedPassword = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.unprotect().unwrap(), "SuperSecret123!");
    }

    #[test]
    fn connection_record_json_contains_no_plaintext() {
        let record = ConnectionRecord {
            id: uuid::Uuid::new_v4(),
            display_name: "Work PC".into(),
            host: "10.0.0.5".into(),
            port: 3389,
            username: Some("alice".into()),
            domain: Some("CORP".into()),
            saved_password: protect("CorrectHorseBatteryStaple"),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            !json.contains("CorrectHorseBatteryStaple"),
            "connections JSON leaked the plaintext: {json}"
        );
        // Round-trip preserves the record and its recoverable password.
        let parsed: ConnectionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, record);
        assert_eq!(
            parsed.saved_password.unprotect().unwrap(),
            "CorrectHorseBatteryStaple"
        );
    }

    #[test]
    fn mru_touch_increments_count_and_refreshes_time() {
        let mut m = MruRecord {
            host: "host.example".into(),
            port: 3390,
            username: Some("bob".into()),
            domain: None,
            display_name: "Host Example".into(),
            last_connected_at: 100,
            connect_count: 3,
            connection_id: None,
        };
        let before = m.last_connected_at;
        m.touch();
        assert_eq!(m.connect_count, 4, "touch must increment the counter");
        assert!(
            m.last_connected_at >= before,
            "touch must refresh the timestamp"
        );
        assert!(m.last_connected_at > 1_700_000_000, "timestamp should be unix 'now'");
        m.touch();
        assert_eq!(m.connect_count, 5);
        assert!(m.last_connected_at >= 1_700_000_000);
    }

    #[test]
    fn mru_serializes_without_any_password_field() {
        let mut m = MruRecord::new("box.local", 3389, Some("u".into()), None, "Box");
        m.touch();
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("password"),
            "MRU JSON must never contain a password field: {json}"
        );
        let back: MruRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn connection_target_serialization_never_emits_password() {
        let t = ConnectionTarget {
            display_name: "Box".into(),
            host: "box.local".into(),
            port: 3389,
            username: Some("u".into()),
            domain: None,
            password: Some("PLAINTEXT-SECRET".into()),
            connection_id: Some(uuid::Uuid::new_v4()),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(
            !json.contains("PLAINTEXT-SECRET"),
            "serialized ConnectionTarget leaked the plaintext: {json}"
        );
        assert!(!json.contains("password"));
        // The in-memory-only fields are dropped by (de)serialization by design.
        let back: ConnectionTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.password, None);
        assert_eq!(back.connection_id, None);
        assert_eq!(back.host, "box.local");
    }

    #[test]
    fn connection_target_new_defaults_sane() {
        let t = ConnectionTarget::new("10.1.2.3", 3390);
        assert_eq!(t.host, "10.1.2.3");
        assert_eq!(t.port, 3390);
        assert_eq!(t.display_name, "10.1.2.3");
        assert_eq!(t.username, None);
        assert_eq!(t.password, None);
        assert_eq!(t.connection_id, None);
    }
}
