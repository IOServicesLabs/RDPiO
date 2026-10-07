//! webrtc-rs-backed WebRTC engine (feature `engine`).
//!
//! Phase B: turn the reversed webrtc.1 object-model calls into *real* WebRTC via
//! [`webrtc`] (webrtc-rs). Given the `createPeerConnection` / `addTransceiver` /
//! `setDirection` / `createOffer` / `setLocal`+`setRemoteDescription` calls a
//! session issues, this drives a live [`RTCPeerConnection`] and produces the SDP
//! offer, gathers ICE, and accepts the peer's answer — the same operations the
//! Windows add-in performs, but portable. Validated against a real captured Teams
//! call (see `tests/engine_replay.rs`).
//!
//! The engine is intentionally a thin, imperative surface (one method per RPC).
//! The [`crate::session`] dispatcher will call these and marshal the results/
//! events back onto the channel; keeping the engine free of protocol-framing
//! concerns makes it independently testable.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::Value;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCRtpTransceiver, RTCRtpTransceiverInit};
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::ice::TurnResolver;

pub use crate::engine_api::{AudioCaptureSource, I420Frame, MediaSink, PeerEvent, VideoCaptureSource};

/// The dispatcher's switch: webrtc-rs needs the SDP workarounds (it must apply
/// its own exact offer, and can't parse Plaza's answers unaided).
pub const NATIVE_JSEP: bool = false;
use crate::engine_api::push_event;

/// Samples per 20 ms Opus frame at the 48 kHz RTP clock.
const OPUS_FRAME_TICKS: u32 = 960;

/// Stamps the mic's Opus frames into RTP packets the way libwebrtc does with
/// DTX: every 20 ms frame advances the timestamp, but DTX frames (the 1–2 byte
/// packets the encoder emits during silence) and muted frames are not sent — so
/// sequence numbers stay contiguous across silence while the timestamp jumps, and
/// the first packet after a gap carries the marker bit (talkspurt start).
/// Teams' media server asks for this (`usedtx=1` in its answer); the reference
/// add-in sends ~5 packets/s while quiet, and the other side's audio only began
/// flowing once it did.
struct OpusPacketizer {
    sequence: u16,
    timestamp: u32,
    after_gap: bool,
}

impl OpusPacketizer {
    fn new() -> Self {
        use std::hash::{BuildHasher, Hasher};
        // Random initial sequence number and timestamp (RFC 3550 §5.1).
        let r = std::collections::hash_map::RandomState::new().build_hasher().finish();
        Self { sequence: r as u16, timestamp: (r >> 32) as u32, after_gap: true }
    }

    /// The RTP packet for the next 20 ms frame, or `None` when it isn't sent
    /// (DTX or muted: `frame` of 2 bytes or less).
    fn next(&mut self, frame: &[u8]) -> Option<Packet> {
        let timestamp = self.timestamp;
        self.timestamp = self.timestamp.wrapping_add(OPUS_FRAME_TICKS);
        if frame.len() <= 2 {
            self.after_gap = true;
            return None;
        }
        let packet = Packet {
            header: Header {
                version: 2,
                marker: std::mem::take(&mut self.after_gap),
                sequence_number: self.sequence,
                timestamp,
                ..Default::default()
            },
            payload: bytes::Bytes::copy_from_slice(frame),
        };
        self.sequence = self.sequence.wrapping_add(1);
        Some(packet)
    }
}

/// Opus exactly as webrtc-rs's default codec table registers it (PT 111), so the
/// mic track binds to the negotiated audio codec.
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Engine errors: either the underlying webrtc-rs failure, or a protocol misuse
/// (a call that needs a peer connection before one exists).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Webrtc(#[from] webrtc::Error),
    #[error("no active peer connection")]
    NoPeerConnection,
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// A native WebRTC engine driven by webrtc.1 RPC calls.
pub struct WebrtcEngine {
    pc: Option<Arc<RTCPeerConnection>>,
    /// `transceiverRpcObjectId` → engine transceiver, so later `setDirection` /
    /// `replaceTrack` calls can find it.
    transceivers: HashMap<u64, Arc<RTCRtpTransceiver>>,
    /// `senderRpcObjectId` → its transceiver. `replaceTrack` targets the *sender*
    /// object (a sub-object of the transceiver), so we index by sender id too, filled
    /// in at `addTransceiver` (whose args carry both ids).
    senders: HashMap<u64, Arc<RTCRtpTransceiver>>,
    /// Source of outbound camera video (set before `create_peer_connection`). When
    /// Teams `replaceTrack`s a camera onto a video sender, we attach an H.264 send
    /// track fed from here so the offer carries real outbound video.
    video_source: Option<Arc<dyn VideoCaptureSource>>,
    /// Source of outbound mic audio (set before `create_peer_connection`), pumped
    /// into the audio sender's Opus track once Teams `replaceTrack`s the mic.
    audio_source: Option<Arc<dyn AudioCaptureSource>>,
    /// `senderRpcObjectId` → the Opus send track created with that audio sender,
    /// so `replaceTrack` can start feeding it. RTP-level so the mic pump can
    /// stamp packets itself (see [`OpusPacketizer`]).
    audio_tracks: HashMap<u64, Arc<TrackLocalStaticRTP>>,
    /// The Teams track id (`trackRpcObjectId`) currently feeding the mic sender,
    /// and whether it is enabled — Teams mutes by disabling that track
    /// (`onEnabledAttributeChanged [false]`), which must silence what we send.
    mic_track: Option<String>,
    mic_enabled: Arc<AtomicBool>,
    /// Cleared per peer connection; set on close to stop the per-track send loops.
    send_stop: Arc<AtomicBool>,
    /// Teams track id → the capture device its stream was created for.
    track_devices: HashMap<String, String>,
    /// Data channels the session created, by their remoted object id. Held so they
    /// stay open — and so the offer carries the `m=application` (SCTP) section
    /// Teams requires: it opens a "main-channel" data channel before `createOffer`
    /// and tears the whole peer connection down if the offer doesn't negotiate it.
    data_channels: HashMap<u64, Arc<RTCDataChannel>>,
    /// Local ICE candidates gathered so far (from `on_ice_candidate`), each as the
    /// trickle-event `candidate` object the add-in sends (`{candidate, sdp_mid,
    /// sdp_mline_index, usernameFragment}` — the standard RTCIceCandidateInit shape
    /// Teams' `onicecandidate` handler consumes).
    candidates: Arc<Mutex<Vec<Value>>>,
    /// The offer's ICE ufrag (`a=ice-ufrag`), captured at `set_local_offer`. Teams'
    /// `addIceCandidate` needs each trickled candidate's `usernameFragment`, but
    /// webrtc-rs's `RTCIceCandidate::to_json()` hardcodes it to `None` — so we fill
    /// it from here. Shared so the `on_ice_candidate` callback can read it.
    ice_ufrag: Arc<Mutex<Option<String>>>,
    /// Where inbound remote media is delivered (set before `createPeerConnection`).
    sink: Option<Arc<dyn MediaSink>>,
    /// Connection-state and data-channel events waiting for [`Self::take_events`].
    events: Arc<Mutex<Vec<PeerEvent>>>,
    /// Follows TURN `300 Try Alternate` redirects webrtc-rs can't (set before
    /// `createPeerConnection`); used to rewrite Teams' anycast relay URL to its
    /// unicast backend so a UDP relay candidate can actually be allocated.
    turn_resolver: Option<Arc<dyn TurnResolver>>,
}

impl Default for WebrtcEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WebrtcEngine {
    pub fn new() -> Self {
        Self {
            pc: None,
            transceivers: HashMap::new(),
            senders: HashMap::new(),
            data_channels: HashMap::new(),
            candidates: Arc::new(Mutex::new(Vec::new())),
            ice_ufrag: Arc::new(Mutex::new(None)),
            sink: None,
            events: Arc::new(Mutex::new(Vec::new())),
            video_source: None,
            audio_source: None,
            audio_tracks: HashMap::new(),
            mic_track: None,
            mic_enabled: Arc::new(AtomicBool::new(true)),
            send_stop: Arc::new(AtomicBool::new(false)),
            track_devices: HashMap::new(),
            turn_resolver: None,
        }
    }

    /// `MediaStreamTrack.onEnabledAttributeChanged` — Teams' mute. Only the track
    /// feeding the mic sender matters; while it is disabled the mic pump sends
    /// nothing (as a browser's muted Opus sender in DTX does).
    /// Record which capture device (`rdpio-audioinput-N` / `rdpio-videoinput-N`)
    /// the stream holding Teams track `track_id` was created for, so a later
    /// `replaceTrack` starts that device rather than the system default.
    pub fn set_track_device(&mut self, track_id: &str, device_id: &str) {
        self.track_devices.insert(track_id.to_string(), device_id.to_string());
    }

    /// The device to capture for Teams track `track_id` (the track id itself
    /// when unknown — the source then falls back to the default device).
    fn device_for(&self, track_id: &str) -> String {
        self.track_devices.get(track_id).cloned().unwrap_or_else(|| track_id.to_string())
    }

    /// Tell the host which Teams `rpcObjectId` names remote stream `stream_id`.
    pub fn announce_remote_stream(&self, rpc_id: u64, stream_id: &str) {
        if let Some(sink) = &self.sink {
            sink.on_remote_stream(rpc_id, stream_id);
        }
    }

    pub fn set_track_enabled(&self, track_id: &str, enabled: bool) {
        if self.mic_track.as_deref() == Some(track_id) {
            let was = self.mic_enabled.swap(enabled, Ordering::SeqCst);
            if was != enabled {
                tracing::info!(track_id, enabled, "call mic {}", if enabled { "unmuted" } else { "muted" });
            }
        }
    }

