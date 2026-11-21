//! rdpio Display: the Hyper virtual display (`drivers/rdpio-display`).
//!
//! With the driver installed, the agent streams a monitor made for the
//! client: exactly its resolution and refresh rate (no scaling, no black
//! bars), rendered on the GPU that encodes. While it streams, that monitor is
//! the only one on (the host's own screens go dark, as with consolidation),
//! and afterwards the host's layout comes back and the monitor goes away.
//! If the agent dies, the driver removes the monitor by itself (its
//! watchdog) and the next agent restores the layout.
//!
//! The driver's interface is `drivers/rdpio-display/src/Public.h`: keep this
//! file in step with it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use windows::core::{w, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};

/// GUID_DEVINTERFACE_RDPIO_DISPLAY.
const INTERFACE: GUID = GUID::from_u128(0x6d3b1a7e_2c41_4f0e_9a51_3e8b7c215d94);
const INTERFACE_VERSION: u32 = 1;
/// The monitors' EDID manufacturer and product: "RDO", 0x0001.
const EDID_MANUFACTURER: u16 = 0x488F;
const EDID_PRODUCT: u16 = 0x0001;
/// The hardware id the INF installs on.
pub(crate) const HARDWARE_ID: &str = r"Root\RdpioDisplay";

const fn ioctl(n: u32) -> u32 {
    // CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900 + n, METHOD_BUFFERED, FILE_ANY_ACCESS)
    (0x22 << 16) | ((0x900 + n) << 2)
}
const IOCTL_INFO: u32 = ioctl(0);
const IOCTL_ADD: u32 = ioctl(1);
const IOCTL_REMOVE: u32 = ioctl(2);
const IOCTL_PING: u32 = ioctl(3);
const FLAG_REPLACE: u32 = 1;

#[repr(C, packed(4))]
#[derive(Default, Clone, Copy)]
struct Info {
    interface_version: u32,
    max_monitors: u32,
    monitors: u32,
    adapter_ready: u32,
}

#[repr(C, packed(4))]
#[derive(Default, Clone, Copy)]
struct ModeRequest {
    width: u32,
    height: u32,
    refresh_hz: u32,
    width_mm: u32,
    height_mm: u32,
    flags: u32,
    render_adapter: LUID,
}

#[repr(C, packed(4))]
#[derive(Default, Clone, Copy)]
struct Added {
    monitor_id: u32,
    connector_index: u32,
}

/// The size a virtual monitor for a `width`×`height` client gets: even
/// (4:2:0 video), within what the driver takes.
pub fn even_size(width: u32, height: u32) -> (u32, u32) {
    (width.clamp(640, 7680) & !1, height.clamp(480, 4320) & !1)
}

/// How long each driver call may take before the driver counts as hung: a
/// call that never comes back would stall the agent, which answers no sign-in
/// meanwhile (a removal once hung about 8 s, until Windows gave up on the
/// driver).
const INFO_TIMEOUT: Duration = Duration::from_secs(1);
const PING_TIMEOUT: Duration = Duration::from_secs(1);
const ADD_TIMEOUT: Duration = Duration::from_secs(3);
const REMOVE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a new monitor waits for the driver's adapter, which starts a
/// moment after the device does.
const ADAPTER_WAIT: Duration = Duration::from_secs(2);
/// How often a monitor's driver is asked whether it's still there (it also
/// keeps the driver's watchdog from removing the monitor).
const PING_EVERY: Duration = Duration::from_secs(1);
/// How long a device that's there without its interface is given to start
/// before it counts as stuck.
const STARTING_GRACE: Duration = Duration::from_secs(2);
/// How long the driver must have run before its restart count starts over:
/// one that dies as it starts must not make a restart loop.
const RUNNING_STEADY: Duration = Duration::from_secs(120);
/// What a restart may take inside a sign-in (the client gives the host 15 s
/// to start the stream), and from the launcher's Repair.
pub const SIGN_IN_HEAL: Duration = Duration::from_secs(3);
const REPAIR_BUDGET: Duration = Duration::from_secs(20);

/// A virtual monitor's driver stopped answering while the monitor existed
/// (cleared as the next one is made).
static LOST: AtomicBool = AtomicBool::new(false);
/// How many virtual monitors exist right now (a stream's, or one kept for a
/// quick reconnect).
static SHOWING: AtomicUsize = AtomicUsize::new(0);

/// Whether the driver of the virtual monitor that's up stopped answering:
/// the monitor is gone with it.
pub fn lost() -> bool {
    LOST.load(Ordering::Relaxed)
}

/// Whether a virtual monitor exists (the device is never restarted then).
pub fn showing() -> bool {
    SHOWING.load(Ordering::Relaxed) > 0
}

/// Device Manager's problem codes that matter here.
const PROBLEM_DISABLED: u32 = 22;
/// Problems a restart doesn't fix: not installed (1, 18, 28), a restart of
/// the PC due (14), the driver's service off (32), blocked (48), unsigned or
/// test signing off (52).
const BLOCKED_PROBLEMS: [u32; 7] = [1, 14, 18, 28, 32, 48, 52];

/// The rdpio Display device's state, as Windows reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Its interface is there.
    Running,
    /// The device is there and not failed, but its interface isn't (yet), or
    /// the driver doesn't answer.
    Starting,
    /// Windows stopped the driver (the problem code, typically 43).
    Failed(u32),
    /// Switched off in Device Manager (the user's choice: only Repair turns
    /// it back on).
    Disabled,
    /// Windows won't load it (the problem code): a restart of the device
    /// doesn't help.
    Blocked(u32),
    /// Not installed.
    Missing,
}

impl Health {
    /// Device Manager's problem code, where there is one.
    pub fn problem(self) -> Option<u32> {
        match self {
            Health::Failed(p) | Health::Blocked(p) => Some(p),
            Health::Disabled => Some(PROBLEM_DISABLED),
            _ => None,
        }
    }

    /// The launcher's name for it ("installed": there, not running yet).
    pub fn name(self) -> &'static str {
        match self {
            Health::Running => "running",
            Health::Starting => "installed",
            Health::Failed(_) => "failed",
            Health::Disabled => "disabled",
            Health::Blocked(_) => "blocked",
            Health::Missing => "missing",
        }
    }
}

impl std::fmt::Display for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Health::Running => f.write_str("running"),
            Health::Starting => f.write_str("starting"),
            Health::Failed(p) => write!(f, "stopped by Windows (code {p})"),
            Health::Disabled => f.write_str("disabled in Device Manager"),
            Health::Blocked(p) => write!(f, "not loaded by Windows (code {p})"),
            Health::Missing => f.write_str("not installed"),
        }
    }
}

/// Our device's state from its device node (status flags and problem code,
/// if there is one) and whether its interface is there.
fn classify(devnode: Option<(u32, u32)>, interface: bool) -> Health {
    if interface {
        return Health::Running;
    }
    match devnode {
        None => Health::Missing,
        Some((_, PROBLEM_DISABLED)) => Health::Disabled,
        Some((_, p)) if BLOCKED_PROBLEMS.contains(&p) => Health::Blocked(p),
        Some((_, 0)) => Health::Starting,
        Some((_, p)) => Health::Failed(p),
    }
}

/// The open-source driver's state: it has no interface of ours, and off
/// (disabled) is how rdpio keeps it between streams.
fn mtt_classify(devnode: Option<(u32, u32)>) -> Health {
    match devnode {
        None => Health::Missing,
        Some((_, 0)) => Health::Running,
        Some((_, PROBLEM_DISABLED)) => Health::Disabled,
        Some((_, p)) if BLOCKED_PROBLEMS.contains(&p) => Health::Blocked(p),
        Some((_, p)) => Health::Failed(p),
    }
}

/// The rdpio Display device's state. Works unelevated: it reads the device
/// node and the interface list, and opens nothing.
pub fn health() -> Health {
    // (The interface first: a device that starts between the two reads
    // counts as starting, not missing.)
    let interface = interface_path().is_some();
    classify(devnode_info(HARDWARE_ID).map(|(_, s, p)| (s, p)), interface)
}

fn mtt_health() -> Health {
    mtt_classify(devnode_info(MTT_HARDWARE_ID).map(|(_, s, p)| (s, p)))
}

/// When the next restart of a failed device may be tried: at once, then
/// after 10 s, 60 s and 5 minutes, then every 30 minutes.
#[derive(Debug, Default)]
struct Backoff {
    attempts: u32,
    next: Option<Instant>,
}

impl Backoff {
    fn due(&self, now: Instant) -> bool {
        self.next.map_or(true, |t| now >= t)
    }

