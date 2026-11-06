//! Teams video for Microsoft's add-in (`--teams`): the host's half of drawing it.
//!
//! The add-in decodes the call's video and captures the camera itself, but draws
//! nothing: for each video element it asks the host for a renderer
//! (`IWTSBitmapRenderService::GetMappedRenderer`, by MS-RDPEGT mapping id), waits
//! to hear the element's size (`IWTSBitmapRendererCallback::OnTargetSizeChanged`),
//! fits each frame to it, and hands it over (`IWTSBitmapRenderer::Render`). Without
//! a renderer it drops every frame: the call's audio works, and no video shows.
//!
//! Here a frame becomes an [`Overlay`] at the desktop rectangle the server tracks
//! for that mapping over the geometry channel, as [`crate::teams_video`] draws the
//! native engine's.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rdp_channels::geometry::{GeoRect, GeometryUpdate, MappedGeometry};
use windows::core::{implement, Ref, Result, GUID};
use windows::Win32::Foundation::{E_POINTER, RECT};
use windows::Win32::System::RemoteDesktop::{
    IWTSBitmapRenderService, IWTSBitmapRenderService_Impl, IWTSBitmapRenderer, IWTSBitmapRendererCallback,
    IWTSBitmapRenderer_Impl, BITMAP_RENDERER_STATISTICS,
};

use crate::teams_video::{Overlay, OverlayOut, Picture};

/// Largest element we will draw (guards against a bogus geometry).
const MAX_SIDE: i32 = 8192;

const FORMAT_RGB32: u32 = 0x0000_0016;
const FORMAT_ARGB32: u32 = 0x0000_0015;
const FORMAT_NV12: u32 = 0x3231_564E;
const FORMAT_I420: u32 = 0x3032_3449;
/// `MFVideoFormat_*` are all this GUID with the FOURCC in `data1`.
const MF_FORMAT_BASE: u128 = 0x0000_0000_0000_0010_8000_00AA_0038_9B71;

/// A callback the add-in gave us, only ever called on the plugin's host thread.
#[derive(Clone)]
struct Callback(IWTSBitmapRendererCallback);

// SAFETY: it's kept here from whichever thread asked for the renderer, and only
// called on the host thread, the one the plugin itself runs on.
unsafe impl Send for Callback {}

#[derive(Default)]
struct State {
    /// Mapping id → its latest geometry.
    geometry: HashMap<u64, MappedGeometry>,
    callbacks: HashMap<u64, Callback>,
    /// Mappings that got a renderer and haven't been told their size yet.
    unsized_mappings: Vec<u64>,
    /// Where each mapping's video was last drawn, to erase when it moves away.
    drawn: HashMap<u64, GeoRect>,
}

/// The renderers' shared state. Frames come on the add-in's own threads.
pub struct MappedVideo {
    state: Mutex<State>,
    out: OverlayOut,
    delivered: AtomicU32,
    dropped: AtomicU32,
}

impl MappedVideo {
    pub fn new(out: OverlayOut) -> Arc<Self> {
        Arc::new(Self { state: Mutex::default(), out, delivered: AtomicU32::new(0), dropped: AtomicU32::new(0) })
    }

    /// Apply one MS-RDPEGT update. On the host thread.
    pub fn geometry(&self, update: GeometryUpdate) {
        let Ok(mut s) = self.state.lock() else { return };
        let (resized, moved_from) = match update {
            GeometryUpdate::Update(g) => {
                let id = g.mapping_id;
                let rect = g.desktop_rect();
                let size = |r: &GeoRect| (r.width(), r.height());
                let old = s.geometry.insert(id, g);
                if old.is_none() {
                    tracing::info!(mapping_id = id, ?rect, "Teams video: new geometry mapping");
                }
                let resized = old.is_none_or(|o| size(&o.desktop_rect()) != size(&rect));
                let moved = s.drawn.get(&id).copied().filter(|r| *r != rect);
                if moved.is_some() {
                    s.drawn.remove(&id);
                }
                (resized.then_some(id), moved)
            }
            GeometryUpdate::Clear(id) => {
                s.geometry.remove(&id);
                (Some(id), s.drawn.remove(&id))
            }
        };
        drop(s);
        if let Some(r) = moved_from {
            erase(&r);
        }
        if let Some(id) = resized {
            self.tell_size(id);
        }
    }

