//! Hyper Accelerator client (`rdpio --host … --hyper`).
//!
//! While the normal RDP session runs, [`HyperRedirector`] answers the host
//! agent on the private `RDPIO.HYPER` channel: it says hello, receives the UDP
//! port and session key, proves the UDP path with an encrypted ping, and tells
//! the host to go. The host then moves the session off RDP (the RDP connection
//! drops), and the UI loop hands its window and renderer to [`run`], which
//! plays the stream: FEC reassembly → GPU H.264 decode → present, with input,
//! feedback and clock-sync pings going back over the same socket.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rdp_gpu::h264::H264GpuDecoder;
use rdp_gpu::Renderer;
use rdp_graphics::redirect::DvcRedirector;
use rdp_hyper::control::{self, Control, Feedback, Handshake, CHANNEL_NAME, PROTOCOL};
use rdp_hyper::input::{InputEvent, InputSender};
use rdp_hyper::video::{Event, Reassembler};
use rdp_hyper::wire::{Direction, Kind, Session};

use crate::window::{Frame, RawInput, Window};

/// A working UDP path to the host agent, ready to stream.
pub struct Link {
    socket: UdpSocket,
    /// The probe's AEAD state carries on: restarting a sequence counter under
    /// the same key would reuse nonces.
    send: Session,
    recv: Session,
    /// Desktop size and refresh rate the host offered.
    size: (u32, u32),
    fps: u32,
    codec: rdp_gpu::h264::Codec,
    /// The host has a newer rdpio for us (fetched beside the stream).
    update: bool,
    /// No protocol in common: the session only fetches the host's rdpio.
    update_only: bool,
    /// The host's rdpio version.
    host_app: String,
}

/// This process downloaded an update (installed when it ends).
static DOWNLOADED: AtomicBool = AtomicBool::new(false);

static LINK: Mutex<Option<Link>> = Mutex::new(None);
/// `--hyper-vsync`: present on vblank instead of immediately (tearing).
static VSYNC: AtomicBool = AtomicBool::new(false);

/// Choose vsync'd presentation for Hyper (default: present immediately — the
/// lowest latency; a VRR display shows no tearing either way).
pub fn set_vsync(on: bool) {
    VSYNC.store(on, Ordering::Relaxed);
}

/// Resolve `host:port`, preferring IPv4: the host agent listens on IPv4
/// (Tailscale and ZeroTier both give IPv4 addresses), and a LAN name that also
/// has an IPv6 address would otherwise be tried over IPv6 first.
pub fn resolve(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address for the host"))
}
/// When `ClientReady` went out to the host (it switches over right after).
static READY_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Microseconds since this process's first call — the client clock.
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// True once the host has had time to switch over: the UI loop stops waiting
/// for the RDP connection to drop on its own.
pub fn handoff_due() -> bool {
    READY_AT.lock().unwrap().is_some_and(|t| t.elapsed() > Duration::from_secs(3))
}

/// The established link, once the handshake finished.
pub fn take_link() -> Option<Link> {
    READY_AT.lock().unwrap().is_some().then(|| LINK.lock().unwrap().take()).flatten()
}

/// The `RDPIO.HYPER` channel handler, chained into the RDP session's DVC mux.
pub struct HyperRedirector {
    host: IpAddr,
    hello: Vec<u8>,
    channel: Option<u32>,
    outbound: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl HyperRedirector {
    /// `host` is the RDP server's address (the agent listens there too);
    /// `fps` the frame rate to ask for.
    pub fn new(host: IpAddr, width: u32, height: u32, fps: u32, codec: Option<rdp_gpu::h264::Codec>) -> Self {
        // HEVC first where the GPU decodes it (faster through NVENC and
        // better for the bits; see the direct sign-in).
        let codecs = rdp_gpu::hyper::client_codecs(codec);
        let hello = Handshake::ClientHello {
            protocol: PROTOCOL,
            codecs,
            width,
            height,
            refresh_hz: fps,
            // One monitor is streamed (and played in one window): the host
            // turns its others off rather than leave windows where nobody
            // can see them.
            monitors: 1,
        }
        .to_bytes();
        Self { host, hello, channel: None, outbound: Default::default() }
    }
}

impl DvcRedirector for HyperRedirector {
    fn claims(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case(CHANNEL_NAME)
    }

    fn on_create(&mut self, channel_id: u32, _name: &str) -> bool {
        tracing::info!(channel_id, "Hyper host agent found; offering to accelerate");
        self.channel = Some(channel_id);
        self.outbound.lock().unwrap().push(self.hello.clone());
        true
    }

    fn on_data(&mut self, _channel_id: u32, message: &[u8]) {
        match Handshake::parse(message) {
            Some(Handshake::HostOffer { protocol, port, key, width, height, fps, codec }) => {
                let Some(key) = control::decode_key(&key) else {
                    tracing::warn!("Hyper offer carried a bad key");
                    return;
                };
                tracing::info!(protocol, port, width, height, fps, %codec, "Hyper offer received; probing UDP");
                let host = SocketAddr::new(self.host, port);
                let outbound = self.outbound.clone();
                let codec = rdp_gpu::h264::Codec::from_name(&codec).unwrap_or(rdp_gpu::h264::Codec::H264);
                let _ = std::thread::Builder::new().name("hyper-probe".into()).spawn(move || {
                    match probe(host, &key) {
                        Ok((socket, send, recv)) => {
                            *LINK.lock().unwrap() = Some(Link { socket, send, recv, size: (width, height), fps, codec, update: false, update_only: false, host_app: String::new() });
                            outbound.lock().unwrap().push(Handshake::ClientReady.to_bytes());
                            // (The session worker sleeps until there's something to send.)
                            crate::net_wait::worker_wake::signal();
                        }
                        Err(e) => tracing::warn!(%host, error = %e, "Hyper UDP path failed; staying on RDP"),
                    }
                });
            }
            Some(Handshake::HostDecline { reason }) => {
                tracing::warn!(%reason, "host declined Hyper; staying on RDP");
                LINK.lock().unwrap().take();
                READY_AT.lock().unwrap().take();
            }
            other => tracing::debug!(?other, "unexpected Hyper message"),
        }
    }

    fn on_close(&mut self, _channel_id: u32) {
        self.channel = None;
    }

    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
        let Some(id) = self.channel else { return Vec::new() };
        let msgs = std::mem::take(&mut *self.outbound.lock().unwrap());
        for m in &msgs {
            if matches!(Handshake::parse(m), Some(Handshake::ClientReady)) {
                tracing::info!("UDP path confirmed; asking the host to switch to Hyper");
                *READY_AT.lock().unwrap() = Some(Instant::now());
            }
        }
        msgs.into_iter().map(|m| (id, m)).collect()
    }
}

