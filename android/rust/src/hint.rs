//! Performance hints (ADPF, Android 13+): a thread tells Android how long
//! each frame's work took against its deadline, and the scheduler sets the
//! CPU's clock to just meet it — lower clocks while the work is light (the
//! battery), a boost as soon as it runs late (no missed frames), instead of
//! the governor guessing from load after the fact.
//!
//! Looked up at run time (the app runs on Android 11 too); the newer calls
//! (a workload spike: Android 16, power efficiency: Android 15) only where
//! the device has them.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;
use std::time::Duration;

type GetManager = unsafe extern "C" fn() -> *mut c_void;
type CreateSession = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type Report = unsafe extern "C" fn(*mut c_void, i64) -> i32;
type UpdateTarget = unsafe extern "C" fn(*mut c_void, i64) -> i32;
type Close = unsafe extern "C" fn(*mut c_void);
type NotifySpike = unsafe extern "C" fn(*mut c_void, bool, bool, *const c_char) -> i32;
type PreferEfficiency = unsafe extern "C" fn(*mut c_void, bool) -> i32;

struct Api {
    manager: usize,
    create: CreateSession,
    report: Report,
    update: UpdateTarget,
    close: Close,
    spike: Option<NotifySpike>,
    efficiency: Option<PreferEfficiency>,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let sym = |name: &std::ffi::CStr| {
            let p = libc::dlsym(lib, name.as_ptr());
            (!p.is_null()).then_some(p)
        };
        let get: GetManager = std::mem::transmute(sym(c"APerformanceHint_getManager")?);
        let manager = get();
        if manager.is_null() {
            return None;
        }
        Some(Api {
            manager: manager as usize,
            create: std::mem::transmute::<*mut c_void, CreateSession>(sym(c"APerformanceHint_createSession")?),
            report: std::mem::transmute::<*mut c_void, Report>(sym(c"APerformanceHint_reportActualWorkDuration")?),
            update: std::mem::transmute::<*mut c_void, UpdateTarget>(sym(c"APerformanceHint_updateTargetWorkDuration")?),
            close: std::mem::transmute::<*mut c_void, Close>(sym(c"APerformanceHint_closeSession")?),
            spike: sym(c"APerformanceHint_notifyWorkloadSpike").map(|p| std::mem::transmute::<*mut c_void, NotifySpike>(p)),
            efficiency: sym(c"APerformanceHint_setPreferPowerEfficiency").map(|p| std::mem::transmute::<*mut c_void, PreferEfficiency>(p)),
        })
    })
    .as_ref()
}

/// One hint session, for the calling thread (made and used on it).
pub struct Hint {
    session: *mut c_void,
    target: Duration,
    /// Asked for power efficiency (see [`Hint::prefer_efficiency`]).
    efficient: bool,
}

impl Hint {
    /// None when the device has none.
    pub fn for_this_thread(target: Duration) -> Option<Self> {
        let api = api()?;
        let tid = unsafe { libc::gettid() };
        let session = unsafe { (api.create)(api.manager as *mut c_void, &tid, 1, target.as_nanos() as i64) };
        if session.is_null() {
            log::info!("performance hints: no session");
            return None;
        }
        log::info!(
            "performance hints: on, {:.1} ms a frame (workload spikes: {}, power efficiency: {})",
            target.as_secs_f64() * 1000.0,
            if api.spike.is_some() { "yes" } else { "no" },
            if api.efficiency.is_some() { "yes" } else { "no" }
        );
        Some(Self { session, target, efficient: false })
    }

    /// The frame's work took `work`.
    pub fn report(&self, work: Duration) {
        if let Some(api) = api() {
            unsafe { (api.report)(self.session, (work.as_nanos() as i64).max(1)) };
        }
    }

    /// A new frame interval (the stream's rate changed).
    pub fn set_target(&mut self, target: Duration) {
        if target != self.target && !target.is_zero() {
            self.target = target;
            if let Some(api) = api() {
                unsafe { (api.update)(self.session, target.as_nanos() as i64) };
            }
        }
    }
}

impl Hint {
    /// A big frame (a keyframe) is about to be worked on: the clocks go up
    /// now, before it, rather than after it ran late. Whether the device took
    /// it (Android 16).
    pub fn spike(&self) -> bool {
        match api().and_then(|a| a.spike) {
            Some(spike) => unsafe { spike(self.session, true, true, c"keyframe".as_ptr()) == 0 },
            None => false,
        }
    }

    /// Lower clocks for the same deadlines where the work allows (office
    /// mode), or not (Android 15). Only a change goes to the system.
    pub fn prefer_efficiency(&mut self, on: bool) {
        if on == self.efficient {
            return;
        }
        self.efficient = on;
        if let Some(set) = api().and_then(|a| a.efficiency) {
            let status = unsafe { set(self.session, on) };
            log::info!("performance hints: power efficiency {}{}", if on { "on" } else { "off" }, if status == 0 { "" } else { " (refused)" });
        }
    }
}

impl Drop for Hint {
    fn drop(&mut self) {
        if let Some(api) = api() {
            unsafe { (api.close)(self.session) };
        }
    }
}
