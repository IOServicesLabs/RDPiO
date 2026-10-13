//! NVIDIA's encoder (NVENC) reached directly, not through Media Foundation:
//! the bitrate changes in place (Media Foundation's NVIDIA encoder keeps its
//! HRD buffer once streaming, so a big change meant a new encoder, a ~30 ms
//! stall and a keyframe), long-term references are set per picture, and the
//! preset is NVENC's own (P1-P7, tuned for ultra-low latency).
//!
//! The session lives in a small C bridge (`nvenc/rdp_nvenc.c`) compiled
//! against NVIDIA's header, so its structures always match the driver's. Any
//! failure to open (no NVIDIA GPU, an old driver, another GPU's device) is the
//! caller's cue to use Media Foundation instead.

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
}

#[repr(C)]
struct Frame {
    force_idr: u32,
    ltr_mark: i32,
    ltr_use_mask: u32,
    timestamp: u64,
}

extern "C" {
    fn rdp_nvenc_open(device: *mut c_void, cfg: *const Config, out: *mut *mut c_void, err: *mut c_char, errlen: i32) -> i32;
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
    fn rdp_nvenc_close(s: *mut c_void);
}

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
}

/// One NVENC session on a D3D11 device. Encodes synchronously.
pub struct Session {
    raw: *mut c_void,
    /// Keeps the device alive as long as the session uses it.
    _device: ID3D11Device,
}

// The session is used from one thread at a time (it moves with its encoder).
unsafe impl Send for Session {}

impl Session {
    /// `codec`: 0 H.264, 1 HEVC. `preset`: 1 (fastest) to 7 (best).
    /// `argb`: the textures are BGRA (else NV12).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        device: &ID3D11Device,
        hevc: bool,
        (width, height): (u32, u32),
        fps: u32,
        bitrate: u32,
        vbv_bytes: u32,
        preset: u32,
        ltr_frames: u32,
        argb: bool,
    ) -> Result<Self, String> {
        let cfg = Config { codec: hevc as u32, width, height, fps, bitrate, vbv_bytes, preset, ltr_frames, argb: argb as u32 };
        let mut raw = core::ptr::null_mut();
        let mut err = [0u8; 512];
        let r = unsafe { rdp_nvenc_open(device.as_raw(), &cfg, &mut raw, err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if r != 0 || raw.is_null() {
            return Err(message(&err));
        }
        Ok(Self { raw, _device: device.clone() })
    }

    /// Encode one texture of the session's size and format: its access unit
    /// (Annex B, parameter sets on keyframes), whether it's a keyframe, and
    /// whether it was kept as a long-term reference.
    pub fn encode(&mut self, texture: &ID3D11Texture2D, pic: Picture, timestamp: u64) -> Result<(Vec<u8>, bool, bool), String> {
        let f = Frame {
            force_idr: pic.keyframe as u32,
            ltr_mark: pic.mark.map_or(-1, |m| m as i32),
            ltr_use_mask: pic.from.unwrap_or(0),
            timestamp,
        };
        let (mut data, mut len, mut key, mut ltr) = (core::ptr::null(), 0u32, 0u32, 0u32);
        let mut err = [0u8; 512];
        let r = unsafe {
            rdp_nvenc_encode(self.raw, texture.as_raw(), &f, &mut data, &mut len, &mut key, &mut ltr, err.as_mut_ptr() as *mut c_char, err.len() as i32)
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
        let r = unsafe { rdp_nvenc_set_bitrate(self.raw, bitrate, vbv_bytes, err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if r != 0 {
            return Err(message(&err));
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { rdp_nvenc_close(self.raw) };
    }
}
