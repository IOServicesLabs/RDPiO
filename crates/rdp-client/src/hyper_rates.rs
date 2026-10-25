//! What each client's path sustained last time, for a warm start (see
//! `RateController::warm_start`): `%ProgramData%\rdpio\hyper-rates.json`,
//! `{ "<client address>": { "bps": …, "at": <unix seconds> } }`, written by
//! the service. Keyed by the client's IP, so the tablet's LAN address and
//! its Tailscale one (a relay, often ~1 Mbps) are remembered apart.
//! Memories older than a week are forgotten.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A memory older than this (seconds) is forgotten.
const KEEP_SECS: u64 = 7 * 24 * 3600;

fn file() -> PathBuf {
    crate::hyper_direct::data_dir().join("hyper-rates.json")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The rate `ip`'s path sustained last time (bits per second), if it's
/// remembered (and not a loopback selftest).
pub fn recall(ip: IpAddr) -> Option<u32> {
    if ip.is_loopback() {
        return None;
    }
    recall_in(&file(), ip, now_secs())
}

/// Remember that `ip`'s path sustained `bps`.
pub fn remember(ip: IpAddr, bps: u32) {
    if ip.is_loopback() {
        return;
    }
    if let Err(e) = remember_in(&file(), ip, bps, now_secs()) {
        tracing::debug!(error = %e, "couldn't save the path's rate");
    }
}

fn read(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn recall_in(path: &Path, ip: IpAddr, now: u64) -> Option<u32> {
    let all = read(path);
    let entry = all.get(&ip.to_string())?;
    let at = entry.get("at")?.as_u64()?;
    if now.saturating_sub(at) > KEEP_SECS {
        return None;
    }
    entry.get("bps")?.as_u64().map(|b| b.min(u32::MAX as u64) as u32)
}

fn remember_in(path: &Path, ip: IpAddr, bps: u32, now: u64) -> std::io::Result<()> {
    let mut all = read(path);
    all.retain(|_, v| v.get("at").and_then(|a| a.as_u64()).is_some_and(|at| now.saturating_sub(at) <= KEEP_SECS));
    all.insert(ip.to_string(), serde_json::json!({ "bps": bps, "at": now }));
    // Whole or not at all: a new file, then renamed over the old one.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::Value::Object(all).to_string())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_are_remembered_per_address_for_a_week() {
        let dir = std::env::temp_dir().join(format!("rdpio-rates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hyper-rates.json");
        let (lan, relay): (IpAddr, IpAddr) = ("192.168.1.20".parse().unwrap(), "100.101.102.103".parse().unwrap());
        assert_eq!(recall_in(&path, relay, 1_000), None);
        remember_in(&path, relay, 1_100_000, 1_000).unwrap();
        remember_in(&path, lan, 45_000_000, 2_000).unwrap();
        assert_eq!(recall_in(&path, relay, 1_000 + 3600), Some(1_100_000));
        assert_eq!(recall_in(&path, lan, 2_000), Some(45_000_000));
        // A week on, the relay's is forgotten (and dropped at the next write).
        assert_eq!(recall_in(&path, relay, 1_000 + KEEP_SECS + 1), None);
        remember_in(&path, lan, 40_000_000, 1_000 + KEEP_SECS + 1).unwrap();
        assert!(!read(&path).contains_key(&relay.to_string()));
        // A broken file is no memory at all.
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(recall_in(&path, lan, 2_000), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
