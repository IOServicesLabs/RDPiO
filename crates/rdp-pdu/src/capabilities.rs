//! Capability exchange (MS-RDPBCGR 2.2.1.13): the Share Control Header, the
//! Demand Active PDU (parsed for its shareId), the client capability sets, and
//! the Confirm Active PDU.

use crate::{ensure, PduError, PduResult};

// Share Control Header pduType: low nibble = PDUTYPE, high bits = version.
const PDUTYPE_DEMAND_ACTIVE: u16 = 0x1;
const PDUTYPE_CONFIRM_ACTIVE: u16 = 0x3;
const PROTOCOL_VERSION: u16 = 0x10;

// Capability set types.
pub const CAPSET_GENERAL: u16 = 1;
pub const CAPSET_BITMAP: u16 = 2;
pub const CAPSET_ORDER: u16 = 3;
pub const CAPSET_POINTER: u16 = 8;
pub const CAPSET_INPUT: u16 = 13;
pub const CAPSET_VIRTUAL_CHANNEL: u16 = 20;
pub const CAPSET_SURFACE_COMMANDS: u16 = 0x1C;
pub const CAPSET_BITMAP_CODECS: u16 = 0x1D;

/// The codec id we assign RemoteFX in the Bitmap Codecs capability. The server
/// echoes it in each Surface Bits command's `TS_BITMAP_DATA_EX.codecID`, which
/// is how the graphics layer recognises an RFX (vs uncompressed) payload.
pub const CODEC_ID_REMOTEFX: u8 = 0x03;

// General capability extraFlags (MS-RDPBCGR 2.2.7.1.1).
const FASTPATH_OUTPUT_SUPPORTED: u16 = 0x0001;
const NO_BITMAP_COMPRESSION_HDR: u16 = 0x0400;

/// The server channel id used as the Confirm Active originatorId.
const ORIGINATOR_ID: u16 = 0x03EA;

#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Wrap a capability payload in its 4-byte (type, length) header.
fn cap_set(cap_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    put_u16(cap_type, &mut out);
    put_u16((payload.len() + 4) as u16, &mut out);
    out.extend_from_slice(payload);
    out
}

fn general_caps(fastpath_output: bool) -> Vec<u8> {
    // NO_BITMAP_COMPRESSION_HDR keeps slow-path bitmaps header-less; when we
    // advertise RemoteFX we also set FASTPATH_OUTPUT_SUPPORTED so the server may
    // send Surface Commands (and any other output) over fast-path — the only
    // transport RemoteFX has.
    let mut extra_flags = NO_BITMAP_COMPRESSION_HDR;
    if fastpath_output {
        extra_flags |= FASTPATH_OUTPUT_SUPPORTED;
    }
    let mut p = Vec::new();
    put_u16(1, &mut p); // osMajorType = WINDOWS
    put_u16(3, &mut p); // osMinorType = WINDOWS NT
    put_u16(0x0200, &mut p); // protocolVersion
    put_u16(0, &mut p); // pad
    put_u16(0, &mut p); // generalCompressionTypes
    put_u16(extra_flags, &mut p); // extraFlags
    put_u16(0, &mut p); // updateCapabilityFlag
    put_u16(0, &mut p); // remoteUnshareFlag
    put_u16(0, &mut p); // generalCompressionLevel
    p.push(0); // refreshRectSupport
    p.push(0); // suppressOutputSupport
    cap_set(CAPSET_GENERAL, &p)
}

