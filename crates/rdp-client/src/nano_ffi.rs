//! Host Microsoft's `rdpnanoTransport.dll` (W365 RDP Shortpath / "Basix DCT")
//! directly via its exported C API, instead of reimplementing the proprietary
//! ICE + Smiles + URCP + schannel transport stack in Rust.
//!
//! Why host the DLL: the Shortpath signaling (ICE SessionDescription) is
//! exchanged inside a nested TLS session that is NOT schannel, so it can't be
//! captured passively; and the transport itself is a full boost.asio/schannel
//! C++ library. `msrdc.exe` drives it purely through the exports below, and the
//! DLL depends only on stock Windows system DLLs (WS2_32/MSWSOCK/bcrypt/Secur32/
//! ADVAPI32/CRYPT32/ntdll/KERNEL32/IPHLPAPI/ole32/OLEAUT32 + delay WINHTTP), so
//! rdpio can `LoadLibrary` it standalone. See memory `rdpio-nano-dll-internals`.
//!
//! ## Reversed API surface (v3.2506, static analysis)
//! Every `Create*` export has the uniform shape
//! `HRESULT Create*(u32 ifaceVersion /*ecx*/, IUnknown** out /*rdx*/)`:
//! it validates `ifaceVersion` against a per-type constant (else `E_NOINTERFACE`),
//! requires `out != null` (else `E_POINTER`/`E_INVALIDARG`), constructs the
//! object, (for transport/connector) `QueryInterface`s it for a known IID, and
//! writes it to `*out`. Returned objects are IUnknown-derived COM objects:
//! `vtable[0]=QueryInterface, [1]=AddRef, [2]=Release`, interface methods at [3+].
//! Objects implement multiple interfaces (multiple vtables), classic COM.
//!
//! `RdpNanoInitialize2(arg1, u16 version /*must==0x7B08*/, void** outTable /*6 fns*/)`
//! is the versioned loader entrypoint (`rdpnanodll\loader.cpp`); it returns a
//! dispatch table of 6 function pointers. The named exports remain callable
//! directly, which is what we use here.
//!
//! ## Status
//! Loader + object creation + IUnknown lifetime are complete and final. The
//! per-interface method *semantics* (the 11-slot UDP stream vtable, the WS
//! wrapper, connector wiring, config structs, completion callbacks) are still
//! being reversed dynamically (Frida hooks on the vtable slots during a live
//! msrdc connection). Until then this module can load the DLL and mint objects,
//! but not yet drive a connection — so it stays `dead_code` and unwired.
#![allow(dead_code)]

use std::ffi::c_void;
use std::path::PathBuf;

use windows::core::{GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
};

/// `ifaceVersion` (ecx) constants each factory validates. Wrong value → E_NOINTERFACE.
pub mod iface {
    pub const CLIENT_SIDE_TRANSPORT: u32 = 5;
    pub const SERVER_SIDE_TRANSPORT: u32 = 6; // unverified; server path, not used by a client
    pub const TRANSPORT_CONNECTOR: u32 = 4;
    pub const WEBSOCKET_STREAM_WRAPPER: u32 = 1;
    pub const UDP_STREAM_WRAPPER: u32 = 0xB;
}

/// Magic passed to `RdpNanoInitialize2` as the `version` (dx) argument.
pub const NANO_INIT_VERSION: u16 = 0x7B08;

/// IID QueryInterface'd by `CreateRdpNanoClientSideTransport` — {3174AAFB-94C2-4BD0-95FE-100A1FCA0E45}.
pub const IID_CLIENT_SIDE_TRANSPORT: GUID =
    GUID::from_u128(0x3174AAFB_94C2_4BD0_95FE_100A1FCA0E45);
/// IID QueryInterface'd by `CreateRdpNanoTransportConnector` — {ABAB65C5-812B-4EBC-9B10-BF3AD274A02F}.
pub const IID_TRANSPORT_CONNECTOR: GUID =
    GUID::from_u128(0xABAB65C5_812B_4EBC_9B10_BF3AD274A02F);

/// `HRESULT Create*(u32 ifaceVersion, void** out)` — the shared factory ABI.
type CreateFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
/// `HRESULT RdpNanoInitialize2(void* arg1, u16 version, void** outTable[6])`.
type InitializeFn = unsafe extern "system" fn(*mut c_void, u16, *mut *mut c_void) -> HRESULT;
/// `void RdpNanoFreeTaskMemory(void* p)` — frees DLL-allocated task memory.
type FreeTaskMemoryFn = unsafe extern "system" fn(*mut c_void);

/// A minimal IUnknown vtable prefix (all nano objects start with this).
#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// Owned pointer to a nano COM object; releases on drop.
pub struct NanoObject(*mut c_void);

impl NanoObject {
    #[inline]
    fn vtbl(&self) -> *const IUnknownVtbl {
        // *obj == pointer to the object's first (IUnknown) vtable.
        unsafe { *(self.0 as *const *const IUnknownVtbl) }
    }
    /// Raw object pointer (for calling interface methods once their layout is known).
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }
    /// QueryInterface for another interface on this object.
    pub fn query_interface(&self, iid: &GUID) -> windows::core::Result<NanoObject> {
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { ((*self.vtbl()).query_interface)(self.0, iid, &mut out) };
        hr.ok()?;
        if out.is_null() {
            return Err(windows::core::Error::from(HRESULT(0x8000_4002u32 as i32)));
        }
        Ok(NanoObject(out))
    }
}

