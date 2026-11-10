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
    pub images: usize,
    /// Completed long-decode continuation hops.
    pub hops: u32,
    /// This attempt continues a finished answer (`/continue`).
    pub continuation: bool,
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
            images: self.images,
            hops: self.hops,
            continuation: self.continuation,
        })
    }
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
            apply(&app, &bot, chat_id, reply_to, turn_id, attempt_id, &state, &mut sent, &mut fallback).await
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
                    match apply(&app, &bot, chat_id, reply_to, turn_id, attempt_id, &state, &mut sent, &mut fallback)
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
            break;
        }
    }
    sent.into_iter().map(|(id, _)| id).collect()
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
) -> Result<(), String> {
    'retry: loop {
        let pages = pages_for(state, *fallback);
        for (i, content) in pages.iter().enumerate() {
            let hash = content.hash();
            let res = if i < sent.len() {
                if sent[i].1.as_deref() == Some(hash.as_str()) {
                    continue;
                }
                outbox::edit(&app.db, bot, chat_id, sent[i].0, content).await.map(|_| sent[i].0)
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