/// Large socket buffers: Windows' default 64 KB receive buffer overflows on
/// the first keyframe (100+ KB arriving within a few ms), which then can never
/// be rebuilt.
pub(crate) fn set_socket_buffers(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{setsockopt, SOCKET, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
    let size: i32 = 8 * 1024 * 1024;
    let bytes = size.to_ne_bytes();
    unsafe {
        let s = SOCKET(socket.as_raw_socket() as usize);
        setsockopt(s, SOL_SOCKET, SO_RCVBUF, Some(&bytes));
        setsockopt(s, SOL_SOCKET, SO_SNDBUF, Some(&bytes));
    }
}

/// Ping the agent until it answers (proves the port is reachable and the key
/// matches).
fn probe(host: SocketAddr, key: &[u8; 32]) -> std::io::Result<(UdpSocket, Session, Session)> {
    let socket = connect_socket(host)?;
    let mut send = Session::new(key, Direction::ClientToHost);
    let mut recv = Session::new(key, Direction::ClientToHost);
    ping_until_pong(&socket, &mut send, &mut recv, Duration::from_secs(8))?;
    Ok((socket, send, recv))
}

fn connect_socket(host: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(if host.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    socket.connect(host)?;
    set_socket_buffers(&socket);
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket)
}

/// Sealed pings until the host answers: proves the path and the key.
fn ping_until_pong(socket: &UdpSocket, send: &mut Session, recv: &mut Session, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut id = 0;
    while Instant::now() < deadline {
        id += 1;
        socket.send(&send.seal(Kind::Control, &Control::Ping { id, client_us: now_us() }.to_bytes()))?;
        let mut buf = [0u8; 2048];
        if let Ok(n) = socket.recv(&mut buf) {
            if let Ok((Kind::Control, body)) = recv.open(&buf[..n]) {
                if let Some(Control::Pong { client_us, .. }) = Control::parse(&body) {
                    tracing::info!(rtt_ms = (now_us() - client_us) as f64 / 1000.0, "Hyper host answered over UDP");
                    return Ok(());
                }
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer (UDP blocked by a firewall?)"))
}

/// `host clock - client clock`, from the lowest-RTT ping in recent history.
#[derive(Default)]
struct ClockSync {
    offset_us: i64,
    best_rtt_us: u64,
    at: Option<Instant>,
    rtt_us: u64,
}

impl ClockSync {
    fn sample(&mut self, client_sent_us: u64, host_us: u64) {
        let rtt = now_us().saturating_sub(client_sent_us);
        self.rtt_us = rtt;
        let stale = self.at.is_none_or(|t| t.elapsed() > Duration::from_secs(10));
        if stale || rtt <= self.best_rtt_us {
            self.best_rtt_us = rtt;
            self.offset_us = host_us as i64 - (client_sent_us + rtt / 2) as i64;
            self.at = Some(Instant::now());
        }
    }
}

struct Shared {
    socket: UdpSocket,
    send: Mutex<Session>,
    stop: AtomicBool,
    input_ack: AtomicU32,
    clock: Mutex<ClockSync>,
    /// Latest host status: bitrate (kbps) and FEC (‰).
    bitrate_kbps: AtomicU32,
    fec_permille: AtomicU32,
    host_fps: AtomicU32,
    bytes: AtomicU64,
    /// The stream size the host last reported.
    size: Mutex<(u32, u32)>,
    /// Why the stream stopped (set by the receive thread).
    host_bye: AtomicBool,
    lost: AtomicBool,
    /// Anything authentic came from the host since sign-in.
    heard: AtomicBool,
    /// Frames handed to the UI thread and not yet taken (the decode backlog).
    queued: AtomicU32,
    /// The newest long-term reference frame decoded (`u32::MAX`: none), told
    /// to the host with the feedback.
    ltr_decoded: AtomicU32,
    /// The host's newer rdpio, downloading beside the stream.
    /// Cameras this PC can send the host (`u32::MAX` until counted).
    cameras: AtomicU32,
    /// A notice from the host to show over the stream (`Control::Notice`).
    notice: Mutex<Option<String>>,
    update: Mutex<Option<rdp_hyper::update::Fetch>>,
    /// Its version, once downloaded (for a notice on screen).
    update_note: Mutex<Option<String>>,
}

/// Decoding a backlog that would take longer than this skips it for a fresh
/// keyframe instead (which costs a round trip and a bitrate spike). Frames
/// that arrive together after a Wi-Fi stall are decoded in a moment and only
/// the newest is shown: no keyframe for them.
const MAX_CATCH_UP_US: u64 = 50_000;

/// Whether decoding `backlog` frames at `decode_us` each takes too long.
fn skip_backlog(backlog: u32, decode_us: u64) -> bool {
    backlog as u64 * decode_us.max(200) > MAX_CATCH_UP_US
}
/// The bulk lane's pace toward the host (clipboard, files). The reliable
/// channel also backs off on its own once the round trip grows.
const CLIENT_BULK_BPS: u64 = 40_000_000;

/// How a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The user closed the window.
    Closed,
    /// The host ended it.
    HostLeft,
    /// The network went quiet — worth reconnecting.
    Lost,
    /// The host signed us in but never sent a thing (its stream didn't
    /// start): reconnecting may not help.
    NeverStarted,
}

impl Shared {
    fn send(&self, kind: Kind, body: &[u8]) {
        // Sealed under the lock, sent outside it (a thread preempted inside
        // the send mustn't hold up input); the host's replay window accepts
        // the slight reordering that allows.
        let packet = self.send.lock().unwrap().seal(kind, body);
        let _ = self.socket.send(&packet);
    }
}

/// GPU (DXVA) decode when the machine has it; Media Foundation's CPU decoder
/// otherwise (e.g. a laptop whose GPU driver is missing).
enum Decoder {
    Gpu(H264GpuDecoder),
    Cpu(rdp_gpu::h264::H264Decoder),
}

/// A decoded picture, on the GPU or in memory.
enum Picture {
    Gpu(rdp_gpu::h264::DecodedTexture),
    Cpu(rdp_gpu::h264::DecodedFrame),
}

impl Decoder {
    fn new(renderer: &Renderer, w: u32, h: u32, codec: rdp_gpu::h264::Codec) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some((device, context)) = renderer.device_context_clone() {
            // Pictures read where the decoder put them (each is drawn, then
            // dropped, before the next decode): unless that failed to draw.
            let made = if IN_PLACE_FAILED.load(Ordering::Relaxed) {
                H264GpuDecoder::with_codec(w, h, &device, &context, codec)
            } else {
                H264GpuDecoder::with_codec_in_place(w, h, &device, &context, codec)
            };
            match made {
                Ok(d) => return Ok(Self::Gpu(d)),
                Err(e) => tracing::warn!(error = %e, codec = codec.name(), "no GPU decoder (check the GPU driver); decoding on the CPU"),
            }
        }
        Ok(Self::Cpu(rdp_gpu::h264::H264Decoder::with_codec(w, h, codec)?))
    }

    /// Decode one access unit; the newest picture it produced, if any.
    fn decode(&mut self, data: &[u8]) -> windows::core::Result<Option<Picture>> {
        Ok(match self {
            Self::Gpu(d) => d.decode(data)?.into_iter().last().map(Picture::Gpu),
            Self::Cpu(d) => d.decode(data)?.into_iter().last().map(Picture::Cpu),
        })
    }
}

/// Play the Hyper stream in `window` until it's closed or the host leaves.
pub fn run(window: &Window, renderer: &mut Renderer, link: Link) -> Result<Ended, Box<dyn std::error::Error>> {
    crate::session::force_rel_mouse();
    // Win, Alt+Tab and the like reach the host while the stream is in front
    // (Ctrl+Shift+K gives them to this PC; Ctrl+Alt+Del always stays here).
    let _keys = crate::window::SystemKeysFollowCapture::on();
    // Present the moment a frame is decoded; waiting for vblank costs up to a
    // refresh interval and blocks the loop that also sends input.
    renderer.set_low_latency(!VSYNC.load(Ordering::Relaxed));
    // Pictures on a hardware overlay plane where this display has one (the
    // display engine converts and composes them; `RDPIO_VIDEO_LAYER=0` off).
    renderer.set_video_layer(std::env::var("RDPIO_VIDEO_LAYER").map_or(true, |v| v.trim() != "0"));
    // A stream of another shape than the window (a 16:9 desktop on a 3:2
    // laptop) keeps its shape: scaled to fit, black around it.
    renderer.set_keep_aspect(true);
    // Input from before the stream (a reconnect wait) is stale: drop it
    // rather than replay seconds of it into the game.
    let _ = window.drain_input();
    // The window may have changed size while no stream was playing (not
    // while minimized: an empty rect isn't a size).
    {
        use windows::Win32::Foundation::{HWND, RECT};
        let mut r = RECT::default();
        let got = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(HWND(window.hwnd_raw() as *mut _), &mut r) };
        if got.is_ok() && r.right > r.left && r.bottom > r.top {
            renderer.resize((r.right - r.left) as u32, (r.bottom - r.top) as u32)?;
        }
    }
    let (mut w, mut h) = link.size;
    renderer.ensure_framebuffer(w, h)?;
    let codec = link.codec;
    let mut decoder = Decoder::new(renderer, w, h, codec)?;
    tracing::info!(width = w, height = h, fps = link.fps, codec = codec.name(), "Hyper stream: playing");
    window.set_title(&format!("RDPiO — Hyper {w}x{h}"));

    let shared = Arc::new(Shared {
        socket: link.socket.try_clone()?,
        send: Mutex::new(link.send),
        stop: AtomicBool::new(false),
        input_ack: AtomicU32::new(0),
        clock: Mutex::new(ClockSync::default()),
        bitrate_kbps: AtomicU32::new(0),
        fec_permille: AtomicU32::new(0),
        host_fps: AtomicU32::new(link.fps),
        bytes: AtomicU64::new(0),
        size: Mutex::new(link.size),
        host_bye: AtomicBool::new(false),
        lost: AtomicBool::new(false),
        heard: AtomicBool::new(false),
        queued: AtomicU32::new(0),
        ltr_decoded: AtomicU32::new(u32::MAX),
        cameras: AtomicU32::new(u32::MAX),
        notice: Mutex::new(None),
        update: Mutex::new(link.update.then(|| rdp_hyper::update::Fetch::new(crate::updater::version(), crate::updater::build()))),
        update_note: Mutex::new(None),
    });
    {
        // (Counting cameras asks Media Foundation: off this thread.)
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("hyper-cameras".into()).spawn(move || {
            crate::wasapi::com_init();
            s.cameras.store(crate::hyper_camera::camera_count() as u32, Ordering::Relaxed);
        });
    }
    if link.update {
        tracing::info!(host_app = %link.host_app, "the host has a newer rdpio: downloading it beside the stream");
        let s = shared.clone();
        std::thread::Builder::new().name("hyper-update".into()).spawn(move || {
            crate::net_tune::eco_thread();
            fetch_beside_stream(s)
        })?;
    }
    // Wi-Fi without power-save naps or channel scans, and the stream's
    // packets marked as audio/video — for as long as it plays.
    // (These three are let go while no one can see the stream: see `paused`.)
    let mut wifi = crate::net_tune::WifiLowLatency::enable();
    let _qos = crate::net_tune::QosFlow::tag(&shared.socket, None);
    // This thread decodes and presents: scheduled like a game's own.
    let mut boost = crate::net_tune::ThreadBoost::games();
    let mut awake = Some(crate::net_tune::KeepAwake::new());
    // The decoder's waits at 1 ms, not the 15.6 ms default (see FineTimer) —
    // while anything happens: a still screen lets the laptop's timer coarsen
    // (see FINE_TIMER_IDLE).
    let mut fine_timer = Some(crate::net_tune::FineTimer::new());
    let mut last_activity = Instant::now();
    let (tx, rx) = mpsc::channel::<Event>();
    let (cursor_tx, cursor_rx) = mpsc::channel::<crate::session::CursorUpdate>();
    let reliable = crate::hyper_link::ReliableLink::new();
    reliable.set_bulk_rate(Some(CLIENT_BULK_BPS));
    let pump_stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let s = shared.clone();
        reliable.spawn_pump(move |chunk| s.send(Kind::Reliable, chunk), pump_stop.clone())
    };
    let clipboard = {
        crate::hyper_clipboard::ClipboardSync::start(reliable.clone()).map(Arc::new)
    };
    let rx_shared = shared.clone();
    let recv = link.recv;
    let socket = link.socket;
    let rx_reliable = reliable.clone();
    let rx_clipboard = clipboard.clone();
    let receiver = std::thread::Builder::new()
        .name("hyper-rx".into())
        .spawn(move || receive_loop(socket, recv, rx_shared, tx, rx_reliable, rx_clipboard, cursor_tx))?;
    let mut last_shape: Option<crate::session::CursorUpdate> = None;

    let mut input = InputSender::new();
    let mut held = Held::default();
    // An End-down consumed as Ctrl+Alt+Del: its repeats and key-up are
    // swallowed too, whatever Ctrl/Alt do meanwhile.
    let mut sas_end = false;
    // Minimized: the host is told to encode nothing (repeated, in case a
    // packet is lost); restored: a keyframe brings the picture back.
    let (mut paused, mut pause_sent) = (false, Instant::now());
    let mut last_input_send = Instant::now();
    let mut awaiting_key = true;
    // What the decoder holds is sound (only frames went missing): a repair
    // frame from the host will do instead of a keyframe. Not after a decode
    // error, and not before the first keyframe.
    let mut can_repair = false;
    let mut awaiting_since = Instant::now();
    // Keyframes the decoder rejected in a row (it gets replaced at 3).
    let mut bad_keyframes = 0u32;
    let mut last_key_request = Instant::now() - Duration::from_secs(1);
    let mut lat_ms: Vec<f64> = Vec::new();
    let (mut shown, mut decoded, mut lost) = (0u32, 0u32, 0u32);
    // Time in the decoder this second (total, longest call), in presenting,
    // and the process's CPU time (for a weak laptop: where its CPU goes).
    let (mut dec_us_total, mut dec_us_max) = (0u64, 0u64);
    // Recent decode time per frame (until measured, a slow decoder's).
    let mut decode_us_avg = 5_000u64;
    // When the power source was last looked at (see GPU_FOLLOWS_POWER).
    let mut power_checked = Instant::now();
    // The screen's refresh rate as last told to the host (see Control::Refresh),
    // and when (at which display epoch) it was last read.
    let (mut refresh_sent, mut refresh_hz) = (Instant::now() - Duration::from_secs(10), 0u32);
    let (mut refresh_read, mut refresh_epoch) = (Instant::now() - Duration::from_secs(60), u32::MAX);
    let mut last_title = String::new();
    let mut present_us_total = 0u64;
    let mut cpu_before = process_cpu_us();
    let mut report = Instant::now();
    let mut overlay: Option<(u16, u16, Vec<u8>)> = None;
    // The hotkeys, on screen for the stream's first seconds (later, a mode
    // change's notice for a moment).
    let mut hint_until = Instant::now() + HINT_FOR;
    let mut hint = crate::hyper_overlay::render(&hint_lines(matches!(decoder, Decoder::Cpu(_))));
    // Office or game mode: told to the host now and every second after.
    let (mut office_sent, mut mode_sent) = (crate::window::office_mode(), Instant::now() - Duration::from_secs(2));
    // Overlays are drawn over the picture: one that changes is presented even
    // without a new picture (and before the first one).
    let (mut stats_were_on, mut hint_was_on, mut overlay_dirty) = (false, false, true);
    // An overlay went away: a fresh picture is asked for unless one comes.
    let mut repaint_due: Option<Instant> = None;
    let mut paint_failed_logged = false;
    // The CPU colour conversion's buffer (see `paint`), kept between frames.
    let mut rgba: Vec<u8> = Vec::new();
    // The newest decoded picture not yet on screen: presented once the
    // swapchain has room (see `frame_ready`), replaced by a newer one
    // meanwhile.
    let mut pending: Option<(Picture, u64)> = None;
    // The last picture went straight to the screen (`present_nv12`), not
    // through the framebuffer: an overlay drawn over the framebuffer now
    // would show an older picture, so a fresh one is asked for.
    let mut framebuffer_stale = false;
    // Until the first picture: how long it's been (said on screen, not a
    // black window), and whether the wait was logged.
    let (started, mut ever_shown, mut said_waiting) = (Instant::now(), false, false);
    shared.send(Kind::Control, &Control::KeyframeRequest.to_bytes());

    // The loop lives in a closure so any error (a GPU reset, a decoder
    // failure) still runs the cleanup below: threads stopped, host told.
    let mut play = || -> Result<Ended, Box<dyn std::error::Error>> {
    let ended = loop {
        match window.pump() {
            Frame::Quit => break Ended::Closed,
            Frame::Continue { resize } => {
                if let Some((cw, ch)) = resize {
                    renderer.resize(cw, ch)?;
                }
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            if shared.lost.load(Ordering::Relaxed) {
                break if shared.heard.load(Ordering::Relaxed) { Ended::Lost } else { Ended::NeverStarted };
            }
            tracing::info!("host ended the Hyper stream");
            break Ended::HostLeft;
        }
        // The host changed resolution: new decoder and framebuffer, then wait
        // for the keyframe the host sends at the new size.
        let size = *shared.size.lock().unwrap();
        if size != (w, h) && size.0 > 0 && size.1 > 0 {
            tracing::info!(width = size.0, height = size.1, "host resolution changed");
            (w, h) = size;
            renderer.ensure_framebuffer(w, h)?;
            decoder = Decoder::new(renderer, w, h, codec)?;
            awaiting_key = true;
            // (Nothing to repair from in a new decoder.)
            can_repair = false;
            shared.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
            window.set_title(&format!("RDPiO — Hyper {w}x{h}"));
        }
        // The host's pointer, drawn locally (no round trip on mouse moves).
        while let Ok(update) = cursor_rx.try_recv() {
            use crate::session::CursorUpdate;
            match &update {
                CursorUpdate::Shape { .. } => last_shape = Some(update.clone()),
                CursorUpdate::Default => {
                    // "Visible again": restore the last shape.
                    if let Some(s) = last_shape.clone() {
                        window.set_cursor(s);
                        continue;
                    }
                }
                CursorUpdate::Hide => {}
            }
            window.set_cursor(update);
        }

        // Input → host. Pointer positions map to where the picture is.
        let client = client_size(window);
        let view = if renderer.keeps_aspect() { rdp_gpu::fit_rect((w, h), client) } else { (0, 0, client.0, client.1) };
        let raw = window.drain_input();
        let had_input = !raw.is_empty();
        // Input comes before the picture that answers it: the fine timer is
        // back before the host's reply is decoded.
        if had_input {
            last_activity = Instant::now();
            fine_timer.get_or_insert_with(crate::net_tune::FineTimer::new);
        }
        for r in raw {
            if matches!(r, RawInput::FocusLost) {
                // Release whatever is held: its key-up will go to the other window.
                for ev in held.release_all() {
                    input.push(ev);
                }
                sas_end = false;
                continue;
            }
            for ev in map_input(r, view) {
                // Ctrl+Alt+End = Ctrl+Alt+Del on the host (as in mstsc): the
                // real combination is reserved for this machine.
                let ev = match ev {
                    InputEvent::Key { scancode: 0x4F, extended: true, down: true } if sas_end => continue,
                    // (A fresh End press only: an End already held and passed
                    // through must keep its own key-up.)
                    InputEvent::Key { scancode: 0x4F, extended: true, down: true }
                        if held.ctrl_alt() && !held.is_down(0x4F, true) =>
                    {
                        tracing::info!("Ctrl+Alt+End: sending Ctrl+Alt+Del to the host");
                        sas_end = true;
                        InputEvent::SecureAttention
                    }
                    InputEvent::Key { scancode: 0x4F, extended: true, down: false } if sas_end => {
                        sas_end = false;
                        continue;
                    }
                    other => other,
                };
                held.note(&ev);
                input.push(ev);
            }
        }
        input.ack(shared.input_ack.load(Ordering::Relaxed));
        // New input goes at once — but pointer motion at most once a
        // millisecond, merged meanwhile: a 4-8 kHz gaming mouse otherwise
        // costs a Wi-Fi packet per report. Unacknowledged input is resent
        // every 15 ms.
        let unsent = input.unsent();
        let due = match unsent {
            Some(false) => true,
            Some(true) => last_input_send.elapsed() >= INPUT_MOTION_EVERY,
            None => false,
        };
        if input.has_pending() && (due || last_input_send.elapsed() > Duration::from_millis(15)) {
            // Oldest first; a burst bigger than a packet takes two at a time
            // (the rest follows as the host acknowledges).
            for p in input.packets(2) {
                shared.send(Kind::Input, &p);
            }
            last_input_send = Instant::now();
        }

        if fine_timer.is_some() && last_activity.elapsed() > FINE_TIMER_IDLE {
            fine_timer = None;
        }

        // The screen's refresh rate, to the host: a laptop that drops to 60 Hz
        // on battery gets 60 frames a second, not the 240 it can't show.
        // (Read again only after a display, monitor or power change, and now
        // and then: asking the driver costs milliseconds on this thread.)
        if refresh_sent.elapsed() >= Duration::from_secs(3) {
            let epoch = crate::window::display_epoch();
            if epoch != refresh_epoch || refresh_read.elapsed() >= Duration::from_secs(15) {
                let hz = window_refresh_hz(window);
                if hz != refresh_hz && refresh_hz != 0 {
                    tracing::info!(hz, "this screen's refresh rate changed; telling the host");
                }
                (refresh_hz, refresh_epoch, refresh_read) = (hz, epoch, Instant::now());
            }
            shared.send(Kind::Control, &Control::Refresh { hz: refresh_hz.min(u16::MAX as u32) as u16 }.to_bytes());
            // (And whether there's a camera to offer the host's apps.)
            let cameras = shared.cameras.load(Ordering::Relaxed);
            if cameras != u32::MAX {
                shared.send(Kind::Control, &Control::Cameras { count: cameras.min(u8::MAX as u32) as u8 }.to_bytes());
            }
            refresh_sent = Instant::now();
        }
        // Plugged in or unplugged: decoding and presenting move to the GPU
        // that suits it, in place (a fresh picture follows; the stream goes on).
        if GPU_FOLLOWS_POWER.load(Ordering::Relaxed) && power_checked.elapsed() >= Duration::from_secs(3) {
            power_checked = Instant::now();
            let display = wants_display_gpu();
            if display != rdp_gpu::gpu::prefers_display_adapter() {
                rdp_gpu::gpu::prefer_display_adapter(display);
                let on = renderer.device_context_clone().and_then(|(d, _)| rdp_gpu::gpu::device_adapter_luid(&d));
                if on != rdp_gpu::gpu::chosen_adapter_luid() && renderer.rebuild()? {
                    tracing::info!(display_gpu = rdp_gpu::gpu::prefers_display_adapter(), "power source changed: decoding and presenting moved to the other GPU");
                    decoder = Decoder::new(renderer, w, h, codec)?;
                    awaiting_key = true;
                    can_repair = false;
                    shared.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
                    awaiting_since = Instant::now();
                    last_key_request = Instant::now() - Duration::from_secs(1);
                    overlay_dirty = true;
                }
            }
        }

        // Frames → screen. Decode every frame in order (P-frames reference
        // their predecessor); present only the newest. A backlog that would
        // take long to decode (a slow decoder that fell behind) isn't caught
        // up on: drop it and resume from a fresh keyframe.
        let backlog = shared.queued.load(Ordering::Relaxed);
        if skip_backlog(backlog, decode_us_avg) && !awaiting_key {
            tracing::info!(backlog, decode_us_avg, "decode backlog; skipping to a fresh keyframe");
            awaiting_key = true;
            last_key_request = Instant::now() - Duration::from_secs(1);
        }
        let mut newest = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Lost { first, last } => {
                    lost += last.wrapping_sub(first) + 1;
                    if !awaiting_key {
                        awaiting_since = Instant::now();
                        last_key_request = Instant::now() - Duration::from_secs(1);
                    }
                    awaiting_key = true;
                }
                Event::Frame(f) => {
                    shared.queued.fetch_sub(1, Ordering::Relaxed);
                    if awaiting_key && !f.keyframe && !(f.recovery && can_repair) {
                        continue;
                    }
                    let t_dec = Instant::now();
                    let result = decoder.decode(&f.data);
                    let dec_us = t_dec.elapsed().as_micros() as u64;
                    (dec_us_total, dec_us_max) = (dec_us_total + dec_us, dec_us_max.max(dec_us));
                    decode_us_avg = (decode_us_avg * 7 + dec_us) / 8;
                    match result {
                        Ok(picture) => {
                            awaiting_key = false;
                            bad_keyframes = 0;
                            decoded += 1;
                            last_activity = Instant::now();
                            fine_timer.get_or_insert_with(crate::net_tune::FineTimer::new);
                            if f.keyframe {
                                can_repair = true;
                            }
                            if f.ltr {
                                shared.ltr_decoded.store(f.frame_id, Ordering::Relaxed);
                            }
                            if let Some(p) = picture {
                                newest = Some((p, f.capture_us));
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "Hyper decode failed; waiting for a keyframe");
                            awaiting_key = true;
                            can_repair = false;
                            // A GPU reset shows here first (frames are decoded
                            // before anything is presented): hand it up, so the
                            // renderer is rebuilt and the stream rejoined.
                            if let Some((device, _)) = renderer.device_context_clone() {
                                if let Err(reason) = unsafe { device.GetDeviceRemovedReason() } {
                                    return Err(format!("the GPU was reset ({reason})").into());
                                }
                            }
                            // A decoder that can't even take keyframes: a new
                            // one — on the CPU when the GPU's keeps failing.
                            if f.keyframe {
                                bad_keyframes += 1;
                                if bad_keyframes >= 3 {
                                    bad_keyframes = 0;
                                    decoder = match decoder {
                                        Decoder::Gpu(_) => {
                                            tracing::warn!(error = %e, "the GPU decoder keeps failing; decoding on the CPU");
                                            can_repair = false;
                                            shared.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
                                            Decoder::Cpu(rdp_gpu::h264::H264Decoder::with_codec(w, h, codec)?)
                                        }
                                        Decoder::Cpu(_) => return Err(format!("the video decoder keeps failing: {e}").into()),
                                    };
                                }
                            }
                        }
                    }
                }
            }
        }
        // (Slower while keyframes keep failing to decode: each costs the host
        // a bitrate spike.)
        let key_interval = if bad_keyframes > 0 { Duration::from_millis(500) } else { Duration::from_millis(100) };
        // Lost frames: a repair (asked again only after a round trip); a
        // second without one, or a broken decoder: a keyframe.
        let rtt = Duration::from_micros(shared.clock.lock().unwrap().rtt_us as u64);
        let repair = can_repair && awaiting_since.elapsed() < Duration::from_secs(1);
        let interval = if repair { key_interval.max(rtt * 2) } else { key_interval };
        if awaiting_key && last_key_request.elapsed() > interval {
            let ask = if repair { Control::Recover } else { Control::KeyframeRequest };
            shared.send(Kind::Control, &ask.to_bytes());
            last_key_request = Instant::now();
        }
        // No one can see the stream (minimized, on another virtual desktop,
        // this screen off, this PC locked): the host is told to encode
        // nothing; what still comes is decoded (every frame is a reference for
        // the next) but not drawn.
        let minimized = window.output_hidden();
        if let Some(v) = shared.update_note.lock().unwrap().take() {
            hint = crate::hyper_overlay::render(&[format!("rdpio {v} downloaded from the host: it installs when you close this window")]);
            hint_until = Instant::now() + Duration::from_secs(6);
        }
        if let Some(text) = shared.notice.lock().unwrap().take() {
            hint = crate::hyper_overlay::render(&[text]);
            hint_until = Instant::now() + Duration::from_secs(10);
        }
        let office = crate::window::office_mode();
        if office != office_sent || mode_sent.elapsed() > Duration::from_secs(1) {
            if office != office_sent {
                tracing::info!(office, "stream mode (Ctrl+Shift+O)");
                hint = crate::hyper_overlay::render(&[if office {
                    "Office mode: crisp text, 60 fps".to_string()
                } else {
                    "Game mode: lowest latency".to_string()
                }]);
                hint_until = Instant::now() + Duration::from_secs(2);
            }
            shared.send(Kind::Control, &Control::Mode { office, fast_text: FAST_TEXT.load(Ordering::Relaxed) }.to_bytes());
            (office_sent, mode_sent) = (office, Instant::now());
        }
        // Repeated every second either way: a late "paused" can't leave the
        // host paused behind a visible window.
        if minimized != paused || pause_sent.elapsed() > Duration::from_secs(1) {
            shared.send(Kind::Control, &Control::Paused { paused: minimized }.to_bytes());
            if paused && !minimized {
                awaiting_key = true;
                last_key_request = Instant::now() - Duration::from_secs(1);
                // Seen again: the stream's holds come back before its picture.
                wifi = crate::net_tune::WifiLowLatency::enable();
                boost = crate::net_tune::ThreadBoost::games();
                awake = Some(crate::net_tune::KeepAwake::new());
                tracing::info!("stream visible again");
            } else if !paused && minimized {
                // Nothing to show: Wi-Fi may nap and scan again, the screen
                // and the PC may sleep, this thread is ordinary, the timer
                // coarse.
                (wifi, boost, awake, fine_timer) = (None, None, None, None);
                tracing::info!("stream hidden: paused, Wi-Fi and power settings handed back");
            }
            (paused, pause_sent) = (minimized, Instant::now());
        }
        // Overlays: the stats (Ctrl+Shift+S) and, for the first seconds, the
        // hotkeys — shown before the first picture too, so even a stream that
        // shows nothing says how to leave it. One that goes away needs a
        // fresh picture under it.
        // Signed in, but no picture seconds in (the hotkeys' hint gone): say
        // so on screen, and in the log once.
        if !ever_shown && !minimized && started.elapsed() > Duration::from_secs(3) && Instant::now() >= hint_until {
            let secs = started.elapsed().as_secs();
            hint = crate::hyper_overlay::render(&[
                format!("Signed in to the host - waiting for its picture ({secs} s)..."),
                "The rdpio window's Activity page shows what's happening.   Ctrl+Shift+Q quits".to_string(),
            ]);
            hint_until = Instant::now() + Duration::from_millis(1100);
            overlay_dirty = true;
            if secs >= 10 && !std::mem::replace(&mut said_waiting, true) {
                tracing::warn!(secs, heard = shared.heard.load(Ordering::Relaxed), awaiting_key, "no picture from the host yet");
            }
        }
        let stats_on = crate::window::stats_overlay();
        let hint_on = hint.is_some() && Instant::now() < hint_until;
        if stats_on != stats_were_on || hint_on != hint_was_on {
            overlay_dirty = true;
            if (stats_were_on && !stats_on) || (hint_was_on && !hint_on) {
                // The next picture paints over it; without one soon (a still
                // screen), a fresh one is asked for.
                repaint_due = Some(Instant::now() + Duration::from_millis(150));
            }
        }
        (stats_were_on, hint_was_on) = (stats_on, hint_on);
        if minimized {
            repaint_due = None; // (restoring asks for a keyframe anyway)
        } else if repaint_due.is_some_and(|t| Instant::now() >= t) {
            repaint_due = None;
            shared.send(Kind::Control, &Control::KeyframeRequest.to_bytes());
        }
        if newest.is_some() {
            pending = newest;
        }
        if minimized {
            pending = None;
        }
        // Presented when the screen can take it now; never waited for here
        // (that held up decoding and input a refresh or two per frame where
        // Windows composes the window). Else kept, and the wait below wakes
        // for it.
        let ready = (pending.is_some() || overlay_dirty) && !minimized && renderer.frame_ready();
        let picture = if ready { pending.take() } else { None };
        // One pass where it can: a GPU picture shown 1:1 with no overlay goes
        // straight into the backbuffer.
        let direct = match &picture {
            Some((Picture::Gpu(t), _)) if ready && !stats_on && !hint_on => {
                let t_present = Instant::now();
                let done = renderer.present_nv12(&t.texture, t.slice, (w, h))?;
                if done {
                    present_us_total += t_present.elapsed().as_micros() as u64;
                }
                done
            }
            _ => false,
        };
        if direct {
            let capture_us = picture.map_or(0, |(_, c)| c);
            framebuffer_stale = true;
            repaint_due = None;
            overlay_dirty = false;
            shown += 1;
            let offset = shared.clock.lock().unwrap().offset_us;
            let host_now = now_us() as i64 + offset;
            lat_ms.push((host_now - capture_us as i64) as f64 / 1000.0);
        } else if ready {
            if picture.is_some() {
                framebuffer_stale = false;
            } else if std::mem::take(&mut framebuffer_stale) {
                // (An overlay over an older picture: a fresh one follows.)
                shared.send(Kind::Control, &Control::KeyframeRequest.to_bytes());
            }
            let captured = picture.and_then(|(p, capture_us)| {
                let in_place = matches!(&p, Picture::Gpu(t) if t.held.is_some());
                let painted = paint(renderer, p, (w, h), &mut rgba);
                if !painted && in_place && IN_PLACE_FAILED.load(Ordering::Relaxed) {
                    // A decoder that copies, and a fresh picture for it.
                    match Decoder::new(renderer, w, h, codec) {
                        Ok(d) => decoder = d,
                        Err(e) => tracing::warn!(error = %e, "no new decoder"),
                    }
                    awaiting_key = true;
                    can_repair = false;
                    shared.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
                    last_key_request = Instant::now() - Duration::from_secs(1);
                } else if !painted && !std::mem::replace(&mut paint_failed_logged, true) {
                    tracing::warn!(width = w, height = h, "a decoded picture couldn't be drawn (said once)");
                }
                painted.then_some(capture_us)
            });
            if captured.is_some() {
                repaint_due = None;
            }
            let mut y = 16u16;
            if stats_on {
                if let Some((ow, oh, px)) = &overlay {
                    renderer.update_rect(16, y, *ow, *oh, px);
                    y = y.saturating_add(*oh + 8);
                }
            }
            if hint_on {
                if let Some((hw, hh, px)) = &hint {
                    renderer.update_rect(16, y, *hw, *hh, px);
                }
            }
            let t_present = Instant::now();
            renderer.present_frame()?;
            present_us_total += t_present.elapsed().as_micros() as u64;
            overlay_dirty = false;
            if let Some(capture_us) = captured {
                shown += 1;
                let offset = shared.clock.lock().unwrap().offset_us;
                let host_now = now_us() as i64 + offset;
                lat_ms.push((host_now - capture_us as i64) as f64 / 1000.0);
            }
        }

        ever_shown |= shown > 0;
        if report.elapsed() > Duration::from_secs(1) {
            let secs = report.elapsed().as_secs_f64();
            lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| lat_ms.get(((lat_ms.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
            let rtt = shared.clock.lock().unwrap().rtt_us as f64 / 1000.0;
            let mbps = shared.bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / secs / 1e6;
            tracing::info!(
                target: "perf",
                shown_fps = format!("{:.1}", shown as f64 / secs),
                decoded_fps = format!("{:.1}", decoded as f64 / secs),
                latency_ms_p50 = format!("{:.1}", p(0.5)),
                latency_ms_p95 = format!("{:.1}", p(0.95)),
                decode_ms_avg = format!("{:.2}", dec_us_total as f64 / decoded.max(1) as f64 / 1000.0),
                decode_ms_max = format!("{:.1}", dec_us_max as f64 / 1000.0),
                present_ms_avg = format!("{:.2}", present_us_total as f64 / shown.max(1) as f64 / 1000.0),
                cpu_cores = {
                    let now = process_cpu_us();
                    let used = now.saturating_sub(cpu_before) as f64 / (secs * 1e6);
                    cpu_before = now;
                    format!("{used:.2}")
                },
                rtt_ms = format!("{rtt:.1}"),
                mbps = format!("{mbps:.1}"),
                target_mbps = shared.bitrate_kbps.load(Ordering::Relaxed) / 1000,
                fec_pct = shared.fec_permille.load(Ordering::Relaxed) / 10,
                frames_lost = lost,
                "hyper"
            );
            // The screen stays on while the picture moves (someone watching
            // without touching anything); a still one may dim like any app's.
            if let Some(a) = awake.as_mut() {
                a.display(shown as f64 / secs >= MOVING_FPS);
            }
            // (Only when it reads differently: each change repaints the caption.)
            let title = format!(
                "RDPiO — Hyper {w}x{h} · {:.0} fps · {:.1} ms · {mbps:.0} Mbps",
                shown as f64 / secs,
                p(0.5)
            );
            if title != last_title {
                window.set_title(&title);
                last_title = title;
            }
            if crate::window::stats_overlay() {
                let decode_kind = match decoder {
                    Decoder::Gpu(_) => "GPU",
                    Decoder::Cpu(_) => "CPU",
                };
                overlay = crate::hyper_overlay::render(&[
                    format!(
                        "Hyper {w}x{h} {} ({decode_kind} decode, {} mode)",
                        codec.name().to_uppercase(),
                        if crate::window::office_mode() { "office" } else { "game" }
                    ),
                    format!("{:>5.1} fps   latency {:.1} / {:.1} ms", shown as f64 / secs, p(0.5), p(0.95)),
                    format!("rtt {rtt:.1} ms   {mbps:.1} / {} Mbps", shared.bitrate_kbps.load(Ordering::Relaxed) / 1000),
                    format!("fec {}%   lost frames {lost}", shared.fec_permille.load(Ordering::Relaxed) / 10),
                    format!(
                        "Ctrl+Shift+S hide · Ctrl+Shift+M mouse · Ctrl+Shift+K keys: {}",
                        if crate::window::system_keys_to_host() { "host" } else { "this PC" }
                    ),
                ]);
                overlay_dirty = true;
            }
            lat_ms.clear();
            (shown, decoded, lost) = (0, 0, 0);
            (dec_us_total, dec_us_max, present_us_total) = (0, 0, 0);
            report = Instant::now();
        }

        // Woken at once by a frame, input or a window message; the timeout
        // only bounds time-based work: input resends, keyframe requests, an
        // overlay's end, the once-a-second report. Hidden or still, that's
        // a few wakes a second, not twenty.
        let timeout = if input.unsent().is_some() {
            // (Motion merged for the next millisecond.)
            1
        } else if input.has_pending() {
            5
        } else if minimized {
            1_000
        } else if awaiting_key || repaint_due.is_some() || hint_on || stats_on {
            50
        } else {
            250
        };
        if pending.is_some() {
            if window.wait_for_work_or(timeout, renderer.frame_ready_handle()) {
                renderer.note_frame_ready();
            }
        } else {
            window.wait_for_work(timeout);
        }
    };
    Ok(ended)
    };
    let result = play();
    // (Back in an RDP session, the window has no video layer.)
    renderer.set_video_layer(false);

    shared.stop.store(true, Ordering::Relaxed);
    // Goodbye only when the window was closed: after a network drop or a
    // local failure (a GPU reset) the caller rejoins, and the host keeps the
    // session unlocked meanwhile.
    if matches!(result, Ok(Ended::Closed)) {
        for _ in 0..3 {
            shared.send(Kind::Control, &Control::Bye.to_bytes());
        }
    }
    let _ = receiver.join();
    pump_stop.store(true, Ordering::Relaxed);
    reliable.nudge();
    let _ = pump.join();
    drop(clipboard);
    result
}

/// A picture read in place (see `H264GpuDecoder::with_codec_in_place`)
/// failed to draw: decoders copy their pictures out instead.
static IN_PLACE_FAILED: AtomicBool = AtomicBool::new(false);

/// No picture and no input this long: the 1 ms timer goes (it keeps the
/// laptop's CPU from its deeper sleep on a still screen).
const FINE_TIMER_IDLE: Duration = Duration::from_secs(2);

/// Pointer motion goes at most this often (see the input send in `run`).
const INPUT_MOTION_EVERY: Duration = Duration::from_millis(1);

/// Shown frames a second at which the picture counts as moving (video, a
/// game) and keeps the screen on; office trickling stays under it.
const MOVING_FPS: f64 = 15.0;

/// No video this long: the receive loop wakes less often and the feedback
/// thins out (nothing to report on a still screen).
const RECEIVE_IDLE: Duration = Duration::from_secs(1);

/// The direct Hyper client: the GPU follows the power source (see
/// [`wants_display_gpu`]) for the whole session.
static GPU_FOLLOWS_POWER: AtomicBool = AtomicBool::new(false);

/// Whether to decode and present on the GPU the display is attached to: on
/// battery (a hybrid laptop's discrete GPU can then power off, and no frame
/// is copied across to the display's GPU); plugged in, the fastest one.
/// `RDPIO_GPU=display` or `performance` decides instead.
pub(crate) fn wants_display_gpu() -> bool {
    let choice = std::env::var("RDPIO_GPU").ok().unwrap_or_else(|| GPU_CHOICE.lock().unwrap().clone());
    match choice.trim() {
        "display" => true,
        "performance" => false,
        _ => on_battery(),
    }
}

/// `--hyper-gpu auto|performance|display` (see [`wants_display_gpu`]).
static GPU_CHOICE: Mutex<String> = Mutex::new(String::new());

/// Which GPU decodes and presents: "auto" (the display's on battery, the
/// fastest plugged in), "performance" or "display".
pub fn set_gpu_choice(choice: &str) {
    *GPU_CHOICE.lock().unwrap() = choice.to_string();
}

fn on_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s = SYSTEM_POWER_STATUS::default();
    // (ACLineStatus: 0 offline, 1 online, 255 unknown — a desktop.)
    unsafe { GetSystemPowerStatus(&mut s).is_ok() && s.ACLineStatus == 0 }
}

/// Office mode with the encoder's fast preset (the default; `--hyper-text
/// best` turns it off).
static FAST_TEXT: AtomicBool = AtomicBool::new(true);

/// Office text: the best-quality preset (crisper new text, ~6 ms more
/// latency) or the fast one.
pub fn set_fast_text(on: bool) {
    FAST_TEXT.store(on, Ordering::Relaxed);
}

/// How long the hotkeys stay on screen when a stream starts.
const HINT_FOR: Duration = Duration::from_secs(8);

/// The hotkeys, as shown when a stream starts.
fn hint_lines(cpu_decode: bool) -> Vec<String> {
    let mut lines = vec![
        "Ctrl+Shift+Q   quit".to_string(),
        "Ctrl+Shift+M   lock the mouse in (games)".to_string(),
        "Ctrl+Shift+S   stats".to_string(),
        "Ctrl+Shift+O   office mode (crisp text) / game".to_string(),
        "Ctrl+Shift+K   Alt+Tab, Win: host / this PC".to_string(),
    ];
    if cpu_decode {
        lines.push("No GPU video decoder: is the GPU driver installed?".to_string());
    }
    lines
}

/// Put a decoded picture into the framebuffer: colour-converted on the GPU
/// where it can, else on the CPU. Windows' software renderer (a PC without a
/// GPU driver) has no video processor, and a picture the GPU path declines
/// must never just be dropped — that was a black screen. `rgba`: the CPU
/// conversion's buffer, kept between frames. Whether the picture got there.
fn paint(renderer: &mut Renderer, picture: Picture, (w, h): (u32, u32), rgba: &mut Vec<u8>) -> bool {
    let (nv12, fw, fh) = match picture {
        Picture::Gpu(tex) => {
            if renderer.blit_texture_slice(0, 0, w, h, &tex.texture, tex.slice, &[]) {
                return true;
            }
            // A picture read in place that won't draw: copies from now on
            // (the caller makes a new decoder).
            if tex.held.is_some() {
                if !IN_PLACE_FAILED.swap(true, Ordering::Relaxed) {
                    tracing::warn!("decoded pictures can't be drawn in place here; copying them instead");
                }
                return false;
            }
            match renderer.read_nv12(&tex.texture, w, h) {
                Some(nv12) => (nv12, w, h),
                None => return false,
            }
        }
        Picture::Cpu(f) => (f.nv12, f.width, f.height),
    };
    if fw <= w && fh <= h && renderer.blit_nv12(0, 0, fw, fh, &nv12, &[]) {
        return true;
    }
    // (Tightly packed NV12: `fw` is the row pitch; anything past the frame is
    // cropped, as is an odd last row.)
    let (luma, chroma) = nv12.split_at(((fw * fh) as usize).min(nv12.len()));
    let (cw, ch) = (fw.min(w) as usize, (fh.min(h) as usize) & !1);
    let ok = rdp_graphics::yuv::nv12_to_rgba_into(luma, chroma, cw, ch, fw as usize, rgba, conversion_threads());
    if ok {
        renderer.update_rect(0, 0, cw as u16, ch as u16, rgba);
    }
    ok
}

/// This process's CPU time so far (kernel + user, µs, all threads).
fn process_cpu_us() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut a, mut b, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10;
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut kernel, &mut user).is_err() {
            return 0;
        }
    }
    ft(kernel) + ft(user)
}

