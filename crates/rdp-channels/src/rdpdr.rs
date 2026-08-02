//! Device redirection (MS-RDPEFS / RDPDR) over the static `rdpdr` channel:
//! the initialization handshake plus drive (file system) redirection.
//!
//! Handshake: server Announce → client Announce Reply + Client Name; server
//! Capability Request → client Capability Response + Device List Announce
//! (re-announced on User Logged On). If a share is configured, the device list
//! includes one file-system device, and the server then drives file operations
//! as Device I/O Requests (Create/Read/Write/Close/QueryInformation/
//! QueryDirectory/QueryVolumeInformation/SetInformation), which this module
//! services against the local directory with `std::fs` and answers with Device
//! I/O Completions. No `unsafe`, fully testable against a temp dir.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::SystemTime;

/// `RDPDR_CTYP_CORE` — the core (non-printing) component.
pub const RDPDR_CTYP_CORE: u16 = 0x4472;

const PAKID_CORE_SERVER_ANNOUNCE: u16 = 0x496E;
const PAKID_CORE_CLIENTID_CONFIRM: u16 = 0x4343;
const PAKID_CORE_CLIENT_NAME: u16 = 0x434E;
const PAKID_CORE_SERVER_CAPABILITY: u16 = 0x5350;
const PAKID_CORE_CLIENT_CAPABILITY: u16 = 0x4350;
const PAKID_CORE_DEVICELIST_ANNOUNCE: u16 = 0x4441;
const PAKID_CORE_SERVER_USER_LOGGEDON: u16 = 0x554C;
const PAKID_CORE_DEVICE_IOREQUEST: u16 = 0x4952;
const PAKID_CORE_DEVICE_IOCOMPLETION: u16 = 0x4943;

const CAP_GENERAL_TYPE: u16 = 0x0001;
const GENERAL_CAPABILITY_VERSION_02: u32 = 0x0000_0002;
const RDPDR_EXTENDED_PDU: u32 = 0x0000_0007;
const RDPDR_IOCODE1_ALL: u32 = 0x0000_FFFF;

const RDPDR_DTYP_FILESYSTEM: u32 = 0x0000_0003;
const RDPDR_DTYP_PRINT: u32 = 0x0000_0004;

/// Printer announce flag: this is the client's default printer.
const RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER: u32 = 0x0000_0004;

// IRP major functions.
const IRP_MJ_CREATE: u32 = 0x00;
const IRP_MJ_CLOSE: u32 = 0x02;
const IRP_MJ_READ: u32 = 0x03;
const IRP_MJ_WRITE: u32 = 0x04;
const IRP_MJ_QUERY_INFORMATION: u32 = 0x05;
const IRP_MJ_SET_INFORMATION: u32 = 0x06;
const IRP_MJ_QUERY_VOLUME_INFORMATION: u32 = 0x0A;
const IRP_MJ_DIRECTORY_CONTROL: u32 = 0x0C;
const IRP_MJ_DEVICE_CONTROL: u32 = 0x0E;
const IRP_MN_QUERY_DIRECTORY: u32 = 0x01;

// CreateDisposition.
const FILE_SUPERSEDE: u32 = 0;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_OVERWRITE: u32 = 4;
const FILE_OVERWRITE_IF: u32 = 5;
// CreateOptions.
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
// Create Information response.
const FILE_OPENED: u8 = 1;
const FILE_CREATED: u8 = 2;
const FILE_OVERWRITTEN: u8 = 3;

// NTSTATUS.
const STATUS_SUCCESS: u32 = 0x0000_0000;
const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;
const STATUS_NO_SUCH_FILE: u32 = 0xC000_000F;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;

// File attributes.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;

// File information classes (query/set).
const FILE_BASIC_INFORMATION: u32 = 4;
const FILE_STANDARD_INFORMATION: u32 = 5;
const FILE_ATTRIBUTE_TAG_INFORMATION: u32 = 35;
const FILE_END_OF_FILE_INFORMATION: u32 = 20;
const FILE_DISPOSITION_INFORMATION: u32 = 13;
const FILE_RENAME_INFORMATION: u32 = 10;
const FILE_ALLOCATION_INFORMATION: u32 = 19;
// Directory enumeration classes. (FileDirectoryInformation = 1 is the implicit
// base layout in `directory_entry`, so it needs no named constant.)
const FILE_FULL_DIRECTORY_INFORMATION: u32 = 2;
const FILE_BOTH_DIRECTORY_INFORMATION: u32 = 3;
const FILE_NAMES_INFORMATION: u32 = 12;
// Volume information classes.
const FILE_FS_VOLUME_INFORMATION: u32 = 1;
const FILE_FS_SIZE_INFORMATION: u32 = 3;
const FILE_FS_DEVICE_INFORMATION: u32 = 4;
const FILE_FS_ATTRIBUTE_INFORMATION: u32 = 5;
const FILE_FS_FULL_SIZE_INFORMATION: u32 = 7;

const CLIENT_NAME: &str = "RDPIO";
/// The name shown for the redirected drive ("RDPIO" → "RDPIO on <client>").
const DRIVE_DOS_NAME: &str = "RDPIO";

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?,
    ]))
}
#[inline]
fn u64le(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes([
        *b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?,
        *b.get(o + 4)?, *b.get(o + 5)?, *b.get(o + 6)?, *b.get(o + 7)?,
    ]))
}

/// Windows FILETIME (100ns ticks since 1601) from a `SystemTime`.
fn filetime(t: Option<SystemTime>) -> u64 {
    let Some(st) = t else { return 0 };
    match st.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() + 11_644_473_600) * 10_000_000 + d.subsec_nanos() as u64 / 100,
        Err(_) => 0,
    }
}

/// UTF-16LE → String, stopping at NUL.
fn from_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// String → UTF-16LE bytes (no NUL).
fn to_utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Start an RDPDR PDU header (Component + PacketId).
fn header(packet_id: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&RDPDR_CTYP_CORE.to_le_bytes());
    v.extend_from_slice(&packet_id.to_le_bytes());
    v
}

