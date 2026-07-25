//! Fast-Path server output (MS-RDPBCGR 2.2.9.1.2) and the Surface Commands it
//! carries (2.2.9.2) — including the `TS_BITMAP_DATA_EX` that a RemoteFX or an
//! uncompressed surface update wraps.
//!
//! A server only uses fast-path output when the client advertised
//! `FASTPATH_OUTPUT_SUPPORTED` (the General capability `extraFlags`). It is the
//! *only* transport for Surface Commands, and therefore for classic RemoteFX —
//! there is no slow-path surface-command update. The first byte distinguishes
//! the two framings: a slow-path TPKT begins with version `3`; a fast-path PDU's
//! low two bits (the `action` field) are `0`.
//!
//! This module is sans-I/O: the transport reads a whole PDU (using
//! [`is_fastpath_output`] + [`output_pdu_len`] to frame it) and hands the bytes
//! to [`parse_output`], which strips the header and reassembles fragments into
//! whole updates. [`parse_surface_commands`] then turns a SURFCMDS update into
//! the positioned bitmap payloads the graphics layer decodes.

/// Update codes (`TS_FP_UPDATE` `updateHeader`, low nibble).
pub const FASTPATH_UPDATETYPE_ORDERS: u8 = 0x0;
pub const FASTPATH_UPDATETYPE_BITMAP: u8 = 0x1;
pub const FASTPATH_UPDATETYPE_PALETTE: u8 = 0x2;
pub const FASTPATH_UPDATETYPE_SYNCHRONIZE: u8 = 0x3;
pub const FASTPATH_UPDATETYPE_SURFCMDS: u8 = 0x4;
pub const FASTPATH_UPDATETYPE_PTR_NULL: u8 = 0x5;
pub const FASTPATH_UPDATETYPE_PTR_DEFAULT: u8 = 0x6;
pub const FASTPATH_UPDATETYPE_PTR_POSITION: u8 = 0x8;
pub const FASTPATH_UPDATETYPE_COLOR_POINTER: u8 = 0x9;
pub const FASTPATH_UPDATETYPE_CACHED_POINTER: u8 = 0xA;
pub const FASTPATH_UPDATETYPE_POINTER: u8 = 0xB;

// updateHeader fragmentation field (bits 4-5).
const FRAG_SINGLE: u8 = 0x0;
const FRAG_LAST: u8 = 0x1;
const FRAG_FIRST: u8 = 0x2;
const FRAG_NEXT: u8 = 0x3;

// updateHeader compression field (bits 6-7): COMPRESSION_USED → a
// compressionFlags byte follows the header.
const COMPRESSION_USED: u8 = 0x2;
// compressionFlags: the update payload is bulk-compressed (MPPC). We never
// negotiate bulk compression, so an update flagged this way is dropped.
const PACKET_COMPRESSED: u8 = 0x20;

// fpOutputHeader flags (byte 0, bits 6-7): the payload is RC4-encrypted with an
// 8-byte dataSignature ahead of it. Only seen under Standard RDP Security; we
// enable fast-path output solely over TLS (ENCRYPTION_METHOD_NONE), so an
// encrypted PDU is declined rather than mis-parsed.
const FASTPATH_OUTPUT_ENCRYPTED: u8 = 0x2;

// A reassembled fragment stream is bounded so a hostile/buggy server can't make
// us allocate without limit. A 4K RGBA frame is ~33 MiB; 64 MiB is generous.
const MAX_FRAGMENT_BYTES: usize = 64 * 1024 * 1024;

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

/// True if `first_byte` begins a fast-path output PDU (action == 0) — i.e. it is
/// not a slow-path TPKT (version 3) frame. The transport peeks one byte to pick
/// the framing.
#[inline]
pub fn is_fastpath_output(first_byte: u8) -> bool {
    (first_byte & 0x03) == 0
}

/// Total length (header + body) of the fast-path output PDU that starts at the
/// front of `buf`, or `None` if not enough bytes are present to read the length
/// field yet. The length is encoded in 1 byte, or 2 when the high bit of the
/// first length byte is set; the value covers the whole PDU including this
/// field. Mirrors [`crate::x224::read_tpkt_len`] for the slow-path framing.
#[inline]
pub fn output_pdu_len(buf: &[u8]) -> Option<usize> {
    let b1 = *buf.get(1)?;
    if b1 & 0x80 != 0 {
        let b2 = *buf.get(2)? as usize;
        Some((((b1 & 0x7f) as usize) << 8) | b2)
    } else {
        Some(b1 as usize)
    }
}