/// Threads for the CPU colour conversion: half the machine's, leaving the
/// rest to the decoder (on the CPU too, there).
fn conversion_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get() / 2).clamp(1, 8)
}

fn receive_loop(
    socket: UdpSocket,
    mut recv: Session,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Event>,
    reliable: Arc<crate::hyper_link::ReliableLink>,
    clipboard: Option<Arc<crate::hyper_clipboard::ClipboardSync>>,
    cursor: mpsc::Sender<crate::session::CursorUpdate>,
) {
    let _ = socket.set_read_timeout(Some(Duration::from_millis(25)));
    crate::net_tune::raise_thread();
    let mut reassembler = Reassembler::new();
    // Several datagrams per call where the kernel coalesces them.
    let mut receiver = crate::udp_batch::Receiver::new(&socket);
    let mut last_feedback = Instant::now();
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut last_rx = Instant::now();
    let mut last_video = Instant::now();
    let mut idle = false;
    let mut ping_id = 0u32;
    let (mut delay_min, mut delay_sum, mut delay_n) = (i64::MAX, 0i64, 0i64);
    let mut game_audio: Option<crate::hyper_audio::AudioSink> = None;
    let mut mic: Option<crate::hyper_audio::AudioSender> = None;
    let mut camera: Option<crate::hyper_camera::CameraSender> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        let datagrams = receiver.recv(&socket);
        for d in datagrams.into_iter().flatten() {
            let n = d.len();
            if let Ok((kind, body)) = recv.open(d) {
                last_rx = Instant::now();
                // Started once video comes (a pong or a cursor shape can come
                // while the host still sets its display up).
                if matches!(kind, Kind::Video) {
                    shared.heard.store(true, Ordering::Relaxed);
                    last_video = last_rx;
                }
                match kind {
                    Kind::Video => {
                        shared.bytes.fetch_add(n as u64, Ordering::Relaxed);
                        let now = now_us();
                        let mut any = false;
                        for ev in reassembler.push(&body, now) {
                            if let Event::Frame(f) = &ev {
                                // Delay from when the host sent the frame (its
                                // capture time plus its own encode — and any hold
                                // for frame-rate matching), so host-side time
                                // doesn't read as network queueing.
                                let d = now as i64 - (f.capture_us + f.encode_us as u64) as i64;
                                delay_min = delay_min.min(d);
                                delay_sum += d;
                                delay_n += 1;
                                shared.queued.fetch_add(1, Ordering::Relaxed);
                            }
                            any = true;
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        if any {
                            crate::window::signal_frame();
                        }
                    }
                    Kind::Control => match Control::parse(&body) {
                        Some(Control::Pong { client_us, host_us, .. }) => {
                            shared.clock.lock().unwrap().sample(client_us, host_us);
                        }
                        Some(Control::Status { input_ack, width, height, fps, bitrate_kbps, fec_permille }) => {
                            shared.input_ack.store(input_ack, Ordering::Relaxed);
                            let size = (width as u32, height as u32);
                            if *shared.size.lock().unwrap() != size {
                                *shared.size.lock().unwrap() = size;
                                crate::window::signal_frame();
                            }
                            shared.host_fps.store(fps as u32, Ordering::Relaxed);
                            shared.bitrate_kbps.store(bitrate_kbps, Ordering::Relaxed);
                            shared.fec_permille.store(fec_permille as u32, Ordering::Relaxed);
                        }
                        Some(Control::Bye) => {
                            shared.host_bye.store(true, Ordering::Relaxed);
                            shared.stop.store(true, Ordering::Relaxed);
                            crate::window::signal_frame();
                        }
                        // The host's console moved to another session (a
                        // sign-in, a user switch): sign in again at once, not
                        // after seconds of silence.
                        Some(Control::Reconnect) if !shared.stop.load(Ordering::Relaxed) => {
                            tracing::info!("the host's console moved to another session; reconnecting now");
                            shared.lost.store(true, Ordering::Relaxed);
                            shared.stop.store(true, Ordering::Relaxed);
                            crate::window::signal_frame();
                        }
                        // A host app is using the camera: send ours, only meanwhile.
                        Some(Control::Camera { on }) => {
                            if camera.as_ref().is_some_and(|c| c.is_finished()) {
                                camera = None;
                            }
                            if on && camera.is_none() {
                                let s = shared.clone();
                                camera = Some(crate::hyper_camera::CameraSender::start(
                                    Arc::new(move |body: &[u8]| s.send(Kind::Camera, body)),
                                    now_us,
                                ));
                            } else if !on {
                                if let Some(c) = camera.take() {
                                    // (Stopping joins the capture: not on this thread.)
                                    std::thread::spawn(move || drop(c));
                                }
                            }
                        }
                        Some(Control::CameraKeyframe) => {
                            if let Some(c) = &camera {
                                c.keyframe();
                            }
                        }
                        Some(Control::Notice { text }) => {
                            tracing::info!(%text, "notice from the host");
                            *shared.notice.lock().unwrap() = Some(text);
                            crate::window::signal_frame();
                        }
                        // A host app is listening to the mic: capture ours (like
                        // RDP's mic redirection, only while it's wanted).
                        Some(Control::Mic { on }) => {
                            // A sender that died (a hard encoder failure) is
                            // replaced on the next "on".
                            if mic.as_ref().is_some_and(|m| m.is_finished()) {
                                mic = None;
                            }
                            if on && mic.is_none() {
                                tracing::info!("host app is listening; sending this microphone");
                                let s = shared.clone();
                                mic = Some(crate::hyper_audio::AudioSender::start(
                                    crate::hyper_audio::Source::Mic(None),
                                    1,
                                    48_000,
                                    now_us,
                                    Arc::new(move |body: &[u8]| s.send(Kind::Audio, body)),
                                ));
                            } else if !on {
                                if let Some(m) = mic.take() {
                                    tracing::info!("host stopped listening; microphone released");
                                    // Stopping joins the capture thread: not here,
                                    // on the thread that receives the stream.
                                    std::thread::spawn(move || drop(m));
                                }
                            }
                        }
                        _ => {}
                    },
                    Kind::Update => {
                        if let (Some(f), Some(m)) = (shared.update.lock().unwrap().as_mut(), rdp_hyper::update::UpdateMsg::parse(&body)) {
                            f.on_msg(m);
                        }
                    }
                    Kind::Reliable => {
                        use rdp_hyper::messages::Message;
                        let (ack, msgs) = reliable.on_packet(&body);
                        if let Some(a) = ack {
                            shared.send(Kind::Reliable, &a);
                        }
                        for m in msgs {
                            match m {
                                Message::CursorShape { width, height, hot_x, hot_y, rgba } => {
                                    let _ = cursor.send(crate::session::CursorUpdate::Shape { width, height, hot_x, hot_y, rgba });
                                    crate::window::signal_frame();
                                }
                                Message::CursorVisible(v) => {
                                    let _ = cursor.send(if v {
                                        crate::session::CursorUpdate::Default
                                    } else {
                                        crate::session::CursorUpdate::Hide
                                    });
                                    crate::window::signal_frame();
                                }
                                other => {
                                    if let Some(c) = &clipboard {
                                        c.apply(other);
                                    }
                                }
                            }
                        }
                    }
                    // Game sound.
                    Kind::Audio => {
                        game_audio.get_or_insert_with(|| crate::hyper_audio::AudioSink::start(None)).push(&body);
                    }
                    _ => {}
                }
            }
        }
        // Until the host's video comes, allow for its setup (a virtual monitor
        // or a display mode, audio, encoder: several seconds); after that, 6 s
        // of silence is a dropped connection.
        let silence = if shared.heard.load(Ordering::Relaxed) { Duration::from_secs(6) } else { Duration::from_secs(15) };
        if last_rx.elapsed() > silence {
            tracing::warn!(secs = silence.as_secs(), "nothing from the Hyper host");
            shared.lost.store(true, Ordering::Relaxed);
            shared.stop.store(true, Ordering::Relaxed);
            crate::window::signal_frame();
        }
        // A still screen sends no video: wake (and report) less often then.
        // (While video flows its packets wake us; the timeout only paces the
        // feedback, every 50 ms.)
        if idle != (last_video.elapsed() > RECEIVE_IDLE) {
            idle = !idle;
            let _ = socket.set_read_timeout(Some(Duration::from_millis(if idle { 125 } else { 25 })));
        }
        if last_feedback.elapsed() >= Duration::from_millis(if idle { 250 } else { 50 }) {
            let s = reassembler.take_stats();
            let fb = Feedback {
                shards: s.shards,
                recovered: s.recovered,
                frames: s.frames,
                frames_lost: s.frames_lost,
                bytes: s.bytes,
                delay_min_us: if delay_n > 0 { delay_min } else { 0 },
                delay_avg_us: if delay_n > 0 { delay_sum / delay_n } else { 0 },
                window_us: last_feedback.elapsed().as_micros() as u32,
                ltr_frame: Some(shared.ltr_decoded.load(Ordering::Relaxed)).filter(|&v| v != u32::MAX),
            };
            shared.send(Kind::Control, &Control::Feedback(fb).to_bytes());
            (delay_min, delay_sum, delay_n) = (i64::MAX, 0, 0);
            last_feedback = Instant::now();
        }
        if last_ping.elapsed() >= Duration::from_millis(if idle { 1000 } else { 500 }) {
            ping_id += 1;
            shared.send(Kind::Control, &Control::Ping { id: ping_id, client_us: now_us() }.to_bytes());
            last_ping = Instant::now();
        }
    }
}

