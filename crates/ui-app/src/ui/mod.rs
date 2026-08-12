//! Native UI window module.
//!
//! The platform-agnostic window/event abstraction shared by every front-end
//! of the `ui-app` binary:
//!
//! * [`event`] defines the [`WindowEvent`]s the front-ends translate their
//!   platform messages into, the [`WindowState`] that maps those events onto
//!   the client-visible window state (size, cursor, held keys, run flag), and
//!   the translation of pointer/keyboard events into RDP input PDUs.
//! * [`window`] defines the [`AppWindow`] contract — open, pump the event
//!   loop, close, and expose a raw window handle for the Direct3D 11
//!   decoder/present path — with the display-free [`HeadlessWindow`]
//!   implementation so CI and unit tests never need a display.
//!
//! Everything here is pure logic: no window, display, or network is required
//! to build or test this module on any host.

pub mod event;
pub mod window;

pub use event::{WindowEvent, WindowState};
pub use window::{AppWindow, Frame, HeadlessWindow, NativeWindowHandle, UiError, WindowOptions};
