//! Win32 clipboard provider for the cliprdr channel.
//!
//! Implements [`rdp_channels::cliprdr::ClipboardProvider`] over the Win32
//! clipboard: text (`CF_UNICODETEXT`), images (`CF_DIB`), and file copy in both
//! directions (`CF_HDROP`), including folders.
//!
//! **Files copied in the session are fetched lazily.** When the session
//! announces files we publish a *delayed-render* `CF_HDROP` — the clipboard
//! advertises files but holds no data — so copying a 10 GB file inside the
//! session costs nothing. The bytes move only if the user actually pastes, at
//! which point Windows sends `WM_RENDERFORMAT` to the owner window and
//! [`crate::window`] drives the transfer. Received data streams straight to
//! disk (never buffered in memory) and is cleaned up when the copy is
//! superseded or the session ends.
//!
//! Runs on the session worker thread (clipboard APIs work on any thread); only
//! the delayed-render callback runs on the UI thread.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rdp_channels::cliprdr::{ClipFile, ClipboardProvider};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// Open the Windows clipboard, retrying briefly before giving up.
///
/// The clipboard is one global lock, and the app that just changed it is often
/// still holding it when the change notification arrives. Explorer takes
/// materially longer to publish a *folder* than a single file — it builds the
/// whole file-group descriptor first — so a single-shot `OpenClipboard` loses
/// that race precisely when a folder was copied, which is why folder copies
/// could fail while file copies worked. Microsoft's own guidance for this API is
/// to retry rather than treat the first failure as final.
///
/// Returning false is logged, never silent: a swallowed failure here makes
/// `get_files` report "no files", which makes the client advertise nothing and
/// the copy simply never happen — with no trace of why.
unsafe fn open_clipboard_retrying(owner: Option<HWND>) -> bool {
    /// ~250 ms total. Long enough for Explorer to finish publishing a large
    /// folder, short enough not to stall the session worker noticeably.
    const ATTEMPTS: u32 = 12;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
    let mut last = None;
    for _ in 0..ATTEMPTS {
        match OpenClipboard(owner) {
            Ok(()) => return true,
            Err(e) => last = Some(e),
        }
        std::thread::sleep(BACKOFF);
    }
    tracing::warn!(
        attempts = ATTEMPTS,
        error = ?last.map(|e| e.code()),
        "could not open the Windows clipboard; another app is holding it"
    );
    false
}

/// `CF_DIB` — a device-independent bitmap (`BITMAPINFO` + pixels): the format
/// Windows apps put screenshots and copied images on the clipboard as.
const CF_DIB: u32 = 8;
/// `CF_UNICODETEXT` clipboard format.
const CF_UNICODETEXT: u32 = 13;
/// `CF_HDROP` — a dropped-file list (for clipboard file copy).
pub(crate) const CF_HDROP: u32 = 15;

/// Ceiling on how many entries one copied folder tree may expand to, so a
/// pathological directory can't stall the session enumerating it.
const MAX_TREE_ENTRIES: usize = 65_536;
/// Ceiling on recursion depth when walking a copied folder.
const MAX_TREE_DEPTH: usize = 32;

/// The Win32 clipboard, exposed as a cliprdr provider.
pub struct Win32Clipboard {
    /// Flattened local paths for the current outbound copy, index-aligned with
    /// the [`ClipFile`] list returned by `get_files` (directories included, so
    /// a File Contents Request's `lindex` maps straight through).
    files: Vec<PathBuf>,
    /// Open handle + position for the file currently being served, so a large
    /// upload doesn't reopen and re-seek the file for every chunk.
    upload: Option<UploadCursor>,
    /// Where received files are written (`--clipboard-dir`, else a temp folder).
    download_dir: Option<PathBuf>,
    /// Per-copy staging directory for the inbound copy in progress.
    staging: Option<PathBuf>,
    /// Staging directories still on disk, removed when superseded / on drop.
    stale_staging: Vec<PathBuf>,
    /// Top-level entries of the inbound copy — what a paste actually yields.
    staged_roots: Vec<PathBuf>,
    /// The file currently streaming in.
    current: Option<std::fs::File>,
    /// Distinguishes successive copies' staging directories.
    copy_seq: u64,
    /// Window that owns the clipboard, for delayed rendering. `0` until set.
    owner_hwnd: isize,
}

/// An open upload file plus its current read position.
struct UploadCursor {
    index: u32,
    file: std::fs::File,
    pos: u64,
}

