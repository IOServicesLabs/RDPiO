//! The Hyper client: direct sign-in (the Hyper password, SPAKE2), then the
//! stream — the same protocol and the same loops as the desktop client
//! (`crates/rdp-client/src/hyper_client.rs`), with MediaCodec for decoding and
//! AAudio for sound.
//!
//! Threads, per stream: `rx` (receive, FEC reassembly, control, audio,
//! feedback and pings), `video` (frames into the decoder, keyframe/repair
//! requests), `ctl` (input and the periodic control messages) and the
//! reliable channel's pump. One `out` thread per decoder renders what it
//! decodes. A dropped network signs in again by itself, like the desktop.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use ndk::native_window::NativeWindow;
use rdp_hyper::auth::{password_secret, AuthMsg, ClientAuth, Want};
use rdp_hyper::control::{Control, Feedback};
use rdp_hyper::input::{InputEvent, InputSender};
use rdp_hyper::messages::Message;
use rdp_hyper::video::{Event, FrameBuffers, Reassembler};
use rdp_hyper::wire::{Direction, Kind, Session};

use crate::audio::{AudioSink, MicSender, MicShape};
use crate::capture;
use crate::decoder::{Codec, Decoder};
use crate::held::Held;
use crate::reliable::ReliableLink;

/// Microseconds since this process's first call — the client clock.
pub fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// The app's version as the host sees it. The host may offer it a newer
/// rdpio; this client never takes one (it isn't a Windows exe).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The camera as the host's virtual camera takes it
/// (`hyper_camera::{WIDTH, HEIGHT}` on the desktop).
const HYPER_CAMERA: capture::CameraRequest = capture::CameraRequest { width: 1280, height: 720, fps: 30, bitrate: 2_500_000 };

/// A parked decoder (see [`Client::set_surface`]) is let go after this long
/// without a Surface: the app stays in the background.
const PARK_AT_MOST: Duration = Duration::from_secs(30);

/// A kept decoder showed nothing on its new Surface once: not tried again
/// this run.
static KEEP_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether a Surface change keeps the decoder (parked while there's none,
/// then onto the next one). (`debug.rdpio.keepdecoder=0`: released, and a new
/// one made for the new Surface, from a fresh keyframe.)
fn keep_decoder() -> bool {
    crate::tune::ab("keepdecoder", true) && !KEEP_FAILED.load(Ordering::Relaxed)
}

/// A working, signed-in UDP path to the host.
struct Link {
    socket: UdpSocket,
    send: Session,
    recv: Session,
    size: (u32, u32),
    fps: u32,
    codec: Codec,
    /// The host injects fingers as Windows touch.
    touch: bool,
    /// The host's local addresses (see `lan`).
    lan: Vec<String>,
}

fn connect_socket(host: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(if host.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    socket.connect(host)?;
    // A keyframe arrives as 100+ packets within a few ms: the default buffer
    // overflows (Android caps the request at its rmem_max, still far more).
    let size: libc::c_int = 8 * 1024 * 1024;
    unsafe {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        for opt in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, &size as *const _ as *const libc::c_void, std::mem::size_of_val(&size) as libc::socklen_t);
        }
        // Mark the stream's packets as video (Wi-Fi WMM: the video queue).
        let tos: libc::c_int = 0xA0;
        if host.is_ipv4() {
            libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_TOS, &tos as *const _ as *const libc::c_void, std::mem::size_of_val(&tos) as libc::socklen_t);
        }
    }
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket)
}

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
                    log::info!("host answered over UDP, rtt {:.1} ms", (now_us() - client_us) as f64 / 1000.0);
                    return Ok(());
                }
            }
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer"))
}

/// Sign in to the host's Hyper agent with the Hyper password
/// (`hyper_client::connect_direct` on the desktop).
fn connect_direct(host: SocketAddr, secret: &[u8; 32], want: &Want) -> Result<Link, String> {
    let socket = connect_socket(host).map_err(|e| e.to_string())?;
    let mut want = want.clone();
    let mut attempts = 0;
    let mut cookie_rounds = 0;
    'attempt: while attempts < 3 {
        attempts += 1;
        let (auth, hello) = ClientAuth::start(secret, want.clone());
        socket.send(&hello).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            let Ok(n) = socket.recv(&mut buf) else { continue };
            if let Some(v) = rdp_hyper::auth::foreign_version(&buf[..n]) {
                return Err(format!("the host runs an older rdpio (Hyper protocol {v}): update rdpio on the PC"));
            }
            match AuthMsg::parse(&buf[..n]) {
                Some(AuthMsg::Cookie { cookie }) if cookie_rounds < 3 => {
                    cookie_rounds += 1;
                    want.cookie = Some(rdp_hyper::auth::cookie_to_hex(&cookie));
                    attempts -= 1;
                    continue 'attempt;
                }
                Some(AuthMsg::Denied { retry_after_s }) => {
                    return Err(format!("the PC is refusing sign-ins from this address after failed attempts; try again in {retry_after_s} s"));
                }
                Some(ref msg @ AuthMsg::Challenge { .. }) if !auth.answers(msg) => {}
                Some(AuthMsg::Challenge { offer, .. }) => {
                    let key = auth.finish(&buf[..n]).ok_or("the PC's sign-in reply was malformed")?;
                    if offer.protocol == 0 {
                        return Err(format!(
                            "the PC runs rdpio {}, which can't stream to this app ({APP_VERSION}): update {}",
                            offer.app,
                            if rdp_hyper::update::newer((&offer.app, &offer.build), (APP_VERSION, "")) { "this app" } else { "rdpio on the PC" }
                        ));
                    }
                    let (min, max) = (rdp_hyper::wire::MIN_VERSION, rdp_hyper::wire::VERSION);
                    if !(min..=max).contains(&offer.protocol) {
                        return Err(format!("the PC chose Hyper protocol {}, which this app doesn't speak", offer.protocol));
                    }
                    let mut send = Session::with_version(&key, Direction::ClientToHost, offer.protocol);
                    let mut recv = Session::with_version(&key, Direction::ClientToHost, offer.protocol);
                    return match ping_until_pong(&socket, &mut send, &mut recv, Duration::from_secs(5)) {
                        Ok(()) => Ok(Link {
                            socket,
                            send,
                            recv,
                            size: (offer.width, offer.height),
                            fps: offer.fps,
                            codec: Codec::from_name(&offer.codec).unwrap_or(Codec::H264),
                            touch: offer.touch,
                            lan: offer.lan,
                        }),
                        Err(_) => Err("wrong Hyper password".into()),
                    };
                }
                _ => {}
            }
        }
    }
    Err(format!(
        "no answer from a Hyper host at {host} (is the rdpio Hyper service running on the PC, UDP {} open, and the same rdpio version on both?)",
        host.port()
    ))
}

/// Sign in directly on the PC's local network when this device is on it
/// (see `lan`), else at `addr`; `host` is the address as the user gave it.
fn connect_via(host: &str, addr: SocketAddr, secret: &[u8; 32], want: &Want) -> Result<Link, String> {
    if let Some(direct) = crate::lan::direct(host, addr) {
        match connect_direct(direct, secret, want) {
            Ok(link) => {
                log::info!("streaming over the local network at {direct}, not through {addr}");
                crate::lan::remember(host, &link.lan);
                return Ok(link);
            }
            Err(e) => log::info!("the PC's local address {direct} didn't sign in ({e}); using {addr}"),
        }
    }
    let link = connect_direct(addr, secret, want)?;
    crate::lan::remember(host, &link.lan);
    Ok(link)
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

/// How a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Closed,
    HostLeft,
    Lost,
    NeverStarted,
}

