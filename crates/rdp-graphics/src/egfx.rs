//! EGFX pipeline: turn a reassembled graphics-channel message into EGFX
//! commands. A channel message is ZGFX-compressed (see [`crate::zgfx`]) and,
//! once decompressed, is one or more concatenated RDPGFX PDUs (see
//! [`rdp_pdu::gfx`]). This module wires the two together and exposes the small
//! client PDUs (caps advertise, frame acknowledge) the graphics endpoint sends.

use crate::zgfx::Zgfx;
use rdp_pdu::gfx::{self, GfxCommand};

/// RDPGFX caps flag: the client supports AVC420 (set in a CAPVERSION_81 capset).
pub const CAPS_FLAG_AVC420_ENABLED: u32 = 0x10;
/// RDPGFX caps flag: AVC (H.264) is disabled — the server must use the
/// progressive / ClearCodec / planar codecs instead (set in CAPVERSION_10+).
pub const CAPS_FLAG_AVC_DISABLED: u32 = 0x20;

/// Full caps (the default): advertise AVC444 + AVC420 region updates
/// (CAPVERSION_10) down through the legacy versions. Restricting to ≤ v10 keeps
/// the server on H.264 rather than the progressive codecs. Used when a local
/// GPU H.264 decoder is available, where AVC is the fastest path.
pub const CAPS_FULL: &[(u32, u32)] = &[
    (gfx::CAPVERSION_10, 0),
    (gfx::CAPVERSION_81, 0),
    (gfx::CAPVERSION_8, 0),
];

/// AVC420-only: advertise CAPVERSION_81 with AVC420_ENABLED but *not* the
/// CAPVERSION_10 that would invite AVC444. For a GPU-less client this keeps a
/// single H.264 stream while avoiding AVC444's two software decodes plus the
/// per-pixel chroma combine — the most expensive CPU path.
///
/// Also the `--gaming` default even on a GPU client: AVC444 makes a CPU-only
/// *host* software-encode two H.264 streams per frame (main + aux chroma), and
/// the GPU decode path discards the aux stream anyway — so AVC420-only ≈ halves
/// the host's per-frame encode cost for no visible loss (luma stays full-res).
pub const CAPS_AVC420_ONLY: &[(u32, u32)] = &[
    (gfx::CAPVERSION_81, CAPS_FLAG_AVC420_ENABLED),
    (gfx::CAPVERSION_8, 0),
];

/// No AVC: advertise CAPVERSION_10 with AVC disabled, so the server sends
/// ClearCodec / planar / progressive — which this client decodes efficiently on
/// the CPU and which beats software H.264 at desktop resolution.
pub const CAPS_NO_AVC: &[(u32, u32)] = &[
    (gfx::CAPVERSION_10, CAPS_FLAG_AVC_DISABLED),
    (gfx::CAPVERSION_8, 0),
];

/// Back-compat alias for the default (full) caps.
pub const ADVERTISED_CAPS: &[(u32, u32)] = CAPS_FULL;

/// Stateful EGFX decoder for one connection. Owns the ZGFX history, which must
/// persist across channel messages, plus the caps this client advertises (which
/// the client tunes to whether a local GPU H.264 decoder exists).
pub struct GfxPipeline {
    zgfx: Zgfx,
    caps: Vec<(u32, u32)>,
}

impl Default for GfxPipeline {
    fn default() -> Self {
        Self::with_caps(CAPS_FULL.to_vec())
    }
}

impl GfxPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pipeline that advertises a specific caps set (see [`CAPS_FULL`],
    /// [`CAPS_AVC420_ONLY`], [`CAPS_NO_AVC`]).
    pub fn with_caps(caps: Vec<(u32, u32)>) -> Self {
        Self {
            zgfx: Zgfx::default(),
            caps,
        }
    }

    /// Decompress a graphics-channel message and parse the EGFX commands it
    /// carries. Returns `None` if ZGFX decompression fails.
    pub fn process(&mut self, channel_message: &[u8]) -> Option<Vec<GfxCommand>> {
        let decompressed = self.zgfx.decompress(channel_message)?;
        dump_gfx_payload(&decompressed);
        Some(gfx::parse_commands(&decompressed))
    }

    /// The client `RDPGFX_CAPS_ADVERTISE_PDU` to send right after the graphics
    /// channel opens (wrap in a DRDYNVC data PDU before sending).
    pub fn caps_advertise(&self) -> Vec<u8> {
        gfx::caps_advertise(&self.caps)
    }

    /// A client `RDPGFX_FRAME_ACKNOWLEDGE_PDU` for a completed frame, reporting
    /// `queue_depth` (the client's decode backlog) for server-side flow control.
    pub fn frame_acknowledge(
        &self,
        queue_depth: u32,
        frame_id: u32,
        total_frames_decoded: u32,
    ) -> Vec<u8> {
        gfx::frame_acknowledge(queue_depth, frame_id, total_frames_decoded)
    }
}

