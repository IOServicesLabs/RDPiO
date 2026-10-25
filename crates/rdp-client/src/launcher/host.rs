//! The launcher's "This PC" page: whether the Hyper service is installed and
//! running here, whether a Hyper password is set, the addresses other PCs can
//! reach this one at, and the administrator actions (install, update, remove,
//! set the password) run through an elevated copy of rdpio.

use std::io::{Read, Write};
use std::net::Ipv4Addr;

use serde::Serialize;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};

#[derive(Debug, Serialize)]
pub struct Address {
    pub name: String,
    pub ip: String,
    /// The network's prefix length (24 for a /24).
    pub prefix: u8,
    /// "tailscale", "zerotier", "lan" or "other".
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Status {
    /// "running", "stopped", "starting", "stopping", "missing" or "unknown".
    pub service: &'static str,
    pub password_set: bool,
    /// The service runs an older rdpio than this launcher (or an unsigned
    /// other build).
    pub outdated: bool,
    /// The service's rdpio, when it's a signed release.
    pub service_version: Option<String>,
    pub port: u16,
    pub computer: String,
    pub addresses: Vec<Address>,
    pub gpu: Option<String>,
    /// The virtual display driver: "running", "installed" (not running:
    /// test signing off, or a restart due), "fallback" (the open-source
    /// Virtual Display Driver is used instead), "missing"; and whether this
    /// rdpio carries one to install.
    pub display: &'static str,
    pub display_available: bool,
    /// How this PC streams (its This PC page switches).
    pub settings: crate::hyper_direct::HostSettings,
}

pub fn status() -> Status {
    let service = service_state();
    Status {
        service,
        password_set: password_set(),
        outdated: service != "missing" && outdated(),
        service_version: installed_exe().and_then(|e| crate::updater::package_of(&e).ok()).map(|(_, _, m)| m.version),
        port: rdp_hyper::DEFAULT_PORT,
        computer: std::env::var("COMPUTERNAME").unwrap_or_default(),
        addresses: addresses(),
        gpu: gpu_name(),
        display: if crate::virtual_display::running() {
            "running"
        } else if crate::virtual_display::installed() {
            "installed"
        } else if crate::virtual_display::fallback_installed() {
            "fallback"
        } else {
            "missing"
        },
        display_available: std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("rdpio-display").join("rdpio-display.inf")))
            .is_some_and(|p| p.exists()),
        settings: crate::hyper_direct::host_settings(),
    }
}

fn service_state() -> &'static str {
    use windows::Win32::System::Services::*;
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return "unknown";
        };
        let name = HSTRING::from(crate::hyper_direct::SERVICE);
        let state = match OpenServiceW(scm, &name, SERVICE_QUERY_STATUS) {
            Err(_) => "missing",
            Ok(svc) => {
                let mut st = SERVICE_STATUS::default();
                let s = if QueryServiceStatus(svc, &mut st).is_ok() {
                    match st.dwCurrentState {
                        SERVICE_RUNNING => "running",
                        SERVICE_START_PENDING | SERVICE_CONTINUE_PENDING => "starting",
                        SERVICE_STOP_PENDING | SERVICE_PAUSE_PENDING => "stopping",
                        _ => "stopped",
                    }
                } else {
                    "unknown"
                };
                let _ = CloseServiceHandle(svc);
                s
            }
        };
        let _ = CloseServiceHandle(scm);
        state
    }
}

/// The secret file's there (users may list the folder, not read the file).
fn password_set() -> bool {
    let dir = crate::hyper_direct::data_dir();
    std::fs::read_dir(dir).is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name() == "hyper.secret")))
}

fn installed_exe() -> Option<std::path::PathBuf> {
    use windows::Win32::UI::Shell::FOLDERID_ProgramFiles;
    let exe = crate::hyper_direct::known_folder(&FOLDERID_ProgramFiles, None)?.join("rdpio").join("rdpio.exe");
    exe.exists().then_some(exe)
}

