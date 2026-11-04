//! Clipboard sync for Hyper (both ends): a hidden message-only window listens
//! for clipboard changes (`AddClipboardFormatListener`) and reports text,
//! images and copied files; remote contents are applied with the same format
//! writer the RDP clipboard uses. Our own writes are recognised by the
//! clipboard sequence number, so a paste never echoes back.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rdp_channels::cliprdr::ClipboardProvider;
use rdp_hyper::messages::{FileEntry, Message};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, IsClipboardFormatAvailable, RemoveClipboardFormatListener,
};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream,
    IStream_Impl, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM,
    STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED, TYMED_HGLOBAL,
    TYMED_ISTREAM,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{get_clipboard_bytes, global_block, hdrop_bytes, set_clipboard_formats, CF_DIB, CF_HDROP, CF_UNICODETEXT};
use crate::hyper_link::ReliableLink;

/// Text and images larger than this aren't synced.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Copies larger than this (all files together) aren't transferred — and an
/// inbound one that declares more is refused.
const MAX_FILES_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Entries a copy may list (the folder walk stops at 65,536 too); each
/// becomes a file or folder, created on the spot.
const MAX_FILES: usize = 65_536;
/// Longest name (UTF-16 units, folders included) and deepest nesting an
/// entry may have.
const MAX_NAME: usize = 1024;
const MAX_DEPTH: usize = 64;
/// An inbound copy that would leave less free disk space than this is refused.
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;
/// File data queued on the link at most. Only a memory bound: the link paces
/// its bulk lane itself, so the game stream comes first.
const MAX_BACKLOG: usize = 16 * 1024 * 1024;
const CHUNK: u32 = 1024 * 1024;

static LINK: Mutex<Option<Arc<ReliableLink>>> = Mutex::new(None);
/// The newest outbound file transfer; older ones stop when it changes.
static TRANSFER: AtomicU32 = AtomicU32::new(0);
/// The outbound transfer still sending, and what was copied for it.
static SENDING: Mutex<Option<(u32, Vec<PathBuf>)>> = Mutex::new(None);
/// The clipboard sequence number our last write left. A write holds the lock
/// until its number is recorded, and the listener holds it while it judges a
/// change, so it never sees one of our writes unrecorded (and echoes it).
static OUR_SEQ: Mutex<u32> = Mutex::new(0);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

/// Posted to the clipboard window: offer [`PROMISED`] files on the clipboard.
const WM_PROMISE_FILES: u32 = WM_APP + 1;
/// How long a paste waits for more of a copied file before giving up on it.
const ARRIVAL_STALL: Duration = Duration::from_secs(120);

/// Files the peer copied are offered on our clipboard as soon as their list
/// comes, the way Remote Desktop does: as virtual files (`FileGroupDescriptorW`
/// + a `FileContents` stream per file, see [`FilesObject`]). A paste starts
/// at once, in Windows' own copy dialog with its progress, and reads each
/// file as it arrives. Before, a paste had to wait for the whole copy: a
/// big one (845 MB) hung Explorer, then failed. Once the copy is in, the
/// finished files replace the offer (`CF_HDROP`, for every app).
struct PromisedFiles {
    transfer: u32,
    dir: PathBuf,
    entries: Vec<FileEntry>,
    arrival: Arc<Arrival>,
}

/// The next files to offer (taken by the clipboard window).
static PROMISED: Mutex<Option<PromisedFiles>> = Mutex::new(None);

/// How much of each file of an inbound copy is in, for pastes reading it
/// meanwhile.
struct Arrival {
    state: Mutex<ArrivalState>,
    changed: Condvar,
}

struct ArrivalState {
    /// Bytes in, per entry.
    got: Vec<u64>,
    /// Given up on (refused, abandoned, superseded, the stream ended).
    failed: bool,
}

impl Arrival {
    fn new(entries: usize) -> Self {
        Self { state: Mutex::new(ArrivalState { got: vec![0; entries], failed: false }), changed: Condvar::new() }
    }

    fn update(&self, f: impl FnOnce(&mut ArrivalState)) {
        f(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }

    /// Every file of `entries` in.
    fn is_complete(&self, entries: &[FileEntry]) -> bool {
        let s = self.state.lock().unwrap();
        !s.failed && entries.iter().zip(&s.got).all(|(e, &got)| e.is_dir || got >= e.size)
    }

    /// Wait until every file of `entries` is in (see [`Self::wait_for`]).
    fn wait_all(&self, entries: &[FileEntry]) -> bool {
        entries.iter().enumerate().all(|(i, e)| e.is_dir || self.wait_for(i, e.size))
    }

    /// Wait until `need` bytes of entry `index` are in: false if the copy
    /// failed, or nothing more came for [`ARRIVAL_STALL`].
    fn wait_for(&self, index: usize, need: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        let mut last = s.got[index];
        let mut deadline = Instant::now() + ARRIVAL_STALL;
        loop {
            if s.failed {
                return false;
            }
            if s.got[index] >= need {
                return true;
            }
            if s.got[index] != last {
                last = s.got[index];
                deadline = Instant::now() + ARRIVAL_STALL;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!("a paste waited for a copied file that stopped coming; gave up");
                return false;
            }
            s = self.changed.wait_timeout(s, left).unwrap().0;
        }
    }
}

/// Running clipboard sync; stops on drop.
pub struct ClipboardSync {
    thread: Option<std::thread::JoinHandle<()>>,
    /// Remote contents are applied on their own thread: clipboard writes and
    /// file staging touch the disk, and the caller is a network receive thread
    /// that also carries input and video.
    apply_tx: Option<std::sync::mpsc::Sender<Message>>,
    applier: Option<std::thread::JoinHandle<()>>,
}

/// An inbound file copy being written to a staging folder.
struct Incoming {
    transfer: u32,
    dir: PathBuf,
    /// Whose rights its files are written with.
    user: SessionUser,
    entries: Vec<FileEntry>,
    open: HashMap<u32, std::fs::File>,
    received: HashMap<u32, u64>,
    remaining: usize,
    /// Bytes in all and so far, and when it began (for the log).
    total: u64,
    done: u64,
    began: Instant,
    /// What's in, for a paste streaming it meanwhile.
    arrival: Arc<Arrival>,
}

impl ClipboardSync {
    /// Report local clipboard changes over `link`.
    pub fn start(link: Arc<ReliableLink>) -> Option<Self> {
        *LINK.lock().unwrap() = Some(link);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("hyper-clipboard".into())
            .spawn(move || unsafe {
                crate::net_tune::eco_thread();
                let _ = windows::Win32::System::Ole::OleInitialize(None);
                if running_as_system() {
                    open_com_to_the_user();
                }
                let class = w!("rdpioHyperClipboard");
                let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() };
                RegisterClassW(&wc);
                let Ok(hwnd) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                ) else {
                    let _ = ready_tx.send(false);
                    return;
                };
                // Without the listener nobody will ever ask this window to
                // quit: tear it down at once, and never publish it.
                if AddClipboardFormatListener(hwnd).is_err() {
                    let _ = DestroyWindow(hwnd);
                    let _ = ready_tx.send(false);
                    return;
                }
                HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
                let _ = ready_tx.send(true);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
                HWND_RAW.store(0, Ordering::SeqCst);
                withdraw_offer();
                windows::Win32::System::Ole::OleUninitialize();
            })
            .ok()?;
        if ready_rx.recv().unwrap_or(false) {
            tracing::info!("clipboard sync on");
            let (apply_tx, apply_rx) = std::sync::mpsc::channel::<Message>();
            let applier = std::thread::Builder::new()
                .name("hyper-clipboard-apply".into())
                .spawn(move || {
                    crate::net_tune::eco_thread();
                    let mut incoming: Option<Incoming> = None;
                    for m in apply_rx {
                        apply_now(&mut incoming, m);
                    }
                    // The stream ended mid-copy: no half-written files left.
                    abandon(&mut incoming);
                })
                .ok();
            Some(Self { thread: Some(thread), apply_tx: Some(apply_tx), applier })
        } else {
            *LINK.lock().unwrap() = None;
            let _ = thread.join();
            tracing::warn!("clipboard sync unavailable");
            None
        }
    }

    /// Put the other side's clipboard contents on ours (queued; returns at once).
    pub fn apply(&self, m: Message) {
        if let Some(tx) = &self.apply_tx {
            let _ = tx.send(m);
        }
    }
}

