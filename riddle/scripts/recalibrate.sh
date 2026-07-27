#!/bin/sh
# Re-fit RIDDLE_GROUND_Y_SCALE at the resolution annotate mode ACTUALLY sends.
#
# The constant is a per-model correction for y coming back systematically LONG,
# and it is only valid at the resolution it was fitted at: the first fit (1.025)
# was measured on a 468px page, and annotate mode has since gone to a 1200px cap
# with no 2x floor (ink.rs::to_png). A constant fitted at the wrong scale does
# not merely expire, it actively moves marks off their targets — so re-run this
# whenever that cap, the page pipeline, or the model changes.
#
#   set -a; . ./oracle.env; set +a; scripts/recalibrate.sh
#
# Knobs (all optional):
#   CAP=1200      long-side cap; must match ink.rs's annotate cap
#   TRIALS=3      probes per target
#   DELAY=13      seconds between probes — Cerebras free tier is 5 req/min and
#                 a rate-limited trial returns garbage, not an error
#   PROVIDER=cerebras
#   PAGE=target-host/page.png
set -eu

cd "$(dirname "$0")/.."

if [ -z "${RIDDLE_OPENAI_KEY:-}" ]; then
    cat >&2 <<'EOF'
recalibrate: RIDDLE_OPENAI_KEY is not set — nothing was sent.

  set -a; . ./oracle.env; set +a
  scripts/recalibrate.sh

Budget: TARGETS x TRIALS calls (18 by default) at DELAY seconds apart.
EOF
    exit 2
fi

CAP=${CAP:-1200}
TRIALS=${TRIALS:-3}
DELAY=${DELAY:-13}
PROVIDER=${PROVIDER:-cerebras}
PAGE=${PAGE:-target-host/page.png}
OUT=target-host/recal
mkdir -p "$OUT"

# Ground truth for target-host/page.png, normalized 0-1000 x0,y0,x1,y1. These
# span the full height on purpose: fitting a SCALE needs targets at the top AND
# the bottom, or the fit degenerates into an offset nobody can distinguish.
TARGETS='Tuesday:62,93,142,115
dreamt:79,214,152,236
flooded:216,333,292,355
kept:198,575,241,595
woke:78,693,130,715
grey:226,814,270,835'

# --- the page at the resolution in use ------------------------------------
# Box-downscale by an integer factor, byte-identical to ink.rs::to_png:
# acc/(f*f) over an f x f block, output dimensions truncated. Pure stdlib —
# PIL is not installed in this tree and adding it for one resample is silly.
DOWNSCALE=$(cat <<'PY'
import struct, sys, zlib

def read_gray(path):
    d = open(path, "rb").read()
    if d[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"recalibrate: not a PNG: {path}")
    w = h = None
    idat = bytearray()
    i = 8
    while i < len(d):
        ln = struct.unpack(">I", d[i:i+4])[0]
        typ, body = d[i+4:i+8], d[i+8:i+8+ln]
        if typ == b"IHDR":
            w, h, depth, color, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if (depth, color, interlace) != (8, 0, 0):
                raise SystemExit("recalibrate: want 8-bit non-interlaced grayscale")
        elif typ == b"IDAT":
            idat += body
        i += 12 + ln
    raw = zlib.decompress(bytes(idat))
    px, prev, p = bytearray(w * h), bytearray(w), 0
    for y in range(h):
        ft = raw[p]; p += 1
        line = bytearray(raw[p:p+w]); p += w
        if ft == 1:
            for x in range(1, w):
                line[x] = (line[x] + line[x-1]) & 0xFF
        elif ft == 2:
            for x in range(w):
                line[x] = (line[x] + prev[x]) & 0xFF
        elif ft == 3:
            for x in range(w):
                a = line[x-1] if x else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 0xFF
        elif ft == 4:
            for x in range(w):
                a = line[x-1] if x else 0
                c = prev[x-1] if x else 0
                b = prev[x]
                pa, pb, pc = abs(b-c), abs(a-c), abs(a+b-2*c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 0xFF
        elif ft:
            raise SystemExit(f"recalibrate: bad PNG filter {ft}")
        px[y*w:(y+1)*w] = line
        prev = line
    return w, h, px

def write_gray(path, w, h, px):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw += px[y*w:(y+1)*w]
    def chunk(typ, body):
        return (struct.pack(">I", len(body)) + typ + body
                + struct.pack(">I", zlib.crc32(typ + body) & 0xFFFFFFFF))
    with open(path, "wb") as fh:
        fh.write(b"\x89PNG\r\n\x1a\n")
        fh.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0)))
        fh.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        fh.write(chunk(b"IEND", b""))

