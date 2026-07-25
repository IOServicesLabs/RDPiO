//! Host Microsoft's Teams "Optimized" WebRTC media redirector,
//! `MsRdcWebRTCAddIn.dll`, so Teams (and other WebRTC apps) offload their audio/
//! video to run peer-to-peer from *this* client instead of being encoded into the
//! RDP graphics stream — the "Optimized" state the Windows App reaches.
//!
//! ## Why hosting works
//! RE of the shipped DLL (see memory `rdpio-teams-optimized-webrtc`) shows it is a
//! standard RDP **dynamic virtual channel plugin**: it exports the single
//! `VirtualChannelGetInstance` entry (the `IWTSPlugin` ABI from
//! `tsvirtualchannels.h`), it bundles a full WebRTC engine (`webrtc_voice_engine`
//! / `webrtc_video_engine` internals, MFPlat/d3d11/Secur32/WS2_32 imports), and it
//! depends only on stock Windows + the VC++ runtime. So rdpio can `LoadLibrary` it
//! and act as the plugin *host* exactly as msrdc/mstsc do:
//!
//! 1. `VirtualChannelGetInstance(IID_IWTSPlugin)` → `IWTSPlugin`.
//! 2. We implement [`IWTSVirtualChannelManager`] and pass it to `plugin.Initialize`.
//!    During init the plugin calls `CreateListener("com.microsoft.rdc.dvc.webrtc.1")`.
//! 3. When the Cloud PC opens that DVC, we hand the plugin our
//!    [`IWTSVirtualChannel`] (whose `Write` queues bytes back to the server) via
//!    the listener's `OnNewChannelConnection`, and route inbound DVC data into the
//!    returned `IWTSVirtualChannelCallback::OnDataReceived`.
//!
//! ## Threading
//! All COM lives on a dedicated MTA "webrtc-addin" thread that owns the plugin and
//! its callbacks (thread-affine COM pointers never leave it). The mux talks to it
//! through a `Send` [`WebRtcRedirector`] handle: control messages go over an
//! `mpsc` channel; the plugin's outbound `Write`s land in a shared queue the mux
//! drains via `GraphicsChannel::poll_redirector`. The add-in's own media threads
//! only ever touch the thread-safe outbound queue.
//!
//! ## Status
//! Scaffold: loads + initializes the add-in and bridges the channel end to end.
//! The exact handshake the add-in expects on the DVC (and whether it needs the
//! window-scrape / cursor-inject side channels its strings hint at) is to be
//! confirmed on a live Teams call — hence the heavy secret-safe logging.

#![cfg(windows)]
// COM interface methods (GetService, …) are PascalCase by convention.
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::fs::File;
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rdp_graphics::redirect::DvcRedirector;

use windows::core::{
    implement, interface, s, IUnknown, IUnknown_Vtbl, Interface, Ref, Result, BOOL, BSTR, GUID,
    HRESULT, PCWSTR,
};
use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE, E_NOTIMPL, FreeLibrary, HMODULE, S_OK};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
};
use windows::Win32::System::RemoteDesktop::{
    IWTSListener, IWTSListenerCallback, IWTSListener_Impl, IWTSPlugin, IWTSVirtualChannel,
    IWTSVirtualChannelCallback, IWTSVirtualChannelManager, IWTSVirtualChannelManager_Impl,
    IWTSVirtualChannel_Impl,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
};

/// `HRESULT VirtualChannelGetInstance(REFIID, ULONG* pNumObjs, VOID** ppObjArray)`.
type GetInstanceFn =
    unsafe extern "system" fn(*const GUID, *mut u32, *mut *mut c_void) -> HRESULT;

/// Control messages from the mux (any thread) to the COM host thread.
enum HostMsg {
    /// The server opened a DVC one of the plugin's listeners claimed.
    NewChannel { channel_id: u32, name: String },
    /// A complete reassembled message arrived on an accepted channel.
    Data { channel_id: u32, data: Vec<u8> },
    /// The server closed an accepted channel.
    Close { channel_id: u32 },
    /// Tear the plugin down (on handle drop).
    Shutdown,
}

