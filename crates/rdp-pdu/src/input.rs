//! Client input (MS-RDPBCGR 2.2.8.1.1.3): the slow-path Input Event PDU.
//!
//! We send input over the slow path — a Share Data PDU (`PDUTYPE2_INPUT`)
//! carried in an MCS Send Data Request on the I/O channel. This reuses the
//! Standard RDP Security wrap (RC4 + MAC) already used for activation, instead
//! of the separately-encrypted fast-path input format. Each event is a fixed
//! 12-byte `TS_INPUT_EVENT`: `eventTime`(4) + `messageType`(2) + 6 bytes of
//! event data.

use crate::finalization::share_data;

/// Share Data `pduType2` for an input event PDU.
pub const PDUTYPE2_INPUT: u8 = 28;

// messageType values.
pub const INPUT_EVENT_SYNC: u16 = 0x0000;
pub const INPUT_EVENT_SCANCODE: u16 = 0x0004;
pub const INPUT_EVENT_UNICODE: u16 = 0x0005;
pub const INPUT_EVENT_MOUSE: u16 = 0x8001;
pub const INPUT_EVENT_MOUSEX: u16 = 0x8002;
/// TS_RELPOINTER_EVENT (RDP 10.7+): relative mouse motion/buttons. Only valid
/// when the server advertised `INPUT_FLAG_MOUSE_RELATIVE` in its input caps.
pub const INPUT_EVENT_MOUSEREL: u16 = 0x8004;

// Keyboard event flags (TS_KEYBOARD_EVENT.keyboardFlags).
pub const KBDFLAGS_EXTENDED: u16 = 0x0100;
pub const KBDFLAGS_DOWN: u16 = 0x4000;
pub const KBDFLAGS_RELEASE: u16 = 0x8000;

// Pointer event flags (TS_POINTER_EVENT.pointerFlags).
pub const PTRFLAGS_HWHEEL: u16 = 0x0400;
pub const PTRFLAGS_WHEEL: u16 = 0x0200;
pub const PTRFLAGS_WHEEL_NEGATIVE: u16 = 0x0100;
/// The signed wheel-rotation field within the pointer flags (MS-RDPBCGR
/// 2.2.8.1.1.3.1.1.3, "WheelRotationMask"). It is 9 bits wide; its top bit is
/// [`PTRFLAGS_WHEEL_NEGATIVE`], i.e. the field is a two's-complement signed value.
pub const PTRFLAGS_WHEEL_ROTATION_MASK: u16 = 0x01ff;
pub const PTRFLAGS_MOVE: u16 = 0x0800;
pub const PTRFLAGS_DOWN: u16 = 0x8000;
pub const PTRFLAGS_BUTTON1: u16 = 0x1000; // left
pub const PTRFLAGS_BUTTON2: u16 = 0x2000; // right
pub const PTRFLAGS_BUTTON3: u16 = 0x4000; // middle

// Extended pointer event flags (TS_POINTERX_EVENT.pointerFlags).
pub const PTRXFLAGS_DOWN: u16 = 0x8000;
pub const PTRXFLAGS_BUTTON1: u16 = 0x0001; // XBUTTON1
pub const PTRXFLAGS_BUTTON2: u16 = 0x0002; // XBUTTON2

/// One encoded 12-byte input event.
pub type EventBytes = [u8; 12];

fn event(message_type: u16, a: u16, b: u16, c: u16) -> EventBytes {
    let mut e = [0u8; 12];
    // eventTime (4 bytes) left zero — the server ignores it.
    e[4..6].copy_from_slice(&message_type.to_le_bytes());
    e[6..8].copy_from_slice(&a.to_le_bytes());
    e[8..10].copy_from_slice(&b.to_le_bytes());
    e[10..12].copy_from_slice(&c.to_le_bytes());
    e
}

/// A keyboard scancode event. `flags` is a mask of `KBDFLAGS_*`.
pub fn keyboard_event(flags: u16, key_code: u16) -> EventBytes {
    event(INPUT_EVENT_SCANCODE, flags, key_code, 0)
}

/// A Unicode keyboard event (for keys with no scancode). `flags` uses the same
/// `KBDFLAGS_*` release bit.
pub fn unicode_event(flags: u16, code: u16) -> EventBytes {
    event(INPUT_EVENT_UNICODE, flags, code, 0)
}

/// A mouse event. `flags` is a mask of `PTRFLAGS_*`; `x`/`y` are desktop pixels.
pub fn mouse_event(flags: u16, x: u16, y: u16) -> EventBytes {
    event(INPUT_EVENT_MOUSE, flags, x, y)
}

