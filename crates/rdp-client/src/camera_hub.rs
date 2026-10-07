//! One capture per camera, shared by everyone who needs it: Teams' on-screen
//! self-view (`teams_video`) and the call's outbound video (`webrtc_devices`).
//! A webcam can usually only be opened once, so both lease it from here and
//! read the latest frame; the capture stops when the last lease goes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rdp_channels::camera::{CamFormat, MediaType};

use crate::mf_camera::MfCamera;

/// Capture size: Teams' own camera constraint (1280×720 @ 30), NV12.
pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 720;

struct Entry {
    refs: usize,
    cam: MfCamera,
    /// Latest NV12 frame and its sequence number (0 = none yet).
    latest: Option<Arc<Vec<u8>>>,
    seq: u64,
}

static CAMERAS: Mutex<BTreeMap<u32, Entry>> = Mutex::new(BTreeMap::new());

/// Holds camera `index` open while alive.
pub struct CameraLease {
    index: u32,
}

impl CameraLease {
    pub fn index(&self) -> u32 {
        self.index
    }
}

/// Open (or share) camera `index`.
pub fn acquire(index: u32) -> CameraLease {
    if let Ok(mut cams) = CAMERAS.lock() {
        let e = cams.entry(index).or_insert_with(|| {
            tracing::info!(index, "camera capture started");
            Entry {
                refs: 0,
                cam: MfCamera::start(
                    index,
                    MediaType {
                        format: CamFormat::Nv12,
                        width: WIDTH as u32,
                        height: HEIGHT as u32,
                        fps_num: 30,
                        fps_den: 1,
                    },
                ),
                latest: None,
                seq: 0,
            }
        });
        e.refs += 1;
    }
    CameraLease { index }
}

/// The latest full NV12 frame of camera `index` and its sequence number
/// (increases with every new frame), if it is open and has produced one.
pub fn latest(index: u32) -> Option<(u64, Arc<Vec<u8>>)> {
    let mut cams = CAMERAS.lock().ok()?;
    let e = cams.get_mut(&index)?;
    if let Some(frame) = e.cam.poll_frame() {
        if frame.len() >= WIDTH * HEIGHT * 3 / 2 {
            e.seq += 1;
            e.latest = Some(Arc::new(frame));
        }
    }
    Some((e.seq, e.latest.clone()?))
}

impl Drop for CameraLease {
    fn drop(&mut self) {
        let Ok(mut cams) = CAMERAS.lock() else { return };
        let last = cams.get_mut(&self.index).map(|e| {
            e.refs -= 1;
            e.refs == 0
        });
        if last == Some(true) {
            // Dropping the MfCamera stops and joins its capture thread.
            cams.remove(&self.index);
            tracing::info!(index = self.index, "camera capture stopped");
        }
    }
}
