//! Clipboard redirection (MS-RDPECLIP) — text sync over the static `cliprdr`
//! virtual channel.
//!
//! The protocol is a small request/response dance around an 8-byte
//! `CLIPRDR_HEADER` { msgType, msgFlags, dataLen }:
//!  - server → `CB_MONITOR_READY`; client replies `CB_CLIP_CAPS` + `CB_FORMAT_LIST`;
//!  - either side announces new clipboard contents with `CB_FORMAT_LIST`, acked by
//!    `CB_FORMAT_LIST_RESPONSE`;
//!  - to paste, the peer sends `CB_FORMAT_DATA_REQUEST(formatId)` and gets back
//!    `CB_FORMAT_DATA_RESPONSE(data)`.
//!
//! This module is sans-I/O and OS-agnostic: [`ClipboardChannel`] turns inbound
//! messages into the outbound messages to send, calling a [`ClipboardProvider`]
//! for the actual local clipboard (read for "what do we have / give me the
//! data", write for "remote copied this"). The session layer frames the output
//! with [`crate::svc`] and ships it on the cliprdr channel; the platform
//! supplies the provider.

/// One file offered to the session via the clipboard (copy local → remote).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipFile {
    /// Entry name as shown when pasting. For an item inside a copied folder
    /// this is the path RELATIVE to the copy root, separated by backslashes
    /// (`"docs\\notes\\a.txt"`) — the layout MS-RDPECLIP requires so the peer
    /// can rebuild the tree.
    pub name: String,
    /// Size in bytes; 0 for a directory.
    pub size: u64,
    /// Whether this entry is a directory (it has no contents to transfer; the
    /// peer just creates it).
    pub is_dir: bool,
}

/// The OS clipboard, abstracted so the protocol stays testable and portable.
pub trait ClipboardProvider {
    /// The current local clipboard text, if any (for advertising and for
    /// answering a data request).
    fn get_text(&mut self) -> Option<String>;
    /// Replace the local clipboard text (a remote copy was pasted to us).
    fn set_text(&mut self, text: &str);
    /// Files currently on the local clipboard, to offer the session. Default:
    /// none (no file transfer).
    fn get_files(&mut self) -> Vec<ClipFile> {
        Vec::new()
    }
    /// Read `len` bytes at `offset` from local file `index` (the index into the
    /// last [`get_files`] list), for answering a File Contents Request. `None`
    /// on any error. Default: none.
    fn read_file(&mut self, _index: u32, _offset: u64, _len: u32) -> Option<Vec<u8>> {
        None
    }
    /// The current local clipboard image as raw `CF_DIB` bytes (`BITMAPINFO`
    /// followed by the pixel data — exactly Win32's `CF_DIB` payload), if any.
    /// Default: none (no image transfer).
    fn get_image(&mut self) -> Option<Vec<u8>> {
        None
    }
    /// Replace the local clipboard image with `CF_DIB` bytes pasted from the
    /// session. Default: discard.
    fn set_image(&mut self, _dib: &[u8]) {}
    /// Apply one complete paste from the session: every format fetched for it,
    /// delivered together. A provider backed by a real OS clipboard MUST
    /// override this to publish all formats in a single update — applying them
    /// one at a time empties the clipboard between writes, so the last format
    /// would be the only survivor (copying text+image would lose the text).
    /// Default: apply each in turn, which suits accumulating/test providers.
    fn set_contents(&mut self, text: Option<&str>, image: Option<&[u8]>) {
        if let Some(t) = text {
            self.set_text(t);
        }
        if let Some(i) = image {
            self.set_image(i);
        }
    }
    /// Whether to pull files the *session* puts on its clipboard down to the
    /// local machine (e.g. a download directory is configured). Default: no.
    fn wants_remote_files(&self) -> bool {
        false
    }
    /// The session announced `files` on its clipboard. Nothing has been
    /// transferred yet — a provider advertises them locally here (on Windows,
    /// as a delayed-render `CF_HDROP`) and the bytes are only fetched if the
    /// user actually pastes. Default: nothing to do.
    fn offer_remote_files(&mut self, _files: &[ClipFile]) {}
    /// Start receiving one entry of a session-clipboard copy. A directory
    /// (`is_dir`) has no contents — just create it.
    fn begin_remote_file(&mut self, _name: &str, _size: u64, _is_dir: bool) {}
    /// One chunk of the current file, in order. Streamed straight to disk: a
    /// multi-gigabyte copy must never be accumulated in memory.
    fn write_remote_chunk(&mut self, _data: &[u8]) {}
    /// The current file is complete.
    fn end_remote_file(&mut self) {}
    /// Every entry of one session-clipboard copy has been received.
    fn finish_remote_files(&mut self) {}
    /// The transfer failed or was abandoned partway; drop any partial state.
    fn abort_remote_files(&mut self) {}
}

/// A clipboard provider that holds nothing and discards writes — the default
/// when no OS clipboard is wired in (headless/tests). Clipboard PDUs are still
/// answered correctly (empty/FAIL), so the channel handshake completes.
pub struct NoopClipboard;
impl ClipboardProvider for NoopClipboard {
    fn get_text(&mut self) -> Option<String> {
        None
    }
    fn set_text(&mut self, _text: &str) {}
}

const CB_MONITOR_READY: u16 = 0x0001;
const CB_FORMAT_LIST: u16 = 0x0002;
const CB_FORMAT_LIST_RESPONSE: u16 = 0x0003;
const CB_FORMAT_DATA_REQUEST: u16 = 0x0004;
const CB_FORMAT_DATA_RESPONSE: u16 = 0x0005;
const CB_CLIP_CAPS: u16 = 0x0007;
const CB_FILECONTENTS_REQUEST: u16 = 0x0008;
const CB_FILECONTENTS_RESPONSE: u16 = 0x0009;

const CB_RESPONSE_OK: u16 = 0x0001;
const CB_RESPONSE_FAIL: u16 = 0x0002;

/// File Contents Request types.
const FILECONTENTS_SIZE: u32 = 0x0001;
const FILECONTENTS_RANGE: u32 = 0x0002;

