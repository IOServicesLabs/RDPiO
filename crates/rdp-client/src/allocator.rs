//! Process-global allocation accounting.
//!
//! Wraps the system allocator so the telemetry line can report true allocator
//! pressure (allocation count and bytes allocated) across the whole process.
//! This is intentionally simple: atomic counters are incremented on every
//! allocate/deallocate/reallocate, and the per-window Metrics report drains
//! them. The overhead is two relaxed atomic adds per allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

pub struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOC_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        let old_size = layout.size() as u64;
        let delta = new_size as i64 - old_size as i64;
        if delta >= 0 {
            ALLOC_BYTES.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            ALLOC_BYTES.fetch_sub((-delta) as u64, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

/// Read and reset the global allocation counters, returning `(count, bytes)`.
pub fn drain() -> (u64, u64) {
    (
        ALLOC_COUNT.swap(0, Ordering::Relaxed),
        ALLOC_BYTES.swap(0, Ordering::Relaxed),
    )
}
