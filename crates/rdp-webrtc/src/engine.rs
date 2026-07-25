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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
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
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCRtpTransceiver, RTCRtpTransceiverInit};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::ice::TurnResolver;

/// Receives media from the peer connection's inbound tracks. A real client
/// decodes each track and composites it into the session at the geometry from
/// the presentation model; tests just count what arrives. Delivered from the
/// engine's RTP read loop, so implementors must be cheap and thread-safe.
pub trait MediaSink: Send + Sync {
    /// A remote track began delivering media (`kind` = "audio"/"video",
    /// `codec` = RTP mime type e.g. "video/VP8").
    fn on_track(&self, track_id: &str, kind: &str, codec: &str);
    /// One RTP packet payload arrived on `track_id`.
    fn on_rtp(&self, track_id: &str, payload: &[u8]);
}

/// Supplies encoded outbound video for a send track: **Annex-B H.264 access units**,
/// pulled one per frame interval and written to the webrtc-rs track (which packetizes
/// to RTP). The client implements this over its real camera (Media Foundation capture
/// + H.264 encode, `rdp_gpu::h264::H264Encoder`); tests use a synthetic source.
///
/// Teams attaches the camera to a video sender via `replaceTrack` *before* it calls
/// `createOffer` (verified in the capture), so a source wired here gives the offer's
/// video m-line a real send configuration (`a=ssrc`/`a=msid`) — which is what makes
/// Teams' media server (Plaza) *accept* the video m-line instead of rejecting it. And
/// because the SCTP data channel is bundled with the media, an accepted video m-line
/// keeps the whole bundle (and the "main-channel" data channel Teams requires) alive,
/// where an all-recvonly-rejected offer collapses the bundle and tears the call down.
pub trait VideoCaptureSource: Send + Sync {
    /// Start capturing the device Teams identified by `source_id` (one of our
    /// enumerated `deviceId`s, e.g. `rdpio-videoinput-0`). Returns false if it can't
    /// start (the track then simply sends nothing until a later attach).
    fn start(&self, source_id: &str) -> bool;
    /// The next Annex-B H.264 access unit, if one is ready (non-blocking).
    fn poll_frame(&self) -> Option<Vec<u8>>;
    /// Stop capturing (the sender's track was cleared, or the peer connection closed).
    fn stop(&self);
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
    /// Cleared per peer connection; set on close to stop the per-track send loops.
    send_stop: Arc<AtomicBool>,
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
            video_source: None,
            send_stop: Arc::new(AtomicBool::new(false)),
            turn_resolver: None,
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
        self.data_channels.clear();
        // Stop any previous PC's send loops, then arm a fresh flag for this one.
        self.send_stop.store(true, Ordering::SeqCst);
        self.send_stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut c) = self.candidates.lock() {
            c.clear();
        }
        if let Ok(mut u) = self.ice_ufrag.lock() {
            *u = None;
        }

        let mut media = MediaEngine::default();
        media.register_default_codecs()?;
        register_teams_header_extensions(&mut media)?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
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

        // Deliver inbound remote media to the sink: on each new track, read its
        // RTP in a background task and forward payloads.
        if let Some(sink) = self.sink.clone() {
            pc.on_track(Box::new(move |track: Arc<TrackRemote>, _receiver, _transceiver| {
                let sink = sink.clone();
                Box::pin(async move {
                    let id = track.id();
                    let kind = match track.kind() {
                        RTPCodecType::Audio => "audio",
                        RTPCodecType::Video => "video",
                        _ => "unknown",
                    };
                    let codec = track.codec().capability.mime_type;
                    sink.on_track(&id, kind, &codec);
                    tokio::spawn(async move {
                        while let Ok((packet, _)) = track.read_rtp().await {
                            sink.on_rtp(&id, &packet.payload);
                        }
                    });
                })
            }));
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
        _wants_send: bool,
    ) -> Result<()> {
        let pc = self.pc()?;
        let codec_type = match kind {
            "video" => RTPCodecType::Video,
            _ => RTPCodecType::Audio,
        };
        // Every transceiver is created **receive-only** so webrtc-rs does NOT auto-attach
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
        // camera are all no-ops) so the answer at least applies cleanly. AUDIO keeps
        // Teams' requested direction (the mic can be sendrecv; its m-line is accepted).
        let requested = parse_direction(direction);
        let init = RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        };
        let t = pc.add_transceiver_from_kind(codec_type, Some(init)).await?;
        if codec_type != RTPCodecType::Video && requested != RTCRtpTransceiverDirection::Recvonly {
            t.set_direction(requested).await;
        }
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
        self.data_channels.insert(id, dc);
        Ok(())
    }

    /// `RTCPeerConnection.close` — tear the peer connection down and forget the
    /// objects hanging off it.
    pub async fn close_peer_connection(&mut self) -> Result<()> {
        // Stop the per-track camera send loops before tearing the PC down.
        self.send_stop.store(true, Ordering::SeqCst);
        if let Some(pc) = self.pc.take() {
            pc.close().await?;
        }
        self.transceivers.clear();
        self.senders.clear();
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
    /// bundled data channel with it). Audio senders (mic) are left alone for now — the
    /// audio m-line is already accepted; capturing the mic (opus) is a later step.
    pub async fn replace_track(&mut self, sender_id: u64, source_id: &str) -> Result<()> {
        let Some(t) = self.senders.get(&sender_id).cloned() else {
            tracing::debug!(sender_id, "replaceTrack for unknown sender; ignoring");
            return Ok(());
        };
        if t.kind() != RTPCodecType::Video {
            return Ok(());
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
        if !source.start(source_id) {
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
                        // The direction the answer actually negotiated (`Unspecified`
                        // until an answer is applied). A rejected/inactive m-line ends
                        // up `inactive` here even though its desired `direction` was
                        // recvonly/sendrecv — so `track_events` fires only for the ones
                        // that truly receive.
                        "currentDirection": t.current_direction().to_string(),
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

/// The ICE [`SettingEngine`] webrtc-rs uses for gathering. Restricts to IPv4/UDP
/// and drops link-local addresses and mDNS candidates — Teams' relays are IPv4
/// and a `169.254.x.x` / `.local` candidate only wastes a check. Without this,
/// gathering spends its budget failing to bind link-local NICs and resolve the
/// TURN host over IPv6 ("No available ipv6 IP address found"), starving the real
/// candidates before Teams tears the call down.
fn ice_setting_engine() -> SettingEngine {
    let mut se = SettingEngine::default();
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
        engine.add_transceiver("audio", "sendrecv", 1, 101, false).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, false).await.expect("video");
        let offer = engine.create_offer().await.expect("offer");
        assert!(offer.starts_with("v=0"), "not SDP");
        assert!(offer.contains("m=audio"), "no audio m-line");
        assert!(offer.contains("m=video"), "no video m-line");
        assert!(offer.to_lowercase().contains("opus"), "no opus");
    }

    /// A `sendrecv` transceiver must offer the sendrecv direction (Teams asked for it)
    /// but carry NO send track — no `a=ssrc` / `a=msid`. webrtc-rs auto-attaches a
    /// track fixed to its first codec of the kind for any send-capable direction; when
    /// Teams' answer negotiates a different codec (H264, not our VP8), that track can't
    /// bind and `set_remote_description(answer)` fails with "codec is not supported by
    /// remote", tearing the just-answered call down. Creating receive-only and flipping
    /// the direction avoids the phantom track while still offering to send.
    #[tokio::test]
    async fn sendrecv_transceiver_offers_the_direction_without_a_send_track() {
        let mut engine = WebrtcEngine::new();
        engine
            .create_peer_connection(&serde_json::json!({ "iceServers": [] }))
            .await
            .expect("create pc");
        engine.add_transceiver("audio", "sendrecv", 1, 101, false).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, false).await.expect("video");
        let offer = engine.create_offer().await.expect("offer");

        // Direction honored: the audio m-line is sendrecv (not downgraded).
        let audio_block = offer.split("m=video").next().unwrap_or(&offer);
        assert!(audio_block.contains("a=sendrecv"), "audio m-line lost its sendrecv direction:\n{offer}");
        // But no send track was attached, so there is no outbound ssrc/msid anywhere.
        assert!(!offer.contains("a=ssrc:"), "offer has a phantom send track (a=ssrc):\n{offer}");
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
        engine.add_transceiver("audio", "sendrecv", 1, 101, false).await.expect("audio");
        engine.add_transceiver("video", "sendrecv", 2, 102, false).await.expect("video");
        engine.add_transceiver("video", "recvonly", 3, 103, false).await.expect("video recvonly");
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
        engine.add_transceiver("audio", "sendrecv", 1, 101, false).await.expect("audio");
        engine.add_transceiver("video", "recvonly", 2, 102, false).await.expect("video");

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
        engine.add_transceiver("audio", "sendrecv", 1, 101, false).await.expect("audio");
        // Even with `sendEncodings` (wants_send = true), the camera transceiver is
        // created receive-only.
        engine.add_transceiver("video", "sendrecv", 2, 102, true).await.expect("video");
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
        engine.add_transceiver("video", "sendrecv", 2, 102, false).await.expect("video");
        // No source configured → Ok, no send config appears.
        engine.replace_track(102, "cam").await.expect("noop ok");
        // Unknown sender id → Ok.
        engine.replace_track(999, "cam").await.expect("unknown sender ok");
        let offer = engine.create_offer().await.expect("offer");
        assert!(!offer.contains("a=ssrc:"), "send config appeared without a source:\n{offer}");
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
            fn on_rtp(&self, _id: &str, _payload: &[u8]) {
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
