//! GPU backend: Direct3D 11/12 device + swapchain, hardware H.264 decode, and
//! surface compositing/present.
//!
//! The [`Renderer`] enum wraps either the Direct3D 11 or Direct3D 12 backend and
//! exposes a single uniform API. D3D11 is the default; D3D12 is selectable via
//! [`Backend::D3D12`] and adds a compute-shader NV12→RGB conversion path plus a
//! lower-overhead present queue with tearing/VRR support. [`h264`] provides DXVA
//! GPU decode (zero-copy) for the D3D11 backend, a software decode fallback, and
//! an H.264 encoder.
//!
//! The renderer is Windows-only. A stub keeps the workspace loading on other
//! hosts so the sans-I/O crates stay testable everywhere.

#[cfg(windows)]
mod d3d11;
#[cfg(windows)]
mod d3d12;
#[cfg(windows)]
mod backend;
#[cfg(windows)]
pub use backend::{Backend, Renderer};

#[cfg(windows)]
pub mod h264;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{Backend, Renderer};

/// Which GPU upscaler the present path uses when the remote desktop is rendered
/// smaller than the window (`--render-scale`) and must be scaled up to fill it.
/// Selected once at startup via [`Renderer::set_upscaler`]; the merged desktop
/// framebuffer (text + UI + video, already composited) is scaled as one image,
/// so the choice is a whole-frame trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Upscaler {
    /// Driver AI video super resolution via the D3D11 video processor — NVIDIA
    /// RTX Video Super Resolution on RTX GPUs, Intel VPE Super Resolution on
    /// Intel. Built for full-screen video/gaming — its design target — but
    /// rings/crunches on text and sharp UI edges, so it is opt-in rather than
    /// the default. Unavailable on AMD (use [`Upscaler::Fsr`] there).
    Vsr,
    /// Catmull-Rom bicubic via our own shader. Sharp without hallucinating on
    /// text; the default for mixed desktop + game content. GPU-agnostic.
    #[default]
    Bicubic,
    /// AMD FidelityFX Super Resolution 1.0 (EASU edge-adaptive upscale, with an
    /// RCAS sharpen pass unless sharpening is explicitly disabled). The
    /// vendor-neutral game-content upscaler — runs as a shader on any D3D11-class
    /// GPU (AMD, Intel, NVIDIA), reconstructing edges noticeably better than
    /// bicubic on game imagery.
    Fsr,
    /// Nearest-neighbour point sampling. Pixel-perfect ("lossless") at exact
    /// integer ratios (e.g. `--render-scale 0.5` on a 2× window); blocky at
    /// fractional ratios.
    Nearest,
    /// The video processor's plain bilinear scale. Soft but completely artifact-
    /// free; also the automatic fallback when the shader upscalers can't be built.
    Bilinear,
}
