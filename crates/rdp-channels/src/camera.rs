//! Camera (webcam) redirection (MS-RDPECAM) — present a local camera to the
//! remote session. Runs over the dynamic virtual channel
//! `RDCamera_Device_Enumerator`, plus one per-device channel the server opens
//! after the client announces a device.
//!
//! Flow on the enumerator channel: the server sends **SelectVersionRequest** and
//! the client replies **SelectVersionResponse** with the agreed version; the
//! client then sends a **DeviceAddedNotification** for each local camera (device
//! name + the per-device channel name to open). On the per-device channel the
//! server requests the stream list, media types, picks a current media type, and
//! issues **StartStreams**; the client then pushes **SampleResponse** PDUs
//! carrying frames until **StopStreams**.
//!
//! Every PDU starts with a 2-byte header { Version, MessageId }. This module is
//! the sans-I/O codec + enumerator state machine; device discovery and frame
//! capture come from a [`CameraSource`] the platform supplies (Media Foundation
//! on Windows). If the client announces no cameras, the server simply gets none —
//! the feature is entirely additive.

/// `CAM_MSG_ID` message identifiers (second header byte).
pub mod msg {
    pub const SUCCESS_RESPONSE: u8 = 0x01;
    pub const ERROR_RESPONSE: u8 = 0x02;
    pub const SELECT_VERSION_REQUEST: u8 = 0x03;
    pub const SELECT_VERSION_RESPONSE: u8 = 0x04;
    pub const DEVICE_ADDED: u8 = 0x05;
    pub const DEVICE_REMOVED: u8 = 0x06;
    pub const ACTIVATE_DEVICE_REQUEST: u8 = 0x07;
    pub const DEACTIVATE_DEVICE_REQUEST: u8 = 0x08;
    pub const STREAM_LIST_REQUEST: u8 = 0x09;
    pub const STREAM_LIST_RESPONSE: u8 = 0x0A;
    pub const MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
    pub const MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
    pub const CURRENT_MEDIA_TYPE_REQUEST: u8 = 0x0D;
    pub const CURRENT_MEDIA_TYPE_RESPONSE: u8 = 0x0E;
    pub const START_STREAMS_REQUEST: u8 = 0x0F;
    pub const STOP_STREAMS_REQUEST: u8 = 0x10;
    pub const SAMPLE_REQUEST: u8 = 0x11;
    pub const SAMPLE_RESPONSE: u8 = 0x12;
    pub const SAMPLE_ERROR_RESPONSE: u8 = 0x13;
}

/// The protocol version the client speaks.
const CAM_VERSION: u8 = 0x01;

/// A local camera the client offers to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// Human-readable device name (e.g. "Integrated Webcam").
    pub name: String,
    /// The dynamic-channel name the server should open for this device. Must be
    /// unique per device; the client picks it (commonly the device id).
    pub channel_name: String,
}

/// A camera pixel/stream format the client can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CamFormat {
    /// NV12 (4:2:0) raw — the common uncompressed webcam format.
    Nv12,
    /// YUY2 (4:2:2) raw.
    Yuy2,
    /// Motion JPEG.
    Mjpg,
    /// H.264 (some webcams encode on-device).
    H264,
}

impl CamFormat {
    /// The 4-byte FourCC the wire uses to identify the format.
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            CamFormat::Nv12 => *b"NV12",
            CamFormat::Yuy2 => *b"YUY2",
            CamFormat::Mjpg => *b"MJPG",
            CamFormat::H264 => *b"H264",
        }
    }
}

/// One capture media type a camera stream can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaType {
    pub format: CamFormat,
    pub width: u32,
    pub height: u32,
    /// Frame rate numerator / denominator (e.g. 30/1).
    pub fps_num: u32,
    pub fps_den: u32,
}

impl MediaType {
    /// Serialize as a `CAM_MEDIA_TYPE_DESCRIPTION`-style record: FourCC(4),
    /// Width(4), Height(4), FrameRateNum(4), FrameRateDen(4) — all little-endian.
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.format.fourcc());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.fps_num.to_le_bytes());
        out.extend_from_slice(&self.fps_den.to_le_bytes());
    }
}

/// Build a PDU: 2-byte header { Version, MessageId } + body.
fn message(msg_id: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + body.len());
    v.push(CAM_VERSION);
    v.push(msg_id);
    v.extend_from_slice(body);
    v
}