fn bitmap_caps(width: u16, height: u16, bpp: u16) -> Vec<u8> {
    let mut p = Vec::new();
    put_u16(bpp, &mut p); // preferredBitsPerPixel
    put_u16(1, &mut p); // receive1BitPerPixel
    put_u16(1, &mut p); // receive4BitsPerPixel
    put_u16(1, &mut p); // receive8BitsPerPixel
    put_u16(width, &mut p);
    put_u16(height, &mut p);
    put_u16(0, &mut p); // pad
    put_u16(1, &mut p); // desktopResizeFlag
                        // bitmapCompressionFlag advertises that the *client* can
                        // decode compressed bitmaps. MS-RDPBCGR 2.2.7.1.2 makes
                        // this mandatory: "this field MUST be set to TRUE; if it
                        // is not set to TRUE, the server MUST NOT continue with
                        // the connection." (A 0 here makes Windows RST right after
                        // Confirm Active.) It does not force the server to compress
                        // — it may still send raw rects — and our decoder handles
                        // both (interleaved-RLE via `decompress_interleaved`).
    put_u16(1, &mut p); // bitmapCompressionFlag (mandatory)
    p.push(0); // highColorFlags
    p.push(0); // drawingFlags
    put_u16(1, &mut p); // multipleRectangleSupport
    put_u16(0, &mut p); // pad
    cap_set(CAPSET_BITMAP, &p)
}

fn order_caps() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0u8; 16]); // terminalDescriptor
    put_u32(0, &mut p); // pad4
    put_u16(1, &mut p); // desktopSaveXGranularity
    put_u16(20, &mut p); // desktopSaveYGranularity
    put_u16(0, &mut p); // pad2
    put_u16(1, &mut p); // maximumOrderLevel
    put_u16(0, &mut p); // numberFonts
    // orderFlags: both NEGOTIATEORDERSUPPORT (0x02) and ZEROBOUNDSDELTASSUPPORT
    // (0x08) are mandatory per MS-RDPBCGR 2.2.7.1.3 ("MUST be set"); omitting
    // ZEROBOUNDSDELTASSUPPORT makes the server reject the Confirm Active with
    // ERRINFO_BADCAPABILITIES (0x10EA). (The previous 0x22 set COLORINDEXSUPPORT
    // by mistake instead — 0x08 was misread as 0x20.)
    const NEGOTIATEORDERSUPPORT: u16 = 0x0002;
    const ZEROBOUNDSDELTASSUPPORT: u16 = 0x0008;
    put_u16(NEGOTIATEORDERSUPPORT | ZEROBOUNDSDELTASSUPPORT, &mut p); // orderFlags = 0x000A
    p.extend_from_slice(&[0u8; 32]); // orderSupport (none → server uses bitmap/surface)
    put_u16(0, &mut p); // textFlags
    put_u16(0, &mut p); // orderSupportExFlags
    put_u32(0, &mut p); // pad4
    put_u32(230_400, &mut p); // desktopSaveSize
    put_u16(0, &mut p); // pad2
    put_u16(0, &mut p); // pad2
    put_u16(0x04e4, &mut p); // textANSICodePage = 1252
    put_u16(0, &mut p); // pad2
    cap_set(CAPSET_ORDER, &p)
}

fn pointer_caps() -> Vec<u8> {
    let mut p = Vec::new();
    put_u16(1, &mut p); // colorPointerFlag
    put_u16(20, &mut p); // colorPointerCacheSize
    put_u16(20, &mut p); // pointerCacheSize
    cap_set(CAPSET_POINTER, &p)
}

/// `TS_INPUT_CAPABILITYSET.inputFlags` bit: the peer understands relative mouse
/// events (TS_RELPOINTER_EVENT, RDP 10.7+). In the server's Demand Active it
/// means we MAY send them; in our Confirm Active it declares that we do.
pub const INPUT_FLAG_MOUSE_RELATIVE: u16 = 0x0080;

fn input_caps(keyboard_layout: u32) -> Vec<u8> {
    let mut p = Vec::new();
    // INPUT_FLAG_SCANCODES | INPUT_FLAG_MOUSEX | FASTPATH_INPUT | UNICODE |
    // FASTPATH_INPUT2 | INPUT_FLAG_MOUSE_RELATIVE (we send xbutton + relative
    // mouse events when the server supports them).
    put_u16(0x0039 | 0x0004 | INPUT_FLAG_MOUSE_RELATIVE, &mut p);
    put_u16(0, &mut p); // pad
    put_u32(keyboard_layout, &mut p);
    put_u32(4, &mut p); // keyboardType
    put_u32(0, &mut p); // keyboardSubType
    put_u32(12, &mut p); // keyboardFunctionKey
    p.extend_from_slice(&[0u8; 64]); // imeFileName
    cap_set(CAPSET_INPUT, &p)
}

