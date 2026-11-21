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
use std::sync::atomic::{AtomicU64, Ordering};

use crate::hub::model::{now_unix, ConnectionRecord, MruRecord, ProtectedPassword};
use crate::hub::HubError;

const CONNECTIONS_FILE: &str = "connections.json";
const HISTORY_FILE: &str = "history.json";
/// `%LOCALAPPDATA%\rdpio\settings.json` — user preferences (see [`Settings`]).
const SETTINGS_FILE: &str = "settings.json";
/// Maximum number of MRU entries kept; the oldest are dropped beyond this.
const MRU_CAP: usize = 50;

/// `%LOCALAPPDATA%\rdpio` — the same location as the W365 caches. The directory
/// is created if missing (idempotent, mirroring `password_cache.rs`).
pub(crate) fn data_dir() -> Result<PathBuf, HubError> {
    let local = std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HubError::invalid("LOCALAPPDATA is not set"))?;
    let dir = PathBuf::from(local).join("rdpio");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Monotonic counter that makes the temp file name of [`atomic_write`] unique
/// within this process; the PID component makes it unique across processes.
/// Two interleaved writers — the hub and a detached child session — therefore
/// always target different temp files and can never observe each other's
/// half-written output.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A uniquely-named sibling temp path for `path`:
/// `<stem>.<pid>.<counter>.tmp` in the same directory as `path`. The name
/// carries the process id and a per-process counter, so concurrent writers
/// (hub + child session) never collide on one temp file.
fn temp_path_for(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("store");
    let pid = std::process::id();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!("{stem}.{pid}.{counter}.tmp"))
}