fn apply_now(incoming: &mut Option<Incoming>, m: Message) {
    let formats = match m {
        Message::ClipboardFiles { transfer, entries } => {
            // A new copy supersedes an unfinished one (its sender moved on).
            abandon(incoming);
            let Some(hdrop) = SessionUser::current().and_then(|user| begin_files(incoming, user, transfer, entries)) else {
                // (Files still to come: offered meanwhile. Or refused.)
                if let Some(inc) = incoming.as_ref().filter(|i| i.transfer == transfer) {
                    promise_files(inc);
                }
                return;
            };
            vec![(CF_HDROP, hdrop)]
        }
        Message::FileChunk { transfer, index, offset, data } => {
            let Some(hdrop) = file_chunk(incoming, transfer, index, offset, &data) else { return };
            vec![(CF_HDROP, hdrop)]
        }
        Message::ClipboardText(t) if t.len() <= MAX_BYTES => {
            let mut utf16: Vec<u16> = t.encode_utf16().collect();
            utf16.push(0);
            vec![(CF_UNICODETEXT, utf16.iter().flat_map(|u| u.to_le_bytes()).collect())]
        }
        Message::ClipboardImage(dib) if dib.len() >= 40 && dib.len() <= MAX_BYTES => vec![(CF_DIB, dib)],
        _ => return,
    };
    write_clipboard(&formats);
}

/// Put the peer's contents on the clipboard, recording the sequence number
/// the write left so its change notification isn't sent back.
fn write_clipboard(formats: &[(u32, Vec<u8>)]) {
    let mut ours = OUR_SEQ.lock().unwrap();
    let before = unsafe { GetClipboardSequenceNumber() };
    unsafe { set_clipboard_formats(formats) };
    // The number is only final once the clipboard is closed: closing adds the
    // formats Windows synthesizes (CF_TEXT, CF_LOCALE…), each one a step. So
    // it's read now — and not claimed if our write didn't happen (another
    // app kept the clipboard open) or what's there isn't what we wrote
    // (another app wrote in that instant): that change is a local copy.
    let seq = unsafe { GetClipboardSequenceNumber() };
    let wrote = seq != before
        && formats.first().is_some_and(|(format, bytes)| unsafe {
            IsClipboardFormatAvailable(*format).is_ok()
                && get_clipboard_bytes(*format).map_or(true, |now| now.starts_with(bytes))
        });
    if wrote {
        *ours = seq;
    }
}

/// Whose rights copied files are read (to send) and written (pasted) with,
/// and where pastes are staged. The direct-mode agent runs as SYSTEM: it
/// acts as the user signed in to its session, and stages in that user's own
/// temp folder. As SYSTEM it would send any file whose path the user puts
/// on the clipboard (the SAM hive, another user's documents), and in a
/// folder the user can change, a junction could aim its writes at any file
/// on the machine. (SYSTEM's own temp folder is no use either: the user
/// couldn't read the pasted files.)
struct SessionUser {
    /// That user's token (`None`: act as ourselves, not being SYSTEM).
    token: Option<HANDLE>,
    /// Each inbound copy gets a numbered folder in here.
    root: PathBuf,
}

impl SessionUser {
    fn current() -> Option<Self> {
        if !running_as_system() {
            return Some(Self { token: None, root: std::env::temp_dir().join("rdpio-hyper-paste") });
        }
        use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::Shell::FOLDERID_LocalAppData;
        let mut session = 0u32;
        let mut token = HANDLE::default();
        let signed_in =
            unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session).and_then(|()| WTSQueryUserToken(session, &mut token)) };
        if let Err(e) = signed_in {
            tracing::warn!(error = %e, "nobody is signed in here to copy files for");
            return None;
        }
        let mut user = Self { token: Some(token), root: PathBuf::new() };
        let Some(local) = crate::hyper_direct::known_folder(&FOLDERID_LocalAppData, Some(token)) else {
            tracing::warn!("can't find the signed-in user's local app data; file copies are off");
            return None;
        };
        user.root = local.join("Temp").join("rdpio-hyper-paste");
        Some(user)
    }

    /// Run `f` with this user's rights.
    fn act<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
        let Some(token) = self.token else { return Some(f()) };
        unsafe {
            if let Err(e) = ImpersonateLoggedOnUser(token) {
                tracing::warn!(error = %e, "can't act as the signed-in user to copy files");
                return None;
            }
            let r = f();
            if RevertToSelf().is_err() {
                // Carrying on as someone else would be worse than stopping.
                std::process::abort();
            }
            Some(r)
        }
    }
}

impl Drop for SessionUser {
    fn drop(&mut self) {
        if let Some(token) = self.token {
            unsafe {
                let _ = CloseHandle(token);
            }
        }
    }
}

/// Whether this process runs as LocalSystem (the direct-mode agent) — by SID:
/// the account's name is localized.
fn running_as_system() -> bool {
    static SYSTEM: OnceLock<bool> = OnceLock::new();
    *SYSTEM.get_or_init(|| unsafe {
        use windows::Win32::Security::{GetTokenInformation, IsWellKnownSid, TokenUser, WinLocalSystemSid, TOKEN_QUERY, TOKEN_USER};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        // TOKEN_USER and the SID it points into; u64s for its alignment.
        let mut buf = [0u64; 32];
        let mut len = 0u32;
        let got = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), std::mem::size_of_val(&buf) as u32, &mut len);
        let _ = CloseHandle(token);
        got.is_ok() && IsWellKnownSid((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid, WinLocalSystemSid).as_bool()
    })
}

/// Start an inbound copy: vet its file list, then — as `user` — lay out its
/// folders and empty files. Returns the paste at once if no file data is to
/// come. A refused copy writes nothing.
fn begin_files(slot: &mut Option<Incoming>, user: SessionUser, transfer: u32, entries: Vec<FileEntry>) -> Option<Vec<u8>> {
    let total = match check_entries(&entries) {
        Ok(total) => total,
        Err(why) => {
            tracing::warn!(files = entries.len(), "refusing copied files: {why}");
            return None;
        }
    };
    let dir = user.root.join(transfer.to_string());
    let remaining = user.act(|| lay_out(&user.root, &dir, &entries, total)).flatten()?;
    tracing::info!(files = entries.len(), bytes = total, "receiving copied files");
    let inc = Incoming {
        transfer,
        dir,
        user,
        arrival: Arc::new(Arrival::new(entries.len())),
        entries,
        open: HashMap::new(),
        received: HashMap::new(),
        remaining,
        total,
        done: 0,
        began: Instant::now(),
    };
    if remaining == 0 {
        return finished(inc);
    }
    *slot = Some(inc);
    None
}

/// Hold a file list — a peer's before anything is created, ours before it
/// is sent — to the entry count and total size limits, and to sane names.
/// Returns the total size.
fn check_entries(entries: &[FileEntry]) -> Result<u64, String> {
    if entries.len() > MAX_FILES {
        return Err(format!("{} entries (at most {MAX_FILES})", entries.len()));
    }
    let shown = |name: &str| name.chars().take(80).collect::<String>();
    let mut total = 0u64;
    for e in entries {
        if e.name.encode_utf16().count() > MAX_NAME || e.name.split(['\\', '/']).filter(|c| !c.is_empty()).count() > MAX_DEPTH {
            return Err(format!("a name too long or too deeply nested ({}…)", shown(&e.name)));
        }
        if relative_path(&e.name).is_none() {
            return Err(format!("a name that escapes the paste folder or isn't a plain file name ({})", shown(&e.name)));
        }
        if !e.is_dir {
            total = total.saturating_add(e.size);
        }
    }
    if total > MAX_FILES_BYTES {
        return Err(format!("{total} bytes (at most {MAX_FILES_BYTES})"));
    }
    Ok(total)
}

/// Make room for a copy of `total` bytes in `dir` (under `root`) and create
/// its folders and empty files; returns how many files have data to come.
/// Nothing is written if it would leave the disk nearly full.
fn lay_out(root: &Path, dir: &Path, entries: &[FileEntry], total: u64) -> Option<usize> {
    if let Some(free) = root.ancestors().find(|p| p.is_dir()).and_then(free_bytes) {
        if free < total.saturating_add(MIN_FREE_BYTES) {
            tracing::warn!(bytes = total, free, "refusing copied files: they would leave less than 1 GB of disk free");
            return None;
        }
    }
    // Earlier pastes' copies are no longer offered; clear them out.
    let _ = std::fs::remove_dir_all(root);
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, path = %dir.display(), "can't create the paste folder");
        return None;
    }
    let mut remaining = 0;
    for e in entries {
        let Some(path) = safe_join(dir, &e.name) else { continue }; // vetted already
        if e.is_dir {
            let _ = std::fs::create_dir_all(&path);
        } else {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if e.size == 0 {
                let _ = std::fs::File::create(&path);
            } else {
                remaining += 1;
            }
        }
    }
    Some(remaining)
}