/// msrdc-private "service provider" interface the add-in QueryInterfaces our
/// `IWTSVirtualChannelManager` for (reversed: `Initialize` stores our manager as
/// the plugin's `field_0x68`, then `HandleNewChannelConnection` does
/// `manager->QI(d3e07363)` and calls `GetService(guid,out)` at vtable slot 3 to
/// obtain the host services it needs — data / audio / screen-capture / bitmap-
/// render). Implementing it (even returning nothing) stops the QI from failing,
/// which was the source of the add-in's null-deref crash.
#[interface("d3e07363-087c-476c-86a7-dbb15f46ddb4")]
unsafe trait IMsRdcServiceProvider: IUnknown {
    /// `GetService(REFGUID guidService, void** ppvObject)`.
    unsafe fn GetService(&self, guid: *const GUID, out: *mut *mut core::ffi::c_void) -> HRESULT;
}

/// Canonical `{8-4-4-4-12}` rendering of a GUID for logging.
fn guid_string(g: &GUID) -> String {
    let d = g.data4;
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

// ---------------------------------------------------------------------------
// Null-safe host-service stub.
//
// The add-in acquires up to four host services from us via `GetService` (data /
// audio-capture / screen-capture / bitmap-render). RE shows its teardown path is
// buggy: e.g. `RtcRemoteAudioCapture::StopCapture` does
// `this->audioSvc /*+0x38*/ ->vtbl[5](sink)` with **no null check**. When we
// answered `GetService` with `E_NOINTERFACE` (service absent) that field stayed
// null, so tearing down an optimized call under load null-dereffed and killed the
// whole rdpio process. The call itself works without the services — the add-in
// degrades gracefully on the *setup* path — the only defect is the missing
// null-guard on teardown.
//
// So we hand back a real, non-null COM object whose only job is to exist and do
// nothing. Teardown then calls a live no-op instead of faulting.
//
// Safety choices that keep this from regressing the working call or crashing on
// its own:
//   * `QueryInterface` returns `self` **only** for `IUnknown`; every typed IID
//     gets `E_NOINTERFACE`. So the add-in can't obtain a functional typed
//     interface and stays on exactly the fallback media path it uses today — we
//     change nothing about how the call runs, we only make teardown safe.
//   * It's a leaked `'static` singleton; `AddRef`/`Release` never free it, so the
//     add-in's ref-counting (however unbalanced on error paths) can't cause a
//     use-after-free.
//   * A generous no-op vtable (64 slots): any slot the add-in calls directly on
//     the raw service returns `E_NOTIMPL` rather than running off the end.
// Everything is logged (secret-safe: only the requested IID and slot index) so a
// live test shows whether this clears the crash and which IIDs the add-in wanted.
// ---------------------------------------------------------------------------

/// Number of no-op method slots after the three `IUnknown` slots.
const STUB_METHOD_SLOTS: usize = 61;

#[repr(C)]
struct HostServiceVtbl {
    query_interface:
        unsafe extern "system" fn(*mut HostService, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut HostService) -> u32,
    release: unsafe extern "system" fn(*mut HostService) -> u32,
    /// Every unknown method: a no-op returning `E_NOTIMPL`.
    methods: [unsafe extern "system" fn(
        *mut HostService,
        usize,
        usize,
        usize,
        usize,
    ) -> HRESULT; STUB_METHOD_SLOTS],
}

#[repr(C)]
struct HostService {
    vtbl: *const HostServiceVtbl,
}

// The singleton is stateless (only a vtable pointer to a `'static` table) and is
// only ever read, so sharing it across the add-in's threads is sound.
unsafe impl Sync for HostService {}
unsafe impl Sync for HostServiceVtbl {}

/// Rate-limit logging so continuous media callbacks can't flood the log: log the
/// first few hits of each distinct (call-site) counter, then stay quiet.
fn log_throttle(counter: &AtomicU32, max: u32) -> bool {
    counter.fetch_add(1, Ordering::Relaxed) < max
}

unsafe extern "system" fn stub_query_interface(
    this: *mut HostService,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    static QI_LOGS: AtomicU32 = AtomicU32::new(0);
    let iid = if riid.is_null() { GUID::from_u128(0) } else { unsafe { *riid } };
    // Hand back the object for IUnknown only; refuse every typed interface so the
    // add-in keeps using its own fallback media path (no behaviour change to the
    // running call) while still holding a valid non-null pointer for teardown.
    let is_iunknown = iid == IUnknown::IID;
    if log_throttle(&QI_LOGS, 64) {
        tracing::debug!(
            iid = %guid_string(&iid),
            granted = is_iunknown,
            "host-service stub: QueryInterface"
        );
    }
    if is_iunknown && !ppv.is_null() {
        unsafe { *ppv = this as *mut c_void };
        return S_OK;
    }
    if !ppv.is_null() {
        unsafe { *ppv = core::ptr::null_mut() };
    }
    E_NOINTERFACE
}

unsafe extern "system" fn stub_add_ref(_this: *mut HostService) -> u32 {
    // Leaked singleton: report a stable non-zero count and never free.
    2
}

unsafe extern "system" fn stub_release(_this: *mut HostService) -> u32 {
    2
}

/// Generate the 61 distinct no-op method thunks (so each logs its own slot index)
/// and assemble them into the vtable's `methods` array.
macro_rules! stub_methods {
    ($($idx:literal),+ $(,)?) => {
        [$(
            {
                unsafe extern "system" fn thunk(
                    _this: *mut HostService,
                    _a: usize, _b: usize, _c: usize, _d: usize,
                ) -> HRESULT {
                    static HITS: AtomicU32 = AtomicU32::new(0);
                    if log_throttle(&HITS, 4) {
                        // slot index = 3 (IUnknown) + array position.
                        tracing::debug!(slot = 3 + $idx, "host-service stub: no-op method called");
                    }
                    E_NOTIMPL
                }
                thunk as unsafe extern "system" fn(*mut HostService, usize, usize, usize, usize) -> HRESULT
            }
        ),+]
    };
}

static HOST_SERVICE_VTBL: HostServiceVtbl = HostServiceVtbl {
    query_interface: stub_query_interface,
    add_ref: stub_add_ref,
    release: stub_release,
    methods: stub_methods!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
        48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60,
    ),
};

