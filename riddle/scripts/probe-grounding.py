#!/usr/bin/env python3
"""Measure how accurately a model POINTS at a named thing in a page image.

The whole drawing modality rests on one number: grounding error as a fraction
of image dimension. The budget is ±2-3%; worse than ~5% and region-level
annotation stops being usable (docs/DRAWING-MODALITY.md). This is the cheap
experiment that produces that number, before any of it is built.

  RIDDLE_OPENAI_KEY=… scripts/probe-grounding.py PAGE.png "the word rain" \\
      --provider groq --expect 340,180,520,240 --trials 8

Config comes from the same env the app reads: RIDDLE_OPENAI_KEY (required —
with no key this prints usage and exits, and makes no network call),
RIDDLE_OPENAI_BASE, RIDDLE_OPENAI_MODEL, RIDDLE_OPENAI_REASONING. --provider
names one of the two vision models the README settles on and wins over the
env, so one shell can probe both.

--expect is the target box in the SAME normalized 0-1000 space the model
answers in, x0,y0,x1,y1. Normalized units ARE tenths of a percent of the
dimension, so error in percent is just (units / 10) — no image size needed.
Without --expect the ops are printed and nothing is scored.

Non-streaming on purpose: this measures pointing, not the SSE path
(mock-oracle-draw.py covers that), and one response per trial keeps N cheap.
"""
import argparse
import base64
import json
import math
import os
import re
import struct
import sys
import time
import urllib.error
import urllib.request

OPEN, CLOSE = "⟦", "⟧"

# The probe's OWN copy of the protocol, deliberately not shared with the Rust
# constant: iterating on the wording is the point of the experiment, and a
# rebuild per edit would make that expensive.
DRAW_PROTOCOL = """You are looking at a page of handwriting. You answer by MARKING the page, never by describing it.

Reply with exactly this block and nothing else — no explanation, no code fence:

""" + OPEN + """draw
<one op per line>
""" + CLOSE + """

The ops, and nothing outside this list:
  circle cx cy r
  box x y w h
  arrow x1 y1 x2 y2
  underline x1 y1 x2 y2
  strike x1 y1 x2 y2
  tick x y
  cross x y
  note x y "short text"

All coordinates are integers from 0 to 1000, normalized independently against the image's width and height, origin at the TOP-LEFT: x=0 is the left edge, x=1000 the right edge, y=0 the top, y=1000 the bottom. A circle's r is normalized against the smaller axis."""

# The two vision models the README settles on. Groq's qwen thinks by default
# and Groq's default reasoning_format is "raw", so its chain of thought arrives
# inline in <think>…</think> inside content — see strip_think. We leave
# thinking ON here (spatial reasoning is exactly where it earns its cost) and
# strip it, rather than sending reasoning_effort=none as the app does.
PRESETS = {
    "groq": ("https://api.groq.com/openai/v1", "qwen/qwen3.6-27b", None),
    "cerebras": ("https://api.cerebras.ai/v1", "gemma-4-31b", None),
}

ARITY = {"circle": 3, "box": 4, "arrow": 4, "underline": 4, "strike": 4,
         "tick": 2, "cross": 2}

THINK = re.compile(r"<think>.*?</think>", re.S)


def strip_think(s):
    """Remove inline reasoning before anything tries to read ops out of it."""
    s = THINK.sub("", s)
    # Some gateways emit only the closing tag (the opener is swallowed by the
    # reasoning channel); everything before it is thought, not answer.
    if "</think>" in s:
        s = s.rsplit("</think>", 1)[1]
    # An unterminated opener means the budget ran out mid-thought: there is no
    # answer after it, and parsing on would read reasoning as geometry.
    if "<think>" in s:
        s = s.split("<think>", 1)[0]
    return s.strip()


def draw_block(text):
    """The body of the ⟦draw …⟧ block, or None."""
    i = text.find(OPEN)
    if i < 0:
        return None
    j = text.find(CLOSE, i)
    if j < 0:
        return None
    body = text[i + len(OPEN):j]
    head, _, rest = body.partition("\n")
    return rest if head.strip().lower() == "draw" else None


def parse_ops(block):
    """Mirrors ops.rs::parse_ops — out-of-range is a DROP, not a clamp, so the
    probe scores exactly the marks the app would have rendered."""
    ops, dropped = [], 0
    for line in block.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("```"):
            continue
        op = parse_op(line)
        if op:
            ops.append(op)
        else:
            dropped += 1
    return ops, dropped


