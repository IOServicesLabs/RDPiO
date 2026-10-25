// NVIDIA's encoder, reached through a small C bridge compiled against the
// SDK header (nvenc/), so its structures always match the driver's. Built
// twice: against API 12.0 (any driver from 522) and 13.0 (570 on: long-term
// references for AV1), each in its own library.
fn main() {
    println!("cargo:rerun-if-changed=nvenc/rdp_nvenc.c");
    println!("cargo:rerun-if-changed=nvenc/nvEncodeAPI.h");
    println!("cargo:rerun-if-changed=nvenc/sdk13/rdp_nvenc13.c");
    println!("cargo:rerun-if-changed=nvenc/sdk13/nvEncodeAPI.h");
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        cc::Build::new().file("nvenc/rdp_nvenc.c").include("nvenc").warnings(false).compile("rdp_nvenc");
        cc::Build::new().file("nvenc/sdk13/rdp_nvenc13.c").warnings(false).compile("rdp_nvenc13");
    }
}
