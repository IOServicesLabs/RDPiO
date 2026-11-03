//! The host on battery: a hybrid laptop's virtual monitor rendered — and so
//! captured and encoded — on its integrated GPU, so the discrete one can
//! power off (switch `battigpu`, on by default). Measured on a Razer Blade
//! 15: the RTX 2070 alone drew 25-32 W of the laptop's 35-42 W streaming
//! office work, its encoder 1-7% busy. Plugged in, nothing changes: the
//! fastest GPU, as before.
//!
//! Decided when a stream starts (the sign-in's offer); a stream keeps its GPU
//! until it ends — moving it means a new virtual monitor (the driver takes
//! a render GPU only when it adds one), see docs/hyper.md.

use std::sync::Mutex;
use std::time::Duration;

use rdp_gpu::h264::Codec;

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
    /// Plugged in (or a desktop): the fastest GPU, as always.
    Plugged,
    SwitchOff,
    NoIntegratedEncoder,
    /// The integrated GPU's encoder failed its trial (once: for good).
    TrialFailed,
    /// The client doesn't decode H.264.
    ClientCodec,
    Integrated,
}

fn decide(on_battery: bool, switch: bool, found: bool, trial_ok: impl FnOnce() -> bool, client_h264: bool) -> Decision {
    match () {
        _ if !on_battery => Decision::Plugged,
        _ if !switch => Decision::SwitchOff,
        _ if !found => Decision::NoIntegratedEncoder,
        _ if !client_h264 => Decision::ClientCodec,
        _ if !trial_ok() => Decision::TrialFailed,
        _ => Decision::Integrated,
    }
}

/// Whether the stream about to start runs on the integrated GPU (`Some`)
/// or the fastest one, as plugged in. `client_codecs`: what the client
/// decodes. Logs the choice and why, once a stream.
pub(crate) fn plan(client_codecs: &[String]) -> Option<Plan> {
    let on_battery = rdp_gpu::hyper::battery::on_battery();
    // (Nothing more is looked at plugged in: AC behaves exactly as before.)
    let found = if on_battery { rdp_gpu::hyper::battery::battery_gpu(&[CODEC]) } else { None };
    let client_h264 = client_codecs.iter().any(|c| Codec::from_name(c) == Some(CODEC));
    let decision = decide(
        on_battery,
        on_battery && crate::host_ab::ab("battigpu", true),
        found.is_some(),
        || found.as_ref().is_some_and(|g| trial(g.luid)),
        client_h264,
    );
    let gpu = found.as_ref().map_or("", |g| g.name.as_str());
    match decision {
        Decision::Plugged => {
            tracing::debug!("plugged in: the virtual monitor renders and encodes on the fastest GPU");
            None
        }
        Decision::Integrated => {
            let g = found?;
            let plan = Plan { luid: g.luid, gpu: g.name, encoder: g.encoders.first().cloned().unwrap_or_default() };
            tracing::info!(gpu = %plan.gpu, encoder = %plan.encoder, codec = CODEC.name(), max_fps = MAX_FPS,
                "on battery: the virtual monitor renders and encodes on the integrated GPU (the discrete one can power off)");
            Some(plan)
        }
        why => {
            tracing::info!(?why, integrated = gpu, "on battery, but staying on the fastest GPU");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugged_in_nothing_changes_and_nothing_is_tried() {
        let tried = std::cell::Cell::new(false);
        let d = decide(false, true, true, || { tried.set(true); true }, true);
        assert_eq!(d, Decision::Plugged);
        assert!(!tried.get());
    }

    #[test]
    fn on_battery_the_integrated_gpu_when_everything_holds() {
        assert_eq!(decide(true, true, true, || true, true), Decision::Integrated);
        assert_eq!(decide(true, false, true, || true, true), Decision::SwitchOff);
        assert_eq!(decide(true, true, false, || true, true), Decision::NoIntegratedEncoder);
        assert_eq!(decide(true, true, true, || true, false), Decision::ClientCodec);
        assert_eq!(decide(true, true, true, || false, true), Decision::TrialFailed);
    }
}
