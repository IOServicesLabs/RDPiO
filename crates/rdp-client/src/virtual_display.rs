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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

/// A driver the agent makes the client's monitor with: ours (rdpio
/// Display), or else the signed open-source Virtual Display Driver
/// (github.com/VirtualDrivers/Virtual-Display-Driver), which works without
/// test signing.
pub struct VirtualDisplay {
    backend: Backend,
}

enum Backend {
    Ours { handle: Arc<Handle>, stop: Arc<AtomicBool> },
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

impl Handle {
    fn ioctl<I: Copy, O: Copy + Default>(&self, code: u32, input: Option<&I>) -> windows::core::Result<O> {
        use windows::Win32::System::IO::DeviceIoControl;
        let mut out = O::default();
        let mut got = 0u32;
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                input.map(|i| i as *const I as *const _),
                input.map_or(0, |_| std::mem::size_of::<I>() as u32),
                (std::mem::size_of::<O>() > 0).then_some(&mut out as *mut O as *mut _),
                std::mem::size_of::<O>() as u32,
                Some(&mut got),
                None,
            )?;
        }
        Ok(out)
    }
}

impl VirtualDisplay {
    /// A virtual display driver that's installed here, ours first.
    pub fn open() -> Option<Self> {
        Self::open_ours().or_else(Self::open_mtt)
    }

