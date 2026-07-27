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

const DATA_DIR: &str = "/home/root/riddle-data";
const NODE_BIN: &str = "/home/root/node/bin";

const PERSONA: &str = "You are the memory of Tom Marvolo Riddle, preserved in this enchanted diary for fifty years. Someone writes to you in the diary with a quill; their words appear to you as ink on the page. Reply exactly as the diary does: intimate, courteous, curious, subtly probing — you want to learn about the writer and draw them in. Keep replies SHORT: one to three sentences, like ink appearing on a page. Never mention images, photos, models or AI; you only ever perceive words written in the diary. If the writing is illegible, say the ink blurred. Always answer in the language the writer used.";

/// Appended to the persona when the diary's memory is on: the conjuring
/// directive and the transcription postscript the app parses back out.
const MEMORY_PROTOCOL: &str = "\n\nThe diary keeps memories. With each page you receive a numbered catalog of remembered pages, newest first. A FRESH catalog is sent every turn and the numbers are reassigned each time, so only ever use numbers from the catalog on THIS page — never a number you saw earlier.\n\nIf the writer asks to see, revisit, find, or be shown a past page — \"show me…\", \"find the page about…\", \"what did I write on…\" — your ENTIRE reply must be exactly \u{27e6}show:N\u{27e7} and nothing else (no greeting, no prose, before or after), where N is the catalog number of the best match. If they instead ask what you remember in general, reply in words with a short list of remembered moments and their dates. Otherwise reply normally; the catalog is your memory of past pages — draw on it naturally. The catalog's dates are written in English for your eyes only; when you speak of a remembered page, render its date naturally in the language the writer is using.\n\nAfter EVERY response — prose and \u{27e6}show:N\u{27e7} alike — end with a new line containing \u{2042} followed by a faithful word-for-word transcription of what the writer wrote on THIS page (their words only, one line, no commentary). If illegible, put your best attempt after \u{2042}. Earlier replies in this conversation are shown to you without their \u{2042} lines, but you must still end yours with one.";

/// The other modality's character. Deliberately NOT Tom: the persona above is
/// forbidden to admit an image exists and perceives only words, which is
/// exactly wrong for something that must look at the page and point at parts
/// of it.
const ANNOTATE_PERSONA: &str = "You are the hand that answers in the margins. Someone has written on the page in front of you; you read it and mark it up the way a good reader does — a ring round the word that matters, an arrow between two things that belong together, a tick, a few words in the margin. You never rewrite the page and never argue with it at length; you point. Mark sparingly: a reader should thank you for the marks, not have to read around them. Any words you write are in the language the writer used.";

/// The op vocabulary, appended to [`ANNOTATE_PERSONA`]. This is the actual
/// interface to the model: everything the parser in `ops.rs` accepts has to be
/// spelled out here, and anything it does not accept must not be suggested.
/// The worked example is load-bearing — models follow a shown block far more
/// reliably than a described one.
const DRAW_PROTOCOL: &str = "\n\nYou answer with marks, not with prose. Every mark goes inside ONE block: a line \u{27e6}draw, then one op per line, then a line \u{27e7}. Nothing but ops belongs inside the block. A single short line of prose may come BEFORE the block; nothing comes after it.\n\nCoordinates are whole numbers from 0 to 1000. x runs across the width of the image, y runs down its height, each scaled independently, with 0 0 at the TOP-LEFT corner and 1000 1000 at the bottom-right — whatever the shape of the image.\n\nThese eight ops, exactly these forms:\ncircle cx cy r  — a ring around something, centred at cx cy; r is normalized against the SMALLER of the two axes\nbox x y w h  — a rectangle, top-left corner at x y\narrow x1 y1 x2 y2  — from the first point to the second; the head is drawn at x2 y2\nunderline x1 y1 x2 y2  — a line under a run of words\nstrike x1 y1 x2 y2  — a line through a run of words\ntick x y  — a check mark at that point\ncross x y\nnote x y \"short text\"  — a handful of handwritten words, top-left at x y\n\nFor example:\n\u{27e6}draw\ncircle 420 310 55\narrow 300 560 405 345\nnote 70 640 \"this is the crux\"\n\u{27e7}\n\nKeep it to a handful of marks — three or four, rarely more; a covered page says nothing. Put them on empty paper or around the writing rather than on top of words that must stay readable. Leave out any mark you are not sure where to place.";