    /// Install the media sink that receives inbound remote tracks. Must be set
    /// before `create_peer_connection` so `on_track` is wired.
    pub fn set_sink(&mut self, sink: Arc<dyn MediaSink>) {
        self.sink = Some(sink);
    }

    /// Install the outbound camera video source. Must be set before
    /// `create_peer_connection`; used when Teams `replaceTrack`s a camera onto a
    /// video sender to attach a real H.264 send track.
    pub fn set_video_source(&mut self, source: Arc<dyn VideoCaptureSource>) {
        self.video_source = Some(source);
    }

    /// Install the outbound mic audio source. Must be set before
    /// `create_peer_connection`; fed into the audio sender's Opus track when Teams
    /// `replaceTrack`s the mic.
    pub fn set_audio_source(&mut self, source: Arc<dyn AudioCaptureSource>) {
        self.audio_source = Some(source);
    }

    /// Install the TURN redirect resolver. Must be set before
    /// `create_peer_connection`, which uses it to rewrite anycast `turn:` URLs to
    /// their unicast backend (webrtc-rs can't follow the `300 Try Alternate`).
    pub fn set_turn_resolver(&mut self, resolver: Arc<dyn TurnResolver>) {
        self.turn_resolver = Some(resolver);
    }

    /// `RTCPeerConnection.createPeerConnection` — build the peer connection with
    /// default codecs/interceptors and the session's ICE servers.
    pub async fn create_peer_connection(&mut self, config: &Value) -> Result<()> {
        // A session may build several peer connections in turn (Teams tears one
        // down and retries). Start each from clean state so stale transceivers /
        // data channels / candidates from the previous one can't leak into it.
        self.transceivers.clear();
        self.senders.clear();
        self.audio_tracks.clear();
        self.mic_track = None;
        self.data_channels.clear();
        // Stop any previous PC's send loops, then arm a fresh flag for this one.
        self.send_stop.store(true, Ordering::SeqCst);
        self.send_stop = Arc::new(AtomicBool::new(false));
        self.stop_mic();
        if let Ok(mut c) = self.candidates.lock() {
            c.clear();
        }
        if let Ok(mut u) = self.ice_ufrag.lock() {
            *u = None;
        }
        if let Ok(mut e) = self.events.lock() {
            e.clear();
        }

        let mut media = MediaEngine::default();
        media.register_default_codecs()?;
        register_comfort_noise(&mut media)?;
        register_teams_header_extensions(&mut media)?;
        let mut registry = register_default_interceptors(Registry::new(), &mut media)?;
        // Log what really arrives on each inbound stream (see `crate::probe`).
        registry.add(Box::new(crate::probe::RtpProbeBuilder));
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(ice_setting_engine())
            .build();

        let rtc_config = RTCConfiguration {
            ice_servers: self.resolve_ice_servers(config).await,
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(rtc_config).await?);

        let candidates = self.candidates.clone();
        let ice_ufrag = self.ice_ufrag.clone();
        pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let candidates = candidates.clone();
            let ice_ufrag = ice_ufrag.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    // With max-bundle all candidates ride the bundle transport
                    // (rtcp is muxed onto it); component-2 (rtcp) candidates are
                    // vestigial and the real add-in doesn't trickle them, so skip.
                    if c.component != 1 {
                        return;
                    }
                    let ufrag = ice_ufrag.lock().ok().and_then(|u| u.clone());
                    if let Some(cand) = candidate_event(&c, ufrag) {
                        candidates.lock().unwrap().push(cand);
                    }
                }
            })
        }));

        // Relay the real connection progress, as the add-in does: Teams waits for
        // `connected` states before it treats the call as up. "new" is the
        // starting state, never reported as a change.
        let ev = self.events.clone();
        pc.on_ice_connection_state_change(Box::new(move |s: RTCIceConnectionState| {
            if !matches!(s, RTCIceConnectionState::New | RTCIceConnectionState::Unspecified) {
                push_event(&ev, PeerEvent::State { event: "iceconnectionstatechange", state: s.to_string() });
            }
            Box::pin(async {})
        }));
        let ev = self.events.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            if !matches!(s, RTCPeerConnectionState::New | RTCPeerConnectionState::Unspecified) {
                push_event(&ev, PeerEvent::State { event: "connectionstatechange", state: s.to_string() });
            }
            Box::pin(async {})
        }));
        let ev = self.events.clone();
        pc.on_ice_gathering_state_change(Box::new(move |s: RTCIceGathererState| {
            if s == RTCIceGathererState::Complete {
                push_event(&ev, PeerEvent::State { event: "icegatheringstatechange", state: s.to_string() });
            }
            Box::pin(async {})
        }));

        // Deliver inbound remote media to the sink. We find and read each
        // receiver's tracks ourselves instead of waiting for `on_track`: webrtc-rs
        // only fires `on_track` after peeking a first packet whose payload type it
        // negotiated, and abandons the track for good when that one packet's type
        // is unknown — even though the track keeps receiving. Teams' audio died
        // exactly that way (`Could not determine PayloadType for SSRC 1000`) with
        // every payload type in its answer negotiated.
        if let Some(sink) = self.sink.clone() {
            tokio::spawn(read_remote_tracks(Arc::downgrade(&pc), sink));
        }

        self.pc = Some(pc);
        Ok(())
    }

    /// `RTCPeerConnection.addTransceiver` — add a media m-line of the given kind
    /// and initial direction, remembered by its transceiver *and* sender object ids
    /// (`replaceTrack` later targets the sender).
    pub async fn add_transceiver(
        &mut self,
        kind: &str,
        direction: &str,
        id: u64,
        sender_id: u64,
        _send_encodings: &[crate::engine_api::SendEncoding],
    ) -> Result<()> {
        let pc = self.pc()?;
        let codec_type = match kind {
            "video" => RTPCodecType::Video,
            _ => RTPCodecType::Audio,
        };
        // Video transceivers are created **receive-only** so webrtc-rs does NOT auto-attach
        // a send track. `add_transceiver_from_kind(Sendrecv/Sendonly)` builds a
        // `TrackLocalStaticSample` fixed to the media engine's FIRST codec of that kind
        // (VP8 for video); when Teams' answer negotiates a different codec (H264) the
        // sender can't bind that track and `set_remote_description` fails with "codec is
        // not supported by remote", tearing the just-answered call down.
        //
        // VIDEO stays receive-only permanently for now: camera send is disabled. Teams'
        // media server (Plaza) rejects our video m-lines (port 0), and a send track bound
        // to a rejected m-line can't start — which aborts the ENTIRE answer with "codec
        // is not supported by remote", killing every negotiation round. Until video
        // *acceptance* is solved, sending camera video is impossible anyway, so we keep
        // video recv-only (Teams' `sendEncodings`/`setDirection`/`replaceTrack` for the
        // camera are all no-ops) so the answer at least applies cleanly. AUDIO is built
        // around an Opus send track and then set to Teams' requested direction (below).
        let requested = parse_direction(direction);
        let t = if codec_type == RTPCodecType::Audio {
            // AUDIO gets a real Opus send track up front. Teams creates the audio
            // transceiver `inactive` and flips it to `sendrecv` later, and a sender
            // created receive-only has no send encoding to `replaceTrack` onto — so
            // the mic could never attach. Opus is exactly what Teams' answer
            // negotiates for audio, so this track always binds (unlike video, where
            // the default first codec is VP8 and the answer picks H.264).
            let track = Arc::new(TrackLocalStaticRTP::new(
                opus_capability(),
                format!("audio{sender_id}"),
                format!("nativeAudio{sender_id}"),
            ));
            let init = RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Sendrecv,
                send_encodings: vec![],
            };
            let t = pc
                .add_transceiver_from_track(
                    track.clone() as Arc<dyn TrackLocal + Send + Sync>,
                    Some(init),
                )
                .await?;
            if requested != RTCRtpTransceiverDirection::Sendrecv {
                t.set_direction(requested).await;
            }
            self.audio_tracks.insert(sender_id, track);
            t
        } else {
            let init = RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            };
            pc.add_transceiver_from_kind(codec_type, Some(init)).await?
        };
        self.transceivers.insert(id, t.clone());
        self.senders.insert(sender_id, t);
        Ok(())
    }

    /// `RTCPeerConnection.createDataChannel` — open a data channel so the offer
    /// carries the `m=application` (DTLS/SCTP) section. Teams opens "main-channel"
    /// before `createOffer`; an offer without it makes Teams close the data channel
    /// and then the whole peer connection without ever answering.
    pub async fn create_data_channel(&mut self, label: &str, id: u64) -> Result<()> {
        let pc = self.pc()?;
        let dc = pc.create_data_channel(label, None).await?;
        // Report open / closing / every message, keyed by the channel's remoted id.
        // The SCTP stream id is only assigned once the transport is up, so read it
        // when each event fires (through a weak ref — the channel owns these).
        let stream_id = {
            let weak = Arc::downgrade(&dc);
            move || weak.upgrade().map(|d| d.id()).unwrap_or(0)
        };
        let (ev, sid) = (self.events.clone(), stream_id.clone());
        dc.on_open(Box::new(move || {
            push_event(&ev, PeerEvent::ChannelOpen { object_id: id, stream_id: sid() });
            Box::pin(async {})
        }));
        let (ev, sid) = (self.events.clone(), stream_id.clone());
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            push_event(
                &ev,
                PeerEvent::ChannelMessage { object_id: id, stream_id: sid(), data: msg.data.to_vec() },
            );
            Box::pin(async {})
        }));
        let (ev, sid) = (self.events.clone(), stream_id);
        dc.on_close(Box::new(move || {
            push_event(&ev, PeerEvent::ChannelClosing { object_id: id, stream_id: sid() });
            Box::pin(async {})
        }));
        self.data_channels.insert(id, dc);
        Ok(())
    }

    /// `RTCDataChannel.send` — send `data` (binary) on the data channel Teams
    /// remoted as `id`. Unknown channel → no-op.
    pub async fn send_data(&self, id: u64, data: Vec<u8>) -> Result<()> {
        if let Some(dc) = self.data_channels.get(&id) {
            dc.send(&bytes::Bytes::from(data)).await?;
        } else {
            tracing::debug!(id, "send on an unknown data channel; dropped");
        }
        Ok(())
    }

    /// `RTCDataChannel.close`.
    pub async fn close_data_channel(&mut self, id: u64) -> Result<()> {
        if let Some(dc) = self.data_channels.remove(&id) {
            dc.close().await?;
        }
        Ok(())
    }

    /// The codecs the applied answer negotiated for the first `kind`
    /// ("audio"/"video") receiver, as `(payload type, mime type)` — what inbound
    /// packets are matched against.
    pub async fn negotiated_codecs(&self, kind: &str) -> Vec<(u8, String)> {
        let Ok(pc) = self.pc() else { return Vec::new() };
        for t in pc.get_transceivers().await {
            let matches = match t.kind() {
                RTPCodecType::Audio => kind == "audio",
                RTPCodecType::Video => kind == "video",
                _ => false,
            };
            if matches {
                let params = t.receiver().await.get_parameters().await;
                return params
                    .codecs
                    .iter()
                    .map(|c| (c.payload_type, c.capability.mime_type.clone()))
                    .collect();
            }
        }
        Vec::new()
    }

    /// `RTCPeerConnection.getStats`, shaped like the add-in's (libwebrtc) report:
    /// `{stats: [RTCStats…], receivers: []}` with W3C `type`s, camelCase fields
    /// and microsecond timestamps. Teams polls it every second once a call
    /// connects and reads it to confirm media is flowing — notably the
    /// transport's `dtlsState` and `selectedCandidatePairId`, which webrtc-rs
    /// leaves out and we fill in. In the reference call the media server began
    /// sending call audio right after the add-in's first report; answering with a
    /// bare ack got a call whose audio never started.
    pub async fn stats_report(&self) -> Value {
        let Ok(pc) = self.pc() else {
            return serde_json::json!({ "stats": [], "receivers": [] });
        };
        let mut stats: Vec<Value> = pc
            .get_stats()
            .await
            .reports
            .values()
            .filter_map(|r| serde_json::to_value(r).ok())
            .collect();
        let kind = |s: &Value| s.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        // The pair carrying the media: nominated and succeeded, busiest first.
        let selected_pair = stats
            .iter()
            .filter(|s| {
                kind(s) == "candidate-pair"
                    && s.get("nominated").and_then(Value::as_bool) == Some(true)
                    && s.get("state").and_then(Value::as_str) == Some("succeeded")
            })
            .max_by_key(|s| s.get("bytesReceived").and_then(Value::as_u64).unwrap_or(0))
            .and_then(|s| s.get("id").cloned());
        let transport_id = stats
            .iter()
            .find(|s| kind(s) == "transport")
            .and_then(|s| s.get("id").cloned())
            .unwrap_or_else(|| Value::from("T01"));
        let dtls_state = match pc.connection_state() {
            RTCPeerConnectionState::Connected | RTCPeerConnectionState::Disconnected => "connected",
            RTCPeerConnectionState::Connecting => "connecting",
            RTCPeerConnectionState::Failed => "failed",
            RTCPeerConnectionState::Closed => "closed",
            _ => "new",
        };
        for s in &mut stats {
            let t = kind(s);
            let Some(obj) = s.as_object_mut() else { continue };
            // libwebrtc reports integer microseconds since the epoch.
            if let Some(secs) = obj.get("timestamp").and_then(Value::as_f64) {
                obj.insert("timestamp".into(), Value::from((secs * 1e6) as i64));
            }
            match t.as_str() {
                "transport" => {
                    obj.insert("dtlsState".into(), Value::from(dtls_state));
                    obj.insert("dtlsRole".into(), Value::from("client"));
                    if let Some(p) = &selected_pair {
                        obj.insert("selectedCandidatePairId".into(), p.clone());
                    }
                }
                "inbound-rtp" => {
                    obj.entry("jitter").or_insert(Value::from(0));
                    obj.entry("packetsLost").or_insert(Value::from(0));
                    obj.entry("transportId").or_insert(transport_id.clone());
                }
                "outbound-rtp" | "candidate-pair" | "local-candidate" | "remote-candidate"
                | "codec" => {
                    obj.entry("transportId").or_insert(transport_id.clone());
                }
                _ => {}
            }
        }
        serde_json::json!({ "stats": stats, "receivers": [] })
    }

    /// Connection-state and data-channel events since the last call.
    pub fn take_events(&self) -> Vec<PeerEvent> {
        self.events.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    /// `RTCPeerConnection.close` — tear the peer connection down and forget the
    /// objects hanging off it.
    pub async fn close_peer_connection(&mut self) -> Result<()> {
        // Stop the per-track camera/mic send loops before tearing the PC down.
        self.send_stop.store(true, Ordering::SeqCst);
        self.stop_mic();
        if let Some(pc) = self.pc.take() {
            pc.close().await?;
        }
        self.transceivers.clear();
        self.senders.clear();
        self.audio_tracks.clear();
        self.data_channels.clear();
        if let Ok(mut c) = self.candidates.lock() {
            c.clear();
        }
        Ok(())
    }

    /// `RTCRtpSender.replaceTrack` — Teams attaches a capture track to a sender. For a
    /// **video** sender (the camera) we build an H.264 [`TrackLocalStaticSample`], bind
    /// it to that sender, ensure the transceiver is send-capable, and spawn a loop that
    /// pumps Annex-B frames from the [`VideoCaptureSource`] into it. This is what puts a
    /// real send configuration on the offer's video m-line so Plaza accepts it (and the
    /// bundled data channel with it). An **audio** sender (the mic) starts feeding the
    /// Opus track it was created with — see [`Self::attach_mic`].
    pub async fn replace_track(&mut self, sender_id: u64, source_id: &str) -> Result<()> {
        let Some(t) = self.senders.get(&sender_id).cloned() else {
            tracing::debug!(sender_id, "replaceTrack for unknown sender; ignoring");
            return Ok(());
        };
        if t.kind() != RTPCodecType::Video {
            return self.attach_mic(sender_id, source_id);
        }
        // Camera video send is disabled (see add_transceiver): video transceivers are
        // receive-only, so their sender has no send encoding to replace. Attaching a
        // track here would fail `ErrRTPSenderNewTrackHasIncorrectEnvelope`, and even if
        // it bound, Plaza rejects the m-line so the track couldn't start (aborting the
        // answer). Ack the replaceTrack as a no-op so Teams' state stays consistent.
        if !t.direction().has_send() {
            tracing::debug!(
                sender_id,
                source_id,
                "video sender is receive-only; replaceTrack acked as no-op (camera send disabled)"
            );
            return Ok(());
        }
        let Some(source) = self.video_source.clone() else {
            tracing::warn!("replaceTrack(video) but no camera source is configured — offer will carry no outbound video");
            return Ok(());
        };

        // webrtc-rs's default H.264 capability, so the send track's codec is guaranteed
        // to be in the negotiated set (a mismatched codec fails the track bind and tears
        // the just-answered call down — the `codec is not supported by remote` failure).
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                    .to_owned(),
                ..Default::default()
            },
            format!("video{sender_id}"),
            format!("nativeVideo{sender_id}"),
        ));
        let sender = t.sender().await;
        sender
            .replace_track(Some(track.clone() as Arc<dyn TrackLocal + Send + Sync>))
            .await?;
        // Make sure the m-line actually sends (Teams sets the direction separately).
        if !t.direction().has_send() {
            let dir = if t.direction().has_recv() {
                RTCRtpTransceiverDirection::Sendrecv
            } else {
                RTCRtpTransceiverDirection::Sendonly
            };
            t.set_direction(dir).await;
        }
        if !source.start(&self.device_for(source_id)) {
            tracing::warn!(source_id, "camera source failed to start");
        }

        let stop = self.send_stop.clone();
        tokio::spawn(async move {
            // ~30 fps: pull whatever the source has and keep the track alive. Frames
            // are Annex-B H.264 access units; webrtc-rs splits + FU-A-packetizes them.
            let mut ticker = tokio::time::interval(Duration::from_millis(33));
            while !stop.load(Ordering::SeqCst) {
                ticker.tick().await;
                if let Some(h264) = source.poll_frame() {
                    let _ = track
                        .write_sample(&Sample {
                            data: bytes::Bytes::from(h264),
                            duration: Duration::from_millis(33),
                            ..Default::default()
                        })
                        .await;
                }
            }
            source.stop();
        });
        tracing::info!(sender_id, source_id, "attached H.264 camera send track to a video sender");
        Ok(())
    }

    /// Start feeding an audio sender's Opus track from the mic ([`AudioCaptureSource`]),
    /// one 20 ms packet per sample. Once per sender: the track is taken on first
    /// attach, so a repeated `replaceTrack` doesn't start a second pump. The source is
    /// stopped when the peer connection is torn down, not by this loop, so a loop
    /// finishing late can't stop a capture the next call already restarted.
    fn attach_mic(&mut self, sender_id: u64, source_id: &str) -> Result<()> {
        let Some(track) = self.audio_tracks.remove(&sender_id) else {
            tracing::debug!(sender_id, "replaceTrack(audio): no fresh Opus track for this sender");
            return Ok(());
        };
        let Some(source) = self.audio_source.clone() else {
            tracing::warn!("replaceTrack(audio) but no mic source is configured — the call sends no audio");
            return Ok(());
        };
        if !source.start(&self.device_for(source_id)) {
            tracing::warn!(source_id, "mic source failed to start");
        }
        self.mic_track = Some(source_id.to_string());
        self.mic_enabled.store(true, Ordering::SeqCst);
        let enabled = self.mic_enabled.clone();
        let stop = self.send_stop.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(10));
            let mut rtp = OpusPacketizer::new();
            while !stop.load(Ordering::SeqCst) {
                ticker.tick().await;
                while let Some(opus) = source.poll_frame() {
                    // Muted: keep the clock running, send nothing.
                    let frame = if enabled.load(Ordering::Relaxed) { opus.as_slice() } else { &[] };
                    if let Some(packet) = rtp.next(frame) {
                        let _ = track.write_rtp(&packet).await;
                    }
                }
            }
        });
        tracing::info!(sender_id, source_id, "attached the mic (Opus) to the audio sender");
        Ok(())
    }

    /// Stop the mic capture, if one is configured.
    fn stop_mic(&self) {
        if let Some(source) = &self.audio_source {
            source.stop();
        }
    }

    /// `RTCRtpTransceiver.setDirection`.
    pub async fn set_transceiver_direction(&mut self, id: u64, direction: &str) -> Result<()> {
        if let Some(t) = self.transceivers.get(&id) {
            // Camera send is disabled: never let Teams flip a VIDEO transceiver into a
            // send direction (see add_transceiver) — a send video m-line Plaza rejects
            // aborts the answer. Clamp video to receive-only; honor audio as requested.
            let dir = if t.kind() == RTPCodecType::Video {
                RTCRtpTransceiverDirection::Recvonly
            } else {
                parse_direction(direction)
            };
            t.set_direction(dir).await;
        }
        Ok(())
    }

    /// `RTCPeerConnection.createOffer` — returns the SDP offer string.
    pub async fn create_offer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        let offer = pc.create_offer(None).await?;
        let offer_sdp = offer.sdp.clone();
        // Remember the offer's ICE ufrag so trickled candidates carry the
        // `usernameFragment` Teams expects (webrtc-rs omits it from `to_json`).
        if let Some(ufrag) = ice_ufrag_of(&offer_sdp) {
            if let Ok(mut u) = self.ice_ufrag.lock() {
                *u = Some(ufrag);
            }
        }
        // Apply the offer as our local description NOW (this starts ICE gathering) and
        // WAIT for gathering to finish, so the SDP we return to Teams already carries its
        // ICE candidates inline (a real `c=` address + `a=candidate:` lines), exactly like
        // the real add-in's offer. Teams' media server (Plaza) accepts the non-bundle-owner
        // m-lines — the recv video grid and the SCTP data channel — only when the offer
        // already proves connectivity; a candidate-less offer that trickles afterward gets
        // only audio (the bundle owner) accepted while video + data are rejected, and Teams
        // tears the call down (~100 ms after the answer) before the trickled candidates can
        // upgrade them. `set_local_offer` becomes a no-op afterward (already in
        // have-local-offer). Bounded so a slow/hanging TURN allocation can't stall call
        // setup — we return whatever gathered by the deadline.
        let mut gather_done = pc.gathering_complete_promise().await;
        pc.set_local_description(offer).await?;
        let _ = tokio::time::timeout(Duration::from_secs(4), gather_done.recv()).await;
        let gathered = pc.local_description().await.map(|d| d.sdp).unwrap_or(offer_sdp);
        Ok(gathered)
    }

    /// `RTCPeerConnection.setLocalDescription(offer)`.
    pub async fn set_local_offer(&mut self, sdp: &str) -> Result<()> {
        let pc = self.pc()?;
        // `create_offer` already applied the local description and gathered its ICE
        // candidates, so Teams' subsequent setLocalDescription is a redundant
        // re-application — skip it (re-setting a local offer in have-local-offer errors).
        // The transceiver mids the caller returns to Teams are already assigned.
        if pc.signaling_state() == RTCSignalingState::HaveLocalOffer {
            return Ok(());
        }
        // Capture the offer's ICE ufrag so trickled candidates can carry the
        // `usernameFragment` Teams expects (webrtc-rs omits it from `to_json`).
        if let Some(ufrag) = ice_ufrag_of(sdp) {
            if let Ok(mut u) = self.ice_ufrag.lock() {
                *u = Some(ufrag);
            }
        }
        let desc = RTCSessionDescription::offer(sdp.to_string())?;
        pc.set_local_description(desc).await?;
        Ok(())
    }

    /// `RTCPeerConnection.setRemoteDescription(answer)`.
    pub async fn set_remote_answer(&mut self, sdp: &str) -> Result<()> {
        let pc = self.pc()?;
        let desc = RTCSessionDescription::answer(sdp.to_string())?;
        pc.set_remote_description(desc).await?;
        Ok(())
    }

    /// `RTCPeerConnection.setRemoteDescription(offer)` — when acting as answerer.
    /// webrtc-rs synthesizes recv transceivers to match the offer's m-lines.
    pub async fn set_remote_offer(&mut self, sdp: &str) -> Result<()> {
        let pc = self.pc()?;
        let desc = RTCSessionDescription::offer(sdp.to_string())?;
        pc.set_remote_description(desc).await?;
        Ok(())
    }

    /// `RTCPeerConnection.createAnswer` — returns the SDP answer string.
    pub async fn create_answer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        let answer = pc.create_answer(None).await?;
        Ok(answer.sdp)
    }

    /// Create the answer *and* apply it as our local description (the answerer
    /// role: `set_local_description(create_answer())`). Returns the SDP.
    pub async fn create_and_set_answer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        let answer = pc.create_answer(None).await?;
        let sdp = answer.sdp.clone();
        pc.set_local_description(answer).await?;
        Ok(sdp)
    }

    /// Block until ICE gathering finishes, so `local_description()` then carries
    /// the full candidate set (non-trickle exchange).
    pub async fn wait_ice_gathering(&self) -> Result<()> {
        if let Some(pc) = &self.pc {
            let mut done = pc.gathering_complete_promise().await;
            let _ = done.recv().await;
        }
        Ok(())
    }

    /// The ICE candidates gathered so far (each a trickle `candidate` object).
    pub fn local_candidates(&self) -> Vec<Value> {
        self.candidates.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Take and clear the ICE candidates gathered since the last drain — the
    /// dispatcher turns each into a trickle `icecandidate` event.
    pub fn take_candidates(&mut self) -> Vec<Value> {
        self.candidates.lock().map(|mut c| std::mem::take(&mut *c)).unwrap_or_default()
    }

    /// The transceivers' post-`setLocalDescription` state: each `rpcObjectId` (as
    /// Teams assigned it at `addTransceiver`) with its now-assigned `mid`, direction
    /// and kind, ordered by mid. Teams reads this back from the `setLocalDescription`
    /// result to map the transceivers/senders it created onto the offer's m-lines;
    /// returning a bare ack (no mids) leaves it unable to correlate the streams and it
    /// tears the call down ~100 ms later without ever answering.
    pub fn transceiver_states(&self) -> Vec<Value> {
        let mut ordered: Vec<(i64, Value)> = self
            .transceivers
            .iter()
            .map(|(id, t)| {
                let mid = t.mid().map(|m| m.to_string());
                // Order by numeric mid so the list matches the m-line order; a
                // not-yet-assigned mid sorts last.
                let order = mid.as_deref().and_then(|m| m.parse::<i64>().ok()).unwrap_or(i64::MAX);
                (
                    order,
                    serde_json::json!({
                        "rpcObjectId": id,
                        "direction": t.direction().to_string(),
                        // No `currentDirection`: the add-in doesn't report one, and
                        // webrtc-rs leaves it "Unspecified" (not a W3C value) for an
                        // answer m-line without a direction attribute — Plaza's audio.
                        "mid": mid,
                        "kind": t.kind().to_string(),
                    }),
                )
            })
            .collect();
        ordered.sort_by_key(|(o, _)| *o);
        ordered.into_iter().map(|(_, v)| v).collect()
    }

    /// The current local description SDP (grows as ICE candidates are gathered);
    /// this is what a local-description-update event carries.
    pub async fn local_description(&self) -> Option<String> {
        match &self.pc {
            Some(pc) => pc.local_description().await.map(|d| d.sdp),
            None => None,
        }
    }

    /// Parse the session's ICE servers and, if a [`TurnResolver`] is installed,
    /// rewrite each anycast `turn:…?transport=udp` URL to its unicast backend
    /// (following the `300 Try Alternate` webrtc-rs can't) and drop the TCP/TLS
    /// TURN URLs webrtc-rs can't gather. The redirect probe is blocking UDP, so it
    /// runs on a blocking thread to keep the runtime responsive.
    async fn resolve_ice_servers(&self, config: &Value) -> Vec<RTCIceServer> {
        let servers = parse_ice_servers(config);
        let Some(resolver) = self.turn_resolver.clone() else {
            return servers;
        };
        let original = servers.clone();
        tokio::task::spawn_blocking(move || {
            servers
                .into_iter()
                .map(|mut s| {
                    s.urls = rewrite_turn_urls(s.urls, &resolver);
                    s
                })
                .collect()
        })
        .await
        .unwrap_or(original)
    }

    fn pc(&self) -> Result<Arc<RTCPeerConnection>> {
        self.pc.clone().ok_or(EngineError::NoPeerConnection)
    }
}

