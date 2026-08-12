//! Drive, clipboard, audio, mic, camera, and printer redirection.
//!
//! Redirection is carried over static virtual channels (`cliprdr`, `rdpdr`,
//! `rdpsnd`, `drdynvc`) declared during the MCS connect and joined before the
//! session becomes active. This module owns the *configuration* side — which
//! channels are enabled and what the local resources are — and maps a received
//! channel id back to a purpose. The wire-side PDU encoding for each channel
//! lives in the workspace's `rdp-channels` crate; this module only decides
//! what to open and where to route bytes.

use wire_main::{ChannelPurpose, WireError};

/// Clipboard redirection static channel.
pub const CLIPBOARD_CHANNEL: &str = "cliprdr";
/// Drive + printer redirection static channel.
pub const RDPDR_CHANNEL: &str = "rdpdr";
/// Audio playback static channel.
pub const RDPSND_CHANNEL: &str = "rdpsnd";
/// Dynamic virtual channel manager (audio input, camera, graphics).
pub const DRDYNVC_CHANNEL: &str = "drdynvc";

/// Redirection configuration derived from the CLI flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedirectionConfig {
    /// Share the clipboard (`cliprdr`).
    pub clipboard: bool,
    /// Local directories/roots to share as drives (rdpdr device list).
    pub drive_paths: Vec<String>,
    /// Play session audio on the client (`rdpsnd`).
    pub audio_playback: bool,
    /// Capture the microphone into the session (drdynvc `AUDIO_INPUT`).
    pub audio_input: bool,
    /// Enumerate and stream client cameras (drdynvc).
    pub camera: bool,
    /// Redirect client printers (rdpdr).
    pub printer: bool,
}

/// A channel this configuration wants to open, with its local resource kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Clipboard,
    Drive,
    Printer,
    AudioOutput,
    Microphone,
    Camera,
}

/// A resource the client offers into the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRoute {
    pub channel: &'static str,
    pub kind: DeviceKind,
}

/// Configuration errors detected before connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectionError {
    /// A drive path appears more than once.
    DuplicateDrive(String),
    /// A drive path entry is empty.
    EmptyDrivePath,
}

impl std::fmt::Display for RedirectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedirectionError::DuplicateDrive(path) => {
                write!(f, "drive path `{path}` is listed more than once")
            }
            RedirectionError::EmptyDrivePath => write!(f, "drive path must not be empty"),
        }
    }
}

impl std::error::Error for RedirectionError {}

impl From<RedirectionError> for WireError {
    fn from(e: RedirectionError) -> Self {
        WireError::Protocol(e.to_string())
    }
}

impl RedirectionConfig {
    /// Whether drive/printer redirection (rdpdr) is requested at all.
    pub fn wants_rdpdr(&self) -> bool {
        !self.drive_paths.is_empty() || self.printer
    }

