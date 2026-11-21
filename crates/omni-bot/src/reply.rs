//! Simple bot notices (command output, errors) as rich messages.

use std::sync::Arc;

use omni_markdown::rich::{RBlock, RText};
use tracing::warn;

use crate::app::App;
use crate::tg::bot::BotConn;
use crate::tg::outbox::{self, Content, Link, SendError, SendReq};
use crate::tg::rich::RichMedia;
use crate::tg::upload::{self, Source};

/// Send a short bot-generated notice replying to `reply_to`, as plain text.
/// (It cannot be mistaken for model output: model replies always start with
/// the `🤖 model` header line.)
pub async fn notice(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, reply_to: i32, text: &str) {
    let blocks = vec![RBlock::Paragraph(crate::compose::with_code(text))];
    send_blocks(app, bot, chat_id, reply_to, blocks).await;
}

/// Pseudo part indexes of the "queued" notice (see [`queued`]) and of its
/// plain-text fallback: below every index [`send_blocks`] uses, so the notice
/// can be found again by the message it replies to.
const QUEUED_PARTS: [i64; 2] = [-1, -2];

fn queued_links(reply_to: i32) -> [Link; 2] {
    QUEUED_PARTS.map(|part_index| Link { turn_id: -(reply_to as i64), attempt_id: None, part_index })
}

/// Tell the sender that their message waits for the answer it continues. The
/// waiting answer's first message replaces it: [`drop_queued`] (owner,
/// 2026-11-22: "真正处理的消息发送出来，就把这条排队的消息删掉").
pub async fn queued(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, reply_to: i32, text: &str) {
    let [rich_link, text_link] = queued_links(reply_to);
    let blocks = vec![RBlock::Paragraph(crate::compose::with_code(text))];
    let content = Content::Rich(blocks.clone());
    let req = SendReq { chat_id, reply_to: Some(reply_to), content: &content, link: rich_link, media: None };
    let res = match outbox::send(&app.db, bot, req).await {
        Err(SendError::Rejected(_)) => {
            let content = Content::Text(crate::compose::blocks_to_plain(&blocks));
            let req = SendReq { chat_id, reply_to: Some(reply_to), content: &content, link: text_link, media: None };
            outbox::send(&app.db, bot, req).await
        }
        r => r,
    };
    if let Err(e) = res {
        warn!(error = %e, "failed to send the queued notice");
    }
}

/// Delete the "queued" notice that replies to the message `reply_to`, if one
/// is shown (the first message of an answer to it is out).
pub async fn drop_queued(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, reply_to: i32) {
    match outbox::sent_ids(&app.db, &bot.name, chat_id, &queued_links(reply_to)).await {
        Ok(ids) if !ids.is_empty() => {
            if let Err(e) = outbox::delete(&app.db, bot, chat_id, &ids).await {
                warn!(error = %format!("{e:#}"), "failed to delete the queued notice");
            }
        }
        Ok(_) => {}
        Err(e) => warn!(error = %format!("{e:#}"), "failed to look up the queued notice"),
    }
}

/// Output in several messages: each one says which part it is, the first too
/// (owner, 2026-11-14), so that nobody takes it for all of it. An output that
/// starts with a `🤖 title` line has it on every message with ` · part N`; any
/// other gets a small `part N` line first.
pub fn label_parts(pages: Vec<Vec<RBlock>>) -> Vec<Vec<RBlock>> {
    if pages.len() <= 1 {
        return pages;
    }
    let total = pages.len();
    let title = match pages[0].first() {
        Some(b @ (RBlock::Paragraph(t) | RBlock::Footer(t))) if t.to_plain().starts_with('🤖') => {
            Some((t.clone(), matches!(b, RBlock::Footer(_))))
        }
        _ => None,
    };
    pages
        .into_iter()
        .enumerate()
        .map(|(i, mut p)| {
            let part = crate::compose::PartOf { n: i + 1, of: Some(total) }.label();
            match &title {
                Some((t, footer)) => {
                    let line = RText::concat(vec![t.clone(), RText::plain(format!(" · {part}"))]);
                    let block = if *footer { RBlock::Footer(line) } else { RBlock::Paragraph(line) };
                    if i == 0 {
                        p[0] = block;
                    } else {
                        p.insert(0, block);
                    }
                }
                None => p.insert(0, RBlock::Footer(RText::plain(part))),
            }
            p
        })
        .collect()
}

