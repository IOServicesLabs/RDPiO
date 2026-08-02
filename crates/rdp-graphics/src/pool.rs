//! Simple capacity-bucketed `Vec<u8>` pool for decode-thread allocations.
//!
//! The progressive tile decoder produces thousands of 64×64 RGBA buffers per
//! second. A pooled buffer avoids the per-tile heap trip when the same size is
//! reused, which is the common case for full tiles and band-assembly scratch.

/// A bucketed pool of `Vec<u8>` buffers indexed by power-of-two capacity.
///
/// Buffers are handed out with at least the requested length; capacity is
/// preserved across reuse so repeated same-size allocations amortize to one
/// initial allocation. The pool caps retained buffers so memory does not grow
/// without bound if the caller suddenly needs larger sizes.
#[derive(Debug, Default)]
pub struct BufferPool {
    /// Buckets where bucket `i` holds buffers with capacity in `[2^i, 2^(i+1))`.
    buckets: Vec<Vec<Vec<u8>>>,
    /// Maximum buffers retained in any one bucket.
    max_per_bucket: usize,
}

impl BufferPool {
    /// Create an empty pool with a default retention limit.
    pub fn new() -> Self {
        Self::with_limit(8)
    }

    /// Create an empty pool retaining at most `max_per_bucket` buffers per
    /// capacity bucket.
    pub fn with_limit(max_per_bucket: usize) -> Self {
        Self {
            buckets: Vec::new(),
            max_per_bucket,
        }
    }

    /// Acquire a buffer with length at least `len`. Capacity may be larger.
    pub fn acquire(&mut self, len: usize) -> Vec<u8> {
        let idx = bucket_index(len);
        if idx < self.buckets.len() {
            // Take only the matching buffer; the rest of the bucket must stay
            // pooled (a `drain(..)` here would free every remaining buffer the
            // moment we returned, silently degrading the pool to malloc-per-tile).
            let bucket = &mut self.buckets[idx];
            if let Some(pos) = bucket.iter().position(|c| c.capacity() >= len) {
                let mut candidate = bucket.swap_remove(pos);
                candidate.clear();
                candidate.resize(len, 0);
                return candidate;
            }
        }
        vec![0u8; len]
    }

    /// Return a buffer to the pool. The buffer is cleared and capacity is
    /// retained for reuse. Buffers whose capacity is tiny or whose bucket is
    /// over the retention limit are dropped.
    pub fn release(&mut self, mut buf: Vec<u8>) {
        let len = buf.capacity();
        if len < 64 {
            return; // not worth the bookkeeping
        }
        buf.clear();
        let idx = bucket_index(len);
        if self.buckets.len() <= idx {
            self.buckets.resize_with(idx + 1, Vec::new);
        }
        let bucket = &mut self.buckets[idx];
        if bucket.len() < self.max_per_bucket {
            bucket.push(buf);
        }
    }
}

fn bucket_index(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    (usize::BITS - (capacity - 1).leading_zeros()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooled_buffer_is_zeroed_and_sized() {
        let mut pool = BufferPool::new();
        let buf = pool.acquire(1024);
        assert_eq!(buf.len(), 1024);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn release_and_reuse_preserves_capacity() {
        let mut pool = BufferPool::new();
        let buf = pool.acquire(1000);
        let cap = buf.capacity();
        pool.release(buf);
        let buf2 = pool.acquire(1000);
        assert_eq!(buf2.capacity(), cap);
        assert_eq!(buf2.len(), 1000);
    }

    #[test]
    fn acquire_keeps_other_pooled_buffers() {
        let mut pool = BufferPool::new();
        for _ in 0..4 {
            pool.release(Vec::with_capacity(1024));
        }
        let _one = pool.acquire(1024);
        assert_eq!(pool.buckets[bucket_index(1024)].len(), 3);
    }

    #[test]
    fn retention_limit_respected() {
        let mut pool = BufferPool::with_limit(2);
        for _ in 0..10 {
            pool.release(vec![0u8; 256]);
        }
        assert_eq!(pool.buckets[bucket_index(256)].len(), 2);
    }
}