/// Our assigned format id for the registered "FileGroupDescriptorW" format
/// (any value in the registered-clipboard-format range works; both sides
/// reference it by the id the announcer picks).
const CF_FILEGROUPDESCRIPTORW: u32 = 0x0000_C0C0;
/// Capability flag: this client supports file-list clipboard transfer.
const CB_STREAM_FILECLIP_ENABLED: u32 = 0x0000_0004;
/// FILEDESCRIPTORW flags: the fileName + size fields are valid.
const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_FILESIZE: u32 = 0x0000_0040;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
/// `FILE_ATTRIBUTE_DIRECTORY` — marks a descriptor entry as a folder.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

const CB_CAPSTYPE_GENERAL: u16 = 0x0001;
const CB_CAPS_VERSION_2: u32 = 0x0000_0002;
const CB_USE_LONG_FORMAT_NAMES: u32 = 0x0000_0002;

/// `CF_UNICODETEXT` — UTF-16LE text, the format we sync.
const CF_UNICODETEXT: u32 = 13;
/// `CF_DIB` — a device-independent bitmap (`BITMAPINFO` + pixels), the
/// standard interchange format for clipboard images.
const CF_DIB: u32 = 8;

/// Build a CLIPRDR PDU (`CLIPRDR_HEADER` + data).
fn message(msg_type: u16, msg_flags: u16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.extend_from_slice(&msg_type.to_le_bytes());
    v.extend_from_slice(&msg_flags.to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// Client Clipboard Capabilities: one General set, long format names.
fn capabilities() -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&1u16.to_le_bytes()); // cCapabilitiesSets
    d.extend_from_slice(&0u16.to_le_bytes()); // pad
    d.extend_from_slice(&CB_CAPSTYPE_GENERAL.to_le_bytes());
    d.extend_from_slice(&12u16.to_le_bytes()); // lengthCapability
    d.extend_from_slice(&CB_CAPS_VERSION_2.to_le_bytes());
    // Long format names + file-clip transfer.
    d.extend_from_slice(&(CB_USE_LONG_FORMAT_NAMES | CB_STREAM_FILECLIP_ENABLED).to_le_bytes());
    message(CB_CLIP_CAPS, 0, &d)
}

/// UTF-16LE, NUL-terminated form of a format name.
fn utf16z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    v.extend_from_slice(&[0, 0]);
    v
}

/// A Format List advertising what we hold (long-format-name layout: `formatId`
/// + NUL-terminated UTF-16 name). Empty means "we have nothing".
fn format_list(has_text: bool, has_files: bool, has_image: bool) -> Vec<u8> {
    let mut d = Vec::new();
    if has_text {
        d.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        d.extend_from_slice(&[0, 0]); // empty long-format name (standard format)
    }
    if has_image {
        d.extend_from_slice(&CF_DIB.to_le_bytes());
        d.extend_from_slice(&[0, 0]); // standard format, no name
    }
    if has_files {
        d.extend_from_slice(&CF_FILEGROUPDESCRIPTORW.to_le_bytes());
        d.extend_from_slice(&utf16z("FileGroupDescriptorW"));
    }
    message(CB_FORMAT_LIST, 0, &d)
}

/// Pack a `CLIPRDR_FILELIST` (cItems + that many 592-byte `FILEDESCRIPTORW`).
fn file_group_descriptor(files: &[ClipFile]) -> Vec<u8> {
    let mut d = Vec::with_capacity(4 + files.len() * 592);
    d.extend_from_slice(&(files.len() as u32).to_le_bytes()); // cItems
    for f in files {
        d.extend_from_slice(&(FD_ATTRIBUTES | FD_FILESIZE).to_le_bytes()); // flags
        d.extend_from_slice(&[0u8; 32]); // reserved1
        let attrs = if f.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        };
        d.extend_from_slice(&attrs.to_le_bytes()); // fileAttributes
        d.extend_from_slice(&[0u8; 16]); // reserved2
        d.extend_from_slice(&0u64.to_le_bytes()); // lastWriteTime
        d.extend_from_slice(&((f.size >> 32) as u32).to_le_bytes()); // fileSizeHigh
        d.extend_from_slice(&(f.size as u32).to_le_bytes()); // fileSizeLow
        // fileName: 260 UTF-16 code units (520 bytes), NUL-padded.
        let mut name = [0u16; 260];
        for (i, u) in f.name.encode_utf16().take(259).enumerate() {
            name[i] = u;
        }
        for u in name {
            d.extend_from_slice(&u.to_le_bytes());
        }
    }
    d
}

/// Encode `text` as a Format Data Response payload (UTF-16LE, NUL-terminated).
fn unicode_response(text: &str) -> Vec<u8> {
    let mut d: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    d.extend_from_slice(&[0, 0]); // NUL terminator
    d
}

/// Decode a Format Data Response payload (UTF-16LE, trailing NUL) to a `String`.
fn decode_unicode(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Parse a long-format-name Format List into `(formatId, name)` pairs.
fn parse_format_list(data: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut o = 0;
    while o + 4 <= data.len() {
        let id = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        o += 4;
        // NUL-terminated UTF-16 name.
        let mut units = Vec::new();
        while o + 2 <= data.len() {
            let u = u16::from_le_bytes([data[o], data[o + 1]]);
            o += 2;
            if u == 0 {
                break;
            }
            units.push(u);
        }
        out.push((id, String::from_utf16_lossy(&units)));
    }
    out
}

/// Parse a `CLIPRDR_FILELIST` (cItems + 592-byte FILEDESCRIPTORW each) into
/// entries, carrying the directory flag so a copied folder can be rebuilt.
fn parse_file_descriptors(data: &[u8]) -> Vec<ClipFile> {
    let mut out = Vec::new();
    let Some(count) = data.get(0..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) else {
        return out;
    };
    const REC: usize = 592;
    for i in 0..count as usize {
        let base = 4 + i * REC;
        let Some(rec) = data.get(base..base + REC) else {
            break;
        };
        // attributes at +36, sizeHigh at +64, sizeLow at +68, fileName at +72.
        let attrs = u32::from_le_bytes([rec[36], rec[37], rec[38], rec[39]]);
        let size_high = u32::from_le_bytes([rec[64], rec[65], rec[66], rec[67]]) as u64;
        let size_low = u32::from_le_bytes([rec[68], rec[69], rec[70], rec[71]]) as u64;
        let size = (size_high << 32) | size_low;
        let name_units: Vec<u16> = rec[72..72 + 520]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        out.push(ClipFile {
            name: String::from_utf16_lossy(&name_units),
            size: if is_dir { 0 } else { size },
            is_dir,
        });
    }
    out
}

/// Build a File Contents Request (RANGE) for `len` bytes of file `index`.
fn file_contents_request(stream_id: u32, index: u32, offset: u64, len: u32) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&stream_id.to_le_bytes());
    d.extend_from_slice(&index.to_le_bytes());
    d.extend_from_slice(&FILECONTENTS_RANGE.to_le_bytes());
    d.extend_from_slice(&(offset as u32).to_le_bytes()); // nPositionLow
    d.extend_from_slice(&((offset >> 32) as u32).to_le_bytes()); // nPositionHigh
    d.extend_from_slice(&len.to_le_bytes()); // cbRequested
    d.extend_from_slice(&0u32.to_le_bytes()); // clipDataId
    message(CB_FILECONTENTS_REQUEST, 0, &d)
}