impl Drop for NanoObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ((*self.vtbl()).release)(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

/// The loaded `rdpnanoTransport.dll` and its resolved exports.
pub struct NanoLib {
    module: HMODULE,
    create_client_side_transport: CreateFn,
    create_transport_connector: CreateFn,
    create_websocket_stream_wrapper: CreateFn,
    create_udp_stream_wrapper: CreateFn,
    initialize2: InitializeFn,
    free_task_memory: FreeTaskMemoryFn,
}

// SAFETY: the DLL's objects are internally thread-safe (boost.asio io_context);
// the HMODULE + fn pointers are immutable after load.
unsafe impl Send for NanoLib {}
unsafe impl Sync for NanoLib {}

impl NanoLib {
    /// Load the DLL, resolving it from (1) next to our exe, else (2) the installed
    /// AVD HostApp package under `WindowsApps`. Uses `LOAD_WITH_ALTERED_SEARCH_PATH`
    /// so the DLL's own dependencies resolve from its directory.
    pub fn load() -> windows::core::Result<Self> {
        let path = resolve_dll_path().ok_or_else(|| {
            windows::core::Error::new(
                HRESULT(0x8007_0002u32 as i32), // ERROR_FILE_NOT_FOUND
                "rdpnanoTransport.dll not found next to rdpio.exe or in the AVD HostApp package",
            )
        })?;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module = unsafe {
            LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH)?
        };

        // Resolve exports; any missing one is a version mismatch we must not paper over.
        unsafe fn proc<T>(m: HMODULE, name: &[u8]) -> windows::core::Result<T> {
            match GetProcAddress(m, PCSTR(name.as_ptr())) {
                Some(p) => Ok(std::mem::transmute_copy::<_, T>(&p)),
                None => Err(windows::core::Error::new(
                    HRESULT(0x8007_007Fu32 as i32), // ERROR_PROC_NOT_FOUND
                    format!("missing export {}", String::from_utf8_lossy(name)),
                )),
            }
        }
        let lib = unsafe {
            NanoLib {
                create_client_side_transport: proc(module, b"CreateRdpNanoClientSideTransport\0")?,
                create_transport_connector: proc(module, b"CreateRdpNanoTransportConnector\0")?,
                create_websocket_stream_wrapper: proc(module, b"CreateRdpWebSocketStreamWrapper\0")?,
                create_udp_stream_wrapper: proc(module, b"CreateRdpUdpStreamWrapper\0")?,
                initialize2: proc(module, b"RdpNanoInitialize2\0")?,
                free_task_memory: proc(module, b"RdpNanoFreeTaskMemory\0")?,
                module,
            }
        };
        Ok(lib)
    }

    fn create(&self, f: CreateFn, iface_version: u32) -> windows::core::Result<NanoObject> {
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { f(iface_version, &mut out) };
        hr.ok()?;
        if out.is_null() {
            return Err(windows::core::Error::new(
                HRESULT(0x8000_4004u32 as i32), // E_ABORT
                "nano factory returned S_OK but a null object",
            ));
        }
        Ok(NanoObject(out))
    }

    pub fn create_client_side_transport(&self) -> windows::core::Result<NanoObject> {
        self.create(self.create_client_side_transport, iface::CLIENT_SIDE_TRANSPORT)
    }
    pub fn create_transport_connector(&self) -> windows::core::Result<NanoObject> {
        self.create(self.create_transport_connector, iface::TRANSPORT_CONNECTOR)
    }
    pub fn create_websocket_stream_wrapper(&self) -> windows::core::Result<NanoObject> {
        self.create(self.create_websocket_stream_wrapper, iface::WEBSOCKET_STREAM_WRAPPER)
    }
    pub fn create_udp_stream_wrapper(&self) -> windows::core::Result<NanoObject> {
        self.create(self.create_udp_stream_wrapper, iface::UDP_STREAM_WRAPPER)
    }

    /// Free memory the DLL allocated for us (task/out params documented as such).
    pub unsafe fn free_task_memory(&self, p: *mut c_void) {
        (self.free_task_memory)(p)
    }
}

impl Drop for NanoLib {
    fn drop(&mut self) {
        // FreeLibrary on shutdown; ignore failure (process is exiting anyway).
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}

use std::os::windows::ffi::OsStrExt;

/// Locate `rdpnanoTransport.dll`: bundled next to our exe first (recommended for
/// redistribution), else the highest-versioned installed AVD HostApp package.
fn resolve_dll_path() -> Option<PathBuf> {
    const DLL: &str = "rdpnanoTransport.dll";

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join(DLL);
            if bundled.exists() {
                return Some(bundled);
            }
        }
    }

    // WindowsApps\MicrosoftCorporationII.AzureVirtualDesktopHostApp_<ver>_x64__8wekyb3d8bbwe\
    let program_files =
        std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles"))?;
    let apps = PathBuf::from(program_files).join("WindowsApps");
    // read_dir may be denied by the WindowsApps ACL; that's fine — fall through to None.
    // Track the lexicographically-latest package folder (≈ highest version).
    let mut best: Option<(String, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&apps) {
        for e in entries.flatten() {
            let folder = e.file_name().to_string_lossy().into_owned();
            if folder.starts_with("MicrosoftCorporationII.AzureVirtualDesktopHostApp_")
                && folder.ends_with("_x64__8wekyb3d8bbwe")
            {
                let cand = e.path().join(DLL);
                if cand.exists() && best.as_ref().map_or(true, |(f, _)| folder > *f) {
                    best = Some((folder, cand));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}
