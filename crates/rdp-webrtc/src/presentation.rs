//! Media presentation model — *what renders where*.
//!
//! Beyond the peer connection, the redirector remotes the DOM media graph so the
//! client knows how to present each stream inside the session: which
//! `MediaElement` shows which `MediaStream`, its object-fit and mirror transform,
//! whether it's visible, and the overall clip rectangle of the redirected surface.
//! A native renderer (Phase C) consumes this to composite decoded video into the
//! right place in the RDP session window.
//!
//! This layer is pure (no engine dependency) and reconstructs the state from the
//! same webrtc.1 Calls the dispatcher receives — validated against a real capture
//! in `tests/presentation_replay.rs`.

use std::collections::HashMap;

use serde_json::Value;

use crate::rpc::{RpcMessage, RpcMessageKind};

/// A rectangle in session pixels, as the redirector expresses geometry
/// (`{left, top, right, bottom}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }
    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }
    pub fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }
}

/// A presented media sink (a `<video>`/`<audio>` surface in the session).
#[derive(Debug, Clone, Default)]
pub struct MediaElement {
    /// `"video"` or `"audio"`.
    pub kind: String,
    /// Placement rect from `createMediaElement` (often refined by the app; may be
    /// empty when the host manages layout via the clip rect instead).
    pub rect: Rect,
    /// CSS object-fit, e.g. `"contain"` / `"cover"`.
    pub object_fit: Option<String>,
    /// CSS transform, e.g. `"scaleX(-1)"` — a mirror, typical of self-view.
    pub transform: Option<String>,
    /// Whether the element is currently shown.
    pub visible: bool,
    /// The `MediaStream` object id feeding this element (`srcObject`).
    pub src_stream_id: Option<u64>,
    /// The MS-RDPEGT geometry mapping id locating this element on the desktop —
    /// the token `notifyVisibilityChanged` carries as a decimal string.
    pub mapping_id: Option<u64>,
}

impl MediaElement {
    /// A self-view is mirrored (`scaleX(-1)`); remote participants are not.
    pub fn is_mirrored(&self) -> bool {
        self.transform.as_deref().is_some_and(|t| t.contains("scaleX(-1)"))
    }
}

/// A remoted `MediaStream` and the tracks bound to it.
#[derive(Debug, Clone, Default)]
pub struct MediaStream {
    /// `rpcObjectId`s of the `MediaStreamTrack`s in this stream.
    pub tracks: Vec<u64>,
    /// The capture device a local stream was opened on: the camera's
    /// `constraints.video.mandatory.sourceId` or the mic's
    /// `constraints.audio.deviceId`. `None` for a constraint-less clone.
    pub source_id: Option<String>,
    /// Whether the stream carries video (from its constraints or its tracks).
    pub video: bool,
}

/// Reconstructed presentation state.
#[derive(Debug, Default)]
pub struct PresentationModel {
    /// `MediaElement` object id → element.
    pub elements: HashMap<u64, MediaElement>,
    /// `MediaStream` object id → stream.
    pub streams: HashMap<u64, MediaStream>,
    /// The redirected surface's clip rectangle (from the root redirector).
    pub clip_rect: Option<Rect>,
    /// Whether the redirected surface is currently visible.
    pub clip_visible: bool,
}

fn parse_rect(v: &Value) -> Option<Rect> {
    Some(Rect {
        left: v.get("left")?.as_i64()? as i32,
        top: v.get("top")?.as_i64()? as i32,
        right: v.get("right")?.as_i64()? as i32,
        bottom: v.get("bottom")?.as_i64()? as i32,
    })
}

