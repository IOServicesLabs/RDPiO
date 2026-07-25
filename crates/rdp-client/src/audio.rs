//! Win32 `waveOut` audio sink for the rdpsnd channel.
//!
//! Implements [`rdp_channels::rdpsnd::AudioSink`] over the legacy but simple
//! `waveOut` API: open the default device for a PCM format, and submit each wave
//! buffer with `waveOutWrite`. Each submitted buffer (its `WAVEHDR` and the PCM
//! bytes) is kept alive until the device flags it `WHDR_DONE`, then unprepared
//! and freed. Runs on the session worker thread. Blind Windows FFI — never run.
//!
//! Compressed formats (AAC/HE-AAC advertised by the RDPSND channel) are decoded
//! to PCM through a Media Foundation transform before playback.

use rdp_channels::rdpsnd::{AudioFormat, AudioSink};
use windows::core::PSTR;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::{
    waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader,
    waveOutWrite, CALLBACK_NULL, HWAVEOUT, WAVEFORMATEX, WAVEHDR, WAVE_FORMAT_PCM, WAVE_MAPPER,
    WHDR_DONE,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFInitMediaTypeFromWaveFormatEx, MFStartup, MFT_CATEGORY_AUDIO_DECODER,
    MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STATUS_SAMPLE_READY, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_VERSION, MFSTARTUP_LITE,
};

/// `MMSYSERR_NOERROR` — a `waveOut*` call succeeded.
const MM_OK: u32 = 0;

/// Maximum queued-but-unplayed audio, in milliseconds. `waveOut` is a gapless
/// FIFO: whatever we write plays back in order at exactly the sample rate, so any
/// backlog it accumulates (from the server front-running audio in bursts, or from
/// audio arriving bursty behind heavy video on a shared TCP link) becomes
/// *permanent* latency — audio drifts behind video and never recovers. When the
/// backlog crosses this cap we flush and resync (one brief discontinuity) so
/// audio stays lip-synced with the graphics stream. ~180 ms is low enough to keep
/// sync tight while leaving headroom for normal network jitter so resyncs are rare.
const MAX_QUEUED_MS: u64 = 180;

/// Ensure Media Foundation is initialized once per process.
fn init_media_foundation() -> windows::core::Result<()> {
    static INIT: std::sync::Once = std::sync::Once::new();
    static mut OK: bool = false;
    unsafe {
        INIT.call_once(|| {
            OK = MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok();
        });
        if OK {
            Ok(())
        } else {
            Err(windows::core::Error::new(E_FAIL, "MFStartup failed"))
        }
    }
}

/// Media Foundation AAC/HE-AAC decoder producing 16-bit PCM.
struct AacDecoder {
    transform: IMFTransform,
    pcm_format: (u16, u32, u16),
}

impl AacDecoder {
    /// Create a decoder for `format`. Returns `None` if Media Foundation cannot
    /// build an AAC MFT for the requested channels/rate.
    unsafe fn new(format: &AudioFormat) -> Option<Self> {
        let _ = init_media_foundation().ok()?;

        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        windows::Win32::Media::MediaFoundation::MFTEnumEx(
            MFT_CATEGORY_AUDIO_DECODER,
            MFT_ENUM_FLAG_SORTANDFILTER | MFT_ENUM_FLAG_ALL,
            None,
            None,
            &mut devices,
            &mut count,
        )
        .ok()?;
        if count == 0 || devices.is_null() {
            return None;
        }
        let list = std::slice::from_raw_parts(devices, count as usize);
        let activate = list.first().and_then(|d| d.as_ref())?;
        let transform: IMFTransform = activate.ActivateObject().ok()?;

        // Build input WAVEFORMATEX with the format-specific extra bytes.
        let block_align = if format.tag == WAVE_FORMAT_PCM as u16 {
            format.channels * 2
        } else {
            1
        };
        let wfx_input = WAVEFORMATEX {
            wFormatTag: format.tag,
            nChannels: format.channels,
            nSamplesPerSec: format.samples_per_sec,
            nAvgBytesPerSec: format.samples_per_sec * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: format.bits_per_sample,
            cbSize: format.extra.len() as u16,
        };
        let mut wfx_buf = vec![0u8; std::mem::size_of::<WAVEFORMATEX>() + format.extra.len()];
        std::ptr::copy_nonoverlapping(
            &wfx_input as *const _ as *const u8,
            wfx_buf.as_mut_ptr(),
            std::mem::size_of::<WAVEFORMATEX>(),
        );
        if !format.extra.is_empty() {
            std::ptr::copy_nonoverlapping(
                format.extra.as_ptr(),
                wfx_buf.as_mut_ptr().add(std::mem::size_of::<WAVEFORMATEX>()),
                format.extra.len(),
            );
        }

        let input_type: IMFMediaType = MFCreateMediaType().ok()?;
        MFInitMediaTypeFromWaveFormatEx(
            &input_type,
            wfx_buf.as_ptr() as *const WAVEFORMATEX,
            wfx_buf.len() as u32,
        )
        .ok()?;
        transform.SetInputType(0, &input_type, 0).ok()?;

        // Output: 16-bit PCM at the same sample rate / channel count.
        let bits = 16u16;
        let out_block = format.channels * (bits / 8);
        let wfx_output = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: format.channels,
            nSamplesPerSec: format.samples_per_sec,
            nAvgBytesPerSec: format.samples_per_sec * out_block as u32,
            nBlockAlign: out_block,
            wBitsPerSample: bits,
            cbSize: 0,
        };
        let output_type: IMFMediaType = MFCreateMediaType().ok()?;
        MFInitMediaTypeFromWaveFormatEx(
            &output_type,
            &wfx_output,
            std::mem::size_of::<WAVEFORMATEX>() as u32,
        )
        .ok()?;
        transform.SetOutputType(0, &output_type, 0).ok()?;

        transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok()?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .ok()?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .ok()?;

        Some(Self {
            transform,
            pcm_format: (format.channels, format.samples_per_sec, bits),
        })
    }

    /// Decode one compressed frame. Returns the decoded PCM bytes, or `None` if
    /// the decoder needs more input or hit an error.
    unsafe fn decode(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.is_empty() {
            return None;
        }

        // Input sample.
        let buffer = MFCreateMemoryBuffer(payload.len() as u32).ok()?;
        {
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer.Lock(&mut ptr, Some(&mut max_len), None).ok()?;
            std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr as *mut u8, payload.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(payload.len() as u32).ok()?;
        }
        let sample: IMFSample = MFCreateSample().ok()?;
        sample.AddBuffer(&buffer).ok()?;

        self.transform.ProcessInput(0, &sample, 0).ok()?;

        // Drain output.
        let mut pcm = Vec::new();
        loop {
            let mut status = 0u32;
            let mut output_data = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let hr = self
                .transform
                .ProcessOutput(0, std::slice::from_mut(&mut output_data), &mut status);
            if let Err(e) = hr {
                let code = e.code().0;
                let _ = output_data.pSample.take();
                if code == MF_E_TRANSFORM_NEED_MORE_INPUT.0 {
                    break;
                }
                if code == MF_E_TRANSFORM_STREAM_CHANGE.0 {
                    continue;
                }
                break;
            }
            if status as i32 != MFT_OUTPUT_STATUS_SAMPLE_READY.0 {
                let _ = output_data.pSample.take();
                break;
            }
            if let Some(out_sample) = output_data.pSample.as_ref() {
                let out_buf = out_sample.GetBufferByIndex(0).ok()?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                let mut max_len = 0u32;
                out_buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut len))
                    .ok()?;
                let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
                pcm.extend_from_slice(slice);
                let _ = out_buf.Unlock();
            }
            // `pSample` is owned by the MFT_OUTPUT_DATA_BUFFER; release it.
            let _ = output_data.pSample.take();
        }

        if pcm.is_empty() {
            None
        } else {
            Some(pcm)
        }
    }
}

/// A `waveOut` audio output device exposed as an rdpsnd sink.
pub struct Win32Audio {
    handle: Option<HWAVEOUT>,
    format: Option<(u16, u32, u16)>,
    /// In-flight buffers: the boxed `WAVEHDR` (stable address for `waveOut`) and
    /// the PCM bytes it points at, held until the device is done with them.
    pending: Vec<(Box<WAVEHDR>, Vec<u8>)>,
    /// AAC/HE-AAC decoder, created on first compressed frame.
    aac_decoder: Option<AacDecoder>,
}

// SAFETY: the sink is created on the UI thread and moved once into the session
// worker thread, then only ever touched there. The `WAVEHDR` raw pointers it
// holds are owned (not shared) and `waveOut` handles are not thread-affine, so
// the single-owner move across threads is sound.
unsafe impl Send for Win32Audio {}

impl Win32Audio {
    pub fn new() -> Self {
        Self {
            handle: None,
            format: None,
            pending: Vec::new(),
            aac_decoder: None,
        }
    }

    const HDR_SIZE: u32 = std::mem::size_of::<WAVEHDR>() as u32;

