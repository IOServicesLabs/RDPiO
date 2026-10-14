//! Keeping rdpio up to date.
//!
//! Packages are signed releases (see [`rdp_hyper::update`]): `rdpio.exe` plus
//! its `rdpio.exe.update` manifest beside it. They come from GitHub (the
//! launcher's check, the host service's own) or from the host a client signs
//! in to, which hands over its own package. Nothing is installed unless the
//! release key signed it and it is newer than what runs.
//!
//! Installing: a package is staged in `%LOCALAPPDATA%\rdpio\updates`, then
//! the staged exe itself puts itself in place of the old one
//! (`--finish-update`, once the old process has exited: an exe can't
//! overwrite itself) and starts whatever should run next. A folder the user
//! can't write (Program Files) gets an administrator prompt.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rdp_hyper::update::{Manifest, Package, Serve, Signed};

/// Where releases are published.
pub const REPO: &str = "IOServicesLabs/RDPiO";
const EXE_ASSET: &str = "rdpio.exe";
const MANIFEST_ASSET: &str = "rdpio.exe.update";

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn build() -> &'static str {
    env!("RDPIO_BUILD")
}

/// The Hyper protocol versions this build speaks, oldest and newest.
pub fn protocols() -> (u8, u8) {
    (rdp_hyper::wire::MIN_VERSION, rdp_hyper::wire::VERSION)
}

/// This PC's update preferences (the launcher's Settings).
pub fn prefs() -> crate::launcher::profile::UpdatePrefs {
    crate::launcher::profile::Store::load().map(|s| s.file.updates).unwrap_or_default()
}

/// Whether to take a newer rdpio from a Hyper host (Settings, and
/// `RDPIO_NO_UPDATE` to say no).
pub fn updates_from_hosts() -> bool {
    std::env::var_os("RDPIO_NO_UPDATE").is_none() && prefs().from_hosts
}

/// The rdpio now at this exe's path, when it is newer than the one running
/// (installed while this ran): its version.
pub fn replaced_by_newer() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let signed = std::fs::read(sidecar(&exe)).ok()?;
    let m = Signed::parse(&signed).and_then(|s| s.verify()).ok()?;
    m.newer_than(version(), build()).then_some(m.version)
}

/// `rdpio.exe` → `rdpio.exe.update`.
pub fn sidecar(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().map(OsString::from).unwrap_or_default();
    name.push(".update");
    exe.with_file_name(name)
}

/// The package of the exe at `exe`: its signed manifest (beside it), checked
/// against the release key and the exe's bytes.
pub fn package_of(exe: &Path) -> Result<(Vec<u8>, Vec<u8>, Manifest), String> {
    let signed = std::fs::read(sidecar(exe)).map_err(|_| "no signed manifest beside it (a local build?)".to_string())?;
    let bytes = std::fs::read(exe).map_err(|e| e.to_string())?;
    let m = Signed::parse(&signed).and_then(|s| s.verify()).map_err(|e| e.to_string())?;
    if !m.matches(&bytes) {
        return Err("the manifest beside it is for another build".into());
    }
    Ok((signed, bytes, m))
}

/// This rdpio's own package, to hand to clients — when it is a signed
/// release (a local build has none, and can't pass itself on).
pub fn own_package() -> Option<Arc<Serve>> {
    static OWN: OnceLock<Option<Arc<Serve>>> = OnceLock::new();
    OWN.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        match package_of(&exe).and_then(|(signed, bytes, _)| Serve::new(signed, bytes).map_err(|e| e.to_string())) {
            Ok(s) => {
                tracing::info!(version = %s.manifest.version, build = %s.manifest.build, "this rdpio can update the clients that connect");
                Some(Arc::new(s))
            }
            Err(why) => {
                tracing::info!(%why, "this rdpio can't pass itself on to clients");
                None
            }
        }
    })
    .clone()
}

