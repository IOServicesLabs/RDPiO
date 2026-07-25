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
            "setAttribute" => {
                if let (Some(id), Some("srcObject")) = (oid, arg(0).and_then(Value::as_str)) {
                    if let Some(sid) = arg(1).and_then(|v| v.get("rpcObjectId")).and_then(Value::as_u64)
                    {
                        self.elements.entry(id).or_default().src_stream_id = Some(sid);
                    }
                }
            }
            "notifyObjectFitChanged" => {
                if let (Some(id), Some(f)) = (oid, arg(0).and_then(Value::as_str)) {
                    self.elements.entry(id).or_default().object_fit = Some(f.to_string());
                }
            }
            "notifyTransformChanged" => {
                if let (Some(id), Some(t)) = (oid, arg(0).and_then(Value::as_str)) {
                    self.elements.entry(id).or_default().transform = Some(t.to_string());
                }
            }
            "notifyVisibilityChanged" => {
                if let (Some(id), Some(v)) = (oid, arg(0).and_then(Value::as_bool)) {
                    self.elements.entry(id).or_default().visible = v;
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
                    self.streams.entry(id).or_default();
                }
            }
            "createMediaStreamTrack" => {
                if let Some(track_id) = oid {
                    if let Some(stream_id) =
                        arg(0).and_then(|v| v.get("mediaStreamRpcObjectId")).and_then(Value::as_u64)
                    {
                        self.streams.entry(stream_id).or_default().tracks.push(track_id);
                    }
                }
            }
            _ => {}
        }
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

    #[test]
    fn hiding_an_element_removes_it_from_targets() {
        let mut m = PresentationModel::new();
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"createMediaElement","rpcArgs":["video","h",{"left":0,"top":0,"right":0,"bottom":0}]}"#));
        assert_eq!(m.visible_video_targets().len(), 1);
        m.observe(&call(br#"{"rpcObjectType":"MediaElement","rpcObjectId":1,"rpcName":"notifyVisibilityChanged","rpcArgs":[false,"t"]}"#));
        assert_eq!(m.visible_video_targets().len(), 0);
    }
}