/// An open file/dir on the redirected drive.
struct OpenFile {
    path: PathBuf,
    is_dir: bool,
    file: Option<File>,
    /// Cached directory entry names for an enumeration in progress.
    dir_entries: Option<Vec<String>>,
    dir_pos: usize,
    /// Marked for deletion on close (FileDispositionInformation).
    delete_on_close: bool,
}

/// A redirected local directory exposed as a file-system device.
struct DriveDevice {
    root: PathBuf,
    device_id: u32,
    /// The name the session shows for this drive (≤7 ASCII chars — the
    /// DEVICE_ANNOUNCE PreferredDosName limit), e.g. `"C"` for `C:\`.
    dos_name: String,
    files: HashMap<u32, OpenFile>,
    next_file_id: u32,
}

impl DriveDevice {
    fn new(root: PathBuf, device_id: u32, dos_name: String) -> Self {
        Self {
            root,
            device_id,
            dos_name,
            files: HashMap::new(),
            next_file_id: 1,
        }
    }

    /// Map a wire path ("\dir\file", backslashes) to a local path under `root`,
    /// refusing parent-directory traversal.
    fn resolve(&self, wire: &str) -> Option<PathBuf> {
        resolve_path(&self.root, wire)
    }

    /// Dispatch one Device I/O Request body (after the rdpdr header), returning
    /// (io_status, response payload) for the Device I/O Completion.
    fn io(&mut self, body: &[u8]) -> (u32, u32, u32, Vec<u8>) {
        // DR_DEVICE_IOREQUEST: DeviceId, FileId, CompletionId, Major, Minor.
        let file_id = u32le(body, 4).unwrap_or(0);
        let completion_id = u32le(body, 8).unwrap_or(0);
        let major = u32le(body, 12).unwrap_or(0);
        let minor = u32le(body, 16).unwrap_or(0);
        let params = body.get(20..).unwrap_or(&[]);

        let (status, payload) = match major {
            IRP_MJ_CREATE => self.create(params),
            IRP_MJ_CLOSE => self.close(file_id),
            IRP_MJ_READ => self.read(file_id, params),
            IRP_MJ_WRITE => self.write(file_id, params),
            IRP_MJ_QUERY_INFORMATION => self.query_info(file_id, params),
            IRP_MJ_SET_INFORMATION => self.set_info(file_id, params),
            IRP_MJ_QUERY_VOLUME_INFORMATION => self.query_volume(params),
            IRP_MJ_DIRECTORY_CONTROL if minor == IRP_MN_QUERY_DIRECTORY => {
                self.query_directory(file_id, params)
            }
            IRP_MJ_DIRECTORY_CONTROL => (STATUS_SUCCESS, vec![0, 0, 0, 0]), // notify-change: empty
            IRP_MJ_DEVICE_CONTROL => (STATUS_NOT_SUPPORTED, vec![0; 4]),
            _ => (STATUS_NOT_SUPPORTED, Vec::new()),
        };
        (completion_id, major, status, payload)
    }

    fn create(&mut self, p: &[u8]) -> (u32, Vec<u8>) {
        // DesiredAccess(4), AllocationSize(8), FileAttributes(4), SharedAccess(4),
        // CreateDisposition(4), CreateOptions(4), PathLength(4), Path.
        let disposition = u32le(p, 20).unwrap_or(FILE_OPEN);
        let options = u32le(p, 24).unwrap_or(0);
        let path_len = u32le(p, 28).unwrap_or(0) as usize;
        let wire = from_utf16(p.get(32..32 + path_len).unwrap_or(&[]));
        let Some(path) = self.resolve(&wire) else {
            return (STATUS_ACCESS_DENIED, vec![0; 5]);
        };

        let want_dir = options & FILE_DIRECTORY_FILE != 0;
        let exists = path.exists();
        let is_dir = path.is_dir() || (want_dir && !exists);

        // Map the NT disposition onto std::fs.
        let information;
        if is_dir {
            match disposition {
                FILE_CREATE | FILE_OPEN_IF | FILE_OVERWRITE_IF | FILE_SUPERSEDE => {
                    if !exists {
                        if fs::create_dir(&path).is_err() {
                            return (STATUS_UNSUCCESSFUL, vec![0; 5]);
                        }
                        information = FILE_CREATED;
                    } else {
                        information = FILE_OPENED;
                    }
                }
                _ => {
                    if !exists {
                        return (STATUS_NO_SUCH_FILE, vec![0; 5]);
                    }
                    information = FILE_OPENED;
                }
            }
        } else {
            let mut opts = OpenOptions::new();
            opts.read(true).write(true);
            match disposition {
                FILE_OPEN => {
                    if !exists {
                        return (STATUS_NO_SUCH_FILE, vec![0; 5]);
                    }
                    information = FILE_OPENED;
                }
                FILE_CREATE => {
                    if exists {
                        return (STATUS_OBJECT_NAME_COLLISION, vec![0; 5]);
                    }
                    opts.create_new(true);
                    information = FILE_CREATED;
                }
                FILE_OPEN_IF => {
                    opts.create(true);
                    information = if exists { FILE_OPENED } else { FILE_CREATED };
                }
                FILE_OVERWRITE => {
                    if !exists {
                        return (STATUS_NO_SUCH_FILE, vec![0; 5]);
                    }
                    opts.truncate(true);
                    information = FILE_OVERWRITTEN;
                }
                FILE_OVERWRITE_IF | FILE_SUPERSEDE => {
                    opts.create(true).truncate(true);
                    information = if exists { FILE_OVERWRITTEN } else { FILE_CREATED };
                }
                _ => {
                    opts.create(true);
                    information = FILE_OPENED;
                }
            }
            // Read-only-friendly: if write open fails (e.g. read-only file),
            // retry read-only so browsing still works.
            let file = opts.open(&path).or_else(|_| File::open(&path));
            match file {
                Ok(f) => {
                    let id = self.next_file_id;
                    self.next_file_id += 1;
                    self.files.insert(id, OpenFile {
                        path,
                        is_dir: false,
                        file: Some(f),
                        dir_entries: None,
                        dir_pos: 0,
                        delete_on_close: false,
                    });
                    let mut out = id.to_le_bytes().to_vec();
                    out.push(information);
                    return (STATUS_SUCCESS, out);
                }
                Err(_) => return (STATUS_OBJECT_NAME_NOT_FOUND, vec![0; 5]),
            }
        }

        // Directory handle.
        let id = self.next_file_id;
        self.next_file_id += 1;
        self.files.insert(id, OpenFile {
            path,
            is_dir: true,
            file: None,
            dir_entries: None,
            dir_pos: 0,
            delete_on_close: false,
        });
        let mut out = id.to_le_bytes().to_vec();
        out.push(information);
        (STATUS_SUCCESS, out)
    }

