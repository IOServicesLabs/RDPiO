//! # Hub theme — palette, spacing, and GDI helpers
//!
//! Every color and grid value the hub UI paints comes from this module: the
//! palette constants here are the single source of truth, and `ui.rs` spells
//! no raw color literals. GDI [`COLORREF`] stores `0x00BBGGRR`; [`rgb`] builds
//! one from RGB bytes so the byte order can never be mis-remembered at a call
//! site.
//!
//! The palette is the classic Visual Studio dark theme:
//!   - window background `#1E1E1E` ([`BG`])
//!   - panel / list background `#252526` ([`PANEL`])
//!   - borders `#2D2D30` ([`BORDER`])
//!   - primary text `#D4D4D4` ([`TEXT`]), muted text `#808080` ([`MUTED`])
//!   - accent `#0078D4` ([`ACCENT`], Windows blue)
//!
//! Row tints ([`ROW_ALT`], [`ROW_HOVER`], [`ROW_SELECT`]) are *derived* from
//! the palette through [`color_mix`] / [`lighten`], so the handful of hand
//! picked anchors above is the only place colors are chosen. Spacing uses an
//! 8px grid at 96 DPI; [`scale_px`] lifts grid values to the window's real DPI
//! with MulDiv-style rounding. [`ui_font`] / [`ui_font_semibold`] return the
//! cached Segoe UI faces (9pt / 10pt semibold) every hub control is set to.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::w;
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, HFONT, HGDIOBJ, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
};

/// Build a GDI [`COLORREF`] from RGB bytes. `COLORREF` is `0x00BBGGRR`, so
/// `rgb(r, g, b)` hides the byte order from callers entirely.
pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

// --- Core palette -----------------------------------------------------------

/// Hub window background (`#1E1E1E`).
pub const BG: COLORREF = rgb(0x1E, 0x1E, 0x1E);
/// Panel / list background (`#252526`): rail buttons, list rows, form panels.
pub const PANEL: COLORREF = rgb(0x25, 0x25, 0x26);
/// 1px borders and separators (`#2D2D30`).
pub const BORDER: COLORREF = rgb(0x2D, 0x2D, 0x30);
/// Primary text (`#D4D4D4`).
pub const TEXT: COLORREF = rgb(0xD4, 0xD4, 0xD4);
/// Muted / secondary text (`#808080`): labels, inactive rail items.
pub const MUTED: COLORREF = rgb(0x80, 0x80, 0x80);
/// Accent (`#0078D4`): active-section bar, primary button fill, selection.
pub const ACCENT: COLORREF = rgb(0x00, 0x78, 0xD4);

// --- Back-compat aliases ----------------------------------------------------
//
// The first hub iteration shipped `PANEL_BG` / `MUTED_TEXT`; keep them as
// aliases so existing call sites (ui.rs) keep compiling and the canonical
// names from the plan are the ones future steps spell. The row-tint aliases
// (`ROW_ALT_BG`, `HOVER_BG`, `SELECTION_BG`) live with the derived colors
// below, after their canonical definitions.

/// Alias for [`PANEL`] (pre-step-2 name).
pub const PANEL_BG: COLORREF = PANEL;
/// Alias for [`MUTED`] (pre-step-2 name).
pub const MUTED_TEXT: COLORREF = MUTED;

// --- Color math -------------------------------------------------------------

/// Pure white, the lighten endpoint.
pub const WHITE: COLORREF = rgb(0xFF, 0xFF, 0xFF);

/// Red channel of a [`COLORREF`].
#[inline]
const fn r_of(c: COLORREF) -> u8 {
    (c.0 & 0xFF) as u8
}

/// Green channel of a [`COLORREF`].
#[inline]
const fn g_of(c: COLORREF) -> u8 {
    ((c.0 >> 8) & 0xFF) as u8
}

/// Blue channel of a [`COLORREF`].
#[inline]
const fn b_of(c: COLORREF) -> u8 {
    ((c.0 >> 16) & 0xFF) as u8
}

/// Linearly interpolate channel `a` toward `b` by `t` percent (0..=100).
/// Integer math only, so the whole chain stays `const fn`.
const fn mix_channel(a: u8, b: u8, t: u32) -> u8 {
    let delta = b as i32 - a as i32;
    (a as i32 + delta * t as i32 / 100) as u8
}