/// Input from the app, and what's held (released when the app loses focus).
struct InputState {
    sender: InputSender,
    held: Held,
    last_send: Option<Instant>,
    /// The last packets carrying everything unacknowledged.
    last_full_send: Option<Instant>,
    /// When the oldest event not yet sent came in (for the stats line).
    waiting_since: Option<Instant>,
}

impl Default for InputState {
    fn default() -> Self {
        // (`InputSender::new`, not its `Default`: the host takes sequence 1 first.)
        Self { sender: InputSender::new(), held: Held::default(), last_send: None, last_full_send: None, waiting_since: None }
    }
}

impl InputState {
    /// Whatever is due goes out now: a new button or key at once, motion
    /// at most once a millisecond (coalesced), a full resend after 15 ms (or
    /// a round trip, with `inputdup`).
    fn send_due(&mut self, live: &Live, stats: &Stats) -> bool {
        let since = self.last_send.map_or(Duration::MAX, |t| t.elapsed());
        let due = match self.sender.unsent() {
            Some(false) => true,
            Some(true) => since >= Duration::from_millis(1),
            None => false,
        };
        // `inputdup` (`debug.rdpio.inputdup=0`: off): each packet carries the
        // newest few events (and a key or tap until it went three times), a
        // lone key-up or finger-up goes again 5 ms later, and everything
        // unacknowledged once a round trip — instead of every unacknowledged
        // event in every packet, which over a relayed path (300 ms, seconds of
        // queue) is more input than the link carries (`capped_packets`).
        let capped = crate::tune::ab("inputdup", true);
        let resend_after = if capped {
            let rtt = Duration::from_micros(live.clock.lock().unwrap().rtt_us);
            (rtt + rtt / 4 + Duration::from_millis(10)).clamp(Duration::from_millis(15), Duration::from_secs(1))
        } else {
            Duration::from_millis(15)
        };
        let full = self.last_full_send.is_none_or(|t| t.elapsed() > resend_after);
        let repeat = capped && self.sender.wants_repeat() && since >= Duration::from_millis(5);
        if !(self.sender.has_pending() && (due || full || repeat)) {
            return false;
        }
        let packets = if capped && !full { self.sender.capped_packets(2) } else { self.sender.packets(2) };
        for p in packets {
            live.send(Kind::Input, &p);
        }
        self.last_send = Some(Instant::now());
        if full || !capped {
            self.last_full_send = self.last_send;
        }
        if let Some(t) = self.waiting_since.take() {
            stats.input_ms.lock().unwrap().push(t.elapsed().as_secs_f32() * 1000.0);
        }
        true
    }
}

/// A frame in the decoder, until its picture comes out.
struct InFlight {
    pts: u64,
    frame_id: u32,
    ltr: bool,
    capture_us: u64,
    /// Handed to the decoder (its decode time ends when the picture comes out).
    queued_at: Instant,
    /// A keyframe or a recovery frame (its decode time goes to the log): which,
    /// its size, and whether the clocks were told ahead (`adpfspike`).
    big: Option<(&'static str, usize, bool)>,
}

/// The pointer as the host draws it (shown by the app as the mouse's icon).
pub struct Cursor {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// Counters for the once-a-second stats line.
#[derive(Default)]
struct Stats {
    shown: AtomicU32,
    lost: AtomicU32,
    bytes: AtomicU64,
    latency_ms: Mutex<Vec<f32>>,
    /// Each frame's time in the decoder (queued → picture out).
    decode_ms: Mutex<Vec<f32>>,
    /// Input's wait from the app to the network.
    input_ms: Mutex<Vec<f32>>,
    /// Receive calls and datagrams.
    rx_calls: AtomicU64,
    rx_packets: AtomicU64,
    /// When the line last went to the log, and the PC's bitrate then.
    logged: Mutex<Option<(Instant, u32)>>,
    /// Pictures decoded and let go unshown behind a newer one (a clump).
    stale: AtomicU32,
    /// Automatic resolution: each second's numbers, for the app's next ask.
    auto: Mutex<Auto>,
}

/// Automatic resolution's state (see `autores`).
#[derive(Default)]
struct Auto {
    res: crate::autores::AutoRes,
    /// The latest second's numbers, and their number (each used once).
    sample: Option<(u64, crate::autores::Sample)>,
    used: u64,
}

/// Seconds since this process's first call (automatic resolution's clock).
fn now_s() -> f64 {
    now_us() as f64 / 1e6
}

/// The stats line goes to the log this often (and at once on a loss or a
/// bitrate cut): once a second it rolled everything else out of the log
/// within minutes.
const LOG_EVERY: Duration = Duration::from_secs(10);

pub struct Client {
    /// The PC's address as the user gave it ("host:port"), and resolved.
    host: String,
    addr: SocketAddr,
    secret: [u8; 32],
    want: Want,
    /// The user ended it (or the app is going away).
    quit: AtomicBool,
    /// "connecting", "playing", "reconnecting", "ended: …" (for the app).
    state: Mutex<String>,
    /// The stream's current link (for sending), while one plays.
    link: RwLock<Option<Arc<Live>>>,
    // What the app sets.
    surface: Mutex<Option<NativeWindow>>,
    surface_gen: AtomicU32,
    paused: AtomicBool,
    office: AtomicBool,
    view: Mutex<(u32, u32)>,
    /// The video thread's queue, for [`Client::nudge_video`] (while a stream plays).
    video_tx: Mutex<Option<mpsc::Sender<Event>>>,
    refresh_hz: AtomicU32,
    input: Mutex<InputState>,
    input_wake: Condvar,
    // Decoding (outlives a stream: a reconnect keeps the decoder).
    decoder: RwLock<Option<Arc<Decoder>>>,
    in_flight: Mutex<VecDeque<InFlight>>,
    ltr_decoded: AtomicU32,
    // What the app shows.
    /// The last shape the host sent (kept while it's hidden: the host shows
    /// it again without resending it).
    pub cursor: Mutex<Option<Cursor>>,
    pub cursor_visible: AtomicBool,
    pub cursor_gen: AtomicU32,
    pub clipboard: Mutex<Option<String>>,
    pub clipboard_gen: AtomicU32,
    pub notice: Mutex<Option<String>>,
    stats: Stats,
    stats_line: Mutex<String>,
    /// When the sign-in began, until the first picture (for the log).
    connect_at: Mutex<Option<Instant>>,
    /// The last new Surface, until the first picture on it: when it came,
    /// and how the decoder got to it (for the log).
    surface_at: Mutex<Option<(Instant, &'static str)>>,
}

/// What one stream's threads share.
struct Live {
    socket: UdpSocket,
    send: Mutex<Session>,
    stop: AtomicBool,
    lost: AtomicBool,
    heard: AtomicBool,
    input_ack: AtomicU32,
    clock: Mutex<ClockSync>,
    size: Mutex<(u32, u32)>,
    codec: Codec,
    touch: bool,
    fps: AtomicU32,
    bitrate_kbps: AtomicU32,
    fec_permille: AtomicU32,
    /// How the host reaches us, as it last said ("relay ord", "direct",
    /// "LAN"; empty: not told). For the stats line.
    path: Mutex<String>,
    reliable: Arc<ReliableLink>,
    /// Frames received and not yet taken by the video thread.
    queued: AtomicU32,
    /// Where the video thread gives decoded-from frames' buffers back to the
    /// receive thread's reassembler: about 5x less work a frame there.
    frames: FrameBuffers,
}

impl Live {
    fn send(&self, kind: Kind, body: &[u8]) {
        let packet = self.send.lock().unwrap().seal(kind, body);
        let _ = self.socket.send(&packet);
    }
}

impl Client {
    /// Sign in (blocking: seconds). The stream then plays on its own threads
    /// until [`Client::close`]; it signs in again after a network drop.
    pub fn connect(host: &str, port: u16, password: &str, want: Want) -> Result<Arc<Self>, String> {
        let addr = resolve(host, port)?;
        let secret = password_secret(password);
        log::info!("signing in to {addr}: {}x{} @ {} Hz, codecs {:?}", want.width, want.height, want.refresh_hz, want.codecs);
        let host = format!("{host}:{port}");
        let started = Instant::now();
        // The decoder's component, made while the sign-in runs.
        crate::decoder::prewarm(want.codecs.iter().find_map(|c| Codec::from_name(c)));
        let link = match connect_via(&host, addr, &secret, &want) {
            Ok(l) => l,
            Err(e) => {
                crate::decoder::drop_warm();
                return Err(e);
            }
        };
        let client = Arc::new(Self {
            host,
            addr,
            secret,
            office: AtomicBool::new(want.office),
            refresh_hz: AtomicU32::new(want.refresh_hz),
            view: Mutex::new((want.width, want.height)),
            video_tx: Mutex::new(None),
            want,
            quit: AtomicBool::new(false),
            state: Mutex::new("connecting".into()),
            link: RwLock::new(None),
            surface: Mutex::new(None),
            surface_gen: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            input: Mutex::new(InputState::default()),
            input_wake: Condvar::new(),
            decoder: RwLock::new(None),
            in_flight: Mutex::new(VecDeque::new()),
            ltr_decoded: AtomicU32::new(u32::MAX),
            cursor: Mutex::new(None),
            cursor_visible: AtomicBool::new(true),
            cursor_gen: AtomicU32::new(0),
            clipboard: Mutex::new(None),
            clipboard_gen: AtomicU32::new(0),
            notice: Mutex::new(None),
            stats: Stats::default(),
            stats_line: Mutex::new(String::new()),
            connect_at: Mutex::new(Some(started)),
            surface_at: Mutex::new(None),
        });
        let c = client.clone();
        std::thread::Builder::new().name("hyper-session".into()).spawn(move || c.supervise(link)).map_err(|e| e.to_string())?;
        Ok(client)
    }

