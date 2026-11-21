//! The Hyper client's stats overlay (Ctrl+Shift+S): a few lines of text drawn
//! with GDI into a small RGBA panel, put into the corner of each frame.
//! Rendered only when the numbers change (once a second). A picture shown 1:1
//! gets the panel copied over it from a texture uploaded once per change; a
//! scaled one gets it uploaded into the framebuffer with each frame.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::*;

/// The text height at 100% scale with the picture shown 1:1, and the line
/// height and margin that go with it (they grow with the text).
const TEXT_PX: i32 = 18;
const LINE_H: i32 = 22;
const PAD: i32 = 8;

/// What the panels are shown in: the picture's width (its pixels are the
/// panels' pixels), and the session window's client width and DPI. 0: not
/// known yet. Set by the session ([`set_picture_width`]) and the window
/// ([`set_view`]).
static PICTURE_W: AtomicU32 = AtomicU32::new(0);
static VIEW_W: AtomicU32 = AtomicU32::new(0);
static VIEW_DPI: AtomicU32 = AtomicU32::new(0);

/// The picture the panels are drawn over is `width` pixels across.
pub fn set_picture_width(width: u32) {
    PICTURE_W.store(width, Ordering::Relaxed);
}

/// The session window is `width` pixels across at `dpi` (96 = 100%).
pub fn set_view(width: u32, dpi: u32) {
    VIEW_W.store(width, Ordering::Relaxed);
    VIEW_DPI.store(dpi, Ordering::Relaxed);
}

/// The text height in picture pixels that shows as normal UI text on this
/// screen: larger on a high-DPI screen, and in proportion when the picture
/// is scaled up or down to the window. Unknowns count as 100% and 1:1.
fn text_px_for(dpi: u32, picture_w: u32, view_w: u32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi } as u64;
    let (pic, view) = if picture_w == 0 || view_w == 0 { (1, 1) } else { (picture_w as u64, view_w as u64) };
    ((TEXT_PX as u64 * dpi * pic) / (96 * view)).clamp(12, 54) as i32
}

/// A rendered panel and a number no other panel of this process has, so a
/// renderer that holds a copy of it can tell when it has to upload again.
pub struct Panel {
    pub w: u16,
    pub h: u16,
    pub px: Vec<u8>,
    pub id: u64,
}

/// [`render`] as a [`Panel`].
pub fn panel(lines: &[String]) -> Option<Panel> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let (w, h, px) = render(lines)?;
    Some(Panel { w, h, px, id: NEXT.fetch_add(1, Ordering::Relaxed) })
}

/// Draw `lines` into a top-down RGBA panel as wide as the longest one:
/// `(width, height, pixels)`. The text is sized for the screen (see
/// [`text_px_for`]).
pub fn render(lines: &[String]) -> Option<(u16, u16, Vec<u8>)> {
    let px = text_px_for(VIEW_DPI.load(Ordering::Relaxed), PICTURE_W.load(Ordering::Relaxed), VIEW_W.load(Ordering::Relaxed));
    render_at(lines, px)
}

fn render_at(lines: &[String], text_px: i32) -> Option<(u16, u16, Vec<u8>)> {
    let (line_h, pad) = (LINE_H * text_px / TEXT_PX, PAD * text_px / TEXT_PX);
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontW(
            text_px,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (FF_MODERN.0 | FIXED_PITCH.0) as u32,
            w!("Consolas"),
        );
        let old_font = SelectObject(dc, font.into());
        let widest = lines
            .iter()
            .map(|line| {
                let wide: Vec<u16> = line.encode_utf16().collect();
                let mut size = windows::Win32::Foundation::SIZE::default();
                let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
                size.cx
            })
            .max()
            .unwrap_or(0);
        let (w, h) = ((widest + pad * 2).clamp(64, 4096), pad * 2 + line_h * lines.len() as i32);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            SelectObject(dc, old_font);
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(dc);
            return None;
        };
        let old_bmp = SelectObject(dc, bmp.into());
        let brush = CreateSolidBrush(COLORREF(0x0018_1818));
        FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, brush);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00F0_F0F0));
        for (i, line) in lines.iter().enumerate() {
            let wide: Vec<u16> = line.encode_utf16().collect();
            let _ = TextOutW(dc, pad, pad + i as i32 * line_h, &wide);
        }
        let _ = GdiFlush();
        let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        let mut rgba = Vec::with_capacity(src.len());
        for px in src.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        SelectObject(dc, old_font);
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        Some((w as u16, h as u16, rgba))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_visible_text_on_a_dark_panel() {
        let (w, h, px) = super::render(&["120.0 fps   latency 9.1 ms".into(), "rtt 1.2 ms".into()]).unwrap();
        assert_eq!(px.len(), w as usize * h as usize * 4);
        let bright = px.chunks_exact(4).filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200).count();
        let dark = px.chunks_exact(4).filter(|p| p[0] < 40).count();
        assert!(bright > 200, "text pixels: {bright}");
        assert!(dark > bright, "mostly background");
    }

    #[test]
    fn text_is_sized_for_the_screen_not_the_stream() {
        use super::text_px_for;
        // Nothing known yet, or 100% at 1:1: as before.
        assert_eq!(text_px_for(0, 0, 0), 18);
        assert_eq!(text_px_for(96, 1920, 1920), 18);
        // A 200% laptop screen, 1:1: twice the pixels.
        assert_eq!(text_px_for(192, 2880, 2880), 36);
        // The same screen with a half-size stream: half as many picture
        // pixels, so the same size on screen.
        assert_eq!(text_px_for(192, 1440, 2880), 18);
        // Bounded both ways.
        assert_eq!(text_px_for(96, 640, 2880), 12);
        assert_eq!(text_px_for(288, 3840, 1280), 54);
    }

    #[test]
    fn bigger_text_makes_a_bigger_panel() {
        let lines = ["Ctrl+Shift+Q   quit".to_string()];
        let (w1, h1, _) = super::render_at(&lines, 18).unwrap();
        let (w2, h2, _) = super::render_at(&lines, 36).unwrap();
        assert_eq!(h1, 8 * 2 + 22);
        assert_eq!(h2, 16 * 2 + 44);
        assert!(w2 > w1 * 3 / 2, "{w1} -> {w2}");
    }
}
