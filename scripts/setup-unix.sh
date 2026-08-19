#!/usr/bin/env bash
# setup-unix.sh — install the native dependencies RDPiO needs to build and run
# on Linux/macOS.
#
# RDPiO's protocol stack, codecs and WebRTC redirector are pure Rust; the Linux
# port (see PORTING.md) additionally needs a C toolchain (for ring/rustls) and,
# for the interactive-client stages, the media libraries that replace the
# Windows Media Foundation / WASAPI backends:
#
#   * build-essential / clang + pkg-config — C toolchain required to link
#     rustls (ring) and any -sys crates.
#   * ffmpeg            — H.264 decode for the interactive client (Stage 5).
#   * libopus-dev       — Opus audio for the WebRTC redirector / audio backend.
#   * libva + vaapi     — VA-API hardware decode drivers for H.264 (Stage 5).
#   * NVENC SDK         — NVIDIA Video Codec SDK headers (optional; only the
#                         experimental NVENC capture path needs them).
#
# Idempotent: every step checks whether the dependency is already present and
# exits 0 when it is. Re-running this script is safe.
#
# Usage:
#   ./scripts/setup-unix.sh [--include-optional] [--sudo]
#
# Flags:
#   --include-optional  also install optional media deps (ffmpeg, libopus-dev,
#                       libva/vaapi drivers). Default: skip them.
#   --sudo              use sudo for package-manager calls even when already root.
#
# Exit codes:
#   0  all required deps present/installed (optional deps skipped or installed)
#   1  a required dependency could not be installed

set -u

INCLUDE_OPTIONAL=0
USE_SUDO=""
for arg in "$@"; do
    case "$arg" in
        --include-optional) INCLUDE_OPTIONAL=1 ;;
        --sudo) USE_SUDO="sudo" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

if [[ -z "${USE_SUDO}" ]] && [[ "$(id -u)" -ne 0 ]]; then
    USE_SUDO="sudo"
fi

step()  { printf '\033[36m==> %s\033[0m\n' "$1"; }
ok()    { printf '\033[32m    OK: %s\033[0m\n' "$1"; }
skip()  { printf '\033[33m    skip: %s\033[0m\n' "$1"; }
fail()  { printf '\033[31m    FAIL: %s\033[0m\n' "$1"; }

have()  { command -v "$1" >/dev/null 2>&1; }
pkg_have() { pkg-config --exists "$1" 2>/dev/null; }

detect_pkg_manager() {
    if have apt-get; then echo apt
    elif have dnf; then echo dnf
    elif have pacman; then echo pacman
    elif have apk; then echo apk
    elif have brew; then echo brew
    else echo none
    fi
}

PM="$(detect_pkg_manager)"
if [[ "$PM" == "none" ]]; then
    echo "setup-unix.sh: no supported package manager found (apt/dnf/pacman/apk/brew)" >&2
    exit 1
fi

install_pkgs() {
    local pkgs=("$@")
    case "$PM" in
        apt)
            ${USE_SUDO} apt-get update -qq
            ${USE_SUDO} apt-get install -y -qq "${pkgs[@]}"
            ;;
        dnf)
            ${USE_SUDO} dnf install -y "${pkgs[@]}"
            ;;
        pacman)
            ${USE_SUDO} pacman -S --needed --noconfirm "${pkgs[@]}"
            ;;
        apk)
            ${USE_SUDO} apk add --no-cache "${pkgs[@]}"
            ;;
        brew)
            brew install "${pkgs[@]}"
            ;;
    esac
}

# ---------------------------------------------------------------------------
# 1. C toolchain + pkg-config (REQUIRED — ring/rustls links against libc)
# ---------------------------------------------------------------------------
step "Checking C toolchain and pkg-config..."
missing=()
have cc || missing+=(cc)
have gcc || missing+=(gcc)
have pkg-config || missing+=(pkg-config)
have make  || missing+=(make)

if [[ ${#missing[@]} -eq 0 ]]; then
    ok "C toolchain (cc/gcc), make and pkg-config present"
else
    case "$PM" in
        apt) install_pkgs build-essential pkg-config make ;;
        dnf) install_pkgs gcc gcc-c++ make pkgconf-pkg-config ;;
        pacman) install_pkgs base-devel pkg-config ;;
        apk) install_pkgs build-base pkgconf ;;
        brew) install_pkgs pkg-config ;;
    esac
    if have cc && have pkg-config; then
        ok "C toolchain installed"
    else
        fail "could not install C toolchain"
        exit 1
    fi
