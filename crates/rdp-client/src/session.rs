//! Activation driver: runs the MCS connect + RDP activation sequence over a
//! stream, including the Standard RDP Security key exchange + RC4/MAC encryption
//! when the negotiated path is legacy (not TLS/NLA).
//!
//! Sequence (MS-RDPBCGR 1.3.1.1): basic-settings → Erect Domain / Attach User /
//! Channel Join → [Security Exchange + key derivation, legacy only] → Client
//! Info → licensing → Demand/Confirm Active → finalization → Font Map.
//!
//! The plaintext path is exercised by a scripted-mock loopback test; the
//! encryption format (header + MAC + RC4) is covered by a unit round-trip.
//! Live wire details (security-header presence per encryption level, licensing
//! variants) are validated against a real server.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rdp_core::ClientConfig;
use rdp_pdu::x224::SecurityProtocol;
use rdp_pdu::{capabilities, finalization, gcc, mcs, security};

use crate::transport::read_tpkt_pdu;

/// Max EMT graphics PDUs to drain from the UDP tunnel per session-loop iteration.
/// Bounds how long a saturated UDP stream can defer TCP input / clipboard service
/// before the loop cycles back — high enough that several full frames' worth of
/// PDUs clear per pass, low enough to keep input responsive under a graphics flood.
const UDP_DRAIN_BUDGET: u32 = 256;

/// Set by the platform when the *local* clipboard changes (so we re-advertise it
/// to the server); drained by the session loops. Process-global — there is one
/// session/window. See [`clipboard_changed`] / [`take_clipboard_changed`].
pub(crate) static CLIPBOARD_DIRTY: AtomicBool = AtomicBool::new(false);
/// Set by the clipboard provider just before it writes a *remote* paste into the
/// local clipboard, so the resulting change notification isn't echoed back to
/// the server (which would loop). Consumed by [`clipboard_changed`].
#[cfg(windows)]
pub(crate) static CLIPBOARD_SUPPRESS: AtomicBool = AtomicBool::new(false);

/// Note a local clipboard change (called from the window's clipboard listener),
/// unless it was self-induced by applying a remote paste.
#[cfg(windows)]
pub(crate) fn clipboard_changed() {
    if CLIPBOARD_SUPPRESS.swap(false, Ordering::SeqCst) {
        return; // our own SetClipboardData — don't echo it back
    }
    CLIPBOARD_DIRTY.store(true, Ordering::SeqCst);
    crate::net_wait::worker_wake::signal();
}

/// Suppress the next local clipboard-change notification (the provider calls this
/// before writing a remote paste locally).
#[cfg(windows)]
pub(crate) fn suppress_clipboard_echo() {
    CLIPBOARD_SUPPRESS.store(true, Ordering::SeqCst);
}

fn take_clipboard_changed() -> bool {
    CLIPBOARD_DIRTY.swap(false, Ordering::SeqCst)
}

/// Hand-off for lazily-staged clipboard files, between the UI thread (which
/// owns the clipboard and receives `WM_RENDERFORMAT` when someone pastes) and
/// the session worker (which runs the actual transfer).
///
/// Files copied in the session are only ADVERTISED at first. On a paste the UI
/// thread sets `wanted` and blocks on `done`; the worker notices, streams the
/// files to disk, and publishes their paths — which the UI thread then turns
/// into a real `CF_HDROP`.
#[cfg(windows)]
#[derive(Default)]
struct ClipFileHandoff {
    /// Entries the session currently offers (0 = nothing to paste).
    offered: usize,
    /// A paste is waiting for the bytes.
    wanted: bool,
    /// Staged local paths once the transfer finishes (empty = failed).
    ready: Option<Vec<std::path::PathBuf>>,
}

#[cfg(windows)]
static CLIP_FILES: std::sync::Mutex<ClipFileHandoff> =
    std::sync::Mutex::new(ClipFileHandoff {
        offered: 0,
        wanted: false,
        ready: None,
    });
#[cfg(windows)]
static CLIP_FILES_DONE: std::sync::Condvar = std::sync::Condvar::new();

#[cfg(windows)]
fn clip_files() -> std::sync::MutexGuard<'static, ClipFileHandoff> {
    CLIP_FILES.lock().unwrap_or_else(|p| p.into_inner())
}

/// The session announced `count` clipboard files (nothing transferred yet).
#[cfg(windows)]
pub(crate) fn clipboard_files_offered(count: usize) {
    let mut g = clip_files();
    g.offered = count;
    g.wanted = false;
    g.ready = None;
}

/// UI thread: a paste needs the files. Blocks (bounded) until the worker has
/// staged them, and returns their local paths — empty if there is nothing to
/// paste or the transfer failed.
#[cfg(windows)]
pub(crate) fn request_clipboard_files(timeout: std::time::Duration) -> Vec<std::path::PathBuf> {
    let mut g = clip_files();
    if g.offered == 0 {
        return Vec::new();
    }
    if let Some(paths) = g.ready.clone() {
        return paths; // already staged (e.g. a second paste)
    }
    g.wanted = true;
    crate::net_wait::worker_wake::signal();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("timed out staging clipboard files for a paste");
            return Vec::new();
        }
        let (next, wait) = CLIP_FILES_DONE
            .wait_timeout(g, remaining)
            .unwrap_or_else(|p| p.into_inner());
        g = next;
        if let Some(paths) = g.ready.clone() {
            return paths;
        }
        if wait.timed_out() {
            tracing::warn!("timed out staging clipboard files for a paste");
            return Vec::new();
        }
    }
}

/// Worker: whether a paste is waiting for the offered files. Clears the flag.
#[cfg(windows)]
fn take_clipboard_file_request() -> bool {
    let mut g = clip_files();
    std::mem::take(&mut g.wanted)
}

/// Worker: the files are staged at `paths` (empty = the transfer failed).
#[cfg(windows)]
pub(crate) fn clipboard_files_ready(paths: Vec<std::path::PathBuf>) {
    let mut g = clip_files();
    g.ready = Some(paths);
    drop(g);
    CLIP_FILES_DONE.notify_all();
}

/// UI thread: our advertised copy was superseded on the local clipboard, so
/// anything already staged for it is no longer reachable and can be dropped.
#[cfg(windows)]
pub(crate) fn clipboard_files_discarded() {
    let mut g = clip_files();
    g.offered = 0;
    g.wanted = false;
    g.ready = None;
    drop(g);
    // Release any paste still blocked on us.
    CLIP_FILES_DONE.notify_all();
}

/// A pending desktop-resize request, stored as the full monitor layout the UI
/// wants the remote desktop to adopt. Set by the UI when the window/monitor
/// topology changes; drained by the graphics loop, which asks the server (via
/// Display Control) to resize to match.
#[cfg(windows)]
pub(crate) static RESIZE_REQUEST: std::sync::Mutex<Option<Vec<gcc::MonitorDef>>> =
    std::sync::Mutex::new(None);

/// Request the remote desktop adopt `monitors` (called by the UI).
#[cfg(windows)]
pub(crate) fn request_resize(monitors: Vec<gcc::MonitorDef>) {
    if let Ok(mut req) = RESIZE_REQUEST.lock() {
        *req = Some(monitors);
    }
    crate::net_wait::worker_wake::signal();
}

#[cfg(windows)]
fn take_resize_request() -> Option<Vec<gcc::MonitorDef>> {
    RESIZE_REQUEST.lock().ok().and_then(|mut req| req.take())
}

/// Pending multi-touch contacts queued by the UI and drained by the graphics
/// loop to be sent over the RDPEI dynamic channel.
#[cfg(windows)]
static TOUCH_QUEUE: std::sync::Mutex<Vec<rdp_channels::rdpei::RdpInputContact>> =
    std::sync::Mutex::new(Vec::new());

/// Queue a batch of touch contacts to be sent on the RDPEI channel.
#[cfg(windows)]
pub(crate) fn queue_touch(contacts: Vec<rdp_channels::rdpei::RdpInputContact>) {
    if let Ok(mut q) = TOUCH_QUEUE.lock() {
        q.extend(contacts);
    }
    crate::net_wait::worker_wake::signal();
}

#[cfg(windows)]
fn take_touch_queue() -> Vec<rdp_channels::rdpei::RdpInputContact> {
    TOUCH_QUEUE.lock().ok().map_or(Vec::new(), |mut q| std::mem::take(&mut *q))
}

/// The server's `TS_INPUT_CAPABILITYSET.inputFlags` from the latest Demand
/// Active (bit 15 marks "seen", so 0 = not yet known). The UI thread reads this
/// to decide whether mouse-capture mode may send relative pointer events.
static SERVER_INPUT_FLAGS: AtomicU32 = AtomicU32::new(0);

fn note_server_input_flags(share_pdu: &[u8]) {
    if let Some(flags) = rdp_pdu::capabilities::parse_server_input_flags(share_pdu) {
        SERVER_INPUT_FLAGS.store(0x8000_0000 | flags as u32, Ordering::SeqCst);
        tracing::info!(
            input_flags = format!("0x{flags:04x}"),
            relative_mouse =
                flags & rdp_pdu::capabilities::INPUT_FLAG_MOUSE_RELATIVE != 0,
            "server input capabilities"
        );
    }
}

/// Whether the server supports TS_RELPOINTER_EVENT relative mouse input
/// (INPUT_FLAG_MOUSE_RELATIVE in its Demand Active input caps).
pub(crate) fn rel_mouse_supported() -> bool {
    SERVER_INPUT_FLAGS.load(Ordering::SeqCst)
        & rdp_pdu::capabilities::INPUT_FLAG_MOUSE_RELATIVE as u32
        != 0
}

#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Pdu(#[from] rdp_pdu::PduError),
    #[error("activation error: {0}")]
    Protocol(String),
    #[error("server redirection: {0:?}")]
    Redirect(rdp_pdu::redirection::ServerRedirection),
}

/// Identifiers describing the live session once activation completes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionInfo {
    pub user_channel_id: u16,
    pub io_channel_id: u16,
    pub share_id: u32,
    pub channel_ids: Vec<u16>,
    /// Advertised static-VC names, parallel to `channel_ids` (the server assigns
    /// ids in advertised order), so a channel can be found by name.
    pub channel_names: Vec<String>,
    pub encrypted: bool,
}

impl SessionInfo {
    /// The MCS channel id of a static virtual channel by its advertised name.
    pub fn channel_id(&self, name: &str) -> Option<u16> {
        let idx = self.channel_names.iter().position(|n| n == name)?;
        self.channel_ids.get(idx).copied()
    }
}

fn proto_err(msg: impl Into<String>) -> ActivateError {
    ActivateError::Protocol(msg.into())
}

/// Standard RDP Security state: directional RC4 ciphers (each self-rekeying
/// every 4096 packets per MS-RDPBCGR 5.3.7) + the MAC key.
struct SecuritySession {
    encrypt: rdp_crypto::SessionCipher,
    decrypt: rdp_crypto::SessionCipher,
    mac_key: Vec<u8>,
}

impl SecuritySession {
    fn new(keys: rdp_crypto::keys::SessionKeys, method: u32) -> Self {
        Self {
            encrypt: rdp_crypto::SessionCipher::new(keys.client_encrypt_key, method),
            decrypt: rdp_crypto::SessionCipher::new(keys.server_decrypt_key, method),
            mac_key: keys.mac_key,
        }
    }

    /// Wrap a plaintext share payload: basic security header (SEC_ENCRYPT +
    /// `extra_flags`) + 8-byte MAC + RC4-encrypted data.
    fn wrap(&mut self, extra_flags: u16, plaintext: &[u8]) -> Vec<u8> {
        wrap_security(&mut self.encrypt, &self.mac_key, extra_flags, plaintext)
    }

    /// Strip the basic security header, RC4-decrypt, and verify the MAC.
    fn unwrap(&mut self, payload: &[u8]) -> Vec<u8> {
        decrypt_and_verify(&mut self.decrypt, &self.mac_key, payload)
    }

    /// Consume the session, splitting it into the inbound (decrypt + MAC) half
    /// kept by the reader and the outbound (encrypt + MAC) half handed to the
    /// input sender, so each RC4 direction has exactly one owner across threads.
    /// The per-direction 4096-packet re-key counter lives inside each
    /// [`rdp_crypto::SessionCipher`], so it survives this move intact.
    fn split(self) -> (InboundCrypto, OutboundCrypto) {
        (
            InboundCrypto {
                decrypt: self.decrypt,
                mac_key: self.mac_key.clone(),
            },
            OutboundCrypto {
                encrypt: self.encrypt,
                mac_key: self.mac_key,
            },
        )
    }
}

/// The outbound (client→server) half of the security state: the RC4 encrypt
/// cipher and the MAC key. Shared after activation between the session worker
/// (channel replies, reactivation, share PDUs) and the [`InputSender`] thread.
struct OutboundCrypto {
    encrypt: rdp_crypto::SessionCipher,
    mac_key: Vec<u8>,
}

impl OutboundCrypto {
    /// Wrap a plaintext share payload (see [`wrap_security`]).
    fn wrap(&mut self, extra_flags: u16, plaintext: &[u8]) -> Vec<u8> {
        wrap_security(&mut self.encrypt, &self.mac_key, extra_flags, plaintext)
    }
}

/// The outbound cipher, shared between the session worker and the input-sender
/// thread on the legacy path. There is exactly ONE client→server RC4 keystream
/// per connection, and both threads send on it: the worker answers channel
/// traffic (cliprdr/rdpdr/drdynvc) and reactivations, the input thread sends
/// pointer/keyboard events. Whoever encrypts a PDU must also put it on the wire
/// before releasing the lock — RC4 and the MAC are stateful, so the server
/// decrypts strictly in arrival order; encrypt-order ≠ wire-order desyncs the
/// keystream and the server silently resets the connection.
type SharedOutbound = std::sync::Arc<std::sync::Mutex<OutboundCrypto>>;

/// Lock a [`SharedOutbound`], recovering the state from a poisoned mutex (a
/// panicked peer thread leaves the cipher usable — RC4 state is just bytes).
fn lock_outbound(c: &SharedOutbound) -> std::sync::MutexGuard<'_, OutboundCrypto> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

/// The inbound (server→client) half of the security state: the RC4 decrypt
/// cipher and the MAC key. Owned by the reader after activation; consumed via
/// the free [`unwrap_inbound`] (which both paths share).
struct InboundCrypto {
    decrypt: rdp_crypto::SessionCipher,
    mac_key: Vec<u8>,
}

/// Wrap `plaintext` in the Standard RDP Security format: a basic security header
/// (`SEC_ENCRYPT | extra_flags`) + 8-byte MAC + RC4-encrypted data. Shared by
/// the activation-time and input-time encryptors.
fn wrap_security(
    encrypt: &mut rdp_crypto::SessionCipher,
    mac_key: &[u8],
    extra_flags: u16,
    plaintext: &[u8],
) -> Vec<u8> {
    let mac = rdp_crypto::keys::mac_signature(mac_key, plaintext);
    let mut data = plaintext.to_vec();
    encrypt.apply_packet(&mut data);

    let flags = security::SEC_ENCRYPT | extra_flags;
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flagsHi
    out.extend_from_slice(&mac);
    out.extend_from_slice(&data);
    out
}