def parse_op(line):
    tok = line.split()
    if not tok:
        return None
    kind = tok[0].lower()
    if kind == "note":
        if len(tok) < 3 or line.count('"') < 2:
            return None
        v = nums(tok[1:3])
        text = line[line.find('"') + 1:line.rfind('"')].strip()[:60]
        return (kind, v, text) if v and text else None
    n = ARITY.get(kind)
    if n is None or len(tok) - 1 != n:
        return None
    v = nums(tok[1:])
    if not v:
        return None
    if kind == "circle" and v[2] <= 0:
        return None
    if kind == "box" and (v[2] <= 0 or v[3] <= 0):
        return None
    return (kind, v, "")


def nums(toks):
    out = []
    for t in toks:
        try:
            v = int(t.rstrip(","))
        except ValueError:
            return None
        if not 0 <= v <= 1000:
            return None
        out.append(v)
    return out


def anchor(op):
    """Where the op actually points — the one coordinate worth scoring."""
    kind, v, _ = op
    if kind == "circle":
        return v[0], v[1]
    if kind == "box":
        return v[0] + v[2] // 2, v[1] + v[3] // 2
    if kind == "arrow":
        return v[2], v[3]  # the head, not the tail
    if kind in ("underline", "strike"):
        return (v[0] + v[2]) // 2, (v[1] + v[3]) // 2
    return v[0], v[1]  # tick, cross, note


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n" or head[12:16] != b"IHDR":
        raise SystemExit(f"probe: not a PNG: {path}")
    return struct.unpack(">II", head[16:24])


def fmt_op(op, w, h):
    kind, v, text = op
    ax, ay = anchor(op)
    px, py = ax * w // 1000, ay * h // 1000
    line = f"  {kind:9s} {' '.join(str(x) for x in v)}"
    if text:
        line += f' "{text}"'
    line = f"{line:44s} → anchor ({px},{py})px"
    if kind == "circle":
        line += f" r={v[2] * min(w, h) // 1000}px"
    return line