fn virtual_channel_caps() -> Vec<u8> {
    let mut p = Vec::new();
    put_u32(0, &mut p); // flags = VCCAPS_NO_COMPR
    cap_set(CAPSET_VIRTUAL_CHANNEL, &p)
}

/// Surface Commands capability (MS-RDPBCGR 2.2.7.2.9): advertise that we accept
/// SET/STREAM Surface Bits and Frame Markers — the fast-path commands RemoteFX
/// (and uncompressed surface output) arrive in.
fn surface_commands_caps() -> Vec<u8> {
    const SURFCMDS_SETSURFACEBITS: u32 = 0x02;
    const SURFCMDS_FRAMEMARKER: u32 = 0x10;
    const SURFCMDS_STREAMSURFACEBITS: u32 = 0x40;
    let mut p = Vec::new();
    put_u32(
        SURFCMDS_SETSURFACEBITS | SURFCMDS_FRAMEMARKER | SURFCMDS_STREAMSURFACEBITS,
        &mut p,
    ); // cmdFlags
    put_u32(0, &mut p); // reserved
    cap_set(CAPSET_SURFACE_COMMANDS, &p)
}

/// Bitmap Codecs capability (MS-RDPBCGR 2.2.7.2.10) advertising one codec:
/// RemoteFX, with a `TS_RFX_CLNT_CAPS_CONTAINER` (MS-RDPRFX 2.2.1.1) declaring
/// 64×64 tiles, the 5/3 DWT, ICT colour conversion, image mode, and both RLGR1
/// and RLGR3 entropy coders (our decoder supports both).
fn bitmap_codecs_caps() -> Vec<u8> {
    // CODEC_GUID_REMOTEFX = {76772F12-BD72-4463-AFB3-B73C9C6F7886}, on the wire
    // as Data1/2/3 little-endian then Data4 in order.
    const GUID_REMOTEFX: [u8; 16] = [
        0x12, 0x2F, 0x77, 0x76, // Data1 LE
        0x72, 0xBD, // Data2 LE
        0x63, 0x44, // Data3 LE
        0xAF, 0xB3, 0xB7, 0x3C, 0x9C, 0x6F, 0x78, 0x86, // Data4
    ];

    // One TS_RFX_ICAP (8 bytes): version 1.0, 64×64 tile, image mode, ICT,
    // 5/3 DWT, with the given entropy coder.
    let icap = |entropy: u8| -> [u8; 8] {
        [
            0x00, 0x01, // version = CLW_VERSION_1_0
            0x40, 0x00, // tileSize = CT_TILE_64x64
            0x02, // flags = CODEC_MODE (image mode)
            0x01, // colConvBits = CLW_COL_CONV_ICT
            0x01, // transformBits = CLW_XFORM_DWT_53_A
            entropy,
        ]
    };
    const CLW_ENTROPY_RLGR1: u8 = 0x01;
    const CLW_ENTROPY_RLGR3: u8 = 0x04;

    // TS_RFX_CAPSET: header + codecId + capsetType + numIcaps + icapLen + icaps.
    let mut capset = Vec::new();
    put_u16(0xCBC1, &mut capset); // blockType = CBY_CAPSET
    put_u32(6 + 1 + 2 + 2 + 2 + 16, &mut capset); // blockLen (= 29)
    capset.push(0x01); // codecId
    put_u16(0xCFC0, &mut capset); // capsetType = CLY_CAPSET
    put_u16(2, &mut capset); // numIcaps
    put_u16(8, &mut capset); // icapLen
    capset.extend_from_slice(&icap(CLW_ENTROPY_RLGR1));
    capset.extend_from_slice(&icap(CLW_ENTROPY_RLGR3));

    // TS_RFX_CAPS: header + numCapsets (the capset blocks follow it).
    let mut caps_data = Vec::new();
    put_u16(0xCBC0, &mut caps_data); // blockType = CBY_CAPS
    put_u32(8, &mut caps_data); // blockLen (always 8)
    put_u16(1, &mut caps_data); // numCapsets
    caps_data.extend_from_slice(&capset);

    // TS_RFX_CLNT_CAPS_CONTAINER wrapping the caps data.
    const CARDP_CAPS_CAPTURE_NON_CAC: u32 = 0x01;
    let mut props = Vec::new();
    put_u32((4 + 4 + 4 + caps_data.len()) as u32, &mut props); // length (incl. self)
    put_u32(CARDP_CAPS_CAPTURE_NON_CAC, &mut props); // captureFlags
    put_u32(caps_data.len() as u32, &mut props); // capsLength
    props.extend_from_slice(&caps_data);

    // TS_BITMAPCODEC for RemoteFX.
    let mut codec = Vec::new();
    codec.extend_from_slice(&GUID_REMOTEFX); // codecGUID
    codec.push(CODEC_ID_REMOTEFX); // codecID
    put_u16(props.len() as u16, &mut codec); // codecPropertiesLength
    codec.extend_from_slice(&props);

    // TS_BITMAPCODECS: count + array.
    let mut p = Vec::new();
    p.push(1); // bitmapCodecCount
    p.extend_from_slice(&codec);
    cap_set(CAPSET_BITMAP_CODECS, &p)
}