    fn set_state(&self, s: impl Into<String>) {
        *self.state.lock().unwrap() = s.into();
        crate::notify::poke();
    }

    /// Streams, and signs in again when the network drops.
    fn supervise(self: Arc<Self>, first: Link) {
        let mut link = first;
        let mut failures: Vec<Instant> = Vec::new();
        loop {
            self.set_state("playing");
            let ended = self.clone().stream(link);
            log::info!("stream ended: {ended:?}");
            match ended {
                Ended::Closed => return self.set_state("ended: closed"),
                Ended::HostLeft => return self.set_state("ended: the PC ended the stream"),
                Ended::NeverStarted => {
                    failures.retain(|t| t.elapsed() < Duration::from_secs(60));
                    if failures.len() >= 3 {
                        return self.set_state("ended: the PC accepts the sign-in but doesn't start the stream (see its log)");
                    }
                    failures.push(Instant::now());
                }
                Ended::Lost => {}
            }
            // (Its first picture is logged against this: drop → picture.)
            let dropped = Instant::now();
            *self.connect_at.lock().unwrap() = Some(dropped);
            let mut relinked = None;
            for attempt in 1..=15 {
                if self.quit.load(Ordering::Relaxed) {
                    return self.set_state("ended: closed");
                }
                self.set_state(format!("reconnecting ({attempt})"));
                // 250 ms, doubling to 2 s (`fastreconnect`; off: 0.3 s, then 2 s):
                // the PC ends the stream we gave up on as our sign-in arrives.
                let wait = rdp_hyper::liveness::reconnect_wait(attempt, crate::tune::ab("fastreconnect", true));
                let until = Instant::now() + wait;
                while Instant::now() < until && !self.quit.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(25));
                }
                match connect_via(&self.host, self.addr, &self.secret, &self.want) {
                    Ok(l) => {
                        log::info!("signed in again {} ms after the drop (attempt {attempt})", dropped.elapsed().as_millis());
                        relinked = Some(l);
                        break;
                    }
                    Err(e) if e.contains("refusing") => return self.set_state(format!("ended: {e}")),
                    Err(e) => log::info!("not back yet: {e}"),
                }
            }
            match relinked {
                Some(l) => link = l,
                None => return self.set_state("ended: lost the connection to the PC"),
            }
        }
    }

