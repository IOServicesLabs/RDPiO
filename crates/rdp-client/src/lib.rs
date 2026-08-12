//! RDPiO client library facade.
//!
//! The `rdpio` binary lives in this crate (`src/main.rs`); the library target
//! exists so the shared persistence types — [`connections::ConnectionProfile`]
//! and [`connections::ConnectionStore`] — are usable by other crates and by
//! tests without pulling in the binary's platform modules.
//!
//! The binary itself keeps its module tree private to `main.rs` (see
//! `mod connect`/`mod connections` there); this library re-exposes only the
//! self-contained, cross-platform pieces.

pub mod connections;

// Re-exports of the in-workspace crates the `rdpio` binary (and any consumer
// that assembles a full client from the individual crates) needs: the ASN.1
// codec, the wire PDU codec, the sans-I/O connection state machine, and the
// wire transport. `main.rs` imports the same crates directly; the facade makes
// the assembled dependency set importable in one place (integration tests,
// tooling, and the `rdpio` assembly crate).
pub use rdp_asn1;
pub use rdp_core;
pub use rdp_pdu;
pub use wire_main;
