//! Camera (webcam) redirection (MS-RDPECAM) — present a local camera to the
//! remote session. Runs over the dynamic virtual channel
//! `RDCamera_Device_Enumerator`, plus one per-device channel the server opens
//! after the client announces a device.
//!
//! Flow on the enumerator channel: once the server opens it, the **client**
//! sends **SelectVersionRequest** (the version it speaks, in the header); the
//! server answers **SelectVersionResponse**, and the client then sends a
//! **DeviceAddedNotification** for each local camera (UTF-16 device name + the
//! ANSI name of the per-device channel for the server to open). On the
//! per-device channel the server activates the device, requests the stream list
//! and media types, may query the current media type, and issues
//! **StartStreams** naming the media type it picked. It then pulls frames: each
//! **SampleRequest** is answered by one **SampleResponse**, until
//! **StopStreams**.
//!
//! Every PDU starts with a 2-byte header { Version, MessageId }. We speak
//! version 1, which has no property messages. This module is
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
    /// The `CAM_MEDIA_FORMAT` value the wire uses to identify the format.
    pub fn wire(self) -> u8 {
        match self {
            CamFormat::H264 => 0x01,
            CamFormat::Mjpg => 0x02,
            CamFormat::Yuy2 => 0x03,
            CamFormat::Nv12 => 0x04,
        }
    }

    fn from_wire(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(CamFormat::H264),
            0x02 => Some(CamFormat::Mjpg),
            0x03 => Some(CamFormat::Yuy2),
            0x04 => Some(CamFormat::Nv12),
            _ => None,
        }
    }

    /// Compressed formats the server must decode before handing frames on.
    fn is_compressed(self) -> bool {
        matches!(self, CamFormat::H264 | CamFormat::Mjpg)
    }
}

/// `CAM_MEDIA_TYPE_DESCRIPTION.Flags`: the sample must be decoded.
const MEDIA_FLAG_DECODING_REQUIRED: u8 = 0x01;
/// Wire size of a `CAM_MEDIA_TYPE_DESCRIPTION`.
const MEDIA_TYPE_LEN: usize = 26;
/// `CAM_STREAM_DESCRIPTION.FrameSourceTypes`: a colour camera.
const FRAME_SOURCE_COLOR: u16 = 0x0001;
/// `CAM_STREAM_DESCRIPTION.StreamCategory`: a capture stream.
const STREAM_CATEGORY_CAPTURE: u8 = 0x01;

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
    /// Serialize as a 26-byte `CAM_MEDIA_TYPE_DESCRIPTION`: Format(1), Width(4),
    /// Height(4), FrameRateNumerator(4), FrameRateDenominator(4),
    /// PixelAspectRatioNumerator(4), PixelAspectRatioDenominator(4), Flags(1) —
    /// little-endian, square pixels.
    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.format.wire());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.fps_num.to_le_bytes());
        out.extend_from_slice(&self.fps_den.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.push(if self.format.is_compressed() {
            MEDIA_FLAG_DECODING_REQUIRED
        } else {
            0
        });
    }

    /// Parse a `CAM_MEDIA_TYPE_DESCRIPTION` (see [`MediaType::write`]).
    fn parse(b: &[u8]) -> Option<Self> {
        let b = b.get(..MEDIA_TYPE_LEN)?;
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(MediaType {
            format: CamFormat::from_wire(b[0])?,
            width: u32_at(1),
            height: u32_at(5),
            fps_num: u32_at(9),
            fps_den: u32_at(13),
        })
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

/// Encode a UTF-16LE, NUL-terminated string (the wire form for device names).
fn utf16z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    v.extend_from_slice(&[0, 0]); // NUL terminator
    v
}

/// Encode an ANSI, NUL-terminated string (the wire form for channel names —
/// DVC names are 8-bit). Non-ASCII characters are replaced with `_`.
fn ansiz(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s
        .chars()
        .map(|c| if c.is_ascii() && c != '\0' { c as u8 } else { b'_' })
        .collect();
    v.push(0);
    v
}

