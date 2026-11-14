//! Incoming rich messages (sent by users, via inline bots, or forwarded):
//! their blocks as Markdown text for the model, and the photos, files and map
//! positions they contain.
//!
//! Markdown keeps the structure (headings, lists, tables, code, quotes, math)
//! that a rich message is made of; plain text would lose it. Media are sent as
//! their own parts (before the text, like the media of any message).

use grammers_tl_types as tl;

/// What a rich message holds.
#[derive(Debug, Default, PartialEq)]
pub struct RichContent {
    pub markdown: String,
    /// Photo ids, in block order.
    pub photo_ids: Vec<i64>,
    /// Document ids (files, videos, audio, inline images), in block order.
    pub document_ids: Vec<i64>,
    /// Map blocks: (latitude, longitude, accuracy in meters).
    pub maps: Vec<(f64, f64, Option<u32>)>,
    /// How deep the conversion is nested now (blocks and inline text).
    depth: usize,
}

/// Limits for crafted messages, far beyond what any real one has: this runs on
/// every message before anything else (even from people who may not use the
/// bot), so a deeply nested or hugely spanning one must not exhaust the stack
/// or memory.
/// - Nesting of blocks and inline text (lists, quotes, folds, formatting).
const MAX_DEPTH: usize = 32;
/// - Columns a table cell spans.
const MAX_COLSPAN: i32 = 64;
/// - Characters of a list item's own number (`num`).
const MAX_NUM_CHARS: usize = 20;

/// Convert the blocks of a rich message.
pub fn convert(blocks: &[tl::enums::PageBlock]) -> RichContent {
    let mut c = RichContent::default();
    let parts: Vec<String> = blocks.iter().map(|b| block(b, &mut c)).filter(|s| !s.trim().is_empty()).collect();
    c.markdown = parts.join("\n\n");
    c
}

fn caption(cap: &tl::enums::PageCaption, c: &mut RichContent) -> String {
    let tl::enums::PageCaption::Caption(cap) = cap;
    let text = inline(&cap.text, c);
    let credit = inline(&cap.credit, c);
    match (text.trim().is_empty(), credit.trim().is_empty()) {
        (false, false) => format!("{text} — {credit}"),
        (false, true) => text,
        (true, false) => credit,
        (true, true) => String::new(),
    }
}

