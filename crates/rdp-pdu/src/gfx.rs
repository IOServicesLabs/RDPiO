//! RDPGFX / EGFX (MS-RDPEGFX): the graphics-pipeline command PDUs carried over
//! the `Microsoft::Windows::RDS::Graphics` dynamic virtual channel.
//!
//! This module is the **wire layer only**: it parses the server command stream
//! (after any ZGFX bulk-decompression) into typed [`GfxCommand`]s and builds the
//! two client PDUs we need — capability advertise and frame acknowledge. Each
//! PDU begins with an 8-byte [`GfxHeader`]; a channel payload may concatenate
//! several, so [`parse_commands`] walks them by `pdu_length`.
//!
//! ZGFX decompression, surface compositing, and H.264 (AVC420/444) decoding —
//! including the region metablock that precedes an AVC bitstream — are separate
//! steps handled by the codec layer (`rdp_graphics`).

// --- Command identifiers (MS-RDPEGFX 2.2.1.5 RDPGFX_HEADER cmdId) -----------
pub const CMDID_WIRE_TO_SURFACE_1: u16 = 0x0001;
pub const CMDID_WIRE_TO_SURFACE_2: u16 = 0x0002;
pub const CMDID_DELETE_ENCODING_CONTEXT: u16 = 0x0003;
pub const CMDID_SOLIDFILL: u16 = 0x0004;
pub const CMDID_SURFACE_TO_SURFACE: u16 = 0x0005;
pub const CMDID_SURFACE_TO_CACHE: u16 = 0x0006;
pub const CMDID_CACHE_TO_SURFACE: u16 = 0x0007;
pub const CMDID_EVICT_CACHE_ENTRY: u16 = 0x0008;
pub const CMDID_CREATE_SURFACE: u16 = 0x0009;
pub const CMDID_DELETE_SURFACE: u16 = 0x000A;
pub const CMDID_START_FRAME: u16 = 0x000B;
pub const CMDID_END_FRAME: u16 = 0x000C;
pub const CMDID_FRAME_ACKNOWLEDGE: u16 = 0x000D;
pub const CMDID_RESET_GRAPHICS: u16 = 0x000E;
pub const CMDID_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
pub const CMDID_CACHE_IMPORT_OFFER: u16 = 0x0010;
pub const CMDID_CACHE_IMPORT_REPLY: u16 = 0x0011;
pub const CMDID_CAPS_ADVERTISE: u16 = 0x0012;
pub const CMDID_CAPS_CONFIRM: u16 = 0x0013;
pub const CMDID_MAP_SURFACE_TO_WINDOW: u16 = 0x0016;
pub const CMDID_QOE_FRAME_ACKNOWLEDGE: u16 = 0x0018;

// --- Capability versions (MS-RDPEGFX 2.2.3) ---------------------------------
pub const CAPVERSION_8: u32 = 0x0008_0004;
pub const CAPVERSION_81: u32 = 0x0008_0105;
pub const CAPVERSION_10: u32 = 0x000A_0002;
pub const CAPVERSION_101: u32 = 0x000A_0100;
pub const CAPVERSION_102: u32 = 0x000A_0200;
pub const CAPVERSION_103: u32 = 0x000A_0301;
pub const CAPVERSION_104: u32 = 0x000A_0400;
pub const CAPVERSION_105: u32 = 0x000A_0502;
pub const CAPVERSION_106: u32 = 0x000A_0600;

// --- Codec identifiers (MS-RDPEGFX 2.2.4.1 RDPGFX_WIRE_TO_SURFACE_PDU_1) -----
pub const CODECID_UNCOMPRESSED: u16 = 0x0000;
pub const CODECID_CAVIDEO: u16 = 0x0003; // RemoteFX (RFX) video
pub const CODECID_CLEARCODEC: u16 = 0x0008;
pub const CODECID_PLANAR: u16 = 0x000A;
pub const CODECID_AVC420: u16 = 0x000B; // H.264 AVC 4:2:0 (single stream)
pub const CODECID_AVC444: u16 = 0x000E;
pub const CODECID_AVC444V2: u16 = 0x000F;
pub const CODECID_ALPHA: u16 = 0x000C;

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
#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}
#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The 8-byte RDPGFX_HEADER prefixing every EGFX PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfxHeader {
    pub cmd_id: u16,
    pub flags: u16,
    /// Length of the whole PDU, including this header.
    pub pdu_length: u32,
}