/// One fully-reassembled fast-path output update: its `code`
/// (`FASTPATH_UPDATETYPE_*`) and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathUpdate {
    pub code: u8,
    pub data: Vec<u8>,
}

/// Reassembles `FIRST`/`NEXT`/`LAST` update fragments that span several fast-path
/// PDUs (a large RemoteFX frame easily exceeds the 32 KiB PDU cap). RDP fragments
/// only one update at a time, so a single buffer suffices. Held across reads by
/// the session loop.
#[derive(Debug, Default)]
pub struct FragmentReassembler {
    buf: Vec<u8>,
    code: Option<u8>,
}

impl FragmentReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Abandon any in-flight fragment stream (e.g. after a compressed update we
    /// can't decode, which breaks the chain).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.code = None;
    }

    fn start(&mut self, code: u8, data: &[u8]) {
        self.buf.clear();
        self.buf.extend_from_slice(data);
        self.code = Some(code);
    }

    fn push(&mut self, data: &[u8]) {
        if self.code.is_none() || self.buf.len() + data.len() > MAX_FRAGMENT_BYTES {
            self.reset();
            return;
        }
        self.buf.extend_from_slice(data);
    }

    /// Append the final fragment and, if the chain is intact, return the whole
    /// reassembled update.
    fn finish(&mut self, code: u8, data: &[u8]) -> Option<FastPathUpdate> {
        if self.code != Some(code) || self.buf.len() + data.len() > MAX_FRAGMENT_BYTES {
            self.reset();
            return None;
        }
        self.buf.extend_from_slice(data);
        let out = FastPathUpdate {
            code,
            data: std::mem::take(&mut self.buf),
        };
        self.code = None;
        Some(out)
    }
}

/// Strip the fast-path output header from a whole `pdu` and return the updates
/// it completes, reassembling fragments via `frag`. A control-only or
/// still-fragmenting PDU yields an empty `Vec`. Returns `None` if the PDU is
/// encrypted (we carry no per-PDU RC4 here — fast-path output is TLS-only) or
/// the header is malformed.
pub fn parse_output(pdu: &[u8], frag: &mut FragmentReassembler) -> Option<Vec<FastPathUpdate>> {
    let b0 = *pdu.get(0)?;
    if !is_fastpath_output(b0) {
        return None;
    }
    if (b0 >> 6) & FASTPATH_OUTPUT_ENCRYPTED != 0 {
        return None; // encrypted body + dataSignature; only over legacy security
    }
    let total = output_pdu_len(pdu)?;
    if total > pdu.len() || total < 2 {
        return None;
    }
    let b1 = pdu[1];
    let header_len = if b1 & 0x80 != 0 { 3 } else { 2 };
    let body = pdu.get(header_len..total)?;
    Some(parse_updates(body, frag))
}

/// Walk the `TS_FP_UPDATE` records in a fast-path body, emitting each completed
/// update. Bulk-compressed updates are dropped (we never negotiate compression).
fn parse_updates(body: &[u8], frag: &mut FragmentReassembler) -> Vec<FastPathUpdate> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < body.len() {
        let hdr = body[off];
        off += 1;
        let code = hdr & 0x0f;
        let fragmentation = (hdr >> 4) & 0x03;
        let compression = (hdr >> 6) & 0x03;

        let mut compressed = false;
        if compression & COMPRESSION_USED != 0 {
            let Some(cf) = body.get(off).copied() else {
                break;
            };
            off += 1;
            compressed = cf & PACKET_COMPRESSED != 0;
        }

        let Some(size) = u16le(body, off) else {
            break;
        };
        off += 2;
        let size = size as usize;
        let Some(data) = body.get(off..off + size) else {
            break;
        };
        off += size;

        if compressed {
            // Can't MPPC-decompress; drop and break any fragment chain so we
            // never splice mismatched data.
            frag.reset();
            continue;
        }

        match fragmentation {
            FRAG_SINGLE => out.push(FastPathUpdate {
                code,
                data: data.to_vec(),
            }),
            FRAG_FIRST => frag.start(code, data),
            FRAG_NEXT => frag.push(data),
            FRAG_LAST => {
                if let Some(update) = frag.finish(code, data) {
                    out.push(update);
                }
            }
            _ => {}
        }
    }
    out
}