/// Strip the basic security header from an inbound payload. On the legacy path
/// (`inbound` present) RC4-decrypt the body and verify its 8-byte MAC; on the
/// TLS/NLA path (`inbound` is `None`) the payload is the share PDU as-is.
fn unwrap_inbound<'a>(
    inbound: Option<&mut InboundCrypto>,
    payload: &'a [u8],
) -> std::borrow::Cow<'a, [u8]> {
    match inbound {
        Some(ic) => {
            std::borrow::Cow::Owned(decrypt_and_verify(&mut ic.decrypt, &ic.mac_key, payload))
        }
        // TLS path: the payload IS the share PDU — no copy.
        None => std::borrow::Cow::Borrowed(payload),
    }
}

/// RC4-decrypt an inbound Standard RDP Security payload and verify its MAC. A
/// MAC mismatch means our keystream has desynced from the server's (e.g. a
/// missed 4096-packet re-key); we surface it loudly but still return the
/// decrypted bytes rather than tear the session down, since the caller may
/// recover.
fn decrypt_and_verify(
    decrypt: &mut rdp_crypto::SessionCipher,
    mac_key: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    if payload.len() < 4 {
        return payload.to_vec();
    }
    let flags = u16::from_le_bytes([payload[0], payload[1]]);
    let body = &payload[4..];
    if flags & security::SEC_ENCRYPT != 0 && body.len() >= 8 {
        let received_mac = [
            body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
        ];
        let mut data = body[8..].to_vec(); // body[0..8] is the dataSignature (MAC)
        decrypt.apply_packet(&mut data);
        if rdp_crypto::keys::mac_signature(mac_key, &data) != received_mac {
            warn_mac_mismatch();
        }
        data
    } else {
        body.to_vec()
    }
}

/// Log an inbound-MAC mismatch, rate-limited so a persistent desync can't flood
/// the log: warn on the first failure and then once every 1000.
fn warn_mac_mismatch() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 1000 == 0 {
        tracing::warn!(
            failures = n + 1,
            "inbound MAC verification failed — RC4 keystream desynced from the server \
             (Standard RDP Security); the session may be unstable"
        );
    }
}

/// 32 cryptographically-secure random bytes for the Standard RDP Security
/// client random, from the OS CSPRNG. If the OS RNG is somehow unavailable we
/// log loudly and fall back to a weak time-seeded value rather than abort.
fn random_32() -> [u8; 32] {
    let mut out = [0u8; 32];
    if crate::rng::fill(&mut out) {
        return out;
    }
    tracing::error!(
        "OS CSPRNG unavailable — using a WEAK time-seeded client random; \
         the legacy session key is not secure"
    );
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for chunk in out.chunks_mut(8) {
        // SplitMix64 fallback.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let n = chunk.len();
        chunk.copy_from_slice(&z.to_le_bytes()[..n]);
    }
    out
}

/// Run the activation sequence to the Active state.
pub fn activate<S: Read + Write>(
    stream: &mut S,
    config: &ClientConfig,
    selected_protocol: SecurityProtocol,
    reconnect: Option<&rdp_pdu::logon::ReconnectCookie>,
) -> Result<ActiveSession, ActivateError> {
    // 1) Basic settings exchange.
    let standard_security = selected_protocol.is_empty();
    let cs_security = if standard_security {
        gcc::ClientSecurityData {
            encryption_methods: gcc::ENCRYPTION_METHOD_40BIT
                | gcc::ENCRYPTION_METHOD_128BIT
                | gcc::ENCRYPTION_METHOD_56BIT,
            ext_encryption_methods: 0,
        }
    } else {
        gcc::ClientSecurityData::default()
    };
    let mut core = gcc::ClientCoreData {
        desktop_width: config.width,
        desktop_height: config.height,
        server_selected_protocol: selected_protocol.bits(),
        client_name: if config.hostname.is_empty() {
            "rdpio".into()
        } else {
            config.hostname.clone()
        },
        ..Default::default()
    };
    if let Some(layout) = config.keyboard_layout {
        core.keyboard_layout = layout;
    }
    if let Some(depth) = config.color_depth {
        core.high_color_depth = depth;
    }
    // Standard RDP Security can't carry EGFX: the legacy receive loop
    // (`run_session`/`pump_once`) services neither the RDPGFX dynamic channel nor
    // network auto-detect probes. Leaving the GFX flag set makes Win10/11 stream
    // the whole desktop over a channel we ignore → black screen, then a ~20s
    // watchdog teardown. Clearing it (and the autodetect flag) makes the host fall
    // back to slow-path Bitmap Updates, which `paint_bitmap_update` renders — the
    // same path mstsc uses over legacy security.
    if standard_security {
        core.early_capability_flags &= !(gcc::RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL
            | gcc::RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT);
        tracing::info!(
            "legacy Standard RDP Security: requesting slow-path bitmap output (no EGFX/autodetect)"
        );
    }
    // Advertise the dynamic VC manager (RDPGFX) plus the clipboard channel. The
    // server assigns channel ids in this order, so `advertised_channels` lines
    // up with `server.channel_ids` for name→id lookup.
    //
    // No static `rdpsnd` channel: like mstsc, audio is carried over the dynamic
    // `AUDIO_PLAYBACK_DVC` channel (negotiated through drdynvc). Advertising both
    // leaves the host ambiguous about where to stream waves. Every supported host
    // (Windows Server 2019+ / Win10-latest / Win11) does DVC audio, so there is
    // no legacy path to keep.
    let network = gcc::ClientNetworkData {
        channels: vec![
            gcc::Channel::drdynvc(),
            gcc::Channel::cliprdr(),
            gcc::Channel::rdpdr(),
        ],
    };
    let advertised_channels: Vec<String> =
        network.channels.iter().map(|c| c.name.clone()).collect();
    // Advertise the client's monitor layout for a spanned multi-monitor desktop
    // (empty/single → omitted; the server then uses just the core desktop size).
    let monitors = gcc::ClientMonitorData {
        monitors: config.monitors.clone(),
    };
    // RDP multipathing: advertise the side-band UDP transports (reliable + lossy,
    // UDP-preferred for graphics) so the server sends an Initiate Multitransport
    // Request and the EGFX pipeline can ride UDP. Only when `config.multitransport`
    // is set — i.e. a direct host we can open a UDP socket to; the UDP dial in the
    // session loop honours the same condition, so we never advertise a transport
    // we can't bring up.
    let multitransport = if config.multitransport {
        tracing::info!(
            "advertising CS_MULTITRANSPORT (UDP-R/UDP-L, UDP-preferred) — soliciting an \
             Initiate Multitransport Request"
        );
        gcc::ClientMultitransportData::enabled()
    } else {
        gcc::ClientMultitransportData::default()
    };
    let basic = mcs::basic_settings_pdu(
        &core,
        &cs_security,
        &network,
        &gcc::ClientClusterData::default(),
        &monitors,
        &multitransport,
    )?;
    stream.write_all(&basic)?;
    stream.flush()?;

    let response = read_tpkt_pdu(stream)?;
    let connect = mcs::parse_connect_response(&response)?;
    if connect.result != 0 {
        return Err(proto_err(format!(
            "MCS Connect-Response result = {}",
            connect.result
        )));
    }
    let server = gcc::parse_server_data(&connect.user_data)
        .ok_or_else(|| proto_err("server GCC data missing"))?;
    tracing::info!(io = server.io_channel_id, channels = ?server.channel_ids, enc_level = server.encryption_level, "MCS Connect-Response OK");

    // 2) Erect Domain + Attach User.
    stream.write_all(&mcs::frame(&mcs::erect_domain_request())?)?;
    stream.write_all(&mcs::frame(&mcs::attach_user_request())?)?;
    stream.flush()?;
    let confirm = read_tpkt_pdu(stream)?;
    let user_id = mcs::parse_attach_user_confirm(&confirm)?;
    tracing::info!(user_id, "attached user");

    // 3) Join the user channel, the I/O channel, and each virtual channel.
    let mut channels = vec![user_id, server.io_channel_id];
    channels.extend(server.channel_ids.iter().copied());
    for &channel in &channels {
        stream.write_all(&mcs::frame(&mcs::channel_join_request(user_id, channel))?)?;
        stream.flush()?;
        let pdu = read_tpkt_pdu(stream)?;
        let joined = mcs::parse_channel_join_confirm(&pdu)?;
        if joined != channel {
            return Err(proto_err(format!("joined {joined}, expected {channel}")));
        }
    }
    tracing::info!(count = channels.len(), "channels joined");

    // 4) Standard RDP Security: Security Exchange (RSA-encrypted client random)
    //    + session key derivation. (No-op on the TLS/NLA path.)
    let mut sec: Option<SecuritySession> = None;
    // Kept for the auto-reconnect verifier (HMAC-MD5 over this connection's
    // client random); only set on the Standard RDP Security path.
    let mut client_random_used: Option<[u8; 32]> = None;
    if standard_security && server.encryption_level >= 1 {
        let key = server
            .public_key
            .as_ref()
            .ok_or_else(|| proto_err("Standard RDP Security but no server certificate"))?;
        let client_random = random_32();
        client_random_used = Some(client_random);
        let mut encrypted_random =
            rdp_crypto::rsa::encrypt_le(&client_random, &key.modulus_le, &key.exponent_le);
        encrypted_random.extend_from_slice(&[0u8; 8]); // 8 bytes of trailing padding
        let exchange = security::security_exchange(&encrypted_random);
        send_payload(stream, user_id, server.io_channel_id, &exchange)?;

        let keys = rdp_crypto::keys::derive(
            &client_random,
            &server.server_random,
            server.encryption_method,
        );
        sec = Some(SecuritySession::new(keys, server.encryption_method));
        tracing::info!(
            method = server.encryption_method,
            "sent Security Exchange; derived session keys"
        );
    }

    // 5) Client Info (logon). Encrypted path wraps the TS_INFO_PACKET; plaintext
    //    path uses the unencrypted SEC_INFO_PKT header.
    let info = security::ClientInfo {
        domain: config.credentials.domain.clone(),
        username: config.credentials.username.clone(),
        password: config.credentials.password.clone(),
        load_balance_info: config.load_balance_info.clone().unwrap_or_default(),
        redirected_session_id: config.redirected_session_id.unwrap_or_default(),
        ..Default::default()
    };
    // On reconnect over Standard RDP Security, attach the auto-reconnect cookie
    // (its verifier is HMAC-MD5(server arc_random, this connection's client
    // random)). The initial connect — and the TLS path, where there is no RDP
    // client random — send the plain Client Info unchanged.
    let arc_cookie = match (reconnect, client_random_used) {
        (Some(c), Some(cr)) => {
            let verifier = rdp_crypto::hmac_md5(&c.arc_random, &cr);
            tracing::info!(logon_id = c.logon_id, "attaching auto-reconnect cookie");
            Some(security::auto_reconnect_cookie(c.logon_id, &verifier))
        }
        _ => None,
    };
    let client_info_payload = match sec.as_mut() {
        Some(s) => {
            // Legacy RC4 path: build the TS_INFO_PACKET + extended info (balanced
            // perf flags, plus the reconnect cookie when present) and wrap it.
            let mut ts = Vec::new();
            info.encode(&mut ts);
            ts.extend_from_slice(&security::extended_info_packet(
                security::PERF_BALANCED,
                arc_cookie.as_ref(),
                info.redirected_session_id,
            ));
            s.wrap(security::SEC_INFO_PKT, &ts)
        }
        // TLS/plaintext path: no RDP client random, so never a reconnect cookie
        // here; client_info_payload already appends the balanced perf flags.
        None => security::client_info_payload(&info),
    };
    send_payload(stream, user_id, server.io_channel_id, &client_info_payload)?;
    tracing::info!("sent Client Info");

    // 6) Complete the licensing exchange, then wait for Demand Active.
    let share_id = recv_demand_active(stream, &mut sec, user_id, server.io_channel_id)?;
    tracing::info!(share_id, "received Demand Active");

    // 7) Confirm Active + finalization.
    let confirm_active = capabilities::confirm_active(
        share_id,
        user_id,
        config.width,
        config.height,
        core.keyboard_layout,
        config.enable_rfx,
    );
    tracing::debug!(
        len = confirm_active.len(),
        bytes = %confirm_active.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "Confirm Active PDU"
    );
    send_share(
        stream,
        user_id,
        server.io_channel_id,
        &mut sec,
        &confirm_active,
    )?;
    let io = server.io_channel_id;
    send_share(
        stream,
        user_id,
        io,
        &mut sec,
        &finalization::synchronize_pdu(share_id, user_id, io),
    )?;
    send_share(
        stream,
        user_id,
        io,
        &mut sec,
        &finalization::control_pdu(share_id, user_id, finalization::CTRLACTION_COOPERATE),
    )?;
    send_share(
        stream,
        user_id,
        io,
        &mut sec,
        &finalization::control_pdu(share_id, user_id, finalization::CTRLACTION_REQUEST_CONTROL),
    )?;
    send_share(
        stream,
        user_id,
        io,
        &mut sec,
        &finalization::font_list_pdu(share_id, user_id),
    )?;
    tracing::info!("sent Confirm Active + finalization");

    // 8) Wait for the server Font Map → Active.
    recv_until(stream, &mut sec, user_id, server.io_channel_id, "Font Map", |payload| {
        (finalization::data_pdu_type2(payload) == Some(finalization::PDUTYPE2_FONTMAP))
            .then_some(())
    })?;

    let encrypted = sec.is_some();
    let (inbound, outbound) = match sec {
        Some(s) => {
            let (decrypt, out) = s.split();
            (
                Some(decrypt),
                Some(std::sync::Arc::new(std::sync::Mutex::new(out))),
            )
        }
        None => (None, None),
    };

    Ok(ActiveSession {
        info: SessionInfo {
            user_channel_id: user_id,
            io_channel_id: server.io_channel_id,
            share_id,
            channel_ids: server.channel_ids,
            channel_names: advertised_channels,
            encrypted,
        },
        inbound,
        outbound,
        cursor_cache: std::collections::HashMap::new(),
        clipboard: ClipboardState::default(),
        audio: AudioState::default(),
        rdpdr: {
            let mut s = RdpdrState::default();
            for path in &config.drive_paths {
                let root = std::path::PathBuf::from(path);
                let name = rdp_channels::rdpdr::dos_name_for(&root);
                s.channel.add_drive(root, name.clone());
                tracing::info!(path, name, "sharing local path as a redirected drive");
            }
            s
        },
        width: config.width,
        height: config.height,
        keyboard_layout: core.keyboard_layout,
        enable_rfx: config.enable_rfx,
    })
}

/// A live, activated RDP session. Holds the inbound (decrypt) cipher used to
/// keep reading server PDUs, and the shared outbound half used both here (for
/// channel replies and reactivation) and by the [`InputSender`] built via
/// [`ActiveSession::take_input_sender`].
pub struct ActiveSession {
    info: SessionInfo,
    inbound: Option<InboundCrypto>,
    outbound: Option<SharedOutbound>,
    /// Server-cached cursor shapes; a `Cached` pointer update reuses one by
    /// index instead of re-sending the bitmap.
    cursor_cache: std::collections::HashMap<u16, rdp_graphics::pointer::CursorShape>,
    /// Clipboard (cliprdr) channel state + reassembly + the OS clipboard.
    clipboard: ClipboardState,
    /// Audio (rdpsnd) channel state + reassembly + the OS audio output.
    audio: AudioState,
    /// Device redirection (rdpdr) handshake state + reassembly.
    rdpdr: RdpdrState,
    /// Capabilities we re-send on a Deactivate All → reactivation (same size).
    width: u16,
    height: u16,
    keyboard_layout: u32,
    /// Whether RemoteFX was advertised; re-sent verbatim on reactivation.
    enable_rfx: bool,
}

