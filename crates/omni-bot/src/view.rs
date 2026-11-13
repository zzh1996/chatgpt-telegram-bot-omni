//! A reply view: keeps the Telegram messages of one attempt in sync with the
//! newest render state.
//!
//! The generation side keeps a buffer with the newest version of the reply
//! up to date (a `watch` channel: every token updates it). The view queues for
//! a slot of the chat's gate, and only once it has the slot does it read the
//! buffer and render, so every message operation carries the newest content.
//! Each operation is weighed (see [`crate::tg::flood`]) and the gate starts it
//! only when Telegram's budget has room. Finished replies go first, then the
//! first message of a new reply, then updates of replies still being
//! generated, each at its own pace: young replies often, long-running ones
//! less often (see [`pace`] and [`crate::tg::gate`]).

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use grammers_tl_types as tl;
use omni_core::{Part, Usage};
use omni_llm::LlmError;
use omni_markdown::Mode;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::app::App;
use crate::compose::{self, ComposeInput, PriorAttempt, Status};
use crate::conv;
use crate::tg::bot::BotConn;
use crate::tg::flood::{self, Shape, Weight};
use crate::tg::gate::{Ask, Class, GatePermit};
use crate::tg::outbox::{self, Content, Link, OpError, SendReq};
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
    /// When this version was made: the time shown while generating.
    pub as_of: Instant,
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
    /// Costs above this (USD) are bold; 0: never (`render.bold_cost_usd`).
    pub bold_cost: f64,
    pub reasoning_open: bool,
    /// `render.answer_first`.
    pub answer_first: bool,
    pub mode: Mode,
    /// Earlier attempts of this reply (shown above, in the same messages).
    pub prior: Vec<PriorAttempt>,
    /// Latest previews of images still being generated (streamed partial images).
    pub previews: Vec<(omni_core::BlobRef, Option<(u32, u32)>)>,
    /// How the reply was served where that differs from the request (`served <model>`, `tier …`, `geo us`).
    pub served: Vec<String>,
    /// The reply finally failed: a "🔁 Retry" button under its last message.
    pub retry_button: bool,
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
            elapsed: self.elapsed_override.unwrap_or_else(|| self.as_of.saturating_duration_since(self.started)),
            show_usage: self.show_usage,
            bold_cost: self.bold_cost,
            reasoning_open: self.reasoning_open,
            answer_first: self.answer_first,
            mode: self.mode,
            raw_ai,
            hops: self.hops,
            continuation: self.continuation,
            attachments: &self.attachments.iter().map(|a| a.key.clone()).collect::<Vec<_>>(),
            settings: &self.settings,
            prior: &self.prior,
            previews: &self.previews,
            served: &self.served,
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
    tx: Arc<watch::Sender<Option<RenderState>>>,
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
        let tx = Arc::new(watch::channel::<Option<RenderState>>(None).0);
        let rx = tx.subscribe();
        let ctx = Ctx { app, bot, chat_id, turn_id, attempt_id, tx: Arc::downgrade(&tx) };
        let task = tokio::spawn(sync_loop(ctx, reply_to, rx, first_delay));
        Self { tx, task }
    }

    /// Replace the newest version.
    pub fn update(&self, mut s: RenderState) {
        s.as_of = Instant::now();
        self.tx.send_replace(Some(s));
    }

    /// Change the newest version in place (cheap: called for every token).
    pub fn modify(&self, f: impl FnOnce(&mut RenderState)) {
        self.tx.send_modify(|o| {
            if let Some(s) = o.as_mut() {
                f(s);
                s.as_of = Instant::now();
            }
        });
    }

    /// Push the final state and wait until Telegram shows it. Returns message ids.
    pub async fn finish(self, mut s: RenderState) -> Vec<i32> {
        s.done = true;
        // Freeze the time shown, so that re-renders (read-back check, retries) are identical.
        if s.elapsed_override.is_none() {
            s.elapsed_override = Some(s.started.elapsed());
        }
        s.as_of = Instant::now();
        self.tx.send_replace(Some(s));
        self.task.await.unwrap_or_default()
    }
}

struct Ctx {
    app: Arc<App>,
    bot: Arc<BotConn>,
    chat_id: i64,
    turn_id: i64,
    attempt_id: i64,
    /// The newest-version buffer, owned by the [`ReplyView`] (weak, so that the
    /// view ends when its owner drops it without a final state).
    tx: Weak<watch::Sender<Option<RenderState>>>,
}