impl PresentationModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one message into the model. Only server Calls carry presentation
    /// state; results/events are ignored.
    pub fn observe(&mut self, msg: &RpcMessage) {
        if msg.kind() != RpcMessageKind::Call {
            return;
        }
        let method = msg.name.as_deref().unwrap_or("");
        let oid = msg.object_id_u64();
        let arg = |i: usize| msg.args.as_ref().and_then(|a| a.get(i));

        match method {
            "createMediaElement" => {
                let Some(id) = oid else { return };
                let e = self.elements.entry(id).or_default();
                if let Some(k) = arg(0).and_then(Value::as_str) {
                    e.kind = k.to_string();
                }
                if let Some(r) = arg(2).and_then(parse_rect) {
                    e.rect = r;
                }
                e.visible = true;
            }
            // The rest only update a live element: Teams sends late notifications
            // for elements it already shut down, which must not bring them back.
            "setAttribute" => {
                if let (Some(e), Some("srcObject")) =
                    (oid.and_then(|id| self.elements.get_mut(&id)), arg(0).and_then(Value::as_str))
                {
                    if let Some(sid) = arg(1).and_then(|v| v.get("rpcObjectId")).and_then(Value::as_u64)
                    {
                        e.src_stream_id = Some(sid);
                    }
                }
            }
            "notifyObjectFitChanged" => {
                if let (Some(e), Some(f)) =
                    (oid.and_then(|id| self.elements.get_mut(&id)), arg(0).and_then(Value::as_str))
                {
                    e.object_fit = Some(f.to_string());
                }
            }
            "notifyTransformChanged" => {
                if let (Some(e), Some(t)) =
                    (oid.and_then(|id| self.elements.get_mut(&id)), arg(0).and_then(Value::as_str))
                {
                    e.transform = Some(t.to_string());
                }
            }
            "notifyVisibilityChanged" => {
                if let (Some(e), Some(v)) =
                    (oid.and_then(|id| self.elements.get_mut(&id)), arg(0).and_then(Value::as_bool))
                {
                    e.visible = v;
                    if let Some(m) = arg(1).and_then(Value::as_str).and_then(|t| t.parse().ok()) {
                        e.mapping_id = Some(m);
                    }
                }
            }
            "shutdown" if msg.object_type.as_deref() == Some("MediaElement") => {
                if let Some(id) = oid {
                    self.elements.remove(&id);
                }
            }
            "notifyClipRectChanged" => {
                if let Some(v) = arg(0).and_then(Value::as_bool) {
                    self.clip_visible = v;
                }
                if let Some(r) = arg(1).and_then(parse_rect) {
                    self.clip_rect = Some(r);
                }
            }
            "createMediaStream" => {
                if let Some(id) = oid {
                    let s = self.streams.entry(id).or_default();
                    let constraints = arg(0).and_then(|v| v.get("constraints"));
                    let video = constraints.and_then(|c| c.get("video"));
                    s.source_id = video
                        .and_then(|v| v.pointer("/mandatory/sourceId"))
                        .or_else(|| constraints.and_then(|c| c.pointer("/audio/deviceId")))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    s.video |= video.is_some();
                }
            }
            "createMediaStreamTrack" => {
                if let Some(track_id) = oid {
                    if let Some(stream_id) =
                        arg(0).and_then(|v| v.get("mediaStreamRpcObjectId")).and_then(Value::as_u64)
                    {
                        let s = self.streams.entry(stream_id).or_default();
                        s.tracks.push(track_id);
                        s.video |= arg(0).and_then(|v| v.get("kind")).and_then(Value::as_str)
                            == Some("video");
                    }
                }
            }
            _ => {}
        }
    }

    /// The local camera (`sourceId`, e.g. `rdpio-videoinput-0`) a stream shows,
    /// for streams the server created (remote streams arrive through `track`
    /// events instead and never appear here). A constraint-less stream is a clone
    /// Teams makes for display: it shows the camera of the newest video stream
    /// opened on a device before it (Teams opens the device, then clones it).
    pub fn camera_for_stream(&self, stream_id: u64) -> Option<&str> {
        let stream = self.streams.get(&stream_id)?;
        if !stream.video {
            return None;
        }
        if let Some(src) = stream.source_id.as_deref() {
            return Some(src);
        }
        self.streams
            .iter()
            .filter(|(id, s)| **id < stream_id && s.video && s.source_id.is_some())
            .max_by_key(|(id, _)| **id)
            .and_then(|(_, s)| s.source_id.as_deref())
    }

    /// Visible video elements — the surfaces a renderer must draw into.
    pub fn visible_video_targets(&self) -> Vec<(u64, &MediaElement)> {
        let mut v: Vec<_> = self
            .elements
            .iter()
            .filter(|(_, e)| e.kind == "video" && e.visible)
            .map(|(id, e)| (*id, e))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(json: &[u8]) -> RpcMessage {
        RpcMessage::parse(json).unwrap()
    }

    #[test]
    fn tracks_a_video_element_and_its_stream() {
        let mut m = PresentationModel::new();
        m.observe(&call(br#"{"rpcObjectType":"MediaStream","rpcObjectId":6,"rpcName":"createMediaStream","rpcArgs":[{"id":"s6"}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaStreamTrack","rpcObjectId":7,"rpcName":"createMediaStreamTrack","rpcArgs":[{"mediaStreamRpcObjectId":6,"kind":"video"}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"createMediaElement","rpcArgs":["video","hwnd",{"left":0,"top":0,"right":0,"bottom":0}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"setAttribute","rpcArgs":["srcObject",{"rpcObjectType":"MediaStream","rpcObjectId":6}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"notifyObjectFitChanged","rpcArgs":["contain"]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"notifyTransformChanged","rpcArgs":["scaleX(-1)"]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"notifyVisibilityChanged","rpcArgs":[true,"tok"]}"#));
        m.observe(&call(br#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"notifyClipRectChanged","rpcArgs":[true,{"left":0,"top":32,"right":1507,"bottom":92}]}"#));

        let e = &m.elements[&1];
        assert_eq!(e.kind, "video");
        assert_eq!(e.src_stream_id, Some(6));
        assert_eq!(e.object_fit.as_deref(), Some("contain"));
        assert!(e.is_mirrored());
        assert!(e.visible);
        assert_eq!(m.streams[&6].tracks, vec![7]);
        assert_eq!(m.clip_rect.unwrap().width(), 1507);
        assert!(m.clip_visible);
        assert_eq!(m.visible_video_targets().len(), 1);
    }

    /// The Teams Settings camera preview, as captured live: Teams opens the
    /// device (`sourceId`), then shows a constraint-less clone of it in a
    /// mirrored video element whose visibility carries the geometry mapping id.
    #[test]
    fn a_camera_preview_resolves_to_its_device_and_mapping() {
        let mut m = PresentationModel::new();
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":155,"rpcName":"createMediaElement","rpcArgs":["video","0000000000030358",{"top":0,"right":0,"bottom":0,"left":0}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaStream","rpcObjectId":158,"rpcName":"createMediaStream","rpcArgs":[{"id":"a","constraints":{"video":{"mandatory":{"sourceId":"rdpio-videoinput-1","minWidth":1280}}}}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaStream","rpcObjectId":159,"rpcName":"createMediaStream","rpcArgs":[{"id":"mic","constraints":{"audio":{"deviceId":"rdpio-audioinput-3"}}}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaStream","rpcObjectId":160,"rpcName":"createMediaStream","rpcArgs":[{"id":"b"}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaStreamTrack","rpcObjectId":161,"rpcName":"createMediaStreamTrack","rpcArgs":[{"mediaStreamRpcObjectId":160,"id":"t","kind":"video","label":"EOS Webcam Utility"}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":155,"rpcName":"setAttribute","rpcArgs":["srcObject",{"rpcObjectType":"MediaStream","rpcObjectId":160}]}"#));
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":155,"rpcName":"notifyVisibilityChanged","rpcArgs":[true,"9223415772510227732"]}"#));

        let e = &m.elements[&155];
        assert_eq!(e.mapping_id, Some(9223415772510227732));
        assert_eq!(e.src_stream_id, Some(160));
        assert_eq!(m.camera_for_stream(160), Some("rdpio-videoinput-1"));
        assert_eq!(m.camera_for_stream(158), Some("rdpio-videoinput-1"));
        assert_eq!(m.camera_for_stream(159), None, "a mic stream is not a camera");
        assert_eq!(m.camera_for_stream(999), None, "a remote stream is not local");

        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":155,"rpcName":"shutdown","rpcArgs":[]}"#));
        assert!(m.elements.is_empty(), "shutdown removes the element");
    }

    #[test]
    fn hiding_an_element_removes_it_from_targets() {
        let mut m = PresentationModel::new();
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"createMediaElement","rpcArgs":["video","h",{"left":0,"top":0,"right":0,"bottom":0}]}"#));
        assert_eq!(m.visible_video_targets().len(), 1);
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"notifyVisibilityChanged","rpcArgs":[false,"t"]}"#));
        assert_eq!(m.visible_video_targets().len(), 0);
    }
}