fi

# ---------------------------------------------------------------------------
# 2. Rust (REQUIRED if cargo is missing)
# ---------------------------------------------------------------------------
step "Checking Rust toolchain..."
if have cargo; then
    ok "cargo $(cargo --version | awk '{print $2}') present"
elif have rustup; then
    ok "rustup present (cargo will be installed on first use)"
else
    skip "cargo not found; install via https://rustup.rs (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh)"
fi

# ---------------------------------------------------------------------------
# 3. ffmpeg (OPTIONAL — H.264 decode for the interactive client, Stage 5)
# ---------------------------------------------------------------------------
if [[ "$INCLUDE_OPTIONAL" -eq 1 ]]; then
    step "Checking ffmpeg..."
    if have ffmpeg; then
        ok "ffmpeg present"
    else
        case "$PM" in
            apt) install_pkgs ffmpeg ;;
            dnf) install_pkgs ffmpeg-free ;;
            pacman) install_pkgs ffmpeg ;;
            apk) install_pkgs ffmpeg ;;
            brew) install_pkgs ffmpeg ;;
        esac
        if have ffmpeg; then ok "ffmpeg installed"; else skip "ffmpeg install failed (optional)"; fi
    fi
else
    skip "ffmpeg skipped (pass --include-optional)"
fi

# ---------------------------------------------------------------------------
# 4. opus (OPTIONAL — Opus audio for the WebRTC redirector)
# ---------------------------------------------------------------------------
if [[ "$INCLUDE_OPTIONAL" -eq 1 ]]; then
    step "Checking libopus..."
    if pkg_have opus; then
        ok "libopus present"
    else
        case "$PM" in
            apt) install_pkgs libopus-dev ;;
            dnf) install_pkgs opus-devel ;;
            pacman) install_pkgs opus ;;
            apk) install_pkgs opus-dev ;;
            brew) install_pkgs opus ;;
        esac
        if pkg_have opus; then ok "libopus installed"; else skip "libopus install failed (optional)"; fi
    fi
else
    skip "libopus skipped (pass --include-optional)"
fi

# ---------------------------------------------------------------------------
# 5. VA-API drivers (OPTIONAL — hardware H.264 decode, Stage 5)
# ---------------------------------------------------------------------------
if [[ "$INCLUDE_OPTIONAL" -eq 1 ]]; then
    step "Checking VA-API drivers..."
    if have vainfo; then
        ok "vainfo present"
    else
        case "$PM" in
            apt) install_pkgs libva2 libva-drm2 vainfo intel-media-driver i965-va-driver 2>/dev/null ;;
            dnf) install_pkgs libva libva-utils intel-media-driver 2>/dev/null ;;
            pacman) install_pkgs libva-utils intel-media-driver 2>/dev/null ;;
            apk) skip "VA-API packages not mapped for apk" ;;
            brew) install_pkgs libva 2>/dev/null ;;
        esac
        if have vainfo; then ok "VA-API tooling installed"; else skip "VA-API install unavailable (optional)"; fi
    fi
else
    skip "VA-API skipped (pass --include-optional)"
fi

# ---------------------------------------------------------------------------
# 6. NVENC SDK headers (OPTIONAL — experimental NVENC capture path)
# ---------------------------------------------------------------------------
if [[ "$INCLUDE_OPTIONAL" -eq 1 ]]; then
    step "Checking NVENC SDK headers..."
    if ls /usr/include/nvEncodeAPI.h >/dev/null 2>&1; then
        ok "nvEncodeAPI.h present"
    else
        skip "NVENC SDK headers not installed; download the NVIDIA Video Codec SDK from https://developer.nvidia.com/nvidia-video-codec-sdk if you build the NVENC capture path"
    fi
else
    skip "NVENC SDK headers skipped (pass --include-optional)"
fi

step "setup-unix.sh finished."
printf '\033[32m    Next: cargo build --release -p rdp-client\033[0m\n'
exit 0