/// Clipboard channel state: the protocol machine, the inbound chunk reassembler,
/// and the OS clipboard provider (a no-op until the platform installs a real
/// one via [`ActiveSession::set_clipboard_provider`]).
struct ClipboardState {
    channel: rdp_channels::cliprdr::ClipboardChannel,
    reasm: rdp_channels::svc::Reassembler,
    provider: Box<dyn rdp_channels::cliprdr::ClipboardProvider + Send>,
}

impl Default for ClipboardState {
    fn default() -> Self {
        Self {
            channel: rdp_channels::cliprdr::ClipboardChannel::new(),
            reasm: rdp_channels::svc::Reassembler::new(),
            provider: Box::new(rdp_channels::cliprdr::NoopClipboard),
        }
    }
}

/// Audio channel state: the RDPSND machine, the inbound chunk reassembler, and
/// the OS audio sink (a null sink that drops audio until a real one is set).
struct AudioState {
    channel: rdp_channels::rdpsnd::RdpsndChannel,
    reasm: rdp_channels::svc::Reassembler,
    sink: Box<dyn rdp_channels::rdpsnd::AudioSink + Send>,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            channel: rdp_channels::rdpsnd::RdpsndChannel::new(),
            reasm: rdp_channels::svc::Reassembler::new(),
            sink: Box::new(rdp_channels::rdpsnd::NullAudio),
        }
    }
}

/// Device-redirection channel state: the RDPDR handshake machine + reassembler.
#[derive(Default)]
struct RdpdrState {
    channel: rdp_channels::rdpdr::RdpdrChannel,
    reasm: rdp_channels::svc::Reassembler,
}

impl ActiveSession {
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Build an [`InputSender`] over `stream` (typically a clone of the session
    /// socket). The outbound cipher is SHARED with the session — the worker
    /// still needs it for channel replies (cliprdr/rdpdr/drdynvc) and
    /// reactivation; taking it away made those go out unencrypted, which an
    /// encryption-required server answers with a hard TCP reset right after
    /// activation. Both writers serialize on the cipher lock, held across the
    /// socket write so keystream order matches wire order.
    pub fn take_input_sender<S: Write>(&mut self, stream: S) -> InputSender<S> {
        InputSender {
            stream,
            crypto: self.outbound.clone(),
            user_id: self.info.user_channel_id,
            io_channel: self.info.io_channel_id,
            share_id: self.info.share_id,
        }
    }

    /// Send `payload` on `channel_id` inside an MCS Send Data Request, RC4/MAC-
    /// wrapping it when the session still holds the outbound cipher (legacy);
    /// otherwise sent raw (the TLS tunnel provides confidentiality). Used to
    /// answer DRDYNVC/frame-acks (graphics) and clipboard (both paths).
    pub fn send_channel<S: Write>(
        &mut self,
        stream: &mut S,
        channel_id: u16,
        payload: &[u8],
    ) -> Result<(), ActivateError> {
        match self.outbound.as_ref() {
            Some(c) => {
                // Hold the cipher lock across the write (see [`SharedOutbound`]).
                let mut c = lock_outbound(c);
                let wrapped = c.wrap(0, payload);
                send_payload(stream, self.info.user_channel_id, channel_id, &wrapped)
            }
            None => send_payload(stream, self.info.user_channel_id, channel_id, payload),
        }
    }

    /// Send a DRDYNVC PDU on the (static) `drdynvc` channel, framing it with the
    /// `CHANNEL_PDU_HEADER` the static-VC layer requires — but WITHOUT
    /// `CHANNEL_FLAG_SHOW_PROTOCOL`, since drdynvc isn't opened with that option
    /// (the server strips the header before handing the DVC PDU to its manager).
    /// Sending DVC data without this header makes the server report
    /// ERRINFO_VCHANNELDATATOOSHORT and tear the connection down.
    #[cfg(windows)]
    fn send_dvc<S: Write>(
        &mut self,
        stream: &mut S,
        channel_id: u16,
        dvc_pdu: &[u8],
    ) -> Result<(), ActivateError> {
        for piece in rdp_channels::svc::chunks_dvc(dvc_pdu) {
            self.send_channel(stream, channel_id, &piece)?;
        }
        Ok(())
    }

    /// Install the OS clipboard provider, replacing the default no-op (so
    /// remote↔local clipboard text actually transfers). Platform-supplied.
    #[cfg(windows)]
    pub fn set_clipboard_provider(
        &mut self,
        provider: Box<dyn rdp_channels::cliprdr::ClipboardProvider + Send>,
    ) {
        self.clipboard.provider = provider;
    }

    /// Feed one inbound cliprdr channel chunk: reassemble it, run the clipboard
    /// state machine against the OS provider, and send any responses back on the
    /// cliprdr channel (each SVC-chunked). Unknown/partial input is a no-op.
    fn handle_clipboard<S: Write>(
        &mut self,
        stream: &mut S,
        chunk: &[u8],
    ) -> Result<(), ActivateError> {
        let Some(cliprdr_id) = self.info.channel_id(rdp_pdu::gcc::CLIPRDR_CHANNEL) else {
            return Ok(());
        };
        let Some(msg) = self.clipboard.reasm.push(chunk) else {
            return Ok(());
        };
        let responses = self
            .clipboard
            .channel
            .process(&msg, self.clipboard.provider.as_mut());
        for resp in responses {
            for piece in rdp_channels::svc::chunks(&resp) {
                self.send_channel(stream, cliprdr_id, &piece)?;
            }
        }
        Ok(())
    }

    /// Start fetching the clipboard files the session offered, if a local paste
    /// is waiting for them. This is the lazy half of file redirection: nothing
    /// moves until [`request_clipboard_files`] flags a paste.
    #[cfg(windows)]
    fn pump_clipboard_files<S: Write>(&mut self, stream: &mut S) -> Result<(), ActivateError> {
        if !take_clipboard_file_request() {
            return Ok(());
        }
        let Some(cliprdr_id) = self.info.channel_id(rdp_pdu::gcc::CLIPRDR_CHANNEL) else {
            clipboard_files_ready(Vec::new());
            return Ok(());
        };
        let requests = self
            .clipboard
            .channel
            .begin_file_fetch(self.clipboard.provider.as_mut());
        if requests.is_empty() {
            // Nothing to fetch (or a fetch is already running) — don't leave a
            // paste blocked waiting for us.
            clipboard_files_ready(Vec::new());
            return Ok(());
        }
        tracing::info!("paste requested the session's clipboard files; transferring");
        for req in requests {
            for piece in rdp_channels::svc::chunks(&req) {
                self.send_channel(stream, cliprdr_id, &piece)?;
            }
        }
        Ok(())
    }

    /// Re-advertise the local clipboard to the server (called when the local
    /// clipboard changed), so the remote can paste from us. No-op before the
    /// cliprdr handshake completes or if the channel isn't present.
    fn announce_clipboard<S: Write>(&mut self, stream: &mut S) -> Result<(), ActivateError> {
        let Some(cliprdr_id) = self.info.channel_id(rdp_pdu::gcc::CLIPRDR_CHANNEL) else {
            return Ok(());
        };
        let has_text = self.clipboard.provider.get_text().is_some();
        let files = self.clipboard.provider.get_files();
        let has_files = !files.is_empty();
        let has_image = self.clipboard.provider.get_image().is_some();
        // What we advertise here decides whether the session ever asks for the
        // data at all, so it is the first thing to check when a copy silently
        // does nothing on the far side.
        tracing::info!(
            text = has_text,
            files = files.len(),
            image = has_image,
            "local clipboard changed; advertising to the session"
        );
        if let Some(msg) = self
            .clipboard
            .channel
            .announce_local(has_text, has_files, has_image)
        {
            for piece in rdp_channels::svc::chunks(&msg) {
                self.send_channel(stream, cliprdr_id, &piece)?;
            }
        }
        Ok(())
    }

    /// Install the OS audio sink, replacing the default null sink (so remote
    /// audio actually plays). Platform-supplied.
    #[cfg(windows)]
    pub fn set_audio_sink(
        &mut self,
        sink: Box<dyn rdp_channels::rdpsnd::AudioSink + Send>,
    ) {
        self.audio.sink = sink;
    }

    /// Redirect a local printer into the session (announced over rdpdr; the
    /// server's print jobs are spooled to `sink`). Call before the session loop
    /// runs the rdpdr handshake. Platform-supplied.
    #[cfg(windows)]
    pub fn set_printer(
        &mut self,
        print_name: String,
        driver_name: String,
        sink: Box<dyn rdp_channels::rdpdr::PrinterSink>,
    ) {
        self.rdpdr.channel.set_printer(print_name, driver_name, sink);
    }

    /// Feed one inbound rdpsnd channel chunk: reassemble, run the RDPSND state
    /// machine against the OS audio sink, and send any responses (format reply,
    /// training echo, wave confirms) back on the rdpsnd channel.
    fn handle_audio<S: Write>(&mut self, stream: &mut S, chunk: &[u8]) -> Result<(), ActivateError> {
        let Some(rdpsnd_id) = self.info.channel_id(rdp_pdu::gcc::RDPSND_CHANNEL) else {
            return Ok(());
        };
        let Some(msg) = self.audio.reasm.push(chunk) else {
            return Ok(());
        };
        let responses = self.audio.channel.process(&msg, self.audio.sink.as_mut());
        for resp in responses {
            for piece in rdp_channels::svc::chunks(&resp) {
                self.send_channel(stream, rdpsnd_id, &piece)?;
            }
        }
        Ok(())
    }

    /// Run one reassembled RDPSND PDU that arrived on the `AUDIO_PLAYBACK_DVC`
    /// dynamic channel through the same RDPSND state machine and OS sink as the
    /// static `rdpsnd` path, returning the responses to wrap as DVC data. Modern
    /// Windows streams the session's audio over this dynamic channel rather than
    /// the static SVC, so without this the audio never reaches the sink.
    pub fn process_audio_dvc(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        self.audio.channel.process(msg, self.audio.sink.as_mut())
    }

    /// Feed one inbound rdpdr channel chunk: reassemble, run the device-
    /// redirection handshake, and send responses on the rdpdr channel. No
    /// devices are shared yet, so this just brings the channel up cleanly.
    fn handle_rdpdr<S: Write>(&mut self, stream: &mut S, chunk: &[u8]) -> Result<(), ActivateError> {
        let Some(rdpdr_id) = self.info.channel_id(rdp_pdu::gcc::RDPDR_CHANNEL) else {
            return Ok(());
        };
        let Some(msg) = self.rdpdr.reasm.push(chunk) else {
            return Ok(());
        };
        for resp in self.rdpdr.channel.process(&msg) {
            for piece in rdp_channels::svc::chunks(&resp) {
                self.send_channel(stream, rdpdr_id, &piece)?;
            }
        }
        Ok(())
    }

    /// Send a batch of client input events on the I/O channel. On the TLS path
    /// (no outbound RC4 cipher) this uses **fast-path input** (`TS_FP_INPUT_PDU`)
    /// — a tiny header written straight to the stream that the server injects
    /// immediately, the same mechanism mstsc uses for low input latency. On the
    /// legacy RC4 path it falls back to the slow-path Input Event PDU (fast-path
    /// input under Standard Security needs its own MAC/checksum, not worth it).
    /// This lets the TLS worker — which owns the single, unsplittable SChannel
    /// context — send input itself; the legacy path uses a separate
    /// [`InputSender`] thread.
    #[cfg(windows)]
    pub fn send_input<S: Write>(
        &mut self,
        stream: &mut S,
        events: &[rdp_pdu::input::EventBytes],
    ) -> Result<(), ActivateError> {
        if events.is_empty() {
            return Ok(());
        }
        // TLS path: fast-path input straight onto the stream (no TPKT/MCS wrap).
        // The server tells it apart from a TPKT by the first byte's action bits.
        if self.outbound.is_none() {
            if let Some(fp) = rdp_pdu::fastpath::input_pdu(events) {
                stream.write_all(&fp)?;
                stream.flush()?;
                return Ok(());
            }
        }
        let share =
            rdp_pdu::input::input_pdu(self.info.share_id, self.info.user_channel_id, events);
        match self.outbound.as_ref() {
            Some(c) => {
                // Hold the cipher lock across the write (see [`SharedOutbound`]).
                let mut c = lock_outbound(c);
                let payload = c.wrap(0, &share);
                send_payload(
                    stream,
                    self.info.user_channel_id,
                    self.info.io_channel_id,
                    &payload,
                )
            }
            None => send_payload(
                stream,
                self.info.user_channel_id,
                self.info.io_channel_id,
                &share,
            ),
        }
    }

    /// If `plaintext` is a slow-path Pointer Update PDU, decode it and forward a
    /// [`CursorUpdate`] to `sink`, maintaining the shape cache that `Cached`
    /// updates reference. Returns whether it was a recognised pointer update.
    /// Shared by the legacy and graphics loops.
    fn handle_pointer<F: FrameSink>(&mut self, plaintext: &[u8], sink: &mut F) -> bool {
        use rdp_graphics::pointer::{self, PointerUpdate};
        if finalization::data_pdu_type2(plaintext) != Some(finalization::PDUTYPE2_POINTER) {
            return false;
        }
        // Pointer data follows the 18-byte Share Data Header.
        let Some(body) = plaintext.get(18..) else {
            return false;
        };
        let Some(update) = pointer::parse_pointer_update(body) else {
            // A pointer PDU we could not decode: the cursor silently keeps its
            // previous shape — make that observable instead of invisible.
            let message_type = body
                .get(..2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]));
            tracing::debug!(
                ?message_type,
                len = body.len(),
                "pointer update parse failed; cursor unchanged"
            );
            return true;
        };
        if let PointerUpdate::Shape { cache_index, shape } = &update {
            tracing::debug!(
                cache_index,
                w = shape.width,
                h = shape.height,
                "pointer shape update"
            );
        }
        match update {
            PointerUpdate::Hidden => sink.cursor(CursorUpdate::Hide),
            PointerUpdate::SystemDefault => sink.cursor(CursorUpdate::Default),
            // The local mouse drives position; a server-driven move is ignored.
            PointerUpdate::Position { .. } => {}
            PointerUpdate::Cached { cache_index } => match self.cursor_cache.get(&cache_index) {
                Some(shape) => sink.cursor(cursor_update_from_shape(shape)),
                None => tracing::debug!(cache_index, "cached pointer not seen yet; ignoring"),
            },
            PointerUpdate::Shape { cache_index, shape } => {
                sink.cursor(cursor_update_from_shape(&shape));
                self.cursor_cache.insert(cache_index, shape);
            }
        }
        true
    }

    /// Send a Share Control/Data PDU, RC4/MAC-wrapping it with the outbound
    /// cipher on the legacy path (raw under TLS) — the post-activation analogue
    /// of [`send_share`], which uses the not-yet-split `SecuritySession`.
    fn send_share_outbound<S: Write>(
        &mut self,
        stream: &mut S,
        share_pdu: &[u8],
    ) -> Result<(), ActivateError> {
        match self.outbound.as_ref() {
            Some(c) => {
                // Hold the cipher lock across the write (see [`SharedOutbound`]).
                let mut c = lock_outbound(c);
                let payload = c.wrap(0, share_pdu);
                send_payload(
                    stream,
                    self.info.user_channel_id,
                    self.info.io_channel_id,
                    &payload,
                )
            }
            None => send_payload(
                stream,
                self.info.user_channel_id,
                self.info.io_channel_id,
                share_pdu,
            ),
        }
    }

    /// React to a server Deactivate All by re-running the capability exchange:
    /// wait for the new Demand Active, re-send Confirm Active + finalization
    /// (same size/caps), and wait for the Font Map. The session resumes on the
    /// new share id. A size change isn't applied to the framebuffer yet, so this
    /// covers in-place reactivations (reconnect, shadow/control start, the
    /// server resetting the session) rather than live desktop resizes.
    fn reactivate<S: Read + Write>(&mut self, stream: &mut S) -> Result<(), ActivateError> {
        // 1) Wait for the new Demand Active.
        let mut share_id = None;
        for _ in 0..64 {
            let pdu = read_tpkt_pdu(stream)?;
            let (_ch, payload) = mcs::parse_send_data_indication(&pdu)?;
            let plaintext = unwrap_inbound(self.inbound.as_mut(), &payload);
            note_session_events(&plaintext);
            if let Ok(id) = capabilities::parse_demand_active(&plaintext) {
                note_server_input_flags(&plaintext);
                share_id = Some(id);
                break;
            }
        }
        let share_id =
            share_id.ok_or_else(|| proto_err("no Demand Active after Deactivate All"))?;
        self.info.share_id = share_id;

        // 2) Confirm Active + finalization, re-using the original size/caps.
        let user_id = self.info.user_channel_id;
        let io = self.info.io_channel_id;
        let confirm = capabilities::confirm_active(
            share_id,
            user_id,
            self.width,
            self.height,
            self.keyboard_layout,
            self.enable_rfx,
        );
        self.send_share_outbound(stream, &confirm)?;
        self.send_share_outbound(stream, &finalization::synchronize_pdu(share_id, user_id, io))?;
        self.send_share_outbound(
            stream,
            &finalization::control_pdu(share_id, user_id, finalization::CTRLACTION_COOPERATE),
        )?;
        self.send_share_outbound(
            stream,
            &finalization::control_pdu(share_id, user_id, finalization::CTRLACTION_REQUEST_CONTROL),
        )?;
        self.send_share_outbound(stream, &finalization::font_list_pdu(share_id, user_id))?;

        // 3) Wait for the server Font Map → reactivated.
        for _ in 0..64 {
            let pdu = read_tpkt_pdu(stream)?;
            let (_ch, payload) = mcs::parse_send_data_indication(&pdu)?;
            let plaintext = unwrap_inbound(self.inbound.as_mut(), &payload);
            note_session_events(&plaintext);
            if finalization::data_pdu_type2(&plaintext) == Some(finalization::PDUTYPE2_FONTMAP) {
                tracing::info!(share_id, "reactivated after Deactivate All");
                return Ok(());
            }
        }
        Err(proto_err("no Font Map after reactivation"))
    }
}