/// Register the RTP header extensions Teams' media server needs to see in our
/// offer. webrtc-rs's `register_default_codecs()` + `register_default_interceptors()`
/// only give us `transport-cc`; a real libwebrtc endpoint (the Windows add-in) also
/// offers `sdes:mid`, `abs-send-time`, `ssrc-audio-level` and `video-orientation`.
///
/// The critical one is **`sdes:mid`**: Teams builds a single max-bundle peer
/// connection with ~11 m-lines (audio + up to nine recv video + the data channel)
/// all sharing one ICE/DTLS transport, and the media server demultiplexes inbound
/// RTP to the right m-line by the MID header extension. Without `a=extmap … sdes:mid`
/// in the offer it cannot route the nine video streams, so it never produces an
/// answer and Teams tears the call down ~90 ms after `setLocalDescription` — exactly
/// the fast-close-without-`setRemoteDescription` seen against the live server. We
/// register on all directions (`None`) so the recv-only video m-lines carry it too.
fn register_teams_header_extensions(media: &mut MediaEngine) -> Result<()> {
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability;
    use webrtc::sdp::extmap::{ABS_SEND_TIME_URI, AUDIO_LEVEL_URI, SDES_MID_URI, VIDEO_ORIENTATION_URI};

    let mut register = |uri: &str, typ: RTPCodecType| -> Result<()> {
        media.register_header_extension(
            RTCRtpHeaderExtensionCapability { uri: uri.to_owned() },
            typ,
            None,
        )?;
        Ok(())
    };
    // MID + abs-send-time on both media kinds; MID is the bundle-demux requirement.
    for typ in [RTPCodecType::Audio, RTPCodecType::Video] {
        register(SDES_MID_URI, typ)?;
        register(ABS_SEND_TIME_URI, typ)?;
    }
    register(AUDIO_LEVEL_URI, RTPCodecType::Audio)?;
    register(VIDEO_ORIENTATION_URI, RTPCodecType::Video)?;
    // The full VIDEO header-extension set the real add-in offers. Teams' media server
    // (Plaza) rejects a video m-line whose *extension* set it can't route — the same
    // class of gate as `sdes:mid` (without which the server never answers at all). A
    // captured working add-in call advertises all of these on every video m-line;
    // ours advertised only MID/abs-send-time/video-orientation/transport-cc, and Plaza
    // rejected all our video (adding RTX codecs changed the answer by zero bytes, which
    // is what pointed the finger at the extension set rather than the codecs). The
    // `sdes:rtp-stream-id` / `repaired-rtp-stream-id` pair (RID + RTX-repair stream
    // identification) is the most load-bearing; the rest (toffset, playout-delay,
    // video-content-type/-timing, color-space, video-layers-allocation, AV1 dependency
    // descriptor) round out parity. webrtc-rs advertises any registered extension by
    // URI whether or not it processes it, which is all Plaza needs to see. IDs stay
    // within the one-byte 1..=14 space (video carries 13 total, under webrtc-rs's cap).
    for uri in [
        "urn:ietf:params:rtp-hdrext:toffset",
        "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
        "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
        "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
        "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
        "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
    ] {
        register(uri, RTPCodecType::Video)?;
    }
    Ok(())
}