/// Blend two colors: `t` is the weight of `b`, in percent (0 = all `a`,
/// 100 = all `b`, 50 = exact midpoint). Values outside 0..=100 clamp.
pub const fn color_mix(a: COLORREF, b: COLORREF, t: u32) -> COLORREF {
    let t = if t > 100 { 100 } else { t };
    rgb(
        mix_channel(r_of(a), r_of(b), t),
        mix_channel(g_of(a), g_of(b), t),
        mix_channel(b_of(a), b_of(b), t),
    )
}

/// Lift `c` by `t` percent toward white (0 = unchanged, 100 = white).
pub const fn lighten(c: COLORREF, t: u32) -> COLORREF {
    color_mix(c, WHITE, t)
}

// --- Derived colors ---------------------------------------------------------
//
// Row tints are computed from the core palette with the helpers above, so they
// track the anchors automatically instead of being a second set of literals.

/// Alternating list row: [`PANEL`] lifted 3% toward white — a quiet zebra tint.
pub const ROW_ALT: COLORREF = lighten(PANEL, 3);
/// Hover fill for rail buttons and list rows: [`PANEL`] lifted 7% toward white.
pub const ROW_HOVER: COLORREF = lighten(PANEL, 7);
/// Selected list row: [`PANEL`] pulled 35% toward [`ACCENT`] — an accent-tinted
/// dark selection that stays readable behind `TEXT`.
pub const ROW_SELECT: COLORREF = color_mix(PANEL, ACCENT, 35);
/// Active-section fill: brighter than hover so the current pane stands out.
pub const ACTIVE_BG: COLORREF = rgb(0x37, 0x37, 0x3A);
/// Text drawn on accent-filled surfaces (primary button, accent bar).
pub const ON_ACCENT: COLORREF = rgb(0xFF, 0xFF, 0xFF);

// Row-tint back-compat aliases (pre-step-2 names), mirroring the canonical
// derived constants above so ui.rs call sites keep compiling.

/// Alias for [`ROW_ALT`] (pre-step-2 name).
pub const ROW_ALT_BG: COLORREF = ROW_ALT;
/// Alias for [`ROW_HOVER`] (pre-step-2 name).
pub const HOVER_BG: COLORREF = ROW_HOVER;
/// Alias for [`ROW_SELECT`] (pre-step-2 name).
pub const SELECTION_BG: COLORREF = ROW_SELECT;

// --- Spacing & DPI ----------------------------------------------------------

/// Base grid unit in 96-DPI pixels (8px). Scale with [`scale_px`].
pub const GRID: i32 = 8;

/// Scale a 96-DPI layout value to `dpi`, rounding like GDI's `MulDiv`
/// (`(px * dpi + 48) / 96` with truncating division). `scale_px(GRID, dpi)`
/// gives the DPI-aware grid step; at 100% it is identity, at 150% ×1.5,
/// at 200% ×2.
pub const fn scale_px(px: i32, dpi: u32) -> i32 {
    ((px as i64 * dpi as i64 + 48) / 96) as i32
}

/// Alias for [`scale_px`] (pre-step-2 name).
pub const fn scale(px: i32, dpi: u32) -> i32 {
    scale_px(px, dpi)
}

// --- Typography -------------------------------------------------------------

/// Base UI font point size: Segoe UI 9pt.
pub const UI_FONT_PT: i32 = 9;
/// Semibold UI font point size: Segoe UI 10pt (section headers, emphasis).
pub const UI_FONT_SEMIBOLD_PT: i32 = 10;
/// `lfWeight` for the regular (book) Segoe UI face.
pub const FONT_WEIGHT_NORMAL: u32 = 400;
/// `lfWeight` for the semibold Segoe UI face.
pub const FONT_WEIGHT_SEMIBOLD: u32 = 600;

/// GDI `lfHeight` for `point_size` at `dpi`: the negative of
/// `MulDiv(point_size, dpi, 72)`. The negation is what tells `CreateFontW` to
/// interpret the height as *character* height (ascender→descender) rather than
/// the full cell, which is the value that makes a point size render at its
/// nominal size. At 96 DPI, 9pt → −12 and 10pt → −13, matching Segoe UI.
pub const fn font_height_pt(point_size: i32, dpi: u32) -> i32 {
    -((point_size as i64 * dpi as i64 + 36) / 72) as i32
}

