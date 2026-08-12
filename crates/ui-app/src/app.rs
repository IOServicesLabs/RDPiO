//! Platform-agnostic application controller.
//!
//! The controller owns the parsed [`CliOptions`](crate::cli::CliOptions), the
//! redirection configuration, and the connected [`WireSession`]. Both platform
//! front-ends (the Win32 + D3D11 window on Windows, the headless fallback
//! elsewhere) drive this same type, so connection setup, PDU I/O, input and
//! redirection forwarding, and teardown behave identically and can be
//! unit-tested without a window or a GPU.
//!
//! The golden rule enforced here: the session is only opened *after*
//! connection setup succeeds. [`AppController::connect`] returns `Ok` only
//! once `WireSession::connect` has completed the full RDP connection sequence
//! (X.224, MCS, security exchange, channel joins, licensing, capability
//! exchange). The render loop must not start before that.

use crate::cli::CliOptions;
use crate::redirection::RedirectionConfig;
use wire_main::{
    ConnectOptions, InputEvent, ServerPdu, WireError, WireResult, WireSession, WireTransport,
};

/// Events surfaced by [`AppController::poll`].
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A decoded-frame candidate: the H.264 access unit plus the SPS-derived
    /// resolution when known. The Windows front-end feeds it to the D3D11
    /// decoder and then to the render callback.
    Frame {
        access_unit: Vec<u8>,
        is_keyframe: bool,
        width: Option<u16>,
        height: Option<u16>,
    },
    /// Clipboard data from the `cliprdr` channel.
    Clipboard { data: Vec<u8> },
    /// Any other virtual-channel data (rdpdr, rdpsnd, drdynvc, ...).
    DeviceData { channel: String, data: Vec<u8> },
    /// The server requested multitransport over the UDP side-band.
    MultitransportRequest,
    /// The server closed the connection.
    Disconnected,
}

/// The application controller: options + redirection config + live session.
pub struct AppController {
    opts: CliOptions,
    redirection: RedirectionConfig,
    session: Option<WireSession>,
}

impl AppController {
    /// Create a controller from parsed options. The connection is *not*
    /// opened until [`Self::connect`] is called.
    pub fn new(opts: CliOptions) -> Self {
        Self {
            opts,
            redirection: RedirectionConfig::default(),
            session: None,
        }
    }

    /// Create a controller with an explicit redirection configuration.
    pub fn with_redirection(opts: CliOptions, redirection: RedirectionConfig) -> Self {
        Self {
            opts,
            redirection,
            session: None,
        }
    }

    /// The parsed options.
    pub fn opts(&self) -> &CliOptions {
        &self.opts
    }

    /// The redirection configuration.
    pub fn redirection(&self) -> &RedirectionConfig {
        &self.redirection
    }

    /// Whether a session is currently connected.
    pub fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    /// The negotiated session settings, once connected.
    pub fn settings(&self) -> Option<&wire_main::SessionSettings> {
        self.session.as_ref().map(|s| s.settings())
    }

    /// Access to the underlying transport (peer address, UDP side-band).
    pub fn transport(&mut self) -> Option<&mut WireTransport> {
        self.session.as_mut().map(|s| s.transport())
    }

    /// Open the connection. Returns `Ok` only after the full RDP connection
    /// sequence completes. Idempotent: a second call while connected is a
    /// no-op.
    pub fn connect(&mut self) -> WireResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        self.redirection.validate()?;

        // The client desktop spans the physical monitors; single-monitor
        // layouts fall back to the requested width/height.
        let layout = crate::monitor::layout(&crate::monitor::enumerate_windows_monitors());
        let monitors = if layout.monitors.len() > 1 {
            layout.monitors.clone()
        } else {
            Vec::new()
        };

