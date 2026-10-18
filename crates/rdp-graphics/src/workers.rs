//! The decoders' worker threads: one pool for the process, started on first
//! use and kept. A frame's parallel work (progressive tiles, a CPU colour
//! conversion) used to start and join its own OS threads every time — ~25 µs
//! each, a few hundred a second while the screen scrolls, every core woken
//! for it. Pool threads sleep between frames instead, and a job is handed out
//! in microseconds. Jobs are scoped (they borrow the frame), so this crate
//! still needs no `unsafe`.

use std::sync::OnceLock;

/// How many threads a frame's work may be split over (the calling thread
/// included): the machine's, at most 16.
pub fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(16)
}

/// The pool (the caller works too, so one thread fewer than [`threads`]).
/// `None` where it couldn't start: callers then do the work themselves.
pub(crate) fn pool() -> Option<&'static rayon_core::ThreadPool> {
    static POOL: OnceLock<Option<rayon_core::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = threads().saturating_sub(1);
        if n == 0 {
            return None;
        }
        rayon_core::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("rdp-decode-{i}"))
            .build()
            .map_err(|e| tracing::warn!(error = %e, "no decode thread pool; decoding on one thread"))
            .ok()
    })
    .as_ref()
}
