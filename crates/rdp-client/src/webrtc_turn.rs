//! TURN redirect resolver for the native Teams engine (`--teams-native`).
//!
//! webrtc-rs 0.17's ICE agent can't follow a TURN `300 Try Alternate` redirect,
//! and Teams' relays are fronted by an Azure **anycast** address that answers the
//! first Allocate with exactly that — so webrtc-rs allocates no relay candidate
//! and every ICE check fails. rdpio already speaks TURN (for W365 Shortpath), so
//! this reuses [`crate::stun::TurnClient`] to follow the redirect (an
//! unauthenticated probe — the 300 comes before any auth) and report the unicast
//! backend. [`rdp_webrtc`]'s engine then rewrites the `turn:` URL to that backend,
//! which webrtc-rs can allocate with the ordinary (redirect-free) flow.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use rdp_webrtc::TurnResolver;

use crate::stun::TurnClient;

/// Per-probe round-trip budget. The 300 comes back on the first datagram, so this
/// only bounds the pathological no-reply case; kept short to stay well inside the
/// few seconds Teams waits before tearing an un-negotiated call down.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Resolves Teams' anycast TURN relays to their unicast backend via native STUN.
pub struct WinTurnResolver;

impl TurnResolver for WinTurnResolver {
    fn resolve_alternate(&self, host: &str, port: u16) -> Option<SocketAddr> {
        // Resolve to an IPv4 address (webrtc-rs gathers UDP4; the backend we hand
        // back must match).
        let server = (host, port).to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)?;
        let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
        // No credentials needed: the redirect is issued before authentication.
        let mut client = TurnClient::new(socket, server, "", "", "");
        match client.resolve_backend(PROBE_TIMEOUT) {
            Ok(backend) if backend != server => {
                tracing::info!(
                    %server,
                    %backend,
                    "resolved Teams TURN backend behind the anycast front-end"
                );
                Some(backend)
            }
            // No redirect — the original URL is already unicast; leave it be.
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(%server, error = %e, "TURN alternate-server probe failed; using the original URL");
                None
            }
        }
    }
}