        let opts = ConnectOptions {
            host: self.opts.host.clone(),
            port: self.opts.port,
            username: self.opts.user.clone().unwrap_or_default(),
            password: self.opts.password.clone().unwrap_or_default(),
            domain: String::new(),
            width: self.opts.width,
            height: self.opts.height,
            color_depth: 24,
            client_name: hostname_fallback("rdpio"),
            insecure: self.opts.insecure,
            udp: self.opts.udp,
            clipboard: self.redirection.clipboard,
            drive_redirection: !self.redirection.drive_paths.is_empty(),
            audio_playback: self.redirection.audio_playback,
            audio_input: self.redirection.audio_input,
            camera: self.redirection.camera,
            printer: self.redirection.printer,
            monitors,
        };
        let mut session = WireSession::connect(opts)?;
        // Switch to poll mode so the message loop never blocks on recv.
        session.set_poll_timeout(std::time::Duration::from_millis(50))?;
        tracing::info!(
            io_channel = session.settings().io_channel,
            "session connected",
        );
        self.session = Some(session);
        Ok(())
    }

    /// Forward keyboard/mouse input events to the session.
    pub fn send_input(&mut self, events: &[InputEvent]) -> WireResult<()> {
        match self.session.as_mut() {
            Some(s) => s.send_input(events),
            None => Err(WireError::Sequence(
                "cannot send input: session is not connected".into(),
            )),
        }
    }

    /// Forward a redirection payload on a static virtual channel.
    pub fn send_redirection(&mut self, channel: &str, payload: &[u8]) -> WireResult<()> {
        match self.session.as_mut() {
            Some(s) => s.send_channel_data(channel, payload),
            None => Err(WireError::Sequence(
                "cannot send redirection: session is not connected".into(),
            )),
        }
    }

    /// Poll for one server event. Returns `Ok(None)` when the poll timeout
    /// expires with nothing to report (callers retry on their own cadence).
    pub fn poll(&mut self) -> WireResult<Option<AppEvent>> {
        let session = match self.session.as_mut() {
            Some(s) => s,
            None => return Ok(None),
        };
        match session.recv() {
            Ok(pdu) => Ok(Some(self.map_pdu(pdu))),
            Err(WireError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(WireError::Closed) => Ok(Some(AppEvent::Disconnected)),
            Err(e) => Err(e),
        }
    }

    /// Tear down the session gracefully. Safe to call multiple times.
    pub fn close(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.close();
        }
        self.session = None;
    }

    fn map_pdu(&self, pdu: ServerPdu) -> AppEvent {
        match pdu {
            ServerPdu::ChannelData { channel_id, data } => {
                let name = self
                    .settings()
                    .and_then(|s| {
                        s.static_channels
                            .iter()
                            .find(|c| c.id == channel_id)
                            .map(|c| c.name.clone())
                    })
                    .unwrap_or_else(|| format!("#{channel_id}"));
                if name == crate::redirection::CLIPBOARD_CHANNEL {
                    AppEvent::Clipboard { data }
                } else {
                    AppEvent::DeviceData {
                        channel: name,
                        data,
                    }
                }
            }
            ServerPdu::Update(unit) | ServerPdu::GraphicsUpdate(unit) => {
                let (is_keyframe, cfg) = frame_meta(&unit);
                AppEvent::Frame {
                    access_unit: unit,
                    is_keyframe,
                    width: cfg.as_ref().map(|c| c.width),
                    height: cfg.as_ref().map(|c| c.height),
                }
            }
            ServerPdu::MultitransportRequest => AppEvent::MultitransportRequest,
            ServerPdu::Disconnect => AppEvent::Disconnected,
            ServerPdu::Other(_) => AppEvent::DeviceData {
                channel: "io".into(),
                data: Vec::new(),
            },
        }
    }
}

/// Parse frame metadata out of a (possibly Annex-B) H.264 payload.
fn frame_meta(payload: &[u8]) -> (bool, Option<crate::decode::H264Config>) {
    let nals = crate::decode::annex_b_nal_units(payload);
    let is_keyframe = nals
        .iter()
        .any(|n| n.nal_type == crate::decode::NAL_TYPE_IDR);
    let cfg = crate::decode::parse_config(&nals);
    (is_keyframe, cfg)
}

/// The client computer name advertised in CS_CORE.
fn hostname_fallback(fallback: &str) -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

impl Drop for AppController {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(host: &str) -> CliOptions {
        CliOptions {
            host: host.into(),
            insecure: true,
            ..Default::default()
        }
    }

    #[test]
    fn controller_starts_disconnected() {
        let c = AppController::new(opts("127.0.0.1"));
        assert!(!c.is_connected());
        assert!(c.settings().is_none());
    }

    #[test]
    fn send_without_connect_errors() {
        let mut c = AppController::new(opts("127.0.0.1"));
        assert!(matches!(c.send_input(&[]), Err(WireError::Sequence(_))));
        assert!(matches!(
            c.send_redirection("cliprdr", &[1, 2, 3]),
            Err(WireError::Sequence(_))
        ));
    }

    #[test]
    fn insecure_disabled_connection_rejected() {
        // `--insecure` off means the wire-main transport refuses to connect
        // (no TLS stack is vendored in this crate); the controller must
        // surface that failure instead of pretending the session is up.
        let mut c = AppController::new(CliOptions {
            host: "127.0.0.1".into(),
            port: 1,
            insecure: false,
            ..Default::default()
        });
        assert!(matches!(c.connect(), Err(WireError::Unsupported(_))));
        assert!(!c.is_connected());
    }

    #[test]
    fn duplicate_drive_rejected_before_connecting() {
        let mut c = AppController::new(opts("127.0.0.1"));
        let redirection = RedirectionConfig {
            drive_paths: vec!["C:".into(), "C:".into()],
            ..Default::default()
        };
        c.redirection = redirection;
        assert!(matches!(c.connect(), Err(WireError::Protocol(_))));
        assert!(!c.is_connected());
    }

    #[test]
    fn close_is_idempotent() {
        let mut c = AppController::new(opts("127.0.0.1"));
        c.close();
        c.close();
        assert!(!c.is_connected());
    }

    #[test]
    fn redirection_config_maps_from_options() {
        let opts = CliOptions {
            host: "h".into(),
            drive_paths: vec!["C:".into()],
            mic: true,
            camera: true,
            printer: true,
            clipboard: false,
            audio_playback: false,
            ..Default::default()
        };
        let redirection = RedirectionConfig {
            clipboard: opts.clipboard,
            drive_paths: opts.drive_paths.clone(),
            audio_playback: opts.audio_playback,
            audio_input: opts.mic,
            camera: opts.camera,
            printer: opts.printer,
        };
        assert_eq!(redirection.channel_names(), ["rdpdr", "drdynvc"]);
        assert!(redirection.wants_rdpdr());
    }
}