impl GfxHeader {
    /// Parse the header from the front of `buf` (does not consume).
    pub fn parse(buf: &[u8]) -> Option<GfxHeader> {
        Some(GfxHeader {
            cmd_id: u16le(buf, 0)?,
            flags: u16le(buf, 2)?,
            pdu_length: u32le(buf, 4)?,
        })
    }

    fn write(cmd_id: u16, pdu_length: u32, out: &mut Vec<u8>) {
        put_u16(cmd_id, out);
        put_u16(0, out); // flags
        put_u32(pdu_length, out);
    }
}

/// An inclusive-exclusive 16-bit rectangle (RDPGFX_RECT16): right/bottom are
/// exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect16 {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect16 {
    fn parse(b: &[u8], o: usize) -> Option<Rect16> {
        Some(Rect16 {
            left: u16le(b, o)?,
            top: u16le(b, o + 2)?,
            right: u16le(b, o + 4)?,
            bottom: u16le(b, o + 6)?,
        })
    }
}

/// A 16-bit point (RDPGFX_POINT16): the destination origin for a copy/cache blit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point16 {
    pub x: u16,
    pub y: u16,
}

impl Point16 {
    fn parse(b: &[u8], o: usize) -> Option<Point16> {
        Some(Point16 {
            x: u16le(b, o)?,
            y: u16le(b, o + 2)?,
        })
    }
}

#[inline]
fn u64le(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// A parsed EGFX server command. Commands we do not model individually are
/// reported as [`GfxCommand::Other`] so a stream can still be walked fully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GfxCommand {
    CapsConfirm {
        version: u32,
        flags: u32,
    },
    CreateSurface {
        surface_id: u16,
        width: u16,
        height: u16,
        pixel_format: u8,
    },
    DeleteSurface {
        surface_id: u16,
    },
    MapSurfaceToOutput {
        surface_id: u16,
        x: u32,
        y: u32,
    },
    StartFrame {
        timestamp: u32,
        frame_id: u32,
    },
    EndFrame {
        frame_id: u32,
    },
    ResetGraphics {
        width: u32,
        height: u32,
        monitor_count: u32,
    },
    SolidFill {
        surface_id: u16,
        /// `RDPGFX_COLOR32` fill pixel: bytes are B, G, R, XA (little-endian u32).
        color: u32,
        /// The surface-relative rectangles to fill.
        rects: Vec<Rect16>,
    },
    /// A bitmap/codec payload bound for a surface. `bitmap` is the codec data
    /// (e.g. an AVC420 stream when `codec_id == CODECID_CAVIDEO`).
    WireToSurface1 {
        surface_id: u16,
        codec_id: u16,
        pixel_format: u8,
        dest: Rect16,
        bitmap: Vec<u8>,
    },
    /// A progressive-codec payload bound for a surface (RDPGFX_CMDID_WIRETOSURFACE_2).
    /// Unlike WireToSurface1 this carries no destination rect — the tile positions
    /// live inside the RemoteFX Progressive bitstream — but it does carry a
    /// `codec_context_id` identifying the persistent encode/decode context whose
    /// tile state accumulates across frames (torn down by [`GfxCommand::DeleteEncodingContext`]).
    WireToSurface2 {
        surface_id: u16,
        codec_id: u16,
        codec_context_id: u32,
        pixel_format: u8,
        bitmap: Vec<u8>,
    },
    /// Tear down a progressive codec context (RDPGFX_CMDID_DELETEENCODINGCONTEXT):
    /// the server is done with `codec_context_id` on this surface, so its cached
    /// per-tile coefficient state must be dropped.
    DeleteEncodingContext {
        surface_id: u16,
        codec_context_id: u32,
    },
    /// Copy `rect_src` from one surface to `dest_pts` on another (window moves,
    /// scrolling). `dest_pts` are the top-left corners of each destination copy.
    SurfaceToSurface {
        surface_src: u16,
        surface_dst: u16,
        rect_src: Rect16,
        dest_pts: Vec<Point16>,
    },
    /// Cache `rect_src` of a surface into cache `slot` (keyed by `cache_key`).
    SurfaceToCache {
        surface_id: u16,
        cache_key: u64,
        slot: u16,
        rect_src: Rect16,
    },
    /// Blit cache `slot` onto a surface at each of `dest_pts`.
    CacheToSurface {
        slot: u16,
        surface_id: u16,
        dest_pts: Vec<Point16>,
    },
    /// Any command this module does not decode into a typed variant.
    Other {
        cmd_id: u16,
    },
}

