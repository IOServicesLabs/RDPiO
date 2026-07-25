//! ICE helper seams the engine needs from the host (portable — no `engine`
//! feature, so the trait can be named on any platform).
//!
//! webrtc-rs 0.17's ICE agent can gather host / server-reflexive / **UDP** relay
//! candidates, but it cannot follow a TURN `300 Try Alternate` redirect, and it
//! cannot gather TURN over TCP or TLS. Teams (and W365) front their relays with an
//! Azure **anycast** address that answers the very first Allocate with a 300 whose
//! `ALTERNATE-SERVER` points at the real unicast backend — so webrtc-rs fails to
//! allocate any relay candidate, every ICE check fails, and Teams tears the call
//! down. The host supplies a [`TurnResolver`] that follows that redirect (rdpio
//! already speaks TURN for W365 Shortpath); the engine rewrites the `turn:` URL to
//! the resolved backend before handing it to webrtc-rs, which can then allocate
//! the relay itself with the standard (redirect-free) flow.

use std::net::SocketAddr;

/// Resolves a TURN server's real backend address past any `300 Try Alternate`
/// redirect. Implemented by the host over its native STUN/TURN client.
pub trait TurnResolver: Send + Sync {
    /// Return the unicast backend address to use for `host:port` if the server
    /// redirects there via `ALTERNATE-SERVER`, or `None` if it doesn't redirect
    /// (use the original URL) or the probe fails (nothing better to offer).
    fn resolve_alternate(&self, host: &str, port: u16) -> Option<SocketAddr>;
}
