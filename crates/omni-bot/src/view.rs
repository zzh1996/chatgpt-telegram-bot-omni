//! A reply view: keeps the Telegram messages of one attempt in sync with the
//! latest render state.
//!
//! Updates are coalesced (only the newest state is ever rendered) and all
//! operations go through the per-chat gate, so many concurrent streams in one
//! group share the rate budget fairly (FIFO).

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_core::{Part, Usage};
use omni_llm::LlmError;
use omni_markdown::Mode;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::app::App;
use crate::compose::{self, ComposeInput, Status};
use crate::conv;
use crate::tg::bot::BotConn;
use crate::tg::outbox::{self, Content, Link, SendError, SendReq};
use crate::tg::rich::RichMedia;
use crate::tg::upload::{self, Source, Want};

#[derive(Debug, Clone)]
pub struct RenderState {
    pub model: String,
    pub options: String,
    pub parts: Vec<Part>,
    pub status: Status,
    pub attempt: i64,
    pub max_attempts: u32,
    pub error: Option<LlmError>,
    pub usage: Option<Usage>,
    pub provider: Option<String>,
    pub finish_reason: Option<String>,
    pub native_finish_reason: Option<String>,
    pub notes: Vec<String>,
    pub started: Instant,
    pub elapsed_override: Option<Duration>,
    /// Completed long-decode continuation hops.
    pub hops: u32,
    /// This attempt continues a finished answer (`/continue`).
    pub continuation: bool,
    /// Files the bot attached to the reply (see [`Attachment`]).
    pub attachments: Vec<Attachment>,
    /// Effective request settings, described (see `omni_llm::settings`).
    pub settings: Vec<String>,
    pub show_usage: bool,
    pub mode: Mode,
    /// Final state: sync it and stop.
    pub done: bool,
}

impl RenderState {
    fn compose(&self, raw_ai: bool) -> Vec<omni_markdown::rich::RBlock> {
        compose::compose(&ComposeInput {
            model: &self.model,
            options: &self.options,
            parts: &self.parts,
            status: self.status.clone(),
            attempt: self.attempt,
            max_attempts: self.max_attempts,
            error: self.error.as_ref(),
            usage: self.usage.as_ref(),
            provider: self.provider.as_deref(),
            finish_reason: self.finish_reason.as_deref(),
            native_finish_reason: self.native_finish_reason.as_deref(),
            notes: &self.notes,
            elapsed: self.elapsed_override.unwrap_or_else(|| self.started.elapsed()),
            show_usage: self.show_usage,
            mode: self.mode,
            raw_ai,
            hops: self.hops,
            continuation: self.continuation,
            attachments: &self.attachments.iter().map(|a| a.key.clone()).collect::<Vec<_>>(),
            settings: &self.settings,
        })
    }
}

/// A file the bot attaches to a reply.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub key: String,
    pub filename: String,
    pub blob: omni_core::BlobRef,
}

pub struct ReplyView {
    tx: watch::Sender<Option<RenderState>>,
    task: JoinHandle<Vec<i32>>,
}

/// How far we had to degrade the rendering after Telegram rejected content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fallback {
    None,
    RawAi,
    PlainText,
}

impl ReplyView {
    pub fn start(
        app: Arc<App>,
        bot: Arc<BotConn>,
        chat_id: i64,
        reply_to: Option<i32>,
        turn_id: i64,
        attempt_id: i64,
        first_delay: Duration,
    ) -> Self {
        let (tx, rx) = watch::channel::<Option<RenderState>>(None);
        let task = tokio::spawn(sync_loop(app, bot, chat_id, reply_to, turn_id, attempt_id, rx, first_delay));
        Self { tx, task }
    }

    pub fn update(&self, s: RenderState) {
        let _ = self.tx.send(Some(s));
    }

