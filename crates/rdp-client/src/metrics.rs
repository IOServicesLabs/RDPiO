use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::allocator;

/// Lightweight performance telemetry for the RDP client.
///
/// Designed to be cheap enough to leave enabled in release builds: timing samples
/// are pushed into small per-metric vectors that are drained and logged every
/// 10 seconds, and counters are atomics. Allocation counts come from the
/// process-global [`crate::allocator::TrackingAllocator`].
#[derive(Default)]
pub struct Metrics {
    // Timing samples in microseconds.
    decode_us: Mutex<Vec<u64>>,
    present_us: Mutex<Vec<u64>>,
    frame_interval_us: Mutex<Vec<u64>>,
    gpu_decode_us: Mutex<Vec<u64>>,
    gpu_present_us: Mutex<Vec<u64>>,
    rtt_us: Mutex<Vec<u64>>,

    // Counters.
    /// Frames the server finished sending (EndFrame), counted on the network
    /// thread: the host's real frame rate, independent of the local display's
    /// refresh rate (which caps `frames_presented` under vsync).
    frames_received: AtomicU64,
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
    blits_submitted: AtomicU64,
    network_bytes_rx: AtomicU64,
    network_bytes_tx: AtomicU64,
    /// Most frames waiting for the decode thread at any frame-ack in this
    /// window (the RDP graphics path's decode backlog).
    decode_backlog_peak: AtomicU64,

    /// What the RDP network thread has handed the decode thread and what the
    /// decode thread has finished; flow control reads it (see [`DecodeQueue`]).
    pub decode_queue: DecodeQueue,
}

/// Graphics work handed from the RDP network thread to its decode thread and
/// not yet decoded, kept as monotonic counters on both sides (atomics only:
/// the network thread adds when it queues a batch, the decode thread adds
/// when it has rendered one), so neither side ever waits for the other.
#[derive(Default)]
pub struct DecodeQueue {
    /// Set by the decode loop that keeps `frames_decoded`/`bytes_decoded`.
    /// Until it is, nothing may be held for decode progress (a decode loop
    /// that never reports would otherwise look permanently behind).
    tracked: AtomicBool,
    frames_queued: AtomicU64,
    frames_decoded: AtomicU64,
    bytes_queued: AtomicU64,
    bytes_decoded: AtomicU64,
    /// The network thread holds frame acks until decode catches up; the
    /// decode thread wakes it when this is set.
    acks_held: AtomicBool,
}

impl DecodeQueue {
    /// Zero the counters for a new connection. Called by the network thread
    /// before it queues anything, while the decode thread is idle.
    pub fn reset(&self) {
        self.frames_queued.store(0, Ordering::Relaxed);
        self.frames_decoded.store(0, Ordering::Relaxed);
        self.bytes_queued.store(0, Ordering::Relaxed);
        self.bytes_decoded.store(0, Ordering::Relaxed);
        self.acks_held.store(false, Ordering::Relaxed);
    }

    pub fn set_tracked(&self, on: bool) {
        self.tracked.store(on, Ordering::Release);
    }

    pub fn tracked(&self) -> bool {
        self.tracked.load(Ordering::Acquire)
    }

    /// The network thread queued `frames` complete frames, `bytes` of payload.
    pub fn queued(&self, frames: u64, bytes: u64) {
        self.frames_queued.fetch_add(frames, Ordering::Relaxed);
        self.bytes_queued.fetch_add(bytes, Ordering::Relaxed);
    }

    /// The decode thread finished `frames` frames, `bytes` of payload.
    /// Returns true when the network thread is holding acks for it.
    ///
    /// SeqCst here and in [`set_acks_held`](Self::set_acks_held) /
    /// [`frames_in_flight`](Self::frames_in_flight): the network thread sets the
    /// flag and then re-reads progress, the decode thread adds progress and then
    /// reads the flag, so at least one of them sees the other and a held ack
    /// is never left waiting for a wake that does not come.
    pub fn decoded(&self, frames: u64, bytes: u64) -> bool {
        self.bytes_decoded.fetch_add(bytes, Ordering::Relaxed);
        self.frames_decoded.fetch_add(frames, Ordering::SeqCst);
        self.acks_held.load(Ordering::SeqCst)
    }

    /// Frames queued and not yet decoded.
    pub fn frames_in_flight(&self) -> u64 {
        let done = self.frames_decoded.load(Ordering::SeqCst);
        self.frames_queued.load(Ordering::Relaxed).saturating_sub(done)
    }

    /// Payload bytes queued and not yet decoded.
    pub fn bytes_in_flight(&self) -> u64 {
        let done = self.bytes_decoded.load(Ordering::Relaxed);
        self.bytes_queued.load(Ordering::Relaxed).saturating_sub(done)
    }

