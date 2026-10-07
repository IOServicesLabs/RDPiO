//! Teams video for the native engine (`--teams-native`): draws each visible
//! video element into the session image at the desktop rectangle the server
//! tracks for it.
//!
//! Teams never sends a video element's position over webrtc.1. Instead,
//! `MediaElement.notifyVisibilityChanged` carries a geometry *mapping id*, and
//! the server streams that mapping's rectangle over the MS-RDPEGT geometry
//! channel ([`rdp_channels::geometry`]). This module joins the two: the
//! presentation model (which element shows which stream, its object-fit and
//! mirroring) plus the geometry table give a rectangle per visible element, and
//! a render thread draws the element's picture there roughly 30 times a second.
//!
//! It draws the **local camera** (Teams' Settings preview and the in-call
//! self-view) and **remote participants' video**, which the libwebrtc engine
//! decodes and hands over through [`RemoteVideoSink`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rdp_channels::geometry::{GeoRect, GeometryUpdate, MappedGeometry};
use rdp_webrtc::{I420Frame, PresentationModel, RpcMessage};


/// Camera capture size for on-screen video (the shared capture's size).
const CAPTURE_W: usize = crate::camera_hub::WIDTH;
const CAPTURE_H: usize = crate::camera_hub::HEIGHT;
/// Redraw cadence (~30 fps, the capture rate).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Largest element we will draw (guards against a bogus geometry).
const MAX_SIDE: i32 = 8192;

/// A picture to draw into the session image at desktop `(x, y)`: tightly
/// packed RGBA, `w * h * 4` bytes. May extend past the desktop edges.
pub struct Overlay {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Where overlays go: the UI thread, which owns the renderer.
pub type OverlayOut = Box<dyn Fn(Overlay) + Send + Sync>;

#[derive(Default)]
struct State {
    model: PresentationModel,
    /// Mapping id → its latest geometry.
    geometry: HashMap<u64, MappedGeometry>,
    /// Remote stream rpc id (what `srcObject` names) → its msid.
    remote_streams: HashMap<u64, String>,
    /// Remote stream msid → (frame sequence number, latest decoded frame).
    remote_frames: HashMap<String, (u64, Arc<I420Frame>)>,
    frame_seq: u64,
}

/// What an element shows.
#[derive(Clone, PartialEq, Eq)]
enum Source {
    /// A local camera, by device index.
    Camera(u32),
    /// A remote participant's stream, by msid.
    Remote(String),
}

/// One element to draw this frame.
struct Target {
    rect: GeoRect,
    cover: bool,
    mirror: bool,
    source: Source,
}

/// Receives remote video from the engine (via the call's `MediaSink`).
#[derive(Clone)]
pub struct RemoteVideoSink {
    state: Arc<Mutex<State>>,
}

impl RemoteVideoSink {
    pub fn on_remote_stream(&self, rpc_id: u64, stream_id: &str) {
        if let Ok(mut s) = self.state.lock() {
            s.remote_streams.insert(rpc_id, stream_id.to_string());
        }
    }

    pub fn on_video_frame(&self, stream_id: &str, frame: I420Frame) {
        if let Ok(mut s) = self.state.lock() {
            s.frame_seq += 1;
            let seq = s.frame_seq;
            s.remote_frames.insert(stream_id.to_string(), (seq, Arc::new(frame)));
        }
    }
}

/// Owns the presentation/geometry state and the render thread.
pub struct VideoCompositor {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VideoCompositor {
    pub fn new(out: OverlayOut) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (state.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("teams-video".into())
            .spawn(move || render_loop(s, st, out))
            .map_err(|e| tracing::warn!(error = %e, "no Teams video thread; video won't draw"))
            .ok();
        Self { state, stop, thread }
    }

    /// Where the engine delivers remote participants' video.
    pub fn remote_sink(&self) -> RemoteVideoSink {
        RemoteVideoSink { state: self.state.clone() }
    }

    /// Fold one inbound webrtc.1 call into the presentation model.
    pub fn observe(&self, msg: &RpcMessage) {
        let Ok(mut s) = self.state.lock() else { return };
        s.model.observe(msg);
        if msg.name.as_deref() == Some("notifyVisibilityChanged") {
            if let Some(id) = msg.object_id_u64() {
                let e = s.model.elements.get(&id);
                tracing::info!(
                    element = id,
                    visible = e.map(|e| e.visible),
                    mapping_id = ?e.and_then(|e| e.mapping_id),
                    stream = ?e.and_then(|e| e.src_stream_id),
                    "Teams video element visibility"
                );
            }
        }
    }

