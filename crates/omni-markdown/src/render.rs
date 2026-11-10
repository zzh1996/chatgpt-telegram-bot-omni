//! Markdown AST -> [`Formatted`] (Telegram text + entities).
//!
//! Two display modes:
//! - [`Mode::Clean`]: normal rendering, Markdown syntax removed.
//! - [`Mode::Raw`]: the text is *exactly* the model's Markdown source; entities
//!   only decorate it (like the old bot's renderer). Nothing is hidden.
//!
//! The renderer never emits [`EntityKind::Underline`]; that style is reserved
//! for bot-generated labels so that model output cannot impersonate the bot.

use unicode_width::UnicodeWidthStr;

use crate::ast::{self, Align, Block, Inline};
use crate::formatted::{BreakPriority, EntityKind, Formatted};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Clean,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub mode: Mode,
    /// Render code blocks / tables / display math as `pre` entities. When false
    /// (e.g. inside a collapsed reasoning quote, where `pre` would break the
    /// quote apart) they are rendered as plain text.
    pub allow_pre: bool,
    /// Render block quotes as blockquote entities. When false (e.g. inside a
    /// reasoning quote) quotes are shown with a textual marker instead.
    pub allow_quote: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { mode: Mode::Clean, allow_pre: true, allow_quote: true }
    }
}

impl RenderOptions {
    pub fn nested_in_quote(mode: Mode) -> Self {
        Self { mode, allow_pre: false, allow_quote: false }
    }
}

pub fn render(src: &str, opts: RenderOptions) -> Formatted {
    let blocks = ast::parse(src);
    let mut f = match opts.mode {
        Mode::Clean => {
            let mut w = Writer::new(opts);
            w.blocks(&blocks, false);
            w.out
        }
        Mode::Raw => render_raw(src, &blocks, opts),
    };
    f.normalize();
    f
}

pub fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    ["http://", "https://", "tg://", "mailto:", "ftp://", "tel:"].iter().any(|p| lower.starts_with(p))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ---------------------------------------------------------------- clean mode

struct Writer {
    out: Formatted,
    opts: RenderOptions,
    indent: String,
    at_line_start: bool,
    list_depth: usize,
}

impl Writer {
    fn new(opts: RenderOptions) -> Self {
        Self { out: Formatted::new(), opts, indent: String::new(), at_line_start: true, list_depth: 0 }
    }

    fn pos(&self) -> usize {
        self.out.text.len()
    }

