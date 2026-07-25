//! H.264 video via Media Foundation. Three transforms live here:
//!
//! - [`H264GpuDecoder`] — the system H.264 decoder bound to the D3D11 device
//!   (DXVA): decode runs on the GPU and outputs NV12 textures (zero-copy). This
//!   is the primary AVC420 path.
//! - [`H264Decoder`] — the synchronous, system-memory MFT
//!   (`MFT_ENUM_FLAG_SYNCMFT`): decodes Annex-B to NV12 in system memory,
//!   converted to RGBA on the CPU ([`rdp_graphics::yuv::nv12_to_rgba`]). The
//!   portable fallback when DXVA is unavailable, and the AVC444 sub-stream path.
//! - [`H264Encoder`] — NV12 → H.264, for compressing the redirected camera.
//!
//! Windows-runtime FFI: type-checked against the windows crate for MSVC and
//! validated on a Windows host. The MFT input/output negotiation and
//! ProcessInput/ProcessOutput drain loop mirror the documented MFT contract.

use core::ffi::c_void;
use core::mem::ManuallyDrop;

use windows::core::{Interface, Result as WinResult};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    CODECAPI_AVLowLatencyMode, MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_DECODER,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;

/// Put a freshly-activated MFT into low-latency mode so it emits each frame as
/// soon as it's decoded instead of holding a reorder/lookahead buffer — the
/// frames of latency mstsc avoids and we otherwise pay. Two independent levers
/// are set because decoders honour one or the other (or both):
///
/// - `MF_LOW_LATENCY` on the transform's attribute store (the documented
///   Media Foundation knob for the system H.264 decoder/encoder), and
/// - `CODECAPI_AVLowLatencyMode` via `ICodecAPI` (the codec-property knob).
///
/// Both are best-effort: an older MFT may expose neither, so every failure is
/// logged at debug and ignored — the transform still works, just with the
/// default buffering.
unsafe fn enable_low_latency(transform: &IMFTransform, label: &str) {
    let mut set = false;
    if let Ok(attrs) = transform.GetAttributes() {
        if attrs.SetUINT32(&MF_LOW_LATENCY, 1).is_ok() {
            set = true;
        }
    }
    if let Ok(codec) = transform.cast::<ICodecAPI>() {
        // windows 0.62 dropped the ergonomic `VARIANT` with `From<bool>`, so the
        // VT_BOOL value is assembled by hand (VARIANT_TRUE == -1). The inner
        // `ManuallyDrop` union field won't auto-deref, hence `deref_mut`.
        let mut on = windows::Win32::System::Variant::VARIANT::default();
        (*on.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_BOOL;
        (*on.Anonymous.Anonymous).Anonymous.boolVal =
            windows::Win32::Foundation::VARIANT_BOOL(-1);
        if codec.SetValue(&CODECAPI_AVLowLatencyMode, &on).is_ok() {
            set = true;
        }
    }
    if set {
        tracing::debug!(mft = label, "low-latency mode enabled");
    } else {
        tracing::debug!(mft = label, "low-latency mode unsupported; using default buffering");
    }
}

/// One decoded frame: NV12 (Y plane of `width*height`, then interleaved UV of
/// `width*height/2`), tightly packed (stride == width).
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub nv12: Vec<u8>,
}

impl DecodedFrame {
    /// The Y and UV plane slices, for [`rdp_graphics::yuv::nv12_to_rgba`].
    pub fn planes(&self) -> (&[u8], &[u8]) {
        self.nv12.split_at((self.width * self.height) as usize)
    }
}

const MF_MT_INTERLACE_PROGRESSIVE: u32 = 2;

