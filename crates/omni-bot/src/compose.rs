//! Composition of a reply into rich blocks.
//!
//! Layout (top to bottom):
//! 1. header line: `🤖 **model**` in small text (the owner's choice: the answer
//!    takes the space);
//! 2. reasoning: collapsible details titled with a small 💭 (while the model
//!    is thinking it, also what about: [`thinking_title`]) (open only
//!    while thinking, before any answer);
//! 3. tool calls: collapsible details;
//! 4. the model's answer (rendered Markdown, or raw Markdown in raw mode / fallback);
//! 5. sources: collapsible details;
//! 6. errors: collapsible details (verbatim upstream JSON inside);
//! 7. small-text footers: status and notes, then options, usage, cost, timing, provider.
//!
//! Unambiguous by structure, without visual marking: the first line of every
//! reply message is the bot's header (a small-text block), and everything else
//! the bot adds is a collapsible details block or small text — block types the
//! Markdown renderer never produces from model output. So any reader can tell exactly which part
//! is the model's answer; model output cannot pass for reasoning, errors,
//! notes or usage.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Citation, Part, ReasoningKind, ToolCall};
use omni_llm::LlmError;
use omni_markdown::rich::{self, RBlock, RItem, RText, RichLimits};
use omni_markdown::{EntityKind, Formatted, Mode};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Generating,
    Done,
    Failed,
    /// This attempt failed; another one starts after `in_secs`.
    Retrying {
        in_secs: u64,
    },
}

/// An earlier attempt of the same reply (failed or interrupted): shown folded
/// above the current attempt, in order, in the same messages.
#[derive(Debug, Clone)]
pub struct PriorAttempt {
    pub n: i64,
    pub parts: Vec<Part>,
    pub error: Option<LlmError>,
    pub interrupted: bool,
    pub usage: Option<Usage>,
    pub elapsed: Option<Duration>,
}

pub struct ComposeInput<'a> {
    pub model: &'a str,
    pub options: &'a str,
    pub parts: &'a [Part],
    pub status: Status,
    pub attempt: i64,
    pub max_attempts: u32,
    pub error: Option<&'a LlmError>,
    pub usage: Option<&'a Usage>,
    pub provider: Option<&'a str>,
    pub finish_reason: Option<&'a str>,
    pub native_finish_reason: Option<&'a str>,
    pub notes: &'a [String],
    pub elapsed: Duration,
    pub show_usage: bool,
    /// A cost above this (USD) is bold; 0: never.
    pub bold_cost: f64,
    /// Open the reasoning fold while the model is thinking (`render.reasoning_open`).
    pub reasoning_open: bool,
    /// One continuous answer is shown above its reasoning and tool calls
    /// (`render.answer_first`).
    pub answer_first: bool,
    pub mode: Mode,
    /// Show AI-generated content as raw Markdown (fallback after Telegram rejected the rendering).
    pub raw_ai: bool,
    /// Completed long-decode continuation hops (0 = single request).
    pub hops: u32,
    /// This attempt continues a finished answer (`/continue`).
    pub continuation: bool,
    /// Files attached by the bot (media keys), e.g. the original Markdown when
    /// Telegram altered the rendered text.
    pub attachments: &'a [String],
    /// What the request actually asks for (effort, user options expanded), from
    /// the request body; empty when no request was built.
    pub settings: &'a [String],
    /// Earlier attempts of this reply, oldest first.
    pub prior: &'a [PriorAttempt],
    /// Previews of images still being generated, shown after the output.
    pub previews: &'a [(omni_core::BlobRef, Option<(u32, u32)>)],
    /// How the reply was served where that differs from the request.
    pub served: &'a [String],
}

/// The model output of `parts` as Markdown, exactly (reasoning in HTML comments).
pub fn raw_markdown(parts: &[Part]) -> String {
    let mut md = String::new();
    for p in parts {
        match p {
            Part::Reasoning(r) => {
                if let Some(text) = &r.text {
                    let kind = if r.kind == ReasoningKind::Summary { "reasoning summary" } else { "reasoning" };
                    md.push_str(&format!("<!-- {kind} -->\n{text}\n<!-- end {kind} -->\n\n"));
                }
            }
            Part::Text { text, .. } => md.push_str(text),
            Part::Refusal { text } => md.push_str(&format!("<!-- refusal -->\n{text}\n")),
            _ => {}
        }
    }
    md
}

pub fn label(s: &str) -> RText {
    RText::bold(RText::plain(s))
}

/// The first line of every reply message, in small text.
fn header(model: &str, part: Option<usize>) -> RBlock {
    let mut t = vec![RText::plain("🤖 "), label(model)];
    if let Some(p) = part {
        t.push(RText::plain(format!(" · part {p}")));
    }
    RBlock::Footer(RText::concat(t))
}

fn fmt_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A bot text with `` `code` `` spans shown as code (ids, prefixes, options in
/// notices and error titles); unpaired backticks leave the text as it is.
pub fn with_code(text: &str) -> RText {
    let pieces: Vec<&str> = text.split('`').collect();
    if pieces.len() < 3 || pieces.len().is_multiple_of(2) {
        return RText::plain(text);
    }
    RText::concat(
        pieces
            .iter()
            .enumerate()
            .map(|(i, s)| if i % 2 == 1 { RText::fixed(RText::plain(*s)) } else { RText::plain(*s) })
            .collect(),
    )
}

pub fn fmt_cost(c: f64) -> String {
    if c == 0.0 {
        // Also -0.0, which is what an empty f64 sum gives.
        "$0".to_string()
    } else if c >= 1.0 {
        format!("${c:.2}")
    } else if c >= 0.01 {
        format!("${c:.3}")
    } else {
        format!("${c:.5}")
    }
}

/// A token count for statistics: exact below 1000, else in K / M / B with one
/// decimal below 10 and none above (`1.2K`, `45K`, `380K`, `1.2M`, `12M`), so a
/// large total reads at a glance; the exact count would not mean more.
pub fn fmt_tokens_compact(n: i64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let n = n as f64;
    let units = [("K", 1e3), ("M", 1e6), ("B", 1e9)];
    for (i, (unit, div)) in units.iter().enumerate() {
        let v = n / div;
        let shown = if v < 9.95 { format!("{v:.1}") } else { format!("{v:.0}") };
        // Rounding up to 1000 of a unit is the next unit (999,960 is 1M, not 1000K).
        if shown.parse::<f64>().unwrap_or(0.0) < 1000.0 || i == units.len() - 1 {
            return format!("{}{unit}", shown.strip_suffix(".0").unwrap_or(&shown));
        }
    }
    unreachable!()
}

fn ai_blocks(md: &str, raw: bool, base_depth: usize) -> Vec<RBlock> {
    if raw { rich::raw_markdown_blocks(md) } else { rich::markdown_to_blocks(md, base_depth, RichLimits::TELEGRAM) }
}

/// Whether a part shows up in the reply (something "after" a reasoning block).
fn visible(p: &Part) -> bool {
    match p {
        Part::Reasoning(r) => r.text.as_deref().is_some_and(|t| !t.is_empty()),
        Part::Text { text, citations } => !text.trim().is_empty() || !citations.is_empty(),
        Part::ToolCall(_) | Part::Refusal { .. } => true,
        Part::Image { generated, .. } | Part::File { generated, .. } => *generated,
        _ => false,
    }
}

/// The answer: text, generated images and files, a refusal.
fn is_answer(p: &Part) -> bool {
    match p {
        Part::Text { text, citations } => !text.trim().is_empty() || !citations.is_empty(),
        Part::Image { generated: true, thought, .. } => !thought,
        Part::File { generated: true, .. } | Part::Refusal { .. } => true,
        _ => false,
    }
}

/// How the model got there: reasoning, tool calls, drafts while thinking.
fn is_process(p: &Part) -> bool {
    match p {
        Part::Reasoning(r) => r.text.as_deref().is_some_and(|t| !t.is_empty()),
        Part::ToolCall(_) => true,
        Part::Image { generated: true, thought: true, .. } => true,
        _ => false,
    }
}

/// The parts with the answer first, when the answer is one continuous stretch
/// (text, images, videos, files) that reasoning or tool calls came before: the
/// answer, then everything else in its own order. `None` when there is nothing
/// to move (no answer yet, it comes first anyway, or answer and reasoning
/// alternate: then everything stays in the order it was produced).
fn answer_first(parts: &[Part]) -> Option<Vec<Part>> {
    let (mut stretches, mut in_answer, mut process_before) = (0, false, false);
    for p in parts {
        if is_answer(p) {
            if !in_answer {
                stretches += 1;
                in_answer = true;
            }
        } else if is_process(p) {
            in_answer = false;
            process_before |= stretches == 0;
        }
    }
    if stretches != 1 || !process_before {
        return None;
    }
    let (answer, rest): (Vec<&Part>, Vec<&Part>) = parts.iter().partition(|p| is_answer(p));
    Some(answer.into_iter().chain(rest).cloned().collect())
}

/// What a tool call did, in a few words: the query, URL or code it ran (from
/// its input, whatever the provider's shape), on one line.
fn tool_detail(t: &ToolCall) -> Option<String> {
    use serde_json::Value;
    fn find(v: &Value, depth: usize) -> Option<String> {
        let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let list = |v: &Value| {
            let l: Vec<String> = v.as_array()?.iter().filter_map(text).collect();
            (!l.is_empty()).then(|| l.join("; "))
        };
        match v {
            Value::String(s) => text(&Value::String(s.clone())),
            Value::Object(o) if depth < 3 => {
                // Web search actions that are not searches (OpenAI, xAI).
                let field = |k: &str| o.get(k).and_then(text);
                // A file action (Claude's text editor): `create /tmp/plot.py`.
                if let (Some(c), Some(p)) = (field("command"), field("path"))
                    && !c.contains(char::is_whitespace)
                {
                    return Some(format!("{c} {p}"));
                }
                match o.get("type").and_then(Value::as_str) {
                    Some("open_page") if let Some(u) = field("url") => return Some(format!("open {u}")),
                    Some("find_in_page" | "find") if let Some(p) = field("pattern") => {
                        return Some(match field("url") {
                            Some(u) => format!("find \"{p}\" in {u}"),
                            None => format!("find \"{p}\""),
                        });
                    }
                    _ => {}
                }
                for k in ["query", "queries", "url", "urls", "pattern", "code", "command", "commands", "prompt"] {
                    if let Some(x) = o.get(k)
                        && let Some(d) = text(x).or_else(|| list(x))
                    {
                        return Some(d);
                    }
                }
                // OpenAI-style `action` objects and other nesting.
                ["action", "input", "arguments"].iter().find_map(|k| o.get(*k).and_then(|x| find(x, depth + 1)))
            }
            _ => None,
        }
    }
    let d = find(t.input.as_ref()?, 0)?;
    let line = d.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if line.chars().count() > 60 { format!("{}…", line.chars().take(59).collect::<String>()) } else { line })
}

