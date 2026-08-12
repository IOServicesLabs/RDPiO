//! Windows implementation of the client's top-level window: a Win32 window
//! (the repository's `crate::window::Window`) with the Direct3D 11 renderer
//! from `rdp-gpu` presenting to it. This file is gated `#[cfg(windows)]` by the
//! parent `ui` module; on Linux only the platform-neutral pieces in
//! `crate::ui` compile, so the client stays headless there.

use rdp_gpu::frame::{DecodedFrame, PresentOp};
use rdp_gpu::Renderer;

use crate::ui::{UiEvent, WindowSize};
use crate::window::{Frame, Window};

/// The client's native top-level window: a Win32 window with a Direct3D 11
/// swapchain that decoded frames are presented on.
///
/// All D3D11 objects are owned by the thread that created them — the UI thread
/// — so [`UiWindow`] must be created, pumped, and presented from one thread
/// (see [`UiWindow::handle_events`]). The session worker hands frames to this
/// window through a channel; presentation never blocks the decoder.
pub struct UiWindow {
    window: Window,
    renderer: Renderer,
    size: WindowSize,
}

impl UiWindow {
    /// Create the native top-level window and its D3D11 swapchain.
    pub fn new(title: &str, width: u32, height: u32) -> windows::core::Result<Self> {
        let window = Window::new(title, width, height)?;
        let renderer = Renderer::new(
            window.hwnd_raw(),
            width,
            height,
            rdp_gpu::Backend::default(),
        )?;
        Ok(Self {
            window,
            renderer,
            size: WindowSize::new(width, height),
        })
    }

    /// Present the current framebuffer on the window's swap chain.
    pub fn present_frame(&mut self) -> windows::core::Result<()> {
        self.renderer.present_frame()
    }

    /// Present one decoded frame on the window's swap chain — the decode→UI
    /// handoff sink. The frame arrives over the `rdp_gpu::frame` channel from
    /// the decode thread (which never blocks on this call); here it is
    /// color-converted into the swapchain framebuffer and presented:
    ///
    /// - a GPU frame ([`PresentOp::Gpu`]) is blitted zero-copy — the D3D11 NV12
    ///   texture goes straight to the renderer's video processor, no CPU copy,
    ///   with a CPU read-back fallback if the processor refuses the surface;
    /// - a CPU frame ([`PresentOp::Nv12`]) is uploaded and converted on the
    ///   GPU, again with a CPU fallback so a frame is never dropped.
    pub fn present_decoded(&mut self, frame: &DecodedFrame) -> windows::core::Result<()> {
        match rdp_gpu::frame::present_op(frame) {
            PresentOp::Gpu {
                texture,
                width,
                height,
            } => {
                // Zero-copy: the whole frame is dirty (a decoder frame is a
                // complete picture), so blit it with no region rects.
                if !self
                    .renderer
                    .blit_texture(0, 0, width, height, texture, &[])
                {
                    // The video processor refused the surface — read it back
                    // and convert on the CPU rather than dropping the frame.
                    if let Some(nv12) = self.renderer.read_nv12(texture, width, height) {
                        self.renderer.blit_nv12(0, 0, width, height, &nv12, &[]);
                    }
                }
            }
            PresentOp::Nv12 {
                nv12,
                width,
                height,
            } => {
                if !self.renderer.blit_nv12(0, 0, width, height, nv12, &[]) {
                    // No GPU conversion path (or it failed): CPU NV12→RGBA.
                    let (yp, uv) = nv12.split_at((width as usize) * (height as usize));
                    if let Some(rgba) = rdp_graphics::yuv::nv12_to_rgba(
                        yp,
                        uv,
                        width as usize,
                        height as usize,
                        width as usize,
                    ) {
                        let (w, h) = (
                            width.min(u16::MAX as u32) as u16,
                            height.min(u16::MAX as u32) as u16,
                        );
                        self.renderer.update_rect(0, 0, w, h, &rgba);
                    }
                }
            }
        }
        self.renderer.present_frame()
    }

    /// Clear the swapchain backbuffer to an RGBA colour and present — used for
    /// the idle slate before the first decoded frame arrives.
    pub fn present_clear(&mut self, rgba: [f32; 4]) -> windows::core::Result<()> {
        self.renderer.present_clear(rgba)
    }

    /// Pump the window's message queue once, forwarding any pending resize to
    /// the swapchain, and report whether the run loop should keep going.
    ///
    /// Resize events from Win32 (`WM_SIZE`) are applied to the D3D11 swapchain
    /// here, so the caller only needs to re-present after a
    /// [`UiEvent::Continue`].
    pub fn handle_events(&mut self) -> windows::core::Result<UiEvent> {
        match self.window.pump() {
            Frame::Quit => Ok(UiEvent::Quit),
            Frame::Continue { resize } => {
                if let Some((w, h)) = resize {
                    if self.size.set(w, h) {
                        self.renderer.resize(w, h)?;
                    }
                }
                Ok(UiEvent::Continue { resize })
            }
        }
    }

    /// Update the window title (e.g. to reflect connection state).
    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    /// Resize the native window and its swapchain to `width`×`height`.
    pub fn set_size(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        let (w, h) = (width.max(1), height.max(1));
        self.window.set_size(w, h)?;
        self.renderer.resize(w, h)?;
        self.size = WindowSize::new(w, h);
        Ok(())
    }

    /// Ask the window to close; the next [`UiWindow::handle_events`] reports
    /// [`UiEvent::Quit`] and the run loop exits.
    pub fn close(&mut self) {
        self.window.request_close();
    }

    /// The raw `HWND` value, for handing to other Win32/D3D11 code.
    pub fn hwnd_raw(&self) -> isize {
        self.window.hwnd_raw()
    }

    /// The current client-area size.
    pub fn size(&self) -> (u32, u32) {
        self.size.get()
    }
}
