//! Embeds the git revision into the binary so every log identifies exactly
//! which build produced it — field logs from stale copies of rdpio.exe are
//! otherwise indistinguishable from current ones.

fn main() {
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
}