    /// An attempt was made at `now`: how long until the next one.
    fn tried(&mut self, now: Instant) -> Duration {
        self.attempts += 1;
        let wait = heal_delay(self.attempts);
        self.next = Some(now + wait);
        wait
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The wait after `attempts` restarts.
fn heal_delay(attempts: u32) -> Duration {
    Duration::from_secs(match attempts {
        0 => 0,
        1 => 10,
        2 => 60,
        3 => 300,
        _ => 1800,
    })
}

/// A driver call's error that means the driver (or its host process) is
/// gone or hung, rather than a request it turned down.
fn gone(code: u32) -> bool {
    // ERROR_INVALID_HANDLE, ERROR_GEN_FAILURE, ERROR_BAD_COMMAND,
    // ERROR_DEV_NOT_EXIST, WAIT_TIMEOUT, ERROR_NO_SUCH_DEVICE,
    // ERROR_OPERATION_ABORTED, ERROR_IO_INCOMPLETE, ERROR_DEVICE_NOT_CONNECTED,
    // ERROR_DEVICE_REMOVED.
    matches!(code, 6 | 31 | 22 | 55 | 258 | 433 | 995 | 996 | 1167 | 1617)
}

/// The Win32 error code inside an HRESULT (or the HRESULT itself).
fn win32_code(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    }
}

fn timed_out(e: &windows::core::Error) -> bool {
    matches!(win32_code(e), 258 | 996)
}

/// A driver the agent makes the client's monitor with: ours (rdpio
/// Display), or else the signed open-source Virtual Display Driver
/// (github.com/VirtualDrivers/Virtual-Display-Driver), which works without
/// test signing.
///
/// Holds no device handle: a monitor holds one for as long as it exists
/// (a handle kept open stops Windows from restarting a driver that failed,
/// and from updating it). Which driver is usable is checked again right
/// before every offer, and a failed rdpio Display is restarted between
/// streams.
pub struct VirtualDisplay {
    backend: Option<Backend>,
    health: Health,
    /// The driver answers with another interface version than this rdpio's.
    version: Option<u32>,
    checked: Instant,
    starting_since: Option<Instant>,
    running_since: Option<Instant>,
    backoff: Backoff,
    /// Windows wants the PC restarted to start the device again: not tried
    /// again by this agent.
    reboot_due: bool,
    /// What the last state line said.
    logged: Option<(Health, &'static str, Option<u32>)>,
}

enum Backend {
    Ours,
    /// Configured through its settings file and its control pipe; the
    /// device is enabled only while a stream needs it (if it was off).
    Mtt { settings: PathBuf },
}

struct Handle(HANDLE);
// SAFETY: a device handle; DeviceIoControl may be called from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// One driver call's buffers, which must outlive it: a call that times out
/// leaves them to the driver.
#[repr(C)]
struct Request<I, O> {
    overlapped: windows::Win32::System::IO::OVERLAPPED,
    input: I,
    output: O,
}

impl Handle {
    fn open(path: &str) -> windows::core::Result<Self> {
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        unsafe {
            CreateFileW(
                &HSTRING::from(path),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .map(Handle)
        }
    }

    /// A driver call that gives up after `timeout`: then it's cancelled, and
    /// if the driver doesn't let go of it either, its buffers are leaked
    /// (the driver may still write them) and the error says it timed out.
    fn ioctl<I: Copy + Default, O: Copy + Default>(&self, code: u32, input: Option<&I>, timeout: Duration) -> windows::core::Result<O> {
        use windows::core::HRESULT;
        use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResultEx, OVERLAPPED};
        use windows::Win32::System::Threading::CreateEventW;
        const ERROR_IO_PENDING: u32 = 997;
        const WAIT_TIMEOUT: u32 = 258;
        unsafe {
            let event = CreateEventW(None, true, false, PCWSTR::null())?;
            let mut req = Box::new(Request { overlapped: OVERLAPPED { hEvent: event, ..Default::default() }, input: input.copied().unwrap_or_default(), output: O::default() });
            let in_len = input.map_or(0, |_| std::mem::size_of::<I>() as u32);
            let out_len = std::mem::size_of::<O>() as u32;
            let in_ptr = input.map(|_| &req.input as *const I as *const core::ffi::c_void);
            let out_ptr = (out_len > 0).then(|| &mut req.output as *mut O as *mut core::ffi::c_void);
            if let Err(e) = DeviceIoControl(self.0, code, in_ptr, in_len, out_ptr, out_len, None, Some(&mut req.overlapped)) {
                if win32_code(&e) != ERROR_IO_PENDING {
                    let _ = CloseHandle(event);
                    return Err(e);
                }
            }
            let mut got = 0u32;
            let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
            match GetOverlappedResultEx(self.0, &req.overlapped, &mut got, ms, false) {
                Ok(()) => {
                    let out = req.output;
                    let _ = CloseHandle(event);
                    Ok(out)
                }
                Err(e) if timed_out(&e) => {
                    let _ = CancelIoEx(self.0, Some(&req.overlapped));
                    if GetOverlappedResultEx(self.0, &req.overlapped, &mut got, 100, false).is_err_and(|e| timed_out(&e)) {
                        // Still the driver's: its buffers and event stay.
                        let _ = Box::leak(req);
                    } else {
                        let _ = CloseHandle(event);
                    }
                    Err(HRESULT::from_win32(WAIT_TIMEOUT).into())
                }
                Err(e) => {
                    let _ = CloseHandle(event);
                    Err(e)
                }
            }
        }
    }
}

/// An open handle to rdpio Display, and whether its driver went away.
struct Link {
    handle: Handle,
    /// A call found the driver gone or hung.
    dead: AtomicBool,
    /// Its monitor is being removed: the ping thread stops.
    closing: AtomicBool,
    /// The driver's flight recorder (none from a driver without one).
    trace: Option<Trace>,
    /// What it said was logged already.
    reported: AtomicBool,
}

/// Why rdpio Display can't be talked to.
enum LinkError {
    /// No interface: the driver isn't running.
    NotRunning,
    /// It's there but can't be opened or doesn't answer.
    NoAnswer(String),
    /// It answers with another interface version.
    Version(u32),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NotRunning => write!(f, "the rdpio Display driver isn't running ({})", health()),
            LinkError::NoAnswer(e) => f.write_str(e),
            LinkError::Version(v) => write!(f, "the rdpio Display driver is another version ({v}, this rdpio needs {INTERFACE_VERSION}); reinstall the host"),
        }
    }
}

impl Link {
    /// Open the driver and ask it what it is.
    fn open() -> Result<(Arc<Link>, Info), LinkError> {
        let path = interface_path().ok_or(LinkError::NotRunning)?;
        let handle = Handle::open(&path).map_err(|e| LinkError::NoAnswer(format!("the rdpio Display driver is there but can't be opened: {e}")))?;
        let link = Link { handle, dead: AtomicBool::new(false), closing: AtomicBool::new(false), trace: Trace::open(), reported: AtomicBool::new(false) };
        let info = link.info().map_err(|e| LinkError::NoAnswer(format!("the rdpio Display driver didn't answer: {e}")))?;
        let version = info.interface_version;
        if version != INTERFACE_VERSION {
            return Err(LinkError::Version(version));
        }
        Ok((Arc::new(link), info))
    }

    /// A driver call; an error that means the driver is gone marks the link
    /// dead.
    fn call<I: Copy + Default, O: Copy + Default>(&self, code: u32, input: Option<&I>, timeout: Duration) -> windows::core::Result<O> {
        let r = self.handle.ioctl(code, input, timeout);
        if let Err(e) = &r {
            if gone(win32_code(e)) {
                self.dead.store(true, Ordering::Relaxed);
            }
        }
        r
    }

    fn info(&self) -> windows::core::Result<Info> {
        self.call::<(), Info>(IOCTL_INFO, None, INFO_TIMEOUT)
    }

    /// Log what the driver was doing as it stopped answering, once, from its
    /// flight recorder (nothing when it has none).
    fn report_death(&self) {
        let Some(trace) = &self.trace else { return };
        if self.reported.swap(true, Ordering::Relaxed) {
            return;
        }
        let snap = trace.snapshot();
        if snap.version != TRACE_VERSION {
            return;
        }
        let now = unsafe { GetTickCount64() };
        let (in_calls, events) = (trace_in_calls(&snap, now), trace_events(&snap, now));
        let (boot, host_pid) = (snap.boot, snap.process_id);
        tracing::warn!(boot, host_pid, "rdpio Display driver died: {in_calls}");
        tracing::info!(boot, "rdpio Display driver's last events: {events}");
    }
}

/// A monitor's driver, asked every second whether it's still there. A
/// failed ping (twice) means it's gone, and the monitor with it: that's
/// [`lost`]. Ends with the monitor.
fn ping_loop(link: Weak<Link>) {
    loop {
        std::thread::park_timeout(PING_EVERY);
        let Some(l) = link.upgrade() else { return };
        if l.closing.load(Ordering::Relaxed) {
            return;
        }
        // The pings go to the handle rather than through `call`, so one slow
        // or failed ping doesn't mark the link dead when the retry answers:
        // only both failing does (below). A dead flag here was set by another
        // call (add, remove, a mode change).
        let r = if l.dead.load(Ordering::Relaxed) {
            Err("a call to it failed".to_string())
        } else {
            l.handle
                .ioctl::<(), ()>(IOCTL_PING, None, PING_TIMEOUT)
                .or_else(|_| {
                    std::thread::sleep(Duration::from_millis(200));
                    l.handle.ioctl::<(), ()>(IOCTL_PING, None, PING_TIMEOUT)
                })
                .map_err(|e| e.to_string())
        };
        if let Err(error) = r {
            if l.closing.load(Ordering::Relaxed) {
                return;
            }
            l.dead.store(true, Ordering::Relaxed);
            LOST.store(true, Ordering::Relaxed);
            tracing::warn!(error = %error, "the rdpio Display driver stopped answering: the virtual monitor is gone");
            l.report_death();
            return;
        }
    }
}

// ------------------------------------------------------------ flight recorder

/// `RDPIO_TRACE` in Public.h: what the driver's threads are inside and its
/// last events, in a section the driver host makes. Mapped (read-only) while
/// a monitor exists: the mapping keeps the section after Windows ends the
/// driver host, so it can be read then.
const TRACE_NAME: &str = r"Global\rdpio-display-trace";
const TRACE_VERSION: u32 = 1;
const TRACE_SLOTS: usize = 4;
const TRACE_RING: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TraceCall {
    code: i32,
    arg: u32,
    start_ms: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TraceEvent {
    tick_ms: u64,
    code: u32,
    arg: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TraceView {
    version: u32,
    boot: u32,
    next: i32,
    process_id: u32,
    in_call: [TraceCall; TRACE_SLOTS],
    ring: [TraceEvent; TRACE_RING],
}

#[link(name = "kernel32")]
extern "system" {
    fn GetTickCount64() -> u64;
}

struct Trace {
    mapping: HANDLE,
    view: windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
}
// SAFETY: a read-only mapping, only ever copied out of.
unsafe impl Send for Trace {}
unsafe impl Sync for Trace {}

impl Trace {
    /// The driver's section, if it has one (older builds don't: nothing to
    /// read, and nothing said).
    fn open() -> Option<Trace> {
        use windows::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, FILE_MAP_READ};
        unsafe {
            let mapping = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(TRACE_NAME)).ok()?;
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, std::mem::size_of::<TraceView>());
            if view.Value.is_null() {
                let _ = CloseHandle(mapping);
                return None;
            }
            Some(Trace { mapping, view })
        }
    }

    fn snapshot(&self) -> TraceView {
        // (The driver may be writing: one entry can come out half written.)
        unsafe { std::ptr::read_volatile(self.view.Value as *const TraceView) }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Memory::UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
        }
    }
}

