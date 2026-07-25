#!/usr/bin/env bash
# Build libriddlefb.so -- the rM2 qtfb server / FBController xovi extension.
# Runs INSIDE the reMarkable codex SDK container (see qt-sysroot/sdk-run.sh).
#   sources at /src (ro), outputs at /out (rw)
#
# Flags follow qt-sysroot/build-probe.sh exactly (that combination is the one
# verified to produce a .so whose symbols all resolve against the device libs).
set -euo pipefail

: "${SYSROOT:?run me via sdk-run.sh}"
: "${NAT:?run me via sdk-run.sh}"

OUT=/out
SRC=/src
SO="$OUT/libriddlefb.so"

QTINC="-I$SYSROOT/usr/include \
 -I$SYSROOT/usr/include/QtCore \
 -I$SYSROOT/usr/include/QtGui \
 -I$SYSROOT/usr/include/QtQml \
 -I$SYSROOT/usr/include/QtQuick"

CXXFLAGS="--sysroot=$SYSROOT \
 -mfpu=neon -mfloat-abi=hard -mcpu=cortex-a7 \
 -O2 -pipe -fPIC -std=c++17 -DQT_NO_DEBUG \
 -Wall -Wextra -Wno-unused-parameter \
 -I$SRC -I$OUT $QTINC"

echo "=== moc (SDK 6.5.2) ==="
"$NAT/usr/libexec/moc" $QTINC -I"$SRC" "$SRC/FBController.h" -o "$OUT/moc_FBController.cpp"

echo "=== compile ==="
OBJS=()
for f in FBController.cpp fbmanagement.cpp screenmode.cpp entry.cpp; do
    echo "  cc $f"
    arm-remarkable-linux-gnueabi-g++ $CXXFLAGS -c "$SRC/$f" -o "$OUT/${f%.cpp}.o"
    OBJS+=("$OUT/${f%.cpp}.o")
done
echo "  cc moc_FBController.cpp"
arm-remarkable-linux-gnueabi-g++ $CXXFLAGS -c "$OUT/moc_FBController.cpp" -o "$OUT/moc_FBController.o"
OBJS+=("$OUT/moc_FBController.o")

echo "  cc xovi.c"
arm-remarkable-linux-gnueabi-gcc --sysroot="$SYSROOT" \
    -mfpu=neon -mfloat-abi=hard -mcpu=cortex-a7 -O2 -fPIC -std=gnu11 \
    -c "$SRC/xovi.c" -o "$OUT/xovi.o"
OBJS+=("$OUT/xovi.o")

echo "=== link ==="
arm-remarkable-linux-gnueabi-g++ \
    --sysroot="$SYSROOT" \
    -mfpu=neon -mfloat-abi=hard -mcpu=cortex-a7 \
    -shared -o "$SO" "${OBJS[@]}" \
    -L"$SYSROOT/usr/lib" \
    -lQt6Quick -lQt6Qml -lQt6Gui -lQt6Core \
    -lpthread -lrt \
    -Wl,-O1 -Wl,--hash-style=gnu -Wl,--as-needed -Wl,-z,defs

arm-remarkable-linux-gnueabi-strip --strip-unneeded "$SO"

echo "=== result ==="
arm-remarkable-linux-gnueabi-readelf -dW "$SO" | grep -E 'NEEDED|SONAME' || true
echo "--- exported xovi entry points ---"
arm-remarkable-linux-gnueabi-nm -D --defined-only "$SO" | grep -E '_xovi_|LINKTABLE|EXTENSIONVERSION|METADATAVALUES|Environment|riddlefb' || true
ls -l "$SO"
