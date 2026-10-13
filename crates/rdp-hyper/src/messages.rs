//! Messages carried by the reliable channel ([`crate::reliable`]).

/// The reliable channel runs two independent lanes so small, latency-sensitive
/// messages (the pointer) never queue behind a clipboard image or a file.
/// Every `Kind::Reliable` body starts with the lane byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Urgent = 0,
    Bulk = 1,
}

impl Lane {
    pub const ALL: [Lane; 2] = [Lane::Urgent, Lane::Bulk];

    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Urgent),
            1 => Some(Self::Bulk),
            _ => None,
        }
    }
}

/// One message; each is delivered whole and in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Host → client: the pointer's shape (top-down RGBA8) and hotspot.
    CursorShape { width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: Vec<u8> },
    /// Host → client: the pointer was hidden (games, video) or shown.
    CursorVisible(bool),
    /// Either way: the clipboard now holds this text.
    ClipboardText(String),
    /// Either way: the clipboard now holds this image (a Windows `CF_DIB`
    /// blob: BITMAPINFO + pixels).
    ClipboardImage(Vec<u8>),
    /// Either way: files were copied; their contents follow as `FileChunk`s.
    ClipboardFiles { transfer: u32, entries: Vec<FileEntry> },
    /// Part of file `index` of a `ClipboardFiles` transfer.
    FileChunk { transfer: u32, index: u32, offset: u64, data: Vec<u8> },
}

/// One copied file or folder; `name` is relative to the copy (backslashes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

impl Message {
    /// Which lane carries this message.
    pub fn lane(&self) -> Lane {
        match self {
            Self::CursorShape { .. } | Self::CursorVisible(_) => Lane::Urgent,
            _ => Lane::Bulk,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Self::CursorShape { width, height, hot_x, hot_y, rgba } => {
                o.push(1);
                for v in [width, height, hot_x, hot_y] {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.extend_from_slice(rgba);
            }
            Self::CursorVisible(v) => o.extend_from_slice(&[2, *v as u8]),
            Self::ClipboardText(t) => {
                o.push(3);
                o.extend_from_slice(t.as_bytes());
            }
            Self::ClipboardImage(dib) => {
                o.push(4);
                o.extend_from_slice(dib);
            }
            Self::ClipboardFiles { transfer, entries } => {
                o.push(5);
                o.extend_from_slice(&transfer.to_le_bytes());
                o.extend_from_slice(&serde_json::to_vec(entries).unwrap_or_default());
            }
            Self::FileChunk { transfer, index, offset, data } => {
                o.push(6);
                o.extend_from_slice(&transfer.to_le_bytes());
                o.extend_from_slice(&index.to_le_bytes());
                o.extend_from_slice(&offset.to_le_bytes());
                o.extend_from_slice(data);
            }
        }
        o
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        Some(match *b.first()? {
            1 => {
                let (width, height) = (u16_at(1)?, u16_at(3)?);
                let rgba = b.get(9..)?.to_vec();
                if rgba.len() != width as usize * height as usize * 4 {
                    return None;
                }
                Self::CursorShape { width, height, hot_x: u16_at(5)?, hot_y: u16_at(7)?, rgba }
            }
            2 => Self::CursorVisible(*b.get(1)? != 0),
            3 => Self::ClipboardText(String::from_utf8(b[1..].to_vec()).ok()?),
            4 => Self::ClipboardImage(b[1..].to_vec()),
            5 => Self::ClipboardFiles {
                transfer: u32::from_le_bytes(b.get(1..5)?.try_into().ok()?),
                entries: serde_json::from_slice(b.get(5..)?).ok()?,
            },
            6 => Self::FileChunk {
                transfer: u32::from_le_bytes(b.get(1..5)?.try_into().ok()?),
                index: u32::from_le_bytes(b.get(5..9)?.try_into().ok()?),
                offset: u64::from_le_bytes(b.get(9..17)?.try_into().ok()?),
                data: b.get(17..)?.to_vec(),
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for m in [
            Message::CursorShape { width: 2, height: 1, hot_x: 1, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] },
            Message::CursorVisible(false),
            Message::ClipboardText("héllo ☺".into()),
            Message::ClipboardImage(vec![9; 100]),
            Message::ClipboardFiles {
                transfer: 3,
                entries: vec![FileEntry { name: "dir\\a.txt".into(), size: 5, is_dir: false }],
            },
            Message::FileChunk { transfer: 3, index: 0, offset: 0, data: b"hello".to_vec() },
        ] {
            assert_eq!(Message::parse(&m.to_bytes()), Some(m));
        }
    }

    #[test]
    fn a_cursor_with_the_wrong_pixel_count_is_rejected() {
        let mut b = Message::CursorShape { width: 2, height: 2, hot_x: 0, hot_y: 0, rgba: vec![0; 16] }.to_bytes();
        b.pop();
        assert_eq!(Message::parse(&b), None);
    }
}
