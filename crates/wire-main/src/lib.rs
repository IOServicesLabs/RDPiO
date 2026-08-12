//! wire-main — the RDP wire layer: transport, from-scratch PDU codec, the
//! connection sequence, and the client-facing [`WireSession`].
//!
//! Everything is implemented from scratch on top of the byte formats in
//! MS-RDPBCGR / T.125; the only in-workspace dependency is `rdp-crypto` for
//! the Standard RDP Security primitives (RC4, MD5, SHA-1, RSA, key derivation).
//! No third-party RDP library is used.
//!
//! This crate implements Standard RDP Security (the `--insecure` path): the
//! transport refuses to connect unless [`ConnectOptions::insecure`] is set,
//! because TLS/NLA would require a TLS stack this crate deliberately does not
//! vendor.

pub mod error;
pub mod handshake;
pub mod pdu;
pub mod session;
pub mod transport;

pub use error::{WireError, WireResult};
pub use session::{
    ChannelPurpose, ConnectOptions, Monitor, ServerPdu, SessionSettings, StaticChannel, WireSession,
};
pub use transport::{
    TransportOptions, UdpDelivery, UdpFrame, UdpRecv, UdpSequencer, UdpSideband, UdpStats,
    WireTransport,
};

/// Re-export the input event types so UI code can build input PDUs without
/// importing the codec module directly.
pub use pdu::{kbd, ptr, InputEvent};