/// Build an owned [`CursorUpdate::Shape`] from a decoded cursor shape.
fn cursor_update_from_shape(s: &rdp_graphics::pointer::CursorShape) -> CursorUpdate {
    CursorUpdate::Shape {
        width: s.width,
        height: s.height,
        hot_x: s.hot_x,
        hot_y: s.hot_y,
        rgba: s.rgba.clone(),
    }
}

/// Sends client input (slow-path Input Event PDUs) to the server, RC4/MAC-
/// wrapping them when the session is encrypted. The cipher is shared with the
/// session worker (see [`SharedOutbound`]); the lock is held across the socket
/// write so the keystream order matches the wire order. Generic over the
/// writable stream so it can be unit-tested without a socket.
pub struct InputSender<S: Write> {
    stream: S,
    crypto: Option<SharedOutbound>,
    user_id: u16,
    io_channel: u16,
    share_id: u32,
}

impl<S: Write> InputSender<S> {
    /// Frame and send a batch of input events. A no-op for an empty batch.
    pub fn send(&mut self, events: &[rdp_pdu::input::EventBytes]) -> Result<(), ActivateError> {
        if events.is_empty() {
            return Ok(());
        }
        let share = rdp_pdu::input::input_pdu(self.share_id, self.user_id, events);
        match self.crypto.as_ref() {
            Some(c) => {
                // Hold the cipher lock across the write (see [`SharedOutbound`]).
                let mut c = lock_outbound(c);
                let payload = c.wrap(0, &share);
                let request = mcs::send_data_request(self.user_id, self.io_channel, &payload);
                self.stream.write_all(&mcs::frame(&request)?)?;
                self.stream.flush()?;
            }
            None => {
                let request = mcs::send_data_request(self.user_id, self.io_channel, &share);
                self.stream.write_all(&mcs::frame(&request)?)?;
                self.stream.flush()?;
            }
        }
        Ok(())
    }
}

/// A pointer (mouse cursor) change for the platform to realise. Carries owned
/// pixel data so it can cross the worker→UI thread channel.
#[derive(Debug, Clone)]
pub enum CursorUpdate {
    /// Hide the cursor over the session surface.
    Hide,
    /// Fall back to the platform's default arrow.
    Default,
    /// A concrete shape: top-down RGBA8 (`width*height*4`) with a hotspot.
    Shape {
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        rgba: Vec<u8>,
    },
}

/// A consumer of decoded screen rectangles (implemented by the GPU renderer on
/// Windows, or a logging/test sink elsewhere).
pub trait FrameSink {
    /// Blit a `w`x`h` RGBA8 rectangle to (`x`,`y`) on the framebuffer.
    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]);
    /// Blit a `w`x`h` RGBA8 rectangle, taking ownership of `rgba`. The decoder
    /// already produced an owned buffer, so a sink that ships frames to another
    /// thread can MOVE it instead of copying. Default: borrow and delegate to
    /// [`blit`] (for sinks that don't cross a thread boundary).
    fn blit_owned(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: Vec<u8>) {
        self.blit(x, y, w, h, &rgba);
    }
    /// Blit a `w`x`h` NV12 frame (Y plane then interleaved UV, stride `w`) at
    /// (`x`,`y`), painting only the frame-relative dirty `rects` (empty = whole
    /// frame). Default: convert to RGBA on the CPU and delegate to [`blit`], so
    /// sinks without a GPU path still render. The windowed driver overrides
    /// this to convert on the GPU (D3D11 video processor).
    #[cfg(windows)]
    fn blit_nv12(&mut self, x: u16, y: u16, w: u16, h: u16, nv12: &[u8], rects: &[(u16, u16, u16, u16)]) {
        let (yp, uv) = nv12.split_at((w as usize) * (h as usize));
        let Some(rgba) =
            rdp_graphics::yuv::nv12_to_rgba(yp, uv, w as usize, h as usize, w as usize)
        else {
            return;
        };
        if rects.is_empty() {
            self.blit(x, y, w, h, &rgba);
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
            self.blit(x + rx, y + ry, cw as u16, ch as u16, &cropped);
        }
    }
    /// Blit a GPU NV12 texture (zero-copy DXVA decode) at (`x`,`y`), painting
    /// only the dirty `rects`. Default: drop it (a sink without a GPU can't use
    /// a GPU texture); the windowed driver color-converts it on the GPU.
    #[cfg(windows)]
    fn blit_texture(
        &mut self,
        _x: u16,
        _y: u16,
        _w: u16,
        _h: u16,
        _texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        _rects: &[(u16, u16, u16, u16)],
    ) {
    }
    /// Copy a framebuffer rectangle (`sx`,`sy`,`w`,`h`) to (`dx`,`dy`) on the GPU
    /// (EGFX SurfaceToSurface). Default: ignore — only the windowed driver, which
    /// owns the framebuffer, can do this.
    #[cfg(windows)]
    fn copy_rect(&mut self, _sx: u16, _sy: u16, _w: u16, _h: u16, _dx: u16, _dy: u16) {}
    /// Stash a framebuffer rectangle into GPU cache `slot` (EGFX SurfaceToCache).
    #[cfg(windows)]
    fn cache_rect(&mut self, _slot: u16, _sx: u16, _sy: u16, _w: u16, _h: u16) {}
    /// Blit GPU cache `slot` onto the framebuffer at (`dx`,`dy`) (EGFX
    /// CacheToSurface).
    #[cfg(windows)]
    fn cache_blit(&mut self, _slot: u16, _dx: u16, _dy: u16) {}
    /// Present the accumulated framebuffer.
    fn present(&mut self);
    /// Apply a pointer (cursor) change. Default: ignore — sinks that don't draw
    /// a cursor (tests, logging) need not implement it.
    fn cursor(&mut self, _update: CursorUpdate) {}
    /// The server offered an auto-reconnect cookie (for resuming after a drop).
    /// Default: ignore — only the windowed driver, which can reconnect, uses it.
    fn reconnect_cookie(&mut self, _cookie: rdp_pdu::logon::ReconnectCookie) {}
    /// The server reset the desktop to `w`×`h` (e.g. after a Display Control
    /// resize). Default: ignore — the windowed driver resizes its framebuffer.
    /// Only the Windows graphics loop drives this.
    #[cfg(windows)]
    fn resize(&mut self, _w: u16, _h: u16) {}
}

/// A microphone capture device for audio-input redirection (MS-RDPEAI). The
/// graphics session loop starts it when the server opens the `AUDIO_INPUT`
/// channel and polls it each wakeup, forwarding captured PCM to the server.
/// Abstracted so the protocol wiring stays platform-independent and testable.
#[cfg(windows)]
pub trait MicSource: Send {
    /// Begin capturing PCM in this format. Called once, when capture opens.
    fn start(&mut self, channels: u16, samples_per_sec: u32, bits_per_sample: u16);
    /// Return PCM captured since the last poll (empty if none is ready).
    fn poll(&mut self) -> Vec<u8>;
}

/// Outcome of processing a single server PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pump {
    /// At least one bitmap rectangle was blitted into the sink; the caller
    /// should present a frame.
    Painted,
    /// A PDU was consumed but produced no new pixels (control PDU, empty or
    /// fully-compressed update, non-graphics data).
    Idle,
}

/// Read exactly one server PDU and paint any uncompressed bitmap rectangles it
/// carries into `sink` (without presenting). Returns [`Pump::Painted`] when a
/// rectangle was blitted so the caller can present.
///
/// I/O errors propagate; the caller decides whether a read timeout
/// (`WouldBlock`/`TimedOut`) is fatal or simply "no data yet".
pub fn pump_once<S: Read + Write, F: FrameSink>(
    stream: &mut S,
    session: &mut ActiveSession,
    sink: &mut F,
) -> Result<Pump, ActivateError> {
    // Push any local clipboard change to the server before blocking on a read.
    if take_clipboard_changed() {
        session.announce_clipboard(stream)?;
    }
    // ...and start staging the session's clipboard files if a paste wants them.
    #[cfg(windows)]
    session.pump_clipboard_files(stream)?;

    let pdu = read_tpkt_pdu(stream)?;
    let (channel, payload) = mcs::parse_send_data_indication(&pdu)?;
    let plaintext = unwrap_inbound(session.inbound.as_mut(), &payload);

    // Clipboard and audio ride their own static channels; route and we're done.
    if Some(channel) == session.info.channel_id(rdp_pdu::gcc::CLIPRDR_CHANNEL) {
        session.handle_clipboard(stream, &plaintext)?;
        return Ok(Pump::Idle);
    }
    if Some(channel) == session.info.channel_id(rdp_pdu::gcc::RDPSND_CHANNEL) {
        session.handle_audio(stream, &plaintext)?;
        return Ok(Pump::Idle);
    }
    if Some(channel) == session.info.channel_id(rdp_pdu::gcc::RDPDR_CHANNEL) {
        session.handle_rdpdr(stream, &plaintext)?;
        return Ok(Pump::Idle);
    }

    note_session_events(&plaintext);
    capture_reconnect_cookie(&plaintext, sink);
    // A server Deactivate All means "re-run the capability exchange"; do so
    // in-line so the session survives reconnect/shadow/reset events.
    if capabilities::is_deactivate_all(&plaintext) {
        tracing::info!("server Deactivate All; reactivating");
        session.reactivate(stream)?;
        return Ok(Pump::Idle);
    }
    let painted = paint_bitmap_update(&plaintext, sink);
    // Cursor updates ride the same I/O channel; they don't count as a painted
    // frame (the platform realises them as a hardware cursor, no present).
    session.handle_pointer(&plaintext, sink);

    Ok(if painted { Pump::Painted } else { Pump::Idle })
}

/// If `plaintext` is a slow-path Bitmap Update share-data PDU, decode its
/// rectangles (decompressing interleaved-RLE when flagged) and blit each into
/// `sink`. Returns whether any rectangle was painted. Shared by the legacy and
/// graphics session loops.
fn paint_bitmap_update<F: FrameSink>(plaintext: &[u8], sink: &mut F) -> bool {
    if finalization::data_pdu_type2(plaintext) != Some(finalization::PDUTYPE2_UPDATE) {
        return false; // not a graphics update
    }
    // Update body follows the 18-byte Share Data Header.
    let Some(body) = plaintext.get(18..) else {
        return false;
    };
    let Some(update) = rdp_graphics::bitmap::parse_bitmap_update(body) else {
        return false;
    };
    tracing::debug!(
        rectangles = update.rectangles.len(),
        "bitmap update received"
    );

    let mut painted = false;
    for rect in &update.rectangles {
        // Compressed rectangles are interleaved-RLE; decode to raw pixels first.
        // Both paths then convert the (bottom-up) raw bytes to RGBA8.
        let raw;
        let pixels: &[u8] = if rect.compressed {
            match rdp_graphics::bitmap::decompress_interleaved(
                &rect.data,
                rect.width,
                rect.height,
                rect.bits_per_pixel,
            ) {
                Some(decoded) => {
                    raw = decoded;
                    &raw
                }
                None => {
                    tracing::debug!(
                        w = rect.width,
                        h = rect.height,
                        bpp = rect.bits_per_pixel,
                        "interleaved-RLE decode failed; skipping rect"
                    );
                    continue;
                }
            }
        } else {
            &rect.data
        };

        if let Some(rgba) = rdp_graphics::bitmap::to_rgba(
            pixels,
            rect.width,
            rect.height,
            rect.bits_per_pixel,
            true,
        ) {
            sink.blit(
                rect.dest_left,
                rect.dest_top,
                rect.width,
                rect.height,
                &rgba,
            );
            painted = true;
        }
    }
    painted
}

/// Log the server session-lifecycle PDUs we care about: a successful logon
/// (Save Session Info) and a disconnect reason (Set Error Info). Cheap to call
/// on every inbound PDU — both checks are a `pduType2` comparison first.
fn note_session_events(plaintext: &[u8]) {
    note_error_info(plaintext);
    note_logon(plaintext);
}

/// Log a server Save Session Info PDU — confirmation that authentication
/// succeeded, with the `DOMAIN\user` and session id the server assigned.
fn note_logon(plaintext: &[u8]) {
    use rdp_pdu::logon::{parse_save_session_info, SaveSessionInfo};
    match parse_save_session_info(plaintext) {
        Some(SaveSessionInfo::Logon(l)) => tracing::info!(
            domain = %l.domain,
            username = %l.username,
            session_id = l.session_id,
            "server confirmed logon"
        ),
        Some(other) => tracing::debug!(?other, "server save-session-info"),
        None => {}
    }
}

/// If `plaintext` is a Save Session Info carrying an auto-reconnect cookie,
/// hand it to the sink (the windowed driver stores it to reconnect after a drop).
fn capture_reconnect_cookie<F: FrameSink>(plaintext: &[u8], sink: &mut F) {
    if let Some(rdp_pdu::logon::SaveSessionInfo::Extended { cookie: Some(c) }) =
        rdp_pdu::logon::parse_save_session_info(plaintext)
    {
        tracing::info!(logon_id = c.logon_id, "received server auto-reconnect cookie");
        sink.reconnect_cookie(c);
    }
}

