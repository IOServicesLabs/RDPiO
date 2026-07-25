//! Wire-format PDUs for RDP, plus the codec traits everything is built on.
//!
//! This crate is **sans-I/O**: it only turns bytes into typed PDUs and back.
//! No sockets, no TLS, no OS calls — which is what lets the protocol logic be
//! unit-tested on any platform.
#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PduError {
    #[error("need more data: have {have} bytes, need at least {needed}")]
    NotEnoughData { have: usize, needed: usize },
    #[error("invalid value in field `{field}`: {detail}")]
    InvalidField { field: &'static str, detail: String },
    #[error("unsupported {what}: {value}")]
    Unsupported { what: &'static str, value: String },
    #[error(transparent)]
    Asn1(#[from] rdp_asn1::Asn1Error),
}

pub type PduResult<T> = Result<T, PduError>;

/// A type that can be written to the wire.
pub trait Encode {
    /// Append the encoded form to `dst`.
    fn encode(&self, dst: &mut Vec<u8>) -> PduResult<()>;

    /// Exact number of bytes [`Encode::encode`] will append. Used to size
    /// buffers and back-fill length prefixes without a second pass.
    fn encoded_len(&self) -> usize;
}

/// A type that can be parsed from the wire.
///
/// Decoders take `&mut &[u8]` as a cursor: on success the slice is advanced
/// past the consumed bytes; on failure the cursor position is unspecified.
pub trait Decode<'de>: Sized {
    fn decode(src: &mut &'de [u8]) -> PduResult<Self>;
}

/// Ensure at least `needed` bytes remain in `src`, else [`PduError::NotEnoughData`].
#[inline]
pub fn ensure(src: &[u8], needed: usize) -> PduResult<()> {
    if src.len() < needed {
        Err(PduError::NotEnoughData {
            have: src.len(),
            needed,
        })
    } else {
        Ok(())
    }
}

// --- Protocol layers --------------------------------------------------------

pub mod x224;

pub mod gcc;

pub mod mcs;

pub mod security;

pub mod autodetect;

pub mod capabilities;

pub mod finalization;

pub mod errinfo;

pub mod logon;

pub mod license;

pub mod input;

pub mod gfx;

pub mod fastpath;

pub mod multitransport;

pub mod redirection;

pub mod rdpudp;

pub mod rdstls;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_reports_shortfall() {
        let buf = [0u8; 2];
        let err = ensure(&buf, 4).unwrap_err();
        assert!(matches!(
            err,
            PduError::NotEnoughData { have: 2, needed: 4 }
        ));
        assert!(ensure(&buf, 2).is_ok());
    }
}
