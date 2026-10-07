//! The live redirector dispatcher (feature `engine`).
//!
//! Phase B2: bridge the reversed protocol to the real engine. Where
//! [`crate::session::RedirectorModel`] only *observes* a message stream, this
//! *drives* it — each inbound server [Call] is routed to a [`WebrtcEngine`]
//! method, and the return value / gathered ICE is marshalled back into the
//! webrtc.1 [Result] and [Event] messages the server expects. Feed it a live
//! channel and it runs an optimized call; feed it the calls from a captured one
//! and it re-runs that negotiation (the `dispatch_replay` test).
//!
//! ## Wire fidelity
//! The reply envelope mirrors the real add-in exactly (verified against the
//! capture): every reply carries `hr` (0 = success, else an HRESULT), echoes
//! `rpcObjectType` / `rpcObjectId` / `rpcName`, and is keyed by `rpcCallId`.
//! Crucially, the client answers the server's `setVersionInfo` with an unsolicited
//! `sessioninfo` **event** advertising the client's display, versions and feature
//! set — this capability handshake is what tells Teams the endpoint can host the
//! optimized media path, i.e. what flips the call into the "Optimized" state.
//!
//! [Call]: crate::rpc::RpcMessageKind::Call
//! [Result]: crate::rpc::RpcMessageKind::Result
//! [Event]: crate::rpc::RpcMessageKind::Event

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::devices::{DeviceProvider, MediaDevice};
use crate::engine::{PeerEvent, WebrtcEngine};
use crate::ice::TurnResolver;
use crate::objects::ObjectType;
use crate::rpc::{RpcMessage, RpcMessageKind};