    /// Tell renderers their size for the first time. On the host thread, never
    /// inside `GetMappedRenderer`: the add-in may be holding its own locks there.
    pub fn deliver_sizes(&self) {
        let pending = match self.state.lock() {
            Ok(mut s) if !s.unsized_mappings.is_empty() => std::mem::take(&mut s.unsized_mappings),
            _ => return,
        };
        for id in pending {
            self.tell_size(id);
        }
    }

    fn tell_size(&self, id: u64) {
        let (callback, size) = {
            let Ok(s) = self.state.lock() else { return };
            let Some(callback) = s.callbacks.get(&id).cloned() else { return };
            let rect = s.geometry.get(&id).map(MappedGeometry::desktop_rect).unwrap_or_default();
            (callback, (rect.width(), rect.height()))
        };
        tracing::info!(mapping_id = id, ?size, "Teams video: target size");
        let rect = RECT { left: 0, top: 0, right: size.0, bottom: size.1 };
        if let Err(e) = unsafe { callback.0.OnTargetSizeChanged(rect) } {
            tracing::debug!(error = %e, "OnTargetSizeChanged failed");
        }
    }

    fn add_renderer(&self, id: u64, callback: Option<IWTSBitmapRendererCallback>) {
        tracing::info!(mapping_id = id, "Teams video: renderer asked for");
        let Ok(mut s) = self.state.lock() else { return };
        if let Some(callback) = callback {
            s.callbacks.insert(id, Callback(callback));
            s.unsized_mappings.push(id);
        }
    }

    fn remove_renderer(&self, id: u64) {
        tracing::info!(mapping_id = id, "Teams video: renderer removed");
        let drawn = self.state.lock().ok().and_then(|mut s| {
            s.callbacks.remove(&id);
            s.drawn.remove(&id)
        });
        if let Some(r) = drawn {
            erase(&r);
        }
    }

    fn render(&self, id: u64, format: &GUID, width: u32, height: u32, stride: i32, image: &[u8]) {
        static LOGGED: AtomicU32 = AtomicU32::new(0);
        if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
            tracing::info!(mapping_id = id, format = format!("{:08X}", format.data1), width, height, stride, "Teams video: frame");
        }
        let place = self.state.lock().ok().and_then(|s| {
            s.geometry.get(&id).map(|g| (g.desktop_rect(), g.visible_on_desktop()))
        });
        let (rect, clip) = place.unzip();
        let clip = clip.flatten();
        let rect = rect.filter(|r| (1..=MAX_SIDE).contains(&r.width()) && (1..=MAX_SIDE).contains(&r.height()));
        let nv12 = to_nv12(format, width as usize, height as usize, stride, image);
        let (Some(rect), Some(nv12)) = (rect, nv12) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        (self.out)(Overlay {
            x: rect.left,
            y: rect.top,
            w: rect.width() as u32,
            h: rect.height() as u32,
            picture: Picture::Nv12 { nv12: Arc::new(nv12), width: width & !1, height: height & !1, cover: false, mirror: false },
            clip,
        });
        if let Ok(mut s) = self.state.lock() {
            s.drawn.insert(id, rect);
        }
        self.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// The add-in's `RDCLIENT_BITMAP_RENDER_SERVICE`.
    pub fn service(self: &Arc<Self>) -> IWTSBitmapRenderService {
        RenderService { video: self.clone() }.into()
    }
}

/// Have the server repaint where video was drawn into the session image.
fn erase(r: &GeoRect) {
    crate::session::request_refresh(r.left, r.top, r.width() as u32, r.height() as u32);
}

