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

/// Which GPU renderers decode and present on: the fastest, or the one the
/// display is attached to (on battery).
#[cfg(windows)]
pub mod gpu {
    pub use crate::d3d11::{chosen_adapter_luid, device_adapter_luid, prefer_display_adapter, prefers_display_adapter};
}

#[cfg(windows)]
pub mod h264;
#[cfg(windows)]
pub mod hyper;

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

/// Where a `src`-sized picture goes in a `dst`-sized window when its aspect
/// ratio is kept: the largest such rectangle, centred, as `(x, y, w, h)`.
/// Within 2 px of the whole window it is the whole window (no hairline bars
/// from rounding).
pub fn fit_rect(src: (u32, u32), dst: (u32, u32)) -> (u32, u32, u32, u32) {
    let ((sw, sh), (dw, dh)) = (src, dst);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return (0, 0, dw, dh);
    }
    let scale = (dw as f64 / sw as f64).min(dh as f64 / sh as f64);
    let w = ((sw as f64 * scale).round() as u32).clamp(1, dw);
    let h = ((sh as f64 * scale).round() as u32).clamp(1, dh);
    if dw - w <= 2 && dh - h <= 2 {
        return (0, 0, dw, dh);
    }
    ((dw - w) / 2, (dh - h) / 2, w, h)
}

#[cfg(test)]
mod fit_rect_tests {
    use super::fit_rect;

    #[test]
    fn a_picture_keeps_its_shape_in_any_window() {
        // Same shape: the whole window, scaled or not.
        assert_eq!(fit_rect((2560, 1440), (1920, 1080)), (0, 0, 1920, 1080));
        assert_eq!(fit_rect((1920, 1080), (1920, 1080)), (0, 0, 1920, 1080));
        // 16:9 on a 3:2 laptop: full width, bars above and below — 1:1 when
        // the host already scaled it to that width.
        assert_eq!(fit_rect((2256, 1268), (2256, 1504)), (0, 118, 2256, 1268));
        assert_eq!(fit_rect((1920, 1080), (2256, 1504)), (0, 117, 2256, 1269));
        // A portrait picture on a landscape screen: bars left and right.
        assert_eq!(fit_rect((1080, 1920), (1920, 1080)), (656, 0, 608, 1080));
        // Rounding doesn't leave a hairline.
        assert_eq!(fit_rect((1366, 768), (1920, 1080)), (0, 0, 1920, 1080));
        // Degenerate sizes: the whole window.
        assert_eq!(fit_rect((0, 0), (800, 600)), (0, 0, 800, 600));
        assert_eq!(fit_rect((800, 600), (0, 0)), (0, 0, 0, 0));
    }
}
