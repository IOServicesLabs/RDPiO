//! Native window abstraction: open, pump the event loop, close, and expose a
//! raw window handle for the Direct3D 11 decoder/present path.
//!
//! [`AppWindow`] is the contract every front-end implements:
//!
//! * [`AppWindow::open`] creates the window (or a headless stand-in);
//! * [`AppWindow::pump`] runs one iteration of the platform event loop and
//!   reports the close/resize decision;
//! * [`AppWindow::drain_events`] hands the accumulated platform events back as
//!   [`WindowEvent`]s for the shared state/input mapping;
//! * [`AppWindow::native_handle`] / [`AppWindow::raw_window_handle`] expose the
//!   native surface the D3D11 swapchain is created against.
//!
//! [`HeadlessWindow`] is the non-Windows implementation: it never opens a
//! display, which keeps CI and unit tests display-free, and reports
//! [`NativeWindowHandle::Headless`]. The Windows front-end hands back
//! [`NativeWindowHandle::Os`] wrapping a `RawWindowHandle::Win32` built from
//! the real `HWND` via the pure [`win32_handle`] helper.

use core::num::NonZeroIsize;

use raw_window_handle::{RawWindowHandle, Win32WindowHandle};

use crate::cli::CliOptions;
use crate::ui::event::WindowEvent;

/// Errors from window creation or the event pump.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// A platform window call failed.
    #[error("window error: {0}")]
    Platform(String),
}

/// Options controlling window creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOptions {
    /// The window title.
    pub title: String,
    /// Requested client-area width in pixels.
    pub width: u32,
    /// Requested client-area height in pixels.
    pub height: u32,
    /// Whether the user may resize the window.
    pub resizable: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "RDPiO".into(),
            width: 1280,
            height: 800,
            resizable: true,
        }
    }
}

impl WindowOptions {
    /// Options derived from the parsed CLI: the desktop size the session was
    /// requested at becomes the initial client-area size.
    pub fn for_opts(opts: &CliOptions) -> Self {
        Self {
            title: "RDPiO".into(),
            width: opts.width as u32,
            height: opts.height as u32,
            resizable: true,
        }
    }
}

/// The decision from one pump of the event queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Keep the loop running; apply `resize` (if any) before the next present.
    Continue { resize: Option<(u32, u32)> },
    /// The window is closing; the loop must exit.
    Quit,
}

/// How a front-end hands its native surface to the D3D11 present path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeWindowHandle {
    /// A real OS window handle in raw-window-handle encoding.
    Os(RawWindowHandle),
    /// No display (headless CI runs and unit tests).
    Headless,
}

/// Encode a Win32 `HWND` (plus its module instance, when known) as a raw
/// window handle. A pure helper so the encoding is unit-testable without ever
/// opening a window. Returns `None` for a null `HWND`.
pub fn win32_handle(hwnd: isize, hinstance: Option<isize>) -> Option<RawWindowHandle> {
    let hwnd = NonZeroIsize::new(hwnd)?;
    let mut handle = Win32WindowHandle::new(hwnd);
    handle.hinstance = hinstance.and_then(NonZeroIsize::new);
    Some(RawWindowHandle::Win32(handle))
}

/// The window/application abstraction shared by every front-end.
///
/// Implementors open a window (or a headless stand-in), run the platform event
/// pump, and expose a raw window handle the D3D11 decoder/present path can use.
pub trait AppWindow {
    /// Errors from window creation or pumping.
    type Error: std::fmt::Display;

    /// Open the window.
    fn open(options: WindowOptions) -> Result<Self, Self::Error>
    where
        Self: Sized;

    /// The native handle for the D3D11 present path.
    fn native_handle(&self) -> NativeWindowHandle;

    /// The raw window handle, when the front-end has a real OS window.
    fn raw_window_handle(&self) -> Option<RawWindowHandle> {
        match self.native_handle() {
            NativeWindowHandle::Os(handle) => Some(handle),
            NativeWindowHandle::Headless => None,
        }
    }

    /// Pump the platform queue once: return the close/resize decision.
    fn pump(&mut self) -> Frame;

    /// Drain the platform events captured since the last pump.
    fn drain_events(&mut self) -> Vec<WindowEvent>;

    /// The current client-area size.
    fn size(&self) -> (u32, u32);

    /// Ask the window to close; the next `pump` returns `Frame::Quit`.
    fn request_close(&mut self);
}

/// A display-free window for headless hosts, CI, and unit tests.
///
/// No OS window is created: the raw handle is [`NativeWindowHandle::Headless`]
/// and events are only ever injected via [`HeadlessWindow::queue_event`]. The
/// event-loop contract is the same as a real window — `pump` reports the
/// pending resize and the quit decision, `drain_events` hands back the queued
/// events — so the integration code paths are exercised without a display.
#[derive(Debug, Clone)]
pub struct HeadlessWindow {
    options: WindowOptions,
    events: Vec<WindowEvent>,
    running: bool,
}