/// The MessageId of a received PDU (second header byte), or `None` if too short.
pub fn message_id(pdu: &[u8]) -> Option<u8> {
    (pdu.len() >= 2).then(|| pdu[1])
}

/// The camera enumerator-channel state machine. Opens version negotiation and
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

    /// The PDUs to send once the server has opened the enumerator channel: the
    /// client's SelectVersionRequest. The server does not speak first — a
    /// client that waits for it leaves the channel idle forever.
    pub fn start(&mut self) -> Vec<Vec<u8>> {
        self.announced = false;
        vec![message(msg::SELECT_VERSION_REQUEST, &[])]
    }

    /// Process one enumerator-channel PDU, returning the PDUs to send back.
    pub fn process(&mut self, pdu: &[u8]) -> Vec<Vec<u8>> {
        let Some(id) = message_id(pdu) else {
            return Vec::new();
        };
        match id {
            msg::SELECT_VERSION_RESPONSE => {
                // Version agreed: announce every local camera (once).
                tracing::info!(version = pdu[0], "camera redirection version agreed");
                let mut out = Vec::new();
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
            msg::ERROR_RESPONSE => {
                tracing::warn!(pdu = ?pdu, "server rejected camera redirection");
                Vec::new()
            }
            other => {
                tracing::debug!(message_id = other, "unhandled camera enumerator message");
                Vec::new()
            }
        }
    }
}

/// Build a DeviceAddedNotification announcing one camera: DeviceName (UTF-16LE,
/// NUL-terminated) then VirtualChannelName (ANSI, NUL-terminated — the channel
/// the server opens for this device's streams).
pub fn device_added(dev: &CameraDevice) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&utf16z(&dev.name));
    body.extend_from_slice(&ansiz(&dev.channel_name));
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
/// honors the server's media-type selection, and counts the server's sample
/// requests so the session sends exactly one SampleResponse per request.
pub struct CameraDeviceChannel {
    media_types: Vec<MediaType>,
    /// The media type the server selected via StartStreams, once streaming.
    streaming: Option<MediaType>,
    /// SampleRequests received and not yet collected by [`Self::take_new_requests`].
    new_requests: u32,
}

impl CameraDeviceChannel {
    /// Create a device channel offering `media_types` (the camera's formats).
    pub fn new(media_types: Vec<MediaType>) -> Self {
        Self {
            media_types,
            streaming: None,
            new_requests: 0,
        }
    }

    /// Whether the server has started the stream (frames should be captured).
    pub fn streaming(&self) -> Option<MediaType> {
        self.streaming
    }

    /// SampleRequests that arrived since the last call; the caller owes one
    /// SampleResponse (one captured frame) for each.
    pub fn take_new_requests(&mut self) -> u32 {
        std::mem::take(&mut self.new_requests)
    }

    /// The media type the server named in StartStreams, snapped to the matching
    /// entry we advertised (format + size); falls back to the parsed type when
    /// it is one we can produce, else to our first type.
    fn select(&self, requested: Option<MediaType>) -> Option<MediaType> {
        let Some(req) = requested else {
            return self.media_types.first().copied();
        };
        self.media_types
            .iter()
            .find(|m| m.format == req.format && m.width == req.width && m.height == req.height)
            .copied()
            .or_else(|| {
                matches!(req.format, CamFormat::H264 | CamFormat::Nv12).then_some(req)
            })
            .or_else(|| self.media_types.first().copied())
    }

