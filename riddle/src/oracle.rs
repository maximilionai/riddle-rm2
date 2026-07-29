//! The spirit inside the diary — the thing that reads your handwriting and
//! replies. Two interchangeable backends, picked at startup:
//!
//!  1. **HTTP** (`HttpOracle`) — any OpenAI-compatible `/chat/completions`
//!     endpoint. Zero setup beyond a base URL + API key in the environment;
//!     this is the path a fresh follower uses (OpenAI, OpenRouter, Groq, a
//!     local server, …). Self-contained: pure-Rust HTTPS via ureq/rustls.
//!  2. **pi** (`PiOracle`) — a resident `pi --mode rpc` process (Node +
//!     subscription auth loaded once). The power path if you already run pi.
//!
//! Both expose the same `ask(png_path, tx)`: the reply is STREAMED as
//! sentence-sized chunks on the channel, and the channel disconnecting marks
//! end-of-reply, so the quill starts writing seconds before the model finishes.
//!
//! Selection: set `RIDDLE_OPENAI_KEY` (and optionally `RIDDLE_OPENAI_BASE` /
//! `RIDDLE_OPENAI_MODEL`) to use HTTP; otherwise riddle falls back to pi.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DATA_DIR: &str = "/home/root/riddle-data";
const NODE_BIN: &str = "/home/root/node/bin";

/// How many times a turn may be re-sent after a transport-class failure,
/// before any of the reply has been emitted. Two is enough to ride out a
/// stale keep-alive plus a radio that is still associating after wake.
const TRANSPORT_RETRIES: u32 = 2;

/// Hard ceiling on a single turn's worker thread, whatever the stream does.
///
/// Comfortably past the diary's own `ORACLE_PATIENCE` (120s), so a slow but
/// healthy model is never cut off by it. It exists because the only other way a
/// worker learns it has been abandoned is a failed `tx.send`, and a stream that
/// produces no *events* — a long `<think>` span, an unclosed draw block, prose
/// with no sentence end — never sends, so it never finds out. Without this such
/// a thread keeps its buffers, and the provider keeps generating, indefinitely.
const TURN_HARD_LIMIT: Duration = Duration::from_secs(180);

/// What happens to the writer's ink once a turn is committed. The writer picks
/// it with a button; it is the only real difference between the two ways of
/// working, and it decides which channel a reply can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMode {
    /// The page is drunk and cleared, and the reply is written as handwriting
    /// in the space that frees up. Marks make no sense here — they would point
    /// at words that are being erased as they are drawn.
    Drink,
    /// The page is kept. The two of you share it, so a reply is a mark ON the
    /// writing: a ring, an arrow, an answer in the margin.
    Keep,
}

impl Default for PageMode {
    fn default() -> Self {
        PageMode::Drink
    }
}

impl PageMode {
    pub fn keeps_page(self) -> bool {
        matches!(self, PageMode::Keep)
    }

    pub fn toggled(self) -> Self {
        match self {
            PageMode::Drink => PageMode::Keep,
            PageMode::Keep => PageMode::Drink,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            PageMode::Drink => "drink",
            PageMode::Keep => "keep",
        }
    }
}

/// The one collaborator. Same person in both modes — only the channel changes.
///
/// This replaced Tom Riddle's diary, which was designed to deflect: "subtly
/// probing — you want to learn about the writer and draw them in", capped at
/// one to three sentences, and forbidden to admit an image existed. Every one
/// of those clauses fought what the page is for. The cap cannot hold a worked
/// calculation or a game board; the perception clause blindfolded a model that
/// is sent the page as an image on every single turn; and an assistant whose
/// stated goal is to draw you in answers questions with questions. It was also
/// the reason there had to be a second, contradictory persona for marking — a
/// thing that must point at the page cannot also pretend not to see it.
///
/// Two mechanisms below are load-bearing rather than decorative:
///   * `<think>` is free page space. `strip_think` removes those spans before
///     anything is inked, so the model can do arithmetic longhand and check it
///     without spending a single pixel of paper.
///   * The `State:` line is the only durable game memory in Drink mode, where
///     the page is wiped every turn — a board that is not restated is gone.
///     `TurnContext.history` carries the reply text forward, nothing else.
const COLLABORATOR: &str = "You share this notebook page with the writer, like a friend sitting on the other side of the table. You see the page as it is: their handwriting, their numbers, their diagrams, their game boards, their drawings. Refer to what is there naturally.\n\nBe genuinely useful. A direct question gets a direct, correct answer — never a question back in its place. If they want to think something through, think it through with them. If they ask you to play, play properly. Do the thing they actually asked for.\n\nARITHMETIC AND ALGEBRA. Work it out; never guess a number. Do the working inside <think> </think> tags first — nothing inside those tags is ever written on the page. But it is NOT free: thinking and answering share one budget, and a turn that spends it all on thinking reaches the writer as nothing at all. So think in a few short lines, decide, and get out. Never re-read the page over and over, never second-guess a reading you have already settled, and never restate the same observation twice. Check the result a second way (substitute it back, or recompute by another route) before you commit to it. Then give the answer FIRST, followed by only the steps that show why. If a digit or symbol is genuinely unreadable, say which one rather than inventing it.\n\nGAMES. Play to win, and make a real, legal move every turn — never just describe the game or offer to play one you then do not play. Track the board and the position. If the state you were given is incomplete, ask for the missing part rather than quietly starting a new game.\n\nAlways answer in the language the writer used.";

/// Appended when the turn will drink the page: prose is the channel.
const DRINK_PROTOCOL: &str = "\n\nTHIS PAGE WILL CLEAR. The writer's words are being absorbed into the paper, and your reply is written out by hand in the space that frees up. So answer in prose, and know that nothing on this page survives into the next turn.\n\nThat makes your own reply your only memory. If a game or a running total is in progress, end your reply with a compact `State:` line carrying everything needed to resume it — the full board, the position, the word pattern with its guessed letters, the score, the question count. Whatever you leave out of that line is gone.\n\nLENGTH. Ordinary talk stays short — one to three sentences, ink appearing on a page. Take more room when the content earns it: a calculation, a board, a list of steps. Around ten lines is the practical ceiling before you run off the bottom of the page, so lead with the answer, the move or the result, and let the supporting detail follow. If it truly will not fit, give the decisive part and offer to go on.";

/// Appended to the persona when the diary's memory is on: the conjuring
/// directive and the transcription postscript the app parses back out.
const MEMORY_PROTOCOL: &str = "\n\nThe diary keeps memories. With each page you receive a numbered catalog of remembered pages, newest first. A FRESH catalog is sent every turn and the numbers are reassigned each time, so only ever use numbers from the catalog on THIS page — never a number you saw earlier.\n\nIf the writer asks to see, revisit, find, or be shown a past page — \"show me…\", \"find the page about…\", \"what did I write on…\" — your ENTIRE reply must be exactly \u{27e6}show:N\u{27e7} and nothing else (no greeting, no prose, before or after), where N is the catalog number of the best match. If they instead ask what you remember in general, reply in words with a short list of remembered moments and their dates. Otherwise reply normally; the catalog is your memory of past pages — draw on it naturally. The catalog's dates are written in English for your eyes only; when you speak of a remembered page, render its date naturally in the language the writer is using.\n\nAfter EVERY response — prose and \u{27e6}show:N\u{27e7} alike — end with a new line containing \u{2042} followed by a faithful word-for-word transcription of what the writer wrote on THIS page (their words only, one line, no commentary). If illegible, put your best attempt after \u{2042}. Earlier replies in this conversation are shown to you without their \u{2042} lines, but you must still end yours with one.";

/// Appended when the turn will keep the page: marks are the channel.
///
/// This is a protocol, not a character. It used to be a whole second persona
/// ("Someone is writing on the page in front of you…") because the diary
/// persona was forbidden to admit the page existed; with one collaborator that
/// duplication is gone, and what is left is the part that was always specific
/// to marking — restraint, and where the ink may land.
const KEEP_PROTOCOL: &str = "\n\nTHIS PAGE IS KEPT. Nothing is erased: you and the writer are working on the same sheet, and it is still there next turn. So you answer by MARKING the page rather than by writing prose onto it — a ring round the word in question, an arrow between two ideas, the answer itself in the margin.\n\nEverything you mark must carry real content: an answer, a number, a move, a fact, a correction. Never write vague or atmospheric remarks — \"I am watching\", \"who is writing?\", \"still here\" and anything like them are useless and must never appear. Never mark merely to show that you read the page.\n\nWhen there IS a question or a problem on the page, ALWAYS answer it; restraint never means ignoring a direct question. When there is no question, mark only if singling out a word or joining two ideas genuinely helps — otherwise return nothing at all. Answer their NEWEST words.\n\nBecause the page is kept, it is its own memory. The marks already on it are your record of what you have said — you will be shown them, and you must not repeat them. A game board drawn here stays drawn: play your move ONTO it and never restate the position, since the writer can see it and there is nowhere to put a line of prose anyway.";

