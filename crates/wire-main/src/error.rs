//! Error type shared by the transport, codec, and connection sequence.

use thiserror::Error;

/// Errors produced by the wire layer.
#[derive(Debug, Error)]
pub enum WireError {
    /// Underlying socket I/O failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The peer sent bytes that violate the RDP wire protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server rejected the X.224 security negotiation.
    #[error("server rejected the security negotiation (failure code {0:#x})")]
    NegotiationRejected(u32),
    /// The server requires a feature this client does not implement.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The peer closed the connection unexpectedly.
    #[error("connection closed by peer")]
    Closed,
    /// The connection sequence reached a state it cannot continue from.
    #[error("sequence error: {0}")]
    Sequence(String),
    /// The MAC signature on an incoming PDU did not verify.
    #[error("MAC verification failed")]
    BadMac,
}

/// Convenience alias.
pub type WireResult<T> = Result<T, WireError>;

/// Build a [`WireError::Protocol`] from a format string.
#[macro_export]
macro_rules! protocol_err {
    ($($arg:tt)*) => {
        $crate::error::WireError::Protocol(format!($($arg)*))
    };
}