/// Whether the service runs an older rdpio than this launcher. (A newer one
/// — it updated itself — isn't: "updating" it would go back.) Builds that
/// aren't signed releases compare by their bytes.
fn outdated() -> bool {
    let (Some(installed), Ok(me)) = (installed_exe(), std::env::current_exe()) else {
        return false;
    };
    if crate::hyper_direct::same_contents(&me, &installed) {
        return false;
    }
    match crate::updater::package_of(&installed) {
        Ok((_, _, m)) => !m.newer_than(crate::updater::version(), crate::updater::build()) && rdp_hyper::update::newer((crate::updater::version(), crate::updater::build()), (&m.version, &m.build)),
        Err(_) => true,
    }
}

fn gpu_name() -> Option<String> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            return None;
        }
        let n = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        Some(String::from_utf16_lossy(&desc.Description[..n]))
    }
}

/// What kind of network an address is on (for the "connect from" hints).
/// `routed`: the adapter has a default gateway (a real network, not a
/// virtual machine's or WSL's private switch).
pub fn kind_of(ip: Ipv4Addr, adapter: &str, routed: bool) -> &'static str {
    let o = ip.octets();
    let lower = adapter.to_ascii_lowercase();
    if lower.contains("tailscale") || (o[0] == 100 && (64..128).contains(&o[1])) {
        "tailscale"
    } else if lower.contains("zerotier") {
        "zerotier"
    } else if ["vmware", "virtualbox", "vethernet", "hyper-v", "wsl", "docker", "loopback"].iter().any(|v| lower.contains(v)) || !routed {
        "virtual"
    } else if ip.is_private() {
        "lan"
    } else {
        "other"
    }
}

/// This PC's IPv4 addresses on adapters that are up (loopback and
/// link-local left out), Tailscale first, then the LAN.
pub fn addresses() -> Vec<Address> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    let mut out = Vec::new();
    let mut size: u32 = 32 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0u64; size as usize / 8 + 1];
        let r = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_INCLUDE_GATEWAYS,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: `size` now says how much
            _ => return out,
        }
    }
    unsafe {
        let mut a = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !a.is_null() {
            let ad = &*a;
            a = ad.Next;
            if ad.OperStatus != IfOperStatusUp || ad.IfType == 24 {
                continue; // down, or loopback
            }
            let name = ad.FriendlyName.to_string().unwrap_or_default();
            let routed = !ad.FirstGatewayAddress.is_null();
            let mut u = ad.FirstUnicastAddress;
            while !u.is_null() {
                let ua = &*u;
                u = ua.Next;
                let sa = ua.Address.lpSockaddr;
                if sa.is_null() || (*sa).sa_family != AF_INET {
                    continue;
                }
                let sin = &*(sa as *const SOCKADDR_IN);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                if ip.is_loopback() || ip.is_link_local() {
                    continue;
                }
                let kind = kind_of(ip, &name, routed);
                // (Virtual machines' and WSL's switches: no other PC reaches those.)
                if kind != "virtual" {
                    out.push(Address { kind, name: name.clone(), ip: ip.to_string(), prefix: ua.OnLinkPrefixLength });
                }
            }
        }
    }
    let rank = |k: &str| match k {
        "tailscale" => 0,
        "lan" => 1,
        "zerotier" => 2,
        _ => 3,
    };
    out.sort_by_key(|a| rank(a.kind));
    out
}

/// An administrator action, run by an elevated copy of this rdpio (Windows
/// asks the user first). The new Hyper password, when there is one, goes to
/// it through a private pipe — never on its command line, which other
/// programs can read. Blocks until it finishes (call it off the UI thread):
/// `Ok(None)` when it worked, `Ok(Some(why))` when it ran and failed,
/// `Err` when it never ran (the user said no).
pub fn run_elevated(args: &[&str], password: Option<String>, owner: HWND) -> Result<Option<String>, String> {
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let pipe = Pipe::new().map_err(|e| format!("could not open a pipe to the elevated rdpio: {e}"))?;
    let mut params: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    params.push("--launcher-pipe".into());
    params.push(pipe.name.clone());
    let params = HSTRING::from(params.join(" "));
    let file = HSTRING::from(exe.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        hwnd: owner,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|_| "Windows didn't allow the change (the administrator prompt was declined)".to_string())?;
    let process = info.hProcess;
    // The elevated copy reads the password (if any), and writes back why it
    // failed (if it does).
    let report = pipe.serve(password.as_deref().map(str::as_bytes), process);
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(process, INFINITE);
        let _ = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
    }
    Ok(match (code, report) {
        (0, _) => None,
        (_, Some(why)) if !why.trim().is_empty() => Some(why.trim().to_string()),
        (c, _) => Some(format!("it stopped with code {c}")),
    })
}

