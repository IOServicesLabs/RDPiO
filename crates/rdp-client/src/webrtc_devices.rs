//! The client's real media devices, reported to Teams via
//! `MediaDevices.enumerateDevices` on the webrtc.1 channel.
//!
//! Teams gates optimization on this list. Before it will move a call's media onto
//! the endpoint it asks what devices the endpoint has; an empty answer means "no
//! microphone, no speaker, no camera", so Teams cannot run the call here and
//! silently falls back to the in-session pipeline — the call still connects, it
//! just isn't optimized. (We watched exactly that happen: with a stubbed empty
//! list, Teams did the capability handshake, called `enumerateDevices`, and then
//! never created a peer connection.)
//!
//! Sources: `waveOut`/`waveIn` for audio endpoints (the same legacy-but-simple APIs
//! [`crate::audio`] and [`crate::mic`] already play/capture through, so whatever we
//! advertise is something we can actually drive), and Media Foundation for cameras
//! (via [`crate::mf_camera::MfCamera::enumerate`]).
//!
//! `deviceId` is opaque to Teams — it hands the same string back as the `sourceId`
//! constraint when asking us to capture from a device — so we mint stable ids of
//! our own (`rdpio-{kind}-{index}`) that map straight back to a wave/MF device index.

use std::sync::Mutex;

use rdp_channels::camera::{CamFormat, MediaType};
use rdp_webrtc::{DeviceKind, DeviceProvider, MediaDevice, VideoCaptureSource, NO_GROUP};

use windows::Win32::Media::Audio::{
    waveInGetDevCapsW, waveInGetNumDevs, waveOutGetDevCapsW, waveOutGetNumDevs, WAVEINCAPSW,
    WAVEOUTCAPSW,
};

use crate::mf_camera::MfCamera;

/// `MMSYSERR_NOERROR`.
const MM_OK: u32 = 0;

/// Decode a `szPname` (fixed-size, NUL-padded UTF-16 name field).
fn wide_name(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

/// Enumerate `waveOut` render endpoints (speakers/headphones).
fn audio_outputs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Safe: read-only capability queries into a stack struct we own.
    unsafe {
        let n = waveOutGetNumDevs();
        for i in 0..n {
            let mut caps = WAVEOUTCAPSW::default();
            if waveOutGetDevCapsW(i as usize, &mut caps, size_of::<WAVEOUTCAPSW>() as u32) == MM_OK
            {
                // `WAVEOUTCAPSW` is `#[repr(packed)]`, so copy the name field out by
                // value — a reference into it would be unaligned.
                let name_buf = caps.szPname;
                let name = wide_name(&name_buf);
                if !name.is_empty() {
                    out.push((format!("rdpio-audiooutput-{i}"), name));
                }
            }
        }
    }
    out
}

/// Enumerate `waveIn` capture endpoints (microphones).
fn audio_inputs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let n = waveInGetNumDevs();
        for i in 0..n {
            let mut caps = WAVEINCAPSW::default();
            if waveInGetDevCapsW(i as usize, &mut caps, size_of::<WAVEINCAPSW>() as u32) == MM_OK {
                // Packed struct — copy the name field out rather than borrowing it.
                let name_buf = caps.szPname;
                let name = wide_name(&name_buf);
                if !name.is_empty() {
                    out.push((format!("rdpio-audioinput-{i}"), name));
                }
            }
        }
    }
    out
}

/// Reports this machine's cameras, microphones and speakers to the redirector.
pub struct WinDeviceProvider;

impl DeviceProvider for WinDeviceProvider {
    fn enumerate(&self) -> Vec<MediaDevice> {
        let mut devices = Vec::new();

        let outputs = audio_outputs();
        let inputs = audio_inputs();

        // Teams expects the W3C-style `default` / `communications` pseudo-devices
        // alongside the real endpoints (the real add-in reports both). We map them
        // onto the first endpoint of each kind, which is what `WAVE_MAPPER` — the
        // device `audio.rs` / `mic.rs` actually open — resolves to.
        if let Some((_, label)) = outputs.first() {
            devices.push(MediaDevice::new(
                DeviceKind::AudioOutput,
                "default",
                format!("Default - {label}"),
                NO_GROUP,
            ));
            devices.push(MediaDevice::new(
                DeviceKind::AudioOutput,
                "communications",
                format!("Communications - {label}"),
                NO_GROUP,
            ));
        }
        if let Some((_, label)) = inputs.first() {
            devices.push(MediaDevice::new(
                DeviceKind::AudioInput,
                "default",
                format!("Default - {label}"),
                NO_GROUP,
            ));
            devices.push(MediaDevice::new(
                DeviceKind::AudioInput,
                "communications",
                format!("Communications - {label}"),
                NO_GROUP,
            ));
        }

        for (id, label) in outputs {
            devices.push(MediaDevice::new(DeviceKind::AudioOutput, id, label, NO_GROUP));
        }
        for (id, label) in inputs {
            devices.push(MediaDevice::new(DeviceKind::AudioInput, id, label, NO_GROUP));
        }
        for (i, cam) in MfCamera::enumerate().into_iter().enumerate() {
            devices.push(MediaDevice::new(
                DeviceKind::VideoInput,
                format!("rdpio-videoinput-{i}"),
                cam.name,
                NO_GROUP,
            ));
        }

        let (mics, cams) = (
            devices.iter().filter(|d| d.kind == DeviceKind::AudioInput).count(),
            devices.iter().filter(|d| d.kind == DeviceKind::VideoInput).count(),
        );
        if mics == 0 && cams == 0 {
            tracing::warn!(
                "no microphone or camera found on this client — Teams will not optimize calls"
            );
        } else {
            tracing::info!(
                total = devices.len(),
                microphones = mics,
                cameras = cams,
                "enumerated client media devices for Teams"
            );
        }
        devices
    }
}

/// Outbound camera for the native Teams path: wraps [`MfCamera`] (Media Foundation
/// capture + `rdp_gpu::h264` encode) as the engine's [`VideoCaptureSource`].
///
/// Teams attaches the camera to a video sender via `replaceTrack`; the engine then
/// pulls Annex-B H.264 access units from here and writes them to the send track, so
/// the offer carries real outbound video and Teams' media server accepts the video
/// m-line (and the SCTP data channel bundled with it) instead of rejecting the whole
/// media session. Capture only runs while a call is holding a track (`start`/`stop`).
#[derive(Default)]
pub struct CameraVideoSource {
    cam: Mutex<Option<MfCamera>>,
}

impl VideoCaptureSource for CameraVideoSource {
    fn start(&self, source_id: &str) -> bool {
        // Teams normally hands back one of our `rdpio-videoinput-{index}` deviceIds as
        // the sourceId; fall back to the first camera when it passes something else
        // (e.g. a track id, until the MediaStream→sourceId mapping is threaded through).
        let index = source_id
            .strip_prefix("rdpio-videoinput-")
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        // 720p30 H.264: MfCamera captures NV12 and encodes to Annex-B on its worker.
        let media = MediaType {
            format: CamFormat::H264,
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
        };
        match self.cam.lock() {
            Ok(mut slot) => {
                *slot = Some(MfCamera::start(index, media));
                tracing::info!(index, "native camera capture started for Teams send");
                true
            }
            Err(_) => false,
        }
    }

    fn poll_frame(&self) -> Option<Vec<u8>> {
        self.cam.lock().ok()?.as_ref()?.poll_frame()
    }

    fn stop(&self) {
        if let Ok(mut slot) = self.cam.lock() {
            slot.take(); // dropping MfCamera signals + joins the capture worker
        }
    }
}
