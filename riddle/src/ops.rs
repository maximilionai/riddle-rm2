//! Annotation ops: the marks the diary can make ON the page, as opposed to
//! the prose it writes onto it.
//!
//! The model does not draw. It *points* — which is spatial grounding, a
//! trained VLM capability, rather than an emergent one. So its output is
//! constrained to the tiny vocabulary below and all geometry is generated
//! here, locally. Nothing free-form (SVG, ASCII art) ever reaches a parser:
//! the model cannot see what it rendered, so it could never correct itself,
//! and a path/transform grammar would be an attack surface for no benefit.
//!
//! Coordinates arriving from the model are integers in `0..=1000`, normalized
//! independently against the sent image's width and height — the convention
//! grounding-trained models are tuned for. [`PageView`] maps them back to the
//! screen; see `ink.rs`.

use ab_glyph::FontRef;

use crate::fb::{self, SCREEN_H, SCREEN_W};
use crate::ink::PageView;
use crate::script;

/// One mark. Eight primitives, each expanding analytically to a polyline.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `r` is normalized against the smaller axis.
    Circle { cx: i32, cy: i32, r: i32 },
    Box { x: i32, y: i32, w: i32, h: i32 },
    /// Head is drawn at (`x2`, `y2`).
    Arrow { x1: i32, y1: i32, x2: i32, y2: i32 },
    Underline { x1: i32, y1: i32, x2: i32, y2: i32 },
    Strike { x1: i32, y1: i32, x2: i32, y2: i32 },
    Tick { x: i32, y: i32 },
    Cross { x: i32, y: i32 },
    /// Handwritten margin note, anchored top-left at (`x`, `y`).
    Note { x: i32, y: i32, text: String },
}

impl Op {
    /// The op as the model would have written it — used to hand its own prior
    /// marks back to it as text on the next turn.
    pub fn to_line(&self) -> String {
        match self {
            Op::Circle { cx, cy, r } => format!("circle {cx} {cy} {r}"),
            Op::Box { x, y, w, h } => format!("box {x} {y} {w} {h}"),
            Op::Arrow { x1, y1, x2, y2 } => format!("arrow {x1} {y1} {x2} {y2}"),
            Op::Underline { x1, y1, x2, y2 } => format!("underline {x1} {y1} {x2} {y2}"),
            Op::Strike { x1, y1, x2, y2 } => format!("strike {x1} {y1} {x2} {y2}"),
            Op::Tick { x, y } => format!("tick {x} {y}"),
            Op::Cross { x, y } => format!("cross {x} {y}"),
            Op::Note { x, y, text } => format!("note {x} {y} \"{text}\""),
        }
    }
}

/// Parse the body of a `⟦draw … ⟧` block: one op per line.
///
/// Returns the ops that parsed and how many lines were DROPPED. Dropping is
/// deliberate — clamping a malformed op silently puts a mark in the wrong
/// place, which is worse than a missing mark — and the count is logged by the
/// caller.
pub fn parse_ops(block: &str) -> (Vec<Op>, usize) {
    let mut ops = Vec::new();
    let mut dropped = 0usize;
    for line in block.lines() {
        let line = line.trim();
        // Blank lines and the model's stray prose/fencing are not failures.
        if line.is_empty() || line.starts_with('#') || line.starts_with("```") {
            continue;
        }
        match parse_op(line) {
            Some(op) => ops.push(op),
            None => dropped += 1,
        }
    }
    (ops, dropped)
}

/// The longest margin note worth writing: past this it stops being an
/// annotation and starts covering the page it is annotating.
const NOTE_MAX: usize = 60;

fn parse_op(line: &str) -> Option<Op> {
    let mut it = line.split_whitespace();
    let kind = it.next()?.to_ascii_lowercase();

    // `note` is the one op with a trailing string, so it is parsed off the raw
    // line rather than the whitespace split.
    if kind == "note" {
        let x = num(it.next()?)?;
        let y = num(it.next()?)?;
        let open = line.find('"')?;
        let close = line.rfind('"')?;
        if close <= open {
            return None;
        }
        let text: String = line[open + 1..close].trim().chars().take(NOTE_MAX).collect();
        if text.is_empty() {
            return None;
        }
        return Some(Op::Note { x, y, text });
    }

    let n: Vec<i32> = it.map(num).collect::<Option<Vec<_>>>()?;
    let op = match (kind.as_str(), n.len()) {
        ("circle", 3) => Op::Circle { cx: n[0], cy: n[1], r: n[2] },
        ("box", 4) => Op::Box { x: n[0], y: n[1], w: n[2], h: n[3] },
        ("arrow", 4) => Op::Arrow { x1: n[0], y1: n[1], x2: n[2], y2: n[3] },
        ("underline", 4) => Op::Underline { x1: n[0], y1: n[1], x2: n[2], y2: n[3] },
        ("strike", 4) => Op::Strike { x1: n[0], y1: n[1], x2: n[2], y2: n[3] },
        ("tick", 2) => Op::Tick { x: n[0], y: n[1] },
        ("cross", 2) => Op::Cross { x: n[0], y: n[1] },
        _ => return None,
    };
    // A zero-size mark is invisible; a zero-length line is a dropped stroke.
    match &op {
        Op::Circle { r, .. } if *r <= 0 => return None,
        Op::Box { w, h, .. } if *w <= 0 || *h <= 0 => return None,
        _ => {}
    }
    Some(op)
}

