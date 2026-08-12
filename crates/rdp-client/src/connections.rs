//! Persistence for saved and recently-used RDP connections.
//!
//! Profiles are stored as a JSON array in the user data directory
//! (`ProjectDirs::from("com", "IOServicesLabs", "RDPiO")` →
//! `connections.json`), falling back to `$TMPDIR/rdpio-connections.json` when
//! no user data directory can be determined. Writes are atomic (temp file +
//! rename) and, on Unix, the file is created with mode `0o600` so plaintext
//! passwords never leak to other users. A corrupt store is quarantined to
//! `<file>.corrupt` and the app starts with an empty store instead of failing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The default RDP server port.
const DEFAULT_PORT: u16 = 3389;

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// A single RDP connection endpoint: either a saved profile (from the "Saved
/// connections" list) or a recently-used one (from the connection history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    /// Display name. For saved connections this is the unique key.
    pub name: String,
    /// Host name or IP address.
    pub host: String,
    /// Server port, defaults to 3389.
    #[serde(default = "default_port")]
    pub port: u16,
    /// User name for the logon.
    pub username: String,
    /// Optional password (kept in the local store for convenience).
    pub password: Option<String>,
    /// Accept self-signed / untrusted TLS certificates.
    #[serde(default)]
    pub insecure: bool,
    /// Whether this profile appears in the "Saved connections" list.
    #[serde(default)]
    pub saved: bool,
    /// Unix timestamp (seconds) of the last successful connection attempt.
    #[serde(default)]
    pub last_connected_at: Option<u64>,
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: DEFAULT_PORT,
            username: String::new(),
            password: None,
            insecure: false,
            saved: false,
            last_connected_at: None,
        }
    }
}

impl ConnectionProfile {
    /// Build a profile from the direct CLI connection flags (`--host`,
    /// `--user`, `--password`, `--insecure`), preserving the defaults the
    /// command line has always used: host is required (returns `None` without
    /// it), the user defaults to an empty logon name, the password to `None`,
    /// and TLS certificate validation stays on unless `--insecure` is given.
    /// The port defaults to 3389 and the display name mirrors the host.
    pub fn from_cli(
        host: Option<String>,
        user: Option<String>,
        password: Option<String>,
        insecure: bool,
    ) -> Option<Self> {
        let host = host?;
        Some(Self {
            name: host.clone(),
            host,
            port: DEFAULT_PORT,
            username: user.unwrap_or_default(),
            password,
            insecure,
            saved: false,
            last_connected_at: None,
        })
    }
}

/// JSON-backed store of [`ConnectionProfile`]s.
///
/// One store owns one file. `load()` opens the well-known location for the
/// current user; `load_from(path)` is available for tests and embedders that
/// want to point the store at a specific file.
#[derive(Debug, Clone)]
#[cfg_attr(not(windows), allow(dead_code))] // persisted-connection store used by the Windows saved-connection UI
pub struct ConnectionStore {
    path: PathBuf,
    profiles: Vec<ConnectionProfile>,
}

#[cfg_attr(not(windows), allow(dead_code))] // store methods used by the Windows saved-connection UI
impl ConnectionStore {
    /// Load the store from the user data directory (or the temp-dir fallback).
    pub fn load() -> Self {
        Self::load_from(default_store_path())
    }

    /// Load the store from a specific file, creating an empty store when the
    /// file is missing or corrupt (a corrupt file is renamed to `<file>.corrupt`).
    pub fn load_from(path: PathBuf) -> Self {
        let profiles = match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Vec<ConnectionProfile>>(&contents) {
                Ok(profiles) => profiles,
                Err(_) => {
                    // Corrupt JSON: quarantine it and start with an empty store.
                    let _ = fs::rename(&path, corrupt_path(&path));
                    Vec::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(_) => Vec::new(),
        };
        Self { path, profiles }
    }

    /// Atomically persist the store to disk (temp file + rename; `0o600` on Unix).
    pub fn save(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.profiles)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        // Write to a temp file in the same directory, then rename over the
        // target so a crash mid-write never leaves a truncated store behind.
        let mut tmp_name = self.path.as_os_str().to_owned();
        tmp_name.push(format!(".tmp{}", std::process::id()));
        let tmp = PathBuf::from(tmp_name);
        fs::write(&tmp, json.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &self.path)
    }

