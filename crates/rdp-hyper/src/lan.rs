//! A direct path to the host on the client's own network. A client signed
//! in through a VPN (Tailscale) has every datagram go through the VPN's own
//! encryption as well — on an Android tablet, three quarters of a core at
//! 24 Mbit/s. The host's offer lists its local addresses
//! ([`crate::auth::Offer::lan`]); the client keeps them, and its next sign-in
//! goes to one directly when it's on that network and the host answers there.
//! The stream is encrypted end to end either way, and the sign-in proves it's
//! the host.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::auth::{AuthMsg, ClientAuth, Want};

/// "10.0.0.5/24" → (10.0.0.5, 24).
pub fn parse(net: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = net.split_once('/')?;
    Some((ip.parse().ok()?, prefix.parse().ok().filter(|&p| p <= 32)?))
}

fn same_network(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    u32::from(a) & mask == u32::from(b) & mask
}

/// The host's local addresses (`lan`, as its offer gave them) on a network
/// this device is on (`mine`: its addresses and prefix lengths), at the port
/// of `via` (the address the user gave, left out).
pub fn candidates(lan: &[String], mine: &[(Ipv4Addr, u8)], via: SocketAddr) -> Vec<SocketAddr> {
    lan.iter()
        .filter_map(|n| parse(n))
        .filter(|&(ip, prefix)| IpAddr::V4(ip) != via.ip() && mine.iter().any(|&(m, p)| p == prefix && same_network(m, ip, prefix)))
        .map(|(ip, _)| SocketAddr::new(ip.into(), via.port()))
        .collect()
}

/// Whether a Hyper host answers at `addr` within `wait`: a hello without a
/// cookie gets one back at once, and the host keeps nothing for it.
pub fn answers(addr: SocketAddr, wait: Duration) -> bool {
    let Ok(socket) = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }) else { return false };
    if socket.connect(addr).is_err() || socket.set_read_timeout(Some(Duration::from_millis(100))).is_err() {
        return false;
    }
    let (_, hello) = ClientAuth::start(&[0; 32], Want::default());
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let _ = socket.send(&hello);
        if let Ok(n) = socket.recv(&mut buf) {
            if matches!(AuthMsg::parse(&buf[..n]), Some(AuthMsg::Cookie { .. } | AuthMsg::Denied { .. })) {
                return true;
            }
        }
    }
    false
}

/// The first of [`candidates`] that [`answers`].
pub fn direct(lan: &[String], mine: &[(Ipv4Addr, u8)], via: SocketAddr) -> Option<SocketAddr> {
    candidates(lan, mine, via).into_iter().find(|&a| answers(a, Duration::from_millis(400)))
}

/// How long a candidate gets at most (as [`direct`] gives each).
const DIRECT_WAIT_MS: u64 = 400;
/// Once the host has answered through `via` after `t`, a candidate gets
/// `2t` plus this from the start (ms).
const DIRECT_MARGIN_MS: u64 = 30;
/// Hellos repeat this often (ms) to whoever hasn't answered.
const HELLO_EVERY_MS: u64 = 100;

/// [`direct`], without waiting long on a network that only looks like the
/// host's: the cookieless hello goes to every one of [`candidates`] and to
/// `via` at the same moment. A candidate that answers wins at once. Once the
/// host has answered through `via` (after `t`), the candidates get until
/// `2t + 30 ms` from the start, never past 400 ms; then `None`: sign in
/// through `via`. A host really on this network answers sooner than through
/// a VPN running over that same network; one at home while this device is
/// on a same-numbered network elsewhere cost a flat 400 ms per candidate on
/// every connect and every reconnect.
pub fn direct_or(lan: &[String], mine: &[(Ipv4Addr, u8)], via: SocketAddr) -> Option<SocketAddr> {
    let candidates = candidates(lan, mine, via);
    if candidates.is_empty() {
        return None;
    }
    race(&candidates, via, Duration::from_millis(DIRECT_WAIT_MS))
}