/// The names of Public.h's RDPIO_TRACE_* codes.
fn trace_code(code: u32) -> &'static str {
    match code {
        1 => "HostStart",
        2 => "DeviceAdd",
        3 => "D0Entry",
        4 => "IoctlInfo",
        5 => "IoctlAdd",
        6 => "IoctlRemove",
        7 => "IoctlPing",
        8 => "AssignSwapChain",
        9 => "UnassignSwapChain",
        10 => "ParseMonitorDescription",
        11 => "DefaultModes",
        12 => "TargetModes",
        13 => "CommitModes",
        14 => "AdapterInitFinished",
        20 => "D3DInit",
        21 => "SetDevice",
        22 => "ReleaseAndAcquireBuffer",
        23 => "Publish",
        24 => "FinishedProcessingFrame",
        25 => "DeleteSwapChain",
        26 => "SetupHardwareCursor",
        27 => "QueryHardwareCursor",
        28 => "StopSwapChainThread",
        29 => "DeviceRemoved",
        30 => "Backoff",
        31 => "ShareTextures",
        40 => "MonitorCreate",
        41 => "MonitorArrival",
        42 => "MonitorDeparture",
        43 => "WatchdogRemove",
        50 => "Exception",
        51 => "Busy",
        52 => "ThreadLeftRunning",
        53 => "NoReader",
        _ => "?",
    }
}

const TRACE_SLOT_NAMES: [&str; TRACE_SLOTS] = ["callback", "swap-chain callback", "swap-chain thread", "watchdog thread"];

/// An event's argument: a failure code in hex, anything else as a number.
fn trace_arg(arg: u32) -> String {
    if arg >= 0x8000_0000 {
        format!("{arg:#010x}")
    } else {
        arg.to_string()
    }
}

/// What each driver thread was inside, and for how long, at `now_ms`.
fn trace_in_calls(t: &TraceView, now_ms: u64) -> String {
    let calls: Vec<String> = t
        .in_call
        .iter()
        .zip(TRACE_SLOT_NAMES)
        .filter(|(c, _)| c.code != 0)
        .map(|(c, slot)| format!("{slot}={}({}) for {} ms", trace_code(c.code as u32), trace_arg(c.arg), now_ms.saturating_sub(c.start_ms)))
        .collect();
    if calls.is_empty() {
        "no driver thread was inside a call".into()
    } else {
        calls.join("; ")
    }
}

/// The ring, oldest first, each with how long before `now_ms` it happened.
fn trace_events(t: &TraceView, now_ms: u64) -> String {
    let next = t.next.max(0) as usize;
    let first = next.saturating_sub(TRACE_RING);
    let events: Vec<String> = (first..next)
        .map(|n| t.ring[n % TRACE_RING])
        .filter(|e| e.code != 0)
        .map(|e| format!("{} {} ({} ms ago)", trace_code(e.code), trace_arg(e.arg), now_ms.saturating_sub(e.tick_ms)))
        .collect();
    if events.is_empty() {
        "none".into()
    } else {
        events.join(", ")
    }
}

/// The device's instance id, status flags and problem code (0: none), for
/// the first device with `hardware_id`.
fn devnode_info(hardware_id: &str) -> Option<(String, u32, u32)> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{CM_Get_DevNode_Status, CM_Get_Device_IDW, CM_DEVNODE_STATUS_FLAGS, CM_PROB, CR_SUCCESS};
    let mut found = None;
    for_each_device(hardware_id, |_, data| unsafe {
        if found.is_some() {
            return;
        }
        let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        if CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) == CR_SUCCESS {
            let mut id = [0u16; 260];
            let name = if CM_Get_Device_IDW(data.DevInst, &mut id, 0) == CR_SUCCESS {
                let n = id.iter().position(|&c| c == 0).unwrap_or(id.len());
                String::from_utf16_lossy(&id[..n])
            } else {
                String::new()
            };
            found = Some((name, status.0, problem.0));
        }
    });
    found
}