/// A coordinate token: an integer inside the normalized range. Out of range is
/// a DROP, not a clamp — a clamped op is a mark confidently placed in the
/// wrong spot, which reads as the diary being wrong rather than silent.
fn num(tok: &str) -> Option<i32> {
    let v: i32 = tok.trim_end_matches(',').parse().ok()?;
    (0..=1000).contains(&v).then_some(v)
}

/// Re-express an op authored against one crop in another crop's coordinates.
///
/// The image sent each turn is a MOVING window: the crop grows as the writer
/// fills the page, so a mark recorded as `500 500` on turn 2 is somewhere else
/// entirely in turn 5's normalized space. Handing the model back the raw
/// numbers would describe marks that are not where the image shows them —
/// worse than saying nothing, because it contradicts what it can see.
///
/// Screen space is the stable frame, so the map is old → screen → new.
pub fn reproject(op: &Op, from: &PageView, to: &PageView) -> Op {
    let p = |x: &i32, y: &i32| {
        let (sx, sy) = from.to_screen(*x, *y);
        to.to_norm(sx, sy)
    };
    // Lengths scale with the ratio of the two crops' spans, on the axis each
    // length is measured against.
    let span_min = |v: &PageView| (v.w * v.f).min(v.h * v.f).max(1) as f32;
    let k_min = span_min(from) / span_min(to);
    let kx = (from.w * from.f).max(1) as f32 / (to.w * to.f).max(1) as f32;
    let ky = (from.h * from.f).max(1) as f32 / (to.h * to.f).max(1) as f32;
    let scale = |v: &i32, k: f32| ((*v as f32 * k).round() as i32).clamp(0, 1000);
    match op {
        Op::Circle { cx, cy, r } => {
            let (x, y) = p(cx, cy);
            Op::Circle { cx: x, cy: y, r: scale(r, k_min) }
        }
        Op::Box { x, y, w, h } => {
            let (a, b) = p(x, y);
            Op::Box { x: a, y: b, w: scale(w, kx), h: scale(h, ky) }
        }
        Op::Arrow { x1, y1, x2, y2 } => {
            let (a, b) = p(x1, y1);
            let (c, d) = p(x2, y2);
            Op::Arrow { x1: a, y1: b, x2: c, y2: d }
        }
        Op::Underline { x1, y1, x2, y2 } => {
            let (a, b) = p(x1, y1);
            let (c, d) = p(x2, y2);
            Op::Underline { x1: a, y1: b, x2: c, y2: d }
        }
        Op::Strike { x1, y1, x2, y2 } => {
            let (a, b) = p(x1, y1);
            let (c, d) = p(x2, y2);
            Op::Strike { x1: a, y1: b, x2: c, y2: d }
        }
        Op::Tick { x, y } => {
            let (a, b) = p(x, y);
            Op::Tick { x: a, y: b }
        }
        Op::Cross { x, y } => {
            let (a, b) = p(x, y);
            Op::Cross { x: a, y: b }
        }
        Op::Note { x, y, text } => {
            let (a, b) = p(x, y);
            Op::Note { x: a, y: b, text: text.clone() }
        }
    }
}

/// Per-model correction for the vertical scale the model actually answers in.
///
/// Measured against gemma-4-31b on a synthetic page with known word boxes
/// (`scripts/probe-grounding.py`): x lands within 0.5% everywhere, but y comes
/// back consistently LONG — the error is ~0 at the top of the page and grows to
/// ~2% at the bottom, which is a scale factor, not noise. 18 samples fitted
/// k = 1.025, and dividing y by it moved every one of six word-level targets
/// from outside its box to inside.
///
/// Left at 1.0 by default because it is fitted to ONE model on ONE synthetic
/// page — font-rendered handwriting, evenly spaced, clean background. Set
/// `RIDDLE_GROUND_Y_SCALE` per model, and re-measure on real pages before
/// trusting it. x has its own knob for symmetry; it has never needed one.
fn calibration() -> (f32, f32) {
    static CAL: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    *CAL.get_or_init(|| {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| (0.5..=2.0).contains(v))
                .unwrap_or(1.0)
        };
        let c = (get("RIDDLE_GROUND_X_SCALE"), get("RIDDLE_GROUND_Y_SCALE"));
        if c != (1.0, 1.0) {
            eprintln!("riddle: grounding calibration x/{} y/{}", c.0, c.1);
        }
        c
    })
}

