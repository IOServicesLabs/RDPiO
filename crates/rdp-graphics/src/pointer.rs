//! Pointer (mouse cursor) updates for the legacy slow path (MS-RDPBCGR
//! 2.2.9.1.1.4, `TS_POINTER_PDU`).
//!
//! The server sends the cursor *shape*; an RDP client renders that shape at the
//! local mouse position (the local pointer drives position, so most sessions
//! never use the server-driven `Position` update). This module is sans-I/O: it
//! decodes a pointer PDU into a [`PointerUpdate`] and turns colour/new-pointer
//! shapes into tightly-packed top-down RGBA8 with a hotspot, ready for the
//! platform to realise as a cursor. The AND/XOR masks are bottom-up with each
//! scan line padded to a 2-byte boundary (per spec).

/// `TS_POINTER_PDU.messageType` values.
pub const TS_PTRMSGTYPE_SYSTEM: u16 = 0x0001;
pub const TS_PTRMSGTYPE_POSITION: u16 = 0x0003;
pub const TS_PTRMSGTYPE_COLOR: u16 = 0x0006;
pub const TS_PTRMSGTYPE_CACHED: u16 = 0x0007;
/// The "new" pointer: a colour pointer prefixed with an explicit `xorBpp`.
pub const TS_PTRMSGTYPE_POINTER: u16 = 0x0008;

/// `systemPointerType`: hide the pointer.
pub const SYSPTR_NULL: u32 = 0x0000_0000;
/// `systemPointerType`: show the default (OS) arrow.
pub const SYSPTR_DEFAULT: u32 = 0x0000_7F00;

/// A decoded cursor shape: top-down RGBA8 (`width*height*4` bytes) plus the
/// hotspot — the pixel within the image that tracks the mouse position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// What a pointer update asks the client to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerUpdate {
    /// Hide the cursor (`SYSPTR_NULL`).
    Hidden,
    /// Show the platform's default arrow (`SYSPTR_DEFAULT`).
    SystemDefault,
    /// Server-driven cursor move (rare; the local mouse normally drives this).
    Position { x: u16, y: u16 },
    /// Reuse a shape the server cached at `cache_index`.
    Cached { cache_index: u16 },
    /// A new shape, which the server also caches at `cache_index`.
    Shape {
        cache_index: u16,
        shape: CursorShape,
    },
}

#[inline]
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Bytes per scan line of a `bits`-wide-per-row mask, padded to 2 bytes.
#[inline]
fn padded_stride(bits: usize) -> usize {
    let bytes = bits.div_ceil(8);
    bytes + (bytes & 1)
}

/// One AND-mask bit (`1` = the spec's "transparent/AND" bit). Out-of-range
/// reads return `1` so a short/malformed mask yields transparency, not garbage.
#[inline]
fn and_bit(and: &[u8], row_start: usize, x: usize) -> u8 {
    match and.get(row_start + x / 8) {
        Some(byte) => (byte >> (7 - (x % 8))) & 1,
        None => 1,
    }
}

/// Parse a `TS_POINTER_PDU` body (starting at `messageType`).
pub fn parse_pointer_update(body: &[u8]) -> Option<PointerUpdate> {
    let message_type = u16le(body, 0)?;
    // body[2..4] = pad2Octets.
    let data = body.get(4..)?;
    match message_type {
        TS_PTRMSGTYPE_SYSTEM => match u32le(data, 0)? {
            SYSPTR_NULL => Some(PointerUpdate::Hidden),
            _ => Some(PointerUpdate::SystemDefault),
        },
        TS_PTRMSGTYPE_POSITION => Some(PointerUpdate::Position {
            x: u16le(data, 0)?,
            y: u16le(data, 2)?,
        }),
        TS_PTRMSGTYPE_CACHED => Some(PointerUpdate::Cached {
            cache_index: u16le(data, 0)?,
        }),
        TS_PTRMSGTYPE_COLOR => {
            let (cache_index, shape) = parse_color_pointer(data, 24)?;
            Some(PointerUpdate::Shape { cache_index, shape })
        }
        TS_PTRMSGTYPE_POINTER => {
            // TS_POINTERATTRIBUTE: xorBpp, then a TS_COLORPOINTERATTRIBUTE.
            let xor_bpp = u16le(data, 0)?;
            let (cache_index, shape) = parse_color_pointer(data.get(2..)?, xor_bpp)?;
            Some(PointerUpdate::Shape { cache_index, shape })
        }
        _ => None,
    }
}

