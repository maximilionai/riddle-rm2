#!/bin/sh
# Run the unit tests on the host architecture, in the same container the
# armv7 cross-build uses. Nothing but Docker is needed.
#
# The drawing modality's renderer (ops.rs) and the crop transform (ink.rs) are
# deliberately free of device code, so they are testable here — no tablet, no
# API key. Extra args are passed through to `cargo test`.
#
# Uses its own CARGO_TARGET_DIR so a host test run never invalidates the
# armv7 release artifacts in target-docker/.
set -eu

HERE=$(cd "$(dirname "$0")/.." && pwd)

exec docker run --rm \
  -v "$HERE":/src \
  -w /src \
  -e CARGO_TARGET_DIR=/src/target-host \
  -e CARGO_HOME=/src/target-host/cargo-home \
  rust:1-bullseye \
  cargo test "$@"
