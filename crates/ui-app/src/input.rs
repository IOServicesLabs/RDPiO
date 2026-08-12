//! Keyboard/mouse input encoding.
//!
//! Turns high-level UI events (key press/release, mouse moves, buttons, wheel)
//! into the RDP input events the wire layer serializes into a TS_INPUT_PDU.
//! Pure logic — no window or network — so it is unit-testable everywhere.

use wire_main::{kbd, ptr, InputEvent};

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// Wheel rotation direction (magnitude fixed at 120 per RDP convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDirection {
    Up,
    Down,
}

/// `PTRFLAGS_XBUTTON1` — the first extended mouse button.
const PTR_FLAG_XBUTTON1: u16 = 0x0080;
/// `PTRFLAGS_XBUTTON2` — the second extended mouse button.
const PTR_FLAG_XBUTTON2: u16 = 0x0100;
/// `PTRFLAGS_WHEEL_NEGATIVE` — the wheel rotation sign bit.
const PTR_FLAG_WHEEL_NEGATIVE: u16 = 0x0100;

fn button_flag(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => ptr::BUTTON1,
        MouseButton::Right => ptr::BUTTON2,
        MouseButton::Middle => ptr::BUTTON3,
        MouseButton::X1 => PTR_FLAG_XBUTTON1,
        MouseButton::X2 => PTR_FLAG_XBUTTON2,
    }
}

/// A scancode key press. `extended` selects the 0xE0-prefixed scancodes
/// (arrows, Insert/Delete/Home/End/PageUp/PageDown, the right Ctrl/Alt/Win).
pub fn key_press(scancode: u8, extended: bool) -> InputEvent {
    let flags = if extended { kbd::EXTENDED } else { 0 };
    InputEvent::Keyboard {
        flags,
        key_code: scancode as u16,
    }
}

/// A scancode key release (the `KBD_FLAG_UP` bit is set).
pub fn key_release(scancode: u8, extended: bool) -> InputEvent {
    let flags = if extended {
        kbd::EXTENDED | kbd::RELEASE
    } else {
        kbd::RELEASE
    };
    InputEvent::Keyboard {
        flags,
        key_code: scancode as u16,
    }
}

/// Move the pointer to `(x, y)` in desktop coordinates.
pub fn mouse_move(x: u16, y: u16) -> InputEvent {
    InputEvent::Mouse {
        flags: ptr::MOVE,
        x,
        y,
    }
}

/// Press or release a mouse button at `(x, y)`.
pub fn mouse_button(button: MouseButton, down: bool, x: u16, y: u16) -> InputEvent {
    let mut flags = button_flag(button);
    if down {
        flags |= ptr::DOWN;
    }
    InputEvent::Mouse { flags, x, y }
}

/// Rotate the wheel by one notch (120) at `(x, _y)`. The position's y
/// coordinate is not part of the RDP extended-mouse encoding (the `y` field of
/// `ExtendedMouse` carries the rotation magnitude), so it is ignored.
pub fn mouse_wheel(direction: WheelDirection, x: u16, _y: u16) -> InputEvent {
    let mut flags = ptr::WHEEL;
    if direction == WheelDirection::Down {
        flags |= PTR_FLAG_WHEEL_NEGATIVE;
    }
    InputEvent::ExtendedMouse {
        flags,
        x,
        y: 120, // magnitude
    }
}

/// A Synchronize event reporting the toggle-key state (bitmask of the keys
/// currently down, matching the `numberOfKeys` field).
pub fn synchronize(number_of_keys: u16) -> InputEvent {
    InputEvent::Sync { number_of_keys }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_press_sets_extended_flag() {
        match key_press(0x1d, true) {
            InputEvent::Keyboard { flags, key_code } => {
                assert_eq!(flags, kbd::EXTENDED);
                assert_eq!(key_code, 0x1d);
            }
            other => panic!("expected keyboard event, got {other:?}"),
        }
    }

    #[test]
    fn key_release_sets_up_flag() {
        match key_release(0x1c, false) {
            InputEvent::Keyboard { flags, key_code } => {
                assert_eq!(flags, kbd::RELEASE);
                assert_eq!(key_code, 0x1c);
            }
            other => panic!("expected keyboard event, got {other:?}"),
        }
    }

    #[test]
    fn mouse_move_carries_coordinates() {
        match mouse_move(640, 480) {
            InputEvent::Mouse { flags, x, y } => {
                assert_eq!(flags, ptr::MOVE);
                assert_eq!((x, y), (640, 480));
            }
            other => panic!("expected mouse event, got {other:?}"),
        }
    }

    #[test]
    fn mouse_button_down_and_up_flip_down_bit() {
        let down = match mouse_button(MouseButton::Left, true, 1, 2) {
            InputEvent::Mouse { flags, .. } => flags,
            other => panic!("expected mouse event, got {other:?}"),
        };
        let up = match mouse_button(MouseButton::Left, false, 1, 2) {
            InputEvent::Mouse { flags, .. } => flags,
            other => panic!("expected mouse event, got {other:?}"),
        };
        assert_eq!(down, ptr::BUTTON1 | ptr::DOWN);
        assert_eq!(up, ptr::BUTTON1);
    }

    #[test]
    fn xbuttons_map_to_extended_flags() {
        assert_eq!(button_flag(MouseButton::X1), PTR_FLAG_XBUTTON1);
        assert_eq!(button_flag(MouseButton::X2), PTR_FLAG_XBUTTON2);
    }

    #[test]
    fn wheel_direction_sets_sign_bit() {
        let up = match mouse_wheel(WheelDirection::Up, 5, 5) {
            InputEvent::ExtendedMouse { flags, y, .. } => (flags, y),
            other => panic!("expected extended mouse event, got {other:?}"),
        };
        let down = match mouse_wheel(WheelDirection::Down, 5, 5) {
            InputEvent::ExtendedMouse { flags, y, .. } => (flags, y),
            other => panic!("expected extended mouse event, got {other:?}"),
        };
        assert_eq!(up, (ptr::WHEEL, 120));
        assert_eq!(down, (ptr::WHEEL | PTR_FLAG_WHEEL_NEGATIVE, 120));
    }

    #[test]
    fn synchronize_reports_toggle_count() {
        assert_eq!(synchronize(3), InputEvent::Sync { number_of_keys: 3 });
    }
}
