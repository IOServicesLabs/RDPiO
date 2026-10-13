// NVIDIA's encoder, reached through a small C bridge compiled against the
// SDK header (nvenc/), so its structures always match the driver's.
fn main() {
    println!("cargo:rerun-if-changed=nvenc/rdp_nvenc.c");
    println!("cargo:rerun-if-changed=nvenc/nvEncodeAPI.h");
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        cc::Build::new().file("nvenc/rdp_nvenc.c").include("nvenc").warnings(false).compile("rdp_nvenc");
    }
}