static HOST_SERVICE: HostService = HostService {
    vtbl: &HOST_SERVICE_VTBL,
};

/// Pointer to the leaked null-safe service singleton, as `GetService` returns it.
fn host_service_ptr() -> *mut c_void {
    &HOST_SERVICE as *const HostService as *mut c_void
}

/// Outbound bytes the plugin wants sent to the server, tagged by channel. Shared
/// between the plugin's `IWTSVirtualChannel::Write` (any thread) and the mux.
type Outbound = Arc<Mutex<VecDeque<(u32, Vec<u8>)>>>;
/// Channel names the plugin registered listeners for, so the mux's `claims()` can
/// answer without touching COM. Shared with the host thread.
type Names = Arc<Mutex<Vec<String>>>;

// ---------------------------------------------------------------------------
// webrtc.1 wire capture (opt-in via the `RDPIO_WEBRTC_CAPTURE=<path>` env var).
//
// The add-in's protocol on `com.microsoft.rdc.dvc.webrtc.1` is the contract we
// must reverse to reimplement Teams "Optimized" natively (e.g. drive `webrtc-rs`
// on Linux, where the Windows DLL can't run). The media itself is standard WebRTC
// (SDP/ICE/RTP — the add-in bundles stock libwebrtc); only this DVC framing is
// Microsoft-specific. So we log every *logical* message in both directions to a
// binary file for offline analysis. ICE ufrag/pwd and TURN creds that appear here
// are ephemeral per-session negotiation material (safe to analyze) — but this is
// gated off by default and writes to a file, never the main log.
//
// File format: header `b"WRTC1\0"`; then records, each little-endian:
//   dir(u8: 0x53 'S'=server→add-in inbound, 0x43 'C'=add-in→server outbound),
//   channel_id(u32), seq(u32), t_ms(u32 since capture open), len(u32), payload.
// ---------------------------------------------------------------------------

const CAP_DIR_INBOUND: u8 = b'S';
const CAP_DIR_OUTBOUND: u8 = b'C';

