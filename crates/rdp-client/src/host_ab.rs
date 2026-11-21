//! The host's switches for A/B testing, like the Android app's
//! `debug.rdpio.<name>`: `%PUBLIC%\rdpio-hyper-switches.txt` (a folder any
//! user can write, so no elevation and no service restart), one `name=0|1` a
//! line. Read at most once a second; a name not in the file keeps its default.
//!
//! The file is read on a thread of its own ("host-ab"), never by a caller:
//! the stream's capture, encode and receive threads ask, and a scanner
//! holding the file open used to stall whichever of them came first, with
//! every other caller waiting on the lock behind it. A caller only looks a
//! name up under a lock held for that. The thread reads once a second while
//! switches are asked for, and sleeps once nobody has asked for a few
//! seconds (no wake-ups on an idle machine); the next ask wakes it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The switches as last read (`None`: not read yet).
static CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
/// The reading thread (`None`: it couldn't be started).
static READER: OnceLock<Option<std::thread::Thread>> = OnceLock::new();
/// A switch was asked for since the reader last looked.
static ASKED: AtomicBool = AtomicBool::new(false);

/// How often the file is read while switches are asked for.
const READ_EVERY: Duration = Duration::from_secs(1);
/// Reads in a row with nobody asking before the reader sleeps until asked.
const IDLE_READS: u32 = 5;

/// Whether switch `name` is on (`default` unless the file says otherwise).
pub fn ab(name: &str, default: bool) -> bool {
    let reader = READER.get_or_init(start_reader);
    if !ASKED.swap(true, Ordering::Relaxed) {
        if let Some(t) = reader {
            t.unpark();
        }
    }
    // (No reader thread: the switches stay as first read.)
    let cache = CACHE.lock().unwrap();
    cache.as_ref().and_then(|m| m.get(name).copied()).unwrap_or(default)
}

/// The first ask reads the file at once (a switch read as a stream starts
/// must see it), then hands reading on to a thread.
fn start_reader() -> Option<std::thread::Thread> {
    let first = read();
    *CACHE.lock().unwrap() = Some(first);
    std::thread::Builder::new()
        .name("host-ab".into())
        .spawn(|| {
            let mut unasked = 0u32;
            loop {
                std::thread::sleep(READ_EVERY);
                let asked = ASKED.swap(false, Ordering::Relaxed);
                unasked = if asked { 0 } else { unasked + 1 };
                if unasked >= IDLE_READS {
                    // Until the next ask (`ab` unparks after setting ASKED).
                    while !ASKED.load(Ordering::Relaxed) {
                        std::thread::park();
                    }
                    unasked = 0;
                }
                let read = read();
                *CACHE.lock().unwrap() = Some(read);
            }
        })
        .ok()
        .map(|h| h.thread().clone())
}

fn read() -> HashMap<String, bool> {
    let dir = std::env::var_os("PUBLIC").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Users\Public".into());
    parse(&std::fs::read_to_string(dir.join("rdpio-hyper-switches.txt")).unwrap_or_default())
}

fn parse(text: &str) -> HashMap<String, bool> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim() == "1"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_read_as_name_equals_0_or_1() {
        let m = parse("floorbusy=0\r\n clockhigh = 1\nnonsense\nbattfps=yes\n");
        assert_eq!(m.get("floorbusy"), Some(&false));
        assert_eq!(m.get("clockhigh"), Some(&true));
        assert_eq!(m.get("battfps"), Some(&false), "anything but 1 is off");
        assert_eq!(m.get("nonsense"), None);
    }
}