/// `E_FAIL` — the generic failure HRESULT we report when an engine call fails.
const HR_E_FAIL: i64 = -2147467259; // 0x80004005
/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)` — what the real add-in answers
/// `rtcSession` with.
const HR_NOT_SUPPORTED: i64 = -2147024846; // 0x80070032

/// The client's send/receive codec + header-extension capabilities, returned from
/// `createPeerConnection`. Mirrors what the real add-in reports (a superset of any
/// single negotiation — standard WebRTC practice); Teams reads it to know what the
/// endpoint can encode/decode. webrtc-rs actually negotiates opus/VP8/H264 from
/// this set in the SDP offer it generates.
const CAPABILITIES_JSON: &str = r#"{
  "sendCapabilities": {
    "audio": {
      "codecs": [
        {"mimeType":"audio/opus","clockRate":48000,"channels":2,"sdpFmtpLine":"minptime=10;useinbandfec=1"},
        {"mimeType":"audio/red","clockRate":48000,"channels":2,"sdpFmtpLine":"=111/111"},
        {"mimeType":"audio/G722","clockRate":8000,"channels":1},
        {"mimeType":"audio/PCMU","clockRate":8000,"channels":1},
        {"mimeType":"audio/PCMA","clockRate":8000,"channels":1},
        {"mimeType":"audio/CN","clockRate":8000,"channels":1},
        {"mimeType":"audio/telephone-event","clockRate":48000,"channels":1},
        {"mimeType":"audio/telephone-event","clockRate":8000,"channels":1}
      ],
      "headerExtensions": [
        {"uri":"urn:ietf:params:rtp-hdrext:ssrc-audio-level"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"},
        {"uri":"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:mid"}
      ]
    },
    "video": {
      "codecs": [
        {"mimeType":"video/VP8","clockRate":90000},
        {"mimeType":"video/rtx","clockRate":90000},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"},
        {"mimeType":"video/VP9","clockRate":90000,"sdpFmtpLine":"profile-id=0"},
        {"mimeType":"video/red","clockRate":90000},
        {"mimeType":"video/ulpfec","clockRate":90000}
      ],
      "headerExtensions": [
        {"uri":"urn:ietf:params:rtp-hdrext:toffset"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"},
        {"uri":"urn:3gpp:video-orientation"},
        {"uri":"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/video-content-type"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:mid"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"}
      ]
    }
  },
  "recvCapabilities": {
    "audio": {
      "codecs": [
        {"mimeType":"audio/opus","clockRate":48000,"channels":2,"sdpFmtpLine":"minptime=10;useinbandfec=1"},
        {"mimeType":"audio/red","clockRate":48000,"channels":2,"sdpFmtpLine":"=111/111"},
        {"mimeType":"audio/G722","clockRate":8000,"channels":1},
        {"mimeType":"audio/PCMU","clockRate":8000,"channels":1},
        {"mimeType":"audio/PCMA","clockRate":8000,"channels":1},
        {"mimeType":"audio/CN","clockRate":8000,"channels":1},
        {"mimeType":"audio/telephone-event","clockRate":48000,"channels":1},
        {"mimeType":"audio/telephone-event","clockRate":8000,"channels":1}
      ],
      "headerExtensions": [
        {"uri":"urn:ietf:params:rtp-hdrext:ssrc-audio-level"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"},
        {"uri":"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:mid"}
      ]
    },
    "video": {
      "codecs": [
        {"mimeType":"video/VP8","clockRate":90000},
        {"mimeType":"video/rtx","clockRate":90000},
        {"mimeType":"video/VP9","clockRate":90000,"sdpFmtpLine":"profile-id=0"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"},
        {"mimeType":"video/H264","clockRate":90000,"sdpFmtpLine":"level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"},
        {"mimeType":"video/red","clockRate":90000},
        {"mimeType":"video/ulpfec","clockRate":90000}
      ],
      "headerExtensions": [
        {"uri":"urn:ietf:params:rtp-hdrext:toffset"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"},
        {"uri":"urn:3gpp:video-orientation"},
        {"uri":"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"},
        {"uri":"http://www.webrtc.org/experiments/rtp-hdrext/video-content-type"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:mid"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"},
        {"uri":"urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"}
      ]
    }
  }
}"#;

/// The feature set the client advertises in its `sessioninfo` event. Mirrors the
/// real add-in's block (the exact set Teams accepted to enable optimization).
const SESSION_FEATURES_JSON: &str = r#"{
  "clipRectMode": "clipRegion",
  "screenshare": "enabled",
  "multimonitorscreenshare": "enabled",
  "appshareClientSupport": "enabled",
  "givecontrolv2": "enabled",
  "hid": "enabled",
  "datachannel": "enabled",
  "unifiedplan": "enabled",
  "e911info": "enabled",
  "sharesystemaudio": "enabled",
  "givecontrol": "enabled",
  "secondaryringer": "enabled",
  "mirrormyselfvideo": "enabled",
  "maxSimulcastLayers": 2,
  "maxOutgoingResolution": 1080
}"#;

/// Drives a [`WebrtcEngine`] from webrtc.1 Calls and produces the outbound
/// Result/Event messages (as JSON values ready to frame with
/// [`crate::framing::frame`]).
pub struct Redirector {
    engine: WebrtcEngine,
    /// The `rpcObjectId` of the peer connection (event target for ICE updates).
    pc_object_id: Option<u64>,
    /// The offer we generated for the current negotiation. On `setLocalDescription`
    /// we apply *this* (webrtc-rs must set the description it created) rather than
    /// the SDP echoed in the call args.
    last_offer: Option<String>,
    /// The offer **Teams believes** is our local description: the (usually munged)
    /// SDP it passed to `setLocalDescription`. We apply our own `last_offer` to
    /// webrtc-rs, but every `icecandidate` event must report *this* back as `desc`
    /// so Teams' mirror of our `localDescription` stays consistent with what it set —
    /// reporting webrtc-rs's own (codec-superset) SDP instead desyncs Teams, which
    /// then aborts the call ~100 ms after `setLocalDescription` (long before a real
    /// media-server answer, which takes ~1.3 s, could arrive).
    teams_local_offer: Option<String>,
    /// Next id for objects WE own and hand to Teams (receiver/sender/track/stream in
    /// the `track` events fired after `setRemoteDescription`). Allocated from a high
    /// base so it can't collide with the small ids Teams assigns to its own objects.
    next_obj_id: u64,
    /// `transceiverRpcObjectId` → the (sender, receiver) object ids Teams assigned
    /// with it at `addTransceiver`. The `track` events must name these.
    transceiver_objects: HashMap<u64, (u64, u64)>,
    /// Whether Teams has applied our offer with `setLocalDescription`. ICE
    /// candidates and the gathering-complete event are held until then: the add-in
    /// only starts gathering at `setLocalDescription`, and every candidate event
    /// carries the local description Teams set.
    local_offer_set: bool,
    /// State events held back until `local_offer_set` (see there).
    held_events: Vec<Value>,
    /// `MediaStream` rpc id → the capture device its constraints named.
    stream_devices: HashMap<u64, String>,
    /// Whether this peer connection's one `negotiationneeded` event has fired.
    negotiation_needed_sent: bool,
    /// Every candidate line trickled to Teams for this peer connection, so each
    /// `icecandidate` event's `desc` carries them all (and the default address).
    trickled: Vec<String>,
    /// Client display size advertised in the `sessioninfo` handshake (width,
    /// height). Informational for the host's video compositing; defaults to 1080p.
    display: (u32, u32),
    /// Supplies the client's real cameras/microphones/speakers. Without one we
    /// report no devices — and Teams then refuses to optimize the call at all.
    devices: Option<Arc<dyn DeviceProvider>>,
}

impl Default for Redirector {
    fn default() -> Self {
        Self::new()
    }
}

impl Redirector {
    pub fn new() -> Self {
        Self {
            engine: WebrtcEngine::new(),
            pc_object_id: None,
            last_offer: None,
            teams_local_offer: None,
            next_obj_id: 0x1_0000_0000,
            transceiver_objects: HashMap::new(),
            local_offer_set: false,
            held_events: Vec::new(),
            negotiation_needed_sent: false,
            stream_devices: HashMap::new(),
            trickled: Vec::new(),
            display: (1920, 1080),
            devices: None,
        }
    }

    /// Set the client display size reported in the `sessioninfo` handshake.
    pub fn set_display_size(&mut self, width: u32, height: u32) {
        self.display = (width, height);
    }

    /// Install the source of the client's real media devices. Required for Teams to
    /// optimize: it will not move a call's media to an endpoint that reports none.
    pub fn set_device_provider(&mut self, devices: Arc<dyn DeviceProvider>) {
        self.devices = Some(devices);
    }

    /// Install the TURN redirect resolver used when building each peer connection
    /// (forwarded to the engine). Without it, webrtc-rs can't allocate a relay
    /// candidate on Teams' anycast TURN and every ICE check fails.
    pub fn set_turn_resolver(&mut self, resolver: Arc<dyn TurnResolver>) {
        self.engine.set_turn_resolver(resolver);
    }

    /// Install the outbound camera video source (forwarded to the engine). When Teams
    /// `replaceTrack`s the camera onto a video sender, the engine attaches an H.264
    /// send track fed from here — putting real outbound video in the offer so Plaza
    /// accepts the video m-line (and the bundled data channel with it).
    pub fn set_video_source(&mut self, source: Arc<dyn crate::engine::VideoCaptureSource>) {
        self.engine.set_video_source(source);
    }

    /// Install the outbound mic audio source (forwarded to the engine), fed into the
    /// audio sender's Opus track when Teams `replaceTrack`s the mic.
    pub fn set_audio_source(&mut self, source: Arc<dyn crate::engine::AudioCaptureSource>) {
        self.engine.set_audio_source(source);
    }

    /// Install where inbound remote media goes (forwarded to the engine) — the
    /// client's decoder/player for the call's audio and video.
    pub fn set_media_sink(&mut self, sink: Arc<dyn crate::engine::MediaSink>) {
        self.engine.set_sink(sink);
    }

    /// Handle one inbound message. Non-Calls are ignored (the client doesn't act
    /// on its own results/events). Returns the outbound messages to send back.
    pub async fn handle(&mut self, msg: &RpcMessage) -> Vec<Value> {
        if msg.kind() != RpcMessageKind::Call {
            return Vec::new();
        }
        let method = msg.name.as_deref().unwrap_or("");
        let otype = msg
            .object_type
            .as_deref()
            .map(ObjectType::from_name)
            .unwrap_or(ObjectType::Unknown);

        // The capability handshake. The server pushes its versions via
        // `setVersionInfo` (no callId); the client answers with an unsolicited
        // `sessioninfo` event advertising its display/versions/features. Emitting
        // this is what makes Teams treat the endpoint as optimization-capable.
        if otype == ObjectType::Redirector && method == "setVersionInfo" {
            tracing::info!("dispatch: setVersionInfo → advertising sessioninfo (capability handshake)");
            return vec![self.session_info_event()];
        }

        // Right after the handshake the server offers an `rtcSession` (a session
        // id, a timestamp, a 64-hex token and the shim version). The real add-in
        // DECLINES it with ERROR_NOT_SUPPORTED — in the reference capture, and
        // Teams then negotiates full audio + video + data-channel WebRTC media.
        // Acking it as a success (our old generic ack) claims a capability the
        // add-in doesn't, before any call exists; every native call since was
        // answered audio-only. Decline exactly as the add-in does.
        if otype == ObjectType::Redirector && method == "rtcSession" {
            tracing::info!("dispatch: rtcSession → declining with ERROR_NOT_SUPPORTED, as the add-in does");
            return msg
                .call_id
                .map(|cid| vec![reply(msg, cid, None, HR_NOT_SUPPORTED)])
                .unwrap_or_default();
        }

        let a0 = msg
            .args
            .as_ref()
            .and_then(|a| a.get(0))
            .cloned()
            .unwrap_or(Value::Null);
        let sdp_arg = a0.get("sdp").and_then(Value::as_str).unwrap_or("");

        // Route to the engine; each arm yields the JSON `result` payload.
        let outcome: Result<Value, String> = match (otype, method) {
            (ObjectType::PeerConnection, "createPeerConnection") => {
                self.pc_object_id = msg.object_id_u64();
                self.last_offer = None;
                self.teams_local_offer = None;
                self.transceiver_objects.clear();
                self.local_offer_set = false;
                self.held_events.clear();
                self.negotiation_needed_sent = false;
                self.trickled.clear();
                let n_ice = a0
                    .get("iceServers")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                match self.engine.create_peer_connection(&a0).await {
                    Ok(()) => {
                        tracing::info!(ice_servers = n_ice, "dispatch: peer connection created");
                        // Report the client's codec capabilities (not a bare ack).
                        Ok(capabilities())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            // Teams opens a "main-channel" data channel BEFORE createOffer and
            // requires the offer to negotiate it (`m=application`/SCTP). Acking
            // this without actually opening one produced an offer with no data
            // section — Teams then closed the data channel and the peer connection
            // without ever answering. So open it for real.
            (ObjectType::PeerConnection, "createDataChannel") => {
                let label = a0.get("label").and_then(Value::as_str).unwrap_or("main-channel");
                let id = a0.get("rpcObjectId").and_then(Value::as_u64).unwrap_or(0);
                match self.engine.create_data_channel(label, id).await {
                    Ok(()) => {
                        tracing::info!(label, "dispatch: data channel opened (offer gains m=application)");
                        Ok(ack())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            // Teams' media-control protocol rides the data channel (capabilities,
            // heartbeats, dominant speaker, video subscriptions) — carry its sends
            // for real. Acking without sending starved Plaza of heartbeats and it
            // dropped the call ~12 s in.
            (ObjectType::DataChannel, "send") => {
                let id = msg.object_id_u64().unwrap_or(0);
                self.engine
                    .send_data(id, data_channel_payload(&a0))
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            (ObjectType::DataChannel, "close") => {
                let id = msg.object_id_u64().unwrap_or(0);
                self.engine
                    .close_data_channel(id)
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            // Teams polls this every second once connected and reads it to decide
            // the call's media is up (transport DTLS state, selected candidate
            // pair, inbound/outbound RTP). A bare ack left the call silent.
            (ObjectType::PeerConnection, "getStats") => Ok(self.engine.stats_report().await),
            (ObjectType::PeerConnection, "close") => {
                self.last_offer = None;
                self.teams_local_offer = None;
                self.engine
                    .close_peer_connection()
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            (ObjectType::PeerConnection, "addTransceiver") => {
                let kind = a0.get("kind").and_then(Value::as_str).unwrap_or("video");
                let dir = a0.get("direction").and_then(Value::as_str).unwrap_or("inactive");
                let id = a0.get("transceiverRpcObjectId").and_then(Value::as_u64).unwrap_or(0);
                let sender_id = a0.get("senderRpcObjectId").and_then(Value::as_u64).unwrap_or(0);
                if let Some(receiver_id) = a0.get("receiverRpcObjectId").and_then(Value::as_u64) {
                    self.transceiver_objects.insert(id, (sender_id, receiver_id));
                }
                // Teams marks a *send* (camera) transceiver by supplying `sendEncodings`
                // (simulcast layers). Those must be created send-capable so `replaceTrack`
                // can bind the real camera track; receive m-lines stay recvonly.
                let encodings: Vec<crate::engine_api::SendEncoding> = a0
                    .get("sendEncodings")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|e| crate::engine_api::SendEncoding {
                                rid: e.get("rid").and_then(Value::as_str).unwrap_or("").to_string(),
                                scale_resolution_down_by: e
                                    .get("scaleResolutionDownBy")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(1.0),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.engine
                    .add_transceiver(kind, dir, id, sender_id, &encodings)
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            // Teams attaches a capture track (mic/camera) to a sender before
            // `createOffer`. For a video sender we bind a real H.264 camera track so
            // the offer carries outbound video — see `WebrtcEngine::replace_track`.
            // The camera device is the client's default video source; the exact
            // `sourceId` Teams passed is threaded through the MediaStream constraints,
            // which we don't map yet, so pass the track id as a best-effort hint.
            // Remember which device each capture stream/track was created for
            // (`constraints.audio.deviceId` / `video.mandatory.sourceId`), so the
            // mic and camera open the device Teams picked, not the default.
            (ObjectType::MediaStream, "createMediaStream") => {
                if let (Some(id), Some(dev)) = (msg.object_id_u64(), stream_device(&a0)) {
                    self.stream_devices.insert(id, dev);
                }
                Ok(ack())
            }
            (ObjectType::MediaStreamTrack, "createMediaStreamTrack") => {
                let stream = a0.get("mediaStreamRpcObjectId").and_then(Value::as_u64);
                if let (Some(track), Some(dev)) =
                    (msg.object_id_u64(), stream.and_then(|s| self.stream_devices.get(&s)))
                {
                    self.engine.set_track_device(&track.to_string(), dev);
                }
                Ok(ack())
            }
            (ObjectType::RtpSender, "replaceTrack") => {
                let sender_id = msg.object_id_u64().unwrap_or(0);
                let track_id = a0
                    .get("trackRpcObjectId")
                    .and_then(Value::as_u64)
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                self.engine
                    .replace_track(sender_id, &track_id)
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            // Teams mutes the mic by disabling its track.
            (ObjectType::MediaStreamTrack, "onEnabledAttributeChanged") => {
                if let (Some(id), Some(enabled)) = (msg.object_id_u64(), a0.as_bool()) {
                    self.engine.set_track_enabled(&id.to_string(), enabled);
                }
                Ok(ack())
            }
            (ObjectType::RtpTransceiver, "setDirection") => {
                let id = msg.object_id_u64().unwrap_or(0);
                let dir = a0.get("direction").and_then(Value::as_str).unwrap_or("inactive");
                self.engine
                    .set_transceiver_direction(id, dir)
                    .await
                    .map(|_| ack())
                    .map_err(|e| e.to_string())
            }
            (ObjectType::PeerConnection, "createOffer") => match self.engine.create_offer().await {
                Ok(sdp) => {
                    self.local_offer_set = false;
                    tracing::info!(sdp_len = sdp.len(), "dispatch: generated local offer");
                    // `setLocalDescription` must apply webrtc-rs's EXACT offer, so
                    // keep the original for that (`last_offer`); but Teams' media
                    // server expects a few attributes webrtc-rs omits, so hand *it*
                    // an enriched copy. The additions are session-level / negotiated,
                    // so Teams' answer still applies cleanly to the original.
                    self.last_offer = Some(sdp.clone());
                    // libwebrtc's offer is already what the add-in sends; only
                    // webrtc-rs's needs enriching.
                    let sent = if crate::engine::NATIVE_JSEP {
                        sdp.clone()
                    } else {
                        with_default_address(&slim_video_offer(&enrich_offer(&sdp)), &[])
                    };
                    Ok(json!({ "desc": { "type": "offer", "sdp": sent } }))
                }
                Err(e) => Err(e.to_string()),
            },
            (ObjectType::PeerConnection, "createAnswer") => self
                .engine
                .create_answer()
                .await
                .map(|sdp| json!({ "desc": { "type": "answer", "sdp": sdp } }))
                .map_err(|e| e.to_string()),
            (ObjectType::PeerConnection, "setLocalDescription") => {
                // Teams hands our own offer back here — usually *munged*: it prunes
                // codecs and narrows every video m-line (we watched 38548B → 33123B).
                // A browser's setLocalDescription tolerates a munged local SDP, but
                // webrtc-rs enforces JSEP strictly: a local *offer* must be byte-
                // identical to what createOffer produced, or set_local_description
                // fails with "new sdp does not match previous offer" and the peer
                // connection is torn down (exactly the live call-setup failure we hit).
                // Munging only prunes codecs — it preserves the m-line count/order/
                // mids and the ICE ufrag / DTLS fingerprint that Teams' later answer
                // aligns to — so we apply our *own* generated offer. That is both what
                // webrtc-rs requires and semantically correct: the answer's codecs are
                // always a subset of what we offered, so it still matches our offer.
                let sdp = match &self.last_offer {
                    // libwebrtc applies Teams' munged offer as given, like the add-in.
                    _ if crate::engine::NATIVE_JSEP && !sdp_arg.is_empty() => sdp_arg.to_string(),
                    Some(o) => {
                        if !sdp_arg.is_empty() && sdp_arg != o.as_str() {
                            tracing::info!(
                                ours = o.len(),
                                server = sdp_arg.len(),
                                same_session = same_dtls_session(o, sdp_arg),
                                "dispatch: server edited our offer; applying our own generated SDP (webrtc-rs requires the exact createOffer output)"
                            );
                        }
                        o.clone()
                    }
                    // No offer of ours on record (e.g. a capture replay feeding a
                    // foreign session's SDP) — pass the arg through unchanged.
                    None => sdp_arg.to_string(),
                };
                // Remember the SDP Teams *thinks* is our local description (its munged
                // arg), so `icecandidate` events report it back as `desc` and Teams'
                // mirror stays consistent. If Teams passed no SDP (replay), fall back
                // to what we actually applied.
                self.teams_local_offer = Some(if sdp_arg.is_empty() {
                    sdp.clone()
                } else {
                    sdp_arg.to_string()
                });
                // The result is NOT a bare ack: Teams reads the transceivers' now-
                // assigned mids back from here to map its senders/streams onto the
                // offer's m-lines. Returning `"RPC succeeded."` (no mids) is what made
                // Teams abort the call ~100 ms after setLocalDescription.
                self.engine
                    .set_local_offer(&sdp)
                    .await
                    .map(|_| json!({ "transceivers": self.engine.transceiver_states() }))
                    .map_err(|e| e.to_string())
            }
            (ObjectType::PeerConnection, "setRemoteDescription") => {
                let is_offer = a0.get("type").and_then(Value::as_str) == Some("offer");
                // Teams' media server rejects unused m-lines (e.g. every video line in
                // an audio-only call) as `m=video 0 … <pt>` + `a=inactive` with a bare
                // dynamic payload type and NO `a=rtpmap`. webrtc-rs's strict SDP layer
                // aborts the whole setRemoteDescription with "payload type not found"
                // on it; repair the answer so parsing completes (the m-line stays
                // rejected). Without this the just-answered call is torn down.
                let sanitized = if crate::engine::NATIVE_JSEP {
                    sdp_arg.to_string()
                } else {
                    sanitize_remote_sdp(sdp_arg)
                };
                let sdp_in = sanitized.as_str();
                let r = if is_offer {
                    self.engine.set_remote_offer(sdp_in).await
                } else {
                    self.engine.set_remote_answer(sdp_in).await
                };
                match &r {
                    Ok(_) => tracing::info!(
                        kind = if is_offer { "offer" } else { "answer" },
                        sdp_len = sdp_arg.len(),
                        "dispatch: applied remote description — negotiation converging"
                    ),
                    Err(e) => tracing::warn!(
                        kind = if is_offer { "offer" } else { "answer" },
                        error = %e,
                        "dispatch: setRemoteDescription failed"
                    ),
                }
                // Same transceivers result as setLocalDescription — the add-in
                // reports them after the answer too, and Teams re-reads its
                // transceiver mirror from it. A bare ack leaves that mirror empty
                // right as the call is being set up.
                r.map(|_| json!({ "transceivers": self.engine.transceiver_states() }))
                    .map_err(|e| e.to_string())
            }
            // The client's real cameras/mics/speakers. Teams gates optimization on
            // this: an endpoint reporting no devices can't host the media, so Teams
            // falls back to the in-session pipeline without even trying to build a
            // peer connection. Must be an array — the server iterates it.
            (ObjectType::MediaDevices, "enumerateDevices") => {
                let list: Vec<Value> = self
                    .devices
                    .as_ref()
                    .map(|d| d.enumerate().iter().map(MediaDevice::to_json).collect())
                    .unwrap_or_default();
                if list.is_empty() {
                    tracing::warn!(
                        "dispatch: reporting NO media devices — Teams will not optimize the call"
                    );
                } else {
                    tracing::info!(devices = list.len(), "dispatch: reporting client media devices");
                }
                Ok(Value::Array(list))
            }
            (ObjectType::HidManager, "enumerateDevices") => Ok(json!([])),
            // E911 (emergency-call location): return a well-formed placeholder.
            (ObjectType::Redirector, "getE911Info") => {
                Ok(json!({ "ipv4": "0.0.0.0", "mac": "00-00-00-00-00-00", "subnetLengthIpv4": "0" }))
            }
            // Everything else (Redirector/Hid/MediaElement/MediaStream(Track)
            // control & UI surfaces not yet engine-backed) → acknowledge so the
            // server's RPC sequence keeps flowing.
            _ => Ok(ack()),
        };

        // Reply only if the Call expects one (carries a callId).
        let Some(cid) = msg.call_id else {
            if let Err(e) = &outcome {
                tracing::warn!(method, error = %e, "dispatch: notification call failed");
            }
            return Vec::new();
        };
        match outcome {
            Ok(result) => {
                let mut out = Vec::new();
                // The add-in fires `negotiationneeded` once, when the first data
                // channel makes the new peer connection negotiable — just ahead of
                // the createDataChannel reply (reference capture).
                if otype == ObjectType::PeerConnection
                    && method == "createDataChannel"
                    && !self.negotiation_needed_sent
                {
                    self.negotiation_needed_sent = true;
                    out.push(json!({
                        "rpcEventTarget": { "rpcObjectType": "RTCPeerConnection", "rpcObjectId": self.pc_object_id },
                        "rpcEventName": "negotiationneeded",
                        "hr": 0,
                    }));
                }
                out.push(reply(msg, cid, Some(result), 0));
                // After setLocalDescription the add-in also fires the standard
                // RTCPeerConnection state events; Teams' JS advances its call-setup
                // state machine on them (and on the transceivers result above) before
                // it will wait for the media server's answer. Without them it aborts.
                if otype == ObjectType::PeerConnection && method == "setLocalDescription" {
                    out.push(self.pc_state_event("signalingstatechange", "have-local-offer"));
                    out.push(self.pc_state_event("icegatheringstatechange", "gathering"));
                    // Candidates gathered since createOffer, then any held
                    // gathering-complete event, now follow (see `local_offer_set`).
                    self.local_offer_set = true;
                }
                // After the answer applies, the add-in fires signaling/ICE state
                // events and — crucially — a `track` event per negotiated transceiver.
                // Those `track` events are how Teams learns the remote receivers exist
                // and wires them to its media elements; without them Teams tears the
                // just-answered call down ~120 ms later (ICE never gets to connect).
                if otype == ObjectType::PeerConnection && method == "setRemoteDescription" {
                    out.push(self.pc_state_event("signalingstatechange", "stable"));
                    out.push(self.pc_state_event("iceconnectionstatechange", "checking"));
                    // Fire ontrack only for the m-lines the answer accepted (non-zero
                    // port, not inactive). We read the accepted mids straight from the
                    // answer SDP rather than webrtc-rs's `current_direction`, which isn't
                    // set synchronously on the offerer right after `set_remote_answer`.
                    let accepted = accepted_mids(sdp_arg);
                    let streams = remote_stream_ids(sdp_arg);
                    let tracks = self.track_events(&accepted, &streams);
                    tracing::info!(
                        tracks = tracks.len(),
                        accepted_mlines = accepted.len(),
                        "dispatch: answer applied — firing ontrack events for the accepted receivers"
                    );
                    out.extend(tracks);
                }
                out
            }
            Err(e) => {
                tracing::warn!(method, error = %e, "dispatch: engine call failed");
                vec![reply(msg, cid, None, HR_E_FAIL)]
            }
        }
    }

    /// A peer-connection state-change event (`signalingstatechange`,
    /// `icegatheringstatechange`, …) in the add-in's shape: `{state}` on the current
    /// peer connection. Fired after `setLocalDescription` so Teams' call-setup state
    /// machine advances.
    fn pc_state_event(&self, name: &str, state: &str) -> Value {
        json!({
            "rpcEventArgs": { "state": state },
            "rpcEventTarget": {
                "rpcObjectType": "RTCPeerConnection",
                "rpcObjectId": self.pc_object_id,
            },
            "rpcEventName": name,
            "hr": 0,
        })
    }

    /// Allocate an id for an object we own and expose to Teams.
    fn alloc_obj_id(&mut self) -> u64 {
        let id = self.next_obj_id;
        self.next_obj_id += 1;
        id
    }

    /// Fire an `ontrack` event for every negotiated transceiver, mirroring the
    /// add-in: after `setRemoteDescription` Teams expects one `track` event per
    /// transceiver describing the remote receiver/track/stream it should wire to a
    /// media element. Missing these is what made Teams close the call ~120 ms after
    /// applying the answer.
    ///
    /// The ids must be the ones Teams can resolve, exactly as the add-in reports
    /// them: the receiver and sender are the objects Teams itself created at
    /// `addTransceiver`, and the stream id is the msid the media server announced
    /// for that m-line (`mainAudio-1000`, `mainVideo-1001`, …). Teams keys the
    /// remote participant streams by that msid; with made-up stream names it never
    /// attached the remote audio to its audio element, and the media server never
    /// sent the call's audio. Only the remote stream and track objects are ours.
    fn track_events(
        &mut self,
        accepted_mids: &std::collections::HashSet<String>,
        stream_ids: &HashMap<String, String>,
    ) -> Vec<Value> {
        let states = self.engine.transceiver_states();
        let mut out = Vec::with_capacity(states.len());
        for tx in &states {
            // Fire `ontrack` ONLY for transceivers whose m-line the answer accepted.
            // Plaza rejects unused m-lines (`m=… 0 …` + `a=inactive`); firing a
            // remote-track event for a rejected mid tells Teams a receiver exists where
            // the answer says none does — an inconsistency on top of the rejected data
            // channel. libwebrtc fires one `ontrack` per accepted (receiving) m-line.
            let in_answer = tx
                .get("mid")
                .and_then(Value::as_str)
                .map(|m| accepted_mids.contains(m))
                .unwrap_or(false);
            if !in_answer {
                continue;
            }
            let tx_id = tx.get("rpcObjectId").and_then(Value::as_u64).unwrap_or(0);
            let (sender, receiver) = match self.transceiver_objects.get(&tx_id).copied() {
                Some(ids) => ids,
                None => (self.alloc_obj_id(), self.alloc_obj_id()),
            };
            // The add-in numbers the remote stream first, then its track.
            let (stream, track) = (self.alloc_obj_id(), self.alloc_obj_id());
            let kind = tx.get("kind").and_then(Value::as_str).unwrap_or("audio");
            let mid = tx.get("mid").and_then(Value::as_str).unwrap_or("0");
            let stream_name = stream_ids.get(mid).cloned().unwrap_or_else(|| {
                format!("native{}-{mid}", if kind == "video" { "Video" } else { "Audio" })
            });
            // The host draws a remote stream into whichever element Teams sets
            // this rpc id as `srcObject` on.
            self.engine.announce_remote_stream(stream, &stream_name);
            out.push(json!({
                "rpcEventArgs": {
                    "receiver": { "rpcObjectId": receiver, "kind": kind },
                    "sender": { "rpcObjectId": sender },
                    "transceiver": {
                        "rpcObjectId": tx.get("rpcObjectId"),
                        "direction": tx.get("direction"),
                        "mid": mid,
                    },
                    "track": { "rpcObjectId": track, "id": format!("00000000-0000-4000-8000-{track:012x}") },
                    "stream": { "rpcObjectId": stream, "id": stream_name },
                    "reuseReceiver": true,
                },
                "rpcEventTarget": { "rpcObjectType": "RTCPeerConnection", "rpcObjectId": self.pc_object_id },
                "rpcEventName": "track",
                "hr": 0,
            }));
        }
        out
    }

    /// The `sessioninfo` capability-handshake event (reply to `setVersionInfo`).
    ///
    /// **`webRTC` / `webRTCRedirector` must report the same versions the real add-in
    /// does** — a compatibility handshake, like a browser's User-Agent. Teams' media
    /// server (Plaza) gates *full* optimized media (the recv video grid + the SCTP
    /// data/control channel) on the redirector being a recognized recent add-in: a
    /// captured DLL-hosting call that reported `webRTC:"M133…"` + `webRTCRedirector:
    /// "3.1.2511.17007"` got audio + 9 video + data all accepted, whereas advertising
    /// our own `webrtc-rs 0.17` / `rdpio-native` strings got a degraded AUDIO-ONLY
    /// answer (video + data rejected) — identical offers, the only difference being
    /// these two version strings. (Confirmed by exhaustively matching the add-in's
    /// offer content, extensions, RPC flow and inline ICE candidates with no change;
    /// under max-BUNDLE the transport is shared across all m-lines, so the audio-vs-
    /// video/data split could only come from a per-endpoint identity signal, and this
    /// is it.) `clientAppName` is NOT gated — the working capture reported the host
    /// process name ("rdpx.exe") and still got full media — so we keep it honest.
    fn session_info_event(&self) -> Value {
        json!({
            "rpcEventArgs": {
                "display": { "width": self.display.0, "height": self.display.1 },
                "version": {
                    "clientOSName": "Windows 10 Pro",
                    "clientOSVersionDetail": "26100.1.amd64fre.ge_release.240331-1435",
                    "clientOS": "Version 2009 (OS Build 26200.8655)",
                    "clientPlatform": "Win10",
                    "clientOSProductSKU": 48,
                    "clientAppName": "rdpio.exe",
                    "clientAppVersion": "",
                    // libwebrtc + add-in versions the real MsRdcWebRTCAddIn.dll reports;
                    // Plaza grants full media only to a recognized redirector version.
                    "webRTC": "M133 (02.10.25) (cd3e295)",
                    "webRTCRedirector": "3.1.2511.17007"
                },
                "session": { "vdiMode": 0 },
                "features": session_features()
            },
            "rpcEventTarget": { "rpcObjectType": "RDWebRTCRedirector" },
            "rpcEventName": "sessioninfo",
            "hr": 0
        })
    }

    /// Relay the engine's connection-state and data-channel events, in the
    /// add-in's shapes (reference capture): `{state}` on the peer connection for
    /// state changes; `{id}` (`open`/`closing`) and `{id, base64MessageData}`
    /// (`message`) on the data channel's remoted object.
    pub fn drain_events(&mut self) -> Vec<Value> {
        use base64::Engine as _;
        let dc_event = |object_id: u64, name: &str, args: Value| {
            json!({
                "rpcEventArgs": args,
                "rpcEventTarget": { "rpcObjectType": "RTCDataChannel", "rpcObjectId": object_id },
                "rpcEventName": name,
                "hr": 0,
            })
        };
        let mut out = Vec::new();
        if self.local_offer_set {
            out.append(&mut self.held_events);
        }
        for e in self.engine.take_events() {
            let ev = match e {
                PeerEvent::State { event, state } => {
                    let ev = self.pc_state_event(event, &state);
                    if event == "icegatheringstatechange" && !self.local_offer_set {
                        self.held_events.push(ev);
                        continue;
                    }
                    ev
                }
                PeerEvent::ChannelOpen { object_id, stream_id } => {
                    tracing::info!(object_id, stream_id, "dispatch: data channel open — Teams' media control can start");
                    dc_event(object_id, "open", json!({ "id": stream_id }))
                }
                PeerEvent::ChannelClosing { object_id, stream_id } => {
                    dc_event(object_id, "closing", json!({ "id": stream_id }))
                }
                PeerEvent::ChannelMessage { object_id, stream_id, data } => dc_event(
                    object_id,
                    "message",
                    json!({
                        "id": stream_id,
                        "base64MessageData": base64::engine::general_purpose::STANDARD.encode(data),
                    }),
                ),
            };
            out.push(ev);
        }
        out
    }

    /// Emit a trickle-ICE event for each candidate gathered since the last drain.
    /// Matches the add-in's `icecandidate` event: the individual `candidate`
    /// object plus the updated local `desc`, targeting the peer connection.
    pub async fn drain_ice(&mut self) -> Vec<Value> {
        // Held in the engine until Teams has set our offer (see `local_offer_set`).
        if !self.local_offer_set {
            return Vec::new();
        }
        let candidates = self.engine.take_candidates();
        if candidates.is_empty() {
            return Vec::new();
        }
        // Report `desc` as the offer Teams set (its munged copy), NOT webrtc-rs's own
        // (codec-superset) local description — the two differ by several KB, and
        // handing Teams a `desc` that doesn't match the SDP it set makes it abort the
        // call within ~100 ms. Fall back to webrtc-rs's only if Teams never set one
        // (capture replay).
        self.trickled.extend(
            candidates
                .iter()
                .filter_map(|c| c.get("candidate").and_then(Value::as_str))
                .map(str::to_owned),
        );
        // …with our real address as its default destination, like libwebrtc's
        // updated localDescription (see `with_default_address`).
        let sdp = match &self.teams_local_offer {
            Some(s) => Some(with_default_address(s, &self.trickled)),
            None => self.engine.local_description().await,
        };
        candidates
            .into_iter()
            .map(|candidate| {
                let mut args = Map::new();
                if let Some(sdp) = &sdp {
                    args.insert("desc".into(), json!({ "sdp": sdp, "type": "offer" }));
                }
                args.insert("candidate".into(), candidate);
                json!({
                    "rpcEventArgs": Value::Object(args),
                    "rpcEventTarget": {
                        "rpcObjectType": "RTCPeerConnection",
                        "rpcObjectId": self.pc_object_id,
                    },
                    "rpcEventName": "icecandidate",
                    "hr": 0,
                })
            })
            .collect()
    }
}

