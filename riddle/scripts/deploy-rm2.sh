#!/bin/sh
# Push a fresh build to the tablet, the way that does not break the gesture.
#
# riddle-gesture OWNS riddle's lifecycle: it watches /dev/input/event2 for a
# 5-finger tap and starts/stops /home/root/riddle/riddle itself. Launching
# riddle by hand over ssh bypasses it — the daemon goes on holding a pid that no
# longer exists, so the next 5-finger tap tries to stop a process that is not
# there and nothing happens. The gesture looks broken and the binary looks
# fine, which is a confusing pair of symptoms to debug.
#
# So: never start riddle here. Replace the binary, restart the daemon, and let
# the writer start it with the gesture.
#
# Prereq: ./scripts/build-armv7.sh
set -eu

HOST=${HOST:-rm2}
DIR=/home/root/riddle
BIN=target-docker/armv7-unknown-linux-gnueabihf/release/riddle

cd "$(dirname "$0")/.."
[ -f "$BIN" ] || { echo "build first: ./scripts/build-armv7.sh" >&2; exit 1; }

echo "==> copying binary"
scp -O -q "$BIN" "$HOST:$DIR/riddle.new"

echo "==> swapping in (previous kept as riddle.prev)"
ssh "$HOST" "cd $DIR && \
  for p in \$(pidof riddle); do kill -9 \$p 2>/dev/null; done; \
  pkill -9 -f riddle-gesture 2>/dev/null; \
  sleep 1; \
  mv -f riddle riddle.prev 2>/dev/null || true; \
  mv riddle.new riddle && chmod 755 riddle"

echo "==> restarting the gesture daemon"
# QTFB_KEY is what the daemon hands riddle so it attaches to the qtfb surface
# xochitl exposes; without it riddle has no display backend on this device.
ssh "$HOST" "cd $DIR && QTFB_KEY=${QTFB_KEY:-777001} nohup ./riddle-gesture \
  > /tmp/riddle-gesture.log 2>&1 & sleep 2; cat /tmp/riddle-gesture.log"

echo
echo "done — 5-finger tap on the tablet to start riddle, tap again to stop it."
echo "riddle's own log: /tmp/riddle.log"