/// Keys and buttons currently held, so focus loss can release them.
#[derive(Default)]
pub(crate) struct Held {
    keys: std::collections::BTreeSet<(u16, bool)>,
    buttons: std::collections::BTreeSet<u8>,
}

impl Held {
    pub(crate) fn note(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { scancode, extended, down: true } => {
                self.keys.insert((scancode, extended));
            }
            InputEvent::Key { scancode, extended, down: false } => {
                self.keys.remove(&(scancode, extended));
            }
            InputEvent::Button { button, down: true } => {
                self.buttons.insert(button);
            }
            InputEvent::Button { button, down: false } => {
                self.buttons.remove(&button);
            }
            _ => {}
        }
    }

    pub(crate) fn is_down(&self, scancode: u16, extended: bool) -> bool {
        self.keys.contains(&(scancode, extended))
    }

    /// Ctrl and Alt (either side) both down.
    pub(crate) fn ctrl_alt(&self) -> bool {
        self.keys.iter().any(|(s, _)| *s == 0x1D) && self.keys.iter().any(|(s, _)| *s == 0x38)
    }

    /// Up events for everything held, and forget it.
    pub(crate) fn release_all(&mut self) -> Vec<InputEvent> {
        let keys = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|(scancode, extended)| InputEvent::Key { scancode, extended, down: false });
        let buttons = std::mem::take(&mut self.buttons).into_iter().map(|button| InputEvent::Button { button, down: false });
        keys.chain(buttons).collect()
    }
}

