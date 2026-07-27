# Plan — a drawing modality: the model annotates the page back

**Status: phases 0-3 built and measured.** See *Results* at the bottom for what
survived contact with a real model, and what the plan got wrong. The design
below is unchanged from the original except where *Results* says otherwise.

The idea: instead of the model replying only in handwriting, let it *mark the
page* — circle a word, arrow from one thing to another, tick a line, write a
note in the margin. The writer then annotates those marks back, and the loop
continues. Both parties draw on the same surface.

The claim this plan rests on: **riddle is already 80% of the way there**, and
the remaining 20% is not "get a model to draw".

---

## Why this is cheap here

The animation layer is already modality-agnostic. `main.rs:657-680` walks a
`WritePlan { strokes: Vec<Vec<(i32,i32)>>, … }` point by point, 26 points per
14 ms tick, stamping brush dots into the surface. It does not know or care that
those polylines came from text. Anything that produces screen-space polylines
gets hand-drawn animation for free.

Today only one producer exists — `plan_reply` (`main.rs:927`), which runs
`script.rs`'s `rasterize_line → thin → trace` pipeline over reply text. Note
that `thin()` and `trace()` take a bare `Line { width, height, mask }`; they
are general raster-skeletonisation, not text code. Only `rasterize_line` is
text-specific.

So: **a second producer that turns annotation ops into polylines is the whole
rendering job.** No changes to the animator, the surface, the display driver,
or the e-ink update path.

Three other pieces already exist:

| need | already there |
|---|---|
| a bracketed out-of-band directive in the stream | `⟦show:N⟧` routing, `oracle.rs:95-133` |
| user strokes as vectors, not pixels | `Ink::stroke_list()`, `ink.rs:24` |
| per-page persistence of strokes | `memory.rs`, via `store.append(…, &turn_strokes)` |

---

## Design

### Don't parse SVG. Don't emit ASCII art.

Free-form SVG makes the model draw blind — it never sees what it rendered, so
there is no error correction, and quality falls off a cliff past simple
geometry. It also hands you a path/transform/arc parser and an
arbitrary-markup attack surface. ASCII art is worse: a text grid the model
also cannot see, fighting the tokenizer over whitespace.

**Annotation is not drawing.** To mark up my page the model does not need to
draw well, it needs to *point* well — which is spatial grounding, a trained
VLM capability (normalized-coordinate boxes and points), not an emergent one.
So constrain the output to a tiny op vocabulary and generate the geometry
locally.

### The op vocabulary

One op per line inside a `⟦draw … ⟧` block. All coordinates are integers in
`0..=1000`, normalized independently against the sent image's width and
height (the same convention the grounding-trained models are used to).

```
circle    cx cy r              # r also normalized, against the smaller axis
box       x y w h
arrow     x1 y1 x2 y2          # head drawn at (x2,y2)
underline x1 y1 x2 y2
strike    x1 y1 x2 y2
tick      x y
cross     x y
note      x y "short text"     # anchored top-left, handwritten
```

Eight primitives. Trivially parseable, impossible to render badly, and each
expands analytically to a polyline: a circle is parametric, an arrow is three
segments, a box is a five-point path. `note` is the only one that needs
`script.rs` — reuse `rasterize_line → thin → trace` anchored at `(x,y)`.

Apply the same deterministic wobble `plan_reply` uses (`main.rs:939-943`),
plus a light per-point offset, so straight lines read as drawn rather than
plotted. Without this the marks will look like clip-art next to the
handwriting and the whole illusion dies.

### Where the coordinates land — the bug you will hit

`ink.rs:115-121` crops to the ink bounding box and downscales by `f`, then
**throws away `x0`, `y0` and `f`**. Every coordinate the model returns is in
the space of that cropped, downscaled image, not the screen. The inverse map
is:

```
screen_x = x0 + (u / 1000) * w * f
screen_y = y0 + (v / 1000) * h * f
```

`to_png` must return the transform. Suggested:

```rust
pub struct PageView { pub x0: i32, pub y0: i32, pub f: usize, pub w: usize, pub h: usize }
impl PageView { pub fn to_screen(&self, u: i32, v: i32) -> (i32, i32) { … } }
```

Second consequence of the crop: **the model can only annotate inside the ink
bbox.** There is no margin to write a note into. For this mode, send a much
wider crop — the full page, or the bbox padded to at least 15% of the page on
each side. The crop exists as a token/latency optimisation, and we measured
that it buys very little: Cerebras caps image cost at 280 tokens by formula,
Qwen lands around 100-300. Relaxing it is nearly free.