def ask(base, key, model, system, prompt, img_b64, max_tokens, reasoning, timeout):
    """One chat completion. Retries the token-cap field once, exactly as
    oracle.rs does: Cerebras dropped max_tokens for max_completion_tokens."""
    def send(cap_field):
        body = {
            "model": model,
            "stream": False,
            cap_field: max_tokens,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url", "image_url": {
                        "url": "data:image/png;base64," + img_b64}},
                ]},
            ],
        }
        if reasoning:
            body["reasoning_effort"] = reasoning
        req = urllib.request.Request(
            base.rstrip("/") + "/chat/completions",
            data=json.dumps(body).encode(),
            # Cerebras sits behind Cloudflare, which answers urllib's default
            # "Python-urllib/3.x" agent with a 403 (code 1010) before the
            # request ever reaches the API — it reads as an auth failure but
            # is not one. Any ordinary agent string gets through.
            headers={"Authorization": "Bearer " + key,
                     "Content-Type": "application/json",
                     "User-Agent": "riddle-probe/0.3"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)

    try:
        return send("max_tokens")
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        if e.code == 400 and ("max_tokens" in detail or "max_completion_tokens" in detail):
            return send("max_completion_tokens")
        raise SystemExit(f"probe: http {e.code}: {detail.strip()[:400]}")


USAGE = """probe-grounding.py IMAGE.png TARGET [options]

Measures how accurately the model points at TARGET, in percent of image
dimension. Budget: ±2-3% good, >5% kills the drawing modality.

  RIDDLE_OPENAI_KEY   required — nothing is sent without it
  RIDDLE_OPENAI_BASE  provider base URL (…/v1)
  RIDDLE_OPENAI_MODEL model id
  RIDDLE_OPENAI_REASONING  optional reasoning_effort

  --provider groq|cerebras   the README's two vision models; wins over env
  --expect x0,y0,x1,y1       target box in normalized 0-1000 coords
  --trials N                 default 5; one sample says nothing
  --delay S                  pause between trials (Cerebras free tier: 5/min)

Example:
  RIDDLE_OPENAI_KEY=$KEY scripts/probe-grounding.py page.png "the word rain" \\
      --provider groq --expect 340,180,520,240 --trials 8
"""


def main():
    ap = argparse.ArgumentParser(add_help=False)
    ap.add_argument("image", nargs="?")
    ap.add_argument("target", nargs="?")
    ap.add_argument("--provider", choices=sorted(PRESETS))
    ap.add_argument("--expect")
    ap.add_argument("--trials", type=int, default=5)
    ap.add_argument("--delay", type=float, default=0.0)
    ap.add_argument("--max-tokens", type=int, default=2000)
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--reasoning")
    ap.add_argument("-h", "--help", action="store_true")
    a = ap.parse_args()

    base, model, reasoning = (PRESETS[a.provider] if a.provider else (None, None, None))
    base = base or os.environ.get("RIDDLE_OPENAI_BASE")
    model = model or os.environ.get("RIDDLE_OPENAI_MODEL")
    reasoning = a.reasoning or reasoning or os.environ.get("RIDDLE_OPENAI_REASONING") or None
    key = os.environ.get("RIDDLE_OPENAI_KEY")

    problems = []
    if a.help or not a.image or not a.target:
        problems.append("image and target are required")
    if not key:
        problems.append("RIDDLE_OPENAI_KEY is not set")
    if not base or not model:
        problems.append("no base/model: pass --provider or set RIDDLE_OPENAI_{BASE,MODEL}")
    expect = None
    if a.expect:
        try:
            expect = [int(v) for v in a.expect.split(",")]
            if len(expect) != 4 or not all(0 <= v <= 1000 for v in expect):
                raise ValueError
        except ValueError:
            problems.append("--expect wants x0,y0,x1,y1 as integers 0-1000")
    if a.trials < 1:
        problems.append("--trials must be >= 1")
    if problems:
        sys.stderr.write(USAGE)
        for p in problems:
            sys.stderr.write(f"\nprobe: {p}")
        sys.stderr.write("\n")
        return 2

    w, h = png_size(a.image)
    with open(a.image, "rb") as f:
        img_b64 = base64.b64encode(f.read()).decode()
    prompt = (f"Circle {a.target}. Put the centre of the circle exactly on it, "
              f"and size the radius to just enclose it.")

    print(f"probe: {a.image} {w}x{h}px  model={model}  base={base}  "
          f"reasoning={reasoning or '-'}  trials={a.trials}")
    if expect:
        cx, cy = (expect[0] + expect[2]) / 2, (expect[1] + expect[3]) / 2
        print(f"probe: target box {expect} centre ({cx:.0f},{cy:.0f}) "
              f"= ({cx * w / 1000:.0f},{cy * h / 1000:.0f})px")

    errors, hits, misparsed, drops = [], 0, 0, 0
    for i in range(a.trials):
        if i and a.delay:
            time.sleep(a.delay)
        t0 = time.time()
        data = ask(base, key, model, DRAW_PROTOCOL, prompt, img_b64,
                   a.max_tokens, reasoning, a.timeout)
        dt = time.time() - t0
        content = strip_think(data["choices"][0]["message"].get("content") or "")
        block = draw_block(content)
        if block is None:
            misparsed += 1
            print(f"[{i + 1}] {dt:5.1f}s  NO DRAW BLOCK: {content[:160]!r}")
            continue
        ops, dropped = parse_ops(block)
        drops += dropped
        print(f"[{i + 1}] {dt:5.1f}s  {len(ops)} op(s)"
              + (f", {dropped} dropped" if dropped else ""))
        for op in ops:
            print(fmt_op(op, w, h))
        if not ops:
            misparsed += 1
            continue
        if expect:
            ax, ay = anchor(ops[0])
            # Normalized units are tenths of a percent of the dimension.
            dx, dy = (ax - (expect[0] + expect[2]) / 2) / 10, (ay - (expect[1] + expect[3]) / 2) / 10
            err = math.hypot(dx, dy)
            inside = expect[0] <= ax <= expect[2] and expect[1] <= ay <= expect[3]
            hits += inside
            errors.append(err)
            print(f"      error dx={dx:+.1f}% dy={dy:+.1f}% |{err:.1f}%|"
                  f"  {'INSIDE' if inside else 'outside'} the target box")

    print(f"\nprobe: {a.trials} trial(s), {misparsed} unusable, {drops} op(s) dropped")
    if not errors:
        return 1 if misparsed == a.trials else 0
    mean, worst = sum(errors) / len(errors), max(errors)
    print(f"probe: mean error {mean:.1f}%  worst {worst:.1f}%  "
          f"inside the box {hits}/{len(errors)}")
    # The verdict the design doc asks for, on the mean rather than one sample.
    verdict = ("within budget" if mean <= 3 else
               "usable, at the edge" if mean <= 5 else
               "OVER BUDGET — region annotation is not viable on this model")
    print(f"probe: {verdict} (budget ±2-3%, >5% kills it)")
    return 0 if mean <= 5 else 1


if __name__ == "__main__":
    sys.exit(main())
