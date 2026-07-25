#!/usr/bin/env bash
# Stage the AppLoad bundle into dist/riddle/, ready to copy to the tablet.
# reMarkable 2 flavour: qtfb (windowed, inside xochitl) only — there is no
# takeover backend on this device, so no libquill.so and no launcher script.
# Prereq: ./scripts/build-armv7.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target-docker/armv7-unknown-linux-gnueabihf/release/riddle
[ -f "$BIN" ] || { echo "build first: ./scripts/build-armv7.sh" >&2; exit 1; }

rm -rf dist/riddle
mkdir -p dist/riddle
install -m 755 "$BIN" dist/riddle/riddle
install -m 644 external.manifest.json icon.png oracle.env.example settings.schema.json dist/riddle/
# oracle.env is the live config; ship it if the operator has filled one in.
[ -f oracle.env ] && install -m 600 oracle.env dist/riddle/

echo "staged: $(du -sh dist/riddle | cut -f1) in dist/riddle/"
echo "copy to the tablet with:"
echo "  scp -O -r dist/riddle root@<tablet>:/home/root/xovi/exthome/appload/"
