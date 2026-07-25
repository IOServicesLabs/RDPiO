use std::sync::atomic::{AtomicU64, Ordering};
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
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
    blits_submitted: AtomicU64,
    network_bytes_rx: AtomicU64,
    network_bytes_tx: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_decode_us(&self, us: u64) {
        self.decode_us.lock().unwrap().push(us);
        self.frames_decoded.fetch_add(1, Ordering::Relaxed);
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
}

impl MetricsReport {
    /// True if any frames were observed in this window.
    pub fn has_data(&self) -> bool {
        self.frames_decoded > 0
            || self.frames_presented > 0
            || self.allocations > 0
            || self.network_rx_bytes > 0
            || self.network_tx_bytes > 0
    }

    /// Format the report as a concise one-line summary.
    pub fn summary(&self) -> String {
        let fps = if self.frame_interval_p50 > 0 {
            1_000_000.0 / self.frame_interval_p50 as f64
        } else {
            0.0
        };
        format!(
            "metrics: fps={fps:.1} dec={}/{}/{}us present={}/{}/{}us interval={}/{}us blits={} allocs={}/{}MB gpu={}/{}us rtt={}/{}us net={}/{}MB",
            self.decode_p50,
            self.decode_p99,
            self.decode_p999,
            self.present_p50,
            self.present_p99,
            self.present_p999,
            self.frame_interval_p50,
            self.frame_interval_p99,
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
