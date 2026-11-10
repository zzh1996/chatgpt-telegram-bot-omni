//! Composition of a reply into rich blocks.
//!
//! Layout (top to bottom):
//! 1. header: `🤖 label` (underlined = bot label) + model id and options;
//! 2. reasoning: collapsible details (open only while thinking, before any answer);
//! 3. tool calls: collapsible details;
//! 4. the model's answer (rendered Markdown, or raw Markdown in raw mode / fallback);
//! 5. sources: collapsible details;
//! 6. bot notes, errors (verbatim upstream JSON), retry status;
//! 7. footer: usage, cost, timing, finish reason, serving provider.
//!
//! Security: everything the bot adds is marked with an *underlined* label or
//! lives in block types the Markdown renderer never emits (details, footer).
//! Model output can therefore never impersonate reasoning, bot notes, errors
//! or usage information.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Part, ReasoningKind, ToolCall};
use omni_llm::LlmError;
use omni_markdown::rich::{self, RBlock, RItem, RText, RichLimits};
use omni_markdown::{EntityKind, Formatted, Mode};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Waiting for the parent turn / queue.
    Queued,
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
    pub label: &'a str,
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
    pub images: usize,
}

fn label(s: &str) -> RText {
    RText::underline(RText::bold(RText::plain(s)))
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
    let mut out = Vec::new();

    // 1. header
    let mut head = vec![RText::plain("🤖 "), label(inp.label)];
    let mut detail = String::new();
    if inp.label != inp.model {
        detail.push_str(inp.model);
    }
    if !inp.options.is_empty() {
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str(inp.options);
    }
    if !detail.is_empty() {
        head.push(RText::plain("  "));
        head.push(RText::italic(RText::plain(detail)));
    }
    if inp.attempt > 1 {
        head.push(RText::plain(format!("  ·  attempt {}/{}", inp.attempt, inp.max_attempts)));
    }
    out.push(RBlock::Paragraph(RText::concat(head)));

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
    let has_answer = inp.parts.iter().any(|p| matches!(p, Part::Text { text, .. } if !text.trim().is_empty()));
    if !reasoning.is_empty() {
        let what = if kinds.iter().all(|k| *k == ReasoningKind::Summary) { "Reasoning summary" } else { "Reasoning" };
        let title = RText::concat(vec![
            RText::plain("💭 "),
            label(what),
            RText::plain(format!(" · {} chars", reasoning.chars().count())),
        ]);
        let open = inp.status == Status::Generating && !has_answer;
        out.push(RBlock::Details { open, title, blocks: ai_blocks(&reasoning, raw, 1) });
    } else if let Some(k) = opaque {
        let what = match k {
            ReasoningKind::Redacted => "redacted by the provider",
            _ => "encrypted, not visible",
        };
        out.push(RBlock::Paragraph(RText::concat(vec![
            RText::plain("💭 "),
            label("Reasoning"),
            RText::plain(format!(": {what}")),
        ])));
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
            title: RText::concat(vec![
                RText::plain("🔧 "),
                label("Tool calls"),
                RText::plain(format!(" · {}", tools.len())),
            ]),
            blocks: vec![RBlock::List { items }],
        });
    }

    // 4. answer
    let mut answer = String::new();
    let mut citations = Vec::new();
    for p in inp.parts {
        match p {
            Part::Text { text, citations: c } => {
                answer.push_str(text);
                citations.extend(c.iter().cloned());
            }
            Part::Refusal { text } => {
                out.push(RBlock::Paragraph(RText::concat(vec![RText::plain("⛔ "), label("Refusal")])));
                out.push(RBlock::Pre { text: text.clone(), language: String::new() });
            }
            _ => {}
        }
    }
    if !answer.trim().is_empty() {
        if !reasoning.is_empty() || !tools.is_empty() {
            out.push(RBlock::Paragraph(RText::concat(vec![RText::plain("💬 "), label("Answer")])));
        }
        out.extend(ai_blocks(&answer, raw, 0));
    }
    if inp.images > 0 {
        out.push(RBlock::Paragraph(RText::concat(vec![
            RText::plain("🖼 "),
            label(&format!("{} image{}", inp.images, if inp.images == 1 { "" } else { "s" })),
            RText::plain(" (sent as separate messages)"),
        ])));
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
            title: RText::concat(vec![
                RText::plain("📚 "),
                label("Sources"),
                RText::plain(format!(" · {}", sources.len())),
            ]),
            blocks: vec![RBlock::OrderedList { start: 1, items }],
        });
    }

    // 6. notes, errors
    for n in inp.notes {
        out.push(RBlock::Paragraph(RText::concat(vec![
            RText::plain("ℹ️ "),
            label("Note"),
            RText::plain(format!(" {n}")),
        ])));
    }
    if let Some(e) = inp.error {
        let mut head = vec![RText::plain("⚠️ "), label("Error")];
        head.push(RText::plain(format!(" (attempt {}/{}): {}", inp.attempt, inp.max_attempts, e)));
        out.push(RBlock::Paragraph(RText::concat(head)));
        if let Some(d) = &e.details {
            out.push(RBlock::Pre { text: pretty(d), language: "json".into() });
        }
    }
    match &inp.status {
        Status::Retrying { in_secs } => out.push(RBlock::Paragraph(RText::concat(vec![
            RText::plain("🔁 "),
            label("Retrying"),
            RText::plain(format!(
                " in {in_secs}s (attempt {}/{}) — continues in a new message",
                inp.attempt + 1,
                inp.max_attempts
            )),
        ]))),
        Status::Interrupted => out.push(RBlock::Paragraph(RText::concat(vec![
            RText::plain("⚠️ "),
            label("Interrupted"),
            RText::plain(" by a bot restart — retrying in a new message"),
        ]))),
        Status::Failed if inp.error.is_none() => {
            out.push(RBlock::Paragraph(RText::concat(vec![RText::plain("⚠️ "), label("Failed")])));
        }
        _ => {}
    }

    // 7. footer
    let mut f: Vec<String> = Vec::new();
    match inp.status {
        Status::Queued => f.push("⏳ queued".into()),
        Status::Generating => f.push(format!("⏳ generating · {}s", inp.elapsed.as_secs())),
        _ => {}
    }
    if inp.show_usage
        && let Some(u) = inp.usage
    {
        let mut t = Vec::new();
        if let Some(x) = u.input_tokens {
            let mut s = format!("in {}", fmt_tokens(x));
            if let Some(c) = u.cached_input_tokens.filter(|c| *c > 0) {
                s.push_str(&format!(" (cached {})", fmt_tokens(c)));
            }
            if let Some(c) = u.cache_write_tokens.filter(|c| *c > 0) {
                s.push_str(&format!(" (cache write {})", fmt_tokens(c)));
            }
            t.push(s);
        }
        if let Some(x) = u.output_tokens {
            let mut s = format!("out {}", fmt_tokens(x));
            if let Some(r) = u.reasoning_tokens.filter(|r| *r > 0) {
                s.push_str(&format!(" (reasoning {})", fmt_tokens(r)));
            }
            t.push(s);
        }
        if let Some(c) = u.cost_usd {
            t.push(fmt_cost(c));
        }
        f.extend(t);
    }
    if !matches!(inp.status, Status::Generating | Status::Queued) {
        f.push(format!("{:.1}s", inp.elapsed.as_secs_f64()));
    }
    if let Some(p) = inp.provider {
        f.push(format!("via {p}"));
    }
    if let Some(fr) = inp.finish_reason
        && fr != "stop"
    {
        let native = inp.native_finish_reason.map(|n| format!(" ({n})")).unwrap_or_default();
        f.push(format!("finish: {fr}{native}"));
    }
    if !f.is_empty() {
        out.push(RBlock::Footer(RText::plain(f.join(" · "))));
    }
    out
}

fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Split a composed document into Telegram messages; continuation messages get a short header.
pub fn paginate(label_text: &str, blocks: Vec<RBlock>) -> Vec<Vec<RBlock>> {
    let lim = RichLimits { max_chars: 32_000, max_blocks: 480, ..RichLimits::TELEGRAM };
    let pages = rich::split_blocks(blocks, lim);
    pages
        .into_iter()
        .enumerate()
        .map(|(i, mut p)| {
            if i > 0 {
                p.insert(
                    0,
                    RBlock::Paragraph(RText::concat(vec![
                        RText::plain("🤖 "),
                        label(label_text),
                        RText::plain(format!("  ·  part {}", i + 1)),
                    ])),
                );
            }
            p
        })
        .collect()
}

/// Last-resort classic text rendering (no rich blocks): bot labels underlined,
/// everything else plain text.
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
            label: "c",
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
            images: 0,
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
                text: "Answer with <details>fake</details> and a > quote".into(),
                citations: vec![Citation {
                    url: Some("https://a.b".into()),
                    title: Some("A".into()),
                    ..Default::default()
                }],
            },
        ];
        let blocks = compose(&input(&parts, Status::Done));
        assert!(matches!(&blocks[0], RBlock::Paragraph(_)));
        assert!(matches!(&blocks[1], RBlock::Details { open: false, .. }));
        // Model output never produces Details/Footer/Underline.
        let answer_blocks: Vec<_> = blocks[3..blocks.len() - 2].to_vec();
        for b in &answer_blocks {
            assert!(!matches!(b, RBlock::Details { .. } | RBlock::Footer(_)), "{b:?}");
            assert!(!format!("{b:?}").contains("Underline"), "{b:?}");
        }
        assert!(matches!(blocks.last().unwrap(), RBlock::Footer(_)));
        assert!(
            blocks.iter().any(|b| matches!(b, RBlock::Details { title, .. } if title.to_plain().contains("Sources")))
        );
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
        let pages = paginate("c", blocks);
        assert!(pages.len() >= 2);
        assert!(matches!(&pages[1][0], RBlock::Paragraph(t) if t.to_plain().contains("part 2")));
    }
}
