//! Where the stream's threads run, and the switches its measurements use.
//!
//! - [`ab`]: a hidden on/off for one improvement (`adb shell setprop
//!   debug.rdpio.<name> 0|1`; unset: the default), so each can be measured
//!   against the stream without it, live, without a settings screen.
//! - [`Placement`]: the latency-critical threads (receive, decode, picture
//!   out, sound) kept off the little cores in game mode. Android's scheduler
//!   wakes a thread that sleeps most of the time — as these do, between
//!   packets and frames — on whatever core is cheapest, often a little one
//!   at a low clock (on a Snapdragon 8 Gen 3: two Cortex-A520 at 2.27 GHz,
//!   against 3.0-3.3 GHz for the others).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// `debug.rdpio.<name>`: "1" on, "0" off, else `default`.
pub fn ab(name: &str, default: bool) -> bool {
    set(name).unwrap_or(default)
}

/// `debug.rdpio.<name>` when set to 0 or 1. Read at most once a second per
/// name (a property read is cheap, but this runs per packet and per frame, on
/// the receive, decode and sound threads at once): in between, a hash of the
/// name, an atomic load and the kernel's coarse clock — no lock, no
/// allocation. (It was a Mutex around a HashMap<String, _> and an
/// Instant::now() a call: contended between those threads.)
pub fn set(name: &str) -> Option<bool> {
    let Some(slot) = slot(name) else { return read(name) };
    let now = now_ms();
    let seen = slot.state.load(Ordering::Relaxed);
    if seen & 3 != 0 {
        if now.wrapping_sub(seen >> 2) < 1000 {
            return decode(seen);
        }
        // Due: one thread reads it again; the others keep the last value meanwhile.
        if slot.state.compare_exchange(seen, now << 2 | seen & 3, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return decode(seen);
        }
    }
    let v = read(name);
    slot.state.store(now << 2 | encode(v), Ordering::Relaxed);
    v
}

/// One name's last reading: `ms << 2 | value` (value 0: never read, 1:
/// unset, 2: 0, 3: 1).
struct Slot {
    /// The name's hash; 0 while free.
    key: AtomicU64,
    state: AtomicU64,
}

/// Room for every switch (a dozen or so); past that, names are read uncached.
static SLOTS: [Slot; 64] = [const { Slot { key: AtomicU64::new(0), state: AtomicU64::new(0) } }; 64];

fn slot(name: &str) -> Option<&'static Slot> {
    // FNV-1a: the names are short, a 64-bit hash of a dozen of them won't collide.
    let h = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)).max(1);
    for i in 0..SLOTS.len() {
        let s = &SLOTS[(h as usize).wrapping_add(i) % SLOTS.len()];
        match s.key.load(Ordering::Relaxed) {
            k if k == h => return Some(s),
            0 => match s.key.compare_exchange(0, h, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Some(s),
                Err(k) if k == h => return Some(s),
                Err(_) => {}
            },
            _ => {}
        }
    }
    None
}

fn encode(v: Option<bool>) -> u64 {
    match v {
        None => 1,
        Some(false) => 2,
        Some(true) => 3,
    }
}

fn decode(state: u64) -> Option<bool> {
    match state & 3 {
        2 => Some(false),
        3 => Some(true),
        _ => None,
    }
}

fn read(name: &str) -> Option<bool> {
    match property(&format!("debug.rdpio.{name}")).as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Milliseconds on the kernel's coarse monotonic clock: a read of the vDSO's
/// page, no hardware counter (a few ns; ticks every 4 ms, plenty here).
#[cfg(any(target_os = "android", target_os = "linux"))]
fn now_ms() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_COARSE, &mut t) };
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}

/// (Off Android, for the benchmark on a PC.)
#[cfg(not(any(target_os = "android", target_os = "linux")))]
fn now_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