/// Log a server Set Error Info PDU when present. A non-zero code is the reason
/// the server is tearing the session down (idle timeout, logged on elsewhere,
/// access denied, …); surfacing it turns an opaque close into a clear cause.
fn note_error_info(plaintext: &[u8]) {
    if let Some(code) = rdp_pdu::errinfo::parse_set_error_info(plaintext) {
        if code == rdp_pdu::errinfo::ERRINFO_NONE {
            tracing::debug!("server Set Error Info: none");
        } else {
            tracing::warn!(
                code = format_args!("0x{code:08X}"),
                reason = rdp_pdu::errinfo::describe(code),
                "server Set Error Info (disconnect reason)"
            );
        }
    }
}

/// After activation, read update PDUs and paint bitmap updates into `sink`.
/// Runs until the connection closes or errors (bounded only by the stream).
pub fn run_session<S: Read + Write, F: FrameSink>(
    stream: &mut S,
    session: &mut ActiveSession,
    sink: &mut F,
) -> Result<(), ActivateError> {
    loop {
        if pump_once(stream, session, sink)? == Pump::Painted {
            sink.present();
        }
    }
}

/// A decoded rectangle ready to blit at (`x`,`y`). Either already-RGBA pixels
/// (uncompressed surfaces, AVC444 full-chroma) or an NV12 frame (AVC420) that
/// the sink converts on the GPU when it can, else on the CPU.
#[cfg(windows)]
pub enum GfxBlit {
    /// Top-down RGBA8, `w*h*4` bytes.
    Rgba {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        rgba: Vec<u8>,
    },
    /// NV12: a `w*h` Y plane then a `w*(h/2)` interleaved UV plane (stride `w`).
    /// `rects` are the frame-relative dirty regions (MS-RDPEGFX `regionRects`);
    /// only they are painted — outside them the decoded picture holds encoder
    /// reference content that may be stale. Empty = paint the whole frame.
    Nv12 {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        nv12: Vec<u8>,
        rects: Vec<(u16, u16, u16, u16)>,
    },
    /// A GPU NV12 texture (zero-copy DXVA decode) to color-convert on the GPU.
    /// `rects` as in [`GfxBlit::Nv12`].
    Texture {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        rects: Vec<(u16, u16, u16, u16)>,
    },
    /// Copy a `w`x`h` framebuffer rectangle from (`sx`,`sy`) to (`dx`,`dy`)
    /// entirely on the GPU (EGFX SurfaceToSurface). Reads the live framebuffer,
    /// so it is correct over H.264-painted regions — unlike a CPU-shadow read.
    CopyRect {
        sx: u16,
        sy: u16,
        w: u16,
        h: u16,
        dx: u16,
        dy: u16,
    },
    /// Stash a `w`x`h` framebuffer rectangle from (`sx`,`sy`) into GPU cache
    /// `slot` (EGFX SurfaceToCache).
    CacheRect {
        slot: u16,
        sx: u16,
        sy: u16,
        w: u16,
        h: u16,
    },
    /// Blit GPU cache `slot` onto the framebuffer at (`dx`,`dy`) (EGFX
    /// CacheToSurface).
    CacheBlit { slot: u16, dx: u16, dy: u16 },
}

/// Turns an EGFX surface command into RGBA blits. The Windows implementation
/// decodes H.264 (AVC420/444) via Media Foundation; non-graphics commands
/// return no blits. (The demux/dispatch core it plugs into —
/// [`rdp_graphics::channel::GraphicsChannel`] — is unit-tested cross-platform.)
#[cfg(windows)]
pub trait GfxRenderer {
    fn render(&mut self, command: &rdp_pdu::gfx::GfxCommand) -> Vec<GfxBlit>;
}

/// Pop one complete TPKT PDU off the front of `buf`, if a whole one is present.
/// Returns `Ok(None)` when more bytes are still needed. Bytes are only removed
/// once a full PDU is available, so a caller can accumulate across reads.
#[cfg(windows)]
fn take_buffered_tpkt(buf: &mut Vec<u8>) -> std::io::Result<Option<Vec<u8>>> {
    use rdp_pdu::x224::{read_tpkt_len, TPKT_HEADER_LEN};
    if buf.len() < TPKT_HEADER_LEN {
        return Ok(None);
    }
    let total = read_tpkt_len(&buf[..TPKT_HEADER_LEN])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if total < TPKT_HEADER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TPKT length smaller than its header",
        ));
    }
    if buf.len() < total {
        return Ok(None);
    }
    // Hand the accumulated buffer over as the PDU (no copy) and keep only the
    // tail — usually empty or a small partial next PDU. The common one-PDU
    // case is a pure pointer swap.
    let rest = buf.split_off(total);
    let pdu = std::mem::replace(buf, rest);
    Ok(Some(pdu))
}

/// Read the next complete server PDU into `buf`, returning it — or `Ok(None)`
/// when the socket's read timeout elapsed before a full PDU arrived. Unlike
/// [`read_tpkt_pdu`], partial bytes are retained in `buf` across timeouts, so a
/// timeout between TLS records (or mid-PDU) never corrupts framing. Requires a
/// read timeout on the socket; without one it blocks like a plain read.
#[cfg(windows)]
fn poll_tpkt_pdu<R: Read>(stream: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::ErrorKind;
    loop {
        if let Some(pdu) = take_buffered_tpkt(buf)? {
            return Ok(Some(pdu));
        }
        // 64 KiB per read: an 8 KiB buffer costs ~8x the syscalls (and, on the
        // TLS path, ~8x the SChannel DecryptMessage calls) for the same bytes.
        // At gigabit that is the difference between ~2k and ~15k reads a second.
        // Read directly into `buf`'s tail — no intermediate stack buffer copy.
        let old = buf.len();
        buf.resize(old + 64 * 1024, 0);
        match stream.read(&mut buf[old..]) {
            Ok(0) => {
                buf.truncate(old);
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "server closed the connection",
                ));
            }
            Ok(n) => buf.truncate(old + n),
            // Windows can report a raced read timeout as a transient overlapped-I/O
            // status (ERROR_IO_PENDING 997 / WSA_IO_INCOMPLETE 996 /
            // ERROR_OPERATION_ABORTED 995) rather than TimedOut; treat all of them
            // as "no full PDU yet" so heavy on-screen motion never kills the loop.
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::TimedOut
                    || matches!(e.raw_os_error(), Some(995 | 996 | 997)) =>
            {
                buf.truncate(old);
                return Ok(None);
            }
            Err(e) => {
                buf.truncate(old);
                return Err(e);
            }
        }
    }
}

/// The decode/composite stage, run on a dedicated thread fed by
/// [`run_graphics_session`]. It owns the stateful [`GfxRenderer`] and a clone of
/// the frame sink. The network thread parses EGFX, acks frames *immediately*
/// (so the server's frame clock is no longer gated by our decode time), and
/// hands each message's owned `Vec<GfxCommand>` over `rx`.
///
/// This loop renders every command strictly in order — the ClearCodec/H.264
/// caches are sequential, so a tile can never be dropped or reordered — but when
/// several frames have queued it **coalesces presents**, painting all their
/// tiles and presenting only the newest, so a decode backlog never wastes GPU
/// time on frames the user will never see.
///
/// `backlog` mirrors the channel depth; the network thread reports it as the
/// RDPGFX frame-ack `queueDepth` so the server throttles its send rate to our
/// decode rate (the correct flow control for a stateful codec — the server
/// re-encodes from its current surface state rather than us skipping tiles). The
/// loop exits when the sender is dropped (the connection closed).
#[cfg(windows)]
pub fn run_decode_loop<F: FrameSink, R: GfxRenderer>(
    rx: std::sync::mpsc::Receiver<Vec<rdp_pdu::gfx::GfxCommand>>,
    renderer: &mut R,
    sink: &mut F,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    metrics: Option<std::sync::Arc<crate::metrics::Metrics>>,
) {
    use rdp_pdu::gfx::GfxCommand;
    use std::sync::atomic::Ordering;

    // Window for surfacing the achieved decode fps at info.
    let mut fps_log_start = std::time::Instant::now();
    let mut fps_log_frames = 0u32;

    while let Ok(first) = rx.recv() {
        // Drain everything already queued so we can coalesce their presents.
        let mut batches = vec![first];
        while let Ok(more) = rx.try_recv() {
            batches.push(more);
        }
        // Decrement the backlog by exactly what we drained (not store(0), which
        // would race with the network thread enqueuing concurrently).
        backlog.fetch_sub(batches.len(), Ordering::Relaxed);

        // Decode every tile in order, but present only the newest complete frame
        // in this drained burst (coalescing).
        let total_end_frames = batches
            .iter()
            .flatten()
            .filter(|c| matches!(c, GfxCommand::EndFrame { .. }))
            .count();
        let mut seen_end_frames = 0usize;
        let mut painted = false;
        let mut in_frame = false;
        let decode_start = std::time::Instant::now();
        let mut tiles = 0u32;

        for commands in &batches {
            for command in commands {
                match command {
                    GfxCommand::ResetGraphics { width, height, .. } => {
                        tracing::info!(width, height, "server reset desktop size");
                        sink.resize(*width as u16, *height as u16);
                    }
                    GfxCommand::StartFrame { .. } => in_frame = true,
                    GfxCommand::EndFrame { .. } => {
                        in_frame = false;
                        fps_log_frames += 1;
                    }
                    GfxCommand::WireToSurface1 { .. } => tiles += 1,
                    _ => {}
                }
                for blit in renderer.render(command) {
                    match blit {
                        GfxBlit::Rgba { x, y, w, h, rgba } => sink.blit_owned(x, y, w, h, rgba),
                        GfxBlit::Nv12 { x, y, w, h, nv12, rects } => {
                            sink.blit_nv12(x, y, w, h, &nv12, &rects)
                        }
                        GfxBlit::Texture { x, y, w, h, texture, rects } => {
                            sink.blit_texture(x, y, w, h, &texture, &rects)
                        }
                        GfxBlit::CopyRect { sx, sy, w, h, dx, dy } => {
                            sink.copy_rect(sx, sy, w, h, dx, dy)
                        }
                        GfxBlit::CacheRect { slot, sx, sy, w, h } => {
                            sink.cache_rect(slot, sx, sy, w, h)
                        }
                        GfxBlit::CacheBlit { slot, dx, dy } => sink.cache_blit(slot, dx, dy),
                    }
                    painted = true;
                }
                if matches!(command, GfxCommand::EndFrame { .. }) {
                    seen_end_frames += 1;
                    if seen_end_frames == total_end_frames && painted {
                        sink.present();
                        painted = false;
                    }
                }
            }
        }
        // A non-framed update (blits with no EndFrame) still needs presenting.
        if painted && !in_frame {
            sink.present();
        }

        if total_end_frames > 0 || tiles > 0 {
            let decode_us = decode_start.elapsed().as_micros() as u64;
            if let Some(m) = metrics.as_ref() {
                m.record_decode_us(decode_us);
            }
            tracing::debug!(
                target: "perf",
                bursts = batches.len(),
                frames = total_end_frames,
                tiles,
                decode_us,
                "decode burst"
            );
        }

        // Surface the achieved decode rate at info so host-side frame-rate tweaks
        // (DWMFRAMEINTERVAL) and render-scale changes are measurable from the client.
        // Quiet when idle: only logs while frames are actually flowing.
        let log_elapsed = fps_log_start.elapsed();
        if log_elapsed >= std::time::Duration::from_secs(2) {
            if fps_log_frames > 0 {
                let fps = fps_log_frames as f32 / log_elapsed.as_secs_f32();
                tracing::info!(fps = format!("{fps:.1}"), frames = fps_log_frames, "decode fps");
            }
            fps_log_start = std::time::Instant::now();
            fps_log_frames = 0;
        }
    }
    tracing::info!("decode loop ended");
}

