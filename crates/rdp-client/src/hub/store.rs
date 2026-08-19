//! Persistent JSON stores for the hub: saved connections and MRU history.
//!
//! Both stores live in the same `%LOCALAPPDATA%\rdpio\` directory the existing
//! `token_cache.rs` / `password_cache.rs` caches use (see the step-1 integration
//! map in `hub::mod.rs` §6) — this reuses that convention instead of inventing
//! a new location:
//!
//! ```text
//! std::env::var("LOCALAPPDATA")? → <local>\rdpio → create_dir_all
//! ```
//!
//! - `connections.json` — [`ConnectionStore`], keyed by [`ConnectionRecord::id`].
//! - `history.json` — [`MruStore`], newest-first, capped at [`MRU_CAP`].
//!
//! Writes are atomic: the JSON is serialized to a sibling `*.tmp` file, flushed
//! to disk, then renamed over the target, so a crash mid-write can never
//! truncate a good store. A corrupt file on load is logged with `tracing`,
//! backed up as `*.corrupt-<unix>`, and the store starts empty so the hub never
//! bricks on a bad file. Passwords are protected via
//! [`ProtectedPassword::protect`] before anything reaches disk — the JSON never
//! contains a plaintext password.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::hub::model::{now_unix, ConnectionRecord, MruRecord, ProtectedPassword};
use crate::hub::HubError;

const CONNECTIONS_FILE: &str = "connections.json";
const HISTORY_FILE: &str = "history.json";
/// Maximum number of MRU entries kept; the oldest are dropped beyond this.
const MRU_CAP: usize = 50;

/// `%LOCALAPPDATA%\rdpio` — the same location as the W365 caches. The directory
/// is created if missing (idempotent, mirroring `password_cache.rs`).
fn data_dir() -> Result<PathBuf, HubError> {
    let local = std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HubError::invalid("LOCALAPPDATA is not set"))?;
    let dir = PathBuf::from(local).join("rdpio");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Write `contents` to `path` atomically: write a sibling `*.tmp` file, flush
/// and sync it, then rename over the target. `std::fs::rename` replaces an
/// existing destination on Windows (MoveFileExW with MOVEFILE_REPLACE_EXISTING),
/// so a crash between the write and the rename leaves the previous file intact.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), HubError> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a JSON array of records from `path`. A missing file is an empty store;
/// unreadable or unparseable content is logged, backed up as
/// `*.corrupt-<unix>`, and recovered as an empty store.
fn load_records<T>(path: &Path, what: &str) -> Vec<T>
where
    T: serde::de::DeserializeOwned,
{
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, what, "could not read store file; starting empty");
            Vec::new()
        }
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, what, "corrupt store file; backing it up and starting empty");
                backup_corrupt(path);
                Vec::new()
            }
        },
    }
}

/// Move a corrupt store file aside so a later inspection keeps the evidence
/// without letting it break the next load. Best-effort: failures are logged.
fn backup_corrupt(path: &Path) {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("store");
    let backup = path.with_file_name(format!("{file_name}.corrupt-{}", now_unix()));
    if let Err(e) = fs::rename(path, &backup) {
        tracing::warn!(error = %e, path = %path.display(), "could not back up corrupt store file");
    }
}

/// Everything the form provides when saving a connection. `password` is the
/// plaintext typed into the form — it exists only in memory and is protected
/// via [`ProtectedPassword::protect`] inside [`ConnectionStore::upsert`] before
/// anything is written to disk. `None` (or an empty string) means "keep the
/// existing DPAPI blob" when updating an existing record, or "no password" for
/// a brand-new record.
#[derive(Debug, Clone, Default)]
pub struct ConnectionInput {
    /// `None` → a new record gets a fresh v4 id; `Some(id)` → update in place.
    pub id: Option<uuid::Uuid>,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Plaintext password, in memory only.
    pub password: Option<String>,
}

/// Saved-connections store, backed by `%LOCALAPPDATA%\rdpio\connections.json`.
///
/// Every mutation (`upsert`/`delete`) persists immediately, so the hub never
/// needs a separate "save" step and a crash can only lose the in-flight edit,
/// never previously saved records.
#[derive(Debug)]
pub struct ConnectionStore {
    path: PathBuf,
    records: Vec<ConnectionRecord>,
}

impl ConnectionStore {
    /// Load from the default location (`%LOCALAPPDATA%\rdpio\connections.json`),
    /// creating the directory if missing.
    pub fn load() -> Result<Self, HubError> {
        Self::load_from(data_dir()?.join(CONNECTIONS_FILE))
    }

