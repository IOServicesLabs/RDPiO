#!/usr/bin/env bash
# Builds the app's native core (librdpio.so) into app/src/main/jniLibs.
# Gradle runs this before every build; it can also be run by hand.
#
# Needs: rustup target add aarch64-linux-android; cargo install cargo-ndk;
# the Android SDK with an NDK and its CMake (sdkmanager "ndk;27.1.12297006"
# "cmake;3.22.1"). libopus is built from source with CMake: CMake's own
# Android support needs the NDK in the environment, and Ninja (Visual Studio
# generators can't target Android).
set -euo pipefail
cd "$(dirname "$0")"

PROFILE="${1:-release}"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-${LOCALAPPDATA:-$HOME}/Android/Sdk}}"
SDK="${SDK//\\//}"
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  ANDROID_NDK_HOME="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1)"
fi
[ -d "$ANDROID_NDK_HOME" ] || { echo "no Android NDK under $SDK/ndk" >&2; exit 1; }
CMAKE_BIN="$(ls -d "$SDK"/cmake/*/bin 2>/dev/null | sort -V | tail -1)"
export ANDROID_NDK_HOME ANDROID_NDK_ROOT="$ANDROID_NDK_HOME" ANDROID_NDK="$ANDROID_NDK_HOME"
export CMAKE_GENERATOR=Ninja
[ -n "$CMAKE_BIN" ] && export PATH="$CMAKE_BIN:$PATH"

FLAGS=()
[ "$PROFILE" = "release" ] && FLAGS+=(--release)
# From rust/, so cargo reads rust/.cargo/config.toml (the CPU features).
cd rust
cargo ndk -t arm64-v8a -P 30 -o ../app/src/main/jniLibs build "${FLAGS[@]}"