    fn open_ours() -> Option<Self> {
        let path = interface_path()?;
        let handle = unsafe {
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                OPEN_EXISTING,
            };
            CreateFileW(
                &HSTRING::from(path.as_str()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        let handle = match handle {
            Ok(h) => Arc::new(Handle(h)),
            Err(e) => {
                tracing::warn!(error = %e, "the rdpio Display driver is there but can't be opened");
                return None;
            }
        };
        let info: Info = match handle.ioctl::<(), Info>(IOCTL_INFO, None) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "the rdpio Display driver didn't answer");
                return None;
            }
        };
        let version = info.interface_version;
        if version != INTERFACE_VERSION {
            tracing::warn!(driver = version, ours = INTERFACE_VERSION, "the rdpio Display driver is another version; reinstall the host");
            return None;
        }
        // Leftovers from an agent that died mid-stream (the watchdog would
        // get them too, a few seconds later).
        let _ = handle.ioctl::<(), ()>(IOCTL_REMOVE, None);
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (handle, stop) = (handle.clone(), stop.clone());
            std::thread::Builder::new()
                .name("rdpio-display-ping".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = handle.ioctl::<(), ()>(IOCTL_PING, None);
                        std::thread::sleep(Duration::from_secs(2));
                    }
                })
                .ok()?;
        }
        tracing::info!("rdpio Display driver ready");
        Some(Self { backend: Backend::Ours { handle, stop } })
    }

    fn open_mtt() -> Option<Self> {
        if !device_with_id(MTT_HARDWARE_ID) {
            return None;
        }
        let settings = mtt_settings_path();
        if !settings.exists() {
            tracing::warn!(path = %settings.display(), "the Virtual Display Driver is installed but its settings file isn't there");
            return None;
        }
        // What a crashed agent left: the user's settings, and the device off
        // if it was.
        mtt_undo_leftovers(&settings);
        tracing::info!("using the Virtual Display Driver for client-sized displays");
        Some(Self { backend: Backend::Mtt { settings } })
    }

    /// A monitor at `width`×`height` and `hz`, made the only active display
    /// and returned with its GDI device name (`\\.\DISPLAYn`) for capture.
    pub fn show(&self, width: u32, height: u32, hz: u32, render_adapter: Option<LUID>) -> Result<VirtualMonitor, String> {
        let (width, height) = even_size(width, height);
        let hz = hz.clamp(24, 500);
        let mut guard = match &self.backend {
            Backend::Ours { handle, .. } => {
                let req = ModeRequest {
                    width,
                    height,
                    refresh_hz: hz,
                    width_mm: 0,
                    height_mm: 0,
                    flags: FLAG_REPLACE,
                    render_adapter: render_adapter.unwrap_or_default(),
                };
                let added: Added =
                    handle.ioctl(IOCTL_ADD, Some(&req)).map_err(|e| format!("the virtual display didn't add a monitor: {e}"))?;
                let id = added.monitor_id;
                tracing::info!(width, height, hz, monitor = id, "virtual monitor added");
                VirtualMonitor { cleanup: Cleanup::Ours(handle.clone(), id), device_name: String::new(), layout_changed: false }
            }
            Backend::Mtt { settings } => {
                let original = std::fs::read_to_string(settings).map_err(|e| format!("can't read {}: {e}", settings.display()))?;
                let enabled = mtt_enabled();
                // Kept beside the settings until the stream ends: a crashed
                // agent's successor puts them back.
                let backup = settings.with_extension("xml.rdpio-backup");
                std::fs::write(&backup, &original).map_err(|e| e.to_string())?;
                if !enabled {
                    let _ = std::fs::write(settings.with_extension("xml.rdpio-enabled"), b"");
                }
                std::fs::write(settings, mtt_settings_with(&original, width, height, hz)).map_err(|e| e.to_string())?;
                let guard = VirtualMonitor {
                    cleanup: Cleanup::Mtt { settings: settings.clone(), disable_after: !enabled },
                    device_name: String::new(),
                    layout_changed: false,
                };
                if enabled {
                    mtt_reload().map_err(|e| format!("the Virtual Display Driver didn't take the new mode: {e}"))?;
                } else {
                    set_device_enabled(MTT_HARDWARE_ID, true).map_err(|e| format!("couldn't turn the Virtual Display Driver on: {e}"))?;
                }
                tracing::info!(width, height, hz, "Virtual Display Driver monitor asked for");
                guard
            }
        };
        let edid = match self.backend {
            Backend::Ours { .. } => (EDID_MANUFACTURER, EDID_PRODUCT),
            Backend::Mtt { .. } => MTT_EDID,
        };
        // Windows sets it up in a moment; then it alone is on. Not waited for
        // long: the client gives the host 15 s to start the stream, and the
        // host's own monitor is the fallback. Waiting is for the monitor to
        // appear; once it's there, a layout Windows refuses is refused again.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut refused_since: Option<Instant> = None;
        loop {
            match activate_only(edid) {
                Ok(name) => {
                    guard.device_name = name;
                    guard.layout_changed = true;
                    break;
                }
                Err(e) if Instant::now() > deadline => return Err(format!("the virtual monitor didn't come up: {e}")),
                // (A monitor that only just arrived can be refused for a
                // moment.)
                Err(Activation::Refused(e)) => {
                    if refused_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(1500) {
                        return Err(format!("Windows won't switch to the virtual monitor: {e}"));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(Activation::NotYet(_)) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
        // Exactly the client's mode (ours prefers it already; the other one
        // lists it among its modes).
        set_exact_mode(&guard.device_name, width, height, hz);
        // Ours hands its frames over itself: no Desktop Duplication.
        if let Cleanup::Ours(_, id) = guard.cleanup {
            rdp_gpu::hyper::driver_frames::register_monitor(&guard.device_name, id);
        }
        // The desktop moves onto it; give duplication a moment.
        std::thread::sleep(Duration::from_millis(400));
        tracing::info!(device = %guard.device_name, "streaming the virtual monitor (the host's own screens are off meanwhile)");
        Ok(guard)
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if let Backend::Ours { handle, stop } = &self.backend {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.ioctl::<(), ()>(IOCTL_REMOVE, None);
        }
    }
}

enum Cleanup {
    /// With the driver's monitor id.
    Ours(Arc<Handle>, u32),
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

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        use windows::Win32::Devices::Display::{SetDisplayConfig, SDC_APPLY, SDC_USE_DATABASE_CURRENT};
        if self.layout_changed {
            let r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
            tracing::info!(code = r, "host display layout restored");
        }
        match &self.cleanup {
            Cleanup::Ours(handle, _) => {
                rdp_gpu::hyper::driver_frames::unregister_monitor(&self.device_name);
                let r = handle.ioctl::<(), ()>(IOCTL_REMOVE, None);
                tracing::info!(ok = r.is_ok(), "virtual monitor removed");
            }
            Cleanup::Mtt { settings, disable_after } => {
                let backup = settings.with_extension("xml.rdpio-backup");
                if let Ok(original) = std::fs::read_to_string(&backup) {
                    let _ = std::fs::write(settings, original);
                    let _ = std::fs::remove_file(&backup);
                }
                let _ = std::fs::remove_file(settings.with_extension("xml.rdpio-enabled"));
                let r = if *disable_after { set_device_enabled(MTT_HARDWARE_ID, false) } else { mtt_reload() };
                tracing::info!(ok = r.is_ok(), off = disable_after, "Virtual Display Driver back as it was");
            }
        }
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

/// Put back what an agent that died mid-stream left.
fn mtt_undo_leftovers(settings: &std::path::Path) {
    let backup = settings.with_extension("xml.rdpio-backup");
    if let Ok(original) = std::fs::read_to_string(&backup) {
        let _ = std::fs::write(settings, original);
        let _ = std::fs::remove_file(&backup);
        tracing::info!("put the Virtual Display Driver's settings back (left by an earlier agent)");
    }
    let _ = std::fs::remove_file(settings.with_extension("xml.rdpio-enabled"));
    // Installed for rdpio, it's managed by rdpio: off between streams (else
    // its monitor sits beside the host's own as an empty extra screen).
    if mtt_enabled() {
        let r = set_device_enabled(MTT_HARDWARE_ID, false);
        tracing::info!(ok = r.is_ok(), "Virtual Display Driver off until a client streams");
    }
}

fn mtt_enabled() -> bool {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{CM_Get_DevNode_Status, CM_DEVNODE_STATUS_FLAGS, CM_PROB, CR_SUCCESS};
    let mut enabled = false;
    for_each_device(MTT_HARDWARE_ID, |_, data| unsafe {
        let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        if CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) == CR_SUCCESS {
            enabled |= problem.0 != 22; // CM_PROB_DISABLED
        }
    });
    enabled
}

/// Enable or disable every device with `hardware_id`.
fn set_device_enabled(hardware_id: &str, enable: bool) -> Result<(), String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    let mut result: Result<(), String> = Err("no such device".into());
    for_each_device(hardware_id, |set, data| unsafe {
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: if enable { DICS_ENABLE } else { DICS_DISABLE },
            Scope: DICS_FLAG_GLOBAL,
            HwProfile: 0,
        };
        result = SetupDiSetClassInstallParamsW(set, Some(data), Some(&params.ClassInstallHeader), std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32)
            .and_then(|()| SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, Some(data)))
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

/// Why the virtual monitor isn't the active display (yet).
#[derive(Debug)]
enum Activation {
    /// Windows hasn't got it ready: worth waiting for.
    NotYet(String),
    /// Windows refused the layout.
    Refused(String),
}

impl std::fmt::Display for Activation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Activation::NotYet(e) | Activation::Refused(e) => f.write_str(e),
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
                return Err(Activation::NotYet(format!("SetDisplayConfig {first} / {r} (the desktop is switching)")));
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
    tracing::info!(percent = SCALE_STEPS[nearest_step(percent)], ok, "virtual monitor scale set to the client's");
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

/// Whether the driver's installed (for the launcher's This PC page).
pub fn installed() -> bool {
    device_present()
}

/// Whether it's running (its interface is there).
pub fn running() -> bool {
    interface_path().is_some()
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
        let vd = VirtualDisplay::open().expect("no virtual display driver");
        let t = Instant::now();
        let shown = vd.show(1920, 1080, 120, None);
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
    fn pnputil_listing_finds_our_package() {
        let listing = "Microsoft PnP Utility\r\n\r\nPublished Name:     oem12.inf\r\nOriginal Name:      nvlddmkm.inf\r\nProvider Name:      NVIDIA\r\n\r\nPublished Name:     oem41.inf\r\nOriginal Name:      rdpio-display.inf\r\nProvider Name:      rdpio\r\n";
        assert_eq!(published_names(listing, "rdpio-display.inf"), ["oem41.inf"]);
        assert!(published_names(listing, "other.inf").is_empty());
    }
}