/// The bytes of an `RTCDataChannel.send`: Teams passes them as a JSON array of
/// byte values (`{"data":[16,15,…]}`); a base64 or plain-string form is accepted
/// too.
fn data_channel_payload(arg: &Value) -> Vec<u8> {
    use base64::Engine as _;
    match arg.get("data") {
        Some(Value::Array(bytes)) => bytes
            .iter()
            .filter_map(Value::as_u64)
            .map(|b| b as u8)
            .collect(),
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        _ => arg
            .get("base64MessageData")
            .and_then(Value::as_str)
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .unwrap_or_default(),
    }
}

/// The add-in's stock success payload for void methods.
fn ack() -> Value {
    json!("RPC succeeded.")
}

/// The set of `mid`s the answer *accepted* — every m-line with a non-zero port that
/// isn't marked `a=inactive`. Plaza rejects unused m-lines as `m=… 0 …` (port 0) with
/// `a=inactive` and often no `a=mid` at all; those are excluded. `track_events` uses
/// this to fire `ontrack` only for receivers the answer actually established.
fn accepted_mids(answer: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut port_nonzero = false;
    let mut inactive = false;
    let mut mid: Option<String> = None;
    let commit = |port_nonzero: bool, inactive: bool, mid: &mut Option<String>, out: &mut std::collections::HashSet<String>| {
        if port_nonzero && !inactive {
            if let Some(m) = mid.take() {
                out.insert(m);
            }
        }
        *mid = None;
    };
    for line in answer.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("m=") {
            // Commit the m-line we just finished, then start the new one.
            commit(port_nonzero, inactive, &mut mid, &mut out);
            port_nonzero = rest.split_whitespace().nth(1).map(|p| p != "0").unwrap_or(false);
            inactive = false;
        } else if t == "a=inactive" {
            inactive = true;
        } else if let Some(m) = t.strip_prefix("a=mid:") {
            mid = Some(m.to_string());
        }
    }
    commit(port_nonzero, inactive, &mut mid, &mut out);
    out
}