/// Process-lifetime cache of created fonts, keyed by
/// `(point_size, weight, dpi)`. GDI font objects are scarce, so every call for
/// the same key returns the same [`HFONT`]; the handles are released by
/// [`delete_cached_fonts`] at hub teardown.
///
/// The cache lives in a `static`, so the handle is wrapped in [`FontHandle`]:
/// `HFONT` itself is a raw pointer (not `Send`/`Sync`), but a font id is just
/// an opaque GDI object number that is never dereferenced here — only created,
/// stored, and deleted — so sharing it across threads under the mutex is
/// sound.
static FONT_CACHE: Mutex<Option<HashMap<(i32, u32, u32), FontHandle>>> = Mutex::new(None);

/// `HFONT` newtype with explicit `Send`/`Sync`: lets [`FONT_CACHE`] be a
/// `static`. `GDI` font objects are not bound to the creating thread and the
/// cache only stores the id, so the impls are sound.
#[derive(Clone, Copy)]
struct FontHandle(HFONT);

// SAFETY: FontHandle is a GDI object id, never dereferenced. All creation and
// destruction happens while holding FONT_CACHE's mutex, so the underlying
// object outlives every handle value handed out.
unsafe impl Send for FontHandle {}
unsafe impl Sync for FontHandle {}

/// Create the Segoe UI font for `point_size` / `weight` at `dpi`, or return the
/// cached handle if one already exists. On `CreateFontW` failure the error is
/// logged with `tracing` and an invalid (null) [`HFONT`] is returned — callers
/// fall back to the system default GUI font rather than panicking.
pub fn font(point_size: i32, weight: u32, dpi: u32) -> HFONT {
    let key = (point_size, weight, dpi);
    let mut cache = match FONT_CACHE.lock() {
        Ok(guard) => guard,
        // A poisoned mutex means another thread panicked mid-insert; the cached
        // handles themselves are still valid, so keep going with the data.
        Err(poisoned) => poisoned.into_inner(),
    };
    let map = cache.get_or_insert_with(HashMap::new);
    if let Some(FontHandle(h)) = map.get(&key) {
        return *h;
    }

    let h = unsafe {
        CreateFontW(
            font_height_pt(point_size, dpi), // height: negative = char height
            0,                               // average width: auto
            0,                               // escapement
            0,                               // orientation
            weight as i32,                   // FW_NORMAL / FW_SEMIBOLD
            0,                               // not italic
            0,                               // no underline
            0,                               // no strikeout
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0, // DEFAULT_PITCH | FF_DONTCARE
            w!("Segoe UI"),
        )
    };

    if h.is_invalid() {
        tracing::error!(
            point_size,
            weight,
            dpi,
            "CreateFontW failed; callers fall back to the default GUI font"
        );
        return h;
    }
    map.insert(key, FontHandle(h));
    h
}

/// Cached Segoe UI 9pt font for `dpi` — the base face for every hub control.
pub fn ui_font(dpi: u32) -> HFONT {
    font(UI_FONT_PT, FONT_WEIGHT_NORMAL, dpi)
}

/// Cached Segoe UI 10pt semibold font for `dpi` — section headers and emphasis.
pub fn ui_font_semibold(dpi: u32) -> HFONT {
    font(UI_FONT_SEMIBOLD_PT, FONT_WEIGHT_SEMIBOLD, dpi)
}

