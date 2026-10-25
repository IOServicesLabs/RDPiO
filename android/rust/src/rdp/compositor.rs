//! The Cloud PC's desktop, assembled from its EGFX commands (MS-RDPEGFX) in
//! one RGBA framebuffer, and shown on the app's Surface.
//!
//! The same command semantics as the desktop client's renderer (its CPU
//! shadow): surfaces mapped onto the desktop, solid fills, copies and the
//! surface cache read and write the framebuffer; ClearCodec (seeded from it),
//! RemoteFX Progressive and uncompressed tiles decode into it; H.264 (AVC420)
//! decodes in hardware ([`AvcDecoder`]) and only its changed rectangles are
//! converted in. Showing it copies what each of the Surface's buffers is
//! missing into it ([`Damage`]).

use std::collections::HashMap;
use std::time::Duration;

use ndk::hardware_buffer_format::HardwareBufferFormat;
use ndk::native_window::NativeWindow;
use rdp_graphics::clearcodec::ClearDecoder;
use rdp_graphics::progressive::ProgressiveDecoder;
use rdp_graphics::surface::SurfaceTable;
use rdp_pdu::gfx::{GfxCommand, CODECID_AVC420, CODECID_CAVIDEO, CODECID_CLEARCODEC, CODECID_UNCOMPRESSED};

use crate::rdp::avc::{self, AvcDecoder};
use crate::rdp::damage::Damage;

/// How long a picture may take to come out of the decoder.
const DECODE_WAIT: Duration = Duration::from_millis(80);

pub struct Compositor {
    fb: Vec<u8>,
    width: u32,
    height: u32,
    /// What changed, per Surface buffer.
    damage: Damage,
    surfaces: SurfaceTable,
    /// Global to the channel (its glyph caches outlive surfaces and resets).
    clear: ClearDecoder,
    progressive: HashMap<u16, ProgressiveDecoder>,
    h264: HashMap<u16, AvcDecoder>,
    /// H.264 that couldn't be decoded (no decoder): warned once.
    h264_failed: bool,
    cache: HashMap<u16, (u32, u32, Vec<u8>)>,
    /// The window size last set on the Surface (re-set when either changes).
    window_size: Option<(u32, u32, usize)>,
    pub frames: u32,
    /// Bytes copied onto the Surface (for the stats line).
    pub copied: u64,
}