/// Find every remote track `pc`'s receivers set up (polling — tracks appear once
/// the transport is up) and pump each one's RTP into `sink`, announcing it on
/// its first readable packet. Packets whose payload type isn't negotiated are
/// skipped, not fatal. Ends when the peer connection is gone or closed; each
/// track's reader ends when its track does.
async fn read_remote_tracks(pc: Weak<RTCPeerConnection>, sink: Arc<dyn MediaSink>) {
    let mut seen = HashSet::new();
    let mut rtcp_started = false;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Some(pc) = pc.upgrade() else { return };
        if pc.connection_state() == RTCPeerConnectionState::Closed {
            return;
        }
        // Once connected, drain every receiver's and sender's RTCP. webrtc-rs only
        // runs inbound RTCP through the interceptors (the probe that logs the
        // media server's sender reports, among others) when someone reads it.
        if !rtcp_started && pc.connection_state() == RTCPeerConnectionState::Connected {
            rtcp_started = true;
            let pair = pc.sctp().transport().ice_transport().get_selected_candidate_pair().await;
            tracing::info!(pair = ?pair.map(|p| p.to_string()), "ICE selected candidate pair");
            for t in pc.get_transceivers().await {
                let receiver = t.receiver().await;
                tokio::spawn(async move { while receiver.read_rtcp().await.is_ok() {} });
                let sender = t.sender().await;
                tokio::spawn(async move { while sender.read_rtcp().await.is_ok() {} });
            }
        }
        for t in pc.get_transceivers().await {
            for track in t.receiver().await.tracks().await {
                let ssrc = track.ssrc();
                if ssrc == 0 || !seen.insert(ssrc) {
                    continue;
                }
                tokio::spawn(pump_remote_track(track, sink.clone()));
            }
        }
    }
}