/// Wait until rdpio Display answers, with its adapter up if that comes
/// before `deadline`. False: it didn't answer in time.
fn wait_running(deadline: Instant) -> bool {
    let link = loop {
        match Link::open() {
            Ok((_, info)) if info.adapter_ready != 0 => return true,
            Ok((link, _)) => break link,
            // (Running, if not this rdpio's.)
            Err(LinkError::Version(_)) => return true,
            Err(_) if Instant::now() >= deadline => return false,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        match link.info() {
            Ok(i) if i.adapter_ready != 0 => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    // It answers; its adapter is still starting (a monitor waits for it).
    true
}

/// Why a restart of the device didn't bring it back.
enum RestartError {
    /// Windows wants the PC restarted first.
    Reboot,
    Failed(String),
}

/// The driver package beside rdpio.exe, where there is one.
fn driver_inf() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let inf = exe.parent()?.join("rdpio-display").join("rdpio-display.inf");
    inf.exists().then_some(inf)
}

/// Restart rdpio Display, each step waiting for it to answer while the
/// deadline allows: the class installer's restart (what Device Manager and
/// pnputil do), then a disable and enable of the device node (not kept
/// across a restart of the PC), then, with the driver package beside
/// rdpio.exe, the driver installed again as Install does (the step known to
/// bring it back once Windows gave up on it). Ok: the step that worked.
fn restart_device(deadline: Instant) -> Result<&'static str, RestartError> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{DICS_FLAG_CONFIGSPECIFIC, DICS_PROPCHANGE};
    let inf = driver_inf();
    let steps: &[&'static str] = if inf.is_some() { &["restart", "disable and enable", "driver reinstall"] } else { &["restart", "disable and enable"] };
    let mut last = String::from("no time left to try");
    for (i, &step) in steps.iter().enumerate() {
        let now = Instant::now();
        let left = deadline.saturating_duration_since(now);
        if left.is_zero() {
            break;
        }
        // A device the user disabled meanwhile stays off: the next steps
        // would enable it again.
        let state = health();
        if state == Health::Disabled {
            return Err(RestartError::Failed(format!("{step}: not tried, the device was {state}")));
        }
        // A device the last step got starting gets the rest of the time
        // instead of another restart.
        if i > 0 && state == Health::Starting {
            if wait_running(deadline) {
                return Ok(steps[i - 1]);
            }
            continue;
        }
        let step_deadline = now + left / (steps.len() - i) as u32;
        let done = match i {
            0 => match change_state(HARDWARE_ID, DICS_PROPCHANGE, DICS_FLAG_CONFIGSPECIFIC) {
                Ok(true) => return Err(RestartError::Reboot),
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            },
            1 => cm_restart(HARDWARE_ID),
            _ => inf.as_deref().map_or(Err("no driver package".into()), |inf| update_driver(inf).map_err(|e| e.to_string())),
        };
        match done {
            Err(e) => last = format!("{step}: {e}"),
            Ok(()) => {
                if wait_running(step_deadline) {
                    return Ok(step);
                }
                last = format!("{step}: it didn't come back ({})", health());
            }
        }
    }
    Err(RestartError::Failed(last))
}

/// Whether a restart is for a device in state `h`: one Windows stopped, or
/// one still starting (the caller's grace decides whether it's stuck).
/// Never one the user disabled, or one that runs again.
fn heal_applies(h: Health) -> bool {
    matches!(h, Health::Failed(_) | Health::Starting)
}

/// Restart the device node with `hardware_id` through cfgmgr32: a disable
/// that isn't kept, then an enable.
fn cm_restart(hardware_id: &str) -> Result<(), String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{CM_Disable_DevNode, CM_Enable_DevNode, CM_DISABLE_UI_NOT_OK, CR_SUCCESS};
    let mut result: Result<(), String> = Err("no such device".into());
    for_each_device(hardware_id, |_, data| unsafe {
        // (A refused disable still gets the enable: that restarts a failed
        // device too.)
        let off = CM_Disable_DevNode(data.DevInst, CM_DISABLE_UI_NOT_OK);
        let on = CM_Enable_DevNode(data.DevInst, 0);
        result = if on == CR_SUCCESS { Ok(()) } else { Err(format!("disable {}, enable {}", off.0, on.0)) };
    });
    result
}

impl VirtualDisplay {
    /// The agent's view of the virtual display drivers installed here, ours
    /// first. Puts back what an agent that died mid-stream left, and logs
    /// the state.
    pub fn new() -> Self {
        // Leftover monitors (the driver's watchdog would get them too, a few
        // seconds later).
        if let Ok((link, _)) = Link::open() {
            let _ = link.call::<(), ()>(IOCTL_REMOVE, None, REMOVE_TIMEOUT);
        }
        let mut mtt_leftover = false;
        if device_with_id(MTT_HARDWARE_ID) {
            let settings = mtt_settings_path();
            if settings.exists() {
                // The user's settings.
                mtt_leftover = mtt_undo_leftovers(&settings);
            }
        }
        let mut vd = Self {
            backend: None,
            health: Health::Missing,
            version: None,
            checked: Instant::now(),
            starting_since: None,
            running_since: None,
            backoff: Backoff::default(),
            reboot_due: false,
            logged: None,
        };
        vd.check(true);
        // The open-source driver goes off only when rdpio manages it: an
        // earlier agent left it on mid-stream, or it's the driver a monitor
        // would come from. While ours runs, a monitor the user keeps on it
        // is left alone.
        if mtt_off_at_start(mtt_leftover, matches!(vd.backend, Some(Backend::Mtt { .. }))) {
            mtt_switch_off();
        }
        vd
    }

    /// The rdpio Display device's state, as last checked.
    pub fn state(&self) -> Health {
        self.health
    }

    /// The driver a monitor would be made with now: "rdpio Display", "Virtual
    /// Display Driver" or "none".
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Some(Backend::Ours) => "rdpio Display",
            Some(Backend::Mtt { .. }) => "Virtual Display Driver",
            None => "none",
        }
    }

    /// Check the drivers again, asking ours whether it answers.
    pub fn refresh(&mut self) {
        self.check(true);
    }

    /// [`refresh`](Self::refresh), unless that was done less than `age` ago.
    pub fn refresh_if_older(&mut self, age: Duration) {
        if self.checked.elapsed() >= age {
            self.check(true);
        }
    }

    /// The idle loop's check, every 2 s: cheap while nothing changed (a
    /// running driver isn't opened again while its interface stays).
    pub fn poll(&mut self) {
        if self.checked.elapsed() >= Duration::from_secs(2) {
            self.check(self.health != Health::Running);
        }
    }

    fn check(&mut self, probe: bool) {
        let now = Instant::now();
        self.checked = now;
        let mut health = health();
        if health != Health::Running {
            self.version = None;
        } else if probe {
            self.version = None;
            match Link::open() {
                Ok(_) => {}
                Err(LinkError::Version(v)) => self.version = Some(v),
                // There, but not answering: as good as starting.
                Err(_) => health = Health::Starting,
            }
        }
        self.starting_since = if health == Health::Starting { self.starting_since.or(Some(now)) } else { None };
        self.running_since = if health == Health::Running { self.running_since.or(Some(now)) } else { None };
        if self.running_since.is_some_and(|t| now.duration_since(t) >= RUNNING_STEADY) {
            self.backoff.reset();
            self.reboot_due = false;
        }
        self.health = health;
        let mut fallback_note = "";
        self.backend = if health == Health::Running && self.version.is_none() {
            Some(Backend::Ours)
        } else {
            match mtt_health() {
                Health::Missing | Health::Blocked(_) => None,
                _ => {
                    let settings = mtt_settings_path();
                    if settings.exists() {
                        Some(Backend::Mtt { settings })
                    } else {
                        fallback_note = "Virtual Display Driver without its settings file";
                        None
                    }
                }
            }
        };
        self.log_change(fallback_note);
    }

    /// One line whenever the state or the driver in use changes.
    fn log_change(&mut self, fallback_note: &'static str) {
        let using = if fallback_note.is_empty() { self.backend_name() } else { fallback_note };
        let key = (self.health, using, self.version);
        if self.logged == Some(key) {
            return;
        }
        self.logged = Some(key);
        let (state, problem) = (self.health, self.health.problem());
        if let Some(v) = self.version {
            tracing::warn!(driver = v, ours = INTERFACE_VERSION, using, "the rdpio Display driver is another version; reinstall the host");
            return;
        }
        match state {
            Health::Running | Health::Missing | Health::Disabled => tracing::info!(state = %state, problem, using, "virtual display"),
            Health::Starting => tracing::warn!(state = %state, using, "rdpio Display is installed but its interface isn't there or doesn't answer"),
            Health::Failed(_) | Health::Blocked(_) => tracing::warn!(state = %state, problem, using, "rdpio Display isn't running"),
        }
    }

    /// Whether an offer can count on a virtual monitor: a driver is usable,
    /// or ours is worth restarting right now.
    pub fn usable(&self) -> bool {
        self.backend.is_some() || (self.heal_wanted(Instant::now(), STARTING_GRACE) && !showing())
    }

    fn heal_wanted(&self, now: Instant, starting_grace: Duration) -> bool {
        let stuck = match self.health {
            Health::Failed(_) => true,
            Health::Starting => self.starting_since.is_some_and(|t| now.duration_since(t) >= starting_grace),
            _ => false,
        };
        stuck && !self.reboot_due && self.backoff.due(now)
    }

    /// Restart rdpio Display when Windows stopped it (or it's stuck
    /// starting), taking at most about `budget`: never while a virtual
    /// monitor exists, never a device the user disabled, and backed off
    /// after each attempt. True: it runs now.
    pub fn heal(&mut self, budget: Duration) -> bool {
        self.heal_with(budget, STARTING_GRACE)
    }

    fn heal_with(&mut self, budget: Duration, starting_grace: Duration) -> bool {
        let now = Instant::now();
        if !self.heal_wanted(now, starting_grace) || showing() {
            return false;
        }
        // The state as it is now, not as last checked: a device the user
        // disabled (or that came back) since then is left alone.
        let interface = interface_path().is_some();
        let info = devnode_info(HARDWARE_ID);
        if !heal_applies(classify(info.as_ref().map(|(_, s, p)| (*s, *p)), interface)) {
            self.check(false);
            return false;
        }
        let attempt = self.backoff.attempts + 1;
        let (device, status, problem) = info.unwrap_or_default();
        let status = format!("{status:#010x}");
        tracing::warn!(device = %device, status = %status, problem, attempt, "rdpio Display isn't running; restarting it");
        let r = restart_device(now + budget);
        let next = self.backoff.tried(now);
        self.check(true);
        let ms = now.elapsed().as_millis() as u64;
        let (state, problem) = (self.health, self.health.problem());
        match &r {
            Ok(step) if state == Health::Running => tracing::info!(step = *step, ms, "rdpio Display is back"),
            Err(RestartError::Reboot) => {
                self.reboot_due = true;
                tracing::warn!(ms, "Windows wants this PC restarted to start rdpio Display again; not trying again until then");
            }
            Ok(_) | Err(RestartError::Failed(_)) => {
                let detail = match &r {
                    Err(RestartError::Failed(e)) => e.as_str(),
                    _ => "",
                };
                tracing::warn!(state = %state, problem, detail, ms, next_try_secs = next.as_secs(), "rdpio Display didn't come back");
            }
        }
        self.health == Health::Running
    }

    /// The client's monitor (see `show_inner`), for a sign-in: the drivers
    /// checked right before, a stopped rdpio Display restarted first
    /// (briefly: the client is waiting), and one more try after a restart
    /// when the driver failed the monitor itself.
    pub fn show_healed(&mut self, width: u32, height: u32, hz: u32, render_adapter: Option<LUID>) -> Result<VirtualMonitor, String> {
        self.check(true);
        let mut healed = false;
        if self.backend.is_none() {
            // A device that's only starting gets a moment first.
            if self.health == Health::Starting && wait_running(Instant::now() + STARTING_GRACE) {
                self.check(true);
            }
            if self.backend.is_none() {
                healed = true;
                self.heal(SIGN_IN_HEAL);
            }
        }
        match self.show_inner(width, height, hz, render_adapter) {
            Ok(m) => Ok(m),
            Err((e, true)) if !healed && !matches!(self.backend, Some(Backend::Mtt { .. })) => {
                tracing::warn!(error = %e, "rdpio Display failed the monitor; restarting it and trying once more");
                self.check(true);
                let back = self.health == Health::Running
                    || wait_running(Instant::now() + Duration::from_secs(1))
                    || self.heal_with(SIGN_IN_HEAL, Duration::ZERO);
                self.check(true);
                if back && self.backend.is_some() {
                    self.show_inner(width, height, hz, render_adapter).map_err(|(e, _)| e)
                } else {
                    Err(e)
                }
            }
            Err((e, _)) => Err(e),
        }
    }

    /// A monitor at `width`×`height` and `hz`, made the only active display
    /// and returned with its GDI device name (`\\.\DISPLAYn`) for capture.
    /// The error says whether rdpio Display itself failed (opening it,
    /// asking it, or adding the monitor), which a restart may fix.
    fn show_inner(&self, width: u32, height: u32, hz: u32, render_adapter: Option<LUID>) -> Result<VirtualMonitor, (String, bool)> {
        let (width, height) = even_size(width, height);
        let hz = hz.clamp(24, 500);
        let other = |e: String| (e, false);
        // A loss is about the monitor that's up: a new one starts clean,
        // whichever driver makes it (a stream checks it on any virtual
        // monitor).
        LOST.store(false, Ordering::Relaxed);
        let mut guard = match &self.backend {
            None => return Err((format!("no virtual display driver is running (rdpio Display: {})", self.health), true)),
            Some(Backend::Ours) => {
                let (link, info) = Link::open().map_err(|e| (e.to_string(), true))?;
                // Its adapter starts a moment after the device (a "not ready"
                // from the addition is the same).
                let until = Instant::now() + ADAPTER_WAIT;
                let mut ready = info.adapter_ready != 0;
                if !ready {
                    let waited = Instant::now();
                    while !ready && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(50));
                        ready = link.info().map_err(|e| (format!("the rdpio Display driver didn't answer: {e}"), true))?.adapter_ready != 0;
                    }
                    tracing::info!(ms = waited.elapsed().as_millis() as u64, ready, "waited for the rdpio Display adapter");
                }
                let req = ModeRequest {
                    width,
                    height,
                    refresh_hz: hz,
                    width_mm: 0,
                    height_mm: 0,
                    flags: FLAG_REPLACE,
                    render_adapter: render_adapter.unwrap_or_default(),
                };
                const ERROR_NOT_READY: u32 = 21;
                let added: Added = loop {
                    match link.call(IOCTL_ADD, Some(&req), ADD_TIMEOUT) {
                        Ok(a) => break a,
                        Err(e) if win32_code(&e) == ERROR_NOT_READY && Instant::now() < until => std::thread::sleep(Duration::from_millis(100)),
                        Err(e) => return Err((format!("the virtual display didn't add a monitor: {e}"), true)),
                    }
                };
                let id = added.monitor_id;
                tracing::info!(width, height, hz, monitor = id, "virtual monitor added");
                let weak = Arc::downgrade(&link);
                let pinger = std::thread::Builder::new().name("rdpio-display-ping".into()).spawn(move || ping_loop(weak)).ok().map(|t| t.thread().clone());
                VirtualMonitor::new(Cleanup::Ours { link, id, pinger })
            }
            Some(Backend::Mtt { settings }) => {
                let original = std::fs::read_to_string(settings).map_err(|e| other(format!("can't read {}: {e}", settings.display())))?;
                let state = mtt_health();
                let was_off = state == Health::Disabled;
                // Kept beside the settings until the stream ends: a crashed
                // agent's successor puts them back.
                let backup = settings.with_extension("xml.rdpio-backup");
                std::fs::write(&backup, &original).map_err(|e| other(e.to_string()))?;
                std::fs::write(settings, mtt_settings_with(&original, width, height, hz)).map_err(|e| other(e.to_string()))?;
                let guard = VirtualMonitor::new(Cleanup::Mtt { settings: settings.clone(), disable_after: was_off });
                match state {
                    Health::Disabled => {
                        set_device_enabled(MTT_HARDWARE_ID, true).map_err(|e| other(format!("couldn't turn the Virtual Display Driver on: {e}")))?
                    }
                    // Stopped by Windows: restarted, it reads the new settings
                    // as it starts.
                    Health::Failed(problem) => {
                        tracing::info!(problem, "the Virtual Display Driver had stopped; restarting it");
                        restart_mtt().map_err(|e| other(format!("couldn't restart the Virtual Display Driver: {e}")))?;
                    }
                    _ => {
                        if let Err(e) = mtt_reload() {
                            tracing::info!(error = %e, "the Virtual Display Driver didn't take the new mode; restarting it");
                            restart_mtt().map_err(|r| other(format!("the Virtual Display Driver didn't take the new mode: {e}; restarting it failed: {r}")))?;
                        }
                    }
                }
                tracing::info!(width, height, hz, "Virtual Display Driver monitor asked for");
                guard
            }
        };
        let edid = match self.backend {
            Some(Backend::Ours) => (EDID_MANUFACTURER, EDID_PRODUCT),
            _ => MTT_EDID,
        };
        // Windows sets it up in a moment; then it alone is on. Not waited for
        // long: the client gives the host 15 s to start the stream, and the
        // host's own monitor is the fallback. Waiting is for the monitor to
        // appear; once it's there, a layout Windows refuses is refused again.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (switching_deadline, retry_switching) = (Instant::now() + SWITCHING_WAIT, crate::host_ab::ab("displayretry", true));
        let mut refused_since: Option<Instant> = None;
        loop {
            match activate_only(edid) {
                Ok(name) => {
                    guard.device_name = name;
                    guard.layout_changed = true;
                    break;
                }
                // The desktop switching: Windows takes a layout only from a
                // thread on the input desktop, which this one may no longer
                // be. Follow it and try again in 250 ms steps, past the usual
                // deadline (`displayretry`): the fallback, the host's own
                // monitor streamed mid-switch, ended in "Catastrophic failure"
                // each time it was logged.
                Err(Activation::Switching(e)) if retry_switching => {
                    if Instant::now() > switching_deadline {
                        return Err(other(format!("the virtual monitor didn't come up: {e}")));
                    }
                    rdp_gpu::hyper::attach_input_desktop();
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) if Instant::now() > deadline => return Err(other(format!("the virtual monitor didn't come up: {e}"))),
                // (A monitor that only just arrived can be refused for a
                // moment.)
                Err(Activation::Refused(e)) => {
                    if refused_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(1500) {
                        return Err(other(format!("Windows won't switch to the virtual monitor: {e}")));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(Activation::NotYet(_) | Activation::Switching(_)) => std::thread::sleep(Duration::from_millis(40)),
            }
        }
        // Exactly the client's mode (ours prefers it already; the other one
        // lists it among its modes).
        set_exact_mode(&guard.device_name, width, height, hz);
        // Ours hands its frames over itself: no Desktop Duplication. The
        // capture opens once its output is listed at the client's size (at
        // most the fixed moment the other driver still gets, whose frames
        // come through duplication).
        let settled = if let Cleanup::Ours { id, .. } = &guard.cleanup {
            rdp_gpu::hyper::driver_frames::register_monitor(&guard.device_name, *id);
            wait_listed(&guard.device_name, width, height, SETTLE_WAIT)
        } else {
            std::thread::sleep(SETTLE_WAIT);
            None
        };
        let settled_ms = settled.map(|d| d.as_millis() as u64);
        tracing::info!(device = %guard.device_name, settled_ms, "streaming the virtual monitor (the host's own screens are off meanwhile)");
        Ok(guard)
    }
}