// TS_SURFCMD cmdType (MS-RDPBCGR 2.2.9.2).
const CMDTYPE_SET_SURFACE_BITS: u16 = 0x0001;
const CMDTYPE_FRAME_MARKER: u16 = 0x0004;
const CMDTYPE_STREAM_SURFACE_BITS: u16 = 0x0006;

// TS_BITMAP_DATA_EX flags: an exBitmapDataHeader (TS_COMPRESSED_BITMAP_HEADER_EX,
// 24 bytes) precedes the bitmapData. Not used by RemoteFX surface bits, but
// handled for completeness.
const EX_COMPRESSED_BITMAP_HEADER_PRESENT: u8 = 0x01;
const EX_BITMAP_HEADER_LEN: usize = 24;

/// A SET/STREAM_SURFACE_BITS command: a destination rectangle on the desktop and
/// the codec-tagged bitmap payload that fills it. `codec_id` is the client-
/// assigned id from the Bitmap Codecs capability (`0` = uncompressed); the
/// graphics layer maps it to RemoteFX or a raw blit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceBits {
    pub dest_left: u16,
    pub dest_top: u16,
    pub dest_right: u16,
    pub dest_bottom: u16,
    pub bpp: u8,
    pub codec_id: u8,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Parse a `FASTPATH_UPDATETYPE_SURFCMDS` update body (a series of `TS_SURFCMD`)
/// into its surface-bits commands. Frame markers are skipped; parsing stops at
/// the first unknown command type (whose length we can't know) or malformed
/// record, returning whatever was decoded so far.
pub fn parse_surface_commands(data: &[u8]) -> Vec<SurfaceBits> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 2 <= data.len() {
        let cmd_type = u16le(data, off).unwrap();
        match cmd_type {
            CMDTYPE_SET_SURFACE_BITS | CMDTYPE_STREAM_SURFACE_BITS => {
                match parse_set_surface_bits(data, off + 2) {
                    Some((bits, next)) => {
                        out.push(bits);
                        off = next;
                    }
                    None => break,
                }
            }
            CMDTYPE_FRAME_MARKER => off += 8, // cmdType(2) frameAction(2) frameId(4)
            _ => break,
        }
    }
    out
}

/// Parse the destination rectangle + `TS_BITMAP_DATA_EX` starting at `start`
/// (just past the cmdType). Returns the parsed command and the offset of the
/// next command.
fn parse_set_surface_bits(data: &[u8], start: usize) -> Option<(SurfaceBits, usize)> {
    let dest_left = u16le(data, start)?;
    let dest_top = u16le(data, start + 2)?;
    let dest_right = u16le(data, start + 4)?;
    let dest_bottom = u16le(data, start + 6)?;

    let ex = start + 8;
    let bpp = *data.get(ex)?;
    let flags = *data.get(ex + 1)?;
    // data[ex + 2] is reserved.
    let codec_id = *data.get(ex + 3)?;
    let width = u16le(data, ex + 4)?;
    let height = u16le(data, ex + 6)?;
    let blen = u32le(data, ex + 8)? as usize;

    let mut payload = ex + 12;
    if flags & EX_COMPRESSED_BITMAP_HEADER_PRESENT != 0 {
        payload += EX_BITMAP_HEADER_LEN;
    }
    let body = data.get(payload..payload + blen)?;
    let next = payload + blen;

    Some((
        SurfaceBits {
            dest_left,
            dest_top,
            dest_right,
            dest_bottom,
            bpp,
            codec_id,
            width,
            height,
            data: body.to_vec(),
        },
        next,
    ))
}

// --- Fast-path client INPUT (MS-RDPBCGR 2.2.8.1.2) ---------------------------
//
// mstsc sends input on the fast path: a tiny header (no TPKT/MCS/share wrappers)
// the server injects immediately, instead of the slow-path `TS_INPUT_PDU` it
// processes on a queued timer. `input_pdu` translates the same 12-byte
// `EventBytes` the slow path uses, so the high-level input mapping is unchanged.
// The result is written straight to the (TLS) stream; the server tells it apart
// from a TPKT by the first byte's low two bits (`action`: TPKT v3 → 0x03 → `11`,
// fast-path → `00`).