#[cfg(target_os = "android")]
fn property(name: &str) -> String {
    let Ok(key) = std::ffi::CString::new(name) else { return String::new() };
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
    let n = unsafe { libc::__system_property_get(key.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
}

/// (Off Android, for the benchmark on a PC: never set.)
#[cfg(not(target_os = "android"))]
fn property(_name: &str) -> String {
    String::new()
}

/// The CPUs that aren't the little cluster (the ones whose top clock isn't
/// the lowest), or none where every core is alike.
fn fast_cpus() -> &'static [usize] {
    static FAST: OnceLock<Vec<usize>> = OnceLock::new();
    FAST.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(8, |n| n.get()).min(64);
        let max: Vec<(usize, u64)> = (0..n)
            .filter_map(|c| {
                let f = std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/cpufreq/cpuinfo_max_freq")).ok()?;
                Some((c, f.trim().parse().ok()?))
            })
            .collect();
        let lowest = max.iter().map(|m| m.1).min().unwrap_or(0);
        let fast: Vec<usize> = max.iter().filter(|m| m.1 > lowest).map(|m| m.0).collect();
        if !fast.is_empty() {
            log::info!("fast cores: {fast:?} (of {n})");
        }
        fast
    })
}

/// The calling thread's placement, changed only when asked for another.
#[derive(Default)]
pub struct Placement {
    fast: Option<bool>,
}

impl Placement {
    /// `game`: the stream is in game mode (`debug.rdpio.bigcores` overrides:
    /// 1 always, 0 never).
    pub fn follow(&mut self, game: bool) {
        let fast = set("bigcores").unwrap_or(game);
        if self.fast == Some(fast) {
            return;
        }
        self.fast = Some(fast);
        let cpus = fast_cpus();
        if cpus.is_empty() {
            return;
        }
        #[cfg(any(target_os = "android", target_os = "linux"))]
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if fast {
                for &c in cpus {
                    libc::CPU_SET(c, &mut set);
                }
            } else {
                for c in 0..std::thread::available_parallelism().map_or(8, |n| n.get()).min(64) {
                    libc::CPU_SET(c, &mut set);
                }
            }
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// The old way (a Mutex around a HashMap, an Instant a call), to compare.
    fn locked(name: &str) -> Option<bool> {
        static CACHE: Mutex<Option<HashMap<String, (Instant, Option<bool>)>>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap();
        let map = cache.get_or_insert_with(HashMap::new);
        if let Some((at, v)) = map.get(name) {
            if at.elapsed() < Duration::from_secs(1) {
                return *v;
            }
        }
        let v = read(name);
        map.insert(name.to_string(), (Instant::now(), v));
        v
    }

    #[test]
    fn reads_and_caches() {
        assert_eq!(set("tune_test_unset"), set("tune_test_unset"));
        assert!(ab("tune_test_unset", true));
        assert!(!ab("tune_test_unset", false));
    }

    /// ns a call, one thread and four at once.
    /// `cargo test --release ab_speed -- --ignored --nocapture` (on the tablet:
    /// `taskset 80` for an X4, `taskset 1` for an A520; four threads: no taskset).
    #[test]
    #[ignore = "a benchmark"]
    fn ab_speed() {
        const N: u32 = 2_000_000;
        fn run(f: fn(&str) -> Option<bool>, threads: u32) -> f64 {
            let t = Instant::now();
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(move || {
                        let mut on = 0u32;
                        for i in 0..N {
                            let name = if i & 1 == 0 { "rxnap" } else { "framebufs" };
                            on += std::hint::black_box(f(std::hint::black_box(name))).unwrap_or(true) as u32;
                        }
                        std::hint::black_box(on);
                    });
                }
            });
            t.elapsed().as_nanos() as f64 / N as f64
        }
        for (what, f) in [("lock-free", set as fn(&str) -> Option<bool>), ("mutex (old)", locked)] {
            run(f, 1); // warm
            let one = run(f, 1);
            let four = run(f, 4);
            println!("tune::set {what}: {one:.1} ns/call on 1 thread, {four:.1} ns/call per thread with 4 contending");
        }
    }
}
