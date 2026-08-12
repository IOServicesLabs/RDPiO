//! D3D11 render path (Windows only): device, swapchain, and the present loop.
//!
//! The renderer owns the D3D11 device, the DXGI flip-model swapchain for the
//! main window, and the render-target view. [`Renderer::present`] uploads a
//! decoded BGRA frame into a texture, blits it onto the back buffer, and
//! presents.

#![cfg(windows)]

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_9_3,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SDK_VERSION,
    D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// Errors from renderer setup or present.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("d3d11 error: {0}")]
    D3D11(#[from] windows::core::Error),
}

/// A decoded BGRA8 frame handed to the renderer (bottom-up, like the swapchain
/// format).
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The D3D11 device + swapchain for the main window.
pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    rtv: ID3D11RenderTargetView,
    frame_texture: Option<ID3D11Texture2D>,
    frame_size: Option<(u32, u32)>,
    width: u32,
    height: u32,
}

fn rtv_desc() -> D3D11_RENDER_TARGET_VIEW_DESC {
    D3D11_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    }
}

impl Renderer {
    /// Create the device and a flip-model swapchain for `hwnd`.
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
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
        let device = device.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
        let context = context.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: 0,
        };
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1()? };
        let swapchain =
            unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)? };

        let backbuffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0)? };
        let rtv = unsafe { device.CreateRenderTargetView(&backbuffer, Some(&rtv_desc()))? };

        Ok(Self {
            device,
            context,
            swapchain,
            rtv,
            frame_texture: None,
            frame_size: None,
            width,
            height,
        })
    }

    /// Present one decoded frame (or a cleared frame when `None`).
    pub fn present(&mut self, frame: Option<&RgbaFrame>) -> Result<()> {
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            self.context
                .ClearRenderTargetView(&self.rtv, &[0.0, 0.0, 0.0, 1.0]);
        }
        if let Some(frame) = frame {
            self.upload_frame(frame)?;
        }
        unsafe {
            let _ = self.swapchain.Present1(1, 0, std::ptr::null());
        }
        Ok(())
    }

    /// Resize the swapchain and recreate the render target (WM_SIZE path).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.width = width.max(1);
        self.height = height.max(1);
        unsafe {
            self.swapchain.ResizeBuffers(
                0,
                self.width,
                self.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                0,
            )?;
        }
        let backbuffer: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
        self.rtv = unsafe {
            self.device
                .CreateRenderTargetView(&backbuffer, Some(&rtv_desc()))?
        };
        self.frame_texture = None;
        self.frame_size = None;
        Ok(())
    }

    /// The current back buffer size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The underlying device (the video decoder shares it).
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Upload a frame into a matching texture, recreating the texture when the
    /// frame size changes.
    fn upload_frame(&mut self, frame: &RgbaFrame) -> Result<()> {
        if self.frame_size != Some((frame.width, frame.height)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: frame.width,
                Height: frame.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let texture = unsafe { self.device.CreateTexture2D(&desc, None)? };
            self.frame_texture = Some(texture);
            self.frame_size = Some((frame.width, frame.height));
        }
        if let Some(texture) = &self.frame_texture {
            unsafe {
                self.context.UpdateSubresource(
                    texture,
                    0,
                    None,
                    frame.pixels.as_ptr() as *const _,
                    frame.width * 4,
                    0,
                );
            }
        }
        Ok(())
    }
}

impl crate::decode::sink::FrameSink for Renderer {
    type Error = RenderError;

    fn present(&mut self, frame: crate::decode::DecodedFrame) -> Result<(), Self::Error> {
        let rgba = RgbaFrame {
            pixels: match frame.payload {
                crate::decode::FramePayload::Bgra8(pixels) => pixels,
            },
            width: frame.width,
            height: frame.height,
        };
        self.present(Some(&rgba))?;
        Ok(())
    }

    fn size(&self) -> (u32, u32) {
        self.size()
    }
}
