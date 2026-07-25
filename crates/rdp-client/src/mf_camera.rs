//! Win32 Media Foundation webcam capture for camera redirection (MS-RDPECAM).
//!
//! Enumerates the system's video capture devices and, on request, captures NV12
//! frames from one of them. `ReadSample` blocks, so capture runs on a dedicated
//! worker thread that pushes frames into a shared queue; the session drains the
//! queue (non-blocking) and forwards frames as SampleResponse PDUs.
//!
//! Provides the device list + frame source the MS-RDPECAM state machines in
//! [`rdp_channels::camera`] need. If no camera is present, [`MfCamera::enumerate`]
//! returns an empty list and the feature stays off. Blind Windows FFI — never
//! executed here; validated on hardware.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rdp_channels::camera::{CamFormat, CameraDevice, MediaType};
use windows::core::PWSTR;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFStartup, MFMediaType_Video,
    MFVideoFormat_NV12, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION, MFSTARTUP_LITE,
};

use rdp_channels::names::CAMERA_DEVICE_PREFIX;

/// Read the friendly name from a capture-device activation object.
unsafe fn device_name(activate: &IMFActivate) -> String {
    let mut ptr = PWSTR::null();
    let mut len = 0u32;
    if activate
        .GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut ptr, &mut len)
        .is_ok()
        && !ptr.is_null()
    {
        let s = ptr.to_string().unwrap_or_default();
        // The string was allocated by MF; we leak the small buffer rather than
        // pull in CoTaskMemFree plumbing for a one-shot enumeration.
        return s;
    }
    "Camera".to_string()
}

/// Initialize Media Foundation (idempotent) and build the vidcap-source filter.
unsafe fn vidcap_attributes() -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFAttributes>
{
    MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.expect("attributes on success");
    attrs.SetGUID(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    )?;
    Ok(attrs)
}

