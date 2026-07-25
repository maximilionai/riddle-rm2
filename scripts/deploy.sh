#!/bin/sh
# Deploy riddlefb.so to a reMarkable 2, with an automatic rollback if xochitl
# does not stay healthy.
#
# Run this ON THE TABLET. Copy it and the .so over first:
#     scp fbcontroller/build/libriddlefb.so root@<tablet>:/home/root/riddlefb-staging/riddlefb.so
#     scp scripts/deploy.sh                 root@<tablet>:/home/root/
#     ssh root@<tablet> sh /home/root/deploy.sh
#
# Why the rollback matters: xochitl runs under Restart=on-failure with
# WatchdogSec=60, so a bad extension is a restart loop, and a restart loop with
# no console is a bad afternoon. This script watches for ~100s and puts the old
# state back if anything moves.
set -u

STAGED=${STAGED:-/home/root/riddlefb-staging/riddlefb.so}
EXT=${EXT:-/home/root/xovi/extensions.d}
DROPIN=${DROPIN:-/etc/systemd/system/xochitl.service.d/xovi.conf}
BAK=${BAK:-/home/root/riddlefb-rollback}
LOG=${LOG:-/home/root/riddlefb-deploy.log}

exec >>"$LOG" 2>&1
echo "=== deploy $(date) ==="

[ -f "$STAGED" ] || { echo "FATAL: $STAGED not found"; exit 2; }

mkdir -p "$BAK"
[ -f "$EXT/riddlefb.so" ] && cp -a "$EXT/riddlefb.so" "$BAK/riddlefb.so.old"
cp -a "$DROPIN" "$BAK/xovi.conf.bak"

state() { systemctl show xochitl -p "$1" | cut -d= -f2; }

rollback() {
    echo "--- ROLLBACK $(date) ---"
    if [ -f "$BAK/riddlefb.so.old" ]; then
        cp -a "$BAK/riddlefb.so.old" "$EXT/riddlefb.so"
    else
        rm -f "$EXT/riddlefb.so"
    fi
    cp -a "$BAK/xovi.conf.bak" "$DROPIN"
    systemctl daemon-reload
    systemctl reset-failed xochitl 2>/dev/null
    systemctl restart xochitl
    sleep 25
    echo "post-rollback ActiveState=$(state ActiveState) NRestarts=$(state NRestarts)"
    echo "=== ROLLED BACK ==="
}

# --- apply -------------------------------------------------------------
pkill -f "/home/root/riddle/riddle$" 2>/dev/null; sleep 1

cp "$STAGED" "$EXT/riddlefb.so"
chmod +x "$EXT/riddlefb.so"

# QML_DISABLE_DISK_CACHE is REQUIRED: without it the extension aborts at every
# start. The screen-mode knobs are what make ink use the fast e-ink waveform.
for kv in \
    "QML_DISABLE_DISK_CACHE=1" \
    "RIDDLE_FB_SCREENMODE=0" \
    "EPFB_SPLIT_UPDATES=1"
do
    key=${kv%%=*}
    sed -i "/^Environment=$key=/d" "$DROPIN"
    printf 'Environment=%s\n' "$kv" >> "$DROPIN"
done

echo "--- drop-in ---"; cat "$DROPIN"

systemctl daemon-reload
systemctl reset-failed xochitl 2>/dev/null
systemctl restart xochitl

# --- watch -------------------------------------------------------------
# NOTE: baseline AFTER the restart. A manual `systemctl restart` does not
# increment NRestarts, so comparing against a pre-restart value is meaningless.
sleep 10
NR0=$(state NRestarts)
echo "baseline NRestarts=$NR0"

i=0
while [ $i -lt 9 ]; do
    sleep 10
    i=$((i + 1))
    ST=$(state ActiveState); NR=$(state NRestarts)
    echo "t=$((i * 10 + 10))s ActiveState=$ST NRestarts=$NR MainPID=$(state MainPID)"
    if [ "$NR" != "$NR0" ]; then
        echo "!! xochitl restarted ($NR0 -> $NR) - rolling back"; rollback; exit 1
    fi
    case "$ST" in
        active|activating) ;;
        *) echo "!! ActiveState=$ST - rolling back"; rollback; exit 1 ;;
    esac
done

echo "=== HEALTHY - change retained ==="
systemctl show xochitl -p ActiveState -p Result -p NRestarts -p MainPID
echo "--- qtfb socket ---"; ls -la /tmp/qtfb.sock 2>&1
echo "--- extension journal ---"
journalctl -u xochitl -b --no-pager 2>/dev/null | grep -E "\[QTFB" | tail -n 12
echo "=== end $(date) ==="