/// `eventCode` values (`TS_FP_INPUT_EVENT.eventHeader`, high 3 bits).
const FASTPATH_INPUT_EVENT_SCANCODE: u8 = 0;
const FASTPATH_INPUT_EVENT_MOUSE: u8 = 1;
const FASTPATH_INPUT_EVENT_MOUSEX: u8 = 2;
const FASTPATH_INPUT_EVENT_SYNC: u8 = 3;
const FASTPATH_INPUT_EVENT_UNICODE: u8 = 4;
const FASTPATH_INPUT_EVENT_RELPOINTER: u8 = 5;

/// Fast-path scancode `eventFlags` (low 5 bits of the event header).
const FASTPATH_INPUT_KBDFLAGS_RELEASE: u8 = 0x01;
const FASTPATH_INPUT_KBDFLAGS_EXTENDED: u8 = 0x02;

/// Translate one 12-byte slow-path [`input::EventBytes`] to a fast-path event
/// (`eventHeader` + body), appending it to `out`. Returns `false` for an event
/// type with no fast-path encoding (none today — all six map cleanly).
fn push_fp_event(ev: &crate::input::EventBytes, out: &mut Vec<u8>) -> bool {
    let msg = u16::from_le_bytes([ev[4], ev[5]]);
    let a = u16::from_le_bytes([ev[6], ev[7]]);
    // `b`/`c` (bytes 8..12) are copied verbatim for the pointer events — same
    // little-endian pointerFlags/x/y (or dx/dy) layout as the slow path.
    let bc = &ev[8..12];
    let header = |code: u8, flags: u8| flags | (code << 5);
    match msg {
        // SCANCODE: keyboardFlags → fast-path kbd eventFlags; keyCode is one byte.
        0x0004 => {
            let mut flags = 0u8;
            if a & crate::input::KBDFLAGS_RELEASE != 0 {
                flags |= FASTPATH_INPUT_KBDFLAGS_RELEASE;
            }
            if a & crate::input::KBDFLAGS_EXTENDED != 0 {
                flags |= FASTPATH_INPUT_KBDFLAGS_EXTENDED;
            }
            out.push(header(FASTPATH_INPUT_EVENT_SCANCODE, flags));
            out.push(ev[8]); // keyCode (low byte of `b`)
        }
        // UNICODE: same release bit; 16-bit code unit.
        0x0005 => {
            let flags = if a & crate::input::KBDFLAGS_RELEASE != 0 {
                FASTPATH_INPUT_KBDFLAGS_RELEASE
            } else {
                0
            };
            out.push(header(FASTPATH_INPUT_EVENT_UNICODE, flags));
            out.extend_from_slice(&ev[8..10]); // unicodeCode
        }
        // MOUSE / MOUSEX / RELPOINTER: pointerFlags(`a`) + two 16-bit fields(`bc`).
        0x8001 | 0x8002 | 0x8004 => {
            let code = match msg {
                0x8001 => FASTPATH_INPUT_EVENT_MOUSE,
                0x8002 => FASTPATH_INPUT_EVENT_MOUSEX,
                _ => FASTPATH_INPUT_EVENT_RELPOINTER,
            };
            out.push(header(code, 0));
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(bc);
        }
        // SYNC: toggle flags ride the event header's low bits; no body. The slow
        // path stores them as a u32 at bytes 8..12; the low byte holds the bits.
        0x0000 => {
            out.push(header(FASTPATH_INPUT_EVENT_SYNC, ev[8] & 0x1F));
        }
        _ => return false,
    }
    true
}