/// The title of a fold of tool calls: what they did, so it reads without
/// opening (`🔧 web_search: rust release`, `🔧 web_search ×2: a; b`, mixed
/// tools separated by ` · `), with any status other than completed.
fn tools_title(calls: &[&ToolCall]) -> String {
    // How it ended (an exit code, an error code, a URL status), else a status other than completed.
    let status = |t: &ToolCall| match (crate::toolview::outcome(t), t.status.as_deref()) {
        (Some(o), _) => format!(" ({o})"),
        (None, None) => String::new(),
        (None, Some(s))
            if matches!(
                s.to_ascii_lowercase().as_str(),
                "completed" | "complete" | "success" | "succeeded" | "done"
            ) =>
        {
            String::new()
        }
        (None, Some(s)) => format!(" ({s})"),
    };
    let same = calls.len() > 1 && calls.windows(2).all(|w| w[0].name == w[1].name);
    let mut title = if same {
        let details: Vec<String> = calls.iter().filter_map(|t| tool_detail(t)).collect();
        let mut s = format!("🔧 {} ×{}", calls[0].name, calls.len());
        let mut ends: Vec<String> = calls.iter().map(|t| status(t)).filter(|s| !s.is_empty()).collect();
        ends.dedup();
        s.push_str(&ends.concat());
        if !details.is_empty() {
            s.push_str(": ");
            s.push_str(&details.join("; "));
        }
        s
    } else {
        let each: Vec<String> = calls
            .iter()
            .map(|t| match tool_detail(t) {
                Some(d) => format!("{}{}: {d}", t.name, status(t)),
                None => format!("{}{}", t.name, status(t)),
            })
            .collect();
        format!("🔧 {}", each.join(" · "))
    };
    if title.chars().count() > 120 {
        title = format!("{}…", title.chars().take(119).collect::<String>());
    }
    title
}

fn tool_item(t: &ToolCall) -> RItem {
    let mut s = t.name.clone();
    if let Some(st) = &t.status {
        s.push_str(&format!(" [{st}]"));
    }
    let mut blocks = vec![RBlock::Paragraph(RText::bold(RText::plain(s)))];
    // Readable where the shape is known, else the JSON (see `toolview`).
    blocks.extend(crate::toolview::blocks(t));
    RItem { checkbox: None, blocks }
}

/// What reasoning is about, for its fold title while the model thinks: the
/// latest heading of a summary (a line that is all bold, `**Planning the
/// answer**`, or a Markdown heading), else the beginning of the latest paragraph.
fn thinking_title(text: &str) -> Option<String> {
    const MAX: usize = 48;
    let heading = text.lines().rev().find_map(|l| {
        let t = l.trim();
        let bold = t.strip_prefix("**").and_then(|x| x.strip_suffix("**")).filter(|x| !x.contains("**"));
        let hashes = t.trim_start_matches('#');
        let md = (hashes.len() < t.len() && hashes.starts_with(' ')).then_some(hashes);
        bold.or(md).map(str::trim).filter(|h| !h.is_empty())
    });
    let picked = heading.or_else(|| text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).last())?;
    let line = picked.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX - 1).collect::<String>())
    } else {
        line
    })
}

/// Emit the pending answer text, followed by the sources it cites: a fold of
/// the links (with the quoted passage when the provider gives one), cited
/// documents without a link (a PDF page), and Google's Search Suggestions as a
/// visible line (Google's terms: grounded answers are shown with them).
fn flush_answer(out: &mut Vec<RBlock>, answer: &mut String, citations: &mut Vec<Citation>, raw: bool, depth: usize) {
    if !answer.trim().is_empty() {
        out.extend(ai_blocks(answer, raw, depth));
    }
    answer.clear();
    let mut seen = std::collections::HashSet::new();
    let mut sources: Vec<Citation> = Vec::new();
    let mut suggestions: Vec<(String, String)> = Vec::new();
    for c in citations.drain(..) {
        if let Some(sg) = c.extra.as_ref().and_then(|e| e.get("searchSuggestion")) {
            let label = sg.get("text").and_then(|x| x.as_str()).map(str::to_string);
            if let (Some(label), Some(url)) = (label, c.url.clone())
                && seen.insert(format!("suggestion {url}"))
            {
                suggestions.push((label, url));
            }
            continue;
        }
        let key = match &c.url {
            Some(u) => u.clone(),
            None if c.title.is_some() || c.cited_text.is_some() => {
                format!("{}\u{0}{}", c.title.as_deref().unwrap_or(""), c.cited_text.as_deref().unwrap_or(""))
            }
            None => continue,
        };
        if seen.insert(key) {
            sources.push(c);
        }
    }
    if !sources.is_empty() {
        let items = sources
            .iter()
            .map(|c| {
                let mut line = match &c.url {
                    Some(url) => {
                        let title = c
                            .title
                            .as_deref()
                            .map(plain_title)
                            .filter(|t| !t.trim().is_empty())
                            .unwrap_or_else(|| url.clone());
                        if omni_markdown::render::is_safe_url(url) {
                            RText::Url { text: Box::new(RText::plain(title)), url: url.clone() }
                        } else {
                            RText::plain(title)
                        }
                    }
                    None => RText::plain(c.title.clone().unwrap_or_else(|| "document".into())),
                };
                if let Some(at) = citation_place(c) {
                    line = RText::Concat(vec![line, RText::plain(format!(" · {at}"))]);
                }
                let mut blocks = vec![RBlock::Paragraph(line)];
                if let Some(q) = c.cited_text.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
                    let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
                    let q = if q.chars().count() > 150 {
                        format!("{}…", q.chars().take(149).collect::<String>())
                    } else {
                        q
                    };
                    blocks.push(RBlock::Footer(RText::plain(format!("“{q}”"))));
                }
                RItem { checkbox: None, blocks }
            })
            .collect();
        out.push(RBlock::Details {
            open: false,
            title: RText::plain(format!("📚 Sources · {}", sources.len())),
            blocks: vec![RBlock::OrderedList { start: 1, items }],
        });
    }
    if !suggestions.is_empty() {
        let mut line = vec![RText::plain("Google Search: ")];
        for (i, (label, url)) in suggestions.into_iter().enumerate() {
            if i > 0 {
                line.push(RText::plain(" · "));
            }
            line.push(if omni_markdown::render::is_safe_url(&url) {
                RText::Url { text: Box::new(RText::plain(label)), url }
            } else {
                RText::plain(label)
            });
        }
        out.push(RBlock::Footer(RText::Concat(line)));
    }
}

