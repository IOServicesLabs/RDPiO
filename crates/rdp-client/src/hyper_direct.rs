//! Hyper direct mode: connect with a Hyper password, without RDP or Windows
//! credentials.
//!
//! Host side, three pieces:
//! - `--hyper-set-password` stores the password's PBKDF2 secret (never the
//!   password) DPAPI-encrypted at machine scope, in a file only SYSTEM and
//!   Administrators can read.
//! - `--hyper-service-install` registers the `rdpioHyper` Windows service,
//!   running a copy of rdpio in Program Files (only administrators can change
//!   it). The service (`--hyper-service`, session 0) keeps one agent running
//!   as SYSTEM in whatever session owns the physical console, relaunching it
//!   when that changes — the Sunshine model. SYSTEM on the console is what
//!   lets the agent show and type into the sign-in screen, the lock screen
//!   and UAC.
//! - The agent (`--hyper-console-agent`) listens on the Hyper UDP port, signs
//!   clients in with SPAKE2 (`rdp_hyper::auth`, with per-address lockout) and
//!   streams the console. When the client leaves — or the service stops — the
//!   session is locked.

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdp_hyper::auth::{self, AuthMsg, Lockout, Offer};
use rdp_hyper::control::Control;
use rdp_hyper::wire::{Direction, Kind, Session};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};

use crate::hyper_host::{qpc_us, HostOptions};

pub(crate) const SERVICE: &str = "rdpioHyper";
/// Set by the service when it stops. Its console agent then ends a stream the
/// way a client's goodbye does — audio and display restored, console locked —
/// and exits. SYSTEM-only, so nobody else can end a stream through it.
const STOP_EVENT: PCWSTR = w!(r"Global\rdpioHyperStop");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `%ProgramData%\rdpio`: the service's logs, the password secret, the
/// stream marker. The first privileged caller in a process makes sure only
/// SYSTEM and Administrators can create or change anything in it (users may
/// read the logs): a folder another user made first is taken back, and a link
/// planted in its place removed — otherwise SYSTEM's writes there could be
/// redirected. Call it before the service or agent opens its log there.
pub(crate) fn data_dir() -> PathBuf {
    use windows::Win32::UI::Shell::FOLDERID_ProgramData;
    let base = known_folder(&FOLDERID_ProgramData, None).unwrap_or_else(|| r"C:\ProgramData".into());
    let dir = base.join("rdpio");
    DATA_DIR_NOTE.get_or_init(|| {
        if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() } {
            secure_dir(&dir)
        } else {
            None
        }
    });
    dir
}

static DATA_DIR_NOTE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// What securing the data folder had to fix, if anything (for the log, once
/// it's open).
pub(crate) fn data_dir_note() -> Option<&'static str> {
    DATA_DIR_NOTE.get().and_then(|n| n.as_deref())
}

/// The data folder's access: SYSTEM and Administrators everything, users
/// read; inherited by what's created inside.
const DATA_DIR_SDDL: PCWSTR = w!("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)");

fn secure_dir(dir: &Path) -> Option<String> {
    use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
    use windows::Win32::Security::{SetFileSecurityW, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    windows::core::link!("kernel32.dll" "system" fn CreateDirectoryW(path: PCWSTR, sa: *const windows::Win32::Security::SECURITY_ATTRIBUTES) -> windows::core::BOOL);
    const ATTR_DIRECTORY: u32 = 0x10;
    const ATTR_REPARSE_POINT: u32 = 0x400;
    let mut notes = Vec::new();
    // A link (junction, symlink) or a file where the folder goes: remove the
    // link itself — never what it points at.
    if let Ok(m) = std::fs::symlink_metadata(dir) {
        let attrs = m.file_attributes();
        if attrs & ATTR_REPARSE_POINT != 0 || attrs & ATTR_DIRECTORY == 0 {
            let removed = if attrs & ATTR_DIRECTORY != 0 { std::fs::remove_dir(dir) } else { std::fs::remove_file(dir) };
            notes.push(format!("removed a link or file planted at {} ({removed:?})", dir.display()));
        }
    }
    let name: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let is_real_dir = |d: &Path| {
        std::fs::symlink_metadata(d).is_ok_and(|m| m.file_attributes() & (ATTR_DIRECTORY | ATTR_REPARSE_POINT) == ATTR_DIRECTORY)
    };
    if is_real_dir(dir) {
        let foreign = !owned_by_admins_dir(dir);
        // Owner and access set outright (this also tightens a folder an
        // older build made with ProgramData's inherited access).
        let set = with_security(DATA_DIR_SDDL, |sa| unsafe {
            SetFileSecurityW(
                PCWSTR(name.as_ptr()),
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                PSECURITY_DESCRIPTOR(sa.lpSecurityDescriptor),
            )
            .as_bool()
        })
        .unwrap_or(false);
        if foreign && set {
            notes.push(format!("took back {}, which another user had created", dir.display()));
        } else if foreign {
            // Can't take it back: move it out of the way.
            let aside = dir.with_file_name(format!("rdpio.untrusted-{}", std::process::id()));
            let moved = std::fs::rename(dir, &aside);
            notes.push(format!("moved {} (made by another user) aside to {} ({moved:?})", dir.display(), aside.display()));
        }
    }
    if !is_real_dir(dir) {
        let made = with_security(DATA_DIR_SDDL, |sa| unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), sa).as_bool() }).unwrap_or(false);
        if !made {
            notes.push(format!("could not create {}: {}", dir.display(), std::io::Error::last_os_error()));
        }
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// [`owned_by_admins`] for a folder, without following a link in its place.
fn owned_by_admins_dir(dir: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const READ_CONTROL: u32 = 0x0002_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .is_ok_and(|f| owned_by_admins(&f))
}

fn secret_path() -> PathBuf {
    data_dir().join("hyper.secret")
}

/// How this PC streams, as set on the launcher's This PC page (kept in
/// `%ProgramData%\rdpio\hyper-host.json`, which only administrators can
/// change). Read at each stream's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HostSettings {
    /// A display of the client's own (the virtual display driver), where
    /// it's installed.
    pub virtual_display: bool,
    /// Without one, this PC's monitor at the client's resolution (or the
    /// closest); off: it stays as it is, the picture scaled to fit.
    pub match_resolution: bool,
    /// The monitor at its fastest refresh rate that's a whole multiple of the
    /// stream's (240 Hz for 120 fps): frames are captured sooner.
    pub fast_refresh: bool,
    /// This PC's other monitors stay on while it streams (otherwise dark).
    pub keep_monitors: bool,
    /// The service updates itself from GitHub (between streams).
    pub auto_update: bool,
    /// …to nightly builds rather than releases.
    pub nightly: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self { virtual_display: true, match_resolution: true, fast_refresh: true, keep_monitors: false, auto_update: true, nightly: false }
    }
}

fn host_settings_path() -> PathBuf {
    data_dir().join("hyper-host.json")
}

/// This PC's [`HostSettings`] (the defaults when none were saved).
pub fn host_settings() -> HostSettings {
    std::fs::read(host_settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// `--hyper-host-settings name=1,name=0,…` (elevated, from the launcher):
/// changes the named settings, keeps the rest.
pub fn save_host_settings() -> Result<(), Box<dyn std::error::Error>> {
    let spec = std::env::args().skip_while(|a| a != "--hyper-host-settings").nth(1).ok_or("no settings given")?;
    let mut s = host_settings();
    for pair in spec.split(',').filter(|p| !p.trim().is_empty()) {
        let (name, value) = pair.split_once('=').ok_or_else(|| format!("{pair:?} isn't name=0|1"))?;
        let on = match value.trim() {
            "1" | "on" | "true" => true,
            "0" | "off" | "false" => false,
            v => return Err(format!("{v:?} isn't 0 or 1").into()),
        };
        match name.trim() {
            "virtual_display" => s.virtual_display = on,
            "match_resolution" => s.match_resolution = on,
            "fast_refresh" => s.fast_refresh = on,
            "keep_monitors" => s.keep_monitors = on,
            "auto_update" => s.auto_update = on,
            "nightly" => s.nightly = on,
            n => return Err(format!("no setting {n:?}").into()),
        }
    }
    std::fs::write(host_settings_path(), serde_json::to_vec_pretty(&s)?)?;
    println!("Hyper host settings: {s:?}");
    Ok(())
}

/// A Windows tool by absolute path in System32. Never a bare name: Windows
/// looks that up in this exe's own folder first, so anyone who can write
/// there could plant an `sc.exe` for the installer or the SYSTEM agent to run.
pub(crate) fn system_tool(name: &str) -> PathBuf {
    system_dir().join(name)
}

/// System32, asked of Windows rather than read from the environment.
fn system_dir() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    windows::core::link!("kernel32.dll" "system" fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32);
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
}

/// A known folder — `token`'s user's rather than ours if given. From the
/// registry, not the environment, which a user can point anywhere.
pub(crate) fn known_folder(id: &windows::core::GUID, token: Option<HANDLE>) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, token).ok()?;
        let path = PathBuf::from(std::ffi::OsString::from_wide(p.as_wide()));
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        Some(path)
    }
}

