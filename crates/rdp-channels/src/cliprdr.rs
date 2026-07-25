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
    /// File name (no path) as shown when pasting in the session.
    pub name: String,
    pub size: u64,
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
    /// Whether to pull files the *session* puts on its clipboard down to the
    /// local machine (e.g. a download directory is configured). Default: no.
    fn wants_remote_files(&self) -> bool {
        false
    }
    /// Save a complete file fetched from the session's clipboard locally.
    fn save_remote_file(&mut self, _name: &str, _data: &[u8]) {}
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

const CB_CAPSTYPE_GENERAL: u16 = 0x0001;
const CB_CAPS_VERSION_2: u32 = 0x0000_0002;
const CB_USE_LONG_FORMAT_NAMES: u32 = 0x0000_0002;

/// `CF_UNICODETEXT` — UTF-16LE text, the format we sync.
const CF_UNICODETEXT: u32 = 13;

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
fn format_list(has_text: bool, has_files: bool) -> Vec<u8> {
    let mut d = Vec::new();
    if has_text {
        d.extend_from_slice(&CF_UNICODETEXT.to_le_bytes());
        d.extend_from_slice(&[0, 0]); // empty long-format name (standard format)
    }
    if has_files {
        d.extend_from_slice(&CF_FILEGROUPDESCRIPTORW.to_le_bytes());
        d.extend_from_slice(&utf16z("FileGroupDescriptorW"));
    }
    message(CB_FORMAT_LIST, 0, &d)
}