/// Send arbitrary bot-generated blocks (command output). Not linked to a turn.
pub async fn send_blocks(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, reply_to: i32, blocks: Vec<RBlock>) {
    // Notices use a synthetic link so they get an idempotent random_id per (inbound message, index).
    let pages = label_parts(omni_markdown::rich::split_blocks(blocks, crate::compose::PAGE_LIMITS));
    let mut prev = reply_to;
    for (i, p) in pages.into_iter().enumerate() {
        let content = Content::Rich(p.clone());
        // Same content => same row (idempotent after a crash); different notices => different rows.
        let h = i64::from_str_radix(&content.hash()[..12], 16).unwrap_or(0);
        let link = Link { turn_id: -(reply_to as i64), attempt_id: None, part_index: (h << 8) + i as i64 };
        match outbox::send(
            &app.db,
            bot,
            SendReq { chat_id, reply_to: Some(prev), content: &content, link, media: None },
        )
        .await
        {
            Ok(id) => prev = id,
            Err(SendError::Rejected(_)) => {
                let f = crate::compose::blocks_to_plain(&p);
                let content = Content::Text(f);
                let link =
                    Link { turn_id: -(reply_to as i64), attempt_id: None, part_index: (h << 8) + 128 + i as i64 };
                if let Ok(id) = outbox::send(
                    &app.db,
                    bot,
                    SendReq { chat_id, reply_to: Some(prev), content: &content, link, media: None },
                )
                .await
                {
                    prev = id;
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to send notice");
                return;
            }
        }
    }
}

/// Send bot-generated blocks that embed media (uploaded first, cached) as one
/// message where the limits allow. Not linked to a turn.
/// Send ready pages (each one message), chained as replies, with their media.
pub async fn send_pages_with_media(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    chat_id: i64,
    reply_to: i32,
    pages: Vec<Vec<RBlock>>,
    sources: &[Source],
) -> anyhow::Result<()> {
    let mut media = RichMedia::default();
    upload::ensure(app, bot, chat_id, sources, &mut media).await?;
    let mut prev = reply_to;
    for (i, p) in pages.into_iter().enumerate() {
        let content = Content::Rich(p);
        let h = i64::from_str_radix(&content.hash()[..12], 16).unwrap_or(0);
        let link = Link { turn_id: -(reply_to as i64), attempt_id: None, part_index: (h << 8) + i as i64 };
        let req = SendReq { chat_id, reply_to: Some(prev), content: &content, link, media: Some(&media) };
        prev = outbox::send(&app.db, bot, req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

pub async fn send_blocks_with_media(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    chat_id: i64,
    reply_to: i32,
    blocks: Vec<RBlock>,
    sources: &[Source],
) -> anyhow::Result<()> {
    let mut media = RichMedia::default();
    upload::ensure(app, bot, chat_id, sources, &mut media).await?;
    let pages = label_parts(omni_markdown::rich::split_blocks(blocks, crate::compose::PAGE_LIMITS));
    let mut prev = reply_to;
    for (i, p) in pages.into_iter().enumerate() {
        let content = Content::Rich(p);
        let h = i64::from_str_radix(&content.hash()[..12], 16).unwrap_or(0);
        let link = Link { turn_id: -(reply_to as i64), attempt_id: None, part_index: (h << 8) + i as i64 };
        let req = SendReq { chat_id, reply_to: Some(prev), content: &content, link, media: Some(&media) };
        prev = outbox::send(&app.db, bot, req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}