impl HeadlessWindow {
    /// Open a headless window. Never touches a display and cannot fail for
    /// platform reasons; the `Result` keeps the [`AppWindow`] shape uniform.
    pub fn open(options: WindowOptions) -> Result<Self, UiError> {
        Ok(Self {
            options,
            events: Vec::new(),
            running: true,
        })
    }

    /// Inject a synthetic event (used by tests and headless harnesses).
    pub fn queue_event(&mut self, event: WindowEvent) {
        if self.running {
            self.events.push(event);
        }
    }

    /// The options the window was opened with.
    pub fn options(&self) -> &WindowOptions {
        &self.options
    }
}

impl AppWindow for HeadlessWindow {
    type Error = UiError;

    fn open(options: WindowOptions) -> Result<Self, Self::Error> {
        HeadlessWindow::open(options)
    }

    fn native_handle(&self) -> NativeWindowHandle {
        NativeWindowHandle::Headless
    }

    fn pump(&mut self) -> Frame {
        if !self.running {
            return Frame::Quit;
        }
        // The most recent resize event wins; the UI loop applies it and the
        // caller drains the events with `drain_events`.
        let resize = self.events.iter().rev().find_map(|ev| match ev {
            WindowEvent::Resized { width, height } => Some((*width, *height)),
            _ => None,
        });
        Frame::Continue { resize }
    }

    fn drain_events(&mut self) -> Vec<WindowEvent> {
        std::mem::take(&mut self.events)
    }

    fn size(&self) -> (u32, u32) {
        (self.options.width, self.options.height)
    }

    fn request_close(&mut self) {
        self.running = false;
        self.events.push(WindowEvent::CloseRequested);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::event::WindowEvent;

    #[test]
    fn options_defaults_are_sane() {
        let o = WindowOptions::default();
        assert_eq!(o.title, "RDPiO");
        assert_eq!((o.width, o.height), (1280, 800));
        assert!(o.resizable);
    }

    #[test]
    fn options_derive_from_cli() {
        let opts = CliOptions {
            host: "h".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let o = WindowOptions::for_opts(&opts);
        assert_eq!((o.width, o.height), (1920, 1080));
        assert!(o.resizable);
    }

    #[test]
    fn win32_handle_encodes_hwnd_and_hinstance() {
        let h = win32_handle(0x1234, Some(0x5678)).expect("non-zero hwnd encodes");
        match h {
            RawWindowHandle::Win32(w) => {
                assert_eq!(w.hwnd.get(), 0x1234);
                assert_eq!(w.hinstance.map(NonZeroIsize::get), Some(0x5678));
            }
            other => panic!("expected Win32 handle, got {other:?}"),
        }
    }

    #[test]
    fn win32_handle_rejects_null_hwnd() {
        assert!(win32_handle(0, None).is_none());
    }

    #[test]
    fn headless_window_never_opens_a_display() {
        let win = HeadlessWindow::open(WindowOptions::default()).unwrap();
        assert_eq!(win.native_handle(), NativeWindowHandle::Headless);
        assert_eq!(win.raw_window_handle(), None);
        assert_eq!(win.size(), (1280, 800));
    }

    #[test]
    fn headless_pump_reports_resize_then_quit() {
        let mut win = HeadlessWindow::open(WindowOptions {
            width: 800,
            height: 600,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(win.size(), (800, 600));
        win.queue_event(WindowEvent::Resized {
            width: 1024,
            height: 768,
        });
        assert_eq!(
            win.pump(),
            Frame::Continue {
                resize: Some((1024, 768)),
            }
        );
        win.request_close();
        assert_eq!(win.pump(), Frame::Quit);
    }

    #[test]
    fn headless_events_are_drained_in_order() {
        let mut win = HeadlessWindow::open(WindowOptions::default()).unwrap();
        win.queue_event(WindowEvent::MouseMove { x: 1, y: 2 });
        win.queue_event(WindowEvent::Keyboard {
            scancode: 0x1c,
            extended: false,
            down: true,
        });
        let evs = win.drain_events();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], WindowEvent::MouseMove { x: 1, y: 2 }));
        assert!(matches!(
            evs[1],
            WindowEvent::Keyboard {
                scancode: 0x1c,
                down: true,
                ..
            }
        ));
        assert!(win.drain_events().is_empty());
    }

    #[test]
    fn events_queued_after_close_are_dropped() {
        let mut win = HeadlessWindow::open(WindowOptions::default()).unwrap();
        win.request_close();
        win.queue_event(WindowEvent::MouseMove { x: 9, y: 9 });
        assert_eq!(win.pump(), Frame::Quit);
        // The close request itself is delivered; the mouse event queued
        // after the close is dropped.
        assert_eq!(win.drain_events(), vec![WindowEvent::CloseRequested]);
    }
}