/// Delete every cached font handle (hub teardown). Returns the number of
/// handles freed; subsequent [`ui_font`] / [`ui_font_semibold`] calls recreate
/// them.
pub fn delete_cached_fonts() -> usize {
    let mut cache = match FONT_CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(map) = cache.as_mut() else {
        return 0;
    };
    let count = map.len();
    for (_, FontHandle(h)) in map.drain() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ::from(h));
        }
    }
    count
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
    fn palette_matches_the_dark_theme_spec() {
        assert_eq!(BG.0, 0x001E1E1E); // #1E1E1E
        assert_eq!(PANEL.0, 0x00262525); // #252526
        assert_eq!(BORDER.0, 0x00302D2D); // #2D2D30
        assert_eq!(TEXT.0, 0x00D4D4D4); // #D4D4D4
        assert_eq!(MUTED.0, 0x00808080); // #808080
        assert_eq!(ACCENT.0, 0x00D47800); // #0078D4
    }

    #[test]
    fn back_compat_aliases_track_the_canonical_names() {
        assert_eq!(PANEL_BG, PANEL);
        assert_eq!(MUTED_TEXT, MUTED);
        assert_eq!(ROW_ALT_BG, ROW_ALT);
        assert_eq!(HOVER_BG, ROW_HOVER);
        assert_eq!(SELECTION_BG, ROW_SELECT);
        assert_eq!(scale(GRID, 144), scale_px(GRID, 144));
    }

    #[test]
    fn color_mix_interpolates_channels_and_clamps() {
        // Endpoints are exact.
        assert_eq!(color_mix(BG, TEXT, 0), BG);
        assert_eq!(color_mix(BG, TEXT, 100), TEXT);
        // Midpoint of #1E1E1E (30) and #D4D4D4 (212) is 121 = 0x79 per channel.
        assert_eq!(color_mix(BG, TEXT, 50).0, 0x00797979);
        // Out-of-range weights clamp rather than wrap.
        assert_eq!(color_mix(BG, TEXT, 250), TEXT);
        // Pulling PANEL toward ACCENT deepens blue and drops red.
        let mixed = color_mix(PANEL, ACCENT, 35);
        assert_eq!(mixed.0, 0x00624219); // r 0x19, g 0x42, b 0x62
        assert!(b_of(mixed) > b_of(PANEL));
        assert!(r_of(mixed) < r_of(PANEL));
    }

    #[test]
    fn lighten_moves_a_color_toward_white() {
        assert_eq!(lighten(BG, 0), BG);
        assert_eq!(lighten(BG, 100), WHITE);
        // #1E1E1E (30) lifted 25% toward 255: 30 + 225*25/100 = 86 = 0x56.
        assert_eq!(lighten(BG, 25).0, 0x00565656);
        // Derived row tints stay strictly inside the panel→white band.
        assert!(r_of(ROW_ALT) > r_of(PANEL));
        assert!(r_of(ROW_HOVER) > r_of(ROW_ALT));
        assert!(r_of(ROW_HOVER) < r_of(WHITE));
    }

    #[test]
    fn derived_row_colors_are_palette_consistent() {
        // Selection is accent-tinted: bluer than the panel, darker than white.
        assert!(b_of(ROW_SELECT) > b_of(PANEL));
        assert!(r_of(ROW_SELECT) < r_of(PANEL));
        // Zebra and hover are visibly distinct from each other and the panel.
        assert_ne!(ROW_ALT, PANEL);
        assert_ne!(ROW_HOVER, ROW_ALT);
        assert_ne!(ROW_SELECT, ACCENT);
    }

    #[test]
    fn scale_px_is_identity_at_96_dpi_and_scales_with_dpi() {
        assert_eq!(scale_px(8, 96), 8);
        assert_eq!(scale_px(8, 144), 12);
        assert_eq!(scale_px(8, 192), 16);
        assert_eq!(scale_px(8, 120), 10);
        assert_eq!(scale_px(0, 96), 0);
        assert_eq!(scale_px(GRID, 96), GRID);
    }

    #[test]
    fn font_height_matches_negated_muldiv() {
        // -MulDiv(pt, dpi, 72), the standard lfHeight for CreateFontW.
        assert_eq!(font_height_pt(9, 96), -12); // Segoe UI 9pt @100%
        assert_eq!(font_height_pt(10, 96), -13); // Segoe UI 10pt @100%
        assert_eq!(font_height_pt(9, 144), -18); // 9pt @150%
        assert_eq!(font_height_pt(9, 192), -24); // 9pt @200%
        assert_eq!(font_height_pt(0, 96), 0);
        // Height is always negative for real point sizes: char height, not cell.
        assert!(font_height_pt(UI_FONT_PT, 96) < 0);
        assert!(font_height_pt(UI_FONT_SEMIBOLD_PT, 96) < 0);
    }

    #[test]
    fn ui_font_factories_cache_and_return_valid_handles() {
        let base = ui_font(96);
        assert!(!base.is_invalid());
        // Same key → same cached handle.
        assert_eq!(base, ui_font(96));
        // Semibold 10pt is a distinct face from regular 9pt.
        let semi = ui_font_semibold(96);
        assert!(!semi.is_invalid());
        assert_ne!(base, semi);
        // Different DPI yields a distinct handle.
        assert_ne!(base, ui_font(144));
        // The general factory shares the cache with the named ones.
        assert_eq!(ui_font(96), font(UI_FONT_PT, FONT_WEIGHT_NORMAL, 96));
    }
}
