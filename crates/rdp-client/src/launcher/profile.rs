//! What the launcher keeps beside the hub's stores: each connection's session
//! options (how it streams, how it looks) and its Hyper password, the options
//! a quick connect uses, and the theme. In `%LOCALAPPDATA%\rdpio\launcher.json`,
//! written atomically like the hub's files; the hub's `connections.json`
//! (shared with the in-session connection bar) is left exactly as it was.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hub::{HubError, ProtectedPassword};

const FILE: &str = "launcher.json";

/// How a connection reaches the other PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Plain RDP.
    Rdp,
    /// Sign in over RDP, then switch to the Hyper stream when the host runs it.
    #[default]
    Hyper,
    /// Sign in to the Hyper service with the Hyper password (no RDP at all).
    Direct,
}

/// What the stream is tuned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Lowest latency (motion first).
    #[default]
    Game,
    /// Crisp text, native resolution where the host has it.
    Office,
}

/// One connection's session options. Every field has a default, so a file from
/// an older launcher (or a hand edit that drops one) still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub transport: Transport,
    pub profile: Profile,
    /// Office text with the encoder's quality preset: new text sharp at once,
    /// for ~8 ms more latency on every frame. (Otherwise the fast preset,
    /// whose still picture sharpens past it within half a second. Profiles
    /// that saved the old `fast_text` get the fast preset.)
    pub best_text: bool,
    /// "auto", "hevc" or "h264".
    pub codec: String,
    /// Frames per second to ask for; 0 = this screen's refresh rate.
    pub fps: u32,
    /// Hyper: the stream's size as a share of the window, pinned: 100, 75, 50
    /// or 33 (%); 0 = automatic (it follows the link).
    pub scale: u32,
    pub fullscreen: bool,
    pub multimon: bool,
    /// RDP: present without waiting for vblank (tearing, lowest latency). A
    /// Hyper stream presents that way unless `vsync` is on.
    pub low_latency: bool,
    /// Hyper: present on vblank.
    pub vsync: bool,
    /// The Hyper service's UDP port; 0 = the default.
    pub hyper_port: u16,
    /// Hyper Direct: which GPU decodes and presents — "auto" (the one the
    /// screen is on while on battery, the fastest plugged in),
    /// "performance" or "display".
    pub gpu: String,
    /// Hyper: Win, Alt+Tab, Alt+F4 stay on this PC (Ctrl+Shift+K switches).
    pub local_keys: bool,
    pub printer: bool,
    /// Share every local drive into an RDP session.
    pub drives: bool,
    /// Teams "Optimized" calls in an RDP session: host the WebRTC redirector
    /// add-in so the camera and microphone work in Teams.
    pub teams: bool,
    /// Accept a host certificate Windows doesn't trust (RDP).
    pub insecure: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            transport: Transport::default(),
            profile: Profile::default(),
            best_text: false,
            codec: "auto".into(),
            fps: 0,
            scale: 0,
            fullscreen: true,
            multimon: false,
            low_latency: false,
            vsync: false,
            hyper_port: 0,
            gpu: "auto".into(),
            local_keys: false,
            printer: false,
            drives: false,
            teams: false,
            insecure: false,
        }
    }
}

/// A Cloud PC's redirects beyond what [`Options`] holds.
pub const CLOUD_PC_REDIRECTS: [&str; 2] = ["--printers", "--com"];

impl Options {
    /// A Cloud PC's session: nothing of Hyper can be installed there, so plain
    /// RDP at its fastest (motion first, presents that don't wait for vblank)
    /// with every redirect on. Only the window's shape follows `defaults`.
    pub fn cloud_pc(defaults: &Self) -> Self {
        Self {
            transport: Transport::Rdp,
            profile: Profile::Game,
            low_latency: true,
            // (Every printer: see `CLOUD_PC_REDIRECTS`.)
            printer: false,
            drives: true,
            insecure: false,
            fullscreen: defaults.fullscreen,
            multimon: defaults.multimon,
            ..Self::default()
        }
    }