    fn close(&mut self, file_id: u32) -> (u32, Vec<u8>) {
        if let Some(f) = self.files.remove(&file_id) {
            if f.delete_on_close {
                let _ = if f.is_dir {
                    fs::remove_dir_all(&f.path)
                } else {
                    fs::remove_file(&f.path)
                };
            }
        }
        (STATUS_SUCCESS, vec![0; 5]) // padding
    }

    fn read(&mut self, file_id: u32, p: &[u8]) -> (u32, Vec<u8>) {
        let length = u32le(p, 0).unwrap_or(0) as usize;
        let offset = u64le(p, 4).unwrap_or(0);
        let Some(of) = self.files.get_mut(&file_id) else {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        };
        let Some(file) = of.file.as_mut() else {
            return (STATUS_ACCESS_DENIED, vec![0; 4]);
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        }
        let mut buf = vec![0u8; length];
        let n = file.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        let mut out = (n as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&buf);
        (STATUS_SUCCESS, out)
    }

    fn write(&mut self, file_id: u32, p: &[u8]) -> (u32, Vec<u8>) {
        let length = u32le(p, 0).unwrap_or(0) as usize;
        let offset = u64le(p, 4).unwrap_or(0);
        // Layout: Length(4), Offset(8), Padding(20), WriteData.
        let data = p.get(32..32 + length).unwrap_or(&[]);
        let Some(of) = self.files.get_mut(&file_id) else {
            return (STATUS_UNSUCCESSFUL, vec![0; 5]);
        };
        let Some(file) = of.file.as_mut() else {
            return (STATUS_ACCESS_DENIED, vec![0; 5]);
        };
        if file.seek(SeekFrom::Start(offset)).is_err() || file.write_all(data).is_err() {
            return (STATUS_ACCESS_DENIED, vec![0; 5]);
        }
        let mut out = (data.len() as u32).to_le_bytes().to_vec();
        out.push(0); // Padding
        (STATUS_SUCCESS, out)
    }

    fn query_info(&mut self, file_id: u32, p: &[u8]) -> (u32, Vec<u8>) {
        let class = u32le(p, 0).unwrap_or(0);
        let Some(of) = self.files.get(&file_id) else {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        };
        let Ok(md) = fs::metadata(&of.path) else {
            return (STATUS_OBJECT_NAME_NOT_FOUND, vec![0; 4]);
        };
        let attrs = attributes(&md);
        let buf = match class {
            FILE_BASIC_INFORMATION => basic_information(&md, attrs),
            FILE_STANDARD_INFORMATION => standard_information(&md),
            FILE_ATTRIBUTE_TAG_INFORMATION => {
                let mut b = attrs.to_le_bytes().to_vec();
                b.extend_from_slice(&0u32.to_le_bytes()); // ReparseTag
                b
            }
            _ => return (STATUS_NOT_SUPPORTED, vec![0; 4]),
        };
        (STATUS_SUCCESS, length_prefixed(&buf))
    }

    fn set_info(&mut self, file_id: u32, p: &[u8]) -> (u32, Vec<u8>) {
        let class = u32le(p, 0).unwrap_or(0);
        // FsInformationClass(4), Length(4), padding(24), SetBuffer.
        let set_len = u32le(p, 4).unwrap_or(0) as usize;
        let buf = p.get(32..32 + set_len).unwrap_or(&[]);
        let Some(of) = self.files.get_mut(&file_id) else {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        };
        let status = match class {
            FILE_END_OF_FILE_INFORMATION | FILE_ALLOCATION_INFORMATION => {
                let eof = u64le(buf, 0).unwrap_or(0);
                match of.file.as_ref() {
                    Some(f) if f.set_len(eof).is_ok() => STATUS_SUCCESS,
                    _ => STATUS_ACCESS_DENIED,
                }
            }
            FILE_DISPOSITION_INFORMATION => {
                of.delete_on_close = true;
                STATUS_SUCCESS
            }
            FILE_RENAME_INFORMATION => {
                // RootDirectory(8)? layout: ReplaceIfExists(1), pad(7?), RootDirectory(8),
                // FileNameLength(4), FileName. We read from a fixed offset used by RDP.
                let name_len = u32le(buf, 6).unwrap_or(0) as usize;
                let new_wire = from_utf16(buf.get(10..10 + name_len).unwrap_or(&[]));
                // Resolve via the free fn against `self.root` (a field disjoint
                // from `self.files`, which `of` borrows).
                match resolve_path(&self.root, &new_wire) {
                    Some(dest) if fs::rename(&of.path, &dest).is_ok() => {
                        of.path = dest;
                        STATUS_SUCCESS
                    }
                    _ => STATUS_ACCESS_DENIED,
                }
            }
            FILE_BASIC_INFORMATION => STATUS_SUCCESS, // times/attrs: accept as no-op
            _ => STATUS_NOT_SUPPORTED,
        };
        (status, set_len_response(set_len))
    }