/// Like [`run_session`], but also drives the RDPGFX dynamic channel: it demuxes
/// the static `drdynvc` channel into a [`rdp_graphics::channel::GraphicsChannel`],
/// sends that channel's DVC responses and frame-acks back over `stream`, and
/// hands decoded EGFX commands to a [`run_decode_loop`] worker thread (so decode
/// time no longer gates the server's frame clock). Bitmap updates on the I/O
/// channel are still painted directly (so a modern host renders even before — or
/// without — EGFX). Used on the TLS/NLA path, where the worker owns a read+write
/// transport; the legacy path uses [`run_session`].
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub fn run_graphics_session<S: Read + Write, F: FrameSink>(
    stream: &mut S,
    session: &mut ActiveSession,
    sink: &mut F,
    input_rx: &std::sync::mpsc::Receiver<Vec<rdp_pdu::input::EventBytes>>,
    mut mic: Option<&mut dyn MicSource>,
    udp_dial: Option<crate::udp::UdpDial>,
    gfx_caps: Vec<(u32, u32)>,
    decode_tx: std::sync::mpsc::Sender<Vec<rdp_pdu::gfx::GfxCommand>>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    metrics: Option<std::sync::Arc<crate::metrics::Metrics>>,
    stop: &std::sync::atomic::AtomicBool,
    redirector: Option<Box<dyn rdp_graphics::redirect::DvcRedirector>>,
    wait: Option<crate::net_wait::SocketWait>,
) -> Result<(), ActivateError> {
    use rdp_graphics::channel::GraphicsChannel;
    use rdp_pdu::gfx::GfxCommand;

    let io_channel = session.info.io_channel_id;
    let dvc_channel = session.info.channel_ids.first().copied();
    tracing::info!(
        io_channel,
        ?dvc_channel,
        ?gfx_caps,
        event_driven = wait.is_some(),
        "graphics session loop started"
    );

    let mut graphics = GraphicsChannel::with_caps(gfx_caps.clone());
    // Optional DVC redirector (e.g. the Teams WebRTC add-in host) that bridges
    // channels — like `com.microsoft.rdc.dvc.webrtc.1` — the mux would decline.
    // It produces data asynchronously on its own threads, so while one is wired
    // in the event wait below must keep a short tick to flush its queue.
    let redirector_active = redirector.is_some();
    if let Some(redirector) = redirector {
        graphics.set_redirector(redirector);
    }
    // Inbound drdynvc data rides the static VC framed in a CHANNEL_PDU_HEADER
    // (length+flags) and may span several ≤1600-byte chunks; this strips/
    // reassembles that wrapper so the graphics demuxer sees a bare DVC PDU.
    let mut dvc_reasm = rdp_channels::svc::Reassembler::new();
    // Microphone (audio-input) redirection state machine; only does anything
    // once the server opens the AUDIO_INPUT channel and a mic source is present.
    let mut audio_in = rdp_channels::audio_input::AudioInputChannel::new();
    let mut mic_started = false;
    // Camera redirection (MS-RDPECAM): enumerate local webcams and advertise
    // them. Empty (no webcam) → nothing announced, feature stays off.
    let cameras = crate::mf_camera::MfCamera::enumerate();
    tracing::info!(
        count = cameras.len(),
        names = ?cameras.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "local cameras available for redirection"
    );
    let mut camera = rdp_channels::camera::CameraEnumerator::new(cameras);
    // Per-device camera channels (keyed by DVC channel id) and the active
    // capture, started when the server begins a stream.
    let mut cam_devices: std::collections::HashMap<
        u32,
        rdp_channels::camera::CameraDeviceChannel,
    > = std::collections::HashMap::new();
    let mut cam_capture: Option<crate::mf_camera::MfCamera> = None;
    // Frames acked so far (the RDPGFX ack carries a running total). Acks are sent
    // here, on the network thread, the instant an EndFrame is parsed — BEFORE the
    // decode thread renders it — so the server's frame clock is no longer gated
    // by our decode time. Adaptive pacing now lives in the decode loop.
    let mut total_frames = 0u32;
    let mut last_ack = std::time::Instant::now();
    // Continuous network auto-detect (MS-RDPBCGR 2.2.14): we advertised
    // NETCHAR_AUTODETECT, so the server probes RTT/bandwidth during the session.
    // Answering keeps it on the fast-LAN profile. (Connect-time probes were
    // already answered during activation by `recv_demand_active`/`recv_until`.)
    let mut autodetect = AutoDetect::new();
    // Experimental UDP side-band transport (--udp). `udp` holds the tunnel once
    // the server requests multitransport and the dial succeeds; `udp_graphics`
    // is its own RDPGFX demuxer (the tunnel carries an independent DVC). All
    // best-effort: any failure leaves graphics flowing over TCP, untouched.
    let mut udp: Option<crate::udp::UdpTunnel> = None;
    let mut udp_graphics = GraphicsChannel::with_caps(gfx_caps);
    // Network back-pressure for the UDP graphics path: on a lossy link (host on
    // Wi-Fi) a fast client's decode backlog stays ~0, so the only thing that
    // slows the server is the RDPGFX frame-ack `queueDepth`. We add a bias to it
    // from observed loss / RTT inflation. `udp_net_prev` is the previous
    // cumulative (recv_total, recv_lost) snapshot, for per-window deltas.
    let mut congestion = crate::congestion::Congestion::new();
    let mut udp_net_prev: (u64, u64) = (0, 0);
    let mut udp_last_pressure: u32 = 0;
    // Holds partial inbound bytes between reads so a read timeout (used to wake
    // for input) never splits a PDU.
    let mut rx_buf: Vec<u8> = Vec::new();
    // Debounce window resizes: a drag fires WM_SIZE continuously, and each
    // Display Control request triggers a FULL server-side graphics reset
    // (surfaces torn down, everything re-encoded) — dozens per drag meant
    // seconds of artifact churn. Only forward once the size has been stable.
    let mut pending_resize: Option<(Vec<gcc::MonitorDef>, std::time::Instant)> = None;
    const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

    loop {
        // The UI thread sets this when the window closes. The reverse-connect /
        // WebSocket transport has no socket handle to `shutdown()` for an
        // unblock, and the read timeout means we never block anyway, so checking
        // an explicit flag each tick is what actually lets the worker exit (else
        // `worker.join()` hangs forever and the process outlives the window).
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }

        // Forward any client input the UI thread queued. The worker owns the one
        // SChannel context, so input must be sent from here — it can't be split
        // onto a second thread the way the legacy plaintext socket can.
        while let Ok(events) = input_rx.try_recv() {
            session.send_input(stream, &events)?;
        }

        // Push any local clipboard change to the server (we wake ~60x/sec).
        if take_clipboard_changed() {
            session.announce_clipboard(stream)?;
        }
        // ...and start staging the session's clipboard files if a paste is
        // waiting on them (the lazy half of file redirection).
        session.pump_clipboard_files(stream)?;

        // Forward a pending window-resize as a Display Control request, but only
        // after the size has been stable for RESIZE_SETTLE (drag-end), so one
        // drag costs one server-side graphics reset instead of dozens.
        if let Some(monitors) = take_resize_request() {
            pending_resize = Some((monitors, std::time::Instant::now()));
        }
        if let Some((monitors, since)) = pending_resize.take() {
            if since.elapsed() >= RESIZE_SETTLE {
                if let (Some(ch), Some(pdu)) =
                    (dvc_channel, graphics.request_resize(&monitors))
                {
                    session.send_dvc(stream, ch, &pdu)?;
                    let primary = monitors
                        .iter()
                        .find(|m| m.primary)
                        .or_else(|| monitors.first())
                        .map(|m| (m.right - m.left + 1, m.bottom - m.top + 1));
                    if let Some((w, h)) = primary {
                        tracing::info!(
                            monitors = monitors.len(),
                            width = w,
                            height = h,
                            "requested remote desktop resize (debounced)"
                        );
                    }
                }
            } else {
                pending_resize = Some((monitors, since));
            }
        }

        // Forward any queued multi-touch contacts over the RDPEI channel.
        let touches = take_touch_queue();
        if !touches.is_empty() {
            if let (Some(ch), Some(pdu)) = (dvc_channel, graphics.wrap_touch_event(&touches)) {
                session.send_dvc(stream, ch, &pdu)?;
                // First frame at info so a log shows whether touch input is
                // reaching the wire at all; the rest stay at trace.
                static TOUCH_FLOWING: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !TOUCH_FLOWING.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(contacts = touches.len(), "touch input active: first RDPEI touch frame sent");
                }
                tracing::trace!(contacts = touches.len(), "sent RDPEI touch frame");
            } else {
                static TOUCH_DROPPED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !TOUCH_DROPPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        contacts = touches.len(),
                        "touch input dropped: RDPEI channel not open/ready (server may not support touch, or the handshake failed)"
                    );
                }
            }
        }

        // Microphone: once the server has opened AUDIO_INPUT, start the capture
        // device (once) and forward any newly-captured PCM to the server.
        if let (Some(fmt), Some(ch)) = (audio_in.capture_format(), dvc_channel) {
            if let Some(m) = mic.as_deref_mut() {
                if !mic_started {
                    m.start(fmt.channels, fmt.samples_per_sec, fmt.bits_per_sample);
                    mic_started = true;
                    tracing::info!(
                        channels = fmt.channels,
                        rate = fmt.samples_per_sec,
                        "microphone capture started"
                    );
                }
                let pcm = m.poll();
                for pdu in audio_in.data_pdus(&pcm) {
                    if let Some(wrapped) = graphics.wrap_audio_input(&pdu) {
                        session.send_dvc(stream, ch, &wrapped)?;
                    }
                }
            }
        }

        // Camera: forward the latest captured frame to every streaming device
        // channel as a SampleResponse (wrapped DVC data rides the static
        // drdynvc channel).
        if let (Some(cap), Some(ch)) = (cam_capture.as_ref(), dvc_channel) {
            if let Some(frame) = cap.poll_frame() {
                for (chan_id, dev) in cam_devices.iter() {
                    if dev.streaming().is_some() {
                        let pdu = rdp_channels::camera::sample_response(0, &frame);
                        let wrapped = graphics.wrap_camera_device(*chan_id, &pdu);
                        session.send_dvc(stream, ch, &wrapped)?;
                    }
                }
            }
        }

        // If the UDP tunnel is up, drain any graphics it delivered: ack each
        // complete frame over the tunnel immediately, then hand the commands to
        // the decode thread (same renderer as the TCP path). A read timeout (no
        // UDP data) is normal — fall through to the TCP read.
        let mut tunnel_dead = false;
        if let Some(tunnel) = udp.as_mut() {
            let mut drained = 0u32;
            while drained < UDP_DRAIN_BUDGET {
                let payload = match tunnel.recv() {
                    Ok(p) => p,
                    // Idle tunnel (read timeout) → service TCP this pass.
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    // A real failure (EOF, TLS desync, socket error): the
                    // server may have soft-synced channels onto this tunnel,
                    // so a silently-dead one stalls them forever. Drop it
                    // loudly; graphics continue/fall back on TCP.
                    Err(e) => {
                        tracing::warn!(error = %e, "UDP tunnel failed; dropping side-band, staying on TCP");
                        tunnel_dead = true;
                        break;
                    }
                };
                drained += 1;
                if !payload.is_empty() {
                    let out = udp_graphics.process(&payload);
                    for resp in &out.responses {
                        let _ = tunnel.send(resp);
                    }
                    // Audio (MS-RDPEA) if the server carries AUDIO_PLAYBACK_DVC over
                    // the UDP tunnel: same RDPSND state machine + OS sink as the TCP
                    // path, acked back on the tunnel. Audio usually stays on TCP, but
                    // handling it here too means it works whichever transport the
                    // server soft-syncs the channel onto.
                    for msg in &out.audio_output {
                        for resp in session.process_audio_dvc(msg) {
                            if let Some(wrapped) = udp_graphics.wrap_audio_output(&resp) {
                                let _ = tunnel.send(&wrapped);
                            }
                        }
                    }
                    // No proactive formats send: like mstsc/FreeRDP, the client
                    // advertises its formats exactly once — in reply to the
                    // server's Server Audio Formats PDU (handled above) — never
                    // before it. A second Client Audio Formats PDU resets the
                    // server's rdpsnd state after Training and stalls the waves.
                    // Fold this window's transport stats into the congestion
                    // controller, then report `decode backlog + network bias` as
                    // the RDPGFX queueDepth so the server paces down under loss.
                    let s = tunnel.net_stats();
                    let recv_delta = s.recv_total.saturating_sub(udp_net_prev.0);
                    let lost_delta = s.recv_lost.saturating_sub(udp_net_prev.1);
                    udp_net_prev = (s.recv_total, s.recv_lost);
                    congestion.update(recv_delta, lost_delta, s.srtt);
                    if congestion.pressure() != udp_last_pressure {
                        udp_last_pressure = congestion.pressure();
                        tracing::info!(
                            target: "perf",
                            pressure = congestion.pressure(),
                            queue_bias = congestion.queue_depth_bias(),
                            loss_pct = congestion.loss_fraction() * 100.0,
                            srtt_us = s.srtt.map(|d| d.as_micros() as u64).unwrap_or(0),
                            jitter_us = s.jitter.as_micros() as u64,
                            "udp back-pressure changed"
                        );
                    }
                    let depth = backlog.load(std::sync::atomic::Ordering::Relaxed) as u32
                        + congestion.queue_depth_bias();
                    for command in &out.commands {
                        if let GfxCommand::EndFrame { frame_id } = command {
                            total_frames += 1;
                            if let Some(ack) = udp_graphics.frame_ack(*frame_id, total_frames, depth)
                            {
                                let _ = tunnel.send(&ack);
                            }
                        }
                    }
                    if !out.commands.is_empty() {
                        backlog.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if decode_tx.send(out.commands).is_err() {
                            return Ok(()); // decode thread gone → end session
                        }
                    }
                }
            }
        }
        if tunnel_dead {
            udp = None;
        }

        // Flush anything the hosted DVC redirector (Teams WebRTC add-in)
        // produced asynchronously on its own threads out to the server over the
        // static drdynvc channel. Empty (a no-op) unless a redirector is wired
        // in and has queued bytes.
        if let Some(ch) = dvc_channel {
            for pdu in graphics.poll_redirector() {
                session.send_dvc(stream, ch, &pdu)?;
            }
        }

        // Next server PDU. `None` = nothing buffered anywhere (`poll_tpkt_pdu`
        // reads until `WouldBlock`, and the TLS layer holds at most a partial
        // record then) — so it is safe to block until the socket has data, a
        // producer signals the worker, or the nearest timed chore is due.
        // Without an event wait (WebSocket paths), the socket's read timeout
        // paces the loop exactly as before. Partial bytes stay in `rx_buf`.
        let pdu = match poll_tpkt_pdu(stream, &mut rx_buf)? {
            Some(p) => p,
            None => {
                if let Some(w) = wait.as_ref() {
                    let idle = if mic_started || cam_capture.is_some() {
                        // Audio/camera capture: poll at the device cadence.
                        std::time::Duration::from_millis(10)
                    } else if udp.is_some() || redirector_active {
                        // Tunnel pump (retransmit RTO floor is 15 ms) and the
                        // redirector's async output queue.
                        std::time::Duration::from_millis(15)
                    } else if let Some((_, since)) = pending_resize.as_ref() {
                        RESIZE_SETTLE
                            .saturating_sub(since.elapsed())
                            .max(std::time::Duration::from_millis(1))
                    } else {
                        // Pure safety tick (background chores); every latency-
                        // sensitive source wakes the worker via its event.
                        std::time::Duration::from_millis(500)
                    };
                    w.wait(idle);
                }
                continue;
            }
        };
        let (channel, payload) = mcs::parse_send_data_indication(&pdu)?;
        let plaintext = unwrap_inbound(session.inbound.as_mut(), &payload);

        tracing::trace!(
            channel,
            len = plaintext.len(),
            is_io = channel == io_channel,
            is_dvc = Some(channel) == dvc_channel,
            io_pdu_type2 = ?finalization::data_pdu_type2(&plaintext),
            "graphics-loop rx"
        );

        if channel == io_channel {
            // Network auto-detect probe (MS-RDPBCGR 2.2.14)? Answer it before any
            // share-PDU handling — these carry a Basic Security Header (present on
            // the TLS path), so they are not Share PDUs. Best-effort: a send error
            // is logged, never fatal, and an unrecognised PDU falls through.
            match autodetect.classify(&plaintext, metrics.as_deref()) {
                AutoDetectOutcome::NotAutoDetect => {}
                AutoDetectOutcome::Consumed => continue,
                AutoDetectOutcome::Reply(resp) => {
                    if let Err(e) = send_payload(
                        stream,
                        session.info.user_channel_id,
                        session.info.io_channel_id,
                        &resp,
                    ) {
                        tracing::debug!(error = %e, "auto-detect response send failed");
                    }
                    continue;
                }
            }
            note_session_events(&plaintext);
            capture_reconnect_cookie(&plaintext, sink);
            // Watch for the server's Initiate Multitransport Request (MS-RDPEMT).
            // The parse is conservative (24..=64 bytes, valid protocol id) so it
            // can't disturb the normal share-PDU path; any failure leaves graphics
            // on TCP. For a direct host we dial the UDP side-band; for W365 there's
            // no direct address, so we log the request_id/cookie length — the input
            // to the Shortpath (TURN relay + ICE rendezvous) path being built.
            if udp.is_none() && (24..=64).contains(&plaintext.len()) {
                if let Some(req) = rdp_pdu::multitransport::InitiateRequest::parse(&plaintext) {
                    tracing::info!(
                        request_id = req.request_id,
                        lossy = req.is_lossy(),
                        cookie_len = req.security_cookie.len(),
                        has_dial = udp_dial.is_some(),
                        "Initiate Multitransport Request received"
                    );
                    // Whatever happens next, the server is now waiting on our
                    // Initiate Multitransport Response (2.2.15.2). Declining
                    // promptly matters: with no response the server waits out
                    // its own multitransport timeout before settling the
                    // session onto TCP at full rate. (An `S_OK` on success is
                    // only defined once Soft-Sync is negotiated — the in-band
                    // RDPEMT tunnel create is what signals success here.)
                    let mut decline = |reason: &str| {
                        tracing::info!(reason, "declining multitransport request");
                        let resp = rdp_pdu::multitransport::response(
                            req.request_id,
                            rdp_pdu::multitransport::HR_E_ABORT,
                        );
                        if let Err(e) = send_payload(
                            stream,
                            session.info.user_channel_id,
                            session.info.io_channel_id,
                            &resp,
                        ) {
                            tracing::debug!(error = %e, "multitransport response send failed");
                        }
                    };
                    // The lossy (FEC) channel is declined: its sender never
                    // retransmits, so a single dropped datagram would leave a
                    // permanent hole in the TLS byte stream the tunnel runs
                    // over. Until DTLS + FEC recovery exist, only the reliable
                    // channel (whose holes retransmission repairs) is sound.
                    if req.is_lossy() {
                        decline("lossy FEC channel not supported (no DTLS/FEC recovery)");
                        continue;
                    }
                    if let Some(dial) = udp_dial.as_ref() {
                        match crate::udp::UdpTunnel::connect(
                            &dial.server,
                            &dial.hostname,
                            dial.accept_invalid_cert,
                            req.request_id,
                            &req.security_cookie,
                            req.is_lossy(),
                            dial.debug,
                        ) {
                            Ok(tunnel) => {
                                tracing::info!("UDP multitransport tunnel established");
                                udp = Some(tunnel);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "UDP side-band failed; staying on TCP");
                                decline("UDP dial failed");
                            }
                        }
                    } else {
                        tracing::info!(
                            "no direct UDP address (W365 Reverse Connect) — this request_id/cookie \
                             feeds the Shortpath TURN/rendezvous tunnel (next step)"
                        );
                        decline("no direct UDP address");
                    }
                    continue;
                }
            }
            if capabilities::is_deactivate_all(&plaintext) {
                tracing::info!("server Deactivate All; reactivating");
                session.reactivate(stream)?;
                continue;
            }
            if paint_bitmap_update(&plaintext, sink) {
                sink.present();
            }
            session.handle_pointer(&plaintext, sink);
            continue;
        }
        if Some(channel) == session.info.channel_id(rdp_pdu::gcc::CLIPRDR_CHANNEL) {
            session.handle_clipboard(stream, &plaintext)?;
            continue;
        }
        if Some(channel) == session.info.channel_id(rdp_pdu::gcc::RDPSND_CHANNEL) {
            session.handle_audio(stream, &plaintext)?;
            continue;
        }
        if Some(channel) == session.info.channel_id(rdp_pdu::gcc::RDPDR_CHANNEL) {
            session.handle_rdpdr(stream, &plaintext)?;
            continue;
        }
        if Some(channel) != dvc_channel {
            tracing::trace!(channel, len = plaintext.len(), "data on unhandled channel");
            continue;
        }

        // RDPGFX dynamic channel. Inbound DVC data on the static drdynvc channel
        // is wrapped in a CHANNEL_PDU_HEADER (length+flags) and may span several
        // static-VC chunks — strip/reassemble that first (as the clipboard path
        // does) before handing the bare DVC PDU to the graphics demuxer. Passing
        // the raw header bytes to drdynvc::parse mis-reads it (channel_id 0 /
        // empty name), so no dynamic channel ever opens.
        let Some(dvc_msg) = dvc_reasm.push(&plaintext) else {
            continue;
        };
        let out = graphics.process(&dvc_msg);
        for resp in &out.responses {
            session.send_dvc(stream, channel, resp)?;
        }
        // Drive the microphone (MS-RDPEAI) state machine with any messages that
        // arrived on the AUDIO_INPUT channel, sending its replies back.
        for msg in &out.audio_input {
            for resp in audio_in.process(msg) {
                if let Some(wrapped) = graphics.wrap_audio_input(&resp) {
                    session.send_dvc(stream, channel, &wrapped)?;
                }
            }
        }
        // Drive the speaker (MS-RDPEA / RDPSND) state machine with messages from
        // the AUDIO_PLAYBACK_DVC channel — formats, training and wave data — which
        // is where modern Windows streams session audio. Play them through the OS
        // sink (shared with the static path) and ack back on the dynamic channel.
        for msg in &out.audio_output {
            for resp in session.process_audio_dvc(msg) {
                if let Some(wrapped) = graphics.wrap_audio_output(&resp) {
                    session.send_dvc(stream, channel, &wrapped)?;
                }
            }
        }
        // Audio negotiation is server-initiated, exactly as mstsc/FreeRDP do it:
        // the client advertises its formats only in reply to the server's Server
        // Audio Formats PDU (handled in the `out.audio_output` loop above), never
        // proactively when the channel opens. Sending client formats a second
        // time resets a Windows host's rdpsnd state after it has sent Training,
        // which stalls the wave stream — the silent-audio bug this fixes.
        if out.audio_output_opened {
            tracing::info!("AUDIO_PLAYBACK_DVC open; awaiting server formats");
        }
        // Drive the camera (MS-RDPECAM) enumerator likewise.
        for msg in &out.camera {
            for resp in camera.process(msg) {
                if let Some(wrapped) = graphics.wrap_camera(&resp) {
                    session.send_dvc(stream, channel, &wrapped)?;
                }
            }
        }
        // Per-device camera channels: negotiate streams/media types and start
        // capture when the server begins a stream.
        for (chan_id, msg) in &out.camera_device {
            let dev = cam_devices.entry(*chan_id).or_insert_with(|| {
                rdp_channels::camera::CameraDeviceChannel::new(
                    crate::mf_camera::MfCamera::media_types(),
                )
            });
            for resp in dev.process(msg) {
                let wrapped = graphics.wrap_camera_device(*chan_id, &resp);
                session.send_dvc(stream, channel, &wrapped)?;
            }
            // The server just started streaming → open the capture device (the
            // single default webcam; multi-camera selection is a future step).
            if cam_capture.is_none() {
                if let Some(media) = dev.streaming() {
                    cam_capture = Some(crate::mf_camera::MfCamera::start(0, media));
                    tracing::info!(
                        channel_id = *chan_id,
                        ?media,
                        "camera capture started; streaming frames to the session"
                    );
                }
            }
        }
        if !out.commands.is_empty() {
            tracing::trace!(commands = out.commands.len(), "EGFX commands decoded");
        }

        // Ack each complete frame IMMEDIATELY (before decode), reporting the
        // decode thread's backlog as the RDPGFX queueDepth so the server paces
        // itself to our decode rate. Then hand the owned command batch to the
        // decode thread — a move, no copy of the tile payloads.
        let depth = backlog.load(std::sync::atomic::Ordering::Relaxed) as u32;
        for command in &out.commands {
            if let GfxCommand::EndFrame { frame_id } = command {
                total_frames += 1;
                // `ack_gap_us` is the server-visible frame clock: time since the
                // previous frame-ack. If it tracks the network inter-frame
                // interval (~16-33ms) the server is streaming at full rate; if it
                // tracks decode time the client is the throttle.
                let now = std::time::Instant::now();
                tracing::debug!(
                    target: "perf",
                    frame_id,
                    queue_depth = depth,
                    ack_gap_us = now.duration_since(last_ack).as_micros() as u64,
                    "frame ack"
                );
                last_ack = now;
                if let Some(ack) = graphics.frame_ack(*frame_id, total_frames, depth) {
                    session.send_dvc(stream, channel, &ack)?;
                }
            }
        }
        if !out.commands.is_empty() {
            backlog.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if decode_tx.send(out.commands).is_err() {
                return Ok(()); // decode thread gone → end session
            }
        }
    }
}

