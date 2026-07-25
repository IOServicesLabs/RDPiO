//! Sync bridge from a DVC mux to the async [`Redirector`] engine (feature `engine`).
//!
//! The graphics DVC mux is a synchronous, single-threaded poll loop; the native
//! WebRTC engine ([`crate::engine::WebrtcEngine`], via [`crate::dispatch::Redirector`])
//! is `async` and spawns its own tokio tasks (ICE agents, DTLS, per-track RTP
//! readers). [`NativeRedirector`] is the seam between them, mirroring how
//! `rdp-client::webrtc_addin::WebRtcRedirector` bridges the mux to the Windows COM
//! host thread — except here the "host thread" runs a tokio runtime instead of an
//! STA message pump.
//!
//! Shape:
//! - a dedicated thread owns a multi-thread tokio runtime and `block_on`s the
//!   dispatch loop;
//! - inbound DVC bytes are pushed over a tokio `mpsc` (non-blocking from the mux);
//!   the loop strips framing, parses, and drives the [`Redirector`], which answers
//!   with webrtc.1 Result/Event JSON;
//! - a periodic tick drains newly-gathered ICE candidates as trickle events
//!   (candidates arrive asynchronously after `setLocalDescription`, so we can't
//!   only drain in response to inbound messages);
//! - outbound framed messages land in a shared queue the mux drains each poll.
//!
//! This type is platform-neutral: `rdp-client` wraps it in a `DvcRedirector` on
//! Windows today; the Linux session path can wrap the same handle later.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::devices::DeviceProvider;
use crate::dispatch::Redirector;
use crate::engine::VideoCaptureSource;
use crate::framing;
use crate::ice::TurnResolver;
use crate::rpc::RpcMessage;

/// The dynamic virtual channel Teams' WebRTC redirector speaks on.
pub const CHANNEL_NAME: &str = "com.microsoft.rdc.dvc.webrtc.1";

// ---------------------------------------------------------------------------
// webrtc.1 wire capture (opt-in via `RDPIO_WEBRTC_CAPTURE=<path>`), byte-identical
// to the DLL-host path's format (`rdp-client::webrtc_addin`) so the same parser
// (`crate::capture`) and fixtures work — and so a native capture can be diffed
// directly against a DLL capture. File format: header `b"WRTC1\0"`; then records,
// each little-endian: dir(u8 'S'=inbound / 'C'=outbound), channel_id(u32),
// seq(u32), t_ms(u32 since open), len(u32), payload. Opt-in + file-only; ICE
// ufrag/pwd and TURN creds here are ephemeral per-session material.
// ---------------------------------------------------------------------------

const CAP_DIR_INBOUND: u8 = b'S';
const CAP_DIR_OUTBOUND: u8 = b'C';

struct CaptureInner {
    file: File,
    seq: u32,
    start: Instant,
}

/// Shared, thread-safe capture sink for the webrtc.1 wire.
#[derive(Clone)]
struct Capture(Arc<Mutex<CaptureInner>>);

impl Capture {
    /// Open the file named by `RDPIO_WEBRTC_CAPTURE`, or `None` if unset/uncreatable.
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("RDPIO_WEBRTC_CAPTURE")?;
        if path.is_empty() {
            return None;
        }
        match File::create(&path) {
            Ok(mut file) => {
                let _ = file.write_all(b"WRTC1\0");
                tracing::info!(path = %Path::new(&path).display(), "webrtc-native: wire capture enabled");
                Some(Capture(Arc::new(Mutex::new(CaptureInner {
                    file,
                    seq: 0,
                    start: Instant::now(),
                }))))
            }
            Err(e) => {
                tracing::warn!(error = %e, "webrtc-native: could not open RDPIO_WEBRTC_CAPTURE; capture disabled");
                None
            }
        }
    }

    /// Append one logical message. Best-effort; capture errors never disrupt the call.
    fn record(&self, dir: u8, channel_id: u32, data: &[u8]) {
        let Ok(mut inner) = self.0.lock() else { return };
        let seq = inner.seq;
        inner.seq = inner.seq.wrapping_add(1);
        let t_ms = inner.start.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut hdr = [0u8; 17];
        hdr[0] = dir;
        hdr[1..5].copy_from_slice(&channel_id.to_le_bytes());
        hdr[5..9].copy_from_slice(&seq.to_le_bytes());
        hdr[9..13].copy_from_slice(&t_ms.to_le_bytes());
        hdr[13..17].copy_from_slice(&(data.len() as u32).to_le_bytes());
        let _ = inner.file.write_all(&hdr);
        let _ = inner.file.write_all(data);
        let _ = inner.file.flush();
    }
}