    fn query_volume(&mut self, p: &[u8]) -> (u32, Vec<u8>) {
        let class = u32le(p, 0).unwrap_or(0);
        let label = to_utf16(&self.dos_name);
        let fs_name = to_utf16("NTFS");
        let buf = match class {
            FILE_FS_VOLUME_INFORMATION => {
                let mut b = Vec::new();
                b.extend_from_slice(&0u64.to_le_bytes()); // VolumeCreationTime
                b.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // VolumeSerialNumber
                b.extend_from_slice(&(label.len() as u32).to_le_bytes());
                b.push(0); // SupportsObjects
                b.push(0); // Reserved
                b.extend_from_slice(&label);
                b
            }
            FILE_FS_SIZE_INFORMATION => {
                let mut b = Vec::new();
                b.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // TotalAllocationUnits
                b.extend_from_slice(&0x000F_0000u64.to_le_bytes()); // AvailableAllocationUnits
                b.extend_from_slice(&8u32.to_le_bytes()); // SectorsPerAllocationUnit
                b.extend_from_slice(&512u32.to_le_bytes()); // BytesPerSector
                b
            }
            FILE_FS_FULL_SIZE_INFORMATION => {
                let mut b = Vec::new();
                b.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // TotalAllocationUnits
                b.extend_from_slice(&0x000F_0000u64.to_le_bytes()); // CallerAvailable
                b.extend_from_slice(&0x000F_0000u64.to_le_bytes()); // ActualAvailable
                b.extend_from_slice(&8u32.to_le_bytes());
                b.extend_from_slice(&512u32.to_le_bytes());
                b
            }
            FILE_FS_ATTRIBUTE_INFORMATION => {
                let mut b = Vec::new();
                b.extend_from_slice(&0x0000_0007u32.to_le_bytes()); // FileSystemAttributes
                b.extend_from_slice(&255u32.to_le_bytes()); // MaximumComponentNameLength
                b.extend_from_slice(&(fs_name.len() as u32).to_le_bytes());
                b.extend_from_slice(&fs_name);
                b
            }
            FILE_FS_DEVICE_INFORMATION => {
                let mut b = Vec::new();
                b.extend_from_slice(&0x0000_0007u32.to_le_bytes()); // DeviceType = FILE_DEVICE_DISK
                b.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
                b
            }
            _ => return (STATUS_NOT_SUPPORTED, vec![0; 4]),
        };
        (STATUS_SUCCESS, length_prefixed(&buf))
    }