/// Pack a `CLIPRDR_FILELIST` (cItems + that many 592-byte `FILEDESCRIPTORW`).
fn file_group_descriptor(files: &[ClipFile]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&(files.len() as u32).to_le_bytes()); // cItems
    for f in files {
        d.extend_from_slice(&(FD_ATTRIBUTES | FD_FILESIZE).to_le_bytes()); // flags
        d.extend_from_slice(&[0u8; 32]); // reserved1
        d.extend_from_slice(&FILE_ATTRIBUTE_ARCHIVE.to_le_bytes()); // fileAttributes
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
/// `(name, size)` pairs.
fn parse_file_descriptors(data: &[u8]) -> Vec<(String, u64)> {
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
        // sizeHigh at +64, sizeLow at +68, fileName (520 bytes UTF-16) at +72.
        let size_high = u32::from_le_bytes([rec[64], rec[65], rec[66], rec[67]]) as u64;
        let size_low = u32::from_le_bytes([rec[68], rec[69], rec[70], rec[71]]) as u64;
        let size = (size_high << 32) | size_low;
        let name_units: Vec<u16> = rec[72..72 + 520]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        out.push((String::from_utf16_lossy(&name_units), size));
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
const FILE_CHUNK: u32 = 4 * 1024 * 1024;

/// The clipboard channel state machine.
#[derive(Default)]
pub struct ClipboardChannel {
    /// Set once the server's Monitor Ready handshake has completed.
    ready: bool,
    /// Remote→local download: the descriptors the session offered, the file
    /// being fetched, its accumulated bytes, and a stream-id counter. Active
    /// only when the provider wants remote files.
    remote_files: Vec<(String, u64)>,
    fetch_index: usize,
    fetch_buf: Vec<u8>,
    /// The next CB_FORMAT_DATA_RESPONSE is the file descriptor list (not text).
    expecting_descriptors: bool,
    stream_id: u32,
}

impl ClipboardChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the initial handshake (Monitor Ready) has been processed.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Build the File Contents Request for the next chunk of the file currently
    /// being downloaded, or `None` when all offered files are done.
    fn request_next_file(&mut self) -> Option<Vec<u8>> {
        let &(_, size) = self.remote_files.get(self.fetch_index)?;
        let offset = self.fetch_buf.len() as u64;
        let len = size.saturating_sub(offset).min(FILE_CHUNK as u64) as u32;
        self.stream_id = self.stream_id.wrapping_add(1);
        Some(file_contents_request(
            self.stream_id,
            self.fetch_index as u32,
            offset,
            len,
        ))
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
                    format_list(provider.get_text().is_some(), files),
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
                        self.expecting_descriptors = true;
                        self.remote_files.clear();
                        self.fetch_index = 0;
                        self.fetch_buf.clear();
                        out.push(message(CB_FORMAT_DATA_REQUEST, 0, &id.to_le_bytes()));
                        return out;
                    }
                }
                let has_text = formats.iter().any(|(id, _)| *id == CF_UNICODETEXT)
                    || (formats.is_empty() && !data.is_empty());
                if has_text {
                    out.push(message(CB_FORMAT_DATA_REQUEST, 0, &CF_UNICODETEXT.to_le_bytes()));
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
                    let files = provider.get_files();
                    if files.is_empty() {
                        vec![message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, &[])]
                    } else {
                        vec![message(
                            CB_FORMAT_DATA_RESPONSE,
                            CB_RESPONSE_OK,
                            &file_group_descriptor(&files),
                        )]
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
                    return Vec::new();
                }
                if self.expecting_descriptors {
                    // The session's file list — start downloading the files.
                    self.expecting_descriptors = false;
                    self.remote_files = parse_file_descriptors(data);
                    self.fetch_index = 0;
                    self.fetch_buf.clear();
                    return self.request_next_file().map(|r| vec![r]).unwrap_or_default();
                }
                provider.set_text(&decode_unicode(data));
                Vec::new()
            }
            CB_FILECONTENTS_RESPONSE => {
                // A chunk of a file we're downloading: streamId(4) + data.
                if msg_flags & CB_RESPONSE_OK != 0 {
                    self.fetch_buf.extend_from_slice(data.get(4..).unwrap_or(&[]));
                    if let Some((name, size)) = self.remote_files.get(self.fetch_index).cloned() {
                        if self.fetch_buf.len() as u64 >= size {
                            provider.save_remote_file(&name, &self.fetch_buf);
                            self.fetch_index += 1;
                            self.fetch_buf.clear();
                        }
                    }
                    return self.request_next_file().map(|r| vec![r]).unwrap_or_default();
                }
                // A failed chunk aborts the current download.
                self.remote_files.clear();
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
                    // Reply with the 8-byte file size.
                    let size = provider
                        .get_files()
                        .get(index as usize)
                        .map(|f| f.size)
                        .unwrap_or(0);
                    let mut body = stream_id.to_le_bytes().to_vec();
                    body.extend_from_slice(&size.to_le_bytes());
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
    pub fn announce_local(&self, has_text: bool, has_files: bool) -> Option<Vec<u8>> {
        self.ready.then(|| format_list(has_text, has_files))
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
        assert_eq!(clip.announce_local(true, false), None); // not ready yet
        let mut prov = MockClipboard::default();
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);
        assert!(clip.announce_local(true, false).is_some());
    }

    #[derive(Default)]
    struct FileClipboard {
        files: Vec<ClipFile>,
    }
    impl ClipboardProvider for FileClipboard {
        fn get_text(&mut self) -> Option<String> {
            None
        }
        fn set_text(&mut self, _t: &str) {}
        fn get_files(&mut self) -> Vec<ClipFile> {
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
            files: vec![ClipFile { name: "report.pdf".into(), size: 1234 }],
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

    #[derive(Default)]
    struct DownloadClipboard {
        saved: Vec<(String, Vec<u8>)>,
    }
    impl ClipboardProvider for DownloadClipboard {
        fn get_text(&mut self) -> Option<String> {
            None
        }
        fn set_text(&mut self, _t: &str) {}
        fn wants_remote_files(&self) -> bool {
            true
        }
        fn save_remote_file(&mut self, name: &str, data: &[u8]) {
            self.saved.push((name.to_string(), data.to_vec()));
        }
    }

    /// Pack a single FILEDESCRIPTORW (592 bytes) for `name`/`size`.
    fn one_descriptor(name: &str, size: u64) -> Vec<u8> {
        let mut d = 1u32.to_le_bytes().to_vec(); // cItems
        let mut rec = vec![0u8; 592];
        rec[64..68].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        rec[68..72].copy_from_slice(&(size as u32).to_le_bytes());
        for (i, u) in name.encode_utf16().enumerate() {
            rec[72 + i * 2..74 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        d.extend_from_slice(&rec);
        d
    }

    #[test]
    fn remote_files_are_downloaded() {
        let mut clip = ClipboardChannel::new();
        let mut prov = DownloadClipboard::default();
        clip.process(&message(CB_MONITOR_READY, 0, &[]), &mut prov);

        // Server advertises a file on its clipboard → client requests the list.
        let mut fl = CF_FILEGROUPDESCRIPTORW.to_le_bytes().to_vec();
        fl.extend_from_slice(&utf16z("FileGroupDescriptorW"));
        let out = clip.process(&message(CB_FORMAT_LIST, 0, &fl), &mut prov);
        // Ack + a data request for the file descriptor format.
        assert_eq!(msg_type(&out[1]), CB_FORMAT_DATA_REQUEST);
        assert_eq!(&out[1][8..12], &CF_FILEGROUPDESCRIPTORW.to_le_bytes());

        // Server returns one 5-byte file descriptor → client requests its bytes.
        let out = clip.process(
            &message(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, &one_descriptor("a.txt", 5)),
            &mut prov,
        );
        assert_eq!(msg_type(&out[0]), CB_FILECONTENTS_REQUEST);

        // Server returns the 5 bytes → client saves the file locally.
        let mut resp = 1u32.to_le_bytes().to_vec(); // streamId
        resp.extend_from_slice(b"hello");
        clip.process(&message(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, &resp), &mut prov);
        assert_eq!(prov.saved.len(), 1);
        assert_eq!(prov.saved[0].0, "a.txt");
        assert_eq!(prov.saved[0].1, b"hello");
    }
}