    /// The session's command line (passwords never: they go by environment).
    pub fn to_args(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| out.push(s.to_string());
        match self.transport {
            Transport::Rdp => {}
            Transport::Hyper => push("--hyper"),
            Transport::Direct => push("--direct"),
        }
        match (self.profile, self.transport) {
            (Profile::Office, _) => push("--office"),
            // Plain RDP's motion-first preset; Hyper streams start in game mode.
            (Profile::Game, Transport::Rdp) => push("--gaming"),
            (Profile::Game, _) => {}
        }
        if self.transport != Transport::Rdp {
            if self.best_text {
                push("--hyper-text");
                push("best");
            }
            if matches!(self.codec.as_str(), "hevc" | "h264") {
                push("--hyper-codec");
                push(&self.codec.clone());
            }
            if self.fps > 0 {
                push("--hyper-fps");
                push(&self.fps.min(480).to_string());
            }
            if matches!(self.scale, 100 | 75 | 50 | 33) {
                push("--hyper-scale");
                push(&self.scale.to_string());
            }
            if self.vsync {
                push("--hyper-vsync");
            }
            if self.hyper_port != 0 {
                push("--hyper-port");
                push(&self.hyper_port.to_string());
            }
            if self.transport == Transport::Direct && matches!(self.gpu.as_str(), "performance" | "display") {
                push("--hyper-gpu");
                push(&self.gpu.clone());
            }
            if self.local_keys {
                push("--hyper-local-keys");
            }
        }
        if self.fullscreen {
            push("--fullscreen");
        }
        if self.multimon && self.transport == Transport::Rdp {
            push("--multimon");
        }
        // (Only RDP's presents follow it: a Hyper stream sets its own, so
        // Hyper Direct, which has no RDP part, takes no flag.)
        if self.low_latency && self.transport != Transport::Direct {
            push("--low-latency");
        }
        if self.transport != Transport::Direct {
            if self.printer {
                push("--printer");
            }
            if self.drives {
                push("--drive");
                push("all");
            }
            if self.teams && self.transport == Transport::Rdp {
                push("--teams");
            }
            if self.insecure {
                push("--insecure");
            }
        }
        out
    }
}

/// A saved connection's launcher side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub options: Options,
    /// DPAPI-protected (this Windows user only).
    pub hyper_password: Option<ProtectedPassword>,
    /// The Workspace folder it's filed under ("" = none; "/" nests:
    /// "Work/Servers").
    pub folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherFile {
    /// "system", "dark" or "light".
    pub theme: String,
    /// What a quick connect (and a new connection) starts with.
    pub defaults: Options,
    /// By saved connection id.
    pub entries: BTreeMap<uuid::Uuid, Entry>,
    /// The options a Recent entry was last launched with, by
    /// "host:port|user" (recents that aren't saved connections).
    pub recent: BTreeMap<String, Options>,
    pub updates: UpdatePrefs,
    /// Full-screen sessions get the bar at the top (Pin, Minimize, Window,
    /// Stats, Save, Disconnect). Off: the hotkeys only.
    pub session_bar: bool,
}

impl Default for LauncherFile {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            defaults: Options::default(),
            entries: BTreeMap::new(),
            recent: BTreeMap::new(),
            updates: UpdatePrefs::default(),
            session_bar: true,
        }
    }
}

/// How this PC's rdpio keeps up to date (see `updater`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatePrefs {
    /// Look for a new release on GitHub when the launcher opens.
    pub check: bool,
    /// Take a newer rdpio from a Hyper host while connected to it.
    pub from_hosts: bool,
    /// "stable" (releases) or "nightly" (every build of main).
    pub channel: String,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self { check: true, from_hosts: true, channel: "stable".into() }
    }
}

/// The key a Recent entry's options are kept under.
pub fn recent_key(host: &str, port: u16, user: Option<&str>) -> String {
    format!("{}:{port}|{}", host.trim().to_ascii_lowercase(), user.unwrap_or("").trim().to_ascii_lowercase())
}

pub struct Store {
    path: PathBuf,
    pub file: LauncherFile,
}

