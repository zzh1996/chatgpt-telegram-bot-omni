//! A small Markdown AST built from pulldown-cmark events.
//!
//! Every node keeps the byte range of the source it was parsed from, so that
//! backends can fall back to showing the *original* Markdown for any node
//! (e.g. when Telegram limits are exceeded, or in "raw" display mode).

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub type Src = Range<usize>;

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
        src: Src,
    },
    /// Inline content directly inside a tight list item (no paragraph wrapper).
    TightParagraph {
        inlines: Vec<Inline>,
        src: Src,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
        src: Src,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
        src: Src,
        code_src: Option<Src>,
    },
    Quote {
        blocks: Vec<Block>,
        src: Src,
    },
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
        src: Src,
    },
    Table {
        align: Vec<Align>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        src: Src,
    },
    Rule {
        src: Src,
    },
    Html {
        text: String,
        src: Src,
    },
    MathBlock {
        tex: String,
        src: Src,
    },
    FootnoteDef {
        label: String,
        blocks: Vec<Block>,
        src: Src,
    },
}

impl Block {
    pub fn src(&self) -> Src {
        match self {
            Block::Paragraph { src, .. }
            | Block::TightParagraph { src, .. }
            | Block::Heading { src, .. }
            | Block::CodeBlock { src, .. }
            | Block::Quote { src, .. }
            | Block::List { src, .. }
            | Block::Table { src, .. }
            | Block::Rule { src }
            | Block::Html { src, .. }
            | Block::MathBlock { src, .. }
            | Block::FootnoteDef { src, .. } => src.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
    pub src: Src,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text { text: String, src: Src },
    Code { code: String, src: Src },
    Math { tex: String, display: bool, src: Src },
    Emph { children: Vec<Inline>, src: Src },
    Strong { children: Vec<Inline>, src: Src },
    Strike { children: Vec<Inline>, src: Src },
    Link { url: String, title: String, children: Vec<Inline>, src: Src },
    Image { url: String, title: String, alt: Vec<Inline>, src: Src },
    Html { html: String, src: Src },
    FootnoteRef { label: String, src: Src },
    SoftBreak { src: Src },
    HardBreak { src: Src },
}

impl Inline {
    pub fn src(&self) -> Src {
        match self {
            Inline::Text { src, .. }
            | Inline::Code { src, .. }
            | Inline::Math { src, .. }
            | Inline::Emph { src, .. }
            | Inline::Strong { src, .. }
            | Inline::Strike { src, .. }
            | Inline::Link { src, .. }
            | Inline::Image { src, .. }
            | Inline::Html { src, .. }
            | Inline::FootnoteRef { src, .. }
            | Inline::SoftBreak { src }
            | Inline::HardBreak { src } => src.clone(),
        }
    }

    /// Concatenated plain text of this node (formatting removed).
    pub fn plain_text(&self, out: &mut String) {
        match self {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Code { code, .. } => out.push_str(code),
            Inline::Math { tex, .. } => out.push_str(tex),
            Inline::Emph { children, .. }
            | Inline::Strong { children, .. }
            | Inline::Strike { children, .. }
            | Inline::Link { children, .. } => {
                for c in children {
                    c.plain_text(out);
                }
            }
            Inline::Image { alt, .. } => {
                for c in alt {
                    c.plain_text(out);
                }
            }
            Inline::Html { html, .. } => out.push_str(html),
            Inline::FootnoteRef { label, .. } => {
                out.push('[');
                out.push_str(label);
                out.push(']');
            }
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => out.push('\n'),
        }
    }
}

pub fn plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        i.plain_text(&mut s);
    }
    s
}

/// Parser options used everywhere. Smart punctuation is deliberately OFF:
/// we must never alter the characters the model produced.
pub fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
}

type Ev<'a> = (Event<'a>, Src);

struct Events<'a> {
    inner: std::iter::Peekable<std::vec::IntoIter<Ev<'a>>>,
}

impl<'a> Events<'a> {
    fn next(&mut self) -> Option<Ev<'a>> {
        self.inner.next()
    }
    fn peek(&mut self) -> Option<&Ev<'a>> {
        self.inner.peek()
    }
}