/// A one-connection duplex pipe the launcher serves to its elevated copy.
struct Pipe {
    name: String,
    handle: HANDLE,
}

impl Pipe {
    fn new() -> windows::core::Result<Self> {
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = format!("rdpio-launcher-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
        let path = HSTRING::from(format!(r"\\.\pipe\{name}"));
        // Only this user (its elevated copy too) and Administrators: a pipe's
        // default access lets anyone read, and pipe names can be listed.
        let handle = crate::hyper_direct::with_security(w!("D:P(A;;GA;;;OW)(A;;GA;;;BA)(A;;GA;;;SY)"), |sa| unsafe {
            CreateNamedPipeW(
                &path,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                Some(sa),
            )
        })?;
        if handle.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { name, handle })
    }

    /// Wait for the elevated copy to connect (or for it to end without doing
    /// so), hand it `secret` (length-prefixed; `u32::MAX` for none), and read
    /// its report to EOF.
    fn serve(self, secret: Option<&[u8]>, process: HANDLE) -> Option<String> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::{ERROR_IO_PENDING, ERROR_PIPE_CONNECTED};
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};
        use windows::Win32::System::IO::{CancelIo, OVERLAPPED};
        unsafe {
            let event = CreateEventW(None, true, false, PCWSTR::null()).ok()?;
            let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
            let connected = match ConnectNamedPipe(self.handle, Some(&mut ov)) {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    // Connected, or the elevated copy ended first.
                    let which = WaitForMultipleObjects(&[event, process], false, INFINITE);
                    if which.0 == 0 {
                        true
                    } else {
                        let _ = CancelIo(self.handle);
                        false
                    }
                }
                Err(_) => false,
            };
            let _ = CloseHandle(event);
            // Only the copy we started gets the password.
            let mut client = 0u32;
            let ours = connected
                && windows::Win32::System::Pipes::GetNamedPipeClientProcessId(self.handle, &mut client).is_ok()
                && client == windows::Win32::System::Threading::GetProcessId(process);
            if !ours {
                if connected {
                    tracing::warn!(client, "another program connected to the launcher's pipe; refused");
                }
                let _ = CloseHandle(self.handle);
                return None;
            }
            // Blocking I/O from here (the overlapped flag only matters to the
            // connect; a file opened that way still reads and writes in order).
            let mut file = std::fs::File::from_raw_handle(self.handle.0);
            let mut msg = secret.map_or(u32::MAX, |s| s.len() as u32).to_le_bytes().to_vec();
            msg.extend_from_slice(secret.unwrap_or_default());
            let _ = file.write_all(&msg);
            let _ = file.flush();
            let mut report = String::new();
            let _ = file.read_to_string(&mut report);
            Some(report)
        }
    }
}

/// In the elevated copy: what the launcher sent over `--launcher-pipe`, and the
/// pipe to report back on.
pub struct LauncherPipe {
    file: std::fs::File,
}

impl LauncherPipe {
    pub fn open(name: &str) -> std::io::Result<(Self, Option<String>)> {
        if !name.starts_with("rdpio-launcher-") || name.contains(['\\', '/']) {
            return Err(std::io::Error::other("not a launcher pipe"));
        }
        // (By CreateFileW: std's open doesn't find a `\\.\pipe\` path here.)
        let file = unsafe {
            use std::os::windows::io::FromRawHandle;
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
            };
            let path = HSTRING::from(format!(r"\\.\pipe\{name}"));
            let h = CreateFileW(&path, (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            std::fs::File::from_raw_handle(h.0)
        };
        let mut file = file;
        let mut len = [0u8; 4];
        file.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len);
        if len == u32::MAX {
            return Ok((Self { file }, None));
        }
        let len = len as usize;
        if len > 4096 {
            return Err(std::io::Error::other("launcher message too long"));
        }
        let mut secret = vec![0u8; len];
        file.read_exact(&mut secret)?;
        let secret = String::from_utf8(secret).map_err(|_| std::io::Error::other("launcher message isn't text"))?;
        Ok((Self { file }, Some(secret)))
    }