enum Cleanup {
    /// The open driver, the monitor's id, and the thread that pings it.
    Ours { link: Arc<Link>, id: u32, pinger: Option<std::thread::Thread> },
    Mtt { settings: PathBuf, disable_after: bool },
}

/// The virtual monitor while it streams: on drop, the host's own layout
/// comes back (from the display database, which this never wrote) and the
/// monitor goes away.
pub struct VirtualMonitor {
    cleanup: Cleanup,
    pub device_name: String,
    layout_changed: bool,
}

impl VirtualMonitor {
    fn new(cleanup: Cleanup) -> Self {
        SHOWING.fetch_add(1, Ordering::Relaxed);
        Self { cleanup, device_name: String::new(), layout_changed: false }
    }

    /// False once its driver stopped answering: the monitor is gone.
    pub fn alive(&self) -> bool {
        match &self.cleanup {
            Cleanup::Ours { link, .. } => !link.dead.load(Ordering::Relaxed),
            Cleanup::Mtt { .. } => true,
        }
    }
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        use windows::Win32::Devices::Display::{SetDisplayConfig, SDC_APPLY, SDC_USE_DATABASE_CURRENT};
        if self.layout_changed {
            let mut r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
            // ERROR_ACCESS_DENIED: the desktop switching (the console locking
            // as the stream ends), as in `show` — the host's own screens stay
            // dark until this goes through (`displayretry`).
            let until = Instant::now() + SWITCHING_RESTORE_WAIT;
            while r == 5 && Instant::now() < until && crate::host_ab::ab("displayretry", true) {
                rdp_gpu::hyper::attach_input_desktop();
                std::thread::sleep(Duration::from_millis(250));
                r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
            }
            tracing::info!(code = r, "host display layout restored");
        }
        match &self.cleanup {
            Cleanup::Ours { link, pinger, .. } => {
                link.closing.store(true, Ordering::Relaxed);
                if let Some(t) = pinger {
                    t.unpark();
                }
                rdp_gpu::hyper::driver_frames::unregister_monitor(&self.device_name);
                if link.dead.load(Ordering::Relaxed) {
                    tracing::info!("virtual monitor gone with its driver (nothing to remove)");
                } else {
                    let r = link.call::<(), ()>(IOCTL_REMOVE, None, REMOVE_TIMEOUT);
                    let error = r.as_ref().err().map(|e| e.to_string());
                    tracing::info!(ok = r.is_ok(), error, "virtual monitor removed");
                    // The removal found the driver gone or hung.
                    if link.dead.load(Ordering::Relaxed) {
                        link.report_death();
                    }
                }
                // (The handle closes with the last reference: here, or as the
                // ping thread lets go.)
            }
            Cleanup::Mtt { settings, disable_after } => {
                let backup = settings.with_extension("xml.rdpio-backup");
                if let Ok(original) = std::fs::read_to_string(&backup) {
                    let _ = std::fs::write(settings, original);
                    let _ = std::fs::remove_file(&backup);
                }
                let r = if *disable_after { set_device_enabled(MTT_HARDWARE_ID, false) } else { mtt_reload() };
                tracing::info!(ok = r.is_ok(), off = disable_after, "Virtual Display Driver back as it was");
            }
        }
        SHOWING.fetch_sub(1, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------ the open-source driver

/// Its hardware id, and its monitors' EDID manufacturer ("MTT") and product.
const MTT_HARDWARE_ID: &str = r"Root\MttVDD";
const MTT_EDID: (u16, u16) = (0x3694, 0x1337);

/// `vdd_settings.xml`, where the driver reads it from (its registry path, or
/// the default folder).
fn mtt_settings_path() -> PathBuf {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 260];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\MikeTheTech\VirtualDisplayDriver"),
            w!("VDDPATH"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    let dir = if r.is_ok() {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        PathBuf::from(String::from_utf16_lossy(&buf[..n]))
    } else {
        PathBuf::from(r"C:\VirtualDisplayDriver")
    };
    dir.join("vdd_settings.xml")
}

/// The settings with one monitor whose first (preferred) mode is ours.
fn mtt_settings_with(xml: &str, width: u32, height: u32, hz: u32) -> String {
    let mut out = xml.to_string();
    // One monitor.
    if let (Some(a), Some(b)) = (out.find("<count>"), out.find("</count>")) {
        if a < b {
            out.replace_range(a..b + "</count>".len(), "<count>1</count>");
        }
    }
    let entry = format!(
        "\n        <resolution>\n            <width>{width}</width>\n            <height>{height}</height>\n            <refresh_rate>{hz}</refresh_rate>\n        </resolution>"
    );
    if let Some(i) = out.find("<resolutions>") {
        out.insert_str(i + "<resolutions>".len(), &entry);
    } else if let Some(i) = out.rfind("</vdd_settings>") {
        out.insert_str(i, &format!("    <resolutions>{entry}\n    </resolutions>\n"));
    }
    out
}

/// Have the driver read its settings again (its control pipe).
fn mtt_reload() -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
    };
    unsafe {
        let h = CreateFileW(
            w!(r"\\.\pipe\MTTVirtualDisplayPipe"),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| e.to_string())?;
        let cmd: Vec<u8> = "RELOAD_DRIVER".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut wrote = 0u32;
        let r = WriteFile(h, Some(&cmd), Some(&mut wrote), None);
        let _ = CloseHandle(h);
        r.map_err(|e| e.to_string())
    }
}

/// Put back the settings an agent that died mid-stream left. True: there
/// was something left (the settings backup, or an older build's marker).
fn mtt_undo_leftovers(settings: &std::path::Path) -> bool {
    let backup = settings.with_extension("xml.rdpio-backup");
    let mut found = false;
    if let Ok(original) = std::fs::read_to_string(&backup) {
        let _ = std::fs::write(settings, original);
        let _ = std::fs::remove_file(&backup);
        found = true;
        tracing::info!("put the Virtual Display Driver's settings back (left by an earlier agent)");
    }
    // (A marker older builds wrote.)
    if std::fs::remove_file(settings.with_extension("xml.rdpio-enabled")).is_ok() {
        found = true;
    }
    found
}

/// Whether an agent starting up turns the open-source driver off: when an
/// earlier agent left it mid-stream, or when it's the driver rdpio would
/// use (ours isn't running).
fn mtt_off_at_start(leftover: bool, backend_is_mtt: bool) -> bool {
    leftover || backend_is_mtt
}

/// The open-source driver off between streams (else its monitor sits beside
/// the host's own as an empty extra screen).
fn mtt_switch_off() {
    if mtt_enabled() {
        let r = set_device_enabled(MTT_HARDWARE_ID, false);
        tracing::info!(ok = r.is_ok(), "Virtual Display Driver off until a client streams");
    }
}

/// Whether the open-source driver's device is on (failed counts as on: it
/// isn't off).
fn mtt_enabled() -> bool {
    !matches!(mtt_health(), Health::Disabled | Health::Missing)
}

/// Restart the open-source driver (it reads its settings file as it starts).
fn restart_mtt() -> Result<(), String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{DICS_FLAG_CONFIGSPECIFIC, DICS_PROPCHANGE};
    change_state(MTT_HARDWARE_ID, DICS_PROPCHANGE, DICS_FLAG_CONFIGSPECIFIC).map(|_| ())
}

/// Enable or disable every device with `hardware_id`.
fn set_device_enabled(hardware_id: &str, enable: bool) -> Result<(), String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{DICS_DISABLE, DICS_ENABLE, DICS_FLAG_GLOBAL};
    change_state(hardware_id, if enable { DICS_ENABLE } else { DICS_DISABLE }, DICS_FLAG_GLOBAL).map(|_| ())
}

