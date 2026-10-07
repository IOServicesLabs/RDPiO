//! Native Teams "Optimized" WebRTC redirector (`--teams-native`).
//!
//! The counterpart to [`crate::webrtc_addin`]: instead of loading Microsoft's
//! `MsRdcWebRTCAddIn.dll` and letting *it* run the media, this drives our own
//! portable WebRTC engine ([`rdp_webrtc`], built on `webrtc-rs`) against the same
//! `com.microsoft.rdc.dvc.webrtc.1` JSON-RPC protocol. No Microsoft binary is
//! involved, so the exact same optimization can run where the DLL can't (Linux) —
//! this module is the Windows wiring for it, validated first against a real Teams
//! call on the platform we can most easily test.
//!
//! All the async/media machinery lives behind [`rdp_webrtc::NativeRedirector`],
//! which owns a tokio runtime thread. This type is a thin, synchronous
//! [`DvcRedirector`] shim: it claims the webrtc.1 channel and forwards the mux's
//! create/data/close/drain calls straight through. It also claims the MS-RDPEGT
//! geometry channel and feeds it, with the webrtc.1 calls, to the
//! [`VideoCompositor`] that draws Teams' video elements where the server says
//! they are.

use std::collections::HashSet;
use std::sync::Arc;

use rdp_channels::{geometry, names};
use rdp_graphics::redirect::DvcRedirector;
use rdp_webrtc::framing::message_json;
use rdp_webrtc::{
    AudioCaptureSource, DeviceProvider, HostMedia, MediaSink, NativeRedirector, RpcMessage,
    TurnResolver, VideoCaptureSource, CHANNEL_NAME,
};

use crate::teams_video::{OverlayOut, VideoCompositor};
use crate::webrtc_devices::{CameraVideoSource, WinDeviceProvider};
use crate::webrtc_media::{CallAudioSink, CallMicSource};
use crate::webrtc_turn::WinTurnResolver;

/// Bridges the graphics DVC mux to the native [`rdp_webrtc`] engine.
pub struct NativeWebRtcRedirector {
    inner: NativeRedirector,
    video: VideoCompositor,
    /// The MS-RDPEGT geometry channels the server has open. It opens several
    /// (one per tracked window, in practice), and each carries its own
    /// mappings' updates.
    geometry_channels: HashSet<u32>,
}

impl NativeWebRtcRedirector {
    /// Bring up the native engine on its runtime thread; Teams video is drawn
    /// through `overlay` (the UI thread's renderer). Returns `None` (→ rdpio
    /// keeps declining the WebRTC channel) only if the runtime thread can't be
    /// spawned.
    pub fn new(overlay: OverlayOut) -> Option<Self> {
        // Report this machine's real cameras/mics/speakers. Teams will not optimize
        // a call on an endpoint that claims to have none.
        let devices: Arc<dyn DeviceProvider> = Arc::new(WinDeviceProvider);
        // Follow Teams' anycast TURN `300 Try Alternate` (webrtc-rs can't) so a
        // relay candidate can actually be allocated.
        let turn: Arc<dyn TurnResolver> = Arc::new(WinTurnResolver);
        // Real camera → H.264 send track, so Teams' media server accepts outbound video
        // (and the bundled data channel) when the user's camera is on.
        let camera: Arc<dyn VideoCaptureSource> = Arc::new(CameraVideoSource::default());
        // Call audio: the remote side's Opus to the speakers, the mic to Opus.
        let mic: Arc<dyn AudioCaptureSource> = Arc::new(CallMicSource::default());
        let video = VideoCompositor::new(overlay);
        let sink: Arc<dyn MediaSink> = Arc::new(CallMediaSink {
            audio: CallAudioSink::default(),
            video: video.remote_sink(),
        });
        match NativeRedirector::new(HostMedia {
            devices: Some(devices),
            turn_resolver: Some(turn),
            video_source: Some(camera),
            audio_source: Some(mic),
            sink: Some(sink),
        }) {
            Ok(inner) => {
                tracing::info!(
                    channel = CHANNEL_NAME,
                    "native Teams WebRTC engine ready (libwebrtc); claiming the DVC"
                );
                Some(Self {
                    inner,
                    video,
                    geometry_channels: HashSet::new(),
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start native WebRTC engine; staying on decline");
                None
            }
        }
    }
}

/// The call's media sink: audio to the speakers, remote video to the compositor.
struct CallMediaSink {
    audio: CallAudioSink,
    video: crate::teams_video::RemoteVideoSink,
}

impl MediaSink for CallMediaSink {
    fn on_track(&self, track_id: &str, kind: &str, codec: &str) {
        if kind == "audio" {
            self.audio.on_track(track_id, kind, codec);
        }
    }
    fn on_rtp(&self, track_id: &str, codec: &str, payload: &[u8]) {
        self.audio.on_rtp(track_id, codec, payload);
    }
    fn on_pcm(&self, track_id: &str, sample_rate: u32, channels: u32, samples: &[i16]) {
        self.audio.on_pcm(track_id, sample_rate, channels, samples);
    }
    fn on_remote_stream(&self, rpc_id: u64, stream_id: &str) {
        self.video.on_remote_stream(rpc_id, stream_id);
    }
    fn on_video_frame(&self, stream_id: &str, frame: rdp_webrtc::I420Frame) {
        self.video.on_video_frame(stream_id, frame);
    }
}

impl DvcRedirector for NativeWebRtcRedirector {
    fn claims(&self, name: &str) -> bool {
        name == CHANNEL_NAME || name.starts_with(names::GEOMETRY_PREFIX)
    }

    fn on_create(&mut self, channel_id: u32, name: &str) -> bool {
        if name.starts_with(names::GEOMETRY_PREFIX) {
            tracing::info!(channel_id, %name, "tracking desktop geometry for Teams video (MS-RDPEGT)");
            self.geometry_channels.insert(channel_id);
            return true;
        }
        if !self.claims(name) {
            return false;
        }
        tracing::info!(channel_id, %name, "native WebRTC engine accepting webrtc.1 channel");
        self.inner.on_create(channel_id);
        true
    }

    fn on_data(&mut self, channel_id: u32, message: &[u8]) {
        if self.geometry_channels.contains(&channel_id) {
            match geometry::parse(message) {
                Some(update) => self.video.geometry(update),
                None => tracing::debug!(len = message.len(), "unparsed geometry packet"),
            }
            return;
        }
        if let Ok(msg) = RpcMessage::parse(message_json(message)) {
            self.video.observe(&msg);
        }
        self.inner.on_data(channel_id, message);
    }

    fn on_close(&mut self, channel_id: u32) {
        if self.geometry_channels.remove(&channel_id) {
            // Mappings on the other geometry channels stay valid; a closed
            // channel's mappings get their own GEOMETRY_CLEAR or go stale
            // harmlessly (no visible element points at them any more).
            return;
        }
        self.inner.on_close(channel_id);
    }

    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.inner.drain_outbound()
    }
}