### Streaming the block out

`StreamParser` (`oracle.rs:55-171`) is fed the *running* accumulated text and
must emit each event exactly once. Two rules differ from `⟦show:N⟧`:

- `show` is only honoured when it **leads** the reply (you cannot un-ink prose
  already drawn). A draw block should be allowed to follow prose — "I see what
  you mean, ⟦draw …⟧" is the natural shape.
- Therefore `strip_directives` (`oracle.rs:590`), which currently deletes any
  `⟦…⟧` span appearing after prose, must special-case `⟦draw`: extract it as an
  event instead of discarding it.

The incremental constraint is the fiddly part: on seeing `⟦draw` with no
closing `⟧` yet, **hold** — emit nothing and do not advance `delivered` past
it — until the block closes or `done` arrives. A half-streamed block must
never render.

Add to the `Event` enum (`oracle.rs:44`):

```rust
Draw(Vec<Op>),
```

### The shared-canvas problem

This is the part that decides whether the modality actually works past turn
two, and it is the same lesson the ⁂ transcription postscript already encodes.

If each turn sends a flattened raster, the model must re-read its own previous
marks out of pixels. That degrades fast, and "move that arrow down a bit" or
"no, the *second* box" become impossible.

Keep a layer model instead:

- **layer 0** — user strokes. Already vectors (`Ink::stroke_list()`).
- **layer 1** — AI ops. Structured, because you generated them.

Then send the model *both* the composite image **and** a text listing of its
own prior ops with coordinates. Same trick as ⁂: give the model a textual
handle on what is otherwise only pixels, one level up at the geometry.

Persist layer 1 next to the strokes in `memory.rs` — that is a record-schema
change, so decide it before writing the parser.

---

## Phases

Each phase is independently testable, and phases 0-1 need no model at all.

**Phase 0 — renderer, offline.** `ops.rs`: the `Op` enum, a text parser, and
`ops_to_strokes(&[Op], &PageView) -> Vec<Vec<(i32,i32)>>`. Add
`riddle --draw-test "circle 500 500 120"` to render a canned op list to a PNG
on the host. Iterate on the wobble here, with no device and no API key.

**Phase 1 — transform plumbing.** `to_png` returns `PageView`; widen the crop;
thread the view to the reply path. Round-trip unit test: screen point →
normalized → screen, within a pixel.

**Phase 2 — protocol.** `Event::Draw`, `StreamParser` block extraction, and
`DRAW_PROTOCOL` as a third prompt constant beside `PERSONA` and
`MEMORY_PROTOCOL`. Drive it from `scripts/mock-oracle.py` with a canned draw
block — no real provider yet.