/// A state change (enable, disable, restart) through the class installer,
/// for every device with `hardware_id`. True: Windows wants the PC
/// restarted for it.
fn change_state(
    hardware_id: &str,
    state: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_STATE_CHANGE,
    scope: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_PROPERTY_CHANGE_SCOPE,
) -> Result<bool, String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    let mut result: Result<bool, String> = Err("no such device".into());
    for_each_device(hardware_id, |set, data| unsafe {
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: state,
            Scope: scope,
            HwProfile: 0,
        };
        let device: *const SP_DEVINFO_DATA = data;
        result = SetupDiSetClassInstallParamsW(set, Some(device), Some(&params.ClassInstallHeader), std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32)
            .and_then(|()| SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, Some(device)))
            .map(|()| {
                let mut install = SP_DEVINSTALL_PARAMS_W { cbSize: std::mem::size_of::<SP_DEVINSTALL_PARAMS_W>() as u32, ..Default::default() };
                SetupDiGetDeviceInstallParamsW(set, Some(device), &mut install).is_ok() && (install.Flags.0 & (DI_NEEDRESTART.0 | DI_NEEDREBOOT.0)) != 0
            })
            .map_err(|e| e.to_string());
    });
    result
}

/// The monitor `gdi_name` at exactly this mode (not saved).
fn set_exact_mode(gdi_name: &str, width: u32, height: u32, hz: u32) {
    use windows::Win32::Graphics::Gdi::*;
    let name = HSTRING::from(gdi_name);
    let mut m = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe {
        if EnumDisplaySettingsW(&name, ENUM_CURRENT_SETTINGS, &mut m).as_bool()
            && (m.dmPelsWidth, m.dmPelsHeight, m.dmDisplayFrequency) == (width, height, hz)
        {
            return;
        }
        m.dmPelsWidth = width;
        m.dmPelsHeight = height;
        m.dmDisplayFrequency = hz;
        m.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        let r = ChangeDisplaySettingsExW(&name, Some(&m), None, CDS_TYPE(0), None);
        if r != DISP_CHANGE_SUCCESSFUL {
            tracing::warn!(code = r.0, width, height, hz, "the virtual monitor didn't take the client's exact mode");
        }
    }
}

/// How long a new monitor is given to settle before its capture opens: the
/// most rdpio Display's waits (see [`wait_listed`]), and what the Virtual
/// Display Driver always gets.
const SETTLE_WAIT: Duration = Duration::from_millis(400);

/// Wait until the desktop lists `gdi_name` at `width`×`height` (what the
/// capture opens on), for at most `most`. How long it took, if it did.
fn wait_listed(gdi_name: &str, width: u32, height: u32, most: Duration) -> Option<Duration> {
    let start = Instant::now();
    loop {
        let listed = rdp_gpu::hyper::list_monitors()
            .is_ok_and(|ms| ms.iter().any(|m| m.device_name.eq_ignore_ascii_case(gdi_name) && (m.width, m.height) == (width, height)));
        if listed {
            return Some(start.elapsed());
        }
        let left = most.saturating_sub(start.elapsed());
        if left.is_zero() {
            return None;
        }
        std::thread::sleep(left.min(Duration::from_millis(15)));
    }
}

/// The driver's device interface path, if it's present.
fn interface_path() -> Option<String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
    };
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_Interface_List_SizeW(&mut len, &INTERFACE, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || len <= 1 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(&INTERFACE, PCWSTR::null(), &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (end > 0).then(|| String::from_utf16_lossy(&buf[..end]))
    }
}

/// How long [`VirtualDisplay::show_healed`] waits out a desktop switch (the lock
/// screen, a session change) before the host's own monitor is streamed
/// instead: past the usual 5 s, and well inside the 15 s a client gives the
/// host to start the stream.
const SWITCHING_WAIT: Duration = Duration::from_secs(8);

/// …and how long the host's layout coming back as a stream ends does.
const SWITCHING_RESTORE_WAIT: Duration = Duration::from_secs(2);

/// Why the virtual monitor isn't the active display (yet).
#[derive(Debug)]
enum Activation {
    /// Windows hasn't got it ready: worth waiting for.
    NotYet(String),
    /// Windows refused the layout.
    Refused(String),
    /// The console is switching desktops (the lock screen coming or going,
    /// a session change): worth waiting for, on the input desktop.
    Switching(String),
}

impl std::fmt::Display for Activation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Activation::NotYet(e) | Activation::Refused(e) | Activation::Switching(e) => f.write_str(e),
        }
    }
}

/// Make the monitor with this EDID (manufacturer, product) the only active
/// display, at its preferred mode, without saving that to the display
/// database. Returns its GDI device name.
fn activate_only(edid: (u16, u16)) -> Result<String, Activation> {
    use windows::Win32::Devices::Display::*;
    unsafe {
        let (paths, _) = query(QDC_ALL_PATHS).map_err(Activation::NotYet)?;
        // A path from any free source to our monitor's target.
        let ours = paths
            .iter()
            .find(|p| p.targetInfo.targetAvailable.as_bool() && is_monitor(p.targetInfo.adapterId, p.targetInfo.id, edid))
            .copied()
            .ok_or(Activation::NotYet("no path to the virtual monitor yet".into()))?;
        let mut path = ours;
        // (DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID)
        path.flags = 1;
        path.sourceInfo.Anonymous.modeInfoIdx = u32::MAX;
        path.targetInfo.Anonymous.modeInfoIdx = u32::MAX;
        // The layout as given, with Windows filling in the modes. (Not
        // "a topology from the database": a new monitor has no entry there,
        // and Windows answers that with ERROR_GEN_FAILURE.)
        let mut r = SetDisplayConfig(Some(&[path]), None, SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES);
        if r != 0 {
            let first = r;
            r = SetDisplayConfig(Some(&[path]), None, SDC_APPLY | SDC_TOPOLOGY_SUPPLIED | SDC_ALLOW_PATH_ORDER_CHANGES);
            // ERROR_ACCESS_DENIED: the console is switching desktops (locking,
            // say, as the host's restart does), not a layout Windows refuses.
            if r == 5 && first == 5 {
                return Err(Activation::Switching(format!("SetDisplayConfig {first} / {r} (the desktop is switching)")));
            }
            if r != 0 {
                return Err(Activation::Refused(format!("SetDisplayConfig {first} / {r}")));
            }
        }
        // Its source's name, now that it's active.
        let (active, _) = query(QDC_ONLY_ACTIVE_PATHS).map_err(Activation::NotYet)?;
        let p = active
            .iter()
            .find(|p| is_monitor(p.targetInfo.adapterId, p.targetInfo.id, edid))
            .ok_or(Activation::NotYet("the virtual monitor isn't active".into()))?;
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        name.header.adapterId = p.sourceInfo.adapterId;
        name.header.id = p.sourceInfo.id;
        if DisplayConfigGetDeviceInfo(&mut name.header) != 0 {
            return Err(Activation::NotYet("no source name".into()));
        }
        let n = name.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(32);
        Ok(String::from_utf16_lossy(&name.viewGdiDeviceName[..n]))
    }
}

fn query(
    flags: windows::Win32::Devices::Display::QUERY_DISPLAY_CONFIG_FLAGS,
) -> Result<(Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO>, Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO>), String> {
    use windows::Win32::Devices::Display::*;
    unsafe {
        for _ in 0..4 {
            let (mut np, mut nm) = (0u32, 0u32);
            if GetDisplayConfigBufferSizes(flags, &mut np, &mut nm).0 != 0 {
                return Err("GetDisplayConfigBufferSizes".into());
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            let r = QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None);
            if r.0 == 122 {
                continue; // ERROR_INSUFFICIENT_BUFFER: the layout changed meanwhile
            }
            if r.0 != 0 {
                return Err(format!("QueryDisplayConfig {}", r.0));
            }
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Ok((paths, modes));
        }
        Err("the display layout kept changing".into())
    }
}

/// Whether a display target is a monitor with these EDID ids.
fn is_monitor(adapter: LUID, id: u32, edid: (u16, u16)) -> bool {
    use windows::Win32::Devices::Display::*;
    let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    name.header.adapterId = adapter;
    name.header.id = id;
    // Windows reports the manufacturer id with its two bytes swapped against
    // the EDID's big-endian order (measured: an MSI monitor's "CND", 0x0DC4 in
    // its EDID, comes back as 0xC40D); the product code as stored.
    unsafe { DisplayConfigGetDeviceInfo(&mut name.header) == 0 && (name.edidManufactureId.swap_bytes(), name.edidProductCodeId) == edid }
}

// ------------------------------------------------------------ display scale

/// Windows' scale steps, in percent.
const SCALE_STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// The step nearest `percent`.
fn nearest_step(percent: u32) -> usize {
    SCALE_STEPS.iter().enumerate().min_by_key(|(_, s)| s.abs_diff(percent)).map_or(0, |(i, _)| i)
}

/// Give the monitor `gdi_name` the client's display scale (text the same
/// size as on the client's own desktop). The setting is the signed-in
/// user's: from the SYSTEM agent through a helper run as them.
pub fn set_scale(gdi_name: &str, percent: u32) {
    if percent == 0 {
        return;
    }
    let session = unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    let ok = match crate::hyper_direct::run_as_user(session, &format!("--hyper-display-scale {gdi_name} {percent}")) {
        Ok(Some(0)) => true,
        Ok(_) => false,
        Err(_) => set_scale_here(gdi_name, percent),
    };
    let percent = SCALE_STEPS[nearest_step(percent)];
    if ok {
        tracing::info!(percent, ok, "virtual monitor scale set to the client's");
    } else {
        tracing::warn!(percent, device = gdi_name, "the virtual monitor didn't take the client's scale; text shows at Windows' own scale for it");
    }
}

/// `--hyper-display-scale <\\.\DISPLAYn> <percent>`, in the user's session.
/// Through DisplayConfigSetDeviceInfo's (undocumented, long-stable) source
/// DPI scale packets, which count in steps from the recommended scale.
pub fn set_scale_here(gdi_name: &str, percent: u32) -> bool {
    use windows::Win32::Devices::Display::*;
    #[repr(C)]
    struct ScaleGet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        min_rel: i32,
        cur_rel: i32,
        max_rel: i32,
    }
    #[repr(C)]
    struct ScaleSet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        rel: i32,
    }
    let Ok((paths, _)) = query(QDC_ONLY_ACTIVE_PATHS) else { return false };
    for p in paths {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        name.header.adapterId = p.sourceInfo.adapterId;
        name.header.id = p.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } != 0 {
            continue;
        }
        let n = name.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(32);
        if !String::from_utf16_lossy(&name.viewGdiDeviceName[..n]).eq_ignore_ascii_case(gdi_name) {
            continue;
        }
        let header = |kind: i32, size: usize| DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(kind),
            size: size as u32,
            adapterId: p.sourceInfo.adapterId,
            id: p.sourceInfo.id,
        };
        let mut get = ScaleGet { header: header(-3, std::mem::size_of::<ScaleGet>()), min_rel: 0, cur_rel: 0, max_rel: 0 };
        if unsafe { DisplayConfigGetDeviceInfo(&mut get.header) } != 0 {
            return false;
        }
        // The recommended scale is `-min_rel` steps up from 100 %.
        let recommended = get.min_rel.unsigned_abs() as i32;
        let rel = (nearest_step(percent) as i32 - recommended).clamp(get.min_rel, get.max_rel);
        let set = ScaleSet { header: header(-4, std::mem::size_of::<ScaleSet>()), rel };
        return unsafe { DisplayConfigSetDeviceInfo(&set.header) } == 0;
    }
    false
}

