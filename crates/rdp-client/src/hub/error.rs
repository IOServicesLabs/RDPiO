//! Hub error type.
//!
//! Every fallible hub operation (loading/writing the JSON stores, protecting and
//! unprotecting passwords with DPAPI, validating user input from the form) reports
//! through [`HubError`], so UI paths can log one typed error instead of juggling
//! `io::Error` / `serde_json::Error` / raw strings. Built on `thiserror`, matching
//! the rest of the workspace.

use std::fmt::Display;

/// Errors surfaced by the hub: persistence I/O, JSON (de)serialization, DPAPI
/// protect/unprotect, UUID handling, and invalid user input.
///
/// `dead_code` is allowed for now: `HubError` is the error type the later hub
/// steps (store, model, ui) return, and no binary path constructs it yet.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    /// Filesystem / atomic-rename / directory-creation failures on the
    /// `%LOCALAPPDATA%\rdpio\` stores.
    #[error("hub I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON parse or serialize failures on `connections.json` / `history.json`.
    #[error("hub JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// DPAPI `CryptProtectData` / `CryptUnprotectData` failures, including
    /// undecryptable blobs (wrong user / machine profile, tampered file).
    #[error("DPAPI error: {0}")]
    Dpapi(String),

    /// A Win32 API failure from the hub UI (window/control creation, GDI
    /// painting, common controls). The `windows` crate reports these as
    /// `windows::core::Error`; the UI paths convert them here so every hub
    /// failure surfaces as one typed error.
    #[error("Win32 error: {0}")]
    Win32(String),

    /// UUID generation / parsing failures on `ConnectionRecord::id`.
    #[error("UUID error: {0}")]
    Uuid(#[from] uuid::Error),

    /// User-supplied input that fails validation (empty host, out-of-range port,
    /// missing required field) or a record lookup that came up empty.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

#[allow(dead_code)]
impl HubError {
    /// Build a [`HubError::Dpapi`] from anything displayable (the `io::Error`
    /// text `CryptProtectData`/`CryptUnprotectData` already carry).
    pub fn dpapi(e: impl Display) -> Self {
        HubError::Dpapi(e.to_string())
    }

    /// Build a [`HubError::Win32`] from a `windows::core::Error` (or any
    /// displayable Win32 failure text).
    pub fn win32(e: impl Display) -> Self {
        HubError::Win32(e.to_string())
    }

    /// Build a [`HubError::InvalidInput`] from a message.
    pub fn invalid(msg: impl Into<String>) -> Self {
        HubError::InvalidInput(msg.into())
    }
}

impl From<&str> for HubError {
    fn from(s: &str) -> Self {
        HubError::InvalidInput(s.to_string())
    }
}

impl From<String> for HubError {
    fn from(s: String) -> Self {
        HubError::InvalidInput(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_variant_wraps_displayable_text() {
        let e = HubError::dpapi("CryptUnprotectData: access denied");
        assert!(e.to_string().contains("CryptUnprotectData"));
    }

    #[test]
    fn invalid_input_from_str_and_string() {
        let a: HubError = "empty host".into();
        let b: HubError = String::from("port out of range").into();
        assert!(a.to_string().contains("empty host"));
        assert!(b.to_string().contains("port out of range"));
    }

    #[test]
    fn io_and_json_convert_via_from() {
        let io = HubError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert!(matches!(io, HubError::Io(_)));
        let json = HubError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        assert!(matches!(json, HubError::Json(_)));
    }
}
