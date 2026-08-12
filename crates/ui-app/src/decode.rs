//! H.264 decode path.
//!
//! The cross-platform half is pure logic: Annex-B start-code scanning, NAL
//! classification, SPS/PPS configuration parsing (with a real Exp-Golomb bit
//! reader), and access-unit splitting. On Windows, a D3D11 hardware video
//! decoder consumes the access units and produces `ID3D11Texture2D` frames for
//! the renderer.

/// One H.264 NAL unit (data excludes the start code, includes the NAL header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit {
    /// `nal_unit_type` from the header byte (`header & 0x1f`).
    pub nal_type: u8,
    /// The raw NAL bytes (header included).
    pub data: Vec<u8>,
}

/// NAL type for a coded IDR slice.
pub const NAL_TYPE_IDR: u8 = 5;
/// NAL type for a non-IDR coded slice.
pub const NAL_TYPE_SLICE: u8 = 1;
/// NAL type for an SPS.
pub const NAL_TYPE_SPS: u8 = 7;
/// NAL type for a PPS.
pub const NAL_TYPE_PPS: u8 = 8;
/// NAL type for an SEI.
pub const NAL_TYPE_SEI: u8 = 6;

/// The smallest possible start code (3-byte `00 00 01`).
#[allow(dead_code)] // H.264 start-code marker retained for future NAL parsing
const START_CODE: [u8; 3] = [0x00, 0x00, 0x01];

/// Scan for the next Annex-B start code at or after `from`, returning the
/// index of its first byte.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            // 4-byte start code 00 00 00 01: the third byte is also 0.
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Split an Annex-B H.264 byte stream into NAL units. Trailing zero bytes are
/// trimmed from each unit.
pub fn annex_b_nal_units(stream: &[u8]) -> Vec<NalUnit> {
    let mut nals = Vec::new();
    let mut pos = 0;
    // Skip any leading zero bytes before the first start code.
    while let Some(code) = find_start_code(stream, pos) {
        let data_start = if stream[code + 2] == 1 {
            code + 3
        } else {
            code + 4
        };
        let next = find_start_code(stream, data_start);
        let end = next.unwrap_or(stream.len());
        // Trim trailing zero bytes (emulation-free Annex B padding).
        let mut data_end = end;
        while data_end > data_start && stream[data_end - 1] == 0 {
            data_end -= 1;
        }
        if data_end > data_start {
            nals.push(NalUnit {
                nal_type: stream[data_start] & 0x1f,
                data: stream[data_start..data_end].to_vec(),
            });
        }
        match next {
            Some(n) => pos = n,
            None => break,
        }
    }
    nals
}

/// One decodable access unit: all NALs between two AU boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub nals: Vec<NalUnit>,
    pub is_keyframe: bool,
}

/// Split NAL units into access units. A boundary is placed before an IDR and
/// before any VCL slice that follows another VCL slice.
pub fn split_access_units(nals: &[NalUnit]) -> Vec<AccessUnit> {
    let mut units = Vec::new();
    let mut current: Vec<NalUnit> = Vec::new();
    let mut has_vcl = false;

    for nal in nals {
        let is_vcl = nal.nal_type >= 1 && nal.nal_type <= 5;
        let boundary = if current.is_empty() {
            false
        } else {
            // A VCL NAL starts a new access unit once the current unit already
            // holds a VCL NAL (an IDR or another slice). Parameter sets and SEI
            // that precede the first VCL of an access unit belong to that unit,
            // so an SPS/PPS directly before an IDR stays with it.
            is_vcl && has_vcl
        };
        if boundary {
            units.push(AccessUnit {
                nals: std::mem::take(&mut current),
                is_keyframe: false,
            });
            has_vcl = false;
        }
        current.push(nal.clone());
        if is_vcl {
            has_vcl = true;
        }
    }
    if !current.is_empty() {
        let is_keyframe = current.iter().any(|n| n.nal_type == NAL_TYPE_IDR);
        units.push(AccessUnit {
            nals: current,
            is_keyframe,
        });
    }
    // Fix the keyframe flag on all units (an IDR inside a unit marks it).
    for unit in units.iter_mut() {
        unit.is_keyframe = unit.nals.iter().any(|n| n.nal_type == NAL_TYPE_IDR);
    }
    units
}