// ------------------------------------------------------------ installing

/// `--hyper-display-install` / `--hyper-display-uninstall` (elevated): the
/// driver package from the `rdpio-display` folder beside rdpio.exe, and its
/// device.
pub fn install(uninstall: bool) -> Result<String, Box<dyn std::error::Error>> {
    if uninstall {
        return remove();
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or("no folder")?.join("rdpio-display");
    let inf = dir.join("rdpio-display.inf");
    if !inf.exists() {
        return Err(format!("the driver isn't next to rdpio.exe ({} is missing)", inf.display()).into());
    }
    // A test-signed build (development): its certificate must be trusted,
    // and Windows must allow test-signed drivers.
    let cer = dir.join("rdpio-test.cer");
    let mut note = String::new();
    if cer.exists() {
        for store in ["Root", "TrustedPublisher"] {
            let out = std::process::Command::new(crate::hyper_direct::system_tool("certutil.exe"))
                .args(["-f", "-addstore", store])
                .arg(&cer)
                .output()?;
            if !out.status.success() {
                return Err(format!("could not trust the test certificate: {}", String::from_utf8_lossy(&out.stdout).trim()).into());
            }
        }
        if !test_signing_on() {
            note = "This is a test-signed driver: Windows loads it only with test signing on (`bcdedit /set testsigning on`, then restart).".into();
        }
    }
    create_device()?;
    update_driver(&inf)?;
    let ready = (0..20).any(|_| {
        std::thread::sleep(Duration::from_millis(250));
        interface_path().is_some()
    });
    Ok(if ready {
        "The rdpio Display driver is installed: streams now get a display at the client's own resolution.".to_string()
    } else if !note.is_empty() {
        note
    } else {
        "The rdpio Display driver is installed but not running yet (a restart may be needed).".to_string()
    })
}

/// `--hyper-display-repair` (elevated, the launcher's Repair): rdpio Display
/// started again, whatever stopped it: switched back on when it was disabled
/// in Device Manager, else restarted (and installed again when the driver
/// package is beside rdpio.exe). The error says what's left and what to do.
pub fn repair() -> Result<String, Box<dyn std::error::Error>> {
    const RUNNING: &str = "rdpio Display is running: streams get a display at the client's own resolution.";
    match health() {
        Health::Running => return Ok(RUNNING.into()),
        Health::Missing => return Err("rdpio Display isn't installed here: install it from This PC.".into()),
        Health::Blocked(52) => {
            return Err("Windows won't load rdpio Display (code 52): this test-signed driver needs test signing on (`bcdedit /set testsigning on`) and a restart of this PC.".into())
        }
        Health::Blocked(14) => return Err("Windows needs this PC restarted before it loads rdpio Display (code 14).".into()),
        Health::Disabled => {
            set_device_enabled(HARDWARE_ID, true).map_err(|e| format!("couldn't switch rdpio Display back on (code 22): {e}"))?;
            if wait_running(Instant::now() + Duration::from_secs(5)) {
                return Ok(RUNNING.into());
            }
        }
        Health::Starting | Health::Failed(_) | Health::Blocked(_) => {}
    }
    match restart_device(Instant::now() + REPAIR_BUDGET) {
        Ok(_) => Ok(RUNNING.into()),
        Err(RestartError::Reboot) => Err("Windows needs this PC restarted to start rdpio Display again (code 14).".into()),
        Err(RestartError::Failed(e)) => Err(format!(
            "rdpio Display still isn't running: {} ({e}). Restart this PC, or remove rdpio Display and install it again from This PC.",
            health()
        )
        .into()),
    }
}

fn test_signing_on() -> bool {
    std::process::Command::new(crate::hyper_direct::system_tool("bcdedit.exe"))
        .args(["/enum", "{current}"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_ascii_lowercase();
            s.lines().any(|l| l.trim_start().starts_with("testsigning") && l.contains("yes"))
        })
        .unwrap_or(false)
}

/// Our root device, unless there is one already.
fn create_device() -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    if device_present() {
        return Ok(());
    }
    // GUID_DEVCLASS_DISPLAY
    let class = GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);
    unsafe {
        let set = SetupDiCreateDeviceInfoList(Some(&class), None)?;
        let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        let result = (|| -> windows::core::Result<()> {
            SetupDiCreateDeviceInfoW(set, &HSTRING::from("Display"), &class, &HSTRING::from("rdpio Display"), None, DICD_GENERATE_ID, Some(&mut data))?;
            // The hardware id, as a double-NUL-terminated multi-string.
            let ids: Vec<u16> = HARDWARE_ID.encode_utf16().chain([0, 0]).collect();
            let bytes = std::slice::from_raw_parts(ids.as_ptr() as *const u8, ids.len() * 2);
            SetupDiSetDeviceRegistryPropertyW(set, &mut data, SPDRP_HARDWAREID, Some(bytes))?;
            SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&data))?;
            Ok(())
        })();
        let _ = SetupDiDestroyDeviceInfoList(set);
        result?;
    }
    Ok(())
}

fn update_driver(inf: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{UpdateDriverForPlugAndPlayDevicesW, INSTALLFLAG_FORCE};
    let mut reboot = windows::core::BOOL::default();
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(None, &HSTRING::from(HARDWARE_ID), &HSTRING::from(inf.as_os_str()), INSTALLFLAG_FORCE, Some(&mut reboot))
            .map_err(|e| format!("Windows didn't install the driver: {e} (a test-signed driver needs test signing on)"))?;
    }
    Ok(())
}