struct CaptureInner {
    file: File,
    seq: u32,
    start: Instant,
}

/// A shared, thread-safe capture sink for the webrtc.1 wire.
#[derive(Clone)]
struct Capture(Arc<Mutex<CaptureInner>>);

impl Capture {
    /// Open the capture file named by `RDPIO_WEBRTC_CAPTURE`, or `None` if the env
    /// var is unset or the file can't be created.
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("RDPIO_WEBRTC_CAPTURE")?;
        if path.is_empty() {
            return None;
        }
        match File::create(&path) {
            Ok(mut file) => {
                let _ = file.write_all(b"WRTC1\0");
                tracing::info!(path = %Path::new(&path).display(), "webrtc.1 wire capture enabled");
                Some(Capture(Arc::new(Mutex::new(CaptureInner {
                    file,
                    seq: 0,
                    start: Instant::now(),
                }))))
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not open RDPIO_WEBRTC_CAPTURE file; capture disabled");
                None
            }
        }
    }

    /// Append one logical DVC message. Best-effort: capture errors never disrupt
    /// the live call.
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

// ---------------------------------------------------------------------------
// Host-side COM objects we implement for the plugin to call into.
// ---------------------------------------------------------------------------

/// Our [`IWTSVirtualChannelManager`]: the plugin calls `CreateListener` during
/// `Initialize` to register the channel names it wants. It also QueryInterfaces
/// us for [`IMsRdcServiceProvider`] to acquire host media services, so we
/// implement that too.
#[implement(IWTSVirtualChannelManager, IMsRdcServiceProvider)]
struct ChannelManager {
    /// `name → listener callback`, read on the host thread when a channel opens.
    /// `Rc`/`RefCell`: only ever touched on the host thread (CreateListener runs
    /// synchronously inside `plugin.Initialize`).
    listeners: Rc<RefCell<Vec<(String, IWTSListenerCallback)>>>,
    /// Mirror of the registered names for the mux's `claims()`.
    names: Names,
}

impl IWTSVirtualChannelManager_Impl for ChannelManager_Impl {
    fn CreateListener(
        &self,
        pszchannelname: &windows::core::PCSTR,
        _uflags: u32,
        plistenercallback: Ref<IWTSListenerCallback>,
    ) -> Result<IWTSListener> {
        let name = unsafe { pszchannelname.to_string() }.unwrap_or_default();
        tracing::info!(%name, "WebRTC add-in registered a DVC listener");
        self.names.lock().unwrap().push(name.clone());
        if let Ok(cb) = plistenercallback.ok() {
            self.listeners.borrow_mut().push((name, cb.clone()));
        }
        Ok(Listener {}.into())
    }
}

impl IMsRdcServiceProvider_Impl for ChannelManager_Impl {
    unsafe fn GetService(&self, guid: *const GUID, out: *mut *mut core::ffi::c_void) -> HRESULT {
        let g = if guid.is_null() {
            GUID::from_u128(0)
        } else {
            unsafe { *guid }
        };
        if out.is_null() {
            return E_FAIL;
        }
        // Hand back the null-safe stub (see `HostService`). This makes the
        // service field the add-in stores non-null, so its teardown paths (e.g.
        // `RtcRemoteAudioCapture::StopCapture`, which derefs the audio service
        // with no null check) call a live no-op instead of faulting — the crash
        // we saw when pushing a call under load. The stub refuses every typed
        // QueryInterface, so the running call still uses the add-in's own
        // fallback media path exactly as before; only teardown is made safe.
        tracing::info!(
            service = %guid_string(&g),
            "Teams add-in requested a host service (GetService) — returning null-safe stub"
        );
        unsafe { *out = host_service_ptr() };
        S_OK
    }
}

/// Our [`IWTSListener`]. The plugin may query per-channel configuration; we have
/// none to offer, so report `E_NOTIMPL` and let it use its defaults.
#[implement(IWTSListener)]
struct Listener;

impl IWTSListener_Impl for Listener_Impl {
    fn GetConfiguration(&self) -> Result<IPropertyBag> {
        Err(E_NOTIMPL.into())
    }
}

