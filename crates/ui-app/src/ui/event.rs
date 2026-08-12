//! Platform-agnostic window events and state mapping.
//!
//! The native front-ends (the Win32 window on Windows, the headless stand-in
//! elsewhere) translate their platform messages into the [`WindowEvent`] enum
//! defined here. The shared [`WindowState`] maps those events onto the
//! client-visible window state (size, cursor, held keys, run flag), and
//! [`to_input_events`] turns pointer/keyboard events into the RDP input PDUs
//! the session layer serializes into a `TS_INPUT_PDU`.
//!
//! All of this is pure logic — no window, display, or network — so it is fully
//! unit-testable on any host, and the unit tests never open a display.

use std::collections::HashSet;

use wire_main::InputEvent;

use crate::input::{self, MouseButton, WheelDirection};

/// A platform-agnostic window event. Front-ends translate their native
/// messages (Win32 `WM_*`, or synthetic headless events) into these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    /// The user asked to close the window (title-bar close, Alt+F4, `WM_CLOSE`).
    CloseRequested,
    /// The client area was resized to `width` × `height` pixels.
    Resized { width: u32, height: u32 },
    /// A key transition, identified by its hardware scancode. `extended` marks
    /// the E0-prefixed scancodes (arrows, nav cluster, right modifiers).
    Keyboard {
        scancode: u8,
        extended: bool,
        down: bool,
    },
    /// The pointer moved to client coordinates `(x, y)`.
    MouseMove { x: u32, y: u32 },
    /// A mouse button transition at client coordinates `(x, y)`.
    MouseButton {
        button: MouseButton,
        x: u32,
        y: u32,
        down: bool,
    },
    /// A vertical wheel notch at `(x, y)`. A positive `delta` is a wheel notch
    /// away from the user (up); negative is toward the user (down).
    MouseWheel { delta: i16, x: u32, y: u32 },
}

/// The client-visible window state, updated by applying [`WindowEvent`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowState {
    /// False once a close has been requested.
    running: bool,
    /// Client-area size in pixels.
    width: u32,
    height: u32,
    /// Last known pointer position in client pixels.
    cursor: (u32, u32),
    /// Scancodes of the keys currently held down.
    keys_down: HashSet<u8>,
    /// The most recent resize, cleared by [`Self::take_pending_resize`].
    pending_resize: Option<(u32, u32)>,
}

impl WindowState {
    /// Fresh state for a `width` × `height` window.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            running: true,
            width,
            height,
            cursor: (0, 0),
            keys_down: HashSet::new(),
            pending_resize: None,
        }
    }

    /// Whether the window is still open (no close has been requested).
    pub fn running(&self) -> bool {
        self.running
    }

    /// Current client-area size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Last known pointer position in client pixels.
    pub fn cursor(&self) -> (u32, u32) {
        self.cursor
    }

    /// The scancodes of the keys currently held down, sorted ascending.
    pub fn keys_down(&self) -> Vec<u8> {
        let mut keys: Vec<u8> = self.keys_down.iter().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Take the pending resize, if any. The UI loop applies it before the next
    /// present and then re-arms by calling this again.
    pub fn take_pending_resize(&mut self) -> Option<(u32, u32)> {
        self.pending_resize.take()
    }

    /// Apply one event to the window state.
    pub fn apply(&mut self, event: &WindowEvent) {
        match *event {
            WindowEvent::CloseRequested => self.running = false,
            WindowEvent::Resized { width, height } => {
                self.width = width;
                self.height = height;
                self.pending_resize = Some((width, height));
            }
            WindowEvent::Keyboard { scancode, down, .. } => {
                if down {
                    self.keys_down.insert(scancode);
                } else {
                    self.keys_down.remove(&scancode);
                }
            }
            WindowEvent::MouseMove { x, y }
            | WindowEvent::MouseButton { x, y, .. }
            | WindowEvent::MouseWheel { x, y, .. } => {
                self.cursor = (x, y);
            }
        }
    }
}

/// Map one window event to the RDP input events it produces. Close and resize
/// are handled locally by the UI and produce no input PDU; pointer/keyboard
/// events map 1:1 onto the wire event types.
pub fn to_input_events(event: &WindowEvent) -> Vec<InputEvent> {
    match *event {
        WindowEvent::Keyboard {
            scancode,
            extended,
            down,
        } => vec![if down {
            input::key_press(scancode, extended)
        } else {
            input::key_release(scancode, extended)
        }],
        WindowEvent::MouseMove { x, y } => vec![input::mouse_move(clamp(x), clamp(y))],
        WindowEvent::MouseButton { button, x, y, down } => {
            vec![input::mouse_button(button, down, clamp(x), clamp(y))]
        }
        WindowEvent::MouseWheel { delta, x, y } => {
            let direction = if delta >= 0 {
                WheelDirection::Up
            } else {
                WheelDirection::Down
            };
            vec![input::mouse_wheel(direction, clamp(x), clamp(y))]
        }
        WindowEvent::CloseRequested | WindowEvent::Resized { .. } => Vec::new(),
    }
}

