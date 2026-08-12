//! Client input events (MS-RDPBCGR 2.2.8.1.1.3).
//!
//! High-level input events produced by the UI layer and serialized into the
//! `TS_INPUT_PDU` slow-path format by the wire layer. The flag constants live
//! in the [`kbd`] and [`ptr`] submodules and match the values used across the
//! workspace (`rdp-pdu`): `KBDFLAGS_*` for keyboard events, `PTRFLAGS_*` for
//! pointer events.

/// Keyboard event flag constants (`TS_KEYBOARD_EVENT.keyboardFlags`).
pub mod kbd {
    /// `KBDFLAGS_EXTENDED` — the key is on the extended (0xE0-prefixed) set:
    /// arrows, Insert/Delete/Home/End/PageUp/PageDown, right Ctrl/Alt/Win.
    pub const EXTENDED: u16 = 0x0100;
    /// `KBDFLAGS_RELEASE` — key-up transition (the transition bit).
    pub const RELEASE: u16 = 0x8000;
    /// `KBDFLAGS_DOWN` — key-down transition.
    pub const DOWN: u16 = 0x4000;
}

/// Pointer event flag constants (`TS_POINTER_EVENT.pointerFlags`).
pub mod ptr {
    /// `PTRFLAGS_BUTTON1` — left mouse button.
    pub const BUTTON1: u16 = 0x1000;
    /// `PTRFLAGS_BUTTON2` — right mouse button.
    pub const BUTTON2: u16 = 0x2000;
    /// `PTRFLAGS_BUTTON3` — middle mouse button.
    pub const BUTTON3: u16 = 0x4000;
    /// `PTRFLAGS_MOVE` — the pointer moved to `(x, y)`.
    pub const MOVE: u16 = 0x0800;
    /// `PTRFLAGS_DOWN` — the button transition is a press (else release).
    pub const DOWN: u16 = 0x8000;
    /// `PTRFLAGS_WHEEL` — vertical wheel rotation, magnitude in the
    /// `WheelRotationMask` (9-bit signed, 120 per notch).
    pub const WHEEL: u16 = 0x0200;
    /// `PTRFLAGS_HWHEEL` — horizontal wheel rotation.
    pub const HWHEEL: u16 = 0x0400;
}

/// One high-level input event, before wire serialization.
///
/// The UI layer builds these from Win32/headless input; the wire layer
/// encodes each into the fixed 12-byte `TS_INPUT_EVENT` record of a
/// slow-path Input Event PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// A scancode key press/release. `flags` is a mask of [`kbd`] constants.
    Keyboard {
        /// `KBDFLAGS_*` mask.
        flags: u16,
        /// The PC/AT scancode (0xE0-prefixed codes use `kbd::EXTENDED`).
        key_code: u16,
    },
    /// An absolute pointer event. `flags` is a mask of [`ptr`] constants.
    Mouse {
        /// `PTRFLAGS_*` mask.
        flags: u16,
        /// Desktop x coordinate.
        x: u16,
        /// Desktop y coordinate.
        y: u16,
    },
    /// An extended mouse event (XBUTTON1/2 buttons, wheel).
    ExtendedMouse {
        /// `PTRFLAGS_*` mask including the wheel/button fields.
        flags: u16,
        /// Desktop x coordinate.
        x: u16,
        /// Wheel magnitude (120 per notch) or y coordinate.
        y: u16,
    },
    /// A synchronize event carrying the toggle-key state.
    Sync {
        /// Bitmask of keys currently down (SCROLL_LOCK 0x1, NUM_LOCK 0x2,
        /// CAPS_LOCK 0x4, KANA_LOCK 0x8).
        number_of_keys: u16,
    },
}

impl InputEvent {
    /// The `TS_INPUT_EVENT.messageType` for this event.
    pub fn message_type(&self) -> u16 {
        match self {
            InputEvent::Keyboard { .. } => 0x0004, // INPUT_EVENT_SCANCODE
            InputEvent::Mouse { .. } => 0x8001,    // INPUT_EVENT_MOUSE
            InputEvent::ExtendedMouse { .. } => 0x8002, // INPUT_EVENT_MOUSEX
            InputEvent::Sync { .. } => 0x0000,     // INPUT_EVENT_SYNC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_types_match_ms_rdpbcgr() {
        assert_eq!(InputEvent::Sync { number_of_keys: 0 }.message_type(), 0x0000);
        assert_eq!(
            InputEvent::Keyboard {
                flags: 0,
                key_code: 0
            }
            .message_type(),
            0x0004
        );
        assert_eq!(
            InputEvent::Mouse {
                flags: 0,
                x: 0,
                y: 0
            }
            .message_type(),
            0x8001
        );
        assert_eq!(
            InputEvent::ExtendedMouse {
                flags: 0,
                x: 0,
                y: 0
            }
            .message_type(),
            0x8002
        );
    }

    #[test]
    fn flag_constants_match_workspace_values() {
        // Values shared with rdp-pdu::input must agree exactly.
        assert_eq!(kbd::EXTENDED, 0x0100);
        assert_eq!(kbd::RELEASE, 0x8000);
        assert_eq!(kbd::DOWN, 0x4000);
        assert_eq!(ptr::MOVE, 0x0800);
        assert_eq!(ptr::DOWN, 0x8000);
        assert_eq!(ptr::WHEEL, 0x0200);
        assert_eq!(ptr::BUTTON1, 0x1000);
        assert_eq!(ptr::BUTTON2, 0x2000);
        assert_eq!(ptr::BUTTON3, 0x4000);
    }
}