/// The race of [`direct_or`]: the first of `candidates` to answer, unless
/// `via` answers well before any of them (or `most` passes).
fn race(candidates: &[SocketAddr], via: SocketAddr, most: Duration) -> Option<SocketAddr> {
    let open = |addr: SocketAddr| {
        let socket = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
        socket.connect(addr).ok()?;
        socket.set_nonblocking(true).ok()?;
        Some(socket)
    };
    // (Index 0 is `via`; a socket that couldn't open never answers.)
    let sockets: Vec<Option<UdpSocket>> = std::iter::once(via).chain(candidates.iter().copied()).map(open).collect();
    let (_, hello) = ClientAuth::start(&[0; 32], Want::default());
    let start = Instant::now();
    let (mut deadline, mut via_answered) = (most, false);
    let mut last_hello: Option<Instant> = None;
    let mut buf = [0u8; 2048];
    loop {
        if last_hello.is_none_or(|t| t.elapsed() >= Duration::from_millis(HELLO_EVERY_MS)) {
            for s in sockets.iter().flatten() {
                let _ = s.send(&hello);
            }
            last_hello = Some(Instant::now());
        }
        for (i, s) in sockets.iter().enumerate() {
            let Some(s) = s else { continue };
            while let Ok(n) = s.recv(&mut buf) {
                if !matches!(AuthMsg::parse(&buf[..n]), Some(AuthMsg::Cookie { .. } | AuthMsg::Denied { .. })) {
                    continue;
                }
                if i > 0 {
                    return Some(candidates[i - 1]);
                }
                if !via_answered {
                    via_answered = true;
                    deadline = most.min(start.elapsed() * 2 + Duration::from_millis(DIRECT_MARGIN_MS));
                }
                break;
            }
        }
        if start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Whether `client` is on one of the host's local networks (`lan`, as in
/// [`crate::auth::Offer::lan`]).
pub fn on_lan(lan: &[String], client: IpAddr) -> bool {
    let IpAddr::V4(c) = client else { return false };
    lan.iter().filter_map(|n| parse(n)).any(|(ip, prefix)| same_network(ip, c, prefix))
}

/// Whether `ip` is a Tailscale address (100.64.0.0/10, fd7a:115c:a1e0::/48).
pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]),
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// How Tailscale carries the peer at `client`, from `tailscale status --json`
/// (`Peer.*`: `TailscaleIPs`, `CurAddr` — set while peer to peer — and
/// `Relay`, its home DERP; `PeerRelay` when another node relays). `None`
/// when no peer has that address.
pub fn tailscale_path(status_json: &str, client: IpAddr) -> Option<(crate::control::PathKind, String)> {
    use crate::control::PathKind;
    let v: serde_json::Value = serde_json::from_str(status_json).ok()?;
    let ip = client.to_string();
    let peer = v.get("Peer")?.as_object()?.values().find(|p| {
        p.get("TailscaleIPs").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(&ip)))
    })?;
    let field = |k: &str| peer.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(if !field("CurAddr").is_empty() {
        (PathKind::Direct, String::new())
    } else if !field("PeerRelay").is_empty() {
        (PathKind::PeerRelay, String::new())
    } else {
        (PathKind::Relay, field("Relay"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::PathKind;

    #[test]
    fn tailscale_status_tells_relayed_from_direct() {
        let json = r#"{"Self":{"TailscaleIPs":["100.64.0.1"],"CurAddr":"","Relay":"dfw"},
            "Peer":{"k1":{"HostName":"tablet","TailscaleIPs":["100.64.0.2","fd7a:115c:a1e0::2"],"CurAddr":"","Relay":"ord","PeerRelay":""},
                    "k2":{"HostName":"laptop","TailscaleIPs":["100.64.0.3"],"CurAddr":"203.0.113.5:41641","Relay":"ord"},
                    "k3":{"HostName":"far","TailscaleIPs":["100.64.0.4"],"CurAddr":"","Relay":"mia","PeerRelay":"100.1.2.3:7"}}}"#;
        let p = |ip: &str| tailscale_path(json, ip.parse().unwrap());
        assert_eq!(p("100.64.0.2"), Some((PathKind::Relay, "ord".into())));
        assert_eq!(p("fd7a:115c:a1e0::2"), Some((PathKind::Relay, "ord".into())));
        assert_eq!(p("100.64.0.3"), Some((PathKind::Direct, String::new())));
        assert_eq!(p("100.64.0.4"), Some((PathKind::PeerRelay, String::new())));
        assert_eq!(p("100.99.99.99"), None);
        assert_eq!(tailscale_path("not json", "100.64.0.2".parse().unwrap()), None);
    }

    #[test]
    fn lan_and_tailscale_addresses_are_told_apart() {
        let lan = ["192.168.196.10/24".to_string()];
        assert!(on_lan(&lan, "192.168.196.52".parse().unwrap()));
        assert!(!on_lan(&lan, "100.64.0.2".parse().unwrap()));
        assert!(is_tailscale("100.64.0.2".parse().unwrap()));
        assert!(is_tailscale("fd7a:115c:a1e0::2".parse().unwrap()));
        assert!(!is_tailscale("192.168.196.52".parse().unwrap()));
        assert!(!is_tailscale("100.200.1.1".parse().unwrap()));
    }

    #[test]
    fn only_the_networks_this_device_is_on() {
        let lan = ["10.0.0.144/24".to_string(), "192.168.1.9/24".into(), "bad".into(), "10.0.0.7/33".into()];
        let via: SocketAddr = "100.70.1.2:47998".parse().unwrap();
        let home = [(Ipv4Addr::new(10, 0, 0, 23), 24), (Ipv4Addr::new(100, 68, 73, 71), 32)];
        assert_eq!(candidates(&lan, &home, via), vec!["10.0.0.144:47998".parse::<SocketAddr>().unwrap()]);
        let away = [(Ipv4Addr::new(10, 150, 10, 12), 24)];
        assert!(candidates(&lan, &away, via).is_empty());
        // Already there: nothing to switch to.
        let via_lan: SocketAddr = "10.0.0.144:47998".parse().unwrap();
        assert!(candidates(&lan, &home, via_lan).is_empty());
    }

    #[test]
    fn a_host_answers_a_cookieless_hello() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = host.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (_, from) = host.recv_from(&mut buf).unwrap();
            host.send_to(&AuthMsg::Cookie { cookie: [7; 16] }.to_datagram(), from).unwrap();
        });
        assert!(answers(addr, Duration::from_secs(2)));
        t.join().unwrap();
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(!answers(silent.local_addr().unwrap(), Duration::from_millis(200)));
    }

    /// A host on loopback answering every hello after `delay`, for `for_ms`.
    fn host_answering(delay: Duration, for_ms: u64) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = host.local_addr().unwrap();
        host.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let t = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_millis(for_ms);
            let mut buf = [0u8; 2048];
            while Instant::now() < until {
                if let Ok((_, from)) = host.recv_from(&mut buf) {
                    std::thread::sleep(delay);
                    let _ = host.send_to(&AuthMsg::Cookie { cookie: [7; 16] }.to_datagram(), from);
                }
            }
        });
        (addr, t)
    }

    #[test]
    fn a_local_host_wins_the_race() {
        let (via, a) = host_answering(Duration::from_millis(60), 1_000);
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (local, b) = host_answering(Duration::ZERO, 1_000);
        let candidates = [silent.local_addr().unwrap(), local];
        assert_eq!(race(&candidates, via, Duration::from_millis(400)), Some(local));
        a.join().unwrap();
        b.join().unwrap();
    }

    /// The host answers through `via` at once and no candidate does: the
    /// candidates don't get the full 400 ms.
    #[test]
    fn a_same_numbered_network_elsewhere_costs_little() {
        let (via, a) = host_answering(Duration::ZERO, 1_000);
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let t = Instant::now();
        assert_eq!(race(&[silent.local_addr().unwrap()], via, Duration::from_millis(400)), None);
        assert!(t.elapsed() < Duration::from_millis(250), "{:?}", t.elapsed());
        a.join().unwrap();
        // Nobody answers anywhere: the full wait, as before.
        let quiet_via = UdpSocket::bind("127.0.0.1:0").unwrap();
        let t = Instant::now();
        assert_eq!(race(&[silent.local_addr().unwrap()], quiet_via.local_addr().unwrap(), Duration::from_millis(200)), None);
        assert!(t.elapsed() >= Duration::from_millis(200));
        // No candidates on this network: no wait at all.
        assert_eq!(direct_or(&[], &[], quiet_via.local_addr().unwrap()), None);
    }
}