fn quote(s: &str) -> String {
    s.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

fn indent(s: &str, first: &str) -> String {
    let pad = " ".repeat(first.chars().count());
    s.lines()
        .enumerate()
        .map(|(i, l)| match (i, l.is_empty()) {
            (0, _) => format!("{first}{l}"),
            (_, true) => String::new(),
            _ => format!("{pad}{l}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn blocks(bs: &[tl::enums::PageBlock], c: &mut RichContent) -> String {
    bs.iter().map(|b| block(b, c)).filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn checkbox(checkbox: bool, checked: bool) -> &'static str {
    match (checkbox, checked) {
        (true, true) => "[x] ",
        (true, false) => "[ ] ",
        _ => "",
    }
}

fn block(b: &tl::enums::PageBlock, c: &mut RichContent) -> String {
    nested(c, |c| block_at(b, c))
}

/// `f` one level deeper; nothing beyond [`MAX_DEPTH`].
fn nested(c: &mut RichContent, f: impl FnOnce(&mut RichContent) -> String) -> String {
    if c.depth >= MAX_DEPTH {
        return String::new();
    }
    c.depth += 1;
    let s = f(c);
    c.depth -= 1;
    s
}

fn block_at(b: &tl::enums::PageBlock, c: &mut RichContent) -> String {
    use tl::enums::PageBlock as B;
    let heading = |level: usize, t: &tl::enums::RichText, c: &mut RichContent| {
        let s = inline(t, c);
        if s.trim().is_empty() { String::new() } else { format!("{} {s}", "#".repeat(level)) }
    };
    match b {
        B::Paragraph(x) => inline(&x.text, c),
        B::Title(x) => heading(1, &x.text, c),
        B::Header(x) => heading(1, &x.text, c),
        B::Heading1(x) => heading(1, &x.text, c),
        B::Subtitle(x) => heading(2, &x.text, c),
        B::Subheader(x) => heading(2, &x.text, c),
        B::Heading2(x) => heading(2, &x.text, c),
        B::Heading3(x) => heading(3, &x.text, c),
        B::Heading4(x) => heading(4, &x.text, c),
        B::Heading5(x) => heading(5, &x.text, c),
        B::Heading6(x) => heading(6, &x.text, c),
        B::Kicker(x) => inline(&x.text, c),
        B::Footer(x) => inline(&x.text, c),
        B::Thinking(x) => inline(&x.text, c),
        B::AuthorDate(x) => inline(&x.author, c),
        B::Preformatted(x) => {
            let code = plain(&x.text);
            let fence = if code.contains("```") { "````" } else { "```" };
            format!("{fence}{}\n{code}\n{fence}", x.language)
        }
        B::Divider => "---".into(),
        // Nothing for the model (buttons, anchors, a channel card; `InputPageBlockMap`
        // only exists in requests).
        B::Anchor(_) | B::Unsupported | B::Channel(_) | B::ButtonRow(_) | B::InputPageBlockMap(_) => String::new(),
        B::List(x) => x
            .items
            .iter()
            .map(|it| match it {
                tl::enums::PageListItem::Text(t) => {
                    let s = inline(&t.text, c);
                    indent(&s, &format!("- {}", checkbox(t.checkbox, t.checked)))
                }
                tl::enums::PageListItem::Blocks(t) => {
                    let s = blocks(&t.blocks, c);
                    indent(&s, &format!("- {}", checkbox(t.checkbox, t.checked)))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        B::OrderedList(x) => {
            let start = x.start.unwrap_or(1);
            x.items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let n = |num: &Option<String>, value: Option<i32>| match num {
                        Some(num) => num.chars().take(MAX_NUM_CHARS).collect(),
                        None => value.unwrap_or(start.saturating_add(i as i32)).to_string(),
                    };
                    match it {
                        tl::enums::PageListOrderedItem::Text(t) => {
                            let s = inline(&t.text, c);
                            indent(&s, &format!("{}. {}", n(&t.num, t.value), checkbox(t.checkbox, t.checked)))
                        }
                        tl::enums::PageListOrderedItem::Blocks(t) => {
                            let s = blocks(&t.blocks, c);
                            indent(&s, &format!("{}. {}", n(&t.num, t.value), checkbox(t.checkbox, t.checked)))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        B::Blockquote(x) => {
            let mut s = inline(&x.text, c);
            let cap = inline(&x.caption, c);
            if !cap.trim().is_empty() {
                s.push_str(&format!("\n— {cap}"));
            }
            quote(&s)
        }
        B::Pullquote(x) => {
            let mut s = inline(&x.text, c);
            let cap = inline(&x.caption, c);
            if !cap.trim().is_empty() {
                s.push_str(&format!("\n— {cap}"));
            }
            quote(&s)
        }
        B::BlockquoteBlocks(x) => {
            let mut s = blocks(&x.blocks, c);
            let cap = inline(&x.caption, c);
            if !cap.trim().is_empty() {
                s.push_str(&format!("\n\n— {cap}"));
            }
            quote(&s)
        }
        B::Photo(x) => {
            c.photo_ids.push(x.photo_id);
            caption(&x.caption, c)
        }
        B::Video(x) => {
            c.document_ids.push(x.video_id);
            caption(&x.caption, c)
        }
        B::Audio(x) => {
            c.document_ids.push(x.audio_id);
            caption(&x.caption, c)
        }
        B::Document(x) => {
            c.document_ids.push(x.document_id);
            caption(&x.caption, c)
        }
        B::Cover(x) => block(&x.cover, c),
        B::Embed(x) => {
            let cap = caption(&x.caption, c);
            [x.url.clone().unwrap_or_default(), cap]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        B::EmbedPost(x) => {
            let inner = blocks(&x.blocks, c);
            let cap = caption(&x.caption, c);
            [format!("{} ({})", x.author, x.url), inner, cap]
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        B::Collage(x) => gallery(&x.items, &x.caption, c),
        B::Slideshow(x) => gallery(&x.items, &x.caption, c),
        B::Table(x) => table(x, c),
        B::Details(x) => {
            let title = inline(&x.title, c);
            let inner = blocks(&x.blocks, c);
            [if title.trim().is_empty() { String::new() } else { format!("**{title}**") }, inner]
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        B::RelatedArticles(x) => {
            let mut lines = vec![inline(&x.title, c)];
            for a in &x.articles {
                let tl::enums::PageRelatedArticle::Article(a) = a;
                let title = a.title.clone().unwrap_or_else(|| a.url.clone());
                lines.push(format!("- [{title}]({})", a.url));
            }
            lines.into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n")
        }
        B::Map(x) => {
            if let tl::enums::GeoPoint::Point(p) = &x.geo {
                c.maps.push((p.lat, p.long, p.accuracy_radius.map(|r| r.max(0) as u32)));
            }
            caption(&x.caption, c)
        }
        B::Math(x) => format!("$$\n{}\n$$", x.source),
    }
}

fn gallery(items: &[tl::enums::PageBlock], cap: &tl::enums::PageCaption, c: &mut RichContent) -> String {
    let inner = blocks(items, c);
    let cap = caption(cap, c);
    [inner, cap].into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn table(x: &tl::types::PageBlockTable, c: &mut RichContent) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for r in &x.rows {
        let tl::enums::PageTableRow::Row(r) = r;
        let mut row = Vec::new();
        for cell in &r.cells {
            let tl::enums::PageTableCell::Cell(cell) = cell;
            let s = cell.text.as_ref().map(|t| inline(t, c)).unwrap_or_default();
            row.push(s.replace('\n', " ").replace('|', "\\|"));
            // A cell spanning several columns keeps the grid with empty cells.
            for _ in 1..cell.colspan.unwrap_or(1).clamp(1, MAX_COLSPAN) {
                row.push(String::new());
            }
        }
        rows.push(row);
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return inline(&x.title, c);
    }
    let line = |r: &[String]| {
        let mut cells: Vec<String> = r.to_vec();
        cells.resize(width, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut out = Vec::new();
    let title = inline(&x.title, c);
    if !title.trim().is_empty() {
        out.push(title);
        out.push(String::new());
    }
    out.push(line(&rows[0]));
    out.push(format!("|{}", " --- |".repeat(width)));
    for r in &rows[1..] {
        out.push(line(r));
    }
    out.join("\n")
}

/// Text without formatting (code blocks).
fn plain(t: &tl::enums::RichText) -> String {
    let mut c = RichContent::default();
    strip(t, &mut c)
}

fn strip(t: &tl::enums::RichText, c: &mut RichContent) -> String {
    nested(c, |c| strip_at(t, c))
}

fn strip_at(t: &tl::enums::RichText, c: &mut RichContent) -> String {
    use tl::enums::RichText as R;
    match t {
        R::TextEmpty => String::new(),
        R::TextPlain(x) => x.text.clone(),
        R::TextConcat(x) => x.texts.iter().map(|t| strip(t, c)).collect(),
        R::TextMath(x) => x.source.clone(),
        R::TextCustomEmoji(x) => x.alt.clone(),
        R::TextImage(x) => {
            c.document_ids.push(x.document_id);
            String::new()
        }
        R::TextBold(x) => strip(&x.text, c),
        R::TextItalic(x) => strip(&x.text, c),
        R::TextUnderline(x) => strip(&x.text, c),
        R::TextStrike(x) => strip(&x.text, c),
        R::TextFixed(x) => strip(&x.text, c),
        R::TextUrl(x) => strip(&x.text, c),
        R::TextEmail(x) => strip(&x.text, c),
        R::TextSubscript(x) => strip(&x.text, c),
        R::TextSuperscript(x) => strip(&x.text, c),
        R::TextMarked(x) => strip(&x.text, c),
        R::TextPhone(x) => strip(&x.text, c),
        R::TextAnchor(x) => strip(&x.text, c),
        R::TextSpoiler(x) => strip(&x.text, c),
        R::TextMention(x) => strip(&x.text, c),
        R::TextHashtag(x) => strip(&x.text, c),
        R::TextBotCommand(x) => strip(&x.text, c),
        R::TextCashtag(x) => strip(&x.text, c),
        R::TextAutoUrl(x) => strip(&x.text, c),
        R::TextAutoEmail(x) => strip(&x.text, c),
        R::TextAutoPhone(x) => strip(&x.text, c),
        R::TextBankCard(x) => strip(&x.text, c),
        R::TextMentionName(x) => strip(&x.text, c),
        R::TextDate(x) => strip(&x.text, c),
        R::TextDiff(x) => strip(&x.text, c),
        R::TextButton(x) => strip(&x.text, c),
        R::TextTonAddress(x) => strip(&x.text, c),
    }
}

/// Inline text as Markdown.
fn inline(t: &tl::enums::RichText, c: &mut RichContent) -> String {
    nested(c, |c| inline_at(t, c))
}

fn inline_at(t: &tl::enums::RichText, c: &mut RichContent) -> String {
    use tl::enums::RichText as R;
    let wrap = |mark: &str, inner: String| {
        if inner.trim().is_empty() { inner } else { format!("{mark}{inner}{mark}") }
    };
    match t {
        R::TextConcat(x) => x.texts.iter().map(|t| inline(t, c)).collect(),
        R::TextBold(x) => wrap("**", inline(&x.text, c)),
        R::TextItalic(x) => wrap("*", inline(&x.text, c)),
        R::TextStrike(x) => wrap("~~", inline(&x.text, c)),
        R::TextFixed(x) => {
            let s = strip(&x.text, c);
            if s.contains('`') { format!("`` {s} ``") } else { wrap("`", s) }
        }
        R::TextUrl(x) => {
            let s = inline(&x.text, c);
            if s == x.url || x.url.is_empty() { s } else { format!("[{s}]({})", x.url) }
        }
        R::TextEmail(x) => {
            let s = inline(&x.text, c);
            if s == x.email { s } else { format!("[{s}](mailto:{})", x.email) }
        }
        R::TextMath(x) => format!("${}$", x.source),
        R::TextUnderline(x) => inline(&x.text, c),
        R::TextSubscript(x) => inline(&x.text, c),
        R::TextSuperscript(x) => inline(&x.text, c),
        R::TextMarked(x) => inline(&x.text, c),
        R::TextAnchor(x) => inline(&x.text, c),
        R::TextSpoiler(x) => inline(&x.text, c),
        R::TextDate(x) => inline(&x.text, c),
        R::TextDiff(x) => inline(&x.text, c),
        R::TextButton(x) => inline(&x.text, c),
        other => strip(other, c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::enums::RichText as R;

    fn p(s: &str) -> R {
        R::TextPlain(tl::types::TextPlain { text: s.into() })
    }
    fn para(t: R) -> tl::enums::PageBlock {
        tl::enums::PageBlock::Paragraph(tl::types::PageBlockParagraph { text: t })
    }

    #[test]
    fn markdown_of_a_rich_message() {
        let blocks = vec![
            para(R::TextConcat(tl::types::TextConcat {
                texts: vec![p("g$ what is "), R::TextBold(Box::new(tl::types::TextBold { text: p("this") })), p("?")],
            })),
            tl::enums::PageBlock::Heading2(tl::types::PageBlockHeading2 { text: p("Notes") }),
            tl::enums::PageBlock::List(tl::types::PageBlockList {
                items: vec![
                    tl::enums::PageListItem::Text(tl::types::PageListItemText {
                        checkbox: false,
                        checked: false,
                        text: p("one"),
                    }),
                    tl::enums::PageListItem::Text(tl::types::PageListItemText {
                        checkbox: true,
                        checked: true,
                        text: p("done"),
                    }),
                ],
            }),
            tl::enums::PageBlock::Preformatted(tl::types::PageBlockPreformatted {
                text: p("fn main() {}"),
                language: "rust".into(),
            }),
            tl::enums::PageBlock::Photo(tl::types::PageBlockPhoto {
                spoiler: false,
                photo_id: 7,
                caption: tl::enums::PageCaption::Caption(tl::types::PageCaption {
                    text: p("a cat"),
                    credit: R::TextEmpty,
                }),
                url: None,
                webpage_id: None,
            }),
            tl::enums::PageBlock::Map(tl::types::PageBlockMap {
                geo: tl::enums::GeoPoint::Point(tl::types::GeoPoint {
                    long: 121.47,
                    lat: 31.23,
                    access_hash: 0,
                    accuracy_radius: Some(20),
                }),
                zoom: 15,
                w: 100,
                h: 100,
                caption: tl::enums::PageCaption::Caption(tl::types::PageCaption {
                    text: R::TextEmpty,
                    credit: R::TextEmpty,
                }),
            }),
        ];
        let c = convert(&blocks);
        assert_eq!(
            c.markdown,
            "g$ what is **this**?\n\n## Notes\n\n- one\n- [x] done\n\n```rust\nfn main() {}\n```\n\na cat"
        );
        assert_eq!(c.photo_ids, vec![7]);
        assert_eq!(c.maps, vec![(31.23, 121.47, Some(20))]);
    }

    #[test]
    fn crafted_messages_stay_small() {
        // Lists nested 300 deep, bold nested 300 deep: converted up to the depth limit.
        let mut b = para(p("deep"));
        for _ in 0..300 {
            b = tl::enums::PageBlock::List(tl::types::PageBlockList {
                items: vec![tl::enums::PageListItem::Blocks(tl::types::PageListItemBlocks {
                    checkbox: false,
                    checked: false,
                    blocks: vec![b],
                })],
            });
        }
        let mut t = p("bold");
        for _ in 0..300 {
            t = R::TextBold(Box::new(tl::types::TextBold { text: t }));
        }
        let c = convert(&[b, para(t)]);
        assert!(c.markdown.matches('-').count() <= MAX_DEPTH, "{}", c.markdown.len());
        assert!(!c.markdown.contains("deep") && !c.markdown.contains("bold"));
        assert!(c.markdown.len() < 10_000, "{}", c.markdown.len());
        // A cell spanning i32::MAX columns keeps at most MAX_COLSPAN of them.
        let cell = |colspan| {
            tl::enums::PageTableCell::Cell(tl::types::PageTableCell {
                header: false,
                align_center: false,
                align_right: false,
                valign_middle: false,
                valign_bottom: false,
                text: Some(p("x")),
                colspan,
                rowspan: None,
            })
        };
        let table = tl::enums::PageBlock::Table(tl::types::PageBlockTable {
            bordered: true,
            striped: false,
            compact: false,
            title: R::TextEmpty,
            rows: vec![tl::enums::PageTableRow::Row(tl::types::PageTableRow {
                cells: vec![cell(Some(i32::MAX)), cell(Some(-5))],
            })],
        });
        let c = convert(&[table]);
        assert_eq!(c.markdown.lines().next().unwrap().matches('|').count(), MAX_COLSPAN as usize + 2);
    }
}