/// Read one remote track until it ends, handing each packet to `sink`.
async fn pump_remote_track(track: Arc<TrackRemote>, sink: Arc<dyn MediaSink>) {
    let id = track.id();
    let kind = match track.kind() {
        RTPCodecType::Audio => "audio",
        RTPCodecType::Video => "video",
        _ => "unknown",
    };
    tracing::info!(track = %id, ssrc = track.ssrc(), kind, "reading remote track");
    let (mut delivered, mut skipped) = (0u64, 0u64);
    loop {
        match track.read_rtp().await {
            Ok((packet, _)) => {
                // `read_rtp` re-resolves the codec per packet.
                let codec = track.codec().capability.mime_type;
                delivered += 1;
                if delivered == 1 {
                    tracing::info!(track = %id, ssrc = track.ssrc(), kind, %codec, skipped, "remote track delivering media");
                    sink.on_track(&id, kind, &codec);
                }
                sink.on_rtp(&id, &codec, &packet.payload);
            }
            // A packet whose payload type isn't negotiated: drop just that one.
            Err(webrtc::Error::ErrCodecNotFound) => {
                skipped += 1;
                if skipped.is_power_of_two() {
                    tracing::info!(track = %id, ssrc = track.ssrc(), skipped, delivered, "remote track: skipping packets of an unnegotiated payload type");
                }
            }
            Err(e) => {
                tracing::info!(track = %id, ssrc = track.ssrc(), delivered, skipped, error = %e, "remote track ended");
                break;
            }
        }
    }
}

/// Register comfort noise (`CN`, RFC 3389) for audio. Teams' media server answers
/// the audio m-line with `CN/48000` (alongside Opus with DTX) whether or not the
/// offer carried it, and sends CN during silence — so a call's very first
/// packets are often CN. webrtc-rs only accepts payload types it negotiated,
/// and one unknown packet at the start aborts the whole inbound track (live:
/// `Could not determine PayloadType for SSRC 1000 (codec not found)`, then a
/// silent call). Negotiation matches by mime type, so registering CN lets the
/// answer's CN (whatever its clock/payload type) be negotiated.
fn register_comfort_noise(media: &mut MediaEngine) -> Result<()> {
    media.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: "audio/CN".to_owned(),
                clock_rate: 8000,
                channels: 1,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            payload_type: 13,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    Ok(())
}

/// The ICE [`SettingEngine`] webrtc-rs uses for gathering. Restricts to IPv4/UDP
/// and drops link-local addresses and mDNS candidates — Teams' relays are IPv4
/// and a `169.254.x.x` / `.local` candidate only wastes a check. Without this,
/// gathering spends its budget failing to bind link-local NICs and resolve the
/// TURN host over IPv6 ("No available ipv6 IP address found"), starving the real
/// candidates before Teams tears the call down.
///
/// Also loosens DTLS signature verification: webrtc-rs rejects an RSA key under
/// 2048 bits (and SHA-1) in the server's handshake signature unless told
/// otherwise, and the first live call to reach ICE `connected` died right there
/// with `Failed to start manager dtls: ring::error::Unspecified`. libwebrtc — what
/// the real add-in runs — accepts these; the server stays pinned by the
/// `a=fingerprint` Teams hands us in the answer. (That flag alone wasn't enough:
/// webrtc-rs also picked the verifier from the hash rather than the key, which
/// the vendored `dtls` crate fixes — see the workspace `[patch.crates-io]`.)
///
/// The SRTP replay window is libwebrtc's 1024 packets rather than webrtc-rs's
/// 64: a media server that switches the stream it forwards on one SSRC can jump
/// the sequence number, and a small window silently drops everything after the
/// jump (only an INFO-level `srtp … duplicated` line says so).
fn ice_setting_engine() -> SettingEngine {
    let mut se = SettingEngine::default();
    se.allow_insecure_verification_algorithm(true);
    se.set_srtp_replay_protection_window(1024);
    // Teams' media server sends its RTCP in pairs that reuse one SRTCP index
    // under the same sender SSRC; the per-SSRC replay check discarded every
    // second one (`srtcp ssrc=1801 index=0: duplicated`).
    se.disable_srtcp_replay_protection(true);
    se.set_network_types(vec![NetworkType::Udp4]);
    se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    se.set_ip_filter(Box::new(|ip: IpAddr| match ip {
        // Keep routable IPv4; drop APIPA/link-local, unspecified and broadcast.
        IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_unspecified() && !v4.is_broadcast(),
        // IPv6 is already excluded by the network types; belt and suspenders.
        IpAddr::V6(_) => false,
    }));
    se
}