/// Parse a `TS_COLORPOINTERATTRIBUTE` (the body of a colour/new pointer) with a
/// given `xor_bpp`, returning the cache index and the decoded RGBA shape.
fn parse_color_pointer(buf: &[u8], xor_bpp: u16) -> Option<(u16, CursorShape)> {
    let cache_index = u16le(buf, 0)?;
    let hot_x = u16le(buf, 2)?;
    let hot_y = u16le(buf, 4)?;
    let width = u16le(buf, 6)?;
    let height = u16le(buf, 8)?;
    let len_and = u16le(buf, 10)? as usize;
    let len_xor = u16le(buf, 12)? as usize;
    let xor_mask = buf.get(14..14 + len_xor)?;
    let and_mask = buf.get(14 + len_xor..14 + len_xor + len_and)?;

    // Guard against absurd shapes (real cursors are ≤ 96×96).
    if width == 0 || height == 0 || width > 384 || height > 384 {
        return None;
    }
    let rgba = decode_masks(xor_mask, and_mask, width as usize, height as usize, xor_bpp)?;
    Some((
        cache_index,
        CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            rgba,
        },
    ))
}

/// Decode bottom-up AND/XOR masks into top-down RGBA8.
fn decode_masks(
    xor: &[u8],
    and: &[u8],
    width: usize,
    height: usize,
    bpp: u16,
) -> Option<Vec<u8>> {
    let xor_stride = padded_stride(width * bpp as usize);
    let and_stride = padded_stride(width);

    // For 32bpp cursors the alpha lives in the XOR mask — but some servers send
    // a zero alpha plane and rely on the AND mask instead. Decide once.
    let use_xor_alpha = bpp == 32 && {
        let mut any = false;
        'outer: for sy in 0..height {
            let row = sy * xor_stride;
            for x in 0..width {
                if xor.get(row + x * 4 + 3).is_some_and(|&a| a != 0) {
                    any = true;
                    break 'outer;
                }
            }
        }
        any
    };

    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let src = height - 1 - y; // masks are bottom-up
        let xor_row = src * xor_stride;
        let and_row = src * and_stride;
        for x in 0..width {
            let a_bit = and_bit(and, and_row, x);
            let (r, g, b) = xor_color(xor, xor_row, x, bpp);
            let alpha = if use_xor_alpha {
                *xor.get(xor_row + x * 4 + 3).unwrap_or(&0)
            } else if a_bit == 0 {
                0xFF // AND=0 → opaque
            } else {
                0 // AND=1 → transparent (inversion is approximated as transparent)
            };
            let di = (y * width + x) * 4;
            out[di] = r;
            out[di + 1] = g;
            out[di + 2] = b;
            out[di + 3] = alpha;
        }
    }
    Some(out)
}

