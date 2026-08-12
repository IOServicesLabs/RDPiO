//! Multi-monitor desktop layout.
//!
//! The client virtual desktop can span several physical monitors. This module
//! computes the union bounding box (the desktop size announced in CS_CORE) and
//! the per-monitor rectangles (the CS_MONITOR block) from the physical layout.
//! All geometry math is pure logic and unit-tested; the Windows enumeration of
//! physical monitors is a thin `cfg(windows)` wrapper.

use wire_main::Monitor;

/// A physical or virtual monitor rectangle in desktop coordinates
/// (right/bottom exclusive, matching `MONITORINFO.rcMonitor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl DeviceRect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn is_valid(&self) -> bool {
        self.width() > 0 && self.height() > 0
    }
}

impl From<DeviceRect> for Monitor {
    fn from(r: DeviceRect) -> Self {
        Monitor {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
            flags: 0,
        }
    }
}

/// The union bounding box of a set of rectangles.
pub fn union_rect(rects: &[DeviceRect]) -> DeviceRect {
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for r in rects {
        if !r.is_valid() {
            continue;
        }
        left = left.min(r.left);
        top = top.min(r.top);
        right = right.max(r.right);
        bottom = bottom.max(r.bottom);
    }
    if left == i32::MAX {
        DeviceRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }
    } else {
        DeviceRect {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// A computed client desktop layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayout {
    /// One entry per physical monitor, in desktop coordinates.
    pub monitors: Vec<Monitor>,
    /// The virtual desktop width (the union width, clamped to u16).
    pub width: u16,
    /// The virtual desktop height (the union height, clamped to u16).
    pub height: u16,
}

/// Build the desktop layout for a set of physical monitor rectangles.
pub fn layout(rects: &[DeviceRect]) -> MonitorLayout {
    let valid: Vec<DeviceRect> = rects.iter().copied().filter(DeviceRect::is_valid).collect();
    let union = union_rect(&valid);
    MonitorLayout {
        monitors: valid.into_iter().map(Monitor::from).collect(),
        width: union.width().clamp(1, u16::MAX as i32) as u16,
        height: union.height().clamp(1, u16::MAX as i32) as u16,
    }
}

/// A single-monitor layout sized `width` × `height` at the origin.
pub fn single(width: u16, height: u16) -> MonitorLayout {
    MonitorLayout {
        monitors: vec![Monitor {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
            flags: 0,
        }],
        width,
        height,
    }
}

/// Scale every rectangle by `factor`, keeping the top-left origin and even
/// dimensions (RDP bitmaps require even widths/heights).
pub fn scale(rects: &[DeviceRect], factor: f32) -> Vec<DeviceRect> {
    let scale_dim = |v: i32| -> i32 {
        let scaled = (v as f32 * factor).round() as i32;
        if scaled % 2 != 0 {
            scaled + 1
        } else {
            scaled
        }
    };
    rects
        .iter()
        .map(|r| DeviceRect {
            left: r.left,
            top: r.top,
            right: r.left + scale_dim(r.width()),
            bottom: r.top + scale_dim(r.height()),
        })
        .collect()
}

/// Enumerate physical monitors. On Windows this queries the real layout via
/// `EnumDisplayMonitors`; elsewhere it falls back to a single 1920×1080
/// monitor so the layout math stays meaningful.
#[cfg(windows)]
pub fn enumerate_windows_monitors() -> Vec<DeviceRect> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORENUMPROC, MONITORINFO,
    };

    unsafe extern "system" fn enum_proc(
        hmon: HMONITOR,
        _hdc: HDC,
        _lprc: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let rects = &mut *(lparam.0 as *mut Vec<DeviceRect>);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut info).as_bool() {
            let rc = info.rcMonitor;
            rects.push(DeviceRect {
                left: rc.left,
                top: rc.top,
                right: rc.right,
                bottom: rc.bottom,
            });
        }
        BOOL(1)
    }

    let mut rects = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(MONITORENUMPROC(Some(enum_proc))),
            LPARAM(&mut rects as *mut Vec<DeviceRect> as isize),
        );
    }
    if rects.is_empty() {
        rects.push(DeviceRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        });
    }
    rects
}

/// Enumerate physical monitors (non-Windows fallback: one 1920×1080 monitor).
#[cfg(not(windows))]
pub fn enumerate_windows_monitors() -> Vec<DeviceRect> {
    vec![DeviceRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_monitor_layout_matches_requested_size() {
        let l = single(1280, 800);
        assert_eq!((l.width, l.height), (1280, 800));
        assert_eq!(l.monitors.len(), 1);
        assert_eq!(
            l.monitors[0],
            Monitor {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 800,
                flags: 0
            }
        );
    }

    #[test]
    fn union_of_two_side_by_side_monitors() {
        let rects = [
            DeviceRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            DeviceRect {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
        ];
        let u = union_rect(&rects);
        assert_eq!((u.left, u.top, u.right, u.bottom), (0, 0, 3840, 1080));
        let l = layout(&rects);
        assert_eq!((l.width, l.height), (3840, 1080));
        assert_eq!(l.monitors.len(), 2);
    }

    #[test]
    fn monitors_below_the_origin_shift_the_union() {
        let rects = [
            DeviceRect {
                left: 0,
                top: -1080,
                right: 1920,
                bottom: 0,
            },
            DeviceRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
        ];
        let u = union_rect(&rects);
        assert_eq!((u.top, u.bottom), (-1080, 1080));
        let l = layout(&rects);
        assert_eq!((l.width, l.height), (1920, 2160));
    }

    #[test]
    fn invalid_rects_are_ignored() {
        let rects = [
            DeviceRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            DeviceRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
        ];
        assert_eq!(union_rect(&rects).right, 1920);
    }

    #[test]
    fn scale_keeps_dimensions_even() {
        let rects = [DeviceRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }];
        let scaled = scale(&rects, 0.7);
        assert_eq!(scaled[0].right % 2, 0);
        assert_eq!(scaled[0].bottom % 2, 0);
        assert!(scaled[0].right < 1920);
    }

    #[test]
    fn non_windows_enumeration_falls_back_to_one_monitor() {
        let rects = enumerate_windows_monitors();
        assert_eq!(rects.len(), 1);
        assert!(rects[0].is_valid());
    }
}
