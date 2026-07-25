//! Windows 365 / AVD **RDP Shortpath** — the "nano transport" rendezvous
//! signaling channel.
//!
//! ## Architecture (reverse-engineered from `rdpnanoTransport.dll` v3.2506)
//!
//! W365 Shortpath does not use the documented MS-RDPEUDP/RDPEMT multitransport
//! (that path needs a directly dialable `host:port`, which a gateway-fronted
//! Cloud PC has not). Instead it is Microsoft's proprietary **Basix DCT** nano
//! transport, a layered UDP stack:
//!
//!  1. **Signaling** — a gateway-relayed WebSocket (`clientRendezvousLocation`
//!     from the ARM `/connections` response) over which the two peers exchange
//!     an ICE `SessionDescription` `{ Version, Username(ufrag), Password(pwd),
//!     Candidates[], PacingMs, StunRetryCount, StunTimeoutMs }`. Peer candidates
//!     arrive as `ICEPeerCandidatesReceived`. **This module.**
//!  2. **ICE** — standard host / server-reflexive (STUN) / relayed (TURN)
//!     candidate gathering + connectivity checks, using the `iceServersConfig`
//!     TURN relay. Primitives live in [`crate::stun`].
//!  3. **Pseudo-TLS** — a BCrypt/DTLS-like handshake securing the winning link.
//!  4. **URCP** — a proprietary reliable-connection + rate controller
//!     (`SYN`/`SYNACK`), the actual high-performance data path.
//!  5. **Smiles v3** — multi-link bonding/redundancy carrying `smiles+userdata`
//!     (the tunneled RDP bytes).
//!
//! Layers 3–5 are undocumented and reverse-engineered incrementally. This module
//! is milestone 1: bring up the rendezvous WebSocket and learn the exact
//! `SessionDescription` wire format from the live gateway before we can speak it.
//!
//! The rendezvous WS is the *same* Azure-ARR-fronted gateway as the main Reverse
//! Connect WS: it carries its own `RDmiGatewayToken` in the query string, needs
//! the `ARRAffinity` cookie primed, and is distinguished by the
//! `X-MS-Rendezvous-Side: client` header (observed in a captured msrdc session).
#![allow(dead_code)] // signaling API is built out across milestones 1–5.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use crate::arm_broker::ShortpathConfig;
use crate::websocket::{prime_affinity_cookies, WebSocketStream};

/// An ICE candidate we gather locally to offer the peer. `base` is the local
/// socket the candidate was learned on (needed for connectivity checks).
#[derive(Debug, Clone)]
struct Candidate {
    /// `"host"` | `"srflx"` (server-reflexive) | `"relay"` (TURN-relayed).
    kind: &'static str,
    /// Transport address the peer would send to.
    addr: SocketAddr,
    /// Local base address the candidate is anchored on.
    base: SocketAddr,
}

/// Gather ICE candidates by TURN-`Allocate`-ing on the gateway's relay: one
/// round trip yields both our **server-reflexive** (`mapped`) and **relayed**
/// candidates. This doubles as the Shortpath go/no-go — if Allocate fails, UDP
/// cannot reach the relay from this network and Shortpath is impossible no
/// matter what the signaling does.
fn gather_candidates(shortpath: &ShortpathConfig) -> io::Result<Vec<Candidate>> {
    let turn = shortpath
        .turn_servers
        .first()
        .ok_or_else(|| io::Error::other("no TURN server offered"))?;
    let server = (turn.host.as_str(), turn.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("TURN host did not resolve"))?;

    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    let base = socket.local_addr()?;
    tracing::info!(%base, %server, realm = %turn.realm, "gathering ICE candidates via TURN Allocate");

    let mut client =
        crate::stun::TurnClient::new(socket, server, &turn.username, &turn.realm, &turn.password);
    let alloc = client.allocate()?;

    let mut cands = Vec::new();
    if let Some(srflx) = alloc.mapped {
        cands.push(Candidate { kind: "srflx", addr: srflx, base });
    }
    cands.push(Candidate { kind: "relay", addr: alloc.relayed, base });
    tracing::info!(
        srflx = ?alloc.mapped,
        relay = %alloc.relayed,
        lifetime = alloc.lifetime,
        "TURN allocation succeeded — UDP path to relay confirmed"
    );
    Ok(cands)
}

/// User-Agent the nano-transport DLL presents on the rendezvous upgrade. The
/// gateway is lenient about the exact value; the decisive header is
/// `X-MS-Rendezvous-Side`. Easy to pin to a captured string if a run 403s.
const RENDEZVOUS_USER_AGENT: &str = "rdpnanotransport.dll";

