//! The host's switches for A/B testing, like the Android app's
//! `debug.rdpio.<name>`: `%PUBLIC%\rdpio-hyper-switches.txt` (a folder any
//! user can write, so no elevation and no service restart), one `name=0|1` a
//! line. Read at most once a second; a name not in the file keeps its default.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static CACHE: Mutex<Option<(Instant, HashMap<String, bool>)>> = Mutex::new(None);

/// Whether switch `name` is on (`default` unless the file says otherwise).
pub fn ab(name: &str, default: bool) -> bool {
    let mut cache = CACHE.lock().unwrap();
    if cache.as_ref().is_none_or(|(t, _)| t.elapsed() >= Duration::from_secs(1)) {
        *cache = Some((Instant::now(), read()));
    }
    cache.as_ref().and_then(|(_, m)| m.get(name).copied()).unwrap_or(default)
}

fn read() -> HashMap<String, bool> {
    let dir = std::env::var_os("PUBLIC").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Users\Public".into());
    std::fs::read_to_string(dir.join("rdpio-hyper-switches.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim() == "1"))
        })
        .collect()
}