/// A source title as text: some providers send markup in it (Google's image
/// search: `<b>Oriental Pearl Tower</b> - Wikipedia`, `Shanghai&#39;s`); the
/// raw title stays in `/dump`.
fn plain_title(t: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in t.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    if in_tag {
        // Not a tag after all (`a < b`): keep the text as it was.
        return t.to_string();
    }
    for (e, c) in [("&#39;", "'"), ("&quot;", "\""), ("&lt;", "<"), ("&gt;", ">"), ("&nbsp;", " "), ("&amp;", "&")] {
        out = out.replace(e, c);
    }
    out
}

/// Where in a cited document the passage is, when the provider says: a page
/// (`p. 3`, `pp. 3–4`) or another location it names.
fn citation_place(c: &Citation) -> Option<String> {
    let e = c.extra.as_ref()?;
    let n = |k: &str| e.get(k).and_then(|v| v.as_u64());
    match (n("start_page_number"), n("end_page_number")) {
        // Claude: the end page is exclusive.
        (Some(a), Some(b)) if b > a + 1 => return Some(format!("pp. {a}–{}", b - 1)),
        (Some(a), _) => return Some(format!("p. {a}")),
        _ => {}
    }
    if let Some(p) = n("page") {
        return Some(format!("p. {p}"));
    }
    e.get("location").and_then(|v| v.as_str()).map(str::to_string)
}

/// The model's output, strictly in the order it was produced: reasoning, tool
/// calls, answer text, generated images, refusals. Only adjacent parts of one
/// kind are joined into a block; nothing is moved past anything else. Sources
/// follow the answer text they belong to. `depth`: nesting of the output
/// (0 at the top of the message, 1 inside an earlier attempt's block).
/// A stretch of reasoning and tool calls that no answer text interrupts.
#[derive(Default)]
struct Stretch {
    blocks: Vec<RBlock>,
    reasoning: usize,
    tool_calls: usize,
    /// What the model is thinking about right now (the stretch ends with live reasoning).
    thinking: Option<String>,
}

impl Stretch {
    /// Its blocks, into `out`: one block as it is; several in one fold titled
    /// with what they are (owner's choice: "💭 3 · 🔧 2"), open if one inside is.
    fn flush(&mut self, out: &mut Vec<RBlock>) {
        let s = std::mem::take(self);
        if s.blocks.len() < 2 {
            out.extend(s.blocks);
            return;
        }
        let open = s.blocks.iter().any(|b| matches!(b, RBlock::Details { open: true, .. }));
        let mut title = Vec::new();
        if s.reasoning > 0 {
            title.push(format!("💭 {}", s.reasoning));
        }
        if s.tool_calls > 0 {
            title.push(format!("🔧 {}", s.tool_calls));
        }
        // The fold one sees: it also says what the model is thinking about (only meanwhile).
        title.extend(s.thinking);
        out.push(RBlock::Details { open, title: RText::sub(RText::plain(title.join(" · "))), blocks: s.blocks });
    }
}

