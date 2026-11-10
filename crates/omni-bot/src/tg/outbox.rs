//! Idempotent message sending and editing.
//!
//! Every outgoing message gets a `random_id` that is persisted *before* the
//! first attempt. Telegram never forgets used random_ids: re-sending with the
//! same id returns the already-sent message instead of a duplicate
//! (`RANDOM_ID_DUPLICATE` only means the first call is still in flight). So a
//! crash at any point can be recovered by simply re-sending — no duplicates, no
//! lost messages.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use grammers_mtsender::InvocationError;
use grammers_tl_types as tl;
use omni_markdown::Formatted;
use omni_markdown::rich::RBlock;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::bot::BotConn;
use super::convert::{formatted_to_tl, sent_message_id};
use super::rich::{RichMedia, input_rich_message};
use crate::store::{Db, now_ms};

/// What a Telegram message should display.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Rich(Vec<RBlock>),
    Text(Formatted),
}

impl Content {
    pub fn hash(&self) -> String {
        let s = match self {
            Content::Rich(b) => serde_json::to_string(b).unwrap_or_default(),
            Content::Text(f) => serde_json::to_string(f).unwrap_or_default(),
        };
        hex::encode(Sha256::digest(s.as_bytes()))
    }
}

/// Links an outgoing message to the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub turn_id: i64,
    pub attempt_id: Option<i64>,
    pub part_index: i64,
}