/// The client capability sets (concatenated) and their count. When `rfx` is set
/// the RemoteFX-enabling sets (Surface Commands + Bitmap Codecs) are appended
/// and fast-path output is requested in the General set.
fn client_capabilities(
    width: u16,
    height: u16,
    keyboard_layout: u32,
    rfx: bool,
) -> (Vec<u8>, u16) {
    let mut sets = vec![
        general_caps(rfx),
        bitmap_caps(width, height, 0x18),
        order_caps(),
        pointer_caps(),
        input_caps(keyboard_layout),
        virtual_channel_caps(),
    ];
    if rfx {
        sets.push(surface_commands_caps());
        sets.push(bitmap_codecs_caps());
    }
    (sets.concat(), sets.len() as u16)
}

/// Share Control `pduType` for a Deactivate All PDU.
const PDUTYPE_DEACTIVATE_ALL: u16 = 0x6;

/// Whether `share_pdu` is a Deactivate All PDU — the server is about to re-run
/// the capability exchange, so the client must reactivate (re-Confirm Active +
/// re-finalize) to keep the session alive.
pub fn is_deactivate_all(share_pdu: &[u8]) -> bool {
    share_pdu.len() >= 4
        && (u16::from_le_bytes([share_pdu[2], share_pdu[3]]) & 0x0f) == PDUTYPE_DEACTIVATE_ALL
}

/// Parse the shareId from a Demand Active PDU (the Share Control payload).
pub fn parse_demand_active(share_pdu: &[u8]) -> PduResult<u32> {
    ensure(share_pdu, 10)?;
    let pdu_type = u16::from_le_bytes([share_pdu[2], share_pdu[3]]) & 0x0f;
    if pdu_type != PDUTYPE_DEMAND_ACTIVE {
        return Err(PduError::InvalidField {
            field: "share_pdu_type",
            detail: format!("expected Demand Active (1), got {pdu_type}"),
        });
    }
    Ok(u32::from_le_bytes([
        share_pdu[6],
        share_pdu[7],
        share_pdu[8],
        share_pdu[9],
    ]))
}

