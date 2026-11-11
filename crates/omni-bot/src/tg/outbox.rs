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
use super::flood::{self, Shape, Weight};
use super::gate::{Ask, GatePermit};
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

/// The inline keyboard under a reply that finally failed: one "🔁 Retry" button
/// (callback data `retry:<turn id>`).
pub fn retry_markup(turn_id: i64) -> tl::enums::ReplyMarkup {
    let button = tl::types::KeyboardInlineButton {
        style: None,
        text: "🔁 Retry".into(),
        r#type: tl::enums::InlineButtonType::Callback(tl::types::InlineButtonTypeCallback {
            requires_password: false,
            data: format!("retry:{turn_id}").into_bytes(),
        }),
    };
    tl::types::ReplyInlineMarkup {
        force_reply: false,
        rows: vec![tl::enums::KeyboardInlineButtonRow::Row(tl::types::KeyboardInlineButtonRow {
            buttons: vec![tl::enums::KeyboardInlineButton::Button(button)],
        })],
    }
    .into()
}

/// What a message shows, buttons included (to know whether it must be edited).
pub fn page_hash(content: &Content, markup: Option<&tl::enums::ReplyMarkup>) -> String {
    match markup {
        None => content.hash(),
        Some(m) => {
            let mut bytes = Vec::new();
            grammers_tl_types::Serializable::serialize(m, &mut |b: &[u8]| bytes.extend_from_slice(b));
            hex::encode(Sha256::digest(format!("{}+{}", content.hash(), hex::encode(bytes)).as_bytes()))
        }
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

/// Why one attempt of a message operation did not succeed.
#[derive(Debug)]
pub enum OpError {
    /// FLOOD_WAIT: the chat's gate is already blocked for this long. Try again
    /// with a new permit (and the newest content).
    Flood(Duration),
    /// A network or server hiccup: worth another attempt.
    Transient(String),
    /// The replied-to message no longer exists.
    ReplyGone,
    /// Telegram rejected this content; the caller may retry with fallback content.
    Rejected(grammers_mtsender::RpcError),
    Other(anyhow::Error),
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::Flood(d) => write!(f, "flood wait {}s", d.as_secs()),
            OpError::Transient(e) => write!(f, "transient error: {e}"),
            OpError::ReplyGone => write!(f, "the replied-to message is gone"),
            OpError::Rejected(e) => write!(f, "rejected by Telegram: {e}"),
            OpError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

fn classify(e: InvocationError, permit: &GatePermit, has_reply: bool) -> OpError {
    if let Some(s) = flood_secs(&e) {
        let wait = Duration::from_secs(s as u64 + 1);
        permit.flood_wait(wait);
        return OpError::Flood(wait);
    }
    if has_reply && (e.is("REPLY_MESSAGE_ID_INVALID") || e.is("MESSAGE_ID_INVALID")) {
        return OpError::ReplyGone;
    }
    if transient(&e) {
        return OpError::Transient(e.to_string());
    }
    if is_content_error(&e)
        && let InvocationError::Rpc(r) = e
    {
        return OpError::Rejected(r);
    }
    OpError::Other(anyhow!("{e}"))
}

fn reply_header(reply_to: Option<i32>) -> Option<tl::enums::InputReplyTo> {
    reply_to.map(|id| {
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
    })
}

/// One attempt to make the message for `req.link` exist, using a permit the
/// caller already holds (so the caller chose the content after getting it).
///
/// Idempotent: a link that was sent before (possibly before a crash) reuses
/// its random_id, which returns the already-sent message.
pub async fn send_once(
    db: &Db,
    bot: &BotConn,
    permit: &GatePermit,
    req: &SendReq<'_>,
    markup: Option<&tl::enums::ReplyMarkup>,
) -> Result<i32, OpError> {
    let (random_id, existing) = reserve(db, &bot.name, req).await.map_err(OpError::Other)?;
    if let Some(id) = existing {
        permit.unused();
        return Ok(id);
    }
    let peer = bot.input_peer(req.chat_id).await.map_err(OpError::Other)?;
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
        peer,
        reply_to: reply_header(req.reply_to),
        message,
        random_id,
        reply_markup: markup.cloned(),
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
    match bot.invoke_once(&call).await {
        Ok(updates) => {
            let Some(id) = sent_message_id(&updates, random_id) else {
                return Err(OpError::Other(anyhow!("sent message id not found in response")));
            };
            confirm(
                db,
                &bot.name,
                req.chat_id,
                random_id,
                id,
                req.link,
                &page_hash(req.content, markup),
                Shape::of(req.content),
            )
            .await
            .map_err(OpError::Other)?;
            debug!(bot = %bot.name, chat = req.chat_id, msg = id, random_id, "message sent");
            Ok(id)
        }
        Err(e) => Err(classify(e, permit, req.reply_to.is_some())),
    }
}

/// One attempt to edit a message, using a permit the caller already holds.
/// `MESSAGE_NOT_MODIFIED` counts as success; a deleted message is skipped.
pub async fn edit_once(
    db: &Db,
    bot: &BotConn,
    permit: &GatePermit,
    chat_id: i64,
    msg_id: i32,
    content: &Content,
    media: Option<&RichMedia>,
    markup: Option<&tl::enums::ReplyMarkup>,
) -> Result<(), OpError> {
    edit_once_status(db, bot, permit, chat_id, msg_id, content, media, markup).await.map(|_| ())
}

/// What an accepted edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    Changed,
    /// Telegram said `MESSAGE_NOT_MODIFIED`.
    Unchanged,
    /// The message no longer exists.
    Gone,
}

/// [`edit_once`], telling what happened.
#[allow(clippy::too_many_arguments)]
pub async fn edit_once_status(
    db: &Db,
    bot: &BotConn,
    permit: &GatePermit,
    chat_id: i64,
    msg_id: i32,
    content: &Content,
    media: Option<&RichMedia>,
    markup: Option<&tl::enums::ReplyMarkup>,
) -> Result<Edited, OpError> {
    let peer = bot.input_peer(chat_id).await.map_err(OpError::Other)?;
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
        peer,
        id: msg_id,
        message,
        media: None,
        // Without a markup Telegram removes the previous one (e.g. a Retry button).
        reply_markup: markup.cloned(),
        entities,
        schedule_date: None,
        schedule_repeat_period: None,
        quick_reply_shortcut_id: None,
        rich_message,
    };
    let outcome = match bot.invoke_once(&call).await {
        Ok(_) => Edited::Changed,
        Err(e) if e.is("MESSAGE_NOT_MODIFIED") => Edited::Unchanged,
        Err(e) if e.is("MESSAGE_ID_INVALID") => {
            // The message was deleted (e.g. by an admin): nothing to update.
            warn!(bot = %bot.name, chat = chat_id, msg = msg_id, "message to edit no longer exists");
            return Ok(Edited::Gone);
        }
        Err(e) => return Err(classify(e, permit, false)),
    };
    let hash = page_hash(content, markup);
    let shape = Shape::of(content);
    let bot_name = bot.name.clone();
    db.call(move |c| {
        c.execute(
            "UPDATE outbox SET content_hash = ?1, updated_at = ?2, shape_chars = ?6, shape_top = ?7
             WHERE bot = ?3 AND chat_id = ?4 AND msg_id = ?5",
            params![hash, now_ms(), bot_name, chat_id, msg_id, shape.chars as i64, shape.top_blocks as i64],
        )?;
        Ok(())
    })
    .await
    .map_err(OpError::Other)?;
    Ok(outcome)
}

/// One attempt to delete messages, using a permit the caller already holds.
pub async fn delete_once(db: &Db, bot: &BotConn, permit: &GatePermit, chat_id: i64, ids: &[i32]) -> Result<(), OpError> {
    permit.set_weight(Weight::Free);
    if ids.is_empty() {
        permit.unused();
        return Ok(());
    }
    let res = if super::convert::chat_kind(chat_id) == super::convert::ChatKind::Channel {
        let channel = bot.input_channel(chat_id).await.map_err(OpError::Other)?;
        bot.invoke_once(&tl::functions::channels::DeleteMessages { channel, id: ids.to_vec() }).await.map(|_| ())
    } else {
        bot.invoke_once(&tl::functions::messages::DeleteMessages { revoke: true, id: ids.to_vec() }).await.map(|_| ())
    };
    if let Err(e) = res {
        return Err(classify(e, permit, false));
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
    .map_err(OpError::Other)
}

/// Back-off before retrying after a transient error.
pub fn transient_backoff(tries: u32) -> Duration {
    Duration::from_secs((2u64 << tries.min(4)).min(30))
}

/// Ensure the message for `link` exists in Telegram; returns its id. For
/// complete content (command answers, notices): highest priority, retried.
pub async fn send(db: &Db, bot: &BotConn, req: SendReq<'_>) -> Result<i32, SendError> {
    let mut req = req;
    let gate = bot.gates.get(req.chat_id);
    let mut tries = 0u32;
    loop {
        tries += 1;
        let permit = gate.acquire(Ask::finals(flood::send_weight(Shape::of(req.content)))).await;
        let res = send_once(db, bot, &permit, &req, None).await;
        drop(permit);
        match res {
            Ok(id) => return Ok(id),
            Err(OpError::Flood(wait)) => warn!(bot = %bot.name, chat = req.chat_id, ?wait, "flood wait on send"),
            Err(OpError::ReplyGone) => {
                warn!(bot = %bot.name, chat = req.chat_id, "replied-to message is gone; sending without reply");
                req.reply_to = None;
            }
            Err(OpError::Rejected(r)) => return Err(SendError::Rejected(r)),
            Err(OpError::Transient(e)) if tries < 30 => {
                let wait = transient_backoff(tries);
                warn!(bot = %bot.name, chat = req.chat_id, error = %e, ?wait, "transient send error; retrying with the same random_id");
                tokio::time::sleep(wait).await;
            }
            Err(OpError::Transient(e)) => return Err(SendError::Other(anyhow!("send failed: {e}"))),
            Err(OpError::Other(e)) => return Err(SendError::Other(e)),
        }
    }
}

/// Delete messages (highest priority, retried on flood waits).
pub async fn delete(db: &Db, bot: &BotConn, chat_id: i64, ids: &[i32]) -> Result<()> {
    let gate = bot.gates.get(chat_id);
    let mut tries = 0u32;
    loop {
        tries += 1;
        let permit = gate.acquire(Ask::finals(Weight::Free)).await;
        let res = delete_once(db, bot, &permit, chat_id, ids).await;
        drop(permit);
        match res {
            Ok(()) => return Ok(()),
            Err(OpError::Flood(_)) => {}
            Err(OpError::Transient(_)) if tries < 10 => tokio::time::sleep(transient_backoff(tries)).await,
            Err(e) => {
                warn!(bot = %bot.name, chat = chat_id, error = %e, "delete failed");
                return Ok(());
            }
        }
    }
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

#[allow(clippy::too_many_arguments)]
async fn confirm(
    db: &Db,
    bot: &str,
    chat: i64,
    random_id: i64,
    msg_id: i32,
    link: Link,
    hash: &str,
    shape: Shape,
) -> Result<()> {
    let (bot, hash) = (bot.to_string(), hash.to_string());
    db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE outbox SET msg_id = ?1, state = 'sent', content_hash = ?2, updated_at = ?3, shape_chars = ?6, shape_top = ?7
             WHERE bot = ?4 AND random_id = ?5",
            params![msg_id, hash, now_ms(), bot, random_id, shape.chars as i64, shape.top_blocks as i64],
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
    let mut tries = 0u32;
    let mut reply_to = reply_to;
    loop {
        tries += 1;
        // Photos and documents: their weight was not measured; assume heavy.
        let permit = gate.acquire(Ask::finals(Weight::Heavy)).await;
        let call = tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: peer.clone(),
            reply_to: reply_header(reply_to),
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
        let res = bot.invoke_once(&call).await;
        let res = match res {
            Ok(updates) => Ok(updates),
            Err(e) if e.is("PHOTO_*") || e.is("IMAGE_PROCESS_FAILED") => match e {
                InvocationError::Rpc(r) => return Err(SendError::Rejected(r)),
                e => Err(OpError::Other(anyhow!("{e}"))),
            },
            Err(e) => Err(classify(e, &permit, reply_to.is_some())),
        };
        drop(permit);
        match res {
            Ok(updates) => {
                let id =
                    sent_message_id(&updates, random_id).ok_or_else(|| SendError::Other(anyhow!("no message id")))?;
                let shape = Shape { chars: caption.chars().count(), top_blocks: 1 };
                confirm(db, &bot.name, chat_id, random_id, id, link, "file", shape).await.map_err(SendError::Other)?;
                info!(bot = %bot.name, chat = chat_id, msg = id, filename, "file sent");
                return Ok(id);
            }
            Err(OpError::Flood(_)) => {}
            Err(OpError::ReplyGone) => reply_to = None,
            Err(OpError::Rejected(r)) => return Err(SendError::Rejected(r)),
            Err(OpError::Transient(_)) if tries < 10 => tokio::time::sleep(Duration::from_secs(3 * tries as u64)).await,
            Err(e) => return Err(SendError::Other(anyhow!("send media failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_button_changes_what_a_page_shows() {
        let c = Content::Text(Formatted::plain("failed"));
        let m = retry_markup(42);
        assert_ne!(page_hash(&c, Some(&m)), page_hash(&c, None));
        assert_eq!(page_hash(&c, None), c.hash());
        assert_eq!(page_hash(&c, Some(&m)), page_hash(&c, Some(&retry_markup(42))));
        assert_ne!(page_hash(&c, Some(&m)), page_hash(&c, Some(&retry_markup(43))));
        let tl::enums::ReplyMarkup::ReplyInlineMarkup(k) = m else { panic!() };
        let tl::enums::KeyboardInlineButtonRow::Row(row) = &k.rows[0];
        let tl::enums::KeyboardInlineButton::Button(b) = &row.buttons[0];
        assert_eq!(b.text, "🔁 Retry");
        assert!(matches!(&b.r#type, tl::enums::InlineButtonType::Callback(c) if c.data == b"retry:42"));
    }
}
