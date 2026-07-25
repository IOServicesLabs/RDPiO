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
//! create/data/close/drain calls straight through.

use std::sync::Arc;

use rdp_graphics::redirect::DvcRedirector;
use rdp_webrtc::{DeviceProvider, NativeRedirector, TurnResolver, VideoCaptureSource, CHANNEL_NAME};

use crate::webrtc_devices::{CameraVideoSource, WinDeviceProvider};
use crate::webrtc_turn::WinTurnResolver;

/// Bridges the graphics DVC mux to the native [`rdp_webrtc`] engine.
pub struct NativeWebRtcRedirector {
    inner: NativeRedirector,
}

impl NativeWebRtcRedirector {
    /// Bring up the native engine on its runtime thread. Returns `None` (→ rdpio
    /// keeps declining the WebRTC channel) only if the runtime thread can't be
    /// spawned.
    pub fn new() -> Option<Self> {
        // Report this machine's real cameras/mics/speakers. Teams will not optimize
        // a call on an endpoint that claims to have none.
        let devices: Arc<dyn DeviceProvider> = Arc::new(WinDeviceProvider);
        // Follow Teams' anycast TURN `300 Try Alternate` (webrtc-rs can't) so a
        // relay candidate can actually be allocated.
        let turn: Arc<dyn TurnResolver> = Arc::new(WinTurnResolver);
        // Real camera → H.264 send track, so Teams' media server accepts outbound video
        // (and the bundled data channel) when the user's camera is on.
        let camera: Arc<dyn VideoCaptureSource> = Arc::new(CameraVideoSource::default());
        match NativeRedirector::new(Some(devices), Some(turn), Some(camera)) {
            Ok(inner) => {
                tracing::info!(
                    channel = CHANNEL_NAME,
                    "native Teams WebRTC engine ready (webrtc-rs); claiming the DVC"
                );
                Some(Self { inner })
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start native WebRTC engine; staying on decline");
                None
            }
        }
    }
}

impl DvcRedirector for NativeWebRtcRedirector {
    fn claims(&self, name: &str) -> bool {
        name == CHANNEL_NAME
    }

    fn on_create(&mut self, channel_id: u32, name: &str) -> bool {
        if !self.claims(name) {
            return false;
        }
        tracing::info!(channel_id, %name, "native WebRTC engine accepting webrtc.1 channel");
        self.inner.on_create(channel_id);
        true
    }

    fn on_data(&mut self, channel_id: u32, message: &[u8]) {
        self.inner.on_data(channel_id, message);
    }

    fn on_close(&mut self, channel_id: u32) {
        self.inner.on_close(channel_id);
    }

    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.inner.drain_outbound()
    }
}