fn client_size(window: &Window) -> (u32, u32) {
    use windows::Win32::Foundation::{HWND, RECT};
    let mut r = RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(HWND(window.hwnd_raw() as *mut _), &mut r);
    }
    (((r.right - r.left).max(1)) as u32, ((r.bottom - r.top).max(1)) as u32)
}

/// Window input → stream input. Pointer positions are normalised to `view`,
/// the part of the window that shows the stream (`(x, y, w, h)`: all of it,
/// or less when the picture keeps its shape in a window of another); the bars
/// around it clamp to its edges.
fn map_input(raw: RawInput, (vx, vy, vw, vh): (u32, u32, u32, u32)) -> Vec<InputEvent> {
    let (vw, vh) = (vw.max(1), vh.max(1));
    let abs = |x: i32, y: i32| InputEvent::MouseAbs {
        x: ((x - vx as i32).clamp(0, vw as i32 - 1) as u64 * 65535 / (vw.max(2) - 1) as u64) as u16,
        y: ((y - vy as i32).clamp(0, vh as i32 - 1) as u64 * 65535 / (vh.max(2) - 1) as u64) as u16,
    };
    let capture = crate::window::capture_mode();
    match raw {
        RawInput::Key { scancode, extended, down } => vec![InputEvent::Key { scancode, extended, down }],
        RawInput::Char { code, down } => vec![InputEvent::Char { code, down }],
        RawInput::MouseMove { x, y } => vec![abs(x, y)],
        RawInput::MouseRel { dx, dy } => vec![InputEvent::MouseRel {
            dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        }],
        RawInput::MouseButton { button, x, y, down } => {
            let b = InputEvent::Button { button, down };
            if capture { vec![b] } else { vec![abs(x, y), b] }
        }
        RawInput::XButton { button, x, y, down } => {
            let b = InputEvent::Button { button: 3 + button, down };
            if capture { vec![b] } else { vec![abs(x, y), b] }
        }
        RawInput::MouseWheel { delta } => vec![InputEvent::Wheel { delta, horizontal: false }],
        RawInput::MouseHWheel { delta } => vec![InputEvent::Wheel { delta, horizontal: true }],
        RawInput::SyncLockKeys { .. } | RawInput::Touch { .. } | RawInput::FocusLost => Vec::new(),
    }
}

/// The primary display's refresh rate (the default frame rate to ask for).
pub fn local_refresh_hz() -> u32 {
    refresh_hz_of(windows::core::PCWSTR::null())
}

/// The refresh rate of the screen `window` is on now.
fn window_refresh_hz(window: &Window) -> u32 {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST};
    unsafe {
        let monitor = MonitorFromWindow(windows::Win32::Foundation::HWND(window.hwnd_raw() as *mut _), MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            return refresh_hz_of(windows::core::PCWSTR(info.szDevice.as_ptr()));
        }
    }
    local_refresh_hz()
}

/// The current refresh rate of display `device` (null: the primary).
fn refresh_hz_of(device: windows::core::PCWSTR) -> u32 {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut m = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(device, ENUM_CURRENT_SETTINGS, &mut m).as_bool() };
    if ok && m.dmDisplayFrequency > 1 { m.dmDisplayFrequency } else { 60 }
}


// ------------------------------------------------------------ direct mode

fn password_cache_path(host: &str) -> Option<std::path::PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let dir = std::path::PathBuf::from(local).join("rdpio").join("hyper");
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    Some(dir.join(format!("{safe}.bin")))
}

/// This PC's primary screen's display scale, in percent (0 if unknown).
fn primary_scale_percent() -> u32 {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    let (mut x, mut y) = (0u32, 0u32);
    unsafe {
        let m = MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY);
        if GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() || x == 0 {
            return 0;
        }
    }
    (x * 100 + 48) / 96
}

/// Whether this PC remembers a Hyper password for `host` (from a direct
/// sign-in that worked).
pub fn has_saved_password(host: &str) -> bool {
    password_cache_path(host).is_some_and(|p| p.exists())
}

/// Sign in to the host's Hyper agent with the Hyper password (SPAKE2).
pub fn connect_direct(host: SocketAddr, password: &str, want: rdp_hyper::auth::Want) -> Result<Link, String> {
    use rdp_hyper::auth::{password_secret, AuthMsg, ClientAuth};
    let secret = password_secret(password);
    let socket = connect_socket(host).map_err(|e| e.to_string())?;
    let mut want = want;
    let mut attempts = 0;
    let mut cookie_rounds = 0;
    // (The codecs go in `client_codecs`' order: HEVC first where the GPU
    // decodes it. H.264 used to go first on a LAN, for ~2 ms less latency
    // through Media Foundation; driving NVENC directly, HEVC encodes faster
    // (1.2 vs 1.35 ms a 1080p frame) and looks better for the bits (+0.3 to
    // +2.3 dB luma at 6-25 Mbps), so it's first everywhere.)
    'attempt: while attempts < 3 {
        attempts += 1;
        let (auth, hello) = ClientAuth::start(&secret, want.clone());
        socket.send(&hello).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            let Ok(n) = socket.recv(&mut buf) else { continue };
            if let Some(v) = rdp_hyper::auth::foreign_version(&buf[..n]) {
                return Err(format!(
                    "the host runs an rdpio from before automatic updates (Hyper protocol {v}); install this rdpio there once and it keeps itself up to date"
                ));
            }
            match AuthMsg::parse(&buf[..n]) {
                // The host wants proof we receive at our address: say hello
                // again with its cookie (doesn't count as an attempt).
                Some(AuthMsg::Cookie { cookie }) if cookie_rounds < 3 => {
                    cookie_rounds += 1;
                    want.cookie = Some(rdp_hyper::auth::cookie_to_hex(&cookie));
                    attempts -= 1;
                    continue 'attempt;
                }
                Some(AuthMsg::Denied { retry_after_s }) => {
                    return Err(format!("the host is refusing sign-ins from this address after failed attempts; try again in {retry_after_s} s"));
                }
                // A late challenge for an earlier attempt: keep waiting for ours.
                Some(ref msg @ AuthMsg::Challenge { .. }) if !auth.answers(msg) => {}
                Some(AuthMsg::Challenge { offer, .. }) => {
                    let key = auth.finish(&buf[..n]).ok_or("the host's sign-in reply was malformed")?;
                    let from_host = offer.update && crate::updater::updates_from_hosts();
                    if offer.protocol == 0 {
                        // No protocol in common: take the host's rdpio, or say why not.
                        if !offer.update {
                            return Err(format!(
                                "the host runs rdpio {}, which can't stream to this rdpio {}: update the host (it updates itself from GitHub when it can)",
                                offer.app,
                                crate::updater::version()
                            ));
                        }
                        if !from_host {
                            return Err(format!(
                                "the host runs rdpio {} and this PC {}: turn on \"Update from the PCs I connect to\" in Settings, or install it by hand",
                                offer.app,
                                crate::updater::version()
                            ));
                        }
                        return Ok(Link {
                            socket,
                            send: Session::new(&key, Direction::ClientToHost),
                            recv: Session::new(&key, Direction::ClientToHost),
                            size: (0, 0),
                            fps: 0,
                            codec: rdp_gpu::h264::Codec::H264,
                            update: true,
                            update_only: true,
                            host_app: offer.app,
                        });
                    }
                    let (min, max) = crate::updater::protocols();
                    if !(min..=max).contains(&offer.protocol) {
                        return Err(format!("the host chose Hyper protocol {}, which this rdpio doesn't speak; update the host", offer.protocol));
                    }
                    let mut send = Session::with_version(&key, Direction::ClientToHost, offer.protocol);
                    let mut recv = Session::with_version(&key, Direction::ClientToHost, offer.protocol);
                    // The host confirms at once (three copies), then sets up
                    // its displays and audio before it answers again: allow a
                    // little more than a lost confirmation would take.
                    return match ping_until_pong(&socket, &mut send, &mut recv, Duration::from_secs(5)) {
                        Ok(()) => Ok(Link {
                            socket,
                            send,
                            recv,
                            size: (offer.width, offer.height),
                            fps: offer.fps,
                            codec: rdp_gpu::h264::Codec::from_name(&offer.codec).unwrap_or(rdp_gpu::h264::Codec::H264),
                            update: from_host,
                            update_only: false,
                            host_app: offer.app,
                        }),
                        Err(_) => Err("wrong Hyper password".into()),
                    };
                }
                _ => {}
            }
        }
    }
    Err(format!(
        "no answer from a Hyper host at {host} (is the rdpio Hyper service installed and running there, UDP {} open, and the same rdpio version on both?)",
        host.port()
    ))
}