fn is_done(rx: &watch::Receiver<Option<RenderState>>) -> bool {
    rx.borrow().as_ref().is_some_and(|s| s.done)
}

/// Resolves once the newest version is final (never, if the sender is gone first).
async fn until_done(mut rx: watch::Receiver<Option<RenderState>>) {
    loop {
        if rx.borrow_and_update().as_ref().is_some_and(|s| s.done) {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A slot of the chat's gate. A reply that finishes while waiting moves up to
/// the class of finished content at once.
async fn slot(ctx: &Ctx, rx: &watch::Receiver<Option<RenderState>>, ask: Ask) -> GatePermit {
    let gate = ctx.bot.gates.get(ctx.chat_id);
    if ask.class == Class::Final || is_done(rx) {
        return gate.acquire(Ask::finals(ask.weight)).await;
    }
    tokio::select! {
        p = gate.acquire(ask) => p,
        _ = until_done(rx.clone()) => gate.acquire(Ask::finals(ask.weight)).await,
    }
}

/// How often a reply still being generated is updated, by its age: short
/// exchanges should feel fast; people mind slower updates of a long-running
/// reply less, and those leave room for the others in the chat.
pub fn pace(age: Duration) -> Duration {
    let s = age.as_secs_f64();
    Duration::from_secs_f64(if s < 10.0 {
        1.5
    } else if s < 60.0 {
        3.0
    } else {
        (3.0 + (s - 60.0) / 30.0).min(10.0)
    })
}

/// Telegram shows `sent` (id, page hash, shape) for a reply: the first page that differs.
fn next_page<'a>(
    pages: &'a [Content],
    sent: &[(i32, Option<String>, Option<Shape>)],
    markup_of: impl Fn(usize) -> Option<&'a tl::enums::ReplyMarkup>,
) -> Option<(usize, &'a Content, String)> {
    pages
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c, outbox::page_hash(c, markup_of(i))))
        .find(|(i, _, h)| sent.get(*i).and_then(|(_, sh, _)| sh.as_deref()) != Some(h.as_str()))
}

/// What sending or editing page `i` to `content` weighs.
fn page_weight(sent: &[(i32, Option<String>, Option<Shape>)], i: usize, content: &Content) -> Weight {
    let shape = Shape::of(content);
    match sent.get(i) {
        Some((_, _, old)) => flood::edit_weight(*old, shape),
        None => flood::send_weight(shape),
    }
}

async fn sleep_or_shutdown(app: &App, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = app.shutdown.cancelled() => false,
    }
}