/// `%LOCALAPPDATA%\rdpio\updates`.
fn staging_dir() -> Option<PathBuf> {
    let dir = crate::hub::store::data_dir().ok()?.join("updates");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Keep a downloaded package until it's installed: the staged exe's path.
pub fn stage(p: &Package) -> std::io::Result<PathBuf> {
    let dir = staging_dir().ok_or_else(|| std::io::Error::other("no folder for updates"))?;
    let stamp: String = p.manifest.build.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let exe = dir.join(format!("rdpio-{}-{stamp}.exe", p.manifest.version));
    let tmp = exe.with_extension("part");
    std::fs::write(&tmp, &p.exe)?;
    std::fs::rename(&tmp, &exe)?;
    std::fs::write(sidecar(&exe), &p.signed)?;
    tracing::info!(version = %p.manifest.version, build = %p.manifest.build, path = %exe.display(), "update downloaded");
    Ok(exe)
}

/// The newest staged package newer than this rdpio (checked again: the
/// folder is the user's). Older ones are deleted.
pub fn staged() -> Option<(PathBuf, Manifest)> {
    let dir = staging_dir()?;
    let mut best: Option<(PathBuf, Manifest)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("exe") {
            continue;
        }
        let usable = package_of(&path).ok().map(|(_, _, m)| m).filter(|m| m.newer_than(version(), build()));
        match usable {
            Some(m) if best.as_ref().is_none_or(|(_, b)| m.newer_than(&b.version, &b.build)) => {
                if let Some((old, _)) = best.replace((path, m)) {
                    remove_package(&old);
                }
            }
            _ => remove_package(&path),
        }
    }
    best
}

fn remove_package(exe: &Path) {
    let _ = std::fs::remove_file(exe);
    let _ = std::fs::remove_file(sidecar(exe));
}

/// A process of ours started detached from any console: no window of its
/// own, and none of ours to inherit. The launcher has let its console go
/// (`FreeConsole`), and handing a child those dead standard handles fails
/// the start ("the handle is invalid").
pub fn detached(exe: &Path) -> std::process::Command {
    use std::os::windows::process::CommandExt as _;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Install a staged package over this rdpio, then run `then` (arguments for
/// the new exe; `None`: nothing). Call, then exit: the new exe waits for this
/// process to end.
pub fn install(staged_exe: &Path, then: Option<Vec<OsString>>) -> std::io::Result<()> {
    let target = std::env::current_exe()?;
    let mut cmd = detached(staged_exe);
    cmd.arg("--finish-update").arg("--target").arg(&target).arg("--wait").arg(std::process::id().to_string());
    if let Some(args) = then {
        cmd.arg("--then").args(args);
    }
    tracing::info!(from = %staged_exe.display(), to = %target.display(), "installing the update");
    cmd.spawn()?;
    Ok(())
}

/// `--finish-update --target T --wait PID [--elevated] [--then ARGS…]`: run by
/// the staged exe. Waits for PID, puts itself (and its manifest) in place of
/// T, then starts T with ARGS.
pub fn finish_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut target = None;
    let mut wait = None;
    let mut elevated = false;
    let mut then: Option<Vec<String>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--target" => {
                target = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--wait" => {
                wait = args.get(i + 1).and_then(|p| p.parse::<u32>().ok());
                i += 1;
            }
            "--elevated" => elevated = true,
            "--then" => {
                then = Some(args[i + 1..].to_vec());
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let target = target.ok_or("--finish-update needs --target")?;
    if let Some(pid) = wait {
        wait_for_exit(pid, Duration::from_secs(60));
    }
    let me = std::env::current_exe()?;
    // Only a signed package puts itself anywhere.
    let (signed, _, m) = package_of(&me)?;
    match replace_exe(&target, &me, &signed) {
        Ok(()) => tracing::info!(version = %m.version, build = %m.build, target = %target.display(), "update installed"),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !elevated => {
            // A folder only administrators change: ask once, then carry on here.
            let mut again: Vec<String> = vec!["--finish-update".into(), "--target".into(), target.display().to_string(), "--elevated".into()];
            again = again.into_iter().map(|a| quote(&a)).collect();
            run_as_admin(&me, &again.join(" "))?;
        }
        Err(e) => return Err(format!("couldn't put the update in place at {}: {e}", target.display()).into()),
    }
    if let (false, Some(args)) = (elevated, then) {
        detached(&target).args(args).spawn()?;
    }
    Ok(())
}

fn quote(a: &str) -> String {
    if a.contains([' ', '\t', '"']) {
        format!("\"{}\"", a.replace('"', "\\\""))
    } else {
        a.to_string()
    }
}

/// Run `exe` elevated (Windows asks) and wait for it.
fn run_as_admin(exe: &Path, params: &str) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|_| "the administrator prompt was declined; the update wasn't installed".to_string())?;
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(format!("the update didn't install (code {code})"))
    }
}