    /// Load from an explicit file path — used by the hub UI tests and any
    /// embedding that wants its own data location. The parent directory is
    /// created if missing.
    pub fn load_from(path: PathBuf) -> Result<Self, HubError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let records = load_records(&path, "connections");
        Ok(Self { path, records })
    }

    /// All saved records, in insertion order.
    pub fn list(&self) -> &[ConnectionRecord] {
        &self.records
    }

    /// The record with `id`, if any.
    pub fn get(&self, id: &uuid::Uuid) -> Option<&ConnectionRecord> {
        self.records.iter().find(|r| &r.id == id)
    }

    /// Insert or update a connection and persist immediately.
    ///
    /// Validation: `host` must be non-empty and `port` in 1..=65535 (the UI
    /// form enforces the same rules up front; this is a defensive second line).
    ///
    /// Password handling: a non-empty `password` is DPAPI-protected and stored;
    /// a blank/`None` password preserves the existing blob when updating (so an
    /// edit that leaves the password field empty does not clobber the saved
    /// credential), and stores a protected empty password for brand-new records.
    pub fn upsert(&mut self, input: ConnectionInput) -> Result<ConnectionRecord, HubError> {
        if input.host.trim().is_empty() {
            return Err(HubError::invalid("host must not be empty"));
        }
        if input.port == 0 {
            return Err(HubError::invalid("port must be 1-65535"));
        }

        let now = now_unix();
        let existing = input
            .id
            .and_then(|id| self.records.iter().position(|r| r.id == id));

        let (id, created_at, saved_password) = match existing {
            Some(idx) => {
                let rec = &self.records[idx];
                let saved_password = match input.password.as_deref() {
                    Some(p) if !p.is_empty() => ProtectedPassword::protect(p)?,
                    _ => rec.saved_password.clone(),
                };
                (rec.id, rec.created_at, saved_password)
            }
            None => {
                let saved_password = match input.password.as_deref() {
                    Some(p) if !p.is_empty() => ProtectedPassword::protect(p)?,
                    _ => ProtectedPassword::protect("")?,
                };
                (uuid::Uuid::new_v4(), now, saved_password)
            }
        };

        let record = ConnectionRecord {
            id,
            display_name: input.display_name,
            host: input.host,
            port: input.port,
            username: input.username,
            domain: input.domain,
            saved_password,
            created_at,
            updated_at: now,
        };

        match existing {
            Some(idx) => self.records[idx] = record.clone(),
            None => self.records.push(record.clone()),
        }
        self.save()?;
        Ok(record)
    }

    /// Remove the record with `id` and persist. Returns `false` (and writes
    /// nothing) when no such record exists.
    pub fn delete(&mut self, id: &uuid::Uuid) -> Result<bool, HubError> {
        let before = self.records.len();
        self.records.retain(|r| &r.id != id);
        if self.records.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), HubError> {
        let json = serde_json::to_vec_pretty(&self.records)?;
        atomic_write(&self.path, &json)?;
        Ok(())
    }
}

/// Most-recently-used history, backed by `%LOCALAPPDATA%\rdpio\history.json`.
///
/// Invariant: `records` is kept sorted by `last_connected_at` descending
/// (newest first) and never exceeds [`MRU_CAP`]. Entries carry no password — an
/// optional `connection_id` links back to a saved record when one exists,
/// otherwise the `prompt.rs` flow asks at connect time.
#[derive(Debug)]
pub struct MruStore {
    path: PathBuf,
    records: Vec<MruRecord>,
}

impl MruStore {
    /// Load from the default location (`%LOCALAPPDATA%\rdpio\history.json`),
    /// creating the directory if missing.
    pub fn load() -> Result<Self, HubError> {
        Self::load_from(data_dir()?.join(HISTORY_FILE))
    }