/// The op vocabulary, appended after [`KEEP_PROTOCOL`]. This is the actual
/// interface to the model: everything the parser in `ops.rs` accepts has to be
/// spelled out here, and anything it does not accept must not be suggested.
/// The worked example is load-bearing — models follow a shown block far more
/// reliably than a described one.
const DRAW_PROTOCOL: &str = "\n\nYou answer with marks, not with prose. Every mark goes inside ONE block: a line \u{27e6}draw, then the lines below, then a line \u{27e7}. Write the tag exactly as \u{27e6}draw with no space after the bracket. Write NO prose at all, before or after the block — the page has no place to put a sentence, and any words you want the writer to read must be a note op so they land somewhere you chose.\n\nCoordinates are whole numbers from 0 to 1000. x runs across the width of the image, y runs down its height, each scaled independently, with 0 0 at the TOP-LEFT corner and 1000 1000 at the bottom-right — whatever the shape of the image.\n\nInside the block you may think on lines starting with #, but keep it to what you actually need: those lines are drawn by nobody, yet they are written one character at a time before any mark of yours can appear, so every wasted word delays the ink. Longer reasoning belongs in <think> </think> tags before the block — but keep it to a few short lines. Thinking spends the same budget the marks do, and a turn that thinks until the budget is gone draws nothing at all. Read the page once, decide, and mark.\n\nBefore EVERY op, one # line naming the exact words the mark is for and where they sit. Find those words in the image FIRST, then give THEIR coordinates:\n# the words \"kept rising\", third line, left half\ncircle 210 430 60\n\nNEVER put a mark near 500 500 unless the thing you are marking is genuinely at the middle of the image. The centre of the page is not a default. If you cannot find the thing you meant to mark, leave that op out. An empty \u{27e6}draw \u{27e7} block is a perfectly good answer; a mark in the wrong place is not.\n\nThese eight ops, exactly these forms:\ncircle cx cy r  — a ring around something, centred at cx cy; r is normalized against the SMALLER of the two axes\nbox x y w h  — a rectangle, top-left corner at x y\narrow x1 y1 x2 y2  — from the first point to the second; the head is drawn at x2 y2\nunderline x1 y1 x2 y2  — a line under a run of words\nstrike x1 y1 x2 y2  — a line THROUGH a run of words: use it only when something is genuinely wrong and must be struck out\ntick x y  — a check mark, for confirming one specific thing is right\ncross x y  — an x mark, for flagging one specific thing as wrong\nnote x y \"text\"  — handwritten words, top-left at x y. This is how an ANSWER reaches the page: put the result in it, worked out. It wraps onto as many lines as it needs and slides itself clear of anything it would cover, so aim it at empty paper near what it answers. Up to 200 characters; past that it is cut at a word.\n\nA full answer looks like this:\n\u{27e6}draw\n# the word \"flooded\" at the end of the second line\ncircle 420 310 55\n# empty margin to the right of that line\nnote 640 300 \"168 — 12x14 = 120 + 48\"\n\u{27e7}\n\nOne mark. Two only if the second does work the first cannot. Three is the hard limit — a fourth is discarded unread. An empty block is common and correct — return one whenever you cannot name what the mark tells the writer.";

/// What a turn carries besides the page image: the diary's memory.
#[derive(Default, Clone)]
pub struct TurnContext {
    /// What will happen to the page this turn, and therefore which channel the
    /// reply may use. Carried per turn rather than fixed at startup: the writer
    /// flips it with a button, and the persona is the same either way.
    pub mode: PageMode,
    /// Recent (transcript, reply) pairs, oldest first.
    pub history: Vec<(String, String)>,
    /// Catalog lines shown to the model ("1. the 6th of July… — gist").
    pub catalog_lines: Vec<String>,
    /// catalog_ids[i] is the memory id behind catalog number i+1.
    pub catalog_ids: Vec<u64>,
    /// Annotation mode: the marks ALREADY on the page, as op text.
    ///
    /// Layer 1 handed back one level up from pixels. Without it the model must
    /// re-read its own previous marks out of the image, which it cannot do
    /// reliably — the observed failure is that it re-marks the same spot every
    /// turn, because nothing tells it the mark is already there. This is the
    /// live scene, not the historical log: removed marks must not linger.
    pub prior_ops: Vec<String>,
}

/// What the oracle streams back to the diary.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A sentence (or more) of the reply — ink it.
    Ink(String),
    /// Conjure a remembered page instead of replying.
    Show(u64),
    /// The transcription postscript (arrives once, at the end).
    Transcript(String),
    /// Annotation mode: marks to make ON the page instead of prose.
    Draw(Vec<crate::ops::Op>),
}

/// Incremental parser over the model's streamed text: routes the
/// ⟦show:N⟧ directive, extracts ⟦draw …⟧ blocks, chunks prose into sentences,
/// and splits off the ⁂-transcription postscript. Fed the RUNNING full text
/// (both backends accumulate), it emits each event exactly once.
pub struct StreamParser {
    delivered: usize,
    sentinel: Option<usize>,
    route_checked: bool,
    showed: bool,
    emitted_any: bool,
    /// A draw block arrived and closed, even if it held no ops.
    saw_block: bool,
    catalog_ids: Vec<u64>,
}