/// Diagnostic: when `RDPIO_DUMP_GFX` is set, append every decompressed EGFX
/// payload (one or more concatenated RDPGFX PDUs) to `<dir>/gfx_<pid>.bin` as
/// length-prefixed records `[len u32][bytes]`, in arrival order. Replaying these
/// through `parse_commands` + the renderer reproduces the live desktop
/// deterministically — including the cache/copy commands the ClearCodec-only
/// capture omits. Buffered + flushed; the renderer is single-threaded here so no
/// locking is needed beyond the process-wide writer.
fn dump_gfx_payload(decompressed: &[u8]) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    // A plain File (no BufWriter): each write_all is one syscall straight to the
    // OS cache, which persists on process exit even though this static is never
    // dropped — so no per-payload flush is needed, keeping decode-thread overhead
    // (and the server's queueDepth) close to a normal run.
    static W: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let writer = W.get_or_init(|| {
        let dir = std::env::var("RDPIO_DUMP_GFX").ok()?;
        let _ = std::fs::create_dir_all(&dir);
        let path = std::path::Path::new(&dir).join(format!("gfx_{}.bin", std::process::id()));
        Some(Mutex::new(std::fs::File::create(path).ok()?))
    });
    let Some(m) = writer else { return };
    if let Ok(mut f) = m.lock() {
        let mut rec = Vec::with_capacity(decompressed.len() + 4);
        rec.extend_from_slice(&(decompressed.len() as u32).to_le_bytes());
        rec.extend_from_slice(decompressed);
        let _ = f.write_all(&rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap `egfx` bytes in an uncompressed single-segment ZGFX blob.
    fn zgfx_uncompressed(egfx: &[u8]) -> Vec<u8> {
        let mut out = vec![0xE0, 0x04]; // SEGMENTED_SINGLE, RDP8 uncompressed
        out.extend_from_slice(egfx);
        out
    }

    /// Build a raw EGFX PDU (8-byte header + body).
    fn egfx_pdu(cmd_id: u16, body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&cmd_id.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn process_decompresses_and_parses() {
        let mut pipe = GfxPipeline::new();
        // A START_FRAME (timestamp 16, frame 7) followed by an END_FRAME (7).
        let mut egfx = egfx_pdu(gfx::CMDID_START_FRAME, &[0x10, 0, 0, 0, 0x07, 0, 0, 0]);
        egfx.extend_from_slice(&egfx_pdu(gfx::CMDID_END_FRAME, &[0x07, 0, 0, 0]));
        let msg = zgfx_uncompressed(&egfx);

        let cmds = pipe.process(&msg).unwrap();
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
    fn caps_advertise_is_well_formed() {
        let pipe = GfxPipeline::new();
        let adv = pipe.caps_advertise();
        let h = gfx::GfxHeader::parse(&adv).unwrap();
        assert_eq!(h.cmd_id, gfx::CMDID_CAPS_ADVERTISE);
        assert_eq!(h.pdu_length as usize, adv.len());
    }

    /// Each degraded caps profile still produces a well-formed advertise PDU,
    /// and the AVC-disabled / AVC420-only sets differ from the full set (so the
    /// GPU-aware choice actually changes the wire bytes).
    #[test]
    fn degraded_caps_profiles_are_well_formed_and_distinct() {
        let full = GfxPipeline::with_caps(CAPS_FULL.to_vec()).caps_advertise();
        let avc420 = GfxPipeline::with_caps(CAPS_AVC420_ONLY.to_vec()).caps_advertise();
        let no_avc = GfxPipeline::with_caps(CAPS_NO_AVC.to_vec()).caps_advertise();
        for adv in [&full, &avc420, &no_avc] {
            let h = gfx::GfxHeader::parse(adv).unwrap();
            assert_eq!(h.cmd_id, gfx::CMDID_CAPS_ADVERTISE);
            assert_eq!(h.pdu_length as usize, adv.len());
        }
        assert_ne!(full, avc420);
        assert_ne!(full, no_avc);
        assert_ne!(avc420, no_avc);
    }

    #[test]
    fn frame_ack_round_trips_through_parser() {
        let pipe = GfxPipeline::new();
        let ack = pipe.frame_acknowledge(0, 42, 100);
        let h = gfx::GfxHeader::parse(&ack).unwrap();
        assert_eq!(h.cmd_id, gfx::CMDID_FRAME_ACKNOWLEDGE);
    }

    #[test]
    fn bad_zgfx_returns_none() {
        let mut pipe = GfxPipeline::new();
        assert!(pipe.process(&[0x99, 0x00]).is_none());
    }
}