/// How often to poll the engine for freshly-gathered ICE candidates. Trickle ICE
/// arrives over hundreds of ms after `setLocalDescription`, independent of any
/// inbound message, so we sweep on a timer as well as after each Call.
const ICE_POLL: Duration = Duration::from_millis(20);

/// Queue of `(channel_id, framed_payload)` the engine wants sent to the server,
/// shared between the runtime thread (producer) and the mux (consumer).
type Outbound = Arc<Mutex<VecDeque<(u32, Vec<u8>)>>>;

/// Control/data messages from the mux thread to the runtime thread.
enum Inbound {
    /// The server opened a webrtc.1 channel — start a fresh negotiation on it.
    Create { channel_id: u32 },
    /// A complete reassembled webrtc.1 message arrived.
    Data { channel_id: u32, bytes: Vec<u8> },
    /// The server closed the channel.
    Close { channel_id: u32 },
    /// Tear the engine down (handle dropped).
    Shutdown,
}

/// Handle the mux holds. `Send`; all async/media work happens on the owned
/// runtime thread. Cheap, non-blocking methods that only enqueue or drain.
pub struct NativeRedirector {
    tx: UnboundedSender<Inbound>,
    outbound: Outbound,
    /// webrtc.1 wire capture (inbound side); `None` unless `RDPIO_WEBRTC_CAPTURE`
    /// is set. A clone records the outbound side on the runtime thread.
    capture: Option<Capture>,
    thread: Option<JoinHandle<()>>,
}