impl Compositor {
    pub fn new(width: u32, height: u32) -> Self {
        let mut damage = Damage::default();
        damage.reset(width, height);
        Self {
            fb: vec![0; width as usize * height as usize * 4],
            width,
            height,
            damage,
            surfaces: SurfaceTable::new(),
            clear: ClearDecoder::new(),
            progressive: HashMap::new(),
            h264: HashMap::new(),
            h264_failed: false,
            cache: HashMap::new(),
            window_size: None,
            frames: 0,
            copied: 0,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The desktop changed size (cleared to black).
    pub fn resize(&mut self, width: u32, height: u32) {
        if (width, height) != (self.width, self.height) {
            self.fb = vec![0; width as usize * height as usize * 4];
            (self.width, self.height) = (width, height);
        }
        self.damage.reset(width, height);
    }

    fn mark(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let (r, b) = ((x + w).min(self.width), (y + h).min(self.height));
        if x >= r || y >= b {
            return;
        }
        self.damage.add((x, y, r, b));
    }

    /// Copy a rectangle of the desktop to another place on it (a scroll), in
    /// place: row by row, in the order that never reads a row already written.
    fn copy_within(&mut self, sx: u32, sy: u32, w: u32, h: u32, dx: u32, dy: u32) {
        if sx >= self.width || sy >= self.height || dx >= self.width || dy >= self.height {
            return;
        }
        let cols = w.min(self.width - sx).min(self.width - dx) as usize;
        let rows = h.min(self.height - sy).min(self.height - dy) as usize;
        let stride = self.width as usize * 4;
        let row = |r: usize| {
            let src = (sy as usize + r) * stride + sx as usize * 4;
            (src..src + cols * 4, (dy as usize + r) * stride + dx as usize * 4)
        };
        if dy > sy {
            for r in (0..rows).rev() {
                let (src, dst) = row(r);
                self.fb.copy_within(src, dst);
            }
        } else {
            for r in 0..rows {
                let (src, dst) = row(r);
                self.fb.copy_within(src, dst);
            }
        }
        self.mark(dx, dy, cols as u32, rows as u32);
    }

    /// Write an RGBA rectangle (clipped to the desktop).
    pub fn put(&mut self, x: u32, y: u32, w: u32, h: u32, rgba: &[u8]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let cols = (self.width - x).min(w) as usize;
        let rows = (self.height - y).min(h) as usize;
        let stride = self.width as usize * 4;
        for row in 0..rows {
            let src = row * w as usize * 4;
            let dst = (y as usize + row) * stride + x as usize * 4;
            if let (Some(s), Some(d)) = (rgba.get(src..src + cols * 4), self.fb.get_mut(dst..dst + cols * 4)) {
                d.copy_from_slice(s);
            }
        }
        self.mark(x, y, w, h);
    }

    /// Read an RGBA rectangle (black outside the desktop).
    fn region(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; w as usize * h as usize * 4];
        if x >= self.width {
            return out;
        }
        let cols = (self.width - x).min(w) as usize;
        let stride = self.width as usize * 4;
        for row in 0..h.min(self.height.saturating_sub(y)) as usize {
            let src = (y as usize + row) * stride + x as usize * 4;
            let dst = row * w as usize * 4;
            out[dst..dst + cols * 4].copy_from_slice(&self.fb[src..src + cols * 4]);
        }
        out
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, px: [u8; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let cols = (self.width - x).min(w) as usize;
        let rows = (self.height - y).min(h) as usize;
        let stride = self.width as usize * 4;
        for row in 0..rows {
            let dst = (y as usize + row) * stride + x as usize * 4;
            for p in self.fb[dst..dst + cols * 4].chunks_exact_mut(4) {
                p.copy_from_slice(&px);
            }
        }
        self.mark(x, y, w, h);
    }

    /// Apply one EGFX command.
    pub fn apply(&mut self, command: &GfxCommand) {
        self.surfaces.apply(command);
        let origin = |s: &SurfaceTable, id: u16| s.output_origin(id).unwrap_or((0, 0));
        match command {
            GfxCommand::ResetGraphics { width, height, .. } => {
                // Stream-scoped decoders go; the ClearCodec and surface caches
                // stay (the server replays them after a reset).
                self.h264.clear();
                self.progressive.clear();
                self.resize(*width, *height);
            }
            GfxCommand::DeleteSurface { surface_id } => {
                self.h264.remove(surface_id);
                self.progressive.remove(surface_id);
            }
            GfxCommand::DeleteEncodingContext { surface_id, codec_context_id } => {
                if let Some(d) = self.progressive.get_mut(surface_id) {
                    d.delete_context(*codec_context_id);
                }
            }
            GfxCommand::EndFrame { .. } => self.frames += 1,
            GfxCommand::SolidFill { surface_id, color, rects } => {
                let (ox, oy) = origin(&self.surfaces, *surface_id);
                let px = [(*color >> 16) as u8, (*color >> 8) as u8, *color as u8, 0xFF];
                for r in rects {
                    let (w, h) = (r.right.saturating_sub(r.left) as u32, r.bottom.saturating_sub(r.top) as u32);
                    self.fill(ox + r.left as u32, oy + r.top as u32, w, h, px);
                }
            }
            GfxCommand::SurfaceToSurface { surface_src, surface_dst, rect_src, dest_pts } => {
                let (sox, soy) = origin(&self.surfaces, *surface_src);
                let (dox, doy) = origin(&self.surfaces, *surface_dst);
                let (w, h) = (rect_src.right.saturating_sub(rect_src.left) as u32, rect_src.bottom.saturating_sub(rect_src.top) as u32);
                let (sx, sy) = (sox + rect_src.left as u32, soy + rect_src.top as u32);
                if w > 0 && h > 0 && dest_pts.len() == 1 {
                    // (A scroll: no copy of the whole area first.)
                    self.copy_within(sx, sy, w, h, dox + dest_pts[0].x as u32, doy + dest_pts[0].y as u32);
                } else if w > 0 && h > 0 {
                    let src = self.region(sx, sy, w, h);
                    for p in dest_pts {
                        self.put(dox + p.x as u32, doy + p.y as u32, w, h, &src);
                    }
                }
            }
            GfxCommand::SurfaceToCache { surface_id, slot, rect_src, .. } => {
                let (ox, oy) = origin(&self.surfaces, *surface_id);
                let (w, h) = (rect_src.right.saturating_sub(rect_src.left) as u32, rect_src.bottom.saturating_sub(rect_src.top) as u32);
                if w > 0 && h > 0 {
                    let px = self.region(ox + rect_src.left as u32, oy + rect_src.top as u32, w, h);
                    self.cache.insert(*slot, (w, h, px));
                }
            }
            GfxCommand::CacheToSurface { slot, surface_id, dest_pts } => {
                let (ox, oy) = origin(&self.surfaces, *surface_id);
                if let Some((w, h, px)) = self.cache.remove(slot) {
                    for p in dest_pts {
                        self.put(ox + p.x as u32, oy + p.y as u32, w, h, &px);
                    }
                    self.cache.insert(*slot, (w, h, px));
                }
            }
            GfxCommand::WireToSurface2 { surface_id, codec_context_id, bitmap, .. } => {
                let (ox, oy) = origin(&self.surfaces, *surface_id);
                let mut dec = self.progressive.remove(surface_id).unwrap_or_default();
                for t in dec.decode(*codec_context_id, bitmap) {
                    self.put(ox + t.x, oy + t.y, t.w, t.h, &t.rgba);
                    dec.return_buffer(t.rgba);
                }
                self.progressive.insert(*surface_id, dec);
            }
            GfxCommand::WireToSurface1 { surface_id, codec_id, dest, bitmap, .. } => {
                let (ox, oy) = origin(&self.surfaces, *surface_id);
                let (x, y) = (ox + dest.left as u32, oy + dest.top as u32);
                let (w, h) = (dest.right.saturating_sub(dest.left), dest.bottom.saturating_sub(dest.top));
                match *codec_id {
                    CODECID_AVC420 | CODECID_CAVIDEO => self.avc420(*surface_id, (ox, oy), bitmap),
                    CODECID_UNCOMPRESSED => {
                        if let Some(rgba) = rdp_graphics::bitmap::to_rgba(bitmap, w, h, 32, false) {
                            self.put(x, y, w as u32, h as u32, &rgba);
                        }
                    }
                    CODECID_CLEARCODEC => {
                        // A persistent-surface codec: what it doesn't re-code
                        // keeps the pixels already there.
                        let seed = rdp_graphics::clearcodec::needs_seed(bitmap, w, h).then(|| self.region(x, y, w as u32, h as u32));
                        if let Some(rgba) = self.clear.decode_seeded(bitmap, w, h, seed.as_deref()) {
                            self.put(x, y, w as u32, h as u32, &rgba);
                        }
                    }
                    other => log::debug!("EGFX codec 0x{other:04x} not handled"),
                }
            }
            _ => {}
        }
    }

    fn avc420(&mut self, surface_id: u16, (ox, oy): (u32, u32), data: &[u8]) {
        let Some(stream) = rdp_graphics::avc::parse_avc420(data) else { return };
        if !self.h264.contains_key(&surface_id) {
            let (w, h) = self.surfaces.get(surface_id).map_or((self.width, self.height), |s| (s.width.max(1) as u32, s.height.max(1) as u32));
            match AvcDecoder::new(w, h) {
                Ok(d) => {
                    self.h264.insert(surface_id, d);
                }
                Err(e) => {
                    if !std::mem::replace(&mut self.h264_failed, true) {
                        log::warn!("no H.264 for the desktop: {e}");
                    }
                    return;
                }
            }
        }
        let rects = stream.rects;
        let (fw, fh) = (self.width, self.height);
        let stride = fw as usize * 4;
        let mut painted = Vec::new();
        let dec = self.h264.get_mut(&surface_id).expect("inserted above");
        let fb = &mut self.fb;
        let result = dec.decode(stream.h264, DECODE_WAIT, |pic| {
            let whole = [rdp_graphics::avc::Rect { left: 0, top: 0, right: pic.width as u16, bottom: pic.height as u16 }];
            for r in if rects.is_empty() { &whole[..] } else { &rects[..] } {
                let (l, t) = (r.left as usize, r.top as usize);
                let (dx, dy) = (ox as usize + l, oy as usize + t);
                if dx >= fw as usize || dy >= fh as usize {
                    continue;
                }
                let w = (r.right.saturating_sub(r.left) as usize).min(fw as usize - dx);
                let h = (r.bottom.saturating_sub(r.top) as usize).min(fh as usize - dy);
                avc::convert(pic, l, t, w, h, &mut fb[dy * stride + dx * 4..], stride);
                painted.push((dx as u32, dy as u32, w as u32, h as u32));
            }
        });
        if let Err(e) = result {
            log::warn!("desktop H.264: {e}; starting it again");
            self.h264.remove(&surface_id);
        }
        for (x, y, w, h) in painted {
            self.mark(x, y, w, h);
        }
    }

    /// Copy what changed onto the Surface's next buffer.
    pub fn show(&mut self, window: &NativeWindow) {
        if self.damage.is_empty() {
            return;
        }
        let key = (self.width, self.height, window.ptr().as_ptr() as usize);
        if self.window_size != Some(key) {
            if let Err(e) = window.set_buffers_geometry(self.width as i32, self.height as i32, Some(HardwareBufferFormat::R8G8B8A8_UNORM)) {
                log::warn!("desktop surface geometry: {e}");
            }
            self.window_size = Some(key);
            // A new buffer chain: all of it, in every buffer.
            self.damage.reset(self.width, self.height);
        }
        if self.damage.broken {
            // (Buffers that move in memory.) Android's way: one rectangle,
            // the rest copied back by Android.
            if let Some(b) = self.damage.take_bounds() {
                self.show_rects(window, b, |_| None);
            }
            return;
        }
        let mut damage = std::mem::take(&mut self.damage);
        self.show_rects(window, (0, 0, self.width, self.height), |address| Some(damage.for_buffer(address)));
        if damage.broken {
            log::warn!("desktop surface: its buffers move in memory; back to one changed rectangle a frame");
        }
        // (show_rects may have reset it: the Surface wasn't ready.)
        if !self.damage.is_empty() {
            damage.reset(self.width, self.height);
        }
        self.damage = damage;
    }

    /// Lock `bounds` of the Surface's next buffer and paint into it the
    /// rectangles `which` gives for its address (None: all of `bounds`).
    fn show_rects(&mut self, window: &NativeWindow, (l, t, r, b): (u32, u32, u32, u32), which: impl FnOnce(usize) -> Option<Vec<(u32, u32, u32, u32)>>) {
        let mut bounds = ndk::native_window::Rect { left: l as i32, top: t as i32, right: r as i32, bottom: b as i32 };
        let Ok(mut lock) = window.lock(Some(&mut bounds)) else { return };
        let (bw, bh, bstride) = (lock.width(), lock.height(), lock.stride() * 4);
        if (bw, bh) != (self.width as usize, self.height as usize) {
            // (The geometry hasn't taken yet.)
            self.window_size = None;
            self.damage.reset(self.width, self.height);
            return;
        }
        // Android may hand back a larger area to fill (no copy of the
        // previous buffer for it).
        let l = (bounds.left.max(0) as usize).min(bw);
        let t = (bounds.top.max(0) as usize).min(bh);
        let r = (bounds.right.max(0) as usize).min(bw);
        let b = (bounds.bottom.max(0) as usize).min(bh);
        let Some(bytes) = lock.bytes() else { return };
        let rects = which(bytes.as_ptr() as usize).unwrap_or_else(|| vec![(l as u32, t as u32, r as u32, b as u32)]);
        let stride = self.width as usize * 4;
        for &(rl, rt, rr, rb) in &rects {
            let (rl, rr) = ((rl as usize).min(bw), (rr as usize).min(bw));
            for y in (rt as usize).min(bh)..(rb as usize).min(bh) {
                let src = &self.fb[y * stride + rl * 4..y * stride + rr * 4];
                let dst = &mut bytes[y * bstride + rl * 4..y * bstride + rr * 4];
                // SAFETY: MaybeUninit<u8> has u8's layout; every byte is written.
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr() as *mut u8, src.len()) };
                self.copied += src.len() as u64;
            }
        }
    }

