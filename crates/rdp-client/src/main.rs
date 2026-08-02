//! RDPiO — a from-scratch, GPU-accelerated RDP client.
//!
//! Usage:
//! ```text
//!   rdpio                         open the M0 window (Windows) / print usage
//!   rdpio --host HOST [--port N] [--user U] [--domain D] [--password P]
//!        [--insecure] [--drive PATH] [--multimon | --fullscreen]
//!        [--quality gaming|office|balanced | --gaming | --office]
//!                                connect, activate, and (on Windows) paint the
//!                                live desktop into a Direct3D 11 window
//! ```
//!
//! `--multimon` spans the remote desktop across every local monitor (a
//! borderless window covering the virtual screen); `--fullscreen` is borderless
//! fullscreen on the primary monitor; `--drive PATH` shares a local folder into
//! the session; `--insecure` accepts self-signed TLS certs; `--cpu-yuv` forces
//! CPU YUV→RGB if GPU-decoded colors look wrong; `--udp` enables the
//! experimental UDP side-band transport (falls back to TCP); `--udp-debug` adds
//! per-datagram RDP-UDP logging for capture/diagnostics; `--printer` redirects
//! the local default printer; `--clipboard-dir DIR` saves files copied in the
//! session to DIR; `--width N`/`--height N`/`--size WxH` set the desktop
//! resolution; `--legacy` forces Standard RDP Security; `--keyboard-layout ID`
//! and `--bpp 16|24|32` set the keyboard layout and color depth;
//! `--quality gaming|office|balanced` selects the latency/clarity preset.
//!
//! The `--host` path connects over TCP, runs the X.224 security negotiation
//! (advertising TLS/CredSSP, falling back to Standard RDP Security), completes
//! MCS activation, and then streams server graphics updates. On Windows the
//! decoded bitmap rectangles are painted to the window via D3D11; elsewhere they
//! are logged (headless), which keeps the whole protocol stack runnable in CI.

mod arm_broker;
mod congestion;
mod feed;
mod prompt;
mod gateway;
mod metrics;
mod rng;
mod session;
mod transport;
mod w365;
// W365/AVD Reverse Connect (RDSTLS over a TLS WebSocket) + its Windows-only UI
// (WebView2 sign-in / Cloud PC picker) and platform bits. These depend on the
// SChannel `tls` module and Windows COM, so they are Windows-only until the
// Linux TLS/auth backends land (see PORTING.md, Stages 2–3).
#[cfg(windows)]
mod rdstls_auth;
#[cfg(windows)]
mod rdstls_v3;
#[cfg(windows)]
mod net_listener;
#[cfg(windows)]
mod reverse_connect;
#[cfg(windows)]
mod cloud_pc_picker;
#[cfg(windows)]
mod websocket;
#[cfg(windows)]
mod webview_auth;
#[cfg(windows)]
mod token_cache;
#[cfg(windows)]
mod password_cache;

/// Resolve the account password the AVD/W365 RDSTLS v3 credential encrypts.
/// Precedence: explicit `--password` (cached for next time) → DPAPI-cached
/// password → hidden interactive prompt (then cached). Clear a stale one with
/// `--w365-relogin`.
#[cfg(windows)]
fn resolve_rdstls_password(account: &str, explicit: Option<&str>) -> String {
    if let Some(p) = explicit {
        password_cache::store(account, p);
        return p.to_string();
    }
    if let Some(p) = password_cache::load(account) {
        tracing::info!("using cached Cloud PC password (DPAPI-encrypted)");
        return p;
    }
    match prompt::read_password(&format!("Password for {account} (hidden, cached securely): ")) {
        Ok(p) => {
            if !p.is_empty() {
                password_cache::store(account, &p);
            }
            p
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read password from console");
            String::new()
        }
    }
}

#[cfg(not(windows))]
fn resolve_rdstls_password(_account: &str, explicit: Option<&str>) -> String {
    explicit.map(str::to_string).unwrap_or_default()
}

mod allocator;

#[global_allocator]
static GLOBAL_ALLOC: allocator::TrackingAllocator = allocator::TrackingAllocator;

use rdp_core::{ClientConfig, Credentials};

fn main() {
    // Must precede any window/monitor API so Win32 reports true physical geometry
    // on mixed-DPI multi-monitor setups (see `window::set_process_dpi_aware`).
    #[cfg(windows)]
    crate::window::set_process_dpi_aware();
    let args = Args::from_env();
    init_tracing(args.log_file.as_deref());
    // First line of every log: which build produced it. Field reports from a
    // stale rdpio.exe are indistinguishable from current ones without this.
    tracing::info!(build = env!("RDPIO_BUILD"), "rdpio starting");
    // Log the faulting module on any native crash (e.g. inside a hosted add-in),
    // so a hard crash is diagnosable instead of a silently-truncated log.
    #[cfg(windows)]
    crate::crash::install();

    // Diagnostic: offline-replay a captured EGFX stream (no server).
    if let Some(path) = args.replay_gfx.clone() {
        #[cfg(windows)]
        {
            if let Err(err) = win::replay_gfx(&path) {
                tracing::error!(error = %err, "gfx replay failed");
                std::process::exit(1);
            }
        }
        // Replay paints through the D3D11 backend, which only exists on Windows.
        #[cfg(not(windows))]
        {
            tracing::error!(path = %path, "--replay-gfx needs the Windows GPU backend");
            std::process::exit(1);
        }
        #[cfg(windows)]
        return;
    }

    if args.host.is_some() || args.w365 || args.feed.is_some() {
        // Windows opens a window and paints the live desktop. Other platforms
        // run the same protocol stack headless, logging decoded rectangles.
        #[cfg(windows)]
        let result = win::run_connected(&args).map_err(|e| e.to_string());
        #[cfg(not(windows))]
        let result = run_connect(&args).map_err(|e| e.to_string());

        if let Err(err) = result {
            tracing::error!(error = %err, "connection attempt failed");
            std::process::exit(1);
        }
        return;
    }

    #[cfg(windows)]
    {
        if let Err(err) = win::run() {
            tracing::error!(error = %err, "rdpio exited with an error");
            std::process::exit(1);
        }
    }

    #[cfg(not(windows))]
    {
        eprintln!(
            "rdpio: no --host given.\n\
             Connect + negotiate:  rdpio --host <server> [--user <u> --domain <d>]\n\
             The GUI window is Windows-only (build the full client with the MSVC toolchain)."
        );
        std::process::exit(2);
    }
}

/// Build a [`ClientConfig`] from the parsed command-line arguments.
/// Choose the RDPGFX caps to advertise from whether the local machine has a
/// working GPU H.264 decoder (and any CLI override). The client can't make the
/// server use the client's GPU, but it *can* steer the server's codec choice:
/// advertise AVC when we can decode it fast (GPU), and degrade to AVC420-only or
/// no-AVC (ClearCodec/planar) when a software decode would be the bottleneck.
///
/// The GPU probe builds and immediately drops a tiny DXVA decoder against the
/// renderer's device — the same path the per-surface decoders take later.
#[cfg(windows)]
fn gfx_caps_for(
    args: &Args,
    device: Option<
        &(
            windows::Win32::Graphics::Direct3D11::ID3D11Device,
            windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        ),
    >,
) -> Vec<(u32, u32)> {
    let gpu_h264 = || {
        let Some((dev, ctx)) = device else {
            tracing::info!("D3D12 backend selected; skipping GPU H.264 probe");
            return false;
        };
        match rdp_gpu::h264::H264GpuDecoder::new(64, 64, dev, ctx) {
            Ok(_) => {
                tracing::info!("local GPU H.264 decode available");
                true
            }
            Err(e) => {
                tracing::info!(error = %e, "no local GPU H.264 decode");
                false
            }
        }
    };
    caps_from_flags(args.no_avc, args.force_avc444, args.quality, gpu_h264)
}

/// Pure caps-selection policy, factored out of [`gfx_caps_for`] so it's testable
/// without a GPU device. `gpu_h264` is evaluated lazily — only the final
/// (non-gaming, no explicit override) branch probes the local decoder.
fn caps_from_flags(
    no_avc: bool,
    force_avc444: bool,
    quality: QualityPreset,
    gpu_h264: impl FnOnce() -> bool,
) -> Vec<(u32, u32)> {
    use rdp_graphics::egfx;
    if no_avc {
        tracing::info!("--no-avc: advertising ClearCodec/planar caps (AVC disabled)");
        return egfx::CAPS_NO_AVC.to_vec();
    }
    if force_avc444 {
        tracing::info!("--force-avc444: advertising full AVC444/AVC420 caps");
        return egfx::CAPS_FULL.to_vec();
    }
    // AVC444 is TWO H.264 streams: a main 4:2:0 view plus an auxiliary stream
    // carrying the chroma needed to reconstruct full 4:4:4. Asking for it roughly
    // DOUBLES the host's encode work and the bytes on the wire.
    //
    // Only the CPU decode path performs that reconstruction. The zero-copy DXVA
    // path decodes the main sub-stream and DISCARDS the auxiliary one (see
    // `decode_avc444`) — so on a machine with GPU H.264, which is the case this
    // client exists for, advertising AVC444 pays double for a picture identical
    // to AVC420. The old policy had this exactly inverted: it asked for AVC444
    // *because* a GPU was present, i.e. precisely when the extra stream would be
    // thrown away.
    //
    // So: ask for AVC444 only when something will consume it. Today nothing does
    // by default — the no-GPU case decodes on the CPU, where software-decoding a
    // second 1080p stream costs far more than 4:2:0 chroma is worth. `--force-avc`
    // remains for anyone who wants it.
    //
    // `quality` and `gpu_h264` stay in the signature deliberately: the moment the
    // GPU path learns to combine the aux stream into 4:4:4, `office` on a
    // GPU-capable client is exactly the case that should ask for AVC444 again.
    // Not probing also keeps a decoder instantiation off the startup path.
    let _ = (quality, gpu_h264);
    tracing::info!(
        "advertising AVC420-only caps (the AVC444 aux chroma stream is discarded by the \
         decode path, so asking for it would double host encode and bandwidth for the \
         same picture; --force-avc overrides)"
    );
    egfx::CAPS_AVC420_ONLY.to_vec()
}

/// Remote-desktop dimensions for client-side render-scaling: the native window
/// size scaled by `scale`, rounded to even (RDP needs even dimensions) and
/// clamped to RDP's 200..=8192 per-axis range. The window stays native; the
/// client GPU upscales this smaller desktop on present.
fn scaled_desktop_dims(win_w: u32, win_h: u32, scale: f32) -> (u32, u32) {
    let scale = scale.clamp(0.4, 1.0);
    let one = |v: u32| -> u32 {
        let s = ((v as f32) * scale).round() as u32;
        s.clamp(200, 8192) & !1
    };
    (one(win_w), one(win_h))
}

/// One per-monitor window placement: where the borderless window sits on the
/// physical screen, its offset within the *native* virtual desktop (the space
/// per-monitor windows emit input in), and the framebuffer slice it presents
/// (the scaled monitor rectangle under render-scale; the native one otherwise).
#[derive(Debug, Clone, Copy)]
struct MonitorPlacement {
    /// Window position on the physical screen (virtual-screen coordinates).
    screen: (i32, i32),
    /// Window (monitor) size in native pixels.
    size: (u32, u32),
    /// Monitor offset within the native virtual desktop, for input mapping.
    input_offset: (i32, i32),
    /// This monitor's framebuffer slice origin.
    src: (u32, u32),
    /// This monitor's framebuffer slice size.
    src_size: (u32, u32),
}

/// Scale a multi-monitor layout for client-side render-scale: the CS_MONITOR
/// defs to advertise, the scaled desktop bounding box, and each monitor's
/// slice rect (origin, size) within the scaled framebuffer.
///
/// Every edge goes through the same monotonic round-to-even map
/// (`round(edge·scale/2)·2`), so an edge shared by two monitors maps to the
/// same scaled coordinate — no 1-px gaps or overlaps at seams — the Windows
/// primary keeps its (0,0) top-left (virtual-screen coordinates pin it there,
/// and 0 maps to 0), and every monitor keeps even dimensions, which the
/// server's H.264 4:2:0 encoder prefers. The server normalizes the layout by
/// translating the bounding-box origin to desktop (0,0), which is exactly the
/// subtraction used for the slice origins here, so EGFX surface offsets land
/// on the same coordinates.
fn scale_monitor_layout(
    rects: &[rdp_pdu::gcc::VirtualScreenRect],
    scale: f32,
) -> (
    Vec<rdp_pdu::gcc::MonitorDef>,
    (u32, u32),
    Vec<((u32, u32), (u32, u32))>,
) {
    let scale = scale.clamp(0.4, 1.0) as f64;
    let e = |v: i32| -> i32 { (((v as f64) * scale / 2.0).round() as i32) * 2 };
    let scaled: Vec<(i32, i32, i32, i32, bool)> = rects
        .iter()
        .map(|r| (e(r.left), e(r.top), e(r.right), e(r.bottom), r.primary))
        .collect();
    let min_x = scaled.iter().map(|r| r.0).min().unwrap_or(0);
    let min_y = scaled.iter().map(|r| r.1).min().unwrap_or(0);
    let max_x = scaled.iter().map(|r| r.2).max().unwrap_or(0);
    let max_y = scaled.iter().map(|r| r.3).max().unwrap_or(0);
    let defs = scaled
        .iter()
        .map(|&(l, t, r, b, primary)| rdp_pdu::gcc::MonitorDef {
            left: l,
            top: t,
            right: r - 1,
            bottom: b - 1,
            primary,
        })
        .collect();
    let slices = scaled
        .iter()
        .map(|&(l, t, r, b, _)| {
            (
                ((l - min_x) as u32, (t - min_y) as u32),
                ((r - l).max(0) as u32, (b - t).max(0) as u32),
            )
        })
        .collect();
    let size = ((max_x - min_x).max(0) as u32, (max_y - min_y).max(0) as u32);
    (defs, size, slices)
}

/// Exponential backoff with jitter for auto-reconnect retries.
/// attempt 1 = ~500 ms, attempt 2 = ~1 s, doubling up to a 30 s cap.
fn reconnect_delay(attempt: u32) -> std::time::Duration {
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);
    const BASE: std::time::Duration = std::time::Duration::from_millis(500);
    let exp = BASE * 2u32.pow((attempt - 1).min(6));
    let base = exp.min(MAX_DELAY);
    // Add up to 25% jitter using the OS CSPRNG; fall back to zero jitter if RNG fails.
    let mut jitter_buf = [0u8; 1];
    let jitter_ms = if crate::rng::fill(&mut jitter_buf) {
        (jitter_buf[0] as u64 * base.as_millis() as u64 / 1024).min(base.as_millis() as u64 / 4)
    } else {
        0
    };
    base + std::time::Duration::from_millis(jitter_ms)
}

/// Path to the persisted reconnect cookie for a given hostname. The cookie is
/// host-specific so a new connection to a different host doesn't accidentally
/// reuse stale state.
fn reconnect_cookie_path(hostname: &str) -> std::path::PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    hostname.hash(&mut h);
    let file = format!("rdpio_reconnect_{:016x}.cookie", h.finish());
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let dir = std::path::PathBuf::from(local).join("rdpio");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(file)
    } else {
        std::env::temp_dir().join(file)
    }
}

fn save_reconnect_cookie(
    hostname: &str,
    cookie: &rdp_pdu::logon::ReconnectCookie,
) -> std::io::Result<()> {
    let path = reconnect_cookie_path(hostname);
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&cookie.logon_id.to_le_bytes());
    buf.extend_from_slice(&cookie.arc_random);
    std::fs::write(path, buf)
}

fn load_reconnect_cookie(hostname: &str) -> Option<rdp_pdu::logon::ReconnectCookie> {
    let path = reconnect_cookie_path(hostname);
    let data = std::fs::read(path).ok()?;
    if data.len() != 20 {
        return None;
    }
    let logon_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let mut arc_random = [0u8; 16];
    arc_random.copy_from_slice(&data[4..20]);
    Some(rdp_pdu::logon::ReconnectCookie {
        logon_id,
        arc_random,
    })
}

fn config_from_args(args: &Args) -> ClientConfig {
    // Split a Windows-style logon name (`DOMAIN\user`, `.\user`, UPN) into the
    // separate domain/user fields RDP needs — passing `.\user` through verbatim
    // makes the server reject it as an unknown account (STATUS_LOGON_FAILURE).
    let (domain, username) = rdp_core::split_domain_user(
        &args.domain.clone().unwrap_or_default(),
        &args.user.clone().unwrap_or_default(),
    );
    tracing::debug!(%domain, %username, "resolved logon identity");
    let mut config = ClientConfig {
        hostname: args.host.clone().unwrap_or_default(),
        port: args.port,
        credentials: Credentials {
            domain,
            username,
            password: args.password.clone().unwrap_or_default(),
        },
        allow_invalid_certificate: args.insecure,
        drive_paths: crate::expand_drive_args(&args.drive),
        ..Default::default()
    };
    // Apply a requested resolution, clamped to RDP's range and rounded to an
    // even width/height (multimon/fullscreen override this from the monitors).
    let sane = |v: u16| (v.clamp(200, 8192)) & !1;
    if let Some(w) = args.width {
        config.width = sane(w);
    }
    if let Some(h) = args.height {
        config.height = sane(h);
    }
    config.force_legacy = args.legacy;
    config.keyboard_layout = args.keyboard_layout;
    config.color_depth = args.bpp;
    config.shortpath = args.shortpath;
    config
}

/// Headless connect path (non-Windows): negotiate, activate, and log decoded
/// bitmap rectangles. Exercises the entire protocol stack without a GPU.
#[cfg(not(windows))]
fn run_connect(args: &Args) -> Result<(), transport::NegotiateError> {
    use rdp_pdu::x224::SecurityProtocol;

    let config = config_from_args(args);

    tracing::info!(host = %config.hostname, port = config.port, "connecting over TCP");
    let (mut stream, _connector, protocol) = transport::connect(&config)?;
    tracing::info!(?protocol, "X.224 negotiation complete");

    // A read timeout prevents hangs against a silent server (set before the TLS
    // handshake, which also does I/O; it persists on the moved socket).
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    // Enhanced RDP Security (SSL) and NLA (HYBRID) both run inside a TLS tunnel;
    // Standard RDP Security runs directly over the socket.
    if protocol.contains(SecurityProtocol::SSL) || protocol.contains(SecurityProtocol::HYBRID) {
        let mut tls =
            match tls::TlsStream::connect(stream, &config.hostname, config.allow_invalid_certificate)
            {
                Ok(tls) => {
                    tracing::info!("TLS established (rustls)");
                    tls
                }
                Err(err) => {
                    tracing::warn!(error = %err, "TLS handshake failed");
                    return Ok(());
                }
            };

        if protocol.contains(SecurityProtocol::HYBRID) {
            // NLA/CredSSP (MS-CSSP) authenticates over the TLS channel — binding to
            // the server certificate's public key — before the MCS connection.
            let cert = match tls.remote_cert_der() {
                Some(cert) => cert,
                None => {
                    tracing::error!("no server certificate available for NLA channel binding");
                    return Ok(());
                }
            };
            let spn = format!("TERMSRV/{}", config.hostname);
            let creds = &config.credentials;
            match rdp_nla::credssp::authenticate(
                &mut tls,
                &spn,
                &cert,
                &creds.domain,
                &creds.username,
                &creds.password,
            ) {
                Ok(()) => tracing::info!("NLA/CredSSP complete"),
                Err(err) => {
                    tracing::error!(error = %err, "NLA/CredSSP failed");
                    return Ok(());
                }
            }
        }

        headless_run(&mut tls, &config, protocol);
    } else {
        // Standard RDP Security (no TLS): run directly over the socket.
        headless_run(&mut stream, &config, protocol);
    }
    Ok(())
}

/// Activate and run a headless session over any `Read + Write` transport, logging
/// decoded rectangles. Shared by the plaintext and rustls-TLS paths.
#[cfg(not(windows))]
fn headless_run<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    config: &rdp_core::ClientConfig,
    protocol: rdp_pdu::x224::SecurityProtocol,
) {
    match session::activate(stream, config, protocol, None) {
        Ok(mut active) => {
            tracing::info!(info = ?active.info(), "RDP session ACTIVE");
            let mut sink = LogSink::default();
            if let Err(err) = session::run_session(stream, &mut active, &mut sink) {
                tracing::info!(error = %err, "session ended");
            }
        }
        Err(err) => tracing::warn!(error = %err, "activation stopped"),
    }
}

/// Headless frame sink: logs decoded bitmap rectangles (non-Windows builds).
#[cfg(not(windows))]
#[derive(Default)]
struct LogSink {
    rects: u64,
}

#[cfg(not(windows))]
impl session::FrameSink for LogSink {
    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
        self.rects += 1;
        tracing::debug!(x, y, w, h, bytes = rgba.len(), "paint rect");
    }

    fn present(&mut self) {
        tracing::info!(painted_rects = self.rects, "frame presented");
    }

    fn cursor(&mut self, update: session::CursorUpdate) {
        match update {
            session::CursorUpdate::Hide => tracing::debug!("cursor update: hide"),
            session::CursorUpdate::Default => tracing::debug!("cursor update: default arrow"),
            session::CursorUpdate::Shape {
                width,
                height,
                hot_x,
                hot_y,
                rgba,
            } => tracing::debug!(
                width,
                height,
                hot_x,
                hot_y,
                bytes = rgba.len(),
                "cursor update: shape"
            ),
        }
    }
}

/// Quality preset for the latency/clarity trade-offs the client controls.
///
/// NOTE: all presets advertise the same AVC420-only EGFX caps (see
/// `gfx_caps_for` — the AVC444 aux chroma is discarded on the GPU decode path,
/// so advertising it buys double host encode for the same picture). What the
/// presets change is presentation: office pins vsync, 1:1 rendering and the
/// bicubic upscaler; gaming permits render-scale and motion-first choices.
/// Full 4:4:4 remains available explicitly via `--force-avc444`, whose chroma
/// reconstruction runs on the CPU decode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualityPreset {
    /// Motion-first: render-scale friendly, upscaler tuned for game imagery.
    Gaming,
    /// Clarity-first: no render-scale, smooth vsync, bicubic.
    Office,
    /// The defaults (identical codec caps; see the enum docs).
    Balanced,
}

impl Default for QualityPreset {
    fn default() -> Self {
        QualityPreset::Balanced
    }
}

