#!/bin/sh
set -eu
apt-get update -qq
apt-get install -y -qq qemu-user-static python3 libc6-armhf-cross libgcc-s1-armhf-cross >/dev/null
cd /src

BIN=target-docker/armv7-unknown-linux-gnueabihf/release/riddle
D=$(dirname "$BIN")

# oracle.env beside the binary — exactly how it ships in the AppLoad bundle.
cat > "$D/oracle.env" <<'EOF'
# self-hosted hermes gateway on the LAN
RIDDLE_OPENAI_KEY=test-api-server-key
RIDDLE_OPENAI_BASE=http://127.0.0.1:8642/v1
RIDDLE_OPENAI_MODEL=hermes-agent
RIDDLE_MEMORY_DIR=/tmp/riddle-mem
EOF

python3 scripts/mock-oracle.py &
SRV=$!
sleep 1

echo "=== riddle --oracle-test (armv7 under qemu) ==="
set +e
QEMU_LD_PREFIX=/usr/arm-linux-gnueabihf qemu-arm-static "$BIN" --oracle-test icon.png
RC=$?
set -e
kill $SRV 2>/dev/null || true
echo "=== exit code: $RC ==="
exit $RC