src, dst, cap = sys.argv[1], sys.argv[2], int(sys.argv[3])
w, h, px = read_gray(src)
f = max(-(-max(w, h) // cap), 1)          # ink.rs: div_ceil(cap).max(1)
ow, oh, n = w // f, h // f, f * f
out = bytearray(ow * oh)
for oy in range(oh):
    base, row = oy * f, oy * ow
    for ox in range(ow):
        acc, c0 = 0, ox * f
        for sy in range(f):
            r = (base + sy) * w + c0
            acc += sum(px[r:r+f])
        out[row + ox] = acc // n
write_gray(dst, ow, oh, out)
sys.stderr.write(f"recalibrate: {src} {w}x{h} -> {dst} {ow}x{oh} (cap {cap}, f={f})\n")
print(f)
PY
)

F=$(python3 -c "$DOWNSCALE" "$PAGE" "$OUT/page-cap$CAP.png" "$CAP")
IMG="$OUT/page-cap$CAP.png"

# --- probe ----------------------------------------------------------------
: > "$OUT/probes.log"
n=0
echo "$TARGETS" | while IFS=: read -r word box; do
    [ -n "$word" ] || continue
    [ "$n" -eq 0 ] || sleep "$DELAY"
    n=1
    echo "=== target $word expect $box" | tee -a "$OUT/probes.log"
    scripts/probe-grounding.py "$IMG" "the word $word" \
        --provider "$PROVIDER" --expect "$box" \
        --trials "$TRIALS" --delay "$DELAY" 2>&1 | tee -a "$OUT/probes.log"
done

# --- fit ------------------------------------------------------------------
# y_model ~= k * y_true through the origin. An offset fit runs alongside it as
# the control: if the intercept is where the error lives, a scale constant is
# the wrong shape of correction and dividing by k makes the top of the page
# worse to help the bottom.
python3 - "$OUT/probes.log" "$CAP" "$F" <<'PY' | tee "$OUT/fit.txt"
import re, sys

log, cap, f = open(sys.argv[1]).read(), sys.argv[2], sys.argv[3]
OPS = r"circle|box|arrow|underline|strike|tick|cross|note"
samples, word, exp, pending = [], None, None, False
for line in log.splitlines():
    m = re.match(r"=== target (\S+) expect (\S+)", line)
    if m:
        word, exp = m.group(1), [int(v) for v in m.group(2).split(",")]
        continue
    m = re.match(r"\[(\d+)\]", line)
    if m:
        pending = True
        continue
    m = re.match(rf"\s+({OPS})\s+(-?\d+)\s+(-?\d+)", line)
    if m and word and pending:
        pending = False
        kind, mx, my = m.group(1), int(m.group(2)), int(m.group(3))
        if kind == "box":                 # x y w h -> centre needs w/h
            v = [int(t) for t in line.split()[1:5]]
            mx, my = v[0] + v[2] // 2, v[1] + v[3] // 2
        samples.append((word, (exp[0]+exp[2])/2, (exp[1]+exp[3])/2, mx, my))

if not samples:
    raise SystemExit("recalibrate: no usable samples in the log")

def fit_scale(t, m):
    return sum(a*b for a, b in zip(t, m)) / sum(a*a for a in t)

def fit_line(t, m):
    n = len(t)
    mt, mm = sum(t)/n, sum(m)/n
    den = sum((a-mt)**2 for a in t)
    b = sum((a-mt)*(c-mm) for a, c in zip(t, m)) / den if den else 0.0
    return mm - b*mt, b

def rms(t, m, k):
    return (sum((c/k - a)**2 for a, c in zip(t, m)) / len(t)) ** 0.5

tx = [s[1] for s in samples]; ty = [s[2] for s in samples]
mx = [s[3] for s in samples]; my = [s[4] for s in samples]

print(f"\nrecalibrate: cap {cap}, f={f}, {len(samples)} usable samples\n")
print(f"{'word':9s} {'y_true':>7s} {'y_model':>8s} {'dy':>6s}   "
      f"{'x_true':>7s} {'x_model':>8s} {'dx':>6s}")
for w_, xt, yt, xm, ym in samples:
    print(f"{w_:9s} {yt:7.0f} {ym:8d} {ym-yt:+6.0f}   "
          f"{xt:7.0f} {xm:8d} {xm-xt:+6.0f}")

for axis, t, m in (("y", ty, my), ("x", tx, mx)):
    k = fit_scale(t, m)
    a, b = fit_line(t, m)
    print(f"\n  {axis}: scale-only fit  k = {k:.4f}"
          f"   (residual RMS {rms(t, m, k):.1f} units, "
          f"uncorrected {rms(t, m, 1.0):.1f})")
    print(f"  {axis}: offset+scale fit  {axis}_model = {a:+.1f} + {b:.4f}*{axis}_true"
          f"   (intercept {abs(a)/10:.2f}% of the page)")
print(f"\n  y at 1.025 (the old constant): residual RMS {rms(ty, my, 1.025):.1f} units")
print("\nrecalibrate: set RIDDLE_GROUND_Y_SCALE to the y scale-only k above,"
      "\n             or leave it at 1.0 if |k-1| is inside the residual noise.")
PY