/// A signaling channel to the Cloud PC over the gateway-relayed rendezvous
/// WebSocket. Message-oriented: each binary frame is one nano-transport
/// signaling message (an ICE `SessionDescription` or candidate update).
pub struct RendezvousChannel {
    ws: WebSocketStream,
}

impl RendezvousChannel {
    /// Connect and upgrade the rendezvous WebSocket described by the ARM
    /// response's `clientRendezvousLocation` (an `https://…/rendezvousclient/…`
    /// URL carrying its own `RDmiGatewayToken`).
    ///
    /// Unlike the main Reverse Connect WS (which sends no bearer — the URL token
    /// authenticates it), the rendezvous endpoint returns 401 without an
    /// `Authorization: Bearer`, and `rdpnanoTransport.dll` does send one, so we
    /// pass `access_token` here.
    pub fn connect(
        rendezvous_url: &str,
        access_token: &str,
        accept_invalid_cert: bool,
    ) -> io::Result<Self> {
        let ws_url = to_ws_scheme(rendezvous_url);
        let sni_host = url::Url::parse(&ws_url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .ok_or_else(|| io::Error::other("rendezvous URL has no host"))?;

        // Same Azure-ARR gateway family as the main Reverse Connect WS: prime the
        // affinity cookie so the upgrade lands on the backend holding our state.
        let cookies = prime_affinity_cookies(&ws_url, &sni_host, accept_invalid_cert);
        let cookie_header = (!cookies.is_empty()).then(|| {
            cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        });

        let mut builder = http::Request::builder()
            .uri(&ws_url)
            .header("Accept", "*/*")
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .header("User-Agent", RENDEZVOUS_USER_AGENT)
            .header("X-Ms-User-Agent", "Windows365NativeClient/2.0.1193.0")
            .header("X-MS-Rendezvous-Side", "client");
        if !access_token.is_empty() {
            builder = builder.header("Authorization", format!("Bearer {access_token}"));
        }
        if let Some(ref cookie) = cookie_header {
            builder = builder.header("Cookie", cookie);
        }
        let request = builder
            .body(())
            .map_err(|e| io::Error::other(format!("bad rendezvous request: {e}")))?;

        tracing::info!(
            %sni_host,
            has_affinity_cookie = cookie_header.is_some(),
            "connecting Shortpath rendezvous WebSocket"
        );
        let ws = WebSocketStream::connect(request, &sni_host, accept_invalid_cert)
            .map_err(|e| io::Error::other(format!("rendezvous WS connect: {e}")))?;
        tracing::info!("Shortpath rendezvous WebSocket established");
        Ok(Self { ws })
    }

    /// Send one signaling message (one binary WebSocket frame).
    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.ws.send_binary_message(data)
    }

    /// Receive the next signaling message. `WouldBlock` on read timeout,
    /// `UnexpectedEof` on peer close.
    pub fn recv(&mut self) -> io::Result<Vec<u8>> {
        self.ws.read_binary_message()
    }

    /// Set/clear the read timeout so [`recv`](Self::recv) can poll without
    /// blocking indefinitely.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.ws.set_read_timeout(dur)
    }
}

/// Step 1 of the rendezvous: `clientRendezvousLocation` is **not** a WebSocket —
/// it is a second REST broker (mirroring ARM `/connections`). A GET returns JSON
/// `{ connectionId, gatewayConnectionToken, gatewayLocation[PreWebSocket], … }`
/// naming the *actual* nano-transport signaling WebSocket to dial. `ureq`
/// de-chunks and reads the whole body for us. The response shares ARM's field
/// names, so we reuse [`crate::arm_broker::parse_connection_response`].
fn broker_rendezvous(
    rendezvous_url: &str,
    access_token: &str,
) -> io::Result<crate::arm_broker::ArmConnection> {
    tracing::info!("brokering Shortpath rendezvous (REST GET)");
    let resp = ureq::get(rendezvous_url)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set("User-Agent", RENDEZVOUS_USER_AGENT)
        .set("X-Ms-User-Agent", "Windows365NativeClient/2.0.1193.0")
        .set("X-MS-Rendezvous-Side", "client")
        .call();
    let text = match resp {
        Ok(r) => r
            .into_string()
            .map_err(|e| io::Error::other(format!("rendezvous body read: {e}")))?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(io::Error::other(format!(
                "rendezvous broker returned {code}: {}",
                &body[..body.len().min(200)]
            )));
        }
        Err(e) => return Err(io::Error::other(format!("rendezvous broker request: {e}"))),
    };

    // Secret-safe field inventory (long strings → `str[len]`, tokens redacted).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        let mut fields = Vec::new();
        crate::arm_broker::collect_json_fields("", &v, &mut fields);
        tracing::info!(fields = %fields.join("  "), "Shortpath rendezvous broker response fields");
    }

    crate::arm_broker::parse_connection_response(&text)
        .map_err(|e| io::Error::other(format!("rendezvous response parse: {e}")))
}

