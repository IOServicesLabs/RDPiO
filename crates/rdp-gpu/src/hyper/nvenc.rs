//! NVIDIA's encoder (NVENC) reached directly, not through Media Foundation:
//! the bitrate changes in place (Media Foundation's NVIDIA encoder keeps its
//! HRD buffer once streaming, so a big change meant a new encoder, a ~30 ms
//! stall and a keyframe), long-term references are set per picture, and the
//! preset is NVENC's own (P1-P7, tuned for ultra-low latency).
//!
//! The session lives in a small C bridge (`nvenc/rdp_nvenc.c`) compiled
//! against NVIDIA's header, so its structures always match the driver's. Any
//! failure to open (no NVIDIA GPU, an old driver, another GPU's device) is the
//! caller's cue to use Media Foundation instead. The bridge is built twice,
//! against API 12.0 and 13.0: AV1 with long-term references needs 13.0 (a
//! driver from 570), everything else keeps to 12.0 (any driver from 522).

use core::ffi::{c_char, c_void};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

#[repr(C)]
struct Config {
    codec: u32,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    vbv_bytes: u32,
    preset: u32,
    ltr_frames: u32,
    argb: u32,
    intra_refresh: u32,
}

#[repr(C)]
struct Frame {
    force_idr: u32,
    ltr_mark: i32,
    ltr_use_mask: u32,
    timestamp: u64,
    intra_refresh: u32,
}

type OpenFn = unsafe extern "C" fn(*mut c_void, *const Config, *mut *mut c_void, *mut c_char, i32) -> i32;
type EncodeFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const Frame, *mut *const u8, *mut u32, *mut u32, *mut u32, *mut c_char, i32) -> i32;
type SetBitrateFn = unsafe extern "C" fn(*mut c_void, u32, u32, *mut c_char, i32) -> i32;
type SetPresetFn = unsafe extern "C" fn(*mut c_void, u32, *mut c_char, i32) -> i32;
type CapFn = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type CloseFn = unsafe extern "C" fn(*mut c_void);

