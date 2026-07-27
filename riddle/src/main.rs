//! riddle — the diary of Tom Riddle, for the reMarkable Paper Pro.
//!
//! Write on the page with the pen. After a pause the diary drinks your ink,
//! and an answer writes itself onto the page in a flowing hand, then fades.
//!
//! Two display backends (picked at runtime): windowed via qtfb/AppLoad when
//! QTFB_KEY is set, or full takeover via the vendor engine (quill) when
//! built with --features takeover and launched with xochitl stopped.

mod display;
mod fb;
mod help;
mod ink;
mod memory;
mod ops;
mod oracle;
mod pen;
mod power;
mod qtfb;
mod script;
mod surface;
mod touch;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ab_glyph::FontRef;

use fb::{BBox, SCREEN_H, SCREEN_W};
use oracle::Event;
use surface::{Surface, BLACK, FADED, WHITE};

const FONT_TTF: &[u8] = include_bytes!("../fonts/DancingScript.ttf");
const PNG_PATH: &str = "/tmp/riddle-page.png";

const IDLE_COMMIT: Duration = Duration::from_millis(2800);
/// Longest the event loop will sleep once the writer has left the diary alone.
///
/// The loop waits on the input descriptors rather than spinning, so an
/// untouched diary would otherwise sleep until it is touched — but SIGTERM is
/// delivered with SA_RESTART, so `poll` does not return for it. This cap is
/// what bounds how long the diary takes to notice it has been asked to close.
/// Override with `RIDDLE_IDLE_MS`.
const IDLE_WAIT_MS: u64 = 250;
/// Longest the loop will sleep while the writer is still at the page.
///
/// Deliberately the old unconditional tick. Sleeping properly between pen
/// samples lets the i.MX7 drop its clock and enter a deeper idle state, and
/// then the first strokes of a gesture are drawn by a CPU that is still
/// ramping — the busy loop this replaced was, by accident, pinning the
/// governor. So the diary only really sleeps once nobody is writing.
/// Override with `RIDDLE_ACTIVE_MS`; setting both to 2 restores the old loop.
const ACTIVE_WAIT_MS: u64 = 2;
/// How long after the last pen or window event the writer still counts as
/// present. Comfortably longer than the gap between two words.
const ACTIVE_WINDOW: Duration = Duration::from_secs(4);
/// Floor on every wait — the tick this loop used to take unconditionally.
const MIN_WAIT: Duration = Duration::from_millis(2);
/// How often to look at the oracle's channel while `Thinking`.
///
/// An `mpsc::Receiver` cannot be handed to `poll`, so the one state that waits
/// on the model rather than on the panel or the pen still has to check by hand.
/// 10ms is invisible against a network round trip and stops the loop spinning
/// for the whole of `ORACLE_PATIENCE`. Giving the oracle a wakeup pipe would
/// remove the last poll entirely.
const ORACLE_POLL: Duration = Duration::from_millis(10);
/// How long the diary waits on a silent oracle before giving up on the turn.
/// Generous: thinking models can lead with a long silence.
const ORACLE_PATIENCE: Duration = Duration::from_secs(120);
// Layout constants below were tuned for the Paper Pro's 1620x2160 page;
// fb::s / the REF_W ratio rescales them to this panel (13/15).
const REPLY_PX: f32 = 96.0 * SCREEN_W as f32 / fb::REF_W as f32;
const MARGIN_X: i32 = fb::s(120);
/// Eraser tip radius, px.
const ERASER_R: i32 = fb::s(22);
/// Half-size of the pulsing "thinking" blot at the page centre.
const BLOT: usize = fb::su(14);

const USAGE: &str = "\
riddle — the diary of Tom Riddle

usage:
  riddle                      open the diary (windowed when AppLoad sets
                              QTFB_KEY, otherwise takeover via libquill)
  riddle --oracle-test [PNG]  run one oracle turn against PNG (default
                              /tmp/riddle-page.png) and print the streamed
                              reply; verifies key + endpoint + model
  riddle --draw-test [OPS]    render annotation ops to /tmp/riddle-draw.png
                              on the host — no tablet, no key. OPS is a
                              newline-separated op list, or a file of one,
                              or omitted for a built-in sample
  riddle --version            print the version

configuration lives in oracle.env next to the binary — see
oracle.env.example for every RIDDLE_* variable.
";

type OracleRx = mpsc::Receiver<Result<Event, String>>;

enum State {
    Listening { last_pen: Option<Instant> },
    Drinking { stage: u32, next: Instant, region: BBox, rx: OracleRx },
    Thinking { rx: OracleRx, pulse: Instant, blot_on: bool, since: Instant },
    Replying { plan: WritePlan, next: Instant, rx: Option<OracleRx> },
    Lingering { until: Instant, region: BBox },
    FadingReply { stage: u32, next: Instant, region: BBox },
    /// The guide panel. `panel: None` = dismissed, waiting for pen-up so the
    /// dismissing touch doesn't leave a mark on the page.
    Help { panel: Option<help::Help>, until: Instant },
    /// A remembered page rising through the paper: date, the writer's own
    /// past ink, Tom's old reply — all in faded ink. `saved` is today's page.
    Conjuring { plan: ConjurePlan, next: Instant, saved: Vec<u8> },
    /// The conjured memory rests on the page. Pen contact (or time) dissolves
    /// it and today's page returns. `saved: None` = dismissed, waiting pen-up.
    MemoryShown { saved: Option<Vec<u8>>, until: Instant, region: BBox },
}

/// A memory being rewritten onto the page: pre-positioned strokes with their
/// original radii, drawn in faded ink.
struct ConjurePlan {
    strokes: Vec<Vec<(i32, i32, i32)>>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
}

struct WritePlan {
    strokes: Vec<Vec<(i32, i32)>>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
    /// Where the next streamed chunk's first line starts.
    next_y: i32,
    /// Ink colour. Annotation marks are drawn lighter than the writer's own
    /// hand: on a shared canvas both parties draw, and neither the writer nor
    /// the model can tell whose mark is whose from a flat black page.
    shade: u16,
}

/// One mark the diary has made, kept as structure rather than as pixels.
///
/// The pixels alone are not enough state. The eraser used to whiten a mark
/// while its op stayed live in layer 1, so the page and the model's idea of the
/// page silently diverged; and removing one mark means redrawing the page from
/// the layers, which needs the strokes kept.
struct Mark {
    op: ops::Op,
    /// The crop it was authored against — its coordinates mean nothing without.
    view: ink::PageView,
    strokes: Vec<Vec<(i32, i32)>>,
    bbox: BBox,
    /// The writer has since drawn on or beside this mark. This is the round
    /// trip: it is how the model learns which of its marks were answered.
    answered: bool,
}

/// Load `oracle.env` from beside the binary into this process's environment.
///
/// On the Paper Pro this was the launcher script's job (`set -a; . oracle.env`),
/// but the rM2 build is a qtfb/AppLoad app: AppLoad execs the binary directly,
/// so nothing would ever set RIDDLE_OPENAI_*. Doing it here also makes
/// `riddle --oracle-test` work from a bare shell on the tablet.
///
/// Variables already present in the environment win, so AppLoad or the shell
/// can still override the file. Format is the plain `KEY=value` subset the
/// example file uses; `#` comments and blank lines are skipped.
fn load_env_file() {
    let path = match std::env::var_os("RIDDLE_ENV_FILE") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let Ok(exe) = std::env::current_exe() else { return };
            let Some(dir) = exe.parent() else { return };
            dir.join("oracle.env")
        }
    };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let mut n = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        if k.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        std::env::set_var(k, v);
        n += 1;
    }
    eprintln!("riddle: loaded {n} setting(s) from {}", path.display());
}