impl Store {
    pub fn load() -> Result<Self, HubError> {
        Self::load_from(crate::hub::store::data_dir()?.join(FILE))
    }

    pub fn load_from(path: PathBuf) -> Result<Self, HubError> {
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "corrupt launcher settings; starting fresh");
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                LauncherFile::default()
            }),
            Err(_) => LauncherFile::default(),
        };
        Ok(Self { path, file })
    }

    pub fn save(&self) -> Result<(), HubError> {
        crate::hub::store::write_json_atomic(&self.path, &self.file)
    }

    pub fn entry(&self, id: &uuid::Uuid) -> Entry {
        self.file.entries.get(id).cloned().unwrap_or_else(|| Entry { options: self.file.defaults.clone(), ..Entry::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_round_trip_and_old_files_load() {
        let o = Options { transport: Transport::Direct, profile: Profile::Office, fps: 120, scale: 50, ..Options::default() };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<Options>(&json).unwrap(), o);
        // A file missing fields (an older launcher's) gets the defaults.
        let old: Options = serde_json::from_str(r#"{"transport":"rdp"}"#).unwrap();
        assert_eq!(old.transport, Transport::Rdp);
        assert_eq!(old.codec, "auto");
        assert_eq!(old.scale, 0);
        assert!(old.fullscreen);
        let file: LauncherFile = serde_json::from_str("{}").unwrap();
        assert_eq!(file.theme, "system");
    }

    /// A `launcher.json` as 0.4.1 wrote it: every option it had comes back,
    /// and the sessions it starts get the command lines they got then.
    #[test]
    fn a_file_from_0_4_1_loads_with_every_option() {
        let json = r#"{
            "theme": "dark",
            "defaults": {"transport":"hyper","profile":"game","best_text":false,"codec":"auto","fps":0,"fullscreen":true,"multimon":false,
                "low_latency":false,"vsync":false,"hyper_port":0,"gpu":"auto","local_keys":false,"printer":false,"drives":false,"teams":false,"insecure":false},
            "entries": {
                "11111111-1111-1111-1111-111111111111": {"options": {"transport":"hyper","profile":"office","best_text":true,"codec":"hevc","fps":120,
                    "fullscreen":false,"multimon":true,"low_latency":true,"vsync":true,"hyper_port":48000,"gpu":"display","local_keys":true,
                    "printer":true,"drives":true,"teams":true,"insecure":true}, "hyper_password": null, "folder": "Work/Servers"},
                "22222222-2222-2222-2222-222222222222": {"options": {"transport":"rdp","profile":"game","low_latency":true,"vsync":true}, "folder": ""}
            },
            "recent": {"pc.lan:3389|bob": {"transport":"direct","low_latency":true,"vsync":true}},
            "updates": {"check": false, "from_hosts": true, "channel": "nightly"}
        }"#;
        let file: LauncherFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.theme, "dark");
        assert!(file.session_bar, "the bar stays on for a file that never had the setting");
        assert_eq!(file.defaults, Options::default());
        assert_eq!(file.updates, UpdatePrefs { check: false, from_hosts: true, channel: "nightly".into() });
        let hyper = &file.entries[&"11111111-1111-1111-1111-111111111111".parse().unwrap()];
        assert_eq!(hyper.folder, "Work/Servers");
        assert_eq!(
            hyper.options,
            Options {
                transport: Transport::Hyper,
                profile: Profile::Office,
                best_text: true,
                codec: "hevc".into(),
                fps: 120,
                scale: 0,
                fullscreen: false,
                multimon: true,
                low_latency: true,
                vsync: true,
                hyper_port: 48000,
                gpu: "display".into(),
                local_keys: true,
                printer: true,
                drives: true,
                teams: true,
                insecure: true,
            }
        );
        assert_eq!(
            hyper.options.to_args(),
            [
                "--hyper", "--office", "--hyper-text", "best", "--hyper-codec", "hevc", "--hyper-fps", "120", "--hyper-vsync", "--hyper-port",
                "48000", "--hyper-local-keys", "--low-latency", "--printer", "--drive", "all", "--insecure"
            ]
        );
        // Both presentation switches on, as that launcher allowed: each still
        // reaches the part of the session it belongs to.
        let rdp = &file.entries[&"22222222-2222-2222-2222-222222222222".parse().unwrap()].options;
        assert!(rdp.low_latency && rdp.vsync);
        assert_eq!(rdp.to_args(), ["--gaming", "--fullscreen", "--low-latency"]);
        let direct = &file.recent["pc.lan:3389|bob"];
        assert!(direct.low_latency && direct.vsync);
        assert_eq!(direct.to_args(), ["--direct", "--hyper-vsync", "--fullscreen"]);
        // What this launcher writes, that one reads (it skips the new field).
        let back: LauncherFile = serde_json::from_str(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn a_pinned_stream_size_goes_to_hyper_only() {
        let o = |transport, scale| Options { transport, scale, fullscreen: false, ..Options::default() };
        assert_eq!(o(Transport::Direct, 50).to_args(), ["--direct", "--hyper-scale", "50"]);
        assert_eq!(o(Transport::Hyper, 33).to_args(), ["--hyper", "--hyper-scale", "33"]);
        // Automatic, and anything the session wouldn't take: no flag.
        assert_eq!(o(Transport::Hyper, 0).to_args(), ["--hyper"]);
        assert_eq!(o(Transport::Hyper, 60).to_args(), ["--hyper"]);
        assert_eq!(o(Transport::Rdp, 50).to_args(), ["--gaming"]);
    }

    /// `--low-latency` is RDP's: Hyper Direct has no RDP part to use it.
    #[test]
    fn low_latency_is_for_the_rdp_part_of_a_session() {
        let o = |transport| Options { transport, low_latency: true, fullscreen: false, ..Options::default() };
        assert_eq!(o(Transport::Rdp).to_args(), ["--gaming", "--low-latency"]);
        assert_eq!(o(Transport::Hyper).to_args(), ["--hyper", "--low-latency"]);
        assert_eq!(o(Transport::Direct).to_args(), ["--direct"]);
    }

    #[test]
    fn a_cloud_pc_is_fast_rdp_with_every_redirect_whatever_the_defaults() {
        let defaults = Options {
            transport: Transport::Direct,
            profile: Profile::Office,
            fps: 30,
            insecure: true,
            fullscreen: false,
            multimon: true,
            ..Options::default()
        };
        assert_eq!(
            Options::cloud_pc(&defaults).to_args(),
            ["--gaming", "--multimon", "--low-latency", "--drive", "all"]
        );
    }

    #[test]
    fn options_make_the_session_command_line() {
        let direct = Options {
            transport: Transport::Direct,
            profile: Profile::Office,
            best_text: true,
            codec: "hevc".into(),
            fps: 120,
            fullscreen: true,
            printer: true, // (not for direct mode)
            gpu: "display".into(),
            local_keys: true,
            ..Options::default()
        };
        assert_eq!(
            direct.to_args(),
            [
                "--direct", "--office", "--hyper-text", "best", "--hyper-codec", "hevc", "--hyper-fps", "120", "--hyper-gpu", "display",
                "--hyper-local-keys", "--fullscreen"
            ]
        );
        let rdp = Options {
            transport: Transport::Rdp,
            profile: Profile::Game,
            fullscreen: false,
            multimon: true,
            drives: true,
            best_text: true, // (Hyper only)
            ..Options::default()
        };
        assert_eq!(rdp.to_args(), ["--gaming", "--multimon", "--drive", "all"]);
        // Junk codec names never reach the command line.
        let odd = Options { codec: "--evil".into(), gpu: "--evil".into(), fullscreen: false, ..Options::default() };
        assert_eq!(odd.to_args(), ["--hyper"]);
    }

    #[test]
    fn recent_keys_ignore_case_and_spaces() {
        assert_eq!(recent_key(" Host.LAN ", 3389, Some("Bob")), recent_key("host.lan", 3389, Some("bob ")));
        assert_ne!(recent_key("host", 3389, None), recent_key("host", 3390, None));
    }
}
