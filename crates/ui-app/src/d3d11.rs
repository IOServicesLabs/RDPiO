//! D3D11 hardware H.264 video decoder (Windows only).
//!
//! Built on `ID3D11VideoDevice` / `ID3D11VideoDecoder` / `ID3D11VideoContext`
//! (windows crate, `Win32_Graphics_Direct3D11`). Compressed access units are
//! submitted as `COMPRESSED_BITSTREAM` buffers; decoded frames land in an
//! output view backed by a `D3D11_BIND_DECODER` NV12 texture.

use super::{to_avcc, AccessUnit, H264Config};
use windows::core::{Error, Interface, Result};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_9_3,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDecoder, ID3D11VideoDecoderOutputView, ID3D11VideoDevice, D3D11_BIND_DECODER,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_DECODER_BUFFER_DESC, D3D11_VIDEO_DECODER_BUFFER_TYPE_COMPRESSED_BITSTREAM,
    D3D11_VIDEO_DECODER_DESC, D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_DECODER_OUTPUT_VIEW_DIMENSION_TEXTURE2D, D3D11_VIDEO_DECODER_OUTPUT_VIEW_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::DXGI_SAMPLE_DESC;

/// The `D3D11_DECODER_PROFILE_H264` GUID
/// (`1b81be64-a0c7-11d3-b984-00c04f2e73c5`).
const H264_DECODER_GUID: windows::core::GUID =
    windows::core::GUID::from_u128(0x1b81be64_a0c7_11d3_b984_00c04f2e73c5);

/// A decoded frame: the output texture plus its resolution.
#[derive(Clone)]
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

/// The D3D11 H.264 decoder for one stream configuration.
pub struct VideoDecoder {
    device: ID3D11Device,
    decoder: ID3D11VideoDecoder,
    output_view: ID3D11VideoDecoderOutputView,
    output_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    /// Create a hardware decoder for `config`, with an NV12 output texture
    /// sized from the SPS-derived resolution.
    pub fn new(config: H264Config) -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_9_3];
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        let video_device: ID3D11VideoDevice = device.cast()?;

        let width = config.width as u32;
        let height = config.height as u32;
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: H264_DECODER_GUID,
            SampleWidth: width,
            SampleHeight: height,
            OutputFormat: DXGI_FORMAT_NV12,
        };
        // NULL config lets the driver pick its default decoder configuration.
        let decoder = unsafe { video_device.CreateVideoDecoder(&desc, std::ptr::null())? };

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DECODER,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let output_texture = unsafe { device.CreateTexture2D(&texture_desc, None)? };
        let view_desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
            DecodeProfile: H264_DECODER_GUID,
            Dimension: D3D11_VIDEO_DECODER_OUTPUT_VIEW_DIMENSION_TEXTURE2D,
            Texture2D: D3D11_VIDEO_DECODER_OUTPUT_VIEW_TEXTURE2D { ArraySlice: 0 },
        };
        let output_view =
            unsafe { video_device.CreateVideoDecoderOutputView(&output_texture, &view_desc)? };

        Ok(Self {
            device,
            decoder,
            output_view,
            output_texture,
            width,
            height,
        })
    }

    /// Decode one access unit and return the output texture holding the frame.
    pub fn decode(&mut self, unit: &AccessUnit) -> Result<DecodedFrame> {
        let bitstream = to_avcc(unit);
        let context: ID3D11VideoContext = self.device.cast()?;

        let mut size = 0u32;
        let mut buffer: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe {
            context.GetDecoderBuffer(
                &self.decoder,
                D3D11_VIDEO_DECODER_BUFFER_TYPE_COMPRESSED_BITSTREAM,
                &mut size,
                &mut buffer,
            )?;
            if (size as usize) < bitstream.len() {
                return Err(Error::from_hresult(E_NOTIMPL));
            }
            std::ptr::copy_nonoverlapping(bitstream.as_ptr(), buffer as *mut u8, bitstream.len());

            let buffer_desc = D3D11_VIDEO_DECODER_BUFFER_DESC {
                BufferType: D3D11_VIDEO_DECODER_BUFFER_TYPE_COMPRESSED_BITSTREAM,
                BufferIndex: 0,
                DataOffset: 0,
                DataSize: bitstream.len() as u32,
                FirstMBAddress: 0,
                NumMBsInBuffer: 0,
                Width: self.width,
                Height: self.height,
                Stride: 0,
                ReservedBits: 0,
                pIV: std::ptr::null_mut(),
                IVSize: 0,
                PartialEncryption: false,
                EncryptedBlockInfoSize: 0,
                pEncryptedBlockInfo: std::ptr::null_mut(),
            };
            context.SubmitDecoderBuffers(&self.decoder, &[buffer_desc])?;
            context.ReleaseDecoderBuffer(
                &self.decoder,
                D3D11_VIDEO_DECODER_BUFFER_TYPE_COMPRESSED_BITSTREAM,
            )?;
            context.DecoderBeginFrame(&self.decoder, &self.output_view, 0, None)?;
            context.DecoderEndFrame(&self.decoder)?;
        }

        Ok(DecodedFrame {
            texture: self.output_texture.clone(),
            width: self.width,
            height: self.height,
        })
    }

    /// The decoded frame resolution.
    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}
