#!/usr/bin/env python3
"""Resolve the Paper Pro qmldiff's hashes against THIS device's QML.

qmldiff hash = djb2 seeded 5481, each char truncated to its low byte.
We build a local hashtab from our carved QML (paths, path segments, identifiers,
string literals) and then see how many of the Paper Pro diff's hashes resolve.
Whatever fails to resolve is the actual drift.
"""
import re
import sys
from pathlib import Path

MASK = (1 << 64) - 1


def qhash(s: str, seed: int = 5481) -> int:
    h = seed
    for ch in s:
        h = ((h << 5) + h + (ord(ch) & 0xFF)) & MASK
    return h


qml_dir = Path(sys.argv[1])
names_file = Path(sys.argv[2])
qmd_file = Path(sys.argv[3])

# --- build our hashtab ---------------------------------------------------
table: dict[int, str] = {}


def add(s: str):
    if s and s not in ("",):
        table.setdefault(qhash(s), s)


# resource names (from the qrc name table)
for line in names_file.read_text(errors="replace").splitlines():
    parts = line.split(None, 1)
    if len(parts) == 2:
        seg = parts[1].strip()
        add(seg)

# identifiers + string literals from carved QML
ident = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
strlit = re.compile(r"\"([^\"\n]{1,80})\"")
for f in qml_dir.glob("*.qml"):
    text = f.read_text(errors="replace")
    for m in ident.finditer(text):
        tok = m.group()
        add(tok)
    for m in strlit.finditer(text):
        add(m.group(1))

print(f"local hashtab entries: {len(table)}")

# --- pull hashes out of the Paper Pro diff -------------------------------
qmd = qmd_file.read_text(errors="replace")
hashes = set()
for m in re.finditer(r"\[\[\"?(\d+)\]\]", qmd):
    hashes.add(int(m.group(1)))
for m in re.finditer(r"~&\"?'?(\d+)&~", qmd):
    hashes.add(int(m.group(1)))

print(f"hashes referenced by the Paper Pro diff: {len(hashes)}")

resolved, unresolved = {}, []
for h in sorted(hashes):
    if h in table:
        resolved[h] = table[h]
    else:
        unresolved.append(h)

print(f"\nRESOLVED against our device's QML: {len(resolved)}/{len(hashes)}")
for h, s in sorted(resolved.items(), key=lambda kv: kv[1]):
    print(f"  {h:24d}  {s}")

print(f"\nUNRESOLVED (drift / not present here): {len(unresolved)}")
for h in unresolved[:40]:
    print(f"  {h}")

# sanity check: the two known-good decodings from the 3.24 hashtab
print("\n--- sanity check of the hash function ---")
for probe, expect in [
    ("/qml/device/view/main/MainView.qml", 2328484894988065446),
    ("id", 5972374),
]:
    got = qhash(probe)
    print(f"  {'OK ' if got == expect else 'BAD'} {probe!r} -> {got} (expected {expect})")