/// Minimal command-line arguments (no external arg-parsing dependency).
#[derive(Debug)]
struct Args {
    host: Option<String>,
    port: u16,
    user: Option<String>,
    domain: Option<String>,
    password: Option<String>,
    /// Accept self-signed / untrusted TLS server certificates (`--insecure`).
    insecure: bool,
    /// Local directories/drive roots to share as redirected drives (`--drive
    /// PATH`, repeatable; `--drive all` = every mounted drive letter, mapped
    /// network drives included).
    drive: Vec<String>,
    /// Span the remote desktop across all local monitors (`--multimon`).
    multimon: bool,
    /// Borderless fullscreen on the primary monitor (`--fullscreen`).
    fullscreen: bool,
    /// Force CPU YUV→RGB conversion instead of the GPU video processor
    /// (`--cpu-yuv`) — a safety valve if GPU-decoded colors look wrong.
    cpu_yuv: bool,
    /// Enable the experimental UDP side-band transport (`--udp`) when the server
    /// requests multitransport. Falls back to TCP on any failure. Off by default.
    udp: bool,
    /// After connecting over Reverse Connect, attempt a direct UDP 3390
    /// Shortpath tunnel to the Cloud PC (`--shortpath`).
    shortpath: bool,
    /// Redirect the local default printer into the session (`--printer`).
    printer: bool,
    /// Log decoded RDP-UDP datagrams for capture/diagnostics (`--udp-debug`).
    udp_debug: bool,
    /// Save files copied in the session to this local dir (`--clipboard-dir`).
    clipboard_dir: Option<String>,
    /// Requested desktop width/height (`--width`/`--height`/`--size WxH`).
    /// Ignored under `--multimon`/`--fullscreen`, which derive size from the
    /// monitors.
    width: Option<u16>,
    height: Option<u16>,
    /// Force legacy Standard RDP Security, skipping TLS/NLA (`--legacy`).
    legacy: bool,
    /// Keyboard layout id (`--keyboard-layout`, hex `0x409` or decimal).
    keyboard_layout: Option<u32>,
    /// Session color depth in bits (`--bpp 16|24|32`).
    bpp: Option<u16>,
    /// Present with tearing (no vsync) for absolute-minimum latency
    /// (`--low-latency` only — no preset implies it). Default off → smooth
    /// vsync for desktop work.
    low_latency: bool,
    /// Quality preset that selects the best codec/path for the workload
    /// (`--quality gaming|office|balanced`, aliases `--gaming`, `--office`).
    /// `gaming` = low latency, AVC420-only, render-scale friendly. `office` =
    /// clarity-first, full AVC444 when the local GPU can decode it, no render-scale.
    /// `balanced` (default) = probe the GPU and fall back sensibly.
    quality: QualityPreset,
    /// Opt back into full AVC444 (4:4:4) caps (`--force-avc444`, alias
    /// `--force-avc`). Otherwise `--gaming` advertises AVC420-only, and a non-gaming
    /// session falls back to the GPU-probe degrade — so this also forces full AVC
    /// when the local GPU H.264 probe fails. Overrides the auto degrade.
    force_avc444: bool,
    /// Advertise no-AVC caps so the server uses ClearCodec/planar instead of
    /// H.264 (`--no-avc`). Useful on hosts where software H.264 is the bottleneck.
    no_avc: bool,
    /// Render the remote desktop at this fraction of the window size and upscale
    /// on the client GPU (`--render-scale F` / `--scale F`, clamped 0.4..=1.0).
    /// Fewer pixels for a CPU-only host to encode → higher frame rate; the RTX
    /// client upscales to native on present. Default 1.0 (no scaling).
    render_scale: f32,
    /// Per-monitor windows (`--per-monitor` / `--multimon-windows`): span the
    /// remote desktop across all monitors (like `--multimon`) but present one
    /// borderless window per physical monitor, each showing its slice, so remote
    /// windows respect monitor boundaries and drag naturally across the seam.
    per_monitor: bool,
    /// Diagnostic (`--no-seed`): decode every ClearCodec tile from black instead
    /// of seeding it with the persistent surface's prior pixels. Used to isolate
    /// whether artifacts come from the seed/persistence path (if so, they change
    /// character with this on) vs the decode/copy path (unchanged).
    no_seed: bool,
    /// Opt-in client frame-pacing (`--pace <fps>` / `--smooth <fps>`, 0 = off,
    /// default off). Presents at most this many frames/sec on an even cadence,
    /// always showing the newest decoded frame — trades a few ms of latency to
    /// smooth uneven frame arrival (motion jitter). Off by default (latency-first).
    pace: u16,
    /// GPU upscaler used when `--render-scale` (or a window larger than the remote
    /// desktop) means the client scales the desktop up on present (`--upscale
    /// vsr|bicubic|fsr|nearest|bilinear`, alias `catmull`/`easu`/`integer`/`none`;
    /// `--vsr` = `--upscale vsr`). Default Catmull-Rom bicubic — sharp without the
    /// text/UI ringing AI video SR causes on non-video content. `fsr` (AMD
    /// FidelityFX SR 1.0, any GPU) reconstructs game imagery best; `vsr` engages
    /// the driver AI SR (NVIDIA RTX VSR / Intel VPE SR) for full-screen video.
    upscale: rdp_gpu::Upscaler,
    /// RCAS adaptive-sharpen strength after the upscale (`--sharpen 0.0..=1.0`).
    /// `None` = default: on (0.9) for `--upscale fsr` — FSR 1.0 is designed as
    /// EASU+RCAS — off for everything else. `Some(0.0)` disables even for fsr.
    sharpen: Option<f32>,
    /// GPU backend (`--backend d3d11|d3d12`). Default D3D11; D3D12 is experimental
    /// and enables the compute-shader YUV→RGB path.
    backend: rdp_gpu::Backend,
    /// Diagnostic (`--replay-gfx <gfx_<pid>.bin>`): offline-replay an
    /// `RDPIO_DUMP_GFX` capture through the real renderer (no server) and write
    /// the composited shadow to BMP, to reproduce pipeline corruption.
    replay_gfx: Option<String>,
    /// Write logs to this file instead of stdout (`--log-file PATH`). The GUI
    /// app's stdout redirection is buffering-dependent and often comes back
    /// empty; a file writer is flushed per line, so it always captures.
    log_file: Option<String>,
    /// RDWeb / W365 feed URL (`--feed URL`). Discovers hosts and RDP settings
    /// from an XML or JSON feed instead of a static `--host`.
    feed: Option<String>,
    /// Use Windows 365 / AVD modern authentication (OAuth2 device-code flow)
    /// (`--w365`). The resulting access token is passed as the logon password.
    w365: bool,
    /// Fall back to the terminal/device-code login prompt instead of the WebView2
    /// login panel (`--w365-device-code`). Useful when WebView2 is unavailable.
    w365_device_code: bool,
    /// Force a fresh W365 sign-in (`--w365-relogin`), discarding the cached
    /// refresh token. Use this to switch accounts or recover from a bad cache.
    w365_relogin: bool,
    /// Discard only the cached RDSTLS logon password (`--forget-password`) and
    /// re-prompt, without clearing the OAuth token (so no re-MFA). Use after a
    /// mistyped or changed account password.
    forget_password: bool,
    /// Microsoft tenant id for `--w365` (`--tenant ID`). Defaults to `common`.
    tenant: Option<String>,
    /// OAuth2 client id for `--w365` (`--client-id ID`). Overrides the default
    /// Remote Desktop / Windows Virtual Desktop application id.
    client_id: Option<String>,
    /// Path to a W365/AVD `.rdp` connection file (`--rdp-file PATH`). With
    /// `--w365`, its `gatewayhostname` + `loadbalanceinfo` drive the ARM Reverse
    /// Connect brokering instead of the JSON feed. Useful for replaying a file
    /// the Windows App generated.
    rdp_file: Option<String>,
    /// Host Microsoft's Teams WebRTC redirector add-in (`--teams`, alias
    /// `--webrtc`) so Teams reaches the "Optimized" state — A/V runs peer-to-peer
    /// from this client instead of through the RDP graphics stream. Windows only;
    /// no-ops (rdpio keeps declining the WebRTC DVC) if the add-in DLL is absent.
    teams: bool,
    /// Reach Teams "Optimized" via rdpio's *own* native WebRTC engine
    /// (`--teams-native`, alias `--webrtc-native`) instead of hosting the DLL —
    /// speaks the same `com.microsoft.rdc.dvc.webrtc.1` protocol over `webrtc-rs`,
    /// needs no Microsoft binary, and is the path that will run on Linux. Takes
    /// precedence over `--teams` when both are set.
    teams_native: bool,
}

impl Args {
    fn from_env() -> Self {
        let mut args = Args {
            host: None,
            port: 3389,
            user: None,
            domain: None,
            password: None,
            insecure: false,
            drive: Vec::new(),
            multimon: false,
            fullscreen: false,
            cpu_yuv: false,
            udp: false,
            shortpath: false,
            printer: false,
            udp_debug: false,
            clipboard_dir: None,
            width: None,
            height: None,
            legacy: false,
            keyboard_layout: None,
            bpp: None,
            low_latency: false,
            quality: QualityPreset::default(),
            force_avc444: false,
            no_avc: false,
            render_scale: 1.0,
            per_monitor: false,
            no_seed: false,
            pace: 0,
            upscale: rdp_gpu::Upscaler::default(),
            sharpen: None,
            backend: rdp_gpu::Backend::default(),
            replay_gfx: None,
            log_file: None,
            feed: None,
            w365: false,
            w365_device_code: false,
            w365_relogin: false,
            forget_password: false,
            tenant: None,
            client_id: None,
            rdp_file: None,
            teams: false,
            teams_native: false,
        };
        // Preset defaults must not silently override something the user asked for
        // by name, so track which of the presettable knobs were set explicitly.
        let mut low_latency_explicit = false;
        let mut render_scale_explicit = false;
        let mut upscale_explicit = false;
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            match flag.as_str() {
                // Note: `-h` is --host (kept for compatibility), so help takes
                // the long form plus the Windows-conventional `/?`.
                "--help" | "--usage" | "-?" | "/?" => {
                    print_help();
                    std::process::exit(0);
                }
                "--version" | "-V" => {
                    println!("rdpio {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--host" | "-h" => args.host = it.next(),
                "--log-file" | "--log" => args.log_file = it.next(),
                "--replay-gfx" => args.replay_gfx = it.next(),
                "--port" => {
                    args.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.port);
                }
                "--user" | "-u" => args.user = it.next(),
                "--domain" | "-d" => args.domain = it.next(),
                "--password" | "-p" => args.password = it.next(),
                "--insecure" | "-k" => args.insecure = true,
                "--drive" | "-D" => {
                    if let Some(v) = it.next() {
                        args.drive.push(v);
                    }
                }
                "--multimon" | "-m" => args.multimon = true,
                "--fullscreen" | "-f" => args.fullscreen = true,
                "--cpu-yuv" => args.cpu_yuv = true,
                "--teams" | "--webrtc" => args.teams = true,
                "--teams-native" | "--webrtc-native" => args.teams_native = true,
                "--low-latency" => {
                    args.low_latency = true;
                    low_latency_explicit = true;
                }
                // Deliberately does NOT imply --low-latency: tearing presents are
                // an artifact trade-off the user must opt into by name.
                "--gaming" => args.quality = QualityPreset::Gaming,
                "--office" => args.quality = QualityPreset::Office,
                "--quality" => {
                    if let Some(v) = it.next() {
                        args.quality = match v.trim().to_ascii_lowercase().as_str() {
                            "gaming" => QualityPreset::Gaming,
                            "office" => QualityPreset::Office,
                            "balanced" => QualityPreset::Balanced,
                            other => {
                                tracing::warn!("unknown --quality mode {other:?}; using balanced");
                                QualityPreset::default()
                            }
                        };
                    }
                }
                "--force-avc" | "--force-avc444" => args.force_avc444 = true,
                "--no-avc" => args.no_avc = true,
                "--render-scale" | "--scale" => {
                    if let Some(v) = it.next().and_then(|v| v.trim().parse::<f32>().ok()) {
                        args.render_scale = v.clamp(0.4, 1.0);
                        render_scale_explicit = true;
                    }
                }
                "--per-monitor" | "--multimon-windows" => args.per_monitor = true,
                "--no-seed" => args.no_seed = true,
                "--pace" | "--smooth" => {
                    args.pace = it
                        .next()
                        .and_then(|v| v.trim().parse::<u16>().ok())
                        .map(|v| v.min(480))
                        .unwrap_or(0);
                }
                "--upscale" | "--upscaler" => {
                    if let Some(v) = it.next() {
                        args.upscale = parse_upscaler(&v);
                        upscale_explicit = true;
                    }
                }
                "--vsr" => {
                    args.upscale = rdp_gpu::Upscaler::Vsr;
                    upscale_explicit = true;
                }
                "--no-vsr" => {
                    args.upscale = rdp_gpu::Upscaler::Bilinear;
                    upscale_explicit = true;
                }
                "--fsr" => {
                    args.upscale = rdp_gpu::Upscaler::Fsr;
                    upscale_explicit = true;
                }
                "--sharpen" => {
                    if let Some(v) = it.next().and_then(|v| v.trim().parse::<f32>().ok()) {
                        args.sharpen = Some(v.clamp(0.0, 1.0));
                    }
                }
                "--backend" => {
                    if let Some(v) = it.next() {
                        args.backend = parse_backend(&v);
                    }
                }
                "--udp" => args.udp = true,
                "--shortpath" => args.shortpath = true,
                "--udp-debug" => {
                    args.udp = true;
                    args.udp_debug = true;
                }
                "--printer" => args.printer = true,
                "--clipboard-dir" => args.clipboard_dir = it.next(),
                "--width" => args.width = it.next().and_then(|v| v.parse().ok()),
                "--height" => args.height = it.next().and_then(|v| v.parse().ok()),
                "--legacy" => args.legacy = true,
                "--keyboard-layout" => {
                    args.keyboard_layout = it.next().and_then(|v| {
                        let v = v.trim();
                        v.strip_prefix("0x")
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                            .or_else(|| v.parse().ok())
                    });
                }
                "--bpp" => args.bpp = it.next().and_then(|v| v.parse().ok()),
                "--feed" => args.feed = it.next(),
                "--w365" => args.w365 = true,
                "--w365-device-code" => args.w365_device_code = true,
                "--w365-relogin" | "--w365-logout" => args.w365_relogin = true,
                "--forget-password" => args.forget_password = true,
                "--tenant" => args.tenant = it.next(),
                "--client-id" => args.client_id = it.next(),
                "--rdp-file" => args.rdp_file = it.next(),
                "--size" => {
                    // "WxH" (or "W,H") → width + height.
                    if let Some(v) = it.next() {
                        let mut parts = v.split(['x', 'X', ',']);
                        args.width = parts.next().and_then(|s| s.trim().parse().ok());
                        args.height = parts.next().and_then(|s| s.trim().parse().ok());
                    }
                }
                other => tracing::warn!("ignoring unknown argument: {other}"),
            }
        }
        // `office` is the text-reading preset, and the three settings below are
        // what actually decide whether text is crisp — so the preset has to own
        // them, not just the advertised codec caps. Each yields to an explicit
        // flag, so `--office --render-scale 0.8` still scales.
        //
        // - vsync, because tearing is at its most visible on scrolling text: the
        //   display shows part of one frame and part of the next, so a line of
        //   text sits offset by the scroll distance until the next present.
        // - 1:1 rendering, because detail lost to downscaling cannot be restored
        //   by any upscaler; glyph stems are exactly the high-frequency content
        //   that goes first.
        // - bicubic, because FSR's edge reconstruction plus RCAS sharpening is
        //   tuned for rendered game frames and rings on glyph edges.
        if args.quality == QualityPreset::Office {
            if !low_latency_explicit {
                args.low_latency = false;
            }
            if !render_scale_explicit {
                args.render_scale = 1.0;
            }
            if !upscale_explicit {
                args.upscale = rdp_gpu::Upscaler::Bicubic;
            }
        }
        args
    }
}