/// Our [`IWTSVirtualChannel`] for one open DVC: the plugin's `Write` queues bytes
/// for the mux to frame and send to the server.
#[implement(IWTSVirtualChannel)]
struct VirtualChannel {
    channel_id: u32,
    outbound: Outbound,
    capture: Option<Capture>,
}

impl IWTSVirtualChannel_Impl for VirtualChannel_Impl {
    fn Write(
        &self,
        cbsize: u32,
        pbuffer: *const u8,
        _preserved: Ref<windows::core::IUnknown>,
    ) -> Result<()> {
        let bytes = if pbuffer.is_null() || cbsize == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(pbuffer, cbsize as usize) }.to_vec()
        };
        tracing::debug!(channel_id = self.channel_id, len = bytes.len(), "WebRTC add-in → server");
        if let Some(cap) = &self.capture {
            cap.record(CAP_DIR_OUTBOUND, self.channel_id, &bytes);
        }
        self.outbound.lock().unwrap().push_back((self.channel_id, bytes));
        Ok(())
    }

    fn Close(&self) -> Result<()> {
        tracing::info!(channel_id = self.channel_id, "WebRTC add-in closed its DVC");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// COM host thread.
// ---------------------------------------------------------------------------

unsafe fn load_addin(path: &Path) -> Result<HMODULE> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH) }
}

/// `VirtualChannelGetInstance(IID_IWTSPlugin)` → one plugin instance.
unsafe fn get_plugin(get_instance: GetInstanceFn) -> Result<IWTSPlugin> {
    let iid = IWTSPlugin::IID;
    let mut num: u32 = 1;
    let mut obj: *mut c_void = std::ptr::null_mut();
    unsafe { get_instance(&iid, &mut num, &mut obj) }.ok()?;
    if obj.is_null() {
        return Err(E_FAIL.into());
    }
    Ok(unsafe { IWTSPlugin::from_raw(obj) })
}

fn run_host(
    dll: PathBuf,
    rx: Receiver<HostMsg>,
    names: Names,
    outbound: Outbound,
    capture: Option<Capture>,
    ready: SyncSender<bool>,
) {
    unsafe {
        // Host the add-in on an STA: it creates hook windows (hence the message
        // pump below) and initializes STA-affine media COM (Media Foundation /
        // DirectShow-DMO / audio). Under MTA its internal setup QI'd an object
        // that came back E_NOINTERFACE and its cleanup path null-derefed (crash at
        // RtcRemoteDesktopPluginImpl.cpp). msrdc hosts DVC plugins on an STA too.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let module = match load_addin(&dll) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "WebRTC add-in: LoadLibrary failed");
                let _ = ready.send(false);
                return;
            }
        };
        let Some(proc) = GetProcAddress(module, s!("VirtualChannelGetInstance")) else {
            tracing::warn!("WebRTC add-in: VirtualChannelGetInstance export missing");
            let _ = FreeLibrary(module);
            let _ = ready.send(false);
            return;
        };
        let get_instance: GetInstanceFn = std::mem::transmute(proc);

        let plugin = match get_plugin(get_instance) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "WebRTC add-in: could not create IWTSPlugin");
                let _ = FreeLibrary(module);
                let _ = ready.send(false);
                return;
            }
        };

        let listeners: Rc<RefCell<Vec<(String, IWTSListenerCallback)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let manager: IWTSVirtualChannelManager = ChannelManager {
            listeners: listeners.clone(),
            names,
        }
        .into();

        if let Err(e) = plugin.Initialize(&manager) {
            tracing::warn!(error = %e, "WebRTC add-in: Initialize failed");
            let _ = FreeLibrary(module);
            let _ = ready.send(false);
            return;
        }
        let _ = plugin.Connected();
        tracing::info!(
            listeners = listeners.borrow().len(),
            "Teams WebRTC add-in initialized and connected"
        );
        let _ = ready.send(true);

        let mut channels: HashMap<u32, IWTSVirtualChannelCallback> = HashMap::new();
        // The add-in creates hidden hook windows (scraper / cursor injector) that
        // need a running message loop on this thread. So instead of blocking on
        // the control channel, interleave: drain queued control messages, pump the
        // window queue, then idle ~10 ms (waking early on any window message).
        let mut running = true;
        while running {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                HostMsg::NewChannel { channel_id, name } => {
                    let lcb = listeners
                        .borrow()
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, c)| c.clone());
                    let Some(lcb) = lcb else { continue };
                    let vc: IWTSVirtualChannel = VirtualChannel {
                        channel_id,
                        outbound: outbound.clone(),
                        capture: capture.clone(),
                    }
                    .into();
                    let mut accept = BOOL(0);
                    let mut cb: Option<IWTSVirtualChannelCallback> = None;
                    let empty = BSTR::default();
                    match lcb.OnNewChannelConnection(&vc, &empty, &mut accept, &mut cb) {
                        Ok(()) => {
                            tracing::info!(channel_id, %name, accept = accept.as_bool(), "WebRTC add-in bound new channel");
                            if let Some(cb) = cb {
                                channels.insert(channel_id, cb);
                            }
                        }
                        Err(e) => tracing::warn!(channel_id, error = %e, "OnNewChannelConnection failed"),
                    }
                }
                HostMsg::Data { channel_id, data } => {
                    if let Some(cb) = channels.get(&channel_id) {
                        if let Err(e) = cb.OnDataReceived(&data) {
                            tracing::warn!(channel_id, error = %e, "OnDataReceived failed");
                        }
                    }
                }
                HostMsg::Close { channel_id } => {
                    if let Some(cb) = channels.remove(&channel_id) {
                        let _ = cb.OnClose();
                    }
                }
                HostMsg::Shutdown => {
                    running = false;
                    break;
                }
                }
            }
            if !running {
                break;
            }
            // Pump the add-in's hook-window messages so its scraper / cursor-
            // injector windows work, then idle until a window message arrives or
            // ~10 ms elapses (then re-drain the control channel).
            let mut wmsg = MSG::default();
            while PeekMessageW(&mut wmsg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&wmsg);
                DispatchMessageW(&wmsg);
            }
            MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        let _ = plugin.Terminated();
        let _ = FreeLibrary(module);
    }
}