pub fn parse(src: &str) -> Vec<Block> {
    let math = latex_delimiters(src);
    let text = math.as_ref().map_or(src, |m| m.text.as_str());
    let events: Vec<Ev<'_>> = Parser::new_ext(text, options())
        .into_offset_iter()
        .map(|(e, r)| match &math {
            // Ranges point into the model's own text, not the rewritten one.
            Some(m) => (e, m.original(r.start, false)..m.original(r.end, true)),
            None => (e, r),
        })
        .collect();
    let mut ev = Events { inner: events.into_iter().peekable() };
    let blocks = parse_blocks(&mut ev, None, 0);
    normalize_display_math(blocks)
}

/// The text with LaTeX's own math delimiters rewritten for the parser, and the
/// way back to the original byte offsets.
struct Rewritten {
    text: String,
    /// `(new offset, original offset, new length, original length)` of every
    /// piece, in order: copied pieces have equal lengths, rewritten delimiters not.
    pieces: Vec<(usize, usize, usize, usize)>,
}

impl Rewritten {
    /// The original offset of `pos` in the rewritten text. Inside a rewritten
    /// formula: its start, or its end for the end of a range.
    fn original(&self, pos: usize, end: bool) -> usize {
        let i = self.pieces.partition_point(|p| p.0 <= pos).saturating_sub(1);
        let Some(&(new, orig, new_len, orig_len)) = self.pieces.get(i) else { return pos };
        if new_len == orig_len {
            orig + (pos - new).min(orig_len)
        } else if pos == new || !end {
            orig
        } else {
            orig + orig_len
        }
    }
}

/// Characters that make a `\[ … \]` body math rather than an escaped bracket.
const MATHY: &[char] = &['\\', '^', '_', '=', '{', '}', '+', '<', '>', '|'];