    /// Push the final state and wait until Telegram shows it. Returns message ids.
    pub async fn finish(self, mut s: RenderState) -> Vec<i32> {
        s.done = true;
        let _ = self.tx.send(Some(s));
        self.task.await.unwrap_or_default()
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_loop(
    app: Arc<App>,
    bot: Arc<BotConn>,
    chat_id: i64,
    reply_to: Option<i32>,
    turn_id: i64,
    attempt_id: i64,
    mut rx: watch::Receiver<Option<RenderState>>,
    first_delay: Duration,
) -> Vec<i32> {
    // Messages already sent for this attempt (non-empty when resuming after a restart).
    let mut sent: Vec<(i32, Option<String>)> = conv::attempt_messages(&app.db, &bot.name, attempt_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(_, id, h)| (id, h))
        .collect();
    let mut fallback = Fallback::None;
    let mut media = RichMedia::default();
    let mut first = sent.is_empty();
    let tick = Duration::from_secs(30);
    loop {
        if first {
            // Batch the first tokens a little before sending the first message.
            let deadline = tokio::time::Instant::now() + first_delay;
            loop {
                let done = rx.borrow().as_ref().is_some_and(|s| s.done);
                if done {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    r = rx.changed() => if r.is_err() { break },
                }
            }
            first = false;
        } else {
            let done = rx.borrow().as_ref().is_some_and(|s| s.done);
            if !done && let Ok(Err(_)) = tokio::time::timeout(tick, rx.changed()).await {
                break;
            }
        }
        let Some(state) = rx.borrow_and_update().clone() else { continue };
        let done = state.done;
        if done && fallback != Fallback::None {
            // A rejection while streaming may have come from an unfinished construct:
            // give the final content the full rendering again.
            fallback = Fallback::None;
        }
        if let Err(e) =
            apply(&app, &bot, chat_id, reply_to, turn_id, attempt_id, &state, &mut sent, &mut fallback, &mut media)
                .await
        {
            warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, "failed to update reply messages");
            if done {
                // Never leave a reply stuck in "generating": keep trying the final state.
                let mut wait = Duration::from_secs(5);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = app.shutdown.cancelled() => return sent.into_iter().map(|(id, _)| id).collect(),
                    }
                    match apply(
                        &app,
                        &bot,
                        chat_id,
                        reply_to,
                        turn_id,
                        attempt_id,
                        &state,
                        &mut sent,
                        &mut fallback,
                        &mut media,
                    )
                    .await
                    {
                        Ok(()) => break,
                        Err(e) => {
                            warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, "final update still failing; retrying");
                            wait = (wait * 2).min(Duration::from_secs(120));
                        }
                    }
                }
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if done {
            // Telegram has stored messages differently from what was sent before
            // (silent truncation): read the final messages back, and if the text
            // was altered after all, attach the exact original so nothing is lost.
            if fallback != Fallback::PlainText
                && let Some(problem) = verify(&bot, chat_id, &sent, &state, fallback).await
            {
                warn!(bot = %bot.name, chat = chat_id, turn = turn_id, %problem, "Telegram altered the stored reply text");
                let mut st = state.clone();
                match attach_original(&app, turn_id, &mut st).await {
                    Ok(()) => {
                        if let Err(e) = apply(
                            &app,
                            &bot,
                            chat_id,
                            reply_to,
                            turn_id,
                            attempt_id,
                            &st,
                            &mut sent,
                            &mut fallback,
                            &mut media,
                        )
                        .await
                        {
                            warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, "attaching the original failed");
                        }
                    }
                    Err(e) => warn!(error = %format!("{e:#}"), "cannot store the original Markdown"),
                }
            }
            break;
        }
    }
    sent.into_iter().map(|(id, _)| id).collect()
}

/// Telegram's own normalization of rich-message text (measured 2026-11-11):
/// control characters (tab, CR, NUL, …) become spaces, U+2028 a line break and
/// U+2029 a space. Differences beyond this are information loss.
fn telegram_normalized(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\u{1}' => c, // '\u{1}' is our block separator
            '\u{2028}' => '\n',
            '\u{2029}' => ' ',
            c if (c as u32) < 0x20 => ' ',
            c => c,
        })
        .collect()
}

