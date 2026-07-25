//! The RDPGFX graphics dynamic-channel state machine.
//!
//! Sits on top of the static `drdynvc` channel and drives the graphics endpoint
//! end to end: it answers the DRDYNVC capabilities request, accepts the
//! `Microsoft::Windows::RDS::Graphics` channel create (declining others),
//! advertises RDPGFX capabilities, reassembles fragmented channel data, and
//! runs it through the ZGFX → EGFX [`GfxPipeline`] to yield typed
//! [`GfxCommand`]s for the renderer. It also builds frame-acknowledge PDUs.
//!
//! This is sans-I/O: [`GraphicsChannel::process`] takes the bytes received on
//! the static channel and returns both the bytes to send back and the decoded
//! commands. The caller owns the actual MCS send/receive and the H.264 decode
//! of any AVC surface payloads.

use rdp_channels::drdynvc::{self, DvcPdu, Reassembler};
use rdp_channels::{disp, names, rdpei};
use rdp_pdu::gfx::GfxCommand;

use crate::egfx::GfxPipeline;
use crate::redirect::DvcRedirector;

/// DVC create-response status for a channel we decline (non-zero = failure).
const STATUS_DECLINE: u32 = 0xC000_0001;

/// How many times the server may re-request the *same* declined channel before we
/// emit a one-shot warning. Windows recycles a freed channel id and re-offers a
/// declined redirection channel whenever a new video region appears, so a handful
/// of retries is normal; a large count means a genuine retry storm worth flagging.
const DECLINE_STORM_THRESHOLD: u32 = 16;

/// What to do after processing one inbound static-channel payload.
#[derive(Default)]
pub struct GraphicsOutput {
    /// Bytes to send back on the static `drdynvc` channel (caps/create
    /// responses and the wrapped caps-advertise).
    pub responses: Vec<Vec<u8>>,
    /// EGFX commands decoded from a completed graphics-channel message.
    pub commands: Vec<GfxCommand>,
    /// Complete messages received on the microphone (`AUDIO_INPUT`) channel, for
    /// the caller's MS-RDPEAI state machine. Empty unless that channel is open.
    pub audio_input: Vec<Vec<u8>>,
    /// Complete messages received on the speaker (`AUDIO_PLAYBACK_DVC`) channel,
    /// for the caller's MS-RDPEA (RDPSND) state machine. Empty unless open.
    pub audio_output: Vec<Vec<u8>>,
    /// Set to `true` in the processing turn where `AUDIO_PLAYBACK_DVC` is created.
    /// The caller should send the client's initial audio formats in response.
    pub audio_output_opened: bool,
    /// Complete messages received on the camera enumerator channel, for the
    /// caller's MS-RDPECAM state machine. Empty unless that channel is open.
    pub camera: Vec<Vec<u8>>,
    /// Complete messages received on a per-device camera channel, tagged with
    /// the channel id so the caller can route to the right device.
    pub camera_device: Vec<(u32, Vec<u8>)>,
}

/// Stateful DRDYNVC multiplexer for one connection. Despite the name it routes
/// every dynamic channel we care about: the RDPGFX graphics pipeline, Display
/// Control (resize), and the microphone (`AUDIO_INPUT`) endpoint. Graphics is
/// decoded here; the audio-input bytes are surfaced raw for the caller's
/// MS-RDPEAI handler so this crate stays free of audio concerns.
#[derive(Default)]
pub struct GraphicsChannel {
    reassembler: Reassembler,
    pipeline: GfxPipeline,
    gfx_channel_id: Option<u32>,
    /// The Display Control dynamic channel id, once opened (for resize).
    disp_channel_id: Option<u32>,
    /// The microphone (`AUDIO_INPUT`) dynamic channel id, once opened.
    audio_in_channel_id: Option<u32>,
    /// The speaker (`AUDIO_PLAYBACK_DVC`) dynamic channel id, once opened. The
    /// formats/training handshake runs here.
    audio_out_channel_id: Option<u32>,
    /// The lossy speaker (`AUDIO_PLAYBACK_LOSSY_DVC`) dynamic channel id, once
    /// opened. When this channel is open, Windows streams the actual wave PDUs
    /// here (only control PDUs stay on the reliable channel), so its data must
    /// reach the same RDPSND state machine or all audio is silently dropped.
    audio_out_lossy_channel_id: Option<u32>,
    /// The camera enumerator dynamic channel id, once opened.
    camera_channel_id: Option<u32>,
    /// Per-device camera channel ids (the server opens one per announced camera).
    camera_device_ids: Vec<u32>,
    /// The multi-touch/pen input (RDPEI) dynamic channel id, once opened.
    rdpei_channel_id: Option<u32>,
    /// RDPEI state machine (server-ready / client-ready handshake, touch frames).
    rdpei: rdpei::RdpInputChannel,
    /// Per-name count of DVC create-requests we've declined. Lets us log a
    /// declined channel once (and flag a real retry storm) instead of emitting a
    /// line every time the server recycles a channel id and re-offers it.
    declined: std::collections::HashMap<String, u32>,
    /// Optional bridge (e.g. the Teams WebRTC add-in host) that claims and
    /// services some channels the mux would otherwise decline. `None` unless the
    /// caller wired one in via [`GraphicsChannel::set_redirector`].
    redirector: Option<Box<dyn DvcRedirector>>,
    /// Channel ids currently owned by the [`redirector`](Self::redirector), so
    /// their data/close is routed to it rather than dropped.
    redirector_channels: std::collections::HashSet<u32>,
}