    pub fn set_acks_held(&self, held: bool) {
        self.acks_held.store(held, Ordering::SeqCst);
    }
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_decode_us(&self, us: u64) {
        self.decode_us.lock().unwrap().push(us);
        self.frames_decoded.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_frame_received(&self) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_present_us(&self, us: u64) {
        self.present_us.lock().unwrap().push(us);
        self.frames_presented.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_frame_interval_us(&self, us: u64) {
        self.frame_interval_us.lock().unwrap().push(us);
    }

    #[allow(dead_code)] // wired up in the D3D11 GPU timing task
    pub fn record_gpu_decode_us(&self, us: u64) {
        self.gpu_decode_us.lock().unwrap().push(us);
    }

    #[allow(dead_code)] // wired up in the D3D11 GPU timing task
    pub fn record_gpu_present_us(&self, us: u64) {
        self.gpu_present_us.lock().unwrap().push(us);
    }

    pub fn record_rtt_us(&self, us: u64) {
        self.rtt_us.lock().unwrap().push(us);
    }

    /// Frames waiting for the decode thread when a frame ack was due; the
    /// window keeps the peak.
    pub fn record_decode_backlog(&self, frames: u64) {
        self.decode_backlog_peak.fetch_max(frames, Ordering::Relaxed);
    }

    pub fn record_blit(&self, _bytes: u64) {
        self.blits_submitted.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)] // wired up in the IOCP / network-accounting task
    pub fn add_network_bytes(&self, rx: u64, tx: u64) {
        self.network_bytes_rx.fetch_add(rx, Ordering::Relaxed);
        self.network_bytes_tx.fetch_add(tx, Ordering::Relaxed);
    }

    /// Drain all samples/counters and return a snapshot suitable for logging.
    pub fn report_and_reset(&self) -> MetricsReport {
        let decode = take_sorted(&self.decode_us);
        let present = take_sorted(&self.present_us);
        let frame_interval = take_sorted(&self.frame_interval_us);
        let gpu_decode = take_sorted(&self.gpu_decode_us);
        let gpu_present = take_sorted(&self.gpu_present_us);
        let rtt = take_sorted(&self.rtt_us);
        let (allocations, bytes_allocated) = allocator::drain();

        MetricsReport {
            decode_p50: percentile(&decode, 0.50),
            decode_p99: percentile(&decode, 0.99),
            decode_p999: percentile(&decode, 0.999),
            present_p50: percentile(&present, 0.50),
            present_p99: percentile(&present, 0.99),
            present_p999: percentile(&present, 0.999),
            frame_interval_p50: percentile(&frame_interval, 0.50),
            frame_interval_p99: percentile(&frame_interval, 0.99),
            frames_received: self.frames_received.swap(0, Ordering::Relaxed),
            frames_decoded: self.frames_decoded.swap(0, Ordering::Relaxed),
            frames_presented: self.frames_presented.swap(0, Ordering::Relaxed),
            blits_submitted: self.blits_submitted.swap(0, Ordering::Relaxed),
            allocations,
            bytes_allocated,
            gpu_decode_p99: percentile(&gpu_decode, 0.99),
            gpu_present_p99: percentile(&gpu_present, 0.99),
            rtt_p50: percentile(&rtt, 0.50),
            rtt_p99: percentile(&rtt, 0.99),
            network_rx_bytes: self.network_bytes_rx.swap(0, Ordering::Relaxed),
            network_tx_bytes: self.network_bytes_tx.swap(0, Ordering::Relaxed),
            decode_backlog_peak: self.decode_backlog_peak.swap(0, Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
pub struct MetricsReport {
    pub decode_p50: u64,
    pub decode_p99: u64,
    pub decode_p999: u64,
    pub present_p50: u64,
    pub present_p99: u64,
    pub present_p999: u64,
    pub frame_interval_p50: u64,
    pub frame_interval_p99: u64,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    pub blits_submitted: u64,
    pub allocations: u64,
    pub bytes_allocated: u64,
    pub gpu_decode_p99: u64,
    pub gpu_present_p99: u64,
    pub rtt_p50: u64,
    pub rtt_p99: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    /// Peak frames waiting for the decode thread (0 while decode keeps up).
    pub decode_backlog_peak: u64,
}

impl MetricsReport {
    /// True if any frames were observed in this window.
    pub fn has_data(&self) -> bool {
        self.frames_decoded > 0
            || self.frames_received > 0
            || self.frames_presented > 0
            || self.allocations > 0
            || self.network_rx_bytes > 0
            || self.network_tx_bytes > 0
    }

    /// Format the report as a concise one-line summary.
    pub fn summary(&self, window: std::time::Duration) -> String {
        let secs = window.as_secs_f64().max(0.001);
        let host_fps = self.frames_received as f64 / secs;
        let shown_fps = self.frames_presented as f64 / secs;
        let fps = if self.frame_interval_p50 > 0 {
            1_000_000.0 / self.frame_interval_p50 as f64
        } else {
            0.0
        };
        format!(
            "metrics: host_fps={host_fps:.1} shown_fps={shown_fps:.1} fps={fps:.1} dec={}/{}/{}us present={}/{}/{}us interval={}/{}us backlog={} blits={} allocs={}/{}MB gpu={}/{}us rtt={}/{}us net={}/{}MB",
            self.decode_p50,
            self.decode_p99,
            self.decode_p999,
            self.present_p50,
            self.present_p99,
            self.present_p999,
            self.frame_interval_p50,
            self.frame_interval_p99,
            self.decode_backlog_peak,
            self.blits_submitted,
            self.allocations,
            self.bytes_allocated / 1_000_000,
            self.gpu_decode_p99,
            self.gpu_present_p99,
            self.rtt_p50,
            self.rtt_p99,
            self.network_rx_bytes / 1_000_000,
            self.network_tx_bytes / 1_000_000,
        )
    }
}

fn take_sorted(v: &Mutex<Vec<u64>>) -> Vec<u64> {
    let mut guard = v.lock().unwrap();
    let mut data = Vec::new();
    std::mem::swap(&mut data, &mut *guard);
    data.sort_unstable();
    data
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p) as usize;
    sorted[idx.min(sorted.len() - 1)]
}