/// Decode one command from its `cmd_id` and body (the PDU bytes after the
/// 8-byte header). Unknown commands map to [`GfxCommand::Other`].
fn parse_one(cmd_id: u16, body: &[u8]) -> GfxCommand {
    let parsed = match cmd_id {
        CMDID_CAPS_CONFIRM => (|| {
            // A single RDPGFX_CAPSET: version(4) + capsDataLength(4) + capsData.
            let version = u32le(body, 0)?;
            let len = u32le(body, 4)? as usize;
            let flags = if len >= 4 { u32le(body, 8)? } else { 0 };
            Some(GfxCommand::CapsConfirm { version, flags })
        })(),
        CMDID_CREATE_SURFACE => (|| {
            Some(GfxCommand::CreateSurface {
                surface_id: u16le(body, 0)?,
                width: u16le(body, 2)?,
                height: u16le(body, 4)?,
                pixel_format: *body.get(6)?,
            })
        })(),
        CMDID_DELETE_SURFACE => {
            u16le(body, 0).map(|surface_id| GfxCommand::DeleteSurface { surface_id })
        }
        CMDID_MAP_SURFACE_TO_OUTPUT => (|| {
            Some(GfxCommand::MapSurfaceToOutput {
                surface_id: u16le(body, 0)?,
                // body[2..4] reserved
                x: u32le(body, 4)?,
                y: u32le(body, 8)?,
            })
        })(),
        CMDID_START_FRAME => (|| {
            Some(GfxCommand::StartFrame {
                timestamp: u32le(body, 0)?,
                frame_id: u32le(body, 4)?,
            })
        })(),
        CMDID_END_FRAME => u32le(body, 0).map(|frame_id| GfxCommand::EndFrame { frame_id }),
        CMDID_RESET_GRAPHICS => (|| {
            Some(GfxCommand::ResetGraphics {
                width: u32le(body, 0)?,
                height: u32le(body, 4)?,
                monitor_count: u32le(body, 8)?,
            })
        })(),
        CMDID_SOLIDFILL => (|| {
            let surface_id = u16le(body, 0)?;
            let color = u32le(body, 2)?;
            let rect_count = u16le(body, 6)? as usize;
            // fillRects follow at offset 8, each an 8-byte RDPGFX_RECT16.
            let mut rects = Vec::with_capacity(rect_count.min(4096));
            for i in 0..rect_count {
                match Rect16::parse(body, 8 + i * 8) {
                    Some(r) => rects.push(r),
                    None => break,
                }
            }
            Some(GfxCommand::SolidFill {
                surface_id,
                color,
                rects,
            })
        })(),
        CMDID_WIRE_TO_SURFACE_1 => (|| {
            let surface_id = u16le(body, 0)?;
            let codec_id = u16le(body, 2)?;
            let pixel_format = *body.get(4)?;
            let dest = Rect16::parse(body, 5)?;
            let bitmap_len = u32le(body, 13)? as usize;
            let start = 17;
            let bitmap = body.get(start..start + bitmap_len)?.to_vec();
            Some(GfxCommand::WireToSurface1 {
                surface_id,
                codec_id,
                pixel_format,
                dest,
                bitmap,
            })
        })(),
        CMDID_WIRE_TO_SURFACE_2 => (|| {
            // RDPGFX_WIRE_TO_SURFACE_PDU_2: surfaceId(2) + codecId(2) +
            // codecContextId(4) + pixelFormat(1) + bitmapData (to end of PDU;
            // there is no explicit length — the progressive stream is the rest).
            let surface_id = u16le(body, 0)?;
            let codec_id = u16le(body, 2)?;
            let codec_context_id = u32le(body, 4)?;
            let pixel_format = *body.get(8)?;
            let bitmap = body.get(9..)?.to_vec();
            Some(GfxCommand::WireToSurface2 {
                surface_id,
                codec_id,
                codec_context_id,
                pixel_format,
                bitmap,
            })
        })(),
        CMDID_DELETE_ENCODING_CONTEXT => (|| {
            Some(GfxCommand::DeleteEncodingContext {
                surface_id: u16le(body, 0)?,
                codec_context_id: u32le(body, 2)?,
            })
        })(),
        CMDID_SURFACE_TO_SURFACE => (|| {
            let surface_src = u16le(body, 0)?;
            let surface_dst = u16le(body, 2)?;
            let rect_src = Rect16::parse(body, 4)?;
            let count = u16le(body, 12)? as usize;
            let mut dest_pts = Vec::with_capacity(count.min(4096));
            for i in 0..count {
                match Point16::parse(body, 14 + i * 4) {
                    Some(p) => dest_pts.push(p),
                    None => break,
                }
            }
            Some(GfxCommand::SurfaceToSurface {
                surface_src,
                surface_dst,
                rect_src,
                dest_pts,
            })
        })(),
        CMDID_SURFACE_TO_CACHE => (|| {
            Some(GfxCommand::SurfaceToCache {
                surface_id: u16le(body, 0)?,
                cache_key: u64le(body, 2)?,
                slot: u16le(body, 10)?,
                rect_src: Rect16::parse(body, 12)?,
            })
        })(),
        CMDID_CACHE_TO_SURFACE => (|| {
            let slot = u16le(body, 0)?;
            let surface_id = u16le(body, 2)?;
            let count = u16le(body, 4)? as usize;
            let mut dest_pts = Vec::with_capacity(count.min(4096));
            for i in 0..count {
                match Point16::parse(body, 6 + i * 4) {
                    Some(p) => dest_pts.push(p),
                    None => break,
                }
            }
            Some(GfxCommand::CacheToSurface {
                slot,
                surface_id,
                dest_pts,
            })
        })(),
        _ => None,
    };
    parsed.unwrap_or(GfxCommand::Other { cmd_id })
}