    /// Insert or overwrite a saved profile, keyed by `name`. The existing
    /// `last_connected_at` is preserved when the incoming profile has none.
    pub fn upsert_saved(&mut self, profile: ConnectionProfile) {
        let incoming_last = profile.last_connected_at;
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|p| p.saved && p.name == profile.name)
        {
            existing.host = profile.host;
            existing.port = profile.port;
            existing.username = profile.username;
            existing.password = profile.password;
            existing.insecure = profile.insecure;
            existing.saved = true;
            if existing.last_connected_at.is_none() {
                existing.last_connected_at = incoming_last;
            }
        } else {
            let mut profile = profile;
            profile.saved = true;
            self.profiles.push(profile);
        }
    }

    /// Record that a connection was made. Entries are deduplicated by
    /// `host:port:username`: a matching entry has its timestamp refreshed and
    /// its credentials updated, while its `saved` flag is preserved. New
    /// entries are appended with the current Unix timestamp.
    pub fn record_connection(&mut self, profile: ConnectionProfile) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(existing) = self.profiles.iter_mut().find(|p| {
            p.host == profile.host && p.port == profile.port && p.username == profile.username
        }) {
            existing.password = profile.password;
            existing.insecure = profile.insecure;
            existing.last_connected_at = Some(now);
        } else {
            let mut profile = profile;
            profile.last_connected_at = Some(now);
            self.profiles.push(profile);
        }
    }

    /// Remove the saved profile with the given name. Entries that were not
    /// saved (e.g. recent-history entries with the same display name) are kept.
    pub fn delete_connection(&mut self, name: &str) {
        self.profiles.retain(|p| !(p.saved && p.name == name));
    }

    /// All saved profiles, sorted by name.
    pub fn saved_connections(&self) -> Vec<ConnectionProfile> {
        let mut saved: Vec<ConnectionProfile> =
            self.profiles.iter().filter(|p| p.saved).cloned().collect();
        saved.sort_by(|a, b| a.name.cmp(&b.name));
        saved
    }

    /// Profiles that have been connected to at least once, newest first,
    /// limited to `limit` entries.
    pub fn recent_connections(&self, limit: usize) -> Vec<ConnectionProfile> {
        let mut recent: Vec<(u64, ConnectionProfile)> = self
            .profiles
            .iter()
            .filter_map(|p| p.last_connected_at.map(|ts| (ts, p.clone())))
            .collect();
        recent.sort_by_key(|b| std::cmp::Reverse(b.0));
        recent.into_iter().take(limit).map(|(_, p)| p).collect()
    }
}

/// The well-known store location: the user data dir for
/// `com.IOServicesLabs.RDPiO`, else the temp-dir fallback.
#[cfg_attr(not(windows), allow(dead_code))] // store persistence path used by the Windows saved-connection UI
fn default_store_path() -> PathBuf {
    match directories::ProjectDirs::from("com", "IOServicesLabs", "RDPiO") {
        Some(dirs) => dirs.data_dir().join("connections.json"),
        None => std::env::temp_dir().join("rdpio-connections.json"),
    }
}