/// An extended mouse event (XBUTTON1/2). `flags` is a mask of `PTRXFLAGS_*`.
pub fn mouse_x_event(flags: u16, x: u16, y: u16) -> EventBytes {
    event(INPUT_EVENT_MOUSEX, flags, x, y)
}

/// A relative mouse event (TS_RELPOINTER_EVENT): `flags` uses the same
/// `PTRFLAGS_*` move/button semantics as [`mouse_event`], but `dx`/`dy` are
/// SIGNED motion deltas instead of absolute coordinates. The FPS-game input
/// path: the remote pointer moves by the delta, so aiming never pins at a
/// screen edge. Requires server `INPUT_FLAG_MOUSE_RELATIVE` support.
pub fn rel_mouse_event(flags: u16, dx: i16, dy: i16) -> EventBytes {
    event(INPUT_EVENT_MOUSEREL, flags, dx as u16, dy as u16)
}

/// A synchronize event carrying the lock-key toggle state (`toggle_flags`:
/// SCROLL_LOCK 0x1, NUM_LOCK 0x2, CAPS_LOCK 0x4, KANA_LOCK 0x8). Sent once at
/// session start so the server agrees on lock-key state.
pub fn sync_event(toggle_flags: u32) -> EventBytes {
    let mut e = [0u8; 12];
    e[4..6].copy_from_slice(&INPUT_EVENT_SYNC.to_le_bytes());
    // 6..8 pad2Octets = 0; 8..12 = toggleFlags (4 bytes).
    e[8..12].copy_from_slice(&toggle_flags.to_le_bytes());
    e
}

/// Build a slow-path Input Event PDU (Share Data, `PDUTYPE2_INPUT`) carrying
/// `events`, sourced from the client's `user_id` for `share_id`.
pub fn input_pdu(share_id: u32, user_id: u16, events: &[EventBytes]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + events.len() * 12);
    body.extend_from_slice(&(events.len() as u16).to_le_bytes()); // numberEvents
    body.extend_from_slice(&0u16.to_le_bytes()); // pad2Octets
    for ev in events {
        body.extend_from_slice(ev);
    }
    share_data(share_id, user_id, PDUTYPE2_INPUT, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finalization::data_pdu_type2;

    #[test]
    fn keyboard_event_layout() {
        let e = keyboard_event(KBDFLAGS_RELEASE | KBDFLAGS_EXTENDED, 0x1D);
        assert_eq!(u16::from_le_bytes([e[4], e[5]]), INPUT_EVENT_SCANCODE);
        assert_eq!(u16::from_le_bytes([e[6], e[7]]), 0x8100);
        assert_eq!(u16::from_le_bytes([e[8], e[9]]), 0x1D);
    }

    #[test]
    fn mouse_event_layout() {
        let e = mouse_event(PTRFLAGS_MOVE, 640, 480);
        assert_eq!(u16::from_le_bytes([e[4], e[5]]), INPUT_EVENT_MOUSE);
        assert_eq!(u16::from_le_bytes([e[6], e[7]]), PTRFLAGS_MOVE);
        assert_eq!(u16::from_le_bytes([e[8], e[9]]), 640);
        assert_eq!(u16::from_le_bytes([e[10], e[11]]), 480);
    }

    #[test]
    fn sync_event_carries_toggle_flags() {
        let e = sync_event(0x4); // CAPS_LOCK
        assert_eq!(u16::from_le_bytes([e[4], e[5]]), INPUT_EVENT_SYNC);
        assert_eq!(u32::from_le_bytes([e[8], e[9], e[10], e[11]]), 0x4);
    }

    #[test]
    fn input_pdu_is_a_well_formed_share_data() {
        let events = [
            mouse_event(PTRFLAGS_MOVE, 10, 20),
            keyboard_event(0, 0x1E), // 'A' down
        ];
        let pdu = input_pdu(0x0001_03EA, 1007, &events);
        // Share Control Header totalLength == buffer length.
        assert_eq!(u16::from_le_bytes([pdu[0], pdu[1]]) as usize, pdu.len());
        // It is a Data PDU of type PDUTYPE2_INPUT.
        assert_eq!(data_pdu_type2(&pdu), Some(PDUTYPE2_INPUT));
        // numberEvents (first body field after the 18-byte share data header).
        assert_eq!(u16::from_le_bytes([pdu[18], pdu[19]]), 2);
        // Total = 18 (header) + 4 (numberEvents+pad) + 2*12 (events).
        assert_eq!(pdu.len(), 18 + 4 + 24);
    }
}