    /// Apply one MS-RDPEGT geometry update.
    pub fn geometry(&self, update: GeometryUpdate) {
        let Ok(mut s) = self.state.lock() else { return };
        match update {
            GeometryUpdate::Update(g) => {
                if !s.geometry.contains_key(&g.mapping_id) {
                    tracing::info!(
                        mapping_id = g.mapping_id,
                        top_level = ?g.top_level,
                        rect = ?g.rect,
                        desktop = ?g.desktop_rect(),
                        visible_rects = g.visible.len(),
                        "new geometry mapping"
                    );
                }
                s.geometry.insert(g.mapping_id, g);
            }
            GeometryUpdate::Clear(id) => {
                s.geometry.remove(&id);
            }
        }
    }

}

impl Drop for VideoCompositor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The visible elements this frame can draw: mapped to a geometry, showing a
/// local camera.
fn targets(s: &State, skipped: &mut HashSet<u64>) -> Vec<Target> {
    let mut out = Vec::new();
    for (id, e) in s.model.visible_video_targets() {
        let geometry = e.mapping_id.and_then(|m| s.geometry.get(&m));
        let source = e.src_stream_id.and_then(|sid| {
            s.model
                .camera_for_stream(sid)
                .and_then(|src| src.strip_prefix("rdpio-videoinput-"))
                .and_then(|i| i.parse::<u32>().ok())
                .map(Source::Camera)
                .or_else(|| s.remote_streams.get(&sid).cloned().map(Source::Remote))
        });
        match (geometry, source) {
            (Some(g), Some(source)) => {
                let rect = g.desktop_rect();
                if rect.width() > 0 && rect.height() > 0 && rect.width() <= MAX_SIDE && rect.height() <= MAX_SIDE {
                    out.push(Target {
                        rect,
                        cover: e.object_fit.as_deref() == Some("cover"),
                        mirror: e.is_mirrored(),
                        source,
                    });
                }
            }
            (g, src) => {
                if skipped.insert(id) {
                    tracing::info!(
                        element = id,
                        has_geometry = g.is_some(),
                        has_source = src.is_some(),
                        "Teams video element not drawn yet (no geometry, or no known stream)"
                    );
                }
            }
        }
    }
    out
}

/// Have the server repaint `r` — where video was drawn into the session image
/// and no longer is. Nothing else restores those pixels: the overlay is written
/// into the desktop image itself, and the server only repaints what changes.
fn erase(r: &GeoRect) {
    crate::session::request_refresh(r.left, r.top, r.width().max(0) as u32, r.height().max(0) as u32);
}

fn render_loop(state: Arc<Mutex<State>>, stop: Arc<AtomicBool>, out: OverlayOut) {
    let mut camera: Option<(u32, crate::camera_hub::CameraLease)> = None;
    let mut camera_seq = 0u64;
    let mut skipped = HashSet::new();
    // Every rectangle video is currently drawn at, to erase when it moves away.
    let mut drawn: Vec<GeoRect> = Vec::new();
    // Remote stream → (frame seq, rect) last drawn, to skip redundant redraws.
    let mut remote_drawn: HashMap<String, (u64, (i32, i32, i32, i32))> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(FRAME_INTERVAL);
        let targets = match state.lock() {
            Ok(s) => targets(&s, &mut skipped),
            Err(_) => return,
        };
        // A video element that moved, resized or went away (Teams shrinks the
        // big call-setup preview into the corner tile, ends the call, …).
        drawn.retain(|r| {
            let still = targets.iter().any(|t| t.rect == *r);
            if !still {
                erase(r);
            }
            still
        });
        // Remote participants: draw each element's stream when it has a new
        // frame (or the element moved).
        for t in &targets {
            let Source::Remote(msid) = &t.source else { continue };
            let latest = state.lock().ok().and_then(|s| s.remote_frames.get(msid).cloned());
            let Some((seq, frame)) = latest else { continue };
            let key = (t.rect.left, t.rect.top, t.rect.right, t.rect.bottom);
            if remote_drawn.get(msid) == Some(&(seq, key)) {
                continue;
            }
            remote_drawn.insert(msid.clone(), (seq, key));
            let (w, h) = (t.rect.width() as usize, t.rect.height() as usize);
            let nv12 = frame.to_nv12();
            let rgba = fit_nv12(&nv12, frame.width as usize, frame.height as usize, w, h, t.cover, t.mirror);
            out(Overlay { x: t.rect.left, y: t.rect.top, w: w as u32, h: h as u32, rgba });
            if !drawn.contains(&t.rect) {
                drawn.push(t.rect);
            }
        }

        // One camera at a time: the first element's. (Teams shows one device.)
        let first_camera = targets.iter().find_map(|t| match t.source {
            Source::Camera(c) => Some(c),
            Source::Remote(_) => None,
        });
        match (first_camera, &camera) {
            (None, Some(_)) => {
                camera = None; // dropping stops the capture
                tracing::info!("Teams video: no camera element visible; camera released");
            }
            (Some(want), Some((have, _))) if want == *have => {}
            (Some(want), _) => {
                drop(camera.take()); // release any other camera first
                camera = Some((want, crate::camera_hub::acquire(want)));
                camera_seq = 0;
                tracing::info!(camera = want, "Teams video: camera started for on-screen video");
            }
            (None, None) => {}
        }
        let Some((index, _lease)) = camera.as_ref() else {
            continue;
        };
        let Some((seq, frame)) = crate::camera_hub::latest(*index) else {
            continue;
        };
        if seq == camera_seq {
            continue;
        }
        camera_seq = seq;
        for t in targets.iter().filter(|t| t.source == Source::Camera(*index)) {
            let (w, h) = (t.rect.width() as usize, t.rect.height() as usize);
            let rgba = fit_nv12(&frame, CAPTURE_W, CAPTURE_H, w, h, t.cover, t.mirror);
            out(Overlay {
                x: t.rect.left,
                y: t.rect.top,
                w: w as u32,
                h: h as u32,
                rgba,
            });
            if !drawn.contains(&t.rect) {
                drawn.push(t.rect);
            }
        }
    }
    for r in &drawn {
        erase(r);
    }
}

/// BT.601 limited-range YUV → RGB (webcams deliver BT.601).
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (y as i32 - 16);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let clamp = |x: i32| (x >> 8).clamp(0, 255) as u8;
    [
        clamp(c + 409 * e + 128),
        clamp(c - 100 * d - 208 * e + 128),
        clamp(c + 516 * d + 128),
    ]
}

