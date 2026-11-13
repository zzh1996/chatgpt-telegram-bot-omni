//! Markdown AST -> Telegram rich-message blocks (mirror types of TL `PageBlock`
//! / `RichText`, converted to TL in the bot crate).
//!
//! Limits (Bot API 10.1, see docs/research/telegram.md §1.2): 32768 characters
//! (code points, incl. formula sources), 500 blocks (incl. list items, table
//! rows, quotes, details), nesting depth 16, 20 table columns.
//!
//! Fallback policy (owner's decision): bot-controlled structure stays rich;
//! any *model-generated* block that cannot be represented is shown as its raw
//! Markdown source in a `markdown` preformatted block — per block, so the rest
//! of the answer keeps its formatting.
//!
//! Anti-spoofing: the Markdown renderer never produces `Underline`, `Sub`,
//! `Super`, `Footer`, `Details` or `CollapsedQuote`; those are reserved for
//! bot-generated structure.

use serde::{Deserialize, Serialize};

use crate::ast::{self, Align, Block, Inline};
use crate::render::is_safe_url;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RText {
    Plain(String),
    Bold(Box<RText>),
    Italic(Box<RText>),
    Underline(Box<RText>),
    Strike(Box<RText>),
    Fixed(Box<RText>),
    Spoiler(Box<RText>),
    /// Smaller text below the line (`textSubscript`). Bot-only.
    Sub(Box<RText>),
    /// Smaller text above the line (`textSuperscript`). Bot-only.
    Super(Box<RText>),
    Url {
        text: Box<RText>,
        url: String,
    },
    Math(String),
    Concat(Vec<RText>),
}