/// Parsed SPS/PPS configuration: resolution, profile, and level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Config {
    pub width: u16,
    pub height: u16,
    pub profile: u8,
    pub level: u8,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// A bit reader over a NAL RBSP, with Exp-Golomb decoding.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit)
    }

    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u32;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb (ue(v)).
    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(zeros)?;
        Some((1u32 << zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb (se(v)).
    fn read_se(&mut self) -> Option<i32> {
        let code_num = self.read_ue()?;
        let value = (code_num + 1) as i64 / 2;
        Some(if code_num % 2 == 0 {
            -(value as i32)
        } else {
            value as i32
        })
    }

    #[allow(dead_code)] // RBSP trailing-bit check; not needed by the current NAL walker
    fn more_rbsp(&self) -> bool {
        self.pos / 8 < self.data.len()
    }
}

/// Profiles that carry the extended chroma/scaling block.
fn is_high_profile(profile: u8) -> bool {
    matches!(
        profile,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244
    )
}

/// Parse the width/height/profile/level out of an SPS NAL (RBSP after the NAL
/// header). Returns `None` if the SPS is truncated or unrecognized.
pub fn parse_sps(sps_nal: &[u8]) -> Option<H264Config> {
    if sps_nal.len() < 4 {
        return None;
    }
    // The RBSP starts after the 1-byte NAL header; NAL data may contain
    // emulation-prevention bytes (00 00 03) which we strip.
    let rbsp = unescape_rbsp(&sps_nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile = r.read_bits(8)? as u8;
    let _constraints = r.read_bits(8)?;
    let level = r.read_bits(8)? as u8;
    let _sps_id = r.read_ue()?;

    if is_high_profile(profile) {
        let chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane = r.read_bit()?;
        }
        let _bit_depth_luma = r.read_ue()?;
        let _bit_depth_chroma = r.read_ue()?;
        let _qpprime_y_zero = r.read_bit()?;
        if r.read_bit()? == 1 {
            // seq_scaling_matrix_present_flag: 8 (or 12) scaling lists.
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.read_bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    read_scaling_list(&mut r, size)?;
                }
            }
        }
    }

    let _log2_max_frame_num = r.read_ue()?;
    let pic_order_cnt_type = r.read_ue()?;
    match pic_order_cnt_type {
        0 => {
            let _log2_max_poc_lsb = r.read_ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero = r.read_bit()?;
            let _offset_non_ref = r.read_se()?;
            let _offset_top_bottom = r.read_se()?;
            let n = r.read_ue()?;
            for _ in 0..n {
                let _offset_ref = r.read_se()?;
            }
        }
        _ => return None, // pic_order_cnt_type 2 needs no extra fields
    }
    let _max_num_ref_frames = r.read_ue()?;
    let _gaps_allowed = r.read_bit()?;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field = r.read_bit()?;
    }
    let _direct_8x8_inference = r.read_bit()?;
    let frame_cropping_flag = r.read_bit()?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        (r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?)
    } else {
        (0, 0, 0, 0)
    };

    let mut width = (pic_width_in_mbs_minus1 + 1) * 16;
    let mut height = (2 - frame_mbs_only_flag as u32) * (pic_height_in_map_units_minus1 + 1) * 16;
    // 4:2:0 chroma cropping units (chroma_format_idc == 1 default).
    let crop_unit_x = 2u32;
    let crop_unit_y = 2 * (2 - frame_mbs_only_flag as u32);
    width -= (crop_left + crop_right) * crop_unit_x;
    height -= (crop_top + crop_bottom) * crop_unit_y;

    if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        return None;
    }
    Some(H264Config {
        width: width as u16,
        height: height as u16,
        profile,
        level,
        sps: Vec::new(),
        pps: Vec::new(),
    })
}

/// Read one `scaling_list` (delta scale se values).
fn read_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.read_se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Some(())
}

/// Remove H.264 emulation-prevention bytes (`00 00 03 xx` → `00 00 xx`).
fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Extract the (SPS, PPS) pair and resolution from a list of NAL units.
pub fn parse_config(nals: &[NalUnit]) -> Option<H264Config> {
    let sps_nal = nals.iter().find(|n| n.nal_type == NAL_TYPE_SPS)?;
    let pps_nal = nals.iter().find(|n| n.nal_type == NAL_TYPE_PPS)?;
    let mut config = parse_sps(&sps_nal.data)?;
    config.sps = sps_nal.data.clone();
    config.pps = pps_nal.data.clone();
    Some(config)
}

/// Convert an access unit to the AVCC length-prefixed sample format the
/// D3D11 video decoder expects.
pub fn to_avcc(unit: &AccessUnit) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in &unit.nals {
        out.extend_from_slice(&(nal.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal.data);
    }
    out
}

/// Is the byte stream Annex-B framed (starts with a start code)?
pub fn is_annex_b(stream: &[u8]) -> bool {
    find_start_code(stream, 0).is_some()
}