    /// Push text, inserting the current indentation at line starts.
    fn text(&mut self, s: &str) {
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                self.out.push_char('\n');
                self.at_line_start = true;
            }
            if !line.is_empty() {
                if self.at_line_start && !self.indent.is_empty() {
                    let ind = self.indent.clone();
                    self.out.push_str(&ind);
                }
                self.out.push_str(line);
                self.at_line_start = false;
            }
        }
    }

    /// Push text verbatim (no indentation), e.g. code block content.
    fn raw(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.out.push_str(s);
        self.at_line_start = s.ends_with('\n');
    }

    fn newline(&mut self) {
        self.out.push_char('\n');
        self.at_line_start = true;
    }

    fn separator(&mut self, tight: bool) {
        if self.out.text.is_empty() {
            return;
        }
        // Normalize the number of trailing newlines.
        let have = self.out.text.chars().rev().take_while(|&c| c == '\n').count();
        let want = if tight { 1 } else { 2 };
        for _ in have..want {
            self.newline();
        }
        self.at_line_start = true;
        self.out.mark_break(if tight { BreakPriority::Line } else { BreakPriority::Block });
    }

    fn blocks(&mut self, blocks: &[Block], tight: bool) {
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                self.separator(tight || matches!(b, Block::TightParagraph { .. }));
            }
            self.block(b, tight);
        }
    }

    fn block(&mut self, b: &Block, tight: bool) {
        match b {
            Block::Paragraph { inlines, .. } | Block::TightParagraph { inlines, .. } => self.inlines(inlines),
            Block::Heading { inlines, .. } => {
                let start = self.pos();
                self.inlines(inlines);
                self.out.wrap_from(start, EntityKind::Bold);
            }
            Block::CodeBlock { lang, code, .. } => self.code_block(lang.as_deref(), code),
            Block::MathBlock { tex, .. } => self.code_block(Some("tex"), tex.trim()),
            Block::Quote { blocks, .. } => {
                if self.opts.allow_quote {
                    let start = self.pos();
                    self.blocks(blocks, tight);
                    self.out.wrap_from(start, EntityKind::Blockquote { collapsed: false });
                } else {
                    let saved = self.indent.clone();
                    self.indent.push_str("│ ");
                    self.blocks(blocks, tight);
                    self.indent = saved;
                }
            }
            Block::List { start, items, .. } => self.list(*start, items),
            Block::Table { align, header, rows, .. } => self.table(align, header, rows),
            Block::Rule { .. } => self.text("────────"),
            Block::Html { text, .. } => self.text(text.trim_end_matches('\n')),
            Block::FootnoteDef { label, blocks, .. } => {
                self.text(&format!("[{label}]: "));
                self.blocks(blocks, true);
            }
        }
    }

    fn code_block(&mut self, lang: Option<&str>, code: &str) {
        let code = code.strip_suffix('\n').unwrap_or(code);
        if code.trim().is_empty() {
            return;
        }
        if self.opts.allow_pre {
            if !self.at_line_start {
                self.newline();
            }
            let start = self.pos();
            self.raw(code);
            self.out.wrap_from(start, EntityKind::Pre { language: lang.unwrap_or("").to_string() });
            self.at_line_start = false;
        } else {
            self.text(code);
        }
    }

    fn list(&mut self, start: Option<u64>, items: &[ast::ListItem]) {
        let tight = items.iter().all(|it| !it.blocks.iter().any(|b| matches!(b, Block::Paragraph { .. })));
        let depth = self.list_depth;
        self.list_depth += 1;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.separator(tight);
            } else if !self.at_line_start {
                self.newline();
            }
            let mut marker = match start {
                Some(n) => format!("{}. ", n + i as u64),
                None => match item.task {
                    Some(_) => String::new(),
                    None => format!("{} ", ["•", "◦", "▪"][depth.min(2)]),
                },
            };
            if let Some(checked) = item.task {
                marker.push_str(if checked { "☑ " } else { "☐ " });
            }
            self.text(&marker);
            let saved = self.indent.clone();
            self.indent.push_str(&" ".repeat(UnicodeWidthStr::width(marker.as_str()).max(2)));
            self.blocks(&item.blocks, tight);
            self.indent = saved;
        }
        self.list_depth -= 1;
    }

    fn table(&mut self, align: &[Align], header: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) {
        let text = format_table(align, header, rows);
        if self.opts.allow_pre {
            if !self.at_line_start {
                self.newline();
            }
            let start = self.pos();
            self.raw(&text);
            self.out.wrap_from(start, EntityKind::Pre { language: String::new() });
            self.at_line_start = false;
        } else {
            self.text(&text);
        }
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for i in inlines {
            self.inline(i);
        }
    }

    fn inline(&mut self, i: &Inline) {
        match i {
            Inline::Text { text, .. } => self.text(text),
            Inline::Code { code, .. } => {
                let start = self.pos();
                self.text(code);
                self.out.wrap_from(start, EntityKind::Code);
            }
            Inline::Math { tex, display, .. } => {
                if *display && self.opts.allow_pre {
                    self.code_block(Some("tex"), tex.trim());
                } else {
                    let start = self.pos();
                    self.text(tex);
                    self.out.wrap_from(start, EntityKind::Code);
                }
            }
            Inline::Emph { children, .. } => self.wrapped(children, EntityKind::Italic),
            Inline::Strong { children, .. } => self.wrapped(children, EntityKind::Bold),
            Inline::Strike { children, .. } => self.wrapped(children, EntityKind::Strikethrough),
            Inline::Link { url, children, .. } => {
                let start = self.pos();
                if ast::plain_text(children).trim().is_empty() {
                    self.text(url);
                } else {
                    self.inlines(children);
                }
                if is_safe_url(url) {
                    self.out.wrap_from(start, EntityKind::TextUrl { url: url.clone() });
                }
            }
            Inline::Image { url, alt, .. } => {
                self.text("🖼 ");
                let start = self.pos();
                let alt_text = ast::plain_text(alt);
                if alt_text.trim().is_empty() {
                    self.text(url);
                } else {
                    self.text(&alt_text);
                }
                if is_safe_url(url) {
                    self.out.wrap_from(start, EntityKind::TextUrl { url: url.clone() });
                }
            }
            Inline::Html { html, .. } => self.text(html),
            Inline::FootnoteRef { label, .. } => self.text(&format!("[{label}]")),
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => self.newline(),
        }
    }

    fn wrapped(&mut self, children: &[Inline], kind: EntityKind) {
        let start = self.pos();
        self.inlines(children);
        self.out.wrap_from(start, kind);
    }
}

