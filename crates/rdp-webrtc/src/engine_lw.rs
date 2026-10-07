//! The WebRTC engine over **Google's libwebrtc** (LiveKit's prebuilt build).
//!
//! The same contract as the webrtc-rs engine (`engine.rs`) — the dispatcher
//! drives either — but the media stack is the one Microsoft's own add-in runs,
//! so Teams' media server sees exactly the endpoint it was built against: the
//! same SDP (codecs, extension ids, `red`, `telephone-event`, bundle/rtcp-mux),
//! DTLS, SRTP, RTCP (receiver reports from our announced SSRC, transport-cc
//! feedback, sender reports), Opus with DTX, and congestion control. Every
//! webrtc-rs mismatch Teams tripped on — DTLS signature checks, direction-less
//! answer sections, re-offer m-line order, a call whose audio never arrived —
//! is the reference implementation's behaviour here, not a patch.
//!
//! libwebrtc also encodes and decodes: the mic is fed as 10 ms PCM frames
//! ([`AudioCaptureSource::poll_pcm`]) and remote audio arrives decoded
//! ([`MediaSink::on_pcm`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::native::apm::AudioProcessingModule;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_stream::native::NativeVideoStream;
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection::TrackEvent;
use libwebrtc::prelude::*;
use libwebrtc::MediaType;
use serde_json::Value;
use tokio_stream::StreamExt;

use crate::engine_api::push_event;
pub use crate::engine_api::{AudioCaptureSource, I420Frame, MediaSink, PeerEvent, VideoCaptureSource};
use crate::ice::TurnResolver;

/// The dispatcher's switch: libwebrtc takes Teams' (munged) local SDP as-is and
/// parses Plaza's answers natively, so none of the webrtc-rs SDP workarounds
/// (applying our own offer, enrichment, answer sanitizing) apply.
pub const NATIVE_JSEP: bool = true;

/// Engine errors.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Rtc(String),
    #[error("no active peer connection")]
    NoPeerConnection,
}

impl From<RtcError> for EngineError {
    fn from(e: RtcError) -> Self {
        EngineError::Rtc(format!("{e:?}"))
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// One libwebrtc factory per process (it owns the signaling/worker/network
/// threads).
fn factory() -> &'static PeerConnectionFactory {
    static FACTORY: OnceLock<PeerConnectionFactory> = OnceLock::new();
    FACTORY.get_or_init(PeerConnectionFactory::default)
}

/// A transceiver Teams created, with the kind it asked for (libwebrtc doesn't
/// expose a transceiver's media type directly).
struct Transceiver {
    inner: RtpTransceiver,
    kind: &'static str,
}

/// A native WebRTC engine driven by webrtc.1 RPC calls.
pub struct WebrtcEngine {
    pc: Option<PeerConnection>,
    /// `transceiverRpcObjectId` → transceiver.
    transceivers: HashMap<u64, Arc<Transceiver>>,
    /// `senderRpcObjectId` → its transceiver (`replaceTrack` targets the sender).
    senders: HashMap<u64, Arc<Transceiver>>,
    data_channels: HashMap<u64, DataChannel>,
    candidates: Arc<Mutex<Vec<Value>>>,
    events: Arc<Mutex<Vec<PeerEvent>>>,
    sink: Option<Arc<dyn MediaSink>>,
    video_source: Option<Arc<dyn VideoCaptureSource>>,
    audio_source: Option<Arc<dyn AudioCaptureSource>>,
    /// Kept for API parity: libwebrtc follows TURN `300 Try Alternate` itself.
    turn_resolver: Option<Arc<dyn TurnResolver>>,
    /// The mic track and the Teams track id feeding it (for mute).
    mic: Option<(String, RtcAudioTrack)>,
    /// The camera track and the Teams track id feeding it (for camera off).
    camera: Option<(String, RtcVideoTrack)>,
    /// Set on close to stop this peer connection's pump tasks.
    send_stop: Arc<AtomicBool>,
    /// The call's mic processing (see [`MicProcessing`]): the mic pump
    /// cleans each frame with it, and the remote-audio pump feeds it what the
    /// user hears so the echo canceller knows what to remove.
    apm: Arc<Mutex<Option<MicProcessing>>>,
    /// Teams track id → the capture device its stream was created for.
    track_devices: HashMap<String, String>,
    /// The offer's ICE ufrag (for each trickled candidate's `usernameFragment`).
    ice_ufrag: Arc<Mutex<Option<String>>>,
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
            events: Arc::new(Mutex::new(Vec::new())),
            sink: None,
            video_source: None,
            audio_source: None,
            turn_resolver: None,
            mic: None,
            camera: None,
            send_stop: Arc::new(AtomicBool::new(false)),
            apm: Arc::new(Mutex::new(None)),
            track_devices: HashMap::new(),
            ice_ufrag: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_sink(&mut self, sink: Arc<dyn MediaSink>) {
        self.sink = Some(sink);
    }

    pub fn set_video_source(&mut self, source: Arc<dyn VideoCaptureSource>) {
        self.video_source = Some(source);
    }

    pub fn set_audio_source(&mut self, source: Arc<dyn AudioCaptureSource>) {
        self.audio_source = Some(source);
    }

    pub fn set_turn_resolver(&mut self, resolver: Arc<dyn TurnResolver>) {
        self.turn_resolver = Some(resolver);
    }

    /// `MediaStreamTrack.onEnabledAttributeChanged` — Teams' mute. Disabling
    /// the mic track makes libwebrtc send silence (DTX), as the add-in does.
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
        if let Some((id, track)) = &self.mic {
            if id == track_id {
                track.set_enabled(enabled);
                tracing::info!(track_id, enabled, "call mic {}", if enabled { "unmuted" } else { "muted" });
            }
        }
        if let Some((id, track)) = &self.camera {
            if id == track_id {
                track.set_enabled(enabled);
                tracing::info!(track_id, enabled, "call camera {}", if enabled { "on" } else { "off" });
            }
        }
    }

    /// `RTCPeerConnection.createPeerConnection`.
    pub async fn create_peer_connection(&mut self, config: &Value) -> Result<()> {
        self.close_peer_connection().await?;
        self.send_stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut e) = self.events.lock() {
            e.clear();
        }
        if let Ok(mut u) = self.ice_ufrag.lock() {
            *u = None;
        }