/// Scale an NV12 frame (`sw`×`sh`) into a `tw`×`th` RGBA picture the way CSS
/// `object-fit` does — `contain` letterboxes with black, `cover` crops — and
/// mirror it horizontally for a self-view. Nearest-neighbour, one pass over the
/// target, reading Y/UV straight from the frame.
fn fit_nv12(nv12: &[u8], sw: usize, sh: usize, tw: usize, th: usize, cover: bool, mirror: bool) -> Vec<u8> {
    let mut out = vec![0u8; tw * th * 4];
    for px in out.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
    let (fx, fy) = (tw as f64 / sw as f64, th as f64 / sh as f64);
    let scale = if cover { fx.max(fy) } else { fx.min(fy) };
    let dw = ((sw as f64 * scale).round() as usize).max(1);
    let dh = ((sh as f64 * scale).round() as usize).max(1);
    // Where the scaled picture sits in the target (negative when `cover` crops).
    let ox = (tw as isize - dw as isize) / 2;
    let oy = (th as isize - dh as isize) / 2;
    let (y_plane, uv_plane) = nv12.split_at(sw * sh);
    for ty in 0..th {
        let dy = ty as isize - oy;
        if dy < 0 || dy >= dh as isize {
            continue;
        }
        let sy = (dy as usize * sh / dh).min(sh - 1);
        let (y_row, uv_row) = (sy * sw, (sy / 2) * sw);
        let row = &mut out[ty * tw * 4..(ty + 1) * tw * 4];
        for tx in 0..tw {
            let dx = tx as isize - ox;
            if dx < 0 || dx >= dw as isize {
                continue;
            }
            let mut sx = (dx as usize * sw / dw).min(sw - 1);
            if mirror {
                sx = sw - 1 - sx;
            }
            let uv = uv_row + (sx & !1);
            let rgb = yuv_to_rgb(y_plane[y_row + sx], uv_plane[uv], uv_plane[uv + 1]);
            row[tx * 4..tx * 4 + 3].copy_from_slice(&rgb);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4×2 NV12 frame: left half white (Y 235), right half black (Y 16),
    /// neutral chroma.
    fn half_white() -> Vec<u8> {
        let mut f = vec![235, 235, 16, 16, 235, 235, 16, 16];
        f.extend_from_slice(&[128; 4]);
        f
    }

    fn px(img: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn contain_letterboxes_and_keeps_orientation() {
        // 4×2 into 4×4: scaled to 4×2, centred with a black band above/below.
        let out = fit_nv12(&half_white(), 4, 2, 4, 4, false, false);
        assert_eq!(px(&out, 4, 0, 0), [0, 0, 0, 255], "letterbox band");
        assert_eq!(px(&out, 4, 0, 1), [255, 255, 255, 255], "left is white");
        assert_eq!(px(&out, 4, 3, 1), [0, 0, 0, 255], "right is black");
    }

    #[test]
    fn mirror_flips_left_and_right() {
        let out = fit_nv12(&half_white(), 4, 2, 4, 2, false, true);
        assert_eq!(px(&out, 4, 0, 0), [0, 0, 0, 255], "mirrored: left is black");
        assert_eq!(px(&out, 4, 3, 0), [255, 255, 255, 255], "mirrored: right is white");
    }

    #[test]
    fn cover_fills_the_target_by_cropping() {
        // 4×2 into 2×2: cover scales to 4×2 and crops the middle — no black band.
        let out = fit_nv12(&half_white(), 4, 2, 2, 2, true, false);
        assert!(out.chunks_exact(4).all(|p| p[3] == 255));
        assert_eq!(px(&out, 2, 0, 0), [255, 255, 255, 255]);
        assert_eq!(px(&out, 2, 1, 1), [0, 0, 0, 255]);
    }
}