    /// Load from an explicit file path (tests / embedding). The parent
    /// directory is created if missing; records are re-sorted newest-first so
    /// the invariant holds even for a hand-edited file.
    pub fn load_from(path: PathBuf) -> Result<Self, HubError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut records: Vec<MruRecord> = load_records(&path, "history");
        records.sort_by(|a, b| b.last_connected_at.cmp(&a.last_connected_at));
        Ok(Self { path, records })
    }

    /// All history entries, newest first (the maintained sort order).
    pub fn list_most_recent_first(&self) -> &[MruRecord] {
        &self.records
    }

    /// The most recently used non-empty username, or `None` when history has
    /// no usable username.
    ///
    /// Walks the MRU list from the front (most recent) to the back and returns
    /// the first username that is present and not blank. This is the New
    /// Connection form's preferred username prefill: the most recent person
    /// this user connected as. Read-only; never mutates the MRU list, so
    /// ordering, cap, dedupe, and move-to-front behavior are untouched.
    pub fn last_used_username(&self) -> Option<&str> {
        self.records
            .iter()
            .filter_map(|r| r.username.as_deref())
            .map(str::trim)
            .find(|u| !u.is_empty())
    }

    /// Record one successful connection and persist immediately.
    ///
    /// Dedupe key is the full target identity `(host, port, username, domain)`:
    /// reconnecting to the same target bumps `connect_count`, refreshes
    /// `last_connected_at`, and moves the entry to the top; a new target is
    /// appended with count 1. `display_name` is refreshed from the latest
    /// connection, and a `Some(connection_id)` (connection came from a Saved
    /// entry) is remembered — a later `None` never clears it.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        host: impl Into<String>,
        port: u16,
        username: Option<String>,
        domain: Option<String>,
        display_name: impl Into<String>,
        connection_id: Option<uuid::Uuid>,
    ) -> Result<(), HubError> {
        let host = host.into();
        let display_name = display_name.into();

        let position = self.records.iter().position(|r| {
            r.host == host && r.port == port && r.username == username && r.domain == domain
        });

        let moved_index = match position {
            Some(idx) => {
                let existing = &mut self.records[idx];
                existing.display_name = display_name;
                if connection_id.is_some() {
                    existing.connection_id = connection_id;
                }
                existing.touch();
                idx
            }
            None => {
                self.records.push(MruRecord {
                    host,
                    port,
                    username,
                    domain,
                    display_name,
                    last_connected_at: now_unix(),
                    connect_count: 1,
                    connection_id,
                });
                self.records.len() - 1
            }
        };

        // The just-recorded target is the most recent by definition. Move it to
        // the front explicitly: `last_connected_at` is second-resolution, so
        // several connections within one second would otherwise leave the order
        // ambiguous under a timestamp-only sort.
        let entry = self.records.remove(moved_index);
        self.records.insert(0, entry);

        // Re-sort as a defence against clock skew (backwards jumps) and
        // hand-edited files; ties keep the move-to-front order because the sort
        // is stable.
        self.records
            .sort_by(|a, b| b.last_connected_at.cmp(&a.last_connected_at));
        self.cap(MRU_CAP);
        self.save()
    }

    /// Keep at most `max` entries, dropping the oldest (the tail, which is the
    /// least recently connected given the newest-first invariant).
    pub fn cap(&mut self, max: usize) {
        if self.records.len() > max {
            let dropped = self.records.len() - max;
            self.records.truncate(max);
            tracing::debug!(dropped, "trimmed MRU history to cap");
        }
    }

    fn save(&self) -> Result<(), HubError> {
        let json = serde_json::to_vec_pretty(&self.records)?;
        atomic_write(&self.path, &json)?;
        Ok(())
    }
}

