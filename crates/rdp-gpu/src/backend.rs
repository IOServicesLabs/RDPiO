//! Backend-agnostic renderer dispatcher.
//!
//! The [`Renderer`] enum wraps either the Direct3D 11 implementation
//! ([`D3D11Renderer`](crate::d3d11::D3D11Renderer)) or the Direct3D 12
//! implementation ([`D3D12Renderer`](crate::d3d12::D3D12Renderer)) and exposes
//! a single uniform API. Callers select the backend at construction time; the
//! default remains D3D11 for compatibility.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};

use crate::Upscaler;
use crate::d3d11::D3D11Renderer;
use crate::d3d12::D3D12Renderer;

/// Which GPU backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Direct3D 11 — the original implementation, kept as the safe default.
    #[default]
    D3D11,
    /// Direct3D 12 — lower present overhead, compute-shader color conversion,
    /// and VRR-capable present path.
    D3D12,
}

/// A GPU renderer independent of the underlying Windows graphics API.
pub enum Renderer {
    D3D11(D3D11Renderer),
    D3D12(D3D12Renderer),
}

impl Renderer {
    /// Create a renderer for `hwnd` with the requested backend.
    pub fn new(hwnd_raw: isize, width: u32, height: u32, backend: Backend) -> windows::core::Result<Self> {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        match backend {
            Backend::D3D11 => Ok(Self::D3D11(D3D11Renderer::new(hwnd_raw, width, height)?)),
            Backend::D3D12 => Ok(Self::D3D12(D3D12Renderer::new(hwnd, width, height)?)),
        }
    }

    /// Enable (or disable) the no-vsync tearing present path.
    pub fn set_low_latency(&mut self, on: bool) {
        match self {
            Self::D3D11(r) => r.set_low_latency(on),
            Self::D3D12(r) => r.set_low_latency(on),
        }
    }

    /// Choose the GPU upscaler used when the remote desktop is rendered smaller
    /// than the window.
    pub fn set_upscaler(&mut self, mode: Upscaler) {
        match self {
            Self::D3D11(r) => r.set_upscaler(mode),
            Self::D3D12(r) => r.set_upscaler(mode),
        }
    }

    /// Set the RCAS adaptive-sharpen strength (`0.0` = off, `1.0` = maximum)
    /// applied after the upscale (or at 1:1 when nothing is scaled).
    pub fn set_sharpen(&mut self, strength: f32) {
        match self {
            Self::D3D11(r) => r.set_sharpen(strength),
            Self::D3D12(r) => r.set_sharpen(strength),
        }
    }

    /// Resize the swapchain backbuffers to match the client area.
    pub fn resize(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        match self {
            Self::D3D11(r) => r.resize(width, height),
            Self::D3D12(r) => r.resize(width, height),
        }
    }

    /// Clear the backbuffer to an RGBA color and present.
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> windows::core::Result<()> {
        match self {
            Self::D3D11(r) => r.present_clear(rgba),
            Self::D3D12(r) => r.present_clear(rgba),
        }
    }

    /// Allocate (or reallocate) the desktop framebuffer to `width`x`height`.
    pub fn ensure_framebuffer(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        match self {
            Self::D3D11(r) => r.ensure_framebuffer(width, height),
            Self::D3D12(r) => r.ensure_framebuffer(width, height),
        }
    }

    /// Upload one decoded RGBA rectangle into the framebuffer at (`x`,`y`).
    pub fn update_rect(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        match self {
            Self::D3D11(r) => r.update_rect(x, y, w, h, rgba),
            Self::D3D12(r) => r.update_rect(x, y, w, h, rgba),
        }
    }

    /// Copy a `w`x`h` framebuffer rectangle from (`sx`,`sy`) to (`dx`,`dy`).
    pub fn copy_rect(&mut self, sx: u16, sy: u16, w: u16, h: u16, dx: u16, dy: u16) {
        match self {
            Self::D3D11(r) => r.copy_rect(sx, sy, w, h, dx, dy),
            Self::D3D12(r) => r.copy_rect(sx, sy, w, h, dx, dy),
        }
    }

    /// Stash a `w`x`h` framebuffer rectangle into GPU cache `slot`.
    pub fn cache_rect(&mut self, slot: u16, sx: u16, sy: u16, w: u16, h: u16) {
        match self {
            Self::D3D11(r) => r.cache_rect(slot, sx, sy, w, h),
            Self::D3D12(r) => r.cache_rect(slot, sx, sy, w, h),
        }
    }

    /// Blit GPU cache `slot` onto the framebuffer at (`dx`,`dy`).
    pub fn cache_blit(&mut self, slot: u16, dx: u16, dy: u16) {
        match self {
            Self::D3D11(r) => r.cache_blit(slot, dx, dy),
            Self::D3D12(r) => r.cache_blit(slot, dx, dy),
        }
    }

