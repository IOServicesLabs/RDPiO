//! Android keys → PC scancodes (what the host's `SendInput` takes).
//!
//! A hardware keyboard reports Linux evdev codes as the scan code
//! (`KeyEvent.getScanCode`): physical positions, so the host's own layout
//! applies, as with a keyboard plugged into it. Codes 1–88 *are* XT set-1
//! scancodes; the rest are the E0-prefixed ("extended") keys. A key without a
//! scan code (an on-screen or virtual keyboard) falls back to its Android key
//! code, mapped to the evdev code of the same key on a US keyboard.

/// Set-1 scancode and the E0 prefix for an evdev code.
pub fn evdev_to_set1(code: u32) -> Option<(u16, bool)> {
    Some(match code {
        // Num Lock and Pause share 0x45: Windows tells them apart by the
        // extended flag (as the desktop client's keyboard hook reports them).
        69 => (0x45, true),
        // Zenkaku/Hankaku: the key left of 1 on a Japanese keyboard.
        85 => (0x29, false),
        1..=88 => (code as u16, false),
        89 => (0x73, false), // Ro (Japanese)
        92 => (0x79, false), // Henkan (Japanese)
        93 => (0x70, false), // Katakana/Hiragana (Japanese)
        94 => (0x7B, false), // Muhenkan (Japanese)
        96 => (0x1C, true),  // keypad Enter
        97 => (0x1D, true),  // right Ctrl
        98 => (0x35, true),  // keypad /
        99 => (0x37, true),  // Print Screen
        100 => (0x38, true), // right Alt (AltGr)
        102 => (0x47, true), // Home
        103 => (0x48, true), // Up
        104 => (0x49, true), // Page Up
        105 => (0x4B, true), // Left
        106 => (0x4D, true), // Right
        107 => (0x4F, true), // End
        108 => (0x50, true), // Down
        109 => (0x51, true), // Page Down
        110 => (0x52, true), // Insert
        111 => (0x53, true), // Delete
        113 => (0x20, true), // Mute
        114 => (0x2E, true), // Volume down
        115 => (0x30, true), // Volume up
        117 => (0x59, false), // keypad =
        119 => (0x45, false), // Pause
        121 => (0x7E, false), // keypad , (Brazilian)
        122 => (0x72, false), // Hangul/English (Korean)
        123 => (0x71, false), // Hanja (Korean)
        124 => (0x7D, false), // Yen (Japanese)
        125 => (0x5B, true), // left Windows
        126 => (0x5C, true), // right Windows
        127 => (0x5D, true), // Menu
        163 => (0x19, true), // next track
        164 => (0x22, true), // play/pause
        165 => (0x10, true), // previous track
        166 => (0x24, true), // stop
        183..=193 => (code as u16 - 183 + 0x64, false), // F13..F23
        194 => (0x76, false),                           // F24
        _ => return None,
    })
}

/// Android's modifier key codes: these follow the key code, not the scan
/// code, so a modifier remapped in Android's keyboard settings (Caps Lock as
/// Ctrl, say) is remapped on the PC too.
fn is_modifier(keycode: u32) -> bool {
    matches!(keycode, 57 | 58 | 59 | 60 | 113 | 114 | 115 | 117 | 118)
}

