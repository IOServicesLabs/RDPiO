//! Win32 `waveIn` microphone capture for the MS-RDPEAI audio-input channel.
//!
//! Implements [`rdp_client::session::MicSource`](crate::session::MicSource) over
//! the legacy but simple `waveIn` API: open the default capture device for the
//! PCM format the server requested, keep a ring of recording buffers queued, and
//! on each poll harvest the buffers the device has filled and requeue them. The
//! captured PCM is forwarded to the server by the graphics session loop. Created
//! on the session worker thread. Blind Windows FFI — never executed here.

use crate::session::MicSource;
use windows::core::PSTR;
use windows::Win32::Media::Audio::{
    waveInAddBuffer, waveInClose, waveInGetNumDevs, waveInOpen, waveInPrepareHeader, waveInReset,
    waveInStart, waveInUnprepareHeader, CALLBACK_NULL, HWAVEIN, WAVEFORMATEX, WAVEHDR,
    WAVE_FORMAT_PCM, WAVE_MAPPER, WHDR_DONE,
};

/// `MMSYSERR_NOERROR` — a `waveIn*` call succeeded.
const MM_OK: u32 = 0;
/// Number of capture buffers kept queued with the device.
const NUM_BUFFERS: usize = 4;

/// A `waveIn` capture device exposed as a [`MicSource`].
pub struct Win32Mic {
    handle: Option<HWAVEIN>,
    /// Queued recording buffers: the boxed `WAVEHDR` (stable address for the
    /// device) and the backing PCM bytes it records into.
    buffers: Vec<(Box<WAVEHDR>, Vec<u8>)>,
}

// SAFETY: created on the UI thread and moved once into the session worker
// thread, then only ever touched there. The `WAVEHDR` raw pointers it holds are
// owned (not shared) and `waveIn` handles are not thread-affine, so the
// single-owner move across threads is sound (mirrors `Win32Audio`).
unsafe impl Send for Win32Mic {}

impl Win32Mic {
    const HDR_SIZE: u32 = std::mem::size_of::<WAVEHDR>() as u32;

    /// Create a capture source if the system has at least one input device.
    /// Returns `None` when there's no microphone (the session then runs without
    /// audio input). The device is opened lazily in [`start`](MicSource::start),
    /// once the server picks a format.
    pub fn new() -> Option<Self> {
        // Safe: `waveInGetNumDevs` just reads the device count.
        let devices = unsafe { waveInGetNumDevs() };
        if devices == 0 {
            tracing::info!("no audio capture device; microphone redirection disabled");
            return None;
        }
        Some(Self {
            handle: None,
            buffers: Vec::new(),
        })
    }

    /// Stop, drain, and close the device (if open), freeing its buffers.
    unsafe fn close(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = waveInReset(h); // marks all buffers done, stops recording
            for (hdr, _data) in self.buffers.iter_mut() {
                let _ = waveInUnprepareHeader(h, &mut **hdr, Self::HDR_SIZE);
            }
            self.buffers.clear();
            let _ = waveInClose(h);
        }
    }
}

impl MicSource for Win32Mic {
    fn start(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16) {
        unsafe {
            self.close();
            let block_align = channels.max(1) * (bits_per_sample / 8).max(1);
            let avg = samples_per_sec * block_align as u32;
            let wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: channels,
                nSamplesPerSec: samples_per_sec,
                nAvgBytesPerSec: avg,
                nBlockAlign: block_align,
                wBitsPerSample: bits_per_sample,
                cbSize: 0,
            };
            let mut h = HWAVEIN::default();
            if waveInOpen(Some(&mut h), WAVE_MAPPER, &wfx, None, None, CALLBACK_NULL) != MM_OK {
                tracing::warn!("waveInOpen failed; microphone disabled");
                return;
            }
            // ~50 ms per buffer, aligned to a whole number of sample frames.
            let mut buf_len = (avg as usize / 20).max(block_align as usize);
            buf_len -= buf_len % block_align as usize;
            for _ in 0..NUM_BUFFERS {
                let mut data = vec![0u8; buf_len];
                let mut hdr = Box::new(WAVEHDR {
                    lpData: PSTR(data.as_mut_ptr()),
                    dwBufferLength: data.len() as u32,
                    ..Default::default()
                });
                if waveInPrepareHeader(h, &mut *hdr, Self::HDR_SIZE) == MM_OK
                    && waveInAddBuffer(h, &mut *hdr, Self::HDR_SIZE) == MM_OK
                {
                    self.buffers.push((hdr, data));
                }
            }
            if self.buffers.is_empty() {
                tracing::warn!("waveIn buffer setup failed; microphone disabled");
                let _ = waveInClose(h);
                return;
            }
            self.handle = Some(h);
            let _ = waveInStart(h);
        }
    }

    fn poll(&mut self) -> Vec<u8> {
        let Some(h) = self.handle else {
            return Vec::new();
        };
        let mut pcm = Vec::new();
        unsafe {
            // Harvest every buffer the device has filled, in queue order, and
            // immediately requeue it (which clears WHDR_DONE and resets the
            // recorded length) so capture stays continuous.
            for (hdr, data) in self.buffers.iter_mut() {
                if hdr.dwFlags & WHDR_DONE != 0 {
                    let n = (hdr.dwBytesRecorded as usize).min(data.len());
                    pcm.extend_from_slice(&data[..n]);
                    let _ = waveInAddBuffer(h, &mut **hdr, Self::HDR_SIZE);
                }
            }
        }
        pcm
    }
}

impl Drop for Win32Mic {
    fn drop(&mut self) {
        unsafe { self.close() }
    }
}