    /// Play one signed-in link until it ends.
    fn stream(self: Arc<Self>, link: Link) -> Ended {
        let Link { socket, send, recv, size, fps, codec, touch, .. } = link;
        log::info!("stream {}x{} @ {fps} fps, {}, touch {touch}", size.0, size.1, codec.name());
        let Ok(rx_socket) = socket.try_clone() else { return Ended::Lost };
        let live = Arc::new(Live {
            socket,
            send: Mutex::new(send),
            stop: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            heard: AtomicBool::new(false),
            input_ack: AtomicU32::new(0),
            clock: Mutex::new(ClockSync::default()),
            size: Mutex::new(size),
            codec,
            touch,
            fps: AtomicU32::new(fps),
            bitrate_kbps: AtomicU32::new(0),
            fec_permille: AtomicU32::new(0),
            path: Mutex::new(String::new()),
            reliable: ReliableLink::new(),
            queued: AtomicU32::new(0),
            frames: FrameBuffers::default(),
        });
        live.reliable.set_bulk_rate(Some(20_000_000));
        *self.link.write().unwrap() = Some(live.clone());
        // Input from before (a reconnect wait) is stale.
        {
            let mut i = self.input.lock().unwrap();
            *i = InputState::default();
        }
        self.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
        let pump_stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let l = live.clone();
            live.reliable.spawn_pump(move |chunk| l.send(Kind::Reliable, chunk), pump_stop.clone())
        };
        let (tx, rx) = mpsc::channel::<Event>();
        *self.video_tx.lock().unwrap() = Some(tx.clone());
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f).ok();
        let threads = [
            {
                let (c, l) = (self.clone(), live.clone());
                spawn("hyper-rx", Box::new(move || c.receive_loop(rx_socket, recv, l, tx)))
            },
            {
                let (c, l) = (self.clone(), live.clone());
                spawn("hyper-video", Box::new(move || c.video_loop(l, rx)))
            },
            {
                let (c, l) = (self.clone(), live.clone());
                spawn("hyper-ctl", Box::new(move || c.control_loop(l)))
            },
        ];
        live.send(Kind::Control, &Control::KeyframeRequest.to_bytes());
        while !live.stop.load(Ordering::Relaxed) {
            if self.quit.load(Ordering::Relaxed) {
                live.stop.store(true, Ordering::Relaxed);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        // (The video thread may be asleep: it sees the stop at once.)
        self.nudge_video();
        self.video_tx.lock().unwrap().take();
        let quit = self.quit.load(Ordering::Relaxed);
        if quit {
            for _ in 0..3 {
                live.send(Kind::Control, &Control::Bye.to_bytes());
            }
        }
        self.input_wake.notify_all();
        for t in threads.into_iter().flatten() {
            let _ = t.join();
        }
        pump_stop.store(true, Ordering::Relaxed);
        live.reliable.nudge();
        let _ = pump.join();
        self.link.write().unwrap().take();
        if quit {
            Ended::Closed
        } else if live.lost.load(Ordering::Relaxed) {
            if live.heard.load(Ordering::Relaxed) {
                Ended::Lost
            } else {
                Ended::NeverStarted
            }
        } else {
            Ended::HostLeft
        }
    }

    fn receive_loop(self: Arc<Self>, socket: UdpSocket, mut recv: Session, live: Arc<Live>, tx: mpsc::Sender<Event>) {
        crate::raise_thread(-10);
        let mut placement = crate::tune::Placement::default();
        let _ = socket.set_read_timeout(Some(Duration::from_millis(25)));
        // Data shards straight into the frame's buffer (`debug.rdpio.stageshards=1`:
        // each shard copied twice, as before; read at connect).
        let mut reassembler = Reassembler::with_buffers(live.frames.clone()).stage_shards(crate::tune::ab("stageshards", false));
        let mut last_feedback = Instant::now();
        let mut last_ping = Instant::now() - Duration::from_secs(1);
        let mut last_rx = Instant::now();
        let mut last_video = Instant::now();
        let mut quiet = 0usize;
        let mut ping_id = 0u32;
        // Silence being probed (logged once a spell).
        let mut probing = false;
        let (mut delay_min, mut delay_sum, mut delay_n) = (i64::MAX, 0i64, 0i64);
        let mut last_trouble = Instant::now();
        let mut audio: Option<AudioSink> = None;
        let mut mic: Option<MicSender> = None;
        let mic_shape = Arc::new(MicShape::default());
        // The camera request this session made (its generation), while on.
        let mut camera: Option<u32> = None;
        let mut rx = crate::rx::Rx::new();
        let mut got = false;
        while !live.stop.load(Ordering::Relaxed) {
            let office = self.office.load(Ordering::Relaxed);
            placement.follow(!office);
            // In office mode, a millisecond's nap after a batch: a frame's
            // shards then come several to a call instead of one wake-up each
            // (through a VPN they trickle in one by one). The receive thread's
            // CPU, 24 Mbit/s over Tailscale: 11% of a core -> 4%, for up to a
            // millisecond on a frame. (`debug.rdpio.rxnap=0`: no nap.)
            if got && office && rx.pending() == 0 && crate::tune::ab("rxnap", true) {
                std::thread::sleep(Duration::from_millis(1));
            }
            got = false;
            if let Some(packet) = rx.recv(&socket) {
                got = true;
                let n = packet.len();
                if let Ok((kind, body)) = recv.open_in_place(packet) {
                    last_rx = Instant::now();
                    match kind {
                        Kind::Video => {
                            live.heard.store(true, Ordering::Relaxed);
                            last_video = last_rx;
                            self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
                            let now = now_us();
                            for ev in reassembler.push(&body, now) {
                                if let Event::Frame(f) = &ev {
                                    // From its first shard: a big frame's own
                                    // length on the wire isn't a queue.
                                    let d = f.first_rx_us as i64 - (f.capture_us + f.encode_us as u64) as i64;
                                    delay_min = delay_min.min(d);
                                    delay_sum += d;
                                    delay_n += 1;
                                    live.queued.fetch_add(1, Ordering::Relaxed);
                                }
                                if tx.send(ev).is_err() {
                                    return;
                                }
                            }
                        }
                        Kind::Control => match Control::parse(&body) {
                            Some(Control::Pong { client_us, host_us, .. }) => live.clock.lock().unwrap().sample(client_us, host_us),
                            Some(Control::Status { input_ack, width, height, fps, bitrate_kbps, fec_permille }) => {
                                live.input_ack.store(input_ack, Ordering::Relaxed);
                                if std::mem::replace(&mut *live.size.lock().unwrap(), (width as u32, height as u32)) != (width as u32, height as u32) {
                                    crate::notify::poke();
                                }
                                live.fps.store(fps as u32, Ordering::Relaxed);
                                live.bitrate_kbps.store(bitrate_kbps, Ordering::Relaxed);
                                live.fec_permille.store(fec_permille as u32, Ordering::Relaxed);
                                self.input_wake.notify_one();
                            }
                            Some(Control::Bye) => {
                                log::info!("the PC ended the stream");
                                live.stop.store(true, Ordering::Relaxed);
                            }
                            // How the PC reaches us (LAN, Tailscale direct or relayed).
                            Some(Control::Path { kind, relay }) => {
                                let now = kind.describe(&relay);
                                let mut path = live.path.lock().unwrap();
                                if *path != now {
                                    log::info!("path to the PC: {now}");
                                    static RELAY_HINTED: AtomicBool = AtomicBool::new(false);
                                    if kind == rdp_hyper::control::PathKind::Relay && !RELAY_HINTED.swap(true, Ordering::Relaxed) {
                                        log::warn!("Tailscale is relaying this connection (no direct UDP): expect lag");
                                    }
                                    *path = now;
                                }
                            }
                            Some(Control::Reconnect) => {
                                // (Its console moved to another session, or its service is restarting.)
                                log::info!("the PC asked for a reconnect; reconnecting now");
                                live.lost.store(true, Ordering::Relaxed);
                                live.stop.store(true, Ordering::Relaxed);
                            }
                            Some(Control::Notice { text }) => {
                                log::info!("notice from the PC: {text}");
                                *self.notice.lock().unwrap() = Some(text);
                                crate::notify::poke();
                            }
                            // A PC app is listening: send the tablet's mic, only meanwhile.
                            Some(Control::Mic { on }) => {
                                if mic.as_ref().is_some_and(|m| m.is_finished()) {
                                    mic = None;
                                }
                                if on && mic.is_none() {
                                    mic_shape.long.store(self.office.load(Ordering::Relaxed), Ordering::Relaxed);
                                    let l = live.clone();
                                    mic = Some(MicSender::start(Arc::new(move |body: &[u8]| l.send(Kind::Audio, body)), mic_shape.clone(), now_us));
                                } else if !on {
                                    // (Stopping joins the capture: not on this thread.)
                                    if let Some(m) = mic.take() {
                                        std::thread::spawn(move || drop(m));
                                    }
                                }
                            }
                            Some(Control::AudioReport { frame_ms, lost }) => {
                                // (Every 2 s: a mode switch reaches the mic within that.)
                                mic_shape.long.store(self.office.load(Ordering::Relaxed), Ordering::Relaxed);
                                mic_shape.report(frame_ms, lost);
                            }
                            // A PC app is using the camera ("rdpio Camera"): send ours.
                            Some(Control::Camera { on }) => {
                                if on && camera.is_none() {
                                    let l = live.clone();
                                    let mut frame_id = 0u32;
                                    let sink: capture::CameraSink = Box::new(move |data: &[u8], keyframe: bool| {
                                        let out = rdp_hyper::video::OutFrame { frame_id, data, keyframe, ltr: false, recovery: false, capture_us: now_us(), encode_us: 0 };
                                        for body in rdp_hyper::video::packetize(&out, 0.1) {
                                            l.send(Kind::Camera, &body);
                                        }
                                        frame_id = frame_id.wrapping_add(1);
                                    });
                                    camera = Some(capture::want_camera(Some((HYPER_CAMERA, sink))));
                                } else if let Some(generation) = camera.filter(|_| !on) {
                                    capture::release_camera(generation);
                                    camera = None;
                                }
                            }
                            Some(Control::CameraKeyframe) => capture::camera_keyframe(),
                            _ => {}
                        },
                        Kind::Audio => {
                            let a = audio.get_or_insert_with(|| {
                                let c = self.clone();
                                AudioSink::start(Box::new(move || c.office.load(Ordering::Relaxed)))
                            });
                            a.push(&body);
                            if let Some(report) = a.report() {
                                live.send(Kind::Control, &report.to_bytes());
                            }
                        }
                        Kind::Reliable => {
                            let (ack, msgs) = live.reliable.on_packet(&body);
                            if let Some(a) = ack {
                                live.send(Kind::Reliable, &a);
                            }
                            for m in msgs {
                                self.on_message(m);
                            }
                        }
                        // (Updates: this app isn't updated by the host.)
                        _ => {}
                    }
                }
            }
            // Silence: suspect after 1.5 s (ping harder), dropped after 3 s
            // or so (`debug.rdpio.fastreconnect=0`: 6 s) — see
            // rdp_hyper::liveness. Hidden, we ping only every 2 s: 6 s then.
            {
                use rdp_hyper::liveness::{verdict, Verdict, PROBE_EVERY};
                let rtt = Duration::from_micros(live.clock.lock().unwrap().rtt_us);
                let silent = last_rx.elapsed();
                let paused = self.paused.load(Ordering::Relaxed);
                match verdict(silent, live.heard.load(Ordering::Relaxed), paused, rtt, crate::tune::ab("fastreconnect", true)) {
                    Verdict::Lost if !live.stop.load(Ordering::Relaxed) => {
                        log::warn!("nothing from the PC for {} ms", silent.as_millis());
                        live.lost.store(true, Ordering::Relaxed);
                        live.stop.store(true, Ordering::Relaxed);
                    }
                    Verdict::Probe => {
                        if !probing {
                            probing = true;
                            log::info!("nothing from the PC for {} ms; probing", silent.as_millis());
                        }
                        if last_ping.elapsed() >= PROBE_EVERY {
                            ping_id += 1;
                            live.send(Kind::Control, &Control::Ping { id: ping_id, client_us: now_us() }.to_bytes());
                            last_ping = Instant::now();
                        }
                    }
                    _ => probing = false,
                }
            }
            // 0 playing, 1 a still picture, 2 paused (hidden, or the app in
            // the background): fewer wakeups and messages the quieter it is.
            let level = if last_video.elapsed() <= Duration::from_secs(1) {
                0
            } else if self.paused.load(Ordering::Relaxed) {
                2
            } else {
                1
            };
            if level != quiet {
                quiet = level;
                let _ = socket.set_read_timeout(Some(Duration::from_millis([25, 125, 500][level])));
            }
            let idle = quiet > 0;
            let every = if quiet == 2 {
                1000
            } else if idle {
                250
            } else if last_trouble.elapsed() > Duration::from_secs(5) {
                100
            } else {
                50
            };
            if last_feedback.elapsed() >= Duration::from_millis(every) {
                let s = reassembler.take_stats();
                if s.frames_lost > 0 || s.recovered > 0 {
                    last_trouble = Instant::now();
                }
                let fb = Feedback {
                    shards: s.shards,
                    recovered: s.recovered,
                    frames: s.frames,
                    frames_lost: s.frames_lost,
                    bytes: s.bytes,
                    delay_min_us: if delay_n > 0 { delay_min } else { 0 },
                    delay_avg_us: if delay_n > 0 { delay_sum / delay_n } else { 0 },
                    window_us: last_feedback.elapsed().as_micros() as u32,
                    ltr_frame: Some(self.ltr_decoded.load(Ordering::Relaxed)).filter(|&v| v != u32::MAX),
                };
                live.send(Kind::Control, &Control::Feedback(fb).to_bytes());
                let (calls, packets) = rx.take_counts();
                self.stats.rx_calls.fetch_add(calls, Ordering::Relaxed);
                self.stats.rx_packets.fetch_add(packets, Ordering::Relaxed);
                (delay_min, delay_sum, delay_n) = (i64::MAX, 0, 0);
                last_feedback = Instant::now();
            }
            if last_ping.elapsed() >= Duration::from_millis([500, 1000, 2000][quiet]) {
                ping_id += 1;
                live.send(Kind::Control, &Control::Ping { id: ping_id, client_us: now_us() }.to_bytes());
                last_ping = Instant::now();
            }
        }
        if let Some(generation) = camera {
            capture::release_camera(generation);
        }
    }

    fn on_message(&self, m: Message) {
        match m {
            Message::CursorShape { width, height, hot_x, hot_y, rgba } => {
                *self.cursor.lock().unwrap() = Some(Cursor { width, height, hot_x, hot_y, rgba });
                self.cursor_visible.store(true, Ordering::Relaxed);
                self.cursor_gen.fetch_add(1, Ordering::Relaxed);
                crate::notify::poke();
            }
            Message::CursorVisible(v) => {
                self.cursor_visible.store(v, Ordering::Relaxed);
                self.cursor_gen.fetch_add(1, Ordering::Relaxed);
                crate::notify::poke();
            }
            Message::ClipboardText(t) => {
                *self.clipboard.lock().unwrap() = Some(t);
                self.clipboard_gen.fetch_add(1, Ordering::Relaxed);
                crate::notify::poke();
            }
            // (Images and files: not on this app yet.)
            _ => {}
        }
    }

    /// (Re)make the decoder for the stream's size and codec on the current
    /// Surface. `None`: no Surface (the app is in the background).
    fn make_decoder(&self, codec: Codec, (w, h): (u32, u32)) -> Option<Arc<Decoder>> {
        let mut slot = self.decoder.write().unwrap();
        if let Some(old) = slot.take() {
            old.release();
        }
        let surface = self.surface.lock().unwrap();
        let window = surface.as_ref()?;
        // Game mode: flat out (every millisecond of decode is latency).
        // Office mode: clocked for its 60 fps, when that switch is on.
        let rate = self.office.load(Ordering::Relaxed).then_some(60);
        match Decoder::new(window, codec, w, h, rate) {
            Ok(d) => {
                let d = Arc::new(d);
                *slot = Some(d.clone());
                self.in_flight.lock().unwrap().clear();
                Some(d)
            }
            Err(e) => {
                log::warn!("decoder: {e}");
                None
            }
        }
    }

    fn video_loop(self: Arc<Self>, live: Arc<Live>, rx: mpsc::Receiver<Event>) {
        crate::raise_thread(-10);
        let mut placement = crate::tune::Placement::default();
        let mut decoder: Option<Arc<Decoder>> = None;
        let mut decoder_gen = u32::MAX;
        let mut out_thread: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> = None;
        let mut awaiting_key = true;
        let mut can_repair = false;
        let mut awaiting_since = Instant::now();
        let mut bad_keyframes = 0u32;
        let mut behind = crate::backlog::Backlog::default();
        let mut last_key_request = Instant::now() - Duration::from_secs(1);
        // (Each frame's handling against the frame interval.)
        let frame_time = |fps: u32| Duration::from_secs(1) / fps.clamp(24, 240);
        let mut hint = crate::hint::Hint::for_this_thread(frame_time(live.fps.load(Ordering::Relaxed)));
        let mut was_paused = false;
        // A decoder that wouldn't start isn't tried again at once.
        let mut retry_decoder_at = Instant::now();
        // A kept decoder just moved to a new Surface: since when, and its
        // (frames queued, pictures out) then — it should show something soon.
        let mut moved: Option<(Instant, (u64, u64))> = None;
        let stop_out = |t: &mut Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>| {
            if let Some((stop, h)) = t.take() {
                stop.store(true, Ordering::Relaxed);
                let _ = h.join();
            }
        };
        while !live.stop.load(Ordering::Relaxed) {
            placement.follow(!self.office.load(Ordering::Relaxed));
            if let Some(h) = hint.as_mut() {
                // Office mode: the same deadlines at lower clocks where the
                // work allows. (`debug.rdpio.adpfeco=0`: never asked.)
                h.prefer_efficiency(self.office.load(Ordering::Relaxed) && crate::tune::ab("adpfeco", true));
            }
            let size = *live.size.lock().unwrap();
            let surface_now = self.surface_gen.load(Ordering::Relaxed);
            // A parked decoder (see `set_surface`) and a Surface change: it
            // stays parked while there's none, or goes onto the new one as it
            // is (same size and codec) — decoding on from where it was.
            if let Some(d) = decoder.as_ref().filter(|d| surface_now != decoder_gen && d.parked_for().is_some()) {
                let fits = (d.width, d.height) == size && d.kind == live.codec && d.is_async() == Decoder::wants_async();
                let slot = self.decoder.write().unwrap();
                let ours = slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, d));
                let surface = self.surface.lock().unwrap();
                match surface.as_ref() {
                    None if ours => decoder_gen = surface_now,
                    Some(w) if ours && fits => match d.unpark(w) {
                        Ok(()) => {
                            decoder_gen = surface_now;
                            if let Some(at) = self.surface_at.lock().unwrap().as_mut() {
                                at.1 = "decoder kept";
                            }
                            moved = Some((Instant::now(), d.counts()));
                            log::info!("decoder kept: onto the new Surface");
                        }
                        Err(e) => log::warn!("decoder: {e}; a new one"),
                    },
                    _ => {}
                }
            }
            // Parked for long (the app stays in the background): let go of it.
            if let Some(t) = decoder.as_ref().and_then(|d| d.parked_for()).filter(|t| *t > PARK_AT_MOST) {
                stop_out(&mut out_thread);
                if let Some(d) = decoder.take() {
                    let mut slot = self.decoder.write().unwrap();
                    if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &d)) {
                        slot.take();
                    }
                    d.release();
                }
                log::info!("decoder let go after {} s parked", t.as_secs());
            }
            // A kept decoder that shows nothing on its new Surface: a new
            // one, and no more keeping.
            if let (Some((at, (q0, s0))), Some(d)) = (moved, decoder.as_ref()) {
                let (q, s) = d.counts();
                if s > s0 {
                    moved = None;
                } else if q >= q0 + 10 && at.elapsed() > Duration::from_millis(1500) {
                    log::warn!("the kept decoder shows nothing on its new Surface; a new one (and none kept from now on)");
                    KEEP_FAILED.store(true, Ordering::Relaxed);
                    decoder_gen = u32::MAX;
                    moved = None;
                }
            }
            // A new Surface (the app came back), a new stream size, or no
            // decoder yet: a decoder for it, and a keyframe to start from.
            let stale = decoder.as_ref().is_none_or(|d| (d.width, d.height) != size || d.kind != live.codec || d.is_async() != Decoder::wants_async())
                || surface_now != decoder_gen;
            let retry = surface_now != decoder_gen || Instant::now() >= retry_decoder_at;
            if stale && size.0 > 0 && size.1 > 0 && retry {
                stop_out(&mut out_thread);
                decoder = None;
                moved = None;
                decoder_gen = surface_now;
                retry_decoder_at = Instant::now() + Duration::from_secs(1);
                if let Some(d) = self.make_decoder(live.codec, size) {
                    let stop = Arc::new(AtomicBool::new(false));
                    let (c, s, dd, l) = (self.clone(), stop.clone(), d.clone(), live.clone());
                    out_thread = std::thread::Builder::new().name("hyper-out".into()).spawn(move || c.output_loop(dd, l, s)).ok().map(|h| (stop, h));
                    decoder = Some(d);
                    log::info!("decoding {}x{}", size.0, size.1);
                }
                awaiting_key = true;
                can_repair = false;
                self.ltr_decoded.store(u32::MAX, Ordering::Relaxed);
                last_key_request = Instant::now() - Duration::from_secs(1);
            }
            let paused = self.paused.load(Ordering::Relaxed);
            if was_paused && !paused {
                awaiting_key = true;
                last_key_request = Instant::now() - Duration::from_secs(1);
            }
            was_paused = paused;
            // Nothing to pace (no keyframe to ask for): sleep until a frame
            // or a nudge (the Surface or the pause changed) comes.
            let pacing = awaiting_key && decoder.is_some() && !paused;
            let wait = if pacing { 20 } else { 1000 };
            match rx.recv_timeout(Duration::from_millis(wait)) {
                // (A nudge: see `nudge_video`.)
                Ok(Event::Lost { first: 1, last: 0 }) => {}
                Ok(Event::Lost { first, last }) => {
                    self.stats.lost.fetch_add(last.wrapping_sub(first) + 1, Ordering::Relaxed);
                    if !awaiting_key {
                        awaiting_since = Instant::now();
                        last_key_request = Instant::now() - Duration::from_secs(1);
                    }
                    awaiting_key = true;
                }
                Ok(Event::Frame(f)) => {
                    let work = Instant::now();
                    let backlog = live.queued.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                    // Behind by more than ~50 ms of frames, and not draining
                    // (a decoder slower than the stream): catching up would
                    // only add latency — drop them and start again from a
                    // fresh keyframe. A clump off a stalled link drains in a
                    // few ms a frame: decoded, not dropped (see `backlog`).
                    let fps = live.fps.load(Ordering::Relaxed);
                    if !awaiting_key && behind.skip(Instant::now(), backlog, fps, crate::tune::ab("burstok", true)) {
                        log::info!("decode backlog of {backlog} frames; skipping to a fresh keyframe");
                        while let Ok(ev) = rx.try_recv() {
                            if matches!(ev, Event::Frame(_)) {
                                live.queued.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        awaiting_key = true;
                        can_repair = false;
                        last_key_request = Instant::now() - Duration::from_secs(1);
                        continue;
                    }
                    let Some(d) = decoder.as_ref() else {
                        awaiting_key = true;
                        continue;
                    };
                    if paused || (awaiting_key && !f.keyframe && !(f.recovery && can_repair)) {
                        continue;
                    }
                    // (Pictures come out in order, so their times tell them
                    // apart; the host's capture clock only moves forward.)
                    let pts = f.capture_us;
                    // A big frame: the clocks up before it rather than after
                    // it ran late (Android 16). (`debug.rdpio.adpfspike=0`: not told.)
                    let big = (f.keyframe || f.recovery).then(|| {
                        let spiked = crate::tune::ab("adpfspike", true) && hint.as_ref().is_some_and(|h| h.spike());
                        (if f.keyframe { "keyframe" } else { "recovery frame" }, f.data.len(), spiked)
                    });
                    let queued = d.queue(&f.data, pts, Duration::from_millis(30));
                    // Its buffer, for a frame to come. (`debug.rdpio.framebufs=0`:
                    // a new one each frame.)
                    if crate::tune::ab("framebufs", true) {
                        live.frames.give(f.data);
                    }
                    if let Some(h) = hint.as_mut() {
                        h.set_target(frame_time(live.fps.load(Ordering::Relaxed)));
                        h.report(work.elapsed());
                    }
                    match queued {
                        Ok(true) => {
                            awaiting_key = false;
                            bad_keyframes = 0;
                            if f.keyframe {
                                can_repair = true;
                            }
                            let mut q = self.in_flight.lock().unwrap();
                            q.push_back(InFlight { pts, frame_id: f.frame_id, ltr: f.ltr, capture_us: f.capture_us, queued_at: Instant::now(), big });
                            while q.len() > 32 {
                                q.pop_front();
                            }
                        }
                        Ok(false) => {
                            // The decoder is behind: this frame is gone, so the
                            // ones after it can't decode either.
                            log::debug!("decoder full; skipping to a keyframe");
                            awaiting_key = true;
                            can_repair = false;
                        }
                        Err(e) => {
                            log::warn!("decode: {e}");
                            awaiting_key = true;
                            can_repair = false;
                            if f.keyframe {
                                bad_keyframes += 1;
                                if bad_keyframes >= 3 {
                                    bad_keyframes = 0;
                                    // A fresh decoder (picked up at the top).
                                    d.distrust();
                                    decoder_gen = u32::MAX;
                                }
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let key_interval = if bad_keyframes > 0 { Duration::from_millis(500) } else { Duration::from_millis(100) };
            let rtt = Duration::from_micros(live.clock.lock().unwrap().rtt_us);
            let repair = can_repair && awaiting_since.elapsed() < Duration::from_secs(1);
            let interval = if repair { key_interval.max(rtt * 2) } else { key_interval };
            if awaiting_key && decoder.is_some() && !paused && last_key_request.elapsed() > interval {
                let ask = if repair { Control::Recover } else { Control::KeyframeRequest };
                live.send(Kind::Control, &ask.to_bytes());
                last_key_request = Instant::now();
            }
        }
        stop_out(&mut out_thread);
    }

    /// Decoded pictures → the screen, as they come.
    fn output_loop(self: Arc<Self>, d: Arc<Decoder>, live: Arc<Live>, stop: Arc<AtomicBool>) {
        crate::raise_thread(-10);
        let mut placement = crate::tune::Placement::default();
        let mut quiet_since = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            placement.follow(!self.office.load(Ordering::Relaxed));
            // The call returns as soon as a picture is out, however long it
            // may wait: a short wait only means more calls, each a round trip
            // through the codec's own thread (~750 wakeups a second at 60 fps
            // with 8 ms). (`debug.rdpio.longwait=0`: the old 8 / 30 ms.)
            let wait = if crate::tune::ab("longwait", true) {
                50
            } else if quiet_since.elapsed() > Duration::from_secs(1) {
                30
            } else {
                8
            };
            match d.render_next(Duration::from_millis(wait)) {
                Ok(Some(shown)) => {
                    quiet_since = Instant::now();
                    let frame = {
                        let mut q = self.in_flight.lock().unwrap();
                        while let Some(f) = q.front().filter(|f| f.pts < shown.pts) {
                            // Decoded and let go unshown (a newer one came with
                            // it): a long-term reference all the same.
                            if f.ltr && shown.skipped.contains(&f.pts) {
                                self.ltr_decoded.store(f.frame_id, Ordering::Relaxed);
                            }
                            q.pop_front();
                        }
                        q.front().filter(|f| f.pts == shown.pts).map(|f| (f.frame_id, f.ltr, f.capture_us, f.queued_at, f.big))
                    };
                    if let Some((frame_id, ltr, capture_us, queued_at, big)) = frame {
                        let decode_ms = queued_at.elapsed().as_secs_f32() * 1000.0;
                        self.stats.decode_ms.lock().unwrap().push(decode_ms);
                        if let Some((what, bytes, spiked)) = big {
                            log::info!("{what} decoded in {decode_ms:.1} ms ({} KB, {}x{}{})", bytes / 1024, d.width, d.height, if spiked { ", clocks told ahead" } else { "" });
                        }
                        if ltr {
                            self.ltr_decoded.store(frame_id, Ordering::Relaxed);
                        }
                        let offset = live.clock.lock().unwrap().offset_us;
                        let host_now = now_us() as i64 + offset;
                        self.stats.latency_ms.lock().unwrap().push((host_now - capture_us as i64) as f32 / 1000.0);
                    }
                    self.stats.shown.fetch_add(1, Ordering::Relaxed);
                    self.stats.stale.fetch_add(shown.skipped.len() as u32, Ordering::Relaxed);
                    if let Some(at) = self.connect_at.lock().unwrap().take() {
                        log::info!(
                            "first picture {:.0} ms after the sign-in began (prewarm {})",
                            at.elapsed().as_secs_f64() * 1000.0,
                            if crate::tune::ab("prewarm", true) { "on" } else { "off" }
                        );
                    }
                    // (Not while parked: that picture went nowhere.)
                    let mut surface_at = self.surface_at.lock().unwrap();
                    if surface_at.is_some() && d.parked_for().is_none() {
                        if let Some((at, how)) = surface_at.take() {
                            log::info!("first picture on the new Surface {:.0} ms after it came ({how})", at.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("decoder output: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Input to the host, the periodic control messages, the stats line.
    fn control_loop(self: Arc<Self>, live: Arc<Live>) {
        crate::raise_thread(-10);
        let mut mode_sent = Instant::now() - Duration::from_secs(2);
        let mut slow_sent = Instant::now() - Duration::from_secs(10);
        let mut report = Instant::now();
        let (mut office_sent, mut paused_sent) = (None, None);
        let mut view_told = (0, 0);
        // The view's size, and since when (sent once it settles: each new
        // stream size costs the host a keyframe).
        let (mut view_seen, mut view_since) = ((0u32, 0u32), Instant::now());
        let mut guard = self.input.lock().unwrap();
        while !live.stop.load(Ordering::Relaxed) {
            guard.sender.ack(live.input_ack.load(Ordering::Relaxed));
            guard.send_due(&live, &self.stats);
            let timeout = if guard.sender.unsent().is_some() {
                1
            } else if guard.sender.has_pending() {
                5
            } else {
                250
            };
            drop(guard);

            let office = self.office.load(Ordering::Relaxed);
            let paused = self.paused.load(Ordering::Relaxed);
            if office_sent != Some(office) || paused_sent != Some(paused) || mode_sent.elapsed() >= Duration::from_secs(1) {
                live.send(Kind::Control, &Control::Mode { office, fast_text: true }.to_bytes());
                live.send(Kind::Control, &Control::Paused { paused }.to_bytes());
                if paused_sent == Some(true) && !paused {
                    live.send(Kind::Control, &Control::KeyframeRequest.to_bytes());
                }
                (office_sent, paused_sent, mode_sent) = (Some(office), Some(paused), Instant::now());
            }
            let view = *self.view.lock().unwrap();
            if view != view_seen {
                (view_seen, view_since) = (view, Instant::now());
            }
            let settled = view_since.elapsed() >= Duration::from_millis(800);
            if (view != view_told && settled) || slow_sent.elapsed() >= Duration::from_secs(3) {
                if settled && view.0 >= 320 && view.1 >= 200 {
                    live.send(Kind::Control, &Control::View { width: view.0.min(65535) as u16, height: view.1.min(65535) as u16 }.to_bytes());
                }
                let hz = self.refresh_hz.load(Ordering::Relaxed);
                if hz > 0 {
                    live.send(Kind::Control, &Control::Refresh { hz: hz.min(65535) as u16 }.to_bytes());
                }
                let cameras = capture::CAMERA_COUNT.load(Ordering::Relaxed).min(u8::MAX as u32) as u8;
                live.send(Kind::Control, &Control::Cameras { count: cameras }.to_bytes());
                if settled {
                    view_told = view;
                }
                slow_sent = Instant::now();
            }
            if report.elapsed() >= Duration::from_secs(1) {
                self.report(&live, report.elapsed().as_secs_f64());
                report = Instant::now();
            }

            guard = self.input.lock().unwrap();
            if !live.stop.load(Ordering::Relaxed) {
                guard = self.input_wake.wait_timeout(guard, Duration::from_millis(timeout)).unwrap().0;
            }
        }
    }

    fn report(&self, live: &Live, secs: f64) {
        let mut lat = std::mem::take(&mut *self.stats.latency_ms.lock().unwrap());
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p = |q: f64| lat.get(((lat.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
        let mut inp = std::mem::take(&mut *self.stats.input_ms.lock().unwrap());
        inp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let i = |q: f64| inp.get(((inp.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
        let mut dec = std::mem::take(&mut *self.stats.decode_ms.lock().unwrap());
        dec.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let d = |q: f64| dec.get(((dec.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
        let shown = self.stats.shown.swap(0, Ordering::Relaxed) as f64 / secs;
        let mbps = self.stats.bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / secs / 1e6;
        let lost = self.stats.lost.swap(0, Ordering::Relaxed);
        let rtt = live.clock.lock().unwrap().rtt_us as f64 / 1000.0;
        let (w, h) = *live.size.lock().unwrap();
        let rx_calls = self.stats.rx_calls.swap(0, Ordering::Relaxed) as f64 / secs;
        let rx_packets = self.stats.rx_packets.swap(0, Ordering::Relaxed) as f64 / secs;
        let stale = self.stats.stale.swap(0, Ordering::Relaxed);
        let line = format!(
            "{w}x{h} {}  {shown:.0} fps  latency {:.1} / {:.1} ms  decode {:.1} / {:.1} ms  input {:.2} / {:.2} ms\nrtt {rtt:.1} ms  {mbps:.1} / {} Mbps  fec {}%  lost {lost}  stale {stale}  ({} mode)\nrx {rx_packets:.0} pkt/s in {rx_calls:.0} calls/s",
            live.codec.name().to_uppercase(),
            p(0.5),
            p(0.95),
            d(0.5),
            d(0.95),
            i(0.5),
            i(0.95),
            live.bitrate_kbps.load(Ordering::Relaxed) / 1000,
            live.fec_permille.load(Ordering::Relaxed) / 10,
            if self.office.load(Ordering::Relaxed) { "office" } else { "game" },
        );
        let path = live.path.lock().unwrap().clone();
        let line = if path.is_empty() { line } else { format!("{line}  path: {path}") };
        let audio = crate::audio::LINE.lock().unwrap().clone();
        let line = if audio.is_empty() { line } else { format!("{line}
{audio}") };
        if shown > 0.0 {
            let kbps = live.bitrate_kbps.load(Ordering::Relaxed);
            let mut logged = self.stats.logged.lock().unwrap();
            let due = match *logged {
                None => true,
                Some((at, then)) => {
                    at.elapsed() >= LOG_EVERY || lost > 0 || (kbps as u64) * 4 < then as u64 * 3 || crate::tune::ab("perflog", false)
                }
            };
            if due {
                log::info!("perf: {}", line.replace('\n', "  "));
                *logged = Some((Instant::now(), kbps));
            }
        }
        // This second's numbers, for automatic resolution (the app asks).
        {
            let mut auto = self.stats.auto.lock().unwrap();
            let seq = auto.sample.map_or(0, |(n, _)| n) + 1;
            let sample = crate::autores::Sample {
                now_s: now_s(),
                received_bps: mbps * 1e6,
                target_bps: live.bitrate_kbps.load(Ordering::Relaxed) as f64 * 1000.0,
                latency_ms: p(0.5) as f64,
                rtt_ms: rtt,
                fps: live.fps.load(Ordering::Relaxed) as f64,
                shown_fps: shown,
                width: w,
                height: h,
            };
            auto.sample = Some((seq, sample));
        }
        *self.stats_line.lock().unwrap() = line;
    }

    /// Automatic resolution: the stream's scale (% of the screen) to use,
    /// given the one in use (`current`) and the mode — `current` unless the
    /// last second's numbers say to change (see `autores`). Each second's
    /// numbers count once, however often the app asks.
    /// (`debug.rdpio.autores=0`: never changes.)
    pub fn auto_scale(&self, current: u32, office: bool) -> u32 {
        if !crate::tune::ab("autores", true) {
            return current;
        }
        let mut auto = self.stats.auto.lock().unwrap();
        let Some((seq, sample)) = auto.sample else { return current };
        if seq == auto.used {
            return current;
        }
        auto.used = seq;
        match auto.res.step(&sample, current, office) {
            Some(d) => {
                log::info!(
                    "auto resolution: {}% ({:.1} Mbps, {:.3} bpp; was {}% at {:.3} bpp; {}x{} {:.0} fps, latency {:.0} ms, rtt {:.0} ms, {:.1} Mbps arrived, next up after {:.0} s clear)",
                    d.scale,
                    d.budget_bps / 1e6,
                    d.bpp,
                    d.from,
                    d.bpp_from,
                    sample.width,
                    sample.height,
                    sample.fps,
                    sample.latency_ms,
                    sample.rtt_ms,
                    sample.received_bps / 1e6,
                    auto.res.up_hold_s(),
                );
                d.scale
            }
            None => current,
        }
    }

    // ---- From the app ----

    pub fn close(&self) {
        self.quit.store(true, Ordering::Relaxed);
        self.input_wake.notify_all();
    }

    /// The Surface to decode onto (`None`: it's going away — returns once the
    /// decoder let go of it).
    ///
    /// The decoder is kept meanwhile, parked on a placeholder (the video
    /// thread moves it onto the next Surface): a new Surface comes with every
    /// upscaler switch and every return from the background, and a new
    /// decoder costs a component, a configure and a fresh keyframe.
    /// (`debug.rdpio.keepdecoder=0`: released here.)
    pub fn set_surface(&self, window: Option<NativeWindow>) {
        if window.is_none() {
            let mut slot = self.decoder.write().unwrap();
            let parked = keep_decoder()
                && slot.as_ref().is_some_and(|d| {
                    d.parked_for().is_some()
                        || match d.park() {
                            Ok(()) => {
                                log::info!("decoder parked (no Surface)");
                                true
                            }
                            Err(e) => {
                                log::info!("decoder not parked: {e}");
                                false
                            }
                        }
                });
            if !parked {
                if let Some(d) = slot.take() {
                    d.release();
                }
            }
        } else {
            *self.surface_at.lock().unwrap() = Some((Instant::now(), "new decoder"));
        }
        *self.surface.lock().unwrap() = window;
        self.surface_gen.fetch_add(1, Ordering::Relaxed);
        self.nudge_video();
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        self.input_wake.notify_one();
        self.nudge_video();
    }

    /// Wake the video thread now (it sleeps while there's nothing to do).
    fn nudge_video(&self) {
        if let Some(tx) = self.video_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Event::Lost { first: 1, last: 0 });
        }
    }

    pub fn set_office(&self, office: bool) {
        self.office.store(office, Ordering::Relaxed);
        self.input_wake.notify_one();
    }

    pub fn office(&self) -> bool {
        self.office.load(Ordering::Relaxed)
    }

    pub fn set_view(&self, w: u32, h: u32) {
        *self.view.lock().unwrap() = (w, h);
    }

    pub fn set_refresh(&self, hz: u32) {
        self.refresh_hz.store(hz, Ordering::Relaxed);
    }

    pub fn input(&self, events: impl IntoIterator<Item = InputEvent>) {
        let touch_ok = self.link.read().unwrap().as_ref().is_some_and(|l| l.touch);
        let mut st = self.input.lock().unwrap();
        for ev in events {
            if matches!(ev, InputEvent::Touch { .. }) && !touch_ok {
                continue;
            }
            for ev in st.held.apply(ev) {
                st.sender.push(ev);
            }
        }
        if st.sender.unsent().is_some() && st.waiting_since.is_none() {
            st.waiting_since = Some(Instant::now());
        }
        // Sent from here, on the app's thread, when it's due: no waking the
        // control thread for it first (a sleeping thread can take a
        // millisecond or more to run). The control thread still sends what's
        // coalesced, and resends. (`debug.rdpio.directinput=0`: it sends all.)
        if crate::tune::ab("directinput", true) {
            if let Some(live) = self.link.read().unwrap().as_ref() {
                st.send_due(live, &self.stats);
            }
        }
        drop(st);
        self.input_wake.notify_one();
    }

    /// Release everything held (the app lost focus: the key-ups go elsewhere).
    pub fn release_all(&self) {
        let mut st = self.input.lock().unwrap();
        for ev in st.held.release_all() {
            st.sender.push(ev);
        }
        drop(st);
        self.input_wake.notify_one();
    }

    pub fn send_clipboard(&self, text: String) {
        if let Some(live) = self.link.read().unwrap().as_ref() {
            live.reliable.send(&Message::ClipboardText(text));
        }
    }

    /// For the app: `state|width|height|cursor generation|clipboard
    /// generation|host takes touch (1/0)` then a newline and the stats.
    pub fn status(&self) -> String {
        let (size, touch) = self.link.read().unwrap().as_ref().map_or(((0, 0), false), |l| (*l.size.lock().unwrap(), l.touch));
        format!(
            "{}|{}|{}|{}|{}|{}\n{}",
            self.state.lock().unwrap(),
            size.0,
            size.1,
            self.cursor_gen.load(Ordering::Relaxed),
            self.clipboard_gen.load(Ordering::Relaxed),
            touch as u8,
            self.stats_line.lock().unwrap()
        )
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("can't find {host}: {e}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| format!("no address for {host}"))
}