        let mut rtc_config = RtcConfiguration::default();
        rtc_config.ice_servers = parse_ice_servers(config);
        rtc_config.continual_gathering_policy = ContinualGatheringPolicy::GatherOnce;
        rtc_config.ice_transport_type = match config.get("iceTransportPolicy").and_then(Value::as_str) {
            Some("relay") => IceTransportsType::Relay,
            _ => IceTransportsType::All,
        };
        let pc = factory().create_peer_connection(rtc_config)?;

        let (candidates, ufrag) = (self.candidates.clone(), self.ice_ufrag.clone());
        pc.on_ice_candidate(Some(Box::new(move |c: IceCandidate| {
            let ufrag = ufrag.lock().ok().and_then(|u| u.clone());
            if let Some(ev) = candidate_event(&c, ufrag) {
                if let Ok(mut q) = candidates.lock() {
                    q.push(ev);
                }
            }
        })));
        let ev = self.events.clone();
        pc.on_ice_connection_state_change(Some(Box::new(move |s: IceConnectionState| {
            if let Some(state) = ice_connection_state(s) {
                push_event(&ev, PeerEvent::State { event: "iceconnectionstatechange", state: state.into() });
            }
        })));
        let ev = self.events.clone();
        pc.on_connection_state_change(Some(Box::new(move |s: PeerConnectionState| {
            if let Some(state) = peer_connection_state(s) {
                tracing::info!(state, "libwebrtc: peer connection state");
                push_event(&ev, PeerEvent::State { event: "connectionstatechange", state: state.into() });
            }
        })));
        let ev = self.events.clone();
        pc.on_ice_gathering_state_change(Some(Box::new(move |s: IceGatheringState| {
            if s == IceGatheringState::Complete {
                push_event(&ev, PeerEvent::State { event: "icegatheringstatechange", state: "complete".into() });
            }
        })));

        // Remote media arrives decoded: pump each audio track's PCM into the sink.
        if let Some(sink) = self.sink.clone() {
            let rt = tokio::runtime::Handle::current();
            let stop = self.send_stop.clone();
            let apm = self.apm.clone();
            pc.on_track(Some(Box::new(move |t: TrackEvent| {
                let mid = t.transceiver.mid().unwrap_or_default();
                match t.track {
                    MediaStreamTrack::Audio(track) => {
                        let id = t.streams.first().map(|s| s.id()).unwrap_or_else(|| track.id());
                        tracing::info!(track = %id, mid, "libwebrtc: remote audio track");
                        rt.spawn(pump_remote_audio(track, id, sink.clone(), stop.clone(), apm.clone()));
                    }
                    MediaStreamTrack::Video(track) => {
                        let id = t.streams.first().map(|s| s.id()).unwrap_or_else(|| track.id());
                        tracing::info!(track = %id, mid, "libwebrtc: remote video track");
                        rt.spawn(pump_remote_video(track, id, sink.clone(), stop.clone()));
                    }
                }
            })));
        }