impl Win32Clipboard {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            upload: None,
            download_dir: None,
            staging: None,
            stale_staging: Vec::new(),
            staged_roots: Vec::new(),
            current: None,
            copy_seq: 0,
            owner_hwnd: 0,
        }
    }

    /// Keep files copied in the session in `dir` instead of a temp folder.
    pub fn with_download_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.download_dir = dir;
        self
    }

    /// The window that owns the clipboard. Required for delayed rendering: it
    /// is the window Windows sends `WM_RENDERFORMAT` to when someone pastes.
    pub fn with_owner(mut self, hwnd_raw: isize) -> Self {
        self.owner_hwnd = hwnd_raw;
        self
    }

    /// Fresh staging directory for one inbound copy. Previous ones are queued
    /// for deletion — that is what keeps temp space bounded.
    fn new_staging(&mut self) -> Option<PathBuf> {
        if let Some(old) = self.staging.take() {
            self.stale_staging.push(old);
        }
        self.sweep_stale();
        let root = match &self.download_dir {
            Some(d) => d.clone(),
            None => std::env::temp_dir().join(format!("rdpio-clipboard-{}", std::process::id())),
        };
        self.copy_seq += 1;
        let dir = root.join(self.copy_seq.to_string());
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                self.staging = Some(dir.clone());
                Some(dir)
            }
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "cannot create clipboard staging dir");
                None
            }
        }
    }

    /// Delete staging directories from previous copies.
    fn sweep_stale(&mut self) {
        for dir in std::mem::take(&mut self.stale_staging) {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => tracing::debug!(dir = %dir.display(), "removed staged clipboard files"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::debug!(error = %e, dir = %dir.display(), "could not remove staging dir");
                    // Try again next time rather than leaking the path.
                    self.stale_staging.push(dir);
                }
            }
        }
    }

    /// Resolve a peer-supplied relative entry name under `base`, rejecting any
    /// component that would escape it (a hostile server sending `..\\..\\`).
    fn safe_join(base: &Path, name: &str) -> Option<PathBuf> {
        let mut p = base.to_path_buf();
        for comp in name.split(['\\', '/']) {
            match comp {
                "" | "." => {}
                ".." => return None,
                other if other.contains(':') => return None, // drive-qualified
                other => p.push(other),
            }
        }
        (p != base).then_some(p)
    }

    /// Flatten `path` into clipboard entries: a plain file yields one entry; a
    /// directory yields itself plus everything under it, named RELATIVE to the
    /// copy root with backslashes (what MS-RDPECLIP expects).
    fn collect_entry(&mut self, path: &Path, out: &mut Vec<ClipFile>) {
        let Some(base) = path.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            return;
        };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "skipping unreadable clipboard entry");
                return;
            }
        };
        if !meta.is_dir() {
            out.push(ClipFile {
                name: base,
                size: meta.len(),
                is_dir: false,
            });
            self.files.push(path.to_path_buf());
            return;
        }
        out.push(ClipFile {
            name: base.clone(),
            size: 0,
            is_dir: true,
        });
        self.files.push(path.to_path_buf());
        self.collect_dir(path, &base, 1, out);
    }

    /// Recurse `dir`, emitting entries named `prefix\\...`.
    fn collect_dir(&mut self, dir: &Path, prefix: &str, depth: usize, out: &mut Vec<ClipFile>) {
        if depth > MAX_TREE_DEPTH || out.len() >= MAX_TREE_ENTRIES {
            tracing::warn!(dir = %dir.display(), "clipboard folder too large/deep; truncating");
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, dir = %dir.display(), "cannot read copied folder");
                return;
            }
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_TREE_ENTRIES {
                tracing::warn!("clipboard folder exceeds the entry cap; truncating");
                return;
            }
            let path = entry.path();
            let Some(name) = path.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let rel = format!("{prefix}\\{name}");
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                out.push(ClipFile {
                    name: rel.clone(),
                    size: 0,
                    is_dir: true,
                });
                self.files.push(path.clone());
                self.collect_dir(&path, &rel, depth + 1, out);
            } else {
                out.push(ClipFile {
                    name: rel,
                    size: meta.len(),
                    is_dir: false,
                });
                self.files.push(path);
            }
        }
    }
}

impl Drop for Win32Clipboard {
    fn drop(&mut self) {
        // Don't leave staged copies behind when the session ends.
        if let Some(d) = self.staging.take() {
            self.stale_staging.push(d);
        }
        self.sweep_stale();
    }
}