/// Clamp client-pixel coordinates to the 16-bit RDP desktop coordinate space.
fn clamp(v: u32) -> u16 {
    v.min(u16::MAX as u32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire_main::{kbd, ptr};

    #[test]
    fn resize_updates_size_and_arms_pending_resize() {
        let mut st = WindowState::new(1280, 800);
        st.apply(&WindowEvent::Resized {
            width: 1920,
            height: 1080,
        });
        assert_eq!(st.size(), (1920, 1080));
        assert_eq!(st.take_pending_resize(), Some((1920, 1080)));
        assert_eq!(st.take_pending_resize(), None);
    }

    #[test]
    fn close_halts_the_window() {
        let mut st = WindowState::new(1, 1);
        assert!(st.running());
        st.apply(&WindowEvent::CloseRequested);
        assert!(!st.running());
    }

    #[test]
    fn keyboard_tracks_held_scancodes() {
        let mut st = WindowState::new(1, 1);
        st.apply(&WindowEvent::Keyboard {
            scancode: 0x1d,
            extended: true,
            down: true,
        });
        st.apply(&WindowEvent::Keyboard {
            scancode: 0x2a,
            extended: false,
            down: true,
        });
        assert_eq!(st.keys_down(), [0x1d, 0x2a]);
        st.apply(&WindowEvent::Keyboard {
            scancode: 0x1d,
            extended: true,
            down: false,
        });
        assert_eq!(st.keys_down(), [0x2a]);
    }

    #[test]
    fn pointer_events_track_the_cursor() {
        let mut st = WindowState::new(100, 100);
        st.apply(&WindowEvent::MouseMove { x: 40, y: 30 });
        assert_eq!(st.cursor(), (40, 30));
        st.apply(&WindowEvent::MouseButton {
            button: MouseButton::Left,
            x: 41,
            y: 31,
            down: true,
        });
        assert_eq!(st.cursor(), (41, 31));
        st.apply(&WindowEvent::MouseWheel {
            delta: 120,
            x: 42,
            y: 32,
        });
        assert_eq!(st.cursor(), (42, 32));
    }

    #[test]
    fn keyboard_events_map_to_input_pdus() {
        let evs = to_input_events(&WindowEvent::Keyboard {
            scancode: 0x1d,
            extended: true,
            down: true,
        });
        assert_eq!(
            evs,
            vec![InputEvent::Keyboard {
                flags: kbd::EXTENDED,
                key_code: 0x1d,
            }]
        );
        let up = to_input_events(&WindowEvent::Keyboard {
            scancode: 0x1c,
            extended: false,
            down: false,
        });
        assert_eq!(
            up,
            vec![InputEvent::Keyboard {
                flags: kbd::RELEASE,
                key_code: 0x1c,
            }]
        );
    }

    #[test]
    fn mouse_events_map_to_input_pdus() {
        let evs = to_input_events(&WindowEvent::MouseMove { x: 640, y: 480 });
        assert_eq!(
            evs,
            vec![InputEvent::Mouse {
                flags: ptr::MOVE,
                x: 640,
                y: 480,
            }]
        );
        let down = to_input_events(&WindowEvent::MouseButton {
            button: MouseButton::Left,
            x: 1,
            y: 2,
            down: true,
        });
        assert_eq!(
            down,
            vec![InputEvent::Mouse {
                flags: ptr::BUTTON1 | ptr::DOWN,
                x: 1,
                y: 2,
            }]
        );
    }

    #[test]
    fn wheel_direction_maps_to_the_sign_bit() {
        let up = to_input_events(&WindowEvent::MouseWheel {
            delta: 120,
            x: 5,
            y: 5,
        });
        let down = to_input_events(&WindowEvent::MouseWheel {
            delta: -120,
            x: 5,
            y: 5,
        });
        match (&up[0], &down[0]) {
            (
                InputEvent::ExtendedMouse { flags: up_f, .. },
                InputEvent::ExtendedMouse { flags: down_f, .. },
            ) => {
                assert_eq!(*up_f, ptr::WHEEL);
                assert_eq!(*down_f, ptr::WHEEL | 0x0100); // PTRFLAGS_WHEEL_NEGATIVE
            }
            other => panic!("expected extended mouse events, got {other:?}"),
        }
    }

    #[test]
    fn coordinates_clamp_to_16_bit_desktop() {
        let evs = to_input_events(&WindowEvent::MouseMove {
            x: u32::MAX,
            y: 70_000,
        });
        assert_eq!(
            evs,
            vec![InputEvent::Mouse {
                flags: ptr::MOVE,
                x: u16::MAX,
                y: u16::MAX,
            }]
        );
    }

    #[test]
    fn close_and_resize_produce_no_input_pdus() {
        assert!(to_input_events(&WindowEvent::CloseRequested).is_empty());
        assert!(to_input_events(&WindowEvent::Resized {
            width: 640,
            height: 480,
        })
        .is_empty());
    }
}