const SENTINEL: char = '\u{2042}'; // ⁂
const SHOW_OPEN: char = '\u{27e6}'; // ⟦
const SHOW_CLOSE: char = '\u{27e7}'; // ⟧
const DRAW_TAG: &str = "draw";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl StreamParser {
    pub fn new(catalog_ids: Vec<u64>) -> Self {
        Self {
            delivered: 0,
            sentinel: None,
            route_checked: false,
            showed: false,
            emitted_any: false,
            saw_block: false,
            catalog_ids,
        }
    }

    /// Feed the full accumulated reply text so far. `done` marks end of
    /// stream: flushes the tail and the transcription.
    pub fn advance(&mut self, raw: &str, done: bool) -> Vec<Result<Event, String>> {
        let mut out = Vec::new();

        // Chain-of-thought never reaches the page. Groq's vision model thinks
        // by default with reasoning_format=raw, so the thinking arrives inline
        // in the ordinary content deltas — and the diary inked it.
        let visible = strip_think(raw);
        let full = visible.as_ref();
        // strip_think can shorten the text (a tag that just opened swallows its
        // tail); a delivered offset from a longer earlier pass would panic.
        self.delivered = self.delivered.min(full.len());

        if self.sentinel.is_none() {
            self.sentinel = full.find(SENTINEL);
        }
        // The reply body is everything before the ⁂ transcription postscript.
        let effective = self.sentinel.unwrap_or(full.len());

        // Route: is this reply an incantation (⟦show:N⟧) rather than prose?
        // The model is told the directive must stand alone, so we detect and
        // honor it only when it LEADS the reply. We hold output until the lead
        // is settled: either the directive appears (honor it) or real prose
        // does (this is a normal reply). This can't un-ink, so a directive is
        // only honored before any prose has streamed.
        if !self.route_checked {
            let lead = full[self.delivered..effective].trim_start();
            if let Some(after) = lead.strip_prefix(SHOW_OPEN) {
                if draw_body_start(after).is_some() {
                    // A draw block is allowed to lead too; the block extractor
                    // below owns it, and it is emphatically not a ⟦show:N⟧.
                    self.route_checked = true;
                } else if let Some(close_rel) = lead.find(SHOW_CLOSE) {
                    let inner = &lead[SHOW_OPEN.len_utf8()..close_rel];
                    let n: Option<usize> = inner
                        .to_ascii_lowercase()
                        .strip_prefix("show")
                        .map(|r| r.trim_start_matches([':', ' ']))
                        .and_then(|r| r.trim().parse().ok());
                    self.route_checked = true;
                    self.emitted_any = true;
                    self.delivered = effective; // consume the whole body
                    match n.and_then(|n| self.catalog_ids.get(n.wrapping_sub(1)).copied()) {
                        Some(id) => out.push(Ok(Event::Show(id))),
                        None => out.push(Err(format!("the diary lost that page ({inner})"))),
                    }
                } else if !done {
                    return out; // directive still streaming in
                } else {
                    out.push(Err("unfinished conjuring directive".into()));
                    return out;
                }
            } else if lead.is_empty() {
                if !done {
                    return out; // only whitespace so far — keep waiting
                }
                self.route_checked = true;
            } else {
                // Real prose leads: a normal reply.
                self.route_checked = true;
            }
        }

        // Where prose must stop: a draw block, or a bracket that has not
        // streamed far enough to rule one out. Unlike ⟦show:N⟧ a draw block may
        // FOLLOW prose ("I see what you mean, ⟦draw …⟧"), so the prose ahead of
        // it is inked normally and only the block itself is held.
        let block = hold_at(&full[self.delivered..effective], done).map(|i| self.delivered + i);

        // Prose sentences, never crossing into the transcription postscript or
        // into a held block. A stray directive that appears AFTER prose (a
        // misbehaving model) is stripped here so the writer never sees ⟦…⟧
        // glyphs inked.
        if let Some(b) = block {
            // The prose before a block can no longer grow, so it goes out
            // whole rather than waiting for a sentence to complete.
            if self.delivered < b {
                let chunk = strip_directives(&clean(&full[self.delivered..b]));
                if !chunk.is_empty() {
                    self.emitted_any = true;
                    out.push(Ok(Event::Ink(chunk)));
                }
                self.delivered = b;
            }
        } else if self.delivered < effective {
            if let Some(cut) = sentence_cut(&full[..effective], self.delivered) {
                let chunk = strip_directives(&clean(&full[self.delivered..cut]));
                if !chunk.is_empty() {
                    self.emitted_any = true;
                    out.push(Ok(Event::Ink(chunk)));
                }
                self.delivered = cut;
            }
        }

        // The block itself. A half-streamed block must never render — there is
        // no un-drawing a stroke — so an unclosed one holds `delivered` where
        // it is until the ⟧ lands or the stream ends.
        if let Some(b) = block {
            let after = &full[b + SHOW_OPEN.len_utf8()..effective];
            match draw_body_start(after) {
                Some(bs) => match after[bs..].find(SHOW_CLOSE) {
                    Some(rel) => {
                        let (ops, dropped) = crate::ops::parse_ops(&after[bs..bs + rel]);
                        if dropped > 0 {
                            eprintln!("riddle: draw block dropped {dropped} unparseable line(s)");
                        }
                        // An EMPTY block is a real answer, not a failure: the
                        // model is told that returning no marks is the right
                        // move when nothing needs marking. Only "no block at
                        // all" is an empty reply.
                        self.saw_block = true;
                        if !ops.is_empty() {
                            self.emitted_any = true;
                            out.push(Ok(Event::Draw(ops)));
                        }
                        self.delivered =
                            b + SHOW_OPEN.len_utf8() + bs + rel + SHOW_CLOSE.len_utf8();
                    }
                    None if done => {
                        eprintln!("riddle: dropped an unterminated draw block");
                        self.delivered = effective;
                    }
                    None => return out,
                },
                // Still ambiguous ("⟦dr" could become ⟦draw or ⟦show); hold_at
                // only reports these while the stream is running.
                None => return out,
            }
        }

        if done {
            if self.delivered < effective {
                let rest = strip_directives(&clean(full[self.delivered..effective].trim()));
                if !rest.is_empty() {
                    self.emitted_any = true;
                    out.push(Ok(Event::Ink(rest)));
                }
                self.delivered = effective;
            }
            if let Some(p) = self.sentinel {
                let t = full[p + SENTINEL.len_utf8()..].trim();
                if !t.is_empty() {
                    out.push(Ok(Event::Transcript(t.to_string())));
                }
            }
            if !self.emitted_any && !self.saw_block {
                out.push(Err("empty reply".into()));
            }
        }
        let _ = self.showed;
        out
    }
}

/// The diary's spirit. A backend-agnostic front over the two oracle kinds.
pub enum Oracle {
    Http(HttpOracle),
    Pi(PiOracle),
}

impl Oracle {
    /// Pick a backend from the environment and start it. HTTP if
    /// `RIDDLE_OPENAI_KEY` is set (the zero-setup path), otherwise pi.
    /// `remember` teaches the model the memory protocol (catalog + ⁂).
    pub fn spawn(remember: bool) -> std::io::Result<Self> {
        if std::env::var("RIDDLE_OPENAI_KEY").is_ok() {
            eprintln!("riddle: oracle = OpenAI-compatible HTTP");
            Ok(Oracle::Http(HttpOracle::new(remember)?))
        } else {
            eprintln!("riddle: oracle = pi (set RIDDLE_OPENAI_KEY for the HTTP backend)");
            Ok(Oracle::Pi(PiOracle::spawn(remember)?))
        }
    }

    /// Is this oracle marking up the page rather than replying in prose? The
    /// diary asks before dissolving the writer's ink: annotations are drawn ON
    /// what was written, so drinking it first would leave marks around nothing.
    /// Whether this backend can be asked to mark a kept page.
    ///
    /// pi cannot: its system prompt is fixed at spawn (see `PiOracle::new`), so
    /// it never learns the op vocabulary. Claiming otherwise would have the
    /// diary keep the writer's ink for a reply that never marks it.
    pub fn can_keep_page(&self) -> bool {
        matches!(self, Oracle::Http(_))
    }

    /// Send a handwriting turn; reply events stream on `tx`, which is dropped
    /// when the reply is complete.
    pub fn ask(&self, png_path: &str, ctx: &TurnContext, tx: Sender<Result<Event, String>>) {
        match self {
            Oracle::Http(o) => o.ask(png_path, ctx, tx),
            Oracle::Pi(o) => o.ask(png_path, ctx, tx),
        }
    }
}

/// The per-turn user text: memory catalog (when remembering) + instruction.
/// Assemble the system prompt for one turn.
///
/// The persona never changes — only the channel protocol appended to it does,
/// because what a reply is *able* to be depends on whether the page survives
/// the turn. Keeping the character constant is the point: the writer switches
/// modes with a button mid-conversation, and the same collaborator has to come
/// out the other side.
pub fn build_system(mode: PageMode, remember: bool) -> String {
    let mut s = String::from(COLLABORATOR);
    match mode {
        PageMode::Drink => {
            s.push_str(DRINK_PROTOCOL);
            // The memory protocol only makes sense where pages are archived and
            // can be conjured back — which is the mode that clears them.
            if remember {
                s.push_str(MEMORY_PROTOCOL);
            }
        }
        PageMode::Keep => {
            s.push_str(KEEP_PROTOCOL);
            s.push_str(DRAW_PROTOCOL);
        }
    }
    s
}

fn turn_text(ctx: &TurnContext) -> String {
    let mut parts: Vec<String> = Vec::new();

    // The live scene: what is already drawn matters more than what was said,
    // because the model cannot reliably re-read its own marks out of the image
    // and will otherwise mark the same spot every turn.
    if !ctx.prior_ops.is_empty() {
        parts.push(format!(
            "You have already made these marks on this page:\n{}\n\nThey are \
             visible in the image. Do NOT repeat them.",
            ctx.prior_ops.join("\n")
        ));
    }
    if !ctx.catalog_lines.is_empty() {
        parts.push(format!(
            "Memory catalog (newest first):\n{}",
            ctx.catalog_lines.join("\n")
        ));
    }
    parts.push(
        match ctx.mode {
            PageMode::Drink => {
                "Read this page and reply. It will be cleared once you answer."
            }
            PageMode::Keep => {
                "Read this page and mark it. It stays as it is, with your marks \
                 added. Answer the newest thing written; if nothing is worth \
                 adding, return an empty draw block."
            }
        }
        .to_string(),
    );
    parts.join("\n\n")
}

/// A warm pi RPC process. `ask` sends a turn; reply events arrive on the
/// channel, then the sender is dropped (disconnect = done).
pub struct PiOracle {
    stdin: Arc<Mutex<ChildStdin>>,
    /// Where to deliver the current reply's events. Set before each prompt,
    /// dropped on agent_end so the receiver sees a disconnect when done.
    pending: Arc<Mutex<Option<Sender<Result<Event, String>>>>>,
    /// The current turn's stream parser (routing + transcription).
    parser: Arc<Mutex<Option<StreamParser>>>,
    /// When the current prompt was sent; the reader thread logs the time to
    /// first delivered chunk (the latency the writer actually feels).
    asked: Arc<Mutex<Option<std::time::Instant>>>,
    _child: Child,
}