/// **Milestone-1 probe.** Two steps: (1) REST-broker the rendezvous location to
/// learn the signaling WebSocket URL, then (2) connect that WS and log every
/// inbound signaling frame (secret-safe: length + a short structural preview)
/// for `budget`, so a live run reveals the exact `SessionDescription` wire
/// format the Cloud PC sends. Purely additive — runs on its own thread alongside
/// the RDP session and never touches the main transport.
///
/// Best-effort throughout: any broker/connect/read error is logged and ends it.
pub fn probe(
    rendezvous_url: &str,
    access_token: &str,
    shortpath: &ShortpathConfig,
    accept_invalid_cert: bool,
    budget: Duration,
) {
    // ICE candidate gathering doubles as the Shortpath go/no-go: do it up front so
    // a UDP-blocked network is diagnosed even if the signaling WS is fine.
    match gather_candidates(shortpath) {
        Ok(cands) => {
            for c in &cands {
                tracing::info!(kind = c.kind, addr = %c.addr, base = %c.base, "ICE candidate");
            }
        }
        Err(e) => tracing::warn!(error = %e, "ICE gathering failed (UDP to relay may be blocked)"),
    }

    let broker = match broker_rendezvous(rendezvous_url, access_token) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "Shortpath rendezvous broker failed");
            return;
        }
    };
    let ws_url = match broker.websocket_url() {
        Some(u) => u.to_string(),
        None => {
            tracing::warn!(
                "Shortpath rendezvous broker response had no signaling WS location \
                 (see 'rendezvous broker response fields' for the actual field name)"
            );
            return;
        }
    };
    tracing::info!(%ws_url, "Shortpath rendezvous signaling WS location resolved");

    let mut ch = match RendezvousChannel::connect(&ws_url, access_token, accept_invalid_cert) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Shortpath rendezvous signaling connect failed");
            return;
        }
    };
    // Poll with a short timeout so the loop honours `budget` even when idle.
    let _ = ch.set_read_timeout(Some(Duration::from_millis(500)));

    let start = Instant::now();
    let mut frames = 0usize;
    while start.elapsed() < budget {
        match ch.recv() {
            Ok(msg) => {
                frames += 1;
                log_frame(frames, &msg);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::info!("Shortpath rendezvous closed by peer");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Shortpath rendezvous recv error");
                break;
            }
        }
    }
    tracing::info!(frames, elapsed_ms = start.elapsed().as_millis() as u64, "Shortpath rendezvous probe finished");
}

/// Log one rendezvous frame secret-safely: total length plus a short hex + ASCII
/// preview of the head. Enough to reveal the framing/schema (JSON vs binary TLV,
/// key names) without dumping full candidate lists or ICE ufrag/pwd material.
fn log_frame(idx: usize, msg: &[u8]) {
    const PREVIEW: usize = 64;
    let head = &msg[..msg.len().min(PREVIEW)];
    let hex: String = head.iter().map(|b| format!("{b:02x}")).collect();
    let ascii: String = head
        .iter()
        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
        .collect();
    tracing::info!(
        idx,
        len = msg.len(),
        truncated = msg.len() > PREVIEW,
        head_hex = %hex,
        head_ascii = %ascii,
        "Shortpath rendezvous frame"
    );
}

/// Upgrade an `https`/`http` URL to its WebSocket equivalent (`wss`/`ws`); a URL
/// already using a ws scheme is returned unchanged.
fn to_ws_scheme(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = u.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        u.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_upgrade() {
        assert_eq!(to_ws_scheme("https://h/api/x?t=1"), "wss://h/api/x?t=1");
        assert_eq!(to_ws_scheme("http://h/p"), "ws://h/p");
        assert_eq!(to_ws_scheme("wss://h/p"), "wss://h/p");
    }

    #[test]
    fn rendezvous_host_parses_from_broker_url() {
        // The shape of a real clientRendezvousLocation (host must parse for SNI).
        let u = "https://afdfp-rdgateway-r1.wvd.microsoft.com/api/arm/v2/connections/rendezvousclient/b475fad8-6b/corr/EUS2/RDGatewayRoleZrRedisCache?RDmiGatewayToken=abc";
        let ws = to_ws_scheme(u);
        let host = url::Url::parse(&ws).unwrap().host_str().unwrap().to_string();
        assert_eq!(host, "afdfp-rdgateway-r1.wvd.microsoft.com");
    }
}