/// Each m-line's remote stream id, by `mid`: the msid stream the answer declares
/// for it, from `a=msid:<stream> <track>` or (Plaza's form) the first
/// `a=ssrc:<n> msid:<stream> <track>`. Plaza names them after the SSRC —
/// `mainAudio-1000`, `mainVideo-1001`, … — and Teams looks its remote streams up
/// by that name.
fn remote_stream_ids(answer: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut mid: Option<String> = None;
    let mut stream: Option<String> = None;
    let mut commit = |mid: &mut Option<String>, stream: &mut Option<String>| {
        if let (Some(m), Some(s)) = (mid.take(), stream.take()) {
            out.insert(m, s);
        }
        *mid = None;
        *stream = None;
    };
    for line in answer.lines() {
        let t = line.trim();
        if t.starts_with("m=") {
            commit(&mut mid, &mut stream);
        } else if let Some(m) = t.strip_prefix("a=mid:") {
            mid = Some(m.to_string());
        } else if stream.is_none() {
            let msid = t.strip_prefix("a=msid:").or_else(|| {
                t.strip_prefix("a=ssrc:")
                    .and_then(|rest| rest.split_once(' '))
                    .and_then(|(_, attr)| attr.strip_prefix("msid:"))
            });
            if let Some(s) = msid.and_then(|v| v.split_whitespace().next()) {
                if s != "-" {
                    stream = Some(s.to_string());
                }
            }
        }
    }
    commit(&mut mid, &mut stream);
    out
}