async fn sync_loop(
    ctx: Ctx,
    reply_to: Option<i32>,
    mut rx: watch::Receiver<Option<RenderState>>,
    first_delay: Duration,
) -> Vec<i32> {
    let (app, bot, chat_id, turn_id, attempt_id) = (&ctx.app, &ctx.bot, ctx.chat_id, ctx.turn_id, ctx.attempt_id);
    // The reply's messages so far: every attempt of a reply uses the same messages
    // (earlier attempts are shown folded above), except `/continue`, which has its own.
    let mut sent: Vec<(i32, Option<String>, Option<Shape>)> =
        conv::reply_messages(&app.db, &bot.name, turn_id, attempt_id).await.unwrap_or_default();
    let ids = |sent: &[(i32, Option<String>, Option<Shape>)]| sent.iter().map(|(id, _, _)| *id).collect::<Vec<_>>();
    // When Telegram last accepted an update of this reply (its pace counts from there).
    let mut last_update: Option<tokio::time::Instant> = None;
    let mut reply_to = reply_to;
    let mut fallback = Fallback::None;
    let mut media = RichMedia::default();
    let mut media_refreshed = false;
    // Final state handled once: rendering reset, read-back check, separate images.
    let mut final_seen = false;
    let mut verified = false;
    let mut failures = 0u32;
    let heartbeat = Duration::from_secs(30);

    if sent.is_empty() {
        // Batch the first tokens a little before sending the first message.
        let deadline = tokio::time::Instant::now() + first_delay;
        while !is_done(&rx) {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                r = rx.changed() => if r.is_err() { break },
            }
        }
    }
    // Whether the newest version may not be fully shown yet.
    let mut pending = true;
    loop {
        if !pending {
            // Telegram shows the newest version: wait for a newer one.
            let changed = tokio::select! {
                r = tokio::time::timeout(heartbeat, rx.changed()) => r,
                _ = app.shutdown.cancelled() => return ids(&sent),
            };
            match changed {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return ids(&sent), // the generation side is gone without a final state
                Err(_) => {
                    // Nothing new for a while (e.g. long hidden thinking): refresh the time shown.
                    let Some(tx) = ctx.tx.upgrade() else { return ids(&sent) };
                    tx.send_modify(|o| {
                        if let Some(s) = o.as_mut().filter(|s| !s.done) {
                            s.as_of = Instant::now();
                        }
                    });
                }
            }
            pending = true;
        }
        // What the next operation is likely to be, to ask for a fitting slot (rendered
        // from a copy: the generation side writes the buffer for every token).
        let current = rx.borrow().clone();
        let ask = match current.as_ref() {
            Some(state) => {
                let pages = pages_for(state, fallback);
                let markup = state.retry_button.then(|| outbox::retry_markup(turn_id));
                let markup_of = |i: usize| if i + 1 == pages.len() { markup.as_ref() } else { None };
                let weight =
                    next_page(&pages, &sent, markup_of).map_or(Weight::Light, |(i, c, _)| page_weight(&sent, i, c));
                let class = if state.done {
                    Class::Final
                } else if sent.is_empty() {
                    Class::First
                } else {
                    Class::Stream
                };
                let not_before =
                    if class == Class::Stream { last_update.map(|t| t + pace(state.started.elapsed())) } else { None };
                Ask { class, weight, not_before }
            }
            None => Ask { class: Class::First, weight: Weight::Light, not_before: None },
        };
        let permit = slot(&ctx, &rx, ask).await;
        // The slot is ours: render the newest version now.
        let Some(state) = rx.borrow_and_update().clone() else {
            permit.unused();
            pending = false;
            continue;
        };
        if state.done && !final_seen {
            final_seen = true;
            // A rejection while streaming may have come from an unfinished construct:
            // give the final content the full rendering again.
            fallback = Fallback::None;
        }
        let pages = pages_for(&state, fallback);
        let markup = state.retry_button.then(|| outbox::retry_markup(turn_id));
        let markup_of = |i: usize| if i + 1 == pages.len() { markup.as_ref() } else { None };
        let sources = media_sources(&state, &pages);
        if sources.iter().any(|s| !media.items.contains_key(&s.key)) {
            // Upload outside the gate, then queue again.
            permit.unused();
            drop(permit);
            if let Err(e) = upload::ensure(app, bot, chat_id, &sources, &mut media).await {
                warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %format!("{e:#}"), "uploading media failed");
                failures += 1;
                if !sleep_or_shutdown(app, retry_wait(failures)).await {
                    return ids(&sent);
                }
            }
            continue;
        }
        // The first message that differs from what Telegram shows.
        let Some((i, content, hash)) = next_page(&pages, &sent, markup_of) else {
            // Telegram shows the newest version.
            if state.done && sent.len() > pages.len() {
                let extra: Vec<i32> = sent[pages.len()..].iter().map(|(id, _, _)| *id).collect();
                debug!(?extra, "deleting surplus messages");
                match outbox::delete_once(&app.db, bot, &permit, chat_id, &extra).await {
                    Ok(()) => sent.truncate(pages.len()),
                    Err(OpError::Flood(_)) => {}
                    Err(e) => {
                        warn!(error = %e, "delete surplus failed");
                        sent.truncate(pages.len());
                    }
                }
                continue;
            }
            permit.unused();
            drop(permit);
            if !state.done {
                pending = false;
                continue;
            }
            if fallback == Fallback::PlainText {
                // Plain text cannot hold images: only now are they sent as separate messages.
                let first = sent.first().map(|(id, _, _)| *id).or(reply_to);
                send_images_separately(app, bot, chat_id, first, turn_id, attempt_id, &state.parts).await;
            }
            if !verified && fallback != Fallback::PlainText {
                verified = true;
                // Telegram may store messages differently from what was sent (silent
                // truncation): read them back, and if the text was altered after all,
                // attach the exact original so nothing is lost.
                if let Some(problem) = verify(bot, chat_id, &sent, &state, fallback).await {
                    warn!(bot = %bot.name, chat = chat_id, turn = turn_id, %problem, "Telegram altered the stored reply text");
                    let mut st = state.clone();
                    match attach_original(app, turn_id, &mut st).await {
                        Ok(()) => {
                            if let Some(tx) = ctx.tx.upgrade() {
                                tx.send_replace(Some(st));
                                continue;
                            }
                        }
                        Err(e) => warn!(error = %format!("{e:#}"), "cannot store the original Markdown"),
                    }
                }
            }
            return ids(&sent);
        };
        // The newest content may weigh more than what the slot was asked for.
        let weight = page_weight(&sent, i, content);
        if weight > ask.weight && !permit.fits(weight) {
            permit.unused();
            continue;
        }
        permit.set_weight(weight);
        let shape = Shape::of(content);
        let res = if i < sent.len() {
            outbox::edit_once(&app.db, bot, &permit, chat_id, sent[i].0, content, Some(&media), markup_of(i))
                .await
                .map(|_| sent[i].0)
        } else {
            let rt = if i == 0 { reply_to } else { Some(sent[i - 1].0) };
            let req = SendReq {
                chat_id,
                reply_to: rt,
                content,
                link: Link { turn_id, attempt_id: Some(attempt_id), part_index: i as i64 },
                media: Some(&media),
            };
            outbox::send_once(&app.db, bot, &permit, &req, markup_of(i)).await
        };
        drop(permit);
        match res {
            Ok(id) => {
                failures = 0;
                last_update = Some(tokio::time::Instant::now());
                if i < sent.len() {
                    sent[i] = (id, Some(hash), Some(shape));
                } else {
                    sent.push((id, Some(hash), Some(shape)));
                }
            }
            // The gate is blocked for the wait; the next slot renders the newest version again.
            Err(OpError::Flood(wait)) => warn!(bot = %bot.name, chat = chat_id, ?wait, "flood wait"),
            Err(OpError::ReplyGone) => {
                warn!(bot = %bot.name, chat = chat_id, "replied-to message is gone; sending without reply");
                if i == 0 {
                    reply_to = None;
                }
            }
            Err(OpError::Rejected(e)) if !media_refreshed && is_media_error(&e) && !media.items.is_empty() => {
                // A cached upload went stale (file reference): upload again once.
                warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, "media rejected; uploading again");
                media_refreshed = true;
                let keys: Vec<String> = media.items.keys().cloned().collect();
                if let Err(e) = upload::forget(&app.db, &bot.name, &keys, &mut media).await {
                    warn!(error = %format!("{e:#}"), "forgetting stale uploads failed");
                }
            }
            Err(OpError::Rejected(e)) if fallback != Fallback::PlainText => {
                let next = if fallback == Fallback::None { Fallback::RawAi } else { Fallback::PlainText };
                warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, ?next, "content rejected; degrading rendering");
                fallback = next;
            }
            Err(e) => {
                // Network trouble, or even plain text rejected: keep trying (a reply must
                // never stay stuck in "generating"), backing off.
                failures += 1;
                warn!(bot = %bot.name, chat = chat_id, turn = turn_id, error = %e, failures, "failed to update reply messages");
                if !sleep_or_shutdown(app, retry_wait(failures)).await {
                    return ids(&sent);
                }
            }
        }
    }
}