/// Render a table as aligned monospace text.
pub fn format_table(align: &[Align], header: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> String {
    let cell = |c: &Vec<Inline>| ast::plain_text(c).replace('\n', " ").trim().to_string();
    let head: Vec<String> = header.iter().map(cell).collect();
    let body: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(cell).collect()).collect();
    let ncols = head.len().max(body.iter().map(Vec::len).max().unwrap_or(0));
    let mut widths = vec![0usize; ncols];
    for row in std::iter::once(&head).chain(body.iter()) {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(UnicodeWidthStr::width(c.as_str()));
        }
    }
    let pad = |s: &str, w: usize, a: Align| {
        let sw = UnicodeWidthStr::width(s);
        let gap = w.saturating_sub(sw);
        match a {
            Align::Right => format!("{}{}", " ".repeat(gap), s),
            Align::Center => format!("{}{}{}", " ".repeat(gap / 2), s, " ".repeat(gap - gap / 2)),
            _ => format!("{}{}", s, " ".repeat(gap)),
        }
    };
    let fmt_row = |row: &Vec<String>| {
        let cells: Vec<String> = (0..ncols)
            .map(|i| {
                let s = row.get(i).map(String::as_str).unwrap_or("");
                pad(s, widths[i], align.get(i).copied().unwrap_or(Align::None))
            })
            .collect();
        cells.join(" │ ").trim_end().to_string()
    };
    let mut out = String::new();
    if !head.is_empty() {
        out.push_str(&fmt_row(&head));
        out.push('\n');
        let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
        out.push_str(&sep.join("─┼─"));
        out.push('\n');
    }
    for r in &body {
        out.push_str(&fmt_row(r));
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

// ---------------------------------------------------------------- raw mode

fn render_raw(src: &str, blocks: &[Block], opts: RenderOptions) -> Formatted {
    let mut f = Formatted::plain(src);
    let mut r = RawSpans { src, f: &mut f, opts };
    for b in blocks {
        r.block(b);
        let end = b.src().end.min(src.len());
        r.f.breaks.push((end, BreakPriority::Block));
    }
    f
}

struct RawSpans<'a> {
    src: &'a str,
    f: &'a mut Formatted,
    opts: RenderOptions,
}

