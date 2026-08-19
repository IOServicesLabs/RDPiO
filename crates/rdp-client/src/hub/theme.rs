//! # Hub theme — palette, spacing, and GDI helpers
//!
//! Every color and grid value the hub UI paints comes from this module: the
//! palette constants here are the single source of truth, and `ui.rs` spells
//! no raw color literals. GDI [`COLORREF`] stores `0x00BBGGRR`; [`rgb`] builds
//! one from RGB bytes so the byte order can never be mis-remembered at a call
//! site.
//!
//! The palette is the classic Visual Studio dark theme:
//!   - window background `#1E1E1E`
//!   - panel / list background `#252526`
//!   - borders `#2D2D30`
//!   - primary text `#D4D4D4`, muted text `#808080`
//!   - accent `#0078D4` (Windows blue)
//!
//! Spacing uses an 8px grid at 96 DPI; [`scale`] lifts grid values to the
//! window's real DPI with MulDiv-style rounding.

use windows::Win32::Foundation::COLORREF;

/// Build a GDI [`COLORREF`] from RGB bytes. `COLORREF` is `0x00BBGGRR`, so
/// `rgb(r, g, b)` hides the byte order from callers entirely.
pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

// --- Core palette -----------------------------------------------------------

/// Hub window background (`#1E1E1E`).
pub const BG: COLORREF = rgb(0x1E, 0x1E, 0x1E);
/// Panel / list background (`#252526`): rail buttons, list rows, form panels.
pub const PANEL_BG: COLORREF = rgb(0x25, 0x25, 0x26);
/// 1px borders and separators (`#2D2D30`).
pub const BORDER: COLORREF = rgb(0x2D, 0x2D, 0x30);
/// Primary text (`#D4D4D4`).
pub const TEXT: COLORREF = rgb(0xD4, 0xD4, 0xD4);
/// Muted / secondary text (`#808080`): labels, inactive rail items.
pub const MUTED_TEXT: COLORREF = rgb(0x80, 0x80, 0x80);
/// Accent (`#0078D4`): active-section bar, primary button fill, selection.
pub const ACCENT: COLORREF = rgb(0x00, 0x78, 0xD4);

// --- Derived colors ---------------------------------------------------------

/// Hover fill: [`PANEL_BG`] lifted slightly.
pub const HOVER_BG: COLORREF = rgb(0x2E, 0x2E, 0x2E);
/// Active-section fill: brighter than hover so the current pane stands out.
pub const ACTIVE_BG: COLORREF = rgb(0x37, 0x37, 0x3A);
/// Selected list row: accent-tinted dark (VS Code selection tone).
pub const SELECTION_BG: COLORREF = rgb(0x04, 0x39, 0x5E);
/// Alternating list row: a touch lighter than [`PANEL_BG`].
pub const ROW_ALT_BG: COLORREF = rgb(0x2A, 0x2A, 0x2B);
/// Text drawn on accent-filled surfaces (primary button, accent bar).
pub const ON_ACCENT: COLORREF = rgb(0xFF, 0xFF, 0xFF);

// --- Spacing & DPI ----------------------------------------------------------

/// Base grid unit in 96-DPI pixels (8px). Scale with [`scale`].
pub const GRID: i32 = 8;

/// Scale a 96-DPI layout value to `dpi`, rounding like GDI's `MulDiv`
/// (`(px * dpi + 48) / 96` with truncating division). `scale(GRID, dpi)` gives
/// the DPI-aware grid step; at 100% it is identity, at 150% ×1.5, at 200% ×2.
pub const fn scale(px: i32, dpi: u32) -> i32 {
    ((px as i64 * dpi as i64 + 48) / 96) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_builds_bbggrr_colorref() {
        assert_eq!(rgb(0x1E, 0x1E, 0x1E).0, 0x001E1E1E);
        assert_eq!(rgb(0x00, 0x78, 0xD4).0, 0x00D47800);
        assert_eq!(rgb(0xFF, 0xFF, 0xFF).0, 0x00FFFFFF);
        assert_eq!(rgb(0x00, 0x00, 0x00).0, 0x00000000);
    }

    #[test]
    fn scale_is_identity_at_96_dpi_and_scales_with_dpi() {
        assert_eq!(scale(8, 96), 8);
        assert_eq!(scale(8, 144), 12);
        assert_eq!(scale(8, 192), 16);
        assert_eq!(scale(0, 96), 0);
        assert_eq!(scale(GRID, 96), GRID);
    }

    #[test]
    fn palette_matches_the_dark_theme_spec() {
        assert_eq!(BG.0, 0x001E1E1E); // #1E1E1E
        assert_eq!(PANEL_BG.0, 0x00262525); // #252526
        assert_eq!(BORDER.0, 0x00302D2D); // #2D2D30
        assert_eq!(TEXT.0, 0x00D4D4D4); // #D4D4D4
        assert_eq!(MUTED_TEXT.0, 0x00808080); // #808080
        assert_eq!(ACCENT.0, 0x00D47800); // #0078D4
    }
}