/// Extract the server's `TS_INPUT_CAPABILITYSET.inputFlags` from a Demand
/// Active PDU (None if the PDU isn't a Demand Active or carries no input caps).
/// Demand Active: shareControlHeader(6) shareId(4) lenSourceDescriptor(2)
/// lenCombinedCapabilities(2) sourceDescriptor(var) numberCapabilities(2)
/// pad2(2) capabilitySets — each set: type(2) length(2) data(length-4).
pub fn parse_server_input_flags(share_pdu: &[u8]) -> Option<u16> {
    if share_pdu.len() < 14
        || (u16::from_le_bytes([share_pdu[2], share_pdu[3]]) & 0x0f) != PDUTYPE_DEMAND_ACTIVE
    {
        return None;
    }
    let len_src = u16::from_le_bytes([share_pdu[10], share_pdu[11]]) as usize;
    let mut off = 14usize + len_src;
    let num = u16::from_le_bytes([
        *share_pdu.get(off)?,
        *share_pdu.get(off + 1)?,
    ]) as usize;
    off += 4; // numberCapabilities + pad2Octets
    for _ in 0..num {
        let set_type = u16::from_le_bytes([*share_pdu.get(off)?, *share_pdu.get(off + 1)?]);
        let set_len = u16::from_le_bytes([*share_pdu.get(off + 2)?, *share_pdu.get(off + 3)?]) as usize;
        if set_len < 4 || off + set_len > share_pdu.len() {
            return None;
        }
        if set_type == CAPSET_INPUT {
            return Some(u16::from_le_bytes([
                *share_pdu.get(off + 4)?,
                *share_pdu.get(off + 5)?,
            ]));
        }
        off += set_len;
    }
    None
}