    pub fn report(&mut self, what: &str) {
        let _ = self.file.write_all(what.as_bytes());
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_classified() {
        assert_eq!(kind_of(Ipv4Addr::new(100, 101, 5, 9), "Ethernet 3", false), "tailscale");
        assert_eq!(kind_of(Ipv4Addr::new(10, 1, 2, 3), "Tailscale", false), "tailscale");
        assert_eq!(kind_of(Ipv4Addr::new(192, 168, 1, 20), "Wi-Fi", true), "lan");
        assert_eq!(kind_of(Ipv4Addr::new(10, 147, 17, 3), "ZeroTier One [abcd]", false), "zerotier");
        assert_eq!(kind_of(Ipv4Addr::new(100, 20, 1, 1), "Ethernet", true), "other");
        assert_eq!(kind_of(Ipv4Addr::new(192, 168, 123, 1), "VMware Network Adapter VMnet1", false), "virtual");
        assert_eq!(kind_of(Ipv4Addr::new(172, 20, 0, 1), "vEthernet (WSL)", true), "virtual");
        assert_eq!(kind_of(Ipv4Addr::new(192, 168, 50, 2), "Ethernet 2", false), "virtual");
    }

    /// The launcher's pipe hands its elevated copy the password and reads
    /// the report back (both ends in this process; the "elevated copy" is a
    /// thread, and the process to watch is this one, which never ends).
    #[test]
    fn the_pipe_carries_the_password_and_the_report() {
        let pipe = Pipe::new().unwrap();
        let name = pipe.name.clone();
        let child = std::thread::spawn(move || {
            let (mut p, secret) = LauncherPipe::open(&name).unwrap();
            p.report(&format!("got {secret:?}"));
        });
        // (A real handle: a wait on the pseudo-handle fails at once. The
        // "elevated copy" is a thread here, so the client is this process.)
        let me = unsafe {
            use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
            OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, GetCurrentProcessId()).unwrap()
        };
        let report = pipe.serve(Some("hunter2 ü".as_bytes()), me);
        child.join().unwrap();
        assert_eq!(report.as_deref(), Some("got Some(\"hunter2 ü\")"));
        // "No password" isn't an empty one.
        let pipe = Pipe::new().unwrap();
        let name = pipe.name.clone();
        let child = std::thread::spawn(move || LauncherPipe::open(&name).unwrap().1);
        let _ = pipe.serve(None, me);
        assert_eq!(child.join().unwrap(), None);
        assert!(LauncherPipe::open(r"..\evil").is_err());
    }

    /// Anyone but the copy the launcher started is refused the password.
    #[test]
    fn the_pipe_refuses_other_programs() {
        use std::os::windows::io::AsRawHandle;
        // The launcher "started" this process; the thread below isn't it.
        let mut other = std::process::Command::new(crate::hyper_direct::system_tool("ping.exe"))
            .args(["-n", "4", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pipe = Pipe::new().unwrap();
        let name = pipe.name.clone();
        let intruder = std::thread::spawn(move || {
            let (_p, secret) = LauncherPipe::open(&name).ok()?;
            secret
        });
        let report = pipe.serve(Some(b"secret"), HANDLE(other.as_raw_handle()));
        assert_eq!(report, None);
        assert_eq!(intruder.join().unwrap(), None, "the intruder read the password");
        let _ = other.kill();
    }

    #[test]
    fn this_pc_has_an_address() {
        // (Any machine running the tests is on some network.)
        let a = addresses();
        assert!(a.iter().all(|a| !a.ip.starts_with("127.")), "{a:?}");
    }
}