/// Free disk space available to us on the volume holding `dir` (a folder).
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{BOOL, PCWSTR};
    windows::core::link!("kernel32.dll" "system" fn GetDiskFreeSpaceExW(
        directory: PCWSTR,
        free_to_caller: *mut u64,
        total: *mut u64,
        total_free: *mut u64
    ) -> BOOL);
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(PCWSTR(path.as_ptr()), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    ok.as_bool().then_some(free)
}

/// Write one chunk of the inbound copy. Returns the paste once its last file
/// is complete.
fn file_chunk(slot: &mut Option<Incoming>, transfer: u32, index: u32, offset: u64, data: &[u8]) -> Option<Vec<u8>> {
    let inc = slot.as_mut().filter(|i| i.transfer == transfer)?;
    let entry = inc.entries.get(index as usize)?;
    // Data must land inside the size the list declared (what the copy was
    // admitted on): past it, or into a folder, the peer could grow a file —
    // or reserve disk with one far offset — without bound.
    if entry.is_dir || offset.checked_add(data.len() as u64).map_or(true, |end| end > entry.size) {
        tracing::warn!(index, offset, len = data.len(), "copied-file data outside its file; dropping the copy");
        abandon(slot);
        return None;
    }
    let (size, path) = (entry.size, safe_join(&inc.dir, &entry.name)?);
    if inc.received.get(&index).copied().unwrap_or(0) >= size {
        return None; // that file is complete already (or was empty)
    }
    let written = (|| -> std::io::Result<()> {
        if !inc.open.contains_key(&index) {
            let file = inc
                .user
                .act(|| std::fs::File::create(&path))
                .unwrap_or_else(|| Err(std::io::Error::other("can't act as the signed-in user")))?;
            inc.open.insert(index, file);
        }
        let f = inc.open.get_mut(&index).expect("opened above");
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)
    })();
    if let Err(e) = written {
        tracing::warn!(error = %e, path = %path.display(), "can't write a copied file (disk full?); dropping the copy");
        abandon(slot);
        return None;
    }
    let got = inc.received.entry(index).or_insert(0);
    *got += data.len() as u64;
    let now = *got;
    inc.arrival.update(|s| s.got[index as usize] = now);
    // Progress: a line per tenth of the copy.
    let before = inc.done;
    inc.done += data.len() as u64;
    if inc.total > 0 && before * 10 / inc.total != inc.done * 10 / inc.total {
        tracing::info!(percent = inc.done * 100 / inc.total, secs = inc.began.elapsed().as_secs(), "receiving copied files");
    }
    if *got < size {
        return None;
    }
    inc.open.remove(&index);
    inc.remaining -= 1;
    if inc.remaining > 0 {
        return None;
    }
    finished(slot.take()?)
}

/// Drop the unfinished inbound copy, and what it had written.
fn abandon(slot: &mut Option<Incoming>) {
    if let Some(mut inc) = slot.take() {
        inc.arrival.update(|s| s.failed = true);
        inc.open.clear(); // closed before they're deleted
        let _ = inc.user.act(|| std::fs::remove_dir_all(&inc.dir));
    }
}

/// A finished copy's top-level entries, as the `CF_HDROP` to paste.
fn finished(inc: Incoming) -> Option<Vec<u8>> {
    let roots: Vec<PathBuf> = inc
        .entries
        .iter()
        .filter(|e| !e.name.contains(['\\', '/']))
        .filter_map(|e| safe_join(&inc.dir, &e.name))
        .collect();
    if roots.is_empty() {
        return None;
    }
    tracing::info!(entries = roots.len(), "copied files ready to paste");
    Some(hdrop_bytes(&roots))
}

/// `base` + a peer-supplied relative name, refusing anything that escapes it.
fn safe_join(base: &Path, name: &str) -> Option<PathBuf> {
    relative_path(name).map(|rel| base.join(rel))
}

/// A peer-supplied relative name as a path. Refused: `..`, drive and stream
/// colons, and names Windows would quietly change (a trailing dot or space
/// is dropped: `.. ` would be `..`) or open as a device (`NUL`, `COM1.txt`).
fn relative_path(name: &str) -> Option<PathBuf> {
    let mut p = PathBuf::new();
    for comp in name.split(['\\', '/']) {
        match comp {
            "" | "." => {}
            c if c.ends_with(['.', ' ']) || c.contains(':') || is_device_name(c) => return None,
            c => p.push(c),
        }
    }
    (!p.as_os_str().is_empty()).then_some(p)
}

/// `CON`, `NUL`, `COM1`… — with any extension, any case.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        s => s
            .strip_prefix("COM")
            .or_else(|| s.strip_prefix("LPT"))
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")),
    }
}

/// Send the files now on the clipboard (on a worker thread; a newer copy
/// cancels it). `seq` is the change being sent: if the clipboard has moved
/// on by the time the list is read (a paste from the peer landing), the list
/// isn't that copy's — the newer change has its own notification.
fn send_files(link: Arc<ReliableLink>, seq: u32) {
    let _ = std::thread::Builder::new().name("hyper-files".into()).spawn(move || {
        crate::net_tune::eco_thread();
        let Some(roots) = copied_paths(seq) else { return };
        // The same files copied again while they're still on their way (a
        // paste that seemed to do nothing, so the user copied again): the
        // transfer carries on rather than starting over.
        {
            let mut sending = SENDING.lock().unwrap();
            if sending.as_ref().is_some_and(|(t, r)| *r == roots && *t == TRANSFER.load(Ordering::SeqCst)) {
                tracing::info!(paths = roots.len(), "the same files copied again: still sending them");
                return;
            }
            *sending = None;
        }
        let transfer = TRANSFER.fetch_add(1, Ordering::SeqCst) + 1;
        *SENDING.lock().unwrap() = Some((transfer, roots.clone()));
        tracing::info!(transfer, paths = roots.len(), "files copied here");
        // Only files the user signed in here could read themselves (the list
        // is read as ourselves: the clipboard isn't the user's to guard, and
        // as SYSTEM impersonating them, Explorer's list never came).
        if let Some(user) = SessionUser::current() {
            user.act(|| send_files_now(&link, transfer, &roots));
        }
        let mut sending = SENDING.lock().unwrap();
        if sending.as_ref().is_some_and(|(t, _)| *t == transfer) {
            *sending = None;
        }
    });
}

/// The paths copied in clipboard change `seq`, once Explorer hands them over
/// (it renders the list on request, and can still be busy publishing a big
/// copy); `None`, logged, if they never come or a newer change came first.
fn copied_paths(seq: u32) -> Option<Vec<PathBuf>> {
    if running_as_system() {
        return paths_from_user(seq);
    }
    for _ in 0..10 {
        let roots = unsafe { crate::clipboard::copied_paths() };
        if unsafe { GetClipboardSequenceNumber() } != seq {
            // A newer change, with its own notification.
            tracing::info!("the clipboard changed again before the copied files were read; sending the newer copy");
            return None;
        }
        match roots {
            Some(roots) if !roots.is_empty() => return Some(roots),
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    tracing::warn!("files were copied, but the clipboard never gave their names");
    None
}

/// What [`print_copied_paths`] exits with.
const PATHS_LISTED: u32 = 0;
const PATHS_NONE: u32 = 1;
const PATHS_MOVED_ON: u32 = 2;
const PATHS_NEVER_CAME: u32 = 3;

/// The paths copied in clipboard change `seq`, from a helper running as the
/// signed-in user. As SYSTEM (the direct-mode agent) Windows shows us a file
/// copy's clipboard as one opaque `DataObject`: no `CF_HDROP`, not even while
/// impersonating the user — so every copy of files on a host went unsent,
/// without a word. Only the user's own processes see the list.
fn paths_from_user(seq: u32) -> Option<Vec<PathBuf>> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.ok()?;
    match crate::hyper_direct::output_as_user(session, &format!("--hyper-clipboard-files {seq}")) {
        Ok(Some((PATHS_LISTED, out))) => {
            let roots: Vec<PathBuf> = String::from_utf8_lossy(&out).lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
            (!roots.is_empty()).then_some(roots)
        }
        // Something other than files was copied, or a newer change came.
        Ok(Some((PATHS_NONE | PATHS_MOVED_ON, _))) => None,
        Ok(other) => {
            tracing::warn!(exit = ?other.map(|o| o.0), "files were copied, but the clipboard never gave their names");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "can't read the signed-in user's clipboard for copied files");
            None
        }
    }
}