    /// The static channels to declare in CS_NET, in the exact order the
    /// session layer will join them (`cliprdr`, `rdpdr`, `rdpsnd`, `drdynvc`).
    pub fn channel_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.clipboard {
            names.push(CLIPBOARD_CHANNEL.to_string());
        }
        if self.wants_rdpdr() {
            names.push(RDPDR_CHANNEL.to_string());
        }
        if self.audio_playback {
            names.push(RDPSND_CHANNEL.to_string());
        }
        // drdynvc always, for the dynamic channels (mic/camera/graphics).
        names.push(DRDYNVC_CHANNEL.to_string());
        names
    }

    /// Whether the given channel purpose is enabled by this configuration.
    pub fn is_enabled(&self, purpose: ChannelPurpose) -> bool {
        match purpose {
            ChannelPurpose::Clipboard => self.clipboard,
            ChannelPurpose::DrivesAndPrinters => self.wants_rdpdr(),
            ChannelPurpose::AudioPlayback => self.audio_playback,
            ChannelPurpose::Dynamic => true,
        }
    }

    /// Map a static channel name to its purpose.
    pub fn purpose_for_channel(name: &str) -> Option<ChannelPurpose> {
        match name {
            CLIPBOARD_CHANNEL => Some(ChannelPurpose::Clipboard),
            RDPDR_CHANNEL => Some(ChannelPurpose::DrivesAndPrinters),
            RDPSND_CHANNEL => Some(ChannelPurpose::AudioPlayback),
            DRDYNVC_CHANNEL => Some(ChannelPurpose::Dynamic),
            _ => None,
        }
    }

    /// The local resources offered into the session, one route per purpose.
    pub fn routes(&self) -> Vec<DeviceRoute> {
        let mut routes = Vec::new();
        if self.clipboard {
            routes.push(DeviceRoute {
                channel: CLIPBOARD_CHANNEL,
                kind: DeviceKind::Clipboard,
            });
        }
        for path in &self.drive_paths {
            routes.push(DeviceRoute {
                channel: RDPDR_CHANNEL,
                kind: DeviceKind::Drive,
            });
            let _ = path;
        }
        if self.printer {
            routes.push(DeviceRoute {
                channel: RDPDR_CHANNEL,
                kind: DeviceKind::Printer,
            });
        }
        if self.audio_playback {
            routes.push(DeviceRoute {
                channel: RDPSND_CHANNEL,
                kind: DeviceKind::AudioOutput,
            });
        }
        if self.audio_input {
            routes.push(DeviceRoute {
                channel: DRDYNVC_CHANNEL,
                kind: DeviceKind::Microphone,
            });
        }
        if self.camera {
            routes.push(DeviceRoute {
                channel: DRDYNVC_CHANNEL,
                kind: DeviceKind::Camera,
            });
        }
        routes
    }

    /// Validate the configuration before opening a connection.
    pub fn validate(&self) -> Result<(), RedirectionError> {
        let mut seen = std::collections::HashSet::new();
        for path in &self.drive_paths {
            if path.trim().is_empty() {
                return Err(RedirectionError::EmptyDrivePath);
            }
            if !seen.insert(path.clone()) {
                return Err(RedirectionError::DuplicateDrive(path.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_declares_only_drdynvc() {
        let cfg = RedirectionConfig::default();
        assert_eq!(cfg.channel_names(), ["drdynvc"]);
        assert!(!cfg.wants_rdpdr());
    }

    #[test]
    fn all_channels_in_declaration_order() {
        let cfg = RedirectionConfig {
            clipboard: true,
            drive_paths: vec!["C:".into()],
            audio_playback: true,
            printer: true,
            ..Default::default()
        };
        assert_eq!(
            cfg.channel_names(),
            ["cliprdr", "rdpdr", "rdpsnd", "drdynvc"]
        );
    }

    #[test]
    fn rdpdr_implied_by_printer_only() {
        let cfg = RedirectionConfig {
            printer: true,
            ..Default::default()
        };
        assert!(cfg.wants_rdpdr());
        assert!(cfg.channel_names().contains(&"rdpdr".to_string()));
    }

    #[test]
    fn purposes_map_to_channels() {
        assert_eq!(
            RedirectionConfig::purpose_for_channel("cliprdr"),
            Some(ChannelPurpose::Clipboard)
        );
        assert_eq!(
            RedirectionConfig::purpose_for_channel("rdpdr"),
            Some(ChannelPurpose::DrivesAndPrinters)
        );
        assert_eq!(
            RedirectionConfig::purpose_for_channel("rdpsnd"),
            Some(ChannelPurpose::AudioPlayback)
        );
        assert_eq!(
            RedirectionConfig::purpose_for_channel("drdynvc"),
            Some(ChannelPurpose::Dynamic)
        );
        assert_eq!(RedirectionConfig::purpose_for_channel("bogus"), None);
    }

    #[test]
    fn is_enabled_tracks_flags() {
        let cfg = RedirectionConfig {
            clipboard: true,
            audio_input: true,
            camera: true,
            ..Default::default()
        };
        assert!(cfg.is_enabled(ChannelPurpose::Clipboard));
        assert!(!cfg.is_enabled(ChannelPurpose::DrivesAndPrinters));
        assert!(cfg.is_enabled(ChannelPurpose::Dynamic));
    }

    #[test]
    fn routes_cover_enabled_purposes() {
        let cfg = RedirectionConfig {
            clipboard: true,
            drive_paths: vec!["C:".into(), "D:".into()],
            audio_playback: true,
            audio_input: true,
            camera: true,
            printer: true,
        };
        let routes = cfg.routes();
        assert!(routes.iter().any(|r| r.kind == DeviceKind::Clipboard));
        assert_eq!(
            routes
                .iter()
                .filter(|r| r.kind == DeviceKind::Drive)
                .count(),
            2
        );
        assert!(routes.iter().any(|r| r.kind == DeviceKind::Printer));
        assert!(routes.iter().any(|r| r.kind == DeviceKind::AudioOutput));
        assert!(routes.iter().any(|r| r.kind == DeviceKind::Microphone));
        assert!(routes.iter().any(|r| r.kind == DeviceKind::Camera));
    }

    #[test]
    fn validation_rejects_duplicate_and_empty_drives() {
        let dup = RedirectionConfig {
            drive_paths: vec!["C:".into(), "C:".into()],
            ..Default::default()
        };
        assert_eq!(
            dup.validate(),
            Err(RedirectionError::DuplicateDrive("C:".into()))
        );
        let empty = RedirectionConfig {
            drive_paths: vec!["  ".into()],
            ..Default::default()
        };
        assert_eq!(empty.validate(), Err(RedirectionError::EmptyDrivePath));
        let ok = RedirectionConfig {
            drive_paths: vec!["C:".into()],
            ..Default::default()
        };
        assert_eq!(ok.validate(), Ok(()));
    }
}