        self.pc = Some(pc);
        Ok(())
    }

    /// `RTCPeerConnection.addTransceiver`.
    pub async fn add_transceiver(
        &mut self,
        kind: &str,
        direction: &str,
        id: u64,
        sender_id: u64,
        send_encodings: &[crate::engine_api::SendEncoding],
    ) -> Result<()> {
        let pc = self.pc()?;
        let (media, kind) = match kind {
            "video" => (MediaType::Video, "video"),
            _ => (MediaType::Audio, "audio"),
        };
        let inner = pc.add_transceiver_for_media(
            media,
            RtpTransceiverInit {
                direction: parse_direction(direction),
                stream_ids: vec![],
                // Teams' simulcast layers for the camera (rid "1" full size,
                // "2" half), which its media server answers with
                // `a=simulcast:recv`.
                send_encodings: send_encodings
                    .iter()
                    .map(|e| RtpEncodingParameters {
                        rid: e.rid.clone(),
                        scale_resolution_down_by: Some(e.scale_resolution_down_by),
                        ..Default::default()
                    })
                    .collect(),
            },
        )?;
        let t = Arc::new(Transceiver { inner, kind });
        self.transceivers.insert(id, t.clone());
        self.senders.insert(sender_id, t);
        Ok(())
    }

    /// `RTCPeerConnection.createDataChannel`.
    pub async fn create_data_channel(&mut self, label: &str, id: u64) -> Result<()> {
        let pc = self.pc()?;
        let dc = pc.create_data_channel(label, DataChannelInit::default())?;
        let (ev, me) = (self.events.clone(), dc.clone());
        dc.on_state_change(Some(Box::new(move |s: DataChannelState| {
            let stream_id = me.id().max(0) as u16;
            match s {
                DataChannelState::Open => {
                    push_event(&ev, PeerEvent::ChannelOpen { object_id: id, stream_id })
                }
                DataChannelState::Closing => {
                    push_event(&ev, PeerEvent::ChannelClosing { object_id: id, stream_id })
                }
                _ => {}
            }
        })));
        let (ev, me) = (self.events.clone(), dc.clone());
        dc.on_message(Some(Box::new(move |buf: DataBuffer| {
            let stream_id = me.id().max(0) as u16;
            push_event(
                &ev,
                PeerEvent::ChannelMessage { object_id: id, stream_id, data: buf.data.to_vec() },
            );
        })));
        self.data_channels.insert(id, dc);
        Ok(())
    }

    /// `RTCDataChannel.send` (binary). Unknown channel → no-op.
    pub async fn send_data(&self, id: u64, data: Vec<u8>) -> Result<()> {
        if let Some(dc) = self.data_channels.get(&id) {
            dc.send(&data, true).map_err(|e| EngineError::Rtc(e.to_string()))?;
        }
        Ok(())
    }

    /// `RTCDataChannel.close`.
    pub async fn close_data_channel(&mut self, id: u64) -> Result<()> {
        if let Some(dc) = self.data_channels.remove(&id) {
            dc.close();
        }
        Ok(())
    }

    /// Not tracked by this engine (diagnostic for the webrtc-rs one).
    pub async fn negotiated_codecs(&self, _kind: &str) -> Vec<(u8, String)> {
        Vec::new()
    }

    /// `RTCPeerConnection.getStats`: libwebrtc's own W3C report, as the add-in
    /// returns it (`{stats: [RTCStats…], receivers: []}`).
    pub async fn stats_report(&self) -> Value {
        let stats = match &self.pc {
            Some(pc) => pc
                .get_stats_json()
                .await
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or_else(|| Value::Array(vec![])),
            None => Value::Array(vec![]),
        };
        serde_json::json!({ "stats": stats, "receivers": [] })
    }

    pub fn take_events(&self) -> Vec<PeerEvent> {
        self.events.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    /// `RTCPeerConnection.close`.
    pub async fn close_peer_connection(&mut self) -> Result<()> {
        self.send_stop.store(true, Ordering::SeqCst);
        if let Some(source) = &self.audio_source {
            if self.mic.is_some() {
                source.stop();
            }
        }
        self.mic = None;
        if self.camera.take().is_some() {
            if let Some(source) = &self.video_source {
                source.stop();
            }
        }
        for dc in self.data_channels.values() {
            dc.close();
        }
        if let Some(pc) = self.pc.take() {
            pc.close();
        }
        self.transceivers.clear();
        self.senders.clear();
        self.data_channels.clear();
        if let Ok(mut c) = self.candidates.lock() {
            c.clear();
        }
        Ok(())
    }

    /// `RTCRtpSender.replaceTrack` — the mic onto the audio sender. The camera
    /// isn't sent yet: a video sender without a track simply sends nothing, so
    /// Teams' camera renegotiation still yields a valid offer.
    pub async fn replace_track(&mut self, sender_id: u64, source_id: &str) -> Result<()> {
        let Some(t) = self.senders.get(&sender_id).cloned() else {
            tracing::debug!(sender_id, "replaceTrack for unknown sender; ignoring");
            return Ok(());
        };
        if t.kind != "audio" {
            return self.attach_camera(&t, source_id);
        }
        if self.mic.is_some() {
            return Ok(());
        }
        let Some(source) = self.audio_source.clone() else {
            tracing::warn!("replaceTrack(audio) but no mic source is configured — the call sends no audio");
            return Ok(());
        };
        if !source.start(&self.device_for(source_id)) {
            tracing::warn!(source_id, "mic source failed to start");
        }
        // A native source bypasses libwebrtc's own capture processing (that
        // only runs on its audio device's recordings), so its options are
        // left off and the pump runs the processing itself.
        let native = NativeAudioSource::new(AudioSourceOptions::default(), MIC_RATE, 1, 100);
        if let Ok(mut apm) = self.apm.lock() {
            *apm = Some(MicProcessing::new());
        }
        let track = factory().create_audio_track(&format!("mic-{source_id}"), native.clone());
        t.inner.sender().set_track(Some(MediaStreamTrack::Audio(track.clone())))?;
        self.mic = Some((source_id.to_string(), track));
        tokio::spawn(pump_mic(source, native, self.send_stop.clone(), self.apm.clone()));
        tracing::info!(sender_id, source_id, "attached the mic (PCM → libwebrtc Opus) to the audio sender");
        Ok(())
    }

    /// The camera onto a video sender: raw frames into a libwebrtc video
    /// source, which encodes them (H.264, the codec Teams' answer picks) into
    /// the simulcast layers the transceiver was created with.
    fn attach_camera(&mut self, t: &Transceiver, track_id: &str) -> Result<()> {
        let Some(source) = self.video_source.clone() else {
            tracing::warn!("replaceTrack(video) but no camera source is configured — the call sends no video");
            return Ok(());
        };
        if let Some((old, _)) = self.camera.take() {
            if old != track_id {
                source.stop();
            }
        }
        let device = self.device_for(track_id);
        if !source.start(&device) {
            tracing::warn!(device, "camera source failed to start");
        }
        let native = NativeVideoSource::new(VideoResolution { width: 1280, height: 720 }, false);
        let track = factory().create_video_track(&format!("camera-{track_id}"), native.clone());
        t.inner.sender().set_track(Some(MediaStreamTrack::Video(track.clone())))?;
        self.camera = Some((track_id.to_string(), track));
        tokio::spawn(pump_camera(source, native, self.send_stop.clone()));
        tracing::info!(track_id, device, "attached the camera (NV12 → libwebrtc) to the video sender");
        Ok(())
    }

    /// `RTCRtpTransceiver.setDirection`.
    pub async fn set_transceiver_direction(&mut self, id: u64, direction: &str) -> Result<()> {
        if let Some(t) = self.transceivers.get(&id) {
            t.inner.set_direction(parse_direction(direction))?;
        }
        Ok(())
    }

    /// `RTCPeerConnection.createOffer` (not applied — Teams applies its munged
    /// copy with `setLocalDescription`, exactly as with the add-in).
    pub async fn create_offer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        let offer = pc.create_offer(OfferOptions::default()).await?;
        let sdp = offer.to_string();
        if let (Some(u), Ok(mut slot)) = (ice_ufrag_of(&sdp), self.ice_ufrag.lock()) {
            *slot = Some(u);
        }
        Ok(sdp)
    }

    /// `RTCPeerConnection.setLocalDescription(offer)` — Teams' (munged) SDP,
    /// applied as given; libwebrtc then gathers and trickles candidates.
    pub async fn set_local_offer(&mut self, sdp: &str) -> Result<()> {
        let pc = self.pc()?;
        if let (Some(u), Ok(mut slot)) = (ice_ufrag_of(sdp), self.ice_ufrag.lock()) {
            *slot = Some(u);
        }
        let desc = SessionDescription::parse(sdp, SdpType::Offer)
            .map_err(|e| EngineError::Rtc(format!("local offer: {e:?}")))?;
        pc.set_local_description(desc).await?;
        Ok(())
    }

    pub async fn set_remote_answer(&mut self, sdp: &str) -> Result<()> {
        self.set_remote(sdp, SdpType::Answer).await
    }

    pub async fn set_remote_offer(&mut self, sdp: &str) -> Result<()> {
        self.set_remote(sdp, SdpType::Offer).await
    }

    async fn set_remote(&mut self, sdp: &str, kind: SdpType) -> Result<()> {
        let pc = self.pc()?;
        let desc = SessionDescription::parse(sdp, kind)
            .map_err(|e| EngineError::Rtc(format!("remote description: {e:?}")))?;
        pc.set_remote_description(desc).await?;
        Ok(())
    }

    pub async fn create_answer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        Ok(pc.create_answer(AnswerOptions::default()).await?.to_string())
    }

    pub async fn create_and_set_answer(&mut self) -> Result<String> {
        let pc = self.pc()?;
        let answer = pc.create_answer(AnswerOptions::default()).await?;
        let sdp = answer.to_string();
        pc.set_local_description(answer).await?;
        Ok(sdp)
    }

    /// Wait (bounded) until gathering completes.
    pub async fn wait_ice_gathering(&self) -> Result<()> {
        let pc = self.pc()?;
        for _ in 0..100 {
            if pc.ice_gathering_state() == IceGatheringState::Complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    /// `RTCPeerConnection.addIceCandidate` — a remote candidate line.
    pub async fn add_ice_candidate(&self, candidate: &str, mid: &str, index: i32) -> Result<()> {
        let pc = self.pc()?;
        let c = IceCandidate::parse(mid, index, candidate)
            .map_err(|e| EngineError::Rtc(format!("candidate: {e:?}")))?;
        pc.add_ice_candidate(c).await?;
        Ok(())
    }

    pub fn local_candidates(&self) -> Vec<Value> {
        self.candidates.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn take_candidates(&mut self) -> Vec<Value> {
        self.candidates.lock().map(|mut c| std::mem::take(&mut *c)).unwrap_or_default()
    }

    /// Each transceiver Teams created, by its id, with its assigned mid.
    pub fn transceiver_states(&self) -> Vec<Value> {
        let mut ordered: Vec<(i64, Value)> = self
            .transceivers
            .iter()
            .map(|(id, t)| {
                let mid = t.inner.mid();
                let order = mid.as_deref().and_then(|m| m.parse::<i64>().ok()).unwrap_or(i64::MAX);
                (
                    order,
                    serde_json::json!({
                        "rpcObjectId": id,
                        "direction": direction_name(t.inner.direction()),
                        "mid": mid,
                        "kind": t.kind,
                    }),
                )
            })
            .collect();
        ordered.sort_by_key(|(o, _)| *o);
        ordered.into_iter().map(|(_, v)| v).collect()
    }

    /// The applied local description (once an answer completed negotiation).
    pub async fn local_description(&self) -> Option<String> {
        self.pc.as_ref()?.current_local_description().map(|d| d.to_string())
    }

    fn pc(&self) -> Result<PeerConnection> {
        self.pc.clone().ok_or(EngineError::NoPeerConnection)
    }
}

/// Mic PCM rate: 48 kHz mono, 10 ms frames (what libwebrtc's audio path runs on).
const MIC_RATE: u32 = 48_000;
const MIC_FRAME: usize = 480;

/// The call mic's processing: libwebrtc's echo cancellation, high-pass
/// filter (rumble, desk bumps) and noise suppression, then [`SpeechLeveler`]
/// to bring a quiet voice up. libwebrtc's own gain control (AGC2) undid the
/// noise suppression: with it, steady noise came out only 0.2 dB quieter
/// (`background_noise_is_suppressed`), in one pass or as a second stage.
pub(crate) struct MicProcessing {
    clean: AudioProcessingModule,
    level: SpeechLeveler,
}

impl MicProcessing {
    pub(crate) fn new() -> Self {
        let mut clean = AudioProcessingModule::new(true, false, true, true);
        // Roughly what the user hears is behind what they send: the host's
        // playback prebuffer plus the output device's own buffering.
        let _ = clean.set_stream_delay_ms(APM_PLAYOUT_DELAY_MS);
        Self { clean, level: SpeechLeveler::default() }
    }

    /// Clean and level one 10 ms mono frame at [`MIC_RATE`].
    pub(crate) fn process(&mut self, frame: &mut [i16]) {
        let _ = self.clean.set_stream_delay_ms(APM_PLAYOUT_DELAY_MS);
        if self.clean.process_stream(frame, MIC_RATE as i32, 1).is_ok() {
            self.level.process(frame);
        }
    }

    /// What the user is about to hear (the echo canceller's reference).
    pub(crate) fn far_end(&mut self, frame: &mut [i16], sample_rate: i32, channels: i32) {
        let _ = self.clean.process_reverse_stream(frame, sample_rate, channels);
    }
}

/// Gain for speech only: frames well above the (already suppressed) noise
/// floor are brought toward a normal speaking level; anything near the floor
/// (pauses, background) gets unity gain, so noise is never amplified.
struct SpeechLeveler {
    /// Running noise floor, as frame RMS.
    floor: f32,
    /// Running speech level, as frame RMS.
    speech: f32,
    /// The gain applied, smoothed so it never jumps within a word.
    gain: f32,
}

impl Default for SpeechLeveler {
    fn default() -> Self {
        Self { floor: 100.0, speech: LEVEL_TARGET, gain: 1.0 }
    }
}

/// Speaking level the leveler aims for (frame RMS, of 32767).
const LEVEL_TARGET: f32 = 5_000.0;
/// Most the leveler boosts (+12 dB).
const LEVEL_MAX_GAIN: f32 = 4.0;

impl SpeechLeveler {
    fn process(&mut self, frame: &mut [i16]) {
        let rms = (frame.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / frame.len() as f32).sqrt();
        // The floor drops fast and creeps up slowly (~1 dB/s), so speech
        // doesn't drag it up but a louder room is learned in seconds.
        self.floor = if rms < self.floor { rms.max(1.0) } else { self.floor * 1.0012 };
        let speaking = rms > self.floor * 4.0 && rms > 150.0;
        let target = if speaking {
            self.speech += (rms - self.speech) * 0.05;
            (LEVEL_TARGET / self.speech.max(1.0)).clamp(1.0, LEVEL_MAX_GAIN)
        } else {
            1.0
        };
        // Up in ~50 ms at speech onset, back down over ~300 ms after it.
        let rate = if target > self.gain { 0.2 } else { 0.03 };
        let start = self.gain;
        self.gain += (target - self.gain) * rate;
        let n = frame.len() as f32;
        for (i, s) in frame.iter_mut().enumerate() {
            let g = start + (self.gain - start) * (i as f32 / n);
            let v = *s as f32 * g;
            // Soft limit above ~75% of full scale instead of clipping.
            let v = if v.abs() > 24_000.0 {
                v.signum() * (24_000.0 + 8_767.0 * ((v.abs() - 24_000.0) / 8_767.0).tanh())
            } else {
                v
            };
            *s = v as i16;
        }
    }
}

/// Playback delay assumed for echo cancellation (see [`MicProcessing`]).
const APM_PLAYOUT_DELAY_MS: i32 = 100;

/// Feed the mic's 10 ms frames, cleaned by the call's audio processing, into
/// the native audio source until stopped.
async fn pump_mic(
    source: Arc<dyn AudioCaptureSource>,
    native: NativeAudioSource,
    stop: Arc<AtomicBool>,
    apm: Arc<Mutex<Option<MicProcessing>>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    while !stop.load(Ordering::SeqCst) {
        ticker.tick().await;
        while let Some(mut pcm) = source.poll_pcm() {
            if pcm.len() != MIC_FRAME {
                continue;
            }
            if let Ok(mut guard) = apm.lock() {
                if let Some(apm) = guard.as_mut() {
                    apm.process(&mut pcm);
                }
            }
            let frame = AudioFrame {
                data: pcm.into(),
                sample_rate: MIC_RATE,
                num_channels: 1,
                samples_per_channel: MIC_FRAME as u32,
            };
            if let Err(e) = native.capture_frame(&frame).await {
                tracing::debug!(error = ?e, "mic frame rejected by libwebrtc");
            }
        }
    }
}

/// Feed the camera's new frames into the video source (~30 fps) until stopped.
async fn pump_camera(source: Arc<dyn VideoCaptureSource>, native: NativeVideoSource, stop: Arc<AtomicBool>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(15));
    let start = std::time::Instant::now();
    let mut frames = 0u64;
    while !stop.load(Ordering::SeqCst) {
        ticker.tick().await;
        let Some(f) = source.poll_nv12() else { continue };
        let (w, h) = (f.width as usize, f.height as usize);
        if f.data.len() < w * h * 3 / 2 {
            continue;
        }
        let mut buffer = NV12Buffer::new(f.width, f.height);
        let (sy, suv) = buffer.strides();
        let (dy, duv) = buffer.data_mut();
        for r in 0..h {
            dy[r * sy as usize..r * sy as usize + w].copy_from_slice(&f.data[r * w..r * w + w]);
        }
        let uv = &f.data[w * h..];
        for r in 0..(h + 1) / 2 {
            let row = &uv[r * w..(r * w + w).min(uv.len())];
            duv[r * suv as usize..r * suv as usize + row.len()].copy_from_slice(row);
        }
        let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
        frame.timestamp_us = start.elapsed().as_micros() as i64;
        native.capture_frame(&frame);
        frames += 1;
        if frames == 1 {
            tracing::info!(width = w, height = h, "camera frames flowing into the call");
        }
    }
}

/// Hand a remote audio track's decoded PCM to the sink until it ends.
async fn pump_remote_audio(
    track: RtcAudioTrack,
    id: String,
    sink: Arc<dyn MediaSink>,
    stop: Arc<AtomicBool>,
    apm: Arc<Mutex<Option<MicProcessing>>>,
) {
    let mut stream = NativeAudioStream::new(track, 48_000, 2);
    let mut frames = 0u64;
    while let Some(frame) = stream.next().await {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        frames += 1;
        if frames == 1 {
            tracing::info!(track = %id, rate = frame.sample_rate, channels = frame.num_channels, "remote call audio delivering");
            sink.on_track(&id, "audio", "audio/L16");
        }
        // What the user is about to hear: the echo canceller's reference.
        if let Ok(mut guard) = apm.lock() {
            if let Some(apm) = guard.as_mut() {
                let mut reference = frame.data.to_vec();
                let ten_ms = (frame.sample_rate / 100 * frame.num_channels) as usize;
                if ten_ms > 0 && reference.len() % ten_ms == 0 {
                    apm.far_end(&mut reference, frame.sample_rate as i32, frame.num_channels as i32);
                }
            }
        }
        sink.on_pcm(&id, frame.sample_rate, frame.num_channels, &frame.data);
    }
    tracing::info!(track = %id, frames, "remote call audio ended");
}

/// Hand a remote video track's decoded frames to the sink until it ends.
/// Teams only sends video the user's layout subscribes to, so a track may stay
/// silent; nothing is logged until its first frame.
async fn pump_remote_video(track: RtcVideoTrack, id: String, sink: Arc<dyn MediaSink>, stop: Arc<AtomicBool>) {
    let mut stream = NativeVideoStream::new(track);
    let mut frames = 0u64;
    while let Some(frame) = stream.next().await {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let i420 = libwebrtc::video_frame::buffer_to_i420(frame.buffer.as_ref());
        let (w, h) = (i420.width(), i420.height());
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        let (sy, su, sv) = i420.strides();
        let (dy, du, dv) = i420.data();
        let pack = |data: &[u8], stride: u32, width: u32, rows: u32| {
            let mut out = Vec::with_capacity((width * rows) as usize);
            for r in 0..rows as usize {
                let start = r * stride as usize;
                out.extend_from_slice(&data[start..start + width as usize]);
            }
            out
        };
        frames += 1;
        if frames == 1 {
            tracing::info!(track = %id, width = w, height = h, "remote video delivering");
            sink.on_track(&id, "video", "video/I420");
        }
        sink.on_video_frame(
            &id,
            I420Frame {
                width: w,
                height: h,
                y: pack(dy, sy, w, h),
                u: pack(du, su, cw, ch),
                v: pack(dv, sv, cw, ch),
            },
        );
    }
    if frames > 0 {
        tracing::info!(track = %id, frames, "remote video ended");
    }
}

/// The trickle-ICE `candidate` object in the add-in's shape.
fn candidate_event(c: &IceCandidate, ufrag: Option<String>) -> Option<Value> {
    let line = c.candidate();
    let line = line.strip_prefix("a=").unwrap_or(&line).to_string();
    // candidate:<foundation> <component> <transport> <priority> <ip> <port> typ <type> … [ufrag <u>]
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 8 {
        return None;
    }
    if f[1] != "1" {
        return None; // rtcp-mux: no component-2 candidates
    }
    let ufrag = f
        .iter()
        .position(|t| *t == "ufrag")
        .and_then(|i| f.get(i + 1))
        .map(|s| s.to_string())
        .or(ufrag);
    Some(serde_json::json!({
        "candidate": line,
        "sdp_mid": c.sdp_mid(),
        "sdp_mline_index": c.sdp_mline_index(),
        "foundation": f[0].strip_prefix("candidate:").unwrap_or(f[0]),
        "component": "rtp",
        "protocol": f[2].to_ascii_lowercase(),
        "priority": f[3].parse::<u64>().unwrap_or(0),
        "address": f[4],
        "port": f[5].parse::<u16>().unwrap_or(0),
        "type": f[7],
        "usernameFragment": ufrag,
    }))
}

fn ice_ufrag_of(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.trim().strip_prefix("a=ice-ufrag:"))
        .map(|s| s.trim().to_string())
}

