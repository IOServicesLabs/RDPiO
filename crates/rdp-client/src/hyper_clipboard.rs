//! Clipboard sync for Hyper (both ends): a hidden message-only window listens
//! for clipboard changes (`AddClipboardFormatListener`) and reports text,
//! images and copied files; remote contents are applied with the same format
//! writer the RDP clipboard uses. Our own writes are recognised by the
//! clipboard sequence number, so a paste never echoes back.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rdp_channels::cliprdr::ClipboardProvider;
use rdp_hyper::messages::{FileEntry, Message};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, IsClipboardFormatAvailable, RemoveClipboardFormatListener,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{get_clipboard_bytes, hdrop_bytes, set_clipboard_formats, CF_DIB, CF_HDROP, CF_UNICODETEXT};
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
const MAX_BACKLOG: usize = 4 * 1024 * 1024;
const CHUNK: u32 = 1024 * 1024;

static LINK: Mutex<Option<Arc<ReliableLink>>> = Mutex::new(None);
/// The newest outbound file transfer; older ones stop when it changes.
static TRANSFER: AtomicU32 = AtomicU32::new(0);
/// The clipboard sequence number our last write left. A write holds the lock
/// until its number is recorded, and the listener holds it while it judges a
/// change, so it never sees one of our writes unrecorded (and echoes it).
static OUR_SEQ: Mutex<u32> = Mutex::new(0);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);

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
}

impl ClipboardSync {
    /// Report local clipboard changes over `link`.
    pub fn start(link: Arc<ReliableLink>) -> Option<Self> {
        *LINK.lock().unwrap() = Some(link);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("hyper-clipboard".into())
            .spawn(move || unsafe {
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
            })
            .ok()?;
        if ready_rx.recv().unwrap_or(false) {
            tracing::info!("clipboard sync on");
            let (apply_tx, apply_rx) = std::sync::mpsc::channel::<Message>();
            let applier = std::thread::Builder::new()
                .name("hyper-clipboard-apply".into())
                .spawn(move || {
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
    let inc = Incoming { transfer, dir, user, entries, open: HashMap::new(), received: HashMap::new(), remaining };
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
    let transfer = TRANSFER.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new().name("hyper-files".into()).spawn(move || {
        // Only files the user signed in here could read themselves.
        if let Some(user) = SessionUser::current() {
            user.act(|| send_files_now(&link, transfer, seq));
        }
    });
}

fn send_files_now(link: &ReliableLink, transfer: u32, seq: u32) {
    let mut cb = crate::clipboard::Win32Clipboard::new();
    let files = cb.get_files();
    if files.is_empty() || unsafe { GetClipboardSequenceNumber() } != seq {
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
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir || e.size == 0 {
            continue;
        }
        let mut offset = 0u64;
        while offset < e.size {
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
        }
    }
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

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