/// Walk a (decompressed) EGFX channel payload into its commands. Stops at the
/// first malformed or truncated header rather than erroring.
pub fn parse_commands(buf: &[u8]) -> Vec<GfxCommand> {
    let mut out = Vec::new();
    let mut rest = buf;
    while rest.len() >= 8 {
        let Some(hdr) = GfxHeader::parse(rest) else {
            break;
        };
        let len = hdr.pdu_length as usize;
        if len < 8 || len > rest.len() {
            break;
        }
        out.push(parse_one(hdr.cmd_id, &rest[8..len]));
        rest = &rest[len..];
    }
    out
}

/// Build a client `RDPGFX_CAPS_ADVERTISE_PDU` offering each `(version, flags)`
/// capability set (each carries a 4-byte flags `capsData`).
pub fn caps_advertise(capsets: &[(u32, u32)]) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(capsets.len() as u16, &mut body); // capsSetCount
    for &(version, flags) in capsets {
        put_u32(version, &mut body);
        put_u32(4, &mut body); // capsDataLength
        put_u32(flags, &mut body);
    }
    let mut out = Vec::with_capacity(8 + body.len());
    GfxHeader::write(CMDID_CAPS_ADVERTISE, (8 + body.len()) as u32, &mut out);
    out.extend_from_slice(&body);
    out
}

/// Build a client `RDPGFX_FRAME_ACKNOWLEDGE_PDU` for `frame_id`.
pub fn frame_acknowledge(queue_depth: u32, frame_id: u32, total_frames_decoded: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    GfxHeader::write(CMDID_FRAME_ACKNOWLEDGE, 20, &mut out);
    put_u32(queue_depth, &mut out);
    put_u32(frame_id, &mut out);
    put_u32(total_frames_decoded, &mut out);
    out
}