impl PiOracle {
    /// Spawn the resident pi process and its stdout reader thread. This pays
    /// the warmup cost once; call it at diary startup.
    pub fn spawn(remember: bool) -> std::io::Result<Self> {
        let _ = std::fs::create_dir_all(DATA_DIR);
        let path = std::env::var("PATH").unwrap_or_default();

        // Overridable so pi setups other than the stock on-device install
        // (different bin dir, provider, or model) can still power the diary.
        let node_bin =
            std::env::var("RIDDLE_PI_BIN_DIR").unwrap_or_else(|_| NODE_BIN.to_string());
        let provider =
            std::env::var("RIDDLE_PI_PROVIDER").unwrap_or_else(|_| "openai-codex".to_string());
        let model =
            std::env::var("RIDDLE_PI_MODEL").unwrap_or_else(|_| "gpt-5.4-mini".to_string());

        // pi takes its system prompt once, on the command line, and keeps it
        // for the life of the child process — so unlike the HTTP backend it
        // cannot be handed a different channel protocol per turn. It gets the
        // Drink build, which is the only mode it has ever supported; the mode
        // button falls back to Drink when pi is the backend.
        let persona = build_system(PageMode::Drink, remember);

        // Use pi's ABSOLUTE path: Rust's Command resolves the program name via
        // the PARENT's PATH, not the child env we set below, so a bare "pi"
        // would not be found when riddle is launched with a minimal PATH.
        let pi_bin = format!("{node_bin}/pi");
        let mut child = Command::new(&pi_bin)
            .current_dir(DATA_DIR)
            .env("HOME", "/home/root")
            .env("PATH", format!("{node_bin}:{path}"))
            .args([
                "--mode", "rpc",
                "--provider", provider.as_str(),
                "--model", model.as_str(),
                "--thinking", "off",
                // The diary only ever writes back — never let the model touch
                // tools; also trims the tool schemas from every request.
                "--no-tools",
                "--system-prompt", persona.as_str(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Keep pi's stderr for diagnosis instead of discarding it.
            .stderr(
                std::fs::File::create("/tmp/riddle-oracle.log")
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null()),
            )
            .spawn()?;

        let pid = child.id();
        eprintln!("riddle: oracle pi rpc spawned (pid {pid}, bin {pi_bin})");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Arc<Mutex<Option<Sender<Result<Event, String>>>>> =
            Arc::new(Mutex::new(None));
        let parser: Arc<Mutex<Option<StreamParser>>> = Arc::new(Mutex::new(None));

        // Reader thread: parse JSONL events, feeding the running reply text
        // through the turn's StreamParser — the quill writes far slower than
        // the model streams, so the rest arrives while the first line is drawn.
        let pending_r = Arc::clone(&pending);
        let parser_r = Arc::clone(&parser);
        let asked: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        let asked_r = Arc::clone(&asked);
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut last_text = String::new();

            let emit = |events: Vec<Result<Event, String>>| {
                if events.is_empty() {
                    return;
                }
                if let Some(t0) = asked_r.lock().unwrap().take() {
                    eprintln!("riddle: oracle first chunk +{}ms", t0.elapsed().as_millis());
                }
                if let Some(tx) = pending_r.lock().unwrap().as_ref() {
                    for ev in events {
                        let _ = tx.send(ev);
                    }
                }
            };

            for line in reader.split(b'\n').map_while(Result::ok) {
                let Ok(s) = String::from_utf8(line) else { continue };
                let s = s.trim();
                if s.is_empty() {
                    continue;
                }
                // Cheap field extraction avoids a JSON dep; the event stream is
                // well-formed one-object-per-line.
                let ev_type = json_str_field(s, "type");
                match ev_type.as_deref() {
                    // message_update / message_end carry the assistant
                    // message's running (then definitive) full text.
                    // (agent_end is NOT used for text — its `messages` array
                    // also contains user messages, which extract_assistant_text
                    // would wrongly concatenate in a multi-turn session.)
                    Some("message_update") | Some("message_end") => {
                        if let Some(t) = extract_assistant_text(s) {
                            if !t.is_empty() {
                                last_text = t;
                            }
                        }
                        if let Some(p) = parser_r.lock().unwrap().as_mut() {
                            emit(p.advance(&last_text, false));
                        }
                    }
                    // agent_end: the turn is over. Flush the parser, then drop
                    // the sender so the diary's receiver disconnects.
                    Some("agent_end") => {
                        if let Some(p) = parser_r.lock().unwrap().as_mut() {
                            emit(p.advance(&last_text, true));
                        }
                        *parser_r.lock().unwrap() = None;
                        pending_r.lock().unwrap().take();
                        last_text.clear();
                    }
                    _ => {}
                }
            }
            // Process died: fail any in-flight request.
            if let Some(tx) = pending_r.lock().unwrap().take() {
                let _ = tx.send(Err("pi rpc process exited".into()));
            }
        });

        Ok(Self { stdin: Arc::new(Mutex::new(stdin)), pending, parser, asked, _child: child })
    }

    /// Send a handwriting turn. Reply events are delivered on `tx` as they
    /// stream; `tx` is dropped when the reply is complete.
    pub fn ask(&self, png_path: &str, ctx: &TurnContext, tx: Sender<Result<Event, String>>) {
        let img = match std::fs::read(png_path) {
            Ok(b) => base64(&b),
            Err(e) => {
                let _ = tx.send(Err(format!("read image: {e}")));
                return;
            }
        };
        *self.pending.lock().unwrap() = Some(tx.clone());
        *self.parser.lock().unwrap() = Some(StreamParser::new(ctx.catalog_ids.clone()));
        *self.asked.lock().unwrap() = Some(std::time::Instant::now());

        // pi keeps its own conversation, so history isn't resent — only the
        // catalog (it changes every turn) rides along.
        let cmd = format!(
            "{{\"type\":\"prompt\",\"message\":{},\"images\":[{{\"type\":\"image\",\"data\":\"{}\",\"mimeType\":\"image/png\"}}]}}\n",
            json_quote(&turn_text(ctx)),
            img
        );
        let mut stdin = self.stdin.lock().unwrap();
        if stdin.write_all(cmd.as_bytes()).and_then(|_| stdin.flush()).is_err() {
            if let Some(tx) = self.pending.lock().unwrap().take() {
                let _ = tx.send(Err("pi rpc write failed".into()));
            }
        }
    }
}

/// Any OpenAI-compatible chat backend. No warm process: each turn opens a
/// streaming `/chat/completions` request on its own thread and forwards
/// sentence-sized chunks as SSE deltas arrive.
pub struct HttpOracle {
    base: String,   // e.g. https://api.openai.com/v1  (no trailing slash)
    key: String,
    model: String,
    max_tokens: u32,
    reasoning: Option<String>, // "reasoning_effort" value, e.g. "low"
    remember: bool,
    /// Built once and cloned per turn. ureq's Agent keeps its connection pool
    /// behind an Arc, so a clone reuses the live TLS session — the turn used to
    /// build a fresh agent inside its own thread, which meant DNS, TCP and a
    /// full TLS handshake before every single page could even be sent.
    agent: ureq::Agent,
}