/// Send a raw payload inside an MCS Send Data Request (no extra security wrap).
fn send_payload<S: Write>(
    stream: &mut S,
    user_id: u16,
    channel_id: u16,
    payload: &[u8],
) -> Result<(), ActivateError> {
    let request = mcs::send_data_request(user_id, channel_id, payload);
    stream.write_all(&mcs::frame(&request)?)?;
    stream.flush()?;
    Ok(())
}

/// Send a share PDU, RC4/MAC-wrapping it when a security session is active.
fn send_share<S: Write>(
    stream: &mut S,
    user_id: u16,
    channel_id: u16,
    sec: &mut Option<SecuritySession>,
    share_pdu: &[u8],
) -> Result<(), ActivateError> {
    let payload = match sec.as_mut() {
        Some(s) => s.wrap(0, share_pdu),
        None => share_pdu.to_vec(),
    };
    send_payload(stream, user_id, channel_id, &payload)
}

/// Network auto-detect responder state (MS-RDPBCGR 2.2.14): the in-flight
/// bandwidth meter and its clock. One instance serves the connect-time probes
/// (interleaved with licensing/activation) and another the continuous probes in
/// the steady-state graphics loop — the wire handling is identical.
struct AutoDetect {
    meter: rdp_pdu::autodetect::BandwidthMeter,
    bw_start: std::time::Instant,
}

/// What an inbound I/O-channel PDU turned out to be, for [`AutoDetect`].
enum AutoDetectOutcome {
    /// Not an auto-detect request — process the PDU normally.
    NotAutoDetect,
    /// An auto-detect request that needs no reply (start / payload / verdict).
    Consumed,
    /// An auto-detect request whose reply must go out on the I/O channel —
    /// immediately, because the server times RTT from it.
    Reply(Vec<u8>),
}

impl AutoDetect {
    fn new() -> Self {
        Self {
            meter: rdp_pdu::autodetect::BandwidthMeter::default(),
            bw_start: std::time::Instant::now(),
        }
    }

    /// Classify `plaintext` (which must still carry its Basic Security Header,
    /// i.e. the TLS path — legacy RC4 strips it during decryption) and update
    /// the bandwidth meter.
    fn classify(
        &mut self,
        plaintext: &[u8],
        metrics: Option<&crate::metrics::Metrics>,
    ) -> AutoDetectOutcome {
        use rdp_pdu::autodetect::AutoDetectRequest as Ad;
        let Some(req) = rdp_pdu::autodetect::parse_request(plaintext) else {
            return AutoDetectOutcome::NotAutoDetect;
        };
        tracing::debug!(target: "perf", ?req, "auto-detect request");
        match req {
            Ad::RttMeasure { sequence } => {
                // Reply instantly — the server times this round trip as the RTT.
                AutoDetectOutcome::Reply(rdp_pdu::autodetect::rtt_response(sequence))
            }
            Ad::BandwidthStart { .. } => {
                self.meter.start();
                self.bw_start = std::time::Instant::now();
                AutoDetectOutcome::Consumed
            }
            Ad::BandwidthPayload { payload_len, .. } => {
                self.meter.add(payload_len);
                AutoDetectOutcome::Consumed
            }
            Ad::BandwidthStop { sequence, payload_len, connect_time } => {
                match self.meter.stop(payload_len) {
                    Some(byte_count) => {
                        let delta =
                            self.bw_start.elapsed().as_millis().min(u32::MAX as u128) as u32;
                        AutoDetectOutcome::Reply(rdp_pdu::autodetect::bandwidth_results(
                            sequence,
                            connect_time,
                            delta,
                            byte_count,
                        ))
                    }
                    None => AutoDetectOutcome::Consumed, // stray stop — ignore
                }
            }
            Ad::NetCharResult { average_rtt_us, .. } => {
                if let (Some(rtt), Some(m)) = (average_rtt_us, metrics) {
                    m.record_rtt_us(rtt as u64);
                }
                AutoDetectOutcome::Consumed // server's verdict; no reply
            }
        }
    }
}

/// Read Send Data Indications (decrypting when a session is active) until `pick`
/// returns a value, skipping the rest. Bounded to avoid spinning forever.
///
/// Connect-time network auto-detect probes (MS-RDPBCGR 2.2.14.1) arrive in this
/// window because we advertised `RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT`; they are
/// answered here (TLS path only). Dropping them instead would leave the server's
/// link characterization to time out into its worst-case network profile — the
/// session then runs throttled no matter how fast the link is. Auto-detect PDUs
/// do not count against the wait budget: a bandwidth-measure train is legally
/// longer than any fixed PDU budget, so only non-probe PDUs decrement it.
fn recv_until<S, T>(
    stream: &mut S,
    sec: &mut Option<SecuritySession>,
    user_id: u16,
    io_channel_id: u16,
    what: &str,
    mut pick: impl FnMut(&[u8]) -> Option<T>,
) -> Result<T, ActivateError>
where
    S: Read + Write,
{
    let mut autodetect = AutoDetect::new();
    let mut budget = 64u32;
    // The absolute iteration cap only guards against a hostile endless probe
    // train; a real bandwidth train is a few hundred PDUs at most.
    for _ in 0..4096 {
        if budget == 0 {
            break;
        }
        let pdu = read_tpkt_pdu(stream)?;
        let (channel, payload) = mcs::parse_send_data_indication(&pdu)?;
        let plaintext = match sec.as_mut() {
            Some(s) => s.unwrap(&payload),
            None => payload,
        };
        if sec.is_none() {
            match autodetect.classify(&plaintext, None) {
                AutoDetectOutcome::NotAutoDetect => {}
                AutoDetectOutcome::Consumed => continue,
                AutoDetectOutcome::Reply(resp) => {
                    send_payload(stream, user_id, io_channel_id, &resp)?;
                    continue;
                }
            }
        }
        budget -= 1;
        // Surface a server Set Error Info (the reason for an activation-time
        // teardown) instead of silently skipping it while hunting for `what`.
        note_error_info(&plaintext);
        tracing::debug!(
            channel,
            len = plaintext.len(),
            pdu_type2 = ?finalization::data_pdu_type2(&plaintext),
            waiting_for = what,
            "recv during finalization"
        );
        if let Some(value) = pick(&plaintext) {
            return Ok(value);
        }
    }
    Err(proto_err(format!("did not receive {what} within 64 PDUs")))
}