impl GraphicsChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// A channel whose graphics pipeline advertises a specific RDPGFX caps set
    /// (see [`crate::egfx::CAPS_FULL`] / [`CAPS_AVC420_ONLY`] / [`CAPS_NO_AVC`]).
    /// The client chooses the set from whether a local GPU H.264 decoder exists.
    ///
    /// [`CAPS_AVC420_ONLY`]: crate::egfx::CAPS_AVC420_ONLY
    /// [`CAPS_NO_AVC`]: crate::egfx::CAPS_NO_AVC
    pub fn with_caps(caps: Vec<(u32, u32)>) -> Self {
        Self {
            pipeline: crate::egfx::GfxPipeline::with_caps(caps),
            ..Default::default()
        }
    }

    /// The dynamic channel id once the graphics channel has been created.
    pub fn channel_id(&self) -> Option<u32> {
        self.gfx_channel_id
    }

    /// Attach a dynamic-channel redirector (e.g. the Teams WebRTC add-in host).
    /// Channels it claims are accepted and bridged instead of declined.
    pub fn set_redirector(&mut self, redirector: Box<dyn DvcRedirector>) {
        self.redirector = Some(redirector);
    }

    /// Drain any DVC PDUs the redirector wants to send to the server. Call each
    /// session-loop iteration: the hosted add-in produces data asynchronously on
    /// its own threads, so this can yield PDUs even when no inbound data arrived.
    pub fn poll_redirector(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(r) = self.redirector.as_mut() {
            for (channel_id, payload) in r.drain_outbound() {
                out.extend(drdynvc::data_message(channel_id, &payload));
            }
        }
        out
    }

    /// Offer an otherwise-unhandled create-request to the redirector. Returns
    /// `true` (and pushes an accept response) if the redirector took the channel.
    fn try_redirect_create(&mut self, channel_id: u32, name: &str, out: &mut GraphicsOutput) -> bool {
        let accepted = match self.redirector.as_mut() {
            Some(r) if r.claims(name) => r.on_create(channel_id, name),
            _ => return false,
        };
        if accepted {
            self.redirector_channels.insert(channel_id);
            out.responses.push(drdynvc::create_response(channel_id, 0));
            tracing::info!(%name, channel_id, "accepted DVC via redirector bridge");
        }
        accepted
    }

    /// Process one DRDYNVC PDU received on the static channel.
    pub fn process(&mut self, payload: &[u8]) -> GraphicsOutput {
        let mut out = GraphicsOutput::default();
        let Ok(pdu) = drdynvc::parse(payload) else {
            return out;
        };
        match pdu {
            DvcPdu::CapabilitiesRequest { version } => {
                tracing::info!(version, "DVC capabilities request → responding");
                out.responses.push(drdynvc::capabilities_response(version));
            }
            DvcPdu::CreateRequest { channel_id, name } => {
                tracing::info!(channel_id, %name, "DVC create request");
                if name == names::GRAPHICS {
                    self.gfx_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                    // Advertise our RDPGFX capabilities on the freshly-opened channel.
                    let caps = self.pipeline.caps_advertise();
                    tracing::info!(channel_id, caps_len = caps.len(), "opened RDPGFX channel; advertising caps");
                    out.responses.push(drdynvc::data(channel_id, &caps));
                } else if name == disp::DISPLAYCONTROL_CHANNEL {
                    // Accept Display Control so we can request desktop resizes.
                    self.disp_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                } else if name == names::AUDIO_INPUT {
                    // Accept the microphone channel; the caller drives MS-RDPEAI.
                    self.audio_in_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                } else if name == names::AUDIO_PLAYBACK {
                    // Accept the speaker channel; the caller drives MS-RDPEA (RDPSND).
                    // Modern Windows streams session audio here, not on static rdpsnd.
                    self.audio_out_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                    out.audio_output_opened = true;
                    tracing::info!(channel_id, "opened AUDIO_PLAYBACK_DVC channel");
                } else if name == names::AUDIO_PLAYBACK_LOSSY {
                    // Accept the unreliable speaker channel AND track it: Windows
                    // streams the wave PDUs on this channel when it is open (the
                    // reliable channel carries only formats/training), so its data
                    // must be routed to the same RDPSND state machine. Not tracking
                    // it is why audio was silently dropped.
                    self.audio_out_lossy_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                    tracing::info!(channel_id, "opened AUDIO_PLAYBACK_LOSSY_DVC channel");
                } else if name == names::CAMERA_ENUMERATOR {
                    // Accept the camera enumerator; the caller drives MS-RDPECAM.
                    self.camera_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                } else if name.starts_with(names::CAMERA_DEVICE_PREFIX) {
                    // A per-device camera channel for a camera we announced.
                    self.camera_device_ids.push(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                } else if name == names::RDPINPUT {
                    // Accept multi-touch/pen input channel.
                    self.rdpei_channel_id = Some(channel_id);
                    out.responses.push(drdynvc::create_response(channel_id, 0));
                    tracing::info!(channel_id, "opened RDPEI channel; awaiting server ready");
                } else if self.try_redirect_create(channel_id, &name, &mut out) {
                    // A hosted redirector (e.g. the Teams WebRTC add-in) claimed
                    // and accepted this channel; its data is bridged, not dropped.
                } else {
                    // Decline everything else. This is deliberate for the server's
                    // media *redirection* channels (WebRTC/MMR/RDPEVOR/geometry):
                    // accepting one without implementing its redirector would make
                    // the host stream video off the graphics channel we decode.
                    // Declining keeps video in the RDPGFX pipeline. Count retries so
                    // the log stays quiet and a real storm is still visible.
                    let count = {
                        let c = self.declined.entry(name.to_string()).or_insert(0);
                        *c += 1;
                        *c
                    };
                    if names::is_redirection_channel(&name) {
                        if count == 1 {
                            tracing::info!(%name, "declining server media-redirection channel; video stays in the in-session graphics pipeline (client-side redirect not implemented)");
                        } else if count == DECLINE_STORM_THRESHOLD {
                            tracing::warn!(%name, count, "server keeps re-requesting a declined redirection channel — retry storm; consider a redirector bridge");
                        }
                    } else if count == 1 {
                        tracing::info!(%name, "declining unhandled dynamic channel");
                    }
                    out.responses
                        .push(drdynvc::create_response(channel_id, STATUS_DECLINE));
                }
            }
            data @ (DvcPdu::DataFirst { .. } | DvcPdu::Data { .. }) => {
                let channel_id = match &data {
                    DvcPdu::DataFirst { channel_id, .. } | DvcPdu::Data { channel_id, .. } => {
                        *channel_id
                    }
                    _ => unreachable!(),
                };
                if Some(channel_id) == self.gfx_channel_id {
                    if let Some((_id, message)) = self.reassembler.accept(data) {
                        let commands = self.pipeline.process(&message);
                        tracing::trace!(
                            gfx_msg_len = message.len(),
                            commands = commands.as_ref().map(|c| c.len()).unwrap_or(0),
                            "RDPGFX message reassembled"
                        );
                        if let Some(commands) = commands {
                            out.commands = commands;
                        }
                    }
                } else if Some(channel_id) == self.audio_in_channel_id {
                    // Surface complete microphone-channel messages for the caller.
                    if let Some((_id, message)) = self.reassembler.accept(data) {
                        out.audio_input.push(message);
                    }
                } else if Some(channel_id) == self.audio_out_channel_id
                    || Some(channel_id) == self.audio_out_lossy_channel_id
                {
                    // Surface complete speaker-channel (RDPSND) messages for the
                    // caller. Wave PDUs may arrive on the lossy channel while the
                    // formats/training handshake stays on the reliable one; both
                    // feed the same RDPSND state machine.
                    let lossy = Some(channel_id) == self.audio_out_lossy_channel_id;
                    tracing::info!(channel_id, lossy, "AUDIO_PLAYBACK_DVC data PDU");
                    if let Some((_id, message)) = self.reassembler.accept(data) {
                        tracing::info!(
                            bytes = message.len(),
                            msg_type = message.first().copied().unwrap_or(0),
                            lossy,
                            "AUDIO_PLAYBACK_DVC message reassembled"
                        );
                        out.audio_output.push(message);
                    }
                } else if Some(channel_id) == self.camera_channel_id {
                    // Surface complete camera enumerator messages for the caller.
                    if let Some((_id, message)) = self.reassembler.accept(data) {
                        out.camera.push(message);
                    }
                } else if self.camera_device_ids.contains(&channel_id) {
                    // Surface a per-device camera message, tagged with its channel.
                    if let Some((id, message)) = self.reassembler.accept(data) {
                        out.camera_device.push((id, message));
                    }
                } else if Some(channel_id) == self.rdpei_channel_id {
                    // RDPEI is a single-frame protocol; route the payload to the
                    // state machine and send any response (client-ready) back.
                    if let DvcPdu::Data { payload, .. } = data {
                        if let Some(response) = self.rdpei.process_server_payload(channel_id, &payload) {
                            out.responses.push(drdynvc::data(channel_id, &response));
                        }
                    }
                } else if self.redirector_channels.contains(&channel_id) {
                    // A channel a hosted redirector accepted: reassemble and hand
                    // the complete message to it. Any bytes it wants to send back
                    // are drained via `poll_redirector` on the session loop.
                    if let Some((_id, message)) = self.reassembler.accept(data) {
                        if let Some(r) = self.redirector.as_mut() {
                            r.on_data(channel_id, &message);
                        }
                    }
                } else {
                    // Data on a channel we opened but don't route. Logged (not
                    // silently dropped) so a misrouted stream — e.g. audio landing
                    // on an untracked channel — is visible instead of mysterious.
                    tracing::warn!(channel_id, "data on unhandled dynamic channel");
                }
            }
            DvcPdu::Close { channel_id } => {
                if Some(channel_id) == self.gfx_channel_id {
                    self.gfx_channel_id = None;
                }
                if Some(channel_id) == self.disp_channel_id {
                    self.disp_channel_id = None;
                }
                if Some(channel_id) == self.audio_in_channel_id {
                    self.audio_in_channel_id = None;
                }
                if Some(channel_id) == self.audio_out_channel_id {
                    self.audio_out_channel_id = None;
                }
                if Some(channel_id) == self.audio_out_lossy_channel_id {
                    self.audio_out_lossy_channel_id = None;
                }
                if Some(channel_id) == self.camera_channel_id {
                    self.camera_channel_id = None;
                }
                self.camera_device_ids.retain(|&id| id != channel_id);
                if Some(channel_id) == self.rdpei_channel_id {
                    self.rdpei_channel_id = None;
                }
                if self.redirector_channels.remove(&channel_id) {
                    if let Some(r) = self.redirector.as_mut() {
                        r.on_close(channel_id);
                    }
                }
            }
            DvcPdu::Other { .. } => {}
        }
        out
    }

    /// Build a Display Control monitor-layout PDU requesting a desktop resize to
    /// the supplied `monitors` layout (wrapped as DRDYNVC data), or `None` if the
    /// Display Control channel isn't open.
    pub fn request_resize(&self,
        monitors: &[rdp_pdu::gcc::MonitorDef],
    ) -> Option<Vec<u8>> {
        self.disp_channel_id
            .map(|id| drdynvc::data(id, &disp::monitor_layout(monitors)))
    }

    /// Whether the microphone (`AUDIO_INPUT`) channel is open.
    pub fn audio_input_open(&self) -> bool {
        self.audio_in_channel_id.is_some()
    }

    /// Wrap an MS-RDPEAI PDU as DRDYNVC data for the microphone channel, or
    /// `None` if that channel isn't open.
    pub fn wrap_audio_input(&self, payload: &[u8]) -> Option<Vec<u8>> {
        self.audio_in_channel_id
            .map(|id| drdynvc::data(id, payload))
    }

    /// Whether the speaker (`AUDIO_PLAYBACK_DVC`) channel is open.
    pub fn audio_output_open(&self) -> bool {
        self.audio_out_channel_id.is_some()
    }

    /// Wrap an RDPSND PDU (e.g. a wave confirm or formats reply) as DRDYNVC data
    /// for the speaker channel, or `None` if that channel isn't open.
    pub fn wrap_audio_output(&self, payload: &[u8]) -> Option<Vec<u8>> {
        self.audio_out_channel_id.map(|id| {
            tracing::info!(
                channel_id = id,
                bytes = payload.len(),
                msg_type = format!("{:#04x}", payload.first().copied().unwrap_or(0)),
                "AUDIO_PLAYBACK_DVC reply"
            );
            drdynvc::data(id, payload)
        })
    }

    /// Whether the camera enumerator channel is open.
    pub fn camera_open(&self) -> bool {
        self.camera_channel_id.is_some()
    }

    /// Wrap an MS-RDPECAM PDU as DRDYNVC data for the camera enumerator channel,
    /// or `None` if that channel isn't open.
    pub fn wrap_camera(&self, payload: &[u8]) -> Option<Vec<u8>> {
        self.camera_channel_id.map(|id| drdynvc::data(id, payload))
    }

    /// Wrap a PDU as DRDYNVC data for a specific per-device camera channel.
    pub fn wrap_camera_device(&self, channel_id: u32, payload: &[u8]) -> Vec<u8> {
        drdynvc::data(channel_id, payload)
    }

    /// Whether the RDPEI multi-touch/pen input channel is open and ready.
    pub fn rdpei_ready(&self) -> bool {
        self.rdpei_channel_id.is_some() && matches!(self.rdpei.state(), rdpei::RdpInputState::Ready(_))
    }

    /// Wrap a touch-event PDU as DRDYNVC data for the RDPEI channel, or `None`
    /// if the channel is not open.
    pub fn wrap_touch_event(&self, contacts: &[rdpei::RdpInputContact]) -> Option<Vec<u8>> {
        let payload = self.rdpei.touch_event(contacts)?;
        self.rdpei_channel_id.map(|id| drdynvc::data(id, &payload))
    }

    /// Build a frame-acknowledge to send on the graphics channel (wrapped as a
    /// DRDYNVC data PDU), or `None` if the channel isn't open. `queue_depth` is
    /// the client's current decode backlog (frames parsed but not yet rendered) —
    /// the server uses it for flow control, slowing its send rate when the client
    /// falls behind. `0` means "caught up".
    pub fn frame_ack(
        &self,
        frame_id: u32,
        total_frames_decoded: u32,
        queue_depth: u32,
    ) -> Option<Vec<u8>> {
        self.gfx_channel_id.map(|id| {
            drdynvc::data(
                id,
                &self
                    .pipeline
                    .frame_acknowledge(queue_depth, frame_id, total_frames_decoded),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdp_pdu::gfx;

    /// Wrap raw EGFX bytes as an uncompressed single-segment ZGFX blob, then as
    /// a DRDYNVC data PDU for `channel_id`.
    fn dvc_egfx(channel_id: u32, egfx: &[u8]) -> Vec<u8> {
        let mut zgfx = vec![0xE0, 0x04];
        zgfx.extend_from_slice(egfx);
        drdynvc::data(channel_id, &zgfx)
    }

    fn egfx_pdu(cmd_id: u16, body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&cmd_id.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn answers_capabilities_request() {
        let mut gc = GraphicsChannel::new();
        // DRDYNVC caps request: header 0x50, pad, version=1.
        let out = gc.process(&[0x50, 0x00, 0x01, 0x00]);
        assert_eq!(out.responses, vec![drdynvc::capabilities_response(1)]);
        assert!(out.commands.is_empty());
    }

    #[test]
    fn creates_graphics_channel_and_advertises() {
        let mut gc = GraphicsChannel::new();
        let mut create = vec![0x10, 0x03]; // CREATE, cb=0, channelId=3
        create.extend_from_slice(names::GRAPHICS.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(gc.channel_id(), Some(3));
        // create-response (accept) then the caps-advertise wrapped as DVC data.
        assert_eq!(out.responses.len(), 2);
        assert_eq!(out.responses[0], drdynvc::create_response(3, 0));
        // The second response is DVC data carrying a CAPS_ADVERTISE EGFX PDU.
        assert_eq!(out.responses[1][0], 0x30); // DATA, cb=0
    }

    #[test]
    fn declines_unknown_channel() {
        let mut gc = GraphicsChannel::new();
        let mut create = vec![0x10, 0x05];
        create.extend_from_slice(b"Some::Other::Channel");
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(gc.channel_id(), None);
        assert_eq!(
            out.responses,
            vec![drdynvc::create_response(5, STATUS_DECLINE)]
        );
    }

    #[test]
    fn declines_media_redirection_channels() {
        // The server's WebRTC/MMR/RDPEVOR redirection channels must be declined
        // (accepting one without a redirector would steal video off the graphics
        // channel we decode) — same wire status as any decline.
        for name in [
            "com.microsoft.rdc.dvc.webrtc.1",
            "com.microsoft.rdc.dvc.mmr.1",
            "Microsoft::Windows::RDS::Video::Data::v08.01",
            "Microsoft::Windows::RDS::Geometry::v08.01",
        ] {
            assert!(names::is_redirection_channel(name), "{name} not classified");
            let mut gc = GraphicsChannel::new();
            let mut create = vec![0x10, 0x07];
            create.extend_from_slice(name.as_bytes());
            create.push(0);
            let out = gc.process(&create);
            assert_eq!(gc.channel_id(), None);
            assert_eq!(out.responses, vec![drdynvc::create_response(7, STATUS_DECLINE)]);
        }
    }

    #[test]
    fn decodes_graphics_data_into_commands() {
        let mut gc = GraphicsChannel::new();
        // Open the channel first.
        let mut create = vec![0x10, 0x03];
        create.extend_from_slice(names::GRAPHICS.as_bytes());
        create.push(0);
        gc.process(&create);

        // A START_FRAME EGFX PDU arrives on channel 3, ZGFX+DVC wrapped.
        let egfx = egfx_pdu(gfx::CMDID_START_FRAME, &[0x10, 0, 0, 0, 0x09, 0, 0, 0]);
        let out = gc.process(&dvc_egfx(3, &egfx));
        assert_eq!(
            out.commands,
            vec![GfxCommand::StartFrame {
                timestamp: 16,
                frame_id: 9,
            }]
        );
    }

    #[test]
    fn ignores_data_for_unopened_channel() {
        let mut gc = GraphicsChannel::new();
        let egfx = egfx_pdu(gfx::CMDID_END_FRAME, &[1, 0, 0, 0]);
        let out = gc.process(&dvc_egfx(9, &egfx));
        assert!(out.commands.is_empty());
        assert!(out.responses.is_empty());
    }

    #[test]
    fn opens_display_control_and_requests_resize() {
        let mut gc = GraphicsChannel::new();
        let monitors = &[rdp_pdu::gcc::MonitorDef {
            left: 0,
            top: 0,
            right: 1279,
            bottom: 719,
            primary: true,
        }];
        assert!(gc.request_resize(monitors).is_none()); // channel not open yet
        let mut create = vec![0x10, 0x07]; // CREATE, cb=0, channelId=7
        create.extend_from_slice(disp::DISPLAYCONTROL_CHANNEL.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(out.responses, vec![drdynvc::create_response(7, 0)]); // accepted
        let resize = gc.request_resize(monitors).unwrap();
        assert_eq!(resize[0], 0x30); // DVC DATA wrapping the monitor-layout PDU
    }

    #[test]
    fn opens_audio_input_and_routes_its_data() {
        let mut gc = GraphicsChannel::new();
        assert!(!gc.audio_input_open());
        assert!(gc.wrap_audio_input(&[0x01]).is_none());
        // CREATE AUDIO_INPUT on channel 9.
        let mut create = vec![0x10, 0x09];
        create.extend_from_slice(names::AUDIO_INPUT.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(out.responses, vec![drdynvc::create_response(9, 0)]); // accepted
        assert!(gc.audio_input_open());
        // A data PDU on channel 9 surfaces as an audio_input message.
        let out = gc.process(&drdynvc::data(9, &[0x01, 0x00, 0x00, 0x00, 0x01]));
        assert_eq!(out.audio_input, vec![vec![0x01, 0x00, 0x00, 0x00, 0x01]]);
        // Outbound mic PDUs wrap as DVC data on channel 9.
        let wrapped = gc.wrap_audio_input(&[0x06, 0xAA]).unwrap();
        assert_eq!(wrapped[0], 0x30); // DATA, cb=0
    }

    #[test]
    fn opens_audio_playback_and_routes_its_data() {
        let mut gc = GraphicsChannel::new();
        assert!(!gc.audio_output_open());
        assert!(gc.wrap_audio_output(&[0x01]).is_none());
        // CREATE AUDIO_PLAYBACK_DVC on channel 16 (as a modern Windows host does).
        let mut create = vec![0x10, 0x10];
        create.extend_from_slice(names::AUDIO_PLAYBACK.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(out.responses, vec![drdynvc::create_response(16, 0)]); // accepted
        assert!(gc.audio_output_open());
        // A data PDU on channel 16 surfaces as an audio_output (RDPSND) message.
        let out = gc.process(&drdynvc::data(16, &[0x07, 0x00, 0x04, 0x00]));
        assert_eq!(out.audio_output, vec![vec![0x07, 0x00, 0x04, 0x00]]);
        // Outbound RDPSND replies (e.g. wave confirm) wrap as DVC data on chan 16.
        assert_eq!(gc.wrap_audio_output(&[0x05, 0x00]).unwrap()[0], 0x30);
    }

    #[test]
    fn opens_camera_enumerator_and_routes_its_data() {
        let mut gc = GraphicsChannel::new();
        assert!(!gc.camera_open());
        let mut create = vec![0x10, 0x0B]; // CREATE channelId=11
        create.extend_from_slice(names::CAMERA_ENUMERATOR.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(out.responses, vec![drdynvc::create_response(11, 0)]);
        assert!(gc.camera_open());
        let out = gc.process(&drdynvc::data(11, &[0x01, 0x03]));
        assert_eq!(out.camera, vec![vec![0x01, 0x03]]);
        assert_eq!(gc.wrap_camera(&[0x01, 0x04]).unwrap()[0], 0x30);
    }

    #[test]
    fn frame_ack_only_after_channel_open() {
        let mut gc = GraphicsChannel::new();
        assert!(gc.frame_ack(1, 1, 0).is_none());
        let mut create = vec![0x10, 0x03];
        create.extend_from_slice(names::GRAPHICS.as_bytes());
        create.push(0);
        gc.process(&create);
        let ack = gc.frame_ack(1, 1, 0).unwrap();
        assert_eq!(ack[0], 0x30); // DVC DATA wrapping the frame-ack
    }

    #[test]
    fn opens_rdpei_and_routes_server_ready() {
        use rdp_channels::rdpei::{CONTACT_FLAG_DOWN, CONTACT_FLAG_INCONTACT, CONTACT_FLAG_INRANGE, RdpInputContact};
        let mut gc = GraphicsChannel::new();
        assert!(!gc.rdpei_ready());
        assert!(gc.wrap_touch_event(&[]).is_none());

        // Server opens the RDPEI channel.
        let mut create = vec![0x10, 0x12]; // CREATE, cb=0, channelId=18
        create.extend_from_slice(names::RDPINPUT.as_bytes());
        create.push(0);
        let out = gc.process(&create);
        assert_eq!(out.responses, vec![drdynvc::create_response(18, 0)]);
        assert!(!gc.rdpei_ready()); // handshake not done yet

        // Server sends SC_READY.
        let mut sc_ready = vec![0u8; 10];
        sc_ready[0..2].copy_from_slice(&0x0001u16.to_le_bytes());
        sc_ready[2..6].copy_from_slice(&10u32.to_le_bytes());
        sc_ready[6..10].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        let out = gc.process(&drdynvc::data(18, &sc_ready));
        assert_eq!(out.responses.len(), 1);
        assert_eq!(out.responses[0][0], 0x30); // DVC DATA wrapping client-ready
        assert!(gc.rdpei_ready());

        // A touch frame now wraps as DVC data on channel 18.
        let contacts = [RdpInputContact {
            id: 1,
            x: 100,
            y: 200,
            flags: CONTACT_FLAG_DOWN | CONTACT_FLAG_INCONTACT | CONTACT_FLAG_INRANGE,
        }];
        let wrapped = gc.wrap_touch_event(&contacts).unwrap();
        assert_eq!(wrapped[0], 0x30); // DVC DATA
    }
}