/// Encode a UTF-16LE, NUL-terminated string (the wire form for names).
fn utf16z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    v.extend_from_slice(&[0, 0]); // NUL terminator
    v
}

/// The MessageId of a received PDU (second header byte), or `None` if too short.
pub fn message_id(pdu: &[u8]) -> Option<u8> {
    (pdu.len() >= 2).then(|| pdu[1])
}

/// The camera enumerator-channel state machine. Answers version negotiation and
/// announces the local cameras; per-device stream/media negotiation is handled
/// on the device channels (see [`device_added`]).
pub struct CameraEnumerator {
    devices: Vec<CameraDevice>,
    announced: bool,
}

impl CameraEnumerator {
    /// Create an enumerator that will advertise `devices` (empty = no cameras,
    /// so nothing is announced and redirection stays off).
    pub fn new(devices: Vec<CameraDevice>) -> Self {
        Self {
            devices,
            announced: false,
        }
    }

    /// Whether any cameras are available to redirect.
    pub fn has_cameras(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Process one enumerator-channel PDU, returning the PDUs to send back.
    pub fn process(&mut self, pdu: &[u8]) -> Vec<Vec<u8>> {
        let Some(id) = message_id(pdu) else {
            return Vec::new();
        };
        match id {
            msg::SELECT_VERSION_REQUEST => {
                // Agree on our version, then announce every local camera.
                let mut out = vec![message(msg::SELECT_VERSION_RESPONSE, &[CAM_VERSION])];
                if !self.announced {
                    for dev in &self.devices {
                        tracing::info!(
                            device = %dev.name,
                            channel = %dev.channel_name,
                            "announcing local camera to the session"
                        );
                        out.push(device_added(dev));
                    }
                    if self.devices.is_empty() {
                        tracing::info!(
                            "camera redirection: no local cameras found; none announced"
                        );
                    }
                    self.announced = true;
                }
                out
            }
            other => {
                tracing::debug!(message_id = other, "unhandled camera enumerator message");
                Vec::new()
            }
        }
    }
}

/// Build a DeviceAddedNotification announcing one camera (its display name and
/// the per-device channel name the server should open).
pub fn device_added(dev: &CameraDevice) -> Vec<u8> {
    let mut body = Vec::new();
    // DeviceName (UTF-16LE, NUL-terminated) then VirtualChannelName (the channel
    // the server opens for this device's streams).
    body.extend_from_slice(&utf16z(&dev.name));
    body.extend_from_slice(&utf16z(&dev.channel_name));
    message(msg::DEVICE_ADDED, &body)
}

/// Wrap captured camera-frame `bytes` as a SampleResponse PDU for a stream.
pub fn sample_response(stream_index: u8, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + bytes.len());
    body.push(stream_index);
    body.extend_from_slice(bytes);
    message(msg::SAMPLE_RESPONSE, &body)
}

/// The per-device camera channel state machine (the channel the server opens for
/// one announced camera). It advertises a single stream and its media types,
/// honors the server's media-type selection, and tracks whether streaming is on
/// so the session knows when to push captured frames as SampleResponse PDUs.
pub struct CameraDeviceChannel {
    media_types: Vec<MediaType>,
    /// The media type the server selected via StartStreams, once streaming.
    streaming: Option<MediaType>,
}

impl CameraDeviceChannel {
    /// Create a device channel offering `media_types` (the camera's formats).
    pub fn new(media_types: Vec<MediaType>) -> Self {
        Self {
            media_types,
            streaming: None,
        }
    }

    /// Whether the server has started the stream (frames should be captured).
    pub fn streaming(&self) -> Option<MediaType> {
        self.streaming
    }