fn main() {
    // Before anything reads the environment or spawns a thread.
    load_env_file();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // Diagnostic: run one oracle turn and print the streamed chunks.
        // Lets you verify your endpoint + key + model before ever launching
        // the diary. No display needed.
        Some("--oracle-test") => {
            let png = args.get(2).map(String::as_str).unwrap_or(PNG_PATH);
            std::process::exit(oracle_test(png));
        }
        // Phase 0 of the drawing modality, offline: expand ops to strokes and
        // stamp them into an off-screen surface. The whole renderer can be
        // iterated on here — wobble especially — with no device and no API key.
        Some("--draw-test") => {
            std::process::exit(draw_test(args.get(2).map(String::as_str)));
        }
        Some("--version" | "-V") => {
            println!("riddle {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return;
        }
        Some(flag) if flag.starts_with('-') => {
            eprintln!("riddle: unknown flag {flag}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
        _ => {}
    }
    if let Err(e) = run() {
        eprintln!("riddle: fatal: {e}");
        std::process::exit(1);
    }
}

fn oracle_test(png: &str) -> i32 {
    let store = memory::MemoryStore::open();
    let o = match oracle::Oracle::spawn(store.is_some()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("oracle spawn failed: {e}");
            return 1;
        }
    };
    let ctx = build_ctx(&store, &[], None);
    let (tx, rx) = mpsc::channel();
    let t0 = Instant::now();
    o.ask(png, &ctx, tx);
    let mut got = String::new();
    loop {
        match rx.recv() {
            Ok(Ok(Event::Ink(chunk))) => {
                if got.is_empty() {
                    eprintln!("first chunk +{}ms", t0.elapsed().as_millis());
                }
                print!("{chunk} ");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                got.push_str(&chunk);
            }
            Ok(Ok(Event::Show(id))) => {
                println!("[would conjure memory {id} — {}]", memory::spoken_date(id));
                got.push_str("(show)");
            }
            Ok(Ok(Event::Draw(marks))) => {
                // Printed as the model wrote them: --oracle-test is how you
                // find out whether it can point before involving the tablet.
                println!("[{} mark(s)]", marks.len());
                for m in &marks {
                    println!("  {}", m.to_line());
                }
                got.push_str("(draw)");
            }
            Ok(Ok(Event::Transcript(t))) => eprintln!("\n[transcript] {t}"),
            Ok(Err(e)) => {
                eprintln!("\noracle error: {e}");
                return 1;
            }
            Err(_) => break, // disconnected = reply complete
        }
    }
    println!("\n--- reply complete ({}ms, {} chars) ---", t0.elapsed().as_millis(), got.len());
    if got.trim().is_empty() { 1 } else { 0 }
}

/// One of each op, so a single run exercises every branch of the renderer.
const DRAW_SAMPLE: &str = "\
circle 300 250 90
box 550 180 300 120
arrow 420 400 640 300
underline 200 560 700 566
strike 200 660 700 668
tick 820 520
cross 880 620
note 150 760 \"look here\"
";

/// Render annotation ops to a PNG on the host — the modality with the tablet,
/// the network and the model all removed. `spec` is an op list, a path to a
/// file holding one, or None for [`DRAW_SAMPLE`].
fn draw_test(spec: Option<&str>) -> i32 {
    let font = match FontRef::try_from_slice(FONT_TTF) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("riddle: font: {e}");
            return 1;
        }
    };
    let text = match spec {
        None => DRAW_SAMPLE.to_string(),
        // A path if it resolves, otherwise treat the argument itself as ops.
        Some(s) => std::fs::read_to_string(s).unwrap_or_else(|_| s.to_string()),
    };
    let (ops, dropped) = ops::parse_ops(&text);
    if dropped > 0 {
        eprintln!("riddle: dropped {dropped} malformed op(s)");
    }
    if ops.is_empty() {
        eprintln!("riddle: no ops to draw");
        return 1;
    }

    // The whole page at 1:1 — --draw-test asks what the marks look like, not
    // where a particular crop would have put them.
    let view = ink::PageView { x0: 0, y0: 0, f: 1, w: SCREEN_W, h: SCREEN_H };
    let strokes = ops::ops_to_strokes(&ops, &view, &font, None);

    let mut buf = vec![0xFFu8; SCREEN_W * SCREEN_H * 4];
    let ptr = buf.as_mut_ptr();
    let mut surf = Surface::new(
        ptr,
        SCREEN_W * SCREEN_H * 4,
        SCREEN_W,
        SCREEN_H,
        SCREEN_W * 4,
        surface::PixFmt::Rgb32,
    );
    paint_buttons(&mut surf);
    // Exactly what the animator does, with the 14ms pacing taken out.
    let mut points = 0usize;
    for s in &strokes {
        for (i, &(x, y)) in s.iter().enumerate() {
            if i > 0 {
                let (px, py) = s[i - 1];
                surf.brush_line(px, py, x, y, 2, BLACK);
            } else {
                surf.stamp(x, y, 2, BLACK);
            }
            points += 1;
        }
    }

    let mut gray = vec![0u8; SCREEN_W * SCREEN_H];
    for y in 0..SCREEN_H {
        for x in 0..SCREEN_W {
            gray[y * SCREEN_W + x] = surf.luma(x as i32, y as i32);
        }
    }
    let out = std::env::var("RIDDLE_DRAW_OUT").unwrap_or_else(|_| "/tmp/riddle-draw.png".into());
    let file = match std::fs::File::create(&out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("riddle: {out}: {e}");
            return 1;
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), SCREEN_W as u32, SCREEN_H as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let wrote = enc
        .write_header()
        .and_then(|mut w| w.write_image_data(&gray));
    if let Err(e) = wrote {
        eprintln!("riddle: encode: {e}");
        return 1;
    }
    // 26 points per 14ms tick is the animator's budget; this is what the marks
    // will actually cost on the page.
    let ms = points as f64 / 26.0 * 14.0;
    println!(
        "{} op(s) -> {} stroke(s), {points} point(s) (~{ms:.0}ms to draw)\nwrote {out}",
        ops.len(),
        strokes.len()
    );
    0
}