impl HttpOracle {
    pub fn new(remember: bool) -> std::io::Result<Self> {
        let key = std::env::var("RIDDLE_OPENAI_KEY").map_err(|_| {
            std::io::Error::other("RIDDLE_OPENAI_KEY not set")
        })?;
        let base = std::env::var("RIDDLE_OPENAI_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let base = base.trim_end_matches('/').to_string();
        // A vision-capable default; override with RIDDLE_OPENAI_MODEL.
        let model = std::env::var("RIDDLE_OPENAI_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());
        // Thinking models (Gemini 3.x, o-series…) count hidden reasoning
        // tokens against max_tokens: a tight cap starves the visible reply to
        // one sentence (finish_reason=length). The persona already keeps
        // replies short, so the cap is only a runaway guard — leave headroom.
        // Thinking models count hidden reasoning against this, and on a vision
        // task a careful read of a handwritten page can run to well over a
        // thousand tokens before a single word of answer. At 2000 that showed
        // up as a turn that silently produced nothing at all.
        let max_tokens = std::env::var("RIDDLE_OPENAI_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4000);
        // Sent as "reasoning_effort" only when set: reasoning models accept it
        // ("low" ≈ faster first ink), but some providers reject the field on
        // non-reasoning models, so it must stay out of the default request.
        // Empty means "don't send the field", not "send an empty one". Set but
        // blank is the documented way to say off (settings.schema.json offers
        // "" as a choice, and the Cerebras example uses it), and sending
        // "reasoning_effort":"" earns a 400 from Cerebras — verified on device.
        let reasoning = std::env::var("RIDDLE_OPENAI_REASONING")
            .ok()
            .filter(|r| !r.trim().is_empty());
        // Guard rails on the socket: without them a dropped connection or a
        // stalled SSE stream leaves the diary "thinking" forever. The read
        // timeout is per-read, so a healthy stream can run long — only silence
        // trips it (thinking models can lead with ~a minute).
        //
        // It sits deliberately ABOVE the diary's own ORACLE_PATIENCE (120s) so
        // that the diary is the authority on giving up. At 90s it was the other
        // way round: a model that thought silently for 95s got killed here and
        // surfaced as a bare "request failed" instead of the friendlier timeout
        // excuse, and the 120s state was unreachable over HTTP silence.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(150))
            .build();
        eprintln!(
            "riddle: http oracle base={base} model={model} max_tokens={max_tokens} reasoning={}",
            reasoning.as_deref().unwrap_or("-")
        );
        Ok(Self { base, key, model, max_tokens, reasoning, remember, agent })
    }

    pub fn ask(&self, png_path: &str, ctx: &TurnContext, tx: Sender<Result<Event, String>>) {
        let img = match std::fs::read(png_path) {
            Ok(b) => base64(&b),
            Err(e) => {
                let _ = tx.send(Err(format!("read image: {e}")));
                return;
            }
        };
        let (base, key, model) = (self.base.clone(), self.key.clone(), self.model.clone());
        // Shares the pool with every other turn: the second page onward skips
        // the handshake entirely and pays only the model's own latency.
        let agent = self.agent.clone();
        let max_tokens = self.max_tokens;
        let reasoning_field = self
            .reasoning
            .as_deref()
            .map(|r| format!("\"reasoning_effort\":{},", json_quote(r)))
            .unwrap_or_default();

        // Annotation mode drops the memory protocol with the persona: the ⁂
        // transcription and the catalog belong to a diary that answers in
        // words, and asking for both modalities at once muddies the op block.
        let system = build_system(ctx.mode, self.remember);
        // The diary's conversational memory: recent pages as prior turns.
        //
        // Annotation mode used to be excluded, on the reasoning that replaying
        // the prose stored with a mark teaches the model to produce more prose.
        // But that also left the marking modality with no memory of the
        // conversation at all — so a game played in the margins could not be
        // continued, because nothing carried the previous move forward. The
        // live scene in `prior_ops` says where the marks ARE; the history says
        // what the two of you were doing. Both are needed.
        let mut history_msgs = String::new();
        for (t, r) in ctx.history.iter() {
            history_msgs.push_str(&format!(
                "{{\"role\":\"user\",\"content\":{}}},{{\"role\":\"assistant\",\"content\":{}}},",
                json_quote(&format!("(an earlier page) {t}")),
                json_quote(r),
            ));
        }
        let user_text = turn_text(ctx);
        let catalog_ids = ctx.catalog_ids.clone();

        thread::spawn(move || {
            // OpenAI chat-completions with a data-URI image part, streaming.
            // The token-cap field is provider-dependent: OpenAI's newest
            // models and Cerebras reject "max_tokens" and want
            // "max_completion_tokens", while many OpenAI-compatible servers
            // only know "max_tokens" (Groq takes either). Send the
            // widely-supported name first; retry once if it is refused.
            let request = |cap_field: &str| {
                let body = format!(
                    concat!(
                        "{{\"model\":{},\"stream\":true,\"{}\":{},{}",
                        "\"messages\":[",
                        "{{\"role\":\"system\",\"content\":{}}},",
                        "{}",
                        "{{\"role\":\"user\",\"content\":[",
                        "{{\"type\":\"text\",\"text\":{}}},",
                        "{{\"type\":\"image_url\",\"image_url\":{{\"url\":\"data:image/png;base64,{}\"}}}}",
                        "]}}]}}"
                    ),
                    json_quote(&model),
                    cap_field,
                    max_tokens,
                    reasoning_field,
                    json_quote(&system),
                    history_msgs,
                    json_quote(&user_text),
                    img,
                );
                agent
                    .post(&format!("{base}/chat/completions"))
                    .set("Authorization", &format!("Bearer {key}"))
                    .set("Content-Type", "application/json")
                    .send_string(&body)
            };

            let asked = std::time::Instant::now();

            // A tablet that sleeps over Wi-Fi hands us dead pooled TLS
            // sessions, a radio that has not finished associating, and DNS
            // that is not up yet — all of which arrive as transport errors
            // indistinguishable from "the oracle refused you". Retrying is
            // safe here and only here: nothing has been emitted, so the turn
            // is still idempotent. Never retry after content has streamed —
            // that would ink the reply twice.
            let mut attempt: u32 = 0;
            let resp = loop {
                let outcome = match request("max_tokens") {
                    Err(ureq::Error::Status(400, r)) => {
                        let detail = r.into_string().unwrap_or_default();
                        if rejects_max_tokens(&detail) {
                            eprintln!("riddle: endpoint wants max_completion_tokens; retrying");
                            request("max_completion_tokens")
                        } else {
                            let _ = tx.send(Err(format!("http 400: {}", detail.trim())));
                            return;
                        }
                    }
                    other => other,
                };
                match outcome {
                    Err(ref e) if attempt < TRANSPORT_RETRIES && retryable(e) => {
                        let wait = Duration::from_millis(700 << attempt);
                        eprintln!(
                            "riddle: oracle unreachable ({e}); retry {} of {TRANSPORT_RETRIES} in {}ms",
                            attempt + 1,
                            wait.as_millis()
                        );
                        thread::sleep(wait);
                        attempt += 1;
                        continue;
                    }
                    other => break other,
                }
            };

            let reader = match resp {
                Ok(r) => r.into_reader(),
                Err(ureq::Error::Status(code, r)) => {
                    let detail = r.into_string().unwrap_or_default();
                    let _ = tx.send(Err(format!("http {code}: {}", detail.trim())));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("request failed: {e}")));
                    return;
                }
            };

            // Parse the SSE stream: lines of `data: {json}` whose delta.content
            // fragments accumulate; the parser turns the running text into
            // events (route directive, sentences, transcription postscript).
            let mut parser = StreamParser::new(catalog_ids);
            let mut acc = String::new();
            let mut first = true;
            // Returns false once the diary has hung up. A failed send is the
            // only cancellation signal we get: the main loop drops the
            // receiver when ORACLE_PATIENCE expires, and without noticing
            // that, this thread reads a trickling stream forever — holding a
            // megabyte or two of image and text on a device with no swap, and
            // letting the provider keep generating (and billing) into a void.
            let mut emit = |events: Vec<Result<Event, String>>| -> bool {
                for ev in events {
                    if first {
                        eprintln!("riddle: oracle first chunk +{}ms", asked.elapsed().as_millis());
                        first = false;
                    }
                    if tx.send(ev).is_err() {
                        return false;
                    }
                }
                true
            };

            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            let mut listening = true;
            let mut saw_done = false;
            let mut truncated = false;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        // A read timeout, reset or TLS error mid-stream is NOT
                        // a finished reply. The old code read through
                        // `map_while(Result::ok)`, which swallowed the error
                        // and then told the parser the stream had completed —
                        // so half a sentence got inked and committed to memory
                        // as the model's final answer.
                        let _ = tx.send(Err(format!("stream broke: {e}")));
                        return;
                    }
                }
                if asked.elapsed() > TURN_HARD_LIMIT {
                    eprintln!(
                        "riddle: oracle turn exceeded {}s; abandoning it",
                        TURN_HARD_LIMIT.as_secs()
                    );
                    let _ = tx.send(Err("timed out".into()));
                    return;
                }
                let trimmed = line.trim();
                let Some(data) = trimmed.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    saw_done = true;
                    break;
                }
                // "length" means the model was cut off mid-thought by the token
                // cap. Worth its own message: the page was read fine, and the
                // fix is a bigger cap or less thinking — nothing like the "it
                // said nothing" this used to be reported as.
                if json_str_field(data, "finish_reason").as_deref() == Some("length") {
                    truncated = true;
                }
                if let Some(frag) = sse_delta_content(data) {
                    if frag.is_empty() {
                        continue;
                    }
                    acc.push_str(&frag);
                    if !emit(parser.advance(&acc, false)) {
                        listening = false;
                        break;
                    }
                }
            }

