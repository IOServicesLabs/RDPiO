//! Build script for the rdpio binary.
//!
//! Two jobs:
//!   1. Stamp the git revision into the binary so every log identifies exactly
//!      which build produced it — field logs from stale copies of rdpio.exe are
//!      otherwise indistinguishable from current ones.
//!   2. Embed `res/rdpio.rc` (the rdpio.ico icon as resource id 1) into the exe.
//!      `embed_resource::NONE` = icon only; no manifest, no version resource.

fn main() {
    // --- Job 1: git revision stamp -----------------------------------------
    let describe = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RDPIO_BUILD={describe}");
    // Re-stamp when the checked-out commit moves (HEAD for branch switches,
    // the ref file for new commits on the same branch).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    // --- Job 2: embed the icon resource ------------------------------------
    // Path is relative to CARGO_MANIFEST_DIR (the crate root). The rc file
    // declares the icon as resource id 1 and nothing else. In embed-resource
    // 2.5.x `compile` returns () and emits a compile error via the cargo
    // rerun-if-changed mechanism on failure.
    embed_resource::compile("res/rdpio.rc", embed_resource::NONE);
}