    /// Process one per-device-channel PDU, returning the PDUs to send back.
    pub fn process(&mut self, pdu: &[u8]) -> Vec<Vec<u8>> {
        let Some(id) = message_id(pdu) else {
            return Vec::new();
        };
        let success = || vec![message(msg::SUCCESS_RESPONSE, &[])];
        match id {
            msg::ACTIVATE_DEVICE_REQUEST => success(),
            msg::DEACTIVATE_DEVICE_REQUEST => {
                self.streaming = None;
                self.new_requests = 0;
                success()
            }
            msg::STREAM_LIST_REQUEST => {
                // One CAM_STREAM_DESCRIPTION: FrameSourceTypes(2),
                // StreamCategory(1), Selected(1), CanBeShared(1).
                let mut body = FRAME_SOURCE_COLOR.to_le_bytes().to_vec();
                body.extend_from_slice(&[STREAM_CATEGORY_CAPTURE, 1, 0]);
                vec![message(msg::STREAM_LIST_RESPONSE, &body)]
            }
            msg::MEDIA_TYPE_LIST_REQUEST => {
                // The descriptions back to back; the count is implied by length.
                let mut body = Vec::with_capacity(self.media_types.len() * MEDIA_TYPE_LEN);
                for mt in &self.media_types {
                    mt.write(&mut body);
                }
                vec![message(msg::MEDIA_TYPE_LIST_RESPONSE, &body)]
            }
            msg::CURRENT_MEDIA_TYPE_REQUEST => {
                let mut body = Vec::with_capacity(MEDIA_TYPE_LEN);
                if let Some(mt) = self.streaming.or_else(|| self.media_types.first().copied()) {
                    mt.write(&mut body);
                }
                vec![message(msg::CURRENT_MEDIA_TYPE_RESPONSE, &body)]
            }
            msg::START_STREAMS_REQUEST => {
                // CAM_START_STREAM_INFO entries: StreamIndex(1) + media type.
                // We expose one stream, so the first entry is ours.
                let requested = pdu.get(3..).and_then(MediaType::parse);
                self.streaming = self.select(requested);
                self.new_requests = 0;
                tracing::info!(requested = ?requested, selected = ?self.streaming, "camera stream start requested");
                success()
            }
            msg::STOP_STREAMS_REQUEST => {
                self.streaming = None;
                self.new_requests = 0;
                success()
            }
            msg::SAMPLE_REQUEST => {
                if self.streaming.is_some() {
                    self.new_requests += 1;
                }
                Vec::new()
            }
            other => {
                tracing::debug!(message_id = other, "unhandled camera device message");
                Vec::new()
            }
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
    fn client_opens_version_negotiation_then_announces_devices() {
        let mut e = CameraEnumerator::new(one_camera());
        assert!(e.has_cameras());
        // The client speaks first: a bare SelectVersionRequest header.
        assert_eq!(e.start(), vec![vec![CAM_VERSION, msg::SELECT_VERSION_REQUEST]]);
        // The server's response triggers the device announcement.
        let out = e.process(&message(msg::SELECT_VERSION_RESPONSE, &[]));
        assert_eq!(out.len(), 1);
        assert_eq!(message_id(&out[0]), Some(msg::DEVICE_ADDED));
        // UTF-16 device name "Test Cam" + NUL, then the ANSI channel name + NUL.
        let mut body = utf16z("Test Cam");
        body.extend_from_slice(b"rdpio_cam0\0");
        assert_eq!(&out[0][2..], &body[..]);
    }

    #[test]
    fn devices_announced_once() {
        let mut e = CameraEnumerator::new(one_camera());
        e.start();
        let resp = message(msg::SELECT_VERSION_RESPONSE, &[]);
        assert_eq!(e.process(&resp).len(), 1);
        assert!(e.process(&resp).is_empty());
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

    fn h264(w: u32, h: u32) -> MediaType {
        MediaType {
            format: CamFormat::H264,
            ..nv12(w, h)
        }
    }

    fn start_streams(mt: MediaType) -> Vec<u8> {
        let mut body = vec![0]; // StreamIndex
        mt.write(&mut body);
        message(msg::START_STREAMS_REQUEST, &body)
    }

    #[test]
    fn media_type_description_layout() {
        let mut b = Vec::new();
        h264(1280, 720).write(&mut b);
        assert_eq!(b.len(), MEDIA_TYPE_LEN);
        assert_eq!(b[0], 0x01); // CAM_MEDIA_FORMAT_H264
        assert_eq!(&b[1..5], &1280u32.to_le_bytes());
        assert_eq!(&b[5..9], &720u32.to_le_bytes());
        assert_eq!(&b[9..13], &30u32.to_le_bytes());
        assert_eq!(&b[13..17], &1u32.to_le_bytes());
        assert_eq!(&b[17..25], &[1, 0, 0, 0, 1, 0, 0, 0]); // square pixels
        assert_eq!(b[25], MEDIA_FLAG_DECODING_REQUIRED);
        assert_eq!(MediaType::parse(&b), Some(h264(1280, 720)));

        let mut raw = Vec::new();
        nv12(640, 480).write(&mut raw);
        assert_eq!((raw[0], raw[25]), (0x04, 0)); // NV12, no decode needed
    }

    #[test]
    fn device_channel_negotiates_and_starts() {
        let mut dev = CameraDeviceChannel::new(vec![h264(1280, 720), nv12(640, 480)]);
        assert!(dev.streaming().is_none());

        // Activate → Success.
        let out = dev.process(&message(msg::ACTIVATE_DEVICE_REQUEST, &[]));
        assert_eq!(out, vec![message(msg::SUCCESS_RESPONSE, &[])]);

        // Stream list → one 5-byte colour capture stream, selected.
        let out = dev.process(&message(msg::STREAM_LIST_REQUEST, &[]));
        assert_eq!(out, vec![message(msg::STREAM_LIST_RESPONSE, &[0x01, 0x00, 0x01, 1, 0])]);

        // Media type list → both descriptions back to back, no count/index.
        let out = dev.process(&message(msg::MEDIA_TYPE_LIST_REQUEST, &[0]));
        assert_eq!(message_id(&out[0]), Some(msg::MEDIA_TYPE_LIST_RESPONSE));
        assert_eq!(out[0].len(), 2 + 2 * MEDIA_TYPE_LEN);
        assert_eq!(MediaType::parse(&out[0][2..]), Some(h264(1280, 720)));
        assert_eq!(MediaType::parse(&out[0][2 + MEDIA_TYPE_LEN..]), Some(nv12(640, 480)));

        // StartStreams naming 640x480 NV12 → Success + streaming that type.
        let out = dev.process(&start_streams(nv12(640, 480)));
        assert_eq!(out, vec![message(msg::SUCCESS_RESPONSE, &[])]);
        assert_eq!(dev.streaming(), Some(nv12(640, 480)));

        // Current media type now reports the streaming type.
        let out = dev.process(&message(msg::CURRENT_MEDIA_TYPE_REQUEST, &[0]));
        assert_eq!(MediaType::parse(&out[0][2..]), Some(nv12(640, 480)));

        // Stop → no longer streaming.
        dev.process(&message(msg::STOP_STREAMS_REQUEST, &[]));
        assert!(dev.streaming().is_none());
    }

    #[test]
    fn sample_requests_are_counted_only_while_streaming() {
        let mut dev = CameraDeviceChannel::new(vec![h264(640, 480)]);
        let req = message(msg::SAMPLE_REQUEST, &[0]);
        assert!(dev.process(&req).is_empty());
        assert_eq!(dev.take_new_requests(), 0); // not streaming yet

        dev.process(&start_streams(h264(640, 480)));
        dev.process(&req);
        dev.process(&req);
        assert_eq!(dev.take_new_requests(), 2);
        assert_eq!(dev.take_new_requests(), 0);

        dev.process(&req);
        dev.process(&message(msg::STOP_STREAMS_REQUEST, &[]));
        assert_eq!(dev.take_new_requests(), 0); // stop discards the backlog
    }
}