// ---------------------------------------------------------------------------
// Windows: D3D11 hardware video decoder
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod d3d11;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an SPS NAL for a baseline-profile stream with the given
    /// macroblock dimensions and no cropping.
    fn build_sps(width_mbs: u32, height_mbs: u32, frame_mbs_only: u32) -> Vec<u8> {
        // bit writer: msb-first, buffering whole bytes
        struct W {
            bytes: Vec<u8>,
            acc: u32,
            nbits: u32,
        }
        impl W {
            fn new() -> Self {
                Self {
                    bytes: Vec::new(),
                    acc: 0,
                    nbits: 0,
                }
            }
            fn bit(&mut self, b: u32) {
                self.acc = (self.acc << 1) | (b & 1);
                self.nbits += 1;
                if self.nbits == 8 {
                    self.bytes.push(self.acc as u8);
                    self.acc = 0;
                    self.nbits = 0;
                }
            }
            fn bits(&mut self, v: u32, n: u32) {
                for i in (0..n).rev() {
                    self.bit((v >> i) & 1);
                }
            }
            fn ue(&mut self, v: u32) {
                let code_num = v + 1;
                let len = 32 - code_num.leading_zeros();
                for _ in 0..len - 1 {
                    self.bit(0);
                }
                self.bits(code_num, len);
            }
            /// Pad with the RBSP stop bit and zero bits to the byte boundary.
            fn finish(mut self) -> Vec<u8> {
                self.bit(1);
                while self.nbits != 0 {
                    self.bit(0);
                }
                self.bytes
            }
        }
        let mut w = W::new();
        w.bits(66, 8); // profile_idc = baseline
        w.bits(0, 8); // constraint flags
        w.bits(30, 8); // level_idc = 3.0
        w.ue(0); // sps_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // pic_order_cnt_type
        w.ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.bit(0); // gaps_in_frame_num_value_allowed
        w.ue(width_mbs - 1);
        w.ue(height_mbs - 1);
        w.bit(frame_mbs_only);
        if frame_mbs_only == 0 {
            w.bit(0); // mb_adaptive_frame_field_flag
        }
        w.bit(1); // direct_8x8_inference
        w.bit(0); // frame_cropping
        let rbsp = w.finish();
        let mut nal = vec![0x67]; // NAL header: forbidden=0, nri=3, type=7
        nal.extend_from_slice(&rbsp);
        nal
    }

    #[test]
    fn annex_b_splits_nal_units() {
        let stream = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, // SPS
            0x00, 0x00, 0x01, 0x68, 0xce, // PPS
            0x00, 0x00, 0x01, 0x65, 0x88, 0x84, // IDR slice
        ];
        let nals = annex_b_nal_units(&stream);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].nal_type, NAL_TYPE_SPS);
        assert_eq!(nals[1].nal_type, NAL_TYPE_PPS);
        assert_eq!(nals[2].nal_type, NAL_TYPE_IDR);
    }

    #[test]
    fn three_byte_start_codes_are_recognized() {
        let stream = [0x00, 0x00, 0x01, 0x67, 0x42];
        let nals = annex_b_nal_units(&stream);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].nal_type, NAL_TYPE_SPS);
    }

    #[test]
    fn access_units_split_at_idr() {
        let stream = [
            0x00, 0x00, 0x01, 0x67, 0x42, // SPS
            0x00, 0x00, 0x01, 0x68, 0xce, // PPS
            0x00, 0x00, 0x01, 0x65, 0x88, // IDR
            0x00, 0x00, 0x01, 0x41, 0x9a, // non-IDR slice
        ];
        let nals = annex_b_nal_units(&stream);
        let units = split_access_units(&nals);
        assert_eq!(units.len(), 2);
        assert!(units[0].is_keyframe);
        assert!(!units[1].is_keyframe);
    }

    #[test]
    fn sps_parses_resolution() {
        let sps = build_sps(20, 15, 1); // 320x240
        let cfg = parse_sps(&sps).unwrap();
        assert_eq!((cfg.width, cfg.height), (320, 240));
        assert_eq!(cfg.profile, 66);
        assert_eq!(cfg.level, 30);
    }

    #[test]
    fn sps_parses_interlaced_height() {
        let sps = build_sps(10, 10, 0); // frame_mbs_only = 0 → doubled height
        let cfg = parse_sps(&sps).unwrap();
        assert_eq!((cfg.width, cfg.height), (160, 320));
    }

    #[test]
    fn emulation_prevention_bytes_are_stripped() {
        let rbsp = unescape_rbsp(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x02]);
        assert_eq!(rbsp, [0x00, 0x00, 0x01, 0x00, 0x00, 0x02]);
    }

    #[test]
    fn avcc_format_length_prefixes_each_nal() {
        let nals = vec![
            NalUnit {
                nal_type: NAL_TYPE_IDR,
                data: vec![0x65, 0x88],
            },
            NalUnit {
                nal_type: NAL_TYPE_SLICE,
                data: vec![0x41],
            },
        ];
        let unit = AccessUnit {
            nals,
            is_keyframe: true,
        };
        let avcc = to_avcc(&unit);
        assert_eq!(&avcc[..4], &2u32.to_be_bytes());
        assert_eq!(&avcc[4..6], &[0x65, 0x88]);
        assert_eq!(&avcc[6..10], &1u32.to_be_bytes());
        assert_eq!(&avcc[10..11], &[0x41]);
    }

    #[test]
    fn is_annex_b_detects_framing() {
        assert!(is_annex_b(&[0x00, 0x00, 0x01, 0x67]));
        assert!(!is_annex_b(&[0x67, 0x42, 0x00, 0x1e]));
    }
}
