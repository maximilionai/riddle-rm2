#!/bin/sh
# Cross-build riddle for the reMarkable 2 (armv7l, glibc 2.35).
#
# Runs in a Debian bullseye Rust container: its armhf cross toolchain targets
# glibc 2.31, comfortably OLDER than the device's 2.35, so the resulting binary
# never asks for a symbol version the tablet lacks. (Bookworm's would be 2.36 —
# too new. Do not "upgrade" the base image.)
#
# The rM2's Cortex-A7 has NEON, but the armv7-unknown-linux-gnueabihf target
# defaults to generic ARMv7 with VFPv3-D16 and no NEON at all — so every
# per-pixel loop in ink.rs/surface.rs was being compiled for a weaker CPU than
# the one it runs on. The C++ half of this project has always passed
# -mfpu=neon -mcpu=cortex-a7; this is the Rust equivalent.
#
# Output: target-docker/armv7-unknown-linux-gnueabihf/release/riddle
set -eu

HERE=$(cd "$(dirname "$0")/.." && pwd)

exec docker run --rm \
  -v "$HERE":/src \
  -w /src \
  -e CARGO_TARGET_DIR=/src/target-docker \
  -e CARGO_HOME=/src/target-docker/cargo-home \
  -e CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
  -e CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
  -e AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar \
  -e CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=cortex-a7" \
  rust:1-bullseye \
  sh -eu -c '
    if ! command -v arm-linux-gnueabihf-gcc >/dev/null 2>&1; then
      apt-get update -qq
      apt-get install -y -qq gcc-arm-linux-gnueabihf libc6-dev-armhf-cross >/dev/null
    fi
    rustup target add armv7-unknown-linux-gnueabihf
    cargo build --release --target armv7-unknown-linux-gnueabihf
    arm-linux-gnueabihf-readelf -d target-docker/armv7-unknown-linux-gnueabihf/release/riddle | grep NEEDED
    echo "--- max GLIBC_ version required ---"
    arm-linux-gnueabihf-readelf -V target-docker/armv7-unknown-linux-gnueabihf/release/riddle \
      | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -5
  '
