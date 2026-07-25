#!/bin/sh
# Cross-build riddle for the reMarkable 2 (armv7l, glibc 2.35).
#
# Runs in a Debian bullseye Rust container: its armhf cross toolchain targets
# glibc 2.31, comfortably OLDER than the device's 2.35, so the resulting binary
# never asks for a symbol version the tablet lacks. (Bookworm's would be 2.36 —
# too new. Do not "upgrade" the base image.)
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