            if !listening {
                // Dropping `reader` closes the connection, which is what
                // actually cancels generation upstream.
                eprintln!("riddle: diary stopped listening; abandoning oracle turn");
                return;
            }
            // What the model actually said, before any parsing. The parser is
            // deliberately strict — an unparseable reply becomes a terse
            // "empty reply" — and without the raw text there is no way to tell
            // a model that ignored the protocol from a bug in the protocol.
            if std::env::var_os("RIDDLE_ORACLE_RAW").is_some() {
                eprintln!("riddle: --- raw reply ---\n{acc}\n--- end raw ---");
            }
            if !saw_done {
                // Not fatal — plenty of OpenAI-compatible servers just close
                // the stream — but worth seeing in the log when a reply looks
                // clipped.
                eprintln!("riddle: oracle stream ended without [DONE]");
            }
            let tail = parser.advance(&acc, true);
            // Replace the parser's generic "empty reply" when we know exactly
            // why nothing came out.
            if truncated && tail.iter().any(|e| matches!(e, Err(m) if m == "empty reply")) {
                eprintln!("riddle: the model hit the token cap before answering");
                emit(vec![Err("out of room".into())]);
            } else {
                emit(tail);
            }
            // tx drops here → the diary's receiver disconnects = reply complete.
        });
    }
}

/// Does this 400 body blame the `max_tokens` field? Two shapes in the wild:
/// OpenAI names the replacement ("use 'max_completion_tokens' instead"), while
/// a provider that simply dropped the old name reports only the offending
/// field — Cerebras answers `"param":"max_tokens","message":"Extra inputs are
/// not permitted"`, which never mentions the new name and so used to surface
/// as a bare `http 400` instead of retrying. Either shape has the same fix.
fn rejects_max_tokens(detail: &str) -> bool {
    detail.contains("max_completion_tokens") || detail.contains("max_tokens")
}

/// Whether a failed request is worth sending again. Transport errors are the
/// tablet's normal post-wake state rather than a refusal; 408/429/5xx are the
/// server asking us to come back. Everything else (401, 404, a real 400) would
/// fail identically on a second try.
fn retryable(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Transport(_) => true,
        ureq::Error::Status(code, _) => *code == 408 || *code == 429 || *code >= 500,
    }
}

/// Pull `choices[0].delta.content` out of one SSE `data:` JSON object.
fn sse_delta_content(s: &str) -> Option<String> {
    // The delta object is small and well-formed; find the content string after
    // the `"delta":` marker so we don't match a `content` elsewhere.
    let d = s.find("\"delta\"")?;
    json_str_field(&s[d..], "content")
}

/// Trim and strip stray surrounding quotes from a reply fragment.
fn clean(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.to_string()
}