/// `rdpio --host H --hyper --direct`: no RDP — sign in with the Hyper password
/// and play the stream.
pub fn run_direct(
    host: &str,
    port: u16,
    password: Option<String>,
    fps: Option<u32>,
    fullscreen: bool,
    size: Option<(u32, u32)>,
    low_latency: bool,
    upscale: rdp_gpu::Upscaler,
    backend: rdp_gpu::Backend,
    codec: Option<rdp_gpu::h264::Codec>,
    office: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::window::set_office_mode(office);
    // Hyper decodes on the GPU through Direct3D 11 (the D3D12 renderer has no
    // video decoding: every frame would fall to the CPU).
    if backend != rdp_gpu::Backend::D3D11 {
        tracing::info!("Hyper renders with Direct3D 11 (the D3D12 renderer can't decode video on the GPU)");
    }
    let backend = rdp_gpu::Backend::D3D11;
    let addr = resolve(host, port)?;
    let cache = password_cache_path(host);
    let cached = cache.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| crate::token_cache::dpapi_unprotect(&b).ok()).and_then(|b| String::from_utf8(b).ok());
    let from_cache = password.is_none() && cached.is_some();
    let password = match password.or(cached) {
        Some(p) => p,
        None => crate::prompt::read_password(&format!("Hyper password for {host}: "))?,
    };
    let monitor = crate::window::enumerate_monitors().primary_rect();
    let (mw, mh) = monitor.map_or((1920, 1080), |p| ((p.right - p.left) as u32, (p.bottom - p.top) as u32));
    // On battery, the GPU the display is on (before asking what it decodes).
    GPU_FOLLOWS_POWER.store(true, Ordering::Relaxed);
    rdp_gpu::gpu::prefer_display_adapter(wants_display_gpu());
    let codecs = rdp_gpu::hyper::client_codecs(codec);
    if !codecs.iter().any(|c| rdp_gpu::h264::Codec::from_name(c).is_some_and(rdp_gpu::hyper::gpu_can_decode)) {
        tracing::warn!(?codecs, "no GPU video decoder (is the GPU driver installed?): decoding on the CPU");
    }
    let mut want = rdp_hyper::auth::Want {
        codecs,
        width: mw,
        height: mh,
        refresh_hz: fps.unwrap_or_else(local_refresh_hz),
        // One monitor is streamed: see `HyperRedirector::new`.
        monitors: 1,
        // At this screen's size: the host scales its desktop down to it,
        // never up (a bigger screen here upscales).
        fit: true,
        office,
        fast_text: FAST_TEXT.load(Ordering::Relaxed),
        cookie: None,
        scale: primary_scale_percent(),
        app: crate::updater::version().into(),
        build: crate::updater::build().into(),
        protocols: crate::updater::protocols(),
    };
    tracing::info!(%addr, fps = want.refresh_hz, "Hyper direct: signing in");
    // An update fetched from the host goes in when this ends.
    let _install = InstallOnExit;
    let link = match connect_direct(addr, &password, want.clone()) {
        Ok(l) => l,
        Err(e) => {
            if e.contains("wrong Hyper password") {
                if let Some(p) = &cache {
                    let _ = std::fs::remove_file(p);
                }
                if from_cache {
                    return Err("the saved Hyper password was rejected (changed on the host?); run again to enter it".into());
                }
            }
            return Err(e.into());
        }
    };
    if let (Some(p), false) = (&cache, from_cache) {
        if let Ok(blob) = crate::token_cache::dpapi_protect(password.as_bytes()) {
            let _ = std::fs::write(p, blob);
        }
    }
    crate::window::init_wake_event();
    crate::window::install_keyboard_hook();
    // (Also while reconnecting, when no stream holds the keys.)
    let _keys = crate::window::SystemKeysFollowCapture::on();
    let (window, sw, sh) = match (fullscreen, monitor) {
        (true, Some(p)) => {
            tracing::info!("borderless fullscreen — press Ctrl+Shift+Q to close; Ctrl+Shift+M captures the mouse");
            (crate::window::Window::new_spanning("RDPiO", p.left, p.top, mw, mh)?, mw, mh)
        }
        _ => {
            let (w, h) = size.unwrap_or((mw * 3 / 4, mh * 3 / 4));
            (crate::window::Window::new("RDPiO", w, h)?, w, h)
        }
    };
    let new_renderer = || -> Result<Renderer, Box<dyn std::error::Error>> {
        let mut r = Renderer::new(window.hwnd_raw(), sw, sh, backend)?;
        r.set_low_latency(low_latency);
        r.set_upscaler(upscale);
        Ok(r)
    };
    let mut renderer = new_renderer()?;
    let mut link = link;
    let mut failures: Vec<Instant> = Vec::new();
    loop {
        if link.update_only {
            return update_from_host(&window, &mut renderer, link, host);
        }
        let size = link.size;
        match run(&window, &mut renderer, link) {
            Ok(Ended::Lost) => {}
            // Signed in, but the host never streamed: it may work next time
            // (a display that was still switching modes) — but not forever.
            Ok(Ended::NeverStarted) => {
                failures.retain(|t| t.elapsed() < Duration::from_secs(60));
                if failures.len() >= 3 {
                    return Err("the Hyper host accepts the sign-in but doesn't start the stream (see its log)".into());
                }
                failures.push(Instant::now());
                tracing::warn!("the host signed us in but sent nothing; rejoining");
            }
            Ok(_) => return Ok(()),
            // Playback failed here (a GPU reset, a driver update): a fresh
            // renderer and a fresh sign-in — unless it keeps happening.
            Err(e) => {
                failures.retain(|t| t.elapsed() < Duration::from_secs(60));
                if failures.len() >= 3 {
                    return Err(e);
                }
                failures.push(Instant::now());
                tracing::warn!(error = %e, "Hyper playback failed; recreating the renderer and rejoining");
                drop(renderer);
                renderer = new_renderer()?;
                renderer.set_keep_aspect(true);
                let _ = renderer.ensure_framebuffer(size.0, size.1);
                // The GPU may have come or gone (a driver installed or lost):
                // ask again what it decodes.
                rdp_gpu::hyper::forget_gpu_decoders();
                want.codecs = rdp_gpu::hyper::client_codecs(codec);
            }
        }
        // The network dropped: sign in again (the host keeps the session
        // unlocked for a short grace period, so this resumes where we were).
        window.set_title("RDPiO — Hyper: connection lost, reconnecting…");
        // On screen too: a fullscreen window has no title bar to read it in.
        if let Some((nw, nh, px)) =
            crate::hyper_overlay::render(&["Connection lost: reconnecting...".to_string(), "Ctrl+Shift+Q   quit".to_string()])
        {
            renderer.update_rect(16, 16, nw, nh, &px);
            let _ = renderer.present_frame();
        }
        let mut relinked = None;
        for attempt in 1..=15 {
            // (The first try at once: the host may only have moved sessions.)
            let until = Instant::now() + if attempt == 1 { Duration::from_millis(300) } else { Duration::from_secs(2) };
            while Instant::now() < until {
                if matches!(window.pump(), Frame::Quit) {
                    return Ok(());
                }
                let _ = window.drain_input(); // nothing to send it to
                window.wait_for_work(100);
            }
            tracing::info!(attempt, "Hyper: reconnecting");
            // Sign in on a worker so the window stays responsive (and
            // closable) for the seconds an attempt can take.
            let (done_tx, done_rx) = mpsc::channel();
            let (pw, w) = (password.clone(), want.clone());
            std::thread::spawn(move || {
                let _ = done_tx.send(connect_direct(addr, &pw, w));
            });
            let result = loop {
                if matches!(window.pump(), Frame::Quit) {
                    return Ok(());
                }
                let _ = window.drain_input();
                match done_rx.try_recv() {
                    Ok(r) => break r,
                    Err(mpsc::TryRecvError::Empty) => window.wait_for_work(50),
                    Err(mpsc::TryRecvError::Disconnected) => break Err("the sign-in worker died".to_string()),
                }
            };
            match result {
                Ok(l) => {
                    relinked = Some(l);
                    break;
                }
                // (No confirmation now isn't a wrong password: it worked minutes
                // ago — keep trying.)
                Err(e) if e.contains("refusing") => return Err(e.into()),
                Err(e) => tracing::info!(error = %e, "Hyper: not back yet"),
            }
        }
        link = relinked.ok_or("lost the connection to the Hyper host")?;
        tracing::info!("Hyper: reconnected");
    }
}

/// Installs what [`fetch_beside_stream`] downloaded once the session ends
/// (an exe can't replace itself while it runs).
struct InstallOnExit;

impl Drop for InstallOnExit {
    fn drop(&mut self) {
        if DOWNLOADED.load(Ordering::Relaxed) {
            if let Some((exe, m)) = crate::updater::staged() {
                tracing::info!(version = %m.version, "installing the rdpio the host gave us");
                if let Err(e) = crate::updater::install(&exe, None) {
                    tracing::warn!(error = %e, "couldn't start the update's install");
                }
            }
        }
    }
}

