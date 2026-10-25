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
}
