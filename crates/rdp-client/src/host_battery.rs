//! The host on battery: a hybrid laptop's virtual monitor rendered — and so
//! captured and encoded — on its integrated GPU, so the discrete one can
//! power off (switch `battigpu`, on by default). Measured on a Razer Blade
//! 15: the RTX 2070 alone drew 25-32 W of the laptop's 35-42 W streaming
//! office work, its encoder 1-7% busy. Plugged in, nothing changes: the
//! fastest GPU, as before.
//!
//! Which GPU is the This PC page's "Host GPU" ([`HostGpu`]): Auto (the
//! default) as above; Fastest always the discrete GPU (as before there was a
//! choice); Power saving always the integrated one, plugged in too.
//! `battigpu=0` overrides all three: the fastest GPU, and nothing moves.
//!
//! Decided when a stream starts (the sign-in's offer): the GPU, and with it
//! the codec and the refresh rate. In Auto a stream follows the power cable
//! ([`move_due`]): once the new source has held for [`SETTLE`], the stream
//! ends with a Reconnect and the client's new sign-in decides again — not a
//! move in place, since the driver takes a render GPU only as it adds a
//! monitor, and the codec changes with the GPU (HEVC on NVIDIA, H.264 on
//! Intel), which no client can do mid-stream. See docs/hyper.md.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rdp_gpu::h264::Codec;

use crate::hyper_direct::HostGpu;

/// The codec on the integrated GPU: H.264. Measured on the UHD 630 through
/// Media Foundation (synthetic office frames, 1 ms timer): Quick Sync H.264
/// codes a 1440p frame in ~8 ms (1080p ~7), where Intel's HEVC encoder takes
/// 34-54 ms when it works at all — with the host's low-latency settings it
/// fails on the first picture (E_FAIL).
pub(crate) const CODEC: Codec = Codec::H264;

/// The refresh rate the stream is capped at on the integrated GPU: Quick
/// Sync refuses 240 fps outright, and every frame it doesn't render or code
/// is battery kept. (The stream measured on battery delivered 9 fps median.)
pub(crate) const MAX_FPS: u32 = 60;

/// The integrated GPU to run a stream on.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub luid: i64,
    pub gpu: String,
    pub encoder: String,
}

/// What [`plan`] decides from what it found.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Plugged in (or a desktop), in Auto: the fastest GPU, as always.
    Plugged,
    /// Host GPU: Fastest.
    FastestChosen,
    SwitchOff,
    NoIntegratedEncoder,
    /// The integrated GPU's encoder failed its trial (once: for good).
    TrialFailed,
    /// The client doesn't decode H.264 (or the stream is too big for it).
    ClientCodec,
    Integrated,
}

/// Whether `setting` wants the integrated GPU now.
fn wants_integrated(setting: HostGpu, on_battery: bool) -> bool {
    match setting {
        HostGpu::Auto => on_battery,
        HostGpu::Fastest => false,
        HostGpu::PowerSaving => true,
    }
}

fn decide(setting: HostGpu, on_battery: bool, switch: bool, found: bool, trial_ok: impl FnOnce() -> bool, client_h264: bool) -> Decision {
    match () {
        _ if setting == HostGpu::Fastest => Decision::FastestChosen,
        _ if !wants_integrated(setting, on_battery) => Decision::Plugged,
        _ if !switch => Decision::SwitchOff,
        _ if !found => Decision::NoIntegratedEncoder,
        _ if !client_h264 => Decision::ClientCodec,
        _ if !trial_ok() => Decision::TrialFailed,
        _ => Decision::Integrated,
    }
}

/// Whether a client decoding `client_codecs` takes the integrated GPU's codec.
pub(crate) fn decodes(client_codecs: &[String]) -> bool {
    client_codecs.iter().any(|c| Codec::from_name(c) == Some(CODEC))
}

/// [`decide`] with what it needs found out — only what it needs: where the
/// fastest GPU is wanted (plugged in, in Auto), nothing is listed, tried or
/// read, so AC behaves exactly as before.
fn look(setting: HostGpu, on_battery: bool, client_h264: bool) -> (Decision, Option<rdp_gpu::hyper::battery::BatteryGpu>) {
    let wanted = wants_integrated(setting, on_battery);
    let found = if wanted { rdp_gpu::hyper::battery::battery_gpu(&[CODEC]) } else { None };
    let decision = decide(
        setting,
        on_battery,
        wanted && crate::host_ab::ab("battigpu", true),
        found.is_some(),
        || found.as_ref().is_some_and(|g| trial(g.luid)),
        client_h264,
    );
    (decision, found)
}

