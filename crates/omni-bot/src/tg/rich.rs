//! Conversion of rich blocks (omni-markdown mirror types) to TL `PageBlock`s.

use grammers_tl_types as tl;
use omni_markdown::rich::{CellAlign, RBlock, RCell, RItem, RText};

pub fn text(t: &RText) -> tl::enums::RichText {
    match t {
        RText::Plain(s) => {
            if s.is_empty() {
                tl::enums::RichText::TextEmpty
            } else {
                tl::types::TextPlain { text: s.clone() }.into()
            }
        }
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
    if s.is_empty() { tl::enums::RichText::TextEmpty } else { tl::types::TextPlain { text: s.to_string() }.into() }
}

fn item(it: &RItem) -> tl::enums::PageListItem {
    tl::types::PageListItemBlocks {
        checkbox: it.checkbox.is_some(),
        checked: it.checkbox == Some(true),
        blocks: it.blocks.iter().map(block).collect(),
    }
    .into()
}

fn ordered_item(it: &RItem) -> tl::enums::PageListOrderedItem {
    tl::types::PageListOrderedItemBlocks {
        checkbox: it.checkbox.is_some(),
        checked: it.checkbox == Some(true),
        num: None,
        blocks: it.blocks.iter().map(block).collect(),
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

pub fn block(b: &RBlock) -> tl::enums::PageBlock {
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
        RBlock::List { items } => tl::types::PageBlockList { items: items.iter().map(item).collect() }.into(),
        RBlock::OrderedList { start, items } => tl::types::PageBlockOrderedList {
            reversed: false,
            items: items.iter().map(ordered_item).collect(),
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
    }
}

/// Build an `InputRichMessage` from blocks. `noautolink` is always set so that
/// model output can never create live mentions, hashtags or bot commands.
pub fn input_rich_message(blocks: &[RBlock]) -> tl::enums::InputRichMessage {
    tl::types::InputRichMessage {
        rtl: false,
        noautolink: true,
        blocks: blocks.iter().map(block).collect(),
        photos: None,
        documents: None,
        users: None,
    }
    .into()
}