/// Apply [`calibration`] to every coordinate an op carries. Lengths (`r`, and
/// a box's `w`/`h`) are left alone: the bias is in where the model thinks a
/// point is, not in how big it thinks a thing is.
fn calibrated(op: &Op) -> Op {
    let (kx, ky) = calibration();
    if (kx, ky) == (1.0, 1.0) {
        return op.clone();
    }
    let cx = |v: &i32| ((*v as f32 / kx).round() as i32).clamp(0, 1000);
    let cy = |v: &i32| ((*v as f32 / ky).round() as i32).clamp(0, 1000);
    match op {
        Op::Circle { cx: a, cy: b, r } => Op::Circle { cx: cx(a), cy: cy(b), r: *r },
        Op::Box { x, y, w, h } => Op::Box { x: cx(x), y: cy(y), w: *w, h: *h },
        Op::Arrow { x1, y1, x2, y2 } => {
            Op::Arrow { x1: cx(x1), y1: cy(y1), x2: cx(x2), y2: cy(y2) }
        }
        Op::Underline { x1, y1, x2, y2 } => {
            Op::Underline { x1: cx(x1), y1: cy(y1), x2: cx(x2), y2: cy(y2) }
        }
        Op::Strike { x1, y1, x2, y2 } => {
            Op::Strike { x1: cx(x1), y1: cy(y1), x2: cx(x2), y2: cy(y2) }
        }
        Op::Tick { x, y } => Op::Tick { x: cx(x), y: cy(y) },
        Op::Cross { x, y } => Op::Cross { x: cx(x), y: cy(y) },
        Op::Note { x, y, text } => Op::Note { x: cx(x), y: cy(y), text: text.clone() },
    }
}

/// Expand ops into screen-space polylines for the animator.
///
/// The output goes straight into `WritePlan.strokes`, so this is the entire
/// rendering job for the modality: the animator, surface, display driver and
/// e-ink update path are all untouched.
pub fn ops_to_strokes(ops: &[Op], view: &PageView, font: &FontRef) -> Vec<Vec<(i32, i32)>> {
    // Correct the model's coordinate bias before any geometry is built, so the
    // wobble seed (hashed from the op's text) still keys off what the model
    // actually said and a given mark keeps rendering identically.
    let ops: Vec<Op> = ops.iter().map(calibrated).collect();
    let ops = &ops[..];
    let mut out = Vec::new();
    for op in ops {
        let seed = op_seed(op);
        match op {
            Op::Circle { cx, cy, r } => {
                let (x, y) = fp(view.to_screen(*cx, *cy));
                let r = view.to_screen_len(*r) as f32;
                emit(&mut out, circle(x, y, r, seed));
            }
            Op::Box { x, y, w, h } => {
                let (x0, y0) = fp(view.to_screen(*x, *y));
                let (x1, y1) = fp(view.to_screen(x + w, y + h));
                let mut hand = Hand::new(seed);
                // Nudge the corners: the per-segment bow curves the sides but
                // cannot stop the box being exactly square, which is the tell.
                let n = fb::s(7) as f32;
                let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
                let mut pts: Vec<(f32, f32)> = corners
                    .iter()
                    .map(|&(px, py)| (px + hand.unit() * n, py + hand.unit() * n))
                    .collect();
                // Close onto the start with its own nudge, so the last corner
                // misses the first by a few pixels the way a real one does.
                pts.push((x0 + hand.unit() * n, y0 + hand.unit() * n));
                // One stroke through five points: a box is drawn without
                // lifting, so the corners must inherit the same tremor.
                emit(&mut out, polyline(&pts, &mut hand));
            }
            Op::Arrow { x1, y1, x2, y2 } => {
                let a = fp(view.to_screen(*x1, *y1));
                let b = fp(view.to_screen(*x2, *y2));
                for s in arrow(a, b, seed) {
                    emit(&mut out, s);
                }
            }
            // Underline and strike differ only in where the caller puts them;
            // both are one drawn line, and the wobble is what keeps them from
            // reading as a ruler edge.
            Op::Underline { x1, y1, x2, y2 } | Op::Strike { x1, y1, x2, y2 } => {
                let a = fp(view.to_screen(*x1, *y1));
                let b = fp(view.to_screen(*x2, *y2));
                emit(&mut out, polyline(&overshoot(a, b), &mut Hand::new(seed)));
            }
            Op::Tick { x, y } => {
                let (x, y) = fp(view.to_screen(*x, *y));
                let s = MARK_PX;
                emit(
                    &mut out,
                    polyline(
                        &[(x - 0.45 * s, y - 0.05 * s), (x - 0.10 * s, y + 0.35 * s), (x + 0.55 * s, y - 0.55 * s)],
                        &mut Hand::new(seed),
                    ),
                );
            }
            Op::Cross { x, y } => {
                let (x, y) = fp(view.to_screen(*x, *y));
                let h = MARK_PX * 0.5;
                emit(&mut out, polyline(&[(x - h, y - h), (x + h, y + h)], &mut Hand::new(seed)));
                // Second seed: the two limbs of a cross are separate pen
                // strokes, so they must not share a drift.
                emit(&mut out, polyline(&[(x + h, y - h), (x - h, y + h)], &mut Hand::new(seed ^ 0x5bf0_3635)));
            }
            Op::Note { x, y, text } => {
                let anchor = view.to_screen(*x, *y);
                for s in note(font, text, anchor, seed) {
                    emit(&mut out, s);
                }
            }
        }
    }
    out
}

