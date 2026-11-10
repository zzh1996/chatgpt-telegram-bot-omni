//! Composition of a reply into rich blocks.
//!
//! Layout (top to bottom):
//! 1. header line: `🤖 model`;
//! 2. reasoning: collapsible details (open only while thinking, before any answer);
//! 3. tool calls: collapsible details;
//! 4. the model's answer (rendered Markdown, or raw Markdown in raw mode / fallback);
//! 5. sources: collapsible details;
//! 6. errors: collapsible details (verbatim upstream JSON inside);
//! 7. small-text footers: status and notes, then options, usage, cost, timing, provider.
//!
//! Unambiguous by structure, without visual marking: the first line of every
//! reply message is the bot's header, and everything else the bot adds is a
//! collapsible details block or a footer — block types the Markdown renderer
//! never produces from model output. So any reader can tell exactly which part
//! is the model's answer; model output cannot pass for reasoning, errors,
//! notes or usage.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Part, ReasoningKind, ToolCall};
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
    /// Interrupted by a bot restart.
    Interrupted,
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

/// The first line of every reply message.
fn header(model: &str, part: Option<usize>) -> RBlock {
    let mut t = vec![RText::plain("🤖 "), label(model)];
    if let Some(p) = part {
        t.push(RText::plain(format!(" · part {p}")));
    }
    RBlock::Paragraph(RText::concat(t))
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

pub fn fmt_cost(c: f64) -> String {
    if c >= 1.0 {
        format!("${c:.2}")
    } else if c >= 0.01 {
        format!("${c:.3}")
    } else {
        format!("${c:.5}")
    }
}

fn ai_blocks(md: &str, raw: bool, base_depth: usize) -> Vec<RBlock> {
    if raw { rich::raw_markdown_blocks(md) } else { rich::markdown_to_blocks(md, base_depth, RichLimits::TELEGRAM) }
}

pub fn compose(inp: &ComposeInput<'_>) -> Vec<RBlock> {
    let raw = inp.raw_ai || inp.mode == Mode::Raw;
    let finished = inp.status != Status::Generating;
    let mut out = vec![header(inp.model, None)];

    // 2. reasoning
    let mut reasoning = String::new();
    let mut kinds = Vec::new();
    let mut opaque = None;
    for p in inp.parts {
        if let Part::Reasoning(r) = p {
            match &r.text {
                Some(t) if !t.is_empty() => {
                    if !reasoning.is_empty() {
                        reasoning.push_str("\n\n");
                    }
                    reasoning.push_str(t);
                    if !kinds.contains(&r.kind) {
                        kinds.push(r.kind);
                    }
                }
                _ => opaque = Some(r.kind),
            }
        }
    }
    let reasoning_tokens = inp.usage.and_then(|u| u.reasoning_tokens).filter(|r| *r > 0);
    let has_answer = inp.parts.iter().any(|p| matches!(p, Part::Text { text, .. } if !text.trim().is_empty()));
    let mut tokens_in_title = false;
    if !reasoning.is_empty() {
        let what = if kinds.iter().all(|k| *k == ReasoningKind::Summary) { "Reasoning summary" } else { "Reasoning" };
        let mut title = format!("💭 {what}");
        if finished && let Some(r) = reasoning_tokens {
            title.push_str(&format!(" · {} tokens", fmt_tokens(r)));
            tokens_in_title = true;
        }
        let open = inp.status == Status::Generating && !has_answer;
        out.push(RBlock::Details { open, title: RText::plain(title), blocks: ai_blocks(&reasoning, raw, 1) });
    }

    // 3. tool calls
    let tools: Vec<&ToolCall> =
        inp.parts.iter().filter_map(|p| if let Part::ToolCall(t) = p { Some(t) } else { None }).collect();
    if !tools.is_empty() {
        let items = tools
            .iter()
            .map(|t| {
                let mut s = t.name.clone();
                if let Some(st) = &t.status {
                    s.push_str(&format!(" [{st}]"));
                }
                let mut blocks = vec![RBlock::Paragraph(RText::bold(RText::plain(s)))];
                if let Some(i) = &t.input {
                    blocks.push(RBlock::Pre { text: pretty(i), language: "json".into() });
                }
                if let Some(o) = &t.output {
                    blocks.push(RBlock::Pre { text: pretty(o), language: "json".into() });
                }
                RItem { checkbox: None, blocks }
            })
            .collect();
        out.push(RBlock::Details {
            open: false,
            title: RText::plain(format!("🔧 Tool calls · {}", tools.len())),
            blocks: vec![RBlock::List { items }],
        });
    }

    // 4. answer, with generated images in the place the model put them
    //    (and refusals, which are model output but not the answer)
    let mut answer = String::new();
    let mut citations = Vec::new();
    for p in inp.parts {
        match p {
            Part::Text { text, citations: c } => {
                answer.push_str(text);
                citations.extend(c.iter().cloned());
            }
            Part::Image { blob, generated: true, dims, .. } => {
                if !answer.trim().is_empty() {
                    out.extend(ai_blocks(&answer, raw, 0));
                }
                answer.clear();
                out.push(RBlock::Photo { key: blob.sha256.clone(), caption: RText::plain(image_caption(blob, *dims)) });
            }
            Part::Refusal { text } => out.push(RBlock::Details {
                open: true,
                title: RText::plain("⛔ Refusal"),
                blocks: vec![RBlock::Paragraph(RText::plain(text.clone()))],
            }),
            _ => {}
        }
    }
    if !answer.trim().is_empty() {
        out.extend(ai_blocks(&answer, raw, 0));
    }

    // 5. sources
    let mut seen = std::collections::HashSet::new();
    let sources: Vec<_> =
        citations.into_iter().filter(|c| c.url.as_ref().is_some_and(|u| seen.insert(u.clone()))).collect();
    if !sources.is_empty() {
        let items = sources
            .iter()
            .map(|c| {
                let url = c.url.clone().unwrap_or_default();
                let title = c.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| url.clone());
                let link = if omni_markdown::render::is_safe_url(&url) {
                    RText::Url { text: Box::new(RText::plain(title)), url }
                } else {
                    RText::plain(title)
                };
                RItem { checkbox: None, blocks: vec![RBlock::Paragraph(link)] }
            })
            .collect();
        out.push(RBlock::Details {
            open: false,
            title: RText::plain(format!("📚 Sources · {}", sources.len())),
            blocks: vec![RBlock::OrderedList { start: 1, items }],
        });
    }

    // 6. error: the message in the title; only the parts of the error object
    //    that carry information beyond it (metadata, upstream raw error…) inside.
    if let Some(e) = inp.error {
        let title = format!("⚠️ Error: {e}");
        match e.details.as_ref().and_then(error_extra) {
            Some(d) => out.push(RBlock::Details {
                open: false,
                title: RText::plain(title),
                blocks: vec![RBlock::Pre { text: pretty(&d), language: "json".into() }],
            }),
            None => out.push(RBlock::Footer(RText::plain(title))),
        }
    }

    // 7. footers: status and notes, then the summary line
    let mut lines: Vec<String> = Vec::new();
    match &inp.status {
        Status::Retrying { in_secs } => lines.push(format!("🔁 retrying in {in_secs}s — continues in a new message")),
        Status::Interrupted => lines.push("⚠️ interrupted by a bot restart — retrying in a new message".into()),
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
        lines.push(format!("ℹ️ {n}"));
    }
    out.extend(lines.into_iter().map(|l| RBlock::Footer(RText::plain(l))));
    out.extend(inp.attachments.iter().map(|k| RBlock::Document { key: k.clone(), caption: RText::plain("") }));

    let mut f: Vec<String> = Vec::new();
    if inp.status == Status::Generating {
        f.push(format!("⏳ generating · {}s", inp.elapsed.as_secs()))
    }
    if !inp.settings.is_empty() {
        f.extend(inp.settings.iter().cloned());
    } else if !inp.options.is_empty() {
        f.push(inp.options.to_string());
    }
    if inp.continuation {
        f.push("continued".into());
    } else if inp.attempt > 1 {
        f.push(format!("attempt {}/{}", inp.attempt, inp.max_attempts));
    }
    let hidden = match opaque {
        Some(_) if !reasoning.is_empty() => None,
        Some(ReasoningKind::Redacted) => Some("redacted"),
        Some(_) => Some("not shown"),
        None => None,
    };
    if inp.show_usage
        && let Some(u) = inp.usage
    {
        if let Some(x) = u.input_tokens {
            let mut s = format!("in {}", fmt_tokens(x));
            if let Some(c) = u.cached_input_tokens.filter(|c| *c > 0) {
                s.push_str(&format!(" (cached {})", fmt_tokens(c)));
            }
            if let Some(c) = u.cache_write_tokens.filter(|c| *c > 0) {
                s.push_str(&format!(" (cache write {})", fmt_tokens(c)));
            }
            f.push(s);
        }
        if let Some(x) = u.output_tokens {
            let mut s = format!("out {}", fmt_tokens(x));
            match (reasoning_tokens, tokens_in_title, hidden) {
                (Some(r), false, Some(h)) => s.push_str(&format!(" (reasoning {}, {h})", fmt_tokens(r))),
                (Some(r), false, None) => s.push_str(&format!(" (reasoning {})", fmt_tokens(r))),
                _ => {}
            }
            f.push(s);
        }
        if let Some(c) = u.cost_usd {
            f.push(fmt_cost(c));
        }
    }
    if finished && !inp.elapsed.is_zero() {
        f.push(format!("{:.1}s", inp.elapsed.as_secs_f64()));
    }
    if let Some(p) = inp.provider {
        f.push(format!("via {p}"));
    }
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
        out.push(RBlock::Footer(RText::plain(f.join(" · "))));
    }
    out
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