/// `<file>` → `<file>.corrupt`, used to quarantine unreadable stores.
#[cfg_attr(not(windows), allow(dead_code))] // store recovery helper used by the Windows saved-connection UI
fn corrupt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".corrupt");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    /// Unique temp directory per test — never touches the real user config dir.
    fn unique_dir() -> PathBuf {
        let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "rdpio-connections-test-{}-{}",
            std::process::id(),
            n
        ))
    }

    fn profile(name: &str, host: &str, username: &str) -> ConnectionProfile {
        ConnectionProfile {
            name: name.to_string(),
            host: host.to_string(),
            port: DEFAULT_PORT,
            username: username.to_string(),
            password: None,
            insecure: false,
            saved: false,
            last_connected_at: None,
        }
    }

    #[test]
    fn save_then_load_returns_same_profiles() {
        let dir = unique_dir();
        let path = dir.join("connections.json");
        let mut store = ConnectionStore::load_from(path.clone());
        let mut p = profile("work", "10.0.0.5", "alice");
        p.password = Some("s3cret".to_string());
        p.insecure = true;
        p.saved = true;
        p.last_connected_at = Some(123);
        store.upsert_saved(p);
        store.save().unwrap();

        let loaded = ConnectionStore::load_from(path);
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].name, "work");
        assert_eq!(loaded.profiles[0].host, "10.0.0.5");
        assert_eq!(loaded.profiles[0].port, DEFAULT_PORT);
        assert_eq!(loaded.profiles[0].username, "alice");
        assert_eq!(loaded.profiles[0].password.as_deref(), Some("s3cret"));
        assert!(loaded.profiles[0].insecure);
        assert!(loaded.profiles[0].saved);
        assert_eq!(loaded.profiles[0].last_connected_at, Some(123));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_saved_overwrites_and_preserves_last_connected_at() {
        let mut store = ConnectionStore::load_from(unique_dir().join("connections.json"));
        let mut first = profile("work", "10.0.0.5", "alice");
        first.saved = true;
        first.last_connected_at = Some(1000);
        store.upsert_saved(first);

        let second = profile("work", "10.0.0.9", "bob");
        store.upsert_saved(second);

        let saved = store.saved_connections();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "work");
        assert_eq!(saved[0].host, "10.0.0.9");
        assert_eq!(saved[0].username, "bob");
        // The incoming profile had no timestamp; the existing one is preserved.
        assert_eq!(saved[0].last_connected_at, Some(1000));
        assert!(saved[0].saved);
    }

    #[test]
    fn record_connection_updates_timestamp_and_deduplicates() {
        let mut store = ConnectionStore::load_from(unique_dir().join("connections.json"));
        let mut first = profile("work", "10.0.0.5", "alice");
        first.saved = true;
        store.record_connection(first);
        let first_ts = store.profiles[0].last_connected_at.expect("stamped");

        // Sleep across a whole second boundary is not required (>= is enough for
        // "updated"), but sleeping keeps the test honest on coarse clocks.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Same host:port:username with a different display name and saved flag.
        let again = profile("Work (renamed)", "10.0.0.5", "alice");
        store.record_connection(again);

        assert_eq!(
            store.profiles.len(),
            1,
            "deduplicated by host:port:username"
        );
        let p = &store.profiles[0];
        assert!(p.saved, "saved flag is preserved across record_connection");
        assert_eq!(p.name, "work", "existing entry keeps its identity");
        assert!(
            p.last_connected_at.unwrap() > first_ts,
            "timestamp was refreshed"
        );
    }

    #[test]
    fn delete_connection_removes_only_the_named_profile() {
        let mut store = ConnectionStore::load_from(unique_dir().join("connections.json"));
        let mut a = profile("a", "h1", "u1");
        a.saved = true;
        let mut b = profile("b", "h2", "u2");
        b.saved = true;
        store.upsert_saved(a);
        store.upsert_saved(b);
        // An unsaved recent entry sharing the name "a" must survive deletion.
        store.record_connection(profile("a", "h9", "u9"));

        store.delete_connection("a");
        let saved = store.saved_connections();
        let names: Vec<&str> = saved.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["b"]);
        assert_eq!(store.profiles.len(), 2, "unsaved 'a' entry is untouched");
    }

    #[test]
    fn recent_connections_sorts_descending_and_omits_never_connected() {
        let mut store = ConnectionStore::load_from(unique_dir().join("connections.json"));
        store.profiles.push(ConnectionProfile {
            name: "old".into(),
            host: "h1".into(),
            port: DEFAULT_PORT,
            username: "u".into(),
            password: None,
            insecure: false,
            saved: true,
            last_connected_at: Some(100),
        });
        store.profiles.push(ConnectionProfile {
            name: "newest".into(),
            host: "h2".into(),
            port: DEFAULT_PORT,
            username: "u".into(),
            password: None,
            insecure: false,
            saved: true,
            last_connected_at: Some(300),
        });
        store.profiles.push(ConnectionProfile {
            name: "mid".into(),
            host: "h3".into(),
            port: DEFAULT_PORT,
            username: "u".into(),
            password: None,
            insecure: false,
            saved: true,
            last_connected_at: Some(200),
        });
        store.profiles.push(ConnectionProfile {
            name: "never".into(),
            host: "h4".into(),
            port: DEFAULT_PORT,
            username: "u".into(),
            password: None,
            insecure: false,
            saved: true,
            last_connected_at: None,
        });

        let recent = store.recent_connections(10);
        let names: Vec<&str> = recent.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["newest", "mid", "old"]);

        let limited = store.recent_connections(2);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].name, "newest");
        assert_eq!(limited[1].name, "mid");
    }

    #[test]
    fn corrupt_file_is_quarantined_and_store_starts_empty() {
        let dir = unique_dir();
        let path = dir.join("connections.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ definitely not json !!!").unwrap();

        let store = ConnectionStore::load_from(path.clone());
        assert!(store.profiles.is_empty());
        assert!(corrupt_path(&path).exists(), "corrupt file was renamed");
        assert!(!path.exists(), "original file was moved away");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_requires_host() {
        assert!(ConnectionProfile::from_cli(None, None, None, false).is_none());
        assert!(
            ConnectionProfile::from_cli(None, Some("u".into()), Some("p".into()), true).is_none()
        );
    }

    #[test]
    fn from_cli_applies_default_user_password_and_port() {
        let p = ConnectionProfile::from_cli(Some("10.0.0.5".into()), None, None, false)
            .expect("host given");
        assert_eq!(p.host, "10.0.0.5");
        assert_eq!(p.name, "10.0.0.5", "display name mirrors the host");
        assert_eq!(p.port, DEFAULT_PORT);
        assert_eq!(p.username, "", "user defaults to an empty logon name");
        assert_eq!(p.password, None, "password defaults to None");
        assert!(!p.insecure, "certificate validation stays on by default");
        assert!(!p.saved);
        assert_eq!(p.last_connected_at, None);
    }

    #[test]
    fn from_cli_carries_user_password_and_insecure() {
        let p = ConnectionProfile::from_cli(
            Some("server.corp".into()),
            Some("alice".into()),
            Some("s3cret".into()),
            true,
        )
        .expect("host given");
        assert_eq!(p.host, "server.corp");
        assert_eq!(p.username, "alice");
        assert_eq!(p.password.as_deref(), Some("s3cret"));
        assert!(p.insecure, "--insecure is recorded on the profile");
    }
}