// ---------------------------------------------------------------------------
// The Send handle the mux holds.
// ---------------------------------------------------------------------------

/// Handle to the hosted Teams WebRTC add-in. Implements [`DvcRedirector`] so the
/// graphics DVC mux can offer it channels it would otherwise decline.
pub struct WebRtcRedirector {
    tx: Sender<HostMsg>,
    names: Names,
    outbound: Outbound,
    /// webrtc.1 wire capture (inbound side); `None` unless `RDPIO_WEBRTC_CAPTURE`
    /// is set. Cloned onto the host thread for the outbound side.
    capture: Option<Capture>,
    thread: Option<JoinHandle<()>>,
}

impl WebRtcRedirector {
    /// Locate, load, and initialize the add-in. Returns `None` (→ rdpio keeps
    /// declining the WebRTC channel) if the DLL is missing or the plugin refuses
    /// to initialize. Blocks up to 10 s for the plugin's one-time init.
    pub fn new() -> Option<Self> {
        let Some(dll) = resolve_addin_dll() else {
            tracing::warn!(
                "Teams WebRTC add-in DLL not found — install the Windows App / AVD HostApp \
                 (or drop MsRdcWebRTCAddIn.dll next to rdpio.exe); staying on decline"
            );
            return None;
        };
        tracing::info!(path = %dll.display(), "hosting Teams WebRTC add-in");

        let names: Names = Arc::new(Mutex::new(Vec::new()));
        let outbound: Outbound = Arc::new(Mutex::new(VecDeque::new()));
        let capture = Capture::from_env();
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let (n, o, cap) = (names.clone(), outbound.clone(), capture.clone());
        let thread = std::thread::Builder::new()
            .name("webrtc-addin".into())
            .spawn(move || run_host(dll, rx, n, o, cap, ready_tx))
            .ok()?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(true) => Some(Self {
                tx,
                names,
                outbound,
                capture,
                thread: Some(thread),
            }),
            other => {
                tracing::warn!(?other, "Teams WebRTC add-in did not initialize; staying on decline");
                None
            }
        }
    }
}