/// `open_thinking`: open the reasoning fold that nothing follows yet (the model
/// is still thinking and `render.reasoning_open` is on). `live`: the reply is
/// being generated, so the reasoning nothing follows yet is the model's current
/// thinking: its fold title says what it is about ([`thinking_title`]); once
/// anything follows it (or the reply is done) the title is a plain 💭 again.
///
/// Strictly in production order. Each uninterrupted stretch of reasoning and
/// tool calls with more than one fold is put in one outer fold, so that models
/// that think and call tools many times do not flood the chat.
fn output_blocks(
    parts: &[Part],
    raw: bool,
    depth: usize,
    open_thinking: bool,
    live: bool,
    thinking_live: bool,
) -> Vec<RBlock> {
    let mut out = Vec::new();
    let mut answer = String::new();
    let mut citations: Vec<Citation> = Vec::new();
    let mut stretch = Stretch::default();
    let mut i = 0;
    while i < parts.len() {
        match &parts[i] {
            Part::Reasoning(_) => {
                let mut text = String::new();
                while let Some(Part::Reasoning(r)) = parts.get(i) {
                    match &r.text {
                        Some(t) if !t.is_empty() => {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                        // Reasoning that was not returned (encrypted, redacted): nothing to show.
                        _ => {}
                    }
                    i += 1;
                }
                if !text.is_empty() {
                    flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                    // The model is thinking this right now: nothing has come after it yet.
                    let current = thinking_live && !parts[i..].iter().any(visible);
                    let open = open_thinking && current;
                    let thinking = (live && current).then(|| thinking_title(&text)).flatten();
                    // "💭 Reasoning" (owner: back once the answer came first, 2026-11-13); while
                    // the model thinks, also what about.
                    let title = match &thinking {
                        Some(t) => format!("💭 Reasoning · {t}"),
                        None => "💭 Reasoning".to_string(),
                    };
                    stretch.reasoning += 1;
                    stretch.thinking = thinking;
                    // One level more for the outer fold it may end up in.
                    stretch.blocks.push(RBlock::Details {
                        open,
                        title: RText::sub(RText::plain(title)),
                        blocks: ai_blocks(&text, raw, depth + 2),
                    });
                }
            }
            Part::ToolCall(_) => {
                flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                let mut calls = Vec::new();
                while let Some(Part::ToolCall(t)) = parts.get(i) {
                    calls.push(t);
                    i += 1;
                }
                stretch.tool_calls += calls.len();
                stretch.blocks.push(RBlock::Details {
                    open: false,
                    title: RText::plain(tools_title(&calls)),
                    blocks: vec![RBlock::List { items: calls.iter().map(|t| tool_item(t)).collect() }],
                });
            }
            Part::Text { text, citations: c } => {
                // Answer text ends a stretch (whitespace between tool calls does not).
                if !text.trim().is_empty() {
                    stretch.flush(&mut out);
                }
                answer.push_str(text);
                citations.extend(c.iter().cloned());
                i += 1;
            }
            // A draft made while thinking: part of the reasoning stretch.
            Part::Image { blob, generated: true, dims, thought: true, .. } => {
                flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                stretch.reasoning += 1;
                stretch.blocks.push(RBlock::Photo {
                    key: blob.sha256.clone(),
                    caption: RText::plain(format!("💭 {}", image_caption(blob, *dims))),
                });
                i += 1;
            }
            Part::Image { blob, generated: true, dims, .. } => {
                flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                stretch.flush(&mut out);
                out.push(RBlock::Photo { key: blob.sha256.clone(), caption: RText::plain(image_caption(blob, *dims)) });
                i += 1;
            }
            // A generated file (a video, a file a code tool wrote): where it appeared.
            Part::File { blob, filename, generated: true } => {
                flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                stretch.flush(&mut out);
                let name = filename.clone().unwrap_or_else(|| blob.mime.clone());
                let (key, caption) = (blob.sha256.clone(), RText::plain(format!("{name} · {}", fmt_size(blob.size))));
                // Videos and audio play in place; anything else is a file.
                out.push(if blob.mime.starts_with("video/") {
                    RBlock::Video { key, caption }
                } else if blob.mime.starts_with("audio/") {
                    RBlock::Audio { key, caption }
                } else {
                    RBlock::Document { key, caption }
                });
                i += 1;
            }
            Part::Refusal { text } => {
                flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
                stretch.flush(&mut out);
                out.push(RBlock::Details {
                    open: true,
                    title: RText::plain("⛔ Refusal"),
                    blocks: vec![RBlock::Paragraph(RText::plain(text.clone()))],
                });
                i += 1;
            }
            _ => i += 1,
        }
    }
    stretch.flush(&mut out);
    flush_answer(&mut out, &mut answer, &mut citations, raw, depth);
    out
}

/// An earlier attempt of this reply: its output and error, folded.
fn prior_block(a: &PriorAttempt, raw: bool, show_usage: bool, bold_cost: f64) -> RBlock {
    let title = if a.interrupted {
        format!("⚠️ Attempt {} interrupted by a bot restart", a.n)
    } else if let Some(e) = &a.error {
        format!("⚠️ Attempt {} failed: {e}", a.n)
    } else {
        format!("⚠️ Attempt {} failed", a.n)
    };
    let mut blocks = output_blocks(&a.parts, raw, 1, false, false, false);
    if let Some(d) = a.error.as_ref().and_then(|e| e.details.as_ref()) {
        blocks.push(RBlock::Pre { text: pretty(&error_shown(d)), language: "json".into() });
    }
    let mut f = Vec::new();
    let mut bold = None;
    if show_usage && let Some(u) = &a.usage {
        f.extend(usage_parts(u));
        bold = bold_cost_at(&f, u, bold_cost);
    }
    if let Some(t) = a.elapsed.filter(|t| !t.is_zero()) {
        f.push(format!("{:.1}s", t.as_secs_f64()));
    }
    if !f.is_empty() {
        blocks.push(RBlock::Footer(footer_text(&f, bold)));
    }
    if blocks.is_empty() {
        blocks.push(RBlock::Footer(RText::plain("no output")));
    }
    RBlock::Details { open: false, title: with_code(&title), blocks }
}

/// Where the cost is in footer parts that end with [`usage_parts`], when it is
/// above `threshold` (USD, 0: never): its index and the length of the amount
/// (`≈$0.250`, without what may be appended to it later).
fn bold_cost_at(f: &[String], u: &Usage, threshold: f64) -> Option<(usize, usize)> {
    let cost = u.cost_usd?;
    (threshold > 0.0 && cost > threshold).then(|| (f.len() - 1, f[f.len() - 1].len()))
}

/// Footer parts joined with ` · `; the amount at `bold` (index, length) in bold.
fn footer_text(f: &[String], bold: Option<(usize, usize)>) -> RText {
    let Some((at, len)) = bold.filter(|(at, len)| f.get(*at).is_some_and(|s| s.len() >= *len)) else {
        return RText::plain(f.join(" · "));
    };
    let mut t = Vec::new();
    for (i, s) in f.iter().enumerate() {
        if i > 0 {
            t.push(RText::plain(" · "));
        }
        if i == at {
            t.push(RText::bold(RText::plain(&s[..len])));
            if len < s.len() {
                t.push(RText::plain(&s[len..]));
            }
        } else {
            t.push(RText::plain(s.clone()));
        }
    }
    RText::concat(t)
}

/// Usage in words: input (cache), output (reasoning), cost. The input count
/// includes the tokens read from or written to the cache (every platform's
/// counts are normalized so): `in 326,770 (cached 224,091)`.
fn usage_parts(u: &Usage) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(x) = u.input_tokens {
        let mut s = format!("in {}", fmt_tokens(x));
        let mut cache = Vec::new();
        if let Some(c) = u.cached_input_tokens.filter(|c| *c > 0) {
            cache.push(format!("cached {}", fmt_tokens(c)));
        }
        if let Some(c) = u.cache_write_tokens.filter(|c| *c > 0) {
            cache.push(format!("cache write {}", fmt_tokens(c)));
        }
        if !cache.is_empty() {
            s.push_str(&format!(" ({})", cache.join(", ")));
        }
        f.push(s);
    }
    if let Some(x) = u.output_tokens {
        let mut s = format!("out {}", fmt_tokens(x));
        // A total for the whole reply: providers do not count reasoning per block.
        if let Some(r) = u.reasoning_tokens.filter(|r| *r > 0) {
            s.push_str(&format!(" (reasoning {})", fmt_tokens(r)));
        }
        f.push(s);
    }
    if let Some(c) = u.cost_usd {
        // `≈`: estimated from the bot's price table, not reported by the API.
        let approx = if u.cost_source == omni_core::usage::CostSource::Estimated { "≈" } else { "" };
        f.push(format!("{approx}{}", fmt_cost(c)));
    }
    f
}

