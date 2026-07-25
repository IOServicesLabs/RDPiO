//! The remoted JavaScript object model.
//!
//! Every method the server invokes names an `rpcObjectType`. These map 1:1 onto
//! W3C WebRTC / Media Capture objects (and a couple of Teams-redirector-specific
//! ones). A native engine implements each type; this enum is how we classify and
//! route calls, and — via [`ObjectType::engine_backed`] — mark which types own a
//! real WebRTC engine object versus which are pure control/UI surfaces.

/// A remoted object type on the webrtc.1 channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectType {
    /// Root control object: version handshake, call info, E911, video geometry
    /// notifications (`notifyClipRectChanged`, …), shutdown.
    Redirector,
    /// `navigator.mediaDevices`: `enumerateDevices`, `setSinkId`.
    MediaDevices,
    /// Call-control HID devices (headset buttons). Stubbable.
    HidManager,
    /// `RTCPeerConnection`: the actual peer connection (offer/answer/ICE/tracks).
    PeerConnection,
    /// `RTCRtpTransceiver`: per-m-line direction + track binding.
    RtpTransceiver,
    /// `RTCRtpSender`: the send side of a transceiver (`replaceTrack`, `getStats`,
    /// `setParameters`).
    RtpSender,
    /// `RTCRtpReceiver`: the receive side of a transceiver.
    RtpReceiver,
    /// `RTCDataChannel`.
    DataChannel,
    /// `MediaStream`.
    MediaStream,
    /// `MediaStreamTrack`.
    MediaStreamTrack,
    /// `HTMLMediaElement`-like sink: where/whether decoded media is presented.
    MediaElement,
    /// An `rpcObjectType` we don't recognize yet.
    Unknown,
}

impl ObjectType {
    /// Classify by the wire `rpcObjectType` string.
    pub fn from_name(s: &str) -> Self {
        match s {
            "RDWebRTCRedirector" => Self::Redirector,
            "MediaDevices" => Self::MediaDevices,
            "HidManager" => Self::HidManager,
            "RTCPeerConnection" => Self::PeerConnection,
            "RTCRtpTransceiver" => Self::RtpTransceiver,
            "RTCRtpSender" => Self::RtpSender,
            "RTCRtpReceiver" => Self::RtpReceiver,
            "RTCDataChannel" => Self::DataChannel,
            "MediaStream" => Self::MediaStream,
            "MediaStreamTrack" => Self::MediaStreamTrack,
            "MediaElement" => Self::MediaElement,
            _ => Self::Unknown,
        }
    }

    /// Whether instances of this type are backed by a real WebRTC engine object
    /// (as opposed to redirector/UI control surfaces). Guides where the future
    /// engine layer needs to allocate `webrtc-rs` objects.
    pub fn engine_backed(self) -> bool {
        matches!(
            self,
            Self::PeerConnection
                | Self::RtpTransceiver
                | Self::RtpSender
                | Self::RtpReceiver
                | Self::DataChannel
                | Self::MediaStream
                | Self::MediaStreamTrack
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_types() {
        assert_eq!(ObjectType::from_name("RTCPeerConnection"), ObjectType::PeerConnection);
        assert_eq!(ObjectType::from_name("RDWebRTCRedirector"), ObjectType::Redirector);
        assert_eq!(ObjectType::from_name("nope"), ObjectType::Unknown);
    }

    #[test]
    fn engine_backing_is_marked() {
        assert!(ObjectType::PeerConnection.engine_backed());
        assert!(!ObjectType::Redirector.engine_backed());
    }
}