impl NativeRedirector {
    /// Spin up the runtime thread and the native engine. Fails only if the OS
    /// refuses to spawn the thread.
    ///
    /// `devices` supplies the client's real cameras/mics/speakers. Pass `None` only
    /// where they genuinely don't exist: Teams refuses to optimize a call on an
    /// endpoint that reports no devices. `turn_resolver` follows the TURN `300 Try
    /// Alternate` webrtc-rs can't (needed for any relay candidate on Teams' anycast
    /// relays); `None` leaves the URLs untouched.
    pub fn new(
        devices: Option<Arc<dyn DeviceProvider>>,
        turn_resolver: Option<Arc<dyn TurnResolver>>,
        video_source: Option<Arc<dyn VideoCaptureSource>>,
    ) -> std::io::Result<Self> {
        let outbound: Outbound = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let capture = Capture::from_env();
        let out_thread = outbound.clone();
        let cap_thread = capture.clone();
        let thread = std::thread::Builder::new()
            .name("webrtc-native".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "webrtc-native: failed to build tokio runtime");
                        return;
                    }
                };
                rt.block_on(run(rx, out_thread, cap_thread, devices, turn_resolver, video_source));
            })?;
        Ok(Self { tx, outbound, capture, thread: Some(thread) })
    }

    /// A create-request for a webrtc.1 channel arrived; begin a fresh call on it.
    pub fn on_create(&self, channel_id: u32) {
        let _ = self.tx.send(Inbound::Create { channel_id });
    }

    /// Forward one reassembled inbound webrtc.1 message.
    pub fn on_data(&self, channel_id: u32, message: &[u8]) {
        if let Some(cap) = &self.capture {
            cap.record(CAP_DIR_INBOUND, channel_id, message);
        }
        let _ = self.tx.send(Inbound::Data { channel_id, bytes: message.to_vec() });
    }

    /// The server closed the channel.
    pub fn on_close(&self, channel_id: u32) {
        let _ = self.tx.send(Inbound::Close { channel_id });
    }

    /// Drain everything the engine has queued for the server since the last call.
    pub fn drain_outbound(&self) -> Vec<(u32, Vec<u8>)> {
        self.outbound
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

impl Drop for NativeRedirector {
    fn drop(&mut self) {
        let _ = self.tx.send(Inbound::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Frame each JSON value (NUL-terminated), record it to the capture if enabled,
/// and enqueue it for the mux, tagged with the channel it belongs to. Best-effort:
/// a serialize failure is logged and dropped, never propagated into the live call.
fn push_framed(outbound: &Outbound, capture: &Option<Capture>, channel_id: u32, msgs: &[Value]) {
    if msgs.is_empty() {
        return;
    }
    let Ok(mut q) = outbound.lock() else { return };
    for m in msgs {
        match serde_json::to_vec(m) {
            Ok(bytes) => {
                let framed = framing::frame(&bytes);
                if let Some(cap) = capture {
                    cap.record(CAP_DIR_OUTBOUND, channel_id, &framed);
                }
                q.push_back((channel_id, framed));
            }
            Err(e) => tracing::warn!(error = %e, "webrtc-native: failed to serialize outbound message"),
        }
    }
}

/// The dispatch loop: own the [`Redirector`], turn inbound Calls into outbound
/// Result/Event messages, and sweep trickle ICE on a timer.
async fn run(
    mut rx: UnboundedReceiver<Inbound>,
    outbound: Outbound,
    capture: Option<Capture>,
    devices: Option<Arc<dyn DeviceProvider>>,
    turn_resolver: Option<Arc<dyn TurnResolver>>,
    video_source: Option<Arc<dyn VideoCaptureSource>>,
) {
    // Each channel (call) gets a fresh redirector; the device provider, TURN resolver
    // and camera source are long-lived host capabilities, so re-attach them to each.
    let new_redirector = || {
        let mut r = Redirector::new();
        if let Some(d) = &devices {
            r.set_device_provider(d.clone());
        }
        if let Some(t) = &turn_resolver {
            r.set_turn_resolver(t.clone());
        }
        if let Some(v) = &video_source {
            r.set_video_source(v.clone());
        }
        r
    };
    let mut redirector = new_redirector();
    // The channel the current negotiation runs on; trickle-ICE events are tagged
    // with it. Set on Create / first Data, cleared on Close.
    let mut channel: Option<u32> = None;
    let mut ticker = tokio::time::interval(ICE_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!("webrtc-native: engine loop started");

    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(Inbound::Create { channel_id }) => {
                    // Fresh call → fresh engine (drops any prior peer connection).
                    tracing::info!(channel_id, "webrtc-native: new webrtc.1 channel; resetting engine");
                    redirector = new_redirector();
                    channel = Some(channel_id);
                }
                Some(Inbound::Data { channel_id, bytes }) => {
                    channel = Some(channel_id);
                    let json = framing::message_json(&bytes);
                    match RpcMessage::parse(json) {
                        Ok(msg) => {
                            if let Some(name) = &msg.name {
                                tracing::debug!(
                                    channel_id,
                                    method = %name,
                                    kind = ?msg.kind(),
                                    "webrtc-native: inbound call"
                                );
                            }
                            let replies = redirector.handle(&msg).await;
                            push_framed(&outbound, &capture, channel_id, &replies);
                            // Some candidates may already be ready right after
                            // setLocalDescription; the ticker catches the rest.
                            let ice = redirector.drain_ice().await;
                            push_framed(&outbound, &capture, channel_id, &ice);
                        }
                        Err(e) => tracing::warn!(
                            channel_id,
                            len = json.len(),
                            error = %e,
                            "webrtc-native: could not parse inbound message"
                        ),
                    }
                }
                Some(Inbound::Close { channel_id }) => {
                    tracing::info!(channel_id, "webrtc-native: channel closed");
                    if channel == Some(channel_id) {
                        channel = None;
                    }
                }
                Some(Inbound::Shutdown) | None => break,
            },
            _ = ticker.tick() => {
                if let Some(ch) = channel {
                    let ice = redirector.drain_ice().await;
                    if !ice.is_empty() {
                        tracing::debug!(channel_id = ch, n = ice.len(), "webrtc-native: trickle ICE update");
                    }
                    push_framed(&outbound, &capture, ch, &ice);
                }
            }
        }
    }
    tracing::info!("webrtc-native: engine loop stopped");
}
