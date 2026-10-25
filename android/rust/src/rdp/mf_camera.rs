//! The tablet's camera for a Cloud PC's apps (MS-RDPECAM), with the API of
//! the desktop's Media Foundation camera so the shared session loop drives it
//! unchanged. The app captures and encodes (Camera2 into a hardware H.264
//! encoder, see `capture`); this queues the frames the server asked for.
//!
//! On demand, like the desktop: the server sends a SampleRequest per frame it
//! wants, and gets the frames in order. The tablet's encoder runs at its own
//! pace, so frames wait here (a few at most) until they're asked for; a
//! server that falls further behind than that loses the queue, and the stream
//! picks up again at the next keyframe (asked for at once) — every H.264
//! frame builds on the one before.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rdp_channels::camera::{CamFormat, CameraDevice, MediaType};
use rdp_channels::names::CAMERA_DEVICE_PREFIX;

use crate::capture::{self, CameraRequest};

/// Frames kept for a server that hasn't asked for them yet (~130 ms at 30 fps).
const MAX_QUEUED: usize = 4;

struct Shared {
    frames: Mutex<VecDeque<Vec<u8>>>,
    ready: Condvar,
    /// Frames still owed to the server.
    demand: AtomicU32,
    /// Frames were dropped: wait for a keyframe.
    resync: AtomicBool,
}

pub struct MfCamera {
    shared: Arc<Shared>,
    generation: u32,
}

impl MfCamera {
    /// The tablet's camera, when it has one (the app counts them).
    pub fn enumerate() -> Vec<CameraDevice> {
        if capture::CAMERA_COUNT.load(Ordering::Relaxed) == 0 {
            return Vec::new();
        }
        vec![CameraDevice { name: "Tablet camera".into(), channel_name: format!("{CAMERA_DEVICE_PREFIX}0") }]
    }

    /// H.264 only: the tablet's encoder does it in hardware.
    pub fn media_types() -> Vec<MediaType> {
        vec![
            MediaType { format: CamFormat::H264, width: 1280, height: 720, fps_num: 30, fps_den: 1 },
            MediaType { format: CamFormat::H264, width: 640, height: 480, fps_num: 30, fps_den: 1 },
        ]
    }

    pub fn start_on_demand(_device_index: u32, media: MediaType) -> Self {
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            demand: AtomicU32::new(0),
            resync: AtomicBool::new(false),
        });
        let fps = (media.fps_num / media.fps_den.max(1)).clamp(5, 60);
        // ~0.1 bit per pixel per frame: 2.8 Mbps at 720p30, 0.9 at 480p.
        let bitrate = (media.width * media.height * fps / 10).clamp(500_000, 4_000_000);
        let request = CameraRequest { width: media.width, height: media.height, fps, bitrate };
        let s = shared.clone();
        let sink: capture::CameraSink = Box::new(move |data: &[u8], keyframe: bool| {
            if s.resync.load(Ordering::Relaxed) && !keyframe {
                return;
            }
            let mut q = s.frames.lock().unwrap();
            if q.len() >= MAX_QUEUED {
                q.clear();
                if !keyframe {
                    s.resync.store(true, Ordering::Relaxed);
                    capture::camera_keyframe();
                    return;
                }
            }
            s.resync.store(false, Ordering::Relaxed);
            q.push_back(data.to_vec());
            drop(q);
            s.ready.notify_all();
            if s.demand.load(Ordering::Acquire) > 0 {
                crate::net_wait::worker_wake::signal();
            }
        });
        let generation = capture::want_camera(Some((request, sink)));
        Self { shared, generation }
    }

    pub fn request_keyframe(&self) {
        capture::camera_keyframe();
    }

    pub fn is_finished(&self) -> bool {
        false
    }

    pub fn request_frames(&self, n: u32) {
        self.shared.demand.fetch_add(n, Ordering::AcqRel);
    }

    /// The oldest frame, if the server is owed one.
    pub fn next_frame(&self) -> Option<Vec<u8>> {
        let mut q = self.shared.frames.lock().ok()?;
        if q.is_empty() {
            return None;
        }
        self.shared.demand.fetch_update(Ordering::AcqRel, Ordering::Acquire, |d| d.checked_sub(1)).ok()?;
        q.pop_front()
    }

    pub fn wait_frame(&self, timeout: std::time::Duration) {
        let Ok(q) = self.shared.frames.lock() else { return };
        if q.is_empty() {
            let _ = self.shared.ready.wait_timeout(q, timeout);
        }
    }

    pub fn poll_frame(&self) -> Option<Vec<u8>> {
        let mut q = self.shared.frames.lock().ok()?;
        let latest = q.pop_back();
        q.clear();
        latest
    }
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        capture::release_camera(self.generation);
    }
}