    /// A slow-path bitmap update (before or without EGFX).
    pub fn bitmap(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        self.put(x as u32, y as u32, w as u32, h as u32, rgba);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdp_pdu::gfx::{Point16, Rect16};

    #[test]
    fn fills_copies_and_caches_like_the_desktop() {
        let mut c = Compositor::new(8, 4);
        c.damage.take_bounds();
        c.apply(&GfxCommand::CreateSurface { surface_id: 1, width: 8, height: 4, pixel_format: 0x20 });
        c.apply(&GfxCommand::MapSurfaceToOutput { surface_id: 1, x: 0, y: 0 });
        c.apply(&GfxCommand::SolidFill { surface_id: 1, color: 0x00FF_0000, rects: vec![Rect16 { left: 0, top: 0, right: 2, bottom: 2 }] });
        assert_eq!(&c.fb[..4], &[0xFF, 0, 0, 0xFF]);
        c.apply(&GfxCommand::SurfaceToSurface {
            surface_src: 1,
            surface_dst: 1,
            rect_src: Rect16 { left: 0, top: 0, right: 2, bottom: 2 },
            dest_pts: vec![Point16 { x: 4, y: 2 }],
        });
        let at = |x: usize, y: usize| &c.fb[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
        assert_eq!(at(5, 3), &[0xFF, 0, 0, 0xFF]);
        assert_eq!(at(3, 3), &[0, 0, 0, 0]);
        assert_eq!(c.damage.take_bounds(), Some((0, 0, 6, 4)));
    }
}