/// Point spacing along every generated path. The animator draws ~1850
/// points/second and joins consecutive points with `brush_line`, so denser
/// only costs time on a page the writer is watching; sparser still renders,
/// it just stops wobbling visibly.
const STEP: f32 = 2.0;

/// Side length of a tick or a cross. Absolute, not derived from the model's
/// coordinates: it points at a spot, not at a size.
const MARK_PX: f32 = fb::s(52) as f32;

/// Margin notes are prose, but smaller than the diary's replies — a note that
/// matches REPLY_PX reads as a second answer rather than a comment.
const NOTE_PX: f32 = 54.0 * SCREEN_W as f32 / fb::REF_W as f32;

/// Keep a note off the panel edge; `to_screen` can put an anchor anywhere.
const NOTE_MARGIN: i32 = fb::s(36);

fn emit(out: &mut Vec<Vec<(i32, i32)>>, stroke: Vec<(i32, i32)>) {
    // A one-point "stroke" is a dot the animator would stamp in a spot the
    // model never asked for; drop degenerate marks instead.
    if stroke.len() >= 2 {
        out.push(stroke);
    }
}

fn fp((x, y): (i32, i32)) -> (f32, f32) {
    (x as f32, y as f32)
}

/// Deterministic seed for one op's tremor.
///
/// Taken from the op's own text so the same mark always renders identically
/// (ops are handed back to the model as text and may be redrawn on a repaint),
/// while two circles on one page still shake differently.
fn op_seed(op: &Op) -> u32 {
    let mut h = 0x811c_9dc5u32;
    for b in op.to_line().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h | 1
}

/// A hand's tremor, at two scales.
///
/// Per-point white noise looks like a bad printer: a real hand's error is
/// correlated between adjacent points. So the offset is a small per-point
/// jitter added to a drift that is itself a bounded random walk on its own
/// velocity — the drift is what makes the error wander instead of buzz.
///
/// The third and largest scale, the deliberate bow along a segment, lives in
/// [`polyline`]: a random walk averages straight, and averaging straight over
/// 900px still reads as a ruler.
struct Hand {
    seed: u32,
    drift: [f32; 2],
    vel: [f32; 2],
}

/// Per-step change in drift velocity, and the cap on that velocity. Tuned so
/// the drift completes roughly one arc over a few hundred points: faster and
/// it reads as noise again, slower and every mark is a clean curve.
const DRIFT_ACCEL: f32 = 0.005;
const DRIFT_VMAX: f32 = 0.06;
/// Per-point noise on top of the drift. Sub-pixel on purpose — it only ever
/// shows up as the odd rounded pixel, which is what a nib on paper does.
const JITTER: f32 = 0.7;

impl Hand {
    fn new(seed: u32) -> Self {
        Self { seed, drift: [0.0; 2], vel: [0.0; 2] }
    }

    /// The LCG `plan_reply` uses for its per-line wobble. No rand crate, and
    /// the same ops must render identically every run.
    fn unit(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.seed >> 16) % 2049) as f32 / 1024.0 - 1.0
    }

    /// Next offset along the path, with the drift held inside `amp` pixels.
    fn offset(&mut self, amp: f32) -> (f32, f32) {
        for i in 0..2 {
            self.vel[i] = (self.vel[i] + self.unit() * DRIFT_ACCEL).clamp(-DRIFT_VMAX, DRIFT_VMAX);
            self.drift[i] += self.vel[i];
            // Reflect off the envelope rather than clamping to it: a clamped
            // drift sits on the rail and the line goes visibly straight again.
            if self.drift[i].abs() > amp {
                self.drift[i] = self.drift[i].clamp(-amp, amp);
                self.vel[i] = -self.vel[i];
            }
        }
        (self.drift[0] + self.unit() * JITTER, self.drift[1] + self.unit() * JITTER)
    }
}

/// Drift envelope for a path of this length. Long marks are allowed to wander
/// further; a 40px tick that strayed 4px would look broken, not drawn.
fn amp_for(len: f32) -> f32 {
    (len * 0.012).clamp(0.8, 5.0)
}

