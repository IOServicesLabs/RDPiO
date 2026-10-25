//! How the client's packets reach this host — the host's own network,
//! Tailscale peer to peer, or Tailscale through a DERP relay (every packet
//! over TCP via a server elsewhere: ~1 Mbit/s, seconds of queue, drops). The
//! host asks Tailscale (`tailscale status --json`, ~30 ms, on a thread of its
//! own every 10 s), logs the path when it changes and tells the client
//! ([`Control::Path`], every 2 s) for its stats line. Switch `pathinfo`.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rdp_hyper::control::{Control, PathKind};

/// How often Tailscale is asked (a path changes when its NAT traversal
/// succeeds or a network changes; this is for the user's eyes).
const REFRESH: Duration = Duration::from_secs(10);
/// How often the client is told (one lost packet costs only that long).
const TELL_EVERY: Duration = Duration::from_secs(2);

/// The path report for one stream; stops when dropped.
pub struct PathReport {
    stop: Arc<AtomicBool>,
}

impl PathReport {
    /// `peer`: the client's address now (it may move); `send`: a control
    /// message to the client.
    pub fn start(peer: impl Fn() -> SocketAddr + Send + 'static, send: impl Fn(&[u8]) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        // Not joined: a hung `tailscale.exe` mustn't hold up the stream's end.
        let _ = std::thread::Builder::new().name("hyper-path".into()).spawn(move || {
            let lan: Vec<String> = crate::launcher::host::addresses()
                .into_iter()
                .filter(|a| a.kind == "lan")
                .map(|a| format!("{}/{}", a.ip, a.prefix))
                .collect();
            let mut known: Option<(IpAddr, PathKind, String)> = None;
            let (mut asked, mut told) = (Instant::now() - REFRESH, Instant::now() - TELL_EVERY);
            while !s.load(Ordering::Relaxed) {
                if !crate::host_ab::ab("pathinfo", true) {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                let ip = peer().ip();
                if known.as_ref().is_none_or(|k| k.0 != ip) || asked.elapsed() >= REFRESH {
                    asked = Instant::now();
                    let (kind, relay) = path_of(ip, &lan);
                    if known.as_ref().is_none_or(|k| (k.0, k.1, &k.2) != (ip, kind, &relay)) {
                        tracing::info!(client = %ip, path = %kind.describe(&relay), "path to the client");
                        if kind == PathKind::Relay {
                            tracing::warn!(relay = %relay, "Tailscale is relaying this connection (no direct UDP): expect lag");
                        }
                        told = Instant::now() - TELL_EVERY;
                    }
                    known = Some((ip, kind, relay));
                }
                if let Some((_, kind, relay)) = &known {
                    if told.elapsed() >= TELL_EVERY {
                        send(&Control::Path { kind: *kind, relay: relay.clone() }.to_bytes());
                        told = Instant::now();
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        Self { stop }
    }
}

impl Drop for PathReport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The path to `ip`: the LAN by address, Tailscale's by asking it.
fn path_of(ip: IpAddr, lan: &[String]) -> (PathKind, String) {
    if rdp_hyper::lan::on_lan(lan, ip) {
        return (PathKind::Lan, String::new());
    }
    if !rdp_hyper::lan::is_tailscale(ip) {
        return (PathKind::Unknown, String::new());
    }
    tailscale_status()
        .and_then(|json| rdp_hyper::lan::tailscale_path(&json, ip))
        .unwrap_or((PathKind::Unknown, String::new()))
}

/// `tailscale status --json` (read-only), from where its installer puts it.
fn tailscale_status() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let dir = std::env::var_os("ProgramFiles").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
    let exe = dir.join("Tailscale").join("tailscale.exe");
    if !exe.is_file() {
        return None;
    }
    let out = std::process::Command::new(exe)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only, on a PC with Tailscale: prints what the host would report
    /// for each peer (`cargo test -p rdp-client hyper_path -- --nocapture`).
    #[test]
    fn this_pcs_tailscale_peers() {
        let Some(json) = tailscale_status() else { return };
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for p in v["Peer"].as_object().into_iter().flat_map(|m| m.values()) {
            let Some(ip) = p["TailscaleIPs"][0].as_str().and_then(|s| s.parse().ok()) else { continue };
            let (kind, relay) = path_of(ip, &[]);
            println!("{} {ip}: {}", p["HostName"], kind.describe(&relay));
            assert_ne!(kind, PathKind::Unknown);
        }
    }
}