/// Whether the stream about to start runs on the integrated GPU (`Some`)
/// or the fastest one. `client_codecs`: what the client decodes; `setting`:
/// this PC's Host GPU. Logs the choice and why, once a stream.
pub(crate) fn plan(client_codecs: &[String], setting: HostGpu) -> Option<Plan> {
    let on_battery = rdp_gpu::hyper::battery::on_battery();
    let (decision, found) = look(setting, on_battery, decodes(client_codecs));
    let gpu = found.as_ref().map_or("", |g| g.name.as_str());
    match decision {
        Decision::Plugged => {
            tracing::debug!("plugged in: the virtual monitor renders and encodes on the fastest GPU");
            None
        }
        Decision::FastestChosen => {
            tracing::debug!("Host GPU is set to Fastest: the virtual monitor renders and encodes on the fastest GPU");
            None
        }
        Decision::Integrated => {
            let g = found?;
            let plan = Plan { luid: g.luid, gpu: g.name, encoder: g.encoders.first().cloned().unwrap_or_default() };
            tracing::info!(?setting, on_battery, gpu = %plan.gpu, encoder = %plan.encoder, codec = CODEC.name(), max_fps = MAX_FPS,
                "the virtual monitor renders and encodes on the integrated GPU (the discrete one can power off)");
            Some(plan)
        }
        why => {
            tracing::info!(?why, ?setting, on_battery, integrated = gpu, "the integrated GPU is wanted, but staying on the fastest GPU");
            None
        }
    }
}

/// Whether the encoder on GPU `luid` codes a 1080p H.264 stream, tried once
/// for the agent's life (a few hundred ms): an integrated encoder that fails
/// would end every stream at once, as Media Foundation on the wrong GPU did.
fn trial(luid: i64) -> bool {
    static TRIED: Mutex<Option<(i64, bool)>> = Mutex::new(None);
    let mut tried = TRIED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((l, ok)) = *tried {
        if l == luid {
            return ok;
        }
    }
    let result = rdp_gpu::hyper::battery::encodes(luid, CODEC, (1920, 1080), MAX_FPS);
    match &result {
        Ok(took) => tracing::info!(ms = took.as_secs_f64() * 1e3, "the integrated GPU's encoder takes a stream"),
        Err(e) => tracing::warn!(error = %e, "the integrated GPU's encoder failed its trial; streams stay on the fastest GPU"),
    }
    let ok = result.is_ok_and(|took| took < TRIAL_SLOWEST);
    *tried = Some((luid, ok));
    ok
}

/// A trial frame slower than this: the encoder can't keep a stream up.
const TRIAL_SLOWEST: Duration = Duration::from_millis(50);

/// How long a new power source must hold before a running stream moves to
/// it: a cable wiggled, a dock that drops for a moment or a charger that
/// stops and starts doesn't bounce the stream (each move costs ~2-3 s of
/// picture, see docs/hyper.md).
pub(crate) const SETTLE: Duration = Duration::from_secs(10);

/// How often a stream looks at the power source (one cheap call).
const POLL: Duration = Duration::from_secs(1);

/// The debounce: the GPU a stream is on, and since when the power source
/// has wanted the other one.
#[derive(Debug)]
struct Settle {
    on_integrated: bool,
    differs_since: Option<Instant>,
}

impl Settle {
    /// Whether to move now, the power source wanting the integrated GPU
    /// (`want`) at `now`. Any look that agrees with where the stream is
    /// starts the wait over.
    fn step(&mut self, want: bool, now: Instant) -> bool {
        if want == self.on_integrated {
            self.differs_since = None;
            return false;
        }
        now.saturating_duration_since(*self.differs_since.get_or_insert(now)) >= SETTLE
    }
}

/// The running stream's watch: set by the agent around a stream on a virtual
/// monitor, in Auto, on a hybrid machine, `battigpu` on.
struct Watch {
    settle: Settle,
    /// The client decodes H.264 and the stream fits Quick Sync: a move to
    /// the integrated GPU would land there.
    client_fits: bool,
    looked: Instant,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);
/// (The stream's loop checks this every frame; the lock only once armed.)
static ARMED: AtomicBool = AtomicBool::new(false);

/// Where a stream moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Move {
    ToIntegrated,
    ToFastest,
}

/// A stream is about to start on a virtual monitor with Host GPU `setting`,
/// on the integrated GPU or not; `client_fits`: it could go there. Only Auto
/// follows the cable (the other two don't depend on it), only on a hybrid
/// machine, and not with `battigpu=0` (the fastest GPU, and nothing moves).
pub(crate) fn watch(setting: HostGpu, on_integrated: bool, client_fits: bool) {
    unwatch();
    if setting != HostGpu::Auto || !crate::host_ab::ab("battigpu", true) {
        return;
    }
    // (Listing the adapters wakes no GPU: no device is made.)
    if rdp_gpu::hyper::battery::frugal(&rdp_gpu::hyper::battery::adapters()).is_none() {
        return;
    }
    tracing::debug!(on_integrated, client_fits, settle_s = SETTLE.as_secs(), "the stream follows the power source between the GPUs");
    *WATCH.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Watch { settle: Settle { on_integrated, differs_since: None }, client_fits, looked: Instant::now() });
    ARMED.store(true, Ordering::Relaxed);
}

