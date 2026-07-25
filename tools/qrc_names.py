#!/usr/bin/env python3
"""Extract Qt resource (qrc) name-table entries from xochitl.

Qt's qrc name section stores each path segment as:
    uint16 length (chars), uint32 name hash, then length UTF-16BE code units.
Finding these gives the real resource paths, which the plain-ASCII carve misses.
"""
import re
import sys
from pathlib import Path

data = Path(sys.argv[1]).read_bytes()

# UTF-16BE ASCII text looks like \x00A\x00B... — find runs of it.
utf16_run = re.compile(rb"(?:\x00[\x20-\x7e]){3,}")

names = {}
for m in utf16_run.finditer(data):
    start, end = m.span()
    text = m.group().decode("utf-16-be", "replace")
    nchars = (end - start) // 2
    # the length+hash header sits 6 bytes before the first code unit
    hdr = start - 6
    if hdr >= 0:
        declared = int.from_bytes(data[hdr : hdr + 2], "big")
        name_hash = int.from_bytes(data[hdr + 2 : hdr + 6], "big")
        if declared == nchars:
            names[start] = (text, name_hash)

print(f"qrc name entries found: {len(names)}")
qml = {t: h for t, h in names.values() if t.endswith(".qml")}
print(f"  of which .qml: {len(qml)}")

out = Path(sys.argv[2])
with out.open("w") as f:
    for text, name_hash in sorted(names.values()):
        f.write(f"{name_hash:10d}  {text}\n")
print(f"wrote {out}")

print("\n--- sample .qml entries (name_hash, segment) ---")
for text, name_hash in sorted(qml.items())[:15]:
    print(f"  {name_hash:10d}  {text}")
