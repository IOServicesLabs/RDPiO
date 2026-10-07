//! What the dispatcher and the host (`rdp-client`) see of a WebRTC engine,
//! whichever backend implements it: the media plug-in traits and the events a
//! peer connection reports. Shared by the webrtc-rs engine (`engine` feature)
//! and the libwebrtc engine (`libwebrtc` feature).

use std::sync::{Arc, Mutex};

/// Receives media from the peer connection's inbound tracks. A real client
/// decodes each track and composites it into the session at the geometry from
/// the presentation model; tests just count what arrives. Delivered from the
/// engine's read loops, so implementors must be cheap and thread-safe.
pub trait MediaSink: Send + Sync {
    /// A remote track began delivering media (`kind` = "audio"/"video",
    /// `codec` = the RTP mime type of its first packet, e.g. "video/VP8", or
    /// "audio/L16" for already-decoded PCM).
    fn on_track(&self, track_id: &str, kind: &str, codec: &str);
    /// One RTP packet payload arrived on `track_id`, carrying `codec` (a track
    /// can switch payload type mid-stream — Opus speech and CN comfort noise
    /// share the audio track). Engines that decode themselves don't call this.
    fn on_rtp(&self, track_id: &str, codec: &str, payload: &[u8]);
    /// Decoded remote audio (interleaved 16-bit PCM), from engines that decode
    /// themselves (libwebrtc). Roughly 10 ms per call.
    fn on_pcm(&self, _track_id: &str, _sample_rate: u32, _channels: u32, _samples: &[i16]) {}
    /// Teams' `rpcObjectId` for the remote stream `stream_id` (its msid, e.g.
    /// "mainVideo-1101") — what a `<video>` element's `srcObject` will name.
    fn on_remote_stream(&self, _rpc_id: u64, _stream_id: &str) {}
    /// A decoded frame of the remote video stream `stream_id` (engines that
    /// decode themselves: libwebrtc).
    fn on_video_frame(&self, _stream_id: &str, _frame: I420Frame) {}
}

/// A tightly packed I420 picture: `y` is `width × height`, `u`/`v` are
/// `((width+1)/2) × ((height+1)/2)`.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    /// The same picture as NV12 (one Y plane, then interleaved U/V).
    pub fn to_nv12(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.y.len() + self.u.len() * 2);
        out.extend_from_slice(&self.y);
        for (u, v) in self.u.iter().zip(&self.v) {
            out.push(*u);
            out.push(*v);
        }
        out
    }
}

/// Supplies encoded outbound video for a send track: **Annex-B H.264 access units**,
/// pulled one per frame interval. The client implements this over its real camera
/// (Media Foundation capture + H.264 encode); tests use a synthetic source.
///
/// Teams attaches the camera to a video sender via `replaceTrack` *before* it calls
/// `createOffer` (verified in the capture).
pub trait VideoCaptureSource: Send + Sync {
    /// Start capturing the device Teams identified by `source_id` (one of our
    /// enumerated `deviceId`s, e.g. `rdpio-videoinput-0`). Returns false if it can't
    /// start (the track then simply sends nothing until a later attach).
    fn start(&self, source_id: &str) -> bool;
    /// The next Annex-B H.264 access unit, if one is ready (non-blocking).
    fn poll_frame(&self) -> Option<Vec<u8>>;
    /// The next raw camera picture, for engines that encode themselves
    /// (libwebrtc). `None` when no new frame is ready.
    fn poll_nv12(&self) -> Option<Nv12Frame> {
        None
    }
    /// Stop capturing (the sender's track was cleared, or the peer connection closed).
    fn stop(&self);
}

/// A raw NV12 picture (`width × height` Y, then interleaved U/V at half size).
#[derive(Debug, Clone)]
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub data: std::sync::Arc<Vec<u8>>,
}

/// One simulcast layer Teams asked for in `addTransceiver`'s `sendEncodings`.
#[derive(Debug, Clone, PartialEq)]
pub struct SendEncoding {
    pub rid: String,
    pub scale_resolution_down_by: f64,
}

/// Supplies outbound mic audio. The webrtc-rs engine takes **20 ms Opus
/// packets** ([`Self::poll_frame`]); the libwebrtc engine encodes itself and
/// takes **10 ms of 48 kHz mono PCM** ([`Self::poll_pcm`]).
///
/// Teams attaches the mic to the audio sender via `replaceTrack` before
/// `createOffer` (verified in the capture), exactly like the camera.
pub trait AudioCaptureSource: Send + Sync {
    /// Start capturing the device Teams identified by `source_id`. Returns false if
    /// it can't start (the track then simply sends nothing).
    fn start(&self, source_id: &str) -> bool;
    /// The next 20 ms Opus packet, if one is ready (non-blocking).
    fn poll_frame(&self) -> Option<Vec<u8>>;
    /// The next 10 ms (480 samples) of 48 kHz mono PCM, if ready (non-blocking).
    fn poll_pcm(&self) -> Option<Vec<i16>> {
        None
    }
    /// Stop capturing (the peer connection closed).
    fn stop(&self);
}

/// Something the peer connection or one of its data channels reported, for the
/// dispatcher to relay to Teams as a webrtc.1 event. Teams' JS drives its call
/// off these — it starts its media-control protocol (capabilities, heartbeats,
/// video subscriptions) on the data channel only after that channel's `open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    /// `iceconnectionstatechange` / `connectionstatechange` /
    /// `icegatheringstatechange`, with the new state's name.
    State { event: &'static str, state: String },
    /// A data channel opened. `object_id` is its remoted id, `stream_id` its SCTP
    /// stream id (the add-in reports it as `id`).
    ChannelOpen { object_id: u64, stream_id: u16 },
    /// A data channel is closing.
    ChannelClosing { object_id: u64, stream_id: u16 },
    /// A message arrived on a data channel.
    ChannelMessage { object_id: u64, stream_id: u16, data: Vec<u8> },
}

/// Queue an event from an engine callback.
pub(crate) fn push_event(events: &Arc<Mutex<Vec<PeerEvent>>>, event: PeerEvent) {
    if let Ok(mut q) = events.lock() {
        q.push(event);
    }
}