#[derive(Debug)]
pub enum SendError {
    /// Telegram rejected this content (e.g. too long, invalid entities, unsupported rich block).
    /// The caller may retry with fallback content.
    Rejected(grammers_mtsender::RpcError),
    /// Anything else (chat not writable, network gave up, ...).
    Other(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Rejected(e) => write!(f, "rejected by Telegram: {e}"),
            SendError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

const CONTENT_ERRORS: &[&str] = &[
    "MESSAGE_TOO_LONG",
    "MESSAGE_EMPTY",
    "ENTITIES_TOO_LONG",
    "ENTITY_BOUNDS_INVALID",
    "ENTITY_MENTION_USER_INVALID",
    "ENTITY_TEXTURL_INVALID",
    "BUTTON_URL_INVALID",
    "RICH_*",
    "PAGE_*",
    "MEDIA_CAPTION_TOO_LONG",
    "MEDIA_EMPTY",
    "URL_INVALID",
];

/// 400 errors that are not about the content (fallback rendering cannot help).
const NON_CONTENT_400: &[&str] = &[
    "PEER_ID_INVALID",
    "CHAT_*",
    "USER_*",
    "MESSAGE_ID_INVALID",
    "MSG_ID_INVALID",
    "REPLY_*",
    "INPUT_USER_DEACTIVATED",
    "CHANNEL_*",
    "TOPIC_*",
];

fn is_content_error(e: &InvocationError) -> bool {
    if CONTENT_ERRORS.iter().any(|n| e.is(n)) {
        return true;
    }
    // Any other bad-request error is most likely about the (new, rich) content:
    // let the caller degrade the rendering instead of giving up.
    matches!(e, InvocationError::Rpc(r) if r.code == 400) && !NON_CONTENT_400.iter().any(|n| e.is(n))
}

fn flood_secs(e: &InvocationError) -> Option<u32> {
    match e {
        InvocationError::Rpc(r) if r.code == 420 => Some(r.value.unwrap_or(5)),
        _ => None,
    }
}

fn transient(e: &InvocationError) -> bool {
    match e {
        InvocationError::Io(_) | InvocationError::Dropped(_) | InvocationError::Transport(_) => true,
        InvocationError::Rpc(r) => r.code >= 500 || r.name == "RANDOM_ID_DUPLICATE",
        _ => false,
    }
}

pub struct SendReq<'a> {
    pub chat_id: i64,
    pub reply_to: Option<i32>,
    pub content: &'a Content,
    pub link: Link,
    /// Uploaded media referenced by `Photo`/`Document` blocks of rich content.
    pub media: Option<&'a RichMedia>,
}

/// Ensure the message for `link` exists in Telegram; returns its id.
///
/// If a send for this link was attempted before (possibly before a crash), the
/// same random_id is reused, which returns the already-sent message.
pub async fn send(db: &Db, bot: &BotConn, req: SendReq<'_>) -> Result<i32, SendError> {
    let (random_id, existing) = reserve(db, &bot.name, &req).await.map_err(SendError::Other)?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let peer = bot.input_peer(req.chat_id).await.map_err(SendError::Other)?;
    let gate = bot.gates.get(req.chat_id);
    let mut reply_to = req.reply_to;
    let mut tries = 0u32;
    loop {
        tries += 1;
        let permit = gate.acquire().await;
        let reply = reply_to.map(|id| {
            tl::enums::InputReplyTo::Message(tl::types::InputReplyToMessage {
                reply_to_msg_id: id,
                top_msg_id: None,
                reply_to_peer_id: None,
                quote_text: None,
                quote_entities: None,
                quote_offset: None,
                monoforum_peer_id: None,
                todo_item_id: None,
                poll_option: None,
                quote_rich: None,
            })
        });
        let (message, entities, rich_message) = match req.content {
            Content::Rich(blocks) => {
                (String::new(), None, Some(input_rich_message(blocks, req.media.unwrap_or(&RichMedia::default()))))
            }
            Content::Text(f) => {
                let (t, e) = formatted_to_tl(f);
                (t, if e.is_empty() { None } else { Some(e) }, None)
            }
        };
        let call = tl::functions::messages::SendMessage {
            no_webpage: true,
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: peer.clone(),
            reply_to: reply,
            message,
            random_id,
            reply_markup: None,
            entities,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
            rich_message,
        };
        match bot.invoke(&call).await {
            Ok(updates) => {
                drop(permit);
                let Some(id) = sent_message_id(&updates, random_id) else {
                    return Err(SendError::Other(anyhow!("sent message id not found in response")));
                };
                confirm(db, &bot.name, req.chat_id, random_id, id, req.link, &req.content.hash())
                    .await
                    .map_err(SendError::Other)?;
                debug!(bot = %bot.name, chat = req.chat_id, msg = id, random_id, "message sent");
                return Ok(id);
            }
            Err(e) => {
                if let Some(s) = flood_secs(&e) {
                    warn!(bot = %bot.name, chat = req.chat_id, secs = s, "flood wait on send");
                    permit.flood_wait(Duration::from_secs(s as u64 + 1));
                    continue;
                }
                drop(permit);
                if e.is("REPLY_MESSAGE_ID_INVALID") || e.is("MESSAGE_ID_INVALID") && reply_to.is_some() {
                    warn!(bot = %bot.name, chat = req.chat_id, "replied-to message is gone; sending without reply");
                    reply_to = None;
                    continue;
                }
                if is_content_error(&e)
                    && let InvocationError::Rpc(r) = e
                {
                    return Err(SendError::Rejected(r));
                }
                if transient(&e) && tries < 30 {
                    let wait = Duration::from_secs((2u64 << tries.min(5)).min(30));
                    warn!(bot = %bot.name, chat = req.chat_id, error = %e, ?wait, "transient send error; retrying with the same random_id");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(SendError::Other(anyhow!("send failed: {e}")));
            }
        }
    }
}

/// Edit a message's content. `MESSAGE_NOT_MODIFIED` counts as success.
pub async fn edit(
    db: &Db,
    bot: &BotConn,
    chat_id: i64,
    msg_id: i32,
    content: &Content,
    media: Option<&RichMedia>,
) -> Result<(), SendError> {
    let peer = bot.input_peer(chat_id).await.map_err(SendError::Other)?;
    let gate = bot.gates.get(chat_id);
    let mut tries = 0u32;
    loop {
        tries += 1;
        let permit = gate.acquire().await;
        let (message, entities, rich_message) = match content {
            Content::Rich(blocks) => {
                (Some(String::new()), None, Some(input_rich_message(blocks, media.unwrap_or(&RichMedia::default()))))
            }
            Content::Text(f) => {
                let (t, e) = formatted_to_tl(f);
                (Some(t), Some(e), None)
            }
        };
        let call = tl::functions::messages::EditMessage {
            no_webpage: true,
            invert_media: false,
            peer: peer.clone(),
            id: msg_id,
            message,
            media: None,
            reply_markup: None,
            entities,
            schedule_date: None,
            schedule_repeat_period: None,
            quick_reply_shortcut_id: None,
            rich_message,
        };
        match bot.invoke(&call).await {
            Ok(_) => break,
            Err(e) if e.is("MESSAGE_NOT_MODIFIED") => break,
            Err(e) if e.is("MESSAGE_ID_INVALID") => {
                // The message was deleted (e.g. by an admin): nothing to update.
                warn!(bot = %bot.name, chat = chat_id, msg = msg_id, "message to edit no longer exists");
                return Ok(());
            }
            Err(e) => {
                if let Some(s) = flood_secs(&e) {
                    warn!(bot = %bot.name, chat = chat_id, secs = s, "flood wait on edit");
                    permit.flood_wait(Duration::from_secs(s as u64 + 1));
                    continue;
                }
                drop(permit);
                if is_content_error(&e)
                    && let InvocationError::Rpc(r) = e
                {
                    return Err(SendError::Rejected(r));
                }
                if transient(&e) && tries < 20 {
                    tokio::time::sleep(Duration::from_secs((2u64 << tries.min(4)).min(30))).await;
                    continue;
                }
                return Err(SendError::Other(anyhow!("edit failed: {e}")));
            }
        }
    }
    let hash = content.hash();
    let bot_name = bot.name.clone();
    db.call(move |c| {
        c.execute(
            "UPDATE outbox SET content_hash = ?1, updated_at = ?2 WHERE bot = ?3 AND chat_id = ?4 AND msg_id = ?5",
            params![hash, now_ms(), bot_name, chat_id, msg_id],
        )?;
        Ok(())
    })
    .await
    .map_err(SendError::Other)?;
    Ok(())
}

/// Delete messages (used when a reply shrinks to fewer messages).
pub async fn delete(db: &Db, bot: &BotConn, chat_id: i64, ids: &[i32]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let gate = bot.gates.get(chat_id);
    loop {
        let permit = gate.acquire().await;
        let res = if super::convert::chat_kind(chat_id) == super::convert::ChatKind::Channel {
            let channel = bot.input_channel(chat_id).await?;
            bot.invoke(&tl::functions::channels::DeleteMessages { channel, id: ids.to_vec() }).await.map(|_| ())
        } else {
            bot.invoke(&tl::functions::messages::DeleteMessages { revoke: true, id: ids.to_vec() }).await.map(|_| ())
        };
        match res {
            Ok(()) => break,
            Err(e) => {
                if let Some(s) = flood_secs(&e) {
                    permit.flood_wait(Duration::from_secs(s as u64 + 1));
                    continue;
                }
                warn!(bot = %bot.name, chat = chat_id, error = %e, "delete failed");
                break;
            }
        }
    }
    let bot_name = bot.name.clone();
    let ids = ids.to_vec();
    db.call(move |c| {
        for id in ids {
            c.execute(
                "UPDATE outbox SET state = 'deleted', updated_at = ?1 WHERE bot = ?2 AND chat_id = ?3 AND msg_id = ?4",
                params![now_ms(), bot_name, chat_id, id],
            )?;
        }
        Ok(())
    })
    .await
}

/// Reserve (or find) the outbox row for a link. Returns (random_id, already-sent msg id).
async fn reserve(db: &Db, bot: &str, req: &SendReq<'_>) -> Result<(i64, Option<i32>)> {
    let bot = bot.to_string();
    let (chat, reply_to, link) = (req.chat_id, req.reply_to, req.link);
    db.call(move |c| {
        let existing: Option<(i64, Option<i32>, String)> = c
            .query_row(
                "SELECT random_id, msg_id, state FROM outbox
                 WHERE bot = ?1 AND chat_id = ?2 AND turn_id = ?3 AND attempt_id IS ?4 AND part_index = ?5
                 ORDER BY id DESC LIMIT 1",
                params![bot, chat, link.turn_id, link.attempt_id, link.part_index],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((rid, msg, state)) = existing
            && state != "deleted" {
                return Ok((rid, msg));
            }
        let rid: i64 = loop {
            let r: i64 = rand::random();
            if r != 0 {
                break r;
            }
        };
        let now = now_ms();
        c.execute(
            "INSERT INTO outbox(bot, chat_id, turn_id, attempt_id, part_index, random_id, reply_to, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?8)",
            params![bot, chat, link.turn_id, link.attempt_id, link.part_index, rid, reply_to, now],
        )?;
        Ok((rid, None))
    })
    .await
    .context("reserve outbox row")
}

async fn confirm(db: &Db, bot: &str, chat: i64, random_id: i64, msg_id: i32, link: Link, hash: &str) -> Result<()> {
    let (bot, hash) = (bot.to_string(), hash.to_string());
    db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE outbox SET msg_id = ?1, state = 'sent', content_hash = ?2, updated_at = ?3 WHERE bot = ?4 AND random_id = ?5",
            params![msg_id, hash, now_ms(), bot, random_id],
        )?;
        // Notices (negative pseudo turn ids) are not part of any conversation.
        if link.turn_id > 0 {
            tx.execute(
                "INSERT OR REPLACE INTO tg_messages(bot, chat_id, msg_id, turn_id, attempt_id, part_index) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![bot, chat, msg_id, link.turn_id, link.attempt_id, link.part_index],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Send a single photo or document (generated images, /raw files). Idempotent per link.
pub async fn send_file(
    db: &Db,
    bot: &BotConn,
    chat_id: i64,
    reply_to: Option<i32>,
    bytes: Vec<u8>,
    filename: &str,
    mime: &str,
    as_photo: bool,
    caption: &str,
    link: Link,
) -> Result<i32, SendError> {
    let dummy = Content::Text(Formatted::plain(caption));
    let req = SendReq { chat_id, reply_to, content: &dummy, link, media: None };
    let (random_id, existing) = reserve(db, &bot.name, &req).await.map_err(SendError::Other)?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let peer = bot.input_peer(chat_id).await.map_err(SendError::Other)?;
    let size = bytes.len();
    let mut cursor = std::io::Cursor::new(bytes);
    let uploaded = bot
        .client
        .upload_stream(&mut cursor, size, filename.to_string())
        .await
        .map_err(|e| SendError::Other(anyhow!("upload failed: {e}")))?;
    let media: tl::enums::InputMedia = if as_photo {
        tl::types::InputMediaUploadedPhoto {
            spoiler: false,
            live_photo: false,
            file: uploaded.raw.clone(),
            stickers: None,
            ttl_seconds: None,
            video: None,
        }
        .into()
    } else {
        tl::types::InputMediaUploadedDocument {
            nosound_video: false,
            force_file: true,
            spoiler: false,
            file: uploaded.raw.clone(),
            thumb: None,
            mime_type: mime.to_string(),
            attributes: vec![tl::types::DocumentAttributeFilename { file_name: filename.to_string() }.into()],
            stickers: None,
            video_cover: None,
            video_timestamp: None,
            ttl_seconds: None,
        }
        .into()
    };
    let gate = bot.gates.get(chat_id);
    let mut tries = 0;
    loop {
        tries += 1;
        let permit = gate.acquire().await;
        let call = tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: peer.clone(),
            reply_to: reply_to.map(|id| {
                tl::enums::InputReplyTo::Message(tl::types::InputReplyToMessage {
                    reply_to_msg_id: id,
                    top_msg_id: None,
                    reply_to_peer_id: None,
                    quote_text: None,
                    quote_entities: None,
                    quote_offset: None,
                    monoforum_peer_id: None,
                    todo_item_id: None,
                    poll_option: None,
                    quote_rich: None,
                })
            }),
            media: media.clone(),
            message: caption.to_string(),
            random_id,
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
        };
        match bot.invoke(&call).await {
            Ok(updates) => {
                drop(permit);
                let id =
                    sent_message_id(&updates, random_id).ok_or_else(|| SendError::Other(anyhow!("no message id")))?;
                confirm(db, &bot.name, chat_id, random_id, id, link, "file").await.map_err(SendError::Other)?;
                info!(bot = %bot.name, chat = chat_id, msg = id, filename, "file sent");
                return Ok(id);
            }
            Err(e) => {
                if let Some(s) = flood_secs(&e) {
                    permit.flood_wait(Duration::from_secs(s as u64 + 1));
                    continue;
                }
                drop(permit);
                if (is_content_error(&e) || e.is("PHOTO_*") || e.is("IMAGE_PROCESS_FAILED"))
                    && let InvocationError::Rpc(r) = e
                {
                    return Err(SendError::Rejected(r));
                }
                if transient(&e) && tries < 10 {
                    tokio::time::sleep(Duration::from_secs(3 * tries as u64)).await;
                    continue;
                }
                return Err(SendError::Other(anyhow!("send media failed: {e}")));
            }
        }
    }
}