/// What an error object says beyond its message and status. Known shapes
/// (`{"error": {message, code, metadata}, "user_id"}` from OpenRouter, Ark's
/// `{code, message, param, type}`, in-band stream chunks) are reduced to the
/// parts that vary; anything unrecognized is shown whole.
fn error_extra(details: &Value) -> Option<Value> {
    let Some(obj) = details.as_object() else {
        return Some(details.clone());
    };
    // Fields that are the same for every error (or noise) at the top level.
    const OUTER_NOISE: &[&str] =
        &["error", "user_id", "id", "object", "created", "model", "choices", "system_fingerprint"];
    let (err, outer) = match obj.get("error").and_then(Value::as_object) {
        Some(e) => (e, Some(obj)),
        None => (obj, None),
    };
    if !err.contains_key("message") {
        return Some(details.clone());
    }
    let mut extra = serde_json::Map::new();
    if let Some(o) = outer {
        for (k, v) in o {
            if !OUTER_NOISE.contains(&k.as_str()) {
                extra.insert(k.clone(), v.clone());
            }
        }
    }
    for (k, v) in err {
        match (k.as_str(), v) {
            ("message", _) => {}
            ("code", Value::Number(_)) => {} // the HTTP status, already in the title
            ("metadata", Value::Object(m)) => {
                for (mk, mv) in m {
                    // `raw` is often the upstream error as a JSON string: show it parsed.
                    let mv = match mv.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                        Some(parsed) if parsed.is_object() || parsed.is_array() => parsed,
                        _ => mv.clone(),
                    };
                    extra.insert(mk.clone(), mv);
                }
            }
            (_, Value::Null) => {}
            _ => {
                extra.insert(k.clone(), v.clone());
            }
        }
    }
    (!extra.is_empty()).then_some(Value::Object(extra))
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
        RText::Italic(x) | RText::Strike(x) | RText::Fixed(x) | RText::Spoiler(x) => write_text(f, x),
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
            mode: Mode::Clean,
            raw_ai: false,
            hops: 0,
            continuation: false,
            attachments: &[],
            settings: &[],
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
        let blocks = compose(&i);
        // Header: model only (no alias), no underline/italic.
        assert!(matches!(&blocks[0], RBlock::Paragraph(t) if t.to_plain() == "🤖 anthropic/claude-opus-5.5"));
        let dbg = format!("{blocks:?}");
        assert!(!dbg.contains("Underline") && !dbg.contains("Italic"), "{dbg}");
        // Reasoning collapsed, with its token count.
        assert!(
            matches!(&blocks[1], RBlock::Details { open: false, title, .. } if title.to_plain() == "💭 Reasoning · 40 tokens")
        );
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
                assert!(f.contains("+s") && f.contains("out 50") && !f.contains("reasoning"), "{f}");
            }
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn errors_and_notes_are_never_plain_paragraphs() {
        let parts = vec![Part::text("partial")];
        let mut i = input(&parts, Status::Retrying { in_secs: 10 });
        let mut e = LlmError::request("boom");
        e.details = Some(serde_json::json!({"error": {"message": "boom"}}));
        i.error = Some(&e);
        let notes = vec!["a note".to_string()];
        i.notes = &notes;
        let blocks = compose(&i);
        let tail = &blocks[2..];
        assert!(tail.iter().all(|b| matches!(b, RBlock::Details { .. } | RBlock::Footer(_))), "{tail:?}");
        // Nothing beyond the message: a small-text line.
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
    fn error_details_keep_only_what_varies() {
        let or = serde_json::json!({"error": {"message": "bad temp", "code": 400}, "user_id": "user_x"});
        assert_eq!(error_extra(&or), None);
        let or2 = serde_json::json!({"error": {"message": "upstream", "code": 502, "metadata": {"provider_name": "Google", "raw": "{\"error\":{\"status\":\"UNAVAILABLE\"}}"}}, "user_id": "u"});
        assert_eq!(
            error_extra(&or2),
            Some(serde_json::json!({"provider_name": "Google", "raw": {"error": {"status": "UNAVAILABLE"}}}))
        );
        let ark = serde_json::json!({"code": "QuotaExceeded", "message": "m", "param": "", "type": "TooManyRequests"});
        assert_eq!(
            error_extra(&ark),
            Some(serde_json::json!({"code": "QuotaExceeded", "param": "", "type": "TooManyRequests"}))
        );
        assert_eq!(error_extra(&serde_json::json!("plain text")), Some(serde_json::json!("plain text")));
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
        assert!(
            matches!(&pages[1][0], RBlock::Paragraph(t) if t.to_plain() == "🤖 anthropic/claude-opus-5.5 · part 2")
        );
    }
}