/// The copy of this binary that the SYSTEM service (and the elevated logon
/// task) run: `%ProgramFiles%\rdpio\rdpio.exe`, which only administrators can
/// change. Registering the running exe instead would hand SYSTEM a file any
/// user can swap whenever it sits in a user-writable folder (`C:\_tools`).
/// Refreshed when this build differs.
pub(crate) fn installed_exe() -> std::io::Result<PathBuf> {
    use windows::Win32::UI::Shell::FOLDERID_ProgramFiles;
    let current = std::env::current_exe()?;
    let dir = known_folder(&FOLDERID_ProgramFiles, None)
        .ok_or_else(|| std::io::Error::other("can't find the Program Files folder"))?
        .join("rdpio");
    let target = dir.join("rdpio.exe");
    // The signed manifest goes along (so the host can hand itself to clients).
    let manifest = || match std::fs::read(crate::updater::sidecar(&current)) {
        Ok(m) => std::fs::write(crate::updater::sidecar(&target), m),
        Err(_) => std::fs::remove_file(crate::updater::sidecar(&target)).or(Ok(())),
    };
    if same_contents(&current, &target) {
        manifest()?;
        return Ok(target);
    }
    std::fs::create_dir_all(&dir)?;
    // Earlier builds that were moved aside while still running.
    for old in std::fs::read_dir(&dir)?.flatten() {
        if old.file_name().to_string_lossy().ends_with(".old") {
            let _ = std::fs::remove_file(old.path());
        }
    }
    // The copy takes its ACL from the folder (never from its source).
    let staged = dir.join("rdpio.exe.new");
    std::fs::copy(&current, &staged)?;
    if std::fs::rename(&staged, &target).is_err() {
        // A running exe (an agent still up) can't be replaced, only moved.
        let aside = dir.join(format!("rdpio.{}.old", std::process::id()));
        let swapped = std::fs::rename(&target, &aside).and_then(|()| {
            std::fs::rename(&staged, &target).inspect_err(|_| {
                let _ = std::fs::rename(&aside, &target);
            })
        });
        if let Err(e) = swapped {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    }
    manifest()?;
    Ok(target)
}

/// Whether two files hold the same bytes (false if either can't be read).
pub(crate) fn same_contents(a: &Path, b: &Path) -> bool {
    let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
    len(a).is_some()
        && len(a) == len(b)
        && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Machine-scope DPAPI (any process on this machine can decrypt — the file's
/// ACL is what keeps it to SYSTEM and Administrators).
fn dpapi(data: &[u8], protect: bool) -> std::io::Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        let r = if protect {
            CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        r.map_err(|e| std::io::Error::other(e.to_string()))?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        Ok(v)
    }
}

/// The new Hyper password the launcher handed this (elevated) copy over its
/// pipe, if it did (`--launcher-pipe`).
static LAUNCHER_PASSWORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub(crate) fn set_launcher_password(pw: String) {
    let _ = LAUNCHER_PASSWORD.set(pw);
}

/// `--hyper-set-password`: prompt (twice) and store the password's secret
/// (or the one the launcher sent).
pub fn set_password() -> Result<(), Box<dyn std::error::Error>> {
    require_admin()?;
    let pw = match LAUNCHER_PASSWORD.get() {
        Some(pw) => pw.clone(),
        None => {
            let pw = crate::prompt::read_password("New Hyper password: ")?;
            if crate::prompt::read_password("Again: ")? != pw {
                return Err("the passwords didn't match".into());
            }
            pw
        }
    };
    if pw.is_empty() {
        eprintln!("Note: an empty Hyper password lets anyone who can reach this PC's Hyper port sign in and control it.");
    }
    let blob = dpapi(&auth::password_secret(&pw), true)?;
    write_secret(&secret_path(), &blob)?;
    println!("Hyper password set. Clients connect with: rdpio --host <this PC> --hyper --direct");
    Ok(())
}

/// Store the secret in a new file only SYSTEM and Administrators can open,
/// from the moment it exists. Any user may add files to our ProgramData
/// folder: one could otherwise plant the file first (and own it), or open
/// it before a later lock-down — and with machine-scope DPAPI, reading the
/// blob is as good as knowing the password.
fn write_secret(path: &Path, blob: &[u8]) -> std::io::Result<()> {
    write_new_file(path, blob, w!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)"))
}

/// Replace `path` with a new file holding `bytes`, created with the security
/// descriptor `sddl` — never written through whatever was there before.
fn write_new_file(path: &Path, bytes: &[u8], sddl: PCWSTR) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    windows::core::link!("kernel32.dll" "system" fn CreateFileW(
        name: PCWSTR,
        access: u32,
        share: u32,
        security: *const SECURITY_ATTRIBUTES,
        disposition: u32,
        flags: u32,
        template: HANDLE
    ) -> HANDLE);
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const CREATE_NEW: u32 = 1;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut file = with_security(sddl, |sa| unsafe {
        let h = CreateFileW(PCWSTR(name.as_ptr()), GENERIC_WRITE, 0, sa, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, HANDLE::default());
        if h.is_invalid() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(std::fs::File::from_raw_handle(h.0))
        }
    })
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    file.write_all(bytes)
}

/// Run `f` with security attributes made from `sddl` (a security
/// descriptor in its string form).
pub(crate) fn with_security<T>(sddl: PCWSTR, f: impl FnOnce(&windows::Win32::Security::SECURITY_ATTRIBUTES) -> T) -> windows::core::Result<T> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    windows::core::link!("advapi32.dll" "system" fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: PCWSTR,
        revision: u32,
        sd: *mut PSECURITY_DESCRIPTOR,
        size: *mut u32
    ) -> windows::core::BOOL);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, &mut sd, std::ptr::null_mut()) }.ok()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let result = f(&sa);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    Ok(result)
}

fn load_secret() -> Result<[u8; 32], String> {
    use std::io::Read;
    let not_set = || "no Hyper password set: run `rdpio --hyper-set-password` as administrator".to_string();
    let mut file = std::fs::File::open(secret_path()).map_err(|_| not_set())?;
    // Users can add files to our folder but can't give one away to SYSTEM or
    // Administrators: any other owner means it was planted — a password of
    // someone's choosing.
    if !owned_by_admins(&file) {
        return Err("the stored Hyper password wasn't written by an administrator; set it again with `rdpio --hyper-set-password`".into());
    }
    let mut blob = Vec::new();
    file.read_to_end(&mut blob).map_err(|_| not_set())?;
    dpapi(&blob, false)
        .map_err(|e| format!("can't read the Hyper password: {e}"))?
        .try_into()
        .map_err(|_| "the stored Hyper password is corrupt; set it again".to_string())
}

/// Whether an open file (or folder) is owned by SYSTEM or Administrators.
fn owned_by_admins(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Security::{
        GetKernelObjectSecurity, GetSecurityDescriptorOwner, IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    // An owner-only security descriptor: a header and one SID.
    let mut buf = [0u64; 16];
    let mut needed = 0u32;
    let sd = PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast());
    let (mut owner, mut defaulted) = (PSID::default(), windows::core::BOOL::default());
    unsafe {
        GetKernelObjectSecurity(HANDLE(file.as_raw_handle()), OWNER_SECURITY_INFORMATION.0, Some(sd), std::mem::size_of_val(&buf) as u32, &mut needed)
            .is_ok()
            && GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted).is_ok()
            && !owner.0.is_null()
            && (IsWellKnownSid(owner, WinLocalSystemSid).as_bool() || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool())
    }
}

fn require_admin() -> Result<(), Box<dyn std::error::Error>> {
    if unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() } {
        Ok(())
    } else {
        Err("run this from an elevated (Administrator) PowerShell".into())
    }
}

