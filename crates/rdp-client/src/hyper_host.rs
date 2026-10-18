//! Hyper Accelerator host agent (`rdpio --hyper-host`).
//!
//! Runs in the user's own session, elevated (`--hyper-install` registers a
//! logon task for that). It sleeps until the session is being used over RDP,
//! then offers Hyper to the rdpio client over the private `RDPIO.HYPER`
//! channel. Once the client has proved the UDP path works, the agent moves the
//! session off RDP and onto the physical console (`tscon … /dest:console`) —
//! that is what escapes RDP's 60 Hz virtual display — raises the monitor to
//! the refresh rate the client asked for, and streams it: Desktop Duplication
//! → GPU colour conversion → hardware H.264 → FEC + AES-GCM over UDP. Client
//! input is injected with `SendInput`. When the client leaves (or goes quiet)
//! the monitor's mode is restored and the PC is locked.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdp_gpu::hyper::{list_monitors, DesktopCapture, HwEncoder};
use rdp_hyper::control::{self, Control, Handshake, CHANNEL_NAME, PROTOCOL};
use rdp_hyper::input::{InputEvent, InputReceiver};
use rdp_hyper::rate::{Limits, RateController};
use rdp_hyper::video::{packetize_data, parity, OutFrame};
use rdp_hyper::wire::{random_key, Direction, Kind, Session};
use windows::core::PCSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSVirtualChannelClose, WTSVirtualChannelOpenEx, WTSVirtualChannelRead,
    WTSVirtualChannelWrite, WTS_CHANNEL_OPTION_DYNAMIC, WTS_CURRENT_SESSION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

/// Host agent settings.
#[derive(Debug, Clone)]
pub struct HostOptions {
    pub port: u16,
    /// Cap on the refresh rate the agent will switch the monitor to.
    pub max_fps: Option<u32>,
    /// Bitrate ceiling.
    pub max_mbps: u32,
    /// Encode at this size instead of the desktop's (scaled on the GPU).
    pub stream_size: Option<(u32, u32)>,
    /// The client's screen: stream the desktop scaled down to fit it (never
    /// up). Per client; `stream_size` wins.
    pub fit_to: Option<(u32, u32)>,
    /// Start in office mode (the client switches with `Control::Mode`).
    pub office: bool,
    /// Office mode with the encoder's fast preset.
    pub fast_text: bool,
    /// Leave all host monitors on even when the client has fewer.
    pub keep_monitors: bool,
}

/// How a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// The client said goodbye (window closed).
    Bye,
    /// The client went quiet (network drop) — it may come back.
    Lost,
    /// The console moved to another session (a sign-out, a user switch): the
    /// agent there takes over, and the client reconnects to it.
    Moved,
}

/// While a stream runs, what the host changed is written here, so a crashed
/// agent's successor can put it back (and lock the console) on start.
fn marker_path() -> std::path::PathBuf {
    crate::hyper_direct::data_dir().join("hyper-streaming.json")
}

fn mark_streaming() {
    // Each role's default (console, multimedia, communications), in
    // `wasapi::ROLES` order, and the session that's unlocked meanwhile — with
    // its sign-in time: after a restart the same session number can be a
    // fresh sign-in, which mustn't be locked for an old stream.
    let session = current_session_id();
    let doc = serde_json::json!({
        "output": crate::wasapi::default_ids(crate::wasapi::Flow::Render),
        "mic": crate::wasapi::default_ids(crate::wasapi::Flow::Capture),
        "session": session,
        "logon": session_info(session).map_or(0, |i| i.logon_time),
    });
    let _ = std::fs::write(marker_path(), doc.to_string());
    // If this process dies mid-stream, the console it unlocked is locked on
    // the way down (the next agent restores the rest from the marker).
    crate::crash::set_last_words(Some(lock_session));
}

/// The console is restored and locked (or handed back): nothing left for a
/// successor to recover. Callers of [`stream`] call this once they're done —
/// not `stream` itself, so a crash while the displays are put back and the
/// console locked is still recovered.
pub(crate) fn clear_streaming() {
    crate::crash::set_last_words(None);
    let _ = std::fs::remove_file(marker_path());
}

/// A previous agent died mid-stream: restore the audio devices and display
/// layout it changed, and lock the console it left unlocked.
pub(crate) fn recover_after_crash() {
    let Ok(text) = std::fs::read_to_string(marker_path()) else { return };
    tracing::warn!("previous Hyper stream didn't end cleanly; restoring the host and locking it");
    if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
        for key in ["output", "mic"] {
            match doc.get(key) {
                // A marker from an older build: one id for every role.
                Some(serde_json::Value::String(id)) => {
                    let _ = crate::wasapi::set_default_endpoint(id);
                }
                Some(v) => {
                    if let Ok(ids) = serde_json::from_value::<Vec<Option<String>>>(v.clone()) {
                        let _ = crate::wasapi::restore_default_ids(&ids);
                    }
                }
                None => {}
            }
        }
    }
    unsafe {
        use windows::Win32::Devices::Display::*;
        let _ = SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT);
        let _ = windows::Win32::Graphics::Gdi::ChangeDisplaySettingsExW(
            windows::core::PCWSTR::null(),
            None,
            None,
            windows::Win32::Graphics::Gdi::CDS_TYPE(0),
            None,
        );
    }
    // Lock the session the stream left unlocked: ours after a crash and
    // restart, or — if the console has since moved on (a sign-out, another
    // user) — that one, never the newcomer's.
    let doc = serde_json::from_str::<serde_json::Value>(&text).ok();
    let streamed = doc.as_ref().and_then(|d| d.get("session")?.as_u64()).map(|s| s as u32);
    let logon = doc.as_ref().and_then(|d| d.get("logon")?.as_i64());
    // Still the sign-in that was streamed (not a fresh one after a restart).
    let same_logon = |s: u32| logon.is_some_and(|l| l != 0 && session_info(s).is_some_and(|i| i.logon_time == l));
    match streamed {
        Some(s) if !same_logon(s) => tracing::info!("the interrupted stream's sign-in is gone (a restart?); nothing to lock"),
        Some(s) if s != current_session_id() => unsafe {
            use windows::Win32::System::RemoteDesktop::{WTSDisconnectSession, WTS_CURRENT_SERVER_HANDLE};
            let _ = WTSDisconnectSession(Some(WTS_CURRENT_SERVER_HANDLE), s, false);
        },
        _ => lock_session(),
    }
    clear_streaming();
}

/// Lock the console. LockWorkStation can report success without locking
/// anything (as SYSTEM it depends on the desktop it's called from), so the
/// session's state is checked; a console that didn't lock is disconnected
/// instead — the sign-in screen shows either way, and the user's programs
/// keep running.
pub(crate) fn lock_session() {
    use windows::Win32::System::RemoteDesktop::{WTSDisconnectSession, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION};
    let session = current_session_id();
    if console_moved() {
        tracing::info!("this session isn't on the console now; nothing to lock");
        return;
    }
    let info = session_info(session);
    if info.as_ref().is_some_and(|i| i.locked == Some(true)) {
        tracing::info!("console already locked");
        return;
    }
    // The sign-in screen itself (nobody signed in): nothing to lock.
    if info.as_ref().is_some_and(|i| i.logon_time == 0) {
        return;
    }
    // As the signed-in user: SYSTEM's own LockWorkStation reports success
    // without locking. (The RDP-handoff agent is that user: it can't get the
    // token, and locks directly.)
    let requested = match crate::hyper_direct::lock_as_user(session) {
        Ok(()) => true,
        Err(_) => unsafe { windows::Win32::System::Shutdown::LockWorkStation().is_ok() },
    };
    if requested {
        let until = Instant::now() + Duration::from_secs(4);
        while Instant::now() < until {
            if session_info(session).and_then(|i| i.locked) == Some(true) {
                tracing::info!("console locked");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    tracing::warn!(requested, "the console didn't lock; disconnecting the session instead (the sign-in screen shows)");
    unsafe {
        let _ = WTSDisconnectSession(Some(WTS_CURRENT_SERVER_HANDLE), WTS_CURRENT_SESSION, false);
    }
}

/// The physical console is another session's now (a sign-out, a user switch,
/// this one disconnected).
pub(crate) fn console_moved() -> bool {
    unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() != current_session_id() }
}

/// A session's lock state (`None`: Windows won't say) and sign-in time (0:
/// nobody signed in).
struct SessionInfo {
    locked: Option<bool>,
    logon_time: i64,
}

fn session_info(session: u32) -> Option<SessionInfo> {
    use windows::Win32::System::RemoteDesktop::*;
    unsafe {
        let mut buf = windows::core::PWSTR::null();
        let mut len = 0u32;
        WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), session, WTSSessionInfoEx, &mut buf, &mut len).ok()?;
        if buf.is_null() {
            return None;
        }
        let info = buf.0 as *const WTSINFOEXW;
        let level1 = (len as usize >= std::mem::size_of::<WTSINFOEXW>() && (*info).Level == 1).then(|| {
            let l = &(*info).Data.WTSInfoExLevel1;
            (l.SessionFlags as u32, l.LogonTime)
        });
        WTSFreeMemory(buf.0 as *mut core::ffi::c_void);
        let (flags, logon_time) = level1?;
        let locked = match flags {
            WTS_SESSIONSTATE_LOCK => Some(true),
            WTS_SESSIONSTATE_UNLOCK => Some(false),
            _ => None,
        };
        Some(SessionInfo { locked, logon_time })
    }
}

/// What to stream `device_name`'s desktop at: `--hyper-size`; else scaled
/// down to fit the client's screen (`fit_to`) — worked out each time the
/// pipeline opens, so a desktop that changes mode keeps its shape; else its
/// own size (`None`).
fn stream_size(opts: &HostOptions, device_name: &str) -> Option<(u32, u32)> {
    opts.stream_size.or_else(|| {
        let screen = opts.fit_to?;
        let m = list_monitors().ok()?.into_iter().find(|m| m.device_name == device_name)?;
        fit_within((m.width, m.height), screen)
    })
}

/// The size to offer a client: the host's `--hyper-size`; for a client that
/// asked for its screen's size, the desktop scaled down to fit it; else the
/// desktop's own size (`None`).
pub(crate) fn client_stream_size(opts: &HostOptions, desktop: (u32, u32), want: &rdp_hyper::auth::Want) -> Option<(u32, u32)> {
    opts.stream_size.or_else(|| if want.fit { fit_within(desktop, (want.width, want.height)) } else { None })
}

/// `desktop` scaled down to fit within `screen`, its aspect kept, in even
/// sizes (4:2:0 video needs them). `None` when it fits already, or for a
/// screen too small to be real.
pub(crate) fn fit_within(desktop: (u32, u32), screen: (u32, u32)) -> Option<(u32, u32)> {
    let ((dw, dh), (sw, sh)) = (desktop, screen);
    if dw == 0 || dh == 0 || sw < 320 || sh < 200 || (dw <= sw && dh <= sh) {
        return None;
    }
    let scale = (sw as f64 / dw as f64).min(sh as f64 / dh as f64);
    let even = |v: u32| ((v as f64 * scale).round() as u32 & !1).max(2);
    Some((even(dw), even(dh)))
}

/// No packet from the client for this long ends the stream.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(6);

pub fn run(opts: HostOptions) -> Result<(), Box<dyn std::error::Error>> {
    let elevated = is_elevated();
    tracing::info!(
        port = opts.port,
        elevated,
        "Hyper host agent running; waiting for an rdpio --hyper client over RDP"
    );
    recover_after_crash();
    if !elevated {
        tracing::warn!("not elevated: clients will be told to stay on plain RDP (run `rdpio --hyper-install` once, as administrator)");
    }
    loop {
        if !is_remote_session() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let Some(channel) = Channel::open() else {
            // This RDP client didn't accept the channel (mstsc, or rdpio
            // without --hyper). Check again later in case it reconnects.
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };
        tracing::info!("rdpio client opened the Hyper channel");
        if let Err(e) = serve_client(channel, &opts, elevated) {
            tracing::warn!(error = %e, "Hyper session ended with an error");
        }
        // Don't re-offer to the same RDP connection in a loop.
        let until = Instant::now() + Duration::from_secs(10);
        while is_remote_session() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

fn serve_client(channel: Channel, opts: &HostOptions, elevated: bool) -> Result<(), Box<dyn std::error::Error>> {
    let Some(Handshake::ClientHello { protocol, codecs, width, height, refresh_hz, monitors }) =
        channel.read(Duration::from_secs(10)).and_then(|b| Handshake::parse(&b))
    else {
        return Err("no ClientHello".into());
    };
    tracing::info!(protocol, ?codecs, width, height, refresh_hz, "Hyper client hello");
    let decline = |reason: String| {
        tracing::warn!(%reason, "declining Hyper");
        channel.write(&Handshake::HostDecline { reason }.to_bytes());
    };
    if protocol != PROTOCOL {
        decline(format!("protocol {protocol} not supported (host speaks {PROTOCOL})"));
        return Ok(());
    }
    let monitor = list_monitors()?.into_iter().next().ok_or("no monitor attached")?;
    // The stream at the client's size: the desktop scaled down to fit it.
    let opts = &HostOptions { fit_to: Some((width, height)), ..opts.clone() };
    let (stream_w, stream_h) = stream_size(opts, &monitor.device_name).unwrap_or((monitor.width, monitor.height));
    let Some(codec) = rdp_gpu::hyper::choose_codec(&codecs, (stream_w, stream_h)) else {
        decline(format!("no common codec (client decodes {codecs:?})"));
        return Ok(());
    };
    if !elevated {
        decline("the host agent isn't elevated; run `rdpio --hyper-install` on the host as administrator".into());
        return Ok(());
    }
    let socket = match UdpSocket::bind(("0.0.0.0", opts.port)) {
        Ok(s) => {
            crate::hyper_client::set_socket_buffers(&s);
            s
        }
        Err(e) => {
            decline(format!("UDP port {} unavailable: {e}", opts.port));
            return Ok(());
        }
    };
    let target_fps = opts.max_fps.map_or(refresh_hz, |cap| refresh_hz.min(cap)).max(30);
    let key = random_key();
    channel.write(
        &Handshake::HostOffer {
            protocol: PROTOCOL,
            port: opts.port,
            key: control::encode_key(&key),
            width: stream_w,
            height: stream_h,
            fps: target_fps,
            codec: codec.name().into(),
        }
        .to_bytes(),
    );

    // The client proves the UDP path with an authenticated ping.
    let mut recv = Session::new(&key, Direction::HostToClient);
    let send = Arc::new(Mutex::new(Session::new(&key, Direction::HostToClient)));
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut peer = None;
    while peer.is_none() && Instant::now() < deadline {
        let mut buf = [0u8; 2048];
        let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
        if let Ok((Kind::Control, body)) = recv.open(&buf[..n]) {
            if let Some(Control::Ping { id, client_us }) = Control::parse(&body) {
                let pong = Control::Pong { id, client_us, host_us: qpc_us() };
                let _ = socket.send_to(&send.lock().unwrap().seal(Kind::Control, &pong.to_bytes()), from);
                peer = Some(from);
            }
        }
    }
    let Some(peer) = peer else {
        tracing::warn!("client never reached UDP port {}; staying on RDP (firewall?)", opts.port);
        return Ok(());
    };
    tracing::info!(%peer, "UDP path to the client confirmed");
    // Keep answering pings until the client says go.
    socket.set_read_timeout(Some(Duration::from_millis(20)))?;
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while !ready && Instant::now() < ready_deadline {
        if let Some(msg) = channel.read(Duration::from_millis(20)) {
            ready = matches!(Handshake::parse(&msg), Some(Handshake::ClientReady));
        }
        let mut buf = [0u8; 2048];
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            if let Ok((Kind::Control, body)) = recv.open(&buf[..n]) {
                if let Some(Control::Ping { id, client_us }) = Control::parse(&body) {
                    let pong = Control::Pong { id, client_us, host_us: qpc_us() };
                    let _ = socket.send_to(&send.lock().unwrap().seal(Kind::Control, &pong.to_bytes()), from);
                }
            }
        }
    }
    if !ready {
        return Err("client never sent ClientReady".into());
    }

    // Leave RDP: the session moves to the physical console, unlocked.
    let session_id = current_session_id();
    tracing::info!(session_id, "moving the session to the console (tscon)");
    let out = std::process::Command::new(crate::hyper_direct::system_tool("tscon.exe"))
        .args([session_id.to_string().as_str(), "/dest:console"])
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            decline(format!("tscon failed: {msg}"));
            return Ok(());
        }
        Err(e) => {
            decline(format!("tscon failed: {e}"));
            return Ok(());
        }
    }
    drop(channel);
    let until = Instant::now() + Duration::from_secs(10);
    while is_remote_session() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }

    // One monitor is streamed: the others go dark meanwhile, whatever the
    // client has (nothing should open where nobody can see it).
    let _ = monitors;
    let consolidated = (!opts.keep_monitors).then(|| Consolidation::apply(1)).flatten();
    let display = DisplayMode::raise(&monitor.device_name, target_fps, opts.stream_size.is_none().then_some((width, height)));
    let result = stream(socket, peer, recv, send, opts, &monitor.device_name, display.as_ref().map_or(target_fps, |d| d.fps), codec, true);
    // The monitors are put back before the lock (a locked desktop can't
    // change the display layout).
    drop(display); // restores the monitor's own mode
    drop(consolidated);
    lock_session();
    clear_streaming();
    result.map(|_| ())
}

struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    rate: Mutex<RateController>,
    send: Arc<Mutex<Session>>,
    peer: Mutex<SocketAddr>,
    last_rx: Mutex<Instant>,
    input_ack: AtomicU32,
    status: Mutex<(u16, u16, u16)>,
    /// Sender-thread stats since the last report: µs spent sending frames,
    /// and how many.
    pace_us: AtomicU64,
    paced: AtomicU32,
    /// The client's window is minimized: encode nothing.
    paused: AtomicBool,
    /// The client wants office mode (crisp text) rather than game mode.
    office: AtomicBool,
    /// …with the encoder's fast preset.
    fast_text: AtomicBool,
    /// The client lost frames and asks for a repair (see [`Ltr`]).
    recover: AtomicBool,
    /// The newest long-term reference frame the client decoded (`u32::MAX`:
    /// none said yet).
    ltr_acked: AtomicU32,
    /// When the client's input last came (`qpc_us`; 0: none yet).
    last_input_us: AtomicU64,
    /// The refresh rate of the client's screen now (0: not said — an older
    /// client): the stream runs at most at that (see `Control::Refresh`).
    client_hz: AtomicU32,
    /// The client's camera as a virtual camera here (see `hyper_camera`):
    /// allowed in this stream (not the selftest), set up once, and running.
    camera_allowed: bool,
    camera_started: AtomicBool,
    camera: Mutex<Option<crate::hyper_camera::HostCamera>>,
}

/// A captured frame: converted already (Desktop Duplication), or only seen —
/// the driver's newest, converted when the stream takes it (see
/// `DesktopCapture::peek`), so frames the stream's rate skips cost nothing.
enum Captured {
    Now(rdp_gpu::hyper::CapturedFrame),
    Later { present_us: u64 },
}

impl Captured {
    fn present_us(&self) -> u64 {
        match self {
            Self::Now(f) => f.present_us,
            Self::Later { present_us } => *present_us,
        }
    }

    /// The picture, converted now if it wasn't (`None`: the driver's frame
    /// went missing meanwhile).
    fn into_frame(self, cap: &mut rdp_gpu::hyper::DesktopCapture) -> Option<rdp_gpu::hyper::CapturedFrame> {
        match self {
            Self::Now(f) => Some(f),
            Self::Later { .. } => cap.take().unwrap_or_else(|e| {
                tracing::debug!(error = %e, "a driver frame couldn't be converted");
                None
            }),
        }
    }
}

/// Office mode, nobody at the controls this long, and the screen changing
/// only in small ways (a clock, a caret, a spinner): frames go at most
/// [`TRICKLE_INTERVAL_US`] apart — the newest picture always gets there, and
/// the next input (or a big change) brings the full rate back at once. Each
/// frame spared is a wake-up, a decode and a present spared on the client:
/// on a laptop, its battery.
const TRICKLE_AFTER_INPUT_US: u64 = 300_000;
/// ~12 fps.
const TRICKLE_INTERVAL_US: u64 = 83_333;
/// An encoded frame this small changed little (at any resolution: a big
/// change costs tens of kilobytes).
const SMALL_FRAME: usize = 4096;
/// Small frames in a row before trickling.
const TRICKLE_STREAK: u32 = 3;
/// Office sharpening (re-encoding a still picture, ever finer) scales with
/// the change: a frame per this many bytes of it, up to a second's frames. A
/// caret blink then costs two frames, not a second of them.
const SHARPEN_BYTES_PER_FRAME: usize = 1024;

fn sharpen_frames(change_bytes: usize, fps: u32) -> u32 {
    ((change_bytes / SHARPEN_BYTES_PER_FRAME) as u32).clamp(2, fps.max(2))
}

/// Whether to trickle (see [`TRICKLE_AFTER_INPUT_US`]).
#[derive(Default)]
struct Trickle {
    /// Small new pictures in a row.
    small_streak: u32,
    on: bool,
}

impl Trickle {
    /// The frame interval to keep now, and whether trickling just ended (the
    /// next picture then goes at once).
    fn pace(&mut self, office: bool, since_input_us: u64, interval_us: u64) -> (u64, bool) {
        let on = office && since_input_us > TRICKLE_AFTER_INPUT_US && self.small_streak >= TRICKLE_STREAK;
        let ended = self.on && !on;
        self.on = on;
        (if on { interval_us.max(TRICKLE_INTERVAL_US) } else { interval_us }, ended)
    }

    /// A frame went out: a new picture's size says how much changed.
    fn encoded(&mut self, bytes: usize, refresh: bool, keyframe: bool) {
        if !refresh && !keyframe {
            self.small_streak = if bytes < SMALL_FRAME { self.small_streak.saturating_add(1) } else { 0 };
        }
    }
}

impl Shared {
    fn send(&self, socket: &UdpSocket, kind: Kind, body: &[u8]) {
        let peer = *self.peer.lock().unwrap();
        // Sealed under the lock, sent outside it: a normal-priority thread
        // preempted inside the send mustn't hold up the video and input-ack
        // threads behind the lock. Packets may then leave slightly out of
        // order — the client's replay window accepts 1024 of that.
        let packet = self.send.lock().unwrap().seal(kind, body);
        let _ = socket.send_to(&packet, peer);
    }

    fn send_status(&self, socket: &UdpSocket) {
        let (width, height, fps) = *self.status.lock().unwrap();
        let (bitrate_kbps, fec_permille) = {
            let r = self.rate.lock().unwrap();
            (r.bitrate_bps() / 1000, (r.fec() * 1000.0) as u16)
        };
        let status = Control::Status {
            input_ack: self.input_ack.load(Ordering::Relaxed),
            width,
            height,
            fps,
            bitrate_kbps,
            fec_permille,
        };
        self.send(socket, Kind::Control, &status.to_bytes());
    }
}

/// The NVIDIA GPU's graphics and video (encoder) clocks now, MHz (NVML,
/// which the driver installs; `None` elsewhere). For the log: an encoder on
/// idle clocks takes several times longer per frame.
fn gpu_clocks() -> Option<(u32, u32)> {
    type Init = unsafe extern "C" fn() -> i32;
    type ByIndex = unsafe extern "C" fn(u32, *mut *mut core::ffi::c_void) -> i32;
    type Clock = unsafe extern "C" fn(*mut core::ffi::c_void, u32, *mut u32) -> i32;
    struct Nvml {
        clock: Clock,
        device: usize,
    }
    static NVML: std::sync::OnceLock<Option<Nvml>> = std::sync::OnceLock::new();
    let nvml = NVML
        .get_or_init(|| unsafe {
            use windows::core::{HSTRING, PCSTR};
            use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
            // By full path from System32 (never a planted copy beside us).
            let lib = LoadLibraryW(&HSTRING::from(crate::hyper_direct::system_tool("nvml.dll").as_os_str())).ok()?;
            let init: Init = std::mem::transmute(GetProcAddress(lib, PCSTR(b"nvmlInit_v2\0".as_ptr()))?);
            let by_index: ByIndex = std::mem::transmute(GetProcAddress(lib, PCSTR(b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr()))?);
            let clock: Clock = std::mem::transmute(GetProcAddress(lib, PCSTR(b"nvmlDeviceGetClockInfo\0".as_ptr()))?);
            if init() != 0 {
                return None;
            }
            let mut device = std::ptr::null_mut();
            (by_index(0, &mut device) == 0).then_some(Nvml { clock, device: device as usize })
        })
        .as_ref()?;
    let read = |kind: u32| {
        let mut mhz = 0u32;
        (unsafe { (nvml.clock)(nvml.device as *mut _, kind, &mut mhz) } == 0).then_some(mhz)
    };
    // (NVML_CLOCK_GRAPHICS = 0, NVML_CLOCK_VIDEO = 3.)
    Some((read(0)?, read(3)?))
}