pub fn compose(inp: &ComposeInput<'_>) -> Vec<RBlock> {
    let raw = inp.raw_ai || inp.mode == Mode::Raw;
    let finished = inp.status != Status::Generating;
    let mut out = vec![header(inp.model, None)];

    // 2. earlier attempts of this reply (failed or interrupted), oldest first, folded.
    for a in inp.prior {
        out.push(prior_block(a, raw, inp.show_usage, inp.bold_cost));
    }
    // 3. this attempt's output, in the order it was produced.
    let live = inp.status == Status::Generating;
    // One continuous answer: above its reasoning and tool calls, so that Telegram's
    // "show more" on a long message never hides it (owner's rule, `render.answer_first`).
    let reordered = inp.answer_first.then(|| answer_first(inp.parts)).flatten();
    let (parts, thinking_live) = match &reordered {
        // The model thinks right now only if its last visible output is reasoning.
        Some(p) => {
            (p.as_slice(), inp.parts.iter().rev().find(|p| visible(p)).is_some_and(|p| matches!(p, Part::Reasoning(_))))
        }
        None => (inp.parts, true),
    };
    out.extend(output_blocks(parts, raw, 0, inp.reasoning_open && live, live, thinking_live));
    // Images still being generated: their latest streamed preview.
    for (blob, dims) in inp.previews {
        out.push(RBlock::Photo {
            key: blob.sha256.clone(),
            caption: RText::plain(format!("⏳ preview · {}", image_caption(blob, *dims))),
        });
    }

    // 6. error: the message in the title; the complete error object inside.
    if let Some(e) = inp.error {
        let title = format!("⚠️ Error: {e}");
        match &e.details {
            Some(d) => out.push(RBlock::Details {
                open: false,
                title: with_code(&title),
                blocks: vec![RBlock::Pre { text: pretty(&error_shown(d)), language: "json".into() }],
            }),
            None => out.push(RBlock::Footer(with_code(&title))),
        }
    }

    // 7. footers: status and notes, then the summary line
    let mut lines: Vec<String> = Vec::new();
    match &inp.status {
        Status::Retrying { in_secs } => lines.push(format!("🔁 retrying in {in_secs}s")),
        Status::Failed if inp.error.is_none() => lines.push("⚠️ failed".into()),
        Status::Done => {
            if let Some(why) = abnormal_finish(inp.finish_reason) {
                let native = inp.native_finish_reason.map(|n| format!(" / {n}")).unwrap_or_default();
                lines.push(format!(
                    "⚠️ incomplete: {why} (finish_reason: {}{native})",
                    inp.finish_reason.unwrap_or_default()
                ));
            }
        }
        _ => {}
    }
    for n in inp.notes {
        // Some notes come with their ℹ️ already.
        let n = n.trim_start_matches("ℹ\u{fe0f}").trim_start_matches('ℹ').trim_start();
        lines.push(format!("ℹ️ {n}"));
    }
    out.extend(lines.into_iter().map(|l| RBlock::Footer(RText::plain(l))));
    out.extend(inp.attachments.iter().map(|k| RBlock::Document { key: k.clone(), caption: RText::plain("") }));

    let mut f: Vec<String> = Vec::new();
    if inp.status == Status::Generating {
        f.push(format!("⏳ generating · {}s", inp.elapsed.as_secs()))
    }
    // `+ …` settings name charges the cost estimate leaves out: shown after the cost.
    let (extra_costs, settings): (Vec<&String>, Vec<&String>) = inp.settings.iter().partition(|s| s.starts_with("+ "));
    if !settings.is_empty() {
        f.extend(settings.into_iter().cloned());
    } else if !inp.options.is_empty() {
        f.push(inp.options.to_string());
    }
    if inp.continuation {
        f.push("continued".into());
    } else if inp.attempt > inp.max_attempts as i64 {
        // Retried by hand after the automatic attempts ran out.
        f.push(format!("attempt {}", inp.attempt));
    } else if inp.attempt > 1 {
        f.push(format!("attempt {}/{}", inp.attempt, inp.max_attempts));
    }
    let mut bold = None;
    if inp.show_usage
        && let Some(u) = inp.usage
    {
        f.extend(usage_parts(u));
        bold = bold_cost_at(&f, u, inp.bold_cost);
    }
    if !extra_costs.is_empty() {
        let extra = extra_costs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
        match f.last_mut().filter(|_| inp.show_usage && inp.usage.is_some_and(|u| u.cost_usd.is_some())) {
            Some(cost) => cost.push_str(&format!(" {extra}")),
            None => f.push(extra),
        }
    }
    if finished && !inp.elapsed.is_zero() {
        f.push(format!("{:.1}s", inp.elapsed.as_secs_f64()));
    }
    if let Some(p) = inp.provider {
        f.push(format!("via {p}"));
    }
    f.extend(inp.served.iter().cloned());
    if inp.hops > 0 {
        f.push(format!("long decode: {} hops", inp.hops + 1));
    }
    if let Some(fr) = inp.finish_reason
        && fr != "stop"
        && abnormal_finish(Some(fr)).is_none()
    {
        f.push(format!("finish: {fr}"));
    }
    if !f.is_empty() {
        out.push(RBlock::Footer(footer_text(&f, bold)));
    }
    out
}

/// A file size: `623.9 KB`, `12.4 MB`.
fn fmt_size(b: u64) -> String {
    if b >= 1_000_000 { format!("{:.1} MB", b as f64 / 1e6) } else { format!("{:.1} KB", b as f64 / 1e3) }
}

/// `1024×1024 · 623.9 KB · JPEG` (dimensions when known, file size, format).
pub fn image_caption(blob: &omni_core::BlobRef, dims: Option<(u32, u32)>) -> String {
    let mut p = Vec::new();
    if let Some((w, h)) = dims {
        p.push(format!("{w}×{h}"));
    }
    p.push(fmt_bytes(blob.size));
    if let Some(f) = blob.mime.strip_prefix("image/") {
        p.push(f.to_ascii_uppercase());
    }
    p.join(" · ")
}

pub fn fmt_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / (1u64 << 10) as f64),
        n => format!("{n} B"),
    }
}

/// The complete error object as shown to users: everything the provider and
/// router returned, except the requester's `user_id` and OpenRouter's echo of
/// our own request (`metadata.debug`, which can be huge; it is in /info).
/// A JSON string in `metadata.raw` (the upstream body) is shown parsed.
fn error_shown(details: &Value) -> Value {
    let mut d = details.clone();
    if let Some(o) = d.as_object_mut() {
        o.remove("user_id");
    }
    for meta in [d.pointer_mut("/error/metadata"), None].into_iter().flatten() {
        if let Some(m) = meta.as_object_mut() {
            m.remove("debug");
            if let Some(raw) = m.get_mut("raw")
                && let Some(parsed) = raw.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok())
                && (parsed.is_object() || parsed.is_array())
            {
                *raw = parsed;
            }
        }
    }
    if let Some(m) = d.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("debug");
    }
    d
}

/// Why a successfully streamed answer may be incomplete, from its finish reason.
fn abnormal_finish(fr: Option<&str>) -> Option<&'static str> {
    match fr? {
        "length" => Some("the output limit was reached"),
        "content_filter" => Some("stopped by the provider's content filter"),
        "error" => Some("the provider stopped the generation with an error"),
        _ => None,
    }
}

fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Telegram's rich-message limits (32768 characters of text in all blocks,
/// 500 blocks counting nested blocks, list items and table rows — measured
/// against the live API, 2026-11-11), minus room for the header line that
/// continuation pages get.
pub const PAGE_LIMITS: RichLimits = RichLimits { max_chars: 32_768 - 128, max_blocks: 500 - 1, ..RichLimits::TELEGRAM };

/// Split a composed document into as few Telegram messages as the limits
/// allow; continuation messages get the header line too.
pub fn paginate(model: &str, blocks: Vec<RBlock>) -> Vec<Vec<RBlock>> {
    let pages = rich::split_blocks(rich::sanitize(blocks), PAGE_LIMITS);
    pages
        .into_iter()
        .enumerate()
        .map(|(i, mut p)| {
            if i > 0 {
                p.insert(0, header(model, Some(i + 1)));
            }
            p
        })
        .collect()
}

/// Last-resort classic text rendering (no rich blocks): the same structure as
/// plain text, details as expandable quotations.
pub fn blocks_to_plain(blocks: &[RBlock]) -> Formatted {
    let mut f = Formatted::new();
    for b in blocks {
        write_block(&mut f, b, 0);
        f.ensure_trailing_newlines(2);
        f.mark_break(omni_markdown::BreakPriority::Block);
    }
    f
}

fn write_text(f: &mut Formatted, t: &RText) {
    match t {
        RText::Plain(s) | RText::Math(s) => f.push_str(s),
        RText::Concat(v) => v.iter().for_each(|x| write_text(f, x)),
        RText::Underline(x) => {
            let s = f.len();
            write_text(f, x);
            f.wrap_from(s, EntityKind::Underline);
        }
        RText::Bold(x) => {
            let s = f.len();
            write_text(f, x);
            f.wrap_from(s, EntityKind::Bold);
        }
        RText::Italic(x) | RText::Strike(x) | RText::Fixed(x) | RText::Spoiler(x) | RText::Sub(x) | RText::Super(x) => {
            write_text(f, x)
        }
        RText::Url { text, .. } => write_text(f, text),
    }
}