/// Display devices with `hardware_id` (enabled or not).
fn for_each_device(hardware_id: &str, mut f: impl FnMut(windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO, &mut windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA)) {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    let class = GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(Some(&class), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_ALLCLASSES) else { return };
        let mut i = 0;
        loop {
            let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set, i, &mut data).is_err() {
                break;
            }
            i += 1;
            let mut buf = [0u8; 512];
            let mut size = 0u32;
            if SetupDiGetDeviceRegistryPropertyW(set, &data, SPDRP_HARDWAREID, None, Some(&mut buf), Some(&mut size)).is_ok() {
                let wide: Vec<u16> = buf[..size as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let ids = String::from_utf16_lossy(&wide);
                if ids.split('\0').any(|id| id.eq_ignore_ascii_case(hardware_id)) {
                    f(set, &mut data);
                }
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
}

fn device_present() -> bool {
    device_with_id(HARDWARE_ID)
}

fn device_with_id(hardware_id: &str) -> bool {
    let mut found = false;
    for_each_device(hardware_id, |_, _| found = true);
    found
}

fn remove() -> Result<String, Box<dyn std::error::Error>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{SetupDiCallClassInstaller, DIF_REMOVE};
    let mut removed = 0;
    for_each_device(HARDWARE_ID, |set, data| unsafe {
        if SetupDiCallClassInstaller(DIF_REMOVE, set, Some(data)).is_ok() {
            removed += 1;
        }
    });
    // And the package out of the driver store.
    let pnputil = crate::hyper_direct::system_tool("pnputil.exe");
    if let Ok(out) = std::process::Command::new(&pnputil).args(["/enum-drivers"]).output() {
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        for published in published_names(&text, "rdpio-display.inf") {
            let _ = std::process::Command::new(&pnputil).args(["/delete-driver", &published, "/uninstall", "/force"]).output();
        }
    }
    Ok(if removed > 0 { "The rdpio Display driver is removed.".into() } else { "The rdpio Display driver wasn't installed.".into() })
}

/// The `oemNN.inf` names pnputil lists for a package whose original INF is
/// `original` (pnputil's output: blocks of "Name: value" lines).
fn published_names(pnputil: &str, original: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut published: Option<String> = None;
    for line in pnputil.lines() {
        let Some((k, v)) = line.split_once(':') else {
            published = None;
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k.starts_with("published name") {
            published = Some(v.to_string());
        } else if k.starts_with("original name") && v.eq_ignore_ascii_case(original) {
            if let Some(p) = published.take() {
                out.push(p);
            }
        }
    }
    out
}

/// Whether the open-source Virtual Display Driver is installed (the
/// fallback when ours isn't running).
pub fn fallback_installed() -> bool {
    device_with_id(MTT_HARDWARE_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_is_only_for_a_stopped_or_starting_device() {
        assert!(heal_applies(Health::Failed(43)));
        assert!(heal_applies(Health::Starting));
        assert!(!heal_applies(Health::Disabled));
        assert!(!heal_applies(Health::Running));
        assert!(!heal_applies(Health::Missing));
        assert!(!heal_applies(Health::Blocked(52)));
        // A device the user disabled since the last check reads as disabled
        // now, whatever was cached.
        assert!(!heal_applies(classify(Some((0, PROBLEM_DISABLED)), false)));
        assert!(heal_applies(classify(Some((0, 43)), false)));
    }

    #[test]
    fn the_open_source_driver_stays_on_while_ours_runs() {
        assert!(!mtt_off_at_start(false, false));
        assert!(mtt_off_at_start(true, false));
        assert!(mtt_off_at_start(false, true));
    }

    #[test]
    fn leftovers_are_found_and_put_back() {
        let dir = std::env::temp_dir().join(format!("rdpio-vd-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let settings = dir.join("vdd_settings.xml");
        std::fs::write(&settings, "streamed").unwrap();
        assert!(!mtt_undo_leftovers(&settings));
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "streamed");
        std::fs::write(settings.with_extension("xml.rdpio-backup"), "user's").unwrap();
        assert!(mtt_undo_leftovers(&settings));
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "user's");
        assert!(!settings.with_extension("xml.rdpio-backup").exists());
        std::fs::write(settings.with_extension("xml.rdpio-enabled"), "").unwrap();
        assert!(mtt_undo_leftovers(&settings));
        assert!(!mtt_undo_leftovers(&settings));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wire_structs_match_the_driver() {
        // Public.h, #pragma pack(4).
        assert_eq!(std::mem::size_of::<Info>(), 16);
        assert_eq!(std::mem::size_of::<ModeRequest>(), 32);
        assert_eq!(std::mem::size_of::<Added>(), 8);
        assert_eq!(IOCTL_INFO, 0x0022_2400);
        assert_eq!(IOCTL_PING, 0x0022_240C);
        // "RDO" in EDID's 5-bit letters.
        let pack = |s: &[u8; 3]| ((s[0] - b'@') as u16) << 10 | ((s[1] - b'@') as u16) << 5 | (s[2] - b'@') as u16;
        assert_eq!(pack(b"RDO"), EDID_MANUFACTURER);
    }

    #[test]
    fn the_flight_recorder_matches_the_driver() {
        // Public.h, #pragma pack(8).
        assert_eq!(std::mem::size_of::<TraceCall>(), 16);
        assert_eq!(std::mem::size_of::<TraceEvent>(), 16);
        assert_eq!(std::mem::offset_of!(TraceView, in_call), 16);
        assert_eq!(std::mem::offset_of!(TraceView, ring), 16 + 16 * TRACE_SLOTS);
        assert_eq!(std::mem::size_of::<TraceView>(), 16 + 16 * TRACE_SLOTS + 16 * TRACE_RING);
    }

    #[test]
    fn the_flight_recorder_reads_as_a_log_line() {
        let mut t = TraceView {
            version: TRACE_VERSION,
            boot: 2,
            next: 0,
            process_id: 1,
            in_call: [TraceCall::default(); TRACE_SLOTS],
            ring: [TraceEvent::default(); TRACE_RING],
        };
        assert_eq!(trace_in_calls(&t, 1000), "no driver thread was inside a call");
        assert_eq!(trace_events(&t, 1000), "none");

        t.in_call[1] = TraceCall { code: 9, arg: 3, start_ms: 1000 };
        t.in_call[2] = TraceCall { code: 24, arg: 3, start_ms: 990 };
        assert_eq!(
            trace_in_calls(&t, 11_012),
            "swap-chain callback=UnassignSwapChain(3) for 10012 ms; swap-chain thread=FinishedProcessingFrame(3) for 10022 ms"
        );

        // Oldest first, failures in hex; once the ring wrapped, only its last
        // RDPIO_TRACE_RING entries.
        t.ring[0] = TraceEvent { tick_ms: 900, code: 1, arg: 2 };
        t.ring[1] = TraceEvent { tick_ms: 950, code: 29, arg: 0x887A_0005 };
        t.next = 2;
        assert_eq!(trace_events(&t, 1000), "HostStart 2 (100 ms ago), DeviceRemoved 0x887a0005 (50 ms ago)");
        for n in 0..TRACE_RING + 3 {
            t.ring[n % TRACE_RING] = TraceEvent { tick_ms: n as u64, code: 7, arg: n as u32 };
        }
        t.next = (TRACE_RING + 3) as i32;
        let events = trace_events(&t, 100);
        assert_eq!(events.split(", ").count(), TRACE_RING);
        assert!(events.starts_with("IoctlPing 3 "), "{events}");
        assert!(events.ends_with(&format!("IoctlPing {} ({} ms ago)", TRACE_RING + 2, 100 - (TRACE_RING + 2))), "{events}");
        assert_eq!(trace_code(1234), "?");
    }

    /// What Windows reports as each display target's EDID ids (to check their
    /// byte order against the EDID's). `cargo test -p rdp-client --bin rdpio edid_ids_as_windows -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn edid_ids_as_windows_reports_them() {
        use windows::Win32::Devices::Display::*;
        let (paths, _) = query(QDC_ALL_PATHS).unwrap();
        let mut seen = std::collections::HashSet::new();
        for p in paths {
            if !seen.insert((p.targetInfo.adapterId.LowPart, p.targetInfo.id)) {
                continue;
            }
            let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            name.header.adapterId = p.targetInfo.adapterId;
            name.header.id = p.targetInfo.id;
            if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } == 0 {
                let n = name.monitorFriendlyDeviceName.iter().position(|&c| c == 0).unwrap_or(0);
                println!(
                    "{:<24} mfr {:#06x} product {:#06x} available {}",
                    String::from_utf16_lossy(&name.monitorFriendlyDeviceName[..n]),
                    name.edidManufactureId,
                    name.edidProductCodeId,
                    p.targetInfo.targetAvailable.as_bool()
                );
            }
        }
    }

    /// The installed driver's monitor made the only display for a few
    /// seconds, then the host's layout back (the screens go dark meanwhile;
    /// elevated, as the driver opens only for admins and the service).
    /// `cargo test -p rdp-client --bin rdpio a_virtual_monitor_comes_up -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_virtual_monitor_comes_up() {
        let mut vd = VirtualDisplay::new();
        assert!(vd.usable(), "no virtual display driver ({})", vd.state());
        let t = Instant::now();
        let shown = vd.show_healed(1920, 1080, 120, None);
        println!("show: {:?} after {:?}", shown.as_ref().map(|m| m.device_name.clone()), t.elapsed());
        std::thread::sleep(Duration::from_secs(2));
        drop(shown);
    }

    #[test]
    fn the_open_source_drivers_settings_get_our_mode_first() {
        let xml = "<vdd_settings>\n    <monitors>\n        <count>3</count>\n    </monitors>\n    <resolutions>\n        <resolution>\n            <width>1920</width>\n            <height>1080</height>\n            <refresh_rate>60</refresh_rate>\n        </resolution>\n    </resolutions>\n</vdd_settings>";
        let out = mtt_settings_with(xml, 2256, 1504, 60);
        assert!(out.contains("<count>1</count>") && !out.contains("<count>3</count>"));
        let ours = out.find("<width>2256</width>").unwrap();
        assert!(ours < out.find("<width>1920</width>").unwrap(), "ours isn't first: {out}");
        assert!(out.contains("<height>1504</height>"));
        // A file without a resolution list gets one.
        let bare = mtt_settings_with("<vdd_settings>\n</vdd_settings>", 1280, 800, 120);
        assert!(bare.contains("<resolutions>") && bare.contains("<refresh_rate>120</refresh_rate>") && bare.trim_end().ends_with("</vdd_settings>"));
    }

    #[test]
    fn scales_round_to_windows_steps() {
        assert_eq!(SCALE_STEPS[nearest_step(150)], 150);
        assert_eq!(SCALE_STEPS[nearest_step(144)], 150);
        assert_eq!(SCALE_STEPS[nearest_step(96)], 100);
        assert_eq!(SCALE_STEPS[nearest_step(201)], 200);
        assert_eq!(SCALE_STEPS[nearest_step(900)], 500);
    }

    #[test]
    fn device_states_from_windows_problem_codes() {
        const STARTED: u32 = 0x0180_200A;
        // The interface decides "running", whatever the node says.
        assert_eq!(classify(Some((STARTED, 0)), true), Health::Running);
        assert_eq!(classify(None, true), Health::Running);
        assert_eq!(classify(None, false), Health::Missing);
        // There, no problem, no interface yet: starting.
        assert_eq!(classify(Some((STARTED, 0)), false), Health::Starting);
        // What this host had after the driver host was killed.
        assert_eq!(classify(Some((0x0180_2401, 43)), false), Health::Failed(43));
        assert_eq!(classify(Some((0, 10)), false), Health::Failed(10));
        assert_eq!(classify(Some((0, 22)), false), Health::Disabled);
        for p in [52, 14, 28, 1, 18, 32, 48] {
            assert_eq!(classify(Some((0, p)), false), Health::Blocked(p));
        }
        assert_eq!(Health::Failed(43).problem(), Some(43));
        assert_eq!(Health::Disabled.problem(), Some(22));
        assert_eq!(Health::Starting.problem(), None);
        let names: Vec<_> = [Health::Running, Health::Starting, Health::Failed(43), Health::Disabled, Health::Blocked(52), Health::Missing]
            .iter()
            .map(|h| h.name())
            .collect();
        assert_eq!(names, ["running", "installed", "failed", "disabled", "blocked", "missing"]);
    }

    #[test]
    fn the_open_source_driver_is_on_off_or_failed() {
        assert_eq!(mtt_classify(None), Health::Missing);
        assert_eq!(mtt_classify(Some((0, 0))), Health::Running);
        // Off between streams is how rdpio keeps it: usable.
        assert_eq!(mtt_classify(Some((0, 22))), Health::Disabled);
        assert_eq!(mtt_classify(Some((0, 43))), Health::Failed(43));
        assert_eq!(mtt_classify(Some((0, 52))), Health::Blocked(52));
    }

    #[test]
    fn restarts_back_off() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        assert!(b.due(t0));
        assert_eq!(b.tried(t0), Duration::from_secs(10));
        assert!(!b.due(t0 + Duration::from_secs(9)));
        assert!(b.due(t0 + Duration::from_secs(10)));
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(b.tried(t1), Duration::from_secs(60));
        assert_eq!(b.tried(t1), Duration::from_secs(300));
        assert_eq!(b.tried(t1), Duration::from_secs(1800));
        assert_eq!(b.tried(t1), Duration::from_secs(1800));
        assert!(!b.due(t1 + Duration::from_secs(1799)));
        b.reset();
        assert!(b.due(t1));
        assert_eq!(b.attempts, 0);
    }

    #[test]
    fn a_dead_driver_is_told_from_a_refusal() {
        use windows::core::HRESULT;
        let err = |code: u32| windows::core::Error::from(HRESULT::from_win32(code));
        // "The handle is invalid", as logged after the driver died.
        assert!(gone(win32_code(&err(6))));
        assert!(gone(win32_code(&err(1617))));
        assert!(gone(win32_code(&err(258))));
        assert!(timed_out(&err(258)));
        // Not ready (the adapter starting) and a bad request are not.
        assert!(!gone(win32_code(&err(21))));
        assert!(!gone(win32_code(&err(87))));
        assert!(!timed_out(&err(21)));
    }

    #[test]
    fn pnputil_listing_finds_our_package() {
        let listing = "Microsoft PnP Utility\r\n\r\nPublished Name:     oem12.inf\r\nOriginal Name:      nvlddmkm.inf\r\nProvider Name:      NVIDIA\r\n\r\nPublished Name:     oem41.inf\r\nOriginal Name:      rdpio-display.inf\r\nProvider Name:      rdpio\r\n";
        assert_eq!(published_names(listing, "rdpio-display.inf"), ["oem41.inf"]);
        assert!(published_names(listing, "other.inf").is_empty());
    }
}
