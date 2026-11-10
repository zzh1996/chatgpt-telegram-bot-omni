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
    let blocks = vec![RBlock::Paragraph(RText::plain(text))];
    send_blocks(app, bot, chat_id, reply_to, blocks).await;
}

/// Send arbitrary bot-generated blocks (command output). Not linked to a turn.
pub async fn send_blocks(app: &Arc<App>, bot: &Arc<BotConn>, chat_id: i64, reply_to: i32, blocks: Vec<RBlock>) {
    // Notices use a synthetic link so they get an idempotent random_id per (inbound message, index).
    let pages = omni_markdown::rich::split_blocks(blocks, crate::compose::PAGE_LIMITS);
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
    let pages = omni_markdown::rich::split_blocks(blocks, crate::compose::PAGE_LIMITS);
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
