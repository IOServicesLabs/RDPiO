//! Typed view over the webrtc.1 JSON-RPC message forms.
//!
//! Three shapes appear on the wire (all objects, all with a mix of the fields
//! below):
//!
//! - **Call** — the server invoking a method on a remoted object:
//!   `{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"createOffer",
//!     "rpcArgs":[…],"rpcCallId":79}`. `rpcObjectId` is absent for calls on a
//!   singleton/root object; `rpcArgs`/`rpcCallId` are absent on early fire-and-
//!   forget notifications (e.g. `setVersionInfo`).
//! - **Result** — the client's reply to a Call, correlated by `rpcCallId`:
//!   `{"result":…,"rpcObjectType":…,"rpcCallId":79}`.
//! - **Event** — an unsolicited notification from the client (e.g. an ICE
//!   candidate / local-description update, a track ended):
//!   `{"rpcEventArgs":{…}}` and/or `{"rpcEventTarget":{"rpcObjectType":…,
//!   "rpcObjectId":…}}`.
//!
//! We keep `rpcArgs`/`result`/event bodies as [`serde_json::Value`]: the schema
//! is wide (mirrors the whole WebRTC/MediaDevices API) and only a handful of
//! fields drive control flow. Higher layers reach into the `Value` for the parts
//! they need (SDP, ICE config, transceiver kind, …).

use serde::Deserialize;
use serde_json::Value;

/// Which of the three message shapes a parsed message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcMessageKind {
    /// Server → client method invocation.
    Call,
    /// Client → server reply to a [`RpcMessageKind::Call`].
    Result,
    /// Client → server unsolicited notification.
    Event,
    /// Recognizable JSON but none of the above (unknown/edge shape).
    Other,
}

/// The raw JSON fields common to webrtc.1 messages. Deserialized leniently so a
/// single struct covers all three shapes; use [`RpcMessage::kind`] to classify.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcMessage {
    #[serde(rename = "rpcObjectType", default)]
    pub object_type: Option<String>,
    /// Present on calls to a specific object. Numeric on the wire; kept as a
    /// `Value` because a few messages omit or vary it.
    #[serde(rename = "rpcObjectId", default)]
    pub object_id: Option<Value>,
    #[serde(rename = "rpcName", default)]
    pub name: Option<String>,
    #[serde(rename = "rpcArgs", default)]
    pub args: Option<Value>,
    #[serde(rename = "rpcCallId", default)]
    pub call_id: Option<u64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(rename = "rpcEventArgs", default)]
    pub event_args: Option<Value>,
    #[serde(rename = "rpcEventTarget", default)]
    pub event_target: Option<Value>,
}

impl RpcMessage {
    /// Parse one message's JSON bytes (already stripped of framing).
    pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(json)
    }

    /// Classify the message shape.
    pub fn kind(&self) -> RpcMessageKind {
        if self.result.is_some() {
            RpcMessageKind::Result
        } else if self.event_args.is_some() || self.event_target.is_some() {
            RpcMessageKind::Event
        } else if self.name.is_some() {
            RpcMessageKind::Call
        } else {
            RpcMessageKind::Other
        }
    }

    /// The target object's id, if the message carries a numeric `rpcObjectId`.
    pub fn object_id_u64(&self) -> Option<u64> {
        self.object_id.as_ref().and_then(|v| v.as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_a_call() {
        let m = RpcMessage::parse(
            br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"createOffer","rpcArgs":[],"rpcCallId":79}"#,
        )
        .unwrap();
        assert_eq!(m.kind(), RpcMessageKind::Call);
        assert_eq!(m.object_type.as_deref(), Some("RTCPeerConnection"));
        assert_eq!(m.object_id_u64(), Some(11));
        assert_eq!(m.name.as_deref(), Some("createOffer"));
        assert_eq!(m.call_id, Some(79));
    }

    #[test]
    fn classifies_a_result() {
        let m = RpcMessage::parse(br#"{"result":{"desc":{"type":"offer","sdp":"v=0"}},"rpcCallId":79}"#)
            .unwrap();
        assert_eq!(m.kind(), RpcMessageKind::Result);
        assert_eq!(m.call_id, Some(79));
        assert!(m.result.is_some());
    }

    #[test]
    fn classifies_an_event() {
        let m = RpcMessage::parse(br#"{"rpcEventArgs":{"desc":{"sdp":"v=0"}}}"#).unwrap();
        assert_eq!(m.kind(), RpcMessageKind::Event);
    }

    #[test]
    fn fire_and_forget_notification_is_a_call() {
        // setVersionInfo carries inline fields, no rpcCallId.
        let m = RpcMessage::parse(
            br#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"setVersionInfo","version":{"shim":"1"}}"#,
        )
        .unwrap();
        assert_eq!(m.kind(), RpcMessageKind::Call);
        assert_eq!(m.call_id, None);
    }
}