/// `--hyper-service-install` / `--hyper-service-uninstall`.
pub fn install_service(uninstall: bool, opts: &HostOptions) -> Result<(), Box<dyn std::error::Error>> {
    require_admin()?;
    // Stopped (not killed) first, so a streaming agent restores the console.
    let removed = remove_service()?;
    // The older per-user logon agent would fight over the port.
    let _ = crate::hyper_host::install(true, opts.port);
    if uninstall {
        if removed {
            println!("Removed the {SERVICE} service.");
        } else {
            println!("The {SERVICE} service isn't installed.");
        }
        return Ok(());
    }
    // (A password from the launcher replaces the one there.)
    if load_secret().is_err() || LAUNCHER_PASSWORD.get().is_some() {
        set_password()?;
    }
    let exe = installed_exe()?;
    // The virtual display too, when it ships beside this rdpio (best
    // effort: without it, streams use the host's own monitor).
    if std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("rdpio-display").join("rdpio-display.inf"))).is_some_and(|p| p.exists()) {
        match crate::virtual_display::install(false) {
            Ok(note) => println!("{note}"),
            Err(e) => println!("The rdpio Display driver wasn't installed: {e}"),
        }
    }
    let sc = |args: &[&str]| std::process::Command::new(system_tool("sc.exe")).args(args).output();
    let mut bin = format!(
        "\"{}\" --hyper-service --hyper-port {} --hyper-mbps {} --log-file \"{}\"",
        exe.display(),
        opts.port,
        opts.max_mbps,
        data_dir().join("hyper-service.log").display()
    );
    if let Some(f) = opts.max_fps {
        bin += &format!(" --hyper-fps {f}");
    }
    if let Some((w, h)) = opts.stream_size {
        bin += &format!(" --hyper-size {w}x{h}");
    }
    if opts.keep_monitors {
        bin += " --hyper-keep-monitors";
    }
    let out = sc(&["create", SERVICE, "binPath=", &bin, "start=", "auto", "DisplayName=", "rdpio Hyper Accelerator"])?;
    if !out.status.success() {
        return Err(format!("sc create failed: {}", String::from_utf8_lossy(&out.stdout).trim()).into());
    }
    let _ = sc(&["description", SERVICE, "Low-latency game streaming for rdpio clients (Hyper direct mode)."]);
    // Let services raise Ctrl+Alt+Del (Ctrl+Alt+End on the client), as remote
    // tools must for a "press Ctrl+Alt+Del to sign in" lock screen.
    let _ = std::process::Command::new(system_tool("reg.exe"))
        .args([
            "add",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System",
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ])
        .output();
    let _ = sc(&["failure", SERVICE, "reset=", "60", "actions=", "restart/2000/restart/5000/restart/10000"]);
    // install(false) adds the firewall rule and the logon task; we only want the rule.
    let rule = "rdpio Hyper (UDP)";
    let _ = std::process::Command::new(system_tool("netsh.exe"))
        .args(["advfirewall", "firewall", "add", "rule", &format!("name={rule}"), "dir=in", "action=allow", "protocol=UDP", &format!("localport={}", opts.port)])
        .output();
    let out = sc(&["start", SERVICE])?;
    if !out.status.success() {
        return Err(format!("the service didn't start: {}", String::from_utf8_lossy(&out.stdout).trim()).into());
    }
    println!(
        "Installed and started the {SERVICE} service ({}); UDP {} allowed through the firewall.\nLogs: {}",
        exe.display(),
        opts.port,
        data_dir().display()
    );
    Ok(())
}

/// Stop the `rdpioHyper` service and delete it; `Ok(false)` if it isn't
/// installed. Waits for the stop, which gives a streaming agent time to
/// restore the console, and lets a reinstall replace the exe and recreate
/// the service (one still stopping can be neither).
pub(crate) fn remove_service() -> Result<bool, String> {
    remove_named_service(SERVICE)
}

fn remove_named_service(name: &str) -> Result<bool, String> {
    use windows::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE};
    use windows::Win32::System::Services::*;
    /// The standard DELETE access right.
    const DELETE: u32 = 0x0001_0000;
    let wname = wide(name);
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map_err(|e| format!("can't open the service manager: {e}"))?;
        let svc = match OpenServiceW(scm, PCWSTR(wname.as_ptr()), SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE) {
            Ok(svc) => svc,
            Err(e) => {
                let _ = CloseServiceHandle(scm);
                return if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() {
                    Ok(false)
                } else {
                    Err(format!("can't open the {name} service: {e}"))
                };
            }
        };
        let mut status = SERVICE_STATUS::default();
        let deadline = Instant::now() + SERVICE_STOP_TIMEOUT;
        while QueryServiceStatus(svc, &mut status).is_ok() && status.dwCurrentState != SERVICE_STOPPED && Instant::now() < deadline {
            if status.dwCurrentState == SERVICE_RUNNING {
                // (Again if it was still starting the first time.)
                let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if status.dwCurrentState != SERVICE_STOPPED {
            tracing::warn!(service = name, "the service didn't stop in time; deleting it anyway");
        }
        let deleted = match DeleteService(svc) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == ERROR_SERVICE_MARKED_FOR_DELETE.to_hresult() => Ok(true),
            Err(e) => Err(format!("can't delete the {name} service: {e}")),
        };
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        deleted
    }
}

// ---------------------------------------------------------------- service

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: Mutex<Option<isize>> = Mutex::new(None);
static SERVICE_OPTS: Mutex<Option<HostOptions>> = Mutex::new(None);
/// The stop event's raw handle (0: none), for the control handler to set.
static STOP_EVENT_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// How long a stopping service gives its agent to wind a stream down.
const AGENT_STOP_GRACE: Duration = Duration::from_secs(5);
/// How long `remove_service` waits for the service to stop: two agent
/// graces (the agent, then one restoring after it) and some slack.
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// `--hyper-service`: hand this process to the service control manager.
pub fn run_service(opts: HostOptions) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::System::Services::*;
    *SERVICE_OPTS.lock().unwrap() = Some(opts);
    let mut name = wide(SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr())? };
    Ok(())
}

/// Report `state`. While stopping, each report needs a higher `checkpoint`
/// within the wait hint, or the service manager decides the service hung.
fn set_status(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, checkpoint: u32) {
    use windows::Win32::System::Services::*;
    let Some(h) = *STATUS_HANDLE.lock().unwrap() else { return };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        // Pre-shutdown comes early enough in a shutdown to stop the agent
        // properly (the default audio devices it changed persist).
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_PRESHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: 3000,
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(h as *mut _), &status);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    use windows::Win32::System::Services::*;
    // Before the handler exists: a stop can come at once.
    match create_stop_event(STOP_EVENT) {
        Ok(ev) => STOP_EVENT_HANDLE.store(ev.0 as isize, Ordering::SeqCst),
        Err(e) => tracing::warn!(error = %e, "no stop event: stopping the service will cut a stream off"),
    }
    let name = wide(SERVICE);
    match RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None) {
        Ok(h) => *STATUS_HANDLE.lock().unwrap() = Some(h.0 as isize),
        Err(e) => {
            tracing::error!(error = %e, "RegisterServiceCtrlHandlerEx failed");
            return;
        }
    }
    set_status(SERVICE_RUNNING, 0);
    let opts = SERVICE_OPTS.lock().unwrap().clone().expect("service options");
    tracing::info!(port = opts.port, "rdpio Hyper service running");
    supervise(&opts);
    tracing::info!("rdpio Hyper service stopped");
    set_status(SERVICE_STOPPED, 0);
}