/// A Media Foundation webcam, captured on a worker thread into a frame queue.
pub struct MfCamera {
    frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MfCamera {
    /// Enumerate the system's cameras as redirectable devices. Empty when there
    /// is no webcam. The channel name is `rdpio_cam{index}` so the DVC demuxer can
    /// recognize the per-device channels the server opens.
    pub fn enumerate() -> Vec<CameraDevice> {
        let mut out = Vec::new();
        unsafe {
            let Ok(attrs) = vidcap_attributes() else {
                return out;
            };
            let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            if MFEnumDeviceSources(&attrs, &mut devices, &mut count).is_err() {
                return out;
            }
            let list = std::slice::from_raw_parts(devices, count as usize);
            for (i, dev) in list.iter().enumerate() {
                if let Some(activate) = dev {
                    out.push(CameraDevice {
                        name: device_name(activate),
                        channel_name: format!("{CAMERA_DEVICE_PREFIX}{i}"),
                    });
                }
            }
        }
        out
    }

    /// The capture formats we offer for a device. H.264 first (compact, WAN-
    /// friendly — encoded on the worker), then raw NV12 as a low-latency LAN
    /// fallback the server can pick instead.
    pub fn media_types() -> Vec<MediaType> {
        vec![
            MediaType { format: CamFormat::H264, width: 1280, height: 720, fps_num: 30, fps_den: 1 },
            MediaType { format: CamFormat::H264, width: 640, height: 480, fps_num: 30, fps_den: 1 },
            MediaType { format: CamFormat::Nv12, width: 640, height: 480, fps_num: 30, fps_den: 1 },
        ]
    }

    /// Begin capturing `device_index` at the given media type on a worker thread.
    pub fn start(device_index: u32, media: MediaType) -> Self {
        let frames = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (frames_w, stop_w) = (frames.clone(), stop.clone());
        let worker = std::thread::spawn(move || unsafe {
            if let Err(e) = capture_loop(device_index, media, &frames_w, &stop_w) {
                tracing::warn!(error = %e, "camera capture loop ended");
            }
        });
        Self {
            frames,
            stop,
            worker: Some(worker),
        }
    }

    /// Drain the most recent captured frame, if any (keeps only the latest to
    /// avoid unbounded queueing if the session falls behind).
    pub fn poll_frame(&self) -> Option<Vec<u8>> {
        let mut q = self.frames.lock().ok()?;
        let latest = q.pop_back();
        q.clear();
        latest
    }
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Activate `device_index`, configure NV12 at the requested size, and pump
/// frames into `frames` until `stop` is set. Runs on the capture worker thread.
unsafe fn capture_loop(
    device_index: u32,
    media: MediaType,
    frames: &Arc<Mutex<VecDeque<Vec<u8>>>>,
    stop: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let attrs = vidcap_attributes()?;
    let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFEnumDeviceSources(&attrs, &mut devices, &mut count)?;
    let list = std::slice::from_raw_parts(devices, count as usize);
    let activate = list
        .get(device_index as usize)
        .and_then(|d| d.as_ref())
        .ok_or_else(windows::core::Error::from_thread)?;

    let source: IMFMediaSource = activate.ActivateObject()?;
    // Enable the source reader's advanced video processing so it inserts format AND
    // resolution converters as needed. Without it, `SetCurrentMediaType(NV12, 720p)`
    // fails `MF_E_TOPO_CODEC_NOT_FOUND (0xC00D5212)` on any camera whose native output
    // isn't already NV12 at that exact size — e.g. MJPEG/YUY2 webcams and virtual
    // cameras (EOS Webcam Utility) that emit one fixed format. With it, MF transcodes
    // whatever the camera produces into the NV12 frames the H.264 encoder wants.
    let reader = {
        let mut reader_attrs = None;
        MFCreateAttributes(&mut reader_attrs, 1)?;
        let reader_attrs = reader_attrs.expect("attributes on success");
        reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromMediaSource(&source, &reader_attrs)?;
        reader
    };

    // Request NV12 at the chosen resolution (the reader converts/scales to it).
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    let frame_size = ((media.width as u64) << 32) | media.height as u64;
    mt.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    reader.SetCurrentMediaType(stream, None, &mt)?;

    // When the server picked H.264, compress each captured NV12 frame on the
    // worker; otherwise send raw NV12. The encoder is best-effort — if it can't
    // be created we fall back to raw NV12 so the camera still streams.
    let fps = media.fps_num.max(1) / media.fps_den.max(1);
    let mut encoder = if media.format == CamFormat::H264 {
        match rdp_gpu::h264::H264Encoder::new(media.width, media.height, fps, 2_000_000) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "H.264 camera encoder unavailable; sending raw NV12");
                None
            }
        }
    } else {
        None
    };

    while !stop.load(Ordering::SeqCst) {
        let mut stream_flags = 0u32;
        let mut sample = None;
        reader.ReadSample(
            stream,
            0,
            None,
            Some(&mut stream_flags),
            None,
            Some(&mut sample),
        )?;
        let Some(sample) = sample else {
            continue; // no frame this iteration (e.g. a format change marker)
        };
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&mut data, Some(&mut max_len), Some(&mut cur_len))?;
        if !data.is_null() && cur_len > 0 {
            let raw = std::slice::from_raw_parts(data, cur_len as usize);
            // Encode to H.264 when negotiated, else forward the raw NV12.
            let frame = match encoder.as_mut() {
                Some(enc) => match enc.encode(raw) {
                    Ok(h264) if !h264.is_empty() => Some(h264),
                    Ok(_) => None, // encoder buffered this frame; nothing to send yet
                    Err(e) => {
                        tracing::debug!(error = %e, "camera H.264 encode failed; skipping frame");
                        None
                    }
                },
                None => Some(raw.to_vec()),
            };
            if let Some(frame) = frame {
                if let Ok(mut q) = frames.lock() {
                    // Keep the queue shallow: drop stale frames so we always send fresh.
                    if q.len() > 2 {
                        q.clear();
                    }
                    q.push_back(frame);
                }
            }
        }
        let _ = buffer.Unlock();
    }
    Ok(())
}