    fn query_directory(&mut self, file_id: u32, p: &[u8]) -> (u32, Vec<u8>) {
        let class = u32le(p, 0).unwrap_or(FILE_BOTH_DIRECTORY_INFORMATION);
        let initial = p.get(4).copied().unwrap_or(0) != 0;
        let Some(of) = self.files.get_mut(&file_id) else {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        };
        if !of.is_dir {
            return (STATUS_UNSUCCESSFUL, vec![0; 4]);
        }
        if initial || of.dir_entries.is_none() {
            let mut names = vec![".".to_string(), "..".to_string()];
            if let Ok(rd) = fs::read_dir(&of.path) {
                for e in rd.flatten() {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
            of.dir_entries = Some(names);
            of.dir_pos = 0;
        }
        let entries = of.dir_entries.as_ref().unwrap();
        if of.dir_pos >= entries.len() {
            return (STATUS_NO_MORE_FILES, vec![0; 4]);
        }
        let name = entries[of.dir_pos].clone();
        of.dir_pos += 1;
        let entry_path = of.path.join(&name);
        let md = fs::metadata(&entry_path).ok();
        let buf = directory_entry(class, &name, md.as_ref());
        (STATUS_SUCCESS, length_prefixed(&buf))
    }

    /// Build this device's DEVICE_ANNOUNCE entry (file-system device).
    fn announce_entry(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&RDPDR_DTYP_FILESYSTEM.to_le_bytes());
        v.extend_from_slice(&self.device_id.to_le_bytes());
        let mut dos = [0u8; 8];
        for (i, b) in self.dos_name.bytes().take(7).enumerate() {
            dos[i] = b;
        }
        v.extend_from_slice(&dos);
        // DeviceData = the share name (null-terminated UTF-16); length-prefixed.
        let data = {
            let mut d = to_utf16(&self.dos_name);
            d.extend_from_slice(&[0, 0]);
            d
        };
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }
}

/// The DOS name a shared path is announced under: `"C"` for a drive root
/// (`C:\`), else the last path component squeezed to the 7 ASCII chars the
/// DEVICE_ANNOUNCE PreferredDosName field allows, falling back to `"RDPIO"`.
pub fn dos_name_for(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches(['\\', '/']);
    // Bare drive root, e.g. "C:".
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        return trimmed[..1].to_ascii_uppercase();
    }
    let last = trimmed.rsplit(['\\', '/']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(7)
        .collect::<String>()
        .to_ascii_uppercase();
    if cleaned.is_empty() {
        DRIVE_DOS_NAME.to_string()
    } else {
        cleaned
    }
}

/// Map a wire path ("\dir\file", backslashes) to a local path under `root`,
/// refusing parent-directory traversal.
fn resolve_path(root: &std::path::Path, wire: &str) -> Option<PathBuf> {
    let mut p = root.to_path_buf();
    for comp in wire.replace('\\', "/").split('/') {
        match comp {
            "" | "." => {}
            ".." => return None,
            other => p.push(other),
        }
    }
    Some(p)
}

/// Windows file attributes from std metadata.
fn attributes(md: &fs::Metadata) -> u32 {
    let mut a = if md.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if md.permissions().readonly() {
        a |= FILE_ATTRIBUTE_READONLY;
    }
    a
}

fn basic_information(md: &fs::Metadata, attrs: u32) -> Vec<u8> {
    let c = filetime(md.created().ok());
    let a = filetime(md.accessed().ok());
    let w = filetime(md.modified().ok());
    let mut b = Vec::new();
    b.extend_from_slice(&c.to_le_bytes()); // CreationTime
    b.extend_from_slice(&a.to_le_bytes()); // LastAccessTime
    b.extend_from_slice(&w.to_le_bytes()); // LastWriteTime
    b.extend_from_slice(&w.to_le_bytes()); // ChangeTime
    b.extend_from_slice(&attrs.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // Reserved
    b
}

fn standard_information(md: &fs::Metadata) -> Vec<u8> {
    let size = md.len();
    let mut b = Vec::new();
    b.extend_from_slice(&size.to_le_bytes()); // AllocationSize
    b.extend_from_slice(&size.to_le_bytes()); // EndOfFile
    b.extend_from_slice(&1u32.to_le_bytes()); // NumberOfLinks
    b.push(0); // DeletePending
    b.push(if md.is_dir() { 1 } else { 0 }); // Directory
    b.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    b
}

/// Build a single directory-enumeration entry of the requested class.
fn directory_entry(class: u32, name: &str, md: Option<&fs::Metadata>) -> Vec<u8> {
    let name16 = to_utf16(name);
    let (size, attrs, c, a, w) = match md {
        Some(m) => (
            m.len(),
            attributes(m),
            filetime(m.created().ok()),
            filetime(m.accessed().ok()),
            filetime(m.modified().ok()),
        ),
        None => (0, FILE_ATTRIBUTE_ARCHIVE, 0, 0, 0),
    };
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset (single entry)
    b.extend_from_slice(&0u32.to_le_bytes()); // FileIndex
    if class == FILE_NAMES_INFORMATION {
        b.extend_from_slice(&(name16.len() as u32).to_le_bytes());
        b.extend_from_slice(&name16);
        return b;
    }
    // Directory / FullDirectory / BothDirectory share this prefix.
    b.extend_from_slice(&c.to_le_bytes()); // CreationTime
    b.extend_from_slice(&a.to_le_bytes()); // LastAccessTime
    b.extend_from_slice(&w.to_le_bytes()); // LastWriteTime
    b.extend_from_slice(&w.to_le_bytes()); // ChangeTime
    b.extend_from_slice(&size.to_le_bytes()); // EndOfFile
    b.extend_from_slice(&size.to_le_bytes()); // AllocationSize
    b.extend_from_slice(&attrs.to_le_bytes()); // FileAttributes
    b.extend_from_slice(&(name16.len() as u32).to_le_bytes()); // FileNameLength
    if class == FILE_FULL_DIRECTORY_INFORMATION || class == FILE_BOTH_DIRECTORY_INFORMATION {
        b.extend_from_slice(&0u32.to_le_bytes()); // EaSize
    }
    if class == FILE_BOTH_DIRECTORY_INFORMATION {
        b.push(0); // ShortNameLength
        b.push(0); // Reserved
        b.extend_from_slice(&[0u8; 24]); // ShortName
    }
    // class == FILE_DIRECTORY_INFORMATION uses exactly the base layout above.
    b.extend_from_slice(&name16);
    b
}

/// Prefix `buf` with its 32-bit length (the common completion payload shape).
fn length_prefixed(buf: &[u8]) -> Vec<u8> {
    let mut v = (buf.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(buf);
    v
}

/// SET_INFORMATION completion payload: just the echoed length.
fn set_len_response(len: usize) -> Vec<u8> {
    (len as u32).to_le_bytes().to_vec()
}

/// Spools redirected print jobs to a local printer. Abstracted so the protocol
/// is testable; the platform backs it with the Win32 print spooler.
pub trait PrinterSink: Send {
    /// Begin a print job (the server opened the printer device). Returns whether
    /// the job started — `false` makes the device fail the create cleanly.
    fn start_job(&mut self) -> bool;
    /// Append raw spool data (the server-rendered print stream).
    fn write(&mut self, data: &[u8]);
    /// Finish the current print job (the server closed the device).
    fn end_job(&mut self);
}

/// A [`PrinterSink`] that discards print jobs (the default until a real spooler
/// is installed). The protocol still completes so the server sees the printer.
pub struct NullPrinter;
impl PrinterSink for NullPrinter {
    fn start_job(&mut self) -> bool {
        true
    }
    fn write(&mut self, _data: &[u8]) {}
    fn end_job(&mut self) {}
}

/// A redirected printer (MS-RDPEPC): the server renders print jobs to the
/// announced driver and streams the raw spool data as device Writes, which we
/// hand to the local spooler.
struct PrinterDevice {
    device_id: u32,
    /// Display name shown in the remote session's printer list.
    print_name: String,
    /// The driver the server renders with (must exist server-side).
    driver_name: String,
    sink: Box<dyn PrinterSink>,
    /// Whether a job is currently open (between Create and Close).
    job_open: bool,
}

impl PrinterDevice {
    fn new(device_id: u32, print_name: String, driver_name: String, sink: Box<dyn PrinterSink>) -> Self {
        Self {
            device_id,
            print_name,
            driver_name,
            sink,
            job_open: false,
        }
    }

    /// Dispatch one Device I/O Request body, returning
    /// (completion_id, major, io_status, response payload).
    fn io(&mut self, body: &[u8]) -> (u32, u32, u32, Vec<u8>) {
        let completion_id = u32le(body, 8).unwrap_or(0);
        let major = u32le(body, 12).unwrap_or(0);
        let params = body.get(20..).unwrap_or(&[]);
        let (status, payload) = match major {
            IRP_MJ_CREATE => {
                // Start a print job; reply with a FileId (DR_CREATE_RSP).
                self.job_open = self.sink.start_job();
                let status = if self.job_open { STATUS_SUCCESS } else { STATUS_NOT_SUPPORTED };
                let mut p = Vec::new();
                p.extend_from_slice(&1u32.to_le_bytes()); // FileId
                p.push(0); // Information
                (status, p)
            }
            IRP_MJ_WRITE => {
                // DR_WRITE_REQUEST: Length(4), Offset(8), Padding(20), WriteData.
                let length = u32le(params, 0).unwrap_or(0);
                let data = params.get(32..).unwrap_or(&[]);
                let take = (length as usize).min(data.len());
                if self.job_open {
                    self.sink.write(&data[..take]);
                }
                // DR_WRITE_RSP: Length(4), Padding(1).
                let mut p = Vec::new();
                p.extend_from_slice(&(take as u32).to_le_bytes());
                p.push(0);
                (STATUS_SUCCESS, p)
            }
            IRP_MJ_CLOSE => {
                if self.job_open {
                    self.sink.end_job();
                    self.job_open = false;
                }
                (STATUS_SUCCESS, vec![0u8; 4])
            }
            _ => (STATUS_NOT_SUPPORTED, vec![0u8; 4]),
        };
        (completion_id, major, status, payload)
    }

    /// Build this printer's DEVICE_ANNOUNCE entry (MS-RDPEPC printer data).
    fn announce_entry(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&RDPDR_DTYP_PRINT.to_le_bytes());
        v.extend_from_slice(&self.device_id.to_le_bytes());
        // PreferredDosName: "PRN1" padded to 8 bytes.
        let mut dos = [0u8; 8];
        for (i, b) in b"PRN1".iter().enumerate() {
            dos[i] = *b;
        }
        v.extend_from_slice(&dos);

        // DeviceData = RDPDR_PRINTER_ANNOUNCE_DATA.
        let driver = {
            let mut d = to_utf16(&self.driver_name);
            d.extend_from_slice(&[0, 0]);
            d
        };
        let printn = {
            let mut d = to_utf16(&self.print_name);
            d.extend_from_slice(&[0, 0]);
            d
        };
        let mut data = Vec::new();
        data.extend_from_slice(&RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER.to_le_bytes()); // Flags
        data.extend_from_slice(&0u32.to_le_bytes()); // CodePage
        data.extend_from_slice(&0u32.to_le_bytes()); // PnPNameLen (none)
        data.extend_from_slice(&(driver.len() as u32).to_le_bytes()); // DriverNameLen
        data.extend_from_slice(&(printn.len() as u32).to_le_bytes()); // PrintNameLen
        data.extend_from_slice(&0u32.to_le_bytes()); // CachedFieldsLen
        // PnPName omitted (len 0), then DriverName, PrintName.
        data.extend_from_slice(&driver);
        data.extend_from_slice(&printn);

        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }
}

/// The device-redirection channel: init handshake + any number of shared
/// drives and an optional redirected printer.
#[derive(Default)]
pub struct RdpdrChannel {
    client_id: u32,
    drives: Vec<DriveDevice>,
    printer: Option<PrinterDevice>,
}

/// The printer's device id — clear of the drive ids, which grow from 1.
const PRINTER_DEVICE_ID: u32 = 100;

impl RdpdrChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Share `root` as a redirected drive under a name derived from the path
    /// (`"C"` for a drive root, else the folder name). See [`Self::add_drive`].
    pub fn set_drive(&mut self, root: PathBuf) {
        let name = dos_name_for(&root);
        self.add_drive(root, name);
    }

    /// Share `root` as an additional redirected drive announced as `dos_name`
    /// (truncated to the 7 ASCII chars the wire allows). Drives get sequential
    /// device ids starting at 1.
    pub fn add_drive(&mut self, root: PathBuf, dos_name: String) {
        let device_id = self.drives.len() as u32 + 1;
        self.drives.push(DriveDevice::new(root, device_id, dos_name));
    }

    /// Redirect a local printer. `print_name` is shown in the session;
    /// `driver_name` is the driver the server renders with; `sink` spools the
    /// returned job to the local printer.
    pub fn set_printer(&mut self, print_name: String, driver_name: String, sink: Box<dyn PrinterSink>) {
        self.printer = Some(PrinterDevice::new(
            PRINTER_DEVICE_ID,
            print_name,
            driver_name,
            sink,
        ));
    }

    /// Process one inbound RDPDR PDU, returning the responses to send.
    pub fn process(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        let (Some(component), Some(packet_id)) = (u16le(msg, 0), u16le(msg, 2)) else {
            return Vec::new();
        };
        if component != RDPDR_CTYP_CORE {
            return Vec::new();
        }
        match packet_id {
            PAKID_CORE_SERVER_ANNOUNCE => {
                self.client_id = u32le(msg, 8).unwrap_or(0);
                vec![self.announce_reply(), self.client_name(), self.device_list()]
            }
            PAKID_CORE_SERVER_CAPABILITY => {
                vec![self.capability_response(), self.device_list()]
            }
            PAKID_CORE_SERVER_USER_LOGGEDON => vec![self.device_list()],
            PAKID_CORE_DEVICE_IOREQUEST => self.device_io(msg),
            _ => Vec::new(),
        }
    }

    fn device_io(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        let device_id = u32le(msg, 4).unwrap_or(0);
        // Body starts after the 4-byte rdpdr header. Route to whichever device
        // the request targets (one of the drives, or the printer).
        let io = if let Some(d) = self.drives.iter_mut().find(|d| d.device_id == device_id) {
            Some(d.io(&msg[4..]))
        } else if self.printer.as_ref().is_some_and(|p| p.device_id == device_id) {
            self.printer.as_mut().map(|p| p.io(&msg[4..]))
        } else {
            None
        };
        let Some((completion_id, _major, status, payload)) = io else {
            return Vec::new();
        };
        let mut v = header(PAKID_CORE_DEVICE_IOCOMPLETION);
        v.extend_from_slice(&device_id.to_le_bytes());
        v.extend_from_slice(&completion_id.to_le_bytes());
        v.extend_from_slice(&status.to_le_bytes());
        v.extend_from_slice(&payload);
        vec![v]
    }

    fn announce_reply(&self) -> Vec<u8> {
        let mut v = header(PAKID_CORE_CLIENTID_CONFIRM);
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&12u16.to_le_bytes());
        v.extend_from_slice(&self.client_id.to_le_bytes());
        v
    }