impl DvcRedirector for WebRtcRedirector {
    fn claims(&self, name: &str) -> bool {
        self.names
            .lock()
            .map(|n| n.iter().any(|x| x == name))
            .unwrap_or(false)
    }

    fn on_create(&mut self, channel_id: u32, name: &str) -> bool {
        if !self.claims(name) {
            return false;
        }
        self.tx
            .send(HostMsg::NewChannel {
                channel_id,
                name: name.to_string(),
            })
            .is_ok()
    }

    fn on_data(&mut self, channel_id: u32, message: &[u8]) {
        if let Some(cap) = &self.capture {
            cap.record(CAP_DIR_INBOUND, channel_id, message);
        }
        let _ = self.tx.send(HostMsg::Data {
            channel_id,
            data: message.to_vec(),
        });
    }

    fn on_close(&mut self, channel_id: u32) {
        let _ = self.tx.send(HostMsg::Close { channel_id });
    }

    fn drain_outbound(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.outbound
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

impl Drop for WebRtcRedirector {
    fn drop(&mut self) {
        let _ = self.tx.send(HostMsg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Find `MsRdcWebRTCAddIn.dll`: next to our exe first (a user-supplied copy), then
/// the newest WindowsApps package (AVD HostApp / Windows365 / Remote Desktop) that
/// ships it. `None` if not present or WindowsApps is unreadable.
fn resolve_addin_dll() -> Option<PathBuf> {
    const NAME: &str = "MsRdcWebRTCAddIn.dll";

    // Explicit override: point rdpio straight at the DLL. Needed when the add-in lives
    // in a WindowsApps package whose ROOT a normal process can't enumerate (ACL-locked
    // to TrustedInstaller) even though the specific file path is readable — the common
    // case on locked-down clients. Loaded with `LOAD_WITH_ALTERED_SEARCH_PATH`, so the
    // add-in's sibling dependencies still resolve from its package directory.
    if let Some(p) = std::env::var_os("RDPIO_WEBRTC_ADDIN_DLL") {
        let p = PathBuf::from(p);
        if p.exists() {
            tracing::info!(path = %p.display(), "using RDPIO_WEBRTC_ADDIN_DLL override for the Teams add-in");
            return Some(p);
        }
        tracing::warn!(path = %p.display(), "RDPIO_WEBRTC_ADDIN_DLL is set but the file does not exist; falling back to search");
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(NAME);
            if p.exists() {
                return Some(p);
            }
        }
    }

    const PREFIXES: [&str; 3] = [
        "MicrosoftCorporationII.AzureVirtualDesktopHostApp_",
        "MicrosoftCorporationII.Windows365_",
        "Microsoft.RemoteDesktop_",
    ];
    let base = PathBuf::from(r"C:\Program Files\WindowsApps");
    let mut best: Option<(String, PathBuf)> = None;
    match std::fs::read_dir(&base) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let folder = entry.file_name().to_string_lossy().into_owned();
                if !PREFIXES.iter().any(|p| folder.starts_with(p)) {
                    continue;
                }
                // The add-in sits at <pkg>\ or <pkg>\msrdc\ per package layout.
                for sub in ["", "msrdc"] {
                    let cand = if sub.is_empty() {
                        entry.path().join(NAME)
                    } else {
                        entry.path().join(sub).join(NAME)
                    };
                    if cand.exists() && best.as_ref().is_none_or(|(b, _)| folder > *b) {
                        best = Some((folder.clone(), cand));
                    }
                }
            }
        }
        Err(e) => {
            // WindowsApps is ACL-restricted; on some machines a normal process
            // can't enumerate it. Surface it instead of failing silently.
            tracing::warn!(error = %e, "could not enumerate C:\\Program Files\\WindowsApps for the add-in");
        }
    }
    match &best {
        Some((pkg, path)) => tracing::info!(package = %pkg, path = %path.display(), "found Teams WebRTC add-in"),
        None => tracing::warn!("no MsRdcWebRTCAddIn.dll found in any WindowsApps package or next to rdpio.exe"),
    }
    best.map(|(_, p)| p)
}