/// Adopt an NV12 output type after an `MF_E_TRANSFORM_STREAM_CHANGE`. The stream's
/// SPS has now told the decoder its real coded size and stride, so prefer the
/// decoder's *own* enumerated NV12 type (which carries them) over a hand-built one.
/// Forcing a bare type (only major+subtype) is what made the DXVA decoder fail the
/// first `ProcessOutput` with `MF_E_ATTRIBUTENOTFOUND` (it couldn't size its output
/// allocator); forcing a non-macroblock-aligned size sheared the CPU decoder's
/// frames. Falls back to a fully-specified NV12 type only if enumeration yields none.
unsafe fn adopt_nv12_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> WinResult<()> {
    let mut i = 0u32;
    loop {
        match transform.GetOutputAvailableType(0, i) {
            Ok(t) => {
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    transform.SetOutputType(0, &t, 0)?;
                    return Ok(());
                }
                i += 1;
            }
            // MF_E_NO_MORE_TYPES (or any error) → fall back to forcing NV12.
            Err(_) => break,
        }
    }
    let frame_size = ((width as u64) << 32) | height as u64;
    let output = MFCreateMediaType()?;
    output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    output.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
    transform.SetOutputType(0, &output, 0)?;
    Ok(())
}

/// A Media Foundation H.264 → NV12 decoder.
pub struct H264Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    out_buf_size: usize,
    provides_samples: bool,
}

