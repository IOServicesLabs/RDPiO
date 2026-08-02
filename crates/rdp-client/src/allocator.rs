//! Process-global allocation accounting, over mimalloc.
//!
//! Two jobs in one type:
//!
//! 1. **The allocator itself is mimalloc**, not the platform default. On Windows
//!    `System` is `HeapAlloc`, whose per-heap lock is contended by our three hot
//!    threads (network receive, decode, UI/present) — and the tile codecs
//!    (ClearCodec, progressive, planar) allocate a buffer per tile, hundreds per
//!    frame. mimalloc's per-thread free lists serve those from thread-local
//!    storage with no cross-thread lock, which is exactly this workload's shape.
//!
//! 2. **Accounting**, so the telemetry line can report allocator pressure across
//!    the whole process: how many allocations happened in a reporting window and
//!    how many bytes they asked for. Two relaxed atomic adds per allocation; the
//!    per-window Metrics report drains them.
//!
//!    Both counters are add-only. Tracking *live* bytes instead — adding on
//!    alloc, subtracting on free — underflows as soon as a window boundary falls
//!    between an allocation and its matching free, since `drain` resets the
//!    counter to zero and the later free then subtracts from it. That printed
//!    `allocs=1872/18446744073709MB` in the metrics line. Counting only what was
//!    requested is both the more useful number and one that cannot wrap.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};

use mimalloc::MiMalloc;

/// The real allocator behind the counters.
static INNER: MiMalloc = MiMalloc;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

pub struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        INNER.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        INNER.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // Only growth counts as new allocation pressure; a shrink returns memory.
        ALLOC_BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        INNER.realloc(ptr, layout, new_size)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Zeroed allocations are common in the decode path (`vec![0u8; n]` for
        // every tile buffer). Forwarding instead of falling back to
        // alloc-then-memset lets mimalloc hand back already-zero OS pages.
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        INNER.alloc_zeroed(layout)
    }
}

/// Read and reset the global allocation counters, returning `(count, bytes)`.
pub fn drain() -> (u64, u64) {
    (
        ALLOC_COUNT.swap(0, Ordering::Relaxed),
        ALLOC_BYTES.swap(0, Ordering::Relaxed),
    )
}