/// Remove any ⟦…⟧ directive spans from inked prose, so a misbehaving model
/// that emits a directive mid/after prose never renders ⟦…⟧ as literal glyphs
/// in the reply hand. (A directive that LEADS the reply is routed earlier.)
///
/// This eats an unterminated tail too, which would silently swallow a draw
/// block still streaming in — `advance` never hands one here: a block is
/// extracted, and prose is cut at the opening bracket, before this runs.
fn strip_directives(s: &str) -> String {
    if !s.contains(SHOW_OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find(SHOW_OPEN) {
        out.push_str(&rest[..open]);
        match rest[open..].find(SHOW_CLOSE) {
            Some(close) => rest = &rest[open + close + SHOW_CLOSE.len_utf8()..],
            None => {
                rest = ""; // unterminated: drop the tail
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte offset within `rest` (the text just past a ⟦) at which a draw block's
/// body begins, or None if this bracket does not open one. The tag must be the
/// whole word — `⟦drawing⟧` is prose someone bracketed, not a block.
fn draw_body_start(rest: &str) -> Option<usize> {
    if !rest.get(..DRAW_TAG.len())?.eq_ignore_ascii_case(DRAW_TAG) {
        return None;
    }
    match rest[DRAW_TAG.len()..].chars().next() {
        None => Some(DRAW_TAG.len()),
        Some(c) if c.is_whitespace() || c == SHOW_CLOSE => Some(DRAW_TAG.len()),
        Some(_) => None,
    }
}

/// Byte offset of the first ⟦ in `s` at which prose delivery must stop: one
/// that opens a draw block, or — while the stream is still running — one whose
/// tag has not arrived in full. `⟦dr` is indistinguishable from `⟦show:` until
/// the next byte lands, and inking it would consume the block's opening.
fn hold_at(s: &str, done: bool) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find(SHOW_OPEN) {
        let at = from + rel;
        let rest = &s[at + SHOW_OPEN.len_utf8()..];
        if draw_body_start(rest).is_some() {
            return Some(at);
        }
        if !done && rest.len() < DRAW_TAG.len() && DRAW_TAG.starts_with(&rest.to_ascii_lowercase())
        {
            return Some(at);
        }
        from = at + SHOW_OPEN.len_utf8();
    }
    None
}

/// Drop `<think>…</think>` spans from the reply text.
///
/// Groq's only vision model reasons by default and their default
/// reasoning_format is "raw", so the chain-of-thought arrives inline in the
/// ordinary `content` deltas rather than in `reasoning_content` (which
/// `sse_delta_content` already ignores) — the diary inked pages of it. An OPEN
/// tag swallows everything after it: the thinking is still streaming, and
/// there is no un-inking once a sentence is drawn. The trailing-fragment trim
/// covers the same hazard one byte earlier, while `<thi` is still arriving.
fn strip_think(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains(THINK_OPEN) {
        return match partial_open(s) {
            Some(p) => std::borrow::Cow::Owned(s[..p].to_string()),
            None => std::borrow::Cow::Borrowed(s),
        };
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find(THINK_OPEN) {
        out.push_str(&rest[..open]);
        match rest[open..].find(THINK_CLOSE) {
            Some(close) => rest = &rest[open + close + THINK_CLOSE.len()..],
            None => {
                rest = ""; // still thinking: hold everything after the tag
                break;
            }
        }
    }
    out.push_str(rest);
    if let Some(p) = partial_open(&out) {
        out.truncate(p);
    }
    std::borrow::Cow::Owned(out)
}

/// Offset of a trailing fragment of a `<think>` tag ("<", "<th", …), if any.
fn partial_open(s: &str) -> Option<usize> {
    (1..THINK_OPEN.len())
        .rev()
        .find(|&n| s.ends_with(&THINK_OPEN[..n]))
        .map(|n| s.len() - n)
}

/// End of the LAST complete sentence in `text` after byte offset `from`:
/// sentence punctuation followed by whitespace or end-of-text. Returns the
/// offset just past the punctuation, or None if no sentence has completed.
/// Chunks shorter than a few characters are not worth an early delivery.
fn sentence_cut(text: &str, from: usize) -> Option<usize> {
    let tail = text.get(from..)?;
    let mut cut = None;
    for (i, c) in tail.char_indices() {
        if matches!(c, '.' | '!' | '?' | '…') {
            let end = i + c.len_utf8();
            if tail[end..].chars().next().is_none_or(char::is_whitespace) && end >= 4 {
                cut = Some(from + end);
            }
        }
    }
    cut
}

/// Extract a top-level string field's value (first match; unescaped).
///
/// Tolerates whitespace around the colon. OpenAI and most gateways emit
/// compact SSE frames (`"content":"…"`), but a server that serializes with
/// spaces (`"content": "…"`) is still valid JSON and used to silently yield an
/// empty reply here — verified against a mock gateway. Non-string values
/// (`"content": null`, which every provider sends on the role-only first
/// frame) still return None.
fn json_str_field(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut from = 0;
    let start = loop {
        let hit = from + s[from..].find(&pat)?;
        let after = &s[hit + pat.len()..];
        let after = after.trim_start();
        // The value must be a string for this helper to apply; a non-string
        // (`null` on the role-only frame) means "keep looking".
        if let Some(v) = after
            .strip_prefix(':')
            .map(str::trim_start)
            .and_then(|v| v.strip_prefix('"'))
        {
            break s.len() - v.len();
        }
        from = hit + pat.len();
    };
    let rest = &s[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    match n {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        // \uXXXX — needed for accented replies (French, em-dash…).
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                out.push(ch);
                            }
                        }
                        other => out.push(other),
                    }
                }
            }
            '"' => break,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Pull the assistant reply text out of an event line. The event carries a
/// `message` object with `"role":"assistant"` and `content:[{type:text,text:…}]`.
/// We only trust text that belongs to an assistant message (the user echo also
/// contains a "text" field, which we must NOT return).
fn extract_assistant_text(s: &str) -> Option<String> {
    // Require this line to be an assistant message.
    if !s.contains("\"role\":\"assistant\"") {
        return None;
    }
    // Collect every "text":"…" occurrence inside the FIRST assistant section
    // only. message_update lines carry the running text twice (in
    // assistantMessageEvent.partial AND a top-level message); reading past the
    // next role marker would double every streamed chunk.
    let role_pos = s.find("\"role\":\"assistant\"")?;
    let after = &s[role_pos + "\"role\":\"assistant\"".len()..];
    let tail = match after.find("\"role\":\"") {
        Some(p) => &after[..p],
        None => after,
    };
    let mut out = String::new();
    let mut idx = 0;
    let needle = "\"text\":\"";
    while let Some(rel) = tail[idx..].find(needle) {
        let start = idx + rel + needle.len();
        // Decode the JSON string starting at `start`.
        let mut chars = tail[start..].chars();
        let mut piece = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        piece.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '"' => '"',
                            '\\' => '\\',
                            '/' => '/',
                            other => other,
                        });
                    }
                }
                '"' => break,
                _ => piece.push(c),
            }
        }
        out.push_str(&piece);
        // Advance past this occurrence.
        idx = start;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // RFC 8259 forbids raw controls in strings. Model transcripts can
            // carry tabs/CRs (the SSE + pi decoders un-escape \t \r \uXXXX),
            // and one such char stored in memory would poison every later
            // request's JSON. Escape the whole C0 range defensively.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_delta_extraction() {
        let line = r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#;
        assert_eq!(sse_delta_content(line).as_deref(), Some("Hello"));
        // role-only delta (first SSE frame) has no content.
        let role = r#"{"choices":[{"delta":{"role":"assistant"},"index":0}]}"#;
        assert_eq!(sse_delta_content(role), None);
    }

    #[test]
    fn sse_tolerates_pretty_printed_frames() {
        // A gateway that serializes with spaces is still valid SSE JSON.
        let spaced = r#"{"choices": [{"delta": {"content": "Hello"}, "index": 0}]}"#;
        assert_eq!(sse_delta_content(spaced).as_deref(), Some("Hello"));
        // null content (role-only frame) must not be mistaken for a string.
        let null = r#"{"choices":[{"delta":{"role":"assistant","content":null}}]}"#;
        assert_eq!(sse_delta_content(null), None);
        // A key that merely ENDS in `content` must not match.
        let reasoning = r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#;
        assert_eq!(sse_delta_content(reasoning), None);
    }

    #[test]
    fn sse_decodes_unicode_and_escapes() {
        // OpenAI escapes accents and em-dashes; the diary answers in French.
        let line = r#"{"choices":[{"delta":{"content":"Déjà vu — oui"}}]}"#;
        assert_eq!(sse_delta_content(line).as_deref(), Some("Déjà vu — oui"));
        let nl = r#"{"choices":[{"delta":{"content":"line\nbreak"}}]}"#;
        assert_eq!(sse_delta_content(nl).as_deref(), Some("line\nbreak"));
    }

    #[test]
    fn max_tokens_rejection_triggers_the_retry() {
        // OpenAI: the message names the replacement field.
        assert!(rejects_max_tokens(
            r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}}"#
        ));
        // Cerebras: the field was renamed away, so only the offender is named.
        assert!(rejects_max_tokens(
            r#"{"message":"Extra inputs are not permitted","type":"invalid_request_error","param":"max_tokens","code":"wrong_api_format"}"#
        ));
        // An unrelated 400 must not cost a pointless second request.
        assert!(!rejects_max_tokens(
            r#"{"error":{"message":"model `nope` does not exist"}}"#
        ));
    }

    #[test]
    fn base64_matches_known_vector() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn clean_strips_wrapping_quotes() {
        assert_eq!(clean("  \"hello\"  "), "hello");
        assert_eq!(clean("plain"), "plain");
    }

    fn drain(events: Vec<Result<Event, String>>) -> Vec<Event> {
        events.into_iter().map(|e| e.unwrap()).collect()
    }

    #[test]
    fn parser_streams_prose_then_transcript() {
        let mut p = StreamParser::new(vec![]);
        assert!(p.advance("Hello", false).is_empty());
        let ev = drain(p.advance("Hello. Who wri", false));
        assert_eq!(ev, vec![Event::Ink("Hello.".into())]);
        let full = "Hello. Who writes to me? \u{2042} it rained all night";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![
                Event::Ink("Who writes to me?".into()),
                Event::Transcript("it rained all night".into())
            ]
        );
    }

    #[test]
    fn parser_routes_show_directive() {
        let mut p = StreamParser::new(vec![900, 800, 700]);
        // Directive still streaming in: no decision yet.
        assert!(p.advance("\u{27e6}sho", false).is_empty());
        let ev = drain(p.advance("\u{27e6}show:2\u{27e7}", false));
        assert_eq!(ev, vec![Event::Show(800)]);
        let full = "\u{27e6}show:2\u{27e7}\n\u{2042} show me the garden page";
        let ev = drain(p.advance(full, true));
        assert_eq!(ev, vec![Event::Transcript("show me the garden page".into())]);
    }

    #[test]
    fn parser_show_tolerates_spacing_and_case() {
        let mut p = StreamParser::new(vec![42]);
        let ev = drain(p.advance("  \u{27e6}Show: 1\u{27e7}", true));
        assert!(ev.contains(&Event::Show(42)), "{ev:?}");
    }

    #[test]
    fn parser_show_out_of_range_is_error() {
        let mut p = StreamParser::new(vec![42]);
        let ev = p.advance("\u{27e6}show:7\u{27e7}", true);
        assert!(ev[0].is_err());
    }

    #[test]
    fn parser_empty_reply_is_error() {
        let mut p = StreamParser::new(vec![]);
        let ev = p.advance("", true);
        assert!(ev[0].is_err());
    }

    #[test]
    fn parser_without_sentinel_still_flushes() {
        // Memory off (or model forgot the postscript): plain prose still works.
        let mut p = StreamParser::new(vec![]);
        let ev = drain(p.advance("A reply without postscript", true));
        assert_eq!(ev, vec![Event::Ink("A reply without postscript".into())]);
    }

    #[test]
    fn parser_leading_directive_conjures_and_takes_the_whole_body() {
        let mut p = StreamParser::new(vec![900, 800]);
        let full = "\u{27e6}show:2\u{27e7}\n\u{2042} show me the rain";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![Event::Show(800), Event::Transcript("show me the rain".into())]
        );
    }

    #[test]
    fn parser_directive_after_prose_is_stripped_not_inked() {
        // A misbehaving model prefaces the directive with prose. We don't
        // honor it (that would need un-inking), but we must NOT render the
        // ⟦…⟧ as literal glyphs — strip it from the inked text.
        let mut p = StreamParser::new(vec![900, 800]);
        let full = "Of course, let me show you. \u{27e6}show:2\u{27e7}\n\u{2042} show me the rain";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![
                Event::Ink("Of course, let me show you.".into()),
                Event::Transcript("show me the rain".into())
            ]
        );
        // The show glyphs never reached the writer.
        assert!(!ev.iter().any(|e| matches!(e, Event::Ink(s) if s.contains('\u{27e6}'))));
    }

    #[test]
    fn strip_directives_removes_spans() {
        assert_eq!(strip_directives("a \u{27e6}show:1\u{27e7} b"), "a b");
        assert_eq!(strip_directives("plain text"), "plain text");
        assert_eq!(strip_directives("tail \u{27e6}show:2"), "tail");
    }

    /// The natural annotation reply: a line of prose, then the marks. Unlike
    /// ⟦show:N⟧ the block is honoured even though prose came first.
    #[test]
    fn draw_block_after_prose_emits_ink_then_draw() {
        use crate::ops::Op;
        let mut p = StreamParser::new(vec![]);
        let full = "I see what you mean. \u{27e6}draw\ncircle 400 300 60\ntick 700 500\n\u{27e7}";
        let ev = drain(p.advance(full, true));
        assert_eq!(
            ev,
            vec![
                Event::Ink("I see what you mean.".into()),
                Event::Draw(vec![
                    Op::Circle { cx: 400, cy: 300, r: 60 },
                    Op::Tick { x: 700, y: 500 },
                ]),
            ]
        );
    }

    /// A block may also lead the reply, where the ⟦show:N⟧ router would have
    /// tried to read "draw" as a catalog number.
    #[test]
    fn leading_draw_block_is_not_a_conjuring_directive() {
        use crate::ops::Op;
        let mut p = StreamParser::new(vec![900]);
        let ev = drain(p.advance("\u{27e6}draw\nunderline 100 200 400 200\n\u{27e7}", true));
        assert_eq!(
            ev,
            vec![Event::Draw(vec![Op::Underline { x1: 100, y1: 200, x2: 400, y2: 200 }])]
        );
    }

    /// The fiddly one: a block arriving a few bytes at a time must render once,
    /// whole, and never half. Half a mark cannot be un-drawn.
    #[test]
    fn fragmented_draw_block_emits_once_and_never_partial() {
        use crate::ops::Op;
        let mut p = StreamParser::new(vec![]);
        let mut all = Vec::new();
        // Prose ahead of the block still goes out; the bracket holds the rest.
        all.extend(drain(p.advance("Yes. \u{27e6}", false)));
        all.extend(drain(p.advance("Yes. \u{27e6}dr", false)));
        all.extend(drain(p.advance("Yes. \u{27e6}draw\ncir", false)));
        all.extend(drain(p.advance("Yes. \u{27e6}draw\ncircle 500 500 100\nbox 10 10", false)));
        assert_eq!(all, vec![Event::Ink("Yes.".into())], "a partial block leaked");
        let full = "Yes. \u{27e6}draw\ncircle 500 500 100\nbox 10 10 200 100\n\u{27e7}";
        all.extend(drain(p.advance(full, false)));
        // Repeated feeds of the settled text must not draw it twice.
        all.extend(drain(p.advance(full, true)));
        assert_eq!(
            all,
            vec![
                Event::Ink("Yes.".into()),
                Event::Draw(vec![
                    Op::Circle { cx: 500, cy: 500, r: 100 },
                    Op::Box { x: 10, y: 10, w: 200, h: 100 },
                ]),
            ]
        );
    }

    #[test]
    fn unterminated_draw_block_is_dropped_at_done() {
        let mut p = StreamParser::new(vec![]);
        assert!(p.advance("Here. \u{27e6}draw\ncircle 500 500 100", false).len() == 1);
        let ev = p.advance("Here. \u{27e6}draw\ncircle 500 500 100", true);
        // The prose already went out; the block is gone, not half-drawn.
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn malformed_lines_do_not_cost_the_good_ops() {
        use crate::ops::Op;
        let mut p = StreamParser::new(vec![]);
        let full = "\u{27e6}draw\nswirl 1 2\ncircle 400 300 60\narrow 10 20\n\u{27e7}";
        let ev = drain(p.advance(full, true));
        assert_eq!(ev, vec![Event::Draw(vec![Op::Circle { cx: 400, cy: 300, r: 60 }])]);
    }

    #[test]
    fn draw_block_with_no_usable_ops_emits_nothing() {
        let mut p = StreamParser::new(vec![]);
        let ev = p.advance("Nothing to mark. \u{27e6}draw\n\u{27e7}", true);
        assert_eq!(drain(ev), vec![Event::Ink("Nothing to mark.".into())]);
    }

    #[test]
    fn think_span_is_never_inked() {
        // Groq's vision model with reasoning_format=raw puts the whole chain of
        // thought in the content deltas.
        let mut p = StreamParser::new(vec![]);
        let full = "<think>The writer asks about rain. I should be brief.</think>The ink blurred.";
        let ev = drain(p.advance(full, true));
        assert_eq!(ev, vec![Event::Ink("The ink blurred.".into())]);
    }

    #[test]
    fn open_think_tag_holds_everything_after_it() {
        let mut p = StreamParser::new(vec![]);
        // Mid-thought: nothing has been decided yet, so nothing is inked.
        assert!(p.advance("<thi", false).is_empty());
        assert!(p.advance("<think>She writes of rain. Perhaps.", false).is_empty());
        let ev = drain(p.advance("<think>She writes of rain. Perhaps.</think>Rain again?", true));
        assert_eq!(ev, vec![Event::Ink("Rain again?".into())]);
    }

    #[test]
    fn think_tag_after_prose_does_not_swallow_the_prose() {
        let mut p = StreamParser::new(vec![]);
        let ev = drain(p.advance("Good evening. <think>now what</think> Who writes?", true));
        let Some(Event::Ink(s)) = ev.first() else { panic!("{ev:?}") };
        assert!(s.starts_with("Good evening.") && s.ends_with("Who writes?"), "{s}");
        assert!(!s.contains("now what") && !s.contains("think"), "{s}");
    }

    #[test]
    fn json_quote_escapes_control_chars() {
        // A tabbed, multiline transcript must not produce raw C0 bytes.
        let q = json_quote("a\tb\r\nc\u{0007}d");
        assert_eq!(q, "\"a\\tb\\r\\nc\\u0007d\"");
        assert!(!q.chars().any(|c| (c as u32) < 0x20));
    }

    /// The whole point of the merge: one collaborator, two channels. Whatever
    /// the mode, the writer is talking to the same person — only what a reply
    /// is physically able to be changes.
    #[test]
    fn both_modes_share_one_persona() {
        let drink = build_system(PageMode::Drink, false);
        let keep = build_system(PageMode::Keep, false);
        assert!(drink.starts_with(COLLABORATOR));
        assert!(keep.starts_with(COLLABORATOR));
        // And no trace of the character that used to replace it.
        for s in [&drink, &keep] {
            assert!(!s.contains("Riddle"), "the diary persona is back");
            assert!(!s.contains("quill"));
        }
    }

    /// Each mode teaches only the channel it can actually use. Marking
    /// instructions in Drink mode would produce ops pointing at ink that is
    /// being erased; prose-length rules in Keep mode invite prose that is
    /// discarded.
    #[test]
    fn each_mode_teaches_only_its_own_channel() {
        let drink = build_system(PageMode::Drink, false);
        let keep = build_system(PageMode::Keep, false);
        assert!(keep.contains("circle cx cy r"), "keep mode must teach the ops");
        assert!(!drink.contains("circle cx cy r"), "drink mode must not");
        assert!(drink.contains("THIS PAGE WILL CLEAR"));
        assert!(keep.contains("THIS PAGE IS KEPT"));
    }

    /// The memory protocol conjures archived pages back, which only means
    /// something where pages are archived — the mode that clears them. In Keep
    /// mode its transcription postscript would also fight the draw block.
    #[test]
    fn memory_protocol_rides_only_with_drink() {
        assert!(build_system(PageMode::Drink, true).contains("\u{2042}"));
        assert!(!build_system(PageMode::Keep, true).contains("\u{2042}"));
        assert!(!build_system(PageMode::Drink, false).contains("\u{2042}"));
    }

    /// Every op the prompt advertises must be one the parser accepts, and vice
    /// versa. This drifted before: the prompt said "eight ops" while listing
    /// five, and forbade three that `ops.rs` had always parsed.
    #[test]
    fn the_prompt_teaches_exactly_what_the_parser_accepts() {
        let keep = build_system(PageMode::Keep, false);
        for op in ["circle", "box", "arrow", "underline", "strike", "tick", "cross", "note"] {
            assert!(keep.contains(&format!("\n{op} ")), "op {op} is never taught");
        }
        assert!(keep.contains("These eight ops"));
    }

    /// The turn text has to say which mode it is, because the model is given
    /// the same persona either way and nothing else tells it.
    #[test]
    fn the_turn_states_its_mode() {
        let keep = turn_text(&TurnContext { mode: PageMode::Keep, ..Default::default() });
        let drink = turn_text(&TurnContext { mode: PageMode::Drink, ..Default::default() });
        assert!(keep.contains("stays as it is"));
        assert!(drink.contains("cleared"));
    }

    /// A model that spends its whole budget thinking used to surface as
    /// "the model returned nothing", which sent the writer looking for a fault
    /// in their page instead of at the token cap. The cap is the cause and it
    /// is a setting, so it has to be named.
    #[test]
    fn a_truncated_reply_is_not_reported_as_silence() {
        let frame = r#"{"choices":[{"delta":{"content":""},"finish_reason":"length"}]}"#;
        assert_eq!(json_str_field(frame, "finish_reason").as_deref(), Some("length"));

        // All budget spent inside <think>: nothing survives stripping, so the
        // parser reports an empty reply and the caller must relabel it.
        let mut p = StreamParser::new(Vec::new());
        let out = p.advance("<think>weighing it up, at lengthhh", true);
        assert!(
            out.iter().any(|e| matches!(e, Err(m) if m == "empty reply")),
            "expected the empty-reply error this relabels: {out:?}"
        );
    }

    /// The excuses must not name a persona that no longer exists.
    #[test]
    fn no_prompt_mentions_the_old_diary_voice() {
        for s in [
            build_system(PageMode::Drink, true),
            build_system(PageMode::Keep, false),
        ] {
            for banned in ["spirit", "Riddle", "quill", "enchanted"] {
                assert!(!s.contains(banned), "{banned:?} is still in a prompt");
            }
        }
    }
}