// The AVC420 region metablock and the H.264 bitstream a WireToSurface1 carries
// are parsed by the codec layer (`rdp_graphics::avc`), keeping this module to
// the EGFX command framing.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut buf = Vec::new();
        GfxHeader::write(CMDID_START_FRAME, 16, &mut buf);
        let h = GfxHeader::parse(&buf).unwrap();
        assert_eq!(h.cmd_id, CMDID_START_FRAME);
        assert_eq!(h.flags, 0);
        assert_eq!(h.pdu_length, 16);
    }

    fn pdu(cmd_id: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        GfxHeader::write(cmd_id, (8 + body.len()) as u32, &mut out);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn parse_create_surface() {
        // surfaceId=1, width=1024, height=768, pixelFormat=0x20.
        let body = [0x01, 0x00, 0x00, 0x04, 0x00, 0x03, 0x20];
        let cmds = parse_commands(&pdu(CMDID_CREATE_SURFACE, &body));
        assert_eq!(
            cmds,
            vec![GfxCommand::CreateSurface {
                surface_id: 1,
                width: 1024,
                height: 768,
                pixel_format: 0x20,
            }]
        );
    }

    #[test]
    fn parse_two_concatenated_commands() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pdu(CMDID_START_FRAME, &[0x10, 0, 0, 0, 0x07, 0, 0, 0]));
        buf.extend_from_slice(&pdu(CMDID_END_FRAME, &[0x07, 0, 0, 0]));
        let cmds = parse_commands(&buf);
        assert_eq!(
            cmds,
            vec![
                GfxCommand::StartFrame {
                    timestamp: 16,
                    frame_id: 7,
                },
                GfxCommand::EndFrame { frame_id: 7 },
            ]
        );
    }

    #[test]
    fn parse_wire_to_surface_1_keeps_bitmap() {
        let mut body = Vec::new();
        put_u16(2, &mut body); // surfaceId
        put_u16(CODECID_CAVIDEO, &mut body); // codecId (H.264)
        body.push(0x20); // pixelFormat
        body.extend_from_slice(&[0, 0, 0, 0, 0x80, 0x07, 0x38, 0x04]); // dest rect 0,0..1920,1080
        put_u32(3, &mut body); // bitmapLength
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // codec data
        let cmds = parse_commands(&pdu(CMDID_WIRE_TO_SURFACE_1, &body));
        match &cmds[0] {
            GfxCommand::WireToSurface1 {
                surface_id,
                codec_id,
                dest,
                bitmap,
                ..
            } => {
                assert_eq!(*surface_id, 2);
                assert_eq!(*codec_id, CODECID_CAVIDEO);
                assert_eq!(dest.right, 1920);
                assert_eq!(dest.bottom, 1080);
                assert_eq!(bitmap, &[0xAA, 0xBB, 0xCC]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_wire_to_surface_2_progressive() {
        // RDPGFX_WIRE_TO_SURFACE_PDU_2: no dest rect, no length field — the
        // progressive bitstream is everything after the 9-byte fixed header.
        let mut body = Vec::new();
        put_u16(5, &mut body); // surfaceId
        put_u16(0x0009, &mut body); // codecId (CAPROGRESSIVE)
        put_u32(0x1234_5678, &mut body); // codecContextId
        body.push(0x20); // pixelFormat
        body.extend_from_slice(&[0xCC, 0xC0, 0x06, 0x00, 0x00, 0x00]); // a WBT_SYNC block
        let cmds = parse_commands(&pdu(CMDID_WIRE_TO_SURFACE_2, &body));
        match &cmds[0] {
            GfxCommand::WireToSurface2 {
                surface_id,
                codec_id,
                codec_context_id,
                pixel_format,
                bitmap,
            } => {
                assert_eq!(*surface_id, 5);
                assert_eq!(*codec_id, 0x0009);
                assert_eq!(*codec_context_id, 0x1234_5678);
                assert_eq!(*pixel_format, 0x20);
                assert_eq!(bitmap, &[0xCC, 0xC0, 0x06, 0x00, 0x00, 0x00]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_delete_encoding_context() {
        let mut body = Vec::new();
        put_u16(5, &mut body); // surfaceId
        put_u32(0x1234_5678, &mut body); // codecContextId
        let cmds = parse_commands(&pdu(CMDID_DELETE_ENCODING_CONTEXT, &body));
        assert_eq!(
            cmds,
            vec![GfxCommand::DeleteEncodingContext {
                surface_id: 5,
                codec_context_id: 0x1234_5678,
            }]
        );
    }

    #[test]
    fn caps_advertise_then_parse_back_as_caps_confirm_shape() {
        let adv = caps_advertise(&[(CAPVERSION_8, 0), (CAPVERSION_10, 0x20)]);
        let h = GfxHeader::parse(&adv).unwrap();
        assert_eq!(h.cmd_id, CMDID_CAPS_ADVERTISE);
        assert_eq!(h.pdu_length as usize, adv.len());
        // capsSetCount then the first version.
        assert_eq!(u16le(&adv, 8).unwrap(), 2);
        assert_eq!(u32le(&adv, 10).unwrap(), CAPVERSION_8);
        // A CAPS_CONFIRM echoes a single capset; parse_one should read it.
        let confirm = pdu(CMDID_CAPS_CONFIRM, &{
            let mut b = Vec::new();
            put_u32(CAPVERSION_10, &mut b);
            put_u32(4, &mut b);
            put_u32(0x20, &mut b);
            b
        });
        assert_eq!(
            parse_commands(&confirm),
            vec![GfxCommand::CapsConfirm {
                version: CAPVERSION_10,
                flags: 0x20,
            }]
        );
    }

    #[test]
    fn frame_acknowledge_layout() {
        let ack = frame_acknowledge(0, 42, 100);
        let h = GfxHeader::parse(&ack).unwrap();
        assert_eq!(h.cmd_id, CMDID_FRAME_ACKNOWLEDGE);
        assert_eq!(h.pdu_length, 20);
        assert_eq!(ack.len(), 20);
        assert_eq!(u32le(&ack, 12).unwrap(), 42); // frameId
        assert_eq!(u32le(&ack, 16).unwrap(), 100); // totalFramesDecoded
    }

    #[test]
    fn unknown_command_is_walked_not_dropped() {
        // EVICT_CACHE_ENTRY (not modelled) followed by END_FRAME.
        let mut buf = Vec::new();
        buf.extend_from_slice(&pdu(CMDID_EVICT_CACHE_ENTRY, &[0; 4]));
        buf.extend_from_slice(&pdu(CMDID_END_FRAME, &[5, 0, 0, 0]));
        let cmds = parse_commands(&buf);
        assert_eq!(
            cmds[0],
            GfxCommand::Other {
                cmd_id: CMDID_EVICT_CACHE_ENTRY
            }
        );
        assert_eq!(cmds[1], GfxCommand::EndFrame { frame_id: 5 });
    }

    #[test]
    fn parses_surface_to_cache_and_cache_to_surface() {
        // SurfaceToCache: surfaceId=1, cacheKey, slot=7, rectSrc(0,0,64,64).
        let mut s2c = Vec::new();
        s2c.extend_from_slice(&1u16.to_le_bytes());
        s2c.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        s2c.extend_from_slice(&7u16.to_le_bytes());
        s2c.extend_from_slice(&[0, 0, 0, 0, 64, 0, 64, 0]); // rect 0,0,64,64
        match parse_one(CMDID_SURFACE_TO_CACHE, &s2c) {
            GfxCommand::SurfaceToCache { surface_id, slot, rect_src, .. } => {
                assert_eq!(surface_id, 1);
                assert_eq!(slot, 7);
                assert_eq!((rect_src.right, rect_src.bottom), (64, 64));
            }
            other => panic!("expected SurfaceToCache, got {other:?}"),
        }
        // CacheToSurface: slot=7, surfaceId=1, one destPt (10,20).
        let mut c2s = Vec::new();
        c2s.extend_from_slice(&7u16.to_le_bytes());
        c2s.extend_from_slice(&1u16.to_le_bytes());
        c2s.extend_from_slice(&1u16.to_le_bytes()); // count
        c2s.extend_from_slice(&[10, 0, 20, 0]); // point (10,20)
        match parse_one(CMDID_CACHE_TO_SURFACE, &c2s) {
            GfxCommand::CacheToSurface { slot, surface_id, dest_pts } => {
                assert_eq!((slot, surface_id), (7, 1));
                assert_eq!(dest_pts.len(), 1);
                assert_eq!((dest_pts[0].x, dest_pts[0].y), (10, 20));
            }
            other => panic!("expected CacheToSurface, got {other:?}"),
        }
    }
}