/// Make a remote answer/offer parseable by webrtc-rs's strict SDP layer. Teams'
/// media server rejects unused m-lines by zeroing their port and listing a bare
/// dynamic payload type with NO `a=rtpmap` — e.g. an audio-only call answers every
/// video m-line as `m=video 0 UDP/TLS/RTP/SAVPF 36` + `a=inactive`. webrtc-rs's
/// `codecs_from_media_description` calls `get_codec_for_payload_type` for every
/// listed format and tolerates only payload 0; any other unmapped PT aborts the
/// whole `setRemoteDescription` with "payload type not found", tearing the
/// just-answered call down. libwebrtc (the add-in) simply ignores rejected m-lines.
/// We recover the same way: inject a synthetic `a=rtpmap` for any dynamic PT (> 34)
/// a media line lists without one, so parsing completes; the m-line stays rejected.
fn sanitize_remote_sdp(sdp: &str) -> String {
    // Process one block at a time (session preamble + each m= section).
    let mut out = String::with_capacity(sdp.len() + 64);
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in sdp.split_inclusive('\n') {
        if line.trim_start().starts_with("m=") && !cur.is_empty() {
            blocks.push(std::mem::take(&mut cur));
        }
        cur.push(line);
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    for block in &blocks {
        // RTX (retransmission) payload types Teams' answer carries because our
        // *enriched* offer advertised them (see `enrich_offer`), but which webrtc-rs's
        // own RTX-free local offer never declared — so its strict SDP layer would abort
        // `set_remote_description` with "payload type not found" on them. Strip the RTX
        // codec here: the base H264 stream (the actual video) rides the primary payload
        // type and stays, and webrtc-rs simply won't consume Plaza's retransmissions
        // (`a=ssrc-group:FID` is left intact — webrtc-rs tolerates a repair flow whose
        // codec it can't resolve). Also drop the `a=rid`/`a=simulcast` lines the answer
        // uses to describe a layered send we never offered.
        let rtx_pts: std::collections::HashSet<&str> = block
            .iter()
            .filter_map(|l| {
                let mut it = l.trim().strip_prefix("a=rtpmap:")?.split_whitespace();
                let pt = it.next()?;
                it.next()?.starts_with("rtx/").then_some(pt)
            })
            .collect();

        // The kept lines (trimmed, RTX/rid/simulcast removed, m= format list pruned).
        let mut kept: Vec<String> = Vec::new();
        for l in block {
            let t = l.trim();
            if t.starts_with("a=rid:") || t.starts_with("a=simulcast:") {
                continue;
            }
            if let Some(pt) = t.strip_prefix("a=rtpmap:").and_then(|r| r.split_whitespace().next()) {
                if rtx_pts.contains(pt) {
                    continue;
                }
            }
            if let Some(pt) = t.strip_prefix("a=fmtp:").and_then(|r| r.split_whitespace().next()) {
                if rtx_pts.contains(pt) {
                    continue;
                }
            }
            if t.starts_with("m=") && !rtx_pts.is_empty() {
                // Drop the RTX payload types from the format list (keep kind/port/proto).
                let rebuilt: Vec<&str> = t
                    .split_whitespace()
                    .enumerate()
                    .filter(|(i, tok)| *i < 3 || !rtx_pts.contains(tok))
                    .map(|(_, tok)| tok)
                    .collect();
                kept.push(rebuilt.join(" "));
                continue;
            }
            kept.push(t.to_string());
        }
        // An RTP m-section without a direction attribute is sendrecv (RFC 4566
        // §6); Plaza's audio answer is written that way. webrtc-rs instead treats
        // it as "unspecified": it never sets the audio transceiver's negotiated
        // direction, and on the next createOffer (Teams renegotiates the moment
        // the camera turns on) it skips that m-section and appends audio *last* —
        // an m-line reorder Teams answers by closing the call. State the default.
        let is_rtp_media = kept
            .first()
            .is_some_and(|m| m.starts_with("m=audio") || m.starts_with("m=video"));
        let has_direction = kept.iter().any(|l| {
            matches!(l.as_str(), "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive")
        });
        if is_rtp_media && !has_direction {
            kept.push("a=sendrecv".to_string());
        }
        for line in &kept {
            out.push_str(line);
            out.push_str("\r\n");
        }

        // Recover rejected m-lines: Teams zeroes an unused m-line's port and lists a
        // bare dynamic payload type with NO `a=rtpmap` (e.g. `m=video 0 … 36` +
        // `a=inactive`). webrtc-rs's `get_codec_for_payload_type` tolerates only static
        // PT 0; any other unmapped PT aborts the whole `set_remote_description`. Inject a
        // synthetic `a=rtpmap` for any dynamic PT a media line lists without one, so
        // parsing completes; the m-line stays rejected. libwebrtc ignores such lines.
        let Some(mline) = kept.first().map(|s| s.as_str()) else {
            continue;
        };
        let Some(rest) = mline.strip_prefix("m=") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let media = fields.next().unwrap_or("");
        let default = match media {
            "video" => "H264/90000",
            "audio" => "opus/48000/2",
            _ => continue,
        };
        let pts: Vec<&str> = fields.skip(2).collect();
        let mapped: std::collections::HashSet<&str> = kept
            .iter()
            .filter_map(|l| l.strip_prefix("a=rtpmap:").and_then(|r| r.split_whitespace().next()))
            .collect();
        for pt in pts {
            let dynamic = pt.parse::<u16>().map(|n| n > 34).unwrap_or(false);
            if dynamic && !mapped.contains(pt) {
                out.push_str(&format!("a=rtpmap:{pt} {default}\r\n"));
                // For video, give the synthetic codec our camera send track's exact H.264
                // fmtp. When Plaza rejects the camera's video m-line (port 0), an attached
                // send track still has to BIND against that m-line's codec at
                // set_remote_description; a bare `H264/90000` with no fmtp doesn't match
                // the track's profile, so webrtc-rs aborts with "codec is not supported by
                // remote". Matching the fmtp lets the bind succeed (the m-line stays
                // rejected/inactive — nothing actually sends).
                if media == "video" {
                    out.push_str(&format!(
                        "a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n"
                    ));
                }
            }
        }
    }
    out
}

/// Rewrite webrtc-rs's offer into the shape Teams' media server expects (matching
/// the real add-in's offer), applied only to the copy we SEND Teams. Two fixes:
///
/// 1. **Add** the attributes webrtc-rs omits: `a=ice-options:trickle` (we DO
///    trickle — without it the server treats our candidate-less initial offer as
///    final ICE), `a=msid-semantic: WMS` (the media-stream semantic line), and
///    `a=max-message-size` on the SCTP "main-channel".
/// 2. **Strip** the send-track markers (`a=msid` / `a=ssrc…`) off any m-line that
///    ended up **recv-only** (or inactive). Teams adds every transceiver inactive
///    then flips most to recvonly, but webrtc-rs (having created them sendrecv to
///    give the sendrecv ones an msid) leaves a phantom msid/ssrc on the recvonly
///    ones — a send marker on a receive-only line, which Teams' media server
///    rejects (it closes the peer connection without ever answering).
///
/// 3. **Copy the DTLS fingerprint (+ `a=ice-options:trickle`) onto every m-line.**
///    webrtc-rs emits `a=fingerprint` once at session level; the real add-in repeats
///    it per m-line. Teams' media server needs the fingerprint on the `m=application`
///    line to accept the SCTP data channel — without it the server rejects the data
///    channel (port 0), and since Teams tears the peer connection down when its
///    "main-channel" data channel isn't negotiated, the call dies ~110 ms after the
///    answer and Teams retries in a loop.
///
/// 4. **Pair every H264 payload type with an RTX (retransmission) codec.** Teams'
///    media server (Plaza) rejects any video m-line that advertises `nack` — which
///    webrtc-rs's default interceptors always do — but offers no matching
///    `a=rtpmap:<pt> rtx/90000` + `a=fmtp:<pt> apt=<h264pt>` to actually carry the
///    retransmissions. The captured real add-in offer pairs RTX with H264 on *every*
///    video m-line (send and the recv grid alike); ours had none, so Plaza rejected
///    all our video (and the bundled data channel died with it). Under max-BUNDLE all
///    m-lines share one payload-type space, so one H264→RTX map is reused across them.
///    `sanitize_remote_sdp` strips the RTX payload type back out of Teams' answer, so
///    webrtc-rs — whose own (un-enriched) offer has no RTX — still parses it.
///
/// All of this is session-level / per-m-line metadata that doesn't change the DTLS/
/// ICE identity or the *primary* codecs, so `setLocalDescription` still applies
/// webrtc-rs's exact original and Teams' answer still matches it.
fn enrich_offer(sdp: &str) -> String {
    // Split into the session preamble + each m= section, preserving line endings.
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in sdp.split_inclusive('\n') {
        if line.trim_start().starts_with("m=") && !cur.is_empty() {
            blocks.push(std::mem::take(&mut cur));
        }
        cur.push(line);
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    // The DTLS fingerprint webrtc-rs puts once at session level; we repeat it on each
    // m-line (fix 3). Captured up front so each block can splice it.
    let session_fingerprint = sdp.lines().find_map(|l| {
        let t = l.trim();
        t.starts_with("a=fingerprint:").then(|| t.to_string())
    });

    // Build the bundle-wide H264 → RTX payload-type map (fix 4). Collect every payload
    // type already present so allocated RTX PTs never collide, then pair each distinct
    // H264 PT with a fresh one. Skip an H264 PT that already has an RTX codec pointing
    // at it (`apt=<pt>`), so a second pass over our own output is idempotent.
    let mut used_pts: std::collections::HashSet<u16> = std::collections::HashSet::new();
    for l in sdp.lines() {
        let t = l.trim();
        if let Some(pt) = t
            .strip_prefix("a=rtpmap:")
            .and_then(|r| r.split_whitespace().next())
            .and_then(|p| p.parse::<u16>().ok())
        {
            used_pts.insert(pt);
        }
        if let Some(rest) = t.strip_prefix("m=") {
            // m=<kind> <port> <proto> <pt>...
            for tok in rest.split_whitespace().skip(3) {
                if let Ok(pt) = tok.parse::<u16>() {
                    used_pts.insert(pt);
                }
            }
        }
    }
    let already_paired: std::collections::HashSet<u16> = sdp
        .lines()
        .filter_map(|l| {
            let params = l.trim().strip_prefix("a=fmtp:")?.split_whitespace().nth(1)?;
            params.strip_prefix("apt=")?.parse::<u16>().ok()
        })
        .collect();
    let mut rtx_map: Vec<(u16, u16)> = Vec::new(); // (video_pt, rtx_pt), bundle-wide
    for l in sdp.lines() {
        let t = l.trim();
        let Some(r) = t.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let mut it = r.split_whitespace();
        let (Some(pt), Some(codec)) = (it.next().and_then(|p| p.parse::<u16>().ok()), it.next()) else {
            continue;
        };
        // Pair RTX with every *primary* video codec (not just H264): webrtc-rs offers
        // VP8/VP9/AV1/HEVC too, each with `nack`, so each needs a matching RTX or Plaza
        // could still reject the m-line. Skip the codecs that ARE retransmission/FEC
        // (rtx/red/ulpfec/flexfec) — pairing RTX with RTX is meaningless.
        if is_primary_video_codec(codec)
            && !already_paired.contains(&pt)
            && !rtx_map.iter().any(|&(h, _)| h == pt)
        {
            if let Some(rtx) = alloc_free_pt(&mut used_pts) {
                rtx_map.push((pt, rtx));
            }
        }
    }

    let mut out = String::with_capacity(sdp.len() + 512);
    for block in &blocks {
        let recv_only = block.iter().any(|l| {
            let t = l.trim_end();
            t == "a=recvonly" || t == "a=inactive"
        });
        // The SCTP data-channel section is not an RTP media line and carries no
        // direction; webrtc-rs still emits `a=sendrecv` on it, but the real add-in
        // (libwebrtc) omits it, so strip the stray direction to match.
        let is_application = block
            .first()
            .map(|l| l.trim_start().starts_with("m=application"))
            .unwrap_or(false);
        let is_media = block.first().map(|l| l.trim_start().starts_with("m=")).unwrap_or(false);
        let is_video = block.first().map(|l| l.trim_start().starts_with("m=video")).unwrap_or(false);
        let has_fingerprint = block.iter().any(|l| l.trim_start().starts_with("a=fingerprint:"));
        let has_ice_options = block.iter().any(|l| l.trim_start().starts_with("a=ice-options:"));

        // The RTX pairings that apply to THIS video block (those whose H264 PT it lists).
        let block_rtx: Vec<(u16, u16)> = if is_video {
            let pts: std::collections::HashSet<u16> = block
                .iter()
                .filter_map(|l| {
                    l.trim()
                        .strip_prefix("a=rtpmap:")
                        .and_then(|r| r.split_whitespace().next())
                        .and_then(|p| p.parse::<u16>().ok())
                })
                .collect();
            rtx_map.iter().copied().filter(|&(h, _)| pts.contains(&h)).collect()
        } else {
            Vec::new()
        };

        for (i, l) in block.iter().enumerate() {
            let t = l.trim_end_matches(['\r', '\n']);
            if recv_only && (t.starts_with("a=msid:") || t.starts_with("a=ssrc")) {
                continue;
            }
            if is_application
                && (t == "a=sendrecv" || t == "a=recvonly" || t == "a=sendonly" || t == "a=inactive")
            {
                continue;
            }
            // Rewrite the video m= line to append the RTX payload types (fix 4).
            if i == 0 && !block_rtx.is_empty() {
                out.push_str(t);
                for &(_, rtx) in &block_rtx {
                    out.push(' ');
                    out.push_str(&rtx.to_string());
                }
                out.push_str("\r\n");
                continue;
            }
            out.push_str(l);
            if t.starts_with("a=group:BUNDLE") {
                out.push_str("a=ice-options:trickle\r\n");
                out.push_str("a=msid-semantic: WMS\r\n");
            } else if t.starts_with("a=sctp-port:") {
                out.push_str("a=max-message-size:262144\r\n");
            } else if is_media && t.starts_with("a=mid:") {
                // Give every m-line its own DTLS fingerprint + ice-options, like the
                // add-in — required for the media server to accept the data channel.
                if !has_fingerprint {
                    if let Some(fp) = &session_fingerprint {
                        out.push_str(fp);
                        out.push_str("\r\n");
                    }
                }
                if !has_ice_options {
                    out.push_str("a=ice-options:trickle\r\n");
                }
                // Declare the RTX codecs (fix 4) at m-line scope — a=mid always follows
                // the c= line, so appending a= attributes here is valid SDP ordering.
                for &(h264, rtx) in &block_rtx {
                    out.push_str(&format!("a=rtpmap:{rtx} rtx/90000\r\n"));
                    out.push_str(&format!("a=fmtp:{rtx} apt={h264}\r\n"));
                }
            }
        }
    }
    out
}

/// Allocate a payload type not present in `used`, marking it used. Draws from the
/// dynamic range (96..=127 by convention, then 35..=95 as spillover) so it never
/// collides with a static PT. `None` only if the whole dynamic range is exhausted.
fn alloc_free_pt(used: &mut std::collections::HashSet<u16>) -> Option<u16> {
    (96u16..=127).chain(35u16..=95).find(|pt| used.insert(*pt))
}

/// Whether an `a=rtpmap` codec token (e.g. `H264/90000`) names a primary video codec
/// — one that carries actual media and gets a paired RTX — as opposed to a
/// retransmission/FEC codec (`rtx`, `red`, `ulpfec`, `flexfec`) that must not.
fn is_primary_video_codec(codec: &str) -> bool {
    matches!(
        codec.split('/').next().unwrap_or(""),
        "H264" | "VP8" | "VP9" | "AV1" | "H265" | "HEVC"
    )
}

/// Trim each video m-line to what the real add-in offers — **H264 (+ its RTX) only** —
/// and drop webrtc-rs's **duplicate** `rtcp-fb` lines. webrtc-rs offers a noisy video
/// codec set (VP8/VP9/AV1/H265/ulpfec + high-profile H264) and emits each `nack` /
/// `nack pli` TWICE per codec (the codec's own feedback PLUS the interceptor's); the
/// add-in offers a clean H264-only block with a single feedback set per codec. Teams'
/// media server (Plaza) accepts the add-in's video (and the bundled data channel) but
/// gives us an audio-only answer even though every other signal matches — a strict SFU
/// rejects a malformed / over-broad video m-line and degrades the whole offer to
/// audio-only, taking the (byte-identical, clean) data channel down with it. Applied
/// only to the copy relayed to Plaza; webrtc-rs's own offer keeps every codec, and
/// Plaza's answer picks H264 (present in both), so the answer still applies to the
/// original. Non-video blocks pass through untouched.
fn slim_video_offer(sdp: &str) -> String {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in sdp.split_inclusive('\n') {
        if line.trim_start().starts_with("m=") && !cur.is_empty() {
            blocks.push(std::mem::take(&mut cur));
        }
        cur.push(line);
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    let mut out = String::with_capacity(sdp.len());
    for block in &blocks {
        let is_video = block.first().map(|l| l.trim_start().starts_with("m=video")).unwrap_or(false);
        if !is_video {
            for l in block {
                out.push_str(l);
            }
            continue;
        }
        // Keep set: H264 payload types...
        let mut keep: std::collections::HashSet<u16> = std::collections::HashSet::new();
        for l in block {
            let t = l.trim();
            if let Some(r) = t.strip_prefix("a=rtpmap:") {
                let mut it = r.split_whitespace();
                if let (Some(pt), Some(codec)) =
                    (it.next().and_then(|p| p.parse::<u16>().ok()), it.next())
                {
                    if codec.starts_with("H264/") {
                        keep.insert(pt);
                    }
                }
            }
        }
        // ...plus the RTX payload types whose `apt=` points at a kept H264 PT.
        for l in block {
            let t = l.trim();
            if let Some(r) = t.strip_prefix("a=fmtp:") {
                let mut it = r.split_whitespace();
                if let (Some(pt), Some(params)) =
                    (it.next().and_then(|p| p.parse::<u16>().ok()), it.next())
                {
                    if let Some(apt) = params.strip_prefix("apt=").and_then(|a| a.parse::<u16>().ok()) {
                        if keep.contains(&apt) {
                            keep.insert(pt);
                        }
                    }
                }
            }
        }

        let mut seen_fb: std::collections::HashSet<String> = std::collections::HashSet::new();
        for l in block {
            let t = l.trim_end_matches(['\r', '\n']);
            // Rewrite the m= line to keep kind/port/proto + only the kept payload types.
            if t.starts_with("m=video") {
                let toks: Vec<&str> = t.split_whitespace().collect();
                let head = toks.len().min(3);
                let mut rebuilt = toks[..head].join(" ");
                for pt in &toks[head..] {
                    if pt.parse::<u16>().map(|n| keep.contains(&n)).unwrap_or(false) {
                        rebuilt.push(' ');
                        rebuilt.push_str(pt);
                    }
                }
                out.push_str(&rebuilt);
                out.push_str("\r\n");
                continue;
            }
            // Drop rtpmap/fmtp for payload types we're removing.
            let codec_pt = t
                .strip_prefix("a=rtpmap:")
                .or_else(|| t.strip_prefix("a=fmtp:"))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|p| p.parse::<u16>().ok());
            if let Some(pt) = codec_pt {
                if !keep.contains(&pt) {
                    continue;
                }
            }
            // rtcp-fb: drop for removed PTs, and dedupe (webrtc-rs emits nack/pli twice).
            if let Some(rest) = t.strip_prefix("a=rtcp-fb:") {
                if let Ok(pt) = rest.split_whitespace().next().unwrap_or("").parse::<u16>() {
                    if !keep.contains(&pt) {
                        continue;
                    }
                }
                let norm = t.split_whitespace().collect::<Vec<_>>().join(" ");
                if !seen_fb.insert(norm.clone()) {
                    continue;
                }
                out.push_str(&norm);
                out.push_str("\r\n");
                continue;
            }
            out.push_str(l);
        }
    }
    out
}

/// The capture device a `createMediaStream` call's constraints name: audio
/// `deviceId` (a string or `{exact: …}`), or video `mandatory.sourceId` /
/// `deviceId`.
fn stream_device(args: &Value) -> Option<String> {
    let c = args.get("constraints")?;
    let pick = |v: Option<&Value>| -> Option<String> {
        match v? {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("exact").or_else(|| o.get("ideal")).and_then(Value::as_str).map(str::to_owned),
            _ => None,
        }
    };
    pick(c.pointer("/audio/deviceId"))
        .or_else(|| pick(c.pointer("/video/mandatory/sourceId")))
        .or_else(|| pick(c.pointer("/video/deviceId")))
}

/// Put our real transport address on the offer, as libwebrtc does.
///
/// webrtc-rs always writes the bundle m-line as `m=audio 9 …` / `c=IN IP4
/// 0.0.0.0` and never updates it, even with every candidate gathered. libwebrtc
/// (the add-in) rewrites the first m-section's port and `c=` line to its
/// *default candidate* as candidates arrive — reference capture: `62088/10.2.2.20`
/// (host) → `…/203.0.113.5` (srflx) → `52217/52.115.154.49` (relay). Teams hands
/// that SDP to its media server, and to a SIP-lineage media stack `c=0.0.0.0` is
/// the RFC 2543 way of saying "on hold, send me nothing": the server took our
/// audio (it flagged us as the active speaker) yet never sent the call's audio
/// back.
///
/// The default candidate follows libwebrtc's `UpdateConnectionAddress`: RTP
/// component, UDP only, relay over srflx/prflx over host, IPv4 over IPv6.
/// `extra` are candidate lines gathered after the SDP was made (trickled); any
/// not already present are added to the first m-section.
fn with_default_address(sdp: &str, extra: &[String]) -> String {
    let lines: Vec<&str> = sdp.split("\r\n").flat_map(|l| l.split('\n')).collect();
    let Some(first_m) = lines.iter().position(|l| l.starts_with("m=")) else {
        return sdp.to_string();
    };
    let section_end = lines[first_m + 1..]
        .iter()
        .position(|l| l.starts_with("m="))
        .map(|i| first_m + 1 + i)
        .unwrap_or(lines.len());
    let present: Vec<&str> = lines[first_m..section_end]
        .iter()
        .filter_map(|l| l.strip_prefix("a="))
        .filter(|l| l.starts_with("candidate:"))
        .collect();
    let missing: Vec<&str> = extra
        .iter()
        .map(|c| c.strip_prefix("a=").unwrap_or(c))
        .filter(|c| c.starts_with("candidate:") && !present.contains(c))
        .collect();

    // (preference, is_ipv4, ip, port) of the best candidate so far.
    let mut best: Option<(u8, bool, String, u16)> = None;
    for cand in present.iter().chain(missing.iter()) {
        // candidate:<foundation> <component> <transport> <priority> <ip> <port> typ <type> …
        let f: Vec<&str> = cand.split_whitespace().collect();
        if f.len() < 8 || f[1] != "1" || !f[2].eq_ignore_ascii_case("udp") || f[6] != "typ" {
            continue;
        }
        let Ok(port) = f[5].parse::<u16>() else { continue };
        let preference = match f[7] {
            "relay" => 3,
            "srflx" | "prflx" => 2,
            "host" => 1,
            _ => continue,
        };
        let ipv4 = !f[4].contains(':');
        let replace = match &best {
            None => true,
            Some((p, v4, _, _)) => {
                if *v4 == ipv4 {
                    preference > *p
                } else {
                    ipv4 // an IPv4 candidate beats any IPv6 one
                }
            }
        };
        if replace {
            best = Some((preference, ipv4, f[4].to_string(), port));
        }
    }
    let Some((_, ipv4, ip, port)) = best else {
        return sdp.to_string();
    };

    // Trickled candidates join the others: before `a=end-of-candidates`, else at
    // the end of the first m-section (before a trailing empty line).
    let insert_at = lines[first_m..section_end]
        .iter()
        .position(|l| *l == "a=end-of-candidates")
        .map(|i| first_m + i)
        .unwrap_or_else(|| {
            if section_end == lines.len() && lines.last() == Some(&"") {
                section_end - 1
            } else {
                section_end
            }
        });

    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + missing.len());
    for (i, line) in lines.iter().enumerate() {
        if i == insert_at {
            out.extend(missing.iter().map(|c| format!("a={c}")));
        }
        let in_first = i >= first_m && i < section_end;
        if i == first_m {
            let f: Vec<&str> = line.splitn(3, ' ').collect();
            if f.len() == 3 {
                out.push(format!("{} {port} {}", f[0], f[2]));
                continue;
            }
        } else if in_first && line.starts_with("c=") {
            out.push(format!("c=IN {} {ip}", if ipv4 { "IP4" } else { "IP6" }));
            continue;
        }
        out.push(line.to_string());
    }
    if insert_at == lines.len() {
        out.extend(missing.iter().map(|c| format!("a={c}")));
    }
    out.join(eol)
}

/// Whether two SDPs belong to the same peer connection, by their DTLS fingerprint.
///
/// The fingerprint is generated per peer connection and is *not* touched by SDP
/// munging (which only rewrites codec/m-line content), so it reliably tells "the
/// server's edited copy of the offer we just made" apart from an SDP belonging to
/// some other session — which is exactly what a capture replay feeds us.
fn same_dtls_session(ours: &str, theirs: &str) -> bool {
    let fingerprint = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("a=fingerprint:"))
            .map(|l| l.trim().to_owned())
    };
    match (fingerprint(ours), fingerprint(theirs)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Parse the static capability object (constant is validated by the unit test).
fn capabilities() -> Value {
    serde_json::from_str(CAPABILITIES_JSON).expect("capabilities JSON is valid")
}

/// Parse the static feature set.
fn session_features() -> Value {
    serde_json::from_str(SESSION_FEATURES_JSON).expect("session features JSON is valid")
}

/// Build a reply matching the add-in's envelope: optional `result`, echoed
/// `rpcObjectType` / `rpcObjectId` / `rpcName`, the `rpcCallId`, and an `hr`.
fn reply(msg: &RpcMessage, cid: u64, result: Option<Value>, hr: i64) -> Value {
    let mut m = Map::new();
    if let Some(r) = result {
        m.insert("result".into(), r);
    }
    if let Some(t) = &msg.object_type {
        m.insert("rpcObjectType".into(), Value::String(t.clone()));
    }
    if let Some(oid) = &msg.object_id {
        m.insert("rpcObjectId".into(), oid.clone());
    }
    if let Some(n) = &msg.name {
        m.insert("rpcName".into(), Value::String(n.clone()));
    }
    m.insert("rpcCallId".into(), Value::from(cid));
    m.insert("hr".into(), Value::from(hr));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_json_constants_are_valid() {
        // These are parsed via `.expect(...)` at runtime; fail fast in CI instead.
        let caps = capabilities();
        assert!(caps.get("sendCapabilities").is_some());
        assert!(caps.get("recvCapabilities").is_some());
        let feats = session_features();
        assert_eq!(feats.get("unifiedplan").and_then(Value::as_str), Some("enabled"));
    }

    #[test]
    fn session_info_event_has_the_handshake_shape() {
        let r = Redirector::new();
        let ev = r.session_info_event();
        assert_eq!(ev.get("rpcEventName").and_then(Value::as_str), Some("sessioninfo"));
        assert_eq!(ev.get("hr").and_then(Value::as_i64), Some(0));
        assert!(ev.pointer("/rpcEventArgs/features/unifiedplan").is_some());
        assert!(ev.pointer("/rpcEventArgs/display/width").is_some());
    }

    /// The first answer Teams' media server fully accepted (audio + 9 video +
    /// data, captured live): every payload type it will send audio with must be
    /// negotiated, or webrtc-rs discards the inbound audio track at its first
    /// packet ("Could not determine PayloadType … codec not found").
    // webrtc-rs SDP workarounds / answers made for its offers.
    #[cfg(not(feature = "libwebrtc"))]
    #[tokio::test]
    async fn full_media_answer_negotiates_every_audio_payload_type() {
        let answer = include_str!("../tests/fixtures/full_media_answer.sdp");
        let mut r = Redirector::new();
        let call = |json: String| RpcMessage::parse(json.as_bytes()).unwrap();
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createPeerConnection","rpcArgs":[{"iceServers":[]}],"rpcCallId":1}"#.into())).await;
        let out = r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createDataChannel","rpcArgs":[{"label":"main-channel","rpcObjectId":10}],"rpcCallId":2}"#.into())).await;
        // Like the add-in: `negotiationneeded` once, ahead of the reply.
        assert_eq!(out[0].get("rpcEventName").and_then(Value::as_str), Some("negotiationneeded"));
        assert_eq!(out[1].get("rpcCallId").and_then(Value::as_u64), Some(2));
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{"kind":"audio","direction":"inactive","transceiverRpcObjectId":11,"senderRpcObjectId":12,"receiverRpcObjectId":13}],"rpcCallId":3}"#.into())).await;
        r.handle(&call(r#"{"rpcObjectType":"RTCRtpTransceiver","rpcObjectId":11,"rpcName":"setDirection","rpcArgs":[{"direction":"sendrecv"}],"rpcCallId":4}"#.into())).await;
        for i in 0..9u64 {
            let (id, cid) = (20 + 3 * i, 5 + i);
            let (sender, receiver) = (id + 1, id + 2);
            r.handle(&call(format!(
                r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{{"kind":"video","direction":"recvonly","transceiverRpcObjectId":{id},"senderRpcObjectId":{sender},"receiverRpcObjectId":{receiver}}}],"rpcCallId":{cid}}}"#
            ))).await;
        }
        let out = r
            .handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createOffer","rpcArgs":[{}],"rpcCallId":20}"#.into()))
            .await;
        let offer = out[0].pointer("/result/desc/sdp").and_then(Value::as_str).expect("offer").to_string();
        // No candidate or gathering-complete event before Teams sets the offer.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(r.drain_ice().await.is_empty(), "candidates must wait for setLocalDescription");
        assert!(
            !r.drain_events().iter().any(|e| e.get("rpcEventName").and_then(Value::as_str) == Some("icegatheringstatechange")),
            "gathering state must wait for setLocalDescription"
        );
        r.handle(&call(format!(
            r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"setLocalDescription","rpcArgs":[{{"type":"offer","sdp":{}}}],"rpcCallId":21}}"#,
            serde_json::to_string(&offer).unwrap()
        ))).await;
        let out = r
            .handle(&call(format!(
                r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"setRemoteDescription","rpcArgs":[{{"type":"answer","sdp":{}}}],"rpcCallId":22}}"#,
                serde_json::to_string(answer).unwrap()
            )))
            .await;
        assert_eq!(out[0].get("hr").and_then(Value::as_i64), Some(0), "answer must apply: {:?}", out[0]);
        // Held candidates follow once the offer is set.
        assert!(!r.drain_ice().await.is_empty(), "held candidates must be released");

        // Each track event names Teams' own receiver/sender and the stream msid
        // the answer declared — as the add-in reports them (reference capture:
        // receiver 15 / sender 14 / stream "mainAudio-1000" for transceiver 13).
        let track = |mid: &str| {
            out.iter()
                .find(|m| {
                    m.get("rpcEventName").and_then(Value::as_str) == Some("track")
                        && m.pointer("/rpcEventArgs/transceiver/mid").and_then(Value::as_str) == Some(mid)
                })
                .unwrap_or_else(|| panic!("no track event for mid {mid}: {out:#?}"))
                .get("rpcEventArgs")
                .cloned()
                .unwrap()
        };
        let audio_track = track("0");
        assert_eq!(audio_track.pointer("/stream/id").and_then(Value::as_str), Some("mainAudio-1000"));
        assert_eq!(audio_track.pointer("/receiver/rpcObjectId").and_then(Value::as_u64), Some(13));
        assert_eq!(audio_track.pointer("/sender/rpcObjectId").and_then(Value::as_u64), Some(12));
        assert_eq!(audio_track.pointer("/transceiver/rpcObjectId").and_then(Value::as_u64), Some(11));
        let video_track = track("1");
        assert_eq!(video_track.pointer("/stream/id").and_then(Value::as_str), Some("mainVideo-1001"));
        assert_eq!(video_track.pointer("/receiver/rpcObjectId").and_then(Value::as_u64), Some(22));
        assert_eq!(track("9").pointer("/stream/id").and_then(Value::as_str), Some("applicationsharingVideo-1801"));

        // Teams renegotiates as soon as the camera turns on: the next offer must
        // keep the m-line order (audio first) — the answer's direction-less audio
        // section made webrtc-rs move it last, and Teams dropped the call.
        let reoffer = r
            .handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createOffer","rpcArgs":[{}],"rpcCallId":30}"#.into()))
            .await;
        let reoffer = reoffer[0].pointer("/result/desc/sdp").and_then(Value::as_str).expect("re-offer").to_string();
        let mids: Vec<&str> = reoffer.lines().filter_map(|l| l.strip_prefix("a=mid:")).collect();
        assert_eq!(mids, ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"], "re-offer reordered m-lines");
        assert!(reoffer.lines().any(|l| l.starts_with("a=group:BUNDLE 0 1 ")), "bundle group reordered");

        let audio = r.engine.negotiated_codecs("audio").await;
        eprintln!("negotiated audio: {audio:?}");
        eprintln!("negotiated video: {:?}", r.engine.negotiated_codecs("video").await);
        for pt in [111u8, 9, 0, 8, 120, 13] {
            assert!(audio.iter().any(|(p, _)| *p == pt), "audio PT {pt} not negotiated: {audio:?}");
        }
    }

    /// Our real address goes on the bundle m-line like libwebrtc's default
    /// candidate (UDP, relay > srflx > host, IPv4 first) — never `0.0.0.0`, which a
    /// SIP-lineage media server reads as "on hold".
    #[test]
    fn offer_carries_the_default_candidate_address() {
        let sdp = [
            "v=0",
            "o=- 1 2 IN IP4 0.0.0.0",
            "a=group:BUNDLE 0 1",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
            "c=IN IP4 0.0.0.0",
            "a=mid:0",
            "a=candidate:1 1 udp 2130706431 10.0.0.144 51436 typ host",
            "a=candidate:1 2 udp 2130706431 10.0.0.144 51436 typ host",
            "a=candidate:2 1 udp 1694498815 76.27.131.205 51431 typ srflx raddr 0.0.0.0 rport 51431",
            "a=candidate:3 1 udp 16777215 52.115.170.224 58746 typ relay raddr 0.0.0.0 rport 51430",
            "a=candidate:4 1 tcp 16777000 52.115.170.225 443 typ relay raddr 0.0.0.0 rport 51430",
            "a=candidate:5 1 udp 16777215 2603:1063::5 3478 typ relay raddr :: rport 0",
            "a=end-of-candidates",
            "m=video 9 UDP/TLS/RTP/SAVPF 107",
            "c=IN IP4 0.0.0.0",
            "a=mid:1",
            "",
        ]
        .join("\r\n");
        let out = with_default_address(&sdp, &[]);
        let lines: Vec<&str> = out.split("\r\n").collect();
        assert_eq!(lines[3], "m=audio 58746 UDP/TLS/RTP/SAVPF 111", "UDP IPv4 relay wins:\n{out}");
        assert_eq!(lines[4], "c=IN IP4 52.115.170.224");
        // Only the bundle (first) m-section changes.
        assert!(out.contains("m=video 9 UDP/TLS/RTP/SAVPF 107\r\nc=IN IP4 0.0.0.0\r\n"));
        assert!(out.ends_with("a=mid:1\r\n"), "trailing CRLF kept:\n{out:?}");

        // Trickled candidates are added (before end-of-candidates) and count.
        let host_only = sdp
            .lines()
            .filter(|l| !l.contains("typ srflx") && !l.contains("typ relay"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let extra = vec!["candidate:9 1 udp 1694498815 76.27.131.205 51431 typ srflx raddr 0.0.0.0 rport 51431".to_string()];
        let out = with_default_address(&host_only, &extra);
        assert!(out.contains("m=audio 51431 "), "trickled srflx beats host:\n{out}");
        assert!(out.contains("c=IN IP4 76.27.131.205"));
        assert!(
            out.contains("typ srflx raddr 0.0.0.0 rport 51431\r\na=end-of-candidates"),
            "trickled candidate inserted before end-of-candidates:\n{out}"
        );
        // Re-applying adds nothing twice.
        assert_eq!(with_default_address(&out, &extra), out);

        // No candidates → untouched.
        let bare = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n";
        assert_eq!(with_default_address(bare, &[]), bare);
    }

    /// The mic/camera device comes from the stream's constraints, in each form
    /// Teams uses (reference capture: `{"audio":{"deviceId":"rdpio-audioinput-3"}}`,
    /// `{"video":{"mandatory":{"sourceId":"rdpio-videoinput-1",…}}}`).
    #[test]
    fn capture_device_is_read_from_stream_constraints() {
        let audio = json!({ "id": "s", "constraints": { "audio": { "deviceId": "rdpio-audioinput-3" } } });
        assert_eq!(stream_device(&audio).as_deref(), Some("rdpio-audioinput-3"));
        let exact = json!({ "constraints": { "audio": { "deviceId": { "exact": "rdpio-audioinput-1" } } } });
        assert_eq!(stream_device(&exact).as_deref(), Some("rdpio-audioinput-1"));
        let video = json!({ "constraints": { "video": { "mandatory": { "sourceId": "rdpio-videoinput-1", "minWidth": 1280 } } } });
        assert_eq!(stream_device(&video).as_deref(), Some("rdpio-videoinput-1"));
        assert_eq!(stream_device(&json!({ "id": "remote" })), None);
    }

    #[test]
    fn data_channel_payload_reads_teams_byte_arrays() {
        // Teams' send carries the bytes as a JSON array (reference capture).
        let arg = json!({ "data": [16, 15, 146, 0, 91, 93] });
        assert_eq!(data_channel_payload(&arg), vec![16, 15, 146, 0, 91, 93]);
        let b64 = json!({ "base64MessageData": "EA+SAA==" });
        assert_eq!(data_channel_payload(&b64), vec![0x10, 0x0F, 0x92, 0x00]);
        assert!(data_channel_payload(&json!({})).is_empty());
    }

    /// The add-in declines `rtcSession` with ERROR_NOT_SUPPORTED and no result
    /// (reference capture: `{"rpcName":"rtcSession","rpcCallId":342,
    /// "hr":-2147024846}`); we must answer identically, not with a success ack.
    #[tokio::test]
    async fn rtc_session_is_declined_like_the_add_in() {
        let mut r = Redirector::new();
        let call = RpcMessage::parse(
            br#"{"rpcObjectType":"RDWebRTCRedirector","rpcName":"rtcSession","rpcArgs":["7caba3e35f6a4b99bbbc5ff78290413b","1783829691630","32d3e98022738bb282427c9dd05e1ac1a8f48a7c231feeefe0673d31310c3ff4","1.1.2602.23001"],"rpcCallId":342}"#,
        )
        .unwrap();
        let out = r.handle(&call).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("rpcCallId").and_then(Value::as_u64), Some(342));
        assert_eq!(out[0].get("hr").and_then(Value::as_i64), Some(HR_NOT_SUPPORTED));
        assert!(out[0].get("result").is_none(), "decline carries no result: {}", out[0]);
    }

    #[test]
    fn munged_offer_is_recognized_by_its_fingerprint() {
        let ours = "v=0\r\na=ice-ufrag:abcd\r\na=fingerprint:sha-256 AA:BB:CC\r\nm=video 9 RTP 96 97\r\n";
        // The server prunes a codec but keeps the fingerprint → still our offer.
        let munged = "v=0\r\na=ice-ufrag:abcd\r\na=fingerprint:sha-256 AA:BB:CC\r\nm=video 9 RTP 97\r\n";
        // A different session (what a capture replay feeds us) → must be rejected.
        let foreign = "v=0\r\na=ice-ufrag:zzzz\r\na=fingerprint:sha-256 99:88:77\r\nm=video 9 RTP 96\r\n";
        assert!(same_dtls_session(ours, munged));
        assert!(!same_dtls_session(ours, foreign));
        assert!(!same_dtls_session(ours, "v=0\r\n"));
    }

    /// The live call-setup failure, reproduced end-to-end: Teams echoes our offer
    /// back to `setLocalDescription` in a codec-pruned ("munged") form. webrtc-rs
    /// refuses any local offer that isn't byte-identical to `createOffer`'s output
    /// ("new sdp does not match previous offer") and tears the peer connection
    /// down — so the dispatcher must apply our *own* stored offer, not the munged
    /// arg. Drive the whole offerer handshake and assert the munged
    /// setLocalDescription still returns success (hr 0).
    // webrtc-rs SDP workarounds / answers made for its offers.
    #[cfg(not(feature = "libwebrtc"))]
    #[tokio::test]
    async fn munged_set_local_description_applies_our_own_offer() {
        let mut r = Redirector::new();
        let call = |json: String| RpcMessage::parse(json.as_bytes()).unwrap();

        let out = r
            .handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createPeerConnection","rpcArgs":[{"iceServers":[]}],"rpcCallId":1}"#.into()))
            .await;
        assert_eq!(out[0].get("hr").and_then(Value::as_i64), Some(0), "createPeerConnection");

        // Data channel first (gives the offer its m=application), then media.
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createDataChannel","rpcArgs":[{"label":"main-channel","rpcObjectId":10}],"rpcCallId":2}"#.into())).await;
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{"kind":"audio","direction":"sendrecv","transceiverRpcObjectId":11}],"rpcCallId":3}"#.into())).await;
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{"kind":"video","direction":"recvonly","transceiverRpcObjectId":12}],"rpcCallId":4}"#.into())).await;

        let out = r
            .handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createOffer","rpcArgs":[{}],"rpcCallId":5}"#.into()))
            .await;
        let our_sdp = out[0]
            .pointer("/result/desc/sdp")
            .and_then(Value::as_str)
            .expect("offer sdp")
            .to_string();
        assert!(our_sdp.contains("m=application"), "offer lacks the data channel");

        // Munge the way Teams does: prune codec (rtpmap) lines, keeping the DTLS
        // fingerprint. This is a *different* string than createOffer produced, so
        // feeding it straight to webrtc-rs would fail — the dispatcher must not.
        let munged: String = our_sdp
            .lines()
            .filter(|l| !l.starts_with("a=rtpmap:"))
            .collect::<Vec<_>>()
            .join("\r\n");
        assert_ne!(munged, our_sdp, "munge did not change the SDP");

        let sld = format!(
            r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"setLocalDescription","rpcArgs":[{{"type":"offer","sdp":{}}}],"rpcCallId":6}}"#,
            serde_json::to_string(&munged).unwrap()
        );
        let out = r.handle(&call(sld)).await;
        assert_eq!(
            out[0].get("hr").and_then(Value::as_i64),
            Some(0),
            "setLocalDescription must accept our own offer despite the munged arg"
        );
        // The result must carry the transceivers with their assigned mids (Teams maps
        // its senders onto the m-lines from this) — NOT a bare "RPC succeeded." ack.
        let tx = out[0]
            .pointer("/result/transceivers")
            .and_then(Value::as_array)
            .expect("setLocalDescription result must list transceivers");
        assert_eq!(tx.len(), 2, "both transceivers should be reported");
        assert!(
            tx.iter().any(|t| t.get("kind").and_then(Value::as_str) == Some("audio")
                && t.get("mid").and_then(Value::as_str).is_some()),
            "audio transceiver must be reported with an assigned mid: {tx:?}"
        );
        assert!(
            tx.iter().all(|t| t.get("mid").and_then(Value::as_str).is_some()),
            "every transceiver must carry its assigned mid: {tx:?}"
        );
        // And the state-change events the add-in fires must accompany the reply.
        assert!(
            out.iter().any(|m| m.get("rpcEventName").and_then(Value::as_str)
                == Some("signalingstatechange")),
            "signalingstatechange event not emitted after setLocalDescription"
        );

        // ICE now gathers host candidates. Every `icecandidate` event must report the
        // SDP Teams set (the munged arg) back as `desc` — NOT webrtc-rs's own offer,
        // which is a codec-superset several KB larger. Reporting the latter desyncs
        // Teams' mirror of our localDescription and it aborts the call ~100 ms in.
        let mut desc_sdp = None;
        for _ in 0..40 {
            if let Some(ev) = r.drain_ice().await.into_iter().next() {
                desc_sdp = ev
                    .pointer("/rpcEventArgs/desc/sdp")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let desc_sdp = desc_sdp.expect("at least one ICE candidate should gather");
        assert_eq!(
            desc_sdp, munged,
            "icecandidate `desc` must echo the offer Teams set, not webrtc-rs's own"
        );
    }

    #[test]
    fn enrich_offer_adds_attributes_and_strips_recvonly_msid() {
        let sdp = concat!(
            "v=0\r\n",
            "a=group:BUNDLE 0 1 2\r\n",
            "a=extmap-allow-mixed\r\n",
            // Session-level DTLS fingerprint (webrtc-rs emits it once, here).
            "a=fingerprint:sha-256 AA:BB:CC\r\n",
            // m0: sendrecv — keeps its msid/ssrc.
            "m=audio 9 x\r\na=mid:0\r\na=ssrc:111 cname:c\r\na=msid:s t\r\na=sendrecv\r\n",
            // m1: recvonly — msid/ssrc must be stripped.
            "m=video 9 x\r\na=mid:1\r\na=ssrc:222 cname:c\r\na=msid:s t\r\na=recvonly\r\n",
            // m2: data channel — gains max-message-size + a media-level fingerprint,
            // loses the stray a=sendrecv.
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:2\r\na=sctp-port:5000\r\na=sendrecv\r\n",
        );
        let out = enrich_offer(sdp);
        assert!(out.contains("a=ice-options:trickle"), "missing trickle");
        assert!(out.contains("a=msid-semantic: WMS"), "missing msid-semantic");
        assert!(out.contains("a=max-message-size:262144"), "missing max-message-size");
        // The DTLS fingerprint is copied onto every m-line (session + 3 m-lines = 4)
        // so the media server accepts the SCTP data-channel m-line.
        assert_eq!(
            out.matches("a=fingerprint:sha-256 AA:BB:CC").count(),
            4,
            "fingerprint not copied onto every m-line:\n{out}"
        );
        // Specifically the data-channel block must now carry it.
        let app_block = out.split("m=application").nth(1).unwrap_or("");
        assert!(app_block.contains("a=fingerprint:"), "data channel missing fingerprint:\n{out}");
        // Sendrecv m-line keeps its send markers.
        assert!(out.contains("a=ssrc:111"), "sendrecv ssrc wrongly stripped");
        assert!(out.contains("a=msid:s t"), "sendrecv msid wrongly stripped");
        // Recvonly m-line loses them (the phantom send markers Teams rejects).
        assert!(!out.contains("a=ssrc:222"), "recvonly ssrc not stripped");
        // Exactly one msid line survives (the sendrecv one).
        assert_eq!(out.matches("a=msid:s t").count(), 1, "recvonly msid not stripped");
        // The audio m-line's real direction is untouched; only the data channel's
        // stray direction is removed (SCTP m-lines carry no direction).
        assert_eq!(out.matches("a=sendrecv").count(), 1, "data-channel a=sendrecv not stripped");
        assert!(out.contains("a=recvonly"), "recvonly direction preserved");
    }

    /// End-to-end against the SDP Teams' media server actually returned for an
    /// audio-only call (captured live): audio active, every video m-line and the data
    /// channel rejected (port 0) with a bare payload type and no rtpmap. Before the
    /// sanitizer this aborted `setRemoteDescription` with "payload type not found",
    /// tearing the just-answered call down ~1.8 s in. Drive the full public path and
    /// assert the answer now applies (hr 0).
    // webrtc-rs SDP workarounds / answers made for its offers.
    #[cfg(not(feature = "libwebrtc"))]
    #[tokio::test]
    async fn applies_the_real_audio_only_answer_from_teams() {
        let answer = include_str!("../tests/fixtures/audio_only_answer.sdp");
        let mut r = Redirector::new();
        let call = |json: String| RpcMessage::parse(json.as_bytes()).unwrap();

        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createPeerConnection","rpcArgs":[{"iceServers":[]}],"rpcCallId":1}"#.into())).await;
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createDataChannel","rpcArgs":[{"label":"main-channel","rpcObjectId":10}],"rpcCallId":2}"#.into())).await;
        // 1 audio + 9 video transceivers → the same 11 m-lines (audio + 9 video +
        // application) the answer carries.
        r.handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{"kind":"audio","direction":"sendrecv","transceiverRpcObjectId":11}],"rpcCallId":3}"#.into())).await;
        for i in 0..9u64 {
            let (id, cid) = (12 + i, 4 + i);
            r.handle(&call(format!(
                r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"addTransceiver","rpcArgs":[{{"kind":"video","direction":"recvonly","transceiverRpcObjectId":{id}}}],"rpcCallId":{cid}}}"#
            ))).await;
        }
        let out = r
            .handle(&call(r#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"createOffer","rpcArgs":[{}],"rpcCallId":20}"#.into()))
            .await;
        let offer = out[0].pointer("/result/desc/sdp").and_then(Value::as_str).expect("offer").to_string();
        let sld = format!(
            r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"setLocalDescription","rpcArgs":[{{"type":"offer","sdp":{}}}],"rpcCallId":21}}"#,
            serde_json::to_string(&offer).unwrap()
        );
        r.handle(&call(sld)).await;

        let srd = format!(
            r#"{{"rpcObjectType":"RTCPeerConnection","rpcObjectId":1,"rpcName":"setRemoteDescription","rpcArgs":[{{"type":"answer","sdp":{}}}],"rpcCallId":22}}"#,
            serde_json::to_string(answer).unwrap()
        );
        let out = r.handle(&call(srd)).await;
        assert_eq!(
            out[0].get("hr").and_then(Value::as_i64),
            Some(0),
            "real audio-only answer must apply after sanitizing: {:?}",
            out[0]
        );
        // Like the add-in, the result lists every transceiver with its mid — not a
        // bare ack.
        let tx = out[0]
            .pointer("/result/transceivers")
            .and_then(Value::as_array)
            .expect("setRemoteDescription result must list transceivers");
        assert_eq!(tx.len(), 10);
        assert!(tx.iter().all(|t| t.get("mid").is_some() && t.get("kind").is_some()));
        // The add-in fires an `ontrack` event only for the transceivers the answer
        // negotiated as receiving. This answer accepts audio and rejects all 9 video
        // m-lines (port 0 / inactive), so exactly ONE track event (the audio receiver)
        // must fire — firing for the rejected video mids would tell Teams receivers
        // exist where the answer says none do.
        let track_events = out
            .iter()
            .filter(|m| m.get("rpcEventName").and_then(Value::as_str) == Some("track"))
            .count();
        assert_eq!(track_events, 1, "expected one track event for the accepted audio only: {out:#?}");
        assert!(
            out.iter().any(|m| m.get("rpcEventName").and_then(Value::as_str)
                == Some("iceconnectionstatechange")),
            "iceconnectionstatechange not emitted after the answer"
        );
    }

    #[test]
    fn sanitize_remote_sdp_maps_bare_payloads_on_rejected_m_lines() {
        // An audio-only answer: audio active with full rtpmaps, video rejected
        // (port 0) with a bare dynamic PT and no rtpmap — the shape that aborted
        // setRemoteDescription with "payload type not found".
        let answer = concat!(
            "v=0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=audio 3478 UDP/TLS/RTP/SAVPF 111 0\r\n",
            "a=rtpmap:111 OPUS/48000/2\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=inactive\r\n",
            "m=video 0 UDP/TLS/RTP/SAVPF 36\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=inactive\r\n",
        );
        let out = sanitize_remote_sdp(answer);
        // The rejected video line's bare PT 36 gets a synthetic rtpmap so parsing works.
        assert!(out.contains("a=rtpmap:36 H264/90000"), "bare video PT not mapped:\n{out}");
        // The audio line already had rtpmaps — nothing spurious added.
        assert_eq!(out.matches("a=rtpmap:111").count(), 1, "audio rtpmap duplicated");
        assert_eq!(out.matches("a=rtpmap:0 ").count(), 1, "static audio rtpmap duplicated");
        // Idempotent: a second pass changes nothing.
        assert_eq!(sanitize_remote_sdp(&out), out, "sanitize is not idempotent");
    }

    #[test]
    fn slim_video_offer_keeps_only_h264_and_dedupes_rtcp_fb() {
        let sdp = concat!(
            "v=0\r\n",
            "a=group:BUNDLE 0 1\r\n",
            // audio — untouched.
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\na=rtcp-fb:111 nack\r\na=rtcp-fb:111 nack\r\n",
            // video: H264(102)+VP8(96)+AV1(41) with rtx (103->102, 97->96, 42->41) and
            // DUPLICATE nack/nack pli on 102.
            "m=video 9 UDP/TLS/RTP/SAVPF 102 96 41 103 97 42\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=mid:1\r\n",
            "a=rtpmap:102 H264/90000\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:41 AV1/90000\r\n",
            "a=rtpmap:103 rtx/90000\r\n",
            "a=rtpmap:97 rtx/90000\r\n",
            "a=rtpmap:42 rtx/90000\r\n",
            "a=fmtp:103 apt=102\r\n",
            "a=fmtp:97 apt=96\r\n",
            "a=fmtp:42 apt=41\r\n",
            "a=rtcp-fb:102 nack\r\n",
            "a=rtcp-fb:102 nack pli\r\n",
            "a=rtcp-fb:102 nack\r\n",
            "a=rtcp-fb:102 nack pli\r\n",
            "a=rtcp-fb:96 nack\r\n",
            "a=recvonly\r\n",
        );
        let out = slim_video_offer(sdp);
        let vblock = out.split("m=video").nth(1).unwrap_or("");
        // Only H264 (102) + its rtx (103) survive on the m= line.
        let mline = out.lines().find(|l| l.starts_with("m=video")).unwrap();
        assert_eq!(mline, "m=video 9 UDP/TLS/RTP/SAVPF 102 103", "m= line not slimmed:\n{out}");
        // Non-H264 codecs + their rtx are gone.
        assert!(!vblock.contains("VP8"), "VP8 not removed:\n{out}");
        assert!(!vblock.contains("AV1"), "AV1 not removed:\n{out}");
        assert!(!vblock.contains("apt=96") && !vblock.contains("apt=41"), "orphan rtx left:\n{out}");
        // H264 + its rtx remain.
        assert!(vblock.contains("a=rtpmap:102 H264/90000"), "H264 dropped:\n{out}");
        assert!(vblock.contains("a=rtpmap:103 rtx/90000") && vblock.contains("apt=102"), "H264 rtx dropped:\n{out}");
        // rtcp-fb deduped: exactly one `nack` and one `nack pli` for 102.
        assert_eq!(vblock.matches("a=rtcp-fb:102 nack\r\n").count(), 1, "nack not deduped:\n{out}");
        assert_eq!(vblock.matches("a=rtcp-fb:102 nack pli").count(), 1, "nack pli not deduped:\n{out}");
        // Audio block untouched (its duplicate fb is not our concern; audio is accepted).
        assert!(out.contains("a=rtpmap:111 opus/48000/2"), "audio mangled:\n{out}");
    }

    #[test]
    fn enrich_offer_pairs_rtx_with_every_video_codec() {
        // Two video m-lines sharing the same H264 payload types (max-BUNDLE) plus an
        // audio m-line, mirroring the real add-in offer: audio 0, camera send video 1,
        // recv video 2. Every video codec must gain a paired RTX codec; audio must not.
        let sdp = concat!(
            "v=0\r\n",
            "a=group:BUNDLE 0 1 2\r\n",
            "a=fingerprint:sha-256 AA:BB:CC\r\n",
            // audio — must NOT get RTX.
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\na=sendrecv\r\n",
            // camera send video (has ssrc).
            "m=video 9 UDP/TLS/RTP/SAVPF 102 125\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=rtpmap:102 H264/90000\r\na=rtpmap:125 H264/90000\r\na=ssrc:222 cname:c\r\na=msid:s v\r\na=sendrecv\r\n",
            // recv video with the same codecs.
            "m=video 9 UDP/TLS/RTP/SAVPF 102 125\r\nc=IN IP4 0.0.0.0\r\na=mid:2\r\na=rtpmap:102 H264/90000\r\na=rtpmap:125 H264/90000\r\na=recvonly\r\n",
        );
        let out = enrich_offer(sdp);

        // Each distinct H264 PT is paired with exactly one RTX codec, bundle-wide.
        let apt102 = out.matches("apt=102").count();
        let apt125 = out.matches("apt=125").count();
        assert!(apt102 >= 1 && apt125 >= 1, "H264 not paired with RTX:\n{out}");
        // The RTX codecs are declared as rtx/90000.
        assert!(out.contains("rtx/90000"), "no rtx rtpmap emitted:\n{out}");
        // Audio must carry no RTX.
        let audio_block = out.split("m=audio").nth(1).and_then(|s| s.split("m=video").next()).unwrap_or("");
        assert!(!audio_block.contains("rtx/90000"), "RTX wrongly added to audio:\n{out}");
        // The same H264 PT maps to the SAME RTX PT across both video m-lines (max-BUNDLE
        // shares one payload-type space): apt=102 appears once per video m-line = twice.
        assert_eq!(apt102, 2, "apt=102 should appear once per video m-line:\n{out}");
        assert_eq!(apt125, 2, "apt=125 should appear once per video m-line:\n{out}");

        // Idempotent: a second pass sees the RTX already paired and adds nothing.
        let out2 = enrich_offer(&out);
        assert_eq!(
            out2.matches("rtx/90000").count(),
            out.matches("rtx/90000").count(),
            "enrich_offer RTX injection is not idempotent:\n{out2}"
        );
    }

    #[test]
    fn accepted_mids_lists_only_active_answer_m_lines() {
        // The real Plaza audio-only answer shape: audio accepted (port 3478, mid 0),
        // every video rejected (port 0, a=inactive, NO mid), data channel rejected.
        let answer = concat!(
            "v=0\r\n",
            "a=group:BUNDLE 0\r\n",
            "m=audio 3478 UDP/TLS/RTP/SAVPF 111\r\n",
            "a=mid:0\r\n",
            "m=video 0 UDP/TLS/RTP/SAVPF 36\r\n",
            "c=IN IP4 10.10.10.10\r\n",
            "a=inactive\r\n",
            "m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=inactive\r\n",
            "a=sctp-port:5000\r\n",
        );
        let mids = accepted_mids(answer);
        assert!(mids.contains("0"), "accepted audio mid missing: {mids:?}");
        assert_eq!(mids.len(), 1, "only the audio m-line should be accepted: {mids:?}");

        // An all-accepted answer (audio + one video, both with mids and real ports).
        let full = concat!(
            "v=0\r\n",
            "m=audio 3478 x 111\r\na=mid:0\r\n",
            "m=video 3478 x 107\r\na=mid:1\r\n",
        );
        let mids = accepted_mids(full);
        assert_eq!(mids.len(), 2, "both m-lines should be accepted: {mids:?}");
        assert!(mids.contains("0") && mids.contains("1"));
    }

    #[test]
    fn reply_envelope_carries_hr_and_names() {
        let msg = RpcMessage::parse(
            br#"{"rpcObjectType":"RTCPeerConnection","rpcObjectId":11,"rpcName":"createOffer","rpcCallId":79}"#,
        )
        .unwrap();
        let r = reply(&msg, 79, Some(json!("RPC succeeded.")), 0);
        assert_eq!(r.get("hr").and_then(Value::as_i64), Some(0));
        assert_eq!(r.get("rpcName").and_then(Value::as_str), Some("createOffer"));
        assert_eq!(r.get("rpcObjectId").and_then(Value::as_u64), Some(11));
        assert_eq!(r.get("rpcCallId").and_then(Value::as_u64), Some(79));
    }
}