/// Compare what Telegram stored with what was sent. `None` if identical.
async fn verify(
    bot: &BotConn,
    chat_id: i64,
    sent: &[(i32, Option<String>)],
    state: &RenderState,
    fallback: Fallback,
) -> Option<String> {
    let pages = pages_for(state, fallback);
    let ids: Vec<i32> = sent.iter().map(|(id, _)| *id).collect();
    let msgs = match bot.get_messages(chat_id, &ids).await {
        Ok(m) => m,
        Err(e) => {
            debug!(error = %e, "read-back of reply messages failed");
            return None;
        }
    };
    for (i, content) in pages.iter().enumerate() {
        let Content::Rich(blocks) = content else { continue };
        let Some(grammers_tl_types::enums::Message::Message(m)) = msgs.get(i) else { continue };
        let Some(grammers_tl_types::enums::RichMessage::Message(rm)) = &m.rich_message else {
            return Some(format!("message {} has no rich content", m.id));
        };
        let want = telegram_normalized(&crate::tg::rich::expected_text(blocks));
        let got = telegram_normalized(&crate::tg::rich::received_text(&rm.blocks));
        if want != got {
            let at = want.chars().zip(got.chars()).take_while(|(a, b)| a == b).count();
            return Some(format!(
                "message {}: sent {} chars, stored {} (first difference at char {at})",
                m.id,
                want.chars().count(),
                got.chars().count()
            ));
        }
    }
    None
}

/// Attach the exact model output as a Markdown file, with a note.
async fn attach_original(app: &App, turn_id: i64, st: &mut RenderState) -> anyhow::Result<()> {
    let md = compose::raw_markdown(&st.parts);
    let blob = app.blobs.put(md.into_bytes(), "text/markdown".into()).await?;
    let filename = format!("answer-{turn_id}.md");
    st.attachments.push(Attachment { key: format!("{}/{filename}", blob.sha256), filename, blob });
    st.notes.push("Telegram altered the stored text of this reply; the exact original is attached".into());
    Ok(())
}

fn pages_for(state: &RenderState, fallback: Fallback) -> Vec<Content> {
    match fallback {
        Fallback::None | Fallback::RawAi => {
            let blocks = state.compose(fallback == Fallback::RawAi);
            compose::paginate(&state.model, blocks).into_iter().map(Content::Rich).collect()
        }
        Fallback::PlainText => {
            let f = compose::blocks_to_plain(&state.compose(true));
            omni_markdown::split(&f, omni_markdown::Limits { max_utf16: 4000, max_entities: 90 })
                .into_iter()
                .map(Content::Text)
                .collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    chat_id: i64,
    reply_to: Option<i32>,
    turn_id: i64,
    attempt_id: i64,
    state: &RenderState,
    sent: &mut Vec<(i32, Option<String>)>,
    fallback: &mut Fallback,
    media: &mut RichMedia,
) -> Result<(), String> {
    let mut media_refreshed = false;
    'retry: loop {
        let pages = pages_for(state, *fallback);
        // Upload the images these pages show (cached: never uploaded twice).
        let sources = media_sources(state, &pages);
        if !sources.is_empty() {
            upload::ensure(app, bot, chat_id, &sources, media).await.map_err(|e| format!("uploading media: {e:#}"))?;
        }
        for (i, content) in pages.iter().enumerate() {
            let hash = content.hash();
            let res = if i < sent.len() {
                if sent[i].1.as_deref() == Some(hash.as_str()) {
                    continue;
                }
                outbox::edit(&app.db, bot, chat_id, sent[i].0, content, Some(media)).await.map(|_| sent[i].0)
            } else {
                let rt = if i == 0 { reply_to } else { Some(sent[i - 1].0) };
                outbox::send(
                    &app.db,
                    bot,
                    SendReq {
                        chat_id,
                        reply_to: rt,
                        content,
                        link: Link { turn_id, attempt_id: Some(attempt_id), part_index: i as i64 },
                        media: Some(media),
                    },
                )
                .await
            };
            match res {
                Ok(id) => {
                    if i < sent.len() {
                        sent[i] = (id, Some(hash));
                    } else {
                        sent.push((id, Some(hash)));
                    }
                }
                Err(SendError::Rejected(e)) if !media_refreshed && is_media_error(&e) && !media.items.is_empty() => {
                    // A cached upload went stale (file reference): upload again once.
                    warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, "media rejected; uploading again");
                    media_refreshed = true;
                    let keys: Vec<String> = media.items.keys().cloned().collect();
                    upload::forget(&app.db, &bot.name, &keys, media).await.map_err(|e| format!("{e:#}"))?;
                    continue 'retry;
                }
                Err(SendError::Rejected(e)) => {
                    let next = match *fallback {
                        Fallback::None => Fallback::RawAi,
                        Fallback::RawAi => Fallback::PlainText,
                        Fallback::PlainText => return Err(format!("Telegram rejected even plain text: {e}")),
                    };
                    warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, ?next, "content rejected; degrading rendering");
                    *fallback = next;
                    continue 'retry;
                }
                Err(SendError::Other(e)) => return Err(format!("{e:#}")),
            }
        }
        if state.done && *fallback == Fallback::PlainText {
            // Plain text cannot hold images: only now are they sent as separate messages.
            let first = sent.first().map(|(id, _)| *id).or(reply_to);
            send_images_separately(app, bot, chat_id, first, turn_id, attempt_id, &state.parts).await;
        }
        if state.done && sent.len() > pages.len() {
            let extra: Vec<i32> = sent[pages.len()..].iter().map(|(id, _)| *id).collect();
            debug!(?extra, "deleting surplus messages");
            if let Err(e) = outbox::delete(&app.db, bot, chat_id, &extra).await {
                warn!(error = %e, "delete surplus failed");
            }
            sent.truncate(pages.len());
        }
        return Ok(());
    }
}

