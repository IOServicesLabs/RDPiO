//! Non-Windows stub. The real renderer is Direct3D 11 and only built on Windows;
//! this exists so `cargo check`/`test` work on any host for the rest of the tree.

#[derive(Debug, thiserror::Error)]
#[error("rdp-gpu renderer is only implemented on Windows (Direct3D 11)")]
pub struct GpuError;

/// Mirrors the Windows [`crate::Backend`] selector so the shared arg-parsing code
/// compiles on non-Windows hosts (the stub renderer ignores the choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    D3D11,
    D3D12,
}

pub struct Renderer;

impl Renderer {
    pub fn new(_hwnd_raw: isize, _width: u32, _height: u32) -> Result<Self, GpuError> {
        Err(GpuError)
    }

    pub fn resize(&mut self, _width: u32, _height: u32) -> Result<(), GpuError> {
        Err(GpuError)
    }

    pub fn present_clear(&mut self, _rgba: [f32; 4]) -> Result<(), GpuError> {
        Err(GpuError)
    }

    pub fn set_upscaler(&mut self, _mode: crate::Upscaler) {}
}
