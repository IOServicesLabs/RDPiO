//! The Hyper client's stats overlay (Ctrl+Shift+S): a few lines of text drawn
//! with GDI into a small RGBA panel, uploaded into the corner of each frame.
//! Rendered only when the numbers change (once a second), so it costs a tiny
//! texture upload per frame while shown and nothing while hidden.

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::*;

const LINE_H: i32 = 22;
const PAD: i32 = 8;

/// Draw `lines` into a top-down RGBA panel as wide as the longest one:
/// `(width, height, pixels)`.
pub fn render(lines: &[String]) -> Option<(u16, u16, Vec<u8>)> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontW(
            18,
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
        let (w, h) = ((widest + PAD * 2).clamp(64, 1600), PAD * 2 + LINE_H * lines.len() as i32);
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
            let _ = TextOutW(dc, PAD, PAD + i as i32 * LINE_H, &wide);
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
}