fn is_media_error(e: &grammers_mtsender::RpcError) -> bool {
    ["FILE_REFERENCE", "MEDIA_", "PHOTO_", "DOCUMENT_"].iter().any(|p| e.name.starts_with(p))
}

/// The generated images referenced by `pages`, as upload sources.
fn media_sources(state: &RenderState, pages: &[Content]) -> Vec<Source> {
    let mut keys = Vec::new();
    for p in pages {
        if let Content::Rich(blocks) = p {
            for k in omni_markdown::rich::media_keys(blocks) {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
    }
    let mut out: Vec<Source> = state
        .attachments
        .iter()
        .filter(|a| keys.contains(&a.key))
        .map(|a| Source {
            key: a.key.clone(),
            blob: a.blob.clone(),
            filename: a.filename.clone(),
            want: Want::Document,
        })
        .collect();
    let mut n = 0;
    for p in &state.parts {
        if let Part::Image { blob, generated: true, .. } = p {
            n += 1;
            if keys.contains(&blob.sha256) && !out.iter().any(|s: &Source| s.key == blob.sha256) {
                let ext = blob.mime.rsplit('/').next().unwrap_or("png");
                out.push(Source {
                    key: blob.sha256.clone(),
                    blob: blob.clone(),
                    filename: format!("image-{n}.{ext}"),
                    want: Want::Photo,
                });
            }
        }
    }
    out
}

/// Last resort (plain-text rendering): generated images as their own messages.
async fn send_images_separately(
    app: &App,
    bot: &BotConn,
    chat_id: i64,
    reply_to: Option<i32>,
    turn_id: i64,
    attempt_id: i64,
    parts: &[Part],
) {
    let mut idx = 0i64;
    for p in parts {
        if let Part::Image { blob, generated: true, .. } = p {
            idx += 1;
            let Ok(bytes) = app.blobs.get(blob).await else { continue };
            let ext = blob.mime.rsplit('/').next().unwrap_or("png");
            let name = format!("image-{turn_id}-{idx}.{ext}");
            let link = Link { turn_id, attempt_id: Some(attempt_id), part_index: 1000 + idx };
            let res =
                outbox::send_file(&app.db, bot, chat_id, reply_to, bytes.clone(), &name, &blob.mime, true, "", link)
                    .await;
            if let Err(e) = res {
                warn!(error = %e, "sending image as photo failed; sending as file");
                let link = Link { turn_id, attempt_id: Some(attempt_id), part_index: 2000 + idx };
                if let Err(e) =
                    outbox::send_file(&app.db, bot, chat_id, reply_to, bytes, &name, &blob.mime, false, "", link).await
                {
                    warn!(error = %e, "sending image failed");
                }
            }
        }
    }
}