/// One frame as tightly packed NV12, even-sized (which is all NV12 can be).
/// `None` for a format we don't know or a buffer too short for what it claims.
fn to_nv12(format: &GUID, width: usize, height: usize, stride: i32, image: &[u8]) -> Option<Vec<u8>> {
    let is_mf_format = GUID::from_u128(MF_FORMAT_BASE | (u128::from(format.data1) << 96)) == *format;
    let (w, h) = (width & !1, height & !1);
    if !is_mf_format || w == 0 || h == 0 {
        return None;
    }
    let pitch = stride.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    match format.data1 {
        FORMAT_NV12 => {
            if pitch < w || image.len() < pitch * height + pitch * (h / 2 - 1) + w {
                return None;
            }
            (0..h).for_each(|y| out.extend_from_slice(&image[y * pitch..][..w]));
            let uv = &image[pitch * height..];
            (0..h / 2).for_each(|y| out.extend_from_slice(&uv[y * pitch..][..w]));
        }
        FORMAT_I420 => {
            let half = pitch / 2;
            if pitch < w || image.len() < pitch * height + half * height.div_ceil(2) * 2 {
                return None;
            }
            (0..h).for_each(|y| out.extend_from_slice(&image[y * pitch..][..w]));
            let (u, v) = image[pitch * height..].split_at(half * height.div_ceil(2));
            for y in 0..h / 2 {
                for x in 0..w / 2 {
                    out.extend_from_slice(&[u[y * half + x], v[y * half + x]]);
                }
            }
        }
        FORMAT_RGB32 | FORMAT_ARGB32 => {
            if pitch < width * 4 || image.len() < pitch * height {
                return None;
            }
            // (A negative stride is a bottom-up image.)
            let row = |y: usize| {
                let y = if stride < 0 { height - 1 - y } else { y };
                &image[y * pitch..][..width * 4]
            };
            for y in 0..h {
                let r = row(y);
                out.extend((0..w).map(|x| {
                    let (b, g, r) = (i32::from(r[x * 4]), i32::from(r[x * 4 + 1]), i32::from(r[x * 4 + 2]));
                    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
                }));
            }
            for y in (0..h).step_by(2) {
                let (r0, r1) = (row(y), row(y + 1));
                for x in (0..w).step_by(2) {
                    let channel = |i: usize| {
                        [r0[x * 4 + i], r0[(x + 1) * 4 + i], r1[x * 4 + i], r1[(x + 1) * 4 + i]]
                            .iter()
                            .map(|v| i32::from(*v))
                            .sum::<i32>()
                            / 4
                    };
                    let (b, g, r) = (channel(0), channel(1), channel(2));
                    out.push((((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8);
                    out.push((((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8);
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

#[implement(IWTSBitmapRenderService)]
struct RenderService {
    video: Arc<MappedVideo>,
}

impl IWTSBitmapRenderService_Impl for RenderService_Impl {
    fn GetMappedRenderer(&self, mappingid: u64, callback: Ref<IWTSBitmapRendererCallback>) -> Result<IWTSBitmapRenderer> {
        self.video.add_renderer(mappingid, callback.cloned());
        Ok(Renderer { id: mappingid, video: self.video.clone() }.into())
    }
}

#[implement(IWTSBitmapRenderer)]
struct Renderer {
    id: u64,
    video: Arc<MappedVideo>,
}

impl IWTSBitmapRenderer_Impl for Renderer_Impl {
    fn Render(&self, format: &GUID, width: u32, height: u32, stride: i32, size: u32, image: *const u8) -> Result<()> {
        if image.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the add-in's buffer, `size` bytes for the length of this call.
        let image = unsafe { std::slice::from_raw_parts(image, size as usize) };
        self.video.render(self.id, format, width, height, stride, image);
        Ok(())
    }

    fn GetRendererStatistics(&self) -> Result<BITMAP_RENDERER_STATISTICS> {
        Ok(BITMAP_RENDERER_STATISTICS {
            dwFramesDelivered: self.video.delivered.load(Ordering::Relaxed),
            dwFramesDropped: self.video.dropped.load(Ordering::Relaxed),
        })
    }

    fn RemoveMapping(&self) -> Result<()> {
        self.video.remove_renderer(self.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::RemoteDesktop::IWTSBitmapRendererCallback_Impl;

    fn format(fourcc: u32) -> GUID {
        GUID::from_u128(MF_FORMAT_BASE | (u128::from(fourcc) << 96))
    }

    #[test]
    fn the_add_ins_formats_are_the_media_foundation_ones() {
        // MFVideoFormat_NV12, as the add-in's own table has it.
        assert_eq!(format(FORMAT_NV12), GUID::from_u128(0x3231564E_0000_0010_8000_00AA00389B71));
        assert_eq!(to_nv12(&GUID::from_u128(0x3231564E_0000_0010_8000_00AA00389B72), 2, 2, 2, &[0; 6]), None);
    }

    #[test]
    fn nv12_loses_its_stride() {
        // 2x2 with a stride of 3: Y rows, then one UV row.
        let image = [1, 2, 99, 3, 4, 99, 5, 6, 99];
        assert_eq!(to_nv12(&format(FORMAT_NV12), 2, 2, 3, &image).unwrap(), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn i420_interleaves_its_chroma() {
        let image = [1, 2, 3, 4, 5, 6];
        assert_eq!(to_nv12(&format(FORMAT_I420), 2, 2, 2, &image).unwrap(), [1, 2, 3, 4, 5, 6]);
        let image = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24];
        assert_eq!(
            to_nv12(&format(FORMAT_I420), 4, 4, 4, &image).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 21, 18, 22, 19, 23, 20, 24]
        );
    }

    #[test]
    fn rgb32_converts_and_reads_a_negative_stride_bottom_up() {
        let white = [255u8, 255, 255, 255];
        let black = [0u8, 0, 0, 255];
        let image: Vec<u8> = [white, white, black, black].concat();
        let down = to_nv12(&format(FORMAT_RGB32), 2, 2, 8, &image).unwrap();
        assert_eq!(&down[..4], [235, 235, 16, 16]);
        assert!((127..=129).contains(&down[4]) && (127..=129).contains(&down[5]));
        let up = to_nv12(&format(FORMAT_RGB32), 2, 2, -8, &image).unwrap();
        assert_eq!(&up[..4], [16, 16, 235, 235]);
    }

    #[test]
    fn a_buffer_too_short_is_refused_not_read_past() {
        assert_eq!(to_nv12(&format(FORMAT_NV12), 4, 4, 4, &[0; 23]), None);
        assert_eq!(to_nv12(&format(FORMAT_I420), 4, 4, 4, &[0; 23]), None);
        assert_eq!(to_nv12(&format(FORMAT_RGB32), 4, 4, 16, &[0; 63]), None);
        assert_eq!(to_nv12(&format(0x1234_5678), 2, 2, 2, &[0; 64]), None);
    }

    #[implement(IWTSBitmapRendererCallback)]
    struct Sizes(Arc<Mutex<Vec<(i32, i32)>>>);

    impl IWTSBitmapRendererCallback_Impl for Sizes_Impl {
        fn OnTargetSizeChanged(&self, r: &RECT) -> Result<()> {
            self.0.lock().unwrap().push((r.right - r.left, r.bottom - r.top));
            Ok(())
        }
    }

    fn mapping(id: u64, left: i32, top: i32, w: i32, h: i32) -> GeometryUpdate {
        GeometryUpdate::Update(MappedGeometry {
            mapping_id: id,
            top_level: GeoRect { left, top, right: left + 1000, bottom: top + 1000 },
            rect: GeoRect { left: 0, top: 0, right: w, bottom: h },
            ..Default::default()
        })
    }

    #[test]
    fn a_renderer_hears_its_size_and_its_frames_land_on_the_mapping() {
        let drawn = Arc::new(Mutex::new(Vec::new()));
        let d = drawn.clone();
        let video = MappedVideo::new(Box::new(move |o: Overlay| d.lock().unwrap().push((o.x, o.y, o.w, o.h))));
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let callback: IWTSBitmapRendererCallback = Sizes(sizes.clone()).into();
        let renderer = unsafe { video.service().GetMappedRenderer(7, &callback) }.unwrap();
        let nv12 = format(FORMAT_NV12);
        let frame = [0u8; 6];

        // No geometry yet: nowhere to draw it.
        unsafe { renderer.Render(nv12, 2, 2, 2, &frame) }.unwrap();
        assert!(drawn.lock().unwrap().is_empty());
        assert_eq!(unsafe { renderer.GetRendererStatistics() }.unwrap().dwFramesDropped, 1);

        video.geometry(mapping(7, 100, 50, 640, 360));
        assert_eq!(*sizes.lock().unwrap(), [(640, 360)]);
        unsafe { renderer.Render(nv12, 2, 2, 2, &frame) }.unwrap();
        assert_eq!(*drawn.lock().unwrap(), [(100, 50, 640, 360)]);

        // Moved, same size: no new size to hear, the next frame lands there.
        video.geometry(mapping(7, 200, 50, 640, 360));
        assert_eq!(sizes.lock().unwrap().len(), 1);
        unsafe { renderer.Render(nv12, 2, 2, 2, &frame) }.unwrap();
        assert_eq!(drawn.lock().unwrap()[1], (200, 50, 640, 360));

        // Gone: its size is nothing now.
        video.geometry(GeometryUpdate::Clear(7));
        assert_eq!(sizes.lock().unwrap()[1], (0, 0));
    }

    #[test]
    fn a_frame_carries_the_part_of_its_mapping_left_visible() {
        let clips = Arc::new(Mutex::new(Vec::new()));
        let c = clips.clone();
        let video = MappedVideo::new(Box::new(move |o: Overlay| c.lock().unwrap().push((o.is_unclipped(), o.clip))));
        let callback: IWTSBitmapRendererCallback = Sizes(Default::default()).into();
        let renderer = unsafe { video.service().GetMappedRenderer(3, &callback) }.unwrap();
        let visible = vec![GeoRect { left: 0, top: 0, right: 320, bottom: 360 }];
        let mut update = MappedGeometry {
            mapping_id: 3,
            top_level: GeoRect { left: 0, top: 0, right: 1000, bottom: 1000 },
            rect: GeoRect { left: 0, top: 0, right: 640, bottom: 360 },
            visible: visible.clone(),
            ..Default::default()
        };
        video.geometry(GeometryUpdate::Update(update.clone()));
        unsafe { renderer.Render(format(FORMAT_NV12), 2, 2, 2, &[0; 6]) }.unwrap();
        update.visible = vec![update.rect];
        video.geometry(GeometryUpdate::Update(update));
        unsafe { renderer.Render(format(FORMAT_NV12), 2, 2, 2, &[0; 6]) }.unwrap();
        let clips = clips.lock().unwrap();
        assert_eq!(clips[0], (false, Some(visible)));
        assert!(clips[1].0, "a clip that covers the box is no clip");
    }

    #[test]
    fn a_renderer_asked_for_after_its_geometry_hears_its_size_on_the_host_thread() {
        let video = MappedVideo::new(Box::new(|_| {}));
        video.geometry(mapping(9, 0, 0, 320, 180));
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let callback: IWTSBitmapRendererCallback = Sizes(sizes.clone()).into();
        let _renderer = unsafe { video.service().GetMappedRenderer(9, &callback) }.unwrap();
        // (Not inside the call that asked: the add-in may hold locks there.)
        assert!(sizes.lock().unwrap().is_empty());
        video.deliver_sizes();
        assert_eq!(*sizes.lock().unwrap(), [(320, 180)]);
    }
}