/// Create `tmp`, write `contents`, flush + sync it, then rename it over
/// `path`. Split out so [`atomic_write`] can clean the temp file up on error.
fn write_temp_then_rename(tmp: &Path, path: &Path, contents: &[u8]) -> Result<(), HubError> {
    let mut file = fs::File::create(tmp)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// Write `contents` to `path` atomically via write-temp-then-rename: serialize
/// to a uniquely-named sibling `*.tmp` file, flush and sync it, then
/// `std::fs::rename` it over the destination. `std::fs::rename` replaces an
/// existing destination on Windows (MoveFileExW with MOVEFILE_REPLACE_EXISTING),
/// so a crash between the write and the rename leaves the previous file intact.
/// On any error the temp file is deleted before the error is returned.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), HubError> {
    let tmp = temp_path_for(path);
    if let Err(e) = write_temp_then_rename(&tmp, path, contents) {
        // Best-effort cleanup: never leave a half-written temp file behind.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The single atomic JSON-write helper every `%LOCALAPPDATA%\rdpio\` store
/// persists through: serialize `value` as pretty JSON, then write it via
/// [`atomic_write`]'s write-temp-then-rename. All stores (`connections.json`,
/// `history.json`, `settings.json`) save through this one path, so interleaved
/// writers — the hub and a detached child session — can never corrupt a store
/// file.
pub fn write_json_atomic<T>(path: &Path, value: &T) -> Result<(), HubError>
where
    T: serde::Serialize,
{
    let json = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &json)
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

/// Read a JSON array of records from `path` for a change about to be written
/// back. A missing file is an empty store; anything else that goes wrong (the
/// file held open by another program, an I/O error, content that doesn't parse)
/// is an error, so the caller writes nothing rather than replacing every saved
/// record with an empty list. The file is left as it is.
fn try_load_records<T>(path: &Path) -> Result<Vec<T>, HubError>
where
    T: serde::de::DeserializeOwned,
{
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
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

    /// Read the file again for display: another process (a session's
    /// connection bar, a second rdpio) may have written it since this store was
    /// loaded. When it can't be read just now, the records already in memory
    /// stay.
    pub fn reload(&mut self) {
        match try_load_records(&self.path) {
            Ok(records) => self.records = records,
            Err(e) => tracing::warn!(path = %self.path.display(), error = %e, "could not re-read connections; keeping the ones loaded"),
        }
    }

    /// Read the file again before a change that will be written back. Fails,
    /// keeping the records in memory, when the file is there but can't be read
    /// or parsed, so the change never overwrites saved connections it didn't
    /// see.
    pub fn reload_strict(&mut self) -> Result<(), HubError> {
        self.records = try_load_records(&self.path)?;
        Ok(())
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
    ///
    /// Like every change here it starts from the file as it is now, so a
    /// record another process saved in the meantime is kept.
    pub fn upsert(&mut self, input: ConnectionInput) -> Result<ConnectionRecord, HubError> {
        if input.host.trim().is_empty() {
            return Err(HubError::invalid("host must not be empty"));
        }
        if input.port == 0 {
            return Err(HubError::invalid("port must be 1-65535"));
        }
        self.reload_strict()?;

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
        self.reload_strict()?;
        let before = self.records.len();
        self.records.retain(|r| &r.id != id);
        if self.records.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// The position of the record with `id` in [`list`](Self::list).
    pub fn position(&self, id: &uuid::Uuid) -> Option<usize> {
        self.records.iter().position(|r| &r.id == id)
    }

    /// Put back a record that [`delete`](Self::delete) removed (an undo), as it
    /// was, password blob and dates included, at `at` (or last), and persist.
    /// Returns `false` (and writes nothing) when a record with its id is there.
    pub fn restore(&mut self, record: ConnectionRecord, at: usize) -> Result<bool, HubError> {
        self.reload_strict()?;
        if self.records.iter().any(|r| r.id == record.id) {
            return Ok(false);
        }
        let at = at.min(self.records.len());
        self.records.insert(at, record);
        self.save()?;
        Ok(true)
    }

    /// Replace a record's saved password outright (an `upsert` keeps it when
    /// the new one is blank). Returns `false` when there's no such record.
    pub fn replace_password(&mut self, id: &uuid::Uuid, password: ProtectedPassword) -> Result<bool, HubError> {
        self.reload_strict()?;
        let Some(rec) = self.records.iter_mut().find(|r| &r.id == id) else {
            return Ok(false);
        };
        rec.saved_password = password;
        rec.updated_at = now_unix();
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), HubError> {
        write_json_atomic(&self.path, &self.records)
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

    /// Read the file again for display (sessions record their own
    /// connections), newest first. When it can't be read just now, the entries
    /// already in memory stay.
    pub fn reload(&mut self) {
        if let Err(e) = self.reload_strict() {
            tracing::warn!(path = %self.path.display(), error = %e, "could not re-read history; keeping the entries loaded");
        }
    }

    /// Read the file again before a change that will be written back, newest
    /// first. Fails, keeping the entries in memory, when the file is there but
    /// can't be read or parsed, so the change never overwrites history it
    /// didn't see.
    pub fn reload_strict(&mut self) -> Result<(), HubError> {
        let mut records: Vec<MruRecord> = try_load_records(&self.path)?;
        records.sort_by(|a, b| b.last_connected_at.cmp(&a.last_connected_at));
        self.records = records;
        Ok(())
    }

    /// All history entries, newest first (the maintained sort order).
    pub fn list_most_recent_first(&self) -> &[MruRecord] {
        &self.records
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
        // (From the file as it is now: other processes record too.)
        self.reload_strict()?;

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

    /// Forget every entry for `host:port` as `username` (any domain), and
    /// persist. Host and user compare without case, as Windows does.
    pub fn remove(&mut self, host: &str, port: u16, username: Option<&str>) -> Result<bool, HubError> {
        self.reload_strict()?;
        let before = self.records.len();
        let same = |a: Option<&str>, b: Option<&str>| a.unwrap_or("").trim().eq_ignore_ascii_case(b.unwrap_or("").trim());
        self.records
            .retain(|r| !(r.host.trim().eq_ignore_ascii_case(host.trim()) && r.port == port && same(r.username.as_deref(), username)));
        if self.records.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
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
        write_json_atomic(&self.path, &self.records)
    }
}

/// User preferences persisted under `%LOCALAPPDATA%\rdpio\settings.json`.
///
/// `Default` is the factory state: `keep_hub_open` starts **ON**, so a fresh
/// install keeps the hub window open after a connection is launched. Missing
/// or corrupt settings files fall back to [`Settings::default`] (see
/// [`load_settings`]), so the hub never bricks on a bad file and never panics
/// on a missing one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Keep the hub window open after a connection is launched, so the user
    /// can pick another target without re-opening the hub.
    pub keep_hub_open: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { keep_hub_open: true }
    }
}

/// Resolve `%LOCALAPPDATA%\rdpio\settings.json` — the same directory
/// resolution as `history.json` / `connections.json` (see [`data_dir`]). The
/// directory is created if missing.
pub fn settings_path() -> Result<PathBuf, HubError> {
    Ok(data_dir()?.join(SETTINGS_FILE))
}

/// Load settings from `path`, falling back to [`Settings::default()`] on a
/// missing, unreadable, or unparseable file — the hub must never panic on a
/// bad settings file, and a fresh install (no file yet) gets the ON default.
/// Errors are logged with `tracing` so a silently-ignored disk failure stays
/// diagnosable.
pub fn load_settings(path: &Path) -> Settings {
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not read settings file; using defaults"
            );
            Settings::default()
        }
        Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "corrupt settings file; using defaults"
                );
                Settings::default()
            }
        },
    }
}