    /// Unprepare and free any buffers the device has finished playing.
    unsafe fn reclaim(&mut self) {
        let Some(h) = self.handle else {
            return;
        };
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].0.dwFlags & WHDR_DONE != 0 {
                let (mut hdr, _data) = self.pending.remove(i);
                let _ = waveOutUnprepareHeader(h, &mut *hdr, Self::HDR_SIZE);
                // hdr + data drop here, after the device released them.
            } else {
                i += 1;
            }
        }
    }

    /// Bytes played per second in the current format (0 if no format is set).
    fn bytes_per_sec(&self) -> u64 {
        match self.format {
            Some((channels, samples_per_sec, bits_per_sample)) => {
                samples_per_sec as u64 * channels as u64 * (bits_per_sample as u64 / 8)
            }
            None => 0,
        }
    }

    /// Queued-but-unplayed audio, in milliseconds. `reclaim()` has already dropped
    /// finished buffers, so the remaining `pending` bytes approximate the backlog
    /// (an over-estimate by at most the partially-played head buffer).
    fn queued_ms(&self) -> u64 {
        let bps = self.bytes_per_sec();
        if bps == 0 {
            return 0;
        }
        let queued: u64 = self.pending.iter().map(|(h, _)| h.dwBufferLength as u64).sum();
        queued * 1000 / bps
    }

    /// If the backlog has grown past [`MAX_QUEUED_MS`], flush the device so audio
    /// snaps back to low latency instead of drifting ever further behind video.
    /// Costs one brief audible discontinuity; only fires when we're already
    /// audibly out of sync, so it's a net win.
    unsafe fn resync_if_behind(&mut self) {
        let Some(h) = self.handle else {
            return;
        };
        let behind = self.queued_ms();
        if behind <= MAX_QUEUED_MS {
            return;
        }
        // `waveOutReset` marks every queued buffer done; unprepare and drop them.
        let _ = waveOutReset(h);
        for (mut hdr, _data) in self.pending.drain(..) {
            let _ = waveOutUnprepareHeader(h, &mut *hdr, Self::HDR_SIZE);
        }
        tracing::debug!(behind_ms = behind, "audio: backlog exceeded cap; flushed to resync with video");
    }

    /// Reset, drain, and close the device (if open).
    unsafe fn close(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = waveOutReset(h);
            for (hdr, _data) in self.pending.iter_mut() {
                let _ = waveOutUnprepareHeader(h, &mut **hdr, Self::HDR_SIZE);
            }
            self.pending.clear();
            let _ = waveOutClose(h);
        }
        self.format = None;
    }
}

impl AudioSink for Win32Audio {
    fn set_format(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16) {
        // Switching back to PCM from compressed: drop any AAC decoder.
        self.aac_decoder = None;
        if self.handle.is_some() && self.format == Some((channels, samples_per_sec, bits_per_sample))
        {
            return;
        }
        unsafe {
            self.close();
            let block_align = channels * (bits_per_sample / 8);
            let wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: channels,
                nSamplesPerSec: samples_per_sec,
                nAvgBytesPerSec: samples_per_sec * block_align as u32,
                nBlockAlign: block_align,
                wBitsPerSample: bits_per_sample,
                cbSize: 0,
            };
            let mut h = HWAVEOUT::default();
            if waveOutOpen(Some(&mut h), WAVE_MAPPER, &wfx, None, None, CALLBACK_NULL) == MM_OK {
                self.handle = Some(h);
                self.format = Some((channels, samples_per_sec, bits_per_sample));
                tracing::info!(
                    channels,
                    samples_per_sec,
                    bits_per_sample,
                    "audio: waveOut device opened"
                );
            } else {
                tracing::warn!("waveOutOpen failed; audio muted");
            }
        }
    }

    fn play(&mut self, pcm: &[u8]) {
        let Some(h) = self.handle else {
            return;
        };
        if pcm.is_empty() {
            return;
        }
        unsafe {
            self.reclaim();
            // Keep audio from drifting behind video: if the device has built up
            // too much unplayed backlog, flush it before queueing more.
            self.resync_if_behind();
            let mut data = pcm.to_vec();
            let mut hdr = Box::new(WAVEHDR {
                lpData: PSTR(data.as_mut_ptr()),
                dwBufferLength: data.len() as u32,
                ..Default::default()
            });
            if waveOutPrepareHeader(h, &mut *hdr, Self::HDR_SIZE) == MM_OK {
                if waveOutWrite(h, &mut *hdr, Self::HDR_SIZE) == MM_OK {
                    self.pending.push((hdr, data));
                } else {
                    // Write failed: unprepare so the header isn't leaked inside
                    // the driver, then drop the buffer.
                    tracing::warn!("waveOutWrite failed; dropping audio buffer");
                    let _ = waveOutUnprepareHeader(h, &mut *hdr, Self::HDR_SIZE);
                }
            } else {
                tracing::warn!("waveOutPrepareHeader failed; dropping audio buffer");
            }
        }
    }

    fn set_compressed_format(&mut self, format: &AudioFormat) {
        self.aac_decoder = None;
        unsafe {
            match AacDecoder::new(format) {
                Some(dec) => {
                    tracing::info!(
                        tag = format.tag,
                        channels = format.channels,
                        rate = format.samples_per_sec,
                        "audio: AAC decoder opened"
                    );
                    let pcm_format = dec.pcm_format;
                    self.aac_decoder = Some(dec);
                    // Pre-open waveOut for the PCM the decoder will produce.
                    self.set_format(pcm_format.0, pcm_format.1, pcm_format.2);
                }
                None => {
                    tracing::warn!(tag = format.tag, "audio: failed to open AAC decoder");
                }
            }
        }
    }

    fn play_compressed(&mut self, _format: &AudioFormat, payload: &[u8]) {
        unsafe {
            let Some(dec) = self.aac_decoder.as_ref() else {
                return;
            };
            if let Some(pcm) = dec.decode(payload) {
                self.play(&pcm);
            }
        }
    }
}

impl Drop for Win32Audio {
    fn drop(&mut self) {
        unsafe { self.close() }
    }
}