/// Walk a parametric path of approximate length `len`, sampling `STEP` pixels
/// apart, wobbling each point and clamping it to the panel.
///
/// Each stroke gets a fresh [`Hand`], so drift starts at zero and the FIRST
/// point of every mark lands exactly where the model pointed. Accuracy matters
/// at the start of a mark and nowhere else.
fn walk<F: Fn(f32) -> (f32, f32)>(len: f32, hand: &mut Hand, f: F) -> Vec<(i32, i32)> {
    if len < 1.0 || !len.is_finite() {
        return Vec::new();
    }
    let n = (len / STEP).ceil().max(1.0) as usize;
    let amp = amp_for(len);
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let (px, py) = f(i as f32 / n as f32);
        let (ox, oy) = hand.offset(amp);
        let p = (
            (px + ox).round().clamp(0.0, SCREEN_W as f32 - 1.0) as i32,
            (py + oy).round().clamp(0.0, SCREEN_H as f32 - 1.0) as i32,
        );
        // Repeated points are pure animator time: the brush already covers it.
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

/// One continuous stroke through `pts`, arc-length parameterized so corners do
/// not get denser than the straights.
///
/// Each segment gets a half-sine bulge along its own normal on top of the
/// tremor. The drift walk alone only makes a line *irregular* — it still
/// averages straight, and a 700px underline that averages straight still reads
/// as a ruler. The bulge is what actually curves it. It is zero at both ends of
/// the segment, so the mark still starts and finishes where the model pointed.
fn polyline(pts: &[(f32, f32)], hand: &mut Hand) -> Vec<(i32, i32)> {
    let segs: Vec<f32> = pts.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).collect();
    let total: f32 = segs.iter().sum();
    let bows: Vec<(f32, f32)> = segs
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            if s < 1.0 {
                return (0.0, 0.0);
            }
            // Random in sign and in magnitude, but with a floor: a hand that
            // bowed every line identically would be its own tell, and one that
            // sometimes bowed by nothing would draw the occasional ruler line.
            let u = hand.unit();
            let a = u.signum() * (0.35 + 0.65 * u.abs()) * (s * 0.02).min(14.0);
            let (dx, dy) = (pts[i + 1].0 - pts[i].0, pts[i + 1].1 - pts[i].1);
            (-dy / s * a, dx / s * a)
        })
        .collect();
    walk(total, hand, |t| {
        let mut d = t * total;
        for (i, &s) in segs.iter().enumerate() {
            if d <= s || i + 1 == segs.len() {
                let u = if s > 0.0 { (d / s).clamp(0.0, 1.0) } else { 0.0 };
                let (a, b) = (pts[i], pts[i + 1]);
                let bulge = (std::f32::consts::PI * u).sin();
                return (
                    a.0 + (b.0 - a.0) * u + bows[i].0 * bulge,
                    a.1 + (b.1 - a.1) * u + bows[i].1 * bulge,
                );
            }
            d -= s;
        }
        pts[pts.len() - 1]
    })
}

/// An annotation circle is never round and never closes: it is scribbled round
/// a word at speed. Hence the aspect, the tilt, and the overshoot past the
/// start angle.
fn circle(cx: f32, cy: f32, r: f32, seed: u32) -> Vec<(i32, i32)> {
    let mut hand = Hand::new(seed);
    let tilt = hand.unit() * 0.30;
    let (rx, ry) = (r * (1.06 + 0.10 * hand.unit().abs()), r * 0.88);
    // One low-frequency lobe so the loop is out of round somewhere, not an
    // ellipse with texture on it.
    let lobe = hand.unit() * 0.05;
    // Start upper-left, where a right hand starts a loop.
    let a0 = 2.4 + hand.unit() * 0.5;
    let span = std::f32::consts::TAU + 0.55;
    let (st, ct) = tilt.sin_cos();
    let len = (rx + ry) * 0.5 * span;
    walk(len, &mut hand, |t| {
        let a = a0 + t * span;
        // The radius creeps outward as it goes round, so the overshooting tail
        // crosses OUTSIDE the head rather than landing back on it. A perfectly
        // closed loop is the single loudest clip-art tell on the page.
        let k = (1.0 + 0.07 * t) * (1.0 + lobe * (2.0 * a).sin());
        let (s, c) = a.sin_cos();
        let (ex, ey) = (rx * k * c, ry * k * s);
        (cx + ex * ct - ey * st, cy + ex * st + ey * ct)
    })
}

/// Shaft plus two head barbs, three strokes — the pen lifts between them.
fn arrow(a: (f32, f32), b: (f32, f32), seed: u32) -> Vec<Vec<(i32, i32)>> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    let mut out = vec![polyline(&[a, b], &mut Hand::new(seed))];
    if len < 1.0 {
        return out;
    }
    // Proportional, but clamped: a page-long arrow with a page-scale head is a
    // dart, and a 30px arrow with a 30px head is a blob.
    let barb = (len * 0.22).clamp(fb::s(20) as f32, fb::s(56) as f32);
    let dir = dy.atan2(dx) + std::f32::consts::PI;
    for (i, off) in [-0.47f32, 0.47].into_iter().enumerate() {
        let (s, c) = (dir + off).sin_cos();
        out.push(polyline(&[b, (b.0 + barb * c, b.1 + barb * s)], &mut Hand::new(seed.rotate_left(7 * (i as u32 + 1)) | 1)));
    }
    out
}