pub(crate) fn stream(
    socket: UdpSocket,
    peer: SocketAddr,
    recv: Session,
    send: Arc<Mutex<Session>>,
    opts: &HostOptions,
    device_name: &str,
    fps: u32,
    codec: rdp_gpu::h264::Codec,
    route_audio: bool,
) -> Result<StreamEnd, Box<dyn std::error::Error>> {
    // The marker outlives this: the caller clears it once the displays are
    // back and the console is locked (`clear_streaming`).
    if route_audio {
        mark_streaming();
    }
    let farewell = (socket.try_clone().ok(), send.clone());
    let result = stream_inner(socket, peer, recv, send, opts, device_name, fps, codec, route_audio);
    // A stream that failed (perhaps before it ever ran) says goodbye, so the
    // client doesn't wait out its silence timeout and sign in again.
    if let (Err(_), (Some(socket), send)) = (&result, farewell) {
        for _ in 0..3 {
            let packet = send.lock().unwrap().seal(Kind::Control, &Control::Bye.to_bytes());
            let _ = socket.send_to(&packet, peer);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn stream_inner(
    socket: UdpSocket,
    peer: SocketAddr,
    recv: Session,
    send: Arc<Mutex<Session>>,
    opts: &HostOptions,
    device_name: &str,
    fps: u32,
    codec: rdp_gpu::h264::Codec,
    route_audio: bool,
) -> Result<StreamEnd, Box<dyn std::error::Error>> {
    // Game sound to the client (not the host's speakers), and a virtual mic
    // for the client's microphone — restored when the stream ends.
    let routing = if route_audio {
        crate::hyper_audio::HostRouting::set_up()
    } else {
        crate::hyper_audio::HostRouting::none()
    };
    // A Wi-Fi host shouldn't nap or scan mid-stream either, and its packets
    // go out marked as audio/video.
    let _wifi = crate::net_tune::WifiLowLatency::enable();
    let _qos = crate::net_tune::QosFlow::tag(&socket, Some(peer));
    // This thread captures and encodes: scheduled like the game it streams.
    // The display stays on (a sleeping one gives no frames to capture) — and
    // one already asleep is woken.
    let _boost = crate::net_tune::ThreadBoost::games();
    let _awake = crate::net_tune::KeepAwake::new();
    // Media Foundation's waits (the encoder's too) at 1 ms, not 15.6 ms.
    let _timer = crate::net_tune::FineTimer::new();
    wake_display();
    let max_bps = opts.max_mbps.max(2) * 1_000_000;
    let (mut office, mut fast_text) = (opts.office, opts.fast_text);
    // What was agreed; the client's screen may later ask for less (a laptop
    // that drops to 60 Hz on battery), never for more.
    let agreed_fps = fps;
    let mut full_fps = fps;
    let (mut fps, mut tuning) = mode(office, fast_text, full_fps);
    // Office mode: the desktop at the client's own resolution, where the
    // monitor has it (crisp text, nothing scaled). Before the capture opens.
    let mut native = if office && route_audio { opts.fit_to.and_then(|s| NativeResolution::apply(device_name, s)) } else { None };
    let start_bps = if office { 30_000_000 } else { 20_000_000 };
    let limits = Limits { min_bps: 1_500_000, start_bps: start_bps.min(max_bps), max_bps };
    let (mut cap, mut enc) = open_pipeline(device_name, opts, fps, limits.start_bps, codec, tuning)?;
    // Office mode on the host's text: greyscale smoothing (not for a selftest).
    let mut grey_text = if office && route_audio { GreyText::apply() } else { None };
    let (w, h) = cap.stream;
    tracing::info!(width = w, height = h, fps, encoder = %enc.name, "Hyper stream starting");

    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(true),
        rate: Mutex::new(RateController::new(limits)),
        send,
        peer: Mutex::new(peer),
        last_rx: Mutex::new(Instant::now()),
        input_ack: AtomicU32::new(0),
        status: Mutex::new((w as u16, h as u16, fps as u16)),
        pace_us: AtomicU64::new(0),
        paced: AtomicU32::new(0),
        paused: AtomicBool::new(false),
        office: AtomicBool::new(opts.office),
        fast_text: AtomicBool::new(opts.fast_text),
        recover: AtomicBool::new(false),
        ltr_acked: AtomicU32::new(u32::MAX),
        last_input_us: AtomicU64::new(0),
        client_hz: AtomicU32::new(0),
        camera_allowed: route_audio,
        camera_started: AtomicBool::new(false),
        camera: Mutex::new(None),
    });
    // Frames leave from their own thread: pacing one overlaps capturing and
    // encoding the next, and the capture loop never waits on the network.
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Outgoing>(2);
    let sender = {
        let (s, sock) = (shared.clone(), socket.try_clone()?);
        std::thread::Builder::new().name("hyper-host-send".into()).spawn(move || sender_loop(sock, s, frame_rx))?
    };
    // Reliable channel: clipboard both ways, pointer shapes to the client.
    // Its bulk lane follows the rate controller (it yields to the stream).
    let link = crate::hyper_link::ReliableLink::new();
    link.set_bulk_rate(Some(shared.rate.lock().unwrap().bulk_bps()));
    let pump_stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let (s, sock) = (shared.clone(), socket.try_clone()?);
        link.spawn_pump(move |chunk| s.send(&sock, Kind::Reliable, chunk), pump_stop.clone())
    };
    let clipboard = {
        let link = link.clone();
        crate::hyper_clipboard::ClipboardSync::start(link).map(Arc::new)
    };

    let rx_socket = socket.try_clone()?;
    let rx_shared = shared.clone();
    let desktop = cap.desktop;
    let mic_sink = routing.mic_sink.clone();
    let rx_link = link.clone();
    let rx_clipboard = clipboard.clone();
    let receiver = std::thread::Builder::new()
        .name("hyper-host-rx".into())
        .spawn(move || receive_loop(rx_socket, recv, rx_shared, desktop, mic_sink, rx_link, rx_clipboard))?;

    let audio_socket = socket.try_clone()?;
    let audio_shared = shared.clone();
    let _game_audio = crate::hyper_audio::AudioSender::start(
        crate::hyper_audio::Source::Loopback,
        2,
        128_000,
        qpc_us,
        Arc::new(move |body: &[u8]| audio_shared.send(&audio_socket, Kind::Audio, body)),
    );
    // Tell the client to capture its mic only while a host app listens —
    // heard from Windows as apps start and stop recording (polled only where
    // that can't be had), and repeated every 2 s in case a packet is lost.
    let mic_changed = Arc::new(crate::hyper_audio::Changed::default());
    let mic_watch = routing.mic_source.clone().map(|source| {
        let (s, sock, changed) = (shared.clone(), socket.try_clone(), mic_changed.clone());
        std::thread::spawn(move || {
            let Ok(sock) = sock else { return };
            crate::wasapi::com_init();
            let mut watch = crate::hyper_audio::ListenerWatch::new(&source, changed);
            let mut on = false;
            let mut last_sent = Instant::now() - Duration::from_secs(10);
            // Apps told about already (once a stream each).
            let mut noticed = std::collections::HashSet::new();
            while !s.stop.load(Ordering::Relaxed) {
                // An app recording from another microphone: the client's sound
                // can't reach it there. Say so on the client's screen.
                if last_sent.elapsed() >= Duration::from_secs(2) {
                    for (app, mic) in crate::hyper_audio::other_mic_users(&source) {
                        if noticed.insert(app.clone()) {
                            tracing::info!(%app, %mic, "a host app records from another microphone than the stream's");
                            let text = format!("{app} on the host records from \"{mic}\": set its microphone to \"Default\" to hear this PC's");
                            s.send(&sock, Kind::Control, &Control::Notice { text }.to_bytes());
                        }
                    }
                }
                let now_on = match watch.as_mut() {
                    Some(w) => w.listening(),
                    None => crate::hyper_audio::endpoint_has_listeners(&source),
                };
                if now_on != on || last_sent.elapsed() > Duration::from_secs(2) {
                    if now_on != on {
                        tracing::info!(listening = now_on, "host app microphone use changed");
                    }
                    on = now_on;
                    s.send(&sock, Kind::Control, &Control::Mic { on }.to_bytes());
                    last_sent = Instant::now();
                }
                match &watch {
                    Some(w) => w.wait(Duration::from_secs(2).saturating_sub(last_sent.elapsed())),
                    None => std::thread::sleep(Duration::from_millis(300)),
                }
            }
        })
    });

    let mut frame_id = 0u32;
    let mut last_tex = None;
    let mut capture_errors = 0u32;
    let mut last_size_check = Instant::now();
    let mut last_status = Instant::now();
    let mut frames = 0u32;
    let mut bytes = 0u64;
    let mut encode_us_total = 0u64;
    let mut acquire_us_total = 0u64;
    let mut enc_us_total = 0u64;
    let mut report = Instant::now();
    // Frames are encoded at the client's rate even when the monitor runs
    // faster (a 240 Hz panel streaming to a 120 Hz laptop): each captured
    // frame earns its share of our frame interval as credit, and a frame is
    // encoded once a whole credit has accrued (a little jitter tolerated).
    // The newest skipped frame is kept, so a change that ends on a skipped
    // frame still reaches the client when the screen goes still.
    let mut interval_us = 1_000_000 / fps.max(1) as u64;
    let mut credit = 1.0f64;
    let mut last_present_us: Option<u64> = None;
    let mut skipped: Option<Captured> = None;
    // Office mode: once the screen goes still, the last picture goes in again
    // (up to a second's frames), each time sharper: the encoder spends the
    // frame budget refining it. Measured on a screen of new text with the
    // fast preset: luma 33 dB, then 45 at half a second and 52 at one —
    // past the quality preset's 42, which costs ~8 ms on every frame.
    let mut sharpen_left = 0u32;
    let mut last_encoded_us = 0u64;
    // Trickling (see TRICKLE_AFTER_INPUT_US), and frames sent so (for the log).
    let (mut trickle, mut trickle_frames) = (Trickle::default(), 0u32);
    let mut ltr = Ltr::new(0);
    // A failed encoder or a lost GPU (driver reset, device removed) costs a
    // pipeline rebuild, not the session — in direct mode ending the stream
    // would lock the PC. A burst of failures still gives up.
    let mut rebuilds: Vec<Instant> = Vec::new();
    let mut captures = CaptureTimes::default();
    let mut last_nudge = Instant::now();
    // The loop lives in a closure so every exit — the client leaving, going
    // quiet, or an encoder/capture error — runs the same cleanup below:
    // threads stopped, the client told, the host's audio restored.
    let mut run = || -> Result<StreamEnd, Box<dyn std::error::Error>> {
    while !shared.stop.load(Ordering::Relaxed) {
        if shared.last_rx.lock().unwrap().elapsed() > CLIENT_TIMEOUT {
            tracing::info!("client went quiet; ending the stream");
            return Ok(StreamEnd::Lost);
        }
        // The service is stopping (or the PC shutting down): end as if the
        // client said goodbye, so everything is put back and locked — unless
        // the console moved to another session, whose agent the client
        // reconnects to.
        if crate::hyper_direct::stop_requested() {
            if console_moved() {
                tracing::info!("the console moved to another session; ending the stream for the agent there");
                return Ok(StreamEnd::Moved);
            }
            tracing::info!("Hyper service stopping; ending the stream");
            return Ok(StreamEnd::Bye);
        }
        // Office or game mode, as the client asks (Ctrl+Shift+O): a new
        // encoder (frame budget, preset) and frame rate, and the host's text
        // smoothing to match.
        let want = (shared.office.load(Ordering::Relaxed), shared.fast_text.load(Ordering::Relaxed));
        // (Fast text only matters in office mode: game mode isn't rebuilt for it.)
        if !want.0 && !office {
            fast_text = want.1;
        }
        if want != (office, fast_text) {
            let mode_changed = want.0 != office;
            (office, fast_text) = want;
            (fps, tuning) = mode(office, fast_text, full_fps);
            interval_us = 1_000_000 / fps.max(1) as u64;
            let bps = shared.rate.lock().unwrap().bitrate_bps();
            cap.release();
            enc.release();
            // The desktop's own resolution back, or the client's (office).
            if mode_changed {
                native = None;
                if office && route_audio {
                    native = opts.fit_to.and_then(|s| NativeResolution::apply(device_name, s));
                }
            }
            (cap, enc) = reopen_pipeline(device_name, opts, fps, bps, codec, tuning)?;
            captures = CaptureTimes::default();
            (last_tex, skipped, last_present_us, credit) = (None, None, None, 1.0);
            resized(&shared, &socket, cap.stream, fps);
            shared.keyframe.store(true, Ordering::Relaxed);
            // Office text wants a bigger budget to come back to after a lull.
            shared.rate.lock().unwrap().set_resting(if office { 30_000_000 } else { 20_000_000 });
            if mode_changed {
                grey_text = None; // (the old setting back first)
                if office && route_audio {
                    grey_text = GreyText::apply();
                }
            }
            tracing::info!(office, fast_text, fps, "stream mode changed");
            continue;
        }
        // The client's screen changed its refresh rate: the stream follows
        // (within what was agreed), with an encoder budgeted for the new rate.
        let hz = shared.client_hz.load(Ordering::Relaxed);
        let follow = if hz > 0 { hz.clamp(30.min(agreed_fps), agreed_fps) } else { agreed_fps };
        if follow != full_fps {
            full_fps = follow;
            let (new_fps, new_tuning) = mode(office, fast_text, full_fps);
            if new_fps != fps {
                (fps, tuning) = (new_fps, new_tuning);
                interval_us = 1_000_000 / fps.max(1) as u64;
                let bps = shared.rate.lock().unwrap().bitrate_bps();
                cap.release();
                enc.release();
                (cap, enc) = reopen_pipeline(device_name, opts, fps, bps, codec, tuning)?;
                captures = CaptureTimes::default();
                (last_tex, skipped, last_present_us, credit) = (None, None, None, 1.0);
                resized(&shared, &socket, cap.stream, fps);
                shared.keyframe.store(true, Ordering::Relaxed);
                tracing::info!(fps, client_hz = hz, "the client's screen changed its refresh rate; the stream follows");
                continue;
            }
        }
        let want_key = shared.keyframe.swap(false, Ordering::Relaxed);
        let since_input_us = qpc_us().saturating_sub(shared.last_input_us.load(Ordering::Relaxed));
        let (pace_us, resumed) = trickle.pace(office, since_input_us, interval_us);
        if resumed {
            // Input (or a big change): the next picture goes at once.
            credit = credit.max(1.0);
        }
        // While a skipped frame waits, wait for a newer one only until its turn.
        let until_us = |t: u64| t.saturating_sub(qpc_us()).div_ceil(1000).min(10) as u32;
        let wait_ms = match &skipped {
            Some(f) => until_us(f.present_us() + pace_us),
            None if office && sharpen_left > 0 && last_tex.is_some() => until_us(last_encoded_us + pace_us),
            // A screen that changes wakes the wait at once; the timeout only
            // bounds the status and an answer to a keyframe request on a still
            // one — after a second without input, less often.
            None if since_input_us > 1_000_000 => 30,
            None => 10,
        };
        let captured = match if cap.defers() {
            cap.peek(wait_ms).map(|p| p.map(|present_us| Captured::Later { present_us }))
        } else {
            cap.acquire(wait_ms).map(|c| c.map(Captured::Now))
        } {
            Ok(c) => {
                capture_errors = 0;
                c
            }
            Err(e) => {
                capture_errors += 1;
                if capture_errors > 50 {
                    rebuild(&mut rebuilds, &e.to_string())?;
                    let bps = shared.rate.lock().unwrap().bitrate_bps();
                    cap.release();
                    enc.release();
                    (cap, enc) = reopen_pipeline(device_name, opts, fps, bps, codec, tuning)?;
                    captures = CaptureTimes::default();
                    capture_errors = 0;
                    (last_tex, skipped) = (None, None);
                    resized(&shared, &socket, cap.stream, fps);
                    shared.keyframe.store(true, Ordering::Relaxed);
                    continue;
                }
                tracing::debug!(error = %e, "capture error; retrying");
                std::thread::sleep(Duration::from_millis(20));
                None
            }
        };
        // A resolution change (the user, a game going fullscreen, the lock
        // screen): rebuild capture + encoder at the new size; the client
        // follows via the status message.
        if (captured.is_none() || capture_errors > 0) && last_size_check.elapsed() > Duration::from_millis(50) {
            last_size_check = Instant::now();
            if cap.size_changed() {
                let bps = shared.rate.lock().unwrap().bitrate_bps();
                cap.release();
                enc.release();
                (cap, enc) = reopen_pipeline(device_name, opts, fps, bps, codec, tuning)?;
                captures = CaptureTimes::default();
                let (nw, nh) = cap.stream;
                *shared.status.lock().unwrap() = (nw as u16, nh as u16, fps as u16);
                shared.send_status(&socket);
                last_tex = None;
                skipped = None;
                tracing::info!(width = nw, height = nh, "host display changed; stream resized");
                continue;
            }
        }
        for ev in cap.take_pointer_events() {
            link.send(&match ev {
                rdp_gpu::hyper::PointerEvent::Visible(v) => rdp_hyper::messages::Message::CursorVisible(v),
                rdp_gpu::hyper::PointerEvent::Shape { width, height, hot_x, hot_y, rgba } => {
                    rdp_hyper::messages::Message::CursorShape { width, height, hot_x, hot_y, rgba }
                }
            });
        }
        let acquired_us = qpc_us();
        // No one can see the client's window: keep the capture going
        // (pointer, size changes, the picture to show on return) but encode
        // and send nothing — and look (and convert) only twice a second:
        // whatever changes meanwhile waits in the duplication, and the
        // keyframe that ends a pause takes the newest picture.
        if shared.paused.load(Ordering::Relaxed) && !want_key {
            if let Some(f) = captured.and_then(|c| c.into_frame(&mut cap)) {
                last_tex = Some(f.nv12);
            }
            skipped = None;
            if last_status.elapsed() > Duration::from_millis(1000) {
                shared.send_status(&socket);
                last_status = Instant::now();
            }
            // (In steps, so a keyframe request ends the pause at once.)
            for _ in 0..10 {
                if !shared.paused.load(Ordering::Relaxed) || shared.keyframe.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            continue;
        }
        let mut refresh = false;
        let frame = match captured {
            Some(f) => {
                let present_us = f.present_us();
                if let Some(prev) = last_present_us.replace(present_us) {
                    credit = (credit + present_us.saturating_sub(prev) as f64 / pace_us as f64).min(1.5);
                }
                if credit >= 0.9 || want_key {
                    credit = (credit - 1.0).max(0.0);
                    skipped = None;
                    sharpen_left = fps;
                    f.into_frame(&mut cap).map(|f| (f.nv12, f.present_us))
                } else {
                    skipped = Some(f);
                    None
                }
            }
            None => match skipped.take() {
                // Nothing newer came: the screen went still on a skipped frame.
                Some(f) if want_key || acquired_us >= f.present_us() + pace_us => {
                    credit = 0.0;
                    sharpen_left = fps;
                    f.into_frame(&mut cap).map(|f| (f.nv12, f.present_us))
                }
                Some(f) => {
                    skipped = Some(f);
                    None
                }
                // A static screen gives no frames; a keyframe request still
                // needs an answer (re-encode the last picture).
                None if want_key && last_tex.is_some() => Some((last_tex.clone().unwrap(), qpc_us())),
                // A repair asked for on a still screen: the last picture again,
                // coded from a reference the client has.
                None if shared.recover.load(Ordering::Relaxed) && last_tex.is_some() => Some((last_tex.clone().unwrap(), qpc_us())),
                // Still: the last picture again, sharper.
                None if office && sharpen_left > 0 && last_tex.is_some() && acquired_us >= last_encoded_us + pace_us => {
                    sharpen_left -= 1;
                    refresh = true;
                    Some((last_tex.clone().unwrap(), qpc_us()))
                }
                None => None,
            },
        };
        let Some((tex, capture_us)) = frame else {
            if want_key {
                shared.keyframe.store(true, Ordering::Relaxed);
                // No picture yet: Desktop Duplication gets nothing from a
                // display that's asleep, or from a desktop that doesn't
                // change. Now and then, wake the displays and have every
                // window repaint, so there is one to send.
                if last_tex.is_none() && skipped.is_none() && last_nudge.elapsed() > Duration::from_millis(500) {
                    last_nudge = Instant::now();
                    wake_display();
                    unsafe {
                        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(None, None, true);
                    }
                }
            }
            if last_status.elapsed() > Duration::from_millis(250) {
                shared.send_status(&socket);
                last_status = Instant::now();
            }
            continue;
        };
        if want_key {
            enc.request_keyframe();
        }
        let (bps, fec, pace) = {
            let r = shared.rate.lock().unwrap();
            (r.bitrate_bps(), r.fec(), r.pace())
        };
        if (bps as i64 - enc.bitrate as i64).unsigned_abs() > enc.bitrate as u64 / 20 {
            enc.set_bitrate(bps);
        }
        // Long-term references: keep one now and then; repair a loss from
        // the newest the client has (else a keyframe). After the bitrate
        // (a rebuilt encoder has none of the old one's).
        if ltr.encoder != enc.id {
            ltr = Ltr::new(enc.id);
        }
        let acked = shared.ltr_acked.load(Ordering::Relaxed);
        if acked != u32::MAX {
            ltr.acked(acked);
        }
        let recover = shared.recover.swap(false, Ordering::Relaxed);
        match ltr.plan(enc.next_input(), recover, want_key || !enc.ltr, Instant::now()) {
            LtrPlan::Keyframe => {
                tracing::info!("frames lost and no reference the client has: a keyframe");
                enc.request_keyframe();
            }
            LtrPlan::Encode { mark, from } => {
                if let Some(slot) = mark {
                    enc.mark_ltr(slot);
                }
                if let Some(mask) = from {
                    enc.use_ltr(mask);
                }
            }
        }
        let t_enc = Instant::now();
        captures.note(capture_us);
        let out = match enc.encode(&tex) {
            Ok(Some(out)) => out,
            Ok(None) => continue,
            Err(e) => {
                rebuild(&mut rebuilds, &e.to_string())?;
                cap.release();
                enc.release();
                (cap, enc) = reopen_pipeline(device_name, opts, fps, bps, codec, tuning)?;
                (last_tex, skipped) = (None, None);
                captures = CaptureTimes::default();
                resized(&shared, &socket, cap.stream, fps);
                shared.keyframe.store(true, Ordering::Relaxed);
                continue;
            }
        };
        enc_us_total += t_enc.elapsed().as_micros() as u64;
        last_encoded_us = qpc_us();
        // Nothing left to refine: a quarter second in (a big change first
        // drains the frame budget, and those frames are tiny too), a frame
        // with next to nothing in it.
        if refresh && fps.saturating_sub(sharpen_left) >= fps / 4 && out.data.len() < 1500 {
            sharpen_left = 0;
        }
        // A new picture: as much sharpening as the change was big, and small
        // ones in a row lead to trickling.
        if !refresh && !out.keyframe {
            sharpen_left = sharpen_frames(out.data.len(), fps);
        }
        trickle.encoded(out.data.len(), refresh, out.keyframe);
        if trickle.on {
            trickle_frames += 1;
        }
        last_tex = Some(tex);
        // This picture's access unit, then any an encoder held back (never on
        // NVENC), in order — each with its own picture's capture time.
        for out in std::iter::once(out).chain(std::iter::from_fn(|| enc.take_output())) {
            if out.data.is_empty() {
                continue;
            }
            let capture_us = captures.of(out.input, capture_us);
            let encode_us = qpc_us().saturating_sub(capture_us) as u32;
            let (is_ltr, recovery) = ltr.sent(out.input, frame_id, out.keyframe);
            // The data shards now; the sender thread adds parity once they're out.
            let fec = shared.rate.lock().unwrap().parity_for(out.data.len(), out.keyframe);
            let data = packetize_data(
                &OutFrame { frame_id, data: &out.data, keyframe: out.keyframe, ltr: is_ltr, recovery, capture_us, encode_us },
                fec,
            );
            frame_id = frame_id.wrapping_add(1);
            frames += 1;
            bytes += out.data.len() as u64;
            encode_us_total += encode_us as u64;
            acquire_us_total += acquired_us.saturating_sub(capture_us);
            if frame_tx.send(Outgoing { data, payload: out.data.len(), bps, fps, pace, fec }).is_err() {
                return Err("the packet sender stopped".into());
            }
        }
        if last_status.elapsed() > Duration::from_millis(250) {
            shared.send_status(&socket);
            last_status = Instant::now();
        }
        if report.elapsed() > Duration::from_secs(5) {
            let secs = report.elapsed().as_secs_f64();
            let pace_ms = shared.pace_us.swap(0, Ordering::Relaxed) as f64
                / shared.paced.swap(0, Ordering::Relaxed).max(1) as f64
                / 1000.0;
            tracing::info!(
                target: "perf",
                fps = format!("{:.1}", frames as f64 / secs),
                mbps = format!("{:.1}", bytes as f64 * 8.0 / secs / 1e6),
                capture_to_sent_ms = format!("{:.2}", encode_us_total as f64 / frames.max(1) as f64 / 1000.0),
                present_to_captured_ms = format!("{:.2}", acquire_us_total as f64 / frames.max(1) as f64 / 1000.0),
                encode_ms = format!("{:.2}", enc_us_total as f64 / frames.max(1) as f64 / 1000.0),
                pace_ms = format!("{pace_ms:.2}"),
                target_mbps = bps / 1_000_000,
                fec_pct = (fec * 100.0) as u32,
                trickle_pct = trickle_frames * 100 / frames.max(1),
                audio_pps = format!("{:.0}", crate::hyper_audio::take_sent() as f64 / secs),
                gpu_mhz = gpu_clocks().map(|(g, v)| format!("{g}/{v}")).unwrap_or_default(),
                "hyper host"
            );
            (frames, bytes, encode_us_total, acquire_us_total) = (0, 0, 0, 0);
            trickle_frames = 0;
            enc_us_total = 0;
            report = Instant::now();
        }
    }
    Ok(StreamEnd::Bye)
    };
    let result = run();
    shared.stop.store(true, Ordering::Relaxed);
    drop(frame_tx); // the sender finishes the frame in hand, then ends
    let _ = sender.join();
    // Goodbye only for a real end: a client that went quiet may come back,
    // and one whose console moved reconnects to the agent there.
    let farewell = match result {
        Ok(StreamEnd::Bye) => Some(Control::Bye),
        Ok(StreamEnd::Moved) => Some(Control::Reconnect),
        _ => None,
    };
    if let Some(f) = farewell {
        for _ in 0..3 {
            shared.send(&socket, Kind::Control, &f.to_bytes());
        }
    }
    let _ = receiver.join();
    pump_stop.store(true, Ordering::Relaxed);
    link.nudge();
    let _ = pump.join();
    drop(clipboard);
    if let Some(t) = mic_watch {
        mic_changed.notify(); // (out of its wait at once)
        let _ = t.join();
    }
    // (The virtual camera goes with the stream.)
    drop(shared.camera.lock().unwrap().take());
    drop(_game_audio);
    drop(routing);
    match &result {
        Ok(end) => tracing::info!(?end, "Hyper stream ended"),
        Err(e) => tracing::warn!(error = %e, "Hyper stream failed"),
    }
    result
}

/// Capture times of the last few pictures by encode-call index, so an access
/// unit an encoder hands back late is paired with its own picture (the
/// latency stats and the rate controller's delay signal rely on it).
#[derive(Default)]
struct CaptureTimes {
    next: u64,
    recent: std::collections::VecDeque<(u64, u64)>,
}

impl CaptureTimes {
    /// Before each `encode()` call (they're counted from 0 per encoder).
    fn note(&mut self, capture_us: u64) {
        self.recent.push_back((self.next, capture_us));
        self.next += 1;
        if self.recent.len() > 16 {
            self.recent.pop_front();
        }
    }

    fn of(&self, input: u64, fallback: u64) -> u64 {
        self.recent.iter().rev().find(|(i, _)| *i == input).map_or(fallback, |&(_, c)| c)
    }
}

/// Note a pipeline rebuild after `error`; an error once five happened within
/// 30 s (the GPU is gone for good, or something is badly wrong).
fn rebuild(rebuilds: &mut Vec<Instant>, error: &str) -> Result<(), Box<dyn std::error::Error>> {
    rebuilds.retain(|t| t.elapsed() < Duration::from_secs(30));
    if rebuilds.len() >= 5 {
        return Err(format!("capture/encode keeps failing: {error}").into());
    }
    rebuilds.push(Instant::now());
    tracing::warn!(error, "capture/encode failed; rebuilding the pipeline");
    Ok(())
}

/// [`open_pipeline`], retried for a few seconds (after a driver reset the GPU
/// takes a moment to come back).
fn reopen_pipeline(
    device_name: &str,
    opts: &HostOptions,
    fps: u32,
    bps: u32,
    codec: rdp_gpu::h264::Codec,
    tuning: rdp_gpu::hyper::Tuning,
) -> Result<(DesktopCapture, HwEncoder), Box<dyn std::error::Error>> {
    let mut tries = 0;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        match open_pipeline(device_name, opts, fps, bps, codec, tuning) {
            Ok(p) => return Ok(p),
            Err(e) if tries < 20 => {
                tries += 1;
                tracing::debug!(error = %e, "pipeline not back yet");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Long-term reference frames, to repair a loss without a keyframe. Measured
/// on NVENC (H.264 and HEVC, through Media Foundation): after frames were
/// lost, a frame coded from a kept one decodes exactly on the GPU decoder
/// clients use, at the size of a normal frame — where a keyframe is several
/// times bigger, so on a lossy link it's the frame most likely lost again,
/// and it spikes the bitrate.
///
/// Every quarter second a frame is kept, in two slots by turns. The client
/// reports the newest kept frame it decoded; a slot counts only once the
/// client has said so (one being refilled counts for nothing until then: the
/// client may still hold the old picture there). A loss is answered with a
/// frame coded from the newest confirmed one only; with none, or a third
/// repair within a second, a keyframe.
struct Ltr {
    /// The encoder ([`rdp_gpu::hyper::HwEncoder::id`]) these slots are in.
    encoder: u64,
    /// Per slot: the frame kept there, and whether the client has it.
    slots: [Option<(u32, bool)>; rdp_gpu::hyper::LTR_SLOTS as usize],
    next_slot: usize,
    last_mark: Option<Instant>,
    /// Pictures (encoder inputs) told to be kept, with their slot.
    marking: Vec<(u64, usize)>,
    /// The picture coded as a repair.
    repairing: Option<u64>,
    repairs: Vec<Instant>,
}

enum LtrPlan {
    /// Keep the picture in slot `mark`; code it from the references in mask
    /// `from` only (a repair).
    Encode { mark: Option<u32>, from: Option<u32> },
    Keyframe,
}

impl Ltr {
    const EVERY: Duration = Duration::from_millis(250);
    const REPAIRS_PER_SEC: usize = 3;

    fn new(encoder: u64) -> Self {
        Self { encoder, slots: [None; 2], next_slot: 0, last_mark: None, marking: Vec::new(), repairing: None, repairs: Vec::new() }
    }

    /// Before picture `input` goes in: what the encoder should do with it.
    /// `recover`: the client lost frames. `key`: a keyframe comes anyway
    /// (or the encoder keeps no references).
    fn plan(&mut self, input: u64, recover: bool, key: bool, now: Instant) -> LtrPlan {
        if key {
            return if recover { LtrPlan::Keyframe } else { LtrPlan::Encode { mark: None, from: None } };
        }
        if recover {
            self.repairs.retain(|t| now.duration_since(*t) < Duration::from_secs(1));
            let newest = (0..self.slots.len())
                .filter_map(|s| match self.slots[s] {
                    Some((id, true)) => Some((id, s)),
                    _ => None,
                })
                .max_by(|a, b| if a.0.wrapping_sub(b.0) < u32::MAX / 2 { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less });
            return match newest {
                Some((_, slot)) if self.repairs.len() < Self::REPAIRS_PER_SEC => {
                    self.repairs.push(now);
                    self.repairing = Some(input);
                    LtrPlan::Encode { mark: None, from: Some(1 << slot) }
                }
                _ => {
                    self.repairs.clear();
                    LtrPlan::Keyframe
                }
            };
        }
        if self.last_mark.is_some_and(|t| now.duration_since(t) < Self::EVERY) {
            return LtrPlan::Encode { mark: None, from: None };
        }
        let slot = self.next_slot;
        self.next_slot = (slot + 1) % self.slots.len();
        self.slots[slot] = None;
        self.marking.push((input, slot));
        self.last_mark = Some(now);
        LtrPlan::Encode { mark: Some(slot as u32), from: None }
    }

    /// Picture `input` went out as frame `frame_id`: whether it's kept, and
    /// whether it's a repair.
    fn sent(&mut self, input: u64, frame_id: u32, keyframe: bool) -> (bool, bool) {
        if keyframe {
            // A keyframe clears every reference, the client's too.
            self.slots = [None; 2];
            self.repairs.clear();
        }
        let kept = self.marking.iter().position(|&(i, _)| i == input).map(|p| self.marking.remove(p).1);
        self.marking.retain(|&(i, _)| i > input);
        let kept = kept.filter(|_| !keyframe).map(|slot| self.slots[slot] = Some((frame_id, false))).is_some();
        let repair = self.repairing == Some(input);
        if repair || self.repairing.is_some_and(|i| i < input) {
            self.repairing = None;
        }
        (kept, repair && !keyframe)
    }

    /// The client decoded kept frame `frame_id`.
    fn acked(&mut self, frame_id: u32) {
        for (id, acked) in self.slots.iter_mut().flatten() {
            if *id == frame_id {
                *acked = true;
            }
        }
    }
}

/// Tell the client the (possibly new) stream size.
fn resized(shared: &Shared, socket: &UdpSocket, (w, h): (u32, u32), fps: u32) {
    *shared.status.lock().unwrap() = (w as u16, h as u16, fps as u16);
    shared.send_status(socket);
}

/// Capture + encoder. At desktop size, prefer handing the encoder raw BGRA
/// (it converts on its own engine — measured 8.5 ms → see log while a game
/// loads the 3D engine); fall back to our NV12 conversion.
fn open_pipeline(
    device_name: &str,
    opts: &HostOptions,
    fps: u32,
    bps: u32,
    codec: rdp_gpu::h264::Codec,
    tuning: rdp_gpu::hyper::Tuning,
) -> Result<(DesktopCapture, HwEncoder), Box<dyn std::error::Error>> {
    let size = stream_size(opts, device_name);
    // RDPIO_HYPER_NV12=1 forces our own NV12 conversion (for measurements).
    if size.is_none() && std::env::var_os("RDPIO_HYPER_NV12").is_none() {
        if let Ok(cap) = DesktopCapture::new_rgb(device_name, 4) {
            let (w, h) = cap.stream;
            match HwEncoder::new_tuned(&cap.device, cap.adapter_luid().ok(), w, h, fps, bps, true, codec, tuning) {
                Ok(enc) => return Ok((cap, enc)),
                Err(e) => tracing::info!(error = %e, "encoder takes no RGB input; converting to NV12 ourselves"),
            }
        }
    }
    let cap = DesktopCapture::new(device_name, size, 4)?;
    let (w, h) = cap.stream;
    let enc = HwEncoder::new_tuned(&cap.device, cap.adapter_luid().ok(), w, h, fps, bps, false, codec, tuning)?;
    Ok((cap, enc))
}

/// Office mode's frame rate: text and video, not games.
const OFFICE_FPS: u32 = 60;

/// The frame rate and encoder tuning for a mode: office — crisp text (see
/// [`rdp_gpu::hyper::Tuning::office`]) at up to 60 fps; game — the client's
/// own rate, tuned for latency.
fn mode(office: bool, fast_text: bool, client_fps: u32) -> (u32, rdp_gpu::hyper::Tuning) {
    if office {
        let fps = client_fps.clamp(1, OFFICE_FPS);
        let mut tuning = rdp_gpu::hyper::Tuning::office(fps);
        // Fast text: the big frame budget without the quality preset's ~6 ms.
        if fast_text {
            tuning.quality = 0;
        }
        (fps, tuning)
    } else {
        (client_fps, rdp_gpu::hyper::Tuning::GAMING)
    }
}

/// Greyscale font smoothing on the host while it lives (office mode).
/// ClearType tints glyph edges for the host's own LCD subpixels: colour
/// detail that 4:2:0 video halves (measured on office text: 5 dB lost) and
/// that's wrong on any other screen anyway. Set for the session only (never
/// saved), so even a crash undoes it at the next sign-out.
pub(crate) struct GreyText {
    previous: u32,
}

/// `FE_FONTSMOOTHINGSTANDARD`: greyscale. (`FE_FONTSMOOTHINGCLEARTYPE` = 2.)
const FONT_SMOOTHING_STANDARD: u32 = 1;

impl GreyText {
    fn apply() -> Option<Self> {
        let previous = set_font_smoothing(FONT_SMOOTHING_STANDARD)?;
        if previous == FONT_SMOOTHING_STANDARD {
            return None; // already greyscale: nothing to put back
        }
        tracing::info!("greyscale font smoothing on the host while in office mode");
        Some(Self { previous })
    }
}

impl Drop for GreyText {
    fn drop(&mut self) {
        if set_font_smoothing(self.previous).is_some() {
            tracing::info!("the host's font smoothing is back");
        }
    }
}

/// Set the signed-in user's font smoothing type; the one it had. From the
/// SYSTEM agent through a helper run as that user (the setting is theirs),
/// else here.
fn set_font_smoothing(kind: u32) -> Option<u32> {
    match crate::hyper_direct::run_as_user(current_session_id(), &format!("--hyper-font-smoothing {kind}")) {
        Ok(Some(previous)) if previous != 0 => Some(previous),
        Ok(_) => None,
        Err(_) => font_smoothing_here(kind),
    }
}

/// Set this session's font smoothing type, for now (not saved); the one it
/// had. `--hyper-font-smoothing <kind>` (exit code = the previous one).
pub(crate) fn font_smoothing_here(kind: u32) -> Option<u32> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut previous = 0u32;
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHINGTYPE,
            0,
            Some(&mut previous as *mut u32 as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        SystemParametersInfoW(SPI_SETFONTSMOOTHINGTYPE, 0, Some(kind as usize as *mut core::ffi::c_void), SPIF_SENDCHANGE).ok()?;
        Some(previous)
    }
}

/// One encoded frame for the sender thread.
struct Outgoing {
    /// The data shards' packet bodies ([`packetize_data`]).
    data: Vec<Vec<u8>>,
    /// Encoded size (the rate controller turns wire bytes back into this).
    payload: usize,
    bps: u32,
    fps: u32,
    pace: f32,
    fec: f32,
}

/// The sender thread. Each frame's packets are spread over `pace` of a frame
/// interval (adaptive: tight on a clean path, wider under loss — see
/// `RateController::pace`) and no faster than 2.5× the target bitrate, so a
/// big frame doesn't hit a Wi-Fi radio or a router queue as one burst. Waits
/// sleep on a high-resolution timer, in micro-bursts (no spinning). Data
/// shards go first and parity is computed once they're out (the client
/// usually completes a frame from data alone). If the next frame is already
/// waiting, the rest of this one goes out at once: pacing never delays a
/// newer frame. Each packet is sealed right before it's sent, so audio and
/// control packets slot in between instead of waiting out a keyframe.
fn sender_loop(socket: UdpSocket, shared: Arc<Shared>, frames: std::sync::mpsc::Receiver<Outgoing>) {
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let timer = HiResTimer::new();
    let mut next: Option<Outgoing> = None;
    // Each micro-burst's datagrams (one frame's are all one size) go to the
    // kernel in one call.
    let mut batch = crate::udp_batch::SendBatch::default();
    while let Some(f) = next.take().or_else(|| frames.recv().ok()) {
        let start = Instant::now();
        let peer = *shared.peer.lock().unwrap();
        let data_bytes: usize = f.data.iter().map(|b| b.len() + rdp_hyper::wire::OVERHEAD).sum();
        let window = (f.pace as f64 / f.fps.max(1) as f64).max(1e-4);
        let total = data_bytes as f64 * (1.0 + f.fec as f64);
        let rate = (f.bps as f64 * 2.5).max(total * 8.0 / window) / 8.0; // bytes/s
        // Sealed bytes so far (the schedule) and body bytes (what the client
        // reports as delivered, for the rate controller).
        let (mut sent, mut bodies) = (0usize, 0usize);
        let mut send = |body: &[u8], batch: &mut crate::udp_batch::SendBatch| {
            if next.is_none() {
                next = frames.try_recv().ok();
            }
            // On schedule, in micro-bursts: only a lead of more than half a
            // millisecond is slept off (the timer's own granularity) — a
            // small frame goes out at once, a keyframe is spread.
            let ahead = sent as f64 / rate - start.elapsed().as_secs_f64();
            if next.is_none() && ahead > 0.000_5 {
                batch.flush(&socket);
                timer.sleep(Duration::from_secs_f64(ahead));
            }
            let packet = shared.send.lock().unwrap().seal(Kind::Video, body);
            batch.push(&socket, peer, &packet);
            sent += packet.len();
            bodies += body.len();
        };
        // The data shards; parity once they're out (the client usually
        // completes a frame from data alone).
        for body in &f.data {
            send(body, &mut batch);
        }
        batch.flush(&socket);
        for body in &parity(&f.data) {
            send(body, &mut batch);
        }
        batch.flush(&socket);
        shared.rate.lock().unwrap().note_sent(f.payload, bodies);
        shared.pace_us.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        shared.paced.fetch_add(1, Ordering::Relaxed);
    }
}

/// A reusable high-resolution waitable timer: sub-millisecond sleeps without
/// spinning (a plain sleep rounds up to the system tick, often 1-15 ms).
struct HiResTimer(windows::Win32::Foundation::HANDLE);

impl HiResTimer {
    fn new() -> Self {
        use windows::Win32::System::Threading::{
            CreateWaitableTimerExW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
        };
        let h = unsafe {
            CreateWaitableTimerExW(None, windows::core::PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
        };
        Self(h.unwrap_or_default())
    }

    fn sleep(&self, d: Duration) {
        use windows::Win32::System::Threading::{SetWaitableTimer, WaitForSingleObject};
        // Relative due time, in 100 ns units.
        let due = -((d.as_nanos() / 100).max(1) as i64);
        unsafe {
            if self.0.is_invalid() || SetWaitableTimer(self.0, &due, 0, None, None, false).is_err() {
                std::thread::sleep(d);
                return;
            }
            WaitForSingleObject(self.0, 1000);
        }
    }
}

impl Drop for HiResTimer {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

fn receive_loop(
    socket: UdpSocket,
    mut recv: Session,
    shared: Arc<Shared>,
    desktop: (u32, u32),
    mic_sink: Option<String>,
    link: Arc<crate::hyper_link::ReliableLink>,
    clipboard: Option<Arc<crate::hyper_clipboard::ClipboardSync>>,
) {
    let _ = socket.set_read_timeout(Some(Duration::from_millis(10)));
    // Input lands here: a game holding the CPU mustn't delay it.
    crate::net_tune::raise_thread();
    let mut input = InputReceiver::new();
    // Input acks are batched (a 1000 Hz mouse would otherwise get 1000 status
    // packets a second back); the trailing one still goes out within ~10 ms.
    let mut acked = 0u32;
    let mut last_ack = Instant::now();
    let mut held = crate::hyper_client::Held::default();
    let mut mic: Option<crate::hyper_audio::AudioSink> = None;
    let mut last_feedback = Instant::now();
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::Relaxed) {
        if input.applied() != acked && last_ack.elapsed() >= Duration::from_millis(4) {
            acked = input.applied();
            last_ack = Instant::now();
            shared.send_status(&socket);
        }
        let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
        let Ok((kind, body)) = recv.open(&buf[..n]) else { continue };
        *shared.last_rx.lock().unwrap() = Instant::now();
        {
            let mut peer = shared.peer.lock().unwrap();
            if *peer != from {
                tracing::info!(old = %*peer, new = %from, "client address changed");
                *peer = from;
            }
        }
        match kind {
            Kind::Input => {
                shared.last_input_us.store(qpc_us(), Ordering::Relaxed);
                let events = input.receive(&body);
                // Follow the input desktop (lock screen, UAC) — only possible
                // as SYSTEM; harmless otherwise.
                rdp_gpu::hyper::attach_input_desktop();
                for e in &events {
                    held.note(e);
                }
                inject(&events, desktop);
                shared.input_ack.store(input.applied(), Ordering::Relaxed);
                if last_ack.elapsed() >= Duration::from_millis(4) {
                    acked = input.applied();
                    last_ack = Instant::now();
                    shared.send_status(&socket);
                }
            }
            Kind::Control => match Control::parse(&body) {
                Some(Control::Ping { id, client_us }) => {
                    let pong = Control::Pong { id, client_us, host_us: qpc_us() };
                    shared.send(&socket, Kind::Control, &pong.to_bytes());
                }
                Some(Control::KeyframeRequest) => {
                    shared.keyframe.store(true, Ordering::Relaxed);
                    shared.paused.store(false, Ordering::Relaxed);
                }
                Some(Control::Recover) => {
                    shared.recover.store(true, Ordering::Relaxed);
                    shared.paused.store(false, Ordering::Relaxed);
                }
                Some(Control::Paused { paused }) => {
                    if shared.paused.swap(paused, Ordering::Relaxed) != paused {
                        tracing::info!(paused, "client window minimized/restored");
                    }
                }
                Some(Control::Mode { office, fast_text }) => {
                    shared.office.store(office, Ordering::Relaxed);
                    shared.fast_text.store(fast_text, Ordering::Relaxed);
                }
                Some(Control::Refresh { hz }) => {
                    shared.client_hz.store(hz as u32, Ordering::Relaxed);
                }
                // The client has a camera: offer it to apps here, once.
                Some(Control::Cameras { count }) => {
                    if count > 0 && shared.camera_allowed && !shared.camera_started.swap(true, Ordering::Relaxed) {
                        if let Ok(sock) = socket.try_clone() {
                            let s = shared.clone();
                            let _ = std::thread::Builder::new().name("hyper-camera-setup".into()).spawn(move || {
                                let to_client = s.clone();
                                let control: Arc<dyn Fn(&Control) + Send + Sync> =
                                    Arc::new(move |c: &Control| to_client.send(&sock, Kind::Control, &c.to_bytes()));
                                match crate::hyper_camera::HostCamera::start(control) {
                                    // (Unless the stream ended meanwhile.)
                                    Ok(c) if !s.stop.load(Ordering::Relaxed) => *s.camera.lock().unwrap() = Some(c),
                                    Ok(_) => {}
                                    Err(e) => tracing::info!(reason = %e, "no virtual camera for this stream"),
                                }
                            });
                        }
                    }
                }
                Some(Control::Feedback(fb)) => {
                    if let Some(id) = fb.ltr_frame {
                        shared.ltr_acked.store(id, Ordering::Relaxed);
                    }
                    let arrived_ms = last_feedback.elapsed().as_millis().max(1) as u64;
                    last_feedback = Instant::now();
                    // The window the counts cover, as the client measured it
                    // (arrival spacing jitters with the uplink); older clients
                    // don't say.
                    let window_ms =
                        if fb.window_us > 0 { (fb.window_us as u64 / 1000).clamp(10, 1000) } else { arrived_ms };
                    let now_ms = qpc_us() / 1000;
                    let busy = link.bulk_backlog() > 0;
                    let bulk = {
                        let mut r = shared.rate.lock().unwrap();
                        r.set_bulk_busy(busy);
                        r.update(&fb, window_ms, now_ms);
                        r.bulk_bps()
                    };
                    link.set_bulk_rate(Some(bulk));
                }
                Some(Control::Bye) => {
                    tracing::info!("client said goodbye");
                    shared.stop.store(true, Ordering::Relaxed);
                }
                _ => {}
            },
            Kind::Reliable => {
                let (ack, msgs) = link.on_packet(&body);
                if let Some(a) = ack {
                    shared.send(&socket, Kind::Reliable, &a);
                }
                for m in msgs {
                    if let Some(c) = &clipboard {
                        c.apply(m);
                    }
                }
            }
            // The client's camera, into the virtual camera.
            Kind::Camera => {
                if let Some(c) = shared.camera.lock().unwrap().as_ref() {
                    c.push(&body);
                }
            }
            // The client's microphone, into the virtual mic.
            Kind::Audio => {
                if let Some(sink) = &mic_sink {
                    mic.get_or_insert_with(|| crate::hyper_audio::AudioSink::start(Some(sink.clone()))).push(&body);
                }
            }
            // A client fetching this host's newer rdpio while it streams (it
            // paces its requests; see `hyper_client`).
            Kind::Update => {
                if let Some(msg) = rdp_hyper::update::UpdateMsg::parse(&body) {
                    let replies = match crate::updater::own_package() {
                        Some(p) => p.answer(&msg),
                        None => vec![rdp_hyper::update::UpdateMsg::Unavailable.to_bytes()],
                    };
                    for r in replies {
                        shared.send(&socket, Kind::Update, &r);
                    }
                }
            }
            _ => {}
        }
    }
    // A client that vanished mid-press would leave keys or buttons stuck down
    // on the host (a held W keeps the character running).
    let leftovers = held.release_all();
    if !leftovers.is_empty() {
        tracing::info!(count = leftovers.len(), "releasing keys/buttons the client left held");
        inject(&leftovers, desktop);
    }
}

/// Replay client input on this desktop.
fn inject(events: &[InputEvent], _desktop: (u32, u32)) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let mut inputs = Vec::with_capacity(events.len());
    let mouse = |dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    let key = |scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    for ev in events {
        match *ev {
            InputEvent::Key { scancode, extended, down } => {
                let mut f = KEYEVENTF_SCANCODE;
                if extended {
                    f |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    f |= KEYEVENTF_KEYUP;
                }
                inputs.push(key(scancode, f));
            }
            InputEvent::Char { code, down } => {
                let mut f = KEYEVENTF_UNICODE;
                if !down {
                    f |= KEYEVENTF_KEYUP;
                }
                inputs.push(key(code, f));
            }
            // 0..=65535 across the primary monitor, which is the one streamed.
            InputEvent::MouseAbs { x, y } => {
                inputs.push(mouse(x as i32, y as i32, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE));
            }
            InputEvent::MouseRel { dx, dy } => inputs.push(mouse(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE)),
            InputEvent::Button { button, down } => {
                let (flags, data) = match (button, down) {
                    (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (0, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (3, true) => (MOUSEEVENTF_XDOWN, 1),
                    (3, false) => (MOUSEEVENTF_XUP, 1),
                    (4, true) => (MOUSEEVENTF_XDOWN, 2),
                    (4, false) => (MOUSEEVENTF_XUP, 2),
                    _ => continue,
                };
                inputs.push(mouse(0, 0, data, flags));
            }
            InputEvent::Wheel { delta, horizontal } => {
                let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
                inputs.push(mouse(0, 0, delta as i32, flags));
            }
            InputEvent::SecureAttention => send_sas(),
        }
    }
    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

/// Turn sleeping displays back on. Asking Windows to keep them on doesn't
/// wake ones already off; input does — so a nudge of the pointer one count
/// right and back, which counts as the user's.
fn wake_display() {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let nudge = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
    };
    unsafe {
        SendInput(&[nudge(1), nudge(-1)], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Raise Ctrl+Alt+Del (the secure attention sequence). Only SYSTEM may, and
/// only with the SoftwareSASGeneration policy (set by the service install).
fn send_sas() {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    type SendSas = unsafe extern "system" fn(i32);
    unsafe {
        // From System32 only: never a sas.dll planted next to this exe.
        let Ok(lib) = LoadLibraryExW(windows::core::w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else {
            tracing::warn!("Ctrl+Alt+Del unavailable (no sas.dll)");
            return;
        };
        if let Some(f) = GetProcAddress(lib, windows::core::s!("SendSAS")) {
            let f: SendSas = std::mem::transmute(f);
            f(0); // AsUser = FALSE: as a service
            tracing::info!("sent Ctrl+Alt+Del");
        }
    }
}

/// The RDP side channel, from the host end.
struct Channel(HANDLE);

impl Channel {
    fn open() -> Option<Self> {
        let name = std::ffi::CString::new(CHANNEL_NAME).ok()?;
        unsafe {
            WTSVirtualChannelOpenEx(WTS_CURRENT_SESSION, PCSTR(name.as_ptr() as *const u8), WTS_CHANNEL_OPTION_DYNAMIC)
                .ok()
                .filter(|h| !h.is_invalid())
                .map(Self)
        }
    }

    /// One message, or `None` on timeout. Dynamic-channel reads may carry an
    /// 8-byte CHANNEL_PDU_HEADER in front of the data; our messages are JSON,
    /// so anything not starting with `{` has it stripped.
    fn read(&self, timeout: Duration) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut n = 0u32;
        unsafe {
            WTSVirtualChannelRead(self.0, timeout.as_millis() as u32, &mut buf, &mut n).ok()?;
        }
        buf.truncate(n as usize);
        if buf.first() != Some(&b'{') && buf.len() > 8 {
            buf.drain(..8);
        }
        (!buf.is_empty()).then_some(buf)
    }

    fn write(&self, msg: &[u8]) {
        let mut n = 0u32;
        unsafe {
            if let Err(e) = WTSVirtualChannelWrite(self.0, msg, &mut n) {
                tracing::warn!(error = %e, "Hyper channel write failed");
            }
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            let _ = WTSVirtualChannelClose(self.0);
        }
    }
}

/// Host monitors turned off for the stream because the client has fewer
/// screens — windows gather on the streamed (primary) one. Applied without
/// saving to the display database, so dropping this (or a reboot, or Win+P)
/// restores the user's own layout.
pub(crate) struct Consolidation;

impl Consolidation {
    pub(crate) fn apply(client_monitors: u32) -> Option<Self> {
        use windows::Win32::Devices::Display::*;
        unsafe {
            let (mut np, mut nm) = (0u32, 0u32);
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).0 != 0 {
                return None;
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).0 != 0 {
                return None;
            }
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            if paths.len() as u32 <= client_monitors.max(1) {
                return None;
            }
            // The primary is the source positioned at the desktop origin.
            let source_mode = |p: &DISPLAYCONFIG_PATH_INFO| modes.get(p.sourceInfo.Anonymous.modeInfoIdx as usize).copied();
            let primary = *paths.iter().find(|p| {
                source_mode(p).is_some_and(|m| {
                    m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                        && m.Anonymous.sourceMode.position.x == 0
                        && m.Anonymous.sourceMode.position.y == 0
                })
            })?;
            // Keep only that path, with just its two modes, re-indexed.
            let mut path = primary;
            let mut kept = Vec::new();
            if let Some(m) = modes.get(primary.sourceInfo.Anonymous.modeInfoIdx as usize) {
                path.sourceInfo.Anonymous.modeInfoIdx = kept.len() as u32;
                kept.push(*m);
            }
            if let Some(m) = modes.get(primary.targetInfo.Anonymous.modeInfoIdx as usize) {
                path.targetInfo.Anonymous.modeInfoIdx = kept.len() as u32;
                kept.push(*m);
            }
            let r = SetDisplayConfig(
                Some(&[path]),
                Some(&kept),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
            );
            if r != 0 {
                tracing::warn!(code = r, "could not turn off the extra host monitors");
                return None;
            }
            tracing::info!(host_monitors = paths.len(), client_monitors, "extra host monitors off for the stream");
            std::thread::sleep(Duration::from_millis(500));
            Some(Self)
        }
    }
}

impl Drop for Consolidation {
    fn drop(&mut self) {
        use windows::Win32::Devices::Display::*;
        unsafe {
            let r = SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT);
            tracing::info!(code = r, "host monitor layout restored");
        }
    }
}

/// The monitor's display mode while streaming, restored on drop.
pub(crate) struct DisplayMode {
    device: Vec<u16>,
    pub(crate) fps: u32,
}

impl DisplayMode {
    /// Switch `device` to the mode [`choose_mode`] picks for a client screen
    /// of `client` size (`None`: the resolution stays) at `target` fps.
    /// The host's settings (see `HostSettings`), or `RDPIO_MATCH_RESOLUTION=0`
    /// on the host, keep the resolution too; `RDPIO_FAST_REFRESH=0` keeps the
    /// refresh rate at the stream's own.
    pub(crate) fn raise(device: &str, target: u32, client: Option<(u32, u32)>) -> Option<Self> {
        use windows::Win32::Graphics::Gdi::*;
        let settings = crate::hyper_direct::host_settings();
        let client = client.filter(|_| settings.match_resolution && std::env::var("RDPIO_MATCH_RESOLUTION").map_or(true, |v| v != "0"));
        let fastest = settings.fast_refresh && std::env::var("RDPIO_FAST_REFRESH").map_or(true, |v| v != "0");
        let wide: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
        let name = windows::core::PCWSTR(wide.as_ptr());
        unsafe {
            let mut cur = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            if !EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut cur).as_bool() {
                return None;
            }
            let mut modes = Vec::new();
            let mut i = 0;
            loop {
                let mut m = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
                if !EnumDisplaySettingsW(name, ENUM_DISPLAY_SETTINGS_MODE(i), &mut m).as_bool() {
                    break;
                }
                i += 1;
                if m.dmBitsPerPel == cur.dmBitsPerPel && m.Anonymous2.dmDisplayFlags & 2 == 0 /* DM_INTERLACED */ {
                    modes.push((m.dmPelsWidth, m.dmPelsHeight, m.dmDisplayFrequency));
                }
            }
            let now = (cur.dmPelsWidth, cur.dmPelsHeight, cur.dmDisplayFrequency);
            let Some((w, h, hz)) = choose_mode(&modes, now, client, target, fastest) else {
                // Already as fast or faster: the stream still runs at the
                // client's rate (the capture skips the extra frames).
                tracing::info!(width = now.0, height = now.1, hz = now.2, "monitor mode kept for streaming");
                return Some(Self { device: wide, fps: now.2.min(target) });
            };
            let mut best = cur;
            (best.dmPelsWidth, best.dmPelsHeight, best.dmDisplayFrequency) = (w, h, hz);
            best.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
            let r = ChangeDisplaySettingsExW(name, Some(&best), None, CDS_TYPE(0), None);
            if r == DISP_CHANGE_SUCCESSFUL {
                tracing::info!(
                    from = format!("{}x{}@{}", now.0, now.1, now.2),
                    to = format!("{w}x{h}@{hz}"),
                    client = ?client,
                    "monitor mode set for streaming"
                );
                // The mode change resets duplication; give the driver a moment.
                std::thread::sleep(Duration::from_millis(500));
                Some(Self { device: wide, fps: hz.min(target) })
            } else {
                tracing::warn!(code = r.0, to = format!("{w}x{h}@{hz}"), "could not change the monitor's mode");
                Some(Self { device: wide, fps: now.2.min(target) })
            }
        }
    }
}

/// The mode (width, height, refresh) to stream a monitor at, from the `modes`
/// it offers, for a client screen of `client` size wanting `target` fps;
/// `None` keeps the `current` one.
///
/// The resolution: the client's own where the monitor has it, else the one
/// that fills the client's screen best (fewest black bars) without being
/// enlarged on it (that blurs), and of those the least shrunk. Never more
/// pixels than now, and never for fewer frames than the current resolution
/// would give.
///
/// The refresh rate: as many frames as the stream wants (`target`), then a
/// whole multiple of it, so the frames the stream skips are every second or
/// fourth, evenly (144 Hz for 120 fps would skip one in six: judder); of
/// those the fastest (`fastest`), else `target` itself. A frame waits for the
/// next refresh to be composed and captured, half a refresh on average, so
/// 240 Hz for a 120 fps stream takes ~2 ms off every frame.
pub(crate) fn choose_mode(
    modes: &[(u32, u32, u32)],
    current: (u32, u32, u32),
    client: Option<(u32, u32)>,
    target: u32,
    fastest: bool,
) -> Option<(u32, u32, u32)> {
    let (cw, ch, chz) = current;
    let target = target.max(1);
    let rank = |hz: u32| {
        let r = hz as f64 / target as f64;
        let even = hz >= target && (hz as f64 - r.round() * target as f64).abs() <= 0.02 * hz as f64;
        (hz.min(target), even, if even && fastest { hz } else { u32::MAX - hz })
    };
    // A resolution's best refresh rate for the stream.
    let rate = |w: u32, h: u32| modes.iter().filter(|m| (m.0, m.1) == (w, h)).map(|m| m.2).max_by_key(|&hz| rank(hz));
    let here = rate(cw, ch).filter(|&hz| rank(hz) > rank(chz));
    let fps_here = here.unwrap_or(chz).min(target);
    let keep = here.map(|hz| (cw, ch, hz));
    let Some((sw, sh)) = client.filter(|&(w, h)| w >= 320 && h >= 200) else { return keep };
    // How a resolution shows there: the share of the screen it fills (in
    // half-percent steps), and its scale (> 1 is enlarged).
    let shown = |w: u32, h: u32| {
        let s = (sw as f64 / w as f64).min(sh as f64 / h as f64);
        let fill = (w as f64 * s) * (h as f64 * s) / (sw as f64 * sh as f64);
        ((fill * 200.0).round() as u32, s)
    };
    let mut best = ((cw, ch), shown(cw, ch));
    for &(w, h, _) in modes {
        let (fill, scale) = shown(w, h);
        if (w, h) == best.0 || w as u64 * h as u64 > cw as u64 * ch as u64 || scale > 1.0 + 1e-9 {
            continue;
        }
        if rate(w, h).is_none_or(|hz| hz.min(target) < fps_here) {
            continue;
        }
        if fill > best.1 .0 || (fill == best.1 .0 && scale > best.1 .1) {
            best = ((w, h), (fill, scale));
        }
    }
    match best.0 {
        (w, h) if (w, h) != (cw, ch) => Some((w, h, rate(w, h)?)),
        _ => keep,
    }
}

/// Office mode: the monitor at the client's exact resolution while it lives —
/// the desktop drawn natively at the size the client shows (crisp text,
/// nothing scaled) — then the mode it had (resolution and refresh) again.
/// Only where the monitor offers that exact mode, and never larger than now:
/// a near miss would be scaled twice. (A crashed agent's successor resets the
/// display to the saved mode anyway.)
pub(crate) struct NativeResolution {
    device: Vec<u16>,
    previous: windows::Win32::Graphics::Gdi::DEVMODEW,
}

impl NativeResolution {
    fn apply(device: &str, (w, h): (u32, u32)) -> Option<Self> {
        use windows::Win32::Graphics::Gdi::*;
        let wide: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
        let name = windows::core::PCWSTR(wide.as_ptr());
        unsafe {
            let mut cur = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            if !EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut cur).as_bool() {
                return None;
            }
            if (cur.dmPelsWidth, cur.dmPelsHeight) == (w, h) || (w as u64 * h as u64) >= (cur.dmPelsWidth as u64 * cur.dmPelsHeight as u64) {
                return None;
            }
            // That resolution at the refresh rate now, else the fastest offered.
            let mut best: Option<DEVMODEW> = None;
            let mut i = 0;
            loop {
                let mut m = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
                if !EnumDisplaySettingsW(name, ENUM_DISPLAY_SETTINGS_MODE(i), &mut m).as_bool() {
                    break;
                }
                i += 1;
                if (m.dmPelsWidth, m.dmPelsHeight) != (w, h) || m.dmBitsPerPel != cur.dmBitsPerPel {
                    continue;
                }
                let rank = |d: &DEVMODEW| (d.dmDisplayFrequency == cur.dmDisplayFrequency, d.dmDisplayFrequency);
                if best.as_ref().is_none_or(|b| rank(&m) > rank(b)) {
                    best = Some(m);
                }
            }
            let Some(mut best) = best else {
                tracing::info!(width = w, height = h, "the host monitor has no mode at the client's resolution; scaling instead");
                return None;
            };
            best.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
            let r = ChangeDisplaySettingsExW(name, Some(&best), None, CDS_TYPE(0), None);
            if r != DISP_CHANGE_SUCCESSFUL {
                tracing::warn!(code = r.0, width = w, height = h, "could not switch the host monitor to the client's resolution");
                return None;
            }
            tracing::info!(
                from = format!("{}x{}", cur.dmPelsWidth, cur.dmPelsHeight),
                to = format!("{w}x{h}"),
                hz = best.dmDisplayFrequency,
                "host monitor at the client's resolution (office mode)"
            );
            // The mode change resets duplication; give the driver a moment.
            std::thread::sleep(Duration::from_millis(500));
            cur.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
            Some(Self { device: wide, previous: cur })
        }
    }
}

impl Drop for NativeResolution {
    fn drop(&mut self) {
        use windows::Win32::Graphics::Gdi::*;
        unsafe {
            let r = ChangeDisplaySettingsExW(windows::core::PCWSTR(self.device.as_ptr()), Some(&self.previous), None, CDS_TYPE(0), None);
            tracing::info!(
                ok = r == DISP_CHANGE_SUCCESSFUL,
                to = format!("{}x{}", self.previous.dmPelsWidth, self.previous.dmPelsHeight),
                "host monitor resolution back"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

impl Drop for DisplayMode {
    fn drop(&mut self) {
        use windows::Win32::Graphics::Gdi::*;
        unsafe {
            // Back to the mode saved in the registry (the user's own setting).
            let _ = ChangeDisplaySettingsExW(windows::core::PCWSTR(self.device.as_ptr()), None, None, CDS_TYPE(0), None);
        }
    }
}

fn is_remote_session() -> bool {
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

fn current_session_id() -> u32 {
    let mut id = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(std::process::id(), &mut id);
    }
    id
}

fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}


/// Host clock in µs on the same base as Desktop Duplication's present times.
pub fn qpc_us() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as f64 * 1_000_000.0 / f.max(1) as f64) as u64
}

/// `rdpio --hyper-install`: start the agent elevated at every logon, and let
/// its UDP port through the firewall.
pub fn install(uninstall: bool, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    const TASK: &str = "rdpio Hyper host";
    const RULE: &str = "rdpio Hyper (UDP)";
    if !is_elevated() {
        return Err("run this from an elevated (Administrator) PowerShell".into());
    }
    // System tools by absolute path (never a bare name an attacker could
    // plant next to this exe).
    let tool = crate::hyper_direct::system_tool;
    let firewall = |args: &[&str]| std::process::Command::new(tool("netsh.exe")).args(["advfirewall", "firewall"]).args(args).output();
    let _ = firewall(&["delete", "rule", &format!("name={RULE}")]);
    if uninstall {
        // (Captured, not printed: the service install calls this to clear an
        // old task, and "no such task" there is no error worth showing.)
        let s = std::process::Command::new(tool("schtasks.exe")).args(["/Delete", "/F", "/TN", TASK]).output()?;
        if !s.status.success() {
            return Err(format!("schtasks /Delete failed: {}", String::from_utf8_lossy(&s.stderr).trim()).into());
        }
        println!("Removed the '{TASK}' logon task and the '{RULE}' firewall rule.");
        return Ok(());
    }
    let added = firewall(&[
        "add",
        "rule",
        &format!("name={RULE}"),
        "dir=in",
        "action=allow",
        "protocol=UDP",
        &format!("localport={port}"),
    ])?;
    if !added.status.success() {
        return Err(format!("could not add the firewall rule: {}", String::from_utf8_lossy(&added.stdout).trim()).into());
    }
    // Direct mode's service would fight this agent over the port.
    // (it gets time to restore the console, and stops using the installed exe
    // that's about to be refreshed).
    let _ = crate::hyper_direct::remove_service();
    // The elevated task runs a copy only administrators can change (from a
    // user-writable folder, swapping the exe would be a free elevation).
    let exe = crate::hyper_direct::installed_exe()?;
    // Not the user's %TEMP%: an elevated process writing (and renaming) a
    // file in a folder the user controls can be redirected through links.
    let log = crate::hyper_direct::data_dir().join("hyper-host.log");
    let cmd = format!("\"{}\" --hyper-host --hyper-port {port} --log-file \"{}\"", exe.display(), log.display());
    let s = std::process::Command::new(tool("schtasks.exe"))
        .args(["/Create", "/F", "/TN", TASK, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &cmd])
        .status()?;
    if !s.success() {
        return Err("schtasks /Create failed (run this from an elevated PowerShell)".into());
    }
    // schtasks can't set these: Task Scheduler would kill the agent after 72 h
    // and on battery power.
    let _ = std::process::Command::new(tool(r"WindowsPowerShell\v1.0\powershell.exe"))
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Set-ScheduledTask -TaskName '{TASK}' -Settings (New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0 -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)) | Out-Null"
            ),
        ])
        .status();
    let _ = std::process::Command::new(tool("schtasks.exe")).args(["/Run", "/TN", TASK]).status();
    println!(
        "Installed the '{TASK}' logon task (started now and at every sign-in) and allowed UDP {port} through the firewall.\nAgent log: {}",
        log.display()
    );
    Ok(())
}

/// `rdpio --hyper-selftest [secs]`: the whole pipeline on this machine, no
/// RDP — capture the primary monitor (a small window flashes colours so there
/// is motion), encode, FEC + encrypt, send over loopback UDP, reassemble and
/// decode on a separate D3D11 device, and report frame rate and
/// capture→decoded latency on one shared clock.
pub fn selftest(secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    use rdp_hyper::video::{Event, Reassembler};

    let key = random_key();
    let host_sock = UdpSocket::bind("127.0.0.1:0")?;
    let client = UdpSocket::bind("127.0.0.1:0")?;
    crate::hyper_client::set_socket_buffers(&host_sock);
    crate::hyper_client::set_socket_buffers(&client);
    client.connect(host_sock.local_addr()?)?;
    let mut csend = Session::new(&key, Direction::ClientToHost);
    let mut crecv = Session::new(&key, Direction::ClientToHost);
    let mut hrecv = Session::new(&key, Direction::HostToClient);
    let hsend = Arc::new(Mutex::new(Session::new(&key, Direction::HostToClient)));
    // The first ping tells the host where the client is.
    client.send(&csend.seal(Kind::Control, &Control::Ping { id: 1, client_us: 0 }.to_bytes()))?;
    let mut buf = [0u8; 2048];
    let (n, peer) = host_sock.recv_from(&mut buf)?;
    hrecv.open(&buf[..n]).map_err(|e| format!("selftest ping: {e}"))?;

    let monitor = list_monitors()?.into_iter().next().ok_or("no monitor")?;
    // --hyper-office: start in office mode, switch to game mode halfway.
    let office = std::env::args().any(|a| a == "--hyper-office");
    // Office text as clients ask for it: fast unless `--hyper-text best`.
    let fast_text = !std::env::args().any(|a| a == "best");
    let opts = HostOptions { port: 0, max_fps: None, max_mbps: 80, stream_size: None, fit_to: None, office, fast_text, keep_monitors: true };
    let host = std::thread::spawn(move || {
        let codec = if std::env::args().any(|a| a == "--hevc") { rdp_gpu::h264::Codec::Hevc } else { rdp_gpu::h264::Codec::H264 };
        if let Err(e) = stream(host_sock, peer, hrecv, hsend, &opts, &monitor.device_name, 120, codec, false) {
            tracing::error!(error = %e, "selftest host failed");
        }
    });

    // Motion source, on its own thread so presenting never stalls the receiver.
    let painting = Arc::new(AtomicBool::new(true));
    let paint_flag = painting.clone();
    let painter = std::thread::spawn(move || {
        crate::window::SHOW_INACTIVE.store(true, Ordering::Relaxed);
        // `--small-motion`: a speck of motion (a clock, a caret), not a scene.
        let (pw, ph) = if std::env::args().any(|a| a == "--small-motion") { (24, 24) } else { (480, 270) };
        let Ok(window) = crate::window::Window::new("rdpio Hyper selftest", pw, ph) else { return };
        let Ok(mut renderer) = rdp_gpu::Renderer::new(window.hwnd_raw(), pw, ph, rdp_gpu::Backend::D3D11) else { return };
        let mut color = 0u32;
        while paint_flag.load(Ordering::Relaxed) {
            color = color.wrapping_add(1);
            let c = (color % 64) as f32 / 63.0;
            let _ = window.pump();
            let _ = renderer.present_clear([c, 1.0 - c, (c * 3.0) % 1.0, 1.0]);
            // ~250 fps: plenty of motion without flooding the GPU queue the
            // encoder shares.
            std::thread::sleep(Duration::from_millis(4));
        }
    });

    // Client: decode on its own device, like a real client would.
    let (dev, ctx) = unsafe {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        let (mut d, mut c) = (None, None);
        D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT, None,
            D3D11_SDK_VERSION, Some(&mut d), None, Some(&mut c))?;
        (d.unwrap(), c.unwrap())
    };
    let mut decoder: Option<rdp_gpu::h264::H264GpuDecoder> = None;
    let mut reasm = Reassembler::new();
    client.set_read_timeout(Some(Duration::from_millis(1)))?;
    let start = Instant::now();
    let mut lat = Vec::new();
    let (mut frames, mut keyframes, mut lost, mut bytes) = (0u32, 0u32, 0u32, 0u64);
    let mut last_fb = Instant::now();
    // `--drop-every MS`: every MS ms, 20 ms of video packets are lost on the
    // way, so frames go missing past what FEC rebuilds. The client then does
    // what the real one does: asks for a repair (a frame coded from a kept
    // reference it has), a keyframe if none comes.
    let drop_every = std::env::args()
        .skip_while(|a| a != "--drop-every")
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);
    let (mut dropped, mut repairs, mut skipped) = (0u32, 0u32, 0u32);
    let (mut awaiting, mut can_repair, mut awaiting_since, mut last_ask) = (false, false, Instant::now(), Instant::now());
    let mut frozen = Duration::ZERO;
    let mut ltr_decoded: Option<u32> = None;
    let reliable = crate::hyper_link::ReliableLink::new();
    let (mut shapes, mut visibility) = (0u32, 0u32);
    let mut switched = !office;
    while start.elapsed() < Duration::from_secs(secs) {
        if !switched && start.elapsed() > Duration::from_secs(secs) / 2 {
            switched = true;
            println!("switching to game mode after {frames} frames");
            let _ = client.send(&csend.seal(Kind::Control, &Control::Mode { office: false, fast_text: false }.to_bytes()));
        }
        // No pump here: flush batched reliable acks by hand.
        for a in reliable.due_acks() {
            let _ = client.send(&csend.seal(Kind::Reliable, &a));
        }
        // Feedback on a timer, like the real client (it also keeps the host
        // from deciding the client went quiet while video pauses).
        if last_fb.elapsed() > Duration::from_millis(50) {
            let s = reasm.take_stats();
            let fb = rdp_hyper::control::Feedback {
                shards: s.shards,
                recovered: s.recovered,
                frames: s.frames,
                frames_lost: s.frames_lost,
                bytes: s.bytes,
                delay_min_us: 0,
                delay_avg_us: 0,
                window_us: last_fb.elapsed().as_micros() as u32,
                ltr_frame: ltr_decoded,
            };
            let _ = client.send(&csend.seal(Kind::Control, &Control::Feedback(fb).to_bytes()));
            last_fb = Instant::now();
        }
        if awaiting && last_ask.elapsed() > Duration::from_millis(100) {
            let repair = can_repair && awaiting_since.elapsed() < Duration::from_secs(1);
            let ask = if repair { Control::Recover } else { Control::KeyframeRequest };
            let _ = client.send(&csend.seal(Kind::Control, &ask.to_bytes()));
            last_ask = Instant::now();
        }
        let Ok(n) = client.recv(&mut buf) else { continue };
        let Ok((kind, body)) = crecv.open(&buf[..n]) else { continue };
        if kind == Kind::Reliable {
            let (ack, msgs) = reliable.on_packet(&body);
            if let Some(a) = ack {
                let _ = client.send(&csend.seal(Kind::Reliable, &a));
            }
            for m in msgs {
                match m {
                    rdp_hyper::messages::Message::CursorShape { .. } => shapes += 1,
                    rdp_hyper::messages::Message::CursorVisible(_) => visibility += 1,
                    _ => {}
                }
            }
            continue;
        }
        if kind != Kind::Video {
            continue;
        }
        if drop_every.is_some_and(|every| start.elapsed().as_millis() % every.as_millis() < 20) {
            dropped += 1;
            continue;
        }
        bytes += n as u64;
        for ev in reasm.push(&body, qpc_us()) {
            match ev {
                Event::Lost { first, last } => {
                    lost += last.wrapping_sub(first) + 1;
                    if !awaiting {
                        (awaiting, awaiting_since) = (true, Instant::now());
                        last_ask = Instant::now() - Duration::from_secs(1);
                    }
                }
                Event::Frame(f) => {
                    if awaiting && !f.keyframe && !(f.recovery && can_repair) {
                        skipped += 1;
                        continue;
                    }
                    if awaiting {
                        frozen += awaiting_since.elapsed();
                        awaiting = false;
                        repairs += (f.recovery && !f.keyframe) as u32;
                    }
                    if f.keyframe {
                        can_repair = true;
                    }
                    if decoder.is_none() {
                        let codec = if std::env::args().any(|a| a == "--hevc") { rdp_gpu::h264::Codec::Hevc } else { rdp_gpu::h264::Codec::H264 };
                        decoder = Some(rdp_gpu::h264::H264GpuDecoder::with_codec(monitor_w(&f), monitor_h(&f), &dev, &ctx, codec)?);
                    }
                    let out = match decoder.as_mut().unwrap().decode(&f.data) {
                        Ok(out) => out,
                        Err(e) => {
                            println!("decode error on frame {} (recovery {}): {e}", f.frame_id, f.recovery);
                            (awaiting, can_repair, awaiting_since) = (true, false, Instant::now());
                            continue;
                        }
                    };
                    if f.ltr {
                        ltr_decoded = Some(f.frame_id);
                    }
                    if !out.is_empty() {
                        frames += 1;
                        keyframes += f.keyframe as u32;
                        lat.push((qpc_us() - f.capture_us) as f64 / 1000.0);
                    }
                }
            }
        }
    }
    let _ = client.send(&csend.seal(Kind::Control, &Control::Bye.to_bytes()));
    painting.store(false, Ordering::Relaxed);
    let _ = painter.join();
    let _ = host.join();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| lat.get(((lat.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
    let secs_f = start.elapsed().as_secs_f64();
    println!(
        "Hyper selftest: {frames} frames decoded ({:.1} fps, {keyframes} keyframes, {lost} lost), {:.1} Mbps\n\
         capture -> decoded latency: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms
         reliable channel: {shapes} cursor shapes, {visibility} visibility changes
         losses: {dropped} video packets dropped, {repairs} repaired by a reference frame, {skipped} frames skipped while waiting, {:.0} ms frozen in all",
        frames as f64 / secs_f,
        bytes as f64 * 8.0 / secs_f / 1e6,
        p(0.5),
        p(0.95),
        p(1.0),
        frozen.as_secs_f64() * 1e3
    );
    Ok(())
}

/// The selftest decodes at the primary monitor's size.
fn monitor_w(_f: &rdp_hyper::video::InFrame) -> u32 {
    list_monitors().ok().and_then(|m| m.first().map(|m| m.width & !1)).unwrap_or(1920)
}
fn monitor_h(_f: &rdp_hyper::video::InFrame) -> u32 {
    list_monitors().ok().and_then(|m| m.first().map(|m| m.height & !1)).unwrap_or(1080)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_decoding_client_gets_at_most_its_screen() {
        // Same shape: exactly the screen.
        assert_eq!(fit_within((2560, 1440), (1920, 1080)), Some((1920, 1080)));
        // A 3:2 laptop: as wide as it is, the height to keep 16:9 (even).
        assert_eq!(fit_within((2560, 1440), (2256, 1504)), Some((2256, 1268)));
        // A portrait desktop on a landscape screen.
        assert_eq!(fit_within((1680, 2520), (1920, 1080)), Some((720, 1080)));
        // Already fits, or nothing sensible to fit to.
        assert_eq!(fit_within((1920, 1080), (2560, 1440)), None);
        assert_eq!(fit_within((1920, 1080), (1920, 1080)), None);
        assert_eq!(fit_within((2560, 1440), (0, 0)), None);
        assert_eq!(fit_within((0, 0), (1920, 1080)), None);
    }

    /// This session's record reads back right (the lock and the crash marker
    /// rely on it): someone is signed in, and it isn't locked — run from a
    /// signed-in, unlocked desktop: `cargo test -p rdp-client --bin rdpio session_record -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn this_sessions_record_reads_back() {
        let info = session_info(current_session_id()).expect("WTSSessionInfoEx");
        println!("session {}: locked {:?}, logon time {}", current_session_id(), info.locked, info.logon_time);
        assert!(info.logon_time > 0, "a signed-in session has a sign-in time");
        assert_eq!(info.locked, Some(false), "the desktop running the test isn't locked");
        assert!(!console_moved() || std::env::var_os("SESSIONNAME").is_some_and(|s| s != "Console"));
    }

    /// Office mode's text smoothing switch takes, and goes back: run on a
    /// signed-in desktop (it flips the font smoothing for a moment).
    /// `cargo test -p rdp-client --bin rdpio font_smoothing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn font_smoothing_switches_and_goes_back() {
        let original = font_smoothing_here(FONT_SMOOTHING_STANDARD).expect("set greyscale");
        let now = font_smoothing_here(original).expect("put it back");
        assert_eq!(now, FONT_SMOOTHING_STANDARD, "greyscale was set");
        let check = font_smoothing_here(original).unwrap();
        assert_eq!(check, original, "and the original is back");
        println!("font smoothing type {original} -> 1 -> {original}");
    }

    /// Office mode's native resolution: the primary monitor goes to 1920x1080
    /// and back to exactly what it was (it flickers for a moment).
    /// `cargo test -p rdp-client --bin rdpio native_resolution -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn native_resolution_switches_and_goes_back() {
        let m = list_monitors().unwrap().into_iter().next().unwrap();
        let before = (m.width, m.height);
        let native = NativeResolution::apply(&m.device_name, (1920, 1080)).expect("switched");
        let during = list_monitors().unwrap().into_iter().find(|x| x.device_name == m.device_name).unwrap();
        drop(native);
        let after = list_monitors().unwrap().into_iter().find(|x| x.device_name == m.device_name).unwrap();
        println!("{before:?} -> {:?} -> {:?}", (during.width, during.height), (after.width, after.height));
        assert_eq!((during.width, during.height), (1920, 1080));
        assert_eq!((after.width, after.height), before);
    }

    /// A 2560x1440 monitor at 144 Hz with the usual GPU-scaled modes.
    fn monitor_modes() -> Vec<(u32, u32, u32)> {
        let mut v = Vec::new();
        for (w, h) in [(2560, 1440), (1920, 1080), (1600, 900), (1366, 768), (1280, 720), (1920, 1200), (1680, 1050), (1600, 1200)] {
            for hz in [60, 120, 144] {
                v.push((w, h, hz));
            }
        }
        v.push((2560, 1600, 60)); // too big anyway
        v
    }

    #[test]
    fn the_host_takes_the_clients_resolution_or_the_closest() {
        let m = monitor_modes();
        let now = (2560, 1440, 144);
        let c = |modes: &[(u32, u32, u32)], now, client, fps| choose_mode(modes, now, client, fps, true);
        // Exactly the client's, at a whole multiple of the stream's rate
        // (144 Hz for 120 fps would skip unevenly).
        assert_eq!(c(&m, now, Some((1920, 1080)), 120), Some((1920, 1080, 120)));
        assert_eq!(c(&m, now, Some((1920, 1080)), 60), Some((1920, 1080, 120)));
        // 1920x1200 for a 16:10 screen.
        assert_eq!(c(&m, now, Some((1920, 1200)), 60), Some((1920, 1200, 120)));
        // The Surface (3:2, 2256x1504): nothing that size or larger fills
        // more without being enlarged, so the resolution stays (the rate
        // goes even).
        assert_eq!(c(&m, now, Some((2256, 1504)), 60), Some((2560, 1440, 120)));
        // A 1440x900 screen: 1680x1050 fills it, shrunk the least.
        assert_eq!(c(&m, now, Some((1440, 900)), 60), Some((1680, 1050, 120)));
        // 1600x900 for a 1536x864 screen (shrunk the least).
        assert_eq!(c(&m, now, Some((1536, 864)), 60), Some((1600, 900, 120)));
        // A client larger than the monitor: the resolution as it is.
        assert_eq!(c(&m, (2560, 1440, 120), Some((3840, 2160)), 60), None);
        // No client size: the rate only.
        assert_eq!(c(&m, (2560, 1440, 60), None, 120), Some((2560, 1440, 120)));
        assert_eq!(c(&m, (2560, 1440, 120), None, 60), None);
        // A resolution that would cost frames isn't taken.
        let slow = vec![(2560, 1440, 120), (1920, 1080, 60)];
        assert_eq!(c(&slow, (2560, 1440, 120), Some((1920, 1080)), 120), None);
        assert_eq!(c(&slow, (2560, 1440, 120), Some((1920, 1080)), 60), Some((1920, 1080, 60)));
    }

    #[test]
    fn a_240_hz_monitor_streams_120_fps_from_240_hz() {
        let m = vec![(2560, 1440, 60), (2560, 1440, 144), (2560, 1440, 240), (1920, 1080, 60), (1920, 1080, 120), (1920, 1080, 240)];
        let now = (2560, 1440, 60);
        assert_eq!(choose_mode(&m, now, Some((1920, 1080)), 120, true), Some((1920, 1080, 240)));
        assert_eq!(choose_mode(&m, now, Some((1920, 1080)), 60, true), Some((1920, 1080, 240)));
        assert_eq!(choose_mode(&m, now, Some((1920, 1080)), 120, false), Some((1920, 1080, 120)));
        // 144 fps: 240 isn't a multiple; 144 is.
        assert_eq!(choose_mode(&m, now, None, 144, true), Some((2560, 1440, 144)));
        // More frames come first: 165 fps from 240 Hz (none matches evenly).
        assert_eq!(choose_mode(&m, now, None, 165, true), Some((2560, 1440, 240)));
        // Already there.
        assert_eq!(choose_mode(&m, (2560, 1440, 240), None, 120, true), None);
        // A 59.94 Hz client (59): 240 Hz is still four of its frames.
        assert_eq!(choose_mode(&m, now, None, 59, true), Some((2560, 1440, 240)));
    }

    #[test]
    fn a_loss_is_repaired_from_a_reference_the_client_has() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut l = Ltr::new(1);
        let plain = |p: LtrPlan| matches!(p, LtrPlan::Encode { mark: None, from: None });
        // The first picture after a keyframe: no reference yet, a keyframe.
        assert!(matches!(l.plan(0, true, false, at(0)), LtrPlan::Keyframe));
        assert_eq!(l.sent(0, 100, true), (false, false));
        // Picture 1 kept in slot 0 (frame 101); the next ones not, for a while.
        assert!(matches!(l.plan(1, false, false, at(10)), LtrPlan::Encode { mark: Some(0), from: None }));
        assert_eq!(l.sent(1, 101, false), (true, false));
        assert!(plain(l.plan(2, false, false, at(20))));
        l.sent(2, 102, false);
        // Lost before the client said it has 101: a keyframe.
        assert!(matches!(l.plan(3, true, false, at(30)), LtrPlan::Keyframe));
        l.sent(3, 103, false);
        // The client confirms 101: a loss is repaired from slot 0.
        l.acked(101);
        assert!(matches!(l.plan(4, true, false, at(40)), LtrPlan::Encode { mark: None, from: Some(1) }));
        assert_eq!(l.sent(4, 104, false), (false, true));
        // A quarter second on, slot 1 is filled; unconfirmed, slot 0 still
        // repairs.
        assert!(matches!(l.plan(5, false, false, at(300)), LtrPlan::Encode { mark: Some(1), from: None }));
        assert_eq!(l.sent(5, 105, false), (true, false));
        assert!(matches!(l.plan(6, true, false, at(310)), LtrPlan::Encode { from: Some(1), .. }));
        l.sent(6, 106, false);
        // Confirmed: the newer slot 1.
        l.acked(105);
        assert!(matches!(l.plan(7, true, false, at(320)), LtrPlan::Encode { from: Some(2), .. }));
        l.sent(7, 107, false);
        // A third repair within the second: a keyframe instead.
        assert!(matches!(l.plan(8, true, false, at(330)), LtrPlan::Keyframe));
        l.sent(8, 108, true);
        // The keyframe cleared everything.
        assert!(matches!(l.plan(9, true, false, at(2000)), LtrPlan::Keyframe));
        // Refilling slot 0 drops what it held until confirmed again.
        let mut l = Ltr::new(1);
        l.plan(1, false, false, at(0));
        l.sent(1, 11, false);
        l.acked(11);
        l.plan(2, false, false, at(300));
        l.sent(2, 12, false);
        l.plan(3, false, false, at(600)); // slot 0 again
        l.sent(3, 13, false);
        assert!(matches!(l.plan(4, true, false, at(610)), LtrPlan::Keyframe), "11 was replaced; 12, 13 unconfirmed");
    }

    #[test]
    fn a_still_screen_trickles_until_someone_acts() {
        let frame = 16_667; // 60 fps
        let mut t = Trickle::default();
        // Someone typing: full rate, however small the changes.
        for _ in 0..10 {
            assert_eq!(t.pace(true, 50_000, frame), (frame, false));
            t.encoded(300, false, false);
        }
        // Hands off for a moment: ~12 fps.
        assert_eq!(t.pace(true, 400_000, frame).0, TRICKLE_INTERVAL_US);
        // Sharpening and keyframes say nothing about the change.
        t.encoded(90_000, true, false);
        t.encoded(90_000, false, true);
        assert_eq!(t.pace(true, 500_000, frame).0, TRICKLE_INTERVAL_US);
        // A key press: full rate at once, and the next picture goes now.
        assert_eq!(t.pace(true, 1_000, frame), (frame, true));
        assert_eq!(t.pace(true, 2_000, frame), (frame, false));
        // A big change (a video, a page) with nobody at the controls: full rate.
        assert_eq!(t.pace(true, 600_000, frame).0, TRICKLE_INTERVAL_US);
        t.encoded(40_000, false, false);
        assert_eq!(t.pace(true, 700_000, frame), (frame, true));
        // Small again for three frames: trickling again.
        for _ in 0..3 {
            t.encoded(300, false, false);
        }
        assert_eq!(t.pace(true, 800_000, frame).0, TRICKLE_INTERVAL_US);
        // Game mode never trickles.
        assert_eq!(t.pace(false, 800_000, 8_333).0, 8_333);
    }

    #[test]
    fn sharpening_scales_with_the_change() {
        assert_eq!(sharpen_frames(200, 60), 2, "a caret blink");
        assert_eq!(sharpen_frames(8 * 1024, 60), 8);
        assert_eq!(sharpen_frames(500_000, 60), 60, "a new page: a second's frames");
        assert_eq!(sharpen_frames(500_000, 1), 2);
    }

    #[test]
    fn only_a_client_that_asks_is_scaled() {
        let opts = HostOptions { port: 0, max_fps: None, max_mbps: 80, stream_size: None, fit_to: None, office: false, fast_text: false, keep_monitors: true };
        let mut want = rdp_hyper::auth::Want {
            codecs: vec!["h264".into()],
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            monitors: 1,
            fit: false,
            office: false,
            fast_text: false,
            cookie: None,
            scale: 0,
            ..Default::default()
        };
        assert_eq!(client_stream_size(&opts, (2560, 1440), &want), None);
        want.fit = true;
        assert_eq!(client_stream_size(&opts, (2560, 1440), &want), Some((1920, 1080)));
        // The host's own setting wins.
        let sized = HostOptions { stream_size: Some((1280, 720)), ..opts };
        assert_eq!(client_stream_size(&sized, (2560, 1440), &want), Some((1280, 720)));
    }
}
