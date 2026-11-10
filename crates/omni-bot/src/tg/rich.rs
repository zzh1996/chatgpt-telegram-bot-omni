//! Conversion of rich blocks (omni-markdown mirror types) to TL `PageBlock`s.

use std::collections::HashMap;

use grammers_tl_types as tl;
use omni_markdown::rich::{CellAlign, RBlock, RCell, RItem, RText};

/// Uploaded media referenced by `Photo`/`Document` blocks, by media key.
#[derive(Debug, Clone, Default)]
pub struct RichMedia {
    pub items: HashMap<String, Uploaded>,
}

/// What Telegram holds for a media key.
#[derive(Debug, Clone)]
pub enum Uploaded {
    Photo(tl::types::InputPhoto),
    /// Also used for images Telegram would not take as photos (too large, odd dimensions).
    Document(tl::types::InputDocument),
}

fn caption(t: &RText) -> tl::enums::PageCaption {
    tl::types::PageCaption { text: text(t), credit: tl::enums::RichText::TextEmpty }.into()
}

fn media_block(key: &str, cap: &RText, media: &RichMedia) -> tl::enums::PageBlock {
    match media.items.get(key) {
        Some(Uploaded::Photo(p)) => tl::types::PageBlockPhoto {
            spoiler: false,
            photo_id: p.id,
            caption: caption(cap),
            url: None,
            webpage_id: None,
        }
        .into(),
        Some(Uploaded::Document(d)) => tl::types::PageBlockDocument { document_id: d.id, caption: caption(cap) }.into(),
        // Not uploaded (should not happen: senders resolve media first).
        None => tl::types::PageBlockParagraph { text: plain("[media unavailable]") }.into(),
    }
}

/// Telegram silently truncates every text string of a rich message to 70000
/// bytes of UTF-8 — and stores it **empty** when the cut falls inside a
/// character (measured against the live API, 2026-11-11: 23334 CJK characters
/// in one paragraph were stored as nothing, 17501 emoji lost the last one).
/// Strings are therefore sent as several adjacent text nodes of at most this
/// size, which render exactly like one.
pub const NODE_MAX_BYTES: usize = 65_000;