/// Largest single File Contents Request (chunk size for downloading a file).
///
/// The transfer is request/response with one request in flight, so throughput
/// is `chunk / round-trip`: bigger chunks are what make a large copy fast.
/// 8 MiB keeps a 10 ms-RTT link saturated well past a gigabit while staying
/// inside what Windows servers answer in one response.
const FILE_CHUNK: u32 = 8 * 1024 * 1024;

/// The clipboard channel state machine.
#[derive(Default)]
pub struct ClipboardChannel {
    /// Set once the server's Monitor Ready handshake has completed.
    ready: bool,
    /// Remote→local: the entries the session offered. Held from the moment the
    /// descriptor list arrives; the CONTENTS are only fetched when
    /// [`Self::begin_file_fetch`] is called (i.e. the user actually pastes).
    remote_files: Vec<ClipFile>,
    /// Index of the entry being received, and how many of its bytes have
    /// arrived. Chunks stream straight to the provider — never buffered here.
    fetch_index: usize,
    fetch_received: u64,
    /// Whether a contents transfer is currently in flight.
    fetching: bool,
    /// The next CB_FORMAT_DATA_RESPONSE is the file descriptor list (not text).
    expecting_descriptors: bool,
    /// Outstanding format-data requests, front = the one in flight (the spec
    /// allows one at a time; text and image are fetched sequentially).
    pending_formats: std::collections::VecDeque<u32>,
    /// Formats fetched for the paste in progress, applied together once the
    /// last queued request has answered.
    paste_text: Option<String>,
    paste_image: Option<Vec<u8>>,
    stream_id: u32,
    /// Local→remote: the entry list exactly as sent in the descriptor response,
    /// kept so a File Contents Request can be answered from it.
    ///
    /// It has to be a snapshot rather than a fresh `get_files()` per request.
    /// Re-asking the provider re-reads the OS clipboard and re-walks the copied
    /// tree on EVERY request — quadratic once a folder expands to hundreds of
    /// entries — and it invalidates the index→path mapping the peer is midway
    /// through using. This is the list the peer indexes into, so this is the
    /// list we must answer from.
    local_files: Vec<ClipFile>,
}

