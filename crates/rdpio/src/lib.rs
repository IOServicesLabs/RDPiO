//! RDPiO client assembly.
//!
//! The `rdpio` binary itself lives in `crates/rdp-client` (`src/main.rs`,
//! package `rdp-client`); this crate is the workspace's *assembly point*. It
//! depends on every library crate in the workspace — protocol
//! ([`rdp_pdu`], [`rdp_core`], [`rdp_nla`], [`rdp_channels`]), codec
//! ([`rdp_asn1`], [`rdp_pdu`]), transport ([`wire_main`]), and platform
//! ([`rdp_gpu`], [`rdp_graphics`], [`rdp_webrtc`]) — re-exports them all under
//! one namespace, and adds the constructor glue ([`Client`]) that assembles the
//! sans-I/O connection state machine ([`rdp_core::Connector`]) with the
//! TPKT/X.224 wire framing ([`rdp_pdu::x224`]) so a full client can drive the
//! RDP connection setup over any `Read + Write` transport: a real socket, a
//! TLS tunnel, or an in-memory pipe in tests.

pub use rdp_asn1;
pub use rdp_channels;
pub use rdp_client;
pub use rdp_core;
pub use rdp_crypto;
pub use rdp_gpu;
pub use rdp_graphics;
pub use rdp_nla;
pub use rdp_pdu;
pub use rdp_webrtc;
pub use wire_main;

// The types a caller needs to construct and drive a client.
pub use rdp_core::{ClientConfig, Connector, CoreError, Credentials, Phase};
pub use rdp_pdu::x224::SecurityProtocol;

use std::io::{self, Read, Write};

use thiserror::Error;

/// Errors from driving the client's connection setup.
#[derive(Debug, Error)]
pub enum ClientError {
    /// A PDU failed to encode/decode.
    #[error("protocol error: {0}")]
    Pdu(#[from] rdp_pdu::PduError),
    /// The connection state machine rejected the exchange.
    #[error("connection sequence error: {0}")]
    Core(#[from] rdp_core::CoreError),
    /// The underlying transport failed.
    #[error("transport error: {0}")]
    Io(#[from] io::Error),
}

/// The RDP client: a sans-I/O connection state machine wrapped with the
/// TPKT/X.224 wire framing needed to drive the handshake over any
/// [`Read`] + [`Write`] transport.
///
/// The connector holds *only* protocol state ([`rdp_core::Connector`]); the
/// caller owns the transport (socket, TLS tunnel, or in-memory pipe) and feeds
/// it through [`Self::drive_negotiation`], which sends the X.224 Connection
/// Request, reads the server's TPKT-framed Connection Confirm, and advances
/// the handshake.
pub struct Client {
    connector: Connector,
}

impl Client {
    /// Assemble a client from a [`ClientConfig`].
    pub fn new(config: ClientConfig) -> Self {
        Self {
            connector: Connector::new(config),
        }
    }

    /// Where the connection sequence currently is.
    pub fn phase(&self) -> Phase {
        self.connector.phase()
    }

    /// Access to the underlying sans-I/O state machine.
    pub fn connector(&self) -> &Connector {
        &self.connector
    }

    /// Mutable access to the underlying sans-I/O state machine (for the legacy
    /// Standard RDP Security fallback via `set_requested_protocols`).
    pub fn connector_mut(&mut self) -> &mut Connector {
        &mut self.connector
    }

    /// The first bytes to send: the TPKT-framed X.224 Connection Request
    /// advertising the configured security protocols.
    pub fn initial_request(&self) -> Vec<u8> {
        self.connector.initial_request()
    }

    /// Drive the X.224 negotiation over `transport`: write the Connection
    /// Request, read the server's TPKT-framed Connection Confirm, and advance
    /// the state machine. Returns the security protocol the server selected.
    ///
    /// This is the transport-boundary crossing step: a successful return means
    /// the request crossed the transport and the reply was interpreted at the
    /// protocol layer — a rejection surfaces as [`ClientError::Core`], not as
    /// an I/O error.
    pub fn drive_negotiation<R: Read + Write>(
        &mut self,
        transport: &mut R,
    ) -> Result<SecurityProtocol, ClientError> {
        transport.write_all(&self.initial_request())?;
        transport.flush()?;
        let confirm = read_tpkt_pdu(transport)?;
        Ok(self.connector.handle_negotiation_response(&confirm)?)
    }
}

/// Read exactly one TPKT-framed PDU (4-byte header + body) from `reader`,
/// returning the whole frame including the header.
pub fn read_tpkt_pdu<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; rdp_pdu::x224::TPKT_HEADER_LEN];
    reader.read_exact(&mut header)?;
    let total = rdp_pdu::x224::read_tpkt_len(&header)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if total < header.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TPKT length smaller than its header",
        ));
    }
    let mut pdu = vec![0u8; total];
    pdu[..header.len()].copy_from_slice(&header);
    reader.read_exact(&mut pdu[header.len()..])?;
    Ok(pdu)
}