impl ClipboardProvider for Win32Clipboard {
    fn get_text(&mut self) -> Option<String> {
        unsafe {
            if !open_clipboard_retrying(None) {
                return None;
            }
            // Read inside a closure so we always CloseClipboard afterwards.
            let text = (|| {
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                // In windows 0.57 `HANDLE` wraps an `isize` while `HGLOBAL` wraps
                // a raw pointer, so the two must be bridged explicitly.
                let hglobal = HGLOBAL(handle.0 as *mut core::ffi::c_void);
                let ptr = GlobalLock(hglobal) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(hglobal);
                Some(s)
            })();
            let _ = CloseClipboard();
            text
        }
    }

    fn get_files(&mut self) -> Vec<ClipFile> {
        // Snapshot the dropped paths, then walk them with the clipboard closed —
        // enumerating a big tree while holding the clipboard open would block
        // every other app on the machine.
        // If OUR window owns the clipboard, whatever CF_HDROP is on it is our own
        // delayed-render advertisement of the SESSION's files. Asking Windows for
        // it would post WM_RENDERFORMAT to our UI thread, which blocks waiting for
        // this very worker thread to stage the transfer — a deadlock until the
        // 180 s paste timeout. There is also nothing to send: those files came
        // from the session, so offering them back is a loop.
        let roots: Vec<PathBuf> = unsafe {
            if self.owner_hwnd != 0
                && GetClipboardOwner().is_ok_and(|o| o.0 as isize == self.owner_hwnd)
            {
                tracing::debug!("skipping clipboard enumeration; the session's own offer owns it");
                return Vec::new();
            }
            if !open_clipboard_retrying(None) {
                return Vec::new();
            }
            let roots = (|| {
                let handle = GetClipboardData(CF_HDROP).ok()?;
                let hdrop = HDROP(handle.0);
                // 0xFFFFFFFF → number of files in the drop.
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
                let mut out = Vec::new();
                for i in 0..count {
                    // Query the path length (chars, excl. NUL), then fetch it.
                    let len = DragQueryFileW(hdrop, i, None) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len + 1];
                    let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                    out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
                }
                Some(out)
            })()
            .unwrap_or_default();
            let _ = CloseClipboard();
            roots
        };
        self.files.clear();
        self.upload = None; // the index→path mapping is about to change
        let mut out = Vec::new();
        for root in &roots {
            self.collect_entry(root, &mut out);
        }
        out
    }

    fn read_file(&mut self, index: u32, offset: u64, len: u32) -> Option<Vec<u8>> {
        // Reuse the open handle across chunks: a multi-gigabyte upload would
        // otherwise reopen and re-seek the file thousands of times.
        let reusable = self
            .upload
            .as_ref()
            .is_some_and(|u| u.index == index);
        if !reusable {
            let path = self.files.get(index as usize)?;
            // A directory entry carries no bytes, but peers do probe them — and
            // `File::open` on a directory fails on Windows, which would turn into
            // a CB_RESPONSE_FAIL and make the far side abandon the whole paste
            // rather than just that entry. Answer with an empty range instead.
            if path.is_dir() {
                return Some(Vec::new());
            }
            let file = std::fs::File::open(path).ok()?;
            self.upload = Some(UploadCursor {
                index,
                file,
                pos: 0,
            });
        }
        let cur = self.upload.as_mut()?;
        if cur.pos != offset {
            cur.file.seek(SeekFrom::Start(offset)).ok()?;
            cur.pos = offset;
        }
        // Fill the whole requested range: `Read::read` is ONE syscall and may
        // return fewer bytes than asked for even mid-file, but the peer treats
        // a short response as the complete answer for that range and moves on —
        // which silently truncates/corrupts the transfer. Small text files
        // usually survive in one read; a multi-megabyte .exe does not. Loop
        // until the buffer is full or we genuinely hit EOF.
        let mut buf = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match cur.file.read(&mut buf[filled..]) {
                Ok(0) => break, // EOF — a short tail here is legitimate
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.upload = None;
                    return None;
                }
            }
        }
        cur.pos += filled as u64;
        buf.truncate(filled);
        Some(buf)
    }

    fn wants_remote_files(&self) -> bool {
        true
    }

    fn offer_remote_files(&mut self, files: &[ClipFile]) {
        // Advertise WITHOUT transferring: a delayed-render CF_HDROP tells
        // Windows we can produce files, and we only fetch them if a paste
        // actually happens (WM_RENDERFORMAT → crate::window).
        if let Some(d) = self.staging.take() {
            self.stale_staging.push(d);
        }
        self.sweep_stale();
        self.staged_roots.clear();
        if files.is_empty() || self.owner_hwnd == 0 {
            return;
        }
        let total: u64 = files.iter().map(|f| f.size).sum();
        tracing::info!(
            entries = files.len(),
            bytes = total,
            "session offered clipboard files; advertising locally (not transferred yet)"
        );
        // Publish FIRST: `EmptyClipboard` inside it sends WM_DESTROYCLIPBOARD to
        // the previous owner — which is our own window when this is a repeat
        // offer — and that handler clears the pending-files state. Recording the
        // offer afterwards keeps it from wiping the offer we just made.
        unsafe { publish_delayed_hdrop(self.owner_hwnd) };
        crate::session::clipboard_files_offered(files.len());
    }

    fn begin_remote_file(&mut self, name: &str, size: u64, is_dir: bool) {
        self.current = None;
        let dir = match self.staging.clone() {
            Some(d) => d,
            None => match self.new_staging() {
                Some(d) => d,
                None => return,
            },
        };
        let Some(path) = Self::safe_join(&dir, name) else {
            tracing::warn!(name, "rejecting clipboard entry with an unsafe path");
            return;
        };
        // A top-level entry (no separator) is what the paste actually yields.
        if !name.contains('\\') && !name.contains('/') {
            self.staged_roots.push(path.clone());
        }
        if is_dir {
            if let Err(e) = std::fs::create_dir_all(&path) {
                tracing::warn!(error = %e, dir = %path.display(), "cannot create pasted folder");
            }
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::create(&path) {
            Ok(f) => {
                // Preallocate so a large write doesn't fragment or grow the
                // file thousands of times.
                if size > 0 {
                    let _ = f.set_len(size);
                }
                self.current = Some(f);
            }
            Err(e) => tracing::warn!(error = %e, file = %path.display(), "cannot create pasted file"),
        }
    }

    fn write_remote_chunk(&mut self, data: &[u8]) {
        if let Some(f) = self.current.as_mut() {
            if let Err(e) = f.write_all(data) {
                tracing::warn!(error = %e, "failed writing pasted file chunk");
                self.current = None;
            }
        }
    }

    fn end_remote_file(&mut self) {
        self.current = None;
    }

    fn finish_remote_files(&mut self) {
        // The bytes are on disk. The UI thread turns `staged_roots` into the
        // real CF_HDROP inside WM_RENDERFORMAT (only it may set clipboard data
        // during a render), so just hand the paths over.
        let paths = std::mem::take(&mut self.staged_roots);
        tracing::info!(count = paths.len(), "clipboard files staged; completing the paste");
        crate::session::clipboard_files_ready(paths);
    }

    fn abort_remote_files(&mut self) {
        self.current = None;
        self.staged_roots.clear();
        if let Some(d) = self.staging.take() {
            self.stale_staging.push(d);
        }
        self.sweep_stale();
        // Unblock a paste that is waiting on us.
        crate::session::clipboard_files_ready(Vec::new());
    }

    fn set_text(&mut self, text: &str) {
        self.set_contents(Some(text), None);
    }

    fn get_image(&mut self) -> Option<Vec<u8>> {
        // CF_DIB is a self-describing blob (BITMAPINFO + pixels) and both ends
        // of the channel speak exactly that, so the bytes pass straight through
        // — no decode, no re-encode, no colour loss.
        unsafe { get_clipboard_bytes(CF_DIB) }
    }

    fn set_image(&mut self, dib: &[u8]) {
        self.set_contents(None, Some(dib));
    }

    fn set_contents(&mut self, text: Option<&str>, image: Option<&[u8]>) {
        // Publish every format of one paste in a SINGLE clipboard session: each
        // write must EmptyClipboard first, so writing them separately would
        // leave only the last (copying text+image would lose the text).
        let mut formats: Vec<(u32, Vec<u8>)> = Vec::new();
        if let Some(t) = text {
            let mut utf16: Vec<u16> = t.encode_utf16().collect();
            utf16.push(0); // NUL terminator
            formats.push((
                CF_UNICODETEXT,
                utf16.iter().flat_map(|u| u.to_le_bytes()).collect(),
            ));
        }
        // A DIB needs at least its BITMAPINFOHEADER; anything shorter is junk.
        if let Some(dib) = image.filter(|d| d.len() >= 40) {
            formats.push((CF_DIB, dib.to_vec()));
            tracing::info!(bytes = dib.len(), "clipboard image pasted from session");
        }
        if formats.is_empty() {
            return;
        }
        unsafe { set_clipboard_formats(&formats) };
    }
}