/// Split `s` at character boundaries into pieces of at most [`NODE_MAX_BYTES`].
pub fn node_chunks(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while rest.len() > NODE_MAX_BYTES {
        let mut cut = NODE_MAX_BYTES;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

fn plain_node(s: &str) -> tl::enums::RichText {
    if s.is_empty() {
        return tl::enums::RichText::TextEmpty;
    }
    let chunks = node_chunks(s);
    if chunks.len() == 1 {
        return tl::types::TextPlain { text: s.to_string() }.into();
    }
    tl::types::TextConcat {
        texts: chunks.into_iter().map(|c| tl::types::TextPlain { text: c.to_string() }.into()).collect(),
    }
    .into()
}

pub fn text(t: &RText) -> tl::enums::RichText {
    match t {
        RText::Plain(s) => plain_node(s),
        RText::Bold(x) => tl::types::TextBold { text: text(x) }.into(),
        RText::Italic(x) => tl::types::TextItalic { text: text(x) }.into(),
        RText::Underline(x) => tl::types::TextUnderline { text: text(x) }.into(),
        RText::Strike(x) => tl::types::TextStrike { text: text(x) }.into(),
        RText::Fixed(x) => tl::types::TextFixed { text: text(x) }.into(),
        RText::Spoiler(x) => tl::types::TextSpoiler { text: text(x) }.into(),
        RText::Url { text: x, url } => tl::types::TextUrl { text: text(x), url: url.clone(), webpage_id: 0 }.into(),
        RText::Math(s) => tl::types::TextMath { source: s.clone() }.into(),
        RText::Concat(v) => tl::types::TextConcat { texts: v.iter().map(text).collect() }.into(),
    }
}

fn plain(s: &str) -> tl::enums::RichText {
    plain_node(s)
}

fn item(it: &RItem, m: &RichMedia) -> tl::enums::PageListItem {
    tl::types::PageListItemBlocks {
        checkbox: it.checkbox.is_some(),
        checked: it.checkbox == Some(true),
        blocks: it.blocks.iter().map(|b| block_with(b, m)).collect(),
    }
    .into()
}

fn ordered_item(it: &RItem, m: &RichMedia) -> tl::enums::PageListOrderedItem {
    tl::types::PageListOrderedItemBlocks {
        checkbox: it.checkbox.is_some(),
        checked: it.checkbox == Some(true),
        num: None,
        blocks: it.blocks.iter().map(|b| block_with(b, m)).collect(),
        value: None,
        r#type: None,
    }
    .into()
}

fn cell(c: &RCell) -> tl::enums::PageTableCell {
    tl::types::PageTableCell {
        header: c.header,
        align_center: c.align == CellAlign::Center,
        align_right: c.align == CellAlign::Right,
        valign_middle: false,
        valign_bottom: false,
        text: Some(if c.text.is_empty() { plain(" ") } else { text(&c.text) }),
        colspan: None,
        rowspan: None,
    }
    .into()
}

fn block_with(b: &RBlock, m: &RichMedia) -> tl::enums::PageBlock {
    let block = |b: &RBlock| block_with(b, m);
    match b {
        RBlock::Paragraph(t) => tl::types::PageBlockParagraph { text: text(t) }.into(),
        RBlock::Heading { level, text: t } => {
            let t = text(t);
            match level {
                1 => tl::types::PageBlockHeading1 { text: t }.into(),
                2 => tl::types::PageBlockHeading2 { text: t }.into(),
                3 => tl::types::PageBlockHeading3 { text: t }.into(),
                4 => tl::types::PageBlockHeading4 { text: t }.into(),
                5 => tl::types::PageBlockHeading5 { text: t }.into(),
                _ => tl::types::PageBlockHeading6 { text: t }.into(),
            }
        }
        RBlock::Pre { text: s, language } => {
            tl::types::PageBlockPreformatted { text: plain(s), language: language.clone() }.into()
        }
        RBlock::Footer(t) => tl::types::PageBlockFooter { text: text(t) }.into(),
        RBlock::Divider => tl::enums::PageBlock::Divider,
        RBlock::Math(s) => tl::types::PageBlockMath { source: s.clone() }.into(),
        RBlock::List { items } => tl::types::PageBlockList { items: items.iter().map(|i| item(i, m)).collect() }.into(),
        RBlock::OrderedList { start, items } => tl::types::PageBlockOrderedList {
            reversed: false,
            items: items.iter().map(|i| ordered_item(i, m)).collect(),
            start: if *start == 1 { None } else { Some(*start) },
            r#type: None,
        }
        .into(),
        RBlock::Quote { blocks } => tl::types::PageBlockBlockquoteBlocks {
            blocks: blocks.iter().map(block).collect(),
            caption: tl::enums::RichText::TextEmpty,
        }
        .into(),
        RBlock::CollapsedQuote { text: t } => {
            tl::types::PageBlockBlockquote { collapsed: true, text: text(t), caption: tl::enums::RichText::TextEmpty }
                .into()
        }
        RBlock::Table { rows, bordered, striped } => tl::types::PageBlockTable {
            bordered: *bordered,
            striped: *striped,
            compact: false,
            title: tl::enums::RichText::TextEmpty,
            rows: rows.iter().map(|r| tl::types::PageTableRow { cells: r.iter().map(cell).collect() }.into()).collect(),
        }
        .into(),
        RBlock::Details { open, title, blocks } => {
            tl::types::PageBlockDetails { open: *open, blocks: blocks.iter().map(block).collect(), title: text(title) }
                .into()
        }
        RBlock::Photo { key, caption: c } | RBlock::Document { key, caption: c } => media_block(key, c, m),
    }
}

/// Build an `InputRichMessage` from blocks, with the uploaded media they reference.
/// `noautolink` is always set so that model output can never create live
/// mentions, hashtags or bot commands.
pub fn input_rich_message(blocks: &[RBlock], media: &RichMedia) -> tl::enums::InputRichMessage {
    let mut photos = Vec::new();
    let mut documents = Vec::new();
    for key in omni_markdown::rich::media_keys(blocks) {
        match media.items.get(&key) {
            Some(Uploaded::Photo(p))
                if !photos
                    .iter()
                    .any(|x: &tl::enums::InputPhoto| matches!(x, tl::enums::InputPhoto::Photo(y) if y.id == p.id)) =>
            {
                photos.push(p.clone().into())
            }
            Some(Uploaded::Document(d))
                if !documents.iter().any(
                    |x: &tl::enums::InputDocument| matches!(x, tl::enums::InputDocument::Document(y) if y.id == d.id),
                ) =>
            {
                documents.push(d.clone().into())
            }
            _ => {}
        }
    }
    tl::types::InputRichMessage {
        rtl: false,
        noautolink: true,
        blocks: blocks.iter().map(|b| block_with(b, media)).collect(),
        photos: (!photos.is_empty()).then_some(photos),
        documents: (!documents.is_empty()).then_some(documents),
        users: None,
    }
    .into()
}

/// All text of a received rich message, in block order (for read-back checks):
/// the same traversal as [`expected_text`].
pub fn received_text(blocks: &[tl::enums::PageBlock]) -> String {
    fn rt(t: &tl::enums::RichText, out: &mut String) {
        use tl::enums::RichText as R;
        match t {
            R::TextEmpty => {}
            R::TextPlain(x) => out.push_str(&x.text),
            R::TextBold(x) => rt(&x.text, out),
            R::TextItalic(x) => rt(&x.text, out),
            R::TextUnderline(x) => rt(&x.text, out),
            R::TextStrike(x) => rt(&x.text, out),
            R::TextFixed(x) => rt(&x.text, out),
            R::TextSpoiler(x) => rt(&x.text, out),
            R::TextUrl(x) => rt(&x.text, out),
            R::TextMath(x) => out.push_str(&x.source),
            R::TextConcat(x) => x.texts.iter().for_each(|t| rt(t, out)),
            other => out.push_str(&format!("<{other:?}>")),
        }
    }
    fn cap(c: &tl::enums::PageCaption, out: &mut String) {
        let tl::enums::PageCaption::Caption(c) = c;
        rt(&c.text, out);
    }
    fn blk(b: &tl::enums::PageBlock, out: &mut String) {
        use tl::enums::PageBlock as B;
        out.push('\u{1}'); // block separator
        match b {
            B::Paragraph(x) => rt(&x.text, out),
            B::Preformatted(x) => rt(&x.text, out),
            B::Footer(x) => rt(&x.text, out),
            B::Heading1(x) => rt(&x.text, out),
            B::Heading2(x) => rt(&x.text, out),
            B::Heading3(x) => rt(&x.text, out),
            B::Heading4(x) => rt(&x.text, out),
            B::Heading5(x) => rt(&x.text, out),
            B::Heading6(x) => rt(&x.text, out),
            B::Math(x) => out.push_str(&x.source),
            B::Divider => {}
            B::List(x) => {
                for it in &x.items {
                    match it {
                        tl::enums::PageListItem::Text(t) => rt(&t.text, out),
                        tl::enums::PageListItem::Blocks(t) => t.blocks.iter().for_each(|b| blk(b, out)),
                    }
                }
            }
            B::OrderedList(x) => {
                for it in &x.items {
                    match it {
                        tl::enums::PageListOrderedItem::Text(t) => rt(&t.text, out),
                        tl::enums::PageListOrderedItem::Blocks(t) => t.blocks.iter().for_each(|b| blk(b, out)),
                    }
                }
            }
            B::BlockquoteBlocks(x) => x.blocks.iter().for_each(|b| blk(b, out)),
            B::Blockquote(x) => rt(&x.text, out),
            B::Table(x) => {
                for r in &x.rows {
                    let tl::enums::PageTableRow::Row(r) = r;
                    for c in &r.cells {
                        let tl::enums::PageTableCell::Cell(c) = c;
                        if let Some(t) = &c.text {
                            rt(t, out);
                        }
                    }
                }
            }
            B::Details(x) => {
                rt(&x.title, out);
                x.blocks.iter().for_each(|b| blk(b, out));
            }
            B::Photo(x) => cap(&x.caption, out),
            B::Document(x) => cap(&x.caption, out),
            other => out.push_str(&format!("<{other:?}>")),
        }
    }
    let mut out = String::new();
    blocks.iter().for_each(|b| blk(b, &mut out));
    out
}

/// The text Telegram should store for `blocks` (same traversal as [`received_text`]).
pub fn expected_text(blocks: &[RBlock]) -> String {
    fn blk(b: &RBlock, out: &mut String) {
        out.push('\u{1}');
        match b {
            RBlock::Paragraph(t)
            | RBlock::Heading { text: t, .. }
            | RBlock::Footer(t)
            | RBlock::CollapsedQuote { text: t }
            | RBlock::Photo { caption: t, .. }
            | RBlock::Document { caption: t, .. } => out.push_str(&t.to_plain()),
            RBlock::Pre { text, .. } => out.push_str(text),
            RBlock::Math(s) => out.push_str(s),
            RBlock::Divider => {}
            RBlock::List { items } | RBlock::OrderedList { items, .. } => {
                items.iter().flat_map(|i| &i.blocks).for_each(|b| blk(b, out))
            }
            RBlock::Quote { blocks } => blocks.iter().for_each(|b| blk(b, out)),
            RBlock::Table { rows, .. } => {
                for r in rows {
                    for c in r {
                        // Empty cells are sent as a single space.
                        let t = c.text.to_plain();
                        out.push_str(if t.is_empty() { " " } else { &t });
                    }
                }
            }
            RBlock::Details { title, blocks, .. } => {
                out.push_str(&title.to_plain());
                blocks.iter().for_each(|b| blk(b, out));
            }
        }
    }
    let mut out = String::new();
    blocks.iter().for_each(|b| blk(b, &mut out));
    out
}