/// Print the full option reference for `--help`.
///
/// Written out rather than generated so each flag can carry the one line of
/// context that actually decides whether you want it — the defaults and the
/// trade-offs are the part that isn't guessable from the name.
fn print_help() {
    println!(
        r#"rdpio {version} — GPU-accelerated RDP client

USAGE
    rdpio --host <ADDR> [-u <USER>] [-p <PASSWORD>] [OPTIONS]

    rdpio --host 10.0.0.5 -u alice -p secret -k
    rdpio --host server.corp --fullscreen --drive all --quality office
    rdpio --w365                       # Windows 365 / Cloud PC via Entra sign-in

CONNECTION
    --host, -h <ADDR>      Server host name or IP. (-h is HOST, not help.)
    --port <PORT>          TCP port. Default 3389.
    --insecure, -k         Accept a self-signed / untrusted TLS certificate.
                           RDP hosts almost always have one, so this is usually
                           required. The session stays encrypted, but the
                           certificate cannot prove you reached the right host.
    --legacy               Use Standard RDP Security (RC4) instead of TLS/NLA,
                           for hosts with Network Level Authentication disabled.
    --udp                  Also open the UDP transport (MS-RDPEUDP) for graphics.
    --shortpath            Azure Virtual Desktop RDP Shortpath (implies --udp).

AUTHENTICATION
    --user, -u <NAME>      User name.
    --password, -p <PASS>  Password. Visible in shell history — prefer omitting
                           it and letting the client prompt.
    --domain, -d <DOMAIN>  Logon domain.
    --forget-password      Clear the saved credential for this host and exit.

DISPLAY
    --fullscreen, -f       Borderless fullscreen on the current monitor.
    --multimon, -m         Span every monitor as one desktop.
    --per-monitor          One window per monitor instead of one spanning window.
    --width <PX>           Desktop width  (default: the window's client area).
    --height <PX>          Desktop height.
    --size <WxH>           Both at once, e.g. --size 2560x1440.
    --bpp <BITS>           Colour depth: 15, 16, 24 or 32. Default 32.
    --keyboard-layout <ID> Layout ID, decimal or 0x-hex (e.g. 0x0409 = US).

IMAGE QUALITY AND SCALING
    --quality <MODE>       gaming | office | balanced. Default balanced.
                           gaming  = AVC420-only, best for motion and video.
                           office  = clarity first, best for reading text:
                                     vsync, 1:1 rendering and bicubic upscale,
                                     each overridable by naming it explicitly.
    --gaming               Shorthand for --quality gaming. Does not change the
                           present mode; add --low-latency if you want tearing
                           presents too (lowest latency, visible shear).
    --office               Shorthand for --quality office.
    --render-scale <F>     Render the remote desktop at 0.4-1.0 of window size
                           and upscale on the GPU. Less bandwidth and less host
                           encode cost. Good for video and motion; bad for text,
                           since glyph detail lost below 1.0 cannot be restored
                           by any upscaler.
    --upscale <MODE>       How a scaled desktop is enlarged:
                             bicubic  Catmull-Rom, sharp on text (default)
                             fsr      AMD FSR 1.0 (EASU+RCAS), best all-round
                             vsr      Vendor AI super-resolution
                                      (NVIDIA RTX VSR / Intel VPE SR)
                             nearest  Point sampling, exact pixels
                             bilinear Driver default, softest
    --fsr / --vsr          Shorthand for the matching --upscale mode.
    --sharpen <0.0-1.0>    RCAS sharpening after upscale. Defaults to 0.9 with
                           --fsr, off otherwise.
    --force-avc444         Advertise full 4:4:4 chroma. The extra chroma stream
                           is only reconstructed on the CPU decode path — the
                           GPU (DXVA) path renders its 4:2:0 main view and
                           logs that the aux stream was discarded — so expect
                           double host encode for a benefit only off-GPU.
    --no-avc               Disable H.264 entirely (RemoteFX / progressive only).
    --cpu-yuv              Skip GPU colour conversion. Diagnostic escape hatch.
    --backend <API>        d3d11 (default) or d3d12.

LATENCY
    --low-latency          Present with tearing allowed instead of waiting for
                           vsync. Lower lag, possible tearing.
    --pace <HZ>            Cap presents to this rate (0 = uncapped, max 480).
                           Useful to stop a 165 Hz panel burning battery.

REDIRECTION
    --drive, -D <PATH>     Share a local folder or drive into the session.
                           Repeatable. `--drive all` shares every mounted drive.
    --printer              Redirect the default Windows printer.
    --clipboard-dir <DIR>  Where pasted files from the session are staged.
                           Default: a per-session folder under %TEMP%.
    --teams-native         Native WebRTC engine for Teams media optimization.
    --teams                Teams optimization via the Microsoft add-in DLL.

WINDOWS 365 / CLOUD PC
    --w365                 Sign in to Windows 365 and connect to a Cloud PC.
    --w365-device-code     Use device-code sign-in (no local browser).
    --w365-relogin         Force a fresh sign-in, discarding cached tokens.
    --tenant <ID>          Entra tenant ID.
    --client-id <ID>       Override the OAuth client ID.
    --feed <URL>           Connect through a specific workspace feed URL.
    --rdp-file <PATH>      Read connection settings from an .rdp file.

DIAGNOSTICS
    --log-file <PATH>      Write the log to a file as well as the console.
    --replay-gfx <PATH>    Replay a captured EGFX stream offline (no server).
    --udp-debug            Verbose UDP transport tracing (implies --udp).
    --no-seed              Skip the initial full-desktop refresh request.
    --help, -?             Show this help.
    --version, -V          Show the version.

    Set RUST_LOG=debug (or trace) for per-PDU detail. At INFO the client prints
    a periodic `metrics:` line with decode/present percentiles and frame rate.

Requires a CPU with AVX2 (Intel Haswell / AMD Excavator, 2013 or newer)."#,
        version = env!("CARGO_PKG_VERSION"),
    );
}

fn parse_backend(v: &str) -> rdp_gpu::Backend {
    match v.trim().to_ascii_lowercase().as_str() {
        "d3d12" | "dx12" | "12" => rdp_gpu::Backend::D3D12,
        "d3d11" | "dx11" | "11" | _ => {
            if !matches!(v.trim().to_ascii_lowercase().as_str(), "d3d11" | "dx11" | "11") {
                tracing::warn!("unknown --backend mode {v:?}; using d3d11");
            }
            rdp_gpu::Backend::D3D11
        }
    }
}

/// Map a `--upscale` value to an [`Upscaler`](rdp_gpu::Upscaler). Case-insensitive;
/// unknown values fall back to the default (Catmull-Rom bicubic) with a warning.
fn parse_upscaler(v: &str) -> rdp_gpu::Upscaler {
    match v.trim().to_ascii_lowercase().as_str() {
        "vsr" | "rtx" | "superres" | "ai" => rdp_gpu::Upscaler::Vsr,
        "bicubic" | "catmull" | "catmull-rom" => rdp_gpu::Upscaler::Bicubic,
        "fsr" | "fsr1" | "easu" => rdp_gpu::Upscaler::Fsr,
        "nearest" | "integer" | "point" | "pixel" => rdp_gpu::Upscaler::Nearest,
        "bilinear" | "linear" | "none" => rdp_gpu::Upscaler::Bilinear,
        other => {
            tracing::warn!("unknown --upscale mode {other:?}; using bicubic");
            rdp_gpu::Upscaler::Bicubic
        }
    }
}

/// Expand `--drive` values into concrete roots: `all` becomes every mounted
/// drive letter — fixed, removable, and mapped network drives alike (anything
/// whose root opens) — and other values pass through. Order is kept; duplicates
/// are dropped.
fn expand_drive_args(drives: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in drives {
        if d.eq_ignore_ascii_case("all") {
            for letter in b'A'..=b'Z' {
                let root = format!("{}:\\", letter as char);
                if std::path::Path::new(&root).exists() {
                    out.push(root);
                }
            }
        } else {
            out.push(d.clone());
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.to_ascii_uppercase()));
    out
}

/// The effective RCAS sharpen strength: an explicit `--sharpen` wins; otherwise
/// FSR defaults to 0.9 (≈ AMD's recommended 0.2-stop RCAS attenuation — FSR 1.0
/// is designed as the EASU+RCAS pair) and every other upscaler to off.
fn effective_sharpen(sharpen: Option<f32>, upscale: rdp_gpu::Upscaler) -> f32 {
    match sharpen {
        Some(s) => s.clamp(0.0, 1.0),
        None if upscale == rdp_gpu::Upscaler::Fsr => 0.9,
        None => 0.0,
    }
}

fn init_tracing(log_file: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    // INFO by default: the startup story (negotiation, TLS, NLA, activation,
    // channel opens) still prints, but the per-PDU / per-frame `debug`/`trace`
    // logs stay off — synchronous logging in the graphics hot path throttles the
    // whole pipeline. Set RUST_LOG=debug (or trace) to diagnose.
    let mk_filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `--log-file` writes straight to a file (flushed per line), the reliable way
    // to capture the GUI app — its stdout redirection often comes back empty.
    if let Some(path) = log_file {
        match std::fs::File::create(path) {
            Ok(file) => {
                tracing_subscriber::fmt()
                    .with_env_filter(mk_filter())
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .init();
                return;
            }
            Err(e) => eprintln!("rdpio: could not open log file '{path}': {e}; using stdout"),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(mk_filter())
        .with_target(true)
        .init();
}

#[cfg(windows)]
mod connect;

#[cfg(windows)]
mod iocp;

#[cfg(windows)]
mod net_wait;

#[cfg(windows)]
mod tls;
#[cfg(not(windows))]
#[path = "tls_rustls.rs"]
mod tls;

// STUN/TURN client for W365 RDP Shortpath (UDP). Portable + unit-tested; the
// live Shortpath driver that consumes it is wired on the W365 path.
mod stun;

// W365 Shortpath "nano transport" rendezvous signaling (WebSocket). Milestone 1
// of bringing up the proprietary Basix DCT UDP path; consumed on the W365 path.
// Windows-only: it rides `crate::websocket`, which is part of the Windows-gated
// W365 stack (see PORTING.md, Stage 4).
#[cfg(windows)]
mod rendezvous;

// W365 Shortpath: host Microsoft's rdpnanoTransport.dll via its COM-style C
// exports instead of reimplementing the Basix DCT transport. Loader + object
// creation are done; interface method semantics still being reversed (unwired).
#[cfg(windows)]
mod nano_ffi;

// Teams "Optimized": host Microsoft's MsRdcWebRTCAddIn.dll via the standard
// IWTSPlugin dynamic-virtual-channel plugin ABI, bridging the
// `com.microsoft.rdc.dvc.webrtc.1` channel so Teams A/V runs client-side.
#[cfg(windows)]
mod webrtc_addin;

// Teams "Optimized", the native path (`--teams-native`): drive our own
// webrtc-rs-backed engine against the same `com.microsoft.rdc.dvc.webrtc.1`
// JSON-RPC protocol instead of hosting the DLL — the reimplementation that also
// unlocks Linux.
#[cfg(windows)]
mod webrtc_native;

// The client's real cameras/mics/speakers, reported to Teams over webrtc.1. Teams
// refuses to optimize a call on an endpoint that reports no devices.
#[cfg(windows)]
mod webrtc_devices;

// Follows the TURN `300 Try Alternate` redirect for the native webrtc.1 engine
// (webrtc-rs can't), reusing rdpio's own STUN/TURN client (`stun`). Without it no
// relay candidate can be allocated on Teams' anycast relays.
#[cfg(windows)]
mod webrtc_turn;

// Unhandled-exception logger: names the faulting module on a native crash.
#[cfg(windows)]
mod crash;

#[cfg(windows)]
mod window;

#[cfg(windows)]
mod connbar;

#[cfg(windows)]
mod clipboard;

#[cfg(windows)]
mod audio;

#[cfg(windows)]
mod mic;

#[cfg(windows)]
mod udp;

#[cfg(windows)]
mod mf_camera;

#[cfg(windows)]
mod printer;

#[cfg(windows)]
mod win {
    use std::net::Shutdown;
    use std::sync::mpsc::{self, TryRecvError};
    use std::sync::Arc;
    use std::time::Duration;

    use rdp_gpu::Renderer;

    use crate::window::{Frame, RawInput, Window};
    use crate::{
        config_from_args, connect, feed, gateway, net_listener, reconnect_delay,
        save_reconnect_cookie, session, w365, Args,
    };
    use rdp_pdu::input as inpdu;

    /// The idle slate background, shown before the first server frame arrives.
    const SLATE: [f32; 4] = [0.06, 0.09, 0.16, 1.0];

    /// How long after the last input/resize/frame the UI loop stays in its
    /// 1 ms low-latency poll before dropping back to the 8 ms idle cadence.
    const ACTIVITY_WINDOW: Duration = Duration::from_millis(250);

    /// A frame-sink message forwarded from the session worker thread to the UI
    /// thread (which owns every D3D11 object).
    enum FrameMsg {
        Blit {
            x: u16,
            y: u16,
            w: u16,
            h: u16,
            rgba: Vec<u8>,
        },
        /// An NV12 frame for the UI thread to color-convert on the GPU (with a
        /// CPU fallback) and blit into the framebuffer. `rects` are the
        /// frame-relative dirty regions to paint (empty = whole frame).
        BlitNv12 {
            x: u16,
            y: u16,
            w: u16,
            h: u16,
            nv12: Vec<u8>,
            rects: Vec<(u16, u16, u16, u16)>,
        },
        /// A GPU NV12 texture (zero-copy DXVA decode) for the UI thread to
        /// color-convert on the GPU. `rects` as in [`FrameMsg::BlitNv12`].
        BlitTexture {
            x: u16,
            y: u16,
            w: u16,
            h: u16,
            texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
            rects: Vec<(u16, u16, u16, u16)>,
        },
        /// GPU framebuffer→framebuffer copy (EGFX SurfaceToSurface).
        CopyRect {
            sx: u16,
            sy: u16,
            w: u16,
            h: u16,
            dx: u16,
            dy: u16,
        },
        /// Stash a framebuffer rectangle into GPU cache `slot` (SurfaceToCache).
        CacheRect {
            slot: u16,
            sx: u16,
            sy: u16,
            w: u16,
            h: u16,
        },
        /// Blit GPU cache `slot` onto the framebuffer (CacheToSurface).
        CacheBlit {
            slot: u16,
            dx: u16,
            dy: u16,
        },
        Present,
        /// A cursor-shape change for the UI thread to realise as a Win32 cursor.
        Cursor(session::CursorUpdate),
        /// The server's auto-reconnect cookie, for resuming after a transient drop.
        Cookie(rdp_pdu::logon::ReconnectCookie),
        /// The server reset the desktop size (after a Display Control resize).
        Resize(u16, u16),
    }

    /// [`session::FrameSink`] that ships decoded rectangles to the UI thread.
    struct ChannelSink {
        tx: mpsc::Sender<FrameMsg>,
        metrics: Option<Arc<crate::metrics::Metrics>>,
    }

    impl session::FrameSink for ChannelSink {
        fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
            self.blit_owned(x, y, w, h, rgba.to_vec());
        }
        fn blit_owned(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: Vec<u8>) {
            // Move the decoder's owned buffer straight into the channel message —
            // no second copy (the ClearCodec hot path ships hundreds of tiles/frame).
            if let Some(m) = self.metrics.as_ref() {
                m.record_blit(rgba.len() as u64);
            }
            let _ = self.tx.send(FrameMsg::Blit { x, y, w, h, rgba });
        }
        fn blit_nv12(
            &mut self,
            x: u16,
            y: u16,
            w: u16,
            h: u16,
            nv12: &[u8],
            rects: &[(u16, u16, u16, u16)],
        ) {
            // Hand the NV12 frame to the UI thread, which owns the D3D11 device
            // and converts it on the GPU (falling back to CPU there if needed).
            if let Some(m) = self.metrics.as_ref() {
                m.record_blit(nv12.len() as u64);
            }
            let _ = self.tx.send(FrameMsg::BlitNv12 {
                x,
                y,
                w,
                h,
                nv12: nv12.to_vec(),
                rects: rects.to_vec(),
            });
        }
        fn blit_texture(
            &mut self,
            x: u16,
            y: u16,
            w: u16,
            h: u16,
            texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
            rects: &[(u16, u16, u16, u16)],
        ) {
            // The texture lives on the shared (multithread-protected) device, so
            // it's safe to hand to the UI thread, which color-converts it.
            let _ = self.tx.send(FrameMsg::BlitTexture {
                x,
                y,
                w,
                h,
                texture: texture.clone(),
                rects: rects.to_vec(),
            });
        }
        fn copy_rect(&mut self, sx: u16, sy: u16, w: u16, h: u16, dx: u16, dy: u16) {
            let _ = self.tx.send(FrameMsg::CopyRect { sx, sy, w, h, dx, dy });
        }
        fn cache_rect(&mut self, slot: u16, sx: u16, sy: u16, w: u16, h: u16) {
            let _ = self.tx.send(FrameMsg::CacheRect { slot, sx, sy, w, h });
        }
        fn cache_blit(&mut self, slot: u16, dx: u16, dy: u16) {
            let _ = self.tx.send(FrameMsg::CacheBlit { slot, dx, dy });
        }
        fn present(&mut self) {
            let _ = self.tx.send(FrameMsg::Present);
            // A complete frame is queued — wake the UI thread now instead of
            // letting it discover this on its next poll tick.
            crate::window::signal_frame();
        }
        fn cursor(&mut self, update: session::CursorUpdate) {
            let _ = self.tx.send(FrameMsg::Cursor(update));
            crate::window::signal_frame();
        }
        fn reconnect_cookie(&mut self, cookie: rdp_pdu::logon::ReconnectCookie) {
            let _ = self.tx.send(FrameMsg::Cookie(cookie));
        }
        fn resize(&mut self, w: u16, h: u16) {
            let _ = self.tx.send(FrameMsg::Resize(w, h));
        }
    }

    /// Write a tightly-packed RGBA buffer as a 24-bit bottom-up BMP (for visual
    /// diagnostics). Rows are padded to 4 bytes; alpha is dropped.
    fn write_bmp(path: &std::path::Path, w: usize, h: usize, rgba: &[u8]) {
        if rgba.len() < w * h * 4 {
            return;
        }
        let row = (w * 3 + 3) & !3;
        let data = row * h;
        let mut buf: Vec<u8> = Vec::with_capacity(54 + data);
        buf.extend_from_slice(b"BM");
        buf.extend_from_slice(&((54 + data) as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&54u32.to_le_bytes());
        buf.extend_from_slice(&40u32.to_le_bytes());
        buf.extend_from_slice(&(w as i32).to_le_bytes());
        buf.extend_from_slice(&(h as i32).to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&24u16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(data as u32).to_le_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        for y in (0..h).rev() {
            for x in 0..w {
                let i = (y * w + x) * 4;
                buf.push(rgba[i + 2]); // B
                buf.push(rgba[i + 1]); // G
                buf.push(rgba[i]); // R
            }
            buf.resize(buf.len() + (row - w * 3), 0);
        }
        let _ = std::fs::write(path, buf);
    }

    /// Diagnostic: when `RDPIO_DUMP_GPU` is set, read the live GPU framebuffer
    /// back and write it to `<dir>/gpu_NNN.bmp` every ~120 presents — exactly
    /// what's on screen, to compare against the (correct) decoded tiles.
    fn maybe_dump_gpu_framebuffer(renderer: &mut Renderer) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let Ok(dir) = std::env::var("RDPIO_DUMP_GPU") else {
            return;
        };
        let k = N.fetch_add(1, Ordering::Relaxed);
        if k % 120 != 119 {
            return;
        }
        if let Some((w, h, rgba)) = renderer.readback_framebuffer() {
            let path = std::path::Path::new(&dir).join(format!("gpu_{k:05}.bmp"));
            write_bmp(&path, w as usize, h as usize, &rgba);
        }
    }

    /// Diagnostic: when `RDPIO_DUMP_CC` is set, write decoded ClearCodec image
    /// tiles (≥64×64) to `<dir>/tile_SS_x_y_wxh.bmp` in a 24-slot ROLLING buffer
    /// (slot = seq % 24), so after a video plays the files hold the most-recent
    /// tiles — the actual video frames — tagged with their on-desktop position.
    fn dump_tile_bmp(rgba: &[u8], x: u32, y: u32, w: u16, h: u16) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let Ok(dir) = std::env::var("RDPIO_DUMP_CC") else {
            return;
        };
        if w < 64 || h < 64 {
            return;
        }
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let slot = n % 24;
        let path =
            std::path::Path::new(&dir).join(format!("tile_{slot:02}_{x}_{y}_{w}x{h}.bmp"));
        write_bmp(&path, w as usize, h as usize, rgba);
    }

    /// Diagnostic: when `RDPIO_DUMP_CC_RAW` is set, append every raw
    /// CLEARCODEC_BITMAP_STREAM (the decoder INPUT) to `<dir>/cc_<pid>.bin` in
    /// ARRIVAL ORDER as length-prefixed records `[x u32][y u32][w u32][h u32][len
    /// u32][bytes]`. The glyph/vBar caches are stateful, so the exact ordered
    /// sequence is what lets the `cc_replay` example reproduce a garbled desktop
    /// offline and deterministically. Buffered + flushed so a killed session keeps
    /// its data; capped to bound disk.
    fn dump_cc_raw(stream: &[u8], x: u32, y: u32, w: u16, h: u16) {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Mutex, OnceLock};
        static W: OnceLock<Option<Mutex<std::io::BufWriter<std::fs::File>>>> = OnceLock::new();
        static N: AtomicU32 = AtomicU32::new(0);
        let writer = W.get_or_init(|| {
            let dir = std::env::var("RDPIO_DUMP_CC_RAW").ok()?;
            let _ = std::fs::create_dir_all(&dir);
            let path = std::path::Path::new(&dir).join(format!("cc_{}.bin", std::process::id()));
            Some(Mutex::new(std::io::BufWriter::new(std::fs::File::create(path).ok()?)))
        });
        let Some(m) = writer else { return };
        if N.fetch_add(1, Ordering::Relaxed) >= 40000 {
            return;
        }
        if let Ok(mut f) = m.lock() {
            for v in [x, y, w as u32, h as u32, stream.len() as u32] {
                let _ = f.write_all(&v.to_le_bytes());
            }
            let _ = f.write_all(stream);
            let _ = f.flush();
        }
    }

    /// Diagnostic: when `RDPIO_DUMP_PROG` is set, write every raw WireToSurface2
    /// progressive payload to `<dir>/prog_NNNNN_<ctx>.bin` (capped). These replay
    /// offline through `cargo run -p rdp-graphics --example prog_replay -- <dir>`,
    /// reproducing the exact decoder state evolution without a live session.
    fn maybe_dump_prog(ctx: u32, data: &[u8]) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let Ok(dir) = std::env::var("RDPIO_DUMP_PROG") else {
            return;
        };
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n >= 2400 {
            return; // ~80 s of 30 fps video; keeps disk use bounded
        }
        // One subdir per process so successive runs never overwrite each other
        // (replay needs one uninterrupted session's stream).
        let dir = std::path::Path::new(&dir).join(format!("sess_{}", std::process::id()));
        if n == 0 {
            let _ = std::fs::create_dir_all(&dir);
        }
        let _ = std::fs::write(dir.join(format!("prog_{n:05}_{ctx}.bin")), data);
    }

    // ---- Static-codec corruption diagnostics --------------------------------
    // Localizes the "garbled text + black holes" to a specific codec/failure mode
    // without a live debugger: per-codec paint counters (logged ~every 3s as
    // "EGFX codec usage (diag)"), rate-limited anomaly warnings (decode failures,
    // cache misses, all-black tiles), and an opt-in codec OVERLAY (env
    // RDPIO_CODEC_OVERLAY) that tints each painted region by the codec that
    // produced it — so a single screenshot maps every corrupt region to its codec
    // and a region NOTHING paints stays pure black.
    static D_SOLIDFILL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_PROG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_PROG_BLACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_CLEAR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_CLEAR_BLACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_CLEAR_FAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_UNCOMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_UNCOMP_BLACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_UNCOMP_FAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_UNSUPPORTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_CACHE_BLIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_CACHE_MISS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_COPY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static D_H264: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn diag_overlay_enabled() -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("RDPIO_CODEC_OVERLAY").is_some())
    }

    /// Whether verbose codec diagnostics (the periodic per-codec summary and the
    /// all-black-tile warning, which false-positives on legitimately black UI) are
    /// on. Off by default; enabled by `RDPIO_DIAG` or the overlay. The cache-miss
    /// and decode-failure warnings stay on always as cheap regression canaries.
    fn diag_enabled() -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            std::env::var_os("RDPIO_DIAG").is_some() || diag_overlay_enabled()
        })
    }

    /// Blend a per-codec tint 50/50 into an RGBA region (overlay mode).
    fn diag_tint(rgba: &mut [u8], c: [u8; 3]) {
        for px in rgba.chunks_exact_mut(4) {
            px[0] = ((px[0] as u16 + c[0] as u16) / 2) as u8;
            px[1] = ((px[1] as u16 + c[1] as u16) / 2) as u8;
            px[2] = ((px[2] as u16 + c[2] as u16) / 2) as u8;
        }
    }

    /// Rate-limited anomaly warning (first 12, then every 256th) with a total.
    fn diag_anomaly(counter: &std::sync::atomic::AtomicU64, kind: &str, x: u32, y: u32, w: u32, h: u32) {
        use std::sync::atomic::Ordering;
        let n = counter.fetch_add(1, Ordering::Relaxed);
        if n < 12 || n % 256 == 0 {
            tracing::warn!(kind, x, y, w, h, count = n + 1, "EGFX paint anomaly");
        }
    }

    /// Count, all-black-detect, and (with RDPIO_CODEC_OVERLAY) tint the RGBA blits a
    /// codec produced. Non-RGBA blits (H.264 NV12/texture) are left untouched.
    fn diag_emit(
        blits: &mut [session::GfxBlit],
        color: [u8; 3],
        ok: &std::sync::atomic::AtomicU64,
        black: Option<&std::sync::atomic::AtomicU64>,
    ) {
        use std::sync::atomic::Ordering;
        let overlay = diag_overlay_enabled();
        for b in blits {
            if let session::GfxBlit::Rgba { x, y, w, h, rgba } = b {
                ok.fetch_add(1, Ordering::Relaxed);
                if let Some(bc) = black {
                    if diag_enabled()
                        && !rgba.is_empty()
                        && rgba.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
                    {
                        diag_anomaly(bc, "all-black-tile", *x as u32, *y as u32, *w as u32, *h as u32);
                    }
                }
                if overlay {
                    diag_tint(rgba, color);
                }
            }
        }
    }

    /// Log the per-codec paint counters at most once every 3s.
    fn diag_tick() {
        use std::sync::atomic::Ordering;
        use std::sync::Mutex;
        use std::time::Instant;
        if !diag_enabled() {
            return;
        }
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        let now = Instant::now();
        {
            let mut g = LAST.lock().unwrap();
            match *g {
                Some(t) if now.duration_since(t).as_secs() < 3 => return,
                _ => *g = Some(now),
            }
        }
        tracing::info!(
            solidfill = D_SOLIDFILL.load(Ordering::Relaxed),
            progressive = D_PROG.load(Ordering::Relaxed),
            prog_black = D_PROG_BLACK.load(Ordering::Relaxed),
            clearcodec = D_CLEAR.load(Ordering::Relaxed),
            clear_black = D_CLEAR_BLACK.load(Ordering::Relaxed),
            clear_fail = D_CLEAR_FAIL.load(Ordering::Relaxed),
            uncompressed = D_UNCOMP.load(Ordering::Relaxed),
            uncomp_black = D_UNCOMP_BLACK.load(Ordering::Relaxed),
            uncomp_fail = D_UNCOMP_FAIL.load(Ordering::Relaxed),
            unsupported = D_UNSUPPORTED.load(Ordering::Relaxed),
            cache_blit = D_CACHE_BLIT.load(Ordering::Relaxed),
            cache_miss = D_CACHE_MISS.load(Ordering::Relaxed),
            copy_rect = D_COPY.load(Ordering::Relaxed),
            h264 = D_H264.load(Ordering::Relaxed),
            "EGFX codec usage (diag)"
        );
    }

    /// EGFX renderer backed by the Media Foundation H.264 decoder. AVC420/AVC444
    /// surface payloads are decoded to NV12 then converted to RGBA; uncompressed
    /// surface bits convert directly. Created on the worker thread because the
    /// decoder holds COM objects that are not `Send`.
    ///
    /// A [`SurfaceTable`](rdp_graphics::surface::SurfaceTable) tracks where each
    /// server surface is mapped on the desktop, so surface-relative update rects
    /// are translated to absolute screen coordinates before they're blitted.
    struct MfRenderer {
        /// Per-surface CPU decoders for the main view (AVC444's luma sub-stream,
        /// and AVC420 when the GPU/DXVA decoder is unavailable). Each EGFX surface
        /// carries its own independent H.264 bitstream (own SPS/PPS and reference
        /// frames), so a multi-monitor server — one surface per monitor — must not
        /// share a decoder: interleaving streams corrupts reference prediction.
        /// Keyed by surface id, created lazily at the surface's own size.
        cpu_decoders: std::collections::HashMap<u16, rdp_gpu::h264::H264Decoder>,
        /// Per-surface CPU decoders for AVC444's auxiliary (chroma) sub-stream.
        /// Separate from `cpu_decoders` because each sub-stream is its own H.264
        /// sequence with its own references.
        cpu_aux_decoders: std::collections::HashMap<u16, rdp_gpu::h264::H264Decoder>,
        /// Per-surface zero-copy DXVA decoders for AVC420 (GPU decode → GPU
        /// texture). Populated only when a shared device is available and DXVA
        /// init succeeds; keyed by surface id like the CPU maps.
        gpu_decoders: std::collections::HashMap<u16, rdp_gpu::h264::H264GpuDecoder>,
        /// Region rects for access units handed to a decoder but not yet answered
        /// with a picture — oldest first, one entry per submitted unit, per surface.
        ///
        /// An H.264 decoder is a PIPELINE: feeding access unit N typically yields
        /// unit N-1's picture (and the first few units yield nothing at all while
        /// the MFT settles its output type). The region rects that mask a frame
        /// belong to the unit that *encoded* it, so attaching the rects of the unit
        /// we just submitted paints every frame through the wrong mask — a constant
        /// off-by-one that never self-corrects, leaving scrolled text half-updated.
        /// Queueing the rects and popping one per emitted picture re-pairs them.
        pending_rects: std::collections::HashMap<
            u16,
            std::collections::VecDeque<(i64, Vec<(u16, u16, u16, u16)>)>,
        >,
        /// The shared device/context for creating the DXVA decoder; `None` →
        /// always use the CPU path.
        device: Option<(
            windows::Win32::Graphics::Direct3D11::ID3D11Device,
            windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        )>,
        /// DXVA failures so far. A transient MFT/DXVA error (a resolution
        /// change, an adapter power transition) used to latch the whole session
        /// onto software decode; instead each failure arms a backoff-and-retry,
        /// and only `MAX_GPU_DECODE_FAILURES` strikes make the CPU fallback
        /// permanent.
        gpu_fail_count: u32,
        /// When armed, the earliest time the DXVA path may be retried.
        gpu_retry_at: Option<std::time::Instant>,
        /// One-shot: warned that the GPU path renders AVC444 as 4:2:0.
        avc444_gpu_warned: bool,
        surfaces: rdp_graphics::surface::SurfaceTable,
        desktop_w: u32,
        desktop_h: u32,
        /// ClearCodec decoder (codec 0x08): the CPU desktop codec a GPU-less host
        /// streams the static desktop / UI / text with. Stateful — it holds a
        /// 4000-entry glyph cache and a two-level vBar cache. These are GLOBAL to
        /// the GFX channel (MS-RDPEGFX / FreeRDP parity): one decoder context is
        /// shared across ALL surfaces, and the server picks cache indices on that
        /// assumption. So, exactly like `gfx_cache` and the GPU-side cache, this
        /// MUST persist across a ResetGraphics (desktop resize) AND across surface
        /// delete/recreate — wiping it makes the server's later glyph/vBar cache
        /// hits miss, which paints black boxes and gutted, unreadable text.
        clear_decoder: rdp_graphics::clearcodec::ClearDecoder,
        /// Per-surface RemoteFX Progressive decoders (the codec a GPU-less host
        /// streams *video* with, via WireToSurface2). Like the ClearCodec and
        /// H.264 decoders these are stateful — a progressive context's per-tile
        /// coefficient state accumulates across frames — and scoped to one surface
        /// stream, so each surface gets its own. Keyed by surface id, created lazily.
        progressive_decoders: std::collections::HashMap<u16, rdp_graphics::progressive::ProgressiveDecoder>,
        /// CPU shadow of the output desktop (RGBA). Kept in sync with every RGBA
        /// blit so SurfaceToSurface/CacheToSurface can read prior pixels back —
        /// without it those copy/cache commands (which carry no pixels) can't be
        /// satisfied and leave large regions stale/black.
        fb: Vec<u8>,
        fb_w: u32,
        fb_h: u32,
        /// EGFX surface cache: slot → (w, h, RGBA), filled by SurfaceToCache.
        gfx_cache: std::collections::HashMap<u16, (u32, u32, Vec<u8>)>,
        /// Diagnostic (`--no-seed`): decode ClearCodec tiles from black, never
        /// seeding from the persistent surface. Isolates seed-path artifacts.
        seed_disabled: bool,
    }

    impl MfRenderer {
        fn new(
            desktop_w: u32,
            desktop_h: u32,
            device: Option<(
                windows::Win32::Graphics::Direct3D11::ID3D11Device,
                windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
            )>,
        ) -> Self {
            Self {
                cpu_decoders: std::collections::HashMap::new(),
                cpu_aux_decoders: std::collections::HashMap::new(),
                gpu_decoders: std::collections::HashMap::new(),
                pending_rects: std::collections::HashMap::new(),
                device,
                gpu_fail_count: 0,
                gpu_retry_at: None,
                avc444_gpu_warned: false,
                surfaces: rdp_graphics::surface::SurfaceTable::new(),
                desktop_w,
                desktop_h,
                clear_decoder: rdp_graphics::clearcodec::ClearDecoder::new(),
                progressive_decoders: std::collections::HashMap::new(),
                fb: Vec::new(),
                fb_w: 0,
                fb_h: 0,
                gfx_cache: std::collections::HashMap::new(),
                seed_disabled: false,
            }
        }

        /// (Re)allocate the CPU desktop shadow to `w`x`h`, clearing it.
        fn fb_ensure(&mut self, w: u32, h: u32) {
            if self.fb_w != w || self.fb_h != h {
                self.fb = vec![0u8; (w as usize) * (h as usize) * 4];
                self.fb_w = w;
                self.fb_h = h;
            }
        }

        /// Write an RGBA rectangle into the CPU desktop shadow (clipped).
        fn fb_put(&mut self, x: u32, y: u32, w: u32, h: u32, rgba: &[u8]) {
            if self.fb_w == 0 {
                self.fb_ensure(self.desktop_w.max(x + w), self.desktop_h.max(y + h));
            }
            if x >= self.fb_w {
                return;
            }
            let cols = (self.fb_w - x).min(w) as usize;
            for row in 0..h {
                let dy = y + row;
                if dy >= self.fb_h {
                    break;
                }
                let src = (row as usize) * (w as usize) * 4;
                let dst = ((dy as usize) * (self.fb_w as usize) + x as usize) * 4;
                if src + cols * 4 <= rgba.len() && dst + cols * 4 <= self.fb.len() {
                    self.fb[dst..dst + cols * 4].copy_from_slice(&rgba[src..src + cols * 4]);
                }
            }
        }

        /// Read a `w`x`h` RGBA rectangle out of the CPU shadow (black-padded
        /// where it falls outside the shadow).
        fn fb_region(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
            let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
            if x >= self.fb_w {
                return out;
            }
            let cols = (self.fb_w - x).min(w) as usize;
            for row in 0..h {
                let sy = y + row;
                if sy >= self.fb_h {
                    break;
                }
                let src = ((sy as usize) * (self.fb_w as usize) + x as usize) * 4;
                let dst = (row as usize) * (w as usize) * 4;
                if src + cols * 4 <= self.fb.len() && dst + cols * 4 <= out.len() {
                    out[dst..dst + cols * 4].copy_from_slice(&self.fb[src..src + cols * 4]);
                }
            }
            out
        }

        /// Mirror every RGBA blit into the CPU desktop shadow.
        fn shadow_blits(&mut self, blits: &[session::GfxBlit]) {
            for b in blits {
                if let session::GfxBlit::Rgba { x, y, w, h, rgba } = b {
                    self.fb_put(*x as u32, *y as u32, *w as u32, *h as u32, rgba);
                }
            }
        }

        /// Diagnostic: when `RDPIO_DUMP_FB` is set, write the whole composited CPU
        /// shadow (the desktop as we've assembled it) to `<dir>/fb_NNN.bmp` every
        /// ~300 tiles, so the assembled surface can be compared to the screen.
        fn maybe_dump_shadow(&self) {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let Ok(dir) = std::env::var("RDPIO_DUMP_FB") else {
                return;
            };
            let k = N.fetch_add(1, Ordering::Relaxed);
            if k % 300 != 299 || self.fb_w == 0 || self.fb.is_empty() {
                return;
            }
            let path = std::path::Path::new(&dir).join(format!("fb_{k:05}.bmp"));
            write_bmp(&path, self.fb_w as usize, self.fb_h as usize, &self.fb);
        }

        /// Crop `rect` (surface-relative, right/bottom exclusive) out of a tightly
        /// packed RGBA frame and emit a blit at the desktop position
        /// `(origin + rect.top_left)`. Returns `None` when the rect doesn't
        /// intersect the decoded frame. AVC region rects are clamped to the coded
        /// frame because a server may name pixels outside what H.264 produced.
        fn crop_blit(
            rgba: &[u8],
            fw: usize,
            fh: usize,
            rect: rdp_graphics::avc::Rect,
            origin_x: u32,
            origin_y: u32,
        ) -> Option<session::GfxBlit> {
            let l = (rect.left as usize).min(fw);
            let t = (rect.top as usize).min(fh);
            let r = (rect.right as usize).min(fw);
            let b = (rect.bottom as usize).min(fh);
            if r <= l || b <= t {
                return None;
            }
            let (cw, ch) = (r - l, b - t);
            let mut out = Vec::with_capacity(cw * ch * 4);
            for row in t..b {
                let start = (row * fw + l) * 4;
                out.extend_from_slice(&rgba[start..start + cw * 4]);
            }
            // Surface origin + rect offset, saturating into the u16 screen space
            // the blit sink expects (RDP desktop coordinates fit in u16).
            let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            Some(session::GfxBlit::Rgba {
                x: clamp(origin_x + l as u32),
                y: clamp(origin_y + t as u32),
                w: cw as u16,
                h: ch as u16,
                rgba: out,
            })
        }

        /// The decode-size hint for a surface: its own dimensions if the surface
        /// is known, else the desktop size. Per-monitor surfaces are monitor-sized
        /// — a virtual desktop spanning several 4K monitors exceeds single-stream
        /// H.264 level limits, so hinting the decoder at the surface (not the
        /// whole desktop) keeps each stream within level.
        fn surface_dims(&self, surface_id: u16) -> (u32, u32) {
            self.surfaces
                .get(surface_id)
                .map(|s| (s.width.max(1) as u32, s.height.max(1) as u32))
                .unwrap_or((self.desktop_w, self.desktop_h))
        }

        /// Lazily create the per-surface decoder in `decoders[surface_id]` (sized
        /// to the surface) and decode one Annex-B access unit. Returns the decoded
        /// frames; an init or decode error logs and yields none (the caller simply
        /// paints nothing).
        /// Get (creating on first use) the CPU H.264 decoder for `surface_id`.
        fn ensure_cpu_decoder<'d>(
            decoders: &'d mut std::collections::HashMap<u16, rdp_gpu::h264::H264Decoder>,
            surface_id: u16,
            w: u32,
            h: u32,
            label: &str,
        ) -> Option<&'d mut rdp_gpu::h264::H264Decoder> {
            use std::collections::hash_map::Entry;
            match decoders.entry(surface_id) {
                Entry::Occupied(e) => Some(e.into_mut()),
                Entry::Vacant(v) => match rdp_gpu::h264::H264Decoder::new(w, h) {
                    Ok(d) => {
                        tracing::info!(decoder = label, surface_id, w, h, "H.264 decoder created");
                        Some(v.insert(d))
                    }
                    Err(e) => {
                        tracing::warn!(decoder = label, surface_id, error = %e, "H.264 decoder init failed");
                        None
                    }
                },
            }
        }

        fn decode_stream(
            decoders: &mut std::collections::HashMap<u16, rdp_gpu::h264::H264Decoder>,
            surface_id: u16,
            w: u32,
            h: u32,
            label: &str,
            h264: &[u8],
        ) -> Vec<rdp_gpu::h264::DecodedFrame> {
            let Some(decoder) = Self::ensure_cpu_decoder(decoders, surface_id, w, h, label)
            else {
                return Vec::new();
            };
            match decoder.decode(h264) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(decoder = label, surface_id, error = %e, "H.264 decode failed");
                    Vec::new()
                }
            }
        }

        /// Take the queued region rects for decoder output `unit_id`.
        ///
        /// Queue entries older than `unit_id` belong to units that produced no
        /// picture (SPS/PPS-only, or dropped by the decoder) — discarding them
        /// here is what keeps the pairing *exact* instead of drifting one-off
        /// forever after any drop, which the old positional FIFO did. An
        /// unknown id (decoder didn't echo the tag) falls back to FIFO order.
        /// An empty return means "no bounded region" — the painter overpaints
        /// the whole frame, the safe failure.
        fn take_rects(
            q: &mut std::collections::VecDeque<(i64, Vec<(u16, u16, u16, u16)>)>,
            unit_id: i64,
        ) -> Vec<(u16, u16, u16, u16)> {
            if unit_id < 0 {
                return q.pop_front().map(|(_, r)| r).unwrap_or_default();
            }
            while let Some((id, _)) = q.front() {
                if *id < unit_id {
                    q.pop_front();
                } else {
                    break;
                }
            }
            match q.front() {
                Some((id, _)) if *id == unit_id => {
                    q.pop_front().map(|(_, r)| r).unwrap_or_default()
                }
                _ => Vec::new(),
            }
        }

        /// Blit a decoded RGBA frame's dirty regions to their desktop positions.
        /// The decoded frame is aligned to the surface origin `(0,0)`; each
        /// metablock region rect selects a changed slice (MS-RDPEGFX 2.2.4.4).
        /// With no regions the whole `dest` rectangle is the update.
        fn blits_for_regions(
            rgba: &[u8],
            fw: usize,
            fh: usize,
            rects: &[rdp_graphics::avc::Rect],
            dest: rdp_pdu::gfx::Rect16,
            origin: (u32, u32),
            out: &mut Vec<session::GfxBlit>,
        ) {
            let (ox, oy) = origin;
            if rects.is_empty() {
                let whole = rdp_graphics::avc::Rect {
                    left: dest.left,
                    top: dest.top,
                    right: dest.right,
                    bottom: dest.bottom,
                };
                if let Some(b) = Self::crop_blit(rgba, fw, fh, whole, ox, oy) {
                    out.push(b);
                }
            } else {
                for &r in rects {
                    if let Some(b) = Self::crop_blit(rgba, fw, fh, r, ox, oy) {
                        out.push(b);
                    }
                }
            }
        }

        /// Combine a paired AVC444 main + auxiliary frame into full-chroma RGBA
        /// (MS-RDPEGFX 2.2.4.5). Returns `None` (→ caller falls back to luma-only)
        /// if the frames don't pair up dimensionally or a plane is malformed.
        fn reconstruct_yuv444_rgba(
            main: &rdp_gpu::h264::DecodedFrame,
            aux: &rdp_gpu::h264::DecodedFrame,
        ) -> Option<Vec<u8>> {
            let (fw, fh) = (main.width as usize, main.height as usize);
            // The aux luma rows must line up with the main width for the
            // odd-row chroma fill; bail to luma-only if the streams disagree.
            if aux.width != main.width || fw == 0 || fh == 0 {
                return None;
            }
            let (my, muv) = main.planes();
            let (ay, auv) = aux.planes();
            let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
            let (mu, mv) = rdp_graphics::yuv::nv12_chroma_to_planar(muv, cw, ch, cw * 2)?;
            let (au, av) = rdp_graphics::yuv::nv12_chroma_to_planar(auv, cw, ch, cw * 2)?;
            let (u444, v444) = rdp_graphics::yuv::combine_avc444_to_yuv444(
                &mu,
                &mv,
                ay,
                &au,
                &av,
                fw,
                fh,
                aux.height as usize,
            )?;
            rdp_graphics::yuv::yuv444_to_rgba(my, &u444, &v444, fw, fh, fw)
        }

        /// Frame-relative dirty regions for the GPU/NV12 blit paths: `(x, y, w, h)`
        /// tuples from the AVC metablock's `regionRects` (exclusive right/bottom).
        fn region_tuples(rects: &[rdp_graphics::avc::Rect]) -> Vec<(u16, u16, u16, u16)> {
            rects
                .iter()
                .map(|r| {
                    (
                        r.left,
                        r.top,
                        r.right.saturating_sub(r.left),
                        r.bottom.saturating_sub(r.top),
                    )
                })
                .collect()
        }

        /// Decode an AVC420 payload into NV12 blits. The decoded H.264 frame is a
        /// complete picture aligned to the surface origin, but only the metablock's
        /// `regionRects` hold valid NEW content — outside them the picture is the
        /// encoder's reference, which goes stale as soon as another codec
        /// (ClearCodec/progressive) paints the same surface. The rects ride along
        /// with each blit so the painter clips to them (MS-RDPEGFX 2.2.4.4);
        /// painting the whole frame caused ghosting/flicker under heavy motion.
        fn decode_avc420(
            &mut self,
            data: &[u8],
            _dest: rdp_pdu::gfx::Rect16,
            surface_id: u16,
            origin_x: u32,
            origin_y: u32,
        ) -> Vec<session::GfxBlit> {
            let Some(stream) = rdp_graphics::avc::parse_avc420(data) else {
                tracing::debug!("AVC420 stream parse failed");
                return Vec::new();
            };
            let h264 = stream.h264.to_vec();
            let rects = Self::region_tuples(&stream.rects);
            let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            let (sw, sh) = self.surface_dims(surface_id);

            // Zero-copy GPU path: decode straight into GPU NV12 textures.
            if let Some(blits) = self.decode_avc420_gpu(
                surface_id,
                sw,
                sh,
                &h264,
                clamp(origin_x),
                clamp(origin_y),
                &rects,
            ) {
                return blits;
            }

            // CPU fallback: software decode to NV12, converted on the UI thread.
            // The software decoder is the same pipelined MFT, so its pictures
            // need the same rect pairing as the GPU path. The decoder must
            // exist BEFORE the push so this unit's rects can be queued under
            // the id the decoder will stamp on it.
            let unit_id = match Self::ensure_cpu_decoder(
                &mut self.cpu_decoders,
                surface_id,
                sw,
                sh,
                "main",
            ) {
                Some(d) => d.next_unit_id(),
                None => return Vec::new(),
            };
            {
                let q = self.pending_rects.entry(surface_id).or_default();
                q.push_back((unit_id, rects.clone()));
                // Id-keyed pairing survives drops, so the cap is purely a
                // memory bound, not a correctness cliff.
                const MAX_PIPELINE_DEPTH: usize = 32;
                while q.len() > MAX_PIPELINE_DEPTH {
                    q.pop_front();
                }
            }
            let frames =
                Self::decode_stream(&mut self.cpu_decoders, surface_id, sw, sh, "main", &h264);
            let queued = self.pending_rects.entry(surface_id).or_default();
            let mut blits = Vec::new();
            for f in frames {
                if f.width == 0 || f.height == 0 {
                    continue;
                }
                let rects = Self::take_rects(queued, f.unit_id);
                blits.push(session::GfxBlit::Nv12 {
                    x: clamp(origin_x),
                    y: clamp(origin_y),
                    w: f.width as u16,
                    h: f.height as u16,
                    nv12: f.nv12,
                    rects,
                });
            }
            tracing::debug!(blits = blits.len(), "AVC420 decoded (NV12)");
            blits
        }

        /// After this many DXVA failures the CPU fallback is permanent.
        const MAX_GPU_DECODE_FAILURES: u32 = 3;

        /// Whether the DXVA path may be tried right now: a device exists, the
        /// failure count hasn't gone permanent, and any retry backoff elapsed.
        fn gpu_decode_allowed(&mut self) -> bool {
            if self.device.is_none() || self.gpu_fail_count >= Self::MAX_GPU_DECODE_FAILURES {
                return false;
            }
            match self.gpu_retry_at {
                Some(t) if std::time::Instant::now() < t => false,
                Some(_) => {
                    self.gpu_retry_at = None;
                    tracing::info!(
                        failures = self.gpu_fail_count,
                        "retrying DXVA GPU decode after backoff"
                    );
                    true
                }
                None => true,
            }
        }

        /// Record a DXVA failure: drop the decoders (their surfaces may be tied
        /// to the failed state) and arm a backoff retry, or go permanent after
        /// `MAX_GPU_DECODE_FAILURES` strikes.
        fn note_gpu_failure(&mut self) {
            self.gpu_fail_count += 1;
            self.gpu_decoders.clear();
            self.pending_rects.clear();
            if self.gpu_fail_count >= Self::MAX_GPU_DECODE_FAILURES {
                tracing::warn!(
                    failures = self.gpu_fail_count,
                    "DXVA failed repeatedly; CPU H.264 decode for the rest of the session"
                );
            } else {
                let backoff = std::time::Duration::from_secs(2u64 << (self.gpu_fail_count - 1));
                self.gpu_retry_at = Some(std::time::Instant::now() + backoff);
            }
        }

        /// Try the zero-copy DXVA path for an AVC420 unit. Returns `Some(blits)`
        /// when the GPU decoder is in use (possibly empty if a unit produced no
        /// frame), or `None` to signal "fall back to the CPU decoder".
        #[allow(clippy::too_many_arguments)]
        fn decode_avc420_gpu(
            &mut self,
            surface_id: u16,
            w: u32,
            h: u32,
            h264: &[u8],
            x: u16,
            y: u16,
            rects: &[(u16, u16, u16, u16)],
        ) -> Option<Vec<session::GfxBlit>> {
            if !self.gpu_decode_allowed() {
                return None;
            }
            // Lazily create the per-surface DXVA decoder from the shared device.
            if !self.gpu_decoders.contains_key(&surface_id) {
                let (dev, ctx) = self.device.clone()?;
                match rdp_gpu::h264::H264GpuDecoder::new(w, h, &dev, &ctx) {
                    Ok(d) => {
                        tracing::info!(surface_id, w, h, "DXVA GPU H.264 decoder created (zero-copy)");
                        self.gpu_decoders.insert(surface_id, d);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "DXVA decoder unavailable; using CPU decode");
                        self.note_gpu_failure();
                        return None;
                    }
                }
            }
            // Queue this unit's mask BEFORE decoding, keyed by the id the
            // decoder will stamp on the picture it produces (the MFT echoes
            // each input sample's time), then pair each output picture with
            // its own unit's mask — see `pending_rects` and `take_rects`.
            {
                let unit_id = self.gpu_decoders.get(&surface_id)?.next_unit_id();
                let q = self.pending_rects.entry(surface_id).or_default();
                q.push_back((unit_id, rects.to_vec()));
                // A decoder that swallows units without ever emitting them
                // would grow this without bound; id-keyed pairing survives the
                // drop, so this is purely a memory bound.
                const MAX_PIPELINE_DEPTH: usize = 32;
                while q.len() > MAX_PIPELINE_DEPTH {
                    q.pop_front();
                }
            }
            let decoder = self.gpu_decoders.get_mut(&surface_id)?;
            match decoder.decode(h264) {
                Ok(frames) => {
                    let q = self.pending_rects.entry(surface_id).or_default();
                    Some(
                        frames
                            .into_iter()
                            .filter(|f| f.width != 0 && f.height != 0)
                            .map(|f| {
                                // An empty mask means "paint the whole frame" —
                                // the safe failure when this picture's entry
                                // was dropped from the queue.
                                let rects = Self::take_rects(q, f.unit_id);
                                session::GfxBlit::Texture {
                                    x,
                                    y,
                                    w: f.width as u16,
                                    h: f.height as u16,
                                    texture: f.texture,
                                    rects,
                                }
                            })
                            .collect(),
                    )
                }
                Err(e) => {
                    // A DXVA failure on one surface predicts failure on all (the
                    // device/driver is the common cause), so the reaction is
                    // global: drop every GPU decoder and fall to CPU — but with
                    // a backoff retry, because the common causes (resolution
                    // change, adapter power transition) are transient.
                    tracing::warn!(surface_id, error = %e, "DXVA decode failed; falling back to CPU");
                    self.note_gpu_failure();
                    None
                }
            }
        }

        /// Decode an AVC444/AVC444v2 payload. When both sub-streams are present
        /// the chroma is reconstructed to full 4:4:4; otherwise (luma-only frame,
        /// or a per-frame combine miss) it falls back to the main view's 4:2:0,
        /// so colour is approximate but the picture is always correct.
        fn decode_avc444(
            &mut self,
            data: &[u8],
            dest: rdp_pdu::gfx::Rect16,
            surface_id: u16,
            origin_x: u32,
            origin_y: u32,
            full_chroma: bool,
        ) -> Vec<session::GfxBlit> {
            let Some(stream) = rdp_graphics::avc::parse_avc444(data) else {
                tracing::debug!("AVC444 stream parse failed");
                return Vec::new();
            };
            let main_h264 = stream.stream1.h264.to_vec();
            let (sw, sh) = self.surface_dims(surface_id);

            // GPU fast path (default): the AVC444 *main* sub-stream is itself an
            // ordinary AVC420 bitstream (full-res luma + 4:2:0 chroma), so decode
            // it straight to an NV12 texture via DXVA — exactly like the AVC420
            // path. This is the performance-optimal route for video/gaming: H.264
            // decode and colour conversion both run on the GPU with zero per-pixel
            // CPU work, instead of two software decodes plus a CPU YUV444 combine.
            // The auxiliary sub-stream (which would lift chroma to full 4:4:4) is
            // dropped here; the CPU path below reconstructs full chroma when the
            // GPU is unavailable. Luma — where text/edge detail lives — is
            // full-resolution either way.
            if self.gpu_decode_allowed() {
                // Be loud (once) about the trade: the user asked for 4:4:4 and
                // the server is paying double encode for it, but this path
                // renders the 4:2:0 main view only.
                if stream.stream2.is_some() && !self.avc444_gpu_warned {
                    self.avc444_gpu_warned = true;
                    tracing::warn!(
                        "AVC444: GPU (DXVA) path discards the aux chroma stream — rendering \
                         4:2:0. Full 4:4:4 reconstruction currently requires the CPU decode \
                         path (e.g. --no-avc off-GPU machines, or a DXVA failure fallback)."
                    );
                }
                let x = u16::try_from(origin_x).unwrap_or(u16::MAX);
                let y = u16::try_from(origin_y).unwrap_or(u16::MAX);
                let rects = Self::region_tuples(&stream.stream1.rects);
                if let Some(blits) =
                    self.decode_avc420_gpu(surface_id, sw, sh, &main_h264, x, y, &rects)
                {
                    return blits;
                }
            }

            // CPU fallback: full 4:4:4 reconstruction from the main + aux
            // sub-streams (now stride-correct after the NV12 extraction fix).
            let rects = stream.stream1.rects.clone();
            let main_frames =
                Self::decode_stream(&mut self.cpu_decoders, surface_id, sw, sh, "main", &main_h264);

            // The auxiliary (chroma) stream is decoded only for a "Both" frame
            // on the ChromaV1-capable codec; otherwise we render luma-only.
            let aux_frames = match (full_chroma, stream.lc, stream.stream2.as_ref()) {
                (true, rdp_graphics::avc::Avc444Lc::Both, Some(aux)) => {
                    let aux_h264 = aux.h264.to_vec();
                    Self::decode_stream(
                        &mut self.cpu_aux_decoders,
                        surface_id,
                        sw,
                        sh,
                        "aux",
                        &aux_h264,
                    )
                }
                _ => Vec::new(),
            };

            let mut blits = Vec::new();
            let mut full = 0usize;
            for (i, mf) in main_frames.iter().enumerate() {
                let (fw, fh) = (mf.width as usize, mf.height as usize);
                let (my, muv) = mf.planes();
                // Full 4:4:4 when a matching aux frame reconstructs cleanly,
                // else the main view's 4:2:0.
                let rgba = match aux_frames.get(i).and_then(|af| Self::reconstruct_yuv444_rgba(mf, af))
                {
                    Some(rgba) => {
                        full += 1;
                        Some(rgba)
                    }
                    None => rdp_graphics::yuv::nv12_to_rgba(my, muv, fw, fh, fw),
                };
                let Some(rgba) = rgba else { continue };
                Self::blits_for_regions(&rgba, fw, fh, &rects, dest, (origin_x, origin_y), &mut blits);
            }
            tracing::debug!(
                main = main_frames.len(),
                aux = aux_frames.len(),
                full_chroma = full,
                blits = blits.len(),
                "AVC444 decoded"
            );
            blits
        }
    }

    impl session::GfxRenderer for MfRenderer {
        fn render(&mut self, command: &rdp_pdu::gfx::GfxCommand) -> Vec<session::GfxBlit> {
            use rdp_pdu::gfx::{
                GfxCommand, CODECID_AVC420, CODECID_AVC444, CODECID_AVC444V2, CODECID_CAVIDEO,
                CODECID_CLEARCODEC, CODECID_UNCOMPRESSED,
            };
            // Track surface create/delete/map first so the output origin is known
            // for this and every later update. Non-surface commands are no-ops.
            self.surfaces.apply(command);
            diag_tick();

            // Log surface lifecycle so multimon surface topology is visible (how
            // many independent surface streams the server creates, and at what
            // sizes — one per monitor vs one spanning surface).
            match command {
                GfxCommand::CreateSurface {
                    surface_id,
                    width,
                    height,
                    ..
                } => tracing::info!(surface_id, width, height, "EGFX CreateSurface"),
                GfxCommand::MapSurfaceToOutput { surface_id, x, y } => {
                    tracing::info!(surface_id, x, y, "EGFX MapSurfaceToOutput")
                }
                _ => {}
            }

            // Diagnostic: surface, the first time we see it, every EGFX command we
            // DON'T handle (dropped) and every WireToSurface1 codec — to reveal
            // what actually paints the (currently black) video region.
            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static SEEN_OTHER: AtomicU64 = AtomicU64::new(0);
                static SEEN_CODEC: AtomicU64 = AtomicU64::new(0);
                match command {
                    GfxCommand::Other { cmd_id } => {
                        let b = 1u64 << (*cmd_id & 63);
                        if SEEN_OTHER.fetch_or(b, Ordering::Relaxed) & b == 0 {
                            tracing::warn!(
                                cmd_id = format!("0x{cmd_id:04x}"),
                                "EGFX command NOT handled — dropped (unrecognized command id)"
                            );
                        }
                    }
                    GfxCommand::WireToSurface1 { codec_id, .. } => {
                        let b = 1u64 << (*codec_id & 63);
                        if SEEN_CODEC.fetch_or(b, Ordering::Relaxed) & b == 0 {
                            tracing::info!(
                                codec_id = format!("0x{codec_id:04x}"),
                                "EGFX WireToSurface1 codec in use"
                            );
                        }
                    }
                    _ => {}
                }
            }

            // A desktop reset changes the size and tears down every surface: drop
            // the stream-scoped decoders so they're recreated for the new layout,
            // and re-base the hint size.
            //
            // Do NOT clear `gfx_cache` OR `clear_decoder`: both are GLOBAL caches
            // (not surface-scoped) that PERSIST across a ResetGraphics. The server
            // keeps its copies and, right after a desktop resize, replays cached
            // slots onto the new surface — CacheToSurface for `gfx_cache`, and
            // ClearCodec glyph/vBar cache hits for `clear_decoder` (cached glyphs /
            // repeated UI tiles / text columns) — and never re-sends that content.
            // Clearing either made every post-resize hit miss → unpainted black
            // tiles, missing glyph cells, and gutted/unreadable text (the "garbled
            // static desktop" bug). The GPU renderer's matching cache is likewise
            // never cleared, so they all stay in sync.
            if let GfxCommand::ResetGraphics { width, height, .. } = command {
                self.desktop_w = *width;
                self.desktop_h = *height;
                self.cpu_decoders.clear();
                self.cpu_aux_decoders.clear();
                self.gpu_decoders.clear();
                // The queued masks belong to units the dropped decoders will now
                // never emit; keeping them would offset every later frame.
                self.pending_rects.clear();
                self.progressive_decoders.clear();
                self.fb_ensure(*width, *height);
                return Vec::new();
            }

            // A deleted surface's stream is over: drop its stream-scoped decoders
            // so a later surface reusing the same id starts fresh rather than
            // inheriting stale H.264 reference frames or Progressive coefficients.
            // The ClearCodec `clear_decoder` is deliberately NOT dropped: its
            // glyph/vBar cache is global to the channel and the server keeps
            // replaying those slots by index even after a surface is recreated.
            if let GfxCommand::DeleteSurface { surface_id } = command {
                self.cpu_decoders.remove(surface_id);
                self.cpu_aux_decoders.remove(surface_id);
                self.gpu_decoders.remove(surface_id);
                self.pending_rects.remove(surface_id);
                self.progressive_decoders.remove(surface_id);
                return Vec::new();
            }

            // A progressive codec context is finished. NOTE: deliberately does NOT
            // clear per-tile coefficient state (delete_context is a no-op, FreeRDP
            // parity) — live difference/upgrade tiles still build on it. Logged
            // because a server that sends this mid-stream was the prime suspect
            // for the "gray emboss" video corruption.
            if let GfxCommand::DeleteEncodingContext {
                surface_id,
                codec_context_id,
            } = command
            {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 8 || n % 100 == 0 {
                    tracing::info!(
                        surface_id = *surface_id,
                        ctx = *codec_context_id,
                        count = n + 1,
                        "EGFX DeleteEncodingContext (tile state kept)"
                    );
                }
                if let Some(dec) = self.progressive_decoders.get_mut(surface_id) {
                    dec.delete_context(*codec_context_id);
                }
                return Vec::new();
            }

            // SolidFill: paint each rectangle a single colour (RDPGFX_COLOR32 is
            // B,G,R,XA). These cover large swaths of the desktop (window/UI
            // backgrounds), so skipping them leaves big stale/black regions.
            if let GfxCommand::SolidFill {
                surface_id,
                color,
                rects,
            } = command
            {
                let (ox, oy) = self.surfaces.output_origin(*surface_id).unwrap_or((0, 0));
                let (r, g, b) = ((*color >> 16) as u8, (*color >> 8) as u8, *color as u8);
                let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                let mut blits = Vec::with_capacity(rects.len());
                for rect in rects {
                    let w = rect.right.saturating_sub(rect.left);
                    let h = rect.bottom.saturating_sub(rect.top);
                    if w == 0 || h == 0 {
                        continue;
                    }
                    let mut rgba = vec![0u8; w as usize * h as usize * 4];
                    for px in rgba.chunks_exact_mut(4) {
                        px[0] = r;
                        px[1] = g;
                        px[2] = b;
                        px[3] = 0xFF;
                    }
                    blits.push(session::GfxBlit::Rgba {
                        x: clamp(ox + rect.left as u32),
                        y: clamp(oy + rect.top as u32),
                        w,
                        h,
                        rgba,
                    });
                }
                self.shadow_blits(&blits);
                diag_emit(&mut blits, [0, 200, 0], &D_SOLIDFILL, None);
                return blits;
            }

            // SurfaceToSurface: copy a rectangle from one surface to one or more
            // points on another (window moves, scrolling). The copy runs on the
            // GPU framebuffer (correct over H.264-painted regions); the CPU shadow
            // is updated in parallel so later ClearCodec seeding stays consistent.
            if let GfxCommand::SurfaceToSurface {
                surface_src,
                surface_dst,
                rect_src,
                dest_pts,
            } = command
            {
                let (sox, soy) = self.surfaces.output_origin(*surface_src).unwrap_or((0, 0));
                let (dox, doy) = self.surfaces.output_origin(*surface_dst).unwrap_or((0, 0));
                let w = rect_src.right.saturating_sub(rect_src.left) as u32;
                let h = rect_src.bottom.saturating_sub(rect_src.top) as u32;
                let mut blits = Vec::new();
                if w > 0 && h > 0 {
                    let sx = sox + rect_src.left as u32;
                    let sy = soy + rect_src.top as u32;
                    // Shadow source pixels (for ClearCodec seeding of non-H.264
                    // content); the GPU copy reads the live framebuffer instead.
                    let src = self.fb_region(sx, sy, w, h);
                    let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                    for p in dest_pts {
                        let dx = dox + p.x as u32;
                        let dy = doy + p.y as u32;
                        blits.push(session::GfxBlit::CopyRect {
                            sx: clamp(sx),
                            sy: clamp(sy),
                            w: clamp(w),
                            h: clamp(h),
                            dx: clamp(dx),
                            dy: clamp(dy),
                        });
                        self.fb_put(dx, dy, w, h, &src);
                    }
                }
                D_COPY.fetch_add(blits.len() as u64, std::sync::atomic::Ordering::Relaxed);
                return blits;
            }

            // SurfaceToCache: stash a surface rectangle into a cache slot, on the
            // GPU (live framebuffer) and in the CPU shadow (for seeding).
            if let GfxCommand::SurfaceToCache {
                surface_id,
                slot,
                rect_src,
                ..
            } = command
            {
                let (ox, oy) = self.surfaces.output_origin(*surface_id).unwrap_or((0, 0));
                let w = rect_src.right.saturating_sub(rect_src.left) as u32;
                let h = rect_src.bottom.saturating_sub(rect_src.top) as u32;
                let mut blits = Vec::new();
                if w > 0 && h > 0 {
                    let sx = ox + rect_src.left as u32;
                    let sy = oy + rect_src.top as u32;
                    let px = self.fb_region(sx, sy, w, h);
                    self.gfx_cache.insert(*slot, (w, h, px));
                    let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                    blits.push(session::GfxBlit::CacheRect {
                        slot: *slot,
                        sx: clamp(sx),
                        sy: clamp(sy),
                        w: clamp(w),
                        h: clamp(h),
                    });
                }
                return blits;
            }

            // CacheToSurface: blit a cached rectangle onto a surface at each point
            // (the server's way of replaying repeated content without resending).
            if let GfxCommand::CacheToSurface {
                slot,
                surface_id,
                dest_pts,
            } = command
            {
                let (ox, oy) = self.surfaces.output_origin(*surface_id).unwrap_or((0, 0));
                let mut blits = Vec::new();
                if let Some((w, h, px)) = self.gfx_cache.get(slot).cloned() {
                    D_CACHE_BLIT.fetch_add(dest_pts.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                    for p in dest_pts {
                        let dx = ox + p.x as u32;
                        let dy = oy + p.y as u32;
                        blits.push(session::GfxBlit::CacheBlit {
                            slot: *slot,
                            dx: clamp(dx),
                            dy: clamp(dy),
                        });
                        self.fb_put(dx, dy, w, h, &px);
                    }
                } else {
                    // The slot was never populated (a SurfaceToCache we dropped or
                    // failed) → CacheToSurface paints nothing → black hole.
                    let (mx, my) = dest_pts
                        .first()
                        .map(|p| (ox + p.x as u32, oy + p.y as u32))
                        .unwrap_or((ox, oy));
                    diag_anomaly(&D_CACHE_MISS, "cache-miss", mx, my, *slot as u32, 0);
                }
                return blits;
            }

            // RemoteFX Progressive (the video codec on a GPU-less host) arrives
            // here via WireToSurface2. The tile positions live inside the bitstream,
            // so the decoder returns already-positioned 64×64 RGBA tiles which we
            // place at the surface's desktop origin. One stateful decoder per
            // surface (its per-tile coefficient state persists across frames).
            if let GfxCommand::WireToSurface2 {
                surface_id,
                codec_id,
                codec_context_id,
                bitmap,
                ..
            } = command
            {
                let (origin_x, origin_y) =
                    self.surfaces.output_origin(*surface_id).unwrap_or((0, 0));
                maybe_dump_prog(*codec_context_id, bitmap);
                let dec = self
                    .progressive_decoders
                    .entry(*surface_id)
                    .or_insert_with(rdp_graphics::progressive::ProgressiveDecoder::new);
                let tiles = dec.decode(*codec_context_id, bitmap);
                // One-time diagnostic: confirm WireToSurface2 actually arrives and
                // whether the progressive decoder produced tiles. Reveals the
                // codec id, payload size, and the first bytes (block framing).
                {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static LOGGED: AtomicBool = AtomicBool::new(false);
                    if !LOGGED.swap(true, Ordering::Relaxed) {
                        let head: Vec<String> =
                            bitmap.iter().take(16).map(|b| format!("{b:02x}")).collect();
                        tracing::info!(
                            codec_id = format!("0x{:04x}", *codec_id),
                            ctx = *codec_context_id,
                            bytes = bitmap.len(),
                            tiles = tiles.len(),
                            head = head.join(" "),
                            "EGFX WireToSurface2 (progressive) received"
                        );
                    }
                }
                // Coalesce horizontally adjacent equal-height tiles into row
                // bands: a 30-tile video row becomes ONE update_rect instead of
                // 30, slashing per-frame GPU upload calls (the UI thread was the
                // bottleneck at 300+ tiles/frame under motion).
                let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                let mut tiles = tiles;
                tiles.sort_by_key(|t| (t.y, t.x));
                let mut blits: Vec<session::GfxBlit> = Vec::new();
                let mut k = 0usize;
                while k < tiles.len() {
                    let (ry, rh, rx) = (tiles[k].y, tiles[k].h, tiles[k].x);
                    let mut end = k + 1;
                    let mut run_w = tiles[k].w as usize;
                    while end < tiles.len() {
                        let (p, n) = (&tiles[end - 1], &tiles[end]);
                        if n.y == ry && n.h == rh && n.x == p.x + p.w {
                            run_w += n.w as usize;
                            end += 1;
                        } else {
                            break;
                        }
                    }
                    let h = rh as usize;
                    let rgba = if end - k == 1 {
                        std::mem::take(&mut tiles[k].rgba)
                    } else {
                        let mut band = dec.acquire_buffer(run_w * h * 4);
                        let mut xoff = 0usize;
                        for t in &tiles[k..end] {
                            let tw = t.w as usize;
                            for row in 0..h {
                                let dst = (row * run_w + xoff) * 4;
                                let src = row * tw * 4;
                                band[dst..dst + tw * 4]
                                    .copy_from_slice(&t.rgba[src..src + tw * 4]);
                            }
                            xoff += tw;
                        }
                        for t in &mut tiles[k..end] {
                            dec.return_buffer(std::mem::take(&mut t.rgba));
                        }
                        band
                    };
                    blits.push(session::GfxBlit::Rgba {
                        x: clamp(origin_x + rx),
                        y: clamp(origin_y + ry),
                        w: clamp(run_w as u32),
                        h: clamp(rh),
                        rgba,
                    });
                    k = end;
                }
                self.shadow_blits(&blits);
                diag_emit(&mut blits, [0, 90, 255], &D_PROG, Some(&D_PROG_BLACK));
                self.maybe_dump_shadow();
                return blits;
            }

            let GfxCommand::WireToSurface1 {
                surface_id,
                codec_id,
                dest,
                bitmap,
                ..
            } = command
            else {
                return Vec::new();
            };
            // Where this surface sits on the desktop (created-but-unmapped → 0,0).
            let (origin_x, origin_y) = self.surfaces.output_origin(*surface_id).unwrap_or((0, 0));
            let mut blits = match *codec_id {
                // Single-stream H.264 4:2:0. The standard AVC420 id is 0x000B;
                // some servers/contexts have historically tagged the same H.264
                // payload as CAVIDEO (0x0003), so accept both. Without 0x000B the
                // AVC420-only path (--gaming) dropped every frame (catch-all → no
                // paint) since the server emits 0x000B, not 0x0003.
                CODECID_AVC420 | CODECID_CAVIDEO => {
                    self.decode_avc420(bitmap, *dest, *surface_id, origin_x, origin_y)
                }
                // AVC444 v1 (CODECID_AVC444) uses the ChromaV1 layout we
                // reconstruct. AVC444v2 uses a different chroma packing and
                // requires advertising CAPVERSION_102+, which we don't — so a
                // v2 frame shouldn't arrive; if one does, decode it luma-only
                // (correct picture, approximate colour) rather than mis-unpack.
                CODECID_AVC444 => {
                    self.decode_avc444(bitmap, *dest, *surface_id, origin_x, origin_y, true)
                }
                CODECID_AVC444V2 => {
                    self.decode_avc444(bitmap, *dest, *surface_id, origin_x, origin_y, false)
                }
                CODECID_UNCOMPRESSED => {
                    let w = dest.right.saturating_sub(dest.left);
                    let h = dest.bottom.saturating_sub(dest.top);
                    match rdp_graphics::bitmap::to_rgba(bitmap, w, h, 32, false) {
                        Some(rgba) => {
                            let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                            vec![session::GfxBlit::Rgba {
                                x: clamp(origin_x + dest.left as u32),
                                y: clamp(origin_y + dest.top as u32),
                                w,
                                h,
                                rgba,
                            }]
                        }
                        None => Vec::new(),
                    }
                }
                CODECID_CLEARCODEC => {
                    let w = dest.right.saturating_sub(dest.left);
                    let h = dest.bottom.saturating_sub(dest.top);
                    // Capture the raw input stream (ordered) for offline replay.
                    dump_cc_raw(bitmap, origin_x + dest.left as u32, origin_y + dest.top as u32, w, h);
                    // Seed the decode with the desktop's current pixels at this
                    // rect: ClearCodec is a persistent-surface codec, so a stream
                    // with no residual layer only re-codes the changed pixels and
                    // expects the rest to keep the previous frame. Without this,
                    // partial updates (window motion, video) paint black holes.
                    // The seed costs a w*h*4 alloc + copy, so skip it when the
                    // composition fully defines the tile (glyph hit, or one
                    // whole-tile RAW/NSCodec region — the video hot path).
                    let seed = if !self.seed_disabled
                        && rdp_graphics::clearcodec::needs_seed(bitmap, w, h)
                    {
                        Some(self.fb_region(
                            origin_x + dest.left as u32,
                            origin_y + dest.top as u32,
                            w as u32,
                            h as u32,
                        ))
                    } else {
                        None
                    };
                    // One channel-global decoder: its glyph/vBar caches are shared
                    // across all surfaces (MS-RDPEGFX/FreeRDP parity) and persist
                    // across resize + surface delete/recreate.
                    match self.clear_decoder.decode_seeded(bitmap, w, h, seed.as_deref()) {
                        Some(rgba) if rgba.len() == w as usize * h as usize * 4 => {
                            // Diagnostic: dump decoded image tiles to BMP (tagged
                            // with their on-desktop position) for visual inspection.
                            dump_tile_bmp(&rgba, origin_x + dest.left as u32, origin_y + dest.top as u32, w, h);
                            let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                            vec![session::GfxBlit::Rgba {
                                x: clamp(origin_x + dest.left as u32),
                                y: clamp(origin_y + dest.top as u32),
                                w,
                                h,
                                rgba,
                            }]
                        }
                        _ => {
                            tracing::debug!(w, h, "ClearCodec decode produced no frame");
                            Vec::new()
                        }
                    }
                }
                other => {
                    tracing::debug!(codec = other, "unsupported EGFX codec; skipping");
                    Vec::new()
                }
            };
            self.shadow_blits(&blits);
            self.maybe_dump_shadow();
            // Diagnostics: attribute this paint to its codec, flag empty results
            // (unpainted = black hole) and all-black tiles, and tint for the overlay.
            let (dx0, dy0) = (origin_x + dest.left as u32, origin_y + dest.top as u32);
            let (dw, dh) = (
                dest.right.saturating_sub(dest.left) as u32,
                dest.bottom.saturating_sub(dest.top) as u32,
            );
            match *codec_id {
                CODECID_AVC420 | CODECID_CAVIDEO | CODECID_AVC444 | CODECID_AVC444V2 => {
                    D_H264.fetch_add(blits.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                CODECID_UNCOMPRESSED => {
                    if blits.is_empty() {
                        diag_anomaly(&D_UNCOMP_FAIL, "uncompressed-decode-fail", dx0, dy0, dw, dh);
                    } else {
                        diag_emit(&mut blits, [255, 220, 0], &D_UNCOMP, Some(&D_UNCOMP_BLACK));
                    }
                }
                CODECID_CLEARCODEC => {
                    if blits.is_empty() {
                        diag_anomaly(&D_CLEAR_FAIL, "clearcodec-decode-fail", dx0, dy0, dw, dh);
                    } else {
                        diag_emit(&mut blits, [255, 0, 0], &D_CLEAR, Some(&D_CLEAR_BLACK));
                    }
                }
                _ => {
                    if blits.is_empty() {
                        diag_anomaly(&D_UNSUPPORTED, "unsupported-codec", dx0, dy0, dw, dh);
                    }
                }
            }
            blits
        }
    }

    /// Diagnostic (`--replay-gfx`): offline-replay an `RDPIO_DUMP_GFX` capture
    /// through the real renderer (CPU-only, `device=None`) and write the
    /// composited CPU shadow to BMP next to the input. Reproduces the live
    /// desktop — cache/copy commands included — with no server, so a pipeline bug
    /// shows up deterministically. The renderer self-sizes from ResetGraphics.
    pub fn replay_gfx(path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let data = std::fs::read(path)?;
        let mut renderer = MfRenderer::new(1920, 1080, None);
        let out_dir = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let mut p = 0usize;
        let mut n = 0usize;
        while p + 4 <= data.len() {
            let len =
                u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
            p += 4;
            if p + len > data.len() {
                break;
            }
            for cmd in rdp_pdu::gfx::parse_commands(&data[p..p + len]) {
                let _ = session::GfxRenderer::render(&mut renderer, &cmd);
            }
            p += len;
            n += 1;
            if n % 1500 == 0 {
                let snap = out_dir.join(format!("gfxrep_{n:06}.bmp"));
                write_bmp(&snap, renderer.fb_w as usize, renderer.fb_h as usize, &renderer.fb);
            }
        }
        let final_path = out_dir.join("gfxrep_final.bmp");
        write_bmp(
            &final_path,
            renderer.fb_w as usize,
            renderer.fb_h as usize,
            &renderer.fb,
        );
        println!(
            "replayed {n} payloads -> {} ({}x{})",
            final_path.display(),
            renderer.fb_w,
            renderer.fb_h
        );
        Ok(())
    }

    /// The no-host demo window (slate background): launched without `--host`.
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let (width, height) = (1280u32, 720u32);
        let window = Window::new("RDPiO", width, height)?;
        let mut renderer = Renderer::new(window.hwnd_raw(), width, height, rdp_gpu::Backend::default())?;
        tracing::info!("M0 window + D3D11 swapchain up; entering message loop");

        loop {
            match window.pump() {
                Frame::Quit => break,
                Frame::Continue { resize } => {
                    if let Some((w, h)) = resize {
                        renderer.resize(w, h)?;
                    }
                    renderer.present_clear(SLATE)?;
                }
            }
        }
        tracing::info!("window closed; shutting down");
        Ok(())
    }

    /// Whether a Direct3D/DXGI error means the GPU device is gone.
    ///
    /// This is not retryable in place: the device, its swapchain, the desktop
    /// framebuffer and every DXVA decoder bound to it are all invalid, so the
    /// only recovery is to build a new one. Causes are routine rather than
    /// exotic — a driver reset (TDR), a driver update installing underneath us,
    /// a GPU hang, and on laptops every suspend/resume cycle.
    fn is_device_lost(err: &windows::core::Error) -> bool {
        matches!(
            err.code().0 as u32,
            0x887A_0005 // DXGI_ERROR_DEVICE_REMOVED
                | 0x887A_0006 // DXGI_ERROR_DEVICE_HUNG
                | 0x887A_0007 // DXGI_ERROR_DEVICE_RESET
                | 0x887A_0020 // DXGI_ERROR_DRIVER_INTERNAL_ERROR
                | 0x8876_0868 // D3DERR_DEVICELOST (older runtimes)
        )
    }

    /// Create the GPU renderer for `hwnd` and apply everything the session
    /// depends on: colour-conversion mode, present pacing, upscaler, sharpening,
    /// the desktop framebuffer, and the per-monitor present targets.
    ///
    /// This is one function precisely so a device lost mid-session can be rebuilt
    /// *identically*. A rebuild path that quietly forgot, say, the upscaler or the
    /// render-scale slices would leave the session subtly different after every
    /// resume — the kind of drift that is very hard to notice and harder to
    /// attribute.
    fn build_renderer(
        hwnd: isize,
        sc_w: u32,
        sc_h: u32,
        args: &Args,
        desktop: (u32, u32),
        per_monitor_layout: Option<&Vec<crate::MonitorPlacement>>,
        extra_hwnds: &[isize],
    ) -> Result<Renderer, Box<dyn std::error::Error>> {
        let mut renderer = Renderer::new(hwnd, sc_w, sc_h, args.backend)?;
        if args.cpu_yuv {
            renderer.disable_gpu_yuv();
            tracing::info!("--cpu-yuv: forcing CPU YUV→RGB conversion");
        }
        renderer.set_low_latency(args.low_latency);
        renderer.set_upscaler(args.upscale);
        renderer.set_sharpen(crate::effective_sharpen(args.sharpen, args.upscale));
        // The framebuffer is always the whole remote desktop; per-monitor windows
        // present slices of it. Under render-scale it's smaller than the window
        // and the present path upscales each slice to its swapchain.
        renderer.ensure_framebuffer(desktop.0, desktop.1)?;
        if let Some(placements) = per_monitor_layout {
            let m = placements[0];
            renderer.set_primary_src(m.src.0, m.src.1, m.src_size.0, m.src_size.1);
            for (m, &target) in placements[1..].iter().zip(extra_hwnds) {
                renderer.add_present_target(
                    target,
                    m.size.0,
                    m.size.1,
                    m.src.0,
                    m.src.1,
                    m.src_size.0,
                    m.src_size.1,
                )?;
            }
        }
        Ok(renderer)
    }

    /// Connect to `--host`, activate, then open a window and paint the live
    /// desktop. The blocking session read loop runs on a worker thread; the UI
    /// thread pumps window messages and presents decoded frames.
    pub fn run_connected(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
        let mut config = config_from_args(args);

        // Windows 365 / AVD modern auth + feed discovery.
        //
        // When `--w365` is set we authenticate via device-code flow, then fetch
        // the W365 feed using the resulting access token. The first feed entry
        // supplies the Reverse Connect gateway and resource context. The token
        // itself is passed as the RDP logon password.
        if args.w365 {
            let tenant = args.tenant.as_deref().unwrap_or("common");
            // `--w365-relogin` discards any cached credentials so the user can
            // switch accounts / force a fresh MFA sign-in.
            if args.w365_relogin {
                let _ = crate::token_cache::clear();
                let _ = crate::password_cache::clear();
            }
            // Reuse a cached refresh token when possible so we do not prompt for
            // credentials + MFA on every launch; only fall back to an interactive
            // sign-in when there is no usable cache. A successful interactive or
            // device-code login is cached for next time.
            let token = if let Some(cached) = (!args.w365_relogin)
                .then(|| crate::token_cache::load_silent(tenant, args.client_id.as_deref()))
                .flatten()
            {
                cached
            } else if args.w365_device_code {
                let t = w365::authenticate_device_code(
                    tenant,
                    args.client_id.as_deref(),
                    None,
                )?;
                crate::token_cache::store(tenant, args.client_id.as_deref(), &t);
                t
            } else {
                let t = crate::webview_auth::authenticate(
                    tenant,
                    args.client_id.as_deref(),
                )?;
                crate::token_cache::store(tenant, args.client_id.as_deref(), &t);
                t
            };
            tracing::info!(tenant, "W365 authentication complete");

            if let Some(rdp_path) = args.rdp_file.clone() {
                // Drive Reverse Connect from a local `.rdp` (e.g. one the Windows
                // App generated): its `gatewayhostname` + `loadbalanceinfo` set up
                // ARM brokering, authenticated by the token we just obtained.
                let contents = std::fs::read_to_string(&rdp_path)
                    .map_err(|e| format!("could not read --rdp-file {rdp_path}: {e}"))?;
                feed::apply_rdp_file(&mut config, &contents);
                match config.reverse_connect.as_mut() {
                    Some(rc) => {
                        rc.access_token = token.token.clone();
                        tracing::info!(
                            gateway = %rc.gateway_fqdn,
                            "W365 Reverse Connect configured from .rdp"
                        );
                    }
                    None => {
                        return Err(format!(
                            "--rdp-file {rdp_path} is not a W365/AVD ARM Reverse Connect .rdp \
                             (need resourceprovider:arm + gatewayhostname + loadbalanceinfo)"
                        )
                        .into());
                    }
                }
                config.credentials.password = token.token;
            } else {

            // Prefer the Windows App's local resource cache: one signed ARM
            // `.rdp` per subscribed Cloud PC, which drives the validated ARM
            // broker path with no live-feed parsing. Fall back to live ARM feed
            // discovery if the cache is absent (or `--feed` was given explicitly).
            let entries = if args.feed.is_some() {
                w365::fetch_feed(&token, tenant, args.client_id.as_deref(), args.feed.as_deref())?
            } else {
                let cached = w365::discover_cached_cloud_pcs();
                if cached.is_empty() {
                    tracing::info!("no cached Cloud PCs found; querying live W365 ARM feed");
                    w365::fetch_feed(&token, tenant, args.client_id.as_deref(), None)?
                } else {
                    tracing::info!(
                        count = cached.len(),
                        "discovered Cloud PCs from Windows App cache"
                    );
                    cached
                }
            };
            if entries.is_empty() {
                return Err("W365: no Cloud PCs found (resource cache empty and feed returned none)".into());
            }
            tracing::info!(count = entries.len(), "discovered hosts for W365 selection");
            for e in &entries {
                tracing::info!(
                    id = %e.id,
                    name = %e.display_name,
                    gateway = %e.gateway_fqdn,
                    "W365 feed entry"
                );
            }

            // With more than one Cloud PC, let the user pick in a WebView panel.
            // A single resource (or a picker that can't open) connects directly.
            let choice = if entries.len() > 1 {
                match crate::cloud_pc_picker::choose_cloud_pc(&entries) {
                    Ok(i) => i.min(entries.len() - 1),
                    Err(crate::cloud_pc_picker::PickerError::Cancelled) => {
                        return Err("Cloud PC selection cancelled".into());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Cloud PC picker unavailable; using first entry");
                        0
                    }
                }
            } else {
                0
            };
            let chosen = &entries[choice];
            tracing::info!(
                id = %chosen.id,
                name = %chosen.display_name,
                "selected Cloud PC"
            );

            // The Reverse Connect gateway is the connection target; the Cloud PC
            // itself is reached through the gateway using the resource id.
            if !chosen.gateway_fqdn.is_empty() {
                config.reverse_connect = Some(rdp_core::ReverseConnectConfig {
                    gateway_fqdn: chosen.gateway_fqdn.clone(),
                    resource_id: chosen.resource_id.clone(),
                    tenant_id: chosen.tenant_id.clone(),
                    session_id: chosen.session_id.clone(),
                    access_token: token.token.clone(),
                    load_balance_info: chosen
                        .load_balance_info
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default(),
                    application_name: "Windows365NativeClient".to_string(),
                    // For cached Cloud PCs this is overwritten by apply_rdp_file
                    // below (from the .rdp's remoteapplicationprogram); for a live
                    // feed entry the resource id is the best available value.
                    remote_application: chosen.resource_id.clone(),
                    // The user's real logon password for the RDSTLS v3 credential
                    // (`--password`). The OAuth token overwrites `credentials`, so
                    // capture the account password separately here.
                    rdstls_password: args.password.clone().unwrap_or_default(),
                    // Resolved centrally below (default "AzureAD" unless --domain).
                    rdstls_domain: String::new(),
                });
            } else if !chosen.hostname.is_empty() {
                // Fallback for feeds that still expose a direct address.
                config.hostname = chosen.hostname.clone();
                config.port = chosen.port;
            } else {
                return Err("W365 feed entry has no gateway FQDN or hostname".into());
            }

            config.load_balance_info = chosen.load_balance_info.clone();
            if let Some(file) = &chosen.rdp_file {
                feed::apply_rdp_file(&mut config, file);
            }
            config.credentials.password = token.token;
            }

            // Default the RDSTLS logon username from the signed-in identity
            // (the id_token UPN) so the user need not also pass `--user`.
            if config.credentials.username.is_empty() {
                if let Some(upn) = token.username.as_ref() {
                    tracing::info!(upn, "defaulting W365 logon username from signed-in identity");
                    config.credentials.username = upn.clone();
                }
            }

            // Resolve the RDSTLS logon password the v3 credential encrypts:
            // an explicit `--password` wins (and is cached); otherwise reuse the
            // DPAPI-cached password; otherwise prompt once (hidden) and cache it.
            if config.reverse_connect.is_some() {
                if args.forget_password {
                    let _ = crate::password_cache::clear();
                }
                let account = config.credentials.username.clone();
                let pw = crate::resolve_rdstls_password(&account, args.password.as_deref());
                // Logon domain: explicit `--domain` wins (even empty); otherwise
                // default to "AzureAD" (pure-Entra Cloud PCs). Hybrid/AD-joined
                // hosts may need the AD domain or an empty string.
                let dom = args.domain.clone().unwrap_or_else(|| "AzureAD".to_string());
                // The post-RDSTLS Client Info (logon) PDU must carry the SAME real
                // credentials that satisfied RDSTLS — the OAuth token overwrote
                // `credentials.password` earlier, which the target rejects (the
                // gateway closes the tunnel right after the logon packet).
                config.credentials.password = pw.clone();
                config.credentials.domain = dom.clone();
                if let Some(rc) = config.reverse_connect.as_mut() {
                    rc.rdstls_password = pw;
                    rc.rdstls_domain = dom;
                }
            }
        } else if let Some(url) = &args.feed {
            // Generic RDWeb feed (non-W365).
            let entries = feed::fetch(url)?;
            if entries.is_empty() {
                return Err("feed returned no hosts".into());
            }
            tracing::info!(count = entries.len(), "discovered hosts from feed");
            for e in &entries {
                tracing::info!(id = %e.id, name = %e.display_name, host = %e.hostname, "feed entry");
            }
            let chosen = &entries[0];
            config.hostname = chosen.hostname.clone();
            config.port = chosen.port;
            config.load_balance_info = chosen.load_balance_info.clone();
            if let Some(file) = &chosen.rdp_file {
                feed::apply_rdp_file(&mut config, file);
            }
            if let Some(gw_host) = &chosen.gateway {
                let gw = gateway::GatewayConfig {
                    hostname: gw_host.clone(),
                    port: 443,
                    auth: gateway::GatewayAuth::Same,
                    bypass_for_local: false,
                };
                gateway::apply_to_config(&mut config, &gw);
                tracing::info!(gateway = %gw_host, "feed entry includes an RD Gateway (tunnel not yet implemented)");
            }
        }

        if config.hostname.is_empty() && config.reverse_connect.is_none() {
            return Err("no host: use --host or --feed or --w365".into());
        }

        // RDP multipathing (UDP multitransport). Advertising CS_MULTITRANSPORT
        // makes the host send an Initiate Multitransport Request (MS-RDPEMT); it's
        // advisory, so if we never bring UDP up the host simply stays on TCP.
        //   - Direct/feed hosts (`--udp`): dial the UDP side-band straight to the
        //     host (needs a direct `hostname:port`).
        //   - W365/AVD Reverse Connect (`--shortpath`): there is no direct UDP
        //     address, so the request is the trigger for the Shortpath path
        //     (TURN relay + ICE rendezvous, in progress); we advertise it to learn
        //     the request_id/cookie the Shortpath tunnel needs.
        let w365 = config.reverse_connect.is_some();
        config.multitransport = (args.udp && !w365) || (args.shortpath && w365);
        if args.udp && w365 && !args.shortpath {
            tracing::warn!(
                "--udp: classic UDP multitransport needs a direct host; over W365 \
                 Reverse Connect use --shortpath (RDP Shortpath) instead — staying on TCP"
            );
        }

        // Multi-monitor: enumerate the client's monitors, size the remote
        // desktop to the virtual-screen bounding box, advertise the layout
        // (CS_MONITOR), and remember the origin so we can place a borderless
        // window spanning every monitor. Degrades to the configured single
        // size if enumeration finds nothing.
        let mut span_origin: Option<(i32, i32)> = None;
        // Per-monitor present placements: (screen_x, screen_y, w, h, fb_off_x,
        // fb_off_y) for each physical monitor, when `--per-monitor` is set. The
        // remote desktop is the same spanned virtual desktop as `--multimon`;
        // only the client presentation differs (one window per monitor).
        let mut per_monitor_layout: Option<Vec<crate::MonitorPlacement>> = None;
        if args.multimon || args.per_monitor {
            let vd = crate::window::enumerate_monitors();
            if vd.size.0 > 0 && vd.size.1 > 0 {
                // RDP caps the logical desktop at 8192 per dimension; warn (but
                // still try) if a very wide/tall array exceeds it, since the
                // server may clamp or reject.
                if vd.size.0 > 8192 || vd.size.1 > 8192 {
                    tracing::warn!(
                        width = vd.size.0,
                        height = vd.size.1,
                        "virtual desktop exceeds the 8192px RDP limit; the server may clamp it"
                    );
                }
                config.width = vd.size.0.min(u16::MAX as u32) as u16;
                config.height = vd.size.1.min(u16::MAX as u32) as u16;
                config.monitors = vd.monitor_defs();
                span_origin = Some(vd.origin);
                tracing::info!(
                    monitors = config.monitors.len(),
                    width = config.width,
                    height = config.height,
                    origin = ?vd.origin,
                    per_monitor = args.per_monitor,
                    "multi-monitor: spanning the virtual desktop"
                );
                // Per-monitor mode needs each monitor's screen position, size and
                // offset within the spanned framebuffer to drive one window each.
                // The presented slice (`src`/`src_size`) starts native; the
                // render-scale block below swaps in the scaled slice rects.
                if args.per_monitor {
                    let placements: Vec<crate::MonitorPlacement> = vd
                        .rects
                        .iter()
                        .map(|r| {
                            let size = (
                                (r.right - r.left).max(1) as u32,
                                (r.bottom - r.top).max(1) as u32,
                            );
                            let off = (r.left - vd.origin.0, r.top - vd.origin.1);
                            crate::MonitorPlacement {
                                screen: (r.left, r.top),
                                size,
                                input_offset: off,
                                src: (off.0 as u32, off.1 as u32),
                                src_size: size,
                            }
                        })
                        .collect();
                    if !placements.is_empty() {
                        per_monitor_layout = Some(placements);
                    }
                }
            } else {
                tracing::warn!(
                    "multi-monitor requested but no monitors enumerated; using single-monitor size"
                );
            }
        } else if args.fullscreen {
            // Borderless fullscreen on the primary monitor: size the desktop to
            // it and place the window at its top-left. Single monitor, so no
            // CS_MONITOR block is advertised.
            if let Some(p) = crate::window::enumerate_monitors().primary_rect() {
                let (pw, ph) = ((p.right - p.left).max(1), (p.bottom - p.top).max(1));
                config.width = (pw as u32).min(u16::MAX as u32) as u16;
                config.height = (ph as u32).min(u16::MAX as u32) as u16;
                span_origin = Some((p.left, p.top));
                tracing::info!(
                    width = config.width,
                    height = config.height,
                    origin = ?(p.left, p.top),
                    "fullscreen: borderless on the primary monitor"
                );
            } else {
                tracing::warn!("fullscreen requested but no monitor enumerated; using a window");
            }
        }

        // Native window size — what we present at. `config.width/height` currently
        // equal this; the render-scale below shrinks the *remote desktop* the
        // server renders/encodes while the window stays native.
        let (width, height) = (config.width as u32, config.height as u32);

        // Client-side render-scale: ask the host to render a smaller desktop (huge
        // encode-cost win on a CPU-only host) and upscale it to the native window on
        // the client GPU at present time. Multi-monitor scales every monitor edge
        // through one consistent map (seams stay seams), advertises the scaled
        // CS_MONITOR layout, and upscales each monitor's slice on present.
        let (desktop_w, desktop_h) = if args.render_scale < 0.999 {
            if args.multimon || args.per_monitor {
                // Recover the exclusive-edge monitor rects from the native defs
                // set above, scale them consistently, and swap the scaled layout
                // into the connection config.
                let rects: Vec<rdp_pdu::gcc::VirtualScreenRect> = config
                    .monitors
                    .iter()
                    .map(|m| rdp_pdu::gcc::VirtualScreenRect {
                        left: m.left,
                        top: m.top,
                        right: m.right + 1,
                        bottom: m.bottom + 1,
                        primary: m.primary,
                    })
                    .collect();
                if rects.is_empty() {
                    (width, height)
                } else {
                    let (defs, (dw, dh), slices) =
                        crate::scale_monitor_layout(&rects, args.render_scale);
                    config.monitors = defs;
                    config.width = dw.min(u16::MAX as u32) as u16;
                    config.height = dh.min(u16::MAX as u32) as u16;
                    // Point each per-monitor present at its scaled slice.
                    if let Some(pl) = per_monitor_layout.as_mut() {
                        for (m, s) in pl.iter_mut().zip(&slices) {
                            m.src = s.0;
                            m.src_size = s.1;
                        }
                    }
                    tracing::info!(
                        scale = args.render_scale,
                        native_w = width,
                        native_h = height,
                        desktop_w = dw,
                        desktop_h = dh,
                        monitors = slices.len(),
                        "multimon render-scale: host renders a smaller spanned desktop; client GPU upscales each monitor"
                    );
                    (dw, dh)
                }
            } else {
                let (dw, dh) = crate::scaled_desktop_dims(width, height, args.render_scale);
                config.width = dw as u16;
                config.height = dh as u16;
                tracing::info!(
                    scale = args.render_scale,
                    window_w = width,
                    window_h = height,
                    desktop_w = dw,
                    desktop_h = dh,
                    "render-scale: host renders a smaller desktop; client GPU upscales"
                );
                (dw, dh)
            }
        } else {
            (width, height)
        };

        // First connection must succeed; the window + renderer are then reused
        // across any auto-reconnects. Retry a few times with exponential backoff so a
        // momentarily-unavailable server (rebooting, slow to listen) doesn't
        // abort the launch outright. If we have a persisted reconnect cookie from a
        // prior run, use it to resume the session without re-entering credentials.
        let first = {
            const INITIAL_RETRIES: u32 = 3;
            let mut attempt = 0;
            let persisted_cookie = crate::load_reconnect_cookie(&config.hostname);
            if persisted_cookie.is_some() {
                tracing::info!("found persisted reconnect cookie; will try to resume session");
            }
            loop {
                match connect::establish_reconnect(&mut config,
                    persisted_cookie.as_ref()
                ) {
                    Ok(c) => break c,
                    Err(e) if attempt < INITIAL_RETRIES => {
                        attempt += 1;
                        let delay = crate::reconnect_delay(attempt);
                        tracing::warn!(
                            error = %e,
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            "initial connect failed; retrying"
                        );
                        std::thread::sleep(delay);
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        // The primary window. In per-monitor mode it's the first monitor's
        // borderless window (extra monitors get their own windows below); the
        // single-window path is spanning or normal-resizable as before.
        let window = match per_monitor_layout.as_ref() {
            Some(p) => {
                let m = p[0];
                Window::new_monitor(
                    "RDPiO",
                    m.screen.0,
                    m.screen.1,
                    m.size.0,
                    m.size.1,
                    m.input_offset,
                )?
            }
            None => match span_origin {
                Some((x, y)) => Window::new_spanning("RDPiO", x, y, width, height)?,
                None => Window::new("RDPiO", width, height)?,
            },
        };
        // The primary swapchain is sized to the primary monitor in per-monitor
        // mode, else to the whole window.
        let (sc_w, sc_h) = match per_monitor_layout.as_ref() {
            Some(p) => p[0].size,
            None => (width, height),
        };
        // Per-monitor: one borderless window per physical monitor. They are all
        // created BEFORE the renderer so building the renderer is a pure function
        // of windows that already exist — which is what lets it be rebuilt
        // identically if the GPU device is lost. The windows outlive any such
        // rebuild (their HWNDs back the swapchains) for the whole connection.
        let mut extra_windows: Vec<crate::window::Window> = Vec::new();
        if let Some(placements) = per_monitor_layout.as_ref() {
            for m in &placements[1..] {
                extra_windows.push(Window::new_monitor(
                    "RDPiO",
                    m.screen.0,
                    m.screen.1,
                    m.size.0,
                    m.size.1,
                    m.input_offset,
                )?);
            }
            tracing::info!(
                monitors = placements.len(),
                "per-monitor windows: one window per physical monitor"
            );
        }
        let extra_hwnds: Vec<isize> = extra_windows.iter().map(|w| w.hwnd_raw()).collect();
        let mut renderer = build_renderer(
            window.hwnd_raw(),
            sc_w,
            sc_h,
            args,
            (desktop_w, desktop_h),
            per_monitor_layout.as_ref(),
            &extra_hwnds,
        )?;
        let per_monitor = per_monitor_layout.is_some();
        renderer.present_clear(SLATE)?;
        tracing::info!("window up; streaming desktop updates");

        // Capture the reserved system key combos the OS would otherwise swallow
        // (the Win keys, Alt+Tab/Alt+Shift+Tab, Alt+Esc, Ctrl+Esc) and forward them
        // to the remote session whenever an rdpio window is the foreground window —
        // in EVERY mode, so windowed sessions get Alt+Tab too. The hook self-gates
        // on our window being foreground, so clicking another local app restores
        // local Alt+Tab.
        crate::window::install_keyboard_hook();
        // The event the session worker signals when a frame is ready, so the UI
        // loop waits for work instead of polling on a timer.
        crate::window::init_wake_event();

        // Borderless modes have no window frame to close from: add a floating
        // connection bar (Pin + Disconnect) on the primary monitor.
        let conn_bar = if args.multimon || args.per_monitor || args.fullscreen {
            crate::window::enumerate_monitors()
                .primary_rect()
                .and_then(|p| crate::connbar::ConnBar::new(p.left, p.top, p.right - p.left).ok())
        } else {
            None
        };
        if span_origin.is_some() {
            tracing::info!("borderless mode — press Ctrl+Shift+Q to close the client");
            tracing::info!("Ctrl+Shift+M toggles mouse capture (confine cursor; relative aim for FPS games)");
        }

        // Decide the RDPGFX caps to advertise once (the device persists across
        // reconnects, so the GPU probe needn't repeat). Each worker advertises
        // this set when it opens the graphics channel.
        let gfx_caps = crate::gfx_caps_for(args, renderer.device_context_clone().as_ref());

        // Reconnect state carried across connections.
        let mut cookie: Option<rdp_pdu::logon::ReconnectCookie> = None;
        let mut pending = Some(first);
        let mut attempts = 0u32;
        const MAX_RECONNECT: u32 = 5;
        let mut client = (width, height); // current window client size, for scaling
        let mut desktop = (desktop_w, desktop_h); // current remote desktop size (resizable)
        let mut last_pos = (0u16, 0u16); // last pointer position (desktop pixels)
        // Windows digitizer contact ids (TOUCHINPUT.dwID) are arbitrary u32
        // driver cursor ids; RDPEI contact ids must be stable 0-255 slots.
        let mut touch_slots: std::collections::HashMap<u32, u8> = std::collections::HashMap::new();

        // Performance telemetry: shared across the UI, network, and decode threads.
        // Drained and logged every 10 seconds by the UI loop.
        let metrics = crate::metrics::Metrics::new();
        // GPU timestamp queries cost driver work on every present; only pay for
        // them when someone is actually reading the perf telemetry
        // (RUST_LOG=perf=debug). The queries themselves are already gated on
        // the callback being installed.
        let gpu_timing = tracing::enabled!(target: "perf", tracing::Level::DEBUG);
        if gpu_timing {
            let m = metrics.clone();
            renderer.set_gpu_timing_callback(Some(Box::new(move |label: &str, us: u64| {
                if label == "present" {
                    m.record_gpu_present_us(us);
                }
            })));
        }
        // Network-change listener: wakes reconnect retries immediately when an
        // interface comes back up, instead of waiting out the backoff.
        let net_change_rx = net_listener::subscribe();

        // Set when a renderer call reports the GPU device is gone, so the next
        // pass through `'session` rebuilds it before reconnecting.
        let mut device_lost = false;

        'session: loop {
            // A lost GPU device invalidates the swapchain, the framebuffer and
            // every decoder bound to it, so rebuild before touching the network.
            // Rebuilding can itself fail for a while — after a resume the adapter
            // may not be back yet — so retry on the same backoff the network path
            // uses rather than giving up on the first attempt.
            if device_lost {
                let mut tries = 0u32;
                loop {
                    window.set_title("Reconnecting… — RDPiO");
                    match build_renderer(
                        window.hwnd_raw(),
                        sc_w,
                        sc_h,
                        args,
                        desktop,
                        per_monitor_layout.as_ref(),
                        &extra_hwnds,
                    ) {
                        Ok(mut r) => {
                            if gpu_timing {
                                let m = metrics.clone();
                                r.set_gpu_timing_callback(Some(Box::new(
                                    move |label: &str, us: u64| {
                                        if label == "present" {
                                            m.record_gpu_present_us(us);
                                        }
                                    },
                                )));
                            }
                            renderer = r;
                            device_lost = false;
                            tracing::info!(attempts = tries, "GPU device rebuilt");
                            break;
                        }
                        Err(e) if tries < MAX_RECONNECT => {
                            tries += 1;
                            let delay = reconnect_delay(tries);
                            tracing::warn!(
                                error = %e,
                                attempt = tries,
                                delay_ms = delay.as_millis() as u64,
                                "GPU device not available yet; retrying"
                            );
                            std::thread::sleep(delay);
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "could not rebuild the GPU device");
                            return Err(e);
                        }
                    }
                }
            }
            // Obtain a connection: the first iteration uses the one we already
            // established; later iterations reconnect (with the server cookie).
            let conn = match pending.take() {
                Some(c) => {
                    attempts = 0;
                    c
                }
                None => match connect::establish_reconnect(&mut config, cookie.as_ref()) {
                    Ok(c) => {
                        attempts = 0;
                        c
                    }
                    Err(e) => {
                        attempts += 1;
                        if cookie.is_some() && attempts <= MAX_RECONNECT {
                            let delay = reconnect_delay(attempts);
                            tracing::warn!(
                                error = %e,
                                attempt = attempts,
                                delay_ms = delay.as_millis() as u64,
                                "reconnect failed; retrying"
                            );
                            if net_listener::wait_with_network_wake(delay, &net_change_rx) {
                                tracing::info!("network change detected; retrying reconnect immediately");
                            }
                            window.set_title("Reconnecting… — RDPiO");
                            continue 'session;
                        }
                        window.set_title("RDPiO");
                        tracing::info!(error = %e, "auto-reconnect exhausted; window stays open");
                        idle_until_close(&window, &mut renderer)?;
                        break 'session;
                    }
                },
            };

            let connect::Established {
                transport,
                mut session,
                control,
                input_tcp,
                protocol,
            } = conn;
            tracing::info!(?protocol, info = ?session.info(), "RDP session ACTIVE");
            // Unconditional: the title also says "Reconnecting…" after a GPU
            // device rebuild, which can reach here with `attempts` still zero.
            window.set_title("RDPiO");

            // Enhanced-security transports carry EGFX graphics: direct TLS *and*
            // the W365/AVD RDSTLS-over-WebSocket tunnel. Both must take the graphics
            // path (decode thread + `run_graphics_session`); otherwise the desktop
            // never decodes and the screen stays black.
            let graphics_path = matches!(
                transport,
                connect::Transport::Tls(_) | connect::Transport::WebSocketTls(_)
            );
            // Dial info for the UDP side-band, captured by the worker. Gated on
            // `config.multitransport` — the exact condition under which we
            // advertised the transport in the GCC block — so we only ever dial a
            // side-band we told the server we'd bring up. It is restricted to the
            // direct TLS host (a real `host:port`); the Reverse Connect / WebSocket
            // transports have no direct address to dial, so they never bring up UDP.
            let direct_tls = matches!(transport, connect::Transport::Tls(_));
            let udp_dial = (config.multitransport && direct_tls).then(|| crate::udp::UdpDial {
                server: format!("{}:{}", config.hostname, config.port),
                hostname: config.hostname.clone(),
                accept_invalid_cert: config.allow_invalid_certificate,
                debug: args.udp_debug,
            });
            let mut input = input_tcp.map(|c| session.take_input_sender(c));
            let (input_tx, input_rx) = mpsc::channel::<Vec<inpdu::EventBytes>>();
            if let Some(sender) = input.as_mut() {
                let _ = sender.send(&[inpdu::sync_event(0)]);
            } else if graphics_path {
                let _ = input_tx.send(vec![inpdu::sync_event(0)]);
            }
            let shutdown = control;
            session.set_clipboard_provider(Box::new(
                crate::clipboard::Win32Clipboard::new()
                    .with_download_dir(args.clipboard_dir.clone().map(std::path::PathBuf::from))
                    // The window owns the clipboard so Windows can ask it to
                    // render the session's files (WM_RENDERFORMAT) on a paste.
                    .with_owner(window.hwnd_raw()),
            ));
            session.set_audio_sink(Box::new(crate::audio::Win32Audio::new()));
            if args.printer {
                match crate::printer::Win32Printer::default_printer() {
                    Some((name, driver, sink)) => {
                        tracing::info!(%name, %driver, "redirecting local default printer");
                        session.set_printer(name, driver, Box::new(sink));
                    }
                    None => tracing::warn!("--printer set but no default printer found"),
                }
            }

            // Share the D3D11 device/context with the worker so its H.264 path
            // can decode on the GPU (DXVA, zero-copy) into the same device.
            let gpu_device = renderer.device_context_clone();
            let gfx_caps = gfx_caps.clone();
            let no_seed = args.no_seed;
            let teams = args.teams;
            let teams_native = args.teams_native;
            let (tx, rx) = mpsc::channel::<FrameMsg>();
            let worker_metrics = metrics.clone();
            // Explicit stop flag so closing the window unblocks the worker on
            // EVERY transport. The reverse-connect/WebSocket path leaves
            // `control` (the shutdown socket) as None, so without this the worker
            // polls forever and `worker.join()` below hangs — the process outlives
            // the window. Set by the UI thread before the join.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_worker = stop.clone();
            // Event-driven waits for the worker where the transport exposes its
            // raw socket (direct TCP/TLS). Registering switches the socket to
            // non-blocking event notification — the TLS read/write paths ride
            // that out — and drops the worker's idle wakeups from 60-1000/s
            // (the old 1 ms poll, a steady battery cost) to ~2/s. Input still
            // ships instantly: every producer signals the worker's wake event.
            let sock_wait = if graphics_path {
                transport.raw_socket().and_then(|s| {
                    match crate::net_wait::SocketWait::new(s) {
                        Ok(w) => Some(w),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "socket event registration failed; using timeout pacing"
                            );
                            None
                        }
                    }
                })
            } else {
                None
            };
            let worker = std::thread::spawn(move || {
                let mut session = session;
                let mut transport = transport;
                let result = if graphics_path {
                    if sock_wait.is_none() {
                        // No waitable socket (WebSocket paths): fall back to the
                        // 1 ms read-timeout poll so queued input still ships
                        // promptly between reads.
                        if let Err(e) =
                            transport.set_read_timeout(Some(Duration::from_millis(1)))
                        {
                            tracing::warn!(error = %e, "could not set input poll timeout; input may lag");
                        }
                    }
                    // Decode/composite runs on its OWN thread so decode time never
                    // blocks the network read + frame-ack loop. The heavy stateful
                    // renderer lives there; this (network) thread keeps a cheap
                    // clone of the frame sink for bitmap/cursor/cookie updates and
                    // hands EGFX command batches over `decode_tx`. `backlog` is the
                    // decode queue depth, reported to the server as the frame-ack
                    // queueDepth for flow control.
                    let (decode_tx, decode_rx) =
                        mpsc::channel::<Vec<rdp_pdu::gfx::GfxCommand>>();
                    let backlog = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let decode_backlog = backlog.clone();
                    let decode_sink_tx = tx.clone();
                    let decode_metrics = worker_metrics.clone();
                    let decode = std::thread::spawn(move || {
                        let mut renderer = MfRenderer::new(width, height, gpu_device);
                        renderer.seed_disabled = no_seed;
                        if no_seed {
                            tracing::info!("--no-seed: ClearCodec tiles decode from black");
                        }
                        let mut decode_sink = ChannelSink {
                            tx: decode_sink_tx,
                            metrics: Some(decode_metrics.clone()),
                        };
                        session::run_decode_loop(
                            decode_rx,
                            &mut renderer,
                            &mut decode_sink,
                            decode_backlog,
                            Some(decode_metrics),
                        );
                    });
                    let mut net_sink = ChannelSink {
                        tx,
                        metrics: Some(worker_metrics.clone()),
                    };
                    // Open the microphone for audio-input redirection. If no
                    // capture device is available, run without one (the server
                    // simply gets no mic).
                    let mut mic = crate::mic::Win32Mic::new();
                    let mic_ref = mic
                        .as_mut()
                        .map(|m| m as &mut dyn session::MicSource);
                    // Teams "Optimized": bridge the `com.microsoft.rdc.dvc.webrtc.1`
                    // channel client-side instead of declining it. Two backends:
                    //   --teams-native → rdpio's own webrtc-rs engine (no MS binary;
                    //                    the path that also runs on Linux),
                    //   --teams        → host Microsoft's MsRdcWebRTCAddIn.dll.
                    // Native wins if both are set. None (→ decline) if neither is
                    // requested or the chosen backend fails to come up.
                    let redirector: Option<Box<dyn rdp_graphics::redirect::DvcRedirector>> =
                        if teams_native {
                            tracing::info!("--teams-native: bringing up rdpio's native WebRTC engine");
                            let r = crate::webrtc_native::NativeWebRtcRedirector::new();
                            tracing::info!(active = r.is_some(), "native Teams WebRTC engine status");
                            r.map(|r| Box::new(r) as Box<dyn rdp_graphics::redirect::DvcRedirector>)
                        } else if teams {
                            tracing::info!("--teams: bringing up the Teams WebRTC redirector bridge");
                            let r = crate::webrtc_addin::WebRtcRedirector::new();
                            tracing::info!(active = r.is_some(), "Teams WebRTC redirector bridge status");
                            r.map(|r| Box::new(r) as Box<dyn rdp_graphics::redirect::DvcRedirector>)
                        } else {
                            tracing::info!(
                                "Teams WebRTC redirector not requested \
                                 (pass --teams-native for the native engine, or --teams for the DLL)"
                            );
                            None
                        };
                    let result = session::run_graphics_session(
                        &mut transport,
                        &mut session,
                        &mut net_sink,
                        &input_rx,
                        mic_ref,
                        udp_dial,
                        gfx_caps,
                        decode_tx,
                        backlog,
                        Some(worker_metrics),
                        &stop_worker,
                        redirector,
                        sock_wait,
                    );
                    // `run_graphics_session` returning drops `decode_tx`, so the
                    // decode thread's `recv()` ends; join it before we exit.
                    let _ = decode.join();
                    result
                } else {
                    let mut sink = ChannelSink {
                        tx,
                        metrics: Some(worker_metrics.clone()),
                    };
                    session::run_session(&mut transport, &mut session, &mut sink)
                };
                if let Err(err) = result {
                    if !graphics_path {
                        // Legacy Standard RDP Security. A sudden server reset with
                        // no preceding Set Error Info usually means the RC4
                        // keystream / MAC desynced (the server tore the connection
                        // down rather than explaining why).
                        tracing::info!(
                            error = %err,
                            "session ended (Standard RDP Security); a silent reset here often \
                             means an RC4/MAC desync rather than a clean disconnect"
                        );
                    } else {
                        tracing::info!(error = %err, "session ended");
                    }
                }
            });

            // Inner UI loop for this connection.
            let mut window_closed = false;
            let mut reconnect = false;
            let mut worker_alive = true;
            // Pixel blits + frame markers received but not yet applied. We only
            // apply through the last complete frame boundary each pump cycle, so
            // a half-arrived next frame is never presented; its leading blits
            // wait here for their own EndFrame. Per-connection (reset on reconnect).
            let mut pending: Vec<FrameMsg> = Vec::new();
            // Opt-in frame pacing (--pace <fps>): present on an even cadence,
            // always the newest frame, to smooth uneven arrival. Off → present ASAP.
            let pace_interval = (args.pace > 0)
                .then(|| std::time::Duration::from_secs_f32(1.0 / args.pace as f32));
            let mut last_present = std::time::Instant::now();
            let mut pending_present = false;
            let mut metrics_report_start = std::time::Instant::now();
            let mut activity_deadline = std::time::Instant::now();
            loop {
                match window.pump() {
                    Frame::Quit => {
                        window_closed = true;
                        break;
                    }
                    Frame::Continue { resize } => {
                        if let Some(bar) = conn_bar.as_ref() {
                            bar.tick();
                        }
                        let mut dirty = false;
                        // Per-monitor windows are fixed-size borderless surfaces;
                        // their (creation-time) WM_SIZE must not resize the primary
                        // swapchain or rescale input — the framebuffer is the whole
                        // desktop and input is already in absolute desktop space.
                        if let (Some((w, h)), false) = (resize, per_monitor) {
                            if let Err(e) = renderer.resize(w, h) {
                                // Same policy as presenting below: no GPU error
                                // here is worth killing the app over — rebuild
                                // and reconnect (bounded by MAX_RECONNECT).
                                if is_device_lost(&e) {
                                    tracing::warn!(
                                        error = %e,
                                        "GPU device lost while resizing; rebuilding and reconnecting"
                                    );
                                } else {
                                    tracing::error!(
                                        error = %e, w, h,
                                        "swapchain resize failed (not device-lost); rebuilding GPU and reconnecting"
                                    );
                                }
                                // Leaving the loop with `window_closed` false is
                                // what routes us back into the reconnect path.
                                device_lost = true;
                                break;
                            }
                            client = (w, h);
                            // Ask the server to resize the remote desktop to match
                            // the new window (Display Control; ignored if the
                            // server didn't open that channel) — scaled down by the
                            // render-scale, so the host keeps encoding fewer pixels.
                            // Spanned multimon keeps its fixed CS_MONITOR layout: a
                            // single-monitor resize request would collapse it.
                            if !args.multimon {
                                let (rw, rh) = if args.render_scale < 0.999 {
                                    crate::scaled_desktop_dims(w, h, args.render_scale)
                                } else {
                                    (w, h)
                                };
                                crate::session::request_resize(vec![rdp_pdu::gcc::MonitorDef {
                                    left: 0,
                                    top: 0,
                                    right: rw as i32 - 1,
                                    bottom: rh as i32 - 1,
                                    primary: true,
                                }]);
                            }
                            dirty = true;
                            activity_deadline = std::time::Instant::now() + ACTIVITY_WINDOW;
                        }
                        if worker_alive {
                            let rel_capture = crate::window::capture_mode()
                                && crate::session::rel_mouse_supported();
                            let mut touches: Vec<rdp_channels::rdpei::RdpInputContact> = Vec::new();
                            let events: Vec<inpdu::EventBytes> = window
                                .drain_input()
                                .into_iter()
                                .filter_map(|raw| {
                                    if let RawInput::Touch { id, x, y, phase } = raw {
                                        let slot = touch_slot(&mut touch_slots, id, phase);
                                        touches.push(touch_to_contact(slot, x, y, phase, desktop, client));
                                        return None;
                                    }
                                    map_input(raw, desktop, client, &mut last_pos, rel_capture)
                                })
                                .collect();
                            let events = coalesce_input_events(events);
                            let had_input = !events.is_empty() || !touches.is_empty();
                            if !events.is_empty() {
                                if let Some(sender) = input.as_mut() {
                                    if let Err(err) = sender.send(&events) {
                                        tracing::debug!(error = %err, "input send failed");
                                    }
                                } else if graphics_path && input_tx.send(events).is_err() {
                                    tracing::debug!("input worker gone; dropping input");
                                } else {
                                    // Wake the (event-driven) worker so the
                                    // queued input ships immediately.
                                    crate::net_wait::worker_wake::signal();
                                }
                            }
                            if !touches.is_empty() {
                                crate::session::queue_touch(touches);
                            }
                            if had_input {
                                activity_deadline = std::time::Instant::now() + ACTIVITY_WINDOW;
                            }
                            // Drain everything available. Cursor/cookie are frame-
                            // independent (a static desktop still moves its cursor),
                            // so apply them at once; pixel blits + frame markers are
                            // queued for boundary-gated application below.
                            loop {
                                match rx.try_recv() {
                                    Ok(FrameMsg::Cursor(update)) => window.set_cursor(update),
                                    Ok(FrameMsg::Cookie(c)) => {
                                    cookie = Some(c);
                                    if let Err(e) = save_reconnect_cookie(&config.hostname, &c) {
                                        tracing::warn!(error = %e, "failed to persist reconnect cookie");
                                    }
                                }
                                    Ok(msg) => pending.push(msg),
                                    Err(TryRecvError::Empty) => break,
                                    Err(TryRecvError::Disconnected) => {
                                        worker_alive = false;
                                        // Reconnect if the server gave us a cookie;
                                        // otherwise keep the window open until close.
                                        reconnect = cookie.is_some();
                                        tracing::info!(reconnect, "session worker finished");
                                        break;
                                    }
                                }
                            }
                            // Apply only through the last complete frame boundary
                            // (Present / Resize). Blits after it belong to a frame
                            // still arriving, so they stay in `pending` until their
                            // EndFrame — we never present a torn, half-updated frame.
                            if let Some(boundary) = pending
                                .iter()
                                .rposition(|m| matches!(m, FrameMsg::Present | FrameMsg::Resize(..)))
                            {
                                for msg in pending.drain(..=boundary) {
                                    match msg {
                                        FrameMsg::Blit { x, y, w, h, rgba } => {
                                            renderer.update_rect(x, y, w, h, &rgba);
                                        }
                                        FrameMsg::BlitNv12 { x, y, w, h, nv12, rects } => {
                                            // GPU color-convert; fall back to CPU so
                                            // the frame is never dropped. Only the
                                            // dirty region rects are painted.
                                            let regions = region_tuples_u32(&rects);
                                            if !renderer.blit_nv12(
                                                x as u32, y as u32, w as u32, h as u32, &nv12,
                                                &regions,
                                            ) {
                                                let (yp, uv) =
                                                    nv12.split_at((w as usize) * (h as usize));
                                                if let Some(rgba) = rdp_graphics::yuv::nv12_to_rgba(
                                                    yp, uv, w as usize, h as usize, w as usize,
                                                ) {
                                                    blit_rgba_regions(
                                                        &mut renderer, x, y, w, h, &rgba, &rects,
                                                    );
                                                }
                                            }
                                        }
                                        FrameMsg::BlitTexture { x, y, w, h, texture, rects } => {
                                            // Zero-copy GPU NV12 texture color-convert.
                                            let regions = region_tuples_u32(&rects);
                                            if !renderer.blit_texture(
                                                x as u32, y as u32, w as u32, h as u32, &texture,
                                                &regions,
                                            ) {
                                                // The video processor refused the
                                                // frame. Read it back and convert on
                                                // the CPU rather than dropping it:
                                                // discarding the frame is what turned
                                                // one rejected blit into a permanently
                                                // blank screen (and, where another
                                                // codec was also painting, text that
                                                // never finished refreshing).
                                                if let Some(nv12) =
                                                    renderer.read_nv12(&texture, w as u32, h as u32)
                                                {
                                                    let (yp, uv) =
                                                        nv12.split_at((w as usize) * (h as usize));
                                                    if let Some(rgba) = rdp_graphics::yuv::nv12_to_rgba(
                                                        yp, uv, w as usize, h as usize, w as usize,
                                                    ) {
                                                        blit_rgba_regions(
                                                            &mut renderer, x, y, w, h, &rgba, &rects,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        FrameMsg::CopyRect { sx, sy, w, h, dx, dy } => {
                                            renderer.copy_rect(sx, sy, w, h, dx, dy);
                                        }
                                        FrameMsg::CacheRect { slot, sx, sy, w, h } => {
                                            renderer.cache_rect(slot, sx, sy, w, h);
                                        }
                                        FrameMsg::CacheBlit { slot, dx, dy } => {
                                            renderer.cache_blit(slot, dx, dy);
                                        }
                                        FrameMsg::Present => dirty = true,
                                        FrameMsg::Resize(w, h) => {
                                            // Remote desktop resized; match the
                                            // framebuffer + input scaling to it.
                                            let _ =
                                                renderer.ensure_framebuffer(w as u32, h as u32);
                                            desktop = (w as u32, h as u32);
                                            dirty = true;
                                        }
                                        // Handled during drain; never queued.
                                        FrameMsg::Cursor(_) | FrameMsg::Cookie(_) => {}
                                    }
                                }
                            }
                        }
                        if reconnect {
                            break;
                        }
                        // Carry "a frame is ready to show" across iterations so a
                        // paced present deferred this tick still fires on a later one.
                        if dirty {
                            pending_present = true;
                            activity_deadline = std::time::Instant::now() + ACTIVITY_WINDOW;
                        }
                        let pace_ready =
                            pace_interval.map_or(true, |iv| last_present.elapsed() >= iv);
                        if pending_present && pace_ready {
                            let present_start = std::time::Instant::now();
                            if let Err(e) = renderer.present_frame() {
                                // Don't take the session down with the GPU:
                                // drop this connection, rebuild the renderer,
                                // and let the reconnect path resume from the
                                // cookie. This used to be device-lost-only,
                                // and any other present error killed the whole
                                // app; a presentation failure never deserves
                                // that (bounded by MAX_RECONNECT regardless).
                                if is_device_lost(&e) {
                                    tracing::warn!(
                                        error = %e,
                                        "GPU device lost while presenting; rebuilding and reconnecting"
                                    );
                                } else {
                                    tracing::error!(
                                        error = %e,
                                        "present failed (not device-lost); rebuilding GPU and reconnecting — see the preceding rdp_gpu error for the failing call"
                                    );
                                }
                                // Leaving the loop with `window_closed` false is
                                // what routes us back into the reconnect path.
                                device_lost = true;
                                break;
                            }
                            let present_us = present_start.elapsed().as_micros() as u64;
                            let interval_us = last_present.elapsed().as_micros() as u64;
                            metrics.record_present_us(present_us);
                            metrics.record_frame_interval_us(interval_us);
                            maybe_dump_gpu_framebuffer(&mut renderer);
                            pending_present = false;
                            last_present = std::time::Instant::now();
                        }

                        // Drain and log telemetry every 10 seconds.
                        if metrics_report_start.elapsed() >= Duration::from_secs(10) {
                            let report = metrics.report_and_reset();
                            if report.has_data() {
                                tracing::info!(target: "perf", "{}", report.summary());
                            }
                            metrics_report_start = std::time::Instant::now();
                        }

                        if !(pending_present && pace_ready) {
                            // Wait for actual work rather than polling: the worker's
                            // event fires the moment a frame is queued and window
                            // messages wake us for input, so neither waits out a
                            // sleep interval. The timeout only bounds time-based
                            // work — the next paced present, else the telemetry
                            // tick — so an idle session costs a couple of wakeups
                            // a second instead of a thousand.
                            let timeout_ms = match (pending_present, pace_interval) {
                                (true, Some(iv)) => {
                                    let left = iv.saturating_sub(last_present.elapsed());
                                    (left.as_millis() as u32).clamp(1, 100)
                                }
                                _ => {
                                    let active = args.low_latency
                                        || pending_present
                                        || activity_deadline > std::time::Instant::now();
                                    if active {
                                        100
                                    } else {
                                        250
                                    }
                                }
                            };
                            window.wait_for_work(timeout_ms);
                        }
                    }
                }
            }

            // Tear down this connection's worker.
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            crate::net_wait::worker_wake::signal(); // unblock an event wait
            if let Some(s) = &shutdown {
                let _ = s.shutdown(Shutdown::Both); // unblock the worker's read
            }
            let _ = worker.join();

            if window_closed {
                break 'session;
            }
            // Otherwise the worker dropped with a cookie → loop to reconnect.
            tracing::info!("attempting auto-reconnect…");
        }

        tracing::info!("window closed; shutting down");
        Ok(())
    }

    /// Keep the window responsive (and showing the last frame) until the user
    /// closes it — used after a disconnect we can't (or won't) reconnect.
    ///
    /// Nothing animates here, so block on the message queue instead of pumping
    /// on a 16 ms sleep: the flip-model swapchain retains its last present, so
    /// DWM keeps compositing the final frame without our help. Re-presenting an
    /// unchanged frame at ~60 fps only kept the CPU/GPU awake (battery) for a
    /// dead session. A present is still needed after a resize, which rebuilds
    /// the backbuffer.
    fn idle_until_close(
        window: &Window,
        renderer: &mut Renderer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            match window.pump() {
                Frame::Quit => return Ok(()),
                Frame::Continue { resize } => {
                    if let Some((w, h)) = resize {
                        renderer.resize(w, h)?;
                        renderer.present_frame()?;
                    }
                    window.wait_for_work(1_000);
                }
            }
        }
    }

    /// Frame-relative dirty rects widened to the u32 tuples the GPU blit API takes.
    fn region_tuples_u32(rects: &[(u16, u16, u16, u16)]) -> Vec<(u32, u32, u32, u32)> {
        rects
            .iter()
            .map(|&(x, y, w, h)| (x as u32, y as u32, w as u32, h as u32))
            .collect()
    }

    /// CPU fallback for an NV12 frame that could not be GPU-converted: upload the
    /// converted RGBA, clipped to the dirty region rects (whole frame when none) —
    /// outside them the decoded picture is stale encoder reference content.
    fn blit_rgba_regions(
        renderer: &mut Renderer,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        rgba: &[u8],
        rects: &[(u16, u16, u16, u16)],
    ) {
        if rects.is_empty() {
            renderer.update_rect(x, y, w, h, rgba);
            return;
        }
        for &(rx, ry, rw, rh) in rects {
            if rx >= w || ry >= h {
                continue;
            }
            let cw = rw.min(w - rx) as usize;
            let ch = rh.min(h - ry) as usize;
            if cw == 0 || ch == 0 {
                continue;
            }
            let mut cropped = Vec::with_capacity(cw * ch * 4);
            for row in 0..ch {
                let start = ((ry as usize + row) * w as usize + rx as usize) * 4;
                cropped.extend_from_slice(&rgba[start..start + cw * 4]);
            }
            renderer.update_rect(x + rx, y + ry, cw as u16, ch as u16, &cropped);
        }
    }

    /// Coalesce consecutive mouse-move events into the latest one. A high-DPI
    /// gaming mouse can generate 1000+ Hz of tiny motion PDUs; collapsing them
    /// cuts network traffic and server-side pointer work without affecting final
    /// position. Button/wheel/key events break the coalescing chain so they are
    /// never dropped.
    fn coalesce_input_events(events: Vec<inpdu::EventBytes>) -> Vec<inpdu::EventBytes> {
        const MOUSE_MOVE_TYPES: [u16; 2] = [inpdu::INPUT_EVENT_MOUSE, inpdu::INPUT_EVENT_MOUSEREL];
        let mut out: Vec<inpdu::EventBytes> = Vec::with_capacity(events.len());
        for e in events {
            let msg_type = u16::from_le_bytes([e[4], e[5]]);
            let flags = u16::from_le_bytes([e[6], e[7]]);
            let is_move = MOUSE_MOVE_TYPES.contains(&msg_type)
                && (flags & inpdu::PTRFLAGS_MOVE) != 0
                && (flags & !(inpdu::PTRFLAGS_MOVE)) == 0;
            if let Some(last) = out.last_mut() {
                let last_type = u16::from_le_bytes([last[4], last[5]]);
                if is_move && last_type == msg_type {
                    *last = e;
                    continue;
                }
            }
            out.push(e);
        }
        out
    }

    /// Map a Windows digitizer contact id to a stable RDPEI slot (0-255): the
    /// lowest slot not held by another live contact, held from down to up.
    /// MS-RDPEI contact ids are a single byte, and servers track the contact
    /// state machine per id — truncating the raw driver id could collide two
    /// simultaneous contacts or break down/up pairing.
    fn touch_slot(slots: &mut std::collections::HashMap<u32, u8>, id: u32, phase: u8) -> u8 {
        if let Some(&s) = slots.get(&id) {
            if phase == 1 {
                slots.remove(&id);
            }
            return s;
        }
        let mut slot = 0u8;
        while slots.values().any(|&v| v == slot) && slot < u8::MAX {
            slot += 1;
        }
        // An up for an unknown contact still reports the computed slot; it is
        // not retained, so a stray up cannot leak a slot.
        if phase != 1 {
            slots.insert(id, slot);
        }
        slot
    }

    /// Map a multi-touch contact from window client pixels to RDPEI virtual-
    /// desktop coordinates and contact-state flags.
    fn touch_to_contact(
        slot: u8,
        x: i32,
        y: i32,
        phase: u8,
        desktop: (u32, u32),
        client: (u32, u32),
    ) -> rdp_channels::rdpei::RdpInputContact {
        use rdp_channels::rdpei::{
            CONTACT_FLAG_DOWN, CONTACT_FLAG_INCONTACT, CONTACT_FLAG_INRANGE,
            CONTACT_FLAG_UP, CONTACT_FLAG_UPDATE,
        };
        let cw = client.0.max(1);
        let ch = client.1.max(1);
        let dx = (x.max(0) as u32).saturating_mul(desktop.0) / cw;
        let dy = (y.max(0) as u32).saturating_mul(desktop.1) / ch;
        let flags = match phase {
            0 => CONTACT_FLAG_DOWN | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
            // UP alone: UP|INRANGE is the "lifted but still hovering" pen
            // transition, which is invalid for a touch digitizer's end-of-
            // contact and gets the frame rejected by strict servers.
            1 => CONTACT_FLAG_UP,
            _ => CONTACT_FLAG_UPDATE | CONTACT_FLAG_INRANGE | CONTACT_FLAG_INCONTACT,
        };
        rdp_channels::rdpei::RdpInputContact {
            id: slot,
            x: dx as i32,
            y: dy as i32,
            flags,
        }
    }

    /// Translate one captured event into an RDP input event, scaling pointer
    /// coordinates from window client pixels to desktop pixels. `last_pos` holds
    /// the most recent pointer position (used for wheel events, which carry no
    /// coordinates of their own). With `rel_capture` (mouse-capture mode on a
    /// relative-mouse-capable server) motion and buttons go out as
    /// TS_RELPOINTER_EVENTs, so the remote pointer moves by deltas and FPS
    /// aiming cannot pin at an edge.
    fn map_input(
        raw: RawInput,
        desktop: (u32, u32),
        client: (u32, u32),
        last_pos: &mut (u16, u16),
        rel_capture: bool,
    ) -> Option<inpdu::EventBytes> {
        let scale = |x: i32, y: i32| -> (u16, u16) {
            let cw = client.0.max(1);
            let ch = client.1.max(1);
            let dx = (x.max(0) as u32).saturating_mul(desktop.0) / cw;
            let dy = (y.max(0) as u32).saturating_mul(desktop.1) / ch;
            (
                dx.min(desktop.0.saturating_sub(1)) as u16,
                dy.min(desktop.1.saturating_sub(1)) as u16,
            )
        };
        match raw {
            RawInput::Key {
                scancode,
                extended,
                down,
            } => {
                if scancode == 0 {
                    return None;
                }
                let mut flags = 0;
                if extended {
                    flags |= inpdu::KBDFLAGS_EXTENDED;
                }
                if !down {
                    flags |= inpdu::KBDFLAGS_RELEASE;
                }
                Some(inpdu::keyboard_event(flags, scancode))
            }
            RawInput::MouseMove { x, y } => {
                let p = scale(x, y);
                *last_pos = p;
                Some(inpdu::mouse_event(inpdu::PTRFLAGS_MOVE, p.0, p.1))
            }
            RawInput::MouseRel { dx, dy } => {
                let clamp16 = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                Some(inpdu::rel_mouse_event(
                    inpdu::PTRFLAGS_MOVE,
                    clamp16(dx),
                    clamp16(dy),
                ))
            }
            RawInput::MouseButton { button, x, y, down } => {
                let bflag = match button {
                    0 => inpdu::PTRFLAGS_BUTTON1,
                    1 => inpdu::PTRFLAGS_BUTTON2,
                    2 => inpdu::PTRFLAGS_BUTTON3,
                    _ => return None,
                };
                let flags = bflag | if down { inpdu::PTRFLAGS_DOWN } else { 0 };
                // Relative capture: a button click must not teleport the remote
                // pointer to the (parked) local cursor position — send it as a
                // zero-delta relative event instead.
                if rel_capture {
                    return Some(inpdu::rel_mouse_event(flags, 0, 0));
                }
                let p = scale(x, y);
                *last_pos = p;
                Some(inpdu::mouse_event(flags, p.0, p.1))
            }
            RawInput::XButton { button, x, y, down } => {
                let bflag = match button {
                    0 => inpdu::PTRXFLAGS_BUTTON1, // back
                    1 => inpdu::PTRXFLAGS_BUTTON2, // forward
                    _ => return None,
                };
                let flags = bflag | if down { inpdu::PTRXFLAGS_DOWN } else { 0 };
                // No relative form exists for extended buttons; in capture mode
                // reuse the last sent position rather than the parked cursor.
                let p = if rel_capture { *last_pos } else { scale(x, y) };
                *last_pos = p;
                Some(inpdu::mouse_x_event(flags, p.0, p.1))
            }
            RawInput::MouseWheel { delta } => {
                // RDP's wheel rotation is a 9-bit *signed* field (WheelRotationMask
                // 0x01FF); PTRFLAGS_WHEEL_NEGATIVE (0x0100) is its sign bit, so a
                // negative rotation is the two's-complement low 9 bits — NOT the
                // positive magnitude with the sign bit OR'd on. The latter sends
                // |delta|-256, a runaway negative for the small high-resolution
                // deltas modern mice/touchpads emit (the "extremely fast
                // down-scroll" bug); up-scroll happened to be correct because its
                // magnitude was used directly.
                let units = (delta.clamp(-255, 255) as u16) & inpdu::PTRFLAGS_WHEEL_ROTATION_MASK;
                Some(inpdu::mouse_event(inpdu::PTRFLAGS_WHEEL | units, last_pos.0, last_pos.1))
            }
            RawInput::MouseHWheel { delta } => {
                let units = (delta.clamp(-255, 255) as u16) & inpdu::PTRFLAGS_WHEEL_ROTATION_MASK;
                Some(inpdu::mouse_event(inpdu::PTRFLAGS_HWHEEL | units, last_pos.0, last_pos.1))
            }
            RawInput::Char { code, down } => {
                // Unicode keyboard event (IME-composed text); release flag on up.
                let flags = if down { 0 } else { inpdu::KBDFLAGS_RELEASE };
                Some(inpdu::unicode_event(flags, code))
            }
            RawInput::SyncLockKeys { toggle_flags } => Some(inpdu::sync_event(toggle_flags)),
            // Touch is routed through the RDPEI dynamic channel, not the fast-path
            // input stream; it is filtered out before this function is called.
            RawInput::Touch { .. } => None,
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::{
        caps_from_flags, effective_sharpen, parse_upscaler, scale_monitor_layout,
        scaled_desktop_dims, QualityPreset,
    };
    use rdp_graphics::egfx;

    #[test]
    fn gaming_advertises_avc420_only_even_with_a_gpu() {
        // --gaming wins over the GPU probe: a CPU-only host then encodes one H.264
        // stream, not AVC444's two. The probe must not even be consulted.
        let caps = caps_from_flags(false, false, QualityPreset::Gaming, || panic!("probe should be skipped"));
        assert_eq!(caps, egfx::CAPS_AVC420_ONLY.to_vec());
    }

    /// AVC444 costs ~2x host encode and ~2x bandwidth for a second stream that the
    /// decode path discards, so no preset asks for it — not even the clarity-first
    /// one, and not even when a GPU is present (which is precisely when the aux
    /// chroma gets thrown away). The probe must not be needed to know that.
    #[test]
    fn no_preset_asks_for_avc444_because_the_aux_chroma_is_discarded() {
        for quality in [
            QualityPreset::Office,
            QualityPreset::Balanced,
            QualityPreset::Gaming,
        ] {
            assert_eq!(
                caps_from_flags(false, false, quality, || panic!("probe should be skipped")),
                egfx::CAPS_AVC420_ONLY.to_vec(),
                "{quality:?} should advertise AVC420-only"
            );
        }
    }

    #[test]
    fn force_avc444_opts_back_into_full_caps() {
        let caps = caps_from_flags(false, true, QualityPreset::Gaming, || panic!("probe should be skipped"));
        assert_eq!(caps, egfx::CAPS_FULL.to_vec());
    }

    #[test]
    fn no_avc_takes_precedence() {
        let caps = caps_from_flags(true, true, QualityPreset::Gaming, || unreachable!());
        assert_eq!(caps, egfx::CAPS_NO_AVC.to_vec());
    }

    /// `--force-avc` is the one way back to AVC444, and it wins over every preset.
    #[test]
    fn force_avc444_overrides_every_preset() {
        for quality in [
            QualityPreset::Office,
            QualityPreset::Balanced,
            QualityPreset::Gaming,
        ] {
            assert_eq!(
                caps_from_flags(false, true, quality, || unreachable!()),
                egfx::CAPS_FULL.to_vec()
            );
        }
    }

    #[test]
    fn render_scale_dims_are_even_clamped_and_unscaled_at_one() {
        // 1080p @0.66 → even-rounded ~2/3 each axis.
        assert_eq!(scaled_desktop_dims(1920, 1080, 0.66), (1267 & !1, 713 & !1));
        // 1.0 leaves native dims unchanged (already even).
        assert_eq!(scaled_desktop_dims(1920, 1080, 1.0), (1920, 1080));
        // Results are always even.
        let (w, h) = scaled_desktop_dims(1366, 769, 0.5);
        assert_eq!((w & 1, h & 1), (0, 0));
        // Clamped to the RDP floor and to the [0.4, 1.0] scale range.
        assert_eq!(scaled_desktop_dims(300, 300, 0.4), (200, 200)); // 120 → floor 200
        assert_eq!(scaled_desktop_dims(1000, 1000, 2.0), (1000, 1000)); // scale clamped to 1.0
    }

    #[test]
    fn upscale_flag_parses_modes_and_defaults_to_bicubic() {
        use rdp_gpu::Upscaler;
        assert_eq!(parse_upscaler("vsr"), Upscaler::Vsr);
        assert_eq!(parse_upscaler("RTX"), Upscaler::Vsr); // case-insensitive
        assert_eq!(parse_upscaler("ai"), Upscaler::Vsr);
        assert_eq!(parse_upscaler("bicubic"), Upscaler::Bicubic);
        assert_eq!(parse_upscaler("catmull-rom"), Upscaler::Bicubic);
        assert_eq!(parse_upscaler("fsr"), Upscaler::Fsr);
        assert_eq!(parse_upscaler("FSR1"), Upscaler::Fsr);
        assert_eq!(parse_upscaler("easu"), Upscaler::Fsr);
        assert_eq!(parse_upscaler("nearest"), Upscaler::Nearest);
        assert_eq!(parse_upscaler("integer"), Upscaler::Nearest);
        assert_eq!(parse_upscaler("bilinear"), Upscaler::Bilinear);
        assert_eq!(parse_upscaler("none"), Upscaler::Bilinear);
        // Unknown values and the default both resolve to Catmull-Rom bicubic.
        assert_eq!(parse_upscaler("wat"), Upscaler::Bicubic);
        assert_eq!(Upscaler::default(), Upscaler::Bicubic);
    }

    #[test]
    fn sharpen_defaults_on_for_fsr_only_and_explicit_wins() {
        use rdp_gpu::Upscaler;
        // FSR is designed as EASU + RCAS: default the sharpen pass on.
        assert_eq!(effective_sharpen(None, Upscaler::Fsr), 0.9);
        assert_eq!(effective_sharpen(None, Upscaler::Bicubic), 0.0);
        assert_eq!(effective_sharpen(None, Upscaler::Vsr), 0.0);
        // An explicit --sharpen always wins, clamped to 0..=1.
        assert_eq!(effective_sharpen(Some(0.0), Upscaler::Fsr), 0.0);
        assert_eq!(effective_sharpen(Some(0.5), Upscaler::Bilinear), 0.5);
        assert_eq!(effective_sharpen(Some(7.0), Upscaler::Bicubic), 1.0);
    }

    fn rect(l: i32, t: i32, r: i32, b: i32, primary: bool) -> rdp_pdu::gcc::VirtualScreenRect {
        rdp_pdu::gcc::VirtualScreenRect {
            left: l,
            top: t,
            right: r,
            bottom: b,
            primary,
        }
    }

    #[test]
    fn scale_monitor_layout_keeps_seams_origin_and_even_dims() {
        // Two 2560x1440 monitors side by side, primary first.
        let rects = [rect(0, 0, 2560, 1440, true), rect(2560, 0, 5120, 1440, false)];
        let (defs, size, slices) = scale_monitor_layout(&rects, 0.66);
        // The shared seam at x=2560 maps to one coordinate on both sides:
        // monitor 0's right edge == monitor 1's left edge (inclusive defs are
        // exclusive-1).
        assert_eq!(defs[0].right + 1, defs[1].left);
        // Primary keeps its (0,0) top-left.
        assert_eq!((defs[0].left, defs[0].top), (0, 0));
        // Bounding box is the sum of the slices along x.
        assert_eq!(size.0, slices[0].1 .0 + slices[1].1 .0);
        // Every slice is even-sized and slice offsets match the defs.
        for (def, (off, dim)) in defs.iter().zip(&slices) {
            assert_eq!(dim.0 & 1, 0);
            assert_eq!(dim.1 & 1, 0);
            assert_eq!(off.0 as i32, def.left);
            assert_eq!(off.1 as i32, def.top);
        }
    }

    #[test]
    fn scale_monitor_layout_handles_negative_origins() {
        // A 1080p monitor left of the primary: virtual-screen coords go negative,
        // the primary still anchors (0,0), and the bbox origin is the min corner.
        let rects = [rect(-1920, 0, 0, 1080, false), rect(0, 0, 2560, 1440, true)];
        let (defs, size, slices) = scale_monitor_layout(&rects, 0.5);
        assert_eq!((defs[1].left, defs[1].top), (0, 0));
        // Slice offsets are bbox-relative and non-negative.
        assert_eq!(slices[0].0, (0, 0));
        assert_eq!(slices[1].0 .0, slices[0].1 .0);
        // The scaled bbox covers both scaled monitors.
        assert_eq!(size.0, slices[0].1 .0 + slices[1].1 .0);
        // Shared seam at x=0 stays shared.
        assert_eq!(defs[0].right + 1, defs[1].left);
    }

    #[test]
    fn scale_monitor_layout_is_identity_at_one() {
        let rects = [rect(0, 0, 1920, 1080, true), rect(1920, 0, 3840, 1080, false)];
        let (defs, size, slices) = scale_monitor_layout(&rects, 1.0);
        assert_eq!(size, (3840, 1080));
        assert_eq!((defs[0].left, defs[0].right), (0, 1919));
        assert_eq!(slices[1], ((1920, 0), (1920, 1080)));
    }
}