/// A drawn underline runs slightly past the word at both ends; a ruled one
/// stops dead on it.
fn overshoot(a: (f32, f32), b: (f32, f32)) -> [(f32, f32); 2] {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    if len < 1.0 {
        return [a, b];
    }
    let e = fb::s(5) as f32 / len;
    [(a.0 - dx * e, a.1 - dy * e), (b.0 + dx * e, b.1 + dy * e)]
}

/// A handwritten margin note: the same rasterize/thin/trace pipeline the reply
/// path uses, just smaller and anchored instead of centred.
fn note(font: &FontRef, text: &str, anchor: (i32, i32), seed: u32) -> Vec<Vec<(i32, i32)>> {
    let (ax, ay) = anchor;
    let max_w = (SCREEN_W as i32 - NOTE_MARGIN - ax).max(fb::s(320)) as f32;
    let line_h = (NOTE_PX * 1.2) as i32;
    let mut hand = Hand::new(seed);
    let mut out = Vec::new();
    let mut y = ay;
    for text in script::wrap(font, text, NOTE_PX, max_w) {
        let mut raster = script::rasterize_line(font, &text, NOTE_PX);
        script::thin(&mut raster);
        // The anchor is where the model pointed, but a wide note anchored near
        // the right edge would pile up against the clamp in `walk`.
        let x = ax.min(SCREEN_W as i32 - NOTE_MARGIN - raster.width as i32).max(0);
        let y0 = y.min(SCREEN_H as i32 - NOTE_MARGIN - raster.height as i32).max(0);
        // One offset per line, as in `plan_reply` — the glyph shapes already
        // carry the hand, so per-point jitter here would only fray them.
        // Straight off the LCG, not through the drift: the drift starts at zero
        // and would take a hundred calls to reach a visible offset.
        let (wx, wy) = ((hand.unit() * 3.0) as i32, (hand.unit() * 3.0) as i32);
        for s in script::trace(&raster) {
            out.push(
                s.iter()
                    .map(|&(sx, sy)| {
                        (
                            (x + sx + wx).clamp(0, SCREEN_W as i32 - 1),
                            (y0 + sy + wy).clamp(0, SCREEN_H as i32 - 1),
                        )
                    })
                    .collect(),
            );
        }
        y += line_h;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontRef<'static> {
        FontRef::try_from_slice(include_bytes!("../fonts/DancingScript.ttf")).unwrap()
    }

    /// A crop that covers 1000x1400 screen pixels starting at (100, 200) —
    /// close to what `to_png` produces for a page of handwriting.
    fn view() -> PageView {
        PageView { x0: 100, y0: 200, f: 2, w: 500, h: 700 }
    }

    #[test]
    fn every_op_kind_parses() {
        let block = "circle 100 200 40\n\
                     box 10 20 300 100\n\
                     arrow 0 0 500 500\n\
                     underline 100 400 600 400\n\
                     strike 100 500 600 500\n\
                     tick 700 300\n\
                     cross 720 320\n\
                     note 800 100 \"quite\"";
        let (ops, dropped) = parse_ops(block);
        assert_eq!(dropped, 0);
        assert_eq!(
            ops,
            vec![
                Op::Circle { cx: 100, cy: 200, r: 40 },
                Op::Box { x: 10, y: 20, w: 300, h: 100 },
                Op::Arrow { x1: 0, y1: 0, x2: 500, y2: 500 },
                Op::Underline { x1: 100, y1: 400, x2: 600, y2: 400 },
                Op::Strike { x1: 100, y1: 500, x2: 600, y2: 500 },
                Op::Tick { x: 700, y: 300 },
                Op::Cross { x: 720, y: 320 },
                Op::Note { x: 800, y: 100, text: "quite".into() },
            ]
        );
        // Every op round-trips through the text form fed back to the model.
        for op in &ops {
            assert_eq!(parse_ops(&op.to_line()).0, vec![op.clone()]);
        }
    }

    #[test]
    fn note_keeps_spaces_inside_the_quotes() {
        let (ops, dropped) = parse_ops("note 40 900 \"  a word about this  \"");
        assert_eq!(dropped, 0);
        assert_eq!(ops, vec![Op::Note { x: 40, y: 900, text: "a word about this".into() }]);
    }

    #[test]
    fn malformed_lines_drop_and_are_counted() {
        let cases = [
            "scribble 10 20 30",     // unknown op
            "circle 10 20",          // wrong arity
            "circle 10 20 30 40",    // wrong arity
            "arrow 0 0 500",         // wrong arity
            "circle ten 20 30",      // non-numeric
            "tick 10 x",             // non-numeric
            "circle 100 100 0",      // invisible
            "box 10 10 0 40",        // invisible
            "note 10 20 unquoted",   // note without quotes
            "note 10 20 \"\"",       // empty note
        ];
        for line in cases {
            let (ops, dropped) = parse_ops(line);
            assert!(ops.is_empty() && dropped == 1, "{line:?} should have dropped");
        }
    }

    #[test]
    fn out_of_range_coordinates_drop_rather_than_clamp() {
        // A clamped op is a mark confidently placed in the wrong spot.
        for line in ["circle 1001 200 40", "circle 100 -1 40", "arrow 0 0 1200 500", "tick 5000 5000"] {
            let (ops, dropped) = parse_ops(line);
            assert!(ops.is_empty() && dropped == 1, "{line:?} should have dropped");
        }
    }

    #[test]
    fn blank_lines_and_fences_are_not_failures() {
        let (ops, dropped) = parse_ops("```\n\n  \ntick 10 10\n# a comment\n\n```\n");
        assert_eq!(dropped, 0);
        assert_eq!(ops, vec![Op::Tick { x: 10, y: 10 }]);
    }

    #[test]
    fn circle_loops_around_its_centre() {
        let v = view();
        let (cx, cy) = v.to_screen(500, 500);
        let r = v.to_screen_len(120);
        assert!(r > 50, "test view should give a visible radius, got {r}");
        let strokes = ops_to_strokes(&[Op::Circle { cx: 500, cy: 500, r: 120 }], &v, &font());
        assert_eq!(strokes.len(), 1);
        let pts = &strokes[0];
        assert!(pts.len() > 100, "circle should be densely sampled, got {}", pts.len());
        let mut quadrants = [false; 4];
        for &(x, y) in pts {
            let d = ((x - cx) as f32).hypot((y - cy) as f32);
            assert!(
                d > 0.7 * r as f32 - 10.0 && d < 1.4 * r as f32 + 10.0,
                "point ({x},{y}) is {d} from centre, radius {r}"
            );
            quadrants[(2 * (y > cy) as usize) + (x > cx) as usize] = true;
        }
        assert!(quadrants.iter().all(|&q| q), "loop does not go all the way round: {quadrants:?}");
        // It comes back to the start, but overshoots past it rather than
        // meeting it — a perfectly closed loop is the loudest clip-art tell.
        let (a, b) = (pts[0], pts[pts.len() - 1]);
        let gap = ((a.0 - b.0) as f32).hypot((a.1 - b.1) as f32);
        assert!(gap > 4.0, "loop closed perfectly: gap {gap}");
        assert!(gap < r as f32, "loop overshot into a spiral: gap {gap}");
    }

    #[test]
    fn arrow_is_a_shaft_and_two_barbs() {
        let v = view();
        let strokes = ops_to_strokes(&[Op::Arrow { x1: 100, y1: 100, x2: 700, y2: 600 }], &v, &font());
        assert_eq!(strokes.len(), 3, "shaft + two head barbs");
        let head = v.to_screen(700, 600);
        // Both barbs start at the head, and neither is longer than the shaft.
        let shaft_len = {
            let (a, b) = (v.to_screen(100, 100), head);
            ((a.0 - b.0) as f32).hypot((a.1 - b.1) as f32)
        };
        for barb in &strokes[1..] {
            let d = ((barb[0].0 - head.0) as f32).hypot((barb[0].1 - head.1) as f32);
            assert!(d < 4.0, "barb starts {d}px off the head");
            let tip = barb[barb.len() - 1];
            let bl = ((tip.0 - head.0) as f32).hypot((tip.1 - head.1) as f32);
            assert!(bl > 10.0 && bl < shaft_len * 0.5, "barb length {bl} against shaft {shaft_len}");
        }
    }

    #[test]
    fn ops_render_identically_every_time() {
        let v = view();
        let f = font();
        let ops = vec![
            Op::Circle { cx: 300, cy: 300, r: 90 },
            Op::Box { x: 100, y: 600, w: 300, h: 120 },
            Op::Arrow { x1: 50, y1: 50, x2: 400, y2: 400 },
            Op::Underline { x1: 100, y1: 800, x2: 800, y2: 810 },
            Op::Strike { x1: 100, y1: 850, x2: 800, y2: 850 },
            Op::Tick { x: 900, y: 200 },
            Op::Cross { x: 900, y: 400 },
            Op::Note { x: 200, y: 900, text: "hm".into() },
        ];
        assert_eq!(ops_to_strokes(&ops, &v, &f), ops_to_strokes(&ops, &v, &f));
        // ...and two identical shapes in different places do not share a tremor.
        let a = ops_to_strokes(&[Op::Circle { cx: 300, cy: 300, r: 90 }], &v, &f);
        let b = ops_to_strokes(&[Op::Circle { cx: 600, cy: 300, r: 90 }], &v, &f);
        let shifted: Vec<Vec<(i32, i32)>> = b
            .iter()
            .map(|s| s.iter().map(|&(x, y)| (x - (v.to_screen(600, 0).0 - v.to_screen(300, 0).0), y)).collect())
            .collect();
        assert_ne!(a, shifted, "two circles wobbled in lockstep");
    }

    #[test]
    fn every_point_stays_on_screen() {
        let f = font();
        // A view deliberately hanging off the bottom-right of the panel.
        let v = PageView { x0: 900, y0: 1500, f: 3, w: 400, h: 400 };
        let ops = vec![
            Op::Circle { cx: 1000, cy: 1000, r: 200 },
            Op::Box { x: 900, y: 900, w: 100, h: 100 },
            Op::Arrow { x1: 0, y1: 0, x2: 1000, y2: 1000 },
            Op::Underline { x1: 0, y1: 1000, x2: 1000, y2: 1000 },
            Op::Tick { x: 1000, y: 1000 },
            Op::Cross { x: 0, y: 0 },
            Op::Note { x: 990, y: 990, text: "a note pushed off the edge".into() },
        ];
        let strokes = ops_to_strokes(&ops, &v, &f);
        assert!(!strokes.is_empty());
        for s in &strokes {
            assert!(s.len() >= 2, "degenerate stroke of {} points", s.len());
            for &(x, y) in s {
                assert!(
                    (0..SCREEN_W as i32).contains(&x) && (0..SCREEN_H as i32).contains(&y),
                    "({x},{y}) is off the panel"
                );
            }
        }
    }

    #[test]
    fn note_writes_handwriting() {
        let v = view();
        let strokes = ops_to_strokes(&[Op::Note { x: 100, y: 100, text: "curious".into() }], &v, &font());
        let total: usize = strokes.iter().map(|s| s.len()).sum();
        assert!(strokes.len() > 3 && total > 200, "{} strokes, {total} points", strokes.len());
        let (ax, ay) = v.to_screen(100, 100);
        // Traced glyphs land at the anchor, within one line box.
        let min_x = strokes.iter().flatten().map(|p| p.0).min().unwrap();
        let min_y = strokes.iter().flatten().map(|p| p.1).min().unwrap();
        assert!((min_x - ax).abs() < 20, "note x {min_x} against anchor {ax}");
        assert!(min_y >= ay - 5 && min_y < ay + NOTE_PX as i32, "note y {min_y} against anchor {ay}");
    }

    #[test]
    fn a_drawn_line_bows_instead_of_buzzing() {
        // The whole illusion: error correlated between adjacent points, not
        // white noise. Without the drift term the marks read as clip-art.
        let v = view();
        let strokes = ops_to_strokes(&[Op::Underline { x1: 100, y1: 500, x2: 800, y2: 500 }], &v, &font());
        assert_eq!(strokes.len(), 1);
        let pts = &strokes[0];
        let base = pts[0].1 as f32;
        let dev: Vec<f32> = pts.iter().map(|&(_, y)| y as f32 - base).collect();
        let peak = dev.iter().fold(0.0f32, |m, d| m.max(d.abs()));
        assert!(peak >= 2.0, "line never left the ruler: peak deviation {peak}");
        // Correlated: consecutive deviations almost never jump.
        let jumps = dev.windows(2).filter(|w| (w[1] - w[0]).abs() > 2.0).count();
        assert!(jumps * 20 < dev.len(), "{jumps} jumps over {} points is noise, not a hand", dev.len());
    }

    #[test]
    fn degenerate_marks_produce_nothing() {
        let f = font();
        // A view so small every mark collapses to under two points.
        let v = PageView { x0: 10, y0: 10, f: 1, w: 3, h: 3 };
        for op in [
            Op::Circle { cx: 500, cy: 500, r: 1 },
            Op::Underline { x1: 500, y1: 500, x2: 500, y2: 500 },
            Op::Arrow { x1: 500, y1: 500, x2: 500, y2: 500 },
        ] {
            for s in ops_to_strokes(std::slice::from_ref(&op), &v, &f) {
                assert!(s.len() >= 2, "{op:?} emitted a {}-point stroke", s.len());
            }
        }
    }

    #[test]
    fn view_round_trips_within_two_pixels() {
        // The transform the whole modality rests on: a stroke described to the
        // model in normalized space must come back to the pixel it came from.
        for v in [
            view(),
            PageView { x0: 0, y0: 0, f: 2, w: 700, h: 936 },
            PageView { x0: 317, y0: 41, f: 3, w: 233, h: 411 },
        ] {
            let span = ((v.w * v.f) as i32, (v.h * v.f) as i32);
            for &(dx, dy) in &[(0, 0), (1, 1), (span.0 / 2, span.1 / 3), (span.0 - 1, span.1 - 1)] {
                let (x, y) = (v.x0 + dx, v.y0 + dy);
                let (u, w) = v.to_norm(x, y);
                let (rx, ry) = v.to_screen(u, w);
                assert!((rx - x).abs() <= 2 && (ry - y).abs() <= 2, "({x},{y}) -> ({u},{w}) -> ({rx},{ry})");
            }
        }
    }
}