/// Build a `TS_FP_INPUT_PDU` carrying `events`, ready to write directly to the
/// (TLS) stream. Returns `None` if there are no events or one fails to translate
/// (the caller then falls back to the slow path). Caps at 255 events per PDU —
/// input batches are tiny, far below that.
pub fn input_pdu(events: &[crate::input::EventBytes]) -> Option<Vec<u8>> {
    if events.is_empty() || events.len() > 255 {
        return None;
    }
    let mut body = Vec::with_capacity(events.len() * 7);
    for ev in events {
        if !push_fp_event(ev, &mut body) {
            return None;
        }
    }
    let num = events.len();
    // fpInputHeader: action=0 (fast-path), flags=0; numEvents in bits 2..6 when it
    // fits in 4 bits, else 0 with a trailing numberEvents byte.
    let (header_byte, sep_count) = if num <= 15 {
        ((num as u8) << 2, false)
    } else {
        (0, true)
    };
    // total = header(1) + length(2, always the 2-byte form) + [numberEvents(1)] + body.
    let total = 1 + 2 + usize::from(sep_count) + body.len();
    let mut out = Vec::with_capacity(total);
    out.push(header_byte);
    // length1/length2: 2-byte big-endian, high bit of the first byte set.
    out.extend_from_slice(&(0x8000 | total as u16).to_be_bytes());
    if sep_count {
        out.push(num as u8);
    }
    out.extend_from_slice(&body);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fast-path output PDU around `body` (the concatenated updates),
    /// choosing the 1- or 2-byte length form automatically.
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut pdu = vec![0u8]; // action = fast-path, flags = 0
        let total_short = body.len() + 2;
        if total_short <= 0x7f {
            pdu.push(total_short as u8);
        } else {
            let total = body.len() + 3;
            pdu.push(0x80 | ((total >> 8) as u8));
            pdu.push((total & 0xff) as u8);
        }
        pdu.extend_from_slice(body);
        pdu
    }

    /// A single (FRAG_SINGLE, uncompressed) update record.
    fn single(code: u8, data: &[u8]) -> Vec<u8> {
        let mut u = vec![code & 0x0f]; // frag=SINGLE, compression=0
        u.extend_from_slice(&(data.len() as u16).to_le_bytes());
        u.extend_from_slice(data);
        u
    }

    fn frag_record(code: u8, frag: u8, data: &[u8]) -> Vec<u8> {
        let mut u = vec![(code & 0x0f) | (frag << 4)];
        u.extend_from_slice(&(data.len() as u16).to_le_bytes());
        u.extend_from_slice(data);
        u
    }

    #[test]
    fn distinguishes_framing() {
        assert!(is_fastpath_output(0x00));
        assert!(is_fastpath_output(0x80)); // action bits still 0
        assert!(!is_fastpath_output(0x03)); // TPKT version 3
    }

    #[test]
    fn one_byte_and_two_byte_length() {
        assert_eq!(output_pdu_len(&[0x00, 0x10]), Some(0x10));
        // high bit set → 2-byte: 0x81,0x00 → 0x100
        assert_eq!(output_pdu_len(&[0x00, 0x81, 0x00]), Some(0x100));
        assert_eq!(output_pdu_len(&[0x00]), None); // need the length byte
    }

    #[test]
    fn parses_a_single_update() {
        let pdu = frame(&single(FASTPATH_UPDATETYPE_BITMAP, &[1, 2, 3, 4]));
        let mut frag = FragmentReassembler::new();
        let updates = parse_output(&pdu, &mut frag).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].code, FASTPATH_UPDATETYPE_BITMAP);
        assert_eq!(updates[0].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn two_updates_in_one_pdu() {
        let mut body = single(FASTPATH_UPDATETYPE_SURFCMDS, &[0xAA]);
        body.extend_from_slice(&single(FASTPATH_UPDATETYPE_PTR_POSITION, &[1, 0, 2, 0]));
        let pdu = frame(&body);
        let mut frag = FragmentReassembler::new();
        let updates = parse_output(&pdu, &mut frag).unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].code, FASTPATH_UPDATETYPE_SURFCMDS);
        assert_eq!(updates[1].code, FASTPATH_UPDATETYPE_PTR_POSITION);
    }

    #[test]
    fn reassembles_fragments_across_pdus() {
        let mut frag = FragmentReassembler::new();
        let code = FASTPATH_UPDATETYPE_SURFCMDS;

        let first = frame(&frag_record(code, FRAG_FIRST, &[1, 2, 3]));
        assert!(parse_output(&first, &mut frag).unwrap().is_empty());

        let next = frame(&frag_record(code, FRAG_NEXT, &[4, 5]));
        assert!(parse_output(&next, &mut frag).unwrap().is_empty());

        let last = frame(&frag_record(code, FRAG_LAST, &[6]));
        let updates = parse_output(&last, &mut frag).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].data, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn compressed_update_is_dropped_not_misparsed() {
        // header: code=BITMAP, frag=SINGLE, compression=USED; + compressionFlags
        // with PACKET_COMPRESSED; + size; + (garbage) payload.
        let mut u = vec![FASTPATH_UPDATETYPE_BITMAP | (COMPRESSION_USED << 6)];
        u.push(PACKET_COMPRESSED);
        u.extend_from_slice(&3u16.to_le_bytes());
        u.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
        let pdu = frame(&u);
        let mut frag = FragmentReassembler::new();
        let updates = parse_output(&pdu, &mut frag).unwrap();
        assert!(updates.is_empty());
    }

    #[test]
    fn encrypted_pdu_is_declined() {
        // flags bit for FASTPATH_OUTPUT_ENCRYPTED is bits 6-7 of byte0 → 0x80.
        let pdu = [0x80u8, 0x04, 0x00, 0x00];
        let mut frag = FragmentReassembler::new();
        assert!(parse_output(&pdu, &mut frag).is_none());
    }

    #[test]
    fn parses_set_surface_bits() {
        // TS_SURFCMD: cmdType=SET_SURFACE_BITS, rect (10,20,74,84),
        // TS_BITMAP_DATA_EX: bpp=32, flags=0, reserved=0, codecID=3, w=64, h=64,
        // bitmapDataLength=4, data=[9,8,7,6].
        let mut d = Vec::new();
        d.extend_from_slice(&CMDTYPE_SET_SURFACE_BITS.to_le_bytes());
        d.extend_from_slice(&10u16.to_le_bytes());
        d.extend_from_slice(&20u16.to_le_bytes());
        d.extend_from_slice(&74u16.to_le_bytes());
        d.extend_from_slice(&84u16.to_le_bytes());
        d.extend_from_slice(&[32, 0, 0, 3]); // bpp, flags, reserved, codecID
        d.extend_from_slice(&64u16.to_le_bytes());
        d.extend_from_slice(&64u16.to_le_bytes());
        d.extend_from_slice(&4u32.to_le_bytes());
        d.extend_from_slice(&[9, 8, 7, 6]);

        let cmds = parse_surface_commands(&d);
        assert_eq!(cmds.len(), 1);
        let c = &cmds[0];
        assert_eq!((c.dest_left, c.dest_top, c.dest_right, c.dest_bottom), (10, 20, 74, 84));
        assert_eq!(c.codec_id, 3);
        assert_eq!((c.width, c.height), (64, 64));
        assert_eq!(c.data, vec![9, 8, 7, 6]);
    }

    #[test]
    fn frame_marker_is_skipped_before_surface_bits() {
        let mut d = Vec::new();
        // FRAME_MARKER begin: cmdType(2)=4, frameAction(2)=0, frameId(4)=1.
        d.extend_from_slice(&CMDTYPE_FRAME_MARKER.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        // then a SET_SURFACE_BITS.
        d.extend_from_slice(&CMDTYPE_SET_SURFACE_BITS.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes()); // left
        d.extend_from_slice(&0u16.to_le_bytes()); // top
        d.extend_from_slice(&64u16.to_le_bytes()); // right
        d.extend_from_slice(&64u16.to_le_bytes()); // bottom
        d.extend_from_slice(&[32, 0, 0, 0]); // bpp, flags, reserved, codecID=0
        d.extend_from_slice(&2u16.to_le_bytes()); // width
        d.extend_from_slice(&2u16.to_le_bytes()); // height
        d.extend_from_slice(&1u32.to_le_bytes()); // bitmapDataLength
        d.push(0x42);

        let cmds = parse_surface_commands(&d);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].codec_id, 0);
        assert_eq!(cmds[0].data, vec![0x42]);
    }

    #[test]
    fn surface_bits_with_ex_header_present() {
        let mut d = Vec::new();
        d.extend_from_slice(&CMDTYPE_STREAM_SURFACE_BITS.to_le_bytes());
        d.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // rect zeros
        d.extend_from_slice(&[32, EX_COMPRESSED_BITMAP_HEADER_PRESENT, 0, 5]);
        d.extend_from_slice(&1u16.to_le_bytes()); // width
        d.extend_from_slice(&1u16.to_le_bytes()); // height
        d.extend_from_slice(&2u32.to_le_bytes()); // bitmapDataLength
        d.extend_from_slice(&[0u8; EX_BITMAP_HEADER_LEN]); // exBitmapDataHeader
        d.extend_from_slice(&[0xAB, 0xCD]);

        let cmds = parse_surface_commands(&d);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].codec_id, 5);
        assert_eq!(cmds[0].data, vec![0xAB, 0xCD]);
    }

    #[test]
    fn garbage_never_panics() {
        let mut frag = FragmentReassembler::new();
        for seed in 0u16..2000 {
            let b = seed.to_le_bytes();
            let junk = [b[0], b[1], b[0].wrapping_mul(3), b[1] ^ 0x5a, b[0].wrapping_add(7)];
            let _ = parse_output(&junk, &mut frag);
            let _ = parse_surface_commands(&junk);
            let _ = output_pdu_len(&junk);
        }
    }

    // --- Fast-path INPUT -----------------------------------------------------
    use crate::input;

    /// Strip the fast-path input header, returning (numEvents, events-slice).
    fn unframe_input(pdu: &[u8]) -> (usize, &[u8]) {
        assert_eq!(pdu[0] & 0x03, 0, "action must be fast-path (0)");
        // 2-byte big-endian length with the high bit set.
        assert_eq!(pdu[1] & 0x80, 0x80, "length should use the 2-byte form");
        let total = u16::from_be_bytes([pdu[1] & 0x7f, pdu[2]]) as usize;
        assert_eq!(total, pdu.len(), "length covers the whole PDU");
        let hdr_num = (pdu[0] >> 2) & 0x0f;
        if hdr_num == 0 {
            (pdu[3] as usize, &pdu[4..]) // separate numberEvents byte
        } else {
            (hdr_num as usize, &pdu[3..])
        }
    }

    #[test]
    fn fp_scancode_release_extended() {
        let ev = input::keyboard_event(input::KBDFLAGS_RELEASE | input::KBDFLAGS_EXTENDED, 0x1D);
        let pdu = input_pdu(&[ev]).unwrap();
        let (n, body) = unframe_input(&pdu);
        assert_eq!(n, 1);
        // eventCode 0 (scancode) in high 3 bits; flags RELEASE|EXTENDED = 0x03.
        assert_eq!(body[0] >> 5, FASTPATH_INPUT_EVENT_SCANCODE);
        assert_eq!(body[0] & 0x1f, 0x03);
        assert_eq!(body[1], 0x1D); // keyCode
        assert_eq!(body.len(), 2);
    }

    #[test]
    fn fp_mouse_and_relpointer_bodies() {
        let m = input::mouse_event(input::PTRFLAGS_MOVE, 640, 480);
        let mp = input_pdu(&[m]).unwrap();
        let body = unframe_input(&mp).1;
        assert_eq!(body[0] >> 5, FASTPATH_INPUT_EVENT_MOUSE);
        assert_eq!(u16::from_le_bytes([body[1], body[2]]), input::PTRFLAGS_MOVE);
        assert_eq!(u16::from_le_bytes([body[3], body[4]]), 640);
        assert_eq!(u16::from_le_bytes([body[5], body[6]]), 480);

        // Relative aim: dx/dy are signed and survive the round trip.
        let r = input::rel_mouse_event(input::PTRFLAGS_MOVE, -5, 7);
        let rp = input_pdu(&[r]).unwrap();
        let rb = unframe_input(&rp).1;
        assert_eq!(rb[0] >> 5, FASTPATH_INPUT_EVENT_RELPOINTER);
        assert_eq!(i16::from_le_bytes([rb[3], rb[4]]), -5);
        assert_eq!(i16::from_le_bytes([rb[5], rb[6]]), 7);
    }

    #[test]
    fn fp_unicode_and_sync() {
        let u = input::unicode_event(0, 0x20AC); // euro sign, key down
        let up = input_pdu(&[u]).unwrap();
        let ub = unframe_input(&up).1;
        assert_eq!(ub[0] >> 5, FASTPATH_INPUT_EVENT_UNICODE);
        assert_eq!(u16::from_le_bytes([ub[1], ub[2]]), 0x20AC);
        assert_eq!(ub.len(), 3);

        let s = input::sync_event(0x4); // CAPS_LOCK
        let sp = input_pdu(&[s]).unwrap();
        let sb = unframe_input(&sp).1;
        assert_eq!(sb[0] >> 5, FASTPATH_INPUT_EVENT_SYNC);
        assert_eq!(sb[0] & 0x1f, 0x04);
        assert_eq!(sb.len(), 1); // no body
    }

    #[test]
    fn fp_batch_packs_num_events() {
        let evs = [
            input::mouse_event(input::PTRFLAGS_MOVE, 1, 2),
            input::keyboard_event(0, 0x1E),
        ];
        let pdu = input_pdu(&evs).unwrap();
        let (n, _) = unframe_input(&pdu);
        assert_eq!(n, 2);
        // numEvents packed in the header (≤15), no separate byte.
        assert_eq!((pdu[0] >> 2) & 0x0f, 2);
        assert!(input_pdu(&[]).is_none());
    }
}