fn retry_wait(failures: u32) -> Duration {
    Duration::from_secs((5u64 << failures.saturating_sub(1).min(5)).min(120))
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
    sent: &[(i32, Option<String>, Option<Shape>)],
    state: &RenderState,
    fallback: Fallback,
) -> Option<String> {
    let pages = pages_for(state, fallback);
    let ids: Vec<i32> = sent.iter().map(|(id, _, _)| *id).collect();
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
    if md.is_empty() {
        anyhow::bail!("no model output to attach");
    }
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
    // Previews of images still being generated.
    for (i, (blob, _)) in state.previews.iter().enumerate() {
        if keys.contains(&blob.sha256) && !out.iter().any(|s: &Source| s.key == blob.sha256) {
            out.push(Source {
                key: blob.sha256.clone(),
                blob: blob.clone(),
                filename: format!("preview-{}.{}", i + 1, blob.mime.rsplit('/').next().unwrap_or("png")),
                want: Want::Photo,
            });
        }
    }
    // Generated files (videos, files written by code tools), as documents.
    for p in &state.parts {
        if let Part::File { blob, filename, generated: true } = p
            && keys.contains(&blob.sha256)
            && !out.iter().any(|s: &Source| s.key == blob.sha256)
        {
            let ext = blob.mime.rsplit('/').next().unwrap_or("bin");
            let want = if blob.mime.starts_with("video/") {
                Want::Video
            } else if blob.mime.starts_with("audio/") {
                Want::Audio
            } else {
                Want::Document
            };
            out.push(Source {
                key: blob.sha256.clone(),
                blob: blob.clone(),
                filename: filename.clone().unwrap_or_else(|| format!("file.{ext}")),
                want,
            });
        }
    }
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