/// Persist `settings` to `path` atomically via [`write_json_atomic`]
/// (write-temp-then-rename), so an interleaved writer or a crash mid-write can
/// never leave a truncated settings file behind.
pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), HubError> {
    write_json_atomic(path, settings)
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

    /// Names of any `*.tmp` siblings in `path`'s parent directory — a leak
    /// check for the atomic write-temp-then-rename helpers: after a successful
    /// write the uniquely-named temp file must have been renamed over the
    /// target, leaving no `*.tmp` behind.
    fn leftover_temp_files(path: &Path) -> Vec<String> {
        let Some(parent) = path.parent() else {
            return Vec::new();
        };
        fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
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
            // Atomic write leaves no temp file behind.
            assert!(
                leftover_temp_files(&path).is_empty(),
                "stale temp files after atomic write"
            );
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
    fn a_store_loaded_earlier_keeps_what_another_process_saved() {
        // The launcher loads its store once; a session's bar saves through a
        // store of its own. The launcher's next change must not erase it.
        let path = temp_file("two-writers");
        let mut launcher = ConnectionStore::load_from(path.clone()).unwrap();
        let mine = launcher.upsert(input("Mine", "mine.corp", 3389, None)).unwrap();
        let mut bar = ConnectionStore::load_from(path.clone()).unwrap();
        let theirs = bar.upsert(input("Theirs", "theirs.corp", 3389, None)).unwrap();
        launcher.upsert(ConnectionInput { id: Some(mine.id), ..input("Mine, renamed", "mine.corp", 3389, None) }).unwrap();
        let ids: Vec<_> = ConnectionStore::load_from(path.clone()).unwrap().list().iter().map(|r| r.id).collect();
        assert_eq!(ids, [mine.id, theirs.id]);
        // Deleting, restoring and replacing a password start from the file too.
        let other = bar.upsert(input("Other", "other.corp", 3389, None)).unwrap();
        assert!(launcher.delete(&mine.id).unwrap());
        assert!(launcher.replace_password(&other.id, ProtectedPassword::protect("pw").unwrap()).unwrap());
        let ids: Vec<_> = ConnectionStore::load_from(path.clone()).unwrap().list().iter().map(|r| r.id).collect();
        assert_eq!(ids, [theirs.id, other.id]);
        // A reload shows the others' records without changing anything.
        let mut fresh = ConnectionStore::load_from(path.clone()).unwrap();
        bar.upsert(input("Late", "late.corp", 3389, None)).unwrap();
        fresh.reload();
        assert_eq!(fresh.list().len(), 3);
        cleanup(&path);
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_written_over() {
        // Loaded fine at start; later the file can't be read back (here its
        // content doesn't parse). Changes refuse to write instead of saving a
        // list without the records they didn't see, and the file stays as it is.
        let path = temp_file("unreadable");
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        let kept = store.upsert(input("Kept", "kept.corp", 3389, None)).unwrap();
        let mut history = MruStore::load_from(path.with_extension("history")).unwrap();
        history.record("kept.corp", 3389, None, None, "Kept", None).unwrap();
        let garbage = b"{ not json";
        fs::write(&path, garbage).unwrap();
        fs::write(path.with_extension("history"), garbage).unwrap();

        assert!(store.upsert(input("New", "new.corp", 3389, None)).is_err());
        assert!(store.upsert(ConnectionInput { id: Some(kept.id), ..input("Renamed", "kept.corp", 3389, None) }).is_err());
        assert!(store.delete(&kept.id).is_err());
        assert!(store.restore(kept.clone(), 0).is_err());
        assert!(store.replace_password(&kept.id, ProtectedPassword::protect("pw").unwrap()).is_err());
        assert!(history.record("new.corp", 3389, None, None, "New", None).is_err());
        assert!(history.remove("kept.corp", 3389, None).is_err());
        assert_eq!(fs::read(&path).unwrap(), garbage);
        assert_eq!(fs::read(path.with_extension("history")).unwrap(), garbage);

        // A reload for display keeps what was loaded.
        store.reload();
        assert_eq!(store.list().iter().map(|r| r.id).collect::<Vec<_>>(), [kept.id]);
        history.reload();
        assert_eq!(history.list_most_recent_first().len(), 1);

        // A missing file is still an empty store that a change can start.
        fs::remove_file(&path).unwrap();
        assert!(store.upsert(input("New", "new.corp", 3389, None)).is_ok());
        assert_eq!(ConnectionStore::load_from(path.clone()).unwrap().list().len(), 1);
        let _ = fs::remove_file(path.with_extension("history"));
        cleanup(&path);
    }

    #[test]
    fn history_loaded_earlier_keeps_what_another_process_recorded() {
        let path = temp_file("two-histories");
        let mut launcher = MruStore::load_from(path.clone()).unwrap();
        let mut session = MruStore::load_from(path.clone()).unwrap();
        session.record("rdp.corp", 3389, None, None, "RDP", None).unwrap();
        launcher.record("hyper.corp", 3389, None, None, "Hyper", None).unwrap();
        let hosts: Vec<_> = MruStore::load_from(path.clone()).unwrap().list_most_recent_first().iter().map(|r| r.host.clone()).collect();
        assert_eq!(hosts, ["hyper.corp", "rdp.corp"]);
        session.record("late.corp", 3389, None, None, "Late", None).unwrap();
        assert!(launcher.remove("hyper.corp", 3389, None).unwrap());
        let hosts: Vec<_> = MruStore::load_from(path.clone()).unwrap().list_most_recent_first().iter().map(|r| r.host.clone()).collect();
        assert_eq!(hosts, ["late.corp", "rdp.corp"]);
        cleanup(&path);
    }

    #[test]
    fn a_deleted_connection_comes_back_whole() {
        let path = temp_file("restore");
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        let a = store.upsert(input("A", "a.corp", 3389, Some("pa"))).unwrap();
        let b = store.upsert(input("B", "b.corp", 3390, Some("pb"))).unwrap();
        let c = store.upsert(input("C", "c.corp", 3391, None)).unwrap();
        let at = store.position(&b.id).unwrap();
        let gone = store.get(&b.id).cloned().unwrap();
        assert!(store.delete(&b.id).unwrap());
        assert!(store.restore(gone.clone(), at).unwrap());
        assert!(!store.restore(gone, at).unwrap(), "a second restore is a no-op");
        // Back in its place, with its password, after a reload too.
        let reloaded = ConnectionStore::load_from(path.clone()).unwrap();
        let ids: Vec<_> = reloaded.list().iter().map(|r| r.id).collect();
        assert_eq!(ids, [a.id, b.id, c.id]);
        let back = reloaded.get(&b.id).unwrap();
        assert_eq!(back.saved_password.unprotect().unwrap(), "pb");
        assert_eq!((back.port, back.created_at), (3390, b.created_at));
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
    fn mru_entries_can_be_forgotten() {
        let path = temp_file("mru-remove");
        let mut store = MruStore::load_from(path.clone()).unwrap();
        store.record("pc.lan", 3389, Some("bob".into()), None, "PC", None).unwrap();
        store.record("pc.lan", 3389, Some("amy".into()), None, "PC", None).unwrap();
        assert!(store.remove("PC.lan", 3389, Some("Bob")).unwrap());
        assert!(!store.remove("pc.lan", 3390, Some("amy")).unwrap());
        let reloaded = MruStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.list_most_recent_first().len(), 1);
        assert_eq!(reloaded.list_most_recent_first()[0].username.as_deref(), Some("amy"));
        cleanup(&path);
    }

    #[test]
    fn a_saved_password_can_be_replaced_with_none() {
        let path = temp_file("conn-replace");
        let mut store = ConnectionStore::load_from(path.clone()).unwrap();
        let rec = store.upsert(input("PC", "pc.lan", 3389, Some("secret"))).unwrap();
        assert!(store.replace_password(&rec.id, ProtectedPassword::protect("").unwrap()).unwrap());
        let reloaded = ConnectionStore::load_from(path.clone()).unwrap();
        assert_eq!(reloaded.get(&rec.id).unwrap().saved_password.unprotect().unwrap(), "");
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
    fn write_json_atomic_persists_history_and_leaves_no_temp_files() {
        let path = temp_file("atomic-history");
        // The exact payload the hub persists for Recent: a newest-first MRU
        // history list.
        let payload = vec![
            mru(Some("alice"), 1_700_000_000),
            mru(None, 1_700_000_001),
        ];

        // Step-1 acceptance: history writes go through write-temp-then-rename.
        write_json_atomic(&path, &payload).unwrap();

        // The file reloads through the store's own loader and parses back to
        // the same records.
        let reloaded: Vec<MruRecord> = load_records(&path, "history");
        assert_eq!(reloaded.len(), 2, "history payload must survive a reload");
        assert_eq!(reloaded[0].host, "box.local");
        assert_eq!(reloaded[0].username.as_deref(), Some("alice"));
        assert_eq!(reloaded[0].last_connected_at, 1_700_000_000);
        assert_eq!(reloaded[1].username, None);
        assert_eq!(reloaded[1].last_connected_at, 1_700_000_001);

        // No temp file remains: the uniquely-named `*.tmp` was renamed over
        // the target.
        assert!(
            leftover_temp_files(&path).is_empty(),
            "temp files must not remain: {:?}",
            leftover_temp_files(&path)
        );

        // A second write through the same helper still leaves nothing behind
        // (the PID+counter naming never collides with the previous temp file).
        write_json_atomic(&path, &payload).unwrap();
        assert!(leftover_temp_files(&path).is_empty());
        cleanup(&path);
    }

    #[test]
    fn settings_missing_file_loads_default_on() {
        let path = temp_file("settings-missing");
        // Never written: load_settings must not panic and must fall back to
        // the ON default.
        let settings = load_settings(&path);
        assert!(
            settings.keep_hub_open,
            "missing settings must default to keep-hub-open ON"
        );
        cleanup(&path);
    }

    #[test]
    fn settings_save_then_load_round_trips() {
        let path = temp_file("settings-roundtrip");

        // OFF survives a save → load round-trip.
        save_settings(&path, &Settings { keep_hub_open: false }).unwrap();
        assert!(!load_settings(&path).keep_hub_open);

        // ON (the default) round-trips too.
        save_settings(&path, &Settings::default()).unwrap();
        assert!(load_settings(&path).keep_hub_open);

        // The atomic save leaves no temp sibling behind.
        assert!(
            leftover_temp_files(&path).is_empty(),
            "settings save must not leave temp files: {:?}",
            leftover_temp_files(&path)
        );
        cleanup(&path);
    }

    #[test]
    fn settings_garbage_file_loads_default_on() {
        let path = temp_file("settings-garbage");
        fs::write(&path, b"\x00\x01{ not json \xff\xfe").unwrap();

        // Corrupt settings must never panic and must fall back to the ON
        // default.
        let settings = load_settings(&path);
        assert!(
            settings.keep_hub_open,
            "corrupt settings must default to keep-hub-open ON"
        );
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

    /// A fixed MRU record with the given username / timestamp; the remaining
    /// fields are constants so tests read as "username present / absent".
    fn mru(user: Option<&str>, last_connected_at: u64) -> MruRecord {
        MruRecord {
            host: "box.local".into(),
            port: 3389,
            username: user.map(String::from),
            domain: None,
            display_name: "Box".into(),
            last_connected_at,
            connect_count: 1,
            connection_id: None,
        }
    }
}