    /// Process one per-device-channel PDU, returning the PDUs to send back.
    pub fn process(&mut self, pdu: &[u8]) -> Vec<Vec<u8>> {
        let Some(id) = message_id(pdu) else {
            return Vec::new();
        };
        match id {
            msg::STREAM_LIST_REQUEST => {
                // Advertise a single video stream (index 0).
                vec![message(msg::STREAM_LIST_RESPONSE, &[1u8 /* stream count */, 0])]
            }
            msg::MEDIA_TYPE_LIST_REQUEST => {
                let mut body = Vec::new();
                body.push(0); // stream index
                body.push(self.media_types.len() as u8);
                for mt in &self.media_types {
                    mt.write(&mut body);
                }
                vec![message(msg::MEDIA_TYPE_LIST_RESPONSE, &body)]
            }
            msg::CURRENT_MEDIA_TYPE_REQUEST => {
                // Report the first media type as current (until StartStreams).
                let mut body = vec![0]; // stream index
                if let Some(mt) = self.media_types.first() {
                    mt.write(&mut body);
                }
                vec![message(msg::CURRENT_MEDIA_TYPE_RESPONSE, &body)]
            }
            msg::START_STREAMS_REQUEST => {
                // The server selects a media type by index (after the stream id).
                let idx = pdu.get(3).copied().unwrap_or(0) as usize;
                self.streaming = self
                    .media_types
                    .get(idx)
                    .or_else(|| self.media_types.first())
                    .copied();
                vec![message(msg::SUCCESS_RESPONSE, &[])]
            }
            msg::STOP_STREAMS_REQUEST => {
                self.streaming = None;
                vec![message(msg::SUCCESS_RESPONSE, &[])]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_camera() -> Vec<CameraDevice> {
        vec![CameraDevice {
            name: "Test Cam".into(),
            channel_name: "rdpio_cam0".into(),
        }]
    }

    #[test]
    fn no_cameras_by_default() {
        let e = CameraEnumerator::new(Vec::new());
        assert!(!e.has_cameras());
    }

    #[test]
    fn select_version_is_answered_and_devices_announced() {
        let mut e = CameraEnumerator::new(one_camera());
        assert!(e.has_cameras());
        let req = message(msg::SELECT_VERSION_REQUEST, &[CAM_VERSION]);
        let out = e.process(&req);
        // SelectVersionResponse + one DeviceAdded.
        assert_eq!(out.len(), 2);
        assert_eq!(message_id(&out[0]), Some(msg::SELECT_VERSION_RESPONSE));
        assert_eq!(out[0][2], CAM_VERSION);
        assert_eq!(message_id(&out[1]), Some(msg::DEVICE_ADDED));
        // The announcement carries the UTF-16 device name "Test Cam".
        assert_eq!(&out[1][2..4], b"T\0");
    }

    #[test]
    fn devices_announced_once() {
        let mut e = CameraEnumerator::new(one_camera());
        let req = message(msg::SELECT_VERSION_REQUEST, &[CAM_VERSION]);
        let _ = e.process(&req);
        // A second version request only re-sends the response, not the device.
        let out = e.process(&req);
        assert_eq!(out.len(), 1);
        assert_eq!(message_id(&out[0]), Some(msg::SELECT_VERSION_RESPONSE));
    }

    #[test]
    fn sample_response_carries_stream_and_bytes() {
        let pdu = sample_response(2, &[0xAA, 0xBB]);
        assert_eq!(message_id(&pdu), Some(msg::SAMPLE_RESPONSE));
        assert_eq!(pdu[2], 2); // stream index
        assert_eq!(&pdu[3..], &[0xAA, 0xBB]);
    }

    fn nv12(w: u32, h: u32) -> MediaType {
        MediaType {
            format: CamFormat::Nv12,
            width: w,
            height: h,
            fps_num: 30,
            fps_den: 1,
        }
    }

    #[test]
    fn device_channel_negotiates_and_starts() {
        let mut dev = CameraDeviceChannel::new(vec![nv12(1280, 720), nv12(640, 480)]);
        assert!(dev.streaming().is_none());

        // Stream list → one stream.
        let out = dev.process(&message(msg::STREAM_LIST_REQUEST, &[]));
        assert_eq!(message_id(&out[0]), Some(msg::STREAM_LIST_RESPONSE));

        // Media type list → both formats, FourCC "NV12".
        let out = dev.process(&message(msg::MEDIA_TYPE_LIST_REQUEST, &[0]));
        assert_eq!(message_id(&out[0]), Some(msg::MEDIA_TYPE_LIST_RESPONSE));
        assert_eq!(out[0][2], 0); // stream index
        assert_eq!(out[0][3], 2); // two media types
        assert_eq!(&out[0][4..8], b"NV12");

        // StartStreams selecting index 1 (640x480) → SuccessResponse + streaming.
        let out = dev.process(&message(msg::START_STREAMS_REQUEST, &[0 /*stream*/, 1 /*mt idx*/]));
        assert_eq!(message_id(&out[0]), Some(msg::SUCCESS_RESPONSE));
        assert_eq!(dev.streaming(), Some(nv12(640, 480)));

        // Stop → no longer streaming.
        dev.process(&message(msg::STOP_STREAMS_REQUEST, &[]));
        assert!(dev.streaming().is_none());
    }
}
