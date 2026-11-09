//! Build script for the rdpio binary.
//!
//! Two jobs:
//!   1. Stamp the build time and git commit into the binary so every log
//!      identifies exactly which build produced it — field logs from stale
//!      copies of rdpio.exe are otherwise indistinguishable from current ones.
//!   2. Embed the rdpio.ico icon (resource id 1) and a version resource into the
//!      exe. Microsoft's Teams plugin tells Teams its host's name and version from
//!      that resource.

fn main() {
    // --- Job 1: build stamp ------------------------------------------------
    // "2026.10.12.1432+9c915af": when it was built (UTC) and the commit it
    // was built from — every build its own, uncommitted changes or not.
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let stamp = build_time();
    let build = match commit {
        Some(c) => format!("{stamp}+{c}"),
        None => stamp,
    };
    println!("cargo:rustc-env=RDPIO_BUILD={build}");
    // Re-stamp when the commit moves or the source changes.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=res");

    // --- The launcher's brand fonts, when this checkout has them -------------
    // (licensed for IO Services, so never in git: see launcher/mod.rs).
    println!("cargo::rustc-check-cfg=cfg(rdpio_brand_fonts)");
    println!("cargo:rerun-if-changed=src/launcher/fonts");
    let fonts = std::path::Path::new("src/launcher/fonts");
    if ["Geomisans.woff2", "SanDiego.woff", "GelatoTypewriter-Regular.woff2"].iter().all(|f| fonts.join(f).exists()) {
        println!("cargo:rustc-cfg=rdpio_brand_fonts");
    }

    // --- The virtual camera's media source ---------------------------------
    build_vcam();

    // --- Job 2: embed the icon resource ------------------------------------
    // Written here to carry the crate's version. In embed-resource 2.5.x
    // `compile` returns () and emits a compile error via the cargo
    // rerun-if-changed mechanism on failure.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap().replace('\\', "/");
    let version = env!("CARGO_PKG_VERSION");
    let rc = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("rdpio.rc");
    std::fs::write(&rc, version_rc(&manifest_dir, version)).expect("write rdpio.rc");
    embed_resource::compile(&rc, embed_resource::NONE);

    // --- A manifest asking for common controls 6 -----------------------------
    // Microsoft's Teams add-in imports functions only that version has, so
    // without this Windows refuses to load it ("procedure not found").
    if std::env::var("TARGET").is_ok_and(|t| t.contains("windows-msvc")) {
        for link_arg in [
            "/MANIFEST:EMBED",
            "/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0'              processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'",
        ] {
            println!("cargo:rustc-link-arg-bins={link_arg}");
        }
    }
}

/// The exe's resources: the icon, as resource id 1, and rdpio's name and version.
fn version_rc(manifest_dir: &str, version: &str) -> String {
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let mut part = || parts.next().unwrap_or(0);
    let numbers = format!("{},{},{},0", part(), part(), part());
    format!(
        r#"1 ICON "{manifest_dir}/res/rdpio.ico"
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS 0x40004
FILETYPE 1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "IO Services"
            VALUE "FileDescription", "RDPiO"
            VALUE "FileVersion", "{version}"
            VALUE "OriginalFilename", "rdpio.exe"
            VALUE "ProductName", "RDPiO"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 0x4B0
    END
END
"#
    )
}

/// `rdpio_vcam.dll` (vcam/rdpio_vcam.cpp): the media source Windows' Frame
/// Server loads for the host's virtual camera during a Hyper stream. Built here
/// with the MSVC toolchain into OUT_DIR and carried inside rdpio.exe
/// (`hyper_camera::DLL`), which writes it out on the host. Where it can't be
/// built (no MSVC, not Windows) an empty file stands in, and the camera over
/// Hyper says it's missing from this build.
fn build_vcam() {
    println!("cargo:rerun-if-changed=vcam");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dll = out.join("rdpio_vcam.dll");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        let _ = std::fs::write(&dll, b"");
        return;
    }
    if let Err(e) = link_vcam(&target, &out, &dll) {
        println!("cargo:warning=no virtual camera in this build (rdpio_vcam.dll: {e})");
        let _ = std::fs::write(&dll, b"");
    }
}

fn link_vcam(target: &str, out: &std::path::Path, dll: &std::path::Path) -> Result<(), String> {
    let mut build = cc::Build::new();
    build.cpp(true).file("vcam/rdpio_vcam.cpp").flag("/std:c++20").flag("/EHsc").flag("/permissive-").warnings(false);
    // C++/WinRT's headers sit beside the SDK's `um` ones.
    let compiler = build.try_get_compiler().map_err(|e| e.to_string())?;
    let include = compiler
        .env()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("INCLUDE"))
        .map(|(_, v)| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    for dir in std::env::split_paths(&include) {
        if dir.ends_with("um") {
            let cppwinrt = dir.with_file_name("cppwinrt");
            if cppwinrt.exists() {
                build.include(cppwinrt);
            }
        }
    }
    let objects = build.try_compile_intermediates().map_err(|e| e.to_string())?;
    let link = cc::windows_registry::find_tool(target, "link.exe").ok_or("no link.exe")?;
    let mut cmd = link.to_command();
    cmd.arg("/NOLOGO")
        .arg("/DLL")
        .arg(format!("/OUT:{}", dll.display()))
        .arg(format!("/DEF:{}", std::path::Path::new("vcam/rdpio_vcam.def").display()))
        .arg(format!("/IMPLIB:{}", out.join("rdpio_vcam.lib").display()))
        .args(&objects)
        .args(["mfplat.lib", "mfsensorgroup.lib", "mfuuid.lib", "ole32.lib", "oleaut32.lib", "propsys.lib", "windowsapp.lib"]);
    let status = cmd.status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("link failed ({status})"));
    }
    Ok(())
}

/// Now (UTC) as `YYYY.MM.DD.HHMM`.
fn build_time() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y}.{m:02}.{d:02}.{:02}{:02}", rem / 3600, rem % 3600 / 60)
}