fn wait_for_exit(pid: u32, limit: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, limit.as_millis() as u32);
            let _ = CloseHandle(h);
        }
    }
}

/// Put `new` (and its manifest) at `target`. A running exe can't be
/// overwritten, only moved aside (other rdpio windows keep running the old
/// one; it's deleted later, see [`cleanup`]).
pub fn replace_exe(target: &Path, new: &Path, signed: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    let staged = target.with_extension("exe.new");
    std::fs::copy(new, &staged)?;
    if std::fs::rename(&staged, target).is_err() {
        let aside = dir.join(format!("{}.{}.old", target.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
        let swapped = std::fs::rename(target, &aside).and_then(|()| {
            std::fs::rename(&staged, target).inspect_err(|_| {
                let _ = std::fs::rename(&aside, target);
            })
        });
        if let Err(e) = swapped {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    }
    std::fs::write(sidecar(target), signed)?;
    Ok(())
}

/// Delete what earlier updates left: old exes moved aside beside this one,
/// and staged packages that aren't newer.
pub fn cleanup() {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(".old") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let _ = staged();
}

// ------------------------------------------------------------ GitHub

/// A newer release on GitHub.
#[derive(Debug, Clone)]
pub struct Remote {
    pub signed: Vec<u8>,
    pub manifest: Manifest,
    pub exe_url: String,
    /// The release's page.
    pub page: String,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(30)).build()
}

/// The latest release on `channel` ("stable", or "nightly": every build of
/// main), if it is newer than this rdpio.
pub fn check_github(channel: &str) -> Result<Option<Remote>, String> {
    let api = match channel {
        "nightly" => format!("https://api.github.com/repos/{REPO}/releases/tags/nightly"),
        _ => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let ua = format!("rdpio/{} (+https://github.com/{REPO})", version());
    let release: serde_json::Value = agent()
        .get(&api)
        .set("User-Agent", &ua)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?
        .into_json()
        .map_err(|e| e.to_string())?;
    let asset = |name: &str| {
        release["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == name))
            .and_then(|x| x["browser_download_url"].as_str())
            .map(String::from)
    };
    let (Some(manifest_url), Some(exe_url)) = (asset(MANIFEST_ASSET), asset(EXE_ASSET)) else {
        // (Releases from before updates were signed.)
        return Ok(None);
    };
    let mut signed = Vec::new();
    use std::io::Read as _;
    agent()
        .get(&manifest_url)
        .set("User-Agent", &ua)
        .call()
        .map_err(|e| format!("couldn't download the release's manifest: {e}"))?
        .into_reader()
        .take(64 * 1024)
        .read_to_end(&mut signed)
        .map_err(|e| e.to_string())?;
    let manifest = Signed::parse(&signed).and_then(|s| s.verify()).map_err(|e| e.to_string())?;
    if !manifest.newer_than(version(), build()) {
        return Ok(None);
    }
    let page = release["html_url"].as_str().unwrap_or_default().to_string();
    Ok(Some(Remote { signed, manifest, exe_url, page }))
}

/// Download a release's exe (checked against its manifest). `progress` gets
/// (bytes so far, total).
pub fn download(remote: &Remote, mut progress: impl FnMut(u64, u64)) -> Result<Package, String> {
    use std::io::Read as _;
    let size = remote.manifest.size;
    let mut reader = agent()
        .get(&remote.exe_url)
        .set("User-Agent", &format!("rdpio/{}", version()))
        .call()
        .map_err(|e| format!("couldn't download the update: {e}"))?
        .into_reader()
        .take(size + 1);
    let mut exe = Vec::with_capacity(size as usize);
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("the download broke off: {e}"))?;
        if n == 0 {
            break;
        }
        exe.extend_from_slice(&buf[..n]);
        progress(exe.len() as u64, size);
    }
    if !remote.manifest.matches(&exe) {
        return Err("the downloaded file isn't the one the release describes".into());
    }
    Ok(Package { signed: remote.signed.clone(), manifest: remote.manifest.clone(), exe })
}

// ------------------------------------------------------------ signing

/// `--release-sign <exe> [--key <file>]`: write the signed manifest beside a
/// release build (the release pipeline; the key from `RDPIO_SIGNING_KEY`, the
/// file, or `%USERPROFILE%\.rdpio\release-signing.key` — a base64 Ed25519
/// seed). The exe must be this very build, so the manifest's version is
/// right.
pub fn release_sign(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine as _;
    let exe = PathBuf::from(args.first().ok_or("--release-sign needs the exe to sign")?);
    let key_file = args.iter().position(|a| a == "--key").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let seed_b64 = match (std::env::var("RDPIO_SIGNING_KEY").ok().filter(|k| !k.trim().is_empty()), key_file) {
        (Some(k), _) => k,
        (None, Some(f)) => std::fs::read_to_string(f)?,
        (None, None) => {
            let home = std::env::var("USERPROFILE")?;
            std::fs::read_to_string(Path::new(&home).join(".rdpio").join("release-signing.key"))?
        }
    };
    let seed: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(seed_b64.trim())?
        .try_into()
        .map_err(|_| "the signing key isn't a 32-byte Ed25519 seed")?;
    if Signed::public_key(&seed) != rdp_hyper::update::RELEASE_KEY {
        return Err("that isn't the release key (its public half isn't the one rdpio trusts)".into());
    }
    let bytes = std::fs::read(&exe)?;
    if bytes != std::fs::read(std::env::current_exe()?)? {
        return Err("sign a build with itself (run the rdpio.exe being signed), so the manifest's version is its own".into());
    }
    let m = Manifest::of(&bytes, version(), build(), protocols());
    let signed = Signed::sign(&m, &seed);
    std::fs::write(sidecar(&exe), signed.to_bytes())?;
    println!("Signed {} — rdpio {} ({}), protocols {}–{}", exe.display(), m.version, m.build, m.protocol_min, m.protocol_max);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_sits_beside_the_exe() {
        assert_eq!(sidecar(Path::new(r"C:\x\rdpio.exe")), PathBuf::from(r"C:\x\rdpio.exe.update"));
    }

    /// The launcher lets its console go; its standard handles are then dead,
    /// and a child that inherits them doesn't start. (Swaps this process's
    /// handles for a moment: run alone.)
    /// `cargo test -p rdp-client --bin rdpio dead_console -- --ignored --test-threads 1`
    #[test]
    #[ignore]
    fn starts_from_a_process_whose_console_is_gone() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        let cmd = crate::hyper_direct::system_tool("cmd.exe");
        let kinds = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
        let saved: Vec<HANDLE> = kinds.iter().map(|k| unsafe { GetStdHandle(*k) }.unwrap_or_default()).collect();
        for k in kinds {
            unsafe { SetStdHandle(k, HANDLE(0x1ee4 as *mut _)) }.unwrap();
        }
        let inherited = std::process::Command::new(&cmd).args(["/c", "exit"]).status();
        let detached = detached(&cmd).args(["/c", "exit"]).status();
        for (k, h) in kinds.iter().zip(saved) {
            unsafe { SetStdHandle(*k, h) }.unwrap();
        }
        println!("inheriting dead handles: {inherited:?}");
        println!("detached: {detached:?}");
        assert!(matches!(detached, Ok(s) if s.success()));
    }

    #[test]
    fn an_exe_is_replaced_even_while_it_runs() {
        let dir = std::env::temp_dir().join(format!("rdpio-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("rdpio.exe");
        std::fs::write(&target, b"old").unwrap();
        // Held open the way a running exe is (no delete/rename sharing for
        // writers, but renaming the file itself is allowed).
        let _running = std::fs::File::open(&target).unwrap();
        let new = dir.join("new.exe");
        std::fs::write(&new, b"new").unwrap();
        replace_exe(&target, &new, b"{manifest}").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(sidecar(&target)).unwrap(), b"{manifest}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