/// `--hyper-clipboard-files <seq>`, as the signed-in user (see
/// [`paths_from_user`]): print the paths copied in clipboard change `seq`,
/// one a line, waiting a little for Explorer to hand them over. The exit
/// code says how it went.
pub fn print_copied_paths(seq: u32) -> i32 {
    use std::io::Write;
    for _ in 0..10 {
        if unsafe { GetClipboardSequenceNumber() } != seq {
            return PATHS_MOVED_ON as i32;
        }
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) }.is_err() {
            return PATHS_NONE as i32;
        }
        if let Some(roots) = unsafe { crate::clipboard::copied_paths() }.filter(|r| !r.is_empty()) {
            let mut out = std::io::stdout().lock();
            for r in roots {
                let _ = writeln!(out, "{}", r.display());
            }
            return if out.flush().is_ok() { PATHS_LISTED as i32 } else { PATHS_NEVER_CAME as i32 };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    PATHS_NEVER_CAME as i32
}

fn send_files_now(link: &ReliableLink, transfer: u32, roots: &[PathBuf]) {
    let mut cb = crate::clipboard::Win32Clipboard::new();
    let files = cb.list_files(roots);
    if files.is_empty() {
        tracing::warn!(paths = roots.len(), "files were copied, but none of them could be read");
        return;
    }
    let entries: Vec<FileEntry> =
        files.iter().map(|f| FileEntry { name: f.name.clone(), size: f.size, is_dir: f.is_dir }).collect();
    // The limits the other side holds a copy to: one it would refuse (say,
    // several roots past the walk's entry cap) isn't streamed to it for nothing.
    let total = match check_entries(&entries) {
        Ok(total) => total,
        Err(why) => {
            tracing::warn!(files = entries.len(), "not sending copied files: {why}");
            return;
        }
    };
    link.send(&Message::ClipboardFiles { transfer, entries: entries.clone() });
    tracing::info!(files = entries.len(), bytes = total, "sending copied files");
    let began = Instant::now();
    // How the bulk lane reads the path, every few seconds of a copy.
    let mut progress = Instant::now();
    let mut queued_bytes = 0u64;
    let mut report = |queued_bytes: u64| {
        if progress.elapsed() >= Duration::from_secs(2) {
            progress = Instant::now();
            let s = link.bulk_stats();
            tracing::info!(
                transfer,
                queued_mb = queued_bytes / 1_000_000,
                backlog_kb = link.bulk_backlog() / 1000,
                cwnd = s.cwnd as u32,
                srtt_ms = format!("{:.1}", s.srtt.as_secs_f64() * 1000.0),
                min_rtt_ms = format!("{:.1}", s.min_rtt.as_secs_f64() * 1000.0),
                rto_ms = s.rto.as_millis() as u64,
                paced_kbps = s.rate_bps.map(|b| b / 1000),
                wide = s.wide,
                cuts = format!("timeout:{} loss:{} delay:{}", s.cuts_timeout, s.cuts_loss, s.cuts_delay),
                resent = s.resent,
                "copy progress"
            );
        }
    };
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir || e.size == 0 {
            continue;
        }
        let mut offset = 0u64;
        while offset < e.size {
            report(queued_bytes);
            if TRANSFER.load(Ordering::SeqCst) != transfer {
                return; // superseded by a newer copy
            }
            // The link paces its bulk lane; this only bounds the data
            // waiting in memory.
            while link.bulk_backlog() > MAX_BACKLOG {
                std::thread::sleep(std::time::Duration::from_millis(5));
                if TRANSFER.load(Ordering::SeqCst) != transfer {
                    return;
                }
            }
            // Never past the size announced: the receiver drops a copy
            // whose data overruns a file (one that grew meanwhile).
            let want = (e.size - offset).min(CHUNK as u64) as u32;
            let Some(data) = cb.read_file(i as u32, offset, want) else {
                tracing::warn!(name = %e.name, "couldn't read a copied file");
                return;
            };
            if data.is_empty() {
                break;
            }
            let n = data.len() as u64;
            link.send(&Message::FileChunk { transfer, index: i as u32, offset, data });
            offset += n;
            queued_bytes += n;
        }
    }
    // (Handed to the link: on the wire once its bulk lane drains.)
    tracing::info!(transfer, bytes = total, secs = began.elapsed().as_secs(), "copied files all queued to send");
    let queued = began.elapsed();
    while link.bulk_backlog() > 0 {
        if TRANSFER.load(Ordering::SeqCst) != transfer || began.elapsed() > Duration::from_secs(3600) {
            return;
        }
        report(queued_bytes);
        std::thread::sleep(Duration::from_millis(20));
    }
    let secs = began.elapsed().as_secs_f64().max(queued.as_secs_f64()).max(0.001);
    tracing::info!(
        transfer,
        bytes = total,
        ms = (secs * 1000.0) as u64,
        mbps = format!("{:.1}", total as f64 * 8.0 / secs / 1e6),
        "copied files all delivered"
    );
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        let h = HWND_RAW.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.apply_tx.take(); // ends the applier's loop
        if let Some(t) = self.applier.take() {
            let _ = t.join();
        }
        TRANSFER.fetch_add(1, Ordering::SeqCst); // stop any outbound transfer
        *LINK.lock().unwrap() = None;
    }
}

/// Offer an inbound copy's files on the clipboard now (see
/// [`PromisedFiles`]): the clipboard window does it, as their owner.
/// Take our offer off the clipboard (going away: it can't be read then).
unsafe fn withdraw_offer() {
    use windows::Win32::System::DataExchange::GetClipboardOwner;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    let owner = GetClipboardOwner().unwrap_or_default();
    if !owner.is_invalid() && GetWindowThreadProcessId(owner, None) == GetCurrentThreadId() {
        let _ = windows::Win32::System::Ole::OleSetClipboard(None);
    }
}

fn promise_files(inc: &Incoming) {
    // (A virtual file's name is held to MAX_PATH; such a copy waits to be
    // pasted whole.)
    if inc.entries.iter().any(|e| e.name.encode_utf16().count() >= 260) {
        tracing::info!("copied files with a very long name: pasteable once they're all in");
        return;
    }
    *PROMISED.lock().unwrap() = Some(PromisedFiles {
        transfer: inc.transfer,
        dir: inc.dir.clone(),
        entries: inc.entries.clone(),
        arrival: inc.arrival.clone(),
    });
    let h = HWND_RAW.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_PROMISE_FILES, WPARAM(inc.transfer as usize), LPARAM(0));
        }
    }
}