/// Wait for the Demand Active PDU, handling the licensing phase that precedes
/// it. The common case is a `SERVER_LICENSE_ERROR_PDU` with `STATUS_VALID_CLIENT`
/// ("no CAL required"), after which the server sends Demand Active; granted /
/// upgraded licenses are likewise treated as complete. A server that demands
/// full CAL issuance (`LICENSE_REQUEST` / `PLATFORM_CHALLENGE`) fails fast with a
/// precise message rather than the previous 64-PDU timeout, because the client
/// does not perform the licensing key exchange.
fn recv_demand_active<S: Read + Write>(
    stream: &mut S,
    sec: &mut Option<SecuritySession>,
    user_id: u16,
    io_channel_id: u16,
) -> Result<u32, ActivateError> {
    use rdp_pdu::license::{parse_license_message, LicenseMessage};
    let mut autodetect = AutoDetect::new();
    let mut budget = 64u32;
    for _ in 0..4096 {
        if budget == 0 {
            break;
        }
        let pdu = read_tpkt_pdu(stream)?;
        let (_channel, payload) = mcs::parse_send_data_indication(&pdu)?;
        let plaintext = match sec.as_mut() {
            Some(s) => s.unwrap(&payload),
            None => payload,
        };

        // Connect-time auto-detect probes arrive interleaved with licensing;
        // answer them (see `recv_until`) and don't count them against the
        // Demand Active wait budget.
        if sec.is_none() {
            match autodetect.classify(&plaintext, None) {
                AutoDetectOutcome::NotAutoDetect => {}
                AutoDetectOutcome::Consumed => continue,
                AutoDetectOutcome::Reply(resp) => {
                    send_payload(stream, user_id, io_channel_id, &resp)?;
                    continue;
                }
            }
        }
        budget -= 1;

        // A server Set Error Info here explains an activation-time refusal
        // (denied connection, insufficient privileges, license failure, …).
        note_session_events(&plaintext);

        // AVD / RDS broker redirection arrives instead of Demand Active.
        if let Some(redir) = rdp_pdu::redirection::parse(&plaintext) {
            return Err(ActivateError::Redirect(redir));
        }

        // Demand Active ends the wait (and the licensing phase).
        if let Ok(share_id) = capabilities::parse_demand_active(&plaintext) {
            note_server_input_flags(&plaintext);
            return Ok(share_id);
        }

        // Otherwise classify any licensing PDU and react.
        if let Some(msg) = parse_license_message(&plaintext) {
            if msg.is_complete() {
                tracing::info!(?msg, "licensing satisfied; awaiting Demand Active");
                continue;
            }
            if msg.demands_cal_issuance() {
                return Err(proto_err(
                    "server requires RDS CAL licensing (per-device/per-user license issuance), \
                     which rdpio does not implement — connect to a host that returns \
                     STATUS_VALID_CLIENT (e.g. an admin/console session, or a host where \
                     per-user CAL enforcement is not in effect)",
                ));
            }
            if let LicenseMessage::ErrorAlert {
                error_code,
                state_transition,
            } = msg
            {
                return Err(proto_err(format!(
                    "server rejected licensing: error 0x{error_code:08X}, state 0x{state_transition:08X}"
                )));
            }
            tracing::debug!(?msg, "ignoring unmodelled licensing message");
        }
    }
    Err(proto_err("did not receive Demand Active within 64 PDUs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn sdi(channel: u16, payload: &[u8]) -> Vec<u8> {
        let ch = channel.to_be_bytes();
        let mut mcs = vec![0x68, 0x00, 0x00, ch[0], ch[1], 0x70, payload.len() as u8];
        mcs.extend_from_slice(payload);
        mcs::frame(&mcs).unwrap()
    }

    fn connect_response() -> Vec<u8> {
        let mut user = b"McDn".to_vec();
        user.push(0x0a);
        user.extend_from_slice(&[0x03, 0x0c, 0x0a, 0x00, 0xeb, 0x03, 0x01, 0x00, 0xec, 0x03]);
        let mut content = vec![0x0a, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x00];
        content.push(0x04);
        content.push(user.len() as u8);
        content.extend_from_slice(&user);
        let mut cr = vec![0x7f, 0x66, content.len() as u8];
        cr.extend_from_slice(&content);
        mcs::frame(&cr).unwrap()
    }

    fn demand_active(share_id: u32) -> Vec<u8> {
        let s = share_id.to_le_bytes();
        sdi(
            1003,
            &[0x00, 0x00, 0x11, 0x00, 0xea, 0x03, s[0], s[1], s[2], s[3]],
        )
    }

    fn font_map() -> Vec<u8> {
        let payload = [
            0x00, 0x00, 0x17, 0x00, 0xea, 0x03, 0xea, 0x03, 0x01, 0x00, 0x00, 0x01, 0x12, 0x00, 40,
            0x00, 0x00, 0x00,
        ];
        sdi(1003, &payload)
    }

    fn join_confirm(channel: u16) -> Vec<u8> {
        let c = channel.to_be_bytes();
        mcs::frame(&[0x3e, 0x00, 0x00, 0x06, c[0], c[1], c[0], c[1]]).unwrap()
    }

    #[test]
    fn activate_runs_to_font_map_plaintext() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let share_id = 0x0001_03EA;
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_tpkt_pdu(&mut s).unwrap();
            s.write_all(&connect_response()).unwrap();
            read_tpkt_pdu(&mut s).unwrap();
            read_tpkt_pdu(&mut s).unwrap();
            s.write_all(&mcs::frame(&[0x2e, 0x00, 0x00, 0x06]).unwrap())
                .unwrap();
            for ch in [1007u16, 1003, 1004] {
                read_tpkt_pdu(&mut s).unwrap();
                s.write_all(&join_confirm(ch)).unwrap();
            }
            read_tpkt_pdu(&mut s).unwrap(); // client info
            s.write_all(&sdi(1003, &[0x80, 0x00, 0x00, 0x00, 0x01, 0x02]))
                .unwrap();
            s.write_all(&demand_active(share_id)).unwrap();
            for _ in 0..5 {
                read_tpkt_pdu(&mut s).unwrap();
            }
            s.write_all(&font_map()).unwrap();
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        let config = ClientConfig {
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let session = activate(&mut stream, &config, SecurityProtocol::HYBRID, None).unwrap();
        assert_eq!(session.info().user_channel_id, 1007);
        assert_eq!(session.info().io_channel_id, 1003);
        assert_eq!(session.info().share_id, share_id);
        assert!(!session.info().encrypted);
        server.join().unwrap();
    }

    #[derive(Default)]
    struct RecordingSink {
        rects: Vec<(u16, u16, u16, u16, usize)>,
        presents: usize,
    }
    impl FrameSink for RecordingSink {
        fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rgba: &[u8]) {
            self.rects.push((x, y, w, h, rgba.len()));
        }
        fn present(&mut self) {
            self.presents += 1;
        }
    }

    #[test]
    fn run_session_paints_a_bitmap_update() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Update body: BITMAP update, one 1x1 @32bpp uncompressed rect at (5,10).
            let mut body = vec![0x01, 0x00, 0x01, 0x00];
            body.extend_from_slice(&[
                5, 0, 10, 0, 6, 0, 11, 0, // dest rect
                1, 0, 1, 0, // width, height
                0x20, 0, // 32 bpp
                0, 0, // flags (uncompressed)
                4, 0, // bitmapLength
            ]);
            body.extend_from_slice(&[0x10, 0x20, 0x30, 0xFF]); // BGRA
                                                               // Wrap in a Share Data Header with pduType2 = UPDATE.
            let total = (18 + body.len()) as u16;
            let mut share = Vec::new();
            share.extend_from_slice(&total.to_le_bytes());
            share.extend_from_slice(&0x17u16.to_le_bytes()); // DATAPDU | version
            share.extend_from_slice(&1002u16.to_le_bytes()); // pduSource
            share.extend_from_slice(&0x0001_03EAu32.to_le_bytes()); // shareId
            share.push(0); // pad1
            share.push(1); // streamId
            share.extend_from_slice(&total.to_le_bytes()); // uncompressedLength
            share.push(2); // pduType2 = UPDATE
            share.push(0); // compressedType
            share.extend_from_slice(&0u16.to_le_bytes()); // compressedLength
            share.extend_from_slice(&body);
            s.write_all(&sdi(1003, &share)).unwrap();
            // Drop the socket → client's next read returns EOF and the loop ends.
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        let mut session = ActiveSession {
            info: SessionInfo {
                user_channel_id: 1007,
                io_channel_id: 1003,
                share_id: 0x0001_03EA,
                channel_ids: vec![],
                channel_names: vec![],
                encrypted: false,
            },
            inbound: None,
            outbound: None,
            cursor_cache: Default::default(),
            clipboard: Default::default(),
            audio: Default::default(),
            rdpdr: Default::default(),
            width: 1024,
            height: 768,
            keyboard_layout: 0x0409,
            enable_rfx: false,
        };
        let mut sink = RecordingSink::default();
        let _ = run_session(&mut stream, &mut session, &mut sink); // ends with EOF

        assert_eq!(sink.rects, vec![(5, 10, 1, 1, 4)]);
        assert_eq!(sink.presents, 1);
        server.join().unwrap();
    }

    #[test]
    fn input_sender_frames_a_tpkt_send_data_request() {
        let mut session = ActiveSession {
            info: SessionInfo {
                user_channel_id: 1007,
                io_channel_id: 1003,
                share_id: 0x0001_03EA,
                channel_ids: vec![],
                channel_names: vec![],
                encrypted: false,
            },
            inbound: None,
            outbound: None,
            cursor_cache: Default::default(),
            clipboard: Default::default(),
            audio: Default::default(),
            rdpdr: Default::default(),
            width: 1024,
            height: 768,
            keyboard_layout: 0x0409,
            enable_rfx: false,
        };
        let events = [rdp_pdu::input::mouse_event(
            rdp_pdu::input::PTRFLAGS_MOVE,
            100,
            50,
        )];
        let mut buf: Vec<u8> = Vec::new();
        session.take_input_sender(&mut buf).send(&events).unwrap();

        // TPKT header (version 3) with a length that matches the buffer.
        assert_eq!(buf[0], 0x03);
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]) as usize, buf.len());
        // Plaintext path: the exact Input Event PDU sits at the tail.
        let expected = rdp_pdu::input::input_pdu(0x0001_03EA, 1007, &events);
        assert!(buf.ends_with(&expected));
    }

    #[test]
    fn input_sender_send_empty_is_noop() {
        let mut session = ActiveSession {
            info: SessionInfo {
                user_channel_id: 1007,
                io_channel_id: 1003,
                share_id: 1,
                channel_ids: vec![],
                channel_names: vec![],
                encrypted: false,
            },
            inbound: None,
            outbound: None,
            cursor_cache: Default::default(),
            clipboard: Default::default(),
            audio: Default::default(),
            rdpdr: Default::default(),
            width: 1024,
            height: 768,
            keyboard_layout: 0x0409,
            enable_rfx: false,
        };
        let mut buf: Vec<u8> = Vec::new();
        session.take_input_sender(&mut buf).send(&[]).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn security_session_wrap_unwrap_roundtrip() {
        use rdp_crypto::keys::SessionKeys;
        let shared = vec![0x11u8; 16];
        let mac = vec![0x22u8; 16];
        // Sender encrypts with `shared`; receiver decrypts with the same key.
        let mut sender = SecuritySession::new(
            SessionKeys {
                mac_key: mac.clone(),
                client_encrypt_key: shared.clone(),
                server_decrypt_key: vec![0x99; 16],
            },
            rdp_crypto::keys::METHOD_128BIT,
        );
        let mut receiver = SecuritySession::new(
            SessionKeys {
                mac_key: mac,
                client_encrypt_key: vec![0x99; 16],
                server_decrypt_key: shared,
            },
            rdp_crypto::keys::METHOD_128BIT,
        );

        let wrapped = sender.wrap(security::SEC_INFO_PKT, b"top secret share data");
        // Encrypted + SEC_INFO_PKT flag present in the header.
        let flags = u16::from_le_bytes([wrapped[0], wrapped[1]]);
        assert_eq!(flags, security::SEC_ENCRYPT | security::SEC_INFO_PKT);
        assert_eq!(receiver.unwrap(&wrapped), b"top secret share data");
    }

    #[test]
    fn recv_demand_active_detects_server_redirection() {
        use rdp_pdu::redirection::{SEC_REDIRECTION_PKT, REDIRECT_FLAG_LOAD_BALANCE_INFO};

        // Build a minimal Server Redirection PDU.
        let cookie = b"Cookie: msts=broker-token\r\n";
        let mut fields = Vec::new();
        fields.extend_from_slice(&(cookie.len() as u32).to_le_bytes());
        fields.extend_from_slice(cookie);

        let mut packet = Vec::new();
        packet.extend_from_slice(&SEC_REDIRECTION_PKT.to_le_bytes());
        packet.extend_from_slice(&((12 + fields.len()) as u16).to_le_bytes());
        packet.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // sessionID
        packet.extend_from_slice(&REDIRECT_FLAG_LOAD_BALANCE_INFO.to_le_bytes());
        packet.extend_from_slice(&fields);

        let total = (6 + packet.len()) as u16;
        let mut share = Vec::new();
        share.extend_from_slice(&total.to_le_bytes());
        share.extend_from_slice(&0x000Au16.to_le_bytes()); // PDUTYPE_SERVER_REDIR_PKT
        share.extend_from_slice(&[0u8; 2]); // pad2Octets
        share.extend_from_slice(&packet);

        let mut stream = std::io::Cursor::new(sdi(1003, &share));
        let err = recv_demand_active(&mut stream, &mut None, 1002, 1003).unwrap_err();
        match err {
            ActivateError::Redirect(r) => {
                assert_eq!(r.session_id, 0xDEAD_BEEF);
                assert_eq!(r.load_balance_info, cookie);
            }
            other => panic!("expected Redirect, got {other:?}"),
        }
    }

    /// A read side fed from a script and a write side that records what the
    /// client sent — for exercising activation paths that must *reply* (the
    /// connect-time auto-detect probes).
    struct Duplex {
        rx: std::io::Cursor<Vec<u8>>,
        tx: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Connect-time RTT probes arrive before Demand Active; the client must
    /// answer them (or the server characterizes the link as worst-case and
    /// throttles) and they must not consume the Demand Active wait budget.
    #[test]
    fn recv_demand_active_answers_connect_time_rtt_probe() {
        // RTT Measure Request 0x1001, sequence 7, wrapped in a Basic Security
        // Header carrying SEC_AUTODETECT_REQ.
        let mut probe = Vec::new();
        probe.extend_from_slice(&security::SEC_AUTODETECT_REQ.to_le_bytes());
        probe.extend_from_slice(&0u16.to_le_bytes()); // flagsHi
        probe.push(0x06); // headerLength
        probe.push(0x00); // headerTypeId = request
        probe.extend_from_slice(&7u16.to_le_bytes()); // sequenceNumber
        probe.extend_from_slice(&0x1001u16.to_le_bytes()); // RTT_REQUEST_CONNECTTIME

        let mut script = sdi(1003, &probe);
        // Then a Demand Active (shareId 0x11223344) so the call succeeds.
        let s = 0x1122_3344u32.to_le_bytes();
        script.extend_from_slice(&sdi(
            1003,
            &[0x00, 0x00, 0x11, 0x00, 0xea, 0x03, s[0], s[1], s[2], s[3]],
        ));

        let mut stream = Duplex { rx: std::io::Cursor::new(script), tx: Vec::new() };
        let share_id = recv_demand_active(&mut stream, &mut None, 1002, 1003).unwrap();
        assert_eq!(share_id, 0x1122_3344);

        // The reply is a TPKT-framed MCS Send Data Request whose payload is an
        // RTT Measure Response: SEC_AUTODETECT_RSP header, then the 6-byte
        // detection header echoing sequence 7 with responseType 0x0000.
        let expected = rdp_pdu::autodetect::rtt_response(7);
        assert!(
            stream
                .tx
                .windows(expected.len())
                .any(|w| w == expected.as_slice()),
            "activation did not send the RTT response: {:02x?}",
            stream.tx
        );
    }
}

#[cfg(test)]
mod crypto_tests {
    use super::*;

    /// Regression test for the legacy black-screen disconnect: the session
    /// worker and the [`InputSender`] share ONE outbound RC4 keystream. Before
    /// the fix, `take_input_sender` moved the cipher away and the worker's
    /// channel replies (cliprdr/rdpdr) went out unencrypted — an
    /// encryption-required server answers that with a hard TCP reset right
    /// after activation. Two handles to the shared cipher must produce one
    /// continuous keystream that a single server-side cipher can decrypt in
    /// wire order, with valid MACs.
    #[test]
    fn shared_outbound_keystream_is_continuous_across_handles() {
        let key = vec![0x11u8; 16];
        let mac_key = vec![0x22u8; 16];
        let method = 2; // 128-bit RC4
        let shared: SharedOutbound = std::sync::Arc::new(std::sync::Mutex::new(OutboundCrypto {
            encrypt: rdp_crypto::SessionCipher::new(key.clone(), method),
            mac_key: mac_key.clone(),
        }));
        let worker = shared.clone(); // stays with the session worker
        let input = shared; // handed to the InputSender

        let a = b"worker: cliprdr reply".to_vec();
        let b = b"input: mouse move".to_vec();
        let wire1 = lock_outbound(&worker).wrap(0, &a);
        let wire2 = lock_outbound(&input).wrap(0, &b);

        // One server-side decryptor over the wire order must recover both
        // plaintexts — proving the two handles didn't fork the keystream.
        let mut decrypt = rdp_crypto::SessionCipher::new(key, method);
        for (wire, plain) in [(wire1, a), (wire2, b)] {
            // Layout: 4-byte security header, 8-byte MAC, RC4 body.
            let mut body = wire[12..].to_vec();
            decrypt.apply_packet(&mut body);
            assert_eq!(body, plain);
            let mac = rdp_crypto::keys::mac_signature(&mac_key, &body);
            assert_eq!(&wire[4..12], &mac[..]);
        }
    }
}