    /// Disable the GPU NV12 path (forces CPU YUV conversion).
    pub fn disable_gpu_yuv(&mut self) {
        match self {
            Self::D3D11(r) => r.disable_gpu_yuv(),
            Self::D3D12(r) => r.disable_gpu_yuv(),
        }
    }

    /// Whether the GPU NV12 color-conversion path is currently usable.
    pub fn gpu_yuv_available(&self) -> bool {
        match self {
            Self::D3D11(r) => r.gpu_yuv_available(),
            Self::D3D12(r) => r.gpu_yuv_available(),
        }
    }

    /// Convert an NV12 frame to RGB on the GPU and write its dirty `regions`
    /// (frame-relative; empty = whole frame) into the framebuffer.
    pub fn blit_nv12(
        &mut self,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        nv12: &[u8],
        regions: &[(u32, u32, u32, u32)],
    ) -> bool {
        match self {
            Self::D3D11(r) => r.blit_nv12(dest_x, dest_y, w, h, nv12, regions),
            Self::D3D12(r) => r.blit_nv12(dest_x, dest_y, w, h, nv12, regions),
        }
    }

    /// Color-convert a GPU NV12 texture's dirty `regions` into the framebuffer
    /// at `(dest_x, dest_y)`. The D3D12 backend does not implement zero-copy
    /// DXVA in this version, so it always returns `false` here; the caller
    /// falls back to CPU decode + [`Self::blit_nv12`].
    #[allow(clippy::too_many_arguments)]
    pub fn blit_texture(
        &mut self,
        dest_x: u32,
        dest_y: u32,
        w: u32,
        h: u32,
        tex: &ID3D11Texture2D,
        regions: &[(u32, u32, u32, u32)],
    ) -> bool {
        match self {
            Self::D3D11(r) => r.blit_texture(dest_x, dest_y, w, h, tex, regions),
            Self::D3D12(_r) => false,
        }
    }

    /// Read an NV12 GPU texture back to the CPU as packed NV12, for when
    /// [`Self::blit_texture`] reports the video processor would not take it.
    /// `None` on the D3D12 backend, which has no zero-copy DXVA path to fall
    /// back from (it never produces one of these textures in the first place).
    pub fn read_nv12(&self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Option<Vec<u8>> {
        match self {
            Self::D3D11(r) => r.read_nv12(tex, w, h),
            Self::D3D12(_r) => None,
        }
    }

    /// Present the framebuffer.
    pub fn present_frame(&mut self) -> windows::core::Result<()> {
        match self {
            Self::D3D11(r) => r.present_frame(),
            Self::D3D12(r) => r.present_frame(),
        }
    }

    /// Read the live framebuffer back to CPU as tightly-packed RGBA.
    pub fn readback_framebuffer(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        match self {
            Self::D3D11(r) => r.readback_framebuffer(),
            Self::D3D12(r) => r.readback_framebuffer(),
        }
    }

    /// Clone the D3D11 device + immediate context for DXVA decode on the worker
    /// thread. Returns `None` for the D3D12 backend in this version.
    pub fn device_context_clone(&self) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        match self {
            Self::D3D11(r) => Some(r.device_context_clone()),
            Self::D3D12(_r) => None,
        }
    }

    /// Add a per-monitor present target that presents a slice of the framebuffer.
    /// `src_w`/`src_h` give the slice size (0 = window-sized, 1:1); a smaller
    /// slice — render-scale under per-monitor — is upscaled on present.
    #[allow(clippy::too_many_arguments)]
    pub fn add_present_target(
        &mut self,
        hwnd_raw: isize,
        width: u32,
        height: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
    ) -> windows::core::Result<()> {
        match self {
            Self::D3D11(r) => {
                r.add_present_target(hwnd_raw, width, height, src_x, src_y, src_w, src_h)
            }
            Self::D3D12(r) => {
                r.add_present_target(hwnd_raw, width, height, src_x, src_y, src_w, src_h)
            }
        }
    }

    /// Set the framebuffer slice the primary swapchain presents from (offset +
    /// size; a `src_w`/`src_h` of 0 means swapchain-sized, 1:1).
    pub fn set_primary_src(&mut self, x: u32, y: u32, src_w: u32, src_h: u32) {
        match self {
            Self::D3D11(r) => r.set_primary_src(x, y, src_w, src_h),
            Self::D3D12(r) => r.set_primary_src(x, y, src_w, src_h),
        }
    }

    /// Install a callback that receives `(label, microseconds)` for completed
    /// GPU timing queries.
    pub fn set_gpu_timing_callback(
        &mut self,
        cb: Option<Box<dyn Fn(&str, u64) + Send + Sync>>,
    ) {
        match self {
            Self::D3D11(r) => r.set_gpu_timing_callback(cb),
            Self::D3D12(r) => r.set_gpu_timing_callback(cb),
        }
    }
}