/// Build a Confirm Active PDU (Share Control payload) for `share_id`, sourced
/// from the client's `user_id`.
pub fn confirm_active(
    share_id: u32,
    user_id: u16,
    width: u16,
    height: u16,
    keyboard_layout: u32,
    rfx: bool,
) -> Vec<u8> {
    let (caps, num_caps) = client_capabilities(width, height, keyboard_layout, rfx);
    let source = b"rdpio";

    let mut body = Vec::new();
    put_u32(share_id, &mut body);
    put_u16(ORIGINATOR_ID, &mut body);
    put_u16(source.len() as u16, &mut body); // lengthSourceDescriptor
    put_u16((2 + 2 + caps.len()) as u16, &mut body); // lengthCombinedCapabilities
    body.extend_from_slice(source);
    put_u16(num_caps, &mut body);
    put_u16(0, &mut body); // pad2octets
    body.extend_from_slice(&caps);

    let total_len = (6 + body.len()) as u16;
    let mut out = Vec::with_capacity(total_len as usize);
    put_u16(total_len, &mut out);
    put_u16(PDUTYPE_CONFIRM_ACTIVE | PROTOCOL_VERSION, &mut out);
    put_u16(user_id, &mut out);
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_demand_active_extracts_share_id() {
        // totalLength, pduType(DEMAND|version), pduSource, shareId(LE).
        let pdu = [
            0x00, 0x00, // totalLength (ignored here)
            0x11, 0x00, // pduType = DEMANDACTIVE (1) | PROTOCOL_VERSION (0x10)
            0xea, 0x03, // pduSource
            0xea, 0x03, 0x01, 0x00, // shareId = 0x000103EA
        ];
        assert_eq!(parse_demand_active(&pdu).unwrap(), 0x0001_03EA);
    }

    #[test]
    fn parse_demand_active_rejects_other_pdu_types() {
        let pdu = [0x00, 0x00, 0x13, 0x00, 0xea, 0x03, 0x00, 0x00, 0x00, 0x00];
        assert!(parse_demand_active(&pdu).is_err()); // 0x13 = CONFIRMACTIVE, not DEMAND
    }

    #[test]
    fn detects_deactivate_all() {
        // pduType low nibble 6 = DEACTIVATEALLPDU (| PROTOCOL_VERSION 0x10).
        let deact = [0x00, 0x00, 0x16, 0x00, 0xea, 0x03];
        assert!(is_deactivate_all(&deact));
        // A Demand Active is not a Deactivate All.
        let demand = [0x00, 0x00, 0x11, 0x00, 0xea, 0x03];
        assert!(!is_deactivate_all(&demand));
        assert!(!is_deactivate_all(&[0x00, 0x00])); // too short
    }

    #[test]
    fn confirm_active_structure() {
        let pdu = confirm_active(0x0001_03EA, 1007, 1920, 1080, 0x0409, false);
        // Share Control Header: totalLength == buffer length.
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]) as usize, pdu.len());
        // pduType = CONFIRMACTIVE | PROTOCOL_VERSION.
        assert_eq!(
            u16::from_le_bytes([pdu[2], pdu[3]]),
            PDUTYPE_CONFIRM_ACTIVE | PROTOCOL_VERSION
        );
        // pduSource = user id.
        assert_eq!(u16::from_le_bytes([pdu[4], pdu[5]]), 1007);
        // shareId echoed.
        assert_eq!(
            u32::from_le_bytes([pdu[6], pdu[7], pdu[8], pdu[9]]),
            0x0001_03EA
        );
        // originatorId.
        assert_eq!(u16::from_le_bytes([pdu[10], pdu[11]]), ORIGINATOR_ID);
        // numberCapabilities == 6 (after the 5-char "rdpio" source descriptor).
        // layout: ...shareId(4) origin(2) lenSrc(2) lenCombined(2) "rdpio"(5) numCaps(2)
        let num_caps_off = 6 + 4 + 2 + 2 + 2 + 5;
        assert_eq!(
            u16::from_le_bytes([pdu[num_caps_off], pdu[num_caps_off + 1]]),
            6
        );
    }

    #[test]
    fn each_capability_set_has_the_canonical_length() {
        assert_eq!(general_caps(false).len(), 24);
        assert_eq!(general_caps(true).len(), 24); // same size, only a flag differs
        assert_eq!(bitmap_caps(1920, 1080, 0x18).len(), 28);
        assert_eq!(order_caps().len(), 88);
        assert_eq!(pointer_caps().len(), 10);
        assert_eq!(input_caps(0x0409).len(), 88);
        assert_eq!(virtual_channel_caps().len(), 8);
        assert_eq!(surface_commands_caps().len(), 12);
        assert_eq!(bitmap_codecs_caps().len(), 73);
    }

    #[test]
    fn rfx_flag_sets_fastpath_output_in_general_caps() {
        // extraFlags is the 6th u16 of the payload (after the 4-byte cap header).
        let off = 4 + 5 * 2;
        let extra = |caps: &[u8]| u16::from_le_bytes([caps[off], caps[off + 1]]);
        assert_eq!(extra(&general_caps(false)) & FASTPATH_OUTPUT_SUPPORTED, 0);
        assert_eq!(
            extra(&general_caps(true)) & FASTPATH_OUTPUT_SUPPORTED,
            FASTPATH_OUTPUT_SUPPORTED
        );
        // NO_BITMAP_COMPRESSION_HDR stays set either way.
        assert_eq!(
            extra(&general_caps(true)) & NO_BITMAP_COMPRESSION_HDR,
            NO_BITMAP_COMPRESSION_HDR
        );
    }

    #[test]
    fn rfx_adds_two_capability_sets() {
        let (_, base) = client_capabilities(1920, 1080, 0x0409, false);
        let (_, with_rfx) = client_capabilities(1920, 1080, 0x0409, true);
        assert_eq!(base, 6);
        assert_eq!(with_rfx, 8);
    }

    #[test]
    fn bitmap_codecs_advertises_remotefx() {
        let caps = bitmap_codecs_caps();
        // capsType + length header.
        assert_eq!(u16::from_le_bytes([caps[0], caps[1]]), CAPSET_BITMAP_CODECS);
        assert_eq!(u16::from_le_bytes([caps[2], caps[3]]) as usize, caps.len());
        // bitmapCodecCount == 1.
        assert_eq!(caps[4], 1);
        // codecID is our RemoteFX id (after the 16-byte GUID).
        assert_eq!(caps[4 + 1 + 16], CODEC_ID_REMOTEFX);
        // GUID Data1 little-endian: 0x76772F12 → 12 2F 77 76.
        assert_eq!(&caps[5..9], &[0x12, 0x2F, 0x77, 0x76]);
    }

    #[test]
    fn confirm_active_with_rfx_has_eight_caps() {
        let pdu = confirm_active(0x0001_03EA, 1007, 1920, 1080, 0x0409, true);
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]) as usize, pdu.len());
        let num_caps_off = 6 + 4 + 2 + 2 + 2 + 5;
        assert_eq!(
            u16::from_le_bytes([pdu[num_caps_off], pdu[num_caps_off + 1]]),
            8
        );
    }
}