/// LaTeX's own delimiters, `\( … \)` for inline math and `\[ … \]` for display
/// math, which OpenAI, DeepSeek, Qwen, Grok and Doubao models write (seen
/// 2026-11-14), are CommonMark escapes of brackets: they are rewritten to
/// `$…$` and `$$…$$` so that the math extension takes them (outside code; an
/// inline formula's inner spaces trimmed, which `$` needs). `\[ … \]` only
/// when the body looks like math, so that an escaped bracket like `\[1\]`
/// stays one. `None`: nothing to rewrite.
fn latex_delimiters(src: &str) -> Option<Rewritten> {
    if !src.contains("\\(") && !src.contains("\\[") {
        return None;
    }
    // Code spans and blocks (and raw HTML) keep their text.
    let mut code: Vec<Range<usize>> = Vec::new();
    let mut depth = 0usize;
    for (e, r) in Parser::new_ext(src, options()).into_offset_iter() {
        match e {
            Event::Code(_) | Event::InlineHtml(_) | Event::Html(_) if depth == 0 => code.push(r),
            Event::Start(Tag::CodeBlock(_)) => {
                if depth == 0 {
                    code.push(r);
                }
                depth += 1;
            }
            Event::End(TagEnd::CodeBlock) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let in_code = |i: usize| code.iter().any(|r| r.contains(&i));
    let bytes = src.as_bytes();
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let (close, display) = match bytes[i + 1] {
            b'(' => (b')', false),
            b'[' => (b']', true),
            // An escaped backslash, or another escape.
            _ => {
                i += 2;
                continue;
            }
        };
        if in_code(i) {
            i += 2;
            continue;
        }
        let start = i + 2;
        let Some(len) = closing(&bytes[start..], close) else {
            i += 2;
            continue;
        };
        let body = &src[start..start + len];
        let end = start + len + 2;
        let ok = !body.trim().is_empty()
            && !body.contains('$')
            && !body.contains("\n\n")
            && !body.contains("\r\n\r\n")
            && (!display || body.contains('\n') || body.contains(MATHY))
            && !in_code(end - 1);
        if !ok {
            i += 2;
            continue;
        }
        let new = if display { format!("$${body}$$") } else { format!("${}$", body.trim_matches([' ', '\t'])) };
        edits.push((i..end, new));
        i = end;
    }
    if edits.is_empty() {
        return None;
    }
    let mut text = String::with_capacity(src.len());
    let mut pieces = Vec::new();
    let mut at = 0;
    for (r, new) in edits {
        if r.start > at {
            pieces.push((text.len(), at, r.start - at, r.start - at));
            text.push_str(&src[at..r.start]);
        }
        pieces.push((text.len(), r.start, new.len(), r.end - r.start));
        text.push_str(&new);
        at = r.end;
    }
    pieces.push((text.len(), at, src.len() - at, src.len() - at));
    text.push_str(&src[at..]);
    Some(Rewritten { text, pieces })
}

/// The length of a body up to its closing `\)` or `\]` (escaped backslashes
/// skipped), if there is one.
fn closing(rest: &[u8], close: u8) -> Option<usize> {
    let mut j = 0;
    while j + 1 < rest.len() {
        if rest[j] == b'\\' {
            if rest[j + 1] == close {
                return Some(j);
            }
            j += 2;
        } else {
            j += 1;
        }
    }
    None
}

fn is_inline_start(e: &Event<'_>) -> bool {
    match e {
        Event::Start(t) => matches!(
            t,
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ),
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::InlineHtml(_)
        | Event::FootnoteReference(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::TaskListMarker(_) => true,
        // `Html` events outside an HtmlBlock are inline-ish; treat them as inline.
        Event::Html(_) => true,
        _ => false,
    }
}

/// How deep blocks and inline formatting nest before the rest is kept as
/// plain text: far beyond what renders (the rich conversion stops at 16), and
/// shallow enough that a model writing thousands of `>` or `- ` cannot overflow
/// the stack (each level is a recursion).
const MAX_DEPTH: usize = 64;

/// Everything up to the `End(end)` matching an already consumed start (nested
/// starts of the same kind counted), as plain text, without recursion.
fn text_until_end(ev: &mut Events<'_>, end: TagEnd) -> String {
    let mut depth = 0usize;
    let mut text = String::new();
    while let Some((event, _)) = ev.next() {
        match event {
            Event::Start(t) if t.to_end() == end => depth += 1,
            Event::End(e) if e == end => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::Heading(_) | TagEnd::CodeBlock) => text.push('\n'),
            _ => {}
        }
    }
    text
}

/// Parse blocks until the matching `End(end)` (consumed) or end of input.
#[allow(clippy::while_let_loop)]
fn parse_blocks(ev: &mut Events<'_>, end: Option<TagEnd>, depth: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    loop {
        let Some(peeked) = ev.peek() else { break };
        if let Event::End(e) = &peeked.0
            && Some(*e) == end
        {
            ev.next();
            break;
        }
        // Inline content directly at block level (tight list items).
        if is_inline_start(&peeked.0) && !matches!(peeked.0, Event::TaskListMarker(_)) {
            let start = peeked.1.start;
            let (inlines, src_end) = parse_inline_run(ev, depth);
            if !inlines.is_empty() {
                blocks.push(Block::TightParagraph { inlines, src: start..src_end });
            }
            continue;
        }
        let (event, src) = ev.next().unwrap();
        match event {
            Event::Start(tag) => {
                if let Some(b) = parse_block_tag(ev, tag, src, depth) {
                    blocks.push(b);
                }
            }
            Event::End(_) => {
                // Unbalanced end (should not happen); ignore.
            }
            Event::Rule => blocks.push(Block::Rule { src }),
            Event::TaskListMarker(_) => {}
            other => {
                // Any stray inline event: wrap into a tight paragraph.
                let mut inlines = Vec::new();
                push_simple_inline(&mut inlines, other, src.clone());
                blocks.push(Block::TightParagraph { inlines, src });
            }
        }
    }
    blocks
}

/// Consume a run of inline events at block level (until a block-level event).
fn parse_inline_run(ev: &mut Events<'_>, depth: usize) -> (Vec<Inline>, usize) {
    let mut out = Vec::new();
    let mut last_end = 0;
    while let Some(p) = ev.peek() {
        if !is_inline_start(&p.0) || matches!(p.0, Event::TaskListMarker(_)) {
            break;
        }
        let (event, src) = ev.next().unwrap();
        last_end = last_end.max(src.end);
        match event {
            Event::Start(tag) => {
                let (inl, end) = parse_inline_tag(ev, tag, src, depth);
                last_end = last_end.max(end);
                out.push(inl);
            }
            other => push_simple_inline(&mut out, other, src),
        }
    }
    (out, last_end)
}

