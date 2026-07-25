//! CredSSP / Network Level Authentication (MS-CSSP).
//!
//! We frame the CredSSP `TSRequest` (DER) ourselves on top of
//! [`rdp_asn1::der`] ([`tsrequest`]), extract the server's bound public key
//! from its TLS certificate ([`x509`]), and — on Windows — drive the full
//! SPNEGO/NTLM/Kerberos token exchange plus public-key binding through the OS
//! Security Support Provider Interface ([`sspi::authenticate`]).
#![cfg_attr(not(windows), allow(dead_code))]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NlaError {
    #[error("SSPI call failed: 0x{0:08X}")]
    Sspi(i32),
    #[error("CredSSP sequence error: {0}")]
    Sequence(String),
    #[error("server certificate public-key validation failed")]
    PubKeyMismatch,
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Asn1(#[from] rdp_asn1::Asn1Error),
}

pub mod tsrequest;

pub mod x509;

#[cfg(windows)]
pub mod sspi;

/// Portable CredSSP/NLA (NTLMv2) for non-Windows hosts — the counterpart to the
/// Win32 SSPI engine in [`sspi`]. Same `authenticate` entry-point signature.
#[cfg(not(windows))]
pub mod credssp;