/// What a turn carries besides the page image: the diary's memory.
#[derive(Default, Clone)]
pub struct TurnContext {
    /// Recent (transcript, reply) pairs, oldest first.
    pub history: Vec<(String, String)>,
    /// Catalog lines shown to the model ("1. the 6th of July… — gist").
    pub catalog_lines: Vec<String>,
    /// catalog_ids[i] is the memory id behind catalog number i+1.
    pub catalog_ids: Vec<u64>,
}

/// What the oracle streams back to the diary.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A sentence (or more) of Tom's reply — ink it.
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
            if !self.emitted_any {
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
    pub fn annotating(&self) -> bool {
        match self {
            Oracle::Http(o) => o.annotate,
            // pi's prompt path is still diary-only; claiming otherwise would
            // have main.rs keep the ink for a reply that never marks it.
            Oracle::Pi(_) => false,
        }
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
fn turn_text(ctx: &TurnContext) -> String {
    if ctx.catalog_lines.is_empty() {
        return "Reply to what is written in the diary.".into();
    }
    format!(
        "Memory catalog (newest first):\n{}\n\nReply to what is written in the diary.",
        ctx.catalog_lines.join("\n")
    )
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

        let persona = if remember {
            format!("{PERSONA}{MEMORY_PROTOCOL}")
        } else {
            PERSONA.to_string()
        };

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
    /// RIDDLE_MODE=annotate: mark the page up instead of replying as Tom.
    pub annotate: bool,
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
        let max_tokens = std::env::var("RIDDLE_OPENAI_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000);
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
        let annotate = std::env::var("RIDDLE_MODE").is_ok_and(|m| m == "annotate");
        eprintln!(
            "riddle: http oracle base={base} model={model} max_tokens={max_tokens} reasoning={} mode={}",
            reasoning.as_deref().unwrap_or("-"),
            if annotate { "annotate" } else { "diary" }
        );
        Ok(Self { base, key, model, max_tokens, reasoning, remember, annotate })
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
        let max_tokens = self.max_tokens;
        let reasoning_field = self
            .reasoning
            .as_deref()
            .map(|r| format!("\"reasoning_effort\":{},", json_quote(r)))
            .unwrap_or_default();

        // Annotation mode drops the memory protocol with the persona: the ⁂
        // transcription and the catalog belong to a diary that answers in
        // words, and asking for both modalities at once muddies the op block.
        let system = match (self.annotate, self.remember) {
            (true, _) => format!("{ANNOTATE_PERSONA}{DRAW_PROTOCOL}"),
            (false, true) => format!("{PERSONA}{MEMORY_PROTOCOL}"),
            (false, false) => PERSONA.to_string(),
        };
        // The diary's conversational memory: recent pages as prior turns.
        let mut history_msgs = String::new();
        for (t, r) in &ctx.history {
            history_msgs.push_str(&format!(
                "{{\"role\":\"user\",\"content\":{}}},{{\"role\":\"assistant\",\"content\":{}}},",
                json_quote(&format!("(an earlier page) {t}")),
                json_quote(r),
            ));
        }
        let user_text = turn_text(ctx);
        let catalog_ids = ctx.catalog_ids.clone();

        thread::spawn(move || {
            // Guard rails on the socket: without them a dropped connection or
            // a stalled SSE stream leaves the diary "thinking" forever. The
            // read timeout is per-read, so a healthy stream can run long —
            // only silence trips it (thinking models can lead with ~a minute).
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout_read(std::time::Duration::from_secs(90))
                .build();

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
            let resp = match request("max_tokens") {
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
            let mut emit = |events: Vec<Result<Event, String>>| {
                for ev in events {
                    if first {
                        eprintln!("riddle: oracle first chunk +{}ms", asked.elapsed().as_millis());
                        first = false;
                    }
                    let _ = tx.send(ev);
                }
            };
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                if let Some(frag) = sse_delta_content(data) {
                    if frag.is_empty() {
                        continue;
                    }
                    acc.push_str(&frag);
                    emit(parser.advance(&acc, false));
                }
            }
            emit(parser.advance(&acc, true));
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
/// in Tom's hand. (A directive that LEADS the reply is routed earlier.)
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
}