/// Pack paths into a `CF_HDROP` payload: a `DROPFILES` header followed by a
/// double-NUL-terminated list of NUL-terminated UTF-16 paths.
pub(crate) fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_le_bytes()); // pFiles: offset of the list
    out.extend_from_slice(&0u32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0u32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide: paths are UTF-16
    for p in paths {
        for u in p.as_os_str().to_string_lossy().encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]); // terminate this path
    }
    out.extend_from_slice(&[0, 0]); // terminate the list
    out
}

/// Allocate a `GMEM_MOVEABLE` block holding `bytes`, ready to hand to
/// `SetClipboardData` (which takes ownership on success).
unsafe fn global_block(bytes: &[u8]) -> Option<HGLOBAL> {
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let ptr = GlobalLock(hglobal) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(Some(hglobal));
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(hglobal);
    Some(hglobal)
}

/// Advertise `CF_HDROP` with NO data (delayed rendering), owned by `hwnd_raw`.
/// Windows asks that window for the data via `WM_RENDERFORMAT` if — and only
/// if — something actually pastes.
unsafe fn publish_delayed_hdrop(hwnd_raw: isize) {
    crate::session::suppress_clipboard_echo();
    let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
    if !open_clipboard_retrying(Some(hwnd)) {
        tracing::warn!("could not open the clipboard to advertise session files");
        return;
    }
    let _ = EmptyClipboard(); // makes `hwnd` the clipboard owner
    if SetClipboardData(CF_HDROP, None).is_err() {
        tracing::debug!("could not advertise delayed CF_HDROP");
    }
    let _ = CloseClipboard();
}