/// On the clipboard window: put the [`PROMISED`] files on the clipboard.
unsafe fn offer_promised(hwnd: HWND, transfer: u32) {
    use windows::Win32::System::Ole::OleSetClipboard;
    let Some(files) = PROMISED.lock().unwrap().take().filter(|f| f.transfer == transfer) else { return };
    if files.arrival.state.lock().unwrap().failed {
        return;
    }
    if running_as_system() && !(crate::host_ab::ab("comopen", true) && open_com_to_the_user()) {
        promise_hdrop(hwnd, files);
        return;
    }
    let object: IDataObject = FilesObject(Arc::new(files)).into();
    let mut ours = OUR_SEQ.lock().unwrap();
    let mut tries = 0;
    while let Err(e) = OleSetClipboard(&object) {
        tries += 1;
        if tries == 100 {
            tracing::warn!(error = %e, "couldn't offer copied files on the clipboard");
            #[cfg(test)]
            eprintln!("couldn't offer copied files on the clipboard: {e}");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    *ours = GetClipboardSequenceNumber();
    tracing::info!("copied files offered on the clipboard: a paste streams them as they arrive");
}

/// As SYSTEM (the direct-mode agent): let the signed-in user's processes
/// call into this process's COM objects — Explorer, pasting, reads the
/// copied files' streams from ours. By default COM lets a SYSTEM process be
/// called by SYSTEM and administrators only, so a paste had to wait for every
/// file, blocked with no progress (a 685 MB upload: 2.5 minutes of a frozen
/// Explorer). Local calls by SYSTEM, administrators and the interactive
/// user, at medium integrity or above. Once per process (COM's security is
/// set once, before anything is marshalled); false if it couldn't be.
fn open_com_to_the_user() -> bool {
    static OPEN: OnceLock<bool> = OnceLock::new();
    *OPEN.get_or_init(|| unsafe {
        use windows::Win32::System::Com::{CoInitializeSecurity, EOAC_NONE, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IDENTIFY};
        let Some(sd) = com_access_for_the_user() else {
            tracing::warn!("couldn't build the clipboard's COM access");
            return false;
        };
        match CoInitializeSecurity(Some(sd), -1, None, None, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IDENTIFY, None, EOAC_NONE, None) {
            Ok(()) => {
                tracing::info!("the signed-in user may read copied files from this SYSTEM process: pastes stream");
                true
            }
            Err(e) => {
                tracing::info!(error = %e, "COM access already set: pastes of copied files wait for them");
                false
            }
        }
    })
}

/// [`open_com_to_the_user`] first thing in the agent's process: COM's
/// security is set once, before any interface is marshalled — set from the
/// clipboard's thread, it came too late ("Security must be initialized
/// before any interfaces are marshalled"). On a thread of its own in the
/// process's multithreaded apartment, which it keeps alive (COM's settings
/// last while COM is loaded).
pub(crate) fn open_com_early() {
    if !running_as_system() || !crate::host_ab::ab("comopen", true) {
        return;
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("com-access".into()).spawn(move || unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = done_tx.send(open_com_to_the_user());
        loop {
            std::thread::park();
        }
    });
    if spawned.is_ok() {
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
    }
}

/// [`open_com_to_the_user`]'s access, as the absolute security descriptor
/// COM takes (SDDL gives a self-relative one, which it refuses: "not in the
/// right format"). Kept for the process's life.
unsafe fn com_access_for_the_user() -> Option<windows::Win32::Security::PSECURITY_DESCRIPTOR> {
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{IsValidSecurityDescriptor, MakeAbsoluteSD, ACL, PSECURITY_DESCRIPTOR, PSID};
    let mut rel = PSECURITY_DESCRIPTOR::default();
    // (COM_RIGHTS_EXECUTE | EXECUTE_LOCAL.)
    let sddl = w!("O:SYG:SYD:(A;;0x3;;;SY)(A;;0x3;;;BA)(A;;0x3;;;IU)S:(ML;;NX;;;ME)");
    ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &mut rel, None).ok()?;
    // Generous buffers, u64s for alignment.
    let buf = |n: usize| Box::leak(vec![0u64; n / 8].into_boxed_slice()).as_mut_ptr().cast::<core::ffi::c_void>();
    let (abs, dacl, sacl, owner, group) = (buf(256), buf(1024), buf(1024), buf(256), buf(256));
    let (mut abs_n, mut dacl_n, mut sacl_n, mut owner_n, mut group_n) = (256u32, 1024u32, 1024u32, 256u32, 256u32);
    MakeAbsoluteSD(
        rel,
        Some(PSECURITY_DESCRIPTOR(abs)),
        &mut abs_n,
        Some(dacl.cast::<ACL>()),
        &mut dacl_n,
        Some(sacl.cast::<ACL>()),
        &mut sacl_n,
        Some(PSID(owner)),
        &mut owner_n,
        Some(PSID(group)),
        &mut group_n,
    )
    .ok()?;
    let sd = PSECURITY_DESCRIPTOR(abs);
    IsValidSecurityDescriptor(sd).as_bool().then_some(sd)
}

/// As SYSTEM (the direct-mode agent), the files are promised as a
/// delayed-rendered `CF_HDROP` instead, handed over once they're all in:
/// the signed-in user's Explorer may not call into a SYSTEM process.
static HDROP_PROMISED: Mutex<Option<Arc<PromisedFiles>>> = Mutex::new(None);

unsafe fn promise_hdrop(hwnd: HWND, files: PromisedFiles) {
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    let mut ours = OUR_SEQ.lock().unwrap();
    let mut tries = 0;
    while OpenClipboard(Some(hwnd)).is_err() {
        tries += 1;
        if tries == 100 {
            tracing::warn!("couldn't open the clipboard to promise copied files");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    *HDROP_PROMISED.lock().unwrap() = Some(Arc::new(files));
    let _ = EmptyClipboard();
    let _ = SetClipboardData(CF_HDROP, None);
    let _ = CloseClipboard();
    *ours = GetClipboardSequenceNumber();
    tracing::info!("copied files promised on the clipboard: a paste waits for them");
}

/// Hand promised files to a paste (the clipboard is open for us).
unsafe fn render_hdrop(paste: &[u8]) {
    use windows::Win32::System::DataExchange::SetClipboardData;
    if let Some(h) = global_block(paste) {
        if SetClipboardData(CF_HDROP, Some(HANDLE(h.0))).is_err() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(h));
        }
    }
}

impl PromisedFiles {
    /// The finished copy's paste (as [`finished`] makes it).
    fn hdrop(&self) -> Vec<u8> {
        let roots: Vec<PathBuf> =
            self.entries.iter().filter(|e| !e.name.contains(['\\', '/'])).filter_map(|e| safe_join(&self.dir, &e.name)).collect();
        hdrop_bytes(&roots)
    }
}

/// Copied files on their way, as the clipboard offers them (see
/// [`PromisedFiles`]).
#[windows::core::implement(IDataObject)]
struct FilesObject(Arc<PromisedFiles>);

/// `FileGroupDescriptorW`, `FileContents` and `Preferred DropEffect`.
fn virtual_formats() -> (u16, u16, u16) {
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::UI::Shell::{CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT};
    unsafe {
        (
            RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16,
            RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
            RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16,
        )
    }
}

impl FilesObject {
    /// Entry `lindex` of the copy, if it's a file `format` offers.
    fn offers(&self, format: &FORMATETC) -> windows::core::Result<Option<usize>> {
        use windows::Win32::Foundation::{DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED};
        let (descriptor, contents, effect) = virtual_formats();
        let wants = |tymed: TYMED| if format.tymed & tymed.0 as u32 != 0 { Ok(()) } else { Err(windows::core::Error::from(DV_E_TYMED)) };
        if format.cfFormat == descriptor || format.cfFormat == effect {
            wants(TYMED_HGLOBAL)?;
            return Ok(None);
        }
        if format.cfFormat == contents {
            // (Listed as an HGLOBAL too, as the OLE clipboard only hands on
            // formats Win32's clipboard can hold; read as a stream.)
            wants(TYMED_ISTREAM)?;
            let file = usize::try_from(format.lindex).ok().filter(|&i| self.0.entries.get(i).is_some_and(|e| !e.is_dir));
            return file.map(Some).ok_or(DV_E_LINDEX.into());
        }
        Err(DV_E_FORMATETC.into())
    }

    /// The `FILEGROUPDESCRIPTORW`: every entry, with its size, as Explorer
    /// lays the paste out.
    fn descriptor(&self) -> Vec<u8> {
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
        use windows::Win32::UI::Shell::{FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW};
        let entries = &self.0.entries;
        let mut out = (entries.len() as u32).to_le_bytes().to_vec();
        for e in entries {
            let mut name = [0u16; 260];
            for (d, c) in name.iter_mut().zip(e.name.replace('/', "\\").encode_utf16()) {
                *d = c;
            }
            let d = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
                dwFileAttributes: if e.is_dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
                nFileSizeHigh: (e.size >> 32) as u32,
                nFileSizeLow: e.size as u32,
                cFileName: name,
                ..Default::default()
            };
            out.extend_from_slice(unsafe {
                std::slice::from_raw_parts(&d as *const FILEDESCRIPTORW as *const u8, std::mem::size_of::<FILEDESCRIPTORW>())
            });
        }
        out
    }
}

fn hglobal_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    let h = unsafe { global_block(bytes) }.ok_or(windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
    Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: std::mem::ManuallyDrop::new(None) })
}

impl IDataObject_Impl for FilesObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
        match self.offers(format)? {
            Some(index) => {
                let stream: IStream = FileStream { files: self.0.clone(), index, at: Mutex::new((0, None)) }.into();
                Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                })
            }
            None if format.cfFormat == virtual_formats().0 => hglobal_medium(&self.descriptor()),
            // Preferred DropEffect: copy.
            None => hglobal_medium(&1u32.to_le_bytes()),
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn QueryGetData(&self, format: *const FORMATETC) -> windows::core::HRESULT {
        match unsafe { format.as_ref() }.map(|f| self.offers(f)) {
            Some(Ok(_)) => windows::Win32::Foundation::S_OK,
            Some(Err(e)) => e.code(),
            None => windows::Win32::Foundation::E_POINTER,
        }
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> windows::core::HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        windows::Win32::Foundation::DATA_S_SAMEFORMATETC
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: windows::core::BOOL) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(windows::Win32::Foundation::E_NOTIMPL.into());
        }
        let (descriptor, contents, effect) = virtual_formats();
        let format = |cf: u16, tymed: TYMED| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed.0 as u32,
        };
        unsafe {
            windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc(&[
                format(descriptor, TYMED_HGLOBAL),
                format(contents, TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0)),
                format(effect, TYMED_HGLOBAL),
            ])
        }
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: windows::core::Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// One file of a [`FilesObject`], read as it arrives: a read waits for its
/// bytes.
#[windows::core::implement(IStream)]
struct FileStream {
    files: Arc<PromisedFiles>,
    index: usize,
    /// Where reading is, and the staged file once opened.
    at: Mutex<(u64, Option<std::fs::File>)>,
}

