//! The client's media devices, as reported to `MediaDevices.enumerateDevices`.
//!
//! **This is a gate on Teams optimizing at all.** Before Teams will run a call's
//! media on the endpoint, it asks the redirector to enumerate the client's
//! devices. An endpoint that answers with an empty list has no microphone, no
//! speaker and no camera — so it plainly *cannot* host the media, and Teams
//! silently falls back to rendering the call inside the RDP session (the call
//! still connects; it just isn't optimized). We observed exactly that: with a
//! stubbed `[]` reply, Teams did the capability handshake, called
//! `enumerateDevices`, and then never even created a peer connection.
//!
//! The list is platform-specific (Win32 wave/Media Foundation, ALSA/PulseAudio/V4L2
//! on Linux), so this crate only defines the shape and lets the host supply it via
//! [`DeviceProvider`].

use serde_json::{json, Value};

/// What a device does. Serializes to the webrtc.1 / W3C `kind` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

impl DeviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::AudioInput => "audioinput",
            DeviceKind::AudioOutput => "audiooutput",
            DeviceKind::VideoInput => "videoinput",
        }
    }
}

/// One entry of the `enumerateDevices` result, mirroring W3C `MediaDeviceInfo`.
///
/// `device_id` is opaque to Teams — it hands it straight back as the `sourceId`
/// constraint when it asks us to capture from the device, so the host is free to
/// choose the encoding as long as it can map it back to a real device.
#[derive(Debug, Clone)]
pub struct MediaDevice {
    pub device_id: String,
    pub group_id: String,
    pub kind: DeviceKind,
    pub label: String,
}

impl MediaDevice {
    pub fn new(
        kind: DeviceKind,
        device_id: impl Into<String>,
        label: impl Into<String>,
        group_id: impl Into<String>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            group_id: group_id.into(),
            kind,
            label: label.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "deviceId": self.device_id,
            "groupId": self.group_id,
            "kind": self.kind.as_str(),
            "label": self.label,
        })
    }
}

/// The group id the real add-in reports for devices with no container grouping.
pub const NO_GROUP: &str = "{00000000-0000-0000-FFFF-FFFFFFFFFFFF}";

/// Supplies the client's real media devices. Implemented per platform by the host
/// (`rdp-client` on Windows); without one the redirector reports no devices and
/// Teams will not optimize.
pub trait DeviceProvider: Send + Sync {
    fn enumerate(&self) -> Vec<MediaDevice>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_mediadeviceinfo_entry() {
        let d = MediaDevice::new(DeviceKind::AudioInput, "default", "Default - Mic", NO_GROUP);
        let j = d.to_json();
        assert_eq!(j.get("kind").and_then(Value::as_str), Some("audioinput"));
        assert_eq!(j.get("deviceId").and_then(Value::as_str), Some("default"));
        assert_eq!(j.get("label").and_then(Value::as_str), Some("Default - Mic"));
        assert!(j.get("groupId").is_some());
    }

    #[test]
    fn kind_strings_match_the_w3c_names() {
        assert_eq!(DeviceKind::AudioInput.as_str(), "audioinput");
        assert_eq!(DeviceKind::AudioOutput.as_str(), "audiooutput");
        assert_eq!(DeviceKind::VideoInput.as_str(), "videoinput");
    }
}