    fn client_name(&self) -> Vec<u8> {
        let mut name = to_utf16(CLIENT_NAME);
        name.extend_from_slice(&[0, 0]);
        let mut v = header(PAKID_CORE_CLIENT_NAME);
        v.extend_from_slice(&1u32.to_le_bytes()); // UnicodeFlag
        v.extend_from_slice(&0u32.to_le_bytes()); // CodePage
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&name);
        v
    }

    fn capability_response(&self) -> Vec<u8> {
        let mut v = header(PAKID_CORE_CLIENT_CAPABILITY);
        v.extend_from_slice(&1u16.to_le_bytes()); // numCapabilities
        v.extend_from_slice(&0u16.to_le_bytes()); // pad
        v.extend_from_slice(&CAP_GENERAL_TYPE.to_le_bytes());
        v.extend_from_slice(&44u16.to_le_bytes());
        v.extend_from_slice(&GENERAL_CAPABILITY_VERSION_02.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // osType
        v.extend_from_slice(&0u32.to_le_bytes()); // osVersion
        v.extend_from_slice(&1u16.to_le_bytes()); // protocolMajor
        v.extend_from_slice(&12u16.to_le_bytes()); // protocolMinor
        v.extend_from_slice(&RDPDR_IOCODE1_ALL.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // ioCode2
        v.extend_from_slice(&RDPDR_EXTENDED_PDU.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // extraFlags1
        v.extend_from_slice(&0u32.to_le_bytes()); // extraFlags2
        v.extend_from_slice(&0u32.to_le_bytes()); // SpecialTypeDeviceCap
        v
    }

    fn device_list(&self) -> Vec<u8> {
        let mut entries = Vec::new();
        let mut count = 0u32;
        for d in &self.drives {
            entries.extend_from_slice(&d.announce_entry());
            count += 1;
        }
        if let Some(p) = self.printer.as_ref() {
            entries.extend_from_slice(&p.announce_entry());
            count += 1;
        }
        let mut v = header(PAKID_CORE_DEVICELIST_ANNOUNCE);
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&entries);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_pdu(packet_id: u16, body: &[u8]) -> Vec<u8> {
        let mut v = header(packet_id);
        v.extend_from_slice(body);
        v
    }
    fn pkt_id(m: &[u8]) -> u16 {
        u16::from_le_bytes([m[2], m[3]])
    }

    fn io_request(device_id: u32, file_id: u32, comp: u32, major: u32, minor: u32, params: &[u8]) -> Vec<u8> {
        let mut v = header(PAKID_CORE_DEVICE_IOREQUEST);
        v.extend_from_slice(&device_id.to_le_bytes());
        v.extend_from_slice(&file_id.to_le_bytes());
        v.extend_from_slice(&comp.to_le_bytes());
        v.extend_from_slice(&major.to_le_bytes());
        v.extend_from_slice(&minor.to_le_bytes());
        v.extend_from_slice(params);
        v
    }

    fn create_params(disposition: u32, options: u32, wire_path: &str) -> Vec<u8> {
        let name = {
            let mut n = to_utf16(wire_path);
            n.extend_from_slice(&[0, 0]);
            n
        };
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_le_bytes()); // DesiredAccess
        p.extend_from_slice(&0u64.to_le_bytes()); // AllocationSize
        p.extend_from_slice(&0u32.to_le_bytes()); // FileAttributes
        p.extend_from_slice(&0u32.to_le_bytes()); // SharedAccess
        p.extend_from_slice(&disposition.to_le_bytes());
        p.extend_from_slice(&options.to_le_bytes());
        p.extend_from_slice(&(name.len() as u32).to_le_bytes());
        p.extend_from_slice(&name);
        p
    }

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rdpio_rdpdr_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn handshake_with_drive_announces_one_device() {
        let mut r = RdpdrChannel::new();
        r.set_drive(tmp_dir());
        let mut body = vec![1, 0, 12, 0];
        body.extend_from_slice(&7u32.to_le_bytes());
        let out = r.process(&server_pdu(PAKID_CORE_SERVER_ANNOUNCE, &body));
        // reply + name + device list (with one device).
        assert_eq!(out.len(), 3);
        let dl = &out[2];
        assert_eq!(pkt_id(dl), PAKID_CORE_DEVICELIST_ANNOUNCE);
        assert_eq!(u32::from_le_bytes([dl[4], dl[5], dl[6], dl[7]]), 1); // DeviceCount
    }

    #[test]
    fn dos_names_derive_from_paths() {
        use std::path::Path;
        // Drive roots become their letter — how mstsc labels redirected drives.
        assert_eq!(dos_name_for(Path::new("C:\\")), "C");
        assert_eq!(dos_name_for(Path::new("z:/")), "Z");
        // Folders use the (squeezed) folder name, 7 chars max.
        assert_eq!(dos_name_for(Path::new("C:\\Users\\a\\Shared Stuff")), "SHAREDS");
        // Degenerate paths fall back to the classic share name.
        assert_eq!(dos_name_for(Path::new("/")), "RDPIO");
    }

    #[test]
    fn multiple_drives_announce_and_route_independently() {
        let dir_a = tmp_dir().join("a");
        let dir_b = tmp_dir().join("b");
        let _ = fs::create_dir_all(&dir_a);
        let _ = fs::create_dir_all(&dir_b);
        fs::write(dir_b.join("only-in-b.txt"), b"b").unwrap();
        let mut r = RdpdrChannel::new();
        r.add_drive(dir_a, "C".into());
        r.add_drive(dir_b, "D".into());

        let mut body = vec![1, 0, 12, 0];
        body.extend_from_slice(&7u32.to_le_bytes());
        let out = r.process(&server_pdu(PAKID_CORE_SERVER_ANNOUNCE, &body));
        let dl = &out[2];
        assert_eq!(u32::from_le_bytes([dl[4], dl[5], dl[6], dl[7]]), 2); // both drives

        // A create routed to device 2 opens the file that only exists in b.
        let out = r.process(&io_request(
            2,
            0,
            42,
            IRP_MJ_CREATE,
            0,
            &create_params(FILE_OPEN, 0, "\\only-in-b.txt"),
        ));
        assert_eq!(out.len(), 1);
        let comp = &out[0];
        assert_eq!(pkt_id(comp), PAKID_CORE_DEVICE_IOCOMPLETION);
        assert_eq!(u32::from_le_bytes([comp[4], comp[5], comp[6], comp[7]]), 2); // device id
        let status = u32::from_le_bytes([comp[12], comp[13], comp[14], comp[15]]);
        assert_eq!(status, STATUS_SUCCESS);
        // The same path on device 1 (empty dir) does not resolve to a file.
        let out = r.process(&io_request(
            1,
            0,
            43,
            IRP_MJ_CREATE,
            0,
            &create_params(FILE_OPEN, 0, "\\only-in-b.txt"),
        ));
        let status = u32::from_le_bytes([out[0][12], out[0][13], out[0][14], out[0][15]]);
        assert_ne!(status, STATUS_SUCCESS);
    }

    #[test]
    fn create_read_a_file_over_the_drive() {
        let dir = tmp_dir();
        fs::write(dir.join("hello.txt"), b"hello rdpio").unwrap();
        let mut r = RdpdrChannel::new();
        r.set_drive(dir);

        // CREATE (open existing).
        let out = r.process(&io_request(1, 0, 100, IRP_MJ_CREATE, 0, &create_params(FILE_OPEN, 0, "\\hello.txt")));
        let comp = &out[0];
        // header(4) + deviceId(4) + completionId(4) + ioStatus(4) + fileId(4) + info(1)
        assert_eq!(u32::from_le_bytes([comp[12], comp[13], comp[14], comp[15]]), STATUS_SUCCESS);
        let file_id = u32::from_le_bytes([comp[16], comp[17], comp[18], comp[19]]);

        // READ 11 bytes at offset 0.
        let mut rp = Vec::new();
        rp.extend_from_slice(&11u32.to_le_bytes()); // Length
        rp.extend_from_slice(&0u64.to_le_bytes()); // Offset
        rp.extend_from_slice(&[0u8; 20]); // padding
        let out = r.process(&io_request(1, file_id, 101, IRP_MJ_READ, 0, &rp));
        let comp = &out[0];
        assert_eq!(u32::from_le_bytes([comp[12], comp[13], comp[14], comp[15]]), STATUS_SUCCESS);
        let read_len = u32::from_le_bytes([comp[16], comp[17], comp[18], comp[19]]);
        assert_eq!(read_len, 11);
        assert_eq!(&comp[20..31], b"hello rdpio");
    }

    #[test]
    fn query_directory_lists_entries_then_no_more() {
        let dir = tmp_dir();
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut r = RdpdrChannel::new();
        r.set_drive(dir);
        // Open the root directory.
        let out = r.process(&io_request(1, 0, 1, IRP_MJ_CREATE, 0, &create_params(FILE_OPEN, FILE_DIRECTORY_FILE, "\\")));
        let file_id = u32::from_le_bytes([out[0][16], out[0][17], out[0][18], out[0][19]]);
        // Query directory: ., .., a.txt → 3 successful entries, then NO_MORE_FILES.
        let mut last_status = 0;
        let mut entries = 0;
        for i in 0..10 {
            let mut p = Vec::new();
            p.extend_from_slice(&FILE_BOTH_DIRECTORY_INFORMATION.to_le_bytes());
            p.push(if i == 0 { 1 } else { 0 }); // InitialQuery
            p.extend_from_slice(&0u32.to_le_bytes()); // PathLength
            p.extend_from_slice(&[0u8; 23]);
            let out = r.process(&io_request(1, file_id, 10 + i, IRP_MJ_DIRECTORY_CONTROL, IRP_MN_QUERY_DIRECTORY, &p));
            last_status = u32::from_le_bytes([out[0][12], out[0][13], out[0][14], out[0][15]]);
            if last_status == STATUS_SUCCESS {
                entries += 1;
            } else {
                break;
            }
        }
        assert_eq!(entries, 3);
        assert_eq!(last_status, STATUS_NO_MORE_FILES);
    }

    #[derive(Default)]
    struct MockPrinter {
        jobs: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        current: Vec<u8>,
    }
    impl PrinterSink for MockPrinter {
        fn start_job(&mut self) -> bool {
            self.current.clear();
            true
        }
        fn write(&mut self, data: &[u8]) {
            self.current.extend_from_slice(data);
        }
        fn end_job(&mut self) {
            self.jobs.lock().unwrap().push(std::mem::take(&mut self.current));
        }
    }

    #[test]
    fn printer_announced_and_spools_a_job() {
        let mut r = RdpdrChannel::new();
        let jobs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        r.set_printer(
            "Office Printer".into(),
            "Generic / Text Only".into(),
            Box::new(MockPrinter { jobs: jobs.clone(), current: Vec::new() }),
        );

        // The device list announces the printer (device type PRINT).
        let dl = r.device_list();
        assert_eq!(u32::from_le_bytes([dl[4], dl[5], dl[6], dl[7]]), 1); // one device
        assert_eq!(u32::from_le_bytes([dl[8], dl[9], dl[10], dl[11]]), RDPDR_DTYP_PRINT);

        // Create (start job) → Write (spool) → Close (finish). The printer's
        // device id sits clear of the (variable) drive ids.
        r.process(&io_request(PRINTER_DEVICE_ID, 0, 1, IRP_MJ_CREATE, 0, &[]));
        let mut wparams = Vec::new();
        wparams.extend_from_slice(&5u32.to_le_bytes()); // Length
        wparams.extend_from_slice(&0u64.to_le_bytes()); // Offset
        wparams.extend_from_slice(&[0u8; 20]); // Padding
        wparams.extend_from_slice(b"hello"); // WriteData
        let out = r.process(&io_request(PRINTER_DEVICE_ID, 1, 2, IRP_MJ_WRITE, 0, &wparams));
        assert_eq!(u32::from_le_bytes([out[0][12], out[0][13], out[0][14], out[0][15]]), STATUS_SUCCESS);
        r.process(&io_request(PRINTER_DEVICE_ID, 1, 3, IRP_MJ_CLOSE, 0, &[]));

        let jobs = jobs.lock().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0], b"hello");
    }
}