/// The evdev code for an Android `KeyEvent.KEYCODE_*` (US positions).
pub fn android_keycode_to_evdev(keycode: u32) -> Option<u32> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    ];
    Some(match keycode {
        7 => 11,                 // 0
        8..=16 => keycode - 6,   // 1..9 → 2..10
        29..=54 => LETTERS[(keycode - 29) as usize],
        19 => 103,  // DPAD_UP
        20 => 108,  // DPAD_DOWN
        21 => 105,  // DPAD_LEFT
        22 => 106,  // DPAD_RIGHT
        55 => 51,   // ,
        56 => 52,   // .
        57 => 56,   // left Alt
        58 => 100,  // right Alt
        59 => 42,   // left Shift
        60 => 54,   // right Shift
        61 => 15,   // Tab
        62 => 57,   // Space
        66 => 28,   // Enter
        67 => 14,   // Backspace (KEYCODE_DEL)
        68 => 41,   // `
        69 => 12,   // -
        70 => 13,   // =
        71 => 26,   // [
        72 => 27,   // ]
        73 => 43,   // \
        74 => 39,   // ;
        75 => 40,   // '
        76 => 53,   // /
        82 => 127,  // Menu
        85 => 164,  // media play/pause
        86 => 166,  // media stop
        87 => 163,  // media next
        88 => 165,  // media previous
        92 => 104,  // Page Up
        93 => 109,  // Page Down
        111 => 1,   // Escape
        112 => 111, // Delete (KEYCODE_FORWARD_DEL)
        113 => 29,  // left Ctrl
        114 => 97,  // right Ctrl
        115 => 58,  // Caps Lock
        116 => 70,  // Scroll Lock
        117 => 125, // left Meta
        118 => 126, // right Meta
        120 => 99,  // SysRq / Print Screen
        121 => 119, // Break / Pause
        122 => 102, // Home
        123 => 107, // End
        124 => 110, // Insert
        131..=140 => keycode - 72, // F1..F10 → 59..68
        141 => 87,  // F11
        142 => 88,  // F12
        143 => 69,  // Num Lock
        144 => 82,  // keypad 0
        145 => 79,
        146 => 80,
        147 => 81,
        148 => 75,
        149 => 76,
        150 => 77,
        151 => 71,
        152 => 72,
        153 => 73, // keypad 9
        154 => 98, // keypad /
        155 => 55, // keypad *
        156 => 74, // keypad -
        157 => 78, // keypad +
        158 => 83, // keypad .
        159 => 121, // keypad ,
        160 => 96,  // keypad Enter
        161 => 117, // keypad =
        211 => 85,  // Zenkaku/Hankaku
        213 => 94,  // Muhenkan
        214 => 92,  // Henkan
        215 => 93,  // Katakana/Hiragana
        216 => 124, // Yen
        217 => 89,  // Ro
        _ => return None,
    })
}

/// The scancode for a key event: by its scan code where it has one we know,
/// else by its key code (and modifiers always by key code).
pub fn to_set1(scan_code: u32, keycode: u32) -> Option<(u16, bool)> {
    let by_keycode = || evdev_to_set1(android_keycode_to_evdev(keycode)?);
    if is_modifier(keycode) {
        return by_keycode();
    }
    (scan_code != 0).then(|| evdev_to_set1(scan_code)).flatten().or_else(by_keycode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_the_scancodes_windows_expects() {
        assert_eq!(to_set1(1, 0), Some((0x01, false))); // Esc
        assert_eq!(to_set1(30, 0), Some((0x1E, false))); // A
        assert_eq!(to_set1(103, 0), Some((0x48, true))); // Up
        assert_eq!(to_set1(0, 29), Some((0x1E, false))); // KEYCODE_A
        assert_eq!(to_set1(0, 8), Some((0x02, false))); // KEYCODE_1
        assert_eq!(to_set1(0, 7), Some((0x0B, false))); // KEYCODE_0
        assert_eq!(to_set1(0, 131), Some((0x3B, false))); // F1
        assert_eq!(to_set1(0, 140), Some((0x44, false))); // F10
        assert_eq!(to_set1(0, 117), Some((0x5B, true))); // Meta
        assert_eq!(to_set1(0, 9999), None);
        assert_eq!(to_set1(69, 143), Some((0x45, true))); // Num Lock
        assert_eq!(to_set1(119, 121), Some((0x45, false))); // Pause
        assert_eq!(to_set1(89, 217), Some((0x73, false))); // Ro
        assert_eq!(to_set1(183, 0), Some((0x64, false))); // F13
        // Caps Lock remapped to Ctrl in Android's settings: Ctrl on the PC.
        assert_eq!(to_set1(58, 113), Some((0x1D, false)));
    }
}