unsafe extern "system" fn control_handler(control: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    use windows::Win32::System::Services::*;
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN | SERVICE_CONTROL_PRESHUTDOWN => {
            set_status(SERVICE_STOP_PENDING, 1);
            STOP.store(true, Ordering::SeqCst);
            // The agent hears it too; supervise() then waits for it.
            let ev = STOP_EVENT_HANDLE.load(Ordering::SeqCst);
            if ev != 0 {
                let _ = windows::Win32::System::Threading::SetEvent(HANDLE(ev as *mut _));
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

/// Keep one agent running in the console session. An agent that dies
/// quickly (no password set, port taken…) is restarted with growing delays
/// rather than in a tight loop.
fn supervise(opts: &HostOptions) {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    let update = watch_for_updates();
    let mut child: Option<(u32, HANDLE, Instant)> = None;
    let mut backoff = Duration::ZERO;
    let mut next_launch = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let alive = child
            .as_ref()
            .is_some_and(|(s, h, _)| *s == session && unsafe { WaitForSingleObject(*h, 0) } == WAIT_TIMEOUT);
        if !alive && session != u32::MAX {
            if let Some((old_session, h, started)) = child.take() {
                if old_session != session {
                    // Still running in the old session: wound down, not killed.
                    hand_off(h);
                }
                unsafe {
                    let _ = TerminateProcess(h, 0);
                    let _ = CloseHandle(h);
                }
                if old_session == session {
                    // It exited on its own: back off if it didn't last.
                    backoff = if started.elapsed() < Duration::from_secs(30) {
                        (backoff * 2).clamp(Duration::from_secs(2), Duration::from_secs(60))
                    } else {
                        Duration::ZERO
                    };
                    tracing::warn!(backoff_s = backoff.as_secs(), "Hyper agent exited; restarting");
                    next_launch = Instant::now() + backoff;
                } else {
                    tracing::info!(session, "console session changed; moving the Hyper agent");
                    next_launch = Instant::now();
                }
            }
            if Instant::now() >= next_launch {
                match launch_agent(session, opts) {
                    Ok(h) => {
                        tracing::info!(session, "Hyper agent started on the console");
                        child = Some((session, h, Instant::now()));
                    }
                    Err(e) => {
                        tracing::warn!(session, error = %e, "could not start the Hyper agent");
                        next_launch = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
        }
        // A newer rdpio, between streams: in place, and the service restarts on it.
        if !streaming_marker().exists() {
            if let Some(p) = update.lock().unwrap().take() {
                if let Err(e) = install_update(&p, child.take(), opts) {
                    tracing::warn!(error = %e, "the update didn't install; carrying on with this rdpio");
                }
            }
        }
        sleep_unless_stopping(Duration::from_millis(500));
    }
    if let Some((session, h, _)) = child {
        stop_agent(session, h, opts);
    }
}

/// How often the service looks for a new rdpio (and how long after starting).
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(120);

/// Look on GitHub for a newer rdpio now and then (when this PC's settings
/// allow); one that's found and checked waits in the returned slot.
fn watch_for_updates() -> Arc<Mutex<Option<rdp_hyper::update::Package>>> {
    let slot = Arc::new(Mutex::new(None));
    let found = slot.clone();
    let _ = std::thread::Builder::new().name("rdpio-updates".into()).spawn(move || {
        crate::net_tune::eco_thread();
        sleep_unless_stopping(FIRST_UPDATE_CHECK);
        while !STOP.load(Ordering::SeqCst) {
            let s = host_settings();
            if s.auto_update {
                let channel = if s.nightly { "nightly" } else { "stable" };
                match crate::updater::check_github(channel) {
                    Ok(Some(remote)) => {
                        tracing::info!(version = %remote.manifest.version, build = %remote.manifest.build, "a newer rdpio is out: downloading");
                        match crate::updater::download(&remote, |_, _| {}) {
                            Ok(p) => {
                                *found.lock().unwrap() = Some(p);
                                return;
                            }
                            Err(e) => tracing::warn!(error = %e, "the update didn't download"),
                        }
                    }
                    Ok(None) => tracing::debug!(channel, "rdpio is up to date"),
                    Err(e) => tracing::info!(error = %e, "couldn't look for updates"),
                }
            }
            sleep_unless_stopping(UPDATE_EVERY);
        }
    });
    slot
}

/// Put a downloaded rdpio in place of this one, then end this process
/// without saying "stopped": the service manager restarts it (its recovery
/// actions), on the new exe.
fn install_update(
    p: &rdp_hyper::update::Package,
    child: Option<(u32, HANDLE, Instant)>,
    opts: &HostOptions,
) -> std::io::Result<()> {
    use windows::Win32::System::Threading::{ResetEvent, SetEvent};
    let target = std::env::current_exe()?;
    let new = data_dir().join("rdpio-update.exe");
    std::fs::write(&new, &p.exe)?;
    // The agent first (it runs this exe), wound down like a service stop.
    let ev = HANDLE(STOP_EVENT_HANDLE.load(Ordering::SeqCst) as *mut _);
    if let Some((session, h, _)) = child {
        unsafe {
            let _ = SetEvent(ev);
        }
        stop_agent(session, h, opts);
        unsafe {
            let _ = ResetEvent(ev);
        }
    }
    let placed = crate::updater::replace_exe(&target, &new, &p.signed);
    let _ = std::fs::remove_file(&new);
    placed?;
    tracing::info!(version = %p.manifest.version, build = %p.manifest.build, "rdpio updated; the service restarts on it");
    std::process::exit(1);
}

/// The console moved to another session (a sign-out, a user switch): end the
/// old session's agent the way a service stop does — its stream winds down,
/// putting the audio devices and displays back — instead of killing it and
/// leaving the next agent to recover. The stop event is cleared again for the
/// next agent (unless the service itself is stopping meanwhile).
fn hand_off(agent: HANDLE) {
    use windows::Win32::System::Threading::{ResetEvent, SetEvent, WaitForSingleObject};
    let ev = STOP_EVENT_HANDLE.load(Ordering::SeqCst);
    if ev == 0 {
        return; // no event: the caller kills it
    }
    let ev = HANDLE(ev as *mut _);
    unsafe {
        let _ = SetEvent(ev);
        if WaitForSingleObject(agent, AGENT_STOP_GRACE.as_millis() as u32) != WAIT_OBJECT_0 {
            tracing::warn!("the old session's Hyper agent didn't stop in time; ending it");
        }
        if !STOP.load(Ordering::SeqCst) {
            let _ = ResetEvent(ev);
            // A stop that came in meanwhile keeps its event set.
            if STOP.load(Ordering::SeqCst) {
                let _ = SetEvent(ev);
            }
        }
    }
}

/// The service's stop event: manual-reset and SYSTEM-only (nobody else may
/// set it). Reset, as an agent from an earlier run may still hold it set.
unsafe fn create_stop_event(name: PCWSTR) -> windows::core::Result<HANDLE> {
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent};
    let ev = with_security(w!("D:(A;;GA;;;SY)"), |sa| CreateEventW(Some(std::ptr::from_ref(sa)), true, false, name))??;
    let _ = ResetEvent(ev);
    Ok(ev)
}

/// Sleep up to `limit`, waking as soon as the service is told to stop.
fn sleep_unless_stopping(limit: Duration) {
    use windows::Win32::System::Threading::WaitForSingleObject;
    match STOP_EVENT_HANDLE.load(Ordering::SeqCst) {
        0 => std::thread::sleep(limit),
        ev => unsafe {
            let _ = WaitForSingleObject(HANDLE(ev as *mut _), limit.as_millis() as u32);
        },
    }
}

/// The service is stopping and the stop event is set: give the agent time to
/// end a stream the normal way (audio and display restored, console locked)
/// before killing it. What a killed agent left is restored by a fresh one —
/// it recovers, sees the stop and exits. And a killed agent's console session
/// is disconnected, which at least locks it: its marker may be gone already
/// (cleared before the display is put back and the console locked).
fn stop_agent(session: u32, agent: HANDLE, opts: &HostOptions) {
    let mut checkpoint = 1;
    let killed = !wait_exit(agent, AGENT_STOP_GRACE, &mut checkpoint);
    if killed {
        tracing::warn!("the Hyper agent didn't stop in time; ending it");
        kill(agent);
    }
    unsafe {
        let _ = CloseHandle(agent);
    }
    let marker = streaming_marker();
    let mut locked = false;
    // Only an agent that finds the stop event set knows it is there just to
    // recover (without one it would sign clients in until killed).
    if marker.exists() && STOP_EVENT_HANDLE.load(Ordering::SeqCst) != 0 {
        tracing::warn!(session, "a Hyper stream wasn't wound down; restoring the console");
        match launch_agent(session, opts) {
            Ok(h) => {
                if !wait_exit(h, AGENT_STOP_GRACE, &mut checkpoint) {
                    kill(h);
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not start an agent to restore the console"),
        }
        // Recovery locks the console before it clears the marker.
        locked = !marker.exists();
    }
    if !locked && (killed || marker.exists()) {
        use windows::Win32::System::RemoteDesktop::{WTSDisconnectSession, WTS_CURRENT_SERVER_HANDLE};
        let r = unsafe { WTSDisconnectSession(Some(WTS_CURRENT_SERVER_HANDLE), session, false) };
        tracing::warn!(session, ok = r.is_ok(), "disconnected the console session to lock it");
    }
}

/// Wait up to `limit` for a process to exit, telling the service manager
/// the stop is progressing.
fn wait_exit(process: HANDLE, limit: Duration, checkpoint: &mut u32) -> bool {
    use windows::Win32::System::Services::SERVICE_STOP_PENDING;
    use windows::Win32::System::Threading::WaitForSingleObject;
    let deadline = Instant::now() + limit;
    loop {
        let step = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(1));
        let r = unsafe { WaitForSingleObject(process, step.as_millis() as u32) };
        if r == WAIT_OBJECT_0 {
            return true;
        }
        if r != WAIT_TIMEOUT || step.is_zero() {
            return false;
        }
        *checkpoint += 1;
        set_status(SERVICE_STOP_PENDING, *checkpoint);
    }
}

/// End a process now, and give it a moment to be gone (its exe file and UDP
/// port released).
fn kill(process: HANDLE) {
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    unsafe {
        let _ = TerminateProcess(process, 1);
        let _ = WaitForSingleObject(process, 2000);
    }
}

/// The file `hyper_host` keeps while a stream has the console changed and
/// unlocked; it outlives an agent that didn't wind its stream down.
fn streaming_marker() -> PathBuf {
    data_dir().join("hyper-streaming.json")
}

/// Start `--hyper-console-agent` as SYSTEM in `session` (our own token with
/// its session id changed — needs SeTcbPrivilege, which LocalSystem has).
fn launch_agent(session: u32, opts: &HostOptions) -> windows::core::Result<HANDLE> {
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::*;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token)?;
        let mut dup = HANDLE::default();
        let r = DuplicateTokenEx(token, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut dup);
        let _ = CloseHandle(token);
        r?;
        // Closed whatever happens below (a failing launch is retried every
        // few seconds: a leak there would add up).
        struct Owned(HANDLE);
        impl Drop for Owned {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _dup = Owned(dup);
        SetTokenInformation(dup, TokenSessionId, &session as *const u32 as *const _, 4)?;
        let exe = std::env::current_exe().map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        let log = data_dir().join("hyper-agent.log");
        let mut cmd = format!(
            "\"{}\" --hyper-console-agent --hyper-port {} --hyper-mbps {} --log-file \"{}\"",
            exe.display(),
            opts.port,
            opts.max_mbps,
            log.display()
        );
        if let Some(f) = opts.max_fps {
            cmd += &format!(" --hyper-fps {f}");
        }
        if let Some((w, h)) = opts.stream_size {
            cmd += &format!(" --hyper-size {w}x{h}");
        }
        if opts.keep_monitors {
            cmd += " --hyper-keep-monitors";
        }
        let mut cmd_w = wide(&cmd);
        let mut desktop = wide(r"winsta0\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(
            Some(dup),
            PCWSTR::null(),
            Some(PWSTR(cmd_w.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        r?;
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}

/// Lock `session` as its signed-in user — a SYSTEM caller's LockWorkStation
/// reports success without locking: this exe's `--hyper-lock-session` runs
/// with the user's token on their desktop. Needs SYSTEM (the user's token).
pub(crate) fn lock_as_user(session: u32) -> windows::core::Result<()> {
    run_as_user(session, "--hyper-lock-session").map(|_| ())
}

/// Run this exe with `args` as `session`'s signed-in user, on their desktop,
/// and wait (up to 3 s) for it: its exit code, if it finished. Needs SYSTEM
/// (the user's token).
pub(crate) fn run_as_user(session: u32, args: &str) -> windows::core::Result<Option<u32>> {
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::*;
    unsafe {
        let mut token = HANDLE::default();
        WTSQueryUserToken(session, &mut token)?;
        struct Owned(HANDLE);
        impl Drop for Owned {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _token = Owned(token);
        let exe = std::env::current_exe().map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        let mut cmd_w = wide(&format!("\"{}\" {args}", exe.display()));
        let mut desktop = wide(r"winsta0\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd_w.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )?;
        let finished = WaitForSingleObject(pi.hProcess, 3000) == WAIT_OBJECT_0;
        let mut code = 0u32;
        let code = (finished && GetExitCodeProcess(pi.hProcess, &mut code).is_ok()).then_some(code);
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(code)
    }
}

// ------------------------------------------------------------------ agent

/// Raised in the agent once its service is stopping.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The service that started this agent is stopping: a stream should end as
/// if the client had said goodbye — so the console is restored and locked —
/// and the agent exit. Never true in a process the service didn't start.
pub(crate) fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

/// Agent side of the stop event: raise `flag` once the service sets it.
fn watch_for_stop(name: PCWSTR, flag: &'static AtomicBool) {
    use windows::Win32::System::Threading::{OpenEventW, WaitForSingleObject, INFINITE, SYNCHRONIZATION_SYNCHRONIZE};
    let ev = match unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, name) } {
        Ok(ev) => ev,
        Err(e) => {
            tracing::info!(error = %e, "no Hyper service stop event (not started by the service)");
            return;
        }
    };
    // Checked before returning: an agent that a stopping service starts only
    // to restore what a killed one left must know before it does anything.
    if unsafe { WaitForSingleObject(ev, 0) } == WAIT_OBJECT_0 {
        flag.store(true, Ordering::SeqCst);
        unsafe {
            let _ = CloseHandle(ev);
        }
        return;
    }
    let raw = ev.0 as isize;
    let watcher = std::thread::Builder::new().name("hyper-stop-watch".into()).spawn(move || unsafe {
        let ev = HANDLE(raw as *mut _);
        if WaitForSingleObject(ev, INFINITE) == WAIT_OBJECT_0 {
            tracing::info!("the Hyper service is stopping");
            flag.store(true, Ordering::SeqCst);
        }
        let _ = CloseHandle(ev);
    });
    if watcher.is_err() {
        unsafe {
            let _ = CloseHandle(ev);
        }
    }
}

struct Pending {
    addr: SocketAddr,
    key: [u8; 32],
    deadline: Instant,
    /// A sealed packet that didn't open with this attempt's key arrived — a
    /// password guess was tested (charged when the attempt ends unfinished).
    tested: bool,
    fps: u32,
    monitors: u32,
    codec: rdp_gpu::h264::Codec,
    fit_to: Option<(u32, u32)>,
    office: bool,
    fast_text: bool,
    scale: u32,
    /// The protocol version agreed; 0: none in common (an update-only session).
    protocol: u8,
}

/// A client that proved it knows the Hyper password.
pub(crate) struct SignedIn {
    pub addr: SocketAddr,
    pub recv: Session,
    pub send: Arc<Mutex<Session>>,
    pub fps: u32,
    pub monitors: u32,
    pub codec: rdp_gpu::h264::Codec,
    /// The client's screen, when it asked for the stream to fit it.
    pub fit_to: Option<(u32, u32)>,
    /// It starts in office mode.
    pub office: bool,
    /// …with the fast preset.
    pub fast_text: bool,
    /// The client's display scale (percent; 0 unknown).
    pub scale: u32,
    /// No protocol version in common: the session only hands the client
    /// this host's rdpio (see [`serve_update`]).
    pub update_only: bool,
    /// The client's first packet, when it was an update request (answered
    /// by [`serve_update`]).
    pub first_update: Option<Vec<u8>>,
}

/// The agent's sign-in state machine (separate from the socket loop so it can
/// be tested end to end over loopback).
pub(crate) struct SignIn {
    secret: [u8; 32],
    lockout: Lockout,
    cookies: auth::CookieJar,
    epoch: Instant,
    /// Unfinished sign-ins, per client address (one address can't disturb
    /// another's).
    pending: std::collections::HashMap<SocketAddr, Pending>,
    /// SPAKE2 computations in the current second (rate limit).
    work: (Instant, u32),
}

/// Unfinished sign-ins tracked at most, and per source IP (a client has
/// one, two while retrying; more would be parallel password guesses racing
/// the lockout).
const MAX_PENDING: usize = 16;
const MAX_PENDING_PER_IP: usize = 2;
/// Once a packet that doesn't open arrives for an attempt, the attempt has
/// this long left: enough for the real client's ping (if that junk was
/// spoofed), short enough that a wrong guess is charged promptly.
const AFTER_FAILED_OPEN: Duration = Duration::from_secs(1);
/// SPAKE2 computations per second at most (each costs a few hundred µs).
const MAX_WORK_PER_S: u32 = 20;

impl SignIn {
    /// A new Hyper password (`--hyper-set-password`), for sign-ins from now on.
    pub(crate) fn set_secret(&mut self, secret: [u8; 32]) {
        self.secret = secret;
    }

    pub(crate) fn new(secret: [u8; 32]) -> Self {
        Self {
            secret,
            lockout: Lockout::new(),
            cookies: auth::CookieJar::new(),
            epoch: Instant::now(),
            pending: Default::default(),
            work: (Instant::now(), 0),
        }
    }

    /// Expire unfinished attempts. One that tested a password (sent a packet
    /// its key couldn't open) counts as a failure; an abandoned one doesn't —
    /// it guessed nothing.
    pub(crate) fn tick(&mut self) {
        let now = Instant::now();
        let expired: Vec<SocketAddr> = self.pending.iter().filter(|(_, p)| now > p.deadline).map(|(a, _)| *a).collect();
        for addr in expired {
            let p = self.pending.remove(&addr).unwrap();
            if p.tested {
                tracing::warn!(client = %addr, "wrong Hyper password");
                self.lockout.fail(addr.ip(), now);
            } else {
                tracing::info!(client = %addr, "sign-in abandoned");
            }
        }
    }

    /// Handle one datagram. `offer` describes the stream for a new hello.
    pub(crate) fn handle(
        &mut self,
        socket: &UdpSocket,
        d: &[u8],
        from: SocketAddr,
        offer: impl FnOnce(&auth::Want) -> Option<Offer>,
    ) -> Option<SignedIn> {
        if let Some(v) = auth::foreign_version(d) {
            // Another rdpio build: say so (newer clients report it) instead
            // of going silent.
            tracing::warn!(client = %from, version = v, ours = rdp_hyper::wire::VERSION, "sign-in from a different rdpio version");
            let _ = socket.send_to(&AuthMsg::Denied { retry_after_s: 0 }.to_datagram(), from);
            return None;
        }
        if auth::is_auth(d) {
            let Some(AuthMsg::Hello { want, .. }) = AuthMsg::parse(d) else { return None };
            let now = Instant::now();
            if let Err(wait) = self.lockout.check(from.ip(), now) {
                tracing::warn!(client = %from, wait_s = wait.as_secs(), "sign-in refused: too many failures");
                let denied = AuthMsg::Denied { retry_after_s: wait.as_secs().clamp(1, u16::MAX as u64) as u16 };
                let _ = socket.send_to(&denied.to_datagram(), from);
                return None;
            }
            // Prove the address first: costs us nothing, keeps no state.
            let now_s = self.epoch.elapsed().as_secs();
            let valid = want
                .cookie
                .as_deref()
                .and_then(auth::cookie_from_hex)
                .is_some_and(|c| self.cookies.check(from, &c, now_s));
            if !valid {
                let _ = socket.send_to(&AuthMsg::Cookie { cookie: self.cookies.make(from, now_s) }.to_datagram(), from);
                return None;
            }
            // Bound the expensive part.
            if self.work.0.elapsed() >= Duration::from_secs(1) {
                self.work = (now, 0);
            }
            if self.work.1 >= MAX_WORK_PER_S {
                return None;
            }
            self.work.1 += 1;
            if let Some(p) = self.pending.remove(&from) {
                // The same client starting over: its unfinished attempt counts
                // if it tested a password.
                if p.tested {
                    self.lockout.fail(p.addr.ip(), now);
                }
            }
            if self.pending.len() >= MAX_PENDING
                || self.pending.keys().filter(|a| a.ip() == from.ip()).count() >= MAX_PENDING_PER_IP
            {
                return None;
            }
            // The highest protocol both speak; none in common: the client can
            // still take this host's rdpio, if it's newer (and signed).
            let protocol = rdp_hyper::update::negotiate(crate::updater::protocols(), want.protocols);
            let mut offer = match protocol {
                Some(v) => Offer { protocol: v, ..offer(&want)? },
                None => Offer { protocol: 0, codec: "h264".into(), ..Default::default() },
            };
            offer.app = crate::updater::version().into();
            offer.build = crate::updater::build().into();
            offer.update = !want.app.is_empty()
                && crate::updater::own_package().is_some_and(|p| p.manifest.newer_than(&want.app, &want.build));
            if protocol.is_none() {
                tracing::info!(client = %from, client_app = %want.app, client_protocols = ?want.protocols, update = offer.update, "sign-in from an rdpio with no protocol in common");
            } else if offer.update {
                tracing::info!(client = %from, client_app = %want.app, "the client runs an older rdpio; offering it this one");
            }
            let fps = offer.fps;
            let fit_to = want.fit.then_some((want.width, want.height));
            let (office, fast_text, scale) = (want.office, want.fast_text, want.scale);
            let monitors = want.monitors;
            let codec = rdp_gpu::h264::Codec::from_name(&offer.codec)?;
            let protocol = offer.protocol;
            let (challenge, key) = auth::host_respond(&self.secret, d, offer)?;
            let _ = socket.send_to(&challenge, from);
            tracing::info!(client = %from, "sign-in started");
            self.pending.insert(
                from,
                Pending { addr: from, key, deadline: now + Duration::from_secs(5), tested: false, fps, monitors, codec, fit_to, office, fast_text, scale, protocol },
            );
            return None;
        }
        // A sealed packet: only meaningful from a client mid-sign-in. One
        // that doesn't open is noted, not acted on — anyone can spoof the
        // client's address, and the real client's ping may still come.
        let p = self.pending.get_mut(&from)?;
        let version = if p.protocol == 0 { rdp_hyper::wire::VERSION } else { p.protocol };
        let mut recv = Session::with_version(&p.key, Direction::HostToClient, version);
        // The proof: a ping, or (a client taking an update) an update request.
        let (ping, first_update) = match recv.open(d) {
            Ok((Kind::Control, body)) => match Control::parse(&body) {
                Some(Control::Ping { id, client_us }) => (Some((id, client_us)), None),
                _ => return None,
            },
            Ok((Kind::Update, body)) => (None, Some(body)),
            _ => {
                if !p.tested {
                    p.tested = true;
                    p.deadline = p.deadline.min(Instant::now() + AFTER_FAILED_OPEN);
                }
                return None;
            }
        };
        let p = self.pending.remove(&from).unwrap();
        self.lockout.succeed(from.ip());
        tracing::info!(client = %from, update_only = p.protocol == 0, "client signed in with the Hyper password");
        let send = Arc::new(Mutex::new(Session::with_version(&p.key, Direction::HostToClient, version)));
        if let Some((id, client_us)) = ping {
            let pong = Control::Pong { id, client_us, host_us: qpc_us() };
            // Three copies: the next answer only comes once the stream is set
            // up (seconds), and a client that hears none takes it for a wrong
            // password.
            for _ in 0..3 {
                let _ = socket.send_to(&send.lock().unwrap().seal(Kind::Control, &pong.to_bytes()), from);
            }
        }
        Some(SignedIn {
            addr: from,
            recv,
            send,
            fps: p.fps,
            monitors: p.monitors,
            codec: p.codec,
            fit_to: p.fit_to,
            office: p.office,
            fast_text: p.fast_text,
            scale: p.scale,
            update_only: p.protocol == 0,
            first_update,
        })
    }
}

/// Hand a signed-in client this host's rdpio over its session, until it has
/// it (or goes quiet). Nothing else happens meanwhile: no display changes, no
/// stream.
fn serve_update(socket: &UdpSocket, mut client: SignedIn) -> std::io::Result<()> {
    use rdp_hyper::update::UpdateMsg;
    let package = crate::updater::own_package();
    let started = Instant::now();
    let mut last = Instant::now();
    let mut buf = [0u8; 2048];
    let send = client.send.clone();
    let addr = client.addr;
    let answer = |body: &[u8]| -> bool {
        let Some(msg) = UpdateMsg::parse(body) else { return true };
        if msg == UpdateMsg::Done {
            return false;
        }
        let replies = match &package {
            Some(p) => p.answer(&msg),
            None => vec![UpdateMsg::Unavailable.to_bytes()],
        };
        let mut send = send.lock().unwrap();
        for r in replies {
            let _ = socket.send_to(&send.seal(Kind::Update, &r), addr);
        }
        true
    };
    if let Some(first) = client.first_update.take() {
        answer(&first);
    }
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    while last.elapsed() < Duration::from_secs(15) && started.elapsed() < Duration::from_secs(600) && !stop_requested() {
        let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
        if from != addr {
            continue;
        }
        let Ok((Kind::Update, body)) = client.recv.open(&buf[..n]) else { continue };
        last = Instant::now();
        if !answer(&body) {
            tracing::info!(client = %addr, "the client has the update");
            break;
        }
    }
    Ok(())
}

/// `--hyper-console-agent`: sign clients in and stream the console.
pub fn run_console_agent(opts: HostOptions) -> Result<(), Box<dyn std::error::Error>> {
    watch_for_stop(STOP_EVENT, &STOP_REQUESTED);
    rdp_gpu::hyper::attach_input_desktop();
    if stop_requested() {
        // Started by a stopping service only to put back what a killed agent
        // left: no clients to serve, so no port (the killed one may still
        // hold it).
        crate::hyper_host::recover_after_crash();
        return Ok(());
    }
    let secret = load_secret()?;
    // The port doubles as the lock: only one agent streams at a time, so
    // recovering after binding can't undo another agent's live stream.
    let socket = UdpSocket::bind(("0.0.0.0", opts.port))?;
    crate::hyper_client::set_socket_buffers(&socket);
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    crate::hyper_host::recover_after_crash();
    // rdpio Display, when installed: each client gets a monitor of its own
    // size and refresh rate.
    let virtual_display = crate::virtual_display::VirtualDisplay::open();
    tracing::info!(port = opts.port, virtual_display = virtual_display.is_some(), "Hyper direct agent listening");
    let mut sign_in = SignIn::new(secret);
    // A password set while the agent runs applies from the next sign-in.
    let secret_stamp = || std::fs::metadata(secret_path()).and_then(|m| m.modified()).ok();
    let (mut stamp, mut stamp_checked) = (secret_stamp(), Instant::now());
    let mut buf = [0u8; 2048];
    // After a network drop the console stays unlocked this long so the client
    // can come back to where it was; then it locks.
    let mut grace_until: Option<Instant> = None;
    loop {
        if stop_requested() {
            if grace_until.is_some() {
                lock_session(); // a dropped client's console doesn't stay open
                crate::hyper_host::clear_streaming();
            }
            tracing::info!("the service is stopping; Hyper agent exiting");
            return Ok(());
        }
        sign_in.tick();
        if grace_until.is_some_and(|t| Instant::now() > t) {
            tracing::info!("client didn't come back; locking the console");
            grace_until = None;
            lock_session();
            crate::hyper_host::clear_streaming();
        }
        let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
        if auth::is_auth(&buf[..n]) && stamp_checked.elapsed() > Duration::from_secs(1) {
            stamp_checked = Instant::now();
            let now = secret_stamp();
            if now != stamp {
                stamp = now;
                match load_secret() {
                    Ok(s) => {
                        sign_in.set_secret(s);
                        tracing::info!("the Hyper password changed; signing in with the new one");
                    }
                    Err(e) => tracing::warn!(error = %e, "the Hyper password changed but can't be read; keeping the old one"),
                }
            }
        }
        let offer = |want: &auth::Want| {
            let monitor = rdp_gpu::hyper::list_monitors().ok()?.into_iter().next()?;
            // With the virtual display, exactly the client's screen (made
            // after sign-in); else the desktop, scaled down to fit it.
            let exact = (virtual_display.is_some() && want.fit && opts.stream_size.is_none())
                .then(|| crate::virtual_display::even_size(want.width, want.height));
            let size = exact.or_else(|| crate::hyper_host::client_stream_size(&opts, (monitor.width, monitor.height), want));
            let (width, height) = size.unwrap_or((monitor.width, monitor.height));
            Some(Offer {
                width,
                height,
                fps: opts.max_fps.map_or(want.refresh_hz, |cap| want.refresh_hz.min(cap)).max(30),
                codec: rdp_gpu::hyper::choose_codec(&want.codecs, (width, height))?.name().into(),
                // (Versions and updates: filled in by the sign-in.)
                ..Default::default()
            })
        };
        let Some(client) = sign_in.handle(&socket, &buf[..n], from, offer) else { continue };
        if client.update_only {
            serve_update(&socket, client)?;
            continue;
        }
        let monitor = rdp_gpu::hyper::list_monitors()?.into_iter().next().ok_or("no monitor")?;
        // One monitor is streamed: the others go dark meanwhile, whatever the
        // client has (nothing should open where nobody can see it).
        let _ = client.monitors;
        // The client's own display, when there's the driver for it (else the
        // host's monitor, consolidated and at the client's refresh rate) and
        // this PC's settings allow it.
        let settings = host_settings();
        let virtual_monitor = match (&virtual_display, client.fit_to) {
            (Some(vd), Some((w, h))) if opts.stream_size.is_none() && settings.virtual_display && settings.match_resolution => vd
                .show(w, h, client.fps, None)
                .inspect(|m| crate::virtual_display::set_scale(&m.device_name, client.scale))
                .inspect_err(|e| tracing::warn!(error = %e, "no virtual monitor; streaming the host's own"))
                .ok(),
            _ => None,
        };
        let device_name = virtual_monitor.as_ref().map_or_else(|| monitor.device_name.clone(), |v| v.device_name.clone());
        let consolidated =
            (virtual_monitor.is_none() && !opts.keep_monitors && !settings.keep_monitors).then(|| crate::hyper_host::Consolidation::apply(1)).flatten();
        // Else the host's monitor at the client's resolution, or the closest.
        let display = if virtual_monitor.is_none() {
            crate::hyper_host::DisplayMode::raise(&monitor.device_name, client.fps, client.fit_to.filter(|_| opts.stream_size.is_none()))
        } else {
            None
        };
        let fps = display.as_ref().map_or(client.fps, |d| d.fps);
        grace_until = None;
        // At the client's screen size (as offered), refitted if the desktop
        // changes mode mid-stream.
        let stream_opts = HostOptions { fit_to: client.fit_to, office: client.office, fast_text: client.fast_text, ..opts.clone() };
        let end = crate::hyper_host::stream(socket.try_clone()?, client.addr, client.recv, client.send, &stream_opts, &device_name, fps, client.codec, true);
        drop(display);
        drop(consolidated);
        drop(virtual_monitor);
        // The stream's marker stays until the console is locked: a crash
        // before that is still recovered (and locked) by the next agent.
        match end {
            Ok(crate::hyper_host::StreamEnd::Lost) => {
                tracing::info!("client lost; holding the session for 20 s");
                grace_until = Some(Instant::now() + RECONNECT_GRACE);
            }
            Ok(crate::hyper_host::StreamEnd::Bye) => {
                lock_session();
                crate::hyper_host::clear_streaming();
            }
            // The agent in the console's new session takes over (the client
            // reconnects to it); this one is on its way out.
            Ok(crate::hyper_host::StreamEnd::Moved) => crate::hyper_host::clear_streaming(),
            Err(e) => {
                tracing::warn!(error = %e, "stream ended with an error");
                lock_session();
                crate::hyper_host::clear_streaming();
            }
        }
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    }
}

use crate::hyper_host::lock_session;

/// How long the console stays unlocked for a client that dropped off the
/// network (it reconnects automatically).
const RECONNECT_GRACE: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the agent's sign-in loop on loopback until one client signs in.
    fn agent(secret: [u8; 32], stop: Arc<AtomicBool>) -> (SocketAddr, std::thread::JoinHandle<Option<SocketAddr>>) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let addr = socket.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut s = SignIn::new(secret);
            let mut buf = [0u8; 2048];
            let until = Instant::now() + Duration::from_secs(60);
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                s.tick();
                let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
                let offer = |_: &auth::Want| Some(Offer { width: 1280, height: 720, fps: 120, codec: "h264".into(), ..Default::default() });
                if let Some(c) = s.handle(&socket, &buf[..n], from, offer) {
                    return Some(c.addr);
                }
            }
            None
        });
        (addr, t)
    }

    fn want() -> auth::Want {
        auth::Want {
            codecs: vec!["h264".into()],
            width: 1920,
            height: 1080,
            refresh_hz: 144,
            monitors: 1,
            app: crate::updater::version().into(),
            build: crate::updater::build().into(),
            protocols: crate::updater::protocols(),
            ..Default::default()
        }
    }

    #[test]
    fn hellos_without_a_cookie_cost_nothing_and_lock_nobody_out() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut s = SignIn::new(auth::password_secret("correct horse battery staple"));
        let (_, hello) = auth::ClientAuth::start(&auth::password_secret("guess"), want());
        let victim: SocketAddr = "127.0.0.1:9".parse().unwrap();
        for _ in 0..100 {
            let offer = |_: &auth::Want| Some(Offer { width: 1, height: 1, fps: 60, codec: "h264".into(), ..Default::default() });
            assert!(s.handle(&host, &hello, victim, offer).is_none());
        }
        s.tick();
        assert!(s.pending.is_empty());
        assert!(s.lockout.check(victim.ip(), Instant::now()).is_ok(), "spoofing can't lock the victim out");
    }

    #[test]
    fn spoofed_garbage_mid_sign_in_neither_aborts_nor_locks_out() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let from = client.local_addr().unwrap();
        let secret = auth::password_secret("correct horse battery staple");
        let mut s = SignIn::new(secret);
        let offer = |_: &auth::Want| Some(Offer { width: 1, height: 1, fps: 60, codec: "h264".into(), ..Default::default() });
        // Get a cookie, then a challenge.
        let (_, hello) = auth::ClientAuth::start(&secret, want());
        assert!(s.handle(&host, &hello, from, offer).is_none());
        let mut buf = [0u8; 2048];
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let n = client.recv(&mut buf).unwrap();
        let Some(AuthMsg::Cookie { cookie }) = AuthMsg::parse(&buf[..n]) else { panic!("no cookie") };
        let mut w = want();
        w.cookie = Some(auth::cookie_to_hex(&cookie));
        let (ca, hello) = auth::ClientAuth::start(&secret, w);
        assert!(s.handle(&host, &hello, from, offer).is_none());
        let n = client.recv(&mut buf).unwrap();
        let key = ca.finish(&buf[..n]).expect("challenge");
        // An attacker spoofs junk from the client's address…
        for junk in [&[1u8][..], &[0xD7; 40][..]] {
            assert!(s.handle(&host, junk, from, offer).is_none());
        }
        // …and the real ping still signs in.
        let mut send = Session::new(&key, Direction::ClientToHost);
        let ping = send.seal(Kind::Control, &Control::Ping { id: 1, client_us: 0 }.to_bytes());
        assert!(s.handle(&host, &ping, from, offer).is_some());
        assert!(s.lockout.check(from.ip(), Instant::now()).is_ok());
    }

    /// A build with no protocol in common still signs in: to an update-only
    /// session, opened by an update request, that never starts a stream.
    #[test]
    fn a_build_with_no_protocol_in_common_signs_in_to_take_an_update() {
        use rdp_hyper::update::UpdateMsg;
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let from = client.local_addr().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let secret = auth::password_secret("pw");
        let mut s = SignIn::new(secret);
        let offer = |_: &auth::Want| -> Option<Offer> { panic!("an update-only sign-in asks for no stream") };
        let future = || auth::Want { protocols: (200, 201), app: "9.0.0".into(), ..want() };
        let (_, hello) = auth::ClientAuth::start(&secret, future());
        assert!(s.handle(&host, &hello, from, offer).is_none());
        let mut buf = [0u8; 2048];
        let n = client.recv(&mut buf).unwrap();
        let Some(AuthMsg::Cookie { cookie }) = AuthMsg::parse(&buf[..n]) else { panic!("no cookie") };
        let (ca, hello) = auth::ClientAuth::start(&secret, auth::Want { cookie: Some(auth::cookie_to_hex(&cookie)), ..future() });
        assert!(s.handle(&host, &hello, from, offer).is_none());
        let n = client.recv(&mut buf).unwrap();
        let Some(AuthMsg::Challenge { offer: o, .. }) = AuthMsg::parse(&buf[..n]) else { panic!("no challenge") };
        assert_eq!(o.protocol, 0, "nothing in common");
        assert!(!o.update, "the client is newer: nothing to give it");
        assert_eq!(o.app, crate::updater::version());
        let key = ca.finish(&buf[..n]).expect("key");
        // A ping in its own protocol doesn't open here; an update request does.
        let mut send = Session::with_version(&key, Direction::ClientToHost, 200);
        let ping = send.seal(Kind::Control, &Control::Ping { id: 1, client_us: 0 }.to_bytes());
        assert!(s.handle(&host, &ping, from, offer).is_none());
        let ask = send.seal(Kind::Update, &UpdateMsg::GetManifest.to_bytes());
        let signed = s.handle(&host, &ask, from, offer).expect("signed in");
        assert!(signed.update_only);
        assert_eq!(signed.first_update.as_deref(), Some(&UpdateMsg::GetManifest.to_bytes()[..]));
        // The host has no package (a test build): it says so.
        let host_addr = host.local_addr().unwrap();
        let t = std::thread::spawn(move || serve_update(&host, signed));
        let mut recv = Session::with_version(&key, Direction::ClientToHost, 200);
        let n = client.recv(&mut buf).unwrap();
        let (kind, body) = recv.open(&buf[..n]).unwrap();
        assert_eq!((kind, UpdateMsg::parse(&body)), (Kind::Update, Some(UpdateMsg::Unavailable)));
        // Done ends it.
        client.send_to(&send.seal(Kind::Update, &UpdateMsg::Done.to_bytes()), host_addr).unwrap();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn the_right_password_signs_in() {
        let (addr, t) = agent(auth::password_secret("correct horse battery staple"), Default::default());
        let link = crate::hyper_client::connect_direct(addr, "correct horse battery staple", want());
        assert!(link.is_ok(), "{:?}", link.err());
        assert!(t.join().unwrap().is_some());
    }

    #[test]
    fn a_wrong_password_is_refused_and_repeated_tries_lock_out() {
        let stop = Arc::new(AtomicBool::new(false));
        let (addr, t) = agent(auth::password_secret("correct horse battery staple"), stop.clone());
        let mut errors = Vec::new();
        for _ in 0..6 {
            errors.push(crate::hyper_client::connect_direct(addr, "wrong password guess", want()).err().unwrap());
        }
        assert!(errors[0].contains("wrong Hyper password"), "{errors:?}");
        assert!(errors.last().unwrap().contains("refusing sign-ins"), "{errors:?}");
        // Even the right password is refused while locked out.
        let e = crate::hyper_client::connect_direct(addr, "correct horse battery staple", want()).err().unwrap();
        assert!(e.contains("refusing sign-ins"), "{e}");
        stop.store(true, Ordering::Relaxed);
        assert!(t.join().unwrap().is_none());
    }

    #[test]
    fn system_tools_are_run_by_absolute_path_from_system32() {
        for tool in ["sc.exe", "icacls.exe", "reg.exe", "netsh.exe", "schtasks.exe", r"WindowsPowerShell\v1.0\powershell.exe"] {
            let path = system_tool(tool);
            assert!(path.is_absolute() && path.is_file(), "{}", path.display());
            assert!(path.to_string_lossy().to_ascii_lowercase().contains(r"\system32\"), "{}", path.display());
        }
    }

    #[test]
    fn the_installed_copy_is_refreshed_only_when_the_bytes_differ() {
        let dir = std::env::temp_dir().join(format!("rdpio-same-contents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let a = file("a", b"rdpio build 1");
        assert!(same_contents(&a, &file("b", b"rdpio build 1")));
        assert!(!same_contents(&a, &file("c", b"rdpio build 2")), "same size, other bytes");
        assert!(!same_contents(&a, &file("d", b"rdpio build 10")), "other size");
        assert!(!same_contents(&a, &dir.join("missing")));
        assert!(!same_contents(&dir.join("missing"), &dir.join("missing too")), "nothing to compare isn't equal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn elevated() -> bool {
        unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
    }

    #[test]
    fn only_a_secret_owned_by_system_or_administrators_is_trusted() {
        use std::os::windows::fs::OpenOptionsExt;
        // ProgramData belongs to SYSTEM (opened as a folder, to read its owner).
        let program_data = known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramData, None).unwrap();
        let folder = std::fs::OpenOptions::new()
            .access_mode(0x0002_0000) // READ_CONTROL
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(&program_data)
            .unwrap();
        assert!(owned_by_admins(&folder));
        // A file a user made: what a planted secret would be.
        let planted = std::env::temp_dir().join(format!("rdpio-planted-secret-{}", std::process::id()));
        std::fs::write(&planted, b"a password of someone's choosing").unwrap();
        if !elevated() {
            assert!(!owned_by_admins(&std::fs::File::open(&planted).unwrap()));
        }
        let _ = std::fs::remove_file(&planted);
    }

    #[test]
    fn the_secret_file_is_closed_to_others_from_the_moment_it_exists() {
        let path = std::env::temp_dir().join(format!("rdpio-secret-{}", std::process::id()));
        std::fs::write(&path, b"planted first").unwrap();
        // write_secret's ACL, minus the owner (setting that needs elevation).
        write_new_file(&path, b"the secret", w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)")).expect("replaced, not written through");
        if !elevated() {
            let denied = std::fs::read(&path).unwrap_err();
            assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_a_service_that_isnt_installed_does_nothing() {
        assert_eq!(remove_named_service("rdpioHyperTestNoSuchService"), Ok(false));
    }

    #[test]
    fn only_system_can_open_the_stop_event() {
        use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};
        // A session-local name: creating Global ones takes a privilege tests lack.
        let name = wide(&format!(r"Local\rdpioHyperStopTest-{}", std::process::id()));
        let ev = unsafe { create_stop_event(PCWSTR(name.as_ptr())) }.expect("create the event");
        let opened = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) };
        assert!(opened.is_err(), "anyone but SYSTEM was let in");
        unsafe {
            let _ = CloseHandle(ev);
        }
    }

    #[test]
    fn the_agent_hears_the_stop_event_whenever_it_is_set() {
        use windows::Win32::System::Threading::{CreateEventW, SetEvent};
        static SET_BEFORE: AtomicBool = AtomicBool::new(false);
        static SET_LATER: AtomicBool = AtomicBool::new(false);
        static NO_EVENT: AtomicBool = AtomicBool::new(false);
        let name = |what: &str| wide(&format!(r"Local\rdpioHyperStop{what}-{}", std::process::id()));
        let (before, later, missing) = (name("Before"), name("Later"), name("Missing"));
        unsafe {
            // Set before the agent looks (an agent started only to recover).
            let ev_before = CreateEventW(None, true, true, PCWSTR(before.as_ptr())).unwrap();
            watch_for_stop(PCWSTR(before.as_ptr()), &SET_BEFORE);
            assert!(SET_BEFORE.load(Ordering::SeqCst), "must be known on return");
            // Set while it runs.
            let ev_later = CreateEventW(None, true, false, PCWSTR(later.as_ptr())).unwrap();
            watch_for_stop(PCWSTR(later.as_ptr()), &SET_LATER);
            assert!(!SET_LATER.load(Ordering::SeqCst));
            SetEvent(ev_later).unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            while !SET_LATER.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(SET_LATER.load(Ordering::SeqCst));
            let _ = CloseHandle(ev_before);
            let _ = CloseHandle(ev_later);
        }
        // Not started by the service: nothing to hear, nothing raised.
        watch_for_stop(PCWSTR(missing.as_ptr()), &NO_EVENT);
        assert!(!NO_EVENT.load(Ordering::SeqCst));
    }
}