/// Render the identity portion of a Recent entry: `user@host` when the user
/// is present and non-empty, otherwise just `host`.
///
/// Pure and side-effect free so the Recent-list formatting (step-12) can be
/// unit-tested without a window. A blank username renders as bare `host`,
/// matching the CLI convention (`main.rs` shows `user@host` only when a user
/// is known). The username is trimmed so a whitespace-only value degrades to
/// the bare-host form instead of producing a dangling `@`.
pub fn format_recent_identity(user: Option<&str>, host: &str) -> String {
    match user.map(str::trim).filter(|u| !u.is_empty()) {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique file path inside a fresh temp directory, so parallel tests
    /// never collide. The caller is responsible for `remove_dir_all(parent)`.
    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rdpio-store-test-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(format!("{name}.json"))
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    fn input(display_name: &str, host: &str, port: u16, password: Option<&str>) -> ConnectionInput {
        ConnectionInput {
            id: None,
            display_name: display_name.into(),
            host: host.into(),
            port,
            username: Some("alice".into()),
            domain: Some("CORP".into()),
            password: password.map(String::from),
        }
    }

    #[test]
    fn connections_survive_reload_and_delete() {
        let path = temp_file("conns");
        {
            let mut store = ConnectionStore::load_from(path.clone()).unwrap();
            let rec = store
                .upsert(input("Work", "work.corp", 3389, Some("S3cret!")))
                .unwrap();
            assert_eq!(store.list().len(), 1);
            assert_eq!(store.get(&rec.id).unwrap().host, "work.corp");
            // Atomic write leaves no .tmp behind.
            assert!(!path.with_extension("tmp").exists(), "stale .tmp file");
        }

        // Reload in a fresh store: the record (and its recoverable password)
        // survived.
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        assert_eq!(store.list().len(), 1);
        assert_eq!(
            store.list()[0].saved_password.unprotect().unwrap(),
            "S3cret!"
        );

        // Delete, then confirm the deletion also survives a reload.
        let id = store.list()[0].id;
        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap(), "double delete is a no-op");
        let reloaded = ConnectionStore::load_from(path.clone()).unwrap();
        assert!(reloaded.list().is_empty());
        cleanup(&path);
    }

    #[test]
    fn upsert_blank_password_preserves_existing_blob() {
        let path = temp_file("blankpw");
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();

        let rec = store
            .upsert(input("Box", "box", 3390, Some("original-pw")))
            .unwrap();
        let blob_before = rec.saved_password.as_base64().to_string();

        // Edit with a blank password field → the existing DPAPI blob is kept.
        let updated = store
            .upsert(ConnectionInput {
                id: Some(rec.id),
                display_name: "Box v2".into(),
                host: "box".into(),
                port: 3390,
                username: None,
                domain: None,
                password: None,
            })
            .unwrap();
        assert_eq!(updated.saved_password.as_base64(), blob_before);
        assert_eq!(updated.saved_password.unprotect().unwrap(), "original-pw");
        // updated_at moved, created_at did not.
        assert_eq!(updated.created_at, rec.created_at);
        assert!(updated.updated_at >= rec.updated_at);

        // A new non-empty value replaces the blob.
        let changed = store
            .upsert(ConnectionInput {
                id: Some(rec.id),
                display_name: "Box v3".into(),
                host: "box".into(),
                port: 3390,
                username: None,
                domain: None,
                password: Some("new-pw".into()),
            })
            .unwrap();
        assert_eq!(changed.saved_password.unprotect().unwrap(), "new-pw");
        assert_ne!(changed.saved_password.as_base64(), blob_before);
        cleanup(&path);
    }

    #[test]
    fn no_plaintext_on_disk() {
        let path = temp_file("plaintext");
        {
            let mut store = ConnectionStore::load_from(path.clone()).unwrap();
            store
                .upsert(input(
                    "Secret Box",
                    "secret.box",
                    3389,
                    Some("DiskSecret99"),
                ))
                .unwrap();
        }
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("DiskSecret99"),
            "connections.json leaked the plaintext: {on_disk}"
        );
        // The only credential field is `saved_password` (a DPAPI blob); its
        // value must decode as base64 and never be the plaintext itself.
        let doc: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        let blob = doc[0]["saved_password"].as_str().unwrap();
        assert_ne!(blob, "DiskSecret99");
        assert!(!blob.is_empty());
        cleanup(&path);
    }

    #[test]
    fn upsert_validates_host_and_port() {
        let path = temp_file("validate");
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        let err = store
            .upsert(ConnectionInput {
                host: "   ".into(),
                ..input("X", "ignored", 3389, None)
            })
            .unwrap_err();
        assert!(err.to_string().contains("host"));
        let err = store
            .upsert(ConnectionInput {
                port: 0,
                ..input("X", "ok", 3389, None)
            })
            .unwrap_err();
        assert!(err.to_string().contains("port"));
        cleanup(&path);
    }

    #[test]
    fn mru_dedupes_and_moves_to_front() {
        let path = temp_file("mru");
        let mut store = MruStore::load_from(path.clone()).unwrap();

        store
            .record("host-a", 3389, Some("u".into()), None, "A", None)
            .unwrap();
        store
            .record("host-a", 3389, Some("u".into()), None, "A", None)
            .unwrap();
        store
            .record("host-a", 3389, Some("u".into()), None, "A", None)
            .unwrap();
        assert_eq!(store.list_most_recent_first().len(), 1);
        assert_eq!(store.list_most_recent_first()[0].connect_count, 3);

        // A different target goes second; the repeated one is still oldest.
        store
            .record("host-b", 3389, Some("u".into()), None, "B", None)
            .unwrap();
        assert_eq!(store.list_most_recent_first().len(), 2);
        assert_eq!(store.list_most_recent_first()[0].host, "host-b");

        // Touching host-a again moves it back to the top and bumps the count.
        store
            .record("host-a", 3389, Some("u".into()), None, "A", None)
            .unwrap();
        let list = store.list_most_recent_first();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].host, "host-a");
        assert_eq!(list[0].connect_count, 4);

        // The same host with a different port is a distinct entry.
        store
            .record("host-a", 3390, Some("u".into()), None, "A-alt", None)
            .unwrap();
        assert_eq!(store.list_most_recent_first().len(), 3);

        // Reload keeps order and counts.
        let reloaded = MruStore::load_from(path.clone()).unwrap();
        let list = reloaded.list_most_recent_first();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].host, "host-a");
        assert_eq!(list[0].port, 3390);
        cleanup(&path);
    }

    #[test]
    fn mru_caps_at_fifty_dropping_oldest() {
        let path = temp_file("mrucap");
        let mut store = MruStore::load_from(path.clone()).unwrap();

        for i in 0..55 {
            store
                .record(
                    format!("host-{i:02}"),
                    3389,
                    None,
                    None,
                    format!("h{i:02}"),
                    None,
                )
                .unwrap();
        }
        let list = store.list_most_recent_first();
        assert_eq!(list.len(), 50, "history must be capped at 50");
        // Newest first: host-54 at the front; the five oldest were dropped.
        assert_eq!(list[0].host, "host-54");
        assert!(
            list.iter().all(|r| r.host != "host-00"),
            "oldest entries must be dropped"
        );

        // The cap is also enforced on reload (defensive against hand-edited files).
        let reloaded = MruStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.list_most_recent_first().len(), 50);
        cleanup(&path);
    }

    #[test]
    fn mru_keeps_connection_id_from_saved_entry() {
        let path = temp_file("mruid");
        let mut store = MruStore::load_from(path.clone()).unwrap();
        let saved_id = uuid::Uuid::new_v4();

        store
            .record(
                "saved-host",
                3389,
                Some("u".into()),
                None,
                "Saved",
                Some(saved_id),
            )
            .unwrap();
        // A later CLI connection to the same target has no id; the link to the
        // saved password must be preserved, not cleared.
        store
            .record("saved-host", 3389, Some("u".into()), None, "Saved", None)
            .unwrap();
        let entry = &store.list_most_recent_first()[0];
        assert_eq!(entry.connection_id, Some(saved_id));
        assert_eq!(entry.connect_count, 2);
        cleanup(&path);
    }

    #[test]
    fn corrupt_connections_file_is_backed_up_and_recovers_empty() {
        let path = temp_file("corrupt");
        fs::write(&path, b"{ this is not json").unwrap();

        let store = ConnectionStore::load_from(path.clone()).unwrap();
        assert!(store.list().is_empty(), "corrupt store must start empty");

        // The bad file was moved aside as *.corrupt-<unix>.
        assert!(!path.exists(), "corrupt file should have been moved aside");
        let parent = path.parent().unwrap();
        let backups: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1, "expected one corrupt backup");

        // And the store is fully writable afterwards.
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        store
            .upsert(input("Recovered", "rec.box", 3389, Some("pw")))
            .unwrap();
        let reloaded = ConnectionStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].saved_password.unprotect().unwrap(), "pw");
        cleanup(&path);
    }

    #[test]
    fn last_used_username_walks_mru_front_to_back() {
        let path = temp_file("lastuser");
        let mut store = MruStore::load_from(path.clone()).unwrap();
        assert_eq!(
            store.last_used_username(),
            None,
            "empty history has no username"
        );

        store
            .record("first", 3389, None, None, "First", None)
            .unwrap();
        assert_eq!(store.last_used_username(), None, "no entry has a username");

        store
            .record("second", 3389, Some("bob".into()), None, "Second", None)
            .unwrap();
        store
            .record("third", 3389, Some("carol".into()), None, "Third", None)
            .unwrap();
        // Newest first: carol is the most recent non-empty username.
        assert_eq!(store.last_used_username(), Some("carol"));

        // A blank username at the front is skipped in favour of the next one.
        store
            .record("blank", 3390, Some("".into()), None, "Blank", None)
            .unwrap();
        assert_eq!(store.last_used_username(), Some("carol"));

        // The helper is read-only: history is unchanged by calling it.
        assert_eq!(store.list_most_recent_first().len(), 4);
        cleanup(&path);
    }

    #[test]
    fn format_recent_identity_renders_user_at_host_or_bare_host() {
        assert_eq!(
            format_recent_identity(Some("alice"), "box.local"),
            "alice@box.local"
        );
        assert_eq!(
            format_recent_identity(Some("alice"), "10.0.0.5"),
            "alice@10.0.0.5"
        );
        // Empty or blank user → bare host, never a dangling '@'.
        assert_eq!(format_recent_identity(None, "box.local"), "box.local");
        assert_eq!(format_recent_identity(Some(""), "box.local"), "box.local");
        assert_eq!(format_recent_identity(Some("  "), "box.local"), "box.local");
    }
}