fn parse_direction(d: &str) -> RtpTransceiverDirection {
    match d {
        "sendrecv" => RtpTransceiverDirection::SendRecv,
        "sendonly" => RtpTransceiverDirection::SendOnly,
        "recvonly" => RtpTransceiverDirection::RecvOnly,
        _ => RtpTransceiverDirection::Inactive,
    }
}

fn direction_name(d: RtpTransceiverDirection) -> &'static str {
    match d {
        RtpTransceiverDirection::SendRecv => "sendrecv",
        RtpTransceiverDirection::SendOnly => "sendonly",
        RtpTransceiverDirection::RecvOnly => "recvonly",
        RtpTransceiverDirection::Stopped => "stopped",
        _ => "inactive",
    }
}

fn ice_connection_state(s: IceConnectionState) -> Option<&'static str> {
    Some(match s {
        IceConnectionState::Checking => "checking",
        IceConnectionState::Connected => "connected",
        IceConnectionState::Completed => "completed",
        IceConnectionState::Failed => "failed",
        IceConnectionState::Disconnected => "disconnected",
        IceConnectionState::Closed => "closed",
        _ => return None,
    })
}

fn peer_connection_state(s: PeerConnectionState) -> Option<&'static str> {
    Some(match s {
        PeerConnectionState::Connecting => "connecting",
        PeerConnectionState::Connected => "connected",
        PeerConnectionState::Disconnected => "disconnected",
        PeerConnectionState::Failed => "failed",
        PeerConnectionState::Closed => "closed",
        PeerConnectionState::New => return None,
    })
}