impl RText {
    pub fn plain(s: impl Into<String>) -> Self {
        RText::Plain(s.into())
    }
    pub fn bold(t: RText) -> Self {
        RText::Bold(Box::new(t))
    }
    pub fn italic(t: RText) -> Self {
        RText::Italic(Box::new(t))
    }
    pub fn underline(t: RText) -> Self {
        RText::Underline(Box::new(t))
    }
    pub fn fixed(t: RText) -> Self {
        RText::Fixed(Box::new(t))
    }
    pub fn sub(t: RText) -> Self {
        RText::Sub(Box::new(t))
    }
    pub fn sup(t: RText) -> Self {
        RText::Super(Box::new(t))
    }
    pub fn concat(v: Vec<RText>) -> Self {
        let mut v: Vec<RText> = v.into_iter().filter(|t| !t.is_empty()).collect();
        match v.len() {
            0 => RText::Plain(String::new()),
            1 => v.pop().unwrap(),
            _ => RText::Concat(v),
        }
    }
    pub fn is_empty(&self) -> bool {
        match self {
            RText::Plain(s) => s.is_empty(),
            RText::Math(s) => s.is_empty(),
            RText::Concat(v) => v.iter().all(RText::is_empty),
            RText::Bold(t)
            | RText::Italic(t)
            | RText::Underline(t)
            | RText::Strike(t)
            | RText::Fixed(t)
            | RText::Spoiler(t)
            | RText::Sub(t)
            | RText::Super(t) => t.is_empty(),
            RText::Url { text, .. } => text.is_empty(),
        }
    }
    /// Number of Unicode code points (Telegram counts code points).
    pub fn chars(&self) -> usize {
        match self {
            RText::Plain(s) | RText::Math(s) => s.chars().count(),
            RText::Concat(v) => v.iter().map(RText::chars).sum(),
            RText::Bold(t)
            | RText::Italic(t)
            | RText::Underline(t)
            | RText::Strike(t)
            | RText::Fixed(t)
            | RText::Spoiler(t)
            | RText::Sub(t)
            | RText::Super(t) => t.chars(),
            RText::Url { text, .. } => text.chars(),
        }
    }
    pub fn depth(&self) -> usize {
        match self {
            RText::Plain(_) | RText::Math(_) => 0,
            RText::Concat(v) => v.iter().map(RText::depth).max().unwrap_or(0),
            RText::Bold(t)
            | RText::Italic(t)
            | RText::Underline(t)
            | RText::Strike(t)
            | RText::Fixed(t)
            | RText::Spoiler(t)
            | RText::Sub(t)
            | RText::Super(t) => 1 + t.depth(),
            RText::Url { text, .. } => 1 + text.depth(),
        }
    }
    pub fn to_plain(&self) -> String {
        let mut s = String::new();
        self.write_plain(&mut s);
        s
    }
    fn write_plain(&self, out: &mut String) {
        match self {
            RText::Plain(s) | RText::Math(s) => out.push_str(s),
            RText::Concat(v) => v.iter().for_each(|t| t.write_plain(out)),
            RText::Bold(t)
            | RText::Italic(t)
            | RText::Underline(t)
            | RText::Strike(t)
            | RText::Fixed(t)
            | RText::Spoiler(t)
            | RText::Sub(t)
            | RText::Super(t) => t.write_plain(out),
            RText::Url { text, .. } => text.write_plain(out),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RCell {
    pub text: RText,
    pub header: bool,
    pub align: CellAlign,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RItem {
    /// `Some(checked)` for task-list items.
    pub checkbox: Option<bool>,
    pub blocks: Vec<RBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RBlock {
    Paragraph(RText),
    Heading {
        level: u8,
        text: RText,
    },
    Pre {
        text: String,
        language: String,
    },
    Footer(RText),
    Divider,
    Math(String),
    List {
        items: Vec<RItem>,
    },
    OrderedList {
        start: i32,
        items: Vec<RItem>,
    },
    /// Quotation containing blocks (`pageBlockBlockquoteBlocks`).
    Quote {
        blocks: Vec<RBlock>,
    },
    /// Expandable text-only quotation (`pageBlockBlockquote collapsed`). Bot-only.
    CollapsedQuote {
        text: RText,
    },
    Table {
        rows: Vec<Vec<RCell>>,
        bordered: bool,
        striped: bool,
    },
    /// Collapsible section (`pageBlockDetails`). Bot-only.
    Details {
        open: bool,
        title: RText,
        blocks: Vec<RBlock>,
    },
    /// An inline image (`pageBlockPhoto`). `key` names media the sender
    /// uploads (e.g. a blob hash); never produced from Markdown.
    Photo {
        key: String,
        caption: RText,
    },
    /// An attached file (`pageBlockDocument`). Never produced from Markdown.
    Document {
        key: String,
        caption: RText,
    },
    /// A video, playable in place (`pageBlockVideo`; uploaded like a document).
    Video {
        key: String,
        caption: RText,
    },
    /// An audio file, playable in place (`pageBlockAudio`).
    Audio {
        key: String,
        caption: RText,
    },
}

impl RBlock {
    /// Media keys referenced by this block (photos and documents), recursively.
    pub fn media_keys(&self, out: &mut Vec<String>) {
        match self {
            RBlock::Photo { key, .. }
            | RBlock::Document { key, .. }
            | RBlock::Video { key, .. }
            | RBlock::Audio { key, .. } => {
                if !out.contains(key) {
                    out.push(key.clone());
                }
            }
            RBlock::List { items } | RBlock::OrderedList { items, .. } => {
                items.iter().flat_map(|i| &i.blocks).for_each(|b| b.media_keys(out))
            }
            RBlock::Quote { blocks } | RBlock::Details { blocks, .. } => blocks.iter().for_each(|b| b.media_keys(out)),
            _ => {}
        }
    }
}

/// Media keys referenced by `blocks`, in order of first appearance.
pub fn media_keys(blocks: &[RBlock]) -> Vec<String> {
    let mut out = Vec::new();
    blocks.iter().for_each(|b| b.media_keys(&mut out));
    out
}

/// Size metrics of blocks against rich-message limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    pub chars: usize,
    pub blocks: usize,
    pub depth: usize,
    pub max_table_cols: usize,
    /// Photo/document references.
    pub media: usize,
}

impl Metrics {
    fn add(&mut self, o: Metrics) {
        self.chars += o.chars;
        self.blocks += o.blocks;
        self.depth = self.depth.max(o.depth);
        self.max_table_cols = self.max_table_cols.max(o.max_table_cols);
        self.media += o.media;
    }
}

impl RBlock {
    pub fn metrics(&self) -> Metrics {
        let mut m = Metrics { blocks: 1, depth: 1, ..Default::default() };
        let child = |blocks: &[RBlock], m: &mut Metrics| {
            let mut c = Metrics::default();
            for b in blocks {
                c.add(b.metrics());
            }
            m.chars += c.chars;
            m.blocks += c.blocks;
            m.depth = m.depth.max(1 + c.depth);
            m.max_table_cols = m.max_table_cols.max(c.max_table_cols);
            m.media += c.media;
        };
        match self {
            RBlock::Paragraph(t)
            | RBlock::Footer(t)
            | RBlock::Heading { text: t, .. }
            | RBlock::CollapsedQuote { text: t }
            | RBlock::Photo { caption: t, .. }
            | RBlock::Document { caption: t, .. }
            | RBlock::Video { caption: t, .. }
            | RBlock::Audio { caption: t, .. } => {
                m.chars = t.chars();
                m.depth = 1 + t.depth();
                if matches!(
                    self,
                    RBlock::Photo { .. } | RBlock::Document { .. } | RBlock::Video { .. } | RBlock::Audio { .. }
                ) {
                    m.media = 1;
                }
            }
            RBlock::Pre { text, .. } => m.chars = text.chars().count(),
            RBlock::Math(s) => m.chars = s.chars().count(),
            RBlock::Divider => {}
            RBlock::List { items } | RBlock::OrderedList { items, .. } => {
                for it in items {
                    m.blocks += 1; // the item itself
                    let mut im = Metrics { depth: 1, ..Default::default() };
                    child(&it.blocks, &mut im);
                    m.chars += im.chars;
                    m.blocks += im.blocks;
                    m.depth = m.depth.max(1 + im.depth);
                    m.max_table_cols = m.max_table_cols.max(im.max_table_cols);
                    m.media += im.media;
                }
            }
            RBlock::Quote { blocks } => child(blocks, &mut m),
            RBlock::Details { title, blocks, .. } => {
                m.chars += title.chars();
                child(blocks, &mut m);
            }
            RBlock::Table { rows, .. } => {
                for r in rows {
                    m.blocks += 1;
                    m.max_table_cols = m.max_table_cols.max(r.len());
                    for c in r {
                        m.chars += c.text.chars();
                        m.depth = m.depth.max(2 + c.text.depth());
                    }
                }
            }
        }
        m
    }
}

pub fn metrics_of(blocks: &[RBlock]) -> Metrics {
    let mut m = Metrics::default();
    for b in blocks {
        m.add(b.metrics());
    }
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RichLimits {
    pub max_chars: usize,
    pub max_blocks: usize,
    pub max_depth: usize,
    pub max_table_cols: usize,
    /// Photo/document references per message (the same photo counts each time).
    pub max_media: usize,
}

impl RichLimits {
    /// Measured against the live API (2026-11-11).
    pub const TELEGRAM: RichLimits =
        RichLimits { max_chars: 32768, max_blocks: 500, max_depth: 16, max_table_cols: 20, max_media: 50 };
}

/// Make blocks acceptable to Telegram, which rejects empty content in most
/// containers (verified against the live API, 2026-11):
///
/// | rejected (`RICH_MESSAGE_CONTENT_REQUIRED` etc.) | accepted |
/// |---|---|
/// | preformatted with `""` | preformatted with whitespace |
/// | empty heading, footer, collapsed quote | empty paragraph, empty table cell |
/// | empty math (block or inline) | empty bold/url/fixed spans |
/// | list without items, item without blocks | item with an empty paragraph |
/// | quote / details without blocks, table without rows | details with an empty title |
///
/// Empty constructs appear in model output (an unclosed ```` ``` ```` at the
/// end) and in every streaming snapshot that just opened one. They are kept
/// visible where Markdown shows something (an empty code box, a bullet, a
/// quote bar) and dropped where it shows nothing.
pub fn sanitize(blocks: Vec<RBlock>) -> Vec<RBlock> {
    blocks.into_iter().filter_map(sanitize_block).collect()
}

fn non_empty_children(blocks: Vec<RBlock>) -> Vec<RBlock> {
    let b = sanitize(blocks);
    if b.is_empty() { vec![RBlock::Paragraph(RText::plain(""))] } else { b }
}

/// Strings Telegram cannot store whole (it truncates every text string to
/// 70000 UTF-8 bytes); text nodes are split by the sender, but formula
/// sources and URLs are single strings, so long ones become plain text.
pub const STRING_MAX_BYTES: usize = 65_000;

fn sanitize_text(t: RText) -> RText {
    match t {
        RText::Math(m) if m.trim().is_empty() => RText::Plain(m),
        RText::Math(m) if m.len() > STRING_MAX_BYTES => RText::Plain(m),
        RText::Url { text, url } if url.len() > STRING_MAX_BYTES => sanitize_text(*text),
        RText::Bold(x) => RText::Bold(Box::new(sanitize_text(*x))),
        RText::Italic(x) => RText::Italic(Box::new(sanitize_text(*x))),
        RText::Underline(x) => RText::Underline(Box::new(sanitize_text(*x))),
        RText::Strike(x) => RText::Strike(Box::new(sanitize_text(*x))),
        RText::Fixed(x) => RText::Fixed(Box::new(sanitize_text(*x))),
        RText::Spoiler(x) => RText::Spoiler(Box::new(sanitize_text(*x))),
        RText::Sub(x) => RText::Sub(Box::new(sanitize_text(*x))),
        RText::Super(x) => RText::Super(Box::new(sanitize_text(*x))),
        RText::Url { text, url } => RText::Url { text: Box::new(sanitize_text(*text)), url },
        RText::Concat(v) => RText::Concat(v.into_iter().map(sanitize_text).collect()),
        other => other,
    }
}

fn sanitize_block(b: RBlock) -> Option<RBlock> {
    Some(match b {
        RBlock::Paragraph(t) => RBlock::Paragraph(sanitize_text(t)),
        RBlock::Heading { level, text } => {
            let text = sanitize_text(text);
            if text.is_empty() {
                return None;
            }
            RBlock::Heading { level, text }
        }
        RBlock::Footer(t) | RBlock::CollapsedQuote { text: t } if t.is_empty() => return None,
        RBlock::Footer(t) => RBlock::Footer(sanitize_text(t)),
        RBlock::CollapsedQuote { text } => RBlock::CollapsedQuote { text: sanitize_text(text) },
        RBlock::Pre { text, language } if text.is_empty() => RBlock::Pre { text: " ".into(), language },
        // Telegram turns tabs and carriage returns into single spaces: expand tabs to
        // their columns (keeps indentation readable) and normalize line breaks.
        RBlock::Pre { text, language } if text.contains(['\t', '\r']) => {
            RBlock::Pre { text: expand_tabs(&text.replace("\r\n", "\n").replace('\r', "\n")), language }
        }
        RBlock::Math(m) if m.trim().is_empty() => return None,
        RBlock::Math(m) if m.len() > STRING_MAX_BYTES => RBlock::Pre { text: m, language: "latex".into() },
        RBlock::List { items } | RBlock::OrderedList { items, .. } if items.is_empty() => return None,
        RBlock::List { items } => RBlock::List { items: sanitize_items(items) },
        RBlock::OrderedList { start, items } => RBlock::OrderedList { start, items: sanitize_items(items) },
        RBlock::Quote { blocks } => RBlock::Quote { blocks: non_empty_children(blocks) },
        RBlock::Details { open, title, blocks } => {
            RBlock::Details { open, title: sanitize_text(title), blocks: non_empty_children(blocks) }
        }
        RBlock::Photo { key, caption } => RBlock::Photo { key, caption: sanitize_text(caption) },
        RBlock::Document { key, caption } => RBlock::Document { key, caption: sanitize_text(caption) },
        RBlock::Video { key, caption } => RBlock::Video { key, caption: sanitize_text(caption) },
        RBlock::Audio { key, caption } => RBlock::Audio { key, caption: sanitize_text(caption) },
        RBlock::Table { rows, .. } if rows.iter().all(|r| r.is_empty()) => return None,
        RBlock::Table { rows, bordered, striped } => RBlock::Table {
            rows: rows
                .into_iter()
                .filter(|r| !r.is_empty())
                .map(|r| r.into_iter().map(|c| RCell { text: sanitize_text(c.text), ..c }).collect())
                .collect(),
            bordered,
            striped,
        },
        other => other,
    })
}

/// Expand tabs to spaces at 4-column tab stops (per line).
pub fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            '\t' => {
                let n = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

fn sanitize_items(items: Vec<RItem>) -> Vec<RItem> {
    items.into_iter().map(|it| RItem { blocks: non_empty_children(it.blocks), ..it }).collect()
}

/// Convert Markdown to rich blocks. `base_depth` is the nesting depth at which
/// the blocks will be placed (e.g. 1 inside a details block).
pub fn markdown_to_blocks(src: &str, base_depth: usize, limits: RichLimits) -> Vec<RBlock> {
    let ast = ast::parse(src);
    let mut out = Vec::new();
    for b in &ast {
        let converted = sanitize(block(b));
        let ok = converted.iter().all(|rb| {
            let m = rb.metrics();
            base_depth + m.depth <= limits.max_depth && m.max_table_cols <= limits.max_table_cols
        });
        if ok {
            out.extend(converted);
        } else {
            out.push(raw_block(src, b.src()));
        }
    }
    out
}

/// The raw Markdown of a source range as a `markdown` preformatted block.
pub fn raw_block(src: &str, range: std::ops::Range<usize>) -> RBlock {
    let end = range.end.min(src.len());
    let start = range.start.min(end);
    RBlock::Pre { text: src[start..end].trim_end_matches('\n').to_string(), language: "markdown".into() }
}

/// Whole-source raw display (the "raw" display mode).
pub fn raw_markdown_blocks(src: &str) -> Vec<RBlock> {
    if src.trim().is_empty() {
        return Vec::new();
    }
    vec![RBlock::Pre { text: src.to_string(), language: "markdown".into() }]
}

fn block(b: &Block) -> Vec<RBlock> {
    match b {
        Block::Paragraph { inlines, .. } | Block::TightParagraph { inlines, .. } => {
            // Display math inside a paragraph becomes its own math block.
            split_display_math(inlines)
        }
        Block::Heading { level, inlines, .. } => {
            vec![RBlock::Heading { level: *level, text: inlines_to_rtext(inlines) }]
        }
        Block::CodeBlock { lang, code, .. } => {
            let code = code.strip_suffix('\n').unwrap_or(code);
            vec![RBlock::Pre { text: code.to_string(), language: lang.clone().unwrap_or_default() }]
        }
        Block::MathBlock { tex, .. } => vec![RBlock::Math(tex.trim().to_string())],
        Block::Quote { blocks, .. } => vec![RBlock::Quote { blocks: blocks.iter().flat_map(block).collect() }],
        Block::List { start, items, .. } => {
            let items = items
                .iter()
                .map(|it| RItem { checkbox: it.task, blocks: it.blocks.iter().flat_map(block).collect() })
                .collect();
            match start {
                Some(n) => vec![RBlock::OrderedList { start: i32::try_from(*n).unwrap_or(1), items }],
                None => vec![RBlock::List { items }],
            }
        }
        Block::Table { align, header, rows, .. } => {
            let al = |i: usize| match align.get(i).copied().unwrap_or(Align::None) {
                Align::Center => CellAlign::Center,
                Align::Right => CellAlign::Right,
                _ => CellAlign::Left,
            };
            let mut out_rows = Vec::new();
            if !header.is_empty() {
                out_rows.push(
                    header
                        .iter()
                        .enumerate()
                        .map(|(i, c)| RCell { text: inlines_to_rtext(c), header: true, align: al(i) })
                        .collect(),
                );
            }
            for r in rows {
                out_rows.push(
                    r.iter()
                        .enumerate()
                        .map(|(i, c)| RCell { text: inlines_to_rtext(c), header: false, align: al(i) })
                        .collect(),
                );
            }
            vec![RBlock::Table { rows: out_rows, bordered: true, striped: false }]
        }
        Block::Rule { .. } => vec![RBlock::Divider],
        Block::Html { text, .. } => vec![RBlock::Paragraph(RText::plain(text.trim_end_matches('\n')))],
        Block::FootnoteDef { label, blocks, .. } => {
            let mut v = vec![RBlock::Paragraph(RText::plain(format!("[{label}]:")))];
            v.extend(blocks.iter().flat_map(block));
            v
        }
    }
}

fn split_display_math(inlines: &[Inline]) -> Vec<RBlock> {
    let mut out = Vec::new();
    let mut cur: Vec<Inline> = Vec::new();
    for i in inlines {
        if let Inline::Math { tex, display: true, .. } = i {
            let t = inlines_to_rtext(&trim_breaks(&cur));
            if !t.is_empty() {
                out.push(RBlock::Paragraph(t));
            }
            cur.clear();
            out.push(RBlock::Math(tex.trim().to_string()));
        } else {
            cur.push(i.clone());
        }
    }
    let t = inlines_to_rtext(&trim_breaks(&cur));
    if !t.is_empty() || out.is_empty() {
        out.push(RBlock::Paragraph(t));
    }
    out
}

fn trim_breaks(v: &[Inline]) -> Vec<Inline> {
    let is_break = |i: &Inline| matches!(i, Inline::SoftBreak { .. } | Inline::HardBreak { .. });
    let start = v.iter().position(|i| !is_break(i)).unwrap_or(v.len());
    let end = v.iter().rposition(|i| !is_break(i)).map(|p| p + 1).unwrap_or(start);
    v[start..end.max(start)].to_vec()
}

pub fn inlines_to_rtext(inlines: &[Inline]) -> RText {
    RText::concat(inlines.iter().map(inline).collect())
}

fn inline(i: &Inline) -> RText {
    match i {
        Inline::Text { text, .. } => linkify(text),
        Inline::Code { code, .. } => RText::fixed(RText::plain(code.clone())),
        Inline::Math { tex, .. } => RText::Math(tex.clone()),
        Inline::Emph { children, .. } => RText::italic(inlines_to_rtext(children)),
        Inline::Strong { children, .. } => RText::bold(inlines_to_rtext(children)),
        Inline::Strike { children, .. } => RText::Strike(Box::new(inlines_to_rtext(children))),
        Inline::Link { url, children, .. } => {
            let text = if ast::plain_text(children).trim().is_empty() {
                RText::plain(url.clone())
            } else {
                inlines_to_rtext(children)
            };
            if is_safe_url(url) { RText::Url { text: Box::new(text), url: url.clone() } } else { text }
        }
        Inline::Image { url, alt, .. } => {
            let alt_text = ast::plain_text(alt);
            let label = RText::plain(format!(
                "🖼 {}",
                if alt_text.trim().is_empty() { url.as_str() } else { alt_text.as_str() }
            ));
            if is_safe_url(url) { RText::Url { text: Box::new(label), url: url.clone() } } else { label }
        }
        Inline::Html { html, .. } => RText::plain(html.clone()),
        Inline::FootnoteRef { label, .. } => RText::plain(format!("[{label}]")),
        Inline::SoftBreak { .. } | Inline::HardBreak { .. } => RText::plain("\n"),
    }
}

/// Make bare http(s) URLs clickable. Needed because rich messages are sent
/// with `noautolink` (so that `@names`, `#tags` and `/commands` in model output
/// never become live mentions/commands).
pub fn linkify(text: &str) -> RText {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(pos) = find_url_start(rest) {
        let (before, tail) = rest.split_at(pos);
        let end = tail
            .char_indices()
            .find(|&(_, c)| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '）' | '，' | '。'))
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        let mut url = &tail[..end];
        // Trailing punctuation is usually not part of the URL.
        while let Some(c) = url.chars().last() {
            if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') && !balanced_close(url, c) {
                url = &url[..url.len() - c.len_utf8()];
            } else {
                break;
            }
        }
        if url.len() <= "https://".len() {
            parts.push(RText::plain(&rest[..pos + 1]));
            rest = &rest[pos + 1..];
            continue;
        }
        if !before.is_empty() {
            parts.push(RText::plain(before));
        }
        parts.push(RText::Url { text: Box::new(RText::plain(url)), url: url.to_string() });
        rest = &tail[url.len()..];
    }
    if !rest.is_empty() {
        parts.push(RText::plain(rest));
    }
    RText::concat(parts)
}

fn balanced_close(url: &str, c: char) -> bool {
    let open = match c {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => return false,
    };
    url.matches(open).count() >= url.matches(c).count()
}

fn find_url_start(s: &str) -> Option<usize> {
    let a = s.find("https://");
    let b = s.find("http://");
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

// ------------------------------------------------------------------ splitting

/// Split a block list into messages that each satisfy `limits` (chars and
/// block count). Oversized blocks are split structurally (code by lines,
/// lists by items, tables by rows with the header repeated, details by
/// children, paragraphs by lines/words).
pub fn split_blocks(blocks: Vec<RBlock>, limits: RichLimits) -> Vec<Vec<RBlock>> {
    let mut msgs: Vec<Vec<RBlock>> = Vec::new();
    let mut cur: Vec<RBlock> = Vec::new();
    let mut cur_m = Metrics::default();
    let fits =
        |m: &Metrics| m.chars <= limits.max_chars && m.blocks <= limits.max_blocks && m.media <= limits.max_media;
    let mut queue: std::collections::VecDeque<RBlock> = blocks.into();
    while let Some(b) = queue.pop_front() {
        let m = b.metrics();
        let mut total = cur_m;
        total.add(m);
        if fits(&total) {
            cur.push(b);
            cur_m = total;
            continue;
        }
        if !cur.is_empty() && fits(&m) {
            msgs.push(std::mem::take(&mut cur));
            cur_m = m;
            cur.push(b);
            continue;
        }
        // The block alone does not fit in the remaining space: split it so that
        // the first piece fills the current message as much as possible.
        let room = Metrics {
            chars: limits.max_chars.saturating_sub(cur_m.chars),
            blocks: limits.max_blocks.saturating_sub(cur_m.blocks),
            ..Default::default()
        };
        let use_room = if room.chars >= limits.max_chars / 4 && room.blocks >= 4 {
            room
        } else {
            Metrics { chars: limits.max_chars, blocks: limits.max_blocks, ..Default::default() }
        };
        if use_room.chars == limits.max_chars && !cur.is_empty() {
            msgs.push(std::mem::take(&mut cur));
            cur_m = Metrics::default();
        }
        let fallback_text = match &b {
            RBlock::Math(s) => Some(s.clone()),
            _ => None,
        };
        match split_block(b, use_room.chars, use_room.blocks) {
            Some((first, rest)) => {
                queue.push_front(rest);
                queue.push_front(first);
            }
            None => {
                // Cannot split structurally (e.g. a giant single formula): show its
                // source as preformatted text, which can be split by lines.
                let t = fallback_text.unwrap_or_default();
                if !cur.is_empty() {
                    msgs.push(std::mem::take(&mut cur));
                    cur_m = Metrics::default();
                }
                if t.chars().count() > limits.max_chars {
                    queue.push_front(RBlock::Pre { text: t, language: "tex".into() });
                } else {
                    cur.push(RBlock::Pre { text: t, language: "tex".into() });
                    cur_m = metrics_of(&cur);
                }
            }
        }
    }
    if !cur.is_empty() {
        msgs.push(cur);
    }
    msgs
}

/// Appended to the title of a fold that goes on in the next message.
const CONT: &str = " (cont.)";

/// Split one block into a first piece (fitting `chars`/`blocks` if possible) and the rest.
fn split_block(b: RBlock, chars: usize, blocks: usize) -> Option<(RBlock, RBlock)> {
    let chars = chars.max(64);
    let blocks = blocks.max(2);
    match b {
        RBlock::Pre { text, language } => {
            let (a, rest) = split_text_at(&text, chars)?;
            Some((RBlock::Pre { text: a, language: language.clone() }, RBlock::Pre { text: rest, language }))
        }
        RBlock::Paragraph(t) => {
            // Formatting of a giant paragraph is dropped at the split point (plain text kept exactly).
            let plain = t.to_plain();
            let (a, rest) = split_text_at(&plain, chars)?;
            Some((RBlock::Paragraph(RText::plain(a)), RBlock::Paragraph(RText::plain(rest))))
        }
        RBlock::List { items } => {
            let (a, r) = split_items(items, chars, blocks)?;
            Some((RBlock::List { items: a }, RBlock::List { items: r }))
        }
        RBlock::OrderedList { start, items } => {
            let n = items.len();
            let (a, r) = split_items(items, chars, blocks)?;
            let next = start + (n - r.len()) as i32;
            Some((RBlock::OrderedList { start, items: a }, RBlock::OrderedList { start: next, items: r }))
        }
        RBlock::Table { rows, bordered, striped } => {
            if rows.len() < 2 {
                return None;
            }
            let header: Option<Vec<RCell>> = rows.first().filter(|r| r.iter().any(|c| c.header)).cloned();
            let mut acc = 0usize;
            let mut cut = 0;
            for (i, r) in rows.iter().enumerate() {
                let c: usize = r.iter().map(|c| c.text.chars()).sum();
                if i > 0 && (acc + c > chars || i + 1 > blocks.saturating_sub(1)) {
                    break;
                }
                acc += c;
                cut = i + 1;
            }
            if cut == 0 || cut >= rows.len() {
                cut = rows.len() / 2;
            }
            if cut == 0 {
                return None;
            }
            let mut rest: Vec<Vec<RCell>> = rows[cut..].to_vec();
            if let Some(h) = header
                && cut > 1
            {
                rest.insert(0, h);
            }
            Some((
                RBlock::Table { rows: rows[..cut].to_vec(), bordered, striped },
                RBlock::Table { rows: rest, bordered, striped },
            ))
        }
        RBlock::Details { open, title, blocks: children } => {
            let (a, r) = split_children(children, chars.saturating_sub(title.chars() + 16), blocks.saturating_sub(1))?;
            // One marker however many messages the fold runs on: never "(cont.) (cont.)".
            let cont = if title.to_plain().ends_with(CONT) {
                title.clone()
            } else {
                RText::concat(vec![title.clone(), RText::plain(CONT)])
            };
            Some((RBlock::Details { open, title, blocks: a }, RBlock::Details { open, title: cont, blocks: r }))
        }
        RBlock::Quote { blocks: children } => {
            let (a, r) = split_children(children, chars, blocks.saturating_sub(1))?;
            Some((RBlock::Quote { blocks: a }, RBlock::Quote { blocks: r }))
        }
        RBlock::CollapsedQuote { text } => {
            let plain = text.to_plain();
            let (a, rest) = split_text_at(&plain, chars)?;
            Some((
                RBlock::CollapsedQuote { text: RText::plain(a) },
                RBlock::CollapsedQuote { text: RText::plain(rest) },
            ))
        }
        RBlock::Heading { level, text } => {
            let plain = text.to_plain();
            let (a, rest) = split_text_at(&plain, chars)?;
            Some((RBlock::Heading { level, text: RText::plain(a) }, RBlock::Paragraph(RText::plain(rest))))
        }
        RBlock::Footer(t) => {
            let plain = t.to_plain();
            let (a, rest) = split_text_at(&plain, chars)?;
            Some((RBlock::Footer(RText::plain(a)), RBlock::Footer(RText::plain(rest))))
        }
        RBlock::Math(_)
        | RBlock::Divider
        | RBlock::Photo { .. }
        | RBlock::Document { .. }
        | RBlock::Video { .. }
        | RBlock::Audio { .. } => None,
    }
}

fn split_items(items: Vec<RItem>, chars: usize, blocks: usize) -> Option<(Vec<RItem>, Vec<RItem>)> {
    if items.len() < 2 {
        // A single giant item: split its children.
        let mut items = items;
        let it = items.pop()?;
        let (a, r) = split_children(it.blocks, chars, blocks.saturating_sub(2))?;
        return Some((vec![RItem { checkbox: it.checkbox, blocks: a }], vec![RItem { checkbox: None, blocks: r }]));
    }
    let mut acc = Metrics::default();
    let mut cut = 0;
    for (i, it) in items.iter().enumerate() {
        let m = metrics_of(&it.blocks);
        if i > 0 && (acc.chars + m.chars > chars || acc.blocks + m.blocks + 1 > blocks) {
            break;
        }
        acc.add(m);
        acc.blocks += 1;
        cut = i + 1;
    }
    let cut = cut.clamp(1, items.len() - 1);
    let mut a = items;
    let r = a.split_off(cut);
    Some((a, r))
}

fn split_children(children: Vec<RBlock>, chars: usize, blocks: usize) -> Option<(Vec<RBlock>, Vec<RBlock>)> {
    if children.is_empty() {
        return None;
    }
    let mut acc = Metrics::default();
    let mut cut = 0;
    for (i, c) in children.iter().enumerate() {
        let m = c.metrics();
        if acc.chars + m.chars > chars || acc.blocks + m.blocks > blocks {
            break;
        }
        acc.add(m);
        cut = i + 1;
    }
    if cut == 0 {
        // First child alone is too big: split it.
        let mut it = children.into_iter();
        let first = it.next()?;
        let (a, r) = split_block(first, chars, blocks)?;
        let mut rest = vec![r];
        rest.extend(it);
        return Some((vec![a], rest));
    }
    if cut >= children.len() {
        return None;
    }
    let mut a = children;
    let r = a.split_off(cut);
    Some((a, r))
}

/// Split text into a first part of at most `max_chars` code points (preferring
/// a line break, then a space) and the rest.
fn split_text_at(text: &str, max_chars: usize) -> Option<(String, String)> {
    let total = text.chars().count();
    if total <= max_chars || max_chars == 0 {
        return None;
    }
    let byte_limit = text.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(text.len());
    let window = &text[..byte_limit];
    let min = byte_limit / 2;
    let cut = window
        .rfind('\n')
        .filter(|&i| i >= min)
        .map(|i| i + 1)
        .or_else(|| window.rfind(' ').filter(|&i| i >= min).map(|i| i + 1))
        .unwrap_or(byte_limit);
    let (a, b) = text.split_at(cut);
    Some((a.trim_end_matches('\n').to_string(), b.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_markdown() {
        let md = "# T\n\nHello **b** `c` [l](https://x.y) $a^2$\n\n- [x] done\n- item\n\n1. one\n2. two\n\n```rust\nfn x(){}\n```\n\n| a | b |\n|:-:|--:|\n| 1 | 2 |\n\n> q\n\n---\n\n$$\nE=mc^2\n$$\n";
        let b = markdown_to_blocks(md, 0, RichLimits::TELEGRAM);
        assert!(matches!(&b[0], RBlock::Heading { level: 1, .. }));
        match &b[1] {
            RBlock::Paragraph(RText::Concat(v)) => {
                assert!(v.iter().any(|t| matches!(t, RText::Bold(_))));
                assert!(v.iter().any(|t| matches!(t, RText::Fixed(_))));
                assert!(v.iter().any(|t| matches!(t, RText::Url { url, .. } if url == "https://x.y")));
                assert!(v.iter().any(|t| matches!(t, RText::Math(m) if m == "a^2")));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&b[2], RBlock::List { items } if items[0].checkbox == Some(true)));
        assert!(matches!(&b[3], RBlock::OrderedList { start: 1, .. }));
        assert!(matches!(&b[4], RBlock::Pre { language, .. } if language == "rust"));
        match &b[5] {
            RBlock::Table { rows, .. } => {
                assert!(rows[0][0].header);
                assert_eq!(rows[0][0].align, CellAlign::Center);
                assert_eq!(rows[1][1].align, CellAlign::Right);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&b[6], RBlock::Quote { .. }));
        assert!(matches!(&b[7], RBlock::Divider));
        assert!(matches!(&b[8], RBlock::Math(m) if m == "E=mc^2"));
    }

    #[test]
    fn wide_table_falls_back_to_raw() {
        let cols = 25;
        let header: String = (0..cols).map(|i| format!("| c{i} ")).collect::<String>() + "|\n";
        let sep: String = (0..cols).map(|_| "|---").collect::<String>() + "|\n";
        let md = format!("before\n\n{header}{sep}\nafter");
        let b = markdown_to_blocks(&md, 0, RichLimits::TELEGRAM);
        assert!(matches!(&b[0], RBlock::Paragraph(_)));
        assert!(matches!(&b[1], RBlock::Pre { language, text } if language == "markdown" && text.starts_with("| c0")));
        assert!(matches!(&b[2], RBlock::Paragraph(_)));
    }

    #[test]
    fn deep_nesting_falls_back_to_raw() {
        let md: String = (0..20).map(|i| format!("{}- level {i}\n", "  ".repeat(i))).collect();
        let b = markdown_to_blocks(&md, 1, RichLimits::TELEGRAM);
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], RBlock::Pre { language, .. } if language == "markdown"));
    }

    #[test]
    fn linkify_bare_urls() {
        let t = linkify("see https://example.com/a_(b). and http://x.io, ok");
        match t {
            RText::Concat(v) => {
                let urls: Vec<_> = v
                    .iter()
                    .filter_map(|t| if let RText::Url { url, .. } = t { Some(url.as_str()) } else { None })
                    .collect();
                assert_eq!(urls, vec!["https://example.com/a_(b)", "http://x.io"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(linkify("no links @user #tag /cmd"), RText::plain("no links @user #tag /cmd"));
    }

    #[test]
    fn split_respects_limits() {
        let lim = RichLimits { max_chars: 1000, max_blocks: 20, max_depth: 16, max_table_cols: 20, max_media: 50 };
        let mut blocks = Vec::new();
        for i in 0..30 {
            blocks.push(RBlock::Paragraph(RText::plain(format!("paragraph {i} {}", "x".repeat(80)))));
        }
        blocks.push(RBlock::Pre { text: (0..200).map(|i| format!("line {i}\n")).collect(), language: "txt".into() });
        blocks.push(RBlock::List {
            items: (0..50)
                .map(|i| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(RText::plain(format!("item {i}")))] })
                .collect(),
        });
        blocks.push(RBlock::Details {
            open: false,
            title: RText::plain("Reasoning"),
            blocks: (0..40)
                .map(|i| RBlock::Paragraph(RText::plain(format!("thought {i} {}", "y".repeat(60)))))
                .collect(),
        });
        let total_before: usize = metrics_of(&blocks).chars;
        let msgs = split_blocks(blocks, lim);
        assert!(msgs.len() > 3);
        let mut total_after = 0;
        for m in &msgs {
            let mm = metrics_of(m);
            assert!(mm.chars <= lim.max_chars, "chars {}", mm.chars);
            assert!(mm.blocks <= lim.max_blocks, "blocks {}", mm.blocks);
            total_after += mm.chars;
        }
        // Only titles of continuation details are added.
        assert!(total_after >= total_before - 50);
    }

    /// A fold over many messages: each continuation says "(cont.)" once.
    #[test]
    fn a_long_fold_is_marked_once_per_message() {
        let lim = RichLimits { max_chars: 1000, max_blocks: 20, max_depth: 16, max_table_cols: 20, max_media: 50 };
        let fold = RBlock::Details {
            open: false,
            title: RText::plain("💭 41 · 🔧 98"),
            blocks: (0..120).map(|i| RBlock::Paragraph(RText::plain(format!("step {i} {}", "z".repeat(60))))).collect(),
        };
        let msgs = split_blocks(vec![fold], lim);
        assert!(msgs.len() >= 5, "{}", msgs.len());
        let titles: Vec<String> = msgs
            .iter()
            .map(|m| match &m[0] {
                RBlock::Details { title, .. } => title.to_plain(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(titles[0], "💭 41 · 🔧 98");
        assert!(titles[1..].iter().all(|t| t == "💭 41 · 🔧 98 (cont.)"), "{titles:?}");
    }

    #[test]
    fn streaming_partial_markdown_never_panics() {
        let full = "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```py\nprint(1)\n```\n\n- [ ] x\n  - y\n\n> quote **b**\n\n$$x$$ tail $y$";
        for end in 0..=full.len() {
            if full.is_char_boundary(end) {
                let b = markdown_to_blocks(&full[..end], 1, RichLimits::TELEGRAM);
                let _ = split_blocks(
                    b,
                    RichLimits { max_chars: 40, max_blocks: 5, max_depth: 16, max_table_cols: 20, max_media: 50 },
                );
            }
        }
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn unclosed_fence_and_empty_containers_are_accepted_shapes() {
        let b = markdown_to_blocks("text\n\n```", 0, RichLimits::TELEGRAM);
        assert_eq!(b.last(), Some(&RBlock::Pre { text: " ".into(), language: String::new() }));
        let b = markdown_to_blocks("- \n\n>\n\n## \n\n$$$$", 0, RichLimits::TELEGRAM);
        for blk in &b {
            match blk {
                RBlock::List { items } => assert!(items.iter().all(|i| !i.blocks.is_empty())),
                RBlock::Quote { blocks } => assert!(!blocks.is_empty()),
                RBlock::Heading { text, .. } => assert!(!text.is_empty()),
                RBlock::Math(m) => assert!(!m.trim().is_empty()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn tabs_expand_to_columns() {
        assert_eq!(expand_tabs("\tx\n a\tb"), "    x\n a  b");
        let b = sanitize(vec![RBlock::Pre { text: "a\r\n\tb".into(), language: "go".into() }]);
        assert_eq!(b, vec![RBlock::Pre { text: "a\n    b".into(), language: "go".into() }]);
    }
}

#[cfg(test)]
mod media_limit_tests {
    use super::*;

    #[test]
    fn at_most_50_media_per_message() {
        let blocks: Vec<RBlock> =
            (0..120).map(|i| RBlock::Photo { key: format!("k{i}"), caption: RText::plain("") }).collect();
        let pages = split_blocks(blocks, RichLimits::TELEGRAM);
        assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), vec![50, 50, 20]);
    }
}