**Phase 3 — a real model.** Point it at Qwen 3.6 27B or Gemma 4 31B (see the
README's *Choosing a model*) and find out how well it actually grounds. Turn
thinking **on** for this mode; spatial reasoning is exactly where it earns its
cost, and the output is only a few dozen tokens of geometry.

**Phase 4 — layer state.** Persist ops per page; feed prior ops back as text.

**Phase 5 — the round trip.** User annotates the AI's marks; the composite plus
both layers goes back. This is where the idea is actually proved.

---

## Numbers to design against

- **Draw time.** The animator does 26 points / 14 ms ≈ 1,850 points/s. A
  circle at r=80 is ~250 points; a five-op annotation ≈ 1,500 points ≈ 0.8 s.
  Comfortable.
- **Grounding precision.** Expect ±2-3% of image dimension. On a full-page
  1404-wide view that is ±40 px. Enough to circle a paragraph, not enough to
  point at a character. Design the interaction around regions.
- **Output length.** A few dozen tokens, so latency is all TTFT. The
  throughput differences between providers stay irrelevant.

## Decisions to make before writing code

1. **Is this Tom?** The diary persona is instructed to never mention images and
   to only perceive words. An annotating assistant is a different character and
   arguably a different app. Cleanest is a second mode behind
   `RIDDLE_MODE=annotate` with its own persona, sharing everything below the
   prompt.
2. **Structured outputs vs streaming.** Cerebras supports strict JSON schemas —
   which would guarantee well-formed ops — but its `json_object` mode is *not*
   compatible with streaming. For short geometry output, giving up streaming may
   be the better trade. Worth measuring both.
3. **Op record schema** in `memory.rs`, before phase 2 (see above).
4. **Failure mode.** When an op is malformed or out of range: clamp, drop the
   op, or fail the turn? Clamping silently puts marks in the wrong place;
   dropping is probably right, with the count logged.

## What would kill it

If phase 3 shows grounding is worse than ~5% of image dimension on real
handwriting, region-level annotation stops being usable and the whole idea
needs a different input representation — most likely sending the user's
strokes as *vector coordinates in the prompt* alongside the image, so the model
can reference stroke indices instead of guessing pixel positions. That is a
bigger change, but it is the fallback worth knowing about early.

---

# Results

Phases 0-3 are built. 55 tests pass on the host (`scripts/test-host.sh`, no
tablet, no key) and the armv7 cross-build is clean.

## Grounding is better than the plan budgeted for — with one correction

Measured with `scripts/probe-grounding.py` against Cerebras `gemma-4-31b`, on a
synthetic page whose word boxes are known exactly (rendered by `--draw-test`
from `note` ops, ground truth recovered from the ink bitmap).

| what was asked for | mean error | inside the target box |
|---|---|---|
| a whole line | 0.8% | 5/5 |
| a single word | 0.5 - 2.2% | 4/15 |

Line-level pointing is comfortably inside the ±2-3% budget. Word-level looked
marginal until the errors turned out not to be noise:

**x lands within 0.5% everywhere. y comes back systematically LONG** — ~0% error
at the top of the page, growing to ~2% at the bottom. That is a scale factor,
not imprecision. Fitting 18 samples gives **k = 1.025**, and dividing every
returned y by it moves all six word-level targets from outside their box to
inside. Latency was ~0.5s per call, so the plan's "latency is all TTFT" holds.

This is now `RIDDLE_GROUND_Y_SCALE`, defaulting to 1.0. It is deliberately NOT
on by default: it is fitted to one model on one synthetic page. Re-measure per
model — Groq's qwen will differ — and on real handwriting before trusting it.

The plan's kill criterion (worse than ~5%) is not met. The modality is viable.

## What the plan got wrong

**The ink is drunk before the reply.** The plan says "both parties draw on the
same surface" but never notices that `Drinking` dissolves the writer's ink and
`FadingReply` fades the answer. Marks would have pointed at a blank page. In
annotation mode both are now skipped, which makes the page a genuine shared
canvas — and means it never clears itself. That is unresolved; see below.

**The crop hides the diary's own marks.** `to_png` crops to the ink bbox, and
the marks are not the writer's ink, so the next turn's crop would cut them off
and the model would lose sight of its own previous turn. `to_png` now takes an
extra region to union in.

**Phase 3's advice would have inked the reasoning.** The plan says to turn
thinking on and names Qwen 3.6 27B. On Groq — the only place that model is
served — it thinks by default and `reasoning_format` is `raw`, so the
chain-of-thought arrives in the ordinary content deltas and the quill draws it.
Cerebras `gemma-4-31b` is the default instead; a `<think>` stripper makes Groq
survivable if the A/B is worth running.

**`memory.rs` needs no schema change.** Strokes already go to a per-page sidecar
with the index kept separate, so layer 1 can be an `<id>.ops` sidecar. Decision
3 does not block anything.

## Known-wrong, deliberately left

- **The eraser diverges from the op list.** `erase_point` whitens pixels and
  drops points from the writer's strokes only. Erasing across a mark removes the
  pixels while the op stays live in layer 1 — display and state disagree. Needs
  ownership-aware hit-testing, or a rule that the eraser cannot touch AI marks.
- **The page never clears.** No New Page / Clear gesture in annotation mode.
- **Layer 1 is collected but not yet fed back.** `turn_ops` accumulates and is
  not yet persisted or sent to the model, so phase 4-5 (a model that can refer
  to "the second box") is not done. This is the next thing to build.
- **No cap on marks per turn.** A dense reply is ~2s of animation.

## The second opinion worth acting on

A review of the design (codex, high effort) agreed the op vocabulary beats SVG
and that independent 0-1000 axes are correct, but argued the ops should not
also be the state model: it wants stable IDs per mark, `update`/`remove` as well
as `add`, and an authoritative scene graph that the composite is rendered FROM.
That is the right shape for the multi-turn canvas and is what the eraser
divergence above is really a symptom of. Deferred as an optimisation, not
rejected.

It also flagged that `circle cx cy r` is the wrong primitive — VLMs are trained
to emit boxes, so `encircle x1 y1 x2 y2` should ground better and the renderer
can inscribe the ellipse. Untested; worth an A/B with the probe, and cheap,
because the probe measures exactly this.
