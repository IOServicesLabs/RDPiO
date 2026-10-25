//! A client's fingers as Windows touch (`InjectTouchInput`): taps, flicks,
//! press-and-hold, pinch and the edge swipes work as on a touch screen, not
//! as a mouse that jumps to each finger.
//!
//! Windows wants every frame to carry all contacts that are down, and a held
//! contact updated now and then: each event injects the whole set, and
//! [`keep_alive`] repeats it while fingers rest.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rdp_hyper::input::TouchPhase;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAGS, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, PT_TOUCH, SM_CXSCREEN, SM_CYSCREEN, TOUCH_FLAG_NONE, TOUCH_MASK_NONE};

/// Most fingers at once (Windows' own touch screens report 10).
const MAX_CONTACTS: usize = 10;
/// A resting finger is told again this often (or Windows cancels it).
const HOLD_REFRESH: Duration = Duration::from_millis(100);
/// Touch injection that couldn't start (the secure desktop was up, say) is
/// tried again after this.
const INIT_RETRY: Duration = Duration::from_secs(5);

struct Contact {
    slot: u32,
    at: POINT,
}

struct Touch {
    ready: bool,
    failed_at: Option<Instant>,
    contacts: BTreeMap<u8, Contact>,
    last: Option<Instant>,
    /// The stream whose fingers these are (a reconnect's new stream may start
    /// before the old one's thread finishes; its cleanup mustn't lift them).
    owner: u64,
}

static TOUCH: Mutex<Touch> = Mutex::new(Touch { ready: false, failed_at: None, contacts: BTreeMap::new(), last: None, owner: 0 });

/// Run `f` in physical pixels: injection and the screen's size both, whatever
/// this process's DPI awareness (a scaled desktop would move every touch).
fn physical<T>(f: impl FnOnce() -> T) -> T {
    unsafe {
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let out = f();
        if !old.is_invalid() {
            SetThreadDpiAwarenessContext(old);
        }
        out
    }
}

/// Stream position (0..=65535 across the primary monitor, the one streamed)
/// → screen pixels. (Call inside [`physical`].)
fn to_pixels(x: u16, y: u16) -> POINT {
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN).max(1), GetSystemMetrics(SM_CYSCREEN).max(1)) };
    POINT { x: (x as i64 * (w - 1) as i64 / 65535) as i32, y: (y as i64 * (h - 1) as i64 / 65535) as i32 }
}

fn info(slot: u32, at: POINT, flags: POINTER_FLAGS) -> POINTER_TOUCH_INFO {
    POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO { pointerType: PT_TOUCH, pointerId: slot, ptPixelLocation: at, pointerFlags: flags, ..Default::default() },
        touchFlags: TOUCH_FLAG_NONE,
        touchMask: TOUCH_MASK_NONE,
        ..Default::default()
    }
}

impl Touch {
    fn ready(&mut self) -> bool {
        if self.ready {
            return true;
        }
        if self.failed_at.is_some_and(|t| t.elapsed() < INIT_RETRY) {
            return false;
        }
        match unsafe { InitializeTouchInjection(MAX_CONTACTS as u32, TOUCH_FEEDBACK_DEFAULT) } {
            Ok(()) => self.ready = true,
            Err(e) => {
                tracing::warn!(error = %e, "touch injection unavailable for now; touches are dropped");
                self.failed_at = Some(Instant::now());
            }
        }
        self.ready
    }

    /// One frame: `changed` with `flags`, every other contact as it rests.
    /// False when Windows refused it.
    fn inject(&mut self, changed: Option<(u32, POINT, POINTER_FLAGS)>) -> bool {
        let held = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
        let mut frame: Vec<POINTER_TOUCH_INFO> =
            self.contacts.values().filter(|c| changed.is_none_or(|(s, _, _)| s != c.slot)).map(|c| info(c.slot, c.at, held)).collect();
        if let Some((slot, at, flags)) = changed {
            frame.push(info(slot, at, flags));
        }
        if frame.is_empty() {
            return true;
        }
        self.last = Some(Instant::now());
        match physical(|| unsafe { InjectTouchInput(&frame) }) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, contacts = frame.len(), "touch injection refused");
                false
            }
        }
    }

    fn apply(&mut self, id: u8, phase: TouchPhase, x: u16, y: u16) {
        if !self.ready() {
            return;
        }
        let at = physical(|| to_pixels(x, y));
        match phase {
            TouchPhase::Down => {
                if self.contacts.contains_key(&id) {
                    // (A down we already have: its up was lost. Lift it first.)
                    self.apply(id, TouchPhase::Cancel, x, y);
                }
                let Some(slot) = (0..MAX_CONTACTS as u32).find(|s| self.contacts.values().all(|c| c.slot != *s)) else { return };
                // Only a finger Windows took is kept: one it refused would
                // make every later frame (carrying it) refused too.
                if self.inject(Some((slot, at, POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT))) {
                    self.contacts.insert(id, Contact { slot, at });
                }
            }
            TouchPhase::Move => {
                let Some(c) = self.contacts.get_mut(&id) else { return };
                c.at = at;
                let slot = c.slot;
                self.inject(Some((slot, at, POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT)));
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                let Some(c) = self.contacts.remove(&id) else { return };
                let at = if phase == TouchPhase::Up { at } else { c.at };
                let flags = if phase == TouchPhase::Up { POINTER_FLAG_UP } else { POINTER_FLAG_UP | POINTER_FLAG_CANCELED };
                self.inject(Some((c.slot, at, flags)));
            }
        }
    }

    fn cancel_all(&mut self) {
        let ids: Vec<u8> = self.contacts.keys().copied().collect();
        for id in ids {
            self.apply(id, TouchPhase::Cancel, 0, 0);
        }
    }
}

/// A finger from the client of stream `owner` (any id unique to the stream).
pub fn apply(owner: u64, id: u8, phase: TouchPhase, x: u16, y: u16) {
    let mut t = TOUCH.lock().unwrap();
    if t.owner != owner {
        // A new stream: whatever an old one left down goes first.
        t.cancel_all();
        t.owner = owner;
    }
    t.apply(id, phase, x, y);
}

/// Call often (the input thread's loop): fingers resting on the client's
/// screen are told to Windows again before it gives up on them.
pub fn keep_alive() {
    let mut t = TOUCH.lock().unwrap();
    if !t.contacts.is_empty() && t.last.is_none_or(|l| l.elapsed() >= HOLD_REFRESH) {
        t.inject(None);
    }
}

/// Lift stream `owner`'s fingers (its client left mid-touch), without a tap.
pub fn release_all(owner: u64) {
    let mut t = TOUCH.lock().unwrap();
    if t.owner == owner {
        t.cancel_all();
    }
}