impl H264Decoder {
    /// Create a decoder for `width`x`height` H.264 (the size is a hint; the
    /// stream's own SPS may trigger a one-time output renegotiation).
    pub fn new(width: u32, height: u32) -> WinResult<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)?;

            let in_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT,
                Some(&in_info),
                Some(&out_info),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            // MFTEnumEx returns `count` AddRef'd IMFActivate pointers in one
            // CoTaskMemAlloc'd block. Keep a clone of the first, then Release
            // *every* enumerated reference (`ptr::read` + drop runs each one's
            // Drop) before freeing the block — otherwise all `count` leak.
            let list = std::slice::from_raw_parts(activates, count as usize);
            let first = list[0].clone();
            for i in 0..count as usize {
                drop(std::ptr::read(activates.add(i)));
            }
            CoTaskMemFree(Some(activates as *const c_void));
            let activate = first.ok_or_else(windows::core::Error::from_thread)?;
            let transform: IMFTransform = activate.ActivateObject()?;
            enable_low_latency(&transform, "cpu-decoder");

            // Input: H.264, progressive, with the hinted frame size.
            let frame_size = ((width as u64) << 32) | height as u64;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetInputType(0, &input, 0)?;

            // Output: NV12.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetOutputType(0, &output, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let out_buf_size = (info.cbSize as usize).max((width * height * 3 / 2) as usize);

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                width,
                height,
                out_buf_size,
                provides_samples,
            })
        }
    }

    /// Decode one Annex-B access unit, returning any frames it produced (a unit
    /// may yield zero — e.g. SPS/PPS only — or several).
    pub fn decode(&mut self, annex_b: &[u8]) -> WinResult<Vec<DecodedFrame>> {
        if annex_b.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(annex_b.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            if ptr.is_null() {
                buffer.Unlock()?;
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
            buffer.SetCurrentLength(annex_b.len() as u32)?;
            buffer.Unlock()?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(0)?;
            self.transform.ProcessInput(0, &sample, 0)?;

            self.drain()
        }
    }

    /// Pull all currently-available output frames from the transform.
    unsafe fn drain(&mut self) -> WinResult<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        loop {
            // Supply an output sample unless the MFT allocates its own.
            let out_sample = if self.provides_samples {
                None
            } else {
                let b = MFCreateMemoryBuffer(self.out_buf_size as u32)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&b)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(out_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut out, &mut status);
            // Reclaim the (Manually-dropped) buffer fields regardless of outcome.
            let produced = out[0].pSample.take();
            let _ = out[0].pEvents.take();

            match result {
                Ok(()) => {
                    if let Some(s) = produced {
                        if let Some(frame) = self.read_nv12(&s)? {
                            frames.push(frame);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The stream's SPS set the real output type; re-assert NV12.
                    self.reset_output_type()?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }

    /// Re-set the NV12 output type after an `MF_E_TRANSFORM_STREAM_CHANGE`. The
    /// stream's SPS has now told the decoder its real coded size, so we adopt the
    /// decoder's *own* preferred NV12 output type (which carries that coded size
    /// and stride) rather than forcing the original size hint — forcing a
    /// non-macroblock-aligned size is what produced sheared/mis-chroma'd frames.
    unsafe fn reset_output_type(&mut self) -> WinResult<()> {
        adopt_nv12_output_type(&self.transform, self.width, self.height)?;
        let info = self.transform.GetOutputStreamInfo(0)?;
        self.out_buf_size =
            (info.cbSize as usize).max((self.width * self.height * 3 / 2) as usize);
        Ok(())
    }

    /// The decoder's current output row stride (bytes per Y row) and coded height
    /// (padded up to the codec's macroblock grid). Falls back to a tight
    /// width-stride / display-height layout if the attributes are absent.
    unsafe fn output_layout(&self) -> (usize, usize) {
        let mut stride = self.width as usize;
        let mut coded_h = self.height as usize;
        if let Ok(t) = self.transform.GetOutputCurrentType(0) {
            if let Ok(s) = t.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                // DEFAULT_STRIDE is signed (negative = bottom-up); the magnitude
                // is the row pitch. Ignore values narrower than the display width.
                let s = (s as i32).unsigned_abs() as usize;
                if s >= self.width as usize {
                    stride = s;
                }
            }
            if let Ok(fs) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                let h = (fs & 0xFFFF_FFFF) as usize;
                if h >= self.height as usize {
                    coded_h = h;
                }
            }
        }
        (stride, coded_h)
    }

    /// Copy a decoded sample's NV12 bytes out, honoring the decoder's real stride
    /// and coded height, into a tightly packed display-size frame (the repack
    /// math lives in [`rdp_graphics::yuv::nv12_repack_tight`], which is unit
    /// tested cross-platform).
    unsafe fn read_nv12(&self, sample: &IMFSample) -> WinResult<Option<DecodedFrame>> {
        let (stride, coded_h) = self.output_layout();
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let frame = if ptr.is_null() {
            None
        } else {
            let src = std::slice::from_raw_parts(ptr, len as usize);
            rdp_graphics::yuv::nv12_repack_tight(
                src,
                self.width as usize,
                self.height as usize,
                stride,
                coded_h,
            )
            .map(|nv12| DecodedFrame {
                width: self.width,
                height: self.height,
                nv12,
            })
        };
        buffer.Unlock()?;
        Ok(frame)
    }
}

// Deliberately no `Drop`/`MFShutdown` on any MF wrapper in this module:
// `MFShutdown` tears the whole Media Foundation platform down when the startup
// refcount reaches zero, and Rust drops struct fields (the MFT, the DXGI device
// manager) *after* `Drop::drop` runs — so the COM releases would land on a
// dead platform. That exact ordering crashed with an access violation when a
// lone GPU-probe decoder was dropped at startup. `MFStartup` is refcounted and
// cheap to call per-constructor; the platform simply stays up until process
// exit (same pattern as `mf_camera`), which Windows reclaims cleanly.

/// A Media Foundation H.264 *encoder* (NV12 → H.264), for compressing redirected
/// camera frames before they cross the wire. Mirrors [`H264Decoder`] in reverse:
/// the MFT's output (H.264) type is configured first — with the target bitrate —
/// then the NV12 input type. Frames in, Annex-B access units out.
pub struct H264Encoder {
    transform: IMFTransform,
    out_buf_size: usize,
    provides_samples: bool,
}

impl H264Encoder {
    /// Create an encoder for `width`x`height` at `fps` targeting `bitrate` bps.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> WinResult<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
            let in_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT,
                Some(&in_info),
                Some(&out_info),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let list = std::slice::from_raw_parts(activates, count as usize);
            let first = list[0].clone();
            for i in 0..count as usize {
                drop(std::ptr::read(activates.add(i)));
            }
            CoTaskMemFree(Some(activates as *const c_void));
            let activate = first.ok_or_else(windows::core::Error::from_thread)?;
            let transform: IMFTransform = activate.ActivateObject()?;
            enable_low_latency(&transform, "encoder");

            let frame_size = ((width as u64) << 32) | height as u64;
            let frame_rate = ((fps as u64) << 32) | 1; // fps/1

            // Encoders require the OUTPUT (H.264) type set before the input.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            output.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetOutputType(0, &output, 0)?;

            // Input: NV12 at the same geometry.
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetInputType(0, &input, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples =
                info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let out_buf_size = (info.cbSize as usize).max((width * height) as usize);

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                out_buf_size,
                provides_samples,
            })
        }
    }

    /// Encode one NV12 frame, returning any H.264 bytes produced (may be empty
    /// when the encoder buffers input before emitting an access unit).
    pub fn encode(&mut self, nv12: &[u8]) -> WinResult<Vec<u8>> {
        if nv12.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            if ptr.is_null() {
                buffer.Unlock()?;
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.SetCurrentLength(nv12.len() as u32)?;
            buffer.Unlock()?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(0)?;
            self.transform.ProcessInput(0, &sample, 0)?;

            self.drain()
        }
    }

    /// Pull all currently-available encoded output and concatenate it.
    unsafe fn drain(&mut self) -> WinResult<Vec<u8>> {
        let mut encoded = Vec::new();
        loop {
            let out_sample = if self.provides_samples {
                None
            } else {
                let b = MFCreateMemoryBuffer(self.out_buf_size as u32)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&b)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(out_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut out, &mut status);
            let produced = out[0].pSample.take();
            let _ = out[0].pEvents.take();

            match result {
                Ok(()) => {
                    if let Some(s) = produced {
                        let buffer = s.ConvertToContiguousBuffer()?;
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut len = 0u32;
                        buffer.Lock(&mut ptr, None, Some(&mut len))?;
                        if !ptr.is_null() && len > 0 {
                            encoded.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                        }
                        buffer.Unlock()?;
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Re-assert the output type after a stream change, then retry.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(encoded)
    }
}

/// One GPU-decoded frame that never left the GPU: an NV12 D3D11 texture (slice
/// 0) plus its dimensions. Produced by [`H264GpuDecoder`] and handed straight to
/// the renderer's video processor — no system-memory copy, no CPU conversion.
pub struct DecodedTexture {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

/// A DXVA (GPU) H.264 decoder: the system decoder MFT bound to the caller's
/// Direct3D 11 device via an `IMFDXGIDeviceManager`, so decoding runs on the GPU
/// and outputs NV12 textures. Each decoded frame is copied out of the decoder's
/// internal pool into a small ring of standalone textures (so the renderer can
/// read a frame while the decoder keeps producing). The caller falls back to the
/// CPU [`H264Decoder`] if [`H264GpuDecoder::new`] fails (older GPU / no DXVA).
pub struct H264GpuDecoder {
    transform: IMFTransform,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Kept alive for the decoder's lifetime (it references the D3D device).
    _manager: IMFDXGIDeviceManager,
    width: u32,
    height: u32,
}

impl H264GpuDecoder {
    /// Create a GPU decoder bound to `device`/`context`. Returns `Err` if DXVA
    /// can't be set up, so the caller can fall back to CPU decode.
    pub fn new(
        width: u32,
        height: u32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> WinResult<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)?;

            // Bind a DXGI device manager to our D3D11 device.
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(windows::core::Error::from_thread)?;
            manager.ResetDevice(device, token)?;

            // The system H.264 decoder MFT (H.264 → NV12).
            let in_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT,
                Some(&in_info),
                Some(&out_info),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let list = std::slice::from_raw_parts(activates, count as usize);
            let first = list[0].clone();
            for i in 0..count as usize {
                drop(std::ptr::read(activates.add(i)));
            }
            CoTaskMemFree(Some(activates as *const c_void));
            let transform: IMFTransform = first
                .ok_or_else(windows::core::Error::from_thread)?
                .ActivateObject()?;
            enable_low_latency(&transform, "gpu-decoder");

            // Hand the decoder our D3D manager → DXVA, D3D-backed output samples.
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let frame_size = ((width as u64) << 32) | height as u64;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetInputType(0, &input, 0)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MF_MT_INTERLACE_PROGRESSIVE)?;
            transform.SetOutputType(0, &output, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                device: device.clone(),
                context: context.clone(),
                _manager: manager,
                width,
                height,
            })
        }
    }

    /// Allocate a standalone NV12 texture to copy a decoded frame into. Sized to the
    /// decoder pool's slice dimensions (and never below the display size rounded up to
    /// the 16-pixel macroblock grid), so the full-subresource copy from the decoder
    /// pool is exact and never clips the bottom/right padding rows; the video processor
    /// later samples only the top-left display region, so the padding is unused.
    ///
    /// A *fresh* texture per frame is deliberate: the frame's texture is handed
    /// across the channel to the renderer thread, where its COM refcount keeps
    /// the pixels alive until present finishes and the renderer drops it. Reusing
    /// a fixed ring instead let the decoder overwrite a frame still queued for
    /// present, which is what corrupted fast motion. D3D11 recycles the freed
    /// allocations internally, so per-frame creation is cheap.
    unsafe fn acquire_texture(&mut self, coded_w: u32, coded_h: u32) -> WinResult<ID3D11Texture2D> {
        // Never smaller than the display's macroblock-aligned size.
        let coded_w = coded_w.max((self.width + 15) & !15);
        let coded_h = coded_h.max((self.height + 15) & !15);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: coded_w,
            Height: coded_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
        t.ok_or_else(windows::core::Error::from_thread)
    }

    /// Decode one Annex-B access unit, returning GPU NV12 textures (zero, one, or
    /// several frames).
    pub fn decode(&mut self, annex_b: &[u8]) -> WinResult<Vec<DecodedTexture>> {
        if annex_b.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(annex_b.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            if ptr.is_null() {
                buffer.Unlock()?;
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
            buffer.SetCurrentLength(annex_b.len() as u32)?;
            buffer.Unlock()?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(0)?;
            self.transform.ProcessInput(0, &sample, 0)?;
            self.drain()
        }
    }

    unsafe fn drain(&mut self) -> WinResult<Vec<DecodedTexture>> {
        let mut frames = Vec::new();
        loop {
            // D3D manager set → the MFT allocates its own (texture-backed) samples.
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut out, &mut status);
            let produced = out[0].pSample.take();
            let _ = out[0].pEvents.take();
            match result {
                Ok(()) => {
                    if let Some(s) = produced {
                        if let Some(tex) = self.copy_out(&s)? {
                            frames.push(tex);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Adopt the decoder's own NV12 output type, which now carries the
                    // real coded size/stride from the parsed SPS. The previous bare
                    // type (major+subtype only) left the DXVA output allocator unsized,
                    // so the next ProcessOutput failed with MF_E_ATTRIBUTENOTFOUND
                    // (0xC00D36E6) → silent CPU fallback. Mirrors the CPU decoder.
                    adopt_nv12_output_type(&self.transform, self.width, self.height)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }

    /// Copy the decoded D3D texture (a slice of the decoder's pool) into the next
    /// ring texture and return it.
    unsafe fn copy_out(&mut self, sample: &IMFSample) -> WinResult<Option<DecodedTexture>> {
        let buffer = sample.GetBufferByIndex(0)?;
        let dxgi: IMFDXGIBuffer = buffer.cast()?;
        let mut resource: *mut core::ffi::c_void = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut resource)?;
        if resource.is_null() {
            return Ok(None);
        }
        let pool_tex = ID3D11Texture2D::from_raw(resource); // takes ownership of the ref
        let subresource = dxgi.GetSubresourceIndex()?;

        // Match the destination to the decoder pool's actual slice size so the
        // full-subresource copy never overruns: hardware decoders pad NV12 slices
        // to their own alignment, which can exceed the display/macroblock size.
        let mut pool_desc: D3D11_TEXTURE2D_DESC = core::mem::zeroed();
        pool_tex.GetDesc(&mut pool_desc);
        let dest = self.acquire_texture(pool_desc.Width, pool_desc.Height)?;
        // Copy just this frame's array slice out of the shared decoder pool.
        self.context
            .CopySubresourceRegion(&dest, 0, 0, 0, 0, &pool_tex, subresource, None);
        Ok(Some(DecodedTexture {
            texture: dest,
            width: self.width,
            height: self.height,
        }))
    }
}
