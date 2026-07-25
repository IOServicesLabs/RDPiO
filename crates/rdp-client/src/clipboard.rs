//! Win32 clipboard provider for the cliprdr channel (CF_UNICODETEXT).
//!
//! Implements [`rdp_channels::cliprdr::ClipboardProvider`] over the Win32
//! clipboard: read the local clipboard text to answer the remote's data
//! requests, and write text the remote copied into the local clipboard. Runs on
//! the session worker thread (clipboard APIs work on any thread). Only text is
//! handled; richer formats fall through as "no data".

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use rdp_channels::cliprdr::{ClipFile, ClipboardProvider};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// `CF_UNICODETEXT` clipboard format.
const CF_UNICODETEXT: u32 = 13;
/// `CF_HDROP` — a dropped-file list (for clipboard file copy).
const CF_HDROP: u32 = 15;

/// The Win32 clipboard, exposed as a cliprdr provider. Caches the file paths
/// from the most recent `get_files` so `read_file` can stream them by index,
/// and optionally downloads session-clipboard files to `download_dir`.
pub struct Win32Clipboard {
    files: Vec<PathBuf>,
    download_dir: Option<PathBuf>,
}

impl Win32Clipboard {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            download_dir: None,
        }
    }

    /// Save files copied in the remote session to `dir` (enables remote→local
    /// clipboard file download).
    pub fn with_download_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.download_dir = dir;
        self
    }
}

impl ClipboardProvider for Win32Clipboard {
    fn get_text(&mut self) -> Option<String> {
        unsafe {
            if OpenClipboard(None).is_err() {
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
        unsafe {
            if OpenClipboard(None).is_err() {
                return Vec::new();
            }
            let files = (|| {
                let handle = GetClipboardData(CF_HDROP).ok()?;
                let hdrop = HDROP(handle.0);
                // 0xFFFFFFFF → number of files in the drop.
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
                let mut out = Vec::new();
                self.files.clear();
                for i in 0..count {
                    // Query the path length (chars, excl. NUL), then fetch it.
                    let len = DragQueryFileW(hdrop, i, None) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len + 1];
                    let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                    let path = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into());
                    out.push(ClipFile { name, size });
                    self.files.push(path);
                }
                Some(out)
            })()
            .unwrap_or_default();
            let _ = CloseClipboard();
            files
        }
    }

    fn read_file(&mut self, index: u32, offset: u64, len: u32) -> Option<Vec<u8>> {
        let path = self.files.get(index as usize)?;
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len as usize];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        Some(buf)
    }

    fn wants_remote_files(&self) -> bool {
        self.download_dir.is_some()
    }

    fn save_remote_file(&mut self, name: &str, data: &[u8]) {
        let Some(dir) = &self.download_dir else {
            return;
        };
        // Use only the base name to avoid path traversal from a hostile server.
        let base = std::path::Path::new(name)
            .file_name()
            .map(|s| s.to_owned())
            .unwrap_or_else(|| name.into());
        let path = dir.join(base);
        if let Err(e) = std::fs::write(&path, data) {
            tracing::warn!(error = %e, file = %path.display(), "failed to save clipboard file");
        } else {
            tracing::info!(file = %path.display(), bytes = data.len(), "saved clipboard file from session");
        }
    }

    fn set_text(&mut self, text: &str) {
        unsafe {
            let mut utf16: Vec<u16> = text.encode_utf16().collect();
            utf16.push(0); // NUL terminator
            let bytes = utf16.len() * 2;

            // Don't echo our own write back to the server as a "local change".
            crate::session::suppress_clipboard_echo();
            if OpenClipboard(None).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            // GMEM_MOVEABLE memory whose ownership transfers to the clipboard on
            // a successful SetClipboardData (so it must not be freed after that).
            if let Ok(hglobal) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
                let ptr = GlobalLock(hglobal) as *mut u16;
                if ptr.is_null() {
                    let _ = GlobalFree(Some(hglobal));
                } else {
                    std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
                    let _ = GlobalUnlock(hglobal);
                    if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))).is_err() {
                        // Ownership didn't transfer; reclaim the block.
                        let _ = GlobalFree(Some(hglobal));
                    }
                }
            }
            let _ = CloseClipboard();
        }
    }
}