/// Rewrite one ICE server's URLs for what webrtc-rs 0.17 can actually gather:
/// resolve anycast `turn:…?transport=udp` to the unicast backend past a `300 Try
/// Alternate`, keep `stun:` as-is, and drop `turn(s):…?transport=tcp` / `turns:`
/// (webrtc-rs logs "Unable to handle URL" and never gathers them). Runs on a
/// blocking thread — the resolver does synchronous UDP.
fn rewrite_turn_urls(urls: Vec<String>, resolver: &Arc<dyn TurnResolver>) -> Vec<String> {
    let mut out = Vec::with_capacity(urls.len());
    for url in urls {
        let Some((scheme, rest)) = url.split_once(':') else {
            out.push(url);
            continue;
        };
        match scheme {
            "stun" | "stuns" => {
                out.push(url);
                continue;
            }
            "turns" => {
                tracing::debug!(%url, "dropping turns: URL — webrtc-rs can't gather TURN over TLS");
                continue;
            }
            "turn" => {}
            _ => {
                out.push(url);
                continue;
            }
        }
        // rest = "host:port" optionally followed by "?transport=udp|tcp".
        let (host_port, transport) = match rest.split_once('?') {
            Some((hp, q)) => (hp, q.strip_prefix("transport=").unwrap_or(q)),
            None => (rest, "udp"),
        };
        if transport.eq_ignore_ascii_case("tcp") {
            tracing::debug!(%url, "dropping turn ?transport=tcp URL — webrtc-rs can't gather TURN over TCP");
            continue;
        }
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().unwrap_or(3478)),
            None => (host_port, 3478),
        };
        match resolver.resolve_alternate(host, port) {
            Some(alt) => {
                let rewritten = format!("turn:{alt}?transport=udp");
                tracing::info!(
                    from = %url,
                    to = %rewritten,
                    "rewrote Teams TURN URL past the 300 Try Alternate redirect (webrtc-rs can allocate the unicast backend directly)"
                );
                out.push(rewritten);
            }
            // No redirect (or probe failed): hand webrtc-rs the original URL.
            None => out.push(url),
        }
    }
    out
}

/// Build the trickle-ICE `candidate` object in the shape Teams' `addIceCandidate`
/// expects: the standard `{candidate, sdpMid, sdpMLineIndex, usernameFragment}`
/// plus the discrete fields the real add-in also sends. webrtc-rs's `to_json()`
/// hardcodes an empty `sdpMid` and a *null* `usernameFragment` — which Teams
/// rejected, tearing the whole call down on the first such candidate — so we fill
/// them from the candidate and the offer's ufrag. With max-bundle every candidate
/// rides the bundle transport, so `sdpMid` is "0" / `sdpMLineIndex` 0.
fn candidate_event(c: &RTCIceCandidate, ufrag: Option<String>) -> Option<Value> {
    let init = c.to_json().ok()?;
    Some(serde_json::json!({
        "candidate": init.candidate,
        "sdp_mid": "0",
        "sdp_mline_index": 0,
        "foundation": c.foundation,
        "component": "rtp",
        "protocol": c.protocol.to_string(),
        "priority": c.priority,
        "address": c.address,
        "port": c.port,
        "type": c.typ.to_string(),
        "usernameFragment": ufrag,
    }))
}

/// Extract `a=ice-ufrag:<value>` from an SDP (with BUNDLE every m-line repeats the
/// same ufrag, so the first is the session's).
fn ice_ufrag_of(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix("a=ice-ufrag:"))
        .map(|s| s.trim().to_string())
}

fn parse_direction(d: &str) -> RTCRtpTransceiverDirection {
    match d {
        "sendrecv" => RTCRtpTransceiverDirection::Sendrecv,
        "sendonly" => RTCRtpTransceiverDirection::Sendonly,
        "recvonly" => RTCRtpTransceiverDirection::Recvonly,
        _ => RTCRtpTransceiverDirection::Inactive,
    }
}