extern "C" {
    fn rdp_nvenc_driver_api() -> u32;
    fn rdp_nvenc_open(device: *mut c_void, cfg: *const Config, out: *mut *mut c_void, err: *mut c_char, errlen: i32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn rdp_nvenc_encode(
        s: *mut c_void,
        texture: *mut c_void,
        f: *const Frame,
        data: *mut *const u8,
        len: *mut u32,
        keyframe: *mut u32,
        ltr: *mut u32,
        err: *mut c_char,
        errlen: i32,
    ) -> i32;
    fn rdp_nvenc_set_bitrate(s: *mut c_void, bitrate: u32, vbv_bytes: u32, err: *mut c_char, errlen: i32) -> i32;
    fn rdp_nvenc_set_preset(s: *mut c_void, preset: u32, err: *mut c_char, errlen: i32) -> i32;
    fn rdp_nvenc_cap(s: *mut c_void, cap: i32) -> i32;
    fn rdp_nvenc_close(s: *mut c_void);
    fn rdp_nvenc_open_v13(device: *mut c_void, cfg: *const Config, out: *mut *mut c_void, err: *mut c_char, errlen: i32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn rdp_nvenc_encode_v13(
        s: *mut c_void,
        texture: *mut c_void,
        f: *const Frame,
        data: *mut *const u8,
        len: *mut u32,
        keyframe: *mut u32,
        ltr: *mut u32,
        err: *mut c_char,
        errlen: i32,
    ) -> i32;
    fn rdp_nvenc_set_bitrate_v13(s: *mut c_void, bitrate: u32, vbv_bytes: u32, err: *mut c_char, errlen: i32) -> i32;
    fn rdp_nvenc_set_preset_v13(s: *mut c_void, preset: u32, err: *mut c_char, errlen: i32) -> i32;
    fn rdp_nvenc_cap_v13(s: *mut c_void, cap: i32) -> i32;
    fn rdp_nvenc_close_v13(s: *mut c_void);
}

/// One build of the bridge (see the module notes).
struct Api {
    open: OpenFn,
    encode: EncodeFn,
    set_bitrate: SetBitrateFn,
    set_preset: SetPresetFn,
    cap: CapFn,
    close: CloseFn,
    name: &'static str,
}

static API12: Api = Api {
    open: rdp_nvenc_open,
    encode: rdp_nvenc_encode,
    set_bitrate: rdp_nvenc_set_bitrate,
    set_preset: rdp_nvenc_set_preset,
    cap: rdp_nvenc_cap,
    close: rdp_nvenc_close,
    name: "12.0",
};

static API13: Api = Api {
    open: rdp_nvenc_open_v13,
    encode: rdp_nvenc_encode_v13,
    set_bitrate: rdp_nvenc_set_bitrate_v13,
    set_preset: rdp_nvenc_set_preset_v13,
    cap: rdp_nvenc_cap_v13,
    close: rdp_nvenc_close_v13,
    name: "13.0",
};

/// The newest NVENC API the driver offers, as (major, minor); (0, 0) without
/// an NVIDIA driver.
pub fn driver_api() -> (u32, u32) {
    let v = unsafe { rdp_nvenc_driver_api() };
    (v >> 4, v & 0xf)
}

/// `NV_ENC_CAPS_NUM_MAX_LTR_FRAMES`.
pub const CAP_MAX_LTR_FRAMES: i32 = 40;
/// `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH`.
pub const CAP_INTRA_REFRESH: i32 = 25;

fn message(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// What to do with one picture.
#[derive(Debug, Default, Clone, Copy)]
pub struct Picture {
    pub keyframe: bool,
    /// Keep it as long-term reference `n`.
    pub mark: Option<u32>,
    /// Code it from only the long-term references in this mask.
    pub from: Option<u32>,
    /// Start a gradual refresh here (sessions opened with `intra_refresh`).
    pub refresh: bool,
}

/// How a session is opened beyond its size and rate.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// 1 (fastest) to 7 (best).
    pub preset: u32,
    pub ltr_frames: u32,
    /// The textures are BGRA (else NV12).
    pub argb: bool,
    /// Pictures a gradual refresh is spread over (0: none).
    pub intra_refresh: u32,
}

/// One NVENC session on a D3D11 device. Encodes synchronously.
pub struct Session {
    raw: *mut c_void,
    api: &'static Api,
    intra_refresh: u32,
    /// Keeps the device alive as long as the session uses it.
    _device: ID3D11Device,
}

// The session is used from one thread at a time (it moves with its encoder).
unsafe impl Send for Session {}

impl Session {
    /// Open a session. AV1 with long-term references goes through API 13.0
    /// where the driver has it (else none: AV1 then repairs with keyframes);
    /// `RDPIO_NVENC_API=12|13` picks one, for measuring.
    pub fn open(
        device: &ID3D11Device,
        codec: crate::h264::Codec,
        (width, height): (u32, u32),
        fps: u32,
        bitrate: u32,
        vbv_bytes: u32,
        opts: Options,
    ) -> Result<Self, String> {
        let av1 = codec == crate::h264::Codec::Av1;
        let codec = match codec {
            crate::h264::Codec::H264 => 0,
            crate::h264::Codec::Hevc => 1,
            crate::h264::Codec::Av1 => 2,
        };
        let forced = std::env::var("RDPIO_NVENC_API").ok().and_then(|v| v.trim().parse::<u32>().ok());
        let has13 = driver_api() >= (13, 0);
        let api: &'static Api = match forced {
            Some(13) => &API13,
            Some(_) => &API12,
            None if av1 && opts.ltr_frames > 0 && has13 => &API13,
            None => &API12,
        };
        // API 12.0 can't keep AV1 references: open without them.
        let ltr_frames = if av1 && core::ptr::eq(api, &API12) { 0 } else { opts.ltr_frames };
        let cfg = Config {
            codec,
            width,
            height,
            fps,
            bitrate,
            vbv_bytes,
            preset: opts.preset,
            ltr_frames,
            argb: opts.argb as u32,
            intra_refresh: opts.intra_refresh,
        };
        let mut raw = core::ptr::null_mut();
        let mut err = [0u8; 512];
        let r = unsafe { (api.open)(device.as_raw(), &cfg, &mut raw, err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if r != 0 || raw.is_null() {
            return Err(message(&err));
        }
        Ok(Self { raw, api, intra_refresh: opts.intra_refresh, _device: device.clone() })
    }

    /// Whether this session keeps long-term references (AV1: only on API 13.0).
    pub fn keeps_ltr(&self, codec: crate::h264::Codec, asked: u32) -> bool {
        asked > 0 && (codec != crate::h264::Codec::Av1 || core::ptr::eq(self.api, &API13))
    }

    /// The bridge's API version ("12.0" or "13.0").
    pub fn api(&self) -> &'static str {
        self.api.name
    }

    /// Pictures a gradual refresh takes (0: the session can't do one).
    pub fn intra_refresh(&self) -> u32 {
        self.intra_refresh
    }

    /// One of the driver's capabilities (`NV_ENC_CAPS`) for this codec; -1
    /// where it wouldn't say.
    pub fn cap(&self, cap: i32) -> i32 {
        unsafe { (self.api.cap)(self.raw, cap) }
    }

    /// Encode one texture of the session's size and format: its access unit
    /// (Annex B, parameter sets on keyframes; AV1: a temporal unit of OBUs,
    /// a sequence header on keyframes), whether it's a keyframe, and
    /// whether it was kept as a long-term reference.
    pub fn encode(&mut self, texture: &ID3D11Texture2D, pic: Picture, timestamp: u64) -> Result<(Vec<u8>, bool, bool), String> {
        let f = Frame {
            force_idr: pic.keyframe as u32,
            ltr_mark: pic.mark.map_or(-1, |m| m as i32),
            ltr_use_mask: pic.from.unwrap_or(0),
            timestamp,
            intra_refresh: if pic.refresh { self.intra_refresh } else { 0 },
        };
        let (mut data, mut len, mut key, mut ltr) = (core::ptr::null(), 0u32, 0u32, 0u32);
        let mut err = [0u8; 512];
        let r = unsafe {
            (self.api.encode)(self.raw, texture.as_raw(), &f, &mut data, &mut len, &mut key, &mut ltr, err.as_mut_ptr() as *mut c_char, err.len() as i32)
        };
        if r != 0 {
            return Err(message(&err));
        }
        let bytes = if len == 0 { Vec::new() } else { unsafe { core::slice::from_raw_parts(data, len as usize) }.to_vec() };
        Ok((bytes, key != 0, ltr != 0))
    }

    /// A new bitrate and largest frame, in place (no keyframe).
    pub fn set_bitrate(&mut self, bitrate: u32, vbv_bytes: u32) -> Result<(), String> {
        let mut err = [0u8; 512];
        let r = unsafe { (self.api.set_bitrate)(self.raw, bitrate, vbv_bytes, err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if r != 0 {
            return Err(message(&err));
        }
        Ok(())
    }

    /// Another preset (1-7), in place: no new session, no keyframe.
    pub fn set_preset(&mut self, preset: u32) -> Result<(), String> {
        let mut err = [0u8; 512];
        let r = unsafe { (self.api.set_preset)(self.raw, preset, err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if r != 0 {
            return Err(message(&err));
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { (self.api.close)(self.raw) };
    }
}