fn push_simple_inline(out: &mut Vec<Inline>, event: Event<'_>, src: Src) {
    match event {
        Event::Text(t) => {
            // Merge adjacent text nodes (pulldown splits text at escapes etc.).
            if let Some(Inline::Text { text, src: psrc }) = out.last_mut()
                && (psrc.end == src.start || psrc.end <= src.start)
            {
                text.push_str(&t);
                psrc.end = src.end;
                return;
            }
            out.push(Inline::Text { text: t.into_string(), src })
        }
        Event::Code(t) => out.push(Inline::Code { code: t.into_string(), src }),
        Event::InlineMath(t) => out.push(Inline::Math { tex: t.into_string(), display: false, src }),
        Event::DisplayMath(t) => out.push(Inline::Math { tex: t.into_string(), display: true, src }),
        Event::Html(t) | Event::InlineHtml(t) => out.push(Inline::Html { html: t.into_string(), src }),
        Event::FootnoteReference(l) => out.push(Inline::FootnoteRef { label: l.into_string(), src }),
        Event::SoftBreak => out.push(Inline::SoftBreak { src }),
        Event::HardBreak => out.push(Inline::HardBreak { src }),
        Event::TaskListMarker(_) | Event::Rule | Event::Start(_) | Event::End(_) => {}
    }
}

/// Parse inline children until `End(end)` (consumed). Returns children and max src end.
fn parse_inlines_until(ev: &mut Events<'_>, end: TagEnd, depth: usize) -> (Vec<Inline>, usize) {
    let mut out = Vec::new();
    let mut last_end = 0;
    while let Some((event, src)) = ev.next() {
        last_end = last_end.max(src.end);
        match event {
            Event::End(e) if e == end => break,
            Event::End(_) => {}
            Event::Start(tag) => {
                if is_inline_tag(&tag) {
                    let (inl, e) = parse_inline_tag(ev, tag, src, depth);
                    last_end = last_end.max(e);
                    out.push(inl);
                } else {
                    // A block tag inside inline context should not happen; skip its content.
                    skip_until_end(ev, tag.to_end());
                }
            }
            other => push_simple_inline(&mut out, other, src),
        }
    }
    (out, last_end)
}

fn is_inline_tag(t: &Tag<'_>) -> bool {
    matches!(
        t,
        Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::Link { .. }
            | Tag::Image { .. }
    )
}