fn write_block(f: &mut Formatted, b: &RBlock, depth: usize) {
    let indent = "  ".repeat(depth);
    match b {
        RBlock::Paragraph(t)
        | RBlock::Heading { text: t, .. }
        | RBlock::Footer(t)
        | RBlock::CollapsedQuote { text: t } => {
            f.push_str(&indent);
            write_text(f, t);
        }
        RBlock::Pre { text, .. } => {
            let s = f.len();
            f.push_str(text);
            f.wrap_from(s, EntityKind::Pre { language: String::new() });
        }
        RBlock::Math(s) => f.push_str(s),
        RBlock::Divider => f.push_str("────────"),
        RBlock::List { items } | RBlock::OrderedList { items, .. } => {
            for (i, it) in items.iter().enumerate() {
                f.push_str(&format!("{indent}{}. ", i + 1));
                for (j, b) in it.blocks.iter().enumerate() {
                    if j > 0 {
                        f.push_char('\n');
                    }
                    write_block(f, b, depth + 1);
                }
                f.push_char('\n');
            }
        }
        RBlock::Quote { blocks } => {
            for b in blocks {
                write_block(f, b, depth);
                f.push_char('\n');
            }
        }
        RBlock::Table { rows, .. } => {
            for r in rows {
                let cells: Vec<String> = r.iter().map(|c| c.text.to_plain()).collect();
                f.push_str(&cells.join(" | "));
                f.push_char('\n');
            }
        }
        RBlock::Photo { caption, .. } => {
            f.push_str("🖼 [image] ");
            write_text(f, caption);
        }
        RBlock::Document { caption, .. } => {
            f.push_str("📎 [file] ");
            write_text(f, caption);
        }
        RBlock::Video { caption, .. } => {
            f.push_str("🎬 [video] ");
            write_text(f, caption);
        }
        RBlock::Audio { caption, .. } => {
            f.push_str("🔊 [audio] ");
            write_text(f, caption);
        }
        RBlock::Details { title, blocks, .. } => {
            write_text(f, title);
            f.push_char('\n');
            let s = f.len();
            for b in blocks {
                write_block(f, b, 0);
                f.push_char('\n');
            }
            f.wrap_from(s, EntityKind::Blockquote { collapsed: true });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_says_what_the_model_is_thinking_while_it_thinks() {
        // A summary's latest heading; else the start of the latest paragraph.
        assert_eq!(
            thinking_title("**Reading the question**\n\nThe user asks…\n\n**Checking the dates**\n\nIt was").as_deref(),
            Some("Checking the dates")
        );
        assert_eq!(thinking_title("## Plan\nsteps").as_deref(), Some("Plan"));
        assert_eq!(thinking_title("First idea.\n\nThen   the second\nline").as_deref(), Some("Then the second line"));
        let long = "word ".repeat(30);
        assert_eq!(thinking_title(&long).unwrap().chars().count(), 48);
        let reasoning = |t: &str| {
            Part::Reasoning(omni_core::Reasoning {
                kind: omni_core::content::ReasoningKind::Summary,
                text: Some(t.into()),
                ..Default::default()
            })
        };
        let parts = vec![reasoning("**Planning**\n\nSo")];
        let title = |blocks: &[RBlock]| {
            blocks
                .iter()
                .find_map(|b| match b {
                    RBlock::Details { title, .. } => Some(title.to_plain()),
                    _ => None,
                })
                .unwrap()
        };
        // While generating: the heading; once the reply is done: a plain 💭.
        assert_eq!(title(&compose(&input(&parts, Status::Generating))), "💭 Reasoning · Planning");
        assert_eq!(title(&compose(&input(&parts, Status::Done))), "💭 Reasoning");
        // Something after it (the answer): thinking is over.
        let parts = vec![reasoning("**Planning**\n\nSo"), Part::text("Answer")];
        assert_eq!(title(&compose(&input(&parts, Status::Generating))), "💭 Reasoning");
    }

    #[test]
    fn extra_charges_follow_the_cost() {
        let usage =
            Usage { input_tokens: Some(10), output_tokens: Some(5), cost_usd: Some(0.008), ..Default::default() };
        let settings = vec!["effort max".to_string(), "code execution".to_string(), "+ code execution".to_string()];
        let mut i = input(&[], Status::Done);
        i.usage = Some(&usage);
        i.settings = &settings;
        let blocks = compose(&i);
        let RBlock::Footer(t) = blocks.last().unwrap() else { panic!() };
        let footer = t.to_plain();
        assert!(
            footer.starts_with("effort max · code execution · in 10 · out 5 · $0.00800 + code execution · "),
            "{footer}"
        );
        assert_eq!(footer.matches("+ code execution").count(), 1, "{footer}");
    }

    /// A cost above `render.bold_cost_usd` is bold, without what follows it.
    #[test]
    fn costs_above_the_threshold_are_bold() {
        let footer = |cost: f64, threshold: f64, extra: bool| {
            let usage =
                Usage { input_tokens: Some(10), output_tokens: Some(5), cost_usd: Some(cost), ..Default::default() };
            let settings = if extra { vec!["+ code execution".to_string()] } else { vec![] };
            let mut i = input(&[], Status::Done);
            i.usage = Some(&usage);
            i.settings = &settings;
            i.bold_cost = threshold;
            let Some(RBlock::Footer(t)) = compose(&i).pop() else { panic!() };
            t
        };
        let bold = |t: &RText| match t {
            RText::Concat(v) => v
                .iter()
                .filter_map(|x| match x {
                    RText::Bold(b) => Some(b.to_plain()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert!(bold(&footer(0.2, 0.2, false)).is_empty(), "not above it");
        assert_eq!(bold(&footer(0.25, 0.2, false)), ["$0.250"]);
        let t = footer(0.25, 0.2, true);
        assert_eq!(bold(&t), ["$0.250"]);
        assert!(t.to_plain().contains("out 5 · $0.250 + code execution · "), "{}", t.to_plain());
        assert!(bold(&footer(5.0, 0.0, false)).is_empty(), "0: never");
    }

    #[test]
    fn source_titles_lose_markup() {
        assert_eq!(plain_title("<b>Oriental Pearl Tower</b> - Wikipedia"), "Oriental Pearl Tower - Wikipedia");
        assert_eq!(plain_title("Shanghai&#39;s modern icon &amp; more"), "Shanghai's modern icon & more");
        assert_eq!(plain_title("a < b"), "a < b");
    }

    #[test]
    fn tool_fold_titles_say_what_ran() {
        let call = |name: &str, input: serde_json::Value, status: &str| ToolCall {
            id: None,
            name: name.into(),
            status: Some(status.into()),
            input: Some(input),
            output: None,
        };
        let search =
            call("web_search", serde_json::json!({"type": "search", "query": "rust latest version"}), "completed");
        let openai = call(
            "web_search",
            serde_json::json!({"action": {"type": "search", "query": "rust 1.100 date"}}),
            "completed",
        );
        let gemini = call("google_search", serde_json::json!({"queries": ["a", "", "b"]}), "completed");
        let code = call("code_interpreter", serde_json::json!("print(2**100)\nprint(1)"), "in_progress");
        let fetch = call("web_fetch", serde_json::json!({"url": "https://www.rust-lang.org/"}), "completed");
        let open = call("web_search", serde_json::json!({"type": "open_page", "url": "https://a.b/c"}), "completed");
        let find = call(
            "web_search",
            serde_json::json!({"type": "find_in_page", "pattern": "1.100.0", "url": "https://a.b/c"}),
            "completed",
        );
        assert_eq!(
            tools_title(&[&open, &find]),
            "🔧 web_search ×2: open https://a.b/c; find \"1.100.0\" in https://a.b/c"
        );
        assert_eq!(tools_title(&[&search]), "🔧 web_search: rust latest version");
        assert_eq!(tools_title(&[&search, &openai]), "🔧 web_search ×2: rust latest version; rust 1.100 date");
        assert_eq!(tools_title(&[&gemini]), "🔧 google_search: a; b");
        assert_eq!(
            tools_title(&[&code, &fetch]),
            "🔧 code_interpreter (in_progress): print(2**100) print(1) · web_fetch: https://www.rust-lang.org/"
        );
        let bare = ToolCall { name: "x_search".into(), ..Default::default() };
        assert_eq!(tools_title(&[&bare]), "🔧 x_search");
        let long = call("web_search", serde_json::json!({"query": "word ".repeat(40)}), "completed");
        assert!(tools_title(&[&long]).chars().count() <= 120);
    }

    #[test]
    fn token_counts_in_units() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_234, "1.2K"),
            (9_940, "9.9K"),
            (9_960, "10K"),
            (45_321, "45K"),
            (380_499, "380K"),
            (999_499, "999K"),
            (999_960, "1M"),
            (1_234_567, "1.2M"),
            (12_345_678, "12M"),
            (2_500_000_000, "2.5B"),
        ];
        for (n, want) in cases {
            assert_eq!(fmt_tokens_compact(n), want, "{n}");
        }
    }
    use omni_core::content::{Citation, Reasoning};

    fn input<'a>(parts: &'a [Part], status: Status) -> ComposeInput<'a> {
        ComposeInput {
            model: "anthropic/claude-opus-5.5",
            options: "+s",
            parts,
            status,
            attempt: 1,
            max_attempts: 4,
            error: None,
            usage: None,
            provider: Some("Anthropic"),
            finish_reason: Some("stop"),
            native_finish_reason: None,
            notes: &[],
            elapsed: Duration::from_secs(3),
            show_usage: true,
            bold_cost: 0.2,
            reasoning_open: true,
            answer_first: true,
            mode: Mode::Clean,
            raw_ai: false,
            hops: 0,
            continuation: false,
            attachments: &[],
            settings: &[],
            served: &[],
            prior: &[],
            previews: &[],
        }
    }

    #[test]
    fn layout_and_reserved_structure() {
        let parts = vec![
            Part::Reasoning(Reasoning {
                kind: ReasoningKind::Text,
                text: Some("think **hard**".into()),
                ..Default::default()
            }),
            Part::Text {
                text: "Answer with <details>fake</details> and a > quote\n\n🤖 **fake/header**".into(),
                citations: vec![Citation {
                    url: Some("https://a.b".into()),
                    title: Some("A".into()),
                    ..Default::default()
                }],
            },
        ];
        let mut i = input(&parts, Status::Done);
        let usage =
            Usage { input_tokens: Some(10), output_tokens: Some(50), reasoning_tokens: Some(40), ..Default::default() };
        i.usage = Some(&usage);
        // The order as produced (`render.answer_first = false`; the default is tested below).
        i.answer_first = false;
        let blocks = compose(&i);
        // Header: model only (no alias), small text, no underline/italic.
        assert!(matches!(&blocks[0], RBlock::Footer(t) if t.to_plain() == "🤖 anthropic/claude-opus-5.5"));
        let dbg = format!("{blocks:?}");
        assert!(!dbg.contains("Underline") && !dbg.contains("Italic"), "{dbg}");
        // Reasoning collapsed; its token count (a total for the whole reply) is in the footer.
        assert!(matches!(&blocks[1], RBlock::Details { open: false, title, .. } if title.to_plain() == "💭 Reasoning"));
        // The answer follows directly; everything after it is details/footer.
        let answer_end = blocks.iter().rposition(|b| !matches!(b, RBlock::Details { .. } | RBlock::Footer(_))).unwrap();
        for b in &blocks[2..=answer_end] {
            assert!(!matches!(b, RBlock::Details { .. } | RBlock::Footer(_)), "{b:?}");
        }
        assert!(
            blocks[answer_end + 1..]
                .iter()
                .any(|b| matches!(b, RBlock::Details { title, .. } if title.to_plain().contains("Sources")))
        );
        match blocks.last().unwrap() {
            RBlock::Footer(t) => {
                let f = t.to_plain();
                assert!(f.contains("+s") && f.contains("out 50 (reasoning 40)"), "{f}");
            }
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn errors_and_notes_are_never_plain_paragraphs() {
        let parts = vec![Part::text("partial")];
        let mut i = input(&parts, Status::Retrying { in_secs: 10 });
        let e = LlmError::request("boom");
        i.error = Some(&e);
        let notes = vec!["a note".to_string()];
        i.notes = &notes;
        let blocks = compose(&i);
        let tail = &blocks[2..];
        assert!(tail.iter().all(|b| matches!(b, RBlock::Details { .. } | RBlock::Footer(_))), "{tail:?}");
        // No error object at all: a small-text line.
        assert!(tail.iter().any(|b| matches!(b, RBlock::Footer(t) if t.to_plain().starts_with("⚠️ Error"))));
        // With upstream details: collapsible, details inside.
        let mut e2 = LlmError::request("boom");
        e2.details = Some(serde_json::json!({"error": {"message": "boom", "metadata": {"provider_name": "X"}}}));
        i.error = Some(&e2);
        let blocks = compose(&i);
        assert!(
            blocks[2..]
                .iter()
                .any(|b| matches!(b, RBlock::Details { title, .. } if title.to_plain().starts_with("⚠️ Error")))
        );
    }

    #[test]
    fn errors_are_shown_completely_except_user_id_and_request_echo() {
        let or = serde_json::json!({
            "error": {"message": "upstream", "code": 502, "metadata": {
                "provider_name": "Google", "is_byok": false,
                "raw": "{\"error\":{\"status\":\"UNAVAILABLE\"}}",
                "debug": {"echo_upstream_body": {"contents": []}}}},
            "user_id": "user_x",
            "openrouter_metadata": {"region": "HEL"}});
        assert_eq!(
            error_shown(&or),
            serde_json::json!({
                "error": {"message": "upstream", "code": 502, "metadata": {
                    "provider_name": "Google", "is_byok": false, "raw": {"error": {"status": "UNAVAILABLE"}}}},
                "openrouter_metadata": {"region": "HEL"}})
        );
        let ark = serde_json::json!({"code": "QuotaExceeded", "message": "m", "param": "", "type": "TooManyRequests"});
        assert_eq!(error_shown(&ark), ark);
        assert_eq!(error_shown(&serde_json::json!("plain text")), serde_json::json!("plain text"));
    }

    /// A compact signature of a block, for order checks.
    fn kind(b: &RBlock) -> String {
        match b {
            RBlock::Details { title, open, .. } => {
                format!("{}{}", title.to_plain().split(' ').next().unwrap_or(""), if *open { "+" } else { "" })
            }
            RBlock::Photo { .. } => "photo".into(),
            RBlock::Footer(t) if t.to_plain().starts_with("🤖") => "header".into(),
            RBlock::Footer(_) => "footer".into(),
            RBlock::Paragraph(t) => format!("p:{}", t.to_plain()),
            other => format!("{other:?}").chars().take(12).collect(),
        }
    }

    fn tool_call() -> Part {
        Part::ToolCall(ToolCall { id: Some("t".into()), name: "web_search".into(), ..Default::default() })
    }

    /// One continuous answer comes first, its reasoning and tool calls after
    /// it in their order; answers that alternate with reasoning stay in order.
    #[test]
    fn one_answer_comes_first() {
        let reasoning = |t: &str| {
            Part::Reasoning(Reasoning { kind: ReasoningKind::Summary, text: Some(t.into()), ..Default::default() })
        };
        let cite = Citation { url: Some("https://a.b".into()), title: Some("A".into()), ..Default::default() };
        let image = Part::Image {
            blob: omni_core::BlobRef::for_bytes(b"png", "image/png"),
            filename: None,
            generated: true,
            dims: Some((1, 1)),
            thought: false,
        };
        let kinds =
            |parts: &[Part], status: Status| compose(&input(parts, status)).iter().map(kind).collect::<Vec<_>>();
        // Thinking, a search, thinking, then the answer (text with sources and an image).
        let parts = vec![
            reasoning("plan"),
            tool_call(),
            reasoning("after the search"),
            Part::Text { text: "answer".into(), citations: vec![cite] },
            image,
        ];
        assert_eq!(kinds(&parts, Status::Done), ["header", "p:answer", "📚", "photo", "💭", "footer"]);
        let blocks = compose(&input(&parts, Status::Done));
        let RBlock::Details { title, blocks: inner, .. } = &blocks[4] else { panic!("{:?}", blocks[4]) };
        assert_eq!(title.to_plain(), "💭 2 · 🔧 1");
        assert_eq!(inner.iter().map(kind).collect::<Vec<_>>(), ["💭", "🔧", "💭"]);
        // While the answer streams the reasoning is below it, closed and plain: thinking is over.
        let parts = vec![reasoning("**Planning**\n\nSo"), Part::text("Ans")];
        assert_eq!(kinds(&parts, Status::Generating), ["header", "p:Ans", "💭", "footer"]);
        // Thinking again after the one answer: still one answer, so it stays first; the
        // reasoning in progress keeps its title.
        let parts = vec![reasoning("a"), Part::text("Ans"), reasoning("**Checking**\n\nb")];
        let blocks = compose(&input(&parts, Status::Generating));
        assert_eq!(blocks.iter().map(kind).collect::<Vec<_>>(), ["header", "p:Ans", "💭+", "footer"]);
        assert!(matches!(&blocks[2], RBlock::Details { title, .. } if title.to_plain() == "💭 Reasoning · Checking"));
        // Answers alternating with reasoning or tool calls: the order as produced.
        let parts = vec![reasoning("a"), Part::text("first"), tool_call(), Part::text("second")];
        assert_eq!(kinds(&parts, Status::Done), ["header", "💭", "p:first", "🔧", "p:second", "footer"]);
        // Nothing to move: reasoning only, or the answer first anyway.
        assert_eq!(kinds(&[reasoning("a")], Status::Done), ["header", "💭", "footer"]);
        assert_eq!(kinds(&[Part::text("Ans"), tool_call()], Status::Done), ["header", "p:Ans", "🔧", "footer"]);
        // `render.answer_first = false`: the order as produced.
        let mut i = input(&parts[..2], Status::Done);
        i.answer_first = false;
        assert_eq!(compose(&i).iter().map(kind).collect::<Vec<_>>(), ["header", "💭", "p:first", "footer"]);
    }

    #[test]
    fn output_is_shown_in_the_order_it_was_produced() {
        // As produced (`render.answer_first = false`).
        let chrono = |parts: &[Part], status: Status| {
            let mut i = input(parts, status);
            i.answer_first = false;
            compose(&i)
        };
        let reasoning = |t: &str| {
            Part::Reasoning(Reasoning { kind: ReasoningKind::Summary, text: Some(t.into()), ..Default::default() })
        };
        let tool = Part::ToolCall(ToolCall {
            id: Some("1".into()),
            name: "web_search".into(),
            status: Some("completed".into()),
            ..Default::default()
        });
        let cite = Citation { url: Some("https://a.b".into()), title: Some("A".into()), ..Default::default() };
        let image = Part::Image {
            blob: omni_core::BlobRef::for_bytes(b"png", "image/png"),
            filename: None,
            generated: true,
            dims: Some((1, 1)),
            thought: false,
        };
        let parts = vec![
            reasoning("plan"),
            reasoning("more plan"),
            tool.clone(),
            reasoning("after the search"),
            Part::Text { text: "first".into(), citations: vec![cite] },
            image,
            Part::text("second"),
            tool,
        ];
        let blocks = chrono(&parts, Status::Done);
        let kinds: Vec<String> = blocks.iter().map(kind).collect();
        // The stretch of reasoning and tool calls before the answer is one fold;
        // the lone tool call after the second text stays as it is.
        assert_eq!(kinds, vec!["header", "💭", "p:first", "📚", "photo", "p:second", "🔧", "footer"], "{blocks:#?}");
        let RBlock::Details { title, blocks: inner, .. } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(title.to_plain(), "💭 2 · 🔧 1");
        assert_eq!(inner.iter().map(kind).collect::<Vec<_>>(), vec!["💭", "🔧", "💭"]);
        // Adjacent reasoning parts form one block, in order.
        assert!(format!("{:?}", inner[0]).contains("plan") && format!("{:?}", inner[0]).contains("more plan"));

        // While generating, only reasoning with nothing after it yet is open
        // (with `render.reasoning_open`), and the fold around it.
        let parts = vec![reasoning("a"), Part::text("x"), reasoning("b")];
        let kinds: Vec<String> = chrono(&parts, Status::Generating).iter().map(kind).collect();
        assert_eq!(kinds, vec!["header", "💭", "p:x", "💭+", "footer"]);
        let parts = vec![reasoning("a"), tool_call(), reasoning("b")];
        let kinds: Vec<String> = chrono(&parts, Status::Generating).iter().map(kind).collect();
        assert_eq!(kinds, vec!["header", "💭+", "footer"]);
        // Whitespace between tool calls does not split a stretch.
        let parts = vec![tool_call(), Part::text("\n\n"), reasoning("a"), Part::text("done")];
        let blocks = chrono(&parts, Status::Done);
        assert!(
            matches!(&blocks[1], RBlock::Details { title, .. } if title.to_plain() == "💭 1 · 🔧 1"),
            "{blocks:#?}"
        );
        assert_eq!(blocks.len(), 4, "{blocks:#?}");
    }

    #[test]
    fn earlier_attempts_are_folded_above_in_order() {
        let mut e = LlmError::request("boom");
        e.details =
            Some(serde_json::json!({"error": {"message": "boom", "metadata": {"provider_name": "X"}}, "user_id": "u"}));
        let prior = vec![
            PriorAttempt {
                n: 1,
                parts: vec![Part::text("partial answer")],
                error: Some(e),
                interrupted: false,
                usage: Some(Usage {
                    input_tokens: Some(5),
                    output_tokens: Some(7),
                    cost_usd: Some(0.001),
                    ..Default::default()
                }),
                elapsed: Some(Duration::from_millis(2500)),
            },
            PriorAttempt { n: 2, parts: vec![], error: None, interrupted: true, usage: None, elapsed: None },
        ];
        let parts = vec![Part::text("final answer")];
        let mut i = input(&parts, Status::Done);
        i.attempt = 3;
        i.prior = &prior;
        let blocks = compose(&i);
        let kinds: Vec<String> = blocks.iter().map(kind).collect();
        assert_eq!(kinds, vec!["header", "⚠️", "⚠️", "p:final answer", "footer"], "{blocks:#?}");
        let RBlock::Details { title, blocks: inner, .. } = &blocks[1] else { unreachable!() };
        assert!(title.to_plain().starts_with("⚠️ Attempt 1 failed: "), "{}", title.to_plain());
        let inner_dbg = format!("{inner:?}");
        assert!(
            inner_dbg.contains("partial answer")
                && inner_dbg.contains("provider_name")
                && !inner_dbg.contains("user_id")
        );
        assert!(inner_dbg.contains("out 7") && inner_dbg.contains("2.5s"), "{inner_dbg}");
        assert!(
            matches!(&blocks[2], RBlock::Details { title, .. } if title.to_plain() == "⚠️ Attempt 2 interrupted by a bot restart")
        );
        assert!(matches!(blocks.last(), Some(RBlock::Footer(t)) if t.to_plain().contains("attempt 3/4")));
    }

    #[test]
    fn reasoning_open_while_thinking() {
        let parts = vec![Part::Reasoning(Reasoning {
            kind: ReasoningKind::Summary,
            text: Some("t".into()),
            ..Default::default()
        })];
        let blocks = compose(&input(&parts, Status::Generating));
        assert!(matches!(&blocks[1], RBlock::Details { open: true, .. }));
        // Off (the default): always folded, even while thinking.
        let mut i = input(&parts, Status::Generating);
        i.reasoning_open = false;
        assert!(matches!(&compose(&i)[1], RBlock::Details { open: false, .. }));
    }

    #[test]
    fn raw_fallback_keeps_bot_structure() {
        let parts = vec![Part::text("# Title\n\n**bold**")];
        let mut i = input(&parts, Status::Done);
        i.raw_ai = true;
        let blocks = compose(&i);
        assert!(blocks.iter().any(
            |b| matches!(b, RBlock::Pre { language, text } if language == "markdown" && text.contains("**bold**"))
        ));
        assert!(matches!(blocks.last().unwrap(), RBlock::Footer(_)));
    }

    #[test]
    fn pagination_adds_headers() {
        let long: String = (0..2000).map(|i| format!("line {i} {}\n\n", "x".repeat(30))).collect();
        let parts = vec![Part::text(long)];
        let blocks = compose(&input(&parts, Status::Done));
        let pages = paginate("anthropic/claude-opus-5.5", blocks);
        assert!(pages.len() >= 2);
        assert!(matches!(&pages[1][0], RBlock::Footer(t) if t.to_plain() == "🤖 anthropic/claude-opus-5.5 · part 2"));
    }
}