/// Download the host's newer rdpio beside the stream, gently (about 1 MB/s:
/// the stream comes first), and keep it for when the session ends.
fn fetch_beside_stream(s: Arc<Shared>) {
    use rdp_hyper::update::{FetchState, UpdateMsg};
    // Let the stream settle first.
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle {
        if s.stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    loop {
        if s.stop.load(Ordering::Relaxed) {
            return;
        }
        let msgs = {
            let mut g = s.update.lock().unwrap();
            let Some(f) = g.as_mut() else { return };
            if f.finished() {
                break;
            }
            f.poll(Instant::now(), 16)
        };
        for m in msgs {
            s.send(Kind::Update, &m.to_bytes());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let package = {
        let mut g = s.update.lock().unwrap();
        let f = g.as_mut().expect("fetch");
        if let FetchState::Failed(why) = f.state() {
            tracing::info!(%why, "no update from the host");
        }
        f.take()
    };
    for _ in 0..3 {
        s.send(Kind::Update, &UpdateMsg::Done.to_bytes());
    }
    let Some(p) = package else { return };
    match crate::updater::stage(&p) {
        Ok(_) => {
            DOWNLOADED.store(true, Ordering::Relaxed);
            *s.update_note.lock().unwrap() = Some(p.manifest.version.clone());
            crate::window::signal_frame();
        }
        Err(e) => tracing::warn!(error = %e, "couldn't keep the host's rdpio"),
    }
}

/// No protocol in common with the host: take its rdpio (progress on screen),
/// install it, and start this same connection again on it.
fn update_from_host(window: &Window, renderer: &mut Renderer, link: Link, host: &str) -> Result<(), Box<dyn std::error::Error>> {
    use rdp_hyper::update::{Fetch, FetchState, UpdateMsg};
    tracing::info!(host_app = %link.host_app, ours = crate::updater::version(), "updating rdpio from the host before streaming");
    let Link { socket, mut send, mut recv, host_app, .. } = link;
    socket.set_read_timeout(Some(Duration::from_millis(5)))?;
    let mut fetch = Fetch::new(crate::updater::version(), crate::updater::build());
    let mut show = |lines: &[String]| {
        if let Some((w, h, px)) = crate::hyper_overlay::render(lines) {
            renderer.update_rect(16, 16, w, h, &px);
            let _ = renderer.present_frame();
        }
    };
    window.set_title(&format!("RDPiO — updating to rdpio {host_app} from {host}"));
    let mut buf = [0u8; 2048];
    let mut last_rx = Instant::now();
    let mut drawn = Instant::now() - Duration::from_secs(1);
    while !fetch.finished() {
        if matches!(window.pump(), Frame::Quit) {
            let _ = socket.send(&send.seal(Kind::Update, &UpdateMsg::Done.to_bytes()));
            return Ok(());
        }
        let _ = window.drain_input();
        for m in fetch.poll(Instant::now(), 48) {
            let _ = socket.send(&send.seal(Kind::Update, &m.to_bytes()));
        }
        let until = Instant::now() + Duration::from_millis(10);
        while Instant::now() < until {
            let Ok(n) = socket.recv(&mut buf) else { break };
            if let Ok((Kind::Update, body)) = recv.open(&buf[..n]) {
                last_rx = Instant::now();
                if let Some(m) = UpdateMsg::parse(&body) {
                    fetch.on_msg(m);
                }
            }
        }
        if last_rx.elapsed() > Duration::from_secs(15) {
            return Err("the host stopped sending its rdpio".into());
        }
        if drawn.elapsed() > Duration::from_millis(200) {
            drawn = Instant::now();
            show(&[
                format!("{host} runs a newer rdpio ({host_app}): updating this PC  {:.0}%", fetch.progress() * 100.0),
                "Ctrl+Shift+Q   cancel".to_string(),
            ]);
        }
    }
    for _ in 0..3 {
        let _ = socket.send(&send.seal(Kind::Update, &UpdateMsg::Done.to_bytes()));
    }
    if let FetchState::Failed(why) = fetch.state() {
        return Err(format!("couldn't take the host's rdpio: {why}").into());
    }
    let package = fetch.take().ok_or("the update went missing")?;
    let staged = crate::updater::stage(&package)?;
    show(&[format!("rdpio {} installed: reconnecting...", package.manifest.version)]);
    // The same connection again, on the new rdpio.
    crate::updater::install(&staged, Some(std::env::args_os().skip(1).collect()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{client_size, paint, Decoder, Held, Renderer};
    use rdp_gpu::h264::Codec;
    use rdp_hyper::input::InputEvent;
    use std::time::Duration;

    /// Four bars, left to right: red, green, blue, white — as NV12 (BT.709
    /// limited range, what the encoder takes) and as the RGB to expect.
    const BARS_YUV: [(u8, u8, u8); 4] = [(63, 102, 240), (173, 42, 26), (32, 240, 118), (235, 128, 128)];
    const BARS_RGB: [(u8, u8, u8); 4] = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)];

    /// A client on a PC without a GPU driver (Windows' software renderer, no
    /// video decoder or video processor): what its CPU decodes reaches the
    /// framebuffer and the window. It used to be dropped — the stream played
    /// and the screen stayed black. The stream is made on this PC's hardware
    /// encoder: `cargo test -p rdp-client --bin rdpio software_rendered -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_software_rendered_client_shows_what_its_cpu_decodes() {
        crate::window::set_process_dpi_aware();
        for codec in [Codec::H264, Codec::Hevc] {
            if !rdp_gpu::hyper::can_decode(codec, None) {
                eprintln!("{}: no CPU decoder on this PC; skipped", codec.name());
                continue;
            }
            unsafe { show_bars(codec, false) };
        }
    }

    /// A GPU client: the decoder's own surfaces drawn in place (no copy),
    /// colours right in the framebuffer and the window. `cargo test -p rdp-client --bin rdpio drawn_in_place -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_gpu_client_shows_pictures_drawn_in_place() {
        crate::window::set_process_dpi_aware();
        for codec in [Codec::H264, Codec::Hevc] {
            unsafe { show_bars(codec, true) };
        }
        assert!(!super::IN_PLACE_FAILED.load(std::sync::atomic::Ordering::Relaxed), "a picture read in place failed to draw");
    }

    /// A GPU client showing the stream 1:1 with nothing over it: each picture
    /// goes straight into the backbuffer in one pass (`present_nv12`), and the
    /// window shows the newest one's colours. `cargo test -p rdp-client --bin rdpio straight_to_the_screen -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pictures_go_straight_to_the_screen() {
        crate::window::set_process_dpi_aware();
        for codec in [Codec::H264, Codec::Hevc] {
            unsafe {
                let (w, h) = (1280u32, 720u32);
                let units = bars_stream(codec, w, h);
                let window = crate::window::Window::new("rdpio one-pass test", 640, 360).unwrap();
                let (cw, ch) = client_size(&window);
                // A swapchain the stream's size (the window shows it scaled by
                // the compositor), so the picture is 1:1 in the backbuffer.
                let mut renderer = Renderer::new(window.hwnd_raw(), w, h, rdp_gpu::Backend::D3D11).unwrap();
                renderer.set_keep_aspect(true);
                renderer.ensure_framebuffer(w, h).unwrap();
                let mut decoder = Decoder::new(&renderer, w, h, codec).unwrap();
                let (mut direct, mut last) = (0, None);
                for u in &units {
                    if let Some(super::Picture::Gpu(t)) = decoder.decode(&u.data).unwrap() {
                        let at = std::time::Instant::now();
                        assert!(renderer.present_nv12(&t.texture, t.slice, (w, h)).unwrap(), "{}: not presented in one pass", codec.name());
                        eprintln!("{}: one-pass present in {:.2} ms", codec.name(), at.elapsed().as_secs_f64() * 1e3);
                        direct += 1;
                        last = Some(t);
                    }
                    let _ = window.pump();
                }
                assert!(direct > 0, "{}: nothing decoded", codec.name());
                std::thread::sleep(Duration::from_millis(100));
                let _ = window.pump();
                check_bars(codec, "one-pass window", &window_pixels(&window, cw, ch), cw, &[2, ch / 2, ch - 4]);
                // A scaled picture isn't drawn in one pass (the upscaler's
                // kernels are the usual path's).
                renderer.resize(w / 2, h / 2).unwrap();
                let t = last.unwrap();
                assert!(!renderer.present_nv12(&t.texture, t.slice, (w, h)).unwrap(), "{}: a scaled picture went the one-pass way", codec.name());
            }
        }
    }

    /// The video layer (NV12 for a hardware overlay plane): where this PC's
    /// display has one, pictures go there and the window shows them; where it
    /// hasn't (or Windows composes the layer), they're drawn in one pass
    /// instead — either way the window shows the newest picture. Prints what
    /// Windows did with the layer. `cargo test -p rdp-client --bin rdpio video_layer -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn the_video_layer_shows_pictures_or_steps_aside() {
        let _ = tracing_subscriber::fmt().with_env_filter("info").with_writer(std::io::stderr).try_init();
        crate::window::set_process_dpi_aware();
        let (w, h) = (1280u32, 720u32);
        let units = unsafe { bars_stream(Codec::H264, w, h) };
        let window = crate::window::Window::new("rdpio video layer test", 640, 360).unwrap();
        let (cw, ch) = client_size(&window);
        let mut renderer = Renderer::new(window.hwnd_raw(), w, h, rdp_gpu::Backend::D3D11).unwrap();
        renderer.set_keep_aspect(true);
        renderer.set_video_layer(true);
        renderer.ensure_framebuffer(w, h).unwrap();
        let mut decoder = Decoder::new(&renderer, w, h, Codec::H264).unwrap();
        let mut last = None;
        for u in &units {
            if let Some(super::Picture::Gpu(t)) = decoder.decode(&u.data).unwrap() {
                assert!(renderer.present_nv12(&t.texture, t.slice, (w, h)).unwrap());
                last = Some(t);
            }
            let _ = window.pump();
        }
        // Keep presenting the newest picture a while: Windows says where it put
        // the layer's frames only every so often.
        let t = last.expect("decoded");
        for _ in 0..240 {
            assert!(renderer.present_nv12(&t.texture, t.slice, (w, h)).unwrap());
            let _ = window.pump();
        }
        eprintln!("video layer (shown, presents, on overlay, composed): {:?}", renderer.video_layer_status());
        std::thread::sleep(Duration::from_millis(100));
        let _ = window.pump();
        check_bars(Codec::H264, "video layer window", &window_pixels(&window, cw, ch), cw, &[2, ch / 2, ch - 4]);
        // An overlay to draw (the stats): the layer steps aside and the window's
        // own swapchain shows the framebuffer.
        drop(t);
        if renderer.video_layer_status().is_some_and(|s| s.0) {
            renderer.present_clear([1.0, 0.0, 0.0, 1.0]).unwrap();
            assert!(renderer.video_layer_status().is_some_and(|s| !s.0), "the layer stayed in sight over a clear");
        }
    }

    /// The test stream: three pictures of four bars, then three of them
    /// mirrored — encoded on this PC's GPU.
    unsafe fn bars_stream(codec: Codec, w: u32, h: u32) -> Vec<rdp_gpu::hyper::Encoded> {
        use windows::core::Interface;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
        // The stream, encoded on this PC's GPU.
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .unwrap();
        let device: ID3D11Device = device.unwrap();
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let bar = |x: u32| BARS_YUV[(x * 4 / w) as usize];
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                nv12[(y * w + x) as usize] = bar(x).0;
            }
        }
        for y in 0..h / 2 {
            for x in (0..w).step_by(2) {
                let i = (w * h + y * w + x) as usize;
                (nv12[i], nv12[i + 1]) = (bar(x).1, bar(x).2);
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: nv12.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
        let mut tex = None;
        device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).unwrap();
        let tex = tex.unwrap();
        // A second picture, the bars the other way round: what's shown must
        // be the last one, not the first.
        let mut flipped = nv12.clone();
        for y in 0..h {
            for x in 0..w {
                flipped[(y * w + x) as usize] = bar(w - 1 - x).0;
            }
        }
        for y in 0..h / 2 {
            for x in (0..w).step_by(2) {
                let i = (w * h + y * w + x) as usize;
                (flipped[i], flipped[i + 1]) = (bar(w - 2 - x).1, bar(w - 2 - x).2);
            }
        }
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: flipped.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
        let mut tex2 = None;
        device.CreateTexture2D(&desc, Some(&init), Some(&mut tex2)).unwrap();
        let tex2 = tex2.unwrap();
        let mut enc = rdp_gpu::hyper::HwEncoder::new(&device, None, w, h, 60, 8_000_000, false, codec).unwrap();
        let mut units = Vec::new();
        for i in 0..6 {
            units.extend(enc.encode(if i < 3 { &tex } else { &tex2 }).unwrap());
            while let Some(u) = enc.take_output() {
                units.push(u);
            }
        }
        assert!(units.first().is_some_and(|u| u.keyframe), "{}: the stream starts with a keyframe", codec.name());
        units
    }

    /// `gpu`: the GPU renderer and decoder (pictures read in place), else
    /// the software renderer and the CPU decoder.
    unsafe fn show_bars(codec: Codec, gpu: bool) {
        let (w, h) = (1280u32, 720u32);
        let units = bars_stream(codec, w, h);

        // The client: the software renderer, in a window smaller than the
        // stream (so the picture is scaled on the way to the screen, too).
        let window = crate::window::Window::new("rdpio software-render test", 640, 360).unwrap();
        let (cw, ch) = client_size(&window);
        let new_renderer = |cw, ch| if gpu { Renderer::new(window.hwnd_raw(), cw, ch, rdp_gpu::Backend::D3D11) } else { Renderer::new_software(window.hwnd_raw(), cw, ch) };
        let mut renderer = new_renderer(cw, ch).unwrap();
        renderer.ensure_framebuffer(w, h).unwrap();
        let mut decoder = Decoder::new(&renderer, w, h, codec).unwrap();
        assert_eq!(matches!(decoder, Decoder::Gpu(_)), gpu, "the software renderer has no video decoder");
        let mut rgba = Vec::new();
        let mut shown = 0;
        for u in &units {
            if let Some(p) = decoder.decode(&u.data).unwrap() {
                let t = std::time::Instant::now();
                if gpu {
                    assert!(matches!(&p, super::Picture::Gpu(t) if t.held.is_some()), "{}: not read in place", codec.name());
                }
                assert!(paint(&mut renderer, p, (w, h), &mut rgba), "{}: a picture wasn't drawn", codec.name());
                assert!(gpu || !renderer.gpu_yuv_available(), "the CPU conversion is what's tested");
                renderer.present_frame().unwrap();
                eprintln!("{}: painted + presented in {:.2} ms", codec.name(), t.elapsed().as_secs_f64() * 1e3);
                shown += 1;
            }
            let _ = window.pump();
        }
        assert!(shown > 0, "{}: nothing decoded", codec.name());
        let (fw, fh, fb) = renderer.readback_framebuffer().unwrap();
        assert_eq!((fw, fh), (w, h));
        check_bars(codec, "framebuffer", &fb, w, &[1, h / 2, h - 3]);
        // What the window shows (as the compositor has it).
        for _ in 0..3 {
            renderer.present_frame().unwrap();
            let _ = window.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        check_bars(codec, "window", &window_pixels(&window, cw, ch), cw, &[2, ch / 2, ch - 4]);

        // A window of another shape (4:3): the 16:9 picture keeps its shape,
        // black above and below.
        let window = crate::window::Window::new("rdpio software-render test (4:3)", 640, 480).unwrap();
        let (cw, ch) = client_size(&window);
        let mut renderer = if gpu { Renderer::new(window.hwnd_raw(), cw, ch, rdp_gpu::Backend::D3D11) } else { Renderer::new_software(window.hwnd_raw(), cw, ch) }.unwrap();
        renderer.set_keep_aspect(true);
        renderer.ensure_framebuffer(w, h).unwrap();
        let mut decoder = Decoder::new(&renderer, w, h, codec).unwrap();
        for u in &units {
            if let Some(p) = decoder.decode(&u.data).unwrap() {
                paint(&mut renderer, p, (w, h), &mut rgba);
            }
        }
        for _ in 0..3 {
            renderer.present_frame().unwrap();
            let _ = window.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let shot = window_pixels(&window, cw, ch);
        let (_, by, _, bh) = rdp_gpu::fit_rect((w, h), (cw, ch));
        assert!(by > 0, "{cw}x{ch} is taller than 16:9");
        check_bars(codec, "letterboxed window", &shot, cw, &[by + 2, by + bh / 2, by + bh - 4]);
        for y in [by / 2, by + bh + (ch - by - bh) / 2] {
            let p = &shot[((y * cw + cw / 2) * 4) as usize..][..3];
            assert!(p.iter().all(|&c| c < 16), "{} letterboxed window: bar at row {y} shows {p:?}", codec.name());
        }
    }

    /// What a client without a GPU driver pays per frame at 1920x1080 (the
    /// size it asks for on a 1080p laptop), with moving content. Prints
    /// timings; run it in release:
    /// `cargo test --release -p rdp-client --bin rdpio software_rendered_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn software_rendered_speed() {
        crate::window::set_process_dpi_aware();
        for codec in [Codec::H264, Codec::Hevc] {
            if rdp_gpu::hyper::can_decode(codec, None) {
                // What it asks for now (its screen's size), and what it got
                // before (the host's 1440p desktop, scaled down here).
                unsafe { speed(codec, (1920, 1080), (1920, 1080)) };
                unsafe { speed(codec, (2560, 1440), (1920, 1080)) };
            }
        }
    }

    unsafe fn speed(codec: Codec, (w, h): (u32, u32), (sw, sh): (u32, u32)) {
        use windows::core::Interface;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .unwrap();
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
        let tex = tex.unwrap();
        let mut enc = rdp_gpu::hyper::HwEncoder::new(&device, None, w, h, 60, 15_000_000, false, codec).unwrap();
        // Bars sliding 8 px a frame, with a gradient: every frame differs.
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        let mut units = Vec::new();
        for i in 0..120u32 {
            for y in 0..h {
                for x in 0..w {
                    let b = BARS_YUV[(((x + i * 8) % w) * 4 / w) as usize];
                    nv12[(y * w + x) as usize] = b.0.saturating_add((y * 32 / h) as u8);
                }
            }
            for y in 0..h / 2 {
                for x in (0..w).step_by(2) {
                    let b = BARS_YUV[(((x + i * 8) % w) * 4 / w) as usize];
                    let k = (w * h + y * w + x) as usize;
                    (nv12[k], nv12[k + 1]) = (b.1, b.2);
                }
            }
            context.UpdateSubresource(&tex, 0, None, nv12.as_ptr() as *const _, w, 0);
            units.extend(enc.encode(&tex).unwrap());
            while let Some(u) = enc.take_output() {
                units.push(u);
            }
        }
        // Borderless at the screen's size, presenting at once: as the client
        // runs fullscreen.
        let window = crate::window::Window::new_spanning("rdpio software-render speed", 0, 0, sw, sh).unwrap();
        let (cw, ch) = client_size(&window);
        let mut renderer = Renderer::new_software(window.hwnd_raw(), cw, ch).unwrap();
        renderer.set_low_latency(true);
        renderer.ensure_framebuffer(w, h).unwrap();
        let mut decoder = Decoder::new(&renderer, w, h, codec).unwrap();
        let mut rgba = Vec::new();
        let (mut dec, mut pnt, mut pre) = (Vec::new(), Vec::new(), Vec::new());
        for u in &units {
            let t0 = std::time::Instant::now();
            let picture = decoder.decode(&u.data).unwrap();
            let t1 = std::time::Instant::now();
            if let Some(p) = picture {
                paint(&mut renderer, p, (w, h), &mut rgba);
                let t2 = std::time::Instant::now();
                renderer.present_frame().unwrap();
                let t3 = std::time::Instant::now();
                dec.push((t1 - t0).as_secs_f64() * 1e3);
                pnt.push((t2 - t1).as_secs_f64() * 1e3);
                pre.push((t3 - t2).as_secs_f64() * 1e3);
            }
            let _ = window.pump();
        }
        let p = |v: &mut Vec<f64>, q: f64| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.get(((v.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0)
        };
        let n = dec.len();
        eprintln!(
            "{} {w}x{h} into a {cw}x{ch} window, software rendering, {n} frames: decode p50 {:.2} p95 {:.2} | convert+upload p50 {:.2} p95 {:.2} | present p50 {:.2} p95 {:.2} ms ({} conversion threads)",
            codec.name(),
            p(&mut dec, 0.5),
            p(&mut dec, 0.95),
            p(&mut pnt, 0.5),
            p(&mut pnt, 0.95),
            p(&mut pre, 0.5),
            p(&mut pre, 0.95),
            super::conversion_threads()
        );
    }

    /// A picture kept at its shape in a window of another: positions map to
    /// the picture, the bars clamp to its edges.
    #[test]
    fn the_pointer_maps_to_a_letterboxed_picture() {
        use super::RawInput;
        let view = rdp_gpu::fit_rect((2256, 1268), (2256, 1504));
        assert_eq!(view, (0, 118, 2256, 1268));
        let at = |x, y| match super::map_input(RawInput::MouseMove { x, y }, view)[..] {
            [InputEvent::MouseAbs { x, y }] => (x, y),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(at(0, 118), (0, 0));
        assert_eq!(at(2255, 118 + 1267), (65535, 65535));
        assert_eq!(at(1128, 0), at(1128, 118), "the top bar is the top edge");
        assert_eq!(at(1128, 1503).1, 65535, "the bottom bar is the bottom edge");
        // The whole window, as before, when nothing is kept.
        assert_eq!(super::map_input(RawInput::MouseMove { x: 99, y: 0 }, (0, 0, 100, 100)), vec![InputEvent::MouseAbs { x: 65535, y: 0 }]);
    }

    /// The Direct3D 12 renderer: uploads in one frame each land intact — rows
    /// of any width (staged at the 256-byte pitch copies need), one upload
    /// never overwriting another's staging before the GPU copied it, and an
    /// NV12 frame bigger than anything staged before.
    /// `cargo test -p rdp-client --bin rdpio d3d12_uploads -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn d3d12_uploads_land_intact() {
        crate::window::set_process_dpi_aware();
        let window = crate::window::Window::new("rdpio d3d12 upload test", 640, 480).unwrap();
        let (cw, ch) = client_size(&window);
        let mut renderer = Renderer::new(window.hwnd_raw(), cw, ch, rdp_gpu::Backend::D3D12).unwrap();
        // The framebuffer is the window's size: what it holds is what shows.
        renderer.ensure_framebuffer(cw, ch).unwrap();
        let solid = |rw: u32, rh: u32, px: [u8; 4]| px.repeat((rw * rh) as usize);
        // A small one first (the staging buffer starts small), then an NV12
        // frame over all of it, then two odd-width rects on top.
        renderer.update_rect(0, 0, 61, 40, &solid(61, 40, [255, 0, 0, 255]));
        let (w, h) = (cw & !1, ch & !1);
        // Blue (BT.709 limited range), so the chroma maths is tested too.
        let mut nv12 = vec![32u8; (w * h) as usize];
        nv12.extend([240u8, 118].repeat((w * h / 4) as usize));
        assert!(renderer.blit_nv12(0, 0, w, h, &nv12, &[]), "D3D12 NV12 blit");
        renderer.update_rect(10, 10, 61, 40, &solid(61, 40, [255, 0, 0, 255]));
        renderer.update_rect(300, 200, 37, 30, &solid(37, 30, [0, 255, 0, 255]));
        for _ in 0..3 {
            renderer.present_frame().unwrap();
            let _ = window.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let shot = window_pixels(&window, cw, ch);
        let at = |x: u32, y: u32| shot[((y * cw + x) * 4) as usize..][..3].to_vec();
        assert_eq!(at(40, 30), vec![255, 0, 0], "the first rect");
        assert_eq!(at(70, 49), vec![255, 0, 0], "its last pixel");
        assert_eq!(at(318, 215), vec![0, 255, 0], "the second rect, staged after the first");
        let blue = |p: Vec<u8>| p[0] < 40 && p[1] < 40 && p[2] > 215;
        assert!(blue(at(500, 400)), "the NV12 frame (blue): {:?}", at(500, 400));
        assert!(blue(at(5, 5)), "the frame over the first small rect: {:?}", at(5, 5));
    }

    /// The middle of each bar, on rows top to bottom, is its colour (give or
    /// take compression) — the second picture's: the bars right to left.
    fn check_bars(codec: Codec, what: &str, rgba: &[u8], w: u32, rows: &[u32]) {
        for &y in rows {
            for (i, want) in BARS_RGB.iter().rev().enumerate() {
                let x = w * (2 * i as u32 + 1) / 8;
                let p = &rgba[((y * w + x) * 4) as usize..][..3];
                let close = p.iter().zip([want.0, want.1, want.2]).all(|(&got, want)| got.abs_diff(want) < 60);
                assert!(close, "{} {what}: bar {i} on row {y} shows {p:?}, not {want:?}", codec.name());
            }
        }
    }

    /// A session the launcher opens in a Workspace tab draws into a child of
    /// the launcher's window (`RDPIO_EMBED_PARENT`), over the page's own
    /// window: its swapchain works there, the parent shows its picture where
    /// it is, and a tab that isn't in front counts as unseen.
    /// `cargo test -p rdp-client --bin rdpio inside_another -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_window_inside_another_shows_pictures() {
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::WindowsAndMessaging::*;
        crate::window::set_process_dpi_aware();
        let codec = Codec::H264;
        let (w, h) = (1280u32, 720u32);
        unsafe {
            let units = bars_stream(codec, w, h);
            let parent = CreateWindowExW(Default::default(), w!("STATIC"), w!("rdpio embed test"), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VISIBLE, 80, 80, 1000, 700, None, None, None, None).unwrap();
            // (The page's window, under the tab's session: GDI-drawn here.)
            let page = CreateWindowExW(Default::default(), w!("STATIC"), w!("the page"), WS_CHILD | WS_VISIBLE, 0, 0, 1000, 700, Some(parent), None, None, None).unwrap();
            std::env::set_var("RDPIO_EMBED_PARENT", (parent.0 as isize).to_string());
            let window = crate::window::Window::new("rdpio embedded", 640, 360).unwrap();
            std::env::remove_var("RDPIO_EMBED_PARENT");
            let child = HWND(window.hwnd_raw() as *mut _);
            assert_eq!(GetParent(child).ok(), Some(parent), "not inside the parent");
            assert!(window.output_hidden(), "hidden until the launcher places it");
            // Where the tab is (as the launcher does it).
            let (x, y, cw, ch) = (40, 60, 640u32, 360u32);
            SetWindowPos(child, Some(HWND_TOP), x, y, cw as i32, ch as i32, SWP_SHOWWINDOW | SWP_NOACTIVATE).unwrap();
            let _ = window.pump();
            assert!(!window.output_hidden(), "placed, it's seen");
            let mut renderer = Renderer::new(window.hwnd_raw(), cw, ch, rdp_gpu::Backend::D3D11).unwrap();
            renderer.set_keep_aspect(true);
            renderer.ensure_framebuffer(w, h).unwrap();
            let mut decoder = Decoder::new(&renderer, w, h, codec).unwrap();
            let mut rgba = Vec::new();
            for u in &units {
                if let Some(p) = decoder.decode(&u.data).unwrap() {
                    paint(&mut renderer, p, (w, h), &mut rgba);
                    renderer.present_frame().unwrap();
                }
                let _ = window.pump();
            }
            // The page repaints (as it does all the time): the session stays on top.
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(page), None, true);
            for _ in 0..4 {
                renderer.present_frame().unwrap();
                let _ = window.pump();
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut rc = RECT::default();
            GetClientRect(parent, &mut rc).unwrap();
            let (pw, ph) = (rc.right as u32, rc.bottom as u32);
            let shot = hwnd_pixels(parent.0 as isize, pw, ph);
            let mut tab = Vec::with_capacity((cw * ch * 4) as usize);
            for row in 0..ch {
                let at = (((y as u32 + row) * pw + x as u32) * 4) as usize;
                tab.extend_from_slice(&shot[at..at + (cw * 4) as usize]);
            }
            check_bars(codec, "window in a tab", &tab, cw, &[2, ch / 2, ch - 4]);
            // Another tab in front: this one is hidden, and unseen.
            let _ = ShowWindow(child, SW_HIDE);
            assert!(window.output_hidden(), "a hidden tab's session still counts as seen");
            let _ = DestroyWindow(parent);
        }
    }

    /// The window's client area as the compositor shows it (RGBA).
    fn window_pixels(window: &crate::window::Window, w: u32, h: u32) -> Vec<u8> {
        hwnd_pixels(window.hwnd_raw(), w, h)
    }

    fn hwnd_pixels(hwnd: isize, w: u32, h: u32) -> Vec<u8> {
        use windows::Win32::Graphics::Gdi::*;
        #[link(name = "user32")]
        extern "system" {
            fn PrintWindow(hwnd: *mut core::ffi::c_void, hdc: *mut core::ffi::c_void, flags: u32) -> i32;
        }
        const PW_CLIENTONLY: u32 = 1;
        const PW_RENDERFULLCONTENT: u32 = 2;
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(dc, bmp.into());
            let ok = PrintWindow(hwnd as *mut _, dc.0, PW_CLIENTONLY | PW_RENDERFULLCONTENT);
            assert!(ok != 0, "PrintWindow failed");
            let _ = GdiFlush();
            let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let out = src.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
            SelectObject(dc, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            out
        }
    }

    #[test]
    fn focus_loss_releases_exactly_what_is_held() {
        let mut h = Held::default();
        let w = |down| InputEvent::Key { scancode: 0x11, extended: false, down };
        h.note(&w(true));
        h.note(&InputEvent::Key { scancode: 0x1d, extended: false, down: true });
        h.note(&InputEvent::Key { scancode: 0x1d, extended: false, down: false });
        h.note(&InputEvent::Button { button: 0, down: true });
        h.note(&InputEvent::MouseRel { dx: 4, dy: 0 });
        let released = h.release_all();
        assert_eq!(released, vec![w(false), InputEvent::Button { button: 0, down: false }]);
        assert!(h.release_all().is_empty());
    }
}

    /// A renderer moved to the display's GPU and back in place, decoding and
    /// presenting after each move. `cargo test -p rdp-client --bin rdpio a_renderer_moves_gpus -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_renderer_moves_gpus() {
        crate::window::SHOW_INACTIVE.store(true, Ordering::Relaxed);
        let window = crate::window::Window::new("rdpio gpu move", 640, 360).unwrap();
        let mut renderer = Renderer::new(window.hwnd_raw(), 640, 360, rdp_gpu::Backend::D3D11).unwrap();
        renderer.ensure_framebuffer(640, 360).unwrap();
        for display in [true, false, true] {
            rdp_gpu::gpu::prefer_display_adapter(display);
            assert!(renderer.rebuild().unwrap());
            let on = renderer.device_context_clone().and_then(|(d, _)| rdp_gpu::gpu::device_adapter_luid(&d));
            assert_eq!(on, rdp_gpu::gpu::chosen_adapter_luid());
            let decoder = Decoder::new(&renderer, 640, 360, rdp_gpu::h264::Codec::Hevc).unwrap();
            println!("display gpu {display}: adapter {on:x?}, GPU decoder {}", matches!(decoder, Decoder::Gpu(_)));
            renderer.present_frame().unwrap();
            let _ = window.pump();
        }
        rdp_gpu::gpu::prefer_display_adapter(false);
    }

    /// Presenting only when the swapchain has room, waiting on its signal
    /// otherwise: as many presents as the screen takes, none of them blocking.
    /// `cargo test -p rdp-client --bin rdpio presents_without_waiting -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn presents_without_waiting() {
        crate::window::SHOW_INACTIVE.store(true, Ordering::Relaxed);
        for low_latency in [true, false] {
            let window = crate::window::Window::new("rdpio present pacing", 640, 360).unwrap();
            let mut renderer = Renderer::new(window.hwnd_raw(), 640, 360, rdp_gpu::Backend::D3D11).unwrap();
            renderer.set_low_latency(low_latency);
            renderer.ensure_framebuffer(640, 360).unwrap();
            let px = vec![200u8; 64 * 64 * 4];
            let (mut presents, mut times, mut wakes, mut passes) = (0u32, Vec::new(), 0u32, 0u32);
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(1) {
                passes += 1;
                let _ = window.pump();
                if renderer.frame_ready() {
                    renderer.update_rect((presents % 500) as u16, 0, 64, 64, &px);
                    let t = Instant::now();
                    renderer.present_frame().unwrap();
                    // (Not the first few: the window is still appearing.)
                    if presents >= 5 {
                        times.push(t.elapsed().as_secs_f64() * 1e3);
                    }
                    presents += 1;
                } else if window.wait_for_work_or(50, renderer.frame_ready_handle()) {
                    renderer.note_frame_ready();
                    wakes += 1;
                }
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |f: f64| times[((times.len() - 1) as f64 * f) as usize];
            println!(
                "low latency {low_latency}: {presents} presents in 1 s, present p50 {:.2} p95 {:.2} max {:.2} ms, {wakes} wakes on the signal, {passes} passes",
                q(0.5),
                q(0.95),
                q(1.0)
            );
            assert!(presents >= 30, "presents stalled");
            assert!(passes < presents * 3 + 50, "the loop spun: {passes} passes");
            assert!(q(0.95) < 4.0, "presents waited: p95 {:.2} ms", q(0.95));
        }
    }

    #[test]
    fn frames_bunched_by_a_stall_are_decoded_not_skipped() {
        // A GPU decoder (0.4 ms) catches up on a Wi-Fi burst of 5 or 50.
        assert!(!skip_backlog(5, 400));
        assert!(!skip_backlog(50, 400));
        // A CPU decoder at 12 ms a frame doesn't chase 5 behind.
        assert!(skip_backlog(5, 12_000));
        assert!(!skip_backlog(3, 12_000));
    }