impl RawSpans<'_> {
    fn span(&mut self, r: std::ops::Range<usize>, kind: EntityKind) {
        let end = r.end.min(self.src.len());
        if r.start < end {
            self.f.add_span(r.start, end, kind);
        }
    }

    fn block(&mut self, b: &Block) {
        match b {
            Block::Paragraph { inlines, .. } | Block::TightParagraph { inlines, .. } => self.inlines(inlines),
            Block::Heading { inlines, src, .. } => {
                self.span(src.clone(), EntityKind::Bold);
                self.inlines(inlines);
            }
            Block::CodeBlock { lang, code_src, .. } => {
                if let (true, Some(cs)) = (self.opts.allow_pre, code_src) {
                    self.span(cs.clone(), EntityKind::Pre { language: lang.clone().unwrap_or_default() });
                }
            }
            Block::MathBlock { src, .. } => {
                if self.opts.allow_pre {
                    self.span(src.clone(), EntityKind::Pre { language: "tex".into() });
                }
            }
            Block::Table { src, .. } => {
                if self.opts.allow_pre {
                    self.span(src.clone(), EntityKind::Pre { language: String::new() });
                }
            }
            Block::Quote { blocks, src } => {
                if self.opts.allow_quote {
                    self.span(src.clone(), EntityKind::Blockquote { collapsed: false });
                }
                for b in blocks {
                    self.block(b);
                }
            }
            Block::List { items, .. } => {
                for it in items {
                    for b in &it.blocks {
                        self.block(b);
                    }
                }
            }
            Block::FootnoteDef { blocks, .. } => {
                for b in blocks {
                    self.block(b);
                }
            }
            Block::Rule { .. } | Block::Html { .. } => {}
        }
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for i in inlines {
            self.inline(i);
        }
    }

    fn inline(&mut self, i: &Inline) {
        match i {
            Inline::Code { src, .. } | Inline::Math { src, display: false, .. } => {
                // Exclude the delimiters so tap-to-copy copies only the content.
                let s = &self.src[src.clone()];
                let lead = s.len() - s.trim_start_matches(['`', '$']).len();
                let trail = s.len() - s.trim_end_matches(['`', '$']).len();
                if lead + trail < s.len() {
                    self.span(src.start + lead..src.end - trail, EntityKind::Code);
                }
            }
            Inline::Math { src, display: true, .. } => self.span(src.clone(), EntityKind::Code),
            Inline::Emph { children, src } => {
                self.span(src.clone(), EntityKind::Italic);
                self.inlines(children);
            }
            Inline::Strong { children, src } => {
                self.span(src.clone(), EntityKind::Bold);
                self.inlines(children);
            }
            Inline::Strike { children, src } => {
                self.span(src.clone(), EntityKind::Strikethrough);
                self.inlines(children);
            }
            Inline::Link { url, children, src, .. } => {
                if is_safe_url(url) {
                    self.span(src.clone(), EntityKind::TextUrl { url: url.clone() });
                }
                self.inlines(children);
            }
            Inline::Image { url, src, .. } => {
                if is_safe_url(url) {
                    self.span(src.clone(), EntityKind::TextUrl { url: url.clone() });
                }
            }
            Inline::Text { .. }
            | Inline::Html { .. }
            | Inline::FootnoteRef { .. }
            | Inline::SoftBreak { .. }
            | Inline::HardBreak { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn clean(md: &str) -> Formatted {
        render(md, RenderOptions::default())
    }

    fn spans(f: &Formatted) -> Vec<(String, EntityKind)> {
        f.spans.iter().map(|s| (f.text[s.start..s.end].to_string(), s.kind.clone())).collect()
    }

    #[test]
    fn clean_inline_styles() {
        let f = clean("Hello **bold** *it* ~~del~~ `code` [link](https://example.com).");
        assert_eq!(f.text, "Hello bold it del code link.");
        assert_eq!(
            spans(&f),
            vec![
                ("bold".into(), EntityKind::Bold),
                ("it".into(), EntityKind::Italic),
                ("del".into(), EntityKind::Strikethrough),
                ("code".into(), EntityKind::Code),
                ("link".into(), EntityKind::TextUrl { url: "https://example.com".into() }),
            ]
        );
    }

    #[test]
    fn clean_heading_and_paragraphs() {
        let f = clean("# Title\nline1\nline2\n\npara2");
        assert_eq!(f.text, "Title\n\nline1\nline2\n\npara2");
        assert_eq!(spans(&f), vec![("Title".into(), EntityKind::Bold)]);
    }

    #[test]
    fn clean_code_block() {
        let f = clean("text\n\n```python\nprint(1)\n  x = 2\n```\nafter");
        assert_eq!(f.text, "text\n\nprint(1)\n  x = 2\n\nafter");
        assert_eq!(spans(&f), vec![("print(1)\n  x = 2".into(), EntityKind::Pre { language: "python".into() })]);
    }

    #[test]
    fn clean_lists() {
        let f = clean("- a\n- b\n  - c\n1. x\n2. y\n- [x] done\n- [ ] todo");
        assert_eq!(f.text, "• a\n• b\n  ◦ c\n\n1. x\n2. y\n\n☑ done\n☐ todo");
    }

    #[test]
    fn clean_quote() {
        let f = clean("> quoted **b**\n\nnot");
        assert_eq!(f.text, "quoted b\n\nnot");
        assert_eq!(
            spans(&f),
            vec![("quoted b".into(), EntityKind::Blockquote { collapsed: false }), ("b".into(), EntityKind::Bold)]
        );
    }

    #[test]
    fn clean_table() {
        let f = clean("| a | bb |\n|---|---:|\n| 1 | 2 |\n| 中文 | x |");
        assert_eq!(f.text, "a    │ bb\n─────┼───\n1    │  2\n中文 │  x");
        assert!(matches!(f.spans[0].kind, EntityKind::Pre { .. }));
    }

    #[test]
    fn unsafe_link_is_plain() {
        let f = clean("[x](javascript:alert(1)) [y](relative/path)");
        assert!(f.spans.is_empty(), "{:?}", f.spans);
        assert_eq!(f.text, "x y");
    }

    #[test]
    fn html_is_literal_and_no_underline() {
        let f = clean("<u>not underline</u> <b>x</b>");
        assert_eq!(f.text, "<u>not underline</u> <b>x</b>");
        assert!(f.spans.iter().all(|s| s.kind != EntityKind::Underline));
    }

    #[test]
    fn raw_mode_keeps_source() {
        let md = "# T\n\n**b** `c`\n\n```rs\nfn main(){}\n```\n> q";
        let f = render(md, RenderOptions { mode: Mode::Raw, ..Default::default() });
        assert_eq!(f.text, md);
        let s = spans(&f);
        assert!(s.contains(&("# T".into(), EntityKind::Bold)));
        assert!(s.contains(&("**b**".into(), EntityKind::Bold)));
        assert!(s.contains(&("c".into(), EntityKind::Code)));
        assert!(s.contains(&("fn main(){}".into(), EntityKind::Pre { language: "rs".into() })));
        assert!(s.contains(&("> q".into(), EntityKind::Blockquote { collapsed: false })));
    }

    #[test]
    fn nested_in_quote_has_no_pre() {
        let f = render("x\n\n```\ncode\n```\n\n> q", RenderOptions::nested_in_quote(Mode::Clean));
        assert!(f.spans.iter().all(|s| !matches!(s.kind, EntityKind::Pre { .. } | EntityKind::Blockquote { .. })));
        assert_eq!(f.text, "x\n\ncode\n\n│ q");
    }

    #[test]
    fn streaming_partial_markdown_does_not_panic() {
        let full = "Some **bold** text\n\n```rust\nfn a() {\n    1\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quote\n- [x] t\n$$x^2$$";
        for end in 0..=full.len() {
            if full.is_char_boundary(end) {
                let _ = clean(&full[..end]);
                let _ = render(&full[..end], RenderOptions { mode: Mode::Raw, ..Default::default() });
            }
        }
    }
}