fn parse_ice_servers(config: &Value) -> Vec<IceServer> {
    let Some(servers) = config.get("iceServers").and_then(Value::as_array) else {
        return Vec::new();
    };
    servers
        .iter()
        .map(|s| IceServer {
            urls: match s.get("urls") {
                Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                Some(Value::String(u)) => vec![u.clone()],
                _ => Vec::new(),
            },
            username: s.get("username").and_then(Value::as_str).unwrap_or("").to_string(),
            password: s.get("credential").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A 440 Hz tone, one 10 ms frame per poll (every other call returns None so
    /// the pump's drain loop ends each tick).
    #[derive(Default)]
    struct ToneMic {
        polls: AtomicUsize,
        phase: Mutex<f32>,
    }
    impl AudioCaptureSource for ToneMic {
        fn start(&self, _: &str) -> bool {
            true
        }
        fn poll_frame(&self) -> Option<Vec<u8>> {
            None
        }
        fn poll_pcm(&self) -> Option<Vec<i16>> {
            if self.polls.fetch_add(1, Ordering::SeqCst) % 2 == 1 {
                return None;
            }
            let mut phase = self.phase.lock().unwrap();
            Some(
                (0..MIC_FRAME)
                    .map(|_| {
                        *phase += 2.0 * std::f32::consts::PI * 440.0 / MIC_RATE as f32;
                        (phase.sin() * 12_000.0) as i16
                    })
                    .collect(),
            )
        }
        fn stop(&self) {}
    }

    #[derive(Default)]
    struct LoudSink {
        loud_frames: AtomicUsize,
    }
    impl MediaSink for LoudSink {
        fn on_track(&self, _: &str, _: &str, _: &str) {}
        fn on_rtp(&self, _: &str, _: &str, _: &[u8]) {}
        fn on_pcm(&self, _: &str, _: u32, _: u32, samples: &[i16]) {
            if samples.iter().any(|s| s.unsigned_abs() > 1000) {
                self.loud_frames.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Two libwebrtc engines over loopback: the offerer's mic tone must arrive
    /// decoded at the answerer, and the offerer's data channel must open.
    #[tokio::test(flavor = "multi_thread")]
    async fn mic_audio_and_data_channel_cross_a_loopback_call() {
        loopback_call().await;
    }

    /// A second call in the same process must get audio too (live: the
    /// second echo call received packets but decoded zero samples).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_call_in_the_same_process_still_gets_audio() {
        // The same engines for both calls, as the live redirector reuses its
        // engine across a session's calls.
        let (mut a, mut b, sink) = engines();
        loopback_call_with(&mut a, &mut b, &sink).await;
        sink.loud_frames.store(0, Ordering::SeqCst);
        loopback_call_with(&mut a, &mut b, &sink).await;
    }

    fn engines() -> (WebrtcEngine, WebrtcEngine, Arc<LoudSink>) {
        let mut a = WebrtcEngine::new();
        a.set_audio_source(Arc::new(ToneMic::default()));
        let sink = Arc::new(LoudSink::default());
        let mut b = WebrtcEngine::new();
        b.set_sink(sink.clone());
        (a, b, sink)
    }

    async fn loopback_call() {
        let (mut a, mut b, sink) = engines();
        loopback_call_with(&mut a, &mut b, &sink).await;
    }

    /// Both directions, like a real call: the offerer (our engine's role
    /// against Teams) sends its mic and also receives the far end's audio.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_offerer_receives_audio_on_consecutive_calls() {
        let mut a = WebrtcEngine::new();
        a.set_audio_source(Arc::new(ToneMic::default()));
        let a_sink = Arc::new(LoudSink::default());
        a.set_sink(a_sink.clone());
        let mut b = WebrtcEngine::new();
        b.set_sink(Arc::new(LoudSink::default()));
        for call in 0..2 {
            a_sink.loud_frames.store(0, Ordering::SeqCst);
            let none = serde_json::json!({ "iceServers": [] });
            a.create_peer_connection(&none).await.unwrap();
            b.create_peer_connection(&none).await.unwrap();
            a.add_transceiver("audio", "sendrecv", 1, 2, &[]).await.unwrap();
            a.replace_track(2, "mic0").await.unwrap();
            let offer = a.create_offer().await.unwrap();
            a.set_local_offer(&offer).await.unwrap();
            b.set_remote_offer(&offer).await.unwrap();
            // The far end sends a tone back on its answering transceiver.
            let far = NativeAudioSource::new(AudioSourceOptions::default(), MIC_RATE, 1, 100);
            let track = factory().create_audio_track("far", far.clone());
            let bpc = b.pc().unwrap();
            let bt = bpc.transceivers()[0].clone();
            bt.sender().set_track(Some(MediaStreamTrack::Audio(track))).unwrap();
            bt.set_direction(RtpTransceiverDirection::SendRecv).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            tokio::spawn(pump_mic(Arc::new(ToneMic::default()), far, stop.clone(), Arc::new(Mutex::new(None))));
            let answer = b.create_and_set_answer().await.unwrap();
            a.set_remote_answer(&answer).await.unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline && a_sink.loud_frames.load(Ordering::SeqCst) <= 20 {
                let (from_a, from_b) = (a.take_candidates(), b.take_candidates());
                for (cands, to) in [(from_a, &b), (from_b, &a)] {
                    for c in cands {
                        let line = c["candidate"].as_str().unwrap().to_string();
                        let mid = c["sdp_mid"].as_str().unwrap_or("0").to_string();
                        let idx = c["sdp_mline_index"].as_i64().unwrap_or(0) as i32;
                        to.add_ice_candidate(&line, &mid, idx).await.unwrap();
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            stop.store(true, Ordering::SeqCst);
            let got = a_sink.loud_frames.load(Ordering::SeqCst);
            a.close_peer_connection().await.unwrap();
            b.close_peer_connection().await.unwrap();
            assert!(got > 20, "call {call}: the offerer received only {got} audible frames");
        }
    }

    async fn loopback_call_with(a: &mut WebrtcEngine, b: &mut WebrtcEngine, sink: &Arc<LoudSink>) {
        let none = serde_json::json!({ "iceServers": [] });
        a.create_peer_connection(&none).await.unwrap();
        b.create_peer_connection(&none).await.unwrap();

        a.create_data_channel("main-channel", 10).await.unwrap();
        a.add_transceiver("audio", "sendrecv", 1, 2, &[]).await.unwrap();
        a.replace_track(2, "mic0").await.unwrap();
        let offer = a.create_offer().await.unwrap();
        assert!(offer.contains("opus/48000/2"), "libwebrtc offer:\n{offer}");
        a.set_local_offer(&offer).await.unwrap();
        b.set_remote_offer(&offer).await.unwrap();
        let answer = b.create_and_set_answer().await.unwrap();
        a.set_remote_answer(&answer).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut opened = false;
        while std::time::Instant::now() < deadline {
            let (from_a, from_b) = (a.take_candidates(), b.take_candidates());
            for (cands, to) in [(from_a, &*b), (from_b, &*a)] {
                for c in cands {
                    let line = c["candidate"].as_str().unwrap().to_string();
                    let mid = c["sdp_mid"].as_str().unwrap_or("0").to_string();
                    let idx = c["sdp_mline_index"].as_i64().unwrap_or(0) as i32;
                    to.add_ice_candidate(&line, &mid, idx).await.unwrap();
                }
            }
            opened |= a.take_events().iter().any(|e| matches!(e, PeerEvent::ChannelOpen { .. }));
            if opened && sink.loud_frames.load(Ordering::SeqCst) > 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let loud = sink.loud_frames.load(Ordering::SeqCst);
        let stats = a.stats_report().await;
        a.close_peer_connection().await.unwrap();
        b.close_peer_connection().await.unwrap();
        assert!(opened, "data channel never opened");
        assert!(loud > 20, "answerer received only {loud} audible frames");
        let types: Vec<&str> = stats["stats"]
            .as_array()
            .expect("stats array")
            .iter()
            .filter_map(|s| s["type"].as_str())
            .collect();
        assert!(types.contains(&"outbound-rtp") && types.contains(&"transport"), "stats types: {types:?}");
    }

    /// A synthetic camera: a moving gradient, one new frame per poll.
    struct TestCamera {
        n: std::sync::atomic::AtomicU32,
    }
    impl VideoCaptureSource for TestCamera {
        fn start(&self, _: &str) -> bool {
            true
        }
        fn poll_frame(&self) -> Option<Vec<u8>> {
            None
        }
        fn poll_nv12(&self) -> Option<crate::engine_api::Nv12Frame> {
            let n = self.n.fetch_add(1, Ordering::SeqCst) as u8;
            let (w, h) = (640usize, 360usize);
            let mut data = vec![128u8; w * h * 3 / 2];
            for y in 0..h {
                for x in 0..w {
                    data[y * w + x] = (x as u8).wrapping_add(n);
                }
            }
            Some(crate::engine_api::Nv12Frame { width: w as u32, height: h as u32, data: Arc::new(data) })
        }
        fn stop(&self) {}
    }

    #[derive(Default)]
    struct VideoCount {
        frames: AtomicUsize,
        streams: Mutex<Vec<(u64, String)>>,
        size: Mutex<(u32, u32)>,
    }
    impl MediaSink for VideoCount {
        fn on_track(&self, _: &str, _: &str, _: &str) {}
        fn on_rtp(&self, _: &str, _: &str, _: &[u8]) {}
        fn on_remote_stream(&self, rpc_id: u64, stream_id: &str) {
            self.streams.lock().unwrap().push((rpc_id, stream_id.to_string()));
        }
        fn on_video_frame(&self, _: &str, frame: I420Frame) {
            *self.size.lock().unwrap() = (frame.width, frame.height);
            self.frames.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Keep only H.264 (and its RTX) on video m-lines — the munging Teams does
    /// to our offer before `setLocalDescription`.
    fn h264_only(sdp: &str) -> String {
        let lines: Vec<&str> = sdp.split("\r\n").collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if !lines[i].starts_with("m=video") {
                out.push(lines[i].to_string());
                i += 1;
                continue;
            }
            let end = (i + 1..lines.len()).find(|&j| lines[j].starts_with("m=")).unwrap_or(lines.len());
            let section = &lines[i..end];
            let h264: Vec<String> = section
                .iter()
                .filter_map(|l| l.strip_prefix("a=rtpmap:"))
                .filter(|r| r.contains(" H264/"))
                .filter_map(|r| r.split(' ').next().map(str::to_owned))
                .collect();
            let mut keep = h264.clone();
            for l in section {
                if let Some(f) = l.strip_prefix("a=fmtp:") {
                    let (pt, params) = f.split_once(' ').unwrap_or((f, ""));
                    if params.starts_with("apt=") && h264.iter().any(|h| params == format!("apt={h}")) {
                        keep.push(pt.to_string());
                    }
                }
            }
            let pt_of = |l: &str| -> Option<String> {
                for p in ["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"] {
                    if let Some(r) = l.strip_prefix(p) {
                        return r.split(' ').next().map(str::to_owned);
                    }
                }
                None
            };
            let m: Vec<&str> = section[0].splitn(4, ' ').collect();
            out.push(format!("{} {} {} {}", m[0], m[1], m[2], keep.join(" ")));
            for l in &section[1..] {
                match pt_of(l) {
                    Some(pt) if pt != "*" && !keep.contains(&pt) => {}
                    _ => out.push(l.to_string()),
                }
            }
            i = end;
        }
        out.join("\r\n")
    }

    /// Camera send and remote video receive over loopback, H.264 only (what
    /// Teams negotiates): the far end gets decoded frames for the stream the
    /// host was told about.
    #[tokio::test(flavor = "multi_thread")]
    async fn camera_video_crosses_a_loopback_call_as_h264() {
        let caps = factory().get_rtp_sender_capabilities(MediaType::Video);
        assert!(
            caps.codecs.iter().any(|c| c.mime_type.eq_ignore_ascii_case("video/H264")),
            "no H.264 encoder in this libwebrtc build"
        );
        let mut a = WebrtcEngine::new();
        a.set_video_source(Arc::new(TestCamera { n: Default::default() }));
        let sink = Arc::new(VideoCount::default());
        let mut b = WebrtcEngine::new();
        b.set_sink(sink.clone());
        let none = serde_json::json!({ "iceServers": [] });
        a.create_peer_connection(&none).await.unwrap();
        b.create_peer_connection(&none).await.unwrap();
        a.add_transceiver("video", "sendrecv", 1, 2, &[]).await.unwrap();
        a.replace_track(2, "rdpio-videoinput-0").await.unwrap();
        let offer = h264_only(&a.create_offer().await.unwrap());
        a.set_local_offer(&offer).await.expect("libwebrtc applies the H.264-only munged offer");
        b.set_remote_offer(&offer).await.unwrap();
        let answer = b.create_and_set_answer().await.unwrap();
        assert!(answer.contains("H264/90000"), "answer:\n{answer}");
        a.set_remote_answer(&answer).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline && sink.frames.load(Ordering::SeqCst) < 10 {
            let (from_a, from_b) = (a.take_candidates(), b.take_candidates());
            for (cands, to) in [(from_a, &b), (from_b, &a)] {
                for c in cands {
                    let line = c["candidate"].as_str().unwrap().to_string();
                    let mid = c["sdp_mid"].as_str().unwrap_or("0").to_string();
                    let idx = c["sdp_mline_index"].as_i64().unwrap_or(0) as i32;
                    to.add_ice_candidate(&line, &mid, idx).await.unwrap();
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let frames = sink.frames.load(Ordering::SeqCst);
        let size = *sink.size.lock().unwrap();
        a.close_peer_connection().await.unwrap();
        b.close_peer_connection().await.unwrap();
        assert!(frames >= 10, "far end decoded only {frames} video frames");
        assert_eq!(size, (640, 360));
    }

    /// The media server's answer from the add-in's recorded call (made for a
    /// libwebrtc offer) applies to ours as-is: Teams' shape — one audio
    /// sendrecv, nine recvonly video, the data channel — negotiates every m-line.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_media_servers_answer_to_the_add_in_applies_to_our_offer() {
        let answer = include_str!("../tests/fixtures/addin_call_answer.sdp");
        let mut e = WebrtcEngine::new();
        e.create_peer_connection(&serde_json::json!({ "iceServers": [] })).await.unwrap();
        e.create_data_channel("main-channel", 12).await.unwrap();
        // Teams' flow: created inactive, then setDirection.
        e.add_transceiver("audio", "inactive", 13, 14, &[]).await.unwrap();
        e.set_transceiver_direction(13, "sendrecv").await.unwrap();
        e.add_transceiver("video", "inactive", 16, 17, &[crate::engine_api::SendEncoding { rid: "1".into(), scale_resolution_down_by: 1.0 }]).await.unwrap();
        e.set_transceiver_direction(16, "sendrecv").await.unwrap();
        for i in 0..8u64 {
            e.add_transceiver("video", "inactive", 19 + 3 * i, 20 + 3 * i, &[]).await.unwrap();
            e.set_transceiver_direction(19 + 3 * i, "recvonly").await.unwrap();
        }
        let offer = e.create_offer().await.unwrap();
        e.set_local_offer(&offer).await.unwrap();
        e.set_remote_answer(answer).await.expect("the add-in call's answer applies");
        let states = e.transceiver_states();
        let mids: Vec<&str> = states.iter().filter_map(|t| t["mid"].as_str()).collect();
        assert_eq!(mids, ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        // A renegotiation keeps the m-line order (the webrtc-rs failure).
        let reoffer = e.create_offer().await.unwrap();
        assert!(reoffer.contains("a=group:BUNDLE 0 1 2 3 4 5 6 7 8 9 10"), "{reoffer}");
        e.close_peer_connection().await.unwrap();
    }
}


#[cfg(test)]
mod audio_processing_tests {
    use super::*;

    fn rng(seed: &mut u32) -> i16 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        ((*seed % 2001) as i32 - 1000) as i16
    }
    fn energy(f: &[i16]) -> f64 {
        f.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / f.len() as f64
    }

    /// Steady background noise (fan, hiss) is what noise suppression removes:
    /// after it adapts, the processed output must be well below the input.
    #[test]
    fn background_noise_is_suppressed() {
        let mut p = MicProcessing::new();
        let mut seed = 0x1234_5678u32;
        let (mut input, mut output) = (0.0, 0.0);
        for i in 0..300 {
            let mut frame: Vec<i16> = (0..MIC_FRAME).map(|_| rng(&mut seed)).collect();
            let before = energy(&frame);
            p.process(&mut frame);
            if i >= 200 {
                input += before;
                output += energy(&frame);
            }
        }
        let reduction_db = 10.0 * (input / output.max(1.0)).log10();
        assert!(reduction_db > 6.0, "noise only reduced by {reduction_db:.1} dB");
    }

    /// A quiet voice (~10% of full scale, the live mic) is brought up.
    #[test]
    fn a_quiet_voice_is_made_louder() {
        let mut p = MicProcessing::new();
        let (mut input, mut output) = (0.0, 0.0);
        let mut t = 0f32;
        for i in 0..500 {
            // A 220 Hz "voice" with a syllable-like envelope.
            let mut frame: Vec<i16> = (0..MIC_FRAME)
                .map(|_| {
                    t += 1.0 / MIC_RATE as f32;
                    let env = (0.5 + 0.5 * (t * 3.0 * std::f32::consts::TAU).sin()).powi(2);
                    ((t * 220.0 * std::f32::consts::TAU).sin() * 3_000.0 * env) as i16
                })
                .collect();
            let before = energy(&frame);
            p.process(&mut frame);
            if i >= 300 {
                input += before;
                output += energy(&frame);
            }
        }
        let gain_db = 10.0 * (output / input).log10();
        assert!(gain_db > 3.0, "voice only changed by {gain_db:.1} dB");
    }
}