/// What the diary sends alongside the page: its memory of recent turns and
/// the catalog the oracle picks conjured pages from. Empty when memory is off.
fn build_ctx(
    store: &Option<memory::MemoryStore>,
    page_marks: &[Mark],
    now: Option<&ink::PageView>,
) -> oracle::TurnContext {
    // Re-express every prior mark in THIS turn's crop before describing it,
    // or the numbers contradict the image the model is looking at. A mark the
    // writer has drawn on is flagged: that is the whole round trip, and it is
    // the difference between "you marked this" and "you marked this and they
    // answered you".
    let prior_ops: Vec<String> = match now {
        Some(now) => page_marks
            .iter()
            .map(|m| {
                let line = ops::reproject(&m.op, &m.view, now).to_line();
                if m.answered {
                    format!("{line}   <- the writer has since written on this one")
                } else {
                    line
                }
            })
            .collect(),
        None => Vec::new(),
    };
    let Some(s) = store else {
        return oracle::TurnContext { prior_ops, ..Default::default() };
    };
    let turns: usize = std::env::var("RIDDLE_MEMORY_TURNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let (catalog_lines, catalog_ids) = s.catalog(40);
    oracle::TurnContext { history: s.recent_dialogue(turns), catalog_lines, catalog_ids, prior_ops }
}

fn run() -> std::io::Result<()> {
    let font = FontRef::try_from_slice(FONT_TTF).map_err(std::io::Error::other)?;

    let (disp, mut surf) = display::Display::open()?;
    let takeover = matches!(disp, display::Display::Quill);
    eprintln!(
        "riddle: display {} ({}x{} stride {})",
        if takeover { "quill/takeover" } else { "qtfb" },
        surf.w,
        surf.h,
        surf.stride
    );

    let mut pen_dev = match pen::PenDevice::open() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("riddle: raw pen unavailable ({e}), falling back to qtfb pen events");
            None
        }
    };
    // Takeover mode: touch is ours too; 5-finger tap = quit.
    let mut touch_dev = if takeover { touch::TouchDevice::open().ok() } else { None };
    // Takeover mode: the power button is ours too (sleep page + suspend).
    let mut power_dev = if takeover {
        power::PowerButton::open().map_err(|e| eprintln!("riddle: no power button ({e})")).ok()
    } else {
        None
    };
    // Ignore power presses briefly after a wake: the waking press itself (and
    // key bounce) arrives on our grabbed fd and must not re-suspend.
    let mut power_grace = Instant::now();

    // Everything the loop can be woken by. Built once: the set never changes,
    // and the pen is by far the most important member — waiting on it instead
    // of re-reading it every 2ms is what takes the idle diary off the CPU.
    let mut wait_set: Vec<libc::pollfd> = [
        pen_dev.as_ref().map(|d| d.raw_fd()),
        disp.raw_fd(),
        touch_dev.as_ref().map(|d| d.raw_fd()),
        power_dev.as_ref().map(|d| d.raw_fd()),
    ]
    .into_iter()
    .flatten()
    .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
    .collect();
    let ms_env = |k: &str, default: u64| -> Duration {
        let v = std::env::var(k)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| (1..=2000).contains(v))
            .unwrap_or(default);
        Duration::from_millis(v)
    };
    let active_wait = ms_env("RIDDLE_ACTIVE_MS", ACTIVE_WAIT_MS);
    let idle_wait = ms_env("RIDDLE_IDLE_MS", IDLE_WAIT_MS);
    // Any pen or window event refreshes this; while it is recent the loop keeps
    // ticking rather than sleeping, so the CPU stays awake under the writer's
    // hand. See ACTIVE_WAIT_MS.
    let mut last_input = Instant::now();
    eprintln!(
        "riddle: waiting on {} descriptor(s), tick {}ms writing / {}ms idle",
        wait_set.len(),
        active_wait.as_millis(),
        idle_wait.as_millis()
    );

    let sigterm = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&sigterm))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&sigterm))?;

    // Blank page.
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    disp.update_all(surf.w, surf.h);

    // The diary's memory (None = RIDDLE_MEMORY=off or the dir is unusable).
    let mut store = memory::MemoryStore::open();
    if let Some(ref s) = store {
        eprintln!("riddle: memory holds {} pages", s.entries.len());
    }

    // Warm the oracle now: pi loads Node + extensions + codex auth ONCE here,
    // while you're still picking up the pen, so replies pay only model latency.
    let oracle = match oracle::Oracle::spawn(store.is_some()) {
        Ok(o) => {
            eprintln!("riddle: oracle ready");
            Some(o)
        }
        Err(e) => {
            eprintln!("riddle: oracle spawn failed: {e}");
            None
        }
    };

    let mut user_ink = ink::Ink::new();
    let mut state = State::Listening { last_pen: None };
    let mut pen_down = false;
    // The turn being remembered: strokes captured at commit, transcript and
    // reply accumulated as they stream, stored when the turn completes.
    let mut turn_id: u64 = 0;
    let mut turn_strokes: memory::Strokes = Vec::new();
    let mut turn_reply = String::new();
    let mut turn_transcript: Option<String> = None;
    let mut turn_failed = false;
    // Annotation mode: the diary marks the page instead of writing on it.
    // Everything below the prompt is shared; what changes is that the writer's
    // ink is NOT drunk (the marks have to point at something) and the marks are
    // not faded (the writer answers them on the next turn).
    let annotating = oracle.as_ref().is_some_and(|o| o.annotating());
    // The crop the model last saw. Every coordinate it returns is in this
    // space; without it a reply cannot be mapped back onto the page.
    let mut page_view: Option<ink::PageView> = None;
    // Layer 1: the marks the diary made this turn, kept as ops rather than
    // pixels. Handed back to the model next turn so it can refer to its own
    // previous annotations instead of re-reading them out of the image.
    // Each mark with the crop it was authored against: the crop moves as the
    // page fills, so the raw numbers only mean something next to their view.
    let mut page_marks: Vec<Mark> = Vec::new();
    // Raw stylus contact, tracked in every state (the guide dismisses on it).
    // `stylus_on` is the level; `stylus_tapped` latches any contact seen this
    // loop iteration, so a tap that starts AND ends within one drain still
    // registers.
    let mut stylus_on = false;
    let mut stylus_tapped = false;
    let mut ink_dirty = BBox::empty();
    let mut last_flush = Instant::now();
    // Takeover swaps are cheap and synchronous; qtfb needs coalescing.
    //
    // rM2: 35ms (~28 updates/s) oversubscribes the panel badly. Each qtfb
    // update goes shm -> Qt scene graph -> xochitl's compositor -> SWTCON, and
    // that path sustains far fewer than 28/s, so requests queue and ink lags
    // further behind the further you write. Raising this REDUCES latency.
    // Tunable at runtime so the sweet spot can be found without a rebuild.
    let flush_every = {
        let default_ms = if takeover { 8 } else { 120 };
        let ms = std::env::var("RIDDLE_FLUSH_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v >= 1 && *v <= 2000)
            .unwrap_or(default_ms);
        eprintln!("riddle: ink flush every {}ms (RIDDLE_FLUSH_MS to change)", ms);
        Duration::from_millis(ms)
    };

    if annotating {
        draw_clear_button(&mut surf, &disp);
    }
    eprintln!("riddle: the diary is open");

    loop {
        if sigterm.load(Ordering::Relaxed) {
            break;
        }
        if let Some(ref mut t) = touch_dev {
            if t.drain_check_quit() {
                eprintln!("riddle: 5-finger quit");
                break;
            }
        }

        // ---- power button: sleep page, suspend, restore on wake ----
        if let Some(ref mut p) = power_dev {
            let pressed = p.drain_pressed();
            if pressed && Instant::now() >= power_grace {
                eprintln!("riddle: sleeping (power button)");
                let saved = help::show_sleep(&mut surf, &font);
                disp.full_refresh(surf.w, surf.h);
                // Let the flashing refresh finish before the panel loses power.
                std::thread::sleep(Duration::from_millis(800));
                // Suspend, and confirm via the kernel's success counter. The
                // EPD regulator refuses to sleep while its post-update vpdd
                // timer (≤30s) runs — the whole suspend aborts with "Some
                // devices failed to suspend" — so retry until it sticks.
                let count0 = power::suspend_count();
                let mut attempts = 0;
                'sleeping: loop {
                    if p.grabbed {
                        let _ = std::process::Command::new("systemctl").arg("suspend").status();
                    }
                    attempts += 1;
                    let t0 = Instant::now();
                    while t0.elapsed() < Duration::from_secs(6) {
                        std::thread::sleep(Duration::from_millis(400));
                        if power::suspend_count() > count0 {
                            break 'sleeping;
                        }
                    }
                    if attempts >= 8 {
                        eprintln!("riddle: suspend never happened ({attempts} tries); waking the page");
                        break;
                    }
                    eprintln!("riddle: suspend aborted (EPD discharge timer), retrying");
                }
                eprintln!("riddle: waking");
                help::restore_sleep(&mut surf, &saved);
                disp.full_refresh(surf.w, surf.h);
                power::wifi_heal();
                // Discard input that queued while asleep — stale pen events
                // would otherwise replay as phantom ink on the restored page.
                if let Some(ref mut pd) = pen_dev {
                    let _ = pd.drain();
                }
                if let Some(ref mut td) = touch_dev {
                    let _ = td.drain_check_quit();
                }
                p.drain_pressed();
                power_grace = Instant::now() + Duration::from_secs(3);
            }
        }

        // ---- raw pen (preferred path) ----
        if let Some(ref mut pdev) = pen_dev {
            let samples = pdev.drain();
            // Hover counts: the pen coming into digitizer range is the writer
            // arriving, and that is exactly when the CPU needs to be awake.
            if !samples.is_empty() {
                last_input = Instant::now();
            }
            for s in samples {
                let writing = s.touching && s.pressure > 40;
                stylus_on = writing;
                stylus_tapped |= writing;
                if !writing {
                    if pen_down {
                        pen_down = false;
                        user_ink.pen_up();
                        if let State::Listening { ref mut last_pen } = state {
                            *last_pen = Some(Instant::now());
                        }
                    }
                    continue;
                }
                match state {
                    State::Listening { ref mut last_pen } => {
                        // The clear button is checked before any ink is laid
                        // down, so tapping it never leaves a stroke behind.
                        if annotating && hit(clear_button(), s.x, s.y) {
                            clear_page(&mut surf, &disp);
                            user_ink.clear();
                            page_marks.clear();
                            page_view = None;
                            *last_pen = None;
                            eprintln!("riddle: page cleared");
                            continue;
                        }
                        if annotating && hit(undo_button(), s.x, s.y) {
                            match page_marks.pop() {
                                Some(m) => {
                                    eprintln!("riddle: undid {}", m.op.to_line());
                                    recompose(&mut surf, &disp, &user_ink, &page_marks, annotating);
                                }
                                None => eprintln!("riddle: nothing to undo"),
                            }
                            *last_pen = None;
                            continue;
                        }
                        pen_down = true;
                        let d = match s.tool {
                            pen::Tool::Pen => {
                                let r = 2 + s.pressure * 3 / pen::MAX_PRESSURE;
                                user_ink.pen_point(&mut surf, s.x, s.y, r)
                            }
                            pen::Tool::Eraser => {
                                // Rubbing out a mark must take the op with it,
                                // or the diary keeps telling the model about a
                                // mark the writer has already removed.
                                if erase_marks_near(&mut page_marks, s.x, s.y, ERASER_R) {
                                    recompose(&mut surf, &disp, &user_ink, &page_marks, annotating);
                                }
                                user_ink.erase_point(&mut surf, s.x, s.y, ERASER_R)
                            }
                        };
                        if !d.is_empty() {
                            ink_dirty.add(d.x0, d.y0, 0);
                            ink_dirty.add(d.x1, d.y1, 0);
                        }
                        *last_pen = Some(Instant::now());
                    }
                    State::Lingering { region, .. } => {
                        // Touching the page while marks rest on it means the
                        // writer is answering them, so in annotation mode this
                        // is the start of the next turn, not a dismissal.
                        state = if annotating {
                            State::Listening { last_pen: Some(Instant::now()) }
                        } else {
                            State::FadingReply { stage: 0, next: Instant::now(), region }
                        };
                    }
                    _ => {}
                }
            }
        }

        // ---- window-system events (qtfb close detection + pen fallback) ----
        let events = match disp.pump() {
            Ok(v) => v,
            Err(_) => break, // qtfb window closed
        };
        if !events.is_empty() {
            last_input = Instant::now();
        }
        for ev in events {
            if pen_dev.is_some() {
                continue;
            }
            match ev.input_type {
                qtfb::INPUT_PEN_PRESS | qtfb::INPUT_PEN_UPDATE => {
                    stylus_on = true;
                    stylus_tapped = true;
                    if let State::Listening { ref mut last_pen } = state {
                        pen_down = true;
                        let r = 2 + ev.d.clamp(0, 100) / 45;
                        let d = user_ink.pen_point(&mut surf, ev.x, ev.y, r);
                        if !d.is_empty() {
                            ink_dirty.add(d.x0, d.y0, 0);
                            ink_dirty.add(d.x1, d.y1, 0);
                        }
                        *last_pen = Some(Instant::now());
                    } else if let State::Lingering { region, .. } = state {
                        state = if annotating {
                            State::Listening { last_pen: Some(Instant::now()) }
                        } else {
                            State::FadingReply { stage: 0, next: Instant::now(), region }
                        };
                    }
                }
                qtfb::INPUT_PEN_RELEASE => {
                    stylus_on = false;
                    if pen_down {
                        pen_down = false;
                        user_ink.pen_up();
                        if let State::Listening { ref mut last_pen } = state {
                            *last_pen = Some(Instant::now());
                        }
                    }
                }
                _ => {}
            }
        }

        // ---- coalesced ink flush ----
        if !ink_dirty.is_empty() && last_flush.elapsed() >= flush_every {
            let (x, y, w, h) = ink_dirty.rect();
            disp.update(x, y, w, h, true);
            ink_dirty = BBox::empty();
            last_flush = Instant::now();
        }

        // ---- state machine ----
        state = match state {
            State::Listening { last_pen } => match last_pen {
                Some(t) if !pen_down && t.elapsed() >= IDLE_COMMIT && !user_ink.is_empty() => {
                    if region_all_white(&surf, user_ink.bbox) {
                        // Everything was erased before the pause: nothing to
                        // commit (and no phantom "?" from erased strokes).
                        user_ink.clear();
                        State::Listening { last_pen: None }
                    } else if help::looks_like_question_mark(user_ink.stroke_list()) {
                        // Absorb the "?" and open the guide instead of asking.
                        let (qx, qy, qw, qh) = user_ink.bbox.rect();
                        surf.fill_rect(qx as usize, qy as usize, qw as usize, qh as usize, WHITE);
                        disp.update(qx, qy, qw, qh, false);
                        user_ink.clear();
                        let panel = help::show(&mut surf, &font, takeover);
                        let (px, py, pw, ph) = panel.region.rect();
                        disp.update(px, py, pw, ph, false);
                        eprintln!("riddle: guide shown");
                        State::Help { panel: Some(panel), until: Instant::now() + Duration::from_secs(45) }
                    } else if oracle.is_none() {
                        // No spirit at all: don't eat ink that nothing will
                        // answer — leave the writing and put the reason below.
                        let y = (user_ink.bbox.y1 + fb::s(90)).min(SCREEN_H as i32 - fb::s(400));
                        let plan = plan_reply(&font, &oracle_excuse("no oracle"), Some(y));
                        State::Replying { plan, next: Instant::now(), rx: None }
                    } else {
                        // Annotation needs room to annotate INTO: a bbox-tight
                        // crop leaves the model nowhere to put a margin note,
                        // because it can only mark inside the image it was
                        // sent. The reply path keeps the tight crop (0.0), so
                        // its token cost is unchanged.
                        let pad = if annotating { 0.15 } else { 0.0 };
                        match user_ink.to_png(&surf, PNG_PATH, pad, marks_bbox(&page_marks)) {
                            Ok(v) => page_view = Some(v),
                            Err(e) => {
                                eprintln!("riddle: rasterize failed: {e}");
                                page_view = None;
                            }
                        }
                        // Remember this page: strokes now (they're cleared
                        // after the drink), transcript/reply as they stream.
                        turn_id = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        turn_strokes = user_ink.stroke_list().to_vec();
                        turn_reply.clear();
                        turn_transcript = None;
                        turn_failed = false;
                        // Ask NOW: the model streams while the diary drinks the
                        // ink, hiding most of the reply latency in the animation.
                        note_answered(&mut page_marks, &user_ink);
                        let (tx, rx) = mpsc::channel();
                        if let Some(ref o) = oracle {
                            o.ask(PNG_PATH, &build_ctx(&store, &page_marks, page_view.as_ref()), tx);
                        }
                        // Both backends read the page before ask() returns; the
                        // writer's words don't need to sit on disk afterwards.
                        if std::env::var_os("RIDDLE_KEEP_PAGE").is_none() {
                            let _ = std::fs::remove_file(PNG_PATH);
                        }
                        let region = user_ink.bbox;
                        if annotating {
                            // Don't drink the ink: the marks are about to point
                            // at it, and the writer answers them on the page
                            // they are drawn on. The page is a shared canvas
                            // from here, so it also never gets cleared for us.
                            State::Thinking {
                                rx,
                                pulse: Instant::now(),
                                blot_on: false,
                                since: Instant::now(),
                            }
                        } else {
                            State::Drinking { stage: 0, next: Instant::now(), region, rx }
                        }
                    }
                }
                _ => State::Listening { last_pen },
            },

            State::Drinking { stage, next, region, rx } => {
                const STAGES: u32 = 14;
                if Instant::now() >= next {
                    ink::dissolve_pass(&mut surf, region, stage, STAGES);
                    let (x, y, w, h) = region.rect();
                    disp.update(x, y, w, h, true);
                    if stage + 1 >= STAGES {
                        user_ink.clear();
                        State::Thinking { rx, pulse: Instant::now(), blot_on: false, since: Instant::now() }
                    } else {
                        State::Drinking { stage: stage + 1, next: Instant::now() + Duration::from_millis(70), region, rx }
                    }
                } else {
                    State::Drinking { stage, next, region, rx }
                }
            }

            State::Thinking { rx, pulse, blot_on, since } => match rx.try_recv() {
                Ok(result) => {
                    surf.fill_rect(SCREEN_W / 2 - BLOT, SCREEN_H / 2 - BLOT, BLOT * 2, BLOT * 2, WHITE);
                    disp.update(SCREEN_W as i32 / 2 - BLOT as i32, SCREEN_H as i32 / 2 - BLOT as i32, BLOT as i32 * 2, BLOT as i32 * 2, true);
                    // First streamed event: start writing now; keep the
                    // receiver so the rest of the reply can append itself.
                    match result {
                        Ok(Event::Show(id)) => {
                            // An incantation: the rest of this turn is the
                            // conjured memory, not a reply. (rx drops here.)
                            match conjure(&font, &store, id, &mut surf, &disp) {
                                Some(st) => st,
                                None => {
                                    eprintln!("riddle: memory {id} is missing");
                                    let plan = plan_reply(&font, &oracle_excuse("lost page"), None);
                                    turn_failed = true;
                                    State::Replying { plan, next: Instant::now(), rx: None }
                                }
                            }
                        }
                        Ok(Event::Draw(ops)) => {
                            let (plan, made) = plan_each(&font, &ops, page_view.as_ref(), &surf);
                            page_marks.extend(made);
                            State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                        }
                        // Prose has nowhere to go in annotation mode.
                        // plan_reply always starts a fresh reply at the same
                        // fixed y, which the fade used to clear — and the fade
                        // is off here, so every turn's prose piled onto the
                        // last one in the same spot. Words the model wants read
                        // must be a note op, which lands where it chose.
                        Ok(Event::Ink(text)) if annotating => {
                            eprintln!("riddle: prose ignored in annotate mode: {text}");
                            turn_reply.push_str(&text);
                            State::Thinking { rx, pulse, blot_on, since }
                        }
                        Ok(Event::Ink(text)) => {
                            turn_reply.push_str(&text);
                            let plan = plan_reply(&font, &text, None);
                            State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                        }
                        Ok(Event::Transcript(t)) => {
                            // Transcript with no prose (model skipped the
                            // reply): remember the words, keep waiting.
                            turn_transcript = Some(t);
                            State::Thinking { rx, pulse, blot_on, since }
                        }
                        Err(e) => {
                            eprintln!("riddle: oracle failed: {e}");
                            turn_failed = true;
                            let plan = plan_reply(&font, &oracle_excuse(&e), None);
                            State::Replying { plan, next: Instant::now(), rx: None }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if since.elapsed() >= ORACLE_PATIENCE {
                        // The oracle never answered (stalled stream, dead pi):
                        // stop pulsing and say so instead of thinking forever.
                        eprintln!("riddle: oracle timed out after {}s", ORACLE_PATIENCE.as_secs());
                        surf.fill_rect(SCREEN_W / 2 - BLOT, SCREEN_H / 2 - BLOT, BLOT * 2, BLOT * 2, WHITE);
                        disp.update(SCREEN_W as i32 / 2 - BLOT as i32, SCREEN_H as i32 / 2 - BLOT as i32, BLOT as i32 * 2, BLOT as i32 * 2, true);
                        let plan = plan_reply(&font, &oracle_excuse("timed out"), None);
                        State::Replying { plan, next: Instant::now(), rx: None }
                    } else if pulse.elapsed() >= Duration::from_millis(600) {
                        let (cx, cy) = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
                        if blot_on {
                            surf.fill_rect(cx as usize - BLOT, cy as usize - BLOT, BLOT * 2, BLOT * 2, WHITE);
                        } else {
                            surf.stamp(cx, cy, fb::s(9), BLACK);
                        }
                        disp.update(cx - BLOT as i32, cy - BLOT as i32, BLOT as i32 * 2, BLOT as i32 * 2, true);
                        State::Thinking { rx, pulse: Instant::now(), blot_on: !blot_on, since }
                    } else {
                        State::Thinking { rx, pulse, blot_on, since }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The turn ended without a single event — in annotation
                    // mode that is the model correctly deciding the page needs
                    // no mark. Wipe the pulsing blot on the way out, or the
                    // "nothing to add" answer leaves a dot at the page centre.
                    surf.fill_rect(SCREEN_W / 2 - BLOT, SCREEN_H / 2 - BLOT, BLOT * 2, BLOT * 2, WHITE);
                    disp.update(
                        SCREEN_W as i32 / 2 - BLOT as i32,
                        SCREEN_H as i32 / 2 - BLOT as i32,
                        BLOT as i32 * 2,
                        BLOT as i32 * 2,
                        true,
                    );
                    State::Listening { last_pen: None }
                }
            },

            State::Replying { mut plan, next, mut rx } => {
                // More of the reply may still be streaming in: append each
                // new chunk below what is already planned, mid-animation.
                if let Some(ref r) = rx {
                    let drop_rx = match r.try_recv() {
                        // Marks can arrive after prose — "I see what you mean,
                        // ⟦draw …⟧" is the natural shape — so they splice into
                        // a running animation the same way more text does.
                        Ok(Ok(Event::Draw(more_ops))) => {
                            let (add, made) = plan_each(&font, &more_ops, page_view.as_ref(), &surf);
                            if !add.strokes.is_empty() {
                                plan.region.add(add.region.x0, add.region.y0, 0);
                                plan.region.add(add.region.x1, add.region.y1, 0);
                                plan.strokes.extend(add.strokes);
                                page_marks.extend(made);
                            }
                            false
                        }
                        Ok(Ok(Event::Ink(more))) if annotating => {
                            eprintln!("riddle: prose ignored in annotate mode: {more}");
                            turn_reply.push_str(&more);
                            false
                        }
                        Ok(Ok(Event::Ink(more))) => {
                            if plan.next_y > SCREEN_H as i32 - fb::s(200) {
                                // The page is full: let the rest go unwritten
                                // rather than inking below the visible page.
                                eprintln!("riddle: reply reached the page bottom; trailing text dropped");
                                true
                            } else {
                                turn_reply.push_str(" ");
                                turn_reply.push_str(&more);
                                append_reply(&font, &mut plan, &more);
                                false
                            }
                        }
                        Ok(Ok(Event::Transcript(t))) => {
                            turn_transcript = Some(t);
                            false // the disconnect is still coming
                        }
                        Ok(Ok(Event::Show(_))) => {
                            eprintln!("riddle: conjuring directive mid-reply ignored");
                            false
                        }
                        Ok(Err(e)) => {
                            eprintln!("riddle: oracle failed mid-reply: {e}");
                            turn_failed = true;
                            true
                        }
                        Err(mpsc::TryRecvError::Disconnected) => true,
                        Err(mpsc::TryRecvError::Empty) => false,
                    };
                    if drop_rx {
                        rx = None;
                    }
                }
                if Instant::now() >= next {
                    let mut dirty = BBox::empty();
                    let mut budget = 26;
                    while budget > 0 && plan.stroke_i < plan.strokes.len() {
                        let stroke = &plan.strokes[plan.stroke_i];
                        if plan.point_i >= stroke.len() {
                            plan.stroke_i += 1;
                            plan.point_i = 0;
                            continue;
                        }
                        let (x, y) = stroke[plan.point_i];
                        if plan.point_i > 0 {
                            let (px, py) = stroke[plan.point_i - 1];
                            surf.brush_line(px, py, x, y, 2, plan.shade);
                        } else {
                            surf.stamp(x, y, 2, plan.shade);
                        }
                        dirty.add(x, y, 4);
                        plan.point_i += 1;
                        budget -= 1;
                    }
                    if !dirty.is_empty() {
                        let (x, y, w, h) = dirty.rect();
                        disp.update(x, y, w, h, true);
                    }
                    if plan.stroke_i >= plan.strokes.len() && rx.is_none() {
                        // The turn is complete: the diary remembers it.
                        if !turn_failed && !turn_reply.is_empty() {
                            if let Some(ref mut s) = store {
                                s.append(
                                    turn_id,
                                    turn_transcript.as_deref().unwrap_or(""),
                                    turn_reply.trim(),
                                    &turn_strokes,
                                );
                                // Layer 1 keeps the crop each mark was made
                                // against; without it the coordinates cannot be
                                // reprojected when the page is read back.
                                let lines: Vec<String> = page_marks
                                    .iter()
                                    .map(|m| {
                                        format!(
                                            "{} {} {} {} {}\t{}",
                                            m.view.x0, m.view.y0, m.view.f, m.view.w, m.view.h,
                                            m.op.to_line()
                                        )
                                    })
                                    .collect();
                                s.save_marks(turn_id, &lines);
                            }
                        }
                        turn_strokes = Vec::new();
                        let chars: usize = plan.strokes.iter().map(|s| s.len()).sum();
                        let linger = Duration::from_millis(4000 + (chars as u64) * 2);
                        let region = plan.region;
                        State::Lingering { until: Instant::now() + linger.min(Duration::from_secs(20)), region }
                    } else {
                        State::Replying { plan, next: Instant::now() + Duration::from_millis(14), rx }
                    }
                } else {
                    State::Replying { plan, next, rx }
                }
            }

            State::Lingering { until, region } => {
                if Instant::now() >= until {
                    if annotating {
                        // The marks stay. Fading them would erase the thing the
                        // writer is meant to answer, and the shared canvas with
                        // it — layer 1 has to survive to the next turn.
                        State::Listening { last_pen: None }
                    } else {
                        State::FadingReply { stage: 0, next: Instant::now(), region }
                    }
                } else {
                    State::Lingering { until, region }
                }
            }

            State::Help { panel, until } => match panel {
                Some(p) => {
                    if stylus_tapped || Instant::now() >= until {
                        let region = p.dismiss(&mut surf);
                        let (x, y, w, h) = region.rect();
                        disp.update(x, y, w, h, false);
                        eprintln!("riddle: guide dismissed");
                        State::Help { panel: None, until }
                    } else {
                        State::Help { panel: Some(p), until }
                    }
                }
                // Dismissed: swallow the closing touch, listen again on pen-up.
                None if stylus_on => State::Help { panel: None, until },
                None => State::Listening { last_pen: None },
            },

            State::Conjuring { mut plan, next, saved } => {
                if stylus_tapped {
                    // The writer interrupts: today's page returns at once.
                    surf.paste_rect(0, 0, SCREEN_W, SCREEN_H, &saved);
                    disp.full_refresh(surf.w, surf.h);
                    State::MemoryShown { saved: None, until: Instant::now(), region: plan.region }
                } else if Instant::now() >= next {
                    // The memory pours back faster than Tom writes: it is
                    // remembered, not composed.
                    let mut dirty = BBox::empty();
                    let mut budget = 48;
                    while budget > 0 && plan.stroke_i < plan.strokes.len() {
                        let stroke = &plan.strokes[plan.stroke_i];
                        if plan.point_i >= stroke.len() {
                            plan.stroke_i += 1;
                            plan.point_i = 0;
                            continue;
                        }
                        let (x, y, r) = stroke[plan.point_i];
                        if plan.point_i > 0 {
                            let (px, py, pr) = stroke[plan.point_i - 1];
                            surf.brush_line(px, py, x, y, r.min(pr + 1), FADED);
                        } else {
                            surf.stamp(x, y, r, FADED);
                        }
                        dirty.add(x, y, r + 2);
                        plan.point_i += 1;
                        budget -= 1;
                    }
                    if !dirty.is_empty() {
                        let (x, y, w, h) = dirty.rect();
                        disp.update(x, y, w, h, true);
                    }
                    if plan.stroke_i >= plan.strokes.len() {
                        let region = plan.region;
                        State::MemoryShown {
                            saved: Some(saved),
                            until: Instant::now() + Duration::from_secs(120),
                            region,
                        }
                    } else {
                        State::Conjuring { plan, next: Instant::now() + Duration::from_millis(10), saved }
                    }
                } else {
                    State::Conjuring { plan, next, saved }
                }
            }

            State::MemoryShown { saved, until, region } => match saved {
                Some(s) => {
                    if stylus_tapped || Instant::now() >= until {
                        // The paper swallows its memory; today's page returns.
                        surf.paste_rect(0, 0, SCREEN_W, SCREEN_H, &s);
                        disp.full_refresh(surf.w, surf.h);
                        eprintln!("riddle: memory dismissed");
                        State::MemoryShown { saved: None, until, region }
                    } else {
                        State::MemoryShown { saved: Some(s), until, region }
                    }
                }
                // Dismissed: swallow the closing touch, listen again on pen-up.
                None if stylus_on => State::MemoryShown { saved: None, until, region },
                None => State::Listening { last_pen: None },
            },

            State::FadingReply { stage, next, region } => {
                const STAGES: u32 = 10;
                if Instant::now() >= next {
                    ink::dissolve_pass(&mut surf, region, stage, STAGES);
                    let (x, y, w, h) = region.rect();
                    disp.update(x, y, w, h, true);
                    if stage + 1 >= STAGES {
                        disp.full_refresh(surf.w, surf.h);
                        State::Listening { last_pen: None }
                    } else {
                        State::FadingReply { stage: stage + 1, next: Instant::now() + Duration::from_millis(80), region }
                    }
                } else {
                    State::FadingReply { stage, next, region }
                }
            }
        };

        stylus_tapped = false;

        // Sleep until the earliest thing that needs us: the state machine's own
        // next tick, a pending ink flush, the oracle's channel, or a descriptor
        // becoming readable. Whichever comes first.
        let now = Instant::now();
        // Only let the CPU go properly idle once the writer has actually
        // stopped; under the hand, keep the old tick.
        let cap = if last_input.elapsed() < ACTIVE_WINDOW { active_wait } else { idle_wait };
        let mut wake = now + cap;
        if let Some(d) = state_deadline(&state, user_ink.is_empty(), pen_down) {
            wake = wake.min(d);
        }
        if !ink_dirty.is_empty() {
            wake = wake.min(last_flush + flush_every);
        }
        if matches!(state, State::Thinking { .. }) {
            wake = wake.min(now + ORACLE_POLL);
        }
        wait_for_input(&mut wait_set, wake.saturating_duration_since(now));
    }

    eprintln!("riddle: the diary closes");
    disp.terminate();
    Ok(())
}

/// When the state machine next has work to do regardless of any input.
///
/// Every state is either animating on a timer or waiting for a moment to pass,
/// so this is what lets the loop sleep instead of spin: without it the diary
/// woke 500 times a second to discover that nothing had happened.
///
/// `None` means "only input can change anything here" — and it has to be None
/// whenever the deadline would be one the state machine declines to act on,
/// because a deadline already in the past that never gets consumed is a
/// zero-length sleep, i.e. a hard spin. `Listening` is exactly that case: it
/// commits on the idle timer only if there is ink and the pen is up, and if the
/// writer erases everything mid-page its `last_pen` stays set forever.
fn state_deadline(state: &State, ink_empty: bool, pen_down: bool) -> Option<Instant> {
    match state {
        // Deliberately mirrors the commit guard in `run`'s state machine.
        State::Listening { last_pen } => last_pen
            .filter(|_| !ink_empty && !pen_down)
            .map(|t| t + IDLE_COMMIT),
        State::Drinking { next, .. } => Some(*next),
        // The pulsing blot; the channel is handled by ORACLE_POLL.
        State::Thinking { pulse, .. } => Some(*pulse + Duration::from_millis(600)),
        State::Replying { next, .. } => Some(*next),
        State::Lingering { until, .. } => Some(*until),
        State::FadingReply { next, .. } => Some(*next),
        State::Help { until, .. } => Some(*until),
        State::Conjuring { next, .. } => Some(*next),
        State::MemoryShown { until, .. } => Some(*until),
    }
}

/// Sleep until one of `fds` has something to read or `timeout` elapses.
///
/// `fds` is built once — the descriptor set never changes over a run — and
/// `revents` is cleared on the way in so a stale hangup cannot make this return
/// instantly forever.
fn wait_for_input(fds: &mut [libc::pollfd], timeout: Duration) {
    // Never shorter than the sleep this loop used to take unconditionally. A
    // deadline that has already passed computes to zero, and a zero-timeout
    // poll in a loop is a spin — this is the backstop that makes the worst case
    // no worse than the old 2ms tick, whatever state gets added later.
    let timeout = timeout.max(MIN_WAIT);
    if fds.is_empty() {
        // No descriptors at all (takeover with no input devices): there is
        // nothing to wait on, so fall back to sleeping out the deadline.
        std::thread::sleep(timeout);
        return;
    }
    for p in fds.iter_mut() {
        p.revents = 0;
    }
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // EINTR just means a signal arrived, which is exactly a reason to go round
    // the loop again — there is no retry to do here.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
}

/// True if the region no longer holds any dark pixels (fully erased).
fn region_all_white(surf: &Surface, region: BBox) -> bool {
    if region.is_empty() {
        return true;
    }
    for y in region.y0..=region.y1 {
        for x in region.x0..=region.x1 {
            if surf.luma(x, y) < 200 {
                return false;
            }
        }
    }
    true
}

/// What Tom writes when the spirit cannot answer: short, in a diary's voice,
/// but specific enough to act on. The raw error still goes to stderr.
fn oracle_excuse(e: &str) -> String {
    if e.contains("no oracle") {
        "The diary lies dormant: it found no oracle. \
         Put an API key in oracle.env, then open me again."
            .into()
    } else if e.starts_with("http 401") || e.starts_with("http 403") {
        "The oracle refused the diary's key. Check RIDDLE_OPENAI_KEY in oracle.env.".into()
    } else if e.starts_with("http ") {
        let code = e.split(':').next().unwrap_or("an error");
        format!("The oracle rejected the diary's plea ({code}). Check the model and endpoint in oracle.env.")
    } else if e.contains("request failed") || e.contains("timed out") {
        "The diary cannot reach its oracle. Is the tablet connected to Wi-Fi?".into()
    } else if e.contains("empty reply") {
        "The spirit read your words but said nothing. Write again.".into()
    } else {
        "The ink blurred before it could answer. Write again.".into()
    }
}

/// Summon a remembered page: snapshot today's page, clear the paper, and plan
/// the memory's rewriting — the date in a small hand, the writer's own strokes
/// exactly as they were penned, Tom's old reply beneath — all in faded ink.
fn conjure(
    font: &FontRef,
    store: &Option<memory::MemoryStore>,
    id: u64,
    surf: &mut Surface,
    disp: &display::Display,
) -> Option<State> {
    let s = store.as_ref()?;
    let entry = s.get(id)?.clone();
    let strokes = s.strokes(id).unwrap_or_default();
    eprintln!("riddle: conjuring memory {id} ({})", memory::spoken_date(id));

    let saved = surf.copy_rect(0, 0, SCREEN_W, SCREEN_H);
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    disp.update_all(surf.w, surf.h);

    let mut all: Vec<Vec<(i32, i32, i32)>> = Vec::new();
    let mut region = BBox::empty();

    // The date, small and centered near the top, like a diary heading.
    let date = memory::spoken_date(entry.id);
    let mut raster = script::rasterize_line(font, &date, 54.0 * SCREEN_W as f32 / fb::REF_W as f32);
    script::thin(&mut raster);
    let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
    let date_top = fb::s(64);
    let mut ink_bottom = date_top;
    for stroke in script::trace(&raster) {
        let mapped: Vec<(i32, i32, i32)> =
            stroke.iter().map(|&(sx, sy)| (x0 + sx, date_top + sy, 1)).collect();
        for &(x, y, r) in &mapped {
            region.add(x, y, r + 2);
            ink_bottom = ink_bottom.max(y);
        }
        all.push(mapped);
    }

    // The writer's own hand, exactly as it was penned.
    for stroke in &strokes {
        for &(x, y, r) in stroke {
            region.add(x, y, r + 2);
            ink_bottom = ink_bottom.max(y);
        }
        all.push(stroke.clone());
    }

    // Tom's old reply, below.
    if !entry.reply.is_empty() {
        let y = (ink_bottom + fb::s(130)).min(SCREEN_H as i32 - fb::s(400));
        let reply = plan_reply(font, &entry.reply, Some(y));
        for stroke in reply.strokes {
            let mapped: Vec<(i32, i32, i32)> = stroke.iter().map(|&(x, y)| (x, y, 2)).collect();
            for &(x, y, r) in &mapped {
                region.add(x, y, r + 2);
            }
            all.push(mapped);
        }
    }

    Some(State::Conjuring {
        plan: ConjurePlan { strokes: all, stroke_i: 0, point_i: 0, region },
        next: Instant::now(),
        saved,
    })
}

/// Lay out reply text and produce screen-space strokes. `y_start` continues a
/// streamed reply below its previous chunk; None places the first chunk.
fn plan_reply(font: &FontRef, text: &str, y_start: Option<i32>) -> WritePlan {
    let max_w = (SCREEN_W as i32 - 2 * MARGIN_X) as f32;
    let lines = script::wrap(font, text, REPLY_PX, max_w);
    let line_h = (REPLY_PX * 1.25) as i32;
    let total_h = line_h * lines.len() as i32;
    let mut y = y_start.unwrap_or(((SCREEN_H as i32 - total_h) / 3).max(fb::s(60)));
    let mut strokes = Vec::new();
    let mut region = BBox::empty();
    let mut seed = 0x1234u32;
    let mut jitter = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((seed >> 16) % 7) as i32 - 3
    };

    for line_text in &lines {
        let mut raster = script::rasterize_line(font, line_text, REPLY_PX);
        script::thin(&mut raster);
        let line_strokes = script::trace(&raster);
        let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
        let wobble = jitter();
        for s in line_strokes {
            let mapped: Vec<(i32, i32)> = s.iter().map(|&(sx, sy)| (x0 + sx, y + sy + wobble)).collect();
            for &(x, yy) in &mapped {
                region.add(x, yy, 5);
            }
            strokes.push(mapped);
        }
        y += line_h;
    }

    WritePlan { strokes, stroke_i: 0, point_i: 0, region, next_y: y, shade: BLACK }
}

/// Half-size of the clear button's touch target, top-right of the page.
const CLEAR_BTN: i32 = fb::s(52);

/// Where the clear button sits. Annotation mode is a shared canvas: the ink is
/// never drunk and the marks never fade, so without this the page only ever
/// accumulates — and a page crowded with old marks is also the page the model
/// has to read.
fn clear_button() -> BBox {
    let cx = SCREEN_W as i32 - CLEAR_BTN - fb::s(30);
    let cy = CLEAR_BTN + fb::s(30);
    let mut b = BBox::empty();
    b.add(cx - CLEAR_BTN, cy - CLEAR_BTN, 0);
    b.add(cx + CLEAR_BTN, cy + CLEAR_BTN, 0);
    b
}

/// Undo sits under the clear button. Touch gestures are not an option here:
/// `touch::TouchDevice` is only opened in takeover mode, and the rM2 runs under
/// qtfb, so two-finger-tap-to-undo never reaches this process.
fn undo_button() -> BBox {
    let c = clear_button();
    let mut b = BBox::empty();
    b.add(c.x0, c.y1 + fb::s(24), 0);
    b.add(c.x1, c.y1 + fb::s(24) + (c.y1 - c.y0), 0);
    b
}

fn hit(b: BBox, x: i32, y: i32) -> bool {
    !b.is_empty() && x >= b.x0 && x <= b.x1 && y >= b.y0 && y <= b.y1
}

fn draw_buttons(surf: &mut Surface, disp: &display::Display) {
    paint_buttons(surf);
    let (x, y, w, h) = clear_button().rect();
    disp.update(x, y, w, h, false);
    let (ux, uy, uw, uh) = undo_button().rect();
    disp.update(ux, uy, uw, uh, false);
}

/// The glyphs alone, with no display update, so `--draw-test` can render the
/// furniture into a PNG on the host and it can actually be looked at.
fn paint_buttons(surf: &mut Surface) {
    let b = clear_button();
    let (x, y, w, h) = b.rect();
    let inset = fb::s(16);
    // A plain X. Drawn faintly: it is furniture, not one of the diary's marks.
    surf.brush_line(x + inset, y + inset, x + w - inset, y + h - inset, 2, FADED);
    surf.brush_line(x + w - inset, y + inset, x + inset, y + h - inset, 2, FADED);

    // Undo: an arc with an arrowhead. The sweep is barely more than half a
    // turn on purpose — the first version ran 286 degrees, which is close
    // enough to a closed ring that the head vanished into it and the button
    // read as "a circle", telling the writer nothing about what it does.
    let u = undo_button();
    let (ux, uy, uw, uh) = u.rect();
    let (cx, cy) = (ux + uw / 2, uy + uh / 2 + fb::s(6));
    let r = (uw.min(uh) / 2 - fb::s(16)) as f32;
    let (a0, a1) = (0.35f32, 3.35f32);
    let mut prev: Option<(i32, i32)> = None;
    let mut a = a0;
    while a <= a1 {
        let p = (cx + (r * a.cos()) as i32, cy - (r * a.sin()) as i32);
        if let Some(q) = prev {
            surf.brush_line(q.0, q.1, p.0, p.1, 2, FADED);
        }
        prev = Some(p);
        a += 0.1;
    }
    // Head at the left end, pointing back the way the arc came: two barbs off
    // the tangent, long enough to read at this size.
    let tip = (cx + (r * a1.cos()) as i32, cy - (r * a1.sin()) as i32);
    let barb = fb::s(20) as f32;
    for off in [-0.7f32, 0.7] {
        let d = a1 + std::f32::consts::FRAC_PI_2 + off;
        surf.brush_line(
            tip.0,
            tip.1,
            tip.0 + (barb * d.cos()) as i32,
            tip.1 - (barb * d.sin()) as i32,
            2,
            FADED,
        );
    }
}

fn draw_clear_button(surf: &mut Surface, disp: &display::Display) {
    draw_buttons(surf, disp);
}

/// Wipe the page back to blank paper and forget both layers.
///
/// A full (non-flashing) update, not a partial one: annotation mode leaves
/// marks on e-ink for many turns, and partial updates leave ghosts behind that
/// a partial clear would preserve.
fn clear_page(surf: &mut Surface, disp: &display::Display) {
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    draw_clear_button(surf, disp);
    disp.update(0, 0, SCREEN_W as i32, SCREEN_H as i32, false);
}

/// The region the diary's own marks occupy, for the crop sent to the model.
fn marks_bbox(marks: &[Mark]) -> BBox {
    let mut b = BBox::empty();
    for m in marks {
        if !m.bbox.is_empty() {
            b.add(m.bbox.x0, m.bbox.y0, 0);
            b.add(m.bbox.x1, m.bbox.y1, 0);
        }
    }
    b
}

/// Redraw the whole page from the two layers.
///
/// Removing a mark cannot be done by un-drawing it — the marks cross the
/// writer's ink, so whitening a mark's pixels takes the writing with it. The
/// layers are authoritative and the page is derived from them, which is the
/// only way erase and undo can be correct.
fn recompose(
    surf: &mut Surface,
    disp: &display::Display,
    user: &ink::Ink,
    marks: &[Mark],
    annotating: bool,
) {
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    for stroke in user.stroke_list() {
        for (i, &(x, y, r)) in stroke.iter().enumerate() {
            if i > 0 {
                let (px, py, pr) = stroke[i - 1];
                surf.brush_line(px, py, x, y, r.min(pr + 1), BLACK);
            } else {
                surf.stamp(x, y, r, BLACK);
            }
        }
    }
    for m in marks {
        for stroke in &m.strokes {
            for (i, &(x, y)) in stroke.iter().enumerate() {
                if i > 0 {
                    let (px, py) = stroke[i - 1];
                    surf.brush_line(px, py, x, y, 2, FADED);
                } else {
                    surf.stamp(x, y, 2, FADED);
                }
            }
        }
    }
    if annotating {
        draw_clear_button(surf, disp);
    }
    // Full, non-flashing: a page this old has accumulated ghosts that a
    // partial update would keep.
    disp.update(0, 0, SCREEN_W as i32, SCREEN_H as i32, false);
}

/// Drop every mark the eraser tip is touching. Returns whether any went.
///
/// Without this the eraser whitens a mark's pixels but leaves its op live in
/// layer 1, so the page and the state the model is told about drift apart — the
/// writer rubs a mark out and the diary keeps insisting it is there.
fn erase_marks_near(marks: &mut Vec<Mark>, x: i32, y: i32, r: i32) -> bool {
    let r2 = (r + 4) * (r + 4);
    let before = marks.len();
    marks.retain(|m| {
        !m.strokes.iter().flatten().any(|&(mx, my)| {
            let (dx, dy) = (mx - x, my - y);
            dx * dx + dy * dy <= r2
        })
    });
    before != marks.len()
}

/// Note which marks the writer has since drawn on. That is the round trip: the
/// model gets told which of its marks were answered, rather than having to spot
/// fresh ink near old geometry in a flattened raster.
fn note_answered(marks: &mut [Mark], user: &ink::Ink) {
    for m in marks {
        if m.answered || m.bbox.is_empty() {
            continue;
        }
        let near = fb::s(40);
        m.answered = user.stroke_list().iter().flatten().any(|&(x, y, _)| {
            x >= m.bbox.x0 - near
                && x <= m.bbox.x1 + near
                && y >= m.bbox.y0 - near
                && y <= m.bbox.y1 + near
        });
    }
}

/// Turn annotation ops into a write plan. The animator is modality-agnostic —
/// it walks polylines and has no idea whether they came from text or geometry —
/// so this is the whole of the second producer.
///
/// `view` is the crop the model was answering about. Without it the normalized
/// coordinates are meaningless, so a missing view yields no marks rather than
/// marks in the wrong place.
fn plan_marks(
    font: &FontRef,
    marks: &[ops::Op],
    view: Option<&ink::PageView>,
    page: &Surface,
) -> WritePlan {
    let empty = WritePlan {
        strokes: Vec::new(),
        stroke_i: 0,
        point_i: 0,
        region: BBox::empty(),
        next_y: 0,
        shade: FADED,
    };
    let Some(view) = view else {
        eprintln!("riddle: marks arrived with no page view; dropping {}", marks.len());
        return empty;
    };
    let strokes = ops::ops_to_strokes(marks, view, font, Some(page));
    if strokes.is_empty() {
        return empty;
    }
    let mut region = BBox::empty();
    let mut lowest = 0;
    for s in &strokes {
        for &(x, y) in s {
            region.add(x, y, 5);
            lowest = lowest.max(y);
        }
    }
    WritePlan { strokes, stroke_i: 0, point_i: 0, region, next_y: lowest, shade: FADED }
}

/// Render each op separately, so every mark keeps the strokes that are its
/// own. A single combined plan animates fine but cannot be taken apart again,
/// and erase and undo both need to remove exactly one mark.
///
/// Returns the plan to animate and the marks to remember. The marks are capped:
/// a model that returns a dozen ops would otherwise bury the page under a turn
/// that takes ten seconds to draw.
fn plan_each(
    font: &FontRef,
    ops: &[ops::Op],
    view: Option<&ink::PageView>,
    page: &Surface,
) -> (WritePlan, Vec<Mark>) {
    const MAX_MARKS: usize = 3;
    let mut plan = plan_marks(font, &[], view, page);
    let mut marks = Vec::new();
    let Some(v) = view else { return (plan, marks) };
    if ops.len() > MAX_MARKS {
        eprintln!("riddle: {} marks offered, keeping {MAX_MARKS}", ops.len());
    }
    for op in ops.iter().take(MAX_MARKS) {
        let one = plan_marks(font, std::slice::from_ref(op), Some(v), page);
        if one.strokes.is_empty() {
            continue;
        }
        plan.region.add(one.region.x0, one.region.y0, 0);
        plan.region.add(one.region.x1, one.region.y1, 0);
        plan.next_y = plan.next_y.max(one.next_y);
        marks.push(Mark {
            op: op.clone(),
            view: *v,
            strokes: one.strokes.clone(),
            bbox: one.region,
            answered: false,
        });
        plan.strokes.extend(one.strokes);
    }
    (plan, marks)
}

/// Splice a streamed continuation chunk into a running write animation.
fn append_reply(font: &FontRef, plan: &mut WritePlan, more: &str) {
    let cont = plan_reply(font, more, Some(plan.next_y));
    if cont.strokes.is_empty() {
        return;
    }
    plan.region.add(cont.region.x0, cont.region.y0, 0);
    plan.region.add(cont.region.x1, cont.region.y1, 0);
    plan.strokes.extend(cont.strokes);
    plan.next_y = cont.next_y;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop sleeps until `state_deadline`, so a deadline the state machine
    /// will not act on is a zero-length sleep — a spin at full tilt rather than
    /// the old 2ms tick. `Listening` only commits with ink on the page and the
    /// pen up, and this must agree with that guard exactly.
    #[test]
    fn listening_offers_no_deadline_it_will_not_act_on() {
        let long_ago = Instant::now() - Duration::from_secs(60);
        let listening = State::Listening { last_pen: Some(long_ago) };

        // Ink down, pen up: the commit is genuinely due.
        assert!(state_deadline(&listening, false, false).is_some());

        // The writer rubbed the page out before the pause — `last_pen` stays
        // set forever and nothing will ever consume it.
        assert!(state_deadline(&listening, true, false).is_none());
        // Still writing: pen events drive the loop, not the clock.
        assert!(state_deadline(&listening, false, true).is_none());
        // Never touched the page at all.
        assert!(state_deadline(&State::Listening { last_pen: None }, true, false).is_none());
    }

    /// Every other state either animates on a timer or expires, so each one
    /// must name a moment — otherwise the diary sleeps through its own
    /// animation until the 250ms idle cap wakes it.
    #[test]
    fn timed_states_all_name_a_deadline() {
        let soon = Instant::now() + Duration::from_millis(50);
        let region = BBox::empty();
        let states = [
            State::Lingering { until: soon, region },
            State::FadingReply { stage: 0, next: soon, region },
            State::Help { panel: None, until: soon },
            State::MemoryShown { saved: None, until: soon, region },
        ];
        for s in &states {
            assert!(state_deadline(s, true, false).is_some(), "a timed state sleeps");
        }
    }
}