/// Satisfy a `WM_RENDERFORMAT` for `CF_HDROP`. Called on the UI thread from the
/// window procedure with the clipboard already in the rendering state — it must
/// NOT open or empty the clipboard.
pub(crate) fn render_hdrop(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    unsafe {
        if let Some(h) = global_block(&hdrop_bytes(paths)) {
            if SetClipboardData(CF_HDROP, Some(HANDLE(h.0))).is_err() {
                let _ = GlobalFree(Some(h));
            }
        }
    }
}

/// Read a clipboard format's raw bytes. The size comes from `GlobalSize`, which
/// is what makes a self-describing blob like `CF_DIB` copyable without parsing.
unsafe fn get_clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    if !open_clipboard_retrying(None) {
        return None;
    }
    let out = (|| {
        let handle = GetClipboardData(format).ok()?;
        let hglobal = HGLOBAL(handle.0 as *mut core::ffi::c_void);
        let size = GlobalSize(hglobal);
        if size == 0 {
            return None;
        }
        let ptr = GlobalLock(hglobal) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = GlobalUnlock(hglobal);
        Some(bytes)
    })();
    let _ = CloseClipboard();
    out
}

/// Publish `formats` — `(format id, raw bytes)` — on the clipboard, replacing
/// its contents, in one open/empty/set session so every format survives.
unsafe fn set_clipboard_formats(formats: &[(u32, Vec<u8>)]) {
    // Don't echo our own write back to the server as a "local change".
    crate::session::suppress_clipboard_echo();
    if !open_clipboard_retrying(None) {
        tracing::warn!("could not open the clipboard to apply the remote paste");
        return;
    }
    let _ = EmptyClipboard();
    for (format, bytes) in formats {
        if let Some(h) = global_block(bytes) {
            if SetClipboardData(*format, Some(HANDLE(h.0))).is_err() {
                // Ownership didn't transfer; reclaim the block.
                let _ = GlobalFree(Some(h));
            }
        }
    }
    let _ = CloseClipboard();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clipboard file transfer must return the FULL requested range. The old
    /// implementation issued a single `Read::read`, which is allowed to come up
    /// short mid-file; the peer treats that short answer as the whole range and
    /// advances, so binaries (an .exe, a zip) arrived truncated/corrupt while
    /// small text files — satisfied by one read — looked fine.
    #[test]
    fn read_file_fills_the_whole_requested_range() {
        let dir = std::env::temp_dir().join(format!("rdpio_clip_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.bin");
        // Several MB, byte-position-derived so any mis-ordering is detectable.
        let data: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let mut clip = Win32Clipboard::new();
        clip.files = vec![path.clone()];

        // A whole-file range comes back complete and byte-exact.
        let got = clip.read_file(0, 0, data.len() as u32).unwrap();
        assert_eq!(got.len(), data.len());
        assert_eq!(got, data);

        // An interior range is exact too (offset honoured, no drift).
        let (offset, len) = (1_000_003u64, 700_000u32);
        let got = clip.read_file(0, offset, len).unwrap();
        assert_eq!(got.len(), len as usize);
        assert_eq!(got[..], data[offset as usize..offset as usize + len as usize]);

        // A range running past EOF returns just the tail — the one legitimate
        // short answer.
        let tail_at = data.len() as u64 - 10;
        let got = clip.read_file(0, tail_at, 4096).unwrap();
        assert_eq!(got.len(), 10);
        assert_eq!(got[..], data[tail_at as usize..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sequential chunks must reuse the open handle (no reopen per chunk) and
    /// still stitch back together byte-exactly.
    #[test]
    fn sequential_chunks_reuse_the_handle_and_stay_exact() {
        let dir = std::env::temp_dir().join(format!("rdpio_clipseq_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stream.bin");
        let data: Vec<u8> = (0..500_000).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let mut clip = Win32Clipboard::new();
        clip.files = vec![path];

        let mut got = Vec::new();
        let chunk = 64 * 1024u32;
        let mut offset = 0u64;
        while offset < data.len() as u64 {
            let part = clip.read_file(0, offset, chunk).unwrap();
            assert!(!part.is_empty());
            offset += part.len() as u64;
            got.extend_from_slice(&part);
            // The cursor tracks position, so no seek was needed after the first.
            assert_eq!(clip.upload.as_ref().unwrap().pos, offset);
        }
        assert_eq!(got, data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hdrop_payload_is_well_formed() {
        let paths = vec![PathBuf::from(r"C:\tmp\a.exe"), PathBuf::from(r"C:\tmp\b.bin")];
        let b = hdrop_bytes(&paths);
        // DROPFILES: list starts at 20, and fWide marks the paths as UTF-16.
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 20);
        assert_eq!(u32::from_le_bytes([b[16], b[17], b[18], b[19]]), 1);
        // The list is double-NUL terminated.
        assert_eq!(&b[b.len() - 4..], &[0, 0, 0, 0]);
        let units: Vec<u16> = b[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let joined = String::from_utf16_lossy(&units);
        assert!(joined.starts_with("C:\\tmp\\a.exe\0"));
        assert!(joined.contains("C:\\tmp\\b.bin"));
    }

    #[test]
    fn a_copied_folder_flattens_to_relative_entries() {
        let dir = std::env::temp_dir().join(format!("rdpio_cliptree_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/sub")).unwrap();
        std::fs::write(dir.join("docs/a.txt"), b"aaa").unwrap();
        std::fs::write(dir.join("docs/sub/b.bin"), b"bb").unwrap();

        let mut clip = Win32Clipboard::new();
        let mut out = Vec::new();
        clip.collect_entry(&dir.join("docs"), &mut out);

        // The root folder, then its contents under a relative backslash path.
        assert_eq!(out[0].name, "docs");
        assert!(out[0].is_dir);
        let mut names: Vec<String> = out.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            ["docs", "docs\\a.txt", "docs\\sub", "docs\\sub\\b.bin"]
        );
        // Sizes are carried for files; directories are zero.
        let a = out.iter().find(|e| e.name == "docs\\a.txt").unwrap();
        assert_eq!((a.size, a.is_dir), (3, false));
        // The index→path map stays aligned with the entry list.
        assert_eq!(clip.files.len(), out.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_entry_names_are_rejected() {
        let base = Path::new(r"C:\stage");
        assert!(Win32Clipboard::safe_join(base, r"..\evil.exe").is_none());
        assert!(Win32Clipboard::safe_join(base, r"a\..\..\evil.exe").is_none());
        assert!(Win32Clipboard::safe_join(base, r"C:\absolute.exe").is_none());
        assert_eq!(
            Win32Clipboard::safe_join(base, r"docs\ok.txt"),
            Some(PathBuf::from(r"C:\stage\docs\ok.txt"))
        );
    }
}
