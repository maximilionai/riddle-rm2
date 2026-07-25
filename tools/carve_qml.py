#!/usr/bin/env python3
"""Carve QML source files out of xochitl's embedded Qt resource (qrc) data.

Qt stores qrc data-section entries as a big-endian uint32 length followed by the
raw file bytes. Since this build embeds QML uncompressed, we can find entries by
scanning for length prefixes whose payload looks like QML source.
"""
import re
import sys
import hashlib
from pathlib import Path

binary = Path(sys.argv[1]).read_bytes()
outdir = Path(sys.argv[2])
outdir.mkdir(parents=True, exist_ok=True)

PRINTABLE = set(range(0x20, 0x7F)) | {0x09, 0x0A, 0x0D}
QML_HINT = re.compile(
    rb"(^\s*import\s+[A-Za-z]|^\s*//|^\s*[A-Z][A-Za-z0-9_.]*\s*\{|^\s*pragma\s)", re.M
)

found = {}
i = 0
n = len(binary)
while i < n - 4:
    size = int.from_bytes(binary[i : i + 4], "big")
    if 20 <= size <= 400_000 and i + 4 + size <= n:
        payload = binary[i + 4 : i + 4 + size]
        # cheap reject before the expensive checks
        if payload[:1] and payload[0] in PRINTABLE:
            bad = sum(1 for b in payload if b not in PRINTABLE)
            if bad / size < 0.02 and QML_HINT.search(payload):
                digest = hashlib.sha1(payload).hexdigest()[:12]
                if digest not in found:
                    found[digest] = payload
                i += 4 + size
                continue
    i += 1

for digest, payload in found.items():
    text = payload.decode("utf-8", "replace")
    # try to name it from a leading comment or the first component id
    m = re.search(r"^\s*//\s*([A-Za-z0-9_]+\.qml)", text) or re.search(
        r"\b([A-Z][A-Za-z0-9_]+)\s*\{", text
    )
    stem = m.group(1).replace(".qml", "") if m else "unknown"
    (outdir / f"{stem}-{digest}.qml").write_text(text, encoding="utf-8")

print(f"carved {len(found)} QML sources -> {outdir}")
total = sum(len(v) for v in found.values())
print(f"total bytes: {total}")