fn skip_until_end(ev: &mut Events<'_>, end: TagEnd) {
    let mut depth = 0usize;
    while let Some((event, _)) = ev.next() {
        match event {
            Event::Start(t) if t.to_end() == end => depth += 1,
            Event::End(e) if e == end => {
                if depth == 0 {
                    return;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
}

fn parse_inline_tag(ev: &mut Events<'_>, tag: Tag<'_>, src: Src, depth: usize) -> (Inline, usize) {
    let end = tag.to_end();
    if depth >= MAX_DEPTH {
        let text = text_until_end(ev, end);
        return (Inline::Text { text, src: src.clone() }, src.end);
    }
    let (children, e) = parse_inlines_until(ev, end, depth + 1);
    let src_end = src.end.max(e);
    let src = src.start..src_end;
    let inl = match tag {
        Tag::Emphasis => Inline::Emph { children, src },
        Tag::Strong => Inline::Strong { children, src },
        Tag::Strikethrough => Inline::Strike { children, src },
        // Superscript/subscript are not enabled; keep content as-is.
        Tag::Superscript | Tag::Subscript => Inline::Emph { children, src },
        Tag::Link { dest_url, title, .. } => {
            Inline::Link { url: dest_url.into_string(), title: title.into_string(), children, src }
        }
        Tag::Image { dest_url, title, .. } => {
            Inline::Image { url: dest_url.into_string(), title: title.into_string(), alt: children, src }
        }
        _ => unreachable!("not an inline tag"),
    };
    (inl, src_end)
}

#[allow(clippy::while_let_loop)]
fn parse_block_tag(ev: &mut Events<'_>, tag: Tag<'_>, src: Src, depth: usize) -> Option<Block> {
    // Beyond MAX_DEPTH a nested block is its text (no deeper recursion).
    if depth >= MAX_DEPTH && matches!(tag, Tag::BlockQuote(_) | Tag::List(_) | Tag::FootnoteDefinition(_)) {
        let text = text_until_end(ev, tag.to_end());
        let inlines = vec![Inline::Text { text, src: src.clone() }];
        return Some(Block::Paragraph { inlines, src });
    }
    match tag {
        Tag::Paragraph => {
            let (inlines, _) = parse_inlines_until(ev, TagEnd::Paragraph, depth);
            Some(Block::Paragraph { inlines, src })
        }
        Tag::Heading { level, .. } => {
            let (inlines, _) = parse_inlines_until(ev, TagEnd::Heading(level), depth);
            let level = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };
            Some(Block::Heading { level, inlines, src })
        }
        Tag::CodeBlock(kind) => {
            let lang = match kind {
                CodeBlockKind::Fenced(info) => {
                    let l = info.split_whitespace().next().unwrap_or("").to_string();
                    if l.is_empty() { None } else { Some(l) }
                }
                CodeBlockKind::Indented => None,
            };
            let mut code = String::new();
            let mut code_src: Option<Src> = None;
            while let Some((event, esrc)) = ev.next() {
                match event {
                    Event::End(TagEnd::CodeBlock) => break,
                    Event::Text(t) => {
                        code.push_str(&t);
                        code_src = Some(match code_src {
                            None => esrc,
                            Some(r) => r.start.min(esrc.start)..r.end.max(esrc.end),
                        });
                    }
                    _ => {}
                }
            }
            Some(Block::CodeBlock { lang, code, src, code_src })
        }
        Tag::BlockQuote(kind) => {
            let blocks = parse_blocks(ev, Some(TagEnd::BlockQuote(kind)), depth + 1);
            Some(Block::Quote { blocks, src })
        }
        Tag::HtmlBlock => {
            let mut text = String::new();
            while let Some((event, _)) = ev.next() {
                match event {
                    Event::End(TagEnd::HtmlBlock) => break,
                    Event::Html(t) | Event::Text(t) => text.push_str(&t),
                    _ => {}
                }
            }
            Some(Block::Html { text, src })
        }
        Tag::List(start) => {
            let ordered = start.is_some();
            let mut items = Vec::new();
            loop {
                let Some((event, isrc)) = ev.next() else { break };
                match event {
                    Event::End(TagEnd::List(o)) if o == ordered => break,
                    Event::Start(Tag::Item) => {
                        let mut task = None;
                        if let Some((Event::TaskListMarker(_), _)) = ev.peek()
                            && let Some((Event::TaskListMarker(c), _)) = ev.next()
                        {
                            task = Some(c);
                        }
                        // Loose items begin with a paragraph; the marker may also follow the paragraph start.
                        let mut blocks = parse_blocks(ev, Some(TagEnd::Item), depth + 1);
                        if task.is_none() {
                            task = extract_task_marker(&mut blocks);
                        }
                        items.push(ListItem { task, blocks, src: isrc });
                    }
                    _ => {}
                }
            }
            Some(Block::List { start, items, src })
        }
        Tag::FootnoteDefinition(label) => {
            let blocks = parse_blocks(ev, Some(TagEnd::FootnoteDefinition), depth + 1);
            Some(Block::FootnoteDef { label: label.into_string(), blocks, src })
        }
        Tag::Table(aligns) => {
            let align = aligns
                .into_iter()
                .map(|a| match a {
                    Alignment::None => Align::None,
                    Alignment::Left => Align::Left,
                    Alignment::Center => Align::Center,
                    Alignment::Right => Align::Right,
                })
                .collect();
            let mut header = Vec::new();
            let mut rows = Vec::new();
            let mut current_row: Option<Vec<Vec<Inline>>> = None;
            let mut in_head = false;
            while let Some((event, _)) = ev.next() {
                match event {
                    Event::End(TagEnd::Table) => break,
                    Event::Start(Tag::TableHead) => {
                        in_head = true;
                        current_row = Some(Vec::new());
                    }
                    Event::End(TagEnd::TableHead) => {
                        in_head = false;
                        header = current_row.take().unwrap_or_default();
                    }
                    Event::Start(Tag::TableRow) => current_row = Some(Vec::new()),
                    Event::End(TagEnd::TableRow) => {
                        if let Some(r) = current_row.take() {
                            if in_head {
                                header = r;
                            } else {
                                rows.push(r);
                            }
                        }
                    }
                    Event::Start(Tag::TableCell) => {
                        let (cell, _) = parse_inlines_until(ev, TagEnd::TableCell, depth);
                        current_row.get_or_insert_with(Vec::new).push(cell);
                    }
                    _ => {}
                }
            }
            Some(Block::Table { align, header, rows, src })
        }
        Tag::MetadataBlock(_) | Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
            // Not enabled; consume defensively.
            skip_until_end(ev, tag.to_end());
            None
        }
        Tag::Item | Tag::TableHead | Tag::TableRow | Tag::TableCell => {
            skip_until_end(ev, tag.to_end());
            None
        }
        t if is_inline_tag(&t) => {
            let (inl, end) = parse_inline_tag(ev, t, src.clone(), depth);
            Some(Block::TightParagraph { inlines: vec![inl], src: src.start..end })
        }
        _ => None,
    }
}

/// In loose lists the task marker is emitted inside the first paragraph.
fn extract_task_marker(_blocks: &mut [Block]) -> Option<bool> {
    // pulldown-cmark emits TaskListMarker as the first event of the item (before Paragraph)
    // for both tight and loose lists, which `parse_block_tag` handles. Kept for clarity.
    None
}

/// A paragraph that only contains display math becomes a math block.
fn normalize_display_math(blocks: Vec<Block>) -> Vec<Block> {
    blocks
        .into_iter()
        .map(|b| match b {
            Block::Paragraph { inlines, src } | Block::TightParagraph { inlines, src }
                if is_only_display_math(&inlines) =>
            {
                let tex = inlines
                    .iter()
                    .find_map(|i| match i {
                        Inline::Math { tex, display: true, .. } => Some(tex.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                Block::MathBlock { tex, src }
            }
            Block::Quote { blocks, src } => Block::Quote { blocks: normalize_display_math(blocks), src },
            Block::List { start, items, src } => Block::List {
                start,
                items: items
                    .into_iter()
                    .map(|it| ListItem { task: it.task, blocks: normalize_display_math(it.blocks), src: it.src })
                    .collect(),
                src,
            },
            Block::FootnoteDef { label, blocks, src } => {
                Block::FootnoteDef { label, blocks: normalize_display_math(blocks), src }
            }
            other => other,
        })
        .collect()
}

fn is_only_display_math(inlines: &[Inline]) -> bool {
    let mut seen = false;
    for i in inlines {
        match i {
            Inline::Math { display: true, .. } if !seen => seen = true,
            Inline::Text { text, .. } if text.trim().is_empty() => {}
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => {}
            _ => return false,
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_nesting_cannot_overflow_the_stack() {
        // What a model can be made to write: thousands of `>` or `- `, nested
        // emphasis. Parsed and converted on a thread with Tokio's 2 MiB stack.
        let inputs = [
            format!("{} deep quote", ">".repeat(5000)),
            format!("{} deep list", "- ".repeat(5000)),
            format!("{}deep{}", "*_".repeat(2000), "_*".repeat(2000)),
        ];
        let handle = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                inputs
                    .iter()
                    .map(|md| {
                        let blocks = parse(md);
                        let rich = crate::rich::markdown_to_blocks(md, 0, crate::rich::RichLimits::TELEGRAM);
                        (format!("{blocks:?}").contains("deep"), rich.len())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap();
        for (kept, n) in handle.join().expect("no stack overflow") {
            assert!(kept, "the deepest text is kept as plain text");
            assert!(n > 0);
        }
    }

    #[test]
    fn parses_basic_structure() {
        let md = "# Title\n\nHello **bold** and `code`.\n\n- a\n- b\n\n```rust\nfn x() {}\n```\n";
        let blocks = parse(md);
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Paragraph { .. }));
        assert!(matches!(blocks[2], Block::List { .. }));
        match &blocks[3] {
            Block::CodeBlock { lang, code, code_src, .. } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert_eq!(code, "fn x() {}\n");
                let r = code_src.clone().unwrap();
                assert_eq!(&md[r], "fn x() {}\n");
            }
            b => panic!("unexpected {b:?}"),
        }
    }

    #[test]
    fn task_list_markers() {
        let blocks = parse("- [ ] todo\n- [x] done\n");
        match &blocks[0] {
            Block::List { items, .. } => {
                assert_eq!(items[0].task, Some(false));
                assert_eq!(items[1].task, Some(true));
            }
            b => panic!("unexpected {b:?}"),
        }
    }

    #[test]
    fn display_math_block() {
        let blocks = parse("$$\nx^2\n$$\n");
        assert!(matches!(blocks[0], Block::MathBlock { .. }), "{blocks:?}");
    }

    #[test]
    fn unclosed_fence_is_code_until_end() {
        let blocks = parse("```python\nprint(1)\n");
        match &blocks[0] {
            Block::CodeBlock { code, .. } => assert_eq!(code, "print(1)\n"),
            b => panic!("unexpected {b:?}"),
        }
    }

    fn math_of(inlines: &[Inline]) -> Vec<(String, bool)> {
        inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Math { tex, display, .. } => Some((tex.clone(), *display)),
                _ => None,
            })
            .collect()
    }

    /// LaTeX's own delimiters, as chat-latest writes them (2026-11-14).
    #[test]
    fn latex_delimiters_are_math() {
        let md = "We are given:\n\n\\[\nx^2 - 5x + 6 = 0\n\\]\n\nComparing with \\(ax^2 + bx + c = 0\\):\n\n- \\(a = 1\\)\n- \\( b = -5 \\)\n";
        let blocks = parse(md);
        match &blocks[1] {
            Block::MathBlock { tex, src } => {
                assert_eq!(tex.trim(), "x^2 - 5x + 6 = 0");
                // Ranges are the model's own text.
                assert_eq!(md[src.clone()].trim_end(), "\\[\nx^2 - 5x + 6 = 0\n\\]");
            }
            b => panic!("unexpected {b:?}"),
        }
        match &blocks[2] {
            Block::Paragraph { inlines, src } => {
                assert_eq!(math_of(inlines), [("ax^2 + bx + c = 0".to_string(), false)]);
                assert_eq!(md[src.clone()].trim_end(), "Comparing with \\(ax^2 + bx + c = 0\\):");
                let Inline::Math { src: m, .. } = &inlines[1] else { panic!("{inlines:?}") };
                assert_eq!(&md[m.clone()], "\\(ax^2 + bx + c = 0\\)");
            }
            b => panic!("unexpected {b:?}"),
        }
        let Block::List { items, .. } = &blocks[3] else { panic!("{:?}", blocks[3]) };
        let tex: Vec<_> = items
            .iter()
            .flat_map(|it| match &it.blocks[0] {
                Block::TightParagraph { inlines, .. } => math_of(inlines),
                b => panic!("unexpected {b:?}"),
            })
            .collect();
        assert_eq!(tex, [("a = 1".to_string(), false), ("b = -5".to_string(), false)]);
        // Underscores and stars inside stay math, not emphasis.
        let blocks = parse("\\(a_1 * b_2 * c\\) and \\[\\sum_{i=1}^n i\\]");
        let Block::Paragraph { inlines, .. } = &blocks[0] else { panic!("{blocks:?}") };
        assert_eq!(math_of(inlines), [("a_1 * b_2 * c".to_string(), false), ("\\sum_{i=1}^n i".to_string(), true)]);
    }

    #[test]
    fn what_stays_text() {
        let text = |md: &str| match &parse(md)[..] {
            [Block::Paragraph { inlines, .. }] => (plain_text(inlines), math_of(inlines).len()),
            b => panic!("unexpected {b:?}"),
        };
        // Escaped brackets that are not math, unclosed or empty delimiters, escaped backslashes.
        assert_eq!(text("see \\[1\\] and \\[note\\]"), ("see [1] and [note]".to_string(), 0));
        assert_eq!(text("a \\(b"), ("a (b".to_string(), 0));
        assert_eq!(text("a \\( \\) b"), ("a ( ) b".to_string(), 0));
        assert_eq!(text("a \\\\(b\\\\) c"), ("a \\(b\\) c".to_string(), 0));
        // A dollar inside: left alone.
        assert_eq!(text("\\(\\$5\\)").1, 0);
        // Code keeps its text.
        match &parse("`\\(x\\)` and \\(y\\)")[..] {
            [Block::Paragraph { inlines, .. }] => {
                assert!(matches!(&inlines[0], Inline::Code { code, .. } if code == "\\(x\\)"), "{inlines:?}");
                assert_eq!(math_of(inlines), [("y".to_string(), false)]);
            }
            b => panic!("unexpected {b:?}"),
        }
        match &parse("```\n\\[x^2\\]\n```\n")[..] {
            [Block::CodeBlock { code, .. }] => assert_eq!(code, "\\[x^2\\]\n"),
            b => panic!("unexpected {b:?}"),
        }
        // No paragraph break inside a formula.
        assert_eq!(parse("\\(a\n\nb\\)").len(), 2);
    }
}