/// The stream ended: nothing to watch.
pub(crate) fn unwatch() {
    ARMED.store(false, Ordering::Relaxed);
    WATCH.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// From the stream's loop: `Some` — once — when the power source changed and
/// held, and the stream should end so the client signs in again on the other
/// GPU. One move a stream: the watch is gone once it fires (the new stream
/// watches afresh), so a move never starts while one is under way.
pub(crate) fn move_due() -> Option<Move> {
    if !ARMED.load(Ordering::Relaxed) {
        return None;
    }
    let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    let w = guard.as_mut()?;
    let now = Instant::now();
    if now.duration_since(w.looked) < POLL {
        return None;
    }
    w.looked = now;
    let on_battery = rdp_gpu::hyper::battery::on_battery();
    if !w.settle.step(on_battery, now) {
        return None;
    }
    let client_fits = w.client_fits;
    *guard = None;
    ARMED.store(false, Ordering::Relaxed);
    drop(guard);
    // (In Auto the integrated GPU is wanted exactly on battery.)
    let to = if on_battery { Move::ToIntegrated } else { Move::ToFastest };
    // Only where the new sign-in would land there: a stream it would put
    // back on the fastest GPU (a client without H.264, a failed trial — the
    // first may run now, a few hundred ms on this thread) would end again
    // every 10 s for nothing.
    if to == Move::ToIntegrated {
        let (why, _) = look(HostGpu::Auto, true, client_fits);
        if why != Decision::Integrated {
            tracing::info!(?why, "on battery and it held, but the integrated GPU can't take this stream: it stays on the fastest GPU");
            return None;
        }
    }
    tracing::info!(?to, held_s = SETTLE.as_secs(), on_battery,
        "the power source changed and held: moving the stream to the other GPU (the client signs in again)");
    Some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugged_in_nothing_changes_and_nothing_is_tried() {
        let tried = std::cell::Cell::new(false);
        let d = decide(HostGpu::Auto, false, true, true, || { tried.set(true); true }, true);
        assert_eq!(d, Decision::Plugged);
        assert!(!tried.get());
    }

    #[test]
    fn on_battery_the_integrated_gpu_when_everything_holds() {
        let auto = HostGpu::Auto;
        assert_eq!(decide(auto, true, true, true, || true, true), Decision::Integrated);
        assert_eq!(decide(auto, true, false, true, || true, true), Decision::SwitchOff);
        assert_eq!(decide(auto, true, true, false, || true, true), Decision::NoIntegratedEncoder);
        assert_eq!(decide(auto, true, true, true, || true, false), Decision::ClientCodec);
        assert_eq!(decide(auto, true, true, true, || false, true), Decision::TrialFailed);
    }

    #[test]
    fn the_setting_overrides_the_power_source() {
        // Fastest: never the integrated GPU, nothing tried.
        assert_eq!(decide(HostGpu::Fastest, true, true, true, || panic!("tried"), true), Decision::FastestChosen);
        // Power saving: the integrated GPU plugged in too — still only where it works.
        assert_eq!(decide(HostGpu::PowerSaving, false, true, true, || true, true), Decision::Integrated);
        assert_eq!(decide(HostGpu::PowerSaving, false, true, true, || false, true), Decision::TrialFailed);
        // The switch off (`battigpu=0`) wins over Power saving.
        assert_eq!(decide(HostGpu::PowerSaving, false, false, true, || true, true), Decision::SwitchOff);
    }

    #[test]
    fn a_power_change_moves_the_stream_only_once_it_held() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // On the fastest GPU, unplugged at 0 s: nothing until 10 s.
        let mut s = Settle { on_integrated: false, differs_since: None };
        assert!(!s.step(true, at(0)));
        assert!(!s.step(true, at(9)));
        assert!(s.step(true, at(10)));
        // A brief unplug (back at 5 s) starts the wait over.
        let mut s = Settle { on_integrated: false, differs_since: None };
        assert!(!s.step(true, at(0)));
        assert!(!s.step(false, at(5)));
        assert!(!s.step(true, at(6)));
        assert!(!s.step(true, at(15)));
        assert!(s.step(true, at(16)));
        // On the integrated GPU: plugging in moves it back, as long after.
        let mut s = Settle { on_integrated: true, differs_since: None };
        assert!(!s.step(true, at(0)));
        assert!(!s.step(false, at(1)));
        assert!(s.step(false, at(11)));
    }
}