impl FileStream {
    fn size(&self) -> u64 {
        self.files.entries[self.index].size
    }
}

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, read: *mut u32) -> windows::core::HRESULT {
        use windows::Win32::Foundation::{S_FALSE, S_OK, STG_E_INVALIDPOINTER, STG_E_READFAULT};
        if pv.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        let mut at = self.at.lock().unwrap();
        let want = (cb as u64).min(self.size().saturating_sub(at.0));
        let mut done = 0usize;
        // The bytes asked for, once they're in (a short read reads as the end).
        if want > 0 && self.files.arrival.wait_for(self.index, at.0 + want) {
            let opened = match at.1.take() {
                Some(f) => Ok(f),
                None => safe_join(&self.files.dir, &self.files.entries[self.index].name)
                    .ok_or(std::io::Error::other("bad name"))
                    .and_then(std::fs::File::open),
            };
            let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, want as usize) };
            let pos = at.0;
            let r = opened.and_then(|mut f| {
                f.seek(SeekFrom::Start(pos))?;
                f.read_exact(buf)?;
                Ok(f)
            });
            match r {
                Ok(f) => {
                    at.1 = Some(f);
                    done = want as usize;
                }
                Err(e) => tracing::warn!(error = %e, "a paste couldn't read a copied file"),
            }
        }
        at.0 += done as u64;
        if let Some(read) = unsafe { read.as_mut() } {
            *read = done as u32;
        }
        match (done, want) {
            (_, 0) => S_FALSE,
            (0, _) => STG_E_READFAULT,
            _ => S_OK,
        }
    }
    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> windows::core::HRESULT {
        windows::Win32::Foundation::STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, by: i64, origin: STREAM_SEEK, new: *mut u64) -> windows::core::Result<()> {
        let mut at = self.at.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => at.0 as i64,
            STREAM_SEEK_END => self.size() as i64,
            _ => return Err(windows::Win32::Foundation::STG_E_INVALIDFUNCTION.into()),
        };
        let to = u64::try_from(base.saturating_add(by)).map_err(|_| windows::core::Error::from(windows::Win32::Foundation::STG_E_INVALIDFUNCTION))?;
        at.0 = to;
        if let Some(new) = unsafe { new.as_mut() } {
            *new = to;
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(&self, _: windows::core::Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, out: *mut STATSTG, flag: &STATFLAG) -> windows::core::Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(windows::core::Error::from(windows::Win32::Foundation::STG_E_INVALIDPOINTER))?;
        let mut name = windows::core::PWSTR::null();
        if *flag != STATFLAG_NONAME {
            let leaf = self.files.entries[self.index].name.rsplit(['\\', '/']).next().unwrap_or_default();
            let wide: Vec<u16> = leaf.encode_utf16().chain(Some(0)).collect();
            unsafe {
                let p = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    name = windows::core::PWSTR(p);
                }
            }
        }
        *out = STATSTG { pwcsName: name, r#type: STGTY_STREAM.0 as u32, cbSize: self.size(), ..Default::default() };
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_PROMISE_FILES {
        offer_promised(hwnd, wparam.0 as u32);
        return LRESULT(0);
    }
    if msg == WM_RENDERFORMAT && wparam.0 as u32 == CF_HDROP {
        // A paste of files promised as SYSTEM: handed over once they're in.
        let files = HDROP_PROMISED.lock().unwrap().clone();
        if let Some(files) = files.filter(|f| f.arrival.wait_all(&f.entries)) {
            render_hdrop(&files.hdrop());
        }
        return LRESULT(0);
    }
    if msg == WM_RENDERALLFORMATS {
        // Going away with a promise on the clipboard: kept only if the files
        // are in (no waiting: the window is closing).
        use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
        let files = HDROP_PROMISED.lock().unwrap().take();
        if let Some(files) = files.filter(|f| f.arrival.is_complete(&f.entries)) {
            if OpenClipboard(Some(hwnd)).is_ok() {
                render_hdrop(&files.hdrop());
                let _ = CloseClipboard();
            }
        }
        return LRESULT(0);
    }
    if msg == WM_CLIPBOARDUPDATE {
        if let Some(seq) = local_change() {
            let link = LINK.lock().unwrap().clone();
            if let Some(link) = link {
                if IsClipboardFormatAvailable(CF_HDROP).is_ok() {
                    send_files(link, seq);
                } else if let Some(m) = read_clipboard() {
                    // Unless a paste from the peer landed meanwhile — then
                    // what we read is that, not the copy judged above.
                    if GetClipboardSequenceNumber() == seq {
                        link.send(&m);
                    }
                } else if running_as_system() {
                    // Nothing we can read: as SYSTEM, that's how a copy of
                    // files looks (see `paths_from_user`). The user's helper
                    // says whether it was one.
                    send_files(link, seq);
                }
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// The clipboard's sequence number if its contents aren't our last write —
/// a local copy, to send. Judged under the lock a write holds until it has
/// recorded its number.
fn local_change() -> Option<u32> {
    let ours = OUR_SEQ.lock().unwrap();
    let seq = unsafe { GetClipboardSequenceNumber() };
    (seq != *ours).then_some(seq)
}

fn read_clipboard() -> Option<Message> {
    unsafe {
        if let Some(bytes) = get_clipboard_bytes(CF_UNICODETEXT) {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
            if units.len() * 2 <= MAX_BYTES {
                return Some(Message::ClipboardText(String::from_utf16_lossy(&units)));
            }
        }
        get_clipboard_bytes(CF_DIB).filter(|d| d.len() >= 40 && d.len() <= MAX_BYTES).map(Message::ClipboardImage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCWSTR;

    #[test]
    fn copied_names_stay_inside_the_paste_folder() {
        let base = Path::new(r"C:\tmp\paste");
        assert_eq!(safe_join(base, r"dir\a.txt"), Some(base.join("dir").join("a.txt")));
        assert_eq!(safe_join(base, "a/b"), Some(base.join("a").join("b")));
        assert_eq!(safe_join(base, r"..\evil.exe"), None);
        assert_eq!(safe_join(base, r"dir\..\..\evil"), None);
        assert_eq!(safe_join(base, r"C:\Windows\x"), None);
        assert_eq!(safe_join(base, ""), None);
        // Names Windows would quietly change, or open as a device.
        for name in [r"dir\.. \evil", "a.", "a ", "...", r"x\NUL", "con.txt", "COM1", "lpt9.log", "COM¹", "file.txt:stream"] {
            assert_eq!(safe_join(base, name), None, "{name}");
        }
        for name in ["console.txt", "COM10", "comma", "nul_file", ".gitignore", "a.b.c"] {
            assert!(safe_join(base, name).is_some(), "{name}");
        }
    }

    fn entry(name: &str, size: u64, is_dir: bool) -> FileEntry {
        FileEntry { name: name.into(), size, is_dir }
    }

    #[test]
    fn inbound_copies_are_held_to_the_senders_limits() {
        assert_eq!(check_entries(&[entry("a", 5, false), entry("d", 0, true), entry(r"d\b", 7, false)]), Ok(12));
        assert!(check_entries(&vec![entry("f", 1, false); MAX_FILES]).is_ok(), "a walk stopped at its cap");
        assert!(check_entries(&vec![entry("f", 1, false); MAX_FILES + 1]).is_err(), "entry count");
        assert!(check_entries(&[entry("a", MAX_FILES_BYTES, false), entry("b", 1, false)]).is_err(), "total size");
        assert!(check_entries(&[entry("a", u64::MAX, false), entry("b", u64::MAX, false)]).is_err(), "sizes summing past u64");
        assert!(check_entries(&[entry(&"x".repeat(MAX_NAME + 1), 1, false)]).is_err(), "name length");
        assert!(check_entries(&[entry(&vec!["d"; MAX_DEPTH + 1].join("\\"), 1, false)]).is_err(), "nesting");
        assert!(check_entries(&[entry(r"..\x", 1, false)]).is_err(), "escape");
    }

    /// A paste folder of the test's own (the real one is the user's rdpio's).
    fn test_user(name: &str) -> SessionUser {
        SessionUser { token: None, root: std::env::temp_dir().join(format!("rdpio-paste-test-{name}-{}", std::process::id())) }
    }

    #[test]
    fn a_copy_is_written_within_its_declared_sizes() {
        let user = test_user("ok");
        let root = user.root.clone();
        let mut slot = None;
        let entries = vec![entry("a.txt", 5, false), entry("d", 0, true), entry(r"d\b.bin", 3, false), entry("empty", 0, false)];
        assert!(begin_files(&mut slot, user, 7, entries).is_none(), "data still to come");
        let dir = root.join("7");
        assert!(dir.join("empty").is_file() && dir.join("d").is_dir());
        assert!(file_chunk(&mut slot, 7, 0, 0, b"hel").is_none());
        assert!(file_chunk(&mut slot, 7, 0, 3, b"lo").is_none());
        assert!(file_chunk(&mut slot, 7, 0, 0, b"HELLO").is_none(), "a finished file isn't written again");
        assert!(file_chunk(&mut slot, 7, 3, 0, b"").is_none() && slot.is_some(), "an empty file has nothing to finish");
        let hdrop = file_chunk(&mut slot, 7, 2, 0, b"xyz").expect("the paste");
        assert!(slot.is_none());
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("d").join("b.bin")).unwrap(), b"xyz");
        // The top-level entries are what's offered: a.txt, d and empty.
        let listed: Vec<u16> = hdrop[20..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&listed).split('\0').filter(|s| !s.is_empty()).count(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_outside_a_declared_file_drops_the_whole_copy() {
        let past_the_end = [(0u32, 3u64, &b"xyz"[..]), (0, u64::MAX, &b"x"[..]), (1, 0, &b"x"[..]), (2, 0, &b"x"[..])];
        for (index, offset, data) in past_the_end {
            let user = test_user("overrun");
            let root = user.root.clone();
            let mut slot = None;
            begin_files(&mut slot, user, 1, vec![entry("a.txt", 5, false), entry("d", 0, true), entry("empty", 0, false)]);
            assert!(file_chunk(&mut slot, 1, 0, 0, b"he").is_none());
            assert!(file_chunk(&mut slot, 1, index, offset, data).is_none());
            assert!(slot.is_none(), "{index}@{offset} should drop the copy");
            assert!(!root.join("1").exists(), "nothing half-written stays behind");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_refused_copy_writes_nothing_and_keeps_the_last_paste() {
        let user = test_user("refused");
        let root = user.root.clone();
        std::fs::create_dir_all(root.join("1")).unwrap();
        std::fs::write(root.join("1").join("kept.txt"), b"the last paste").unwrap();
        let mut slot = None;
        assert!(begin_files(&mut slot, user, 2, vec![entry("fine.txt", 1, false), entry(r"..\escape", 1, false)]).is_none());
        assert!(slot.is_none());
        assert!(root.join("1").join("kept.txt").is_file());
        assert!(!root.join("2").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Run `body` in a child test process on a private window station: the
    /// clipboard is per window station, so the user's own is never touched.
    /// `body` gets the desktop, for threads of its own to join.
    fn on_private_clipboard(test: &str, body: fn(isize)) {
        const CHILD: &str = "RDPIO_TEST_PRIVATE_CLIPBOARD";
        if std::env::var_os(CHILD).is_none() {
            // One at a time: an unnamed window station is named for the
            // logon session, so children running at once share its clipboard.
            static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
            let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test, "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && log.contains("1 passed"), "{log}");
            return;
        }
        use windows::Win32::Foundation::GENERIC_ALL;
        use windows::Win32::System::StationsAndDesktops::*;
        /// WINSTA_ALL_ACCESS.
        const ALL_ACCESS: u32 = 0x37F;
        unsafe {
            let station = CreateWindowStationW(PCWSTR::null(), 0, ALL_ACCESS, None).expect("a private window station");
            SetProcessWindowStation(station).expect("switch to it");
            let desktop = CreateDesktopW(w!("rdpio-test"), PCWSTR::null(), None, DESKTOP_CONTROL_FLAGS(0), GENERIC_ALL.0, None)
                .expect("a desktop on it");
            let raw = desktop.0 as isize;
            // A fresh thread: one that has made no window calls can join it.
            std::thread::spawn(move || {
                SetThreadDesktop(HDESK(raw as *mut _)).expect("join the desktop");
                body(raw);
            })
            .join()
            .unwrap();
        }
    }

    /// Copied files are offered as soon as their list comes, and a paste
    /// (through OLE, as Explorer's) reads each file as it arrives, then gets
    /// the finished copy's `CF_HDROP`.
    #[test]
    fn a_paste_streams_files_as_they_arrive() {
        // (As SYSTEM, as CI may run tests, files are promised as a CF_HDROP
        // instead: see the next test.)
        if running_as_system() {
            println!("skipped: virtual files are offered only when not SYSTEM");
            return;
        }
        on_private_clipboard("hyper_clipboard::tests::a_paste_streams_files_as_they_arrive", |desktop| {
            use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize};
            use windows::Win32::System::StationsAndDesktops::{SetThreadDesktop, HDESK};
            // The clipboard window, an OLE apartment on a thread of its own as in `start`.
            let window = std::thread::spawn(move || unsafe {
                SetThreadDesktop(HDESK(desktop as *mut _)).unwrap();
                OleInitialize(None).unwrap();
                let class = w!("rdpioHyperClipboardTest");
                RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() });
                let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None).unwrap();
                HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
                withdraw_offer();
            });
            while HWND_RAW.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            let user = test_user("stream");
            let root = user.root.clone();
            let mut slot = None;
            let entries = vec![entry("d", 0, true), entry(r"d\a.txt", 5, false), entry("b.bin", 3, false)];
            assert!(begin_files(&mut slot, user, 9, entries).is_none(), "data still to come");
            promise_files(slot.as_ref().unwrap());
            let (descriptor, contents, _) = virtual_formats();
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { IsClipboardFormatAvailable(descriptor as u32) }.is_err() {
                let owner = unsafe { windows::Win32::System::DataExchange::GetClipboardOwner() };
                assert!(Instant::now() < deadline, "offered on the clipboard (owner {owner:?})");
                std::thread::sleep(Duration::from_millis(10));
            }
            // Someone pastes: the list at once, then each file as it comes.
            let (started_tx, started) = std::sync::mpsc::channel();
            let paster = std::thread::spawn(move || unsafe {
                SetThreadDesktop(HDESK(desktop as *mut _)).unwrap();
                OleInitialize(None).unwrap();
                let object = OleGetClipboard().unwrap();
                let format = |cf: u16, tymed: TYMED, lindex: i32| FORMATETC {
                    cfFormat: cf,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex,
                    tymed: tymed.0 as u32,
                };
                let mut list = object.GetData(&format(descriptor, TYMED_HGLOBAL, -1)).unwrap();
                let g = list.u.hGlobal;
                let bytes = std::slice::from_raw_parts(windows::Win32::System::Memory::GlobalLock(g) as *const u8, 4).to_vec();
                let _ = windows::Win32::System::Memory::GlobalUnlock(g);
                windows::Win32::System::Ole::ReleaseStgMedium(&mut list);
                assert_eq!(u32::from_le_bytes(bytes.try_into().unwrap()), 3, "every entry listed");
                started_tx.send(()).unwrap();
                let t = Instant::now();
                let read = |lindex: i32| {
                    let medium = object.GetData(&format(contents, TYMED_ISTREAM, lindex)).unwrap();
                    let stream = (*medium.u.pstm).clone().unwrap();
                    let mut got = Vec::new();
                    let mut buf = [0u8; 2];
                    loop {
                        let mut n = 0u32;
                        let hr = stream.Read(buf.as_mut_ptr() as *mut _, buf.len() as u32, Some(&mut n));
                        assert!(hr.is_ok(), "read {hr:?}");
                        got.extend_from_slice(&buf[..n as usize]);
                        if n == 0 {
                            break;
                        }
                    }
                    got
                };
                let a = read(1);
                let b = read(2);
                (t.elapsed(), a, b)
            });
            started.recv().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            // …which come in a while later.
            assert!(file_chunk(&mut slot, 9, 1, 0, b"hel").is_none());
            std::thread::sleep(Duration::from_millis(100));
            assert!(file_chunk(&mut slot, 9, 1, 3, b"lo").is_none());
            let hdrop = file_chunk(&mut slot, 9, 2, 0, b"xyz").expect("the paste");
            let (waited, a, b) = paster.join().unwrap();
            assert!(waited >= Duration::from_millis(250), "the paste waited ({waited:?})");
            assert_eq!((a.as_slice(), b.as_slice()), (&b"hello"[..], &b"xyz"[..]), "and read each file whole");
            // The finished copy then replaces the offer.
            write_clipboard(&[(CF_HDROP, hdrop.clone())]);
            assert!(unsafe { get_clipboard_bytes(CF_HDROP) }.is_some_and(|now| now.starts_with(&hdrop)));
            unsafe {
                let _ = PostMessageW(Some(HWND(HWND_RAW.load(Ordering::SeqCst) as *mut _)), WM_QUIT, WPARAM(0), LPARAM(0));
            }
            window.join().unwrap();
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// By hand, on the real desktop: offers `dig.bin` (3 MB) arriving over
    /// ~3 s, for a shell paste meanwhile (`RDPIO_SHELL_TEST` names the file
    /// the bytes go in, for checking).
    #[test]
    #[ignore]
    fn manual_shell_paste_of_arriving_files() {
        use windows::Win32::System::Ole::OleInitialize;
        let window = std::thread::spawn(|| unsafe {
            OleInitialize(None).unwrap();
            let class = w!("rdpioHyperClipboardShellTest");
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() });
            let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None).unwrap();
            HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
            withdraw_offer();
        });
        while HWND_RAW.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let user = test_user("shell");
        let mut slot = None;
        const SIZE: usize = 3 * 1024 * 1024;
        let flat = std::env::var_os("RDPIO_SHELL_FLAT").is_some();
        let entries = if flat { vec![entry("big.bin", SIZE as u64, false)] } else { vec![entry("d", 0, true), entry(r"d\big.bin", SIZE as u64, false)] };
        let file = entries.len() as u32 - 1;
        assert!(begin_files(&mut slot, user, 77, entries).is_none());
        promise_files(slot.as_ref().unwrap());
        let data: Vec<u8> = (0..SIZE).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(std::env::var("RDPIO_SHELL_TEST").unwrap(), &data).unwrap();
        println!("OFFERED");
        std::thread::sleep(Duration::from_secs(4));
        for (n, c) in data.chunks(256 * 1024).enumerate() {
            let _ = file_chunk(&mut slot, 77, file, (n * 256 * 1024) as u64, c);
            std::thread::sleep(Duration::from_millis(250));
        }
        println!("ALL IN");
        std::thread::sleep(Duration::from_secs(15));
        unsafe {
            let _ = PostMessageW(Some(HWND(HWND_RAW.load(Ordering::SeqCst) as *mut _)), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        window.join().unwrap();
    }

    /// By hand, in another process than the offer: read the offered file
    /// (OLE clipboard → FileContents stream) into `RDPIO_SHELL_GOT`.
    #[test]
    #[ignore]
    fn manual_read_offered_file_from_another_process() {
        use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize};
        unsafe {
            OleInitialize(None).unwrap();
            let object = OleGetClipboard().unwrap();
            let (_, contents, _) = virtual_formats();
            let index = std::env::var("RDPIO_SHELL_INDEX").map_or(1, |v| v.parse().unwrap());
            let t = Instant::now();
            let medium = object
                .GetData(&FORMATETC {
                    cfFormat: contents,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: index,
                    tymed: (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
                })
                .unwrap();
            let stream = (*medium.u.pstm).clone().unwrap();
            let mut got = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut n = 0u32;
                let hr = stream.Read(buf.as_mut_ptr() as *mut _, buf.len() as u32, Some(&mut n));
                assert!(hr.is_ok(), "read {hr:?}");
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n as usize]);
            }
            std::fs::write(std::env::var("RDPIO_SHELL_GOT").unwrap(), &got).unwrap();
            println!("READ {} bytes in {:?}", got.len(), t.elapsed());
        }
    }

    /// As SYSTEM: the files are promised as a `CF_HDROP`, and a paste waits
    /// for the last byte, getting the finished copy.
    /// COM takes the access the SYSTEM agent opens to the user (run alone:
    /// COM's security is set once per process).
    /// `cargo test -p rdp-client com_takes_the_users_access -- --ignored`
    #[test]
    #[ignore]
    fn com_takes_the_users_access() {
        unsafe {
            let _ = windows::Win32::System::Ole::OleInitialize(None);
        }
        assert!(open_com_to_the_user());
    }

    #[test]
    fn com_access_for_the_user_is_absolute() {
        let sd = unsafe { com_access_for_the_user() }.expect("built");
        let mut control = 0u16;
        let mut rev = 0u32;
        unsafe { windows::Win32::Security::GetSecurityDescriptorControl(sd, &mut control, &mut rev) }.unwrap();
        assert_eq!(control & 0x8000, 0, "self-relative");
    }

    #[test]
    fn a_paste_of_files_promised_as_system_waits_for_them() {
        on_private_clipboard("hyper_clipboard::tests::a_paste_of_files_promised_as_system_waits_for_them", |desktop| {
            use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
            use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
            use windows::Win32::System::StationsAndDesktops::{SetThreadDesktop, HDESK};
            let window = std::thread::spawn(move || unsafe {
                SetThreadDesktop(HDESK(desktop as *mut _)).unwrap();
                let class = w!("rdpioHyperClipboardTestSystem");
                RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() });
                let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None).unwrap();
                HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
            });
            while HWND_RAW.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            let hwnd = HWND(HWND_RAW.load(Ordering::SeqCst) as *mut _);
            let user = test_user("system");
            let root = user.root.clone();
            let mut slot = None;
            assert!(begin_files(&mut slot, user, 9, vec![entry("a.txt", 5, false)]).is_none(), "data still to come");
            let inc = slot.as_ref().unwrap();
            let files = PromisedFiles { transfer: 9, dir: inc.dir.clone(), entries: inc.entries.clone(), arrival: inc.arrival.clone() };
            unsafe { promise_hdrop(hwnd, files) };
            assert!(unsafe { IsClipboardFormatAvailable(CF_HDROP) }.is_ok(), "promised on the clipboard");
            let paster = std::thread::spawn(move || unsafe {
                SetThreadDesktop(HDESK(desktop as *mut _)).unwrap();
                let t = Instant::now();
                OpenClipboard(None).unwrap();
                let h = GetClipboardData(CF_HDROP).expect("the files, once in");
                let g = windows::Win32::Foundation::HGLOBAL(h.0);
                let bytes = std::slice::from_raw_parts(GlobalLock(g) as *const u8, GlobalSize(g)).to_vec();
                let _ = GlobalUnlock(g);
                let _ = CloseClipboard();
                (t.elapsed(), bytes)
            });
            std::thread::sleep(Duration::from_millis(300));
            assert!(file_chunk(&mut slot, 9, 0, 0, b"hel").is_none());
            let hdrop = file_chunk(&mut slot, 9, 0, 3, b"lo").expect("the paste");
            let (waited, pasted) = paster.join().unwrap();
            assert!(waited >= Duration::from_millis(250), "the paste waited ({waited:?})");
            assert!(pasted.starts_with(&hdrop), "and got the finished copy");
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_QUIT, WPARAM(0), LPARAM(0));
            }
            window.join().unwrap();
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// A copy given up on fails a paste reading it (it doesn't hang).
    #[test]
    fn a_paste_of_an_abandoned_copy_fails() {
        let user = test_user("abandon");
        let root = user.root.clone();
        let mut slot = None;
        assert!(begin_files(&mut slot, user, 4, vec![entry("a", 10, false)]).is_none());
        let arrival = slot.as_ref().unwrap().arrival.clone();
        let waiter = std::thread::spawn(move || arrival.wait_for(0, 10));
        std::thread::sleep(Duration::from_millis(100));
        abandon(&mut slot);
        assert!(!waiter.join().unwrap(), "the read gives up");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_paste_from_the_peer_is_not_sent_back_but_a_local_copy_is() {
        on_private_clipboard("hyper_clipboard::tests::a_paste_from_the_peer_is_not_sent_back_but_a_local_copy_is", |desktop| {
            use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
            use windows::Win32::System::StationsAndDesktops::{SetThreadDesktop, HDESK};
            let text = |s: &str| (CF_UNICODETEXT, s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
            // Closing the clipboard after our write moves the number on
            // (Windows adds CF_TEXT, CF_LOCALE…): still recognised as ours.
            write_clipboard(&[text("from the peer")]);
            assert_eq!(local_change(), None);
            // Any other writer is a local copy, to send.
            unsafe { set_clipboard_formats(&[text("copied here")]) };
            let local = local_change().expect("a local copy");
            assert_eq!(local_change(), Some(local), "judging it doesn't use it up");
            // While another app holds the clipboard our write can't happen;
            // the local copy still there isn't taken for ours.
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || unsafe {
                SetThreadDesktop(HDESK(desktop as *mut _)).unwrap();
                // Held by a window, as apps do (two window-less opens don't
                // exclude each other).
                let window = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
                .unwrap();
                OpenClipboard(Some(window)).unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
                CloseClipboard().unwrap();
                let _ = DestroyWindow(window);
            });
            held_rx.recv().unwrap();
            write_clipboard(&[text("from the peer, again")]);
            release_tx.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(local_change(), Some(local));
            // An image from the peer.
            let mut dib = vec![0u8; 44];
            (dib[0], dib[4], dib[8], dib[12], dib[14]) = (40, 1, 1, 1, 32);
            write_clipboard(&[(CF_DIB, dib)]);
            assert_eq!(local_change(), None);
        });
    }
}