impl ClipboardChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the initial handshake (Monitor Ready) has been processed.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Entries the session has offered but whose contents have not been
    /// fetched. Empty when there is nothing pending.
    pub fn offered_files(&self) -> &[ClipFile] {
        &self.remote_files
    }

    /// Start fetching the offered files' contents — call this when the user
    /// actually pastes. Returns the first File Contents Request (or nothing if
    /// there is nothing to fetch). Directories are created immediately; only
    /// real files cost a round trip.
    pub fn begin_file_fetch(&mut self, provider: &mut dyn ClipboardProvider) -> Vec<Vec<u8>> {
        if self.fetching || self.remote_files.is_empty() {
            return Vec::new();
        }
        self.fetching = true;
        self.fetch_index = 0;
        self.fetch_received = 0;
        self.pump(provider)
    }

    /// Drive the download: create any directories, finish a completed file, and
    /// issue the request for the next chunk — or finish the copy when every
    /// entry is done. Never buffers file data (chunks go straight to the
    /// provider as they arrive).
    fn pump(&mut self, provider: &mut dyn ClipboardProvider) -> Vec<Vec<u8>> {
        loop {
            let Some(entry) = self.remote_files.get(self.fetch_index).cloned() else {
                // Every entry received.
                provider.finish_remote_files();
                self.remote_files.clear();
                self.fetching = false;
                return Vec::new();
            };
            if entry.is_dir {
                provider.begin_remote_file(&entry.name, 0, true);
                provider.end_remote_file();
                self.fetch_index += 1;
                continue;
            }
            if self.fetch_received == 0 {
                provider.begin_remote_file(&entry.name, entry.size, false);
            }
            if self.fetch_received >= entry.size {
                provider.end_remote_file();
                self.fetch_index += 1;
                self.fetch_received = 0;
                continue;
            }
            let len = (entry.size - self.fetch_received).min(FILE_CHUNK as u64) as u32;
            self.stream_id = self.stream_id.wrapping_add(1);
            return vec![file_contents_request(
                self.stream_id,
                self.fetch_index as u32,
                self.fetch_received,
                len,
            )];
        }
    }

    /// Process one complete inbound CLIPRDR message, returning the messages to
    /// send back (already CLIPRDR-framed; the caller wraps them for the SVC).
    pub fn process(&mut self, msg: &[u8], provider: &mut dyn ClipboardProvider) -> Vec<Vec<u8>> {
        if msg.len() < 8 {
            return Vec::new();
        }
        let msg_type = u16::from_le_bytes([msg[0], msg[1]]);
        let msg_flags = u16::from_le_bytes([msg[2], msg[3]]);
        let data_len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
        let data = msg.get(8..8 + data_len).unwrap_or(&msg[8..]);

        match msg_type {
            CB_MONITOR_READY => {
                self.ready = true;
                // Announce our capabilities, then what's on the local clipboard.
                let files = !provider.get_files().is_empty();
                vec![
                    capabilities(),
                    format_list(
                        provider.get_text().is_some(),
                        files,
                        provider.get_image().is_some(),
                    ),
                ]
            }
            CB_FORMAT_LIST => {
                // Ack, then ask for what we can use. If the session offered files
                // and we're configured to receive them, fetch the file list;
                // otherwise ask for text.
                let mut out = vec![message(CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_OK, &[])];
                let formats = parse_format_list(data);
                let file_fmt = formats
                    .iter()
                    .find(|(_, n)| n == "FileGroupDescriptorW")
                    .map(|(id, _)| *id);
                if let Some(id) = file_fmt {
                    if provider.wants_remote_files() {
                        // Ask only for the DESCRIPTORS (names/sizes) here — the
                        // file contents wait until the user pastes.
                        if self.fetching {
                            provider.abort_remote_files();
                        }
                        self.expecting_descriptors = true;
                        self.fetching = false;
                        self.remote_files.clear();
                        self.fetch_index = 0;
                        self.fetch_received = 0;
                        out.push(message(CB_FORMAT_DATA_REQUEST, 0, &id.to_le_bytes()));
                        return out;
                    }
                }
                let has_text = formats.iter().any(|(id, _)| *id == CF_UNICODETEXT)
                    || (formats.is_empty() && !data.is_empty());
                let has_image = formats.iter().any(|(id, _)| *id == CF_DIB);
                // Fetch what the session offers, one request at a time (text
                // first); each response triggers the next queued request.
                self.pending_formats.clear();
                if has_text {
                    self.pending_formats.push_back(CF_UNICODETEXT);
                }
                if has_image {
                    self.pending_formats.push_back(CF_DIB);
                }
                if let Some(&first) = self.pending_formats.front() {
                    out.push(message(CB_FORMAT_DATA_REQUEST, 0, &first.to_le_bytes()));
                }
                out
            }
            CB_FORMAT_DATA_REQUEST => {
                // The peer wants our clipboard data for a specific format.
                let format_id = u32::from_le_bytes([
                    *data.first().unwrap_or(&0),
                    *data.get(1).unwrap_or(&0),
                    *data.get(2).unwrap_or(&0),
                    *data.get(3).unwrap_or(&0),
                ]);
                if format_id == CF_FILEGROUPDESCRIPTORW {
                    // Snapshot the tree ONCE, here, and answer every later File
                    // Contents Request from it — the peer's `lindex` refers to
                    // this exact list.
                    let files = provider.get_files();
                    self.local_files = files.clone();
                    if files.is_empty() {
                        tracing::warn!(
                            "session asked for the copied file list but the local clipboard \
                             yielded no entries"
                        );
                        vec![message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, &[])]
                    } else {
                        tracing::info!(
                            entries = files.len(),
                            dirs = files.iter().filter(|f| f.is_dir).count(),
                            bytes = files.iter().map(|f| f.size).sum::<u64>(),
                            "sending the copied file list to the session"
                        );
                        vec![message(
                            CB_FORMAT_DATA_RESPONSE,
                            CB_RESPONSE_OK,
                            &file_group_descriptor(&files),
                        )]
                    }
                } else if format_id == CF_DIB {
                    match provider.get_image() {
                        Some(dib) => vec![message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &dib)],
                        None => vec![message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, &[])],
                    }
                } else {
                    match provider.get_text() {
                        Some(text) => vec![message(
                            CB_FORMAT_DATA_RESPONSE,
                            CB_RESPONSE_OK,
                            &unicode_response(&text),
                        )],
                        None => vec![message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, &[])],
                    }
                }
            }
            CB_FORMAT_DATA_RESPONSE => {
                if msg_flags & CB_RESPONSE_OK == 0 {
                    self.expecting_descriptors = false;
                    // The in-flight request failed; move on to the next queued one.
                    self.pending_formats.pop_front();
                    return self.next_format_request();
                }
                if self.expecting_descriptors {
                    // The session's file list. Advertise it locally and STOP —
                    // the bytes are fetched only if the user pastes
                    // (`begin_file_fetch`), so copying a huge file in the
                    // session costs nothing until it is wanted.
                    self.expecting_descriptors = false;
                    self.remote_files = parse_file_descriptors(data);
                    self.fetch_index = 0;
                    self.fetch_received = 0;
                    provider.offer_remote_files(&self.remote_files);
                    return Vec::new();
                }
                // Buffer each format; the whole paste is applied at once when the
                // last queued request has answered (see `set_contents`).
                match self.pending_formats.pop_front() {
                    Some(CF_DIB) => self.paste_image = Some(data.to_vec()),
                    // Text is also the legacy default when nothing was tracked.
                    _ => self.paste_text = Some(decode_unicode(data)),
                }
                if self.pending_formats.is_empty() {
                    provider.set_contents(
                        self.paste_text.as_deref(),
                        self.paste_image.as_deref(),
                    );
                    self.paste_text = None;
                    self.paste_image = None;
                }
                self.next_format_request()
            }
            CB_FILECONTENTS_RESPONSE => {
                // A chunk of a file we're downloading: streamId(4) + data.
                if !self.fetching {
                    return Vec::new(); // stale response from an abandoned copy
                }
                if msg_flags & CB_RESPONSE_OK != 0 {
                    // Straight to the provider — the data is never held here, so
                    // a multi-gigabyte file costs no memory.
                    let chunk = data.get(4..).unwrap_or(&[]);
                    provider.write_remote_chunk(chunk);
                    self.fetch_received += chunk.len() as u64;
                    // A server that answers with nothing would spin us forever;
                    // treat it as end-of-file for this entry.
                    if chunk.is_empty() {
                        if let Some(e) = self.remote_files.get_mut(self.fetch_index) {
                            e.size = self.fetch_received;
                        }
                    }
                    return self.pump(provider);
                }
                // A failed chunk aborts the whole copy.
                provider.abort_remote_files();
                self.remote_files.clear();
                self.fetching = false;
                Vec::new()
            }
            CB_FILECONTENTS_REQUEST => {
                // streamId(4), lindex(4), dwFlags(4), posLow(4), posHigh(4), cbRequested(4).
                let rd = |o: usize| {
                    u32::from_le_bytes([
                        *data.get(o).unwrap_or(&0),
                        *data.get(o + 1).unwrap_or(&0),
                        *data.get(o + 2).unwrap_or(&0),
                        *data.get(o + 3).unwrap_or(&0),
                    ])
                };
                let stream_id = rd(0);
                let index = rd(4);
                let flags = rd(8);
                let offset = ((rd(16) as u64) << 32) | rd(12) as u64;
                let requested = rd(20);
                if flags & FILECONTENTS_SIZE != 0 {
                    // Answer from the snapshot we sent — never by re-walking the
                    // tree, which would both cost O(entries) per request and
                    // renumber the indices out from under the peer mid-copy.
                    let Some(entry) = self.local_files.get(index as usize) else {
                        return vec![message(
                            CB_FILECONTENTS_RESPONSE,
                            CB_RESPONSE_FAIL,
                            &stream_id.to_le_bytes(),
                        )];
                    };
                    let mut body = stream_id.to_le_bytes().to_vec();
                    body.extend_from_slice(&entry.size.to_le_bytes());
                    vec![message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &body)]
                } else if flags & FILECONTENTS_RANGE != 0 {
                    match provider.read_file(index, offset, requested) {
                        Some(bytes) => {
                            let mut body = stream_id.to_le_bytes().to_vec();
                            body.extend_from_slice(&bytes);
                            vec![message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &body)]
                        }
                        None => vec![message(
                            CB_FILECONTENTS_RESPONSE,
                            CB_RESPONSE_FAIL,
                            &stream_id.to_le_bytes(),
                        )],
                    }
                } else {
                    vec![message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_FAIL, &stream_id.to_le_bytes())]
                }
            }
            // CB_CLIP_CAPS, CB_FORMAT_LIST_RESPONSE, others: nothing to send.
            _ => Vec::new(),
        }
    }

    /// Build a Format List announcing the local clipboard changed (call on a
    /// local clipboard-update notification). `None` before the handshake.
    pub fn announce_local(
        &self,
        has_text: bool,
        has_files: bool,
        has_image: bool,
    ) -> Option<Vec<u8>> {
        self.ready.then(|| format_list(has_text, has_files, has_image))
    }

    /// The data request for the next queued format, if any remain.
    fn next_format_request(&mut self) -> Vec<Vec<u8>> {
        match self.pending_formats.front() {
            Some(&id) => vec![message(CB_FORMAT_DATA_REQUEST, 0, &id.to_le_bytes())],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockClipboard {
        local: Option<String>,
        pasted: Option<String>,
    }
    impl ClipboardProvider for MockClipboard {
        fn get_text(&mut self) -> Option<String> {
            self.local.clone()
        }
        fn set_text(&mut self, text: &str) {
            self.pasted = Some(text.to_string());
        }
    }

    fn msg_type(m: &[u8]) -> u16 {
        u16::from_le_bytes([m[0], m[1]])
    }

    #[test]
    fn monitor_ready_triggers_caps_and_format_list() {
        let mut clip = ClipboardChannel::new();
        let mut prov = MockClipboard {
            local: Some("local text".into()),
            ..Default::default()
        };
        let out = clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        assert!(clip.is_ready());
        assert_eq!(out.len(), 2);
        assert_eq!(msg_type(&out[0]), CB_CLIP_CAPS);
        assert_eq!(msg_type(&out[1]), CB_FORMAT_LIST);
        // Format list non-empty because we have local text.
        assert!(out[1].len() > 8);
    }

    #[test]
    fn server_format_list_acks_and_requests_text() {
        let mut clip = ClipboardChannel::new();
        let mut prov = MockClipboard::default();
        // Server advertises a format (non-empty list).
        let mut list = Vec::new();
        list.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        list.extend_from_slice(&[0, 0]);
        let out = clip.process(&message(CB_FORMAT_LIST, 0, &list), &mut prov);
        assert_eq!(out.len(), 2);
        assert_eq!(msg_type(&out[0]), CB_FORMAT_LIST_RESPONSE);
        assert_eq!(msg_type(&out[1]), CB_FORMAT_DATA_REQUEST);
        assert_eq!(&out[1][8..12], &CF_UNICODETEXT.to_le_bytes());
    }

    #[test]
    fn remote_data_response_sets_local_clipboard() {
        let mut clip = ClipboardChannel::new();
        let mut prov = MockClipboard::default();
        let payload = unicode_response("hello™");
        clip.process(
            &message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &payload),
            &mut prov,
        );
        assert_eq!(prov.pasted.as_deref(), Some("hello™"));
    }

    #[test]
    fn data_request_answers_with_local_text() {
        let mut clip = ClipboardChannel::new();
        let mut prov = MockClipboard {
            local: Some("copy me".into()),
            ..Default::default()
        };
        let out = clip.process(
            &message(CB_FORMAT_DATA_REQUEST, 0, &CF_UNICODETEXT.to_le_bytes()),
            &mut prov,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(msg_type(&out[0]), CB_FORMAT_DATA_RESPONSE);
        assert_eq!(
            u16::from_le_bytes([out[0][2], out[0][3]]),
            CB_RESPONSE_OK
        );
        assert_eq!(decode_unicode(&out[0][8..]), "copy me");
    }

    #[test]
    fn data_request_with_empty_clipboard_fails_cleanly() {
        let mut clip = ClipboardChannel::new();
        let mut prov = MockClipboard::default();
        let out = clip.process(
            &message(CB_FORMAT_DATA_REQUEST, 0, &CF_UNICODETEXT.to_le_bytes()),
            &mut prov,
        );
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_FAIL);
    }

    #[test]
    fn announce_local_gated_on_handshake() {
        let mut clip = ClipboardChannel::new();
        assert_eq!(clip.announce_local(true, false, false), None); // not ready yet
        let mut prov = MockClipboard::default();
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        assert!(clip.announce_local(true, false, false).is_some());
    }

    /// A provider holding an image (and optionally text), to exercise CF_DIB.
    #[derive(Default)]
    struct ImageClipboard {
        image: Option<Vec<u8>>,
        text: Option<String>,
        pasted_image: Option<Vec<u8>>,
        pasted_text: Option<String>,
        set_contents_calls: usize,
    }
    impl ClipboardProvider for ImageClipboard {
        fn get_text(&mut self) -> Option<String> {
            self.text.clone()
        }
        fn set_text(&mut self, text: &str) {
            self.pasted_text = Some(text.to_string());
        }
        fn get_image(&mut self) -> Option<Vec<u8>> {
            self.image.clone()
        }
        fn set_image(&mut self, dib: &[u8]) {
            self.pasted_image = Some(dib.to_vec());
        }
        /// Mirrors a real OS clipboard: publishing a paste REPLACES everything,
        /// so a channel that applied formats one at a time would lose all but
        /// the last.
        fn set_contents(&mut self, text: Option<&str>, image: Option<&[u8]>) {
            self.pasted_text = text.map(|t| t.to_string());
            self.pasted_image = image.map(|i| i.to_vec());
            self.set_contents_calls += 1;
        }
    }

    #[test]
    fn local_image_is_announced_and_served_as_cf_dib() {
        let dib = vec![0x28, 0, 0, 0, 0xAA, 0xBB]; // stand-in BITMAPINFO + pixels
        let mut clip = ClipboardChannel::new();
        let mut prov = ImageClipboard {
            image: Some(dib.clone()),
            ..Default::default()
        };
        // The handshake advertises CF_DIB when the local clipboard holds one.
        let out = clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        let list = &out[1];
        let formats = parse_format_list(&list[8..]);
        assert!(formats.iter().any(|(id, _)| *id == CF_DIB));
        // ...and a data request for it returns the bytes verbatim.
        let out = clip.process(
            &message(CB_FORMAT_DATA_REQUEST, 0, &CF_DIB.to_le_bytes()),
            &mut prov,
        );
        assert_eq!(msg_type(&out[0]), CB_FORMAT_DATA_RESPONSE);
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_OK);
        assert_eq!(&out[0][8..], &dib[..]);
    }

    #[test]
    fn remote_image_is_requested_after_text_and_pasted_locally() {
        let mut clip = ClipboardChannel::new();
        let mut prov = ImageClipboard::default();
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);

        // The session offers both text and an image.
        let mut list = Vec::new();
        list.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        list.extend_from_slice(&[0, 0]);
        list.extend_from_slice(&CF_DIB.to_le_bytes());
        list.extend_from_slice(&[0, 0]);
        let out = clip.process(&message(CB_FORMAT_LIST, 0, &list), &mut prov);
        // Ack + a request for text first (one request may be in flight).
        assert_eq!(out.len(), 2);
        assert_eq!(msg_type(&out[1]), CB_FORMAT_DATA_REQUEST);
        assert_eq!(&out[1][8..12], &CF_UNICODETEXT.to_le_bytes());

        // Answer the text; the channel then asks for the image. Nothing is
        // applied yet — the paste is held until every format has answered.
        let out = clip.process(
            &message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &unicode_response("hi")),
            &mut prov,
        );
        assert_eq!(prov.set_contents_calls, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][8..12], &CF_DIB.to_le_bytes());

        // Answer the image; now the whole paste lands in ONE update, so the
        // text isn't wiped by the image write (a real clipboard replaces all).
        let dib = vec![0x28, 0, 0, 0, 1, 2, 3, 4];
        let out = clip.process(
            &message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &dib),
            &mut prov,
        );
        assert_eq!(prov.set_contents_calls, 1);
        assert_eq!(prov.pasted_text.as_deref(), Some("hi"));
        assert_eq!(prov.pasted_image.as_deref(), Some(&dib[..]));
        assert!(out.is_empty()); // nothing left queued
    }

    #[test]
    fn text_only_paste_still_applies_without_an_image() {
        let mut clip = ClipboardChannel::new();
        let mut prov = ImageClipboard::default();
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        let mut list = Vec::new();
        list.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        list.extend_from_slice(&[0, 0]);
        clip.process(&message(CB_FORMAT_LIST, 0, &list), &mut prov);
        clip.process(
            &message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &unicode_response("solo")),
            &mut prov,
        );
        assert_eq!(prov.pasted_text.as_deref(), Some("solo"));
        assert_eq!(prov.pasted_image, None);
    }

    #[derive(Default)]
    struct FileClipboard {
        files: Vec<ClipFile>,
        /// How many times the channel re-enumerated the copied tree.
        walks: usize,
    }
    impl ClipboardProvider for FileClipboard {
        fn get_text(&mut self) -> Option<String> {
            None
        }
        fn set_text(&mut self, _t: &str) {}
        fn get_files(&mut self) -> Vec<ClipFile> {
            self.walks += 1;
            self.files.clone()
        }
        fn read_file(&mut self, index: u32, offset: u64, len: u32) -> Option<Vec<u8>> {
            // Synthetic content: byte value = index, `len` bytes.
            (index < self.files.len() as u32).then(|| {
                let _ = offset;
                vec![index as u8; len as usize]
            })
        }
    }

    #[test]
    fn file_clipboard_announces_and_serves_contents() {
        let mut clip = ClipboardChannel::new();
        let mut prov = FileClipboard {
            files: vec![ClipFile { name: "report.pdf".into(), size: 1234, is_dir: false }],
            ..Default::default()
        };
        // Monitor Ready → caps + a format list that includes FileGroupDescriptorW.
        let out = clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        let fl = &out[1];
        assert_eq!(u16::from_le_bytes([fl[0], fl[1]]), CB_FORMAT_LIST);
        assert_eq!(u32::from_le_bytes([fl[8], fl[9], fl[10], fl[11]]), CF_FILEGROUPDESCRIPTORW);

        // Server requests the file group descriptor → packed FILEDESCRIPTORW.
        let out = clip.process(
            &message(CB_FORMAT_DATA_REQUEST, 0, &CF_FILEGROUPDESCRIPTORW.to_le_bytes()),
            &mut prov,
        );
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_OK);
        // cItems = 1, then the 592-byte descriptor; fileSizeLow = 1234.
        assert_eq!(u32::from_le_bytes([out[0][8], out[0][9], out[0][10], out[0][11]]), 1);

        // File Contents Request (SIZE) → 8-byte size.
        let mut req = Vec::new();
        req.extend_from_slice(&7u32.to_le_bytes()); // streamId
        req.extend_from_slice(&0u32.to_le_bytes()); // lindex
        req.extend_from_slice(&FILECONTENTS_SIZE.to_le_bytes());
        req.extend_from_slice(&[0u8; 12]); // pos + cbRequested
        let out = clip.process(&message(CB_FILECONTENTS_REQUEST, 0, &req), &mut prov);
        assert_eq!(u16::from_le_bytes([out[0][0], out[0][1]]), CB_FILECONTENTS_RESPONSE);
        assert_eq!(u32::from_le_bytes([out[0][8], out[0][9], out[0][10], out[0][11]]), 7); // streamId echoed
        let size = u64::from_le_bytes([
            out[0][12], out[0][13], out[0][14], out[0][15], out[0][16], out[0][17], out[0][18], out[0][19],
        ]);
        assert_eq!(size, 1234);

        // File Contents Request (RANGE) → bytes from read_file.
        let mut req = Vec::new();
        req.extend_from_slice(&8u32.to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes());
        req.extend_from_slice(&FILECONTENTS_RANGE.to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes()); // posLow
        req.extend_from_slice(&0u32.to_le_bytes()); // posHigh
        req.extend_from_slice(&4u32.to_le_bytes()); // cbRequested
        let out = clip.process(&message(CB_FILECONTENTS_REQUEST, 0, &req), &mut prov);
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_OK);
        assert_eq!(&out[0][12..16], &[0, 0, 0, 0]); // 4 bytes of file 0
    }

    /// Copying a FOLDER is where the per-request re-enumeration bit: every File
    /// Contents Request used to re-read the clipboard and re-walk the whole tree,
    /// which is quadratic in the entry count and renumbers the indices the peer
    /// is midway through using. The descriptor response is the one and only walk.
    #[test]
    fn folder_contents_requests_reuse_the_descriptor_snapshot() {
        let mut clip = ClipboardChannel::new();
        let mut prov = FileClipboard {
            files: vec![
                ClipFile { name: "docs".into(), size: 0, is_dir: true },
                ClipFile { name: "docs\\a.txt".into(), size: 3, is_dir: false },
                ClipFile { name: "docs\\sub".into(), size: 0, is_dir: true },
                ClipFile { name: "docs\\sub\\b.bin".into(), size: 9, is_dir: false },
            ],
            ..Default::default()
        };
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        clip.process(
            &message(CB_FORMAT_DATA_REQUEST, 0, &CF_FILEGROUPDESCRIPTORW.to_le_bytes()),
            &mut prov,
        );
        let after_descriptors = prov.walks;

        let size_request = |lindex: u32| {
            let mut req = Vec::new();
            req.extend_from_slice(&1u32.to_le_bytes()); // streamId
            req.extend_from_slice(&lindex.to_le_bytes());
            req.extend_from_slice(&FILECONTENTS_SIZE.to_le_bytes());
            req.extend_from_slice(&[0u8; 12]);
            message(CB_FILECONTENTS_REQUEST, 0, &req)
        };
        let size_of = |out: &[Vec<u8>]| {
            u64::from_le_bytes([
                out[0][12], out[0][13], out[0][14], out[0][15], out[0][16], out[0][17], out[0][18],
                out[0][19],
            ])
        };

        // A directory reports zero; the files report their real sizes.
        let out = clip.process(&size_request(0), &mut prov);
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_OK);
        assert_eq!(size_of(&out), 0);
        assert_eq!(size_of(&clip.process(&size_request(1), &mut prov)), 3);
        assert_eq!(size_of(&clip.process(&size_request(3), &mut prov)), 9);

        // An index past the offered list is a failure, not a silent zero — a zero
        // there reads as "empty file" and the peer writes a truncated copy.
        let out = clip.process(&size_request(9), &mut prov);
        assert_eq!(u16::from_le_bytes([out[0][2], out[0][3]]), CB_RESPONSE_FAIL);

        // The whole exchange cost no extra tree walks.
        assert_eq!(prov.walks, after_descriptors);
    }

    #[derive(Default)]
    struct DownloadClipboard {
        /// Completed entries: (name, contents, is_dir).
        saved: Vec<(String, Vec<u8>, bool)>,
        /// The entry currently streaming in.
        current: Option<(String, Vec<u8>, bool)>,
        /// Entries the session offered but whose bytes were not requested.
        offered: Vec<ClipFile>,
        /// Set when the channel signals the copy is complete — this is what a
        /// real provider hangs "publish as CF_HDROP" off, so a paste works.
        published: bool,
        aborted: bool,
    }
    impl ClipboardProvider for DownloadClipboard {
        fn get_text(&mut self) -> Option<String> {
            None
        }
        fn set_text(&mut self, _t: &str) {}
        fn wants_remote_files(&self) -> bool {
            true
        }
        fn offer_remote_files(&mut self, files: &[ClipFile]) {
            self.offered = files.to_vec();
        }
        fn begin_remote_file(&mut self, name: &str, _size: u64, is_dir: bool) {
            self.current = Some((name.to_string(), Vec::new(), is_dir));
        }
        fn write_remote_chunk(&mut self, data: &[u8]) {
            if let Some((_, buf, _)) = self.current.as_mut() {
                buf.extend_from_slice(data);
            }
        }
        fn end_remote_file(&mut self) {
            if let Some(e) = self.current.take() {
                self.saved.push(e);
            }
        }
        fn finish_remote_files(&mut self) {
            self.published = true;
        }
        fn abort_remote_files(&mut self) {
            self.aborted = true;
            self.current = None;
        }
    }

    /// Pack a FILEDESCRIPTORW list from `(name, size, is_dir)` entries.
    fn descriptors(entries: &[(&str, u64, bool)]) -> Vec<u8> {
        let mut d = (entries.len() as u32).to_le_bytes().to_vec(); // cItems
        for &(name, size, is_dir) in entries {
            let mut rec = vec![0u8; 592];
            let attrs: u32 = if is_dir { 0x10 } else { 0x20 };
            rec[36..40].copy_from_slice(&attrs.to_le_bytes());
            rec[64..68].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
            rec[68..72].copy_from_slice(&(size as u32).to_le_bytes());
            for (i, u) in name.encode_utf16().enumerate() {
                rec[72 + i * 2..74 + i * 2].copy_from_slice(&u.to_le_bytes());
            }
            d.extend_from_slice(&rec);
        }
        d
    }

    /// Walk a provider through the announce → descriptor exchange, leaving the
    /// files offered but NOT fetched.
    fn offer(clip: &mut ClipboardChannel, prov: &mut DownloadClipboard, desc: &[u8]) {
        clip.process(&message(CB_MONITOR_READY, 0, &[]), prov);
        let mut fl = CF_FILEGROUPDESCRIPTORW.to_le_bytes().to_vec();
        fl.extend_from_slice(&utf16z("FileGroupDescriptorW"));
        let out = clip.process(&message(CB_FORMAT_LIST, 0, &fl), prov);
        assert_eq!(msg_type(&out[1]), CB_FORMAT_DATA_REQUEST);
        assert_eq!(&out[1][8..12], &CF_FILEGROUPDESCRIPTORW.to_le_bytes());
        let out = clip.process(&message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, desc), prov);
        // Nothing is transferred yet — the descriptors only advertise.
        assert!(out.is_empty(), "descriptors must not trigger a transfer");
    }

    #[test]
    fn files_are_offered_without_transferring_until_a_paste() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        offer(&mut clip, &mut prov, &descriptors(&[("a.txt", 5, false)]));

        // Advertised locally, but no bytes moved: copying a huge file in the
        // session costs nothing until it is actually pasted.
        assert_eq!(prov.offered.len(), 1);
        assert_eq!(prov.offered[0].name, "a.txt");
        assert!(prov.saved.is_empty());
        assert!(!prov.published);

        // The paste asks for the bytes.
        let out = clip.begin_file_fetch(&mut prov);
        assert_eq!(msg_type(&out[0]), CB_FILECONTENTS_REQUEST);

        let mut resp = 1u32.to_le_bytes().to_vec(); // streamId
        resp.extend_from_slice(b"hello");
        let out = clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &resp), &mut prov);
        assert_eq!(prov.saved.len(), 1);
        assert_eq!(prov.saved[0].0, "a.txt");
        assert_eq!(prov.saved[0].1, b"hello");
        assert!(prov.published, "provider must be told the copy finished");
        assert!(out.is_empty());
    }

    #[test]
    fn large_file_streams_in_chunks_without_buffering() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        // Two chunks' worth plus a tail, so the request/response loop repeats.
        let total = FILE_CHUNK as u64 * 2 + 7;
        offer(&mut clip, &mut prov, &descriptors(&[("big.bin", total, false)]));

        let out = clip.begin_file_fetch(&mut prov);
        // First request asks for a full chunk from offset 0.
        assert_eq!(&out[0][12..16], &0u32.to_le_bytes()); // lindex
        assert_eq!(&out[0][20..24], &0u32.to_le_bytes()); // nPositionLow
        assert_eq!(&out[0][28..32], &FILE_CHUNK.to_le_bytes()); // cbRequested

        // Answer chunk 1 → the next request continues at the chunk boundary.
        let mut r = 1u32.to_le_bytes().to_vec();
        r.extend_from_slice(&vec![0xAB; FILE_CHUNK as usize]);
        let out = clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &r), &mut prov);
        assert_eq!(&out[0][20..24], &FILE_CHUNK.to_le_bytes()); // resumes at 8 MiB
        assert!(prov.saved.is_empty(), "file is not complete yet");

        // Answer chunk 2, then the 7-byte tail.
        let mut r = 2u32.to_le_bytes().to_vec();
        r.extend_from_slice(&vec![0xCD; FILE_CHUNK as usize]);
        let out = clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &r), &mut prov);
        assert_eq!(&out[0][28..32], &7u32.to_le_bytes()); // only the remainder
        let mut r = 3u32.to_le_bytes().to_vec();
        r.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        let out = clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &r), &mut prov);

        assert_eq!(prov.saved.len(), 1);
        assert_eq!(prov.saved[0].1.len() as u64, total);
        assert!(prov.published);
        assert!(out.is_empty());
    }

    #[test]
    fn folders_are_created_without_costing_a_round_trip() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        // A folder, a file inside it, and a nested empty folder.
        offer(
            &mut clip,
            &mut prov,
            &descriptors(&[
                ("docs", 0, true),
                ("docs\\notes.txt", 4, false),
                ("docs\\empty", 0, true),
            ]),
        );

        // The first request is for the FILE — both directories are created
        // inline, so a deep tree costs no extra round trips.
        let out = clip.begin_file_fetch(&mut prov);
        assert_eq!(msg_type(&out[0]), CB_FILECONTENTS_REQUEST);
        assert_eq!(&out[0][12..16], &1u32.to_le_bytes()); // lindex 1 = the file
        assert_eq!(prov.saved.len(), 1);
        assert_eq!(prov.saved[0].0, "docs");
        assert!(prov.saved[0].2, "first entry is a directory");

        let mut r = 1u32.to_le_bytes().to_vec();
        r.extend_from_slice(b"abcd");
        let out = clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &r), &mut prov);
        assert!(out.is_empty());
        // Tree rebuilt in order, with the relative path preserved.
        let names: Vec<&str> = prov.saved.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, ["docs", "docs\\notes.txt", "docs\\empty"]);
        assert_eq!(prov.saved[1].1, b"abcd");
        assert!(prov.saved[2].2);
        assert!(prov.published);
    }

    #[test]
    fn a_failed_chunk_aborts_the_copy() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        offer(&mut clip, &mut prov, &descriptors(&[("a.bin", 9, false)]));
        clip.begin_file_fetch(&mut prov);
        let out = clip.process(
            &message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_FAIL, &1u32.to_le_bytes()),
            &mut prov,
        );
        assert!(prov.aborted, "partial state must be dropped");
        assert!(!prov.published, "a failed copy must not publish");
        assert!(out.is_empty());
    }

    #[test]
    fn a_new_copy_supersedes_an_unfetched_one() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        offer(&mut clip, &mut prov, &descriptors(&[("old.bin", 5, false)]));
        assert_eq!(prov.offered[0].name, "old.bin");
        // The session copies something else before we ever paste.
        let mut fl = CF_FILEGROUPDESCRIPTORW.to_le_bytes().to_vec();
        fl.extend_from_slice(&utf16z("FileGroupDescriptorW"));
        clip.process(&message(CB_FORMAT_LIST, 0, &fl), &mut prov);
        clip.process(
            &message(
                CB_FORMAT_DATA_RESPONSE,
                CB_RESPONSE_OK,
                &descriptors(&[("new.bin", 6, false)]),
            ),
            &mut prov,
        );
        assert_eq!(prov.offered.len(), 1);
        assert_eq!(prov.offered[0].name, "new.bin");
    }
}