fn parse_ice_servers(config: &Value) -> Vec<RTCIceServer> {
    let Some(servers) = config.get("iceServers").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s
                .get("urls")
                .and_then(|u| u.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            username: s.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            credential: s.get("credential").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ice_ufrag_is_extracted_from_the_offer() {
        let sdp = "v=0\r\na=group:BUNDLE 0\r\na=ice-ufrag:zpqOlNbBCHMZcHyY\r\na=ice-pwd:secret\r\nm=audio 9 x\r\na=ice-ufrag:zpqOlNbBCHMZcHyY\r\n";
        assert_eq!(ice_ufrag_of(sdp).as_deref(), Some("zpqOlNbBCHMZcHyY"));
        assert_eq!(ice_ufrag_of("v=0\r\nm=audio 9 x\r\n"), None);
    }

    #[tokio::test]
    async fn generates_an_offer_with_default_codecs() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("audio", "sendrecv", 1, 101, &[]).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, &[]).await.expect("video");
        let offer = engine.create_offer().await.expect("offer");
        assert!(offer.starts_with("v=0"), "not SDP");
        assert!(offer.contains("m=audio"), "no audio m-line");
        assert!(offer.contains("m=video"), "no video m-line");
        assert!(offer.to_lowercase().contains("opus"), "no opus");
    }

    /// Audio offers `sendrecv` with a real **Opus** send track (the mic attaches to
    /// it), while video carries NO send track. webrtc-rs auto-attaches a track fixed
    /// to its first codec of the kind for any send-capable direction; for video that
    /// is VP8, which can't bind when Teams' answer negotiates H264, and
    /// `set_remote_description(answer)` fails with "codec is not supported by
    /// remote", tearing the just-answered call down. For audio the first codec is
    /// Opus — exactly what the answer negotiates — so a send track is safe there.
    #[tokio::test]
    async fn audio_offers_an_opus_send_track_and_video_none() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("audio", "sendrecv", 1, 101, &[]).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, &[]).await.expect("video");
        let offer = engine.create_offer().await.expect("offer");

        // Direction honored: the audio m-line is sendrecv, with an outbound stream.
        let audio_block = offer.split("m=video").next().unwrap_or(&offer);
        assert!(audio_block.contains("a=sendrecv"), "audio m-line lost its sendrecv direction:\n{offer}");
        assert!(audio_block.contains("a=ssrc:"), "audio m-line has no send stream:\n{offer}");
        // Comfort noise is negotiable, so Teams' CN packets don't kill the track.
        assert!(audio_block.contains("CN/8000"), "audio m-line lacks comfort noise:\n{offer}");
        // No phantom (VP8) send track on video.
        let video_block = offer.split("m=video").nth(1).unwrap_or("");
        assert!(!video_block.contains("a=ssrc:"), "video has a phantom send track:\n{offer}");
        assert!(
            engine.transceiver_states().iter().any(|t| t
                .get("direction")
                .and_then(Value::as_str)
                == Some("sendrecv")),
            "no sendrecv transceiver reported"
        );
    }

    /// Teams builds one max-bundle peer connection with an audio m-line and several
    /// recv-only video m-lines all on a single transport; the media server routes
    /// inbound RTP to the right m-line by the MID header extension. Our offer must
    /// therefore advertise `sdes:mid` on every m-line, or the server can't demux the
    /// video streams, never answers, and Teams closes the call. webrtc-rs omits it by
    /// default, so `register_teams_header_extensions` adds it — guard that here.
    #[tokio::test]
    async fn offer_advertises_the_mid_header_extension() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("audio", "sendrecv", 1, 101, &[]).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, &[]).await.expect("video");
        engine.add_transceiver("video", "recvonly", 3, 103, &[]).await.expect("video recvonly");
        let offer = engine.create_offer().await.expect("offer");

        let mid_lines = offer
            .lines()
            .filter(|l| l.contains("urn:ietf:params:rtp-hdrext:sdes:mid"))
            .count();
        // One per m-line (audio + 2 video) — the recv-only line needs it too.
        assert!(mid_lines >= 3, "sdes:mid not advertised on every m-line: {mid_lines} lines\n{offer}");

        // The video header extensions Plaza gates acceptance on (see
        // register_teams_header_extensions): each appears on both video m-lines.
        for uri in [
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
            "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
        ] {
            let n = offer.lines().filter(|l| l.contains(uri)).count();
            assert!(n >= 2, "{uri} not advertised on every video m-line: {n}\n{offer}");
        }
    }

    /// Teams opens a "main-channel" data channel before `createOffer` and requires
    /// the offer to negotiate it. When we merely acked `createDataChannel` without
    /// opening one, the offer had no `m=application` section and Teams closed the
    /// data channel and then the peer connection without ever answering — the live
    /// call-setup failure. Guard the shape of the offer so that can't regress.
    #[tokio::test]
    async fn data_channel_gives_the_offer_an_application_section() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        // Same order Teams uses: data channel first, then the media transceivers.
        engine.create_data_channel("main-channel", 12).await.expect("data channel");
        engine.add_transceiver("audio", "sendrecv", 1, 101, &[]).await.expect("audio");
        engine.add_transceiver("video", "recvonly", 2, 102, &[]).await.expect("video");

        let offer = engine.create_offer().await.expect("offer");
        assert!(offer.contains("m=application"), "offer lacks the data-channel m-line");
        assert!(offer.contains("webrtc-datachannel"), "offer lacks webrtc-datachannel");
        assert!(offer.contains("m=audio"), "offer lacks audio");
        assert!(offer.contains("m=video"), "offer lacks video");
    }

    /// A synthetic camera: emits a minimal Annex-B H.264 access unit each poll and
    /// records that it was started. Stands in for the Media-Foundation-backed source
    /// so the send path is testable offline.
    #[derive(Default)]
    struct TestCamera {
        started: std::sync::atomic::AtomicBool,
    }
    impl VideoCaptureSource for TestCamera {
        fn start(&self, _source_id: &str) -> bool {
            self.started.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        }
        fn poll_frame(&self) -> Option<Vec<u8>> {
            // Start code + a tiny NAL so webrtc-rs's H.264 payloader has something to
            // packetize.
            Some(vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0xab, 0xcd])
        }
        fn stop(&self) {}
    }

    /// The camera-send unblock: when Teams `replaceTrack`s the camera onto a video
    /// sender, the offer's video m-line must gain a real send configuration (`a=ssrc`)
    /// so Plaza accepts the video (and the bundled data channel with it). A track-less
    /// sendrecv video m-line — what we offered before — has no ssrc and Plaza rejects
    /// it. Drive addTransceiver→replaceTrack and assert the offer changes.
    #[tokio::test]
    async fn replace_track_keeps_video_receive_only_camera_send_disabled() {
        // Camera video send is deliberately disabled: Plaza rejects our video m-lines,
        // and a send track bound to a rejected m-line aborts the whole answer with
        // "codec is not supported by remote". So even when Teams marks the video
        // transceiver `sendEncodings` and replaceTracks the camera onto it, the engine
        // keeps video receive-only and treats replaceTrack as a no-op. This test guards
        // that invariant (re-enable together when video *acceptance* is solved).
        let mut engine = WebrtcEngine::new();
        let cam = Arc::new(TestCamera::default());
        engine.set_video_source(cam.clone());
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("audio", "sendrecv", 1, 101, &[]).await.expect("audio");
        // Even with `sendEncodings` (wants_send = true), the camera transceiver is
        // created receive-only.
        engine.add_transceiver("video", "sendrecv", 2, 102, &[crate::engine_api::SendEncoding { rid: "1".into(), scale_resolution_down_by: 1.0 }]).await.expect("video");
        // Teams also flips it via setDirection — which must stay clamped to recvonly.
        engine.set_transceiver_direction(2, "sendrecv").await.expect("setDirection");

        engine.replace_track(102, "rdpio-videoinput-0").await.expect("replaceTrack");
        assert!(
            !cam.started.load(std::sync::atomic::Ordering::SeqCst),
            "camera source must NOT start — camera send is disabled"
        );

        let after = engine.create_offer().await.expect("offer after replaceTrack");
        // Video stays receive-only: no send stream (`a=ssrc`) on the video m-line.
        let video_block = after.split("m=video").nth(1).unwrap_or("");
        assert!(
            !video_block.contains("a=ssrc:"),
            "video m-line must have no send config (camera send disabled):\n{after}"
        );
        assert!(video_block.contains("a=recvonly"), "video m-line must be recvonly:\n{after}");
        // Audio is unaffected — it still honors Teams' sendrecv request.
        assert!(after.contains("H264"), "offer lost H264 (recv codecs):\n{after}");
    }

    /// A `replaceTrack` on an unknown sender, or with no camera source configured, must
    /// be a harmless no-op (Teams still gets its ack) rather than an error that fails
    /// the call.
    #[tokio::test]
    async fn replace_track_is_a_noop_without_a_source_or_sender() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("video", "sendrecv", 2, 102, &[]).await.expect("video");
        // No source configured → Ok, no send config appears.
        engine.replace_track(102, "cam").await.expect("noop ok");
        // Unknown sender id → Ok.
        engine.replace_track(999, "cam").await.expect("unknown sender ok");
        let offer = engine.create_offer().await.expect("offer");
        assert!(!offer.contains("a=ssrc:"), "send config appeared without a source:\n{offer}");
    }

    /// DTX as libwebrtc sends it: silent (≤2 byte) and muted frames are skipped,
    /// sequence numbers stay contiguous, the timestamp keeps the 20 ms clock
    /// across the gap, and the talkspurt's first packet carries the marker.
    #[test]
    fn opus_packetizer_skips_dtx_frames_without_a_sequence_gap() {
        let mut rtp = OpusPacketizer::new();
        let speech = [0x78u8, 1, 2, 3, 4];
        let first = rtp.next(&speech).expect("speech is sent");
        assert!(first.header.marker, "a stream opens with a talkspurt");
        let second = rtp.next(&speech).expect("speech is sent");
        assert!(!second.header.marker);
        assert_eq!(second.header.sequence_number, first.header.sequence_number.wrapping_add(1));
        assert_eq!(second.header.timestamp, first.header.timestamp.wrapping_add(960));

        assert!(rtp.next(&[0x78]).is_none(), "1-byte DTX frame is not sent");
        assert!(rtp.next(&[0x78, 0x00]).is_none(), "2-byte DTX frame is not sent");
        assert!(rtp.next(&[]).is_none(), "muted frame is not sent");

        let resumed = rtp.next(&speech).expect("speech is sent");
        assert!(resumed.header.marker, "speech after silence starts a talkspurt");
        assert_eq!(resumed.header.sequence_number, second.header.sequence_number.wrapping_add(1));
        assert_eq!(resumed.header.timestamp, second.header.timestamp.wrapping_add(4 * 960));
        assert_eq!(&resumed.payload[..], &speech);
    }

    /// Teams' mute (`onEnabledAttributeChanged [false]`) only applies to the
    /// track feeding the mic sender.
    #[test]
    fn only_the_mic_track_mutes_the_mic() {
        let mut engine = WebrtcEngine::new();
        engine.mic_track = Some("60".into());
        engine.set_track_enabled("52", false);
        assert!(engine.mic_enabled.load(Ordering::SeqCst), "another track's state is ignored");
        engine.set_track_enabled("60", false);
        assert!(!engine.mic_enabled.load(Ordering::SeqCst));
        engine.set_track_enabled("60", true);
        assert!(engine.mic_enabled.load(Ordering::SeqCst));
    }

    /// The send half for audio, end to end: our engine as the offerer (Teams'
    /// shape — an audio transceiver created `inactive`, flipped to `sendrecv`, then
    /// `replaceTrack`ed onto the mic) pumps the mic source's Opus packets over real
    /// DTLS/SRTP to a plain webrtc-rs answerer, which must receive them as audio.
    #[tokio::test]
    async fn mic_audio_reaches_the_peer() {
        use std::sync::atomic::AtomicUsize;
        use webrtc::api::interceptor_registry::register_default_interceptors;
        use webrtc::api::media_engine::MediaEngine;
        use webrtc::api::APIBuilder;
        use webrtc::interceptor::registry::Registry;
        use webrtc::peer_connection::configuration::RTCConfiguration;
        use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

        /// One placeholder Opus packet every other poll (the pump drains until
        /// `None`, so a source that never runs dry would spin it).
        #[derive(Default)]
        struct TestMic {
            started: AtomicBool,
            polls: AtomicUsize,
        }
        impl AudioCaptureSource for TestMic {
            fn start(&self, _source_id: &str) -> bool {
                self.started.store(true, Ordering::SeqCst);
                true
            }
            fn poll_frame(&self) -> Option<Vec<u8>> {
                let odd = self.polls.fetch_add(1, Ordering::SeqCst) % 2 == 0;
                (self.started.load(Ordering::SeqCst) && odd).then(|| vec![0xF8, 0xFF, 0xFE])
            }
            fn stop(&self) {
                self.started.store(false, Ordering::SeqCst);
            }
        }

        let mic = Arc::new(TestMic::default());
        let mut engine = WebrtcEngine::new();
        engine.set_audio_source(mic.clone());
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("pc");
        engine.add_transceiver("audio", "inactive", 1, 101, &[]).await.expect("audio");
        engine.set_transceiver_direction(1, "sendrecv").await.expect("setDirection");
        engine.replace_track(101, "rdpio-audioinput-0").await.expect("replaceTrack");
        assert!(mic.started.load(Ordering::SeqCst), "mic source must start on replaceTrack");

        let offer = engine.create_offer().await.expect("offer");
        let audio = offer.split("m=audio").nth(1).unwrap_or("");
        assert!(audio.contains("a=ssrc:"), "audio m-line must carry a send stream:\n{offer}");
        assert!(audio.contains("a=sendrecv"), "audio m-line must be sendrecv:\n{offer}");
        engine.set_local_offer(&offer).await.expect("set local");
        engine.wait_ice_gathering().await.expect("gather");
        let offer_sdp = engine.local_description().await.expect("local desc");

        // The far end: a plain webrtc-rs answerer counting inbound audio RTP.
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        let peer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let packets = Arc::new(AtomicUsize::new(0));
        let counted = packets.clone();
        peer.on_track(Box::new(move |track, _, _| {
            let counted = counted.clone();
            Box::pin(async move {
                assert_eq!(track.kind(), RTPCodecType::Audio);
                tokio::spawn(async move {
                    while track.read_rtp().await.is_ok() {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                });
            })
        }));
        peer.set_remote_description(RTCSessionDescription::offer(offer_sdp).unwrap())
            .await
            .expect("peer accepts offer");
        let answer = peer.create_answer(None).await.unwrap();
        peer.set_local_description(answer).await.unwrap();
        {
            let mut g = peer.gathering_complete_promise().await;
            let _ = g.recv().await;
        }
        let answer_sdp = peer.local_description().await.unwrap().sdp;
        engine.set_remote_answer(&answer_sdp).await.expect("engine accepts answer");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while packets.load(Ordering::SeqCst) < 5 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let got = packets.load(Ordering::SeqCst);
        engine.close_peer_connection().await.expect("close");
        let _ = peer.close().await;
        assert!(got >= 5, "peer received only {got} audio packets from the mic track");
        assert!(!mic.started.load(Ordering::SeqCst), "closing the call must stop the mic");
    }

    /// Receiving call audio when the answer's audio m-line carries NO direction
    /// attribute — the shape Teams' media server answers with (live capture). Our
    /// engine offers Teams-style (audio `inactive` → `sendrecv`); a plain
    /// webrtc-rs answerer sends Opus; its answer is munged to drop the audio
    /// direction. The sink must receive that audio.
    #[tokio::test]
    async fn receives_audio_when_the_answer_omits_the_direction() {
        use std::sync::atomic::AtomicUsize;
        use webrtc::api::interceptor_registry::register_default_interceptors;
        use webrtc::api::media_engine::MediaEngine;
        use webrtc::api::APIBuilder;
        use webrtc::interceptor::registry::Registry;
        use webrtc::peer_connection::configuration::RTCConfiguration;
        use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

        #[derive(Default)]
        struct Count {
            audio: AtomicUsize,
        }
        impl MediaSink for Count {
            fn on_track(&self, _: &str, _: &str, _: &str) {}
            fn on_rtp(&self, _: &str, codec: &str, _: &[u8]) {
                if codec.eq_ignore_ascii_case(MIME_TYPE_OPUS) {
                    self.audio.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        let sink = Arc::new(Count::default());
        let mut engine = WebrtcEngine::new();
        engine.set_sink(sink.clone());
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("pc");
        engine.add_transceiver("audio", "inactive", 1, 101, &[]).await.expect("audio");
        engine.set_transceiver_direction(1, "sendrecv").await.expect("setDirection");
        let offer = engine.create_offer().await.expect("offer");
        engine.set_local_offer(&offer).await.expect("set local");
        engine.wait_ice_gathering().await.expect("gather");
        let offer_sdp = engine.local_description().await.expect("local desc");

        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        let peer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let voice = Arc::new(TrackLocalStaticSample::new(opus_capability(), "a".into(), "peer".into()));
        // Added before the offer is applied, the track lands on the offered audio
        // m-line.
        peer.add_track(voice.clone() as Arc<dyn TrackLocal + Send + Sync>).await.unwrap();
        peer.set_remote_description(RTCSessionDescription::offer(offer_sdp).unwrap())
            .await
            .expect("peer accepts offer");
        let answer = peer.create_answer(None).await.unwrap();
        peer.set_local_description(answer).await.unwrap();
        {
            let mut g = peer.gathering_complete_promise().await;
            let _ = g.recv().await;
        }
        let answer_sdp = peer.local_description().await.unwrap().sdp;
        // Drop the audio section's direction attribute, as Teams' answer does.
        let munged: String = answer_sdp
            .split_inclusive("\r\n")
            .filter(|l| !matches!(l.trim_end(), "a=sendrecv" | "a=sendonly" | "a=recvonly"))
            .collect();
        engine.set_remote_answer(&munged).await.expect("engine accepts answer");

        let writer = {
            let voice = voice.clone();
            tokio::spawn(async move {
                for _ in 0..300 {
                    let _ = voice
                        .write_sample(&Sample {
                            data: bytes::Bytes::from_static(&[0xF8, 0xFF, 0xFE]),
                            duration: Duration::from_millis(20),
                            ..Default::default()
                        })
                        .await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while sink.audio.load(Ordering::SeqCst) < 10 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        writer.abort();
        let got = sink.audio.load(Ordering::SeqCst);
        engine.close_peer_connection().await.expect("close");
        let _ = peer.close().await;
        assert!(got >= 10, "sink received only {got} Opus packets");
    }

    /// Teams' media-control protocol path, end to end: our engine opens Teams'
    /// "main-channel" as the offerer; a plain webrtc-rs answerer echoes every
    /// message. The engine must report the connection coming up, the channel's
    /// `open`, and the echo of what `send_data` sent — the events Teams waits on.
    #[tokio::test]
    async fn data_channel_open_send_and_receive_are_reported() {
        use webrtc::api::interceptor_registry::register_default_interceptors;
        use webrtc::api::media_engine::MediaEngine;
        use webrtc::api::APIBuilder;
        use webrtc::interceptor::registry::Registry;
        use webrtc::peer_connection::configuration::RTCConfiguration;
        use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("pc");
        engine.create_data_channel("main-channel", 12).await.expect("dc");
        let offer = engine.create_offer().await.expect("offer");
        engine.set_local_offer(&offer).await.expect("set local");
        engine.wait_ice_gathering().await.expect("gather");
        let offer_sdp = engine.local_description().await.expect("local desc");

        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        let peer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.unwrap());
        peer.on_data_channel(Box::new(|dc: Arc<RTCDataChannel>| {
            Box::pin(async move {
                let echo = dc.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let echo = echo.clone();
                    Box::pin(async move {
                        let _ = echo.send(&msg.data).await;
                    })
                }));
            })
        }));
        peer.set_remote_description(RTCSessionDescription::offer(offer_sdp).unwrap())
            .await
            .expect("peer accepts offer");
        let answer = peer.create_answer(None).await.unwrap();
        peer.set_local_description(answer).await.unwrap();
        {
            let mut g = peer.gathering_complete_promise().await;
            let _ = g.recv().await;
        }
        let answer_sdp = peer.local_description().await.unwrap().sdp;
        engine.set_remote_answer(&answer_sdp).await.expect("engine accepts answer");

        let heartbeat = br#"[{"type":"heartbeat","state":0}]"#.to_vec();
        let mut events = Vec::new();
        let mut sent = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            events.extend(engine.take_events());
            let open = events.iter().any(|e| matches!(e, PeerEvent::ChannelOpen { object_id: 12, .. }));
            if open && !sent {
                engine.send_data(12, heartbeat.clone()).await.expect("send");
                sent = true;
            }
            if events.iter().any(|e| matches!(e, PeerEvent::ChannelMessage { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // While connected, getStats must look like libwebrtc's: a connected DTLS
        // transport naming its selected (succeeded) candidate pair, with
        // integer microsecond timestamps.
        let report = engine.stats_report().await;
        let stats = report["stats"].as_array().expect("stats list");
        let transport = stats
            .iter()
            .find(|s| s["type"] == "transport")
            .unwrap_or_else(|| panic!("no transport stats: {report}"));
        assert_eq!(transport["dtlsState"], "connected", "{transport}");
        let pair_id = transport["selectedCandidatePairId"].as_str().expect("selected pair");
        assert!(
            stats.iter().any(|s| s["type"] == "candidate-pair" && s["id"] == pair_id),
            "selected pair {pair_id} not in the report"
        );
        assert!(transport["timestamp"].as_i64().unwrap_or(0) > 1_000_000_000_000_000);
        assert!(stats.iter().any(|s| s["type"] == "data-channel"), "no data-channel stats");

        engine.close_peer_connection().await.expect("close");
        let _ = peer.close().await;

        assert!(
            events.contains(&PeerEvent::State { event: "connectionstatechange", state: "connected".into() }),
            "no connected state reported: {events:?}"
        );
        assert!(sent, "the data channel never opened: {events:?}");
        let echoed = events.iter().find_map(|e| match e {
            PeerEvent::ChannelMessage { object_id: 12, data, .. } => Some(data.clone()),
            _ => None,
        });
        assert_eq!(echoed, Some(heartbeat), "the echo didn't come back as a message event");
    }

    /// End-to-end media path: a raw webrtc-rs peer sends a VP8 track to our
    /// engine (acting as the answerer/receiver), and the engine delivers the RTP
    /// to a [`MediaSink`]. Proves `on_track` + the read loop over real SRTP, fully
    /// offline — the receive half of what a live optimized call needs.
    #[tokio::test]
    async fn loopback_media_reaches_the_sink() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use webrtc::api::interceptor_registry::register_default_interceptors;
        use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_VP8};
        use webrtc::api::APIBuilder;
        use webrtc::interceptor::registry::Registry;
        use webrtc::media::Sample;
        use webrtc::peer_connection::configuration::RTCConfiguration;
        use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
        use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
        use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
        use webrtc::track::track_local::TrackLocal;

        #[derive(Default)]
        struct CountSink {
            tracks: AtomicUsize,
            packets: AtomicUsize,
        }
        impl MediaSink for CountSink {
            fn on_track(&self, _id: &str, _kind: &str, _codec: &str) {
                self.tracks.fetch_add(1, Ordering::SeqCst);
            }
            fn on_rtp(&self, _id: &str, _codec: &str, _payload: &[u8]) {
                self.packets.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountSink::default());

        // Receiver: our engine, wired to the sink.
        let mut recv = WebrtcEngine::new();
        recv.set_sink(sink.clone());
        recv.create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("recv pc");

        // Sender: a raw webrtc-rs peer with a VP8 track.
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        let send_pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.unwrap());
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability { mime_type: MIME_TYPE_VP8.to_owned(), ..Default::default() },
            "video".to_owned(),
            "loopback".to_owned(),
        ));
        send_pc
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .unwrap();

        // Offer (sender) → answer (engine), full SDP with candidates each way.
        let offer = send_pc.create_offer(None).await.unwrap();
        send_pc.set_local_description(offer).await.unwrap();
        {
            let mut g = send_pc.gathering_complete_promise().await;
            let _ = g.recv().await;
        }
        let offer_sdp = send_pc.local_description().await.unwrap().sdp;

        recv.set_remote_offer(&offer_sdp).await.expect("recv accepts offer");
        recv.create_and_set_answer().await.expect("answer");
        recv.wait_ice_gathering().await.unwrap();
        let answer_sdp = recv.local_description().await.expect("recv local desc");

        send_pc
            .set_remote_description(RTCSessionDescription::answer(answer_sdp).unwrap())
            .await
            .expect("sender accepts answer");

        // Push VP8 samples until media arrives or we give up.
        let writer = {
            let track = track.clone();
            tokio::spawn(async move {
                for _ in 0..250 {
                    let _ = track
                        .write_sample(&Sample {
                            data: bytes::Bytes::from_static(&[0u8; 64]),
                            duration: Duration::from_millis(20),
                            ..Default::default()
                        })
                        .await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };

        let mut got_media = false;
        for _ in 0..100 {
            if sink.packets.load(Ordering::SeqCst) > 0 {
                got_media = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        writer.abort();

        assert!(sink.tracks.load(Ordering::SeqCst) >= 1, "on_track never fired");
        assert!(got_media, "no RTP packets reached the sink");
    }
}