/// Extract the (R,G,B) of one XOR-mask pixel. Out-of-range reads yield black.
#[inline]
fn xor_color(xor: &[u8], row_start: usize, x: usize, bpp: u16) -> (u8, u8, u8) {
    match bpp {
        32 => {
            let o = row_start + x * 4;
            // BGRA order on the wire.
            (
                *xor.get(o + 2).unwrap_or(&0),
                *xor.get(o + 1).unwrap_or(&0),
                *xor.get(o).unwrap_or(&0),
            )
        }
        24 => {
            let o = row_start + x * 3;
            (
                *xor.get(o + 2).unwrap_or(&0),
                *xor.get(o + 1).unwrap_or(&0),
                *xor.get(o).unwrap_or(&0),
            )
        }
        16 => {
            let o = row_start + x * 2;
            let v = u16::from_le_bytes([
                *xor.get(o).unwrap_or(&0),
                *xor.get(o + 1).unwrap_or(&0),
            ]);
            // RGB565.
            let r = ((v >> 11) & 0x1F) as u8;
            let g = ((v >> 5) & 0x3F) as u8;
            let b = (v & 0x1F) as u8;
            ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
        }
        _ => {
            // 1bpp monochrome: bit set → white, clear → black.
            let bit = (xor.get(row_start + x / 8).copied().unwrap_or(0) >> (7 - (x % 8))) & 1;
            if bit == 1 {
                (0xFF, 0xFF, 0xFF)
            } else {
                (0, 0, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_pointer_pdu(
        msg: u16,
        cache: u16,
        w: u16,
        h: u16,
        xor: &[u8],
        and: &[u8],
        xor_bpp: Option<u16>,
    ) -> Vec<u8> {
        let mut b = vec![];
        b.extend_from_slice(&msg.to_le_bytes());
        b.extend_from_slice(&[0, 0]); // pad
        if let Some(bpp) = xor_bpp {
            b.extend_from_slice(&bpp.to_le_bytes()); // new-pointer xorBpp
        }
        b.extend_from_slice(&cache.to_le_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]); // hotspot (0,0)
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&(and.len() as u16).to_le_bytes());
        b.extend_from_slice(&(xor.len() as u16).to_le_bytes());
        b.extend_from_slice(xor);
        b.extend_from_slice(and);
        b
    }

    #[test]
    fn system_pointer_hide_and_default() {
        let mut hide = vec![0x01, 0x00, 0, 0];
        hide.extend_from_slice(&SYSPTR_NULL.to_le_bytes());
        assert_eq!(parse_pointer_update(&hide), Some(PointerUpdate::Hidden));

        let mut def = vec![0x01, 0x00, 0, 0];
        def.extend_from_slice(&SYSPTR_DEFAULT.to_le_bytes());
        assert_eq!(
            parse_pointer_update(&def),
            Some(PointerUpdate::SystemDefault)
        );
    }

    #[test]
    fn position_and_cached() {
        let mut pos = vec![0x03, 0x00, 0, 0];
        pos.extend_from_slice(&7u16.to_le_bytes());
        pos.extend_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            parse_pointer_update(&pos),
            Some(PointerUpdate::Position { x: 7, y: 9 })
        );

        let mut cached = vec![0x07, 0x00, 0, 0];
        cached.extend_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            parse_pointer_update(&cached),
            Some(PointerUpdate::Cached { cache_index: 3 })
        );
    }

    #[test]
    fn color_pointer_24bpp_and_mask_transparency() {
        // 2x2, 24bpp. XOR rows are bottom-up; stride padded to 2 bytes
        // (2px*3 = 6 bytes, already even). One opaque red pixel (top-left),
        // the rest transparent via the AND mask.
        // Bottom row first: pixels (0,1)=(0,1). Top row: (0,0)=red,(1,0)=x.
        let red = [0u8, 0, 0xFF]; // B,G,R
        let zero = [0u8, 0, 0];
        // bottom row (y=1): two transparent
        // top row (y=0): red, transparent
        let mut xor = vec![];
        xor.extend_from_slice(&zero);
        xor.extend_from_slice(&zero); // bottom row
        xor.extend_from_slice(&red);
        xor.extend_from_slice(&zero); // top row
        // AND mask: 1 bit/px, stride padded to 2 bytes per row. Row = bits
        // [px0,px1,...] MSB-first. transparent=1, opaque=0.
        // bottom row: both transparent → bits 1,1 → 0b11000000 = 0xC0, pad byte 0
        // top row: opaque,transparent → 0,1 → 0b01000000 = 0x40, pad byte 0
        let and = vec![0xC0, 0x00, 0x40, 0x00];
        let pdu = color_pointer_pdu(0x0006, 5, 2, 2, &xor, &and, None);
        let PointerUpdate::Shape { cache_index, shape } =
            parse_pointer_update(&pdu).unwrap()
        else {
            panic!("expected shape");
        };
        assert_eq!(cache_index, 5);
        assert_eq!((shape.width, shape.height), (2, 2));
        // Top-left is opaque red.
        assert_eq!(&shape.rgba[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
        // Top-right transparent.
        assert_eq!(shape.rgba[7], 0x00);
        // Bottom row both transparent (alpha 0).
        assert_eq!(shape.rgba[11], 0x00);
        assert_eq!(shape.rgba[15], 0x00);
    }

    #[test]
    fn new_pointer_32bpp_uses_xor_alpha() {
        // 1x2 new pointer, 32bpp BGRA with explicit alpha; AND mask all zero.
        // bottom row pixel: green half-alpha; top row pixel: blue opaque.
        let mut xor = vec![];
        xor.extend_from_slice(&[0, 0xFF, 0, 0x80]); // bottom: B0 G255 R0 A128
        xor.extend_from_slice(&[0xFF, 0, 0, 0xFF]); // top:    B255 G0 R0 A255
        let and = vec![0x00, 0x00]; // 1px wide → 1 byte, padded to 2; all opaque-by-AND
        let pdu = color_pointer_pdu(0x0008, 1, 1, 2, &xor, &and, Some(32));
        let PointerUpdate::Shape { shape, .. } = parse_pointer_update(&pdu).unwrap() else {
            panic!("expected shape");
        };
        // Top pixel: blue opaque.
        assert_eq!(&shape.rgba[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
        // Bottom pixel: green, alpha 0x80 from the XOR mask.
        assert_eq!(&shape.rgba[4..8], &[0x00, 0xFF, 0x00, 0x80]);
    }

    #[test]
    fn rejects_truncated_and_absurd() {
        assert_eq!(parse_pointer_update(&[0x06, 0x00]), None);
        // width 0 is rejected.
        let pdu = color_pointer_pdu(0x0006, 0, 0, 2, &[], &[], None);
        assert_eq!(parse_pointer_update(&pdu), None);
    }
}
