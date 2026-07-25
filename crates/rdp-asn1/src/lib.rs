//! Minimal ASN.1 codec for the encodings RDP needs.
//!
//! The [`der`] module (Distinguished Encoding Rules) drives the CredSSP
//! `TSRequest` (MS-CSSP): a length codec plus a small TLV reader/writer covering
//! the tags CredSSP uses (SEQUENCE, INTEGER, OCTET STRING, EXPLICIT context
//! tags). The BER/PER that MCS and the GCC conference data (T.125 / T.124) need
//! is emitted inline by the `rdp-pdu` mcs/gcc modules, which only use a fixed
//! handful of encodings.
#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Asn1Error {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("integer overflow decoding length or value")]
    LengthOverflow,
    #[error("length not encoded in minimal form (DER violation)")]
    NonMinimalLength,
    #[error("indefinite lengths are not allowed in DER")]
    IndefiniteLength,
    #[error("unexpected tag: expected {expected:#04x}, found {found:#04x}")]
    UnexpectedTag { expected: u8, found: u8 },
}

pub mod der;
